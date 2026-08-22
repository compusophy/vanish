//! vanish — autonomous self-editing coding harness.
//!
//! architecture: the entire harness is a single wasm binary loaded twice.
//! `boot_ui` runs on the main thread and owns the dom; `boot_worker` runs
//! inside a web worker and owns the agent loop, the working tree, and every
//! network call. they speak only `protocol::Command` / `protocol::Event`.
//!
//! there is no server. the previous incarnation ran the loop inside a
//! serverless function, where the request lifetime bounded the run and the
//! staging area lived in per-request memory — so any run that outlived its
//! request lost all staged edits. a worker has no request and no deadline,
//! and the working tree lives in opfs, so neither failure mode exists here.

pub mod agent;
pub mod platform;
pub mod protocol;
pub mod ui;
pub mod worker;

use wasm_bindgen::prelude::*;

/// build identity, stamped at compile time. the ui compares this against the
/// copy it booted with to detect that a new version shipped.
pub const BUILD: &str = env!("VANISH_BUILD");

#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
pub fn build_id() -> String {
    BUILD.to_string()
}
