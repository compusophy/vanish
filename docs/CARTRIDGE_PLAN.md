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
8. [ ] cognitive orchestrator: first hot-swappable reasoning module
9. [ ] corpus capture: prompt → rustlite → wasm → trace, persisted
10. [ ] the opcode-model experiment (§9) — gated on 9
