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

- [x] **verification layer for autonomous loop mode** — LANDED (267647f).
      cargo test runs in build.sh; a failing test fails the deploy. three
      suites cover protocol round-trips, the traversal guard, transcript
      index logic, and SSE tool-call reassembly. system prompt rule 8
      requires each iteration to state observable behavior + verification.
      the evidence standard for unattended loop mode is now "compiles AND
      passes tests", not just "compiles". next hardening step if needed:
      extend coverage as new pure logic lands (rule 8 enforces this).

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
      diff; the red build was the only detector. new standing rule: before
      ANY commit touching a file you have not written this session, fetch
      its raw blob from github and diff mentally against your local copy —
      for EVERY file in the changeset, not just the one you edited first.
      better fix if it keeps recurring: make git_commit refuse when the
      remote head advanced past the sha this session's sync_repo recorded,
      forcing an explicit re-read first.**
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
