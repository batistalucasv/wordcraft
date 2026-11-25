//! Collapsible output drawer for [`CodeChunk`]: console streams, zoomable plots, and tables.

use craft::theme::ControlSize;
use craft::theme::variant::{Tone, Variant};
use craft::ui::buttons::Button;
use craft::ui::contract::{Bind, Props as _, Show as _, TooltipSpec};
use craft::ui::controls::disclosure::Disclosure;
use craft::ui::display::{Shade, Text};
use craft::ui::style::{TypeRole, UiTokens, rgba_color};
use egui::{Align, Layout, Margin, RichText, Ui};
use wordcraft_engine::chunks::{ExecutionResult, PlotOutput, TabularOutput};

use super::{CodeChunk, CodeChunkAction};

/// Most of a stream the console shows, in bytes. The kernel already captured the whole output;
/// this only bounds what is laid out every frame.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

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

impl CodeChunk {
    pub(super) fn drawer(&mut self, ui: &mut Ui, tokens: &UiTokens, action: &mut Option<CodeChunkAction>) {
        let mut open = self.output_open;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = tokens.space.gap.tight;
            Disclosure::new(Bind::bound(&mut open))
                .label(crate::tl!("Toggle output"))
                .tooltip(crate::tl!("Toggle output"))
                .id_salt(("code-chunk-toggle-output", self.id))
                .show(ui);
            if Button::new(crate::tl!("Output"))
                .variant(craft::theme::Variant::GHOST)
                .size(ControlSize::Sm)
                .id_salt(("code-chunk-output-btn", self.id))
                .show(ui)
                .clicked()
            {
                open = !open;
            }
        });
        self.output_open = open;
        if !self.output_open {
            return;
        }

        let has_plot = self.output.as_ref().is_some_and(ChunkOutput::has_plot);
        let has_tables = self.output.as_ref().is_some_and(ChunkOutput::has_tables);
        ui.horizontal_wrapped(|ui| {
            for (tab, label, enabled, salt) in [
                (OutputTab::Console, crate::tl!("Console"), true, "console"),
                (OutputTab::Plot, crate::tl!("Plot"), has_plot, "plot"),
                (OutputTab::Table, crate::tl!("Table"), has_tables, "table"),
            ] {
                let variant = if self.tab == tab { Variant::SECONDARY } else { Variant::GHOST };
                if Button::new(label)
                    .variant(variant)
                    .size(ControlSize::Sm)
                    .enabled(enabled)
                    .id_salt(("code-chunk-tab", salt, self.id))
                    .show(ui)
                    .clicked()
                    && enabled
                {
                    self.tab = tab;
                }
            }
        });
        ui.add_space(tokens.space.gap.tight);

        match self.tab {
            OutputTab::Console => self.console(ui, tokens, action),
            OutputTab::Plot => self.plot(ui, tokens, action),
            OutputTab::Table => self.table(ui, tokens, action),
        }
    }

    fn console(&self, ui: &mut Ui, tokens: &UiTokens, action: &mut Option<CodeChunkAction>) {
        let Some(output) = self.output.as_ref() else {
            Text::new(crate::tl!("Run the chunk to see its output.")).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
            return;
        };
        if let Some(insert_text) = output.console_text() {
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let resp = Button::new(crate::tl!("Insert into document"))
                        .size(ControlSize::Sm)
                        .enabled(!self.read_only)
                        .tooltip(crate::tl!("Insert console output into the document as code text"))
                        .id_salt(("code-chunk-insert-console", self.id))
                        .show(ui);
                    if resp.clicked() {
                        *action = Some(CodeChunkAction::InsertOutput { text: insert_text });
                    }
                });
            });
        }
        if let Some(error) = &output.error {
            Text::new(error.clone()).role(TypeRole::Body).tone(Tone::Bad).show(ui);
            return;
        }
        if output.timed_out {
            let msg = crate::i18n::fmt(crate::tl!("Stopped after its timeout: {ms} ms."), &[("ms", &output.duration_ms.to_string())]);
            Text::new(msg).role(TypeRole::Caption).tone(Tone::Warn).show(ui);
        }
        if output.stdout.is_empty() && output.stderr.is_empty() {
            let text = if output.success { crate::tl!("No output") } else { crate::tl!("No output, but the exit code was not zero.") };
            Text::new(text).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
        }
        self.console_streams(ui, tokens, output);
    }

    fn console_streams(&self, ui: &mut Ui, tokens: &UiTokens, output: &ChunkOutput) {
        let well = rgba_color(tokens.color.surface.sunken);
        let text_color = rgba_color(tokens.color.text.normal);
        let err_color = rgba_color(tokens.color.status.bad.fg);
        let mono_size = tokens.typography.mono.size;
        let radius = UiTokens::corner(tokens.shape.radius.sm);
        egui::ScrollArea::vertical().id_salt(("wordcraft-code-chunk-console", self.id)).max_height(CONSOLE_MAX_H).auto_shrink([false, true]).show(
            ui,
            |ui| {
                for (title, text, color) in [(crate::tl!("stdout"), &output.stdout, text_color), (crate::tl!("stderr"), &output.stderr, err_color)] {
                    if text.is_empty() {
                        continue;
                    }
                    Text::new(title).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
                    egui::Frame::NONE.fill(well).corner_radius(radius).inner_margin(Margin::symmetric(8, 6)).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(text).monospace().size(mono_size).color(color)).wrap());
                    });
                    ui.add_space(tokens.space.gap.tight);
                }
            },
        );
    }

    fn plot(&mut self, ui: &mut Ui, tokens: &UiTokens, action: &mut Option<CodeChunkAction>) {
        let Some(count) = self.output.as_ref().map(ChunkOutput::plot_count).filter(|&n| n > 0) else {
            let msg = if self.output.is_none() { crate::tl!("Run the chunk to see its plot.") } else { crate::tl!("This run captured no plot.") };
            Text::new(msg).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
            return;
        };
        let mut selected = self.plot_index.min(count - 1);
        if count > 1 {
            ui.horizontal_wrapped(|ui| {
                for index in 0..count {
                    let label = crate::i18n::fmt(crate::tl!("Plot {n}"), &[("n", &(index + 1).to_string())]);
                    let variant = if selected == index { Variant::SECONDARY } else { Variant::GHOST };
                    if Button::new(label)
                        .variant(variant)
                        .size(ControlSize::Sm)
                        .id_salt(("code-chunk-plot-picker", self.id, index))
                        .show(ui)
                        .clicked()
                    {
                        selected = index;
                    }
                }
            });
        }
        self.plot_index = selected;

        let has_png = self.ensure_plot_texture(ui.ctx(), count, selected);
        let texture = self.plot_textures.get(selected).and_then(Option::as_ref).cloned();
        if self.plot_auto_fit
            && let Some(tex) = texture.as_ref()
        {
            self.zoom = fit_zoom_for_width(tex, ui);
        }
        let insert = self.plot_toolbar(ui, texture.as_ref(), has_png);
        self.plot_preview(ui, tokens, selected, texture.as_ref(), has_png);

        if insert && let Some((png, svg)) = self.output.as_ref().and_then(|o| o.plot_at(selected)) {
            *action = Some(CodeChunkAction::InsertPlot { png: png.map(ToOwned::to_owned), svg: svg.map(ToOwned::to_owned) });
        }
    }

    fn ensure_plot_texture(&mut self, ctx: &egui::Context, count: usize, selected: usize) -> bool {
        if self.plot_textures.len() < count {
            self.plot_textures.resize_with(count, || None);
            self.plot_decode_failed.resize(count, false);
        }
        let Some(output) = self.output.as_ref() else { return false };
        let Some((png, _)) = output.plot_at(selected) else { return false };
        if let (Some(slot), Some(failed), Some(bytes)) = (self.plot_textures.get_mut(selected), self.plot_decode_failed.get_mut(selected), png)
            && slot.is_none()
            && !*failed
        {
            #[cfg(test)]
            {
                self.plot_decode_attempts += 1;
            }
            *slot = decode_plot(ctx, bytes, self.id, selected);
            if slot.is_none() {
                *failed = true;
            }
        }
        png.is_some()
    }

    fn plot_toolbar(&mut self, ui: &mut Ui, texture: Option<&egui::TextureHandle>, has_png: bool) -> bool {
        let mut insert = false;
        let can_zoom = texture.is_some();
        ui.horizontal(|ui| {
            if Button::new("−")
                .size(ControlSize::Sm)
                .enabled(can_zoom && self.zoom > MIN_ZOOM)
                .tooltip(crate::tl!("Zoom out"))
                .id_salt(("code-chunk-zoom-out", self.id))
                .show(ui)
                .clicked()
            {
                self.plot_auto_fit = false;
                self.zoom = (self.zoom / 1.25).clamp(MIN_ZOOM, MAX_ZOOM);
            }
            Text::new(format!("{:.0}%", self.zoom * 100.0)).role(TypeRole::Mono).show(ui);
            if Button::new("+")
                .size(ControlSize::Sm)
                .enabled(can_zoom && self.zoom < MAX_ZOOM)
                .tooltip(crate::tl!("Zoom in"))
                .id_salt(("code-chunk-zoom-in", self.id))
                .show(ui)
                .clicked()
            {
                self.plot_auto_fit = false;
                self.zoom = (self.zoom * 1.25).clamp(MIN_ZOOM, MAX_ZOOM);
            }
            if Button::new(crate::tl!("Fit"))
                .size(ControlSize::Sm)
                .enabled(can_zoom)
                .tooltip(crate::tl!("Fit plot to width"))
                .id_salt(("code-chunk-zoom-fit", self.id))
                .show(ui)
                .clicked()
                && let Some(tex) = texture
            {
                self.plot_auto_fit = true;
                self.zoom = fit_zoom_for_width(tex, ui);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let tip = TooltipSpec::new(crate::tl!("Add the plot to the document as a picture"))
                    .disabled_text(crate::tl!("Inline insertion requires a PNG plot; rerun with PNG or both formats."));
                if Button::new(crate::tl!("Insert into document"))
                    .size(ControlSize::Sm)
                    .enabled(!self.read_only && has_png)
                    .tooltip(tip)
                    .id_salt(("code-chunk-insert-plot", self.id))
                    .show(ui)
                    .clicked()
                {
                    insert = true;
                }
            });
        });
        insert
    }

    fn plot_preview(&self, ui: &mut Ui, tokens: &UiTokens, selected: usize, texture: Option<&egui::TextureHandle>, has_png: bool) {
        if let Some(tex) = texture {
            let natural = tex.size_vec2() / ui.ctx().pixels_per_point();
            let size = natural * self.zoom;
            let well = rgba_color(tokens.color.surface.sunken);
            egui::ScrollArea::both()
                .id_salt(("wordcraft-code-chunk-plot", self.id, selected))
                .max_height(PLOT_MAX_H)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::Frame::NONE.fill(well).inner_margin(Margin::same(4)).show(ui, |ui| {
                        let resp = ui.add(egui::Image::new(tex).fit_to_exact_size(size));
                        ui.ctx().accesskit_node_builder(resp.id, |node| {
                            node.set_role(egui::accesskit::Role::Image);
                            node.set_label(crate::tl!("Plot preview"));
                        });
                    });
                });
        } else {
            let message = if !has_png {
                crate::tl!("The plot is vector-only (SVG). Inline insertion requires a PNG; rerun the chunk with PNG or both plot formats.")
            } else {
                crate::tl!("This plot could not be decoded for preview.")
            };
            Text::new(message).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
        }
    }

    fn table(&mut self, ui: &mut Ui, tokens: &UiTokens, action: &mut Option<CodeChunkAction>) {
        let Some(output) = self.output.as_ref() else {
            Text::new(crate::tl!("Run the chunk to see its table.")).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
            return;
        };
        if !output.has_tables() {
            Text::new(crate::tl!("This run captured no table.")).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
            return;
        }

        let count = output.tables.len();
        let mut selected = self.table_index.min(count - 1);
        if count > 1 {
            ui.horizontal_wrapped(|ui| {
                for index in 0..count {
                    let label = crate::i18n::fmt(crate::tl!("Table {n}"), &[("n", &(index + 1).to_string())]);
                    let variant = if selected == index { Variant::SECONDARY } else { Variant::GHOST };
                    if Button::new(label)
                        .variant(variant)
                        .size(ControlSize::Sm)
                        .id_salt(("code-chunk-table-picker", self.id, index))
                        .show(ui)
                        .clicked()
                    {
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
            Text::new(crate::tl!("This run captured no table.")).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
            return;
        }
        let insert = self.table_toolbar(ui, table.rows.len(), table.headers.len());
        self.table_grid(ui, tokens, table, preview_rows, preview_cols);
        if preview_rows < table.rows.len() || preview_cols < table.headers.len() {
            let msg = crate::i18n::fmt(
                crate::tl!("Showing the first {rows} × {cols}. Inserting keeps the whole table."),
                &[("rows", &preview_rows.to_string()), ("cols", &preview_cols.to_string())],
            );
            Text::new(msg).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
        }
        if insert {
            *action = Some(CodeChunkAction::InsertTable { headers: table.headers.clone(), rows: table.rows.clone() });
        }
    }

    fn table_toolbar(&self, ui: &mut Ui, row_count: usize, col_count: usize) -> bool {
        let mut insert = false;
        ui.horizontal(|ui| {
            Text::new(format!("{row_count} × {col_count}"))
                .role(TypeRole::Caption)
                .shade(Shade::Muted)
                .tooltip(crate::tl!("Rows × columns"))
                .show(ui);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if Button::new(crate::tl!("Insert into document"))
                    .size(ControlSize::Sm)
                    .enabled(!self.read_only)
                    .tooltip(crate::tl!("Add the table to the document as a Word table"))
                    .id_salt(("code-chunk-insert-table", self.id))
                    .show(ui)
                    .clicked()
                {
                    insert = true;
                }
            });
        });
        insert
    }

    fn table_grid(&self, ui: &mut Ui, tokens: &UiTokens, table: &TabularOutput, preview_rows: usize, preview_cols: usize) {
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
        let text_color = rgba_color(tokens.color.text.normal);
        let label_size = tokens.typography.caption.size;
        let mono_size = tokens.typography.mono.size;
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
                                ui.add(egui::Label::new(RichText::new(text).strong().size(label_size).color(text_color)));
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(18.0, preview_rows, |mut row| {
                            let Some(cells) = table.rows.get(row.index()) else { return };
                            for column in 0..preview_cols {
                                row.col(|ui| {
                                    let text = cells.get(column).map_or("", String::as_str);
                                    let label = egui::Label::new(RichText::new(text).monospace().size(mono_size).color(text_color));
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
    }
}

/// Whether a cell reads as a number, so the table lines it up on the right.
pub(super) fn is_numeric(text: &str) -> bool {
    !text.is_empty() && text.parse::<f64>().is_ok()
}

/// Cut a stream to what the console lays out, at a character boundary.
fn capped(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_owned();
    }
    let end = text.floor_char_boundary(MAX_OUTPUT_BYTES);
    let mut out = text.get(..end).unwrap_or_default().to_owned();
    out.push('\n');
    out.push_str(crate::tl!("… output cut after 64 KiB"));
    out
}

fn fit_zoom_for_width(tex: &egui::TextureHandle, ui: &Ui) -> f32 {
    let natural = tex.size_vec2() / ui.ctx().pixels_per_point();
    let room = (ui.available_width() - 24.0).max(1.0);
    (room / natural.x.max(1.0)).clamp(MIN_ZOOM, MAX_ZOOM)
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
