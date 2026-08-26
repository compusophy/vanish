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

/// worker state. Default derives: every field's zero value is the real
/// initial value (empty config/history, no run, nothing reconciled yet).
#[derive(Default)]
struct WorkerState {
    config: Config,
    history: Vec<Message>,
    running: bool,
    /// incremented on every run start and on every forced stop-recovery. a
    /// run captures this at birth; once the two diverge that run is a zombie
    /// and must neither keep working nor write its state back. this is what
    /// makes the stop escape hatch below safe — without it, forcing `running`
    /// false would leave the old future alive, invisible, and still billing.
    run_seq: u64,
    stop_requested: bool,
    /// which thread `history` belongs to. every save is addressed to this id,
    /// so switching threads mid-session cannot write one conversation's
    /// messages into another's file.
    conversation: String,
    /// set once the session-level D10 reconcile has run against a verified
    /// github token (the Configure handler). until then every cached file is
    /// guilty until reconciled — the exact mechanism behind the 37-file
    /// staleness discovery, where a fresh session trusted a snapshot that
    /// predated upstream fixes.
    auto_reconciled: bool,
}

thread_local! {
    static STATE: Rc<RefCell<WorkerState>> = Rc::new(RefCell::new(WorkerState::default()));
}

/// how long Stop waits for the run to unwind on its own before taking
/// control back by force. long enough that a healthy run almost always
/// beats it (the loop polls between stream chunks), short enough that a
/// wedged one does not leave the user staring at a dead button.
const STOP_GRACE_MS: i32 = 5_000;

/// ceiling on waiting for in-flight checkpoint writes at the end of a run.
/// this wait is pure durability housekeeping and happens after the ui has
/// already been told the run ended, so a stalled write costs a slightly
/// stale transcript — never the user's control of the app.
const DRAIN_TIMEOUT_MS: i32 = 5_000;

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

    // self-config: the saved credentials are mirrored to opfs by the ui on
    // every save, so the worker can load them itself. without this the
    // harness is blind until a human presses "save settings" in a panel the
    // worker cannot see — which stranded an entire session that could not
    // read its own build logs (the vercel client stayed None because
    // Configure had last been sent before the token was pasted). booting
    // with what is on disk makes every capability that depends on
    // credentials — check_deployment's compiler output, github, openrouter —
    // available from second zero.
    wasm_bindgen_futures::spawn_local(async move {
        // first ever boot reads nothing: not an error, just no mirror yet.
        if let Ok(raw) = crate::platform::opfs::read(crate::protocol::CONFIG_MIRROR_PATH).await {
            match serde_json::from_str::<Config>(&raw) {
                Ok(cfg) => {
                    let has_credentials =
                        !cfg.openrouter_key.is_empty() || !cfg.github_token.is_empty();
                    if has_credentials {
                        STATE.with(|s| s.borrow_mut().config = cfg.clone());
                        emit(Event::Note {
                            thread: String::new(),
                            text: format!(
                                "⚙ config restored from opfs ({}) — self-configured at boot",
                                crate::protocol::CONFIG_MIRROR_PATH
                            ),
                        });
                        // run the same credential verification + auto-reconcile
                        // path a ui-driven Configure would, so boot-time state
                        // (verified tokens, reconciled tree) matches exactly
                        // what a human-pressed save produces.
                        handle(Command::Configure(cfg));
                    } else {
                        emit(Event::Note {
                            thread: String::new(),
                            text: "opfs config mirror holds no usable credentials; \
                                   waiting for the settings panel"
                                .to_string(),
                        });
                    }
                }
                Err(e) => emit(Event::Error {
                    thread: String::new(),
                    scope: "config".to_string(),
                    message: format!(
                        "opfs config mirror did not parse ({e}); it will be \
                         replaced on the next settings save"
                    ),
                }),
            }
        }
    });

    // the reasoning policy comes up alongside the config: it is not a
    // dependency of the loop, so nothing waits on it, but a prompt typed in
    // the first second should already go through whatever module the user
    // last swapped in.
    wasm_bindgen_futures::spawn_local(boot_cognition());
    wasm_bindgen_futures::spawn_local(boot_corpus());

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

        // a batch parked by an earlier boot resumes here — BEFORE the
        // loop-resume marker handling, because the two are independent: a
        // batch can be interrupted BETWEEN tasks (no run in flight, no
        // marker), and take-on-read of the marker must not strand it.
        // unlike the marker this is NOT cleared on read: the queue survives
        // repeated boots until it drains or is cancelled.
        let parked = crate::platform::transcript::get_batch().await;
        match parked.map(|json| (load_batch_from(&json), json)) {
            Some((Some(batch), _)) => {
                emit(Event::Note {
                    thread: conv(),
                    text: format!(
                        "↻ resuming interrupted batch — {} task(s) remaining",
                        batch.tasks.len() - batch.current
                    ),
                });
                BATCH.with(|b| *b.borrow_mut() = Some(batch.clone()));
                wasm_bindgen_futures::spawn_local(drive_batch(batch));
            }
            Some((None, _)) => {
                // unparsable state would otherwise sit there forever; drop it
                // loudly rather than silently (D4).
                crate::platform::transcript::clear_batch().await;
                emit(Event::Error {
                    thread: conv(),
                    scope: "batch".to_string(),
                    message: "a parked batch was unreadable and has been discarded".to_string(),
                });
            }
            None => {}
        }

        // every run's promise now survives its own death, not just loop
        // mode's: if ANY run was in flight when the page died — refresh,
        // ota reload, or the browser discarding a hidden tab (memory saver,
        // mobile os), which kills the worker with no event at all — the
        // next boot continues it instead of leaving a dead card in the feed.
        // take_loop_resume clears the marker, so a failed resume cannot loop
        // on boot forever.
        //
        // the marked conversation is ADOPTED first when it differs from the
        // active one: a tab discarded while the user was on another thread
        // used to drop the marker as "stale" and lose the run outright.
        // adoption loads that thread's history into worker memory so the
        // resumed run has its real context.
        //
        // the resume itself is deferred until Configure arrives: at boot the
        // config in STATE is still empty (credentials travel with that
        // command), so starting now would bounce off the credential check.
        // PENDING_RESUME parks it; the Configure handler picks it up.
        let marker = crate::platform::transcript::take_loop_resume().await;
        let marker = match marker {
            Some(m) => m,
            None => return,
        };

        // a marker hours old is a pause button; one days old is
        // archaeology. auto-resuming it would resurrect something the user
        // reasonably considers finished — the residue of every past attempt
        // to make reloads survivable was exactly such surprise runs. the
        // threshold is pure and pinned (control::resume_marker_is_fresh).
        if !crate::agent::control::resume_marker_is_fresh(
            marker.interrupted_at,
            js_sys::Date::now(),
        ) {
            let age_h = ((js_sys::Date::now() - marker.interrupted_at) / 3_600_000.0) as u64;
            emit(Event::Note {
                thread: conv(),
                text: format!(
                    "↺ an interrupted run ({age_h}h old) was found but is too old to \
                     resume automatically — start it again manually if you want it."
                ),
            });
            return;
        }

        // which conversation should hold the resumed run? refuse to adopt a
        // conversation that no longer exists (a deleted thread must never
        // resurrect as a surprise run) — control::resume_target answers this
        // purely, and the eval suite pins it.
        let index = crate::platform::transcript::load_index().await;
        let existing: Vec<String> = index.items.iter().map(|c| c.id.clone()).collect();
        let target = match crate::agent::control::resume_target(&existing, &marker.conversation)
        {
            Some(t) => t,
            None => {
                // the thread was deleted while the run was interrupted;
                // nothing to continue. the marker is already cleared.
                return;
            }
        };

        // a run that is ALREADY going (the user typed fast, or a previous
        // resume fired) owns the worker: parking another resume behind it
        // would strand the parked prompt forever, so the interrupted run is
        // dropped rather than queued.
        if STATE.with(|s| s.borrow().running) {
            return;
        }

        let resumed_here = target == active;
        if !resumed_here {
            adopt_conversation(&target).await;
            let mut index = index;
            index.active = target.clone();
            let _ = crate::platform::transcript::save_index(&index).await;
            publish_conversations(&index);
        }

        let kind = if marker.loop_mode { "loop mode" } else { "run" };
        emit(Event::Note {
            thread: target.clone(),
            text: format!(
                "↻ {kind} was interrupted ({ago}s ago) — resuming once settings load",
                ago = ((js_sys::Date::now() - marker.interrupted_at) / 1000.0) as u64
            ),
        });
        PENDING_RESUME.with(|p| *p.borrow_mut() = Some(marker.prompt));
    });
}

thread_local! {
    /// a loop run waiting for Configure before it can start. see the boot
    /// handler above for why the resume cannot simply fire immediately.
    static PENDING_RESUME: RefCell<Option<String>> = const { RefCell::new(None) };
    /// the branch head the boot-time reconcile discovered. every run's
    /// workspace is seeded with it, so the FIRST commit of a session is
    /// D10-guarded instead of sailing through with an empty synced_head —
    /// which was exactly when a stale-tree commit could slip past the guard.
    static RECONCILED_HEAD: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// the conversation id of THIS run, read by agent::run so path claims are
/// attributed to the right thread (STACKED_PRS_PLAN §2 C1). empty before
/// boot completes; an unknown owner still gets its own claims recorded.
pub fn active_conversation(f: impl FnOnce(&str)) {
    let id = STATE.with(|s| s.borrow().conversation.clone());
    f(&id);
}

/// called when a run ends: its claims must stop contesting paths, or
/// finished work would warn future writes forever.
pub fn release_run_claims(conversation_id: &str) -> Vec<String> {
    let released = crate::agent::claims::registry_release_conversation(conversation_id);
    if !released.is_empty() {
        emit(Event::Note {
            thread: conversation_id.to_string(),
            text: format!(
                "released {} path claim(s): {:?}",
                released.len(),
                released
            ),
        });
    }
    released
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

    let seq = STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.running = true;
        st.stop_requested = false;
        st.run_seq = st.run_seq.wrapping_add(1);
        st.run_seq
    });

    emit(Event::RunStarted {
        thread_id: STATE.with(|s| s.borrow().conversation.clone()),
        model: config.model.clone(),
    });

    spawn_run(config, prompt, seq);
}

/// the async half of a run, factored out so both the user-initiated path and
/// the boot-time resume path share it.
///
/// the persist callback checkpoints after every durable unit; writes are
/// serialized through a drain queue so overlapping opfs writes can never
/// land out of order. the final save waits for the queue to drain first.
///
/// a resume marker is written before EVERY run starts and cleared when it
/// ends — completed, stopped, failed, or step limit — so it can only be
/// observed while a run genuinely has no worker attached. this is what
/// makes an interrupted run resumable: the browser may discard a hidden
/// tab (and with it the whole worker) without firing any event, and the
/// marker is the only trace left to continue from.
fn spawn_run(config: Config, prompt: String, seq: u64) {
    let is_loop = config.loop_mode;
    let conversation_id = STATE.with(|s| s.borrow().conversation.clone());
    // kept aside for the automatic loop-continuation below; the async body
    // moves everything else it captures.
    let prompt_for_restart = prompt.clone();
    /// pause between an ended run and its automatic successor: long enough
    /// for the final saves and ui transitions of the dead run to land, short
    /// enough that an overnight loop loses minutes, not hours.
    const RESTART_DELAY_MS: i32 = 5_000;

    // EVERY run writes a resume marker, not just loop mode. loop mode needs
    // it because its runs are unbounded — but so does any run that outlives
    // its renderer: the browser discards hidden tabs (memory saver, mobile
    // os) without firing an event, and the only trace left is this marker
    // plus the per-step transcript checkpoints. without it, an interrupted
    // plain run came back from a tab discard as a dead card in the feed.
    let marker = crate::platform::transcript::LoopResume {
        conversation: conversation_id.clone(),
        prompt: prompt.clone(),
        interrupted_at: js_sys::Date::now(),
        loop_mode: is_loop,
    };
    wasm_bindgen_futures::spawn_local(async move {
        let _ = crate::platform::transcript::set_loop_resume(marker).await;
    });

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
            // two ways to be told to stop. the obvious one is the user
            // pressing stop. the second is being superseded: the stop escape
            // hatch bumps run_seq to hand control back to the user, and a run
            // whose seq no longer matches is a zombie — it must unwind rather
            // than keep streaming tokens nobody is watching.
            move || {
                STATE.with(|s| {
                    let st = s.borrow();
                    st.run_seq != seq || st.stop_requested
                })
            },
            persist,
            &CartridgeReasoning,
        )
        .await;

        // snapshot the finished history ONCE, before ownership moves into
        // the write-back below — this is what the final transcript save
        // persists, addressed to the run's own conversation.
        let history_snapshot = history.clone();

        // a run that was force-recovered (or replaced) no longer speaks for
        // the worker: clearing `running` from here would cancel whatever run
        // took its place, and writing its history back would clobber state
        // that has moved on.
        let superseded = STATE.with(|s| s.borrow().run_seq != seq);

        STATE.with(|s| {
            let mut st = s.borrow_mut();
            // write-back is guarded twice: by the seq above, and by the
            // conversation. the run's history returns to worker memory only
            // if it is still the current run AND the user is still on its
            // conversation. switching threads mid-run loads a different
            // conversation into st.history; blindly overwriting it here would
            // pour thread A's finished messages into whatever thread B has on
            // screen — and the next save would corrupt B's transcript file.
            // this guard, together with persist() being addressed by
            // conversation_id, is what makes mid-run switching safe rather
            // than forbidden.
            if !superseded && st.conversation == conversation_id {
                st.history = history;
            } else {
                // the user moved on; park the run's history back in its own
                // transcript file so nothing is lost, and leave B untouched.
                let parked = history;
                wasm_bindgen_futures::spawn_local({
                    let conversation_id = conversation_id.clone();
                    async move {
                        if let Err(e) =
                            crate::platform::transcript::save(&conversation_id, &parked).await
                        {
                            emit(Event::Error {
                                thread: conversation_id,
                                scope: "transcript".to_string(),
                                message: format!(
                                    "could not save the switched-away conversation: {e}"
                                ),
                            });
                        }
                    }
                });
            }
            // only the current run may hand control back. a zombie clearing
            // these would cancel a live run it has nothing to do with.
            if !superseded {
                st.running = false;
                st.stop_requested = false;
            }
        });

        // the resume marker must not outlive its run: whatever ended this
        // run — completed, stopped, failed, step limit — a stale marker
        // would resurrect it on the next boot as an involuntary run.
        // EVERY run writes one now, so EVERY run clears one here; keeping
        // the old is_loop gate would strand markers behind finished runs.
        crate::platform::transcript::clear_loop_resume().await;

        // and this run's path claims must not outlive it either: a finished
        // conversation holding a claim would contest every future write to
        // that path as a ghost. released paths are surfaced so the model can
        // see what stopped being contested.
        release_run_claims(&conversation_id);

        // RunFinished goes out FIRST — before the checkpoint drain, before
        // the final save. the buttons only flip back on this event, so
        // anything placed in front of it can hold the dock hostage on
        // "stop" with no run behind it. durability work belongs after the
        // user-visible transition; a save failure is reported separately
        // below and never holds the control state ransom.
        //
        // a superseded run stays silent: the stop hatch already reported the
        // ending, and a second RunFinished would just print a duplicate card.
        if !superseded {
            emit(Event::RunFinished {
                thread: conversation_id.clone(),
                steps: outcome.steps,
                reason: outcome.reason,
            });
            // batch bookkeeping: record how THIS run ended so the batch
            // driver can advance its queue. a superseded (force-stopped)
            // run stays silent and lets the driver see the seq bump instead.
            LAST_FINISH.with(|f| {
                *f.borrow_mut() = Some(outcome.reason.as_str().to_string());
            });
        }

        // let any in-flight checkpoint finish before the authoritative final
        // save, so a slower older snapshot cannot land after it and win.
        //
        // this MUST yield through a timer. awaiting an already-resolved
        // promise only drains the microtask queue, and re-queueing one every
        // iteration is an endless microtask chain that starves the event loop
        // outright: the opfs write being waited on can never reach its
        // completion callback, so `q.1` never clears and the loop spins
        // forever at 100% of a core. worse, a starved event loop stops
        // dispatching `onmessage` — so Stop, RunState and every later Run are
        // never even seen, and the whole app is bricked with no feedback.
        // that is the bug this comment exists to prevent a third time; it was
        // fixed in 699ada0 and silently reverted by 016f3db.
        //
        // the wait is also bounded: a checkpoint that never completes must
        // not cost the user their transcript save.
        let mut waited_ms = 0;
        while (queue.borrow().0.is_some() || queue.borrow().1) && waited_ms < DRAIN_TIMEOUT_MS {
            crate::agent::http::sleep_ms(10).await;
            waited_ms += 10;
        }

        // final save is addressed to the RUN'S conversation, not to whatever
        // STATE currently holds — the user may have switched threads mid-run,
        // and saving to the wrong file would corrupt the visible thread.
        // (the in-memory write-back above is guarded the same way.)
        let save_result =
            crate::platform::transcript::save(&conversation_id, &history_snapshot).await;

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

        // ---- automatic loop continuation --------------------------------
        //
        // loop mode promises run-until-stopped. a failure budget, a step
        // ceiling, or even a clean completion is not the user stopping —
        // those endings used to simply strand an unattended loop, and it
        // died quietly in the night looking like a hang. the decision is
        // pure (control::decide_after_run_end) and pinned by evals.
        //
        // a superseded run stays dead: its stop hatch already reported the
        // ending, and continuing from it would fight whatever took control.
        // a run started by the batch driver belongs to the queue: the
        // driver advances it, and an automatic successor here would race
        // it or fire as a ghost after the batch drains.
        let in_batch = BATCH.with(|b| b.borrow().is_some());
        let still_on_thread = STATE.with(|s| s.borrow().conversation == conversation_id);
        let continuation = if !superseded {
            crate::agent::control::decide_after_run_end(
                outcome.reason.as_str(),
                config.loop_mode,
                still_on_thread,
                in_batch,
            )
        } else {
            crate::agent::control::LoopContinuation::LetEnd
        };
        if continuation == crate::agent::control::LoopContinuation::Restart {
            // crash-loop breaker: N automatic restarts per rolling window,
            // then the loop stays down until a manual run resets it. this is
            // what keeps "the loop continues" from becoming a billing pump.
            let allowed = RESTART_BUDGET.with(|b| b.borrow_mut().record(js_sys::Date::now()));
            if !allowed {
                emit(Event::Note {
                    thread: conversation_id.clone(),
                    text: format!(
                        "∞ loop paused — {} restart(s) in the last {}h all failed or ended early. \
                         press run to resume the loop; nothing was lost.",
                        crate::agent::control::MAX_RESTARTS_PER_WINDOW,
                        (crate::agent::control::RESTART_WINDOW_MS / 3_600_000.0) as u64,
                    ),
                });
                return;
            }

            let delay = RESTART_DELAY_MS;
            let conv = conversation_id.clone();
            let prompt = prompt_for_restart.clone();
            emit(Event::Note {
                thread: conversation_id.clone(),
                text: format!(
                    "∞ loop mode continues — restarting in {}s (previous run ended: {})",
                    delay / 1000,
                    outcome.reason.as_str()
                ),
            });
            wasm_bindgen_futures::spawn_local(async move {
                crate::agent::http::sleep_ms(delay).await;
                // re-check both guards after the wait: the user may have
                // stopped or switched threads during it. start_run performs
                // its own credential check, so no duplication here.
                let (running, on_thread) =
                    STATE.with(|s| (s.borrow().running, s.borrow().conversation == conv));
                if running || !on_thread {
                    return;
                }
                start_run(prompt);
            });
        }
    });
}

thread_local! {
    /// crash-loop breaker state for automatic loop continuations. reset by
    /// every MANUAL run (Command::Run), because a human pressing the button
    /// is the strongest possible signal the work is still wanted.
    static RESTART_BUDGET: RefCell<crate::agent::control::RestartBudget> = RefCell::new(
        crate::agent::control::RestartBudget::new(
            crate::agent::control::RESTART_WINDOW_MS,
            crate::agent::control::MAX_RESTARTS_PER_WINDOW,
        ),
    );
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

/// the branch head recorded by this session's boot-time reconcile, if it ran.
/// `agent::run` seeds every workspace with it so a session's first commit is
/// D10-guarded rather than waved through. a function, not a pub thread_local,
/// so the accessor cannot drift from the state.
pub fn reconciled_head(with: impl FnOnce(&str)) {
    RECONCILED_HEAD.with(|h| {
        if let Some(sha) = h.borrow().as_ref() {
            with(sha);
        }
    });
}

// ---- batch driver ---------------------------------------------------------
//
// the programmatic work-submission path. a harness calls enqueue_batch (or
// the ui posts Command::RunBatch); tasks run sequentially, each as its own
// one-shot run ending at task_complete. results are exported to opfs and
// announced via Event::BatchFinished.

thread_local! {
    /// the live batch queue. persisted to the transcript index on every
    /// transition, so a tab discard resumes it on next boot — the same
    /// durability single runs get from LoopResume.
    static BATCH: RefCell<Option<crate::agent::control::BatchState>> =
        const { RefCell::new(None) };
}

const BATCH_RESULTS_PATH: &str = "vanish-batch/results.json";

/// export results so far. called after every task AND at cancel time, so the
/// file is always current — an external harness can poll it mid-batch.
fn write_batch_results(batch: &crate::agent::control::BatchState) {
    let payload = serde_json::json!({
        "results": batch.results(),
        "remaining": batch.tasks.len().saturating_sub(batch.current),
    });
    let body = match serde_json::to_string_pretty(&payload) {
        Ok(b) => b,
        Err(e) => {
            emit(Event::Error {
                thread: conv(),
                scope: "batch".to_string(),
                message: format!("could not serialize batch results: {e}"),
            });
            return;
        }
    };
    let thread = conv();
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = crate::platform::opfs::write(BATCH_RESULTS_PATH, &body).await {
            emit(Event::Error {
                thread,
                scope: "batch".to_string(),
                message: format!("could not write batch results: {e}"),
            });
        }
    });
}

/// persist + remember the queue state. every transition goes through here.
fn save_batch(batch: &crate::agent::control::BatchState) {
    BATCH.with(|b| *b.borrow_mut() = Some(batch.clone()));
    match serde_json::to_string(batch) {
        Ok(json) => {
            let json = json.to_string();
            wasm_bindgen_futures::spawn_local(async move {
                let _ = crate::platform::transcript::set_batch(&json).await;
            });
        }
        Err(e) => emit(Event::Error {
            thread: conv(),
            scope: "batch".to_string(),
            message: format!("could not persist batch state: {e}"),
        }),
    }
}

fn load_batch_from(json: &str) -> Option<crate::agent::control::BatchState> {
    serde_json::from_str(json).ok()
}

/// end the batch: announce, export, clear memory and disk.
async fn finish_batch(status: &str) {
    let results = BATCH.with(|b| {
        b.borrow().as_ref().map(crate::agent::control::BatchState::results)
    });
    BATCH.with(|b| *b.borrow_mut() = None);
    crate::platform::transcript::clear_batch().await;
    emit(Event::Note {
        thread: conv(),
        text: format!(
            "☑ batch {status} — {} task(s) recorded, full results at {BATCH_RESULTS_PATH}",
            results.as_ref().map(Vec::len).unwrap_or(0)
        ),
    });
    emit(Event::BatchFinished {
        status: status.to_string(),
        results: results.unwrap_or_default(),
    });
}

/// run the queued tasks one by one. each is a fresh one-shot run through
/// start_run — the SAME path as a typed prompt, so batch behavior cannot
/// diverge from interactive behavior — and its outcome lands back in the
/// batch state. stop, cooperative or forced, cancels whatever has not
/// started yet; failures and step-limits are recorded and the queue continues.
async fn drive_batch(mut batch: crate::agent::control::BatchState) {
    while let Some(prompt) = batch.next_prompt() {
        // wait until the worker is idle: a stale parked batch must never fire
        // into a busy worker or yank control from a user's own run.
        while STATE.with(|s| s.borrow().running) {
            crate::agent::http::sleep_ms(500).await;
        }

        batch.mark_running();
        save_batch(&batch);

        emit(Event::Note {
            thread: conv(),
            text: format!(
                "▶ batch task {}/{} starting",
                batch.current + 1,
                batch.tasks.len()
            ),
        });

        start_run(prompt);

        // capture the seq of the run we just started, then wait for THIS run
        // to end. a forced stop bumps run_seq; seeing that means the run was
        // killed out from under the batch and the whole queue is dead.
        let my_seq = STATE.with(|s| s.borrow().run_seq);
        loop {
            crate::agent::http::sleep_ms(500).await;
            let (running, seq) = STATE.with(|s| (s.borrow().running, s.borrow().run_seq));
            if !running || seq != my_seq {
                break;
            }
        }

        let superseded = STATE.with(|s| s.borrow().run_seq != my_seq);
        let reason = if superseded {
            "stopped".to_string()
        } else {
            LAST_FINISH.with(|f| f.borrow_mut().take()).unwrap_or_else(|| "failed".to_string())
        };

        batch.complete_current(&reason);
        save_batch(&batch);
        write_batch_results(&batch);

        if reason == "stopped" {
            finish_batch("cancelled").await;
            return;
        }
    }

    finish_batch("completed").await;
}

thread_local! {
    /// the FinishReason of the most recent completed run, set by spawn_run
    /// and consumed by drive_batch. a channel of one.
    static LAST_FINISH: RefCell<Option<String>> = const { RefCell::new(None) };
}

// ---- internal eval suite --------------------------------------------------
//
// pinned self-edit tasks driven through drive_batch, then graded against
// mechanical checkers over a snapshot of observable facts. grading lives in
// agent::bench (pure); this is only io and sequencing.

/// did a git commit land since the benchmark started? the commit event is
/// emitted by the tool layer; watching the feed would be fragile, so this
/// snapshot records only file facts today (has_commit stays a parameter
/// for when commit observation lands).
async fn snapshot_bench(commit_seen_before: bool) -> crate::agent::bench::BenchSnapshot {
    let mut files = std::collections::BTreeMap::new();
    for path in crate::agent::bench::checked_paths() {
        // an unreadable file is indistinguishable from a missing one for
        // checker purposes: both fail FileExists.
        if let Ok(body) = crate::platform::opfs::read(path).await {
            files.insert(path.to_string(), body);
        }
    }
    crate::agent::bench::BenchSnapshot {
        files,
        test_count: 0,
        has_commit: commit_seen_before,
    }
}

/// run the whole eval suite. writes vanish-bench/report.json so the score
/// survives a reload; the return value is for callers that want the report
/// inline (spawn_local discards it).
async fn run_benchmark_suite() {
    let tasks: Vec<crate::protocol::BatchTask> = crate::agent::bench::bench_tasks()
        .iter()
        .map(|t| crate::protocol::BatchTask {
            id: t.id.to_string(),
            prompt: t.prompt.to_string(),
        })
        .collect();

    emit(Event::Note {
        thread: conv(),
        text: format!(
            "benchmark starting — {} pinned task(s); each runs as its own one-shot",
            tasks.len()
        ),
    });

    handle(Command::RunBatch { tasks });
    // wait for the batch to end (completed or cancelled) before grading:
    // grading mid-batch would read half-written files.
    while BATCH.with(|b| b.borrow().is_some()) {
        crate::agent::http::sleep_ms(500).await;
    }

    let snap = snapshot_bench(false).await;
    let report = crate::agent::bench::grade_all(&snap);

    let body = serde_json::to_string_pretty(&report)
        .unwrap_or_else(|_| "{{\"error\":\"unserializable report\"}}".to_string());
    if let Err(e) = crate::platform::opfs::write("vanish-bench/report.json", &body).await {
        emit(Event::Error {
            thread: conv(),
            scope: "bench".to_string(),
            message: format!("could not write benchmark report: {e}"),
        });
    }

    emit(Event::Note {
        thread: conv(),
        text: format!(
            "— scorecard —\n{}",
            report.scorecard()
        ),
    });
    emit(Event::BenchmarkFinished {
        passed: report.passed(),
        total: report.total(),
    });
}

// ---- the reasoning cartridge (CARTRIDGE_PLAN §12, build item 8b) --------
//
// the agent loop's policy is a cartridge, and this is where it lives in the
// browser: one `Cognition<MemHost>` owned by the worker, booted from opfs,
// consulted around every prompt and every answer, and replaceable from the
// ui mid-conversation.
//
// three rules shape the glue below. it is never on the critical path — a
// missing or broken policy is a passthrough, not an error (article iv, D9).
// it never reads a clock itself: the worker sets `now` per hook so every
// decision inside a cartridge is reproducible from its inputs (D1). and
// nothing it remembers stays only in memory: each hook's dirty keys are
// flushed to opfs immediately afterwards (D2), the same write-behind shape
// the transcript checkpoint uses.

thread_local! {
    static COGNITION: RefCell<Option<crate::cartridges::Cognition<crate::cartridges::MemHost>>> =
        const { RefCell::new(None) };
}

thread_local! {
    /// every candidate program this browser has seen, with the runtime's
    /// verdict on it (CARTRIDGE_PLAN §9). loaded at boot, written back after
    /// every swap attempt — including the refused ones, which are the only
    /// record of where the boundary actually is.
    static CORPUS: RefCell<crate::cartridges::Corpus> =
        RefCell::new(crate::cartridges::Corpus::new());
}

/// read the corpus back at boot. a corpus that does not parse is reported
/// and started fresh: losing training data is survivable, refusing to boot
/// the loop over it is not (D4).
async fn boot_corpus() {
    let path = crate::cartridges::corpus_path();
    let Ok(text) = crate::platform::opfs::read(&path).await else {
        return;
    };
    match crate::cartridges::Corpus::decode(&text) {
        Ok(c) => {
            let (n, v) = (c.samples.len(), c.verified());
            CORPUS.with(|slot| *slot.borrow_mut() = c);
            if n > 0 {
                emit(Event::Note {
                    thread: String::new(),
                    text: format!("📚 corpus restored: {n} program(s), {v} verified"),
                });
            }
        }
        Err(e) => emit(Event::Error {
            thread: String::new(),
            scope: "corpus".to_string(),
            message: format!("the cartridge corpus did not parse ({e}) — starting a fresh one"),
        }),
    }
}

/// what the corpus looks like right now, for a feed note or a tool result.
pub struct CorpusStats {
    pub samples: usize,
    pub verified: usize,
    pub refused: usize,
    pub top_ops: Vec<(String, usize)>,
}

/// record one candidate and write the corpus back. the write is spawned,
/// like every other durable write in this file: the swap path is
/// synchronous and must not wait on opfs (D2's write-behind shape).
fn record_sample(sample: crate::cartridges::Sample) -> CorpusStats {
    let (stats, body) = CORPUS.with(|slot| {
        let mut c = slot.borrow_mut();
        c.record(sample);
        (
            CorpusStats {
                samples: c.samples.len(),
                verified: c.verified(),
                refused: c.refused(),
                top_ops: c.top_ops(5),
            },
            c.encode(),
        )
    });
    wasm_bindgen_futures::spawn_local(async move {
        let path = crate::cartridges::corpus_path();
        if let Err(e) = crate::platform::opfs::write(&path, &body).await {
            emit(Event::Error {
                thread: String::new(),
                scope: "corpus".to_string(),
                message: format!("could not persist the cartridge corpus to {path}: {e}"),
            });
        }
    });
    stats
}

/// compile and instantiate the reasoning policy: whatever the user last
/// swapped in, or the built-in reference v1.
///
/// a SAVED policy that no longer compiles falls back to v1 loudly rather
/// than leaving the worker with no policy at all — the failure the user
/// needs to see is "your module broke", not a silently degraded loop.
async fn boot_cognition() {
    use crate::cartridges as carts;
    let slug = carts::REASONER_SLUG;

    let saved_source = crate::platform::opfs::read(&carts::source_path(slug)).await.ok();
    let saved_manifest = crate::platform::opfs::read(&carts::manifest_path(slug))
        .await
        .unwrap_or_default();
    let kv = crate::platform::opfs::read(&carts::kv_path(slug)).await.ok();

    let from_disk = saved_source.is_some();
    let source =
        saved_source.unwrap_or_else(|| crate::cartridges::cognitive::REASONING_V1.to_string());

    // which module the worker ENDED UP running, which is not always the one
    // it set out to run — saying "your swap is live" after falling back to
    // the reference module would be the exact kind of confident-and-wrong
    // status line this project keeps deleting.
    let mut fell_back = false;
    let booted =
        carts::boot_reasoner(&saved_manifest, &source, kv.as_deref(), carts::REASONER_FUEL);
    let (cog, mut notes) = match booted {
        Ok(ok) => ok,
        Err(e) => {
            emit(Event::Error {
                thread: String::new(),
                scope: "cartridge".to_string(),
                message: format!("reasoning policy did not boot: {e}"),
            });
            if !from_disk {
                return;
            }
            // the swapped-in module is the suspect; the reference one is
            // known-good, so the loop keeps a policy either way.
            fell_back = true;
            match carts::boot_reasoner(
                "",
                crate::cartridges::cognitive::REASONING_V1,
                kv.as_deref(),
                carts::REASONER_FUEL,
            ) {
                Ok(ok) => ok,
                Err(e) => {
                    emit(Event::Error {
                        thread: String::new(),
                        scope: "cartridge".to_string(),
                        message: format!(
                            "the reference reasoning policy did not boot either ({e}) — the \
                             loop will run with prompts unshaped"
                        ),
                    });
                    return;
                }
            }
        }
    };

    notes.insert(
        0,
        format!(
            "🧠 reasoning policy '{slug}' up ({})",
            match (from_disk, fell_back) {
                (_, true) => "reference v1 — your saved module did not compile",
                (true, false) => "your last hot-swap, restored from opfs",
                (false, false) => "reference v1",
            }
        ),
    );
    COGNITION.with(|c| *c.borrow_mut() = Some(cog));
    // cart_init may already have written to kv; it is durable before the
    // first prompt, not after it.
    flush_cognition(take_cognition_flushes());
    for text in notes {
        emit(Event::Note {
            thread: String::new(),
            text,
        });
    }
}

fn take_cognition_flushes() -> Vec<crate::cartridges::KvFlush> {
    COGNITION.with(|c| {
        c.borrow_mut()
            .as_mut()
            .map(|cog| cog.take_flushes())
            .unwrap_or_default()
    })
}

/// write-behind: every key a cartridge touched during a hook reaches opfs
/// right after it, without the hook (which is synchronous) waiting on it.
fn flush_cognition(flushes: Vec<crate::cartridges::KvFlush>) {
    for f in flushes {
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = crate::platform::opfs::write(&f.path, &f.body).await {
                emit(Event::Error {
                    thread: String::new(),
                    scope: "cartridge".to_string(),
                    message: format!(
                        "could not persist '{}' memory ({}) to {}: {e}",
                        f.slug,
                        f.keys.join(", "),
                        f.path
                    ),
                });
            }
        });
    }
}

/// the loop's view of the cartridge. every method is total: with no policy
/// loaded it behaves exactly like `NoReasoning`.
struct CartridgeReasoning;

impl crate::agent::Reasoning for CartridgeReasoning {
    fn before(&self, prompt: &str) -> crate::cartridges::Shaped {
        let now = js_sys::Date::now() as i64;
        let (shaped, flushes) = COGNITION.with(|c| {
            let mut slot = c.borrow_mut();
            let Some(cog) = slot.as_mut() else {
                return (
                    crate::cartridges::Shaped {
                        prompt: prompt.to_string(),
                        notes: Vec::new(),
                    },
                    Vec::new(),
                );
            };
            cog.set_now(now);
            let mut shaped = cog.before(prompt, now);
            shaped.notes.extend(cog.take_logs());
            let flushes = cog.take_flushes();
            (shaped, flushes)
        });
        flush_cognition(flushes);
        shaped
    }

    fn after(&self, answer: &str) -> Vec<String> {
        let now = js_sys::Date::now() as i64;
        let (notes, flushes) = COGNITION.with(|c| {
            let mut slot = c.borrow_mut();
            let Some(cog) = slot.as_mut() else {
                return (Vec::new(), Vec::new());
            };
            cog.set_now(now);
            let mut notes = cog.after(answer, now);
            notes.extend(cog.take_logs());
            let flushes = cog.take_flushes();
            (notes, flushes)
        });
        flush_cognition(flushes);
        notes
    }
}

/// what a completed policy swap looks like to whoever asked for it — the
/// ui, or the agent through the `swap_cartridge` tool.
pub struct PolicySwap {
    pub slug: String,
    pub rehearsal: crate::cartridges::Rehearsal,
    /// feed lines the swap produced (the Swapped event, the new module's
    /// own init log).
    pub notes: Vec<String>,
    /// set when the new module is LIVE but could not be written to opfs:
    /// it will not survive a reload, and saying otherwise would be a lie
    /// the next boot exposes.
    pub save_error: Option<String>,
    /// the corpus AFTER this attempt was recorded. a refused swap has one
    /// of these too — that is the point of recording refusals.
    pub corpus: CorpusStats,
}

/// the swap itself: rehearse the candidate, record it in the corpus
/// whatever happens, and install it if the verdict allows. synchronous,
/// because everything it touches is.
///
/// the corpus write is not conditional on success. a refused program is the
/// only evidence this system ever gets about where its own boundary is, and
/// throwing it away is how a training set ends up full of nothing but
/// answers that already worked (§9).
fn apply_policy_swap(
    manifest: &str,
    source: &str,
    origin: crate::cartridges::Origin,
) -> Result<PolicySwap, String> {
    if source.trim().is_empty() {
        return Err("no cartridge source to compile — a policy is a rustlite module".to_string());
    }
    let now = js_sys::Date::now() as i64;
    let outcome = COGNITION.with(|c| {
        let mut slot = c.borrow_mut();
        let Some(cog) = slot.as_mut() else {
            return Err(
                "no reasoning cartridge is running — reload the page and try again".to_string(),
            );
        };
        cog.set_now(now);
        let (sample, result) = cog.swap_policy(manifest, source, origin, now);
        Ok(match result {
            Ok((slug, rehearsal)) => {
                let mut notes = cog.drain_notes();
                notes.extend(cog.take_logs());
                (sample, Ok((slug, rehearsal, notes, cog.take_flushes())))
            }
            // drain the events even on a refusal: a wiring error can have
            // pushed one, and leaving it queued would surface it later
            // attached to the wrong action.
            Err(e) => {
                let _ = cog.drain_notes();
                (sample, Err(e))
            }
        })
    })?;

    let (sample, result) = outcome;
    let corpus = record_sample(sample);
    let (slug, rehearsal, notes, flushes) = result?;
    flush_cognition(flushes);
    Ok(PolicySwap {
        slug,
        rehearsal,
        notes,
        save_error: None,
        corpus,
    })
}

/// remember a swapped-in policy so the next boot brings it back.
async fn persist_policy(slug: &str, manifest: &str, source: &str) -> Result<(), String> {
    let src_path = crate::cartridges::source_path(slug);
    let man_path = crate::cartridges::manifest_path(slug);
    crate::platform::opfs::write(&src_path, source)
        .await
        .map_err(|e| format!("{src_path}: {e}"))?;
    crate::platform::opfs::write(&man_path, manifest)
        .await
        .map_err(|e| format!("{man_path}: {e}"))
}

fn describe_corpus(c: &CorpusStats) -> String {
    let ops: Vec<String> = c
        .top_ops
        .iter()
        .map(|(op, n)| format!("{op}×{n}"))
        .collect();
    let tail = if ops.is_empty() {
        String::new()
    } else {
        format!(" — most-emitted: {}", ops.join(", "))
    };
    format!(
        "📚 corpus: {} program(s), {} verified, {} refused{tail}",
        c.samples, c.verified, c.refused
    )
}

fn describe_rehearsal(r: &crate::cartridges::Rehearsal) -> String {
    let mut line = format!(
        "🧪 rehearsal passed: \"{}\" → \"{}\"",
        crate::cartridges::cognitive::REHEARSAL_PROMPT,
        r.shaped
    );
    if !r.wrote.is_empty() {
        line.push_str(&format!(" (writes {})", r.wrote.join(", ")));
    }
    line
}

/// the ui's door (`Command::SwapCartridge`): apply, narrate, save in the
/// background. a refusal changes nothing — not the running module, not what
/// is on disk — and carries the compiler's or the rehearsal's own words.
fn swap_cartridge(manifest: String, source: String) {
    let swap = match apply_policy_swap(&manifest, &source, crate::cartridges::Origin::Human) {
        Ok(s) => s,
        Err(e) => {
            emit(Event::Error {
                thread: conv(),
                scope: "cartridge".to_string(),
                message: format!("hot-swap refused: {e}"),
            });
            return;
        }
    };

    emit(Event::Note {
        thread: conv(),
        text: describe_rehearsal(&swap.rehearsal),
    });
    for text in swap.notes {
        emit(Event::Note {
            thread: conv(),
            text,
        });
    }
    emit(Event::Note {
        thread: conv(),
        text: format!(
            "🔁 '{}' is now the reasoning policy — the next prompt goes through it",
            swap.slug
        ),
    });
    emit(Event::Note {
        thread: conv(),
        text: describe_corpus(&swap.corpus),
    });

    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = persist_policy(&swap.slug, &manifest, &source).await {
            emit(Event::Error {
                thread: String::new(),
                scope: "cartridge".to_string(),
                message: format!(
                    "the swap is live but could not be saved ({e}) — it will not survive a reload"
                ),
            });
        }
    });
}

/// the AGENT's door: the `swap_cartridge` tool rewrites the policy the loop
/// reasons with, mid-run. a function rather than a pub thread_local so the
/// rehearse-then-install order cannot be skipped by a caller.
///
/// what a swap does NOT do is retroactive: the prompt of the run making the
/// call was already shaped by the old policy. the new one takes effect at
/// the next hook, and the returned summary says so rather than leaving the
/// model to assume otherwise.
pub async fn swap_reasoning_policy(
    manifest: &str,
    source: &str,
    intent: &str,
) -> Result<PolicySwap, String> {
    let origin = crate::cartridges::Origin::Agent {
        intent: intent.to_string(),
    };
    let mut swap = apply_policy_swap(manifest, source, origin)?;
    emit(Event::Note {
        thread: conv(),
        text: describe_rehearsal(&swap.rehearsal),
    });
    for text in &swap.notes {
        emit(Event::Note {
            thread: conv(),
            text: text.clone(),
        });
    }
    // the tool awaits the save rather than spawning it: the model is about
    // to be told whether the swap is durable, and that answer has to be
    // true when it is given.
    if let Err(e) = persist_policy(&swap.slug, manifest, source).await {
        swap.save_error = Some(e);
    }
    emit(Event::Note {
        thread: conv(),
        text: format!(
            "🔁 the agent swapped its own reasoning policy to '{}'",
            swap.slug
        ),
    });
    emit(Event::Note {
        thread: conv(),
        text: describe_corpus(&swap.corpus),
    });
    Ok(swap)
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

                // D10, armed from second zero instead of depending on the
                // agent remembering to sync first: the FIRST Configure with a
                // VERIFIED github token reconciles the working tree against
                // the branch (drops stale clean caches, never dirty files)
                // and records the head so the session's very first commit is
                // already guarded. boot time itself is too early — credentials
                // only arrive here — but this is still before any read or edit
                // a run could make. a failure surfaces as an Error event, not
                // a silent skip (D4); the next Configure retries.
                //
                // the claim is taken BEFORE verification starts and atomically,
                // because TWO Configures arrive at every boot: the worker
                // self-configures from the opfs mirror, then the ui sends its
                // own. checking-then-setting after the await would let both
                // tasks pass the gate while the first was still verifying and
                // run two concurrent reconciles over the same cache files —
                // their colliding removeEntry calls are exactly the recurring
                // "reconcile error ... NoModificationAllowedError". claiming
                // first means the loser skips cleanly instead of racing.
                let claimed_reconcile = github_ok && STATE.with(|s| {
                    let already = s.borrow().auto_reconciled;
                    // should_auto_reconcile stays the single definition of the
                    // gate; this call site adds only the atomic set.
                    if !crate::agent::tools::should_auto_reconcile(true, already) {
                        return false;
                    }
                    s.borrow_mut().auto_reconciled = true;
                    true
                });
                if claimed_reconcile {
                    let gh = crate::agent::github::Github::new(
                        &config.github_token,
                        &config.repo,
                        &config.branch,
                    );
                    let mut ws = crate::agent::tools::Workspace::new(gh).await;
                    match ws.reconcile_against_branch().await {
                        Ok(report) => {
                            RECONCILED_HEAD.with(|h| *h.borrow_mut() = Some(report.head.clone()));
                            emit(Event::Note {
                                thread: conv(),
                                text: format!(
                                    "⇅ tree reconciled against {} at boot: {} file(s) on branch{}, {} uncommitted locally{}",
                                    report.head.chars().take(7).collect::<String>(),
                                    report.files_on_branch,
                                    if report.refreshed.is_empty() {
                                        String::new()
                                    } else {
                                        format!(
                                            ", {} stale cache(s) dropped",
                                            report.refreshed.len()
                                        )
                                    },
                                    report.uncommitted.len(),
                                    if report.failed.is_empty() {
                                        String::new()
                                    } else {
                                        format!(
                                            "; {} cache file(s) were locked by another task and will refresh on a later read: {:?}",
                                            report.failed.len(),
                                            report.failed
                                        )
                                    },
                                ),
                            });
                            if !report.refreshed.is_empty() {
                                emit(Event::TreeChanged { dirty: ws.dirty() });
                            }
                        }
                        Err(e) => {
                            // the claim is released so the next Configure
                            // retries — but only after this failure has been
                            // surfaced (D4). a failure that permanently
                            // disarmed reconcile would be worse than the bug.
                            STATE.with(|s| s.borrow_mut().auto_reconciled = false);
                            emit(Event::Error {
                                thread: conv(),
                                scope: "reconcile".to_string(),
                                message: format!(
                                    "boot-time tree reconciliation failed ({e}); \
                                     call sync_repo before committing"
                                ),
                            });
                        }
                    }
                }

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
            let seq = STATE.with(|s| {
                let mut st = s.borrow_mut();
                st.stop_requested = true;
                st.run_seq
            });

            // cooperative stop is the normal path: the loop notices between
            // chunks and unwinds cleanly, usually within a second.
            //
            // but stop is also the only escape from a wedged run, and a run
            // that is not polling — stuck in a tool, awaiting a dead socket,
            // or lost to a bug — will never notice. without a hatch, nothing
            // ever clears `running`, every later Run is refused with "a run
            // is already in progress", and the app is bricked until reload.
            // the escape from that state IS this command, so it cannot
            // itself depend on the run being healthy.
            //
            // so: give the run a moment to end itself, then take control
            // back regardless. bumping run_seq is what makes that safe — the
            // abandoned run sees it, stops polling, and cannot write back.
            wasm_bindgen_futures::spawn_local(async move {
                crate::agent::http::sleep_ms(STOP_GRACE_MS).await;
                let stuck = STATE.with(|s| {
                    let st = s.borrow();
                    st.running && st.run_seq == seq
                });
                if !stuck {
                    return;
                }
                STATE.with(|s| {
                    let mut st = s.borrow_mut();
                    st.running = false;
                    st.stop_requested = false;
                    st.run_seq = st.run_seq.wrapping_add(1);
                });
                crate::platform::transcript::clear_loop_resume().await;
                emit(Event::Note {
                    thread: conv(),
                    text: "the run did not stop on its own and was ended — control is back with you. anything already written to the working tree is safe."
                        .to_string(),
                });
                emit(Event::RunFinished {
                    thread: conv(),
                    steps: 0,
                    reason: FinishReason::Stopped,
                });
            });
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
            // a human pressing run is the strongest possible signal the
            // work is still wanted: clear crash-loop suspicion so a paused
            // loop can always be relaunched by hand.
            RESTART_BUDGET.with(|b| b.borrow_mut().reset());
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
            // safe mid-run for the same reason switching is: the run owns
            // its history copy and its writes are addressed to its own
            // conversation id. the new thread starts empty and untouched by
            // whatever is running.
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
            // deliberately NO reject_while_running: switching is now safe
            // mid-run. the running loop owns its own history copy; persist()
            // writes are addressed to the run's conversation_id, not to
            // whatever STATE holds; and the write-back at run end is guarded
            // by comparing STATE.conversation against conversation_id, so a
            // finished run can never pour its messages into another thread.
            // locking the whole app for the duration of an infinite-loop run
            // was the real usability bug — an unattended loop must not make
            // the rest of the harness untouchable.
            if STATE.with(|s| s.borrow().running) {
                emit(Event::Note {
                    thread: conv(),
                    text: format!(
                        "↳ run continues in the background on \"{}\" — you can watch from its row in the sidebar",
                        id
                    ),
                });
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
            // deleting the RUNNING thread mid-run would pull its transcript
            // out from under persist() — that refusal stays. deleting any
            // OTHER thread is safe and no longer locks the app.
            let running_here = STATE.with(|s| s.borrow().running)
                && STATE.with(|s| s.borrow().conversation == id);
            if running_here {
                emit(Event::Error {
                    thread: conv(),
                    scope: "conversation".to_string(),
                    message: "cannot delete a conversation while it has a run in progress — press stop first.".to_string(),
                });
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

        Command::RunBatch { tasks } => {
            if STATE.with(|s| s.borrow().running) {
                emit(Event::Error {
                    thread: conv(),
                    scope: "batch".to_string(),
                    message: "cannot queue a batch while a run is in progress — press stop first."
                        .to_string(),
                });
                return;
            }
            let batch = crate::agent::control::BatchState::new(
                tasks.into_iter().map(|t| (t.id, t.prompt)).collect(),
            );
            if batch.is_empty() {
                emit(Event::Error {
                    thread: conv(),
                    scope: "batch".to_string(),
                    message: "an empty batch has nothing to run".to_string(),
                });
                return;
            }
            save_batch(&batch);
            emit(Event::Note {
                thread: conv(),
                text: format!("☑ queued {} task(s) for sequential execution", batch.tasks.len()),
            });
            wasm_bindgen_futures::spawn_local(drive_batch(batch));
        }

        Command::SwapCartridge { manifest, source } => swap_cartridge(manifest, source),

        Command::RunBenchmark => {
            if STATE.with(|s| s.borrow().running) {
                emit(Event::Error {
                    thread: conv(),
                    scope: "bench".to_string(),
                    message: "cannot start a benchmark while a run is in progress — press stop first."
                        .to_string(),
                });
                return;
            }
            wasm_bindgen_futures::spawn_local(run_benchmark_suite());
        }
    }
}

/// submit work programmatically. this is the function an external benchmark
/// harness calls instead of typing into the prompt box: tasks run
/// sequentially, results land in opfs at vanish-batch/results.json, and
/// Event::BatchFinished announces completion on the ui channel.
#[wasm_bindgen]
pub fn enqueue_batch(tasks_json: &str) -> Result<(), JsValue> {
    let tasks: Vec<crate::protocol::BatchTask> = serde_json::from_str(tasks_json)
        .map_err(|e| JsValue::from_str(&format!("bad batch json: {e}")))?;
    handle(Command::RunBatch { tasks });
    Ok(())
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
