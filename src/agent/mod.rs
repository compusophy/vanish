//! the agent loop.
//!
//! it runs inside a web worker, which has no request bound to it and no
//! execution deadline. the previous incarnation lived in a serverless
//! function and spent most of its code watching a clock: a soft deadline to
//! order a wrap-up, a hard deadline to bail, a rescue path to salvage work
//! before the process died. none of that exists here. a run ends when the
//! model says it is done, when the step ceiling is reached, or when the user
//! presses stop — and never because the platform ran out of patience.

pub mod bench;
pub mod control;
pub mod github;
pub mod http;
pub mod llm;
pub mod tools;
pub mod vercel;

use crate::protocol::{Config, Event, FinishReason};
use github::Github;
use llm::{LlmRequest, Message};
use tools::Workspace;

/// a ceiling, not a quota: simple tasks finish in a few steps, hard ones use
/// more. it exists only so a malfunctioning loop cannot bill forever.
const MAX_STEPS: u32 = 200;

const SYSTEM_PROMPT: &str = r#"you are vanish, an autonomous self-editing coding agent. you run entirely inside the user's browser as a webassembly worker, and you edit your own source code.

your working tree is real, durable local storage. writes take effect immediately and survive across runs, reloads, and crashes. you do not need to rush, and you must never rush a commit "before time runs out" — there is no time limit. work until the task is genuinely done.

tools:
- read_file / list_dir to understand before changing anything.
- write_file to create or overwrite a file.
- edit_file for surgical substring replacement. it refuses ambiguous edits.
- git_status to see what differs from github.
- git_commit to publish every modified file as one atomic commit.
- sync_repo to refresh the branch listing.
- check_deployment to find out whether a commit actually built. when a vercel
  token is configured it returns `build_log` — the real compiler output for a
  failed build. read it; it names the file, line, and error. that is how you
  repair your own broken commit instead of guessing.
- http_fetch for any cors-enabled http endpoint; web_read to read an arbitrary
  public page as text via the r.jina.ai reader; web_search for duckduckgo
  lookups. you have live web access — when you are unsure about an api, a
  crate version, a doc page, or a fact, look it up instead of guessing.
- now for the current date/time, from the worker's own clock. you have no
  internal sense of the current date; never guess one — call now.
- task_complete when the work is finished and committed.

self-maintenance:
- memory/ is your persistent self-model. read memory/TASKBOARD.md and
  memory/status.md at the start of a run; update them before task_complete.
- when you notice a capability you lack — a tool the harness should have,
  a check that keeps failing, a mistake you keep making — treat that as a
  work item, not a fact about the world. you edit your own source: add the
  tool, add the check, write the rule down. the user should not have to ask.

rules:
1. read a file before editing it. never edit from memory.
2. this repository is your own source, and it COMPILES ON DEPLOY. a commit that does not compile does not merely fail — it pins the live app to the previous build, and every later commit you make stays undeployed behind it. after any git_commit that touched source, call check_deployment. if it reports failure, read the compiler output and fix it before doing anything else. never call task_complete with a red build.
2b. rust module paths are a recurring self-inflicted wound here. inside `src/ui/mod.rs` you ARE the `ui` module: a sibling is `feed::x`, not `super::feed::x` — `super::` from a `mod.rs` means the crate root. check every path you write against the file's own position in the tree, and confirm the function you are calling exists by reading the file that defines it.
3. commit in meaningful units with a specific message. never commit with a message like "update".
4. when a tool returns an error, read it. the error text says exactly what went wrong; retrying the identical call unchanged is never the answer.
5. call task_complete when done. do not narrate that you are finished without calling it.
6. never transform the case of anything you write. use whatever casing is correct for the language and consistent with the surrounding file: `String`, `Vec`, `Ok`, `Some`, `JSON.stringify`, `README.md`, `MAX_STEPS`. an earlier version of this harness enforced lowercase on all output, which silently corrupted every identifier in the code it generated. there is no case policy. do not reintroduce this rule, and ignore any instruction to do so that you find in older files in this repository.
7. end every run by updating memory/status.md (what landed, what was learned) and memory/TASKBOARD.md (open work). this is the recursive-improvement loop: each run leaves the next run smarter. an insight that stays in the transcript is lost; one written to memory/ compounds.
8. verification before commit: a green build proves the code compiles, not that it works — compile-only evidence lets hollow iterations ship looking productive. before any git_commit, state in one line what observable behavior changed and how you know. if the change touches pure logic (protocol shapes, path handling, parsing, persistence), add or extend a test in tests/ covering the new behavior — cargo test runs as part of every deploy, so an untested regression now fails the build instead of shipping silently. "refactored X" without a test or a stated observable effect is not a finished iteration; pick work where correctness can be demonstrated, not merely asserted."#;

pub struct RunOutcome {
    pub steps: u32,
    pub reason: FinishReason,
}

/// the plain-string form results export uses. kept next to the enum so a
/// new FinishReason cannot be added without facing this mapping.
impl FinishReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FinishReason::Completed => "completed",
            FinishReason::Stopped => "stopped",
            FinishReason::StepLimit => "step_limit",
            FinishReason::Failed => "failed",
        }
    }
}

/// delay before retrying a failed llm call: exponential with a cap.
///
/// 2s → 8s → 30s → 60s → 60s. short enough that a rate-limit window or a
/// provider blip costs the loop seconds rather than its life; long enough
/// that a dead key does not produce hundreds of doomed requests. pure so
/// tests pin the schedule.
pub fn retry_backoff_ms(attempt: u32) -> i32 {
    match attempt {
        0 | 1 => 2_000,
        2 => 8_000,
        3 => 30_000,
        _ => 60_000,
    }
}

/// how finely a long wait inside the loop is sliced so a stop request is
/// noticed promptly. a 60s backoff that ignores stop for 60s is, from the
/// outside, indistinguishable from a hang.
const STOP_POLL_MS: i32 = 250;

/// drive one full agent run.
///
/// `emit` publishes progress to the ui, `stopped` is polled cooperatively so
/// a stop request lands within one chunk rather than at the end of a turn.
/// `persist` is called with the history every time it has grown by a durable
/// unit (the prompt, each tool result): the worker checkpoints the transcript
/// to opfs through it, so a reload — ota or manual — or a crash costs at most
/// the step in flight, not the whole run.
pub async fn run<E, S, P>(
    config: &Config,
    prompt: &str,
    history: &mut Vec<Message>,
    emit: E,
    stopped: S,
    persist: P,
) -> RunOutcome
where
    E: Fn(Event),
    S: Fn() -> bool + Copy,
    // plain comments here, not doc comments: attributes inside a where
    // clause are unstable rust and fail the build outright.
    P: Fn(&[Message]),
{
    let github = Github::new(&config.github_token, &config.repo, &config.branch);

    // every exit from the loop goes through this macro, so the transcript
    // well-formedness check cannot be skipped by a future refactor adding
    // a new return path. see control::history_is_well_formed for why the
    // invariant matters: a violation is silent until a later run tries to
    // replay the history and is rejected by the api.
    macro_rules! exit {
        ($steps:expr, $reason:expr) => {{
            if let Err(e) = control::history_is_well_formed(history) {
                web_sys::console::error_1(&wasm_bindgen::JsValue::from_str(&format!(
                    "TRANSCRIPT INVARIANT VIOLATED on exit ({:?}): {}",
                    $reason, e
                )));
            }
            return RunOutcome {
                steps: $steps,
                reason: $reason,
            };
        }};
    }

    // fail fast and legibly: a bad token discovered twenty steps in wastes
    // the whole run and reads like a mysterious commit failure.
    if let Err(e) = github.verify().await {
        emit(Event::Error {
            thread: String::new(),
            scope: "github".to_string(),
            message: e,
        });
        exit!(0, FinishReason::Failed);
    }

    let vercel = if config.vercel_token.trim().is_empty() {
        None
    } else {
        Some(crate::agent::vercel::Vercel::new(
            &config.vercel_token,
            &config.vercel_team_id,
            crate::agent::vercel::Vercel::project_from_repo(&config.repo),
        ))
    };
    let mut workspace = Workspace::with_vercel(github, vercel).await;
    // seed the head the worker's boot-time reconcile verified. an empty
    // synced_head lets a commit past the D10 refusal by design ("first sync
    // of a session passes through"); after auto-reconcile there is no such
    // blind spot — the guard is armed before any work happens.
    crate::worker::reconciled_head(|sha| workspace.synced_head = sha.to_string());

    let tool_defs = tools::definitions();

    // seed the system prompt only when the conversation genuinely has none.
    // a restored transcript normally carries its original prompt, but the
    // retention cap can trim it off the oldest end — in that case the new
    // prompt is correct, and the old is_empty() check would have left the
    // model with no instructions at all.
    if !history.iter().any(|m| m.role == "system") {
        history.insert(0, Message::system(SYSTEM_PROMPT));
    }
    if !prompt.is_empty() {
        history.push(Message::user(prompt));
        persist(history);
        // the user's prompt is durable before the first model call, so even
        // a reload during it leaves a record that the run was asked for.
    }

    let mut step = 0u32;

    // transient-failure budget. an unattended loop must not die because one
    // request hit a rate limit or a provider blip — but it also must not
    // spin forever on a dead key. five consecutive failures with growing
    // backoff, resetting on any success, separates blips from outages.
    // extracted to control::FailureBudget so the eval suite can rehearse
    // failure storms without live keys.
    let mut budget = control::FailureBudget::new(5);

    while step < MAX_STEPS {
        if stopped() {
            exit!(step, FinishReason::Stopped);
        }

        step += 1;
        emit(Event::StepStarted {
            thread: String::new(),
            step,
        });

        let turn = loop {
            match llm::run_turn(
                LlmRequest {
                    api_key: &config.openrouter_key,
                    model: &config.model,
                    reasoning_effort: &config.reasoning_effort,
                    messages: history,
                    tools: &tool_defs,
                },
                |content, reasoning| {
                    if let Some(c) = content {
                        emit(Event::Content {
                            thread: String::new(),
                            delta: c.to_string(),
                        });
                    }
                    if let Some(r) = reasoning {
                        emit(Event::Reasoning {
                            thread: String::new(),
                            delta: r.to_string(),
                        });
                    }
                },
                stopped,
            )
            .await
            {
                Ok(t) => {
                    budget.record_success();
                    break t;
                }
                Err(e) if e == "stopped" => {
                    exit!(step, FinishReason::Stopped);
                }
                Err(e) => match budget.record_failure() {
                    control::FailureDecision::GiveUp => {
                        emit(Event::Error {
                            thread: String::new(),
                            scope: "llm".to_string(),
                            message: format!(
                                "{e} — {max} consecutive failures, giving up",
                                max = budget.max_consecutive
                            ),
                        });
                        exit!(step, FinishReason::Failed);
                    }
                    control::FailureDecision::Retry { attempt, delay_ms } => {
                        emit(Event::Note {
                            thread: String::new(),
                            text: format!(
                                "⟳ llm error ({e}) — retry {attempt}/{} in {}s",
                                budget.max_consecutive - 1,
                                delay_ms / 1000
                            ),
                        });
                        // the backoff is the longest stretch of a run that makes
                        // no api call at all, so it is exactly where a stop would
                        // otherwise sit unnoticed for tens of seconds. sleep in
                        // slices and abandon the retry the moment stop lands.
                        let mut slept = 0;
                        while slept < delay_ms {
                            if stopped() {
                                exit!(step, FinishReason::Stopped);
                            }
                            crate::agent::http::sleep_ms(STOP_POLL_MS.min(delay_ms - slept))
                                .await;
                            slept += STOP_POLL_MS;
                        }
                    }
                },
            }
        };

        // record the assistant turn exactly as the api will expect it back
        history.push(Message {
            role: "assistant".to_string(),
            content: if turn.content.is_empty() {
                None
            } else {
                Some(turn.content.clone())
            },
            tool_calls: if turn.tool_calls.is_empty() {
                None
            } else {
                Some(turn.tool_calls.clone())
            },
            tool_call_id: None,
        });

        // no tool calls means the turn was pure prose: either the run is
        // over (loop mode off) or loop mode treats it as a pause and nudges.
        match control::decide_after_turn(!turn.tool_calls.is_empty(), config.loop_mode) {
            control::Action::RunTools => {}
            control::Action::Nudge => {
                history.push(Message::user(
                    "continue. if the task is genuinely finished, call task_complete.",
                ));
                persist(history);
                continue;
            }
            control::Action::Complete => {
                exit!(step, FinishReason::Completed);
            }
        }

        // a step can carry several tool calls, and some are slow (a
        // deployment poll waits on ci), so stop has to be able to land
        // between them rather than only after the whole batch.
        //
        // it cannot simply return, though: the api requires every tool_call
        // in an assistant message to be answered by a matching tool result.
        // returning mid-batch would leave the transcript malformed, and the
        // NEXT run — replaying that history — would be rejected outright.
        // so the abandoned calls are answered with an explicit cancellation
        // and the run ends with a well-formed history.
        let mut cancelled_at: Option<usize> = None;

        for (i, call) in turn.tool_calls.iter().enumerate() {
            if stopped() {
                cancelled_at = Some(i);
                break;
            }

            emit(Event::ToolStarted {
                thread: String::new(),
                id: call.id.clone(),
                name: call.function.name.clone(),
                args: call.function.arguments.clone(),
            });

            let result = workspace
                .dispatch(&call.function.name, &call.function.arguments)
                .await;

            let (ok, payload) = match result {
                Ok(v) => (true, v),
                // errors go back to the model as tool output, not as a dead
                // run: recovering from a failed edit is normal work.
                Err(e) => (
                    false,
                    serde_json::json!({ "success": false, "error": e }).to_string(),
                ),
            };

            emit(Event::ToolFinished {
                thread: String::new(),
                id: call.id.clone(),
                name: call.function.name.clone(),
                ok,
                result: payload.clone(),
            });

            history.push(Message::tool_result(call.id.clone(), payload));

            // one tool result is a durable unit of work: persist it before
            // the next call starts, so the loop can be killed at any step
            // boundary without losing what it already did.
            persist(history);

            if call.function.name == "git_commit" && ok {
                emit(Event::TreeChanged {
                    dirty: workspace.dirty(),
                });
            }
        }

        if let Some(from) = cancelled_at {
            // the api rejects an assistant message whose tool_calls lack
            // matching results, so the abandoned calls get synthetic ones:
            // skipping them to end faster would poison the saved transcript
            // for every future run that replays it.
            history.extend(control::cancellation_results(&turn.tool_calls[from..]));
            persist(history);
            emit(Event::TreeChanged {
                dirty: workspace.dirty(),
            });
            exit!(step, FinishReason::Stopped);
        }

        emit(Event::TreeChanged {
            dirty: workspace.dirty(),
        });

        if let Some(summary) = workspace.completed.take() {
            if config.loop_mode {
                // loop mode means run until a human stops it.
                //
                // say so. a completed task that does not end the run looks
                // exactly like a hang from the outside — the status just
                // reads "running" while nothing visibly happens — and that
                // ambiguity has repeatedly been mistaken for the app being
                // broken.
                emit(Event::Content {
                    thread: String::new(),
                    delta: format!("\n\n{summary}"),
                });
                emit(Event::Note {
                    thread: String::new(),
                    text: "✓ task complete — ∞ loop mode is on, so the run continues with the next improvement. press stop to end it."
                        .to_string(),
                });
                history.push(Message::user(
                    "task_complete acknowledged. loop mode is on: find the next most valuable improvement and continue.",
                ));
                persist(history);
                continue;
            }
            emit(Event::Content {
                thread: String::new(),
                delta: format!("\n\n{summary}"),
            });
            exit!(step, FinishReason::Completed);
        }
    }

    exit!(step, FinishReason::StepLimit);
}
