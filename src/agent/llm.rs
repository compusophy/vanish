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

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
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

    while let Some(payload) = stream.next().await? {
        if should_stop() {
            stream.cancel();
            return Err("stopped".to_string());
        }
        if payload == "[DONE]" {
            break;
        }

        let chunk: Chunk = match serde_json::from_str(&payload) {
            Ok(c) => c,
            // a malformed frame mid-stream is not worth killing a long run
            // over; keep reading and let the turn end on its own terms.
            Err(_) => continue,
        };

        if let Some(e) = chunk.error {
            return Err(format!("provider error: {}", e.message));
        }

        let Some(choice) = chunk.choices.into_iter().next() else {
            continue;
        };

        if let Some(reason) = choice.finish_reason {
            turn.finish_reason = Some(reason);
        }

        if let Some(r) = choice.delta.reasoning.as_deref() {
            if !r.is_empty() {
                turn.reasoning.push_str(r);
                on_delta(None, Some(r));
            }
        }
        if let Some(c) = choice.delta.content.as_deref() {
            if !c.is_empty() {
                turn.content.push_str(c);
                on_delta(Some(c), None);
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
    }

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
                // an empty fragment stream still has to parse as an object
                arguments: if p.arguments.trim().is_empty() {
                    "{}".to_string()
                } else {
                    p.arguments
                },
            },
        })
        .collect();

    Ok(turn)
}
