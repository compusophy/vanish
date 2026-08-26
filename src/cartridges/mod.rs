//! the cartridge substrate (docs/CARTRIDGE_PLAN.md).
//!
//! L1 manifest + abi — identity and the host surface. L2 rustlite compiler
//! (front-end + wasm emission). L3 runtime — the fuel-bounded interpreter.
//! lifecycle — L1 wired over L3: load, init, handle. L4 ports + composition
//! — a SET of cartridges wired by what they provide and require, addressed
//! by port. L5 orchestrator — actors with mailboxes, supervision, hot-swap.
//! each layer was independently verifiable before the next existed.

pub mod abi;
pub mod composition;
pub mod lifecycle;
pub mod manifest;
pub mod orchestrator;
pub mod ports;
pub mod runtime;
pub mod rustlite;
pub mod wasm;

pub use abi::{pack, unpack, GuestFn, Host, HostFn};
pub use composition::{ComposeError, Composition};
pub use lifecycle::{CallError, Cartridge, LoadError, Verified};
pub use manifest::{CartridgeKind, CartridgeManifest, ABI_VERSION};
pub use orchestrator::{
    ActorHost, Envelope, Event, Health, Orchestrator, MAX_CALL_CHAIN, MAX_MAILBOX, MAX_RESTARTS,
    RESTART_BASE_MS,
};
pub use ports::{wire, Edge, WireError, Wiring};
