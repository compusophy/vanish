//! L1 wired over L3: the cartridge lifecycle (CARTRIDGE_PLAN §11 item 5).
//!
//! `Cartridge::load` is the door: manifest validated, module decoded, every
//! import resolved against the ABI table WITH its signature, every
//! lifecycle export present with its signature, memory exported. a
//! cartridge that passes load cannot fail on a missing host function or a
//! mis-shaped entry point later — it can only trap on its own behavior
//! (fuel, bounds, host refusals), and every trap is named.
//!
//! message flow (§4): the host asks the guest for a buffer (`cart_alloc`),
//! writes the message into guest memory, calls `cart_handle(ptr, len)`,
//! and reads the packed (ptr, len) answer back out of the same memory.
//! the guest never sees host memory; the host never trusts a guest pointer
//! without bounds-checking it against the memory it instantiated.

use super::abi::{self, GuestFn, Host, HostFn};
use super::manifest::CartridgeManifest;
use super::runtime::{self, decode, Module, Trap, Val};

/// why a cartridge was refused at the door. the message names the fix.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadError(pub String);

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cartridge refused: {}", self.0)
    }
}

/// why a lifecycle call did not produce an answer.
#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    /// the guest trapped: fuel, bounds, a host failure, … — see `Trap`.
    Trap(Trap),
    /// `cart_init` returned a nonzero status: the cartridge refused its
    /// configuration. the code is the guest's own; the orchestrator
    /// surfaces it verbatim (D4) rather than interpreting it.
    Refused(i32),
    /// the guest answered with something the host cannot honor — a packed
    /// (ptr, len) outside its memory, or the wrong value shape.
    BadResponse(String),
    /// `handle` before a successful `init`. the ABI promises cart_init
    /// runs first; a host that skipped it would hand the guest a message
    /// against uninitialized state.
    NotInitialized,
}

impl From<Trap> for CallError {
    fn from(t: Trap) -> Self {
        CallError::Trap(t)
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Trap(t) => write!(f, "cartridge trapped: {t}"),
            CallError::Refused(code) => write!(f, "cart_init refused the configuration (status {code})"),
            CallError::BadResponse(m) => write!(f, "cartridge answered badly: {m}"),
            CallError::NotInitialized => write!(f, "cart_handle before cart_init"),
        }
    }
}

/// one loaded cartridge: its module, its own linear memory, and the host it
/// talks to. generic over the host so native tests inject a recording fake
/// and the browser injects the real feed/clock/kv/bus with no dynamic
/// dispatch at the type level (the runtime still sees `&mut dyn Host`).
pub struct Cartridge<H: Host> {
    manifest: CartridgeManifest,
    module: Module,
    memory: Vec<u8>,
    host: H,
    /// DEFINED-function indices (into module.funcs) of the lifecycle exports.
    init_idx: usize,
    handle_idx: usize,
    initialized: bool,
}

impl<H: Host> std::fmt::Debug for Cartridge<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cartridge")
            .field("slug", &self.manifest.slug)
            .field("memory_len", &self.memory.len())
            .field("initialized", &self.initialized)
            .finish_non_exhaustive()
    }
}

/// a cartridge that has passed every door but has no host yet: manifest
/// validated, module decoded, imports/exports/memory verified. Clone, so a
/// supervisor can re-instantiate a crashed actor from the same image
/// without decoding a byte — and so a hot-swap can verify the replacement
/// BEFORE the old instance's host moves across.
#[derive(Debug, Clone)]
pub struct Verified {
    manifest: CartridgeManifest,
    module: Module,
    init_idx: usize,
    handle_idx: usize,
}

impl Verified {
    pub fn manifest(&self) -> &CartridgeManifest {
        &self.manifest
    }

    /// give the image a host and a fresh zeroed memory. the result is
    /// uninitialized: `init` runs the guest's cart_init.
    pub fn instantiate<H: Host>(self, host: H) -> Cartridge<H> {
        let memory = self.module.initial_memory();
        Cartridge {
            manifest: self.manifest,
            module: self.module,
            memory,
            host,
            init_idx: self.init_idx,
            handle_idx: self.handle_idx,
            initialized: false,
        }
    }

    /// validate everything that can be validated without running a single
    /// instruction. refusals name the missing piece and its expected shape.
    pub fn verify(manifest: CartridgeManifest, bytes: &[u8]) -> Result<Self, LoadError> {
        manifest
            .validate()
            .map_err(|e| LoadError(format!("manifest: {e}")))?;
        let module = decode(bytes).map_err(|e| {
            LoadError(format!(
                "module does not decode (byte {}): {}",
                e.offset, e.msg
            ))
        })?;

        // imports: only the ABI module, only its functions, only their shapes.
        for imp in &module.imports {
            if imp.module != abi::IMPORT_MODULE {
                return Err(LoadError(format!(
                    "import {}.{}: the only import module is '{}'",
                    imp.module,
                    imp.name,
                    abi::IMPORT_MODULE
                )));
            }
            let Some(hf) = imp.host_fn else {
                return Err(LoadError(format!(
                    "import '{}' is not in the vanish ABI v1 — the host provides: {}",
                    imp.name,
                    HostFn::names()
                )));
            };
            if hf.since() > manifest.abi_version {
                return Err(LoadError(format!(
                    "import '{}' exists since ABI v{}, but the manifest declares abi_version {} \
                     — bump it (and mean it: the cartridge now depends on v{} semantics)",
                    imp.name,
                    hf.since(),
                    manifest.abi_version,
                    hf.since()
                )));
            }
            let ft = &module.types[imp.type_idx as usize]; // decode validated the index
            let (want_p, want_r) = hf.valtypes();
            if ft.params != want_p || ft.results != want_r {
                let (p, r) = hf.signature();
                return Err(LoadError(format!(
                    "import '{}' has a signature the ABI does not define — it is {}",
                    imp.name,
                    abi::describe(p, r)
                )));
            }
        }

        // exports: every lifecycle entry point, defined here, ABI-shaped.
        let lifecycle = |g: GuestFn| -> Result<usize, LoadError> {
            let (p, r) = g.signature();
            let Some(def) = module.defined_export(g.name()) else {
                return Err(LoadError(format!(
                    "the module does not export '{}' — a cartridge must define `pub fn {}` as {}",
                    g.name(),
                    g.name(),
                    abi::describe(p, r)
                )));
            };
            let ft = &module.types[module.funcs[def].type_idx as usize];
            let (want_p, want_r) = g.valtypes();
            if ft.params != want_p || ft.results != want_r {
                return Err(LoadError(format!(
                    "export '{}' has the wrong signature — the ABI defines it as {}",
                    g.name(),
                    abi::describe(p, r)
                )));
            }
            Ok(def)
        };
        let init_idx = lifecycle(GuestFn::CartInit)?;
        let handle_idx = lifecycle(GuestFn::CartHandle)?;
        lifecycle(GuestFn::CartAlloc)?;

        if !module.exports_memory() {
            return Err(LoadError(format!(
                "the module exports no '{}' — the host cannot hand it a message",
                abi::MEMORY_EXPORT
            )));
        }

        Ok(Verified {
            manifest,
            module,
            init_idx,
            handle_idx,
        })
    }
}

impl<H: Host> Cartridge<H> {
    /// verify, then instantiate — one call for callers that hold the host
    /// already. the two halves exist separately for supervisors.
    pub fn load(manifest: CartridgeManifest, bytes: &[u8], host: H) -> Result<Self, LoadError> {
        Ok(Verified::verify(manifest, bytes)?.instantiate(host))
    }

    pub fn manifest(&self) -> &CartridgeManifest {
        &self.manifest
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    pub fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    pub fn into_host(self) -> H {
        self.host
    }

    pub fn memory_len(&self) -> usize {
        self.memory.len()
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// write `bytes` into guest memory through the guest's own allocator.
    /// returns the (ptr, len) pair the ABI passes to the entry point.
    fn hand_over(&mut self, bytes: &[u8], fuel: &mut u64) -> Result<(i32, i32), CallError> {
        let host: &mut dyn Host = &mut self.host;
        let ptr = runtime::guest_alloc(
            &self.module,
            &mut self.memory,
            Some(host),
            fuel,
            0,
            bytes.len(),
        )?;
        self.memory[ptr..ptr + bytes.len()].copy_from_slice(bytes);
        // guest_alloc proved ptr + len fits the memory, which is capped far
        // below i32::MAX, so both casts are exact.
        Ok((ptr as i32, bytes.len() as i32))
    }

    /// `cart_init(config)` under `fuel`. status 0 arms `handle`; any other
    /// status is the guest refusing, surfaced verbatim.
    pub fn init(&mut self, config: &[u8], fuel: u64) -> Result<(), CallError> {
        let mut fuel = fuel;
        let (ptr, len) = self.hand_over(config, &mut fuel)?;
        let host: &mut dyn Host = &mut self.host;
        let out = runtime::invoke_hosted(
            &self.module,
            &mut self.memory,
            host,
            self.init_idx,
            &[Val::I32(ptr), Val::I32(len)],
            &mut fuel,
        )?;
        match out {
            Some(Val::I32(0)) => {
                self.initialized = true;
                Ok(())
            }
            Some(Val::I32(code)) => Err(CallError::Refused(code)),
            other => Err(CallError::BadResponse(format!(
                "cart_init returned {other:?}, expected an i32 status"
            ))),
        }
    }

    /// `cart_handle(msg)` under `fuel`: one message in, one response out.
    /// the response bytes are COPIED out of guest memory before returning,
    /// so the caller never holds a view into the sandbox.
    pub fn handle(&mut self, msg: &[u8], fuel: u64) -> Result<Vec<u8>, CallError> {
        let mut fuel = fuel;
        self.handle_with(msg, &mut fuel)
    }

    /// `handle` on a SHARED fuel counter — what remains is left in `fuel`.
    /// a synchronous `call` runs the callee this way, so the caller pays
    /// for the work it asked for.
    pub fn handle_with(&mut self, msg: &[u8], fuel: &mut u64) -> Result<Vec<u8>, CallError> {
        if !self.initialized {
            return Err(CallError::NotInitialized);
        }
        let (ptr, len) = self.hand_over(msg, fuel)?;
        let host: &mut dyn Host = &mut self.host;
        let out = runtime::invoke_hosted(
            &self.module,
            &mut self.memory,
            host,
            self.handle_idx,
            &[Val::I32(ptr), Val::I32(len)],
            fuel,
        )?;
        let packed = match out {
            Some(Val::I64(v)) => v,
            other => {
                return Err(CallError::BadResponse(format!(
                    "cart_handle returned {other:?}, expected a packed i64"
                )))
            }
        };
        let (rptr, rlen) = abi::unpack(packed);
        let start = rptr as usize;
        let end = start
            .checked_add(rlen as usize)
            .filter(|&end| end <= self.memory.len())
            .ok_or_else(|| {
                CallError::BadResponse(format!(
                    "cart_handle answered with {rlen} byte(s) at {rptr}, outside the {}-byte memory",
                    self.memory.len()
                ))
            })?;
        Ok(self.memory[start..end].to_vec())
    }
}
