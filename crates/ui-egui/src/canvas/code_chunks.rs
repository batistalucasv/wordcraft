//! Interactive mounting, synchronization, and async execution of code chunks on the canvas.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::mpsc::Receiver;

use egui::{Pos2, Rect, Ui, pos2, vec2};
use serde_json::{Value, json};
use wordcraft_doc::StoryRef;
use wordcraft_engine::chunks::{self, ChunkOutput as EngineOutput};
#[cfg(not(target_arch = "wasm32"))]
use wordcraft_kernel::ExecutionRequest;
use wordcraft_kernel::{CodeLanguage, PlotFormat};
use wordcraft_layout::{DocLayout, Placed};

use crate::WordApp;
use crate::canvas::pages::page_screen_scale;
use crate::code_chunk::{ChunkOutput as WidgetOutput, CodeChunk, CodeChunkAction};

const DEBOUNCE_SECS: f64 = 0.5;
const HEIGHT_HYSTERESIS_CENTIPTS: i64 = 75; // 0.75 pt
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
const MAX_CONCURRENT_CHUNK_RUNS: usize = 4;

/// Per-chunk mounted widget and document-sync bookkeeping.
pub(crate) struct CodeChunkEntry {
    pub(crate) widget: CodeChunk,
    last_synced_lang: CodeLanguage,
    last_synced_code: String,
    dirty: bool,
    last_edit_time: Option<f64>,
    measured_centipts: Option<u32>,
    applied_output_rev: Option<u64>,
}

/// In-flight UI background execution tracked to store completion back into `Session::chunks`.
struct PendingRun {
    chunk_id: i64,
    language: CodeLanguage,
    code: String,
    generation: u64,
    rx: Receiver<Result<wordcraft_kernel::ExecutionResult, String>>,
}

/// Queued background run waiting for a concurrency slot in `Run All`.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
struct QueuedRun {
    chunk_id: i64,
    language: CodeLanguage,
    code: String,
    timeout_ms: u64,
    plot: Option<PlotFormat>,
    generation: u64,
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
    queued_runs: VecDeque<QueuedRun>,
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

    fn has_in_flight_run(&self, id: i64) -> bool {
        self.pending_runs.iter().any(|r| r.chunk_id == id)
            || self.queued_runs.iter().any(|r| r.chunk_id == id)
            || self.widgets.get(&id).is_some_and(|e| e.widget.is_running())
    }
}

/// Whether any mounted code chunk widget has unsaved edits not yet flushed into the document.
pub(crate) fn has_dirty_chunks(app: &WordApp) -> bool {
    app.canvas.code_chunks.widgets.values().any(|e| e.dirty)
}

fn top_level_chunks(doc: &wordcraft_doc::Document) -> Vec<wordcraft_doc::ChunkRef> {
    chunks::chunks_in(doc, StoryRef::Body).into_iter().filter(|r| r.start.path.0.len() == 1 && r.end.path.0.len() == 1).collect()
}

fn reconcile_entry(entry: &mut CodeChunkEntry, lang: CodeLanguage, code: &str) -> bool {
    if entry.last_synced_lang == lang && entry.last_synced_code == code {
        return false;
    }
    if entry.widget.language() != lang {
        entry.widget.set_language(lang);
    }
    if entry.widget.code() != code {
        entry.widget.set_code(code);
    }
    entry.last_synced_lang = lang;
    entry.last_synced_code = code.to_owned();
    entry.dirty = false;
    entry.last_edit_time = None;
    true
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
    if app.canvas.code_chunks.pending_runs.is_empty() && app.canvas.code_chunks.queued_runs.is_empty() {
        return;
    }
    let current_gen = app.session.chunks.generation();
    let pending = std::mem::take(&mut app.canvas.code_chunks.pending_runs);
    let mut still_running = Vec::new();
    let mut any_stored = false;
    for run in pending {
        match run.rx.try_recv() {
            Ok(Ok(res)) => {
                if run.generation == current_gen {
                    let engine_out = EngineOutput::from_result(res);
                    app.session.chunks.store_for(run.chunk_id, run.language.as_str(), &run.code, engine_out);
                    if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&run.chunk_id) {
                        entry.applied_output_rev = app.session.chunks.output_with_rev(run.chunk_id).map(|(_, r)| r);
                    }
                    app.canvas.code_chunks.last_outputs_rev = Some(app.session.chunks.rev());
                    any_stored = true;
                }
            }
            Ok(Err(_)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Widget attached to a paired receiver reports the error cleanly in its drawer.
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if run.generation == current_gen {
                    still_running.push(run);
                } else if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&run.chunk_id) {
                    entry.widget.clear_output();
                    entry.applied_output_rev = None;
                }
            }
        }
    }
    app.canvas.code_chunks.pending_runs = still_running;
    pump_queued_runs(app, Some(ctx));
    if !app.canvas.code_chunks.pending_runs.is_empty() {
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    }
    if any_stored {
        ctx.request_repaint();
    }
}

/// Synchronize widget state with `app.session.doc` and `app.session.chunks` before `layout()`.
pub(crate) fn sync_before_layout(app: &mut WordApp, ui: &Ui) {
    let ctx = ui.ctx();
    let now = ui.input(|i| i.time);
    poll_pending_runs(app, ctx);

    let refs = top_level_chunks(&app.session.doc);
    let mut live_ids = HashSet::with_capacity(refs.len());
    for r in &refs {
        let Some(id) = r.chunk.id else { continue };
        let Some(lang) = parse_lang(&r.chunk.language) else { continue };
        live_ids.insert(id);

        if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) {
            // Reconcile external document edits (e.g. Undo/Redo/MCP) before flushing drafts.
            reconcile_entry(entry, lang, &r.chunk.code);
        } else {
            let mut widget = CodeChunk::new(lang, r.chunk.code.clone());
            let applied_output_rev = if let Some((out, out_rev)) = app.session.chunks.output_with_rev(id) {
                widget.set_output(to_widget_output(out));
                Some(out_rev)
            } else {
                None
            };
            app.canvas.code_chunks.widgets.insert(
                id,
                CodeChunkEntry {
                    widget,
                    last_synced_lang: lang,
                    last_synced_code: r.chunk.code.clone(),
                    dirty: false,
                    last_edit_time: None,
                    measured_centipts: None,
                    applied_output_rev,
                },
            );
        }
    }

    app.canvas.code_chunks.widgets.retain(|id, _| live_ids.contains(id));
    app.canvas.code_chunks.queued_runs.retain(|q| live_ids.contains(&q.chunk_id));
    if app.canvas.code_chunks.active_chunk.is_some_and(|id| !live_ids.contains(&id)) {
        app.canvas.code_chunks.active_chunk = None;
    }
    if app.canvas.code_chunks.context_chunk.is_some_and(|id| !live_ids.contains(&id)) {
        app.canvas.code_chunks.context_chunk = None;
    }

    // Flush any editor whose typing debounce expired after external document changes reconciled.
    let mut expired: Vec<i64> = app
        .canvas
        .code_chunks
        .widgets
        .iter()
        .filter(|(_, e)| e.dirty && e.last_edit_time.is_some_and(|t| now - t >= DEBOUNCE_SECS))
        .map(|(id, _)| *id)
        .collect();
    expired.sort_unstable();
    for id in expired {
        if let Err(err) = flush_one(app, id) {
            app.status(err);
        }
    }

    // Apply external output / kernel generation changes (clear_output, restart_kernel, MCP run).
    let rev = app.session.chunks.rev();
    let kernel_gen = app.session.chunks.generation();
    if app.canvas.code_chunks.last_generation != Some(kernel_gen) {
        app.canvas.code_chunks.queued_runs.retain(|q| q.generation == kernel_gen);
        let mut stale_ids = Vec::new();
        app.canvas.code_chunks.pending_runs.retain(|p| {
            let keep = p.generation == kernel_gen;
            if !keep {
                stale_ids.push(p.chunk_id);
            }
            keep
        });
        for id in stale_ids {
            if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) {
                entry.widget.clear_output();
                entry.applied_output_rev = None;
            }
        }
    }
    if app.canvas.code_chunks.last_outputs_rev != Some(rev) || app.canvas.code_chunks.last_generation != Some(kernel_gen) {
        for (&id, entry) in &mut app.canvas.code_chunks.widgets {
            if entry.widget.is_running() {
                continue;
            }
            if let Some((out, out_rev)) = app.session.chunks.output_with_rev(id) {
                if entry.applied_output_rev != Some(out_rev) {
                    entry.widget.set_output(to_widget_output(out));
                    entry.applied_output_rev = Some(out_rev);
                }
            } else if entry.applied_output_rev.is_some() || entry.widget.output().is_some() {
                entry.widget.clear_output();
                entry.applied_output_rev = None;
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
pub(crate) fn flush_one(app: &mut WordApp, id: i64) -> Result<(), String> {
    let Some(entry) = app.canvas.code_chunks.widgets.get(&id) else {
        return Ok(());
    };
    if !entry.dirty {
        return Ok(());
    }
    let live = top_level_chunks(&app.session.doc).into_iter().find(|r| r.chunk.id == Some(id));
    match live {
        None => {
            if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) {
                entry.dirty = false;
                entry.last_edit_time = None;
            }
            return Ok(());
        }
        Some(r) => {
            if let Some(live_lang) = parse_lang(&r.chunk.language)
                && let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id)
                && reconcile_entry(entry, live_lang, &r.chunk.code)
            {
                return Ok(());
            }
        }
    }

    let Some(entry) = app.canvas.code_chunks.widgets.get(&id) else {
        return Ok(());
    };
    let code = entry.widget.code().to_owned();
    let lang = entry.widget.language();
    if entry.last_synced_lang == lang && entry.last_synced_code == code {
        if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) {
            entry.dirty = false;
            entry.last_edit_time = None;
        }
        return Ok(());
    }
    match app.session.run(
        "code.update_chunk",
        &json!({
            "chunkId": id,
            "language": lang.as_str(),
            "code": code,
        }),
    ) {
        Ok(_) => {
            if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) {
                entry.last_synced_lang = lang;
                entry.last_synced_code = code;
                entry.dirty = false;
                entry.last_edit_time = None;
            }
            Ok(())
        }
        Err(e) => {
            // Stop repeating timer flushes every frame, but keep `dirty = true` until resolved.
            if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) {
                entry.last_edit_time = None;
            }
            Err(e.to_string())
        }
    }
}

/// Flush every dirty code chunk editor into the document before Undo/Redo/Save/Export/Run.
pub(crate) fn flush_dirty(app: &mut WordApp) -> Result<(), String> {
    let mut ids: Vec<i64> = app.canvas.code_chunks.widgets.iter().filter(|(_, e)| e.dirty).map(|(id, _)| *id).collect();
    ids.sort_unstable();
    let mut first_err = None;
    for id in ids {
        if let Err(err) = flush_one(app, id)
            && first_err.is_none()
        {
            first_err = Some(err);
        }
    }
    match first_err {
        Some(err) => Err(err),
        None => Ok(()),
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

    let read_mode = app.session.view.read_mode;
    let pending_focus = if read_mode {
        app.canvas.code_chunks.pending_focus = None;
        app.canvas.code_chunks.active_chunk = None;
        None
    } else {
        app.canvas.code_chunks.pending_focus.take()
    };
    let mut blur_flush = Vec::new();
    let mut newly_focused = Vec::new();
    let mut completed_runs: Vec<(i64, CodeLanguage, String)> = Vec::new();
    let mut actions = Vec::new();
    let mut arrow_exits = Vec::new();
    let mut height_changed = false;

    for (id, pi, sr, pscale) in mounted_positions {
        let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) else { continue };
        entry.widget.read_only = read_mode;
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
        if !read_mode && frame.editor_has_focus {
            newly_focused.push((id, pi));
        }
        if frame.editor_lost_focus {
            blur_flush.push(id);
        }
        if let Some((run_lang, run_code)) = frame.completed_run {
            completed_runs.push((id, run_lang, run_code));
        }
        if !read_mode && let Some(action) = frame.action {
            actions.push((id, action));
        }
        if !read_mode && let Some(dir) = frame.arrow_exit {
            arrow_exits.push((id, dir));
        }
        if let Some(pos) = frame.context_click_pos {
            app.canvas.code_chunks.pending_context_click = Some((id, pos));
        }
    }

    for id in blur_flush {
        if let Err(err) = flush_one(app, id) {
            app.status(err);
        }
        if app.canvas.code_chunks.active_chunk == Some(id) {
            app.canvas.code_chunks.active_chunk = None;
        }
    }

    if let Some((id, pi)) = newly_focused.last().copied() {
        app.canvas.code_chunks.active_chunk = Some(id);
        app.session.page_hint = pi;
        if !chunks::chunk_at(&app.session.doc, &app.session.sel.focus).is_some_and(|r| r.chunk.id == Some(id))
            && let Some(r) = top_level_chunks(&app.session.doc).into_iter().find(|r| r.chunk.id == Some(id))
        {
            app.session.sel = wordcraft_engine::Selection::caret(r.start);
        }
    }

    for (id, run_lang, run_code) in completed_runs {
        if let Err(err) = flush_one(app, id) {
            app.status(err);
        }
        if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id)
            && let Some(out) = entry.widget.output()
        {
            let engine_out = to_engine_output(out);
            app.session.chunks.store_for(id, run_lang.as_str(), &run_code, engine_out);
            entry.applied_output_rev = app.session.chunks.output_with_rev(id).map(|(_, r)| r);
            app.canvas.code_chunks.last_outputs_rev = Some(app.session.chunks.rev());
        }
    }

    for (id, action) in actions {
        match flush_one(app, id) {
            Ok(()) => dispatch_chunk_action(app, id, action),
            Err(err) => app.status(err),
        }
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
    let result = match action {
        CodeChunkAction::InsertPlot { png, svg } => {
            if let Some(bytes) = png {
                let b64 = wordcraft_engine::cmd::insert::base64_encode(&bytes);
                app.run("code.insert_plot", json!({ "chunkId": chunk_id, "png": b64, "svg": svg }))
            } else {
                app.run("code.insert_plot", json!({ "chunkId": chunk_id, "svg": svg }))
            }
        }
        CodeChunkAction::InsertTable { headers, rows } => {
            app.run("code.insert_table", json!({ "chunkId": chunk_id, "headers": headers, "rows": rows }))
        }
    };
    if let Err(err) = result {
        app.status(err);
    }
    app.canvas.want_focus = true;
}

/// Move focus smoothly out of chunk `id` (`dir < 0`: above, `dir > 0`: below) into an adjacent chunk or canvas paragraph.
fn exit_chunk_to_canvas(app: &mut WordApp, id: i64, dir: i32) {
    if let Err(err) = flush_one(app, id) {
        app.status(err);
    }
    app.canvas.code_chunks.active_chunk = None;
    let refs = top_level_chunks(&app.session.doc);
    let Some(idx) = refs.iter().position(|r| r.chunk.id == Some(id)) else {
        app.canvas.want_focus = true;
        return;
    };
    let current = &refs[idx];
    let first_block = current.start.path.0.first().copied().unwrap_or(0) as usize;
    let last_block = current.end.path.0.first().copied().unwrap_or(0) as usize;

    if dir < 0 {
        if first_block == 0 {
            app.session.sel = wordcraft_engine::Selection::caret(current.start.clone());
            app.canvas.want_focus = true;
            return;
        }
        let target_block = first_block - 1;
        if idx > 0
            && refs[idx - 1].end.path.0.first().copied().unwrap_or(0) as usize == target_block
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
            && next_ref.start.path.0.first().copied().unwrap_or(0) as usize == target_block
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
        } else {
            app.session.sel = wordcraft_engine::Selection::caret(current.end.clone());
        }
        app.canvas.want_focus = true;
    }
}

/// If a plain Up/Down key on the canvas crosses into a mounted code chunk, enter its editor cleanly.
pub(crate) fn maybe_enter_from_canvas_arrow(app: &mut WordApp, key: egui::Key, modifiers: egui::Modifiers) -> bool {
    if modifiers != egui::Modifiers::NONE || app.session.view.read_mode {
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
    let cur_pos = app.session.sel.focus.clone();
    let Some(cur_block) = cur_pos.path.0.first().map(|&i| i as usize) else {
        return false;
    };
    let layout = app.session.layout();
    let next_block =
        layout.vertical(&cur_pos, app.session.goal_x, dir, app.session.page_hint).and_then(|(p, _)| p.path.0.first().map(|&i| i as usize));
    if next_block == Some(cur_block) {
        return false;
    }
    let refs = top_level_chunks(&app.session.doc);

    if dir > 0 {
        for r in refs {
            let Some(cid) = r.chunk.id else { continue };
            if !app.canvas.code_chunks.is_mounted(cid) {
                continue;
            }
            let first = r.start.path.0.first().copied().unwrap_or(0) as usize;
            let last = r.end.path.0.first().copied().unwrap_or(0) as usize;
            if cur_block < first {
                let jumped_over = next_block.is_some_and(|nb| nb > last);
                let entered_chunk = next_block.is_some_and(|nb| (first..=last).contains(&nb));
                let at_edge_before = next_block.is_none() && cur_block + 1 == first;
                if jumped_over || entered_chunk || at_edge_before {
                    app.canvas.code_chunks.request_focus(cid, false);
                    return true;
                }
            }
        }
    } else {
        for r in refs.into_iter().rev() {
            let Some(cid) = r.chunk.id else { continue };
            if !app.canvas.code_chunks.is_mounted(cid) {
                continue;
            }
            let first = r.start.path.0.first().copied().unwrap_or(0) as usize;
            let last = r.end.path.0.first().copied().unwrap_or(0) as usize;
            if cur_block > last {
                let jumped_over = next_block.is_some_and(|nb| nb < first);
                let entered_chunk = next_block.is_some_and(|nb| (first..=last).contains(&nb));
                let at_edge_after = next_block.is_none() && cur_block == last + 1;
                if jumped_over || entered_chunk || at_edge_after {
                    app.canvas.code_chunks.request_focus(cid, true);
                    return true;
                }
            }
        }
    }
    false
}

fn parse_run_options(params: &Value) -> (u64, Option<PlotFormat>) {
    let timeout_ms =
        params.get("timeoutMs").and_then(Value::as_u64).unwrap_or(chunks::DEFAULT_TIMEOUT_MS).clamp(chunks::MIN_TIMEOUT_MS, chunks::MAX_TIMEOUT_MS);
    let plot = match params.get("plot").and_then(Value::as_str).unwrap_or("both") {
        "png" => Some(PlotFormat::Png),
        "svg" => Some(PlotFormat::Svg),
        "none" => None,
        _ => Some(PlotFormat::Both),
    };
    (timeout_ms, plot)
}

fn validate_chunk_run(lang_str: &str, code: &str) -> Result<CodeLanguage, String> {
    if code.trim().is_empty() {
        return Err("the code chunk is empty".into());
    }
    if code.len() > chunks::MAX_CHUNK_BYTES {
        return Err(format!("the code chunk is longer than {} bytes", chunks::MAX_CHUNK_BYTES));
    }
    parse_lang(lang_str).ok_or_else(|| format!("unsupported language: {lang_str}"))
}

fn pump_queued_runs(app: &mut WordApp, ctx: Option<&egui::Context>) {
    #[cfg(target_arch = "wasm32")]
    let _ = (app, ctx);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let current_gen = app.session.chunks.generation();
        while app.canvas.code_chunks.pending_runs.len() < MAX_CONCURRENT_CHUNK_RUNS {
            let Some(next) = app.canvas.code_chunks.queued_runs.pop_front() else {
                break;
            };
            if next.generation != current_gen {
                continue;
            }
            if let Err(err) = launch_worker(app, next, ctx) {
                app.status(err);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn launch_worker(app: &mut WordApp, queued: QueuedRun, ctx: Option<&egui::Context>) -> Result<(), String> {
    let mut req = ExecutionRequest::new(queued.language, queued.code.clone()).with_timeout(queued.timeout_ms);
    if let Some(fmt) = queued.plot {
        req = req.with_plot_capture(true).with_plot_format(fmt);
    } else {
        req = req.with_plot_capture(false);
    }

    let (tx_ui, rx_ui) = std::sync::mpsc::channel();
    let (tx_store, rx_store) = std::sync::mpsc::channel();
    let ctx_clone = ctx.cloned();
    std::thread::Builder::new()
        .name(format!("wc-chunk-{}", queued.chunk_id))
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

    if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&queued.chunk_id) {
        entry.widget.attach_run(rx_ui);
    }
    app.canvas.code_chunks.pending_runs.push(PendingRun {
        chunk_id: queued.chunk_id,
        language: queued.language,
        code: queued.code,
        generation: queued.generation,
        rx: rx_store,
    });
    if let Some(ctx) = ctx {
        ctx.request_repaint();
    }
    Ok(())
}

/// Non-blocking UI execution for `code.run_chunk` when dispatched interactively via `WordApp::run`.
pub(crate) fn run_chunk_async(app: &mut WordApp, params: &Value, ctx: Option<&egui::Context>) -> Result<Value, String> {
    if wordcraft_engine::file_access::current().is_some() {
        return Err("code execution is disabled under automation filesystem confinement".into());
    }
    let chunk_id = params.get("chunkId").and_then(Value::as_i64).or(app.canvas.code_chunks.active_chunk);
    let range = match chunk_id {
        Some(id) => {
            top_level_chunks(&app.session.doc).into_iter().find(|r| r.chunk.id == Some(id)).ok_or_else(|| format!("no code chunk with id {id}"))?
        }
        None => {
            let r = chunks::chunk_at(&app.session.doc, &app.session.sel.focus).ok_or_else(|| "the cursor is not inside a code chunk".to_owned())?;
            if r.start.path.0.len() != 1 || r.end.path.0.len() != 1 {
                return Err("code chunks inside table cells are not supported".into());
            }
            r
        }
    };
    let Some(id) = range.chunk.id else {
        return app.session.run("code.run_chunk", params).map_err(|e| e.to_string());
    };
    if app.canvas.code_chunks.has_in_flight_run(id) {
        return Err(format!("code chunk {id} is already running"));
    }
    let lang = validate_chunk_run(&range.chunk.language, &range.chunk.code)?;
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (lang, ctx);
        return app.session.run("code.run_chunk", params).map_err(|e| e.to_string());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (timeout_ms, plot) = parse_run_options(params);
        let queued =
            QueuedRun { chunk_id: id, language: lang, code: range.chunk.code.clone(), timeout_ms, plot, generation: app.session.chunks.generation() };
        launch_worker(app, queued, ctx)?;
        Ok(json!({ "started": true, "chunk": { "id": id, "language": range.chunk.language } }))
    }
}

/// Non-blocking UI execution for `code.run_all` when dispatched interactively via `WordApp::run`.
pub(crate) fn run_all_async(app: &mut WordApp, params: &Value, ctx: Option<&egui::Context>) -> Result<Value, String> {
    if wordcraft_engine::file_access::current().is_some() {
        return Err("code execution is disabled under automation filesystem confinement".into());
    }
    let ranges = top_level_chunks(&app.session.doc);
    if ranges.is_empty() {
        return Err("the document has no code chunks".into());
    }
    let (timeout_ms, plot) = parse_run_options(params);
    let generation = app.session.chunks.generation();
    let mut batch = Vec::new();
    let mut seen_ids = HashSet::new();
    for r in ranges {
        if !r.chunk.options.eval() {
            continue;
        }
        let Some(id) = r.chunk.id else { continue };
        if r.chunk.code.trim().is_empty() {
            continue;
        }
        if !seen_ids.insert(id) || app.canvas.code_chunks.has_in_flight_run(id) {
            return Err(format!("code chunk {id} is already running"));
        }
        let lang = validate_chunk_run(&r.chunk.language, &r.chunk.code)?;
        batch.push(QueuedRun { chunk_id: id, language: lang, code: r.chunk.code, timeout_ms, plot, generation });
    }
    if batch.is_empty() {
        return Ok(json!({ "started": 0 }));
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (ctx, batch);
        return app.session.run("code.run_all", params).map_err(|e| e.to_string());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let started = batch.len();
        app.canvas.code_chunks.queued_runs.extend(batch);
        pump_queued_runs(app, ctx);
        Ok(json!({ "started": started }))
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
    let writable = !app.session.view.read_mode;
    let show_code = app.canvas.code_chunks.widgets.get(&id).is_none_or(|e| e.widget.show_code);
    let item = |ui: &mut egui::Ui, label: &str, shortcut: &str, enabled: bool| -> bool {
        ui.add_enabled(enabled, crate::rtl::menu_item(ui, label, shortcut, 0.0)).clicked()
    };

    let run_sc = crate::widgets::shortcut_text(app, "code.run_chunk");
    let run_on = writable && crate::widgets::enabled(app, "code.run_chunk");
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
    if item(ui, crate::tl!("Clear Output"), "", writable && has_output) {
        let _ = app.run("code.clear_output", json!({ "chunkId": id }));
        ui.close();
    }
    if item(ui, crate::tl!("Restart Kernel"), "", writable) {
        let _ = app.run("code.restart_kernel", json!({}));
        ui.close();
    }
    ui.separator();
    if item(ui, crate::tl!("Delete Code Chunk"), "", writable) {
        let _ = app.run("code.delete_chunk", json!({ "chunkId": id }));
        app.canvas.want_focus = true;
        ui.close();
    }
}
