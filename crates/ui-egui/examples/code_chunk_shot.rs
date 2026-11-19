//! Renders the code chunk widget offscreen with wgpu, so the result can be looked at without a
//! desktop: `cargo run -p wordcraft-ui-egui --example code_chunk_shot -- /tmp/code-chunk.png`.
//!
//! The picture shows a finished Python chunk with its console and a teaching-mode R chunk with
//! its plot, which is what the widget looks like after a run.

use wordcraft_kernel::CodeLanguage;
use wordcraft_ui_egui::code_chunk::{ChunkOutput, CodeChunk, OutputTab};

struct Demo {
    console: CodeChunk,
    plot: CodeChunk,
    fonts: bool,
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| std::env::temp_dir().join("wordcraft-code-chunk.png").to_string_lossy().into_owned());
    let mut console =
        CodeChunk::new(CodeLanguage::Python, "import pandas as pd\n\ndata = pd.read_csv(\"plots.csv\")\nsummary = data.describe()\nprint(summary)");
    console.set_output(ChunkOutput {
        stdout: "       year      cover\ncount   120.0     120.0\nmean   2012.5      62.4\nstd       5.8      18.1\nmin    2003.0      31.0\nmax    2022.0      94.0\n".into(),
        success: true,
        exit_code: Some(0),
        duration_ms: 1340,
        ..ChunkOutput::default()
    });
    console.tab = OutputTab::Console;

    let mut plot = CodeChunk::new(CodeLanguage::R, "plot(cars$speed, cars$dist, main = \"Stopping distance\")\n");
    plot.show_code = false;
    plot.set_output(ChunkOutput {
        success: true,
        exit_code: Some(0),
        duration_ms: 820,
        plot_png: Some(plot_png(720, 440)),
        ..ChunkOutput::default()
    });

    let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(880.0, 760.0)).with_pixels_per_point(2.0).wgpu().build_ui_state(
        move |ui, demo: &mut Demo| {
            if !demo.fonts {
                ui.ctx().set_visuals(egui::Visuals::light());
                wordcraft_ui_egui::theme::install_fonts_for(ui.ctx(), false);
                demo.fonts = true;
            }
            ui.set_width(820.0);
            ui.add_space(16.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                demo.console.show(ui);
                ui.add_space(18.0);
                demo.plot.show(ui);
            });
        },
        Demo { console, plot, fonts: false },
    );
    for _ in 0..6 {
        harness.step();
    }
    match harness.render() {
        Ok(image) => {
            image.save(&out).expect("save png");
            println!("{out}");
        }
        Err(error) => eprintln!("render failed: {error}"),
    }
}

/// A small chart drawn in code (original artwork): axes, a curve and its points.
fn plot_png(width: u32, height: u32) -> Vec<u8> {
    let mut image = image::RgbaImage::from_pixel(width, height, image::Rgba([255, 255, 255, 255]));
    let ink = image::Rgba([64, 64, 64, 255]);
    let line = image::Rgba([48, 92, 219, 255]);
    let point = image::Rgba([224, 123, 31, 255]);
    let (left, right, top, bottom) = (56, width - 24, 28, height - 40);
    for x in left..=right {
        image.put_pixel(x, bottom, ink);
    }
    for y in top..=bottom {
        image.put_pixel(left, y, ink);
    }
    let mut previous = None;
    for x in left..=right {
        let t = (x - left) as f32 / (right - left).max(1) as f32;
        let y = bottom as f32 - 14.0 - (t * std::f32::consts::PI).sin() * (bottom - top) as f32 * 0.72;
        let y = y.clamp(top as f32, bottom as f32) as u32;
        if let Some((px, py)) = previous {
            line_between(&mut image, px, py, x, y, line);
        }
        previous = Some((x, y));
    }
    for step in 0..=12 {
        let x = left + (right - left) * step / 12;
        let t = (x - left) as f32 / (right - left).max(1) as f32;
        let y = (bottom as f32 - 14.0 - (t * std::f32::consts::PI).sin() * (bottom - top) as f32 * 0.72).clamp(top as f32, bottom as f32) as u32;
        for dy in -3_i32..=3 {
            for dx in -3_i32..=3 {
                if dx * dx + dy * dy <= 9 {
                    let px = x as i32 + dx;
                    let py = y as i32 + dy;
                    if px >= 0 && py >= 0 && (px as u32) < width && (py as u32) < height {
                        image.put_pixel(px as u32, py as u32, point);
                    }
                }
            }
        }
    }
    for (x, y) in [(left, top), (left, bottom)] {
        image.put_pixel(x, y, ink);
    }
    wordcraft_geom::picture::encode_format(&image::DynamicImage::ImageRgba8(image), image::ImageFormat::Png).unwrap_or_default()
}

fn line_between(image: &mut image::RgbaImage, x0: u32, y0: u32, x1: u32, y1: u32, colour: image::Rgba<u8>) {
    let steps = (x1.abs_diff(x0)).max(y1.abs_diff(y0)).max(1);
    for step in 0..=steps {
        let x = x0 as i64 + (x1 as i64 - x0 as i64) * step as i64 / steps as i64;
        let y = y0 as i64 + (y1 as i64 - y0 as i64) * step as i64 / steps as i64;
        if x >= 0 && y >= 0 && (x as u32) < image.width() && (y as u32) < image.height() {
            image.put_pixel(x as u32, y as u32, colour);
        }
    }
}
