//! over-the-air updates, with no server to ask.
//!
//! the binary is stamped at build time with the commit it came from. the ui
//! polls github for the branch head; when the head differs from the running
//! build, a new version exists. the banner shows what changed and reloads on
//! its own, because a user who does not know to hard-refresh will otherwise
//! keep driving a stale client — which is precisely how this app shipped a
//! dom/logic mismatch to a live session twice.

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use super::{by_id, create, doc, Shared};
use crate::agent::http::request;

const POLL_MS: i32 = 45_000;
/// grace period so the changelog is readable before the page goes away.
const RELOAD_DELAY_MS: i32 = 6_000;
/// localStorage key holding the sha the user chose not to take yet. cleared
/// implicitly by taking the update, since the running build then matches.
const DECLINED_KEY: &str = "vanish.ota.declined";

pub fn start_watching(ui: &Shared) {
    let ui = ui.clone();
    let tick = Closure::<dyn FnMut()>::new(move || {
        let ui = ui.clone();
        wasm_bindgen_futures::spawn_local(async move {
            check_once(&ui).await;
        });
    });

    if let Some(window) = web_sys::window() {
        let _ = window.set_interval_with_callback_and_timeout_and_arguments_0(
            tick.as_ref().unchecked_ref(),
            POLL_MS,
        );
    }
    tick.forget();
}

async fn check_once(ui: &Shared) {
    let (build, repo, branch, token, running) = {
        let u = ui.borrow();
        (
            u.build.clone(),
            u.config.repo.clone(),
            u.config.branch.clone(),
            u.config.github_token.clone(),
            u.running,
        )
    };

    // "dev" means the binary was built outside a git checkout; there is
    // nothing meaningful to compare against.
    if build.is_empty() || build == "dev" || repo.is_empty() || token.is_empty() {
        return;
    }

    let headers = vec![
        ("Authorization", format!("Bearer {token}")),
        ("Accept", "application/vnd.github+json".to_string()),
    ];
    let url = format!("https://api.github.com/repos/{repo}/commits?sha={branch}&per_page=5");

    let Ok(resp) = request("GET", &url, &headers, None).await else {
        // a failed poll is not worth interrupting the user over; the next
        // tick will try again.
        return;
    };
    if !resp.ok() {
        return;
    }

    let Ok(commits) = serde_json::from_str::<Vec<serde_json::Value>>(&resp.body) else {
        return;
    };
    let Some(head) = commits.first() else { return };
    let head_sha = head
        .get("sha")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .chars()
        .take(7)
        .collect::<String>();

    if head_sha.is_empty() || head_sha == build {
        return;
    }

    // the user pressed "later" for this version. respect that until a newer
    // commit lands — polling must never nag more than the release cadence.
    let declined = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(DECLINED_KEY).ok().flatten())
        .unwrap_or_default();
    if declined == head_sha {
        return;
    }

    // collect the commits between the running build and the head, so the
    // banner says what actually changed rather than just "a new version".
    let mut changelog = Vec::new();
    for c in &commits {
        let sha = c
            .get("sha")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .chars()
            .take(7)
            .collect::<String>();
        if sha == build {
            break;
        }
        if let Some(msg) = c
            .get("commit")
            .and_then(|c| c.get("message"))
            .and_then(|m| m.as_str())
        {
            changelog.push(msg.lines().next().unwrap_or(msg).to_string());
        }
    }

    show_banner(&head_sha, &changelog, running);
}

fn show_banner(sha: &str, changelog: &[String], run_in_progress: bool) {
    // already showing this version
    if let Some(existing) = by_id("ota") {
        if existing.get_attribute("data-sha").as_deref() == Some(sha) {
            return;
        }
        existing.remove();
    }

    let banner = create("div", "ota sparkle");
    let _ = banner.set_attribute("id", "ota");
    let _ = banner.set_attribute("data-sha", sha);

    let title = create("div", "ota-title");
    title.set_text_content(Some(&format!("✨ new version {sha} deployed")));
    let _ = banner.append_child(&title);

    if !changelog.is_empty() {
        let list = create("ul", "ota-changelog");
        for line in changelog.iter().take(5) {
            let li = create("li", "");
            li.set_text_content(Some(line));
            let _ = list.append_child(&li);
        }
        let _ = banner.append_child(&list);
    }

    let footer = create("div", "ota-footer");
    let _ = banner.append_child(&footer);

    // a run in flight must not be cut off. the tree is durable, so nothing
    // would be lost — but a half-finished run would be, so wait it out.
    if run_in_progress {
        footer.set_text_content(Some(
            "a run is in progress — updating as soon as it finishes",
        ));
        let button = create("button", "ota-btn");
        button.set_text_content(Some("update now anyway"));
        attach_reload(&button, 0);
        let _ = banner.append_child(&button);
    } else {
        footer.set_text_content(Some("updating automatically…"));
        let button = create("button", "ota-btn");
        button.set_text_content(Some("update now"));
        attach_reload(&button, 0);
        let _ = banner.append_child(&button);
        schedule_reload(RELOAD_DELAY_MS);
    }

    // the conversation survives a reload now (it is written through to opfs
    // after every run), so updating is no longer destructive — but it still
    // interrupts whatever the user is reading, and "not now" costs nothing
    // to offer. the next poll re-offers the update.
    let later = create("button", "ota-btn quiet");
    later.set_text_content(Some("later — keep this session"));
    attach_dismiss(&later, sha);
    let _ = banner.append_child(&later);

    if let Some(body) = doc().body() {
        let _ = body.append_child(&banner);
    }
}

/// dismiss the banner for this version. the poll would otherwise redraw it
/// every 45 seconds; remembering which sha was declined keeps that from
/// nagging until the next actual release.
fn attach_dismiss(el: &web_sys::Element, sha: &str) {
    let sha = sha.to_string();
    let cb = Closure::<dyn FnMut()>::new(move || {
        if let Some(w) = web_sys::window() {
            if let Ok(Some(storage)) = w.local_storage() {
                let _ = storage.set_item(DECLINED_KEY, &sha);
            }
        }
        if let Some(b) = by_id("ota") {
            b.remove();
        }
    });
    let _ = el.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
    cb.forget();
}

fn reload_now() {
    if let Some(w) = web_sys::window() {
        // reload from the network, not the bfcache, so the new wasm is what
        // actually loads.
        let _ = w.location().reload_with_forceget(true);
    }
}

fn attach_reload(el: &web_sys::Element, delay_ms: i32) {
    let cb = Closure::<dyn FnMut()>::new(move || {
        if delay_ms > 0 {
            schedule_reload(delay_ms);
        } else {
            reload_now();
        }
    });
    let _ = el.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
    cb.forget();
}

fn schedule_reload(delay_ms: i32) {
    let cb = Closure::<dyn FnMut()>::new(reload_now);
    if let Some(w) = web_sys::window() {
        let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(),
            delay_ms,
        );
    }
    cb.forget();
}

/// called when a run ends while an update is pending, so the deferred
/// reload happens the moment it is safe.
pub fn apply_pending_if_any() {
    if by_id("ota").is_some() {
        schedule_reload(RELOAD_DELAY_MS);
    }
}
