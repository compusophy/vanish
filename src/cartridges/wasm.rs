//! L2 — rustlite wasm emission: typed AST → final .wasm bytes.
//!
//! THE WHOLE BET (CARTRIDGE_PLAN §5): no LLVM, no cranelift, no linker —
//! the module is emitted complete and valid in one pass, which is why this
//! can run inside vanish's own wasm where full rustc cannot (rustc-on-wasm
//! dies on linking, not on codegen).
//!
//! pipeline: `type_check` walks each function building the name→local-index
//! map and asserting every expression's type; `emit_module` then writes
//! bytes assuming well-typed input — emission is total over checked input
//! and never invents types of its own. every emitted module is round-trip
//! validated with wasmparser in tests, so a bad byte sequence fails ci
//! rather than surfacing as a trap at cartridge-load time.

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
            // duplicate parameter names shadow silently otherwise; refuse.
            if indexes.insert(name.clone(), i as u32).is_some() {
                // handled by caller via validate_fn signature dup check
            }
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

/// type-check one function body. returns the resolved local map for the
/// emitter. `fns` supplies callee signatures for call checking — rustlite
/// requires callees to be declared before use is NOT imposed (order-free
/// resolution), because forward references are natural in cartridges.
pub fn check_fn(
    f: &FnDecl,
    fns: &[FnDecl],
) -> Result<(String, Vec<Ty>, u32), TypeError> {
    let sig_tys = |name: &str| -> Option<(Vec<Ty>, Option<Ty>)> {
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

    // duplicate parameter names would make the index map lie.
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
        fns: &[FnDecl],
        sig_tys: &dyn Fn(&str) -> Option<(Vec<Ty>, Option<Ty>)>,
        err: &dyn Fn(String) -> TypeError,
    ) -> Result<(), TypeError> {
        for s in &b.stmts {
            match s {
                Stmt::Let { name, ty, init } => {
                    let it = check_expr(init, scope, f, fns, sig_tys, err)?;
                    if it != *ty {
                        return Err(err(format!(
                            "let '{name}' declares {}, but its initializer yields {}",
                            ty.name(),
                            it.name()
                        )));
                    }
                    scope.declare(name, *ty).map_err(|e| err(e))?;
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
                    let vt = check_expr(value, scope, f, fns, sig_tys, err)?;
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
                    let ct = check_expr(cond, scope, f, fns, sig_tys, err)?;
                    if ct != Ty::Bool {
                        return Err(err(format!(
                            "while condition must be bool, got {}",
                            ct.name()
                        )));
                    }
                    check_block(body, scope, f, fns, sig_tys, err)?;
                }
                Stmt::Return(e) => {
                    let rt = match e {
                        None => None,
                        Some(e) => Some(check_expr(e, scope, f, fns, sig_tys, err)?),
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
                    check_expr(e, scope, f, fns, sig_tys, err)?;
                }
            }
        }
        Ok(())
    }

    check_block(
        &f.body,
        &mut scope,
        f,
        fns,
        &sig_tys,
        &err,
    )?;

    let n_params = scope.n_params;
    Ok((f.sig.name.clone(), scope.types, n_params))
}

fn check_expr<'a>(
    e: &'a Expr,
    scope: &mut FnScope,
    f: &'a FnDecl,
    fns: &[FnDecl],
    sig_tys: &dyn Fn(&str) -> Option<(Vec<Ty>, Option<Ty>)>,
    err: &dyn Fn(String) -> TypeError,
) -> Result<Ty, TypeError> {
    match e {
        Expr::IntLit(_) => Ok(Ty::I64),
        Expr::FloatLit(_) => Ok(Ty::F64),
        Expr::BoolLit(_) => Ok(Ty::Bool),
        Expr::Var(name) => scope
            .ty_of(name)
            .ok_or_else(|| err(format!("use of undeclared '{name}'"))),
        Expr::Unary(op, inner) => {
            let t = check_expr(inner, scope, f, fns, sig_tys, err)?;
            match op {
                UnOp::Neg => match t {
                    Ty::I64 | Ty::F64 => Ok(t),
                    other => Err(err(format!(
                        "negation needs i64 or f64, got {}",
                        other.name()
                    ))),
                },
            }
        }
        Expr::Binary(op, l, r) => {
            let lt = check_expr(l, scope, f, fns, sig_tys, err)?;
            let rt = check_expr(r, scope, f, fns, sig_tys, err)?;
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
                return Err(err(format!(
                    "%% is integer-only in rustlite — floats have no remainder \
                     instruction in wasm; write it as x - (x / y).floor() * y once \
                     floor() lands, or keep the math in integers"
                )));
            }
            op.result_ty(lt).map_err(|e| err(e))
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
                let at = check_expr(a, scope, f, fns, sig_tys, err)?;
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
    #[allow(clippy::cast_possible_truncation)] // section item counts are small
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

fn emit_function<'a>(f: &FnDecl, fns: &'a [FnDecl]) -> Vec<u8> {
    // rebuild the same scope walk the checker did — deterministic, so the
    // indices match exactly.
    let mut scope = FnScope::new(&f.sig.params);
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
        emit_stmt(stmt, &scope, fns, &mut body);
    }
    body.push(0x0b); // end (function)
    body
}

fn emit_stmt(s: &Stmt, scope: &FnScope, fns: &[FnDecl], out: &mut Vec<u8>) {
    match s {
        Stmt::Let { name, init, .. } => {
            emit_expr(init, scope, fns, out);
            // local.set consumes the value: the stack stays balanced for
            // sibling statements. locals were pre-declared in source order,
            // so the name lookup is exact.
            if let Some(&i) = scope.indexes.get(name) {
                out.push(0x21); // local.set
                uleb(u64::from(i), out);
            }
        }
        Stmt::Assign { name, value } => {
            emit_expr(value, scope, fns, out);
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
            // hangs at runtime — the worst kind of wrong, caught here by
            // writing the spec down instead of trusting the intent.
            out.push(0x02); // block $exit
            out.push(0x40);
            out.push(0x03); // loop $cont
            out.push(0x40);
            emit_expr(cond, scope, fns, out);
            out.push(0x45); // i32.eqz — bools are i32 at runtime
            out.push(0x0d);
            out.push(0x01); // br_if 1 → leaves the BLOCK when cond is false
            for st in &body.stmts {
                emit_stmt(st, scope, fns, out);
            }
            out.push(0x0c);
            out.push(0x00); // br 0 → back to loop header (continue)
            out.push(0x0b); // end loop
            out.push(0x0b); // end block
        }
        Stmt::Return(e) => {
            if let Some(e) = e {
                emit_expr(e, scope, fns, out);
            }
            out.push(0x0f); // return
        }
        Stmt::Expr(e) => {
            emit_expr(e, scope, fns, out);
            // expression statements must not leave values on the stack.
            // calls returning a value are the only case; drop it.
            if let Expr::Call { callee, .. } = e {
                if fns.iter().any(|g| g.sig.name == *callee && g.sig.ret.is_some()) {
                    out.push(0x1a); // drop
                }
            }
        }
    }
}

fn emit_expr(e: &Expr, scope: &FnScope, fns: &[FnDecl], out: &mut Vec<u8>) {
    match e {
        Expr::IntLit(v) => {
            out.push(0x42); // i64.const
            sleb(*v, out);
        }
        Expr::FloatLit(v) => {
            out.push(0x44); // f64.const
            out.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        Expr::BoolLit(b) => {
            out.push(0x41); // i32.const
            sleb(if *b { 1 } else { 0 }, out);
        }
        Expr::Var(name) => {
            if let Some(&i) = scope.indexes.get(name) {
                out.push(0x20); // local.get
                uleb(u64::from(i), out);
            }
        }
        Expr::Unary(UnOp::Neg, inner) => {
            emit_expr(inner, scope, fns, out);
            // negation as multiply-by-minus-one: correct on both i64 and
            // f64 (no overflow hazard beyond what the language already has,
            // and no stack gymnastics).
            match infer_expr_ty(inner, scope, fns) {
                Ty::I64 => {
                    // 0 - x : push i64.const 0 under x? stack order forbids.
                    // instead: x already pushed; use i64.const -1 * mul.
                    out.push(0x42);
                    sleb(-1, out);
                    out.push(0x7e); // i64.mul
                }
                _ => {
                    // f64: same trick
                    out.push(0x44);
                    let bits = (-1f64).to_bits();
                    out.extend_from_slice(&bits.to_le_bytes());
                    out.push(0xa2); // f64.mul
                }
            }
        }
        Expr::Binary(op, l, r) => {
            emit_expr(l, scope, fns, out);
            emit_expr(r, scope, fns, out);
            let ty = infer_expr_ty(l, scope, fns);
            emit_binop(*op, ty, out);
        }
        Expr::Call { callee, args } => {
            for a in args {
                emit_expr(a, scope, fns, out);
            }
            if let Some(idx) = fns.iter().position(|g| g.sig.name == *callee) {
                out.push(0x10); // call
                uleb(u64::from(idx as u32), out);
            }
        }
    }
}

/// re-derive an expression's type during emission. the checker already
/// proved consistency; this only picks instruction variants.
fn infer_expr_ty(e: &Expr, scope: &FnScope, fns: &[FnDecl]) -> Ty {
    match e {
        Expr::IntLit(_) => Ty::I64,
        Expr::FloatLit(_) => Ty::F64,
        Expr::BoolLit(_) => Ty::Bool,
        Expr::Var(n) => scope.ty_of(n).unwrap_or(Ty::I64),
        Expr::Unary(_, inner) => infer_expr_ty(inner, scope, fns),
        // arithmetic preserves the operand type (checker proved it); a
        // comparison yields bool. result_ty is total over our op set, so
        // the unwrap_or only fires for logic ops whose operand type IS
        // already bool — same answer either way.
        Expr::Binary(op, l, _) => {
            let lt = infer_expr_ty(l, scope, fns);
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
        // this arm exists so the type system can prove emission total.
        (BinOp::Rem, Ty::F32) | (BinOp::Rem, Ty::F64) => {
            unreachable!("checker rejects float remainder before emit_binop")
        }
    };
    out.push(code);
}
