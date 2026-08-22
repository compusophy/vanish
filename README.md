# vanish

An autonomous, self-editing coding agent that runs **entirely in your
browser**. One Rust crate compiled to WebAssembly. No server, no serverless
functions, no runtime dependencies.

The agent edits this repository — its own source — and commits the result to
GitHub.

## How it works

The same wasm binary is loaded twice:

```
browser tab
├── main thread   boot_ui()      src/ui/       DOM only
│        ▲ Event          │ Command            typed by src/protocol.rs
└── web worker    boot_worker()  src/worker.rs
                  ├── src/agent/mod.rs      the loop — no deadline
                  ├── src/agent/llm.rs      OpenRouter, streamed
                  ├── src/agent/github.rs   blobs → tree → commit → ref
                  ├── src/agent/tools.rs    the tool surface
                  └── src/platform/opfs.rs  the working tree, on disk
```

The loop runs in a Web Worker, so it has no request bounding it and no
execution deadline. The working tree lives in the Origin Private File
System, so a write is durable the moment it happens — it survives the run, a
reload, a crash, and a closed tab.

Both halves compile against `src/protocol.rs`, so a UI/logic mismatch is a
build error rather than a blank page.

See [ARCHITECTURE.md](ARCHITECTURE.md) for why the previous serverless design
failed and how each of its failure modes is now structurally impossible.

## Tools

| tool | effect |
| --- | --- |
| `read_file` | read from the working tree, falling back to GitHub on first touch |
| `write_file` | create or overwrite; durable immediately |
| `edit_file` | exact substring replacement; refuses ambiguous matches |
| `list_dir` | list the branch, flagging locally modified files |
| `git_status` | what differs from the last synced blob |
| `git_commit` | every modified file as one atomic commit |
| `sync_repo` | refresh the branch listing |
| `task_complete` | declare the work finished |

There is no `run_command`. Nothing executes shell commands in a browser.

## Running it

```sh
wasm-pack build --target web --out-dir web/pkg --no-typescript
cargo run --features devserver --bin serve      # http://localhost:8787
```

`src/bin/serve.rs` is std-only Rust and exists only because wasm modules and
workers need correct MIME types over HTTP. It is never deployed — `web/` is
static files.

## Credentials

There is no backend, so there is no sign-in. GitHub OAuth cannot work here:
the code-for-token exchange needs a client secret, and a secret shipped to a
browser is not secret. Instead, in the settings panel:

- **OpenRouter API key** — from openrouter.ai/keys
- **GitHub token** — a fine-grained PAT scoped to this repository with
  `Contents: read and write`

Both are stored in your browser's `localStorage`, never in the deployed
bundle. Saving verifies each against the real service and reports which half
failed.

## Deployment

Vercel hosts `web/` as static files and compiles the Rust at build time
(`build.sh`). The wasm is deliberately **not** committed: this repository
edits itself, so a checked-in artifact would never reflect the agent's own
changes.

The running build is stamped with its commit. The UI polls the branch head
and, when a newer build exists, shows the changelog and reloads itself —
deferring while a run is in flight.

## Conventions

Write code in the casing correct for its language. There is no case policy.
An earlier version enforced lowercase globally, which corrupted every
identifier the agent generated; see `D6` in
[memory/TASKBOARD.md](memory/TASKBOARD.md).
