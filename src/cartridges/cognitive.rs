//! the cognitive orchestrator (CARTRIDGE_PLAN §8 last paragraph, §12;
//! build-order item 8): the agent loop's reasoning policy as a
//! hot-swappable cartridge.
//!
//! THE SHAPE, and why: the model call is async (a streamed fetch in the
//! worker) and the interpreter is synchronous and cannot suspend, so a
//! cartridge cannot MAKE the model call. what it can do is decide what
//! goes in and what comes out. two ports:
//!
//! - `reasoning` — BEFORE the model: the user's prompt in, the prompt to
//!   actually send out. empty = unchanged.
//! - `reasoning.after` — AFTER the model: the assistant's answer in; the
//!   cartridge digests it (kv memory, log) and may answer with a note for
//!   the feed. empty = nothing.
//!
//! whichever cartridge provides those ports IS the reasoning policy, and
//! `Orchestrator::swap` replaces it between prompts — mid-conversation,
//! kv intact. that is the plan's "swapping vanish's own reasoning module
//! becomes a manifest edit", made literal.
//!
//! the loop is never hostage to a cartridge (article iv, D9): no provider
//! → passthrough silently; a trapped or failed provider → passthrough with
//! a note, and supervision takes it from there. the reasoning module is an
//! enhancement of the loop, not a dependency of it.

use super::abi::Host;
use super::composition::ComposeError;
use super::manifest::{CartridgeKind, CartridgeManifest, Port, ABI_VERSION};
use super::memhost::{KvFlush, MemHost};
use super::orchestrator::{Event, Orchestrator};

pub const PORT_BEFORE: &str = "reasoning";
pub const PORT_AFTER: &str = "reasoning.after";

/// FRAMING: cart_handle receives bytes only — no topic — and one cartridge
/// normally provides both ports, so every cognitive message carries a
/// phase byte first. ports remain the wiring/capability key; the phase
/// byte is the guest's dispatch. a framed message is never empty.
pub const PHASE_BEFORE: u8 = 0x00;
pub const PHASE_AFTER: u8 = 0x01;

fn framed(phase: u8, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 1);
    v.push(phase);
    v.extend_from_slice(body);
    v
}

/// what `before` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shaped {
    /// the prompt to send to the model.
    pub prompt: String,
    /// feed-worthy notes from the orchestrator (crashes, denials, swaps —
    /// never routine deliveries).
    pub notes: Vec<String>,
}

pub struct Cognition<H: Host> {
    orch: Orchestrator<H>,
    /// fuel per request. the policy runs once per prompt and once per
    /// answer; a runaway module hits this and is supervised, never waited
    /// on.
    pub fuel: u64,
}

impl<H: Host> Cognition<H> {
    pub fn new(orch: Orchestrator<H>, fuel: u64) -> Self {
        Self { orch, fuel }
    }

    pub fn orchestrator(&mut self) -> &mut Orchestrator<H> {
        &mut self.orch
    }

    /// is any cartridge providing the reasoning port right now?
    pub fn has_policy(&self) -> bool {
        self.orch.provider_of(PORT_BEFORE).is_some()
    }

    /// shape the prompt before the model sees it.
    pub fn before(&mut self, prompt: &str, now_ms: i64) -> Shaped {
        let mut notes = Vec::new();
        let msg = framed(PHASE_BEFORE, prompt.as_bytes());
        let shaped = match self.orch.request(PORT_BEFORE, &msg, now_ms, self.fuel) {
            Ok(bytes) if bytes.is_empty() => prompt.to_string(),
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(ComposeError::NoProvider(_)) => prompt.to_string(),
            Err(e) => {
                notes.push(format!(
                    "🧠 reasoning module could not shape the prompt ({e}) — sending it as written"
                ));
                prompt.to_string()
            }
        };
        notes.extend(self.drain_notes());
        Shaped {
            prompt: shaped,
            notes,
        }
    }

    /// let the policy digest the model's answer. returns feed notes: the
    /// cartridge's own (its non-empty response) plus the orchestrator's.
    pub fn after(&mut self, answer: &str, now_ms: i64) -> Vec<String> {
        let mut notes = Vec::new();
        let msg = framed(PHASE_AFTER, answer.as_bytes());
        match self.orch.request(PORT_AFTER, &msg, now_ms, self.fuel) {
            Ok(bytes) if bytes.is_empty() => {}
            Ok(bytes) => notes.push(String::from_utf8_lossy(&bytes).into_owned()),
            Err(ComposeError::NoProvider(_)) => {}
            Err(e) => notes.push(format!("🧠 reasoning module could not digest the answer ({e})")),
        }
        notes.extend(self.drain_notes());
        notes
    }

    /// feed lines for everything the orchestrator recorded since the last
    /// drain. public because a swap is initiated from outside a hook and its
    /// events (Swapped, or a Failed on a refused config) still belong on the
    /// feed.
    pub fn drain_notes(&mut self) -> Vec<String> {
        self.orch
            .drain_events()
            .iter()
            .filter_map(describe)
            .collect()
    }
}

/// a feed line for an orchestrator event, or None for routine traffic.
pub fn describe(e: &Event) -> Option<String> {
    Some(match e {
        Event::Delivered { .. } | Event::Called { .. } => return None,
        Event::Crashed {
            slug,
            reason,
            attempt,
            ..
        } => format!("💥 cartridge '{slug}' crashed ({reason}) — restart {attempt}/{} scheduled", super::orchestrator::MAX_RESTARTS),
        Event::Restarted { slug, attempt } => format!("↻ cartridge '{slug}' restarted (attempt {attempt})"),
        Event::Failed { slug, reason } => format!("⛔ cartridge '{slug}' FAILED: {reason} — swap it to revive"),
        Event::Denied { from, topic, .. } => format!("🚫 cartridge '{from}' tried port '{topic}' without declaring it"),
        Event::Undeliverable { topic, reason } => format!("📭 undeliverable on '{topic}': {reason}"),
        Event::Swapped { slug } => format!("🔁 cartridge '{slug}' hot-swapped"),
        Event::CallFailed { from, port, reason } => format!("📵 call from '{from}' to '{port}' failed: {reason}"),
        Event::Reset { slug, reason } => format!("♻ cartridge '{slug}' reset ({reason})"),
    })
}

/// the reference reasoning policy, v1: remember the last prompt and the
/// last answer in kv, change nothing. the baseline every swap is measured
/// against — and proof that a policy module needs no host import beyond
/// v1's store_set/log to be useful.
pub const REASONING_V1: &str = r#"
    extern "C" {
        fn log(level: i32, ptr: i32, len: i32);
        fn store_set(k_ptr: i32, k_len: i32, v_ptr: i32, v_len: i32) -> i32;
    }
    pub fn cart_alloc(size: i32) -> i32 {
        let hp: i32 = load_i32(0);
        if hp == 0 { hp = data_end(); }
        store_i32(0, hp + size);
        return hp;
    }
    pub fn cart_init(p: i32, n: i32) -> i32 {
        log(1, unpack_ptr("reasoning v1 up: passthrough + remember"), unpack_len("reasoning v1 up: passthrough + remember"));
        return 0;
    }
    // byte 0 is the phase (0 = prompt in, 1 = answer in); the body follows.
    // reasoning: remember the prompt, send it unchanged (empty = unchanged).
    // reasoning.after: remember the answer, say nothing.
    pub fn cart_handle(p: i32, n: i32) -> i64 {
        if n == 0 { return 0; }
        if load_u8(p) == 1 {
            store_set(unpack_ptr("last_answer"), unpack_len("last_answer"), p + 1, n - 1);
            return 0;
        }
        store_set(unpack_ptr("last_prompt"), unpack_len("last_prompt"), p + 1, n - 1);
        return 0;
    }
"#;

/// the reference policy, v2: like v1, but every prompt is prefixed with
/// "[v2] " — a visible change, so a live hot-swap is observable from the
/// feed with no instrumentation. (rustlite has no string concatenation;
/// the prefix is copied byte by byte, which is exactly what a real policy
/// would do to assemble a prompt from kv memory.)
pub const REASONING_V2: &str = r#"
    extern "C" {
        fn log(level: i32, ptr: i32, len: i32);
        fn store_set(k_ptr: i32, k_len: i32, v_ptr: i32, v_len: i32) -> i32;
    }
    pub fn cart_alloc(size: i32) -> i32 {
        let hp: i32 = load_i32(0);
        if hp == 0 { hp = data_end(); }
        store_i32(0, hp + size);
        return hp;
    }
    pub fn cart_init(p: i32, n: i32) -> i32 {
        log(1, unpack_ptr("reasoning v2 up: prefix + remember"), unpack_len("reasoning v2 up: prefix + remember"));
        return 0;
    }
    pub fn cart_handle(p: i32, n: i32) -> i64 {
        if n == 0 { return 0; }
        if load_u8(p) == 1 {
            store_set(unpack_ptr("last_answer"), unpack_len("last_answer"), p + 1, n - 1);
            return 0;
        }
        let body: i32 = p + 1;
        let blen: i32 = n - 1;
        store_set(unpack_ptr("last_prompt"), unpack_len("last_prompt"), body, blen);
        let plen: i32 = unpack_len("[v2] ");
        let pptr: i32 = unpack_ptr("[v2] ");
        let out: i32 = cart_alloc(plen + blen);
        let i: i32 = 0;
        while i < plen {
            store_u8(out + i, load_u8(pptr + i));
            i = i + 1;
        }
        let j: i32 = 0;
        while j < blen {
            store_u8(out + plen + j, load_u8(body + j));
            j = j + 1;
        }
        return pack(out, plen + blen);
    }
"#;

// ---- the reasoner, as the browser boots and swaps it -------------------
//
// item 8b: everything above is host-agnostic. what follows is the ONE
// configuration the worker actually runs — a single cognitive cartridge
// named `reasoner` providing both reasoning ports over a write-behind
// MemHost — expressed as pure functions so the browser glue is reduced to
// opfs reads, opfs writes, and feed notes.

pub const REASONER_SLUG: &str = "reasoner";

/// fuel for one hook. a policy that will not finish inside this is
/// supervised (crash → restart → passthrough), never waited on.
pub const REASONER_FUEL: u64 = 2_000_000;

/// the manifest a policy gets when the user supplies none: the reasoner,
/// providing both phases and requiring nothing.
pub fn reasoner_manifest() -> CartridgeManifest {
    CartridgeManifest {
        slug: REASONER_SLUG.to_string(),
        kind: CartridgeKind::Cognitive,
        version: "0.1.0".to_string(),
        abi_version: ABI_VERSION,
        provides: vec![
            Port {
                name: PORT_BEFORE.to_string(),
            },
            Port {
                name: PORT_AFTER.to_string(),
            },
        ],
        requires: vec![],
    }
}

/// rustlite → wasm, with the compiler's own words kept. a policy that does
/// not compile must say WHERE, or the textarea is untypable.
pub fn compile_policy(src: &str) -> Result<Vec<u8>, String> {
    let program = super::rustlite::parse(src).map_err(|e| format!("rustlite: {}", e.msg))?;
    super::wasm::emit_module(&program)
        .map_err(|e| format!("rustlite `{}`: {}", e.fn_name, e.msg))
}

/// a manifest (blank = the default reasoner one) plus a source, verified as
/// far as they can be without instantiating anything.
pub fn parse_policy(manifest_json: &str, src: &str) -> Result<(CartridgeManifest, Vec<u8>), String> {
    let manifest = if manifest_json.trim().is_empty() {
        reasoner_manifest()
    } else {
        let m = CartridgeManifest::parse(manifest_json).map_err(|e| format!("manifest: {e}"))?;
        m.validate().map_err(|e| format!("manifest: {e}"))?;
        m
    };
    let bytes = compile_policy(src)?;
    Ok((manifest, bytes))
}

/// build the worker's cognition: compile, instantiate over a MemHost seeded
/// with whatever the last session flushed, then init.
///
/// the kv is seeded BEFORE `cart_init` so a policy that reads its own memory
/// at boot sees it. a store that does not parse is reported and skipped —
/// losing a policy's notes is survivable, refusing to boot the loop over it
/// is not (D4, article iv).
pub fn boot_reasoner(
    manifest_json: &str,
    src: &str,
    kv: Option<&str>,
    fuel: u64,
) -> Result<(Cognition<MemHost>, Vec<String>), String> {
    let (manifest, bytes) = parse_policy(manifest_json, src)?;
    let slug = manifest.slug.clone();
    let mut orch = Orchestrator::load(&[(manifest, bytes)], |m| MemHost::new(&m.slug))
        .map_err(|e| format!("{e}"))?;

    let mut notes = Vec::new();
    if let Some(text) = kv {
        match orch.with_host_mut(&slug, |h| h.seed_encoded(text)) {
            Some(Ok(n)) if n > 0 => {
                notes.push(format!("🧠 '{slug}' restored {n} remembered key(s)"))
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => notes.push(format!(
                "⚠ '{slug}' memory did not parse ({e}) — starting with an empty store"
            )),
            None => {}
        }
    }

    orch.boot(&|_| Vec::new(), fuel).map_err(|e| format!("{e}"))?;
    let mut cog = Cognition::new(orch, fuel);
    notes.extend(cog.take_logs());
    notes.extend(cog.drain_notes());
    Ok((cog, notes))
}

impl Cognition<MemHost> {
    /// the clock, passed in once per step. the host reads no clock (D1), so
    /// this is the only way `now_ms` inside a cartridge is anything but 0.
    pub fn set_now(&mut self, now_ms: i64) {
        for slug in self.orch.order() {
            self.orch.with_host_mut(&slug, |h| h.set_now(now_ms));
        }
    }

    /// the guest's own log lines since the last take, as feed notes.
    pub fn take_logs(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for slug in self.orch.order() {
            if let Some(logs) = self.orch.with_host_mut(&slug, |h| h.take_logs()) {
                out.extend(logs.into_iter().map(|(_, m)| format!("🧠 {slug}: {m}")));
            }
        }
        out
    }

    /// everything that must reach opfs after this hook (D2). empty when no
    /// cartridge wrote anything.
    pub fn take_flushes(&mut self) -> Vec<KvFlush> {
        let mut out = Vec::new();
        for slug in self.orch.order() {
            if let Some(Some(f)) = self.orch.with_host_mut(&slug, |h| h.take_flush()) {
                out.push(f);
            }
        }
        out
    }

    /// replace the running policy from source. the host — and with it every
    /// remembered key — carries over; a bad manifest or a source that does
    /// not compile changes nothing.
    pub fn swap_policy(&mut self, manifest_json: &str, src: &str) -> Result<String, String> {
        let (manifest, bytes) = parse_policy(manifest_json, src)?;
        let slug = manifest.slug.clone();
        let fuel = self.fuel;
        self.orch
            .swap(manifest, &bytes, fuel)
            .map_err(|e| format!("{e}"))?;
        Ok(slug)
    }
}
