//! over-the-air updates, with no server to ask.
//!
//! the running binary knows the commit it was built from (`crate::BUILD`).
//! the deployed site publishes the same identity at `/build.json`, written by
//! `build.rs`. an update exists when those two disagree.
//!
//! an earlier version compared against the github branch head instead, which
//! is a different question and the wrong one: the branch moving does not mean
//! a new build shipped. when a build failed — which happens routinely, since
//! this agent commits to its own repository — head advanced while production
//! stayed pinned to the last good build. the mismatch never resolved, so the
//! page reloaded itself every poll, forever, and the app was unusable.
//!
//! asking the server what it is serving cannot produce that loop: a failed
//! deploy leaves the old manifest in place, which matches, so nothing fires.

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use super::Shared;
use crate::agent::http::request;

const POLL_MS: i32 = 60_000;
/// grace period so the changelog is readable before the page goes away.
const RELOAD_DELAY_MS: i32 = 6_000;
/// remembers which build we have already reloaded for, so a mismatch that
/// survives a reload degrades into a manual button instead of a loop.
const RELOAD_GUARD: &str = "vanish.ota.reloaded_for";

/// check once, now. called as soon as the worker reports its build id, so a
/// shell that was served from cache against a newer deploy is caught on load
/// rather than up to a poll interval later.
pub fn check_now(ui: &Shared) {
    let ui = ui.clone();
    wasm_bindgen_futures::spawn_local(async move {
        check_once(&ui).await;
    });
}

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

fn session() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.session_storage().ok().flatten())
}

/// what the server currently serves.
async fn deployed() -> Option<(String, String)> {
    // a query string defeats any intermediary that ignores no-store; the
    // manifest is tiny, so re-fetching costs nothing.
    let bust = js_sys::Date::now() as u64;
    let resp = request("GET", &format!("/build.json?t={bust}"), &[], None)
        .await
        .ok()?;
    if !resp.ok() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&resp.body).ok()?;
    let build = v.get("build")?.as_str()?.to_string();
    let message = v
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string();
    Some((build, message))
}

async fn check_once(ui: &Shared) {
    let (running, run_in_progress) = {
        let u = ui.borrow();
        (u.build.clone(), u.running)
    };

    // "dev" means the binary was built outside a git checkout; there is
    // nothing meaningful to compare against.
    if running.is_empty() || running == "dev" {
        return;
    }

    let Some((deployed_build, message)) = deployed().await else {
        // a missing or unreadable manifest is not an update. staying quiet is
        // strictly better than guessing and reloading.
        return;
    };

    if deployed_build == running {
        // in sync — clear the guard so a genuine future update can auto-apply
        if let Some(s) = session() {
            let _ = s.remove_item(RELOAD_GUARD);
        }
        // and retract any stale update notice: the reload worked.
        PENDING.with(|p| *p.borrow_mut() = None);
        super::notify::dismiss(OTA_ID);
        return;
    }

    // have we already reloaded trying to reach this exact build? if so the
    // reload did not take (a cached shell, a proxy, a half-published deploy)
    // and reloading again would just spin.
    let already_tried = session()
        .and_then(|s| s.get_item(RELOAD_GUARD).ok().flatten())
        .map(|v| v == deployed_build)
        .unwrap_or(false);

    show_banner(&deployed_build, &message, run_in_progress, already_tried);
}

/// the single id every update notice is written to. keeping one id means a
/// notice can change what it says — "waiting for the run to finish" then
/// "ready" — instead of leaving a stale card claiming a run is still going.
const OTA_ID: &str = "ota-update";

thread_local! {
    /// the build a pending update would move us to, so the notification's
    /// action button knows what it is reloading toward.
    static PENDING: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// invoked by the notification's action button.
pub fn reload_for_pending() {
    let sha = PENDING.with(|p| p.borrow().clone());
    if let Some(sha) = sha {
        if let Some(s) = session() {
            let _ = s.set_item(RELOAD_GUARD, &sha);
        }
    }
    reload_now();
}

fn show_banner(sha: &str, message: &str, run_in_progress: bool, already_tried: bool) {
    PENDING.with(|p| *p.borrow_mut() = Some(sha.to_string()));

    let title = format!("✨ new version {sha} available");
    let (body, label) = if already_tried {
        (
            format!(
                "{message}\n\nalready reloaded once for this build and still running the old one. \
                 the browser may be serving a cached shell — try a hard refresh."
            ),
            "reload again",
        )
    } else if run_in_progress {
        (
            format!("{message}\n\na run is in progress; this will apply when it finishes."),
            "update now anyway",
        )
    } else {
        (format!("{message}\n\nupdating automatically…"), "update now")
    };

    super::notify::upsert(
        OTA_ID,
        &title,
        &body,
        Some((label.to_string(), super::notify::Action::Reload)),
    );

    if !already_tried && !run_in_progress {
        arm_reload(sha, RELOAD_DELAY_MS);
    }
}

fn arm_reload(sha: &str, delay_ms: i32) {
    if let Some(s) = session() {
        let _ = s.set_item(RELOAD_GUARD, sha);
    }
    schedule_reload(delay_ms);
}

fn reload_now() {
    if let Some(w) = web_sys::window() {
        // from the network, not the bfcache, so the new wasm is what loads.
        let _ = w.location().reload_with_forceget(true);
    }
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

/// called when a run ends while an update is pending, so the deferred reload
/// happens the moment it is safe.
///
/// this also has to correct what the notification SAYS. previously the notice
/// was rendered once and kept claiming "a run is in progress" long after the
/// run had ended, with no way to act on it.
pub fn apply_pending_if_any() {
    let Some(sha) = PENDING.with(|p| p.borrow().clone()) else {
        return;
    };

    let already_tried = session()
        .and_then(|s| s.get_item(RELOAD_GUARD).ok().flatten())
        .map(|v| v == sha)
        .unwrap_or(false);

    if already_tried {
        super::notify::upsert(
            OTA_ID,
            &format!("✨ new version {sha} available"),
            "a reload already failed to pick this build up — try a hard refresh.",
            Some(("reload again".to_string(), super::notify::Action::Reload)),
        );
        return;
    }

    super::notify::upsert(
        OTA_ID,
        &format!("✨ new version {sha} available"),
        "the run has finished — updating now.",
        Some(("update now".to_string(), super::notify::Action::Reload)),
    );
    arm_reload(&sha, RELOAD_DELAY_MS);
}
