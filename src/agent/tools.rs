//! the tool surface, and the workspace it acts on.
//!
//! every mutating tool writes through to opfs immediately. there is no
//! staging map held in memory waiting to be flushed, because that design is
//! precisely what lost work every time a run ended early. "uncommitted" here
//! means bytes on disk that differ from the last synced github blob — a fact
//! that outlives the run, the tab, and the browser.

use crate::agent::github::{FileChange, Github};
use crate::agent::http;
use crate::platform::opfs::{self, Index, IndexEntry};

pub struct Workspace {
    pub github: Github,
    /// optional build-log reader. absent means check_deployment can report a
    /// verdict but not a cause.
    pub vercel: Option<crate::agent::vercel::Vercel>,
    pub index: Index,
    /// branch head as this session last saw it (sync_repo / git_commit).
    ///
    /// D10: git_commit refuses when the live head differs from this, because
    /// a commit built on unreconciled local files silently reverts whatever
    /// upstream landed — which has now happened three times, once reverting
    /// three commits while looking like a clean three-file diff.
    pub synced_head: String,
    /// set by `task_complete`; the loop reads it to know the model is done.
    pub completed: Option<String>,
}

/// what sync_repo should do with one locally-cached file.
#[derive(Debug, PartialEq, Eq)]
pub enum Reconcile {
    /// the local copy is trustworthy; nothing to do.
    Keep,
    /// the cache may be stale relative to the branch: drop it so the next
    /// read goes back to github. never applied to dirty files.
    Refresh,
}

/// pure decision function behind sync_repo reconciliation (D10).
///
/// dirty files are ALWAYS kept: they are uncommitted local work, and
/// dropping one is data loss — the single worst thing sync could do. clean
/// files whose recorded base blob sha matches what the branch reports stay;
/// anything else — including a base sha we never learned (a read-through
/// cache filled before any listing) — is treated as possibly stale rather
/// than trusted. distrust over convenience: a stale cache is how incidents
/// #1–#3 happened.
pub fn reconcile_entry(dirty: bool, base_sha: &str, remote_sha: Option<&str>) -> Reconcile {
    if dirty {
        return Reconcile::Keep;
    }
    match remote_sha {
        Some(remote) if remote == base_sha && !base_sha.is_empty() => Reconcile::Keep,
        _ => Reconcile::Refresh,
    }
}

/// what one reconciliation pass found. returned by
/// `Workspace::reconcile_against_branch` and rendered by both consumers
/// (the sync_repo tool and the worker's boot-time auto-reconcile).
#[derive(Debug, Clone)]
pub struct ReconcileReport {
    pub head: String,
    pub files_on_branch: usize,
    /// clean cached copies that were dropped because the branch moved.
    pub refreshed: Vec<String>,
    /// files whose local bytes differ from github — never dropped.
    pub uncommitted: Vec<String>,
}

/// whether the session-level auto-reconcile should run. pure so the test
/// suite pins both directions: it fires exactly once, and only for a token
/// that was actually exercised and worked.
pub fn should_auto_reconcile(github_usable: bool, already_reconciled: bool) -> bool {
    github_usable && !already_reconciled
}

// ---- branch policy (STACKED_PRS_PLAN §4; D10's sibling for refs) -----------
//
// main is promoted, never pushed blind. the rules live here as pure
// functions so tests can pin them; the tool layer only applies verdicts.

/// the production ref. every rule below keys off this one name.
pub const PROTECTED_BRANCH: &str = "main";

/// which branch a conversation commits to. pure: pinned by tests.
///
/// - a conversation that has claimed an agent/ branch keeps it (stable
///   identity across runs — re-deriving would scatter work).
/// - otherwise the DEFAULT is isolation: agent work lands on
///   `agent/{conversation-id}`, never on main directly.
pub fn branch_for_conversation(current: Option<&str>, conversation_id: &str) -> String {
    match current {
        Some(b) if Github::is_agent_ref(b) => b.to_string(),
        _ => format!("agent/{conversation_id}"),
    }
}

/// may a commit land directly on `branch`? pure: pinned by tests.
///
/// direct commits to the protected branch are refused with guidance;
/// anything else (agent/* today, future human branches) passes through.
pub fn commit_allowed_on(branch: &str) -> Result<(), String> {
    if branch == PROTECTED_BRANCH {
        return Err(format!(
            "REFUSED: '{PROTECTED_BRANCH}' is protected — commit to an agent/ branch \
             (git_create_branch + git_checkout) and promote it with open_pr once checks pass."
        ));
    }
    Ok(())
}

/// the merge gate: when may pr #n land on the protected branch? pure over
/// PrStatus so the whole discipline is testable without network access.
///
/// - mergeable must be a computed true (None = github still counting:
///   merging then is a coin flip, so we wait);
/// - ci must have SETTLED GREEN. "pending" refuses (a red build discovered
///   after merge is the expensive kind); "none" refuses too — an absent
///   signal is not a passing one, it is an unread one (D4).
pub fn pr_gate(status: &crate::agent::github::PrStatus) -> Result<(), String> {
    let n = status.number;
    if status.mergeable != Some(true) {
        return Err(format!(
            "pr #{n} is not mergeable yet (mergeable={:?}) — resolve conflicts first",
            status.mergeable
        ));
    }
    match status.deploy_verdict.as_str() {
        "success" => Ok(()),
        "failure" => Err(format!(
            "pr #{n} head {} FAILED its build — fix before merging, never after",
            &status.head_sha.chars().take(7).collect::<String>()
        )),
        "pending" => Err(format!(
            "pr #{n} build still running — call pr_status again until it settles"
        )),
        other => Err(format!(
            "pr #{n} has no settled check verdict ({other}) — no green signal, no merge"
        )),
    }
}

/// the current wall-clock time in milliseconds since the unix epoch.
/// wasm asks the browser (`js_sys::Date`); the native test build falls back
/// to `std::time`. no network request is involved either way.
pub fn now_epoch_ms() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as i64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// howard hinnant's civil-from-days algorithm: days since 1970-01-01 to a
/// proleptic-gregorian (year, month, day). pure so tests can pin it against
/// known timestamps instead of trusting whatever the runtime says.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

/// epoch milliseconds → `"2026-08-23T22:09:05Z"`.
pub fn format_timestamp_iso(epoch_ms: i64) -> String {
    let secs = epoch_ms.div_euclid(1_000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3_600,
        (sod % 3_600) / 60,
        sod % 60
    )
}

/// epoch milliseconds → `"Sunday, August 23, 2026 · 22:09 UTC"` for prose
/// replies. epoch day 0 was a thursday; the table starts there.
pub fn format_timestamp_readable(epoch_ms: i64) -> String {
    const WEEKDAYS: [&str; 7] = [
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
    ];
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let secs = epoch_ms.div_euclid(1_000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let weekday = WEEKDAYS[days.rem_euclid(7) as usize];
    format!(
        "{weekday}, {} {d}, {y} · {:02}:{:02} UTC",
        MONTHS[(mo - 1) as usize],
        sod / 3_600,
        (sod % 3_600) / 60
    )
}

/// the json tool schema handed to the model each turn.
pub fn definitions() -> serde_json::Value {
    serde_json::json!([
      {
        "type": "function",
        "function": {
          "name": "read_file",
          "description": "read a file from the working tree. returns numbered lines. reads the local copy, which includes edits made earlier this run.",
          "parameters": {
            "type": "object",
            "properties": {
              "path": { "type": "string", "description": "repo-relative path, e.g. src/agent/mod.rs" },
              "start_line": { "type": "integer", "description": "optional 1-indexed first line" },
              "end_line": { "type": "integer", "description": "optional inclusive last line" }
            },
            "required": ["path"]
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "write_file",
          "description": "create or overwrite a file in the working tree. the write is durable immediately; it survives the end of this run.",
          "parameters": {
            "type": "object",
            "properties": {
              "path": { "type": "string" },
              "content": { "type": "string" }
            },
            "required": ["path", "content"]
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "edit_file",
          "description": "replace an exact substring in a file. fails if the target appears zero times or more than once, so an ambiguous edit never silently hits the wrong place.",
          "parameters": {
            "type": "object",
            "properties": {
              "path": { "type": "string" },
              "target": { "type": "string", "description": "exact text to replace, including indentation" },
              "replacement": { "type": "string" }
            },
            "required": ["path", "target", "replacement"]
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "list_dir",
          "description": "list files in the working tree, optionally under a path prefix.",
          "parameters": {
            "type": "object",
            "properties": { "path": { "type": "string", "description": "optional prefix, omit for the whole tree" } }
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "git_status",
          "description": "list every file whose local content differs from the last synced github blob.",
          "parameters": { "type": "object", "properties": {} }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "git_commit",
          "description": "commit every modified file to github as ONE atomic commit on the connected branch.",
          "parameters": {
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"]
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "git_create_branch",
          "description": "create a new agent/* branch at the current head and switch this session to it. only names under agent/ are allowed; main is protected.",
          "parameters": {
            "type": "object",
            "properties": { "name": { "type": "string", "description": "branch name; must start with agent/, e.g. agent/fix-d10-guard" } },
            "required": ["name"]
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "git_checkout",
          "description": "switch this session to a different branch (must be an agent/ branch, or the protected base branch for read-only work). the working tree is re-synced from that branch's head — commit or accept loss of dirty files first.",
          "parameters": {
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "open_pr",
          "description": "open a pull request from the current agent/ branch into main, with title and body.",
          "parameters": {
            "type": "object",
            "properties": {
              "title": { "type": "string" },
              "body": { "type": "string", "description": "what changed and why; verification evidence" }
            },
            "required": ["title", "body"]
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "pr_status",
          "description": "read a pull request's mergeability and ci verdicts. call before merging; the build must be settled green.",
          "parameters": {
            "type": "object",
            "properties": { "number": { "type": "integer" } },
            "required": ["number"]
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "merge_pr",
          "description": "merge a pull request into main (squash). REFUSED unless github reports it mergeable AND its build checks are green — never merge red, never merge blind.",
          "parameters": {
            "type": "object",
            "properties": { "number": { "type": "integer" } },
            "required": ["number"]
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "diff_branches",
          "description": "list files changed between two refs (the parallel-diff view) — what a merge would carry.",
          "parameters": {
            "type": "object",
            "properties": {
              "base": { "type": "string" },
              "head": { "type": "string" }
            },
            "required": ["base", "head"]
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "sync_repo",
          "description": "pull the branch head from github into the working tree. this DISCARDS local edits to files that changed upstream, so commit first.",
          "parameters": { "type": "object", "properties": {} }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "now",
          "description": "the current date and time from the worker's own clock. no network call. returns iso-8601 (utc), a human-readable line, and the raw epoch milliseconds. use this whenever a task needs to know today's date or the current time.",
          "parameters": { "type": "object", "properties": {} }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "http_fetch",
          "description": "make an arbitrary http request and return the status and body as text (truncated). works against any cors-enabled endpoint (github api, wikipedia, open-meteo, most public apis). endpoints that omit cors headers cannot be called from a browser — use web_read for those.",
          "parameters": {
            "type": "object",
            "properties": {
              "url": { "type": "string" },
              "method": { "type": "string", "description": "default GET" },
              "headers": { "type": "object", "description": "optional string-to-string header map" },
              "body": { "type": "string", "description": "optional request body" }
            },
            "required": ["url"]
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "web_read",
          "description": "fetch an arbitrary public web page as readable text via the https://r.jina.ai reader proxy, which sends permissive cors headers. use this to read documentation, articles, or any page http_fetch cannot reach directly.",
          "parameters": {
            "type": "object",
            "properties": { "url": { "type": "string" } },
            "required": ["url"]
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "web_search",
          "description": "search the web via the duckduckgo instant-answer api. returns an abstract plus related topics. good for facts and lookups; follow up with web_read on a specific url for depth.",
          "parameters": {
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "check_deployment",
          "description": "check whether a commit actually built and deployed. THIS REPOSITORY COMPILES ON DEPLOY: a commit that does not compile takes the live app down and you will not find out any other way. call this after every git_commit that touched source. by default it waits for the build to finish and reports the failure reason.",
          "parameters": {
            "type": "object",
            "properties": {
              "sha": { "type": "string", "description": "commit to check; omit for the current branch head" },
              "wait": { "type": "boolean", "description": "wait for the build to settle instead of returning a pending snapshot (default true)" }
            }
          }
        }
      },
      {
        "type": "function",
        "function": {
          "name": "task_complete",
          "description": "declare the task finished. call this only when the work is done and committed.",
          "parameters": {
            "type": "object",
            "properties": { "summary": { "type": "string", "description": "what was accomplished" } },
            "required": ["summary"]
          }
        }
      }
    ])
}

fn arg<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn number_lines(content: &str, start: usize, end: usize) -> String {
    content
        .lines()
        .enumerate()
        .filter(|(i, _)| {
            let n = i + 1;
            n >= start && n <= end
        })
        .map(|(i, l)| format!("{}: {l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Workspace {
    pub async fn new(github: Github) -> Self {
        Self::with_vercel(github, None).await
    }

    pub async fn with_vercel(
        github: Github,
        vercel: Option<crate::agent::vercel::Vercel>,
    ) -> Self {
        let index = opfs::load_index().await;
        Self {
            github,
            vercel,
            index,
            synced_head: String::new(),
            completed: None,
        }
    }

    /// files whose bytes differ from what github last gave us.
    pub fn dirty(&self) -> Vec<String> {
        self.index
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// D10 reconciliation, shared by the sync_repo tool and by the worker's
    /// boot-time auto-reconcile. lists the branch, drops clean cached copies
    /// whose blob sha no longer matches (their next read re-fetches current
    /// content), never touches dirty files — they are uncommitted local work
    /// and dropping one is data loss — and records the head so this session's
    /// first commit is guarded rather than waved through. both callers go
    /// through THIS function so their behavior cannot drift.
    pub async fn reconcile_against_branch(&mut self) -> Result<ReconcileReport, String> {
        let items = self.github.list_tree().await?;
        let head = self.github.head_sha().await?;

        let mut refreshed: Vec<String> = Vec::new();
        for item in items.iter().filter(|i| i.kind == "blob") {
            let decision = reconcile_entry(
                self.index.get(&item.path).map(|e| e.dirty).unwrap_or(false),
                self.index
                    .get(&item.path)
                    .map(|e| e.base_sha.as_str())
                    .unwrap_or(""),
                item.sha.as_deref(),
            );
            if decision == Reconcile::Refresh && opfs::read(&item.path).await.is_ok() {
                opfs::delete(&item.path).await?;
                refreshed.push(item.path.clone());
            }
        }

        self.synced_head = head.clone();
        let uncommitted = self.dirty();

        Ok(ReconcileReport {
            head,
            files_on_branch: items.iter().filter(|i| i.kind == "blob").count(),
            refreshed,
            uncommitted,
        })
    }

    async fn mark_dirty(&mut self, path: &str, size: usize) -> Result<(), String> {
        let entry = self.index.entry(path.to_string()).or_insert(IndexEntry {
            base_sha: String::new(),
            size,
            dirty: true,
        });
        entry.size = size;
        entry.dirty = true;
        // persist the index alongside the file so a crash between the two
        // cannot leave a modified file that reports itself as clean.
        opfs::save_index(&self.index).await
    }

    /// read through: local copy first, falling back to github and caching it.
    /// this is what lets the agent read a file it has not touched without
    /// the whole repo being downloaded up front.
    async fn read_through(&mut self, path: &str) -> Result<String, String> {
        if let Ok(local) = opfs::read(path).await {
            return Ok(local);
        }
        let remote = self.github.read_file(path).await?;
        opfs::write(path, &remote).await?;
        self.index.insert(
            path.to_string(),
            IndexEntry {
                base_sha: String::new(),
                size: remote.len(),
                dirty: false,
            },
        );
        let _ = opfs::save_index(&self.index).await;
        Ok(remote)
    }

    pub async fn dispatch(&mut self, name: &str, args_json: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(args_json).unwrap_or(serde_json::Value::Object(Default::default()));

        match name {
            "read_file" => {
                let path = arg(&args, "path").ok_or("read_file requires 'path'")?;
                let content = self.read_through(path).await?;
                let total = content.lines().count();
                let start = args
                    .get("start_line")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1)
                    .max(1) as usize;
                let end = args
                    .get("end_line")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(total);
                Ok(serde_json::json!({
                    "path": path,
                    "total_lines": total,
                    "content": number_lines(&content, start, end),
                })
                .to_string())
            }

            "write_file" => {
                let path = arg(&args, "path").ok_or("write_file requires 'path'")?;
                let content = arg(&args, "content").ok_or("write_file requires 'content'")?;
                opfs::write(path, content).await?;
                self.mark_dirty(path, content.len()).await?;
                Ok(serde_json::json!({
                    "success": true,
                    "path": path,
                    "bytes": content.len(),
                    "note": "written to the working tree and durable. call git_commit to publish."
                })
                .to_string())
            }

            "edit_file" => {
                let path = arg(&args, "path").ok_or("edit_file requires 'path'")?;
                let target = arg(&args, "target").ok_or("edit_file requires 'target'")?;
                let replacement =
                    arg(&args, "replacement").ok_or("edit_file requires 'replacement'")?;

                let content = self.read_through(path).await?;
                let hits = content.matches(target).count();
                // refusing on 0 and on >1 is what stops a "successful" edit
                // from landing somewhere the model never looked at.
                if hits == 0 {
                    return Err(format!(
                        "target text not found in {path}. read the file first and copy the exact text including indentation."
                    ));
                }
                if hits > 1 {
                    return Err(format!(
                        "target text appears {hits} times in {path}; include more surrounding context to make it unique."
                    ));
                }
                let updated = content.replacen(target, replacement, 1);
                opfs::write(path, &updated).await?;
                self.mark_dirty(path, updated.len()).await?;
                Ok(serde_json::json!({
                    "success": true,
                    "path": path,
                    "note": "edit applied to the working tree."
                })
                .to_string())
            }

            "list_dir" => {
                let prefix = arg(&args, "path").unwrap_or("");
                let items = self.github.list_tree().await?;
                let listed: Vec<_> = items
                    .iter()
                    .filter(|i| prefix.is_empty() || i.path.starts_with(prefix))
                    .map(|i| {
                        serde_json::json!({
                            "path": i.path,
                            "type": if i.kind == "tree" { "directory" } else { "file" },
                            "size": i.size,
                            "modified_locally": self.index.get(&i.path).map(|e| e.dirty).unwrap_or(false),
                        })
                    })
                    .collect();
                Ok(serde_json::json!({ "entries": listed }).to_string())
            }

            "git_status" => {
                let dirty = self.dirty();
                Ok(serde_json::json!({
                    "branch": self.github.branch,
                    "repo": self.github.repo,
                    "modified": dirty,
                    "clean": dirty.is_empty(),
                })
                .to_string())
            }

            "git_commit" => {
                let message = arg(&args, "message").unwrap_or("update vanish harness");
                let dirty = self.dirty();
                if dirty.is_empty() {
                    return Err(
                        "nothing to commit — no file in the working tree differs from github."
                            .to_string(),
                    );
                }

                // branch policy: main is promoted, never pushed blind. a
                // direct commit to it is refused with the escape hatch named
                // (D9 applies to refusals too: always say the way out).
                commit_allowed_on(&self.github.branch)?;

                // D10: a commit built on unreconciled local files silently
                // reverts whatever upstream landed. three incidents and
                // counting — one reverted three commits while looking like a
                // clean diff. if the branch moved since this session last
                // looked, refuse and say exactly what to do about it. the
                // commit object itself would still land (github's ref update
                // is what fails), so refusing EARLY is also cheaper.
                let live_head = self.github.head_sha().await?;
                let expected = self.synced_head.clone();
                if !expected.is_empty() && live_head != expected {
                    // record the new head: the NEXT git_commit attempt is
                    // allowed through, because this error has by then been
                    // surfaced and the model has had its chance to reconcile.
                    self.synced_head = live_head.clone();
                    let _ = opfs::save_index(&self.index).await;
                    return Err(format!(
                        "REFUSED: branch head moved since this session last synced \
                         ({live_head} != {expected}), so these local files may be stale and \
                         committing them would revert upstream work. re-read every \
                         file in the changeset against github (read_file serves the \
                         local copy; cross-check raw.githubusercontent for anything \
                         surprising), re-apply your edit on top of the CURRENT \
                         upstream text, then call git_commit again."
                    ));
                }

                let mut changes = Vec::with_capacity(dirty.len());
                for path in &dirty {
                    let content = opfs::read(path).await?;
                    changes.push(FileChange {
                        path: path.clone(),
                        content: Some(content),
                    });
                }

                let (sha, short) = self.github.commit(message, &changes).await?;

                // only clear dirty flags once github has confirmed the commit.
                for path in &dirty {
                    if let Some(e) = self.index.get_mut(path) {
                        e.dirty = false;
                    }
                }
                // and record where the tree now stands: this session's next
                // commit is legitimate precisely because it builds on THIS head.
                self.synced_head = sha.clone();
                opfs::save_index(&self.index).await?;

                Ok(serde_json::json!({
                    "success": true,
                    "sha": sha,
                    "short_sha": short,
                    "files": dirty.len(),
                    "message": message,
                })
                .to_string())
            }

            "git_create_branch" => {
                let name = arg(&args, "name").ok_or("git_create_branch requires 'name'")?;
                if !Github::is_agent_ref(name) {
                    return Err(format!(
                        "REFUSED: '{name}' is not an agent/ branch — only refs under agent/ may be created."
                    ));
                }
                // base the new branch on THIS session's verified head: it is
                // the newest sha this tree provably matches (D10 discipline),
                // so the branch starts from content we can vouch for.
                let at = if self.synced_head.is_empty() {
                    self.github.head_sha().await?
                } else {
                    self.synced_head.clone()
                };
                self.github.create_ref(name, &at).await?;
                self.github.branch = name.to_string();
                self.synced_head = at.clone();
                Ok(serde_json::json!({
                    "success": true,
                    "branch": name,
                    "created_at": &at[..7.min(at.len())],
                    "note": "session switched to the new branch; commits now land there.",
                })
                .to_string())
            }

            "git_checkout" => {
                let name = arg(&args, "name").ok_or("git_checkout requires 'name'")?;
                let dirty = self.dirty();
                if !dirty.is_empty() {
                    return Err(format!(
                        "{} uncommitted file(s) ({:?}) would be lost by switching branches — commit first.",
                        dirty.len(),
                        dirty
                    ));
                }
                self.github.branch = name.to_string();
                // re-verify against the new ref and reconcile the cache, so
                // reads after a switch serve THAT branch's content.
                let report = self.reconcile_against_branch().await?;
                Ok(serde_json::json!({
                    "success": true,
                    "branch": name,
                    "head": &report.head[..7.min(report.head.len())],
                    "files_on_branch": report.files_on_branch,
                })
                .to_string())
            }

            "open_pr" => {
                let title = arg(&args, "title").ok_or("open_pr requires 'title'")?;
                let body = arg(&args, "body").unwrap_or("");
                let head = self.github.branch.clone();
                // prs come FROM agent/* INTO main. opening one from main is
                // meaningless; from anything else it is unreviewed surface.
                if !Github::is_agent_ref(&head) {
                    return Err(format!(
                        "REFUSED: prs are opened from an agent/ branch, not '{head}' — create one with git_create_branch first."
                    ));
                }
                let (number, url) =
                    self.github.create_pr(&head, PROTECTED_BRANCH, title, body).await?;
                Ok(serde_json::json!({
                    "success": true,
                    "number": number,
                    "url": url,
                    "head": head,
                    "base": PROTECTED_BRANCH,
                    "note": "pr opened. call pr_status until checks settle green, then merge_pr.",
                })
                .to_string())
            }

            "pr_status" => {
                let number = args
                    .get("number")
                    .and_then(|v| v.as_u64())
                    .ok_or("pr_status requires 'number'")?;
                let s = self.github.pr_status(number).await?;
                let gate = pr_gate(&s);
                Ok(serde_json::json!({
                    "number": s.number,
                    "head_sha": &s.head_sha[..7.min(s.head_sha.len())],
                    "mergeable": s.mergeable,
                    "deploy_verdict": s.deploy_verdict,
                    "gate": match &gate { Ok(()) => "OPEN — merge permitted".to_string(), Err(e) => e },
                })
                .to_string())
            }

            "merge_pr" => {
                let number = args
                    .get("number")
                    .and_then(|v| v.as_u64())
                    .ok_or("merge_pr requires 'number'")?;
                // the gate is checked HERE, immediately before the merge call:
                // a verdict read earlier in the run says nothing about right now.
                let s = self.github.pr_status(number).await?;
                pr_gate(&s)?;
                let merged = self.github.merge_pr(number).await?;
                Ok(serde_json::json!({
                    "success": true,
                    "number": number,
                    "github_response": merged.chars().take(300).collect::<String>(),
                    "note": "merged. main has been PROMOTED through a green-gated pr, not pushed blind.",
                })
                .to_string())
            }

            "diff_branches" => {
                let base = arg(&args, "base").ok_or("diff_branches requires 'base'")?;
                let head = arg(&args, "head").ok_or("diff_branches requires 'head'")?;
                let files = self.github.compare(base, head).await?;
                Ok(serde_json::json!({ "base": base, "head": head, "files": files }).to_string())
            }

            "sync_repo" => {
                // D10: a listing refresh is not reconciliation. the old
                // version left opfs caches in place, so read_through kept
                // serving pre-sync bytes — the mechanism behind incidents
                // #1–#3. the real work lives in reconcile_against_branch,
                // which the worker's boot-time auto-reconcile also calls, so
                // the two paths cannot drift.
                let report = self.reconcile_against_branch().await?;
                let refreshed = &report.refreshed;

                Ok(serde_json::json!({
                    "success": true,
                    "files_on_branch": report.files_on_branch,
                    "branch": self.github.branch,
                    "synced_head": report.head,
                    "cache_refreshed": refreshed,
                    "uncommitted_local_files": report.uncommitted,
                    "note": if refreshed.is_empty() {
                        "tree reconciled: every cached file matches the branch."
                    } else {
                        "tree reconciled: stale cached files were dropped and will re-fetch from github on next read. dirty files were never touched."
                    },
                })
                .to_string())
            }

            "now" => {
                let ms = now_epoch_ms();
                Ok(serde_json::json!({
                    "iso": format_timestamp_iso(ms),
                    "readable": format_timestamp_readable(ms),
                    "epoch_ms": ms,
                    "source": "browser clock (js_sys::Date), no network",
                })
                .to_string())
            }

            "http_fetch" => {
                let url = arg(&args, "url").ok_or("http_fetch requires 'url'")?;
                let method = arg(&args, "method").unwrap_or("GET").to_uppercase();
                let mut headers: Vec<(String, String)> = Vec::new();
                if let Some(h) = args.get("headers").and_then(|v| v.as_object()) {
                    for (k, v) in h {
                        if let Some(s) = v.as_str() {
                            headers.push((k.clone(), s.to_string()));
                        }
                    }
                }
                let header_refs: Vec<(&str, String)> =
                    headers.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                let body = arg(&args, "body").map(|s| s.to_string());

                // the underlying client is the same one openrouter and github
                // already use; there is no proxy and no server in between.
                let resp = http::request(
                    &method,
                    url,
                    &header_refs,
                    body.as_deref(),
                )
                .await?;

                // capture status before the body is moved into the
                // truncation branch below.
                let status = resp.status;

                let truncated = if resp.body.len() > 20_000 {
                    let head: String = resp.body.chars().take(20_000).collect();
                    // char-boundary-safe truncation: byte length of what was kept
                    let kept = head.len();
                    format!(
                        "{head}\n\n[truncated: {kept} of {} bytes shown]",
                        resp.body.len()
                    )
                } else {
                    resp.body
                };

                Ok(serde_json::json!({
                    "status": status,
                    "ok": (200..300).contains(&status),
                    "body": truncated,
                })
                .to_string())
            }

            "web_read" => {
                let url = arg(&args, "url").ok_or("web_read requires 'url'")?;
                if !url.starts_with("https://") {
                    return Err("web_read requires an https:// url".to_string());
                }
                let reader_url = format!("https://r.jina.ai/{url}");
                let resp = http::request("GET", &reader_url, &[], None).await?;
                if !resp.ok() {
                    return Err(format!(
                        "reader returned http {} for {url}: {}",
                        resp.status,
                        resp.body.chars().take(300).collect::<String>()
                    ));
                }
                Ok(serde_json::json!({
                    "url": url,
                    "content": resp.body.chars().take(30_000).collect::<String>(),
                })
                .to_string())
            }

            "web_search" => {
                let query = arg(&args, "query").ok_or("web_search requires 'query'")?;
                let encoded = js_sys::encode_uri_component(query)
                    .as_string()
                    .unwrap_or_else(|| query.to_string());
                let url = format!(
                    "https://api.duckduckgo.com/?q={encoded}&format=json&no_html=1&no_redirect=1"
                );
                let resp = http::request("GET", &url, &[], None).await?;
                if !resp.ok() {
                    return Err(format!(
                        "search returned http {}: {}",
                        resp.status,
                        resp.body.chars().take(300).collect::<String>()
                    ));
                }
                let parsed: serde_json::Value = serde_json::from_str(&resp.body)
                    .map_err(|e| format!("could not parse search response: {e}"))?;
                Ok(serde_json::json!({
                    "query": query,
                    "abstract": parsed.get("AbstractText"),
                    "abstract_url": parsed.get("AbstractURL"),
                    "answer": parsed.get("Answer"),
                    "definition": parsed.get("Definition"),
                    "related": parsed.get("RelatedTopics"),
                })
                .to_string())
            }

            "check_deployment" => {
                let sha = match arg(&args, "sha") {
                    Some(s) if !s.trim().is_empty() => s.to_string(),
                    _ => self.github.head_sha().await?,
                };
                let wait = args
                    .get("wait")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);

                let mut state = self.github.deployment_state(&sha).await?;

                if wait {
                    // builds take roughly a minute. poll rather than guess.
                    // there is no run deadline, so waiting is free — and a
                    // wrong "success" is far more expensive than a slow one.
                    let mut waited = 0;
                    while !state.settled() && waited < 300 {
                        crate::agent::http::sleep_ms(10_000).await;
                        waited += 10;
                        state = self.github.deployment_state(&sha).await?;
                    }
                }

                let short: String = sha.chars().take(7).collect();

                // a verdict without a cause is not actionable. when a vercel
                // token is configured, pull the real build output so a failed
                // commit can be repaired in the same run that broke it.
                let mut build_log = serde_json::Value::Null;
                if state.verdict == "failure" {
                    build_log = match &self.vercel {
                        None => serde_json::json!(
                            "no vercel token configured, so the compiler output is unavailable — \
                             only the pass/fail verdict. add one in settings to see why builds fail."
                        ),
                        Some(v) => match v.deployment_for_commit(&sha).await {
                            Err(e) => serde_json::json!(format!("could not reach vercel: {e}")),
                            Ok(None) => serde_json::json!(
                                "vercel has no deployment recorded for this commit yet."
                            ),
                            Ok(Some(dep)) => match v.build_logs(&dep.id).await {
                                Err(e) => serde_json::json!(format!("could not read build logs: {e}")),
                                Ok(lines) => {
                                    serde_json::json!(crate::agent::vercel::extract_errors(&lines))
                                }
                            },
                        },
                    };
                }

                let guidance = match state.verdict.as_str() {
                    "failure" => "the build FAILED. the live app is now serving the last good build, not your commit. open the check url for the compiler output, fix the cause, and commit the fix.",
                    "success" => "the build succeeded and this commit is live.",
                    "pending" => "still building. call check_deployment again before you finish.",
                    _ => "no build checks reported for this commit yet. if this repository deploys on push, wait a few seconds and check again.",
                };

                Ok(serde_json::json!({
                    "sha": short,
                    "verdict": state.verdict,
                    "checks": state.checks,
                    "build_log": build_log,
                    "guidance": guidance,
                })
                .to_string())
            }

            "task_complete" => {
                let summary = arg(&args, "summary").unwrap_or("task complete");
                self.completed = Some(summary.to_string());
                Ok(serde_json::json!({ "acknowledged": true }).to_string())
            }

            other => Err(format!("unknown tool '{other}'")),
        }
    }
}
