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

- [x] web access — `http_fetch`, `web_read` (r.jina.ai reader proxy),
      `web_search` (duckduckgo instant answers) in `src/agent/tools.rs`,
      wired to the existing `src/agent/http.rs` fetch client. no new infra.
      open follow-ups: r.jina.ai may rate-limit anonymous requests; if
      `web_read` starts returning 429, consider caching reads in opfs.
- [ ] wire the file tree: clicking a path should open it in an editor pane
      (`Command::ReadFile` and `Event::FileContent` already exist and work;
      only the click handler and the editor element are missing).
- [ ] conversation threads — the old client had multiple persisted threads.
      the rust client currently keeps one history in the worker.
- [x] persist conversation history to opfs so a reload does not lose the
      thread — done: `src/platform/transcript.rs`, `Event::HistoryRestored`
      replay on boot, retention cap (200 messages / 4MB), clear-conversation
      button. ota reloads are now non-destructive.
- [ ] save the transcript after each step, not just at run end, so even
      "update now anyway" clicked mid-run loses nothing. transcript.rs is
      the place; worker.rs Run handler calls save() once today.
- [ ] `sync_repo` only refreshes the tree listing; it does not yet reconcile
      upstream changes against dirty local files.
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
