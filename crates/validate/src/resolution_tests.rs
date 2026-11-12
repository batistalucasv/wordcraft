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
