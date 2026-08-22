# vanish run status — persistent context for the next run

> the agent has no memory between runs. this file is the memory.
> update it at the end of every run. read it first thing every run.

## landed this run (commit: keep-going + step budget semantics)

- [x] `lib/agent.js`: new `keepGoing` option on `runAgentLoop`. when true and
      the model tries to finish early (0 tool calls) with steps left in the
      budget, the loop refuses the exit, pushes a "keep going" user nudge
      into the conversation, emits a `continue_nudge` event, and continues
      until the full `maxSteps` budget is exhausted (or abort).
- [x] `lib/app.js`: `/api/agent/run` reads `keepGoing` from the body and
      passes it into the loop (`keepGoing === true`).
- [x] `public/index.html`: added `#chk-keep-going` toggle in the dock header,
      positioned immediately left of the "new chat" button (user requirement).
- [x] `public/app.js`: persists toggle state in localStorage
      (`vanish_keep_going`), sends `keepGoing` in the run request, renders
      `continue_nudge` as a glowing chip in the agent feed.
- [x] `public/style.css`: `.keep-going-toggle` switch styles (emerald when on)
      + `.continue-nudge` chip with pulse animation.

## semantics now

- keep going off (default): agent finishes as soon as it stops calling tools
  (previous behavior).
- keep going on: agent is forced to spend the entire step budget; each early
  finish attempt gets a nudge to verify/refine/harden until steps run out.

## lesson learned this run

- a 20-step run died at exactly step 20 trying to git_commit: all staged
  edits were lost and had to be redone from scratch. commit EARLY — after
  the first couple of file edits, not at the end of the run. staged work
  does NOT survive between runs, ever.

## still open (from docs/refactor_plan.md)

- [ ] re-land hardened agent loop: retry with backoff on 429/5xx,
      consecutive-error threshold 3, finish_reason handling
- [ ] shared edit engine in lib/tools.js: occurrence-indexed replace,
      replace_all, fuzzy fallback
- [ ] .github/workflows/ci.yml (node --test on push)
- [ ] test/session.test.js + test/edit-engine.test.js
- [ ] ci_status tool (github checks api)

## rules for the next run

- commit early, commit often. one small atomic commit beats one big
  staged batch that dies uncommitted.
- markdown-only commits are safe (cannot break the deploy) — use them
  to checkpoint progress.
- code commits: review git_diff first. committing is deploying.
- note: file contents are camelCase on disk; the ui transcript lowercases
  everything for display. match casing exactly when editing.
