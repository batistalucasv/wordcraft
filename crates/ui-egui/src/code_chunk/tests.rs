use std::sync::mpsc;

use egui::Modifiers;
use egui_kittest::kittest::Queryable;

use super::*;

fn python() -> CodeChunk {
    CodeChunk::new(CodeLanguage::Python, "print('hello')")
}

fn plot_png() -> Vec<u8> {
    let image =
        image::RgbaImage::from_fn(
            16,
            12,
            |x, y| {
                if x % 4 == 0 || y % 4 == 0 { image::Rgba([48, 92, 219, 255]) } else { image::Rgba([255, 255, 255, 255]) }
            },
        );
    wordcraft_geom::picture::encode_format(&image::DynamicImage::ImageRgba8(image), image::ImageFormat::Png).unwrap()
}

fn result(stdout: &str, plot: Option<Vec<u8>>) -> ExecutionResult {
    ExecutionResult {
        stdout: stdout.to_owned(),
        stderr: String::new(),
        exit_code: Some(0),
        success: true,
        timed_out: false,
        output_truncated: false,
        plot_png: plot,
        plot_svg: None,
        plots: Vec::new(),
        tables: Vec::new(),
        duration_ms: 120,
    }
}

fn table(headers: &[&str], rows: &[&[&str]]) -> TabularOutput {
    TabularOutput {
        headers: headers.iter().map(|header| (*header).to_owned()).collect(),
        rows: rows.iter().map(|row| row.iter().map(|cell| (*cell).to_owned()).collect()).collect(),
    }
}

struct Fixture {
    chunk: CodeChunk,
    actions: Vec<CodeChunkAction>,
}

fn harness(chunk: CodeChunk) -> egui_kittest::Harness<'static, Fixture> {
    let mut fonts = false;
    egui_kittest::Harness::builder().with_size(egui::vec2(760.0, 620.0)).build_ui_state(
        move |ui, fixture: &mut Fixture| {
            if !fonts {
                crate::theme::install_fonts(ui.ctx());
                fonts = true;
            }
            ui.add_space(8.0);
            ui.set_width(720.0);
            if let Some(action) = fixture.chunk.show(ui) {
                fixture.actions.push(action);
            }
        },
        Fixture { chunk, actions: Vec::new() },
    )
}

fn steps(harness: &mut egui_kittest::Harness<'static, Fixture>, n: usize) {
    for _ in 0..n {
        harness.step();
    }
}

#[test]
fn highlight_spans_rebuild_the_source() {
    let cases = [
        (CodeLanguage::Python, "def f(x):\n    # note\n    return \"a#b\" + 1.5\n"),
        (CodeLanguage::Python, "s = '''first\nsecond'''\nprint(s)\n"),
        (CodeLanguage::R, "x <- c(1, 2, 3)\nif (TRUE) {\n  plot(x) # line\n}\n"),
        (CodeLanguage::R, ""),
    ];
    for (language, code) in cases {
        let joined: String = highlight(code, language).iter().map(|token| token.text).collect();
        assert_eq!(joined, code, "{language:?} spans must cover the source exactly");
    }
}

#[test]
fn highlight_marks_the_usual_pieces() {
    let code = "def f(x):\n    # note\n    return \"a#b\" + 1.5\n";
    let tokens = highlight(code, CodeLanguage::Python);
    let kind = |text: &str| tokens.iter().find(|token| token.text == text).map(|token| token.kind);
    assert_eq!(kind("def"), Some(TokenKind::Keyword));
    assert_eq!(kind("# note"), Some(TokenKind::Comment));
    assert_eq!(kind("\"a#b\""), Some(TokenKind::String), "a hash inside a string is not a comment");
    assert_eq!(kind("1.5"), Some(TokenKind::Number));
    assert_eq!(kind("f"), Some(TokenKind::Function), "a name before a parenthesis is a call");
}

#[test]
fn output_is_cut_on_a_character_boundary() {
    let long = "λ".repeat(MAX_OUTPUT_BYTES);
    let output = ChunkOutput::from_result(&ExecutionResult { stdout: long, ..result("", None) });
    assert!(output.stdout.len() <= MAX_OUTPUT_BYTES + 64, "the console text is bounded");
    assert!(output.stdout.ends_with("output cut after 64 KiB"), "{}", output.stdout.len());
    assert!(output.stdout.is_char_boundary(output.stdout.len() - 1), "no broken character");
}

#[test]
fn a_result_on_the_channel_becomes_the_output() {
    let ctx = egui::Context::default();
    let mut chunk = python();
    let (tx, rx) = mpsc::channel();
    chunk.attach_run(rx);
    tx.send(Ok(result("hello", Some(plot_png())))).unwrap();
    chunk.poll(&ctx);
    assert!(!chunk.is_running());
    let output = chunk.output().expect("the run's output");
    assert_eq!(output.stdout, "hello");
    assert!(output.success);
    assert!(output.has_plot(), "the plot makes the drawer open on its tab");
    assert_eq!(chunk.tab, OutputTab::Plot);
    assert!(chunk.output_open);
}

#[test]
fn a_runner_that_disconnects_is_reported() {
    let ctx = egui::Context::default();
    let mut chunk = python();
    let (tx, rx) = mpsc::channel();
    chunk.pending = Some(rx);
    drop(tx);
    chunk.poll(&ctx);
    assert!(!chunk.is_running());
    let output = chunk.output().expect("an error output");
    assert!(output.error.is_some(), "the drawer says the runner stopped");
    assert_eq!(chunk.tab, OutputTab::Console);
}

#[test]
fn switching_language_drops_the_other_interpreter_output() {
    let mut chunk = python();
    chunk.set_output(ChunkOutput { stdout: "hello".into(), success: true, ..ChunkOutput::default() });
    chunk.set_language(CodeLanguage::R);
    assert_eq!(chunk.language(), CodeLanguage::R);
    assert!(chunk.output().is_none(), "the old output belonged to Python");
    assert!(!chunk.is_running());
}

#[test]
fn ctrl_enter_is_taken_only_while_the_editor_has_focus() {
    let id = egui::Id::unique("code-chunk-editor-shortcut-test");
    let run_with = |modifiers: Modifiers, focus: bool| {
        let mut raw = egui::RawInput::default();
        raw.events.push(egui::Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers });
        let ctx = egui::Context::default();
        if focus {
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
        let mut taken = false;
        ctx.run_ui(raw, |ui| taken = shortcut_pressed(ui, id)).drop_without_applying_deltas();
        taken
    };
    assert!(run_with(Modifiers::COMMAND, true), "Ctrl+Enter runs the chunk whose editor has focus");
    assert!(!run_with(Modifiers::COMMAND | Modifiers::SHIFT, true), "Shift makes it Ctrl+Shift+Enter, which is not this action");
    assert!(!run_with(Modifiers::COMMAND, false), "elsewhere Ctrl+Enter stays the page break");
}

#[test]
fn the_language_badge_switches_language() {
    let mut harness = harness(python());
    steps(&mut harness, 3);
    harness.get_by_label("[ Python ]").click();
    steps(&mut harness, 2);
    harness.get_by_label("R").click();
    steps(&mut harness, 1);
    assert_eq!(harness.state().chunk.language(), CodeLanguage::R);
}

#[test]
fn the_options_menu_hides_and_shows_the_source() {
    let mut harness = harness(python());
    steps(&mut harness, 3);
    assert!(harness.query_by_role(egui::accesskit::Role::MultilineTextInput).is_some(), "the editor is drawn");

    harness.get_by_label("Options").click();
    steps(&mut harness, 2);
    harness.get_by_label("Hide code (Teaching Mode)").click();
    steps(&mut harness, 2);
    assert!(!harness.state().chunk.show_code);
    assert!(harness.query_by_role(egui::accesskit::Role::MultilineTextInput).is_none(), "Teaching Mode hides the source");

    harness.get_by_label("Options").click();
    steps(&mut harness, 2);
    harness.get_by_label("Show code").click();
    steps(&mut harness, 2);
    assert!(harness.query_by_role(egui::accesskit::Role::MultilineTextInput).is_some(), "the source is back");
}

#[test]
fn the_console_shows_stdout() {
    let mut chunk = python();
    chunk.set_output(ChunkOutput {
        stdout: "hello from the chunk".into(),
        success: true,
        exit_code: Some(0),
        duration_ms: 12,
        ..ChunkOutput::default()
    });
    chunk.tab = OutputTab::Console;
    let mut harness = harness(chunk);
    steps(&mut harness, 3);
    assert!(harness.query_by_label_contains("hello from the chunk").is_some(), "stdout is in the drawer");
    assert!(harness.query_by_label_contains("OK 0.01 s").is_some(), "the header says it succeeded");
}

#[test]
fn the_plot_tab_offers_insertion_into_the_document() {
    let mut chunk = python();
    chunk.set_output(ChunkOutput { plot_png: Some(plot_png()), success: true, duration_ms: 40, ..ChunkOutput::default() });
    let mut harness = harness(chunk);
    steps(&mut harness, 3);
    assert_eq!(harness.state().chunk.tab, OutputTab::Plot, "a plot opens on its tab");
    assert!(harness.query_by_label("Insert into document").is_some(), "the action is offered");

    harness.get_by_label("Insert into document").click();
    steps(&mut harness, 2);
    match harness.state().actions.last() {
        Some(CodeChunkAction::InsertPlot { png: Some(png), .. }) => assert!(!png.is_empty(), "the PNG reaches the host"),
        other => panic!("expected an insert action, got {other:?}"),
    }
}

#[test]
fn a_result_with_tables_opens_the_table_tab() {
    let ctx = egui::Context::default();
    let mut chunk = python();
    let (tx, rx) = mpsc::channel();
    chunk.attach_run(rx);
    tx.send(Ok(ExecutionResult { tables: vec![table(&["n"], &[&["1"]])], ..result("", None) })).unwrap();
    chunk.poll(&ctx);

    assert_eq!(chunk.tab, OutputTab::Table, "a run that captured a table opens on its tab");
    assert!(chunk.output().expect("the run's output").has_tables());
}

#[test]
fn a_plot_still_wins_the_initial_tab_when_a_run_produced_both() {
    let mut chunk = python();
    chunk.set_output(ChunkOutput { plot_png: Some(plot_png()), tables: vec![table(&["n"], &[&["1"]])], success: true, ..ChunkOutput::default() });

    assert_eq!(chunk.tab, OutputTab::Plot);
}

#[test]
fn the_table_tab_shows_the_captured_frame() {
    let mut chunk = python();
    chunk.set_output(ChunkOutput {
        tables: vec![table(&["species", "n"], &[&["setosa", "50"], &["virginica", "50"]])],
        success: true,
        ..ChunkOutput::default()
    });
    let mut harness = harness(chunk);
    steps(&mut harness, 3);

    assert_eq!(harness.state().chunk.tab, OutputTab::Table);
    assert!(harness.query_by_label_contains("species").is_some(), "the header is drawn");
    assert!(harness.query_by_label_contains("virginica").is_some(), "the cells are drawn");
}

#[test]
fn the_table_tab_offers_insertion_and_carries_every_row() {
    let rows: Vec<Vec<String>> = (0..250).map(|index| vec![index.to_string()]).collect();
    let mut chunk = python();
    chunk.set_output(ChunkOutput { tables: vec![TabularOutput { headers: vec!["n".to_owned()], rows }], success: true, ..ChunkOutput::default() });
    let mut harness = harness(chunk);
    steps(&mut harness, 3);

    assert!(harness.query_by_label_contains("Showing the first").is_some(), "the drawer says the grid is a preview");
    harness.get_by_label("Insert into document").click();
    steps(&mut harness, 2);
    match harness.state().actions.last() {
        Some(CodeChunkAction::InsertTable { headers, rows }) => {
            assert_eq!(headers, &vec!["n".to_owned()]);
            assert_eq!(rows.len(), 250, "the action carries the table, not the bounded preview");
        }
        other => panic!("expected a table insert action, got {other:?}"),
    }
}

#[test]
fn a_run_with_two_tables_lets_the_drawer_pick_one() {
    let mut chunk = python();
    chunk.set_output(ChunkOutput {
        tables: vec![table(&["first"], &[&["one"]]), table(&["second"], &[&["two"]])],
        success: true,
        ..ChunkOutput::default()
    });
    let mut harness = harness(chunk);
    steps(&mut harness, 3);

    assert!(harness.query_by_label_contains("Table 2").is_some(), "both tables are offered");
    harness.get_by_label("Table 2").click();
    steps(&mut harness, 2);
    assert_eq!(harness.state().chunk.table_index, 1);
    assert!(harness.query_by_label_contains("second").is_some(), "the second table is drawn");
}

#[test]
fn numeric_cells_are_recognised_for_right_alignment() {
    assert!(is_numeric("1.5"));
    assert!(is_numeric("-2"));
    assert!(is_numeric("1e6"));
    assert!(!is_numeric("NA"));
    assert!(!is_numeric(""));
    assert!(!is_numeric("setosa"));
}

#[test]
fn a_table_without_columns_does_not_reach_the_grid() {
    let mut chunk = python();
    chunk.set_output(ChunkOutput {
        tables: vec![TabularOutput { headers: Vec::new(), rows: vec![vec!["1".to_owned()]] }],
        success: true,
        ..ChunkOutput::default()
    });
    let mut harness = harness(chunk);
    steps(&mut harness, 3);

    assert!(harness.query_by_label_contains("This run captured no table.").is_some());
    assert!(harness.query_by_label("Insert into document").is_none(), "nothing to insert");
}

#[test]
fn a_wide_table_previews_fifty_columns_and_says_so() {
    let headers: Vec<String> = (0..60).map(|index| format!("c{index}")).collect();
    let row: Vec<String> = (0..60).map(|index| index.to_string()).collect();
    let mut chunk = python();
    chunk.set_output(ChunkOutput { tables: vec![TabularOutput { headers, rows: vec![row] }], success: true, ..ChunkOutput::default() });
    let mut harness = harness(chunk);
    steps(&mut harness, 3);

    assert!(harness.query_by_label_contains("Showing the first").is_some(), "the drawer says the grid is a preview");
    assert!(harness.query_by_label_contains("c49").is_some(), "the last previewed column is drawn");
}

#[test]
fn set_language_updates_in_read_mode_and_poll_carries_completed_output() {
    let mut chunk = python();
    chunk.read_only = true;
    chunk.set_language(CodeLanguage::R);
    assert_eq!(chunk.language(), CodeLanguage::R, "host sync must update language even in Read Mode");

    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::channel();
    chunk.pending = Some(rx);
    chunk.pending_source = Some((CodeLanguage::R, "1 + 1".into()));
    chunk.started_ms = Some(craft::platform::clock::monotonic_ms());
    tx.send(Ok(result("[1] 2", None))).unwrap();

    let completed = chunk.poll(&ctx).expect("completed run record");
    chunk.set_language(CodeLanguage::Python);
    assert_eq!(completed.language, CodeLanguage::R);
    assert_eq!(completed.code, "1 + 1");
    assert_eq!(completed.output.stdout, "[1] 2");
    assert!(chunk.output().is_none(), "language switch still drops widget's displayed output");
}

#[test]
fn embedded_chunk_emits_run_and_clear_requests_and_explains_svg_only_plot() {
    let mut chunk = python();
    chunk.available = true;
    chunk.set_output(ChunkOutput { plot_svg: Some("<svg xmlns=\"http://www.w3.org/2000/svg\"/>".into()), success: true, ..ChunkOutput::default() });
    let mut fonts = false;
    let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(760.0, 620.0)).build_ui_state(
        move |ui, (chunk, saw_run, saw_clear): &mut (CodeChunk, bool, bool)| {
            if !fonts {
                crate::theme::install_fonts(ui.ctx());
                fonts = true;
            }
            ui.set_width(720.0);
            let frame = chunk.show_embedded(ui);
            *saw_run |= frame.run_requested;
            *saw_clear |= frame.clear_requested;
        },
        (chunk, false, false),
    );
    for _ in 0..3 {
        h.step();
    }
    assert!(
        h.query_by_label_contains("Inline insertion requires a PNG").is_some(),
        "SVG-only fallback must tell users to rerun with PNG or both formats"
    );

    h.get_by_label("Run").click();
    h.step();
    assert!(h.state().1, "embedded Run button must emit run_requested without starting directly");
    assert!(!h.state().0.is_running());

    h.get_by_label("Options").click();
    for _ in 0..2 {
        h.step();
    }
    h.get_by_label("Clear output").click();
    h.step();
    assert!(h.state().2, "embedded header Clear output must emit clear_requested");
}

#[test]
fn embedded_chunk_run_button_test_passes_without_python_on_path() {
    let mut chunk = python();
    chunk.available = false;
    let mut fonts = false;
    let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(760.0, 620.0)).build_ui_state(
        move |ui, (chunk, saw_run): &mut (CodeChunk, bool)| {
            if !fonts {
                crate::theme::install_fonts(ui.ctx());
                fonts = true;
            }
            ui.set_width(720.0);
            let frame = chunk.show_embedded(ui);
            *saw_run |= frame.run_requested;
        },
        (chunk, false),
    );
    for _ in 0..2 {
        h.step();
    }
    h.get_by_label("Run").click();
    h.step();
    assert!(!h.state().1, "Run button stays disabled when chunk.available is false");

    let bin = CodeLanguage::Python.binary_name();
    let bin_exe = format!("{bin}.exe");
    let stripped_path = std::env::var_os("PATH")
        .map(|path_var| {
            let dirs = std::env::split_paths(&path_var).filter(|dir| !dir.join(bin).exists() && !dir.join(&bin_exe).exists());
            std::env::join_paths(dirs).expect("join stripped PATH")
        })
        .unwrap_or_default();

    let test_bin = std::env::current_exe().expect("current test executable");
    let output = std::process::Command::new(test_bin)
        .args(["--exact", "code_chunk::tests::embedded_chunk_emits_run_and_clear_requests_and_explains_svg_only_plot", "--nocapture"])
        .env("PATH", stripped_path)
        .output()
        .expect("run embedded_chunk_emits_run_and_clear_requests_and_explains_svg_only_plot without Python on PATH");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "embedded chunk Run-button test must pass without Python on PATH\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("1 passed"), "expected exact test to execute\nstdout:\n{stdout}\nstderr:\n{stderr}");
}

#[test]
fn new_and_set_code_cap_source_at_max_code_chars() {
    let over_ascii = "a".repeat(MAX_CODE_CHARS + 32);
    let over_utf8 = "λ".repeat(MAX_CODE_CHARS + 17);

    let mut chunk = CodeChunk::new(CodeLanguage::Python, over_ascii);
    assert_eq!(chunk.code().chars().count(), MAX_CODE_CHARS);

    chunk.set_code(over_utf8);
    assert_eq!(chunk.code().chars().count(), MAX_CODE_CHARS);
    assert!(chunk.code().chars().all(|c| c == 'λ'));
}

#[test]
fn attach_run_uses_monotonic_clock_and_renders_elapsed_time() {
    let (_tx, rx) = mpsc::channel();
    let mut chunk = python();
    chunk.attach_run(rx);
    assert!(chunk.started_ms.is_some());
    chunk.started_ms = Some(craft::platform::clock::monotonic_ms() - 1500.0);

    let mut harness = harness(chunk);
    steps(&mut harness, 2);
    assert!(harness.query_by_label_contains("Running…").is_some(), "running spinner and elapsed time are shown");

    harness.state_mut().chunk.started_ms = Some(craft::platform::clock::monotonic_ms() + 5000.0);
    steps(&mut harness, 1);
    assert!(harness.query_by_label_contains("0.0 s").is_some(), "clock skew clamps elapsed time cleanly");
}

#[test]
fn failed_plot_decode_is_cached_until_output_changes() {
    let mut chunk = python();
    chunk.set_output(ChunkOutput { plot_png: Some(b"not a valid png".to_vec()), success: true, ..ChunkOutput::default() });
    let mut harness = harness(chunk);
    steps(&mut harness, 5);

    assert_eq!(harness.state().chunk.plot_decode_attempts, 1, "corrupt PNG is decoded only once");
    assert_eq!(harness.state().chunk.plot_decode_failed.first().copied(), Some(true));
    assert!(harness.state().chunk.plot_textures.first().and_then(Option::as_ref).is_none());

    harness.state_mut().chunk.set_output(ChunkOutput { plot_png: Some(plot_png()), success: true, ..ChunkOutput::default() });
    steps(&mut harness, 4);

    assert_eq!(harness.state().chunk.plot_decode_attempts, 2, "replacing output retries decode once");
    assert_eq!(harness.state().chunk.plot_decode_failed.first().copied(), Some(false));
    assert!(harness.state().chunk.plot_textures.first().and_then(Option::as_ref).is_some());
}

#[test]
fn active_run_gates_run_set_language_and_attach_run() {
    let ctx = egui::Context::default();
    let (tx1, rx1) = mpsc::channel();
    let (tx2, rx2) = mpsc::channel();
    let mut chunk = python();

    chunk.attach_run(rx1);
    assert!(chunk.is_running());

    chunk.set_language(CodeLanguage::R);
    assert_eq!(chunk.language(), CodeLanguage::Python, "cannot switch language while a run is active");
    assert!(chunk.is_running());

    chunk.run(&ctx);
    assert!(chunk.output().is_none(), "run is ignored while another run is active");

    chunk.attach_run(rx2);
    assert!(tx2.send(Ok(result("second", None))).is_err(), "second receiver must not replace the active run");

    let mut harness = harness(chunk);
    steps(&mut harness, 2);
    harness.get_by_label("[ Python ]").click();
    steps(&mut harness, 2);
    assert!(harness.query_by_label("R").is_none(), "language menu is disabled while running");

    tx1.send(Ok(result("first", None))).unwrap();
    steps(&mut harness, 2);
    assert!(!harness.state().chunk.is_running());
    assert_eq!(harness.state().chunk.output().expect("first result").stdout, "first");

    harness.state_mut().chunk.set_language(CodeLanguage::R);
    assert_eq!(harness.state().chunk.language(), CodeLanguage::R);
}

#[test]
fn browser_mode_reports_desktop_only_message() {
    let mut chunk = python();
    chunk.run_browser();
    let output = chunk.output().expect("browser run records an explanatory error");
    assert_eq!(output.error.as_deref(), Some(BROWSER_NO_INTERPRETER_MESSAGE));
    assert_eq!(chunk.tab, OutputTab::Console);
    assert!(chunk.output_open);

    let mut harness = harness(chunk);
    steps(&mut harness, 3);
    assert!(harness.query_by_label_contains(BROWSER_NO_INTERPRETER_MESSAGE).is_some());
    assert!(harness.query_by_label_contains("Did not run").is_some());

    let (_tx, rx) = mpsc::channel();
    harness.state_mut().chunk.attach_run(rx);
    harness.state_mut().chunk.run_browser();
    assert!(harness.state().chunk.is_running(), "browser run gate also respects active run");
}

#[test]
fn multi_plot_selection_console_insert_and_inline_rename() {
    let mut chunk = python();
    chunk.set_output(ChunkOutput {
        stdout: "linha 1\nlinha 2\n".into(),
        plots: vec![
            wordcraft_kernel::PlotOutput { png: Some(plot_png()), svg: None },
            wordcraft_kernel::PlotOutput { png: Some(plot_png()), svg: Some("<svg/>".into()) },
        ],
        plot_png: Some(plot_png()),
        success: true,
        ..ChunkOutput::default()
    });
    chunk.set_label("Tabela de Dados");
    assert_eq!(chunk.label(), "Tabela de Dados");

    let mut h = harness(chunk);
    steps(&mut h, 3);
    assert!(h.query_by_label("Tabela de Dados").is_some(), "custom chunk label must show in header");
    assert!(h.query_by_label("Plot 1").is_some());
    assert!(h.query_by_label("Plot 2").is_some());

    // Select Plot 2 and insert: must emit Plot 2's SVG alongside PNG.
    h.get_by_label("Plot 2").click();
    steps(&mut h, 2);
    h.get_by_label("Insert into document").click();
    steps(&mut h, 2);
    assert!(
        matches!(h.state().actions.last(), Some(CodeChunkAction::InsertPlot { png: Some(_), svg: Some(s) }) if s == "<svg/>"),
        "Plot 2 insert must pass second plot payload: {:?}",
        h.state().actions
    );

    // Switch to Console tab and insert output as code text.
    h.state_mut().actions.clear();
    h.get_by_label("Console").click();
    steps(&mut h, 2);
    h.get_by_label("Insert into document").click();
    steps(&mut h, 2);
    assert_eq!(
        h.state().actions.last(),
        Some(&CodeChunkAction::InsertOutput { text: "linha 1\nlinha 2".into() }),
        "Console insert must emit captured console text"
    );
}
