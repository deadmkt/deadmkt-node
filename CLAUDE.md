# deadmkt-node

Rust trading node for the deadmkt decentralized batch-auction protocol on Supra Testnet.

## Operator-facing surface (post-MR1..MR7)

Three entry points, in order of how an operator typically encounters them:

- **One-liner installer:** `curl -sSL https://get.deadmkt.com | bash` runs `install.sh` (source build, `--restart=unless-stopped`, idempotent). Backs the v1 SetupResult JSON for failure cases.
- **CLI:** `deadmkt-node {setup,status,escrow,reactivate,deposit,burn,withdraw,agent-config,apply-params,update-params}`. The action commands (`burn`, `withdraw <sub>`, `agent-config`) support `--json` to emit a v1 action-result envelope and `--password-stdin` for scripting (MR3). `status --json` emits the v1 status payload via the SP8 endpoint at `127.0.0.1:9292` with a `node_running:false` fallback (MR2).
- **Operator playbook:** `deadmkt.com/playbook` (MR5) is the AI-paste page; operators feed it to any chat AI for guided setup / monitoring / withdrawal.

The interactive `deadmkt-node setup` wizard uses dialoguer + indicatif (no-echo password, validated inputs, spinners on slow ops) since MR7. The non-interactive `--config setup.json` path (MR1a/b/c) drives the same orchestrator with a `SilentIO` adapter and emits the v1 SetupResult JSON.

## Build & Test

```bash
cargo build --release
cargo test
```

## Rules

- No crate interface breaks without explicit approval
- No non-ASCII characters in .move files (applies to companion contract repo)
- Tag before and after significant changes
- Each save point must be independently deployable
- Strategy crate has zero external dependencies by design -- keep it that way

## Architecture

- 17 crates in `crates/`, main wiring in `src/run.rs`
- Gossip swarm runs in a dedicated tokio::spawn task (SwarmCommand channel for outbound, mpsc for inbound)
- Main loop is a `tokio::select!` with two arms: gossip drain + chain poll
- Settlement worker runs in a separate tokio::spawn with tx_lock shared with token_worker
- Strategy communicates over WebSocket (bridge.py on Python side)

## Key Files

- `install.sh` -- MR4a one-liner installer (source build path).
- `src/main.rs` -- CLI entrypoints (status / withdraw / burn / agent-config handlers; `mr3_load_signed_chain` shared bring-up; ActionResultBuilder envelope; non-interactive setup / restore routers).
- `src/cli.rs` -- clap `Command` enum + WithdrawAction / BurnTarget subcommands.
- `src/run.rs` -- main event loop, phase handlers, all subsystem wiring (~3500 lines). MR2 v1 status atomics + `build_status_v1_json`/`build_status_v1_disk_only_json` live here.
- `src/setup_bridge.rs` -- `StdIO` (interactive `WizardIO` impl using dialoguer + indicatif) + `SupraSetupClient` (concrete `ChainClient`).
- `src/token_worker.rs` -- background Mint/Burn/Lock/Unlock handler.
- `crates/setup/src/lib.rs` -- `WizardIO` trait (typed prompts + `SpinnerHandle`), `ChainClient` trait, wizard orchestrator, `SetupConfig`/`SetupResult` schemas.
- `crates/setup/src/noninteractive.rs` -- MR1a/b/c non-interactive entrypoints + `SilentIO`.
- `crates/strategy/src/lib.rs` -- event/action types, BatchStartData struct, JSON serialization.
- `crates/strategy/src/server.rs` -- WebSocket server for strategy connections.
- `crates/settlement/src/lib.rs` -- SettlementSubmitter (4A) + SettlementManager (4B).
- `crates/gossip/src/network.rs` -- libp2p gossipsub node.
- `crates/matching/src/lib.rs` -- deterministic price-time priority matching engine.
- `crates/escrow_tracker/src/lib.rs` -- in-memory balance tracking (projected vs confirmed).
- `examples/` -- setup-config*.json (MR1), status-output.json (MR2), action-result-output.json (MR3).

## Contract

Address (DMKT14, deploy target): `0x79b2ad6fea72a9aed2ea4bb7ded31c2741d52d7777b92fbc83dd80a7862094a9`
Chain: Supra Testnet (chain_id: 6)
7 modules: nft, escrow, settlement, pool_config, tokens, exits, ops_treasury

> Note: DMKT14 (the `dmkt14/remove-beneficiary` revision -- beneficiary NFT removed, off-chain `payout_address`, single-call burn) is the reserved deploy target this build defaults to; **not yet published on-chain**. DMKT12 (`0x9b8fd778...731c`) remains the live deploy until DMKT14 publishes -- so against live testnet, override with `DEADMKT_CONTRACT_ADDR=0x9b8fd778...731c`. The DMKT13 target (`0x7bbf47b7...a55f`) was published but never cut over; that cutover is abandoned. Deploy checklist: `planning/current/DMKT14_POST_DEPLOY.md`.

## Reference Docs

### Supra (chain layer)
- Supra docs home: https://docs.supra.com/
- Move on Supra overview: https://docs.supra.com/network/move
- Move Book (Supra edition): https://docs.supra.com/network/move/move-book
- Ethereum to Supra Move cheatsheet: https://docs.supra.com/network/move/ethereum-to-supra-move-cheatsheet
- Fungible Asset (FA) module: https://docs.supra.com/network/move/supra-fungible-asset-fa-module
- REST API -- testnet accounts: https://docs.supra.com/network/move/rest-api/testnet/accounts
- REST API -- testnet transactions: https://docs.supra.com/network/move/rest-api/testnet/transactions
- REST API -- testnet view functions: https://docs.supra.com/network/move/rest-api/testnet/view
- REST API -- testnet events: https://docs.supra.com/network/move/rest-api/testnet/events
- REST API -- testnet faucet: https://docs.supra.com/network/move/rest-api/testnet/faucet
- Network info (RPC URLs, chain IDs): https://docs.supra.com/network-information
- dVRF (randomness): https://docs.supra.com/network/oracle/dvrf
- Supra framework source: https://github.com/Entropy-Foundation/aptos-core/tree/dev/aptos-move/framework/supra-framework

### Rust crate docs (gossip + crypto)
- libp2p gossipsub: https://docs.rs/libp2p-gossipsub/latest
- libp2p (full): https://docs.rs/libp2p/latest/libp2p/
- ed25519-dalek: https://docs.rs/ed25519-dalek/latest
- tokio (async runtime): https://docs.rs/tokio/latest/tokio/
- BCS serialization: https://docs.rs/bcs/latest

### Project
- Documentation: https://deadmkt.com/docs/
- Contract API reference: https://deadmkt.com/docs/guides/websocket-api
