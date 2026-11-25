//! Interactive mounting, synchronization, and async execution of code chunks on the canvas.

mod mount;
mod pool;

use std::collections::{BTreeMap, HashMap, VecDeque};

use egui::accesskit::{Live, Role};
use egui::{Pos2, Rect, Ui};
use serde_json::json;
use wordcraft_doc::{ChunkRef, StoryRef};
use wordcraft_engine::chunks::{self, ChunkOutput as EngineOutput, CodeLanguage};

use crate::WordApp;
use crate::code_chunk::{ChunkOutput as WidgetOutput, CodeChunk};

pub(crate) use mount::{
    apply_pending_focus_scroll, chunk_placement, maybe_enter_from_canvas_arrow, paint_and_interact, try_click_below_trailing_chunk,
};
#[cfg(test)]
pub(crate) use mount::{chunk_focus_band_y, dispatch_chunk_action_for_test, exit_chunk_to_canvas_for_test};
pub(crate) use pool::{run_all_async, run_chunk_async};

const DEBOUNCE_SECS: f64 = 0.5;
const HEIGHT_HYSTERESIS_CENTIPTS: i64 = 75; // 0.75 pt
const ANNOUNCEMENT_MS: f64 = 3000.0;

/// Unsaved code-chunk drafts `(language, code)` keyed by chunk control id, parked across document switches.
pub(crate) type ChunkDrafts = HashMap<i64, (CodeLanguage, String)>;

/// Screen-reader announcement emitted when an async chunk run settles.
#[derive(Clone, Debug)]
struct LiveAnnouncement {
    text: String,
    serial: u64,
    at_ms: f64,
}

/// Per-chunk mounted widget and document-sync bookkeeping.
pub(crate) struct CodeChunkEntry {
    pub(crate) widget: CodeChunk,
    pub(crate) last_synced_lang: CodeLanguage,
    pub(crate) last_synced_code: String,
    pub(crate) dirty: bool,
    pub(crate) last_edit_time: Option<f64>,
    measured_centipts: Option<u32>,
    applied_output_rev: Option<u64>,
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
    pending_runs: Vec<pool::PendingRun>,
    queued_runs: VecDeque<pool::QueuedRun>,
    /// Unsaved drafts restored across document switches or window handoffs to re-apply once mounted.
    pub(crate) restored_drafts: ChunkDrafts,
    /// Top-level body chunk refs memoized by `(session.document_id(), session.rev())`.
    cached_chunks: Option<((u64, u64), Vec<ChunkRef>)>,
    live_announcement: Option<LiveAnnouncement>,
    next_announcement_serial: u64,
    last_outputs_rev: Option<u64>,
    last_generation: Option<u64>,
    next_batch_id: u64,
    #[cfg(test)]
    fail_next_launch: Option<String>,
}

impl CodeChunksState {
    pub(crate) fn with_restored_drafts(restored_drafts: ChunkDrafts) -> Self {
        Self { restored_drafts, ..Self::default() }
    }

    pub(crate) fn is_mounted(&self, id: i64) -> bool {
        self.widgets.contains_key(&id)
    }

    pub(crate) fn has_dirty(&self) -> bool {
        self.widgets.values().any(|e| e.dirty) || !self.restored_drafts.is_empty()
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

    pub(crate) fn discard_draft(&mut self, id: i64) {
        self.widgets.remove(&id);
        self.restored_drafts.remove(&id);
        if self.active_chunk == Some(id) {
            self.active_chunk = None;
        }
        if self.pending_focus.is_some_and(|(fid, _)| fid == id) {
            self.pending_focus = None;
        }
    }

    /// Return top-level body chunks, memoized by `(session.document_id(), session.rev())`.
    pub(crate) fn top_level_chunks(&mut self, session: &wordcraft_engine::Session) -> &[ChunkRef] {
        let key = (session.document_id(), session.rev());
        let hit = self.cached_chunks.as_ref().is_some_and(|(k, _)| *k == key);
        if !hit {
            let refs = if session.doc.has_chunks(StoryRef::Body) {
                chunks::chunks_in(&session.doc, StoryRef::Body).into_iter().filter(|r| r.start.path.0.len() == 1 && r.end.path.0.len() == 1).collect()
            } else {
                Vec::new()
            };
            self.cached_chunks = Some((key, refs));
        }
        self.cached_chunks.as_ref().map_or(&[], |(_, refs)| refs.as_slice())
    }

    /// Drop the memoized top-level chunk list (e.g., if `plan_chunk_run` assigned a missing id without touching `rev`).
    pub(crate) fn invalidate_chunk_cache(&mut self) {
        self.cached_chunks = None;
    }

    fn set_announcement(&mut self, text: String, ctx: &egui::Context) {
        let serial = self.next_announcement_serial;
        self.next_announcement_serial = self.next_announcement_serial.wrapping_add(1);
        let at_ms = ctx.input(|i| i.time) * 1000.0;
        self.live_announcement = Some(LiveAnnouncement { text, serial, at_ms });
    }

    fn expose_announcement(&self, ctx: &egui::Context) {
        let Some(announcement) = &self.live_announcement else {
            return;
        };
        let age = ctx.input(|i| i.time) * 1000.0 - announcement.at_ms;
        if !(0.0..ANNOUNCEMENT_MS).contains(&age) {
            return;
        }
        ctx.accesskit_node_builder(egui::Id::unique(("code-chunk-announcement", announcement.serial)), |node| {
            node.set_role(Role::Label);
            node.set_live(Live::Polite);
            node.set_value(announcement.text.clone());
        });
        ctx.request_repaint_after(std::time::Duration::from_secs_f64((ANNOUNCEMENT_MS - age) / 1000.0));
    }

    fn allocate_batch_id(&mut self) -> u64 {
        let limit = self.pending_runs.len().saturating_add(self.queued_runs.len());
        for _ in 0..=limit {
            self.next_batch_id = self.next_batch_id.wrapping_add(1).max(1);
            let candidate = self.next_batch_id;
            if !self.pending_runs.iter().any(|r| r.batch_id == candidate) && !self.queued_runs.iter().any(|r| r.batch_id == candidate) {
                return candidate;
            }
        }
        self.next_batch_id = self.next_batch_id.wrapping_add(1).max(1);
        self.next_batch_id
    }

    /// Transfer in-flight pending and queued runs to `target`, enforcing at most one live
    /// (non-invalidated) run per chunk: a live pending run always supersedes any queued run for
    /// the same chunk, and `target`'s existing live run wins over a duplicate from `self`.
    pub(crate) fn transfer_in_flight_runs_to(&mut self, target: &mut CodeChunksState) {
        let mut batch_map: HashMap<u64, u64> = HashMap::new();
        let mut remap = |target: &mut CodeChunksState, batch_id: u64, stop_on_error: bool| -> u64 {
            if batch_id == 0 && !stop_on_error {
                return 0;
            }
            *batch_map.entry(batch_id).or_insert_with(|| target.allocate_batch_id())
        };
        for mut run in self.pending_runs.drain(..) {
            if run.invalidated {
                if matches!(run.rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
                    target.pending_runs.push(run);
                }
            } else if target.pending_runs.iter().any(|r| !r.invalidated && r.planned.chunk_id == run.planned.chunk_id) {
                target.queued_runs.retain(|q| q.planned.chunk_id != run.planned.chunk_id);
                run.invalidated = true;
                if matches!(run.rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
                    target.pending_runs.push(run);
                }
            } else {
                target.queued_runs.retain(|q| q.planned.chunk_id != run.planned.chunk_id);
                run.batch_id = remap(target, run.batch_id, run.stop_on_error);
                target.pending_runs.push(run);
            }
        }
        for mut run in self.queued_runs.drain(..) {
            if !target.queued_runs.iter().any(|r| r.planned.chunk_id == run.planned.chunk_id)
                && !target.pending_runs.iter().any(|r| !r.invalidated && r.planned.chunk_id == run.planned.chunk_id)
            {
                run.batch_id = remap(target, run.batch_id, run.stop_on_error);
                target.queued_runs.push_back(run);
            }
        }
    }

    fn invalidate_stale_runs(&mut self, current_gen: u64) {
        self.queued_runs.retain(|q| q.planned.kernel_gen == current_gen);
        let mut newly_invalidated = Vec::new();
        for run in &mut self.pending_runs {
            if run.planned.kernel_gen != current_gen && !run.invalidated {
                run.invalidated = true;
                newly_invalidated.push(run.planned.chunk_id);
            }
        }
        for id in newly_invalidated {
            let has_current = self.pending_runs.iter().any(|p| !p.invalidated && p.planned.kernel_gen == current_gen && p.planned.chunk_id == id);
            if !has_current && let Some(entry) = self.widgets.get_mut(&id) {
                entry.widget.clear_output();
                entry.applied_output_rev = None;
            }
        }
    }

    fn reap_invalidated_runs(&mut self) {
        self.pending_runs.retain(|run| !run.invalidated || matches!(run.rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
    }

    fn has_in_flight_run(&self, id: i64) -> bool {
        self.pending_runs.iter().any(|r| !r.invalidated && r.planned.chunk_id == id)
            || self.queued_runs.iter().any(|r| r.planned.chunk_id == id)
            || self.widgets.get(&id).is_some_and(|e| e.widget.is_running())
    }

    /// Flush one dirty chunk widget into `session` after reconciling external document edits.
    pub(crate) fn flush_one(&mut self, session: &mut wordcraft_engine::Session, id: i64) -> Result<(), String> {
        if !self.widgets.get(&id).is_some_and(|e| e.dirty) {
            return Ok(());
        }
        let live = self.top_level_chunks(session).iter().find(|r| r.chunk.id == Some(id)).cloned();
        match live {
            None => {
                if let Some(entry) = self.widgets.get_mut(&id) {
                    entry.dirty = false;
                    entry.last_edit_time = None;
                }
                return Ok(());
            }
            Some(r) => {
                if let Some(live_lang) = parse_lang(&r.chunk.language)
                    && let Some(entry) = self.widgets.get_mut(&id)
                    && reconcile_entry(entry, live_lang, &r.chunk.code)
                {
                    return Ok(());
                }
            }
        }

        let Some(entry) = self.widgets.get(&id) else {
            return Ok(());
        };
        let code = entry.widget.code().to_owned();
        let lang = entry.widget.language();
        if entry.last_synced_code.len() > crate::code_chunk::MAX_CODE_CHARS && code.len() < entry.last_synced_code.len() {
            return Ok(());
        }
        if entry.last_synced_lang == lang && entry.last_synced_code == code {
            if let Some(entry) = self.widgets.get_mut(&id) {
                entry.dirty = false;
                entry.last_edit_time = None;
            }
            return Ok(());
        }
        match session.run(
            "code.update_chunk",
            &json!({
                "chunkId": id,
                "language": lang.as_str(),
                "code": code,
            }),
        ) {
            Ok(_) => {
                if let Some(entry) = self.widgets.get_mut(&id) {
                    entry.last_synced_lang = lang;
                    entry.last_synced_code = code;
                    entry.dirty = false;
                    entry.last_edit_time = None;
                }
                Ok(())
            }
            Err(e) => {
                // Stop repeating timer flushes every frame, but keep `dirty = true` until resolved.
                if let Some(entry) = self.widgets.get_mut(&id) {
                    entry.last_edit_time = None;
                }
                Err(e.to_string())
            }
        }
    }

    /// Flush dirty chunk widgets and restored drafts into `session`, skipping an optional target chunk.
    pub(crate) fn flush_dirty_except(&mut self, session: &mut wordcraft_engine::Session, except: Option<i64>) -> Result<(), String> {
        let mut ids: Vec<i64> = self.widgets.iter().filter(|(id, e)| e.dirty && Some(**id) != except).map(|(id, _)| *id).collect();
        ids.sort_unstable();
        let mut first_err = None;
        for id in ids {
            if let Err(err) = self.flush_one(session, id)
                && first_err.is_none()
            {
                first_err = Some(err);
            }
        }
        let mut parked_ids: Vec<i64> = self.restored_drafts.keys().filter(|id| Some(**id) != except).copied().collect();
        parked_ids.sort_unstable();
        for id in parked_ids {
            let Some((lang, code)) = self.restored_drafts.get(&id).cloned() else {
                continue;
            };
            let live = self.top_level_chunks(session).iter().find(|r| r.chunk.id == Some(id)).cloned();
            let Some(r) = live else {
                self.restored_drafts.remove(&id);
                continue;
            };
            if r.chunk.language == lang.as_str() && r.chunk.code == code {
                self.restored_drafts.remove(&id);
                continue;
            }
            match session.run(
                "code.update_chunk",
                &json!({
                    "chunkId": id,
                    "language": lang.as_str(),
                    "code": code,
                }),
            ) {
                Ok(_) => {
                    self.restored_drafts.remove(&id);
                }
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e.to_string());
                    }
                }
            }
        }
        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn cached_chunks_key_for_test(&self) -> Option<(u64, u64)> {
        self.cached_chunks.as_ref().map(|(k, _)| *k)
    }

    #[cfg(test)]
    pub(crate) fn push_pending_for_test(
        &mut self,
        chunk_id: i64,
        language: CodeLanguage,
        code: &str,
        batch: (u64, u64, u64, bool),
        rx: std::sync::mpsc::Receiver<Result<chunks::ExecutionResult, String>>,
    ) {
        let (doc_id, generation, batch_id, stop_on_error) = batch;
        let planned = chunks::PlannedChunkRun {
            doc_id,
            kernel_gen: generation,
            chunk_id,
            doc_language: language.as_str().to_owned(),
            request: chunks::ExecutionRequest::new(language, code.to_owned()),
            include_plot: false,
            include_tables: false,
        };
        self.pending_runs.push(pool::PendingRun { planned, batch_id, stop_on_error, invalidated: false, rx });
    }

    #[cfg(test)]
    pub(crate) fn queue_for_test(&mut self, chunk_id: i64, language: CodeLanguage, code: &str, batch: (u64, u64, u64, bool)) {
        let (doc_id, generation, batch_id, stop_on_error) = batch;
        let planned = chunks::PlannedChunkRun {
            doc_id,
            kernel_gen: generation,
            chunk_id,
            doc_language: language.as_str().to_owned(),
            request: chunks::ExecutionRequest::new(language, code.to_owned())
                .with_timeout(chunks::DEFAULT_TIMEOUT_MS)
                .with_plot_capture(true)
                .with_plot_format(chunks::PlotFormat::Both),
            include_plot: false,
            include_tables: false,
        };
        self.queued_runs.push_back(pool::QueuedRun { planned, batch_id, stop_on_error });
    }

    #[cfg(test)]
    pub(crate) fn pending_len_for_test(&self) -> usize {
        self.pending_runs.len()
    }

    #[cfg(test)]
    pub(crate) fn queued_len_for_test(&self) -> usize {
        self.queued_runs.len()
    }

    #[cfg(test)]
    pub(crate) fn pending_batch_ids_for_test(&self) -> Vec<(i64, u64)> {
        self.pending_runs.iter().map(|r| (r.planned.chunk_id, r.batch_id)).collect()
    }

    #[cfg(test)]
    pub(crate) fn queued_batch_ids_for_test(&self) -> Vec<(i64, u64)> {
        self.queued_runs.iter().map(|r| (r.planned.chunk_id, r.batch_id)).collect()
    }

    #[cfg(test)]
    pub(crate) fn allocate_batch_id_for_test(&mut self) -> u64 {
        self.allocate_batch_id()
    }

    #[cfg(test)]
    pub(crate) fn fail_next_launch_for_test(&mut self, err: impl Into<String>) {
        self.fail_next_launch = Some(err.into());
    }
}

#[cfg(test)]
impl CodeChunkEntry {
    pub(crate) fn mark_dirty_for_test(&mut self) {
        self.dirty = true;
    }
}

/// Whether any mounted code chunk widget has unsaved edits not yet flushed into the document.
pub(crate) fn has_dirty_chunks(app: &WordApp) -> bool {
    app.canvas.code_chunks.has_dirty()
}

/// Collect dirty chunk drafts from the active canvas so switching documents in one window preserves them.
pub(crate) fn take_dirty_drafts(app: &mut WordApp) -> ChunkDrafts {
    let mut drafts = std::mem::take(&mut app.canvas.code_chunks.restored_drafts);
    for (&id, entry) in &app.canvas.code_chunks.widgets {
        if entry.dirty {
            drafts.insert(id, (entry.widget.language(), entry.widget.code().to_owned()));
        }
    }
    drafts
}

/// Discard dirty chunk drafts back to each chunk's last committed `(language, code)` in the document.
pub(crate) fn revert_dirty_chunks(app: &mut WordApp) -> bool {
    let mut reverted = !app.canvas.code_chunks.restored_drafts.is_empty();
    app.canvas.code_chunks.restored_drafts.clear();
    for entry in app.canvas.code_chunks.widgets.values_mut() {
        if entry.dirty {
            let lang = entry.last_synced_lang;
            let code = entry.last_synced_code.clone();
            if entry.widget.language() != lang {
                entry.widget.set_language(lang);
            }
            if entry.widget.code() != code {
                entry.widget.set_code(code);
            }
            entry.dirty = false;
            entry.last_edit_time = None;
            reverted = true;
        }
    }
    reverted
}

fn parse_lang(raw: &str) -> Option<CodeLanguage> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "r" => Some(CodeLanguage::R),
        "python" | "py" => Some(CodeLanguage::Python),
        _ => None,
    }
}

fn reconcile_entry(entry: &mut CodeChunkEntry, lang: CodeLanguage, code: &str) -> bool {
    if entry.last_synced_lang == lang && entry.last_synced_code == code {
        return false;
    }
    if entry.widget.language() != lang {
        entry.widget.set_language(lang);
    }
    if entry.widget.language() != lang {
        return false;
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
        plots: o.plots.clone(),
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
        plots: o.plots.clone(),
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

/// Synchronize widget state with `app.session.doc` and `app.session.chunks` before `layout()`.
pub(crate) fn sync_before_layout(app: &mut WordApp, ui: &Ui) {
    let ctx = ui.ctx();
    let now = ui.input(|i| i.time);
    pool::poll_pending_runs(app, ctx);
    app.canvas.code_chunks.expose_announcement(ctx);

    let read_mode = app.session.view.read_mode;
    if read_mode {
        app.canvas.code_chunks.pending_focus = None;
        app.canvas.code_chunks.active_chunk = None;
        app.canvas.code_chunks.context_chunk = None;
        app.canvas.code_chunks.screen_rects.clear();
    }

    // O(1) fast path when the document body contains no chunks and nothing is mounted or running.
    if !app.session.doc.has_chunks(StoryRef::Body) && app.canvas.code_chunks.widgets.is_empty() && app.canvas.code_chunks.pending_runs.is_empty() {
        app.canvas.code_chunks.queued_runs.clear();
        app.canvas.code_chunks.restored_drafts.clear();
        app.canvas.code_chunks.screen_rects.clear();
        app.canvas.code_chunks.active_chunk = None;
        app.canvas.code_chunks.context_chunk = None;
        app.canvas.code_chunks.pending_focus = None;
        app.session.set_chunk_heights(BTreeMap::new());
        return;
    }

    let live_ids = reconcile_mounted_widgets(app, read_mode);
    prune_stale_ids(&mut app.canvas.code_chunks, &live_ids);
    flush_expired_drafts(app, now);
    sync_session_outputs(app);
    sync_chunk_heights(app);
}

fn reconcile_mounted_widgets(app: &mut WordApp, read_mode: bool) -> std::collections::HashSet<i64> {
    let refs: Vec<ChunkRef> = app.canvas.code_chunks.top_level_chunks(&app.session).to_vec();
    let mut live_ids = std::collections::HashSet::with_capacity(refs.len());
    for r in &refs {
        let Some(id) = r.chunk.id else { continue };
        let Some(lang) = parse_lang(&r.chunk.language) else { continue };
        live_ids.insert(id);

        let label = r.chunk.options.label().unwrap_or("");
        if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) {
            entry.widget.read_only = read_mode;
            reconcile_entry(entry, lang, &r.chunk.code);
            if !entry.widget.is_editing_label() && entry.widget.label() != label {
                entry.widget.set_label(label);
            }
        } else {
            let mut widget = CodeChunk::new(lang, r.chunk.code.clone());
            widget.read_only = read_mode;
            widget.set_label(label);
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

        if let Some((draft_lang, draft_code)) = app.canvas.code_chunks.restored_drafts.remove(&id)
            && let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id)
        {
            if entry.widget.language() != draft_lang {
                entry.widget.set_language(draft_lang);
            }
            if entry.widget.code() != draft_code {
                entry.widget.set_code(draft_code);
            }
            entry.dirty = true;
            entry.last_edit_time = None;
        }
    }
    live_ids
}

fn prune_stale_ids(state: &mut CodeChunksState, live_ids: &std::collections::HashSet<i64>) {
    state.widgets.retain(|id, _| live_ids.contains(id));
    state.queued_runs.retain(|q| live_ids.contains(&q.planned.chunk_id));
    state.restored_drafts.retain(|id, _| live_ids.contains(id));
    if state.active_chunk.is_some_and(|id| !live_ids.contains(&id)) {
        state.active_chunk = None;
    }
    if state.context_chunk.is_some_and(|id| !live_ids.contains(&id)) {
        state.context_chunk = None;
    }
    if state.pending_focus.is_some_and(|(id, _)| !live_ids.contains(&id)) {
        state.pending_focus = None;
    }
}

fn flush_expired_drafts(app: &mut WordApp, now: f64) {
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
}

fn sync_session_outputs(app: &mut WordApp) {
    let rev = app.session.chunks.rev();
    let kernel_gen = app.session.chunks.generation();
    let gen_changed = app.canvas.code_chunks.last_generation != Some(kernel_gen);
    if gen_changed {
        app.canvas.code_chunks.invalidate_stale_runs(kernel_gen);
        app.canvas.code_chunks.reap_invalidated_runs();
    }
    if app.canvas.code_chunks.last_outputs_rev != Some(rev) || gen_changed {
        for (&id, entry) in &mut app.canvas.code_chunks.widgets {
            if entry.widget.is_running() {
                continue;
            }
            if let Some((out, out_rev)) = app.session.chunks.output_with_rev(id) {
                if entry.applied_output_rev != Some(out_rev) {
                    entry.widget.set_output(to_widget_output(out));
                    entry.applied_output_rev = Some(out_rev);
                }
            } else if entry.applied_output_rev.is_some()
                || (entry.widget.output().is_some() && (gen_changed || entry.widget.output().is_some_and(|o| o.error.is_none())))
            {
                entry.widget.clear_output();
                entry.applied_output_rev = None;
            }
        }
        app.canvas.code_chunks.last_outputs_rev = Some(rev);
        app.canvas.code_chunks.last_generation = Some(kernel_gen);
    }
}

fn sync_chunk_heights(app: &mut WordApp) {
    let mut heights = BTreeMap::new();
    for (&id, entry) in &app.canvas.code_chunks.widgets {
        let h = entry.measured_centipts.unwrap_or_else(|| default_height_centipts(&entry.widget));
        heights.insert(id, h);
    }
    app.session.set_chunk_heights(heights);
}

/// Flush one dirty chunk widget into the document as a clean `code.update_chunk` transaction.
pub(crate) fn flush_one(app: &mut WordApp, id: i64) -> Result<(), String> {
    app.canvas.code_chunks.flush_one(&mut app.session, id)
}

/// Flush every dirty code chunk editor into the document before Undo/Redo/Save/Export/Run.
pub(crate) fn flush_dirty(app: &mut WordApp) -> Result<(), String> {
    flush_dirty_except(app, None)
}

/// Flush dirty code chunks, skipping an optional target chunk (e.g. for `code.delete_chunk`).
pub(crate) fn flush_dirty_except(app: &mut WordApp, except: Option<i64>) -> Result<(), String> {
    let mut first_err = app.canvas.code_chunks.flush_dirty_except(&mut app.session, except).err();
    // Also flush dirty chunk drafts from other non-shown windows displaying this document.
    let current_doc = app.session.document_id();
    if let Err(err) = app.windows.flush_dirty_chunks_for(&mut app.session, current_doc, except)
        && first_err.is_none()
    {
        first_err = Some(err);
    }
    match first_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn clear_chunk_output(app: &mut WordApp, id: i64) {
    let _ = app.run("code.clear_output", json!({ "chunkId": id }));
    if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) {
        entry.widget.clear_output();
        entry.applied_output_rev = None;
    }
}

/// Whether the document caret currently sits inside a mounted and placed code chunk (so the canvas caret hides).
pub(crate) fn caret_in_mounted_chunk(app: &mut WordApp) -> bool {
    let layout = app.session.layout();
    if let Some(active_id) = app.canvas.code_chunks.active_chunk {
        return chunk_placement(&layout, active_id).is_some();
    }
    let pos = &app.session.sel.focus;
    chunks::chunk_at(&app.session.doc, pos)
        .is_some_and(|r| r.chunk.id.is_some_and(|id| app.canvas.code_chunks.is_mounted(id) && chunk_placement(&layout, id).is_some()))
}

/// Draw the right-click context menu for mounted code chunk `id`.
pub(crate) fn chunk_context_menu(app: &mut WordApp, ui: &mut egui::Ui, id: i64) {
    use craft::keymap::CommandRef;
    use craft::ui::contract::{Props as _, Show as _};
    use craft::ui::display::Divider;
    use craft::ui::menu::MenuItem;

    let writable = !app.session.view.read_mode;
    let show_code = app.canvas.code_chunks.widgets.get(&id).is_none_or(|e| e.widget.show_code);

    let _ = MenuItem::new(crate::tl!("Run Chunk").to_string())
        .command(CommandRef::with_params("code.run_chunk", json!({ "chunkId": id })))
        .enabled(writable && app.session.why_disabled("code.run_chunk").is_none())
        .show(ui);
    let toggle_label = if show_code { crate::tl!("Hide Code (Teaching Mode)") } else { crate::tl!("Show Code") };
    if MenuItem::new(toggle_label.to_string()).show(ui).ran
        && let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id)
    {
        entry.widget.show_code = !entry.widget.show_code;
    }
    let has_output = app.canvas.code_chunks.widgets.get(&id).is_some_and(|e| e.widget.output().is_some()) || app.session.chunks.output(id).is_some();
    if MenuItem::new(crate::tl!("Clear Output").to_string()).enabled(writable && has_output).show(ui).ran {
        clear_chunk_output(app, id);
    }
    let _ = MenuItem::new(crate::tl!("Restart Kernel").to_string())
        .command(CommandRef::with_params("code.restart_kernel", json!({})))
        .enabled(writable)
        .show(ui);
    let _ = Divider::horizontal().show(ui);
    if MenuItem::new(crate::tl!("Delete Code Chunk").to_string()).enabled(writable).show(ui).ran {
        let _ = app.run("code.delete_chunk", json!({ "chunkId": id }));
        app.canvas.focus_page(ui.ctx());
    }
}
