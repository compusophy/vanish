//! items 8b/8c evals: everything the browser wiring of the reasoning
//! cartridge rests on, tested where it can actually be tested.
//!
//! the worker glue itself (opfs reads, `Command::SwapCartridge`, the
//! `swap_cartridge` tool, the feed) cannot run natively — it is wasm-bound
//! and verified live. so the pieces it is made of are pulled out as pure
//! functions and pinned here: booting a policy from a saved source and a
//! saved store, the write-behind flush that makes a cartridge's memory
//! durable (D2), the rehearsal a candidate must survive before it can
//! replace a running policy, the swap itself, and the passthrough that must
//! survive every one of those going wrong (D9, article iv).

use vanish::agent::{NoReasoning, Reasoning};
use vanish::cartridges::cognitive::{
    CARRY_MARKER, REASONING_V1, REASONING_V2, REASONING_V3, REHEARSAL_PROMPT, STANDING_KEY,
    STANDING_MAX, STANDING_PROTOCOL,
};
use vanish::cartridges::memhost::{decode_kv, encode_kv, KvFlush};
use vanish::cartridges::{
    boot_reasoner, kv_path, manifest_path, parse_policy, reasoner_manifest, rehearse, source_path,
    CartridgeKind, Cognition, MemHost, PORT_AFTER, PORT_BEFORE, REASONER_FUEL, REASONER_SLUG,
};

fn boot(src: &str, kv: Option<&str>) -> (Cognition<MemHost>, Vec<String>) {
    boot_reasoner("", src, kv, REASONER_FUEL).expect("the reference policy boots")
}

fn kv_of(c: &mut Cognition<MemHost>, key: &str) -> Option<String> {
    c.orchestrator()
        .with_host(REASONER_SLUG, |h| {
            h.kv.get(key.as_bytes())
                .map(|v| String::from_utf8_lossy(v).into_owned())
        })
        .flatten()
}

/// the worker's own move: take everything dirty and hand back the one body
/// that would land in opfs.
fn flush_body(c: &mut Cognition<MemHost>) -> Option<String> {
    let mut flushes = c.take_flushes();
    assert!(flushes.len() <= 1, "one cartridge, at most one flush");
    flushes.pop().map(|f: KvFlush| f.body)
}

// ---- the durable encoding ----------------------------------------------

#[test]
fn kv_round_trips_including_bytes_that_are_not_text() {
    let pairs = vec![
        (b"last_prompt".to_vec(), "héllo — ok".as_bytes().to_vec()),
        (b"empty".to_vec(), Vec::new()),
        (vec![0x00, 0xff, 0x80], vec![0xde, 0xad, 0xbe, 0xef]),
    ];
    let text = encode_kv(&pairs);
    assert_eq!(decode_kv(&text).unwrap(), pairs);
    // and it is json a human can look at, not an opaque blob.
    assert!(text.contains("\"version\":1"), "{text}");
    assert!(text.contains("\"6c6173745f70726f6d7074\""), "{text}");
}

#[test]
fn a_corrupt_store_is_named_rather_than_guessed_at() {
    for (bad, want) in [
        ("not json at all", "not json"),
        ("{\"version\":1}", "no `pairs` array"),
        ("{\"pairs\":[]}", "no `version`"),
        ("{\"version\":2,\"pairs\":[]}", "format version 2"),
        ("{\"version\":1,\"pairs\":[[\"6\",\"00\"]]}", "odd-length hex"),
        ("{\"version\":1,\"pairs\":[[\"zz\",\"00\"]]}", "not a hex digit"),
        ("{\"version\":1,\"pairs\":[[\"00\"]]}", "not a [key, value] array"),
        ("{\"version\":1,\"pairs\":[[1,\"00\"]]}", "non-string key"),
    ] {
        let err = decode_kv(bad).expect_err(bad);
        assert!(err.contains(want), "decoding {bad}: got {err}, wanted {want}");
    }
}

#[test]
fn the_state_paths_are_outside_anything_the_working_tree_could_claim() {
    // opfs's root is shared with the repo mirror; a cartridge writing to
    // `cartridges/…` would collide with a source directory of that name.
    for path in [
        kv_path(REASONER_SLUG),
        source_path(REASONER_SLUG),
        manifest_path(REASONER_SLUG),
    ] {
        assert!(
            path.starts_with("vanish-cartridges/reasoner/"),
            "unexpected state path: {path}"
        );
    }
}

// ---- the write-behind flush --------------------------------------------

#[test]
fn a_flush_happens_only_when_a_cartridge_wrote_something() {
    let (mut c, _) = boot(REASONING_V1, None);
    // v1 writes nothing at init, so boot leaves nothing to persist.
    assert!(c.take_flushes().is_empty(), "init wrote to kv unexpectedly");

    c.before("what is 2+2?", 0);
    let flushes = c.take_flushes();
    assert_eq!(flushes.len(), 1);
    assert_eq!(flushes[0].slug, REASONER_SLUG);
    assert_eq!(flushes[0].path, kv_path(REASONER_SLUG));
    assert_eq!(flushes[0].keys, vec!["last_prompt".to_string()]);

    // taken means taken: nothing new happened, nothing is written again.
    assert!(c.take_flushes().is_empty());

    c.after("4", 0);
    let flushes = c.take_flushes();
    assert_eq!(flushes[0].keys, vec!["last_answer".to_string()]);
    // the body is the WHOLE store, not just the key that changed — that is
    // what makes one file per cartridge sufficient.
    let pairs = decode_kv(&flushes[0].body).unwrap();
    let keys: Vec<String> = pairs
        .iter()
        .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
        .collect();
    assert_eq!(keys, vec!["last_answer", "last_prompt"]);
}

#[test]
fn what_a_policy_remembered_survives_a_reload() {
    let (mut first, _) = boot(REASONING_V1, None);
    first.before("remember this", 0);
    first.after("remembered", 0);
    let body = flush_body(&mut first).expect("the policy wrote to kv");
    drop(first);

    // a fresh worker, a fresh cartridge, the same memory.
    let (mut second, notes) = boot(REASONING_V1, Some(&body));
    assert_eq!(kv_of(&mut second, "last_prompt").as_deref(), Some("remember this"));
    assert_eq!(kv_of(&mut second, "last_answer").as_deref(), Some("remembered"));
    assert!(
        notes.iter().any(|n| n.contains("restored 2 remembered key(s)")),
        "the restore should be visible in the feed: {notes:?}"
    );
    // seeded keys are already on disk, so a reboot alone rewrites nothing.
    assert!(second.take_flushes().is_empty());
}

#[test]
fn a_store_that_does_not_parse_costs_the_memory_not_the_loop() {
    let (mut c, notes) = boot(REASONING_V1, Some("{ this is not the store }"));
    assert!(
        notes.iter().any(|n| n.contains("did not parse") && n.contains("empty store")),
        "a corrupt store must be loud (D4): {notes:?}"
    );
    assert_eq!(kv_of(&mut c, "last_prompt"), None);
    // and the policy still works.
    assert_eq!(c.before("still here", 0).prompt, "still here");
}

// ---- booting a policy ---------------------------------------------------

#[test]
fn booting_reports_the_module_greeting_it_logged() {
    let (mut c, notes) = boot(REASONING_V1, None);
    assert!(c.has_policy());
    assert!(
        notes.iter().any(|n| n.contains("reasoning v1 up: passthrough + remember")),
        "cart_init's own log belongs on the feed: {notes:?}"
    );
    // the logs were TAKEN by boot, not left to reappear on the first prompt.
    assert!(c.take_logs().is_empty());
}

#[test]
fn the_default_manifest_is_the_reasoner_providing_both_phases() {
    let m = reasoner_manifest();
    assert_eq!(m.slug, REASONER_SLUG);
    assert_eq!(m.kind, CartridgeKind::Cognitive);
    let provides: Vec<&str> = m.provides.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(provides, vec![PORT_BEFORE, PORT_AFTER]);
    assert!(m.requires.is_empty());
    m.validate().expect("the default manifest must validate");

    // a blank manifest field in the ui means exactly that manifest.
    let (parsed, _) = parse_policy("   \n", REASONING_V1).unwrap();
    assert_eq!(parsed, m);
}

#[test]
fn a_policy_that_cannot_be_built_says_where() {
    let err = parse_policy("", "pub fn cart_handle(p: i32) -> i64 { return notathing(p); }")
        .expect_err("undeclared call");
    assert!(err.contains("rustlite"), "{err}");

    let err = parse_policy("{ not json }", REASONING_V1).expect_err("bad manifest");
    assert!(err.starts_with("manifest:"), "{err}");

    // a manifest that parses but breaks its own rules is refused too.
    let err = parse_policy("{\"slug\":\"NOT A SLUG\",\"kind\":\"cognitive\",\"version\":\"0.1.0\",\"abi_version\":2}", REASONING_V1)
        .expect_err("invalid slug");
    assert!(err.starts_with("manifest:"), "{err}");
}

// ---- the hot-swap, which is the whole point -----------------------------

#[test]
fn swapping_the_policy_changes_the_next_prompt_and_keeps_the_memory() {
    let (mut c, _) = boot(REASONING_V1, None);
    assert_eq!(c.before("first", 0).prompt, "first");
    c.after("one", 0);
    let _ = c.take_flushes();

    let (slug, rehearsal) = c.swap_policy("", REASONING_V2).expect("v2 compiles and wires");
    assert_eq!(slug, REASONER_SLUG);
    // the candidate was made to run before it was installed, and the
    // rehearsal says what it did to the probe.
    assert_eq!(rehearsal.shaped, format!("[v2] {REHEARSAL_PROMPT}"));
    // both phases ran, so v2 remembered a prompt AND an answer — in the
    // scratch store, which is why the live one below is untouched by it.
    assert_eq!(
        rehearsal.wrote,
        vec!["last_answer".to_string(), "last_prompt".to_string()]
    );

    let notes = c.drain_notes();
    assert!(
        notes.iter().any(|n| n.contains("hot-swapped")),
        "the swap belongs on the feed: {notes:?}"
    );
    assert!(
        c.take_logs().iter().any(|n| n.contains("reasoning v2 up")),
        "the new module's init log belongs on the feed too"
    );

    // THE PROOF, minus the browser: the very next prompt is shaped by v2,
    // with no reboot and no lost memory.
    assert_eq!(c.before("second", 0).prompt, "[v2] second");
    assert_eq!(kv_of(&mut c, "last_answer").as_deref(), Some("one"));
    assert_eq!(kv_of(&mut c, "last_prompt").as_deref(), Some("second"));

    // and the swapped-in policy's writes are still durable.
    let body = flush_body(&mut c).expect("v2 wrote to kv");
    assert!(decode_kv(&body).unwrap().len() == 2);
}

#[test]
fn a_refused_swap_leaves_the_running_policy_alone() {
    let (mut c, _) = boot(REASONING_V1, None);
    c.before("before the bad swap", 0);
    let _ = c.take_flushes();

    let err = c
        .swap_policy("", "pub fn cart_handle(p: i32, n: i32) -> i64 { return oops(); }")
        .expect_err("a source that does not compile cannot be swapped in");
    assert!(err.contains("rustlite"), "{err}");

    // still v1, still remembering.
    assert_eq!(c.before("after", 0).prompt, "after");
    assert_eq!(kv_of(&mut c, "last_prompt").as_deref(), Some("after"));
    assert!(c.drain_notes().is_empty(), "a refusal is not an event");
}

// ---- v3: the first policy worth swapping to ------------------------------

/// what v3 makes of a prompt when nothing is standing.
fn plain(prompt: &str) -> String {
    format!("{prompt}\n\n{STANDING_PROTOCOL}")
}

#[test]
fn v3_appends_the_protocol_so_the_policy_teaches_its_own_contract() {
    // the module cannot edit the system prompt, so the only way the agent
    // learns the channel exists is the prompt itself.
    let (mut c, notes) = boot(REASONING_V3, None);
    assert!(
        notes.iter().any(|n| n.contains("reasoning v3 up: standing note")),
        "{notes:?}"
    );
    assert_eq!(c.before("first question", 0).prompt, plain("first question"));
    assert!(STANDING_PROTOCOL.contains(CARRY_MARKER));
}

#[test]
fn an_answer_carries_one_line_forward_and_only_when_it_says_to() {
    let (mut c, _) = boot(REASONING_V3, None);

    // no marker, no note. a policy that hoarded every answer would be v1
    // with extra steps.
    c.after("an ordinary answer with no marker", 0);
    assert_eq!(kv_of(&mut c, STANDING_KEY), None);
    assert_eq!(c.before("q", 0).prompt, plain("q"));

    // the marker takes the rest of ITS line, not the rest of the answer.
    c.after("reasoning first\nCARRY: the config mirror is vanish-config/config.json\nthen more prose", 0);
    assert_eq!(
        kv_of(&mut c, STANDING_KEY).as_deref(),
        Some("the config mirror is vanish-config/config.json")
    );

    assert_eq!(
        c.before("second question", 0).prompt,
        format!(
            "[standing note] the config mirror is vanish-config/config.json\n\nsecond question\n\n{STANDING_PROTOCOL}"
        )
    );
}

#[test]
fn a_marker_with_nothing_after_it_clears_the_note() {
    let (mut c, _) = boot(REASONING_V3, None);
    c.after("CARRY: something", 0);
    assert_eq!(kv_of(&mut c, STANDING_KEY).as_deref(), Some("something"));

    c.after("done with that\nCARRY:", 0);
    assert_eq!(kv_of(&mut c, STANDING_KEY).as_deref(), Some(""));
    // an empty note is not a note: the prompt goes back to plain.
    assert_eq!(c.before("q", 0).prompt, plain("q"));
}

#[test]
fn the_note_is_capped_so_it_cannot_grow_every_later_prompt() {
    let (mut c, _) = boot(REASONING_V3, None);
    let long = "x".repeat(STANDING_MAX + 250);
    c.after(&format!("CARRY: {long}"), 0);
    let held = kv_of(&mut c, STANDING_KEY).expect("something was stored");
    assert_eq!(held.len(), STANDING_MAX);
    assert!(held.chars().all(|ch| ch == 'x'));
}

#[test]
fn the_note_outlives_a_reload_which_is_the_whole_point() {
    // the transcript is trimmed at KEEP_MESSAGES and is per-conversation.
    // a cartridge's kv is neither — that is what v3 sells.
    let (mut first, _) = boot(REASONING_V3, None);
    first.after("CARRY: main is protected; promote through prs", 0);
    let body = flush_body(&mut first).expect("the note was written");
    drop(first);

    let (mut second, _) = boot(REASONING_V3, Some(&body));
    assert_eq!(
        second.before("a much later prompt", 0).prompt,
        format!(
            "[standing note] main is protected; promote through prs\n\na much later prompt\n\n{STANDING_PROTOCOL}"
        )
    );
}

#[test]
fn v3_can_be_swapped_in_over_v1_with_the_rehearsal_passing() {
    let (mut c, _) = boot(REASONING_V1, None);
    c.before("under v1", 0);
    let (slug, rehearsal) = c.swap_policy("", REASONING_V3).expect("v3 rehearses and installs");
    assert_eq!(slug, REASONER_SLUG);
    // with nothing standing, the rehearsal probe comes back plain — proof
    // the candidate ran, not merely compiled.
    assert_eq!(rehearsal.shaped, plain(REHEARSAL_PROMPT));
    assert_eq!(c.before("under v3", 0).prompt, plain("under v3"));
}

/// the rust-side mirrors of literals inside the rustlite module must not
/// drift: rustlite cannot be handed a constant, so nothing but this test
/// keeps `CARRY_MARKER`, `STANDING_KEY`, `STANDING_MAX` and the protocol
/// line in step with the source they describe.
#[test]
fn v3s_rust_side_constants_match_the_module_they_describe() {
    for needle in [
        format!("\"{CARRY_MARKER}\""),
        format!("\"{STANDING_KEY}\""),
        format!("{STANDING_MAX}"),
        format!("\"{STANDING_PROTOCOL}\""),
    ] {
        assert!(
            REASONING_V3.contains(&needle),
            "REASONING_V3 no longer contains {needle:?} — the rust mirror has drifted"
        );
    }
}

// ---- the rehearsal: a candidate must RUN, not merely compile -------------

const ALLOC: &str = r#"
    pub fn cart_alloc(size: i32) -> i32 {
        let hp: i32 = load_i32(0);
        if hp == 0 { hp = data_end(); }
        store_i32(0, hp + size);
        return hp;
    }
"#;

/// compiles, starts, writes a key, and THEN reads out of bounds. the exact
/// shape the rehearsal exists for: `parse_policy` accepts it and supervision
/// would only find it after it had already replaced a working policy.
fn trapping_policy() -> String {
    format!(
        r#"
        extern "C" {{ fn store_set(k_ptr: i32, k_len: i32, v_ptr: i32, v_len: i32) -> i32; }}
        {ALLOC}
        pub fn cart_init(p: i32, n: i32) -> i32 {{ return 0; }}
        pub fn cart_handle(p: i32, n: i32) -> i64 {{
            store_set(unpack_ptr("rehearsal_leak"), unpack_len("rehearsal_leak"), p, n);
            let boom: i32 = load_i32(2000000000);
            return 0;
        }}
    "#
    )
}

fn init_refusing_policy() -> String {
    format!(
        r#"
        {ALLOC}
        pub fn cart_init(p: i32, n: i32) -> i32 {{ return 7; }}
        pub fn cart_handle(p: i32, n: i32) -> i64 {{ return 0; }}
    "#
    )
}

#[test]
fn a_candidate_that_traps_on_its_first_message_never_reaches_the_loop() {
    let (mut c, _) = boot(REASONING_V1, None);
    c.before("the real prompt", 0);
    c.after("the real answer", 0);
    let _ = c.take_flushes();

    let err = c
        .swap_policy("", &trapping_policy())
        .expect_err("a module that traps on its first message must be refused");
    assert!(err.contains("failed on a prompt"), "{err}");

    // the running policy is untouched, its memory intact, and NOTHING the
    // candidate wrote during its rehearsal reached the live store.
    assert_eq!(c.before("after the refusal", 0).prompt, "after the refusal");
    assert_eq!(kv_of(&mut c, "last_answer").as_deref(), Some("the real answer"));
    assert_eq!(kv_of(&mut c, "rehearsal_leak"), None);
}

#[test]
fn a_candidate_that_refuses_its_own_init_is_refused() {
    let (mut c, _) = boot(REASONING_V1, None);
    let err = c
        .swap_policy("", &init_refusing_policy())
        .expect_err("cart_init returning nonzero is the module refusing");
    assert!(err.contains("refused to start"), "{err}");
    assert!(c.has_policy(), "the old policy is still wired");
}

#[test]
fn a_candidate_that_declares_no_reasoning_port_is_refused() {
    let (mut c, _) = boot(REASONING_V1, None);
    let manifest = "{\"slug\":\"reasoner\",\"kind\":\"cognitive\",\"version\":\"0.1.0\",\
                    \"abi_version\":2,\"provides\":[],\"requires\":[]}";
    let err = c
        .swap_policy(manifest, REASONING_V1)
        .expect_err("nothing would route to a module that provides no port");
    assert!(err.contains(PORT_BEFORE), "{err}");
    assert_eq!(c.before("still v1", 0).prompt, "still v1");
}

#[test]
fn the_rehearsal_runs_against_a_copy_of_the_memory_it_would_inherit() {
    // a policy that ONLY works when the store already holds something is the
    // reason the rehearsal seeds from the live kv rather than from nothing.
    let (mut c, _) = boot(REASONING_V1, None);
    c.before("seeded", 0);
    let live = c
        .orchestrator()
        .with_host(REASONER_SLUG, |h| h.snapshot())
        .unwrap();
    assert_eq!(live.len(), 1);
    // clear the pending flush from that prompt, so anything still dirty
    // afterwards can only have come from the rehearsal.
    let _ = c.take_flushes();

    let (manifest, bytes) = parse_policy("", REASONING_V2).unwrap();
    let r = rehearse(manifest, &bytes, live.clone(), REASONER_FUEL).expect("v2 rehearses");
    assert_eq!(r.shaped, format!("[v2] {REHEARSAL_PROMPT}"));
    assert_eq!(r.note, None);

    // and the live store is exactly where it was: the rehearsal wrote into a
    // scratch host that no longer exists.
    assert_eq!(
        c.orchestrator().with_host(REASONER_SLUG, |h| h.snapshot()).unwrap(),
        live
    );
    assert!(
        c.take_flushes().is_empty(),
        "the rehearsal dirtied the live host"
    );
}

// ---- the loop without a cartridge ---------------------------------------

#[test]
fn no_reasoning_is_the_identity_policy() {
    let r = NoReasoning;
    let shaped = r.before("untouched");
    assert_eq!(shaped.prompt, "untouched");
    assert!(shaped.notes.is_empty());
    assert!(r.after("an answer").is_empty());
}
