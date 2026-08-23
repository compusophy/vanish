//! the agent worker: everything that is not the dom.
//!
//! this is the whole reason the harness stopped losing work. a worker is not
//! bound to a request, so a run has no deadline; it owns the working tree in
//! opfs, so its edits are durable; and it is off the main thread, so a
//! twenty-minute run never freezes the ui the user needs in order to stop it.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{DedicatedWorkerGlobalScope, MessageEvent};

use crate::agent::llm::Message;
use crate::protocol::{Command, Config, Event, FinishReason, HistoryTurn};

struct WorkerState {
    config: Config,
    history: Vec<Message>,
    running: bool,
    stop_requested: bool,
    /// which thread `history` belongs to. every save is addressed to this id,
    /// so switching threads mid-session cannot write one conversation's
    /// messages into another's file.
    conversation: String,
}

impl Default for WorkerState {
    fn default() -> Self {
        Self {
            config: Config::default(),
            history: Vec::new(),
            running: false,
            stop_requested: false,
            conversation: String::new(),
        }
    }
}

thread_local! {
    static STATE: Rc<RefCell<WorkerState>> = Rc::new(RefCell::new(WorkerState::default()));
}

fn scope() -> Option<DedicatedWorkerGlobalScope> {
    js_sys::global().dyn_into::<DedicatedWorkerGlobalScope>().ok()
}

fn emit(event: Event) {
    let Some(scope) = scope() else { return };
    match event.to_js() {
        Ok(v) => {
            let _ = scope.post_message(&v);
        }
        Err(e) => {
            // an event that cannot be encoded would otherwise vanish, and a
            // silently dropped event is how a ui ends up frozen on "running".
            let fallback = Event::Error {
                thread: String::new(),
                scope: "worker".to_string(),
                message: format!("failed to encode event: {e}"),
            };
            if let Ok(v) = fallback.to_js() {
                let _ = scope.post_message(&v);
            }
        }
    }
}

/// which conversation the worker currently has loaded. used to stamp
/// run-scoped events; empty before boot completes, which the ui treats as
/// "whatever thread is active".
fn conv() -> String {
    STATE.with(|s| s.borrow().conversation.clone())
}

#[wasm_bindgen]
pub fn boot_worker() {
    console_error_panic_hook::set_once();

    let Some(scope) = scope() else {
        return;
    };

    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        let command: Command = match serde_wasm_bindgen::from_value(ev.data()) {
            Ok(c) => c,
            Err(e) => {
                emit(Event::Error {
                    thread: String::new(),
                    scope: "worker".to_string(),
                    message: format!("unrecognised command from ui: {e}"),
                });
                return;
            }
        };
        handle(command);
    });

    scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    // the closure must outlive this function; the worker lives for the life
    // of the page, so leaking it deliberately is the correct lifetime here.
    on_message.forget();

    emit(Event::Ready {
        build: crate::BUILD.to_string(),
    });

    // restore the previous conversation before the first command arrives, so
    // a reload (ota or manual) resumes the thread instead of starting over.
    // this is the fix for ota updates wiping the transcript: the messages now
    // live in opfs, and the feed is rebuilt from them on boot.
    wasm_bindgen_futures::spawn_local(async move {
        let mut index = crate::platform::transcript::load_index().await;
        if index.active.is_empty() {
            // first ever boot, or every thread was deleted: open one so the
            // user always has somewhere to type.
            let _ = crate::platform::transcript::create().await;
            index = crate::platform::transcript::load_index().await;
        }
        let active = index.active.clone();
        STATE.with(|s| s.borrow_mut().conversation = active.clone());

        let saved = crate::platform::transcript::load(&active).await;
        let total = saved.len();

        // without this line the feed would show a history the model cannot
        // see: the first new run would start from an empty context. the
        // restored messages ARE the working context.
        STATE.with(|s| s.borrow_mut().history = saved.clone());

        // the system prompt is rebuilt by the loop; it is not part of what
        // the user sees or needs replayed.
        let turns: Vec<HistoryTurn> = saved
            .into_iter()
            .filter(|m| m.role != "system")
            .map(|m| HistoryTurn {
                tools: m
                    .tool_calls
                    .unwrap_or_default()
                    .iter()
                    .map(|c| {
                        format!("⚡ {} {}", c.function.name, truncate_args(&c.function.arguments))
                    })
                    .collect(),
                content: m.content.filter(|c| !c.is_empty()),
                role: m.role,
            })
            .collect();
        let trimmed = total.saturating_sub(turns.len());
        emit(Event::HistoryRestored { turns, trimmed });
        publish_conversations(&index);

        // loop mode's promise survives its own death: if a loop run was in
        // flight when the page died, continue it. take_loop_resume clears
        // the marker, so a failed resume cannot loop on boot forever.
        //
        // the resume is deferred until Configure arrives: at boot the config
        // in STATE is still empty (credentials travel with that command), so
        // starting now would bounce off the credential check. PENDING_RESUME
        // parks it; the Configure handler picks it up.
        if let Some(marker) = crate::platform::transcript::take_loop_resume().await {
            if marker.conversation == active && !STATE.with(|s| s.borrow().running) {
                emit(Event::Note {
                    thread: active.clone(),
                    text: format!(
                        "↻ loop mode was interrupted by a reload ({}s ago) — resuming once settings load",
                        ((js_sys::Date::now() - marker.interrupted_at) / 1000.0) as u64
                    ),
                });
                PENDING_RESUME.with(|p| *p.borrow_mut() = Some(marker.prompt));
                // surface the pending state in the ui immediately: a reload
                // that lands on broken credentials would otherwise leave the
                // loop silently parked forever.
                emit(Event::Note {
                    thread: active.clone(),
                    text: "⏸ loop resume pending — waiting for working credentials"
                        .to_string(),
                });
                return;
            }
            // stale: wrong thread or already running. drop it rather than
            // injecting a nudge into a conversation that was not expecting one.
            crate::platform::transcript::clear_loop_resume().await;
        }
    });
}

thread_local! {
    /// a loop run waiting for Configure before it can start. see the boot
    /// handler above for why the resume cannot simply fire immediately.
    static PENDING_RESUME: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// shared body of Command::Run and the boot-time loop resume. everything
/// that checks state, emits RunStarted and drives the async run lives here
/// so the two entry points cannot drift.
fn start_run(prompt: String) {
    let config = STATE.with(|s| s.borrow().config.clone());
    if config.openrouter_key.is_empty() {
        emit(Event::Error {
            thread: conv(),
            scope: "config".to_string(),
            message: "no openrouter api key set — open settings and add one".to_string(),
        });
        return;
    }
    if config.github_token.is_empty() || config.repo.is_empty() {
        emit(Event::Error {
            thread: conv(),
            scope: "config".to_string(),
            message: "no github token or repo set — open settings and add them".to_string(),
        });
        return;
    }

    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.running = true;
        st.stop_requested = false;
    });

    emit(Event::RunStarted {
        thread_id: STATE.with(|s| s.borrow().conversation.clone()),
        model: config.model.clone(),
    });

    spawn_run(config, prompt);
}

/// the async half of a run, factored out so both the user-initiated path and
/// the boot-time loop-resume path share it.
///
/// the persist callback checkpoints after every durable unit; writes are
/// serialized through a drain queue so overlapping opfs writes can never
/// land out of order. the final save waits for the queue to drain first.
///
/// the loop marker is written before the run starts and cleared when it
/// ends — completed, stopped, failed, or step limit — so it can only be
/// observed while a loop run genuinely has no worker attached.
fn spawn_run(config: Config, prompt: String) {
    let is_loop = config.loop_mode;
    let conversation_id = STATE.with(|s| s.borrow().conversation.clone());

    if is_loop {
        let marker = crate::platform::transcript::LoopResume {
            conversation: conversation_id.clone(),
            prompt: prompt.clone(),
            interrupted_at: js_sys::Date::now(),
        };
        wasm_bindgen_futures::spawn_local(async move {
            let _ = crate::platform::transcript::set_loop_resume(marker).await;
        });
    }

    wasm_bindgen_futures::spawn_local(async move {
        let mut history = STATE.with(|s| s.borrow().history.clone());

        // mid-run checkpoints. the loop persists after every durable
        // unit (the prompt, each tool result), so a reload — ota or
        // manual — or a crash costs at most the step in flight, not
        // the whole run.
        //
        // the writes are serialized through a single drain queue: a
        // checkpoint arriving while a write is in flight parks its
        // snapshot in `pending`, and the one writer task drains the
        // queue in order. without this, two overlapping opfs writes
        // could complete out of order and an older history would
        // land after a newer one.
        let queue: Rc<RefCell<(Option<Vec<Message>>, bool)>> =
            Rc::new(RefCell::new((None, false)));

        let persist = {
            let queue = queue.clone();
            let conversation = conversation_id.clone();
            move |messages: &[Message]| {
                let snapshot: Vec<Message> = messages.to_vec();
                let mut q = queue.borrow_mut();
                if q.1 {
                    // a write is in flight; the writer will pick
                    // this snapshot up after the current one.
                    q.0 = Some(snapshot);
                    return;
                }
                q.1 = true;
                q.0 = Some(snapshot);
                drop(q);
                let queue = queue.clone();
                let conversation = conversation.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    loop {
                        let snapshot = queue.borrow_mut().0.take();
                        match snapshot {
                            Some(s) => {
                                let _ = crate::platform::transcript::save(
                                    &conversation, &s,
                                )
                                .await;
                            }
                            None => {
                                queue.borrow_mut().1 = false;
                                break;
                            }
                        }
                    }
                });
            }
        };

        let emit_tagged = {
            let conversation = conversation_id.clone();
            move |event: Event| {
                let tagged = match event {
                    Event::StepStarted { step, .. } => Event::StepStarted {
                        thread: conversation.clone(),
                        step,
                    },
                    Event::Reasoning { delta, .. } => Event::Reasoning {
                        thread: conversation.clone(),
                        delta,
                    },
                    Event::Content { delta, .. } => Event::Content {
                        thread: conversation.clone(),
                        delta,
                    },
                    Event::ToolStarted { id, name, args, .. } => Event::ToolStarted {
                        thread: conversation.clone(),
                        id,
                        name,
                        args,
                    },
                    Event::ToolFinished {
                        id,
                        name,
                        ok,
                        result,
                        ..
                    } => Event::ToolFinished {
                        thread: conversation.clone(),
                        id,
                        name,
                        ok,
                        result,
                    },
                    Event::RunFinished { steps, reason, .. } => Event::RunFinished {
                        thread: conversation.clone(),
                        steps,
                        reason,
                    },
                    Event::Error { scope, message, .. } => Event::Error {
                        thread: conversation.clone(),
                        scope,
                        message,
                    },
                    Event::Note { text, .. } => Event::Note {
                        thread: conversation.clone(),
                        text,
                    },
                    other => other,
                };
                emit(tagged);
            }
        };

        let outcome = crate::agent::run(
            &config,
            &prompt,
            &mut history,
            emit_tagged,
            || STATE.with(|s| s.borrow().stop_requested),
            persist,
        )
        .await;

        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.history = history;
            st.running = false;
            st.stop_requested = false;
        });

        // the loop marker must not outlive its run: whatever ended this run,
        // a stale marker would resurrect the loop on every future boot.
        if is_loop {
            crate::platform::transcript::clear_loop_resume().await;
        }

        // flush the updated conversation to opfs *before* reporting
        // the run finished. if the page reloads the moment this card
        // renders, nothing is lost.
        //
        // first, let any in-flight checkpoint finish: the queue's
        // writer task runs on the same microtask queue, so yielding
        // on a resolved promise lets it drain. skipping this wait
        // would allow a slower older snapshot to land after — and
        // overwrite — this authoritative final save.
        while queue.borrow().0.is_some() || queue.borrow().1 {
            let _ = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::new(
                &mut |resolve, _reject| {
                    let _ = resolve.call0(&JsValue::NULL);
                },
            ))
            .await;
        }

        let (saved, conversation) =
            STATE.with(|s| (s.borrow().history.clone(), s.borrow().conversation.clone()));

        // RunFinished goes out BEFORE the transcript save, not after. this
        // ordering was the root cause of the stuck-stop-button bug: the
        // buttons only flip back on this event, and it used to wait behind
        // the opfs queue drain plus the final save — so a slow or wedged
        // write held the dock hostage on "stop" with no run behind it. the
        // save is durability work and belongs after the user-visible
        // transition; a save failure is reported separately below and
        // cannot hold the control state ransom.
        emit(Event::RunFinished {
            thread: conversation_id.clone(),
            steps: outcome.steps,
            reason: outcome.reason,
        });

        let save_result = crate::platform::transcript::save(&conversation, &saved).await;

        // surfaced after RunFinished so a failure never hides the
        // run's own outcome, but never silently either (D4).
        match save_result {
            Ok(()) => {
                // the title is derived from the first prompt, so the
                // sidebar entry only becomes meaningful after a save.
                let index = crate::platform::transcript::load_index().await;
                publish_conversations(&index);
            }
            Err(e) => emit(Event::Error {
                thread: conversation_id.clone(),
                scope: "transcript".to_string(),
                message: format!("could not save the conversation: {e}"),
            }),
        }
    });
}

/// switching or deleting a thread mid-run would pull the context out from
/// under the loop. refuse, and say why, rather than corrupting the run.
fn reject_while_running(action: &str) -> bool {
    let running = STATE.with(|s| s.borrow().running);
    if running {
        emit(Event::Error {
            thread: conv(),
            scope: "conversation".to_string(),
            message: format!("cannot {action} while a run is in progress — press stop first."),
        });
    }
    running
}

/// load a conversation into worker memory and replay it to the feed.
/// shared by SwitchConversation (which then also moves index.active) and
/// Attach (which must not), so the two cannot drift on how adoption works.
async fn adopt_conversation(id: &str) -> crate::platform::transcript::Index {
    let messages = crate::platform::transcript::load(id).await;
    let total = messages.len();
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.conversation = id.to_string();
        st.history = messages.clone();
    });

    let turns = replay_turns(messages);
    emit(Event::HistoryCleared);
    emit(Event::HistoryRestored {
        trimmed: total.saturating_sub(turns.len()),
        turns,
    });

    crate::platform::transcript::load_index().await
}

fn publish_conversations(index: &crate::platform::transcript::Index) {
    emit(Event::Conversations {
        items: index
            .sorted()
            .into_iter()
            .map(|c| crate::protocol::ConversationSummary {
                id: c.id,
                title: c.title,
                count: c.count,
            })
            .collect(),
        active: index.active.clone(),
    });
}

/// collapse stored messages into the display shape the feed replays.
fn replay_turns(messages: Vec<Message>) -> Vec<HistoryTurn> {
    messages
        .into_iter()
        .filter(|m| m.role != "system")
        .map(|m| HistoryTurn {
            tools: m
                .tool_calls
                .unwrap_or_default()
                .iter()
                .map(|c| format!("⚡ {} {}", c.function.name, truncate_args(&c.function.arguments)))
                .collect(),
            content: m.content.filter(|c| !c.is_empty()),
            role: m.role,
        })
        .collect()
}

/// tool arguments can be megabytes of file content; the restored card shows a
/// hint, not the payload. the full text still lives in the message history.
fn truncate_args(args: &str) -> String {
    const MAX: usize = 80;
    let flat = args.replace('\n', " ");
    if flat.chars().count() <= MAX {
        return flat;
    }
    let head: String = flat.chars().take(MAX).collect();
    format!("{head}…")
}

fn handle(command: Command) {
    match command {
        Command::Configure(config) => {
            STATE.with(|s| s.borrow_mut().config = config.clone());

            // credentials are pasted by hand, so the overwhelmingly likely
            // failure is a typo or a token missing a scope. check both now
            // and say exactly which one is wrong, instead of letting it
            // surface as a mysterious failure twenty steps into a run.
            if config.openrouter_key.is_empty() && config.github_token.is_empty() {
                return;
            }

            wasm_bindgen_futures::spawn_local(async move {
                let mut notes: Vec<String> = Vec::new();

                let openrouter_ok = if config.openrouter_key.is_empty() {
                    notes.push("no openrouter key set".to_string());
                    false
                } else {
                    match crate::agent::llm::verify_key(&config.openrouter_key).await {
                        Ok(msg) => {
                            notes.push(msg);
                            true
                        }
                        Err(e) => {
                            notes.push(e);
                            false
                        }
                    }
                };

                let github_ok = if config.github_token.is_empty() || config.repo.is_empty() {
                    notes.push("no github token or repo set".to_string());
                    false
                } else {
                    let gh = crate::agent::github::Github::new(
                        &config.github_token,
                        &config.repo,
                        &config.branch,
                    );
                    match gh.verify().await {
                        Ok(repo) => {
                            notes.push(format!("github ok (write access to {repo})"));
                            true
                        }
                        Err(e) => {
                            notes.push(e);
                            false
                        }
                    }
                };

                // optional: absent is fine and not an error, but a token that
                // is present and broken must be reported now rather than
                // during the incident it exists to diagnose.
                let vercel_ok = if config.vercel_token.trim().is_empty() {
                    notes.push(
                        "no vercel token (build failures will have no compiler output)".to_string(),
                    );
                    None
                } else {
                    let v = crate::agent::vercel::Vercel::new(
                        &config.vercel_token,
                        &config.vercel_team_id,
                        crate::agent::vercel::Vercel::project_from_repo(&config.repo),
                    );
                    match v.verify().await {
                        Ok(msg) => {
                            notes.push(msg);
                            Some(true)
                        }
                        Err(e) => {
                            notes.push(format!("vercel token unusable: {e}"));
                            Some(false)
                        }
                    }
                };

                // the loop resume waits for this exact point; only a working
                // core credential set may release it.
                let core_credentials_usable =
                    openrouter_ok && github_ok;

                emit(Event::ConfigStatus {
                    openrouter_ok,
                    github_ok,
                    vercel_ok,
                    detail: notes.join(" · "),
                });

                // a loop run interrupted by the last reload resumes here:
                // this is the first moment credentials are known good. if
                // verification failed, the pending resume is dropped — an
                // autonomous run on broken credentials would just fail.
                if core_credentials_usable {
                    let resume = PENDING_RESUME.with(|p| p.borrow_mut().take());
                    if let Some(prompt) = resume {
                        if !STATE.with(|s| s.borrow().running) {
                            start_run(prompt);
                        }
                    }
                } else {
                    PENDING_RESUME.with(|p| *p.borrow_mut() = None);
                }
            });
        }

        Command::Stop => {
            STATE.with(|s| s.borrow_mut().stop_requested = true);
        }

        // dock reconciliation. RunFinished crosses the same channel as
        // everything else and can be delayed (it used to wait behind the
        // final transcript save) or lost entirely; when that happens the
        // buttons stay on "stop" with no run behind them. the worker's own
        // `running` flag is ground truth, so a periodic ping corrects any
        // drift within seconds.
        Command::RunState => {
            let running = STATE.with(|s| s.borrow().running);
            emit(Event::RunStateReport { running });
        }

        Command::Run { prompt, thread_id } => {
            let already_running = STATE.with(|s| s.borrow().running);
            if already_running {
                emit(Event::Error {
                    thread: conv(),
                    scope: "run".to_string(),
                    message: "a run is already in progress".to_string(),
                });
                return;
            }

            // switching threads mid-run is refused elsewhere; a run always
            // belongs to the conversation that is active when it starts.
            let _ = thread_id;
            start_run(prompt);
        }

        Command::Commit { message } => {
            let config = STATE.with(|s| s.borrow().config.clone());
            wasm_bindgen_futures::spawn_local(async move {
                let github = crate::agent::github::Github::new(
                    &config.github_token,
                    &config.repo,
                    &config.branch,
                );
                let mut ws = crate::agent::tools::Workspace::new(github).await;
                match ws
                    .dispatch("git_commit", &serde_json::json!({ "message": message }).to_string())
                    .await
                {
                    Ok(payload) => {
                        let v: serde_json::Value =
                            serde_json::from_str(&payload).unwrap_or_default();
                        emit(Event::Committed {
                            sha: v["short_sha"].as_str().unwrap_or_default().to_string(),
                            message,
                            files: v["files"].as_u64().unwrap_or(0) as usize,
                        });
                        emit(Event::TreeChanged { dirty: ws.dirty() });
                    }
                    Err(e) => emit(Event::Error {
                        thread: conv(),
                        scope: "commit".to_string(),
                        message: e,
                    }),
                }
            });
        }

        Command::ListTree => {
            let config = STATE.with(|s| s.borrow().config.clone());
            wasm_bindgen_futures::spawn_local(async move {
                let github = crate::agent::github::Github::new(
                    &config.github_token,
                    &config.repo,
                    &config.branch,
                );
                let index = crate::platform::opfs::load_index().await;
                match github.list_tree().await {
                    Ok(items) => {
                        let entries = items
                            .into_iter()
                            .map(|i| crate::protocol::TreeEntry {
                                dirty: index.get(&i.path).map(|e| e.dirty).unwrap_or(false),
                                is_dir: i.kind == "tree",
                                size: i.size.unwrap_or(0),
                                path: i.path,
                            })
                            .collect();
                        emit(Event::Tree { entries });
                    }
                    Err(e) => emit(Event::Error {
                        thread: conv(),
                        scope: "tree".to_string(),
                        message: e,
                    }),
                }
            });
        }

        Command::ReadFile { path } => {
            let config = STATE.with(|s| s.borrow().config.clone());
            wasm_bindgen_futures::spawn_local(async move {
                let github = crate::agent::github::Github::new(
                    &config.github_token,
                    &config.repo,
                    &config.branch,
                );
                let mut ws = crate::agent::tools::Workspace::new(github).await;
                match ws
                    .dispatch("read_file", &serde_json::json!({ "path": path }).to_string())
                    .await
                {
                    Ok(_) => {
                        // the editor wants the raw bytes, not numbered lines
                        match crate::platform::opfs::read(&path).await {
                            Ok(content) => emit(Event::FileContent { path, content }),
                            Err(e) => emit(Event::Error {
                                thread: conv(),
                                scope: "read".to_string(),
                                message: e,
                            }),
                        }
                    }
                    Err(e) => emit(Event::Error {
                        thread: conv(),
                        scope: "read".to_string(),
                        message: e,
                    }),
                }
            });
        }

        Command::WriteFile { path, content } => {
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = crate::platform::opfs::write(&path, &content).await {
                    emit(Event::Error {
                        thread: conv(),
                        scope: "write".to_string(),
                        message: e,
                    });
                    return;
                }
                let mut index = crate::platform::opfs::load_index().await;
                let entry = index.entry(path.clone()).or_default();
                entry.dirty = true;
                entry.size = content.len();
                if let Err(e) = crate::platform::opfs::save_index(&index).await {
                    emit(Event::Error {
                        thread: conv(),
                        scope: "write".to_string(),
                        message: e,
                    });
                    return;
                }
                emit(Event::TreeChanged {
                    dirty: index
                        .iter()
                        .filter(|(_, e)| e.dirty)
                        .map(|(p, _)| p.clone())
                        .collect(),
                });
            });
        }

        Command::ClearHistory => {
            // memory first, disk second: a crash between the two leaves at
            // worst an orphaned file, never a "cleared" ui sitting on top of
            // a history that will reappear on the next reload.
            STATE.with(|s| s.borrow_mut().history.clear());
            let id = STATE.with(|s| s.borrow().conversation.clone());
            wasm_bindgen_futures::spawn_local(async move {
                // clear empties THIS thread and opens a fresh one; the other
                // threads are untouched.
                match crate::platform::transcript::delete(&id).await {
                    Ok(_) => {
                        if let Ok(new_id) = crate::platform::transcript::create().await {
                            STATE.with(|s| s.borrow_mut().conversation = new_id);
                        }
                        emit(Event::HistoryCleared);
                        let index = crate::platform::transcript::load_index().await;
                        publish_conversations(&index);
                    }
                    Err(e) => emit(Event::Error {
                        thread: conv(),
                        scope: "transcript".to_string(),
                        message: format!("could not clear the conversation: {e}"),
                    }),
                }
            });
        }

        Command::NewConversation => {
            if reject_while_running("start a new conversation") {
                return;
            }
            STATE.with(|s| s.borrow_mut().history.clear());
            wasm_bindgen_futures::spawn_local(async move {
                match crate::platform::transcript::create().await {
                    Ok(id) => {
                        STATE.with(|s| s.borrow_mut().conversation = id);
                        emit(Event::HistoryCleared);
                        let index = crate::platform::transcript::load_index().await;
                        publish_conversations(&index);
                    }
                    Err(e) => emit(Event::Error {
                        thread: conv(),
                        scope: "transcript".to_string(),
                        message: format!("could not start a conversation: {e}"),
                    }),
                }
            });
        }

        Command::SwitchConversation { id } => {
            if reject_while_running("switch conversations") {
                return;
            }
            wasm_bindgen_futures::spawn_local(async move {
                let index = adopt_conversation(&id).await;
                let mut index = index;
                index.active = id;
                let _ = crate::platform::transcript::save_index(&index).await;
                publish_conversations(&index);
            });
        }

        Command::Attach { id } => {
            // deliberately no reject_while_running: Attach targets a fresh,
            // idle worker by construction. refusing here would break the
            // phase-2 spawn flow this command exists to serve.
            if STATE.with(|s| s.borrow().running) {
                emit(Event::Error {
                    thread: conv(),
                    scope: "attach".to_string(),
                    message: "cannot attach while a run is in progress".to_string(),
                });
                return;
            }
            wasm_bindgen_futures::spawn_local(async move {
                // same adoption as a switch — history loaded, feed replayed
                // — minus the global active-id write, which is the point:
                // attaching one worker to one conversation must not yank
                // the ui's notion of "current" away from the user.
                let _index = adopt_conversation(&id).await;
                let _ = _index; // publish_conversations is NOT called: see doc above
            });
        }

        Command::DeleteConversation { id } => {
            if reject_while_running("delete a conversation") {
                return;
            }
            wasm_bindgen_futures::spawn_local(async move {
                match crate::platform::transcript::delete(&id).await {
                    Ok(next) => {
                        let active = if next.is_empty() {
                            crate::platform::transcript::create()
                                .await
                                .unwrap_or_default()
                        } else {
                            next
                        };
                        adopt_conversation(&active).await;
                        let index = crate::platform::transcript::load_index().await;
                        publish_conversations(&index);
                    }
                    Err(e) => emit(Event::Error {
                        thread: conv(),
                        scope: "transcript".to_string(),
                        message: format!("could not delete the conversation: {e}"),
                    }),
                }
            });
        }

        Command::ListConversations => {
            wasm_bindgen_futures::spawn_local(async move {
                let index = crate::platform::transcript::load_index().await;
                publish_conversations(&index);
            });
        }
    }
}

/// exposed so the ui can render a finish reason without duplicating the enum.
pub fn describe_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Completed => "completed",
        FinishReason::Stopped => "stopped by you",
        FinishReason::StepLimit => "hit the step ceiling",
        FinishReason::Failed => "failed",
    }
}
