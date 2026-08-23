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
    pub index: Index,
    /// set by `task_complete`; the loop reads it to know the model is done.
    pub completed: Option<String>,
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
    let doe = (z - era * 146_097) as i64; // [0, 146096]
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
        let index = opfs::load_index().await;
        Self {
            github,
            index,
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

            "sync_repo" => {
                let items = self.github.list_tree().await?;
                let blobs: Vec<_> = items.iter().filter(|i| i.kind == "blob").collect();
                let dirty = self.dirty();
                Ok(serde_json::json!({
                    "success": true,
                    "files_on_branch": blobs.len(),
                    "branch": self.github.branch,
                    "uncommitted_local_files": dirty,
                    "note": "tree listing refreshed. files are fetched lazily on first read.",
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
