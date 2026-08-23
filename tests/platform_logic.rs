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

    // and an index saved before loop_resume existed parses with None.
    let old = r#"{"active":"a","items":[]}"#;
    let legacy: Index = serde_json::from_str(old).unwrap();
    assert!(legacy.loop_resume.is_none());
}

fn conv(id: &str, updated: f64) -> vanish::platform::transcript::ConversationMeta {
    vanish::platform::transcript::ConversationMeta {
        id: id.into(),
        title: format!("thread {id}"),
        updated,
        count: 5,
    }
}
