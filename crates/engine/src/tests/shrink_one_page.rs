use serde_json::json;
use wordcraft_doc::Document;
use wordcraft_doc::para::PAGE_BREAK;

use crate::Session;

/// Creates a document with `lines` lines of text.
fn create_multiline_doc(lines: usize) -> Session {
    let mut text = String::new();
    for i in 1..=lines {
        text.push_str(&format!("Line {i}: The quick brown fox jumps over the lazy dog.\n"));
    }
    Session::new(Document::from_text(&text))
}

#[test]
fn shrink_one_page_fails_on_single_page() {
    let mut s = create_multiline_doc(5);
    let before = s.doc.clone();
    let res = s.run("layout.shrinkOnePage", &json!({}));
    assert!(res.is_err());
    let err = res.unwrap_err().to_string();
    assert!(err.contains("already fits on one page"), "unexpected error: {err}");
    assert_eq!(s.doc, before, "document must not change on failure");
}

#[test]
fn shrink_one_page_fails_on_hard_page_break() {
    let text = format!("Page 1 content\n{PAGE_BREAK}Page 2 content\n");
    let mut s = Session::new(Document::from_text(&text));
    let before = s.doc.clone();
    let res = s.run("layout.shrinkOnePage", &json!({}));
    assert!(res.is_err());
    let err = res.unwrap_err().to_string();
    assert!(err.contains("explicit page or section breaks"), "unexpected error: {err}");
    assert_eq!(s.doc, before, "document must not change on failure");
}

#[test]
fn shrink_one_page_fails_on_next_page_section_break() {
    let mut s = Session::new(Document::from_text("First section body text"));
    s.run("layout.break", &json!({"kind": "nextPage"})).unwrap();
    s.run("text.insert", &json!({"text": "Second section text on next page"})).unwrap();
    assert_eq!(s.layout().pages.len(), 2, "break must cause 2 pages");

    let before = s.doc.clone();
    let res = s.run("layout.shrinkOnePage", &json!({}));
    assert!(res.is_err());
    let err = res.unwrap_err().to_string();
    assert!(err.contains("explicit page or section breaks"), "unexpected error: {err}");
    assert_eq!(s.doc, before, "document must not change on failure");
}

#[test]
fn shrink_one_page_condenses_slight_overflow_and_undoes() {
    // Find the boundary where the document just spills onto page 2
    let mut lines = 30;
    let mut s = create_multiline_doc(lines);
    while s.layout().pages.len() < 2 && lines < 80 {
        lines += 2;
        s = create_multiline_doc(lines);
    }
    assert_eq!(s.layout().pages.len(), 2, "must start as a 2-page document with {lines} lines");

    let doc_before = s.doc.clone();
    let res = s.run("layout.shrinkOnePage", &json!({}));
    assert!(res.is_ok(), "shrink should succeed on slight overflow: {res:?}");
    assert_eq!(s.layout().pages.len(), 1, "document must now fit on 1 page");
    assert!(s.status.contains("Document shrunk to 1 page"), "status was: {}", s.status);

    // Test single atomic undo
    let undo_res = s.run("edit.undo", &json!({}));
    assert!(undo_res.is_ok(), "undo must succeed");
    assert_eq!(s.layout().pages.len(), 2, "undo must restore 2-page layout");
    assert_eq!(s.doc, doc_before, "undo must restore exact pre-shrink document state");

    // Test redo
    let redo_res = s.run("edit.redo", &json!({}));
    assert!(redo_res.is_ok(), "redo must succeed");
    assert_eq!(s.layout().pages.len(), 1, "redo must restore 1-page layout");
}

#[test]
fn shrink_one_page_respects_max_delta() {
    let mut lines = 30;
    let mut s = create_multiline_doc(lines);
    while s.layout().pages.len() < 2 && lines < 80 {
        lines += 2;
        s = create_multiline_doc(lines);
    }
    // Add 15 more lines so that slight spacing compression alone (delta 0.0) cannot fit it onto 1 page
    for i in 1..=15 {
        s.run("text.insert", &json!({"text": format!("Extra line {i} to force font reduction.\n")})).unwrap();
    }
    assert_eq!(s.layout().pages.len(), 2);

    // With maxDeltaPt = 0.0 and adjustSpacing = false, no change can be made
    let res = s.run("layout.shrinkOnePage", &json!({"maxDeltaPt": 0.0, "adjustSpacing": false}));
    assert!(res.is_err(), "must fail when font delta is capped at 0.0 and spacing is disabled");
}
