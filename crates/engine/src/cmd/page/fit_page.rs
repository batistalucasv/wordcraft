//! Fit One Page command implementation (`layout.fitOnePage` / `layout.shrinkOnePage`).
//!
//! Automatically condenses a multi-page document by one page using speculative
//! layout evaluation, minimal typographical perturbation, and strict readability
//! safeguards (line spacing floor and font size floor).

use serde_json::Value;
use wordcraft_doc::Document;
use wordcraft_doc::fit_page::fit_document_typography;
use wordcraft_doc::section::SectionStart;
use wordcraft_layout::{LayoutCache, LayoutOptions, ViewMode};

use crate::cmd::sel_result;
use crate::{CmdError, CmdResult, Session, p};

/// Minimum comfortable body font size before refusing further reduction (Anti-Compound Guard).
pub const MIN_COMFORTABLE_BODY_FONT_SIZE: f32 = 8.5;

/// Maximum number of fine-grained trial steps allowed when `stepPt` is specified.
pub const MAX_FINE_STEPS: usize = 20;

/// Pre-calculated candidate steps `(delta_font_pt, space_factor)`.
///
/// Evaluated in ascending order of aesthetic perturbation:
/// 1. Tries paragraph spacing adjustment first with zero font size perturbation.
/// 2. Then tests micro font adjustments with minimal spacing changes.
/// 3. Respects a line-spacing floor (>= 0.94) and paragraph spacing floor (>= 0.85)
///    to guarantee that lines and paragraphs never crowd or collide.
const CANDIDATE_STEPS: [(f32, f32); 8] = [
    (0.0, 0.92), // 1. Spacing-only compression (font sizes untouched)
    (0.5, 0.96), // 2. Micro font reduction (-0.5 pt) with near-normal spacing
    (0.5, 0.90), // 3. Micro font reduction (-0.5 pt) with gentle spacing reduction
    (1.0, 0.96), // 4. Moderate font reduction (-1.0 pt) with near-normal spacing
    (1.0, 0.90), // 5. Moderate font reduction (-1.0 pt) with gentle spacing reduction
    (1.0, 0.85), // 6. Moderate font reduction (-1.0 pt) with tighter paragraph spacing
    (1.5, 0.90), // 7. Higher font reduction (-1.5 pt)
    (2.0, 0.88), // 8. Maximum safe font reduction (-2.0 pt)
];

/// Calls `f` on every paragraph in a block, including paragraphs inside table cells, using a stack.
fn walk_block_paras<'a>(b: &'a wordcraft_doc::Block, f: &mut dyn FnMut(&'a wordcraft_doc::para::Paragraph)) {
    let mut stack = vec![b];
    while let Some(current) = stack.pop() {
        if let Some(p) = current.as_para() {
            f(p);
        } else if let Some(tbl) = current.as_table() {
            for row in &tbl.rows {
                for cell in &row.cells {
                    for cb in &cell.blocks {
                        stack.push(cb);
                    }
                }
            }
        }
    }
}

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

/// Checks whether effective body font sizes are at or below the minimum readable font size (8.5 pt).
///
/// Inspects default styles, named styles, and all body paragraphs (including paragraphs in tables),
/// resolving effective font sizes against styles while ignoring superscript/subscript characters.
fn doc_has_unreadable_body_font(doc: &Document) -> bool {
    let default_size = doc.styles.default_chr.size.unwrap_or(wordcraft_doc::props::DEFAULT_FONT_SIZE);
    if default_size < MIN_COMFORTABLE_BODY_FONT_SIZE {
        return true;
    }

    if let Some(normal) = doc.styles.get("Normal")
        && let Some(sz) = normal.chr.size
        && sz < MIN_COMFORTABLE_BODY_FONT_SIZE
    {
        return true;
    }

    let mut found_unreadable = false;
    let mut check_para = |p: &wordcraft_doc::para::Paragraph| {
        if found_unreadable {
            return;
        }
        let style_id = p.props.style.as_deref();
        let mark_sz = p.mark.size.unwrap_or_else(|| doc.styles.resolve_char(style_id, &p.mark).size);
        if mark_sz < MIN_COMFORTABLE_BODY_FONT_SIZE {
            found_unreadable = true;
            return;
        }
        for r in &p.runs {
            if matches!(r.props.vert_align, Some(wordcraft_doc::props::VertAlign::Superscript | wordcraft_doc::props::VertAlign::Subscript)) {
                continue;
            }
            let eff_sz = r.props.size.unwrap_or_else(|| doc.styles.resolve_char(style_id, &r.props).size);
            if eff_sz < MIN_COMFORTABLE_BODY_FONT_SIZE {
                found_unreadable = true;
                return;
            }
        }
    };

    for b in &doc.body {
        walk_block_paras(b, &mut check_para);
    }
    found_unreadable
}

/// Executes the `layout.fitOnePage` command.
///
/// Condenses the document by one page if feasible, preserving typographical
/// hierarchies, protecting line spacing, and clamping to readability bounds.
pub fn fit_one_page(s: &mut Session, v: &Value) -> CmdResult {
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
    let step_pt_opt = p::f32(v, "stepPt");
    let adjust_spacing = p::bool(v, "adjustSpacing").unwrap_or(true);

    // 3. Lightweight transient layout options (proofing disabled for performance)
    let trial_opts = LayoutOptions {
        view: ViewMode::Print,
        proofing: false,
        proof: wordcraft_layout::ProofOptions { grammar: false, spelling: Default::default() },
        ..LayoutOptions::default()
    };

    // 4. Generate candidate steps: either bounded fine-grained stepPt or pre-calculated steps
    let mut candidates: Vec<(f32, f32)> = Vec::new();
    if let Some(step_pt) = step_pt_opt {
        if step_pt > 0.0 && step_pt.is_finite() {
            if adjust_spacing {
                candidates.push((0.0, 0.92));
            }
            let count = (((max_delta + 1e-4) / step_pt).floor()).min(MAX_FINE_STEPS as f32).max(0.0) as usize;
            for i in 1..=count {
                let d = i as f32 * step_pt;
                if adjust_spacing {
                    candidates.push((d, 0.95));
                    candidates.push((d, 0.90));
                } else {
                    candidates.push((d, 1.0));
                }
            }
        }
    } else {
        for &(delta, alpha) in &CANDIDATE_STEPS {
            if delta <= max_delta {
                let alpha = if adjust_spacing { alpha } else { 1.0 };
                if delta > 0.0 || (alpha - 1.0).abs() >= f32::EPSILON {
                    candidates.push((delta, alpha));
                }
            }
        }
        // If maxDeltaPt was explicitly configured above 2.0 pt, test higher reductions up to max_delta
        if max_delta > 2.0 {
            let mut extra_delta = 2.5;
            while extra_delta <= max_delta + 1e-4 {
                let alpha = if adjust_spacing { 0.85 } else { 1.0 };
                candidates.push((extra_delta, alpha));
                extra_delta += 0.5;
            }
        }
    }

    // 5. Speculative search loop using ephemeral cache
    let mut successful_doc = None;
    let mut applied_step = None;
    let mut final_pages = target_pages;

    for (delta, alpha) in candidates {
        // Anti-compound guard: refuse font reduction if body font is already at or below the readability floor,
        // while allowing spacing-only candidates (delta == 0.0) to run.
        if delta > 0.0 && doc_has_unreadable_body_font(&s.doc) {
            continue;
        }

        let mut trial_doc = s.doc.clone();
        fit_document_typography(&mut trial_doc, delta, alpha);

        // Validate the trial document's effective body font sizes against 8.5 pt minimum before accepting
        if delta > 0.0 && doc_has_unreadable_body_font(&trial_doc) {
            continue;
        }

        let mut trial_cache = LayoutCache::new();
        let trial_layout = wordcraft_layout::layout(&trial_doc, &mut trial_cache, &trial_opts);

        if trial_layout.pages.len() <= target_pages {
            final_pages = trial_layout.pages.len();
            successful_doc = Some(trial_doc);
            applied_step = Some((delta, alpha));
            break;
        }
    }

    // 6. Success commit or clean rollback
    match (successful_doc, applied_step) {
        (Some(new_doc), Some((d, a))) => {
            s.doc = new_doc;
            s.clamp_selection();
            let page_word = if final_pages == 1 { "page" } else { "pages" };
            s.status = if d > 0.0 {
                format!("Document fitted to {} {} (font: -{:.1} pt, spacing: {:.0}%).", final_pages, page_word, d, a * 100.0)
            } else {
                format!("Document fitted to {} {} (spacing: {:.0}%).", final_pages, page_word, a * 100.0)
            };
            sel_result(s)
        }
        _ => {
            if has_explicit_breaks(&s.doc) {
                Err(CmdError::Params("Unable to fit document to one page: explicit page or section breaks prevent page reduction.".into()))
            } else if doc_has_unreadable_body_font(&s.doc) {
                Err(CmdError::Params("Unable to shrink document further without reducing text beyond readability limits.".into()))
            } else {
                Err(CmdError::Params(
                    "Unable to fit document to one page: content on the final page exceeds limits for aesthetic page reduction.".into(),
                ))
            }
        }
    }
}

/// Backward compatibility alias for Word parity and existing callers.
pub fn shrink_one_page(s: &mut Session, v: &Value) -> CmdResult {
    fit_one_page(s, v)
}
