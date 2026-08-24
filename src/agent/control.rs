//! the agent loop's decision layer, extracted for evaluation.
//!
//! the loop in `mod.rs` interleaves three kinds of code: i/o (streaming a
//! turn, dispatching a tool), durability (persist checkpoints), and
//! DECISIONS (retry or give up? execute the next tool or honor the stop?
//! nudge because loop mode is on, or end the run?). the i/o needs a browser
//! and live keys; the decisions need nothing but inputs. extracting them
//! here means an eval suite can rehearse failure storms, mid-batch stops
//! and malformed transcripts at test speed — the paths that shipped two
//! bricking bugs with zero behavioral coverage.
//!
//! rule of the module: pure. no js, no await, no globals. anything that
//! needs the world takes it as an argument.

use crate::agent::llm::{Message, ToolCall};

// ---- transient-failure budget -------------------------------------------

/// what to do after one more consecutive llm failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureDecision {
    /// wait `delay_ms` and try again; `attempt` is 1-based.
    Retry { attempt: u32, delay_ms: i32 },
    /// the budget is spent; end the run rather than spin on a dead key.
    GiveUp,
}

/// counts consecutive llm failures; any success resets it.
///
/// the schedule lives in `crate::agent::retry_backoff_ms` and is pinned by
/// tests/loop_nervous_system.rs; this type owns only the counting and the
/// give-up threshold, so the two halves can be evaluated independently.
#[derive(Debug)]
pub struct FailureBudget {
    pub max_consecutive: u32,
    consecutive: u32,
}

impl FailureBudget {
    pub fn new(max_consecutive: u32) -> Self {
        Self {
            max_consecutive,
            consecutive: 0,
        }
    }

    pub fn record_success(&mut self) {
        self.consecutive = 0;
    }

    /// the nth consecutive failure retries until n reaches the budget,
    /// then gives up. 1-based attempts, matching the ui notes.
    pub fn record_failure(&mut self) -> FailureDecision {
        self.consecutive += 1;
        if self.consecutive >= self.max_consecutive {
            return FailureDecision::GiveUp;
        }
        FailureDecision::Retry {
            attempt: self.consecutive,
            delay_ms: crate::agent::retry_backoff_ms(self.consecutive),
        }
    }

    pub fn consecutive(&self) -> u32 {
        self.consecutive
    }
}

// ---- system-prompt seeding ----------------------------------------------

/// seed only when the conversation genuinely has no system role. a restored
/// transcript normally carries its original prompt; the retention cap can
/// trim it off the oldest end — in that case the fresh prompt is correct,
/// and a naive `history.is_empty()` check would leave the model with no
/// instructions at all.
pub fn needs_system_seed(history: &[Message]) -> bool {
    !history.iter().any(|m| m.role == "system")
}

// ---- after-turn routing --------------------------------------------------

/// what the loop should do with a turn that produced no tool calls — or,
/// seen from the other side, that produced some.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// dispatch the turn's tool calls.
    RunTools,
    /// end the run: the model said something final and loop mode is off.
    Complete,
    /// loop mode treats silence as a pause, not an ending: append the nudge
    /// and keep going.
    Nudge,
}

pub fn decide_after_turn(has_tool_calls: bool, loop_mode: bool) -> Action {
    if has_tool_calls {
        Action::RunTools
    } else if loop_mode {
        Action::Nudge
    } else {
        Action::Complete
    }
}

// ---- interruption-resume targeting ---------------------------------------

/// which conversation a boot-time resume should continue in, given the
/// conversations that exist and the one recorded in the interruption marker.
///
/// EVERY run writes a resume marker, not just loop mode: the browser may
/// discard a hidden tab at any moment (memory saver, mobile os) — killing
/// the worker with no event. the transcript survives that (it checkpoints
/// per step); the marker is what lets the next boot CONTINUE the run rather
/// than leave a dead one behind. adoption is refused when the marked
/// conversation no longer exists — a deleted thread must never resurrect as
/// a surprise run — or when the marker names nothing at all. pure so the
/// eval suite can pin the decision without touching storage.
pub fn resume_target(items: &[String], marker_conversation: &str) -> Option<String> {
    if marker_conversation.is_empty() {
        return None;
    }
    items
        .iter()
        .find(|id| id.as_str() == marker_conversation)
        .cloned()
}

// ---- mid-batch cancellation ---------------------------------------------

/// synthetic results for tool calls abandoned when stop landed mid-batch.
///
/// the api REJECTS any assistant message whose tool_calls lack matching
/// results, so skipping these to end a run faster would poison the saved
/// transcript: the NEXT run replaying that history would fail outright.
/// ending cleanly costs one synthetic message per abandoned call.
pub fn cancellation_results(calls: &[ToolCall]) -> Vec<Message> {
    calls
        .iter()
        .map(|c| {
            Message::tool_result(
                c.id.clone(),
                serde_json::json!({
                    "success": false,
                    "error": "not run — the user stopped the run before this call"
                })
                .to_string(),
            )
        })
        .collect()
}

// ---- batch queue (the programmatic driver) --------------------------------
//
// a benchmark harness needs to submit work and read outcomes; the prompt box
// is neither. BatchState is the durable queue: it persists beside the resume
// marker so a tab discard mid-batch resumes where it left off, and its
// results are exported to opfs when the batch ends.

/// one task's progress through a batch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TaskStatus {
    /// not yet started.
    Queued,
    /// currently executing.
    Running,
    /// finished with this outcome (mirrors FinishReason, as a string, so the
    /// exported results file stays plain json).
    Done(String),
}

/// the durable state of a running batch. persisted with the transcript index.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BatchState {
    pub tasks: Vec<(String, String)>,
    pub status: Vec<TaskStatus>,
    pub current: usize,
}

impl BatchState {
    pub fn new(tasks: Vec<(String, String)>) -> Self {
        let status = vec![TaskStatus::Queued; tasks.len()];
        Self {
            tasks,
            status,
            current: 0,
        }
    }

    /// the prompt to run next, if any task remains. empty when the queue is
    /// drained — callers distinguish "done" from "run this".
    pub fn next_prompt(&self) -> Option<String> {
        self.tasks.get(self.current).map(|(_, p)| p.clone())
    }

    pub fn mark_running(&mut self) {
        if let Some(slot) = self.status.get_mut(self.current) {
            *slot = TaskStatus::Running;
        }
    }

    /// record the outcome of the current task and advance the cursor.
    /// advancing past the end is how "queue drained" is represented; callers
    /// detect completion via `next_prompt() -> None`.
    pub fn complete_current(&mut self, reason: &str) {
        if let Some(slot) = self.status.get_mut(self.current) {
            *slot = TaskStatus::Done(reason.to_string());
        }
        self.current += 1;
    }

    /// results so far, in submission order. unfinished tasks are simply not
    /// in the list yet — a cancelled batch exports what actually ran.
    pub fn results(&self) -> Vec<crate::protocol::BatchResult> {
        self.tasks
            .iter()
            .zip(self.status.iter())
            .filter_map(|((id, _), s)| match s {
                TaskStatus::Done(reason) => Some(crate::protocol::BatchResult {
                    id: id.clone(),
                    reason: reason.clone(),
                    steps: 0,
                }),
                _ => None,
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

// ---- automatic loop continuation ------------------------------------------
//
// loop mode promises "run until a human stops it". three endings are not a
// human stopping: the failure budget giving up, the step ceiling, and
// (defensively) completion. each of those used to simply end the run — so
// an overnight loop died quietly at 2am and looked, from the feed, like it
// had never been asked to continue. these pure functions decide whether a
// run that just ended should be followed by another one.

/// what the worker should do after a run ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopContinuation {
    /// start another run on the same thread after a delay.
    Restart,
    /// leave the run ended.
    LetEnd,
}

/// may an ended run be followed by another one, automatically?
///
/// - loop mode off: a run is a unit of work with an end. never restart.
/// - reason "stopped": NEVER restart, even in loop mode. stop is the only
///   escape hatch from a wedged run (D9); second-guessing it would trap
///   the user inside a loop they explicitly tried to kill.
/// - the run belongs to a batch: NEVER restart. the batch driver owns the
///   queue and starts the next task itself — a successor run here would
///   race the driver or fire as a ghost after the queue drains.
/// - the user switched to another thread mid-run: restarting there would
///   startle the person typing on the thread they chose. skip; the boot
///   resume machinery still covers genuine interruptions.
/// - anything else (failed, step_limit, completed): restart. these are
///   deaths an attended user would shrug off and relaunch — exactly what
///   an unattended loop needs done for it.
pub fn decide_after_run_end(
    reason: &str,
    loop_mode: bool,
    still_on_thread: bool,
    in_batch: bool,
) -> LoopContinuation {
    if !loop_mode {
        return LoopContinuation::LetEnd;
    }
    if reason == "stopped" {
        return LoopContinuation::LetEnd;
    }
    if in_batch {
        return LoopContinuation::LetEnd;
    }
    if !still_on_thread {
        return LoopContinuation::LetEnd;
    }
    LoopContinuation::Restart
}

/// how far back a resume marker may be trusted. a marker hours old is a
/// pause button; a marker DAYS old is archaeology — auto-resuming it risks
/// resurrecting something the user considers finished, on a machine state
/// that no longer exists. twelve hours comfortably spans an overnight run.
pub const RESUME_MARKER_MAX_AGE_MS: f64 = 12.0 * 3_600_000.0;

/// should boot continue the run this marker describes?
///
/// a future timestamp counts as fresh: the device clock moving backwards
/// between write and read should not cost the user their run.
pub fn resume_marker_is_fresh(interrupted_at_ms: f64, now_ms: f64) -> bool {
    now_ms - interrupted_at_ms <= RESUME_MARKER_MAX_AGE_MS
}

/// bounds automatic restarts so "the loop continues" cannot become a
/// crash loop: a transcript that fails within seconds of every start, or a
/// revoked key, would otherwise cycle run-after-run until the credits run
/// dry. N attempts per rolling window, oldest falling off; a MANUAL run
/// resets it, because a human pressing run is the strongest possible
/// signal the work is still wanted.
pub const RESTART_WINDOW_MS: f64 = 3_600_000.0;
pub const MAX_RESTARTS_PER_WINDOW: u32 = 6;

#[derive(Debug)]
pub struct RestartBudget {
    window_ms: f64,
    max: usize,
    stamps: Vec<f64>,
}

impl RestartBudget {
    pub fn new(window_ms: f64, max: u32) -> Self {
        Self {
            window_ms,
            max: max as usize,
            stamps: Vec::new(),
        }
    }

    /// ask to spend one restart. true = allowed (and recorded); false =
    /// the window is saturated and the loop should stay down.
    pub fn record(&mut self, now_ms: f64) -> bool {
        self.stamps.retain(|t| now_ms - *t <= self.window_ms);
        if self.stamps.len() >= self.max {
            return false;
        }
        self.stamps.push(now_ms);
        true
    }

    /// a manual run clears crash-loop suspicion entirely.
    pub fn reset(&mut self) {
        self.stamps.clear();
    }

    pub fn used(&self) -> usize {
        self.stamps.len()
    }
}

// ---- transcript well-formedness -----------------------------------------
//
// the invariant everything above protects, stated once and checked
// everywhere: every assistant message that carries tool_calls must be
// followed by exactly one tool result per call id before the next
// assistant message. the agent::run exit path asserts this on EVERY exit,
// because a violation is silent until a future run tries to replay the
// history and gets rejected by the api — days later, in a different
// conversation, with nothing pointing back here.

pub fn history_is_well_formed(history: &[Message]) -> Result<(), String> {
    let mut pending: Vec<String> = Vec::new();

    for (i, m) in history.iter().enumerate() {
        match m.role.as_str() {
            "assistant" => {
                if !pending.is_empty() {
                    return Err(format!(
                        "message {i}: a new assistant turn started while {} tool_call(s) were still unanswered",
                        pending.len()
                    ));
                }
                for c in m.tool_calls.iter().flatten() {
                    pending.push(c.id.clone());
                }
            }
            "tool" => {
                let Some(id) = &m.tool_call_id else {
                    return Err(format!("message {i}: tool result without a tool_call_id"));
                };
                match pending.iter().position(|p| p == id) {
                    Some(p) => {
                        pending.remove(p);
                    }
                    None => {
                        return Err(format!(
                            "message {i}: tool result answers unknown call id '{id}'"
                        ))
                    }
                }
            }
            _ => {}
        }
    }

    if !pending.is_empty() {
        return Err(format!(
            "{pending_len} tool_call(s) never received results: {pending:?}",
            pending_len = pending.len()
        ));
    }
    Ok(())
}
