//! tests for the streaming reassembly logic — the code that turns a
//! provider's fragmented SSE frames into complete tool calls.
//!
//! this is the highest-consequence pure logic in the crate: every multi-tool
//! agent turn flows through it. a regression here does not crash anything;
//! it silently feeds the loop corrupted arguments, which then fail three
//! steps later in ways that look like model mistakes rather than harness
//! bugs. these tests are why that class of failure is now impossible to
//! ship.

use vanish::agent::llm::{absorb_chunk, finalize_turn, PartialCall, Turn};
use std::collections::BTreeMap;

fn turn_from_frames(frames: &[&str]) -> Turn {
    let mut turn = Turn::default();
    let mut partials: BTreeMap<usize, PartialCall> = BTreeMap::new();
    for f in frames {
        assert!(
            absorb_chunk(f, &mut turn, &mut partials),
            "frame should not terminate the stream: {f}"
        );
    }
    finalize_turn(&mut turn, partials);
    turn
}

#[test]
fn content_and_reasoning_accumulate_across_frames() {
    let t = turn_from_frames(&[
        r#"{"choices":[{"delta":{"reasoning":"think "}}]}"#,
        r#"{"choices":[{"delta":{"reasoning":"hard"}}]}"#,
        r#"{"choices":[{"delta":{"content":"Hello "}}]}"#,
        r#"{"choices":[{"delta":{"content":"world"}}]}"#,
    ]);
    assert_eq!(t.reasoning, "think hard");
    assert_eq!(t.content, "Hello world");
}

#[test]
fn done_frame_stops_the_stream() {
    let mut turn = Turn::default();
    let mut partials: BTreeMap<usize, PartialCall> = BTreeMap::new();
    // absorb_chunk returns false on [DONE] — the caller breaks its loop.
    assert!(!absorb_chunk("[DONE]", &mut turn, &mut partials));
}

#[test]
fn malformed_frames_are_skipped_not_fatal() {
    // a torn frame mid-stream must not kill a long run.
    let t = turn_from_frames(&[
        r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
        r#"{"choi"#, // torn mid-frame
        r#""#,       // empty payload
        r#"{"choices":[{"delta":{"content":" still fine"}}]}"#,
    ]);
    assert_eq!(t.content, "ok still fine");
}

#[test]
fn provider_error_frame_is_recorded() {
    let mut turn = Turn::default();
    let mut partials: BTreeMap<usize, PartialCall> = BTreeMap::new();
    let keep_going = absorb_chunk(
        r#"{"error":{"message":"rate limited"}}"#,
        &mut turn,
        &mut partials,
    );
    // false: the stream must stop.
    assert!(!keep_going);
    assert_eq!(turn.error.as_deref(), Some("provider error: rate limited"));
}

/// the core scenario: one tool call whose name and arguments arrive in
/// fragments across many frames, keyed only by index. this is exactly what
/// openrouter sends for any nontrivial tool call.
#[test]
fn fragmented_tool_call_reassembles() {
    let frames = [
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"edit_"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"file","arguments":"{\"path\":"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"src/x.rs\",\"replacement\":\"y\"}"}}]}}]}"#,
    ];
    let t = turn_from_frames(&frames);

    assert_eq!(t.tool_calls.len(), 1);
    let call = &t.tool_calls[0];
    assert_eq!(call.id, "call_abc");
    assert_eq!(call.function.name, "edit_file");
    // arguments are valid json — parseable, because the dispatcher will.
    let args: serde_json::Value =
        serde_json::from_str(&call.function.arguments).expect("reassembled arguments parse");
    assert_eq!(args["path"], "src/x.rs");
    assert_eq!(args["replacement"], "y");
}

/// two calls interleaved by index must land in separate slots, in index
/// order, with no cross-contamination of their argument fragments.
#[test]
fn two_interleaved_tool_calls_stay_separate() {
    let frames = [
        // call 0 starts
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c0","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}"#,
        // call 1 starts before 0 finishes — real streams do this
        r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c1","function":{"name":"write_file","arguments":"{\"p"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"ath\":\"b.rs\",\"content\":\"x\"}"}}]}}]}"#,
    ];
    let t = turn_from_frames(&frames);

    assert_eq!(t.tool_calls.len(), 2);
    assert_eq!(t.tool_calls[0].id, "c0");
    assert_eq!(t.tool_calls[1].id, "c1");

    let args0: serde_json::Value = serde_json::from_str(&t.tool_calls[0].function.arguments).unwrap();
    assert_eq!(args0["path"], "a.rs"); // c0 got only c0's fragments

    let args1: serde_json::Value = serde_json::from_str(&t.tool_calls[1].function.arguments).unwrap();
    assert_eq!(args1["path"], "b.rs");
    assert_eq!(args1["content"], "x");
}

/// a provider that sends a name-only call with no arguments at all must
/// still produce a dispatchable "{}" — otherwise the dispatcher's
/// serde_json::from_str("") fails and the whole call errors for no reason.
#[test]
fn argumentless_call_becomes_empty_object() {
    let frames = [
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c9","function":{"name":"sync_repo"}}]}}]}"#,
    ];
    let t = turn_from_frames(&frames);
    assert_eq!(t.tool_calls.len(), 1);
    assert_eq!(t.tool_calls[0].function.arguments, "{}");
}

/// a slot that received fragments but never a name is noise from the
/// provider; it must be dropped, not dispatched as an unnamed call.
#[test]
fn nameless_slot_is_dropped() {
    let frames = [
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"cx","function":{"arguments":"{}"}}]}}]}"#,
    ];
    let t = turn_from_frames(&frames);
    assert!(t.tool_calls.is_empty(), "unnamed partials are dropped");
}

#[test]
fn missing_id_falls_back_to_name_derived_id() {
    let frames = [
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"list_dir"}}]}}]}"#,
    ];
    let t = turn_from_frames(&frames);
    assert_eq!(t.tool_calls[0].id, "call_list_dir");
}

#[test]
fn finish_reason_is_captured_even_with_tool_calls() {
    let t = turn_from_frames(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"git_commit"}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    assert_eq!(t.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(t.tool_calls.len(), 1);
}

/// frames with no choices field (keep-alives, usage pings) are ignored.
#[test]
fn keepalive_frames_are_ignored() {
    let t = turn_from_frames(&[
        r#"{"id":"x","object":"chat.completion.chunk"}"#,
        r#"{"choices":[],"usage":{"total_tokens":42}}"#,
        r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
    ]);
    assert_eq!(t.content, "hi");
}
