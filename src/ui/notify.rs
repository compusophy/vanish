//! the notification centre.
//!
//! update notices used to be a fixed card floating in the bottom-right
//! corner, directly over the dock — so an available update covered the stop
//! button, which is the control you most need when something is wrong. worse,
//! the card was rendered once: a notice that said "a run is in progress"
//! still said that after the run ended, because nothing re-rendered it.
//!
//! notifications now live behind a bell. they are addressed by id, so the
//! same notice updates in place instead of stacking or going stale, and
//! nothing overlaps the controls.

use std::cell::RefCell;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use super::{by_id, create};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// reload the page from the network (an update is waiting).
    Reload,
}

#[derive(Clone)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub body: String,
    pub action: Option<(String, Action)>,
    pub seen: bool,
}

thread_local! {
    static ITEMS: RefCell<Vec<Item>> = const { RefCell::new(Vec::new()) };
    static OPEN: RefCell<bool> = const { RefCell::new(false) };
}

/// add or replace a notification.
///
/// keyed by id on purpose: the update notice changes state (deferred while a
/// run is in flight, then ready) and must be able to say something new rather
/// than leaving a stale card on screen or pushing a duplicate.
pub fn upsert(
    id: &str,
    title: &str,
    body: &str,
    action: Option<(String, Action)>,
) {
    ITEMS.with(|items| {
        let mut items = items.borrow_mut();
        match items.iter_mut().find(|i| i.id == id) {
            Some(existing) => {
                // only an actual change re-marks it unread; re-rendering the
                // same content must not keep the badge lit forever.
                let changed = existing.title != title || existing.body != body;
                existing.title = title.to_string();
                existing.body = body.to_string();
                existing.action = action;
                if changed {
                    existing.seen = false;
                }
            }
            None => items.insert(
                0,
                Item {
                    id: id.to_string(),
                    title: title.to_string(),
                    body: body.to_string(),
                    action,
                    seen: false,
                },
            ),
        }
        // a long session should not accumulate an unbounded list.
        items.truncate(30);
    });
    render();
}

pub fn dismiss(id: &str) {
    ITEMS.with(|items| items.borrow_mut().retain(|i| i.id != id));
    render();
}

fn unseen() -> usize {
    ITEMS.with(|items| items.borrow().iter().filter(|i| !i.seen).count())
}

pub fn toggle() {
    let now_open = OPEN.with(|o| {
        let v = !*o.borrow();
        *o.borrow_mut() = v;
        v
    });
    if now_open {
        // opening is what counts as reading them.
        ITEMS.with(|items| {
            for i in items.borrow_mut().iter_mut() {
                i.seen = true;
            }
        });
    }
    render();
}

pub fn render() {
    let open = OPEN.with(|o| *o.borrow());

    if let Some(badge) = by_id("notif-badge") {
        let count = unseen();
        badge.set_text_content(Some(&count.to_string()));
        let _ = badge.set_attribute("style", if count == 0 { "display:none" } else { "" });
    }

    let Some(panel) = by_id("notif-panel") else {
        return;
    };
    let _ = panel.set_attribute("style", if open { "" } else { "display:none" });
    if !open {
        return;
    }

    panel.set_inner_html("");

    let head = create("div", "notif-head");
    head.set_text_content(Some("notifications"));
    let _ = panel.append_child(&head);

    let items = ITEMS.with(|i| i.borrow().clone());
    if items.is_empty() {
        let empty = create("div", "notif-empty");
        empty.set_text_content(Some("nothing yet"));
        let _ = panel.append_child(&empty);
        return;
    }

    for item in items {
        let card = create("div", "notif-item");

        let title = create("div", "notif-title");
        title.set_text_content(Some(&item.title));
        let _ = card.append_child(&title);

        if !item.body.is_empty() {
            let body = create("div", "notif-body");
            body.set_text_content(Some(&item.body));
            let _ = card.append_child(&body);
        }

        if let Some((label, action)) = item.action.clone() {
            let btn = create("button", "notif-action");
            btn.set_text_content(Some(&label));
            let cb = Closure::<dyn FnMut()>::new(move || match action {
                Action::Reload => super::update::reload_for_pending(),
            });
            let _ = btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
            cb.forget();
            let _ = card.append_child(&btn);
        }

        let close = create("button", "notif-dismiss");
        close.set_text_content(Some("×"));
        let id = item.id.clone();
        let cb = Closure::<dyn FnMut()>::new(move || dismiss(&id));
        let _ = close.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
        cb.forget();
        let _ = card.append_child(&close);

        let _ = panel.append_child(&card);
    }
}

pub fn wire() {
    if let Some(bell) = by_id("notif-bell") {
        let cb = Closure::<dyn FnMut()>::new(toggle);
        let _ = bell.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
        cb.forget();
    }
    render();
}
