//! the tool surface, and the workspace it acts on.
//!
//! every mutating tool writes through to opfs immediately. there is no
//! staging map held in memory waiting to be flushed, because that design is
//! precisely what lost work every time a run ended early. "uncommitted" here
//! means bytes on disk that differ from the last synced github blob — a fact
//! that outlives the run, the tab, and the browser.

use crate::agent::github::{FileChange, Github};
use crate::platform::opfs::{self, Index, IndexEntry};

pub struct Workspace {
    pub github: Github,
    pub index: Index,
    /// set by `task_complete`; the loop reads it to know the model is done.
    pub completed: Option<String>,
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

            "task_complete" => {
                let summary = arg(&args, "summary").unwrap_or("task complete");
                self.completed = Some(summary.to_string());
                Ok(serde_json::json!({ "acknowledged": true }).to_string())
            }

            other => Err(format!("unknown tool '{other}'")),
        }
    }
}
