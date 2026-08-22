# vanish run status — persistent context for the next run

> the agent has no memory between runs. this file is the memory.
> update it at the end of every run. read it first thing every run.

## landed this run (web access + self-improvement directive)

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

## verified live

- researched compusophy/localharness via GitHub API + its llms.txt; full
  notes in memory/notes/localharness.md (closest cousin project: one
  Rust crate, wasm32, OPFS, self-sovereign agents).

- all three web tools exercised successfully in one run: `web_search`
  (empty result for "latest stable version" style query — expected),
  `http_fetch` (correctly blocked by non-CORS site), `web_read` (full
  markdown of rust-lang.org). pipeline is healthy end to end.

## still open

- [ ] r.jina.ai anonymous rate limits: if `web_read` starts returning 429,
      add an opfs cache keyed by url with a ttl.
- [ ] duckduckgo instant answers only cover abstract-style queries; for
      full results pages, chain `web_search` → `web_read` on a result url.

## rules for the next run

- commit early, commit often. one small atomic commit beats one big
  staged batch that dies uncommitted.
- markdown-only commits are safe (cannot break the deploy) — use them
  to checkpoint progress.
- code commits: review git_diff first. committing is deploying.
- the ui no longer transforms case. what you read in a tool result is what
  is on disk, so no casing workaround is needed when editing.
