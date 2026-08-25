//! L1 — the cartridge manifest: identity, kind, ABI version, ports.
//!
//! a cartridge is a wasm module plus this metadata. everything here is pure:
//! parsing and validation are deterministic functions over bytes, so every
//! decision is testable without a runtime (charter article ii). the runtime
//! (CARTRIDGE_PLAN §6) refuses to LOAD anything that fails `validate` —
//! loudly, naming the reason (D4). a malformed cartridge must fail at the
//! door, not mid-message.

use serde::{Deserialize, Serialize};

/// the only ABI this build understands. a manifest claiming a NEWER major
/// version was built against host functions we do not have; loading it
/// would trap unpredictably mid-run, so it is refused at the door instead.
pub const ABI_VERSION: u32 = 1;

/// what kind of computation this module performs. mirrors tempo-x402's
/// proven taxonomy (its cognitive kind = hot-swappable brain modules);
/// vanish starts with backend + cognitive because those are the consumers
/// that exist today — interactive/frontend arrive with their runtimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CartridgeKind {
    /// request/response computation (tools, transforms, utilities).
    Backend,
    /// a swappable reasoning/decision module routed by the orchestrator.
    Cognitive,
}

impl CartridgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CartridgeKind::Backend => "backend",
            CartridgeKind::Cognitive => "cognitive",
        }
    }
}

/// one declared capability. `provides` names what this module makes
/// available to others; `requires` names what it needs composed in.
/// wiring matches these strings exactly — no globs, no prefixes, because a
/// fuzzy port match is a silent mis-wiring (the D4 class).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Port {
    pub name: String,
}

/// metadata for one cartridge. kept deliberately small: everything else
/// (hashes, prices, owners) belongs to the registry layer, not identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CartridgeManifest {
    /// unique id, `[a-z0-9-]`, 1–64 chars. becomes the kv namespace and the
    /// orchestrator's routing key, so it must be filesystem- and log-safe.
    pub slug: String,
    pub kind: CartridgeKind,
    #[serde(default = "default_version")]
    pub version: String,
    /// which ABI the module was compiled against.
    pub abi_version: u32,
    #[serde(default)]
    pub provides: Vec<Port>,
    #[serde(default)]
    pub requires: Vec<Port>,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

impl CartridgeManifest {
    /// parse + validate from json bytes. the single entry point loaders use.
    pub fn parse(json: &str) -> Result<Self, String> {
        let m: Self = serde_json::from_str(json).map_err(|e| format!("bad manifest json: {e}"))?;
        m.validate()?;
        Ok(m)
    }

    /// all refusals in one place, each naming the fix (D9 for errors).
    pub fn validate(&self) -> Result<(), String> {
        validate_slug(&self.slug)?;
        if self.version.trim().is_empty() {
            return Err("manifest.version must not be empty".to_string());
        }
        if self.abi_version > ABI_VERSION {
            return Err(format!(
                "manifest.abi_version {} is newer than this runtime's {} — rebuild \
                 the cartridge against the current ABI (older majors still load)",
                self.abi_version, ABI_VERSION
            ));
        }
        // duplicate provided ports inside ONE module are ambiguous wiring.
        let mut seen_provides = std::collections::BTreeSet::new();
        for p in &self.provides {
            if p.name.trim().is_empty() {
                return Err("a provided port has an empty name".to_string());
            }
            if !seen_provides.insert(&p.name) {
                return Err(format!("port '{}' is provided twice", p.name));
            }
        }
        // a module cannot require what it itself provides — that is a cycle
        // of length one and would deadlock the composer at wire time.
        for r in &self.requires {
            if r.name.trim().is_empty() {
                return Err("a required port has an empty name".to_string());
            }
            if self.provides.iter().any(|p| p.name == r.name) {
                return Err(format!(
                    "port '{}' is both provided and required by '{}' — split it \
                     into two modules",
                    r.name, self.slug
                ));
            }
        }
        Ok(())
    }
}

/// slug rules: lowercase alphanumerics and hyphens, start/end alphanumeric,
/// 1–64 chars. pure so tests pin the exact boundary.
pub fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.ends_with(|c: char| c.is_ascii_alphanumeric())
        && !s.contains("--")
}

fn validate_slug(s: &str) -> Result<(), String> {
    if valid_slug(s) {
        Ok(())
    } else {
        Err(format!(
            "manifest.slug '{s}' is invalid: use 1–64 chars of [a-z0-9-], \
             starting and ending alphanumeric"
        ))
    }
}
