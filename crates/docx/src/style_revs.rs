//! Style-definition revision helpers that extend `craft-docx` with WordCraft-specific state:
//! - Preserving level-only list inheritance provenance (`numbering_from_base`, `old_numbering_from_base`)
//!   across `word/styles.xml` read and write.
//! - Re-resolving pre-change level-only list references along the pre-change based-on chain.
//! - Overlaying `w:tblStylePr[@w:type="wholeTable"]` paragraph/run properties onto `ch.old` so
//!   rejecting a table style's tracked property change preserves `wholeTable` defaults.

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::sync::Arc;

use craft_docx::DocxError;
use quick_xml::events::Event;
use quick_xml::{Reader as XmlReader, Writer as XmlWriter};
use wordcraft_doc::Document;
use wordcraft_doc::props::NumRef;
use wordcraft_doc::styles::{StyleKind, StyleSheet};
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

use crate::{ReadOptions, is_encrypted};

/// Maximum `word/styles.xml` size processed for style revision metadata (32 MiB).
const MAX_STYLES_XML: u64 = 32 << 20;
/// Maximum `w:style` elements scanned per stylesheet.
const MAX_STYLES: usize = 10_000;
/// Maximum XML nesting depth tracked in `styles.xml`.
const MAX_XML_DEPTH: usize = 64;
/// Maximum `based_on` chain traversal steps.
const MAX_CHAIN_DEPTH: usize = 32;

const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

#[derive(Clone, Copy, Default)]
struct NumPrScan {
    has_ilvl: bool,
    ilvl: u8,
    has_num_id: bool,
}

impl NumPrScan {
    fn is_level_only(self) -> bool {
        self.has_ilvl && !self.has_num_id
    }
}

#[derive(Clone, Default)]
struct StyleXmlInfo {
    style_id: String,
    kind: StyleKind,
    is_default: bool,
    name: Option<String>,
    live_num: Option<NumPrScan>,
    old_num: Option<NumPrScan>,
    has_change: bool,
    whole_ppr: Vec<u8>,
    whole_rpr: Vec<u8>,
}

/// Rewrites `word/styles.xml` inside serialized DOCX ZIP bytes when any style carries
/// `numbering_from_base` or `old_numbering_from_base`, omitting `w:numId` so reload keeps
/// the level-only inheritance provenance.
pub(crate) fn postprocess_written_docx(doc: &Document, zip_bytes: Vec<u8>) -> Result<Vec<u8>, DocxError> {
    let sheet = &doc.styles;
    let needs_rewrite =
        sheet.styles.iter().any(|s| s.numbering_from_base || (s.old_numbering_from_base && (s.para_change.is_some() || s.para.fmt_change.is_some())));
    if !needs_rewrite || !zip_bytes.starts_with(b"PK") {
        return Ok(zip_bytes);
    }

    let mut archive = match ZipArchive::new(Cursor::new(&zip_bytes)) {
        Ok(a) => a,
        Err(_) => return Ok(zip_bytes),
    };
    let styles_idx = (0..archive.len()).find(|&i| archive.by_index(i).ok().is_some_and(|f| f.name() == "word/styles.xml"));
    let Some(styles_idx) = styles_idx else {
        return Ok(zip_bytes);
    };

    let raw_styles = {
        let entry = archive.by_index(styles_idx).map_err(|e| DocxError::Xml(format!("styles.xml zip entry: {e}")))?;
        let mut buf = Vec::new();
        entry.take(MAX_STYLES_XML + 1).read_to_end(&mut buf).map_err(|e| DocxError::Xml(format!("read styles.xml: {e}")))?;
        if buf.len() as u64 > MAX_STYLES_XML {
            return Ok(zip_bytes);
        }
        buf
    };

    let rewritten = strip_inherited_num_ids(&raw_styles, sheet)?;
    let mut out = Cursor::new(Vec::with_capacity(zip_bytes.len()));
    {
        let mut writer = ZipWriter::new(&mut out);
        for i in 0..archive.len() {
            if i == styles_idx {
                writer
                    .start_file("word/styles.xml", SimpleFileOptions::default())
                    .map_err(|e| DocxError::Xml(format!("write styles.xml entry: {e}")))?;
                writer.write_all(&rewritten).map_err(|e| DocxError::Xml(format!("write styles.xml bytes: {e}")))?;
            } else {
                let entry = archive.by_index(i).map_err(|e| DocxError::Xml(format!("copy zip entry: {e}")))?;
                writer.raw_copy_file(entry).map_err(|e| DocxError::Xml(format!("raw copy zip entry: {e}")))?;
            }
        }
        writer.finish().map_err(|e| DocxError::Xml(format!("finish docx zip: {e}")))?;
    }
    Ok(out.into_inner())
}

/// Restores `numbering_from_base`, `old_numbering_from_base`, pre-change level-only numbering,
/// and `wholeTable` overlays on style change snapshots from `word/styles.xml`.
pub(crate) fn postprocess_read_docx(bytes: &[u8], opts: &ReadOptions<'_>, doc: &mut Document) -> Result<(), DocxError> {
    if doc.styles.styles.is_empty() {
        return Ok(());
    }
    let decrypted;
    let pkg_bytes = if is_encrypted(bytes) {
        decrypted = crate::decrypt(bytes, opts.password)?;
        decrypted.as_slice()
    } else {
        bytes
    };
    let Ok(mut archive) = ZipArchive::new(Cursor::new(pkg_bytes)) else {
        return Ok(());
    };
    let Ok(entry) = archive.by_name("word/styles.xml") else {
        return Ok(());
    };
    let mut xml = Vec::new();
    if entry.take(MAX_STYLES_XML + 1).read_to_end(&mut xml).is_err() || xml.len() as u64 > MAX_STYLES_XML {
        return Ok(());
    }

    let scanned = scan_styles_xml(&xml);
    if scanned.is_empty() {
        return Ok(());
    }
    let aliases = compute_style_aliases(&scanned);
    let mut by_id: HashMap<String, StyleXmlInfo> = HashMap::with_capacity(scanned.len());
    for info in scanned {
        let canonical = aliases.get(&info.style_id).cloned().unwrap_or_else(|| info.style_id.clone());
        by_id.insert(canonical, info);
    }

    let sheet = Arc::make_mut(&mut doc.styles);
    for st in &mut sheet.styles {
        if let Some(info) = by_id.get(&st.id) {
            st.numbering_from_base = info.live_num.is_some_and(NumPrScan::is_level_only);
            st.old_numbering_from_base = info.old_num.is_some_and(NumPrScan::is_level_only);
        }
    }

    // Re-resolve old level-only numbering against the pre-change based-on chain so intermediate
    // unchanged styles with live inherited numbering don't shadow the base style's old list.
    let resolved_old: Vec<(usize, Option<NumRef>)> = sheet
        .styles
        .iter()
        .enumerate()
        .filter(|(_, s)| s.old_numbering_from_base)
        .filter_map(|(i, s)| {
            let info = by_id.get(&s.id)?;
            let level = info
                .old_num
                .map(|n| n.ilvl)
                .or_else(|| s.para_change.as_ref().or(s.para.fmt_change.as_ref()).and_then(|c| c.old.numbering.map(|n| n.level)))?;
            let ch = s.para_change.as_ref().or(s.para.fmt_change.as_ref())?;
            let base = ch.old.style.as_deref().or(s.based_on.as_deref())?;
            let num_ref = chain_list_before(sheet, base).map(|num| NumRef { num, level });
            Some((i, num_ref))
        })
        .collect();
    for (idx, num_ref) in resolved_old {
        if let Some(st) = sheet.styles.get_mut(idx) {
            if let Some(ch) = st.para_change.as_mut() {
                ch.old.numbering = num_ref;
            }
            if let Some(ch) = st.para.fmt_change.as_mut() {
                ch.old.numbering = num_ref;
            }
        }
    }

    // Overlay `wholeTable` paragraph/run properties onto `ch.old` for table styles with changes.
    for st in &mut sheet.styles {
        if st.kind != StyleKind::Table || (st.para_change.is_none() && st.chr_change.is_none()) {
            continue;
        }
        let Some(info) = by_id.get(&st.id) else { continue };
        if info.whole_ppr.is_empty() && info.whole_rpr.is_empty() {
            continue;
        }
        if let Some((wt_para, wt_chr)) = parse_whole_table_props(&info.whole_ppr, &info.whole_rpr) {
            if let Some(ch) = st.para_change.as_mut() {
                ch.old.overlay(&wt_para);
            }
            if let Some(ch) = st.para.fmt_change.as_mut() {
                ch.old.overlay(&wt_para);
            }
            if let Some(ch) = st.chr_change.as_mut() {
                ch.old.overlay(&wt_chr);
            }
            if let Some(ch) = st.chr.fmt_change.as_mut() {
                ch.old.overlay(&wt_chr);
            }
        }
    }

    Ok(())
}

/// Nearest explicit list ID in `style`'s based-on chain in its pre-change state.
fn chain_list_before(sheet: &StyleSheet, style: &str) -> Option<u32> {
    let mut cur = Some(style);
    for _ in 0..MAX_CHAIN_DEPTH {
        let Some(id) = cur else { break };
        let st = sheet.styles.iter().find(|s| s.id == id)?;
        let inherited_here =
            if st.para_change.is_some() || st.para.fmt_change.is_some() { st.old_numbering_from_base } else { st.numbering_from_base };
        if !inherited_here {
            let num = st.para_change.as_ref().or(st.para.fmt_change.as_ref()).map_or(st.para.numbering, |c| c.old.numbering).map(|n| n.num);
            if let Some(n) = num {
                return Some(n);
            }
        }
        cur = st.based_on.as_deref();
    }
    None
}

fn strip_inherited_num_ids(xml: &[u8], sheet: &StyleSheet) -> Result<Vec<u8>, DocxError> {
    #[derive(Clone, Copy)]
    struct StyleFlags {
        live: bool,
        old: bool,
    }
    enum Frame {
        Other,
        Style(Option<StyleFlags>),
        StylePPr,
        PPrChange,
        ChangePPr,
        NumPr { strip: bool },
        SkipNumId,
    }

    let mut reader = XmlReader::from_reader(xml);
    let mut writer = XmlWriter::new(Vec::with_capacity(xml.len()));
    let mut stack: Vec<Frame> = Vec::with_capacity(16);

    loop {
        let ev = reader.read_event().map_err(|e| DocxError::Xml(format!("styles.xml parse: {e}")))?;
        match ev {
            Event::Eof => break,
            Event::Start(e) => {
                if stack.len() >= MAX_XML_DEPTH {
                    return Err(DocxError::Limit("styles.xml nesting depth exceeded".into()));
                }
                let local = e.local_name();
                let next = match stack.last() {
                    Some(Frame::SkipNumId) => Frame::SkipNumId,
                    Some(Frame::NumPr { strip: true }) if local.as_ref() == b"numId" => Frame::SkipNumId,
                    Some(Frame::StylePPr) if local.as_ref() == b"pPrChange" => Frame::PPrChange,
                    Some(Frame::PPrChange) if local.as_ref() == b"pPr" => Frame::ChangePPr,
                    Some(Frame::StylePPr) if local.as_ref() == b"numPr" => {
                        let strip = stack.iter().rev().find_map(|f| match f {
                            Frame::Style(flags) => Some(flags.is_some_and(|fl| fl.live)),
                            _ => None,
                        });
                        Frame::NumPr { strip: strip.unwrap_or(false) }
                    }
                    Some(Frame::ChangePPr) if local.as_ref() == b"numPr" => {
                        let strip = stack.iter().rev().find_map(|f| match f {
                            Frame::Style(flags) => Some(flags.is_some_and(|fl| fl.old)),
                            _ => None,
                        });
                        Frame::NumPr { strip: strip.unwrap_or(false) }
                    }
                    Some(Frame::Style(_)) if local.as_ref() == b"pPr" => Frame::StylePPr,
                    _ if stack.len() == 1 && local.as_ref() == b"style" => {
                        let flags = attr_value(&e, b"styleId")
                            .and_then(|id| sheet.get(&id).map(|st| StyleFlags { live: st.numbering_from_base, old: st.old_numbering_from_base }));
                        Frame::Style(flags)
                    }
                    _ => Frame::Other,
                };
                let skipping = matches!(next, Frame::SkipNumId);
                stack.push(next);
                if !skipping {
                    writer.write_event(Event::Start(e)).map_err(|err| DocxError::Xml(format!("styles.xml write: {err}")))?;
                }
            }
            Event::Empty(e) => {
                let skip = matches!(stack.last(), Some(Frame::SkipNumId))
                    || (matches!(stack.last(), Some(Frame::NumPr { strip: true })) && e.local_name().as_ref() == b"numId");
                if !skip {
                    writer.write_event(Event::Empty(e)).map_err(|err| DocxError::Xml(format!("styles.xml write: {err}")))?;
                }
            }
            Event::End(e) => {
                let popped = stack.pop();
                if !matches!(popped, Some(Frame::SkipNumId)) {
                    writer.write_event(Event::End(e)).map_err(|err| DocxError::Xml(format!("styles.xml write: {err}")))?;
                }
            }
            other => {
                if !matches!(stack.last(), Some(Frame::SkipNumId)) {
                    writer.write_event(other).map_err(|err| DocxError::Xml(format!("styles.xml write: {err}")))?;
                }
            }
        }
    }
    Ok(writer.into_inner())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanFrame {
    Root,
    Style,
    StylePPr,
    LiveNumPr,
    PPrChange,
    ChangePPr,
    OldNumPr,
    WholeTable,
    WholePPr,
    WholeRPr,
    Other,
}

#[derive(Clone, Copy)]
enum WholePart {
    PPr,
    RPr,
}

struct StyleXmlScanner {
    out: Vec<StyleXmlInfo>,
    stack: Vec<ScanFrame>,
    cur: Option<StyleXmlInfo>,
    whole_writer: Option<(WholePart, usize, XmlWriter<Vec<u8>>)>,
}

impl StyleXmlScanner {
    fn new() -> Self {
        Self { out: Vec::new(), stack: Vec::with_capacity(16), cur: None, whole_writer: None }
    }

    fn write_captured(&mut self, ev: Event<'_>) {
        if let Some((_, _, w)) = self.whole_writer.as_mut() {
            let _ = w.write_event(ev);
        }
    }

    fn on_start(&mut self, e: quick_xml::events::BytesStart<'_>) -> bool {
        if self.stack.len() >= MAX_XML_DEPTH {
            return false;
        }
        let local = e.local_name();
        let parent = self.stack.last().copied();
        let frame = match parent {
            None => ScanFrame::Root,
            Some(ScanFrame::Root) if local.as_ref() == b"style" => {
                if self.out.len() >= MAX_STYLES {
                    return false;
                }
                self.start_style(&e);
                ScanFrame::Style
            }
            Some(ScanFrame::Style) if local.as_ref() == b"pPr" => ScanFrame::StylePPr,
            Some(ScanFrame::Style) if local.as_ref() == b"tblStylePr" && attr_value(&e, b"type").as_deref() == Some("wholeTable") => {
                ScanFrame::WholeTable
            }
            Some(ScanFrame::StylePPr) if local.as_ref() == b"numPr" => {
                if let Some(info) = self.cur.as_mut() {
                    info.live_num.get_or_insert_with(NumPrScan::default);
                }
                ScanFrame::LiveNumPr
            }
            Some(ScanFrame::StylePPr) if local.as_ref() == b"pPrChange" => {
                if let Some(info) = self.cur.as_mut() {
                    info.has_change = true;
                }
                ScanFrame::PPrChange
            }
            Some(ScanFrame::PPrChange) if local.as_ref() == b"pPr" => ScanFrame::ChangePPr,
            Some(ScanFrame::ChangePPr) if local.as_ref() == b"numPr" => {
                if let Some(info) = self.cur.as_mut() {
                    info.old_num.get_or_insert_with(NumPrScan::default);
                }
                ScanFrame::OldNumPr
            }
            Some(ScanFrame::WholeTable) if local.as_ref() == b"pPr" => {
                self.begin_whole(WholePart::PPr);
                ScanFrame::WholePPr
            }
            Some(ScanFrame::WholeTable) if local.as_ref() == b"rPr" => {
                self.begin_whole(WholePart::RPr);
                ScanFrame::WholeRPr
            }
            _ => ScanFrame::Other,
        };
        if let Some((_, depth, w)) = self.whole_writer.as_mut()
            && self.stack.len() >= *depth
        {
            let _ = w.write_event(Event::Start(e.borrow()));
        }
        self.stack.push(frame);
        true
    }

    fn start_style(&mut self, e: &quick_xml::events::BytesStart<'_>) {
        let Some(id) = attr_value(e, b"styleId").filter(|s| !s.is_empty()) else {
            self.cur = None;
            return;
        };
        let kind = match attr_value(e, b"type").as_deref() {
            Some("character") => StyleKind::Character,
            Some("table") => StyleKind::Table,
            Some("numbering") => StyleKind::Numbering,
            _ => StyleKind::Paragraph,
        };
        let is_default = attr_value(e, b"default").is_some_and(|v| v == "1" || v == "true");
        self.cur = Some(StyleXmlInfo { style_id: id, kind, is_default, ..Default::default() });
    }

    fn begin_whole(&mut self, part: WholePart) {
        if self.cur.as_ref().is_some_and(|i| i.kind == StyleKind::Table) {
            self.whole_writer = Some((part, self.stack.len(), XmlWriter::new(Vec::new())));
        }
    }

    fn on_empty(&mut self, e: quick_xml::events::BytesStart<'_>) {
        let local = e.local_name();
        if let Some(info) = self.cur.as_mut() {
            match self.stack.last().copied() {
                Some(ScanFrame::Style) if local.as_ref() == b"name" => info.name = attr_value(&e, b"val"),
                Some(ScanFrame::LiveNumPr) => update_num_pr(info.live_num.get_or_insert_with(NumPrScan::default), local.as_ref(), &e),
                Some(ScanFrame::OldNumPr) => update_num_pr(info.old_num.get_or_insert_with(NumPrScan::default), local.as_ref(), &e),
                Some(ScanFrame::StylePPr) if local.as_ref() == b"pPrChange" => info.has_change = true,
                _ => {}
            }
        }
        self.write_captured(Event::Empty(e.borrow()));
    }

    fn on_end(&mut self, e: quick_xml::events::BytesEnd<'_>) {
        if let Some((_, depth, w)) = self.whole_writer.as_mut()
            && self.stack.len().saturating_sub(1) >= *depth
        {
            let _ = w.write_event(Event::End(e.borrow()));
        }
        match self.stack.pop() {
            Some(ScanFrame::WholePPr | ScanFrame::WholeRPr) => {
                if let (Some((part, _, w)), Some(info)) = (self.whole_writer.take(), self.cur.as_mut()) {
                    match part {
                        WholePart::PPr => info.whole_ppr = w.into_inner(),
                        WholePart::RPr => info.whole_rpr = w.into_inner(),
                    }
                }
            }
            Some(ScanFrame::Style) => {
                if let Some(info) = self.cur.take() {
                    self.out.push(info);
                }
            }
            _ => {}
        }
    }
}

fn scan_styles_xml(xml: &[u8]) -> Vec<StyleXmlInfo> {
    let mut reader = XmlReader::from_reader(xml);
    let mut scanner = StyleXmlScanner::new();
    while let Ok(ev) = reader.read_event() {
        match ev {
            Event::Eof => break,
            Event::Start(e) => {
                if !scanner.on_start(e) {
                    break;
                }
            }
            Event::Empty(e) => scanner.on_empty(e),
            Event::End(e) => scanner.on_end(e),
            other => scanner.write_captured(other.borrow()),
        }
    }
    scanner.out
}

fn update_num_pr(scan: &mut NumPrScan, local: &[u8], e: &quick_xml::events::BytesStart<'_>) {
    if local == b"ilvl" {
        scan.has_ilvl = true;
        if let Some(v) = attr_value(e, b"val").and_then(|s| s.trim().parse::<u8>().ok()) {
            scan.ilvl = v.min(8);
        }
    } else if local == b"numId" {
        scan.has_num_id = true;
    }
}

fn compute_style_aliases(styles: &[StyleXmlInfo]) -> HashMap<String, String> {
    let mut alias = HashMap::new();
    let has_normal = styles.iter().any(|s| s.style_id == "Normal");
    if !has_normal && let Some(def) = styles.iter().find(|s| s.kind == StyleKind::Paragraph && s.is_default) {
        alias.insert(def.style_id.clone(), "Normal".to_string());
    }
    let mut taken: Vec<String> = styles.iter().map(|s| s.style_id.clone()).collect();
    for s in styles.iter().filter(|s| s.kind == StyleKind::Paragraph) {
        let Some(name) = s.name.as_deref() else { continue };
        let canonical = match name.trim().to_ascii_lowercase().as_str() {
            "toc heading" => "TOCHeading".to_string(),
            n => match n.strip_prefix("toc ").and_then(|d| d.parse::<u8>().ok()).filter(|d| (1..=9).contains(d)) {
                Some(d) => format!("TOC{d}"),
                None => continue,
            },
        };
        if !taken.contains(&canonical) && !alias.contains_key(&s.style_id) {
            taken.push(canonical.clone());
            alias.insert(s.style_id.clone(), canonical);
        }
    }
    alias
}

fn parse_whole_table_props(ppr_xml: &[u8], rpr_xml: &[u8]) -> Option<(wordcraft_doc::props::ParaProps, wordcraft_doc::props::CharProps)> {
    const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    const DOC_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;
    const CT: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>"#;

    let ppr = String::from_utf8_lossy(ppr_xml);
    let rpr = String::from_utf8_lossy(rpr_xml);
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="{W_NS}"><w:style w:type="paragraph" w:styleId="_WT">{ppr}{rpr}</w:style></w:styles>"#
    );
    let doc_xml =
        format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W_NS}"><w:body><w:p/></w:body></w:document>"#);

    let mut zw = ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in [
        ("[Content_Types].xml", CT.as_bytes()),
        ("_rels/.rels", ROOT_RELS.as_bytes()),
        ("word/_rels/document.xml.rels", DOC_RELS.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
        ("word/document.xml", doc_xml.as_bytes()),
    ] {
        zw.start_file(name, opts).ok()?;
        zw.write_all(bytes).ok()?;
    }
    let zip_bytes = zw.finish().ok()?.into_inner();
    let shared = craft_docx::read(&zip_bytes).ok()?;
    let doc = crate::from_shared_model(shared).ok()?;
    let st = doc.styles.get("_WT")?;
    Some((st.para.clone(), st.chr.clone()))
}

fn attr_value(e: &quick_xml::events::BytesStart<'_>, local_target: &[u8]) -> Option<String> {
    for attr in e.attributes().flatten() {
        if attr.key.local_name().as_ref() == local_target {
            return String::from_utf8(attr.value.into_owned()).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_table_overlay_test() {
        let styles = br#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
         <w:style w:type="table" w:styleId="Whole"><w:name w:val="Whole"/>
          <w:pPr><w:jc w:val="left"/><w:pPrChange w:id="1" w:author="Editor" w:date="2026-01-01T00:00:00Z"><w:pPr><w:jc w:val="right"/></w:pPr></w:pPrChange></w:pPr>
          <w:rPr><w:i/><w:rPrChange w:id="2" w:author="Editor" w:date="2026-01-01T00:00:00Z"><w:rPr><w:b/></w:rPr></w:rPrChange></w:rPr>
          <w:tblStylePr w:type="wholeTable"><w:pPr><w:jc w:val="center"/></w:pPr><w:rPr><w:color w:val="1F4E79"/></w:rPr></w:tblStylePr>
         </w:style>
        </w:styles>"#;
        let scanned = scan_styles_xml(styles);
        assert_eq!(scanned.len(), 1);
        let ppr = String::from_utf8_lossy(&scanned[0].whole_ppr);
        let rpr = String::from_utf8_lossy(&scanned[0].whole_rpr);
        let parsed = parse_whole_table_props(&scanned[0].whole_ppr, &scanned[0].whole_rpr);
        assert!(parsed.is_some(), "ppr={ppr:?} rpr={rpr:?} — read_xml failed");
        let (p, c) = parsed.unwrap();
        assert_eq!(p.align, Some(wordcraft_doc::props::Align::Center), "ppr={ppr:?}");
        assert_eq!(c.color, Some(wordcraft_doc::props::TextColor::Rgb(wordcraft_doc::Rgb(0x1F, 0x4E, 0x79))), "rpr={rpr:?}");
    }
}
