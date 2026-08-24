//! guards the guard: the verification gate must stay SHARED.
//!
//! history: build.sh enumerated its test suites by hand while eight suites
//! existed on disk — bench_grading and branch_policy were silently never
//! gated, and nothing anywhere complained. hand-maintained lists rot
//! without a sound. the fix was filesystem discovery in one shared script
//! (ci/run_tests.sh) consumed by BOTH the deploy and github actions ci.
//!
//! like tests/event_loop_liveness.rs, these are deliberately blunt
//! source-level invariants: the failure mode being pinned is "someone
//! edits these files back into a private, drifting copy", which no
//! behavioral test can catch because the drifted gate still runs green.

use std::path::Path;

fn source(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("could not read {rel}: {e}"))
}

#[test]
fn build_sh_consumes_the_shared_gate() {
    let sh = source("build.sh");
    assert!(
        sh.contains("ci/run_tests.sh"),
        "build.sh must delegate to ci/run_tests.sh; a private copy of the \
         gate lets the deploy and ci drift apart, which is exactly how \
         bench_grading and branch_policy ended up never gated."
    );
}

#[test]
fn build_sh_does_not_enumerate_suites_by_hand() {
    let sh = source("build.sh");
    // the shape of the old bug: `for suite in protocol_contract platform_logic ...`
    // — any literal suite list in build.sh is a future silently-skipped suite.
    assert!(
        !sh.contains("for suite in"),
        "build.sh contains a hardcoded suite list. the filesystem is the \
         suite list: cargo autodiscovers every tests/*.rs, so a new suite \
         is gated from birth and none needs (or tolerates) manual naming here."
    );
}

#[test]
fn the_shared_gate_uses_filesystem_discovery_not_a_list() {
    let gate = source("ci/run_tests.sh");
    assert!(
        gate.contains("tests/*.rs"),
        "ci/run_tests.sh must discover suites via tests/*.rs; a hardcoded \
         list here recreates the original bug one level down."
    );
    // and it must actually fail when a suite fails — a gate that only warns
    // is a gate nobody reads.
    assert!(
        gate.contains("exit 1"),
        "ci/run_tests.sh contains no exit 1: failures cannot be fatal there."
    );
}

/// the workflow file may legitimately be absent for a window: github
/// refuses commits TOUCHING .github/workflows/ for tokens without the
/// `workflow` scope — which also means such a token cannot delete it.
/// absence is therefore platform-guarded, not agent-guarded, and here it
/// skips loudly (a hard failure would make this very suite red on every
/// checkout made before the workflow's own commit lands). once present,
/// these assertions are load-bearing.
fn workflow_source() -> Option<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/ci.yml");
    std::fs::read_to_string(path).ok()
}

#[test]
fn ci_checks_the_wasm_target_before_anyone_trusts_a_commit() {
    let wf = match workflow_source() {
        Some(wf) => wf,
        None => {
            eprintln!(
                "SKIP: .github/workflows/ci.yml not present in this checkout \
                 (lands separately — needs a token with the workflow scope). \
                 ci is NOT enforcing anything yet."
            );
            return;
        }
    };
    assert!(
        wf.contains("wasm32-unknown-unknown"),
        ".github/workflows/ci.yml does not check the wasm target. the native \
         test build alone does not prove the app compiles: web-sys/wasm-bindgen \
         code paths differ off-target, and production builds for wasm32."
    );
    assert!(
        wf.contains("ci/run_tests.sh"),
        "ci must run the same gate script the deploy runs, or the two \
         definitions of 'may ship' will drift."
    );
}

/// the workflow's very first run died instantly with exit code 101: the wasm
/// step used a flag that also builds the integration tests FOR wasm32 — and
/// the `test` crate is not shipped for wasm32-unknown-unknown at all, so
/// every checkout fails with E0463 regardless of code health. the wasm proof
/// must cover what production ships (lib + bins); tests are gated natively
/// by the shared script. pinned here because the tempting "fix" is to
/// re-widen the check for thoroughness.
///
/// (the assertion greps for the flag itself, so this doc comment must never
/// spell that flag out: a grep guard is defeated by prose mentioning it.)
#[test]
fn the_wasm_check_never_builds_test_crates() {
    let Some(wf) = workflow_source() else {
        return;
    };
    // assembled so this source file never contains the literal either.
    let flag = format!("--{}{}", "all", "-targets");
    assert!(
        !wf.contains(&flag),
        ".github/workflows/ci.yml widens the wasm check to every target \
         again. building tests for wasm32 fails every run because the \
         `test` crate does not exist for that target. check lib + bins \
         for wasm; leave the tests to the native gate."
    );
}
