//! tests for the loop's nervous system: when it waits, when it gives up,
//! and how it reads its own build results. these are exactly the decisions
//! an unattended loop makes with no human watching, so their behavior is
//! pinned here rather than trusted.

use vanish::agent::control::resume_target;
use vanish::agent::github::{CheckSummary, DeployState};
use vanish::agent::retry_backoff_ms;

// ---- interruption-resume targeting ---------------------------------------
// every run writes a resume marker because the browser can discard a hidden
// tab at any moment, killing the worker without an event. these pin WHICH
// interrupted run a boot may continue: an existing conversation only, never
// a deleted thread resurrected as a surprise run, never an empty target.

#[test]
fn resume_targets_the_marked_conversation_when_it_exists() {
    let items = vec!["111".to_string(), "222".to_string()];
    assert_eq!(resume_target(&items, "222"), Some("222".to_string()));
    assert_eq!(resume_target(&items, "111"), Some("111".to_string()));
}

#[test]
fn resume_never_resurrects_a_deleted_thread() {
    let items = vec!["111".to_string()];
    // the marked thread was deleted while the run was interrupted.
    assert_eq!(resume_target(&items, "999"), None);
}

#[test]
fn resume_refuses_an_empty_marker() {
    let items = vec!["111".to_string()];
    assert_eq!(resume_target(&items, ""), None);
    assert_eq!(resume_target(&[], ""), None);
}

#[test]
fn resume_with_no_conversations_at_all_is_none() {
    assert_eq!(resume_target(&[], "111"), None);
}

// ---- retry backoff -------------------------------------------------------
// the schedule an unattended loop follows after a transient llm failure.
// too aggressive and a rate-limit window kills the run; too timid and a
// dead key produces hundreds of doomed requests before giving up.

#[test]
fn backoff_grows_then_caps() {
    assert_eq!(retry_backoff_ms(1), 2_000);
    assert_eq!(retry_backoff_ms(2), 8_000);
    assert_eq!(retry_backoff_ms(3), 30_000);
    // attempts beyond the schedule cap at one minute.
    assert_eq!(retry_backoff_ms(4), 60_000);
    assert_eq!(retry_backoff_ms(9), 60_000);
    assert_eq!(retry_backoff_ms(100), 60_000);
}

#[test]
fn backoff_is_always_positive_and_bounded() {
    for attempt in 0..50 {
        let d = retry_backoff_ms(attempt);
        assert!(d > 0, "attempt {attempt}: delay must be positive");
        assert!(d <= 60_000, "attempt {attempt}: delay must stay capped");
    }
}

#[test]
fn backoff_never_decreases() {
    let mut last = 0;
    for attempt in 1..12 {
        let d = retry_backoff_ms(attempt);
        assert!(d >= last, "attempt {attempt}: schedule went backwards");
        last = d;
    }
}

// ---- deploy verdicts ------------------------------------------------------
// check_deployment is how the loop learns whether its own commit compiled.
// misreading these states means either shipping broken code (false success)
// or freezing forever (false pending). DeployState::from IS the aggregation
// the tool uses, so the tests below drive the real function, not a mirror.
fn checks(states: &[(&str, &str)]) -> Vec<CheckSummary> {
    states
        .iter()
        .map(|(name, state)| CheckSummary {
            name: name.to_string(),
            state: state.to_string(),
            detail: String::new(),
            url: String::new(),
        })
        .collect()
}

#[test]
fn verdict_matrix() {
    let cases: Vec<(Vec<(&str, &str)>, &str)> = vec![
        (vec![], "none"),
        (vec![("vercel", "success")], "success"),
        (vec![("vercel", "failure")], "failure"),
        // any failure wins over any amount of success.
        (vec![("vercel", "success"), ("vercel", "failure")], "failure"),
        (vec![("vercel", "error")], "failure"),
        (vec![("vercel", "timed_out")], "failure"),
        (vec![("vercel", "cancelled")], "failure"),
        // still-running beats success: reporting success mid-build would
        // let the loop move on from possibly-broken code.
        (vec![("vercel", "success"), ("vercel", "pending")], "pending"),
        (vec![("vercel", "queued")], "pending"),
        (vec![("vercel", "in_progress")], "pending"),
        (vec![("vercel", "waiting")], "pending"),
        // but a failure alongside still-running states still reports
        // failure immediately — bad news does not wait.
        (
            vec![("a", "failure"), ("b", "in_progress")],
            "failure",
        ),
    ];

    for (raw, expected) in cases {
        let c = checks(&raw);
        let state = DeployState::from(c);
        assert_eq!(
            state.verdict, expected,
            "states {raw:?} should read as {expected}"
        );
        assert_eq!(state.checks.len(), raw.len());
        assert_eq!(state.settled(), expected == "success" || expected == "failure");
    }
}

#[test]
fn settled_only_after_success_or_failure() {
    let s = DeployState {
        verdict: "success".into(),
        checks: vec![],
    };
    let f = DeployState {
        verdict: "failure".into(),
        checks: vec![],
    };
    let p = DeployState {
        verdict: "pending".into(),
        checks: vec![],
    };
    let n = DeployState {
        verdict: "none".into(),
        checks: vec![],
    };
    assert!(s.settled());
    assert!(f.settled());
    assert!(!p.settled(), "pending must keep the loop waiting");
    assert!(!n.settled(), "no-checks-yet must keep the loop waiting");
}

