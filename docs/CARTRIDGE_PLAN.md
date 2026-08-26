# cartridges: the composable computation substrate for vanish

> written 2026-08-27 at the owner's direction ("wasm cartridge modules with
> universal computing virtualized for hot swapping modules — cognitive
> modules mostly, but all other kinds of"). this is the design doc that turns
> the tempo-x402 cartridge precedent and localharness's rustlite compiler
> into vanish's next gradient step. read STACKED_PRS_PLAN.md first if you
> want the git/PR substrate this rides on.

## 1. what the owner asked for, in one paragraph

not a monolithic agent that rebuilds itself end to end on every change — a
**composable actor model of computation**: small wasm "cartridges" (cognitive
ones mostly, but any kind) that hot-swap into a running system without
rebuilding the whole thing. apps composed of other apps, unlimited up/down/
lateral hierarchy. an LLM as execution environment (tokenized instruction
sets, opcodes) rather than a code printer. proven pieces exist already:
tempo-x402 shipped the ABI + engine + cognitive orchestrator; localharness
shipped rustlite, a Rust-subset → wasm compiler that runs where cargo can't.
this document is the synthesis: what to build, in what order, and why it is
new rather than a copy of either.

## 2. prior art we own (facts, verified against the sources)

### tempo-x402 (compusophy/tempo-x402, crates/tempo-x402-cartridge)

- **manifest**: `CartridgeKind` enum = `backend | interactive | frontend |
  cognitive`. cognitive = "hot-swappable brain module", routed via a
  CognitiveOrchestrator. ABI_VERSION = 2; v2 added x402_call so a cartridge
  can call another cartridge by slug — composition was worth a version bump,
  which tells you where the value is.
- **host ABI** (raw `extern "C"`, zero dependencies — deliberate): the guest
  imports `response`, `log`, `kv_get`, `kv_set`, `payment_info`, and `call`
  from a named wasm import module (`#[link(wasm_import_module = "x402")]`).
  guests are `#![no_std]` with a hand-written panic handler and a static
  scratch buffer for host→guest transfers. no allocator needed.
- **compiler**: server-side today (`tokio::process::Command::new("cargo")`),
  with a shared target dir caching deps across cartridges (~2–5 min cold,
  seconds warm). the gap: it cannot run in a browser.
- **engine**: wasmtime sandbox, fuel/time bounded per request.

### localharness (memory/notes/localharness.md)

- three embedded DSLs, all pure Rust: `rustlite` (Rust subset → wasm),
  `bashlite` (fuel-bounded shell), `soliditylite` (subset → EVM bytecode).
- same self-sovereign shape as vanish: one crate, wasm32 in browser, OPFS,
  no backend. closest cousin; the wallet/x402 layers are theirs, the
  compile-in-the-browser DSL idea is the part that matters here.

### the wider landscape (researched 2026-08-27)

- **full rustc-in-browser**: bjorn3 got rustc *itself* compiled to wasm32-
  wasip1 running under wasmer (2019, miri#722); oligamiq's rubrc (2024)
  compiles real Rust in-browser but needs a WASI sysroot and still fights
  linkers. possible, but heavy: hundreds of MB, minutes of compile, and the
  linker story (lld / wasm-component-ld) remains the hard edge.
- **wasm-micro-runtime (WAMR)**: an interpreter/sandbox you CAN compile to
  wasm32-unknown-unknown — this is how the browser hosts guest wasm today
  without wabt's JIT (JIT is unavailable in wasm). interpreter speed only.
- **wasm-interp (wabt)**: pure C++, has been compiled to wasm for online
  demos. proof that a wasm-hosted wasm interpreter ships.
- **the honest takeaway**: do not port rustc. define the language small
  enough that the COMPILER is trivially portable, and target wasm bytes as
  its only output. that is exactly rustlite's bet, and it is the right one.

## 3. the architecture: five layers

```
┌─────────────────────────────────────────────────────────────┐
│ L5  ORCHESTRATOR   actor model: mailboxes, supervision,     │
│                    hot-swap, hierarchy (spawn/monitor/link) │
├─────────────────────────────────────────────────────────────┤
│ L4  COMPOSITION    cartridge-calls-cartridge (the v2 ABI's  │
│                    call()); typed ports; capability grants  │
├─────────────────────────────────────────────────────────────┤
│ L3  RUNTIME        wasm interpreter hosted in vanish's wasm │
│                    (WAMR-style), fuel-metered, OPFS-backed  │
├─────────────────────────────────────────────────────────────┤
│ L2  LANGUAGE       rustlite: a Rust subset whose compiler   │
│                    is pure Rust, runs in the browser, and   │
│                    emits .wasm bytes directly               │
├─────────────────────────────────────────────────────────────┤
│ L1  ABI            the host surface cartridges program      │
│                    against (imports), versioned forever     │
└─────────────────────────────────────────────────────────────┘
```

build order is bottom-up: L1 → L2 → L3 → L4 → L5. each layer is useful
before the next exists (article i: capabilities ship when their consumer
arrives, not speculatively).

## 4. L1 — the vanish cartridge ABI (v1)

vanish's ABI starts smaller than x402's because vanish has different needs
(no payments yet; cognition and tool-like computation first). named import
module `vanish`. all pointers are guest-linear-memory offsets; all strings
are (ptr: i32, len: i32) pairs.

guest IMPORTS (host provides):

- `log(level: i32, ptr: i32, len: i32)` — structured feed output
- `now_ms() -> i64` — clock (the ONLY time source; D1 forbids deadlines, so
  there is deliberately NO sleep/deadline import)
- `store_get(k_ptr: i32, k_len: i32) -> i64` — packed (ptr<<32|len), 0 = miss
- `store_set(k_ptr: i32, k_len: i32, v_ptr: i32, v_len: i32) -> i32`
- `emit(topic_ptr: i32, topic_len: i32, ptr: i32, len: i32)` — publish an
  event to the orchestrator bus (L4/L5's mailbox primitive)

guest EXPORTS (host calls):

- `cart_init(config_ptr: i32, config_len: i32) -> i32` — once at load
- `cart_handle(msg_ptr: i32, msg_len: i32) -> i64` — packed response;
  the workhorse. every message in, one result out.
- `cart_alloc(size: i32) -> i32` — host asks where to write the next buffer

versioning: the manifest carries `abi_version`; the runtime refuses to load
a cartridge built for an ABI newer than its own, LOUDLY (D4). breaking
changes bump the major and keep the old loading — old cartridges never break
when the host moves.

**v2 (landed 2026-08-25):** `call(port_ptr, port_len, msg_ptr, msg_len)
-> i64` — a synchronous request to the provider of `port`, mediated by
the orchestrator (§7/§8), allowed only for ports the caller declared under
`requires`, charged to the caller's fuel, answered as a packed (ptr, len)
written into the caller's memory; 0 = the call did not happen (every such
case is an orchestrator event). a manifest declaring `abi_version: 1`
cannot import it (`HostFn::since`).

## 5. L2 — rustlite-for-vanish

the language must be small enough to compile with a few thousand lines of
pure Rust, and expressive enough to write cognitive modules. starting set:

- types: i32, i64, f32, f64, bool, fixed-size arrays, structs of those
- functions, let bindings, arithmetic, comparisons, if/match
- loops with explicit `while` (no iterators, no closures, no generics v1)
- `extern "C"` blocks mapping to the L1 ABI imports (a thin `vanish::*`
  prelude makes them ergonomic)
- #[no_mangle] pub extern "C" exports for cart_init/cart_handle/cart_alloc
- NO: std, alloc, traits beyond auto, lifetimes-as-constraints, macros,
  async, panics (a panic = trap = fuel exhaustion, surfaced as such)

compilation pipeline: lexer → Pratt parser → typed AST (Hindley-Milner-lite:
inference over the closed type set, no trait solving) → straight-line wasm
module emission (types map 1:1 to wasm valtypes; control flow maps to
blocks/loops/br_if). no LLVM, no cranelift, no linking problem: emit final
bytes directly. this is the whole trick — rustc-on-wasm fails on the LINKER,
and a subset language with no linking step doesn't have one.

verification: golden tests (source → expected wasm bytes/hex), round-trip
validation of emitted modules with `wasmparser` in CI, and behavioral tests
that run the compiled module through L3 and assert observable outputs.

## 6. L3 — the runtime

a wasm interpreter INSIDE vanish's own wasm module. options ranked:

1. **compile WAMR (interpreter profile) to wasm32-unknown-unknown** — most
   credible path; WAMR is built for embedding, has a fast interpreter, and
   others have compiled C/wasm toolchains to wasm before (wasm-interp
   demos). risk: build complexity, memory footprint.
2. **write a small wasm interpreter in Rust** targeting only the wasm
   feature subset rustlite emits (no simd, no threads, no reference types).
   more code we own, but perfectly scoped: we only need to interpret what
   OUR compiler emits, which is a tiny, frozen dialect.
3. hybrid: start with (2) sized to rustlite's emission set, swap in WAMR
   later if third-party cartridges (compiled by full rustc) need supporting.

fuel metering: every instruction costs from a per-invocation budget; a
cartridge that burns it traps and reports honestly (D4). this is what makes
untrusted/hostile cartridges safe to run beside the agent loop.

storage backing `store_get/store_set`: OPFS namespace per-cartridge
(`cartridges/{slug}/kv/...`), so durability rules (D2) apply to cartridges
for free.

## 7. L4 — composition

- `call(slug, message)` inside the guest ABI → orchestrator-mediated
  message send (never direct instantiation): capability-based, auditable,
  revocable.
- manifests declare REQUIRED ports (`requires: ["vision.embedding"]`) and
  PROVIDED ports (`provides: ["vision.embedding"]`); the loader wires
  providers to requirers at compose time and refuses loudly on missing
  providers (D4) instead of failing at call time.
- cycles detected at wire time; lateral/up/down hierarchy is just a graph,
  so "apps made of apps" falls out of ports rather than a special case.

## 8. L5 — the orchestrator (actor semantics, minimal)

- every cartridge instance is an actor: private state (its kv namespace +
  linear memory), a mailbox, and behavior defined by its exported
  cart_handle.
- messages are bytes + topic; delivery is at-most-once with an ack the
  guest returns packed in cart_handle's result.
- supervision: the orchestrator restarts a trapped cartridge with
  exponential backoff, up to a budget; after that it marks it failed and
  tells the user loudly. never silently (D4).
- hot-swap: unloading an actor waits for its current message to finish,
  swaps the module, replays nothing (state lives in kv, not memory) — this
  is why state-in-kv matters architecturally: it is what makes hot-swap
  safe.
- the COGNITIVE orchestrator is just the first consumer: routing prompts to
  whichever `cognitive-*` cartridge provides the active "reasoning" port.
  swapping vanish's own reasoning module becomes a manifest edit.

## 9. the LLM-as-execution-environment horizon (deliberate, not now)

the owner's deepest cut: train/fine-tune a model whose output space is
OPODES for this VM rather than source text. rustlite's instruction selection
gives us the opcode vocabulary for free; the cartridge runtime gives us the
verifier (a hostile opcode stream is just a trapped cartridge, fuel-bounded).
sequence: land L1–L5 with text-emitting models first; record (prompt →
opcode trace) pairs from every successful cartridge build; once the corpus
exists, fine-tune against VERIFIED traces — the runtime is the reward model.
do not start this until the corpus collection exists, or there is nothing
to train on.

**the collection exists as of 2026-08-26** (`src/cartridges/corpus.rs`,
build item 9). every candidate policy that goes through a swap — from the
ui or from the agent's `swap_cartridge` tool — is recorded as a `Sample`:
its rustlite source, its opcode trace, where it came from, and the
runtime's verdict. it is persisted to `vanish-cartridges/corpus.json`,
bounded at `MAX_SAMPLES`, and keyed by a fingerprint of the source, so
re-trying a program updates its verdict rather than growing the log.

three decisions worth defending:

- **refusals are kept.** a corpus of only successes teaches nothing about
  the boundary, and the boundary is where a generated program actually
  fails. a program that emitted and then trapped keeps its trace — a
  refused opcode sequence with the rehearsal's own words attached is the
  most useful negative available. a program that never compiled has no
  trace, and the sample says so rather than inventing one.
- **the trace drops operands.** emission is deterministic, so
  `emit_module(parse(source))` rebuilds the module exactly: the source IS
  the record, and the trace is the same program in the shape a model would
  emit it. storing operands would double the corpus to hold what it already
  holds.
- **the histogram is over VERIFIED programs only.** an opcode sequence that
  was rejected is not evidence about what good code looks like, even though
  it is excellent evidence about what fails.

the `prompt` half of §9's pair is the `intent` argument on
`swap_cartridge` — one line from the model on what it meant the program to
do. it is required, including on attempts that are refused.

WHAT THIS STILL DOES NOT GIVE US: a judgement of whether a policy HELPED.
the corpus records what was tried and whether it ran. "did the shaped prompt
produce a better answer" needs an outcome signal the loop does not yet
collect, and no amount of corpus makes that question answerable.

## 10. relationship to the charter

- article i (loop closes): cartridges make CAPABILITIES swappable — a new
  capability is a new module, not a rebuild; the missing-capability response
  gets a new verb.
- article vii (gradient measured): each layer lands with evals — golden
  compiler tests, runtime fuel/trap tests, orchestration delivery tests.
  "v1" for this doc = one cognitive cartridge, written in rustlite,
  compiled in the browser, running under the orchestrator, hot-swapped live.
- article viii (harness not output): the runtime/compiler/orchestrator ARE
  harness. every future improvement inherits them.

## 12. the cognitive orchestrator — design (item 8, written 2026-08-25)

**constraint that shapes everything:** the model call is an async streamed
fetch in the worker; the L3 interpreter is synchronous and cannot suspend
(no continuations, no resume bookkeeping — by design, §6). therefore a
cartridge cannot MAKE the model call. what it can own is the policy around
it: what goes in, what comes out, and what is remembered. that is the
"reasoning module" in v1 — thin by construction, but the seam is real and
the swap is live.

**the two-phase protocol** (`src/cartridges/cognitive.rs`):

- port `reasoning`       — before the model. in: the user's prompt. out:
  the prompt to send (empty = unchanged).
- port `reasoning.after` — after the model. in: the assistant's answer.
  out: a note for the feed (empty = nothing). the cartridge digests the
  answer into kv (memory across prompts and across swaps).
- framing: `cart_handle` receives bytes only, and one cartridge normally
  provides both ports, so every message is prefixed with a phase byte
  (0x00 prompt, 0x01 answer). ports remain the wiring/capability key; the
  phase byte is the guest's dispatch.
- `Cognition::before/after` drive it through `Orchestrator::request` — a
  host-originated synchronous request to whichever cartridge provides the
  port (plan §8's routing rule, made literal).

**never hostage (article iv, D9):** no provider → passthrough, silently.
a provider that traps, is restarting, or has failed → passthrough with a
feed note; supervision does the rest; a swap revives it mid-conversation.
the reasoning module is an enhancement of the loop, not a dependency.

**reference modules** (`REASONING_V1`, `REASONING_V2`, rustlite, compiled
in the browser): v1 = passthrough + remember last prompt/answer; v2 = the
same plus a visible "[v2] " prefix, so a live hot-swap is observable from
the feed without instrumentation.

**the host** (`src/cartridges/memhost.rs`): the `Host` trait is sync and
opfs is async, so each cartridge's kv lives in a `MemHost` during a step
and the worker flushes it to `vanish-cartridges/{slug}/kv.json` right
after — the transcript checkpoint's write-behind shape; the window in
which work exists only in memory is one pump (D2). logs/emits are taken
the same way and rendered as feed notes; time is set per step (D1).

**8b — browser wiring (built 2026-08-26):** the worker owns the
`Cognition<MemHost>` in a `COGNITION` thread_local and boots it from opfs
(`boot_cognition`): the source the user last swapped in, else the
reference v1, over a kv seeded from the last flush. `agent::run` takes a
`&dyn Reasoning` — `NoReasoning` is the identity policy, `CartridgeReasoning`
is the worker's — and calls `before` exactly where the user prompt is
pushed (what the cartridge returns is both what the model is asked and
what the transcript records, so a replay cannot disagree with what was
sent) and `after` on every turn that carries no tool calls, which is the
only point in the loop where the model has answered rather than asked for
work. each hook sets the clock, takes the guest's own log lines as feed
notes, and hands back `KvFlush`es the worker spawns into opfs — the hook
itself stays synchronous, the durability does not wait on it (D2).

`Command::SwapCartridge { manifest, source }` compiles rustlite in the
worker and calls `Orchestrator::swap`; a manifest that will not parse or a
source that will not compile changes neither the running module nor what
is on disk, and the compiler's own words reach the feed. a successful swap
writes `source.rustlite` + `manifest.json` beside the kv, so it survives a
reload — and a saved module that later stops compiling falls back to the
reference v1 loudly rather than leaving the loop with no policy at all.
the right rail carries the editor plus "load v1"/"load v2" buttons wired
to the crate's own reference constants.

state lives under `vanish-cartridges/{slug}/` rather than `cartridges/`:
opfs's root is shared with the working-tree mirror, so the obvious name
would collide with a source directory of that name. the store is ONE json
file per cartridge (hex keys and values, versioned) because opfs
directory iteration is the least dependable corner of the api — the same
reason the tree keeps an index file.

live proof, watched on the preview of bdc7008: boot brings the policy up
and puts the GUEST's own init log on the feed; "load v2" + "hot-swap
policy" compiles rustlite in the worker, swaps atomically and re-inits with
no reload; a full page reload comes back as "your last hot-swap, restored
from opfs". the fourth link — "[v2] " on a real prompt — is not reachable
from a preview: `Command::Run` refuses at the credential check before
`agent::run` is entered, and a preview branch is its own origin with no
saved credentials. that half is owed on production, and the blind spot is
general: nothing downstream of the credential gate can be proven on a
preview.

**8c — the recursive step (built 2026-08-26):** `swap_cartridge` is a
TOOL. the agent rewrites the module it reasons with, mid-run, through the
same door the ui uses.

that door got a lock first. "it compiles" is the bar `parse_policy` sets,
and supervision catches a module that traps *later* — but between those two
sits the case that matters here: a module that compiles and then traps on
its FIRST message would be installed, crash on the next real prompt, and
cost a restart cycle and a passthrough before anyone learned why. so
`rehearse(manifest, bytes, kv, fuel)` instantiates the candidate over a
SCRATCH `MemHost` seeded from a COPY of the live memory, inits it, and puts
one message through each phase it declares. a module that will not start,
declares no `reasoning` port, or traps is refused with nothing changed —
not the running policy, not its memory, not what is on disk. the scratch
host is discarded, so a rehearsal can neither read stale state nor write
into the live store; seeding it from the live kv is deliberate, because a
policy that only works against an empty store is exactly the one that would
pass a naive check and fail in production. `Cognition::swap_policy` now
returns the `Rehearsal` alongside the slug, and both the ui and the tool
report it ("🧪 rehearsal passed: \"…\" → \"…\"").

**what a policy may do to itself while a run is in flight** (the question
8b left open): swap, but not retroactively. the prompt of the run making
the call was already shaped by the old policy and is already in the
transcript; the new module takes effect at the next hook. the tool result
says so rather than leaving the model to assume otherwise. nothing else is
restricted: the loop is never hostage to a cartridge, so the worst a bad
swap can do is degrade its own future prompts — and a module that RUNS and
reasons badly is not something the harness can catch, which is why the
system prompt names this the sharpest tool the agent has and the easiest to
misuse.

**8d — a policy worth swapping TO (built 2026-08-26):** v1 and v2 are
demonstrations. one changes nothing; the other prefixes a visible marker so
a swap is observable. neither does anything the loop could not do without
it, which made "swap your policy" an offer with no answer to "to what?".

`REASONING_V3` is the answer, and it is deliberately the smallest thing
that is genuinely NOT available otherwise. the transcript is trimmed at
`KEEP_MESSAGES` and is per-conversation; a cartridge's kv is neither. so v3
gives the agent ONE line of memory that outlives both:

- on an answer, the first `CARRY:` in it — to the end of that line — becomes
  the standing note. no marker, no change; `CARRY:` with nothing after it
  clears it.
- on a prompt, the note is prepended and a protocol line is appended. the
  module cannot edit the system prompt, so it teaches its own contract in
  the only channel it has: the prompt it is already shaping.

it is also the first reference module to read its own memory back
(`store_get`, and the packed-i64 unpack path), to assemble a prompt from
three sources, and to search bytes — items 5's intrinsics and 7's language
doing work rather than being demonstrated. the note is capped at
`STANDING_MAX` bytes: uncapped, it would grow every later prompt for as
long as the policy ran.

what v3 is NOT is a judge of whether the shaped prompt was BETTER. nothing
in the substrate can answer that yet; item 9's corpus capture is where that
starts. until then a policy can be verified to do what it says, not to
help — and the plan says so rather than letting a demo imply otherwise.
rustlite cannot be handed a rust constant, so `CARRY_MARKER`,
`STANDING_KEY`, `STANDING_MAX` and `STANDING_PROTOCOL` are mirrors of
literals inside the module and an eval pins them together.

**what a host import for model calls would mean:** a synchronous
`ask_model` cannot exist under this runtime. if a policy ever needs the
model in the loop, the honest path is the two-phase protocol extended
with a "please ask the model this, then call me back" response — the
worker does the async work and re-enters the cartridge. not until a
policy needs it (article i).

## 11. build order (each item independently verifiable)

1. [x] L1 ABI: manifest struct + validation (pure, testable now) — PR #13
2. [x] L2 rustlite: lexer + parser + typed AST + golden tests — PR #13
3. [x] L2 rustlite: wasm emission + wasmparser round-trip in ci — PR #14
4. [x] L3 runtime: stack-machine interpreter over rustlite's dialect,
       fuel-bounded; trap taxonomy — PR #15
5. [x] L1+L3 wired: cart_init/cart_handle lifecycle over the interpreter
       — abi.rs (the one table), extern/pub/if/intrinsics in rustlite,
       import/memory/export emission, host dispatch + bounded re-entry,
       lifecycle.rs. language gap noted: string literals / data segments
       (needed before 8)
6. [x] L4 ports/requires wiring + cycle detection — ports.rs (pure wire)
       + composition.rs (wire-then-load, providers-first init, route by
       port). the guest `call` import waits for 7's mediator (ABI v2)
7. [x] L5 mailboxes + supervision + hot-swap (kv-backed state) —
       orchestrator.rs: deterministic pump (time passed in), at-most-once,
       backoff ladder to Failed, atomic swap keeping host + mailbox,
       emits routed by declared capability. `call` (ABI v2) rides on it
       next; string literals + data segments first.
8. [x] cognitive orchestrator: first hot-swappable reasoning module —
       8a (design §12, cognitive.rs, Orchestrator::request, MemHost,
       reference v1/v2 modules, swap-with-kv-intact proven natively);
       8b (browser wiring: COGNITION in the worker, `Reasoning` hooks in
       agent::run, durable kv + saved source, Command::SwapCartridge and
       the right-rail editor); 8c (the recursive step: `rehearse` as the
       gate, and `swap_cartridge` as a TOOL — the agent rewrites the
       module it reasons with). the live "[v2] " swap is verified in the
       browser, not by the gate
8d.[x] a reference policy worth swapping to: REASONING_V3, the standing
       note — one line of memory that outlives the transcript, set by the
       agent writing `CARRY:` in an answer. the first module to read its
       own kv back
9. [x] corpus capture: intent → rustlite → wasm → opcode trace → verdict,
       persisted and bounded — corpus.rs, recorded at the rehearsal so
       refusals land too. §9 above for what it does and does not buy
10. [ ] the opcode-model experiment (§9) — gated on 9
