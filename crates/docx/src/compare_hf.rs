//! The Compare header/footer flag record in a DOCX custom XML part.
//!
//! Compare turns on Different first page and Different odd & even pages so the First and Even
//! stories it inserts can be shown, and records the values those flags had so Reject All can put
//! them back. That record belongs to the document: a result that is saved, reopened and then
//! rejected must restore them too (#5837). It is not a document property, so it stays out of
//! `docProps/custom.xml`, like the Fit One Page budget (`fit.rs`).

use wordcraft_doc::CompareHfFlags;

use crate::DocxError;
use crate::xml::{El, esc};

/// The part this crate writes and reads back.
pub(crate) const PART: &str = "customxml/wordcraftcomparehf.xml";
/// Root element of the store.
pub(crate) const ROOT: &str = "wordcraftCompareHf";
/// Namespace of the store.
pub(crate) const NS: &str = "urn:wordcraft:compare-hf";
/// Marker attribute the parser keeps (namespace declarations are stripped from `El`).
const KIND: &str = "wordcraft-compare-hf-v1";

/// Whether `root` is our store (and not a user's custom XML part with a coincidental name).
pub(crate) fn is_state(root: &El) -> bool {
    root.local() == ROOT && root.attr("kind") == Some(KIND)
}

/// Serialize `flags` as the custom XML store.
pub(crate) fn write(flags: &CompareHfFlags) -> Result<Vec<u8>, DocxError> {
    let json = serde_json::to_string(flags).map_err(|e| DocxError::Xml(e.to_string()))?;
    Ok(format!(r#"<?xml version="1.0" encoding="UTF-8"?><{ROOT} xmlns="{NS}" kind="{KIND}"><compareHf>{}</compareHf></{ROOT}>"#, esc(&json))
        .into_bytes())
}

/// Read the flags from a parsed custom XML root, if it is ours.
pub(crate) fn read(root: &El) -> Option<CompareHfFlags> {
    if !is_state(root) {
        return None;
    }
    let state = root.els().find(|e| e.local() == "compareHf")?;
    serde_json::from_str(&state.text()).ok()
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
