# deadmkt-node

Rust trading node for the deadmkt decentralized batch-auction protocol on Supra Testnet.

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

- `src/run.rs` -- main event loop, phase handlers, all subsystem wiring (~2300 lines)
- `src/token_worker.rs` -- background Mint/Burn/Lock/Unlock handler
- `crates/strategy/src/lib.rs` -- event/action types, BatchStartData struct, JSON serialization
- `crates/strategy/src/server.rs` -- WebSocket server for strategy connections
- `crates/settlement/src/lib.rs` -- SettlementSubmitter (4A) + SettlementManager (4B)
- `crates/gossip/src/network.rs` -- libp2p gossipsub node
- `crates/matching/src/lib.rs` -- deterministic price-time priority matching engine
- `crates/escrow_tracker/src/lib.rs` -- in-memory balance tracking (projected vs confirmed)

## Contract

Address (DMKT12): `0x9b8fd778b08131297d22b577c1f4e2f6ed85d04479cd73e0eb14bbf41fc6731c`
Chain: Supra Testnet (chain_id: 6)
5 modules: nft, escrow, settlement, pool_config, tokens

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
