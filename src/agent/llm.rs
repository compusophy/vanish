//! openrouter, streamed straight from the worker.
//!
//! tool calls arrive as fragments spread across many chunks — the name in
//! one, the arguments a few characters at a time — keyed only by an index.
//! reassembly happens here so the loop above never sees a partial call.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::agent::http::EventStream;

const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// a conversation turn, in the shape the api expects on the way back in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text("system", content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::text("user", content)
    }
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn tool_result(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(id.into()),
        }
    }
}

/// what one assistant turn produced.
#[derive(Debug, Default)]
pub struct Turn {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    /// set when the provider returned an error frame; the stream stops.
    pub error: Option<String>,
    /// (content, reasoning) deltas in arrival order, so tests — and any
    /// future caller that wants the raw stream — can replay them.
    pub deltas: Vec<(Option<String>, Option<String>)>,
}

// ---- streaming wire shapes -------------------------------------------

#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// openrouter exposes chain-of-thought under `reasoning` for models that
    /// emit it; absent for models that do not.
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default)]
pub struct PartialCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// exercise the api key before a run depends on it.
///
/// `/key` returns the key's own metadata, so this both proves the key is
/// valid and surfaces a spent-out account — which otherwise appears as a
/// mid-run failure several steps in.
pub async fn verify_key(api_key: &str) -> Result<String, String> {
    let headers = vec![("Authorization", format!("Bearer {api_key}"))];
    let resp = crate::agent::http::request(
        "GET",
        "https://openrouter.ai/api/v1/key",
        &headers,
        None,
    )
    .await?;

    if resp.status == 401 || resp.status == 403 {
        return Err("openrouter rejected this api key".to_string());
    }
    if !resp.ok() {
        return Err(format!("openrouter returned http {}", resp.status));
    }

    let v: serde_json::Value =
        serde_json::from_str(&resp.body).map_err(|e| format!("bad key response: {e}"))?;
    let data = v.get("data").unwrap_or(&v);

    let limit = data.get("limit").and_then(|l| l.as_f64());
    let usage = data.get("usage").and_then(|u| u.as_f64()).unwrap_or(0.0);

    match limit {
        Some(l) if usage >= l => Err(format!(
            "openrouter key is out of credit (used {usage:.2} of {l:.2})"
        )),
        Some(l) => Ok(format!("openrouter ok ({usage:.2} of {l:.2} used)")),
        None => Ok("openrouter ok".to_string()),
    }
}

/// fold one streamed frame into the accumulating turn.
///
/// extracted from run_turn so the reassembly rules have their own tests:
/// tool-call fragments arrive spread across many frames keyed only by an
/// index, and getting this wrong silently corrupts every multi-tool turn.
/// returns false when the caller should stop reading ("[DONE]" or an error
/// frame, which lands in `turn.error`).
pub fn absorb_chunk(
    payload: &str,
    turn: &mut Turn,
    partials: &mut BTreeMap<usize, PartialCall>,
) -> bool {
    if payload == "[DONE]" {
        return false;
    }

    let chunk: Chunk = match serde_json::from_str(payload) {
        Ok(c) => c,
        // a malformed frame mid-stream is not worth killing a long run
        // over; keep reading and let the turn end on its own terms.
        Err(_) => return true,
    };

    if let Some(e) = chunk.error {
        turn.error = Some(format!("provider error: {}", e.message));
        return false;
    }

    let Some(choice) = chunk.choices.into_iter().next() else {
        return true;
    };

    if let Some(reason) = choice.finish_reason {
        turn.finish_reason = Some(reason);
    }

    if let Some(r) = choice.delta.reasoning.as_deref() {
        if !r.is_empty() {
            turn.reasoning.push_str(r);
            turn.deltas.push((None, Some(r.to_string())));
        }
    }
    if let Some(c) = choice.delta.content.as_deref() {
        if !c.is_empty() {
            turn.content.push_str(c);
            turn.deltas.push((Some(c.to_string()), None));
        }
    }

    for d in choice.delta.tool_calls.into_iter().flatten() {
        let slot = partials.entry(d.index).or_default();
        if let Some(id) = d.id {
            slot.id = id;
        }
        if let Some(f) = d.function {
            if let Some(name) = f.name {
                slot.name.push_str(&name);
            }
            if let Some(args) = f.arguments {
                slot.arguments.push_str(&args);
            }
        }
    }

    true
}

pub struct LlmRequest<'a> {
    pub api_key: &'a str,
    pub model: &'a str,
    pub reasoning_effort: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a serde_json::Value,
}

/// run one assistant turn, invoking `on_delta` as text arrives.
///
/// `should_stop` is polled between chunks so pressing stop takes effect
/// immediately rather than after the model finishes talking.
pub async fn run_turn<F, S>(
    req: LlmRequest<'_>,
    mut on_delta: F,
    should_stop: S,
) -> Result<Turn, String>
where
    F: FnMut(Option<&str>, Option<&str>),
    S: Fn() -> bool,
{
    let mut body = serde_json::json!({
        "model": req.model,
        "messages": req.messages,
        "tools": req.tools,
        "tool_choice": "auto",
        "stream": true,
    });
    if req.reasoning_effort != "none" {
        body["reasoning"] = serde_json::json!({ "effort": req.reasoning_effort });
    }

    let headers = vec![
        ("Authorization", format!("Bearer {}", req.api_key)),
        ("Content-Type", "application/json".to_string()),
        // openrouter attributes traffic with these; harmless and polite.
        ("X-Title", "vanish".to_string()),
    ];

    let mut stream = EventStream::open(ENDPOINT, &headers, &body.to_string()).await?;

    let mut turn = Turn::default();
    let mut partials: BTreeMap<usize, PartialCall> = BTreeMap::new();

    // absorb_chunk records deltas into turn.deltas rather than taking the
    // callback itself — that keeps it a pure function the test binary can
    // drive without mocks. the cursor replays each new delta to the caller
    // exactly once, in arrival order, so streaming behaves identically to
    // the pre-refactor inline loop.
    let mut seen_deltas = 0usize;

    while let Some(payload) = stream.next().await? {
        if should_stop() {
            stream.cancel();
            return Err("stopped".to_string());
        }
        if !absorb_chunk(&payload, &mut turn, &mut partials) {
            break;
        }
        if let Some(e) = turn.error.take() {
            return Err(e);
        }
        while seen_deltas < turn.deltas.len() {
            let (c, r) = &turn.deltas[seen_deltas];
            on_delta(c.as_deref(), r.as_deref());
            seen_deltas += 1;
        }
    }

    finalize_turn(&mut turn, partials);
    Ok(turn)
}

/// materialize the reassembled partial calls into the turn's tool_calls,
/// applying the api's shape requirements (arguments must be a json object
/// string; a call with no name is noise and is dropped).
pub fn finalize_turn(turn: &mut Turn, partials: BTreeMap<usize, PartialCall>) {
    turn.tool_calls = partials
        .into_values()
        .filter(|p| !p.name.is_empty())
        .map(|p| ToolCall {
            id: if p.id.is_empty() {
                format!("call_{}", p.name)
            } else {
                p.id
            },
            kind: "function".to_string(),
            function: FunctionCall {
                name: p.name,
                arguments: if p.arguments.trim().is_empty() {
                    "{}".to_string()
                } else {
                    p.arguments
                },
            },
        })
        .collect();
}
