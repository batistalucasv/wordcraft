//! Document typography shrinking helper.
//!
//! Adjusts font sizes and paragraph spacing across styles and editable stories
//! while preserving visual hierarchy, clamping to safe bounds, and excluding
//! fixed framing stories like headers, footers, and text boxes anchored in them.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::Document;
use crate::block::Block;
use crate::edit::each_para_mut;
use crate::para::Paragraph;
use crate::parts::PartKind;
use crate::props::{CharProps, LineSpacing, ParaProps};
use crate::resolve::AUTO_SPACING;
use crate::styles::{PartSet, StyleSheet};
use crate::table::{MAX_TABLE_DEPTH, Table};

/// Minimum font size allowed in points (WordCraft floor).
pub const MIN_SHRINK_FONT_SIZE: f32 = 4.0;

/// Maximum font size allowed in points.
pub const MAX_SHRINK_FONT_SIZE: f32 = 1638.0;

/// Minimum paragraph spacing scaling factor allowed (Issue #3 specifies `[0.80, 1.0]`).
pub const MIN_SHRINK_SPACE_FACTOR: f32 = 0.80;

/// Maximum paragraph spacing scaling factor allowed.
pub const MAX_SHRINK_SPACE_FACTOR: f32 = 1.0;

/// Clamp a font size to `[MIN_SHRINK_FONT_SIZE, MAX_SHRINK_FONT_SIZE]` and round to 0.5 pt (half-points).
/// If `size` is non-finite, returns `MIN_SHRINK_FONT_SIZE`.
#[inline]
pub fn round_half_pt(size: f32) -> f32 {
    if !size.is_finite() {
        return MIN_SHRINK_FONT_SIZE;
    }
    let clamped = size.clamp(MIN_SHRINK_FONT_SIZE, MAX_SHRINK_FONT_SIZE);
    (clamped * 2.0).round() / 2.0
}

/// Scale a font size down by `delta_pt`. Normalizes non-finite values to `MIN_SHRINK_FONT_SIZE`.
/// Ensures the selected half-point size never exceeds the original size when within supported bounds.
#[inline]
fn scale_size(size: f32, delta_pt: f32) -> f32 {
    if !size.is_finite() || !delta_pt.is_finite() {
        return MIN_SHRINK_FONT_SIZE;
    }
    let mut scaled = round_half_pt(size - delta_pt);
    if (MIN_SHRINK_FONT_SIZE..=MAX_SHRINK_FONT_SIZE).contains(&size) && scaled > size {
        scaled = (scaled - 0.5).max(MIN_SHRINK_FONT_SIZE);
    }
    scaled
}

/// Scale a spacing value (e.g. `space_before`, `space_after`) by `space_factor`.
/// Returns 0.0 for non-finite values.
#[inline]
fn scale_spacing(val: f32, space_factor: f32) -> f32 {
    if !val.is_finite() || !space_factor.is_finite() {
        return 0.0;
    }
    let scaled = (val * space_factor).clamp(0.0, 1584.0);
    (scaled * 2.0).round() / 2.0
}

/// Scale line spacing by `space_factor`. Normalizes non-finite values safely.
#[inline]
fn scale_line_spacing(ls: LineSpacing, space_factor: f32) -> LineSpacing {
    match ls {
        LineSpacing::Multiple(m) => {
            if !m.is_finite() || !space_factor.is_finite() {
                return LineSpacing::Multiple(1.0);
            }
            let new_m = (m * space_factor).clamp(0.5, 132.0);
            LineSpacing::Multiple((new_m * 100.0).round() / 100.0)
        }
        LineSpacing::AtLeast(pts) => {
            if !pts.is_finite() {
                return LineSpacing::AtLeast(0.0);
            }
            LineSpacing::AtLeast(scale_spacing(pts, space_factor))
        }
        LineSpacing::Exactly(pts) => {
            if !pts.is_finite() {
                return LineSpacing::Exactly(12.0);
            }
            LineSpacing::Exactly(scale_spacing(pts, space_factor).max(0.7))
        }
    }
}

/// Scale explicit font sizes on a `CharProps`.
/// Validates and normalizes non-finite values so invalid numbers cannot escape bounds.
#[inline]
fn scale_char_props(c: &mut CharProps, delta_pt: f32) {
    if let Some(s) = c.size.as_mut() {
        if s.is_finite() {
            *s = scale_size(*s, delta_pt);
        } else {
            *s = MIN_SHRINK_FONT_SIZE;
        }
    }
    if let Some(s) = c.size_cs.as_mut() {
        if s.is_finite() {
            *s = scale_size(*s, delta_pt);
        } else {
            *s = MIN_SHRINK_FONT_SIZE;
        }
    }
}

/// Scale explicit and automatic paragraph spacing on a `ParaProps`.
/// When automatic spacing is active (`space_before_auto` or `space_after_auto`),
/// converts it to explicit spacing scaled from `AUTO_SPACING` (14 pt) so the reduction
/// takes effect upon property resolution.
#[inline]
fn scale_para_props(p: &mut ParaProps, space_factor: f32) {
    if space_factor < 1.0 - f32::EPSILON {
        if p.space_before_auto == Some(true) {
            p.space_before_auto = Some(false);
            p.space_before = Some(scale_spacing(AUTO_SPACING, space_factor));
        } else if let Some(sb) = p.space_before.as_mut() {
            if sb.is_finite() {
                *sb = scale_spacing(*sb, space_factor);
            } else {
                *sb = 0.0;
            }
        }

        if p.space_after_auto == Some(true) {
            p.space_after_auto = Some(false);
            p.space_after = Some(scale_spacing(AUTO_SPACING, space_factor));
        } else if let Some(sa) = p.space_after.as_mut() {
            if sa.is_finite() {
                *sa = scale_spacing(*sa, space_factor);
            } else {
                *sa = 0.0;
            }
        }

        if let Some(ls) = p.line_spacing.as_mut() {
            *ls = scale_line_spacing(*ls, space_factor);
        }
    }
}

/// Freeze paragraph runs and paragraph spacing in a header/footer paragraph using
/// table-aware resolution when inside a table cell. Preserves inherited automatic spacing.
fn freeze_para_typography(p: &mut Paragraph, old_styles: &StyleSheet, table_ctx: Option<(&CharProps, &ParaProps)>) {
    let style = p.props.style.clone();
    let (t_chr, t_para) = match table_ctx {
        Some((c, pr)) => (Some(c), Some(pr)),
        None => (None, None),
    };
    for r in &mut p.runs {
        if r.props.size.is_none() {
            let resolved = old_styles.resolve_char_in(style.as_deref(), t_chr, &r.props).size;
            r.props.size = Some(resolved);
        }
        if r.props.size_cs.is_none() {
            let resolved_cs = old_styles.resolve_char_in(style.as_deref(), t_chr, &r.props).size_cs;
            r.props.size_cs = Some(resolved_cs);
        }
    }
    if p.mark.size.is_none() {
        let resolved = old_styles.resolve_char_in(style.as_deref(), t_chr, &p.mark).size;
        p.mark.size = Some(resolved);
    }
    if p.mark.size_cs.is_none() {
        let resolved_cs = old_styles.resolve_char_in(style.as_deref(), t_chr, &p.mark).size_cs;
        p.mark.size_cs = Some(resolved_cs);
    }
    let resolved_p = old_styles.resolve_para_in(&p.props, t_para);
    if p.props.space_before.is_none() && p.props.space_before_auto != Some(true) {
        if resolved_p.before_auto {
            p.props.space_before_auto = Some(true);
        } else {
            p.props.space_before = Some(resolved_p.space_before);
        }
    }
    if p.props.space_after.is_none() && p.props.space_after_auto != Some(true) {
        if resolved_p.after_auto {
            p.props.space_after_auto = Some(true);
        } else {
            p.props.space_after = Some(resolved_p.space_after);
        }
    }
    if p.props.line_spacing.is_none() {
        p.props.line_spacing = Some(resolved_p.line_spacing);
    }
}

/// Freeze table cell typography in a header/footer table, carrying conditional table style parts.
fn freeze_table_typography(table: &mut Table, old_styles: &StyleSheet, depth: usize) {
    let st_props = table.props.style.as_deref().and_then(|id| old_styles.table_style(id));
    let nrows = table.rows.len();
    let ncols = table.cols().max(1);
    let band_rows = st_props.as_ref().and_then(|s| s.parts.band_size).unwrap_or(0);
    let band_cols = st_props.as_ref().and_then(|s| s.parts.col_band_size).unwrap_or(0);

    for (ri, row) in table.rows.iter_mut().enumerate() {
        let mut g = row.first_grid();
        for cell in &mut row.cells {
            let span = cell.span();
            let (cell_chr, cell_para) = if let Some(st) = &st_props {
                let region = PartSet::of_cell(&table.props.look, (band_rows, band_cols), (ri, nrows), (g, span, ncols));
                let mut chr = CharProps::default();
                let mut para = st.para.clone();
                for part in region.iter() {
                    let f = st.part(part);
                    chr.overlay(&f.chr);
                    if f.align.is_some() {
                        para.align = f.align;
                    }
                }
                (Some(chr), Some(para))
            } else {
                (None, None)
            };

            let ctx = match (&cell_chr, &cell_para) {
                (Some(c), Some(p)) => Some((c, p)),
                _ => None,
            };

            for b in &mut cell.blocks {
                freeze_header_footer_block(Arc::make_mut(b), old_styles, ctx, depth + 1);
            }

            g = g.saturating_add(span);
        }
    }
}

/// Freeze typography in header or footer blocks using table-aware resolution,
/// preserving explicit and inherited styles so headers/footers remain completely unchanged.
fn freeze_header_footer_block(block: &mut Block, old_styles: &StyleSheet, table_ctx: Option<(&CharProps, &ParaProps)>, depth: usize) {
    if depth > MAX_TABLE_DEPTH {
        return;
    }
    match block {
        Block::Para(p) => {
            freeze_para_typography(p, old_styles, table_ctx);
        }
        Block::Table(t) => {
            freeze_table_typography(t, old_styles, depth);
        }
    }
}

/// Identify all text-box story IDs transitively reached from headers and footers.
fn header_footer_text_boxes(doc: &Document) -> BTreeSet<u32> {
    let mut header_boxes = BTreeSet::new();
    let mut queue = Vec::new();

    for part in doc.parts.values() {
        if matches!(part.kind, PartKind::Header | PartKind::Footer) {
            for b in &part.blocks {
                crate::edit::each_para(b, 0, &mut |p| {
                    for obj in &p.objects {
                        for box_id in obj.text_boxes() {
                            if header_boxes.insert(box_id) {
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
            for b in &part.blocks {
                crate::edit::each_para(b, 0, &mut |p| {
                    for obj in &p.objects {
                        for child_id in obj.text_boxes() {
                            if header_boxes.insert(child_id) {
                                queue.push(child_id);
                            }
                        }
                    }
                });
            }
        }
    }

    header_boxes
}

/// Shrinks font sizes across the document by `delta_pt` points and scales paragraph
/// spacing by `space_factor`.
///
/// Preserves relative typography hierarchies between headings, body text, and direct run formatting.
/// Clamps font sizes to `[4.0, 1638.0]` pt and rounds to 0.5 pt (half-points).
/// Spacing factor is clamped to `[0.80, 1.0]`.
/// Headers, footers, and text boxes anchored within them are explicitly excluded and retain their exact original sizes and spacing.
/// Footnotes and body text boxes are included and shrink alongside body text.
pub fn shrink_document_typography(doc: &mut Document, delta_pt: f32, space_factor: f32) {
    if !delta_pt.is_finite() || !space_factor.is_finite() {
        return;
    }
    let delta_pt = delta_pt.max(0.0);
    let space_factor = space_factor.clamp(MIN_SHRINK_SPACE_FACTOR, MAX_SHRINK_SPACE_FACTOR);

    if delta_pt == 0.0 && (space_factor - 1.0).abs() < f32::EPSILON {
        return;
    }

    // Identify text boxes anchored within headers or footers so they are frozen and excluded from shrinking.
    let hf_text_boxes = header_footer_text_boxes(doc);

    // 1. Freeze header, footer, and header-anchored text box typography before modifying styles,
    // so fixed framing stories never shrink even if they inherit from default or shared styles.
    // Table-aware resolution ensures cells in header tables preserve their table style typography.
    let old_styles = doc.styles.clone();
    for (id, part) in &mut doc.parts {
        if matches!(part.kind, PartKind::Header | PartKind::Footer) || hf_text_boxes.contains(id) {
            for b in &mut part.blocks {
                freeze_header_footer_block(Arc::make_mut(b), &old_styles, None, 0);
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

    // 5. Traverse secondary editable stories (footnotes, text boxes), excluding headers, footers,
    // and text boxes anchored within headers or footers.
    for (id, part) in &mut doc.parts {
        if matches!(part.kind, PartKind::Header | PartKind::Footer) || hf_text_boxes.contains(id) {
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
    use crate::styles::{Style, StyleKind};
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
        // 1.15 * 0.9 = 1.035 -> round to 2 decimals = 1.04
        assert_eq!(p_after.props.line_spacing, Some(LineSpacing::Multiple(1.04)));
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

    #[test]
    fn test_shrink_automatic_spacing() {
        let mut doc = Document::new();
        let mut p = Paragraph::with_text("Auto spaced paragraph", CharProps::default());
        p.props.space_before_auto = Some(true);
        p.props.space_after_auto = Some(true);
        doc.body = vec![para_block(p)];

        shrink_document_typography(&mut doc, 0.5, 0.9);

        let p_after = doc.body[0].as_para().unwrap();
        // AUTO_SPACING is 14.0 pt. 14.0 * 0.9 = 12.6 -> 12.5 pt
        assert_eq!(p_after.props.space_before_auto, Some(false));
        assert_eq!(p_after.props.space_before, Some(12.5));
        assert_eq!(p_after.props.space_after_auto, Some(false));
        assert_eq!(p_after.props.space_after, Some(12.5));

        // When resolved, paragraph resolves to 12.5 pt spacing rather than unscaled 14.0 pt
        let resolved = doc.styles.resolve_para(&p_after.props);
        assert_eq!(resolved.space_before, 12.5);
        assert_eq!(resolved.space_after, 12.5);
    }

    #[test]
    fn test_shrink_hostile_nan_values() {
        let mut doc = Document::new();
        let mut p = Paragraph::with_text("Hostile values", CharProps { size: Some(f32::NAN), ..Default::default() });
        p.props.space_before = Some(f32::NAN);
        p.props.space_after = Some(f32::NAN);
        p.props.line_spacing = Some(LineSpacing::Multiple(f32::NAN));
        doc.body = vec![para_block(p)];

        shrink_document_typography(&mut doc, 0.5, 0.9);

        let p_after = doc.body[0].as_para().unwrap();
        assert_eq!(p_after.runs[0].props.size, Some(MIN_SHRINK_FONT_SIZE));
        assert_eq!(p_after.props.space_before, Some(0.0));
        assert_eq!(p_after.props.space_after, Some(0.0));
        assert_eq!(p_after.props.line_spacing, Some(LineSpacing::Multiple(1.0)));
    }

    #[test]
    fn test_shrink_spacing_factor_clamp_range() {
        let mut doc = Document::new();
        let mut p = Paragraph::with_text("Spacing clamp", CharProps::default());
        p.props.space_before = Some(20.0);
        doc.body = vec![para_block(p)];

        // Supply 0.1, which is below MIN_SHRINK_SPACE_FACTOR (0.80)
        shrink_document_typography(&mut doc, 0.5, 0.1);

        let p_after = doc.body[0].as_para().unwrap();
        // 20.0 * 0.80 = 16.0 pt (not 20.0 * 0.1 = 2.0 pt)
        assert_eq!(p_after.props.space_before, Some(16.0));
    }

    #[test]
    fn test_shrink_header_table_cell_style_preserved() {
        let mut doc = Document::new();
        // Register a table style with explicit character font size
        let custom_table_style = Style {
            id: "HeaderTableStyle".into(),
            name: "Header Table Style".into(),
            kind: StyleKind::Table,
            chr: CharProps { size: Some(16.0), ..Default::default() },
            ..Default::default()
        };
        Arc::make_mut(&mut doc.styles).upsert(custom_table_style);

        // Header table whose cell inherits 16.0 pt from HeaderTableStyle
        let mut table = Table::new(1, 1, 100.0);
        table.props.style = Some("HeaderTableStyle".into());
        let cell_p = Paragraph::with_text("Header cell text", CharProps::default());
        table.rows[0].cells[0].blocks = vec![para_block(cell_p)];

        let h_id = doc.add_part(PartKind::Header, vec![Arc::new(Block::Table(table))]);

        // Shrink document typography
        shrink_document_typography(&mut doc, 1.0, 0.85);

        // Header cell must retain its 16.0 pt size from the table style, not document defaults or shrunk
        let h_part = doc.parts.get(&h_id).unwrap();
        let tbl = h_part.blocks[0].as_table().unwrap();
        let frozen_cell_p = tbl.rows[0].cells[0].blocks[0].as_para().unwrap();
        assert_eq!(frozen_cell_p.runs[0].props.size, Some(16.0));
    }

    #[test]
    fn test_scale_size_never_increases() {
        // Fractional font sizes: rounding to 0.5 pt must never increase the original size
        assert_eq!(scale_size(12.4, 0.1), 12.0);
        assert_eq!(scale_size(12.4, 0.0), 12.0);
        assert_eq!(scale_size(12.5, 0.0), 12.5);
        assert_eq!(scale_size(12.6, 0.1), 12.5);
        assert_eq!(scale_size(4.3, 0.0), 4.0);
        assert_eq!(scale_size(4.0, 0.5), 4.0);
    }

    #[test]
    fn test_freeze_preserves_inherited_auto_spacing() {
        let mut doc = Document::new();
        // Style with automatic spacing
        let auto_style = Style {
            id: "AutoStyle".into(),
            name: "Auto Style".into(),
            kind: StyleKind::Paragraph,
            para: ParaProps { space_before_auto: Some(true), space_after_auto: Some(true), ..Default::default() },
            ..Default::default()
        };
        Arc::make_mut(&mut doc.styles).upsert(auto_style);

        let p = Paragraph::with_text("Header auto text", CharProps::default()).styled("AutoStyle");
        let h_id = doc.add_part(PartKind::Header, vec![para_block(p)]);

        shrink_document_typography(&mut doc, 0.5, 0.9);

        let h_part = doc.parts.get(&h_id).unwrap();
        let h_p = h_part.blocks[0].as_para().unwrap();
        // Inherited auto flags are preserved on the header paragraph
        assert_eq!(h_p.props.space_before_auto, Some(true));
        assert_eq!(h_p.props.space_after_auto, Some(true));
    }

    #[test]
    fn test_shrink_excludes_header_text_boxes() {
        use crate::para::{InlineObject, ShapeKind};

        let shape = |story| InlineObject::Shape {
            kind: ShapeKind::TextBox,
            w: 50.0,
            h: 20.0,
            fill: None,
            stroke: None,
            stroke_width: 0.0,
            float: Default::default(),
            story: Some(story),
            freeform: None,
            effects: Default::default(),
            extra: Default::default(),
        };

        let mut doc = Document::new();
        // Text box anchored in header
        let h_box_id = doc.add_part(
            PartKind::TextBox,
            vec![para_block(Paragraph::with_text("Header box text", CharProps { size: Some(14.0), ..Default::default() }))],
        );
        let mut h_para = Paragraph::with_text("Header text with box", CharProps::default());
        h_para.objects.push(shape(h_box_id));
        doc.add_part(PartKind::Header, vec![para_block(h_para)]);

        // Text box anchored in body
        let b_box_id = doc.add_part(
            PartKind::TextBox,
            vec![para_block(Paragraph::with_text("Body box text", CharProps { size: Some(14.0), ..Default::default() }))],
        );
        let mut b_para = Paragraph::with_text("Body text with box", CharProps::default());
        b_para.objects.push(shape(b_box_id));
        doc.body = vec![para_block(b_para)];

        shrink_document_typography(&mut doc, 1.0, 1.0);

        // Header text box retained its 14.0 pt size
        let h_box = doc.parts.get(&h_box_id).unwrap();
        let h_box_p = h_box.blocks[0].as_para().unwrap();
        assert_eq!(h_box_p.runs[0].props.size, Some(14.0));

        // Body text box was shrunk down to 13.0 pt
        let b_box = doc.parts.get(&b_box_id).unwrap();
        let b_box_p = b_box.blocks[0].as_para().unwrap();
        assert_eq!(b_box_p.runs[0].props.size, Some(13.0));
    }
}
