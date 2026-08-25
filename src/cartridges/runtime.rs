//! L3 — the cartridge runtime: a fuel-bounded interpreter over EXACTLY the
//! wasm dialect `wasm.rs` emits (CARTRIDGE_PLAN §6 option 2).
//!
//! scope discipline: we interpret OUR frozen emission set — no simd, no
//! threads, no reference types, no tables, no globals, no data segments, no
//! memory growth. ONE fixed linear memory, function imports from the L1 ABI
//! module only, and named exports — exactly what a cartridge needs to
//! receive a message and answer it. that is deliberate: a hostile cartridge
//! can only be as expressive as rustlite, so the interpreter stays small
//! enough to audit. if third-party full-rustc cartridges ever need hosting,
//! WAMR swaps in behind the same interface.
//!
//! host boundary: every import call pops its arguments, bounds-checks and
//! COPIES the guest bytes it names, and only then calls the `Host` trait —
//! a Host implementation never sees a pointer. `store_get` writes its value
//! back through the guest's own `cart_alloc`, a bounded re-entry (the guest
//! runs under the same fuel, at most MAX_HOST_REENTRY deep).
//!
//! safety posture: this is the component that will run UNTRUSTED code beside
//! the agent loop (§9: a hostile opcode stream is just a trapped cartridge).
//! therefore every decode failure is a named DecodeError, every runtime
//! anomaly is a named Trap, and NOTHING panics — pinned by fuzz tests that
//! feed every truncation and single-byte corruption of a valid module
//! through decode+invoke and assert no panic escapes.
//!
//! fuel model: one unit per instruction, charged BEFORE dispatch. a
//! cartridge that burns its budget stops mid-stream and reports
//! FuelExhausted honestly (D4) — it cannot wedge the agent loop.
//!
//! call discipline: each activation owns a Frame (its own locals, its own
//! ip, its stack_base). the CALLER's ip already sits past its Call
//! instruction when the callee runs, so returning is: pop the frame,
//  truncate the stack to the frame's base, push the result. no resume
//! bookkeeping exists anywhere — the simplest correct design.

use super::abi::{self, GuestFn, Host, HostFn};

// ---- values ------------------------------------------------------------------

/// a runtime value. bools do not exist here: they are i32 at the wire level,
/// same as in emission. the distinction lives in the type checker only.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Val {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
}

impl Val {
    /// the zero value for a valtype byte (0x7f..0x7c), mirroring the spec's
    /// default-value rule for non-param locals.
    fn zero(valty: u8) -> Option<Val> {
        match valty {
            0x7f => Some(Val::I32(0)),
            0x7e => Some(Val::I64(0)),
            0x7d => Some(Val::F32(0.0)),
            0x7c => Some(Val::F64(0.0)),
            _ => None,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Val::I32(_) => "i32",
            Val::I64(_) => "i64",
            Val::F32(_) => "f32",
            Val::F64(_) => "f64",
        }
    }
}

// ---- traps -------------------------------------------------------------------

/// why an invocation stopped early. every variant names its cause — a
/// trapped cartridge reports WHY, never silently (D4).
#[derive(Debug, Clone, PartialEq)]
pub enum Trap {
    /// the invocation's fuel budget ran out mid-stream.
    FuelExhausted,
    /// `unreachable` executed. the emitter places one after an if/else
    /// that returns on both arms (so the validator accepts the function's
    /// end); executing it means control fell off a path the checker
    /// believed always returned.
    Unreachable,
    /// integer division or remainder by zero.
    DivideByZero,
    /// signed division of MIN by -1 (result unrepresentable). remainder
    /// does NOT trap here: the spec defines MIN % -1 = 0.
    IntegerOverflow,
    /// control structure malformed — unreachable from our emitter,
    /// possible from hand-built bytes.
    BadControl(String),
    /// the value stack did not hold what an instruction needed.
    InvalidStack(String),
    /// call arguments did not match the callee's signature.
    BadArguments(String),
    /// a load/store named bytes outside the linear memory.
    MemoryOutOfBounds(String),
    /// the call stack passed MAX_CALL_DEPTH — unbounded recursion is a
    /// named trap, never a native stack overflow.
    CallDepthExceeded(usize),
    /// host-import failures: no host attached, an import outside the ABI,
    /// a host that could not deliver, or a guest allocator that answered
    /// with a pointer outside its own memory.
    HostError(String),
}

impl std::fmt::Display for Trap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trap::FuelExhausted => write!(f, "fuel exhausted — the cartridge ran past its budget"),
            Trap::Unreachable => write!(f, "unreachable executed — control fell off a path that should have returned"),
            Trap::DivideByZero => write!(f, "integer division by zero"),
            Trap::IntegerOverflow => write!(f, "signed division overflow (MIN / -1)"),
            Trap::BadControl(m) => write!(f, "malformed control flow: {m}"),
            Trap::InvalidStack(m) => write!(f, "invalid stack state: {m}"),
            Trap::BadArguments(m) => write!(f, "argument mismatch: {m}"),
            Trap::MemoryOutOfBounds(m) => write!(f, "memory access out of bounds: {m}"),
            Trap::CallDepthExceeded(n) => {
                write!(f, "call depth exceeded {n} frames — unbounded recursion")
            }
            Trap::HostError(m) => write!(f, "host call failed: {m}"),
        }
    }
}

// ---- decode ------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct DecodeError {
    pub offset: usize,
    pub msg: String,
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn err<T>(&self, msg: impl Into<String>) -> Result<T, DecodeError> {
        Err(DecodeError {
            offset: self.pos,
            msg: msg.into(),
        })
    }

    fn byte(&mut self) -> Result<u8, DecodeError> {
        match self.b.get(self.pos) {
            Some(&v) => {
                self.pos += 1;
                Ok(v)
            }
            None => self.err("unexpected end of module"),
        }
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or_else(|| DecodeError {
            offset: self.pos,
            msg: "length overflow".into(),
        })?;
        match self.b.get(self.pos..end) {
            Some(s) => {
                self.pos = end;
                Ok(s)
            }
            None => self.err("unexpected end of module"),
        }
    }

    fn uleb(&mut self) -> Result<u64, DecodeError> {
        let mut v: u64 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.byte()?;
            v |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift >= 70 {
                return self.err("unterminated LEB128");
            }
        }
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let v = self.uleb()?;
        u32::try_from(v).map_err(|_| DecodeError {
            offset: self.pos,
            msg: format!("index {v} exceeds u32"),
        })
    }

    /// signed LEB128 for constant payloads.
    fn sleb(&mut self) -> Result<i64, DecodeError> {
        let mut v: i64 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.byte()?;
            v |= ((byte & 0x7f) as i64) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                // sign-extend the final fragment
                if shift < 64 && byte & 0x40 != 0 {
                    v |= -1i64 << shift;
                }
                return Ok(v);
            }
            if shift >= 70 {
                return self.err("unterminated signed LEB128");
            }
        }
    }

    fn valtype(&mut self) -> Result<u8, DecodeError> {
        let b = self.byte()?;
        match b {
            0x7c..=0x7f => Ok(b),
            other => self.err(format!("unknown valtype {other:#04x}")),
        }
    }

    /// a wasm `name`: uleb length + utf-8 bytes. capped so a corrupted
    /// length cannot ask for a gigabyte before the bounds check runs.
    fn name(&mut self) -> Result<String, DecodeError> {
        let n = self.uleb()? as usize;
        if n > 1024 {
            return self.err(format!("name of {n} bytes refused"));
        }
        let raw = self.bytes(n)?;
        match std::str::from_utf8(raw) {
            Ok(s) => Ok(s.to_string()),
            Err(_) => self.err("name is not utf-8"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuncType {
    pub params: Vec<u8>,
    pub results: Vec<u8>,
}

/// one arithmetic/comparison operation, resolved to its exact width at
/// decode time so execution never re-inspects types.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    I32Add,
    I32Sub,
    I32Mul,
    I32DivS,
    I32RemS,
    I32LtS,
    I32GtS,
    I32LeS,
    I32GeS,
    I32Eq,
    I32Ne,

    I64Add,
    I64Sub,
    I64Mul,
    I64DivS,
    I64RemS,
    I64LtS,
    I64GtS,
    I64LeS,
    I64GeS,
    I64Eq,
    I64Ne,

    F32Add,
    F32Sub,
    F32Mul,
    F32Div,
    F32Eq,
    F32Ne,
    F32Lt,
    F32Gt,
    F32Le,
    F32Ge,

    F64Add,
    F64Sub,
    F64Mul,
    F64Div,
    F64Eq,
    F64Ne,
    F64Lt,
    F64Gt,
    F64Le,
    F64Ge,

    /// logical and/or on i32 operands (bools at runtime). EAGER, not
    /// short-circuit — mirrors emission; documented in wasm.rs.
    I32And,
    I32Or,

    /// the packed-result ops behind the `pack`/`unpack_*` intrinsics.
    /// shift counts are taken mod 64 per spec.
    I64Shl,
    I64ShrU,
    I64Or,
}

/// a decoded instruction. BRANCH TARGETS ARE ABSOLUTE: resolved at decode
/// time through the control-frame pass below, so runtime branch dispatch is
/// one assignment. this is the difference between a throwaway vm and a
/// runtime — no scanning, no shape assumptions.
#[derive(Debug, Clone, PartialEq)]
pub enum Instr {
    I32Const(i32),
    I64Const(i64),
    F32Const(f32),
    F64Const(f64),
    LocalGet(u32),
    LocalSet(u32),
    Call(u32),
    Drop,
    /// traps on execution; exists so the emitter can close every-path-
    /// returns bodies in a way the validator accepts.
    Unreachable,
    /// unconditional: jump to the resolved target (loop header = restart/
    /// continue; block exit = past the block's end).
    Br(u32),
    /// conditional on a truthy i32; falls through when false.
    BrIf(u32),
    /// i32.eqz — while conditions, and the false-branch test decode
    /// synthesizes for `if` (an if is `eqz; br_if else-or-end`).
    I32Eqz,
    Bin(BinOp),
    /// i64.extend_i32_u / i32.wrap_i64 — the width moves `pack`/`unpack_*`
    /// need. the ONLY conversions in the dialect.
    I64ExtendI32U,
    I32WrapI64,
    /// memory ops, each carrying its static offset (align is a hint the
    /// decoder reads and discards — misaligned access is legal per spec).
    I32Load(u32),
    I32Load8U(u32),
    I32Store(u32),
    I32Store8(u32),
    /// memory.size — pages currently allocated (fixed for our modules).
    MemorySize,
    /// the terminating end of a FUNCTION body. control-frame ends were
    /// consumed at decode time and survive only inside branch targets.
    FunctionEnd,
}

#[derive(Debug, Clone)]
pub struct FuncBody {
    /// valtype bytes for ALL locals (params first), parallel to indices.
    pub locals: Vec<u8>,
    pub code: Vec<Instr>,
    /// index into module.types.
    pub type_idx: u32,
}

/// one function import. `host_fn` is resolved at decode (module == the ABI
/// module AND the name is in the table); calling an unresolved import is a
/// named HostError at runtime, and the lifecycle refuses such a module at
/// load so it never gets that far.
#[derive(Debug, Clone, PartialEq)]
pub struct Import {
    pub module: String,
    pub name: String,
    pub type_idx: u32,
    pub host_fn: Option<HostFn>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    Func,
    Memory,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Export {
    pub name: String,
    pub kind: ExportKind,
    /// function index (imports first, then defined) or memory index.
    pub index: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Module {
    pub types: Vec<FuncType>,
    /// function indices 0..imports.len() — the wasm index space puts
    /// imports before defined functions.
    pub imports: Vec<Import>,
    pub funcs: Vec<FuncBody>,
    /// declared minimum pages of the (single) linear memory, if any. our
    /// modules declare min == max; growth does not exist in the dialect.
    pub memory_pages: Option<u32>,
    pub exports: Vec<Export>,
}

impl Module {
    /// a fresh zeroed memory of the declared size (empty when none is
    /// declared — every load/store then traps out of bounds).
    pub fn initial_memory(&self) -> Vec<u8> {
        vec![0u8; self.memory_pages.unwrap_or(0) as usize * abi::PAGE_BYTES]
    }

    /// the wasm function index exported under `name`, if any.
    pub fn export_func(&self, name: &str) -> Option<u32> {
        self.exports
            .iter()
            .find(|e| e.kind == ExportKind::Func && e.name == name)
            .map(|e| e.index)
    }

    /// the DEFINED-function index (into `funcs`) exported under `name`.
    /// None when the name is not exported OR re-exports an import — a
    /// lifecycle entry point must be the cartridge's own code.
    pub fn defined_export(&self, name: &str) -> Option<usize> {
        let idx = self.export_func(name)? as usize;
        idx.checked_sub(self.imports.len())
    }

    pub fn exports_memory(&self) -> bool {
        self.exports
            .iter()
            .any(|e| e.kind == ExportKind::Memory && e.name == abi::MEMORY_EXPORT)
    }
}

/// decode a module produced by `super::wasm::emit_module` (or equivalent
/// bytes). everything wrong is a named DecodeError; nothing panics.
pub fn decode(bytes: &[u8]) -> Result<Module, DecodeError> {
    let mut r = Reader { b: bytes, pos: 0 };
    if r.bytes(4)? != b"\0asm" {
        return r.err("bad magic — not a wasm module");
    }
    let version = r.bytes(4)?;
    if version != [1, 0, 0, 0] {
        return r.err(format!(
            "unsupported wasm version {version:?} (only MVP version 1)"
        ));
    }

    let mut m = Module::default();
    let mut type_indices: Vec<u32> = Vec::new();

    while r.pos < r.b.len() {
        let id = r.byte()?;
        let size = r.uleb()? as usize;
        let section_end = r.pos.checked_add(size).ok_or_else(|| DecodeError {
            offset: r.pos,
            msg: "section length overflow".into(),
        })?;
        if section_end > r.b.len() {
            return r.err(format!(
                "section {id} declares {size} bytes past module end"
            ));
        }
        match id {
            1 => {
                let n = r.uleb()?;
                for _ in 0..n {
                    let form = r.byte()?;
                    if form != 0x60 {
                        return r.err(format!("type entry {form:#04x} is not a functype"));
                    }
                    let np = r.uleb()? as usize;
                    let mut params = Vec::with_capacity(np.min(1024));
                    for _ in 0..np {
                        params.push(r.valtype()?);
                    }
                    let nr = r.uleb()? as usize;
                    let mut results = Vec::with_capacity(nr.min(1024));
                    for _ in 0..nr {
                        results.push(r.valtype()?);
                    }
                    m.types.push(FuncType { params, results });
                }
            }
            3 => {
                let n = r.uleb()?;
                for _ in 0..n {
                    type_indices.push(r.u32()?);
                }
            }
            10 => {
                let n = r.uleb()?;
                for _ in 0..n {
                    let body_size = r.uleb()? as usize;
                    let body_end =
                        r.pos
                            .checked_add(body_size)
                            .ok_or_else(|| DecodeError {
                                offset: r.pos,
                                msg: "function body length overflow".into(),
                            })?;
                    if body_end > r.b.len() {
                        return r.err("function body extends past module end");
                    }
                    let ndecl = r.uleb()?;
                    let mut locals: Vec<u8> = Vec::new();
                    for _ in 0..ndecl {
                        let count = r.uleb()?;
                        if count > 100_000 {
                            return r.err(format!("local group of {count} refused"));
                        }
                        let vt = r.valtype()?;
                        for _ in 0..count {
                            locals.push(vt);
                        }
                    }
                    let body = decode_code(&mut r)?;
                    if r.pos != body_end {
                        // a body can end SHORT of its declared size too (a
                        // corrupted size byte, or an early `end`); the
                        // lifecycle fuzz found the unsigned subtraction here
                        // — same class as the section-overshoot fix.
                        let (n, dir) = if r.pos > body_end {
                            (r.pos - body_end, "past")
                        } else {
                            (body_end - r.pos, "short of")
                        };
                        return r.err(format!(
                            "function body ended {n} byte(s) {dir} its declared {body_size}"
                        ));
                    }
                    m.funcs.push(FuncBody {
                        locals,
                        code: body,
                        type_idx: 0, // patched below
                    });
                }
            }
            2 => {
                let n = r.uleb()?;
                for _ in 0..n {
                    let module = r.name()?;
                    let name = r.name()?;
                    let kind = r.byte()?;
                    if kind != 0x00 {
                        return r.err(format!(
                            "import {module}.{name} has kind {kind:#04x} — only function \
                             imports exist in the dialect"
                        ));
                    }
                    let type_idx = r.u32()?;
                    let host_fn = if module == abi::IMPORT_MODULE {
                        HostFn::from_name(&name)
                    } else {
                        None
                    };
                    m.imports.push(Import {
                        module,
                        name,
                        type_idx,
                        host_fn,
                    });
                }
            }
            5 => {
                let n = r.uleb()?;
                if n > 1 {
                    return r.err(format!("{n} memories declared — the dialect has at most one"));
                }
                if n == 1 {
                    if m.memory_pages.is_some() {
                        return r.err("memory declared twice");
                    }
                    let flags = r.byte()?;
                    let min = r.u32()?;
                    let max = match flags {
                        0x00 => None,
                        0x01 => Some(r.u32()?),
                        other => {
                            return r.err(format!("memory limits flag {other:#04x} unsupported"))
                        }
                    };
                    if min > abi::MAX_MEMORY_PAGES {
                        return r.err(format!(
                            "memory of {min} pages refused — the runtime caps at {} pages \
                             ({} MiB)",
                            abi::MAX_MEMORY_PAGES,
                            abi::MAX_MEMORY_PAGES / 16
                        ));
                    }
                    if let Some(mx) = max {
                        if mx < min {
                            return r.err(format!("memory max {mx} is below its min {min}"));
                        }
                    }
                    m.memory_pages = Some(min);
                }
            }
            7 => {
                let n = r.uleb()?;
                for _ in 0..n {
                    let name = r.name()?;
                    let kind = match r.byte()? {
                        0x00 => ExportKind::Func,
                        0x02 => ExportKind::Memory,
                        other => {
                            return r.err(format!(
                                "export '{name}' has kind {other:#04x} — only functions and \
                                 memory are exported in the dialect"
                            ))
                        }
                    };
                    let index = r.u32()?;
                    m.exports.push(Export { name, kind, index });
                }
            }
            0 => {
                // custom section (names, producers, …): skipped untouched.
                r.bytes(size)?;
                continue;
            }
            other => {
                // NOT skipped: a data, table, global, start, or element
                // section would change semantics we do not implement, and
                // silently dropping it is exactly the D4 failure — the
                // module would "load" and then behave unlike its bytes.
                return r.err(format!(
                    "section id {other} is outside the dialect — no tables, globals, \
                     start, elements, or data segments"
                ));
            }
        }
        if r.pos != section_end {
            // a decoder arm overran (or the section was skipped short);
            // report the overrun magnitude without underflowing when the
            // fuzz flips bytes inside a size field.
            let overshoot = r.pos.saturating_sub(section_end);
            return r.err(format!(
                "section {id} decoded {overshoot} byte(s) off its declared end"
            ));
        }
    }

    // stitch signatures to bodies. PARAMS ARE NOT LOCALS: our emitter writes
    // only the non-param lets into the local-declaration groups (params live
    // in the signature per spec §C), so execution prepends the signature's
    // param types when materializing a frame. validate that every local
    // INDEX referenced by the code exists in params+declared — after this
    // check, local.get/set bounds errors are genuinely runtime anomalies.
    for (i, imp) in m.imports.iter().enumerate() {
        if imp.type_idx as usize >= m.types.len() {
            return r.err(format!(
                "import {i} ('{}') references type {}, which does not exist",
                imp.name, imp.type_idx
            ));
        }
    }
    if type_indices.len() != m.funcs.len() {
        return r.err(format!(
            "{} function signatures but {} bodies",
            type_indices.len(),
            m.funcs.len()
        ));
    }
    for (i, ti) in type_indices.into_iter().enumerate() {
        let ft = m.types.get(ti as usize).ok_or_else(|| DecodeError {
            offset: 0,
            msg: format!("function {i} references type {ti}, which does not exist"),
        })?;
        let n_total = ft.params.len() + m.funcs[i].locals.len();
        let max_local = max_local_index(&m.funcs[i].code);
        // a function with no local references at all passes trivially — the
        // unwrap_or(0) in max_local_index must not read as "references 0".
        if n_total > 0 && max_local >= n_total as u32 {
            return r.err(format!(
                "function {i} references local {max_local} but has only \
                 {} params + {} declared locals ({n_total} total)",
                ft.params.len(),
                m.funcs[i].locals.len()
            ));
        }
        m.funcs[i].type_idx = ti;
    }

    // exports must name things that exist, exactly once each.
    let n_funcs = m.imports.len() + m.funcs.len();
    let mut names = std::collections::BTreeSet::new();
    for e in &m.exports {
        if !names.insert(&e.name) {
            return r.err(format!("export '{}' appears twice", e.name));
        }
        match e.kind {
            ExportKind::Func => {
                if e.index as usize >= n_funcs {
                    return r.err(format!(
                        "export '{}' names function {} but only {n_funcs} exist",
                        e.name, e.index
                    ));
                }
            }
            ExportKind::Memory => {
                if e.index != 0 || m.memory_pages.is_none() {
                    return r.err(format!(
                        "export '{}' names memory {} but the module declares {}",
                        e.name,
                        e.index,
                        if m.memory_pages.is_some() { "one" } else { "none" }
                    ));
                }
            }
        }
    }
    Ok(m)
}

/// highest local index any instruction references; 0 when none do.
fn max_local_index(code: &[Instr]) -> u32 {
    code.iter()
        .filter_map(|i| match i {
            Instr::LocalGet(n) | Instr::LocalSet(n) => Some(*n),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// decode ONE function body's instruction stream, resolving branch targets.
///
/// control frames carry their unresolved branch sites; when the frame's end
/// arrives, every site registered against it patches — loop headers get the
/// frame's start ip, block exits get the ip past their end. a branch
/// targeting an OUTER frame registers THERE and patches later, which makes
/// nested exits skip every intervening end for free (the bug class the emit
/// tests' mini-vm hit twice, eliminated structurally here).
fn decode_code(r: &mut Reader) -> Result<Vec<Instr>, DecodeError> {
    let mut code: Vec<Instr> = Vec::new();
    #[derive(PartialEq)]
    enum Kind {
        Block,
        Loop,
        If,
    }
    struct Frame {
        kind: Kind,
        start: u32,
        /// indices into `code` of Br/BrIf instructions targeting THIS frame.
        unresolved: Vec<usize>,
        /// an If frame's synthesized false-branch site, pending until its
        /// `else` (→ start of the else arm) or `end` (→ past the frame).
        else_site: Option<usize>,
    }
    let mut frames: Vec<Frame> = Vec::new();

    loop {
        let op = r.byte()?;
        let instr = match op {
            0x41 => Instr::I32Const(r.sleb()? as i32),
            0x42 => Instr::I64Const(r.sleb()?),
            0x43 => {
                let raw = r.bytes(4)?;
                Instr::F32Const(f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
            }
            0x44 => {
                let raw = r.bytes(8)?;
                Instr::F64Const(f64::from_le_bytes([
                    raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
                ]))
            }
            0x20 => Instr::LocalGet(r.u32()?),
            0x21 => Instr::LocalSet(r.u32()?),
            0x10 => Instr::Call(r.u32()?),
            0x1a => Instr::Drop,
            0x00 => Instr::Unreachable,
            0x45 => Instr::I32Eqz,
            0xad => Instr::I64ExtendI32U,
            0xa7 => Instr::I32WrapI64,
            0x28 | 0x2d | 0x36 | 0x3a => {
                let _align = r.uleb()?; // a hint; misaligned access is legal
                let offset = r.u32()?;
                match op {
                    0x28 => Instr::I32Load(offset),
                    0x2d => Instr::I32Load8U(offset),
                    0x36 => Instr::I32Store(offset),
                    _ => Instr::I32Store8(offset),
                }
            }
            0x3f => {
                let idx = r.byte()?;
                if idx != 0 {
                    return r.err("memory.size names a memory other than 0");
                }
                Instr::MemorySize
            }
            0x02 | 0x03 => {
                let bt = r.byte()?; // blocktype: only void exists in our dialect
                if bt != 0x40 {
                    return r.err(format!(
                        "blocktype {bt:#04x} unsupported — the dialect emits void blocks only"
                    ));
                }
                frames.push(Frame {
                    kind: if op == 0x03 { Kind::Loop } else { Kind::Block },
                    start: code.len() as u32,
                    unresolved: Vec::new(),
                    else_site: None,
                });
                continue; // no runtime instruction for openers
            }
            0x04 => {
                // `if` lowers to a false-test branch: eqz; br_if <else|end>.
                // the site is patched when the frame learns where its else
                // arm (or its end) lives — same unresolved-site discipline
                // as every other branch.
                let bt = r.byte()?;
                if bt != 0x40 {
                    return r.err(format!(
                        "blocktype {bt:#04x} unsupported — the dialect emits void ifs only"
                    ));
                }
                code.push(Instr::I32Eqz);
                code.push(Instr::BrIf(0));
                let site = code.len() - 1;
                frames.push(Frame {
                    kind: Kind::If,
                    start: code.len() as u32,
                    unresolved: Vec::new(),
                    else_site: Some(site),
                });
                continue;
            }
            0x05 => {
                let Some(frame) = frames.last_mut() else {
                    return r.err("else outside any if");
                };
                if frame.kind != Kind::If {
                    return r.err("else closes a block or loop, not an if");
                }
                let Some(site) = frame.else_site.take() else {
                    return r.err("a second else for one if");
                };
                // falling out of the then-arm must skip the else-arm: an
                // unconditional exit registered on this frame's end.
                code.push(Instr::Br(0));
                frame.unresolved.push(code.len() - 1);
                let else_start = code.len() as u32;
                code[site] = resolve_branch(code[site].clone(), else_start);
                continue;
            }
            0x0c | 0x0d => {
                let depth = r.uleb()? as usize;
                let Some(frame_idx) = (frames.len()).checked_sub(depth + 1) else {
                    return r.err(format!(
                        "branch depth {depth} exceeds {} open frame(s)",
                        frames.len()
                    ));
                };
                code.push(if op == 0x0c {
                    Instr::Br(0)
                } else {
                    Instr::BrIf(0)
                });
                frames[frame_idx].unresolved.push(code.len() - 1);
                continue;
            }
            0x71 => Instr::Bin(BinOp::I32And),
            0x72 => Instr::Bin(BinOp::I32Or),

            0x6a => Instr::Bin(BinOp::I32Add),
            0x6b => Instr::Bin(BinOp::I32Sub),
            0x6c => Instr::Bin(BinOp::I32Mul),
            0x6d => Instr::Bin(BinOp::I32DivS),
            0x6f => Instr::Bin(BinOp::I32RemS),
            0x48 => Instr::Bin(BinOp::I32LtS),
            0x4a => Instr::Bin(BinOp::I32GtS),
            0x4c => Instr::Bin(BinOp::I32LeS),
            0x4e => Instr::Bin(BinOp::I32GeS),
            0x46 => Instr::Bin(BinOp::I32Eq),
            0x47 => Instr::Bin(BinOp::I32Ne),

            0x7c => Instr::Bin(BinOp::I64Add),
            0x7d => Instr::Bin(BinOp::I64Sub),
            0x7e => Instr::Bin(BinOp::I64Mul),
            0x7f => Instr::Bin(BinOp::I64DivS),
            0x81 => Instr::Bin(BinOp::I64RemS),
            0x84 => Instr::Bin(BinOp::I64Or),
            0x86 => Instr::Bin(BinOp::I64Shl),
            0x88 => Instr::Bin(BinOp::I64ShrU),
            0x53 => Instr::Bin(BinOp::I64LtS),
            0x55 => Instr::Bin(BinOp::I64GtS),
            0x57 => Instr::Bin(BinOp::I64LeS),
            0x59 => Instr::Bin(BinOp::I64GeS),
            0x51 => Instr::Bin(BinOp::I64Eq),
            0x52 => Instr::Bin(BinOp::I64Ne),

            0x92 => Instr::Bin(BinOp::F32Add),
            0x93 => Instr::Bin(BinOp::F32Sub),
            0x94 => Instr::Bin(BinOp::F32Mul),
            0x95 => Instr::Bin(BinOp::F32Div),
            0x5b => Instr::Bin(BinOp::F32Eq),
            0x5c => Instr::Bin(BinOp::F32Ne),
            0x5d => Instr::Bin(BinOp::F32Lt),
            0x5e => Instr::Bin(BinOp::F32Gt),
            0x5f => Instr::Bin(BinOp::F32Le),
            0x60 => Instr::Bin(BinOp::F32Ge),

            0xa0 => Instr::Bin(BinOp::F64Add),
            0xa1 => Instr::Bin(BinOp::F64Sub),
            0xa2 => Instr::Bin(BinOp::F64Mul),
            0xa3 => Instr::Bin(BinOp::F64Div),
            0x61 => Instr::Bin(BinOp::F64Eq),
            0x62 => Instr::Bin(BinOp::F64Ne),
            0x63 => Instr::Bin(BinOp::F64Lt),
            0x64 => Instr::Bin(BinOp::F64Gt),
            0x65 => Instr::Bin(BinOp::F64Le),
            0x66 => Instr::Bin(BinOp::F64Ge),

            0x0f => Instr::FunctionEnd, // explicit return terminates decode
            0x0b => match frames.pop() {
                None => {
                    // the FUNCTION body's end: stop decoding.
                    code.push(Instr::FunctionEnd);
                    return Ok(code);
                }
                Some(frame) => {
                    let target = if frame.kind == Kind::Loop {
                        frame.start
                    } else {
                        code.len() as u32 // past this end
                    };
                    for site in frame.unresolved {
                        let patched = resolve_branch(code[site].clone(), target);
                        code[site] = patched;
                    }
                    if let Some(site) = frame.else_site {
                        // an if with no else: the false branch lands past
                        // the frame, same as any block exit.
                        code[site] = resolve_branch(code[site].clone(), target);
                    }
                    continue; // ends are consumed; they exist only as targets
                }
            },
            other => {
                return r.err(format!(
                    "opcode {other:#04x} is outside the rustlite dialect"
                ))
            }
        };
        code.push(instr);
    }
}

fn resolve_branch(existing: Instr, target: u32) -> Instr {
    match existing {
        Instr::Br(_) => Instr::Br(target),
        Instr::BrIf(_) => Instr::BrIf(target),
        other => other,
    }
}

// ---- execution ----------------------------------------------------------------

/// one activation. owns everything that differs between simultaneous calls:
/// its locals, its instruction pointer, and where in the value stack its
/// operands begin. NO resume_ip — the caller's ip already sits past its
/// Call when the callee runs.
struct Frame {
    func_idx: usize,
    type_idx: u32,
    /// index of this function's body in module.funcs — the code slice is
    /// re-borrowed from there on every fetch (avoids holding a &Module
    /// inside the frame while frames mutate).
    code_idx: usize,
    ip: usize,
    locals: Vec<Val>,
    /// stack height at entry (args already popped off); unwound on return.
    stack_base: usize,
}

/// deepest wasm call stack allowed. rustlite functions recurse (if/else
/// exists), so unbounded recursion must die as a NAMED trap long before
/// the native stack does. 1024 frames × a few locals is kilobytes.
pub const MAX_CALL_DEPTH: usize = 1024;
/// deepest host→guest re-entry (store_get → cart_alloc → store_get → …).
/// each hop is a nested native `exec`, so this bounds native recursion too.
pub const MAX_HOST_REENTRY: u32 = 4;

/// invoke DEFINED function `func_idx` (an index into `module.funcs` —
/// imports are not callable from outside) with `args` under a fuel budget,
/// on a fresh memory and with NO host: an import call traps HostError.
/// this is the pure-computation entry the evals use; cartridges go through
/// `invoke_hosted` via the lifecycle. returns the function's result (None
/// for void). EVERY abnormal ending is a named Trap.
pub fn invoke(m: &Module, func_idx: usize, args: &[Val], fuel: u64) -> Result<Option<Val>, Trap> {
    let mut mem = m.initial_memory();
    let mut fuel = fuel;
    exec(m, &mut mem, None, &mut fuel, 0, func_idx, args)
}

/// invoke with a live memory and host. `fuel` is shared by everything the
/// call does, including host-triggered re-entries into the guest, and is
/// left holding whatever remains.
pub fn invoke_hosted(
    m: &Module,
    mem: &mut Vec<u8>,
    host: &mut dyn Host,
    func_idx: usize,
    args: &[Val],
    fuel: &mut u64,
) -> Result<Option<Val>, Trap> {
    exec(m, mem, Some(host), fuel, 0, func_idx, args)
}

// `dyn Host + '_` gives the trait object its OWN lifetime: `&mut dyn Host`
// alone pins it to the reference's, and `&mut T` is invariant in T, so a
// reborrow could never be shorter than the whole call — the borrow checker
// would (correctly) refuse the second loop iteration.
fn exec(
    m: &Module,
    mem: &mut Vec<u8>,
    mut host: Option<&mut (dyn Host + '_)>,
    fuel: &mut u64,
    depth: u32,
    func_idx: usize,
    args: &[Val],
) -> Result<Option<Val>, Trap> {
    let mut stack: Vec<Val> = Vec::new();
    let mut frames: Vec<Frame> = Vec::new();
    frames.push(make_frame(m, func_idx, args, &stack)?);

    loop {
        if *fuel == 0 {
            return Err(Trap::FuelExhausted);
        }
        *fuel -= 1;

        // fetch + advance: borrow the code slice through the module (the
        // frame carries its function's INDEX, not a reference — frames must
        // stay mutable while code is read). Instr is small; cloning beats
        // fighting the aliasing.
        let instr: Instr = {
            let f = frames.last().expect("invoke keeps one frame alive");
            let body_code: &[Instr] = &m.funcs[f.code_idx].code;
            match body_code.get(f.ip) {
                Some(i) => i.clone(),
                None => {
                    return Err(Trap::BadControl(format!(
                        "instruction pointer escaped function {}'s body",
                        f.func_idx
                    )))
                }
            }
        };
        frames.last_mut().expect("frame alive").ip += 1;

        match instr {
            Instr::I32Const(v) => stack.push(Val::I32(v)),
            Instr::I64Const(v) => stack.push(Val::I64(v)),
            Instr::F32Const(v) => stack.push(Val::F32(v)),
            Instr::F64Const(v) => stack.push(Val::F64(v)),

            Instr::LocalGet(idx) => {
                let f = frames.last().expect("frame alive");
                match f.locals.get(idx as usize) {
                    Some(v) => stack.push(*v),
                    None => {
                        return Err(Trap::InvalidStack(format!(
                            "local.get {idx} outside {} declared locals",
                            f.locals.len()
                        )))
                    }
                }
            }
            Instr::LocalSet(idx) => {
                let v = pop_val(&mut stack)?;
                let f = frames.last_mut().expect("frame alive");
                match f.locals.get_mut(idx as usize) {
                    Some(slot) => *slot = v,
                    None => {
                        return Err(Trap::InvalidStack(format!(
                            "local.set {idx} outside {} declared locals",
                            f.locals.len()
                        )))
                    }
                }
            }

            Instr::Drop => {
                pop_val(&mut stack)?;
            }
            Instr::Unreachable => return Err(Trap::Unreachable),
            Instr::I32Eqz => {
                let v = pop_i32(&mut stack)?;
                stack.push(Val::I32((v == 0) as i32));
            }

            Instr::Br(target) => {
                frames.last_mut().expect("frame alive").ip = target as usize;
            }
            Instr::BrIf(target) => {
                let cond = pop_i32(&mut stack)?;
                if cond != 0 {
                    frames.last_mut().expect("frame alive").ip = target as usize;
                }
            }

            Instr::I64ExtendI32U => {
                let v = pop_i32(&mut stack)?;
                stack.push(Val::I64(v as u32 as i64));
            }
            Instr::I32WrapI64 => {
                let v = pop_i64(&mut stack)?;
                stack.push(Val::I32(v as i32));
            }

            Instr::I32Load(off) => {
                let addr = pop_i32(&mut stack)?;
                let r = mem_range(mem, addr, off, 4)?;
                let b = &mem[r];
                stack.push(Val::I32(i32::from_le_bytes([b[0], b[1], b[2], b[3]])));
            }
            Instr::I32Load8U(off) => {
                let addr = pop_i32(&mut stack)?;
                let r = mem_range(mem, addr, off, 1)?;
                stack.push(Val::I32(mem[r.start] as i32));
            }
            Instr::I32Store(off) => {
                let v = pop_i32(&mut stack)?; // value is on top, address beneath
                let addr = pop_i32(&mut stack)?;
                let r = mem_range(mem, addr, off, 4)?;
                mem[r].copy_from_slice(&v.to_le_bytes());
            }
            Instr::I32Store8(off) => {
                let v = pop_i32(&mut stack)?;
                let addr = pop_i32(&mut stack)?;
                let r = mem_range(mem, addr, off, 1)?;
                mem[r.start] = v as u8;
            }
            Instr::MemorySize => {
                stack.push(Val::I32((mem.len() / abi::PAGE_BYTES) as i32));
            }

            Instr::Call(idx) => {
                // the wasm index space: imports first, then defined.
                let n_imports = m.imports.len();
                if (idx as usize) < n_imports {
                    call_host(m, mem, host.as_deref_mut(), fuel, depth, idx as usize, &mut stack)?;
                    continue;
                }
                let callee_idx = idx as usize - n_imports;
                if frames.len() >= MAX_CALL_DEPTH {
                    return Err(Trap::CallDepthExceeded(MAX_CALL_DEPTH));
                }
                let n_args = {
                    let Some(callee) = m.funcs.get(callee_idx) else {
                        return Err(Trap::BadArguments(format!(
                            "call to function {idx}, which does not exist \
                             (module has {n_imports} import(s) + {} defined)",
                            m.funcs.len()
                        )));
                    };
                    m.types[callee.type_idx as usize].params.len()
                };
                if stack.len() < n_args {
                    return Err(Trap::InvalidStack(format!(
                        "call to function {idx} needs {n_args} argument(s), \
                         stack holds {}",
                        stack.len()
                    )));
                }
                let call_args: Vec<Val> = stack.split_off(stack.len() - n_args);
                let frame = make_frame(m, callee_idx, &call_args, &stack)?;
                frames.push(frame);
            }

            Instr::FunctionEnd => {
                let wants_result = {
                    let f = frames.last().expect("frame alive");
                    !m.types[f.type_idx as usize].results.is_empty()
                };
                let result = if wants_result {
                    Some(pop_val(&mut stack)?)
                } else {
                    None
                };
                let frame = frames.pop().expect("frame alive");
                stack.truncate(frame.stack_base);
                if let Some(v) = result {
                    stack.push(v);
                }
                if frames.is_empty() {
                    return Ok(result);
                }
                // the caller resumes: its ip already points past its Call.
            }

            Instr::Bin(op) => apply_bin(op, &mut stack)?,
        }
    }
}

/// dispatch one import call to the host. arguments are popped and
/// type-checked against the import's declared signature (a hostile module
/// may declare `log` with any shape it likes — the lifecycle refuses those
/// at load, and this arm refuses them at runtime for bare modules); every
/// (ptr, len) pair is bounds-checked and COPIED before the host sees it.
fn call_host(
    m: &Module,
    mem: &mut Vec<u8>,
    host: Option<&mut (dyn Host + '_)>,
    fuel: &mut u64,
    depth: u32,
    import_idx: usize,
    stack: &mut Vec<Val>,
) -> Result<(), Trap> {
    let imp = &m.imports[import_idx];
    let Some(ft) = m.types.get(imp.type_idx as usize) else {
        return Err(Trap::BadArguments(format!(
            "import '{}' references type {}, which does not exist",
            imp.name, imp.type_idx
        )));
    };
    let n = ft.params.len();
    if stack.len() < n {
        return Err(Trap::InvalidStack(format!(
            "call to import '{}' needs {n} argument(s), stack holds {}",
            imp.name,
            stack.len()
        )));
    }
    let args: Vec<Val> = stack.split_off(stack.len() - n);
    for (i, (a, pt)) in args.iter().zip(ft.params.iter()).enumerate() {
        if !val_matches(a, *pt) {
            return Err(Trap::BadArguments(format!(
                "import '{}' argument {}: expected {}, got {}",
                imp.name,
                i + 1,
                valtype_name(*pt),
                a.type_name()
            )));
        }
    }
    let Some(hf) = imp.host_fn else {
        return Err(Trap::HostError(format!(
            "import {}.{} is not part of the vanish ABI v1 (the host provides: {})",
            imp.module,
            imp.name,
            HostFn::names()
        )));
    };
    let Some(host) = host else {
        return Err(Trap::HostError(format!(
            "the module imports '{}' but no host is attached — cartridges run \
             through the lifecycle, not bare invoke",
            imp.name
        )));
    };
    // the ABI's shapes are all i32 arguments; a module whose import type
    // says otherwise is answered with a named trap, never a panic.
    let i32_at = |k: usize| -> Result<i32, Trap> {
        match args.get(k) {
            Some(Val::I32(v)) => Ok(*v),
            other => Err(Trap::HostError(format!(
                "{}: argument {} is {}, the ABI wants i32",
                hf.name(),
                k + 1,
                other.map(|v| v.type_name()).unwrap_or("missing")
            ))),
        }
    };
    match hf {
        HostFn::Log => {
            let level = i32_at(0)?;
            let msg = bytes_at(mem, i32_at(1)?, i32_at(2)?)?;
            host.log(level, &msg);
        }
        HostFn::NowMs => {
            stack.push(Val::I64(host.now_ms()));
        }
        HostFn::StoreGet => {
            let key = bytes_at(mem, i32_at(0)?, i32_at(1)?)?;
            // bound first: a match scrutinee would hold the reborrow across
            // the arm that hands `host` to guest_alloc.
            let got = host.store_get(&key);
            match got {
                Err(e) => return Err(Trap::HostError(format!("store_get failed: {e}"))),
                Ok(None) => stack.push(Val::I64(0)),
                Ok(Some(value)) => {
                    let ptr = guest_alloc(m, mem, Some(host), fuel, depth, value.len())?;
                    mem[ptr..ptr + value.len()].copy_from_slice(&value);
                    stack.push(Val::I64(abi::pack(ptr as u32, value.len() as u32)));
                }
            }
        }
        HostFn::StoreSet => {
            let key = bytes_at(mem, i32_at(0)?, i32_at(1)?)?;
            let value = bytes_at(mem, i32_at(2)?, i32_at(3)?)?;
            let status = match host.store_set(&key, &value) {
                Ok(()) => 0,
                Err(_) => 1,
            };
            stack.push(Val::I32(status));
        }
        HostFn::Emit => {
            let topic = bytes_at(mem, i32_at(0)?, i32_at(1)?)?;
            let payload = bytes_at(mem, i32_at(2)?, i32_at(3)?)?;
            host.emit(&topic, &payload)
                .map_err(|e| Trap::HostError(format!("emit failed: {e}")))?;
        }
    }
    Ok(())
}

/// ask the guest for `len` bytes through its exported `cart_alloc`, under
/// the caller's fuel, and verify the answer fits the memory. `depth` counts
/// host→guest hops: a cart_alloc that itself calls store_get re-enters
/// here one level deeper, and the bound turns that into a named trap.
/// returns the byte offset the host may write to.
pub fn guest_alloc(
    m: &Module,
    mem: &mut Vec<u8>,
    host: Option<&mut (dyn Host + '_)>,
    fuel: &mut u64,
    depth: u32,
    len: usize,
) -> Result<usize, Trap> {
    if depth >= MAX_HOST_REENTRY {
        return Err(Trap::HostError(format!(
            "host re-entry deeper than {MAX_HOST_REENTRY} — is cart_alloc calling \
             back into the host?"
        )));
    }
    let Some(alloc_idx) = m.defined_export(GuestFn::CartAlloc.name()) else {
        return Err(Trap::HostError(
            "the cartridge exports no cart_alloc, so the host cannot hand it bytes".into(),
        ));
    };
    let len_i32 = i32::try_from(len).map_err(|_| {
        Trap::HostError(format!("{len} bytes does not fit an i32 length"))
    })?;
    let out = exec(m, mem, host, fuel, depth + 1, alloc_idx, &[Val::I32(len_i32)])?;
    let ptr = match out {
        Some(Val::I32(p)) => p,
        other => {
            return Err(Trap::HostError(format!(
                "cart_alloc returned {other:?}, expected an i32 pointer"
            )))
        }
    };
    let start = ptr as u32 as usize;
    match start.checked_add(len) {
        Some(end) if end <= mem.len() => Ok(start),
        _ => Err(Trap::HostError(format!(
            "cart_alloc returned {ptr} for {len} byte(s), outside the {}-byte memory",
            mem.len()
        ))),
    }
}

/// build an activation for `func_idx` with `args`. validates arity AND
/// per-argument valtypes against the signature (hostile modules get named
/// rejections, not silent coercion).
fn make_frame(
    m: &Module,
    func_idx: usize,
    args: &[Val],
    stack_after_args_popped: &[Val],
) -> Result<Frame, Trap> {
    let Some(body) = m.funcs.get(func_idx) else {
        return Err(Trap::BadArguments(format!(
            "function {func_idx} does not exist (module has {})",
            m.funcs.len()
        )));
    };
    let ft = &m.types[body.type_idx as usize];
    if args.len() != ft.params.len() {
        return Err(Trap::BadArguments(format!(
            "function {func_idx} takes {} argument(s), got {}",
            ft.params.len(),
            args.len()
        )));
    }
    for (i, (a, pt)) in args.iter().zip(ft.params.iter()).enumerate() {
        if !val_matches(a, *pt) {
            return Err(Trap::BadArguments(format!(
                "function {func_idx} argument {}: expected {}, got {}",
                i + 1,
                valtype_name(*pt),
                a.type_name()
            )));
        }
    }
    // locals layout: PARAMS FIRST (indices 0..n_params — the signature is
    // their declaration site per spec), then the declared groups zeroed.
    let mut locals =
        Vec::with_capacity(ft.params.len() + body.locals.len());
    for a in args {
        locals.push(*a);
    }
    for &vt in &body.locals {
        locals.push(Val::zero(vt).ok_or_else(|| {
            Trap::BadControl(format!("unknown valtype {vt:#04x} in locals"))
        })?);
    }
    Ok(Frame {
        func_idx,
        type_idx: body.type_idx,
        code_idx: func_idx,
        ip: 0,
        locals,
        stack_base: stack_after_args_popped.len(),
    })
}

fn val_matches(v: &Val, vt: u8) -> bool {
    matches!(
        (v, vt),
        (Val::I32(_), 0x7f)
            | (Val::I64(_), 0x7e)
            | (Val::F32(_), 0x7d)
            | (Val::F64(_), 0x7c)
    )
}

fn valtype_name(vt: u8) -> &'static str {
    match vt {
        0x7f => "i32",
        0x7e => "i64",
        0x7d => "f32",
        _ => "f64",
    }
}

fn pop_val(stack: &mut Vec<Val>) -> Result<Val, Trap> {
    stack
        .pop()
        .ok_or_else(|| Trap::InvalidStack("popped from an empty stack".into()))
}

/// typed pops. each returns its pair in evaluation order (rhs first, lhs
/// second) and names BOTH the wanted type and what it actually found on
/// mismatch — hostile modules get precise rejections, not confusion.
fn pop2_i32(stack: &mut Vec<Val>) -> Result<(i32, i32), Trap> {
    fn one(s: &mut Vec<Val>) -> Result<i32, Trap> {
        match s.pop() {
            Some(Val::I32(v)) => Ok(v),
            Some(other) => Err(Trap::InvalidStack(format!(
                "expected i32, found {}",
                other.type_name()
            ))),
            None => Err(Trap::InvalidStack("expected i32, stack empty".into())),
        }
    }
    let b = one(stack)?;
    let a = one(stack)?;
    Ok((a, b))
}

fn pop2_i64(stack: &mut Vec<Val>) -> Result<(i64, i64), Trap> {
    fn one(s: &mut Vec<Val>) -> Result<i64, Trap> {
        match s.pop() {
            Some(Val::I64(v)) => Ok(v),
            Some(other) => Err(Trap::InvalidStack(format!(
                "expected i64, found {}",
                other.type_name()
            ))),
            None => Err(Trap::InvalidStack("expected i64, stack empty".into())),
        }
    }
    let b = one(stack)?;
    let a = one(stack)?;
    Ok((a, b))
}

fn pop2_f32(stack: &mut Vec<Val>) -> Result<(f32, f32), Trap> {
    fn one(s: &mut Vec<Val>) -> Result<f32, Trap> {
        match s.pop() {
            Some(Val::F32(v)) => Ok(v),
            Some(other) => Err(Trap::InvalidStack(format!(
                "expected f32, found {}",
                other.type_name()
            ))),
            None => Err(Trap::InvalidStack("expected f32, stack empty".into())),
        }
    }
    let b = one(stack)?;
    let a = one(stack)?;
    Ok((a, b))
}

fn pop2_f64(stack: &mut Vec<Val>) -> Result<(f64, f64), Trap> {
    fn one(s: &mut Vec<Val>) -> Result<f64, Trap> {
        match s.pop() {
            Some(Val::F64(v)) => Ok(v),
            Some(other) => Err(Trap::InvalidStack(format!(
                "expected f64, found {}",
                other.type_name()
            ))),
            None => Err(Trap::InvalidStack("expected f64, stack empty".into())),
        }
    }
    let b = one(stack)?;
    let a = one(stack)?;
    Ok((a, b))
}

fn pop_i32(stack: &mut Vec<Val>) -> Result<i32, Trap> {
    match stack.pop() {
        Some(Val::I32(v)) => Ok(v),
        Some(other) => Err(Trap::InvalidStack(format!(
            "expected i32, found {}",
            other.type_name()
        ))),
        None => Err(Trap::InvalidStack("expected i32, stack empty".into())),
    }
}

fn pop_i64(stack: &mut Vec<Val>) -> Result<i64, Trap> {
    match stack.pop() {
        Some(Val::I64(v)) => Ok(v),
        Some(other) => Err(Trap::InvalidStack(format!(
            "expected i64, found {}",
            other.type_name()
        ))),
        None => Err(Trap::InvalidStack("expected i64, stack empty".into())),
    }
}

// ---- memory --------------------------------------------------------------------

/// the byte range an access names, or a named trap. the effective address
/// is computed in u64 so `addr + offset + width` cannot wrap into bounds.
fn mem_range(
    mem: &[u8],
    addr: i32,
    offset: u32,
    width: usize,
) -> Result<std::ops::Range<usize>, Trap> {
    let start = addr as u32 as u64 + offset as u64;
    let end = start + width as u64;
    if end > mem.len() as u64 {
        return Err(Trap::MemoryOutOfBounds(format!(
            "{width} byte(s) at {start} exceed the {}-byte memory",
            mem.len()
        )));
    }
    Ok(start as usize..end as usize)
}

/// copy a guest (ptr, len) buffer out of memory — the ONLY way host code
/// ever sees guest bytes. a negative length is a hostile argument, not a
/// wraparound.
fn bytes_at(mem: &[u8], ptr: i32, len: i32) -> Result<Vec<u8>, Trap> {
    let len = usize::try_from(len).map_err(|_| {
        Trap::MemoryOutOfBounds(format!("negative buffer length {len}"))
    })?;
    let r = mem_range(mem, ptr, 0, len)?;
    Ok(mem[r].to_vec())
}

/// binary operation dispatch. width-resolved at DECODE time; execution only
/// moves values. integer comparisons yield i32 bools (the dialect's only
/// boolean representation). division traps per spec: ÷0 always; MIN÷-1 for
/// div_s only — remainder DEFINES MIN % -1 = 0 and must not trap.
fn apply_bin(op: BinOp, stack: &mut Vec<Val>) -> Result<(), Trap> {
    use BinOp::*;
    let res: Val = match op {
        I32Add => {
            let (a, b) = pop2_i32(stack)?;
            Val::I32(a.wrapping_add(b))
        }
        I32Sub => {
            let (a, b) = pop2_i32(stack)?;
            Val::I32(a.wrapping_sub(b))
        }
        I32Mul => {
            let (a, b) = pop2_i32(stack)?;
            Val::I32(a.wrapping_mul(b))
        }
        I32DivS => {
            let (a, b) = pop2_i32(stack)?;
            if b == 0 {
                return Err(Trap::DivideByZero);
            }
            if a == i32::MIN && b == -1 {
                return Err(Trap::IntegerOverflow);
            }
            Val::I32(a / b)
        }
        I32RemS => {
            let (a, b) = pop2_i32(stack)?;
            if b == 0 {
                return Err(Trap::DivideByZero);
            }
            Val::I32(a.wrapping_rem(b)) // MIN % -1 = 0 by definition
        }
        I32LtS => {
            let (a, b) = pop2_i32(stack)?;
            Val::I32((a < b) as i32)
        }
        I32GtS => {
            let (a, b) = pop2_i32(stack)?;
            Val::I32((a > b) as i32)
        }
        I32LeS => {
            let (a, b) = pop2_i32(stack)?;
            Val::I32((a <= b) as i32)
        }
        I32GeS => {
            let (a, b) = pop2_i32(stack)?;
            Val::I32((a >= b) as i32)
        }
        I32Eq => {
            let (a, b) = pop2_i32(stack)?;
            Val::I32((a == b) as i32)
        }
        I32Ne => {
            let (a, b) = pop2_i32(stack)?;
            Val::I32((a != b) as i32)
        }
        I32And => {
            let (a, b) = pop2_i32(stack)?;
            Val::I32(a & b)
        }
        I32Or => {
            let (a, b) = pop2_i32(stack)?;
            Val::I32(a | b)
        }

        I64Add => {
            let (a, b) = pop2_i64(stack)?;
            Val::I64(a.wrapping_add(b))
        }
        I64Sub => {
            let (a, b) = pop2_i64(stack)?;
            Val::I64(a.wrapping_sub(b))
        }
        I64Mul => {
            let (a, b) = pop2_i64(stack)?;
            Val::I64(a.wrapping_mul(b))
        }
        I64DivS => {
            let (a, b) = pop2_i64(stack)?;
            if b == 0 {
                return Err(Trap::DivideByZero);
            }
            if a == i64::MIN && b == -1 {
                return Err(Trap::IntegerOverflow);
            }
            Val::I64(a / b)
        }
        I64RemS => {
            let (a, b) = pop2_i64(stack)?;
            if b == 0 {
                return Err(Trap::DivideByZero);
            }
            Val::I64(a.wrapping_rem(b))
        }
        I64LtS => {
            let (a, b) = pop2_i64(stack)?;
            Val::I32((a < b) as i32)
        }
        I64GtS => {
            let (a, b) = pop2_i64(stack)?;
            Val::I32((a > b) as i32)
        }
        I64LeS => {
            let (a, b) = pop2_i64(stack)?;
            Val::I32((a <= b) as i32)
        }
        I64GeS => {
            let (a, b) = pop2_i64(stack)?;
            Val::I32((a >= b) as i32)
        }
        I64Eq => {
            let (a, b) = pop2_i64(stack)?;
            Val::I32((a == b) as i32)
        }
        I64Ne => {
            let (a, b) = pop2_i64(stack)?;
            Val::I32((a != b) as i32)
        }
        I64Shl => {
            let (a, b) = pop2_i64(stack)?;
            Val::I64(a.wrapping_shl((b & 63) as u32)) // count mod 64 per spec
        }
        I64ShrU => {
            let (a, b) = pop2_i64(stack)?;
            Val::I64(((a as u64) >> (b & 63)) as i64)
        }
        I64Or => {
            let (a, b) = pop2_i64(stack)?;
            Val::I64(a | b)
        }

        F32Add => {
            let (a, b) = pop2_f32(stack)?;
            Val::F32(a + b)
        }
        F32Sub => {
            let (a, b) = pop2_f32(stack)?;
            Val::F32(a - b)
        }
        F32Mul => {
            let (a, b) = pop2_f32(stack)?;
            Val::F32(a * b)
        }
        F32Div => {
            let (a, b) = pop2_f32(stack)?;
            Val::F32(a / b)
        }
        F32Eq => {
            let (a, b) = pop2_f32(stack)?;
            Val::I32((a == b) as i32)
        }
        F32Ne => {
            let (a, b) = pop2_f32(stack)?;
            Val::I32((a != b) as i32)
        }
        F32Lt => {
            let (a, b) = pop2_f32(stack)?;
            Val::I32((a < b) as i32)
        }
        F32Gt => {
            let (a, b) = pop2_f32(stack)?;
            Val::I32((a > b) as i32)
        }
        F32Le => {
            let (a, b) = pop2_f32(stack)?;
            Val::I32((a <= b) as i32)
        }
        F32Ge => {
            let (a, b) = pop2_f32(stack)?;
            Val::I32((a >= b) as i32)
        }

        F64Add => {
            let (a, b) = pop2_f64(stack)?;
            Val::F64(a + b)
        }
        F64Sub => {
            let (a, b) = pop2_f64(stack)?;
            Val::F64(a - b)
        }
        F64Mul => {
            let (a, b) = pop2_f64(stack)?;
            Val::F64(a * b)
        }
        F64Div => {
            let (a, b) = pop2_f64(stack)?;
            Val::F64(a / b)
        }
        F64Eq => {
            let (a, b) = pop2_f64(stack)?;
            Val::I32((a == b) as i32)
        }
        F64Ne => {
            let (a, b) = pop2_f64(stack)?;
            Val::I32((a != b) as i32)
        }
        F64Lt => {
            let (a, b) = pop2_f64(stack)?;
            Val::I32((a < b) as i32)
        }
        F64Gt => {
            let (a, b) = pop2_f64(stack)?;
            Val::I32((a > b) as i32)
        }
        F64Le => {
            let (a, b) = pop2_f64(stack)?;
            Val::I32((a <= b) as i32)
        }
        F64Ge => {
            let (a, b) = pop2_f64(stack)?;
            Val::I32((a >= b) as i32)
        }
    };
    stack.push(res);
    Ok(())
}
