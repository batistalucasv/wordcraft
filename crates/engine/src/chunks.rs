//! Code chunks: the transient kernel state behind [`wordcraft_doc::chunk`]'s document model.
//!
//! The chunk itself — its language, code and Quarto options — lives in the document as a
//! block-level content control tagged `wordcraft:chunk:<language>` (see [`wordcraft_doc::chunk`]),
//! so saving keeps it. What a run produces is not part of the document: it is kept here, like a
//! notebook's kernel output, and forgotten by `code.clear_output`, `code.restart_kernel`, another
//! run, or opening another document.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};

use serde_json::{Value, json};
use wordcraft_doc::{ChunkBounds, ChunkRef, Document, Pos, StoryRef};

pub use wordcraft_kernel::{
    CodeLanguage, ExecutionRequest, ExecutionResult, KernelError, PlotFormat, PlotOutput, TabularOutput, execute, is_language_available,
};

pub use crate::cmd::code::{ApplyChunkRunOutcome, PlannedChunkRun, RunAllEntry, apply_chunk_run_result, plan_chunk_run, plan_run_all};

/// Default execution limit when a command doesn't pass `timeoutMs`.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Shortest execution limit a command may ask for.
pub const MIN_TIMEOUT_MS: u64 = 100;
/// Longest execution limit a command may ask for (ten minutes).
pub const MAX_TIMEOUT_MS: u64 = 600_000;

/// Longest chunk source that is run, in bytes: the document model's own limit.
pub const MAX_CHUNK_BYTES: usize = wordcraft_doc::chunk::MAX_CHUNK_CODE_BYTES;

/// Longest stdout, stderr or SVG text reported in a command's JSON, in bytes. A runaway script
/// can print more; the rest stays in memory (within [`MAX_RETAINED_OUTPUT_BYTES`]) for the front
/// end but is not echoed to an agent.
const MAX_REPORTED_TEXT: usize = 256 * 1024;

/// Maximum combined retained chunk output bytes kept in memory per open document (64 MiB).
///
/// When storing a new chunk output would exceed this budget, the oldest retained outputs for
/// the document are evicted and [`ChunkState::rev`] advances so mounted widgets drop stale
/// copies. A single output larger than this budget is not retained.
pub const MAX_RETAINED_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// Maximum number of distinct chunk outputs retained per open document, bounding map overhead
/// even when outputs have empty payloads.
const MAX_RETAINED_OUTPUTS: usize = 4_096;

/// Whether `p` is inside the chunk bounds (between its markers, either edge included).
fn bounds_contain(bounds: &ChunkBounds, p: &Pos) -> bool {
    p.story == bounds.start.story
        && (&p.path, p.off) >= (&bounds.start.path, bounds.start.off)
        && (&p.path, p.off) <= (&bounds.end.path, bounds.end.off)
}

/// The innermost code chunk holding `p`, if any.
pub fn chunk_at(doc: &Document, p: &Pos) -> Option<ChunkRef> {
    if !doc.has_chunks(p.story) {
        return None;
    }
    let target = doc.chunk_bounds(p.story).into_iter().filter(|b| bounds_contain(b, p)).max_by(|a, b| {
        (&a.start.path, a.start.off).cmp(&(&b.start.path, b.start.off)).then_with(|| (&b.end.path, b.end.off).cmp(&(&a.end.path, a.end.off)))
    })?;
    if let Some(id) = target.id
        && let Some(r) = doc.chunk_ref_by_id(p.story, id)
        && r.start == target.start
        && r.end == target.end
    {
        return Some(r);
    }
    doc.chunk_refs(p.story).into_iter().find(|r| r.start == target.start && r.end == target.end)
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
    /// All captured plots, in creation order.
    pub plots: Vec<PlotOutput>,
    /// Tables passed to `wc_table()`, in call order.
    pub tables: Vec<TabularOutput>,
}

impl ChunkOutput {
    pub fn from_result(result: ExecutionResult) -> Self {
        let plots = if !result.plots.is_empty() {
            result.plots
        } else if result.plot_png.is_some() || result.plot_svg.is_some() {
            vec![PlotOutput { png: result.plot_png.clone(), svg: result.plot_svg.clone() }]
        } else {
            Vec::new()
        };
        let plot_png = result.plot_png.or_else(|| plots.first().and_then(|p| p.png.clone()));
        let plot_svg = result.plot_svg.or_else(|| plots.first().and_then(|p| p.svg.clone()));
        ChunkOutput {
            stdout: result.stdout,
            stderr: result.stderr,
            exit_code: result.exit_code,
            success: result.success,
            timed_out: result.timed_out,
            output_truncated: result.output_truncated,
            duration_ms: result.duration_ms,
            plot_png,
            plot_svg,
            plots,
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
        let plot_count = if !self.plots.is_empty() { self.plots.len() } else { usize::from(self.plot_png.is_some() || self.plot_svg.is_some()) };
        let mut out = json!({
            "stdout": stdout,
            "stderr": stderr,
            "exitCode": self.exit_code,
            "success": self.success,
            "timedOut": self.timed_out,
            "outputTruncated": truncated,
            "durationMs": self.duration_ms,
            "hasPlot": plot_count > 0,
            "plotCount": plot_count,
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

    /// Approximate heap memory retained by this output, in bytes.
    pub fn retained_bytes(&self) -> usize {
        let mut bytes = self.stdout.len().saturating_add(self.stderr.len());
        if let Some(png) = &self.plot_png {
            bytes = bytes.saturating_add(png.len());
        }
        if let Some(svg) = &self.plot_svg {
            bytes = bytes.saturating_add(svg.len());
        }
        bytes = bytes.saturating_add(self.plots.len().saturating_mul(std::mem::size_of::<PlotOutput>()));
        for plot in &self.plots {
            if let Some(png) = &plot.png {
                bytes = bytes.saturating_add(png.len());
            }
            if let Some(svg) = &plot.svg {
                bytes = bytes.saturating_add(svg.len());
            }
        }
        bytes = bytes.saturating_add(self.tables.len().saturating_mul(std::mem::size_of::<TabularOutput>()));
        for table in &self.tables {
            bytes = bytes.saturating_add(table.headers.len().saturating_mul(std::mem::size_of::<String>()));
            for header in &table.headers {
                bytes = bytes.saturating_add(header.len());
            }
            bytes = bytes.saturating_add(table.rows.len().saturating_mul(std::mem::size_of::<Vec<String>>()));
            for row in &table.rows {
                bytes = bytes.saturating_add(row.len().saturating_mul(std::mem::size_of::<String>()));
                for cell in row {
                    bytes = bytes.saturating_add(cell.len());
                }
            }
        }
        bytes
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

/// How a chunk is executed. The default runs `wordcraft-kernel`; tests install a stub runner that
/// never spawns `Rscript` or `python3` unless overridden via [`ChunkState::set_runner`].
pub type Runner = Box<dyn FnMut(&ExecutionRequest) -> Result<ExecutionResult, KernelError> + Send>;

fn canonical_language(language: &str) -> String {
    match language.parse::<CodeLanguage>() {
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
    rev: u64,
    bytes: usize,
}

/// Per-document chunk outputs, mounted heights, and kernel generation, swapped with [`DocState`].
#[derive(Debug)]
pub(crate) struct DocChunkOutputs {
    outputs: BTreeMap<i64, StoredOutput>,
    /// Insertion/update order `(chunk_id, entry_rev)` for FIFO eviction when the budget is exceeded.
    order: VecDeque<(i64, u64)>,
    retained_bytes: usize,
    max_retained_bytes: usize,
    heights: BTreeMap<i64, u32>,
    generation: u64,
    rev: u64,
    heights_rev: u64,
}

impl Default for DocChunkOutputs {
    fn default() -> Self {
        Self {
            outputs: BTreeMap::new(),
            order: VecDeque::new(),
            retained_bytes: 0,
            max_retained_bytes: MAX_RETAINED_OUTPUT_BYTES,
            heights: BTreeMap::new(),
            generation: 0,
            rev: 0,
            heights_rev: 0,
        }
    }
}

/// The kernel's transient state in a session: the active document's last chunk outputs and
/// generation, plus the session-wide runner.
pub struct ChunkState {
    doc: DocChunkOutputs,
    runner: Runner,
}

impl Default for ChunkState {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkState {
    /// Create a fresh chunk state with the default kernel runner.
    ///
    /// Under `cfg(test)`, installs a non-spawning stub runner so tests (including command fuzzing)
    /// never spawn `Rscript`/`python3` processes unless explicitly overridden via
    /// [`Self::set_runner`].
    pub fn new() -> Self {
        #[cfg(test)]
        let runner: Runner = Box::new(|_| Err(KernelError::Io("kernel execution is disabled in tests unless set_runner is installed".into())));
        #[cfg(not(test))]
        let runner: Runner = Box::new(wordcraft_kernel::execute);
        ChunkState { doc: DocChunkOutputs::default(), runner }
    }

    /// Run one request through the kernel.
    pub fn run(&mut self, request: &ExecutionRequest) -> Result<ExecutionResult, KernelError> {
        (self.runner)(request)
    }

    /// Keep `output` as the last output of chunk `id`, evicting the oldest outputs in the active
    /// document if retaining it would exceed [`Self::max_retained_bytes`].
    pub fn store(&mut self, id: i64, output: ChunkOutput) {
        self.insert_output(id, None, output);
    }

    /// Keep `output` as the last output of chunk `id`, bound to its `language` and `code`,
    /// evicting the oldest outputs in the active document if retaining it would exceed
    /// [`Self::max_retained_bytes`].
    pub fn store_for(&mut self, id: i64, language: &str, code: &str, output: ChunkOutput) {
        self.insert_output(id, Some(ChunkFingerprint::new(language, code)), output);
    }

    /// Combined bytes currently retained across all stored chunk outputs in the active document.
    pub fn retained_bytes(&self) -> usize {
        self.doc.retained_bytes
    }

    /// Maximum combined output bytes retained in memory for the active document.
    pub fn max_retained_bytes(&self) -> usize {
        self.doc.max_retained_bytes
    }

    /// Update the active document's retained output budget, immediately evicting the oldest
    /// entries and advancing [`Self::rev`] if current outputs exceed `limit`.
    pub fn set_max_retained_bytes(&mut self, limit: usize) {
        self.doc.max_retained_bytes = limit;
        let before = self.doc.outputs.len();
        self.evict_for(0, 0);
        if self.doc.outputs.len() != before {
            self.doc.rev = self.doc.rev.wrapping_add(1);
            self.compact_order_if_needed();
        }
    }

    fn insert_output(&mut self, id: i64, fingerprint: Option<ChunkFingerprint>, output: ChunkOutput) {
        let bytes = output.retained_bytes();
        if let Some(prev) = self.doc.outputs.remove(&id) {
            self.doc.retained_bytes = self.doc.retained_bytes.saturating_sub(prev.bytes);
        }
        self.doc.rev = self.doc.rev.wrapping_add(1);
        if bytes > self.doc.max_retained_bytes {
            self.compact_order_if_needed();
            return;
        }
        self.evict_for(bytes, 1);
        let rev = self.doc.rev;
        self.doc.outputs.insert(id, StoredOutput { output, fingerprint, rev, bytes });
        self.doc.retained_bytes = self.doc.retained_bytes.saturating_add(bytes);
        self.doc.order.push_back((id, rev));
        self.compact_order_if_needed();
    }

    fn evict_one_oldest(&mut self) -> bool {
        while let Some((evict_id, evict_rev)) = self.doc.order.pop_front() {
            if self.doc.outputs.get(&evict_id).is_some_and(|e| e.rev == evict_rev)
                && let Some(removed) = self.doc.outputs.remove(&evict_id)
            {
                self.doc.retained_bytes = self.doc.retained_bytes.saturating_sub(removed.bytes);
                return true;
            }
        }
        false
    }

    fn evict_for(&mut self, incoming_bytes: usize, incoming_entries: usize) {
        while self.doc.retained_bytes.saturating_add(incoming_bytes) > self.doc.max_retained_bytes
            || self.doc.outputs.len().saturating_add(incoming_entries) > MAX_RETAINED_OUTPUTS
        {
            if !self.evict_one_oldest() {
                break;
            }
        }
    }

    fn compact_order_if_needed(&mut self) {
        let threshold = self.doc.outputs.len().saturating_mul(2).max(64);
        if self.doc.order.len() > threshold {
            let outputs = &self.doc.outputs;
            self.doc.order.retain(|&(id, rev)| outputs.get(&id).is_some_and(|e| e.rev == rev));
        }
    }

    /// The last output of chunk `id`, if it ran since the last clear.
    pub fn output(&self, id: i64) -> Option<&ChunkOutput> {
        self.doc.outputs.get(&id).map(|entry| &entry.output)
    }

    /// The last output of chunk `id` together with the revision at which it was stored.
    pub fn output_with_rev(&self, id: i64) -> Option<(&ChunkOutput, u64)> {
        self.doc.outputs.get(&id).map(|entry| (&entry.output, entry.rev))
    }

    /// Whether chunk `id`'s stored output matches `language` and `code`.
    pub fn output_matches(&self, id: i64, language: &str, code: &str) -> bool {
        self.doc.outputs.get(&id).is_some_and(|entry| entry.fingerprint.as_ref().is_none_or(|fp| fp.matches(language, code)))
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
        self.doc.heights_rev = self.doc.heights_rev.wrapping_add(1);
        true
    }

    /// Monotonically bumped whenever the mounted chunk heights map changes.
    pub fn heights_rev(&self) -> u64 {
        self.doc.heights_rev
    }

    /// Forget chunk `id`'s last output; whether it had one.
    pub fn clear(&mut self, id: i64) -> bool {
        let removed = if let Some(entry) = self.doc.outputs.remove(&id) {
            self.doc.retained_bytes = self.doc.retained_bytes.saturating_sub(entry.bytes);
            self.doc.rev = self.doc.rev.wrapping_add(1);
            self.compact_order_if_needed();
            true
        } else {
            false
        };
        if self.doc.heights.remove(&id).is_some() {
            self.doc.heights_rev = self.doc.heights_rev.wrapping_add(1);
        }
        removed
    }

    /// Forget every output (and mounted height); returns how many outputs there were.
    pub fn clear_all(&mut self) -> usize {
        let n = self.doc.outputs.len();
        self.doc.outputs.clear();
        self.doc.order.clear();
        self.doc.retained_bytes = 0;
        if n > 0 {
            self.doc.rev = self.doc.rev.wrapping_add(1);
        }
        if !self.doc.heights.is_empty() {
            self.doc.heights.clear();
            self.doc.heights_rev = self.doc.heights_rev.wrapping_add(1);
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
        if self.doc.outputs.is_empty() && self.doc.heights.is_empty() {
            return;
        }
        if !doc.has_any_chunks() {
            self.clear_all();
            return;
        }
        if self.doc.outputs.is_empty() {
            let mut live_ids = HashSet::new();
            for story in std::iter::once(StoryRef::Body).chain(doc.parts.keys().map(|id| StoryRef::Part(*id))) {
                for b in doc.chunk_bounds(story) {
                    if let Some(id) = b.id {
                        live_ids.insert(id);
                    }
                }
            }
            let h_before = self.doc.heights.len();
            self.doc.heights.retain(|id, _| live_ids.contains(id));
            if self.doc.heights.len() != h_before {
                self.doc.heights_rev = self.doc.heights_rev.wrapping_add(1);
            }
            return;
        }
        let live: BTreeMap<i64, (String, String)> = doc.chunks().into_iter().filter_map(|c| c.id.map(|id| (id, (c.language, c.code)))).collect();
        let h_before = self.doc.heights.len();
        self.doc.heights.retain(|id, _| live.contains_key(id));
        if self.doc.heights.len() != h_before {
            self.doc.heights_rev = self.doc.heights_rev.wrapping_add(1);
        }
        let o_before = self.doc.outputs.len();
        self.doc.outputs.retain(|id, entry| {
            let Some((language, code)) = live.get(id) else { return false };
            entry.fingerprint.as_ref().is_none_or(|fp| fp.matches(language, code))
        });
        if self.doc.outputs.len() != o_before {
            self.doc.retained_bytes = self.doc.outputs.values().fold(0usize, |acc, entry| acc.saturating_add(entry.bytes));
            let outputs = &self.doc.outputs;
            self.doc.order.retain(|&(id, rev)| outputs.get(&id).is_some_and(|e| e.rev == rev));
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
    fn default_runner_under_cfg_test_does_not_spawn_interpreters() {
        let mut state = ChunkState::new();
        let req = ExecutionRequest::new(CodeLanguage::R, "1 + 1");
        let err = state.run(&req).expect_err("cfg(test) stub runner must not spawn interpreters");
        assert!(matches!(err, KernelError::Io(_)), "{err:?}");
    }

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
        assert!(state.set_chunk_heights(BTreeMap::from([(1, 4200)])));
        let h_rev_before = state.heights_rev();
        assert_eq!(state.restart(), 2);
        assert!(state.output(1).is_none());
        assert!(state.chunk_heights().is_empty());
        assert!(state.heights_rev() > h_rev_before);
        assert_eq!(state.retained_bytes(), 0);
        assert_eq!(state.generation(), 1);
        assert_eq!(state.clear_all(), 0);
    }

    #[test]
    fn retained_output_budget_evicts_oldest_entries_and_bumps_rev() {
        let mut state = ChunkState::default();
        state.set_max_retained_bytes(100);

        state.store(1, ChunkOutput { stdout: "a".repeat(40), ..Default::default() });
        state.store(2, ChunkOutput { stdout: "b".repeat(40), ..Default::default() });
        let rev_after_two = state.rev();
        assert_eq!(state.retained_bytes(), 80);

        // Refreshing chunk 1 makes chunk 2 the oldest entry without evicting it yet.
        state.store(1, ChunkOutput { stdout: "a".repeat(50), ..Default::default() });
        assert_eq!(state.retained_bytes(), 90);
        assert!(state.output(1).is_some());
        assert!(state.output(2).is_some());
        let rev_after_refresh = state.rev();
        assert!(rev_after_refresh > rev_after_two);

        // Storing chunk 3 overflows the 100-byte budget and evicts chunk 2 (now oldest).
        state.store(3, ChunkOutput { stdout: "c".repeat(40), ..Default::default() });
        assert!(state.retained_bytes() <= state.max_retained_bytes());
        assert_eq!(state.retained_bytes(), 90);
        assert!(state.output(2).is_none(), "oldest untouched chunk must be evicted");
        assert!(state.output_with_rev(2).is_none());
        assert!(state.output(1).is_some());
        assert!(state.output(3).is_some());
        assert!(state.rev() > rev_after_refresh, "eviction must advance rev so mounted widgets clear stale copies");

        // Shrinking the budget immediately evicts older entries and advances rev.
        let rev_before_shrink = state.rev();
        state.set_max_retained_bytes(50);
        assert!(state.retained_bytes() <= 50);
        assert!(state.output(1).is_none());
        assert!(state.output(3).is_some());
        assert!(state.rev() > rev_before_shrink);
    }

    #[test]
    fn single_output_exceeding_budget_drops_prior_entry_and_bumps_rev() {
        let mut state = ChunkState::default();
        state.set_max_retained_bytes(100);

        state.store(1, ChunkOutput { stdout: "a".repeat(40), ..Default::default() });
        state.store(2, ChunkOutput { stdout: "b".repeat(40), ..Default::default() });
        let rev_before = state.rev();

        // An oversized update to chunk 1 must not retain stale output for 1 or exceed budget,
        // while keeping unrelated chunk 2 within budget and bumping rev.
        state.store(1, ChunkOutput { stdout: "x".repeat(120), ..Default::default() });
        assert!(state.output(1).is_none());
        assert!(state.output(2).is_some());
        assert_eq!(state.retained_bytes(), 40);
        assert!(state.rev() > rev_before);

        // An oversized store for a new chunk id also advances rev even when nothing was replaced.
        let rev_before_new = state.rev();
        state.store(3, ChunkOutput { stdout: "y".repeat(120), ..Default::default() });
        assert!(state.output(3).is_none());
        assert_eq!(state.retained_bytes(), 40);
        assert!(state.rev() > rev_before_new);
    }

    #[test]
    fn retained_bytes_tracks_plots_tables_and_reclaims_on_clear_and_invalidation() {
        use wordcraft_doc::CodeChunk;

        let plot_and_table = ChunkOutput {
            stdout: "out".into(),
            stderr: "err".into(),
            plot_png: Some(vec![1; 32]),
            plot_svg: Some("<svg/>".into()),
            plots: vec![PlotOutput { png: Some(vec![2; 16]), svg: Some("<svg></svg>".into()) }],
            tables: vec![TabularOutput { headers: vec!["h".into(), String::new()], rows: vec![vec![String::new(), "v".into()]] }],
            ..Default::default()
        };
        assert!(plot_and_table.retained_bytes() > 3 + 3 + 32 + 6 + 16 + 11 + 2);

        let mut doc = Document::new();
        let id1 = doc.push_chunk(&CodeChunk::new("r", "1")).unwrap();
        let id2 = doc.push_chunk(&CodeChunk::new("r", "2")).unwrap();

        let mut state = ChunkState::default();
        state.set_max_retained_bytes(100);
        state.store_for(id1, "r", "1", ChunkOutput { stdout: "a".repeat(60), ..Default::default() });
        assert_eq!(state.retained_bytes(), 60);
        assert!(state.clear(id1));
        assert_eq!(state.retained_bytes(), 0);

        state.store_for(id1, "r", "1", ChunkOutput { stdout: "a".repeat(60), ..Default::default() });
        state.store_for(id2, "r", "2", ChunkOutput { stdout: "b".repeat(40), ..Default::default() });
        assert_eq!(state.retained_bytes(), 100);

        // Changing chunk 1 invalidates its cached output and reclaims its bytes.
        doc.update_chunk(id1, "r", "1 + 1").unwrap();
        state.invalidate_for_document(&doc);
        assert!(state.output(id1).is_none());
        assert!(state.output(id2).is_some());
        assert_eq!(state.retained_bytes(), 40);
    }
}
