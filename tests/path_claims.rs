//! path-claim registry evals (STACKED_PRS_PLAN §2 C1).
//!
//! the claim registry is coordination infrastructure for concurrent agents:
//! its whole job is to make a collision VISIBLE before it becomes a merge
//! conflict. a registry that silently swallows a contest (or invents one) is
//! worse than no registry — so every decision here carries its negative
//! control: contested paths assert as directly as clear ones.

use vanish::agent::claims::{
    contest_warning, registry_claim, registry_entries, registry_release_conversation,
    registry_release_paths, ClaimRegistry, ClaimVerdict, CLAIM_TTL_MS,
};

// ---- verdicts -------------------------------------------------------------

#[test]
fn an_unclaimed_path_is_clear() {
    let mut r = ClaimRegistry::new();
    let v = r.claim("src/agent/mod.rs", "conv-a", 1_000);
    assert_eq!(v, ClaimVerdict::Clear);
}

#[test]
fn a_second_conversation_on_the_same_path_is_contested() {
    let mut r = ClaimRegistry::new();
    r.claim("src/lib.rs", "conv-a", 1_000);
    // conv-b arrives on the same path: this is THE case the registry exists
    // for, and it must be loud.
    match r.claim("src/lib.rs", "conv-b", 2_000) {
        ClaimVerdict::Contested { holder } => assert_eq!(holder, "conv-a"),
        other => panic!("expected Contested, got {other:?}"),
    }
}

#[test]
fn the_same_conversation_revisiting_is_not_a_conflict() {
    let mut r = ClaimRegistry::new();
    r.claim("src/lib.rs", "conv-a", 1_000);
    // conv-a continues working the same file across steps: that is normal
    // iteration, not a collision. treating it as one would spam every run
    // with false warnings about its own work.
    assert!(!r.claim("src/lib.rs", "conv-a", 2_000).is_contested());
}

#[test]
fn an_expired_claim_does_not_contest() {
    let mut r = ClaimRegistry::new();
    r.claim("src/lib.rs", "conv-a", 1_000);
    // just inside the ttl: still contested. (check, not claim — a claim here
    // would REPLACE conv-a's entry with conv-b's and poison the next line.)
    assert!(
        r.check("src/lib.rs", "conv-b", 1_000 + CLAIM_TTL_MS).is_contested()
    );
    // past it: conv-a has been silent half an hour; the path is free again.
    let v = r.check("src/lib.rs", "conv-b", 1_000 + CLAIM_TTL_MS + 1);
    assert_eq!(v, ClaimVerdict::Clear);
}

#[test]
fn expiry_never_reads_negative_time_as_fresh() {
    let mut r = ClaimRegistry::new();
    // a backwards clock must read as "very old", never negative-fresh.
    r.claim("src/lib.rs", "conv-a", 10_000);
    let v = r.claim("src/lib.rs", "conv-b", 5_000);
    assert!(
        v.is_contested(),
        "a backwards clock must not launder an old claim into freshness"
    );
}

#[test]
fn check_reports_without_recording() {
    let mut r = ClaimRegistry::new();
    r.claim("a.rs", "conv-a", 1_000);
    // a pure read by a third party must not take over the claim.
    let _ = r.check("a.rs", "conv-c", 2_000);
    assert_eq!(r.entries(), vec![("a.rs".to_string(), "conv-a".to_string())]);
}

// ---- lifecycle --------------------------------------------------------------

#[test]
fn release_conversation_drops_only_its_own_claims() {
    let mut r = ClaimRegistry::new();
    r.claim("a.rs", "conv-a", 1_000);
    r.claim("b.rs", "conv-a", 1_000);
    r.claim("c.rs", "conv-b", 1_000);

    let mut released = r.release_conversation("conv-a");
    released.sort();
    assert_eq!(released, vec!["a.rs".to_string(), "b.rs".to_string()]);
    // conv-b's claim survives — its run is still live.
    assert_eq!(r.entries(), vec![("c.rs".to_string(), "conv-b".to_string())]);
}

#[test]
fn release_paths_drops_by_name_regardless_of_owner() {
    let mut r = ClaimRegistry::new();
    r.claim("a.rs", "conv-a", 1_000);
    r.claim("b.rs", "conv-b", 1_000);

    let dropped = r.release_paths(&["a.rs".to_string()]);
    assert_eq!(dropped, 1);
    assert_eq!(r.entries(), vec![("b.rs".to_string(), "conv-b".to_string())]);
    // dropping a path nobody claimed reports zero, not an error.
    assert_eq!(r.release_paths(&["ghost.rs".to_string()]), 0);
}

#[test]
fn expire_sweeps_only_stale_entries_and_returns_them() {
    let mut r = ClaimRegistry::new();
    r.claim("old.rs", "conv-a", 0);
    r.claim("fresh.rs", "conv-b", CLAIM_TTL_MS - 1_000);

    let dead = r.expire(CLAIM_TTL_MS + 1_000);
    assert_eq!(dead, vec!["old.rs".to_string()]);
    assert_eq!(r.entries(), vec![("fresh.rs".to_string(), "conv-b".to_string())]);
}

// ---- session-level accessors -------------------------------------------------
//
// these go through the thread_local the tools actually use; each test cleans
// up after itself because the registry is process-global.

#[test]
fn session_registry_round_trip_and_cleanup() {
    let t = now();
    assert!(!registry_claim("x1.rs", "conv-x", t).is_contested());
    // second conversation contests through the SAME global surface...
    match registry_claim("x1.rs", "conv-y", t) {
        ClaimVerdict::Contested { holder } => assert_eq!(holder, "conv-x"),
        other => panic!("expected Contested, got {other:?}"),
    }
    // ...and its claim now owns the path (last writer wins).
    match registry_claim("x1.rs", "conv-z", t) {
        ClaimVerdict::Contested { holder } => assert_eq!(holder, "conv-y"),
        other => panic!("expected Contested, got {other:?}"),
    }
    // conv-x also holds a second path untouched since.
    registry_claim("x2.rs", "conv-x", t);
    assert_eq!(
        registry_release_conversation("conv-x"),
        vec!["x2.rs".to_string()]
    );
    // conv-y was overtaken on its only path — nothing left to release.
    assert!(registry_release_conversation("conv-y").is_empty());
    assert_eq!(
        registry_release_conversation("conv-z"),
        vec!["x1.rs".to_string()]
    );
    // after all releases the global surface holds nothing from this test.
    assert!(registry_entries().iter().all(|(p, _)| {
        p != "x1.rs" && p != "x2.rs" && p != "committed.rs"
    }));
}

#[test]
fn committed_paths_are_released_through_the_session_surface() {
    registry_claim("committed.rs", "conv-z", now());
    assert_eq!(registry_release_paths(&["committed.rs".to_string()]), 1);
    // releasing again finds nothing: the path is already gone.
    assert_eq!(registry_release_paths(&["committed.rs".to_string()]), 0);
}

// ---- warning text ------------------------------------------------------------

#[test]
fn the_contest_warning_names_both_path_and_holder() {
    let w = contest_warning("src/agent/mod.rs", "conv-9");
    assert!(w.contains("src/agent/mod.rs"), "{w}");
    assert!(w.contains("conv-9"), "{w}");
    // D9 for warnings: say what to do about it.
    assert!(w.contains("merge conflict") || w.contains("coordinate"), "{w}");
}

// ---- clock --------------------------------------------------------------------

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
