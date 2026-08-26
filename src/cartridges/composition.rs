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
use super::lifecycle::{CallError, Cartridge, LoadError, Verified};
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
    /// a replacement image carried a different slug than the member it
    /// was meant to replace — renaming is not a swap.
    SlugMismatch { expected: String, found: String },
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
            ComposeError::SlugMismatch { expected, found } => write!(
                f,
                "replacement is named '{found}' but the member being replaced is '{expected}'"
            ),
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
        host_for: impl FnMut(&CartridgeManifest) -> H,
    ) -> Result<Self, ComposeError> {
        let manifests: Vec<CartridgeManifest> = entries.iter().map(|(m, _)| m.clone()).collect();
        // wire before a single byte is decoded.
        wire(&manifests)?;
        let mut images = Vec::with_capacity(entries.len());
        for (m, bytes) in entries {
            images.push(Verified::verify(m.clone(), bytes).map_err(|error| {
                ComposeError::Load {
                    slug: m.slug.clone(),
                    error,
                }
            })?);
        }
        Self::from_verified(images, host_for)
    }

    /// compose from already-verified images (a supervisor keeps these so a
    /// restart never re-decodes). wires first, exactly like `load`.
    pub fn from_verified(
        images: Vec<Verified>,
        mut host_for: impl FnMut(&CartridgeManifest) -> H,
    ) -> Result<Self, ComposeError> {
        let manifests: Vec<CartridgeManifest> =
            images.iter().map(|v| v.manifest().clone()).collect();
        let wiring = wire(&manifests)?;
        let mut cartridges = BTreeMap::new();
        for image in images {
            let host = host_for(image.manifest());
            let slug = image.manifest().slug.clone();
            cartridges.insert(slug, image.instantiate(host));
        }
        Ok(Self {
            wiring,
            cartridges,
        })
    }

    pub fn wiring(&self) -> &Wiring {
        &self.wiring
    }

    /// every member's manifest, in slug order.
    pub fn manifests(&self) -> Vec<CartridgeManifest> {
        self.cartridges
            .values()
            .map(|c| c.manifest().clone())
            .collect()
    }

    /// re-instantiate one member from an image — fresh memory, SAME host.
    /// state lives in kv, not linear memory: that is what makes a restart
    /// safe. the member comes back uninitialized; the caller re-inits it.
    pub fn reinstantiate(&mut self, slug: &str, image: Verified) -> Result<(), ComposeError> {
        if image.manifest().slug != slug {
            return Err(ComposeError::SlugMismatch {
                expected: slug.to_string(),
                found: image.manifest().slug.clone(),
            });
        }
        let old = self
            .cartridges
            .remove(slug)
            .ok_or_else(|| ComposeError::UnknownCartridge(slug.to_string()))?;
        let host = old.into_host();
        self.cartridges
            .insert(slug.to_string(), image.instantiate(host));
        Ok(())
    }

    /// hot-swap one member ATOMICALLY: the new wiring is computed first (a
    /// port change that breaks the set is refused with nothing touched),
    /// the image was verified by the caller before it got here, and only
    /// then does the old member's host move into the new instance.
    /// `rehost` may wrap or inspect the host on its way across. the new
    /// member comes back uninitialized; the caller re-inits it.
    pub fn swap(
        &mut self,
        image: Verified,
        rehost: impl FnOnce(H) -> H,
    ) -> Result<(), ComposeError> {
        let slug = image.manifest().slug.clone();
        if !self.cartridges.contains_key(&slug) {
            return Err(ComposeError::UnknownCartridge(slug));
        }
        let manifests: Vec<CartridgeManifest> = self
            .cartridges
            .values()
            .map(|c| {
                if c.manifest().slug == slug {
                    image.manifest().clone()
                } else {
                    c.manifest().clone()
                }
            })
            .collect();
        let wiring = wire(&manifests)?;
        let old = self
            .cartridges
            .remove(&slug)
            .ok_or_else(|| ComposeError::UnknownCartridge(slug.clone()))?;
        let host = rehost(old.into_host());
        self.cartridges.insert(slug, image.instantiate(host));
        self.wiring = wiring;
        Ok(())
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

    /// remove a member for the duration of a guest run, so the rest of the
    /// composition stays borrowable while that guest executes (a
    /// synchronous `call` from inside it needs exactly that). None when
    /// the slug is unknown OR already taken — a re-entrant request for a
    /// busy member is refused, not deadlocked. `put_back` restores it.
    pub fn take(&mut self, slug: &str) -> Option<Cartridge<H>> {
        self.cartridges.remove(slug)
    }

    pub fn put_back(&mut self, slug: String, cart: Cartridge<H>) {
        self.cartridges.insert(slug, cart);
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
