//! The interactive code chunk: a rounded code container with a language badge, a Run button, an
//! options menu, a monospace editor with line numbers and light syntax colouring, and a
//! collapsible output drawer holding the console, a zoomable plot viewer and captured tables.
//!
//! The widget is UI only. It keeps its own state (language, source, output and the run in
//! flight) and tells the host what must reach the document or runner through [`CodeChunkAction`]
//! (or [`EmbeddedFrame`] when mounted in the document canvas). Hosts attach asynchronous results
//! with [`CodeChunk::attach_run`] or [`CodeChunk::finish_attached_run`] while the drawer shows a
//! spinner and live elapsed time. The browser has no interpreter, so Run explains that rather than
//! starting a job.

mod highlight;
mod output;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use craft::behavior::EditEvent;
use craft::theme::variant::Tone;
use craft::theme::{ControlSize, Variant};
use craft::ui::buttons::Button;
use craft::ui::contract::{Bind, Entry, Props as _, Show as _, ShowContainer as _, TooltipSpec, Trigger};
use craft::ui::controls::disclosure::{Disclosure, DisclosureGlyph};
use craft::ui::display::{Divider, Shade, Spinner, Text};
use craft::ui::menu::{Menu, MenuItem};
use craft::ui::runtime::env::Env;
use craft::ui::runtime::focus::FocusRequest;
use craft::ui::style::{TypeRole, UiTokens, rgba_color};
use craft::ui::text::TextField;
use egui::text::LayoutJob;
use egui::{Align, Align2, CornerRadius, FontId, Id, Key, Layout, Margin, Stroke, TextEdit, Ui, pos2, vec2};
use wordcraft_engine::chunks::{CodeLanguage, ExecutionResult, KernelError};

use self::highlight::{CodeColours, highlight};
pub use self::output::{ChunkOutput, MAX_OUTPUT_BYTES, OutputTab};

/// Longest source a chunk keeps, in characters. Pasted code is cut here rather than growing
/// without bound.
pub const MAX_CODE_CHARS: usize = 4 * 1024 * 1024;

/// The line-number gutter's text size, in points.
const LINE_NUMBER_SIZE: f32 = 10.5;

/// Message shown when someone tries to run a chunk in the browser build, which has no
/// local R/Python interpreter.
const BROWSER_NO_INTERPRETER_MESSAGE: &str = "Code chunks run in the desktop app; the browser has no interpreter.";

/// One run's reply from the runner job: the kernel's result, or why it could not run.
pub type RunResult = Result<ExecutionResult, KernelError>;

/// What the host must do after interacting with the widget. The widget never touches the document
/// or spawns runner threads itself.
#[derive(Debug, Clone, PartialEq)]
pub enum CodeChunkAction {
    /// Execute `code` with `language` and feed the result back via [`CodeChunk::attach_run`] or
    /// [`CodeChunk::finish_attached_run`].
    Run {
        /// Language selected on the chunk.
        language: CodeLanguage,
        /// Current source code to execute.
        code: String,
    },
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

/// Output and launch source of an asynchronous attached run that finished this frame.
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
    /// Action requested from the output drawer (`InsertPlot`, `InsertTable`, `InsertOutput`).
    pub action: Option<CodeChunkAction>,
    /// True if the source text or language changed this frame.
    pub dirty: bool,
    /// True if the inline editor currently holds keyboard focus.
    pub editor_has_focus: bool,
    /// True if the inline editor lost keyboard focus this frame.
    pub editor_lost_focus: bool,
    /// Up (`-1`) or Down (`1`) arrow pressed on the first or last source line to leave the chunk.
    pub arrow_exit: Option<i32>,
    /// Output and launch source of an asynchronous attached run that finished this frame.
    pub completed_run: Option<CompletedRun>,
    /// Screen position of a secondary click inside the chunk card, if any.
    pub context_click_pos: Option<egui::Pos2>,
    /// True if the user pressed Run or Ctrl+Enter inside the chunk this frame.
    pub run_requested: bool,
    /// True if the user clicked Clear output in the chunk header this frame.
    pub clear_requested: bool,
    /// New or cleared chunk label committed from the inline header editor this frame.
    pub committed_label: Option<String>,
    /// True if the user clicked the code editor area this frame.
    pub editor_clicked: bool,
    /// True if the user started dragging the chunk's move handle this frame.
    pub drag_started: bool,
}

/// Outcome of [`CodeChunk::header`] on a single frame.
#[derive(Debug, Default)]
struct HeaderOutcome {
    run: bool,
    clear: bool,
    committed_label: Option<String>,
    drag_started: bool,
}

/// Outcome of [`CodeChunk::editor`] on a single frame.
#[derive(Debug, Default)]
struct EditorOutcome {
    changed: bool,
    has_focus: bool,
    lost_focus: bool,
    clicked: bool,
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
    /// Monotonic start timestamp from [`craft::platform::clock::monotonic_ms`], in milliseconds.
    started_ms: Option<f64>,
    /// The plots' decoded textures, lazily filled by index.
    plot_textures: Vec<Option<egui::TextureHandle>>,
    /// Remembers failed PNG decodes per plot index so corrupt bytes are not retried every frame.
    plot_decode_failed: Vec<bool>,
    plot_auto_fit: bool,
    #[cfg(test)]
    plot_decode_attempts: usize,
    #[cfg(test)]
    force_browser_for_test: bool,
    /// Optional human-readable chunk label (`#| label: ...`).
    label: String,
    editing_label: bool,
    label_draft: String,
    label_focus_pending: bool,
    /// Pending cursor placement when entering the editor (`Some(false)` = start, `Some(true)` = end).
    pending_cursor_edge: Option<bool>,
    /// Read-only view (e.g. Read Mode): blocks editing source, language, and run/insert actions.
    pub read_only: bool,
    /// Snapshot of `(language, code)` at the moment a run was attached with source tracking.
    pending_source: Option<(CodeLanguage, String)>,
}

impl CodeChunk {
    /// A chunk in `language` holding `code`, with the source visible and the drawer closed until needed.
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
            plot_auto_fit: true,
            #[cfg(test)]
            plot_decode_attempts: 0,
            #[cfg(test)]
            force_browser_for_test: false,
            label: String::new(),
            editing_label: false,
            label_draft: String::new(),
            label_focus_pending: false,
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

    /// Show a run whose result will arrive on `rx`, from a host that executes chunks itself. The
    /// widget then shows the spinner and applies the result when it arrives. Does nothing while
    /// another run is in flight.
    pub fn attach_run(&mut self, rx: Receiver<RunResult>) {
        if self.is_running() {
            return;
        }
        self.begin_pending(rx);
    }

    /// Record an explanatory error in the console drawer without starting a run.
    pub fn fail(&mut self, message: impl Into<String>) {
        self.apply_failure(ChunkOutput::error(message));
    }

    #[cfg(test)]
    fn run_browser(&mut self) {
        if self.is_running() {
            return;
        }
        self.fail(crate::tl!(BROWSER_NO_INTERPRETER_MESSAGE));
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
        self.plot_auto_fit = true;
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
            if self.is_browser() {
                self.fail(crate::tl!(BROWSER_NO_INTERPRETER_MESSAGE));
            } else if !self.available {
                self.fail(unavailable_run_message(self.language, false));
            } else {
                return Some(CodeChunkAction::Run { language: self.language, code: self.code.clone() });
            }
        }
        frame.action
    }

    /// Draw the chunk mounted inside the document canvas and report UI, focus, and edit status.
    pub fn show_embedded(&mut self, ui: &mut Ui) -> EmbeddedFrame {
        let prev_lang = self.language;
        let completed_run = self.poll(ui.ctx());
        let tokens = Env::of(ui).tokens.clone();
        let editor = self.editor_id();
        let shortcut_run = shortcut_pressed(ui, editor) && !self.read_only && !self.is_running();
        let focused = ui.memory(|m| m.has_focus(editor));
        let border = if focused { rgba_color(tokens.color.line.focus) } else { rgba_color(tokens.color.line.border) };
        let radius = UiTokens::corner(tokens.shape.radius.panel);
        let width = ui.available_width();
        let mut action = None;
        let mut header_outcome = HeaderOutcome::default();
        let mut editor_outcome = EditorOutcome::default();

        let frame_resp = egui::Frame::NONE
            .fill(rgba_color(tokens.color.surface.panel))
            .stroke(Stroke::new(tokens.stroke.thin, border))
            .corner_radius(radius)
            .show(ui, |ui| {
                if width.is_finite() {
                    ui.set_width((width - 2.0).max(0.0));
                }
                header_outcome = self.header(ui, &tokens);
                if self.show_code {
                    editor_outcome = self.editor(ui, &tokens);
                } else {
                    ui.add_space(tokens.space.gap.tight);
                }
                Divider::horizontal().show(ui);
                egui::Frame::NONE.inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
                    self.drawer(ui, &tokens, &mut action);
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
            run_requested: !self.read_only && !self.is_running() && (shortcut_run || header_outcome.run),
            clear_requested: !self.read_only && !self.is_running() && header_outcome.clear,
            committed_label: header_outcome.committed_label,
            editor_clicked: editor_outcome.clicked,
            drag_started: !self.read_only && header_outcome.drag_started,
        }
    }

    /// The id of the source editor, for focus checks and tests.
    pub fn editor_id(&self) -> Id {
        Id::unique(("wordcraft-code-chunk-editor", self.id))
    }

    /// Apply a result that arrived from a host runner. Returns the completed record when
    /// `pending_source` was captured.
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
                let out = ChunkOutput::error(crate::tl!("The code runner stopped before it reported a result."));
                self.apply_failure(out.clone());
                source.map(|(language, code)| CompletedRun { language, code, output: out })
            }
        }
    }

    fn apply_failure(&mut self, output: ChunkOutput) {
        self.output = Some(output);
        self.output_open = true;
        self.tab = OutputTab::Console;
        self.clear_plot_cache();
    }

    fn header(&mut self, ui: &mut Ui, tokens: &UiTokens) -> HeaderOutcome {
        let current = self.language;
        let mut picked = None;
        let mut toggle_code = false;
        let mut clear = false;
        let mut copy = false;
        let mut run = false;
        let mut drag_started = false;
        let r = UiTokens::corner(tokens.shape.radius.panel).nw;
        let committed_label = egui::Frame::NONE
            .fill(rgba_color(tokens.color.surface.sunken))
            .corner_radius(CornerRadius { nw: r, ne: r, sw: 0, se: 0 })
            .inner_margin(Margin::symmetric(8, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if !self.read_only {
                        let grip = ui
                            .add_sized(
                                vec2(20.0, 20.0),
                                egui::Label::new(egui::RichText::new("⋮⋮").size(14.0).strong().color(rgba_color(tokens.color.text.muted)))
                                    .sense(egui::Sense::click_and_drag()),
                            )
                            .on_hover_cursor(egui::CursorIcon::Grab)
                            .on_hover_text(crate::tl!("Drag to move code chunk"));
                        if grip.drag_started() {
                            drag_started = true;
                        }
                    }
                    picked = self.header_language_badge(ui, current);
                    let mut show_code = self.show_code;
                    Disclosure::new(Bind::bound(&mut show_code))
                        .label(crate::tl!("Toggle code"))
                        .tooltip(crate::tl!("Toggle code"))
                        .id_salt(("code-chunk-toggle-code", self.id))
                        .show(ui);
                    self.show_code = show_code;
                    let committed = self.header_title(ui);

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        self.header_options_menu(ui, &mut toggle_code, &mut copy, &mut clear);
                        if self.header_run_button(ui, current) {
                            run = true;
                        }
                        self.header_status(ui);
                    });
                    committed
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
            committed_label,
            drag_started,
        }
    }

    fn header_language_badge(&self, ui: &mut Ui, current: CodeLanguage) -> Option<CodeLanguage> {
        let badge_label = format!("[ {} ]", crate::tl!(language_name(current)));
        let can_pick = !self.read_only && !self.is_running();
        let badge = Button::new(badge_label)
            .variant(Variant::PRIMARY)
            .size(ControlSize::Sm)
            .enabled(can_pick)
            .tooltip(crate::tl!("Choose the chunk's language"))
            .id_salt(("code-chunk-language", self.id))
            .show(ui);
        if !can_pick {
            return None;
        }
        let entries = [
            Entry::new(CodeLanguage::R, crate::tl!("R")).checked(current == CodeLanguage::R),
            Entry::new(CodeLanguage::Python, crate::tl!("Python")).checked(current == CodeLanguage::Python),
        ];
        Menu::new(crate::tl!("Chunk language"))
            .values()
            .id_salt(("code-chunk-language-menu", self.id))
            .opened_by(&badge.response, Trigger::Press)
            .pick(ui, &entries)
            .picked
    }

    fn header_options_menu(&mut self, ui: &mut Ui, toggle_code: &mut bool, copy: &mut bool, clear: &mut bool) {
        let options_btn = Button::new(crate::tl!("Options"))
            .variant(Variant::GHOST)
            .size(ControlSize::Sm)
            .trailing_icon(DisclosureGlyph::menu_button())
            .tooltip(crate::tl!("Chunk options"))
            .id_salt(("code-chunk-options", self.id))
            .show(ui);
        Menu::new(crate::tl!("Options")).id_salt(("code-chunk-options-menu", self.id)).opened_by(&options_btn.response, Trigger::Press).show(
            ui,
            |ui, _| {
                if MenuItem::new(crate::tl!("Rename chunk")).enabled(!self.read_only).show(ui).ran {
                    self.label_draft = self.label.clone();
                    self.editing_label = true;
                    self.label_focus_pending = true;
                }
                if MenuItem::new(crate::tl!("Copy code")).show(ui).ran {
                    *copy = true;
                }
                let toggle_label = if self.show_code { crate::tl!("Hide code (Teaching Mode)") } else { crate::tl!("Show code") };
                if MenuItem::new(toggle_label).show(ui).ran {
                    *toggle_code = true;
                }
                if MenuItem::new(crate::tl!("Clear output")).enabled(!self.read_only && self.output.is_some() && !self.is_running()).show(ui).ran {
                    *clear = true;
                }
            },
        );
    }

    fn header_run_button(&self, ui: &mut Ui, current: CodeLanguage) -> bool {
        let tip = if !self.available {
            let msg = unavailable_run_message(current, self.is_browser());
            TooltipSpec::new(msg.clone()).disabled_text(msg)
        } else {
            TooltipSpec::new(crate::tl!("Run the chunk (Ctrl+Enter)"))
        };
        Button::new(crate::tl!("Run"))
            .variant(Variant::PRIMARY)
            .size(ControlSize::Sm)
            .enabled(!self.read_only && !self.is_running() && self.available)
            .tooltip(tip)
            .id_salt(("code-chunk-run", self.id))
            .show(ui)
            .clicked()
    }

    fn header_status(&self, ui: &mut Ui) {
        if let Some((text, tone)) = self.status() {
            Text::new(text).role(TypeRole::Caption).tone(tone).show(ui);
        } else if self.is_running() {
            let secs = self.started_ms.map_or(0.0, |started| {
                let elapsed = (craft::platform::clock::monotonic_ms() - started) / 1000.0;
                if elapsed.is_finite() { elapsed.max(0.0) as f32 } else { 0.0 }
            });
            let msg = crate::i18n::fmt(crate::tl!("Running… {seconds} s"), &[("seconds", &format!("{secs:.1}"))]);
            Text::new(msg).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
            Spinner::new().size(ControlSize::Sm).show(ui);
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }

    fn label_editor_id(&self) -> Id {
        Id::unique(("wordcraft-code-chunk-label", self.id))
    }

    fn header_title(&mut self, ui: &mut Ui) -> Option<String> {
        if self.read_only {
            self.editing_label = false;
            self.label_focus_pending = false;
        }
        if self.editing_label {
            let editor_id = self.label_editor_id();
            let mut field = TextField::new(crate::tl!("Chunk name"), Bind::bound(&mut self.label_draft))
                .placeholder(crate::tl!("Chunk name"))
                .size(ControlSize::Sm)
                .width_chars(18)
                .limit(128)
                .submits()
                .select_on_focus()
                .id(editor_id);
            if std::mem::take(&mut self.label_focus_pending) {
                field = field.request_focus(FocusRequest::Take);
            }
            let resp = field.show(ui);
            let cancel = resp.events.iter().any(|e| matches!(e, EditEvent::Cancel { .. })) || ui.input(|i| i.key_pressed(Key::Escape));
            let commit = resp.events.committed().is_some() || resp.response.lost_focus();
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

        let tokens = Env::of(ui).tokens.clone();
        let display = if self.label.is_empty() { crate::tl!("Code chunk").to_owned() } else { self.label.clone() };
        let title_size = tokens.typography.body.size + 2.0;
        let title_color = if self.label.is_empty() { rgba_color(tokens.color.text.muted) } else { rgba_color(tokens.color.text.normal) };
        let title_text = egui::RichText::new(display).size(title_size).strong().color(title_color);
        if self.read_only {
            ui.label(title_text);
            return None;
        }
        let resp = ui
            .add(egui::Label::new(title_text).sense(egui::Sense::click()))
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(crate::tl!("Click to name this chunk"));
        if resp.clicked() {
            self.label_draft = self.label.clone();
            self.editing_label = true;
            self.label_focus_pending = true;
        }
        None
    }

    fn editor(&mut self, ui: &mut Ui, tokens: &UiTokens) -> EditorOutcome {
        let editor_id = self.editor_id();
        let arrow_exit = self.update_editor_cursor_and_exit(ui, editor_id);

        let font = FontId::monospace(tokens.typography.mono.size);
        let mut layouter = build_code_layouter(self.language, font.clone(), CodeColours::new(tokens));
        let lines = self.code.matches('\n').count() + 1;
        let char_w = ui.fonts_mut(|fonts| fonts.glyph_width(&font, '0')).max(1.0);
        let line_h = ui.fonts_mut(|fonts| fonts.row_height(&font)).max(1.0);
        let columns = self.code.split('\n').map(|line| line.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum::<usize>()).max().unwrap_or(1);
        let text_w = columns as f32 * char_w + 2.0 * char_w;
        let digits = lines.to_string().len().max(2);
        let gutter = digits as f32 * char_w + 14.0;
        let editor_w = text_w.max((ui.available_width() - gutter).max(2.0 * char_w));

        let mut outcome = EditorOutcome { arrow_exit, ..Default::default() };
        let read_only = self.read_only;
        let accessible_name = editor_accessible_name(&self.label, self.language);
        let border_color = rgba_color(tokens.color.line.border);
        let number_color = rgba_color(tokens.color.text.disabled);

        let frame_resp = egui::Frame::NONE.inner_margin(Margin::symmetric(0, 6)).show(ui, |ui| {
            egui::ScrollArea::horizontal().id_salt(("wordcraft-code-chunk-scroll", self.id)).auto_shrink([false, true]).show(ui, |ui| {
                // The child's width is at least the card's width, and wider when long lines scroll.
                let size = vec2(gutter + editor_w, lines as f32 * line_h + 4.0);
                ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.add_space(gutter);
                    let mut show_box = |ui: &mut Ui, buffer: &mut dyn egui::TextBuffer| {
                        TextEdit::multiline(buffer)
                            .id(editor_id)
                            .code_editor()
                            .lock_focus(false)
                            .frame(egui::Frame::NONE)
                            .margin(Margin::ZERO)
                            .desired_width(editor_w)
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
                        show_box(ui, &mut self.code)
                    };
                    ui.ctx().accesskit_node_builder(out.response.id, |node| {
                        node.set_label(accessible_name.clone());
                    });
                    outcome.changed = !read_only && out.response.changed();
                    outcome.has_focus = out.response.has_focus();
                    outcome.lost_focus = out.response.lost_focus();
                    outcome.clicked =
                        !read_only && (out.response.clicked() || (out.response.has_focus() && ui.input(|i| i.pointer.primary_pressed())));
                    paint_line_numbers(ui, &out, gutter, border_color, number_color);
                });
            });
        });
        if !read_only
            && ui.input(|i| i.pointer.primary_pressed())
            && ui.input(|i| i.pointer.interact_pos()).is_some_and(|pos| frame_resp.response.rect.contains(pos))
        {
            ui.memory_mut(|m| m.request_focus(editor_id));
            outcome.has_focus = true;
            outcome.clicked = true;
        }
        outcome
    }

    fn update_editor_cursor_and_exit(&mut self, ui: &mut Ui, editor_id: Id) -> Option<i32> {
        if self.read_only {
            self.pending_cursor_edge = None;
            return None;
        }
        if let Some(at_end) = self.pending_cursor_edge.take() {
            ui.memory_mut(|m| m.request_focus(editor_id));
            let mut state = TextEdit::load_state(ui.ctx(), editor_id).unwrap_or_default();
            let idx = if at_end { self.code.chars().count() } else { 0 };
            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(idx))));
            state.store(ui.ctx(), editor_id);
            return None;
        }
        if ui.memory(|m| m.has_focus(editor_id))
            && let Some(range) = TextEdit::load_state(ui.ctx(), editor_id).and_then(|s| s.cursor.char_range())
            && range.primary == range.secondary
        {
            let b = self.code.char_indices().nth(range.primary.index.into()).map_or(self.code.len(), |(i, _)| i);
            let on_first_line = self.code.get(..b).is_some_and(|s| !s.contains('\n'));
            let on_last_line = self.code.get(b..).is_some_and(|s| !s.contains('\n'));
            if on_first_line && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowUp)) {
                ui.memory_mut(|m| m.surrender_focus(editor_id));
                return Some(-1);
            }
            if on_last_line && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowDown)) {
                ui.memory_mut(|m| m.surrender_focus(editor_id));
                return Some(1);
            }
        }
        None
    }

    fn status(&self) -> Option<(String, Tone)> {
        if self.is_running() {
            return None;
        }
        let output = self.output.as_ref()?;
        if output.error.is_some() {
            return Some((crate::tl!("Did not run").to_owned(), Tone::Bad));
        }
        if output.timed_out {
            return Some((crate::tl!("Timed out").to_owned(), Tone::Warn));
        }
        let secs = output.duration_ms as f64 / 1000.0;
        if output.success {
            Some((crate::i18n::fmt(crate::tl!("OK {seconds} s"), &[("seconds", &format!("{secs:.2}"))]), Tone::Good))
        } else {
            let code = output.exit_code.map_or_else(|| "—".to_owned(), |code| code.to_string());
            Some((crate::i18n::fmt(crate::tl!("× Exit {code}"), &[("code", &code)]), Tone::Bad))
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

fn build_code_layouter(
    language: CodeLanguage,
    code_font: FontId,
    colours: CodeColours,
) -> impl FnMut(&Ui, &dyn egui::TextBuffer, f32) -> std::sync::Arc<egui::Galley> {
    move |ui: &Ui, buffer: &dyn egui::TextBuffer, _wrap: f32| {
        let mut job = LayoutJob::default();
        // Code reads better unwrapped: the editor scrolls sideways instead.
        job.wrap.max_width = f32::INFINITY;
        job.keep_trailing_whitespace = true;
        for token in highlight(buffer.as_str(), language) {
            let format = egui::TextFormat { font_id: code_font.clone(), color: colours.of(token.kind), ..Default::default() };
            if token.text.contains('\u{FFFC}') {
                // Display-only replacement avoids missing-glyph panics while keeping U+FFFC in `self.code`.
                let display = token.text.replace('\u{FFFC}', "\u{FFFD}");
                job.append(&display, 0.0, format);
            } else {
                job.append(token.text, 0.0, format);
            }
        }
        ui.fonts_mut(|fonts| fonts.layout_job(job))
    }
}

fn paint_line_numbers(ui: &Ui, out: &egui::text_edit::TextEditOutput, gutter: f32, border_color: egui::Color32, number_color: egui::Color32) {
    let top = out.galley_pos;
    let painter = ui.painter();
    let height = out.galley.rect.height();
    painter.vline(top.x - gutter + 6.0, egui::Rangef::new(top.y, top.y + height), Stroke::new(1.0, border_color));
    let number_font = FontId::monospace(LINE_NUMBER_SIZE);
    let mut line = 1;
    let mut first_row = true;
    for row in &out.galley.rows {
        if first_row {
            let y = top.y + row.pos.y + row.rect().height() * 0.5;
            painter.text(pos2(top.x - 8.0, y), Align2::RIGHT_CENTER, line.to_string(), number_font.clone(), number_color);
            line += 1;
        }
        first_row = row.ends_with_newline;
    }
}

fn editor_accessible_name(label: &str, language: CodeLanguage) -> String {
    let code_word = crate::tl!("Code");
    let lang = crate::tl!(language_name(language));
    if label.is_empty() { format!("{code_word} ({lang})") } else { format!("{code_word} — {label} ({lang})") }
}

fn unavailable_run_message(language: CodeLanguage, browser: bool) -> String {
    if browser {
        crate::tl!(BROWSER_NO_INTERPRETER_MESSAGE).to_owned()
    } else {
        crate::i18n::fmt(
            crate::tl!("{binary} was not found on PATH. Install {language} to run this chunk."),
            &[("binary", language.binary_name()), ("language", crate::tl!(language_name(language)))],
        )
    }
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
        wordcraft_engine::chunks::is_language_available(language)
    }
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

fn clean_label(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(128).collect()
}
