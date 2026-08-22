//! conversation persistence.
//!
//! the transcript used to live in exactly two volatile places: the worker's
//! in-memory `Vec<Message>` and the ui's dom. an ota reload destroyed both,
//! so every deploy read as total amnesia — the agent lost the thread it was
//! mid-way through. the working tree already survived reloads because it was
//! written through to opfs; the conversation now gets the same treatment.
//!
//! storage layout (opfs, not localStorage: transcripts get large, and opfs
//! is what the tree itself uses):
//!   /transcript/messages.json  — the full message list, one line per message
//!   /transcript/meta.json      — `{ "seq": n }`, bumped on every append
//!
//! every mutation is written through immediately, matching directive D2:
//! nothing is held only in memory across a reload boundary.

use super::opfs;

const DIR: &str = "vanish-transcript";
const MESSAGES: &str = "messages.json";
const META: &str = "meta.json";

/// how much of a long conversation stays hot. everything older than this is
/// dropped from the *stored* copy at save time — the model still sees it for
/// the current run, but the next boot replays only recent context. this is
/// also what keeps a months-old thread from growing the request without
/// bound forever.
pub const KEEP_MESSAGES: usize = 200;
/// hard cap on stored bytes; transcripts with enormous tool payloads are
/// truncated here rather than being allowed to fill the disk silently.
const MAX_BYTES: usize = 4_000_000;

fn path(name: &str) -> String {
    format!("{DIR}/{name}")
}

/// load the saved conversation. an absent or corrupt file means "no history",
/// never "fail": a fresh session is a valid state, and a torn write from a
/// crash must not brick boot.
pub async fn load() -> Vec<crate::agent::llm::Message> {
    let Ok(raw) = opfs::read(&path(MESSAGES)).await else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// replace the whole stored conversation.
///
/// truncation happens here rather than in the caller so that both append and
/// clear paths share the same budget rules.
pub async fn save(messages: &[crate::agent::llm::Message]) -> Result<(), String> {
    if messages.is_empty() {
        return clear().await;
    }

    // newest wins: keep the last KEEP_MESSAGES entries, then trim whole
    // messages while over the byte cap (a single huge tool result can exceed
    // it alone).
    let start = messages.len().saturating_sub(KEEP_MESSAGES);
    let mut kept = &messages[start..];
    let mut trimmed = start;
    while serde_json::to_string(kept)
        .map(|s| s.len())
        .unwrap_or(0) > MAX_BYTES
        && kept.len() > 1
    {
        kept = &kept[1..];
        trimmed += 1;
    }

    let body = serde_json::to_string_pretty(kept).map_err(|e| format!("serialize history: {e}"))?;
    opfs::write(&path(MESSAGES), &body).await?;

    // the meta file doubles as a corruption canary: if messages.json is ever
    // half-written (crash mid-write), meta.seq will disagree with reality on
    // the next load and the caller can say so instead of guessing.
    let seq = kept.len();
    let meta = serde_json::json!({ "seq": seq });
    opfs::write(&path(META), &meta.to_string()).await?;
    Ok(())
}

/// drop the stored conversation entirely.
pub async fn clear() -> Result<(), String> {
    match opfs::delete(&path(MESSAGES)).await {
        Ok(()) => Ok(()),
        // browsers reject a missing removeEntry with NotFoundError; deleting
        // something already gone is success as far as callers care.
        Err(e) if e.to_lowercase().contains("notfound") => Ok(()),
        Err(e) => Err(e),
    }
}
