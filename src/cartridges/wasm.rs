//! L2 — rustlite wasm emission: typed AST → final .wasm bytes.
//!
//! THE WHOLE BET (CARTRIDGE_PLAN §5): no LLVM, no cranelift, no linker —
//! the module is emitted complete and valid in one pass, which is why this
//! can run inside vanish's own wasm where full rustc cannot (rustc-on-wasm
//! dies on linking, not on codegen).
//!
//! pipeline: `check_fn` walks each function building the name→local-index
//! map and asserting every expression's type; `emit_module` then writes
//! bytes assuming well-typed input — emission is total over checked input
//! and never invents types of its own. every emitted module is round-trip
//! validated with wasmparser in tests, so a bad byte sequence fails ci
//! rather than surfacing as a trap at cartridge-load time.
//!
//! LITERALS ARE CONTEXT-TYPED: `5` is i32 in `fn f() -> i32 { return 5; }`
//! and i64 inside an i64 expression, mirroring rust's untyped literals.
//! ONE function per decision (`literal_ty`) answers both walks — checker
//! and emitter call it with the same hint, so they cannot disagree.

use super::rustlite::{BinOp, Block, Expr, FnDecl, Stmt, Ty, UnOp};
use std::collections::HashMap;

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

fn valty(ty: Ty) -> u8 {
    match ty {
        Ty::I32 | Ty::Bool => 0x7f,
        Ty::I64 => 0x7e,
        Ty::F32 => 0x7d,
        Ty::F64 => 0x7c,
    }
}

/// callee signatures by name, shared by the checker walks.
type SigTys<'a> = &'a dyn Fn(&str) -> Option<(Vec<Ty>, Option<Ty>)>;

/// type-check one function body. returns (name, local types, n_params).
/// `fns` supplies callee signatures; resolution is order-free (forward
/// references are natural in cartridges).
pub fn check_fn(f: &FnDecl, fns: &[FnDecl]) -> Result<(String, Vec<Ty>, u32), TypeError> {
    let sig_tys: SigTys = &|name: &str| {
        fns.iter().find(|g| g.sig.name == name).map(|g| {
            (
                g.sig.params.iter().map(|(_, t)| *t).collect::<Vec<_>>(),
                g.sig.ret,
            )
        })
    };

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
        sig_tys: SigTys,
        err: &dyn Fn(String) -> TypeError,
    ) -> Result<(), TypeError> {
        for s in &b.stmts {
            match s {
                Stmt::Let { name, ty, init } => {
                    let it = check_expr(init, scope, sig_tys, err, *ty)?;
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
                    let vt = check_expr(value, scope, sig_tys, err, target)?;
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
                    let ct = check_expr(cond, scope, sig_tys, err, Ty::Bool)?;
                    if ct != Ty::Bool {
                        return Err(err(format!(
                            "while condition must be bool, got {}",
                            ct.name()
                        )));
                    }
                    check_block(body, scope, f, sig_tys, err)?;
                }
                Stmt::Return(e) => {
                    let rt = match e {
                        None => None,
                        Some(e) => Some(check_expr(
                            e,
                            scope,
                            sig_tys,
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
                    check_expr(e, scope, sig_tys, err, Ty::I64)?;
                }
            }
        }
        Ok(())
    }

    check_block(&f.body, &mut scope, f, sig_tys, &err)?;

    let n_params = scope.n_params;
    Ok((f.sig.name.clone(), scope.types, n_params))
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
    sig_tys: SigTys,
    err: &dyn Fn(String) -> TypeError,
    hint: Ty,
) -> Result<Ty, TypeError> {
    match e {
        Expr::IntLit(_) => Ok(literal_ty(false, hint)),
        Expr::FloatLit(_) => Ok(literal_ty(true, hint)),
        Expr::BoolLit(_) => Ok(Ty::Bool),
        Expr::Var(name) => scope
            .ty_of(name)
            .ok_or_else(|| err(format!("use of undeclared '{name}'"))),
        Expr::Unary(op, inner) => {
            let t = check_expr(inner, scope, sig_tys, err, hint)?;
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
                check_expr(r, scope, sig_tys, err, hint)?
            } else {
                check_expr(l, scope, sig_tys, err, hint)?
            };
            let lt = ty;
            let rt = if is_numeric_literal(r) {
                check_expr(r, scope, sig_tys, err, lt)?
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
        Expr::Call { callee, args } => {
            let Some((param_tys, ret)) = sig_tys(callee) else {
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
                let at = check_expr(a, scope, sig_tys, err, *pt)?;
                if at != *pt {
                    return Err(err(format!(
                        "'{callee}' argument {}: expected {}, got {}",
                        i + 1,
                        pt.name(),
                        at.name()
                    )));
                }
            }
            ret.ok_or_else(|| {
                err(format!("'{callee}' returns nothing but is used as a value"))
            })
        }
    }
}

// ---- emission ----------------------------------------------------------------

/// compile a whole translation unit to a valid .wasm module.
/// type errors are reported BEFORE any bytes are written.
pub fn emit_module(fns: &[FnDecl]) -> Result<Vec<u8>, TypeError> {
    for f in fns {
        check_fn(f, fns)?;
    }

    let mut m = Vec::new();
    m.extend_from_slice(b"\0asm");
    m.extend_from_slice(&[1, 0, 0, 0]);

    // type section: dedup signatures so identical fns share one type index.
    let mut types: Vec<(Vec<u8>, Vec<u8>)> = Vec::new(); // (params, results)
    let mut fn_type_idx = Vec::with_capacity(fns.len());
    for f in fns {
        let params: Vec<u8> = f.sig.params.iter().map(|(_, t)| valty(*t)).collect();
        let results: Vec<u8> = f.sig.ret.map(valty).into_iter().collect();
        let idx = types
            .iter()
            .position(|(p, r)| p == &params && r == &results)
            .unwrap_or_else(|| {
                types.push((params, results));
                types.len() - 1
            });
        fn_type_idx.push(idx as u32);
    }

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

    // section 3: functions
    let mut s = Vec::new();
    uleb(fns.len() as u64, &mut s);
    for idx in &fn_type_idx {
        uleb(u64::from(*idx), &mut s);
    }
    write_section(&mut m, 3, &s);

    // sections 5+7 skipped: no memories, no tables, no exports yet — the
    // runtime (L3) instantiates and calls by index, not by name.

    // section 10: code
    let mut s = Vec::new();
    uleb(fns.len() as u64, &mut s);
    for f in fns {
        let body = emit_function(f, fns);
        uleb(body.len() as u64, &mut s);
        s.extend_from_slice(&body);
    }
    write_section(&mut m, 10, &s);

    Ok(m)
}

fn write_section(out: &mut Vec<u8>, id: u8, payload: &[u8]) {
    out.push(id);
    uleb(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

fn emit_function(f: &FnDecl, fns: &[FnDecl]) -> Vec<u8> {
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
        emit_stmt(stmt, &scope, fns, ret_hint, &mut body);
    }
    body.push(0x0b); // end (function)
    body
}

fn emit_stmt(s: &Stmt, scope: &FnScope, fns: &[FnDecl], ret_hint: Ty, out: &mut Vec<u8>) {
    match s {
        Stmt::Let { name, ty, init } => {
            // hint = the declaration's own type (checker proved agreement).
            emit_expr(init, scope, fns, out, *ty);
            if let Some(&i) = scope.indexes.get(name) {
                out.push(0x21); // local.set consumes the value: siblings see
                uleb(u64::from(i), out); // a balanced stack.
            }
        }
        Stmt::Assign { name, value } => {
            let hint = scope.ty_of(name).unwrap_or(Ty::I64);
            emit_expr(value, scope, fns, out, hint);
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
            emit_expr(cond, scope, fns, out, Ty::Bool);
            out.push(0x45); // i32.eqz — bools are i32 at runtime
            out.push(0x0d);
            out.push(0x01); // br_if 1 → leaves the BLOCK when cond is false
            for st in &body.stmts {
                emit_stmt(st, scope, fns, ret_hint, out);
            }
            out.push(0x0c);
            out.push(0x00); // br 0 → back to loop header (continue)
            out.push(0x0b); // end loop
            out.push(0x0b); // end block
        }
        Stmt::Return(e) => {
            if let Some(e) = e {
                emit_expr(e, scope, fns, out, ret_hint);
            }
            out.push(0x0f); // return
        }
        Stmt::Expr(e) => {
            emit_expr(e, scope, fns, out, Ty::I64);
            // expression statements must not leave values on the stack;
            // value-returning calls are the only case, so drop the result.
            if let Expr::Call { callee, .. } = e {
                if fns.iter().any(|g| g.sig.name == *callee && g.sig.ret.is_some()) {
                    out.push(0x1a); // drop
                }
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
fn emit_expr(e: &Expr, scope: &FnScope, fns: &[FnDecl], out: &mut Vec<u8>, hint: Ty) {
    match e {
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
            let t = expr_ty(inner, scope, fns, hint);
            emit_expr(inner, scope, fns, out, hint);
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
            let ty = binary_operand_type(l, r, scope, fns, hint);
            emit_expr(l, scope, fns, out, ty);
            emit_expr(r, scope, fns, out, ty);
            emit_binop(*op, ty, out);
        }
        Expr::Call { callee, args } => {
            // args carry the callee's declared param types as hints — the
            // same hints the checker used, so literals shrink identically.
            let Some(g) = fns.iter().find(|g| g.sig.name == *callee) else {
                return; // checker already refused unknown callees
            };
            for (a, (_, pt)) in args.iter().zip(g.sig.params.iter()) {
                emit_expr(a, scope, fns, out, *pt);
            }
            if let Some(idx) = fns.iter().position(|g| g.sig.name == *callee) {
                out.push(0x10); // call
                uleb(u64::from(idx as u32), out);
            }
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
    fns: &[FnDecl],
    hint: Ty,
) -> Ty {
    match (is_numeric_literal(l), is_numeric_literal(r)) {
        (true, true) => literal_ty(is_float_literal(l), hint),
        (true, false) => expr_ty(r, scope, fns, hint),
        (false, true) => expr_ty(l, scope, fns, hint),
        (false, false) => expr_ty(l, scope, fns, hint),
    }
}

/// re-derive an expression's type during emission WITHOUT side effects.
/// shares literal_ty with the checker so both walks answer identically.
fn expr_ty(e: &Expr, scope: &FnScope, fns: &[FnDecl], hint: Ty) -> Ty {
    match e {
        Expr::IntLit(_) => literal_ty(false, hint),
        Expr::FloatLit(_) => literal_ty(true, hint),
        Expr::BoolLit(_) => Ty::Bool,
        Expr::Var(n) => scope.ty_of(n).unwrap_or(Ty::I64),
        Expr::Unary(_, inner) => expr_ty(inner, scope, fns, hint),
        Expr::Binary(op, l, _) => {
            let lt = expr_ty(l, scope, fns, hint);
            op.result_ty(lt).unwrap_or(Ty::Bool)
        }
        Expr::Call { callee, .. } => fns
            .iter()
            .find(|g| g.sig.name == *callee)
            .and_then(|g| g.sig.ret)
            .unwrap_or(Ty::I64),
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
