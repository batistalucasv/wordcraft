//! Interactive mounting, synchronization, and async execution of code chunks on the canvas.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc::Receiver;

use egui::{Pos2, Rect, Ui, pos2, vec2};
use serde_json::{Value, json};
use wordcraft_doc::StoryRef;
use wordcraft_engine::chunks::{self, ChunkOutput as EngineOutput};
use wordcraft_kernel::{CodeLanguage, ExecutionRequest, PlotFormat};
use wordcraft_layout::{DocLayout, Placed};

use crate::WordApp;
use crate::canvas::pages::page_screen_scale;
use crate::code_chunk::{ChunkOutput as WidgetOutput, CodeChunk, CodeChunkAction};

const DEBOUNCE_SECS: f64 = 0.5;
const HEIGHT_HYSTERESIS_CENTIPTS: i64 = 75; // 0.75 pt

/// Per-chunk mounted widget and document-sync bookkeeping.
pub(crate) struct CodeChunkEntry {
    pub(crate) widget: CodeChunk,
    last_synced_lang: CodeLanguage,
    last_synced_code: String,
    dirty: bool,
    last_edit_time: Option<f64>,
    measured_centipts: Option<u32>,
}

/// In-flight UI background execution tracked to store completion back into `Session::chunks`.
struct PendingRun {
    chunk_id: i64,
    language: CodeLanguage,
    code: String,
    rx: Receiver<Result<wordcraft_kernel::ExecutionResult, String>>,
}

/// Canvas-owned cache of mounted code chunks, hit boxes, and asynchronous runs.
#[derive(Default)]
pub(crate) struct CodeChunksState {
    pub(crate) widgets: HashMap<i64, CodeChunkEntry>,
    pub(crate) screen_rects: HashMap<i64, Rect>,
    pub(crate) active_chunk: Option<i64>,
    pub(crate) context_chunk: Option<i64>,
    pub(crate) pending_focus: Option<(i64, bool)>,
    pending_context_click: Option<(i64, Pos2)>,
    pending_runs: Vec<PendingRun>,
    last_outputs_rev: Option<u64>,
    last_generation: Option<u64>,
}

impl CodeChunksState {
    pub(crate) fn is_mounted(&self, id: i64) -> bool {
        self.widgets.contains_key(&id)
    }

    pub(crate) fn hit(&self, pos: Pos2) -> Option<i64> {
        self.screen_rects.iter().find(|(_, r)| r.contains(pos)).map(|(id, _)| *id)
    }

    pub(crate) fn take_context_click(&mut self) -> Option<(i64, Pos2)> {
        self.pending_context_click.take()
    }

    pub(crate) fn request_focus(&mut self, id: i64, at_end: bool) {
        self.pending_focus = Some((id, at_end));
    }
}

fn parse_lang(raw: &str) -> Option<CodeLanguage> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "r" => Some(CodeLanguage::R),
        "python" | "py" => Some(CodeLanguage::Python),
        _ => None,
    }
}

fn to_widget_output(o: &EngineOutput) -> WidgetOutput {
    WidgetOutput {
        stdout: o.stdout.clone(),
        stderr: o.stderr.clone(),
        exit_code: o.exit_code,
        success: o.success,
        timed_out: o.timed_out,
        duration_ms: o.duration_ms,
        plot_png: o.plot_png.clone(),
        plot_svg: o.plot_svg.clone(),
        tables: o.tables.clone(),
        error: None,
    }
}

fn to_engine_output(o: &WidgetOutput) -> EngineOutput {
    EngineOutput {
        stdout: o.stdout.clone(),
        stderr: if let Some(err) = &o.error {
            if o.stderr.is_empty() { err.clone() } else { format!("{}\n{err}", o.stderr) }
        } else {
            o.stderr.clone()
        },
        exit_code: o.exit_code,
        success: o.success && o.error.is_none(),
        timed_out: o.timed_out,
        output_truncated: false,
        duration_ms: o.duration_ms,
        plot_png: o.plot_png.clone(),
        plot_svg: o.plot_svg.clone(),
        tables: o.tables.clone(),
    }
}

fn default_height_centipts(w: &CodeChunk) -> u32 {
    let header_h = 34.0;
    let code_h = if w.show_code {
        let lines = w.code().matches('\n').count() + 1;
        (lines as f32) * 16.5 + 16.0
    } else {
        6.0
    };
    let drawer_h = if w.output_open { 96.0 } else { 30.0 };
    (((header_h + code_h + drawer_h).clamp(48.0, 20000.0)) * 100.0).round() as u32
}

/// Poll background engine-initiated runs so completed results land in `Session::chunks` and widgets.
fn poll_pending_runs(app: &mut WordApp, ctx: &egui::Context) {
    if app.canvas.code_chunks.pending_runs.is_empty() {
        return;
    }
    let pending = std::mem::take(&mut app.canvas.code_chunks.pending_runs);
    let mut still_running = Vec::new();
    let mut any_stored = false;
    for run in pending {
        match run.rx.try_recv() {
            Ok(Ok(res)) => {
                let engine_out = EngineOutput::from_result(res);
                app.session.chunks.store_for(run.chunk_id, run.language.as_str(), &run.code, engine_out);
                any_stored = true;
            }
            Ok(Err(_)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Widget attached to a paired receiver reports the error cleanly in its drawer.
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => still_running.push(run),
        }
    }
    if !still_running.is_empty() {
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    }
    app.canvas.code_chunks.pending_runs = still_running;
    if any_stored {
        ctx.request_repaint();
    }
}

/// Synchronize widget state with `app.session.doc` and `app.session.chunks` before `layout()`.
pub(crate) fn sync_before_layout(app: &mut WordApp, ui: &Ui) {
    let ctx = ui.ctx();
    let now = ui.input(|i| i.time);
    poll_pending_runs(app, ctx);

    // Flush any editor whose typing debounce expired.
    let expired: Vec<i64> = app
        .canvas
        .code_chunks
        .widgets
        .iter()
        .filter(|(_, e)| e.dirty && e.last_edit_time.is_some_and(|t| now - t >= DEBOUNCE_SECS))
        .map(|(id, _)| *id)
        .collect();
    for id in expired {
        flush_one(app, id);
    }

    let refs = chunks::chunks_in(&app.session.doc, StoryRef::Body);
    let mut live_ids = HashSet::with_capacity(refs.len());
    for r in &refs {
        let Some(id) = r.chunk.id else { continue };
        let Some(lang) = parse_lang(&r.chunk.language) else { continue };
        live_ids.insert(id);

        if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) {
            // Only overwrite widget edits when the document changed externally (e.g. Undo/Redo/MCP).
            if entry.last_synced_lang != lang || entry.last_synced_code != r.chunk.code {
                if entry.widget.language() != lang {
                    entry.widget.set_language(lang);
                }
                if entry.widget.code() != r.chunk.code {
                    entry.widget.set_code(&r.chunk.code);
                }
                entry.last_synced_lang = lang;
                entry.last_synced_code = r.chunk.code.clone();
                entry.dirty = false;
                entry.last_edit_time = None;
            }
        } else {
            let mut widget = CodeChunk::new(lang, r.chunk.code.clone());
            if let Some(out) = app.session.chunks.output(id) {
                widget.set_output(to_widget_output(out));
            }
            app.canvas.code_chunks.widgets.insert(
                id,
                CodeChunkEntry {
                    widget,
                    last_synced_lang: lang,
                    last_synced_code: r.chunk.code.clone(),
                    dirty: false,
                    last_edit_time: None,
                    measured_centipts: None,
                },
            );
        }
    }

    app.canvas.code_chunks.widgets.retain(|id, _| live_ids.contains(id));
    if app.canvas.code_chunks.active_chunk.is_some_and(|id| !live_ids.contains(&id)) {
        app.canvas.code_chunks.active_chunk = None;
    }
    if app.canvas.code_chunks.context_chunk.is_some_and(|id| !live_ids.contains(&id)) {
        app.canvas.code_chunks.context_chunk = None;
    }

    // Apply external output / kernel generation changes (clear_output, restart_kernel, MCP run).
    let rev = app.session.chunks.rev();
    let kernel_gen = app.session.chunks.generation();
    if app.canvas.code_chunks.last_outputs_rev != Some(rev) || app.canvas.code_chunks.last_generation != Some(kernel_gen) {
        for (&id, entry) in &mut app.canvas.code_chunks.widgets {
            if entry.widget.is_running() {
                continue;
            }
            if let Some(out) = app.session.chunks.output(id) {
                entry.widget.set_output(to_widget_output(out));
            } else {
                entry.widget.clear_output();
            }
        }
        app.canvas.code_chunks.last_outputs_rev = Some(rev);
        app.canvas.code_chunks.last_generation = Some(kernel_gen);
    }

    let mut heights = BTreeMap::new();
    for (&id, entry) in &app.canvas.code_chunks.widgets {
        let h = entry.measured_centipts.unwrap_or_else(|| default_height_centipts(&entry.widget));
        heights.insert(id, h);
    }
    app.session.set_chunk_heights(heights);
}

/// Flush one dirty chunk widget into the document as a clean `code.update_chunk` transaction.
pub(crate) fn flush_one(app: &mut WordApp, id: i64) {
    let Some(entry) = app.canvas.code_chunks.widgets.get(&id) else { return };
    if !entry.dirty {
        return;
    }
    let code = entry.widget.code().to_owned();
    let lang = entry.widget.language();
    let unchanged_since_sync = entry.last_synced_lang == lang && entry.last_synced_code == code;
    if unchanged_since_sync {
        if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) {
            entry.dirty = false;
            entry.last_edit_time = None;
        }
        return;
    }
    let res = app.session.run(
        "code.update_chunk",
        &json!({
            "chunkId": id,
            "language": lang.as_str(),
            "code": code,
        }),
    );
    if res.is_ok()
        && let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id)
    {
        entry.last_synced_lang = lang;
        entry.last_synced_code = code;
        entry.dirty = false;
        entry.last_edit_time = None;
    }
}

/// Flush every dirty code chunk editor into the document before Undo/Redo/Save/Export/Run.
pub(crate) fn flush_dirty(app: &mut WordApp) {
    let ids: Vec<i64> = app.canvas.code_chunks.widgets.iter().filter(|(_, e)| e.dirty).map(|(id, _)| *id).collect();
    for id in ids {
        flush_one(app, id);
    }
}

/// Draw mounted `Placed::Chunk` widgets inside the scroll viewport and handle all interactions.
pub(crate) fn paint_and_interact(app: &mut WordApp, ui: &mut Ui, layout: &DocLayout, rects: &[Rect], scale: f32) {
    app.canvas.code_chunks.screen_rects.clear();
    let now = ui.input(|i| i.time);
    let view_clip = ui.clip_rect().expand(120.0);

    let mut mounted_positions: Vec<(i64, usize, Rect, f32)> = Vec::new();
    for (pi, page) in layout.pages.iter().enumerate() {
        let Some(pr) = rects.get(pi) else { continue };
        let pscale = page_screen_scale(*pr, page, scale);
        for it in &page.items {
            if let Placed::Chunk { id, rect, .. } = it {
                let sr = Rect::from_min_size(pos2(pr.min.x + rect.x * pscale, pr.min.y + rect.y * pscale), vec2(rect.w * pscale, rect.h * pscale));
                app.canvas.code_chunks.screen_rects.insert(*id, sr);
                if sr.intersects(view_clip) || app.canvas.code_chunks.pending_focus.is_some_and(|(fid, _)| fid == *id) {
                    mounted_positions.push((*id, pi, sr, pscale));
                }
            }
        }
    }

    let pending_focus = app.canvas.code_chunks.pending_focus.take();
    let mut blur_flush = Vec::new();
    let mut newly_focused = Vec::new();
    let mut completed_runs = Vec::new();
    let mut actions = Vec::new();
    let mut arrow_exits = Vec::new();
    let mut height_changed = false;

    for (id, pi, sr, pscale) in mounted_positions {
        let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) else { continue };
        if let Some((fid, at_end)) = pending_focus
            && fid == id
        {
            entry.widget.focus_edge(at_end);
            app.session.page_hint = pi;
        }

        // Measure intrinsic height cleanly without capping height to last frame's box.
        let max_rect = Rect::from_min_size(sr.min, vec2(sr.width().max(48.0), f32::INFINITY));
        let mut child_ui = ui.new_child(egui::UiBuilder::new().max_rect(max_rect));
        let frame = entry.widget.show_embedded(&mut child_ui);

        let measured_pt = (child_ui.min_rect().height() / pscale.max(0.01)).clamp(24.0, 20000.0);
        let new_centipts = (measured_pt * 100.0).round() as u32;
        if entry.measured_centipts.is_none_or(|old| (old as i64 - new_centipts as i64).abs() > HEIGHT_HYSTERESIS_CENTIPTS) {
            entry.measured_centipts = Some(new_centipts);
            height_changed = true;
        }

        if frame.dirty {
            entry.dirty = true;
            entry.last_edit_time = Some(now);
        }
        if frame.editor_has_focus {
            newly_focused.push((id, pi));
        }
        if frame.editor_lost_focus {
            blur_flush.push(id);
        }
        if frame.run_completed {
            completed_runs.push(id);
        }
        if let Some(action) = frame.action {
            actions.push((id, action));
        }
        if let Some(dir) = frame.arrow_exit {
            arrow_exits.push((id, dir));
        }
        if let Some(pos) = frame.context_click_pos {
            app.canvas.code_chunks.pending_context_click = Some((id, pos));
        }
    }

    for id in blur_flush {
        flush_one(app, id);
        if app.canvas.code_chunks.active_chunk == Some(id) {
            app.canvas.code_chunks.active_chunk = None;
        }
    }

    if let Some((id, pi)) = newly_focused.last().copied() {
        app.canvas.code_chunks.active_chunk = Some(id);
        app.session.page_hint = pi;
        if !chunks::chunk_at(&app.session.doc, &app.session.sel.focus).is_some_and(|r| r.chunk.id == Some(id))
            && let Some(r) = chunks::chunks_in(&app.session.doc, StoryRef::Body).into_iter().find(|r| r.chunk.id == Some(id))
        {
            app.session.sel = wordcraft_engine::Selection::caret(r.start);
        }
    }

    for id in completed_runs {
        flush_one(app, id);
        if let Some(entry) = app.canvas.code_chunks.widgets.get(&id)
            && let Some(out) = entry.widget.output()
        {
            let engine_out = to_engine_output(out);
            app.session.chunks.store_for(id, entry.widget.language().as_str(), entry.widget.code(), engine_out);
            app.canvas.code_chunks.last_outputs_rev = Some(app.session.chunks.rev());
        }
    }

    for (id, action) in actions {
        flush_one(app, id);
        dispatch_chunk_action(app, id, action);
    }

    for (id, dir) in arrow_exits {
        exit_chunk_to_canvas(app, id, dir);
    }

    if height_changed {
        let mut heights = BTreeMap::new();
        for (&id, entry) in &app.canvas.code_chunks.widgets {
            heights.insert(id, entry.measured_centipts.unwrap_or_else(|| default_height_centipts(&entry.widget)));
        }
        if app.session.set_chunk_heights(heights) {
            ui.ctx().request_repaint();
        }
    }
}

fn dispatch_chunk_action(app: &mut WordApp, chunk_id: i64, action: CodeChunkAction) {
    match action {
        CodeChunkAction::InsertPlot { png, svg } => {
            if let Some(bytes) = png {
                let b64 = wordcraft_engine::cmd::insert::base64_encode(&bytes);
                let _ = app.run("code.insert_plot", json!({ "chunkId": chunk_id, "png": b64, "svg": svg }));
            } else {
                let _ = app.run("code.insert_plot", json!({ "chunkId": chunk_id, "svg": svg }));
            }
        }
        CodeChunkAction::InsertTable { headers, rows } => {
            let _ = app.run("code.insert_table", json!({ "chunkId": chunk_id, "headers": headers, "rows": rows }));
        }
    }
    app.canvas.want_focus = true;
}

/// Move focus smoothly out of chunk `id` (`dir < 0`: above, `dir > 0`: below) into an adjacent chunk or canvas paragraph.
fn exit_chunk_to_canvas(app: &mut WordApp, id: i64, dir: i32) {
    flush_one(app, id);
    app.canvas.code_chunks.active_chunk = None;
    let refs = chunks::chunks_in(&app.session.doc, StoryRef::Body);
    let Some(idx) = refs.iter().position(|r| r.chunk.id == Some(id)) else {
        app.canvas.want_focus = true;
        return;
    };
    let current = &refs[idx];
    let first_block = current.start.path.last();
    let last_block = current.end.path.last();

    if dir < 0 {
        if first_block == 0 {
            app.session.sel = wordcraft_engine::Selection::caret(current.start.clone());
            return;
        }
        let target_block = first_block - 1;
        if idx > 0
            && refs[idx - 1].end.path.last() == target_block
            && let Some(prev_id) = refs[idx - 1].chunk.id
            && app.canvas.code_chunks.is_mounted(prev_id)
        {
            app.canvas.code_chunks.request_focus(prev_id, true);
            return;
        }
        if let Some(pos) = app.session.doc.body.get(target_block).map(|b| match &**b {
            wordcraft_doc::Block::Para(p) => wordcraft_doc::Pos::body(target_block, p.len()),
            wordcraft_doc::Block::Table(_) => wordcraft_doc::Pos::body(target_block, 0),
        }) {
            app.session.sel = wordcraft_engine::Selection::caret(pos);
            app.canvas.want_focus = true;
        }
    } else {
        let target_block = last_block + 1;
        if let Some(next_ref) = refs.get(idx + 1)
            && next_ref.start.path.last() == target_block
            && let Some(next_id) = next_ref.chunk.id
            && app.canvas.code_chunks.is_mounted(next_id)
        {
            app.canvas.code_chunks.request_focus(next_id, false);
            return;
        }
        if let Some(pos) = app.session.doc.body.get(target_block).map(|b| match &**b {
            wordcraft_doc::Block::Para(_) => wordcraft_doc::Pos::body(target_block, 0),
            wordcraft_doc::Block::Table(_) => wordcraft_doc::Pos::new(StoryRef::Body, wordcraft_doc::Path(vec![target_block as u32, 0, 0, 0]), 0),
        }) {
            app.session.sel = wordcraft_engine::Selection::caret(pos);
            app.canvas.want_focus = true;
        }
    }
}

/// If a plain Up/Down key on the canvas crosses into a mounted code chunk, enter its editor cleanly.
pub(crate) fn maybe_enter_from_canvas_arrow(app: &mut WordApp, key: egui::Key, modifiers: egui::Modifiers) -> bool {
    if modifiers != egui::Modifiers::NONE {
        return false;
    }
    let dir = match key {
        egui::Key::ArrowUp => -1,
        egui::Key::ArrowDown => 1,
        _ => return false,
    };
    if app.session.math.is_some() || !app.session.sel.is_collapsed() || app.session.sel.focus.story != StoryRef::Body {
        return false;
    }
    if app.session.sel.focus.path.0.len() != 1 {
        return false;
    }
    let cur_pos = app.session.sel.focus.clone();
    let cur_block = cur_pos.path.last();
    let layout = app.session.layout();
    let next_block = layout.vertical(&cur_pos, app.session.goal_x, dir, app.session.page_hint).map(|(p, _)| p.path.last());
    let refs = chunks::chunks_in(&app.session.doc, StoryRef::Body);

    for r in refs {
        let Some(cid) = r.chunk.id else { continue };
        if !app.canvas.code_chunks.is_mounted(cid) {
            continue;
        }
        let first = r.start.path.last();
        let last = r.end.path.last();
        if dir > 0 && cur_block < first {
            let jumped_over = next_block.is_some_and(|nb| nb > last);
            let at_edge_before = next_block.is_none() && cur_block + 1 == first;
            if jumped_over || at_edge_before {
                app.canvas.code_chunks.request_focus(cid, false);
                return true;
            }
        } else if dir < 0 && cur_block > last {
            let jumped_over = next_block.is_some_and(|nb| nb < first);
            let at_edge_after = next_block.is_none() && cur_block == last + 1;
            if jumped_over || at_edge_after {
                app.canvas.code_chunks.request_focus(cid, true);
                return true;
            }
        }
    }
    false
}

/// Non-blocking UI execution for `code.run_chunk` when dispatched interactively via `WordApp::run`.
pub(crate) fn run_chunk_async(app: &mut WordApp, params: &Value, ctx: Option<&egui::Context>) -> Result<Value, String> {
    if wordcraft_engine::file_access::current().is_some() {
        return Err("code execution is disabled under automation filesystem confinement".into());
    }
    let chunk_id = params.get("chunkId").and_then(Value::as_i64).or(app.canvas.code_chunks.active_chunk);
    let range = match chunk_id {
        Some(id) => chunks::chunks_in(&app.session.doc, StoryRef::Body)
            .into_iter()
            .find(|r| r.chunk.id == Some(id))
            .ok_or_else(|| format!("no code chunk with id {id}"))?,
        None => chunks::chunk_at(&app.session.doc, &app.session.sel.focus).ok_or_else(|| "the cursor is not inside a code chunk".to_owned())?,
    };
    let Some(id) = range.chunk.id else {
        return app.session.run("code.run_chunk", params).map_err(|e| e.to_string());
    };
    if !spawn_chunk_run(app, id, &range.chunk.language, &range.chunk.code, params, ctx)? {
        return app.session.run("code.run_chunk", params).map_err(|e| e.to_string());
    }
    Ok(json!({ "started": true, "chunk": { "id": id, "language": range.chunk.language } }))
}

/// Non-blocking UI execution for `code.run_all` when dispatched interactively via `WordApp::run`.
pub(crate) fn run_all_async(app: &mut WordApp, params: &Value, ctx: Option<&egui::Context>) -> Result<Value, String> {
    if wordcraft_engine::file_access::current().is_some() {
        return Err("code execution is disabled under automation filesystem confinement".into());
    }
    let ranges = chunks::chunks_in(&app.session.doc, StoryRef::Body);
    if ranges.is_empty() {
        return Err("the document has no code chunks".into());
    }
    let mut started = 0usize;
    for r in ranges {
        if !r.chunk.options.eval() {
            continue;
        }
        let Some(id) = r.chunk.id else { continue };
        if r.chunk.code.trim().is_empty() {
            continue;
        }
        if spawn_chunk_run(app, id, &r.chunk.language, &r.chunk.code, params, ctx)? {
            started += 1;
        }
    }
    Ok(json!({ "started": started }))
}

fn spawn_chunk_run(app: &mut WordApp, id: i64, lang_str: &str, code: &str, params: &Value, ctx: Option<&egui::Context>) -> Result<bool, String> {
    if code.trim().is_empty() {
        return Err("the code chunk is empty".into());
    }
    if code.len() > chunks::MAX_CHUNK_BYTES {
        return Err(format!("the code chunk is longer than {} bytes", chunks::MAX_CHUNK_BYTES));
    }
    let Some(lang) = parse_lang(lang_str) else {
        return Err(format!("unsupported language: {lang_str}"));
    };

    #[cfg(target_arch = "wasm32")]
    {
        let _ = (app, id, lang, params, ctx);
        return Ok(false);
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        let timeout_ms = params
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(chunks::DEFAULT_TIMEOUT_MS)
            .clamp(chunks::MIN_TIMEOUT_MS, chunks::MAX_TIMEOUT_MS);
        let plot = match params.get("plot").and_then(Value::as_str).unwrap_or("both") {
            "png" => Some(PlotFormat::Png),
            "svg" => Some(PlotFormat::Svg),
            "none" => None,
            _ => Some(PlotFormat::Both),
        };
        let mut req = ExecutionRequest::new(lang, code.to_owned()).with_timeout(timeout_ms);
        if let Some(fmt) = plot {
            req = req.with_plot_capture(true).with_plot_format(fmt);
        } else {
            req = req.with_plot_capture(false);
        }

        let (tx_ui, rx_ui) = std::sync::mpsc::channel();
        let (tx_store, rx_store) = std::sync::mpsc::channel();
        let ctx_clone = ctx.cloned();
        std::thread::Builder::new()
            .name(format!("wc-chunk-{id}"))
            .spawn(move || {
                let res = wordcraft_kernel::execute(&req);
                let clone = res.as_ref().map(Clone::clone).map_err(|e| e.to_string());
                let _ = tx_ui.send(res);
                let _ = tx_store.send(clone);
                if let Some(ctx) = ctx_clone {
                    ctx.request_repaint();
                }
            })
            .map_err(|e| format!("failed to spawn code chunk runner: {e}"))?;

        if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) {
            entry.widget.attach_run(rx_ui);
        }
        app.canvas.code_chunks.pending_runs.push(PendingRun { chunk_id: id, language: lang, code: code.to_owned(), rx: rx_store });
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
        Ok(true)
    }
}

/// Whether the document caret currently sits inside a mounted code chunk (so the canvas caret hides).
pub(crate) fn caret_in_mounted_chunk(app: &WordApp) -> bool {
    if app.canvas.code_chunks.active_chunk.is_some() {
        return true;
    }
    let pos = &app.session.sel.focus;
    chunks::chunk_at(&app.session.doc, pos).is_some_and(|r| r.chunk.id.is_some_and(|id| app.canvas.code_chunks.is_mounted(id)))
}

/// Draw the right-click context menu for mounted code chunk `id`.
pub(crate) fn chunk_context_menu(app: &mut WordApp, ui: &mut egui::Ui, id: i64) {
    let show_code = app.canvas.code_chunks.widgets.get(&id).is_none_or(|e| e.widget.show_code);
    let item = |ui: &mut egui::Ui, label: &str, shortcut: &str, enabled: bool| -> bool {
        ui.add_enabled(enabled, crate::rtl::menu_item(ui, label, shortcut, 0.0)).clicked()
    };

    let run_sc = crate::widgets::shortcut_text(app, "code.run_chunk");
    let run_on = crate::widgets::enabled(app, "code.run_chunk");
    if item(ui, crate::tl!("Run Chunk"), &run_sc, run_on) {
        let _ = app.run("code.run_chunk", json!({ "chunkId": id }));
        ui.close();
    }
    let toggle_label = if show_code { crate::tl!("Hide Code (Teaching Mode)") } else { crate::tl!("Show Code") };
    if item(ui, toggle_label, "", true) {
        if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) {
            entry.widget.show_code = !entry.widget.show_code;
        }
        ui.close();
    }
    let has_output = app.canvas.code_chunks.widgets.get(&id).is_some_and(|e| e.widget.output().is_some()) || app.session.chunks.output(id).is_some();
    if item(ui, crate::tl!("Clear Output"), "", has_output) {
        let _ = app.run("code.clear_output", json!({ "chunkId": id }));
        ui.close();
    }
    if item(ui, crate::tl!("Restart Kernel"), "", true) {
        let _ = app.run("code.restart_kernel", json!({}));
        ui.close();
    }
    ui.separator();
    if item(ui, crate::tl!("Delete Code Chunk"), "", true) {
        let _ = app.run("code.delete_chunk", json!({ "chunkId": id }));
        app.canvas.want_focus = true;
        ui.close();
    }
}
