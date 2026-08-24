# vanish run status — persistent context for the next run

> the agent has no memory between runs. this file is the memory.
> update it at the end of every run. read it first thing every run.

## landed this run (batch/task-queue mode — build order item 2 DONE, 2249454)

vanish is now SCORABLE. the programmatic driver exists:

- **Command::RunBatch { tasks: Vec<BatchTask> }** — each task {id, prompt}
  runs sequentially through start_run (the SAME path as a typed prompt, so
  batch behavior cannot diverge from interactive behavior).
- **results export**: opfs `vanish-batch/results.json` rewritten after EVERY
  task (a harness can poll mid-batch) + Event::BatchFinished on the wire.
  BatchResult = {id, reason ("completed"|"stopped"|"step_limit"|"failed"),
  steps}.
- **durability rides the resume machinery**: BatchState persists in the
  transcript index beside LoopResume; boot resumes a parked batch BEFORE
  marker handling (a batch can be interrupted between tasks — no marker,
  no run in flight), and unlike the marker it is NOT cleared on read (it
  must survive repeated boots until drained or cancelled). stop cancels
  remaining queue; failures/step-limits are recorded and the batch continues.
- **enqueue_batch(tasks_json)** wasm export = the external entry point.
- tests: RunBatch/BatchFinished protocol round-trips; 5 new evals pinning
  BatchState advance/order/serde-round-trip/cancelled-export/empty-reject.

### how this run went (5 red builds before green — honest accounting)

every failure named by build logs, but the pattern matters more than any
one error:
1. E0308 &str→String, then its own follow-up one line later (&str param at
   the call site the first fix's diff should have included). lesson: after
   fixing a signature mismatch, re-read EVERY use of the value you touched.
2. E0004 non-exhaustive tuple match on (Option, Option) where one arm was
   impossible — restructured to map-then-match so the impossible state
   cannot be expressed at all.
3. E0433 crate::protocol in an integration test → vanish::protocol.
   rule 2b's sibling: inside tests/, the crate is the EXTERN, not crate::.
4. E0063 missing `batch` field in THREE Index literals in platform_logic.rs
   — the exact failure class I had documented IN THE PREVIOUS COMMIT
   MESSAGE (5e79eff precedent), committed anyway. grep literals across src/
   AND tests/ when a serde shape gains a field; serde(default) hides gaps
   everywhere except literals. THIS IS NOW THE MOST-REPEATED SELF-INFLICTED
   ERROR IN THE PROJECT'S HISTORY (3rd occurrence).
5. unclosed delimiter: my scenario-6 insert REPLACED the seeding test's
   tail instead of following it. edit_file with an insert-shaped target
   must include everything it displaces; full-file re-read after repair.

the meta-lesson: these were all mechanical errors visible to cargo check.
the deploy pipeline is ~4min; each red cost ~8min of main pinned. until a
local compile gate exists, the cheapest defense is slowing down at commit
time: re-read every changed hunk + grep for shape-literal fallout BEFORE
git_commit, because the build log only tells you afterward what patience
would have said before.

## landed earlier this session (auto-reconcile + clippy gate — item 1)

- [x] **auto-reconcile at session start** (7b9c43a + fixes): the worker runs
      the D10 reconcile pass on the FIRST Configure whose github token
      VERIFIES (boot time has no credentials — they travel with Configure,
      so that is the earliest correct moment; the taskboard's old "on
      Event::Ready" wording was wrong and is corrected). observable: a
      visible "⇅ tree reconciled against <sha> at boot" note (or loud Error,
      latch stays unset so next Configure retries); sync_repo and the boot
      path share Workspace::reconcile_against_branch so they cannot drift;
      and agent::run seeds its workspace from worker::reconciled_head(),
      which CLOSES THE D10 BLIND SPOT where a session's first commit passed
      the refusal because synced_head was empty — exactly when a stale-tree
      commit was most dangerous.
- [x] **fatal clippy gate in build.sh** (after test suites, so logic
      failures still report first). its first pass caught 6 pre-existing
      lints + 1 dead function (reject_while_running, dead since 51815df).
- [x] 71 native tests green, incl. 3 new pins for should_auto_reconcile
      (fires once per session for verified tokens only; never re-runs; never
      arms on an unverified token).

### what four red builds taught THIS time (all self-inflicted, all named by logs)

1. **E0308**: closure param &str into String field → .to_string(). trivial,
   but it shipped because I did not recompile mentally after writing the
   accessor's signature.
2. **clippy's suggestion can be wrong for context**: map_err(|e| e) → .ok()
   type-errors when the outer combinator is Result::and_then. read the
   error, not just the fix-it hint. final form:
   `.and_then(JsCast::dyn_into::<Function>)` — Result→Result, no closure,
   no deprecated try_from. two more builds burned learning this.
3. **the deploy pipeline is the slowest feedback loop in this repo** (~4min
   wasm build). there is NO local cargo check here — but the cost of a red
   commit is ~8 minutes of main pinned to the previous build. mental
   type-checking before committing source is worth real minutes.
4. **a warning gate that only warns is a gate nobody reads** — proven same-
   day: -D warnings surfaced six lints that had sat invisible for weeks.

---

## benchmark readiness assessment (earlier run, still current)

asked: "are we ready for a third party benchmark, or is there still a lot to
build until v1?" verdict: **not ready — but the reliability core is closer
than the board reads.** main was clean and green at 73163c1 (verified via
check_deployment + commit log; no stale-tree surprises).

would survive scrutiny today:
- durability layer (opfs write-through, per-step checkpoints,
  resume-after-discard with universal LoopResume markers)
- self-correction (retry budget w/ backoff, check_deployment with real
  compiler logs, history_is_well_formed asserted on every exit path)
- a deploy-gated test layer (7 suites incl. behavioral evals with negative
  controls) — structurally, this IS a judge; it just doesn't score TASKS.

three disqualifiers:
1. **no programmatic driver.** the only task-entry point is the prompt box;
   transcripts live in opfs per conversation with no result export. a
   benchmark harness cannot invoke or score us.
2. **no environment isolation.** every commit → production main deploy.
   concurrent runs would race one tree; per-task evidence is overwritten by
   each next commit. STACKED_PRS_PLAN.md solves both and is written but
   unbuilt.
3. **executor mismatch.** swebench-style sandboxes need a shell we don't
   have by design. our executors are the vercel build (wasm compile +
   native cargo test). honest benchmark shape: self-edit tasks scored by
   green deploy + tests — buildable from existing parts, not yet built.

pre-v1 blockers regardless of benchmarking: auto-reconcile at boot (still
top structural item), plus owed live verifications (tab-discard resume,
mid-run thread-switch replay, D10 refusal recovery, stop-button flip).

build order written into TASKBOARD "open work": auto-reconcile → batch/task
queue mode + export → internal eval harness (~20 pinned self-edit tasks) →
branch isolation before any concurrency/external exposure.

---

## landed most recently (bell relocation to bottom-right, 3ec83b1)

user: the bell was never supposed to be in the text input container — it
was supposed to be at the bottom right, with the harness panel. correct:
notify::ensure_dom mounted into .dock-status (the run-status row directly
above the prompt box), so it read as part of the input.

- [x] ensure_dom now mounts on <body>; the bell is position:fixed at
      right:20px / bottom:20px (34×34, z-index 55), floating beside the
      harness rail's edge regardless of dock layout. the panel anchors
      directly above it (bottom:64px = bell top 54 + 10px gap). the old
      `.dock-status { display:flex }` rule that existed only to host the
      bell was removed with it.
- lesson: a self-mounting dom must still choose its mount point like it is
  user-facing design, not plumbing. "mounts itself" fixed drift; it did not
  fix placement. placement feedback belongs in the same iteration as the
  feature.

## landed earlier (background-tab deaths: universal resume, 5d9ee87 + b4de758)

user report: "tasks die when i switch tabs or open other programs — isn't
the point of workers that they persist without being throttled?"

**the premise was half right, and the research matters:**
- web worker timers are NOT throttled by tab visibility. verified against
  WHATWG HTML §8.7 "run steps after a timeout": a Window's timer waits for
  the document to be "fully active" (the visibility gate); a
  WorkerGlobalScope's timer only waits "milliseconds with the worker not
  suspended". chrome's IntensiveWakeUpThrottling docs are page-scoped too.
  our loop's sleep_ms / stop-poll / deploy-poll all keep full speed hidden.
- what actually kills runs: the browser discarding/freezing a HIDDEN TAB —
  memory saver on desktop after ~2h (opt-out in settings), mobile OSes in
  minutes. this kills the ENTIRE renderer including the worker, fires no
  event, and nothing in-page can observe or prevent it. the fix is not
  scheduling; it is surviving the discard.

what landed:
- [x] EVERY run now writes the LoopResume marker at start (was loop-mode-
      only) with a new `loop_mode` field (serde default true so pre-existing
      markers parse as loop runs), and EVERY run clears it on any ending.
      leaving the old `is_loop` gate would have stranded markers behind
      finished plain runs → involuntary resurrection on next boot. caught
      by re-review before commit.
- [x] boot adopts the marked conversation even when it is NOT index.active
      (previously dropped as "stale" — losing runs discarded while the user
      was on another thread). deleted threads are refused via the new pure
      `control::resume_target`, pinned by 4 tests. a resume arriving behind
      an already-running run is dropped rather than parked forever.
- [x] ui pings RunState on `visibilitychange` (feed::wire_visibility_reconcile)
      so returning to a frozen tab corrects stale dock buttons immediately
      instead of within the 3s watchdog tick.
- [x] tests/platform_logic.rs pins the serde default itself.

two red builds paid for it, both self-inflicted, both named by build logs:
1. missed the LoopResume literal in tests/platform_logic.rs when adding a
   field — grep struct literals across TESTS TOO, not just src/.
2. wrote camelCase keys (`loopResume`, `interruptedAt`) in a json fixture;
   these types have NO rename_all so the wire shape is snake_case, and
   #[serde(default)] silently parsed my typo as None instead of erroring.
   #[serde(default)] turns key typos into logic failures — fixtures must be
   checked against the real wire casing.

live verification still owed: start a run, background the tab long enough
for a discard, return — expect "↻ run was interrupted … resuming" and the
run continuing from its last checkpoint.

## landed earlier (the event-loop deadlock, fixed a second time)

the app was bricking itself: a run would finish its work, print its
task_complete summary, and then the dock stayed on "running" forever. stop
did nothing, no error appeared, and no further message could be sent. the
only recovery was a page reload.

root cause — `src/worker.rs`, end of `spawn_run`:

```rust
while queue.borrow().0.is_some() || queue.borrow().1 {
    JsFuture::from(Promise::new(&mut |resolve, _| { resolve.call0(...); })).await;
}
```

awaiting an already-resolved promise drains the MICROTASK queue only. doing
it in a loop re-queues one every iteration, so the event loop never gets to
run a task. that means: (a) the opfs write being waited on can never reach
its completion callback, so the condition never clears and it spins at 100%
of a core forever, and (b) **`onmessage` never fires again**. Stop, RunState
and every later Run were posted into a worker that could no longer hear
them. hence "the stop button doesn't work" and "no feedback" — both were the
same single bug.

this had already been fixed once, in 699ada0, with a timer-based bounded
wait. **016f3db reverted it** — a refactor that rewrote the whole block from
an older copy, under a commit message claiming "no visible behavior change".
the same commit also deleted the Stop escape hatch and `run_seq`.

what landed now:

- [x] `src/worker.rs`: drain yields through `sleep_ms(10)` and is bounded by
      `DRAIN_TIMEOUT_MS`. it now runs AFTER `RunFinished` is emitted, so
      durability work can never gate the dock at all.
- [x] `src/worker.rs`: `Command::Stop` escape hatch restored — after
      `STOP_GRACE_MS` it takes control back by force. `run_seq` restored so
      the abandoned run is invalidated rather than left running invisibly:
      the run's stop predicate is now `run_seq != seq || stop_requested`, so
      being superseded IS a stop signal. write-back and `running` are guarded
      by the same seq.
- [x] `src/agent/mod.rs`: stop is honored during retry backoff (sliced at
      `STOP_POLL_MS` instead of sleeping through up to 60s) and between tool
      calls in a multi-tool step. abandoned calls get synthetic "not run"
      tool results first — bailing mid-batch would otherwise leave an
      assistant message whose tool_calls have no matching results, and the
      NEXT run replaying that history would be rejected by the api.
- [x] `src/ui/feed.rs`: the watchdog now counts unanswered health checks and
      says "the agent worker has stopped responding" after ~9s. the watchdog
      could only ever reconcile a worker that ANSWERS; a starved one looks
      exactly like a healthy long run. silence is now reported.
- [x] `tests/event_loop_liveness.rs`: 5 source-level invariants pinning all
      of the above. verified they fail against the deployed build (4/5) and
      pass against the fix.
- [x] `src/agent/http.rs`: `EventStream::next` now polls the stop flag every
      `STOP_POLL_MS` WHILE waiting for a frame, instead of only after one
      arrives. verified live: before this, stopping a run whose model was
      thinking took the full `STOP_GRACE_MS` and reported "the run did not
      stop on its own" — technically working, but it reads like a failure
      and made the escape hatch the normal path instead of the last resort.
      the read promise is created once and re-raced against a fresh short
      timer each pass; re-issuing read() while one is pending is an error on
      a default reader.

## the lesson (this one cost two ships)

**a comment cannot defend an invariant against a refactor.** the old code
carried a 15-line comment explaining precisely why the timer was required.
the rewrite dropped the comment and the fix together, and nothing failed —
the build was green, the tests passed, the bug shipped. an invariant that
matters needs a test that fails, even a blunt grep-level one.

corollary: **"no visible behavior change" is a claim, not a fact.** when a
refactor rewrites a block rather than editing it, diff the OLD file against
the new one and account for every line that disappeared.

## landed in an earlier run (web access + self-improvement directive)

- [x] `src/agent/tools.rs`: three new tools wired to the existing fetch
      client in `src/agent/http.rs`:
      - `http_fetch` — arbitrary method/headers/body, 20k-char truncation
        (char-boundary safe), returns status + body.
      - `web_read` — any https page as text via `https://r.jina.ai/{url}`
        reader proxy, which sends permissive cors headers.
      - `web_search` — duckduckgo instant-answer api (`api.duckduckgo.com`),
        abstract/answer/definition/related topics.
- [x] `src/agent/mod.rs` system prompt: web tools documented; new
      "self-maintenance" section making capability-gaps into work items; new
      rule 7 requiring memory/ updates before task_complete.
- [x] `memory/TASKBOARD.md`: lesson recorded — describe your limits from the
      substrate, not the tool list.

## what was learned this run

- the agent claimed "no network access" while its own runtime made network
  requests every turn. the tool list is an interface, not a description of
  capability. read the source before stating a limit.
- the user's framing ("change yourself so this happens automatically") is
  the right shape for self-editing agents: fixes that live only in a reply
  evaporate; fixes that live in the prompt + memory files recur every run.

## landed this run (multiagent phase 1 groundwork + red-build recovery)

- [x] **thread-tagged events** (e56fc72): every run-scoped Event variant
      (StepStarted, Reasoning, Content, ToolStarted, ToolFinished,
      RunFinished, Error, Note) now carries a serde-default `thread` tag;
      `Event::thread()` exposes it. the worker stamps the active conversation
      id onto everything `agent::run` emits (tagging closure `emit_tagged`
      wraps the emit passed to run) and on direct emissions via `conv()`.
      the ui routes events whose tag differs from `Ui::active_thread` to a
      compact badge on that conversation's sidebar row instead of the feed.
      all dormant until phase 2 — one worker means tags always match.
- [x] **vercel inputs** in web/index.html under a collapsed <details>
      (.adv-creds styled) — closes the "Config supports fields the panel
      never rendered" item.
- [x] **pending loop-resume note**: boot emits an explicit
      "⏸ loop resume pending" Note so an interrupted loop parked on missing
      credentials is visible instead of silent.
- [x] **main un-broken again** (5aa1e7e + e56fc72). two causes:
      1. my own commit: adding `thread` to protocol variants missed literals
         in agent/mod.rs and worker.rs, plus patterns in feed.rs. the build
         log named every site; mechanical fix.
      2. pre-existing upstream breakage inherited from main:
         update.rs referenced `super::notify::*` but ui/mod.rs never declared
         `mod notify`. notify.rs existed; only the declaration was missing.
         main had been red since that file was added, pinning deploys.
- [x] moved-value lesson: a local closure named `emit` shadowed the
      module-level fn emit and was consumed by agent::run — renamed
      emit_tagged. never name a closure after an existing fn it calls.

## landed this run (mid-run unlock)

- [x] **the app no longer locks during a run** (51815df): switching
      conversations and creating new ones now works WHILE a run continues
      in the background; the sidebar badge (phase-1 routing) shows its
      progress. the lock existed to hide a real corruption bug:
      spawn_run wrote its finished history back into STATE unconditionally
      (and saved STATE.history to STATE.conversation), so a mid-run switch
      would pour thread A's messages into thread B's transcript. fixed at
      the root: guarded write-back (only restore when still on the run's
      conversation), final save addressed to conversation_id with a
      pre-write-back snapshot, park-switched-away history straight to its
      own file. DeleteConversation still refuses only when the target IS
      the running thread.
- lesson: **a lock that forbids safe operations is usually hiding a bug
  worth fixing instead.** find what the lock protects against; fix that;
  delete most of the lock.

## landed this run (stuck-stop-button fix)

- [x] **root cause of the recurring "button stuck on stop" bug found and
      closed** (695abf0 + ee89601): RunFinished — the ONLY event that
      flips the dock back to run — was emitted AFTER draining the opfs
      write queue and saving the transcript, so a slow/wedged save held
      the control transition hostage indefinitely. fixed three ways:
      (1) RunFinished now emits BEFORE the final save; (2) a heartbeat —
      Command::RunState pinged every 3s while the ui thinks a run is
      active, worker answers RunStateReport from its own running flag,
      disagreement snaps the buttons back with an explanatory note;
      (3) Event::touches_run_state() makes start/finish bypass
      background-thread routing so one thread's finish can never strand
      the shared dock. tests pin all three.
- lesson: **control transitions must not queue behind durability work.**
  emit user-visible state first, then persist; persistence failures get
  their own report path. also: any event whose loss strands ui state
  needs a reconciliation channel, not just careful ordering.

## what was learned this run

- **stale-tree incident #2, worse**: this time read_file served a version of
  src/agent/mod.rs MISSING code the deployed version had (a whole loop-mode
  Note emission), and git_commit shipped only 6 of 7 modified files. the raw
  github fetch showed the true blob. rule upgraded from "when versions
  disagree" to: before editing a file you have not touched this session,
  cross-check against the raw blob if anything about it surprises you.
- the build log is the fastest possible diagnosis for enum-shape changes:
  it lists every literal/pattern site at once. when changing a protocol
  variant, grep the whole repo first anyway (`Event::<Variant> {`) — the
  compiler should never be the tool that finds your call sites.

## landed this run (verification layer — loop mode active)

- [x] **native test suite wired into the deploy** (267647f): `cargo test
      --lib --tests` now runs in build.sh after the wasm build; failure
      fails the vercel build and pins production to the last good build.
      verified live: the suite compiles natively on the host target AND
      passes — wasm-bindgen/web-sys compile fine off-target, the pure-logic
      restriction holds.
- [x] three suites: protocol contract (round-trips, old-config loading,
      unknown-field tolerance, thread-tag routing + legacy parse,
      finish-reason strings, message shapes), platform logic (traversal
      guard, char-boundary truncation, index sort purity, LoopResume serde),
      streaming (SSE reassembly: fragmented calls, interleaved indexes,
      torn frames, provider errors, {} arguments, nameless-slot drop).
- [x] llm.rs refactor: absorb_chunk/finalize_turn extracted from run_turn
      and made pub for tests. Turn gained error + deltas fields; run_turn
      replays deltas through the callback exactly once so streaming is
      unchanged. LESSON: nearly shipped without replaying deltas — the
      callback would have been silently dead. when extracting a callback-
      taking function into a pure one, trace where the side effect went.
- [x] system prompt rule 8: state observable behavior + how verified before
      any commit; pure-logic changes need a test. anti-hollow-iteration
      guard for unattended loop mode.

## landed this run (loop resilience — iteration 2)

- [x] **transient llm failures no longer kill the run** (7a6bfb2): retry
      loop in agent::run — 4 retries per step, exponential backoff via
      pure `retry_backoff_ms` (2s→8s→30s→60s cap), resets on success, gives
      up after 5 consecutive failures. every retry emits a visible
      "⟳ llm error (...) — retry N/4 in Xs" Note. before: one rate-limit
      blip ended an unattended loop forever; now it costs seconds.
- [x] **DeployState::from made pub + pinned** by tests/loop_nervous_system:
      the full verdict matrix check_deployment relies on (failure wins;
      running beats success so the loop never moves on mid-build) plus
      settled() semantics. the loop's own build-result reading is no longer
      trusted to review alone.
- [x] compile miss caught by pipeline: leftover Result match after the
      retry-loop refactor returned Turn directly. fixed in one commit.
- note on rule 8 in practice: first draft of the verdict tests mirrored the
  logic instead of exercising it — caught myself and made DeployState::from
  pub so the real code path is what's tested. a mirror test verifies the
  mirror, not the system.

## landed this run (phase-2 groundwork — iteration 3)

- [x] **Command::Attach** (8fc29ca): a worker can adopt one specific
      conversation without touching index.active — the missing primitive
      for spawning a per-conversation worker. SwitchConversation/
      DeleteConversation/Attach now share `adopt_conversation()` so the
      load-into-memory + feed-replay logic cannot drift between them.
      contract test added for the new command.
- design note: Attach deliberately does NOT publish_conversations or write
  index.active. with several workers coexisting, "which thread is on
  screen" belongs to the ui's worker pool, not to any single worker.

## verified live (earlier runs)

- researched compusophy/localharness via GitHub API + its llms.txt; full
  notes in memory/notes/localharness.md (closest cousin project: one
  Rust crate, wasm32, OPFS, self-sovereign agents).

- all three web tools exercised successfully in one run: `web_search`
  (empty result for "latest stable version" style query — expected),
  `http_fetch` (correctly blocked by non-CORS site), `web_read` (full
  markdown of rust-lang.org). pipeline is healthy end to end.

## landed this run (durable conversation history)

- [x] **transcript persistence** — `src/platform/transcript.rs` (new). the
      full `Vec<Message>` is written through to opfs (`vanish-transcript/
      messages.json`) after every run; retention keeps the last 200 messages
      under a 4MB cap. previously the transcript lived only in worker memory
      + the dom, so every ota reload read as total amnesia.
- [x] **boot replay** — `boot_worker` loads the saved conversation into live
      state (the model sees it as context again) and emits the new
      `Event::HistoryRestored`; `feed.rs` renders user/assistant turns behind
      a "↩ restored from the previous session" divider, collapsing tool calls
      into one-line summaries.
- [x] **system-prompt seeding fixed** — the loop now inserts the prompt when
      no system role exists rather than when history is empty, so a restored
      transcript whose prompt aged out of retention still gets instructions.
- [x] **clear conversation** — `Command::ClearHistory` + a button in the
      right rail. memory clears first, disk second, ui wipes on confirmation.
- [x] **ota "later" button** — dismiss records the declined sha in
      localStorage (`vanish.ota.declined`); the poll stops re-nagging until
      a newer commit lands.
- [x] build.rs already stamps VANISH_BUILD from VERCEL_GIT_COMMIT_SHA first —
      an ls-remote stamp in build.sh was tried and reverted as redundant.

## what was learned this run

- "live reconfiguration without refresh" is not possible for wasm: a loaded
  module cannot be replaced in place while the page holds it. the honest fix
  is making the reload non-destructive instead — persistence converts a data
  loss event into a non-event.
- the subtle bug class here: replaying history to the *ui* without loading it
  into the *worker's* state would show context the model cannot see. restore
  must land in both places.
- edit_file's ambiguity refusal also fires after your own previous edit made
  the target stale — re-read before retrying, same as tool rule 4.

## landed this run (settings persistence)

- [x] **settings survive reloads without pressing save** — three fixes in
      4480fc9:
      1. `protocol.rs` `Config` has `#[serde(default)]` — shape drift
         between builds no longer silently discards the whole stored
         config (the likely cause of the reported symptom).
      2. `ui/mod.rs` `load_config` distinguishes fresh/stored/corrupt; a
         corrupt store renders an error card and parks the raw json under
         `vanish.config.corrupt` instead of being overwritten.
      3. the merged config is written back to localStorage at boot, so
         defaults filled in at load persist and reloads read exactly what
         the panel shows.
- [x] `ui/feed.rs`: `append_error` / `append_card` for boot-time failures
      with no run context (D4).

## what was learned this run

- the user's symptom ("fields look remembered but i must hit save") had a
  two-layer explanation: browser password autofill fills the inputs while
  our own load had discarded the stored config. the visible state and the
  loaded state can disagree; hydrate-from-storage must be total or loud.
- edit_file consumed a function signature once because the replacement
  text omitted it — re-read after every multi-line edit, not just after
  failures.

## landed this run (rail collapse button mirroring)

- [x] `web/index.html`: the right rail's `#collapse-right` button moved from
      the end of `.rail-head` to the start, so it sits on the inner edge —
      the same middle-facing side as the left rail's trailing button. the
      two collapse affordances are now true mirror images. css needed no
      change: `.rail-collapse` has no positional selectors and the collapsed
      state centers it via `margin: 0 auto` either way. deployed green
      (778f294).

## landed this run (durability + the great reconciliation)

- [x] **prompt drafts survive reloads** (030256f): every keystroke in the
      prompt box writes through to localStorage (`vanish.prompt.draft`) and
      is restored on boot; cleared when the message is actually sent or the
      box emptied by hand. an ota reload mid-typing no longer destroys work.
- [x] **mid-run transcript checkpoints** (030256f + 761eaaa): agent::run now
      takes a `persist` callback fired after every durable unit — the user's
      prompt, each tool result, each loop-mode nudge. the worker serializes
      these through a drain queue (`Rc<RefCell<(Option<Vec<Message>>, bool)>>`)
      so overlapping opfs writes can never land out of order, and the final
      authoritative save waits for the queue to drain first. a reload mid-run
      now costs at most the step in flight, not the whole run.
- [x] **main un-broken** (761eaaa). what happened: an earlier session added
      the vercel build-log integration to tools.rs / feed.rs / protocol.rs
      but never updated agent/mod.rs, worker.rs, ui/mod.rs. main failed to
      compile from that moment and every subsequent deploy was pinned to the
      last good build. my durability commit inherited that red state, and its
      build log exposed both my real errors and the pre-existing ones:
      - mine: doc comments inside a where clause are unstable rust (use `//`);
        tuple != on a non-PartialEq element; fixed directly.
      - pre-existing: missing `pub mod vercel`, ConfigStatus without
        vercel_ok, Config missing vercel_token/vercel_team_id,
        finish_settings_check never defined, move-borrow of the collapse
        button in wire_rails.
      fix: fetched the vercel-era versions of the three straggler files from
      the parent commit via raw.githubusercontent (the local read_file was
      serving stale content for tools.rs even after sync_repo — trust the raw
      fetch when versions disagree), then re-applied my changes on top.

## what was learned this run

- the local working tree can serve stale file contents even after sync_repo.
  when a build error references code that does not match what read_file
  shows, fetch the raw blob from github before concluding anything.
- committing to main while main is red means inheriting someone else's
  failure. check_deployment on the *parent* state (or just reading the last
  build log) would have shown the vercel breakage before I built on it.
- doc comments are attributes; attributes in where clauses are unstable.
  comments there must be plain `//`.
- the settings-save throttle, vercel fields, etc. exist upstream but have no
  inputs in web/index.html — set_input/input_value no-op on missing ids, so
  this is deliberate-ish and harmless; noted here so a future "vercel token
  field missing" report is answered fast: the fields were simply never added
  to the panel.

## landed this run (loop mode: editor pane)

- [x] **file tree → editor pane wired** (f59e491). the last missing piece of
      the agent-inspecting-its-own-source loop from the ui. clicking a path
      in the workspace tree sends Command::ReadFile; FileContent reveals an
      editor pane between the rails and fills it; save goes through
      Command::WriteFile and TreeChanged confirms ("saved to the working
      tree — uncommitted"). delegation on #tree so the listener survives
      re-renders. next iterations noted on the taskboard (per-file commit
      button, dirty highlight of the open file).

## landed this run (loop mode survives refresh)

- [x] **the loop restarts itself after its own death** (a282862 + ba97c58).
      loop mode's promise was "run until stopped", but a reload killed the
      worker and the restored transcript sat idle. now: a LoopResume marker
      (conversation, prompt, timestamp) lives in the transcript index while a
      loop run is in flight and is cleared the moment it ends. boot finds it,
      parks the prompt until Configure confirms working credentials (at boot
      STATE.config is empty — resuming earlier would bounce off the check),
      then start_run resumes. take_loop_resume clears on read so no failure
      mode can turn every future boot into an involuntary run.
- [x] protocol gained Event::Note for informational worker messages; feed
      renders them as plain notes.
- build miss caught by check_deployment: adding a field to Index broke the
  literal initializer in migrate_legacy. struct literals should use
  ..Default::default() when the struct has serde(default) + Default.

## still open

- [ ] r.jina.ai anonymous rate limits: if `web_read` starts returning 429,
      add an opfs cache keyed by url with a ttl.
- [ ] duckduckgo instant answers only cover abstract-style queries; for
      full results pages, chain `web_search` → `web_read` on a result url.

## what was learned this run (parallelism question)

- the user asked why not webworkers/wgpu-threads/multi-agent for parallelism.
  full verdict in docs/MULTIAGENT_PLAN.md. the load-bearing fact: the agent
  loop is **i/o-bound** (model streams + api calls), so shared-memory wasm
  threads and wgpu are category mismatches — they accelerate compute this
  codebase does not do, at the cost of cross-origin isolation headers and a
  Send-ification rewrite of all thread_local/Rc state. the correct tool is
  *more web workers*, one per conversation, which the per-thread transcript
  design already half-supports.
- "can't start a new chat while one is running" is not an arbitrary limit:
  with ONE shared worker, switching threads mid-run would mutate history out
  from under the loop and the run-end write-back would clobber the switch.
  it dissolves naturally when conversations own their workers (phase 2).
- concurrent agents committing to one branch will race on non-fast-forward
  updates — git strategy must be decided BEFORE shipping concurrency.

## landed this run (stacked prs + preview-gated deploys research)

- [x] docs/STACKED_PRS_PLAN.md — the full design answering "how do we manage
      parallel diffs, and can we preview + e2e before production?". grounded
      in a read of github.rs (commits already name parents explicitly, so a
      stacked chain is just choosing a different base) and vercel.rs
      (deployment_for_commit is branch-agnostic; previews are fetchable
      today). build order: github.rs primitives → tools exposure → claim
      registry → e2e workflow → prompt update.
- key insight: cli stacked-pr tools (graphite, git-spice, stgit) require a
  local clone and cannot exist in the browser. but they automate restacking,
  which over the git data api reduces to rebuilding parents and force-moving
  agent-owned refs — four REST calls we can write ourselves.

## what was learned this run (stacked prs)

- web_search (duckduckgo instant answers) returned empty for both tooling
  queries; web_read on specific urls worked. ddg instant answers cover
  abstract-style lookups, not niche dev-tooling queries — go straight to
  known doc urls when researching engineering topics.
- vercel docs urls move often; the /docs nav itself lists current sections.
- preview deployments may be gated by vercel deployment protection — if the
  url serves an auth interstitial instead of the app, nothing (human or
  playwright) can test against it until protection is relaxed for previews.

## landed this run (scrollbar styling)

- [x] web/style.css: slim dark scrollbars (06e3c9f). the win95 look was
      unstyled native chrome. `* { scrollbar-width: thin; scrollbar-color }`
      for firefox + `::-webkit-scrollbar` block for chromium/safari, thumb
      rgba dim-grey 35% on transparent track, emerald hover accent,
      transparent corner. deployed green.

## rules for the next run

- commit early, commit often. one small atomic commit beats one big
  staged batch that dies uncommitted.
- markdown-only commits are safe (cannot break the deploy) — use them
  to checkpoint progress.
- code commits: review git_diff first. committing is deploying.
- the ui no longer transforms case. what you read in a tool result is what
  is on disk, so no casing workaround is needed when editing.

## landed this run (`now` tool) + stale-tree incident #3

- [x] **the agent can tell time without hitting an api** (e739f00 +
      ba61277): `now` tool reads `js_sys::Date` (wasm) / `SystemTime`
      (native tests). calendar conversion is hinnant's civil-from-days,
      pure, pinned by six tests in tests/platform_logic.rs (epoch zero,
      leap day, pre-epoch flooring, sub-second truncation, year rollover,
      known modern timestamps cross-checked against timeapi.io). system
      prompt now says: you have no internal sense of the date; never guess
      one — call now.
- [x] **incident #3, self-inflicted and repaired** (ba61277): e739f00 was
      built on a stale local tree. three upstream commits (6428c52 memory,
      04bb089 event-loop deadlock fix, 20714f62 stop-during-thinking +
      vercel build-log integration) landed after this worker's snapshot;
      git_commit shipped stale agent/mod.rs and agent/tools.rs on top of
      the true head, silently REVERTING all of that while looking like a
      clean 3-file diff. the red build caught it; both files were restored
      byte-for-byte from parent 20714f62 with the `now` tool re-applied.
      what made it worse: the first fetch of mod.rs from main ALREADY
      contained the upstream changes and I read past them without
      comparing to my local copy. verification that checks one file is not
      verification — every file a commit touches needs the raw-blob
      cross-check before editing.
- [x] **the missing notification bell found and fixed** (ba61277). user
      asked where the bell went. answer: there never was one — notify.rs
      expected #notif-bell/#notif-badge/#notif-panel in web/index.html and
      .notif-* styles in style.css; neither ever landed. every lookup
      silently no-oped (D4 violation), and because the panel replaced the
      old floating ota card, update notices rendered nowhere. fix:
      notify::wire() now calls ensure_dom(), which MOUNTS its own dom into
      .dock-status (panel on <body>); html drift cannot silence it again,
      and a missing host falls back loudly instead of quietly. styles
      added. verified green build; live check: bell visible next to the
      run status, clicking opens the panel, ota notices land inside it.
- lesson: **a feature whose wiring depends on hand-maintained html must
  mount that html itself, or its absence must be loud.** silent optional
  lookups are how a whole notification centre ships invisible.

## landed this run (D10 — the stale-tree guard)

incident #3 had a sequel within the hour: the memory commit d846fcd was
itself built on a stale snapshot and silently deleted D7–D9 from
TASKBOARD.md plus the event-loop postmortem from status.md. fourth
occurrence of the same mechanism. what landed:

- [x] D7–D9 restored verbatim from upstream; the postmortem restored too.
- [x] **D10 written into the directives**: never commit from a tree you
      have not reconciled.
- [x] **git_commit now refuses when the branch head differs from
      synced_head** (the sha this session last synced or committed). the
      refusal message names both shas and instructs re-reading every file
      in the changeset against github before retrying. it also records the
      new head, so one explicit reconcile is enough to proceed — the guard
      is a checkpoint, not a lockout. first sync of a session (empty
      synced_head) passes through unchanged so boot-time commits are not
      blocked.
- [x] **sync_repo is now a real reconciliation**, not a listing refresh:
      every clean cached file whose blob sha diverges from the branch (or
      was never recorded) is dropped from opfs so read_through re-fetches
      current content. dirty files are NEVER dropped — that would destroy
      uncommitted work. synced_head recorded, cache_refreshed reported.
- [x] reconcile_entry extracted as pure + pinned by 5 tests, the first of
      which (dirty files are never refreshed, for any base sha or remote
      answer) is the data-loss invariant.
- lesson: the memory files are not exempt from the stale-tree problem —
  they are its most dangerous victim, because deleting a directive deletes
  the defense against the next incident. treat memory/ edits with the same
  raw-blob cross-check as source edits.

## landed this run (behavioral eval layer)

the user's ask: "make the app better in a verifiable eval way, not just
guessing." the gap: unit tests pinned pure functions, but the loop's
DECISIONS had zero behavioral coverage — exactly where two bricking bugs
shipped.

- [x] `src/agent/control.rs`: the loop's decision layer extracted as pure
      code — FailureBudget (consecutive counting + give-up threshold),
      decide_after_turn (tools/nudge/complete), cancellation_results,
      needs_system_seed, history_is_well_formed (every tool_call answered
      before the next assistant turn).
- [x] agent::run routes all decisions through control::* and EVERY exit
      path goes through one exit! macro asserting transcript well-formedness
      before returning. violations log loudly at the source instead of
      surfacing days later as an api rejection.
- [x] tests/agent_evals.rs: five scenarios WITH negative controls that
      directly assert rejection verdicts (see lesson below). ten evals total.
- [x] **notify::wire was dead code** — boot_ui never called it, so even the
      self-mounting DOM would never have rendered. wired into boot; the
      bell is live for real now. caught via dead-code warnings in the build
      log — read those warnings, they are diagnosis.
- [x] build.sh: per-suite test runs with markers + --nocapture +
      RUST_BACKTRACE=1. parallel/captured runs could kill the harness
      before any failing test printed, naming nothing.
- lessons, paid for in three red builds:
  1. **a #[should_panic] negative control must panic when the checker is
     RIGHT about BAD input.** my first controls asserted on the wrong side
     and punished the checker for rejecting corruption — they would have
     failed on every future checker improvement and passed on regressions.
     assert the Err verdict directly instead.
  2. **verify the counting base before pinning a ladder**: FailureBudget
     counts 1-based, so retry_backoff_ms sees 1..4 → 2s/8s/30s/60s. i
     "fixed" a correct table into a wrong one by assuming 0-based, and the
     line number in the next failure exposed it.
  3. truncated logs are a build-pipeline bug, not an inconvenience: if a
     failure can occur without its name being visible, fix the pipeline
     first, THEN debug.

## landed this run (the 37-file discovery)

asked "what else can we improve"; while investigating the dead-code warning
on reject_while_running, the local worker.rs showed the RESOLVED-PROMISE
drain — the exact bug D7 forbids, unbounded, ahead of RunFinished. size-
matched against upstream to rule out incident #5: identical bytes. the
truth was worse in scale: the ENTIRE local opfs cache (37 files) had been
stale since incident #3, because ba61277 only restored its own changeset.
main was never wrong (untouched files were never committed), and deploy
liveness tests always compiled against upstream's fixed worker.rs — which
is why they passed. but any future run that read worker.rs locally and
edited it would have shipped the pre-deadlock-fix version, reverting the
fix under a green build. the D10 refusal could not catch it either:
synced_head starts empty each session, and the first commit passes by
design.

- [x] ran the rebuilt sync_repo for real: 37 stale caches dropped,
      synced_head recorded. verified local worker.rs is now the fixed
      drain (sleep_ms + DRAIN_TIMEOUT_MS + RunFinished-first).
- lesson: **a fresh session's tree is guilty until reconciled.** the
  stale-tree hazard is not per-incident, it is PER-SESSION — every file
  not written this session is suspect until sync_repo clears them all.
- NEW WORK ITEM (structural): auto-reconcile at boot. the worker should
  run the reconcile pass on Event::Ready (list_tree + head_sha, drop
  diverged clean caches, record synced_head) so D10's guard is armed from
  second zero instead of depending on the agent remembering to sync.
  until then: FIRST ACTION OF EVERY RUN IS sync_repo, before reading or
  editing anything.
