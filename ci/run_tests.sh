#!/usr/bin/env bash
# the shared verification gate — ONE definition of "this code may ship",
# consumed by BOTH callers that can put a commit in front of users:
#
#   * vercel build.sh (the deploy itself)
#   * github actions ci (.github/workflows/ci.yml, before anyone trusts it)
#
# this used to live only inside build.sh, which made the ~4-minute deploy
# the ONLY compiler feedback in the repo: every red build pinned production
# to the last good one the whole time. running these checks twice costs
# minutes; discovering a breakage only at deploy costs hours of a pinned
# main plus an email per failure.
#
# suite DISCOVERY is the load-bearing part. build.sh once enumerated six
# suites by hand while eight existed on disk (bench_grading and
# branch_policy were silently never gated) and nothing complained. here the
# filesystem is the list: cargo's autotest discovery makes every tests/*.rs
# a suite whether or not any script knows its name, so a new suite is gated
# from birth and a deleted one cannot leave a dangling entry behind.
#
# serialized + nocapture + backtrace: a parallel or captured run can kill
# the harness before any failing test prints, leaving a log that names
# nothing. one thread costs seconds; markers make every failure
# self-identifying even in a truncated log.
#
# inside github actions, every failure is ALSO mirrored into the job
# summary. job logs need admin rights to read through the api; the summary
# rides the public check-run payload, so an agent diagnosing its own red
# build sees actual compiler output instead of just "exit code 101".
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

SUMMARY=""
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  SUMMARY="${GITHUB_STEP_SUMMARY}"
fi

summarize_failure() { # $1 = what failed, $2 = log file with its output
  if [ -z "$SUMMARY" ]; then return 0; fi
  {
    echo "### FAILED: $1"
    echo '```'
    grep -E "^error(\[|:)|^warning|-->|panicked at|FAILED|Caused by|cannot find" "$2" | head -60 || true
    echo '```'
  } >> "$SUMMARY" || true
}

echo "--> [gate] unit tests (src/lib.rs)"
if ! cargo test --lib -- --test-threads=1 --nocapture 2>&1 | tee /tmp/gate-lib.log; then
  summarize_failure "src/lib.rs unit tests" /tmp/gate-lib.log
  echo ""
  echo "!! SUITE FAILED: src/lib.rs unit tests — full output above"
  exit 1
fi

shopt -s nullglob
suites=(tests/*.rs)
if [ "${#suites[@]}" -eq 0 ]; then
  echo "!! no integration suites found in tests/ — the gate found nothing to run"
  exit 1
fi

for f in "${suites[@]}"; do
  suite="$(basename "$f" .rs)"
  echo "--> [gate] suite: $suite"
  if ! RUST_BACKTRACE=1 cargo test --test "$suite" -- --test-threads=1 --nocapture 2>&1 | tee "/tmp/gate-$suite.log"; then
    summarize_failure "$suite" "/tmp/gate-$suite.log"
    echo ""
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
    echo "!! NATIVE TESTS FAILED: suite '$suite'                       !!"
    echo "!! A failing test means broken logic shipped to production.  !!"
    echo "!! An uncompilable suite means the verification layer itself !!"
    echo "!! is broken and must be fixed before the next commit.       !!"
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
    exit 1
  fi
done

echo "--> [gate] clippy (warnings are fatal)"
# lints run AFTER tests so a logic failure still reports first. a lint gate
# that only warns is a lint nobody reads; failing the gate on warnings is
# how the gate stays real. the minimal rustup profile omits clippy, so it
# installs itself when absent.
if ! cargo clippy --version >/dev/null 2>&1; then
  rustup component add clippy
fi
if ! cargo clippy --lib --tests -- --deny warnings 2>&1 | tee /tmp/gate-clippy.log; then
  summarize_failure "clippy" /tmp/gate-clippy.log
  echo ""
  echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
  echo "!! CLIPPY FAILED: fix the warnings above and re-commit   !!"
  echo "!! A warning gate that only warns is a gate nobody reads. !!"
  echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
  exit 1
fi

echo "--> [gate] PASSED: unit tests + $((${#suites[@]})) suites + clippy"
