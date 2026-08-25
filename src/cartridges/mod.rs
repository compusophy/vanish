//! the cartridge substrate (docs/CARTRIDGE_PLAN.md).
//!
//! L1 manifest + abi — identity and the host surface. L2 rustlite compiler
//! (front-end + wasm emission). L3 runtime — the fuel-bounded interpreter.
//! lifecycle — L1 wired over L3: load, init, handle. L4 composition and L5
//! orchestration land here as their layers arrive; each is independently
//! verifiable before the next exists.

pub mod abi;
pub mod lifecycle;
pub mod manifest;
pub mod runtime;
pub mod rustlite;
pub mod wasm;

pub use abi::{pack, unpack, GuestFn, Host, HostFn};
pub use lifecycle::{CallError, Cartridge, LoadError};
pub use manifest::{CartridgeKind, CartridgeManifest, ABI_VERSION};
