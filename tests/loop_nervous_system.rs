//! tests for the loop's nervous system: when it waits, when it gives up,
//! and how it reads its own build results. these are exactly the decisions
//! an unattended loop makes with no human watching, so their behavior is
//! pinned here rather than trusted.

use vanish::agent::control::{
    decide_after_run_end, resume_marker_is_fresh, resume_target, LoopContinuation, RestartBudget,
    MAX_RESTARTS_PER_WINDOW, RESUME_MARKER_MAX_AGE_MS, RESTART_WINDOW_MS,
};
use vanish::agent::github::{CheckSummary, DeployState};
use vanish::agent::retry_backoff_ms;

// ---- automatic loop continuation ------------------------------------------
// loop mode promises run-until-stopped. a failure budget, a step ceiling,
// or a clean completion is NOT the user stopping — each used to simply end
// the run, so an unattended loop died quietly in the night. these pin which
// endings continue and which must never be second-guessed.

#[test]
fn loop_mode_restarts_on_non_stop_endings() {
    // failed (budget spent), step_limit (ceiling hit) and completed all get
    // another attempt — an attended user would relaunch by hand; that is
    // exactly what the unattended case needs done for it.
    for reason in ["failed", "step_limit", "completed"] {
        assert_eq!(
            decide_after_run_end(reason, true, true, false),
            LoopContinuation::Restart,
            "reason {reason} must restart an overnight loop"
        );
    }
}

#[test]
fn stop_is_never_second_guessed() {
    // stop is the only escape hatch from a wedged run (D9). restarting
    // after it would trap the user inside a loop they explicitly killed.
    assert_eq!(
        decide_after_run_end("stopped", true, true, false),
        LoopContinuation::LetEnd
    );
}

#[test]
fn loop_off_never_restarts() {
    for reason in ["completed", "stopped", "failed", "step_limit"] {
        assert_eq!(
            decide_after_run_end(reason, false, true, false),
            LoopContinuation::LetEnd,
            "reason {reason}: without loop mode a run is a unit of work with an end"
        );
    }
}

#[test]
fn a_user_who_switched_threads_is_not_restarted_under() {
    // they are typing on the thread they chose; a surprise run appearing
    // there is worse than a dead loop.
    assert_eq!(
        decide_after_run_end("failed", true, false, false),
        LoopContinuation::LetEnd
    );
}

#[test]
fn batch_runs_are_never_continued_by_the_loop() {
    // batch tasks run through start_run too; the driver owns the queue and
    // starts each next task itself. an automatic successor would race the
    // driver or fire as a ghost after the queue drains.
    for reason in ["completed", "failed", "step_limit"] {
        assert_eq!(
            decide_after_run_end(reason, true, true, true),
            LoopContinuation::LetEnd,
            "reason {reason}: a batch task's ending belongs to the driver"
        );
    }
}

#[test]
fn unknown_reasons_default_to_continuing_in_loop_mode() {
    // a FinishReason added later must not silently break continuation;
    // the conservative default for loop mode is to go on.
    assert_eq!(
        decide_after_run_end("something_new", true, true, false),
        LoopContinuation::Restart
    );
}

// ---- resume marker freshness ------------------------------------------------

#[test]
fn fresh_markers_resume_and_stale_ones_do_not() {
    let now = 1_800_000_000_000.0;
    assert!(resume_marker_is_fresh(now - 60_000.0, now), "a minute old");
    assert!(
        resume_marker_is_fresh(now - 11.0 * 3_600_000.0, now),
        "eleven hours still spans an overnight run"
    );
    // exactly at the threshold counts as fresh (inclusive bound).
    assert!(resume_marker_is_fresh(now - RESUME_MARKER_MAX_AGE_MS, now));
    assert!(
        !resume_marker_is_fresh(now - RESUME_MARKER_MAX_AGE_MS - 1.0, now),
        "past the window it is archaeology, not a pause"
    );
    assert!(!resume_marker_is_fresh(now - 48.0 * 3_600_000.0, now), "two days");
}

#[test]
fn a_backwards_clock_does_not_cost_the_run() {
    // device clock moved backwards between write and read: negative age
    // must read as fresh, not as infinitely stale.
    let written_later = 1_800_000_000_000.0 + 5 * 60_000.0;
    let read_now = 1_800_000_000_000.0;
    assert!(resume_marker_is_fresh(written_later, read_now));
}

#[test]
fn the_marker_window_spans_a_work_night_but_not_a_weekend() {
    const H: f64 = 3_600_000.0;
    assert!(RESUME_MARKER_MAX_AGE_MS >= 8.0 * H);
    assert!(RESUME_MARKER_MAX_AGE_MS <= 24.0 * H);
    assert_eq!(RESUME_MARKER_MAX_AGE_MS, 12.0 * H);
}

// ---- crash-loop breaker ------------------------------------------------------
// automatic restarts must not become a billing pump when something is
// structurally broken (poisoned transcript, revoked key): N per rolling
// window, oldest falling off, and a manual run resets everything.

#[test]
fn the_budget_allows_exactly_max_restarts_then_refuses() {
    let mut b = RestartBudget::new(RESTART_WINDOW_MS, MAX_RESTARTS_PER_WINDOW);
    let t = 1000.0;
    for i in 0..MAX_RESTARTS_PER_WINDOW as u64 {
        assert!(
            b.record(t + i as f64 * 60_000.0),
            "restart {} within the budget must be allowed",
            i + 1
        );
    }
    assert!(
        !b.record(t + 6.0 * 60_000.0),
        "the window is saturated: staying down is the point"
    );
}

#[test]
fn old_attempts_expire_so_a_slow_loop_survives() {
    let window = RESTART_WINDOW_MS;
    let mut b = RestartBudget::new(window, 2);
    let t = 500_000.0;
    assert!(b.record(t));
    assert!(b.record(t + window / 2.0)); // two used, first expires at t+window
    assert!(b.record(t + window + 1_000.0)); // first fell off; allowed again
    assert_eq!(b.used(), 2);
}

#[test]
fn saturation_clears_once_everything_has_expired() {
    let window = RESTART_WINDOW_MS;
    let mut b = RestartBudget::new(window, 1);
    let t = 42.0;
    assert!(b.record(t));
    assert!(!b.record(t + 1_000.0));
    assert!(
        b.record(t + window + 1_000.0),
        "once the failure has aged out of the window, one more try is honest"
    );
}

#[test]
fn a_manual_run_resets_crash_loop_suspicion() {
    let mut b = RestartBudget::new(RESTART_WINDOW_MS, 1);
    assert!(b.record(0.0));
    assert!(!b.record(1.0));
    b.reset();
    assert!(
        b.record(2.0),
        "a human pressing run overrides the breaker entirely"
    );
}

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

