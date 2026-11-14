//! The Compare header/footer flag record in a DOCX custom XML part.
//!
//! Compare turns on Different first page and Different odd & even pages so the First and Even
//! stories it inserts can be shown, and records the values those flags had so Reject All can put
//! them back. That record belongs to the document: a result that is saved, reopened and then
//! rejected must restore them too (#5837). It is not a document property, so it stays out of
//! `docProps/custom.xml`, like the Fit One Page budget (`fit.rs`).

use craft_docx::DocxError;
use wordcraft_doc::CompareHfFlags;

/// The part this crate writes and reads back.
pub(crate) const PART: &str = "customxml/wordcraftcomparehf.xml";
/// Root element of the store.
pub(crate) const ROOT: &str = "wordcraftCompareHf";
/// Namespace of the store.
pub(crate) const NS: &str = "urn:wordcraft:compare-hf";
/// Marker attribute the parser keeps (namespace declarations are stripped from `El`).
const KIND: &str = "wordcraft-compare-hf-v1";

/// Serialize `flags` as the custom XML store.
pub(crate) fn write(flags: &CompareHfFlags) -> Result<Vec<u8>, DocxError> {
    let json = serde_json::to_string(flags).map_err(|e| DocxError::Xml(e.to_string()))?;
    let esc_json = quick_xml::escape::escape(&json);
    Ok(format!(r#"<?xml version="1.0" encoding="UTF-8"?><{ROOT} xmlns="{NS}" kind="{KIND}"><compareHf>{}</compareHf></{ROOT}>"#, esc_json).into_bytes())
}

/// Read the flags from a custom XML part's bytes, if it is ours.
pub(crate) fn read_from_bytes(bytes: &[u8]) -> Option<CompareHfFlags> {
    let mut reader = quick_xml::Reader::from_reader(bytes);
    let mut buf = Vec::new();
    let mut is_our_kind = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(ref e)) => {
                let name = e.local_name();
                if name.as_ref() == ROOT.as_bytes() {
                    for attr in e.attributes().flatten() {
                        if attr.key.local_name().as_ref() == b"kind" && attr.value.as_ref() == KIND.as_bytes() {
                            is_our_kind = true;
                        }
                    }
                } else if name.as_ref() == b"compareHf" && is_our_kind {
                    let text = reader.read_text(e.to_end().name()).ok()?;
                    let s = std::str::from_utf8(text.as_ref()).ok()?;
                    let unescaped = quick_xml::escape::unescape(s).ok()?;
                    return serde_json::from_str(&unescaped).ok();
                }
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags() -> CompareHfFlags {
        CompareHfFlags { title_page: vec![(Some(3), false), (None, false)], even_odd: Some(false) }
    }

    /// The record survives a real DOCX write and read, not just `serde_json`, so rejecting after
    /// a reopen can still restore the flags.
    #[test]
    fn compare_hf_flags_round_trip_through_docx() {
        let mut doc = wordcraft_doc::Document::from_text("Alpha\nBeta");
        doc.compare_hf = Some(flags());
        let bytes = crate::write(&doc).expect("write docx");
        let back = crate::read(&bytes).expect("read docx");
        assert_eq!(back.compare_hf, doc.compare_hf);
    }
}