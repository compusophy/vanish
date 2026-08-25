//! L4 — a composed set of loaded cartridges, addressed by slug or by PORT.
//!
//! `Composition::load` wires the manifests FIRST (ports.rs) and only then
//! instantiates cartridges, so a mis-wired set is refused before any memory
//! exists. `init_all` boots in wiring order — every provider before what
//! requires it — and stops at the first refusal, naming the slug: a half-
//! initialized set is not something to keep running quietly (D4).
//! `handle_port` is the composition primitive item 8 builds on: the
//! cognitive orchestrator routes a prompt to whichever cartridge provides
//! the active "reasoning" port, and swapping that module becomes a manifest
//! edit rather than a code change.

use std::collections::BTreeMap;

use super::abi::Host;
use super::lifecycle::{CallError, Cartridge, LoadError};
use super::manifest::CartridgeManifest;
use super::ports::{wire, WireError, Wiring};

#[derive(Debug, Clone, PartialEq)]
pub enum ComposeError {
    Wire(WireError),
    /// one cartridge failed `Cartridge::load`; the set is refused whole.
    Load { slug: String, error: LoadError },
    UnknownCartridge(String),
    NoProvider(String),
    /// a lifecycle call on one member failed.
    Call { slug: String, error: CallError },
}

impl From<WireError> for ComposeError {
    fn from(e: WireError) -> Self {
        ComposeError::Wire(e)
    }
}

impl std::fmt::Display for ComposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComposeError::Wire(e) => write!(f, "cannot compose: {e}"),
            ComposeError::Load { slug, error } => write!(f, "cartridge '{slug}': {error}"),
            ComposeError::UnknownCartridge(s) => {
                write!(f, "no cartridge named '{s}' in this composition")
            }
            ComposeError::NoProvider(p) => {
                write!(f, "no cartridge in this composition provides port '{p}'")
            }
            ComposeError::Call { slug, error } => write!(f, "cartridge '{slug}': {error}"),
        }
    }
}

pub struct Composition<H: Host> {
    wiring: Wiring,
    cartridges: BTreeMap<String, Cartridge<H>>,
}

impl<H: Host> std::fmt::Debug for Composition<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Composition")
            .field("order", &self.wiring.order)
            .field("providers", &self.wiring.providers)
            .finish_non_exhaustive()
    }
}

impl<H: Host> Composition<H> {
    /// wire, then load. `host_for` builds one host per cartridge — the
    /// browser hands each its own kv namespace and feed prefix; tests hand
    /// each a recorder.
    pub fn load(
        entries: &[(CartridgeManifest, Vec<u8>)],
        mut host_for: impl FnMut(&CartridgeManifest) -> H,
    ) -> Result<Self, ComposeError> {
        let manifests: Vec<CartridgeManifest> = entries.iter().map(|(m, _)| m.clone()).collect();
        let wiring = wire(&manifests)?;
        let mut cartridges = BTreeMap::new();
        for (m, bytes) in entries {
            let host = host_for(m);
            let cart = Cartridge::load(m.clone(), bytes, host).map_err(|error| {
                ComposeError::Load {
                    slug: m.slug.clone(),
                    error,
                }
            })?;
            cartridges.insert(m.slug.clone(), cart);
        }
        Ok(Self {
            wiring,
            cartridges,
        })
    }

    pub fn wiring(&self) -> &Wiring {
        &self.wiring
    }

    pub fn slugs(&self) -> impl Iterator<Item = &str> {
        self.cartridges.keys().map(String::as_str)
    }

    pub fn get(&self, slug: &str) -> Option<&Cartridge<H>> {
        self.cartridges.get(slug)
    }

    pub fn get_mut(&mut self, slug: &str) -> Option<&mut Cartridge<H>> {
        self.cartridges.get_mut(slug)
    }

    /// the slug providing `port`, if the wiring has one.
    pub fn provider_of(&self, port: &str) -> Option<&str> {
        self.wiring.providers.get(port).map(String::as_str)
    }

    /// `cart_init` every member in wiring order, each with `config_for(slug)`
    /// under `fuel`. returns the slugs initialized, in order; stops at the
    /// first failure naming its slug (members before it stay initialized —
    /// inspect with `get(slug).is_initialized()`).
    pub fn init_all(
        &mut self,
        config_for: &dyn Fn(&str) -> Vec<u8>,
        fuel: u64,
    ) -> Result<Vec<String>, ComposeError> {
        let mut done = Vec::with_capacity(self.wiring.order.len());
        for slug in self.wiring.order.clone() {
            let cart = self
                .cartridges
                .get_mut(&slug)
                .ok_or_else(|| ComposeError::UnknownCartridge(slug.clone()))?;
            cart.init(&config_for(&slug), fuel)
                .map_err(|error| ComposeError::Call {
                    slug: slug.clone(),
                    error,
                })?;
            done.push(slug);
        }
        Ok(done)
    }

    /// one message to one cartridge, by slug.
    pub fn handle(&mut self, slug: &str, msg: &[u8], fuel: u64) -> Result<Vec<u8>, ComposeError> {
        let cart = self
            .cartridges
            .get_mut(slug)
            .ok_or_else(|| ComposeError::UnknownCartridge(slug.to_string()))?;
        cart.handle(msg, fuel).map_err(|error| ComposeError::Call {
            slug: slug.to_string(),
            error,
        })
    }

    /// one message to whoever provides `port`. THE composition primitive:
    /// callers name a capability, never a module.
    pub fn handle_port(
        &mut self,
        port: &str,
        msg: &[u8],
        fuel: u64,
    ) -> Result<Vec<u8>, ComposeError> {
        let slug = self
            .provider_of(port)
            .ok_or_else(|| ComposeError::NoProvider(port.to_string()))?
            .to_string();
        self.handle(&slug, msg, fuel)
    }
}
