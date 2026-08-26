//! item 8a evals: the reasoning policy as a hot-swappable cartridge. the
//! two-phase protocol (prompt in → prompt out; answer in → note out) over
//! the real orchestrator with the real reference cartridges, kv memory
//! that survives a swap, passthrough whenever the policy is absent or
//! broken, and the write-behind host the worker will use.

mod common;

use common::{compile, crasher_src, manifest_with, ALLOC};
use vanish::cartridges::cognitive::{describe, REASONING_V1, REASONING_V2};
use vanish::cartridges::{
    CartridgeManifest, Cognition, Event, Health, Host, MemHost, Orchestrator, PORT_AFTER,
    PORT_BEFORE,
};

const FUEL: u64 = 200_000;

fn policy_manifest() -> CartridgeManifest {
    let mut m = manifest_with("reasoner", &[PORT_BEFORE, PORT_AFTER], &[]);
    m.kind = vanish::cartridges::CartridgeKind::Cognitive;
    m
}

/// a cognition over one reasoning cartridge with a MemHost — exactly the
/// worker's configuration, minus opfs.
fn cognition(src: &str) -> Cognition<MemHost> {
    let entries = vec![(policy_manifest(), compile(src))];
    let mut orch = Orchestrator::load(&entries, |m| MemHost::new(&m.slug)).unwrap();
    orch.boot(&|_| vec![], FUEL).unwrap();
    Cognition::new(orch, FUEL)
}

/// read a kv entry through the orchestrator's host view.
fn kv_of(c: &mut Cognition<MemHost>, key: &str) -> Option<String> {
    c.orchestrator()
        .with_host("reasoner", |h| {
            h.kv.get(key.as_bytes())
                .map(|v| String::from_utf8_lossy(v).into_owned())
        })
        .flatten()
}

// ---- the policy shapes prompts and digests answers ---------------------------------

#[test]
fn v1_is_a_passthrough_that_remembers() {
    let mut c = cognition(REASONING_V1);
    assert!(c.has_policy());
    // boot logged the module's own greeting through its host.
    let logs = c.orchestrator().with_host_mut("reasoner", |h| h.take_logs()).unwrap();
    assert_eq!(logs, vec![(1, "reasoning v1 up: passthrough + remember".to_string())]);

    let shaped = c.before("what is 2+2?", 0);
    assert_eq!(shaped.prompt, "what is 2+2?", "v1 changes nothing");
    assert!(shaped.notes.is_empty(), "routine deliveries are not notes: {:?}", shaped.notes);
    assert_eq!(kv_of(&mut c, "last_prompt").as_deref(), Some("what is 2+2?"));

    let notes = c.after("4", 0);
    assert!(notes.is_empty());
    assert_eq!(kv_of(&mut c, "last_answer").as_deref(), Some("4"));
    // the phase byte never leaks into what the guest stored.
    assert_eq!(kv_of(&mut c, "last_prompt").as_deref(), Some("what is 2+2?"));
    // the write-behind host reports exactly what changed, once.
    let dirty = c.orchestrator().with_host_mut("reasoner", |h| h.take_dirty()).unwrap();
    let mut keys: Vec<String> = dirty.iter().map(|(k, _)| String::from_utf8_lossy(k).into_owned()).collect();
    keys.sort();
    assert_eq!(keys, vec!["last_answer", "last_prompt"]);
    assert!(c.orchestrator().with_host_mut("reasoner", |h| h.take_dirty()).unwrap().is_empty());
}

#[test]
fn v2_prefixes_every_prompt() {
    let mut c = cognition(REASONING_V2);
    assert_eq!(c.before("hello", 0).prompt, "[v2] hello");
    assert_eq!(c.before("", 0).prompt, "[v2] ", "an empty prompt is still framed and shaped");
    assert_eq!(kv_of(&mut c, "last_prompt").as_deref(), Some(""));
    let notes = c.after("hi there", 0);
    assert!(notes.is_empty());
    assert_eq!(kv_of(&mut c, "last_answer").as_deref(), Some("hi there"));
}

// ---- hot-swap mid-conversation ------------------------------------------------------

#[test]
fn the_policy_hot_swaps_between_prompts_with_kv_intact() {
    let mut c = cognition(REASONING_V1);
    assert_eq!(c.before("first", 0).prompt, "first");
    c.after("one", 0);

    // swap v1 → v2 between prompts: the very next prompt is shaped by v2,
    // and v2 sees v1's memory (same host, kv untouched by the swap).
    c.orchestrator()
        .swap(policy_manifest(), &compile(REASONING_V2), FUEL)
        .unwrap();
    assert_eq!(kv_of(&mut c, "last_answer").as_deref(), Some("one"), "memory survived the swap");
    let shaped = c.before("second", 1_000);
    assert_eq!(shaped.prompt, "[v2] second");
    assert_eq!(shaped.notes, vec!["🔁 cartridge 'reasoner' hot-swapped".to_string()]);
    assert_eq!(kv_of(&mut c, "last_prompt").as_deref(), Some("second"));
    let logs = c.orchestrator().with_host_mut("reasoner", |h| h.take_logs()).unwrap();
    assert_eq!(
        logs.iter().map(|(_, m)| m.as_str()).collect::<Vec<_>>(),
        vec!["reasoning v1 up: passthrough + remember", "reasoning v2 up: prefix + remember"]
    );

    // and back: the swap is symmetric.
    c.orchestrator()
        .swap(policy_manifest(), &compile(REASONING_V1), FUEL)
        .unwrap();
    assert_eq!(c.before("third", 2_000).prompt, "third");
}

// ---- the loop is never hostage to the policy ------------------------------------------

#[test]
fn without_a_policy_everything_passes_through_silently() {
    let entries: Vec<(CartridgeManifest, Vec<u8>)> = vec![];
    let mut orch = Orchestrator::load(&entries, |m| MemHost::new(&m.slug)).unwrap();
    orch.boot(&|_| vec![], FUEL).unwrap();
    let mut c = Cognition::new(orch, FUEL);
    assert!(!c.has_policy());
    let shaped = c.before("as written", 0);
    assert_eq!(shaped.prompt, "as written");
    assert!(shaped.notes.is_empty());
    assert!(c.after("whatever", 0).is_empty());
}

#[test]
fn a_crashing_policy_falls_back_to_passthrough_with_a_note_and_is_supervised() {
    let mut c = cognition(&crasher_src());
    let shaped = c.before("still sent", 0);
    assert_eq!(shaped.prompt, "still sent");
    assert_eq!(shaped.notes.len(), 2, "{:?}", shaped.notes);
    assert!(shaped.notes[0].contains("could not shape the prompt") && shaped.notes[0].contains("out of bounds"));
    assert!(shaped.notes[1].starts_with("💥 cartridge 'reasoner' crashed"));
    assert!(matches!(c.orchestrator().health("reasoner"), Some(Health::Restarting { attempt: 1, .. })));

    // while it is down: passthrough, with the reason, no second crash.
    let shaped = c.before("again", 10);
    assert_eq!(shaped.prompt, "again");
    assert_eq!(shaped.notes.len(), 1);
    assert!(shaped.notes[0].contains("not up") && shaped.notes[0].contains("restarting"), "{:?}", shaped.notes);

    // once its backoff elapsed, a request rebuilds it first (then it
    // crashes again — it is a crasher — and the ladder continues).
    let shaped = c.before("later", 1_000);
    assert_eq!(shaped.prompt, "later");
    assert!(shaped.notes.iter().any(|n| n.starts_with("↻ cartridge 'reasoner' restarted")), "{:?}", shaped.notes);
    assert!(shaped.notes.iter().any(|n| n.contains("restart 2/")), "{:?}", shaped.notes);

    // a swap to a working policy revives it, mid-conversation.
    c.orchestrator()
        .swap(policy_manifest(), &compile(REASONING_V2), FUEL)
        .unwrap();
    assert_eq!(c.before("fixed", 2_000).prompt, "[v2] fixed");
}

#[test]
fn a_policy_that_refuses_boot_is_reported_and_bypassed() {
    let refuser = format!(
        "{ALLOC} pub fn cart_init(p: i32, n: i32) -> i32 {{ return 3; }} \
         pub fn cart_handle(p: i32, n: i32) -> i64 {{ return 0; }}"
    );
    let entries = vec![(policy_manifest(), compile(&refuser))];
    let mut orch = Orchestrator::load(&entries, |m| MemHost::new(&m.slug)).unwrap();
    assert!(orch.boot(&|_| vec![], FUEL).is_err());
    let mut c = Cognition::new(orch, FUEL);
    let shaped = c.before("p", 0);
    assert_eq!(shaped.prompt, "p");
    // the boot failure event and the not-up refusal both reach the feed.
    assert!(shaped.notes.iter().any(|n| n.starts_with("⛔ cartridge 'reasoner' FAILED")), "{:?}", shaped.notes);
    assert!(shaped.notes.iter().any(|n| n.contains("failed:")), "{:?}", shaped.notes);
}

// ---- describe: what reaches the feed ---------------------------------------------------

#[test]
fn describe_keeps_routine_traffic_out_of_the_feed() {
    assert!(describe(&Event::Delivered {
        to: "a".into(),
        from: None,
        topic: "t".into(),
        response: vec![]
    })
    .is_none());
    assert!(describe(&Event::Called {
        from: "a".into(),
        to: "b".into(),
        port: "p".into(),
        response_len: 0
    })
    .is_none());
    let text = describe(&Event::Denied {
        from: "a".into(),
        topic: "vision".into(),
        reason: String::new(),
    })
    .unwrap();
    assert!(text.contains("'a'") && text.contains("'vision'"));
    let text = describe(&Event::Failed {
        slug: "a".into(),
        reason: "boom".into(),
    })
    .unwrap();
    assert!(text.contains("FAILED") && text.contains("swap"));
}

// ---- the write-behind host ---------------------------------------------------------------

#[test]
fn memhost_tracks_dirty_keys_logs_emits_and_clock_without_reading_one() {
    let mut h = MemHost::new("m");
    h.seed(vec![(b"k".to_vec(), b"seeded".to_vec())]);
    assert!(h.take_dirty().is_empty(), "seeded keys are already on disk");
    assert_eq!(h.store_get(b"k").unwrap(), Some(b"seeded".to_vec()));
    h.store_set(b"k", b"v1").unwrap();
    h.store_set(b"k", b"v2").unwrap();
    h.store_set(b"j", b"x").unwrap();
    let mut dirty = h.take_dirty();
    dirty.sort();
    assert_eq!(dirty, vec![(b"j".to_vec(), b"x".to_vec()), (b"k".to_vec(), b"v2".to_vec())], "latest value, once per key");
    assert!(h.take_dirty().is_empty());
    h.log(2, b"warn");
    h.emit(b"topic", b"payload").unwrap();
    assert_eq!(h.take_logs(), vec![(2, "warn".to_string())]);
    assert_eq!(h.take_emits(), vec![("topic".to_string(), b"payload".to_vec())]);
    assert_eq!(h.now_ms(), 0);
    h.set_now(1234);
    assert_eq!(h.now_ms(), 1234);
    let mut fuel = 10;
    assert_eq!(h.call(b"p", b"m", &mut fuel).unwrap(), None, "a bare MemHost mediates nothing");
}
