# vanish task board

> standing directives and open work. read at the start of a run, update at
> the end. user feedback lands here once and stays honored.
>
> READ ORDER: CHARTER.md (mission + constitution) → this file →
> memory/status.md. the charter outranks tactics; D-rules are its case law.

## the mission (CHARTER.md, 2026-08-25)

build toward recursive self-improvement: an agent whose primary work is
making itself more capable, safely and verifiably — tool → autonomous →
general. vanish is the vehicle: self-sovereign, browser-resident, no
infrastructure between it and its work. eight articles govern every run:
close the loop, evidence over assertion, memory is identity, durability is
a right, the human is sovereign, honesty is structural, measure the
gradient, improve the harness not just the output. amendments require the
owner; the agent may only propose them.

## the architecture (as of the rust/wasm rebuild)

vanish is a single rust crate compiled to wasm and loaded twice in the
browser: `boot_ui()` on the main thread (dom only) and `boot_worker()` in a
web worker (the agent loop, the working tree, all networking). they speak
`src/protocol.rs` and nothing else. **there is no server and no serverless
function.** vercel is a static host for `web/`, nothing more.

see ARCHITECTURE.md for the full map and for why the previous design failed.

## standing directives — never regress these

- **D1 — no deadlines in the loop.** the old harness hard-killed runs at 52s
  (`RUN_HARD_DEADLINE_MS`) and blamed the platform. a worker has no request
  and no time limit. never reintroduce clock-watching, wrap-up injection, or
  "time budget" logic. a run ends when the task is done, the step ceiling is
  hit, or the user stops it.
- **D2 — never hold work only in memory.** every file write goes through
  `src/platform/opfs.rs` immediately. the old `staged: new Map()` was
  per-request and lost everything on an early exit. "uncommitted" means bytes
  on disk that differ from the last synced github blob.
- **D3 — no javascript beyond the two boot files.** `web/index.html` and
  `web/worker.js` contain 8 lines of hand-written js between them. everything
  else is rust. `web/pkg/` is wasm-bindgen output and is never edited by hand.
- **D4 — surface every failure.** a silent catch is what shipped a dropdown
  that looked fine and did nothing. errors render; missing dom ids are
  reported loudly, not skipped.
- **D5 — steps are a budget, not a target.** simple tasks end fast, hard ones
  run long. `MAX_STEPS` is a runaway backstop, not a goal.
- **D6 — there is no case policy, and never will be again.** an earlier
  version enforced lowercase in three places at once: a global css
  `text-transform`, a system-prompt rule, and this directive. the css made
  the ui misreport every filename and code sample; the prompt rule made the
  agent lowercase the code it generated, corrupting identifiers wholesale.
  `memory/status.md` even carried a warning telling the agent to work around
  its own ui. write code and prose in the casing correct for the language.
  do not reintroduce this rule in any form.
- **D7 — never await a resolved promise as a yield point.** the worker is
  single-threaded. `await`ing an already-resolved promise drains only the
  microtask queue; doing it in a `while` loop starves the event loop
  outright, and a starved worker stops dispatching `onmessage` — so Stop,
  RunState and every later Run are silently never received, and the app is
  bricked with no error anywhere. yield through `sleep_ms` (a real timer),
  and bound every wait. this exact bug shipped twice: fixed in 699ada0,
  reverted by 016f3db, fixed again with `tests/event_loop_liveness.rs`
  pinning it. that test file is load-bearing — do not delete it to make a
  refactor pass.
- **D8 — control state is never gated on durability work.** `RunFinished` is
  what returns the stop button to "run", so it is emitted BEFORE the
  checkpoint drain and the transcript save, never after. a slow or wedged
  opfs write may cost a stale transcript; it may never cost the user control
  of the app.
- **D9 — every escape hatch must work when the thing it rescues is broken.**
  Stop is the only way out of a wedged run, so it cannot depend on the run
  being healthy enough to poll for it: it takes control back by force after
  `STOP_GRACE_MS`. the same logic applies to any future recovery path — if it
  only works when things are fine, it is not a recovery path.
- **D10 — never commit from a tree you have not reconciled.** the local
  working tree can lag the branch head (three incidents and counting:
  761eaaa inherited red main, e739f00 reverted three upstream commits,
  d846fcd deleted D7–D9 and two postmortems from these very files).
  `git_commit` now refuses when the branch head differs from the sha this
  session last synced/committed; re-read every file the changeset touches
  against the raw github blob, re-apply, and commit again. verifying one
  file is not verification — the changeset is the unit.

## landed in the rebuild

the previous checkpoint (43b2c73, written to the now-removed lowercase
taskboard) asked for four things. all four are resolved:

- [x] **ota hot reload** — `src/ui/update.rs`. it polls the github branch
      head rather than a `/api/version` endpoint, because there is no server
      to host one. shows the changelog between the running build and head,
      then reloads itself; deferred while a run is in flight.
- [x] **right-hand config panel** — `web/index.html` `.rail-right`, styled in
      `web/style.css`. credentials, model, effort, loop toggle, manual commit.
- [x] **sparkle shimmer + changelog styling** — `.sparkle::after` sweep and
      `.ota-changelog`, with a `prefers-reduced-motion` opt-out.
- [x] **infra decision** — resolved by removing the infrastructure. the old
      note concluded "wasm is NOT the fix — the problem is function lifetime,
      not runtime." the first half was exactly right. what it missed is that
      leaving the function is what removes the lifetime: the loop now runs in
      a web worker, which has no request bounding it at all.

## open work

- [x] **e2e/preview gate LANDED (PR #12 merged, da6111d)** — STACKED_PRS_PLAN
      §4 items 4–5 done. .github/workflows/e2e.yml resolves vercel's preview
      of the PR head sha and ci/e2e.mjs asserts the app BOOTS (#status leaves
      "booting…" = Event::Ready); check lands where deployment_state reads,
      so merge_pr refuses until booted-green. check_deployment surfaces
      preview_url; system prompt documents green-means-booted; guards in
      tests/ci_gate.rs. LIVE VERIFICATION OWED: a deliberately-broken pr has
      not yet been watched going red — record here when the refusal fires.
      **FINDING (2026-08-25, while landing PR #16): smoke-preview has been
      RED ON EVERY PR since it landed (#12–#16).** cause: preview
      deployments sit behind Vercel Deployment Protection — the preview
      url 302s to vercel.com/sso-api — and ci/e2e.mjs's interstitial
      markers did not match the "Login – Vercel" page, so it misreported
      "app html did not mount". and it never blocked anything: main has NO
      branch protection (gh api …/branches/main/protection → 404), so a red
      smoke-preview leaves the pr MERGEABLE/UNSTABLE, and merge_pr merged
      #12–#15 through it. fixed in agent/e2e-sso-diagnosis: the smoke now
      detects the redirect + login page and exits 3 with the fix named,
      and sends VERCEL_AUTOMATION_BYPASS_SECRET as x-vercel-protection-
      bypass when the secret exists. **OWNER ACTION (the agent cannot do
      this — it needs the Vercel dashboard + repo secrets):** Vercel →
      vanish project → Settings → Deployment Protection → "Protection
      Bypass for Automation" → generate; add it as the GitHub repo secret
      `VERCEL_AUTOMATION_BYPASS_SECRET`. then, optionally, protect main
      with `verify` + `smoke-preview` as required checks so red actually
      blocks. until then: verify boots by hand (PR #16's preview was opened
      in an authenticated browser — #status "ready", build sha matched).
      LESSON from this landing: memory edits committed to an agent/ branch
      AFTER its pr is opened are stranded when the branch is squash-merged
      (commit 67e8516 never reached main). commit memory BEFORE open_pr, or
      re-apply onto the next branch.
- [ ] **cartridge substrate** — docs/CARTRIDGE_PLAN.md §11 build order:
      - [x] item 1 DONE (PR #13, 1fcd65c): L1 manifest — parse+validate,
            slug/ABI/port rules pinned both directions in
            tests/cartridges_manifest.rs.
      - [x] item 2 DONE (same pr): rustlite lexer + Pratt parser + typed
            AST, goldens/negatives in tests/rustlite_front.rs. closed type
            set {i32,i64,f32,f64,bool}; REQUIRED type annotations (checking
            stays a one-pass walk); precedence pinned by tree-shape goldens;
            Assign stmt added after the gate caught its absence; parser is
            scope-blind by design (undeclared-name assignment = CHECKER
            error for the next pass).
      - [x] item 3 DONE (PR #14, d69a259): rustlite → raw .wasm emission.
            context-typed literals via ONE shared literal_ty() called by both
            walks; while = block{loop{}} with true label semantics;
            wasmparser 0.258 round-trip validation + behavioral mini-vm
            proving double(21)=42 and count(5)=5 through real branch rules.
            float % refused at check time (wasm has no frem).
            META-LESSON PROVEN: the gate cannot catch self-consistent
            wrongness in tests verifying their own implementation — quote
            the spec in the comment, make code match words, not intent.
      - [x] item 4 DONE (PR #15, ee5c8bd): L3 runtime — decoder with
            branch targets resolved to absolute ips at decode time, fuel
            per instruction, frame-per-activation calls, named trap
            taxonomy, THE FUZZ (every truncation + single-byte corruption
            through decode+invoke, no panic permitted).
      - [x] item 5 DONE (agent/cartridge-lifecycle, 2026-08-25): L1 wired
            over L3. src/cartridges/abi.rs = the ONE ABI table (HostFn ×5,
            GuestFn ×3, pack/unpack, `Host` trait) read by compiler,
            runtime, and lifecycle alike. rustlite grew `extern "C" {}`
            imports, `pub fn` exports, `if/else`, and eight inline
            intrinsics (load_u8/store_u8/load_i32/store_i32/memory_size/
            pack/unpack_ptr/unpack_len). emitter writes import/memory/
            export sections (fixed 1 MiB memory, exported as "memory").
            runtime decodes them, dispatches import calls to the Host with
            bounds-checked COPIES (a host never sees a pointer), bounds
            host→guest re-entry (store_get → cart_alloc) at 4, caps call
            depth at 1024, refuses non-custom unknown sections instead of
            skipping them. src/cartridges/lifecycle.rs: Cartridge::load
            (manifest + imports + exports + memory all verified at the
            door) → init → handle. tests/cartridge_lifecycle.rs: an echo
            cartridge in rustlite through the REAL pipeline against a
            recording fake host, every host fn exercised, every refusal
            named, plus the corruption fuzz across the host boundary.
            three real bugs the new evals caught: void calls were
            unwritable as statements (`log(…);` refused by the checker);
            item 4's decoder underflowed `r.pos - body_end` on a short
            body (the lifecycle fuzz found it; the pure-shape fuzz never
            could); every-path-returns bodies failed wasmparser (fixed by
            a trailing `unreachable`, now a named trap).
            NOT YET in the language: string literals / data segments —
            a cartridge cannot name a topic or key except byte-by-byte
            via store_u8. needed before item 8 (a cognitive module must
            name its ports); do it as its own item.
      - [x] item 6 DONE (agent/cartridge-ports, 2026-08-25): L4 ports.
            src/cartridges/ports.rs = pure `wire(&[manifest])` → Wiring
            {providers, edges, order} or a named WireError (bad manifest,
            duplicate slug, ambiguous provider, missing provider WITH
            every requirer, cycle WITH the loop written out); order is
            providers-first and deterministic (kahn + sorted ready set,
            so manifest order never leaks into boot order).
            src/cartridges/composition.rs = Composition<H>: wire FIRST,
            then load each Cartridge (a mis-wired set never instantiates
            a memory); init_all in wiring order, stopping at the first
            refusal naming its slug; handle(slug) and handle_port(port) —
            the primitive item 8 routes prompts through. tests/
            cartridge_ports.rs (11) + tests/common/mod.rs (shared fake
            host + rustlite fixtures; a SUBDIRECTORY so the gate's
            tests/*.rs discovery does not mistake it for a suite).
            NOT in item 6 (deliberately): the guest-side `call(slug, msg)`
            import — it is an ABI v2 bump and needs item 7's mailbox to
            mediate; composition today is host-side routing by port.
      - [x] item 7 DONE (agent/cartridge-orchestrator, 2026-08-25): L5.
            src/cartridges/orchestrator.rs = Orchestrator<H> over
            Composition<ActorHost<H>>: every cartridge an actor with a
            mailbox (cap MAX_MAILBOX 256, overflow = Undeliverable event);
            `pump(now_ms, fuel)` delivers ONE message, round-robin in
            wiring order — time is PASSED IN, never read (D1, and every
            backoff decision is exact in tests); at-most-once (a message
            that crashes its actor is gone, recorded with the trap);
            supervision = re-instantiate from the Verified image with the
            SAME host (kv survives) after RESTART_BASE_MS·2^(n−1) backoff,
            MAX_RESTARTS 5 consecutive, then Health::Failed + a Failed
            event (loud); one success resets the count; a boot refusal is
            Failed immediately (not transient). hot-swap = Composition::
            swap: new wiring computed, new bytes verified, ONLY THEN the
            host moves; re-init with the remembered config; mailbox kept;
            revives a Failed actor. guest emit(topic) routes to the port's
            provider iff the emitter declared it under `requires` —
            capability-based, every denial an event; the real host still
            observes every emit first. lifecycle.rs split into
            Verified::verify + instantiate so restarts never re-decode.
            tests/cartridge_orchestrator.rs (8): fairness, capability
            routing + denial, undeliverable/full, the full backoff ladder
            to Failed with the host's init log proving state survived
            5 restarts, crash-count reset, boot refusal isolation, swap
            keeping host state + pending mail and reviving Failed, swap
            refusals leaving nothing changed.
      - [x] string literals + data segments DONE (agent/rustlite-strings,
            2026-08-25): a literal is an i64 EXPRESSION — its packed
            (ptr, len), the ABI's own string representation — so
            `unpack_ptr("inc")` / `unpack_len("inc")` feed any host call,
            and `return "static answer";` is a valid cart_handle result.
            no string type, one AST node (Expr::StrLit), one new
            intrinsic `data_end()`. wasm.rs `Layout::of` interns every
            literal SORTED (deterministic) into one active data segment
            at DATA_BASE=16; the blob must fit guest memory (compile-time
            refusal naming the size). runtime decodes section 11 (active
            mode only, `i32.const N; end` offsets, bounds checked against
            the declared memory AT DECODE — the spec would trap at
            instantiation) and `initial_memory()` re-applies segments on
            every instantiation, so restarts/swaps get their literals
            back (pinned). tests/common ALLOC now starts its heap at
            data_end(); emitter_src spells topics as literals. +7 evals
            across front/emit/runtime/lifecycle.
      - [x] `call` as ABI v2 DONE (agent/abi-v2-call, 2026-08-25):
            ABI_VERSION 2; `HostFn::Call` (since 2 — a v1 manifest that
            imports it is refused at the door naming the bump), signature
            (port_ptr, port_len, msg_ptr, msg_len) -> packed i64, 0 =
            "did not happen". RE-ENTRANCY DISCIPLINE in orchestrator.rs:
            state behind Rc<RefCell<Shared>>, every actor's host holds a
            Weak back-link + its declared `requires`; EVERY guest run is
            on a cartridge TAKEN OUT of the composition (Composition::
            take/put_back) with no borrow held, so a nested call borrows
            the rest, a busy callee is refused (not deadlocked), chains
            are bounded (MAX_CALL_CHAIN 8), and the callee runs on the
            CALLER's fuel (Cartridge::handle_with shares the counter).
            failures are soft for the guest (0) and loud for the log
            (Denied / CallFailed); a callee that traps is supervised as a
            crash — EXCEPT fuel exhaustion, which is the caller's fault:
            the callee gets `Event::Reset` (rebuilt on the next pump, no
            backoff, no crash counted — otherwise a stingy caller could
            back an innocent provider into Failed). the pump now rebuilds
            a down actor even with no mail (call-only providers must come
            back). boot/restart/swap all run guest code through the same
            take/put_back path (init_all would hold the borrow a nested
            call needs). API: with_host(slug, f) replaces host(slug);
            order() / provider_of() replace composition(). +5 orchestrator
            evals (answer round-trip + caller-pays fuel + reset, denied
            call, callee trap → soft fail + supervision + "not up"
            refusal, 3-deep chain with answers propagating, emits during
            a nested call routed after it), +2 lifecycle (host-routed call
            writes the answer into the caller's memory; v1 manifest
            refused for `call`, v1 with v1 imports still loads).
      - [x] item 8a DONE (agent/cognitive-policy, 2026-08-25): the
            reasoning policy as a hot-swappable cartridge — everything
            native-testable. DESIGN in plan §12: the model call is async
            and the interpreter cannot suspend, so a cartridge cannot
            MAKE the model call; it owns the policy AROUND it. two
            ports: `reasoning` (prompt in → prompt out, empty = as
            written) and `reasoning.after` (answer in → feed note out;
            digest into kv). every message carries a phase byte (0x00 /
            0x01) because cart_handle gets bytes only and one cartridge
            provides both ports. `Cognition<H>::before/after` over
            `Orchestrator::request` (new: host-originated synchronous
            request — rebuilds a due actor first, NotUp otherwise, trap
            = supervised crash). NEVER HOSTAGE: no provider → silent
            passthrough; crashed/restarting/failed → passthrough + feed
            note; swap revives mid-conversation. `describe(&Event)` =
            the feed line (routine traffic excluded). REASONING_V1
            (passthrough + remember) and REASONING_V2 ("[v2] " prefix +
            remember) as rustlite consts; the swap between them keeps
            kv. `MemHost` (memhost.rs): the sync write-behind host the
            worker gives each cartridge — kv + dirty set (take_dirty →
            opfs flush after each step), take_logs/take_emits for feed
            notes, set_now per step (no clock read). NO host import for
            model calls (article i — nothing needs one; §12 records the
            honest path if a policy ever does). tests/cartridge_cognitive
            (8). ComposeError::NotUp added.
      - [x] item 8b DONE (agent/cognitive-browser-wiring, 2026-08-26):
            browser wiring. `COGNITION` thread_local in worker.rs holds
            the Cognition<MemHost>; `boot_cognition()` reads
            `vanish-cartridges/reasoner/{source.rustlite,manifest.json,
            kv.json}` and boots the last swapped-in module, else
            REASONING_V1, over a seeded kv — a saved module that no
            longer compiles falls back to v1 LOUDLY (the loop always has
            a policy). agent::run takes `&dyn Reasoning` (`NoReasoning`
            = identity, `CartridgeReasoning` = the worker's): `before`
            fires exactly where the user prompt is pushed and its OUTPUT
            is what both the model and the transcript get (a replay must
            not disagree with what was sent); `after` fires on every turn
            with no tool calls — the only point where the model answered
            rather than asked for work. each hook sets the clock (D1),
            takes guest logs as feed notes, and hands back KvFlush values
            the worker spawns into opfs (D2 — the hook stays sync, the
            write does not block it). `Command::SwapCartridge {manifest,
            source}` compiles rustlite in the worker → Orchestrator::swap
            → persists source+manifest; a refusal changes NOTHING (not
            the running module, not disk) and carries the compiler's own
            words. right rail: source textarea + optional manifest +
            "load v1"/"load v2" (the crate's own consts, so a reference
            policy that stops compiling breaks the build, not the button)
            + "hot-swap policy". state lives under `vanish-cartridges/`
            NOT `cartridges/`: opfs's root is shared with the working-tree
            mirror and the obvious name would collide with a source dir.
            kv is ONE versioned json file of hex pairs per cartridge —
            opfs directory iteration is the least dependable corner of the
            api (same reason the tree keeps an index file).
            tests/cognitive_wiring.rs (12): encoding round-trip incl.
            non-utf8, every corrupt-store refusal named, flush only when
            something changed, memory surviving a "reload", boot fallback,
            swap changing the next prompt with kv intact, a refused swap
            leaving the policy alone.
            LIVE VERIFICATION (preview of bdc7008, watched in an
            authenticated browser 2026-08-26) — THREE OF THE FOUR LINKS
            PROVEN, one still owed:
              [x] boot: the feed shows "🧠 reasoning policy 'reasoner' up
                  (reference v1)" and "🧠 reasoner: reasoning v1 up:
                  passthrough + remember" — the cognition instantiates in
                  the worker and the GUEST's own init log reaches the feed.
              [x] swap: "load v2" fills the editor; "hot-swap policy" gives
                  "🔁 cartridge 'reasoner' hot-swapped", "🧠 reasoner:
                  reasoning v2 up: prefix + remember", "🔁 'reasoner' is now
                  the reasoning policy" — rustlite COMPILED IN THE WORKER,
                  swapped atomically, re-inited, all without a reload.
              [x] persistence: after a full page reload the boot note reads
                  "up (your last hot-swap, restored from opfs)" followed by
                  v2's init log — source.rustlite + manifest.json were
                  written and read back.
              [ ] the "[v2] " prefix on a real prompt. NOT reachable on a
                  preview: Command::Run refuses on the credential check in
                  worker.rs BEFORE agent::run is entered, and a preview
                  branch is its own origin, so its opfs has no saved
                  credentials. do this on production (or in a preview with
                  credentials entered): type a prompt, load v2, hot-swap,
                  type another — the second must render with "[v2] ". the
                  before-hook itself is pinned natively in
                  tests/cognitive_wiring.rs; what is unproven is only that
                  agent::run reaches it in the browser.
      - [ ] item 8c NEXT: the RECURSIVE step — a `swap_cartridge` TOOL, so
            the agent rewrites its own reasoning policy mid-run rather than
            waiting for a human to paste one. the worker half already
            exists (swap_cartridge()); this is a tools.rs entry plus the
            system-prompt line, and a decision about what a policy is
            allowed to do to itself while a run is in flight.
      - [ ] items 9–10 per plan §11 (corpus capture → opcode-model
            experiment).
      strategic context: owner wants composable hot-swappable cognitive
      modules (actor model), NOT a localharness clone; opcode-model horizon
      gated on corpus capture (plan §9).
- [x] **path-claim registry LANDED (agent/path-claim-registry)** —
      STACKED_PRS_PLAN §4 item 3 (§2 C1): pure ClaimRegistry in
      src/agent/claims.rs (ttl 30min, saturating expiry), thread_local
      session accessors, wired into write_file/edit_file (advisory ⚠
      warning naming the holder), git_commit (release committed paths),
      git_status (`path_claims` + `claims_expired`), worker run teardown
      (release on end). 13 evals in tests/path_claims.rs with negative
      controls. DORMANT until the phase-2 worker pool exists — single
      worker means nothing can contest. live verification owed then:
      two conversations edit one path → mutual ⚠; run ends → release
      note; git_status drains to empty.
- [ ] **worker pool / multiagent phase 2** — now unblocked by the claim
      registry: HashMap<conversation, WorkerHandle> in ui/mod.rs, lazy
      spawn, cap 3–4. claims give concurrent conversations their early
      warning; git strategy (per-conversation agent/* branches) already
      landed via branch_for_conversation. re-read docs/MULTIAGENT_PLAN.md
      phases 2–3 before building.
- [x] **e2e workflow + preview deploys — DONE, PR #12 (see open work top)**.
- [x] **pr_wait LANDED (PR #6 merged, squash ff1ad44)** — pr_status polling
      loops (8–10 identical calls per merge) flooded conversations; the user
      called it out explicitly. NEVER poll pr_status in a loop again: call
      `pr_wait` ONCE — it sleeps inside a single tool call (10s interval,
      300s budget) and returns one settled answer. merge_pr still gates on
      settled green. pacing pinned by tests in tests/branch_policy.rs.
- [x] **check_deployment bug RESOLVED (PR #8 merged, squash 3772afb)** —
      with no sha argument it used to check MAIN's head instead of the
      session branch's new commit, and counted a CANCELLED duplicate
      workflow run as "failure" while its own build log showed every suite
      passing. both fixed: `default_deploy_target(synced_head, live_head)`
      prefers the session's synced head (2 tests in branch_policy.rs), and
      cancelled checks are dropped BEFORE aggregation as non-verdicts (3
      negative-control tests in loop_nervous_system.rs). verified live
      during a health check: check_deployment with no sha reported the
      session's own head with verdict success, not main's.
- [x] **PROMOTION BLOCKED ON TOKEN SCOPE #3 — RESOLVED (PR #2 merged)**.
      open_pr had returned http 403 because the fine-grained PAT lacked
      "Pull requests: read and write". COMPLETE fine-grained token scope
      set for vanish (recorded so the next wall is diagnosed in one step):
      Contents rw · Workflows rw · Pull requests rw · Metadata read
      (auto-set); Checks read + Actions read recommended for reading CI
      results. Workflows alone was NEVER enough — the earlier taskboard
      note asking only for Workflows was incomplete and cost a session.
      PR #2 (agent/fix-red-landing-and-self-config → main) merged green:
      mergeable=true, both gates success at 4ee87bc, squash e48e4ee.
      OPERATIONAL: open_pr REFUSES while the session sits on main —
      git_checkout the agent/ branch first (a page reload resets the
      session branch to main; reconcile then checkout).
- [ ] live verification owed: ∞ loop restart after failure/step-limit;
      stop mid-restart keeps it down; browser close+reopen within 12h
      resumes; restart budget saturates at 6/hour with the pause note;
      ⚙ self-config note appears in feed on boot after mirror is seeded.
- [ ] consider: a guard test that pins build.sh's delegation AND reads
      ci/run_tests.sh for the same gate id (done); next structural item
      is verifying committed bytes vs local for EVERY file in an atomic
      changeset (spot-check rule written into status.md this run).
- [x] **boot reconcile error ROOT-CAUSED AND FIXED (agent/reconcile-double-fire)**.
      the recurring "reconcile error ... removeEntry ...
      NoModificationAllowedError" after every completion was NOT random
      browser flakiness: TWO Configure commands arrive at every boot (the
      worker self-configures from the opfs config mirror AND the ui sends its
      own on Ready), and the auto-reconcile latch closed only AFTER the first
      pass finished — check-then-set across an await — so both tasks passed
      the gate and ran concurrent reconciles over the same cache files;
      chrome refuses removeEntry while another task holds the file open.
      worse, reconcile propagated the error with `?`, aborting the whole D10
      arming pass on ONE locked file. fixed three ways, pinned by new suite
      tests/boot_reconcile.rs: (1) atomic claim gate (checked+set in one
      STATE.with, through should_auto_reconcile so the shared gate stays
      load-bearing); (2) opfs::delete retries locked files on a real timer
      (50ms x10) instead of failing — D7-compliant, no resolved-promise
      awaits; (3) per-file delete failures collected into
      ReconcileReport.failed instead of aborting, surfaced in both the boot
      note and sync_repo output; a failed PASS still releases the claim so
      the next Configure retries. LIVE VERIFICATION OWED: reload the app,
      expect exactly ONE "⇅ tree reconciled" note and NO red reconcile error.

- [x] **UNBLOCK ALL COMMITS — superseded by agent/fix-red-landing-and-self-config**.
      the earlier landing on agent/ci-gate-and-loop-survival (0582489) went
      RED on both gates and was NEVER merged — an old taskboard entry here
      falsely claimed a green-checked promotion; corrected this run after
      finding main still missing ci/run_tests.sh entirely. what survived:
      docs/ci-workflow.yml is a retired pointer stub (do not re-copy it;
      tests/ci_gate.rs enforces the live .github/workflows/ci.yml). the
      red landing's diagnosed causes are all fixed on the superseding
      branch, which landed as PR #2: wasm check --lib --bins only,
      stale partial control.rs restored, build.sh back to delegation,
      diagnostics branch loop.

## landed (overnight-loop survival + ci gate, agent/ci-gate-and-loop-survival)

- [x] **overnight-loop survival**: decide_after_run_end restarts
      loop-mode runs 5s after failed/step_limit/completed endings (never
      after stop, never off-loop-mode, never onto a thread the user
      switched to, and NEVER for batch tasks — the driver owns its queue,
      a successor there races it or ghosts after drain; found in review,
      signature gained an in_batch flag + eval);
      resume_marker_is_fresh expires boot markers at 12h with an explicit
      too-old note instead of surprise runs; RestartBudget caps automatic
      restarts at 6/hour, resets on manual run. evals in
      tests/loop_nervous_system.rs incl. negative controls.
      live verification still owed: toggle ∞, force a failure,
      watch "∞ loop mode continues — restarting in 5s", confirm stop
      mid-restart keeps the loop down; ALSO verify a full browser close +
      reopen within 12h continues the loop (marker → resume → loop_mode
      persists via saved Config → continuation re-arms).

- [ ] **v1 / benchmark readiness** — build order: ~~(1) auto-reconcile~~
      DONE · ~~(2) batch/task-queue + export~~ DONE (2249454) ·
      ~~(3) internal eval suite~~ **DONE (c8c7c6c)** → (4) branch
      isolation via agent/* refs (STACKED_PRS_PLAN §4) BEFORE any
      concurrency or external benchmark — external runs must not be able
      to take down production main. multiworker pool only after 4.

- [ ] **run the first live benchmark**: press "run benchmark" (or console:
      `vanish` ui → Command::RunBenchmark via the worker). expect 5 tasks
      running one-by-one, then a scorecard note ("benchmark: N/5 passed")
      and vanish-bench/report.json in the file tree. NOTE: bench-rust-fn
      edits src/lib.rs — run it on a branch you are willing to commit to,
      or expect the dirty file. grading is mechanical; a fail means the
      agent did not do the edit, not that the checker is broken.

- [ ] **verify batch mode live**: enqueue a 2-task batch (e.g. via console:
      `vanish.enqueue_batch('[{"id":"t1","prompt":"read README.md and say
      done"},{"id":"t2","prompt":"list files"}]')`). expect ▶ task notes,
      results.json appearing in the file tree after each task, ☑ finished
      card. then reload mid-batch: expect "↻ resuming interrupted batch".
      stop mid-batch: expect "cancelled" and remaining tasks dropped.

- [ ] **verify auto-reconcile live** (landed 7b9c43a): reload the app with
      credentials saved. expect the "⇅ tree reconciled against <sha> at
      boot" note in the feed within a few seconds of Configure; a second
      Configure (re-save settings) must NOT re-run it. if upstream moved
      since the last session, expect "N stale cache(s) dropped".

- [ ] **verify universal resume live** (landed 5d9ee87 + b4de758, needs a
      real discard to prove it): start a run, switch tabs long enough for
      the browser to freeze/discard the page (memory saver ~2h desktop,
      minutes on mobile), return. expect a "↻ run was interrupted …
      resuming once settings load" note and the run continuing from its
      last checkpoint in the SAME conversation even if another thread was
      active at boot. also verify: a run that ENDS normally leaves no
      marker (no involuntary resume on next reload), and deleting the
      interrupted thread before returning produces NO surprise run.
      research note for whoever picks this up: worker timers are NOT
      visibility-throttled (WHATWG HTML §8.7: only Window timers wait on
      document visibility; WorkerGlobalScope does not) — the killer is
      tab DISCARD (memory saver / mobile os), which fires no event. any
      future "runs die when hidden" report starts there, not at throttling.

- [ ] **auto-reconcile at boot — RESOLVED (7b9c43a, this run)**: runs on the
      first Configure with a verified github token (not Event::Ready — no
      credentials exist at literal boot; the board's original wording was
      wrong). shared pass with sync_repo; head seeded into every run's
      workspace so the first-commit D10 blind spot is closed. live
      verification still owed (item above).
      discovered via the 37-file cache staleness found this run — see
      status.md "the 37-file discovery".
- [x] **mid-run unlock** (51815df): switching conversations and creating
      new ones works while a run continues in the background (guarded
      write-back + final save addressed to the run's conversation_id).
      deleting the RUNNING thread still refuses, correctly. this was the
      biggest "bricked" complaint for loop mode: an infinite run made
      everything else untouchable.
- [ ] verify after a live run: switch to another thread mid-run, confirm
      the background run's traffic shows as sidebar badges and switching
      back replays its full history.

- [x] **loop resilience** (7a6bfb2): transient llm failures retry with
      exponential backoff (4 retries, reset on success, give up after 5
      consecutive); DeployState::from verdict matrix pinned by tests.
      unattended loop mode no longer dies on a rate-limit blip.

- [ ] multi-agent phase 2: the worker pool — Attach command landed (8fc29ca,
      a worker adopts a specific conversation without touching
      index.active; adoption logic shared via adopt_conversation).
      RE-EVALUATE FIRST: mid-run switching on the single worker now works
      (51815df), which may satisfy most of what the pool was for. decide
      whether true PARALLEL runs (two agents at once) are still wanted
      before building HashMap<conversation, WorkerHandle>; if yes, git
      strategy must land before concurrent agents share one tree.

- [x] **verification layer for autonomous loop mode** — LANDED (267647f),
      EXTENDED into behavioral evals (1d3a6a8): cargo test runs in build.sh
      per-suite with markers; a failing test fails the deploy. suites:
      protocol contract, platform logic (+ reconcile_entry), loop nervous
      system, event-loop liveness, streaming, and agent_evals (failure
      storm, mid-batch stop replayability, restored transcripts, after-turn
      routing, seeding) — the evals carry negative controls that directly
      assert rejection verdicts. system prompt rule 8 requires each
      iteration to state observable behavior + verification. next
      hardening step: evals for ui-state machines (dock transitions,
      thread routing) once their logic is extracted pure; live-fire the
      D10 refusal in a real session; verify bell + OTA notices render.

- [x] web access — `http_fetch`, `web_read` (r.jina.ai reader proxy),
      `web_search` (duckduckgo instant answers) in `src/agent/tools.rs`,
      wired to the existing `src/agent/http.rs` fetch client. no new infra.
      open follow-ups: r.jina.ai may rate-limit anonymous requests; if
      `web_read` starts returning 429, consider caching reads in opfs.
- [x] rail collapse buttons mirrored — both now sit on the middle-facing
      edge of their panel (left rail: trailing in header; right rail:
      leading). 778f294.
- [x] wire the file tree: clicking a path opens it in an editor pane
      (f59e491). delegation on #tree survives re-renders; save writes through
      Command::WriteFile; TreeChanged confirms. next iteration: a "commit"
      button in the editor footer driving Command::Commit for the current
      path set, and dirty-state highlighting of the open file.
## open work

- [x] **stuck stop button** — ROOT-CAUSED AND CLOSED (695abf0/ee89601).
      RunFinished — the only event that flips the dock back to run — was
      queued BEHIND the transcript save, so a slow/wedged opfs write held
      the control transition indefinitely. fixed three ways: emitted before
      the save now; a 3s RunState heartbeat reconciles the dock against the
      worker's own running flag while a run is believed active;
      touches_run_state() guarantees start/finish bypass background-thread
      routing. if it EVER recurs, the feed note "state reconciled after a
      lost finish event" is the diagnostic fingerprint.
- [ ] verify after next deploy: run a short task, confirm the button flips
      back to run on completion within ~3s, with no reconciliation note
      (the note appearing means the ordering fix alone wasn't enough and
      the heartbeat caught it — still fixed, but report it).

- [ ] settings persistence was hardened in 4480fc9 (serde defaults,
      corrupt-store recovery, boot write-back). verify after the next
      deploy that a plain reload no longer asks for save settings; if the
      user still sees the symptom, the next suspect is the deploy pipeline
      serving a stale pkg/ against fresh html.
- [ ] conversation threads — partially done: transcripts are per-conversation
      opfs files and switching works while idle. remaining: a run pins its
      worker (`reject_while_running` refuses new-chat/switch/delete mid-run).
      full fix is multi-worker concurrency — see docs/MULTIAGENT_PLAN.md
      phases 1–2 (thread-tagged events + a worker pool keyed by conversation).
- [ ] multi-agent parallelism — phase 1 GROUNDWORK LANDED (e56fc72): all
      run-scoped events carry a `thread` tag (serde-default, old payloads
      still parse); the worker stamps the active conversation onto every
      emission; feed::render routes mismatched tags to a compact
      `.conv-activity` badge on that conversation's sidebar row;
      `Ui::active_thread` tracks the visible thread from Conversations
      events. REMAINING for phase 1→2: nothing user-visible changes until a
      second worker exists — next concrete task is the worker pool in
      ui/mod.rs (HashMap<conversation, WorkerHandle>, lazy spawn, cap 3–4),
      then git strategy BEFORE enabling concurrent runs on one tree
      (docs/MULTIAGENT_PLAN.md phases 2–3).
- [x] stacked prs design doc exists (docs/STACKED_PRS_PLAN.md)
- [ ] stacked prs / parallel diffs / preview-gated deploys — full design in
      docs/STACKED_PRS_PLAN.md, extends multiagent phase 3. adopted: the
      stacked-pr MODEL over plain REST (our commits already name parents
      explicitly; no cli tools — they need local git). plan: opfs path-claim
      registry (early warning on overlapping edits), agent/* branches + PRs
      via new github.rs primitives (create_ref, compare, create_pr,
      merge_pr), preview deploys gated by a playwright e2e workflow whose
      check runs deployment_state() already reads. main becomes
      promote-on-green, never pushed blind. force-push only ever to agent/*
      refs. concrete build order in §4 of that doc.
- [x] persist conversation history to opfs so a reload does not lose the
      thread — done: `src/platform/transcript.rs`, `Event::HistoryRestored`
      replay on boot, retention cap (200 messages / 4MB), clear-conversation
      button. ota reloads are now non-destructive.
- [x] save the transcript after each step, not just at run end, so even
      "update now anyway" clicked mid-run loses nothing. done in 030256f +
      761eaaa: agent::run takes a persist callback (prompt / each tool
      result / loop nudges) and the worker checkpoints through a serialized
      drain queue. a reload mid-run now costs at most the step in flight.
- [x] prompt drafts survive reloads — localStorage write-through on input,
      restored at boot (`vanish.prompt.draft`), cleared when sent.
- [x] loop mode survives page refreshes (a282862 + ba97c58). a LoopResume
      marker in the transcript index is written when a loop run starts and
      cleared when it ends; on boot a marker for the active conversation
      parks the prompt in PENDING_RESUME until Configure confirms working
      credentials, then start_run resumes. take_loop_resume clears on read,
      so a failed resume cannot loop boots forever. next iteration: surface
      the pending-resume state in the ui (a "resuming…" chip) so an
      interrupted loop is visible even if Configure never arrives.
- [x] web/index.html vercel token/team-id inputs — landed (e56fc72) under a
      collapsed `<details class="adv-creds">` in the credentials section;
      hydrate/collect already read cfg-vercel-token / cfg-vercel-team, so
      build-log reading is now fully user-configurable.
- [ ] `sync_repo` only refreshes the tree listing; it does not yet reconcile
      upstream changes against dirty local files. related incidents (twice
      now): local read_file served stale content — for tools.rs in an earlier
      run, and this run for agent/mod.rs where whole emissions were missing
      from the local view while the deployed blob had them. when a file you
      did not touch this session surprises you, fetch the raw blob from
      github before trusting the local copy. also: git_commit once shipped
      6 of 7 modified files — always re-run git_status after committing and
      verify the file count against what you edited.
      **UPGRADED after incident #3 (ba61277): stale-tree clobbering is no
      longer hypothetical. e739f00 shipped two stale source files that
      silently reverted three upstream commits while looking like a clean
      diff; the red build was the only detector. STRUCTURAL FIX LANDED
      (this run, D10): git_commit refuses when the branch head moved since
      this session's last sync/commit, and sync_repo now drops clean cache
      entries that diverged upstream (never dirty ones) so reads re-fetch.
      the remaining manual step — re-reading each changeset file against
      github before re-applying an edit — is enforced by the refusal
      message. mark resolved once one live run passes through the refusal
      and recovers correctly.**
- [ ] notification centre: the bell now mounts itself (ba61277) and update
      notices land behind it instead of vanishing. verify live after deploy:
      bell visible at the right edge of .dock-status, click opens the
      panel, an ota notice appears inside it rather than nowhere.
- [x] RESOLVED incident: update.rs referenced super::notify::* but ui/mod.rs
      never declared `mod notify` — main was red from that moment and every
      deploy was pinned behind it. fixed in 5aa1e7e. lesson recorded: adding
      a module file without its declaration is invisible locally if nothing
      else references it, but breaks main the moment another file does.
      after landing a new .rs file, confirm it is declared in its parent
      mod.rs in the same commit.
- [ ] wasm64 is a one-line target change once `wasm64-unknown-unknown` leaves
      tier 3 and wasm-bindgen supports it. wasm32 gives a 4gb address space,
      which is far past what this needs.

## history worth not repeating

three separate failures had one cause — a long-lived stateful process inside
a short-lived stateless container:

1. runs died at a self-imposed 52s wall while `vercel.json` said 300s.
2. staged edits lived in per-request memory and vanished with the request.
3. a browser-owned loop was written to fix (1) and (2) and then never wired
   up — `public/app.js` still called the old endpoint. dead code.

the rebuild removes the container, so none of the three has anywhere to live.

## history worth not repeating, part 2

the agent once told the user it had no network access and no way to search
the web. this was false: `src/agent/http.rs` already had a full fetch client
(openrouter, github api, sse streaming) — the capability existed one layer
below the tool list, and the agent described itself from the tool list
instead of reading its own source. lesson: **inventory the substrate before
declaring a limit.** "i don't have a tool for X" and "X is impossible" are
different claims, and only the first is usually true. the standing fix is in
the system prompt: notice a missing capability → treat it as a work item →
edit the source → write the lesson to memory/.
