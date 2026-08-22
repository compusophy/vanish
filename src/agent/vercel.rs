//! vercel, for build logs.
//!
//! github tells the agent *that* a build failed; only vercel can say *why*.
//! the check run github receives carries no compiler output — its entire
//! detail is "Deployment has failed — run this Vercel CLI command" — so an
//! agent limited to github has a verdict and no cause, and cannot repair its
//! own broken commit. that is the gap this closes.
//!
//! the vercel api sends `access-control-allow-origin: *` and accepts an
//! `authorization` header, so the browser can call it directly, exactly like
//! openrouter and github. no backend, no proxy.

use crate::agent::http::request;

const API: &str = "https://api.vercel.com";

pub struct Vercel {
    token: String,
    /// team-scoped projects need this; personal accounts can leave it blank.
    team_id: String,
    /// vercel project name, used to scope lookups. a token scoped to a single
    /// project can only see that project, so every call must name it.
    project: String,
}

pub struct Deployment {
    pub id: String,
    pub state: String,
    pub url: String,
}

impl Vercel {
    pub fn new(
        token: impl Into<String>,
        team_id: impl Into<String>,
        project: impl Into<String>,
    ) -> Self {
        Self {
            token: token.into(),
            team_id: team_id.into(),
            project: project.into(),
        }
    }

    /// vercel projects created from a repo take the repository's name, so
    /// "owner/name" -> "name" is the right default and saves a settings field.
    pub fn project_from_repo(repo: &str) -> String {
        repo.rsplit('/').next().unwrap_or(repo).trim().to_string()
    }

    pub fn configured(&self) -> bool {
        !self.token.trim().is_empty()
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        vec![("Authorization", format!("Bearer {}", self.token))]
    }

    fn team_param(&self) -> String {
        if self.team_id.trim().is_empty() {
            String::new()
        } else {
            format!("&teamId={}", self.team_id.trim())
        }
    }

    /// verify the token works, so a bad one is reported at save time rather
    /// than discovered during an incident.
    ///
    /// this deliberately exercises the deployments endpoint — the same call
    /// the build-log path depends on — rather than something like `/v2/user`.
    /// a token scoped to a single project has no user scope and answers 404
    /// there, so checking it would fail a token that is in fact perfectly
    /// good for the one job it has.
    pub async fn verify(&self) -> Result<String, String> {
        let url = format!(
            "{API}/v6/deployments?limit=1&app={}{}",
            self.project,
            self.team_param()
        );
        let resp = request("GET", &url, &self.headers(), None).await?;

        if resp.status == 401 || resp.status == 403 {
            return Err(format!(
                "vercel rejected this token for project '{}'. check it is scoped to that project (or to the whole team).",
                self.project
            ));
        }
        if resp.status == 404 {
            return Err(format!(
                "vercel has no project named '{}' visible to this token. the project name is taken from the repository name; rename the repo field or the vercel project so they match.",
                self.project
            ));
        }
        if !resp.ok() {
            return Err(format!("vercel returned http {}", resp.status));
        }

        let v: serde_json::Value =
            serde_json::from_str(&resp.body).map_err(|e| format!("bad vercel response: {e}"))?;
        let seen = v
            .get("deployments")
            .and_then(|d| d.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        Ok(format!(
            "vercel ok (build logs available for '{}'{})",
            self.project,
            if seen == 0 {
                "; no deployments yet"
            } else {
                ""
            }
        ))
    }

    /// the deployment produced by a given commit.
    pub async fn deployment_for_commit(&self, sha: &str) -> Result<Option<Deployment>, String> {
        let url = format!(
            "{API}/v6/deployments?limit=20&meta-githubCommitSha={sha}{}",
            self.team_param()
        );
        let resp = request("GET", &url, &self.headers(), None).await?;
        if !resp.ok() {
            return Err(format!(
                "listing deployments failed (http {}): {}",
                resp.status,
                resp.body.chars().take(200).collect::<String>()
            ));
        }
        let v: serde_json::Value =
            serde_json::from_str(&resp.body).map_err(|e| format!("bad deployments response: {e}"))?;

        let Some(list) = v.get("deployments").and_then(|d| d.as_array()) else {
            return Ok(None);
        };
        let Some(first) = list.first() else {
            return Ok(None);
        };

        Ok(Some(Deployment {
            // the id field has been `uid` for a long time; `id` is accepted
            // too, so read whichever is present rather than assuming.
            id: first
                .get("uid")
                .or_else(|| first.get("id"))
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
            state: first
                .get("state")
                .or_else(|| first.get("readyState"))
                .and_then(|s| s.as_str())
                .unwrap_or("UNKNOWN")
                .to_string(),
            url: first
                .get("url")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
        }))
    }

    /// raw build output for a deployment.
    pub async fn build_logs(&self, deployment_id: &str) -> Result<Vec<String>, String> {
        let url = format!(
            "{API}/v3/deployments/{deployment_id}/events?builds=1&limit=1000{}",
            self.team_param()
        );
        let resp = request("GET", &url, &self.headers(), None).await?;
        if !resp.ok() {
            return Err(format!(
                "fetching build logs failed (http {}): {}",
                resp.status,
                resp.body.chars().take(200).collect::<String>()
            ));
        }
        Ok(parse_events(&resp.body))
    }
}

/// the events endpoint has returned both a json array and newline-delimited
/// json objects depending on version and content type. accept either rather
/// than breaking the moment the shape shifts.
fn parse_events(body: &str) -> Vec<String> {
    let mut lines = Vec::new();

    if let Ok(serde_json::Value::Array(items)) = serde_json::from_str(body) {
        for item in items {
            if let Some(t) = event_text(&item) {
                lines.push(t);
            }
        }
        return lines;
    }

    for raw in body.lines() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
            if let Some(t) = event_text(&v) {
                lines.push(t);
            }
        }
    }
    lines
}

fn event_text(v: &serde_json::Value) -> Option<String> {
    let text = v
        .get("payload")
        .and_then(|p| p.get("text"))
        .or_else(|| v.get("text"))
        .and_then(|t| t.as_str())?;
    let trimmed = text.trim_end_matches(['\n', '\r']);
    if trimmed.trim().is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// pull the part of a build log that explains a failure.
///
/// a full rust build log is thousands of lines of "Compiling foo v1.2.3",
/// which would bury the three lines that matter and burn the context window.
/// this keeps diagnostics and the lines beneath them, which is where rustc
/// puts the file, line, and offending source.
pub fn extract_errors(lines: &[String]) -> String {
    const AFTER: usize = 12;
    let interesting = |l: &str| {
        let t = l.trim_start();
        t.starts_with("error")
            || t.starts_with("warning: unused")
            || t.contains("-->")
            || t.starts_with("Caused by")
            || t.contains("exited with")
            || t.contains("Command \"")
            || t.contains("cannot find")
            || t.contains("failed to")
    };

    let mut keep = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        if interesting(line) {
            for k in keep.iter_mut().take((i + AFTER + 1).min(lines.len())).skip(i) {
                *k = true;
            }
        }
    }

    let selected: Vec<&str> = lines
        .iter()
        .zip(keep.iter())
        .filter(|(_, k)| **k)
        .map(|(l, _)| l.as_str())
        .collect();

    // nothing matched: the failure was not a compiler diagnostic (a network
    // blip, an oom). the tail is the best available evidence.
    let chosen = if selected.is_empty() {
        lines
            .iter()
            .rev()
            .take(40)
            .rev()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
    } else {
        selected
    };

    let joined = chosen.join("\n");
    const MAX: usize = 8_000;
    if joined.chars().count() <= MAX {
        joined
    } else {
        let head: String = joined.chars().take(MAX).collect();
        format!("{head}\n… log truncated")
    }
}
