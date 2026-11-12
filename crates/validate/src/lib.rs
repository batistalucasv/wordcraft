//! Structural integrity validation for WordCraft OOXML (`.docx`) packages.
//!
//! [`validate_bytes`] examines a `.docx` package as bytes and returns a [`Report`] of every
//! problem it can prove from the package alone: container and content-type declarations,
//! relationship targets and element-side references, XML well-formedness, illegal characters and
//! namespace prefixes, story-part reference and id spaces (comments, bookmarks, footnotes, styles,
//! numbering, comment threading, custom XML), field/table/markup structure, and media magic bytes.
//! It never panics and never fails: a document that is not a zip at all becomes a `PKG-NOT-ZIP`
//! finding.
//!
//! The checks follow the public OPC and WordprocessingML specifications (ECMA-376 / ISO/IEC
//! 29500); they are written from the specification, and rule ids are stable so callers can rely
//! on them. Every rule's severity and evidence grade is recorded once, in [`RULES`], next to the
//! provenance it was calibrated against.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

// Step 0 of the grammar work: the tables land before any check reads them. This allow is
// removed when the first grammar rule uses the module.
#[allow(dead_code)]
mod model;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod model_tests;
mod package;
mod reserved;
mod scan;

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::io::Read;
use std::path::Path;

use serde::Serialize;

use package::{Package, RelsPart, canonical_entry, entry_by_name, rels_part_name, tail};
use scan::{DecodeError, RefFact};

/// The WordprocessingML main namespace.
pub(crate) const NS_W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
/// The officeDocument relationships namespace.
pub(crate) const NS_R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
/// The DrawingML wordprocessingDrawing namespace.
pub(crate) const NS_WP: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
/// The markup-compatibility namespace.
pub(crate) const NS_MC: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";
/// The Office Math namespace (`m:oMath` and friends).
pub(crate) const NS_M: &str = "http://schemas.openxmlformats.org/officeDocument/2006/math";
/// The Word 2010 wordml namespace (`w14:paraId`).
pub(crate) const NS_W14: &str = "http://schemas.microsoft.com/office/word/2010/wordml";
/// The Word 2012 wordml namespace (`w15:commentEx`).
pub(crate) const NS_W15: &str = "http://schemas.microsoft.com/office/word/2012/wordml";
/// The Word 2016 comment-id namespace (`w16cid:commentId`).
pub(crate) const NS_W16CID: &str = "http://schemas.microsoft.com/office/word/2016/wordml/cid";
/// The OPC custom-XML datastore namespace (`ds:schemaRef`).
pub(crate) const NS_DS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/customXml";

/// Resource budgets shared by every check. Exhausting a budget stops the affected check, records a
/// coverage note and makes the verdict [`Verdict::Incomplete`] rather than letting the run grow
/// without limit. The defaults are deliberately generous; tests may use smaller ones.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Largest input accepted by [`validate_bytes`] (512 MiB).
    pub input_bytes: usize,
    /// Most zip entries examined (20 000).
    pub entries: usize,
    /// Largest single decompressed part examined (256 MiB).
    pub part_bytes: u64,
    /// Largest total of all decompressed parts (512 MiB).
    pub total_bytes: u64,
    /// Deepest XML element nesting examined (512).
    pub depth: usize,
    /// Most XML events examined per part (2 000 000).
    pub events: usize,
    /// Most collected facts per category (200 000).
    pub facts: usize,
    /// Most coverage notes recorded (10 000).
    pub notes: usize,
    /// Most findings recorded per rule (5 000).
    pub findings_per_rule: usize,
}

impl Default for Limits {
    /// Construct default validation limits.
    fn default() -> Self {
        Limits {
            input_bytes: 512 << 20,
            entries: 20_000,
            part_bytes: 256 << 20,
            total_bytes: 512 << 20,
            depth: 512,
            events: 2_000_000,
            facts: 200_000,
            notes: 10_000,
            findings_per_rule: 5_000,
        }
    }
}

/// The overall outcome of a validation run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Verdict {
    /// No Corrupt or Error finding, and every required input was examined.
    Pass,
    /// At least one Corrupt or Error finding.
    Fail,
    /// A required input could not be examined at all.
    Incomplete,
}

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Severity {
    /// The package cannot be a valid document.
    Corrupt,
    /// A rule of the specification is violated.
    Error,
    /// Something Word tolerates, but that is still worth reporting.
    Warning,
}

/// How the rule's truth was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Evidence {
    /// The package format forces it: it cannot be otherwise.
    ByConstruction,
    /// Stated in the OOXML specification.
    MsDocumented,
    /// Seen in documents produced by Word.
    Observed,
    /// Concluded from the package contents.
    Inferred,
}

/// One problem found in a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// Stable rule id, e.g. `"REL-DANGLING"`.
    pub rule: &'static str,
    pub severity: Severity,
    pub evidence: Evidence,
    /// The package part the finding belongs to, if any.
    pub part: Option<String>,
    /// Human-readable, naming the concrete id or value involved.
    pub message: String,
}

/// The result of validating one document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub verdict: Verdict,
    /// Deterministic order: by part, then rule id, then message.
    pub findings: Vec<Finding>,
    /// Things skipped within a completed run.
    pub coverage_notes: Vec<String>,
    /// Every rule id that executed, in the documented order.
    pub checks_run: Vec<&'static str>,
}

/// A rule's fixed severity and evidence. [`Acc::add`] looks a rule's grade up here, so this table
/// is the single place a rule's severity and evidence are recorded, next to their provenance.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RuleSpec {
    pub id: &'static str,
    pub severity: Severity,
    pub evidence: Evidence,
}

/// `PKG-NOT-ZIP`: the input is not a readable zip archive.
pub(crate) const PKG_NOT_ZIP: RuleSpec = RuleSpec { id: "PKG-NOT-ZIP", severity: Severity::Corrupt, evidence: Evidence::ByConstruction };
/// `PKG-DUP-ENTRY`: the same entry name appears twice. Observed: Word web shows the view-only
/// corruption banner.
pub(crate) const PKG_DUP_ENTRY: RuleSpec = RuleSpec { id: "PKG-DUP-ENTRY", severity: Severity::Error, evidence: Evidence::Observed };
/// `PKG-CASE-COLLISION`: two part names differ only by ASCII case. Observed: view-only banner.
pub(crate) const PKG_CASE_COLLISION: RuleSpec = RuleSpec { id: "PKG-CASE-COLLISION", severity: Severity::Error, evidence: Evidence::Observed };
/// `PKG-NO-CONTENT-TYPES`: `[Content_Types].xml` is missing.
pub(crate) const PKG_NO_CONTENT_TYPES: RuleSpec =
    RuleSpec { id: "PKG-NO-CONTENT-TYPES", severity: Severity::Corrupt, evidence: Evidence::ByConstruction };
/// `PKG-NO-MAIN`: no `officeDocument` relationship, or its target is absent. Observed: Word web
/// refuses.
pub(crate) const PKG_NO_MAIN: RuleSpec = RuleSpec { id: "PKG-NO-MAIN", severity: Severity::Error, evidence: Evidence::Observed };
/// `CT-UNDECLARED`: a part has no effective content type. Observed: view-only corruption banner.
pub(crate) const CT_UNDECLARED: RuleSpec = RuleSpec { id: "CT-UNDECLARED", severity: Severity::Error, evidence: Evidence::Observed };
/// `CT-MAIN-TYPE`: the main part's effective content type is not one of the WordprocessingML main
/// types (document, template, or a macro-enabled variant).
/// Observed: Word web refuses.
pub(crate) const CT_MAIN_TYPE: RuleSpec = RuleSpec { id: "CT-MAIN-TYPE", severity: Severity::Error, evidence: Evidence::Observed };
/// `CT-DUP-DEFAULT`: two Default entries for one extension (case-insensitive), OPC §7.2.3.2.1.
pub(crate) const CT_DUP_DEFAULT: RuleSpec = RuleSpec { id: "CT-DUP-DEFAULT", severity: Severity::Error, evidence: Evidence::ByConstruction };
/// `CT-DUP-OVERRIDE`: two Override entries for one part name (case-insensitive), OPC §7.2.3.2.1.
pub(crate) const CT_DUP_OVERRIDE: RuleSpec = RuleSpec { id: "CT-DUP-OVERRIDE", severity: Severity::Error, evidence: Evidence::ByConstruction };
/// `XML-MALFORMED`: an XML part does not parse. Severity is graded per part at the finding site
/// (main part Corrupt, reachable Error, unreferenced Warning); the evidence for the unused
/// Warning case is Observed, the referenced case is Inferred, the main part ByConstruction.
pub(crate) const XML_MALFORMED: RuleSpec = RuleSpec { id: "XML-MALFORMED", severity: Severity::Corrupt, evidence: Evidence::ByConstruction };
/// `XML-UNDECLARED-PREFIX`: an element or attribute uses a prefix not in scope.
pub(crate) const XML_UNDECLARED_PREFIX: RuleSpec =
    RuleSpec { id: "XML-UNDECLARED-PREFIX", severity: Severity::Corrupt, evidence: Evidence::ByConstruction };
/// `XML-INVALID-CHAR`: a part contains a character illegal in XML 1.0, or a reference to one.
pub(crate) const XML_INVALID_CHAR: RuleSpec = RuleSpec { id: "XML-INVALID-CHAR", severity: Severity::Corrupt, evidence: Evidence::ByConstruction };
/// `REL-DUP-ID`: the same `Id` twice within one `.rels` part. Observed: view-only banner.
pub(crate) const REL_DUP_ID: RuleSpec = RuleSpec { id: "REL-DUP-ID", severity: Severity::Error, evidence: Evidence::Observed };
/// `REL-DANGLING`: an internal relationship target is not in the package. Observed: Word web opens
/// in editing mode with the referenced content lost.
pub(crate) const REL_DANGLING: RuleSpec = RuleSpec { id: "REL-DANGLING", severity: Severity::Error, evidence: Evidence::Observed };
/// `REL-TYPED`: a header/footer/image relationship targets the wrong content type. Observed.
pub(crate) const REL_TYPED: RuleSpec = RuleSpec { id: "REL-TYPED", severity: Severity::Error, evidence: Evidence::Observed };
/// `REL-REF-MISSING`: an `r:id`/`r:embed`/`r:link` value has no relationship in the part's own
/// `.rels` (the element-side counterpart of `REL-DANGLING`).
pub(crate) const REL_REF_MISSING: RuleSpec = RuleSpec { id: "REL-REF-MISSING", severity: Severity::Error, evidence: Evidence::Inferred };
/// `CT-MISMATCH`: a part reached through a fixed-type relationship has the wrong content type.
/// Error. The pairing table is the ECMA-376 Part 1 part catalogue (see [`FIXED_REL_CONTENT_TYPES`]);
/// findings are graded at the site. `glossaryDocument` is `Observed` (Word for the web, 2026-11-06).
/// The three extension rows (`commentsExtended`, `commentsIds`, `people`) are also `Observed`, but
/// on **one combined A/B observation, not three independent refusal experiments**: two byte-identical
/// files differing only in those three content-type strings together, the `application/vnd.ms-word.*`
/// spellings against the prescribed `…wordprocessingml.*` spellings; the ms-word set was edit-refused
/// as corrupt, so the refusal is observed for the set, not isolated per part. `commentsExtended`
/// additionally has a positive observation: its resolved state was honoured under the accepted
/// spelling. The rest of the Part 1 catalogue is `MsDocumented`.
pub(crate) const CT_MISMATCH: RuleSpec = RuleSpec { id: "CT-MISMATCH", severity: Severity::Error, evidence: Evidence::MsDocumented };
/// `CT-DUP-MAIN`: more than one part carries the package's main-document content type. Observed:
/// the same 2026-11-06 corpus defect (a main-typed glossary part beside the real main part), which
/// Word for the web refused as corrupt.
pub(crate) const CT_DUP_MAIN: RuleSpec = RuleSpec { id: "CT-DUP-MAIN", severity: Severity::Error, evidence: Evidence::Observed };
/// `ID-COMMENT-DUP`: duplicate `w:comment` ids. Observed: Word web opens in Reading View and
/// refuses editing. ECMA-376 §17.13.4.2 defines collapse semantics the web consumer does not
/// follow.
pub(crate) const ID_COMMENT_DUP: RuleSpec = RuleSpec { id: "ID-COMMENT-DUP", severity: Severity::Error, evidence: Evidence::Observed };
/// `ID-COMMENT-REF`: a `w:commentReference` with no matching `w:comment`. Observed: web shows the
/// "Document corrupted" view-only banner.
pub(crate) const ID_COMMENT_REF: RuleSpec = RuleSpec { id: "ID-COMMENT-REF", severity: Severity::Error, evidence: Evidence::Observed };
/// `ID-DOCPR-DUP`: duplicate `wp:docPr` ids. Conflicting observations: Word web tolerates the
/// shape (both pictures render), while desktop field reports of refusal exist (shape-dependent),
/// so it is a Warning.
pub(crate) const ID_DOCPR_DUP: RuleSpec = RuleSpec { id: "ID-DOCPR-DUP", severity: Severity::Warning, evidence: Evidence::Observed };
/// `ID-BOOKMARK-DUP`: duplicate `w:bookmarkStart` ids. Observed (web): tolerated, Warning.
pub(crate) const ID_BOOKMARK_DUP: RuleSpec = RuleSpec { id: "ID-BOOKMARK-DUP", severity: Severity::Warning, evidence: Evidence::Observed };
/// `BOOKMARK-UNPAIRED`: a `w:bookmarkStart` with no matching `w:bookmarkEnd` (same id, same part)
/// or vice versa, in any story part including comments. Observed: field reports of bookmark markup
/// inside comments.xml silently emptying/hiding the comments.
pub(crate) const BOOKMARK_UNPAIRED: RuleSpec = RuleSpec { id: "BOOKMARK-UNPAIRED", severity: Severity::Warning, evidence: Evidence::Observed };
/// `COMMENT-RANGE-UNPAIRED`: a `w:commentRangeStart` with no matching
/// `w:commentRangeEnd` (same id, same part) or vice versa, in the parts
/// where Word honours ranges — the main and glossary stories, outside
/// text boxes. MS-OI29500 Part 1 §17.13.4.4 notes (b)–(c) and §17.13.4.3
/// notes (a)–(b): a lone start or end is re-anchored at the
/// commentReference, so the comment survives but its highlighted range
/// collapses to a point; and Word ignores both elements under a header,
/// footer, text box, footnote, or endnote.
pub(crate) const COMMENT_RANGE_UNPAIRED: RuleSpec =
    RuleSpec { id: "COMMENT-RANGE-UNPAIRED", severity: Severity::Warning, evidence: Evidence::MsDocumented };
/// `PARAID-DUP`: the same `w14:paraId` value on two elements in one
/// document part. MS-DOCX §2.6.2.3: a paraId "is unique within the
/// document part … with the exception that it need not be unique
/// across the choices or fallback of an Alternate Content block".
/// The collection is branch-aware: a repeat is excused exactly when
/// the two occurrences sit in different branch instances of some
/// block enclosing both (two Choices of one block, a Choice and the
/// Fallback, or different branches of an outer block around nested
/// content) — only one branch of a block is ever selected, so such
/// occurrences cannot coexist. Every other repeat is a duplicate.
/// Values are compared as numbers (leading zeros and case are not
/// distinct identities). MS-DOCX states no open-time consequence;
/// the stake is paragraph identity for anything keyed by paraId.
/// Capped collection goes INCOMPLETE rather than silently passing,
/// including when one value has more mutually excused occurrences
/// than the bounded collection retains.
pub(crate) const PARAID_DUP: RuleSpec = RuleSpec { id: "PARAID-DUP", severity: Severity::Warning, evidence: Evidence::MsDocumented };
/// `PARAID-VALUE`: a `w14:paraId` or `w14:textId` value outside the
/// range MS-DOCX requires (greater than 0 and less than 0x80000000,
/// §2.6.2.3 and §2.6.2.4), a value that is not a hex number at all, or
/// a `w14:textId` on an element with no `w14:paraId` (§2.6.2.4: "Any
/// element having this attribute MUST also have the paraId
/// attribute"). MS-DOCX states no reciprocal requirement, so a
/// paraId without a textId is deliberately NOT flagged.
pub(crate) const PARAID_VALUE: RuleSpec = RuleSpec { id: "PARAID-VALUE", severity: Severity::Warning, evidence: Evidence::MsDocumented };
/// `ID-REVISION-DUP`: duplicate revision ids. Observed (web): tolerated, Warning.
pub(crate) const ID_REVISION_DUP: RuleSpec = RuleSpec { id: "ID-REVISION-DUP", severity: Severity::Warning, evidence: Evidence::Observed };
/// `NOTE-DUP`: duplicate footnote or endnote ids within one note part. ECMA-376 §17.11.10.
pub(crate) const NOTE_DUP: RuleSpec = RuleSpec { id: "NOTE-DUP", severity: Severity::Error, evidence: Evidence::MsDocumented };
/// `REF-NOTE`: a note reference with no note definition. Observed: Word web opens and the footnote
/// renders as a phantom — mark present, pane entry present, body empty.
pub(crate) const REF_NOTE: RuleSpec = RuleSpec { id: "REF-NOTE", severity: Severity::Error, evidence: Evidence::Observed };
/// `REF-HEADER`: a header/footer reference with no matching relationship. Observed: editing refused.
pub(crate) const REF_HEADER: RuleSpec = RuleSpec { id: "REF-HEADER", severity: Severity::Error, evidence: Evidence::Observed };
/// `REF-STYLE`: a style reference with no matching style. Observed (web): tolerated, Warning.
pub(crate) const REF_STYLE: RuleSpec = RuleSpec { id: "REF-STYLE", severity: Severity::Warning, evidence: Evidence::Observed };
/// `STYLE-DUP-ID`: the same styleId on two `w:style` definitions in
/// one styles part. ECMA-376 §17.7.4.17 requires the identifier to be
/// unique and spells out the harm (later definitions are reassigned;
/// references are not repaired). Batch 5 (Word for the web): the
/// second definition silently wins, and ids differing only by case
/// are treated as one — comparison folds case.
pub(crate) const STYLE_DUP_ID: RuleSpec = RuleSpec { id: "STYLE-DUP-ID", severity: Severity::Warning, evidence: Evidence::Observed };
/// `STYLE-DUP-NAME`: two styles in one part whose primary names
/// collide after Word's documented trim (§17.7.4.9(c)), compared
/// case-insensitively. No uniqueness statement exists for names;
/// batch 5 observed the collision in Word for the web at the
/// formatting-loss tier. Which definition name-keyed machinery binds
/// is undocumented, and the message does not claim a direction.
pub(crate) const STYLE_DUP_NAME: RuleSpec = RuleSpec { id: "STYLE-DUP-NAME", severity: Severity::Warning, evidence: Evidence::Observed };
/// `STYLE-RESERVED-NAME`: a style whose primary name is one of the
/// 267 reserved built-in names (MS-OI29500 §17.7.4.9(d) table) while
/// its type differs from the reserved type, or its type is omitted —
/// Word renames the style or changes its type on load. A reserved
/// name WITH its reserved type is the normal form and never fires.
/// The table is the en-US table; built-in names are localised, so
/// wrong-type styles named in other languages are missed by design.
pub(crate) const STYLE_RESERVED_NAME: RuleSpec =
    RuleSpec { id: "STYLE-RESERVED-NAME", severity: Severity::Warning, evidence: Evidence::MsDocumented };
/// `STYLE-RESERVED-ID`: one of the three styleIds MS-OI29500
/// §17.7.4.17(a) documents as special — `NoList`,
/// `DefaultParagraphFont`, `TableNormal`, whose child elements Word
/// ignores — declared with a type different from its reserved type.
/// Payload under a correctly typed reserved id is the normal form
/// (Word's own files carry it) and never fires; an omitted type is
/// not flagged either (the narrowed, documented trigger only).
pub(crate) const STYLE_RESERVED_ID: RuleSpec = RuleSpec { id: "STYLE-RESERVED-ID", severity: Severity::Warning, evidence: Evidence::MsDocumented };
/// `STYLE-COUNT`: more than 4,079 styles in one styles part.
/// MS-OI29500 §17.7.4.17(c): "Word does not open a document
/// containing more than 4,079 styles." Batch 5 found Word for the
/// web opening 4,080 styles cleanly, so the rule is a Warning with
/// that caveat, mirroring the compat-count family.
pub(crate) const STYLE_COUNT: RuleSpec = RuleSpec { id: "STYLE-COUNT", severity: Severity::Warning, evidence: Evidence::MsDocumented };
/// `STYLE-ID-LENGTH`: a styleId over 253 characters. MS-OI29500
/// §17.7.4.17(b) says Word ignores such a value, leaving the style
/// unreferenceable; batch 5 found Word for the web applying a
/// 254-character id, so the grade stays Inferred.
pub(crate) const STYLE_ID_LENGTH: RuleSpec = RuleSpec { id: "STYLE-ID-LENGTH", severity: Severity::Warning, evidence: Evidence::Inferred };
/// `FONT-DUP-NAME`: the same font name twice in one fontTable part,
/// compared case-insensitively (ECMA-376 §17.8.3.10 makes the name
/// the join key for the font record; MS-OI29500 §17.8.3.10(b)
/// documents case-insensitivity). Batch 5: Word for the web showed
/// the corruption banner — Error grade.
pub(crate) const FONT_DUP_NAME: RuleSpec = RuleSpec { id: "FONT-DUP-NAME", severity: Severity::Error, evidence: Evidence::Observed };
/// `PROP-DUP-NAME`: the same custom-property name twice in
/// docProps/custom.xml, compared case-insensitively (ECMA-376 §22.3
/// identifies properties by name; MS-OI29500 §22.3.2.2(g) requires a
/// "case-insensitive unique name"). Batch 5: Word for the web showed
/// the corruption banner — Error grade.
pub(crate) const PROP_DUP_NAME: RuleSpec = RuleSpec { id: "PROP-DUP-NAME", severity: Severity::Error, evidence: Evidence::Observed };
/// `NUM-DANGLING`: a list-numbering reference or abstract-number reference with no definition, or a
/// duplicate definition. Inferred (the compare/merge residue class).
pub(crate) const NUM_DANGLING: RuleSpec = RuleSpec { id: "NUM-DANGLING", severity: Severity::Warning, evidence: Evidence::Inferred };
/// `COMMENTEX-LINK`: a comment-threading link with no counterpart — a commentEx `paraId` with no
/// comments paragraph, a `paraIdParent` with no commentEx `paraId`, a commentsIds `paraId` with
/// no commentEx `paraId`, or a commentEx `paraId` with no commentsIds `paraId`. Inferred (resolved
/// state / reply threading silently drop).
pub(crate) const COMMENTEX_LINK: RuleSpec = RuleSpec { id: "COMMENTEX-LINK", severity: Severity::Warning, evidence: Evidence::Inferred };
/// `FLD-UNBALANCED`: unbalanced field characters within one story part. Observed: Word web refuses
/// editing. §17.16.18's graceful-degradation text is not what the web consumer does.
pub(crate) const FLD_UNBALANCED: RuleSpec = RuleSpec { id: "FLD-UNBALANCED", severity: Severity::Error, evidence: Evidence::Observed };
/// `STRUCT-TC-NO-P`: a table cell with no block-level content. ECMA-376 Part 1 §17.4.65 calls a
/// cell with no block content corrupt. Evidence stays Inferred: Word web tolerated the no-content
/// fixture, desktop behaviour is unknown, and neighbouring field reports repair prompts in this
/// family.
pub(crate) const STRUCT_TC_NO_P: RuleSpec = RuleSpec { id: "STRUCT-TC-NO-P", severity: Severity::Error, evidence: Evidence::Inferred };
/// `STRUCT-END-P`: a nonempty main-part cell whose last block is a nested table rather
/// than a paragraph (ECMA-376 Part 1 §17.4.65). Calibration fixture 32 produced a
/// corruption banner and view-only Word-web document; paragraph-ending control 33
/// opened clean. Wrapper endings (sdt, customXml, altChunk) are not flagged: they can
/// hold a terminal paragraph, so their ending is treated as unknown. Desktop behaviour
/// unrecorded.
pub(crate) const STRUCT_END_P: RuleSpec = RuleSpec { id: "STRUCT-END-P", severity: Severity::Error, evidence: Evidence::Observed };
/// `MATH-PLACEMENT`: math elements where Word refuses them — an `m:oMath` nested in
/// another `m:oMath`, math content outside any `m:oMath`, an `m:oMath` or `m:oMathPara`
/// outside any `w:p`, or an `m:oMathPara` descended from a math element. MS-OI29500
/// Part 1 §22.1.2.77 notes (b), (d), (e), (f) and §22.1.2.78 notes (b), (c), (d) each
/// state Word fails to open the file; the base standard allows every one of these
/// constructs, so this is a documented-divergence rule. Batch b6 (Word for the web,
/// 2026-11-07): the nested-oMath, outside-any-p and oMathPara-in-math cases all drew
/// the corruption banner (Error confirmed), but a lone stray math element outside any
/// oMath opened clean and rendered as plain text — that one case is reported at
/// Warning instead (see scan.rs).
pub(crate) const MATH_PLACEMENT: RuleSpec = RuleSpec { id: "MATH-PLACEMENT", severity: Severity::Error, evidence: Evidence::MsDocumented };
/// `COMMENT-RANGE-COUNT`: more than 32,767 comment ranges in the document.
/// MS-OI29500 Part 1 §17.13.4.4 note (a): the standard allows an unbounded
/// number of comment ranges, but "Word will not open a file with more than
/// 32767 comment ranges". Ranges are counted across every story part.
pub(crate) const COMMENT_RANGE_COUNT: RuleSpec = RuleSpec { id: "COMMENT-RANGE-COUNT", severity: Severity::Error, evidence: Evidence::MsDocumented };
/// `PROTECTION-PASSWORD`: a `w:documentProtection` or `w:writeProtection`
/// element in settings carrying any AG_Password attribute (algorithmName,
/// hashValue, saltValue, spinCount) while missing at least one of
/// algorithmName, hashValue, spinCount. MS-OI29500 Part 1 §17.15.1.93
/// note (c): if any AG_Password attribute is present, Word requires those
/// three, and otherwise will not open the file. Batch b6 (Word for the
/// web, 2026-11-07): a partial set opened clean and stayed editable, so
/// the documented claim is not enforced there and the rule reports at
/// Warning. Desktop behaviour unrecorded.
pub(crate) const PROTECTION_PASSWORD: RuleSpec =
    RuleSpec { id: "PROTECTION-PASSWORD", severity: Severity::Warning, evidence: Evidence::MsDocumented };
/// `HEADERSOURCE-PRESENT`: a `w:headerSource` element under `w:mailMerge`
/// in a settings part. MS-OI29500 Part 1 §17.14.16 note (a): "Word fails
/// to open WordprocessingML documents in which this element is present."
/// Batch b6 (Word for the web, 2026-11-07): corruption banner, view-only
/// — observed.
pub(crate) const HEADERSOURCE_PRESENT: RuleSpec = RuleSpec { id: "HEADERSOURCE-PRESENT", severity: Severity::Error, evidence: Evidence::Observed };
/// `CONTENTPART-TYPE`: a `w:contentPart` element whose relationship target's
/// effective content type is not `text/xml`. MS-OI29500 Part 1 §17.3.3.2
/// note (a): "Word will fail to load the document if the content part's
/// content type is not text/xml." Batch b6 (Word for the web, 2026-11-07):
/// an image/png target drew the corruption banner while the text/xml twin
/// opened clean — observed.
pub(crate) const CONTENTPART_TYPE: RuleSpec = RuleSpec { id: "CONTENTPART-TYPE", severity: Severity::Error, evidence: Evidence::Observed };
/// `BOOKMARK-COUNT`: more than 16,380 bookmarks while settings carries
/// `w:uiCompat97to2003`. MS-OI29500 Part 1 §17.13.6 note (b): Word fails to
/// open such a file (the limit without the flag is 2,147,483,647). Batch
/// b6 (Word for the web, 2026-11-07): 16,381 bookmarks with the flag
/// opened clean, so the documented claim is not enforced there and the
/// rule reports at Warning. Desktop behaviour unrecorded.
pub(crate) const BOOKMARK_COUNT: RuleSpec = RuleSpec { id: "BOOKMARK-COUNT", severity: Severity::Warning, evidence: Evidence::MsDocumented };
/// `PERM-COUNT`: more than 32,752 range permissions while settings carries
/// `w:uiCompat97to2003`. MS-OI29500 Part 1 §17.13.7.1 note (b) and
/// §17.13.7.2 note (d). Batch b6 (Word for the web, 2026-11-07): 32,753
/// range permissions with the flag opened clean — Warning, as for
/// BOOKMARK-COUNT.
pub(crate) const PERM_COUNT: RuleSpec = RuleSpec { id: "PERM-COUNT", severity: Severity::Warning, evidence: Evidence::MsDocumented };
/// `MOVE-RANGE-COUNT`: more than 32,751 moveFrom/moveTo ranges while
/// settings carries `w:uiCompat97to2003`. MS-OI29500 Part 1 §17.13.5.24
/// note (a). Batch b6 (Word for the web, 2026-11-07): 32,752 move ranges
/// (65,504 start markers) with the flag opened clean, so neither counting
/// unit is enforced there. Batch Mac-2 (Word for Mac, 2026-11-08): the
/// same fixture wedged Word — no window, sustained full CPU, killed at
/// the 3-minute observation limit. Verdicts combine by union across
/// consumers, so the rule reports at Error.
pub(crate) const MOVE_RANGE_COUNT: RuleSpec = RuleSpec { id: "MOVE-RANGE-COUNT", severity: Severity::Error, evidence: Evidence::Observed };
/// `NOTE-PR-LISTING`: a special footnote/endnote (separator,
/// continuationSeparator, continuationNotice) whose id no `footnotePr` /
/// `endnotePr` lists. ECMA-376 Part 1 §17.11.9: it "shall not be loaded",
/// and the document is non-conformant when the note "is required by the
/// document content" — a conformance claim, graded ByConstruction.
/// Batch b6 (Word for the web, 2026-11-07): an unlisted separator set
/// opened clean and the footnote rendered with its separator, so the
/// load failure the standard describes is not observed there and every
/// case reports at Warning there. Batch Mac-2 (Word for Mac,
/// 2026-11-08): the same fixture rendered its first screenful and then
/// left Word unresponsive — killed at the 3-minute observation limit,
/// the footnote check uncompletable. Verdicts combine by union across
/// consumers, so the rule reports at Error.
pub(crate) const NOTE_PR_LISTING: RuleSpec = RuleSpec { id: "NOTE-PR-LISTING", severity: Severity::Error, evidence: Evidence::Observed };
/// `NOTE-PR-DANGLING`: a `footnotePr` / `endnotePr` listing names a note
/// id that no footnote/endnote defines. Nothing documents Word loading or
/// rejecting such a listing; it is dead configuration, reported at the
/// lowest grade.
pub(crate) const NOTE_PR_DANGLING: RuleSpec = RuleSpec { id: "NOTE-PR-DANGLING", severity: Severity::Warning, evidence: Evidence::Inferred };
/// `STRUCT-DELTEXT`: a `w:t` inside `w:del`, or a `w:delText` outside one. Observed (web): lenient
/// recovery, the text renders as a tracked deletion anyway, so Warning.
pub(crate) const STRUCT_DELTEXT: RuleSpec = RuleSpec { id: "STRUCT-DELTEXT", severity: Severity::Warning, evidence: Evidence::Observed };
/// `STRUCT-MOVE-UNPAIRED`: unpaired move range starts/ends, mismatched move names, or duplicate
/// move range-start ids. Observed (web): move semantics silently dropped, so Warning.
pub(crate) const STRUCT_MOVE_UNPAIRED: RuleSpec = RuleSpec { id: "STRUCT-MOVE-UNPAIRED", severity: Severity::Warning, evidence: Evidence::Observed };
/// `CUSTOMXML-SCHEMA`: a custom-XML schema reference not attached in settings, or inline
/// `w:customXml` inside a revision/move wrapper. MS-OI29500: Word will not open.
pub(crate) const CUSTOMXML_SCHEMA: RuleSpec = RuleSpec { id: "CUSTOMXML-SCHEMA", severity: Severity::Error, evidence: Evidence::MsDocumented };
/// `MEDIA-MAGIC`: an image whose bytes do not match its extension/type. Observed: Word web opens
/// but the picture does not render — content lost, no format-sniffing rescue.
pub(crate) const MEDIA_MAGIC: RuleSpec = RuleSpec { id: "MEDIA-MAGIC", severity: Severity::Error, evidence: Evidence::Observed };

/// Every rule, in the documented order; [`Acc::add`] grades findings from this table.
pub(crate) const RULES: &[RuleSpec] = &[
    PKG_NOT_ZIP,
    PKG_DUP_ENTRY,
    PKG_CASE_COLLISION,
    PKG_NO_CONTENT_TYPES,
    PKG_NO_MAIN,
    CT_UNDECLARED,
    CT_MAIN_TYPE,
    CT_DUP_MAIN,
    CT_DUP_DEFAULT,
    CT_DUP_OVERRIDE,
    XML_MALFORMED,
    XML_UNDECLARED_PREFIX,
    XML_INVALID_CHAR,
    STRUCT_END_P,
    MATH_PLACEMENT,
    COMMENT_RANGE_COUNT,
    BOOKMARK_COUNT,
    PERM_COUNT,
    MOVE_RANGE_COUNT,
    NOTE_PR_LISTING,
    NOTE_PR_DANGLING,
    PROTECTION_PASSWORD,
    HEADERSOURCE_PRESENT,
    CONTENTPART_TYPE,
    REL_DUP_ID,
    REL_DANGLING,
    REL_TYPED,
    REL_REF_MISSING,
    CT_MISMATCH,
    ID_COMMENT_DUP,
    ID_COMMENT_REF,
    ID_DOCPR_DUP,
    ID_BOOKMARK_DUP,
    BOOKMARK_UNPAIRED,
    COMMENT_RANGE_UNPAIRED,
    PARAID_DUP,
    PARAID_VALUE,
    ID_REVISION_DUP,
    NOTE_DUP,
    REF_NOTE,
    REF_HEADER,
    REF_STYLE,
    STYLE_DUP_ID,
    STYLE_DUP_NAME,
    STYLE_RESERVED_NAME,
    STYLE_RESERVED_ID,
    STYLE_COUNT,
    STYLE_ID_LENGTH,
    FONT_DUP_NAME,
    PROP_DUP_NAME,
    NUM_DANGLING,
    COMMENTEX_LINK,
    FLD_UNBALANCED,
    STRUCT_TC_NO_P,
    STRUCT_DELTEXT,
    STRUCT_MOVE_UNPAIRED,
    CUSTOMXML_SCHEMA,
    MEDIA_MAGIC,
];

/// Look up a rule definition by its stable string identifier.
fn rule_spec(id: &str) -> Option<&'static RuleSpec> {
    RULES.iter().find(|r| r.id == id)
}

#[derive(Default)]
pub(crate) struct Acc {
    findings: Vec<Finding>,
    coverage: Vec<String>,
    checks: Vec<&'static str>,
    incomplete: bool,
    limits: Limits,
    rule_counts: HashMap<&'static str, usize>,
    /// An entry was found whose identity could not be recovered. It could be any part, so no
    /// absence conclusion is established while this is set (see [`ABSENCE_RULES`]).
    unknown_unreadable: bool,
}

/// Rules whose truth depends on something being *absent* from the package (a relationship target, a
/// content-type declaration, a cross-part reference's definition, or the main-part resolution). When
/// the package holds an entry of unknown identity, whatever it might have contained is unknown, so
/// none of these can be concluded: absence is not established. Presence-based rules are unaffected.
const ABSENCE_RULES: &[&str] = &[
    "PKG-NO-CONTENT-TYPES",
    "PKG-NO-MAIN",
    "CT-UNDECLARED",
    "CT-MAIN-TYPE",
    "REL-DANGLING",
    "REL-TYPED",
    "REL-REF-MISSING",
    "ID-COMMENT-REF",
    "REF-NOTE",
    "REF-HEADER",
    "REF-STYLE",
    "STYLE-DUP-ID",
    "STYLE-DUP-NAME",
    "STYLE-RESERVED-NAME",
    "STYLE-RESERVED-ID",
    "STYLE-COUNT",
    "STYLE-ID-LENGTH",
    "FONT-DUP-NAME",
    "PROP-DUP-NAME",
    "NUM-DANGLING",
    "COMMENTEX-LINK",
    "CUSTOMXML-SCHEMA",
];

impl Acc {
    /// Create a new accumulator initialized with the specified resource limits.
    pub(crate) fn with_limits(limits: Limits) -> Acc {
        Acc { limits, ..Acc::default() }
    }

    /// Record that a validation rule was executed.
    pub(crate) fn ran(&mut self, rule: &'static str) {
        self.checks.push(rule);
    }

    /// Record that the archive holds an entry whose identity could not be recovered. Absence
    /// conclusions are suppressed from here on (see [`ABSENCE_RULES`]).
    pub(crate) fn set_unknown_unreadable(&mut self) {
        self.unknown_unreadable = true;
    }

    /// Whether an absence-class finding must be suppressed because an entry of unknown identity
    /// exists: absence is not established. Suppression happens at emission, so no count is spent and
    /// no per-rule budget is touched.
    fn absence_suppressed(&self, rule: &str, severity: Severity) -> bool {
        self.unknown_unreadable && matches!(severity, Severity::Corrupt | Severity::Error) && ABSENCE_RULES.contains(&rule)
    }

    /// Whether another finding for `rule` may be recorded; once the per-rule budget is spent the
    /// run is marked incomplete and further findings of that rule are dropped.
    fn allow(&mut self, rule: &'static str) -> bool {
        let cap = self.limits.findings_per_rule;
        let count = self.rule_counts.entry(rule).or_insert(0);
        if *count >= cap {
            if *count == cap {
                self.incomplete = true;
                if self.coverage.len() < self.limits.notes {
                    self.coverage.push(format!("more than {cap} {rule} findings were found; further ones were not recorded"));
                }
            }
            return false;
        }
        *count += 1;
        true
    }

    /// Record a finding, grading it from [`RULES`].
    pub(crate) fn add(&mut self, rule: &'static str, part: Option<String>, message: String) {
        let (severity, evidence) = rule_spec(rule).map_or((Severity::Error, Evidence::Inferred), |r| (r.severity, r.evidence));
        if self.absence_suppressed(rule, severity) {
            return;
        }
        if !self.allow(rule) {
            return;
        }
        self.findings.push(Finding { rule, severity, evidence, part, message });
    }

    /// Record a finding with an explicit grade, for the few rules whose grade depends on where the
    /// finding is (`XML-MALFORMED`).
    pub(crate) fn add_graded(&mut self, rule: &'static str, severity: Severity, evidence: Evidence, part: Option<String>, message: String) {
        if self.absence_suppressed(rule, severity) {
            return;
        }
        if !self.allow(rule) {
            return;
        }
        self.findings.push(Finding { rule, severity, evidence, part, message });
    }

    /// Record a coverage note. Exhausting the note budget is itself a budget exhaustion: the run is
    /// marked incomplete so it cannot pass silently, and the last slot is replaced with a
    /// deterministic truncation marker so the INCOMPLETE is always explained (H7).
    pub(crate) fn note(&mut self, message: String) {
        const MARKER: &str = "coverage-note budget exhausted; additional explanations omitted";
        if self.coverage.len() >= self.limits.notes {
            self.incomplete = true;
            if self.limits.notes > 0 {
                let last = self.coverage.len() - 1;
                if self.coverage[last] != MARKER {
                    self.coverage[last] = MARKER.to_string();
                }
            }
            return;
        }
        self.coverage.push(message);
    }

    /// Mark the validation outcome as incomplete due to an unreadable part or resource exhaustion.
    pub(crate) fn set_incomplete(&mut self) {
        self.incomplete = true;
    }

    /// Sort findings deterministically and produce the final validation [`Report`].
    fn finish(mut self) -> Report {
        self.findings.sort_by(|a, b| {
            let ka = (a.part.as_deref().unwrap_or(""), a.rule, a.message.as_str());
            let kb = (b.part.as_deref().unwrap_or(""), b.rule, b.message.as_str());
            ka.cmp(&kb)
        });
        let failed = self.findings.iter().any(|f| matches!(f.severity, Severity::Corrupt | Severity::Error));
        let verdict = if failed {
            Verdict::Fail
        } else if self.incomplete {
            Verdict::Incomplete
        } else {
            Verdict::Pass
        };
        Report { verdict, findings: self.findings, coverage_notes: self.coverage, checks_run: self.checks }
    }
}

/// Validate a document held in memory. Never fails: every problem becomes a finding.
pub fn validate_bytes(bytes: &[u8]) -> Report {
    validate_bytes_limited(bytes, Limits::default())
}

/// [`validate_bytes`] with explicit [`Limits`] (used by the budget tests).
pub(crate) fn validate_bytes_limited(bytes: &[u8], limits: Limits) -> Report {
    let mut acc = Acc::with_limits(limits);
    if bytes.len() > limits.input_bytes {
        acc.note(format!("the input is {} bytes, over the {}-byte examination limit; it was not examined", bytes.len(), limits.input_bytes));
        acc.set_incomplete();
        return acc.finish();
    }
    let Some(pkg) = package::load(bytes, &mut acc) else {
        return acc.finish();
    };
    check_content_types(&pkg, &mut acc);
    let xml = check_xml(&pkg, &mut acc);
    check_rels(
        &pkg,
        &xml.refs,
        &xml.content_parts,
        &xml.content_part_exemptions,
        &xml.content_part_incomplete_owners,
        &xml.content_part_scanned_owners,
        &mut acc,
    );
    check_content_part_types(&pkg, &xml.content_parts, xml.content_part_capped_any, &mut acc);
    check_story(&pkg, &xml, &mut acc);
    check_note_pr(&pkg, &xml, &mut acc);
    check_media(&pkg, &mut acc);
    acc.finish()
}

/// Validate a document read from a path. An I/O error reading the file is returned; every
/// problem found inside the file is a finding. A file larger than the input budget is not read:
/// it yields an `INCOMPLETE` report instead.
pub fn validate_path(path: &Path) -> std::io::Result<Report> {
    let limits = Limits::default();
    let meta = std::fs::metadata(path)?;
    if meta.len() > limits.input_bytes as u64 {
        let mut acc = Acc::with_limits(limits);
        acc.note(format!("{} is {} bytes, over the {}-byte examination limit; it was not examined", path.display(), meta.len(), limits.input_bytes));
        acc.set_incomplete();
        return Ok(acc.finish());
    }
    // Read with an explicit byte limit: a file that grows between the metadata check and the read,
    // or that lies about its size, must not be pulled into memory unbounded.
    let mut reader = std::fs::File::open(path)?.take(limits.input_bytes as u64 + 1);
    let mut bytes = Vec::with_capacity(meta.len().min(1 << 20) as usize);
    reader.read_to_end(&mut bytes)?;
    if bytes.len() > limits.input_bytes {
        let mut acc = Acc::with_limits(limits);
        acc.note(format!("{} grew past the {}-byte examination limit while being read; it was not examined", path.display(), limits.input_bytes));
        acc.set_incomplete();
        return Ok(acc.finish());
    }
    Ok(validate_bytes_limited(&bytes, limits))
}

/// Validate `[Content_Types].xml` integrity: undeclared parts, duplicate main parts, and duplicate overrides/defaults.
fn check_content_types(pkg: &Package, acc: &mut Acc) {
    if !pkg.content_types_present {
        return;
    }
    // The manifest is present but its content could not be read: no declaration is known, so no
    // part is established as undeclared (or mistyped). Absence is not established.
    if package::content_unavailable(pkg, "[Content_Types].xml") {
        return;
    }
    acc.ran("CT-UNDECLARED");
    for e in pkg.entries.iter().filter(|e| !e.is_dir) {
        let l = e.name.to_ascii_lowercase();
        if l == "[content_types].xml" || l.ends_with(".rels") {
            continue;
        }
        if !pkg.content_types.incomplete && pkg.content_types.of(&e.name).is_none() {
            acc.add("CT-UNDECLARED", Some(e.name.clone()), format!("part {} has no effective content type", e.name));
        }
    }
    acc.ran("CT-MAIN-TYPE");
    if let Some(main) = &pkg.main
        && (!pkg.content_types.incomplete || pkg.content_types.of(main).is_some())
        && !is_main_typed(pkg, main)
    {
        let got = pkg.content_types.of(main).unwrap_or("(none)");
        acc.add("CT-MAIN-TYPE", Some(main.clone()), format!("main part {main} has content type {got}, not a main-document content type"));
    }
    acc.ran("CT-DUP-MAIN");
    check_dup_main(pkg, acc);
    acc.ran("CT-DUP-DEFAULT");
    for ext in &pkg.content_types.dup_defaults {
        acc.add(
            "CT-DUP-DEFAULT",
            Some("[Content_Types].xml".to_string()),
            format!("[Content_Types].xml declares two Default entries for extension {ext}"),
        );
    }
    acc.ran("CT-DUP-OVERRIDE");
    for part in &pkg.content_types.dup_overrides {
        acc.add(
            "CT-DUP-OVERRIDE",
            Some("[Content_Types].xml".to_string()),
            format!("[Content_Types].xml declares two Override entries for part {part}"),
        );
    }
}

/// Whether `name`'s effective content type is one of the package's main-document content types
/// ([`package::MAIN_CONTENT_TYPES`]: document, template, and their macro-enabled variants).
fn is_main_typed(pkg: &Package, name: &str) -> bool {
    pkg.content_types.of(name).is_some_and(package::is_main_content_type)
}

/// `CT-DUP-MAIN`: at most one part may carry the main-document content type (ECMA-376 Part 1
/// §11.3.10). Every main-typed part other than the one the package's `officeDocument` relationship
/// reaches is reported — including a main-typed part no relationship reaches at all, which the
/// pairing check (`CT-MISMATCH`) cannot see.
fn check_dup_main(pkg: &Package, acc: &mut Acc) {
    let main_typed: Vec<&str> = pkg.entries.iter().filter(|e| !e.is_dir && is_main_typed(pkg, &e.name)).map(|e| e.name.as_str()).collect();
    if main_typed.len() <= 1 {
        return;
    }
    let anchor = pkg.main.as_deref();
    for name in main_typed {
        if anchor.is_some_and(|a| a.eq_ignore_ascii_case(name)) {
            continue;
        }
        acc.add(
            "CT-DUP-MAIN",
            Some(name.to_string()),
            format!("{name} carries the main document content type, but the package's officeDocument relationship reaches a different part; a package may have only one main document"),
        );
    }
}

/// What the XML pass gathered: pooled story facts, and every `r:id`/`r:embed`/`r:link` value seen
/// per part (for `REL-REF-MISSING`).
/// Whether settings carries `w:uiCompat97to2003`, the flag the compat count
/// limits (BOOKMARK-COUNT, PERM-COUNT, MOVE-RANGE-COUNT) key on. `Unknown`
/// when a settings part exists but could not be read: the limits then cannot
/// be applied either way, and the run reports INCOMPLETE rather than guess.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Compat {
    Off,
    On,
    Unknown,
}

struct XmlScan {
    facts: scan::StoryFacts,
    refs: Vec<(String, Vec<RefFact>)>,
    /// `w:contentPart` relationship ids per owning part (for `CONTENTPART-TYPE`).
    content_parts: Vec<(String, Vec<String>)>,
    /// Skipped text/xml contentPart and native InkML ids per owner, used only for the CT-MISMATCH exemption.
    content_part_exemptions: Vec<(String, Vec<String>)>,
    /// A contentPart collection (per part or pooled) hit the fact budget.
    content_part_capped_any: bool,
    /// Owning parts whose contentPart consumption is NOT fully
    /// established: their collection hit the fact budget, or the part
    /// itself did not parse. CT-MISMATCH's customXml scoping must not
    /// conclude non-consumption for these owners.
    content_part_incomplete_owners: HashSet<String>,
    /// Parts (lowercased names) that were actually scanned, so an
    /// owner with a `.rels` part but no scan — oversized, undecodable,
    /// or otherwise skipped — is distinguishable from one whose
    /// complete scan found no contentParts.
    content_part_scanned_owners: HashSet<String>,
    compat: Compat,
    /// Parts whose story facts were only partially examined.
    story_incomplete_parts: HashSet<String>,
    /// (ID, source part) listed by `w:footnotePr` / `w:endnotePr` in settings parts.
    note_listings: (Vec<(String, String)>, Vec<(String, String)>),
    /// Special note definitions: (owning part, id, is_endnote, w:type).
    special_notes: Vec<(String, String, bool, String)>,
    /// Note definition or listing parts with an incomplete scan or pooled collection.
    note_incomplete_parts: HashSet<String>,
}

/// Whether the full XML source was available and completely scanned. Auxiliary parsers can
/// return a prefix at EOF (for example, an unclosed root), which cannot establish absent IDs.
fn xml_source_complete(xml: &XmlScan, entry: &package::Entry) -> bool {
    !entry.content_unavailable && !xml.story_incomplete_parts.contains(&entry.name.to_ascii_lowercase())
}

/// Whether every incoming definition will fit the pooled allowances. A dropped definition
/// makes only its source part incomplete; already collected owner scopes remain checkable.
fn definitions_fit(facts: &scan::StoryFacts, incoming: &scan::StoryFacts, cap: usize) -> bool {
    [
        (facts.comment_defs.len(), incoming.comment_defs.len()),
        (facts.footnote_defs.len(), incoming.footnote_defs.len()),
        (facts.endnote_defs.len(), incoming.endnote_defs.len()),
    ]
    .into_iter()
    .all(|(held, added)| held.saturating_add(added) <= cap)
}

/// Scan all XML parts for well-formedness, undeclared prefixes, illegal characters, and extract story facts.
fn check_xml(pkg: &Package, acc: &mut Acc) -> XmlScan {
    acc.ran("XML-MALFORMED");
    acc.ran("XML-UNDECLARED-PREFIX");
    acc.ran("XML-INVALID-CHAR");
    acc.ran("STRUCT-END-P");
    acc.ran("MATH-PLACEMENT");
    acc.ran("PROTECTION-PASSWORD");
    acc.ran("HEADERSOURCE-PRESENT");
    let roles = story_roles(pkg);
    let mut facts = scan::StoryFacts::default();
    let mut story_incomplete_parts = HashSet::new();
    let mut refs: Vec<(String, Vec<RefFact>)> = Vec::new();
    let mut content_parts: Vec<(String, Vec<String>)> = Vec::new();
    let mut content_part_total = 0usize;
    let mut content_part_exemptions: Vec<(String, Vec<String>)> = Vec::new();
    let mut content_part_exemptions_total = 0usize;
    let mut content_part_capped_any = false;
    let mut content_part_incomplete_owners: HashSet<String> = HashSet::new();
    let mut content_part_scanned_owners: HashSet<String> = HashSet::new();
    let mut compat_on = false;
    let mut compat_unknown = false;
    let mut note_listings = (Vec::new(), Vec::new());
    let mut special_notes: Vec<(String, String, bool, String)> = Vec::new();
    let mut note_incomplete_parts = HashSet::new();
    let mut pooled_capped = false;
    let mut facts_pooled_part: Option<String> = None;
    let mut pooled_refs = 0usize;
    let mut refs_pooled_capped = false;
    let mut refs_pooled_part: Option<String> = None;
    for e in pkg.entries.iter().filter(|e| !e.is_dir) {
        if !package::is_xml_part(pkg, &e.name) {
            continue;
        }
        let l = e.name.to_ascii_lowercase();
        let role = roles.get(&l).copied().unwrap_or_default();
        if role.is_settings && e.decode_error.is_some() {
            compat_unknown = true;
            note_incomplete_parts.insert(l.clone());
        }
        if (role.is_footnotes || role.is_endnotes) && e.decode_error.is_some() {
            note_incomplete_parts.insert(l.clone());
        }
        if let Some(err) = &e.decode_error {
            match err {
                DecodeError::Malformed(msg) => {
                    report_malformed(pkg, &e.name, msg, acc);
                }
                DecodeError::Unsupported(enc) => {
                    acc.note(format!("{} declares the unsupported encoding {enc}; its checks were skipped", e.name));
                    acc.set_incomplete();
                }
                DecodeError::IncompleteDeclaration => {
                    acc.note(format!("{} begins an XML declaration that does not terminate within the scan bound; its encoding could not be decided, so its checks were skipped", e.name));
                    acc.set_incomplete();
                }
            }
            continue;
        }
        let Some(text) = &e.decoded else { continue };
        let mut scanned = scan::scan_part(&e.name, text, role, acc);
        story_incomplete_parts.extend((!scanned.facts_complete).then(|| e.name.to_ascii_lowercase()));
        content_part_scanned_owners.insert(e.name.to_ascii_lowercase());
        note_incomplete_parts.extend(scanned.note_capped.then(|| l.clone()));
        if append_note_items(&mut note_listings, &mut special_notes, &e.name, scanned.note_listings, scanned.special_notes, acc.limits.facts) {
            note_incomplete_parts.insert(l.clone());
        }
        if role.is_settings {
            if scanned.malformed.is_some() {
                compat_unknown = true;
                note_incomplete_parts.insert(l.clone());
            }
            // Only a settings part that scanned clean establishes On: a flag
            // seen before a malformation proves nothing about the document.
            if scanned.compat_97_2003 && scanned.malformed.is_none() {
                compat_on = true;
            }
        }
        if let Some(err) = &scanned.malformed {
            report_malformed(pkg, &e.name, err, acc);
        }
        for prefix in &scanned.unknown_prefixes {
            acc.add("XML-UNDECLARED-PREFIX", Some(e.name.clone()), format!("the namespace prefix `{prefix}` used in {} is not declared", e.name));
        }
        story_incomplete_parts.extend((!definitions_fit(&facts, &scanned.facts, acc.limits.facts)).then(|| l.clone()));
        // Pooled reference drops invalidate "never referenced" conclusions at their source.
        story_incomplete_parts
            .extend((facts.comment_refs.len().saturating_add(scanned.facts.comment_refs.len()) > acc.limits.facts).then(|| l.clone()));
        // A pooled paragraph drop cannot establish absence in this source's comment-threading join.
        if role.is_story && facts.comment_para_ids.len().saturating_add(scanned.facts.comment_para_ids.len()) > acc.limits.facts {
            story_incomplete_parts.insert(l.clone());
            acc.note(format!(
                "{} lost pooled comment paragraph IDs at the {}-entry fact budget; comment-threading paragraph absence was not checked for its owners",
                e.name, acc.limits.facts
            ));
        }
        if role.is_story && scan::merge_facts(&mut facts, scanned.facts, acc.limits.facts) {
            pooled_capped = true;
            if facts_pooled_part.is_none() {
                facts_pooled_part = Some(e.name.clone());
            }
        }
        if scanned.content_part_capped {
            content_part_capped_any = true;
            content_part_incomplete_owners.insert(e.name.clone());
        }
        if !scanned.content_parts.is_empty()
            && append_content_parts(&mut content_parts, &mut content_part_total, &e.name, scanned.content_parts, acc.limits.facts)
        {
            content_part_capped_any = true;
            content_part_incomplete_owners.insert(e.name.clone());
        }
        retain_skipped_text_xml_refs(pkg, &e.name, &mut scanned.content_part_exemptions);
        let ink_targets = retain_native_ink_refs(pkg, &e.name, &mut scanned.ink_content_parts, scanned.ink_content_part_capped);
        let mut exemptions_capped = append_content_parts(
            &mut content_part_exemptions,
            &mut content_part_exemptions_total,
            &e.name,
            scanned.content_part_exemptions,
            acc.limits.facts,
        );
        exemptions_capped |= append_content_parts(
            &mut content_part_exemptions,
            &mut content_part_exemptions_total,
            &e.name,
            scanned.ink_content_parts,
            acc.limits.facts,
        );
        if scanned.content_part_exemptions_capped || (scanned.ink_content_part_capped && ink_targets) || exemptions_capped {
            // Lost references cannot establish non-consumption; native caps matter only where InkML targets exist.
            content_part_incomplete_owners.insert(e.name.clone());
            acc.note(format!(
                "the fact budget stopped collection of contentPart pairing exemptions in {}; its customXml pairing was not checked",
                e.name
            ));
            acc.set_incomplete();
        }
        if !scanned.facts_complete {
            // A scan stopped by events/depth cannot establish non-consumption, even when no
            // contentPart id was reached. Fact caps also conservatively invalidate this source.
            content_part_incomplete_owners.insert(e.name.clone());
        }
        if !scanned.refs.is_empty() && !refs_pooled_capped {
            if pooled_refs.saturating_add(scanned.refs.len()) > acc.limits.facts {
                refs_pooled_capped = true;
                if refs_pooled_part.is_none() {
                    refs_pooled_part = Some(e.name.clone());
                }
            } else {
                pooled_refs += scanned.refs.len();
                refs.push((e.name.clone(), scanned.refs));
            }
        }
        if let Some(desc) = scan::invalid_literal_char(text) {
            acc.add("XML-INVALID-CHAR", Some(e.name.clone()), format!("{} contains {desc}, which is illegal in XML 1.0", e.name));
        }
    }
    if pooled_capped {
        let part = facts_pooled_part.as_deref().unwrap_or("an unknown part");
        acc.note(format!("the pooled story facts exceeded {} entries at {part}; later facts were not recorded", acc.limits.facts));
        acc.set_incomplete();
    }
    if refs_pooled_capped {
        let part = refs_pooled_part.as_deref().unwrap_or("an unknown part");
        acc.note(format!("the package's relationship references exceeded {} entries at {part}; later ones were not recorded", acc.limits.facts));
        acc.set_incomplete();
    }
    let compat = if compat_on {
        Compat::On
    } else if compat_unknown {
        Compat::Unknown
    } else {
        Compat::Off
    };
    // The note collections and the contentPart ids are pooled-budgeted
    // across parts as well as per part; the allowances were enforced
    // while appending in the loop above, so the accumulated collections
    // never exceeded the fact budget at any point.
    XmlScan {
        facts,
        refs,
        content_parts,
        content_part_exemptions,
        content_part_capped_any,
        content_part_incomplete_owners,
        content_part_scanned_owners,
        compat,
        story_incomplete_parts,
        note_listings,
        special_notes,
        note_incomplete_parts,
    }
}

/// Append one part's note listings and special definitions under the
/// pooled fact budget, enforcing the allowance while appending: the
/// accumulated collections never exceed `cap`, however many parts
/// contribute. Returns true when any entry was discarded.
fn append_note_items(
    note_listings: &mut (Vec<(String, String)>, Vec<(String, String)>),
    special_notes: &mut Vec<(String, String, bool, String)>,
    part: &str,
    listings: (Vec<String>, Vec<String>),
    specials: Vec<(String, bool, String)>,
    cap: usize,
) -> bool {
    let mut capped = false;
    let room = cap.saturating_sub(note_listings.0.len() + note_listings.1.len());
    let take_f = room.min(listings.0.len());
    let take_e = (room - take_f).min(listings.1.len());
    if take_f < listings.0.len() || take_e < listings.1.len() {
        capped = true;
    }
    note_listings.0.extend(listings.0.into_iter().take(take_f).map(|id| (id, part.to_string())));
    note_listings.1.extend(listings.1.into_iter().take(take_e).map(|id| (id, part.to_string())));
    for (id, is_end, t) in specials {
        if special_notes.len() >= cap {
            capped = true;
            break;
        }
        special_notes.push((part.to_string(), id, is_end, t));
    }
    capped
}

/// Append one part's contentPart validation or exemption ids under the pooled fact
/// budget, enforcing the allowance while appending (see
/// `append_note_items`). Returns true when any id was discarded.
fn append_content_parts(content_parts: &mut Vec<(String, Vec<String>)>, total: &mut usize, part: &str, mut ids: Vec<String>, cap: usize) -> bool {
    let room = cap.saturating_sub(*total);
    let capped = ids.len() > room;
    if capped {
        ids.truncate(room);
    }
    *total += ids.len();
    if !ids.is_empty() {
        content_parts.push((part.to_string(), ids));
    }
    capped
}

/// Retain only skipped references whose first-wins relationship resolves to text/xml.
/// Word for the web opens that pairing but refuses editing an application/octet-stream target;
/// skipped content must retain CT-MISMATCH for other types without gaining CONTENTPART-TYPE.
fn retain_skipped_text_xml_refs(pkg: &Package, owner: &str, ids: &mut Vec<String>) {
    if ids.is_empty() {
        return;
    }
    let rels_name = rels_part_name(owner);
    let Some(rp) = pkg.rels.iter().find(|rp| rp.name.eq_ignore_ascii_case(&rels_name)) else {
        ids.clear();
        return;
    };
    let mut by_id = HashMap::new();
    for rel in &rp.rels {
        by_id.entry(rel.id.as_str()).or_insert(rel);
    }
    ids.retain(|id| {
        by_id
            .get(id.as_str())
            .and_then(|rel| rel.target_abs.as_deref())
            .and_then(|target| pkg.content_types.of(target))
            .is_some_and(|ct| ct.eq_ignore_ascii_case("text/xml"))
    });
}

/// Retain first-wins native references to InkML (MS-ODRAWXML §2.1.4) and report whether this owner has any InkML target.
/// A capped native collection matters only for such an owner; documents without ink keep their existing checks and verdicts.
fn retain_native_ink_refs(pkg: &Package, owner: &str, ids: &mut Vec<String>, capped: bool) -> bool {
    if ids.is_empty() && !capped {
        return false;
    }
    let rels_name = rels_part_name(owner);
    let Some(rp) = pkg.rels.iter().find(|rp| rp.name.eq_ignore_ascii_case(&rels_name)) else {
        ids.clear();
        return false;
    };
    let mut by_id = HashMap::new();
    for rel in &rp.rels {
        by_id.entry(rel.id.as_str()).or_insert(rel);
    }
    let is_ink = |rel: &package::Rel| {
        rel.target_abs.as_deref().and_then(|target| pkg.content_types.of(target)).is_some_and(|ct| ct.eq_ignore_ascii_case("application/inkml+xml"))
    };
    let ink_targets = by_id.values().any(|rel| is_ink(rel));
    ids.retain(|id| by_id.get(id.as_str()).is_some_and(|rel| is_ink(rel)));
    ink_targets
}

/// `NOTE-PR-LISTING` (ECMA-376 Part 1 §17.11.9) and its reverse,
/// Join special-note definitions and settings listings within each document owner's ID space.
/// Each direction requires its absence-producing source to be complete. Unavailable or capped
/// sources produce explicit INCOMPLETE coverage while independently established findings survive.
/// Unlisted special notes remain Errors under the existing union-of-consumers calibration:
/// Word for the web loads them, but Word for Mac wedged on the same shape (batch Mac-2).
fn check_note_pr(pkg: &Package, xml: &XmlScan, acc: &mut Acc) {
    acc.ran("NOTE-PR-LISTING");
    acc.ran("NOTE-PR-DANGLING");
    let owner_map = owner_index_map(pkg);
    let roles = story_roles(pkg);
    for (index, owner) in owner_scopes(pkg).iter().enumerate() {
        let discovered = !discovery_unavailable(pkg, owner.as_deref());
        let forward_ok = discovered && note_sources_complete(pkg, xml, &owner_map, &roles, index, false);
        let reverse_ok = forward_ok && note_sources_complete(pkg, xml, &owner_map, &roles, index, true);
        if !forward_ok || !reverse_ok {
            acc.note(format!("the footnotePr/endnotePr listings for {} could not be fully joined (ownership, declarations, or source facts were not fully examined)", owner.as_deref().unwrap_or("the package")));
            acc.set_incomplete();
        }
        check_owner_note_pr(xml, &owner_map, index, forward_ok, reverse_ok, acc);
    }
    if xml.special_notes.iter().any(|(part, ..)| part_owners(&owner_map, part).is_empty())
        || xml.note_listings.0.iter().chain(&xml.note_listings.1).any(|(_, part)| part_owners(&owner_map, part).is_empty())
    {
        acc.note("the footnotePr/endnotePr join skipped facts whose document ownership could not be established".to_string());
        acc.set_incomplete();
    }
}

/// Whether every relevant settings or notes source for an owner is available and uncapped.
/// Unknown parts do not veto a fully discovered owner's established sources; callers
/// suppress the owner whose own relationship discovery is incomplete.
fn note_sources_complete(
    pkg: &Package,
    xml: &XmlScan,
    owner_map: &HashMap<String, Vec<usize>>,
    roles: &HashMap<String, scan::PartRole>,
    owner: usize,
    notes: bool,
) -> bool {
    pkg.entries.iter().filter(|e| !e.is_dir).all(|e| {
        let key = e.name.to_ascii_lowercase();
        let relevant = roles.get(&key).is_some_and(|r| if notes { r.is_footnotes || r.is_endnotes } else { r.is_settings });
        if !relevant {
            return true;
        }
        !part_owners(owner_map, &e.name).contains(&owner)
            || (!e.content_unavailable && !xml.story_incomplete_parts.contains(&key) && !xml.note_incomplete_parts.contains(&key))
    })
}

/// Apply both special-note join directions to retained facts from one established owner.
fn check_owner_note_pr(xml: &XmlScan, owner_map: &HashMap<String, Vec<usize>>, owner: usize, forward_ok: bool, reverse_ok: bool, acc: &mut Acc) {
    let owned = |part: &str| part_owners(owner_map, part).contains(&owner);
    let listed_f: HashSet<&str> = xml.note_listings.0.iter().filter(|(_, part)| owned(part)).map(|(id, _)| id.as_str()).collect();
    let listed_e: HashSet<&str> = xml.note_listings.1.iter().filter(|(_, part)| owned(part)).map(|(id, _)| id.as_str()).collect();
    if forward_ok {
        for (part, id, is_end, t) in xml.special_notes.iter().filter(|(part, ..)| owned(part)) {
            let listed = if *is_end { &listed_e } else { &listed_f };
            if !listed.contains(id.as_str()) {
                let (note, pr) = if *is_end { ("endnote", "endnotePr") } else { ("footnote", "footnotePr") };
                acc.add_graded(
                    "NOTE-PR-LISTING", Severity::Error, Evidence::Observed, Some(part.clone()),
                    format!("the {note} with id {id} in {part} has type {t} but is not listed in its owner's {pr}; ECMA-376 says such a note shall not be loaded"),
                );
            }
        }
    }
    if reverse_ok {
        for (note, listings, defs) in
            [("footnote", &xml.note_listings.0, &xml.facts.footnote_defs), ("endnote", &xml.note_listings.1, &xml.facts.endnote_defs)]
        {
            let defined: HashSet<&str> = defs.iter().filter(|(_, part)| owned(part)).map(|(id, _)| id.as_str()).collect();
            for (id, _) in listings.iter().filter(|(_, part)| owned(part)) {
                if !defined.contains(id.as_str()) {
                    acc.add(
                        "NOTE-PR-DANGLING",
                        None,
                        format!("a {note}Pr lists {note} id {id}, but no {note} with that id is defined for its owner"),
                    );
                }
            }
        }
    }
}

/// `CONTENTPART-TYPE` (MS-OI29500 Part 1 §17.3.3.2 note (a)): each
/// `w:contentPart` element's relationship must reach a part whose effective
/// content type is `text/xml`; Word fails to load the document otherwise.
/// The effective type comes from `ContentTypes::of`, where a part Override
/// beats the extension Default (OPC §9.1.2). A relationship id the owner
/// does not define is REL-REF-MISSING's finding, and an undeclared target
/// is CT-UNDECLARED's; this check judges only a resolved, declared type.
fn check_content_part_types(pkg: &Package, content_parts: &[(String, Vec<String>)], capped: bool, acc: &mut Acc) {
    acc.ran("CONTENTPART-TYPE");
    if capped {
        acc.note(
            "the fact budget stopped collection of some w:contentPart references, so CONTENTPART-TYPE did not check every reference, and CT-MISMATCH's customXml pairing was not applied to owners whose collection stopped".to_string(),
        );
        acc.set_incomplete();
    }
    for (owner, ids) in content_parts {
        let rels_name = package::rels_part_name(owner);
        let Some(rp) = pkg.rels.iter().find(|rp| rp.name.eq_ignore_ascii_case(&rels_name)) else { continue };
        // Index the owner's relationships by id once (first wins, as a
        // linear find would return), and dedupe ids in a set: with a
        // Vec both were quadratic in the number of references.
        let by_id: std::collections::HashMap<&str, &package::Rel> = {
            let mut map = std::collections::HashMap::new();
            for r in &rp.rels {
                map.entry(r.id.as_str()).or_insert(r);
            }
            map
        };
        let mut seen: std::collections::HashSet<&String> = std::collections::HashSet::new();
        for id in ids {
            if !seen.insert(id) {
                continue;
            }
            let Some(rel) = by_id.get(id.as_str()) else { continue };
            let Some(target) = &rel.target_abs else { continue };
            let Some(ct) = pkg.content_types.of(target) else { continue };
            if !ct.eq_ignore_ascii_case("text/xml") {
                acc.add(
                    "CONTENTPART-TYPE",
                    Some(owner.clone()),
                    format!("w:contentPart in {owner} references {target} (relationship {id}), whose content type is {ct}, not text/xml"),
                );
            }
        }
    }
}

/// Report an `XML-MALFORMED` finding with severity graded by whether the malformed part is main, reachable, or unused.
fn report_malformed(pkg: &Package, name: &str, err: &str, acc: &mut Acc) {
    let lower = name.to_ascii_lowercase();
    let is_main = pkg.main.as_deref().is_some_and(|m| m.eq_ignore_ascii_case(name));
    let (severity, evidence) = if is_main {
        (Severity::Corrupt, Evidence::ByConstruction)
    } else if pkg.reachable.contains(&lower) {
        (Severity::Error, Evidence::Inferred)
    } else {
        (Severity::Warning, Evidence::Observed)
    };
    acc.add_graded("XML-MALFORMED", severity, evidence, Some(name.to_string()), format!("{name} is not well-formed XML: {err}"));
}

/// Classify the story parts from their effective content type and the relationships that reach
/// them, never from their file names or folder. A comments part stored under an unusual name is
/// still found when a comments relationship or the comments content type names it.
fn story_roles(pkg: &Package) -> HashMap<String, scan::PartRole> {
    let mut roles = HashMap::new();
    if let Some(main) = &pkg.main {
        roles.insert(main.to_ascii_lowercase(), scan::PartRole { is_main: true, is_story: true, ..scan::PartRole::default() });
    }
    for e in pkg.entries.iter().filter(|e| !e.is_dir) {
        if let Some(role) = content_type_role(pkg.content_types.of(&e.name)) {
            roles.entry(e.name.to_ascii_lowercase()).or_insert(role);
        }
    }
    for rp in &pkg.rels {
        for r in &rp.rels {
            let Some(role) = relationship_role(&r.kind) else { continue };
            let Some(target) = &r.target_abs else { continue };
            if let Some(name) = package::canonical_entry(pkg, target) {
                roles.entry(name.to_ascii_lowercase()).or_insert(role);
            }
        }
    }
    roles
}

/// Classify a part role from its declared content type.
fn content_type_role(ct: Option<&str>) -> Option<scan::PartRole> {
    let role = match ct? {
        package::COMMENTS_CONTENT_TYPE => scan::PartRole { is_story: true, is_comments: true, ..scan::PartRole::default() },
        package::FOOTNOTES_CONTENT_TYPE => scan::PartRole { is_story: true, is_footnotes: true, ..scan::PartRole::default() },
        package::ENDNOTES_CONTENT_TYPE => scan::PartRole { is_story: true, is_endnotes: true, ..scan::PartRole::default() },
        package::HEADER_CONTENT_TYPE | package::FOOTER_CONTENT_TYPE => scan::PartRole { is_story: true, ..scan::PartRole::default() },
        package::GLOSSARY_CONTENT_TYPE => scan::PartRole { is_story: true, is_glossary: true, ..scan::PartRole::default() },
        package::SETTINGS_CONTENT_TYPE => scan::PartRole { is_settings: true, ..scan::PartRole::default() },
        _ => return None,
    };
    Some(role)
}

/// Classify a part role from the relationship type that targets it.
fn relationship_role(kind: &str) -> Option<scan::PartRole> {
    // The settings role is matched on the complete relationship type, not the
    // tail: PROTECTION-PASSWORD emits Error findings from it, and a custom
    // relationship type that merely ends in "/settings" must not turn an
    // application-owned part into Word settings.
    if kind == "http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings" {
        return Some(scan::PartRole { is_settings: true, ..scan::PartRole::default() });
    }
    // The glossary role is matched on the complete relationship type for the
    // same reason: COMMENT-RANGE-UNPAIRED adds a part to its scope from it,
    // and a custom relationship type that merely ends in "/glossaryDocument"
    // must not turn an application-owned part into a Word glossary.
    if kind == "http://schemas.openxmlformats.org/officeDocument/2006/relationships/glossaryDocument" {
        return Some(scan::PartRole { is_story: true, is_glossary: true, ..scan::PartRole::default() });
    }
    Some(match tail(kind) {
        "comments" => scan::PartRole { is_story: true, is_comments: true, ..scan::PartRole::default() },
        "footnotes" => scan::PartRole { is_story: true, is_footnotes: true, ..scan::PartRole::default() },
        "endnotes" => scan::PartRole { is_story: true, is_endnotes: true, ..scan::PartRole::default() },
        "header" | "footer" => scan::PartRole { is_story: true, ..scan::PartRole::default() },
        _ => return None,
    })
}

/// Verify relationships: duplicate IDs, dangling targets, typed targets, and content type matches.
fn check_rels(
    pkg: &Package,
    refs: &[(String, Vec<RefFact>)],
    content_parts: &[(String, Vec<String>)],
    content_part_exemptions: &[(String, Vec<String>)],
    content_part_incomplete_owners: &HashSet<String>,
    content_part_scanned_owners: &HashSet<String>,
    acc: &mut Acc,
) {
    acc.ran("REL-DUP-ID");
    for rp in &pkg.rels {
        report_duplicate_rel_ids(rp, acc);
    }
    acc.ran("REL-DANGLING");
    for rp in &pkg.rels {
        for r in &rp.rels {
            let Some(target) = &r.target_abs else { continue };
            // An unexamined archive suffix may still contain this target.
            if !pkg.entries_truncated && package::canonical_entry(pkg, target).is_none() {
                acc.add(
                    "REL-DANGLING",
                    Some(rp.name.clone()),
                    format!(
                        "relationship {} in {} targets {target}, which is missing from the package; the referenced content is lost",
                        r.id, rp.name
                    ),
                );
            }
        }
    }
    acc.ran("REL-TYPED");
    check_typed_targets(pkg, acc);
    check_typed_image_refs(pkg, refs, acc);
    check_part_refs(pkg, refs, acc);
    check_typed_content(pkg, content_parts, content_part_exemptions, content_part_incomplete_owners, content_part_scanned_owners, acc);
}

/// Find duplicate relationship IDs within a single `.rels` part and report `REL-DUP-ID` findings.
fn report_duplicate_rel_ids(rp: &RelsPart, acc: &mut Acc) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for r in &rp.rels {
        *counts.entry(r.id.as_str()).or_default() += 1;
    }
    let mut dupes: Vec<&str> = counts.into_iter().filter(|(_, c)| *c > 1).map(|(id, _)| id).collect();
    dupes.sort_unstable();
    for id in dupes {
        acc.add("REL-DUP-ID", Some(rp.name.clone()), format!("relationship id {id} appears more than once in {}", rp.name));
    }
}

/// Every `r:id`/`r:embed`/`r:link` value in a part must resolve in that part's own `.rels`.
fn check_part_refs(pkg: &Package, refs: &[(String, Vec<RefFact>)], acc: &mut Acc) {
    acc.ran("REL-REF-MISSING");
    for (part, values) in refs {
        // The part's own `.rels` could not be read: which relationships it declares is unknown, so
        // its `r:id`/`r:embed`/`r:link` values are not established as unresolved.
        if package::relationships_incomplete(pkg, part) {
            continue;
        }
        let ids = part_rel_ids(pkg, part);
        let mut reported: HashSet<&str> = HashSet::new();
        for r in values {
            if !ids.contains(&r.value) && reported.insert(r.value.as_str()) {
                acc.add(
                    "REL-REF-MISSING",
                    Some(part.clone()),
                    format!("the r:{} value {} in {part} does not resolve to a relationship in that part's .rels", r.attr, r.value),
                );
            }
        }
    }
}

/// An image element (`a:blip`, `v:imagedata`) must resolve to an image relationship whose target is
/// an image part; anything else is `REL-TYPED` at the referencing part. The check keeps the element
/// identity, so it applies to every owner (including a header's own image references, resolved
/// against the header's `.rels`).
fn check_typed_image_refs(pkg: &Package, refs: &[(String, Vec<RefFact>)], acc: &mut Acc) {
    for (part, values) in refs {
        let Some(rp) = pkg.rels.iter().find(|r| r.name.eq_ignore_ascii_case(&rels_part_name(part))) else { continue };
        let by_id: HashMap<&str, &package::Rel> = rp.rels.iter().map(|r| (r.id.as_str(), r)).collect();
        let mut reported: HashSet<&str> = HashSet::new();
        for r in values.iter().filter(|r| r.image) {
            if !reported.insert(r.value.as_str()) {
                continue;
            }
            let Some(rel) = by_id.get(r.value.as_str()) else { continue };
            if tail(&rel.kind) != "image" {
                acc.add(
                    "REL-TYPED",
                    Some(part.clone()),
                    format!(
                        "an image element in {part} references relationship {}, which is a {} relationship, not an image relationship",
                        r.value,
                        tail(&rel.kind)
                    ),
                );
            } else if let Some(target) = &rel.target_abs {
                // The relationship resolves and targets an internal part. Only the next conclusion
                // depends on the manifest: when its content could not be read the target's declared
                // type is unknown, not absent, so it is skipped. (The non-image-relationship and
                // external-relationship conclusions above and below do not depend on the manifest.)
                if !package::content_unavailable(pkg, "[Content_Types].xml") {
                    let ct = pkg.content_types.of(target);
                    if (!pkg.content_types.incomplete || ct.is_some()) && !ct.is_some_and(|c| c.starts_with("image/")) {
                        acc.add(
                            "REL-TYPED",
                            Some(part.clone()),
                            format!(
                                "an image element in {part} references relationship {}, whose target {target} has content type {}",
                                r.value,
                                ct.unwrap_or("(none)")
                            ),
                        );
                    }
                }
            } else if r.attr != "link" {
                // `r:embed` (and `v:imagedata`'s `r:id`) name an embedded image and must resolve to
                // a part inside the package; only `r:link` may legitimately point at an external
                // resource (H4).
                acc.add(
                    "REL-TYPED",
                    Some(part.clone()),
                    format!(
                        "an image element in {part} references relationship {} with r:{}, which is an external relationship with no internal image target",
                        r.value, r.attr
                    ),
                );
            }
        }
    }
}

/// Collect the set of relationship IDs declared in `part`'s `.rels` file.
fn part_rel_ids(pkg: &Package, part: &str) -> HashSet<String> {
    let want = rels_part_name(part);
    match pkg.rels.iter().find(|r| r.name.eq_ignore_ascii_case(&want)) {
        Some(rp) => rp.rels.iter().map(|r| r.id.clone()).collect(),
        None => HashSet::new(),
    }
}

/// Verify that typed relationships in the main document part target parts with matching content types.
fn check_typed_targets(pkg: &Package, acc: &mut Acc) {
    // Every conclusion here compares a target's declared content type; when the manifest's content
    // could not be read, a target's type is UNKNOWN, not absent, so no type conclusion is
    // established. (The run is already INCOMPLETE from loading the manifest.)
    if package::content_unavailable(pkg, "[Content_Types].xml") {
        return;
    }
    let Some(main) = &pkg.main else { return };
    let want = rels_part_name(main);
    let Some(rp) = pkg.rels.iter().find(|r| r.name.eq_ignore_ascii_case(&want)) else {
        acc.note(format!("the main part {main} has no relationships part; typed targets were not checked"));
        return;
    };
    for r in &rp.rels {
        let Some(target) = &r.target_abs else { continue };
        let ct = pkg.content_types.of(target);
        if ct.is_none() && pkg.content_types.incomplete {
            continue;
        }
        match tail(&r.kind) {
            "header" if ct != Some(package::HEADER_CONTENT_TYPE) => {
                acc.add("REL-TYPED", Some(rp.name.clone()), header_footer_message("header", &r.id, target, ct));
            }
            "footer" if ct != Some(package::FOOTER_CONTENT_TYPE) => {
                acc.add("REL-TYPED", Some(rp.name.clone()), header_footer_message("footer", &r.id, target, ct));
            }
            "image" if !ct.is_some_and(|c| c.starts_with("image/")) => {
                acc.add(
                    "REL-TYPED",
                    Some(rp.name.clone()),
                    format!("image relationship {} targets {target}, whose content type is {}", r.id, ct.unwrap_or("(none)")),
                );
            }
            _ => {}
        }
    }
}

/// Format an error message for mismatched header or footer relationship targets.
fn header_footer_message(kind: &str, id: &str, target: &str, ct: Option<&str>) -> String {
    format!("{kind} relationship {id} targets {target}, whose content type is {}", ct.unwrap_or("(none)"))
}

/// The content type each fixed-type relationship requires of its internal target, keyed by the
/// relationship type's last path segment. This is the ECMA-376 Part 1 part catalogue (§11.3 and
/// §15.2, plus the MS-DOCX extension parts), so CT-MISMATCH is a table lookup rather than a
/// hand-picked list.
///
/// `officeDocument` is absent: the main part's type is pinned by `CT-MAIN-TYPE`. `header` and
/// `footer` are present because the Part 1 catalogue permits the glossary document to own them
/// (§11.3.9, §11.3.6), but the pairing check applies them only to a **non-main** owner's
/// relationships part (see [`check_typed_content`]): the main part's header/footer relationships
/// stay `REL-TYPED`'s alone, so the same part is never reported twice. Kinds the catalogue leaves
/// type-free or extension-dependent — image / media, embedded package, OLE, altChunk, content
/// parts, recipient data — prescribe no type and are deliberately absent; no pairing is invented
/// for them. The `customXml` row is applied only when the relationship is not consumed by a
/// `w:contentPart` element or a native `w14:contentPart` reaching InkML (see [`check_typed_content`]).
pub(crate) const FIXED_REL_CONTENT_TYPES: &[(&str, &str)] = &[
    ("glossaryDocument", package::GLOSSARY_CONTENT_TYPE),
    ("styles", package::STYLES_CONTENT_TYPE),
    ("numbering", package::NUMBERING_CONTENT_TYPE),
    ("settings", package::SETTINGS_CONTENT_TYPE),
    ("fontTable", package::FONT_TABLE_CONTENT_TYPE),
    ("theme", package::THEME_CONTENT_TYPE),
    ("comments", package::COMMENTS_CONTENT_TYPE),
    ("commentsExtended", package::COMMENTS_EXTENDED_CONTENT_TYPE),
    ("commentsIds", package::COMMENTS_IDS_CONTENT_TYPE),
    ("people", package::PEOPLE_CONTENT_TYPE),
    ("footnotes", package::FOOTNOTES_CONTENT_TYPE),
    ("endnotes", package::ENDNOTES_CONTENT_TYPE),
    ("header", package::HEADER_CONTENT_TYPE),
    ("footer", package::FOOTER_CONTENT_TYPE),
    ("webSettings", package::WEB_SETTINGS_CONTENT_TYPE),
    ("chart", package::CHART_CONTENT_TYPE),
    ("customXml", package::XML_CONTENT_TYPE),
    ("customXmlProps", package::CUSTOM_XML_PROPS_CONTENT_TYPE),
    ("bibliography", package::XML_CONTENT_TYPE),
];

/// The content type a fixed-type relationship requires, if the relationship type is one.
fn fixed_rel_content_type(kind: &str) -> Option<&'static str> {
    FIXED_REL_CONTENT_TYPES.iter().find(|(k, _)| *k == kind).map(|(_, ct)| *ct)
}

/// The part that owns a relationships part, derived from the rels
/// part's name (`word/_rels/document.xml.rels` becomes
/// `word/document.xml`, lowercased). `None` for the package
/// relationships part (`_rels/.rels`), which has no containing part.
fn rels_owner_name(rels_name: &str) -> Option<String> {
    let lower = rels_name.to_ascii_lowercase();
    let (dir, file) = match lower.rsplit_once("/_rels/") {
        Some((d, f)) => (d, f),
        None => ("", lower.strip_prefix("_rels/")?),
    };
    let owner_file = file.strip_suffix(".rels")?;
    if owner_file.is_empty() {
        return None;
    }
    Some(if dir.is_empty() { owner_file.to_string() } else { format!("{dir}/{owner_file}") })
}

/// Verify that fixed-type relationships in the package match their prescribed part content types.
///
/// One row is consumption-sensitive (#5982): the `customXml` row prescribes
/// `application/xml`, but a relationship of that type consumed by a
/// `w:contentPart` element is governed by CONTENTPART-TYPE instead —
/// ECMA-376 §17.3.3.2's own contentPart example reaches a `text/xml` part
/// through exactly this relationship type, and Word for the web opens
/// such a file cleanly. The exemption tracks consumption per owner: it
/// applies to the relationship CONTENTPART-TYPE itself resolves for the
/// id (the first with that id in the owner's `.rels`, matching its
/// first-wins index), and for owners whose consumption collection is
/// incomplete (budget-capped or unparseable) the row is not applied at
/// all — non-consumption is not established there, and the run is
/// already INCOMPLETE. Owners with complete collections are checked
/// normally. Skipped contentPart references exempt their first relationship only when
/// its target is text/xml, and remain separate so CONTENTPART-TYPE never validates skipped content. Native
/// w14:contentPart references to InkML join only this exemption collection (MS-ODRAWXML §2.1.4).
fn check_typed_content(
    pkg: &Package,
    content_parts: &[(String, Vec<String>)],
    content_part_exemptions: &[(String, Vec<String>)],
    content_part_incomplete_owners: &HashSet<String>,
    content_part_scanned_owners: &HashSet<String>,
    acc: &mut Acc,
) {
    acc.ran("CT-MISMATCH");
    // Relationship ids consumed by a `w:contentPart`, keyed by the
    // owning part's relationships-part name (compared case-insensitively).
    let mut consumed: HashMap<String, HashSet<&str>> = HashMap::new();
    for (owner, ids) in content_parts.iter().chain(content_part_exemptions) {
        consumed.entry(rels_part_name(owner).to_ascii_lowercase()).or_default().extend(ids.iter().map(String::as_str));
    }
    // Relationships-part names whose owner's consumption collection is
    // incomplete (budget-capped or unparseable): non-consumption is not
    // established for those owners, so the customXml row is not applied
    // to their relationships at all.
    let incomplete_rels: HashSet<String> = content_part_incomplete_owners.iter().map(|owner| rels_part_name(owner).to_ascii_lowercase()).collect();
    // Relationships-part names whose owner is an XML part that was
    // never scanned at all (oversized, undecodable, or skipped before
    // the scan): its consumption is likewise unestablished. An owner
    // with a known non-XML content type cannot contain a `w:contentPart`; an unknown type
    // may have lost its XML classification with a manifest declaration. The package `.rels` has
    // no containing part at all.
    let unscanned_rels: HashSet<String> = pkg
        .rels
        .iter()
        .filter(|rp| {
            rels_owner_name(&rp.name).is_some_and(|owner| {
                // An XML owner that was loaded but never scanned, or an
                // owner missing from the examined entries while the
                // archive's enumeration was truncated (so it is not
                // proven absent): consumption is unestablished either
                // way. A proven-absent owner (complete enumeration)
                // cannot consume anything and is judged normally.
                ((package::is_xml_part(pkg, &owner) || (pkg.content_types.of(&owner).is_none() && entry_by_name(pkg, &owner).is_some()))
                    && !content_part_scanned_owners.contains(&owner))
                    || (pkg.entries_truncated && !pkg.entries.iter().any(|e| e.name.eq_ignore_ascii_case(&owner)))
            })
        })
        .map(|rp| rp.name.to_ascii_lowercase())
        .collect();
    // `header` and `footer` pairings are checked for a non-main owner's relationships part only
    // (the glossary document today, but computed from the owner scopes rather than hardcoded). The
    // main part's header/footer relationships belong to `REL-TYPED`, which already checks them from
    // the main part's content, so they are not reported here. The owner set is derived generically
    // from [`owner_scopes`], so a future owner is covered without another special case.
    let main_rels = pkg.main.as_deref().map(rels_part_name);
    let non_main_owner_rels: HashSet<String> = owner_scopes(pkg)
        .iter()
        .filter_map(|owner| owner.as_deref())
        .filter(|owner| !main_rels.as_deref().is_some_and(|m| m.eq_ignore_ascii_case(&rels_part_name(owner))))
        .map(|owner| rels_part_name(owner).to_ascii_lowercase())
        .collect();
    let mut skipped_consumption = HashSet::new();
    for rp in &pkg.rels {
        let is_non_main_owner = non_main_owner_rels.contains(&rp.name.to_ascii_lowercase());
        let rp_key = rp.name.to_ascii_lowercase();
        // First occurrence of each relationship id in this part, in
        // document order: the occurrence CONTENTPART-TYPE resolves.
        let mut seen_ids: HashSet<&str> = HashSet::new();
        for r in &rp.rels {
            let first_with_id = seen_ids.insert(r.id.as_str());
            let Some(target) = &r.target_abs else { continue };
            let kind = tail(&r.kind);
            let Some(want) = fixed_rel_content_type(kind) else { continue };
            if matches!(kind, "header" | "footer") && !is_non_main_owner {
                continue;
            }
            if kind == "customXml" {
                if incomplete_rels.contains(&rp_key) || unscanned_rels.contains(&rp_key) {
                    // Explain only an actual skipped pairing; unrelated malformed parts have no
                    // consumption check to lose and must retain their existing coverage behavior.
                    if skipped_consumption.insert(rp_key.clone()) {
                        let source = rels_owner_name(&rp.name).unwrap_or_else(|| rp.name.clone());
                        acc.note(format!("{source} was not fully examined as XML; contentPart pairing absence was not checked"));
                        acc.set_incomplete();
                    }
                    continue;
                }
                if first_with_id && consumed.get(&rp_key).is_some_and(|ids| ids.contains(r.id.as_str())) {
                    continue;
                }
            }
            let Some(ct) = pkg.content_types.of(target) else { continue };
            if ct == want {
                continue;
            }
            // The finding belongs to the mistyped part itself. The glossaryDocument,
            // commentsExtended, commentsIds and people pairings carry the 2026-11-06 observations
            // (the three extension rows on one combined A/B observation: see the rule's doc
            // comment); every other row rests on the Part 1 catalogue.
            let evidence = if matches!(kind, "glossaryDocument" | "commentsExtended" | "commentsIds" | "people") {
                Evidence::Observed
            } else {
                Evidence::MsDocumented
            };
            let part = canonical_entry(pkg, target).unwrap_or_else(|| target.clone());
            acc.add_graded(
                "CT-MISMATCH",
                Severity::Error,
                evidence,
                Some(part.clone()),
                format!("relationship {} ({kind}) in {} targets {part}, whose content type {ct} is not the required {want}", r.id, rp.name),
            );
        }
    }
}

/// Check cross-document story identifier families (comments, docPr, bookmarks, revisions, notes, styles, headers).
fn check_story(pkg: &Package, xml: &XmlScan, acc: &mut Acc) {
    let facts = &xml.facts;
    let compat = xml.compat;
    acc.ran("COMMENT-RANGE-COUNT");
    if facts.comment_range_starts > 32_767 {
        acc.add(
            "COMMENT-RANGE-COUNT",
            None,
            format!("the document contains {} comment ranges (w:commentRangeStart), more than the 32767 Word will open", facts.comment_range_starts),
        );
    }
    acc.ran("BOOKMARK-COUNT");
    acc.ran("PERM-COUNT");
    acc.ran("MOVE-RANGE-COUNT");
    match compat {
        Compat::Unknown => {
            acc.note("a settings part could not be read, so the Word 97-2003 compatibility limits on bookmarks, range permissions, and move ranges could not be applied".to_string());
            acc.set_incomplete();
        }
        Compat::On => {
            if facts.bookmark_starts > 16_380 {
                acc.add(
                    "BOOKMARK-COUNT",
                    None,
                    format!(
                        "the document contains {} bookmarks, more than the 16380 documented for Word when uiCompat97to2003 is set",
                        facts.bookmark_starts
                    ),
                );
            }
            if facts.perm_starts > 32_752 {
                acc.add(
                    "PERM-COUNT",
                    None,
                    format!(
                        "the document contains {} range permissions, more than the 32752 documented for Word when uiCompat97to2003 is set",
                        facts.perm_starts
                    ),
                );
            }
            if facts.move_range_starts > 32_751 {
                acc.add(
                    "MOVE-RANGE-COUNT",
                    None,
                    format!(
                        "the document contains {} move ranges, more than the 32751 documented for Word when uiCompat97to2003 is set",
                        facts.move_range_starts
                    ),
                );
            }
        }
        Compat::Off => {}
    }
    acc.ran("ID-COMMENT-DUP");
    report_definition_dups(acc, "ID-COMMENT-DUP", &facts.comment_defs, "w:comment id");
    acc.ran("ID-COMMENT-REF");
    check_owned_ids(pkg, xml, "comments", package::COMMENTS_CONTENT_TYPE, &facts.comment_defs, &facts.comment_refs, acc);
    report_unreferenced_comments(pkg, xml, acc);
    acc.ran("ID-DOCPR-DUP");
    report_dups(acc, "ID-DOCPR-DUP", &facts.docpr, "wp:docPr id");
    acc.ran("ID-BOOKMARK-DUP");
    report_dups(acc, "ID-BOOKMARK-DUP", &facts.bookmarks, "w:bookmarkStart id");
    // BOOKMARK-UNPAIRED findings are produced by the streaming scan; the rule is registered here.
    acc.ran("BOOKMARK-UNPAIRED");
    // COMMENT-RANGE-UNPAIRED likewise.
    acc.ran("COMMENT-RANGE-UNPAIRED");
    // PARAID-DUP and PARAID-VALUE findings are produced by the streaming scan; registered here.
    acc.ran("PARAID-DUP");
    acc.ran("PARAID-VALUE");
    acc.ran("ID-REVISION-DUP");
    report_dups(acc, "ID-REVISION-DUP", &facts.revisions, "revision id");
    acc.ran("NOTE-DUP");
    report_definition_dups(acc, "NOTE-DUP", &facts.footnote_defs, "w:footnote id");
    report_definition_dups(acc, "NOTE-DUP", &facts.endnote_defs, "w:endnote id");
    acc.ran("REF-NOTE");
    check_owned_ids(pkg, xml, "footnotes", package::FOOTNOTES_CONTENT_TYPE, &facts.footnote_defs, &facts.footnote_refs, acc);
    check_owned_ids(pkg, xml, "endnotes", package::ENDNOTES_CONTENT_TYPE, &facts.endnote_defs, &facts.endnote_refs, acc);
    acc.ran("REF-HEADER");
    check_header_footer_refs(pkg, facts, acc);
    acc.ran("REF-STYLE");
    check_style_refs(pkg, xml, acc);
    check_vocabulary(pkg, acc);
    check_numbering(pkg, xml, acc);
    check_commentex(pkg, xml, acc);
    acc.ran("FLD-UNBALANCED");
    acc.ran("STRUCT-TC-NO-P");
    acc.ran("STRUCT-DELTEXT");
    acc.ran("STRUCT-MOVE-UNPAIRED");
    check_custom_xml(pkg, xml, acc);
}

/// Duplicate decimal IDs are forbidden within a definition part, not between independent owners.
fn report_definition_dups(acc: &mut Acc, rule: &'static str, items: &[(String, String)], what: &str) {
    let mut seen = HashSet::new();
    let mut reported = HashSet::new();
    for (id, part) in items {
        if !seen.insert((part, id)) && reported.insert((part, id)) {
            acc.add(rule, Some(part.clone()), format!("duplicate {what} {id} in {part}"));
        }
    }
}

/// Join each owner's references only to the definition part reached by its own relationships.
/// Unavailable or truncated discovery/definition sources cannot establish missing IDs.
fn check_owned_ids(
    pkg: &Package,
    xml: &XmlScan,
    kind: &str,
    ct: &str,
    definitions: &[(String, String)],
    references: &[(String, String)],
    acc: &mut Acc,
) {
    let owners = owner_scopes(pkg);
    let owner_map = owner_index_map(pkg);
    for (index, owner) in owners.iter().enumerate() {
        let source = supporting_part(pkg, owner.as_deref(), kind, ct);
        if source.is_none() && discovery_unavailable(pkg, owner.as_deref()) {
            continue;
        }
        if source.is_some_and(|e| e.content_unavailable || xml.story_incomplete_parts.contains(&e.name.to_ascii_lowercase())) {
            continue;
        }
        let defs: HashSet<String> =
            definitions.iter().filter(|(_, part)| source.is_some_and(|e| e.name.eq_ignore_ascii_case(part))).map(|(id, _)| id.clone()).collect();
        let refs: Vec<_> = references.iter().filter(|(_, part)| part_owners(&owner_map, part).contains(&index)).cloned().collect();
        match kind {
            "comments" => report_missing(acc, "ID-COMMENT-REF", &refs, &defs, "w:commentReference id"),
            "footnotes" => report_missing_note(acc, &refs, &defs, "footnote"),
            "endnotes" => report_missing_note(acc, &refs, &defs, "endnote"),
            _ => {}
        }
    }
}

/// Record unreferenced comments only when the owner's entire reference set was observed.
fn report_unreferenced_comments(pkg: &Package, xml: &XmlScan, acc: &mut Acc) {
    let owner_map = owner_index_map(pkg);
    let roles = story_roles(pkg);
    let mut incomplete_owners = HashSet::new();
    let mut unknown = !ownership_discovery_complete(pkg);
    for e in pkg.entries.iter().filter(|e| !e.is_dir) {
        if roles.get(&e.name.to_ascii_lowercase()).is_some_and(|r| r.is_story) {
            let contexts = part_owners(&owner_map, &e.name);
            unknown |= contexts.is_empty();
            if e.content_unavailable || xml.story_incomplete_parts.contains(&e.name.to_ascii_lowercase()) {
                incomplete_owners.extend(contexts.iter().copied());
            }
        }
    }
    let refs: HashSet<_> =
        xml.facts.comment_refs.iter().flat_map(|(id, part)| part_owners(&owner_map, part).iter().map(move |owner| (owner, id))).collect();
    let mut seen = HashSet::new();
    for (id, part) in &xml.facts.comment_defs {
        let contexts = part_owners(&owner_map, part);
        if !unknown
            && !contexts.is_empty()
            && contexts.iter().all(|owner| !incomplete_owners.contains(owner) && !refs.contains(&(owner, id)))
            && seen.insert((part, id))
        {
            acc.note(format!("w:comment id {id} in {part} is never referenced"));
        }
    }
}

/// Verify header/footer references together: the element reference, the relationship it names and
/// that relationship's kind and target content type. A reference to a non-header relationship (say
/// an image) is a `REF-HEADER` finding, not a pass.
fn check_header_footer_refs(pkg: &Package, facts: &scan::StoryFacts, acc: &mut Acc) {
    let Some(main) = &pkg.main else { return };
    let want = rels_part_name(main);
    // The main part's relationships could not be read: its header/footer relationships are unknown,
    // not absent, so a reference must not be reported as having no relationship.
    if package::relationships_incomplete(pkg, main) {
        return;
    }
    let Some(rp) = pkg.rels.iter().find(|r| r.name.eq_ignore_ascii_case(&want)) else {
        for (id, part) in facts.header_refs.iter().chain(facts.footer_refs.iter()) {
            acc.add("REF-HEADER", Some(part.clone()), format!("the main part {main} has a header/footer reference {id} but no relationships part"));
        }
        return;
    };
    let by_id: HashMap<&str, &package::Rel> = rp.rels.iter().map(|r| (r.id.as_str(), r)).collect();
    let groups: [(&str, &str, &str, &[(String, String)]); 2] = [
        ("header", package::HEADER_CONTENT_TYPE, "headerReference", &facts.header_refs),
        ("footer", package::FOOTER_CONTENT_TYPE, "footerReference", &facts.footer_refs),
    ];
    for (kind, want_ct, element, refs) in groups {
        let mut reported: HashSet<&str> = HashSet::new();
        for (id, part) in refs {
            if !reported.insert(id.as_str()) {
                continue;
            }
            match by_id.get(id.as_str()) {
                None if rp.incomplete => {}
                None => acc.add("REF-HEADER", Some(part.clone()), format!("w:{element} r:id {id} in {part} has no matching relationship")),
                Some(rel) if tail(&rel.kind) != kind => acc.add(
                    "REF-HEADER",
                    Some(part.clone()),
                    format!("w:{element} r:id {id} in {part} resolves to a {} relationship, not a {kind} relationship", tail(&rel.kind)),
                ),
                Some(rel) if rel.external || rel.target_abs.is_none() => acc.add(
                    "REF-HEADER",
                    Some(part.clone()),
                    format!("w:{element} r:id {id} in {part} resolves to an external relationship with no internal target"),
                ),
                Some(rel) => {
                    if let Some(target) = &rel.target_abs {
                        let ct = pkg.content_types.of(target);
                        if (!pkg.content_types.incomplete || ct.is_some()) && ct != Some(want_ct) {
                            acc.add(
                                "REF-HEADER",
                                Some(part.clone()),
                                format!("w:{element} r:id {id} in {part} targets {target}, whose content type is {}", ct.unwrap_or("(none)")),
                            );
                        }
                    }
                }
            }
        }
    }
}

/// The document-level story parts whose relationships own the supporting parts: the main document
/// and, when present, the glossary document.
fn owner_scopes(pkg: &Package) -> Vec<Option<String>> {
    let mut v: Vec<Option<String>> = Vec::new();
    if let Some(m) = &pkg.main {
        v.push(Some(m.clone()));
    }
    if let Some(g) = &pkg.glossary
        && !v.iter().flatten().any(|o| o.eq_ignore_ascii_case(g))
    {
        v.push(Some(g.clone()));
    }
    if v.is_empty() {
        v.push(None);
    }
    v
}

/// The part `owner` reaches through a relationship of `kind`. `None` when the owner has no such
/// relationship.
fn owner_rel_part<'a>(pkg: &'a Package, owner: &str, kind: &str) -> Option<&'a package::Entry> {
    let want = rels_part_name(owner);
    let rp = pkg.rels.iter().find(|r| r.name.eq_ignore_ascii_case(&want))?;
    for r in &rp.rels {
        if tail(&r.kind) != kind {
            continue;
        }
        if let Some(t) = &r.target_abs
            && let Some(name) = canonical_entry(pkg, t)
            && let Some(e) = entry_by_name(pkg, &name)
        {
            return Some(e);
        }
    }
    None
}

/// The part an owner resolves `kind` to. Inside an established owner scope this is the owner's own
/// relationship target and nothing else (J2): a first-by-content-type fallback would let ZIP order
/// change which references are accepted, and a glossary's own parts could be mistaken for the main
/// document's. The package-wide first-by-type fallback is a separate, explicit path used only when
/// no owner scope exists at all (`owner` is `None`), preserving the lone-supporting-part fixtures.
fn supporting_part<'a>(pkg: &'a Package, owner: Option<&str>, kind: &str, ct: &str) -> Option<&'a package::Entry> {
    if let Some(owner) = owner {
        return owner_rel_part(pkg, owner, kind);
    }
    find_by_type(pkg, ct)
}

/// Whether the source that would discover an owner's supporting part could not be read, so the
/// part's existence is unestablished rather than established-absent. In an established owner scope
/// that source is the owner's `.rels`; in the no-owner fallback scope it is the content-type
/// manifest that [`find_by_type`] reads.
fn discovery_unavailable(pkg: &Package, owner: Option<&str>) -> bool {
    // An unexamined archive suffix may hold the target or its discovery declarations.
    if pkg.entries_truncated {
        return true;
    }
    match owner {
        Some(owner) => package::relationships_incomplete(pkg, owner),
        None => package::content_unavailable(pkg, "[Content_Types].xml") || pkg.content_types.incomplete,
    }
}

/// Map each part (lowercased) to every owner context that reaches it, indexed in
/// [`owner_scopes`]. A glossary subtree is attributed to the glossary even though the glossary part
/// is reached from the main document, so its references are never checked against main definitions.
/// A shared story is checked in each established context, never silently assigned to the first.
/// When there is no owner scope at all, every part belongs to the single `None` scope.
fn owner_index_map(pkg: &Package) -> HashMap<String, Vec<usize>> {
    let owners = owner_scopes(pkg);
    let mut map: HashMap<String, Vec<usize>> = HashMap::new();
    if owners.len() == 1 && owners[0].is_none() {
        if !ownership_discovery_complete(pkg) {
            return map;
        }
        for e in pkg.entries.iter().filter(|e| !e.is_dir) {
            map.insert(e.name.to_ascii_lowercase(), vec![0]);
        }
        return map;
    }
    for (i, owner) in owners.iter().enumerate() {
        let Some(owner) = owner else { continue };
        let mut queue = vec![owner.clone()];
        let mut visited: HashSet<String> = HashSet::new();
        while let Some(part) = queue.pop() {
            let key = part.to_ascii_lowercase();
            if !visited.insert(key.clone()) {
                continue;
            }
            map.entry(key).or_default().push(i);
            let want = rels_part_name(&part);
            if let Some(rp) = pkg.rels.iter().find(|r| r.name.eq_ignore_ascii_case(&want)) {
                for r in &rp.rels {
                    if tail(&r.kind) == "glossaryDocument" {
                        continue;
                    }
                    if let Some(t) = &r.target_abs
                        && let Some(name) = canonical_entry(pkg, t)
                    {
                        queue.push(name);
                    }
                }
            }
        }
    }
    // Only complete discovery can assign an unreached supporting part to the primary scope.
    // Otherwise an omitted glossary edge could own it; a missing map entry represents unknown.
    if ownership_discovery_complete(pkg) {
        for e in pkg.entries.iter().filter(|e| !e.is_dir) {
            map.entry(e.name.to_ascii_lowercase()).or_insert_with(|| vec![0]);
        }
    }
    map
}

/// Whether all sources that can discover owner subtrees have been examined.
fn ownership_discovery_complete(pkg: &Package) -> bool {
    !pkg.entries_truncated
        && !pkg.content_types.incomplete
        && !package::content_unavailable(pkg, "[Content_Types].xml")
        && !pkg.rels.iter().any(|rp| rp.incomplete)
}

/// Every established owner context of a part; an empty slice represents uncertain ownership.
fn part_owners<'a>(owner_map: &'a HashMap<String, Vec<usize>>, part: &str) -> &'a [usize] {
    owner_map.get(&part.to_ascii_lowercase()).map(Vec::as_slice).unwrap_or_default()
}

/// Verify that style references in story parts resolve to a defined style in their owner scope.
fn check_style_refs(pkg: &Package, xml: &XmlScan, acc: &mut Acc) {
    let facts = &xml.facts;
    let cap = acc.limits.facts;
    let owners = owner_scopes(pkg);
    let owner_map = owner_index_map(pkg);
    let mut per_owner: Vec<Option<HashSet<String>>> = Vec::with_capacity(owners.len());
    let mut present = false;
    let mut capped = false;
    let mut parsed_any = false;
    for owner in &owners {
        let Some(entry) = supporting_part(pkg, owner.as_deref(), "styles", package::STYLES_CONTENT_TYPE) else {
            per_owner.push(None);
            continue;
        };
        present = true;
        if !xml_source_complete(xml, entry) {
            acc.note(format!("{} was not fully examined; style-reference absence was not checked", entry.name));
            acc.set_incomplete();
            per_owner.push(None);
            continue;
        }
        let mut guard = scan::ParseGuard::new(acc.limits.events, acc.limits.depth);
        let parsed = entry.decoded.as_deref().map(|text| scan::parse_style_ids(text, cap, &mut guard));
        if guard.exhausted() {
            acc.note(format!("{} exceeded the XML {} budget; style definitions were not checked", entry.name, guard.budget()));
            acc.set_incomplete();
            per_owner.push(None);
            continue;
        }
        match parsed {
            Some(Some((ids, c))) => {
                parsed_any = true;
                capped |= c;
                if c {
                    acc.note(format!("{} exceeded the {cap}-entry budget; style-reference absence was not checked", entry.name));
                }
                per_owner.push((!c).then_some(ids));
            }
            Some(None) => per_owner.push(None),
            None => per_owner.push(None),
        }
    }
    if capped {
        acc.note(format!("a styles part exceeded the {cap}-entry budget; later style ids were not recorded"));
        acc.set_incomplete();
    }
    if !present {
        if !facts.style_refs.is_empty() {
            acc.note("no styles part is reachable; w:pStyle/w:rStyle references were not checked".to_string());
        }
        return;
    }
    if !parsed_any {
        if !facts.style_refs.is_empty() {
            acc.note("a styles part is present but does not parse; w:pStyle/w:rStyle references were not checked".to_string());
        }
        return;
    }
    // Check each reference against its owner's own definitions only (H2).
    let mut reported: HashSet<(&str, &str)> = HashSet::new();
    for (id, part) in &facts.style_refs {
        for &owner in part_owners(&owner_map, part) {
            let Some(Some(defs)) = per_owner.get(owner) else { continue };
            if !defs.contains(id) && reported.insert((id.as_str(), part.as_str())) {
                acc.add("REF-STYLE", Some(part.clone()), format!("style id {id} in {part} has no definition in the package"));
            }
        }
    }
}

/// The vocabulary family: id and name collisions inside the styles,
/// fontTable, and custom-properties parts. Each part is judged in its
/// own scope; a part that is present but cannot be read or parsed
/// makes the run INCOMPLETE for these rules rather than passing them
/// silently (absence of the part itself is legitimate and silent).
fn check_vocabulary(pkg: &Package, acc: &mut Acc) {
    let cap = acc.limits.facts;
    let owners = owner_scopes(pkg);
    for owner in &owners {
        if let Some(entry) = supporting_part(pkg, owner.as_deref(), "styles", package::STYLES_CONTENT_TYPE) {
            check_styles_vocabulary(entry, cap, acc);
        }
        if let Some(entry) = supporting_part(pkg, owner.as_deref(), "fontTable", package::FONT_TABLE_CONTENT_TYPE) {
            check_name_dups(entry, cap, acc, true);
        }
    }
    if let Some(entry) = find_by_type(pkg, package::CUSTOM_PROPERTIES_CONTENT_TYPE) {
        check_name_dups(entry, cap, acc, false);
    }
    acc.ran("STYLE-DUP-ID");
    acc.ran("STYLE-DUP-NAME");
    acc.ran("STYLE-RESERVED-NAME");
    acc.ran("STYLE-RESERVED-ID");
    acc.ran("STYLE-COUNT");
    acc.ran("STYLE-ID-LENGTH");
    acc.ran("FONT-DUP-NAME");
    acc.ran("PROP-DUP-NAME");
}

/// Evaluate the six styles-part vocabulary rules for one styles part.
fn check_styles_vocabulary(entry: &package::Entry, cap: usize, acc: &mut Acc) {
    let part = entry.name.clone();
    let mut guard = scan::ParseGuard::new(acc.limits.events, acc.limits.depth);
    let parsed = entry.decoded.as_deref().map(|text| scan::parse_style_defs(text, cap, &mut guard));
    if guard.exhausted() {
        acc.note(format!("{part} exceeded the XML {} budget; style vocabulary was not checked", guard.budget()));
        acc.set_incomplete();
        return;
    }
    let Some(Some((defs, capped, total))) = parsed else {
        acc.note(format!("{part} is present but does not parse; style vocabulary was not checked"));
        acc.set_incomplete();
        return;
    };
    if capped {
        acc.note(format!("{part} exceeded the {cap}-entry budget; later style definitions were not recorded"));
        acc.set_incomplete();
    }
    if total > 4079 {
        acc.add(
            "STYLE-COUNT",
            Some(part.clone()),
            format!("{part} defines {total} styles; MS-OI29500 states Word does not open a document containing more than 4,079 styles (Word for the web was observed opening 4,080 cleanly)"),
        );
    }
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut reported_keys: HashSet<(&'static str, String)> = HashSet::new();
    for def in &defs {
        if let Some(id) = &def.id {
            if id.chars().count() > 253 {
                acc.add(
                    "STYLE-ID-LENGTH",
                    Some(part.clone()),
                    format!("styleId {id} in {part} is longer than 253 characters; MS-OI29500 states Word ignores such a value, leaving the style unreferenceable"),
                );
            }
            let folded = id.to_lowercase();
            if let Some(first) = ids.get(&folded) {
                if reported_keys.insert(("STYLE-DUP-ID", folded.clone())) {
                    acc.add(
                        "STYLE-DUP-ID",
                        Some(part.clone()),
                        format!("styleId {first} is defined more than once in {part} (again as {id}); duplicate definitions are reassigned arbitrarily and their references are not repaired"),
                    );
                }
            } else {
                ids.insert(folded.clone(), id.clone());
            }
            let reserved_type = match folded.as_str() {
                "nolist" => Some("numbering"),
                "defaultparagraphfont" => Some("character"),
                "tablenormal" => Some("table"),
                _ => None,
            };
            if let Some(required) = reserved_type
                && let Some(declared) = &def.style_type
                && declared.to_lowercase() != required
                && reported_keys.insert(("STYLE-RESERVED-ID", folded.clone()))
            {
                acc.add(
                    "STYLE-RESERVED-ID",
                    Some(part.clone()),
                    format!("style {id} in {part} has type {declared}; MS-OI29500 documents this id as a {required} style whose child elements Word ignores"),
                );
            }
        }
        if let Some(name) = &def.name {
            let folded = name.trim().to_lowercase();
            if let Some(first) = names.get(&folded) {
                if reported_keys.insert(("STYLE-DUP-NAME", folded.clone())) {
                    acc.add(
                        "STYLE-DUP-NAME",
                        Some(part.clone()),
                        format!("primary style name {first} is used by more than one style in {part} (again as {})", name.trim()),
                    );
                }
            } else {
                names.insert(folded.clone(), name.trim().to_string());
            }
            if let Some(required) = reserved::reserved_style_type(&folded)
                && !def.style_type.as_deref().is_some_and(|t| t.to_lowercase() == required)
                && reported_keys.insert(("STYLE-RESERVED-NAME", folded.clone()))
            {
                let declared = def.style_type.as_deref().unwrap_or("(omitted)");
                acc.add(
                    "STYLE-RESERVED-NAME",
                    Some(part.clone()),
                    format!("style name {} in {part} is a reserved built-in name requiring type {required}, but the style has type {declared}; Word renames the style or changes its type on load", name.trim()),
                );
            }
        }
    }
}

/// FONT-DUP-NAME / PROP-DUP-NAME: one folded-name collision report
/// per duplicated name in a fontTable or custom-properties part.
fn check_name_dups(entry: &package::Entry, cap: usize, acc: &mut Acc, fonts: bool) {
    let part = entry.name.clone();
    let rule = if fonts { "FONT-DUP-NAME" } else { "PROP-DUP-NAME" };
    let mut guard = scan::ParseGuard::new(acc.limits.events, acc.limits.depth);
    let parsed = entry
        .decoded
        .as_deref()
        .map(|text| if fonts { scan::parse_font_names(text, cap, &mut guard) } else { scan::parse_custom_prop_names(text, cap, &mut guard) });
    if guard.exhausted() {
        acc.note(format!("{part} exceeded the XML {} budget; {rule} was not checked", guard.budget()));
        acc.set_incomplete();
        return;
    }
    let Some(Some((names, capped))) = parsed else {
        acc.note(format!("{part} is present but does not parse; {rule} was not checked"));
        acc.set_incomplete();
        return;
    };
    if capped {
        acc.note(format!("{part} exceeded the {cap}-entry budget; later names were not recorded"));
        acc.set_incomplete();
    }
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut reported: HashSet<String> = HashSet::new();
    for name in &names {
        let folded = name.to_lowercase();
        if let Some(first) = seen.get(&folded) {
            if reported.insert(folded) {
                let message = if fonts {
                    format!(
                        "font name {first} appears more than once in {part} (again as {name}); font names are case-insensitive join keys, and Word shows a corruption notice for this collision"
                    )
                } else {
                    format!(
                        "custom property name {first} appears more than once in {part} (again as {name}); property names must be case-insensitively unique, and Word shows a corruption notice for this collision"
                    )
                };
                acc.add(rule, Some(part.clone()), message);
            }
        } else {
            seen.insert(folded, name.clone());
        }
    }
}

/// The part whose effective content type is `ct`, if any. Supporting parts are found by type, not
/// by their conventional file name or folder.
fn find_by_type<'a>(pkg: &'a Package, ct: &str) -> Option<&'a package::Entry> {
    pkg.entries.iter().find(|e| !e.is_dir && pkg.content_types.of(&e.name) == Some(ct))
}

/// Verify numbering definitions and references in story and styles parts for each owner scope.
fn check_numbering(pkg: &Package, xml: &XmlScan, acc: &mut Acc) {
    let facts = &xml.facts;
    acc.ran("NUM-DANGLING");
    let cap = acc.limits.facts;
    let owners = owner_scopes(pkg);
    let owner_map = owner_index_map(pkg);
    let mut numbering_cache: HashMap<String, Option<HashSet<String>>> = HashMap::new();
    // Each owner's numbering set, in owner order. `Some(set)` = the owner's numbering part was
    // examined, so `set` is the definitions it declares (empty when the owner has no numbering part
    // — established absent, so its references are genuinely dangling). `None` = the owner's
    // numbering could not be established: the part exists but could not be examined, or the
    // discovery source (its `.rels`, or the manifest in the no-owner scope) could not be read, so
    // that owner's references are not established as dangling and its dependent checks are skipped
    // (H8).
    let mut num_sets: Vec<Option<HashSet<String>>> = Vec::with_capacity(owners.len());
    let mut style_checks: Vec<(String, HashSet<String>)> = Vec::new();
    let mut seen_styles: HashSet<String> = HashSet::new();
    for owner in &owners {
        let num_part = supporting_part(pkg, owner.as_deref(), "numbering", package::NUMBERING_CONTENT_TYPE).map(|e| e.name.clone());
        let num_set: Option<HashSet<String>> = match &num_part {
            Some(np) => {
                if let Some(cached) = numbering_cache.get(np) {
                    cached.clone()
                } else {
                    let parsed = process_numbering_part(pkg, xml, np, cap, acc);
                    numbering_cache.insert(np.clone(), parsed.clone());
                    parsed
                }
            }
            None => {
                // A readable discovery source that reaches no numbering part establishes that the
                // owner has none (an empty set, exactly as before). When the discovery source
                // itself could not be read, the owner's numbering is unknown, not absent: its
                // references must not be reported against an empty set (H8).
                if discovery_unavailable(pkg, owner.as_deref()) { None } else { Some(HashSet::new()) }
            }
        };
        // Each owner's styles are checked against that owner's own numbering set, and only when it
        // could actually be examined.
        if let Some(entry) = supporting_part(pkg, owner.as_deref(), "styles", package::STYLES_CONTENT_TYPE)
            && seen_styles.insert(entry.name.to_ascii_lowercase())
            && let Some(set) = &num_set
        {
            style_checks.push((entry.name.clone(), set.clone()));
        }
        num_sets.push(num_set);
    }
    report_num_refs(acc, facts, &num_sets, &owner_map);
    for (name, num_set) in &style_checks {
        report_style_num_refs(pkg, name, num_set, cap, acc);
    }
}

/// Parse a numbering part, reporting duplicate definitions and dangling `w:abstractNumId`
/// references. `None` when the part could not be read, did not parse, or exceeded a budget (its
/// references must not be checked).
fn process_numbering_part(pkg: &Package, xml: &XmlScan, name: &str, cap: usize, acc: &mut Acc) -> Option<HashSet<String>> {
    let entry = entry_by_name(pkg, name)?;
    if !xml_source_complete(xml, entry) {
        acc.note(format!("{name} was not fully examined; list numbering absence was not checked"));
        acc.set_incomplete();
        return None;
    }
    let Some(text) = entry.decoded.as_deref() else {
        acc.note(format!("{name} could not be read; list numbering references were not checked"));
        return None;
    };
    let mut guard = scan::ParseGuard::new(acc.limits.events, acc.limits.depth);
    let defs = scan::parse_numbering(text, cap, &mut guard);
    if guard.exhausted() {
        acc.note(format!("{name} exceeded the XML {} budget; list numbering references were not checked", guard.budget()));
        acc.set_incomplete();
        return None;
    }
    let Some(defs) = defs else {
        acc.note(format!("{name} does not parse; list numbering references were not checked"));
        return None;
    };
    if defs.capped {
        acc.note(format!("{name} exceeded the {cap}-entry budget; numbering definition absence was not checked"));
        acc.set_incomplete();
    }
    report_dups_values(acc, &defs.num_ids, &entry.name, "w:num numId");
    report_dups_values(acc, &defs.abstract_ids, &entry.name, "w:abstractNum abstractNumId");
    let abstract_set: HashSet<&str> = defs.abstract_ids.iter().map(String::as_str).collect();
    for (num, abs) in &defs.num_to_abstract {
        match abs {
            Some(a) if !defs.capped && !abstract_set.contains(a.as_str()) => acc.add(
                "NUM-DANGLING",
                Some(entry.name.clone()),
                format!("w:num numId {num} references w:abstractNumId {a}, which has no w:abstractNum"),
            ),
            None => acc.add("NUM-DANGLING", Some(entry.name.clone()), format!("w:num numId {num} has no w:abstractNumId")),
            _ => {}
        }
    }
    // A retained reference is still checkable, but a truncated definition set cannot prove absence.
    (!defs.capped).then(|| defs.num_ids.into_iter().collect())
}

/// `NUM-DANGLING` for `w:numPr/w:numId` references in a styles part, checked against its owner's
/// numbering part. `numId` `0` is the "remove numbering" sentinel and is never a reference.
fn report_style_num_refs(pkg: &Package, styles_name: &str, num_set: &HashSet<String>, cap: usize, acc: &mut Acc) {
    let Some(entry) = entry_by_name(pkg, styles_name) else { return };
    let Some(text) = entry.decoded.as_deref() else {
        acc.note(format!("{styles_name} could not be read; style numbering references were not checked"));
        return;
    };
    let mut guard = scan::ParseGuard::new(acc.limits.events, acc.limits.depth);
    let parsed = scan::parse_style_num_ids(text, cap, &mut guard);
    if guard.exhausted() {
        acc.note(format!("{styles_name} exceeded the XML {} budget; style numbering references were not checked", guard.budget()));
        acc.set_incomplete();
        return;
    }
    let Some((vals, capped)) = parsed else {
        acc.note(format!("{styles_name} does not parse; style numbering references were not checked"));
        return;
    };
    if capped {
        acc.note(format!("{styles_name} exceeded the {cap}-entry budget; later numbering references were not recorded"));
        acc.set_incomplete();
    }
    let mut reported: HashSet<&str> = HashSet::new();
    for val in &vals {
        if val == "0" {
            continue;
        }
        if !num_set.contains(val.as_str()) && reported.insert(val.as_str()) {
            acc.add(
                "NUM-DANGLING",
                Some(entry.name.clone()),
                format!("a w:numPr in {} references numId {val}, which has no w:num definition", entry.name),
            );
        }
    }
}

/// `NUM-DANGLING` for `w:numPr/w:numId` references in story parts, each checked against its owner's
/// own numbering set. A reference whose owner's numbering could not be examined is skipped, so an
/// unavailable numbering part never manufactures a dangling finding (H2/H8).
fn report_num_refs(acc: &mut Acc, facts: &scan::StoryFacts, num_sets: &[Option<HashSet<String>>], owner_map: &HashMap<String, Vec<usize>>) {
    let mut reported: HashSet<(&str, &str)> = HashSet::new();
    for (val, part) in &facts.num_ids {
        if val == "0" {
            continue;
        }
        for &owner in part_owners(owner_map, part) {
            let Some(Some(set)) = num_sets.get(owner) else { continue };
            if !set.contains(val) && reported.insert((val.as_str(), part.as_str())) {
                acc.add("NUM-DANGLING", Some(part.clone()), format!("w:numPr references numId {val}, which has no w:num definition"));
            }
        }
    }
}

/// Report duplicate string values within a part for a specified attribute/construct name.
fn report_dups_values(acc: &mut Acc, values: &[String], part: &str, what: &str) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for v in values {
        *counts.entry(v.as_str()).or_default() += 1;
    }
    let mut dups: Vec<&str> = counts.into_iter().filter(|(_, c)| *c > 1).map(|(v, _)| v).collect();
    dups.sort_unstable();
    for v in dups {
        acc.add("NUM-DANGLING", Some(part.to_string()), format!("duplicate {what} {v} in {part}"));
    }
}

/// Verify comment-threading relationships between comments, commentsExtended, and commentsIds parts.
fn check_commentex(pkg: &Package, xml: &XmlScan, acc: &mut Acc) {
    let facts = &xml.facts;
    let cap = acc.limits.facts;
    let owners = owner_scopes(pkg);
    let owner_map = owner_index_map(pkg);
    let roles = story_roles(pkg);
    let any_ext = owners.iter().any(|o| supporting_part(pkg, o.as_deref(), "commentsExtended", package::COMMENTS_EXTENDED_CONTENT_TYPE).is_some());
    if !any_ext {
        return;
    }
    acc.ran("COMMENTEX-LINK");
    for (i, owner) in owners.iter().enumerate() {
        // This join pools every comments source in the context, including shared sources after
        // the first relationship target. Every contributing source must establish completeness.
        let mut comments_complete = !discovery_unavailable(pkg, owner.as_deref());
        for entry in pkg.entries.iter().filter(|e| {
            !e.is_dir && roles.get(&e.name.to_ascii_lowercase()).is_some_and(|r| r.is_comments) && part_owners(&owner_map, &e.name).contains(&i)
        }) {
            if !xml_source_complete(xml, entry) {
                comments_complete = false;
                acc.note(format!(
                    "comment-threading paragraphs for {} could not be fully checked: contributing source {} was not fully examined",
                    owner.as_deref().unwrap_or("the package"),
                    entry.name
                ));
            }
        }
        if !comments_complete {
            acc.note(format!(
                "comment-threading paragraphs for {} could not be fully checked (comments ownership or source facts were not fully examined)",
                owner.as_deref().unwrap_or("the package")
            ));
            acc.set_incomplete();
        }
        // Each owner's comment-threading is checked within its own scope: its commentEx entries
        // against all its contributing comments sources, and its commentsIds against its own commentEx.
        let comment_para: HashSet<&str> =
            facts.comment_para_ids.iter().filter(|(_, p)| part_owners(&owner_map, p).contains(&i)).map(|(p, _)| p.as_str()).collect();
        let ext_source = supporting_part(pkg, owner.as_deref(), "commentsExtended", package::COMMENTS_EXTENDED_CONTENT_TYPE);
        let ext = ext_source.and_then(|entry| read_comments_ext(entry, xml, cap, acc).map(|e| (entry.name.clone(), e)));
        let ext_complete = match &ext {
            Some((_, e)) => !e.capped,
            None => ext_source.is_none() && !discovery_unavailable(pkg, owner.as_deref()),
        };
        let ex_para: HashSet<&str> = match &ext {
            Some((_, e)) => e.entries.iter().map(|(p, _)| p.as_str()).collect(),
            None => HashSet::new(),
        };
        if let Some((name, e)) = &ext {
            for (para, parent) in &e.entries {
                if comments_complete && !comment_para.contains(para.as_str()) {
                    acc.add(
                        "COMMENTEX-LINK",
                        Some(name.clone()),
                        format!("w15:commentEx paraId {para} has no paragraph with that w14:paraId in comments.xml"),
                    );
                }
                if ext_complete
                    && let Some(p) = parent
                    && !ex_para.contains(p.as_str())
                {
                    acc.add(
                        "COMMENTEX-LINK",
                        Some(name.clone()),
                        format!("w15:commentEx paraId {para} has paraIdParent {p}, which matches no commentEx paraId"),
                    );
                }
            }
        }
        // commentsIds ids keep their own part name, so an unmatched id is reported at the part it
        // came from, never at another owner's (H9).
        let cid = supporting_part(pkg, owner.as_deref(), "commentsIds", package::COMMENTS_IDS_CONTENT_TYPE)
            .and_then(|entry| read_comments_ids(entry, xml, cap, acc));
        let Some((cid_name, cid_ids, cid_complete)) = &cid else { continue };
        let cid_set: HashSet<&str> = cid_ids.iter().map(String::as_str).collect();
        for id in cid_ids {
            if ext_complete && !ex_para.contains(id.as_str()) {
                acc.add("COMMENTEX-LINK", Some(cid_name.clone()), format!("commentsIds paraId {id} has no commentEx counterpart"));
            }
        }
        if let Some((name, e)) = &ext {
            for (para, _) in &e.entries {
                if *cid_complete && !cid_set.contains(para.as_str()) {
                    acc.add("COMMENTEX-LINK", Some(name.clone()), format!("commentEx paraId {para} has no commentsIds paraId counterpart"));
                }
            }
        }
    }
}

/// Parse a commentsExtended part, recording a note (and INCOMPLETE for a budget exhaustion) when it
/// cannot be checked. `None` when the part's link checks must be skipped.
fn read_comments_ext(entry: &package::Entry, xml: &XmlScan, cap: usize, acc: &mut Acc) -> Option<scan::CommentEx> {
    let name = &entry.name;
    if !xml_source_complete(xml, entry) {
        acc.note(format!("{name} was not fully examined; commentEx definition absence was not checked"));
        acc.set_incomplete();
        return None;
    }
    let Some(text) = entry.decoded.as_deref() else {
        acc.note(format!("{name} could not be read; comment-threading links were not checked"));
        return None;
    };
    let mut guard = scan::ParseGuard::new(acc.limits.events, acc.limits.depth);
    let ext = scan::parse_comments_extended(text, cap, &mut guard);
    if guard.exhausted() {
        acc.note(format!("{name} exceeded the XML {} budget; comment-threading links were not checked", guard.budget()));
        acc.set_incomplete();
        return None;
    }
    let Some(ext) = ext else {
        acc.note(format!("{name} does not parse; comment-threading links were not checked"));
        return None;
    };
    if ext.capped {
        acc.note(format!(
            "{name} exceeded the {cap}-entry budget; later commentEx entries were not recorded and commentEx definition absence was not checked"
        ));
        acc.set_incomplete();
    }
    Some(ext)
}

/// Parse a commentsIds part, returning its part name and `w16cid:paraId` values. `None` when the part's link
/// checks must be skipped. The returned boolean says whether the definition set is complete.
fn read_comments_ids(entry: &package::Entry, xml: &XmlScan, cap: usize, acc: &mut Acc) -> Option<(String, Vec<String>, bool)> {
    let name = &entry.name;
    if !xml_source_complete(xml, entry) {
        acc.note(format!("{name} was not fully examined; commentsIds definition absence was not checked"));
        acc.set_incomplete();
        return None;
    }
    let text = entry.decoded.as_deref()?;
    let mut guard = scan::ParseGuard::new(acc.limits.events, acc.limits.depth);
    let parsed = scan::parse_comments_ids(text, cap, &mut guard);
    if guard.exhausted() {
        acc.note(format!("{name} exceeded the XML {} budget; comment-threading links were not checked", guard.budget()));
        acc.set_incomplete();
        return None;
    }
    let (ids, capped) = parsed?;
    if capped {
        acc.note(format!(
            "{name} exceeded the {cap}-entry budget; later paraIds were not recorded and commentsIds definition absence was not checked"
        ));
        acc.set_incomplete();
    }
    Some((entry.name.clone(), ids, !capped))
}

/// Verify that custom XML itemProperties parts only reference schemas declared in settings.xml.
fn check_custom_xml(pkg: &Package, xml: &XmlScan, acc: &mut Acc) {
    acc.ran("CUSTOMXML-SCHEMA");
    let cap = acc.limits.facts;
    let owners = owner_scopes(pkg);
    let owner_map = owner_index_map(pkg);
    // Schema availability and definitions are tracked per owner (J3): a glossary-only attached
    // schema cannot satisfy a main-owned reference, and an unreadable settings part disables the
    // checks for its owner alone. `None` = that owner's settings could not be established, so its
    // customXml parts are skipped.
    let mut per_owner: Vec<Option<HashSet<String>>> = Vec::with_capacity(owners.len());
    // The owners whose customXml item-properties parts exist, so whose schema references would have
    // been checked. A note about a skipped check is only worth recording for such an owner; an owner
    // with no customXml parts loses nothing when the check is skipped (Opus N5). When the manifest
    // that types the parts could not be read the attribution itself is unavailable, so the note is
    // kept.
    let custom_xml_owners: HashSet<usize> = pkg
        .entries
        .iter()
        .filter(|e| !e.is_dir && pkg.content_types.of(&e.name) == Some(package::CUSTOM_XML_PROPS_CONTENT_TYPE))
        .flat_map(|e| part_owners(&owner_map, &e.name).iter().copied())
        .collect();
    if pkg.entries.iter().any(|e| {
        !e.is_dir && pkg.content_types.of(&e.name) == Some(package::CUSTOM_XML_PROPS_CONTENT_TYPE) && part_owners(&owner_map, &e.name).is_empty()
    }) {
        acc.note("custom XML ownership could not be established; custom XML schema references were not checked".to_string());
        acc.set_incomplete();
    }
    for (i, owner) in owners.iter().enumerate() {
        // The settings part is discovered through the owner's own `.rels`, or in the no-owner
        // fallback scope through the manifest (`find_by_type`). When that source could not be read,
        // which settings part (and so which schemas) the owner reaches is UNKNOWN, not absent: skip
        // the owner rather than substituting an empty schema set, which would report a valid schema
        // reference as unattached. Applying the same predicate in both scopes keeps established
        // absence (a readable source that reaches no settings) a genuine empty set. (The run is
        // already INCOMPLETE from loading the unreadable source.)
        if discovery_unavailable(pkg, owner.as_deref()) {
            if custom_xml_owners.contains(&i) || package::content_unavailable(pkg, "[Content_Types].xml") {
                let source = match owner.as_deref() {
                    Some(o) => rels_part_name(o),
                    None => "[Content_Types].xml".to_string(),
                };
                acc.note(format!("{source} could not be read; custom XML schema references were not checked"));
            }
            acc.set_incomplete();
            per_owner.push(None);
            continue;
        }
        let Some(entry) = supporting_part(pkg, owner.as_deref(), "settings", package::SETTINGS_CONTENT_TYPE) else {
            // The owner's `.rels` was readable and holds no settings relationship: there is
            // genuinely no settings part for this owner, so the attached-schema set is empty.
            per_owner.push(Some(HashSet::new()));
            continue;
        };
        if !xml_source_complete(xml, entry) {
            acc.note(format!("{} was not fully examined; custom XML schema references were not checked", entry.name));
            acc.set_incomplete();
            per_owner.push(None);
            continue;
        }
        let Some(text) = entry.decoded.as_deref() else {
            acc.note(format!("{} could not be read; custom XML schema references were not checked", entry.name));
            acc.set_incomplete();
            per_owner.push(None);
            continue;
        };
        let mut guard = scan::ParseGuard::new(acc.limits.events, acc.limits.depth);
        match scan::parse_attached_schemas(text, cap, &mut guard) {
            Some((vals, capped)) if !guard.exhausted() => {
                if capped {
                    acc.note(format!("{} exceeded the {cap}-entry budget; attached-schema absence was not checked", entry.name));
                    acc.set_incomplete();
                }
                per_owner.push((!capped).then(|| vals.into_iter().collect()));
            }
            result => {
                if guard.exhausted() {
                    acc.note(format!("{} exceeded the XML {} budget; custom XML schema references were not checked", entry.name, guard.budget()));
                } else {
                    debug_assert!(result.is_none());
                    acc.note(format!("{} does not parse; custom XML schema references were not checked", entry.name));
                }
                acc.set_incomplete();
                per_owner.push(None);
            }
        }
    }
    for e in pkg.entries.iter().filter(|e| !e.is_dir) {
        if pkg.content_types.of(&e.name) != Some(package::CUSTOM_XML_PROPS_CONTENT_TYPE) {
            continue;
        }
        let schemas: Vec<_> = part_owners(&owner_map, &e.name).iter().filter_map(|&owner| per_owner.get(owner).and_then(Option::as_ref)).collect();
        if schemas.is_empty() {
            continue;
        }
        let Some(text) = e.decoded.as_deref() else { continue };
        let mut guard = scan::ParseGuard::new(acc.limits.events, acc.limits.depth);
        let (uris, capped) = scan::parse_custom_xml_schemas(text, cap, &mut guard);
        if guard.exhausted() {
            acc.note(format!("{} exceeded the XML {} budget; custom XML schema references were not checked", e.name, guard.budget()));
            acc.set_incomplete();
            continue;
        }
        if capped {
            acc.note(format!("{} exceeded the {cap}-entry budget; later schema references were not recorded", e.name));
            acc.set_incomplete();
        }
        for uri in uris {
            if schemas.iter().any(|set| !set.contains(&uri)) {
                acc.add(
                    "CUSTOMXML-SCHEMA",
                    Some(e.name.clone()),
                    format!("custom XML part {} declares schema {uri}, which is not attached in settings.xml", e.name),
                );
            }
        }
    }
}

/// Report duplicate identifier findings for a given rule across items.
fn report_dups(acc: &mut Acc, rule: &'static str, items: &[(String, String)], what: &str) {
    let mut first: HashMap<&str, &str> = HashMap::new();
    let mut reported: HashSet<&str> = HashSet::new();
    for (id, part) in items {
        match first.entry(id.as_str()) {
            Entry::Vacant(v) => {
                v.insert(part.as_str());
            }
            Entry::Occupied(o) => {
                if reported.insert(id.as_str()) {
                    acc.add(rule, Some(part.clone()), format!("duplicate {what} {id} (first in {})", o.get()));
                }
            }
        }
    }
}

/// Report missing definitions for references collected from story elements.
fn report_missing(acc: &mut Acc, rule: &'static str, refs: &[(String, String)], defs: &HashSet<String>, what: &str) {
    let mut reported: HashSet<&str> = HashSet::new();
    for (id, part) in refs {
        if !defs.contains(id) && reported.insert(id.as_str()) {
            acc.add(rule, Some(part.clone()), format!("{what} {id} in {part} has no definition in the package"));
        }
    }
}

/// Report missing footnote or endnote definitions.
fn report_missing_note(acc: &mut Acc, refs: &[(String, String)], defs: &HashSet<String>, kind: &str) {
    let mut reported: HashSet<&str> = HashSet::new();
    for (id, part) in refs {
        if !defs.contains(id) && reported.insert(id.as_str()) {
            acc.add(
                "REF-NOTE",
                Some(part.clone()),
                format!("w:{kind}Reference id {id} in {part} has no w:{kind} definition; the note body is absent"),
            );
        }
    }
}

/// Verify that media files in `word/media/` or targeted by image relationships match magic byte signatures.
fn check_media(pkg: &Package, acc: &mut Acc) {
    acc.ran("MEDIA-MAGIC");
    let image_targets = image_rel_targets(pkg);
    for e in pkg.entries.iter().filter(|e| !e.is_dir) {
        let l = e.name.to_ascii_lowercase();
        if !l.starts_with("word/media/") && !image_targets.contains(&l) {
            continue;
        }
        let Some(bytes) = &e.bytes else { continue };
        let Some(kind) = media_kind(&e.name, pkg.content_types.of(&e.name)) else { continue };
        if !magic_matches(kind, bytes) {
            acc.add("MEDIA-MAGIC", Some(e.name.clone()), format!("{} does not have the magic bytes of a {kind} file", e.name));
        }
    }
}

/// Collect lowercased target paths for all image relationships in the package.
fn image_rel_targets(pkg: &Package) -> HashSet<String> {
    let mut set = HashSet::new();
    for rp in &pkg.rels {
        for r in &rp.rels {
            if tail(&r.kind) == "image"
                && let Some(t) = &r.target_abs
            {
                set.insert(t.to_ascii_lowercase());
            }
        }
    }
    set
}

/// Determine the expected media format kind from file extension or content type.
fn media_kind(name: &str, ct: Option<&str>) -> Option<&'static str> {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => Some("png"),
        Some("jpg" | "jpeg") => Some("jpeg"),
        Some("gif") => Some("gif"),
        Some("bmp") => Some("bmp"),
        Some("tif" | "tiff") => Some("tiff"),
        _ => ct.and_then(|c| match c {
            "image/png" => Some("png"),
            "image/jpeg" => Some("jpeg"),
            "image/gif" => Some("gif"),
            "image/bmp" => Some("bmp"),
            "image/tiff" => Some("tiff"),
            _ => None,
        }),
    }
}

/// Check if the beginning bytes match the expected magic signature for `kind`.
fn magic_matches(kind: &str, bytes: &[u8]) -> bool {
    match kind {
        "png" => bytes.starts_with(b"\x89PNG"),
        "jpeg" => bytes.starts_with(b"\xFF\xD8\xFF"),
        "gif" => bytes.starts_with(b"GIF8"),
        "bmp" => bytes.starts_with(b"BM"),
        "tiff" => bytes.starts_with(b"\x49\x49\x2A\x00") || bytes.starts_with(b"\x4D\x4D\x00\x2A"),
        _ => true,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod budget_tests {
    //! F1/F2 canaries: every budget has a test that proves exhausting it yields INCOMPLETE with a
    //! coverage note rather than an unbounded run or a silent pass.

    use super::*;
    use crate::{package, scan};

    /// Build an uncompressed in-memory zip archive with the specified entry names and payloads.
    fn tiny_zip(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        use std::io::{Cursor, Write};
        use zip::write::SimpleFileOptions;
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// Build minimal `[Content_Types].xml` bytes declaring default rels/xml and the main document override.
    fn content_types() -> Vec<u8> {
        format!(
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{}"/></Types>"#,
            package::MAIN_CONTENT_TYPE
        )
        .into_bytes()
    }

    /// Build root `_rels/.rels` bytes with an `officeDocument` relationship.
    fn package_rels() -> Vec<u8> {
        br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.to_vec()
    }

    /// Build an empty `word/_rels/document.xml.rels` relationships part.
    fn doc_rels() -> Vec<u8> {
        br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>"#.to_vec()
    }

    /// Build a relationships part XML with the given body content.
    fn rels(body: &str) -> Vec<u8> {
        format!(r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{body}</Relationships>"#)
            .into_bytes()
    }

    /// Build a minimal `word/document.xml` part containing an empty paragraph.
    fn document() -> Vec<u8> {
        format!(r#"<?xml version="1.0"?><w:document xmlns:w="{}"><w:body><w:p/></w:body></w:document>"#, NS_W).into_bytes()
    }

    /// Build `[Content_Types].xml` bytes including an override for `part`.
    fn content_types_with_override(part: &str, ct: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/{part}" ContentType="{ct}"/></Types>"#,
            package::MAIN_CONTENT_TYPE
        )
        .into_bytes()
    }

    /// Build a minimal valid package zip as raw bytes.
    fn minimal_zip() -> Vec<u8> {
        tiny_zip(&[
            ("[Content_Types].xml", content_types()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", document()),
            ("word/_rels/document.xml.rels", doc_rels()),
        ])
    }

    #[test]
    /// A main part over the per-part byte budget is unreadable content: the verdict is INCOMPLETE, never a manufactured verdict on a part that was not read.
    fn oversized_main_part_is_incomplete() {
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", vec![b'x'; 5000]),
            ("word/_rels/document.xml.rels", doc_rels()),
        ]);
        let mut acc = Acc::with_limits(Limits { part_bytes: 4096, ..Limits::default() });
        let _ = package::load(&zip, &mut acc);
        let report = acc.finish();
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(report.coverage_notes.iter().any(|n| n.contains("document.xml")), "{:?}", report.coverage_notes);
    }

    #[test]
    /// A central directory longer than the entry cap truncates the raw duplicate scan, so the run is INCOMPLETE rather than a duplicate-clean PASS.
    fn entry_cap_truncation_is_incomplete() {
        let zip = minimal_zip();
        let mut acc = Acc::with_limits(Limits { entries: 3, ..Limits::default() });
        let _ = package::load(&zip, &mut acc);
        let report = acc.finish();
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
    }

    #[test]
    /// Input over the total byte budget is INCOMPLETE.
    fn input_over_the_byte_budget_is_incomplete() {
        let report = validate_bytes_limited(b"definitely not a zip", Limits { input_bytes: 4, ..Limits::default() });
        assert_eq!(report.verdict, Verdict::Incomplete);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    /// A part nested past the depth limit stops the scan and marks the run INCOMPLETE instead of recursing without bound.
    fn deeply_nested_part_is_incomplete_without_hanging() {
        let mut xml = format!(r#"<w:document xmlns:w="{}">"#, NS_W);
        xml.push_str(&"<w:p>".repeat(2000));
        xml.push_str(&"</w:p>".repeat(2000));
        xml.push_str("</w:document>");
        let mut acc = Acc::with_limits(Limits { depth: 512, ..Limits::default() });
        let role = scan::PartRole { is_main: true, is_story: true, ..scan::PartRole::default() };
        let _ = scan::scan_part("word/document.xml", &xml, role, &mut acc);
        let report = acc.finish();
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(!report.findings.iter().any(|f| f.rule == "XML-MALFORMED"), "{:?}", report.findings);
    }

    #[test]
    /// A part with more elements than the event budget stops the scan and marks the run INCOMPLETE.
    fn extreme_element_count_stops_within_budget() {
        let xml = format!(r#"<w:document xmlns:w="{}">{}</w:document>"#, NS_W, "<w:p/>".repeat(10_000));
        let mut acc = Acc::with_limits(Limits { events: 1000, ..Limits::default() });
        let role = scan::PartRole { is_main: true, is_story: true, ..scan::PartRole::default() };
        let _ = scan::scan_part("word/document.xml", &xml, role, &mut acc);
        let report = acc.finish();
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
    }

    /// Finding 2 canary: the pairing reports must not run on a partial scan. A field left open when
    /// the event budget stops the scan is not an unbalanced field; the same document under normal
    /// limits still reports `FLD-UNBALANCED`.
    #[test]
    fn budget_stopped_scan_does_not_report_a_truncated_field() {
        let body = format!(r#"<w:p><w:r><w:fldChar w:fldCharType="begin"/></w:r>{}</w:p>"#, "<w:r><w:t>x</w:t></w:r>".repeat(80));
        let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{}"><w:body>{body}</w:body></w:document>"#, NS_W).into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", doc_rels()),
        ]);
        let stopped = validate_bytes_limited(&zip, Limits { events: 25, ..Limits::default() });
        assert_eq!(stopped.verdict, Verdict::Incomplete, "findings: {:?}", stopped.findings);
        assert!(
            !stopped.findings.iter().any(|f| f.rule == "FLD-UNBALANCED"),
            "a budget-stopped scan must not report the truncated field: {:?}",
            stopped.findings
        );
        // Regression control: fully scanned, the unclosed field is reported.
        let full = validate_bytes(&zip);
        assert!(full.findings.iter().any(|f| f.rule == "FLD-UNBALANCED"), "findings: {:?}", full.findings);
    }

    /// NOTE-PR-DANGLING must not run on a truncated definition set: with
    /// a fact budget of 1, the footnotes part's first definition (id 2)
    /// fills the budget and the separator's definition (id 0) is dropped
    /// from the pooled facts — the settings listing for id 0 is then NOT
    /// dangling, and the run must not claim it is.
    #[test]
    fn note_pr_dangling_suppressed_when_definitions_capped() {
        let ct = format!(
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/word/footnotes.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footnotes+xml"/><Override PartName="/word/settings.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml"/></Types>"#,
            package::MAIN_CONTENT_TYPE
        );
        let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{NS_W}"><w:body><w:p/></w:body></w:document>"#).into_bytes();
        let footnotes = format!(
            r#"<?xml version="1.0"?><w:footnotes xmlns:w="{NS_W}"><w:footnote w:id="2"><w:p/></w:footnote><w:footnote w:type="separator" w:id="0"><w:p/></w:footnote></w:footnotes>"#
        )
        .into_bytes();
        let settings =
            format!(r#"<?xml version="1.0"?><w:settings xmlns:w="{NS_W}"><w:footnotePr><w:footnote w:id="0"/></w:footnotePr></w:settings>"#)
                .into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", ct.into_bytes()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/footnotes.xml", footnotes),
            ("word/settings.xml", settings),
        ]);
        let report = validate_bytes_limited(&zip, Limits { facts: 1, ..Limits::default() });
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(
            !report.findings.iter().any(|f| f.rule == "NOTE-PR-DANGLING"),
            "a capped definition set must not produce dangling claims: {:?}",
            report.findings
        );
        // Control: at full limits the same package is clean for both rules.
        let full = validate_bytes(&zip);
        assert!(!full.findings.iter().any(|f| f.rule == "NOTE-PR-DANGLING" || f.rule == "NOTE-PR-LISTING"), "findings: {:?}", full.findings);
    }

    /// The note collections are pooled-budgeted across parts: three
    /// settings parts listing two ids each overflow a fact budget of 3
    /// in aggregate even though no single part caps — the join must
    /// report incompleteness and make no dangling claims.
    #[test]
    fn note_pr_listings_pooled_cap_suppresses_the_join() {
        let settings_ct = "application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml";
        let ct = format!(
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/word/settings.xml" ContentType="{settings_ct}"/><Override PartName="/word/settings2.xml" ContentType="{settings_ct}"/><Override PartName="/word/settings3.xml" ContentType="{settings_ct}"/></Types>"#,
            package::MAIN_CONTENT_TYPE
        );
        let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{NS_W}"><w:body><w:p/></w:body></w:document>"#).into_bytes();
        let listing = |a: u8, b: u8| {
            format!(r#"<?xml version="1.0"?><w:settings xmlns:w="{NS_W}"><w:footnotePr><w:footnote w:id="{a}"/><w:footnote w:id="{b}"/></w:footnotePr></w:settings>"#).into_bytes()
        };
        let zip = tiny_zip(&[
            ("[Content_Types].xml", ct.into_bytes()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/settings.xml", listing(1, 2)),
            ("word/settings2.xml", listing(3, 4)),
            ("word/settings3.xml", listing(5, 6)),
        ]);
        let report = validate_bytes_limited(&zip, Limits { facts: 3, ..Limits::default() });
        assert!(
            !report.findings.iter().any(|f| f.rule == "NOTE-PR-DANGLING" || f.rule == "NOTE-PR-LISTING"),
            "a pooled-capped join must not judge listings: {:?}",
            report.findings
        );
        assert!(report.coverage_notes.iter().any(|n| n.contains("footnotePr")), "notes: {:?}", report.coverage_notes);
    }

    /// Build a package whose main part carries `n` contentPart
    /// references, all resolving to an application/xml target (so each
    /// collected id is a CONTENTPART-TYPE finding).
    fn content_part_zip(n: usize) -> Vec<u8> {
        let ct = format!(
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/word/data.xml" ContentType="application/xml"/></Types>"#,
            package::MAIN_CONTENT_TYPE
        );
        let elems: String = (0..n).map(|i| format!(r#"<w:p><w:r><w:contentPart r:id="cp{i}"/></w:r></w:p>"#)).collect();
        let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{NS_W}" xmlns:r="{NS_R}"><w:body>{elems}</w:body></w:document>"#).into_bytes();
        let rels: String = (0..n)
            .map(|i| format!(r#"<Relationship Id="cp{i}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml" Target="data.xml"/>"#))
            .collect();
        let doc_rels = format!(
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
        )
        .into_bytes();
        tiny_zip(&[
            ("[Content_Types].xml", ct.into_bytes()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", doc_rels),
            ("word/data.xml", b"<root/>".to_vec()),
        ])
    }

    /// The contentPart collection honours the fact budget (C3 review):
    /// at a budget of 4 with 6 references, exactly the 4 collected ids
    /// are judged, the run is marked incomplete, and the shortfall is
    /// disclosed — collection does not run past the budget.
    #[test]
    fn content_part_collection_honours_the_fact_budget() {
        let report = validate_bytes_limited(&content_part_zip(6), Limits { facts: 4, ..Limits::default() });
        let fired = report.findings.iter().filter(|f| f.rule == "CONTENTPART-TYPE").count();
        assert_eq!(fired, 4, "findings: {:?}", report.findings);
        assert!(report.coverage_notes.iter().any(|n| n.contains("contentPart")), "notes: {:?}", report.coverage_notes);
        // Established errors keep FAIL precedence over the incomplete
        // marking (the note-join rules differ: there an omission can
        // invalidate a "missing" claim; a collected contentPart
        // resolving to a wrong type is a defect on its own).
        assert_eq!(report.verdict, Verdict::Fail, "findings: {:?}", report.findings);
        // Control: at full limits all six are judged, with no note.
        let full = validate_bytes(&content_part_zip(6));
        assert_eq!(full.findings.iter().filter(|f| f.rule == "CONTENTPART-TYPE").count(), 6);
        assert!(!full.coverage_notes.iter().any(|n| n.contains("contentPart")));
    }

    /// Exemptions obey both fact budgets; capped native references without InkML do not change the verdict.
    #[test]
    fn ct_mismatch_contentpart_exemptions_honour_fact_budgets() {
        for (native, target_ct, limited_verdict) in
            [(false, "text/xml", Verdict::Incomplete), (true, "application/inkml+xml", Verdict::Incomplete), (true, "application/xml", Verdict::Pass)]
        {
            for (main_count, header_count) in [(3, 0), (1, 1)] {
                let tag = if native { "w14:contentPart" } else { "w:contentPart" };
                let elements = |count| format!(r#"<{tag} r:id="cp"/>"#).repeat(count);
                let main = format!(
                    r#"<w:document xmlns:w="{NS_W}" xmlns:r="{NS_R}" xmlns:mc="{NS_MC}" xmlns:w14="{NS_W14}"><w:body><w:p><mc:AlternateContent><mc:Fallback>{}</mc:Fallback></mc:AlternateContent></w:p></w:body></w:document>"#,
                    elements(main_count)
                );
                let header = format!(
                    r#"<w:hdr xmlns:w="{NS_W}" xmlns:r="{NS_R}" xmlns:mc="{NS_MC}" xmlns:w14="{NS_W14}"><w:p><mc:AlternateContent><mc:Fallback>{}</mc:Fallback></mc:AlternateContent></w:p></w:hdr>"#,
                    elements(header_count)
                );
                let cp_rels = rels(
                    r#"<Relationship Id="cp" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml" Target="data.xml"/>"#,
                );
                let ct = String::from_utf8(content_types_with_override("word/data.xml", target_ct)).unwrap().replace(
                    "</Types>",
                    &format!(r#"<Override PartName="/word/header.xml" ContentType="{}"/></Types>"#, package::HEADER_CONTENT_TYPE),
                );
                let bytes = tiny_zip(&[
                    ("[Content_Types].xml", ct.into_bytes()),
                    ("_rels/.rels", package_rels()),
                    ("word/document.xml", main.into_bytes()),
                    ("word/_rels/document.xml.rels", cp_rels.clone()),
                    ("word/header.xml", header.into_bytes()),
                    ("word/_rels/header.xml.rels", if header_count > 0 { cp_rels } else { doc_rels() }),
                    ("word/data.xml", b"<root/>".to_vec()),
                ]);
                let report = validate_bytes_limited(&bytes, Limits { facts: 1, ..Limits::default() });
                assert_eq!(report.verdict, limited_verdict, "{main_count}/{header_count}: {:?}", report.findings);
                assert!(report.findings.is_empty(), "{:?}", report.findings);
                assert_eq!(
                    report.coverage_notes.iter().any(|n| n.contains("contentPart") && n.contains("fact budget")),
                    limited_verdict == Verdict::Incomplete,
                    "{:?}",
                    report.coverage_notes
                );
                assert_eq!(validate_bytes(&bytes).verdict, Verdict::Pass);
            }
        }
    }

    /// The #5982 exemption is per-owner: when the MAIN part's
    /// contentPart collection caps out, a different owner whose
    /// collection completed (here a header with no contentParts at
    /// all) still gets its customXml pairing judged.
    #[test]
    fn ct_mismatch_survives_another_owners_capped_contentpart_collection() {
        let ct = format!(
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/word/header1.xml" ContentType="{}"/><Override PartName="/word/data/bad.xml" ContentType="text/xml"/></Types>"#,
            package::MAIN_CONTENT_TYPE,
            package::HEADER_CONTENT_TYPE
        );
        let elems: String = (0..6).map(|i| format!(r#"<w:p><w:r><w:contentPart r:id="cp{i}"/></w:r></w:p>"#)).collect();
        let doc = format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="{NS_W}" xmlns:r="{NS_R}"><w:body>{elems}<w:sectPr><w:headerReference w:type="default" r:id="rIdH"/></w:sectPr></w:body></w:document>"#
        )
        .into_bytes();
        let mut rel_body: String = (0..6)
            .map(|i| format!(r#"<Relationship Id="cp{i}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml" Target="data.xml"/>"#))
            .collect();
        rel_body.push_str(
            r#"<Relationship Id="rIdH" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>"#,
        );
        let doc_rels = rels(&rel_body);
        let header = format!(r#"<?xml version="1.0"?><w:hdr xmlns:w="{NS_W}"><w:p/></w:hdr>"#).into_bytes();
        let header_rels = rels(
            r#"<Relationship Id="rIdX" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml" Target="data/bad.xml"/>"#,
        );
        let bytes = tiny_zip(&[
            ("[Content_Types].xml", ct.into_bytes()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", doc_rels),
            ("word/header1.xml", header),
            ("word/_rels/header1.xml.rels", header_rels),
            ("word/data/bad.xml", b"<root/>".to_vec()),
            ("word/data.xml", b"<root/>".to_vec()),
        ]);
        let report = validate_bytes_limited(&bytes, Limits { facts: 4, ..Limits::default() });
        let f = report
            .findings
            .iter()
            .find(|f| f.rule == "CT-MISMATCH")
            .unwrap_or_else(|| panic!("CT-MISMATCH was suppressed by another owner's cap; findings: {:?}", report.findings));
        assert_eq!(f.part.as_deref(), Some("word/data/bad.xml"));
        assert!(
            report.coverage_notes.iter().any(|n| n.contains("contentPart")),
            "the capped collection must still be disclosed; notes: {:?}",
            report.coverage_notes
        );
    }

    /// An owner that was never scanned (here the main part exceeds the
    /// per-part byte budget) has unestablished contentPart consumption:
    /// the customXml pairing must not fire for its relationships.
    #[test]
    fn ct_mismatch_skips_an_unscanned_owner() {
        let padding = "x".repeat(5000);
        let doc = format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="{NS_W}" xmlns:r="{NS_R}"><w:body><w:p><w:r><w:contentPart r:id="cp"/></w:r></w:p><!-- {padding} --></w:body></w:document>"#
        )
        .into_bytes();
        let doc_rels =
            rels(r#"<Relationship Id="cp" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml" Target="data.xml"/>"#);
        let bytes = tiny_zip(&[
            ("[Content_Types].xml", content_types_with_override("word/data.xml", "text/xml")),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", doc_rels),
            ("word/data.xml", b"<root/>".to_vec()),
        ]);
        let report = validate_bytes_limited(&bytes, Limits { part_bytes: 4096, ..Limits::default() });
        assert!(!report.findings.iter().any(|f| f.rule == "CT-MISMATCH"), "findings: {:?}", report.findings);
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
    }

    /// The byte-limit test's twin at the archive entry limit: the
    /// owner part falls beyond the examined prefix, so it is not
    /// proven absent and its consumption is unestablished — the
    /// customXml pairing must not fire for its relationships.
    #[test]
    fn ct_mismatch_skips_an_owner_beyond_the_entry_limit() {
        let ct = format!(
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/document.xml" ContentType="{}"/><Override PartName="/data.xml" ContentType="text/xml"/></Types>"#,
            package::MAIN_CONTENT_TYPE
        );
        let root_rels = rels(
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="document.xml"/>"#,
        );
        let extra_rels =
            rels(r#"<Relationship Id="cp" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml" Target="data.xml"/>"#);
        let extra = format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="{NS_W}" xmlns:r="{NS_R}"><w:body><w:p><w:r><w:contentPart r:id="cp"/></w:r></w:p></w:body></w:document>"#
        )
        .into_bytes();
        let bytes = tiny_zip(&[
            ("[Content_Types].xml", ct.into_bytes()),
            ("_rels/.rels", root_rels),
            ("document.xml", document()),
            ("_rels/extra.xml.rels", extra_rels),
            ("data.xml", b"<root/>".to_vec()),
            ("extra.xml", extra),
        ]);
        let report = validate_bytes_limited(&bytes, Limits { entries: 5, ..Limits::default() });
        assert!(!report.findings.iter().any(|f| f.rule == "CT-MISMATCH"), "findings: {:?}", report.findings);
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
    }

    /// The mirror image of the budget test above: when every collected
    /// reference is valid (a text/xml target) and the budget still
    /// truncates collection, the verdict is INCOMPLETE — not PASS,
    /// since not every reference was checked, and not FAIL, since no
    /// defect was established.
    #[test]
    fn content_part_capped_with_valid_references_is_incomplete() {
        let ct = format!(
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/word/data.xml" ContentType="text/xml"/></Types>"#,
            package::MAIN_CONTENT_TYPE
        );
        let elems: String = (0..6).map(|i| format!(r#"<w:p><w:r><w:contentPart r:id="cp{i}"/></w:r></w:p>"#)).collect();
        let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{NS_W}" xmlns:r="{NS_R}"><w:body>{elems}</w:body></w:document>"#).into_bytes();
        // A non-catalogue relationship type keeps CT-MISMATCH out of
        // the scenario (it prescribes application/xml for customXml
        // relationships); CONTENTPART-TYPE resolves by id regardless.
        let rels: String =
            (0..6).map(|i| format!(r#"<Relationship Id="cp{i}" Type="https://example.com/relationships/contentPart" Target="data.xml"/>"#)).collect();
        let doc_rels = format!(
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
        )
        .into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", ct.into_bytes()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", doc_rels),
            ("word/data.xml", b"<root/>".to_vec()),
        ]);
        let report = validate_bytes_limited(&zip, Limits { facts: 4, ..Limits::default() });
        assert_eq!(report.findings.iter().filter(|f| f.rule == "CONTENTPART-TYPE").count(), 0, "findings: {:?}", report.findings);
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(report.coverage_notes.iter().any(|n| n.contains("contentPart")), "notes: {:?}", report.coverage_notes);
    }

    /// The pooled budget binds across owners: two parts with three
    /// references each stay under a per-part budget of 4 individually
    /// but overflow it pooled, so the pooled collection truncates and
    /// the shortfall is disclosed the same way.
    #[test]
    fn content_part_pooled_budget_binds_across_owners() {
        let ct = format!(
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/word/data.xml" ContentType="application/xml"/></Types>"#,
            package::MAIN_CONTENT_TYPE
        );
        let elems = |prefix: &str| -> String { (0..3).map(|i| format!(r#"<w:p><w:r><w:contentPart r:id="{prefix}{i}"/></w:r></w:p>"#)).collect() };
        let rels = |prefix: &str| -> Vec<u8> {
            let rs: String = (0..3)
                .map(|i| format!(r#"<Relationship Id="{prefix}{i}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml" Target="data.xml"/>"#))
                .collect();
            format!(
                r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rs}</Relationships>"#
            )
            .into_bytes()
        };
        let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{NS_W}" xmlns:r="{NS_R}"><w:body>{}</w:body></w:document>"#, elems("a"))
            .into_bytes();
        let other = format!(r#"<?xml version="1.0"?><w:other xmlns:w="{NS_W}" xmlns:r="{NS_R}">{}</w:other>"#, elems("b")).into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", ct.into_bytes()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", rels("a")),
            ("word/other.xml", other),
            ("word/_rels/other.xml.rels", rels("b")),
            ("word/data.xml", b"<root/>".to_vec()),
        ]);
        let report = validate_bytes_limited(&zip, Limits { facts: 4, ..Limits::default() });
        let fired = report.findings.iter().filter(|f| f.rule == "CONTENTPART-TYPE").count();
        assert_eq!(fired, 4, "findings: {:?}", report.findings);
        assert!(report.coverage_notes.iter().any(|n| n.contains("contentPart")), "notes: {:?}", report.coverage_notes);
    }

    /// Many distinct ids are all judged (the dedupe/index path is not
    /// quadratic-gated by a finding cap) — 2,000 references resolve and
    /// report individually.
    #[test]
    fn content_part_many_distinct_ids_all_judged() {
        let report = validate_bytes(&content_part_zip(2_000));
        assert_eq!(report.findings.iter().filter(|f| f.rule == "CONTENTPART-TYPE").count(), 2_000);
    }

    /// §3 canary (a): a budget stop must not hide a defect the scanned prefix already establishes.
    /// An extra `fldChar end` at depth 0 is unbalanced whatever follows, so the run FAILs (not merely
    /// INCOMPLETE) even though the event budget stopped the scan.
    #[test]
    fn budget_stopped_scan_still_reports_a_prefix_established_field_imbalance() {
        let body = format!(r#"<w:p><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>{}"#, "<w:r><w:t>x</w:t></w:r>".repeat(80));
        let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{}"><w:body>{body}</w:body></w:document>"#, NS_W).into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", doc_rels()),
        ]);
        let stopped = validate_bytes_limited(&zip, Limits { events: 25, ..Limits::default() });
        // Independently assert the budget really stopped the scan: the part's event-budget note is
        // emitted only when the scan breaks early on the ceiling, so it cannot be produced by a scan
        // that read the whole part. Without this the test would not discriminate a real stop from a
        // complete scan that merely skipped the prefix report.
        assert!(
            stopped.coverage_notes.iter().any(|n| n.contains("word/document.xml") && n.contains("XML events")),
            "the event budget must be shown to have stopped the scan: {:?}",
            stopped.coverage_notes
        );
        assert!(
            stopped.findings.iter().any(|f| f.rule == "FLD-UNBALANCED" && f.part.as_deref() == Some("word/document.xml")),
            "a prefix-established imbalance must survive the budget stop: {:?}",
            stopped.findings
        );
        assert_eq!(stopped.verdict, Verdict::Fail, "an established Error outranks the stop's INCOMPLETE: {:?}", stopped.findings);
    }

    /// §3 canary (b): a malformed part whose scanned prefix leaves a `fldChar begin` unclosed marks
    /// the run INCOMPLETE with a note naming the part, and reports no FLD-UNBALANCED Error, because
    /// the completion-dependent check that would decide it could not run.
    #[test]
    fn malformed_part_with_unmatched_begin_is_incomplete_without_a_field_error() {
        // An unreferenced part: malformed is Warning-only by calibration, so INCOMPLETE is decisive.
        let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{}"><w:body><w:p><w:r><w:fldChar w:fldCharType="begin"/></w:r></w:p>"#, NS_W)
            .into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", document()),
            ("word/_rels/document.xml.rels", doc_rels()),
            ("word/unused.xml", doc),
        ]);
        let report = validate_bytes(&zip);
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(
            !report.findings.iter().any(|f| f.rule == "FLD-UNBALANCED"),
            "the completion-dependent field check must not run on the malformed prefix: {:?}",
            report.findings
        );
        assert!(
            report.coverage_notes.iter().any(|n| n.contains("word/unused.xml") && n.contains("pairing")),
            "the INCOMPLETE must be explained by a note naming the part and the check: {:?}",
            report.coverage_notes
        );
    }

    /// §3 canary (c): a malformed part whose prefix holds a fully matched pair changes nothing — no
    /// completion-dependent report would have fired, so no INCOMPLETE is added. A matched **bookmark**
    /// pair is used deliberately: it leaves both sets non-empty but equal, so a trigger written as
    /// "sets are non-empty" would wrongly fire, while the correct "sets differ" trigger does not.
    #[test]
    fn malformed_part_with_a_matched_pair_is_unchanged() {
        let doc = format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="{}"><w:body><w:p><w:bookmarkStart w:id="1" w:name="a"/><w:bookmarkEnd w:id="1"/></w:p>"#,
            NS_W
        )
        .into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types_with_override("word/unused.xml", package::COMMENTS_CONTENT_TYPE)),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", document()),
            ("word/_rels/document.xml.rels", doc_rels()),
            ("word/unused.xml", doc),
        ]);
        let report = validate_bytes(&zip);
        assert_eq!(report.verdict, Verdict::Pass, "findings: {:?}", report.findings);
        assert!(
            report.findings.iter().any(|f| f.rule == "XML-MALFORMED" && f.severity == Severity::Warning),
            "the malformed unreferenced part is still a Warning: {:?}",
            report.findings
        );
        assert!(!report.coverage_notes.iter().any(|n| n.contains("pairing")), "a matched pair leaves nothing pending: {:?}", report.coverage_notes);
    }

    /// Finding 2 canary: a bookmark pair split by the fact budget must not be reported as unpaired
    /// on the partial scan; the same document under normal limits still reports `BOOKMARK-UNPAIRED`.
    #[test]
    fn fact_capped_scan_does_not_report_a_split_bookmark() {
        let body = concat!(
            r#"<w:p><w:bookmarkEnd w:id="5"/></w:p>"#,
            r#"<w:p><w:bookmarkStart w:id="1" w:name="a"/></w:p>"#,
            r#"<w:p><w:bookmarkStart w:id="2" w:name="b"/></w:p>"#,
            r#"<w:p><w:bookmarkStart w:id="3" w:name="c"/></w:p>"#,
            r#"<w:p><w:bookmarkStart w:id="4" w:name="d"/></w:p>"#,
            r#"<w:p><w:bookmarkEnd w:id="1"/></w:p>"#,
            r#"<w:p><w:bookmarkEnd w:id="2"/></w:p>"#,
        );
        let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{}"><w:body>{body}</w:body></w:document>"#, NS_W).into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", doc_rels()),
        ]);
        let capped = validate_bytes_limited(&zip, Limits { facts: 3, ..Limits::default() });
        assert_eq!(capped.verdict, Verdict::Incomplete, "findings: {:?}", capped.findings);
        assert!(
            !capped.findings.iter().any(|f| f.rule == "BOOKMARK-UNPAIRED"),
            "a fact-capped scan must not report a split bookmark: {:?}",
            capped.findings
        );
        // Regression control: fully scanned, the unpaired bookmark is reported.
        let full = validate_bytes(&zip);
        assert!(full.findings.iter().any(|f| f.rule == "BOOKMARK-UNPAIRED"), "findings: {:?}", full.findings);
    }

    #[test]
    /// The comment-range mirror of the split-bookmark canary: a range pair
    /// split by the fact budget must not be reported as unpaired on the
    /// partial scan; the same document under normal limits still reports
    /// `COMMENT-RANGE-UNPAIRED`.
    fn fact_capped_scan_does_not_report_a_split_comment_range() {
        let body = concat!(
            r#"<w:p><w:commentRangeEnd w:id="5"/></w:p>"#,
            r#"<w:p><w:commentRangeStart w:id="1"/></w:p>"#,
            r#"<w:p><w:commentRangeStart w:id="2"/></w:p>"#,
            r#"<w:p><w:commentRangeStart w:id="3"/></w:p>"#,
            r#"<w:p><w:commentRangeStart w:id="4"/></w:p>"#,
            r#"<w:p><w:commentRangeEnd w:id="1"/></w:p>"#,
            r#"<w:p><w:commentRangeEnd w:id="2"/></w:p>"#,
        );
        let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{}"><w:body>{body}</w:body></w:document>"#, NS_W).into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", doc_rels()),
        ]);
        let capped = validate_bytes_limited(&zip, Limits { facts: 3, ..Limits::default() });
        assert_eq!(capped.verdict, Verdict::Incomplete, "findings: {:?}", capped.findings);
        assert!(
            !capped.findings.iter().any(|f| f.rule == "COMMENT-RANGE-UNPAIRED"),
            "a fact-capped scan must not report a split comment range: {:?}",
            capped.findings
        );
        // Regression control: fully scanned, the unpaired ranges are reported.
        let full = validate_bytes(&zip);
        assert!(full.findings.iter().any(|f| f.rule == "COMMENT-RANGE-UNPAIRED"), "findings: {:?}", full.findings);
    }

    #[test]
    /// The paraId collection rides the fact budget: more distinct values
    /// than the cap makes the verdict INCOMPLETE, never a silent pass —
    /// a duplicate hiding beyond the cap must not be reported as absent.
    fn paraid_collection_beyond_the_fact_cap_is_incomplete() {
        let mut body = String::new();
        for i in 1..=6u32 {
            body.push_str(&format!(r#"<w:p xmlns:w14="{NS_W14}" w14:paraId="{i:08X}" w14:textId="11111111"/>"#));
        }
        let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{NS_W}"><w:body>{body}</w:body></w:document>"#).into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", doc_rels()),
        ]);
        let capped = validate_bytes_limited(&zip, Limits { facts: 3, ..Limits::default() });
        assert_eq!(capped.verdict, Verdict::Incomplete, "findings: {:?}", capped.findings);
        // Control: the same document under normal limits is a clean PASS.
        let full = validate_bytes(&zip);
        assert_eq!(full.verdict, Verdict::Pass, "findings: {:?}", full.findings);
    }

    #[test]
    /// At the cap boundary the collection stays honest both ways: a
    /// duplicate among the values that WERE collected is still
    /// reported, and a duplicate among dropped values is not invented
    /// (the verdict is INCOMPLETE instead).
    fn paraid_cap_boundary_reports_kept_dups_and_invents_none() {
        let doc_with = |ids: &[u32]| {
            let mut body = String::new();
            for i in ids {
                body.push_str(&format!(r#"<w:p xmlns:w14="{NS_W14}" w14:paraId="{i:08X}" w14:textId="11111111"/>"#));
            }
            let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{NS_W}"><w:body>{body}</w:body></w:document>"#).into_bytes();
            tiny_zip(&[
                ("[Content_Types].xml", content_types()),
                ("_rels/.rels", package_rels()),
                ("word/document.xml", doc),
                ("word/_rels/document.xml.rels", doc_rels()),
            ])
        };
        // 1,2,3,1 at cap 3: the repeat value was collected — reported.
        let kept = validate_bytes_limited(&doc_with(&[1, 2, 3, 1]), Limits { facts: 3, ..Limits::default() });
        assert!(kept.findings.iter().any(|f| f.rule == "PARAID-DUP"), "findings: {:?}", kept.findings);
        // 1,2,3,4,4 at cap 3: value 4 was dropped — no invented dup.
        let dropped = validate_bytes_limited(&doc_with(&[1, 2, 3, 4, 4]), Limits { facts: 3, ..Limits::default() });
        assert!(!dropped.findings.iter().any(|f| f.rule == "PARAID-DUP"), "findings: {:?}", dropped.findings);
        assert_eq!(dropped.verdict, Verdict::Incomplete, "findings: {:?}", dropped.findings);
    }

    #[test]
    /// Mutation: cutting the pooled fact cap so a dangling comment reference is dropped flips the clean verdict to INCOMPLETE (undropped, the same package FAILs).
    fn dropped_dangling_comment_reference_is_incomplete() {
        let mut refs = String::new();
        for _ in 0..3 {
            refs.push_str(r#"<w:commentReference w:id="1"/>"#);
        }
        refs.push_str(r#"<w:commentReference w:id="9"/>"#);
        let doc =
            format!(r#"<?xml version="1.0"?><w:document xmlns:w="{}"><w:body><w:p><w:r>{refs}</w:r></w:p></w:body></w:document>"#, NS_W).into_bytes();
        let comments =
            format!(r#"<?xml version="1.0"?><w:comments xmlns:w="{}"><w:comment w:id="1"><w:p/></w:comment></w:comments>"#, NS_W).into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types_with_override("word/comments.xml", package::COMMENTS_CONTENT_TYPE)),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rC" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/></Relationships>"#.to_vec()),
            ("word/comments.xml", comments),
        ]);
        let report = validate_bytes_limited(&zip, Limits { facts: 3, ..Limits::default() });
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(
            report.coverage_notes.iter().any(|n| n.contains("fact/reference budget") && n.contains("word/document.xml")),
            "{:?}",
            report.coverage_notes
        );
    }

    #[test]
    /// Mutation: the same dropped-fact flip for a dangling style reference.
    fn dropped_dangling_style_reference_is_incomplete() {
        let mut body = String::new();
        for _ in 0..3 {
            body.push_str(r#"<w:p><w:pPr><w:pStyle w:val="Normal"/></w:pPr></w:p>"#);
        }
        body.push_str(r#"<w:p><w:pPr><w:pStyle w:val="Missing"/></w:pPr></w:p>"#);
        let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{}"><w:body>{body}</w:body></w:document>"#, NS_W).into_bytes();
        let styles = format!(
            r#"<?xml version="1.0"?><w:styles xmlns:w="{}"><w:style w:type="paragraph" w:styleId="Normal"><w:name w:val="Normal"/></w:style></w:styles>"#,
            NS_W
        )
        .into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types_with_override("word/styles.xml", package::STYLES_CONTENT_TYPE)),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", doc_rels()),
            ("word/styles.xml", styles),
        ]);
        let report = validate_bytes_limited(&zip, Limits { facts: 3, ..Limits::default() });
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(
            report.coverage_notes.iter().any(|n| n.contains("fact/reference budget") && n.contains("word/document.xml")),
            "the dropped-reference canary must assert its note and location: {:?}",
            report.coverage_notes
        );
    }

    #[test]
    /// A supporting part whose own parser exhausts the event budget marks the run INCOMPLETE.
    fn auxiliary_parser_event_budget_exhaustion_is_incomplete() {
        // A numbering part named without the `.xml` suffix is not read by the main scan, so only
        // the auxiliary numbering parser can hit the event budget (H1).
        let mut numbering = format!(r#"<w:numbering xmlns:w="{}">"#, NS_W);
        for i in 0..300 {
            numbering.push_str(&format!(r#"<w:num w:numId="{i}"><w:abstractNumId w:val="0"/></w:num>"#));
        }
        numbering.push_str("</w:numbering>");
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types_with_override("word/numbering.dat", package::NUMBERING_CONTENT_TYPE)),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", document()),
            (
                "word/_rels/document.xml.rels",
                rels(
                    r#"<Relationship Id="rN" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.dat"/>"#,
                ),
            ),
            ("word/numbering.dat", numbering.into_bytes()),
        ]);
        let report = validate_bytes_limited(&zip, Limits { events: 60, ..Limits::default() });
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(
            report.coverage_notes.iter().any(|n| n.contains("word/numbering.dat") && (n.contains("event") || n.contains("depth"))),
            "the auxiliary parser's exhaustion must identify the event/depth mechanism and its part: {:?}",
            report.coverage_notes
        );
    }

    #[test]
    /// Move range-start ids past the fact budget exhaust the pool and mark the run INCOMPLETE.
    fn repeated_move_start_ids_exhaust_the_move_budget() {
        let mut body = String::new();
        for _ in 0..8 {
            body.push_str(r#"<w:p><w:moveFromRangeStart w:id="1" w:name="A"/></w:p>"#);
        }
        let doc = format!(r#"<?xml version="1.0"?><w:document xmlns:w="{}"><w:body>{body}</w:body></w:document>"#, NS_W).into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", doc_rels()),
        ]);
        let report = validate_bytes_limited(&zip, Limits { facts: 3, ..Limits::default() });
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(
            report.coverage_notes.iter().any(|n| n.contains("fact/reference budget") && n.contains("word/document.xml")),
            "the move-budget exhaustion must name the mechanism and the affected part: {:?}",
            report.coverage_notes
        );
    }

    #[test]
    /// A supporting part nested past the depth limit fails its parse outright, so dependent absence-judging checks skip rather than report.
    fn auxiliary_parser_depth_exhaustion_is_terminal() {
        // J1: a low-depth auxiliary parser must stop at the crossing. A duplicate Override placed
        // AFTER the crossing would fire CT-DUP-OVERRIDE if the content-type parser kept reading;
        // early termination means it is never seen — INCOMPLETE, a depth-budget note naming the
        // part, and no CT-DUP-OVERRIDE.
        let deep = format!(
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{}"/>{}{}<Override PartName="/word/document.xml" ContentType="{}"/></Types>"#,
            package::MAIN_CONTENT_TYPE,
            "<x>".repeat(8),
            "</x>".repeat(8),
            package::MAIN_CONTENT_TYPE,
        );
        let zip = tiny_zip(&[
            ("[Content_Types].xml", deep.into_bytes()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", document()),
            ("word/_rels/document.xml.rels", doc_rels()),
        ]);
        let report = validate_bytes_limited(&zip, Limits { depth: 3, ..Limits::default() });
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(
            report.coverage_notes.iter().any(|n| n.contains("[Content_Types].xml") && (n.contains("depth") || n.contains("event"))),
            "the depth exhaustion note must name the mechanism and the part: {:?}",
            report.coverage_notes
        );
        assert!(
            !report.findings.iter().any(|f| f.rule == "CT-DUP-OVERRIDE"),
            "early termination must drop the declaration after the crossing: {:?}",
            report.findings
        );
    }

    #[test]
    /// A .rels part nested past the depth limit fails its parse outright, so reference checks against it skip.
    fn deep_rels_depth_exhaustion_is_terminal() {
        // J1: a deeply nested .rels part must stop at the depth crossing, so a dangling
        // relationship placed AFTER it is never recorded — INCOMPLETE, a depth-budget note naming
        // the rels part, and no REL-DANGLING for the later relationship.
        let deep = format!(
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{}{}<Relationship Id="rB" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/missing.png"/></Relationships>"#,
            "<x>".repeat(8),
            "</x>".repeat(8),
        );
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", document()),
            ("word/_rels/document.xml.rels", doc_rels()),
            ("word/_rels/extra.xml.rels", deep.into_bytes()),
        ]);
        let report = validate_bytes_limited(&zip, Limits { depth: 6, ..Limits::default() });
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(
            report.coverage_notes.iter().any(|n| n.contains("word/_rels/extra.xml.rels") && (n.contains("depth") || n.contains("event"))),
            "the deep-rels exhaustion note must name the mechanism and the part: {:?}",
            report.coverage_notes
        );
        assert!(
            !report.findings.iter().any(|f| f.rule == "REL-DANGLING" && f.part.as_deref() == Some("word/_rels/extra.xml.rels")),
            "early termination must drop the relationship after the crossing: {:?}",
            report.findings
        );
    }

    #[test]
    /// When the footnote fact budget cuts off collection, a coverage note records that later notes went unrecorded.
    fn note_budget_exhaustion_leaves_a_marker() {
        let comments = format!(
            r#"<?xml version="1.0"?><w:comments xmlns:w="{}"><w:comment w:id="1"><w:p/></w:comment><w:comment w:id="2"><w:p/></w:comment><w:comment w:id="3"><w:p/></w:comment></w:comments>"#,
            NS_W
        )
        .into_bytes();
        let zip = tiny_zip(&[
            ("[Content_Types].xml", content_types_with_override("word/comments.xml", package::COMMENTS_CONTENT_TYPE)),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", document()),
            ("word/_rels/document.xml.rels", doc_rels()),
            ("word/comments.xml", comments),
        ]);
        let report = validate_bytes_limited(&zip, Limits { notes: 1, ..Limits::default() });
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(
            report.coverage_notes.iter().any(|n| n.contains("coverage-note budget exhausted")),
            "exhausting the note budget must leave a marker: {:?}",
            report.coverage_notes
        );
    }

    #[test]
    /// Pooled story-fact exhaustion produces one coverage note naming the first part whose facts were cut.
    fn pooled_story_fact_exhaustion_names_the_first_affected_part() {
        // K2: the pooled story-fact budget is aggregate across parts. Two comments parts each hold
        // fewer than the cap, so neither hits its own per-part budget; only the pooled budget is
        // exceeded, and the note must name the mechanism and the first part that crossed it.
        let comments_a = format!(
            r#"<?xml version="1.0"?><w:comments xmlns:w="{}"><w:comment w:id="1"><w:p/></w:comment><w:comment w:id="2"><w:p/></w:comment></w:comments>"#,
            NS_W
        )
        .into_bytes();
        let comments_b = format!(
            r#"<?xml version="1.0"?><w:comments xmlns:w="{}"><w:comment w:id="3"><w:p/></w:comment><w:comment w:id="4"><w:p/></w:comment></w:comments>"#,
            NS_W
        )
        .into_bytes();
        let zip = tiny_zip(&[
            (
                "[Content_Types].xml",
                format!(
                    r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/word/commentsA.xml" ContentType="{}"/><Override PartName="/word/commentsB.xml" ContentType="{}"/></Types>"#,
                    package::MAIN_CONTENT_TYPE,
                    package::COMMENTS_CONTENT_TYPE,
                    package::COMMENTS_CONTENT_TYPE,
                )
                .into_bytes(),
            ),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", document()),
            ("word/_rels/document.xml.rels", doc_rels()),
            ("word/commentsA.xml", comments_a),
            ("word/commentsB.xml", comments_b),
        ]);
        let report = validate_bytes_limited(&zip, Limits { facts: 3, ..Limits::default() });
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(
            report.coverage_notes.iter().any(|n| n.contains("pooled story facts") && n.contains("word/commentsB.xml")),
            "the pooled-fact note must name the mechanism and the first affected part: {:?}",
            report.coverage_notes
        );
    }

    #[test]
    /// Pooled relationship-reference exhaustion produces one coverage note naming the first part whose references were cut.
    fn pooled_relationship_reference_exhaustion_names_the_first_affected_part() {
        // K2: the pooled relationship-reference budget is aggregate across parts. The main part's
        // two refs resolve; adding the header's two takes the pool past the cap, so the note must
        // name the mechanism and the header, the first part that crossed it.
        let doc = format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="{}" xmlns:r="{}"><w:body><w:p><w:hyperlink r:id="rL1"/><w:hyperlink r:id="rL2"/></w:p></w:body></w:document>"#,
            NS_W, NS_R
        )
        .into_bytes();
        let header = format!(
            r#"<?xml version="1.0"?><w:hdr xmlns:w="{}" xmlns:r="{}"><w:p><w:hyperlink r:id="rL1"/><w:hyperlink r:id="rL2"/></w:p></w:hdr>"#,
            NS_W, NS_R
        )
        .into_bytes();
        let hyperlink_rels = rels(concat!(
            r#"<Relationship Id="rL1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://one.example/" TargetMode="External"/>"#,
            r#"<Relationship Id="rL2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://two.example/" TargetMode="External"/>"#,
        ));
        let zip = tiny_zip(&[
            (
                "[Content_Types].xml",
                format!(
                    r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/word/header1.xml" ContentType="{}"/></Types>"#,
                    package::MAIN_CONTENT_TYPE,
                    package::HEADER_CONTENT_TYPE,
                )
                .into_bytes(),
            ),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", doc),
            ("word/_rels/document.xml.rels", hyperlink_rels.clone()),
            ("word/header1.xml", header),
            ("word/_rels/header1.xml.rels", hyperlink_rels),
        ]);
        let report = validate_bytes_limited(&zip, Limits { facts: 3, ..Limits::default() });
        assert_eq!(report.verdict, Verdict::Incomplete, "findings: {:?}", report.findings);
        assert!(
            report.coverage_notes.iter().any(|n| n.contains("relationship references") && n.contains("word/header1.xml")),
            "the pooled-reference note must name the mechanism and the first affected part: {:?}",
            report.coverage_notes
        );
    }

    /// Replace every occurrence of the same-length `from` with `to` in raw bytes (used to force a
    /// duplicate entry name the zip writer itself refuses to create).
    fn replace_same_len(data: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
        assert_eq!(from.len(), to.len(), "replacement must preserve length");
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            if data[i..].starts_with(from) {
                out.extend_from_slice(to);
                i += from.len();
            } else {
                out.push(data[i]);
                i += 1;
            }
        }
        out
    }

    /// §2 canary: four unique parts plus a duplicate of one, with `entries` equal to the
    /// deduplicated count. The zip reader collapses the duplicate, so the loader's `zip.len()` check
    /// does not fire; the raw central directory holds five records, and the budgeted parse examines
    /// only four. The run must not return a clean PASS: duplicate checking learns a raw record went
    /// unexamined and marks the run INCOMPLETE with a note naming itself. Under a generous budget the
    /// same package still reports the duplicate exactly as before.
    #[test]
    fn duplicate_checking_knows_when_it_was_truncated() {
        let parts = [
            ("[Content_Types].xml", content_types()),
            ("_rels/.rels", package_rels()),
            ("word/document.xml", document()),
            ("word/_rels/document.xml.rels", doc_rels()),
        ];
        // A fifth raw record duplicates `_rels/.rels`: its name is the same 11 bytes, so the reader
        // deduplicates it away while the central directory still lists five records.
        let mut all: Vec<(&str, Vec<u8>)> = parts.to_vec();
        all.push(("_rels/.xels", package_rels()));
        let zip = replace_same_len(&tiny_zip(&all), b"_rels/.xels", b"_rels/.rels");

        let capped = validate_bytes_limited(&zip, Limits { entries: 4, ..Limits::default() });
        assert_ne!(capped.verdict, Verdict::Pass, "a truncated duplicate scan must not pass silently: {:?}", capped.findings);
        assert_eq!(capped.verdict, Verdict::Incomplete, "findings: {:?}", capped.findings);
        assert!(
            capped.coverage_notes.iter().any(|n| n.contains("duplicate entry checking")),
            "the coverage note must name duplicate checking as the incomplete check: {:?}",
            capped.coverage_notes
        );

        // Regression control: under a generous budget the duplicate is reported exactly as before.
        let full = validate_bytes_limited(&zip, Limits::default());
        assert!(
            full.findings.iter().any(|f| f.rule == "PKG-DUP-ENTRY"),
            "the duplicate must still be reported under a generous budget: {:?}",
            full.findings
        );
    }
    /// Finding 1: event and depth limits leave declaration lookups unknown, just like entry caps.
    #[test]
    fn fredapps_1_xml_budget_declaration_completeness() {
        let doc = format!(r#"<w:document xmlns:w="{NS_W}" xmlns:r="{NS_R}"><w:body><w:p><w:hyperlink r:id="last"/></w:p></w:body></w:document>"#)
            .into_bytes();
        let last = r#"<Relationship Id="last" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.org" TargetMode="External"/>"#;
        for (prefix, limits) in [
            (
                (0..40)
                    .map(|i| format!(r#"<Relationship Id="r{i}" Type="urn:custom" Target="https://example.org" TargetMode="External"/>"#))
                    .collect::<String>(),
                Limits { events: 15, ..Limits::default() },
            ),
            ("<nested><nested><nested></nested></nested></nested>".to_string(), Limits { depth: 3, ..Limits::default() }),
        ] {
            let zip = tiny_zip(&[
                ("[Content_Types].xml", content_types()),
                ("_rels/.rels", package_rels()),
                ("word/document.xml", doc.clone()),
                (
                    "word/_rels/document.xml.rels",
                    format!(r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{prefix}{last}</Relationships>"#)
                        .into_bytes(),
                ),
            ]);
            let report = validate_bytes_limited(&zip, limits);
            assert_eq!(report.verdict, Verdict::Incomplete, "{report:?}");
            assert!(!report.findings.iter().any(|f| f.rule == "REL-REF-MISSING"), "{report:?}");
        }
    }
}

#[cfg(test)]
mod round2_reference_tests {
    //! Reference incompleteness includes both per-part and pooled fact drops.
    use super::*;
    use std::io::{Cursor, Write};

    /// A dropped reference must not turn the corresponding comment into a never-referenced note.
    #[test]
    fn round2_capped_comment_references() {
        let ns = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let ct = format!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/word/comments.xml" ContentType="{}"/><Override PartName="/word/header.xml" ContentType="{}"/></Types>"#,
            package::MAIN_CONTENT_TYPE,
            package::COMMENTS_CONTENT_TYPE,
            package::HEADER_CONTENT_TYPE
        );
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rC" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/><Relationship Id="rH" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header.xml"/></Relationships>"#;
        for pooled in [false, true] {
            let refs = if pooled { r#"<w:commentReference w:id="1"/>"# } else { r#"<w:commentReference w:id="1"/><w:commentReference w:id="2"/>"# };
            let doc = format!(r#"<w:document xmlns:w="{ns}"><w:body><w:p><w:r>{refs}</w:r></w:p></w:body></w:document>"#);
            let header_ref = if pooled { r#"<w:commentReference w:id="2"/>"# } else { "" };
            let header = format!(r#"<w:hdr xmlns:w="{ns}"><w:p><w:r>{header_ref}</w:r></w:p></w:hdr>"#);
            let comments = format!(r#"<w:comments xmlns:w="{ns}"><w:comment w:id="2"><w:p/></w:comment></w:comments>"#);
            let root = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rM" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
            for (name, data) in [
                ("[Content_Types].xml", ct.as_str()),
                ("_rels/.rels", root),
                ("word/_rels/document.xml.rels", rels),
                ("word/document.xml", &doc),
                ("word/header.xml", &header),
                ("word/comments.xml", &comments),
            ] {
                zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
                zip.write_all(data.as_bytes()).unwrap();
            }
            let bytes = zip.finish().unwrap().into_inner();
            let report = validate_bytes_limited(&bytes, Limits { facts: 1, ..Limits::default() });
            assert!(
                !report.coverage_notes.iter().any(|n| n.contains("id 2 in word/comments.xml is never referenced")),
                "pooled {pooled}: {report:?}"
            );
            zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        }
    }
}

#[cfg(test)]
mod round3_note_cap_tests {
    //! Caps invalidate only the owner whose supporting collection lost facts.
    use super::*;
    use std::io::{Cursor, Write};

    /// A glossary per-part or pooled cap must preserve the main owner's established note join.
    #[test]
    fn round3_note_caps_preserve_other_owner() {
        let ns = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
        let relns = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
        for (kind, ct) in [("footnote", package::FOOTNOTES_CONTENT_TYPE), ("endnote", package::ENDNOTES_CONTENT_TYPE)] {
            for dangling in [false, true] {
                for pooled in [false, true] {
                    let mut entries = vec![
                        (
                            "_rels/.rels".to_string(),
                            format!(
                                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rM" Type="{relns}/officeDocument" Target="word/document.xml"/></Relationships>"#
                            ),
                        ),
                        ("word/document.xml".to_string(), format!(r#"<w:document xmlns:w="{ns}"><w:body><w:p/></w:body></w:document>"#)),
                        ("word/glossary.xml".to_string(), format!(r#"<w:glossaryDocument xmlns:w="{ns}"><w:docParts/></w:glossaryDocument>"#)),
                    ];
                    let mut overrides = format!(
                        r#"<Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/word/glossary.xml" ContentType="{}"/>"#,
                        package::MAIN_CONTENT_TYPE,
                        package::GLOSSARY_CONTENT_TYPE
                    );
                    for gloss in [false, true] {
                        let prefix = if gloss { "gloss-" } else { "" };
                        let owner = if gloss { "glossary" } else { "document" };
                        let glossary_rel = if gloss {
                            String::new()
                        } else {
                            format!(r#"<Relationship Id="rG" Type="{relns}/glossaryDocument" Target="glossary.xml"/>"#)
                        };
                        entries.push((format!("word/_rels/{owner}.xml.rels"), format!(r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{glossary_rel}<Relationship Id="rN" Type="{relns}/{kind}s" Target="{prefix}{kind}s.xml"/><Relationship Id="rS" Type="{relns}/settings" Target="{prefix}settings.xml"/></Relationships>"#)));
                        let count = if pooled { 2 } else { 3 };
                        let padding = (1..=count).map(|id| format!(r#"<w:{kind} w:id="{id}"><w:p/></w:{kind}>"#)).collect::<String>();
                        let notes = if gloss && dangling {
                            padding
                        } else if !gloss && (!dangling || pooled) {
                            format!(r#"<w:{kind} w:id="-1" w:type="separator"><w:p/></w:{kind}>"#)
                        } else {
                            String::new()
                        };
                        let listing = if gloss && !dangling {
                            (1..=count).map(|id| format!(r#"<w:{kind} w:id="{id}"/>"#)).collect::<String>()
                        } else if !gloss && (dangling || pooled) {
                            format!(r#"<w:{kind} w:id="{}"/>"#, if pooled { "-2" } else { "-1" })
                        } else {
                            String::new()
                        };
                        entries.push((format!("word/{prefix}{kind}s.xml"), format!(r#"<w:{kind}s xmlns:w="{ns}">{notes}</w:{kind}s>"#)));
                        entries.push((
                            format!("word/{prefix}settings.xml"),
                            format!(r#"<w:settings xmlns:w="{ns}"><w:{kind}Pr>{listing}</w:{kind}Pr></w:settings>"#),
                        ));
                        overrides.push_str(&format!(r#"<Override PartName="/word/{prefix}{kind}s.xml" ContentType="{ct}"/><Override PartName="/word/{prefix}settings.xml" ContentType="{}"/>"#, package::SETTINGS_CONTENT_TYPE));
                    }
                    entries.push(("[Content_Types].xml".to_string(), format!(r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>{overrides}</Types>"#)));
                    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
                    for (name, text) in entries {
                        zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
                        zip.write_all(text.as_bytes()).unwrap();
                    }
                    let bytes = zip.finish().unwrap().into_inner();
                    let limited = validate_bytes_limited(&bytes, Limits { facts: 2, ..Limits::default() });
                    let rule = if dangling { "NOTE-PR-DANGLING" } else { "NOTE-PR-LISTING" };
                    assert_eq!(
                        limited.findings.iter().filter(|f| f.rule == rule).count(),
                        1,
                        "{kind}, dangling {dangling}, pooled {pooled}: {limited:?}"
                    );
                    if !dangling {
                        assert_eq!(limited.verdict, Verdict::Fail, "{limited:?}");
                        assert!(
                            limited.findings.iter().any(|f| f.rule == rule && f.part.as_deref() == Some(&format!("word/{kind}s.xml"))),
                            "{limited:?}"
                        );
                    }
                    assert!(limited.coverage_notes.iter().any(|n| n.contains("footnotePr/endnotePr") && n.contains("glossary")), "{limited:?}");
                    // The unlimited control has no skipped special-note joins.
                    let full = validate_bytes(&bytes);
                    assert!(full.findings.iter().any(|f| f.rule == rule), "{full:?}");
                    assert!(!full.coverage_notes.iter().any(|n| n.contains("could not be fully joined")), "{full:?}");
                }
            }
        }
    }
}

#[cfg(test)]
mod round4_comment_para_tests {
    //! Pooled paragraph loss must not manufacture owner-local comment-threading absence.
    use super::*;
    use std::io::{Cursor, Write};

    /// Build the reviewer's shape with three paragraph slots and one paragraph in the other owner.
    fn package(main_count: usize, missing_main: bool, missing_parent: bool) -> Vec<u8> {
        let relns = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
        let mut parts = vec![
            (
                "_rels/.rels".to_string(),
                format!(
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rM" Type="{relns}/officeDocument" Target="word/document.xml"/></Relationships>"#
                ),
            ),
            ("word/document.xml".to_string(), format!(r#"<w:document xmlns:w="{NS_W}"><w:body><w:p/></w:body></w:document>"#)),
            ("word/glossary.xml".to_string(), format!(r#"<w:glossaryDocument xmlns:w="{NS_W}"><w:docParts/></w:glossaryDocument>"#)),
        ];
        let mut overrides = format!(
            r#"<Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/word/glossary.xml" ContentType="{}"/>"#,
            package::MAIN_CONTENT_TYPE,
            package::GLOSSARY_CONTENT_TYPE
        );
        for gloss in [false, true] {
            let prefix = if gloss { "gloss-" } else { "" };
            let owner = if gloss { "glossary" } else { "document" };
            let extra =
                if gloss { String::new() } else { format!(r#"<Relationship Id="rG" Type="{relns}/glossaryDocument" Target="glossary.xml"/>"#) };
            parts.push((format!("word/_rels/{owner}.xml.rels"), format!(r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{extra}<Relationship Id="rC" Type="{relns}/comments" Target="{prefix}comments.xml"/><Relationship Id="rE" Type="{relns}/commentsExtended" Target="{prefix}commentsExtended.xml"/></Relationships>"#)));
            let paragraphs = if gloss {
                r#"<w:p w14:paraId="00030D41"/>"#.to_string()
            } else {
                (1..=main_count).map(|id| format!(r#"<w:p w14:paraId="{id:08X}"/>"#)).collect()
            };
            parts.push((
                format!("word/{prefix}comments.xml"),
                format!(r#"<w:comments xmlns:w="{NS_W}" xmlns:w14="{NS_W14}"><w:comment w:id="1">{paragraphs}</w:comment></w:comments>"#),
            ));
            let id = if gloss {
                "00030D41"
            } else if missing_main {
                "00000009"
            } else {
                "00000001"
            };
            let parent = if gloss && missing_parent { r#" w15:paraIdParent="00000008""# } else { "" };
            parts.push((
                format!("word/{prefix}commentsExtended.xml"),
                format!(r#"<w15:commentsEx xmlns:w15="{NS_W15}"><w15:commentEx w15:paraId="{id}"{parent}/></w15:commentsEx>"#),
            ));
            overrides.push_str(&format!(r#"<Override PartName="/word/{prefix}comments.xml" ContentType="{}"/><Override PartName="/word/{prefix}commentsExtended.xml" ContentType="{}"/>"#, package::COMMENTS_CONTENT_TYPE, package::COMMENTS_EXTENDED_CONTENT_TYPE));
        }
        parts.push(("[Content_Types].xml".to_string(), format!(r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>{overrides}</Types>"#)));
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in parts {
            zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    /// The exact pooled-loss reproduction; reducing main's paragraph count restores PASS.
    #[test]
    fn round4_pooled_comment_paragraph_loss() {
        let limits = Limits { facts: 3, ..Limits::default() };
        let control = validate_bytes_limited(&package(2, false, false), limits);
        assert_eq!(control.verdict, Verdict::Pass, "{control:?}");
        assert!(!control.findings.iter().any(|f| f.rule == "COMMENTEX-LINK"), "{control:?}");
        let report = validate_bytes_limited(&package(3, false, false), limits);
        assert_eq!(report.verdict, Verdict::Incomplete, "{report:?}");
        assert!(!report.findings.iter().any(|f| f.rule == "COMMENTEX-LINK"), "{report:?}");
        assert!(
            report.coverage_notes.iter().any(|n| n.contains("word/gloss-comments.xml") && n.contains("pooled comment paragraph IDs")),
            "{report:?}"
        );
        assert!(report.coverage_notes.iter().any(|n| n.contains("comment-threading") && n.contains("word/glossary.xml")), "{report:?}");
    }

    /// A complete owner's real absence and the incomplete owner's independent parent check survive.
    #[test]
    fn round4_pooled_comment_loss_preserves_independent_links() {
        let report = validate_bytes_limited(&package(3, true, true), Limits { facts: 3, ..Limits::default() });
        let links: Vec<_> = report.findings.iter().filter(|f| f.rule == "COMMENTEX-LINK").collect();
        assert_eq!(links.len(), 2, "{report:?}");
        assert!(links.iter().any(|f| f.part.as_deref() == Some("word/commentsExtended.xml") && f.message.contains("no paragraph")), "{report:?}");
        assert!(
            links.iter().any(|f| f.part.as_deref() == Some("word/gloss-commentsExtended.xml") && f.message.contains("paraIdParent")),
            "{report:?}"
        );
        assert!(
            !links.iter().any(|f| f.part.as_deref() == Some("word/gloss-commentsExtended.xml") && f.message.contains("no paragraph")),
            "{report:?}"
        );
    }
}

#[cfg(test)]
mod round5_completeness_tests {
    //! Absence predicates cover every source in their joined definition set.
    use super::*;
    use std::io::{Cursor, Write};

    /// Package synthetic parts in their supplied order, including typed nonstandard names.
    fn package(parts: Vec<(&str, &str, String)>, main_rels: &str) -> Vec<u8> {
        let relns = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let mut overrides = format!(r#"<Override PartName="/word/document.xml" ContentType="{}"/>"#, package::MAIN_CONTENT_TYPE);
        for (name, ct, _) in &parts {
            if !ct.is_empty() {
                overrides.push_str(&format!(r#"<Override PartName="/{name}" ContentType="{ct}"/>"#));
            }
        }
        let ct = format!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>{overrides}</Types>"#
        );
        let root = format!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rM" Type="{relns}/officeDocument" Target="word/document.xml"/></Relationships>"#
        );
        let rels = relationships(main_rels);
        let doc = format!(r#"<w:document xmlns:w="{NS_W}"><w:body><w:p/></w:body></w:document>"#);
        for (name, text) in [("[Content_Types].xml", ct), ("_rels/.rels", root), ("word/document.xml", doc), ("word/_rels/document.xml.rels", rels)]
            .into_iter()
            .chain(parts.into_iter().map(|(name, _, text)| (name, text)))
        {
            zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    /// Wrap owner-local relationships.
    fn relationships(children: &str) -> String {
        format!(r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{children}</Relationships>"#)
    }

    /// One internal relationship with a stable unique id per target.
    fn rel(kind: &str, target: &str) -> String {
        format!(
            r#"<Relationship Id="{target}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}" Target="{target}"/>"#
        )
    }

    /// Comment paragraphs use the same ids as the threading records.
    fn comments(ids: &[u32]) -> String {
        let paragraphs = ids.iter().map(|id| format!(r#"<w:p w14:paraId="{id:08X}"/>"#)).collect::<String>();
        format!(r#"<w:comments xmlns:w="{NS_W}" xmlns:w14="{NS_W14}"><w:comment w:id="1">{paragraphs}</w:comment></w:comments>"#)
    }

    /// The first glossary comments source is complete; the second, shared source loses id 3.
    fn shared(pooled: bool, loss: bool) -> Vec<u8> {
        let own_ids: &[u32] = if pooled { &[4] } else { &[] };
        let shared_ids: &[u32] = if !loss {
            &[1, 3]
        } else if pooled {
            &[1, 2, 3]
        } else {
            &[1, 2, 4, 3]
        };
        let gloss_rels = rel("comments", "gloss-comments.xml") + &rel("comments", "comments.xml") + &rel("commentsExtended", "gloss-ext.xml");
        package(
            vec![
                (
                    "word/glossary.xml",
                    package::GLOSSARY_CONTENT_TYPE,
                    format!(r#"<w:glossaryDocument xmlns:w="{NS_W}"><w:docParts/></w:glossaryDocument>"#),
                ),
                ("word/_rels/glossary.xml.rels", "", relationships(&gloss_rels)),
                ("word/gloss-comments.xml", package::COMMENTS_CONTENT_TYPE, comments(own_ids)),
                ("word/comments.xml", package::COMMENTS_CONTENT_TYPE, comments(shared_ids)),
                (
                    "word/gloss-ext.xml",
                    package::COMMENTS_EXTENDED_CONTENT_TYPE,
                    format!(
                        r#"<w15:commentsEx xmlns:w15="{NS_W15}"><w15:commentEx w15:paraId="00000003" w15:paraIdParent="00000008"/></w15:commentsEx>"#
                    ),
                ),
                (
                    "word/main-ext.xml",
                    package::COMMENTS_EXTENDED_CONTENT_TYPE,
                    format!(r#"<w15:commentsEx xmlns:w15="{NS_W15}"><w15:commentEx w15:paraId="00000009"/></w15:commentsEx>"#),
                ),
            ],
            &(rel("glossaryDocument", "glossary.xml") + &rel("comments", "comments.xml") + &rel("commentsExtended", "main-ext.xml")),
        )
    }

    /// Both loss paths must suppress glossary absence and preserve independent parent checks.
    fn assert_shared(pooled: bool) {
        let limits = Limits { facts: 3, ..Limits::default() };
        let control = validate_bytes_limited(&shared(pooled, false), limits);
        assert!(control.findings.iter().any(|f| f.part.as_deref() == Some("word/main-ext.xml") && f.message.contains("no paragraph")), "{control:?}");
        assert!(
            !control.findings.iter().any(|f| f.part.as_deref() == Some("word/gloss-ext.xml") && f.message.contains("no paragraph")),
            "{control:?}"
        );
        let report = validate_bytes_limited(&shared(pooled, true), limits);
        eprintln!("shared pooled={pooled}: {report:?}");
        assert_eq!(report.verdict, Verdict::Incomplete, "{report:?}");
        assert!(report.findings.iter().any(|f| f.part.as_deref() == Some("word/gloss-ext.xml") && f.message.contains("paraIdParent")), "{report:?}");
        assert!(!report.findings.iter().any(|f| f.message.contains("no paragraph")), "{report:?}");
        assert!(
            report
                .coverage_notes
                .iter()
                .any(|n| n.contains("comment-threading") && n.contains("word/glossary.xml") && n.contains("word/comments.xml")),
            "{report:?}"
        );
        assert_eq!(report.coverage_notes.iter().any(|n| n.contains("pooled comment paragraph IDs")), pooled, "{report:?}");
    }

    /// Pooled loss in the second contributing comments source.
    #[test]
    fn round5_shared_pooled_loss() {
        assert_shared(true);
    }

    /// Pure per-part loss in the second contributing comments source, with the first empty.
    #[test]
    fn round5_shared_per_part_loss() {
        assert_shared(false);
    }

    /// Definition caps cannot manufacture style-reference absence.
    #[test]
    fn round5_style_definition_cap() {
        let bytes = package(
            vec![
                (
                    "word/styles.xml",
                    package::STYLES_CONTENT_TYPE,
                    format!(r#"<w:styles xmlns:w="{NS_W}"><w:style w:styleId="A"/><w:style w:styleId="B"/></w:styles>"#),
                ),
                (
                    "word/header.xml",
                    package::HEADER_CONTENT_TYPE,
                    format!(r#"<w:hdr xmlns:w="{NS_W}"><w:p><w:pPr><w:pStyle w:val="B"/></w:pPr></w:p></w:hdr>"#),
                ),
            ],
            &(rel("styles", "styles.xml") + &rel("header", "header.xml")),
        );
        let report = validate_bytes_limited(&bytes, Limits { facts: 1, ..Limits::default() });
        eprintln!("style cap: {report:?}");
        assert!(!report.findings.iter().any(|f| f.rule == "REF-STYLE"), "{report:?}");
        assert!(!validate_bytes(&bytes).findings.iter().any(|f| f.rule == "REF-STYLE"));
    }

    /// Numbering caps must guard the abstract set and both story/style joins.
    #[test]
    fn round5_numbering_definition_cap() {
        let bytes = package(
            vec![
                (
                    "word/numbering.xml",
                    package::NUMBERING_CONTENT_TYPE,
                    format!(
                        r#"<w:numbering xmlns:w="{NS_W}"><w:abstractNum w:abstractNumId="1"/><w:abstractNum w:abstractNumId="2"/><w:num w:numId="1"><w:abstractNumId w:val="2"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="2"/></w:num></w:numbering>"#
                    ),
                ),
                (
                    "word/styles.xml",
                    package::STYLES_CONTENT_TYPE,
                    format!(
                        r#"<w:styles xmlns:w="{NS_W}"><w:style w:styleId="A"><w:pPr><w:numPr><w:numId w:val="2"/></w:numPr></w:pPr></w:style></w:styles>"#
                    ),
                ),
                (
                    "word/header.xml",
                    package::HEADER_CONTENT_TYPE,
                    format!(r#"<w:hdr xmlns:w="{NS_W}"><w:p><w:pPr><w:numPr><w:numId w:val="2"/></w:numPr></w:pPr></w:p></w:hdr>"#),
                ),
            ],
            &(rel("numbering", "numbering.xml") + &rel("styles", "styles.xml") + &rel("header", "header.xml")),
        );
        let report = validate_bytes_limited(&bytes, Limits { facts: 1, ..Limits::default() });
        eprintln!("numbering cap: {report:?}");
        assert!(!report.findings.iter().any(|f| f.rule == "NUM-DANGLING"), "{report:?}");
        assert!(!validate_bytes(&bytes).findings.iter().any(|f| f.rule == "NUM-DANGLING"));
    }

    /// Capped attached-schema definitions cannot establish an unattached reference.
    #[test]
    fn round5_attached_schema_cap() {
        let bytes = package(
            vec![
                (
                    "word/settings.xml",
                    package::SETTINGS_CONTENT_TYPE,
                    format!(r#"<w:settings xmlns:w="{NS_W}"><w:attachedSchema w:val="urn:A"/><w:attachedSchema w:val="urn:B"/></w:settings>"#),
                ),
                (
                    "word/props.xml",
                    package::CUSTOM_XML_PROPS_CONTENT_TYPE,
                    format!(
                        r#"<ds:datastoreItem xmlns:ds="{NS_DS}"><ds:schemaRefs><ds:schemaRef ds:uri="urn:B"/></ds:schemaRefs></ds:datastoreItem>"#
                    ),
                ),
            ],
            &(rel("settings", "settings.xml") + &rel("customXmlProps", "props.xml")),
        );
        let report = validate_bytes_limited(&bytes, Limits { facts: 1, ..Limits::default() });
        eprintln!("schema cap: {report:?}");
        assert!(!report.findings.iter().any(|f| f.rule == "CUSTOMXML-SCHEMA"), "{report:?}");
        assert!(!validate_bytes(&bytes).findings.iter().any(|f| f.rule == "CUSTOMXML-SCHEMA"));
    }

    /// Threading joins use each direction's definition completeness, including missing parse results.
    fn assert_threading_loss(mode: &str) {
        let ext = if mode == "ext-malformed" {
            "<".to_string()
        } else {
            let second = if mode == "ext-cap" { r#"<w15:commentEx w15:paraId="00000002"/>"# } else { "" };
            format!(
                r#"<w15:commentsEx xmlns:w15="{NS_W15}"><w15:commentEx w15:paraId="00000001" w15:paraIdParent="00000002"/>{second}</w15:commentsEx>"#
            )
        };
        let ids = if mode == "ids-cap" { vec![2, 1] } else { vec![2] };
        let ids = ids.into_iter().map(|id| format!(r#"<w16cid:commentId w16cid:paraId="{id:08X}"/>"#)).collect::<String>();
        let bytes = package(
            vec![
                ("word/comments.xml", package::COMMENTS_CONTENT_TYPE, comments(&[1])),
                ("word/ext.xml", package::COMMENTS_EXTENDED_CONTENT_TYPE, ext),
                (
                    "word/ids.xml",
                    package::COMMENTS_IDS_CONTENT_TYPE,
                    format!(r#"<w16cid:commentsIds xmlns:w16cid="{NS_W16CID}">{ids}</w16cid:commentsIds>"#),
                ),
            ],
            &(rel("comments", "comments.xml") + &rel("commentsExtended", "ext.xml") + &rel("commentsIds", "ids.xml")),
        );
        let report = validate_bytes_limited(&bytes, Limits { facts: 1, ..Limits::default() });
        eprintln!("threading {mode}: {report:?}");
        if mode != "ids-cap" {
            assert!(
                !report
                    .findings
                    .iter()
                    .any(|f| f.rule == "COMMENTEX-LINK" && (f.message.contains("paraIdParent") || f.message.contains("no commentEx counterpart"))),
                "{mode}: {report:?}"
            );
        } else {
            assert!(!report.findings.iter().any(|f| f.message.contains("no commentsIds paraId counterpart")), "{report:?}");
            assert!(report.findings.iter().any(|f| f.message.contains("paraIdParent")), "{report:?}");
        }
    }

    /// The parent and commentsIds checks cannot draw absence from capped commentEx definitions.
    #[test]
    fn round5_threading_ext_cap() {
        assert_threading_loss("ext-cap");
    }

    /// Only the check against capped commentsIds is suppressed; the complete parent set still warns.
    #[test]
    fn round5_threading_ids_cap() {
        assert_threading_loss("ids-cap");
    }

    /// A failed commentEx parse cannot be replaced with an empty definition set.
    #[test]
    fn round5_threading_ext_malformed() {
        assert_threading_loss("ext-malformed");
    }

    /// An auxiliary parser's EOF result cannot override the XML scan's incomplete source.
    #[test]
    fn round5_unclosed_definition_sources() {
        let mut manufactured = Vec::new();
        for kind in ["styles", "numbering", "settings", "ext"] {
            let (ct, text, ref_ct, reference, reference_kind, rule) = match kind {
                "styles" => (
                    package::STYLES_CONTENT_TYPE,
                    format!(r#"<w:styles xmlns:w="{NS_W}"><w:style w:styleId="A"/>"#),
                    package::HEADER_CONTENT_TYPE,
                    format!(r#"<w:hdr xmlns:w="{NS_W}"><w:p><w:pPr><w:pStyle w:val="B"/></w:pPr></w:p></w:hdr>"#),
                    "header",
                    "REF-STYLE",
                ),
                "numbering" => (
                    package::NUMBERING_CONTENT_TYPE,
                    format!(r#"<w:numbering xmlns:w="{NS_W}"><w:abstractNum w:abstractNumId="1"/>"#),
                    package::HEADER_CONTENT_TYPE,
                    format!(r#"<w:hdr xmlns:w="{NS_W}"><w:p><w:pPr><w:numPr><w:numId w:val="2"/></w:numPr></w:pPr></w:p></w:hdr>"#),
                    "header",
                    "NUM-DANGLING",
                ),
                "settings" => (
                    package::SETTINGS_CONTENT_TYPE,
                    format!(r#"<w:settings xmlns:w="{NS_W}"><w:attachedSchema w:val="urn:A"/>"#),
                    package::CUSTOM_XML_PROPS_CONTENT_TYPE,
                    format!(
                        r#"<ds:datastoreItem xmlns:ds="{NS_DS}"><ds:schemaRefs><ds:schemaRef ds:uri="urn:B"/></ds:schemaRefs></ds:datastoreItem>"#
                    ),
                    "customXmlProps",
                    "CUSTOMXML-SCHEMA",
                ),
                _ => (
                    package::COMMENTS_EXTENDED_CONTENT_TYPE,
                    format!(r#"<w15:commentsEx xmlns:w15="{NS_W15}"><w15:commentEx w15:paraId="00000001" w15:paraIdParent="00000002"/>"#),
                    package::COMMENTS_IDS_CONTENT_TYPE,
                    format!(r#"<w16cid:commentsIds xmlns:w16cid="{NS_W16CID}"><w16cid:commentId w16cid:paraId="00000002"/></w16cid:commentsIds>"#),
                    "commentsIds",
                    "COMMENTEX-LINK",
                ),
            };
            let bytes = package(
                vec![("word/defs.xml", ct, text), ("word/refs.xml", ref_ct, reference)],
                &(rel(if kind == "ext" { "commentsExtended" } else { kind }, "defs.xml") + &rel(reference_kind, "refs.xml")),
            );
            let report = validate_bytes(&bytes);
            eprintln!("unclosed {kind}: {report:?}");
            if report.findings.iter().any(|f| f.rule == rule) {
                manufactured.push(kind);
            }
            assert!(report.findings.iter().any(|f| f.rule == "XML-MALFORMED"), "{report:?}");
        }
        assert!(manufactured.is_empty(), "false absences from unclosed sources: {manufactured:?}");
    }

    /// A relationship's target can lie beyond the examined archive prefix.
    #[test]
    fn round5_archive_prefix_absence() {
        let bytes = package(
            vec![("word/header.xml", package::HEADER_CONTENT_TYPE, format!(r#"<w:hdr xmlns:w="{NS_W}"><w:p/></w:hdr>"#))],
            &rel("header", "header.xml"),
        );
        let report = validate_bytes_limited(&bytes, Limits { entries: 4, ..Limits::default() });
        eprintln!("archive prefix: {report:?}");
        assert!(!report.findings.iter().any(|f| f.rule == "REL-DANGLING"), "{report:?}");
        assert_eq!(report.verdict, Verdict::Incomplete, "{report:?}");
        assert_eq!(validate_bytes(&bytes).verdict, Verdict::Pass);
    }

    /// Unexamined declarations and owners cannot establish missing parts or relationship ids.
    #[test]
    fn round5_archive_prefix_declarations() {
        let mut failures = Vec::new();
        let root = relationships(&rel("officeDocument", "word/document.xml"));
        let doc = format!(r#"<w:document xmlns:w="{NS_W}"><w:body><w:p/></w:body></w:document>"#);
        let ct = format!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="{}"/></Types>"#,
            package::MAIN_CONTENT_TYPE
        );
        for (label, parts) in [
            ("manifest", vec![("_rels/.rels", root.as_str()), ("word/document.xml", doc.as_str()), ("[Content_Types].xml", ct.as_str())]),
            ("main", vec![("[Content_Types].xml", ct.as_str()), ("_rels/.rels", root.as_str()), ("word/document.xml", doc.as_str())]),
        ] {
            let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for (name, text) in parts {
                zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
                zip.write_all(text.as_bytes()).unwrap();
            }
            let report = validate_bytes_limited(&zip.finish().unwrap().into_inner(), Limits { entries: 2, ..Limits::default() });
            eprintln!("archive prefix {label}: {report:?}");
            if report.findings.iter().any(|f| matches!(f.rule, "PKG-NO-CONTENT-TYPES" | "PKG-NO-MAIN")) {
                failures.push(label);
            }
        }
        let bytes = package(
            vec![
                (
                    "word/header.xml",
                    package::HEADER_CONTENT_TYPE,
                    format!(r#"<w:hdr xmlns:w="{NS_W}" xmlns:r="{NS_R}"><w:p><w:r><w:object r:id="x"/></w:r></w:p></w:hdr>"#),
                ),
                ("word/pad.xml", "", "<pad/>".to_string()),
                ("word/_rels/header.xml.rels", "", relationships(r#"<Relationship Id="x" Type="urn:test" Target="pad.xml"/>"#)),
            ],
            &rel("header", "header.xml"),
        );
        let report = validate_bytes_limited(&bytes, Limits { entries: 6, ..Limits::default() });
        eprintln!("archive prefix relationships: {report:?}");
        if report.findings.iter().any(|f| f.rule == "REL-REF-MISSING") {
            failures.push("relationships");
        }
        assert!(failures.is_empty(), "false absences from archive prefixes: {failures:?}");
    }

    /// Repack one synthetic part while preserving archive ordering and all other payloads.
    fn replace_part(bytes: &[u8], target: &str, text: &str) -> Vec<u8> {
        use std::io::Read;
        let mut input = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut output = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for i in 0..input.len() {
            let mut entry = input.by_index(i).unwrap();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            output.start_file(entry.name(), zip::write::SimpleFileOptions::default()).unwrap();
            output.write_all(if entry.name() == target { text.as_bytes() } else { &data }).unwrap();
        }
        output.finish().unwrap().into_inner()
    }

    /// Unreadable or parser-failed second contributors have the same source-set requirements as caps.
    #[test]
    fn round5_shared_unavailable_source() {
        for text in [r#"<?xml version="1.0" encoding="X-unknown"?><comments/>"#, "<"] {
            let bytes = replace_part(&shared(true, false), "word/comments.xml", text);
            let report = validate_bytes(&bytes);
            eprintln!("shared unavailable {text}: {report:?}");
            assert!(!report.findings.iter().any(|f| f.rule == "COMMENTEX-LINK" && f.message.contains("no paragraph")), "{report:?}");
            assert!(report.findings.iter().any(|f| f.message.contains("paraIdParent")), "{report:?}");
            assert!(report.coverage_notes.iter().any(|n| n.contains("word/glossary.xml") && n.contains("word/comments.xml")), "{report:?}");
        }
    }

    /// A truncated consumption scan cannot establish that the customXml relationship is unused.
    #[test]
    fn round5_content_part_scan_stop() {
        let mut failures = Vec::new();
        for depth in [false, true] {
            let padding = if depth { "<x>".repeat(8) + &"</x>".repeat(8) } else { "<x/>".repeat(40) };
            let bytes = package(
                vec![
                    (
                        "word/header.xml",
                        package::HEADER_CONTENT_TYPE,
                        format!(
                            r#"<w:hdr xmlns:w="{NS_W}" xmlns:r="{NS_R}">{padding}<w:p><w:r><w:contentPart r:id="data.xml"/></w:r></w:p></w:hdr>"#
                        ),
                    ),
                    ("word/_rels/header.xml.rels", "", relationships(&rel("customXml", "data.xml"))),
                    ("word/data.xml", "text/xml", "<data/>".to_string()),
                ],
                &rel("header", "header.xml"),
            );
            let limits = if depth { Limits { depth: 6, ..Limits::default() } } else { Limits { events: 25, ..Limits::default() } };
            let report = validate_bytes_limited(&bytes, limits);
            eprintln!("contentPart depth={depth}: {report:?}");
            if report.findings.iter().any(|f| f.rule == "CT-MISMATCH") {
                failures.push(depth);
            }
            assert_eq!(validate_bytes(&bytes).verdict, Verdict::Pass);
        }
        assert!(failures.is_empty(), "false non-consumption from scan stops: depth={failures:?}");
    }

    /// The specialized header join needs the same missing-relationships guard as generic r:id joins.
    #[test]
    fn round5_header_archive_prefix() {
        let ct = format!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/word/header.xml" ContentType="{}"/></Types>"#,
            package::MAIN_CONTENT_TYPE,
            package::HEADER_CONTENT_TYPE
        );
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in [
            ("[Content_Types].xml", ct),
            ("_rels/.rels", relationships(&rel("officeDocument", "word/document.xml"))),
            (
                "word/document.xml",
                format!(
                    r#"<w:document xmlns:w="{NS_W}" xmlns:r="{NS_R}"><w:body><w:p/><w:sectPr><w:headerReference w:type="default" r:id="header.xml"/></w:sectPr></w:body></w:document>"#
                ),
            ),
            ("word/header.xml", format!(r#"<w:hdr xmlns:w="{NS_W}"><w:p/></w:hdr>"#)),
            ("word/_rels/document.xml.rels", relationships(&rel("header", "header.xml"))),
        ] {
            zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();
        let report = validate_bytes_limited(&bytes, Limits { entries: 4, ..Limits::default() });
        eprintln!("header archive prefix: {report:?}");
        assert!(!report.findings.iter().any(|f| f.rule == "REF-HEADER"), "{report:?}");
        assert_eq!(report.verdict, Verdict::Incomplete, "{report:?}");
        assert_eq!(validate_bytes(&bytes).verdict, Verdict::Pass);
    }

    /// Complete definition sources still expose genuine absences in every newly guarded family.
    #[test]
    fn round5_complete_definition_absence_controls() {
        let bytes = package(
            vec![
                (
                    "word/styles.xml",
                    package::STYLES_CONTENT_TYPE,
                    format!(
                        r#"<w:styles xmlns:w="{NS_W}"><w:style w:styleId="A"><w:pPr><w:numPr><w:numId w:val="9"/></w:numPr></w:pPr></w:style></w:styles>"#
                    ),
                ),
                (
                    "word/numbering.xml",
                    package::NUMBERING_CONTENT_TYPE,
                    format!(r#"<w:numbering xmlns:w="{NS_W}"><w:num w:numId="1"><w:abstractNumId w:val="9"/></w:num></w:numbering>"#),
                ),
                (
                    "word/header.xml",
                    package::HEADER_CONTENT_TYPE,
                    format!(
                        r#"<w:hdr xmlns:w="{NS_W}"><w:p><w:pPr><w:pStyle w:val="B"/><w:numPr><w:numId w:val="9"/></w:numPr></w:pPr></w:p></w:hdr>"#
                    ),
                ),
                (
                    "word/settings.xml",
                    package::SETTINGS_CONTENT_TYPE,
                    format!(r#"<w:settings xmlns:w="{NS_W}"><w:attachedSchema w:val="urn:A"/></w:settings>"#),
                ),
                (
                    "word/props.xml",
                    package::CUSTOM_XML_PROPS_CONTENT_TYPE,
                    format!(
                        r#"<ds:datastoreItem xmlns:ds="{NS_DS}"><ds:schemaRefs><ds:schemaRef ds:uri="urn:B"/></ds:schemaRefs></ds:datastoreItem>"#
                    ),
                ),
            ],
            &(rel("styles", "styles.xml")
                + &rel("numbering", "numbering.xml")
                + &rel("header", "header.xml")
                + &rel("settings", "settings.xml")
                + &rel("customXmlProps", "props.xml")),
        );
        let report = validate_bytes_limited(&bytes, Limits { facts: 1, ..Limits::default() });
        assert!(report.findings.iter().any(|f| f.rule == "REF-STYLE"), "{report:?}");
        for part in ["word/header.xml", "word/styles.xml", "word/numbering.xml"] {
            assert!(report.findings.iter().any(|f| f.rule == "NUM-DANGLING" && f.part.as_deref() == Some(part)), "{report:?}");
        }
        assert!(report.findings.iter().any(|f| f.rule == "CUSTOMXML-SCHEMA"), "{report:?}");
    }

    /// An unexamined content-type declaration cannot establish that an owner is non-XML.
    #[test]
    fn round5_content_part_unknown_owner_type() {
        let bytes = package(
            vec![
                (
                    "word/extra.dat",
                    "application/xml",
                    format!(r#"<w:root xmlns:w="{NS_W}" xmlns:r="{NS_R}"><w:contentPart r:id="data.xml"/></w:root>"#),
                ),
                ("word/_rels/extra.dat.rels", "", relationships(&rel("customXml", "data.xml"))),
                ("word/data.xml", "text/xml", "<data/>".to_string()),
            ],
            &rel("unknown", "extra.dat"),
        );
        let padding = (0..10).map(|i| format!(r#"<Default Extension="unused{i}" ContentType="application/xml"/>"#)).collect::<String>();
        let ct = format!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/word/document.xml" ContentType="{}"/><Override PartName="/word/data.xml" ContentType="text/xml"/>{padding}<Override PartName="/word/extra.dat" ContentType="application/xml"/></Types>"#,
            package::MAIN_CONTENT_TYPE
        );
        let report = validate_bytes_limited(&replace_part(&bytes, "[Content_Types].xml", &ct), Limits { entries: 10, ..Limits::default() });
        eprintln!("unknown contentPart owner type: {report:?}");
        assert!(!report.findings.iter().any(|f| f.rule == "CT-MISMATCH"), "{report:?}");
        assert_eq!(validate_bytes(&bytes).verdict, Verdict::Pass);
    }
}
