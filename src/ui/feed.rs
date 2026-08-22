//! rendering the run as it happens.
//!
//! every event has a visible outcome. the previous client swallowed several
//! failure paths in empty catch blocks, which is how it shipped a dropdown
//! that looked fine and did nothing — so here, an error always draws.

use std::cell::RefCell;

use wasm_bindgen::JsCast;
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

pub fn error(scope: &str, message: &str) {
    let card = create("div", "error-card");
    let title = create("div", "error-title");
    title.set_text_content(Some(&format!("⚠ {scope} error")));
    let body = create("div", "error-body");
    body.set_text_content(Some(message));
    let _ = card.append_child(&title);
    let _ = card.append_child(&body);
    append(&card);
    set_status("error", false);
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
    match event {
        Event::Ready { build } => {
            ui.borrow_mut().build = build.clone();
            if let Some(b) = by_id("build-id") {
                b.set_text_content(Some(&format!("build {build}")));
            }
            set_status("ready", false);
        }

        Event::ConfigStatus {
            openrouter_ok,
            github_ok,
            detail,
        } => {
            let both = openrouter_ok && github_ok;
            let card = create("div", if both { "commit-card" } else { "error-card" });
            let title = create("div", if both { "" } else { "error-title" });
            title.set_text_content(Some(if both {
                "✓ credentials verified"
            } else {
                "⚠ credentials not usable"
            }));
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
        }

        Event::StepStarted { step } => {
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

        Event::Reasoning { delta } => append_delta("reasoning", &delta),
        Event::Content { delta } => append_delta("content", &delta),

        Event::ToolStarted { id, name, args } => {
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

        Event::RunFinished { steps, reason } => {
            ui.borrow_mut().running = false;
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

        Event::Error { scope, message } => error(&scope, &message),

        Event::Tree { entries } => {
            let Some(tree) = by_id("tree") else { return };
            tree.set_inner_html("");
            for e in entries.iter().filter(|e| !e.is_dir) {
                let row = create("div", if e.dirty { "file dirty" } else { "file" });
                row.set_text_content(Some(&e.path));
                let _ = tree.append_child(&row);
            }
        }

        Event::FileContent { path, content } => {
            if let Some(ed) = by_id("editor").and_then(|e| {
                e.dyn_into::<web_sys::HtmlTextAreaElement>().ok()
            }) {
                ed.set_value(&content);
            }
            if let Some(name) = by_id("editor-path") {
                name.set_text_content(Some(&path));
            }
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
    }
}

/// the seam between what survived a reload and what happens next. without it
/// a restored transcript reads as if this session produced it.
fn restored_divider() -> Element {
    let d = create("div", "note restored-divider");
    d.set_text_content(Some("↩ restored from the previous session"));
    d
}

/// keep one runaway tool result from making the page unusable.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n… truncated ({} chars total)", s.chars().count())
}
