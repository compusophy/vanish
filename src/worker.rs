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
use crate::protocol::{Command, Config, Event, FinishReason};

struct WorkerState {
    config: Config,
    history: Vec<Message>,
    running: bool,
    stop_requested: bool,
}

impl Default for WorkerState {
    fn default() -> Self {
        Self {
            config: Config::default(),
            history: Vec::new(),
            running: false,
            stop_requested: false,
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
                scope: "worker".to_string(),
                message: format!("failed to encode event: {e}"),
            };
            if let Ok(v) = fallback.to_js() {
                let _ = scope.post_message(&v);
            }
        }
    }
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

                emit(Event::ConfigStatus {
                    openrouter_ok,
                    github_ok,
                    detail: notes.join(" · "),
                });
            });
        }

        Command::Stop => {
            STATE.with(|s| s.borrow_mut().stop_requested = true);
        }

        Command::Run { prompt, thread_id } => {
            let already_running = STATE.with(|s| s.borrow().running);
            if already_running {
                emit(Event::Error {
                    scope: "run".to_string(),
                    message: "a run is already in progress".to_string(),
                });
                return;
            }

            let config = STATE.with(|s| s.borrow().config.clone());
            if config.openrouter_key.is_empty() {
                emit(Event::Error {
                    scope: "config".to_string(),
                    message: "no openrouter api key set — open settings and add one".to_string(),
                });
                return;
            }
            if config.github_token.is_empty() || config.repo.is_empty() {
                emit(Event::Error {
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
                thread_id,
                model: config.model.clone(),
            });

            wasm_bindgen_futures::spawn_local(async move {
                let mut history = STATE.with(|s| s.borrow().history.clone());

                let outcome = crate::agent::run(
                    &config,
                    &prompt,
                    &mut history,
                    emit,
                    || STATE.with(|s| s.borrow().stop_requested),
                )
                .await;

                STATE.with(|s| {
                    let mut st = s.borrow_mut();
                    st.history = history;
                    st.running = false;
                    st.stop_requested = false;
                });

                emit(Event::RunFinished {
                    steps: outcome.steps,
                    reason: outcome.reason,
                });
            });
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
                                scope: "read".to_string(),
                                message: e,
                            }),
                        }
                    }
                    Err(e) => emit(Event::Error {
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
