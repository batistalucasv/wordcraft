//! The interactive code chunk: a rounded code container with a language badge, a Run button, an
//! options menu, a monospace editor with line numbers and light syntax colouring, and a
//! collapsible output drawer holding the console, a zoomable plot viewer and captured tables.
//!
//! The widget is UI only. It keeps its own state (language, source, output and the run in
//! flight) and tells the host what must reach the document through [`CodeChunkAction`].
//! Execution happens on a worker thread through `wordcraft-kernel`, so drawing never waits on the
//! interpreter: the result arrives on a channel while the drawer shows a spinner and a live
//! elapsed time.
//!
//! Hosts that run chunks through an engine command (`code.run_chunk`, issue #7 of
//! `batistalucasv/wordcraft-kernel`) attach that command's result with
//! [`CodeChunk::attach_run`] instead of pressing Run. The browser has no interpreter, so Run
//! explains that rather than starting a thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use egui::text::LayoutJob;
use egui::{Align, Align2, Color32, CornerRadius, FontId, Id, Key, Layout, Margin, RichText, Stroke, TextEdit, Ui, pos2, vec2};
use wordcraft_kernel::{CodeLanguage, ExecutionResult, PlotOutput, TabularOutput};

#[cfg(not(target_arch = "wasm32"))]
use wordcraft_kernel::{ExecutionRequest, PlotFormat};

use crate::theme::Tokens;

#[cfg(test)]
mod tests;

/// Longest source a chunk keeps, in characters. Pasted code is cut here rather than growing
/// without bound.
pub const MAX_CODE_CHARS: usize = 4 * 1024 * 1024;

/// Most of a stream the console shows, in bytes. The kernel already captured the whole output;
/// this only bounds what is laid out every frame.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// The editor's text size, in points.
const CODE_SIZE: f32 = 13.0;

/// The line-number gutter's text size, in points.
const LINE_NUMBER_SIZE: f32 = 10.5;

/// Zoom limits for the plot viewer.
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 4.0;

/// Largest side a plot texture may decode to, before the GPU's own limit.
const MAX_PLOT_SIDE: u32 = 8192;

/// Largest height the console scrolls in before the page takes over.
const CONSOLE_MAX_H: f32 = 180.0;

/// Largest height the plot scrolls in before the page takes over.
const PLOT_MAX_H: f32 = 280.0;

/// Largest height the table scrolls in before the page takes over.
const TABLE_MAX_H: f32 = 300.0;

/// Rows and columns laid out for one table. The kernel keeps up to 10,000 rows, so the drawer
/// previews a bounded slice; insertion still carries the whole table.
const TABLE_PREVIEW_ROWS: usize = 200;
const TABLE_PREVIEW_COLS: usize = 50;

/// Message shown when someone tries to run a chunk in the browser build, which has no
/// local R/Python interpreter.
const BROWSER_NO_INTERPRETER_MESSAGE: &str = "Code chunks run in the desktop app; the browser has no interpreter.";

/// One run's reply from the worker thread: the kernel's result, or why it could not run.
pub type RunResult = Result<ExecutionResult, wordcraft_kernel::KernelError>;

/// The output drawer's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputTab {
    /// `stdout`, `stderr`, the exit status and any runner error.
    Console,
    /// The captured plot, with zoom and an action to put it in the document.
    Plot,
    /// Tables passed to `wc_table()`, with an action to put the chosen one in the document.
    Table,
}

/// A finished run's output.
#[derive(Debug, Clone, Default)]
pub struct ChunkOutput {
    /// Captured standard output (cut at [`MAX_OUTPUT_BYTES`] for display).
    pub stdout: String,
    /// Captured standard error (cut at [`MAX_OUTPUT_BYTES`] for display).
    pub stderr: String,
    /// The process's exit code, when it had one.
    pub exit_code: Option<i32>,
    /// Whether the process exited cleanly.
    pub success: bool,
    /// Whether the run was stopped at its timeout.
    pub timed_out: bool,
    /// How long the run took.
    pub duration_ms: u64,
    /// PNG bytes of the captured plot, for the viewer and for insertion.
    pub plot_png: Option<Vec<u8>>,
    /// Vector SVG of the captured plot, for insertion at full quality.
    pub plot_svg: Option<String>,
    /// All captured plots in creation order.
    pub plots: Vec<PlotOutput>,
    /// Tables passed to `wc_table()`, in call order.
    pub tables: Vec<TabularOutput>,
    /// Why the chunk could not run at all (no interpreter, worker failure). When set, the other
    /// fields carry nothing.
    pub error: Option<String>,
}

impl ChunkOutput {
    /// A chunk that never ran, with `message` saying why.
    pub fn error(message: impl Into<String>) -> Self {
        Self { error: Some(message.into()), ..Self::default() }
    }

    /// A finished kernel run, with streams bounded to what the console shows.
    pub fn from_result(result: &ExecutionResult) -> Self {
        let plots = if !result.plots.is_empty() {
            result.plots.clone()
        } else if result.plot_png.is_some() || result.plot_svg.is_some() {
            vec![PlotOutput { png: result.plot_png.clone(), svg: result.plot_svg.clone() }]
        } else {
            Vec::new()
        };
        let plot_png = result.plot_png.clone().or_else(|| plots.first().and_then(|p| p.png.clone()));
        let plot_svg = result.plot_svg.clone().or_else(|| plots.first().and_then(|p| p.svg.clone()));
        Self {
            stdout: capped(&result.stdout),
            stderr: capped(&result.stderr),
            exit_code: result.exit_code,
            success: result.success,
            timed_out: result.timed_out,
            duration_ms: result.duration_ms,
            plot_png,
            plot_svg,
            plots,
            tables: result.tables.clone(),
            error: None,
        }
    }

    fn plot_count(&self) -> usize {
        if !self.plots.is_empty() { self.plots.len() } else { usize::from(self.plot_png.is_some() || self.plot_svg.is_some()) }
    }

    fn plot_at(&self, index: usize) -> Option<(Option<&[u8]>, Option<&str>)> {
        if !self.plots.is_empty() {
            self.plots.get(index).map(|p| (p.png.as_deref(), p.svg.as_deref()))
        } else if index == 0 && (self.plot_png.is_some() || self.plot_svg.is_some()) {
            Some((self.plot_png.as_deref(), self.plot_svg.as_deref()))
        } else {
            None
        }
    }

    /// Whether the run captured a plot in either format.
    pub fn has_plot(&self) -> bool {
        self.plot_count() > 0
    }

    /// Whether the run captured at least one table.
    pub fn has_tables(&self) -> bool {
        !self.tables.is_empty()
    }

    fn console_text(&self) -> Option<String> {
        if let Some(err) = &self.error {
            let trimmed = err.trim_end_matches(['\r', '\n']);
            return (!trimmed.trim().is_empty()).then(|| trimmed.to_owned());
        }
        let mut combined = self.stdout.trim_end_matches(['\r', '\n']).to_owned();
        let stderr = self.stderr.trim_end_matches(['\r', '\n']);
        if !stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(stderr);
        }
        (!combined.trim().is_empty()).then_some(combined)
    }
}

/// What the host must do to the document after interacting with the widget. The widget never
/// touches the document itself.
#[derive(Debug, Clone, PartialEq)]
pub enum CodeChunkAction {
    /// Add the captured plot to the document as a picture. Either format may be missing.
    InsertPlot {
        /// The high-resolution raster, if one was captured.
        png: Option<Vec<u8>>,
        /// The lossless vector, if one was captured.
        svg: Option<String>,
    },
    /// Insert a table at the document caret (or below the chunk).
    InsertTable {
        /// Column names in display order.
        headers: Vec<String>,
        /// Body rows, each with one cell per header.
        rows: Vec<Vec<String>>,
    },
    /// Insert console output into the document as code-formatted paragraphs at the caret (or below the chunk).
    InsertOutput {
        /// Console text lines to insert into the document.
        text: String,
    },
}

/// Output and launch source of an asynchronous widget-initiated run that finished this frame.
#[derive(Debug, Clone)]
pub struct CompletedRun {
    /// Language active when the run was launched.
    pub language: CodeLanguage,
    /// Source code captured when the run was launched.
    pub code: String,
    /// Console, plot, table, or error output produced by the run.
    pub output: ChunkOutput,
}

/// Outcome from drawing a code chunk mounted inside the document canvas.
#[derive(Debug, Default)]
pub struct EmbeddedFrame {
    /// Action requested from the output drawer (Insert Plot / Insert Table).
    pub action: Option<CodeChunkAction>,
    /// True if the source text or language changed this frame.
    pub dirty: bool,
    /// True if the inline editor currently holds keyboard focus.
    pub editor_has_focus: bool,
    /// True if the inline editor lost keyboard focus this frame.
    pub editor_lost_focus: bool,
    /// Up (`-1`) or Down (`1`) arrow pressed on the first or last source line to leave the chunk.
    pub arrow_exit: Option<i32>,
    /// Output and launch source of an asynchronous widget-initiated run that finished this frame.
    pub completed_run: Option<CompletedRun>,
    /// Screen position of a secondary click inside the chunk card, if any.
    pub context_click_pos: Option<egui::Pos2>,
    /// True if the user pressed Run or Ctrl+Enter inside the chunk this frame.
    pub run_requested: bool,
    /// True if the user clicked Clear output in the chunk header this frame.
    pub clear_requested: bool,
    /// New or cleared chunk label committed from the inline header editor this frame.
    pub committed_label: Option<String>,
}

/// Outcome of [`CodeChunk::header`] on a single frame.
#[derive(Debug, Default)]
struct HeaderOutcome {
    run: bool,
    clear: bool,
    committed_label: Option<String>,
}

/// Outcome of [`CodeChunk::editor`] on a single frame.
#[derive(Debug, Default)]
struct EditorOutcome {
    changed: bool,
    has_focus: bool,
    lost_focus: bool,
    arrow_exit: Option<i32>,
}

/// The interactive code chunk's state, drawn with [`CodeChunk::show`].
pub struct CodeChunk {
    /// A stable id for the editor and the popups of this instance.
    id: u64,
    language: CodeLanguage,
    /// Whether the interpreter for `language` was found when it was chosen.
    available: bool,
    code: String,
    /// Teaching Mode's opposite: show the raw source.
    pub show_code: bool,
    /// Whether the output drawer is open.
    pub output_open: bool,
    /// The drawer's current tab.
    pub tab: OutputTab,
    /// The plot viewer's scale; 1.0 shows one image pixel per device pixel.
    pub zoom: f32,
    /// Which captured plot the drawer shows when a run produced more than one.
    plot_index: usize,
    /// Which captured table the drawer shows when a run produced more than one.
    table_index: usize,
    output: Option<ChunkOutput>,
    /// The run in flight, when there is one.
    pending: Option<Receiver<RunResult>>,
    /// Monotonic start timestamp from [`crate::clock::now_ms`], in milliseconds.
    started_ms: Option<f64>,
    /// The plots' decoded textures, lazily filled by index.
    plot_textures: Vec<Option<egui::TextureHandle>>,
    /// Remembers failed PNG decodes per plot index so corrupt bytes are not retried every frame.
    plot_decode_failed: Vec<bool>,
    #[cfg(test)]
    plot_decode_attempts: usize,
    #[cfg(test)]
    force_browser_for_test: bool,
    /// Optional human-readable chunk label (`#| label: ...`).
    label: String,
    editing_label: bool,
    label_draft: String,
    /// Pending cursor placement when entering the editor (`Some(false)` = start, `Some(true)` = end).
    pending_cursor_edge: Option<bool>,
    /// Read-only view (e.g. Read Mode): blocks editing source, language, and run/insert actions.
    pub read_only: bool,
    /// Snapshot of `(language, code)` at the moment a widget-initiated run started.
    pending_source: Option<(CodeLanguage, String)>,
}

impl CodeChunk {
    /// A chunk in `language` holding `code`, with the source and the drawer closed until needed.
    pub fn new(language: CodeLanguage, code: impl Into<String>) -> Self {
        Self {
            id: next_id(),
            language,
            available: interpreter_available(language),
            code: truncate_code(code),
            show_code: true,
            output_open: false,
            tab: OutputTab::Console,
            zoom: 1.0,
            plot_index: 0,
            table_index: 0,
            output: None,
            pending: None,
            started_ms: None,
            plot_textures: Vec::new(),
            plot_decode_failed: Vec::new(),
            #[cfg(test)]
            plot_decode_attempts: 0,
            #[cfg(test)]
            force_browser_for_test: false,
            label: String::new(),
            editing_label: false,
            label_draft: String::new(),
            pending_cursor_edge: None,
            read_only: false,
            pending_source: None,
        }
    }

    /// Optional user-chosen label (`#| label: ...`).
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Update the chunk's displayed label without changing its code or output.
    pub fn set_label(&mut self, label: impl Into<String>) {
        self.label = clean_label(&label.into());
        if !self.editing_label {
            self.label_draft = self.label.clone();
        }
    }

    /// Whether the user is currently typing a new chunk name in the header.
    pub fn is_editing_label(&self) -> bool {
        self.editing_label
    }

    /// The chunk's language.
    pub fn language(&self) -> CodeLanguage {
        self.language
    }

    /// The chunk's source.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Replace the source, keeping the language and the last output.
    pub fn set_code(&mut self, code: impl Into<String>) {
        self.code = truncate_code(code);
    }

    /// Switch language. The last output belonged to the other interpreter, so it is dropped.
    /// Does nothing while a run is in flight so a second interpreter cannot run concurrently.
    pub fn set_language(&mut self, language: CodeLanguage) {
        if self.language == language || self.is_running() {
            return;
        }
        self.language = language;
        self.available = interpreter_available(language);
        self.pending_source = None;
        self.output = None;
        self.clear_plot_cache();
    }

    /// Whether the interpreter for the current language was found on `PATH`.
    pub fn available(&self) -> bool {
        self.available
    }

    /// Whether a run is in flight.
    pub fn is_running(&self) -> bool {
        self.pending.is_some()
    }

    /// The last finished output, if any.
    pub fn output(&self) -> Option<&ChunkOutput> {
        self.output.as_ref()
    }

    /// Show a finished output, as when a saved document restores its chunk: opens the drawer and
    /// picks the plot tab when there is a plot, then the table tab when there are tables.
    pub fn set_output(&mut self, output: ChunkOutput) {
        self.tab = if output.has_plot() {
            OutputTab::Plot
        } else if output.has_tables() {
            OutputTab::Table
        } else {
            OutputTab::Console
        };
        self.plot_index = 0;
        self.table_index = 0;
        self.output = Some(output);
        self.output_open = true;
        self.clear_plot_cache();
    }

    /// Run the chunk now on a worker thread. Does nothing while another run is in flight.
    pub fn run(&mut self, ctx: &egui::Context) {
        if self.read_only || self.is_running() {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = ctx;
            self.run_browser();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if !self.available {
                self.fail(format!(
                    "{} was not found on PATH. Install {} to run {} chunks.",
                    self.language.binary_name(),
                    language_name(self.language),
                    language_name(self.language)
                ));
                return;
            }
            let launched_source = (self.language, self.code.clone());
            let request = ExecutionRequest::new(self.language, launched_source.1.clone()).with_plot_format(PlotFormat::Both);
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new().name("wordcraft-code-chunk".to_owned()).spawn(move || {
                let _ = tx.send(wordcraft_kernel::execute(&request));
            });
            match spawned {
                Ok(_) => {
                    self.begin_pending(rx);
                    self.pending_source = Some(launched_source);
                    ctx.request_repaint_after(Duration::from_millis(50));
                }
                Err(e) => self.fail(format!("The code runner could not start: {e}")),
            }
        }
    }

    /// Show a run whose result will arrive on `rx`, from a host that executes chunks itself (an
    /// engine command). The widget then shows the spinner and applies the result exactly as Run
    /// does. Does nothing while another run is in flight.
    pub fn attach_run(&mut self, rx: Receiver<RunResult>) {
        if self.is_running() {
            return;
        }
        self.begin_pending(rx);
    }

    #[cfg(any(target_arch = "wasm32", test))]
    fn run_browser(&mut self) {
        if self.is_running() {
            return;
        }
        self.fail(BROWSER_NO_INTERPRETER_MESSAGE);
    }

    fn begin_pending(&mut self, rx: Receiver<RunResult>) {
        self.output = None;
        self.clear_plot_cache();
        self.output_open = true;
        self.tab = OutputTab::Console;
        self.pending = Some(rx);
        self.pending_source = None;
        self.started_ms = Some(craft::platform::clock::monotonic_ms());
    }

    fn clear_plot_cache(&mut self) {
        self.plot_textures.clear();
        self.plot_decode_failed.clear();
    }

    /// Clear any displayed output, plot texture, or in-flight status.
    pub fn clear_output(&mut self) {
        self.output = None;
        self.clear_plot_cache();
        self.pending = None;
        self.pending_source = None;
        self.started_ms = None;
    }

    /// Settle a host-owned attached run with `output`, releasing its in-flight state even when the
    /// widget is currently offscreen and not being painted.
    pub fn finish_attached_run(&mut self, output: ChunkOutput) {
        self.pending = None;
        self.pending_source = None;
        self.started_ms = None;
        if output.error.is_some() {
            self.apply_failure(output);
        } else {
            self.set_output(output);
        }
    }

    /// Show the code editor if hidden, focus it next frame, and place its cursor at the start or end.
    pub fn focus_edge(&mut self, at_end: bool) {
        if self.read_only {
            return;
        }
        self.show_code = true;
        self.pending_cursor_edge = Some(at_end);
    }

    /// Draw the chunk, returning the action the host must take, if any.
    pub fn show(&mut self, ui: &mut Ui) -> Option<CodeChunkAction> {
        let frame = self.show_embedded(ui);
        if frame.clear_requested {
            self.clear_output();
        }
        if frame.run_requested {
            self.run(ui.ctx());
        }
        frame.action
    }

    /// Draw the chunk mounted inside the document canvas and report UI, focus, and edit status.
    pub fn show_embedded(&mut self, ui: &mut Ui) -> EmbeddedFrame {
        let prev_lang = self.language;
        let completed_run = self.poll(ui.ctx());
        let t = Tokens::get(ui.ctx());
        let editor = self.editor_id();
        let shortcut_run = !self.read_only && shortcut_pressed(ui, editor);
        let focus = if ui.memory(|m| m.has_focus(editor)) { t.accent } else { t.border_strong };
        let width = ui.available_width();
        let mut action = None;
        let mut header_outcome = HeaderOutcome::default();
        let mut editor_outcome = EditorOutcome::default();
        let frame_resp = egui::Frame::NONE.fill(t.panel).stroke(Stroke::new(1.0, focus)).corner_radius(CornerRadius::same(8)).show(ui, |ui| {
            if width.is_finite() {
                ui.set_width((width - 2.0).max(0.0));
            }
            header_outcome = self.header(ui, &t);
            if self.show_code {
                editor_outcome = self.editor(ui, &t);
            } else {
                ui.add_space(2.0);
            }
            ui.separator();
            egui::Frame::NONE.inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
                self.drawer(ui, &t, &mut action);
            });
        });
        let context_click_pos = ui.input(|i| {
            if i.pointer.secondary_clicked() { i.pointer.interact_pos().filter(|pos| frame_resp.response.rect.contains(*pos)) } else { None }
        });
        EmbeddedFrame {
            action,
            dirty: !self.read_only && (editor_outcome.changed || self.language != prev_lang),
            editor_has_focus: editor_outcome.has_focus,
            editor_lost_focus: editor_outcome.lost_focus,
            arrow_exit: editor_outcome.arrow_exit,
            completed_run,
            context_click_pos,
            run_requested: !self.read_only && (shortcut_run || header_outcome.run),
            clear_requested: !self.read_only && header_outcome.clear,
            committed_label: header_outcome.committed_label,
        }
    }

    /// The id of the source editor, for focus checks and tests.
    pub fn editor_id(&self) -> Id {
        Id::unique(("wordcraft-code-chunk-editor", self.id))
    }

    /// Apply a result that arrived from a host or a worker thread. Returns the widget-initiated
    /// output and launch source when one finishes.
    fn poll(&mut self, ctx: &egui::Context) -> Option<CompletedRun> {
        let received = match self.pending.as_ref()?.try_recv() {
            Ok(result) => Some(Some(result)),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(None),
        };
        match received {
            None => {
                ctx.request_repaint_after(Duration::from_millis(50));
                None
            }
            Some(Some(result)) => {
                self.pending = None;
                self.started_ms = None;
                let source = self.pending_source.take();
                self.clear_plot_cache();
                let out = match result {
                    Ok(result) => {
                        let out = ChunkOutput::from_result(&result);
                        self.set_output(out.clone());
                        out
                    }
                    Err(e) => {
                        let out = ChunkOutput::error(e.to_string());
                        self.apply_failure(out.clone());
                        out
                    }
                };
                source.map(|(language, code)| CompletedRun { language, code, output: out })
            }
            Some(None) => {
                self.pending = None;
                self.started_ms = None;
                let source = self.pending_source.take();
                let out = ChunkOutput::error("The code runner stopped before it reported a result.");
                self.apply_failure(out.clone());
                source.map(|(language, code)| CompletedRun { language, code, output: out })
            }
        }
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.apply_failure(ChunkOutput::error(message));
    }

    fn apply_failure(&mut self, output: ChunkOutput) {
        self.output = Some(output);
        self.output_open = true;
        self.tab = OutputTab::Console;
        self.clear_plot_cache();
    }

    fn header(&mut self, ui: &mut Ui, t: &Tokens) -> HeaderOutcome {
        let current = self.language;
        let mut picked = None;
        let mut toggle_code = false;
        let mut clear = false;
        let mut copy = false;
        let mut run = false;
        let header_label = egui::Frame::NONE
            .fill(t.input)
            .corner_radius(CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 })
            .inner_margin(Margin::symmetric(8, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let badge = ui.add_enabled(
                        !self.read_only && !self.is_running(),
                        egui::Button::new(RichText::new(format!("[ {} ]", language_name(current))).monospace().color(t.on_accent))
                            .fill(t.accent)
                            .corner_radius(CornerRadius::same(4))
                            .min_size(vec2(0.0, 21.0)),
                    );
                    if !self.read_only {
                        badge.clone().on_hover_text(crate::tl!("Choose the chunk's language"));
                        if !self.is_running() {
                            egui::Popup::menu(&badge).show(|ui| {
                                for language in [CodeLanguage::R, CodeLanguage::Python] {
                                    if ui.selectable_label(current == language, language_name(language)).clicked() {
                                        picked = Some(language);
                                        ui.close();
                                    }
                                }
                            });
                        }
                    }

                    if disclosure_triangle(ui, self.show_code, t.text_dim, crate::tl!("Toggle code")).clicked() {
                        toggle_code = true;
                    }
                    let committed_label = self.header_title(ui, t);

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let options = ui.menu_button(RichText::new(crate::tl!("Options")).color(t.text_dim).size(11.5), |ui| {
                            if ui.add_enabled(!self.read_only, egui::Button::new(crate::tl!("Rename chunk"))).clicked() {
                                self.label_draft = self.label.clone();
                                self.editing_label = true;
                                ui.memory_mut(|m| m.request_focus(self.label_editor_id()));
                                ui.close();
                            }
                            if ui.button(crate::tl!("Copy code")).clicked() {
                                copy = true;
                                ui.close();
                            }
                            if ui.button(if self.show_code { crate::tl!("Hide code (Teaching Mode)") } else { crate::tl!("Show code") }).clicked() {
                                toggle_code = true;
                                ui.close();
                            }
                            if ui
                                .add_enabled(
                                    !self.read_only && self.output.is_some() && !self.is_running(),
                                    egui::Button::new(crate::tl!("Clear output")),
                                )
                                .clicked()
                            {
                                clear = true;
                                ui.close();
                            }
                        });
                        options.response.on_hover_text(crate::tl!("Chunk options"));

                        let run_button = ui.add_enabled(
                            !self.read_only && !self.is_running() && self.available,
                            egui::Button::new(RichText::new(crate::tl!("Run")).color(t.on_accent))
                                .fill(t.accent)
                                .corner_radius(CornerRadius::same(4)),
                        );
                        if run_button.clicked() {
                            run = true;
                        }
                        if !self.available {
                            run_button.on_disabled_hover_text(unavailable_run_message(current, self.is_browser()));
                        } else if !self.read_only {
                            run_button.on_hover_text(crate::tl!("Run the chunk (Ctrl+Enter)"));
                        }

                        if let Some(status) = self.status() {
                            let (text, kind) = status;
                            let color = match kind {
                                StatusKind::Ok => t.green,
                                StatusKind::Warn => t.orange,
                                StatusKind::Fail => t.red,
                            };
                            ui.label(RichText::new(text).color(color).size(11.5));
                        } else if self.is_running() {
                            ui.add(egui::Spinner::new().size(14.0));
                            let secs = self.started_ms.map_or(0.0, |started| {
                                let elapsed = (craft::platform::clock::monotonic_ms() - started) / 1000.0;
                                if elapsed.is_finite() { elapsed.max(0.0) as f32 } else { 0.0 }
                            });
                            ui.label(RichText::new(format!("{} {secs:.1} s", crate::tl!("Running…"))).color(t.text_dim).size(11.5));
                            ui.ctx().request_repaint_after(Duration::from_millis(100));
                        }
                    });
                    committed_label
                })
                .inner
            })
            .inner;
        if !self.read_only
            && let Some(language) = picked
        {
            self.set_language(language);
        }
        if toggle_code {
            self.show_code = !self.show_code;
        }
        if copy {
            ui.ctx().copy_text(self.code.clone());
        }
        HeaderOutcome {
            run: !self.read_only && !self.is_running() && run,
            clear: !self.read_only && !self.is_running() && clear,
            committed_label: header_label,
        }
    }

    fn label_editor_id(&self) -> Id {
        Id::unique(("wordcraft-code-chunk-label", self.id))
    }

    fn header_title(&mut self, ui: &mut Ui, t: &Tokens) -> Option<String> {
        if self.read_only {
            self.editing_label = false;
        }
        if self.editing_label {
            let editor_id = self.label_editor_id();
            let resp = ui.add(
                TextEdit::singleline(&mut self.label_draft).id(editor_id).desired_width(160.0).char_limit(128).hint_text(crate::tl!("Chunk name")),
            );
            let cancel = ui.input(|i| i.key_pressed(Key::Escape));
            let commit = ui.input(|i| i.key_pressed(Key::Enter)) || resp.lost_focus();
            if cancel {
                self.editing_label = false;
                self.label_draft = self.label.clone();
                return None;
            }
            if commit {
                self.editing_label = false;
                let cleaned = clean_label(&self.label_draft);
                if cleaned != self.label {
                    self.label = cleaned.clone();
                    self.label_draft = cleaned.clone();
                    return Some(cleaned);
                }
                self.label_draft = self.label.clone();
            }
            return None;
        }

        let display = if self.label.is_empty() { crate::tl!("Code chunk") } else { self.label.as_str() };
        let color = if self.label.is_empty() { t.text_dim } else { t.text };
        let resp = ui.add_enabled(!self.read_only, egui::Button::new(RichText::new(display).color(color)).frame(false));
        if resp.clicked() {
            self.label_draft = self.label.clone();
            self.editing_label = true;
            ui.memory_mut(|m| m.request_focus(self.label_editor_id()));
        } else if !self.read_only {
            resp.on_hover_text(crate::tl!("Click to name this chunk"));
        }
        None
    }

    fn editor(&mut self, ui: &mut Ui, t: &Tokens) -> EditorOutcome {
        let editor_id = self.editor_id();
        let mut arrow_exit = None;
        if self.read_only {
            self.pending_cursor_edge = None;
        } else if let Some(at_end) = self.pending_cursor_edge.take() {
            ui.memory_mut(|m| m.request_focus(editor_id));
            let mut state = TextEdit::load_state(ui.ctx(), editor_id).unwrap_or_default();
            let idx = if at_end { self.code.chars().count() } else { 0 };
            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(idx))));
            state.store(ui.ctx(), editor_id);
        } else if ui.memory(|m| m.has_focus(editor_id))
            && let Some(range) = TextEdit::load_state(ui.ctx(), editor_id).and_then(|s| s.cursor.char_range())
            && range.primary == range.secondary
        {
            let b = self.code.char_indices().nth(range.primary.index.into()).map_or(self.code.len(), |(i, _)| i);
            let on_first_line = self.code.get(..b).is_some_and(|s| !s.contains('\n'));
            let on_last_line = self.code.get(b..).is_some_and(|s| !s.contains('\n'));
            if on_first_line && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowUp)) {
                arrow_exit = Some(-1);
                ui.memory_mut(|m| m.surrender_focus(editor_id));
            } else if on_last_line && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowDown)) {
                arrow_exit = Some(1);
                ui.memory_mut(|m| m.surrender_focus(editor_id));
            }
        }

        let font = FontId::monospace(CODE_SIZE);
        let code_font = font.clone();
        let language = self.language;
        let colours = CodeColours::new(t);
        let mut layouter = move |ui: &Ui, buffer: &dyn egui::TextBuffer, _wrap: f32| {
            let mut job = LayoutJob::default();
            // Code reads better unwrapped: the editor scrolls sideways instead.
            job.wrap.max_width = f32::INFINITY;
            job.keep_trailing_whitespace = true;
            for token in highlight(buffer.as_str(), language) {
                let format = egui::TextFormat { font_id: code_font.clone(), color: colours.of(token.kind), ..Default::default() };
                if token.text.contains('\u{FFFC}') {
                    // Display-only replacement (both are 1 char / 3 UTF-8 bytes) avoids missing-glyph panics while keeping U+FFFC in `self.code`.
                    let display = token.text.replace('\u{FFFC}', "\u{FFFD}");
                    job.append(&display, 0.0, format);
                } else {
                    job.append(token.text, 0.0, format);
                }
            }
            ui.fonts_mut(|fonts| fonts.layout_job(job))
        };

        let lines = self.code.matches('\n').count() + 1;
        let char_w = ui.fonts_mut(|fonts| fonts.glyph_width(&font, '0')).max(1.0);
        let line_h = ui.fonts_mut(|fonts| fonts.row_height(&font)).max(1.0);
        let columns = self.code.split('\n').map(|line| line.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum::<usize>()).max().unwrap_or(1);
        let text_w = columns as f32 * char_w + 2.0 * char_w;
        let digits = lines.to_string().len().max(2);
        let gutter = digits as f32 * char_w + 14.0;

        let mut outcome = EditorOutcome { arrow_exit, ..Default::default() };
        let read_only = self.read_only;
        egui::Frame::NONE.inner_margin(Margin::symmetric(0, 6)).show(ui, |ui| {
            egui::ScrollArea::horizontal().id_salt(("wordcraft-code-chunk-scroll", self.id)).auto_shrink([false, true]).show(ui, |ui| {
                // The child's width is the source's, so long lines scroll instead of being cut.
                let size = vec2(gutter + text_w, lines as f32 * line_h + 4.0);
                ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.add_space(gutter);
                    let mut show_box = |ui: &mut Ui, buffer: &mut dyn egui::TextBuffer| {
                        TextEdit::multiline(buffer)
                            .id(editor_id)
                            .code_editor()
                            .frame(egui::Frame::NONE)
                            .margin(Margin::ZERO)
                            .desired_width(text_w)
                            .desired_rows(lines)
                            .hint_text(crate::tl!("Code"))
                            .char_limit(MAX_CODE_CHARS)
                            .layouter(&mut layouter)
                            .show(ui)
                    };
                    let out = if read_only {
                        let mut ro = self.code.as_str();
                        show_box(ui, &mut ro)
                    } else {
                        let out = show_box(ui, &mut self.code);
                        if self.code.contains('\r') {
                            self.code = self.code.replace("\r\n", "\n").replace('\r', "");
                        }
                        out
                    };
                    outcome.changed = !read_only && out.response.changed();
                    outcome.has_focus = out.response.has_focus();
                    outcome.lost_focus = out.response.lost_focus();
                    let top = out.galley_pos;
                    let painter = ui.painter();
                    let height = out.galley.rect.height();
                    painter.vline(top.x - gutter + 6.0, egui::Rangef::new(top.y, top.y + height), Stroke::new(1.0, t.border));
                    let number_font = FontId::monospace(LINE_NUMBER_SIZE);
                    let mut line = 1;
                    let mut first_row = true;
                    for row in &out.galley.rows {
                        if first_row {
                            let y = top.y + row.pos.y + row.rect().height() * 0.5;
                            painter.text(pos2(top.x - 8.0, y), Align2::RIGHT_CENTER, line.to_string(), number_font.clone(), t.text_disabled);
                            line += 1;
                        }
                        first_row = row.ends_with_newline;
                    }
                });
            });
        });
        outcome
    }

    fn drawer(&mut self, ui: &mut Ui, t: &Tokens, action: &mut Option<CodeChunkAction>) {
        let mut toggle = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if disclosure_triangle(ui, self.output_open, t.text_dim, crate::tl!("Toggle output")).clicked() {
                toggle = true;
            }
            if ui.add(egui::Button::new(RichText::new(crate::tl!("Output")).color(t.text).size(12.0)).frame(false)).clicked() {
                toggle = true;
            }
        });
        if toggle {
            self.output_open = !self.output_open;
        }
        if !self.output_open {
            return;
        }

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, OutputTab::Console, crate::tl!("Console"));
            let has_plot = self.output.as_ref().is_some_and(ChunkOutput::has_plot);
            ui.add_enabled_ui(has_plot, |ui| {
                ui.selectable_value(&mut self.tab, OutputTab::Plot, crate::tl!("Plot"));
            });
            let has_tables = self.output.as_ref().is_some_and(ChunkOutput::has_tables);
            ui.add_enabled_ui(has_tables, |ui| {
                ui.selectable_value(&mut self.tab, OutputTab::Table, crate::tl!("Table"));
            });
        });
        ui.add_space(2.0);
        match self.tab {
            OutputTab::Console => self.console(ui, t, action),
            OutputTab::Plot => self.plot(ui, t, action),
            OutputTab::Table => self.table(ui, t, action),
        }
    }

    fn console(&self, ui: &mut Ui, t: &Tokens, action: &mut Option<CodeChunkAction>) {
        let Some(output) = self.output.as_ref() else {
            ui.label(RichText::new(crate::tl!("Run the chunk to see its output.")).color(t.text_dim).size(12.0));
            return;
        };
        if let Some(insert_text) = output.console_text() {
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let response = ui.add_enabled(!self.read_only, egui::Button::new(crate::tl!("Insert into document")));
                    if response.clicked() {
                        *action = Some(CodeChunkAction::InsertOutput { text: insert_text });
                    }
                    response.on_hover_text(crate::tl!("Insert console output into the document as code text"));
                });
            });
        }
        if let Some(error) = &output.error {
            ui.label(RichText::new(error).color(t.red).size(12.0));
            return;
        }
        if output.timed_out {
            ui.label(RichText::new(format!("{} {} ms.", crate::tl!("Stopped after its timeout:"), output.duration_ms)).color(t.orange).size(12.0));
        }
        if output.stdout.is_empty() && output.stderr.is_empty() {
            let text = if output.success { crate::tl!("No output") } else { crate::tl!("No output, but the exit code was not zero.") };
            ui.label(RichText::new(text).color(t.text_dim).size(12.0));
        }
        egui::ScrollArea::vertical().id_salt(("wordcraft-code-chunk-console", self.id)).max_height(CONSOLE_MAX_H).auto_shrink([false, true]).show(
            ui,
            |ui| {
                for (title, text, color) in [("stdout", &output.stdout, t.text), ("stderr", &output.stderr, t.red)] {
                    if text.is_empty() {
                        continue;
                    }
                    ui.label(RichText::new(title).color(t.text_dim).size(10.5).monospace());
                    egui::Frame::NONE.fill(t.input).corner_radius(CornerRadius::same(4)).inner_margin(Margin::symmetric(8, 6)).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(text).monospace().size(CODE_SIZE - 1.0).color(color)).wrap());
                    });
                    ui.add_space(2.0);
                }
            },
        );
    }

    fn plot(&mut self, ui: &mut Ui, t: &Tokens, action: &mut Option<CodeChunkAction>) {
        let Some(count) = self.output.as_ref().map(ChunkOutput::plot_count).filter(|&n| n > 0) else {
            let msg = if self.output.is_none() { crate::tl!("Run the chunk to see its plot.") } else { crate::tl!("This run captured no plot.") };
            ui.label(RichText::new(msg).color(t.text_dim).size(12.0));
            return;
        };
        let mut selected = self.plot_index.min(count - 1);
        if count > 1 {
            ui.horizontal_wrapped(|ui| {
                for index in 0..count {
                    let label = format!("{} {}", crate::tl!("Plot"), index + 1);
                    if ui.selectable_label(selected == index, label).clicked() {
                        selected = index;
                    }
                }
            });
        }
        self.plot_index = selected;

        if self.plot_textures.len() < count {
            self.plot_textures.resize_with(count, || None);
            self.plot_decode_failed.resize(count, false);
        }
        let has_png = {
            let Some(output) = self.output.as_ref() else { return };
            let Some((png, _)) = output.plot_at(selected) else { return };
            if self.plot_textures[selected].is_none()
                && !self.plot_decode_failed[selected]
                && let Some(png) = png
            {
                #[cfg(test)]
                {
                    self.plot_decode_attempts += 1;
                }
                self.plot_textures[selected] = decode_plot(ui.ctx(), png, self.id, selected);
                if self.plot_textures[selected].is_none() {
                    self.plot_decode_failed[selected] = true;
                }
            }
            png.is_some()
        };

        let texture = self.plot_textures[selected].clone();
        let mut insert = false;
        ui.horizontal(|ui| {
            let can_zoom = texture.is_some();
            if ui.add_enabled(can_zoom && self.zoom > MIN_ZOOM, egui::Button::new("−").min_size(vec2(22.0, 0.0))).clicked() {
                self.zoom = (self.zoom / 1.25).clamp(MIN_ZOOM, MAX_ZOOM);
            }
            ui.label(RichText::new(format!("{:.0}%", self.zoom * 100.0)).monospace().size(11.0));
            if ui.add_enabled(can_zoom && self.zoom < MAX_ZOOM, egui::Button::new("+").min_size(vec2(22.0, 0.0))).clicked() {
                self.zoom = (self.zoom * 1.25).clamp(MIN_ZOOM, MAX_ZOOM);
            }
            if ui.add_enabled(can_zoom, egui::Button::new(crate::tl!("Fit"))).clicked()
                && let Some(tex) = &texture
            {
                let natural = tex.size_vec2() / ui.ctx().pixels_per_point();
                let room = (ui.available_width() - 24.0).max(1.0);
                self.zoom = (room / natural.x.max(1.0)).clamp(MIN_ZOOM, MAX_ZOOM);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let response = ui.add_enabled(!self.read_only && has_png, egui::Button::new(crate::tl!("Insert into document")));
                if response.clicked() {
                    insert = true;
                }
                let tip = if !has_png {
                    crate::tl!("Inline insertion requires a PNG plot; rerun with PNG or both formats.")
                } else {
                    crate::tl!("Add the plot to the document as a picture")
                };
                response.on_hover_text(tip);
            });
        });

        if let Some(tex) = &texture {
            let natural = tex.size_vec2() / ui.ctx().pixels_per_point();
            let size = natural * self.zoom;
            egui::ScrollArea::both()
                .id_salt(("wordcraft-code-chunk-plot", self.id, selected))
                .max_height(PLOT_MAX_H)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::Frame::NONE.fill(t.input).inner_margin(Margin::same(4)).show(ui, |ui| {
                        ui.add(egui::Image::new(tex).fit_to_exact_size(size));
                    });
                });
        } else {
            let message = if !has_png {
                crate::tl!("The plot is vector-only (SVG). Inline insertion requires a PNG; rerun the chunk with PNG or both plot formats.")
            } else {
                crate::tl!("This plot could not be decoded for preview.")
            };
            ui.label(RichText::new(message).color(t.text_dim).size(12.0));
        }

        if insert && let Some((png, svg)) = self.output.as_ref().and_then(|o| o.plot_at(selected)) {
            *action = Some(CodeChunkAction::InsertPlot { png: png.map(ToOwned::to_owned), svg: svg.map(ToOwned::to_owned) });
        }
    }

    fn table(&mut self, ui: &mut Ui, t: &Tokens, action: &mut Option<CodeChunkAction>) {
        let Some(output) = self.output.as_ref() else {
            ui.label(RichText::new(crate::tl!("Run the chunk to see its table.")).color(t.text_dim).size(12.0));
            return;
        };
        if !output.has_tables() {
            ui.label(RichText::new(crate::tl!("This run captured no table.")).color(t.text_dim).size(12.0));
            return;
        }

        let count = output.tables.len();
        let mut selected = self.table_index.min(count - 1);
        if count > 1 {
            ui.horizontal_wrapped(|ui| {
                for index in 0..count {
                    let label = format!("{} {}", crate::tl!("Table"), index + 1);
                    if ui.selectable_label(selected == index, label).clicked() {
                        selected = index;
                    }
                }
            });
        }
        self.table_index = selected;

        let Some(output) = self.output.as_ref() else { return };
        let Some(table) = output.tables.get(self.table_index) else { return };
        let preview_rows = table.rows.len().min(TABLE_PREVIEW_ROWS);
        let preview_cols = table.headers.len().min(TABLE_PREVIEW_COLS);
        if preview_cols == 0 {
            ui.label(RichText::new(crate::tl!("This run captured no table.")).color(t.text_dim).size(12.0));
            return;
        }
        let mut insert = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} × {}", table.rows.len(), table.headers.len())).color(t.text_dim).size(11.0))
                .on_hover_text(crate::tl!("Rows × columns"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let response = ui.add_enabled(!self.read_only, egui::Button::new(crate::tl!("Insert into document")));
                if response.clicked() {
                    insert = true;
                }
                response.on_hover_text(crate::tl!("Add the table to the document as a Word table"));
            });
        });

        // A column opens at the width of its longest previewed cell, so numeric columns stay
        // readable without measuring every one of the kernel's rows each frame. Only the first 32
        // characters can affect the width, since the result is capped below that.
        let widths: Vec<f32> = (0..preview_cols)
            .map(|column| {
                let header = table.headers.get(column).map_or(0, |cell| cell.chars().take(32).count());
                let body = table
                    .rows
                    .iter()
                    .take(preview_rows)
                    .map(|row| row.get(column).map_or(0, |cell| cell.chars().take(32).count()))
                    .max()
                    .unwrap_or(0);
                (header.max(body) as f32 * 7.0).clamp(48.0, 240.0)
            })
            .collect();
        // A wide frame gets a horizontal scrollbar rather than clipped columns that cannot be
        // reached. The table keeps its own vertical scrollbar inside.
        let scrollbar = ui.spacing().scroll.bar_width + ui.spacing().scroll.bar_inner_margin + ui.spacing().scroll.bar_outer_margin;
        let total_width = widths.iter().sum::<f32>() + ui.spacing().item_spacing.x * widths.len() as f32 + scrollbar;
        egui::ScrollArea::horizontal().id_salt(("wordcraft-code-chunk-table-scroll", self.id, self.table_index)).auto_shrink([false, true]).show(
            ui,
            |ui| {
                ui.set_min_width(total_width);
                let mut builder = egui_extras::TableBuilder::new(ui)
                    .id_salt(("wordcraft-code-chunk-table", self.id, self.table_index))
                    .striped(true)
                    .resizable(true)
                    .auto_shrink([false, true])
                    .max_scroll_height(TABLE_MAX_H)
                    .cell_layout(Layout::left_to_right(Align::Center));
                for width in &widths {
                    builder = builder.column(egui_extras::Column::initial(*width).at_least(40.0).clip(true));
                }
                builder
                    .header(20.0, |mut header| {
                        for column in 0..preview_cols {
                            header.col(|ui| {
                                let text = table.headers.get(column).map_or("", String::as_str);
                                ui.add(egui::Label::new(RichText::new(text).strong().size(11.5).color(t.text)));
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(18.0, preview_rows, |mut row| {
                            let cells = &table.rows[row.index()];
                            for column in 0..preview_cols {
                                row.col(|ui| {
                                    let text = cells.get(column).map_or("", String::as_str);
                                    let label = egui::Label::new(RichText::new(text).monospace().size(11.0).color(t.text));
                                    if is_numeric(text) {
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            ui.add(label);
                                        });
                                    } else {
                                        ui.add(label);
                                    }
                                });
                            }
                        });
                    });
            },
        );
        if preview_rows < table.rows.len() || preview_cols < table.headers.len() {
            ui.label(
                RichText::new(format!(
                    "{} {} × {}. {}",
                    crate::tl!("Showing the first"),
                    preview_rows,
                    preview_cols,
                    crate::tl!("Inserting keeps the whole table.")
                ))
                .color(t.text_dim)
                .size(11.0),
            );
        }
        if insert {
            *action = Some(CodeChunkAction::InsertTable { headers: table.headers.clone(), rows: table.rows.clone() });
        }
    }

    fn status(&self) -> Option<(String, StatusKind)> {
        if self.is_running() {
            return None;
        }
        let output = self.output.as_ref()?;
        if output.error.is_some() {
            return Some((crate::tl!("Did not run").to_owned(), StatusKind::Fail));
        }
        if output.timed_out {
            return Some((crate::tl!("Timed out").to_owned(), StatusKind::Warn));
        }
        let secs = output.duration_ms as f64 / 1000.0;
        if output.success {
            Some((format!("OK {secs:.2} s"), StatusKind::Ok))
        } else {
            let code = output.exit_code.map_or_else(|| "—".to_owned(), |code| code.to_string());
            Some((format!("× {} {code}", crate::tl!("Exit")), StatusKind::Fail))
        }
    }

    fn is_browser(&self) -> bool {
        #[cfg(test)]
        if self.force_browser_for_test {
            return true;
        }
        cfg!(target_arch = "wasm32")
    }
}

fn unavailable_run_message(language: CodeLanguage, browser: bool) -> String {
    if browser {
        crate::tl!(BROWSER_NO_INTERPRETER_MESSAGE).to_owned()
    } else {
        format!("{} was not found on PATH. Install {} to run this chunk.", language.binary_name(), language_name(language))
    }
}

/// A one-word summary for the header and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    Ok,
    Warn,
    Fail,
}

/// Whether a cell reads as a number, so the table lines it up on the right.
fn is_numeric(text: &str) -> bool {
    !text.is_empty() && text.parse::<f64>().is_ok()
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Ctrl+Enter on the platforms WordCraft runs on (`Command` is Cmd on macOS). Shift and Alt make
/// it a different shortcut (Ctrl+Shift+Enter runs every chunk), so they must not be accepted.
///
/// Ctrl+Enter is the page break everywhere else in a document (see `docs/typing-parity.md`), so
/// the host must offer it to the chunk editor before the page-break command while this editor has
/// focus; [`shortcut_pressed`] consumes it only then.
fn shortcut_pressed(ui: &Ui, editor: Id) -> bool {
    if !ui.memory(|memory| memory.has_focus(editor)) {
        return false;
    }
    ui.input_mut(|input| {
        let mut taken = false;
        input.events.retain(|event| {
            let matches = matches!(
                event,
                egui::Event::Key { key: Key::Enter, pressed: true, repeat: false, modifiers, .. }
                    if modifiers.command && !modifiers.shift && !modifiers.alt
            );
            if matches && !taken {
                taken = true;
                false
            } else {
                true
            }
        });
        if taken {
            // On X11/winit, Ctrl+Enter can also emit a control-character text event (such as "\r"
            // or "\n") in the same frame; drop it so `TextEdit` does not insert a stray glyph or
            // mark the chunk dirty.
            input
                .events
                .retain(|event| !matches!(event, egui::Event::Text(text) if !text.is_empty() && text.chars().all(|c| c == '\r' || c == '\n')));
        }
        taken
    })
}

fn interpreter_available(language: CodeLanguage) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = language;
        false
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        wordcraft_kernel::is_language_available(language)
    }
}

/// Paints a crisp vector disclosure triangle (pointing down when `open`, right when collapsed)
/// without depending on Unicode triangle glyphs in UI fonts.
fn disclosure_triangle(ui: &mut Ui, open: bool, color: Color32, label: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(14.0, 16.0), egui::Sense::click());
    let response = response.on_hover_text(label);
    if ui.is_rect_visible(rect) {
        let c = rect.center();
        let r = 3.5;
        let pts = if open {
            vec![pos2(c.x - r, c.y - r * 0.6), pos2(c.x + r, c.y - r * 0.6), pos2(c.x, c.y + r * 0.8)]
        } else {
            vec![pos2(c.x - r * 0.6, c.y - r), pos2(c.x + r * 0.8, c.y), pos2(c.x - r * 0.6, c.y + r)]
        };
        ui.painter().add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
    }
    response
}

fn language_name(language: CodeLanguage) -> &'static str {
    match language {
        CodeLanguage::R => "R",
        CodeLanguage::Python => "Python",
    }
}

/// Keep at most [`MAX_CODE_CHARS`] characters, cutting on a UTF-8 boundary.
fn truncate_code(code: impl Into<String>) -> String {
    let mut code = code.into();
    if let Some((end, _)) = code.char_indices().nth(MAX_CODE_CHARS) {
        code.truncate(end);
    }
    code
}

/// Cut a stream to what the console lays out, at a character boundary.
fn capped(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_owned();
    }
    let end = text.floor_char_boundary(MAX_OUTPUT_BYTES);
    let mut out = text.get(..end).unwrap_or_default().to_owned();
    out.push_str("\n… output cut after 64 KiB");
    out
}

fn clean_label(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(128).collect()
}

fn decode_plot(ctx: &egui::Context, png: &[u8], id: u64, index: usize) -> Option<egui::TextureHandle> {
    let gpu_side = ctx.input(|input| input.max_texture_side).min(MAX_PLOT_SIDE as usize).max(1);
    let side = u32::try_from(gpu_side).unwrap_or(MAX_PLOT_SIDE);
    let image = wordcraft_geom::picture::decode_for_drawing(png, wordcraft_geom::MAX_DECODED_BYTES, side).ok()?;
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    if size[0] == 0 || size[1] == 0 {
        return None;
    }
    let pixels = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    Some(ctx.load_texture(format!("wordcraft-code-chunk-plot-{id}-{index}"), pixels, egui::TextureOptions::LINEAR))
}

#[derive(Debug, Clone, Copy)]
struct CodeColours {
    text: Color32,
    keyword: Color32,
    builtin: Color32,
    function: Color32,
    string: Color32,
    number: Color32,
    comment: Color32,
}

impl CodeColours {
    fn new(t: &Tokens) -> Self {
        Self { text: t.text, keyword: t.accent, builtin: t.accent, function: t.blue, string: t.green, number: t.orange, comment: t.text_dim }
    }

    fn of(self, kind: TokenKind) -> Color32 {
        match kind {
            TokenKind::Plain => self.text,
            TokenKind::Keyword => self.keyword,
            TokenKind::Builtin => self.builtin,
            TokenKind::Function => self.function,
            TokenKind::String => self.string,
            TokenKind::Number => self.number,
            TokenKind::Comment => self.comment,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Plain,
    Keyword,
    Builtin,
    Function,
    String,
    Number,
    Comment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Token<'a> {
    text: &'a str,
    kind: TokenKind,
}

/// Split code into coloured spans that together are the whole input. Single-line comments,
/// strings (including Python's triple-quoted ones), numbers, keywords, common builtins and calls
/// are marked; everything else is plain.
fn highlight(code: &str, language: CodeLanguage) -> Vec<Token<'_>> {
    let mut spans = Vec::new();
    let mut plain = 0;
    let mut i = 0;
    while i < code.len() {
        let rest = code.get(i..).unwrap_or_default();
        let Some(c) = rest.chars().next() else { break };
        if c == '#' {
            push_plain(&mut spans, code, &mut plain, i);
            let end = code.get(i..).and_then(|s| s.find('\n')).map_or(code.len(), |n| i + n);
            push(&mut spans, code, i, end, TokenKind::Comment);
            i = end;
            plain = i;
            continue;
        }
        if c == '"' || c == '\'' {
            push_plain(&mut spans, code, &mut plain, i);
            let triple = language == CodeLanguage::Python && rest.starts_with(&format!("{c}{c}{c}"));
            let end = string_end(code, i, c, triple);
            push(&mut spans, code, i, end, TokenKind::String);
            i = end;
            plain = i;
            continue;
        }
        if c.is_ascii_digit() && !ident_char_before(code, i) {
            push_plain(&mut spans, code, &mut plain, i);
            let end = scan_while(code, i, |c| c.is_ascii_alphanumeric() || c == '.' || c == '_');
            push(&mut spans, code, i, end, TokenKind::Number);
            i = end;
            plain = i;
            continue;
        }
        if ident_start(c) {
            push_plain(&mut spans, code, &mut plain, i);
            let end = scan_while(code, i, ident_char);
            let word = code.get(i..end).unwrap_or_default();
            let kind = if keyword(word, language) {
                TokenKind::Keyword
            } else if builtin(word) {
                TokenKind::Builtin
            } else if code.get(end..).is_some_and(|s| s.trim_start().starts_with('(')) {
                TokenKind::Function
            } else {
                TokenKind::Plain
            };
            push(&mut spans, code, i, end, kind);
            i = end;
            plain = i;
            continue;
        }
        i += c.len_utf8();
    }
    push_plain(&mut spans, code, &mut plain, code.len());
    // Merge neighbouring spans of one kind, so a plain run is one section.
    let mut merged: Vec<Token<'_>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if last.kind == span.kind => {
                let start = last.text.as_ptr() as usize - code.as_ptr() as usize;
                let end = span.text.as_ptr() as usize - code.as_ptr() as usize + span.text.len();
                if let Some(text) = code.get(start..end) {
                    last.text = text;
                }
            }
            _ => merged.push(span),
        }
    }
    merged
}

fn ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '.'
}

fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

fn ident_char_before(code: &str, i: usize) -> bool {
    code.get(..i).and_then(|s| s.chars().next_back()).is_some_and(ident_char)
}

fn scan_while(code: &str, from: usize, keep: impl Fn(char) -> bool) -> usize {
    let mut end = from;
    while let Some(c) = code.get(end..).and_then(|s| s.chars().next()) {
        if !keep(c) {
            break;
        }
        end += c.len_utf8();
    }
    end
}

fn string_end(code: &str, from: usize, quote: char, triple: bool) -> usize {
    let mut i = from + quote.len_utf8() * if triple { 3 } else { 1 };
    while let Some(c) = code.get(i..).and_then(|s| s.chars().next()) {
        if c == '\\' {
            i += c.len_utf8();
            if let Some(next) = code.get(i..).and_then(|s| s.chars().next()) {
                i += next.len_utf8();
            }
            continue;
        }
        if c == quote {
            if triple {
                let closing = format!("{quote}{quote}{quote}");
                if code.get(i..).is_some_and(|s| s.starts_with(&closing)) {
                    return i + 3;
                }
                i += c.len_utf8();
                continue;
            }
            return i + c.len_utf8();
        }
        if c == '\n' && !triple {
            return i;
        }
        i += c.len_utf8();
    }
    code.len()
}

fn push_plain<'a>(spans: &mut Vec<Token<'a>>, code: &'a str, plain: &mut usize, at: usize) {
    if at > *plain {
        push(spans, code, *plain, at, TokenKind::Plain);
    }
    *plain = at;
}

fn push<'a>(spans: &mut Vec<Token<'a>>, code: &'a str, from: usize, to: usize, kind: TokenKind) {
    if let Some(text) = code.get(from..to)
        && !text.is_empty()
    {
        spans.push(Token { text, kind });
    }
}

fn keyword(word: &str, language: CodeLanguage) -> bool {
    match language {
        CodeLanguage::Python => matches!(
            word,
            "False"
                | "None"
                | "True"
                | "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "case"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "elif"
                | "else"
                | "except"
                | "finally"
                | "for"
                | "from"
                | "global"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "match"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "try"
                | "while"
                | "with"
                | "yield"
        ),
        CodeLanguage::R => matches!(
            word,
            "NA" | "NA_character_"
                | "NA_integer_"
                | "NA_real_"
                | "NULL"
                | "NaN"
                | "TRUE"
                | "FALSE"
                | "Inf"
                | "break"
                | "else"
                | "for"
                | "function"
                | "if"
                | "in"
                | "next"
                | "repeat"
                | "return"
                | "while"
        ),
    }
}

fn builtin(word: &str) -> bool {
    matches!(
        word,
        "aes"
            | "c"
            | "cat"
            | "data.frame"
            | "dict"
            | "enumerate"
            | "float"
            | "ggplot"
            | "int"
            | "len"
            | "library"
            | "list"
            | "max"
            | "mean"
            | "min"
            | "plot"
            | "print"
            | "range"
            | "read.csv"
            | "seq"
            | "set"
            | "str"
            | "sum"
            | "summary"
            | "zip"
    )
}
