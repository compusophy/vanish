//! the single wire contract between the ui thread and the agent worker.
//!
//! this module exists because the harness has twice shipped a ui/logic
//! mismatch that silently bricked the app — a client referencing dom or event
//! shapes the other half no longer produced. both sides now compile against
//! these types, so that class of bug becomes a build error instead of a blank
//! screen in production.

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
    /// forget the saved conversation. the ui clears its feed on confirmation.
    ClearHistory,
    /// start a fresh thread, leaving existing ones intact.
    NewConversation,
    /// make an existing thread active and replay it.
    SwitchConversation { id: String },
    /// discard one thread.
    DeleteConversation { id: String },
    /// ask for the thread list (the ui renders it in the left rail).
    ListConversations,
    /// ui -> worker health check: "do you believe a run is in flight?".
    /// the answer drives dock reconciliation — if RunFinished is ever lost,
    /// delayed, or unparseable, this is what unsticks the run/stop buttons.
    RunState,
    /// make this worker own a specific existing conversation, replacing
    /// whatever it loaded at boot. phase-2 groundwork: a per-conversation
    /// worker spawns fresh and always boots on index.active — Attach is how
    /// the pool points it at the thread it was created for. unlike
    /// SwitchConversation it does NOT touch index.active: ownership of the
    /// "which thread is on screen" question moves to the ui's worker pool,
    /// and several workers coexisting means no single global active id.
    Attach { id: String },
    /// queue several tasks to run sequentially, each as its own one-shot run
    /// ending at task_complete. this is the programmatic driver: a benchmark
    /// harness submits work through it instead of typing into the prompt box,
    /// and gets Event::BatchFinished back with machine-readable results.
    /// the queue survives tab discards (it persists beside the resume
    /// marker); the user pressing stop cancels whatever is left.
    RunBatch { tasks: Vec<BatchTask> },
    /// replace the running reasoning policy with a rustlite source, live.
    /// `manifest` may be empty, meaning "the default reasoner manifest";
    /// `source` is rustlite, compiled in the worker. this is the ui half of
    /// the cartridge story: the agent's own reasoning module is a textarea
    /// away from being something else, mid-conversation, with its memory
    /// intact.
    SwapCartridge { manifest: String, source: String },
    /// run the internal eval suite: pinned self-edit tasks through RunBatch,
    /// then grade the working tree against mechanical checkers. results land
    /// in vanish-bench/report.json and Event::BenchmarkFinished carries the
    /// scorecard.
    RunBenchmark,
}

/// one unit of queued work. `id` is caller-chosen (e.g. "bench-001") and is
/// what results are keyed by — prompts are data, ids are identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchTask {
    pub id: String,
    pub prompt: String,
}

/// the outcome of one batch task. `reason` mirrors FinishReason as a plain
/// snake_case string so the exported results file needs no schema beyond
/// json: "completed" | "stopped" | "step_limit" | "failed".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchResult {
    pub id: String,
    pub reason: String,
    pub steps: u32,
}

impl Config {
    /// where the ui mirrors the saved config for the worker to self-load at
    /// boot (opfs is visible from both contexts; localStorage is not).
    pub const MIRROR_PATH: &'static str = "vanish-config/config.json";
}

/// opfs path of the config mirror, as a plain constant for callers that have
/// no Config value handy.
pub const CONFIG_MIRROR_PATH: &str = Config::MIRROR_PATH;

impl Config {
    /// whether there is any point contacting the services yet.
    pub fn is_usable(&self) -> bool {
        !self.openrouter_key.is_empty()
            && !self.github_token.is_empty()
            && !self.repo.is_empty()
    }
}

/// `serde(default)` on the container matters: a config saved by an older
/// build that lacks a newer field must still load. without it, any deploy
/// that touched this shape silently failed to parse the stored json, wiped
/// the user's credentials, and made them press "save settings" on every
/// page load even though the fields looked filled (by browser autofill,
/// not by us).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub openrouter_key: String,
    pub github_token: String,
    pub repo: String,
    pub branch: String,
    pub model: String,
    pub reasoning_effort: String,
    /// optional. without it the agent learns only THAT a build failed;
    /// with it, it can read the compiler output and fix its own commit.
    pub vercel_token: String,
    /// only needed for team-scoped projects.
    pub vercel_team_id: String,
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
    /// result of checking the saved credentials against the real services.
    /// there is no server to validate them at sign-in time, so this is what
    /// replaces "you are logged in": each credential is actually exercised
    /// and the outcome is stated, rather than discovered on the first run.
    ConfigStatus {
        openrouter_ok: bool,
        github_ok: bool,
        /// `None` when no vercel token is set — which is a valid state, not a
        /// failure. `Some(false)` means one was set and does not work.
        vercel_ok: Option<bool>,
        /// human-readable, and specific about which half failed and why.
        detail: String,
    },
    RunStarted {
        thread_id: String,
        model: String,
    },
    StepStarted {
        /// which conversation produced this step. serde(default) keeps older
        /// payloads (and any worker/ui skew across an ota reload) parseable;
        /// an absent tag routes to the active thread.
        #[serde(default)]
        thread: String,
        step: u32,
    },
    /// streamed reasoning delta
    Reasoning {
        #[serde(default)]
        thread: String,
        delta: String,
    },
    /// streamed assistant text delta
    Content {
        #[serde(default)]
        thread: String,
        delta: String,
    },
    ToolStarted {
        #[serde(default)]
        thread: String,
        id: String,
        name: String,
        args: String,
    },
    ToolFinished {
        #[serde(default)]
        thread: String,
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
        #[serde(default)]
        thread: String,
        steps: u32,
        reason: FinishReason,
    },
    /// every failure is surfaced. a silent catch here is what produced the
    /// "empty dropdown that does nothing" bug.
    Error {
        #[serde(default)]
        thread: String,
        scope: String,
        message: String,
    },
    /// informational, not an error: boot notes, loop-resume notices.
    Note {
        #[serde(default)]
        thread: String,
        text: String,
    },
    Tree {
        entries: Vec<TreeEntry>,
    },
    FileContent {
        path: String,
        content: String,
    },
    /// the saved conversation was loaded at boot; the ui should replay it so
    /// a reload (ota or manual) does not look like amnesia.
    /// `trimmed` reports how much of the transcript was too old to keep.
    HistoryRestored {
        turns: Vec<HistoryTurn>,
        trimmed: usize,
    },
    /// the saved conversation was discarded at the ui's request.
    HistoryCleared,
    /// answer to Command::RunState. `running` is the worker's own belief,
    /// not a ui-side flag — the two can disagree exactly when the dock is
    /// stuck, and the worker wins because the run lives there.
    RunStateReport { running: bool },
    /// the full thread list plus which one is active.
    Conversations {
        items: Vec<ConversationSummary>,
        active: String,
    },
    /// a queued batch ended (finished, or cancelled by stop). `results` is
    /// also written to opfs at vanish-batch/results.json — this event is the
    /// live notification, that file is the durable export.
    BatchFinished {
        results: Vec<BatchResult>,
        /// "completed" when every task ran; "cancelled" when stop cut the
        /// queue short.
        status: String,
    },
    /// the internal eval suite finished grading. `passed`/`total` are the
    /// headline; the full per-task report lives in vanish-bench/report.json
    /// and in the scorecard note that precedes this event.
    BenchmarkFinished { passed: usize, total: usize },
}

/// one thread as the sidebar shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub count: usize,
}

/// one exchange, replayed after a reload. this is a display shape, not the
/// wire shape: tool traffic collapses into one line per call so restoring a
/// hundred-step run renders in a handful of cards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryTurn {
    pub role: String,
    pub content: Option<String>,
    /// "⚡ name args" summaries for assistant turns that called tools.
    pub tools: Vec<String>,
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
    /// which conversation a run-scoped event belongs to, for feed routing.
    /// `""` means "untagged" — boot traffic and pre-phase-2 events — which
    /// the ui treats as belonging to whatever thread is active.
    pub fn thread(&self) -> &str {
        match self {
            Event::RunStarted { thread_id, .. } => thread_id,
            Event::StepStarted { thread, .. }
            | Event::Reasoning { thread, .. }
            | Event::Content { thread, .. }
            | Event::ToolStarted { thread, .. }
            | Event::ToolFinished { thread, .. }
            | Event::RunFinished { thread, .. }
            | Event::Error { thread, .. }
            | Event::Note { thread, .. } => thread,
            _ => "",
        }
    }

    /// whether handling this event should touch the dock's run/stop state.
    /// RunStateReport is deliberately EXCLUDED: it is a background health
    /// signal, and letting it drive set_status would make the heartbeat able
    /// to clobber the visible status line mid-run. the ui reads its payload
    /// directly instead.
    pub fn touches_run_state(&self) -> bool {
        matches!(
            self,
            Event::RunStarted { .. } | Event::RunFinished { .. }
        )
    }

    /// events cross the worker boundary as plain json; both halves use this
    /// so the encoding can never diverge between sender and receiver.
    pub fn to_js(&self) -> Result<wasm_bindgen::JsValue, serde_wasm_bindgen::Error> {
        serde_wasm_bindgen::to_value(self)
    }
}
