//! shared cartridge test fixtures: the recording fake host (the ONLY test
//! double — it implements the same trait the browser will), the compile
//! helper, and rustlite sources every suite reuses. lives in a subdirectory
//! on purpose: ci/run_tests.sh discovers suites as tests/*.rs, and a helper
//! module must not masquerade as one.

#![allow(dead_code)]

use std::collections::BTreeMap;

use vanish::cartridges::{
    rustlite::parse, wasm::emit_module, CartridgeKind, CartridgeManifest, Host, ABI_VERSION,
};

#[derive(Default)]
pub struct FakeHost {
    pub logs: Vec<(i32, Vec<u8>)>,
    pub kv: BTreeMap<Vec<u8>, Vec<u8>>,
    pub emitted: Vec<(Vec<u8>, Vec<u8>)>,
    pub now: i64,
    pub now_calls: u32,
    pub refuse_set: bool,
    pub fail_emit: bool,
    /// answer EVERY store_get with this value (for the re-entry bomb).
    pub always_get: Option<Vec<u8>>,
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

pub fn port(name: &str) -> vanish::cartridges::manifest::Port {
    vanish::cartridges::manifest::Port {
        name: name.to_string(),
    }
}

/// a valid backend manifest with no ports.
pub fn manifest(slug: &str) -> CartridgeManifest {
    CartridgeManifest {
        slug: slug.to_string(),
        kind: CartridgeKind::Backend,
        version: "0.1.0".to_string(),
        abi_version: ABI_VERSION,
        provides: vec![],
        requires: vec![],
    }
}

/// a manifest with ports, for wiring tests.
pub fn manifest_with(slug: &str, provides: &[&str], requires: &[&str]) -> CartridgeManifest {
    let mut m = manifest(slug);
    m.provides = provides.iter().map(|p| port(p)).collect();
    m.requires = requires.iter().map(|p| port(p)).collect();
    m
}

pub fn compile(src: &str) -> Vec<u8> {
    let program = parse(src).expect("parse");
    emit_module(&program).expect("emit")
}

/// the bump allocator every test cartridge shares: the heap pointer lives
/// at address 0 (zero on a fresh memory), the heap starts past the string
/// data segment (`data_end()`, DATA_BASE when there are no literals).
pub const ALLOC: &str = r#"
    pub fn cart_alloc(size: i32) -> i32 {
        let hp: i32 = load_i32(0);
        if hp == 0 { hp = data_end(); }
        store_i32(0, hp + size);
        return hp;
    }
"#;

/// echo: init logs its config and stores config → config; handle copies
/// the message into a fresh buffer with every byte + 1, emits (msg → out),
/// reads the clock, logs the answer, and returns it packed.
pub fn echo_src() -> String {
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
pub fn kv_src() -> String {
    format!(
        r#"
        extern "C" {{ fn store_get(k_ptr: i32, k_len: i32) -> i64; }}
        {ALLOC}
        pub fn cart_init(p: i32, n: i32) -> i32 {{ return 0; }}
        pub fn cart_handle(p: i32, n: i32) -> i64 {{ return store_get(p, n); }}
    "#
    )
}

/// a cartridge that traps on EVERY message (memory out of bounds), logging
/// its config at init so restarts are countable through the host.
pub fn crasher_src() -> String {
    format!(
        r#"
        extern "C" {{ fn log(level: i32, ptr: i32, len: i32); }}
        {ALLOC}
        pub fn cart_init(p: i32, n: i32) -> i32 {{ log(1, p, n); return 0; }}
        pub fn cart_handle(p: i32, n: i32) -> i64 {{
            let boom: i32 = load_i32(2000000000);
            return pack(p, n);
        }}
    "#
    )
}

/// a cartridge that traps only on an EMPTY message and echoes otherwise.
pub fn flaky_src() -> String {
    format!(
        r#"
        {ALLOC}
        pub fn cart_init(p: i32, n: i32) -> i32 {{ return 0; }}
        pub fn cart_handle(p: i32, n: i32) -> i64 {{
            if n == 0 {{ let boom: i32 = load_i32(2000000000); }}
            return pack(p, n);
        }}
    "#
    )
}

/// a cartridge that, on every message, emits that message on each of
/// `topics` (string literals — their bytes live in the data segment) and
/// then echoes it back.
pub fn emitter_src(topics: &[&str]) -> String {
    let mut body = String::new();
    for topic in topics {
        body.push_str(&format!(
            "emit(unpack_ptr(\"{topic}\"), unpack_len(\"{topic}\"), p, n);\n"
        ));
    }
    format!(
        r#"
        extern "C" {{ fn emit(t_ptr: i32, t_len: i32, p_ptr: i32, p_len: i32); }}
        {ALLOC}
        pub fn cart_init(p: i32, n: i32) -> i32 {{ return 0; }}
        pub fn cart_handle(p: i32, n: i32) -> i64 {{
            {body}
            return pack(p, n);
        }}
    "#
    )
}

/// a byte-shifting cartridge: every byte of the message + `delta`, logging
/// its config at init so boot order is observable through the host.
pub fn shift_src(delta: i32) -> String {
    format!(
        r#"
        extern "C" {{ fn log(level: i32, ptr: i32, len: i32); }}
        {ALLOC}
        pub fn cart_init(p: i32, n: i32) -> i32 {{ log(1, p, n); return 0; }}
        pub fn cart_handle(msg_ptr: i32, msg_len: i32) -> i64 {{
            let out: i32 = cart_alloc(msg_len);
            let i: i32 = 0;
            while i < msg_len {{
                store_u8(out + i, load_u8(msg_ptr + i) + {delta});
                i = i + 1;
            }}
            return pack(out, msg_len);
        }}
    "#
    )
}
