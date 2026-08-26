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
use super::corpus::{self, Origin, Sample, Verdict};
use super::manifest::{CartridgeKind, CartridgeManifest, Port, ABI_VERSION};
use super::memhost::{KvFlush, KvPairs, MemHost};
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

/// the key the standing note lives under, and the marker the agent writes
/// to set it. these are Rust-side mirrors of literals inside the module
/// below — rustlite has no way to be handed a constant — so a test pins
/// them together rather than trusting them to stay in step.
pub const STANDING_KEY: &str = "standing";
pub const CARRY_MARKER: &str = "CARRY:";
/// how much of a carried line is kept. a note is a note, not a transcript:
/// unbounded, it would grow every later prompt for as long as the policy
/// runs.
pub const STANDING_MAX: usize = 400;

/// the line v3 appends to every prompt, teaching its own contract. the
/// policy cannot edit the system prompt, so it says what it offers in the
/// only channel it has.
pub const STANDING_PROTOCOL: &str =
    "(policy: to carry one line into your next prompt, write CARRY: followed by that line in your answer.)";

/// the reference policy, v3 — the first one that is worth swapping TO.
///
/// v1 and v2 are demonstrations: one changes nothing, the other prefixes a
/// visible marker. v3 does something the loop cannot do for itself. the
/// agent's transcript is trimmed at `KEEP_MESSAGES` and is per-conversation;
/// a cartridge's kv is neither. so v3 gives the agent ONE line of memory
/// that outlives both:
///
/// - on an answer, the first `CARRY:` in it (to the end of that line) is
///   stored as the standing note. an answer without the marker changes
///   nothing; `CARRY:` with nothing after it clears the note.
/// - on a prompt, the standing note is prepended, and the protocol line is
///   appended so the agent knows the channel exists and how to use it.
///
/// this is the first reference module to read its own memory back
/// (`store_get`), to assemble a prompt from three sources, and to search
/// bytes — the language features items 5 and 7 added, doing work.
///
/// what it deliberately is NOT: a judge of whether the shaped prompt was
/// better. that needs the corpus capture in plan §9 (build item 9). until
/// then a policy can be verified to DO what it says, not to help.
pub const REASONING_V3: &str = r#"
    extern "C" {
        fn log(level: i32, ptr: i32, len: i32);
        fn store_get(k_ptr: i32, k_len: i32) -> i64;
        fn store_set(k_ptr: i32, k_len: i32, v_ptr: i32, v_len: i32) -> i32;
    }
    pub fn cart_alloc(size: i32) -> i32 {
        let hp: i32 = load_i32(0);
        if hp == 0 { hp = data_end(); }
        store_i32(0, hp + size);
        return hp;
    }
    // copy len bytes and answer with the position just past them, so a
    // multi-part assembly reads as one chain of writes.
    fn blit(dst: i32, src: i32, len: i32) -> i32 {
        let i: i32 = 0;
        while i < len {
            store_u8(dst + i, load_u8(src + i));
            i = i + 1;
        }
        return dst + len;
    }
    // a blank line. rustlite string literals carry no escapes, so the two
    // newlines are written as bytes rather than hidden inside a literal.
    fn gap(dst: i32) -> i32 {
        store_u8(dst, 10);
        store_u8(dst + 1, 10);
        return dst + 2;
    }
    // index of the first occurrence of nd in h, or -1.
    fn find(h: i32, hlen: i32, nd: i32, ndlen: i32) -> i32 {
        let i: i32 = 0;
        while i + ndlen <= hlen {
            let j: i32 = 0;
            let ok: bool = true;
            while j < ndlen {
                if load_u8(h + i + j) != load_u8(nd + j) {
                    ok = false;
                    j = ndlen;
                } else {
                    j = j + 1;
                }
            }
            if ok { return i; }
            i = i + 1;
        }
        return -1;
    }
    pub fn cart_init(p: i32, n: i32) -> i32 {
        log(1, unpack_ptr("reasoning v3 up: standing note"), unpack_len("reasoning v3 up: standing note"));
        return 0;
    }
    // byte 0 is the phase (0 = prompt in, 1 = answer in); the body follows.
    pub fn cart_handle(p: i32, n: i32) -> i64 {
        if n == 0 { return 0; }
        let body: i32 = p + 1;
        let blen: i32 = n - 1;

        // the answer phase: take the first CARRY: to the end of its line.
        if load_u8(p) == 1 {
            let at: i32 = find(body, blen, unpack_ptr("CARRY:"), unpack_len("CARRY:"));
            if at < 0 { return 0; }
            let s: i32 = at + unpack_len("CARRY:");
            if s < blen {
                if load_u8(body + s) == 32 { s = s + 1; }
            }
            // scan to the newline, remembering WHERE it was: reusing the
            // loop variable to stop would throw the position away.
            let e: i32 = s;
            let stop: i32 = blen;
            while e < blen {
                if load_u8(body + e) == 10 { stop = e; e = blen; } else { e = e + 1; }
            }
            let keep: i32 = stop - s;
            if keep > 400 { keep = 400; }
            if keep < 0 { keep = 0; }
            store_set(unpack_ptr("standing"), unpack_len("standing"), body + s, keep);
            return 0;
        }

        // the prompt phase: note (if any), then the prompt, then the
        // protocol line.
        let held: i64 = store_get(unpack_ptr("standing"), unpack_len("standing"));
        let hptr: i32 = unpack_ptr(held);
        let hlen: i32 = unpack_len(held);
        let lead: i32 = unpack_ptr("[standing note] ");
        let leadlen: i32 = unpack_len("[standing note] ");
        let proto: i32 = unpack_ptr("(policy: to carry one line into your next prompt, write CARRY: followed by that line in your answer.)");
        let protolen: i32 = unpack_len("(policy: to carry one line into your next prompt, write CARRY: followed by that line in your answer.)");

        let head: i32 = 0;
        if hlen > 0 { head = leadlen + hlen + 2; }
        let total: i32 = head + blen + 2 + protolen;
        let out: i32 = cart_alloc(total);
        let w: i32 = out;
        if hlen > 0 {
            w = blit(w, lead, leadlen);
            w = blit(w, hptr, hlen);
            w = gap(w);
        }
        w = blit(w, body, blen);
        w = gap(w);
        w = blit(w, proto, protolen);
        return pack(out, total);
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

/// what a candidate policy did when it was made to run before being
/// allowed to replace the running one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rehearsal {
    /// what the probe prompt became under the candidate.
    pub shaped: String,
    /// what the candidate answered on the `after` phase, if anything.
    pub note: Option<String>,
    /// keys it wrote — in the scratch store, never the live one.
    pub wrote: Vec<String>,
}

/// the two messages a candidate is made to handle. fixed strings, because a
/// rehearsal that varied with the conversation would refuse a policy on
/// monday and accept it on tuesday.
pub const REHEARSAL_PROMPT: &str = "rehearsal: a prompt";
pub const REHEARSAL_ANSWER: &str = "rehearsal: an answer";

/// run a candidate policy end to end before it replaces a running one:
/// instantiate it over a SCRATCH host seeded from a copy of the live memory,
/// init it, and put one message through each phase it declares.
///
/// why this exists. `parse_policy` proves a module compiles; supervision
/// catches one that traps later. between those two sits the case this
/// closes: a module that compiles and then traps on its FIRST message would
/// be installed, crash on the next real prompt, and cost a restart cycle and
/// a passthrough before anyone learned why. rehearsing moves that discovery
/// to the swap, where the answer is simply "no, and here is the trap".
///
/// the scratch host is seeded from a COPY and then discarded, so a rehearsal
/// can neither read stale memory nor write into the live store — and a policy
/// that only works against an empty store is caught here rather than in
/// production.
pub fn rehearse(
    manifest: CartridgeManifest,
    bytes: &[u8],
    kv: KvPairs,
    fuel: u64,
) -> Result<Rehearsal, String> {
    if !manifest.provides.iter().any(|p| p.name == PORT_BEFORE) {
        return Err(format!(
            "the module does not provide '{PORT_BEFORE}', so nothing would route to it — \
             a reasoning policy must declare that port"
        ));
    }
    let slug = manifest.slug.clone();
    let mut orch = Orchestrator::load(&[(manifest, bytes.to_vec())], |m| MemHost::new(&m.slug))
        .map_err(|e| format!("{e}"))?;
    orch.with_host_mut(&slug, |h| h.seed(kv));
    orch.boot(&|_| Vec::new(), fuel)
        .map_err(|e| format!("it refused to start: {e}"))?;

    // straight through the orchestrator rather than `before`/`after`: those
    // two turn a failure into a passthrough on purpose, which is exactly the
    // answer a rehearsal must not accept.
    let mut cog = Cognition::new(orch, fuel);
    let out = cog
        .orch
        .request(
            PORT_BEFORE,
            &framed(PHASE_BEFORE, REHEARSAL_PROMPT.as_bytes()),
            0,
            fuel,
        )
        .map_err(|e| format!("it failed on a prompt: {e}"))?;
    let shaped = if out.is_empty() {
        REHEARSAL_PROMPT.to_string()
    } else {
        String::from_utf8_lossy(&out).into_owned()
    };

    // the after phase is optional: a policy may shape prompts and digest
    // nothing. only a DECLARED port that then fails is a refusal.
    let note = match cog.orch.request(
        PORT_AFTER,
        &framed(PHASE_AFTER, REHEARSAL_ANSWER.as_bytes()),
        0,
        fuel,
    ) {
        Ok(b) if b.is_empty() => None,
        Ok(b) => Some(String::from_utf8_lossy(&b).into_owned()),
        Err(ComposeError::NoProvider(_)) => None,
        Err(e) => return Err(format!("it failed on an answer: {e}")),
    };

    let wrote = cog
        .take_flushes()
        .into_iter()
        .flat_map(|f| f.keys)
        .collect();
    Ok(Rehearsal {
        shaped,
        note,
        wrote,
    })
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

    /// replace the running policy from source, but only after the candidate
    /// has proven it runs (`rehearse`). the host — and with it every
    /// remembered key — carries over; a bad manifest, a source that does not
    /// compile, and a module that fails its rehearsal all change nothing.
    ///
    /// this is the door the agent itself walks through (the `swap_cartridge`
    /// tool), so "compiles" is not a high enough bar: an autonomous run that
    /// installs a policy which traps on its own next prompt has broken the
    /// thing it reasons with, and the only evidence would be a restart
    /// cycle in the feed.
    ///
    /// EVERY ATTEMPT RETURNS A SAMPLE (plan §9, item 9), pass or fail —
    /// whether it also returns a swap is the verdict. a corpus of only the
    /// programs that worked teaches nothing about the boundary, and the
    /// boundary is where a generated program actually fails; the refusal
    /// text stored is the compiler's or the rehearsal's own words.
    pub fn swap_policy(
        &mut self,
        manifest_json: &str,
        src: &str,
        origin: Origin,
        at_ms: i64,
    ) -> (Sample, Result<(String, Rehearsal), String>) {
        // a source that does not compile has no module and therefore no
        // trace; the sample says so rather than inventing one.
        let (manifest, bytes) = match parse_policy(manifest_json, src) {
            Ok(ok) => ok,
            Err(reason) => {
                let sample = corpus::sample(
                    src,
                    None,
                    origin,
                    Verdict::Refused {
                        reason: reason.clone(),
                    },
                    at_ms,
                );
                return (sample, Err(reason));
            }
        };
        let slug = manifest.slug.clone();
        let fuel = self.fuel;
        // a COPY of what the running policy remembers: the candidate has to
        // work against the memory it will actually inherit, not a blank one.
        let kv = self.orch.with_host(&slug, |h| h.snapshot()).unwrap_or_default();

        let rehearsal = match rehearse(manifest.clone(), &bytes, kv, fuel) {
            Ok(r) => r,
            Err(reason) => {
                // it emitted, so there IS a trace — a refused program with a
                // real opcode sequence is the most useful negative there is.
                let sample = corpus::sample(
                    src,
                    Some(&bytes),
                    origin,
                    Verdict::Refused {
                        reason: reason.clone(),
                    },
                    at_ms,
                );
                return (sample, Err(reason));
            }
        };

        let verdict = Verdict::Verified {
            shaped: rehearsal.shaped.clone(),
        };
        let sample = corpus::sample(src, Some(&bytes), origin, verdict, at_ms);

        match self.orch.swap(manifest, &bytes, fuel) {
            Ok(()) => (sample, Ok((slug, rehearsal))),
            Err(e) => {
                let reason = format!("{e}");
                // the rehearsal passed and the install did not: the program
                // is sound, the composition refused it. record the program as
                // verified and hand the caller the wiring error — conflating
                // the two would poison the corpus with a false negative.
                (sample, Err(reason))
            }
        }
    }
}
