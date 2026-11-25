//! Background job pool and engine-unified run scheduling/settlement for code chunks.

use std::collections::HashSet;
use std::sync::mpsc::Receiver;

use serde_json::{Value, json};
use wordcraft_engine::chunks::{
    ApplyChunkRunOutcome, CodeLanguage, ExecutionResult, PlannedChunkRun, RunAllEntry, apply_chunk_run_result, plan_chunk_run, plan_run_all,
};

use super::to_widget_output;
use crate::WordApp;
use crate::code_chunk::ChunkOutput as WidgetOutput;

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
const MAX_CONCURRENT_CHUNK_RUNS: usize = 4;

/// In-flight UI background execution tracked to store completion back into `Session::chunks`.
pub(super) struct PendingRun {
    pub(super) planned: PlannedChunkRun,
    pub(super) batch_id: u64,
    pub(super) stop_on_error: bool,
    pub(super) invalidated: bool,
    pub(super) rx: Receiver<Result<ExecutionResult, String>>,
}

/// Queued background run waiting for a concurrency slot in `Run All`.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(super) struct QueuedRun {
    pub(super) planned: PlannedChunkRun,
    pub(super) batch_id: u64,
    pub(super) stop_on_error: bool,
}

fn language_display(language: CodeLanguage) -> &'static str {
    match language {
        CodeLanguage::R => crate::tl!("R"),
        CodeLanguage::Python => crate::tl!("Python"),
    }
}

fn announce_chunk_completion(app: &mut WordApp, ctx: &egui::Context, language: CodeLanguage, success: bool) {
    let lang = language_display(language);
    let template = if success { crate::tl!("{lang} code chunk finished") } else { crate::tl!("{lang} code chunk failed") };
    let msg = crate::i18n::fmt(template, &[("lang", lang)]);
    app.canvas.code_chunks.set_announcement(msg, ctx);
}

fn settle_single_run(app: &mut WordApp, ctx: &egui::Context, run: PendingRun, res: Result<ExecutionResult, String>) -> (bool, bool) {
    let batch_failed = run.stop_on_error && !res.as_ref().is_ok_and(|exec| exec.success);
    let chunk_id = run.planned.chunk_id;
    let language = run.planned.request.language;

    match apply_chunk_run_result(&mut app.session, &run.planned, res) {
        Ok(ApplyChunkRunOutcome::Applied { chunk_id, success, output_rev, .. }) => {
            if let Some(out) = app.session.chunks.output(chunk_id).cloned()
                && let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&chunk_id)
            {
                entry.widget.finish_attached_run(to_widget_output(&out));
                entry.applied_output_rev = output_rev;
            }
            announce_chunk_completion(app, ctx, language, success);
            (batch_failed, true)
        }
        Ok(ApplyChunkRunOutcome::Stale(_)) => {
            if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&chunk_id)
                && entry.widget.is_running()
            {
                entry.widget.clear_output();
                entry.applied_output_rev = None;
            }
            (batch_failed, false)
        }
        Err(err) => {
            if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&chunk_id) {
                entry.widget.finish_attached_run(WidgetOutput::error(err.to_string()));
                entry.applied_output_rev = None;
            }
            announce_chunk_completion(app, ctx, language, false);
            (run.stop_on_error, true)
        }
    }
}

/// Poll background engine-initiated runs so completed results land in `Session::chunks` and widgets.
pub(super) fn poll_pending_runs(app: &mut WordApp, ctx: &egui::Context) {
    if app.canvas.code_chunks.pending_runs.is_empty() && app.canvas.code_chunks.queued_runs.is_empty() {
        return;
    }
    let current_gen = app.session.chunks.generation();
    app.canvas.code_chunks.invalidate_stale_runs(current_gen);
    let pending = std::mem::take(&mut app.canvas.code_chunks.pending_runs);
    let mut still_running = Vec::new();
    let mut any_stored = false;
    let mut failed_batches = Vec::new();

    for run in pending {
        if run.invalidated {
            if matches!(run.rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
                still_running.push(run);
            }
            continue;
        }
        match run.rx.try_recv() {
            Ok(res) => {
                let batch_id = run.batch_id;
                let (failed, stored) = settle_single_run(app, ctx, run, res);
                if failed {
                    failed_batches.push(batch_id);
                }
                any_stored |= stored;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                let batch_id = run.batch_id;
                let msg = crate::tl!("The code runner stopped before it reported a result.").to_owned();
                let (failed, stored) = settle_single_run(app, ctx, run, Err(msg));
                if failed {
                    failed_batches.push(batch_id);
                }
                any_stored |= stored;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                still_running.push(run);
            }
        }
    }

    app.canvas.code_chunks.pending_runs = still_running;
    if !failed_batches.is_empty() {
        app.canvas.code_chunks.queued_runs.retain(|q| !failed_batches.contains(&q.batch_id));
    }
    let _ = pump_queued_runs(app, Some(ctx));
    if !app.canvas.code_chunks.pending_runs.is_empty() {
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    }
    if any_stored {
        ctx.request_repaint();
    }
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn pump_queued_runs(app: &mut WordApp, ctx: Option<&egui::Context>) -> Vec<(i64, u64, String)> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (app, ctx);
        Vec::new()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let current_gen = app.session.chunks.generation();
        app.canvas.code_chunks.invalidate_stale_runs(current_gen);
        app.canvas.code_chunks.reap_invalidated_runs();
        let live_ids: HashSet<i64> = app.canvas.code_chunks.top_level_chunks(&app.session).iter().filter_map(|r| r.chunk.id).collect();
        let pending_ids: HashSet<i64> = app.canvas.code_chunks.pending_runs.iter().filter(|p| !p.invalidated).map(|p| p.planned.chunk_id).collect();
        app.canvas
            .code_chunks
            .queued_runs
            .retain(|q| q.planned.kernel_gen == current_gen && live_ids.contains(&q.planned.chunk_id) && !pending_ids.contains(&q.planned.chunk_id));

        let mut launch_errors = Vec::new();
        while app.canvas.code_chunks.pending_runs.len() < MAX_CONCURRENT_CHUNK_RUNS {
            let Some(front) = app.canvas.code_chunks.queued_runs.front() else {
                break;
            };
            if front.stop_on_error && app.canvas.code_chunks.pending_runs.iter().any(|p| !p.invalidated && p.batch_id == front.batch_id) {
                break;
            }
            let Some(next) = app.canvas.code_chunks.queued_runs.pop_front() else {
                break;
            };
            if app.canvas.code_chunks.pending_runs.iter().any(|p| !p.invalidated && p.planned.chunk_id == next.planned.chunk_id) {
                continue;
            }
            let (chunk_id, batch_id, stop_on_error) = (next.planned.chunk_id, next.batch_id, next.stop_on_error);
            if let Err(err) = launch_worker(app, next, ctx) {
                app.status(err.clone());
                if stop_on_error {
                    app.canvas.code_chunks.queued_runs.retain(|q| q.batch_id != batch_id);
                }
                launch_errors.push((chunk_id, batch_id, err));
            }
        }
        if !app.canvas.code_chunks.pending_runs.is_empty()
            && let Some(ctx) = ctx
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        launch_errors
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn launch_worker(app: &mut WordApp, queued: QueuedRun, ctx: Option<&egui::Context>) -> Result<(), String> {
    #[cfg(test)]
    if let Some(err) = app.canvas.code_chunks.fail_next_launch.take() {
        return Err(err);
    }
    let req = queued.planned.request.clone();
    let chunk_id = queued.planned.chunk_id;

    let (tx_ui, rx_ui) = std::sync::mpsc::channel();
    let (tx_store, rx_store) = std::sync::mpsc::channel();
    let ctx_clone = ctx.cloned();
    app.host.jobs().spawn(
        "code_chunk",
        Box::new(move || {
            let res = wordcraft_engine::chunks::execute(&req);
            let clone = res.as_ref().map(Clone::clone).map_err(|e| e.to_string());
            let _ = tx_ui.send(res);
            let _ = tx_store.send(clone);
            if let Some(ctx) = ctx_clone {
                ctx.request_repaint();
            }
        }),
    );

    if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&chunk_id) {
        entry.widget.attach_run(rx_ui);
        entry.applied_output_rev = None;
    }
    app.canvas.code_chunks.pending_runs.push(PendingRun {
        planned: queued.planned,
        batch_id: queued.batch_id,
        stop_on_error: queued.stop_on_error,
        invalidated: false,
        rx: rx_store,
    });
    if let Some(ctx) = ctx {
        ctx.request_repaint();
    }
    Ok(())
}

fn params_with_active_chunk(params: &Value, active_chunk: Option<i64>) -> Value {
    if !params.get("chunkId").is_none_or(Value::is_null) {
        return params.clone();
    }
    let Some(id) = active_chunk else {
        return params.clone();
    };
    let mut normalized = params.clone();
    if let Value::Object(ref mut map) = normalized {
        map.insert("chunkId".to_string(), json!(id));
        normalized
    } else {
        json!({ "chunkId": id })
    }
}

/// Non-blocking UI execution for `code.run_chunk` when dispatched interactively via `WordApp::run`.
pub(crate) fn run_chunk_async(app: &mut WordApp, params: &Value, ctx: Option<&egui::Context>) -> Result<Value, String> {
    if let Some(why) = app.session.why_disabled("code.run_chunk") {
        return Err(why.into());
    }
    let current_gen = app.session.chunks.generation();
    app.canvas.code_chunks.invalidate_stale_runs(current_gen);
    app.canvas.code_chunks.reap_invalidated_runs();

    let effective_params = params_with_active_chunk(params, app.canvas.code_chunks.active_chunk);
    let planned = plan_chunk_run(&mut app.session, &effective_params).map_err(|e| e.to_string())?;
    app.canvas.code_chunks.invalidate_chunk_cache();
    let id = planned.chunk_id;
    let doc_language = planned.doc_language.clone();

    if app.canvas.code_chunks.has_in_flight_run(id) {
        return Err(format!("code chunk {id} is already running"));
    }

    #[cfg(target_arch = "wasm32")]
    {
        let _ = (planned, ctx);
        return app.session.run("code.run_chunk", &effective_params).map_err(|e| e.to_string());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let queued = QueuedRun { planned, batch_id: 0, stop_on_error: false };
        app.canvas.code_chunks.queued_runs.push_back(queued);
        let launch_errors = pump_queued_runs(app, ctx);
        if let Some((_, _, err)) = launch_errors.into_iter().find(|(cid, _, _)| *cid == id) {
            return Err(err);
        }
        let started = app.canvas.code_chunks.pending_runs.iter().any(|p| !p.invalidated && p.planned.chunk_id == id);
        if started {
            Ok(json!({ "started": true, "chunk": { "id": id, "language": doc_language } }))
        } else if app.canvas.code_chunks.queued_runs.iter().any(|q| q.planned.chunk_id == id) {
            Ok(json!({ "started": false, "queued": true, "chunk": { "id": id, "language": doc_language } }))
        } else {
            Err(format!("failed to start or queue code chunk {id}"))
        }
    }
}

/// Non-blocking UI execution for `code.run_all` when dispatched interactively via `WordApp::run`.
#[cfg_attr(not(test), allow(dead_code))] // Wired by PR 9 command handlers.
pub(crate) fn run_all_async(app: &mut WordApp, params: &Value, ctx: Option<&egui::Context>) -> Result<Value, String> {
    if let Some(why) = app.session.why_disabled("code.run_all") {
        return Err(why.into());
    }
    let stop_on_error = match params.get("stopOnError") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("stopOnError: expected a boolean".into()),
    };
    let entries = plan_run_all(&mut app.session, params).map_err(|e| e.to_string())?;
    app.canvas.code_chunks.invalidate_chunk_cache();

    let generation = app.session.chunks.generation();
    app.canvas.code_chunks.invalidate_stale_runs(generation);
    app.canvas.code_chunks.reap_invalidated_runs();
    let batch_id = app.canvas.code_chunks.allocate_batch_id();
    let mut batch = Vec::new();
    let mut seen_ids = HashSet::new();
    for entry in entries {
        match entry {
            RunAllEntry::Skipped { .. } => {}
            RunAllEntry::Rejected { error, .. } => return Err(error),
            RunAllEntry::Run(planned) => {
                if !seen_ids.insert(planned.chunk_id) || app.canvas.code_chunks.has_in_flight_run(planned.chunk_id) {
                    return Err(format!("code chunk {} is already running", planned.chunk_id));
                }
                batch.push(QueuedRun { planned, batch_id, stop_on_error });
            }
        }
    }
    if batch.is_empty() {
        return Ok(json!({ "started": 0, "queued": 0 }));
    }

    #[cfg(target_arch = "wasm32")]
    {
        let _ = (ctx, batch);
        return app.session.run("code.run_all", params).map_err(|e| e.to_string());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        app.canvas.code_chunks.queued_runs.extend(batch);
        let launch_errors = pump_queued_runs(app, ctx);
        let started = app.canvas.code_chunks.pending_runs.iter().filter(|p| !p.invalidated && p.batch_id == batch_id).count();
        let queued = app.canvas.code_chunks.queued_runs.iter().filter(|q| q.batch_id == batch_id).count();
        if started == 0
            && queued == 0
            && let Some((_, _, err)) = launch_errors.into_iter().find(|(_, bid, _)| *bid == batch_id)
        {
            return Err(err);
        }
        Ok(json!({ "started": started, "queued": queued }))
    }
}
