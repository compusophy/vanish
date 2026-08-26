//! L1 — the vanish cartridge ABI v1 (CARTRIDGE_PLAN §4), as ONE table that
//! every side reads.
//!
//! the compiler checks `extern "C"` declarations and `pub fn` lifecycle
//! exports against it; the runtime resolves imports through it; the
//! lifecycle wires exports by it. one definition is the point: a guest and a
//! host that read the same table cannot disagree about a signature, which
//! removes the bug class where a cartridge validates, loads, and then
//! misreads its own arguments mid-message.
//!
//! conventions (§4): every pointer is a guest linear-memory offset, every
//! string is a (ptr: i32, len: i32) pair, and a (ptr, len) RESULT travels
//! packed in one i64 as `ptr << 32 | len` so it fits a single return value.
//! there is deliberately NO sleep/deadline import (D1) and no direct
//! cartridge-to-cartridge call — composition goes through the orchestrator
//! (L4/L5), never around it.

use super::rustlite::Ty;

/// the wasm import module every host function lives in.
pub const IMPORT_MODULE: &str = "vanish";
/// the export name of the guest's linear memory. the host reads message
/// buffers and results through it, so a cartridge without it cannot speak.
pub const MEMORY_EXPORT: &str = "memory";
pub const PAGE_BYTES: usize = 65_536;
/// linear memory every rustlite cartridge gets: 16 pages = 1 MiB. FIXED —
/// the dialect has no memory.grow, so a cartridge's footprint is known at
/// load time and a runaway allocator traps on bounds instead of growing.
pub const GUEST_MEMORY_PAGES: u32 = 16;
/// the largest memory the runtime will instantiate for ANY module (256
/// pages = 16 MiB). a hostile module declaring 65536 pages is refused at
/// decode, not allocated.
pub const MAX_MEMORY_PAGES: u32 = 256;

/// host functions the guest may import from module `vanish`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostFn {
    /// `log(level: i32, ptr: i32, len: i32)` — structured feed output.
    Log,
    /// `now_ms() -> i64` — the ONLY time source.
    NowMs,
    /// `store_get(k_ptr, k_len) -> i64` — packed (ptr, len) of the value
    /// written into guest memory via cart_alloc; 0 = miss.
    StoreGet,
    /// `store_set(k_ptr, k_len, v_ptr, v_len) -> i32` — 0 ok, 1 refused.
    StoreSet,
    /// `emit(topic_ptr, topic_len, ptr, len)` — publish to the orchestrator
    /// bus. a host that cannot deliver TRAPS the guest (D4): there is no
    /// status channel to lose the failure in.
    Emit,
    /// ABI v2. `call(port_ptr, port_len, msg_ptr, msg_len) -> i64` — a
    /// SYNCHRONOUS request to whoever provides `port`, mediated by the
    /// orchestrator, charged to the caller's fuel. the answer is written
    /// into the caller's memory via cart_alloc and returned packed; 0 means
    /// the call did not happen (undeclared port, no provider, callee not
    /// up or busy, callee trapped) — every such case is an orchestrator
    /// event, never a silent nothing.
    Call,
}

impl HostFn {
    pub const ALL: [HostFn; 6] = [
        HostFn::Log,
        HostFn::NowMs,
        HostFn::StoreGet,
        HostFn::StoreSet,
        HostFn::Emit,
        HostFn::Call,
    ];

    pub fn name(self) -> &'static str {
        match self {
            HostFn::Log => "log",
            HostFn::NowMs => "now_ms",
            HostFn::StoreGet => "store_get",
            HostFn::StoreSet => "store_set",
            HostFn::Emit => "emit",
            HostFn::Call => "call",
        }
    }

    /// the ABI version that introduced this function. a manifest declaring
    /// an older version cannot import it — the door check that keeps a v1
    /// cartridge honest about what it was built against.
    pub fn since(self) -> u32 {
        match self {
            HostFn::Call => 2,
            _ => 1,
        }
    }

    pub fn from_name(name: &str) -> Option<HostFn> {
        HostFn::ALL.iter().copied().find(|h| h.name() == name)
    }

    /// (params, result) in rustlite types — the compiler's view.
    pub fn signature(self) -> (&'static [Ty], Option<Ty>) {
        match self {
            HostFn::Log => (&[Ty::I32, Ty::I32, Ty::I32], None),
            HostFn::NowMs => (&[], Some(Ty::I64)),
            HostFn::StoreGet => (&[Ty::I32, Ty::I32], Some(Ty::I64)),
            HostFn::StoreSet => (&[Ty::I32, Ty::I32, Ty::I32, Ty::I32], Some(Ty::I32)),
            HostFn::Emit => (&[Ty::I32, Ty::I32, Ty::I32, Ty::I32], None),
            HostFn::Call => (&[Ty::I32, Ty::I32, Ty::I32, Ty::I32], Some(Ty::I64)),
        }
    }

    /// (params, results) as wasm valtype bytes — the runtime's view.
    pub fn valtypes(self) -> (Vec<u8>, Vec<u8>) {
        let (p, r) = self.signature();
        valtypes_of(p, r)
    }

    pub fn names() -> String {
        HostFn::ALL
            .iter()
            .map(|h| h.name())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// the exports the host calls: the cartridge lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestFn {
    /// `cart_init(config_ptr, config_len) -> i32` — once at load; nonzero
    /// refuses the configuration and the cartridge never handles a message.
    CartInit,
    /// `cart_handle(msg_ptr, msg_len) -> i64` — the workhorse. one message
    /// in, one packed (ptr, len) response out.
    CartHandle,
    /// `cart_alloc(size) -> i32` — the host asks where to write the next
    /// buffer. the guest owns its own allocator; the runtime only checks
    /// that the answer fits in memory.
    CartAlloc,
}

impl GuestFn {
    pub const ALL: [GuestFn; 3] = [GuestFn::CartInit, GuestFn::CartHandle, GuestFn::CartAlloc];

    pub fn name(self) -> &'static str {
        match self {
            GuestFn::CartInit => "cart_init",
            GuestFn::CartHandle => "cart_handle",
            GuestFn::CartAlloc => "cart_alloc",
        }
    }

    pub fn from_name(name: &str) -> Option<GuestFn> {
        GuestFn::ALL.iter().copied().find(|g| g.name() == name)
    }

    pub fn signature(self) -> (&'static [Ty], Option<Ty>) {
        match self {
            GuestFn::CartInit => (&[Ty::I32, Ty::I32], Some(Ty::I32)),
            GuestFn::CartHandle => (&[Ty::I32, Ty::I32], Some(Ty::I64)),
            GuestFn::CartAlloc => (&[Ty::I32], Some(Ty::I32)),
        }
    }

    pub fn valtypes(self) -> (Vec<u8>, Vec<u8>) {
        let (p, r) = self.signature();
        valtypes_of(p, r)
    }
}

fn valtypes_of(params: &[Ty], ret: Option<Ty>) -> (Vec<u8>, Vec<u8>) {
    (
        params.iter().map(|t| t.valtype()).collect(),
        ret.map(|t| t.valtype()).into_iter().collect(),
    )
}

/// `fn(i32, i32) -> i64` — the shape of a signature, for refusal messages
/// that name the fix instead of just the failure.
pub fn describe(params: &[Ty], ret: Option<Ty>) -> String {
    let ps = params
        .iter()
        .map(|t| t.name())
        .collect::<Vec<_>>()
        .join(", ");
    match ret {
        Some(r) => format!("fn({ps}) -> {}", r.name()),
        None => format!("fn({ps})"),
    }
}

/// the packed (ptr, len) result representation: `ptr << 32 | len`.
pub fn pack(ptr: u32, len: u32) -> i64 {
    (((ptr as u64) << 32) | len as u64) as i64
}

pub fn unpack(v: i64) -> (u32, u32) {
    let u = v as u64;
    ((u >> 32) as u32, u as u32)
}

/// what the host provides to a running cartridge. native tests inject a
/// recording fake; the browser wires the feed, the clock, the opfs kv
/// namespace, and the orchestrator bus. every method takes bytes, never
/// pointers — the runtime has already bounds-checked and copied them out
/// of guest memory, so a Host implementation cannot be tricked into
/// reading outside the sandbox.
pub trait Host {
    fn log(&mut self, level: i32, msg: &[u8]);
    fn now_ms(&mut self) -> i64;
    /// Ok(None) = miss. Err = the store itself failed (surfaced as a trap).
    fn store_get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, String>;
    /// Err = refused; the guest sees status 1 and decides what to do.
    fn store_set(&mut self, key: &[u8], value: &[u8]) -> Result<(), String>;
    /// Err = undeliverable; the guest traps with the reason.
    fn emit(&mut self, topic: &[u8], payload: &[u8]) -> Result<(), String>;
    /// ABI v2: a synchronous request to the provider of `port`, run under
    /// the CALLER's remaining `fuel`. Ok(Some(bytes)) = the answer;
    /// Ok(None) = the call did not happen (the host has already recorded
    /// why) and the guest sees 0; Err = the host itself cannot route at all
    /// (the guest traps). a host that mediates nothing answers Ok(None).
    fn call(&mut self, port: &[u8], msg: &[u8], fuel: &mut u64) -> Result<Option<Vec<u8>>, String>;
}
