//! the cartridge substrate (docs/CARTRIDGE_PLAN.md).
//!
//! L1 manifest — this module. L2 rustlite compiler, L3 runtime, L4
//! composition, and L5 orchestration land here as their layers arrive;
//! each is independently verifiable before the next exists.

pub mod manifest;
pub mod rustlite;

pub use manifest::{CartridgeKind, CartridgeManifest, ABI_VERSION};
