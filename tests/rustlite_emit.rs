//! wasm emission evals: the emitter's output is only real if a THIRD-PARTY
//! validator accepts it. every golden here round-trips through wasmparser's
//! validator (the same core bytecodealliance ships in wasm-tools), so a bad
//! byte sequence fails ci instead of trapping at cartridge-load time.
//! behavioral tests additionally EXECUTE the module semantics by replaying
//! its instruction stream through a tiny stack machine in this file — a
//! byte-level check independent of the L3 runtime (tests/runtime_evals.rs
//! covers the real interpreter); these pin observable arithmetic/control
//! behavior in the emitted bytes themselves.

use vanish::cartridges::rustlite::parse;
use vanish::cartridges::wasm::{check_fn, emit_module, uleb};

fn compile(src: &str) -> Vec<u8> {
    let program = parse(src).expect("parse");
    emit_module(&program).expect("emit")
}

/// the first error the whole pipeline (parse → check_program → check_fn)
/// reports for `src`.
fn refuse(src: &str) -> String {
    let program = parse(src).expect("parse");
    match emit_module(&program) {
        Ok(_) => panic!("expected a refusal for: {src}"),
        Err(e) => e.msg,
    }
}

// ---- leb128 ------------------------------------------------------------------

#[test]
fn uleb_matches_known_encodings() {
    // pinned against the spec examples: small values single-byte,
    // boundaries grow predictably.
    let cases: &[(u64, &[u8])] = &[
        (0, &[0x00]),
        (1, &[0x01]),
        (63, &[0x3f]),
        (64, &[0x40]),          // still one byte: boundary is 2^7, not 64
        (127, &[0x7f]),
        (128, &[0x80, 0x01]),   // spills to two bytes at 2^7
        (129, &[0x81, 0x01]),
        (16_383, &[0xff, 0x7f]),
        (16_384, &[0x80, 0x80, 0x01]),
        (624_485, &[0xe5, 0x8e, 0x26]), // the spec's canonical example
    ];
    for (value, expected) in cases {
        let mut out = Vec::new();
        uleb(*value, &mut out);
        assert_eq!(&out, expected, "uleb({value})");
    }
}

#[test]
fn section_ids_and_sizes_are_uleb_prefixed() {
    // walk the module's sections the way a parser would: id byte + uleb
    // size must land exactly on each section boundary. this catches padded
    // or fixed-width size fields, which would shift every later offset.
    let bytes = compile("fn f() -> i32 { return 0; }");
    let mut p = 8usize; // past magic + version
    let mut seen = Vec::new();
    while p < bytes.len() {
        let id = bytes[p];
        p += 1;
        let (size, consumed) = read_uleb(&bytes[p..]);
        p += consumed;
        let size = size as usize;
        assert!(
            p + size <= bytes.len(),
            "section {id} declares {size} bytes but only {} remain",
            bytes.len() - p
        );
        seen.push((id, size));
        p += size;
    }
    assert_eq!(p, bytes.len(), "sections must tile the module exactly");
    let ids: Vec<u8> = seen.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![1, 3, 10], "expected types/functions/code sections");
}

fn read_uleb(b: &[u8]) -> (u64, usize) {
    let mut v = 0u64;
    let mut shift = 0;
    for (i, &byte) in b.iter().enumerate() {
        v |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return (v, i + 1);
        }
        shift += 7;
    }
    panic!("unterminated uleb");
}



// ---- validation: every module parses AND validates -----------------------------

fn assert_valid(bytes: &[u8], label: &str) {
    let mut validator = wasmparser::Validator::new();
    validator
        .validate_all(bytes)
        .unwrap_or_else(|e| panic!("{label}: emitted module failed validation: {e}"));
}

#[test]
fn minimal_function_validates() {
    let bytes = compile("fn five() -> i32 { return 5; }");
    assert_valid(&bytes, "minimal");
    assert_eq!(&bytes[..4], b"\0asm", "magic");
    assert_eq!(&bytes[4..8], &[1, 0, 0, 0], "version");
}

#[test]
fn arithmetic_and_control_flow_validate() {
    let src = r#"
        fn count(n: i32) -> i32 {
            let i: i32 = 0;
            while i < n {
                i = i + 1;
            }
            return i;
        }
        fn double(x: i64) -> i64 {
            return x + x;
        }
        fn mix(a: i32, b: i32) -> bool {
            return a < b && a != 0 || b == 7;
        }
        fn polymul(a: f64, b: f64) -> f64 {
            let neg: f64 = -a;
            return neg * b;
        }
    "#;
    let bytes = compile(src);
    assert_valid(&bytes, "arithmetic+control");
}

#[test]
fn calls_between_functions_validate_including_forward_refs() {
    // g calls f BEFORE f appears — order-free resolution must hold.
    let src = r#"
        fn g() -> i64 { return f(21); }
        fn f(x: i64) -> i64 { return x * 2; }
    "#;
    let bytes = compile(src);
    assert_valid(&bytes, "forward-ref call");
}

#[test]
fn expression_statement_drops_call_results_to_stay_stack_balanced() {
    // calling a value-returning function as a statement would leave a
    // stray value on the stack; the emitter must drop it or validation dies.
    let src = r#"
        fn noisy() -> i64 { return 42; }
        fn main() -> i64 {
            noisy();
            return 0;
        }
    "#;
    let bytes = compile(src);
    assert_valid(&bytes, "drop-on-stmt-call");
}

// ---- checker refusals ----------------------------------------------------------

#[test]
fn undeclared_assignment_is_refused_by_the_checker() {
    let p = parse("fn f() -> i32 { x = 5; return 0; }").unwrap();
    let err = check_fn(&p.fns[0], &p).unwrap_err();
    assert!(err.msg.contains("not declared"), "{err:?}");
    assert!(err.msg.contains("let"), "{err:?}"); // names the fix
}

#[test]
fn type_mismatches_are_refused_with_both_types_named() {
    let p = parse("fn f() -> i64 { let x: i32 = 0; return x; }").unwrap();
    let err = check_fn(&p.fns[0], &p).unwrap_err();
    assert!(err.msg.contains("i32") && err.msg.contains("i64"), "{err:?}");
}

#[test]
fn while_condition_must_be_bool() {
    let p = parse("fn f(n: i32) -> i32 { while n { n = n - 1; } return n; }").unwrap();
    let err = check_fn(&p.fns[0], &p).unwrap_err();
    assert!(err.msg.contains("bool"), "{err:?}");
    let p = parse("fn f(n: i32) -> i32 { if n { n = 0; } return n; }").unwrap();
    let err = check_fn(&p.fns[0], &p).unwrap_err();
    assert!(err.msg.contains("if condition") && err.msg.contains("bool"), "{err:?}");
}

#[test]
fn arity_and_arg_types_of_calls_are_checked() {
    let p = parse(
        "fn f(a: i32) -> i32 { return a; } fn g() -> i32 { return f(1, 2); }",
    )
    .unwrap();
    let err = check_fn(&p.fns[1], &p).unwrap_err();
    assert!(err.msg.contains("takes 1") && err.msg.contains("got 2"), "{err:?}");

    let p =
        parse("fn f(a: i32) -> i32 { return a; } fn g(b: i64) -> i32 { return f(b); }").unwrap();
    let err = check_fn(&p.fns[1], &p).unwrap_err();
    assert!(err.msg.contains("argument 1"), "{err:?}");
}

#[test]
fn duplicate_params_refused() {
    let p = parse("fn f(a: i32, a: i32) -> i32 { return a; }").unwrap();
    let err = check_fn(&p.fns[0], &p).unwrap_err();
    assert!(err.msg.contains("duplicate"), "{err:?}");
}

#[test]
fn a_value_returning_function_must_end_in_return_on_every_path() {
    // wasm would reject the module for an empty stack at the function's
    // end — as a validator error three layers away. the checker says it at
    // the source instead, and knows an if/else with returning arms counts.
    let msg = refuse("fn f(x: i64) -> i64 { if x > 0 { return 1; } }");
    assert!(msg.contains("does not end with `return`"), "{msg}");
    let msg = refuse("fn f(x: i64) -> i64 { while x > 0 { return x; } }");
    assert!(msg.contains("does not end with `return`"), "{msg}");
    // both arms return → accepted, and the module validates.
    let bytes = compile(
        "fn max(a: i64, b: i64) -> i64 { if a > b { return a; } else { return b; } }",
    );
    assert_valid(&bytes, "if/else returning arms");
    // void functions need no trailing return.
    assert_valid(&compile("fn tick() { }"), "void without return");
}

#[test]
fn void_calls_and_bare_expressions_are_valid_statements() {
    // a void callee in statement position was a checker error before item 5
    // ("returns nothing but is used as a value") — which made `log(…);`
    // unwritable. and a bare value expression must be dropped, or the
    // module fails validation on a dirty stack.
    let src = r#"
        fn tick() { }
        fn seven() -> i64 { return 7; }
        fn main() -> i32 {
            tick();
            seven();
            1 + 2;
            return 0;
        }
    "#;
    assert_valid(&compile(src), "void call + dropped expression statements");
    // a void callee used as a VALUE is still refused, naming the callee.
    let msg = refuse("fn tick() { } fn f() -> i64 { let x: i64 = tick(); return x; }");
    assert!(msg.contains("tick") && msg.contains("returns nothing"), "{msg}");
}

// ---- cartridges: imports, exports, memory, intrinsics ----------------------------

/// a minimal but complete cartridge in rustlite: every ABI import, every
/// lifecycle export, memory intrinsics, and packing.
const CARTRIDGE: &str = r#"
    extern "C" {
        fn log(level: i32, ptr: i32, len: i32);
        fn now_ms() -> i64;
        fn store_get(k_ptr: i32, k_len: i32) -> i64;
        fn store_set(k_ptr: i32, k_len: i32, v_ptr: i32, v_len: i32) -> i32;
        fn emit(t_ptr: i32, t_len: i32, p_ptr: i32, p_len: i32);
    }
    pub fn cart_alloc(size: i32) -> i32 {
        let hp: i32 = load_i32(0);
        if hp == 0 { hp = 8; }
        store_i32(0, hp + size);
        return hp;
    }
    pub fn cart_init(p: i32, n: i32) -> i32 {
        log(1, p, n);
        return store_set(p, n, p, n);
    }
    pub fn cart_handle(p: i32, n: i32) -> i64 {
        let out: i32 = cart_alloc(n);
        let i: i32 = 0;
        while i < n {
            store_u8(out + i, load_u8(p + i) + 1);
            i = i + 1;
        }
        emit(p, n, out, n);
        let t: i64 = now_ms();
        let prev: i64 = store_get(p, n);
        let plen: i32 = unpack_len(prev) + unpack_ptr(prev) * 0;
        let pages: i32 = memory_size();
        return pack(out, n);
    }
"#;

#[test]
fn a_full_cartridge_validates_with_wasmparser() {
    // THE third-party check for the new sections: imports from `vanish`,
    // a fixed memory, function + memory exports, memory ops, i64 shifts —
    // all accepted by the same validator wasm-tools ships.
    let bytes = compile(CARTRIDGE);
    assert_valid(&bytes, "cartridge");
}

#[test]
fn cartridge_sections_are_emitted_in_spec_order() {
    // types, imports, functions, memory, exports, code — the binary format
    // mandates ascending ids, and a validator refuses any other order.
    let bytes = compile(CARTRIDGE);
    let mut p = 8usize;
    let mut ids = Vec::new();
    while p < bytes.len() {
        let id = bytes[p];
        p += 1;
        let (size, consumed) = read_uleb(&bytes[p..]);
        p += consumed + size as usize;
        ids.push(id);
    }
    assert_eq!(ids, vec![1, 2, 3, 5, 7, 10]);
    // pure modules keep the minimal layout: no memory, no exports.
    let pure = compile("fn f() -> i32 { return 0; }");
    assert!(!pure.windows(6).any(|w| w == b"memory"), "pure module exports no memory");
    // an intrinsic that touches memory pulls a memory in even with nothing
    // exported; pack alone does not.
    let mem = compile("fn f() -> i32 { return load_u8(4); }");
    assert!(mem.windows(6).any(|w| w == b"memory"), "load_u8 needs memory");
    let packed = compile("fn f() -> i64 { return pack(1, 2); }");
    assert!(!packed.windows(6).any(|w| w == b"memory"), "pack needs no memory");
    assert_valid(&mem, "memory-only");
    assert_valid(&packed, "pack-only");
}

#[test]
fn extern_declarations_are_checked_against_the_abi_table() {
    // unknown host function: refused, and the table is named so the fix is
    // one read away.
    let msg = refuse("extern \"C\" { fn sleep(ms: i64); } fn f() -> i32 { return 0; }");
    assert!(msg.contains("sleep") && msg.contains("now_ms") && msg.contains("store_get"), "{msg}");
    // right name, wrong shape: both shapes named.
    let msg = refuse("extern \"C\" { fn now_ms() -> i32; } fn f() -> i32 { return 0; }");
    assert!(msg.contains("now_ms") && msg.contains("fn() -> i64"), "{msg}");
    let msg = refuse("extern \"C\" { fn log(ptr: i32, len: i32); } fn f() -> i32 { return 0; }");
    assert!(msg.contains("fn(i32, i32, i32)"), "{msg}");
}

#[test]
fn lifecycle_exports_are_checked_against_the_abi_table() {
    let msg = refuse("pub fn cart_handle(p: i32) -> i64 { return 0; }");
    assert!(msg.contains("cart_handle") && msg.contains("fn(i32, i32) -> i64"), "{msg}");
    // a lifecycle name that is not pub can never be reached by the host.
    let msg = refuse("fn cart_init(p: i32, n: i32) -> i32 { return 0; }");
    assert!(msg.contains("must be `pub fn`"), "{msg}");
}

#[test]
fn names_are_unique_and_intrinsics_are_reserved() {
    let msg = refuse("fn f() -> i32 { return 0; } fn f() -> i32 { return 1; }");
    assert!(msg.contains("declared twice"), "{msg}");
    let msg = refuse("extern \"C\" { fn now_ms() -> i64; } fn now_ms() -> i64 { return 0; }");
    assert!(msg.contains("declared twice"), "{msg}");
    let msg = refuse("fn load_u8(a: i32) -> i32 { return 0; }");
    assert!(msg.contains("intrinsic"), "{msg}");
    let msg = refuse("extern \"C\" { fn pack(a: i32, b: i32) -> i64; } fn f() -> i32 { return 0; }");
    assert!(msg.contains("intrinsic"), "{msg}");
    // intrinsics are arity/type checked like any call.
    let msg = refuse("fn f() -> i32 { return load_u8(); }");
    assert!(msg.contains("takes 1"), "{msg}");
    let msg = refuse("fn f(x: i64) -> i32 { return load_u8(x); }");
    assert!(msg.contains("argument 1"), "{msg}");
}

#[test]
fn string_literals_validate_and_land_in_one_interned_data_segment() {
    let bytes = compile(r#"fn f() -> i64 { return "hello"; } fn g() -> i32 { return data_end(); }"#);
    assert_valid(&bytes, "strings");
    // types, functions, memory, exports (the memory is always exported
    // when it exists), code, data — the data section trails code.
    let mut p = 8usize;
    let mut ids = Vec::new();
    while p < bytes.len() {
        let id = bytes[p];
        p += 1;
        let (size, consumed) = read_uleb(&bytes[p..]);
        p += consumed + size as usize;
        ids.push(id);
    }
    assert_eq!(ids, vec![1, 3, 5, 7, 10, 11]);
    assert!(bytes.windows(5).any(|w| w == b"hello"), "the segment holds the bytes");

    // interned: two uses of one literal → one copy in the segment.
    let bytes = compile(r#"fn f() -> i64 { return "dup"; } fn g() -> i64 { return "dup"; }"#);
    assert_valid(&bytes, "interned");
    assert_eq!(bytes.windows(3).filter(|w| *w == b"dup").count(), 1);

    // a literal is i64 whatever the context asks for.
    let msg = refuse(r#"fn f() -> i32 { return "no"; }"#);
    assert!(msg.contains("i64") && msg.contains("i32"), "{msg}");

    // the whole blob must fit the guest memory — refused at compile time,
    // naming the size, not at decode three layers away.
    let big = "x".repeat(1_100_000);
    let msg = refuse(&format!("fn f() -> i64 {{ return \"{big}\"; }}"));
    assert!(msg.contains("does not fit"), "{msg}");
}

#[test]
fn if_else_shapes_all_validate() {
    let src = r#"
        fn a(x: i64) -> i64 { if x > 0 { x = 1; } return x; }
        fn b(x: i64) -> i64 { if x > 0 { return 1; } else { return 2; } }
        fn c(x: i64) -> i64 {
            if x < 0 { return -1; } else if x == 0 { return 0; } else { return 1; }
        }
        fn d(x: i64) -> i64 {
            let acc: i64 = 0;
            while x > 0 {
                if x % 2 == 0 { acc = acc + x; } else { acc = acc - 1; }
                x = x - 1;
            }
            return acc;
        }
    "#;
    assert_valid(&compile(src), "if/else shapes");
}

// ---- behavioral: replay the instruction stream ----------------------------------

/// a deliberately tiny stack machine covering exactly the opcodes rustlite
/// emits for pure integer code. NOT the L3 runtime — just enough to prove
/// the emitted BYTES mean what the source said.
fn eval_i64(bytes: &[u8], entry: usize, args: &[i64]) -> i64 {
    // locate the code section by walking sections properly (ids + sizes).
    let mut p = 8usize; // past magic+version
    let mut func_bodies: Vec<(usize, usize)> = Vec::new(); // (start, len)
    while p < bytes.len() {
        let id = bytes[p];
        p += 1;
        // read uleb size
        let mut size: u64 = 0;
        let mut shift = 0;
        loop {
            let b = bytes[p];
            p += 1;
            size |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let start = p;
        if id == 10 {
            // count functions, then record body spans
            let mut q = start;
            let mut n: u64 = 0;
            let mut shift = 0u32;
            loop {
                let b = bytes[q];
                q += 1;
                n |= ((b & 0x7f) as u64) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            for _ in 0..n {
                let mut blen: u64 = 0;
                shift = 0;
                loop {
                    let b = bytes[q];
                    q += 1;
                    blen |= ((b & 0x7f) as u64) << shift;
                    if b & 0x80 == 0 {
                        break;
                    }
                    shift += 7;
                }
                func_bodies.push((q, blen as usize));
                q += blen as usize;
            }
        }
        p = start + size as usize;
    }

    struct Vm<'a> {
        code: &'a [u8],
        ip: usize,
        stack: Vec<i64>,
        locals: Vec<i64>,
    }
    // control frames: (opcode kind, unused, loop-header ip). mirrors what a
    // real interpreter keeps — which is exactly why this harness can now
    // catch inverted branch semantics instead of rubber-stamping them.
    let mut frames: Vec<(u8, usize, usize)> = Vec::new();
    let (start, len) = func_bodies[entry];
    // skip the local declarations: read their total then advance
    let mut vm = Vm {
        code: &bytes[start..start + len],
        ip: 0,
        stack: Vec::new(),
        locals: args.to_vec(),
    };
    // skip decl-count + entries
    let ndecl = vm.code[vm.ip] as usize;
    vm.ip += 1;
    for _ in 0..ndecl {
        let count = vm.code[vm.ip];
        let ty = vm.code[vm.ip + 1];
        for _ in 0..count {
            vm.locals.push(0);
        }
        let _ = ty;
        vm.ip += 2;
    }
    loop {
        let op = vm.code[vm.ip];
        vm.ip += 1;
        match op {
            0x41 | 0x42 => {
                // i32/i64.const sleb
                let mut v: i64 = 0;
                let mut shift = 0;
                loop {
                    let b = vm.code[vm.ip];
                    vm.ip += 1;
                    v |= ((b & 0x7f) as i64) << shift;
                    shift += 7;
                    if b & 0x80 == 0 {
                        if shift < 64 && b & 0x40 != 0 {
                            v |= -1i64 << shift;
                        }
                        break;
                    }
                }
                vm.stack.push(v);
            }
            0x20 => {
                // local.get uleb
                let mut idx: u64 = 0;
                let mut shift = 0;
                loop {
                    let b = vm.code[vm.ip];
                    vm.ip += 1;
                    idx |= ((b & 0x7f) as u64) << shift;
                    if b & 0x80 == 0 {
                        break;
                    }
                    shift += 7;
                }
                let v = vm.locals[idx as usize];
                vm.stack.push(v);
            }
            0x21 => {
                let mut idx: u64 = 0;
                let mut shift = 0;
                loop {
                    let b = vm.code[vm.ip];
                    vm.ip += 1;
                    idx |= ((b & 0x7f) as u64) << shift;
                    if b & 0x80 == 0 {
                        break;
                    }
                    shift += 7;
                }
                let v = vm.stack.pop().unwrap();
                vm.locals[idx as usize] = v;
            }
            0x7c => {
                let b = vm.stack.pop().unwrap();
                let a = vm.stack.pop().unwrap();
                vm.stack.push(a.wrapping_add(b));
            }
            0x7d => {
                let b = vm.stack.pop().unwrap();
                let a = vm.stack.pop().unwrap();
                vm.stack.push(a.wrapping_sub(b));
            }
            0x7e => {
                let b = vm.stack.pop().unwrap();
                let a = vm.stack.pop().unwrap();
                vm.stack.push(a.wrapping_mul(b));
            }
            0x53 => {
                let b = vm.stack.pop().unwrap();
                let a = vm.stack.pop().unwrap();
                vm.stack.push((a < b) as i64);
            }
            0x51 => {
                let b = vm.stack.pop().unwrap();
                let a = vm.stack.pop().unwrap();
                vm.stack.push((a == b) as i64);
            }
            0x45 => {
                let a = vm.stack.pop().unwrap();
                vm.stack.push((a == 0) as i64);
            }
            // real label semantics: depth 0 = innermost frame, growing
            // outward. branching to a LOOP restarts it; branching to a
            // BLOCK exits it, jumping past EVERY end from the innermost
            // frame through the target's own (each control frame owns
            // exactly one end byte). getting either half wrong produces
            // modules that validate but misbehave — which is why this vm
            // keeps real frames instead of pattern-matching shapes.
            0x02 | 0x03 => {
                let kind = op;
                let _blocktype = vm.code[vm.ip]; // always void 0x40 for us
                vm.ip += 1;
                frames.push((kind, 0, vm.ip)); // header = first body byte
            }
            0x0d => {
                let depth = vm.code[vm.ip] as usize;
                vm.ip += 1;
                let taken = vm.stack.pop().unwrap() != 0;
                if taken {
                    let n = frames.len() - 1 - depth;
                    let (kind, _, header) = frames[n];
                    if kind == 0x03 {
                        vm.ip = header; // loop: RESTART
                    } else {
                        // block exit: skip every end from innermost through
                        // the target frame's own (each frame owns one closer)
                        let mut remaining = frames.len() - n;
                        while remaining > 0 && vm.ip < vm.code.len() {
                            match vm.code[vm.ip] {
                                0x02 | 0x03 => remaining += 1,
                                0x0b => remaining -= 1,
                                _ => {}
                            }
                            vm.ip += 1;
                        }
                        frames.truncate(n);
                    }
                }
            }
            0x0c => {
                let depth = vm.code[vm.ip] as usize;
                vm.ip += 1;
                let n = frames.len() - 1 - depth;
                let (kind, _, header) = frames[n];
                if kind == 0x03 {
                    vm.ip = header; // loop: continue
                } else {
                    let mut remaining = frames.len() - n;
                    while remaining > 0 && vm.ip < vm.code.len() {
                        match vm.code[vm.ip] {
                            0x02 | 0x03 => remaining += 1,
                            0x0b => remaining -= 1,
                            _ => {}
                        }
                        vm.ip += 1;
                    }
                    frames.truncate(n);
                }
            }
            0x0b => {
                // a closer belongs to the innermost OPEN frame; when none
                // are open, this is the function body's own end.
                if let Some(frame) = frames.pop() {
                    let _ = frame;
                } else {
                    return *vm.stack.last().unwrap_or(&0);
                }
            }
            0x0f => {
                return *vm.stack.last().unwrap_or(&0);
            }
            other => panic!("vm: unhandled opcode {other:#04x}"),
        }
    }
}

#[test]
fn emitted_arithmetic_actually_computes() {
    // fn double(x: i64) -> i64 { return x + x; }
    let bytes = compile("fn double(x: i64) -> i64 { return x + x; }");
    assert_valid(&bytes, "double");
    // entry 0 is `double`, one param.
    let result = eval_i64(&bytes, 0, &[21]);
    assert_eq!(result, 42, "double(21) must be 42 in the emitted bytes");
}

#[test]
fn emitted_while_loop_actually_iterates() {
    // fn count(n: i32) -> i32 { let i: i32 = 0; while i < n { i = i + 1; } return i; }
    // exercised at i64 opcode level via an i64 twin to reuse the mini-vm.
    let bytes = compile(
        "fn count(n: i64) -> i64 { let i: i64 = 0; while i < n { i = i + 1; } return i; }",
    );
    assert_valid(&bytes, "count");
    let result = eval_i64(&bytes, 0, &[5]);
    assert_eq!(result, 5, "count(5) must iterate the emitted loop to 5");
}
