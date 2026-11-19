//! Insert › Code: code chunks (R and Python) and the commands that run them.
//!
//! The chunk is the document model's ([`wordcraft_doc::chunk`]): a block-level content control
//! tagged `wordcraft:chunk:<language>` whose content is the code and whose Quarto options live in
//! a custom property. Running executes the code with `wordcraft-kernel` in an isolated process,
//! bounded by `timeoutMs`. The command is the same one the ribbon, the shortcuts, the control
//! channel, the CLI and MCP agents call.

use std::str::FromStr;

use serde_json::{Map, Value, json};
use wordcraft_doc::chunk::MAX_CHUNK_LINES;
use wordcraft_doc::para::OBJ;
use wordcraft_doc::{ChunkOptions, ChunkRef, CodeChunk, StoryRef};
use wordcraft_kernel::{CodeLanguage, ExecutionRequest, PlotFormat};

use super::{delete_selection, pos_json, sel_result};
use crate::chunks::{self, ChunkOutput};
use crate::{CmdError, CmdResult, CommandSpec, Selection, Session, p};

const LOC: &str = "Insert › Code";

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
    ]
}

fn has_chunks(s: &Session) -> Option<&'static str> {
    if crate::file_access::current().is_some() {
        return Some("code execution is disabled under automation filesystem confinement");
    }
    if chunks::chunks_in(&s.doc, StoryRef::Body).is_empty() { Some("the document has no code chunks") } else { None }
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
    if chunk.code.len() > chunks::MAX_CHUNK_BYTES {
        return Err(CmdError::Params(format!("code: at most {} bytes", chunks::MAX_CHUNK_BYTES)));
    }
    let line_count = chunk.code.bytes().filter(|b| *b == b'\n').count() + 1;
    if line_count > MAX_CHUNK_LINES {
        return Err(CmdError::Params(format!("code: at most {MAX_CHUNK_LINES} lines")));
    }
    if chunk.code.contains(OBJ) {
        return Err(CmdError::Params("code: cannot contain U+FFFC".into()));
    }
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
    let inserted = s
        .doc
        .chunk_refs(StoryRef::Body)
        .into_iter()
        .find(|r| r.start.path.0.len() == 1 && r.start.path.last() == index)
        .ok_or_else(|| CmdError::Failed("the inserted chunk could not be read back".into()))?;
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

fn plot_option(v: &Value) -> Result<(bool, PlotFormat), CmdError> {
    match p::str(v, "plot").unwrap_or("png") {
        "none" => Ok((false, PlotFormat::Png)),
        "png" => Ok((true, PlotFormat::Png)),
        "svg" => Ok((true, PlotFormat::Svg)),
        "both" => Ok((true, PlotFormat::Both)),
        other => Err(CmdError::Params(format!("plot: expected \"png\", \"svg\", \"both\" or \"none\", not `{other}`"))),
    }
}

fn optional_chunk_id(v: &Value) -> Result<Option<i64>, CmdError> {
    match v.get("chunkId") {
        None | Some(Value::Null) => Ok(None),
        Some(raw) => raw.as_i64().filter(|&id| id > 0).map(Some).ok_or_else(|| CmdError::Params("chunkId: expected a positive integer".into())),
    }
}

fn find_chunk(s: &Session, id: i64) -> Result<ChunkRef, CmdError> {
    chunks::chunks_in(&s.doc, StoryRef::Body)
        .into_iter()
        .find(|r| r.chunk.id == Some(id))
        .ok_or_else(|| CmdError::Failed(format!("no code chunk with id {id}")))
}

/// Run one chunk's code, keep the output and report it. Returns whether it succeeded.
fn execute(s: &mut Session, id: i64, language: CodeLanguage, code: &str, v: &Value) -> Result<(bool, Value), CmdError> {
    if crate::file_access::current().is_some() {
        return Err(CmdError::Disabled("code execution is disabled under automation filesystem confinement".into()));
    }
    if code.trim().is_empty() {
        return Err(CmdError::Failed("the code chunk is empty".into()));
    }
    if code.len() > chunks::MAX_CHUNK_BYTES {
        return Err(CmdError::Failed(format!("the code chunk is longer than {} bytes", chunks::MAX_CHUNK_BYTES)));
    }
    let (capture, plot) = plot_option(v)?;
    let request = ExecutionRequest::new(language, code).with_timeout(timeout_ms(v)).with_plot_capture(capture).with_plot_format(plot);
    let result = s.chunks.run(&request).map_err(|e| CmdError::Failed(e.to_string()))?;
    let success = result.success;
    let output = ChunkOutput::from_result(result);
    let reported = output.json(p::bool(v, "includePlot").unwrap_or(false), p::bool(v, "includeTables").unwrap_or(false));
    s.chunks.store_for(id, language.as_str(), code, output);
    s.status = if success { format!("{language} chunk ran") } else { format!("{language} chunk failed") };
    Ok((success, reported))
}

fn run_chunk(s: &mut Session, v: &Value) -> CmdResult {
    let range = match optional_chunk_id(v)? {
        Some(id) => find_chunk(s, id)?,
        None => chunks::chunk_at(&s.doc, &s.sel.focus).ok_or_else(|| CmdError::Disabled("the cursor is not inside a code chunk".into()))?,
    };
    let _ = plot_option(v)?;
    let language = runnable_language(&range)?;
    let id = chunk_id(s, &range);
    let (success, output) = execute(s, id, language, &range.chunk.code, v)?;
    Ok(json!({
        "chunk": {"id": id, "language": range.chunk.language, "success": success},
        "output": output,
    }))
}

fn run_all(s: &mut Session, v: &Value) -> CmdResult {
    let _ = plot_option(v)?;
    let ranges = chunks::chunks_in(&s.doc, StoryRef::Body);
    let stop = p::bool(v, "stopOnError").unwrap_or(true);
    let mut results = Vec::with_capacity(ranges.len());
    let mut success = true;
    let mut stopped = false;
    for range in ranges {
        if !range.chunk.options.eval() {
            results.push(json!({
                "chunk": {"id": range.chunk.id, "language": range.chunk.language, "skipped": "eval: false"},
            }));
            continue;
        }
        let entry = match runnable_language(&range) {
            Ok(language) => {
                let id = chunk_id(s, &range);
                match execute(s, id, language, &range.chunk.code, v) {
                    Ok((ok, output)) => {
                        success &= ok;
                        json!({"chunk": {"id": id, "language": range.chunk.language, "success": ok}, "output": output})
                    }
                    Err(e) => {
                        success = false;
                        json!({"chunk": {"id": id, "language": range.chunk.language, "success": false}, "error": e.to_string()})
                    }
                }
            }
            Err(e) => {
                success = false;
                json!({"chunk": {"id": range.chunk.id, "language": range.chunk.language, "success": false}, "error": e.to_string()})
            }
        };
        results.push(entry);
        if !success && stop {
            stopped = true;
            break;
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
        if let Some(range) = chunks::chunks_in(&s.doc, StoryRef::Body).into_iter().find(|r| r.chunk.id == Some(id)) {
            out["start"] = pos_json(&range.start);
        }
        return Ok(out);
    }
    let Some(range) = chunks::chunk_at(&s.doc, &s.sel.focus) else { return Ok(json!({"cleared": 0})) };
    let Some(id) = range.chunk.id else { return Ok(json!({"cleared": 0})) };
    let cleared = usize::from(s.chunks.clear(id));
    Ok(json!({"cleared": cleared, "chunk": id, "start": pos_json(&range.start)}))
}
