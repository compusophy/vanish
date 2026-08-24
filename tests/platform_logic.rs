//! pure-logic tests for the platform layer. opfs itself needs a browser,
//! but the guards around it are plain functions — and they are exactly the
//! parts a hostile or confused tool call would hit first.

use vanish::platform::opfs::normalize;

// ---- path normalization -------------------------------------------------
// this is the traversal guard: everything written to the working tree goes
// through it. a regression here means write_file can escape the repo.

#[test]
fn clean_paths_pass_through() {
    for p in ["README.md", "src/agent/mod.rs", "a/b/c/d.txt", "memory/TASKBOARD.md"] {
        assert!(normalize(p).is_ok(), "{p} should be accepted");
    }
}

#[test]
fn backslashes_are_normalized_to_forward_slashes() {
    let parts = normalize("src\\ui\\feed.rs").expect("windows-style separators are normalized");
    assert_eq!(parts, vec!["src".to_string(), "ui".to_string(), "feed.rs".to_string()]);
}

#[test]
fn dot_segments_and_duplicate_slashes_collapse() {
    let parts = normalize("./src//agent/./mod.rs").unwrap();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0], "src");
    assert_eq!(parts[2], "mod.rs");
}

#[test]
fn traversal_is_rejected() {
    // each of these must be an Err, never a silent success.
    for p in [
        "../escape.txt",
        "src/../../etc/passwd",
        "..",
        "a/../..",
    ] {
        let result = normalize(p);
        assert!(result.is_err(), "{p} must not normalize");
        assert!(
            result.unwrap_err().contains("escapes"),
            "the error should say why"
        );
    }
}

#[test]
fn empty_path_is_rejected() {
    assert!(normalize("").is_err());
    // "./" and "//" collapse to nothing, which is also empty.
    assert!(normalize("./").is_err());
    assert!(normalize("//").is_err());
}

// ---- transcript index logic ---------------------------------------------
// these exercise the real functions from src/platform/transcript.rs; they
// take no js because their inputs and outputs are all plain data.

use vanish::platform::transcript::{title_from, Index, LoopResume};
use vanish::agent::llm::Message;

fn user_msg(text: &str) -> Message {
    Message::user(text)
}

#[test]
fn title_from_first_user_line() {
    let msgs = vec![
        Message::system("sys prompt"),
        Message::user("please refactor the feed module\nit has grown large"),
    ];
    assert_eq!(title_from(&msgs), "please refactor the feed module");
}

#[test]
fn title_truncates_on_char_boundaries_not_bytes() {
    // 60 multi-byte chars: slicing bytes at 48 would panic mid-codepoint.
    let long = "é".repeat(60);
    let msgs = vec![user_msg(&long)];
    let t = title_from(&msgs);
    assert_eq!(t.chars().count(), 49); // 48 chars + ellipsis
    assert!(t.ends_with('…'));
}

#[test]
fn empty_or_blank_conversations_get_the_neutral_title() {
    assert_eq!(title_from(&[]), "new conversation");
    assert_eq!(title_from(&[user_msg("   \n  \n")]), "new conversation");
    // a thread with only a system message too.
    assert_eq!(title_from(&[Message::system("x")]), "new conversation");
}

#[test]
fn index_sorts_most_recently_updated_first() {
    let mut idx = Index {
        active: String::new(),
        items: vec![
            conv("1", 100.0),
            conv("2", 300.0),
            conv("3", 200.0),
        ],
        loop_resume: None,
    };
    let sorted = idx.sorted();
    assert_eq!(
        sorted.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        vec!["2", "3", "1"]
    );
    // sorted() is a view; the index itself is unchanged.
    assert_eq!(idx.items[0].id, "1");

    // and it must not reorder in place, which would corrupt save_index.
    idx.active = "x".into();
    assert_eq!(idx.items.len(), 3);
}

#[test]
fn loop_resume_marker_round_trips_with_defaults() {
    let marker = LoopResume {
        conversation: "conv-9".into(),
        prompt: "keep improving".into(),
        interrupted_at: 1_700_000_000_000.0,
        loop_mode: true,
    };
    let idx = Index {
        active: "conv-9".into(),
        items: vec![],
        loop_resume: Some(marker.clone()),
    };
    let json = serde_json::to_string(&idx).unwrap();
    let back: Index = serde_json::from_str(&json).unwrap();
    let m = back.loop_resume.expect("marker survives");
    assert_eq!(m.conversation, "conv-9");
    assert_eq!(m.prompt, "keep improving");
    assert!(m.loop_mode);

    // a plain run's marker round-trips with loop_mode false.
    let plain = serde_json::to_string(&Index {
        active: "conv-9".into(),
        items: vec![],
        loop_resume: Some(LoopResume {
            conversation: "conv-9".into(),
            prompt: "one-shot task".into(),
            interrupted_at: 1_700_000_000_001.0,
            loop_mode: false,
        }),
    })
    .unwrap();
    let back: Index = serde_json::from_str(&plain).unwrap();
    assert!(!back.loop_resume.unwrap().loop_mode);

    // and an index saved before loop_resume existed parses with None.
    let old = r#"{"active":"a","items":[]}"#;
    let legacy: Index = serde_json::from_str(old).unwrap();
    assert!(legacy.loop_resume.is_none());
}

#[test]
fn markers_from_before_every_run_resumed_still_parse_as_loop_runs() {
    // markers written by older builds had no loop_mode field; they were
    // always loop runs, so absence must read as true — otherwise an ota
    // reload mid-loop would come back as a one-shot resume.
    let legacy = r#"{
        "active":"c1",
        "items":[],
        "loopResume":{
            "conversation":"c1",
            "prompt":"keep improving",
            "interruptedAt":1700000000000
        }
    }"#;
    let idx: Index = serde_json::from_str(legacy).unwrap();
    let m = idx.loop_resume.expect("legacy marker parses");
    assert!(m.loop_mode, "absent loop_mode must default to true");
}

fn conv(id: &str, updated: f64) -> vanish::platform::transcript::ConversationMeta {
    vanish::platform::transcript::ConversationMeta {
        id: id.into(),
        title: format!("thread {id}"),
        updated,
        count: 5,
    }
}

// ---- clock formatting -----------------------------------------------------
// the `now` tool reads the browser clock, which needs a browser — but the
// calendar conversion it feeds into is pure rust, and these pins are what
// stop a silent regression from making the agent report wrong dates forever
// (the exact failure mode that motivated the tool: guessing dates).

use vanish::agent::tools::{format_timestamp_iso, format_timestamp_readable};

#[test]
fn epoch_zero_formats_correctly() {
    assert_eq!(format_timestamp_iso(0), "1970-01-01T00:00:00Z");
    // january 1st 1970 was a thursday.
    assert_eq!(
        format_timestamp_readable(0),
        "Thursday, January 1, 1970 · 00:00 UTC"
    );
}

#[test]
fn known_modern_timestamps_round_trip() {
    // 2023-11-14T22:13:20Z, cross-checked against timeapi.io when the tool landed.
    let t = 1_700_000_000_000i64;
    assert_eq!(format_timestamp_iso(t), "2023-11-14T22:13:20Z");
    assert_eq!(
        format_timestamp_readable(t),
        "Tuesday, November 14, 2023 · 22:13 UTC"
    );
}

#[test]
fn leap_day_is_rendered_not_skipped() {
    // 2024-02-29T00:00:00Z. a days-to-(y,m,d) bug classically lands on
    // march 1st here.
    let t = 1_709_164_800_000i64;
    assert_eq!(format_timestamp_iso(t), "2024-02-29T00:00:00Z");
}

#[test]
fn pre_epoch_dates_use_floored_not_truncated_division() {
    // -1 day is 1969-12-31, not 1969-12-30 or 1970-01-00. truncating
    // division on negatives would corrupt every pre-1970 timestamp.
    assert_eq!(format_timestamp_iso(-86_400_000), "1969-12-31T00:00:00Z");
    // and one second before the epoch stays on the same day.
    assert_eq!(format_timestamp_iso(-1_000), "1969-12-31T23:59:59Z");
}

#[test]
fn sub_second_precision_is_dropped_not_rounded() {
    // 23:59:59.999 must truncate to 23:59:59, never roll over to the next
    // minute/day.
    assert_eq!(format_timestamp_iso(999), "1970-01-01T00:00:00Z");
    assert_eq!(format_timestamp_iso(86_399_999), "1970-01-01T23:59:59Z");
}

#[test]
fn year_boundaries_roll_over() {
    // 1999-12-31T23:59:59Z plus one second is 2000-01-01T00:00:00Z.
    let new_years_eve = 946_684_799_000i64;
    assert_eq!(format_timestamp_iso(new_years_eve), "1999-12-31T23:59:59Z");
    assert_eq!(format_timestamp_iso(new_years_eve + 1_000), "2000-01-01T00:00:00Z");
}

// ---- sync reconciliation (D10) --------------------------------------------
// incidents #1–#3 were all one mechanism: a local cache that lagged the
// branch and was trusted anyway. reconcile_entry decides, per file, whether
// sync_repo may drop a cached copy so it re-fetches from github. the pins
// here are not stylistic — the first one is the difference between a stale
// read and destroyed uncommitted work.

use vanish::agent::tools::{reconcile_entry, Reconcile};

#[test]
fn dirty_files_are_never_refreshed_even_when_upstream_moved() {
    // THE invariant: a dirty file is uncommitted work. dropping its cache
    // would lose the only copy of that work. upstream movement, unknown
    // shas, even an empty base sha must all lose to this.
    for base in ["", "stale-sha", "abc123"] {
        assert_eq!(
            reconcile_entry(true, base, Some("brand-new-sha")),
            Reconcile::Keep,
            "dirty file with base '{base}' must be kept"
        );
        assert_eq!(reconcile_entry(true, base, None), Reconcile::Keep);
    }
}

#[test]
fn clean_files_matching_the_branch_are_kept() {
    assert_eq!(
        reconcile_entry(false, "abc123", Some("abc123")),
        Reconcile::Keep
    );
}

#[test]
fn clean_files_that_diverged_are_refreshed() {
    // the branch reports a different blob sha than the cache recorded:
    // upstream moved, the cache did not. this is exactly incident #3.
    assert_eq!(
        reconcile_entry(false, "old-sha", Some("new-sha")),
        Reconcile::Refresh
    );
}

#[test]
fn files_missing_from_the_branch_listing_are_refreshed() {
    // remote_sha None = the path no longer exists on the branch. a clean
    // cached copy of a deleted file serves ghost content.
    assert_eq!(reconcile_entry(false, "whatever", None), Reconcile::Refresh);
}

#[test]
fn an_unrecorded_base_sha_is_distrusted_not_trusted() {
    // read-through caches are written with an empty base sha because the
    // contents api does not hand back the blob sha. "unknown" must mean
    // refresh — trusting an unverifiable cache is how three incidents
    // happened.
    assert_eq!(
        reconcile_entry(false, "", Some("some-sha")),
        Reconcile::Refresh
    );
}
