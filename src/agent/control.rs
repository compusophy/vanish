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
