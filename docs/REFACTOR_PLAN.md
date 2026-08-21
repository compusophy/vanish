# vanish refactor plan — benchmark-first self-improvement

> prime directive: make vanish score higher on terminal-bench 3.0, swe-bench
> (verified), and similar agentic coding benchmarks. every change below is
> justified by its effect on agent pass rate, step efficiency, or token
> economy — not by elegance for its own sake.

---

## 0. situation assessment

what the current harness is: a working two-mode (local fs / github api)
tool-execution loop over openrouter streaming, with a web ide and a
self-deploy loop (commit = deploy).

what it is not: a benchmark-competitive agent. the gaps, ranked by expected
benchmark impact:

| # | gap | benchmark impact |
| --- | --- | --- |
| 1 | no search tools (`grep` / `glob`) | catastrophic on swe-bench; repo navigation dominates step + token spend |
| 2 | unbounded context (no truncation / summarization) | long tasks die mid-run; context rot degrades late decisions |
| 3 | no parallel tool execution | 2-3x wall-clock penalty; blocks batched exploration patterns |
| 4 | fragile loop (any error aborts the run, no retries) | benchmark runs lost to transient api failures |
| 5 | exact-match-only `edit_file` | each failed edit burns a step; models fumble exact whitespace |
| 6 | no verification loop (no tests, no ci visibility) | agent commits blind; self-improvement is unverifiable |
| 7 | no planning / todo / memory / session persistence | multi-step tasks lose the plot; zero learning across runs |
| 8 | no eval harness | cannot measure any improvement — flying blind |
| 9 | static system prompt, no repo map | wasted steps rediscovering repo structure every run |
| 10 | default `maxSteps = 8` | far too low for real swe tasks (typical: 30-100) |

constraints we accept (do not fight them):
- cloud mode has no shell and a read-only fs. github is the database.
- committing to `main` deploys the harness itself — every commit must be
  safe. this is why phase 0 (tests + ci) comes before everything.
- no external services beyond github, vercel, openrouter.

---

## phase 0 — safety net (do first, blocks everything else)

**goal:** the agent must be able to prove its own edits are not broken before
committing, despite having no shell in cloud mode.

1. **test suite** using `node:test` (zero new deps):
   - `test/tools.test.js` — path normalization, workspace sandbox escape
     rejection, edit_file occurrence rules, staging/baseline behavior,
     unified diff correctness.
   - `test/agent.test.js` — loop mechanics against a mocked openrouter
     (inject a `fetch` impl): tool-call accumulation across stream chunks,
     parallel execution ordering, retry-on-5xx, max-steps continuation.
   - `test/session.test.js` — seal/unseal round-trip, expiry, tamper
     rejection.
2. **github actions ci** (`.github/workflows/ci.yml`): `node --test` on every
   push + pr. this is the cloud-mode verification channel.
3. **new tool: `ci_status`** — queries the github checks api for the latest
   commit on the branch. the agent's post-commit verify loop becomes:
   `git_commit` → `ci_status` → fix if red. closes the biggest safety hole
   in self-editing.

**exit criteria:** `npm test` green locally; ci green on main; agent can see
ci results through a tool.

---

## phase 1 — agent core refactor

**goal:** a resilient, context-aware, parallel execution loop.

restructure `lib/agent.js` (currently a 326-line monolith) into:

```
lib/
├── agent.js            # orchestrator only: step loop, event emission
├── llm/
│   ├── client.js       # openrouter streaming, usage accounting, retries
│   └── stream.js       # sse parser (extracted + unit-testable)
├── context.js          # context window manager
└── prompts.js          # system prompt builders (dynamic, mode-aware)
```

### 1.1 llm client (`llm/client.js`)
- extracted sse parser with tests (the current inline parser silently eats
  malformed chunks).
- **usage accounting:** capture `usage` / token counts from the stream,
  emit `usage` events, enforce a token budget per run.
- **retries with exponential backoff** on 429/5xx/network errors
  (3 attempts). a benchmark run should never die on a transient blip.
- **model routing:** config-driven model selection per run
  (cheap model for exploration, strong model for edits) + fallback model
  on persistent failure.
- finish-reason handling: surface `length` (context overflow) so the loop
  can trigger context compaction instead of dying.

### 1.2 parallel tool execution
- execute independent tool calls with `Promise.all` (they already arrive
  as a batch); results returned **in call order** to keep message pairing
  correct.
- serialize only tools with side effects on shared state (`git_commit`,
  `run_command` in local mode).
- emit `tool_exec_start/result` events as they complete (streaming ui
  stays live).

### 1.3 context manager (`context.js`)
- **tool output truncation:** cap tool results (e.g. 8k chars) with a
  `truncated: true` marker + hint to re-read with a line range. read_file
  already paginates; apply the same to grep, diff, and run_command output.
- **history compaction:** when approaching a token budget, summarize older
  tool results (keep the last n verbatim), preserving all assistant
  reasoning and the original task statement.
- **file cache:** remember files already read this run; a re-read of
  unchanged content is served from a stub ("unchanged since step k") to
  stop the classic loop of re-reading the same file.

### 1.4 loop hardening
- `maxSteps` default 8 → **50** (configurable per run).
- on `step_error`: retry the step (via llm client backoff); only abort after
  consecutive-failure threshold (3).
- **max-steps continuation:** when hitting the cap with pending work, emit a
  structured summary event instead of silently ending.
- on `finish_reason: length`: auto-compact and continue.

**exit criteria:** mocked-stream tests prove parallel exec, retry, and
compaction; a 100-step run stays within token budget.

---

## phase 2 — benchmark-grade tool surface

**goal:** the tool set that swe-bench/terminal-bench trajectories actually
need. this is the highest-leverage phase.

### 2.1 search tools (new module `lib/tools/search.js`)
- **`grep`**: pattern + glob/path filter + `output_mode` (`content` with
  line numbers / `files_with_matches` / `count`), head_limit. local: spawn
  ripgrep if present, else js fallback. cloud: scan the git tree (already
  fetched for `list_dir`) + staged files in memory — pure js, no shell
  needed.
- **`glob`**: pattern → file list, sorted by modification time (local) or
  tree order (cloud).
- rationale: on swe-bench, models with grep solve tasks in roughly half the
  steps of models navigating by directory listing alone.

### 2.2 edit_file upgrades
- **occurrence-indexed replace:** optional `occurrence` param (1-based) and
  `replace_all: true`, so the model isn't blocked when a target legitimately
  appears twice.
- **fuzzy fallback:** if exact match fails, retry with whitespace-normalized
  matching (collapse indent differences) and report which fuzzy match was
  applied — one saved retry per failed edit.
- **multi-edit batching:** accept an array of edits applied atomically to
  one file; single staged write.
- **result enrichment:** return a snippet of the edited region so the model
  can self-verify without an extra read.

### 2.3 execution tools
- `run_command` (local): add `cwd`, background execution with
  `get_output` polling (long test suites), output truncation with tail
  preservation (errors live at the end), and env passthrough.
- **new `run_tests` convenience tool (local):** detects the project's test
  runner from `package.json` and runs it with sane defaults; returns
  structured pass/fail/failure-text.

### 2.4 completion protocol
- **`task_complete` tool** with a structured `summary` + `files_changed` +
  `verification` fields. benchmark harnesses and the web ui both benefit
  from an explicit, machine-readable done signal instead of "model stopped
  calling tools."

**exit criteria:** grep/glob tested in both modes; edit fuzzy path tested;
task_complete wired into the loop and ui.

---

## phase 3 — planning, memory, and session history

**goal:** the agent keeps the plot across steps and learns across runs.
no database: github is the persistence layer.

### 3.1 planning tools (in-run)
- **`todo_write` / `todo_read`:** structured task list (like the todo
  pattern that measurably helps on long-horizon benchmarks). stored in the
  tool context, emitted as events so the ui renders a live plan panel.

### 3.2 persistent memory (cross-run)
- a **memory directory in the repo itself**: `memory/`
  - `memory/lessons.md` — distilled failure/success lessons the agent
    appends to after each run (self-improvement in its simplest form).
  - `memory/repo-map.md` — auto-refreshed structural summary of the
    codebase, injected into the system prompt so the agent stops paying the
    exploration tax every run.
- new tool `memory_write` (append-only, size-capped) + prompt guidance for
  when to use it.

### 3.3 session history (the missing chat log)
- **trajectory logging:** every run is serialized as jsonl
  (events + messages + usage + final git status) and committed to
  `memory/trajectories/YYYY-MM-DD-<slug>.jsonl` at run end (opt-in flag,
  off by default to avoid commit noise; a `vanish.json` config flips it on).
- **session continuity:** the web ui keeps per-session history in
  localStorage today-equivalent (browser side); server side, the sealed
  cookie gains a session id so a returning browser can resume its last
  conversation from the trajectory log.
- rationale for trajectories: phase 5 consumes them. no trajectories, no
  learning loop.

### 3.4 dynamic system prompt
- inject at run start: repo map summary, current branch, recent commit
  subjects, open ci status, and the top of `memory/lessons.md`.
- keep it under ~1k tokens; cache-friendly ordering (static first).

**exit criteria:** todo panel live in ui; trajectories landing in
`memory/trajectories/`; system prompt carries repo context.

---

## phase 4 — eval harness (measure or it didn't happen)

**goal:** run the benchmarks locally, score automatically, and produce a
regression dashboard the agent itself can read.

- `eval/` directory, local-mode only (needs shell + containers):
  - `eval/runner.js` — headless agent invocation (reuses
    `runAgentLoop`, no http), captures full trajectory jsonl.
  - **swe-bench harness:** task checkout → run agent → `git diff` vs base →
    apply to eval container → run fail-to-pass/pass-to-pass tests → score.
  - **mini-bench:** a curated set of ~20 in-repo tasks (real bugs in vanish
    itself + synthetic terminal-bench-style tasks) for fast signal between
    full swe-bench runs.
  - `eval/report.js` — markdown scorecard committed to `memory/evals/`:
    pass rate, avg steps, avg tokens, per-task diff vs previous run.
- the agent's own improvement loop reads `memory/evals/latest.md` to see
  whether its last self-edit helped.

**exit criteria:** one command produces a scored run on mini-bench; swe-bench
subset runs end-to-end.

---

## phase 5 — the self-improvement loop

**goal:** close the loop: run evals → analyze failures → edit self → re-run.

1. **failure analysis pass:** after an eval run, a dedicated agent pass
   reads failed trajectories and writes structured diagnoses to
   `memory/failures.md` (e.g. "missed the test file because grep wasn't
   used; over-read large files").
2. **meta-prompt optimization:** lessons feed the system prompt builder;
   prompt variants are A/B'd on mini-bench before landing on main.
3. **guardrails for self-edits:** the agent never commits to `main` directly
   during self-improvement — it opens a pr, waits for ci (phase 0 tool),
   and merges only green. the deploy loop stays intact; risk is bounded.
4. **scheduled runs:** github action (cron) runs mini-bench nightly and
   commits the scorecard, giving the agent a standing self-assessment
   signal even when no human is watching.

---

## sequencing and effort

| phase | depends on | rough size | benchmark lift |
| --- | --- | --- | --- |
| 0 safety net | — | s | enables all the rest safely |
| 1 agent core | 0 | m | reliability, long tasks |
| 2 tool surface | 0 (1 helps) | **l** | **largest single lift** |
| 3 planning/memory | 1 | m | long-horizon tasks, learning |
| 4 eval harness | 0-2 | l | makes lift measurable |
| 5 self-improvement | all | m | compounding |

recommended order: 0 → 2.1/2.2 (search + edits, fastest lift) → 1 → 4
(mini-bench only) → 3 → 4 (full) → 5.

## non-goals (explicit)

- no vector db / rag — grep + repo map covers benchmark-scale repos.
- no multi-agent swarms — a single strong loop with parallel tools beats
  fragile orchestration at this stage.
- no new runtime deps in `lib/` — keep the vercel function lean; dev deps
  for eval are fine.

## immediate next actions

1. phase 0: test suite + ci workflow.
2. `lib/tools/search.js`: grep + glob (both modes).
3. `edit_file`: occurrence index + fuzzy fallback.
4. bump `maxSteps` default, add retries + parallel tool exec.
5. `ci_status` tool.
6. mini-bench skeleton under `eval/`.
