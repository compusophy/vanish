//! behavioral evals for the agent loop's decision layer.
//!
//! unit tests pin individual functions; these are SCENARIOS — sequences of
//! events a real run produces, replayed through the extracted decision code
//! and judged on outcomes. every scenario that asserts "the checker passes
//! this" has a sibling NEGATIVE CONTROL asserting it FAILS corrupted input:
//! a checker that has never caught anything is decoration (the mirror-test
//! lesson from loop mode: a test that mirrors the logic verifies the mirror).
//!
//! pure rust only — cargo test runs in build.sh, so a regression here fails
//! the deploy instead of shipping.

use vanish::agent::control::{
    cancellation_results, decide_after_turn, history_is_well_formed, Action,
    FailureBudget, FailureDecision,
};
use vanish::agent::llm::{Message, ToolCall};

// ---- fixtures ------------------------------------------------------------

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        kind: "function".to_string(),
        function: vanish::agent::llm::FunctionCall {
            name: "read_file".to_string(),
            arguments: r#"{"path":"README.md"}"#.to_string(),
        },
    }
}

fn assistant_with_calls(calls: &[ToolCall]) -> Message {
    Message {
        role: "assistant".to_string(),
        content: None,
        tool_calls: Some(calls.to_vec()),
        tool_call_id: None,
    }
}

fn assistant_prose() -> Message {
    // Message::text is the public constructor; integration tests cannot
    // attach inherent impls to the crate's types.
    Message::text("assistant", "done.")
}

/// a complete, well-formed exchange: prompt → assistant(2 calls) → results →
/// assistant(final prose). this is the shape every real run must produce.
fn healthy_transcript() -> Vec<Message> {
    vec![
        Message::system("sys"),
        Message::user("go"),
        assistant_with_calls(&[call("a"), call("b")]),
        Message::tool_result("a", "ok"),
        Message::tool_result("b", "ok"),
        assistant_prose(),
    ]
}

// ---- scenario 1: failure storm -------------------------------------------
// an unattended loop hits a rate-limit blip. verdict: survive four
// consecutive failures with escalating backoff, die on the fifth, and let a
// single success anywhere reset the whole budget.

#[test]
fn failure_storm_retries_four_times_then_gives_up() {
    let mut b = FailureBudget::new(5);
    let mut attempts = Vec::new();
    for _ in 0..4 {
        match b.record_failure() {
            FailureDecision::Retry { attempt, delay_ms } => attempts.push((attempt, delay_ms)),
            FailureDecision::GiveUp => panic!("budget gave up before the 5th failure"),
        }
    }
    // the budget hands retry_backoff_ms its 1-based consecutive count, so
    // the observed ladder is 2s→8s→30s→60s (the schedule's attempt-0 rung
    // is unreachable through this type).
    assert_eq!(
        attempts,
        vec![(1, 2_000), (2, 8_000), (3, 30_000), (4, 60_000)],
        "backoff must escalate toward the cap"
    );
    assert_eq!(b.record_failure(), FailureDecision::GiveUp);
}

#[test]
fn one_success_amid_flakiness_resets_the_budget() {
    // the realistic pattern: fail, fail, WORK, fail, fail... a run in a
    // rate-limited window must not accumulate toward give-up across a
    // success. total failures here: 7 — more than the budget — but never
    // 5 consecutively.
    let mut b = FailureBudget::new(5);
    for round in 0..3 {
        for _ in 0..2 {
            assert!(matches!(b.record_failure(), FailureDecision::Retry { .. }));
        }
        b.record_success();
        let _ = round;
    }
    assert_eq!(b.consecutive(), 0);
}

#[test]
fn a_dead_key_gives_up_instead_of_billing_forever() {
    // negative-flavored control of the other direction: permanent failure
    // must terminate. five strikes, out.
    let mut b = FailureBudget::new(5);
    for i in 1..5 {
        assert!(
            matches!(b.record_failure(), FailureDecision::Retry { .. }),
            "failure #{i} should still retry"
        );
    }
    assert_eq!(b.record_failure(), FailureDecision::GiveUp);
}

// ---- scenario 2: stop lands mid-batch ------------------------------------
// the user presses stop while three tool calls are queued. verdict: the two
// abandoned calls get synthetic results and the transcript stays replayable;
// a version of the loop that skips them poisons the saved history.

#[test]
fn mid_batch_stop_leaves_a_replayable_transcript() {
    let calls = [call("a"), call("b"), call("c")];
    let mut history = vec![
        Message::system("sys"),
        Message::user("go"),
        assistant_with_calls(&calls),
        Message::tool_result("a", "ran"),
        // stop landed here: b and c were never dispatched.
    ];
    history.extend(cancellation_results(&calls[1..]));

    assert!(
        history_is_well_formed(&history).is_ok(),
        "synthetic results must make the abandoned batch answerable"
    );
}

#[test]
fn negative_control_skipping_abandoned_calls_is_detected() {
    // proves the checker would catch the bug class cancellation_results
    // exists to prevent. the naive fast-exit skips the synthetic result for
    // call b; the checker must REJECT that transcript. if this ever stops
    // holding, the invariant check is decorative and the next mid-batch
    // stop ships silently poisoned history.
    let history = vec![
        Message::system("sys"),
        Message::user("go"),
        assistant_with_calls(&[call("a"), call("b")]),
        Message::tool_result("a", "ran"),
        // b deliberately unanswered — exactly what a naive fast-exit does.
    ];
    let verdict = history_is_well_formed(&history);
    assert!(
        verdict.is_err(),
        "a transcript with an unanswered tool_call MUST be rejected, got: {verdict:?}"
    );
}

// ---- scenario 3: restored transcripts ------------------------------------
// boot replays saved history into both the ui and the model context. any of
// these shapes arriving corrupt must be detected at restore time rather
// than exploding later inside an api call with no diagnosis.

#[test]
fn healthy_transcript_passes_and_retention_trim_stays_valid() {
    let full = healthy_transcript();
    assert!(history_is_well_formed(&full).is_ok());

    // retention trims from the OLDEST end; trimming the system message or
    // the first user turn keeps every call/result pair intact.
    let trimmed: Vec<Message> = full[2..].to_vec();
    assert!(
        history_is_well_formed(&trimmed).is_ok(),
        "oldest-end trim must not break call/result pairing"
    );
}

#[test]
fn negative_control_orphan_result_is_detected() {
    // the shape a bad merge or manual edit produces: a result whose call
    // was lost. replaying this gets rejected by the api.
    let history = vec![
        Message::user("go"),
        Message::tool_result("ghost", "answers nothing"),
    ];
    let verdict = history_is_well_formed(&history);
    assert!(
        verdict.is_err(),
        "an orphaned tool result MUST be rejected, got: {verdict:?}"
    );
}

#[test]
fn negative_control_truncated_tail_is_detected() {
    // a crash mid-save can leave the last tool result missing entirely.
    let history = vec![Message::user("go"), assistant_with_calls(&[call("a")])];
    let verdict = history_is_well_formed(&history);
    assert!(
        verdict.is_err(),
        "an unanswered trailing tool_call MUST be rejected, got: {verdict:?}"
    );
}

// ---- scenario 4: after-turn routing --------------------------------------
// loop mode's contract: silence is a pause (nudge), never an ending — but
// ONLY when loop mode is actually on. this is the path behind the old
// "loop looks like a hang" complaint.

#[test]
fn loop_mode_nudges_on_silence_and_normal_mode_completes() {
    assert_eq!(decide_after_turn(false, true), Action::Nudge);
    assert_eq!(decide_after_turn(false, false), Action::Complete);
    // tools always run regardless of mode.
    assert_eq!(decide_after_turn(true, true), Action::RunTools);
    assert_eq!(decide_after_turn(true, false), Action::RunTools);
}

// ---- scenario 5: system-prompt seeding ------------------------------------
// the retention cap can trim the system prompt off a long conversation. the
// naive `history.is_empty()` gate left such transcripts instruction-less;
// presence-of-system-role is the correct trigger.

#[test]
fn seeding_fires_for_role_absence_not_empty_history() {
    use vanish::agent::control::needs_system_seed;

    assert!(needs_system_seed(&[]));
    // a long-lived thread whose prompt aged out MUST reseed.
    assert!(needs_system_seed(&[
        Message::user("old task from yesterday"),
        assistant_prose(),
    ]));
    // a restored thread still carrying its prompt must NOT get a second one.
    assert!(!needs_system_seed(&[Message::system("sys"), Message::user("go")]));

    // negative control: the old is_empty() gate misses the aged-out case by
    // construction — pinned so nobody "simplifies" back to it.
    let aged_out = [Message::user("old task")];
    assert!(
        needs_system_seed(&aged_out),
        "non-empty history without a system role still needs seeding"
    );
}

// ---- scenario 6: batch queue (the programmatic driver) --------------------
// a benchmark harness submits tasks and reads outcomes. BatchState is the
// durable queue: it must advance exactly one task per outcome, survive a
// serde round trip (it persists in the transcript index), and export only
// what actually ran.

use vanish::agent::control::{BatchState, TaskStatus};

fn batch_fixture() -> BatchState {
    BatchState::new(vec![
        ("t1".into(), "first".into()),
        ("t2".into(), "second".into()),
        ("t3".into(), "third".into()),
    ])
}

#[test]
fn batch_advances_one_task_per_outcome() {
    let mut b = batch_fixture();
    assert_eq!(b.next_prompt().as_deref(), Some("first"));

    b.mark_running();
    assert_eq!(b.status[0], TaskStatus::Running);
    b.complete_current("completed");
    assert_eq!(b.next_prompt().as_deref(), Some("second"));
    assert_eq!(b.status[0], TaskStatus::Done("completed".into()));

    b.complete_current("step_limit");
    b.complete_current("failed");
    // drained: no prompt left, everything recorded.
    assert_eq!(b.next_prompt(), None);
    assert_eq!(b.results().len(), 3);
}

#[test]
fn batch_results_carry_id_and_reason_in_submission_order() {
    let mut b = batch_fixture();
    b.complete_current("completed");
    b.complete_current("stopped");
    let r = b.results();
    assert_eq!(r[0].id, "t1");
    assert_eq!(r[0].reason, "completed");
    assert_eq!(r[1].id, "t2");
    assert_eq!(r[1].reason, "stopped");
}

#[test]
fn batch_survives_a_serde_round_trip() {
    // this is the durability contract: the state persists in the transcript
    // index as json and must come back identical after a tab discard.
    let mut b = batch_fixture();
    b.mark_running();
    b.complete_current("completed");
    let json = serde_json::to_string(&b).unwrap();
    let back: BatchState = serde_json::from_str(&json).unwrap();
    assert_eq!(back, b);
    assert_eq!(back.next_prompt().as_deref(), Some("second"));
}

#[test]
fn cancelled_batch_exports_only_what_ran() {
    let mut b = batch_fixture();
    b.complete_current("completed");
    b.complete_current("completed");
    b.complete_current("stopped"); // stop landed during task 3
    // all three have outcomes; a batch cancelled EARLIER exports fewer.
    assert_eq!(b.results().len(), 3);
    assert_eq!(b.results()[2].reason, "stopped");

    // the early-cancel shape: two ran, one never started.
    let mut c = batch_fixture();
    c.complete_current("completed");
    c.complete_current("stopped");
    assert_eq!(c.results().len(), 2);
}

#[test]
fn empty_batch_is_rejected_before_it_starts() {
    let b = BatchState::new(vec![]);
    assert!(b.is_empty());
    assert_eq!(b.next_prompt(), None);
}
