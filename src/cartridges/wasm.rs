//! L2 — rustlite wasm emission: typed AST → final .wasm bytes.
//!
//! THE WHOLE BET (CARTRIDGE_PLAN §5): no LLVM, no cranelift, no linker —
//! the module is emitted complete and valid in one pass, which is why this
//! can run inside vanish's own wasm where full rustc cannot (rustc-on-wasm
//! dies on linking, not on codegen).
//!
//! pipeline: `check_program` verifies the module-level contract (unique
//! names, extern declarations against the L1 ABI table, lifecycle exports
//! against theirs); `check_fn` walks each function building the name→local-
//! index map and asserting every expression's type; `emit_module` then
//! writes bytes assuming well-typed input — emission is total over checked
//! input and never invents types of its own. every emitted module is
//! round-trip validated with wasmparser in tests, so a bad byte sequence
//! fails ci rather than surfacing as a trap at cartridge-load time.
//!
//! LITERALS ARE CONTEXT-TYPED: `5` is i32 in `fn f() -> i32 { return 5; }`
//! and i64 inside an i64 expression, mirroring rust's untyped literals.
//! ONE function per decision (`literal_ty`) answers both walks — checker
//! and emitter call it with the same hint, so they cannot disagree.
//!
//! FUNCTION INDEX SPACE: wasm numbers imports first, then defined
//! functions. a `call` to a defined function is therefore
//! `externs.len() + position`; the runtime applies the identical rule when
//! it splits calls between host dispatch and frames.
//!
//! STRINGS: every string literal in the program is interned (sorted, so
//! the layout is deterministic) into ONE active data segment at
//! DATA_BASE; a literal expression lowers to `i64.const pack(addr, len)`.
//! `data_end()` lowers to the first byte past the segment (8-aligned), so
//! a guest allocator knows where its heap may begin without a linker.

use super::abi::{self, GuestFn, HostFn};
use super::rustlite::{BinOp, Block, Expr, FnDecl, FnSig, Program, Stmt, Ty, UnOp};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// where the data segment starts in guest memory. bytes 0..16 are left to
/// the guest (the test cartridges keep their heap pointer at 0).
pub const DATA_BASE: u32 = 16;

/// the interned string literals of one program and where they live.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layout {
    /// literal → absolute address of its first byte.
    pub offsets: BTreeMap<String, u32>,
    /// the segment's bytes, in `offsets` order.
    pub blob: Vec<u8>,
    /// first byte past the segment, rounded up to 8 — what `data_end()`
    /// answers. DATA_BASE when there are no literals.
    pub end: u32,
}

impl Layout {
    pub fn of(program: &Program) -> Layout {
        let mut lits: BTreeSet<&str> = BTreeSet::new();
        fn in_expr<'a>(e: &'a Expr, out: &mut BTreeSet<&'a str>) {
            match e {
                Expr::StrLit(s) => {
                    out.insert(s.as_str());
                }
                Expr::Unary(_, inner) => in_expr(inner, out),
                Expr::Binary(_, l, r) => {
                    in_expr(l, out);
                    in_expr(r, out);
                }
                Expr::Call { args, .. } => args.iter().for_each(|a| in_expr(a, out)),
                _ => {}
            }
        }
        fn in_block<'a>(b: &'a Block, out: &mut BTreeSet<&'a str>) {
            for s in &b.stmts {
                match s {
                    Stmt::Let { init, .. } => in_expr(init, out),
                    Stmt::Assign { value, .. } => in_expr(value, out),
                    Stmt::While { cond, body } => {
                        in_expr(cond, out);
                        in_block(body, out);
                    }
                    Stmt::If { cond, then, els } => {
                        in_expr(cond, out);
                        in_block(then, out);
                        if let Some(els) = els {
                            in_block(els, out);
                        }
                    }
                    Stmt::Return(Some(e)) | Stmt::Expr(e) => in_expr(e, out),
                    Stmt::Return(None) => {}
                }
            }
        }
        for f in &program.fns {
            in_block(&f.body, &mut lits);
        }
        let mut layout = Layout::default();
        for s in lits {
            layout
                .offsets
                .insert(s.to_string(), DATA_BASE + layout.blob.len() as u32);
            layout.blob.extend_from_slice(s.as_bytes());
        }
        layout.end = (DATA_BASE + layout.blob.len() as u32).div_ceil(8) * 8;
        layout
    }

    /// the packed value a literal expression evaluates to.
    pub fn packed(&self, s: &str) -> i64 {
        let ptr = self.offsets.get(s).copied().unwrap_or(DATA_BASE);
        abi::pack(ptr, s.len() as u32)
    }
}

// ---- LEB128 ---------------------------------------------------------------

/// unsigned LEB128. every count/size/index in the binary format uses this;
/// getting it wrong is how hand-rolled emitters produce modules that parse
/// as garbage, so it is pinned by its own test below against known bytes.
pub fn uleb(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// signed LEB128 for i32/i64 constants.
fn sleb(mut v: i64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        let sign_bit_set = byte & 0x40 != 0;
        if (v == 0 && !sign_bit_set) || (v == -1 && sign_bit_set) {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

// ---- shared type resolution --------------------------------------------------
//
// these helpers are called by BOTH the checker and the emitter with the
// same inputs. one definition means the two walks cannot drift apart — the
// failure mode this file exists to prevent is exactly "checker approved X,
// emitter wrote Y".

/// the type of an untyped numeric literal under context `hint`.
pub fn literal_ty(is_float: bool, hint: Ty) -> Ty {
    if is_float {
        match hint {
            Ty::F32 => Ty::F32,
            _ => Ty::F64,
        }
    } else {
        match hint {
            Ty::I32 => Ty::I32,
            _ => Ty::I64,
        }
    }
}

fn is_numeric_literal(e: &Expr) -> bool {
    matches!(e, Expr::IntLit(_) | Expr::FloatLit(_))
}

fn is_float_literal(e: &Expr) -> bool {
    matches!(e, Expr::FloatLit(_))
}

fn valty(ty: Ty) -> u8 {
    ty.valtype()
}

// ---- intrinsics ---------------------------------------------------------------

/// memory and packing intrinsics: the only way rustlite touches linear
/// memory or builds an ABI packed result. each parses as an ordinary call
/// and lowers INLINE to one wasm instruction sequence — never a `call`.
/// rustlite has no pointers, casts, or shifts on purpose (each is a
/// type-system feature the checker would then have to understand); these
/// cover exactly what the L1 ABI needs and nothing speculative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intrinsic {
    /// `load_u8(addr: i32) -> i32` — i32.load8_u
    LoadU8,
    /// `store_u8(addr: i32, v: i32)` — i32.store8 (low byte of v)
    StoreU8,
    /// `load_i32(addr: i32) -> i32` — i32.load
    LoadI32,
    /// `store_i32(addr: i32, v: i32)` — i32.store
    StoreI32,
    /// `memory_size() -> i32` — pages of 64 KiB
    MemorySize,
    /// `pack(ptr: i32, len: i32) -> i64` — the ABI's packed result
    Pack,
    /// `unpack_ptr(v: i64) -> i32`
    UnpackPtr,
    /// `unpack_len(v: i64) -> i32`
    UnpackLen,
    /// `data_end() -> i32` — first byte past the string data segment
    /// (8-aligned): where a guest allocator's heap may start.
    DataEnd,
}

impl Intrinsic {
    pub const ALL: [Intrinsic; 9] = [
        Intrinsic::LoadU8,
        Intrinsic::StoreU8,
        Intrinsic::LoadI32,
        Intrinsic::StoreI32,
        Intrinsic::MemorySize,
        Intrinsic::Pack,
        Intrinsic::UnpackPtr,
        Intrinsic::UnpackLen,
        Intrinsic::DataEnd,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Intrinsic::LoadU8 => "load_u8",
            Intrinsic::StoreU8 => "store_u8",
            Intrinsic::LoadI32 => "load_i32",
            Intrinsic::StoreI32 => "store_i32",
            Intrinsic::MemorySize => "memory_size",
            Intrinsic::Pack => "pack",
            Intrinsic::UnpackPtr => "unpack_ptr",
            Intrinsic::UnpackLen => "unpack_len",
            Intrinsic::DataEnd => "data_end",
        }
    }

    pub fn from_name(name: &str) -> Option<Intrinsic> {
        Intrinsic::ALL.iter().copied().find(|i| i.name() == name)
    }

    pub fn signature(self) -> (&'static [Ty], Option<Ty>) {
        match self {
            Intrinsic::LoadU8 => (&[Ty::I32], Some(Ty::I32)),
            Intrinsic::StoreU8 => (&[Ty::I32, Ty::I32], None),
            Intrinsic::LoadI32 => (&[Ty::I32], Some(Ty::I32)),
            Intrinsic::StoreI32 => (&[Ty::I32, Ty::I32], None),
            Intrinsic::MemorySize => (&[], Some(Ty::I32)),
            Intrinsic::Pack => (&[Ty::I32, Ty::I32], Some(Ty::I64)),
            Intrinsic::UnpackPtr => (&[Ty::I64], Some(Ty::I32)),
            Intrinsic::UnpackLen => (&[Ty::I64], Some(Ty::I32)),
            Intrinsic::DataEnd => (&[], Some(Ty::I32)),
        }
    }

    /// does lowering this intrinsic require a linear memory to exist?
    /// (data_end is only meaningful with a memory to lay data out in.)
    pub fn touches_memory(self) -> bool {
        !matches!(
            self,
            Intrinsic::Pack | Intrinsic::UnpackPtr | Intrinsic::UnpackLen
        )
    }
}

// ---- name resolution -----------------------------------------------------------

/// what a call site names. intrinsics are reserved (check_program refuses
/// a user function or extern shadowing one), so resolution is unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Callee {
    Intrinsic(Intrinsic),
    Extern(u32),
    Defined(u32),
}

fn resolve(program: &Program, name: &str) -> Option<Callee> {
    if let Some(i) = Intrinsic::from_name(name) {
        return Some(Callee::Intrinsic(i));
    }
    if let Some(i) = program.externs.iter().position(|s| s.name == name) {
        return Some(Callee::Extern(i as u32));
    }
    program
        .fns
        .iter()
        .position(|f| f.sig.name == name)
        .map(|i| Callee::Defined(i as u32))
}

fn sig_params(sig: &FnSig) -> Vec<Ty> {
    sig.params.iter().map(|(_, t)| *t).collect()
}

/// callee signature by name, resolution order intrinsic → extern → defined.
/// order-free over the program: forward references are natural in
/// cartridges.
fn sig_of(program: &Program, name: &str) -> Option<(Vec<Ty>, Option<Ty>)> {
    match resolve(program, name)? {
        Callee::Intrinsic(i) => {
            let (p, r) = i.signature();
            Some((p.to_vec(), r))
        }
        Callee::Extern(i) => {
            let s = &program.externs[i as usize];
            Some((sig_params(s), s.ret))
        }
        Callee::Defined(i) => {
            let s = &program.fns[i as usize].sig;
            Some((sig_params(s), s.ret))
        }
    }
}

/// the wasm function index a call to `callee` names (imports first).
fn call_index(program: &Program, callee: Callee) -> Option<u32> {
    match callee {
        Callee::Intrinsic(_) => None,
        Callee::Extern(i) => Some(i),
        Callee::Defined(i) => Some(program.externs.len() as u32 + i),
    }
}

// ---- type checking ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct TypeError {
    pub fn_name: String,
    pub msg: String,
}

/// one function's locals: params first (indices 0..n), then lets in
/// declaration order. wasm local.get/set are INDEXED, so this map IS the
/// calling convention between check and emit.
struct FnScope {
    indexes: HashMap<String, u32>,
    /// index → declared type, parallel to the local declarations section.
    types: Vec<Ty>,
    n_params: u32,
}

impl FnScope {
    fn new(params: &[(String, Ty)]) -> Self {
        let mut indexes = HashMap::new();
        let mut types = Vec::new();
        for (i, (name, ty)) in params.iter().enumerate() {
            indexes.insert(name.clone(), i as u32);
            types.push(*ty);
        }
        Self {
            indexes,
            types,
            n_params: params.len() as u32,
        }
    }

    fn declare(&mut self, name: &str, ty: Ty) -> Result<u32, String> {
        if self.indexes.contains_key(name) {
            return Err(format!("'{name}' is already declared in this scope"));
        }
        let idx = self.types.len() as u32;
        self.indexes.insert(name.to_string(), idx);
        self.types.push(ty);
        Ok(idx)
    }

    fn ty_of(&self, name: &str) -> Option<Ty> {
        self.indexes.get(name).map(|&i| self.types[i as usize])
    }
}

/// the module-level contract, checked BEFORE any function body: names are
/// unique and never shadow an intrinsic; every extern is a real L1 host
/// function with the ABI's exact signature; every lifecycle export carries
/// its ABI signature. all of these would otherwise surface as a load-time
/// refusal by the runtime — catching them here names the fix at the
/// source line instead.
pub fn check_program(program: &Program) -> Result<(), TypeError> {
    let mut seen = std::collections::BTreeSet::new();
    for s in &program.externs {
        if let Some(i) = Intrinsic::from_name(&s.name) {
            return Err(TypeError {
                fn_name: s.name.clone(),
                msg: format!("'{}' is a rustlite intrinsic and cannot be imported", i.name()),
            });
        }
        if !seen.insert(s.name.clone()) {
            return Err(TypeError {
                fn_name: s.name.clone(),
                msg: format!("'{}' is declared twice", s.name),
            });
        }
        let Some(h) = HostFn::from_name(&s.name) else {
            return Err(TypeError {
                fn_name: s.name.clone(),
                msg: format!(
                    "extern '{}' is not in the vanish ABI v1 — the host provides exactly: {}",
                    s.name,
                    HostFn::names()
                ),
            });
        };
        let (want_p, want_r) = h.signature();
        if sig_params(s) != want_p || s.ret != want_r {
            return Err(TypeError {
                fn_name: s.name.clone(),
                msg: format!(
                    "extern '{}' is declared as {} but the ABI defines it as {}",
                    s.name,
                    abi::describe(&sig_params(s), s.ret),
                    abi::describe(want_p, want_r)
                ),
            });
        }
    }
    for f in &program.fns {
        let name = &f.sig.name;
        if let Some(i) = Intrinsic::from_name(name) {
            return Err(TypeError {
                fn_name: name.clone(),
                msg: format!("'{}' is a rustlite intrinsic and cannot be redefined", i.name()),
            });
        }
        if !seen.insert(name.clone()) {
            return Err(TypeError {
                fn_name: name.clone(),
                msg: format!("'{name}' is declared twice (extern and fn share one namespace)"),
            });
        }
        if let Some(g) = GuestFn::from_name(name) {
            let (want_p, want_r) = g.signature();
            if sig_params(&f.sig) != want_p || f.sig.ret != want_r {
                return Err(TypeError {
                    fn_name: name.clone(),
                    msg: format!(
                        "lifecycle export '{}' must be {} (found {})",
                        name,
                        abi::describe(want_p, want_r),
                        abi::describe(&sig_params(&f.sig), f.sig.ret)
                    ),
                });
            }
            if !f.is_pub {
                return Err(TypeError {
                    fn_name: name.clone(),
                    msg: format!(
                        "'{name}' is a lifecycle entry point and must be `pub fn` so the host can call it"
                    ),
                });
            }
        }
    }
    Ok(())
}

/// type-check one function body. returns (name, local types, n_params).
/// callee signatures resolve against the whole program, order-free.
pub fn check_fn(f: &FnDecl, program: &Program) -> Result<(String, Vec<Ty>, u32), TypeError> {
    let err = |msg: String| TypeError {
        fn_name: f.sig.name.clone(),
        msg,
    };

    let mut seen = std::collections::BTreeSet::new();
    for (n, _) in &f.sig.params {
        if !seen.insert(n) {
            return Err(err(format!("duplicate parameter '{n}'")));
        }
    }

    let mut scope = FnScope::new(&f.sig.params);

    fn check_block(
        b: &Block,
        scope: &mut FnScope,
        f: &FnDecl,
        program: &Program,
        err: &dyn Fn(String) -> TypeError,
    ) -> Result<(), TypeError> {
        for s in &b.stmts {
            match s {
                Stmt::Let { name, ty, init } => {
                    let it = check_expr(init, scope, program, err, *ty)?;
                    if it != *ty {
                        return Err(err(format!(
                            "let '{name}' declares {}, but its initializer yields {}",
                            ty.name(),
                            it.name()
                        )));
                    }
                    scope.declare(name, *ty).map_err(err)?;
                }
                Stmt::Assign { name, value } => {
                    // assignment to an undeclared name dies HERE, not at
                    // emission — the parser is deliberately scope-blind.
                    let Some(target) = scope.ty_of(name) else {
                        return Err(err(format!(
                            "assignment to '{name}', which is not declared — \
                             add a `let {name}: <type> = …` first"
                        )));
                    };
                    let vt = check_expr(value, scope, program, err, target)?;
                    if vt != target {
                        return Err(err(format!(
                            "cannot assign {} to '{}' (declared {})",
                            vt.name(),
                            name,
                            target.name()
                        )));
                    }
                }
                Stmt::While { cond, body } => {
                    let ct = check_expr(cond, scope, program, err, Ty::Bool)?;
                    if ct != Ty::Bool {
                        return Err(err(format!(
                            "while condition must be bool, got {}",
                            ct.name()
                        )));
                    }
                    check_block(body, scope, f, program, err)?;
                }
                Stmt::If { cond, then, els } => {
                    let ct = check_expr(cond, scope, program, err, Ty::Bool)?;
                    if ct != Ty::Bool {
                        return Err(err(format!(
                            "if condition must be bool, got {}",
                            ct.name()
                        )));
                    }
                    check_block(then, scope, f, program, err)?;
                    if let Some(els) = els {
                        check_block(els, scope, f, program, err)?;
                    }
                }
                Stmt::Return(e) => {
                    let rt = match e {
                        None => None,
                        Some(e) => Some(check_expr(
                            e,
                            scope,
                            program,
                            err,
                            f.sig.ret.unwrap_or(Ty::I64),
                        )?),
                    };
                    if rt != f.sig.ret {
                        return Err(err(format!(
                            "return yields {:?} but the signature says {:?}",
                            rt.map(|t| t.name()),
                            f.sig.ret.as_ref().map(|t| t.name())
                        )));
                    }
                }
                Stmt::Expr(e) => {
                    // a call in statement position may be void — that is
                    // what makes `log(…);` writable at all. any other
                    // expression must type like a value (its result is
                    // dropped at emission).
                    match e {
                        Expr::Call { callee, args } => {
                            check_call(callee, args, scope, program, err)?;
                        }
                        other => {
                            check_expr(other, scope, program, err, Ty::I64)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    check_block(&f.body, &mut scope, f, program, &err)?;

    // a value-returning function must END in a return on every path.
    // rustlite has no implicit tail expression, and wasm validation would
    // reject the module for an empty stack at the function's end — but as
    // a validator error three layers away, not a source-level one.
    if f.sig.ret.is_some() && !block_returns(&f.body) {
        return Err(err(format!(
            "'{}' returns {} but its body does not end with `return` on every \
             path — rustlite v1 has no implicit tail expression",
            f.sig.name,
            f.sig.ret.map(|t| t.name()).unwrap_or("nothing")
        )));
    }

    let n_params = scope.n_params;
    Ok((f.sig.name.clone(), scope.types, n_params))
}

/// does control leave this block only through `return <value>`? an if/else
/// counts when BOTH arms do; a loop never does (its condition may be false
/// on entry).
fn block_returns(b: &Block) -> bool {
    match b.stmts.last() {
        Some(Stmt::Return(Some(_))) => true,
        Some(Stmt::If {
            then,
            els: Some(els),
            ..
        }) => block_returns(then) && block_returns(els),
        _ => false,
    }
}

/// arity + per-argument types of a call, returning its result type (None
/// for void). shared by expression and statement position so a void
/// callee is refused ONLY where its value would be used.
fn check_call(
    callee: &str,
    args: &[Expr],
    scope: &mut FnScope,
    program: &Program,
    err: &dyn Fn(String) -> TypeError,
) -> Result<Option<Ty>, TypeError> {
    let Some((param_tys, ret)) = sig_of(program, callee) else {
        return Err(err(format!("call to unknown function '{callee}'")));
    };
    if args.len() != param_tys.len() {
        return Err(err(format!(
            "'{callee}' takes {} argument(s), got {}",
            param_tys.len(),
            args.len()
        )));
    }
    for (i, (a, pt)) in args.iter().zip(param_tys.iter()).enumerate() {
        let at = check_expr(a, scope, program, err, *pt)?;
        if at != *pt {
            return Err(err(format!(
                "'{callee}' argument {}: expected {}, got {}",
                i + 1,
                pt.name(),
                at.name()
            )));
        }
    }
    Ok(ret)
}

/// the type of `e` under context hint. THE single source of truth — the
/// emitter calls this same function to decide instruction variants, so a
/// disagreement between the walks is structurally impossible rather than
/// merely tested-against. (the emitter's expr_ty mirrors this walk without
/// the error channel because checked input cannot reach it in a failing
/// state.)
fn check_expr(
    e: &Expr,
    scope: &mut FnScope,
    program: &Program,
    err: &dyn Fn(String) -> TypeError,
    hint: Ty,
) -> Result<Ty, TypeError> {
    match e {
        Expr::IntLit(_) => Ok(literal_ty(false, hint)),
        Expr::FloatLit(_) => Ok(literal_ty(true, hint)),
        Expr::BoolLit(_) => Ok(Ty::Bool),
        // a string is its packed (ptr, len): always i64, whatever the hint.
        Expr::StrLit(_) => Ok(Ty::I64),
        Expr::Var(name) => scope
            .ty_of(name)
            .ok_or_else(|| err(format!("use of undeclared '{name}'"))),
        Expr::Unary(op, inner) => {
            let t = check_expr(inner, scope, program, err, hint)?;
            match op {
                UnOp::Neg => match t {
                    Ty::I32 | Ty::I64 | Ty::F32 | Ty::F64 => Ok(t),
                    other => Err(err(format!(
                        "negation needs a numeric type, got {}",
                        other.name()
                    ))),
                },
            }
        }
        Expr::Binary(op, l, r) => {
            // resolve the operand type EXACTLY the way emit_expr's
            // binary_operand_type does (l-first; a literal borrows its
            // partner), THEN check both sides under that type as their
            // hint. checking r first with the ambient hint would let `2.0`
            // become f64 while l resolved i32 — a false mismatch.
            let ty = if is_numeric_literal(l) && !is_numeric_literal(r) {
                check_expr(r, scope, program, err, hint)?
            } else {
                check_expr(l, scope, program, err, hint)?
            };
            let lt = ty;
            let rt = if is_numeric_literal(r) {
                check_expr(r, scope, program, err, lt)?
            } else {
                lt
            };
            if lt != rt {
                return Err(err(format!(
                    "binary op operands must match: {} vs {}",
                    lt.name(),
                    rt.name()
                )));
            }
            // % on floats has no wasm instruction (the spec has no frem);
            // refusing here keeps emission total over checked input.
            if *op == BinOp::Rem && matches!(lt, Ty::F32 | Ty::F64) {
                return Err(err(
                    "% is integer-only in rustlite — floats have no remainder \
                     instruction in wasm; restructure the math or keep it integral"
                        .to_string(),
                ));
            }
            op.result_ty(lt).map_err(err)
        }
        Expr::Call { callee, args } => check_call(callee, args, scope, program, err)?
            .ok_or_else(|| err(format!("'{callee}' returns nothing but is used as a value"))),
    }
}

// ---- emission ----------------------------------------------------------------

/// does any function body use an intrinsic that needs linear memory?
fn uses_memory(program: &Program) -> bool {
    fn in_expr(e: &Expr) -> bool {
        match e {
            Expr::Call { callee, args } => {
                Intrinsic::from_name(callee).is_some_and(|i| i.touches_memory())
                    || args.iter().any(in_expr)
            }
            // a literal lives in the data segment, which needs a memory.
            Expr::StrLit(_) => true,
            Expr::Unary(_, inner) => in_expr(inner),
            Expr::Binary(_, l, r) => in_expr(l) || in_expr(r),
            _ => false,
        }
    }
    fn in_block(b: &Block) -> bool {
        b.stmts.iter().any(|s| match s {
            Stmt::Let { init, .. } => in_expr(init),
            Stmt::Assign { value, .. } => in_expr(value),
            Stmt::While { cond, body } => in_expr(cond) || in_block(body),
            Stmt::If { cond, then, els } => {
                in_expr(cond) || in_block(then) || els.as_ref().is_some_and(in_block)
            }
            Stmt::Return(e) => e.as_ref().is_some_and(in_expr),
            Stmt::Expr(e) => in_expr(e),
        })
    }
    program.fns.iter().any(|f| in_block(&f.body))
}

/// compile a whole translation unit to a valid .wasm module.
/// type errors are reported BEFORE any bytes are written.
///
/// a module gets a linear memory (and exports it as "memory") when it
/// imports anything, exports anything, or touches memory — i.e. whenever it
/// is a cartridge rather than a pure function library. pure modules keep
/// the minimal [types, functions, code] layout.
pub fn emit_module(program: &Program) -> Result<Vec<u8>, TypeError> {
    check_program(program)?;
    for f in &program.fns {
        check_fn(f, program)?;
    }

    let layout = Layout::of(program);
    let memory_bytes = abi::GUEST_MEMORY_PAGES as usize * abi::PAGE_BYTES;
    if layout.end as usize > memory_bytes {
        return Err(TypeError {
            fn_name: String::new(),
            msg: format!(
                "string literals total {} bytes, which does not fit the {memory_bytes}-byte \
                 guest memory",
                layout.blob.len()
            ),
        });
    }

    let exported: Vec<&FnDecl> = program.fns.iter().filter(|f| f.is_pub).collect();
    let needs_memory =
        !program.externs.is_empty() || !exported.is_empty() || uses_memory(program);

    let mut m = Vec::new();
    m.extend_from_slice(b"\0asm");
    m.extend_from_slice(&[1, 0, 0, 0]);

    // type section: dedup signatures so identical fns share one type index.
    // externs are interned FIRST so import type indices exist before the
    // function section references anything.
    let mut types: Vec<(Vec<u8>, Vec<u8>)> = Vec::new(); // (params, results)
    let mut intern = |sig: &FnSig| -> u32 {
        let params: Vec<u8> = sig.params.iter().map(|(_, t)| valty(*t)).collect();
        let results: Vec<u8> = sig.ret.map(valty).into_iter().collect();
        let idx = types
            .iter()
            .position(|(p, r)| p == &params && r == &results)
            .unwrap_or_else(|| {
                types.push((params, results));
                types.len() - 1
            });
        idx as u32
    };
    let extern_type_idx: Vec<u32> = program.externs.iter().map(&mut intern).collect();
    let fn_type_idx: Vec<u32> = program.fns.iter().map(|f| intern(&f.sig)).collect();

    // section 1: types
    let mut s = Vec::new();
    uleb(types.len() as u64, &mut s);
    for (params, results) in &types {
        s.push(0x60); // functype
        uleb(params.len() as u64, &mut s);
        s.extend_from_slice(params);
        uleb(results.len() as u64, &mut s);
        s.extend_from_slice(results);
    }
    write_section(&mut m, 1, &s);

    // section 2: imports — every extern, from the one ABI module
    if !program.externs.is_empty() {
        let mut s = Vec::new();
        uleb(program.externs.len() as u64, &mut s);
        for (sig, ti) in program.externs.iter().zip(&extern_type_idx) {
            write_name(&mut s, abi::IMPORT_MODULE);
            write_name(&mut s, &sig.name);
            s.push(0x00); // importdesc: func
            uleb(u64::from(*ti), &mut s);
        }
        write_section(&mut m, 2, &s);
    }

    // section 3: functions
    let mut s = Vec::new();
    uleb(program.fns.len() as u64, &mut s);
    for idx in &fn_type_idx {
        uleb(u64::from(*idx), &mut s);
    }
    write_section(&mut m, 3, &s);

    // section 5: memory — one, fixed size (min == max: no growth)
    if needs_memory {
        let mut s = Vec::new();
        uleb(1, &mut s);
        s.push(0x01); // limits: min + max present
        uleb(u64::from(abi::GUEST_MEMORY_PAGES), &mut s);
        uleb(u64::from(abi::GUEST_MEMORY_PAGES), &mut s);
        write_section(&mut m, 5, &s);
    }

    // section 7: exports — every `pub fn` by name, plus the memory
    if needs_memory || !exported.is_empty() {
        let mut s = Vec::new();
        uleb(exported.len() as u64 + u64::from(needs_memory), &mut s);
        for f in &exported {
            write_name(&mut s, &f.sig.name);
            s.push(0x00); // exportdesc: func
            let defined = program
                .fns
                .iter()
                .position(|g| g.sig.name == f.sig.name)
                .unwrap_or(0);
            uleb(program.externs.len() as u64 + defined as u64, &mut s);
        }
        if needs_memory {
            write_name(&mut s, abi::MEMORY_EXPORT);
            s.push(0x02); // exportdesc: memory
            uleb(0, &mut s);
        }
        write_section(&mut m, 7, &s);
    }

    // section 10: code
    let mut s = Vec::new();
    uleb(program.fns.len() as u64, &mut s);
    for f in &program.fns {
        let body = emit_function(f, program, &layout);
        uleb(body.len() as u64, &mut s);
        s.extend_from_slice(&body);
    }
    write_section(&mut m, 10, &s);

    // section 11: data — every literal, one active segment at DATA_BASE
    if !layout.blob.is_empty() {
        let mut s = Vec::new();
        uleb(1, &mut s);
        s.push(0x00); // active segment in memory 0
        s.push(0x41); // i32.const DATA_BASE
        sleb(i64::from(DATA_BASE), &mut s);
        s.push(0x0b); // end (of the offset expression)
        uleb(layout.blob.len() as u64, &mut s);
        s.extend_from_slice(&layout.blob);
        write_section(&mut m, 11, &s);
    }

    Ok(m)
}

fn write_section(out: &mut Vec<u8>, id: u8, payload: &[u8]) {
    out.push(id);
    uleb(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

/// a wasm `name`: uleb byte length + utf-8 bytes.
fn write_name(out: &mut Vec<u8>, s: &str) {
    uleb(s.len() as u64, out);
    out.extend_from_slice(s.as_bytes());
}

fn emit_function(f: &FnDecl, program: &Program, layout: &Layout) -> Vec<u8> {
    // rebuild the same scope walk the checker did — deterministic, so the
    // indices match exactly.
    let mut scope = FnScope::new(&f.sig.params);
    let ret_hint = f.sig.ret.unwrap_or(Ty::I64);
    let mut body = Vec::new();

    // pre-walk statements to register lets in source order BEFORE emitting
    // any code, because wasm locals are function-scoped even though rustlite
    // lets read block-scoped. declaring up front preserves the checker's
    // indexing while keeping get/set correct anywhere in the body.
    fn predeclare(scope: &mut FnScope, b: &Block) {
        for s in &b.stmts {
            match s {
                Stmt::Let { name, ty, .. } => {
                    let _ = scope.declare(name, *ty);
                }
                Stmt::While { body, .. } => predeclare(scope, body),
                Stmt::If { then, els, .. } => {
                    predeclare(scope, then);
                    if let Some(els) = els {
                        predeclare(scope, els);
                    }
                }
                _ => {}
            }
        }
    }
    predeclare(&mut scope, &f.body);

    // local declaration groups: only NON-param locals go here, compressed
    // into runs of the same type.
    let mut local_decls: Vec<(u32, u8)> = Vec::new();
    let mut pending_count: u32 = 0;
    let mut pending_ty: Option<u8> = None;
    for ty in &scope.types[scope.n_params as usize..] {
        let v = valty(*ty);
        match pending_ty {
            Some(t) if t == v => pending_count += 1,
            _ => {
                if let Some(t) = pending_ty.take() {
                    local_decls.push((pending_count, t));
                }
                pending_ty = Some(v);
                pending_count = 1;
            }
        }
    }
    if let Some(t) = pending_ty.take() {
        local_decls.push((pending_count, t));
    }

    uleb(local_decls.len() as u64, &mut body);
    for (count, ty) in &local_decls {
        uleb(u64::from(*count), &mut body);
        body.push(*ty);
    }

    for stmt in &f.body.stmts {
        emit_stmt(stmt, &scope, program, layout, ret_hint, &mut body);
    }
    // a value-returning body whose last statement is an if/else (both arms
    // return — check_fn proved it) leaves the validator looking at an empty
    // stack at the function's end: it cannot know every path returned.
    // `unreachable` is stack-polymorphic, so it satisfies the validator, and
    // it can never execute — reaching it would mean check_fn was wrong, and
    // the runtime turns it into a named trap rather than a silent fall-off.
    if f.sig.ret.is_some() && !matches!(f.body.stmts.last(), Some(Stmt::Return(_))) {
        body.push(0x00); // unreachable
    }
    body.push(0x0b); // end (function)
    body
}

fn emit_stmt(
    s: &Stmt,
    scope: &FnScope,
    program: &Program,
    layout: &Layout,
    ret_hint: Ty,
    out: &mut Vec<u8>,
) {
    match s {
        Stmt::Let { name, ty, init } => {
            // hint = the declaration's own type (checker proved agreement).
            emit_expr(init, scope, program, layout, out, *ty);
            if let Some(&i) = scope.indexes.get(name) {
                out.push(0x21); // local.set consumes the value: siblings see
                uleb(u64::from(i), out); // a balanced stack.
            }
        }
        Stmt::Assign { name, value } => {
            let hint = scope.ty_of(name).unwrap_or(Ty::I64);
            emit_expr(value, scope, program, layout, out, hint);
            if let Some(&i) = scope.indexes.get(name) {
                out.push(0x21); // local.set
                uleb(u64::from(i), out);
            }
        }
        Stmt::While { cond, body } => {
            // wasm label semantics: branching TO a loop label RESTARTS it;
            // exiting requires targeting an enclosing BLOCK. so a while is
            // block $exit { loop $cont { cond; eqz; br_if $exit; body;
            // br $cont } }. getting this backwards validates cleanly and
            // hangs at runtime — the worst kind of wrong, caught by writing
            // the spec down instead of trusting the intent.
            out.push(0x02); // block $exit
            out.push(0x40);
            out.push(0x03); // loop $cont
            out.push(0x40);
            emit_expr(cond, scope, program, layout, out, Ty::Bool);
            out.push(0x45); // i32.eqz — bools are i32 at runtime
            out.push(0x0d);
            out.push(0x01); // br_if 1 → leaves the BLOCK when cond is false
            for st in &body.stmts {
                emit_stmt(st, scope, program, layout, ret_hint, out);
            }
            out.push(0x0c);
            out.push(0x00); // br 0 → back to loop header (continue)
            out.push(0x0b); // end loop
            out.push(0x0b); // end block
        }
        Stmt::If { cond, then, els } => {
            // cond; if (void) { then } [else { els }] end — the spec's own
            // if/else frame. arms are statements, so the blocktype is void
            // and neither arm leaves a value.
            emit_expr(cond, scope, program, layout, out, Ty::Bool);
            out.push(0x04); // if
            out.push(0x40); // blocktype: void
            for st in &then.stmts {
                emit_stmt(st, scope, program, layout, ret_hint, out);
            }
            if let Some(els) = els {
                out.push(0x05); // else
                for st in &els.stmts {
                    emit_stmt(st, scope, program, layout, ret_hint, out);
                }
            }
            out.push(0x0b); // end if
        }
        Stmt::Return(e) => {
            if let Some(e) = e {
                emit_expr(e, scope, program, layout, out, ret_hint);
            }
            out.push(0x0f); // return
        }
        Stmt::Expr(e) => {
            emit_expr(e, scope, program, layout, out, Ty::I64);
            // expression statements must not leave values on the stack;
            // whatever the expression yields is dropped. void calls yield
            // nothing and need no drop.
            if expr_ty_opt(e, scope, program, Ty::I64).is_some() {
                out.push(0x1a); // drop
            }
        }
    }
}

/// emit `e`, choosing instruction variants by `hint`.
///
/// the hint threading MIRRORS check_expr argument-for-argument: literals
/// via literal_ty(hint), binary operands resolved l-first with the partner
/// borrowing, call args hinted by declared param types. divergence between
/// what was checked and what is written would be a type lie in the bytes.
fn emit_expr(
    e: &Expr,
    scope: &FnScope,
    program: &Program,
    layout: &Layout,
    out: &mut Vec<u8>,
    hint: Ty,
) {
    match e {
        Expr::StrLit(s) => {
            out.push(0x42); // i64.const — the packed (ptr, len)
            sleb(layout.packed(s), out);
        }
        Expr::IntLit(v) => {
            if literal_ty(false, hint) == Ty::I32 {
                out.push(0x41); // i32.const
            } else {
                out.push(0x42); // i64.const
            }
            sleb(*v, out);
        }
        Expr::FloatLit(v) => {
            if literal_ty(true, hint) == Ty::F32 {
                out.push(0x43); // f32.const
                out.extend_from_slice(&(*v as f32).to_bits().to_le_bytes());
            } else {
                out.push(0x44); // f64.const
                out.extend_from_slice(&v.to_bits().to_le_bytes());
            }
        }
        Expr::BoolLit(b) => {
            out.push(0x41); // i32.const — bools are i32 at runtime
            sleb(if *b { 1 } else { 0 }, out);
        }
        Expr::Var(name) => {
            if let Some(&i) = scope.indexes.get(name) {
                out.push(0x20); // local.get
                uleb(u64::from(i), out);
            }
        }
        Expr::Unary(UnOp::Neg, inner) => {
            // inner's type under the SAME hint the checker used.
            let t = expr_ty(inner, scope, program, hint);
            emit_expr(inner, scope, program, layout, out, hint);
            // negation as multiply-by-minus-one across all widths:
            // i32 ×(-1)=0x6c, i64=0x7e, f32=0x94, f64=0xa2.
            match t {
                Ty::I32 => {
                    out.push(0x41);
                    sleb(-1, out);
                    out.push(0x6c);
                }
                Ty::F32 => {
                    out.push(0x43);
                    out.extend_from_slice(&(-1f32).to_bits().to_le_bytes());
                    out.push(0x94);
                }
                Ty::F64 => {
                    out.push(0x44);
                    out.extend_from_slice(&(-1f64).to_bits().to_le_bytes());
                    out.push(0xa2);
                }
                _ => {
                    out.push(0x42);
                    sleb(-1, out);
                    out.push(0x7e);
                }
            }
        }
        Expr::Binary(op, l, r) => {
            // resolve the operand type FIRST (same rule as the checker:
            // literal borrows its partner; otherwise left's type wins),
            // then emit both sides with it as their hint.
            let ty = binary_operand_type(l, r, scope, program, hint);
            emit_expr(l, scope, program, layout, out, ty);
            emit_expr(r, scope, program, layout, out, ty);
            emit_binop(*op, ty, out);
        }
        Expr::Call { callee, args } => {
            // args carry the callee's declared param types as hints — the
            // same hints the checker used, so literals shrink identically.
            let Some(target) = resolve(program, callee) else {
                return; // checker already refused unknown callees
            };
            if let Callee::Intrinsic(i) = target {
                emit_intrinsic(i, args, scope, program, layout, out);
                return;
            }
            let Some((param_tys, _)) = sig_of(program, callee) else {
                return;
            };
            for (a, pt) in args.iter().zip(param_tys.iter()) {
                emit_expr(a, scope, program, layout, out, *pt);
            }
            if let Some(idx) = call_index(program, target) {
                out.push(0x10); // call
                uleb(u64::from(idx), out);
            }
        }
    }
}

/// lower an intrinsic call inline. argument order and interleaving are
/// dictated by the wasm stack: `pack` must widen the pointer BEFORE the
/// length is pushed, so its arguments are emitted between the ops.
fn emit_intrinsic(
    i: Intrinsic,
    args: &[Expr],
    scope: &FnScope,
    program: &Program,
    layout: &Layout,
    out: &mut Vec<u8>,
) {
    let (pts, _) = i.signature();
    let arg = |k: usize, out: &mut Vec<u8>| {
        if let (Some(a), Some(pt)) = (args.get(k), pts.get(k)) {
            emit_expr(a, scope, program, layout, out, *pt);
        }
    };
    match i {
        Intrinsic::LoadU8 => {
            arg(0, out);
            out.extend_from_slice(&[0x2d, 0x00, 0x00]); // i32.load8_u align=0 offset=0
        }
        Intrinsic::StoreU8 => {
            arg(0, out);
            arg(1, out);
            out.extend_from_slice(&[0x3a, 0x00, 0x00]); // i32.store8
        }
        Intrinsic::LoadI32 => {
            arg(0, out);
            out.extend_from_slice(&[0x28, 0x02, 0x00]); // i32.load align=2 (4 bytes)
        }
        Intrinsic::StoreI32 => {
            arg(0, out);
            arg(1, out);
            out.extend_from_slice(&[0x36, 0x02, 0x00]); // i32.store
        }
        Intrinsic::MemorySize => {
            out.extend_from_slice(&[0x3f, 0x00]); // memory.size 0
        }
        Intrinsic::Pack => {
            arg(0, out);
            out.push(0xad); // i64.extend_i32_u
            out.push(0x42); // i64.const 32
            sleb(32, out);
            out.push(0x86); // i64.shl
            arg(1, out);
            out.push(0xad); // i64.extend_i32_u
            out.push(0x84); // i64.or
        }
        Intrinsic::UnpackPtr => {
            arg(0, out);
            out.push(0x42); // i64.const 32
            sleb(32, out);
            out.push(0x88); // i64.shr_u
            out.push(0xa7); // i32.wrap_i64
        }
        Intrinsic::UnpackLen => {
            arg(0, out);
            out.push(0xa7); // i32.wrap_i64
        }
        Intrinsic::DataEnd => {
            out.push(0x41); // i32.const — resolved at emission, no linker
            sleb(i64::from(layout.end), out);
        }
    }
}

/// the operand type of a binary pair, applying the checker's exact rule:
/// a literal takes its partner's type; two literals take the ambient hint
/// (int→i64 unless hinted i32, float→f64 unless hinted f32); otherwise
/// left's type governs (the checker asserts equality).
fn binary_operand_type(
    l: &Expr,
    r: &Expr,
    scope: &FnScope,
    program: &Program,
    hint: Ty,
) -> Ty {
    match (is_numeric_literal(l), is_numeric_literal(r)) {
        (true, true) => literal_ty(is_float_literal(l), hint),
        (true, false) => expr_ty(r, scope, program, hint),
        (false, true) => expr_ty(l, scope, program, hint),
        (false, false) => expr_ty(l, scope, program, hint),
    }
}

/// re-derive an expression's type during emission WITHOUT side effects.
/// shares literal_ty with the checker so both walks answer identically.
fn expr_ty(e: &Expr, scope: &FnScope, program: &Program, hint: Ty) -> Ty {
    expr_ty_opt(e, scope, program, hint).unwrap_or(Ty::I64)
}

/// like expr_ty, but a void call is None — the statement emitter uses this
/// to decide whether a drop is owed.
fn expr_ty_opt(e: &Expr, scope: &FnScope, program: &Program, hint: Ty) -> Option<Ty> {
    match e {
        Expr::IntLit(_) => Some(literal_ty(false, hint)),
        Expr::FloatLit(_) => Some(literal_ty(true, hint)),
        Expr::BoolLit(_) => Some(Ty::Bool),
        Expr::StrLit(_) => Some(Ty::I64),
        Expr::Var(n) => Some(scope.ty_of(n).unwrap_or(Ty::I64)),
        Expr::Unary(_, inner) => Some(expr_ty(inner, scope, program, hint)),
        Expr::Binary(op, l, _) => {
            let lt = expr_ty(l, scope, program, hint);
            Some(op.result_ty(lt).unwrap_or(Ty::Bool))
        }
        Expr::Call { callee, .. } => sig_of(program, callee).and_then(|(_, ret)| ret),
    }
}

fn emit_binop(op: BinOp, ty: Ty, out: &mut Vec<u8>) {
    let code: u8 = match (op, ty) {
        (BinOp::Add, Ty::I64) => 0x7c,
        (BinOp::Sub, Ty::I64) => 0x7d,
        (BinOp::Mul, Ty::I64) => 0x7e,
        (BinOp::Div, Ty::I64) => 0x7f, // signed div
        (BinOp::Rem, Ty::I64) => 0x81,
        (BinOp::Lt, Ty::I64) => 0x53,
        (BinOp::Gt, Ty::I64) => 0x55,
        (BinOp::Le, Ty::I64) => 0x57,
        (BinOp::Ge, Ty::I64) => 0x59,
        (BinOp::Eq, Ty::I64) => 0x51,
        (BinOp::Ne, Ty::I64) => 0x52,

        (BinOp::Add, Ty::I32 | Ty::Bool) => 0x6a,
        (BinOp::Sub, Ty::I32 | Ty::Bool) => 0x6b,
        (BinOp::Mul, Ty::I32 | Ty::Bool) => 0x6c,
        (BinOp::Div, Ty::I32 | Ty::Bool) => 0x6d,
        (BinOp::Rem, Ty::I32 | Ty::Bool) => 0x6f,
        (BinOp::Lt, Ty::I32 | Ty::Bool) => 0x48,
        (BinOp::Gt, Ty::I32 | Ty::Bool) => 0x4a,
        (BinOp::Le, Ty::I32 | Ty::Bool) => 0x4c,
        (BinOp::Ge, Ty::I32 | Ty::Bool) => 0x4e,
        (BinOp::Eq, Ty::I32 | Ty::Bool) => 0x46,
        (BinOp::Ne, Ty::I32 | Ty::Bool) => 0x47,

        (BinOp::Add, Ty::F64) => 0xa0,
        (BinOp::Sub, Ty::F64) => 0xa1,
        (BinOp::Mul, Ty::F64) => 0xa2,
        (BinOp::Div, Ty::F64) => 0xa3,
        (BinOp::Eq, Ty::F64) => 0x61,
        (BinOp::Ne, Ty::F64) => 0x62,
        (BinOp::Lt, Ty::F64) => 0x63,
        (BinOp::Gt, Ty::F64) => 0x64,
        (BinOp::Le, Ty::F64) => 0x65,
        (BinOp::Ge, Ty::F64) => 0x66,

        (BinOp::Add, Ty::F32) => 0x92,
        (BinOp::Sub, Ty::F32) => 0x93,
        (BinOp::Mul, Ty::F32) => 0x94,
        (BinOp::Div, Ty::F32) => 0x95,
        (BinOp::Eq, Ty::F32) => 0x5b,
        (BinOp::Ne, Ty::F32) => 0x5c,
        (BinOp::Lt, Ty::F32) => 0x5d,
        (BinOp::Gt, Ty::F32) => 0x5e,
        (BinOp::Le, Ty::F32) => 0x5f,
        (BinOp::Ge, Ty::F32) => 0x60,

        // logical ops short-circuit in rust; v1 emits them eagerly (both
        // sides evaluated). documented limitation until control-flow
        // emission lands properly — pure predicates are unaffected.
        (BinOp::And, _) => 0x71, // i32.and (bools are i32)
        (BinOp::Or, _) => 0x72,  // i32.or

        // the checker refuses % on floats before emission ever sees it;
        // this arm exists so the match proves emission total.
        (BinOp::Rem, Ty::F32) | (BinOp::Rem, Ty::F64) => {
            unreachable!("checker rejects float remainder before emit_binop")
        }
    };
    out.push(code);
}
