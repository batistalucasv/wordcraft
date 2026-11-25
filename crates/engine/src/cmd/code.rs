//! Insert › Code: code chunks (R and Python) and the commands that run them.
//!
//! The chunk is the document model's ([`wordcraft_doc::chunk`]): a block-level content control
//! tagged `wordcraft:chunk:<language>` whose content is the code and whose Quarto options live in
//! a custom property. Running executes the code with `wordcraft-kernel` in an isolated process,
//! bounded by `timeoutMs`. The command is the same one the ribbon, the shortcuts, the control
//! channel, the CLI and MCP agents call.

use std::str::FromStr;

use serde_json::{Map, Value, json};
use wordcraft_doc::chunk::{CODE_OUTPUT_STYLE, MAX_CHUNK_LINES};
use wordcraft_doc::para::OBJ;
use wordcraft_doc::table::{Cell, MAX_COLS, MAX_ROWS, Table};
use wordcraft_doc::{Block, CharProps, ChunkOptions, ChunkRef, CodeChunk, Paragraph, Path, Pos, RevisionKind, StoryRef, para_block};
use wordcraft_kernel::{CodeLanguage, ExecutionRequest, ExecutionResult, PlotFormat};

use super::{delete_selection, insert, pos_json, sel_result};
use crate::chunks::{self, ChunkOutput};
use crate::{CmdError, CmdResult, CommandSpec, Selection, Session, p};

const LOC: &str = "Insert › Code";

/// Validated, self-contained execution plan for a single code chunk.
#[derive(Debug, Clone)]
pub struct PlannedChunkRun {
    /// Active document id when planned; checked by [`apply_chunk_run_result`] against cross-document apply.
    pub doc_id: u64,
    /// Kernel generation when planned; checked by [`apply_chunk_run_result`] against post-restart apply.
    pub kernel_gen: u64,
    /// Content-control id of the target chunk in the document body.
    pub chunk_id: i64,
    /// Raw language tag stored on the document chunk (`"r"`, `"python"`, ...).
    pub doc_language: String,
    /// Kernel execution request ready to run synchronously or on a UI worker thread.
    pub request: ExecutionRequest,
    /// Whether command JSON reporting should include `plotSvg` / `plotPngBase64`.
    pub include_plot: bool,
    /// Whether command JSON reporting should include captured `wc_table()` tables.
    pub include_tables: bool,
}

/// One document-order entry produced by [`plan_run_all`].
#[derive(Debug, Clone)]
pub enum RunAllEntry {
    /// Chunk passed validation and is ready to execute.
    Run(PlannedChunkRun),
    /// Chunk was skipped before execution (for example `eval: false`).
    Skipped { chunk_id: Option<i64>, language: String, reason: &'static str },
    /// Chunk was refused before execution (missing options, unsupported language, or invalid code).
    Rejected { chunk_id: Option<i64>, language: String, error: String },
}

/// Result of applying a completed [`PlannedChunkRun`] to a [`Session`].
#[derive(Debug, Clone, PartialEq)]
pub enum ApplyChunkRunOutcome {
    /// The result matched the active document, kernel generation, and live chunk source and was stored.
    Applied { chunk_id: i64, success: bool, output_rev: Option<u64>, reported: Value },
    /// Document, kernel generation, or chunk changed before completion; nothing was stored.
    Stale(&'static str),
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("code.insert_chunk", "Code Chunk", LOC, insert_chunk)
            .key("Mod+Shift+Alt+I")
            .params(
                r#"{"language"?: "r"|"python" (default "r"), "code"?: string (at most 4 MiB), "options"?: object (Quarto chunk options, e.g. {"eval": false, "fig-cap": "A plot"})}"#,
            ),
        CommandSpec::new("code.run_chunk", "Run Code Chunk", LOC, run_chunk)
            .key("Mod+Alt+Enter")
            .params(
                r#"{"chunkId"?: n (control id; default: chunk at cursor), "timeoutMs"?: n (100…600000, default 30000), "plot"?: "png"|"svg"|"both"|"none" (default "png"), "includePlot"?: bool (adds SVG text and PNG base64 to the result), "includeTables"?: bool (adds captured wc_table() tables to the result)}"#,
            )
            .when(has_chunks),
        CommandSpec::new("code.run_all", "Run All Code Chunks", LOC, run_all)
            .key("Mod+Shift+Alt+Enter")
            .params(
                r#"{"timeoutMs"?: n, "plot"?: "png"|"svg"|"both"|"none", "includePlot"?: bool, "includeTables"?: bool, "stopOnError"?: bool (default true; `eval: false` chunks are skipped)}"#,
            )
            .when(has_chunks),
        CommandSpec::new("code.restart_kernel", "Restart Kernel", LOC, restart_kernel).params("{}").pure(),
        CommandSpec::new("code.clear_output", "Clear Code Output", LOC, clear_output)
            .params(r#"{"chunkId"?: n (control id; default: chunk at cursor), "all"?: bool}"#)
            .pure(),
        CommandSpec::new("code.update_chunk", "Update Code Chunk", LOC, update_chunk)
            .params(r#"{"chunkId"?: n (control id; default: chunk at cursor), "code"?: string (at most 4 MiB), "language"?: "r"|"python", "label"?: string (optional chunk name; empty clears)}"#)
            .when(has_any_chunks),
        CommandSpec::new("code.delete_chunk", "Delete Code Chunk", LOC, delete_chunk)
            .params(r#"{"chunkId"?: n (control id; default: chunk at cursor)}"#)
            .when(has_any_chunks),
        CommandSpec::new("code.exit_below_chunk", "Continue After Code Chunk", LOC, exit_below_chunk)
            .params(r#"{"chunkId"?: n (control id; default: chunk at cursor)}"#)
            .when(has_any_chunks),
        CommandSpec::new("code.insert_plot", "Insert Chunk Plot", LOC, insert_plot)
            .params(r#"{"chunkId"?: n (control id; default: chunk at cursor), "plotIndex"?: n (0-based plot index), "png"?: string (base64 PNG), "svg"?: string (SVG XML to archive), "width"?: n (points), "alt"?: string, "atCaret"?: bool}"#),
        CommandSpec::new("code.insert_table", "Insert Chunk Table", LOC, insert_table)
            .params(r#"{"chunkId"?: n (control id; default: chunk at cursor), "headers": [string], "rows"?: [[string]], "atCaret"?: bool}"#),
        CommandSpec::new("code.insert_output", "Insert Chunk Output", LOC, insert_output)
            .params(r#"{"chunkId"?: n (control id; default: chunk at cursor), "text"?: string, "atCaret"?: bool}"#),
    ]
}

fn has_any_chunks(s: &Session) -> Option<&'static str> {
    if s.doc.has_chunks(StoryRef::Body) { None } else { Some("the document has no code chunks") }
}

fn has_chunks(s: &Session) -> Option<&'static str> {
    if crate::file_access::current().is_some() {
        return Some("code execution is disabled under automation filesystem confinement");
    }
    has_any_chunks(s)
}

fn ensure_unconfined() -> Result<(), CmdError> {
    if crate::file_access::current().is_some() {
        return Err(CmdError::Disabled("code execution is disabled under automation filesystem confinement".into()));
    }
    Ok(())
}

fn chunk_language(v: &Value) -> Result<CodeLanguage, CmdError> {
    let Some(name) = p::str(v, "language") else { return Ok(CodeLanguage::R) };
    if name.len() > 16 {
        return Err(CmdError::Params("language: expected \"r\" or \"python\"".into()));
    }
    CodeLanguage::from_str(name).map_err(|e| CmdError::Params(e.to_string()))
}

/// The `options` object as Quarto writes it: scalar values keep their text spelling.
fn chunk_options(v: &Value) -> ChunkOptions {
    let mut options = ChunkOptions::new();
    let Some(Value::Object(map)) = v.get("options") else { return options };
    for (name, value) in map {
        let raw = match value {
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            // Object and array options have no chunk-option spelling here; the model keeps
            // strings only.
            Value::Null | Value::Array(_) | Value::Object(_) => continue,
        };
        options.set(name, raw);
    }
    options
}

fn options_json(options: &ChunkOptions) -> Value {
    let mut out = Map::new();
    for (name, value) in options.iter() {
        out.insert(name.to_owned(), json!(value));
    }
    Value::Object(out)
}

/// Insert a chunk as a block-level content control at the caret.
fn insert_chunk(s: &mut Session, v: &Value) -> CmdResult {
    let language = chunk_language(v)?;
    let (start, end) = s.sel.ordered();
    if start.story != StoryRef::Body || end.story != StoryRef::Body {
        return Err(CmdError::Disabled("code chunks go in the document body".into()));
    }
    // A chunk is read from the story's top-level paragraphs, one line each. Validate the
    // selection before deleting it: a refused insert must not destroy what it refused.
    if start.path.0.len() != 1 || end.path.0.len() != 1 {
        return Err(CmdError::Disabled("code chunks go in the document body, not inside a table".into()));
    }
    // A chunk inside another control is not top-level, so it could not be read back: insert after
    // a chunk, and refuse any other control.
    if chunks::chunk_at(&s.doc, &start).is_none() && s.doc.control_at(&start).is_some() {
        return Err(CmdError::Disabled("code chunks cannot go inside a content control".into()));
    }
    let mut chunk = CodeChunk::new(language.as_str(), p::str(v, "code").unwrap_or(""));
    chunk.options = chunk_options(v);
    // The model refuses an oversized, overly long, or U+FFFC-containing chunk; check before
    // deleting the selection so a refused insert does not destroy the text it would replace.
    validate_code_text(&chunk.code)?;

    let at = delete_selection(s)?;
    let inside = chunks::chunk_at(&s.doc, &at);
    let blocks = s.doc.chunk_blocks(&chunk)?;
    // The chunk gets paragraphs of its own, below the caret's unless that one is empty; from
    // inside a chunk it goes after the chunk's last line.
    let index = match &inside {
        Some(range) => range.end.path.last().saturating_add(1),
        None if s.doc.para_at(&at).is_some_and(|p| p.is_empty()) => at.path.last(),
        None => at.path.last().saturating_add(1),
    };
    for (offset, block) in blocks.into_iter().enumerate() {
        let path = at.path.with_last(index.saturating_add(offset));
        s.doc.insert_block(StoryRef::Body, &path, (*block).clone())?;
    }
    let inserted = find_inserted_chunk(&s.doc, index)?;
    if let Some(id) = inserted.chunk.id {
        s.chunks.clear(id);
    }
    s.sel = Selection::caret(inserted.end.clone());
    s.goal_x = None;
    let mut out = sel_result(s)?;
    out["chunk"] = json!({
        "id": inserted.chunk.id,
        "language": inserted.chunk.language,
        "tag": inserted.chunk.tag(),
        "lines": inserted.chunk.code.split('\n').count(),
        "options": options_json(&inserted.chunk.options),
    });
    Ok(out)
}

fn find_inserted_chunk(doc: &wordcraft_doc::Document, index: usize) -> Result<ChunkRef, CmdError> {
    if let Some(bounds) = doc.chunk_bounds(StoryRef::Body).into_iter().find(|b| b.start.path.0.len() == 1 && b.start.path.last() == index)
        && let Some(id) = bounds.id
        && let Some(found) = doc.chunk_ref_by_id(StoryRef::Body, id)
        && found.start == bounds.start
    {
        return Ok(found);
    }
    doc.chunk_refs(StoryRef::Body)
        .into_iter()
        .find(|r| r.start.path.0.len() == 1 && r.start.path.last() == index)
        .ok_or_else(|| CmdError::Failed("the inserted chunk could not be read back".into()))
}

/// A chunk's control id; one loaded without an id gets one so its output can be kept.
fn chunk_id(s: &mut Session, range: &ChunkRef) -> i64 {
    if let Some(id) = range.chunk.id {
        return id;
    }
    let id = s.doc.next_control_id();
    if let Some(control) = s.doc.control_mut(&range.start) {
        control.id = Some(id);
    }
    id
}

fn runnable_language(range: &ChunkRef) -> Result<CodeLanguage, CmdError> {
    CodeLanguage::from_str(&range.chunk.language).map_err(|e| CmdError::Failed(e.to_string()))
}

fn timeout_ms(v: &Value) -> u64 {
    p::u64(v, "timeoutMs").unwrap_or(chunks::DEFAULT_TIMEOUT_MS).clamp(chunks::MIN_TIMEOUT_MS, chunks::MAX_TIMEOUT_MS)
}

/// Parse `"plot": "png" | "svg" | "both" | "none"`, falling back to `default` when omitted or null.
pub fn parse_plot_option(v: &Value, default: Option<PlotFormat>) -> Result<Option<PlotFormat>, CmdError> {
    match v.get("plot") {
        None | Some(Value::Null) => Ok(default),
        Some(Value::String(s)) => match s.as_str() {
            "none" => Ok(None),
            "png" => Ok(Some(PlotFormat::Png)),
            "svg" => Ok(Some(PlotFormat::Svg)),
            "both" => Ok(Some(PlotFormat::Both)),
            other => Err(CmdError::Params(format!("plot: expected \"png\", \"svg\", \"both\" or \"none\", not `{other}`"))),
        },
        Some(_) => Err(CmdError::Params("plot: expected \"png\", \"svg\", \"both\" or \"none\"".into())),
    }
}

fn optional_chunk_id(v: &Value) -> Result<Option<i64>, CmdError> {
    match v.get("chunkId") {
        None | Some(Value::Null) => Ok(None),
        Some(raw) => raw.as_i64().filter(|&id| id > 0).map(Some).ok_or_else(|| CmdError::Params("chunkId: expected a positive integer".into())),
    }
}

fn find_chunk(s: &Session, id: i64) -> Result<ChunkRef, CmdError> {
    s.doc.chunk_ref_by_id(StoryRef::Body, id).ok_or_else(|| CmdError::Failed(format!("no code chunk with id {id}")))
}

fn build_planned_run(
    s: &mut Session,
    range: &ChunkRef,
    timeout_ms: u64,
    plot: Option<PlotFormat>,
    include_plot: bool,
    include_tables: bool,
) -> Result<PlannedChunkRun, CmdError> {
    let language = runnable_language(range)?;
    if range.chunk.code.trim().is_empty() {
        return Err(CmdError::Failed("the code chunk is empty".into()));
    }
    if range.chunk.code.len() > chunks::MAX_CHUNK_BYTES {
        return Err(CmdError::Failed(format!("the code chunk is longer than {} bytes", chunks::MAX_CHUNK_BYTES)));
    }
    let id = chunk_id(s, range);
    let (capture, fmt) = match plot {
        None => (false, PlotFormat::Png),
        Some(fmt) => (true, fmt),
    };
    let request = ExecutionRequest::new(language, range.chunk.code.clone()).with_timeout(timeout_ms).with_plot_capture(capture).with_plot_format(fmt);
    Ok(PlannedChunkRun {
        doc_id: s.document_id(),
        kernel_gen: s.chunks.generation(),
        chunk_id: id,
        doc_language: range.chunk.language.clone(),
        request,
        include_plot,
        include_tables,
    })
}

/// Validate and build a single-chunk execution plan from `s` and `v`.
///
/// Enforces automation confinement, target resolution (`chunkId` or caret), `plot`, `timeoutMs`,
/// `CodeLanguage`, non-empty/bounded source, assigns a control id if missing, and stamps
/// `(doc_id, kernel_gen)` for [`apply_chunk_run_result`].
pub fn plan_chunk_run(s: &mut Session, v: &Value) -> Result<PlannedChunkRun, CmdError> {
    ensure_unconfined()?;
    let range = resolve_target_chunk(s, v)?;
    let plot = parse_plot_option(v, Some(PlotFormat::Png))?;
    let timeout = timeout_ms(v);
    let include_plot = p::bool(v, "includePlot").unwrap_or(false);
    let include_tables = p::bool(v, "includeTables").unwrap_or(false);
    build_planned_run(s, &range, timeout, plot, include_plot, include_tables)
}

/// Validate and build a document-order `code.run_all` plan from `s` and `v`.
///
/// Enforces automation confinement, non-empty body chunk presence, `plot`, `timeoutMs`, per-chunk
/// `eval: false` skipping, `chunk_options_missing` refusal, `CodeLanguage`, and source validation.
pub fn plan_run_all(s: &mut Session, v: &Value) -> Result<Vec<RunAllEntry>, CmdError> {
    ensure_unconfined()?;
    if !s.doc.has_chunks(StoryRef::Body) {
        return Err(CmdError::Disabled("the document has no code chunks".into()));
    }
    let plot = parse_plot_option(v, Some(PlotFormat::Png))?;
    let timeout = timeout_ms(v);
    let include_plot = p::bool(v, "includePlot").unwrap_or(false);
    let include_tables = p::bool(v, "includeTables").unwrap_or(false);
    let ranges = chunks::chunks_in(&s.doc, StoryRef::Body);
    let mut entries = Vec::with_capacity(ranges.len());
    for range in ranges {
        if !range.chunk.options.eval() {
            entries.push(RunAllEntry::Skipped { chunk_id: range.chunk.id, language: range.chunk.language, reason: "eval: false" });
            continue;
        }
        if s.doc.chunk_options_missing(&range) {
            entries.push(RunAllEntry::Rejected {
                chunk_id: range.chunk.id,
                language: range.chunk.language,
                error: "code chunk options are missing; refusing to assume eval: true".into(),
            });
            continue;
        }
        match build_planned_run(s, &range, timeout, plot, include_plot, include_tables) {
            Ok(planned) => entries.push(RunAllEntry::Run(planned)),
            Err(e) => entries.push(RunAllEntry::Rejected { chunk_id: range.chunk.id, language: range.chunk.language, error: e.to_string() }),
        }
    }
    Ok(entries)
}

fn stale_reason(s: &Session, planned: &PlannedChunkRun) -> Option<&'static str> {
    if s.document_id() != planned.doc_id {
        return Some("the active document changed before the chunk run completed");
    }
    if s.chunks.generation() != planned.kernel_gen {
        return Some("the kernel was restarted before the chunk run completed");
    }
    let Some(live) = s.doc.chunk_ref_by_id(StoryRef::Body, planned.chunk_id) else {
        return Some("the code chunk was removed before the run completed");
    };
    let same_lang = CodeLanguage::from_str(&live.chunk.language).ok() == Some(planned.request.language);
    if !same_lang || live.chunk.code != planned.request.code {
        return Some("the code chunk changed before the run completed");
    }
    None
}

/// Apply a completed [`PlannedChunkRun`] result to `s`.
///
/// Guarded against stale results from another document (`doc_id`), a restarted kernel
/// (`kernel_gen`), or a deleted/modified chunk.
pub fn apply_chunk_run_result(
    s: &mut Session,
    planned: &PlannedChunkRun,
    result: Result<ExecutionResult, String>,
) -> Result<ApplyChunkRunOutcome, CmdError> {
    if let Some(why) = stale_reason(s, planned) {
        return Ok(ApplyChunkRunOutcome::Stale(why));
    }
    let exec = match result {
        Ok(exec) => exec,
        Err(err) => {
            s.chunks.clear(planned.chunk_id);
            return Err(CmdError::Failed(err));
        }
    };
    let success = exec.success;
    let output = ChunkOutput::from_result(exec);
    let reported = output.json(planned.include_plot, planned.include_tables);
    s.chunks.store_for(planned.chunk_id, planned.request.language.as_str(), &planned.request.code, output);
    let output_rev = s.chunks.output_with_rev(planned.chunk_id).map(|(_, rev)| rev);
    s.status = if success { format!("{} chunk ran", planned.request.language) } else { format!("{} chunk failed", planned.request.language) };
    Ok(ApplyChunkRunOutcome::Applied { chunk_id: planned.chunk_id, success, output_rev, reported })
}

fn run_chunk(s: &mut Session, v: &Value) -> CmdResult {
    let planned = plan_chunk_run(s, v)?;
    let res = s.chunks.run(&planned.request).map_err(|e| e.to_string());
    match apply_chunk_run_result(s, &planned, res)? {
        ApplyChunkRunOutcome::Applied { chunk_id, success, reported, .. } => Ok(json!({
            "chunk": {"id": chunk_id, "language": planned.doc_language, "success": success},
            "output": reported,
        })),
        ApplyChunkRunOutcome::Stale(why) => Err(CmdError::Failed(why.into())),
    }
}

fn run_all(s: &mut Session, v: &Value) -> CmdResult {
    let entries = plan_run_all(s, v)?;
    let stop = p::bool(v, "stopOnError").unwrap_or(true);
    let mut results = Vec::with_capacity(entries.len());
    let mut success = true;
    let mut stopped = false;
    for entry in entries {
        match entry {
            RunAllEntry::Skipped { chunk_id, language, reason } => {
                results.push(json!({"chunk": {"id": chunk_id, "language": language, "skipped": reason}}));
            }
            RunAllEntry::Rejected { chunk_id, language, error } => {
                success = false;
                results.push(json!({"chunk": {"id": chunk_id, "language": language, "success": false}, "error": error}));
                if stop {
                    stopped = true;
                    break;
                }
            }
            RunAllEntry::Run(planned) => {
                let res = s.chunks.run(&planned.request).map_err(|e| e.to_string());
                let json_entry = match apply_chunk_run_result(s, &planned, res) {
                    Ok(ApplyChunkRunOutcome::Applied { chunk_id, success: ok, reported, .. }) => {
                        success &= ok;
                        json!({"chunk": {"id": chunk_id, "language": planned.doc_language, "success": ok}, "output": reported})
                    }
                    Ok(ApplyChunkRunOutcome::Stale(why)) => {
                        success = false;
                        json!({"chunk": {"id": planned.chunk_id, "language": planned.doc_language, "success": false}, "error": why})
                    }
                    Err(e) => {
                        success = false;
                        json!({"chunk": {"id": planned.chunk_id, "language": planned.doc_language, "success": false}, "error": e.to_string()})
                    }
                };
                results.push(json_entry);
                if !success && stop {
                    stopped = true;
                    break;
                }
            }
        }
    }
    Ok(json!({"chunks": results, "success": success, "stopped": stopped}))
}

fn restart_kernel(s: &mut Session, _v: &Value) -> CmdResult {
    let cleared = s.chunks.restart();
    s.status = "Kernel restarted".into();
    Ok(json!({
        "restarted": true,
        "generation": s.chunks.generation(),
        "outputsCleared": cleared,
        "persistentSessions": false,
    }))
}

fn clear_output(s: &mut Session, v: &Value) -> CmdResult {
    if p::bool(v, "all").unwrap_or(false) {
        return Ok(json!({"cleared": s.chunks.clear_all()}));
    }
    if let Some(id) = optional_chunk_id(v)? {
        let cleared = usize::from(s.chunks.clear(id));
        let mut out = json!({"cleared": cleared, "chunk": id});
        if let Some(bounds) = s.doc.chunk_bounds(StoryRef::Body).into_iter().find(|b| b.id == Some(id)) {
            out["start"] = pos_json(&bounds.start);
        }
        return Ok(out);
    }
    let Some(range) = chunks::chunk_at(&s.doc, &s.sel.focus) else { return Ok(json!({"cleared": 0})) };
    let Some(id) = range.chunk.id else { return Ok(json!({"cleared": 0})) };
    let cleared = usize::from(s.chunks.clear(id));
    Ok(json!({"cleared": cleared, "chunk": id, "start": pos_json(&range.start)}))
}

fn resolve_target_chunk(s: &Session, v: &Value) -> Result<ChunkRef, CmdError> {
    match optional_chunk_id(v)? {
        Some(id) => find_chunk(s, id),
        None => chunks::chunk_at(&s.doc, &s.sel.focus).ok_or_else(|| CmdError::Disabled("the cursor is not inside a code chunk".into())),
    }
}

fn resolve_optional_chunk(s: &Session, v: &Value) -> Result<Option<ChunkRef>, CmdError> {
    match optional_chunk_id(v)? {
        Some(id) => find_chunk(s, id).map(Some),
        None => Ok(chunks::chunk_at(&s.doc, &s.sel.focus)),
    }
}

fn validate_code_text(code: &str) -> Result<(), CmdError> {
    if code.len() > chunks::MAX_CHUNK_BYTES {
        return Err(CmdError::Params(format!("code: at most {} bytes", chunks::MAX_CHUNK_BYTES)));
    }
    let line_count = code.bytes().filter(|b| *b == b'\n').count() + 1;
    if line_count > MAX_CHUNK_LINES {
        return Err(CmdError::Params(format!("code: at most {MAX_CHUNK_LINES} lines")));
    }
    if code.contains(OBJ) {
        return Err(CmdError::Params("code: cannot contain U+FFFC".into()));
    }
    Ok(())
}

fn ensure_top_level_body_chunk(range: &ChunkRef) -> Result<(usize, usize), CmdError> {
    if range.start.story != StoryRef::Body || range.end.story != StoryRef::Body || range.start.path.0.len() != 1 || range.end.path.0.len() != 1 {
        return Err(CmdError::Disabled("code chunks must be top-level blocks in the document body".into()));
    }
    let first = range.start.path.last();
    let last = range.end.path.last();
    if last < first {
        return Err(CmdError::Failed("chunk block range is out of bounds".into()));
    }
    Ok((first, last))
}

fn remap_pos_after_chunk_resize(
    pos: &mut Pos,
    story: StoryRef,
    parent: &[u32],
    old_span: (usize, usize),
    new_last: usize,
    doc: &wordcraft_doc::Document,
) {
    if pos.story != story || !pos.path.0.starts_with(parent) {
        return;
    }
    let (old_first, old_last) = old_span;
    let depth = parent.len();
    let Some(block) = pos.path.0.get(depth).copied().map(|b| b as usize) else { return };
    if block < old_first {
        return;
    }
    if block <= old_last {
        if pos.path.0.len() == depth + 1 {
            let clamped_block = block.min(new_last);
            if clamped_block != block {
                pos.path.0[depth] = clamped_block as u32;
            }
            let off = pos.off;
            pos.off = doc.para(pos.story, &pos.path).map_or(0, |p| p.clamp(off));
        }
    } else {
        let delta = new_last as isize - old_last as isize;
        let shifted = (block as isize + delta).max(0) as usize;
        pos.path.0[depth] = shifted as u32;
    }
}

fn update_chunk(s: &mut Session, v: &Value) -> CmdResult {
    let range = resolve_target_chunk(s, v)?;
    let code = match v.get("code") {
        None | Some(Value::Null) => range.chunk.code.as_str(),
        Some(Value::String(text)) => {
            validate_code_text(text)?;
            text.as_str()
        }
        Some(_) => return Err(CmdError::Params("code: expected a string".into())),
    };
    let lang_str = match v.get("language") {
        None | Some(Value::Null) => range.chunk.language.clone(),
        Some(Value::String(name)) => {
            if name.len() > 16 {
                return Err(CmdError::Params("language: expected \"r\" or \"python\"".into()));
            }
            CodeLanguage::from_str(name).map_err(|e| CmdError::Params(e.to_string()))?.as_str().to_owned()
        }
        Some(_) => return Err(CmdError::Params("language: expected \"r\" or \"python\"".into())),
    };
    let label_update: Option<Option<&str>> = match v.get("label") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(text)) => Some(Some(text.as_str())),
        Some(_) => return Err(CmdError::Params("label: expected a string or null".into())),
    };

    let id = chunk_id(s, &range);
    let old_span = (range.start.path.last(), range.end.path.last());
    let story = range.start.story;
    let parent = range.start.path.parent().to_vec();
    let mut changed = s.doc.update_chunk(id, &lang_str, code)?;
    if let Some(new_label) = label_update {
        changed |= s.doc.set_chunk_label(id, new_label)?;
    }
    let updated = find_chunk(s, id)?;
    let new_last = updated.end.path.last();

    if changed {
        remap_pos_after_chunk_resize(&mut s.sel.anchor, story, &parent, old_span, new_last, &s.doc);
        remap_pos_after_chunk_resize(&mut s.sel.focus, story, &parent, old_span, new_last, &s.doc);
        s.goal_x = None;
        s.touch();
    }

    let mut out = sel_result(s)?;
    out["changed"] = json!(changed);
    out["chunk"] = json!({
        "id": id,
        "language": updated.chunk.language,
        "label": updated.chunk.options.label(),
        "tag": updated.chunk.tag(),
        "lines": updated.chunk.code.split('\n').count(),
    });
    Ok(out)
}

fn delete_chunk(s: &mut Session, v: &Value) -> CmdResult {
    let range = resolve_target_chunk(s, v)?;
    let (first, last) = ensure_top_level_body_chunk(&range)?;
    if last >= s.doc.body.len() {
        return Err(CmdError::Failed("chunk block range is out of bounds".into()));
    }
    let span = last - first + 1;
    let trailing_section = s.doc.body.get(last).and_then(|b| b.as_para()).and_then(|p| p.section.clone());

    if s.doc.body.len() <= span {
        let mut blank = Paragraph::new();
        blank.section = trailing_section;
        s.doc.body = vec![para_block(blank)];
    } else {
        let mut kept_break = trailing_section;
        if let Some(sect) = kept_break.take() {
            if first > 0
                && let Ok(prev) = s.doc.para_mut(StoryRef::Body, &Path::top(first - 1))
                && prev.section.is_none()
            {
                prev.section = Some(sect);
            } else {
                kept_break = Some(sect);
            }
        }
        for _ in 0..span {
            let _ = s.doc.remove_block(StoryRef::Body, &Path::top(first));
        }
        if let Some(sect) = kept_break {
            let mut blank = Paragraph::new();
            blank.section = Some(sect);
            let _ = s.doc.insert_block(StoryRef::Body, &Path::top(first), Block::Para(blank));
        }
    }

    if let Some(id) = range.chunk.id {
        s.doc.remove_custom_prop(&wordcraft_doc::chunk::chunk_prop_name(id));
        s.chunks.clear(id);
    }

    let next_idx = first.min(s.doc.body.len().saturating_sub(1));
    s.sel = Selection::caret(first_para_pos_at_block(&s.doc, next_idx));
    s.goal_x = None;
    s.touch();

    let mut out = sel_result(s)?;
    out["deleted"] = json!(true);
    out["chunkId"] = json!(range.chunk.id);
    Ok(out)
}

fn first_para_pos_at_block(doc: &wordcraft_doc::Document, idx: usize) -> Pos {
    let mut caret_path = Path::top(idx);
    while matches!(doc.block(StoryRef::Body, &caret_path), Some(Block::Table(_))) {
        caret_path.0.extend([0, 0, 0]);
    }
    if doc.para(StoryRef::Body, &caret_path).is_some() {
        Pos::new(StoryRef::Body, caret_path, 0)
    } else {
        doc.clamp(&Pos::new(StoryRef::Body, Path::top(idx), 0))
    }
}

const MAX_ARCHIVED_SVG_BYTES: usize = 16 * 1024 * 1024;
const MAX_INSERT_TABLE_CELLS: usize = 250_000;
const MAX_INSERT_TABLE_TEXT_BYTES: usize = 8 * 1024 * 1024;

fn move_caret_below_chunk(s: &mut Session, range: &ChunkRef) -> Result<(), CmdError> {
    let after = range.end.path.last().saturating_add(1);
    let path = Path::top(after);
    s.doc.insert_block(StoryRef::Body, &path, Block::Para(Paragraph::new()))?;
    s.sel = Selection::caret(Pos::new(StoryRef::Body, path, 0));
    s.goal_x = None;
    Ok(())
}

fn place_at_caret(s: &Session, v: &Value) -> bool {
    p::bool(v, "atCaret").unwrap_or(false) && chunks::chunk_at(&s.doc, &s.sel.focus).is_none()
}

fn resolve_plot_png(
    s: &Session,
    target_chunk: Option<&ChunkRef>,
    plot_index: usize,
    explicit_png: Option<&str>,
) -> Result<(Vec<u8>, bool), CmdError> {
    let bytes = if let Some(b64) = explicit_png {
        (insert::base64_decode(b64).ok_or_else(|| CmdError::Params("bad base64 PNG plot data".into()))?, false)
    } else if let Some(range) = target_chunk
        && let Some(id) = range.chunk.id
        && let Some(output) = s.chunks.output(id)
    {
        if !s.chunks.output_matches(id, &range.chunk.language, &range.chunk.code) {
            return Err(CmdError::Failed("the code chunk changed since its last run; run the chunk again before inserting its plot".into()));
        }
        let png = output
            .plots
            .get(plot_index)
            .and_then(|p| p.png.clone())
            .or_else(|| if plot_index == 0 { output.plot_png.clone() } else { None })
            .ok_or_else(|| CmdError::Params("insert_plot requires PNG base64 data or a chunk with a cached PNG plot".into()))?;
        (png, true)
    } else {
        return Err(CmdError::Params("insert_plot requires PNG base64 data or a chunk with a cached PNG plot".into()));
    };

    if bytes.0.is_empty() {
        return Err(CmdError::Params("plot PNG payload is empty".into()));
    }
    if bytes.0.len() > (200 << 20) {
        return Err(CmdError::Failed("image is larger than 200 MB".into()));
    }
    // Validate image decodability before mutating the document.
    match wordcraft_geom::picture::probe(&bytes.0) {
        Ok(pic) if pic.format == image::ImageFormat::Png => Ok(bytes),
        Ok(_) => Err(CmdError::Params("plot image data must be a PNG".into())),
        Err(wordcraft_geom::picture::Refusal::Unsupported(why)) => Err(CmdError::Failed(why.into())),
        Err(_) => Err(CmdError::Params("plot PNG bytes could not be decoded as a valid image".into())),
    }
}

fn resolve_plot_svg<'a>(
    s: &'a Session,
    v: &'a Value,
    target_chunk: Option<&ChunkRef>,
    plot_index: usize,
    cached_matches: bool,
) -> Result<Option<Vec<u8>>, CmdError> {
    let svg_opt: Option<&str> = match v.get("svg") {
        None | Some(Value::Null) if cached_matches => target_chunk
            .and_then(|r| r.chunk.id)
            .and_then(|id| s.chunks.output(id))
            .and_then(|o| o.plots.get(plot_index).and_then(|p| p.svg.as_deref()).or(if plot_index == 0 { o.plot_svg.as_deref() } else { None })),
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.as_str()),
        Some(_) => return Err(CmdError::Params("svg: expected a string".into())),
    };
    if let Some(svg) = svg_opt
        && svg.len() > MAX_ARCHIVED_SVG_BYTES
    {
        return Err(CmdError::Params(format!("svg: at most {MAX_ARCHIVED_SVG_BYTES} bytes")));
    }
    Ok(svg_opt.filter(|s| !s.trim().is_empty() && s.contains("</svg>")).map(|s| s.as_bytes().to_vec()))
}

fn insert_plot(s: &mut Session, v: &Value) -> CmdResult {
    let at_caret = place_at_caret(s, v);
    let target_chunk = resolve_optional_chunk(s, v)?;
    if !at_caret && let Some(ref range) = target_chunk {
        let _ = ensure_top_level_body_chunk(range)?;
    }
    let plot_index = p::u64(v, "plotIndex").unwrap_or(0) as usize;
    let explicit_png = p::str(v, "png").or_else(|| p::str(v, "data"));
    let (png_bytes, cached_matches) = resolve_plot_png(s, target_chunk.as_ref(), plot_index, explicit_png)?;
    let archived_svg = resolve_plot_svg(s, v, target_chunk.as_ref(), plot_index, explicit_png.is_none() && cached_matches)?;

    let alt = p::str(v, "alt")
        .map(str::to_owned)
        .or_else(|| target_chunk.as_ref().and_then(|r| r.chunk.options.fig_caption()))
        .unwrap_or_else(|| "Code chunk plot".to_owned());

    if !at_caret && let Some(ref range) = target_chunk {
        move_caret_below_chunk(s, range)?;
    }

    let width = p::f32(v, "width");
    let out = insert::place_picture(s, png_bytes, width, &alt)?;
    if let Some(svg_bytes) = archived_svg {
        let _ = s.doc.add_media(svg_bytes, "svg");
    }
    s.touch();
    Ok(out)
}

fn parse_string_row(arr: &[Value], field: &str, text_budget: &mut usize) -> Result<Vec<String>, CmdError> {
    if arr.len() > MAX_COLS {
        return Err(CmdError::Params(format!("{field}: at most {MAX_COLS} columns")));
    }
    let mut cells = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item.as_str().ok_or_else(|| CmdError::Params(format!("{field}: every cell must be a string")))?;
        *text_budget = text_budget
            .checked_add(s.len())
            .filter(|&n| n <= MAX_INSERT_TABLE_TEXT_BYTES)
            .ok_or_else(|| CmdError::Params(format!("table text exceeds {MAX_INSERT_TABLE_TEXT_BYTES} bytes")))?;
        cells.push(s.to_owned());
    }
    Ok(cells)
}

fn insert_table(s: &mut Session, v: &Value) -> CmdResult {
    let at_caret = place_at_caret(s, v);
    let target_chunk = resolve_optional_chunk(s, v)?;
    if !at_caret && let Some(ref range) = target_chunk {
        let _ = ensure_top_level_body_chunk(range)?;
    }

    let raw_headers = v.get("headers").and_then(Value::as_array).ok_or_else(|| CmdError::Params("headers: required array of strings".into()))?;
    let mut text_budget = 0usize;
    let headers = parse_string_row(raw_headers, "headers", &mut text_budget)?;
    let raw_rows = match v.get("rows") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(arr)) => arr.as_slice(),
        Some(_) => return Err(CmdError::Params("rows: expected array of rows".into())),
    };
    let total_rows = 1usize.saturating_add(raw_rows.len());
    if total_rows > MAX_ROWS {
        return Err(CmdError::Params(format!("rows: at most {MAX_ROWS} rows")));
    }

    let mut rows = Vec::with_capacity(raw_rows.len());
    let mut cols = headers.len();
    for row_val in raw_rows {
        let row_arr = row_val.as_array().ok_or_else(|| CmdError::Params("rows: each row must be an array of strings".into()))?;
        let parsed = parse_string_row(row_arr, "rows", &mut text_budget)?;
        cols = cols.max(parsed.len());
        rows.push(parsed);
    }

    if cols == 0 || cols > MAX_COLS {
        return Err(CmdError::Params(format!("headers/rows: expected 1..={MAX_COLS} columns")));
    }
    if total_rows.checked_mul(cols).is_none_or(|cells| cells > MAX_INSERT_TABLE_CELLS) {
        return Err(CmdError::Params(format!("table exceeds {MAX_INSERT_TABLE_CELLS} total cells")));
    }

    let width = s.doc.sections().first().map(|(_, sp)| sp.text_width()).unwrap_or(468.0);
    let mut table = Table::new(total_rows, cols, width);
    table.rows[0].props.header = true;
    let bold_props = CharProps { bold: Some(true), ..Default::default() };

    for (c, cell) in table.rows[0].cells.iter_mut().enumerate() {
        let text = headers.get(c).map(String::as_str).unwrap_or("");
        *cell = Cell::with_text("");
        cell.blocks = vec![para_block(Paragraph::with_text(text, bold_props.clone()))];
    }
    for (r, row_data) in rows.iter().enumerate() {
        if let Some(row) = table.rows.get_mut(r + 1) {
            for (c, cell) in row.cells.iter_mut().enumerate() {
                let text = row_data.get(c).map(String::as_str).unwrap_or("");
                *cell = Cell::with_text(text);
            }
        }
    }

    let (story, insert_at) = if !at_caret && let Some(ref range) = target_chunk {
        (StoryRef::Body, Path::top(range.end.path.last().saturating_add(1)))
    } else {
        let at = delete_selection(s)?;
        let path = if s.doc.para_at(&at).is_some_and(|p| p.is_empty()) { at.path } else { at.path.with_last(at.path.last().saturating_add(1)) };
        (at.story, path)
    };

    s.doc.insert_block(story, &insert_at, Block::Table(table))?;
    let next = insert_at.with_last(insert_at.last().saturating_add(1));
    if s.doc.block(story, &next).is_none() {
        s.doc.insert_block(story, &next, Block::Para(Paragraph::new()))?;
    }
    let mut first_cell = insert_at.0.clone();
    first_cell.extend([0, 0, 0]);
    s.sel = Selection::caret(Pos::new(story, Path(first_cell), 0));
    s.goal_x = None;
    s.touch();

    Ok(json!({
        "table": insert_at.0,
        "rows": total_rows,
        "cols": cols,
    }))
}

fn resolve_output_text(s: &Session, v: &Value, target_chunk: Option<&ChunkRef>) -> Result<String, CmdError> {
    match v.get("text") {
        None | Some(Value::Null) => {
            let range = target_chunk.ok_or_else(|| CmdError::Params("insert_output requires text or a chunk with cached console output".into()))?;
            let id = range.chunk.id.ok_or_else(|| CmdError::Params("the target chunk has no id".into()))?;
            if !s.chunks.output_matches(id, &range.chunk.language, &range.chunk.code) {
                return Err(CmdError::Failed("the code chunk changed since its last run; run the chunk again before inserting its output".into()));
            }
            let output = s.chunks.output(id).ok_or_else(|| CmdError::Params("the target chunk has no cached output".into()))?;
            let mut combined = output.stdout.trim_end_matches(['\r', '\n']).to_owned();
            let err = output.stderr.trim_end_matches(['\r', '\n']);
            if !err.is_empty() {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str(err);
            }
            Ok(combined)
        }
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(CmdError::Params("text: expected a string".into())),
    }
}

fn insert_output_paragraphs(s: &mut Session, story: StoryRef, start_path: &Path, replace_empty_at: bool, lines: &[&str]) -> Result<Pos, CmdError> {
    s.doc.ensure_code_output_style();
    let last_offset = lines.len().saturating_sub(1);
    let base_idx = start_path.last();
    let mut last_idx = base_idx;
    let mut preserved_section = None;
    for (offset, line) in lines.iter().enumerate() {
        let cur_idx = base_idx.saturating_add(offset);
        last_idx = cur_idx;
        let path = start_path.with_last(cur_idx);
        let mut para = Paragraph::with_text(line, CharProps::default()).styled(CODE_OUTPUT_STYLE);
        if offset == 0
            && replace_empty_at
            && let Ok(existing) = s.doc.para_mut(story, &path)
        {
            // Keep any section break carried by the replaced empty paragraph; for multi-line
            // output it lands on the last inserted paragraph so every output line stays in
            // the original section before the break.
            preserved_section = existing.section.take();
            if offset == last_offset {
                para.section = preserved_section.take();
            }
            *existing = para;
            continue;
        }
        if offset == last_offset {
            para.section = preserved_section.take();
        }
        s.doc.insert_block(story, &path, Block::Para(para))?;
    }

    let next_path = start_path.with_last(last_idx.saturating_add(1));
    if s.doc.block(story, &next_path).is_none() {
        s.doc.insert_block(story, &next_path, Block::Para(Paragraph::new()))?;
        Ok(Pos::new(story, next_path, 0))
    } else {
        let last_path = start_path.with_last(last_idx);
        let end_off = s.doc.para(story, &last_path).map_or(0, |p| p.len());
        Ok(Pos::new(story, last_path, end_off))
    }
}

fn insert_output(s: &mut Session, v: &Value) -> CmdResult {
    let at_caret = place_at_caret(s, v);
    let target_chunk = resolve_optional_chunk(s, v)?;
    if !at_caret && let Some(ref range) = target_chunk {
        let _ = ensure_top_level_body_chunk(range)?;
    }

    let raw_text = resolve_output_text(s, v, target_chunk.as_ref())?;
    let normalized = raw_text.replace("\r\n", "\n");
    let trimmed = normalized.trim_end_matches('\n');
    if trimmed.trim().is_empty() {
        return Err(CmdError::Params("output text is empty".into()));
    }
    validate_code_text(trimmed)?;

    let (story, start_path, replace_empty_at) = if !at_caret && let Some(ref range) = target_chunk {
        (StoryRef::Body, Path::top(range.end.path.last().saturating_add(1)), false)
    } else {
        let at = delete_selection(s)?;
        if s.doc.para_at(&at).is_some_and(|p| p.is_empty()) {
            (at.story, at.path, true)
        } else {
            (at.story, at.path.with_last(at.path.last().saturating_add(1)), false)
        }
    };

    if crate::cmd::protect::enforced(&s.doc) {
        return Err(CmdError::Disabled("the document is protected".into()));
    }

    let lines: Vec<&str> = trimmed.split('\n').collect();
    let end_pos = insert_output_paragraphs(s, story, &start_path, replace_empty_at, &lines)?;
    s.sel = Selection::caret(end_pos);
    s.goal_x = None;
    s.touch();

    Ok(json!({
        "lines": lines.len(),
        "path": start_path.0,
    }))
}

fn exit_below_chunk(s: &mut Session, v: &Value) -> CmdResult {
    let range = resolve_target_chunk(s, v)?;
    exit_below_chunk_range(s, &range, false)?;
    sel_result(s)
}

/// Move the caret after top-level body chunk `range`, creating a trailing normal paragraph
/// when `insert_new_para` is true or when `range` ends the document body.
pub(crate) fn exit_below_chunk_range(s: &mut Session, range: &ChunkRef, insert_new_para: bool) -> Result<(), CmdError> {
    let (_first, last) = ensure_top_level_body_chunk(range)?;
    let next = last.saturating_add(1);
    let path = Path::top(next);
    let pos = if !insert_new_para && s.doc.body.get(next).is_some() {
        first_para_pos_at_block(&s.doc, next)
    } else {
        if crate::cmd::protect::enforced(&s.doc) {
            return Err(CmdError::Disabled("the document is protected".into()));
        }
        let end_path = Path::top(last);
        let mark_revs = s.doc.para(StoryRef::Body, &end_path).map(|p| (p.mark.ins, p.mark.del)).unwrap_or_default();
        s.doc.insert_block(StoryRef::Body, &path, Block::Para(Paragraph::new()))?;
        // A section break belongs to the trailing paragraph mark, so it moves to the new
        // paragraph just like `split_paragraph` (and returns to `end_path` if the break is
        // rejected under Track Changes).
        let section = {
            let p = s.doc.para_mut(StoryRef::Body, &end_path)?;
            let section = p.section.take();
            if section.is_some() {
                p.touch();
            }
            section
        };
        if let Some(section) = section {
            let t = s.doc.para_mut(StoryRef::Body, &path)?;
            t.section = Some(section);
            t.touch();
        }
        if s.doc.settings.track_changes {
            let rid = super::new_revision(s, RevisionKind::Insert);
            let p = s.doc.para_mut(StoryRef::Body, &end_path)?;
            p.mark.ins = Some(rid);
            p.mark.del = None;
            p.touch();
            // The newly inserted paragraph ends with the original trailing mark, keeping that
            // mark's revisions (matching `split_para_as`), while `end_path` carries the inserted
            // paragraph break so rejecting it joins and removes the new paragraph even at story end.
            let t = s.doc.para_mut(StoryRef::Body, &path)?;
            (t.mark.ins, t.mark.del) = mark_revs;
            t.touch();
        }
        s.touch();
        Pos::new(StoryRef::Body, path, 0)
    };
    s.sel = Selection::caret(pos);
    s.goal_x = None;
    Ok(())
}
