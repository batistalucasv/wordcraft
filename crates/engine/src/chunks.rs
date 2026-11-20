//! Code chunks: the transient kernel state behind [`wordcraft_doc::chunk`]'s document model.
//!
//! The chunk itself — its language, code and Quarto options — lives in the document as a
//! block-level content control tagged `wordcraft:chunk:<language>` (see [`wordcraft_doc::chunk`]),
//! so saving keeps it. What a run produces is not part of the document: it is kept here, like a
//! notebook's kernel output, and forgotten by `code.clear_output`, `code.restart_kernel`, another
//! run, or opening another document.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use serde_json::{Value, json};
use wordcraft_doc::{ChunkRef, Document, Pos, StoryRef};
use wordcraft_kernel::{ExecutionRequest, ExecutionResult, KernelError, TabularOutput};

/// Default execution limit when a command doesn't pass `timeoutMs`.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Shortest execution limit a command may ask for.
pub const MIN_TIMEOUT_MS: u64 = 100;
/// Longest execution limit a command may ask for (ten minutes).
pub const MAX_TIMEOUT_MS: u64 = 600_000;

/// Longest chunk source that is run, in bytes: the document model's own limit.
pub const MAX_CHUNK_BYTES: usize = wordcraft_doc::chunk::MAX_CHUNK_CODE_BYTES;

/// Longest stdout, stderr or SVG text reported in a command's JSON, in bytes. A runaway script
/// can print more; the rest stays in memory for the front end but is not echoed to an agent.
const MAX_REPORTED_TEXT: usize = 256 * 1024;

/// Whether `p` is inside the chunk `range` (between its markers, either edge included).
fn contains(range: &ChunkRef, p: &Pos) -> bool {
    p.story == range.start.story && (&p.path, p.off) >= (&range.start.path, range.start.off) && (&p.path, p.off) <= (&range.end.path, range.end.off)
}

/// The innermost code chunk holding `p`, if any.
pub fn chunk_at(doc: &Document, p: &Pos) -> Option<ChunkRef> {
    doc.chunk_refs(p.story).into_iter().filter(|r| contains(r, p)).max_by(|a, b| {
        (&a.start.path, a.start.off).cmp(&(&b.start.path, b.start.off)).then_with(|| (&b.end.path, b.end.off).cmp(&(&a.end.path, a.end.off)))
    })
}

/// The code chunks of a story, in document order.
pub fn chunks_in(doc: &Document, story: StoryRef) -> Vec<ChunkRef> {
    doc.chunk_refs(story)
}

/// What one run of a chunk produced, kept until the next run or a clear.
#[derive(Debug, Clone, Default)]
pub struct ChunkOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub timed_out: bool,
    pub output_truncated: bool,
    pub duration_ms: u64,
    /// Plot as PNG bytes, when the run captured one.
    pub plot_png: Option<Vec<u8>>,
    /// Plot as SVG XML, when the run captured one.
    pub plot_svg: Option<String>,
    /// Tables passed to `wc_table()`, in call order.
    pub tables: Vec<TabularOutput>,
}

impl ChunkOutput {
    pub fn from_result(result: ExecutionResult) -> Self {
        ChunkOutput {
            stdout: result.stdout,
            stderr: result.stderr,
            exit_code: result.exit_code,
            success: result.success,
            timed_out: result.timed_out,
            output_truncated: result.output_truncated,
            duration_ms: result.duration_ms,
            plot_png: result.plot_png,
            plot_svg: result.plot_svg,
            tables: result.tables,
        }
    }

    /// The report a command returns. `include_plot` adds the SVG text and the PNG bytes as
    /// base64; `include_tables` adds the captured `wc_table()` tables; the streams are truncated
    /// at [`MAX_REPORTED_TEXT`] each.
    pub fn json(&self, include_plot: bool, include_tables: bool) -> Value {
        let (stdout, stdout_cut) = reported_text(&self.stdout);
        let (stderr, stderr_cut) = reported_text(&self.stderr);
        let (plot_svg, svg_cut) = match (&self.plot_svg, include_plot) {
            (Some(svg), true) => {
                let (text, cut) = reported_text(svg);
                (Some(text), cut)
            }
            _ => (None, false),
        };
        let truncated = self.output_truncated || stdout_cut || stderr_cut || svg_cut;
        let mut out = json!({
            "stdout": stdout,
            "stderr": stderr,
            "exitCode": self.exit_code,
            "success": self.success,
            "timedOut": self.timed_out,
            "outputTruncated": truncated,
            "durationMs": self.duration_ms,
            "hasPlot": self.plot_png.is_some() || self.plot_svg.is_some(),
            "tableCount": self.tables.len(),
        });
        if include_plot {
            if let Some(svg) = plot_svg {
                out["plotSvg"] = json!(svg);
            }
            if let Some(png) = &self.plot_png {
                out["plotPngBase64"] = json!(crate::cmd::insert::base64_encode(png));
            }
        }
        if include_tables && !self.tables.is_empty() {
            out["tables"] = json!(self.tables);
        }
        out
    }
}

/// `s`, cut at a character boundary when it is longer than the report limit, plus whether it was cut.
fn reported_text(s: &str) -> (String, bool) {
    if s.len() <= MAX_REPORTED_TEXT {
        return (s.to_owned(), false);
    }
    let mut end = MAX_REPORTED_TEXT;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s.get(..end).unwrap_or_default().to_owned();
    out.push_str("\n… (truncated)");
    (out, true)
}

/// How a chunk is executed. The default runs `wordcraft-kernel`; tests install a runner that
/// doesn't need R or Python installed.
pub type Runner = Box<dyn FnMut(&ExecutionRequest) -> Result<ExecutionResult, KernelError> + Send>;

fn canonical_language(language: &str) -> String {
    match language.parse::<wordcraft_kernel::CodeLanguage>() {
        Ok(lang) => lang.as_str().to_owned(),
        Err(_) => language.trim().to_ascii_lowercase(),
    }
}

#[derive(Debug, Clone)]
struct ChunkFingerprint {
    language: String,
    code_hash: u64,
    code_len: usize,
}

impl ChunkFingerprint {
    fn new(language: &str, code: &str) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        code.hash(&mut hasher);
        Self { language: canonical_language(language), code_hash: hasher.finish(), code_len: code.len() }
    }

    fn matches(&self, language: &str, code: &str) -> bool {
        self.language == canonical_language(language) && self.code_len == code.len() && self.code_hash == Self::new(language, code).code_hash
    }
}

#[derive(Debug, Clone)]
struct StoredOutput {
    output: ChunkOutput,
    fingerprint: Option<ChunkFingerprint>,
}

/// Per-document chunk outputs, mounted heights, and kernel generation, swapped with [`DocState`].
#[derive(Debug, Default)]
pub(crate) struct DocChunkOutputs {
    outputs: BTreeMap<i64, StoredOutput>,
    heights: BTreeMap<i64, u32>,
    generation: u64,
    rev: u64,
}

/// The kernel's transient state in a session: the active document's last chunk outputs and
/// generation, plus the session-wide runner.
pub struct ChunkState {
    doc: DocChunkOutputs,
    runner: Runner,
}

impl Default for ChunkState {
    fn default() -> Self {
        ChunkState { doc: DocChunkOutputs::default(), runner: Box::new(wordcraft_kernel::execute) }
    }
}

impl ChunkState {
    /// Run one request through the kernel.
    pub fn run(&mut self, request: &ExecutionRequest) -> Result<ExecutionResult, KernelError> {
        (self.runner)(request)
    }

    /// Keep `output` as the last output of chunk `id`.
    pub fn store(&mut self, id: i64, output: ChunkOutput) {
        self.doc.outputs.insert(id, StoredOutput { output, fingerprint: None });
        self.doc.rev = self.doc.rev.wrapping_add(1);
    }

    /// Keep `output` as the last output of chunk `id`, bound to its `language` and `code`.
    pub fn store_for(&mut self, id: i64, language: &str, code: &str, output: ChunkOutput) {
        self.doc.outputs.insert(id, StoredOutput { output, fingerprint: Some(ChunkFingerprint::new(language, code)) });
        self.doc.rev = self.doc.rev.wrapping_add(1);
    }

    /// The last output of chunk `id`, if it ran since the last clear.
    pub fn output(&self, id: i64) -> Option<&ChunkOutput> {
        self.doc.outputs.get(&id).map(|entry| &entry.output)
    }

    /// UI-measured height map (`control_id -> round(pt * 100)`) passed into [`wordcraft_layout::LayoutOptions`].
    pub fn chunk_heights(&self) -> &BTreeMap<i64, u32> {
        &self.doc.heights
    }

    /// Update mounted chunk heights; returns `true` when the map changed.
    pub fn set_chunk_heights(&mut self, heights: BTreeMap<i64, u32>) -> bool {
        if self.doc.heights == heights {
            return false;
        }
        self.doc.heights = heights;
        true
    }

    /// Forget chunk `id`'s last output; whether it had one.
    pub fn clear(&mut self, id: i64) -> bool {
        let removed = self.doc.outputs.remove(&id).is_some();
        let height_removed = self.doc.heights.remove(&id).is_some();
        if removed || height_removed {
            self.doc.rev = self.doc.rev.wrapping_add(1);
        }
        removed
    }

    /// Forget every output; how many there were.
    pub fn clear_all(&mut self) -> usize {
        let n = self.doc.outputs.len();
        self.doc.outputs.clear();
        if n > 0 {
            self.doc.rev = self.doc.rev.wrapping_add(1);
        }
        n
    }

    /// Monotonically bumped on every store, clear, or invalidation affecting chunk outputs.
    pub fn rev(&self) -> u64 {
        self.doc.rev
    }

    /// Drop outputs whose chunk id no longer exists in `doc` or whose language/code changed
    /// (for Undo, Redo, and transaction rollback).
    pub(crate) fn invalidate_for_document(&mut self, doc: &Document) {
        let live: BTreeMap<i64, (String, String)> = doc.chunks().into_iter().filter_map(|c| c.id.map(|id| (id, (c.language, c.code)))).collect();
        let h_before = self.doc.heights.len();
        self.doc.heights.retain(|id, _| live.contains_key(id));
        if self.doc.outputs.is_empty() {
            let _ = h_before;
            return;
        }
        let o_before = self.doc.outputs.len();
        self.doc.outputs.retain(|id, entry| {
            let Some((language, code)) = live.get(id) else { return false };
            entry.fingerprint.as_ref().is_none_or(|fp| fp.matches(language, code))
        });
        if self.doc.outputs.len() != o_before {
            self.doc.rev = self.doc.rev.wrapping_add(1);
        }
    }

    pub(crate) fn doc_outputs_mut(&mut self) -> &mut DocChunkOutputs {
        &mut self.doc
    }

    /// Forget every output and start a new kernel generation; how many outputs there were.
    ///
    /// R and Python run in a fresh interpreter for every chunk today (#4 will make a session
    /// persistent), so restarting cannot leave memory behind yet: it drops the outputs a stale
    /// kernel produced and tells the front end, through the generation, to redraw them.
    pub fn restart(&mut self) -> usize {
        self.doc.generation = self.doc.generation.wrapping_add(1);
        self.clear_all()
    }

    /// The current kernel generation, which every restart bumps.
    pub fn generation(&self) -> u64 {
        self.doc.generation
    }

    /// Replace the kernel with `runner` for tests and front-end producers.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_runner(&mut self, runner: Runner) {
        self.runner = runner;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_text_is_bounded_and_keeps_character_boundaries() {
        assert_eq!(reported_text("ok"), ("ok".to_owned(), false));
        let long = "é".repeat(MAX_REPORTED_TEXT);
        let (cut, truncated) = reported_text(&long);
        assert!(truncated);
        assert!(cut.len() <= MAX_REPORTED_TEXT + 32);
        assert!(cut.ends_with("(truncated)"));
        assert!(cut.is_char_boundary(cut.len()));
    }

    #[test]
    fn json_marks_output_truncated_when_report_cap_cuts_streams_or_svg() {
        let oversized = "x".repeat(MAX_REPORTED_TEXT + 1);

        let long_stdout = ChunkOutput { stdout: oversized.clone(), ..Default::default() };
        assert_eq!(long_stdout.json(false, false)["outputTruncated"], true);

        let long_stderr = ChunkOutput { stderr: oversized.clone(), ..Default::default() };
        assert_eq!(long_stderr.json(false, false)["outputTruncated"], true);

        let long_svg = ChunkOutput { plot_svg: Some(oversized), ..Default::default() };
        assert_eq!(long_svg.json(false, false)["outputTruncated"], false);
        assert_eq!(long_svg.json(true, false)["outputTruncated"], true);

        let kernel_truncated = ChunkOutput { output_truncated: true, ..Default::default() };
        assert_eq!(kernel_truncated.json(false, false)["outputTruncated"], true);
    }

    #[test]
    fn chunk_at_picks_the_innermost_nested_chunk() {
        use wordcraft_doc::{CharProps, CodeChunk, ContentControl, ControlKind, InlineObject};

        let mut doc = Document::new();
        let outer_id = doc.push_chunk(&CodeChunk::new("r", "outer_top\ninner\nouter_bottom")).unwrap();
        let paths = doc.para_paths(StoryRef::Body);
        let inner_path = paths[paths.len() - 2].clone();
        let outer_path = paths[paths.len() - 3].clone();

        let mut inner_control = ContentControl::new(ControlKind::RichText);
        inner_control.id = Some(outer_id + 1);
        inner_control.tag = "wordcraft:chunk:python".into();
        inner_control.block = true;
        let inner_para = doc.para_mut(StoryRef::Body, &inner_path).unwrap();
        inner_para.insert_object(0, InlineObject::ControlStart { control: Box::new(inner_control) }, &CharProps::default()).unwrap();
        inner_para.insert_object(inner_para.len(), InlineObject::ControlEnd, &CharProps::default()).unwrap();

        assert_eq!(doc.chunk_refs(StoryRef::Body).len(), 2);
        let found_inner = chunk_at(&doc, &Pos { story: StoryRef::Body, path: inner_path, off: 2 }).expect("innermost chunk");
        assert_eq!(found_inner.chunk.id, Some(outer_id + 1));
        assert_eq!(found_inner.chunk.language, "python");

        let found_outer = chunk_at(&doc, &Pos { story: StoryRef::Body, path: outer_path, off: 1 }).expect("outer chunk");
        assert_eq!(found_outer.chunk.id, Some(outer_id));
    }

    #[test]
    fn invalidate_preserves_output_for_unchanged_chunk_with_language_alias() {
        use wordcraft_doc::{CodeChunk, Part};

        let mut doc = Document::new();
        let id = doc.push_chunk(&CodeChunk::new("py", "print(1)")).unwrap();
        let part_id = id + 1;
        let mut part_chunk = CodeChunk::new("r", "1 + 1");
        part_chunk.id = Some(part_id);
        let blocks = doc.chunk_blocks(&part_chunk).unwrap();
        doc.parts.insert(1, Part { blocks, ..Default::default() });

        let mut state = ChunkState::default();
        state.store_for(id, "python", "print(1)", ChunkOutput { stdout: "1\n".into(), success: true, ..Default::default() });
        state.store_for(part_id, "r", "1 + 1", ChunkOutput { stdout: "[1] 2\n".into(), success: true, ..Default::default() });

        state.invalidate_for_document(&doc);
        assert_eq!(state.output(id).map(|o| o.stdout.as_str()), Some("1\n"));
        assert_eq!(state.output(part_id).map(|o| o.stdout.as_str()), Some("[1] 2\n"));
    }

    #[test]
    fn restart_forgets_outputs_and_advances_the_generation() {
        let mut state = ChunkState::default();
        state.store(1, ChunkOutput::default());
        state.store(2, ChunkOutput::default());
        assert_eq!(state.restart(), 2);
        assert!(state.output(1).is_none());
        assert_eq!(state.generation(), 1);
        assert_eq!(state.clear_all(), 0);
    }
}
