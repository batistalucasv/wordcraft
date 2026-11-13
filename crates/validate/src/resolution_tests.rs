//! Resolution controls exercise discovery states independently of report gates.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
#[allow(dead_code, clippy::duplicate_mod)]
#[path = "../tests/common/mod.rs"]
pub(super) mod common;
use common::{CT_STYLES, Doc, NS};

#[test]
fn empty_incomplete_discovery_is_unknown() {
    let mut pkg = package::load(&Doc::minimal().build(), &mut Acc::with_limits(Limits::default())).unwrap();
    assert!(matches!(owner_rel_part(&pkg, "word/document.xml", "styles"), Resolution::Absent));
    pkg.entries_truncated = true;
    assert!(matches!(owner_rel_part(&pkg, "word/document.xml", "styles"), Resolution::Unknown(_)));
    assert!(matches!(find_by_type(&pkg, CT_STYLES), Resolution::Unknown(_)));
    pkg.entries_truncated = false;
    pkg.identity_incomplete = true;
    assert!(matches!(owner_rel_part(&pkg, "word/document.xml", "styles"), Resolution::Unknown(_)));
    assert!(matches!(find_by_type(&pkg, CT_STYLES), Resolution::Unknown(_)));
}

#[test]
fn content_type_singletons_never_choose_by_zip_order() {
    for reverse in [false, true] {
        let mut d = Doc::minimal();
        for name in ["word/a.xml", "word/b.xml"] {
            d.add_with_override(name, CT_STYLES, format!(r#"<w:styles {NS}/>"#));
        }
        if reverse {
            d.entries.reverse();
        }
        let pkg = package::load(&d.build(), &mut Acc::with_limits(Limits::default())).unwrap();
        assert!(matches!(find_by_type(&pkg, CT_STYLES), Resolution::Ambiguous(_)));
    }
}

#[test]
fn unreadable_root_discovery_never_establishes_absence() {
    let mut d = Doc::minimal();
    d.set("_rels/.rels", vec![0xff]);
    let pkg = package::load(&d.build(), &mut Acc::with_limits(Limits::default())).unwrap();
    assert!(matches!(pkg.main, Resolution::Unknown(_)));
    assert!(matches!(package::entry_point_profile(&pkg), Resolution::Unknown(_)));
}

/// R18/45: identity loss never erases a found prefix or establishes missing identity.
#[test]
fn round18_entry_lookup_truth_table() {
    for reverse in [false, true] {
        let mut d = Doc::minimal();
        d.add_with_override("word/Header.XML", common::CT_HEADER, format!(r#"<w:hdr {NS}><w:p/></w:hdr>"#));
        d.set_doc_rels(
            r#"<Relationship Id="h" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="Header.XML"/>"#,
        );
        if reverse {
            d.entries.reverse();
        }
        for (truncated, identity) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut pkg = package::load(&d.build(), &mut Acc::with_limits(Limits::default())).unwrap();
            pkg.entries_truncated = truncated;
            pkg.identity_incomplete = identity;
            assert!(matches!(canonical_entry(&pkg, "/WORD/header.xml"), Resolution::One(n) if n == "word/Header.XML"));
            assert!(matches!(entry_by_name(&pkg, "/WORD/header.xml"), Resolution::One(e) if e.name == "word/Header.XML"));
            assert!(matches!(package::content_unavailable(&pkg, "word/Header.XML"), Resolution::One(false)));
            let missing = canonical_entry(&pkg, "word/missing.xml");
            let unavailable = package::content_unavailable(&pkg, "word/missing.xml");
            if truncated || identity {
                assert!(matches!(missing, Resolution::Unknown(_)));
                assert!(matches!(unavailable, Resolution::Unknown(_)));
            } else {
                assert!(matches!(missing, Resolution::Absent));
                assert!(matches!(unavailable, Resolution::Absent));
            }
            // Positive role classification survives a truncated prefix.
            assert_eq!(story_roles(&pkg).get("word/header.xml"), Some(&scan::PartRole::Story));
            let mut acc = Acc::with_limits(Limits::default());
            let xml = check_xml(&pkg, &mut acc);
            check_rels(&pkg, &xml, &mut acc);
            assert!(!acc.finish().findings.iter().any(|f| f.rule == "REL-DANGLING"));
            pkg.entries.retain(|e| !e.name.eq_ignore_ascii_case("word/Header.XML"));
            let mut acc = Acc::with_limits(Limits::default());
            let xml = check_xml(&pkg, &mut acc);
            check_rels(&pkg, &xml, &mut acc);
            assert_eq!(acc.finish().findings.iter().any(|f| f.rule == "REL-DANGLING"), !truncated && !identity);
        }
    }
}

/// R18/32: the retained whole-source gates are conservative and necessary independently.
#[test]
fn round18_discovery_loss_polarity() {
    for (truncated, identity) in [(false, false), (true, false), (false, true)] {
        let mut pkg = package::load(&Doc::minimal().build(), &mut Acc::with_limits(Limits::default())).unwrap();
        pkg.entries_truncated = truncated;
        pkg.identity_incomplete = identity;
        assert_eq!(discovery_unavailable(&pkg, None), truncated || identity);
        assert_eq!(discovery_unavailable(&pkg, Some("word/document.xml")), truncated || identity);
        assert_eq!(ownership_discovery_complete(&pkg), !truncated && !identity);
    }
}

/// R18/46: each availability consumer has separate readable, unavailable, missing and unknown controls.
#[test]
fn round18_content_availability_sites() {
    for reverse in [false, true] {
        for state in ["readable", "unavailable", "absent", "unknown"] {
            let mut d = Doc::minimal();
            d.set_document(r#"<w:p><w:r><w:drawing><a:blip xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" r:embed="h"/></w:drawing></w:r></w:p>"#);
            d.set_doc_rels(r#"<Relationship Id="h" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="image.xml"/><Relationship Id="header" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="image.xml"/>"#);
            d.add_with_override("word/image.xml", common::CT_STYLES, format!(r#"<w:styles {NS}/>"#));
            if reverse {
                d.entries.reverse();
            }
            let mut pkg = package::load(&d.build(), &mut Acc::with_limits(Limits::default())).unwrap();
            match state {
                "unavailable" => {
                    let e = pkg.entries.iter_mut().find(|e| e.name == "[Content_Types].xml").unwrap();
                    e.content_unavailable = true;
                    e.decoded = None;
                    pkg.content_types.incomplete = true;
                }
                "absent" | "unknown" => {
                    pkg.entries.retain(|e| e.name != "[Content_Types].xml");
                    pkg.content_types_present = false;
                    pkg.entries_truncated = state == "unknown";
                    pkg.content_types.incomplete = state == "unknown";
                }
                _ => {}
            }
            let unavailable = package::content_unavailable(&pkg, "[Content_Types].xml");
            match state {
                "readable" => assert!(matches!(unavailable, Resolution::One(false))),
                "unavailable" => assert!(matches!(unavailable, Resolution::One(true))),
                "absent" => assert!(matches!(unavailable, Resolution::Absent)),
                _ => assert!(matches!(unavailable, Resolution::Unknown(_))),
            }
            let skip = matches!(state, "unavailable" | "unknown");
            assert_eq!(discovery_unavailable(&pkg, None), skip, "2756/{state}");
            assert_eq!(ownership_discovery_complete(&pkg), !skip, "2780/{state}");
            let mut acc = Acc::with_limits(Limits::default());
            check_content_types(&pkg, &mut acc);
            assert_eq!(acc.checks.contains(&"CT-UNDECLARED"), state == "readable", "905/{state}");
            let xml = check_xml(&pkg, &mut acc);
            let mut acc = Acc::with_limits(Limits::default());
            check_typed_image_refs(&pkg, &xml.references(), &mut acc);
            assert_eq!(acc.findings.iter().any(|f| f.rule == "REL-TYPED"), !skip, "2034/{state}");
            let mut acc = Acc::with_limits(Limits::default());
            check_typed_targets(&pkg, &mut acc);
            assert_eq!(acc.findings.iter().any(|f| f.rule == "REL-TYPED"), !skip, "2097/{state}");
            // Site 3650's manifest-driven note belongs to the explicit no-owner fallback.
            pkg.main = Resolution::Absent;
            pkg.glossary = Resolution::Absent;
            let mut acc = Acc::with_limits(Limits::default());
            let xml = check_xml(&pkg, &mut acc);
            let mut acc = Acc::with_limits(Limits::default());
            check_custom_xml(&pkg, &xml, &mut acc);
            assert!(!acc.findings.iter().any(|f| f.rule == "CUSTOMXML-SCHEMA"), "3650/{state}");
            assert_eq!(
                acc.coverage.iter().any(|n| n.contains("custom XML schema references were not checked")),
                state == "unavailable",
                "3650 informational/{state}"
            );
        }
    }
}
