# vanish task board — persistent project management

> read at start of every run; update at end. user feedback lands here once
> and stays honored forever.

## standing directives (permanent — never regress)

- [x] **d1 — default steps = 20** (slider gone from ui entirely now).
- [x] **d2 — steps are a budget, not a target.** keepGoing = verify-before-
      finishing with max 2 nudges, then dynamic finish. always on by default.
- [x] **d3 — ∞ loop mode: run until human stop.** only user-facing autonomy
      control is the "∞ loop" toggle. budget ignored in loop mode.
- [~] **d4 — chat threads / multi-agent.** phase 1 landed: named threads in
      localStorage, switcher select in dock header, "new chat" spawns fresh
      thread preserving old ones.
- [~] **d5 — feedback & error capture.** deaths.md post-mortems land via
      logDeath(); retries on 429/5xx; all failures now render in the ui.

## landed

- agent loop (2cc440d): llm retries w/ backoff, loopMode, dynamic keepGoing,
  death log to memory/deaths.md committed immediately on fatal error.
- server (5d63b92): maxSteps default 20 cap 100, loopMode plumbed,
  keepGoing ignored when loopMode on.
- ui shell (e89155a): step slider removed, single ∞ loop toggle, right-hand
  config panel, thread switcher markup.
- client (ec5c3d0): user prompt bubble above each run ("you: ..."), handler
  crashes render in-feed instead of silent catch{}, scrolltobottom typo fixed,
  step_error/agent_died/agent_stopped/step_retry handlers + styles.

## known failure mode — SOLVED (ae665dd)

- root cause confirmed: vercel hobby tier hard-kills functions at ~60s
  regardless of vercel.json maxDuration=300. high reasoning effort ≈ 12-13
  llm round-trips in 60s → the "always step 13" wall. platform kill is not
  an exception: no catch fires, logDeath never ran, deaths.md never existed,
  staged work evaporated.
- fix shipped: soft deadline 42s injects wrap-up directive (commit staged
  work + summarize), hard deadline 52s stops cleanly so agent_complete and
  context handoff still fire. runs now end gracefully with an honest
  explanation in the ui instead of vanishing.
- remaining options to run longer: upgrade vercel plan (pro = 300s real),
  or lower reasoning effort for more steps per second.

## backlog

- [ ] verify deaths.md gets written on next failure; if empty, logDeath is
      failing silently — investigate executeTool('git_commit') inside a
      dying request.
- [ ] right-hand config panel css (.config-panel) — markup exists, styles
      not yet added (currently relies on sidebar classes).
- [ ] d4 phase 2: true parallel agent runs per thread (multiple sse loops).
- [ ] d5 phase 2: memory/errors.md ledger (expected vs actual per failure).
- [ ] modern thin scrollbars across panels.

## rules

- commit after every file. never hold more than one file staged.
- markdown-only commits are safe checkpoints.
- code commits: review git_diff first. committing is deploying.
