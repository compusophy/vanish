//! an in-memory `Host` with write-behind bookkeeping — the host the worker
//! gives each cartridge.
//!
//! why in-memory: the `Host` trait is synchronous (the interpreter cannot
//! suspend), but the browser's durable store (opfs) is async. so a
//! cartridge's kv lives here during a step, and the worker FLUSHES what
//! changed (`take_flush`) to `vanish-cartridges/{slug}/kv.json` right after
//! the step — the same write-behind shape as the transcript checkpoint
//! drain. D2 holds because the flush is part of every step, not an
//! afterthought; the window in which work exists only in memory is one
//! pump.
//!
//! logs and emits are collected the same way (`take_logs`, `take_emits`),
//! so the worker can render them as feed notes without the host ever
//! touching the dom. time is SET by the worker per step (`set_now`) — the
//! host reads no clock (D1).

use std::collections::{BTreeMap, BTreeSet};

use super::abi::Host;

#[derive(Debug, Default)]
pub struct MemHost {
    pub slug: String,
    pub kv: BTreeMap<Vec<u8>, Vec<u8>>,
    dirty: BTreeSet<Vec<u8>>,
    logs: Vec<(i32, String)>,
    emits: Vec<(String, Vec<u8>)>,
    now_ms: i64,
}

impl MemHost {
    pub fn new(slug: &str) -> Self {
        Self {
            slug: slug.to_string(),
            ..Default::default()
        }
    }

    /// seed the kv from durable storage at boot (the worker reads
    /// `kv_path(slug)` and hands the pairs over). seeded keys are not dirty
    /// — they are already on disk.
    pub fn seed(&mut self, pairs: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>) {
        for (k, v) in pairs {
            self.kv.insert(k, v);
        }
    }

    pub fn set_now(&mut self, now_ms: i64) {
        self.now_ms = now_ms;
    }

    /// every key written since the last take, with its current value —
    /// what the worker flushes to opfs after a step.
    pub fn take_dirty(&mut self) -> KvPairs {
        let keys = std::mem::take(&mut self.dirty);
        keys.into_iter()
            .filter_map(|k| self.kv.get(&k).map(|v| (k.clone(), v.clone())))
            .collect()
    }

    pub fn take_logs(&mut self) -> Vec<(i32, String)> {
        std::mem::take(&mut self.logs)
    }

    pub fn take_emits(&mut self) -> Vec<(String, Vec<u8>)> {
        std::mem::take(&mut self.emits)
    }
}

impl Host for MemHost {
    fn log(&mut self, level: i32, msg: &[u8]) {
        self.logs
            .push((level, String::from_utf8_lossy(msg).into_owned()));
    }
    fn now_ms(&mut self) -> i64 {
        self.now_ms
    }
    fn store_get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        Ok(self.kv.get(key).cloned())
    }
    fn store_set(&mut self, key: &[u8], value: &[u8]) -> Result<(), String> {
        self.kv.insert(key.to_vec(), value.to_vec());
        self.dirty.insert(key.to_vec());
        Ok(())
    }
    fn emit(&mut self, topic: &[u8], payload: &[u8]) -> Result<(), String> {
        self.emits
            .push((String::from_utf8_lossy(topic).into_owned(), payload.to_vec()));
        Ok(())
    }
    /// routing between cartridges is the orchestrator's job (its
    /// ActorHost wraps this one); a bare MemHost mediates nothing.
    fn call(&mut self, _port: &[u8], _msg: &[u8], _fuel: &mut u64) -> Result<Option<Vec<u8>>, String> {
        Ok(None)
    }
}

/// where a cartridge's durable kv lives. NOT under a plausible source path:
/// opfs's `repo/` root is shared with the working tree, and a real
/// `cartridges/` directory in this repository would then collide with the
/// runtime's own state. the transcript and the config mirror use the same
/// `vanish-` prefix for the same reason.
pub fn kv_path(slug: &str) -> String {
    format!("vanish-cartridges/{slug}/kv.json")
}

/// where a swapped-in policy's source and manifest are remembered, so the
/// next boot brings the cartridge the user last chose rather than the
/// built-in reference one.
pub fn source_path(slug: &str) -> String {
    format!("vanish-cartridges/{slug}/source.rustlite")
}

pub fn manifest_path(slug: &str) -> String {
    format!("vanish-cartridges/{slug}/manifest.json")
}

/// a cartridge's store as ordered (key, value) pairs — the unit `snapshot`,
/// `take_dirty` and `decode_kv` all speak in.
pub type KvPairs = Vec<(Vec<u8>, Vec<u8>)>;

/// one pending durable write for a cartridge's kv: what to write, where,
/// and which keys caused it.
///
/// the whole map is written rather than the dirty keys alone because opfs
/// has no directory iteration this project is willing to depend on (the
/// working tree keeps an index file for exactly that reason), so a
/// cartridge's kv is ONE file. `keys` is still the dirty set: it is what
/// the flush is triggered by and what a note names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvFlush {
    pub slug: String,
    pub path: String,
    /// the keys written since the last flush, lossily rendered for a note.
    pub keys: Vec<String>,
    pub body: String,
}

impl MemHost {
    /// the whole kv, in key order — what a flush actually writes.
    pub fn snapshot(&self) -> KvPairs {
        self.kv.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// what must reach opfs after this step, or None when nothing changed.
    /// takes the dirty set, so a key is reported once per change.
    pub fn take_flush(&mut self) -> Option<KvFlush> {
        let dirty = self.take_dirty();
        if dirty.is_empty() {
            return None;
        }
        Some(KvFlush {
            slug: self.slug.clone(),
            path: kv_path(&self.slug),
            keys: dirty
                .iter()
                .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
                .collect(),
            body: encode_kv(&self.snapshot()),
        })
    }

    /// seed from what a previous session flushed. a store that does not
    /// parse is reported, never silently ignored (D4) — the caller decides
    /// whether to start empty or refuse.
    pub fn seed_encoded(&mut self, text: &str) -> Result<usize, String> {
        let pairs = decode_kv(text)?;
        let n = pairs.len();
        self.seed(pairs);
        Ok(n)
    }
}

/// the on-disk format. bumped when the shape below changes; a store
/// written by a newer build is refused rather than half-read.
pub const KV_FORMAT_VERSION: u64 = 1;

/// kv keys and values are arbitrary bytes and opfs stores text, so both are
/// hex. json cannot key a map by bytes either, which rules out the obvious
/// `{key: value}` shape; a list of pairs keeps the store ordered and makes
/// the encoding one code path instead of "text when it happens to decode".
pub fn encode_kv(pairs: &[(Vec<u8>, Vec<u8>)]) -> String {
    let rows: Vec<[String; 2]> = pairs
        .iter()
        .map(|(k, v)| [to_hex(k), to_hex(v)])
        .collect();
    serde_json::json!({ "version": KV_FORMAT_VERSION, "pairs": rows }).to_string()
}

pub fn decode_kv(text: &str) -> Result<KvPairs, String> {
    let doc: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("cartridge kv is not json: {e}"))?;
    // a store written by a future format is not something to guess at: a
    // silently half-read memory is worse than an empty one, and the caller
    // already knows how to start empty and say so.
    match doc.get("version").and_then(|v| v.as_u64()) {
        Some(KV_FORMAT_VERSION) => {}
        Some(other) => {
            return Err(format!(
                "cartridge kv is format version {other}; this build reads {KV_FORMAT_VERSION}"
            ))
        }
        None => return Err("cartridge kv has no `version`".to_string()),
    }
    let rows = doc
        .get("pairs")
        .and_then(|p| p.as_array())
        .ok_or_else(|| "cartridge kv has no `pairs` array".to_string())?;
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let pair = row
            .as_array()
            .filter(|a| a.len() == 2)
            .ok_or_else(|| format!("cartridge kv pair {i} is not a [key, value] array"))?;
        let k = pair[0]
            .as_str()
            .ok_or_else(|| format!("cartridge kv pair {i} has a non-string key"))?;
        let v = pair[1]
            .as_str()
            .ok_or_else(|| format!("cartridge kv pair {i} has a non-string value"))?;
        out.push((
            from_hex(k).map_err(|e| format!("cartridge kv pair {i} key: {e}"))?,
            from_hex(v).map_err(|e| format!("cartridge kv pair {i} value: {e}"))?,
        ));
    }
    Ok(out)
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    s
}

fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(format!("odd-length hex ({} chars)", bytes.len()));
    }
    let nibble = |c: u8| -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(format!("'{}' is not a hex digit", c as char)),
        }
    };
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Ok(out)
}
