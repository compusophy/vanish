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
    // normalized: this repo is edited on a checkout with core.autocrlf=true,
    // and every shape guard below would go red on \r\n while ci passed.
    std::fs::read_to_string(path)
        .ok()
        .map(|t| t.replace("\r\n", "\n"))
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

// ---- e2e / preview gate (STACKED_PRS_PLAN §4 item 4) -----------------------
//
// compile-green is not booted-green: a commit that compiles yet blanks the
// page ships straight to users, because every pre-merge check until now was
// compile-level. .github/workflows/e2e.yml drives playwright against the
// vercel PREVIEW deployment of a pr's head; its check lands on that head,
// where Github::deployment_state already reads it — so merge_pr refuses
// until the app demonstrably boots. same skip-loudly semantics as
// workflow_source() above: the file may legitimately be absent in checkouts
// made before this lands, but once present these assertions are load-bearing.

fn e2e_workflow_source() -> Option<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/e2e.yml");
    std::fs::read_to_string(path).ok()
}

#[test]
fn e2e_gate_waits_for_the_preview_and_smokes_the_real_app() {
    let Some(wf) = e2e_workflow_source() else {
        eprintln!(
            "SKIP: .github/workflows/e2e.yml not present in this checkout — \
             the preview gate is NOT enforcing anything yet."
        );
        return;
    };
    // it must trigger on pull_request (that is what makes main
    // promote-on-green rather than push-and-hope).
    assert!(
        wf.contains("pull_request"),
        "e2e workflow must run on pull_request; gating only pushes would \
         leave merged-to-main commits as untested as before."
    );
    // it must wait for THIS pr's head sha — testing some other deployment's
    // url is theater (the misattributed-verdict class of bug, D4).
    assert!(
        wf.contains("head.sha"),
        "e2e workflow does not resolve the preview by the PR head sha — it \
         may be smoking a stale or unrelated deployment."
    );
    // and the smoke must be the behavioral one, not just an HTTP 200 ping:
    // an auth interstitial also returns 200.
    assert!(
        wf.contains("ci/e2e.mjs"),
        "e2e workflow must drive ci/e2e.mjs; a curl-based gate cannot tell \
         a booted app from a protection interstitial."
    );
}

#[test]
fn the_e2e_smoke_asserts_boot_not_mere_reachability() {
    let script = source("ci/e2e.mjs");
    // #status starts as literally "booting…" in web/index.html and only
    // changes when the worker announces itself — asserting it moved is the
    // observable proof of Event::Ready.
    assert!(
        script.contains("#status") && script.contains("booting"),
        "ci/e2e.mjs no longer asserts the boot signal (#status leaving \
         'booting…'). reachability alone proves nothing about a wasm app."
    );
    // deployment protection produces a 200 page that is NOT the app; the
    // smoke must name that failure mode distinctly instead of reporting a
    // confusing boot failure.
    assert!(
        script.to_lowercase().contains("protection"),
        "ci/e2e.mjs dropped the deployment-protection diagnosis — when the \
         preview walls itself behind auth, the report must say THAT, not a \
         misleading 'worker never announced ready'."
    );
}

/// the diagnostics publisher must not be able to fail the gate, and must
/// only run when there is something to publish.
///
/// both halves are scar tissue from 2026-08-26. the step ran `if: always()`
/// and force-pushed to one branch, so the push- and pull_request-triggered
/// runs of the SAME commit reached it together, one lost the ref lock
/// ("cannot lock ref 'refs/heads/diagnostics'"), and its non-zero exit took
/// the whole verify job red — with the gate itself green three steps above.
/// a green run also overwrote the failure log the next reader needed.
///
/// this is D9's shape applied to ci: a diagnostic that breaks the thing it
/// observes is not a diagnostic.
#[test]
fn the_diagnostics_publisher_cannot_turn_a_green_gate_red() {
    let Some(wf) = workflow_source() else {
        eprintln!("SKIP: .github/workflows/ci.yml not present in this checkout");
        return;
    };

    let step = wf
        .split("- name: publish diagnostics")
        .nth(1)
        .expect("ci.yml has no `publish diagnostics` step — it is the self-repair loop");

    assert!(
        step.contains("if: failure()"),
        "the diagnostics step must run only on failure. `always()` publishes \
         green runs, which CLOBBERS the failure log that is the entire point \
         of the diagnostics branch."
    );
    assert!(
        !step.contains("if: always()"),
        "the diagnostics step is back on `always()`"
    );

    // the push is the racy part: it must be retried and it must not be the
    // last word on the step's exit status.
    assert!(
        step.contains("for attempt in"),
        "the diagnostics push is not retried; a lost ref lock between two \
         concurrent runs of the same commit will fail the job again."
    );
    assert!(
        step.contains("::warning::"),
        "a diagnostics push that gives up must say so loudly (D4) rather than \
         failing silently or fatally."
    );
    assert!(
        !step.trim_end().ends_with("\"HEAD:diagnostics\""),
        "the step ends on a bare push: its exit status decides the gate again"
    );
}
