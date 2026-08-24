//! conversation persistence.
//!
//! the transcript used to live in exactly two volatile places: the worker's
//! in-memory `Vec<Message>` and the ui's dom. an ota reload destroyed both,
//! so every deploy read as total amnesia — the agent lost the thread it was
//! mid-way through. the working tree already survived reloads because it was
//! written through to opfs; the conversation gets the same treatment.
//!
//! storage layout (opfs, not localStorage: transcripts get large, and opfs
//! is what the tree itself uses):
//!   /vanish-transcript/index.json     — {active, items:[{id,title,updated,count}]}
//!   /vanish-transcript/conv-<id>.json — that conversation's messages
//!
//! one file per conversation rather than one big blob: switching threads
//! should not deserialize every thread you have ever had.
//!
//! every mutation is written through immediately, matching directive D2:
//! nothing is held only in memory across a reload boundary.

use serde::{Deserialize, Serialize};

use super::opfs;
use crate::agent::llm::Message;

const DIR: &str = "vanish-transcript";
const INDEX: &str = "index.json";
/// the pre-multi-conversation file. still read once, to migrate.
const LEGACY_MESSAGES: &str = "messages.json";

/// how much of a long conversation stays hot. everything older than this is
/// dropped from the *stored* copy at save time — the model still sees it for
/// the current run, but the next boot replays only recent context. this is
/// also what keeps a months-old thread from growing the request without
/// bound forever.
pub const KEEP_MESSAGES: usize = 200;
/// hard cap on stored bytes per conversation; transcripts with enormous tool
/// payloads are truncated here rather than filling the disk silently.
const MAX_BYTES: usize = 4_000_000;

fn path(name: &str) -> String {
    format!("{DIR}/{name}")
}

fn conv_path(id: &str) -> String {
    path(&format!("conv-{id}.json"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMeta {
    pub id: String,
    pub title: String,
    /// epoch millis of the last write.
    pub updated: f64,
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Index {
    pub active: String,
    pub items: Vec<ConversationMeta>,
    /// present while ANY run is (or was) in flight — not only loop mode.
    /// a background tab discarded by the browser kills the worker with no
    /// event; this marker is what lets the next boot continue the run.
    /// see LoopResume.
    pub loop_resume: Option<LoopResume>,
    /// a queued batch (see control::BatchState). present only while tasks
    /// remain; cleared when the batch finishes or is cancelled. persisting
    /// it HERE means a tab discard mid-batch resumes the queue on next boot,
    /// exactly like an interrupted run — the batch driver gets the same
    /// durability every single run already has.
    pub batch: Option<String>,
}

/// a run that outlived its worker.
///
/// ANY run can now be interrupted by forces outside its control: a page
/// refresh, an ota reload, or — the silent one — the browser discarding a
/// hidden tab (memory saver, mobile os). that discard kills the worker with
/// no event at all; the per-step checkpoints mean the transcript survives,
/// but without this marker the next boot just replays a dead run. the
/// marker is what turns "interrupted" into "paused": boot adopts the marked
/// conversation and continues the work.
///
/// the prompt is re-sent as a nudge, not replayed verbatim: after a reload
/// the model already has every prior step in context, so "the run you were
/// working on was interrupted; continue" is the correct continuation signal
/// — the original prompt would read as a second, duplicate instruction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopResume {
    /// the conversation the interrupted run belonged to. resuming into any
    /// other thread would inject the nudge into the wrong context.
    pub conversation: String,
    /// the prompt that started the run, kept for display and for the case
    /// where the transcript was trimmed past it.
    pub prompt: String,
    /// epoch millis of the interruption, for the ui to say how long ago it
    /// was and for a stale-marker sanity check at boot.
    pub interrupted_at: f64,
    /// true only when the interrupted run had loop mode on. serde(default)
    /// keeps markers written by older builds parseable: they were always
    /// loop runs, so absence reads as true.
    #[serde(default = "default_true")]
    pub loop_mode: bool,
}

fn default_true() -> bool {
    true
}

impl Index {
    pub fn sorted(&self) -> Vec<ConversationMeta> {
        let mut v = self.items.clone();
        // most recently touched first: that is what the user is looking for.
        v.sort_by(|a, b| b.updated.partial_cmp(&a.updated).unwrap_or(std::cmp::Ordering::Equal));
        v
    }
}

fn now() -> f64 {
    js_sys::Date::now()
}

pub fn new_id() -> String {
    // millisecond timestamps are unique enough for threads created by hand,
    // and they sort chronologically as a bonus.
    format!("{}", now() as u64)
}

/// first line of the first user message, which is what a person recognises a
/// thread by. falls back to something neutral rather than an empty row.
pub fn title_from(messages: &[Message]) -> String {
    let raw = messages
        .iter()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    let line = raw.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if line.is_empty() {
        return "new conversation".to_string();
    }
    const MAX: usize = 48;
    if line.chars().count() <= MAX {
        line.to_string()
    } else {
        format!("{}…", line.chars().take(MAX).collect::<String>())
    }
}

pub async fn load_index() -> Index {
    if let Ok(raw) = opfs::read(&path(INDEX)).await {
        if let Ok(idx) = serde_json::from_str::<Index>(&raw) {
            return idx;
        }
    }
    // no index yet. if a pre-multi-conversation transcript exists, adopt it
    // rather than orphaning the thread the user was in the middle of.
    migrate_legacy().await
}

async fn migrate_legacy() -> Index {
    let Ok(raw) = opfs::read(&path(LEGACY_MESSAGES)).await else {
        return Index::default();
    };
    let messages: Vec<Message> = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(_) => return Index::default(),
    };
    if messages.is_empty() {
        return Index::default();
    }

    let id = new_id();
    let meta = ConversationMeta {
        title: title_from(&messages),
        count: messages.len(),
        updated: now(),
        id: id.clone(),
    };
    let index = Index {
        active: id.clone(),
        items: vec![meta],
        loop_resume: None,
    };

    if opfs::write(&conv_path(&id), &raw).await.is_ok() {
        let _ = save_index(&index).await;
        // the legacy file has been copied; removing it prevents a second
        // migration creating a duplicate thread on the next boot.
        let _ = opfs::delete(&path(LEGACY_MESSAGES)).await;
    }
    index
}

pub async fn save_index(index: &Index) -> Result<(), String> {
    let body = serde_json::to_string(index).map_err(|e| format!("serialize index: {e}"))?;
    opfs::write(&path(INDEX), &body).await
}

/// load one conversation. an absent or corrupt file means "no history",
/// never "fail": a fresh thread is a valid state, and a torn write from a
/// crash must not brick boot.
pub async fn load(id: &str) -> Vec<Message> {
    let Ok(raw) = opfs::read(&conv_path(id)).await else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// replace one conversation's stored messages and refresh its index entry.
pub async fn save(id: &str, messages: &[Message]) -> Result<(), String> {
    if messages.is_empty() {
        return Ok(());
    }

    // newest wins: keep the last KEEP_MESSAGES entries, then trim whole
    // messages while over the byte cap (a single huge tool result can exceed
    // it alone).
    let start = messages.len().saturating_sub(KEEP_MESSAGES);
    let mut kept = &messages[start..];
    while serde_json::to_string(kept).map(|s| s.len()).unwrap_or(0) > MAX_BYTES && kept.len() > 1 {
        kept = &kept[1..];
    }

    let body = serde_json::to_string(kept).map_err(|e| format!("serialize history: {e}"))?;
    opfs::write(&conv_path(id), &body).await?;

    let mut index = load_index().await;
    let title = title_from(kept);
    match index.items.iter_mut().find(|c| c.id == id) {
        Some(existing) => {
            existing.count = kept.len();
            existing.updated = now();
            // a thread named from its first prompt keeps that name; only a
            // placeholder gets replaced once a real prompt exists.
            if existing.title == "new conversation" {
                existing.title = title;
            }
        }
        None => index.items.push(ConversationMeta {
            id: id.to_string(),
            title,
            updated: now(),
            count: kept.len(),
        }),
    }
    index.active = id.to_string();
    save_index(&index).await
}

/// start a new empty thread and make it active.
pub async fn create() -> Result<String, String> {
    let id = new_id();
    let mut index = load_index().await;
    index.items.push(ConversationMeta {
        id: id.clone(),
        title: "new conversation".to_string(),
        updated: now(),
        count: 0,
    });
    index.active = id.clone();
    save_index(&index).await?;
    Ok(id)
}

/// drop one conversation. returns the id that should now be active.
pub async fn delete(id: &str) -> Result<String, String> {
    let mut index = load_index().await;
    index.items.retain(|c| c.id != id);

    match opfs::delete(&conv_path(id)).await {
        Ok(()) => {}
        // browsers reject a missing removeEntry with NotFoundError; deleting
        // something already gone is success as far as callers care.
        Err(e) if e.to_lowercase().contains("notfound") => {}
        Err(e) => return Err(e),
    }

    if index.active == id {
        index.active = index.sorted().first().map(|c| c.id.clone()).unwrap_or_default();
    }
    save_index(&index).await?;
    Ok(index.active.clone())
}

/// forget every conversation.
pub async fn clear_all() -> Result<(), String> {
    let index = load_index().await;
    for c in &index.items {
        let _ = opfs::delete(&conv_path(&c.id)).await;
    }
    save_index(&Index::default()).await
}

// ---- loop-mode resume marker -------------------------------------------

/// record that a loop run was in flight. called when a loop run starts;
/// cleared by `clear_loop_resume` when the run ends for any reason —
/// completed, stopped, failed, or step limit. a marker that survives its
/// own run's end would resurrect the loop on every future boot.
pub async fn set_loop_resume(marker: LoopResume) -> Result<(), String> {
    let mut index = load_index().await;
    index.loop_resume = Some(marker);
    save_index(&index).await
}

/// the marker, if one is pending. an empty active-conversation field or a
/// missing conversation means the marker is stale (the thread was deleted);
/// callers should treat that as None and clear it.
pub async fn take_loop_resume() -> Option<LoopResume> {
    let mut index = load_index().await;
    let marker = index.loop_resume.take();
    // always clear on read: resume happens exactly once. if resuming fails,
    // a fresh marker can be set by the next run; a persistent marker would
    // turn every boot into an involuntary run.
    if marker.is_some() {
        let _ = save_index(&index).await;
    }
    marker.filter(|m| !m.conversation.is_empty())
}

/// drop the marker without resuming (thread deleted, user declined).
pub async fn clear_loop_resume() {
    let mut index = load_index().await;
    if index.loop_resume.is_some() {
        index.loop_resume = None;
        let _ = save_index(&index).await;
    }
}

// ---- batch queue persistence ---------------------------------------------
//
// the batch state is stored as its serialized control::BatchState json in
// `Index.batch`. a String rather than a typed field keeps platform/ from
// depending on agent/control — the worker owns the (de)serialization, this
// module only owns durability.

/// persist the queue. called on every state transition so a discard at any
/// point resumes from the last known truth.
pub async fn set_batch(state_json: &str) -> Result<(), String> {
    let mut index = load_index().await;
    index.batch = Some(state_json.to_string());
    save_index(&index).await
}

/// the queue, if one is parked. does NOT clear it: unlike the resume marker,
/// a batch must survive repeated boots until it drains or is cancelled —
/// take-on-read would lose the remaining tasks on the first reload.
pub async fn get_batch() -> Option<String> {
    load_index().await.batch
}

/// the batch ended; forget it.
pub async fn clear_batch() {
    let mut index = load_index().await;
    if index.batch.take().is_some() {
        let _ = save_index(&index).await;
    }
}
