# vanish run status — persistent context for the next run

> the agent has no memory between runs. this file is the memory.
> update it at the end of every run. read it first thing every run.

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

## verified live

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
