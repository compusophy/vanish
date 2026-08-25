//! L5 evals (CARTRIDGE_PLAN §11 item 7): actors with mailboxes pumped
//! deterministically, guest emits routed by declared capability, crashed
//! actors restarted with backoff from their image (same host — kv state
//! survives) and marked Failed loudly past the budget, and hot-swap that
//! keeps host state and pending mail. time is passed in, so every backoff
//! decision here is exact, not approximate.

mod common;

use common::{
    compile, crasher_src, emitter_src, flaky_src, manifest_with, shift_src, FakeHost, ALLOC,
};
use vanish::cartridges::{
    CallError, CartridgeManifest, ComposeError, Event, Health, Orchestrator, WireError,
    MAX_MAILBOX, MAX_RESTARTS, RESTART_BASE_MS,
};

type Entry = (CartridgeManifest, Vec<u8>);

/// `inc` provides "inc" (byte + 1); `dec` provides "dec", requires "inc"
/// (byte − 1). wiring order: inc, dec.
fn pair() -> Vec<Entry> {
    vec![
        (manifest_with("dec", &["dec"], &["inc"]), compile(&shift_src(-1))),
        (manifest_with("inc", &["inc"], &[]), compile(&shift_src(1))),
    ]
}

fn booted(entries: Vec<Entry>) -> Orchestrator<FakeHost> {
    let mut o = Orchestrator::load(&entries, |_| FakeHost::default()).unwrap();
    o.boot(&|slug| slug.as_bytes().to_vec(), 10_000).unwrap();
    o.drain_events();
    o
}

fn delivered(to: &str, from: Option<&str>, topic: &str, response: &[u8]) -> Event {
    Event::Delivered {
        to: to.into(),
        from: from.map(String::from),
        topic: topic.into(),
        response: response.to_vec(),
    }
}

// ---- mailboxes and the pump ----------------------------------------------------------

#[test]
fn messages_queue_and_pump_one_at_a_time_round_robin() {
    let mut o = booted(pair());
    assert!(o.send("inc", b"a"));
    assert!(o.send("inc", b"b"));
    assert!(o.send("dec", b"z"));
    assert_eq!(o.pending("inc"), 2);
    assert_eq!(o.pending("dec"), 1);

    // one delivery per pump, fair across actors in wiring order.
    assert!(o.pump(0, 10_000));
    assert!(o.pump(0, 10_000));
    assert!(o.pump(0, 10_000));
    assert!(!o.pump(0, 10_000), "nothing left");
    assert_eq!(
        o.drain_events(),
        vec![
            delivered("inc", None, "inc", b"b"),
            delivered("dec", None, "dec", b"y"),
            delivered("inc", None, "inc", b"c"),
        ]
    );
    assert_eq!(o.pending("inc"), 0);
    assert_eq!(o.pump_all(0, 10_000, 10), 0);

    // pump_all drains up to its cap.
    for _ in 0..5 {
        o.send("inc", b"q");
    }
    assert_eq!(o.pump_all(0, 10_000, 3), 3);
    assert_eq!(o.pending("inc"), 2);
    assert_eq!(o.pump_all(0, 10_000, 100), 2);
}

#[test]
fn guest_emits_route_by_declared_capability_and_nowhere_else() {
    // `talker` provides "chat" and requires "inc" — it emits every message
    // on "inc" (declared) AND on "dec" (not declared, though dec exists).
    let mut entries = pair();
    entries.push((
        manifest_with("talker", &["chat"], &["inc"]),
        compile(&emitter_src(&["inc", "dec"])),
    ));
    let mut o = booted(entries);
    assert!(o.send("chat", b"hi"));
    assert!(o.pump(0, 10_000));
    let ev = o.drain_events();
    // chronological: the denial happened DURING the call, the delivery is
    // the call completing.
    assert!(
        matches!(&ev[0], Event::Denied { from, topic, .. } if from == "talker" && topic == "dec"),
        "{ev:?}"
    );
    assert_eq!(ev[1], delivered("talker", None, "chat", b"hi"));
    assert_eq!(ev.len(), 2);
    assert_eq!(o.pending("inc"), 1, "the declared emit was queued");
    assert_eq!(o.pending("dec"), 0, "the undeclared one was not");

    // the routed message is delivered with its origin recorded.
    assert!(o.pump(0, 10_000));
    assert_eq!(o.drain_events(), vec![delivered("inc", Some("talker"), "inc", b"ij")]);
    // and the real host observed the emits too (it is the feed's witness).
    assert_eq!(o.host("talker").unwrap().emitted.len(), 2);
}

#[test]
fn unknown_ports_and_full_mailboxes_are_undeliverable_events() {
    let mut o = booted(pair());
    assert!(!o.send("vision", b"x"));
    assert!(
        matches!(&o.drain_events()[..], [Event::Undeliverable { topic, .. }] if topic == "vision")
    );
    for _ in 0..MAX_MAILBOX {
        assert!(o.send("inc", b"x"));
    }
    assert!(!o.send("inc", b"one too many"));
    assert!(
        matches!(&o.drain_events()[..], [Event::Undeliverable { reason, .. }] if reason.contains("full"))
    );
    assert_eq!(o.pending("inc"), MAX_MAILBOX);
}

// ---- supervision -----------------------------------------------------------------------

#[test]
fn a_crashing_actor_is_restarted_with_backoff_then_failed_loudly() {
    let entries = vec![(manifest_with("boom", &["boom"], &[]), compile(&crasher_src()))];
    let mut o = booted(entries);
    for _ in 0..7 {
        o.send("boom", b"x");
    }

    // first crash at t=0: restart not before 500.
    assert!(o.pump(0, 10_000));
    let ev = o.drain_events();
    assert!(
        matches!(&ev[..], [Event::Crashed { slug, attempt: 1, retry_at_ms, reason }]
            if slug == "boom" && *retry_at_ms == RESTART_BASE_MS && reason.contains("out of bounds")),
        "{ev:?}"
    );
    assert!(matches!(o.health("boom"), Some(Health::Restarting { attempt: 1, .. })));
    assert!(!o.pump(0, 10_000), "backoff: nothing runnable yet");
    assert!(!o.pump(RESTART_BASE_MS - 1, 10_000));

    // each restart at exactly its retry time: restarted, delivered, crashed
    // again with the delay doubled — until the budget is spent.
    let mut now = RESTART_BASE_MS;
    for attempt in 1..=MAX_RESTARTS {
        assert!(o.pump(now, 10_000), "attempt {attempt}");
        let ev = o.drain_events();
        assert_eq!(ev[0], Event::Restarted { slug: "boom".into(), attempt });
        if attempt < MAX_RESTARTS {
            let next_delay = RESTART_BASE_MS << attempt;
            assert!(
                matches!(&ev[1], Event::Crashed { attempt: a, retry_at_ms, .. }
                    if *a == attempt + 1 && *retry_at_ms == now + next_delay),
                "{ev:?}"
            );
            now += next_delay;
        } else {
            assert!(
                matches!(&ev[1], Event::Failed { slug, reason } if slug == "boom" && reason.contains("gave up")),
                "{ev:?}"
            );
        }
    }
    assert!(matches!(o.health("boom"), Some(Health::Failed { .. })));
    assert!(!o.pump(1_000_000, 10_000), "a failed actor is never pumped");
    assert_eq!(o.pending("boom"), 1, "7 sent, 6 consumed by crashes, one still queued");
    // the host survived every restart: one init log per boot + restart.
    let logs = &o.host("boom").unwrap().logs;
    assert_eq!(logs.len(), 1 + MAX_RESTARTS as usize);
    assert!(logs.iter().all(|l| l == &(1, b"boom".to_vec())), "same config replayed");
}

#[test]
fn a_successful_delivery_resets_the_crash_count() {
    let entries = vec![(manifest_with("flaky", &["flaky"], &[]), compile(&flaky_src()))];
    let mut o = booted(entries);
    // alternate crash / success well past the budget; never Failed.
    let mut now = 0;
    for round in 0..(MAX_RESTARTS * 2) {
        o.send("flaky", b"");
        o.send("flaky", b"ok");
        assert!(o.pump(now, 10_000), "round {round}: crash");
        let ev = o.drain_events();
        let Event::Crashed { attempt, retry_at_ms, .. } = &ev[0] else { panic!("{ev:?}") };
        assert_eq!(*attempt, 1, "the count reset after the last success");
        now = *retry_at_ms;
        assert!(o.pump(now, 10_000), "round {round}: restart + deliver");
        let ev = o.drain_events();
        assert_eq!(ev[0], Event::Restarted { slug: "flaky".into(), attempt: 1 });
        assert_eq!(ev[1], delivered("flaky", None, "flaky", b"ok"));
    }
    assert_eq!(o.health("flaky"), Some(&Health::Up));
}

#[test]
fn a_boot_refusal_marks_the_actor_failed_and_others_keep_working() {
    let refuser = format!(
        "{ALLOC} pub fn cart_init(p: i32, n: i32) -> i32 {{ return 7; }} \
         pub fn cart_handle(p: i32, n: i32) -> i64 {{ return 0; }}"
    );
    let entries = vec![
        (manifest_with("refuser", &["r"], &[]), compile(&refuser)),
        (manifest_with("inc", &["inc"], &[]), compile(&shift_src(1))),
    ];
    let mut o = Orchestrator::load(&entries, |_| FakeHost::default()).unwrap();
    let err = o.boot(&|_| vec![], 10_000).unwrap_err();
    assert_eq!(
        err,
        ComposeError::Call {
            slug: "refuser".into(),
            error: CallError::Refused(7),
        }
    );
    assert!(matches!(o.health("refuser"), Some(Health::Failed { reason }) if reason.contains("boot")));
    assert!(matches!(&o.drain_events()[..], [Event::Failed { slug, .. }] if slug == "refuser"));
    // inc booted first (wiring order) and works; the refuser's mail waits.
    assert!(o.send("inc", b"a"));
    assert!(o.send("r", b"x"));
    assert!(o.pump(0, 10_000));
    assert_eq!(o.drain_events(), vec![delivered("inc", None, "inc", b"b")]);
    assert!(!o.pump(0, 10_000));
    assert_eq!(o.pending("refuser"), 1, "queued behind the failed actor, never pumped");
}

// ---- hot-swap --------------------------------------------------------------------------

#[test]
fn hot_swap_keeps_host_state_and_pending_mail() {
    let mut o = booted(vec![(manifest_with("inc", &["inc"], &[]), compile(&shift_src(1)))]);
    o.send("inc", b"b");
    o.send("inc", b"b");
    assert!(o.pump(0, 10_000));
    assert_eq!(o.drain_events(), vec![delivered("inc", None, "inc", b"c")]);

    // same slug and ports, new behavior (byte − 1).
    o.swap(manifest_with("inc", &["inc"], &[]), &compile(&shift_src(-1)), 10_000)
        .unwrap();
    assert_eq!(o.drain_events(), vec![Event::Swapped { slug: "inc".into() }]);
    assert_eq!(o.health("inc"), Some(&Health::Up));
    assert_eq!(o.pending("inc"), 1, "pending mail survived the swap");
    assert!(o.pump(0, 10_000));
    assert_eq!(o.drain_events(), vec![delivered("inc", None, "inc", b"a")], "handled by the NEW module");
    // the host carried over: it saw both inits with the remembered config.
    assert_eq!(o.host("inc").unwrap().logs, vec![(1, b"inc".to_vec()), (1, b"inc".to_vec())]);

    // a swap also revives a Failed actor — that is the escape hatch.
    let mut o = booted(vec![(manifest_with("boom", &["boom"], &[]), compile(&crasher_src()))]);
    o.send("boom", b"x");
    let mut now = 0;
    while !matches!(o.health("boom"), Some(Health::Failed { .. })) {
        o.send("boom", b"x");
        if !o.pump(now, 10_000) {
            now += RESTART_BASE_MS << MAX_RESTARTS;
        }
    }
    o.swap(manifest_with("boom", &["boom"], &[]), &compile(&shift_src(1)), 10_000)
        .unwrap();
    assert_eq!(o.health("boom"), Some(&Health::Up));
    o.drain_events();
    assert!(o.pump(now, 10_000));
    assert!(matches!(&o.drain_events()[0], Event::Delivered { response, .. } if response == b"y"));
}

#[test]
fn a_swap_that_breaks_the_wiring_or_the_bytes_is_refused_with_nothing_changed() {
    let mut o = booted(pair());
    // a port change that leaves `dec` without its provider.
    let err = o
        .swap(manifest_with("inc", &["other"], &[]), &compile(&shift_src(1)), 10_000)
        .unwrap_err();
    assert!(
        matches!(err, ComposeError::Wire(WireError::MissingProvider { ref port, .. }) if port == "inc"),
        "{err:?}"
    );
    // bad bytes.
    let err = o
        .swap(manifest_with("inc", &["inc"], &[]), b"not wasm", 10_000)
        .unwrap_err();
    assert!(matches!(err, ComposeError::Load { ref slug, .. } if slug == "inc"), "{err:?}");
    // a slug that is not a member.
    let err = o
        .swap(manifest_with("ghost", &["g"], &[]), &compile(&shift_src(1)), 10_000)
        .unwrap_err();
    assert_eq!(err, ComposeError::UnknownCartridge("ghost".into()));
    // nothing changed: old behavior, old wiring, no Swapped event.
    assert!(o.drain_events().is_empty());
    o.send("inc", b"a");
    assert!(o.pump(0, 10_000));
    assert_eq!(o.drain_events(), vec![delivered("inc", None, "inc", b"b")]);
    assert_eq!(o.composition().wiring().order, vec!["inc", "dec"]);
    assert_eq!(o.host("inc").unwrap().logs.len(), 1, "no re-init happened");
}
