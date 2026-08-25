//! L5 — the orchestrator: actors, mailboxes, supervision, hot-swap
//! (CARTRIDGE_PLAN §8, build-order item 7).
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
//! guest → guest messages: an actor's `emit(topic, payload)` is routed to
//! the provider of port `topic` — IF the emitter declared that port under
//! `requires`. capability-based (declared at wire time), auditable (every
//! denial is an event), never a direct call. a synchronous request/response
//! `call` is ABI v2 and rides on this mailbox as its mediator.

use std::collections::{BTreeMap, VecDeque};

use super::abi::Host;
use super::composition::{ComposeError, Composition};
use super::lifecycle::Verified;
use super::manifest::CartridgeManifest;
use super::ports::wire;

/// consecutive crashes tolerated before an actor is marked Failed.
pub const MAX_RESTARTS: u32 = 5;
/// first backoff; doubles per consecutive crash (0.5s, 1s, 2s, 4s, 8s).
pub const RESTART_BASE_MS: i64 = 500;
/// pending messages one actor may hold; beyond it, sends are refused loudly
/// rather than growing without bound behind a slow or failed actor.
pub const MAX_MAILBOX: usize = 256;

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
    /// an emit to a port the emitter did not declare under `requires`.
    Denied { from: String, topic: String, reason: String },
    /// a host send that has nowhere to go, or a mailbox at capacity.
    Undeliverable { topic: String, reason: String },
    Swapped { slug: String },
}

/// the host each actor actually talks to: delegates everything to the real
/// host and captures emits for routing. the real host still sees the emit
/// first (it is the feed's observer; its refusal traps the guest as ever).
pub struct ActorHost<H> {
    pub inner: H,
    outbox: Vec<(Vec<u8>, Vec<u8>)>,
}

impl<H: Host> ActorHost<H> {
    pub fn new(inner: H) -> Self {
        Self {
            inner,
            outbox: Vec::new(),
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
}

struct Actor {
    mailbox: VecDeque<Envelope>,
    health: Health,
    /// consecutive crashes; reset by a successful delivery.
    crashes: u32,
    /// the config it booted with — replayed on restart and swap.
    config: Vec<u8>,
}

pub struct Orchestrator<H: Host> {
    comp: Composition<ActorHost<H>>,
    /// verified images by slug, for restarts without re-decoding.
    images: BTreeMap<String, Verified>,
    actors: BTreeMap<String, Actor>,
    events: Vec<Event>,
    /// round-robin position over wiring order.
    cursor: usize,
}

impl<H: Host> std::fmt::Debug for Orchestrator<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Orchestrator")
            .field("order", &self.comp.wiring().order)
            .field(
                "health",
                &self
                    .actors
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
        let comp = Composition::from_verified(list, |m| ActorHost::new(host_for(m)))?;
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
        Ok(Self {
            comp,
            images,
            actors,
            events: Vec::new(),
            cursor: 0,
        })
    }

    /// cart_init every actor in wiring order with `config_for(slug)`; the
    /// configs are remembered for restarts and swaps. a refusal is not
    /// transient: that actor is marked Failed (until swapped) and the
    /// error is returned, exactly as Composition::init_all reports it.
    pub fn boot(
        &mut self,
        config_for: &dyn Fn(&str) -> Vec<u8>,
        fuel: u64,
    ) -> Result<Vec<String>, ComposeError> {
        for (slug, actor) in &mut self.actors {
            actor.config = config_for(slug);
        }
        let configs: BTreeMap<String, Vec<u8>> = self
            .actors
            .iter()
            .map(|(s, a)| (s.clone(), a.config.clone()))
            .collect();
        let result = self
            .comp
            .init_all(&|slug| configs.get(slug).cloned().unwrap_or_default(), fuel);
        if let Err(ComposeError::Call { slug, error }) = &result {
            let reason = format!("boot: {error}");
            if let Some(a) = self.actors.get_mut(slug) {
                a.health = Health::Failed {
                    reason: reason.clone(),
                };
            }
            self.events.push(Event::Failed {
                slug: slug.clone(),
                reason,
            });
        }
        result
    }

    /// a message from the host to whoever provides `port`. false (with an
    /// Undeliverable event) when nothing does or the mailbox is full.
    pub fn send(&mut self, port: &str, payload: &[u8]) -> bool {
        let Some(slug) = self.comp.provider_of(port).map(str::to_string) else {
            self.events.push(Event::Undeliverable {
                topic: port.to_string(),
                reason: "no cartridge provides this port".into(),
            });
            return false;
        };
        self.enqueue(None, slug, port, payload)
    }

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

    /// deliver ONE message: the next actor in round-robin order that has
    /// mail and is runnable at `now_ms` (Up, or Restarting with its backoff
    /// elapsed — in which case it is re-instantiated first). returns false
    /// when nothing was runnable. every outcome lands in the event log.
    pub fn pump(&mut self, now_ms: i64, fuel: u64) -> bool {
        let order = self.comp.wiring().order.clone();
        let n = order.len();
        for i in 0..n {
            let idx = (self.cursor + i) % n;
            let slug = order[idx].clone();
            let Some(actor) = self.actors.get(&slug) else {
                continue;
            };
            if actor.mailbox.is_empty() {
                continue;
            }
            match actor.health.clone() {
                Health::Failed { .. } => continue,
                Health::Restarting {
                    attempt,
                    not_before_ms,
                    ..
                } => {
                    if now_ms < not_before_ms {
                        continue;
                    }
                    self.cursor = idx + 1;
                    match self.restart(&slug, fuel) {
                        Ok(()) => {
                            if let Some(a) = self.actors.get_mut(&slug) {
                                a.health = Health::Up;
                            }
                            self.events.push(Event::Restarted {
                                slug: slug.clone(),
                                attempt,
                            });
                        }
                        Err(reason) => {
                            self.crash(&slug, reason, now_ms);
                            return true;
                        }
                    }
                }
                Health::Up => {
                    self.cursor = idx + 1;
                }
            }

            let Some(env) = self
                .actors
                .get_mut(&slug)
                .and_then(|a| a.mailbox.pop_front())
            else {
                continue;
            };
            let result = self.comp.handle(&slug, &env.payload, fuel);
            // emits that happened before a trap still happened: route them.
            self.route_outbox(&slug);
            match result {
                Ok(response) => {
                    if let Some(a) = self.actors.get_mut(&slug) {
                        a.crashes = 0;
                    }
                    self.events.push(Event::Delivered {
                        to: slug,
                        from: env.from,
                        topic: env.topic,
                        response,
                    });
                }
                Err(e) => {
                    let reason = match e {
                        ComposeError::Call { error, .. } => error.to_string(),
                        other => other.to_string(),
                    };
                    self.crash(&slug, reason, now_ms);
                }
            }
            return true;
        }
        false
    }

    /// pump until nothing is runnable or `max` deliveries; returns the count.
    pub fn pump_all(&mut self, now_ms: i64, fuel: u64, max: usize) -> usize {
        let mut n = 0;
        while n < max && self.pump(now_ms, fuel) {
            n += 1;
        }
        n
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

    /// re-instantiate from the image (same host) and re-init with the
    /// remembered config. an Err here counts as another crash.
    fn restart(&mut self, slug: &str, fuel: u64) -> Result<(), String> {
        let image = self
            .images
            .get(slug)
            .cloned()
            .ok_or_else(|| format!("no verified image for '{slug}'"))?;
        self.comp
            .reinstantiate(slug, image)
            .map_err(|e| e.to_string())?;
        let config = self
            .actors
            .get(slug)
            .map(|a| a.config.clone())
            .unwrap_or_default();
        let cart = self
            .comp
            .get_mut(slug)
            .ok_or_else(|| format!("'{slug}' vanished during restart"))?;
        cart.init(&config, fuel)
            .map_err(|e| format!("re-init after restart: {e}"))
    }

    /// route everything `slug` emitted during its last call: to the
    /// provider of the topic's port when the emitter declared it under
    /// `requires`; otherwise a Denied event, never a delivery.
    fn route_outbox(&mut self, slug: &str) {
        let out: Vec<(Vec<u8>, Vec<u8>)> = match self.comp.get_mut(slug) {
            Some(c) => c.host_mut().outbox.drain(..).collect(),
            None => return,
        };
        if out.is_empty() {
            return;
        }
        let requires: Vec<String> = self
            .comp
            .get(slug)
            .map(|c| c.manifest().requires.iter().map(|p| p.name.clone()).collect())
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

    /// replace one actor's module between messages. atomic: a wiring
    /// break or bad bytes refuse with nothing changed. on success the host
    /// (and its kv state) and the mailbox carry over, the actor is re-inited
    /// with its remembered config, and its health resets to Up.
    pub fn swap(
        &mut self,
        manifest: CartridgeManifest,
        bytes: &[u8],
        fuel: u64,
    ) -> Result<(), ComposeError> {
        let slug = manifest.slug.clone();
        let image = Verified::verify(manifest, bytes).map_err(|error| ComposeError::Load {
            slug: slug.clone(),
            error,
        })?;
        self.comp.swap(image.clone(), |h| h)?;
        self.images.insert(slug.clone(), image);
        let config = self
            .actors
            .get(&slug)
            .map(|a| a.config.clone())
            .unwrap_or_default();
        let init = self
            .comp
            .get_mut(&slug)
            .ok_or_else(|| ComposeError::UnknownCartridge(slug.clone()))?
            .init(&config, fuel);
        match init {
            Ok(()) => {
                if let Some(a) = self.actors.get_mut(&slug) {
                    a.health = Health::Up;
                    a.crashes = 0;
                }
                self.events.push(Event::Swapped { slug });
                Ok(())
            }
            Err(error) => {
                // the new module is in place but refused its config: loud.
                let reason = format!("swap: {error}");
                if let Some(a) = self.actors.get_mut(&slug) {
                    a.health = Health::Failed {
                        reason: reason.clone(),
                    };
                }
                self.events.push(Event::Failed {
                    slug: slug.clone(),
                    reason,
                });
                Err(ComposeError::Call { slug, error })
            }
        }
    }

    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn health(&self, slug: &str) -> Option<&Health> {
        self.actors.get(slug).map(|a| &a.health)
    }

    pub fn pending(&self, slug: &str) -> usize {
        self.actors.get(slug).map(|a| a.mailbox.len()).unwrap_or(0)
    }

    /// the REAL host behind an actor (the recorder in tests, the feed/kv
    /// in the browser).
    pub fn host(&self, slug: &str) -> Option<&H> {
        self.comp.get(slug).map(|c| &c.host().inner)
    }

    pub fn host_mut(&mut self, slug: &str) -> Option<&mut H> {
        self.comp.get_mut(slug).map(|c| &mut c.host_mut().inner)
    }

    pub fn composition(&self) -> &Composition<ActorHost<H>> {
        &self.comp
    }
}
