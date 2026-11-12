use serde_json::json;
use wordcraft_doc::Document;
use wordcraft_doc::para::PAGE_BREAK;
use wordcraft_doc::props::CharProps;

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
fn fit_one_page_fails_on_single_page() {
    let mut s = create_multiline_doc(5);
    let before = s.doc.clone();
    let res = s.run("layout.fitOnePage", &json!({}));
    assert!(res.is_err());
    let err = res.unwrap_err().to_string();
    assert!(err.contains("already fits on one page"), "unexpected error: {err}");
    assert_eq!(s.doc, before, "document must not change on failure");
}

#[test]
fn fit_one_page_fails_on_hard_page_break() {
    let text = format!("Page 1 content\n{PAGE_BREAK}Page 2 content\n");
    let mut s = Session::new(Document::from_text(&text));
    let before = s.doc.clone();
    let res = s.run("layout.fitOnePage", &json!({}));
    assert!(res.is_err());
    let err = res.unwrap_err().to_string();
    assert!(err.contains("explicit page or section breaks"), "unexpected error: {err}");
    assert_eq!(s.doc, before, "document must not change on failure");
}

#[test]
fn fit_one_page_fails_on_next_page_section_break() {
    let mut s = Session::new(Document::from_text("First section body text"));
    s.run("layout.break", &json!({"kind": "nextPage"})).unwrap();
    s.run("text.insert", &json!({"text": "Second section text on next page"})).unwrap();
    assert_eq!(s.layout().pages.len(), 2, "break must cause 2 pages");

    let before = s.doc.clone();
    let res = s.run("layout.fitOnePage", &json!({}));
    assert!(res.is_err());
    let err = res.unwrap_err().to_string();
    assert!(err.contains("explicit page or section breaks"), "unexpected error: {err}");
    assert_eq!(s.doc, before, "document must not change on failure");
}

#[test]
fn fit_one_page_condenses_slight_overflow_and_undoes() {
    // Find the boundary where the document just spills onto page 2
    let mut lines = 30;
    let mut s = create_multiline_doc(lines);
    while s.layout().pages.len() < 2 && lines < 80 {
        lines += 2;
        s = create_multiline_doc(lines);
    }
    assert_eq!(s.layout().pages.len(), 2, "must start as a 2-page document with {lines} lines");

    let doc_before = s.doc.clone();
    let res = s.run("layout.fitOnePage", &json!({}));
    assert!(res.is_ok(), "fit should succeed on slight overflow: {res:?}");
    assert_eq!(s.layout().pages.len(), 1, "document must now fit on 1 page");
    assert!(s.status.contains("Document fitted to 1 page"), "status was: {}", s.status);

    // Test single atomic undo
    let undo_res = s.run("edit.undo", &json!({}));
    assert!(undo_res.is_ok(), "undo must succeed");
    assert_eq!(s.layout().pages.len(), 2, "undo must restore 2-page layout");
    assert_eq!(s.doc, doc_before, "undo must restore exact pre-fit document state");

    // Test redo
    let redo_res = s.run("edit.redo", &json!({}));
    assert!(redo_res.is_ok(), "redo must succeed");
    assert_eq!(s.layout().pages.len(), 1, "redo must restore 1-page layout");
}

#[test]
fn fit_one_page_alias_shrink_one_page_works() {
    let mut lines = 30;
    let mut s = create_multiline_doc(lines);
    while s.layout().pages.len() < 2 && lines < 80 {
        lines += 2;
        s = create_multiline_doc(lines);
    }
    assert_eq!(s.layout().pages.len(), 2);
    let res = s.run("layout.shrinkOnePage", &json!({}));
    assert!(res.is_ok(), "shrink alias should succeed: {res:?}");
    assert_eq!(s.layout().pages.len(), 1);
}

#[test]
fn fit_one_page_respects_max_delta() {
    let mut lines = 30;
    let mut s = create_multiline_doc(lines);
    while s.layout().pages.len() < 2 && lines < 80 {
        lines += 2;
        s = create_multiline_doc(lines);
    }
    for i in 1..=15 {
        s.run("text.insert", &json!({"text": format!("Extra line {i} to force font reduction.\n")})).unwrap();
    }
    assert_eq!(s.layout().pages.len(), 2);

    let res = s.run("layout.fitOnePage", &json!({"maxDeltaPt": 0.0, "adjustSpacing": false}));
    assert!(res.is_err(), "must fail when font delta is capped at 0.0 and spacing is disabled");
}

#[test]
fn fit_one_page_respects_step_pt() {
    let mut lines = 28;
    let mut s = create_multiline_doc(lines);
    while s.layout().pages.len() < 2 && lines < 80 {
        lines += 1;
        s = create_multiline_doc(lines);
    }
    assert_eq!(s.layout().pages.len(), 2, "must start as 2-page document with {lines} lines");

    let doc_backup = s.doc.clone();
    let default_res = s.run("layout.fitOnePage", &json!({"maxDeltaPt": 0.4, "adjustSpacing": false}));
    assert!(default_res.is_err(), "default step 0.5 must skip all candidates when maxDelta is 0.4");
    s.doc = doc_backup;

    let res = s.run("layout.fitOnePage", &json!({"maxDeltaPt": 2.0, "stepPt": 0.1}));
    assert!(res.is_ok(), "stepPt=0.1 must generate fine-grained candidates: {res:?}");
    assert_eq!(s.layout().pages.len(), 1);
}

#[test]
fn fit_one_page_allows_spacing_only_at_font_floor() {
    // Find the boundary where the document just spills onto page 2 by 1 line
    let mut lines = 28;
    let mut s = create_multiline_doc(lines);
    while s.layout().pages.len() < 2 && lines < 80 {
        lines += 1;
        s = create_multiline_doc(lines);
    }
    assert_eq!(s.layout().pages.len(), 2);

    // Set body text to 8.5 pt (at MIN_COMFORTABLE_BODY_FONT_SIZE floor)
    for b in &mut s.doc.body {
        if let wordcraft_doc::Block::Para(p) = std::sync::Arc::make_mut(b) {
            for r in &mut p.runs {
                r.props = CharProps { size: Some(8.5), ..Default::default() };
            }
        }
    }
    assert_eq!(s.layout().pages.len(), 2);

    // Spacing-only candidate (0.0, 0.92) must be permitted to run and succeed
    let res = s.run("layout.fitOnePage", &json!({}));
    assert!(res.is_ok(), "spacing-only candidate must succeed at font floor: {res:?}");
    assert_eq!(s.layout().pages.len(), 1);
    assert!(s.status.contains("spacing: 92%"), "status: {}", s.status);
}

#[test]
fn fit_one_page_fails_when_body_font_already_at_minimum_readable_size() {
    let mut lines = 30;
    let mut s = create_multiline_doc(lines);
    while s.layout().pages.len() < 2 && lines < 80 {
        lines += 2;
        s = create_multiline_doc(lines);
    }
    assert_eq!(s.layout().pages.len(), 2);

    // Set body text to 8.0 pt (below MIN_COMFORTABLE_BODY_FONT_SIZE = 8.5 pt)
    for b in &mut s.doc.body {
        if let wordcraft_doc::Block::Para(p) = std::sync::Arc::make_mut(b) {
            for r in &mut p.runs {
                r.props = CharProps { size: Some(8.0), ..Default::default() };
            }
        }
    }

    // When spacing adjustment is disabled, no candidate can run without reducing font
    let res = s.run("layout.fitOnePage", &json!({"adjustSpacing": false}));
    assert!(res.is_err(), "must refuse to shrink font when already <= 8.5 pt");
    let err = res.unwrap_err().to_string();
    assert!(err.contains("Unable to shrink document further without reducing text beyond readability limits."), "unexpected error message: {err}");
}

#[test]
fn fit_one_page_bounds_fine_step_loop() {
    let mut s = create_multiline_doc(10);
    // Extremely small stepPt must not hang or exhaust memory
    let res = s.run("layout.fitOnePage", &json!({"stepPt": 1e-7, "maxDeltaPt": 1.0}));
    assert!(res.is_err());
}

#[test]
fn fit_one_page_supports_max_delta_above_2pt() {
    let mut lines = 30;
    let mut s = create_multiline_doc(lines);
    while s.layout().pages.len() < 2 && lines < 80 {
        lines += 2;
        s = create_multiline_doc(lines);
    }
    for i in 1..=20 {
        s.run("text.insert", &json!({"text": format!("Extra paragraph line {i}\n")})).unwrap();
    }
    assert_eq!(s.layout().pages.len(), 2);

    // With maxDeltaPt = 3.0, candidates up to 3.0 pt are tested
    let res = s.run("layout.fitOnePage", &json!({"maxDeltaPt": 3.0}));
    // Should run smoothly without errors or panic
    let _ = res;
}

#[test]
fn fit_one_page_fails_when_overflow_exceeds_limits() {
    let mut lines = 28;
    let mut s = create_multiline_doc(lines);
    while s.layout().pages.len() < 2 {
        lines += 1;
        s = create_multiline_doc(lines);
    }
    // Add 16 extra lines to page 2: this overflow is too large to fit into page 1 with maxDeltaPt = 0.5
    for i in 1..=16 {
        s.run("text.insert", &json!({"text": format!("Overflow line {i}\n")})).unwrap();
    }
    assert_eq!(s.layout().pages.len(), 2, "must be a 2-page document");

    let res = s.run("layout.fitOnePage", &json!({"maxDeltaPt": 0.5}));
    assert!(res.is_err(), "must refuse when overflow exceeds aesthetic reduction limits");
    let err = res.unwrap_err().to_string();
    assert!(err.contains("exceeds limits for aesthetic page reduction"), "unexpected error message: {err}");
}

#[test]
fn fit_one_page_validates_effective_font_in_tables_before_committing() {
    let mut s = Session::new(Document::new());
    let mut table = wordcraft_doc::Table::new(1, 1, 400.0);
    let cell_p = wordcraft_doc::Paragraph::with_text("Cell text", CharProps { size: Some(9.0), ..Default::default() });
    table.rows[0].cells[0].blocks = vec![wordcraft_doc::para_block(cell_p)];
    s.doc.body = vec![std::sync::Arc::new(wordcraft_doc::Block::Table(table))];

    let mut text = String::new();
    for i in 1..=45 {
        text.push_str(&format!("Line {i} following table: The quick brown fox jumps over the lazy dog.\n"));
    }
    s.doc.body.push(wordcraft_doc::para_block(wordcraft_doc::Paragraph::with_text(&text, CharProps::default())));
    assert_eq!(s.layout().pages.len(), 2);

    // If maxDeltaPt is 1.0, 9.0 pt would reduce to 8.0 pt (violating 8.5 pt floor).
    // The candidate must not be committed.
    let _ = s.run("layout.fitOnePage", &json!({"maxDeltaPt": 1.0, "adjustSpacing": false}));
    if let Some(tbl) = s.doc.body[0].as_table() {
        let p = tbl.rows[0].cells[0].blocks[0].as_para().unwrap();
        let sz = p.runs[0].props.size.unwrap();
        assert!(sz >= 8.5, "table cell font size must remain >= 8.5 pt, was {sz}");
    }
}
