//! Insert › Code: chunk insertion, dispatch and what each command reports.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use wordcraft_doc::chunk::MAX_CHUNK_LINES;
use wordcraft_doc::{CHUNK_STYLE, CodeChunk, StoryRef};
use wordcraft_kernel::{CodeLanguage, ExecutionRequest, ExecutionResult, PlotFormat, TabularOutput};

use super::{run, s};
use crate::chunks;
use crate::{CmdError, Selection, Session};

/// A successful run with `stdout` and no plot.
fn ok(stdout: &str) -> ExecutionResult {
    ExecutionResult {
        stdout: stdout.into(),
        stderr: String::new(),
        exit_code: Some(0),
        success: true,
        timed_out: false,
        output_truncated: false,
        plot_png: None,
        plot_svg: None,
        plots: Vec::new(),
        tables: Vec::new(),
        duration_ms: 7,
    }
}

/// What the fake kernel saw.
struct Fake {
    seen: Arc<Mutex<Vec<(CodeLanguage, String, u64, bool, PlotFormat)>>>,
}

/// Install a kernel that answers by code content: `boom` fails, anything else prints it back.
fn fake(s: &mut Session) -> Fake {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    s.chunks.set_runner(Box::new(move |request: &ExecutionRequest| {
        log.lock().unwrap().push((request.language, request.code.clone(), request.timeout_ms, request.capture_plots, request.plot_format));
        Ok(if request.code.contains("boom") {
            ExecutionResult { success: false, exit_code: Some(1), ..ok("") }
        } else {
            ok(&format!("ran {}", request.code))
        })
    }));
    Fake { seen }
}

fn chunk_id(v: &Value) -> i64 {
    v["chunk"]["id"].as_i64().expect("chunk id")
}

/// Put the caret inside the chunk at `index`.
fn caret_into(s: &mut Session, index: usize) {
    let range = s.doc.chunk_refs(StoryRef::Body).into_iter().nth(index).expect("chunk");
    s.sel = Selection::caret(range.end.clone());
}

#[test]
fn insert_chunk_wraps_a_language_tagged_block_control() {
    let mut s = s();
    let v = run(&mut s, "code.insert_chunk", json!({"language": "python", "code": "print(1)"}));
    let id = chunk_id(&v);
    assert_eq!(v["chunk"]["tag"], "wordcraft:chunk:python");
    let range = chunks::chunk_at(&s.doc, &s.sel.focus).expect("the caret is inside the chunk");
    assert_eq!(range.chunk.id, Some(id));
    assert_eq!(range.chunk.language, "python");
    assert_eq!(range.chunk.code, "print(1)");
    assert_eq!(range.start.off, 0);
    let control = &s.doc.control_ranges(StoryRef::Body)[0].control;
    assert_eq!(control.title, "Python code chunk");
    assert!(control.block);
    // The code is set in the model's code-box style.
    assert_eq!(s.doc.para(StoryRef::Body, &range.start.path).and_then(|p| p.props.style.as_deref()), Some(CHUNK_STYLE));
}

#[test]
fn insert_chunk_gives_the_chunk_its_own_paragraph() {
    let mut s = s();
    run(&mut s, "text.insert", json!({"text": "Hello"}));
    run(&mut s, "code.insert_chunk", json!({"code": "x <- 1"}));
    let range = chunks::chunk_at(&s.doc, &s.sel.focus).expect("caret inside the chunk");
    assert_ne!(range.start.path.last(), 0, "the chunk is not inside the text paragraph");
    assert_eq!(s.doc.plain_text(StoryRef::Body).lines().next(), Some("Hello"));
}

#[test]
fn insert_chunk_uses_the_empty_paragraph_at_the_caret() {
    let mut s = s();
    run(&mut s, "code.insert_chunk", json!({"language": "r", "code": "x <- 1"}));
    let range = chunks::chunk_at(&s.doc, &s.sel.focus).expect("caret inside the chunk");
    assert_eq!(range.start.path.last(), 0);
    assert_eq!(range.chunk.code, "x <- 1");
}

#[test]
fn insert_chunk_refuses_to_nest_in_another_content_control() {
    let mut s = s();
    run(&mut s, "developer.richText", json!({}));
    let e = s.run("code.insert_chunk", &json!({})).unwrap_err();
    assert!(matches!(e, CmdError::Disabled(_)), "{e}");
}

#[test]
fn a_refused_insert_keeps_the_selection() {
    let mut s = Session::new(wordcraft_doc::Document::from_text("hello"));
    run(&mut s, "select.all", json!({}));
    run(&mut s, "developer.richText", json!({}));
    run(&mut s, "edit.find", json!({"text": "hello"}));
    let before = s.doc.clone();
    let e = s.run("code.insert_chunk", &json!({})).unwrap_err();
    assert!(matches!(e, CmdError::Disabled(_)), "{e}");
    assert_eq!(s.doc, before, "a refused insert must not delete the selection");
}

#[test]
fn an_oversized_chunk_is_refused_without_deleting_the_selection() {
    let mut s = Session::new(wordcraft_doc::Document::from_text("hello"));
    run(&mut s, "select.all", json!({}));
    let before = s.doc.clone();
    let code = "x".repeat(chunks::MAX_CHUNK_BYTES + 1);
    let e = s.run("code.insert_chunk", &json!({"code": code})).unwrap_err();
    assert!(matches!(e, CmdError::Params(_)), "{e}");
    assert_eq!(s.doc, before, "an oversized chunk must not delete the selection");
}

#[test]
fn invalid_chunk_code_is_refused_without_deleting_the_selection() {
    let mut s = Session::new(wordcraft_doc::Document::from_text("hello"));
    run(&mut s, "select.all", json!({}));
    let before = s.doc.clone();
    let e = s.run("code.insert_chunk", &json!({"code": "print('\u{FFFC}')"})).unwrap_err();
    assert!(matches!(e, CmdError::Params(_)), "{e}");
    assert_eq!(s.doc, before, "U+FFFC in chunk code must not delete the selection");

    let too_many_lines = "\n".repeat(MAX_CHUNK_LINES);
    let e = s.run("code.insert_chunk", &json!({"code": too_many_lines})).unwrap_err();
    assert!(matches!(e, CmdError::Params(_)), "{e}");
    assert_eq!(s.doc, before, "excess line count must not delete the selection");
}

#[test]
fn insert_chunk_from_inside_a_chunk_goes_after_it() {
    let mut s = s();
    run(&mut s, "code.insert_chunk", json!({"code": "x\n"}));
    // The trailing newline leaves the caret on an empty line, inside the chunk.
    assert!(chunks::chunk_at(&s.doc, &s.sel.focus).is_some());
    run(&mut s, "code.insert_chunk", json!({"code": "y"}));
    let refs = s.doc.chunk_refs(StoryRef::Body);
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0].chunk.code, "x\n");
    assert_eq!(refs[1].chunk.code, "y");
    assert_eq!(refs[1].start.path.last(), refs[0].end.path.last() + 1);
    assert!(s.doc.control_ranges(StoryRef::Body).iter().all(|r| r.depth == 0), "no nested controls");
}

#[test]
fn insert_chunk_keeps_a_multi_line_chunk_line_for_line() {
    let mut s = s();
    run(&mut s, "code.insert_chunk", json!({"language": "python", "code": "a = 1\n\nb = 2"}));
    let range = chunks::chunk_at(&s.doc, &s.sel.focus).expect("caret inside the chunk");
    assert_eq!(range.chunk.code, "a = 1\n\nb = 2");
    assert_eq!(range.end.path.last(), range.start.path.last() + 2);
}

#[test]
fn insert_chunk_carries_quarto_options() {
    let mut s = s();
    let v = run(&mut s, "code.insert_chunk", json!({"code": "plot(1)", "options": {"eval": false, "fig-cap": "A plot"}}));
    let chunk = &s.doc.chunk_refs(StoryRef::Body)[0].chunk;
    assert_eq!(chunk.options.flag("eval"), Some(false));
    assert_eq!(chunk.options.fig_caption().as_deref(), Some("A plot"));
    assert_eq!(v["chunk"]["options"]["eval"], "false");
}

#[test]
fn insert_chunk_refuses_a_language_it_cannot_run() {
    let mut s = s();
    let e = s.run("code.insert_chunk", &json!({"language": "julia"})).unwrap_err();
    assert!(matches!(e, CmdError::Params(_)), "{e}");
    // A non-string language is ignored, as every optional parameter is.
    let v = run(&mut s, "code.insert_chunk", json!({"language": 3}));
    assert_eq!(v["chunk"]["language"], "r");
}

#[test]
fn run_chunk_is_disabled_outside_a_chunk() {
    let mut s = s();
    run(&mut s, "text.insert", json!({"text": "plain text"}));
    let e = s.run("code.run_chunk", &json!({})).unwrap_err();
    assert!(matches!(e, CmdError::Disabled(_)), "{e}");
    assert_eq!(s.registry.get("code.run_chunk").and_then(|c| (c.enabled)(&s)), Some("the document has no code chunks"));

    run(&mut s, "code.insert_chunk", json!({"code": "1 + 1"}));
    s.sel = Selection::caret(s.doc.start_of(StoryRef::Body));
    let e = s.run("code.run_chunk", &json!({})).unwrap_err();
    assert!(matches!(e, CmdError::Disabled(ref msg) if msg == "the cursor is not inside a code chunk"), "{e}");
}

#[test]
fn run_chunk_accepts_chunk_id_when_caret_is_elsewhere() {
    let mut s = s();
    run(&mut s, "text.insert", json!({"text": "lead"}));
    let first = chunk_id(&run(&mut s, "code.insert_chunk", json!({"code": "first"})));
    let _second = chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "python", "code": "second"})));
    s.sel = Selection::caret(s.doc.start_of(StoryRef::Body));
    assert!(chunks::chunk_at(&s.doc, &s.sel.focus).is_none());

    let fake = fake(&mut s);
    let out = run(&mut s, "code.run_chunk", json!({"chunkId": first}));
    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, "first");
    drop(seen);
    assert_eq!(out["chunk"]["id"], first);
    assert_eq!(out["output"]["stdout"], "ran first");
    assert_eq!(s.chunks.output(first).map(|o| o.stdout.as_str()), Some("ran first"));

    for bad in [json!(0), json!(-5), json!("first"), json!(1e30)] {
        let e = s.run("code.run_chunk", &json!({"chunkId": bad})).unwrap_err();
        assert!(matches!(e, CmdError::Params(_)), "{bad}: {e}");
    }
    let e = s.run("code.run_chunk", &json!({"chunkId": 999_999})).unwrap_err();
    assert!(matches!(e, CmdError::Failed(_)), "{e}");
}

#[test]
fn run_chunk_sends_the_chunk_text_and_keeps_the_output() {
    let mut s = s();
    let v = run(&mut s, "code.insert_chunk", json!({"language": "r", "code": "cat('hi')"}));
    let id = chunk_id(&v);
    let fake = fake(&mut s);
    let out = run(&mut s, "code.run_chunk", json!({"timeoutMs": 1234}));
    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, CodeLanguage::R);
    assert_eq!(seen[0].1, "cat('hi')");
    assert_eq!(seen[0].2, 1234);
    assert!(seen[0].3, "plots are captured by default");
    assert_eq!(seen[0].4, PlotFormat::Png);
    drop(seen);
    assert_eq!(out["chunk"]["id"], id);
    assert_eq!(out["output"]["stdout"], "ran cat('hi')");
    assert_eq!(out["output"]["success"], true);
    assert_eq!(s.chunks.output(id).map(|o| o.stdout.as_str()), Some("ran cat('hi')"));
}

#[test]
fn run_chunk_clamps_the_timeout_and_reads_the_plot_option() {
    let mut s = s();
    run(&mut s, "code.insert_chunk", json!({"code": "plot(1)"}));
    let fake = fake(&mut s);
    run(&mut s, "code.run_chunk", json!({"timeoutMs": 1, "plot": "none"}));
    run(&mut s, "code.run_chunk", json!({"timeoutMs": 9_999_999, "plot": "svg"}));
    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen[0].2, chunks::MIN_TIMEOUT_MS);
    assert!(!seen[0].3, "plot none asks for no capture");
    assert_eq!(seen[1].2, chunks::MAX_TIMEOUT_MS);
    assert!(seen[1].3);
    assert_eq!(seen[1].4, PlotFormat::Svg);
    drop(seen);
    let e = s.run("code.run_chunk", &json!({"plot": "gif"})).unwrap_err();
    assert!(matches!(e, CmdError::Params(_)), "{e}");
}

#[test]
fn run_chunk_reports_a_plot_only_when_asked() {
    let mut s = s();
    run(&mut s, "code.insert_chunk", json!({"code": "plot(1)"}));
    s.chunks.set_runner(Box::new(|_: &ExecutionRequest| {
        Ok(ExecutionResult {
            plot_png: Some(vec![0x89, b'P', b'N', b'G', 0, 1, 2, 3]),
            plot_svg: Some("<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>".into()),
            ..ok("")
        })
    }));
    let plain = run(&mut s, "code.run_chunk", json!({}));
    assert_eq!(plain["output"]["hasPlot"], true);
    assert!(plain["output"].get("plotSvg").is_none());
    assert!(plain["output"].get("plotPngBase64").is_none());
    let with = run(&mut s, "code.run_chunk", json!({"includePlot": true}));
    assert!(with["output"]["plotSvg"].as_str().unwrap_or_default().contains("<svg"));
    assert!(!with["output"]["plotPngBase64"].as_str().unwrap_or_default().is_empty());
}

#[test]
fn run_chunk_reports_tables_only_when_asked_and_tracks_truncation() {
    let mut s = s();
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"code": "wc_table(df)"})));
    s.chunks.set_runner(Box::new(|_: &ExecutionRequest| {
        Ok(ExecutionResult {
            output_truncated: true,
            tables: vec![TabularOutput { headers: vec!["x".into()], rows: vec![vec!["1".into()]] }],
            ..ok("truncated")
        })
    }));
    let plain = run(&mut s, "code.run_chunk", json!({}));
    assert_eq!(plain["output"]["outputTruncated"], true);
    assert_eq!(plain["output"]["tableCount"], 1);
    assert!(plain["output"].get("tables").is_none());

    let with = run(&mut s, "code.run_chunk", json!({"includeTables": true}));
    assert_eq!(with["output"]["tables"][0]["headers"], json!(["x"]));
    assert_eq!(with["output"]["tables"][0]["rows"], json!([["1"]]));
    assert_eq!(s.chunks.output(id).map(|o| o.tables.len()), Some(1));
}

#[test]
fn an_empty_chunk_is_an_error_not_a_run() {
    let mut s = s();
    run(&mut s, "code.insert_chunk", json!({}));
    let fake = fake(&mut s);
    let e = s.run("code.run_chunk", &json!({})).unwrap_err();
    assert!(matches!(e, CmdError::Failed(_)), "{e}");
    assert!(fake.seen.lock().unwrap().is_empty(), "the kernel is not asked to run nothing");
}

#[test]
fn a_language_the_kernel_cannot_run_fails_with_its_message() {
    let mut s = s();
    let mut chunk = CodeChunk::new("julia", "1 + 1");
    chunk.id = None;
    s.doc.push_chunk(&chunk).unwrap();
    caret_into(&mut s, 0);
    let e = s.run("code.run_chunk", &json!({})).unwrap_err();
    assert!(matches!(e, CmdError::Failed(_)), "{e}");
    assert!(e.to_string().contains("not supported"), "{e}");
}

#[test]
fn run_all_runs_every_chunk_in_document_order() {
    let mut s = s();
    run(&mut s, "code.insert_chunk", json!({"code": "first"}));
    run(&mut s, "code.insert_chunk", json!({"code": "second"}));
    run(&mut s, "code.insert_chunk", json!({"language": "python", "code": "third"}));
    let fake = fake(&mut s);
    let out = run(&mut s, "code.run_all", json!({}));
    let codes: Vec<String> = fake.seen.lock().unwrap().iter().map(|r| r.1.clone()).collect();
    assert_eq!(codes, ["first", "second", "third"]);
    assert_eq!(out["chunks"].as_array().map(Vec::len), Some(3));
    assert_eq!(out["success"], true);
    assert_eq!(out["stopped"], false);
}

#[test]
fn run_all_skips_chunks_marked_eval_false() {
    let mut s = s();
    run(&mut s, "code.insert_chunk", json!({"code": "skipped", "options": {"eval": false}}));
    run(&mut s, "code.insert_chunk", json!({"code": "runs"}));
    let fake = fake(&mut s);
    let out = run(&mut s, "code.run_all", json!({}));
    assert_eq!(fake.seen.lock().unwrap().len(), 1);
    assert_eq!(out["chunks"][0]["chunk"]["skipped"], "eval: false");
    assert_eq!(out["chunks"][1]["output"]["stdout"], "ran runs");
    assert_eq!(out["success"], true);
}

#[test]
fn run_all_stops_at_the_first_failure_unless_told_otherwise() {
    let mut s = s();
    run(&mut s, "code.insert_chunk", json!({"code": "boom"}));
    run(&mut s, "code.insert_chunk", json!({"code": "after"}));
    let first = fake(&mut s);
    let out = run(&mut s, "code.run_all", json!({}));
    assert_eq!(first.seen.lock().unwrap().len(), 1);
    assert_eq!(out["success"], false);
    assert_eq!(out["stopped"], true);

    let second = fake(&mut s);
    let out = run(&mut s, "code.run_all", json!({"stopOnError": false}));
    assert_eq!(second.seen.lock().unwrap().len(), 2);
    assert_eq!(out["success"], false);
    assert_eq!(out["stopped"], false);
    assert_eq!(out["chunks"][1]["output"]["stdout"], "ran after");
}

#[test]
fn run_all_is_disabled_without_chunks() {
    let mut s = s();
    run(&mut s, "text.insert", json!({"text": "no code here"}));
    let e = s.run("code.run_all", &json!({})).unwrap_err();
    assert!(matches!(e, CmdError::Disabled(_)), "{e}");
    assert_eq!(s.registry.get("code.run_all").and_then(|c| (c.enabled)(&s)), Some("the document has no code chunks"));
}

#[test]
fn clear_output_clears_one_chunk_or_all_of_them() {
    let mut s = s();
    let _fake = fake(&mut s);
    let first = chunk_id(&run(&mut s, "code.insert_chunk", json!({"code": "one"})));
    run(&mut s, "code.run_chunk", json!({}));
    assert!(s.chunks.output(first).is_some());

    // The caret is still inside the first chunk: clearing it leaves the others alone.
    let out = run(&mut s, "code.clear_output", json!({}));
    assert_eq!(out["cleared"], 1);
    assert!(s.chunks.output(first).is_none());

    run(&mut s, "code.run_chunk", json!({}));
    let out = run(&mut s, "code.clear_output", json!({"all": true}));
    assert_eq!(out["cleared"], 1);
    assert!(s.chunks.output(first).is_none());
}

#[test]
fn clear_output_accepts_chunk_id_when_caret_is_elsewhere() {
    let mut s = s();
    run(&mut s, "text.insert", json!({"text": "lead"}));
    let first = chunk_id(&run(&mut s, "code.insert_chunk", json!({"code": "one"})));
    let second = chunk_id(&run(&mut s, "code.insert_chunk", json!({"code": "two"})));
    let _fake = fake(&mut s);
    run(&mut s, "code.run_all", json!({}));
    assert!(s.chunks.output(first).is_some());
    assert!(s.chunks.output(second).is_some());

    s.sel = Selection::caret(s.doc.start_of(StoryRef::Body));
    let out = run(&mut s, "code.clear_output", json!({"chunkId": first}));
    assert_eq!(out["cleared"], 1);
    assert_eq!(out["chunk"], first);
    assert!(out.get("start").is_some());
    assert!(s.chunks.output(first).is_none());
    assert!(s.chunks.output(second).is_some());

    let e = s.run("code.clear_output", &json!({"chunkId": 0})).unwrap_err();
    assert!(matches!(e, CmdError::Params(_)), "{e}");
}

#[test]
fn restart_kernel_forgets_outputs_and_bumps_the_generation() {
    let mut s = s();
    let _fake = fake(&mut s);
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"code": "x"})));
    run(&mut s, "code.run_chunk", json!({}));
    let out = run(&mut s, "code.restart_kernel", json!({}));
    assert_eq!(out["restarted"], true);
    assert_eq!(out["outputsCleared"], 1);
    assert_eq!(out["generation"], 1);
    assert_eq!(out["persistentSessions"], false);
    assert!(s.chunks.output(id).is_none());
}

#[test]
fn opening_another_document_forgets_chunk_outputs() {
    let mut s = s();
    let _fake = fake(&mut s);
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"code": "x"})));
    run(&mut s, "code.run_chunk", json!({}));
    assert!(s.chunks.output(id).is_some());
    s.set_document(wordcraft_doc::Document::new());
    assert!(s.chunks.output(id).is_none());
}

#[test]
fn undo_clears_stale_output_when_chunk_is_removed_or_id_reused() {
    let mut s = s();
    let _fake = fake(&mut s);
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"code": "first"})));
    run(&mut s, "code.run_chunk", json!({}));
    assert!(s.chunks.output(id).is_some());

    // An unrelated edit + Undo keeps the live chunk's output intact.
    s.sel = Selection::caret(s.doc.start_of(StoryRef::Body));
    run(&mut s, "text.insert", json!({"text": "note"}));
    assert!(s.undo());
    assert!(s.chunks.output(id).is_some());

    // Undoing the chunk insertion drops its output so a reused control id cannot inherit it.
    assert!(s.undo());
    assert!(s.chunks.output(id).is_none());
    let reused_id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"code": "first"})));
    assert_eq!(reused_id, id);
    assert!(s.chunks.output(reused_id).is_none());
}

#[test]
fn with_document_preserves_chunk_outputs_across_open_documents() {
    let mut s = s();
    let _fake = fake(&mut s);
    let home = s.document_id();
    let home_chunk = chunk_id(&run(&mut s, "code.insert_chunk", json!({"code": "home"})));
    run(&mut s, "code.run_chunk", json!({}));
    assert_eq!(s.chunks.output(home_chunk).map(|o| o.stdout.as_str()), Some("ran home"));

    let other = s.add_document(wordcraft_doc::Document::new());
    let other_chunk = chunk_id(&run(&mut s, "code.insert_chunk", json!({"code": "other"})));
    run(&mut s, "code.run_chunk", json!({}));
    assert_eq!(s.chunks.output(other_chunk).map(|o| o.stdout.as_str()), Some("ran other"));

    s.activate(home).unwrap();
    assert_eq!(s.chunks.output(home_chunk).map(|o| o.stdout.as_str()), Some("ran home"));

    s.with_document(other, |s| {
        assert_eq!(s.chunks.output(other_chunk).map(|o| o.stdout.as_str()), Some("ran other"));
        run(s, "code.clear_output", json!({"chunkId": other_chunk}));
        assert!(s.chunks.output(other_chunk).is_none());
    })
    .unwrap();
    assert_eq!(s.chunks.output(home_chunk).map(|o| o.stdout.as_str()), Some("ran home"));
}

#[test]
fn execution_is_refused_under_automation_confinement() {
    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!("wc-code-confine-{}-{n}", std::process::id()));
    std::fs::create_dir(&base).unwrap();
    let _guard = TempDir(base.clone());

    let roots = crate::file_access::AutomationRoots::new(Some(&base), Some(&base)).unwrap();
    let mut s = s();
    let fake = fake(&mut s);
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"code": "print(1)"})));

    crate::file_access::confine(roots.as_ref(), || {
        assert!(s.why_disabled("code.run_chunk").is_some());
        assert!(s.why_disabled("code.run_all").is_some());
        assert!(matches!(s.run("code.run_chunk", &json!({"chunkId": id})), Err(CmdError::Disabled(_))));
        assert!(matches!(s.run("code.run_all", &json!({})), Err(CmdError::Disabled(_))));
    });
    assert!(fake.seen.lock().unwrap().is_empty(), "confined session must never invoke the runner");
}

#[test]
fn a_chunk_and_its_options_survive_a_docx_round_trip() {
    let mut s = s();
    run(
        &mut s,
        "code.insert_chunk",
        json!({"language": "python", "code": "print('hi')\nprint('bye')", "options": {"eval": false, "fig-cap": "A plot"}}),
    );
    let bytes = crate::io::save_bytes("chunk.docx", &s.doc).unwrap_or_else(|e| panic!("save: {e}"));
    let reopened = crate::io::open_bytes("chunk.docx", &bytes).unwrap_or_else(|e| panic!("open: {e}"));
    let refs = chunks::chunks_in(&reopened, StoryRef::Body);
    assert_eq!(refs.len(), 1, "one chunk after the round trip");
    assert_eq!(refs[0].chunk.tag(), "wordcraft:chunk:python");
    assert_eq!(refs[0].chunk.code, "print('hi')\nprint('bye')");
    assert_eq!(refs[0].chunk.options.flag("eval"), Some(false));
    assert_eq!(refs[0].chunk.options.fig_caption().as_deref(), Some("A plot"));
    assert!(reopened.control_ranges(StoryRef::Body)[0].control.tag.starts_with("wordcraft:chunk:python?"));
}

#[test]
fn run_all_skips_copied_eval_false_chunk_with_fresh_id_and_no_property() {
    let mut s = s();
    let old_id =
        chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "r", "code": "danger()", "options": {"eval": false, "fig-cap": "Kept"}})));

    // Simulate Word copying the chunk control: fresh w:id, no custom property for the new id.
    let start = s.doc.control_ranges(StoryRef::Body)[0].start.clone();
    s.doc.control_mut(&start).unwrap().id = Some(old_id + 42);
    s.doc.remove_custom_prop(&wordcraft_doc::chunk::chunk_prop_name(old_id));

    let bytes = crate::io::save_bytes("copied.docx", &s.doc).unwrap();
    let reopened = crate::io::open_bytes("copied.docx", &bytes).unwrap();
    let mut s2 = Session::new(reopened);
    let fake = fake(&mut s2);
    let out = run(&mut s2, "code.run_all", json!({}));
    assert!(fake.seen.lock().unwrap().is_empty(), "copied eval: false chunk must never execute");
    assert_eq!(out["chunks"][0]["chunk"]["skipped"], "eval: false");
    assert_eq!(out["success"], true);
}

#[test]
fn run_all_refuses_tagged_chunk_whose_options_are_missing() {
    let mut s = s();
    let old_id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "r", "code": "danger()", "options": {"eval": false}})));
    run(&mut s, "code.insert_chunk", json!({"language": "r", "code": "safe()"}));

    // Simulate copying a legacy chunk whose tag lacks `?options` and whose property is missing.
    let start = s.doc.control_ranges(StoryRef::Body)[0].start.clone();
    let control = s.doc.control_mut(&start).unwrap();
    control.tag = "wordcraft:chunk:r".to_string();
    control.id = Some(old_id + 100);
    s.doc.remove_custom_prop(&wordcraft_doc::chunk::chunk_prop_name(old_id));

    let fake = fake(&mut s);
    let out = run(&mut s, "code.run_all", json!({}));
    assert!(fake.seen.lock().unwrap().is_empty(), "chunk with missing options must never execute");
    assert_eq!(out["success"], false);
    assert_eq!(out["stopped"], true);
    assert_eq!(out["chunks"][0]["chunk"]["success"], false);
    assert!(out["chunks"][0]["error"].as_str().is_some_and(|e| e.contains("options are missing")));
}

#[test]
fn run_all_runs_copied_empty_options_chunk_with_fresh_id() {
    let mut s = s();
    let old_id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "python", "code": "print(1)"})));
    let start = s.doc.control_ranges(StoryRef::Body)[0].start.clone();
    s.doc.control_mut(&start).unwrap().id = Some(old_id + 25);
    s.doc.remove_custom_prop(&wordcraft_doc::chunk::chunk_prop_name(old_id));

    let fake = fake(&mut s);
    let out = run(&mut s, "code.run_all", json!({}));
    assert_eq!(fake.seen.lock().unwrap().len(), 1);
    assert_eq!(out["success"], true);
    assert_eq!(out["chunks"][0]["output"]["stdout"], "ran print(1)");
}

#[test]
fn run_all_skips_copied_exact_budget_eval_false_chunk_with_echo() {
    let mut s = s();
    let eval_exact = format!("false #{}", "x".repeat(990));
    let old_id =
        chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "r", "code": "danger()", "options": {"eval": eval_exact, "echo": false}})));

    let start = s.doc.control_ranges(StoryRef::Body)[0].start.clone();
    s.doc.control_mut(&start).unwrap().id = Some(old_id + 77);
    s.doc.remove_custom_prop(&wordcraft_doc::chunk::chunk_prop_name(old_id));

    let fake = fake(&mut s);
    let out = run(&mut s, "code.run_all", json!({}));
    assert!(fake.seen.lock().unwrap().is_empty(), "exact-budget copied eval: false chunk must never execute");
    assert_eq!(out["chunks"][0]["chunk"]["skipped"], "eval: false");
    assert_eq!(out["success"], true);
}

#[test]
fn update_chunk_modifies_code_and_language_and_supports_undo_redo() {
    let mut s = s();
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "r", "code": "a <- 1\nb <- 2"})));
    let v = run(&mut s, "code.update_chunk", json!({"chunkId": id, "language": "python", "code": "x = 10"}));
    assert_eq!(v["changed"], true);
    assert_eq!(v["chunk"]["language"], "python");
    assert_eq!(chunks::chunks_in(&s.doc, StoryRef::Body)[0].chunk.code, "x = 10");

    // Undo restores previous code and language cleanly.
    run(&mut s, "edit.undo", json!({}));
    let restored = &chunks::chunks_in(&s.doc, StoryRef::Body)[0].chunk;
    assert_eq!(restored.language, "r");
    assert_eq!(restored.code, "a <- 1\nb <- 2");

    // Redo re-applies the updated code.
    run(&mut s, "edit.redo", json!({}));
    assert_eq!(chunks::chunks_in(&s.doc, StoryRef::Body)[0].chunk.code, "x = 10");
}

#[test]
fn delete_chunk_removes_control_and_clears_cached_output() {
    let mut s = s();
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "python", "code": "print(1)"})));
    s.chunks.store_for(id, "python", "print(1)", chunks::ChunkOutput { stdout: "1\n".into(), success: true, ..Default::default() });
    let v = run(&mut s, "code.delete_chunk", json!({"chunkId": id}));
    assert_eq!(v["deleted"], true);
    assert!(chunks::chunks_in(&s.doc, StoryRef::Body).is_empty());
    assert!(s.chunks.output(id).is_none());
    assert!(!s.doc.body.is_empty(), "document body must keep at least one paragraph");

    run(&mut s, "edit.undo", json!({}));
    assert_eq!(chunks::chunks_in(&s.doc, StoryRef::Body).len(), 1);
}

#[test]
fn insert_plot_and_insert_table_place_content_below_chunk() {
    let mut s = s();
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "r", "code": "plot(1:3)"})));
    run(
        &mut s,
        "code.insert_table",
        json!({
            "chunkId": id,
            "headers": ["x", "y"],
            "rows": [["1", "2"], ["3", "4"]],
        }),
    );
    // Chunk is block 0; table sits immediately below it at block 1.
    let wordcraft_doc::Block::Table(tbl) = &*s.doc.body[1] else { panic!("expected table at block 1") };
    assert_eq!(tbl.rows.len(), 3);
    assert!(tbl.rows[0].props.header);

    // Valid 1x1 PNG encoded as base64 + optional SVG archive.
    let png = [
        0x89u8, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n', 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15,
        0xC4, 0x89, 0, 0, 0, 10, b'I', b'D', b'A', b'T', 0x78, 0x9C, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0D, 0x0A, 0x2D, 0xB4, 0, 0, 0, 0, b'I', b'E', b'N',
        b'D', 0xAE, 0x42, 0x60, 0x82,
    ];
    let b64 = crate::cmd::insert::base64_encode(&png);
    let res = run(
        &mut s,
        "code.insert_plot",
        json!({
            "chunkId": id,
            "png": b64,
            "svg": "<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
        }),
    );
    assert!(res["media"].as_str().is_some());
    assert!(res.get("svgMedia").is_none());
    let has_img = s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(1)).is_some_and(|p| !p.objects.is_empty());
    assert!(has_img, "inserted plot paragraph should sit directly below chunk at block 1");
}

#[test]
fn insert_table_and_plot_validate_before_mutating_on_bad_input() {
    let mut s = s();
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "python", "code": "pass"})));
    let before = s.doc.clone();
    assert!(matches!(s.run("code.insert_table", &json!({"chunkId": id, "headers": []})), Err(CmdError::Params(_))));
    assert!(matches!(s.run("code.insert_plot", &json!({"chunkId": id, "png": "not-valid-base64!!!"})), Err(CmdError::Params(_))));
    // Valid GIF image bytes, but not PNG: must return CmdError::Params without mutating doc.
    let gif_b64 = crate::cmd::insert::base64_encode(b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;");
    assert!(matches!(s.run("code.insert_plot", &json!({"chunkId": id, "png": gif_b64})), Err(CmdError::Params(_))));
    assert_eq!(s.doc, before, "rejected plot/table must leave document untouched");
}

#[test]
fn delete_chunk_followed_by_table_moves_caret_to_first_cell_paragraph() {
    let mut s = s();
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "python", "code": "x = 1"})));
    run(&mut s, "code.insert_table", json!({"chunkId": id, "headers": ["a", "b"], "rows": [["1", "2"]]}));
    assert!(matches!(&*s.doc.body[1], wordcraft_doc::Block::Table(_)));

    run(&mut s, "code.delete_chunk", json!({"chunkId": id}));
    assert!(matches!(&*s.doc.body[0], wordcraft_doc::Block::Table(_)));
    assert_eq!(s.sel.focus, wordcraft_doc::Pos::new(StoryRef::Body, wordcraft_doc::Path(vec![0, 0, 0, 0]), 0));
}

#[test]
fn delete_chunk_after_table_preserves_trailing_section_break() {
    let mut s = s();
    s.doc.body = vec![
        std::sync::Arc::new(wordcraft_doc::Block::Table(wordcraft_doc::Table::new(1, 1, 468.0))),
        wordcraft_doc::para_block(wordcraft_doc::Paragraph::new()),
    ];
    s.sel = Selection::caret(wordcraft_doc::Pos::body(1, 0));
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "python", "code": "b = 2"})));
    let range = chunks::chunks_in(&s.doc, StoryRef::Body).into_iter().find(|r| r.chunk.id == Some(id)).expect("chunk");
    let first = range.start.path.last();
    let end_block = range.end.path.last();
    if let Some(wordcraft_doc::Block::Para(p)) = s.doc.body.get_mut(end_block).map(std::sync::Arc::make_mut) {
        p.section = Some(Box::new(wordcraft_doc::SectionProps::default()));
    }

    run(&mut s, "code.delete_chunk", json!({"chunkId": id}));
    let Some(wordcraft_doc::Block::Para(retained)) = s.doc.body.get(first).map(|b| &**b) else {
        panic!("expected a blank paragraph carrying the preserved section break at index {first} after the table");
    };
    assert!(retained.section.is_some(), "trailing section break must not be lost when preceded by a table");
}

#[test]
fn chunk_commands_refuse_nested_table_cell_chunks() {
    let mut s = s();
    let cell_blocks = s.doc.chunk_blocks(&CodeChunk::new("python", "nested = True")).unwrap();
    let nested_id = match &*cell_blocks[0] {
        wordcraft_doc::Block::Para(p) => match &p.objects[0] {
            wordcraft_doc::InlineObject::ControlStart { control } => control.id.unwrap(),
            _ => panic!("expected control start"),
        },
        _ => panic!("expected para"),
    };
    let mut tbl = wordcraft_doc::Table::new(1, 1, 468.0);
    tbl.rows[0].cells[0].blocks = cell_blocks;
    s.doc.body.push(std::sync::Arc::new(wordcraft_doc::Block::Table(tbl)));
    let before = s.doc.clone();

    assert!(matches!(s.run("code.delete_chunk", &json!({"chunkId": nested_id})), Err(CmdError::Disabled(_))));
    assert!(matches!(
        s.run("code.insert_plot", &json!({"chunkId": nested_id, "png": crate::cmd::insert::base64_encode(b"\x89PNG\r\n\x1a\n")})),
        Err(CmdError::Disabled(_))
    ));
    assert!(matches!(s.run("code.insert_table", &json!({"chunkId": nested_id, "headers": ["h"], "rows": [["v"]]})), Err(CmdError::Disabled(_))));
    assert_eq!(s.doc, before, "nested table-cell chunks must not mutate body blocks");
}

#[test]
fn insert_plot_without_explicit_png_rejects_stale_cached_plot() {
    let mut s = s();
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "python", "code": "plot(1)"})));
    let png = vec![
        0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n', 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15,
        0xC4, 0x89, 0, 0, 0, 10, b'I', b'D', b'A', b'T', 0x78, 0x9C, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0D, 0x0A, 0x2D, 0xB4, 0, 0, 0, 0, b'I', b'E', b'N',
        b'D', 0xAE, 0x42, 0x60, 0x82,
    ];
    s.chunks.store_for(id, "python", "plot(1)", chunks::ChunkOutput { plot_png: Some(png), success: true, ..chunks::ChunkOutput::default() });
    run(&mut s, "code.update_chunk", json!({"chunkId": id, "language": "python", "code": "plot(2)"}));

    assert!(
        matches!(s.run("code.insert_plot", &json!({"chunkId": id})), Err(CmdError::Failed(_))),
        "unpinned insert_plot must reject cached output when chunk source changed since last run"
    );
}

#[test]
fn untargeted_insert_table_uses_caret_story_and_rejects_excessive_rows() {
    let mut s = s();
    let pid = s.doc.add_part(
        wordcraft_doc::PartKind::Header,
        vec![wordcraft_doc::para_block(wordcraft_doc::Paragraph::with_text("Header", Default::default()))],
    );
    s.sel = Selection::caret(wordcraft_doc::Pos::new(StoryRef::Part(pid), wordcraft_doc::Path::top(0), 0));
    run(&mut s, "code.insert_table", json!({"headers": ["a", "b"], "rows": [["1", "2"]]}));

    assert!(
        s.doc.para(StoryRef::Part(pid), &wordcraft_doc::Path(vec![1, 0, 0, 0])).is_some_and(|p| p.text == "a"),
        "table must be inserted into the active header story"
    );
    assert_eq!(s.doc.body.len(), 1, "document body must stay untouched when caret is in a header");

    let oversized_rows: Vec<Vec<&str>> = vec![vec!["1"]; wordcraft_doc::table::MAX_ROWS];
    assert!(matches!(s.run("code.insert_table", &json!({"headers": ["a"], "rows": oversized_rows})), Err(CmdError::Params(_))));
}

#[test]
fn update_chunk_clamps_caret_on_utf8_boundary() {
    let mut s = s();
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "python", "code": "abcdef"})));
    let range = chunks::chunks_in(&s.doc, StoryRef::Body).into_iter().find(|r| r.chunk.id == Some(id)).expect("chunk");
    // Paragraph text has a leading 3-byte U+FFFC marker: "\u{FFFC}z😀" has 'z' at 3..4 and '😀' at 4..8.
    // Offset 6 is valid in "\u{FFFC}abcdef\u{FFFC}" and lands strictly inside '😀' after replacement.
    s.sel = Selection::caret(wordcraft_doc::Pos::new(StoryRef::Body, range.start.path.clone(), 6));
    let out = run(&mut s, "code.update_chunk", json!({"chunkId": id, "language": "python", "code": "z😀"}));
    let returned_off = out["focus"]["off"].as_u64().expect("returned focus offset") as usize;
    let p = s.doc.para(StoryRef::Body, &range.start.path).expect("chunk source paragraph");
    assert!(p.text.is_char_boundary(returned_off), "command-returned caret offset {returned_off} must land on a UTF-8 boundary of {:?}", p.text);
    assert!(p.text.is_char_boundary(s.sel.focus.off), "remapped caret offset {} must land on a UTF-8 boundary of {:?}", s.sel.focus.off, p.text);
}

#[test]
fn new_paragraph_at_trailing_chunk_end_exits_below_chunk_instead_of_expanding_it() {
    let mut s = s();
    run(&mut s, "text.insert", json!({"text": "Above"}));
    run(&mut s, "code.insert_chunk", json!({"language": "r", "code": "x <- 1"}));
    assert_eq!(s.doc.body.len(), 2, "chunk after non-empty prose ends the body with no trailing paragraph");
    caret_into(&mut s, 0);
    assert!(chunks::chunk_at(&s.doc, &s.sel.focus).is_some());

    run(&mut s, "text.newParagraph", json!({}));
    assert!(chunks::chunk_at(&s.doc, &s.sel.focus).is_none(), "Enter at chunk end must leave the code chunk control");
    run(&mut s, "text.insert", json!({"text": "After"}));

    let refs = chunks::chunks_in(&s.doc, StoryRef::Body);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].chunk.code, "x <- 1", "Enter must not split or grow the chunk infinitely");
    assert_eq!(s.doc.body.len(), 3);
    assert_eq!(s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(2)).map(|p| p.text.as_str()), Some("After"));
}

#[test]
fn new_paragraph_at_chunk_end_tracks_insertion_and_reject_removes_it() {
    // 1. Trailing chunk at the end of the body: rejectAll, single reject, and acceptAll.
    let mut s = s();
    run(&mut s, "text.insert", json!({"text": "Above"}));
    run(&mut s, "code.insert_chunk", json!({"language": "r", "code": "x <- 1"}));
    run(&mut s, "file.setAuthor", json!({"name": "Alice"}));
    run(&mut s, "review.trackChanges", json!({"value": true}));
    caret_into(&mut s, 0);

    run(&mut s, "text.newParagraph", json!({}));
    assert_eq!(s.doc.body.len(), 3, "Enter at chunk end inserts a trailing blank paragraph");
    assert!(chunks::chunk_at(&s.doc, &s.sel.focus).is_none());
    let chunk_end = s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(1)).expect("chunk paragraph");
    let inserted = s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(2)).expect("new paragraph");
    assert!(chunk_end.mark.ins.is_some(), "the inserted paragraph break must be recorded on the chunk-end mark");
    assert_eq!(inserted.mark.ins, None, "the trailing paragraph keeps the original mark");

    let ch = run(&mut s, "review.changes", json!({}));
    assert_eq!(ch.as_array().map(Vec::len), Some(1), "{ch}");
    assert_eq!((ch[0]["kind"].as_str(), ch[0]["author"].as_str()), (Some("insert"), Some("Alice")));

    // Single-change navigation + reject removes the inserted paragraph even at the end of the body.
    run(&mut s, "review.nextChange", json!({}));
    run(&mut s, "review.reject", json!({}));
    assert_eq!(s.doc.body.len(), 2, "rejecting the tracked chunk-end Enter must remove the inserted paragraph");
    assert_eq!(chunks::chunks_in(&s.doc, StoryRef::Body)[0].chunk.code, "x <- 1");
    assert_eq!(s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(1)).and_then(|p| p.mark.ins), None);

    // Typing tracked text into the new paragraph and running rejectAll removes both text and paragraph.
    caret_into(&mut s, 0);
    run(&mut s, "text.newParagraph", json!({}));
    run(&mut s, "text.insert", json!({"text": "After"}));
    assert_eq!(s.doc.body.len(), 3);
    run(&mut s, "review.rejectAll", json!({}));
    assert_eq!(s.doc.body.len(), 2, "rejectAll must remove the inserted paragraph and its tracked text");
    assert!(s.doc.revisions.is_empty());
    assert_eq!(chunks::chunks_in(&s.doc, StoryRef::Body)[0].chunk.code, "x <- 1");

    // Accepting keeps the inserted paragraph and clears the revision mark.
    caret_into(&mut s, 0);
    run(&mut s, "text.newParagraph", json!({}));
    run(&mut s, "review.acceptAll", json!({}));
    assert_eq!(s.doc.body.len(), 3);
    assert!(s.doc.revisions.is_empty());
    assert!(s.doc.body.iter().filter_map(|b| b.as_para()).all(|p| p.mark.ins.is_none() && p.mark.del.is_none()));
}

#[test]
fn new_paragraph_at_chunk_end_rejects_cleanly_before_paragraph_table_and_section_break() {
    // Followed by a normal paragraph (`insert_chunk` on a blank doc leaves `[Chunk, ""]`).
    let mut s1 = s();
    run(&mut s1, "code.insert_chunk", json!({"language": "r", "code": "a <- 1"}));
    s1.sel = Selection::caret(wordcraft_doc::Pos::body(1, 0));
    run(&mut s1, "text.insert", json!({"text": "Below"}));
    assert_eq!(s1.doc.body.len(), 2);
    run(&mut s1, "review.trackChanges", json!({"value": true}));
    caret_into(&mut s1, 0);
    run(&mut s1, "text.newParagraph", json!({}));
    assert_eq!(s1.doc.body.len(), 3);
    run(&mut s1, "review.rejectAll", json!({}));
    assert_eq!(s1.doc.body.len(), 2);
    assert_eq!(chunks::chunks_in(&s1.doc, StoryRef::Body)[0].chunk.code, "a <- 1");
    assert_eq!(s1.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(1)).map(|p| p.text.as_str()), Some("Below"));

    // Followed by a table (where a trailing-mark revision on the inserted paragraph cannot join).
    let mut s2 = s();
    let id = chunk_id(&run(&mut s2, "code.insert_chunk", json!({"language": "python", "code": "x = 1"})));
    run(&mut s2, "code.insert_table", json!({"chunkId": id, "headers": ["h"], "rows": [["1"]]}));
    assert!(matches!(&*s2.doc.body[1], wordcraft_doc::Block::Table(_)));
    let before_len = s2.doc.body.len();
    run(&mut s2, "review.trackChanges", json!({"value": true}));
    caret_into(&mut s2, 0);
    run(&mut s2, "text.newParagraph", json!({}));
    assert_eq!(s2.doc.body.len(), before_len + 1);
    run(&mut s2, "review.rejectAll", json!({}));
    assert_eq!(s2.doc.body.len(), before_len, "rejectAll must remove the blank paragraph inserted before a table");
    assert!(matches!(&*s2.doc.body[1], wordcraft_doc::Block::Table(_)));

    // Trailing chunk carrying a section break: rejecting the Enter restores the section break.
    let mut s3 = s();
    run(&mut s3, "text.insert", json!({"text": "Above"}));
    run(&mut s3, "code.insert_chunk", json!({"language": "r", "code": "b <- 2"}));
    assert_eq!(s3.doc.body.len(), 2);
    if let Some(wordcraft_doc::Block::Para(p)) = s3.doc.body.get_mut(1).map(std::sync::Arc::make_mut) {
        p.section = Some(Box::new(wordcraft_doc::SectionProps::default()));
    }
    run(&mut s3, "review.trackChanges", json!({"value": true}));
    caret_into(&mut s3, 0);
    run(&mut s3, "text.newParagraph", json!({}));
    assert_eq!(s3.doc.body.len(), 3);
    assert!(s3.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(2)).is_some_and(|p| p.section.is_some()));
    run(&mut s3, "review.rejectAll", json!({}));
    assert_eq!(s3.doc.body.len(), 2);
    assert!(
        s3.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(1)).is_some_and(|p| p.section.is_some()),
        "rejecting a tracked chunk-end Enter must preserve the chunk's trailing section break"
    );

    // `code.exit_below_chunk` tracks the insertion only when it actually inserts a trailing paragraph.
    let mut s4 = s();
    run(&mut s4, "text.insert", json!({"text": "Above"}));
    let cid = chunk_id(&run(&mut s4, "code.insert_chunk", json!({"language": "r", "code": "c <- 3"})));
    assert_eq!(s4.doc.body.len(), 2);
    run(&mut s4, "review.trackChanges", json!({"value": true}));
    run(&mut s4, "code.exit_below_chunk", json!({"chunkId": cid}));
    assert_eq!(s4.doc.body.len(), 3);
    assert_eq!(s4.doc.revisions.len(), 1);
    run(&mut s4, "review.rejectAll", json!({}));
    assert_eq!(s4.doc.body.len(), 2, "rejectAll must remove the trailing paragraph created by code.exit_below_chunk");
    assert!(s4.doc.revisions.is_empty());
    // When a paragraph already follows, `code.exit_below_chunk` only moves the caret and records no revision.
    run(&mut s4, "review.trackChanges", json!({"value": false}));
    run(&mut s4, "code.exit_below_chunk", json!({"chunkId": cid}));
    assert_eq!(s4.doc.body.len(), 3);
    run(&mut s4, "review.trackChanges", json!({"value": true}));
    caret_into(&mut s4, 0);
    run(&mut s4, "code.exit_below_chunk", json!({"chunkId": cid}));
    assert_eq!(s4.doc.body.len(), 3);
    assert!(s4.doc.revisions.is_empty(), "moving the caret to an existing block must not record a revision");
}

#[test]
fn insert_at_caret_insert_output_and_chunk_label_commands() {
    let mut s = s();
    run(&mut s, "text.insert", json!({"text": "Intro"}));
    run(&mut s, "text.newParagraph", json!({}));
    run(&mut s, "text.insert", json!({"text": "Here target"}));
    run(&mut s, "text.newParagraph", json!({}));
    let cid = chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "r", "code": "cat('ok\\nline2')"})));

    // Rename chunk via `code.update_chunk` with `label`, verify persistence + clearing.
    let updated = run(&mut s, "code.update_chunk", json!({"chunkId": cid, "label": "Tabela de Dados"}));
    assert_eq!(updated["chunk"]["label"], "Tabela de Dados");
    assert_eq!(chunks::chunks_in(&s.doc, StoryRef::Body)[0].chunk.options.label(), Some("Tabela de Dados"));
    assert_eq!(s.doc.control_ranges(StoryRef::Body)[0].control.title, "R code chunk (Tabela de Dados)");

    // Cache console output + two plots on chunk.
    let png1 = vec![
        0x89u8, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n', 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15,
        0xC4, 0x89, 0, 0, 0, 10, b'I', b'D', b'A', b'T', 0x78, 0x9C, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0D, 0x0A, 0x2D, 0xB4, 0, 0, 0, 0, b'I', b'E', b'N',
        b'D', 0xAE, 0x42, 0x60, 0x82,
    ];
    let mut out = crate::chunks::ChunkOutput::from_result(ExecutionResult {
        stdout: "ok\nline2\n".into(),
        plots: vec![
            wordcraft_kernel::PlotOutput { png: Some(png1.clone()), svg: None },
            wordcraft_kernel::PlotOutput { png: Some(png1.clone()), svg: None },
        ],
        ..ok("ok\nline2\n")
    });
    out.stdout = "ok\nline2\n".into();
    s.chunks.store_for(cid, "r", "cat('ok\\nline2')", out);

    // Move caret to paragraph 1 ("Here target") before the chunk, and insert table + output at caret.
    s.sel = Selection::caret(wordcraft_doc::Pos::body(1, 0));
    let tbl_res = run(&mut s, "code.insert_table", json!({"chunkId": cid, "headers": ["A"], "rows": [["1"]], "atCaret": true}));
    assert_eq!(tbl_res["table"], json!([2]), "table must be inserted after caret block 1, not below chunk");

    // Put caret on block 0 ("Intro") and insert console output at caret as code-styled paragraphs.
    s.sel = Selection::caret(wordcraft_doc::Pos::body(0, 5));
    let out_res = run(&mut s, "code.insert_output", json!({"chunkId": cid, "atCaret": true}));
    assert_eq!(out_res["lines"], json!(2));
    assert_eq!(out_res["path"], json!([1]));
    let p1 = s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(1)).expect("inserted output line 1");
    let p2 = s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(2)).expect("inserted output line 2");
    assert_eq!(p1.text, "ok");
    assert_eq!(p2.text, "line2");
    assert_eq!(p1.props.style.as_deref(), Some(wordcraft_doc::chunk::CODE_OUTPUT_STYLE));
    assert_eq!(chunks::chunks_in(&s.doc, StoryRef::Body).len(), 1, "inserted output must not become an executable chunk");

    // Second plot via `plotIndex: 1` at caret outside chunk.
    let plot_res = run(&mut s, "code.insert_plot", json!({"chunkId": cid, "plotIndex": 1, "atCaret": true}));
    assert!(plot_res["media"].as_str().is_some());
    assert!(
        s.doc.para(StoryRef::Body, &s.sel.focus.path).is_some_and(|p| !p.objects.is_empty()),
        "second plot must insert at the current caret paragraph outside the chunk"
    );
}

#[test]
fn insert_output_into_empty_paragraph_preserves_section_break_and_output_style() {
    let mut s = s();
    let landscape = wordcraft_doc::SectionProps { landscape: true, ..Default::default() };
    s.doc.body = vec![
        std::sync::Arc::new(wordcraft_doc::Block::Table(wordcraft_doc::Table::new(1, 1, 468.0))),
        wordcraft_doc::para_block(wordcraft_doc::Paragraph::new()),
        wordcraft_doc::para_block(wordcraft_doc::Paragraph::with_text("Next section", Default::default())),
    ];
    s.sel = Selection::caret(wordcraft_doc::Pos::body(1, 0));
    let id = chunk_id(&run(&mut s, "code.insert_chunk", json!({"language": "python", "code": "b = 2"})));
    let range = chunks::chunks_in(&s.doc, StoryRef::Body).into_iter().find(|r| r.chunk.id == Some(id)).expect("chunk");
    let first = range.start.path.last();
    let end_block = range.end.path.last();
    if let Some(wordcraft_doc::Block::Para(p)) = s.doc.body.get_mut(end_block).map(std::sync::Arc::make_mut) {
        p.section = Some(Box::new(landscape.clone()));
    }

    // Deleting a section-ending chunk after a table leaves an empty paragraph carrying the break.
    run(&mut s, "code.delete_chunk", json!({"chunkId": id}));
    assert_eq!(s.sel.focus, wordcraft_doc::Pos::body(first, 0));
    assert_eq!(s.doc.sections().len(), 2);

    // Single-line output into that empty paragraph keeps both the section break and output style.
    run(&mut s, "code.insert_output", json!({"text": "single output"}));
    let p = s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(first)).expect("single output paragraph");
    assert_eq!(p.text, "single output");
    assert_eq!(p.props.style.as_deref(), Some(wordcraft_doc::chunk::CODE_OUTPUT_STYLE));
    assert_eq!(p.section.as_deref(), Some(&landscape), "section break must survive replacing the empty paragraph");
    assert_eq!(s.doc.sections().len(), 2, "surrounding sections must not merge");
    assert_eq!(s.doc.section_index_of(first), 0);
    assert_eq!(s.doc.section_index_of(first + 1), 1);

    // Multi-line output keeps every output line in the original section before the break.
    run(&mut s, "edit.undo", json!({}));
    run(&mut s, "code.insert_output", json!({"text": "line 1\nline 2"}));
    let line1 = s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(first)).expect("first output line");
    let line2 = s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(first + 1)).expect("second output line");
    assert_eq!(line1.text, "line 1");
    assert_eq!(line1.props.style.as_deref(), Some(wordcraft_doc::chunk::CODE_OUTPUT_STYLE));
    assert!(line1.section.is_none(), "break must not split multi-line output across sections");
    assert_eq!(line2.text, "line 2");
    assert_eq!(line2.props.style.as_deref(), Some(wordcraft_doc::chunk::CODE_OUTPUT_STYLE));
    assert_eq!(line2.section.as_deref(), Some(&landscape), "last output line must carry the preserved section break");
    assert_eq!(s.doc.sections().len(), 2);
    assert_eq!(s.doc.section_index_of(first), 0);
    assert_eq!(s.doc.section_index_of(first + 1), 0);
    assert_eq!(s.doc.section_index_of(first + 2), 1);

    // Ordinary empty paragraphs without a section break still take the output style and stay break-free.
    let mut plain = super::s();
    run(&mut plain, "code.insert_output", json!({"text": "plain output"}));
    let ordinary = plain.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(0)).expect("ordinary output paragraph");
    assert_eq!(ordinary.text, "plain output");
    assert_eq!(ordinary.props.style.as_deref(), Some(wordcraft_doc::chunk::CODE_OUTPUT_STYLE));
    assert!(ordinary.section.is_none());
}
