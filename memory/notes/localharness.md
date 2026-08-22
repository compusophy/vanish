# notes: compusophy/localharness

> external repo researched via GitHub API on 2026-08-14. facts as of
> version 0.82.0. refresh before relying on specifics.

## what it is

"agents that own themselves." one Rust crate (Apache-2.0, stable 1.85+)
that is both an agent SDK (`cargo add localharness`) and a self-sovereign,
browser-resident agent platform. every agent is an ERC-721 NFT on Tempo
mainnet (chain 4217) with its own wallet (secp256k1 + BIP-39), persona,
OPFS filesystem, and subdomain `<name>.localharness.xyz`. EIP-2535
Diamond + EIP-6551 token-bound account. agents pay each other in `$LH`
credit per call, settled over x402 v2.

## architecture

- three SDK layers behind one seam: L1 `Agent` facade → L2
  `Conversation`/`ChatResponse` → L3 `Connection`/`ConnectionStrategy`
  transport. backends: Gemini, Anthropic, OpenAI, Mock, opt-in in-browser
  Gemma (~570MB, WebGPU, `local` feature).
- native + wasm32 from one source; `--features browser-app` mounts the
  in-browser IDE. `--features wallet` pulls the on-chain `registry::`
  surface.
- three embedded DSLs, all pure Rust: `rustlite` (Rust-subset → wasm
  cartridges), `bashlite` (fuel-bounded shell over rooted fs),
  `soliditylite` (Solidity-subset → EVM bytecode).
- no server in the middle: browser (OPFS) or user binary holds keys;
  off-chain pieces are only the credit proxy (metered inference) and the
  sponsor relay (gas).

## notable surfaces

- CLI: `onboard --invite`, `create <name>` (1 $LH claim), `compile`
  (free offline dry-run of app.rl), `publish` (rustlite cartridge = the
  agent's public face, free/off-chain), `persona`, `call [--as]`,
  `acp` (Agent Client Protocol server for Zed/JetBrains), `mcp` (stdio
  MCP server exposing call_agent to any MCP client).
- networked MCP endpoint on the proxy: discover_agents + list_bounties
  are free; ask_agent is true x402 pay-per-call with settle-on-success.
- scheduled jobs: `schedule` / `goal` ("ralph" goal-loops that end
  themselves via finish_goal when verifiably complete) / `notify`
  (cross-agent Web Push).
- coordination ladder rung 1: bounty board (escrowed $LH rewards);
  party→guild→DAO later. session rooms = encrypted on-chain kv state.
- invites escrow $LH behind bearer codes, supply-neutral.

## trust model (their own words)

no KMS/HSM/TEE/attestation anywhere; keys are plaintext files
(`~/.localharness/keys/<name>.key`, or OPFS seed in browser). security
boundary is the host device. refreshing honesty about it.

## relevance to vanish

closest existing cousin to this project: also one Rust crate, wasm32 in
browser, OPFS working tree, no backend. differences worth studying:
they added a wallet/on-chain identity layer and an agent-to-agent
payment protocol; their llms.txt is generated from src/docs_manifest.rs
with a drift gate in cargo test — a pattern worth copying if we ever
publish our own agent-facing docs.
