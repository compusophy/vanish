//! github, spoken directly from the browser.
//!
//! commits go through the git data api (blobs -> tree -> commit -> ref)
//! rather than the contents api, because that is the only way to land many
//! files as ONE commit. the previous harness committed file-by-file, so an
//! interrupted run could leave the repository in a state that never existed
//! as a coherent revision — and every partial write triggered its own build.

use serde::Deserialize;

use crate::agent::http::{request, HttpResponse};

const API: &str = "https://api.github.com";

pub struct Github {
    token: String,
    /// "owner/name"
    pub repo: String,
    pub branch: String,
}

#[derive(Debug, Deserialize)]
struct RefObject {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct RefResponse {
    object: RefObject,
}

#[derive(Debug, Deserialize)]
struct CommitTree {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct CommitResponse {
    sha: String,
    tree: CommitTree,
}

#[derive(Debug, Deserialize)]
struct ShaOnly {
    sha: String,
}

#[derive(Debug, Deserialize)]
pub struct TreeItem {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub size: Option<usize>,
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TreeResponse {
    tree: Vec<TreeItem>,
    #[serde(default)]
    truncated: bool,
}

fn field(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckSummary {
    pub name: String,
    pub state: String,
    pub detail: String,
    pub url: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeployState {
    /// "success" | "failure" | "pending" | "none"
    pub verdict: String,
    pub checks: Vec<CheckSummary>,
}

impl DeployState {
    /// any failure wins; otherwise anything still running means pending.
    /// reporting "success" while a build is mid-flight is worse than
    /// reporting pending, because the agent would move on from broken code.
    fn from(checks: Vec<CheckSummary>) -> Self {
        let failed = |s: &str| matches!(s, "failure" | "error" | "timed_out" | "cancelled");
        let running = |s: &str| matches!(s, "pending" | "queued" | "in_progress" | "waiting");

        let verdict = if checks.is_empty() {
            "none"
        } else if checks.iter().any(|c| failed(&c.state)) {
            "failure"
        } else if checks.iter().any(|c| running(&c.state)) {
            "pending"
        } else {
            "success"
        };

        Self {
            verdict: verdict.to_string(),
            checks,
        }
    }

    pub fn settled(&self) -> bool {
        self.verdict == "success" || self.verdict == "failure"
    }
}

/// one file destined for a commit. `content: None` means delete.
pub struct FileChange {
    pub path: String,
    pub content: Option<String>,
}

impl Github {
    pub fn new(token: impl Into<String>, repo: impl Into<String>, branch: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            repo: repo.into(),
            branch: branch.into(),
        }
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Authorization", format!("Bearer {}", self.token)),
            ("Accept", "application/vnd.github+json".to_string()),
            ("X-GitHub-Api-Version", "2022-11-28".to_string()),
            ("Content-Type", "application/json".to_string()),
        ]
    }

    /// github reports failures in a json body; surfacing it verbatim is the
    /// difference between "commit failed" and "resource not accessible by
    /// personal access token", which tells the user exactly what to fix.
    fn check(resp: HttpResponse, what: &str) -> Result<String, String> {
        if resp.ok() {
            return Ok(resp.body);
        }
        let detail = serde_json::from_str::<serde_json::Value>(&resp.body)
            .ok()
            .and_then(|v| {
                v.get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| resp.body.chars().take(300).collect());
        Err(format!("{what} failed (http {}): {detail}", resp.status))
    }

    async fn get(&self, path: &str, what: &str) -> Result<String, String> {
        let resp = request("GET", &format!("{API}{path}"), &self.headers(), None).await?;
        Self::check(resp, what)
    }

    async fn post(&self, path: &str, body: &str, what: &str) -> Result<String, String> {
        let resp = request("POST", &format!("{API}{path}"), &self.headers(), Some(body)).await?;
        Self::check(resp, what)
    }

    /// verify the token before the agent starts editing, so an auth problem
    /// surfaces as a clear message instead of as a failed commit after the
    /// model has already done twenty steps of work.
    pub async fn verify(&self) -> Result<String, String> {
        let body = self
            .get(&format!("/repos/{}", self.repo), "repository lookup")
            .await?;
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("bad repo response: {e}"))?;
        let can_push = v
            .get("permissions")
            .and_then(|p| p.get("push"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        if !can_push {
            return Err(format!(
                "this token can read {} but cannot push to it. the agent could edit files and then fail every commit.",
                self.repo
            ));
        }
        Ok(self.repo.clone())
    }

    /// full recursive listing of the branch.
    pub async fn list_tree(&self) -> Result<Vec<TreeItem>, String> {
        let body = self
            .get(
                &format!("/repos/{}/git/trees/{}?recursive=1", self.repo, self.branch),
                "listing repository tree",
            )
            .await?;
        let parsed: TreeResponse =
            serde_json::from_str(&body).map_err(|e| format!("bad tree response: {e}"))?;
        if parsed.truncated {
            // silently returning a partial tree would make the agent believe
            // files simply do not exist.
            return Err(
                "repository tree is too large for a single listing (github truncated it)"
                    .to_string(),
            );
        }
        Ok(parsed.tree)
    }

    /// raw file text at the branch head.
    pub async fn read_file(&self, path: &str) -> Result<String, String> {
        let mut headers = self.headers();
        // ask for the raw bytes so there is no base64 round trip to botch.
        headers.retain(|(k, _)| *k != "Accept");
        headers.push(("Accept", "application/vnd.github.raw".to_string()));

        let url = format!(
            "{API}/repos/{}/contents/{}?ref={}",
            self.repo, path, self.branch
        );
        let resp = request("GET", &url, &headers, None).await?;
        Self::check(resp, &format!("reading {path}"))
    }

    /// the head commit sha of the branch.
    pub async fn head_sha(&self) -> Result<String, String> {
        let body = self
            .get(
                &format!("/repos/{}/commits/{}", self.repo, self.branch),
                "reading branch head",
            )
            .await?;
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("bad commit response: {e}"))?;
        v.get("sha")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "commit response had no sha".to_string())
    }

    /// build/deploy outcome for a commit.
    ///
    /// vercel reports build results back to github as commit statuses and
    /// check runs, so the agent can read whether its own commit compiled
    /// using the token it already has — no vercel token, and no api key for
    /// a second vendor. without this the agent commits broken code, is told
    /// nothing, and believes it succeeded.
    pub async fn deployment_state(&self, sha: &str) -> Result<DeployState, String> {
        let mut checks: Vec<CheckSummary> = Vec::new();

        // combined status api (what vercel has historically posted)
        if let Ok(body) = self
            .get(
                &format!("/repos/{}/commits/{}/status", self.repo, sha),
                "reading commit status",
            )
            .await
        {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                for s in v.get("statuses").and_then(|s| s.as_array()).into_iter().flatten() {
                    checks.push(CheckSummary {
                        name: field(s, "context"),
                        state: field(s, "state"),
                        detail: field(s, "description"),
                        url: field(s, "target_url"),
                    });
                }
            }
        }

        // check-runs api (the newer surface; vercel uses this too)
        if let Ok(body) = self
            .get(
                &format!("/repos/{}/commits/{}/check-runs", self.repo, sha),
                "reading check runs",
            )
            .await
        {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                for c in v
                    .get("check_runs")
                    .and_then(|s| s.as_array())
                    .into_iter()
                    .flatten()
                {
                    let status = field(c, "status");
                    let conclusion = field(c, "conclusion");
                    checks.push(CheckSummary {
                        name: field(c, "name"),
                        // an unfinished run has no conclusion yet
                        state: if conclusion.is_empty() {
                            status
                        } else {
                            conclusion
                        },
                        detail: c
                            .get("output")
                            .map(|o| field(o, "title"))
                            .unwrap_or_default(),
                        url: field(c, "details_url"),
                    });
                }
            }
        }

        Ok(DeployState::from(checks))
    }

    /// land every change as a single atomic commit and move the branch.
    pub async fn commit(
        &self,
        message: &str,
        changes: &[FileChange],
    ) -> Result<(String, String), String> {
        if changes.is_empty() {
            return Err("nothing to commit".to_string());
        }

        // 1. current head, and the tree it points at
        let head_body = self
            .get(
                &format!("/repos/{}/git/ref/heads/{}", self.repo, self.branch),
                &format!("reading branch {}", self.branch),
            )
            .await?;
        let head: RefResponse =
            serde_json::from_str(&head_body).map_err(|e| format!("bad ref response: {e}"))?;

        let commit_body = self
            .get(
                &format!("/repos/{}/git/commits/{}", self.repo, head.object.sha),
                "reading head commit",
            )
            .await?;
        let base: CommitResponse =
            serde_json::from_str(&commit_body).map_err(|e| format!("bad commit response: {e}"))?;

        // 2. one blob per changed file
        let mut tree_entries = Vec::with_capacity(changes.len());
        for change in changes {
            match &change.content {
                Some(content) => {
                    let payload = serde_json::json!({
                        "content": content,
                        "encoding": "utf-8",
                    });
                    let blob_body = self
                        .post(
                            &format!("/repos/{}/git/blobs", self.repo),
                            &payload.to_string(),
                            &format!("uploading {}", change.path),
                        )
                        .await?;
                    let blob: ShaOnly = serde_json::from_str(&blob_body)
                        .map_err(|e| format!("bad blob response: {e}"))?;
                    tree_entries.push(serde_json::json!({
                        "path": change.path,
                        "mode": "100644",
                        "type": "blob",
                        "sha": blob.sha,
                    }));
                }
                None => {
                    // a null sha is how the git data api spells "delete".
                    tree_entries.push(serde_json::json!({
                        "path": change.path,
                        "mode": "100644",
                        "type": "blob",
                        "sha": serde_json::Value::Null,
                    }));
                }
            }
        }

        // 3. a tree layered over the current one
        let tree_payload = serde_json::json!({
            "base_tree": base.tree.sha,
            "tree": tree_entries,
        });
        let tree_body = self
            .post(
                &format!("/repos/{}/git/trees", self.repo),
                &tree_payload.to_string(),
                "building commit tree",
            )
            .await?;
        let tree: ShaOnly =
            serde_json::from_str(&tree_body).map_err(|e| format!("bad tree response: {e}"))?;

        // 4. the commit object
        let commit_payload = serde_json::json!({
            "message": message,
            "tree": tree.sha,
            "parents": [base.sha],
        });
        let new_commit_body = self
            .post(
                &format!("/repos/{}/git/commits", self.repo),
                &commit_payload.to_string(),
                "creating commit",
            )
            .await?;
        let new_commit: ShaOnly = serde_json::from_str(&new_commit_body)
            .map_err(|e| format!("bad commit response: {e}"))?;

        // 5. move the branch. no force: if someone else pushed since step 1,
        //    this fails loudly instead of silently discarding their commit.
        let ref_payload = serde_json::json!({ "sha": new_commit.sha, "force": false });
        let resp = request(
            "PATCH",
            &format!("{API}/repos/{}/git/refs/heads/{}", self.repo, self.branch),
            &self.headers(),
            Some(&ref_payload.to_string()),
        )
        .await?;
        Self::check(resp, "updating branch").map_err(|e| {
            format!("{e} — the commit object was created but the branch was not moved; the branch may have advanced underneath this run.")
        })?;

        let short = new_commit.sha.chars().take(7).collect::<String>();
        Ok((new_commit.sha, short))
    }
}
