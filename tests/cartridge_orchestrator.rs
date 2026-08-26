//! L5 evals (CARTRIDGE_PLAN §11 item 7 + ABI v2 `call`): actors with
//! mailboxes pumped deterministically, guest emits routed by declared
//! capability, crashed actors restarted with backoff from their image
//! (same host — kv state survives) and marked Failed loudly past the
//! budget, hot-swap that keeps host state and pending mail, and
//! synchronous calls between actors — mediated, capability-checked,
//! charged to the caller, soft for the guest and loud for the log. time
//! is passed in, so every backoff decision here is exact, not approximate.

mod common;

use common::{
    caller_src, compile, crasher_src, emitter_src, flaky_src, manifest_with, shift_src, FakeHost,
    ALLOC,
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

fn logs(o: &Orchestrator<FakeHost>, slug: &str) -> Vec<(i32, Vec<u8>)> {
    o.with_host(slug, |h| h.logs.clone()).unwrap()
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
    assert_eq!(o.with_host("talker", |h| h.emitted.len()).unwrap(), 2);
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
    let logs = logs(&o, "boom");
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
    assert_eq!(o.health("flaky"), Some(Health::Up));
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
    assert_eq!(o.health("inc"), Some(Health::Up));
    assert_eq!(o.pending("inc"), 1, "pending mail survived the swap");
    assert!(o.pump(0, 10_000));
    assert_eq!(o.drain_events(), vec![delivered("inc", None, "inc", b"a")], "handled by the NEW module");
    // the host carried over: it saw both inits with the remembered config.
    assert_eq!(logs(&o, "inc"), vec![(1, b"inc".to_vec()), (1, b"inc".to_vec())]);

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
    assert_eq!(o.health("boom"), Some(Health::Up));
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
    assert_eq!(o.order(), vec!["inc", "dec"]);
    assert_eq!(logs(&o, "inc").len(), 1, "no re-init happened");
}

// ---- ABI v2: synchronous calls ---------------------------------------------------------

#[test]
fn a_synchronous_call_reaches_the_provider_and_returns_its_answer() {
    // `ask` provides "ask", requires "inc", and forwards every message to
    // inc via call — the answer comes back through ask's own memory.
    let mut entries = pair();
    entries.push((manifest_with("ask", &["ask"], &["inc"]), compile(&caller_src("inc"))));
    let mut o = booted(entries);
    assert!(o.send("ask", b"abc"));
    assert!(o.pump(0, 10_000));
    let ev = o.drain_events();
    assert_eq!(
        ev,
        vec![
            Event::Called {
                from: "ask".into(),
                to: "inc".into(),
                port: "inc".into(),
                response_len: 3
            },
            delivered("ask", None, "ask", b"bcd"),
        ]
    );
    assert_eq!(o.pending("inc"), 0, "a call is not mail: nothing was queued");

    // the callee is charged to the CALLER's budget: a budget that covers
    // the caller alone is not enough once it calls. running dry INSIDE the
    // callee is the caller's crash, not the callee's — the callee is
    // reset (rebuilt on the next pump, no backoff, no crash counted), so a
    // stingy caller cannot back an innocent provider off into Failed.
    assert!(o.send("ask", b"abc"));
    assert!(o.pump(0, 60));
    let ev = o.drain_events();
    assert!(
        matches!(&ev[0], Event::CallFailed { from, port, reason }
            if from == "ask" && port == "inc" && reason.contains("fuel")),
        "{ev:?}"
    );
    assert!(matches!(&ev[1], Event::Reset { slug, .. } if slug == "inc"), "{ev:?}");
    assert!(
        matches!(&ev[2], Event::Crashed { slug, attempt: 1, reason, .. } if slug == "ask" && reason.contains("fuel")),
        "{ev:?}"
    );
    assert!(matches!(o.health("inc"), Some(Health::Restarting { attempt: 0, .. })));
    // the next pump rebuilds inc even though it has no mail of its own;
    // ask waits out its real backoff.
    assert!(o.pump(0, 10_000));
    assert_eq!(o.drain_events(), vec![Event::Restarted { slug: "inc".into(), attempt: 0 }]);
    assert_eq!(o.health("inc"), Some(Health::Up));
    assert!(!o.pump(0, 10_000));
    // at-most-once: the message that crashed ask is gone. ask restarts at
    // its retry time with nothing to deliver; a fresh message goes through.
    assert!(o.pump(RESTART_BASE_MS, 10_000));
    assert_eq!(o.drain_events(), vec![Event::Restarted { slug: "ask".into(), attempt: 1 }]);
    assert!(o.send("ask", b"abc"));
    assert!(o.pump(RESTART_BASE_MS, 10_000));
    assert!(o.drain_events().contains(&delivered("ask", None, "ask", b"bcd")));
}

#[test]
fn calls_to_undeclared_ports_return_zero_and_are_denied() {
    // `ask` calls "dec" but declared nothing.
    let mut entries = pair();
    entries.push((manifest_with("ask", &["ask"], &[]), compile(&caller_src("dec"))));
    let mut o = booted(entries);
    assert!(o.send("ask", b"abc"));
    assert!(o.pump(0, 10_000));
    let ev = o.drain_events();
    assert!(
        matches!(&ev[0], Event::Denied { from, topic, .. } if from == "ask" && topic == "dec"),
        "{ev:?}"
    );
    // the guest saw 0 and took its fallback path: echo.
    assert_eq!(ev[1], delivered("ask", None, "ask", b"abc"));
    assert_eq!(o.health("ask"), Some(Health::Up), "a denied call is not a crash");
}

#[test]
fn a_callee_that_traps_fails_the_call_softly_and_is_supervised() {
    let entries = vec![
        (manifest_with("boom", &["boom"], &[]), compile(&crasher_src())),
        (manifest_with("ask", &["ask"], &["boom"]), compile(&caller_src("boom"))),
    ];
    let mut o = booted(entries);
    assert!(o.send("ask", b"q"));
    assert!(o.pump(0, 10_000));
    let ev = o.drain_events();
    assert!(
        matches!(&ev[0], Event::CallFailed { from, port, reason }
            if from == "ask" && port == "boom" && reason.contains("out of bounds")),
        "{ev:?}"
    );
    assert!(
        matches!(&ev[1], Event::Crashed { slug, attempt: 1, retry_at_ms, .. }
            if slug == "boom" && *retry_at_ms == RESTART_BASE_MS),
        "the callee is supervised as if it crashed on its own mail: {ev:?}"
    );
    assert_eq!(ev[2], delivered("ask", None, "ask", b"q"), "the caller fell back and completed");
    assert_eq!(o.health("ask"), Some(Health::Up));
    assert!(matches!(o.health("boom"), Some(Health::Restarting { .. })));
    // while the callee is down, a call is refused without running it.
    assert!(o.send("ask", b"r"));
    assert!(o.pump(0, 10_000));
    let ev = o.drain_events();
    assert!(
        matches!(&ev[0], Event::CallFailed { reason, .. } if reason.contains("not up")),
        "{ev:?}"
    );
    assert_eq!(ev[1], delivered("ask", None, "ask", b"r"));
}

#[test]
fn call_chains_nest_and_answers_propagate_back() {
    // a → b → c → inc: three synchronous hops deep, the +1 comes all the
    // way back. proves the re-entrancy discipline (each level takes its
    // callee out of the composition; nothing is borrowed across a run).
    let entries = vec![
        (manifest_with("inc", &["inc"], &[]), compile(&shift_src(1))),
        (manifest_with("c", &["c"], &["inc"]), compile(&caller_src("inc"))),
        (manifest_with("b", &["b"], &["c"]), compile(&caller_src("c"))),
        (manifest_with("a", &["a"], &["b"]), compile(&caller_src("b"))),
    ];
    let mut o = booted(entries);
    assert!(o.send("a", b"abc"));
    assert!(o.pump(0, 100_000));
    let ev = o.drain_events();
    let called: Vec<(String, String)> = ev
        .iter()
        .filter_map(|e| match e {
            Event::Called { from, to, .. } => Some((from.clone(), to.clone())),
            _ => None,
        })
        .collect();
    // innermost completes first.
    assert_eq!(
        called,
        vec![
            ("c".into(), "inc".into()),
            ("b".into(), "c".into()),
            ("a".into(), "b".into())
        ]
    );
    assert_eq!(ev.last().unwrap(), &delivered("a", None, "a", b"bcd"));
}

#[test]
fn emits_during_a_nested_call_are_routed_after_it() {
    // ask → talker (which emits on "inc" during the call). the emit lands
    // in inc's mailbox once the call returns; nothing is lost or reordered.
    let mut entries = pair();
    entries.push((
        manifest_with("talker", &["talk"], &["inc"]),
        compile(&emitter_src(&["inc"])),
    ));
    entries.push((manifest_with("ask", &["ask"], &["talk"]), compile(&caller_src("talk"))));
    let mut o = booted(entries);
    assert!(o.send("ask", b"hi"));
    assert!(o.pump(0, 10_000));
    let ev = o.drain_events();
    assert!(matches!(&ev[0], Event::Called { from, to, .. } if from == "ask" && to == "talker"));
    assert_eq!(ev[1], delivered("ask", None, "ask", b"hi"));
    assert_eq!(o.pending("inc"), 1, "talker's emit during the call was routed");
    assert!(o.pump(0, 10_000));
    assert_eq!(o.drain_events(), vec![delivered("inc", Some("talker"), "inc", b"ij")]);
}
