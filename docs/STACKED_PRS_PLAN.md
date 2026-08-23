# stacked prs, parallel diffs, and preview-gated deploys

> written in response to: "i know about worktrees, but there is a new thing,
> stacked prs … we should look into that, because i don't know how we are
> going to manage parallel diffs — and deploying a preview and e2e testing
> something before deploying to production?"
>
> this extends phase 3 of docs/MULTIAGENT_PLAN.md from "decide a git
> strategy" into an actual design. everything here was checked against the
> real capabilities of src/agent/github.rs and src/agent/vercel.rs — nothing
> assumes infrastructure we do not have.

## 1. what stacked prs actually are, stripped of tooling

the idea predates the tools: graphite, git-spice (`gs`), stgit, and mercurial
quilts all manage the same object — a chain of commits where **each commit's
parent is the previous commit**, and each link becomes its own reviewable,
revertible pull request. you merge bottom-up; merging a link implicitly
carries everything beneath it. tooling automates the painful part (restacking:
re-parenting the whole chain when the base moves).

the reason the tools exist is that local git makes this awkward: branches,
rebases, force pushes, editor interruptions. but look at how WE make a
commit, in `Github::commit()`:

1. read head ref of the branch
2. read its tree
3. upload blobs
4. build a tree layered over base_tree
5. **create a commit with an explicit `"parents": [base.sha]`**
6. move the ref (non-force)

step 5 already names the parent explicitly. a stacked chain is nothing more
than choosing a different base — instead of always parenting onto the branch
head, parent onto *another agent's tip*. the model most harnesses find exotic
is nearly native here, because there is no working copy, no checkout, no
rebase — just commits and refs, which is exactly what the git data api
exposes. what the cli tools automate (restacking) maps to: rebuild the chain
with new parents and force-push the agent-owned refs. force-pushing
`agent/*` refs that no human ever tracks is safe; force-pushing `main` is
forbidden forever.

what graphite/git-spice give us conceptually — small units, independent
review, cheap revert, parallelism without a merge cliff — we adopt. the
binaries themselves require a local git clone and cannot run in a browser;
we re-implement the ~4 operations we need over REST (see §4).

## 2. the actual problem: parallel diffs

two agents editing concurrently, both diffing against `main`, cannot see each
other's intent. the conflict surfaces at merge time, when the context that
produced the edit is long gone. three mechanisms, cheapest first:

**C1 — path claims (early warning, pure opfs).** extend the opfs workspace
index with a claims map: `{path -> conversation id}`. before write_file /
edit_file, the workspace checks whether another live conversation claims the
path; if so, the tool returns a warning ("path also modified by thread X —
coordinate or expect a merge conflict") but proceeds. zero network cost,
catches overlap while both agents are still alive and can react. this is the
highest-value, lowest-cost piece.

**C2 — per-conversation branches + prs (real resolution).** each concurrent
conversation commits to `agent/{conversation-id}` instead of `main`. a PR
against main is opened when the agent (or user) says the work is done. github
computes the diff/conflict at PR time; conflicting PRs simply cannot both be
merged fast-forwarded — the second gets a clear failure, which is exactly the
loud, surfaced behavior D4 wants. no silent clobbering, ever.

**C3 — stacks for deliberate dependencies.** when a spawned agent (phase 4)
builds ON another agent's unmerged work, its branch bases on the parent's
branch tip, not on main. that chain IS a stacked-pr series: merge bottom-up.
this costs nothing beyond passing a different base ref at branch creation.

recommendation: C1 + C2 now, C3 falls out of the orchestrator design for
free. do NOT attempt automatic restacking in v1 — when a stack breaks, report
it and let the agent rebase explicitly with a dedicated tool.

## 3. preview deploys and e2e before production

current reality: pushing to main triggers vercel to build AND promote to
production. a failed compile leaves prod on the last good build (good), but a
build that compiles yet misbehaves ships straight to users (the gap). the fix
has three stages:

**P1 — previews already exist; surface them.** vercel builds every non-main
branch as a preview deployment automatically. `Vercel::deployment_for_commit`
already fetches any deployment by sha — it does not care that today we only
call it for main commits. adding `preview_url` to the flow is a reading
exercise, not a build exercise. caveat discovered during planning: vercel
*deployment protection* may wall previews behind auth; if the preview url
returns a protection interstitial instead of the app, protection must be
turned down for the project (or a bypass token used), otherwise nothing can
test against the preview — including humans.

**P2 — e2e on the preview via github actions.** the worker cannot drive a
browser (it IS a browser). the honest place for browser-driving tests is CI.
a `.github/workflows/e2e.yml` in our own repo — which we can commit like any
file — waits for the preview deployment, points playwright at it, asserts the
app boots: wasm initializes, Event::Ready arrives, the setup card renders,
settings persist across a reload. the workflow reports a check run on the
commit, and `Github::deployment_state` ALREADY READS CHECK RUNS. so:

    agent opens PR → preview deploys → actions runs e2e on the preview
    → check lands on the PR head → agent polls deployment_state()
    → merges ONLY on success

the loop is closed with components that all exist or are thin additions.

**P3 — production becomes promotion, not arrival.** once C2 is live, main
receives commits only through merged PRs whose checks were green. "deploy to
production" stops being "i pushed" and becomes "the gate passed". the OTA
mechanism (ui/update.rs) is unchanged — it just starts observing a calmer
main.

## 4. what has to be built, in order

all of it is additive to existing modules; nothing rewrites anything.

1. **github.rs primitives** (~thin REST wrappers):
   - `create_ref(branch, at_sha)` — POST /git/refs, for agent/* branches
   - `compare(base, head)` — GET /compare/{base}...{head}, the parallel-diff
     view (files changed between two refs)
   - `create_pr(head, base, title, body)` — POST /pulls
   - `merge_pr(number)` — PUT /pulls/{n}/merge (merge or squash)
   - `list_refs(prefix)` — enumerate agent/* branches
2. **tools.rs exposure**: `git_create_branch`, `open_pr`, `merge_pr`,
   `pr_status` (wraps deployment_state for the pr head), `diff_branches`.
   the agent gains the whole stacked workflow through the same dispatch table
   it already has.
3. **claim registry** (§2 C1) in the opfs workspace index.
4. **e2e workflow + branch protection**: commit `.github/workflows/e2e.yml`;
   set `e2e` as a required check on main. the agent's rule 2 ("never call
   task_complete with a red build") extends naturally: never merge with red
   checks.
5. **system prompt update** documenting the branch/pr tools and the new
   merge discipline — after the tools exist, not before.

## 5. decisions recorded

- stacked-pr MODEL adopted; stacked-pr TOOLING rejected (needs local git;
  we re-implement four REST calls instead).
- parallel diffs: opfs path claims for early warning + per-conversation
  branches/PRs for resolution; no automatic restacking in v1.
- production gating: preview deploy → actions-run e2e → required check →
  merge-to-main. main is promoted, never pushed blind.
- force-push permitted ONLY to agent/* refs; forbidden on main, forever.
