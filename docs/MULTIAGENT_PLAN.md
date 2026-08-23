# multi-agent and parallelism — the design verdict

> written in response to: "shouldn't we use webworkers or wgpu threads to do
> parallelism? i can't start a new chat while one is running, and multi-agent
> workflows are the modern paradigm."
>
> short answer: yes to more web workers, no to shared-memory wasm threads,
> no to wgpu. the detail below explains why, and stages the work.

## 1. what kind of workload is the agent loop?

Look at what a run actually does, step by step:

- `llm::run_turn` — awaits an SSE stream from openrouter. seconds of idle
  waiting on the network.
- tool dispatch — github api calls, opfs reads/writes, http fetches. again
  network and storage, not arithmetic.
- between awaits, the wasm does trivial work: parse json, push a message,
  format a string. microseconds.

**the loop is i/o-bound, not cpu-bound.** every expensive thing it does is
*waiting*. this single fact decides the whole question.

## 2. why shared-memory wasm threads (rayon-style) are the wrong tool

`wasm-bindgen-rayon` gives real os-style threads over a shared
`SharedArrayBuffer`. it is spectacular for one job: crunching large arrays of
numbers across cores — image processing, physics, parsing gigabytes.

we have no such job. worse, adopting it would mean:

- **cross-origin isolation**: SharedArrayBuffer requires COOP/COEP headers.
  settable in `vercel.json` (`headers`), but it changes the deployment
  posture for a benefit of zero.
- **a rust rewrite of the state model**: the entire codebase is built on
  `thread_local!`, `Rc<RefCell<..>>`, and closures handed to js. none of that
  is `Send`. shared-memory threads would force a rewrite of worker.rs,
  agent/mod.rs and the persistence path for no speedup — the wall clock time
  of a run is dominated by model latency, and ten threads waiting on one api
  key's rate limit wait exactly as long as one thread does.
- **opfs sync handles are already worker-local**, and the working tree is a
  *shared mutable* resource. adding threads that share it increases corruption
  risk in exactly the place durability matters most.

## 3. why wgpu is the wrong tool

wgpu on the web is WebGPU: compute shaders for massively data-parallel
numeric work. an agent orchestrating edits to a repository has no kernel to
write. there is no array to reduce. this is a category match of zero. (if a
future feature ever needed heavy local computation — embedding search over a
huge corpus, say — wgpu becomes worth revisiting. today it is not that.)

## 4. what actually delivers the goal: more web workers

the modern multi-agent paradigm is *task parallelism*: several independent
agent loops, each with its own context, working concurrently. that is
precisely what web workers are for, and this codebase is already 80% shaped
for it:

already in place:
- transcripts are **per-conversation files in opfs**
  (`platform/transcript.rs`) — each agent's context is already isolated on
  disk.
- the protocol already carries `thread_id` on `Command::Run` and
  `Event::RunStarted`.
- the persist/drain-queue machinery is per-run, not global.
- `LoopResume` markers are already per-conversation.

what assumes one worker (the whole refactor surface):
1. `WorkerState` is a singleton thread_local in `src/worker.rs`; one `history`,
   one `conversation`, one `running` flag.
2. `reject_while_running` refuses new-chat / switch / delete while any run is
   alive — **this is the exact restriction the user hit.** it exists because
   switching mutates shared state out from under the loop (and the run-end
   write-back `st.history = history` would clobber whatever the user switched
   to). the restriction is correct *for one shared worker* and dissolves the
   moment conversations own their workers.
3. run-scoped events (StepStarted, Content, ToolFinished…) carry no thread
   tag, and the feed is one global stream — two interleaved runs would render
   as soup.

### phased plan

**phase 1 — thread-tagged events + feed routing.** add a `thread: String`
to every run-scoped event variant (serde default keeps old payloads
parseable). the feed routes events to the active conversation's stream;
background threads collapse to a compact badge ("⟳ 3 steps · editing
worker.rs") instead of dumping into the visible feed. small, safe, shippable
alone.

**phase 2 — the worker pool.** `ui/mod.rs` holds
`HashMap<String /*conversation*/, WorkerHandle>` instead of one worker.
`NewConversation` spawns a fresh worker (lazy, capped — 3–4 concurrent is
sane); `SwitchConversation` just swaps which handle receives commands and
which stream the feed shows. commands already know their thread; the router
is ~50 lines. each worker boots with Configure + loads its own transcript,
exactly as boot_worker does today. result: **start a new chat any time, run
as many agents as you like.**

**phase 3 — git strategy before anyone presses "run both".** two agents
committing to one branch race: the loser gets a non-fast-forward 409 and a
bad afternoon. options, cheapest first:
- per-agent branches (`agent/{conversation-id}`) with a manual merge button;
- a commit lock: only the "focused" thread may commit, enforced worker-side;
- an orchestrator-owned merge step (heaviest, most automatic).
decide before phase 2 ships concurrency, or the first dual-commit teaches us
the hard way.

**phase 4 — the orchestrator (the paradigm you named).** once 1–3 exist, a
run can spawn peers: a new tool (`spawn_agent(prompt, thread)`) that asks the
worker pool for a free handle and starts a child run. parent collects results
via the transcript files, which are already per-thread. this turns vanish
from "one agent with a chat box" into a fleet with a dispatcher — the
swarm/spinning-up shape.

## 5. costs to state out loud

- n concurrent agents = n concurrent openrouter streams: multiplied token
  spend, and one key's rate limit shared across all of them. the pool cap is
  partly a wallet guard.
- the working tree is shared origin-wide. until phase 3 lands, concurrent
  agents must not both edit; the ui should say so on the second run.
- memory: each worker instantiates the wasm module (~tens of mb). the cap
  exists for this too.

## 6. what was decided today

- no wasm-threads, no wgpu. revisit only for genuine local compute workloads.
- the roadmap above goes on the taskboard; phase 1 is the next concrete
  coding task whenever a run picks it up.
