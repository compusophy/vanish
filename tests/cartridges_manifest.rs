//! L1 manifest evals: every refusal pinned, both directions. the loader
//! will trust these verdicts to keep malformed cartridges out of the
//! runtime, so the negative controls ARE the product.

use vanish::cartridges::{CartridgeKind, CartridgeManifest, ABI_VERSION};

fn good() -> CartridgeManifest {
    CartridgeManifest {
        slug: "reasoner-core".to_string(),
        kind: CartridgeKind::Cognitive,
        version: "0.1.0".to_string(),
        abi_version: ABI_VERSION,
        provides: vec![port("reasoning")],
        requires: vec![],
    }
}

fn port(name: &str) -> vanish::cartridges::manifest::Port {
    vanish::cartridges::manifest::Port {
        name: name.to_string(),
    }
}

#[test]
fn a_good_manifest_parses_and_validates() {
    let json = serde_json::to_string(&good()).unwrap();
    let m = CartridgeManifest::parse(&json).unwrap();
    assert_eq!(m.slug, "reasoner-core");
    assert_eq!(m.kind, CartridgeKind::Cognitive);
}

// ---- slug boundary ------------------------------------------------------------

#[test]
fn slug_rules_are_exact() {
    use vanish::cartridges::manifest::valid_slug;
    assert!(valid_slug("a"));
    assert!(valid_slug("reasoner-core"));
    assert!(valid_slug("v2-fast-9"));
    // refusals:
    assert!(!valid_slug(""), "empty");
    assert!(!valid_slug("-lead"), "must start alphanumeric");
    assert!(!valid_slug("trail-"), "must end alphanumeric");
    assert!(!valid_slug("Has-Caps"), "lowercase only");
    assert!(!valid_slug("has space"), "no spaces");
    assert!(!valid_slug("under_score"), "hyphens only");
    assert!(!valid_slug("a--b"), "no doubled hyphens (kv namespace safety)");
    assert!(!valid_slug(&"x".repeat(65)), "64 char cap");
    assert!(valid_slug(&"x".repeat(64)), "64 chars allowed");
}

// ---- abi gate -----------------------------------------------------------------

#[test]
fn future_abi_versions_are_refused_at_the_door() {
    let mut m = good();
    m.abi_version = ABI_VERSION + 1;
    let err = m.validate().unwrap_err();
    assert!(err.contains("newer than this runtime"), "{err}");
    // older majors still load by design — old cartridges never break.
    m.abi_version = 1;
    assert!(m.validate().is_ok());
}

// ---- port rules ----------------------------------------------------------------

#[test]
fn duplicate_provides_and_self_cycles_are_refused() {
    let mut m = good();
    m.provides.push(port("reasoning"));
    let err = m.validate().unwrap_err();
    assert!(err.contains("provided twice"), "{err}");

    let mut m2 = good();
    m2.requires.push(port("reasoning")); // also provided above
    let err = m2.validate().unwrap_err();
    assert!(err.contains("both provided and required"), "{err}");
}

#[test]
fn empty_port_names_and_versions_are_refused() {
    let mut m = good();
    m.provides.push(port("  "));
    assert!(m.validate().unwrap_err().contains("empty name"));

    let mut m2 = good();
    m2.version = "  ".to_string();
    assert!(m2.validate().unwrap_err().contains("version"));
}

#[test]
fn corrupt_json_is_refused_with_the_parse_reason() {
    let err = CartridgeManifest::parse("{ not json }").unwrap_err();
    assert!(err.contains("bad manifest json"), "{err}");
}
