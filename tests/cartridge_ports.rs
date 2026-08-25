//! L4 evals (CARTRIDGE_PLAN §11 item 6): wiring a set of cartridges by
//! ports — pure graph rules pinned with manifests alone, then a real
//! composition of rustlite cartridges booted in wiring order and addressed
//! by port. negative controls carry the product: every refusal names the
//! port, the slugs, or the loop.

mod common;

use common::{compile, manifest, manifest_with, shift_src, FakeHost};
use vanish::cartridges::{wire, CallError, ComposeError, Composition, Edge, WireError};

// ---- pure wiring ---------------------------------------------------------------

#[test]
fn a_chain_wires_providers_first() {
    // a requires x ← b provides x, requires y ← c provides y.
    let set = [
        manifest_with("a", &[], &["x"]),
        manifest_with("b", &["x"], &["y"]),
        manifest_with("c", &["y"], &[]),
    ];
    let w = wire(&set).unwrap();
    assert_eq!(w.order, vec!["c", "b", "a"], "every provider before its requirer");
    assert_eq!(w.providers["x"], "b");
    assert_eq!(w.providers["y"], "c");
    assert_eq!(
        w.edges,
        vec![
            Edge { requirer: "a".into(), port: "x".into(), provider: "b".into() },
            Edge { requirer: "b".into(), port: "y".into(), provider: "c".into() },
        ]
    );
}

#[test]
fn a_diamond_orders_deterministically() {
    // a requires x,y; b provides x requires z; c provides y requires z;
    // d provides z. d first, a last; b before c because ties break by slug.
    let set = [
        manifest_with("c", &["y"], &["z"]),
        manifest_with("a", &[], &["x", "y"]),
        manifest_with("d", &["z"], &[]),
        manifest_with("b", &["x"], &["z"]),
    ];
    let w = wire(&set).unwrap();
    assert_eq!(w.order, vec!["d", "b", "c", "a"]);
    // the same set in another order wires identically — manifest order
    // must not leak into boot order.
    let shuffled = [set[3].clone(), set[0].clone(), set[1].clone(), set[2].clone()];
    assert_eq!(wire(&shuffled).unwrap().order, w.order);
}

#[test]
fn independent_cartridges_and_unrequired_ports_are_fine() {
    let set = [
        manifest_with("solo", &["unused"], &[]),
        manifest("plain"),
    ];
    let w = wire(&set).unwrap();
    assert_eq!(w.order, vec!["plain", "solo"]);
    assert!(w.edges.is_empty());
    assert_eq!(w.providers["unused"], "solo");
    assert_eq!(wire(&[]).unwrap(), Default::default(), "an empty set wires to nothing");
}

#[test]
fn a_missing_provider_names_the_port_and_every_requirer() {
    let set = [
        manifest_with("a", &[], &["vision"]),
        manifest_with("b", &[], &["vision"]),
        manifest_with("c", &["audio"], &["audio-x"]),
    ];
    let err = wire(&set).unwrap_err();
    // the first missing port BY NAME is reported, with all its requirers.
    assert_eq!(
        err,
        WireError::MissingProvider {
            port: "audio-x".into(),
            required_by: vec!["c".into()],
        }
    );
    let set = [manifest_with("a", &[], &["vision"]), manifest_with("b", &[], &["vision"])];
    let err = wire(&set).unwrap_err();
    assert_eq!(
        err,
        WireError::MissingProvider {
            port: "vision".into(),
            required_by: vec!["a".into(), "b".into()],
        }
    );
    let text = err.to_string();
    assert!(text.contains("'vision'") && text.contains("'a', 'b'") && text.contains("provides"), "{text}");
}

#[test]
fn two_providers_for_one_port_is_refused_not_resolved() {
    let set = [
        manifest_with("fast", &["reasoning"], &[]),
        manifest_with("slow", &["reasoning"], &[]),
        manifest_with("user", &[], &["reasoning"]),
    ];
    let err = wire(&set).unwrap_err();
    assert_eq!(
        err,
        WireError::AmbiguousProvider {
            port: "reasoning".into(),
            providers: vec!["fast".into(), "slow".into()],
        }
    );
    assert!(err.to_string().contains("exactly one provider"), "{err}");
}

#[test]
fn duplicate_slugs_and_bad_manifests_are_refused_first() {
    let set = [manifest("twin"), manifest("twin")];
    assert_eq!(wire(&set).unwrap_err(), WireError::DuplicateSlug("twin".into()));
    let mut bad = manifest("Bad Slug");
    bad.provides = vec![];
    let err = wire(&[manifest("ok"), bad]).unwrap_err();
    assert!(
        matches!(err, WireError::Manifest { ref slug, ref reason } if slug == "Bad Slug" && reason.contains("slug")),
        "{err:?}"
    );
}

#[test]
fn cycles_are_refused_with_the_loop_written_out() {
    // a → b → a
    let set = [
        manifest_with("a", &["pa"], &["pb"]),
        manifest_with("b", &["pb"], &["pa"]),
    ];
    let err = wire(&set).unwrap_err();
    assert_eq!(err, WireError::Cycle(vec!["a".into(), "b".into(), "a".into()]));
    assert!(err.to_string().contains("a → b → a"), "{err}");

    // a three-cycle reached through a non-cyclic head: head → x → y → z → x.
    let set = [
        manifest_with("head", &[], &["px"]),
        manifest_with("x", &["px"], &["py"]),
        manifest_with("y", &["py"], &["pz"]),
        manifest_with("z", &["pz"], &["px"]),
    ];
    let err = wire(&set).unwrap_err();
    let WireError::Cycle(path) = err else { panic!("{err:?}") };
    assert_eq!(path.first(), path.last(), "the path is closed");
    assert!(!path.contains(&"head".to_string()), "the acyclic head is not in the loop: {path:?}");
    assert_eq!(path.len(), 4, "x → y → z → x: {path:?}");

    // a self-cycle dies in the manifest, before wiring ever sees it.
    let err = wire(&[manifest_with("me", &["p"], &["p"])]).unwrap_err();
    assert!(matches!(err, WireError::Manifest { .. }), "{err:?}");
}

// ---- composition of real cartridges ---------------------------------------------

/// `inc` provides "inc" (byte + 1); `dec` provides "dec" and requires "inc"
/// (byte − 1). boot order must be inc then dec.
fn two_cartridges() -> Vec<(vanish::cartridges::CartridgeManifest, Vec<u8>)> {
    vec![
        (manifest_with("dec", &["dec"], &["inc"]), compile(&shift_src(-1))),
        (manifest_with("inc", &["inc"], &[]), compile(&shift_src(1))),
    ]
}

#[test]
fn a_composition_boots_in_wiring_order_and_routes_by_port() {
    let mut comp = Composition::load(&two_cartridges(), |_| FakeHost::default()).unwrap();
    assert_eq!(comp.wiring().order, vec!["inc", "dec"]);
    assert_eq!(comp.provider_of("dec"), Some("dec"));
    assert_eq!(comp.provider_of("inc"), Some("inc"));
    assert_eq!(comp.slugs().collect::<Vec<_>>(), vec!["dec", "inc"]);

    let booted = comp
        .init_all(&|slug| format!("cfg:{slug}").into_bytes(), 10_000)
        .unwrap();
    assert_eq!(booted, vec!["inc", "dec"]);
    // each member saw ITS config through ITS host — no cross-talk.
    assert_eq!(comp.get("inc").unwrap().host().logs, vec![(1, b"cfg:inc".to_vec())]);
    assert_eq!(comp.get("dec").unwrap().host().logs, vec![(1, b"cfg:dec".to_vec())]);

    // callers name a capability, never a module.
    assert_eq!(comp.handle_port("inc", b"abc", 10_000).unwrap(), b"bcd");
    assert_eq!(comp.handle_port("dec", b"bcd", 10_000).unwrap(), b"abc");
    assert_eq!(comp.handle("inc", b"HAL", 10_000).unwrap(), b"IBM");
}

#[test]
fn unknown_ports_and_slugs_are_refused_named() {
    let mut comp = Composition::load(&two_cartridges(), |_| FakeHost::default()).unwrap();
    comp.init_all(&|_| vec![], 10_000).unwrap();
    assert_eq!(
        comp.handle_port("vision", b"x", 1000).unwrap_err(),
        ComposeError::NoProvider("vision".into())
    );
    assert_eq!(
        comp.handle("ghost", b"x", 1000).unwrap_err(),
        ComposeError::UnknownCartridge("ghost".into())
    );
    assert!(comp.get("ghost").is_none());
}

#[test]
fn a_miswired_set_is_refused_before_any_cartridge_is_loaded() {
    // the second entry's bytes are garbage, but the WIRING is wrong too
    // (a cycle) — and wiring is checked first, so the refusal is the
    // cycle, proving no bytes were decoded and no memory instantiated.
    let entries = vec![
        (manifest_with("a", &["pa"], &["pb"]), compile(&shift_src(1))),
        (manifest_with("b", &["pb"], &["pa"]), b"not wasm".to_vec()),
    ];
    let err = Composition::load(&entries, |_| FakeHost::default()).unwrap_err();
    assert!(matches!(err, ComposeError::Wire(WireError::Cycle(_))), "{err:?}");

    // with the wiring right, the bad bytes ARE the refusal, naming the slug.
    let entries = vec![
        (manifest_with("a", &["pa"], &[]), compile(&shift_src(1))),
        (manifest_with("b", &["pb"], &["pa"]), b"not wasm".to_vec()),
    ];
    let err = Composition::load(&entries, |_| FakeHost::default()).unwrap_err();
    assert!(matches!(err, ComposeError::Load { ref slug, .. } if slug == "b"), "{err:?}");
    assert!(err.to_string().contains("'b'") && err.to_string().contains("magic"), "{err}");
}

#[test]
fn init_stops_at_the_first_refusal_naming_the_slug() {
    // `refuser` provides "r" and refuses its config; `user` requires "r",
    // so it boots AFTER refuser — and therefore never boots at all.
    let refuser = format!(
        "{} pub fn cart_init(p: i32, n: i32) -> i32 {{ return 7; }} \
         pub fn cart_handle(p: i32, n: i32) -> i64 {{ return 0; }}",
        common::ALLOC
    );
    let entries = vec![
        (manifest_with("user", &[], &["r"]), compile(&shift_src(1))),
        (manifest_with("refuser", &["r"], &[]), compile(&refuser)),
        (manifest_with("early", &[], &[]), compile(&shift_src(1))),
    ];
    let mut comp = Composition::load(&entries, |_| FakeHost::default()).unwrap();
    assert_eq!(comp.wiring().order, vec!["early", "refuser", "user"]);
    let err = comp.init_all(&|_| vec![], 10_000).unwrap_err();
    assert_eq!(
        err,
        ComposeError::Call {
            slug: "refuser".into(),
            error: CallError::Refused(7),
        }
    );
    assert!(comp.get("early").unwrap().is_initialized(), "boots before the refusal stay up");
    assert!(!comp.get("refuser").unwrap().is_initialized());
    assert!(!comp.get("user").unwrap().is_initialized(), "nothing after the refusal booted");
    // and the un-booted member refuses messages, as the lifecycle promises.
    assert_eq!(
        comp.handle_port("r", b"x", 1000).unwrap_err(),
        ComposeError::Call {
            slug: "refuser".into(),
            error: CallError::NotInitialized,
        }
    );
}
