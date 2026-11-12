//! Document typography shrinking helper.
//!
//! Adjusts font sizes and paragraph spacing across styles and editable stories
//! while preserving visual hierarchy, clamping to safe bounds, and excluding
//! fixed framing stories like headers and footers.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::Document;
use crate::edit::{each_para, each_para_mut};
use crate::para::Paragraph;
use crate::parts::PartKind;
use crate::props::{CharProps, LineSpacing, ParaProps};

/// Minimum font size allowed in points (WordCraft floor).
pub const MIN_SHRINK_FONT_SIZE: f32 = 4.0;

/// Maximum font size allowed in points.
pub const MAX_SHRINK_FONT_SIZE: f32 = 1638.0;

/// Safe line spacing floor factor (preventing line collision / crowding per issue #15).
pub const MIN_LINE_SPACING_FACTOR: f32 = 0.94;

/// Clamp a font size to `[MIN_SHRINK_FONT_SIZE, MAX_SHRINK_FONT_SIZE]` and round to 0.5 pt (half-points).
#[inline]
pub fn round_half_pt(size: f32) -> f32 {
    let clamped = size.clamp(MIN_SHRINK_FONT_SIZE, MAX_SHRINK_FONT_SIZE);
    (clamped * 2.0).round() / 2.0
}

/// Scale a font size down by `delta_pt`.
#[inline]
fn scale_size(size: f32, delta_pt: f32) -> f32 {
    round_half_pt(size - delta_pt)
}

/// Scale a spacing value (e.g. `space_before`, `space_after`) by `space_factor`.
#[inline]
fn scale_spacing(val: f32, space_factor: f32) -> f32 {
    let scaled = (val * space_factor).clamp(0.0, 1584.0);
    (scaled * 2.0).round() / 2.0
}

/// Scale line spacing by `space_factor` respecting the safe readability floor.
#[inline]
fn scale_line_spacing(ls: LineSpacing, space_factor: f32) -> LineSpacing {
    match ls {
        LineSpacing::Multiple(m) => {
            // Guard against ascender/descender collisions: keep line spacing at or above
            // safe readability floor of 0.94 factor (~6% max compression).
            let floor = m * MIN_LINE_SPACING_FACTOR;
            let new_m = (m * space_factor).max(floor).clamp(0.5, 132.0);
            LineSpacing::Multiple((new_m * 100.0).round() / 100.0)
        }
        LineSpacing::AtLeast(pts) => LineSpacing::AtLeast(scale_spacing(pts, space_factor.max(MIN_LINE_SPACING_FACTOR))),
        LineSpacing::Exactly(pts) => LineSpacing::Exactly(scale_spacing(pts, space_factor.max(MIN_LINE_SPACING_FACTOR))),
    }
}

/// Scale explicit font sizes on a `CharProps`.
#[inline]
fn scale_char_props(c: &mut CharProps, delta_pt: f32) {
    if let Some(s) = c.size.as_mut() {
        *s = scale_size(*s, delta_pt);
    }
    if let Some(s) = c.size_cs.as_mut() {
        *s = scale_size(*s, delta_pt);
    }
}

/// Scale explicit paragraph spacing on a `ParaProps`.
#[inline]
fn scale_para_props(p: &mut ParaProps, space_factor: f32) {
    if space_factor < 1.0 - f32::EPSILON {
        if let Some(sb) = p.space_before.as_mut() {
            *sb = scale_spacing(*sb, space_factor);
        }
        if let Some(sa) = p.space_after.as_mut() {
            *sa = scale_spacing(*sa, space_factor);
        }
        if let Some(ls) = p.line_spacing.as_mut() {
            *ls = scale_line_spacing(*ls, space_factor);
        }
    }
}

/// Fits font sizes across the document by `delta_pt` points and scales paragraph
/// spacing by `space_factor`.
///
/// Clean-room typographical helper for Fit One Page. Preserves relative typography
/// hierarchies, clamps line spacing above single-line collision floors, and excludes
/// headers and footers.
#[inline]
pub fn fit_document_typography(doc: &mut Document, delta_pt: f32, space_factor: f32) {
    shrink_document_typography(doc, delta_pt, space_factor);
}

/// Shrinks font sizes across the document by `delta_pt` points and scales paragraph
/// spacing by `space_factor`.
///
/// Preserves relative typography hierarchies between headings, body text, and direct run formatting.
/// Clamps font sizes to `[4.0, 1638.0]` pt and rounds to 0.5 pt (half-points).
/// Headers and footers are explicitly excluded and retain their exact original sizes.
/// Footnotes are included and shrink alongside body text.
pub fn shrink_document_typography(doc: &mut Document, delta_pt: f32, space_factor: f32) {
    if !delta_pt.is_finite() || !space_factor.is_finite() {
        return;
    }
    let delta_pt = delta_pt.max(0.0);
    let space_factor = space_factor.clamp(0.1, 1.0);

    if delta_pt == 0.0 && (space_factor - 1.0).abs() < f32::EPSILON {
        return;
    }

    // 1. Find all text boxes reachable from Header and Footer parts (including nested text boxes and chains).
    let mut hf_boxes: BTreeSet<u32> = BTreeSet::new();
    let mut queue: Vec<u32> = Vec::new();

    for part in doc.parts.values() {
        if matches!(part.kind, PartKind::Header | PartKind::Footer) {
            for b in &part.blocks {
                each_para(b, 0, &mut |p| {
                    for obj in &p.objects {
                        for box_id in obj.text_boxes() {
                            if hf_boxes.insert(box_id) {
                                queue.push(box_id);
                            }
                        }
                    }
                });
            }
        }
    }

    while let Some(box_id) = queue.pop() {
        if let Some(part) = doc.parts.get(&box_id) {
            if let Some(next_id) = part.body.next
                && hf_boxes.insert(next_id)
            {
                queue.push(next_id);
            }
            for b in &part.blocks {
                each_para(b, 0, &mut |p| {
                    for obj in &p.objects {
                        for nested_id in obj.text_boxes() {
                            if hf_boxes.insert(nested_id) {
                                queue.push(nested_id);
                            }
                        }
                    }
                });
            }
        }
    }

    // Freeze header and footer font sizes and spacing (and reachable text boxes) before modifying styles,
    // so headers, footers, and their text boxes never shrink even if they inherit from default or shared styles.
    let old_styles = doc.styles.clone();
    for (id, part) in doc.parts.iter_mut() {
        if matches!(part.kind, PartKind::Header | PartKind::Footer) || hf_boxes.contains(id) {
            for b in &mut part.blocks {
                each_para_mut(Arc::make_mut(b), 0, &mut |p| {
                    let style = p.props.style.clone();
                    for r in &mut p.runs {
                        if r.props.size.is_none() {
                            let resolved = old_styles.resolve_char(style.as_deref(), &r.props).size;
                            r.props.size = Some(resolved);
                        }
                        if r.props.size_cs.is_none() {
                            let resolved_cs = old_styles.resolve_char(style.as_deref(), &r.props).size_cs;
                            r.props.size_cs = Some(resolved_cs);
                        }
                    }
                    if p.mark.size.is_none() {
                        let resolved = old_styles.resolve_char(style.as_deref(), &p.mark).size;
                        p.mark.size = Some(resolved);
                    }
                    if p.mark.size_cs.is_none() {
                        let resolved_cs = old_styles.resolve_char(style.as_deref(), &p.mark).size_cs;
                        p.mark.size_cs = Some(resolved_cs);
                    }
                    let resolved_p = old_styles.resolve_para(&p.props);
                    if p.props.space_before.is_none() {
                        p.props.space_before = Some(resolved_p.space_before);
                    }
                    if p.props.space_after.is_none() {
                        p.props.space_after = Some(resolved_p.space_after);
                    }
                    if p.props.line_spacing.is_none() {
                        p.props.line_spacing = Some(resolved_p.line_spacing);
                    }
                });
            }
        }
    }

    // 2. Adjust styles: document defaults, character styles, paragraph styles, table style regions.
    let st = Arc::make_mut(&mut doc.styles);

    // Default character formatting
    let default_size = st.default_chr.size.unwrap_or(crate::props::DEFAULT_FONT_SIZE);
    st.default_chr.size = Some(scale_size(default_size, delta_pt));
    if let Some(cs) = st.default_chr.size_cs.as_mut() {
        *cs = scale_size(*cs, delta_pt);
    }

    // Default paragraph formatting
    scale_para_props(&mut st.default_para, space_factor);

    // All named styles (excluding header/footer styles if any)
    for style in &mut st.styles {
        let is_hf = style.id.eq_ignore_ascii_case("header")
            || style.id.eq_ignore_ascii_case("footer")
            || style.name.eq_ignore_ascii_case("header")
            || style.name.eq_ignore_ascii_case("footer");
        if is_hf {
            continue;
        }

        scale_char_props(&mut style.chr, delta_pt);
        scale_para_props(&mut style.para, space_factor);

        if let Some(t) = style.table.as_mut() {
            for c in [&mut t.header_chr, &mut t.band_chr, &mut t.first_col_chr, &mut t.last_col_chr, &mut t.total_chr, &mut t.col_band_chr] {
                scale_char_props(c, delta_pt);
            }
            for part in t.regions.values_mut() {
                scale_char_props(&mut part.chr, delta_pt);
            }
        }
    }

    // 3. Helper to shrink editable paragraph content (runs, mark, spacing).
    let mut shrink_paragraph = |p: &mut Paragraph| {
        scale_para_props(&mut p.props, space_factor);

        // Scale explicit paragraph mark size
        scale_char_props(&mut p.mark, delta_pt);

        // Scale explicit run sizes
        for r in &mut p.runs {
            scale_char_props(&mut r.props, delta_pt);
        }

        p.touch();
    };

    // 4. Traverse main body story.
    for b in &mut doc.body {
        each_para_mut(Arc::make_mut(b), 0, &mut shrink_paragraph);
    }

    // 5. Traverse secondary editable stories (footnotes, body text boxes), excluding headers and footers
    // and text boxes reachable from headers or footers.
    for (id, part) in doc.parts.iter_mut() {
        if matches!(part.kind, PartKind::Header | PartKind::Footer) || hf_boxes.contains(id) {
            continue;
        }
        for b in &mut part.blocks {
            each_para_mut(Arc::make_mut(b), 0, &mut shrink_paragraph);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Block;
    use crate::para::Paragraph;
    use crate::props::{CharProps, LineSpacing};
    use crate::table::Table;
    use crate::{Document, para_block};

    #[test]
    fn test_shrink_preserves_hierarchy() {
        let mut doc = Document::new();
        // Setup Heading 1 paragraph and Normal paragraph
        let h1_para = Paragraph::with_text("Heading 1 Title", CharProps::default()).styled("Heading1");
        let normal_para = Paragraph::with_text("Body text in Normal style", CharProps::default()).styled("Normal");
        let mut custom_run_para = Paragraph::with_text("Normal prefix ", CharProps::default()).styled("Normal");
        custom_run_para.insert_text(custom_run_para.len(), "Special large text", &CharProps { size: Some(24.0), ..Default::default() }).unwrap();

        doc.body = vec![para_block(h1_para), para_block(normal_para), para_block(custom_run_para)];

        let h1_before = doc.styles.resolve_char(Some("Heading1"), &CharProps::default()).size;
        let normal_before = doc.styles.resolve_char(Some("Normal"), &CharProps::default()).size;

        shrink_document_typography(&mut doc, 0.5, 1.0);

        let h1_after = doc.styles.resolve_char(Some("Heading1"), &CharProps::default()).size;
        let normal_after = doc.styles.resolve_char(Some("Normal"), &CharProps::default()).size;

        // Both styles scaled down by exactly 0.5 pt
        assert_eq!(h1_after, h1_before - 0.5);
        assert_eq!(normal_after, normal_before - 0.5);
        // Relative hierarchy preserved
        assert_eq!(h1_after - normal_after, h1_before - normal_before);

        // Explicit run in paragraph 3 was 24.0 pt, now 23.5 pt
        let p3 = doc.body[2].as_para().unwrap();
        assert_eq!(p3.runs[1].props.size, Some(23.5));
    }

    #[test]
    fn test_shrink_excludes_headers_and_footers() {
        let mut doc = Document::new();
        let body_para = Paragraph::with_text("Main story body text", CharProps::default());
        doc.body = vec![para_block(body_para)];

        let header_para = Paragraph::with_text("Header Text", CharProps { size: Some(10.0), ..Default::default() });
        let footer_para = Paragraph::with_text("Footer Text Page 1", CharProps::default()); // inherits

        let h_id = doc.add_part(PartKind::Header, vec![para_block(header_para)]);
        let f_id = doc.add_part(PartKind::Footer, vec![para_block(footer_para)]);

        let footer_before_size = doc.styles.resolve_char(None, &CharProps::default()).size;

        shrink_document_typography(&mut doc, 0.5, 1.0);

        // Body default style shrank
        let body_after_size = doc.styles.resolve_char(None, &CharProps::default()).size;
        assert_eq!(body_after_size, footer_before_size - 0.5);

        // Header retained its explicit 10.0 pt size
        let h_part = doc.parts.get(&h_id).unwrap();
        let h_p = h_part.blocks[0].as_para().unwrap();
        assert_eq!(h_p.runs[0].props.size, Some(10.0));

        // Footer retained its original resolved size
        let f_part = doc.parts.get(&f_id).unwrap();
        let f_p = f_part.blocks[0].as_para().unwrap();
        assert_eq!(f_p.runs[0].props.size, Some(footer_before_size));
    }

    #[test]
    fn test_shrink_scales_footnotes() {
        let mut doc = Document::new();
        let body_para = Paragraph::with_text("Main body with note", CharProps::default());
        doc.body = vec![para_block(body_para)];

        let fn_para = Paragraph::with_text("Footnote content", CharProps { size: Some(10.0), ..Default::default() });
        let fn_id = doc.add_part(PartKind::Footnote, vec![para_block(fn_para)]);

        shrink_document_typography(&mut doc, 0.5, 1.0);

        let fn_part = doc.parts.get(&fn_id).unwrap();
        let fn_p = fn_part.blocks[0].as_para().unwrap();
        assert_eq!(fn_p.runs[0].props.size, Some(9.5));
    }

    #[test]
    fn test_shrink_clamps_to_min_floor() {
        let mut doc = Document::new();
        let mut p = Paragraph::with_text("Very tiny text", CharProps { size: Some(4.2), ..Default::default() });
        p.insert_text(p.len(), "Floor text", &CharProps { size: Some(4.0), ..Default::default() }).unwrap();
        doc.body = vec![para_block(p)];

        shrink_document_typography(&mut doc, 1.0, 1.0);

        let p_after = doc.body[0].as_para().unwrap();
        assert_eq!(p_after.runs[0].props.size, Some(4.0));
        assert_eq!(p_after.runs[1].props.size, Some(4.0));
    }

    #[test]
    fn test_shrink_zero_delta_identity() {
        let mut doc = Document::new();
        let h1_para = Paragraph::with_text("Heading 1 Title", CharProps::default()).styled("Heading1");
        let normal_para = Paragraph::with_text("Body text", CharProps::default());
        doc.body = vec![para_block(h1_para), para_block(normal_para)];
        doc.add_part(PartKind::Header, vec![para_block(Paragraph::with_text("Header", CharProps::default()))]);
        doc.add_part(PartKind::Footnote, vec![para_block(Paragraph::with_text("Note", CharProps::default()))]);

        let before = doc.clone();
        shrink_document_typography(&mut doc, 0.0, 1.0);
        assert_eq!(doc, before);
    }

    #[test]
    fn test_shrink_paragraph_spacing() {
        let mut doc = Document::new();
        let mut p = Paragraph::with_text("Spaced paragraph", CharProps::default());
        p.props.space_before = Some(18.0);
        p.props.space_after = Some(12.0);
        p.props.line_spacing = Some(LineSpacing::Multiple(1.15));
        doc.body = vec![para_block(p)];

        shrink_document_typography(&mut doc, 0.5, 0.9);

        let p_after = doc.body[0].as_para().unwrap();
        // 18.0 * 0.9 = 16.2 -> round to 0.5 pt = 16.0
        assert_eq!(p_after.props.space_before, Some(16.0));
        // 12.0 * 0.9 = 10.8 -> round to 0.5 pt = 11.0
        assert_eq!(p_after.props.space_after, Some(11.0));
        // 1.15 * 0.9 = 1.035 -> clamped to safe line spacing floor (1.15 * 0.94 = 1.081) -> 1.08
        assert_eq!(p_after.props.line_spacing, Some(LineSpacing::Multiple(1.08)));
    }

    #[test]
    fn test_line_spacing_floor_prevents_overlap() {
        let mut doc = Document::new();
        let mut p = Paragraph::with_text("Single spaced text", CharProps::default());
        p.props.line_spacing = Some(LineSpacing::Multiple(1.0));
        doc.body = vec![para_block(p)];

        // Even with aggressive space_factor = 0.5, line spacing multiple must not fall below safe floor (0.94)
        fit_document_typography(&mut doc, 0.5, 0.5);

        let p_after = doc.body[0].as_para().unwrap();
        assert_eq!(p_after.props.line_spacing, Some(LineSpacing::Multiple(0.94)));
    }

    #[test]
    fn test_header_text_boxes_are_frozen_and_do_not_shrink() {
        let mut doc = Document::new();
        let body_p = Paragraph::with_text("Body text", CharProps { size: Some(12.0), ..Default::default() });
        doc.body = vec![para_block(body_p)];

        let header_box_id = doc
            .add_part(PartKind::TextBox, vec![para_block(Paragraph::with_text("Header box", CharProps { size: Some(14.0), ..Default::default() }))]);
        let mut header_p = Paragraph::with_text("Header prefix ", CharProps { size: Some(10.0), ..Default::default() });
        let shape = crate::para::InlineObject::Shape {
            kind: crate::para::ShapeKind::TextBox,
            w: 144.0,
            h: 72.0,
            fill: None,
            stroke: None,
            stroke_width: 0.75,
            float: Default::default(),
            story: Some(header_box_id),
            freeform: None,
            effects: Default::default(),
            extra: Default::default(),
        };
        header_p.objects.push(shape);
        doc.add_part(PartKind::Header, vec![para_block(header_p)]);

        let body_box_id =
            doc.add_part(PartKind::TextBox, vec![para_block(Paragraph::with_text("Body box", CharProps { size: Some(16.0), ..Default::default() }))]);

        shrink_document_typography(&mut doc, 2.0, 1.0);

        let b_p = doc.body[0].as_para().unwrap();
        assert_eq!(b_p.runs[0].props.size, Some(10.0));

        let bb_part = doc.parts.get(&body_box_id).unwrap();
        let bb_p = bb_part.blocks[0].as_para().unwrap();
        assert_eq!(bb_p.runs[0].props.size, Some(14.0));

        let hb_part = doc.parts.get(&header_box_id).unwrap();
        let hb_p = hb_part.blocks[0].as_para().unwrap();
        assert_eq!(hb_p.runs[0].props.size, Some(14.0));
    }

    #[test]
    fn test_shrink_table_cells() {
        let mut doc = Document::new();
        let mut table = Table::new(1, 1, 100.0);
        let cell_p = Paragraph::with_text("Cell text", CharProps { size: Some(12.0), ..Default::default() });
        table.rows[0].cells[0].blocks = vec![para_block(cell_p)];
        doc.body = vec![Arc::new(Block::Table(table))];

        shrink_document_typography(&mut doc, 0.5, 1.0);

        let tbl = doc.body[0].as_table().unwrap();
        let cell_p_after = tbl.rows[0].cells[0].blocks[0].as_para().unwrap();
        assert_eq!(cell_p_after.runs[0].props.size, Some(11.5));
    }
}
