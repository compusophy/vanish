//! an in-memory `Host` with write-behind bookkeeping — the host the worker
//! gives each cartridge.
//!
//! why in-memory: the `Host` trait is synchronous (the interpreter cannot
//! suspend), but the browser's durable store (opfs) is async. so a
//! cartridge's kv lives here during a step, and the worker FLUSHES what
//! changed (`take_dirty`) to `cartridges/{slug}/kv/…` right after the step
//! — the same write-behind shape as the transcript checkpoint drain. D2
//! holds because the flush is part of every step, not an afterthought;
//! the window in which work exists only in memory is one pump.
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
    /// `cartridges/{slug}/kv/…` and hands the pairs over). seeded keys are
    /// not dirty — they are already on disk.
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
    pub fn take_dirty(&mut self) -> Vec<(Vec<u8>, Vec<u8>)> {
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
