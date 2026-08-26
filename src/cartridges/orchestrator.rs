//! L5 — the orchestrator: actors, mailboxes, supervision, hot-swap, and
//! (ABI v2) synchronous calls between actors (CARTRIDGE_PLAN §7–§8).
//!
//! every cartridge in a composition is an ACTOR: private state (its kv
//! namespace in the host + its linear memory), a mailbox, and behavior =
//! its exported cart_handle. the orchestrator is a deterministic PUMP —
//! one message per `pump` call, round-robin across actors in wiring order
//! — because the browser worker is single-threaded and the agent loop must
//! interleave with it: the caller decides when to pump and how much fuel
//! each step gets. time is PASSED IN (`now_ms`), never read here, so every
//! backoff decision is testable and D1 holds (no clock watching inside).
//!
//! delivery is at-most-once: a message that crashes its actor is gone,
//! recorded in the event log with the trap that took it. the ack a guest
//! returns is cart_handle's packed response, surfaced as `Event::Delivered`.
//!
//! supervision (§8): a trapped actor is re-instantiated from its verified
//! image — fresh memory, SAME host, so kv state survives — after an
//! exponential backoff (RESTART_BASE_MS × 2^(attempt−1)), for at most
//! MAX_RESTARTS consecutive crashes; then it is marked Failed and the
//! event log says so LOUDLY (D4). one successful delivery resets the count.
//!
//! hot-swap (§8): `swap` replaces an actor's module between messages (the
//! pump never leaves one mid-message), moves its host across, re-inits it
//! with its remembered config, and keeps its mailbox — pending work is not
//! replayed, it is simply still pending. state that mattered was in kv;
//! that is why state-in-kv is an architectural rule, not a style.
//!
//! guest → guest, asynchronously: an actor's `emit(topic, payload)` is
//! routed to the provider of port `topic` — IF the emitter declared that
//! port under `requires`. capability-based (declared at wire time),
//! auditable (every denial is an event), never a direct call.
//!
//! guest → guest, synchronously (ABI v2 `call`): the same capability
//! check, then the callee's cart_handle runs IMMEDIATELY under the
//! caller's fuel and its answer is written back into the caller's memory.
//! RE-ENTRANCY DISCIPLINE: every guest run happens on a cartridge TAKEN
//! OUT of the composition (`Composition::take` / `put_back`), and the
//! shared state lives behind `Rc<RefCell<_>>` that is never borrowed
//! while guest code runs — so a nested call can borrow the rest of the
//! composition, a busy callee is refused (not deadlocked), and the chain
//! is bounded by MAX_CALL_CHAIN. failures are SOFT for the guest (it sees
//! 0) and LOUD for the log (`CallFailed`, `Denied`); a callee that traps
//! inside a call is handed to supervision exactly as if it had crashed on
//! its own mail.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::{Rc, Weak};

use super::abi::Host;
use super::composition::{ComposeError, Composition};
use super::lifecycle::{CallError, Cartridge, Verified};
use super::manifest::CartridgeManifest;
use super::ports::wire;
use super::runtime::Trap;

/// consecutive crashes tolerated before an actor is marked Failed.
pub const MAX_RESTARTS: u32 = 5;
/// first backoff; doubles per consecutive crash (0.5s, 1s, 2s, 4s, 8s).
pub const RESTART_BASE_MS: i64 = 500;
/// pending messages one actor may hold; beyond it, sends are refused loudly
/// rather than growing without bound behind a slow or failed actor.
pub const MAX_MAILBOX: usize = 256;
/// deepest chain of synchronous calls (a → b → c …). each level is a
/// native frame set, so this bounds native recursion as well as guest work.
pub const MAX_CALL_CHAIN: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// the emitting actor, or None for a message from the host.
    pub from: Option<String>,
    pub topic: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Up,
    /// crashed; will be re-instantiated on the first pump at or after
    /// `not_before_ms` that finds mail for it.
    Restarting {
        attempt: u32,
        not_before_ms: i64,
        reason: String,
    },
    /// gave up. mail queues (a swap may revive it) but nothing is pumped.
    Failed { reason: String },
}

impl Health {
    fn label(&self) -> String {
        match self {
            Health::Up => "up".into(),
            Health::Restarting { attempt, .. } => format!("restarting (attempt {attempt})"),
            Health::Failed { reason } => format!("failed: {reason}"),
        }
    }
}

/// everything observable about the pump, for the feed (D4: every failure
/// renders) and for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Delivered {
        to: String,
        from: Option<String>,
        topic: String,
        response: Vec<u8>,
    },
    Crashed {
        slug: String,
        reason: String,
        attempt: u32,
        retry_at_ms: i64,
    },
    Restarted { slug: String, attempt: u32 },
    Failed { slug: String, reason: String },
    /// an emit or call to a port the actor did not declare under `requires`.
    Denied { from: String, topic: String, reason: String },
    /// a host send that has nowhere to go, or a mailbox at capacity.
    Undeliverable { topic: String, reason: String },
    Swapped { slug: String },
    /// a synchronous call completed: `to` answered `from` on `port`.
    Called {
        from: String,
        to: String,
        port: String,
        response_len: usize,
    },
    /// a synchronous call did not complete; the caller saw 0.
    CallFailed { from: String, port: String, reason: String },
    /// an actor is rebuilt on the next pump WITHOUT backoff or a crash
    /// counted: it was interrupted by someone else's fault (the caller's
    /// fuel ran out inside it), and its memory may be mid-write.
    Reset { slug: String, reason: String },
}

/// what the orchestrator and every actor's host share. borrowed only
/// BETWEEN guest runs — see the module doc's re-entrancy discipline.
struct Shared<H: Host> {
    comp: Composition<ActorHost<H>>,
    /// verified images by slug, for restarts without re-decoding.
    images: BTreeMap<String, Verified>,
    actors: BTreeMap<String, Actor>,
    events: Vec<Event>,
    /// the pump's current time, so a crash inside a nested call can
    /// schedule its backoff without the guest passing a clock around.
    now_ms: i64,
    call_depth: u32,
}

/// the host each actor actually talks to: delegates the v1 surface to the
/// real host, captures emits for routing, and mediates v2 `call`s through
/// the shared composition. the real host still sees every emit first (it
/// is the feed's witness); it never sees a `call` — routing is the
/// orchestrator's job, not the platform's.
pub struct ActorHost<H: Host> {
    pub inner: H,
    outbox: Vec<(Vec<u8>, Vec<u8>)>,
    slug: String,
    /// ports this actor declared under `requires` — its call capabilities.
    requires: BTreeSet<String>,
    shared: Weak<RefCell<Shared<H>>>,
}

impl<H: Host> ActorHost<H> {
    fn new(inner: H, manifest: &CartridgeManifest) -> Self {
        Self {
            inner,
            outbox: Vec::new(),
            slug: manifest.slug.clone(),
            requires: manifest.requires.iter().map(|p| p.name.clone()).collect(),
            shared: Weak::new(),
        }
    }
}

impl<H: Host> Host for ActorHost<H> {
    fn log(&mut self, level: i32, msg: &[u8]) {
        self.inner.log(level, msg);
    }
    fn now_ms(&mut self) -> i64 {
        self.inner.now_ms()
    }
    fn store_get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.inner.store_get(key)
    }
    fn store_set(&mut self, key: &[u8], value: &[u8]) -> Result<(), String> {
        self.inner.store_set(key, value)
    }
    fn emit(&mut self, topic: &[u8], payload: &[u8]) -> Result<(), String> {
        self.inner.emit(topic, payload)?;
        self.outbox.push((topic.to_vec(), payload.to_vec()));
        Ok(())
    }

    fn call(&mut self, port: &[u8], msg: &[u8], fuel: &mut u64) -> Result<Option<Vec<u8>>, String> {
        let port = String::from_utf8_lossy(port).into_owned();
        let Some(rc) = self.shared.upgrade() else {
            return Err("no orchestrator is attached to route calls".into());
        };
        if !self.requires.contains(&port) {
            rc.borrow_mut().events.push(Event::Denied {
                from: self.slug.clone(),
                topic: port,
                reason: "port not declared under `requires` — declare it in the manifest so \
                         the wiring can check it"
                    .into(),
            });
            return Ok(None);
        }
        // resolve and TAKE the callee under one short borrow, so the
        // callee's own host can borrow the shared state while it runs.
        let (to, mut callee) = {
            let mut sh = rc.borrow_mut();
            let refuse = |sh: &mut Shared<H>, reason: String| {
                sh.events.push(Event::CallFailed {
                    from: self.slug.clone(),
                    port: port.clone(),
                    reason,
                });
            };
            if sh.call_depth >= MAX_CALL_CHAIN {
                refuse(&mut sh, format!("call chain deeper than {MAX_CALL_CHAIN}"));
                return Ok(None);
            }
            let Some(to) = sh.comp.provider_of(&port).map(str::to_string) else {
                refuse(&mut sh, "no provider in the current wiring".into());
                return Ok(None);
            };
            match sh.actors.get(&to).map(|a| &a.health) {
                Some(Health::Up) => {}
                Some(other) => {
                    let reason = format!("'{to}' is not up: {}", other.label());
                    refuse(&mut sh, reason);
                    return Ok(None);
                }
                None => {
                    refuse(&mut sh, format!("no actor named '{to}'"));
                    return Ok(None);
                }
            }
            let Some(callee) = sh.comp.take(&to) else {
                refuse(&mut sh, format!("'{to}' is busy (already running in this call chain)"));
                return Ok(None);
            };
            sh.call_depth += 1;
            (to, callee)
        };

        // the guest runs with NO shared borrow held, on the caller's fuel.
        let result = callee.handle_with(msg, fuel);

        let mut sh = rc.borrow_mut();
        sh.comp.put_back(to.clone(), callee);
        sh.call_depth -= 1;
        sh.route_outbox(&to);
        match result {
            Ok(bytes) => {
                sh.events.push(Event::Called {
                    from: self.slug.clone(),
                    to,
                    port,
                    response_len: bytes.len(),
                });
                Ok(Some(bytes))
            }
            Err(e) => {
                let reason = e.to_string();
                sh.events.push(Event::CallFailed {
                    from: self.slug.clone(),
                    port,
                    reason: format!("'{to}' failed: {reason}"),
                });
                let now = sh.now_ms;
                if matches!(e, CallError::Trap(Trap::FuelExhausted)) {
                    // the CALLER's budget ran out inside the callee. that is
                    // not the callee's crash — counting it would let a stingy
                    // caller back an innocent provider off into Failed. its
                    // memory may be mid-write, so it is rebuilt on the next
                    // pump: no backoff, no crash counted.
                    sh.reset(&to, "the caller's fuel ran out inside it", now);
                } else {
                    sh.crash(&to, reason, now);
                }
                Ok(None)
            }
        }
    }
}

struct Actor {
    mailbox: VecDeque<Envelope>,
    health: Health,
    /// consecutive crashes; reset by a successful delivery.
    crashes: u32,
    /// the config it booted with — replayed on restart and swap.
    config: Vec<u8>,
}

impl<H: Host> Shared<H> {
    fn enqueue(&mut self, from: Option<String>, to: String, topic: &str, payload: &[u8]) -> bool {
        let Some(actor) = self.actors.get_mut(&to) else {
            self.events.push(Event::Undeliverable {
                topic: topic.to_string(),
                reason: format!("no actor named '{to}'"),
            });
            return false;
        };
        if actor.mailbox.len() >= MAX_MAILBOX {
            self.events.push(Event::Undeliverable {
                topic: topic.to_string(),
                reason: format!("mailbox of '{to}' is full ({MAX_MAILBOX} pending)"),
            });
            return false;
        }
        actor.mailbox.push_back(Envelope {
            from,
            topic: topic.to_string(),
            payload: payload.to_vec(),
        });
        true
    }

    fn crash(&mut self, slug: &str, reason: String, now_ms: i64) {
        let Some(a) = self.actors.get_mut(slug) else {
            return;
        };
        a.crashes += 1;
        let attempt = a.crashes;
        if attempt > MAX_RESTARTS {
            a.health = Health::Failed {
                reason: reason.clone(),
            };
            self.events.push(Event::Failed {
                slug: slug.to_string(),
                reason: format!("{reason} — gave up after {MAX_RESTARTS} restarts"),
            });
        } else {
            let retry_at_ms = now_ms + (RESTART_BASE_MS << (attempt - 1));
            a.health = Health::Restarting {
                attempt,
                not_before_ms: retry_at_ms,
                reason: reason.clone(),
            };
            self.events.push(Event::Crashed {
                slug: slug.to_string(),
                reason,
                attempt,
                retry_at_ms,
            });
        }
    }

    /// schedule a rebuild on the next pump with NO backoff and NO crash
    /// counted — for interruptions that were not the actor's own doing.
    fn reset(&mut self, slug: &str, why: &str, now_ms: i64) {
        let Some(a) = self.actors.get_mut(slug) else {
            return;
        };
        let reason = format!("reset: {why}");
        a.health = Health::Restarting {
            attempt: a.crashes,
            not_before_ms: now_ms,
            reason: reason.clone(),
        };
        self.events.push(Event::Reset {
            slug: slug.to_string(),
            reason,
        });
    }

    /// route everything `slug` emitted during its last run: to the
    /// provider of the topic's port when the emitter declared it under
    /// `requires`; otherwise a Denied event, never a delivery.
    fn route_outbox(&mut self, slug: &str) {
        let out: Vec<(Vec<u8>, Vec<u8>)> = match self.comp.get_mut(slug) {
            Some(c) => std::mem::take(&mut c.host_mut().outbox),
            None => return,
        };
        if out.is_empty() {
            return;
        }
        let requires: BTreeSet<String> = self
            .comp
            .get(slug)
            .map(|c| c.host().requires.clone())
            .unwrap_or_default();
        for (topic, payload) in out {
            let topic = String::from_utf8_lossy(&topic).into_owned();
            if !requires.contains(&topic) {
                self.events.push(Event::Denied {
                    from: slug.to_string(),
                    topic,
                    reason: "port not declared under `requires` — declare it in the manifest \
                             so the wiring can check it"
                        .into(),
                });
                continue;
            }
            let Some(to) = self.comp.provider_of(&topic).map(str::to_string) else {
                self.events.push(Event::Denied {
                    from: slug.to_string(),
                    topic,
                    reason: "no provider in the current wiring".into(),
                });
                continue;
            };
            self.enqueue(Some(slug.to_string()), to, &topic, &payload);
        }
    }
}

pub struct Orchestrator<H: Host> {
    shared: Rc<RefCell<Shared<H>>>,
    /// round-robin position over wiring order.
    cursor: usize,
}

impl<H: Host> std::fmt::Debug for Orchestrator<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sh = self.shared.borrow();
        f.debug_struct("Orchestrator")
            .field("order", &sh.comp.wiring().order)
            .field(
                "health",
                &sh.actors
                    .iter()
                    .map(|(s, a)| (s.clone(), a.health.clone()))
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl<H: Host> Orchestrator<H> {
    /// wire, verify, instantiate — refusals in that order, each named.
    pub fn load(
        entries: &[(CartridgeManifest, Vec<u8>)],
        mut host_for: impl FnMut(&CartridgeManifest) -> H,
    ) -> Result<Self, ComposeError> {
        let manifests: Vec<CartridgeManifest> = entries.iter().map(|(m, _)| m.clone()).collect();
        wire(&manifests)?;
        let mut images = BTreeMap::new();
        let mut list = Vec::with_capacity(entries.len());
        for (m, bytes) in entries {
            let image = Verified::verify(m.clone(), bytes).map_err(|error| {
                ComposeError::Load {
                    slug: m.slug.clone(),
                    error,
                }
            })?;
            images.insert(m.slug.clone(), image.clone());
            list.push(image);
        }
        let comp = Composition::from_verified(list, |m| ActorHost::new(host_for(m), m))?;
        let actors = comp
            .slugs()
            .map(|s| {
                (
                    s.to_string(),
                    Actor {
                        mailbox: VecDeque::new(),
                        health: Health::Up,
                        crashes: 0,
                        config: Vec::new(),
                    },
                )
            })
            .collect();
        let shared = Rc::new(RefCell::new(Shared {
            comp,
            images,
            actors,
            events: Vec::new(),
            now_ms: 0,
            call_depth: 0,
        }));
        // every actor's host learns where the shared state lives.
        {
            let weak = Rc::downgrade(&shared);
            let mut sh = shared.borrow_mut();
            let slugs: Vec<String> = sh.comp.slugs().map(String::from).collect();
            for slug in slugs {
                if let Some(c) = sh.comp.get_mut(&slug) {
                    c.host_mut().shared = weak.clone();
                }
            }
        }
        Ok(Self { shared, cursor: 0 })
    }

    /// run `f` on `slug`'s cartridge TAKEN OUT of the composition, with no
    /// shared borrow held — the only way guest code ever runs here.
    fn run_taken<R>(
        &self,
        slug: &str,
        f: impl FnOnce(&mut Cartridge<ActorHost<H>>) -> R,
    ) -> Option<R> {
        let mut cart = self.shared.borrow_mut().comp.take(slug)?;
        let r = f(&mut cart);
        self.shared.borrow_mut().comp.put_back(slug.to_string(), cart);
        Some(r)
    }

    /// cart_init every actor in wiring order with `config_for(slug)`; the
    /// configs are remembered for restarts and swaps. a refusal is not
    /// transient: that actor is marked Failed (until swapped) and the
    /// error is returned; actors after it in the order never boot.
    pub fn boot(
        &mut self,
        config_for: &dyn Fn(&str) -> Vec<u8>,
        fuel: u64,
    ) -> Result<Vec<String>, ComposeError> {
        let order = {
            let mut sh = self.shared.borrow_mut();
            for (slug, actor) in &mut sh.actors {
                actor.config = config_for(slug);
            }
            sh.comp.wiring().order.clone()
        };
        let mut done = Vec::with_capacity(order.len());
        for slug in order {
            let config = config_for(&slug);
            let result = self
                .run_taken(&slug, |c| c.init(&config, fuel))
                .ok_or_else(|| ComposeError::UnknownCartridge(slug.clone()))?;
            let mut sh = self.shared.borrow_mut();
            sh.route_outbox(&slug);
            if let Err(error) = result {
                let reason = format!("boot: {error}");
                if let Some(a) = sh.actors.get_mut(&slug) {
                    a.health = Health::Failed {
                        reason: reason.clone(),
                    };
                }
                sh.events.push(Event::Failed {
                    slug: slug.clone(),
                    reason,
                });
                return Err(ComposeError::Call { slug, error });
            }
            done.push(slug);
        }
        Ok(done)
    }

    /// a message from the host to whoever provides `port`. false (with an
    /// Undeliverable event) when nothing does or the mailbox is full.
    pub fn send(&mut self, port: &str, payload: &[u8]) -> bool {
        let mut sh = self.shared.borrow_mut();
        let Some(slug) = sh.comp.provider_of(port).map(str::to_string) else {
            sh.events.push(Event::Undeliverable {
                topic: port.to_string(),
                reason: "no cartridge provides this port".into(),
            });
            return false;
        };
        sh.enqueue(None, slug, port, payload)
    }

    /// deliver ONE message: the next actor in round-robin order that has
    /// mail and is runnable at `now_ms` (Up, or Restarting with its backoff
    /// elapsed — in which case it is re-instantiated first). returns false
    /// when nothing was runnable. every outcome lands in the event log.
    pub fn pump(&mut self, now_ms: i64, fuel: u64) -> bool {
        let order = {
            let mut sh = self.shared.borrow_mut();
            sh.now_ms = now_ms;
            sh.comp.wiring().order.clone()
        };
        let n = order.len();
        for i in 0..n {
            let idx = (self.cursor + i) % n;
            let slug = order[idx].clone();
            enum Plan {
                Skip,
                Restart(u32),
                Deliver,
            }
            // a down actor whose backoff elapsed is rebuilt whether or not
            // it has mail: call-only providers are never pumped for mail,
            // and they must come back too.
            let plan = {
                let sh = self.shared.borrow();
                match sh.actors.get(&slug) {
                    None => Plan::Skip,
                    Some(a) => match &a.health {
                        Health::Failed { .. } => Plan::Skip,
                        Health::Restarting {
                            attempt,
                            not_before_ms,
                            ..
                        } => {
                            if now_ms < *not_before_ms {
                                Plan::Skip
                            } else {
                                Plan::Restart(*attempt)
                            }
                        }
                        Health::Up if a.mailbox.is_empty() => Plan::Skip,
                        Health::Up => Plan::Deliver,
                    },
                }
            };
            match plan {
                Plan::Skip => continue,
                Plan::Restart(attempt) => {
                    self.cursor = idx + 1;
                    if !self.try_restart(&slug, attempt, now_ms, fuel) {
                        return true;
                    }
                    let has_mail = self.pending(&slug) > 0;
                    if !has_mail {
                        return true; // the rebuild was the step
                    }
                }
                Plan::Deliver => {
                    self.cursor = idx + 1;
                }
            }

            let Some(env) = self
                .shared
                .borrow_mut()
                .actors
                .get_mut(&slug)
                .and_then(|a| a.mailbox.pop_front())
            else {
                continue;
            };
            let mut budget = fuel;
            let Some(result) = self.run_taken(&slug, |c| c.handle_with(&env.payload, &mut budget))
            else {
                continue;
            };
            let mut sh = self.shared.borrow_mut();
            // emits that happened before a trap still happened: route them.
            sh.route_outbox(&slug);
            match result {
                Ok(response) => {
                    if let Some(a) = sh.actors.get_mut(&slug) {
                        a.crashes = 0;
                    }
                    sh.events.push(Event::Delivered {
                        to: slug,
                        from: env.from,
                        topic: env.topic,
                        response,
                    });
                }
                Err(e) => {
                    sh.crash(&slug, e.to_string(), now_ms);
                }
            }
            return true;
        }
        false
    }

    /// rebuild a down actor whose backoff elapsed: true when it is Up again
    /// (Restarted recorded), false when the rebuild itself crashed (which
    /// schedules the next backoff).
    fn try_restart(&mut self, slug: &str, attempt: u32, now_ms: i64, fuel: u64) -> bool {
        match self.restart(slug, fuel) {
            Ok(()) => {
                let mut sh = self.shared.borrow_mut();
                if let Some(a) = sh.actors.get_mut(slug) {
                    a.health = Health::Up;
                }
                sh.events.push(Event::Restarted {
                    slug: slug.to_string(),
                    attempt,
                });
                true
            }
            Err(reason) => {
                self.shared.borrow_mut().crash(slug, reason, now_ms);
                false
            }
        }
    }

    /// a SYNCHRONOUS request from the host to whoever provides `port`: the
    /// cognitive orchestrator's primitive ("route the prompt to whichever
    /// cartridge provides reasoning" — plan §8). the host needs no
    /// capability; the provider must be Up (a down one whose backoff has
    /// elapsed is rebuilt first; otherwise NotUp). runs on the same
    /// take/put_back path as everything else, so the provider may `call`
    /// out while answering. a trap is supervised exactly like a crash on
    /// mail, and surfaces as the error.
    pub fn request(
        &mut self,
        port: &str,
        msg: &[u8],
        now_ms: i64,
        fuel: u64,
    ) -> Result<Vec<u8>, ComposeError> {
        self.shared.borrow_mut().now_ms = now_ms;
        let slug = self
            .provider_of(port)
            .ok_or_else(|| ComposeError::NoProvider(port.to_string()))?;
        match self.health(&slug) {
            Some(Health::Up) => {}
            Some(Health::Restarting {
                attempt,
                not_before_ms,
                reason,
            }) => {
                if now_ms < not_before_ms || !self.try_restart(&slug, attempt, now_ms, fuel) {
                    return Err(ComposeError::NotUp {
                        slug,
                        reason: format!("restarting (attempt {attempt}): {reason}"),
                    });
                }
            }
            Some(Health::Failed { reason }) => {
                return Err(ComposeError::NotUp {
                    slug,
                    reason: format!("failed: {reason}"),
                })
            }
            None => return Err(ComposeError::UnknownCartridge(slug)),
        }
        let mut budget = fuel;
        let result = self
            .run_taken(&slug, |c| c.handle_with(msg, &mut budget))
            .ok_or_else(|| ComposeError::NotUp {
                slug: slug.clone(),
                reason: "busy (already running in this call chain)".into(),
            })?;
        let mut sh = self.shared.borrow_mut();
        sh.route_outbox(&slug);
        match result {
            Ok(response) => {
                if let Some(a) = sh.actors.get_mut(&slug) {
                    a.crashes = 0;
                }
                sh.events.push(Event::Delivered {
                    to: slug,
                    from: None,
                    topic: port.to_string(),
                    response: response.clone(),
                });
                Ok(response)
            }
            Err(error) => {
                sh.crash(&slug, error.to_string(), now_ms);
                Err(ComposeError::Call { slug, error })
            }
        }
    }

    /// pump until nothing is runnable or `max` deliveries; returns the count.
    pub fn pump_all(&mut self, now_ms: i64, fuel: u64, max: usize) -> usize {
        let mut n = 0;
        while n < max && self.pump(now_ms, fuel) {
            n += 1;
        }
        n
    }

    /// re-instantiate from the image (same host) and re-init with the
    /// remembered config. an Err here counts as another crash.
    fn restart(&mut self, slug: &str, fuel: u64) -> Result<(), String> {
        let (image, config) = {
            let sh = self.shared.borrow();
            let image = sh
                .images
                .get(slug)
                .cloned()
                .ok_or_else(|| format!("no verified image for '{slug}'"))?;
            let config = sh
                .actors
                .get(slug)
                .map(|a| a.config.clone())
                .unwrap_or_default();
            (image, config)
        };
        self.shared
            .borrow_mut()
            .comp
            .reinstantiate(slug, image)
            .map_err(|e| e.to_string())?;
        let result = self
            .run_taken(slug, |c| c.init(&config, fuel))
            .ok_or_else(|| format!("'{slug}' vanished during restart"))?;
        self.shared.borrow_mut().route_outbox(slug);
        result.map_err(|e| format!("re-init after restart: {e}"))
    }

    /// replace one actor's module between messages. atomic: a wiring
    /// break or bad bytes refuse with nothing changed. on success the host
    /// (and its kv state) and the mailbox carry over, the actor's call
    /// capabilities follow the new manifest, it is re-inited with its
    /// remembered config, and its health resets to Up.
    pub fn swap(
        &mut self,
        manifest: CartridgeManifest,
        bytes: &[u8],
        fuel: u64,
    ) -> Result<(), ComposeError> {
        let slug = manifest.slug.clone();
        let requires: BTreeSet<String> =
            manifest.requires.iter().map(|p| p.name.clone()).collect();
        let image = Verified::verify(manifest, bytes).map_err(|error| ComposeError::Load {
            slug: slug.clone(),
            error,
        })?;
        let config = {
            let mut sh = self.shared.borrow_mut();
            sh.comp.swap(image.clone(), |mut h| {
                h.requires = requires;
                h
            })?;
            sh.images.insert(slug.clone(), image);
            sh.actors
                .get(&slug)
                .map(|a| a.config.clone())
                .unwrap_or_default()
        };
        let init = self
            .run_taken(&slug, |c| c.init(&config, fuel))
            .ok_or_else(|| ComposeError::UnknownCartridge(slug.clone()))?;
        let mut sh = self.shared.borrow_mut();
        sh.route_outbox(&slug);
        match init {
            Ok(()) => {
                if let Some(a) = sh.actors.get_mut(&slug) {
                    a.health = Health::Up;
                    a.crashes = 0;
                }
                sh.events.push(Event::Swapped { slug });
                Ok(())
            }
            Err(error) => {
                // the new module is in place but refused its config: loud.
                let reason = format!("swap: {error}");
                if let Some(a) = sh.actors.get_mut(&slug) {
                    a.health = Health::Failed {
                        reason: reason.clone(),
                    };
                }
                sh.events.push(Event::Failed {
                    slug: slug.clone(),
                    reason,
                });
                Err(ComposeError::Call { slug, error })
            }
        }
    }

    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.shared.borrow_mut().events)
    }

    pub fn health(&self, slug: &str) -> Option<Health> {
        self.shared.borrow().actors.get(slug).map(|a| a.health.clone())
    }

    pub fn pending(&self, slug: &str) -> usize {
        self.shared
            .borrow()
            .actors
            .get(slug)
            .map(|a| a.mailbox.len())
            .unwrap_or(0)
    }

    /// the wiring's initialization order.
    pub fn order(&self) -> Vec<String> {
        self.shared.borrow().comp.wiring().order.clone()
    }

    pub fn provider_of(&self, port: &str) -> Option<String> {
        self.shared
            .borrow()
            .comp
            .provider_of(port)
            .map(str::to_string)
    }

    /// inspect the REAL host behind an actor (the recorder in tests, the
    /// feed/kv in the browser). None between a `take` and its `put_back`
    /// — i.e. never from outside a guest run.
    pub fn with_host<R>(&self, slug: &str, f: impl FnOnce(&H) -> R) -> Option<R> {
        let sh = self.shared.borrow();
        sh.comp.get(slug).map(|c| f(&c.host().inner))
    }

    pub fn with_host_mut<R>(&mut self, slug: &str, f: impl FnOnce(&mut H) -> R) -> Option<R> {
        let mut sh = self.shared.borrow_mut();
        sh.comp.get_mut(slug).map(|c| f(&mut c.host_mut().inner))
    }
}
