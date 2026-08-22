//! the single wire contract between the ui thread and the agent worker.
//!
//! this module exists because the harness has twice shipped a ui/logic
//! mismatch that silently bricked the app — a client referencing dom or
//! event shapes the other half no longer produced. both sides now compile
//! against these types, so that class of bug becomes a build error instead
//! of a blank screen in production.

use serde::{Deserialize, Serialize};

/// ui thread -> agent worker
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// hand the worker its credentials and repo target; must precede Run.
    Configure(Config),
    /// start an agent run against the current conversation.
    Run { prompt: String, thread_id: String },
    /// cooperative cancel; the loop checks between steps and mid-stream.
    Stop,
    /// flush the working tree to github as one commit.
    Commit { message: String },
    /// re-read the working tree (used by the file explorer).
    ListTree,
    /// read one file out of the working tree.
    ReadFile { path: String },
    /// write one file into the working tree from the editor pane.
    WriteFile { path: String, content: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub openrouter_key: String,
    pub github_token: String,
    pub repo: String,
    pub branch: String,
    pub model: String,
    pub reasoning_effort: String,
    /// when true the loop never self-terminates; it runs until Stop.
    pub loop_mode: bool,
}

/// agent worker -> ui thread
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Event {
    Ready {
        build: String,
    },
    RunStarted {
        thread_id: String,
        model: String,
    },
    StepStarted {
        step: u32,
    },
    /// streamed reasoning delta
    Reasoning {
        delta: String,
    },
    /// streamed assistant text delta
    Content {
        delta: String,
    },
    ToolStarted {
        id: String,
        name: String,
        args: String,
    },
    ToolFinished {
        id: String,
        name: String,
        ok: bool,
        result: String,
    },
    /// the working tree changed; the ui should refresh the explorer/diff.
    TreeChanged {
        dirty: Vec<String>,
    },
    Committed {
        sha: String,
        message: String,
        files: usize,
    },
    RunFinished {
        steps: u32,
        reason: FinishReason,
    },
    /// every failure is surfaced. a silent catch here is what produced the
    /// "empty dropdown that does nothing" bug.
    Error {
        scope: String,
        message: String,
    },
    Tree {
        entries: Vec<TreeEntry>,
    },
    FileContent {
        path: String,
        content: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// the model declared the task done.
    Completed,
    /// the user pressed stop.
    Stopped,
    /// the loop hit its step ceiling.
    StepLimit,
    /// an unrecoverable error ended the run.
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: usize,
    /// true when the local copy differs from the last synced github blob.
    pub dirty: bool,
}

impl Event {
    /// events cross the worker boundary as plain json; both halves use this
    /// so the encoding can never diverge between sender and receiver.
    pub fn to_js(&self) -> Result<wasm_bindgen::JsValue, serde_wasm_bindgen::Error> {
        serde_wasm_bindgen::to_value(self)
    }
}
