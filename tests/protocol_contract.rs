//! protocol contract tests — run natively by build.sh before the wasm
//! compile. the ui and worker halves of the harness can only drift if these
//! shapes stop agreeing with themselves; these tests are how that class of
//! bug becomes red instead of a blank page.

use vanish::agent::llm::{FunctionCall, Message, ToolCall};
use vanish::protocol::{
    Command, Config, ConversationSummary, Event, FinishReason, HistoryTurn,
};

/// every variant must survive serialize -> deserialize unchanged. this is
/// the exact path a Command takes from dom to worker and an Event takes on
/// the way back (serde_wasm_bindgen uses the same serde data model as
/// serde_json for these plain structs), so a regression here is a broken
/// wire format in production.
#[test]
fn commands_round_trip() {
    let cmds = vec![
        Command::Configure(Config {
            openrouter_key: "sk-or-test".into(),
            github_token: "gh_pat".into(),
            repo: "compusophy/vanish".into(),
            branch: "main".into(),
            model: "stealth/ox-alpha".into(),
            reasoning_effort: "high".into(),
            vercel_token: "vtk".into(),
            vercel_team_id: "team_x".into(),
            loop_mode: true,
        }),
        Command::Run {
            prompt: "do the thing".into(),
            thread_id: "1730000000000".into(),
        },
        Command::Stop,
        Command::Commit {
            message: "fix: something".into(),
        },
        Command::ListTree,
        Command::ReadFile { path: "README.md".into() },
        Command::WriteFile {
            path: "src/x.rs".into(),
            content: "fn main() {}".into(),
        },
        Command::ClearHistory,
        Command::NewConversation,
        Command::SwitchConversation { id: "42".into() },
        Command::DeleteConversation { id: "42".into() },
        Command::ListConversations,
        // phase-2 groundwork: a pool worker adopts one specific thread.
        Command::Attach { id: "conv-77".into() },
        // dock reconciliation: the ui's heartbeat ping and the worker's
        // answer. these exist because RunFinished can be delayed or lost
        // behind the transcript save, leaving the buttons stuck on stop.
        Command::RunState,
        // the programmatic driver: queued sequential tasks with results.
        Command::RunBatch {
            tasks: vec![
                vanish::protocol::BatchTask {
                    id: "bench-001".into(),
                    prompt: "add a doc comment to src/lib.rs".into(),
                },
                vanish::protocol::BatchTask {
                    id: "bench-002".into(),
                    prompt: "fix any clippy warnings".into(),
                },
            ],
        },
    ];

    for cmd in cmds {
        let json = serde_json::to_string(&cmd).expect("serialize");
        let back: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&cmd).unwrap(),
            "round-trip changed {json}"
        );
    }
}

#[test]
fn events_round_trip() {
    let events = vec![
        Event::Ready { build: "abc1234".into() },
        Event::ConfigStatus {
            openrouter_ok: true,
            github_ok: false,
            vercel_ok: Some(false),
            detail: "github token rejected".into(),
        },
        Event::RunStarted {
            thread_id: "t1".into(),
            model: "m".into(),
        },
        Event::StepStarted {
            thread: "t1".into(),
            step: 3,
        },
        Event::Reasoning {
            thread: "t1".into(),
            delta: "thinking...".into(),
        },
        Event::Content {
            thread: "t1".into(),
            delta: "saying...".into(),
        },
        Event::ToolStarted {
            thread: "t1".into(),
            id: "call_1".into(),
            name: "read_file".into(),
            args: "{}".into(),
        },
        Event::ToolFinished {
            thread: "t1".into(),
            id: "call_1".into(),
            name: "read_file".into(),
            ok: false,
            result: "boom".into(),
        },
        Event::TreeChanged { dirty: vec!["a.rs".into()] },
        Event::Committed {
            sha: "deadbee".into(),
            message: "msg".into(),
            files: 2,
        },
        Event::RunFinished {
            thread: "t1".into(),
            steps: 9,
            reason: FinishReason::Completed,
        },
        Event::Error {
            thread: "t1".into(),
            scope: "llm".into(),
            message: "provider error: x".into(),
        },
        Event::Note {
            thread: String::new(),
            text: "hello".into(),
        },
        // batch export event: the live notification that pairs with the
        // vanish-batch/results.json file.
        Event::BatchFinished {
            status: "completed".into(),
            results: vec![vanish::protocol::BatchResult {
                id: "bench-001".into(),
                reason: "completed".into(),
                steps: 4,
            }],
        },
        Event::Tree {
            entries: vec![],
        },
        Event::FileContent {
            path: "f.rs".into(),
            content: "code".into(),
        },
        Event::HistoryRestored {
            turns: vec![HistoryTurn {
                role: "user".into(),
                content: Some("hi".into()),
                tools: vec![],
            }],
            trimmed: 1,
        },
        Event::HistoryCleared,
        Event::Conversations {
            items: vec![ConversationSummary {
                id: "1".into(),
                title: "t".into(),
                count: 3,
            }],
            active: "1".into(),
        },
    ];

    for ev in events {
        let json = serde_json::to_string(&ev).expect("serialize");
        let back: Event = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&ev).unwrap(),
            "round-trip changed {json}"
        );
    }
}

/// the whole point of #[serde(default)] on Config: a config saved by an
/// older build that lacks a newer field must still load. this is the
/// regression that once wiped users' credentials on every deploy.
#[test]
fn config_from_old_build_still_loads() {
    // a pre-vercel-fields config: no vercel_token, no vercel_team_id,
    // no loop_mode.
    let old = r#"{
        "openrouter_key": "sk-or-old",
        "github_token": "tok",
        "repo": "compusophy/vanish",
        "branch": "main",
        "model": "stealth/ox-alpha",
        "reasoning_effort": "high"
    }"#;
    let cfg: Config = serde_json::from_str(old).expect("old config must parse");
    assert_eq!(cfg.openrouter_key, "sk-or-old");
    assert_eq!(cfg.vercel_team_id, "", "missing fields default to empty");
    assert!(!cfg.loop_mode);
    assert!(cfg.is_usable());
}

/// a future field added after this test was written must not break loading
/// today's stored config either.
#[test]
fn config_with_unknown_future_fields_still_loads() {
    let future = r#"{
        "openrouter_key": "k",
        "github_token": "g",
        "repo": "r",
        "some_field_that_does_not_exist_yet": {"nested": [1,2,3]}
    }"#;
    let cfg: Config = serde_json::from_str(future).expect("unknown fields are ignored");
    assert!(cfg.is_usable());
}

// ---- event routing tag -------------------------------------------------

#[test]
fn thread_tag_routes_correctly() {
    // run-scoped events carry their conversation; the feed uses this to
    // keep background threads out of the visible stream.
    let tagged = Event::StepStarted {
        thread: "conv-42".into(),
        step: 1,
    };
    assert_eq!(tagged.thread(), "conv-42");

    let untagged = Event::Note {
        thread: String::new(),
        text: "boot".into(),
    };
    assert_eq!(untagged.thread(), "");

    // non-run-scoped variants report no thread at all.
    assert_eq!(Event::Ready { build: "x".into() }.thread(), "");
    assert_eq!(Event::HistoryCleared.thread(), "");
}

/// events written BEFORE the thread tag existed (an older worker binary
/// still running against a newer ui across an ota reload) must parse, with
/// the missing tag reading as "".
#[test]
fn events_without_thread_tag_parse_as_empty() {
    let legacy = r#"{"ev":"step_started","step":5}"#;
    let ev: Event = serde_json::from_str(legacy).expect("legacy payload parses");
    assert_eq!(ev.thread(), "");
    match ev {
        Event::StepStarted { step, .. } => assert_eq!(step, 5),
        _ => panic!("wrong variant decoded"),
    }
}

// ---- finish reasons -----------------------------------------------------

#[test]
fn finish_reasons_are_stable_strings() {
    // these cross the wire as snake_case; renaming them would orphan every
    // saved transcript's display logic.
    for (reason, expected) in [
        (FinishReason::Completed, "\"completed\""),
        (FinishReason::Stopped, "\"stopped\""),
        (FinishReason::StepLimit, "\"step_limit\""),
        (FinishReason::Failed, "\"failed\""),
    ] {
        assert_eq!(serde_json::to_string(&reason).unwrap(), expected);
    }
}

// ---- dock reconciliation -------------------------------------------------

/// the stuck-stop-button bug: RunFinished crossing the worker boundary can
/// be delayed or lost, and the buttons only flip back on it. the heartbeat
/// (Command::RunState -> Event::RunStateReport) is the correction channel,
/// so its wire shape is pinned here.
#[test]
fn run_state_report_round_trips() {
    for running in [true, false] {
        let ev = Event::RunStateReport { running };
        let json = serde_json::to_string(&ev).expect("serialize");
        let back: Event = serde_json::from_str(&json).expect("deserialize");
        match back {
            Event::RunStateReport { running: r } => assert_eq!(r, running),
            other => panic!("wrong variant decoded: {other:?}"),
        }
    }
}

/// run start/finish are DOCK-level facts. even when a run belongs to a
/// background conversation whose events get routed away from the visible
/// feed, the start/finish pair must reach the dock handler — otherwise one
/// thread's finish leaves the shared stop button stuck.
#[test]
fn run_state_events_always_reach_the_dock() {
    let starts = vec![
        Event::RunStarted {
            thread_id: "some-other-thread".into(),
            model: "stealth/ox-alpha".into(),
        },
        Event::RunFinished {
            thread: "some-other-thread".into(),
            steps: 3,
            reason: FinishReason::Completed,
        },
    ];
    for ev in starts {
        assert!(
            ev.touches_run_state(),
            "{ev:?} must bypass background routing"
        );
    }

    // everything else keeps the router's protection: ordinary traffic from
    // another thread stays out of the visible feed.
    let routed = vec![
        Event::Content {
            thread: "other".into(),
            delta: "text".into(),
        },
        Event::Note {
            thread: "other".into(),
            text: "n".into(),
        },
        Event::Error {
            thread: "other".into(),
            scope: "s".into(),
            message: "m".into(),
        },
    ];
    for ev in routed {
        assert!(
            !ev.touches_run_state(),
            "{ev:?} should remain routable as background traffic"
        );
    }

    // the heartbeat itself must not drive set_status — it is a background
    // signal that would clobber the visible status line mid-run.
    let report = Event::RunStateReport { running: true };
    assert!(!report.touches_run_state());
}

// ---- message shapes ------------------------------------------------------

#[test]
fn messages_round_trip_and_skip_none_fields() {
    let m = Message::system("you are vanish");
    let j = serde_json::to_string(&m).unwrap();
    assert!(!j.contains("tool_calls"), "None fields are omitted");
    let back: Message = serde_json::from_str(&j).unwrap();
    assert_eq!(back.role, "system");
    assert_eq!(back.content.as_deref(), Some("you are vanish"));

    let tr = Message::tool_result("call_9", "result text");
    let j = serde_json::to_string(&tr).unwrap();
    assert!(j.contains("\"tool_call_id\":\"call_9\""));
    assert!(j.contains("\"role\":\"tool\""));
}

/// reconstructing a Message from exactly what the api sends back must give
/// the same struct we would have built by hand — this is what makes
/// multi-turn tool use work at all.
#[test]
fn assistant_message_with_tool_calls_survives_the_wire() {
    let m = Message {
        role: "assistant".into(),
        content: None,
        tool_calls: Some(vec![ToolCall {
            id: "call_a".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "edit_file".into(),
                arguments: "{\"path\":\"src/x.rs\",\"target\":\"a\",\"replacement\":\"b\"}".into(),
            },
        }]),
        tool_call_id: None,
    };

    let json = serde_json::to_string(&m).unwrap();
    let back: Message = serde_json::from_str(&json).unwrap();
    let calls = back.tool_calls.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].function.name, "edit_file");
    // arguments stay a STRING on purpose: the api requires it, and parsing
    // is the dispatcher's job, done where errors can be reported per-call.
    assert!(calls[0].function.arguments.contains("src/x.rs"));
}
