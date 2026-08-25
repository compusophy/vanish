//! lifecycle evals (CARTRIDGE_PLAN §11 item 5): a cartridge written in
//! rustlite, compiled by wasm.rs, decoded and run by runtime.rs, wired to a
//! recording fake host through the L1 ABI — load → init → handle, every
//! host function exercised through guest memory, every refusal at the door
//! named. no layer is mocked; the fake host is the only test double, and it
//! is the same trait the browser implements.

use std::collections::BTreeMap;

use vanish::cartridges::runtime::{Trap, MAX_HOST_REENTRY};
use vanish::cartridges::{
    rustlite::parse, wasm::emit_module, CallError, Cartridge, CartridgeKind, CartridgeManifest,
    Host, ABI_VERSION,
};

// ---- the fake host -------------------------------------------------------------

#[derive(Default)]
struct FakeHost {
    logs: Vec<(i32, Vec<u8>)>,
    kv: BTreeMap<Vec<u8>, Vec<u8>>,
    emitted: Vec<(Vec<u8>, Vec<u8>)>,
    now: i64,
    now_calls: u32,
    refuse_set: bool,
    fail_emit: bool,
    /// answer EVERY store_get with this value (for the re-entry bomb).
    always_get: Option<Vec<u8>>,
}

impl Host for FakeHost {
    fn log(&mut self, level: i32, msg: &[u8]) {
        self.logs.push((level, msg.to_vec()));
    }
    fn now_ms(&mut self) -> i64 {
        self.now_calls += 1;
        self.now
    }
    fn store_get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        if let Some(v) = &self.always_get {
            return Ok(Some(v.clone()));
        }
        Ok(self.kv.get(key).cloned())
    }
    fn store_set(&mut self, key: &[u8], value: &[u8]) -> Result<(), String> {
        if self.refuse_set {
            return Err("quota exceeded".into());
        }
        self.kv.insert(key.to_vec(), value.to_vec());
        Ok(())
    }
    fn emit(&mut self, topic: &[u8], payload: &[u8]) -> Result<(), String> {
        if self.fail_emit {
            return Err("bus down".into());
        }
        self.emitted.push((topic.to_vec(), payload.to_vec()));
        Ok(())
    }
}

fn manifest() -> CartridgeManifest {
    CartridgeManifest {
        slug: "echo".to_string(),
        kind: CartridgeKind::Backend,
        version: "0.1.0".to_string(),
        abi_version: ABI_VERSION,
        provides: vec![],
        requires: vec![],
    }
}

fn compile(src: &str) -> Vec<u8> {
    let program = parse(src).expect("parse");
    emit_module(&program).expect("emit")
}

fn load(src: &str, host: FakeHost) -> Cartridge<FakeHost> {
    Cartridge::load(manifest(), &compile(src), host).expect("load")
}

/// the bump allocator every test cartridge shares: the heap pointer lives
/// at address 0 (zero on a fresh memory), the heap starts at 8.
const ALLOC: &str = r#"
    pub fn cart_alloc(size: i32) -> i32 {
        let hp: i32 = load_i32(0);
        if hp == 0 { hp = 8; }
        store_i32(0, hp + size);
        return hp;
    }
"#;

/// echo: init logs its config and stores config → config; handle copies
/// the message into a fresh buffer with every byte + 1, emits (msg → out),
/// reads the clock, logs the answer, and returns it packed.
fn echo_src() -> String {
    format!(
        r#"
        extern "C" {{
            fn log(level: i32, ptr: i32, len: i32);
            fn now_ms() -> i64;
            fn store_get(k_ptr: i32, k_len: i32) -> i64;
            fn store_set(k_ptr: i32, k_len: i32, v_ptr: i32, v_len: i32) -> i32;
            fn emit(t_ptr: i32, t_len: i32, p_ptr: i32, p_len: i32);
        }}
        {ALLOC}
        pub fn cart_init(cfg_ptr: i32, cfg_len: i32) -> i32 {{
            log(1, cfg_ptr, cfg_len);
            return store_set(cfg_ptr, cfg_len, cfg_ptr, cfg_len);
        }}
        pub fn cart_handle(msg_ptr: i32, msg_len: i32) -> i64 {{
            let out: i32 = cart_alloc(msg_len);
            let i: i32 = 0;
            while i < msg_len {{
                store_u8(out + i, load_u8(msg_ptr + i) + 1);
                i = i + 1;
            }}
            emit(msg_ptr, msg_len, out, msg_len);
            let t: i64 = now_ms();
            log(0, out, msg_len);
            return pack(out, msg_len);
        }}
    "#
    )
}

/// kv: handle answers with whatever the store holds under key = message.
fn kv_src() -> String {
    format!(
        r#"
        extern "C" {{ fn store_get(k_ptr: i32, k_len: i32) -> i64; }}
        {ALLOC}
        pub fn cart_init(p: i32, n: i32) -> i32 {{ return 0; }}
        pub fn cart_handle(p: i32, n: i32) -> i64 {{ return store_get(p, n); }}
    "#
    )
}

// ---- the happy path ---------------------------------------------------------------

#[test]
fn echo_cartridge_runs_the_full_lifecycle() {
    let mut cart = load(&echo_src(), FakeHost { now: 1_700_000_000_000, ..Default::default() });
    assert!(!cart.is_initialized());

    cart.init(b"cfg=1", 10_000).expect("init");
    assert!(cart.is_initialized());
    assert_eq!(cart.host().logs, vec![(1, b"cfg=1".to_vec())], "init logged its config");
    assert_eq!(cart.host().kv.get(&b"cfg=1"[..]), Some(&b"cfg=1".to_vec()), "config stored");

    let out = cart.handle(b"abc", 10_000).expect("handle");
    assert_eq!(out, b"bcd", "every byte + 1, through guest memory both ways");
    let host = cart.host();
    assert_eq!(host.emitted, vec![(b"abc".to_vec(), b"bcd".to_vec())]);
    assert_eq!(host.now_calls, 1);
    assert_eq!(host.logs[1], (0, b"bcd".to_vec()));

    // a second message: the bump allocator keeps moving, results stay right.
    let out = cart.handle(b"HAL", 10_000).expect("handle 2");
    assert_eq!(out, b"IBM");
    assert_eq!(cart.host().emitted.len(), 2);

    // an empty message is a legal message.
    let out = cart.handle(b"", 10_000).expect("empty");
    assert_eq!(out, b"");
}

#[test]
fn store_get_round_trips_the_value_through_guest_memory() {
    // the host writes the value INTO guest memory via cart_alloc and the
    // guest hands it straight back — proving the packed pointer names real,
    // readable bytes and that the re-entry path works at depth 1.
    let mut host = FakeHost::default();
    host.kv.insert(b"k".to_vec(), b"value-for-k".to_vec());
    let mut cart = load(&kv_src(), host);
    cart.init(b"", 1000).unwrap();
    assert_eq!(cart.handle(b"k", 10_000).unwrap(), b"value-for-k");
    // a miss is packed 0 → (0, 0) → an empty answer, not an error.
    assert_eq!(cart.handle(b"missing", 10_000).unwrap(), b"");
}

// ---- lifecycle rules ------------------------------------------------------------------

#[test]
fn handle_before_init_is_refused() {
    let mut cart = load(&echo_src(), FakeHost::default());
    assert_eq!(cart.handle(b"x", 1000).unwrap_err(), CallError::NotInitialized);
    assert!(cart.host().logs.is_empty(), "nothing ran");
}

#[test]
fn a_nonzero_init_status_is_surfaced_verbatim_and_store_set_refusal_is_a_status() {
    // the echo cartridge returns store_set's status from cart_init, so a
    // host that refuses writes makes init report 1 — through the guest,
    // as data, not as a trap. two rules in one observation.
    let mut cart = load(&echo_src(), FakeHost { refuse_set: true, ..Default::default() });
    assert_eq!(cart.init(b"cfg", 10_000).unwrap_err(), CallError::Refused(1));
    assert!(!cart.is_initialized());
    assert_eq!(cart.host().logs.len(), 1, "init ran up to the refusal");
}

#[test]
fn fuel_exhaustion_inside_a_call_is_a_named_trap() {
    let mut cart = load(&echo_src(), FakeHost::default());
    assert_eq!(cart.init(b"c", 3).unwrap_err(), CallError::Trap(Trap::FuelExhausted));
    // and the cartridge is still usable with a real budget afterwards.
    cart.init(b"c", 10_000).unwrap();
    assert_eq!(cart.handle(b"a", 10_000).unwrap(), b"b");
    assert_eq!(cart.handle(b"a", 20).unwrap_err(), CallError::Trap(Trap::FuelExhausted));
}

#[test]
fn an_emit_the_host_cannot_deliver_traps_the_guest() {
    let mut cart = load(&echo_src(), FakeHost { fail_emit: true, ..Default::default() });
    cart.init(b"c", 10_000).unwrap();
    let err = cart.handle(b"a", 10_000).unwrap_err();
    assert!(
        matches!(err, CallError::Trap(Trap::HostError(ref m)) if m.contains("bus down")),
        "{err:?}"
    );
}

// ---- hostile guests -------------------------------------------------------------------

#[test]
fn a_packed_response_outside_memory_is_a_bad_response_not_a_read() {
    let src = format!(
        "{ALLOC} pub fn cart_init(p: i32, n: i32) -> i32 {{ return 0; }} \
         pub fn cart_handle(p: i32, n: i32) -> i64 {{ return pack(2000000000, 16); }}"
    );
    let mut cart = load(&src, FakeHost::default());
    cart.init(b"", 1000).unwrap();
    let err = cart.handle(b"x", 1000).unwrap_err();
    assert!(matches!(err, CallError::BadResponse(ref m) if m.contains("outside")), "{err:?}");
    // a length that overflows when added to a high pointer is also caught.
    let src = format!(
        "{ALLOC} pub fn cart_init(p: i32, n: i32) -> i32 {{ return 0; }} \
         pub fn cart_handle(p: i32, n: i32) -> i64 {{ return pack(-1, -1); }}"
    );
    let mut cart = load(&src, FakeHost::default());
    cart.init(b"", 1000).unwrap();
    assert!(matches!(cart.handle(b"x", 1000).unwrap_err(), CallError::BadResponse(_)));
}

#[test]
fn an_allocator_answering_outside_memory_is_refused_named() {
    let src = "pub fn cart_alloc(size: i32) -> i32 { return 2000000000; } \
               pub fn cart_init(p: i32, n: i32) -> i32 { return 0; } \
               pub fn cart_handle(p: i32, n: i32) -> i64 { return 0; }";
    let mut cart = load(src, FakeHost::default());
    let err = cart.init(b"cfg", 1000).unwrap_err();
    assert!(
        matches!(err, CallError::Trap(Trap::HostError(ref m)) if m.contains("cart_alloc")),
        "{err:?}"
    );
    // a negative pointer is a huge unsigned one — same refusal, no wrap.
    let src = "pub fn cart_alloc(size: i32) -> i32 { return -8; } \
               pub fn cart_init(p: i32, n: i32) -> i32 { return 0; } \
               pub fn cart_handle(p: i32, n: i32) -> i64 { return 0; }";
    let mut cart = load(src, FakeHost::default());
    assert!(matches!(cart.init(b"cfg", 1000).unwrap_err(), CallError::Trap(Trap::HostError(_))));
}

#[test]
fn host_reentry_is_bounded_not_a_native_stack_overflow() {
    // cart_alloc calls store_get; the host always answers; answering needs
    // cart_alloc; … the bound turns the cycle into a named trap.
    let src = r#"
        extern "C" { fn store_get(k_ptr: i32, k_len: i32) -> i64; }
        pub fn cart_alloc(size: i32) -> i32 {
            let v: i64 = store_get(0, 1);
            return 8;
        }
        pub fn cart_init(p: i32, n: i32) -> i32 { return 0; }
        pub fn cart_handle(p: i32, n: i32) -> i64 { return 0; }
    "#;
    let mut cart = load(src, FakeHost { always_get: Some(vec![1]), ..Default::default() });
    let err = cart.init(b"c", 1_000_000).unwrap_err();
    assert!(
        matches!(err, CallError::Trap(Trap::HostError(ref m)) if m.contains("re-entry") && m.contains(&MAX_HOST_REENTRY.to_string())),
        "{err:?}"
    );
}

// ---- refusals at the door --------------------------------------------------------------

#[test]
fn an_import_outside_the_abi_is_refused_at_load() {
    // the compiler refuses unknown externs, so the hostile module is made
    // by hand: rename the `now_ms` import bytes to `now_xs` (same length →
    // every section size still tiles). it decodes; it must not LOAD.
    let mut bytes = compile(&echo_src());
    let pos = bytes
        .windows(6)
        .position(|w| w == b"now_ms")
        .expect("import name present once");
    bytes[pos..pos + 6].copy_from_slice(b"now_xs");
    let err = Cartridge::load(manifest(), &bytes, FakeHost::default()).unwrap_err();
    assert!(err.0.contains("now_xs") && err.0.contains("now_ms"), "{err}");
}

#[test]
fn a_missing_lifecycle_export_is_refused_at_load_naming_it() {
    let src = format!(
        "extern \"C\" {{ fn now_ms() -> i64; }} {ALLOC} \
         pub fn cart_init(p: i32, n: i32) -> i32 {{ return 0; }}"
    );
    let err = Cartridge::load(manifest(), &compile(&src), FakeHost::default()).unwrap_err();
    assert!(err.0.contains("cart_handle") && err.0.contains("fn(i32, i32) -> i64"), "{err}");
    // a pure module is not a cartridge at all.
    let err = Cartridge::load(manifest(), &compile("fn f() -> i32 { return 0; }"), FakeHost::default())
        .unwrap_err();
    assert!(err.0.contains("cart_init"), "{err}");
}

#[test]
fn a_module_without_an_exported_memory_is_refused_at_load() {
    let mut bytes = compile(&echo_src());
    let pos = bytes
        .windows(6)
        .position(|w| w == b"memory")
        .expect("memory export present");
    bytes[pos..pos + 6].copy_from_slice(b"memorx");
    let err = Cartridge::load(manifest(), &bytes, FakeHost::default()).unwrap_err();
    assert!(err.0.contains("memory"), "{err}");
}

#[test]
fn the_manifest_abi_gate_applies_at_load() {
    let mut m = manifest();
    m.abi_version = ABI_VERSION + 1;
    let err = Cartridge::load(m, &compile(&echo_src()), FakeHost::default()).unwrap_err();
    assert!(err.0.contains("manifest") && err.0.contains("newer"), "{err}");
    let err = Cartridge::load(manifest(), b"not wasm", FakeHost::default()).unwrap_err();
    assert!(err.0.contains("decode") && err.0.contains("magic"), "{err}");
}

// ---- THE FUZZ, lifecycle edition ------------------------------------------------------

#[test]
fn every_single_byte_corruption_of_a_cartridge_loads_or_refuses_without_panic() {
    // load (may refuse), init (may fail), handle (may fail): nothing panics,
    // whatever the bytes say. the same structural property the runtime
    // fuzz pins, now across the host boundary and the allocator round trip.
    let bytes = compile(&echo_src());
    for pos in 0..bytes.len() {
        for delta in [1u8, 0x7f, 0x80] {
            let mut mutated = bytes.clone();
            mutated[pos] = mutated[pos].wrapping_add(delta);
            let host = FakeHost { always_get: Some(b"v".to_vec()), ..Default::default() };
            if let Ok(mut cart) = Cartridge::load(manifest(), &mutated, host) {
                if cart.init(b"cfg", 5_000).is_ok() {
                    let _ = cart.handle(b"abc", 5_000);
                    let _ = cart.handle(b"", 5_000);
                }
            }
        }
    }
}
