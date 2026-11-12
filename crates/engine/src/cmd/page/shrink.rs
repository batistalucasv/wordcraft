//! Shrink One Page command implementation (`layout.shrinkOnePage`).
//!
//! Automatically condenses a multi-page document by one page using speculative
//! layout evaluation and minimal aesthetic perturbation.

use serde_json::Value;
use wordcraft_doc::Document;
use wordcraft_doc::section::SectionStart;
use wordcraft_doc::shrink::shrink_document_typography;
use wordcraft_layout::{LayoutCache, LayoutOptions, ViewMode};

use crate::cmd::sel_result;
use crate::{CmdError, CmdResult, Session, p};

/// Heuristic search candidate steps: `(delta_font_pt, space_factor)`.
///
/// Evaluated in order of minimal aesthetic disruption, stopping at the earliest
/// step that achieves `P_trial <= P0 - 1`.
const CANDIDATE_STEPS: [(f32, f32); 7] = [
    (0.0, 0.92), // Gentle paragraph spacing compression only (font sizes unchanged)
    (0.5, 0.95), // Micro font reduction (visually subtle) with near-normal spacing
    (0.5, 0.90), // Balanced font reduction and spacing compression
    (1.0, 0.90), // Moderate font reduction for medium spills
    (1.0, 0.85), // Stronger compression for stubborn multi-line overflows
    (1.5, 0.80), // Upper limit of comfortable readability before rollback
    (2.0, 0.80), // Maximum reduction limit
];

/// Returns true if the document contains explicit page or section breaks that
/// physically force content onto additional pages.
fn has_explicit_breaks(doc: &Document) -> bool {
    let sections = doc.sections();
    if sections.len() > 1 {
        for (_, sec) in &sections[1..] {
            if matches!(sec.start, SectionStart::NextPage | SectionStart::EvenPage | SectionStart::OddPage) {
                return true;
            }
        }
    }
    for b in &doc.body {
        if let Some(p) = b.as_para()
            && p.text.contains(wordcraft_doc::para::PAGE_BREAK)
        {
            return true;
        } else if let Some(tbl) = b.as_table() {
            for row in &tbl.rows {
                for cell in &row.cells {
                    for cb in &cell.blocks {
                        if let Some(p) = cb.as_para()
                            && p.text.contains(wordcraft_doc::para::PAGE_BREAK)
                        {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Executes the `layout.shrinkOnePage` command.
///
/// Condenses the document by one page if feasible, preserving typographical
/// hierarchies, leaving headers and footers intact, and clamping font sizes to safe bounds.
pub fn shrink_one_page(s: &mut Session, v: &Value) -> CmdResult {
    // 1. Initial layout and page count evaluation in Print mode
    let p0 = if s.view.mode == ViewMode::Print && !s.view.read_mode {
        s.layout().pages.len()
    } else {
        let mut cache = LayoutCache::new();
        let opts = LayoutOptions { view: ViewMode::Print, proofing: false, ..LayoutOptions::default() };
        wordcraft_layout::layout(&s.doc, &mut cache, &opts).pages.len()
    };

    if p0 <= 1 {
        return Err(CmdError::Params("The document already fits on one page.".into()));
    }
    let target_pages = p0.saturating_sub(1);

    // 2. Parse parameters safely
    let max_delta = p::f32(v, "maxDeltaPt").unwrap_or(2.0).clamp(0.0, 10.0);
    let adjust_spacing = p::bool(v, "adjustSpacing").unwrap_or(true);

    // 3. Lightweight transient layout options (proofing disabled for performance)
    let trial_opts = LayoutOptions {
        view: ViewMode::Print,
        proofing: false,
        proof: wordcraft_layout::ProofOptions { grammar: false, spelling: Default::default() },
        ..LayoutOptions::default()
    };

    // 4. Speculative search loop using ephemeral cache
    let mut successful_doc = None;
    let mut applied_step = None;
    let mut final_pages = target_pages;

    for (delta, alpha) in CANDIDATE_STEPS {
        if delta > max_delta {
            continue;
        }
        let alpha = if adjust_spacing { alpha } else { 1.0 };
        if delta == 0.0 && (alpha - 1.0).abs() < f32::EPSILON {
            continue;
        }

        let mut trial_doc = s.doc.clone();
        shrink_document_typography(&mut trial_doc, delta, alpha);

        let mut trial_cache = LayoutCache::new();
        let trial_layout = wordcraft_layout::layout(&trial_doc, &mut trial_cache, &trial_opts);

        if trial_layout.pages.len() <= target_pages {
            final_pages = trial_layout.pages.len();
            successful_doc = Some(trial_doc);
            applied_step = Some((delta, alpha));
            break;
        }
    }

    // 5. Success commit or clean rollback
    match (successful_doc, applied_step) {
        (Some(new_doc), Some((d, a))) => {
            s.doc = new_doc;
            s.clamp_selection();
            let page_word = if final_pages == 1 { "page" } else { "pages" };
            s.status = if d > 0.0 {
                format!("Document shrunk to {} {} (font: -{:.1} pt, spacing: {:.0}%).", final_pages, page_word, d, a * 100.0)
            } else {
                format!("Document shrunk to {} {} (spacing: {:.0}%).", final_pages, page_word, a * 100.0)
            };
            sel_result(s)
        }
        _ => {
            if has_explicit_breaks(&s.doc) {
                Err(CmdError::Params("Unable to shrink document: explicit page or section breaks prevent page reduction.".into()))
            } else {
                Err(CmdError::Params("Unable to shrink document to one less page without reducing text beyond readability limits.".into()))
            }
        }
    }
}
