//! guards against the one bug that has bricked this app twice.
//!
//! the worker is single-threaded. `await`ing an already-resolved promise
//! only drains the microtask queue, so a `while` loop that re-queues one
//! every iteration never yields to the event loop at all. nothing else can
//! run: not the opfs write the loop is waiting on (so its condition never
//! clears — it spins at 100% of a core forever), and critically not
//! `onmessage`. a worker in that state stops receiving Stop, RunState, and
//! every subsequent Run, so the ui sits on "running" with a dead stop button
//! and no error anywhere. from the outside the app is simply bricked.
//!
//! it was fixed in 699ada0 by yielding through a timer instead, and silently
//! reverted in 016f3db by a refactor that rewrote the surrounding block.
//! comments did not survive that; this test will.
//!
//! these are source-level invariants rather than behavioral assertions
//! because the failure lives in wasm-only async code that the native test
//! binary cannot drive. a grep is a blunt instrument, but a blunt instrument
//! that fires beats a comment that gets refactored away.

use std::path::Path;

fn source(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("could not read {rel}: {e}"))
}

/// strip `//` comments so prose describing the hazard is never mistaken for
/// the hazard itself — this file's own subject matter makes that likely.
fn code_only(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn worker_never_busy_waits_on_a_resolved_promise() {
    let code = code_only(&source("src/worker.rs"));

    // the shape of the bug: constructing a promise whose executor resolves
    // immediately, in order to `await` it as a yield point.
    assert!(
        !code.contains("resolve.call0"),
        "src/worker.rs resolves a promise immediately to use as a yield point. \
         awaiting one only drains microtasks — in a loop it starves the event \
         loop, the worker stops dispatching onmessage, and stop/run/health \
         checks are never seen again. yield through a timer (sleep_ms) instead."
    );
}

#[test]
fn end_of_run_drain_is_bounded() {
    let code = code_only(&source("src/worker.rs"));

    // whatever the wait is spelled as, it must have a ceiling: a checkpoint
    // that never completes must cost a stale transcript, never the user's
    // control of the app.
    assert!(
        code.contains("DRAIN_TIMEOUT_MS"),
        "the end-of-run checkpoint drain must be bounded by DRAIN_TIMEOUT_MS; \
         an unbounded wait on an opfs write is indistinguishable from a hang."
    );
    assert!(
        code.contains("waited_ms < DRAIN_TIMEOUT_MS"),
        "DRAIN_TIMEOUT_MS exists but is not being used as the drain's ceiling."
    );
}

#[test]
fn stop_has_an_escape_hatch() {
    let code = code_only(&source("src/worker.rs"));

    // stop is the only way out of a wedged run, so it must not itself depend
    // on the run being healthy enough to notice it.
    assert!(
        code.contains("STOP_GRACE_MS"),
        "Command::Stop must take control back by force after STOP_GRACE_MS. \
         a cooperative-only stop leaves `running` true forever when the run \
         is not polling, and every later Run is refused with 'a run is \
         already in progress' with no way out but a reload."
    );
}

#[test]
fn a_forced_stop_cannot_leave_a_zombie_run_working() {
    let code = code_only(&source("src/worker.rs"));

    // forcing `running` false is only safe if the abandoned run is also told
    // to stop. run_seq is that signal: the hatch bumps it, and the run's own
    // stop predicate compares against the seq it captured at birth.
    assert!(
        code.contains("run_seq"),
        "the stop escape hatch needs run_seq to invalidate the abandoned run; \
         without it, forcing `running` false leaves a future that keeps \
         streaming tokens nobody can see or stop."
    );
    assert!(
        code.contains("st.run_seq != seq || st.stop_requested"),
        "a run must treat being superseded as a stop signal, not just an \
         explicit stop request — otherwise a forced stop only hides the run."
    );
}

#[test]
fn run_finished_is_emitted_before_the_transcript_save() {
    let code = code_only(&source("src/worker.rs"));

    let finished = code
        .find("Event::RunFinished")
        .expect("spawn_run must emit RunFinished");
    let save = code
        .find("transcript::save(&conversation_id, &history_snapshot)")
        .expect("spawn_run must save the finished transcript");

    // the dock's buttons flip on RunFinished. anything in front of it —
    // a drain, a save, an await on storage — can hold the ui hostage.
    assert!(
        finished < save,
        "RunFinished must be emitted before the final transcript save; \
         durability work belongs after the user-visible transition, never \
         in front of it."
    );
}
