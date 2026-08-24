#!/usr/bin/env bash
# vercel build: compile the rust harness to wasm and emit a static site.
#
# vercel's build image has no rust toolchain, so it is installed here. that
# is deliberate rather than committing web/pkg to git: this repository edits
# ITSELF, so if the wasm were a checked-in artifact the agent could change
# its own source and the deployed binary would never reflect it.
set -euo pipefail

WASM_PACK_VERSION="0.13.1"

# keep toolchain state inside the project so nothing depends on $HOME layout
export RUSTUP_HOME="${PWD}/.rustup"
export CARGO_HOME="${PWD}/.cargo-home"
export PATH="${CARGO_HOME}/bin:${PWD}/.bin:${PATH}"

echo "--> installing rust (minimal, wasm32 target only)"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain stable \
             --target wasm32-unknown-unknown

echo "--> rust: $(rustc --version)"

# a prebuilt binary rather than `cargo install wasm-pack`, which would
# compile it from source and add several minutes to every deploy.
echo "--> installing wasm-pack ${WASM_PACK_VERSION}"
mkdir -p "${PWD}/.bin"
if [ ! -x "${PWD}/.bin/wasm-pack" ]; then
  TARBALL="wasm-pack-v${WASM_PACK_VERSION}-x86_64-unknown-linux-musl"
  curl -sSfL \
    "https://github.com/rustwasm/wasm-pack/releases/download/v${WASM_PACK_VERSION}/${TARBALL}.tar.gz" \
    -o /tmp/wasm-pack.tar.gz
  tar -xzf /tmp/wasm-pack.tar.gz -C /tmp
  cp "/tmp/${TARBALL}/wasm-pack" "${PWD}/.bin/wasm-pack"
  chmod +x "${PWD}/.bin/wasm-pack"
fi
echo "--> wasm-pack: $(wasm-pack --version)"

echo "--> building wasm"
wasm-pack build --target web --out-dir web/pkg --no-typescript

# fail loudly rather than shipping an index.html that will 404 on its module
test -f web/pkg/vanish_bg.wasm || { echo "build produced no wasm"; exit 1; }
test -f web/pkg/vanish.js      || { echo "build produced no js glue"; exit 1; }

# the verification layer: a deploy must not only compile, it must pass the
# contract tests — native suite + clippy, warnings fatal. the definition
# lives in ci/run_tests.sh, SHARED with github actions ci so the two gates
# cannot drift: ci runs the identical script before vercel ever sees a
# commit, which is how compile errors became a red check on the commit
# instead of a failed production deploy discovered four minutes later.
#
# placed AFTER the wasm build so a compile error still reports fast.
echo "--> running verification gate (tests + clippy)"
bash ./ci/run_tests.sh


echo "--> output:"
ls -la web/pkg/
