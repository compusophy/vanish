//! wasm emission evals: the emitter's output is only real if a THIRD-PARTY
//! validator accepts it. every golden here round-trips through wasmparser's
//! validator (the same core bytecodealliance ships in wasm-tools), so a bad
//! byte sequence fails ci instead of trapping at cartridge-load time.
//! behavioral tests additionally EXECUTE the module semantics by replaying
//! its instruction stream through a tiny stack machine in this file — the
//! full interpreter is L3 and does not exist yet; these pin observable
//! arithmetic/control behavior at the byte level until it does.

use vanish::cartridges::rustlite::parse;
use vanish::cartridges::wasm::{check_fn, emit_module, uleb};

fn compile(src: &str) -> Vec<u8> {
    let fns = parse(src).expect("parse");
    emit_module(&fns).expect("emit")
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
    let fns = parse("fn f() -> i32 { x = 5; return 0; }").unwrap();
    let err = check_fn(&fns[0], &fns).unwrap_err();
    assert!(err.msg.contains("not declared"), "{err:?}");
    assert!(err.msg.contains("let"), "{err:?}"); // names the fix
}

#[test]
fn type_mismatches_are_refused_with_both_types_named() {
    let fns = parse("fn f() -> i64 { let x: i32 = 0; return x; }").unwrap();
    let err = check_fn(&fns[0], &fns).unwrap_err();
    assert!(err.msg.contains("i32") && err.msg.contains("i64"), "{err:?}");
}

#[test]
fn while_condition_must_be_bool() {
    let fns = parse("fn f(n: i32) -> i32 { while n { n = n - 1; } return n; }").unwrap();
    let err = check_fn(&fns[0], &fns).unwrap_err();
    assert!(err.msg.contains("bool"), "{err:?}");
}

#[test]
fn arity_and_arg_types_of_calls_are_checked() {
    let fns = parse(
        "fn f(a: i32) -> i32 { return a; } fn g() -> i32 { return f(1, 2); }",
    )
    .unwrap();
    let err = check_fn(&fns[1], &fns).unwrap_err();
    assert!(err.msg.contains("takes 1") && err.msg.contains("got 2"), "{err:?}");

    let fns =
        parse("fn f(a: i32) -> i32 { return a; } fn g(b: i64) -> i32 { return f(b); }").unwrap();
    let err = check_fn(&fns[1], &fns).unwrap_err();
    assert!(err.msg.contains("argument 1"), "{err:?}");
}

#[test]
fn duplicate_params_refused() {
    let fns = parse("fn f(a: i32, a: i32) -> i32 { return a; }").unwrap();
    let err = check_fn(&fns[0], &fns).unwrap_err();
    assert!(err.msg.contains("duplicate"), "{err:?}");
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
