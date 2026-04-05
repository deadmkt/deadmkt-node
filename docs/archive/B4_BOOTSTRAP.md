# B4 Bootstrap — Settlement + Balance Tracking

## Instructions
You are continuing Build 4 of deadmkt-node. B1 (Move contracts, 146 tests), B2 (Rust foundation, 96 tests), B3 (gossip + matching, 80 tests) are complete. 322 total tests (146 Move + 176 Rust, 5 ignored testnet-only). All passing.

BUILD_PLAN.md is attached separately — it has B4 scope (phases 4A-4F).

Your approach: TDD. Write failing tests first, then implement. Same pattern as B2/B3.

## Workspace Structure
```
crates/
  batch_state/src/lib.rs       — 12 tests, batch phase state machine
  chain/src/{lib,client,types,batch,events,poller,pubkey,market}.rs — 41 tests
  config/src/lib.rs             — 9 tests, NodeConfig struct
  crypto/src/lib.rs             — 16 tests (+3 interop ignored), BCS/Ed25519
  escrow_tracker/src/lib.rs     — SCAFFOLD ONLY (B4 target)
  gossip/src/{lib,messages,network}.rs — 13 tests, gossipsub
  gossip_validation/src/lib.rs  — 16 tests, commit/reveal validation
  integration_tests/src/lib.rs  — 5 tests, cross-crate pipeline
  keystore/src/lib.rs           — 9 tests, encrypted key storage
  matching/src/lib.rs           — 17 tests, deterministic matching
  setup/src/lib.rs              — 27 tests (+3 testnet ignored), wizard
  storage/src/lib.rs            — 10 tests, SQLite persistence
```

## Key Types

### Order (crypto crate)
```rust
pub struct Order {
    pub nft_id: u64,
    pub symbol: Vec<u8>,
    pub side: u8,        // 0=sell, 1=buy
    pub price: u64,      // 8 decimal fixed point
    pub quantity: u64,   // base token smallest unit
    pub batch_id: u64,
    pub nonce: Vec<u8>,  // 32 bytes
}
```

### Match (matching crate)
```rust
pub struct Match {
    pub batch_id: u64,
    pub pool_id: u64,
    pub symbol: Vec<u8>,
    pub buyer: RevealedOrder,
    pub seller: RevealedOrder,
    pub fill_quantity: u64,
    pub settlement_price: u64,   // (buyer_price + seller_price) / 2
    pub match_hash: [u8; 32],    // SHA256(batch_id || buyer_hash || seller_hash || fill_qty)
    pub gas_payer_nft_id: u64,   // lower order_hash pays
}
```

### BatchTradeSettledEvent (chain crate)
```rust
pub struct BatchTradeSettledEvent {
    pub batch_id: u64,
    pub pool_id: u64,
    pub trade_id: String,
    pub buyer_nft_id: u64,
    pub seller_nft_id: u64,
    pub symbol: Vec<u8>,
    pub clearing_price: u64,
    pub base_amount: u64,
    pub quote_amount: u64,
    pub gas_payer: String,
    pub timestamp: u64,
}
```

### ChainEvent enum
```rust
pub enum ChainEvent {
    BatchTradeSettled(BatchTradeSettledEvent),
    MarketPairAdded { symbol: Vec<u8> },
    PauseQueued { after_batch: u64 },
    Unpaused,
    NftPairMinted { nft_id: u64 },
    MintBondReclaimed { nft_id: u64 },
    Unknown(String),
}
```

### NodeConfig (config crate)
```rust
pub struct NodeConfig {
    pub network: Network,
    pub rpc_urls: Vec<String>,
    pub nft_id: u64,
    pub trustee_address: String,
    pub beneficiary_address: String,
    pub sponsor_address: String,
    pub contracts: ContractAddresses,
    pub bootstrap_peers: Vec<String>,
    pub strategy_auth_token: String,
    pub markets: Vec<String>,
    pub token_decimals: HashMap<String, u8>,
    pub profit_taking: ProfitConfig,
    pub withdrawal_rules: WithdrawalConfig,
    pub created_at: String,
}
pub struct ProfitConfig {
    pub base_capital: HashMap<String, u64>,
    pub threshold_pct: u32,
    pub transfer_mode: String,
    pub transfer_mode_value: Option<u64>,
}
```

### SupraClient (chain crate)
```rust
pub fn new(endpoints: Vec<String>, contract_addr: String) -> Self
pub async fn view_raw(module, function, type_args, args) -> Result<Value>
pub async fn get_ledger_info() -> Result<LedgerInfo>
pub async fn submit_raw(payload: &[u8]) -> Result<String>  // returns tx hash
pub async fn simulate_raw(payload: &[u8]) -> Result<SimulateResult>
pub async fn wait_for_tx(hash, timeout_ms, poll_ms) -> Result<TxResult>
pub async fn get_events(address, creation_number, start, limit) -> Result<Vec<ChainEvent>>
pub async fn get_trustee_pubkey(nft_id) -> Result<Vec<u8>>
pub async fn get_market_pair_config(symbol) -> Result<MarketPair>
```

### Storage (existing tables)
```
schema_version, pubkeys, batches, commits, pending, reputation,
registered_nfts, blocked_nfts, market_pairs
```
Key APIs: `cache_pubkey`, `get_pubkey`, `insert_batch`, `get_batch`, `insert_pending`, `get_pending_settlements`, `add_registered_nft`, `is_registered_nft`, `add_blocked_nft`, `is_blocked_nft`, `upsert_market_pair`, `get_market_pair`, `list_active_market_pairs`

### Crypto sign functions
```rust
pub fn sign_order(secret, order) -> Vec<u8>          // Ed25519 over BCS(order)
pub fn sign_commit(secret, batch_id, pool_id, hash) -> Vec<u8>
pub fn compute_match_hash(batch_id, buyer_hash, seller_hash, fill_qty) -> [u8; 32]
pub fn sign_batch_complete(secret, batch_id, pool_id, symbol, price, volume, count) -> Vec<u8>
pub fn sign_settlement_report(secret, batch_id, match_hash, settled, bailer_nft) -> Vec<u8>
```

### Escrow Tracker (scaffold — B4 builds this)
```rust
// deadmkt-escrow-tracker: Projected balance tracking.
// Build 4 implementation. Scaffolded now for workspace structure.
// Will track: projected balance, pending_out, pending_in, earmarked.
```

## Workspace Dependencies (Cargo.toml)
```toml
tokio = { version = "1", features = ["full"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
ed25519-dalek = { version = "2", features = ["rand_core"] }
sha2 = "0.10"
bcs = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
rusqlite = { version = "0.31", features = ["bundled"] }
hex = "0.4"
thiserror = "2"
```

## Settlement Contract Interface (from spec)
```
settle_match(
    submitter: &signer,
    batch_id, pool_id,
    buyer_nft_id, buyer_side, buyer_price, buyer_quantity, buyer_batch_id, buyer_nonce, buyer_signature,
    seller_nft_id, seller_side, seller_price, seller_quantity, seller_batch_id, seller_nonce, seller_signature,
    symbol, fill_quantity
)
```
- Contract computes settlement_price = (buyer_price + seller_price) / 2 on-chain
- Emits BatchTradeSettled event
- Abort codes: E_INSUFFICIENT_BALANCE, E_ALREADY_SETTLED, E_BUYER_INACTIVE, E_SELLER_INACTIVE, E_BUYER_OVERFILL, E_SELLER_OVERFILL

## B4 Phases (from BUILD_PLAN.md)
- **4A**: settlement.rs — build settle_match tx payload, submit to Supra REST
- **4B**: settlement.rs — confirmation poller, abort code detection, backstop submission
- **4C**: escrow_tracker.rs — projected balance (confirmed - pending_out - earmarked)
- **4D**: withdrawal.rs — event handling (WithdrawalRequested, Executed, HoldingPeriod)
- **4E**: profit.rs — auto profit transfer to beneficiary
- **4F**: storage.rs — SQLite persistence extensions (settlement history, balance snapshots)

Begin with B4 TDD spec + phase planning.
