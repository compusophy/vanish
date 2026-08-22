# vanish task board — persistent project management

> this file is the single source of truth for standing directives and open
> work. the agent MUST read it at the start of every run and MUST update it
> at the end. user feedback lands here ONCE and stays honored forever.

## standing directives (permanent — never regress these)

- [ ] **D1 — default max autonomy steps = 20.** asked THREE times by the
      user; shipped as 8 twice. now set to 20 in: sidebar slider default,
      client state config, server-side fallback. never ship a lower
      default again.
- [ ] **D2 — steps are a BUDGET, not a target.** tasks are dynamic: simple
      tasks finish in few steps, hard ones take many. slider caps at 100.
      keepGoing = verify-before-finishing, not burning the full count.
- [ ] **D3 — loop mode: agents run until HUMAN intervention.** a looping
      agent never self-terminates; only the stop button / abort ends it.
      maxSteps does not apply in loop mode.
- [ ] **D4 — multi-agent parallelism with separate chat threads.** this is
      agent management, not a single input/output pipe: distinct named
      conversations (threads) per agent/workflow, switchable without
      losing history. phase 1 landed; true parallel runs next.
- [ ] **D5 — feedback & error capture.** failures must be recorded with
      enough context to fix exactly what failed (expected vs actual),
      not rediscovered from scratch each run.

## in progress this run

- [x] D1 applied: defaults = 20 everywhere (slider, client, server).
- [x] D2 applied: slider max 20 → 100; keepGoing reworked from "burn full
      budget" to ≤2 verification nudges then dynamic finish.
- [x] D3 applied: `loopMode` added — ignores maxSteps, refuses every early
      finish with a "pick next most valuable action" instruction, ends only
      on human abort. UI: "∞ loop" toggle beside "keep going".
- [x] D4 phase 1: persistent named chat threads (localStorage), thread
      switcher select in dock header, "new chat" spawns a fresh thread and
      preserves old ones.
- [x] D5 phase 1: this board + memory/status.md carry expectations vs
      results forward.

## backlog

- [ ] D5 phase 2: memory/errors.md append-only error ledger (attempted /
      expected / actual / root cause / fix / verification). agent reviews
      tail before similar work.
- [ ] D4 phase 2: true parallel agent runs — multiple simultaneous SSE
      loops from the ui, one per thread, per-thread live feeds.
- [ ] D4 phase 3: agent-to-agent delegation (a tool call that spawns a
      sub-run with its own thread and returns a summary).
- [ ] loop guardrails: wall-clock cap + periodic non-blocking human
      checkpoint notifications (never auto-stop).

## rules (unchanged)

- commit early, commit often — staged work dies with the run.
- markdown-only commits are safe checkpoints.
- code commits: review git_diff first. committing is deploying.
- disk files are camelCase; ui transcript lowercases for display.
