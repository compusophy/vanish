# vanish run status — persistent context for the next run

> the agent has no memory between runs. this file is the memory.
> update it at the end of every run. read it first thing every run.

## the wall we hit

1. **staged work is lost between runs.** write_file/edit_file stage in
   memory per-run; if the run ends without git_commit, everything is gone.
   this happened twice: the phase-0 batch (edit engine, ci workflow,
   session test, agent hardening) was staged but never committed.
2. **maxsteps too low + no retries.** runs die on transient openrouter
   errors (any step_error aborts the loop) and run out of steps (default 8).
3. **no persistent context.** each run re-discovers the repo from scratch.

## plan (from docs/refactor_plan.md, phase 0 first)

- [ ] re-land the hardened agent loop: maxsteps default 50, retry with
      backoff on 429/5xx, consecutive-error threshold 3, tool-result
      truncation (12k chars), finish_reason handling
- [ ] re-land the shared edit engine in lib/tools.js: occurrence-indexed
      replace, replace_all, whitespace-tolerant fuzzy fallback,
      edited-region snippet in the result
- [ ] add .github/workflows/ci.yml (node --test on push) — the cloud-mode
      verification channel
- [ ] add test/session.test.js + test/edit-engine.test.js
- [ ] bump app.js maxsteps ceiling 25 → 100
- [ ] ci_status tool (github checks api) to close the post-commit loop

## rules for the next run

- commit early, commit often. one small atomic commit beats one big
  staged batch that dies uncommitted.
- markdown-only commits are safe (cannot break the deploy) — use them
  to checkpoint progress.
- code commits: review git_diff first. committing is deploying.
