//! the ui thread: dom only.
//!
//! it renders, it collects input, and it forwards commands to the worker.
//! it never calls an api and never runs the loop, so no amount of agent work
//! can make the page unresponsive — including the stop button, which in the
//! old single-threaded client could be starved by the very run it was
//! supposed to cancel.

mod feed;
mod update;

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{Document, Element, HtmlInputElement, HtmlSelectElement, HtmlTextAreaElement,
              MessageEvent, Worker, WorkerOptions, WorkerType};

use crate::protocol::{Command, Config, Event};

pub fn doc() -> Document {
    web_sys::window()
        .expect("no window on the ui thread")
        .document()
        .expect("no document")
}

pub fn by_id(id: &str) -> Option<Element> {
    doc().get_element_by_id(id)
}

/// element lookups that the code depends on. a missing id means the html and
/// this binary disagree — the exact drift that twice shipped a blank page —
/// so it is reported loudly instead of being silently skipped.
fn require(id: &str) -> Element {
    match by_id(id) {
        Some(e) => e,
        None => {
            let msg = format!(
                "ui element #{id} is missing from the page. the html and the wasm build are out of sync."
            );
            web_sys::console::error_1(&JsValue::from_str(&msg));
            fatal(&msg);
            panic!("{msg}");
        }
    }
}

fn fatal(message: &str) {
    if let Some(body) = doc().body() {
        let banner = doc().create_element("div").unwrap();
        banner.set_class_name("fatal");
        banner.set_text_content(Some(message));
        let _ = body.prepend_with_node_1(&banner);
    }
}

pub fn create(tag: &str, class: &str) -> Element {
    let e = doc().create_element(tag).unwrap();
    if !class.is_empty() {
        e.set_class_name(class);
    }
    e
}

fn input_value(id: &str) -> String {
    by_id(id)
        .and_then(|e| e.dyn_into::<HtmlInputElement>().ok())
        .map(|i| i.value())
        .unwrap_or_default()
}

fn set_input(id: &str, value: &str) {
    if let Some(i) = by_id(id).and_then(|e| e.dyn_into::<HtmlInputElement>().ok()) {
        i.set_value(value);
    }
}

fn select_value(id: &str) -> String {
    by_id(id)
        .and_then(|e| e.dyn_into::<HtmlSelectElement>().ok())
        .map(|s| s.value())
        .unwrap_or_default()
}

fn checkbox(id: &str) -> bool {
    by_id(id)
        .and_then(|e| e.dyn_into::<HtmlInputElement>().ok())
        .map(|i| i.checked())
        .unwrap_or(false)
}

// ---- persisted settings ----------------------------------------------
// credentials live in localStorage because there is no server to hold them.
// that is the explicit trade of a backendless harness: anything with script
// access to this origin can read them. rotate them like browser passwords.

const STORE: &str = "vanish.config";

fn load_config() -> Config {
    let stored: Option<Config> = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(STORE).ok().flatten())
        .and_then(|raw| serde_json::from_str(&raw).ok());

    // defaults fill gaps rather than applying only to a blank slate. a
    // half-finished config — one credential saved, the rest untouched — used
    // to suppress every default, so the repo field came back empty and had
    // to be typed by hand for no reason.
    let mut cfg = stored.unwrap_or_default();
    if cfg.repo.trim().is_empty() {
        // this harness edits its own repository, so defaulting to it is
        // correct rather than presumptuous.
        cfg.repo = "compusophy/vanish".to_string();
    }
    if cfg.branch.trim().is_empty() {
        cfg.branch = "main".to_string();
    }
    if cfg.model.trim().is_empty() {
        cfg.model = "stealth/ox-alpha".to_string();
    }
    if cfg.reasoning_effort.trim().is_empty() {
        cfg.reasoning_effort = "high".to_string();
    }
    cfg
}

fn save_config(cfg: &Config) -> Result<(), String> {
    let storage = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .ok_or("browser storage is unavailable (private mode or blocked cookies)")?;
    let raw = serde_json::to_string(cfg).map_err(|e| e.to_string())?;
    storage
        .set_item(STORE, &raw)
        // a silent failure here is why settings used to appear to save and
        // then come back empty after a reload.
        .map_err(|_| "browser refused to persist settings (storage full or blocked)".to_string())
}

pub struct Ui {
    pub worker: Worker,
    pub config: Config,
    pub running: bool,
    /// build id the worker reported; compared against github to spot deploys.
    pub build: String,
}

pub type Shared = Rc<RefCell<Ui>>;

pub fn send(worker: &Worker, cmd: &Command) {
    match serde_wasm_bindgen::to_value(cmd) {
        Ok(v) => {
            if let Err(e) = worker.post_message(&v) {
                feed::error("ui", &format!("could not reach the agent worker: {e:?}"));
            }
        }
        Err(e) => feed::error("ui", &format!("could not encode command: {e}")),
    }
}

#[wasm_bindgen]
pub fn boot_ui(worker_url: &str) {
    console_error_panic_hook::set_once();

    let opts = WorkerOptions::new();
    // module type: the worker imports the wasm-bindgen glue as an es module.
    opts.set_type(WorkerType::Module);

    let worker = match Worker::new_with_options(worker_url, &opts) {
        Ok(w) => w,
        Err(e) => {
            fatal(&format!(
                "could not start the agent worker ({e:?}). the harness cannot run without it."
            ));
            return;
        }
    };

    let config = load_config();
    hydrate_settings(&config);

    let ui: Shared = Rc::new(RefCell::new(Ui {
        worker,
        config,
        running: false,
        build: String::new(),
    }));

    wire_worker(&ui);
    wire_controls(&ui);
    update::start_watching(&ui);

    // hand the worker its credentials immediately so it is usable the moment
    // the page loads rather than only after the settings panel is touched.
    let cfg = ui.borrow().config.clone();
    let configured = cfg.is_usable();
    send(&ui.borrow().worker, &Command::Configure(cfg));

    if configured {
        send(&ui.borrow().worker, &Command::ListTree);
    } else {
        // asking github for a tree with no token returns a 401 that reads
        // like a bug. explain the actual situation instead.
        feed::setup_required(
            ui.borrow().config.openrouter_key.is_empty(),
            ui.borrow().config.github_token.is_empty() || ui.borrow().config.repo.is_empty(),
        );
    }
}

fn hydrate_settings(cfg: &Config) {
    set_input("cfg-key", &cfg.openrouter_key);
    set_input("cfg-token", &cfg.github_token);
    set_input("cfg-repo", &cfg.repo);
    set_input("cfg-branch", &cfg.branch);
    if let Some(s) = by_id("cfg-model").and_then(|e| e.dyn_into::<HtmlInputElement>().ok()) {
        s.set_value(&cfg.model);
    }
    if let Some(s) = by_id("cfg-effort").and_then(|e| e.dyn_into::<HtmlSelectElement>().ok()) {
        s.set_value(&cfg.reasoning_effort);
    }
    if let Some(c) = by_id("cfg-loop").and_then(|e| e.dyn_into::<HtmlInputElement>().ok()) {
        c.set_checked(cfg.loop_mode);
    }
}

fn collect_settings() -> Config {
    Config {
        openrouter_key: input_value("cfg-key"),
        github_token: input_value("cfg-token"),
        repo: input_value("cfg-repo").trim().to_string(),
        branch: {
            let b = input_value("cfg-branch");
            if b.trim().is_empty() {
                "main".to_string()
            } else {
                b.trim().to_string()
            }
        },
        model: {
            let m = input_value("cfg-model");
            if m.trim().is_empty() {
                "stealth/ox-alpha".to_string()
            } else {
                m.trim().to_string()
            }
        },
        reasoning_effort: {
            let e = select_value("cfg-effort");
            if e.is_empty() {
                "high".to_string()
            } else {
                e
            }
        },
        loop_mode: checkbox("cfg-loop"),
    }
}

fn wire_worker(ui: &Shared) {
    let ui2 = ui.clone();
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        match serde_wasm_bindgen::from_value::<Event>(ev.data()) {
            Ok(event) => feed::render(&ui2, event),
            Err(e) => feed::error("ui", &format!("unrecognised event from worker: {e}")),
        }
    });
    ui.borrow()
        .worker
        .set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();

    // a worker that dies takes the agent with it; say so rather than leaving
    // the ui spinning on a run that will never report back.
    let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
        feed::error(
            "worker",
            "the agent worker crashed. reload the page to restart it; work already written to the tree is safe.",
        );
    });
    ui.borrow()
        .worker
        .set_onerror(Some(on_error.as_ref().unchecked_ref()));
    on_error.forget();
}

fn on_click<F: FnMut() + 'static>(id: &str, mut f: F) {
    let cb = Closure::<dyn FnMut()>::new(move || f());
    require(id)
        .add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
        .expect("could not attach click handler");
    cb.forget();
}

fn wire_controls(ui: &Shared) {
    // run
    {
        let ui = ui.clone();
        on_click("run", move || {
            let text = by_id("prompt")
                .and_then(|e| e.dyn_into::<HtmlTextAreaElement>().ok())
                .map(|t| t.value())
                .unwrap_or_default();
            if text.trim().is_empty() {
                return;
            }
            if let Some(t) = by_id("prompt").and_then(|e| e.dyn_into::<HtmlTextAreaElement>().ok()) {
                t.set_value("");
            }
            feed::user_message(&text);
            let worker = ui.borrow().worker.clone();
            send(
                &worker,
                &Command::Run {
                    prompt: text,
                    thread_id: "main".to_string(),
                },
            );
        });
    }

    // stop — always responsive, because the loop is on another thread
    {
        let ui = ui.clone();
        on_click("stop", move || {
            let worker = ui.borrow().worker.clone();
            send(&worker, &Command::Stop);
            feed::note("stop requested — finishing the current chunk");
        });
    }

    // save settings
    {
        let ui = ui.clone();
        on_click("cfg-save", move || {
            let cfg = collect_settings();
            match save_config(&cfg) {
                Ok(()) => {
                    let worker = ui.borrow().worker.clone();
                    send(&worker, &Command::Configure(cfg.clone()));
                    ui.borrow_mut().config = cfg;
                    // the tree is fetched only once ConfigStatus confirms the
                    // token works; asking now would emit a 401 card that says
                    // nothing the credential check is not about to say better.
                    feed::note("settings saved — checking credentials");
                }
                Err(e) => feed::error("settings", &e),
            }
        });
    }

    // manual commit of whatever the tree holds
    {
        let ui = ui.clone();
        on_click("commit", move || {
            let message = input_value("commit-msg");
            let message = if message.trim().is_empty() {
                "manual commit from vanish".to_string()
            } else {
                message
            };
            let worker = ui.borrow().worker.clone();
            send(&worker, &Command::Commit { message });
        });
    }

    // forget the conversation. the worker clears memory and opfs, then
    // confirms; the feed is wiped only on that confirmation, so a failed
    // clear never shows an empty transcript over a live history.
    {
        let ui = ui.clone();
        on_click("clear-history", move || {
            let worker = ui.borrow().worker.clone();
            send(&worker, &Command::ClearHistory);
        });
    }

    // enter to run, shift+enter for a newline
    {
        let ui = ui.clone();
        let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(
            move |ev: web_sys::KeyboardEvent| {
                if ev.key() == "Enter" && !ev.shift_key() {
                    ev.prevent_default();
                    let text = by_id("prompt")
                        .and_then(|e| e.dyn_into::<HtmlTextAreaElement>().ok())
                        .map(|t| t.value())
                        .unwrap_or_default();
                    if text.trim().is_empty() {
                        return;
                    }
                    if let Some(t) =
                        by_id("prompt").and_then(|e| e.dyn_into::<HtmlTextAreaElement>().ok())
                    {
                        t.set_value("");
                    }
                    feed::user_message(&text);
                    let worker = ui.borrow().worker.clone();
                    send(
                        &worker,
                        &Command::Run {
                            prompt: text,
                            thread_id: "main".to_string(),
                        },
                    );
                }
            },
        );
        require("prompt")
            .add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref())
            .expect("could not attach keydown handler");
        cb.forget();
    }
}
