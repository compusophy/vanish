//! rendering the run as it happens.
//!
//! every event has a visible outcome. the previous client swallowed several
//! failure paths in empty catch blocks, which is how it shipped a dropdown
//! that looked fine and did nothing — so here, an error always draws.

use std::cell::RefCell;

use wasm_bindgen::{prelude::Closure, JsCast};
use web_sys::Element;

use super::{by_id, create, doc, Shared};
use crate::protocol::{Event, FinishReason};

thread_local! {
    /// the card the current step is streaming into, so deltas append to one
    /// element instead of creating thousands of nodes.
    static ACTIVE: RefCell<Option<Element>> = const { RefCell::new(None) };
    static ACTIVE_REASONING: RefCell<Option<Element>> = const { RefCell::new(None) };
    static ACTIVE_CONTENT: RefCell<Option<Element>> = const { RefCell::new(None) };
}

/// dock reconciliation on return to the tab.
///
/// while this page is hidden the browser may freeze or discard it — memory
/// saver, mobile os — and events that crossed during that window can be
/// dropped or delivered late. the 3s watchdog only runs while a run is
/// BELIEVED active, so a run that ended (or was resumed into another
/// thread) while hidden could leave stale buttons until its next tick.
/// visibilitychange fires the instant the user comes back: one RunState
        // ping here closes any drift immediately instead of within 3s.
pub fn wire_visibility_reconcile(ui: &Shared) {
    let ui = ui.clone();
    let cb = Closure::<dyn FnMut()>::new(move || {
        let worker = ui.borrow().worker.clone();
        super::send(&worker, &crate::protocol::Command::RunState);
    });
    let _ = doc().add_event_listener_with_callback(
        "visibilitychange",
        cb.as_ref().unchecked_ref(),
    );
    cb.forget();
}

fn feed_root() -> Option<Element> {
    by_id("feed")
}

fn append(node: &Element) {
    if let Some(feed) = feed_root() {
        let _ = feed.append_child(node);
        scroll_to_bottom();
    }
}

fn scroll_to_bottom() {
    if let Some(feed) = feed_root() {
        feed.set_scroll_top(feed.scroll_height());
    }
}

fn set_status(text: &str, running: bool) {
    if let Some(s) = by_id("status") {
        s.set_text_content(Some(text));
        s.set_class_name(if running { "status running" } else { "status" });
    }
    set_dock_buttons(running);
}

/// flip run/stop visibility. split out of set_status because reconciliation
/// needs to fix ONLY the buttons without inventing a status line.
pub fn set_dock_buttons(running: bool) {
    if let (Some(run), Some(stop)) = (by_id("run"), by_id("stop")) {
        let _ = run.set_attribute("style", if running { "display:none" } else { "" });
        let _ = stop.set_attribute("style", if running { "" } else { "display:none" });
    }
}

pub fn user_message(text: &str) {
    let card = create("div", "msg user");
    let label = create("div", "msg-label");
    label.set_text_content(Some("you"));
    let body = create("div", "msg-body");
    body.set_text_content(Some(text));
    let _ = card.append_child(&label);
    let _ = card.append_child(&body);
    append(&card);
}

/// shown when the app loads with no usable credentials.
///
/// there is no server and therefore no sign-in, so an unconfigured harness
/// used to just sit there looking ready while being unable to do anything —
/// which is exactly how someone returning to it concludes the app is broken.
/// say what is needed, and precisely how to get it.
pub fn setup_required(missing_key: bool, missing_github: bool) {
    if by_id("setup-card").is_some() {
        return;
    }
    let card = create("div", "setup-card");
    let _ = card.set_attribute("id", "setup-card");

    // static markup only — no interpolated user data reaches this string.
    card.set_inner_html(
        "<div class=\"setup-title\">setup needed — this build has no backend</div>\
         <div class=\"setup-body\">\
           the old version signed you in with github oauth and read the openrouter key \
           from a vercel environment variable. both of those needed a server, and this \
           build has none: it runs entirely in your browser. credentials now live in \
           this browser only, and are entered once in the panel on the right.\
         </div>\
         <ol class=\"setup-steps\">\
           <li><b>openrouter key</b> — openrouter.ai/keys → create key → paste into \
               \"openrouter api key\".</li>\
           <li><b>github token</b> — github.com/settings/personal-access-tokens → \
               generate a fine-grained token, scoped to only this repository, with \
               <b>contents: read and write</b>. oauth cannot work without a server, so \
               a token replaces the sign-in button.</li>\
           <li>set the repository (owner/name) and branch, then press \
               <b>save settings</b>. both credentials are checked immediately and the \
               result is reported here.</li>\
         </ol>",
    );

    if missing_key || missing_github {
        let what = create("div", "setup-missing");
        what.set_text_content(Some(&match (missing_key, missing_github) {
            (true, true) => "missing: openrouter key and github token".to_string(),
            (true, false) => "missing: openrouter key".to_string(),
            (false, true) => "missing: github token or repository".to_string(),
            (false, false) => String::new(),
        }));
        let _ = card.append_child(&what);
    }

    append(&card);

    // draw the eye to where the work happens
    if let Some(panel) = doc().query_selector(".rail-right").ok().flatten() {
        let _ = panel.set_attribute("data-attention", "true");
    }
}

fn clear_setup_card() {
    if let Some(c) = by_id("setup-card") {
        c.remove();
    }
    if let Some(panel) = doc().query_selector(".rail-right").ok().flatten() {
        let _ = panel.remove_attribute("data-attention");
    }
}

pub fn note(text: &str) {
    let n = create("div", "note");
    n.set_text_content(Some(text));
    append(&n);
}

fn error_card(scope: &str, message: &str) -> Element {
    let card = create("div", "error-card");
    let title = create("div", "error-title");
    title.set_text_content(Some(&format!("⚠ {scope} error")));
    let body = create("div", "error-body");
    body.set_text_content(Some(message));
    let _ = card.append_child(&title);
    let _ = card.append_child(&body);
    card
}

pub fn error(scope: &str, message: &str) {
    let card = error_card(scope, message);
    append(&card);
    set_status("error", false);
}

/// boot-time failures have no run to mark as errored, but they must still
/// draw (D4). used by settings loading before any status exists.
pub fn append_error(message: &str) {
    let card = error_card("settings", message);
    append(&card);
}

/// for callers that built their own card shape but need it placed in the feed.
pub fn append_card(card: &Element) {
    append(card);
}

/// lazily create the streaming target for the current step.
fn stream_target(kind: &str) -> Option<Element> {
    let cell = if kind == "reasoning" {
        &ACTIVE_REASONING
    } else {
        &ACTIVE_CONTENT
    };

    cell.with(|c| {
        if let Some(e) = c.borrow().as_ref() {
            return Some(e.clone());
        }
        let parent = ACTIVE.with(|a| a.borrow().clone())?;
        let block = create(
            "div",
            if kind == "reasoning" {
                "stream reasoning"
            } else {
                "stream content"
            },
        );
        if kind == "reasoning" {
            let label = create("div", "stream-label");
            label.set_text_content(Some("thinking"));
            let _ = block.append_child(&label);
        }
        let text = create("div", "stream-text");
        let _ = block.append_child(&text);
        let _ = parent.append_child(&block);
        *c.borrow_mut() = Some(text.clone());
        Some(text)
    })
}

fn append_delta(kind: &str, delta: &str) {
    if let Some(target) = stream_target(kind) {
        let existing = target.text_content().unwrap_or_default();
        target.set_text_content(Some(&format!("{existing}{delta}")));
        scroll_to_bottom();
    }
}

pub fn render(ui: &Shared, event: Event) {
    // belt and braces: RunStarted/RunFinished must ALWAYS reach the dock
    // handler, even when the router would classify them as another thread's
    // traffic. a run's start and end are dock-level facts — the stop button
    // belongs to whatever run is in flight, not to whichever conversation is
    // on screen. without this, a background thread finishing its run leaves
    // the visible dock stuck on "stop".
    if !event.touches_run_state() {
        let tagged = event.thread().to_string();
        let is_background = !tagged.is_empty()
            && {
                let u = ui.borrow();
                !u.active_thread.is_empty() && tagged != u.active_thread
            };
        if is_background {
            background_activity(ui, &tagged, &event);
            return;
        }
    }
    render_active(ui, event);
}

/// collapse another conversation's traffic into a one-line badge on its
/// sidebar row instead of interleaving it with the visible feed.
fn background_activity(_ui: &Shared, thread: &str, event: &Event) {
    let summary = match event {
        Event::StepStarted { step, .. } => Some(format!("step {step}")),
        Event::ToolStarted { name, .. } => Some(format!("⚡ {name}")),
        Event::RunFinished { steps, reason, .. } => Some(format!(
            "run ended after {steps} step(s) ({})",
            match reason {
                FinishReason::Completed => "completed",
                FinishReason::Stopped => "stopped",
                FinishReason::StepLimit => "step limit",
                FinishReason::Failed => "failed",
            }
        )),
        _ => None,
    };
    if summary.is_none() {
        return;
    }
    if let Some(row) = doc()
        .query_selector(&format!("[data-conv-id=\"{thread}\"]"))
        .ok()
        .flatten()
    {
        if let Some(existing) = row.query_selector(".conv-activity").ok().flatten() {
            existing.remove();
        }
        if let Some(text) = summary {
            let badge = create("span", "conv-activity");
            badge.set_text_content(Some(&text));
            let _ = row.append_child(&badge);
        }
    }
}

fn render_active(ui: &Shared, event: Event) {
    // the reconciliation answer is handled before the match because it must
    // not draw anything and must not be routable as background traffic.
    if let Event::RunStateReport { running } = event {
        reconcile(ui, running);
        return;
    }
    match event {
        Event::Ready { build } => {
            ui.borrow_mut().build = build.clone();
            if let Some(b) = by_id("build-id") {
                b.set_text_content(Some(&format!("build {build}")));
            }
            set_status("ready", false);
            // the worker is listening only from this point on. sending the
            // opening Configure any earlier means posting into a worker that
            // has no onmessage handler yet, and the message is discarded —
            // which is what made saved credentials require a manual "save
            // settings" on every page load.
            super::bootstrap_worker(ui);
            // the running build id is only known now, so this is the earliest
            // an update check can mean anything.
            super::update::check_now(ui);
        }

        Event::Conversations { items, active } => {
            ui.borrow_mut().active_thread = active.clone();
            let Some(list) = by_id("conversations") else {
                return;
            };
            list.set_inner_html("");

            for c in &items {
                let row = create(
                    "div",
                    if c.id == active {
                        "conv-row active"
                    } else {
                        "conv-row"
                    },
                );
                let _ = row.set_attribute("data-conv-id", &c.id);

                let title = create("span", "conv-title");
                title.set_text_content(Some(&c.title));
                let _ = row.append_child(&title);

                let count = create("span", "conv-count");
                count.set_text_content(Some(&c.count.to_string()));
                let _ = row.append_child(&count);

                // delete affordance per row; confirmation is the fact that it
                // is a small separate target, not a modal.
                let del = create("button", "conv-del");
                del.set_text_content(Some("×"));
                let _ = del.set_attribute("title", "delete this conversation");
                let _ = del.set_attribute("data-conv-del", &c.id);
                let _ = row.append_child(&del);

                let _ = list.append_child(&row);
            }

            super::wire_conversation_rows(ui);
        }

        Event::ConfigStatus {
            openrouter_ok,
            github_ok,
            vercel_ok,
            detail,
        } => {
            super::finish_settings_check();

            let core = openrouter_ok && github_ok;
            let vercel_broken = vercel_ok == Some(false);
            // the header used to read "✓ credentials verified" while the line
            // underneath said a token was unusable. say one thing.
            let (class, heading) = if !core {
                ("error-card", "⚠ credentials not usable")
            } else if vercel_broken {
                (
                    "error-card",
                    "⚠ core credentials fine — vercel token not usable",
                )
            } else {
                ("commit-card", "✓ credentials verified")
            };

            let both = core;
            let card = create("div", class);
            let title = create("div", if class == "error-card" { "error-title" } else { "" });
            title.set_text_content(Some(heading));
            let body = create("div", "error-body");
            body.set_text_content(Some(&detail));
            let _ = card.append_child(&title);
            let _ = card.append_child(&body);
            append(&card);

            if both {
                clear_setup_card();
                set_status("ready", false);
                // now that the token is known good, populating the explorer
                // cannot produce a confusing 401.
                let worker = ui.borrow().worker.clone();
                super::send(&worker, &crate::protocol::Command::ListTree);
            } else {
                // leave the setup guidance up: it is still the answer
                setup_required(!openrouter_ok, !github_ok);
            }
        }

        Event::RunStarted { model, .. } => {
            ui.borrow_mut().running = true;
            set_status(&format!("running · {model}"), true);
            start_run_watchdog(ui);
        }

        Event::StepStarted { step, .. } => {
            // close out the previous step's streaming targets
            ACTIVE_REASONING.with(|c| *c.borrow_mut() = None);
            ACTIVE_CONTENT.with(|c| *c.borrow_mut() = None);

            let card = create("div", "step");
            let head = create("div", "step-head");
            head.set_text_content(Some(&format!("step {step}")));
            let _ = card.append_child(&head);
            append(&card);
            ACTIVE.with(|a| *a.borrow_mut() = Some(card));
        }

        Event::Reasoning { delta, .. } => append_delta("reasoning", &delta),
        Event::Content { delta, .. } => append_delta("content", &delta),

        Event::ToolStarted { id, name, args, .. } => {
            let Some(parent) = ACTIVE.with(|a| a.borrow().clone()) else {
                return;
            };
            let card = create("div", "tool pending");
            let _ = card.set_attribute("data-tool-id", &id);
            let head = create("div", "tool-head");
            head.set_text_content(Some(&format!("⚡ {name}")));
            let argline = create("pre", "tool-args");
            argline.set_text_content(Some(&truncate(&args, 400)));
            let _ = card.append_child(&head);
            let _ = card.append_child(&argline);
            let _ = parent.append_child(&card);
            scroll_to_bottom();
        }

        Event::ToolFinished {
            id, ok, result, ..
        } => {
            let selector = format!("[data-tool-id=\"{id}\"]");
            let Ok(Some(card)) = doc().query_selector(&selector) else {
                return;
            };
            card.set_class_name(if ok { "tool ok" } else { "tool failed" });
            let out = create("pre", "tool-result");
            out.set_text_content(Some(&truncate(&result, 1200)));
            let _ = card.append_child(&out);
            scroll_to_bottom();
        }

        Event::TreeChanged { dirty } => {
            if let Some(badge) = by_id("dirty-count") {
                badge.set_text_content(Some(&dirty.len().to_string()));
                let _ = badge.set_attribute(
                    "style",
                    if dirty.is_empty() { "display:none" } else { "" },
                );
            }
            if let Some(list) = by_id("dirty-list") {
                list.set_inner_html("");
                for path in &dirty {
                    let row = create("div", "dirty-row");
                    row.set_text_content(Some(path));
                    let _ = list.append_child(&row);
                }
            }
            // an editor save reports back through this event too; clear the
            // "saving…" note and show the file's dirty state.
            if let Some(status) = by_id("editor-status") {
                if status.text_content().as_deref() == Some("saving…") {
                    status.set_text_content(Some("saved to the working tree — uncommitted"));
                }
            }
        }

        Event::Committed {
            sha,
            message,
            files,
        } => {
            let card = create("div", "commit-card");
            card.set_text_content(Some(&format!(
                "✓ committed {sha} — {files} file(s): {message}"
            )));
            append(&card);
        }

        Event::RunFinished { steps, reason, .. } => {
            ui.borrow_mut().running = false;
            stop_run_watchdog();
            ACTIVE.with(|a| *a.borrow_mut() = None);
            ACTIVE_REASONING.with(|c| *c.borrow_mut() = None);
            ACTIVE_CONTENT.with(|c| *c.borrow_mut() = None);

            let label = match reason {
                FinishReason::Completed => "completed",
                FinishReason::Stopped => "stopped",
                FinishReason::StepLimit => "hit the step ceiling",
                FinishReason::Failed => "failed",
            };
            let card = create("div", "note finish");
            card.set_text_content(Some(&format!("run {label} after {steps} step(s)")));
            append(&card);
            set_status(label, false);

            // an update that arrived mid-run was held back so it could not
            // cut the run off. it is safe to apply now.
            super::update::apply_pending_if_any();
        }

        Event::Error { scope, message, .. } => {
            // any failure path also re-arms the save button; a lock that only
            // clears on success is one that eventually never clears.
            super::finish_settings_check();
            error(&scope, &message)
        }

        Event::Note { text, .. } => note(&text),

        Event::Tree { entries } => {
            let Some(tree) = by_id("tree") else { return };
            tree.set_inner_html("");
            for e in entries.iter().filter(|e| !e.is_dir) {
                let row = create(
                    "div",
                    if e.dirty { "file dirty" } else { "file" },
                );
                // clickable: the editor pane opens this path on click
                let _ = row.set_attribute("data-file", &e.path);
                row.set_text_content(Some(&e.path));
                let _ = tree.append_child(&row);
            }
        }

        Event::FileContent { path, content } => {
            // reveal the editor pane, fill it, and say how big the file is.
            if let Some(pane) = by_id("editor-pane") {
                let _ = pane.set_attribute("style", "display: flex");
            }
            if let Some(ed) = by_id("editor").and_then(|e| {
                e.dyn_into::<web_sys::HtmlTextAreaElement>().ok()
            }) {
                ed.set_value(&content);
            }
            if let Some(name) = by_id("editor-path") {
                name.set_text_content(Some(&path));
            }
            if let Some(status) = by_id("editor-status") {
                status.set_text_content(Some(&format!(
                    "{} lines · local copy",
                    content.lines().count()
                )));
            }
            scroll_to_bottom();
        }

        Event::HistoryRestored { turns, .. } => {
            if turns.is_empty() {
                return;
            }
            append(&restored_divider());
            for t in &turns {
                match t.role.as_str() {
                    "user" => {
                        if let Some(c) = &t.content {
                            user_message(c);
                        }
                    }
                    "assistant" => {
                        let card = create("div", "step restored");
                        let head = create("div", "step-head");
                        head.set_text_content(Some("earlier"));
                        let _ = card.append_child(&head);
                        if let Some(c) = &t.content {
                            let body = create("div", "stream content");
                            let text = create("div", "stream-text");
                            text.set_text_content(Some(c));
                            let _ = body.append_child(&text);
                            let _ = card.append_child(&body);
                        }
                        for call in &t.tools {
                            let row = create("div", "tool restored-call");
                            row.set_text_content(Some(call));
                            let _ = card.append_child(&row);
                        }
                        append(&card);
                    }
                    _ => {}
                }
            }
            scroll_to_bottom();
        }

        Event::HistoryCleared => {
            if let Some(feed) = feed_root() {
                feed.set_inner_html("");
            }
            ACTIVE.with(|a| *a.borrow_mut() = None);
            ACTIVE_REASONING.with(|c| *c.borrow_mut() = None);
            ACTIVE_CONTENT.with(|c| *c.borrow_mut() = None);
            note("conversation cleared — the agent will start fresh on the next run");
            set_status("ready", false);
        }

        Event::BatchFinished { results, status } => {
            let label = if status == "completed" { "completed" } else { "cancelled" };
            let card = create("div", "commit-card");
            card.set_text_content(Some(&format!(
                "☑ batch {label}: {} task(s) — results exported to vanish-batch/results.json",
                results.len()
            )));
            append(&card);
        }

        Event::BenchmarkFinished { passed, total } => {
            if let Some(s) = by_id("status") {
                s.set_text_content(Some(&format!("benchmark: {passed}/{total}")));
            }
        }

        // handled above the match (before background routing); unreachable
        // here, but the match must stay exhaustive.
        Event::RunStateReport { .. } => {}
    }
}

/// the seam between what survived a reload and what happens next. without it
/// a restored transcript reads as if this session produced it.
fn restored_divider() -> Element {
    let d = create("div", "note restored-divider");
    d.set_text_content(Some("↩ restored from the previous session"));
    d
}

/// the dock's reconciliation loop.
///
/// RunFinished is an ordinary postMessage crossing a worker boundary, and
/// this bug class has bitten repeatedly: when it is delayed or lost, the
/// buttons stay on "stop" forever with no run behind them — the exact
/// "stuck on stop" failure reported by the user. so while the ui BELIEVES a
/// run is active, it pings the worker every few seconds and asks for ground
/// truth. if the worker says no run is in flight, the buttons snap back to
/// "run" within one interval, whatever went wrong with the event stream.
///
/// one self-canceling interval per run; started by RunStarted, stopped by
/// RunFinished (and by its own correction).
fn start_run_watchdog(ui: &Shared) {
    // any previous interval is stale by definition — a new RunStarted while
    // one is running means the old run ended without our having seen it.
    stop_run_watchdog();

    UNANSWERED.with(|u| *u.borrow_mut() = 0);

    let ui = ui.clone();
    let tick = Closure::<dyn FnMut()>::new(move || {
        // the watchdog can only reconcile a worker that ANSWERS. a worker
        // whose event loop is starved never dispatches onmessage at all, so
        // the pings vanish and the dock sits on "stop" looking identical to
        // a healthy long run. count the silence and say so: an app that is
        // wedged should never also be quiet about it.
        let silent = UNANSWERED.with(|u| {
            let mut n = u.borrow_mut();
            *n += 1;
            *n
        });
        if silent == UNANSWERED_LIMIT {
            error(
                "worker",
                "the agent worker has stopped responding — it has not answered a health check in several seconds. anything already written to the working tree is safe; reload the page to recover.",
            );
        }

        let worker = ui.borrow().worker.clone();
        super::send(&worker, &crate::protocol::Command::RunState);
    });

    if let Some(window) = web_sys::window() {
        let id = window.set_interval_with_callback_and_timeout_and_arguments_0(
            tick.as_ref().unchecked_ref(),
            WATCHDOG_INTERVAL_MS,
        );
        WATCHDOG.with(|w| *w.borrow_mut() = id.ok());
    }
    tick.forget();
}

fn stop_run_watchdog() {
    let had = WATCHDOG.with(|w| w.borrow_mut().take());
    if let (Some(id), Some(window)) = (had, web_sys::window()) {
        window.clear_interval_with_handle(id);
    }
}

thread_local! {
    /// interval handle of the live watchdog, if any. Option<i32> because
    /// set_interval returns i32 and 0 is a valid id — only None means off.
    static WATCHDOG: RefCell<Option<i32>> = const { RefCell::new(None) };

    /// health checks sent since the last answer. reset by every
    /// RunStateReport; a rising count means the worker is not dispatching
    /// messages at all.
    static UNANSWERED: RefCell<u32> = const { RefCell::new(0) };
}

/// how many consecutive unanswered health checks before the ui declares the
/// worker unresponsive. three ticks is ~9s — comfortably longer than any
/// real scheduling hiccup, short enough to beat the user's patience.
const UNANSWERED_LIMIT: u32 = 3;

/// how often the dock checks the worker's actual state while a run is
/// believed in flight. short enough that a stuck button self-corrects
/// before anyone reaches for a reload; cheap enough to be invisible.
const WATCHDOG_INTERVAL_MS: i32 = 3_000;

/// answer from Command::RunState. called for EVERY event; only the report
/// acts, everything else falls through untouched.
fn reconcile(ui: &Shared, running: bool) {
    // an answer arrived: the worker is alive and dispatching.
    UNANSWERED.with(|u| *u.borrow_mut() = 0);

    let believed_running = ui.borrow().running;

    // healthy path: agreement. nothing to do.
    if believed_running == running {
        return;
    }

    // the worker says idle but we show "running" → the finish event was
    // lost or delayed behind the transcript save. fix the buttons now;
    // the real RunFinished may still arrive later and is harmless then.
    if believed_running && !running {
        ui.borrow_mut().running = false;
        stop_run_watchdog();
        set_dock_buttons(false);
        note("run ended — control returned to you (state reconciled after a lost finish event)");
    }
    // the inverse (worker busy, ui shows ready) cannot happen through this
    // channel: every start emits RunStarted first, which re-arms both the
    // flag and the watchdog. leaving it alone avoids clobbering the status
    // line during that window.
}

/// keep one runaway tool result from making the page unusable.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n… truncated ({} chars total)", s.chars().count())
}
