//! fetch, reachable from either the window or a worker.
//!
//! everything the harness talks to (openrouter, the github api) sets
//! `access-control-allow-origin: *` and permits an `authorization` header,
//! which is what makes a backendless harness possible at all: the browser
//! can be the only client, with no relay standing in the middle.

use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{ReadableStreamDefaultReader, Response};

use crate::platform::opfs::describe;

/// how long a stream may go silent before it is treated as dead.
///
/// generous: a high-effort model can think for a long time before emitting
/// its first token. but not infinite — without this, a dropped connection
/// leaves the run awaiting a chunk that will never arrive, `running` stuck
/// true, no steps, no error, and nothing to look at but a "running" label.
const STREAM_IDLE_TIMEOUT_MS: i32 = 180_000;

/// marker resolved by the timeout arm of a race, so the caller can tell
/// "the timer won" from "the read returned".
const TIMEOUT_MARKER: &str = "__vanish_timeout";

/// a promise that resolves to the timeout marker after `ms`.
fn timeout_promise(ms: i32) -> Promise {
    Promise::new(&mut |resolve, _reject| {
        let global = js_sys::global();
        let marker = Object::new();
        let _ = Reflect::set(
            &marker,
            &JsValue::from_str(TIMEOUT_MARKER),
            &JsValue::TRUE,
        );
        let Ok(set_timeout) = Reflect::get(&global, &JsValue::from_str("setTimeout"))
            .and_then(|f| f.dyn_into::<Function>().map_err(|e| e))
        else {
            return;
        };
        let cb = Closure::once_into_js(move || {
            let _ = resolve.call1(&JsValue::NULL, &marker);
        });
        let _ = set_timeout.call2(&global, &cb, &JsValue::from_f64(ms as f64));
    })
}

fn is_timeout(v: &JsValue) -> bool {
    Reflect::get(v, &JsValue::from_str(TIMEOUT_MARKER))
        .ok()
        .and_then(|m| m.as_bool())
        .unwrap_or(false)
}

/// await a timer. the loop has no deadline, so it can afford to wait for a
/// ci build to settle rather than guessing whether its commit was good.
pub async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let global = js_sys::global();
        let Ok(set_timeout) = Reflect::get(&global, &JsValue::from_str("setTimeout"))
            .and_then(|f| f.dyn_into::<Function>().map_err(|e| e))
        else {
            // no timer available: resolve immediately rather than hanging the
            // caller forever.
            let _ = resolve.call0(&JsValue::NULL);
            return;
        };
        let _ = set_timeout.call2(&global, &resolve, &JsValue::from_f64(ms as f64));
    });
    let _ = JsFuture::from(promise).await;
}

pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

fn global_fetch() -> Result<Function, String> {
    Reflect::get(&js_sys::global(), &JsValue::from_str("fetch"))
        .map_err(|e| format!("fetch unavailable: {}", describe(&e)))?
        .dyn_into::<Function>()
        .map_err(|_| "fetch is not callable in this context".to_string())
}

fn build_init(method: &str, headers: &[(&str, String)], body: Option<&str>) -> Object {
    let init = Object::new();
    let _ = Reflect::set(
        &init,
        &JsValue::from_str("method"),
        &JsValue::from_str(method),
    );

    let hdrs = Object::new();
    for (k, v) in headers {
        let _ = Reflect::set(&hdrs, &JsValue::from_str(k), &JsValue::from_str(v));
    }
    let _ = Reflect::set(&init, &JsValue::from_str("headers"), &hdrs);

    if let Some(b) = body {
        let _ = Reflect::set(&init, &JsValue::from_str("body"), &JsValue::from_str(b));
    }
    init
}

async fn raw_fetch(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Option<&str>,
) -> Result<Response, String> {
    let f = global_fetch()?;
    let init = build_init(method, headers, body);
    let promise: Promise = f
        .call2(&JsValue::NULL, &JsValue::from_str(url), &init)
        .map_err(|e| format!("fetch({url}) threw: {}", describe(&e)))?
        .dyn_into()
        .map_err(|_| "fetch did not return a promise".to_string())?;

    JsFuture::from(promise)
        .await
        // a rejected fetch is almost always the network or cors, and the
        // browser deliberately hides which. say so rather than printing an
        // opaque object the user cannot act on.
        .map_err(|e| {
            format!(
                "network request to {url} failed ({}). check connectivity and that the api key is valid.",
                describe(&e)
            )
        })?
        .dyn_into::<Response>()
        .map_err(|_| "fetch did not resolve to a response".to_string())
}

/// a complete request/response round trip, body buffered as text.
pub async fn request(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Option<&str>,
) -> Result<HttpResponse, String> {
    let resp = raw_fetch(method, url, headers, body).await?;
    let status = resp.status();
    let text_promise = resp
        .text()
        .map_err(|e| format!("reading body: {}", describe(&e)))?;
    let body = JsFuture::from(text_promise)
        .await
        .map_err(|e| format!("reading body: {}", describe(&e)))?
        .as_string()
        .unwrap_or_default();
    Ok(HttpResponse { status, body })
}

/// an open server-sent-events response, pulled one chunk at a time.
///
/// the loop needs deltas as they arrive (so the ui renders reasoning live)
/// and needs to abandon the stream the instant the user presses stop, so the
/// reader is exposed rather than collected into a string.
pub struct EventStream {
    reader: ReadableStreamDefaultReader,
    decoder: JsValue,
    buffer: String,
    done: bool,
}

impl EventStream {
    pub async fn open(
        url: &str,
        headers: &[(&str, String)],
        body: &str,
    ) -> Result<Self, String> {
        let resp = raw_fetch("POST", url, headers, Some(body)).await?;

        if !(200..300).contains(&resp.status()) {
            // surface the provider's own error text; a bare status code has
            // repeatedly sent this harness debugging in the wrong direction.
            let detail = match resp.text() {
                Ok(p) => JsFuture::from(p)
                    .await
                    .ok()
                    .and_then(|v| v.as_string())
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };
            return Err(format!(
                "llm request failed with http {}: {}",
                resp.status(),
                if detail.is_empty() {
                    "(no response body)".to_string()
                } else {
                    detail.chars().take(600).collect::<String>()
                }
            ));
        }

        let stream = resp
            .body()
            .ok_or_else(|| "response had no body to stream".to_string())?;
        let reader = stream
            .get_reader()
            .dyn_into::<ReadableStreamDefaultReader>()
            .map_err(|_| "could not acquire a stream reader".to_string())?;

        let decoder = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("TextDecoder"))
            .ok()
            .and_then(|c| c.dyn_into::<Function>().ok())
            .and_then(|c| Reflect::construct(&c, &Array::new()).ok())
            .ok_or_else(|| "TextDecoder unavailable".to_string())?
            .into();

        Ok(Self {
            reader,
            decoder,
            buffer: String::new(),
            done: false,
        })
    }

    fn decode(&self, chunk: &Uint8Array) -> String {
        // streaming: true, so a multi-byte character split across two network
        // chunks is held rather than emitted as a replacement char.
        let opts = Object::new();
        let _ = Reflect::set(&opts, &JsValue::from_str("stream"), &JsValue::TRUE);
        Reflect::get(&self.decoder, &JsValue::from_str("decode"))
            .ok()
            .and_then(|f| f.dyn_into::<Function>().ok())
            .and_then(|f| f.call2(&self.decoder, chunk, &opts).ok())
            .and_then(|v| v.as_string())
            .unwrap_or_default()
    }

    /// next complete sse payload line, or None once the stream ends.
    pub async fn next(&mut self) -> Result<Option<String>, String> {
        loop {
            if let Some(payload) = self.take_buffered() {
                return Ok(Some(payload));
            }
            if self.done {
                return Ok(None);
            }

            // race the read against a stall timer. an unraced read waits
            // forever on a connection that has quietly died, which presents
            // as a run that is "running" but producing nothing.
            let raced = js_sys::Array::new();
            raced.push(&self.reader.read());
            raced.push(&timeout_promise(STREAM_IDLE_TIMEOUT_MS));

            let result = JsFuture::from(js_sys::Promise::race(&raced))
                .await
                .map_err(|e| format!("stream read failed: {}", describe(&e)))?;

            if is_timeout(&result) {
                self.cancel();
                return Err(format!(
                    "the model stream went silent for {}s and was abandoned. the run can be started again; nothing written to the working tree is lost.",
                    STREAM_IDLE_TIMEOUT_MS / 1000
                ));
            }

            let finished = Reflect::get(&result, &JsValue::from_str("done"))
                .ok()
                .and_then(|d| d.as_bool())
                .unwrap_or(true);

            if finished {
                self.done = true;
                continue;
            }

            if let Ok(value) = Reflect::get(&result, &JsValue::from_str("value")) {
                if let Ok(bytes) = value.dyn_into::<Uint8Array>() {
                    let text = self.decode(&bytes);
                    self.buffer.push_str(&text);
                }
            }
        }
    }

    /// pull one `data:` payload out of the buffer if a full line is present.
    fn take_buffered(&mut self) -> Option<String> {
        while let Some(idx) = self.buffer.find('\n') {
            let line = self.buffer[..idx].trim_end_matches('\r').to_string();
            self.buffer.drain(..=idx);

            let Some(rest) = line.strip_prefix("data:") else {
                // comments and blank separators are normal sse framing.
                continue;
            };
            let payload = rest.trim();
            if payload.is_empty() {
                continue;
            }
            return Some(payload.to_string());
        }
        None
    }

    /// abandon the stream early (user pressed stop). best effort: a failed
    /// cancel must not mask the reason the run is ending.
    pub fn cancel(&self) {
        let _ = self.reader.cancel();
    }
}
