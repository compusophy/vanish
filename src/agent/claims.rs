//! the path-claim registry (STACKED_PRS_PLAN §2 C1).
//!
//! two conversations editing the same file concurrently cannot see each
//! other's intent; today the conflict first surfaces at merge time, when the
//! context that produced the edit is long gone. the registry is the cheapest
//! possible early warning: a pure map of `path -> conversation id` consulted
//! before every write. it does not block anything — the write proceeds — but
//! the tool result carries the collision so both agents can react while both
//! are still alive.
//!
//! zero network cost, zero persistence requirements (claims are advisory and
//! rebuilt by use), and no locking: this is coordination, not exclusion.

use std::collections::BTreeMap;

/// how long a claim outlives its owner going silent, in milliseconds.
/// an overnight loop re-arms itself every few seconds; a run that has been
/// gone for this long is not coming back to finish the edit. 30 minutes
/// spans any normal step; it is deliberately NOT hours, because a stale
/// claim that never expires is a permanent false alarm.
pub const CLAIM_TTL_MS: i64 = 30 * 60 * 1000;

/// one live claim on one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub conversation: String,
    pub claimed_at_ms: i64,
}

/// what a write should be told about the path it is about to touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimVerdict {
    /// nobody else holds this path (or only THIS conversation does — that is
    /// just the conversation's own prior work continuing, not a conflict).
    Clear,
    /// another conversation holds this path. proceed, but say so.
    Contested {
        /// who holds it now.
        holder: String,
    },
}

impl ClaimVerdict {
    /// true when the tool result must carry a warning. pure: pinned by tests
    /// so a "silent contested" regression cannot ship.
    pub fn is_contested(&self) -> bool {
        matches!(self, ClaimVerdict::Contested { .. })
    }
}

#[derive(Debug, Default)]
pub struct ClaimRegistry {
    claims: BTreeMap<String, Claim>,
}

impl ClaimRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// decide what a write to `path` by `conversation` should be told,
    /// BEFORE recording the new claim. expired claims do not contest:
    /// expiry is checked against `now_ms`, which keeps this pure and
    /// testable without a clock.
    pub fn check(&self, path: &str, conversation: &str, now_ms: i64) -> ClaimVerdict {
        match self.claims.get(path) {
            Some(c)
                if c.conversation != conversation && !is_expired(c, now_ms) =>
            {
                ClaimVerdict::Contested {
                    holder: c.conversation.clone(),
                }
            }
            _ => ClaimVerdict::Clear,
        }
    }

    /// record/refresh a conversation's claim on a path. returns the same
    /// verdict as `check` so callers can consult-and-record in one call
    /// without racing themselves between the two steps.
    pub fn claim(&mut self, path: &str, conversation: &str, now_ms: i64) -> ClaimVerdict {
        let verdict = self.check(path, conversation, now_ms);
        self.claims.insert(
            path.to_string(),
            Claim {
                conversation: conversation.to_string(),
                claimed_at_ms: now_ms,
            },
        );
        verdict
    }

    /// drop every claim owned by `conversation` — called when its run ends,
    /// so finished work stops contesting paths it no longer touches.
    pub fn release_conversation(&mut self, conversation: &str) -> Vec<String> {
        let owned: Vec<String> = self
            .claims
            .iter()
            .filter(|(_, c)| c.conversation == conversation)
            .map(|(p, _)| p.clone())
            .collect();
        for p in &owned {
            self.claims.remove(p);
        }
        owned
    }

    /// drop every claim on the named paths, whoever owns them — called after
    /// a commit lands those files, because merged content supersedes any
    /// claim made against pre-commit state.
    pub fn release_paths(&mut self, paths: &[String]) -> usize {
        let mut dropped = 0;
        for p in paths {
            if self.claims.remove(p).is_some() {
                dropped += 1;
            }
        }
        dropped
    }

    /// entries whose owners went silent past CLAIM_TTL_MS, dropped against
    /// `now_ms`. returns what was removed so the caller can surface it.
    pub fn expire(&mut self, now_ms: i64) -> Vec<String> {
        let dead: Vec<String> = self
            .claims
            .iter()
            .filter(|(_, c)| is_expired(c, now_ms))
            .map(|(p, _)| p.clone())
            .collect();
        for p in &dead {
            self.claims.remove(p);
        }
        dead
    }

    pub fn len(&self) -> usize {
        self.claims.len()
    }

    pub fn is_empty(&self) -> bool {
        self.claims.is_empty()
    }

    /// snapshot for git_status / diagnostics. pure view, no mutation.
    pub fn entries(&self) -> Vec<(String, String)> {
        self.claims
            .iter()
            .map(|(p, c)| (p.clone(), c.conversation.clone()))
            .collect()
    }
}

fn is_expired(c: &Claim, now_ms: i64) -> bool {
    // saturating: a backwards clock must read as "very old", never as
    // negative-and-therefore-fresh.
    now_ms.saturating_sub(c.claimed_at_ms) > CLAIM_TTL_MS
}

// ---- session-scoped registry -------------------------------------------
//
// a workspace is created per run, but claims must outlive runs: their whole
// point is warning conversation B about work conversation A did EARLIER.
// the worker keeps ONE registry for its lifetime (thread_local, same pattern
// as WorkerState); these accessors are the only way tools touch it, so the
// pure core above stays directly unit-testable without any global state.

use std::cell::RefCell;

thread_local! {
    static REGISTRY: RefCell<ClaimRegistry> = RefCell::new(ClaimRegistry::new());
}

/// consult-and-record, globally. returns the verdict the caller should act
/// on (the claim IS recorded whether contested or not).
pub fn registry_claim(path: &str, conversation: &str, now_ms: i64) -> ClaimVerdict {
    REGISTRY.with(|r| r.borrow_mut().claim(path, conversation, now_ms))
}

/// drop every claim owned by `conversation` (run ended). returns the paths
/// released, for surfacing.
pub fn registry_release_conversation(conversation: &str) -> Vec<String> {
    REGISTRY.with(|r| r.borrow_mut().release_conversation(conversation))
}

/// drop claims on committed paths (merged content supersedes them).
pub fn registry_release_paths(paths: &[String]) -> usize {
    REGISTRY.with(|r| r.borrow_mut().release_paths(paths))
}

/// sweep expired claims; returns what went, for diagnostics.
pub fn registry_expire(now_ms: i64) -> Vec<String> {
    REGISTRY.with(|r| r.borrow_mut().expire(now_ms))
}

/// snapshot for git_status / diagnostics.
pub fn registry_entries() -> Vec<(String, String)> {
    REGISTRY.with(|r| r.borrow().entries())
}

/// the human-facing warning a mutating tool appends to its result when a
/// path is contested. shared by write_file and edit_file so their wording
/// cannot drift. pure; pinned by tests.
pub fn contest_warning(path: &str, holder: &str) -> String {
    format!(
        "⚠ path also claimed by conversation '{holder}' — you may both be editing {path}. \
         coordinate or expect a merge conflict."
    )
}
