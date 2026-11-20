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
use std::time::{Duration, Instant};

use egui::text::LayoutJob;
use egui::{Align, Align2, Color32, CornerRadius, FontId, Id, Key, Layout, Margin, RichText, Stroke, TextEdit, Ui, pos2, vec2};
use wordcraft_kernel::{CodeLanguage, ExecutionResult, TabularOutput};

#[cfg(not(target_arch = "wasm32"))]
use wordcraft_kernel::{ExecutionRequest, PlotFormat};

use crate::theme::Tokens;

#[cfg(test)]
mod tests;

/// Longest source a chunk keeps, in characters. Pasted code is cut here rather than growing
/// without bound.
pub const MAX_CODE_CHARS: usize = 1024 * 1024;

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
#[derive(Debug, Default)]
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
        Self {
            stdout: capped(&result.stdout),
            stderr: capped(&result.stderr),
            exit_code: result.exit_code,
            success: result.success,
            timed_out: result.timed_out,
            duration_ms: result.duration_ms,
            plot_png: result.plot_png.clone(),
            plot_svg: result.plot_svg.clone(),
            tables: result.tables.clone(),
            error: None,
        }
    }

    /// Whether the run captured a plot in either format.
    pub fn has_plot(&self) -> bool {
        self.plot_png.is_some() || self.plot_svg.is_some()
    }

    /// Whether the run captured at least one table.
    pub fn has_tables(&self) -> bool {
        !self.tables.is_empty()
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
    /// Add a captured table to the document as a native Word table. The whole table is carried,
    /// not the bounded preview the drawer draws.
    InsertTable {
        /// Column names in display order.
        headers: Vec<String>,
        /// Body rows, each with one cell per header.
        rows: Vec<Vec<String>>,
    },
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
    /// Which captured table the drawer shows when a run produced more than one.
    table_index: usize,
    output: Option<ChunkOutput>,
    /// The run in flight, when there is one.
    pending: Option<Receiver<RunResult>>,
    started: Option<Instant>,
    /// The plot's texture, decoded on first sight of its bytes.
    plot_texture: Option<egui::TextureHandle>,
}

impl CodeChunk {
    /// A chunk in `language` holding `code`, with the source and the drawer closed until needed.
    pub fn new(language: CodeLanguage, code: impl Into<String>) -> Self {
        let mut code = code.into();
        if let Some((end, _)) = code.char_indices().nth(MAX_CODE_CHARS) {
            code.truncate(end);
        }
        Self {
            id: next_id(),
            language,
            available: interpreter_available(language),
            code,
            show_code: true,
            output_open: false,
            tab: OutputTab::Console,
            zoom: 1.0,
            table_index: 0,
            output: None,
            pending: None,
            started: None,
            plot_texture: None,
        }
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
        self.code = code.into();
    }

    /// Switch language. The last output belonged to the other interpreter, so it is dropped, and
    /// a run in flight is abandoned.
    pub fn set_language(&mut self, language: CodeLanguage) {
        if self.language == language {
            return;
        }
        self.language = language;
        self.available = interpreter_available(language);
        self.pending = None;
        self.started = None;
        self.output = None;
        self.plot_texture = None;
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
        self.table_index = 0;
        self.output = Some(output);
        self.output_open = true;
        self.plot_texture = None;
    }

    /// Run the chunk now on a worker thread. Does nothing while another run is in flight.
    pub fn run(&mut self, ctx: &egui::Context) {
        if self.is_running() {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = ctx;
            self.fail("Code chunks run in the desktop app; the browser has no interpreter.");
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
            let request = ExecutionRequest::new(self.language, self.code.clone()).with_plot_format(PlotFormat::Both);
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new().name("wordcraft-code-chunk".to_owned()).spawn(move || {
                let _ = tx.send(wordcraft_kernel::execute(&request));
            });
            match spawned {
                Ok(_) => {
                    self.output = None;
                    self.plot_texture = None;
                    self.output_open = true;
                    self.tab = OutputTab::Console;
                    self.pending = Some(rx);
                    self.started = Some(Instant::now());
                    ctx.request_repaint_after(Duration::from_millis(50));
                }
                Err(e) => self.fail(format!("The code runner could not start: {e}")),
            }
        }
    }

    /// Show a run whose result will arrive on `rx`, from a host that executes chunks itself (an
    /// engine command). The widget then shows the spinner and applies the result exactly as Run
    /// does.
    pub fn attach_run(&mut self, rx: Receiver<RunResult>) {
        self.output = None;
        self.plot_texture = None;
        self.output_open = true;
        self.tab = OutputTab::Console;
        self.pending = Some(rx);
        self.started = Some(Instant::now());
    }

    /// Draw the chunk, returning the action the host must take, if any.
    pub fn show(&mut self, ui: &mut Ui) -> Option<CodeChunkAction> {
        self.poll(ui.ctx());
        let t = Tokens::get(ui.ctx());
        let editor = self.editor_id();
        if shortcut_pressed(ui, editor) {
            self.run(ui.ctx());
        }
        let focus = if ui.memory(|m| m.has_focus(editor)) { t.accent } else { t.border_strong };
        let width = ui.available_width();
        let mut action = None;
        egui::Frame::NONE.fill(t.panel).stroke(Stroke::new(1.0, focus)).corner_radius(CornerRadius::same(8)).show(ui, |ui| {
            if width.is_finite() {
                ui.set_width((width - 2.0).max(0.0));
            }
            self.header(ui, &t);
            if self.show_code {
                self.editor(ui, &t);
            } else {
                ui.add_space(2.0);
            }
            ui.separator();
            egui::Frame::NONE.inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
                self.drawer(ui, &t, &mut action);
            });
        });
        action
    }

    /// The id of the source editor, for focus checks and tests.
    pub fn editor_id(&self) -> Id {
        Id::unique(("wordcraft-code-chunk-editor", self.id))
    }

    /// Apply a result that arrived from a host or a worker thread.
    fn poll(&mut self, ctx: &egui::Context) {
        let received = match self.pending.as_ref() {
            None => return,
            Some(rx) => match rx.try_recv() {
                Ok(result) => Some(Some(result)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(None),
            },
        };
        match received {
            None => ctx.request_repaint_after(Duration::from_millis(50)),
            Some(Some(result)) => {
                self.pending = None;
                self.started = None;
                self.plot_texture = None;
                match result {
                    Ok(result) => self.set_output(ChunkOutput::from_result(&result)),
                    Err(e) => self.fail(e.to_string()),
                }
            }
            Some(None) => {
                self.pending = None;
                self.started = None;
                self.fail("The code runner stopped before it reported a result.");
            }
        }
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.output = Some(ChunkOutput::error(message));
        self.output_open = true;
        self.tab = OutputTab::Console;
        self.plot_texture = None;
    }

    fn header(&mut self, ui: &mut Ui, t: &Tokens) {
        let current = self.language;
        let mut picked = None;
        let mut toggle_code = false;
        let mut clear = false;
        let mut copy = false;
        let mut run = false;
        egui::Frame::NONE.fill(t.input).corner_radius(CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }).inner_margin(Margin::symmetric(8, 5)).show(
            ui,
            |ui| {
                ui.horizontal(|ui| {
                    let badge = ui.add(
                        egui::Button::new(RichText::new(format!("[ {} ]", language_name(current))).monospace().color(t.on_accent))
                            .fill(t.accent)
                            .corner_radius(CornerRadius::same(4))
                            .min_size(vec2(0.0, 21.0)),
                    );
                    badge.clone().on_hover_text(crate::tl!("Choose the chunk's language"));
                    crate::rtl::menu(&badge).show(|ui| {
                        for language in [CodeLanguage::R, CodeLanguage::Python] {
                            if ui.selectable_label(current == language, language_name(language)).clicked() {
                                picked = Some(language);
                                ui.close();
                            }
                        }
                    });

                    let arrow = if self.show_code { "▾" } else { "▸" };
                    if ui.add(egui::Button::new(RichText::new(arrow).color(t.text_dim)).frame(false)).clicked() {
                        toggle_code = true;
                    }
                    ui.label(RichText::new(crate::tl!("Code chunk")).color(t.text_dim));

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let options = ui.menu_button(RichText::new(crate::tl!("Options")).color(t.text_dim).size(11.5), |ui| {
                            if ui.button(crate::tl!("Copy code")).clicked() {
                                copy = true;
                                ui.close();
                            }
                            if ui.button(if self.show_code { crate::tl!("Hide code (Teaching Mode)") } else { crate::tl!("Show code") }).clicked() {
                                toggle_code = true;
                                ui.close();
                            }
                            if ui.add_enabled(self.output.is_some(), egui::Button::new(crate::tl!("Clear output"))).clicked() {
                                clear = true;
                                ui.close();
                            }
                        });
                        options.response.on_hover_text(crate::tl!("Chunk options"));

                        let run_button = ui.add_enabled(
                            !self.is_running() && self.available,
                            egui::Button::new(RichText::new(format!("▶ {}", crate::tl!("Run"))).color(t.on_accent))
                                .fill(t.accent)
                                .corner_radius(CornerRadius::same(4)),
                        );
                        if run_button.clicked() {
                            run = true;
                        }
                        if !self.available {
                            run_button.on_disabled_hover_text(format!(
                                "{} was not found on PATH. Install {} to run this chunk.",
                                current.binary_name(),
                                language_name(current)
                            ));
                        } else {
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
                            let secs = self.started.map_or(0.0, |started| started.elapsed().as_secs_f32());
                            ui.label(RichText::new(format!("{} {secs:.1} s", crate::tl!("Running…"))).color(t.text_dim).size(11.5));
                            ui.ctx().request_repaint_after(Duration::from_millis(100));
                        }
                    });
                });
            },
        );
        if let Some(language) = picked {
            self.set_language(language);
        }
        if toggle_code {
            self.show_code = !self.show_code;
        }
        if clear {
            self.output = None;
            self.plot_texture = None;
            self.pending = None;
            self.started = None;
        }
        if copy {
            ui.ctx().copy_text(self.code.clone());
        }
        if run {
            self.run(ui.ctx());
        }
    }

    fn editor(&mut self, ui: &mut Ui, t: &Tokens) {
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
                job.append(token.text, 0.0, egui::TextFormat { font_id: code_font.clone(), color: colours.of(token.kind), ..Default::default() });
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

        egui::Frame::NONE.inner_margin(Margin::symmetric(0, 6)).show(ui, |ui| {
            egui::ScrollArea::horizontal().id_salt(("wordcraft-code-chunk-scroll", self.id)).auto_shrink([false, true]).show(ui, |ui| {
                // The child's width is the source's, so long lines scroll instead of being cut.
                let size = vec2(gutter + text_w, lines as f32 * line_h + 4.0);
                ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.add_space(gutter);
                    let editor_id = self.editor_id();
                    let out = TextEdit::multiline(&mut self.code)
                        .id(editor_id)
                        .code_editor()
                        .frame(egui::Frame::NONE)
                        .margin(Margin::ZERO)
                        .desired_width(text_w)
                        .desired_rows(lines)
                        .hint_text(crate::tl!("Code"))
                        .char_limit(MAX_CODE_CHARS)
                        .layouter(&mut layouter)
                        .show(ui);
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
    }

    fn drawer(&mut self, ui: &mut Ui, t: &Tokens, action: &mut Option<CodeChunkAction>) {
        let arrow = if self.output_open { "▾" } else { "▸" };
        let mut toggle = false;
        ui.horizontal(|ui| {
            let text = format!("{arrow} {}", crate::tl!("Output"));
            if ui.add(egui::Button::new(RichText::new(text).color(t.text).size(12.0)).frame(false)).clicked() {
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
            OutputTab::Console => Self::console(ui, self.output.as_ref(), t, self.id),
            OutputTab::Plot => self.plot(ui, t, action),
            OutputTab::Table => self.table(ui, t, action),
        }
    }

    fn console(ui: &mut Ui, output: Option<&ChunkOutput>, t: &Tokens, id: u64) {
        let Some(output) = output else {
            ui.label(RichText::new(crate::tl!("Run the chunk to see its output.")).color(t.text_dim).size(12.0));
            return;
        };
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
        egui::ScrollArea::vertical().id_salt(("wordcraft-code-chunk-console", id)).max_height(CONSOLE_MAX_H).auto_shrink([false, true]).show(
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
        let Some(output) = self.output.as_ref() else {
            ui.label(RichText::new(crate::tl!("Run the chunk to see its plot.")).color(t.text_dim).size(12.0));
            return;
        };
        if !output.has_plot() {
            ui.label(RichText::new(crate::tl!("This run captured no plot.")).color(t.text_dim).size(12.0));
            return;
        }
        if self.plot_texture.is_none() {
            let png = output.plot_png.clone();
            if let Some(png) = png {
                self.plot_texture = decode_plot(ui.ctx(), &png, self.id);
            }
        }

        let mut insert = false;
        ui.horizontal(|ui| {
            let can_zoom = self.plot_texture.is_some();
            if ui.add_enabled(can_zoom && self.zoom > MIN_ZOOM, egui::Button::new("−").min_size(vec2(22.0, 0.0))).clicked() {
                self.zoom = (self.zoom / 1.25).clamp(MIN_ZOOM, MAX_ZOOM);
            }
            ui.label(RichText::new(format!("{:.0}%", self.zoom * 100.0)).monospace().size(11.0));
            if ui.add_enabled(can_zoom && self.zoom < MAX_ZOOM, egui::Button::new("+").min_size(vec2(22.0, 0.0))).clicked() {
                self.zoom = (self.zoom * 1.25).clamp(MIN_ZOOM, MAX_ZOOM);
            }
            if ui.add_enabled(can_zoom, egui::Button::new(crate::tl!("Fit"))).clicked()
                && let Some(texture) = &self.plot_texture
            {
                let natural = texture.size_vec2() / ui.ctx().pixels_per_point();
                let room = (ui.available_width() - 24.0).max(1.0);
                self.zoom = (room / natural.x.max(1.0)).clamp(MIN_ZOOM, MAX_ZOOM);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let response = ui.add_enabled(output.has_plot(), egui::Button::new(crate::tl!("Insert into document")));
                if response.clicked() {
                    insert = true;
                }
                response.on_hover_text(crate::tl!("Add the plot to the document as a picture"));
            });
        });

        if let Some(texture) = &self.plot_texture {
            let natural = texture.size_vec2() / ui.ctx().pixels_per_point();
            let size = natural * self.zoom;
            egui::ScrollArea::both().id_salt(("wordcraft-code-chunk-plot", self.id)).max_height(PLOT_MAX_H).auto_shrink([false, true]).show(
                ui,
                |ui| {
                    egui::Frame::NONE.fill(t.input).inner_margin(Margin::same(4)).show(ui, |ui| {
                        ui.add(egui::Image::new(texture).fit_to_exact_size(size));
                    });
                },
            );
        } else {
            ui.label(
                RichText::new(crate::tl!("The plot is vector (SVG). Insert it into the document to see it at full quality."))
                    .color(t.text_dim)
                    .size(12.0),
            );
        }

        if insert {
            *action = Some(CodeChunkAction::InsertPlot { png: output.plot_png.clone(), svg: output.plot_svg.clone() });
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
                let response = ui.button(crate::tl!("Insert into document"));
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
            Some((format!("✓ {secs:.2} s"), StatusKind::Ok))
        } else {
            let code = output.exit_code.map_or_else(|| "—".to_owned(), |code| code.to_string());
            Some((format!("× {} {code}", crate::tl!("Exit")), StatusKind::Fail))
        }
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

fn language_name(language: CodeLanguage) -> &'static str {
    match language {
        CodeLanguage::R => "R",
        CodeLanguage::Python => "Python",
    }
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

fn decode_plot(ctx: &egui::Context, png: &[u8], id: u64) -> Option<egui::TextureHandle> {
    let gpu_side = ctx.input(|input| input.max_texture_side).min(MAX_PLOT_SIDE as usize).max(1);
    let side = u32::try_from(gpu_side).unwrap_or(MAX_PLOT_SIDE);
    let image = wordcraft_geom::picture::decode_for_drawing(png, wordcraft_geom::MAX_DECODED_BYTES, side).ok()?;
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    if size[0] == 0 || size[1] == 0 {
        return None;
    }
    let pixels = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    Some(ctx.load_texture(format!("wordcraft-code-chunk-plot-{id}"), pixels, egui::TextureOptions::LINEAR))
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
