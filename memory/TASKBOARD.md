# vanish task board

> standing directives and open work. read at the start of a run, update at
> the end. user feedback lands here once and stays honored.

## the architecture (as of the rust/wasm rebuild)

vanish is a single rust crate compiled to wasm and loaded twice in the
browser: `boot_ui()` on the main thread (dom only) and `boot_worker()` in a
web worker (the agent loop, the working tree, all networking). they speak
`src/protocol.rs` and nothing else. **there is no server and no serverless
function.** vercel is a static host for `web/`, nothing more.

see ARCHITECTURE.md for the full map and for why the previous design failed.

## standing directives — never regress these

- **D1 — no deadlines in the loop.** the old harness hard-killed runs at 52s
  (`RUN_HARD_DEADLINE_MS`) and blamed the platform. a worker has no request
  and no time limit. never reintroduce clock-watching, wrap-up injection, or
  "time budget" logic. a run ends when the task is done, the step ceiling is
  hit, or the user stops it.
- **D2 — never hold work only in memory.** every file write goes through
  `src/platform/opfs.rs` immediately. the old `staged: new Map()` was
  per-request and lost everything on an early exit. "uncommitted" means bytes
  on disk that differ from the last synced github blob.
- **D3 — no javascript beyond the two boot files.** `web/index.html` and
  `web/worker.js` contain 8 lines of hand-written js between them. everything
  else is rust. `web/pkg/` is wasm-bindgen output and is never edited by hand.
- **D4 — surface every failure.** a silent catch is what shipped a dropdown
  that looked fine and did nothing. errors render; missing dom ids are
  reported loudly, not skipped.
- **D5 — steps are a budget, not a target.** simple tasks end fast, hard ones
  run long. `MAX_STEPS` is a runaway backstop, not a goal.
- **D6 — there is no case policy, and never will be again.** an earlier
  version enforced lowercase in three places at once: a global css
  `text-transform`, a system-prompt rule, and this directive. the css made
  the ui misreport every filename and code sample; the prompt rule made the
  agent lowercase the code it generated, corrupting identifiers wholesale.
  `memory/status.md` even carried a warning telling the agent to work around
  its own ui. write code and prose in the casing correct for the language.
  do not reintroduce this rule in any form.
- **D7 — never await a resolved promise as a yield point.** the worker is
  single-threaded. `await`ing an already-resolved promise drains only the
  microtask queue; doing it in a `while` loop starves the event loop
  outright, and a starved worker stops dispatching `onmessage` — so Stop,
  RunState and every later Run are silently never received, and the app is
  bricked with no error anywhere. yield through `sleep_ms` (a real timer),
  and bound every wait. this exact bug shipped twice: fixed in 699ada0,
  reverted by 016f3db, fixed again with `tests/event_loop_liveness.rs`
  pinning it. that test file is load-bearing — do not delete it to make a
  refactor pass.
- **D8 — control state is never gated on durability work.** `RunFinished` is
  what returns the stop button to "run", so it is emitted BEFORE the
  checkpoint drain and the transcript save, never after. a slow or wedged
  opfs write may cost a stale transcript; it may never cost the user control
  of the app.
- **D9 — every escape hatch must work when the thing it rescues is broken.**
  Stop is the only way out of a wedged run, so it cannot depend on the run
  being healthy enough to poll for it: it takes control back by force after
  `STOP_GRACE_MS`. the same logic applies to any future recovery path — if it
  only works when things are fine, it is not a recovery path.
- **D10 — never commit from a tree you have not reconciled.** the local
  working tree can lag the branch head (three incidents and counting:
  761eaaa inherited red main, e739f00 reverted three upstream commits,
  d846fcd deleted D7–D9 and two postmortems from these very files).
  `git_commit` now refuses when the branch head differs from the sha this
  session last synced/committed; re-read every file the changeset touches
  against the raw github blob, re-apply, and commit again. verifying one
  file is not verification — the changeset is the unit.

## landed in the rebuild

the previous checkpoint (43b2c73, written to the now-removed lowercase
taskboard) asked for four things. all four are resolved:

- [x] **ota hot reload** — `src/ui/update.rs`. it polls the github branch
      head rather than a `/api/version` endpoint, because there is no server
      to host one. shows the changelog between the running build and head,
      then reloads itself; deferred while a run is in flight.
- [x] **right-hand config panel** — `web/index.html` `.rail-right`, styled in
      `web/style.css`. credentials, model, effort, loop toggle, manual commit.
- [x] **sparkle shimmer + changelog styling** — `.sparkle::after` sweep and
      `.ota-changelog`, with a `prefers-reduced-motion` opt-out.
- [x] **infra decision** — resolved by removing the infrastructure. the old
      note concluded "wasm is NOT the fix — the problem is function lifetime,
      not runtime." the first half was exactly right. what it missed is that
      leaving the function is what removes the lifetime: the loop now runs in
      a web worker, which has no request bounding it at all.

## open work

- [ ] **RED BUILD 0582489 — fix in flight, vercel/native cause unidentified**.
      the two-sessions landing commit failed BOTH gates. workflow cause found
      and fixed (--all-targets on wasm32 → E0463; now --lib --bins, pinned by
      ci_gate). the VERCEL failure is a separate native-gate/wasm failure not
      yet diagnosed: the two stranded eval suites (loop_nervous_system,
      ci_gate) had never compiled anywhere before landing. the fix commit
      adds $GITHUB_STEP_SUMMARY mirroring to the gate + workflow so the next
      red build's actual compiler output is readable via the PUBLIC check-run
      api (job logs need admin). NEXT RUN: commit the fix, check_deployment;
      if still red, read the summary from the check run — no more blind
      debugging. also verify the opfs config mirror seeds (one settings save)
      and that boot_worker self-configures (⚙ note in feed).
- [ ] live verification owed: ∞ loop restart after failure/step-limit;
      stop mid-restart keeps it down; browser close+reopen within 12h
      resumes; restart budget saturates at 6/hour with the pause note.

- [x] **UNBLOCK ALL COMMITS — RESOLVED (agent/ci-gate-and-loop-survival)**.
      user added Workflows rw to the token; sync_repo confirmed the tree at
      dd3734e with all ten dirty files intact, work moved to
      agent/ci-gate-and-loop-survival (git_create_branch carries dirty
      files; git_checkout refuses them) and landed in one atomic commit,
      then promoted through a green-checked pr. docs/ci-workflow.yml is a
      retired pointer stub now that .github/workflows/ci.yml is live —
      do not re-copy it; tests/ci_gate.rs enforces the live file.

## landed (overnight-loop survival + ci gate, agent/ci-gate-and-loop-survival)

- [x] **overnight-loop survival**: decide_after_run_end restarts
      loop-mode runs 5s after failed/step_limit/completed endings (never
      after stop, never off-loop-mode, never onto a thread the user
      switched to, and NEVER for batch tasks — the driver owns its queue,
      a successor there races it or ghosts after drain; found in review,
      signature gained an in_batch flag + eval);
      resume_marker_is_fresh expires boot markers at 12h with an explicit
      too-old note instead of surprise runs; RestartBudget caps automatic
      restarts at 6/hour, resets on manual run. evals in
      tests/loop_nervous_system.rs incl. negative controls.
      live verification still owed: toggle ∞, force a failure,
      watch "∞ loop mode continues — restarting in 5s", confirm stop
      mid-restart keeps the loop down; ALSO verify a full browser close +
      reopen within 12h continues the loop (marker → resume → loop_mode
      persists via saved Config → continuation re-arms).

- [ ] **v1 / benchmark readiness** — build order: ~~(1) auto-reconcile~~
      DONE · ~~(2) batch/task-queue + export~~ DONE (2249454) ·
      ~~(3) internal eval suite~~ **DONE (c8c7c6c)** → (4) branch
      isolation via agent/* refs (STACKED_PRS_PLAN §4) BEFORE any
      concurrency or external benchmark — external runs must not be able
      to take down production main. multiworker pool only after 4.

- [ ] **run the first live benchmark**: press "run benchmark" (or console:
      `vanish` ui → Command::RunBenchmark via the worker). expect 5 tasks
      running one-by-one, then a scorecard note ("benchmark: N/5 passed")
      and vanish-bench/report.json in the file tree. NOTE: bench-rust-fn
      edits src/lib.rs — run it on a branch you are willing to commit to,
      or expect the dirty file. grading is mechanical; a fail means the
      agent did not do the edit, not that the checker is broken.

- [ ] **verify batch mode live**: enqueue a 2-task batch (e.g. via console:
      `vanish.enqueue_batch('[{"id":"t1","prompt":"read README.md and say
      done"},{"id":"t2","prompt":"list files"}]')`). expect ▶ task notes,
      results.json appearing in the file tree after each task, ☑ finished
      card. then reload mid-batch: expect "↻ resuming interrupted batch".
      stop mid-batch: expect "cancelled" and remaining tasks dropped.

- [ ] **verify auto-reconcile live** (landed 7b9c43a): reload the app with
      credentials saved. expect the "⇅ tree reconciled against <sha> at
      boot" note in the feed within a few seconds of Configure; a second
      Configure (re-save settings) must NOT re-run it. if upstream moved
      since the last session, expect "N stale cache(s) dropped".

- [ ] **verify universal resume live** (landed 5d9ee87 + b4de758, needs a
      real discard to prove it): start a run, switch tabs long enough for
      the browser to freeze/discard the page (memory saver ~2h desktop,
      minutes on mobile), return. expect a "↻ run was interrupted …
      resuming once settings load" note and the run continuing from its
      last checkpoint in the SAME conversation even if another thread was
      active at boot. also verify: a run that ENDS normally leaves no
      marker (no involuntary resume on next reload), and deleting the
      interrupted thread before returning produces NO surprise run.
      research note for whoever picks this up: worker timers are NOT
      visibility-throttled (WHATWG HTML §8.7: only Window timers wait on
      document visibility; WorkerGlobalScope does not) — the killer is
      tab DISCARD (memory saver / mobile os), which fires no event. any
      future "runs die when hidden" report starts there, not at throttling.

- [ ] **auto-reconcile at boot — RESOLVED (7b9c43a, this run)**: runs on the
      first Configure with a verified github token (not Event::Ready — no
      credentials exist at literal boot; the board's original wording was
      wrong). shared pass with sync_repo; head seeded into every run's
      workspace so the first-commit D10 blind spot is closed. live
      verification still owed (item above).
      discovered via the 37-file cache staleness found this run — see
      status.md "the 37-file discovery".
- [x] **mid-run unlock** (51815df): switching conversations and creating
      new ones works while a run continues in the background (guarded
      write-back + final save addressed to the run's conversation_id).
      deleting the RUNNING thread still refuses, correctly. this was the
      biggest "bricked" complaint for loop mode: an infinite run made
      everything else untouchable.
- [ ] verify after a live run: switch to another thread mid-run, confirm
      the background run's traffic shows as sidebar badges and switching
      back replays its full history.

- [x] **loop resilience** (7a6bfb2): transient llm failures retry with
      exponential backoff (4 retries, reset on success, give up after 5
      consecutive); DeployState::from verdict matrix pinned by tests.
      unattended loop mode no longer dies on a rate-limit blip.

- [ ] multi-agent phase 2: the worker pool — Attach command landed (8fc29ca,
      a worker adopts a specific conversation without touching
      index.active; adoption logic shared via adopt_conversation).
      RE-EVALUATE FIRST: mid-run switching on the single worker now works
      (51815df), which may satisfy most of what the pool was for. decide
      whether true PARALLEL runs (two agents at once) are still wanted
      before building HashMap<conversation, WorkerHandle>; if yes, git
      strategy must land before concurrent agents share one tree.

- [x] **verification layer for autonomous loop mode** — LANDED (267647f),
      EXTENDED into behavioral evals (1d3a6a8): cargo test runs in build.sh
      per-suite with markers; a failing test fails the deploy. suites:
      protocol contract, platform logic (+ reconcile_entry), loop nervous
      system, event-loop liveness, streaming, and agent_evals (failure
      storm, mid-batch stop replayability, restored transcripts, after-turn
      routing, seeding) — the evals carry negative controls that directly
      assert rejection verdicts. system prompt rule 8 requires each
      iteration to state observable behavior + verification. next
      hardening step: evals for ui-state machines (dock transitions,
      thread routing) once their logic is extracted pure; live-fire the
      D10 refusal in a real session; verify bell + OTA notices render.

- [x] web access — `http_fetch`, `web_read` (r.jina.ai reader proxy),
      `web_search` (duckduckgo instant answers) in `src/agent/tools.rs`,
      wired to the existing `src/agent/http.rs` fetch client. no new infra.
      open follow-ups: r.jina.ai may rate-limit anonymous requests; if
      `web_read` starts returning 429, consider caching reads in opfs.
- [x] rail collapse buttons mirrored — both now sit on the middle-facing
      edge of their panel (left rail: trailing in header; right rail:
      leading). 778f294.
- [x] wire the file tree: clicking a path opens it in an editor pane
      (f59e491). delegation on #tree survives re-renders; save writes through
      Command::WriteFile; TreeChanged confirms. next iteration: a "commit"
      button in the editor footer driving Command::Commit for the current
      path set, and dirty-state highlighting of the open file.
## open work

- [x] **stuck stop button** — ROOT-CAUSED AND CLOSED (695abf0/ee89601).
      RunFinished — the only event that flips the dock back to run — was
      queued BEHIND the transcript save, so a slow/wedged opfs write held
      the control transition indefinitely. fixed three ways: emitted before
      the save now; a 3s RunState heartbeat reconciles the dock against the
      worker's own running flag while a run is believed active;
      touches_run_state() guarantees start/finish bypass background-thread
      routing. if it EVER recurs, the feed note "state reconciled after a
      lost finish event" is the diagnostic fingerprint.
- [ ] verify after next deploy: run a short task, confirm the button flips
      back to run on completion within ~3s, with no reconciliation note
      (the note appearing means the ordering fix alone wasn't enough and
      the heartbeat caught it — still fixed, but report it).

- [ ] settings persistence was hardened in 4480fc9 (serde defaults,
      corrupt-store recovery, boot write-back). verify after the next
      deploy that a plain reload no longer asks for save settings; if the
      user still sees the symptom, the next suspect is the deploy pipeline
      serving a stale pkg/ against fresh html.
- [ ] conversation threads — partially done: transcripts are per-conversation
      opfs files and switching works while idle. remaining: a run pins its
      worker (`reject_while_running` refuses new-chat/switch/delete mid-run).
      full fix is multi-worker concurrency — see docs/MULTIAGENT_PLAN.md
      phases 1–2 (thread-tagged events + a worker pool keyed by conversation).
- [ ] multi-agent parallelism — phase 1 GROUNDWORK LANDED (e56fc72): all
      run-scoped events carry a `thread` tag (serde-default, old payloads
      still parse); the worker stamps the active conversation onto every
      emission; feed::render routes mismatched tags to a compact
      `.conv-activity` badge on that conversation's sidebar row;
      `Ui::active_thread` tracks the visible thread from Conversations
      events. REMAINING for phase 1→2: nothing user-visible changes until a
      second worker exists — next concrete task is the worker pool in
      ui/mod.rs (HashMap<conversation, WorkerHandle>, lazy spawn, cap 3–4),
      then git strategy BEFORE enabling concurrent runs on one tree
      (docs/MULTIAGENT_PLAN.md phases 2–3).
- [x] stacked prs design doc exists (docs/STACKED_PRS_PLAN.md)
- [ ] stacked prs / parallel diffs / preview-gated deploys — full design in
      docs/STACKED_PRS_PLAN.md, extends multiagent phase 3. adopted: the
      stacked-pr MODEL over plain REST (our commits already name parents
      explicitly; no cli tools — they need local git). plan: opfs path-claim
      registry (early warning on overlapping edits), agent/* branches + PRs
      via new github.rs primitives (create_ref, compare, create_pr,
      merge_pr), preview deploys gated by a playwright e2e workflow whose
      check runs deployment_state() already reads. main becomes
      promote-on-green, never pushed blind. force-push only ever to agent/*
      refs. concrete build order in §4 of that doc.
- [x] persist conversation history to opfs so a reload does not lose the
      thread — done: `src/platform/transcript.rs`, `Event::HistoryRestored`
      replay on boot, retention cap (200 messages / 4MB), clear-conversation
      button. ota reloads are now non-destructive.
- [x] save the transcript after each step, not just at run end, so even
      "update now anyway" clicked mid-run loses nothing. done in 030256f +
      761eaaa: agent::run takes a persist callback (prompt / each tool
      result / loop nudges) and the worker checkpoints through a serialized
      drain queue. a reload mid-run now costs at most the step in flight.
- [x] prompt drafts survive reloads — localStorage write-through on input,
      restored at boot (`vanish.prompt.draft`), cleared when sent.
- [x] loop mode survives page refreshes (a282862 + ba97c58). a LoopResume
      marker in the transcript index is written when a loop run starts and
      cleared when it ends; on boot a marker for the active conversation
      parks the prompt in PENDING_RESUME until Configure confirms working
      credentials, then start_run resumes. take_loop_resume clears on read,
      so a failed resume cannot loop boots forever. next iteration: surface
      the pending-resume state in the ui (a "resuming…" chip) so an
      interrupted loop is visible even if Configure never arrives.
- [x] web/index.html vercel token/team-id inputs — landed (e56fc72) under a
      collapsed `<details class="adv-creds">` in the credentials section;
      hydrate/collect already read cfg-vercel-token / cfg-vercel-team, so
      build-log reading is now fully user-configurable.
- [ ] `sync_repo` only refreshes the tree listing; it does not yet reconcile
      upstream changes against dirty local files. related incidents (twice
      now): local read_file served stale content — for tools.rs in an earlier
      run, and this run for agent/mod.rs where whole emissions were missing
      from the local view while the deployed blob had them. when a file you
      did not touch this session surprises you, fetch the raw blob from
      github before trusting the local copy. also: git_commit once shipped
      6 of 7 modified files — always re-run git_status after committing and
      verify the file count against what you edited.
      **UPGRADED after incident #3 (ba61277): stale-tree clobbering is no
      longer hypothetical. e739f00 shipped two stale source files that
      silently reverted three upstream commits while looking like a clean
      diff; the red build was the only detector. STRUCTURAL FIX LANDED
      (this run, D10): git_commit refuses when the branch head moved since
      this session's last sync/commit, and sync_repo now drops clean cache
      entries that diverged upstream (never dirty ones) so reads re-fetch.
      the remaining manual step — re-reading each changeset file against
      github before re-applying an edit — is enforced by the refusal
      message. mark resolved once one live run passes through the refusal
      and recovers correctly.**
- [ ] notification centre: the bell now mounts itself (ba61277) and update
      notices land behind it instead of vanishing. verify live after deploy:
      bell visible at the right edge of .dock-status, click opens the
      panel, an ota notice appears inside it rather than nowhere.
- [x] RESOLVED incident: update.rs referenced super::notify::* but ui/mod.rs
      never declared `mod notify` — main was red from that moment and every
      deploy was pinned behind it. fixed in 5aa1e7e. lesson recorded: adding
      a module file without its declaration is invisible locally if nothing
      else references it, but breaks main the moment another file does.
      after landing a new .rs file, confirm it is declared in its parent
      mod.rs in the same commit.
- [ ] wasm64 is a one-line target change once `wasm64-unknown-unknown` leaves
      tier 3 and wasm-bindgen supports it. wasm32 gives a 4gb address space,
      which is far past what this needs.

## history worth not repeating

three separate failures had one cause — a long-lived stateful process inside
a short-lived stateless container:

1. runs died at a self-imposed 52s wall while `vercel.json` said 300s.
2. staged edits lived in per-request memory and vanished with the request.
3. a browser-owned loop was written to fix (1) and (2) and then never wired
   up — `public/app.js` still called the old endpoint. dead code.

the rebuild removes the container, so none of the three has anywhere to live.

## history worth not repeating, part 2

the agent once told the user it had no network access and no way to search
the web. this was false: `src/agent/http.rs` already had a full fetch client
(openrouter, github api, sse streaming) — the capability existed one layer
below the tool list, and the agent described itself from the tool list
instead of reading its own source. lesson: **inventory the substrate before
declaring a limit.** "i don't have a tool for X" and "X is impossible" are
different claims, and only the first is usually true. the standing fix is in
the system prompt: notice a missing capability → treat it as a work item →
edit the source → write the lesson to memory/.
