//! Canvas mounting, hit testing, focus handoff, scrolling, and action dispatch for code chunks.

use std::collections::{BTreeMap, HashSet};

use craft::behavior::kurbo::Point;
use craft::pages::ui::{GoTo, PlacedPage};
use egui::{Pos2, Rect, Ui, vec2};
use serde_json::json;
use wordcraft_doc::StoryRef;
use wordcraft_engine::chunks;
use wordcraft_layout::{DocLayout, Placed};

use super::{HEIGHT_HYSTERESIS_CENTIPTS, clear_chunk_output, default_height_centipts, flush_one, to_engine_output};
use crate::WordApp;
use crate::code_chunk::{CodeChunkAction, CompletedRun};

#[derive(Default)]
struct PaintEvents {
    blur_flush: Vec<i64>,
    newly_focused: Vec<(i64, usize)>,
    completed_runs: Vec<(i64, CompletedRun)>,
    actions: Vec<(i64, CodeChunkAction)>,
    label_commits: Vec<(i64, String)>,
    run_requests: Vec<i64>,
    clear_requests: Vec<i64>,
    arrow_exits: Vec<(i64, i32)>,
    context_click: Option<(i64, Pos2)>,
    height_changed: bool,
}

impl PaintEvents {
    fn record(&mut self, id: i64, page_index: usize, read_mode: bool, frame: crate::code_chunk::EmbeddedFrame) {
        if !read_mode && frame.editor_has_focus {
            self.newly_focused.push((id, page_index));
        }
        if frame.editor_lost_focus {
            self.blur_flush.push(id);
        }
        if let Some(completed) = frame.completed_run {
            self.completed_runs.push((id, completed));
        }
        if !read_mode {
            if frame.clear_requested {
                self.clear_requests.push(id);
            }
            if let Some(action) = frame.action {
                self.actions.push((id, action));
            }
            if let Some(label) = frame.committed_label {
                self.label_commits.push((id, label));
            }
            if frame.run_requested {
                self.run_requests.push(id);
            }
            if let Some(dir) = frame.arrow_exit {
                self.arrow_exits.push((id, dir));
            }
        }
        if let Some(pos) = frame.context_click_pos {
            self.context_click = Some((id, pos));
        }
    }
}

fn collect_visible_chunks(app: &mut WordApp, ui: &Ui, layout: &DocLayout, pages: &[PlacedPage]) -> Vec<(i64, usize, Rect, f32)> {
    app.canvas.code_chunks.screen_rects.clear();
    let view_clip = ui.clip_rect().expand(120.0);
    let mut visible = Vec::new();
    for placed in pages {
        let Some(page) = layout.pages.get(placed.page) else { continue };
        let pscale = placed.scale as f32;
        for it in &page.items {
            if let Placed::Chunk { id, rect, .. } = it {
                let min = placed.to_screen(Point::new(f64::from(rect.x), f64::from(rect.y)));
                let max = placed.to_screen(Point::new(f64::from(rect.x + rect.w), f64::from(rect.y + rect.h)));
                let sr = Rect::from_two_pos(min, max);
                app.canvas.code_chunks.screen_rects.insert(*id, sr);
                if sr.intersects(view_clip) || app.canvas.code_chunks.pending_focus.is_some_and(|(fid, _)| fid == *id) {
                    visible.push((*id, placed.page, sr, pscale));
                }
            }
        }
    }
    visible
}

fn reconcile_active_chunk_focus(app: &mut WordApp, ui: &Ui, layout: &DocLayout) {
    // Offscreen, unplaced (e.g. Outline view), or Teaching-Mode-hidden chunks are not passed to
    // `TextEdit::show`, so `editor_lost_focus` never fires for them; reconcile `active_chunk` against egui focus.
    let Some(active_id) = app.canvas.code_chunks.active_chunk else {
        return;
    };
    let mut focus_canvas = false;
    let is_placed = chunk_placement(layout, active_id).is_some();
    let still_focused = !app.session.view.read_mode
        && app.canvas.code_chunks.widgets.get(&active_id).is_some_and(|entry| {
            let editor_id = entry.widget.editor_id();
            if !entry.widget.show_code || !is_placed {
                if ui.ctx().memory(|m| m.has_focus(editor_id)) {
                    ui.ctx().memory_mut(|m| m.surrender_focus(editor_id));
                    focus_canvas = true;
                }
                false
            } else {
                ui.ctx().memory(|m| m.has_focus(editor_id))
            }
        });
    if focus_canvas {
        app.canvas.focus_page(ui.ctx());
    }
    if !still_focused {
        if let Err(err) = flush_one(app, active_id) {
            app.status(err);
        }
        app.canvas.code_chunks.active_chunk = None;
    }
}

fn apply_focus_events(app: &mut WordApp, ui: &Ui, layout: &DocLayout, events: &PaintEvents) {
    if let Some(click) = events.context_click {
        app.canvas.code_chunks.pending_context_click = Some(click);
    }

    for &id in &events.blur_flush {
        if let Err(err) = flush_one(app, id) {
            app.status(err);
        }
        if app.canvas.code_chunks.active_chunk == Some(id) {
            app.canvas.code_chunks.active_chunk = None;
        }
    }

    if let Some((id, pi)) = events.newly_focused.last().copied() {
        if let Some(prev_id) = app.canvas.code_chunks.active_chunk.filter(|&prev| prev != id)
            && let Err(err) = flush_one(app, prev_id)
        {
            app.status(err);
        }
        app.canvas.code_chunks.active_chunk = Some(id);
        app.canvas.focus_pending = false;
        app.session.page_hint = pi;
        if !chunks::chunk_at(&app.session.doc, &app.session.sel.focus).is_some_and(|r| r.chunk.id == Some(id))
            && let Some(r) = app.canvas.code_chunks.top_level_chunks(&app.session).iter().find(|r| r.chunk.id == Some(id)).cloned()
        {
            app.session.sel = wordcraft_engine::Selection::caret(r.start);
        }
    }

    reconcile_active_chunk_focus(app, ui, layout);
}

fn apply_edit_and_run_events(app: &mut WordApp, ui: &Ui, events: PaintEvents) {
    for (id, completed) in events.completed_runs {
        if let Err(err) = flush_one(app, id) {
            app.status(err);
        }
        if let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id)
            && entry.widget.language() == completed.language
        {
            let engine_out = to_engine_output(&completed.output);
            app.session.chunks.store_for(id, completed.language.as_str(), &completed.code, engine_out);
            entry.applied_output_rev = app.session.chunks.output_with_rev(id).map(|(_, r)| r);
        }
    }

    for id in events.clear_requests {
        clear_chunk_output(app, id);
    }

    for (id, label) in events.label_commits {
        match flush_one(app, id) {
            Ok(()) => {
                if let Err(err) = app.run("code.update_chunk", json!({ "chunkId": id, "label": label })) {
                    app.status(err);
                }
            }
            Err(err) => app.status(err),
        }
    }

    for (id, action) in events.actions {
        match flush_one(app, id) {
            Ok(()) => dispatch_chunk_action(app, id, action),
            Err(err) => app.status(err),
        }
    }

    for id in events.run_requests {
        if let Err(err) = flush_one(app, id) {
            app.status(err);
            continue;
        }
        if let Err(err) = super::run_chunk_async(app, &json!({ "chunkId": id }), Some(ui.ctx())) {
            app.status(err);
        }
        ui.ctx().request_repaint();
    }

    for (id, dir) in events.arrow_exits {
        exit_chunk_to_canvas(app, id, dir);
    }

    if events.height_changed {
        let mut heights = BTreeMap::new();
        for (&id, entry) in &app.canvas.code_chunks.widgets {
            heights.insert(id, entry.measured_centipts.unwrap_or_else(|| default_height_centipts(&entry.widget)));
        }
        if app.session.set_chunk_heights(heights) {
            ui.ctx().request_repaint();
        }
    }
}

fn apply_paint_events(app: &mut WordApp, ui: &Ui, layout: &DocLayout, events: PaintEvents) {
    apply_focus_events(app, ui, layout, &events);
    apply_edit_and_run_events(app, ui, events);
}

/// Locate a mounted chunk's page index and page-space rectangle in `layout`.
pub(crate) fn chunk_placement(layout: &DocLayout, chunk_id: i64) -> Option<(usize, wordcraft_geom::Rect)> {
    layout.pages.iter().enumerate().find_map(|(pi, page)| {
        page.items.iter().find_map(|it| match it {
            Placed::Chunk { id, rect, .. } if *id == chunk_id => Some((pi, *rect)),
            _ => None,
        })
    })
}

fn overshoot_axis(target: (f32, f32), view: (f32, f32)) -> f64 {
    if target.0 < view.0 {
        f64::from(target.0 - view.0)
    } else if target.1 > view.1 {
        f64::from(target.1 - view.1)
    } else {
        0.0
    }
}

/// Vertical screen-space band `(y_min, y_max)` of `chunk_sr` to bring inside `view_rect` when focusing a chunk.
pub(crate) fn chunk_focus_band_y(chunk_sr: Rect, view_rect: Rect, at_end: bool) -> (f32, f32) {
    let view_h = view_rect.height().max(48.0);
    let margin = 24.0_f32.min(view_h * 0.25);
    if chunk_sr.height() + 2.0 * margin <= view_h {
        (chunk_sr.min.y - margin, chunk_sr.max.y + margin)
    } else {
        let band_h = (view_h * 0.5).max(48.0).min(view_h);
        if at_end {
            let bottom = chunk_sr.max.y + margin.min(view_h - band_h);
            ((bottom - band_h).max(chunk_sr.min.y), bottom)
        } else {
            let top = chunk_sr.min.y - margin.min(view_h - band_h);
            (top, (top + band_h).min(chunk_sr.max.y))
        }
    }
}

fn target_chunk_screen_rect(
    layout: &DocLayout,
    pages: &[PlacedPage],
    laid_pages: &[Rect],
    target_page: usize,
    target_rect: wordcraft_geom::Rect,
) -> Option<Rect> {
    let page_h = layout.pages.get(target_page).map_or(1.0, |p| p.h.max(1.0));
    if let Some(placed) = pages.iter().find(|p| p.page == target_page) {
        let min = placed.to_screen(Point::new(f64::from(target_rect.x), f64::from(target_rect.y)));
        let max = placed.to_screen(Point::new(f64::from(target_rect.x + target_rect.w), f64::from(target_rect.y + target_rect.h)));
        return Some(Rect::from_two_pos(min, max));
    }
    if let Some(&page_rect) = laid_pages.get(target_page) {
        let shift = pages.first().and_then(|p| laid_pages.get(p.page).map(|laid| p.rect.min - laid.min)).unwrap_or(egui::Vec2::ZERO);
        let page_rect = page_rect.translate(shift);
        let s = page_rect.height() / page_h;
        let min = page_rect.min + vec2(target_rect.x * s, target_rect.y * s);
        let max = page_rect.min + vec2((target_rect.x + target_rect.w) * s, (target_rect.y + target_rect.h) * s);
        return Some(Rect::from_two_pos(min, max));
    }
    let anchor = pages.first()?;
    let s = anchor.scale as f32;
    let dy_pages: f32 = if target_page >= anchor.page {
        (anchor.page..target_page).filter_map(|p| layout.pages.get(p)).map(|p| p.h * s).sum()
    } else {
        -(target_page..anchor.page).filter_map(|p| layout.pages.get(p)).map(|p| p.h * s).sum::<f32>()
    };
    let page_min = anchor.rect.min + vec2(0.0, dy_pages);
    let min = page_min + vec2(target_rect.x * s, target_rect.y * s);
    let max = page_min + vec2((target_rect.x + target_rect.w) * s, (target_rect.y + target_rect.h) * s);
    Some(Rect::from_two_pos(min, max))
}

fn scroll_to_pending_chunk(
    app: &mut WordApp,
    ui: &Ui,
    layout: &DocLayout,
    pages: &[PlacedPage],
    target_page: usize,
    target_rect: wordcraft_geom::Rect,
    at_end: bool,
) {
    app.session.page_hint = target_page;
    let view_rect = if app.canvas.shown.view.is_positive() { app.canvas.shown.view } else { ui.clip_rect() };
    let page_h = layout.pages.get(target_page).map_or(1.0, |p| p.h.max(1.0));
    let chunk_sr = target_chunk_screen_rect(layout, pages, &app.canvas.shown.laid, target_page, target_rect);

    if let Some(sr) = chunk_sr {
        let band = chunk_focus_band_y(sr, view_rect, at_end);
        let dy = overshoot_axis(band, (view_rect.min.y, view_rect.max.y));
        if dy != 0.0 {
            app.canvas.view.offset.1 += dy;
        }
    } else {
        let edge_y = if at_end { target_rect.bottom() } else { target_rect.y };
        let frac = f64::from((edge_y / page_h).clamp(0.0, 1.0));
        app.canvas.view.go_to = Some(GoTo::Top { page: target_page, fraction: frac });
        app.canvas.view.current = target_page;
    }
    ui.ctx().request_repaint();
}

/// Resolve scroll adjustments in [`crate::canvas::show_pages`] when a chunk focus request is pending or scrolled an overhanging chunk.
pub(crate) fn apply_pending_focus_scroll(
    app: &mut WordApp,
    ctx: &egui::Context,
    held: bool,
    (placed_pages, laid_pages): (&[PlacedPage], &[Rect]),
    body_rect: Rect,
    fix: &mut crate::canvas::ScrollFix,
) {
    if held || (app.canvas.code_chunks.pending_focus.is_none() && app.canvas.view.offset == (0.0, 0.0)) {
        return;
    }
    let layout = app.session.layout();
    if let Some((fid, at_end)) = app.canvas.code_chunks.pending_focus
        && let Some((page, rect)) = chunk_placement(&layout, fid)
    {
        app.session.page_hint = page;
        let page_h = layout.pages.get(page).map_or(1.0, |p| p.h.max(1.0));
        let chunk_sr = if let Some(placed) = placed_pages.iter().find(|p| p.page == page) {
            let min = placed.to_screen(Point::new(f64::from(rect.x), f64::from(rect.y)));
            let max = placed.to_screen(Point::new(f64::from(rect.x + rect.w), f64::from(rect.y + rect.h)));
            Some(Rect::from_two_pos(min, max))
        } else if let Some(page_rect) = laid_pages.get(page).copied() {
            let s = page_rect.height() / page_h;
            let min = page_rect.min + vec2(rect.x * s, rect.y * s);
            let max = page_rect.min + vec2((rect.x + rect.w) * s, (rect.y + rect.h) * s);
            Some(Rect::from_two_pos(min, max))
        } else {
            None
        };
        match chunk_sr {
            Some(sr) => {
                let band = chunk_focus_band_y(sr, body_rect, at_end);
                fix.delta = (0.0, overshoot_axis(band, (body_rect.min.y, body_rect.max.y)));
                fix.go_to = None;
            }
            None => {
                let edge_y = if at_end { rect.bottom() } else { rect.y };
                fix.go_to = Some((page, f64::from((edge_y / page_h).clamp(0.0, 1.0))));
            }
        }
        ctx.request_repaint();
    } else if app.canvas.view.offset != (0.0, 0.0) {
        fix.delta = app.canvas.view.offset;
    }
}

/// Draw mounted `Placed::Chunk` widgets inside the scroll viewport and handle all interactions.
pub(crate) fn paint_and_interact(app: &mut WordApp, ui: &mut Ui, layout: &DocLayout, pages: &[PlacedPage]) {
    if app.canvas.code_chunks.widgets.is_empty() {
        app.canvas.code_chunks.screen_rects.clear();
        app.canvas.code_chunks.active_chunk = None;
        app.canvas.code_chunks.context_chunk = None;
        app.canvas.code_chunks.pending_focus = None;
        return;
    }
    let now = ui.input(|i| i.time);
    let mounted_positions = collect_visible_chunks(app, ui, layout, pages);

    let read_mode = app.session.view.read_mode;
    let pending_focus = if read_mode {
        app.canvas.code_chunks.pending_focus = None;
        app.canvas.code_chunks.active_chunk = None;
        None
    } else {
        app.canvas.code_chunks.pending_focus
    };
    let mut events = PaintEvents::default();

    for (id, pi, sr, pscale) in mounted_positions {
        let Some(entry) = app.canvas.code_chunks.widgets.get_mut(&id) else { continue };
        entry.widget.read_only = read_mode;
        let requested_edge = if !read_mode { pending_focus.filter(|(fid, _)| *fid == id).map(|(_, at_end)| at_end) } else { None };
        if let Some(at_end) = requested_edge {
            entry.widget.focus_edge(at_end);
            app.session.page_hint = pi;
        }

        // Measure intrinsic height cleanly without capping height to last frame's box.
        let max_rect = Rect::from_min_size(sr.min, vec2(sr.width().max(48.0), f32::INFINITY));
        let mut child_ui = ui.new_child(egui::UiBuilder::new().max_rect(max_rect));
        let frame = entry.widget.show_embedded(&mut child_ui);
        if let Some(at_end) = requested_edge
            && frame.editor_has_focus
        {
            app.canvas.code_chunks.pending_focus = None;
            let view_rect = if app.canvas.shown.view.is_positive() { app.canvas.shown.view } else { ui.clip_rect() };
            let band = chunk_focus_band_y(sr, view_rect, at_end);
            let dy = overshoot_axis(band, (view_rect.min.y, view_rect.max.y));
            if dy != 0.0 {
                app.canvas.view.offset.1 += dy;
                ui.ctx().request_repaint();
            }
        }

        let measured_pt = (child_ui.min_rect().height() / pscale.max(0.01)).clamp(24.0, 20000.0);
        let new_centipts = (measured_pt * 100.0).round() as u32;
        if entry.measured_centipts.is_none_or(|old| (old as i64 - new_centipts as i64).abs() > HEIGHT_HYSTERESIS_CENTIPTS) {
            entry.measured_centipts = Some(new_centipts);
            events.height_changed = true;
        }

        if frame.dirty {
            entry.dirty = true;
            entry.last_edit_time = Some(now);
        }
        events.record(id, pi, read_mode, frame);
    }

    apply_paint_events(app, ui, layout, events);

    if let Some((fid, at_end)) = app.canvas.code_chunks.pending_focus {
        if app.canvas.code_chunks.is_mounted(fid)
            && let Some((target_page, target_rect)) = chunk_placement(layout, fid)
        {
            scroll_to_pending_chunk(app, ui, layout, pages, target_page, target_rect, at_end);
        } else {
            app.canvas.code_chunks.pending_focus = None;
        }
    }
}

fn chunk_output_stale(app: &mut WordApp, chunk_id: i64) -> bool {
    if app.session.chunks.output(chunk_id).is_none() {
        return false;
    }
    let found = app
        .canvas
        .code_chunks
        .top_level_chunks(&app.session)
        .iter()
        .find(|r| r.chunk.id == Some(chunk_id))
        .map(|r| (r.chunk.language.clone(), r.chunk.code.clone()));
    found.is_some_and(|(lang, code)| !app.session.chunks.output_matches(chunk_id, &lang, &code))
}

fn dispatch_chunk_action(app: &mut WordApp, chunk_id: i64, action: CodeChunkAction) {
    let result = match action {
        CodeChunkAction::InsertPlot { png, svg } => {
            if chunk_output_stale(app, chunk_id) {
                app.status(crate::tl!("The code chunk changed since its last run; run the chunk again before inserting its plot."));
                app.canvas.focus_pending = true;
                return;
            }
            if let Some(bytes) = png {
                let b64 = wordcraft_engine::cmd::insert::base64_encode(&bytes);
                app.run("code.insert_plot", json!({ "chunkId": chunk_id, "png": b64, "svg": svg, "atCaret": true }))
            } else if app.session.chunks.output(chunk_id).is_some() {
                app.run("code.insert_plot", json!({ "chunkId": chunk_id, "atCaret": true }))
            } else {
                app.run("code.insert_plot", json!({ "chunkId": chunk_id, "svg": svg, "atCaret": true }))
            }
        }
        CodeChunkAction::InsertTable { headers, rows } => {
            let _ = flush_one(app, chunk_id);
            if chunk_output_stale(app, chunk_id) {
                app.status(crate::tl!("Table output is stale; run the chunk again."));
                app.canvas.focus_pending = true;
                return;
            }
            app.run("code.insert_table", json!({ "chunkId": chunk_id, "headers": headers, "rows": rows, "atCaret": true }))
        }
        CodeChunkAction::InsertOutput { text } => {
            let _ = flush_one(app, chunk_id);
            if chunk_output_stale(app, chunk_id) {
                app.status(crate::tl!("Console output is stale; run the chunk again."));
                app.canvas.focus_pending = true;
                return;
            }
            app.run("code.insert_output", json!({ "chunkId": chunk_id, "text": text, "atCaret": true }))
        }
    };
    if let Err(err) = result {
        app.status(err);
    }
    app.canvas.focus_pending = true;
    app.canvas.glide.restart(craft::platform::clock::monotonic_ms());
}

fn first_table_para_pos(doc: &wordcraft_doc::Document, target_block: usize) -> Option<wordcraft_doc::Pos> {
    let mut path_indices = vec![target_block as u32];
    let mut block = doc.body.get(target_block)?;
    loop {
        match &**block {
            wordcraft_doc::Block::Para(_) => {
                return Some(wordcraft_doc::Pos::new(StoryRef::Body, wordcraft_doc::Path(path_indices), 0));
            }
            wordcraft_doc::Block::Table(table) => {
                let row = table.rows.first()?;
                let cell = row.cells.first()?;
                let next_block = cell.blocks.first()?;
                path_indices.extend([0, 0, 0]);
                block = next_block;
            }
        }
    }
}

fn last_table_para_pos(doc: &wordcraft_doc::Document, target_block: usize) -> Option<wordcraft_doc::Pos> {
    let mut path_indices = vec![target_block as u32];
    let mut block = doc.body.get(target_block)?;
    loop {
        match &**block {
            wordcraft_doc::Block::Para(p) => {
                return Some(wordcraft_doc::Pos::new(StoryRef::Body, wordcraft_doc::Path(path_indices), p.len()));
            }
            wordcraft_doc::Block::Table(table) => {
                let row_idx = table.rows.len().checked_sub(1)?;
                let row = table.rows.get(row_idx)?;
                let cell_idx = row.cells.len().checked_sub(1)?;
                let cell = row.cells.get(cell_idx)?;
                let inner_idx = cell.blocks.len().checked_sub(1)?;
                let next_block = cell.blocks.get(inner_idx)?;
                path_indices.extend([row_idx as u32, cell_idx as u32, inner_idx as u32]);
                block = next_block;
            }
        }
    }
}

/// Move focus smoothly out of chunk `id` (`dir < 0`: above, `dir > 0`: below) into an adjacent chunk or canvas paragraph.
fn exit_chunk_to_canvas(app: &mut WordApp, id: i64, dir: i32) {
    if let Err(err) = flush_one(app, id) {
        app.status(err);
    }
    app.canvas.code_chunks.active_chunk = None;
    let refs = app.canvas.code_chunks.top_level_chunks(&app.session);
    let Some(idx) = refs.iter().position(|r| r.chunk.id == Some(id)) else {
        app.canvas.focus_pending = true;
        return;
    };
    let Some(current) = refs.get(idx).cloned() else {
        app.canvas.focus_pending = true;
        return;
    };
    let prev_chunk = idx.checked_sub(1).and_then(|i| refs.get(i)).cloned();
    let next_chunk = refs.get(idx + 1).cloned();
    let first_block = current.start.path.0.first().copied().unwrap_or(0) as usize;
    let last_block = current.end.path.0.first().copied().unwrap_or(0) as usize;

    if dir < 0 {
        exit_up(app, &current, prev_chunk.as_ref(), first_block);
    } else {
        exit_down(app, id, &current, next_chunk.as_ref(), last_block);
    }
}

fn exit_up(app: &mut WordApp, current: &wordcraft_doc::ChunkRef, prev_chunk: Option<&wordcraft_doc::ChunkRef>, first_block: usize) {
    if first_block == 0 {
        app.session.sel = wordcraft_engine::Selection::caret(current.start.clone());
        app.canvas.focus_pending = true;
        return;
    }
    let target_block = first_block - 1;
    if let Some(prev) = prev_chunk
        && prev.end.path.0.first().copied().unwrap_or(0) as usize == target_block
        && let Some(prev_id) = prev.chunk.id
        && app.canvas.code_chunks.is_mounted(prev_id)
    {
        app.canvas.code_chunks.request_focus(prev_id, true);
        return;
    }
    if let Some(pos) = app.session.doc.body.get(target_block).and_then(|b| match &**b {
        wordcraft_doc::Block::Para(p) => Some(wordcraft_doc::Pos::body(target_block, p.len())),
        wordcraft_doc::Block::Table(_) => last_table_para_pos(&app.session.doc, target_block),
    }) {
        app.session.sel = wordcraft_engine::Selection::caret(pos);
        app.canvas.focus_pending = true;
    } else if app.session.doc.body.get(target_block).is_some() {
        app.session.sel = wordcraft_engine::Selection::caret(app.session.doc.clamp(&wordcraft_doc::Pos::body(target_block, 0)));
        app.canvas.focus_pending = true;
    }
}

fn exit_down(app: &mut WordApp, id: i64, current: &wordcraft_doc::ChunkRef, next_chunk: Option<&wordcraft_doc::ChunkRef>, last_block: usize) {
    let target_block = last_block + 1;
    if let Some(next_ref) = next_chunk
        && next_ref.start.path.0.first().copied().unwrap_or(0) as usize == target_block
        && let Some(next_id) = next_ref.chunk.id
        && app.canvas.code_chunks.is_mounted(next_id)
    {
        app.canvas.code_chunks.request_focus(next_id, false);
        return;
    }
    if let Some(pos) = app.session.doc.body.get(target_block).and_then(|b| match &**b {
        wordcraft_doc::Block::Para(_) => Some(wordcraft_doc::Pos::body(target_block, 0)),
        wordcraft_doc::Block::Table(_) => first_table_para_pos(&app.session.doc, target_block),
    }) {
        app.session.sel = wordcraft_engine::Selection::caret(pos);
    } else if !app.session.view.read_mode && app.run("code.exit_below_chunk", json!({ "chunkId": id })).is_ok() {
        // A trailing normal paragraph now exists below the chunk with the caret inside it.
    } else {
        app.session.sel = wordcraft_engine::Selection::caret(current.end.clone());
    }
    app.canvas.focus_pending = true;
    app.canvas.glide.restart(craft::platform::clock::monotonic_ms());
}

fn page_has_body_content_below(pg: &wordcraft_layout::Page, chunk_rect: wordcraft_geom::Rect, x: f32, y: f32) -> bool {
    let pt = wordcraft_geom::Point::new(x, y);
    pg.items.iter().any(|it| match it {
        Placed::Lines { story: StoryRef::Body, para, l0, l1, x: lx, y: ly, turn, .. } => {
            if turn.is_turned() {
                it.turned_bounds().is_some_and(|r| r.expand(2.0).contains(pt))
            } else {
                para.lines.get(*l0).is_some_and(|first| {
                    (*l0..*l1).filter_map(|li| para.lines.get(li)).any(|l| {
                        let top = *ly + (l.top - first.top);
                        let bottom = top + l.height;
                        let left = *lx + l.left;
                        let right = *lx + l.right;
                        (y >= top - 2.0 && y <= bottom + 2.0 && x >= left - 2.0 && x <= right + 2.0)
                            || (bottom > chunk_rect.bottom() && right >= chunk_rect.x && left <= chunk_rect.right())
                    })
                })
            }
        }
        Placed::Cell { story: StoryRef::Body, rect, .. } => {
            rect.expand(2.0).contains(pt) || (rect.bottom() > chunk_rect.bottom() && rect.right() >= chunk_rect.x && rect.x <= chunk_rect.right())
        }
        _ => false,
    })
}

/// If a plain primary click lands in empty page space below a document-ending code chunk,
/// create/move into a trailing body paragraph below that chunk instead of snapping above it.
pub(crate) fn try_click_below_trailing_chunk(
    app: &mut WordApp,
    ctx: &egui::Context,
    layout: &DocLayout,
    page: usize,
    (x, y): (f32, f32),
    story: StoryRef,
) -> bool {
    if story != StoryRef::Body || app.session.view.read_mode {
        return false;
    }
    let Some((chunk_id, is_trailing)) = app.canvas.code_chunks.top_level_chunks(&app.session).last().and_then(|last_ref| {
        let id = last_ref.chunk.id?;
        let last_block = last_ref.end.path.0.first().copied().unwrap_or(0) as usize;
        let trailing = last_ref.end.path.0.len() == 1 && last_block.saturating_add(1) == app.session.doc.body.len();
        Some((id, trailing))
    }) else {
        return false;
    };
    if !is_trailing {
        return false;
    }
    let Some(pg) = layout.pages.get(page) else {
        return false;
    };
    let Some(chunk_rect) = pg.items.iter().find_map(|it| match it {
        Placed::Chunk { id, rect, .. } if *id == chunk_id => Some(*rect),
        _ => None,
    }) else {
        return false;
    };
    if y <= chunk_rect.bottom() || y > pg.body.bottom() || x < chunk_rect.x || x > chunk_rect.right() {
        return false;
    }
    if page_has_body_content_below(pg, chunk_rect, x, y) {
        return false;
    }
    if let Err(err) = flush_one(app, chunk_id) {
        app.status(err);
    }
    if app.canvas.code_chunks.active_chunk == Some(chunk_id) {
        app.canvas.code_chunks.active_chunk = None;
    }
    if let Some(entry) = app.canvas.code_chunks.widgets.get(&chunk_id) {
        let editor_id = entry.widget.editor_id();
        if ctx.memory(|m| m.has_focus(editor_id)) {
            ctx.memory_mut(|m| m.surrender_focus(editor_id));
        }
    }
    if app.run("code.exit_below_chunk", json!({ "chunkId": chunk_id })).is_err() {
        return false;
    }
    app.session.page_hint = page;
    app.canvas.dragging = false;
    app.canvas.focus_page(ctx);
    app.canvas.glide.restart(craft::platform::clock::monotonic_ms());
    true
}

fn find_arrow_target(
    state: &mut super::CodeChunksState,
    session: &wordcraft_engine::Session,
    placed_ids: &HashSet<i64>,
    cur_block: usize,
    next_block: Option<usize>,
    dir: i32,
) -> Option<(i64, bool)> {
    let mounted: HashSet<i64> = state.widgets.keys().copied().collect();
    let refs = state.top_level_chunks(session);
    let check = |r: &wordcraft_doc::ChunkRef| -> Option<(i64, bool)> {
        let cid = r.chunk.id?;
        if !mounted.contains(&cid) || !placed_ids.contains(&cid) {
            return None;
        }
        let first = r.start.path.0.first().copied().unwrap_or(0) as usize;
        let last = r.end.path.0.first().copied().unwrap_or(0) as usize;
        let crosses = if dir > 0 {
            cur_block < first
                && (next_block.is_some_and(|nb| nb > last || (first..=last).contains(&nb)) || (next_block.is_none() && cur_block + 1 == first))
        } else {
            cur_block > last
                && (next_block.is_some_and(|nb| nb < first || (first..=last).contains(&nb)) || (next_block.is_none() && cur_block == last + 1))
        };
        crosses.then_some((cid, dir < 0))
    };
    if dir > 0 { refs.iter().find_map(check) } else { refs.iter().rev().find_map(check) }
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

    let placed_ids: HashSet<i64> = layout
        .pages
        .iter()
        .flat_map(|pg| pg.items.iter())
        .filter_map(|it| match it {
            Placed::Chunk { id, .. } => Some(*id),
            _ => None,
        })
        .collect();

    if let Some((cid, at_end)) = find_arrow_target(&mut app.canvas.code_chunks, &app.session, &placed_ids, cur_block, next_block, dir) {
        app.canvas.code_chunks.request_focus(cid, at_end);
        return true;
    }
    false
}

#[cfg(test)]
pub(crate) fn dispatch_chunk_action_for_test(app: &mut WordApp, chunk_id: i64, action: CodeChunkAction) {
    dispatch_chunk_action(app, chunk_id, action);
}

#[cfg(test)]
pub(crate) fn exit_chunk_to_canvas_for_test(app: &mut WordApp, id: i64, dir: i32) {
    exit_chunk_to_canvas(app, id, dir);
}
