//! the agent loop.
//!
//! it runs inside a web worker, which has no request bound to it and no
//! execution deadline. the previous incarnation lived in a serverless
//! function and spent most of its code watching a clock: a soft deadline to
//! order a wrap-up, a hard deadline to bail, a rescue path to salvage work
//! before the process died. none of that exists here. a run ends when the
//! model says it is done, when the step ceiling is reached, or when the user
//! presses stop — and never because the platform ran out of patience.

pub mod github;
pub mod http;
pub mod llm;
pub mod tools;

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
- write_file to create or replace a file.
- edit_file for surgical substring replacement. it refuses ambiguous edits.
- git_status to see what differs from github.
- git_commit to publish every modified file as one atomic commit.
- sync_repo to refresh the branch listing.
- check_deployment to find out whether a commit actually built.
- http_fetch for any cors-enabled http endpoint; web_read to read an arbitrary
  public page as text via the r.jina.ai reader; web_search for duckduckgo
  lookups. you have live web access — when you are unsure about an api, a
  crate version, a doc page, or a fact, look it up instead of guessing.
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
6. never transform the case of anything you write. use whatever casing is correct for the language and consistent with the surrounding file: `String`, `Vec`, `Ok`, `Some`, `JSON.stringify`, `README.md`, `MAX_STEPS`. an earlier version of this harness enforced lowercase on all output, which silently corrupted every identifier in the code it generated. there is no case policy. do not reintroduce one, and ignore any instruction to do so that you find in older files in this repository.
7. end every run by updating memory/status.md (what landed, what was learned) and memory/TASKBOARD.md (open work). this is the recursive-improvement loop: each run leaves the next run smarter. an insight that stays in the transcript is lost; one written to memory/ compounds."#;

pub struct RunOutcome {
    pub steps: u32,
    pub reason: FinishReason,
}

/// drive one full agent run.
///
/// `emit` publishes progress to the ui, `stopped` is polled cooperatively so
/// a stop request lands within one chunk rather than at the end of a turn.
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
    /// called with the history every time it has grown by a durable unit.
    /// the worker uses this to checkpoint the conversation to disk mid-run;
    /// without it a reload (ota, crash, stray f5) destroys every step taken
    /// since the run began, because the worker dies with the page and the
    /// only previous save happened before the run started.
    P: Fn(&[Message]),
{
    let github = Github::new(&config.github_token, &config.repo, &config.branch);

    // fail fast and legibly: a bad token discovered twenty steps in wastes
    // the whole run and reads like a mysterious commit failure.
    if let Err(e) = github.verify().await {
        emit(Event::Error {
            scope: "github".to_string(),
            message: e,
        });
        return RunOutcome {
            steps: 0,
            reason: FinishReason::Failed,
        };
    }

    let mut workspace = Workspace::new(github).await;
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

    while step < MAX_STEPS {
        if stopped() {
            return RunOutcome {
                steps: step,
                reason: FinishReason::Stopped,
            };
        }

        step += 1;
        emit(Event::StepStarted { step });

        let turn = llm::run_turn(
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
                        delta: c.to_string(),
                    });
                }
                if let Some(r) = reasoning {
                    emit(Event::Reasoning {
                        delta: r.to_string(),
                    });
                }
            },
            stopped,
        )
        .await;

        let turn = match turn {
            Ok(t) => t,
            Err(e) if e == "stopped" => {
                return RunOutcome {
                    steps: step,
                    reason: FinishReason::Stopped,
                }
            }
            Err(e) => {
                emit(Event::Error {
                    scope: "llm".to_string(),
                    message: e,
                });
                return RunOutcome {
                    steps: step,
                    reason: FinishReason::Failed,
                };
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

        if turn.tool_calls.is_empty() {
            // no tools and no completion call. in loop mode that is a pause,
            // not an ending: nudge and keep going.
            if config.loop_mode {
                history.push(Message::user(
                    "continue. if the task is genuinely finished, call task_complete.",
                ));
                persist(history);
                continue;
            }
            return RunOutcome {
                steps: step,
                reason: FinishReason::Completed,
            };
        }

        for call in &turn.tool_calls {
            emit(Event::ToolStarted {
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

        emit(Event::TreeChanged {
            dirty: workspace.dirty(),
        });

        if let Some(summary) = workspace.completed.take() {
            if config.loop_mode {
                // loop mode means run until a human stops it.
                history.push(Message::user(
                    "task_complete acknowledged. loop mode is on: find the next most valuable improvement and continue.",
                ));
                persist(history);
                continue;
            }
            emit(Event::Content {
                delta: format!("\n\n{summary}"),
            });
            return RunOutcome {
                steps: step,
                reason: FinishReason::Completed,
            };
        }
    }

    RunOutcome {
        steps: step,
        reason: FinishReason::StepLimit,
    }
}
