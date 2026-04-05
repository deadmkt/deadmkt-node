# Build Plan

Six builds. Each delivers something testable. Each unlocks the next.

## Build Status

| Build | Status | Completion Date | Tests | Notes |
|-------|--------|-----------------|-------|-------|
| 1 | ✅ COMPLETE | 2026-02-17 | 146 Move | Deployed to testnet. 86/86 spec coverage. R50–R63 refinements applied. |
| 2 | ✅ COMPLETE | 2026-02-21 | 96 Rust | T_CRYPTO_08 Phase A proven: Rust Ed25519 ↔ Move interop confirmed. |
| 3 | ✅ COMPLETE | 2026-02-22 | 80 Rust | 6 phases. Matching, gossip, validation, batch state, chain poller, integration. |
| 4 | ✅ COMPLETE | 2026-02-22 | 84 Rust | 6 phases. Settlement, escrow, withdrawal, profit, storage, integration. Scope-audited. |
| 5 | ⚠️ PARTIAL | 2026-02-23 | 81 Rust + 4 Python | Setup wizard works on testnet. Run loop unwired. See TESTNET_FINDINGS.md (R64–R70). |
| 5.5 | ⬜ NOT STARTED | — | — | Testnet wiring: fix chain client, wire run loop, end-to-end batch cycle. |
| 6 | ⬜ NOT STARTED | — | — | Batch Data API. Depends on Build 5.5. |

**Rust workspace total: 295 tests passing, 5 ignored (testnet-only), 0 failures.**
**Python total: 4 tests (starter bot).**
**Move total: 146 tests.**
**Grand total: 445 tests across all builds.**

---

## Dependency Graph

```
BUILD 1              BUILD 2              BUILD 3
Contracts            Rust Foundation      Gossip + Matching
(Move)               (Rust)               (Rust)
✅ COMPLETE          ✅ COMPLETE          ✅ COMPLETE
│                    │                    │
│ deploys to         │ reads chain        │ uses gossip
│ testnet            │ state              │ + matching
│                    │                    │
└──────┬─────────────┘                    │
       │                                  │
       ▼                                  │
BUILD 4 ◄─────────────────────────────────┘
Settlement + Balance
(Rust) ✅ COMPLETE
│
│ full batch cycle works
│
▼
BUILD 5
Strategy + Docker
(Rust + Python) ✅ COMPLETE ← YOU ARE HERE
│
│ product shippable
│
▼
BUILD 6
Batch Data API
(Rust, closed)
```

---

## Critical Path Update

All five implementation builds are complete. The remaining critical path is:

**B5.0.1 testnet wiring → B6 (Batch Data API)**

Setup wizard is proven on live testnet (mint_pair + register_trader succeed).
Three critical Supra-vs-Aptos bugs found and documented (R64–R70).
The chain client and settlement submitter still carry the same bugs — must be
fixed before the run loop can go live. See TESTNET_FINDINGS.md.

---

## BUILD 1: Contracts (Move) ✅ COMPLETE

**Goal:** All four contract modules compiled, deployed to testnet, post-deployment checklist passing.

**Result:** 146 unit tests passing. All modules compiled as unified deadmkt package. Specs, tests, and implementation aligned through R63. Deployed to testnet at `0xb38e179b2822331922fbe4a65c738d74f7441dee989375deeb336b9df388522c`.

**Post-completion refinements (R50–R63):**
- R50: Commit quota enforcement with Ed25519 evidence reporting
- R51: Burn cooldown (deregistered_at) preventing premature NFT destruction
- R52: Settlement grace period (enforcement_at) with withdrawal freeze
- R54: Staleness fix for edge cases
- R57: Enforcement delay changed from wall-clock to batch count
- R58: Escrow fund-safety guard on burn_pair (active_escrows tracking)
- R59: Lazy parameter application for concurrency safety
- R63: Event field enrichment (buyer_price, seller_price, order_hashes)

### 1A–1G: All phases ✅

**Build 1 done = ✅ contracts compiled, 146 tests passing, deployed, alignment audited through R63.**

---

## BUILD 2: Rust Foundation ✅ COMPLETE

**Goal:** A Rust binary that can read chain state, sign transactions, manage keys, and run the setup wizard against live testnet contracts.

**Result:** 96 tests passing across 7 crates. T_CRYPTO_08 Phase A proven — Rust Ed25519 pubkey stored on-chain, byte-identical retrieval confirmed.

| Crate | Tests | What it does |
|-------|-------|-------------|
| crypto | 16 + 3 interop | BCS encoding, Ed25519 sign/verify, order hashing, commit signing |
| chain | 31 | Supra REST client, view functions, tx submission, batch params |
| keystore | 9 | AES-256-GCM encryption, Argon2id KDF, three unlock modes |
| config | 9 | config.json load/save, validation, named volume structure |
| storage | 10 | SQLite tables (registered_nfts, blocked_nfts, market_pairs) |
| setup | 27 + 3 ignored | 9-step first-boot wizard, funding wait, NFT mint, escrow deposit |

### 2A–2E: All phases ✅

**Build 2 done = ✅ Rust binary connects to testnet, signs transactions, sets up trading identity. 96 tests passing.**

---

## BUILD 3: Gossip + Matching ✅ COMPLETE

**Goal:** Multiple nodes exchanging commits and reveals over libp2p, computing deterministic matches.

**Result:** 80 tests passing across 6 crates. Deterministic matching proven (CD-1). Pool topic sharding proven (CD-3).

| Crate | Tests | What it does |
|-------|-------|-------------|
| matching | 17 | Deterministic order matching, partial fills, gas payer selection |
| gossip (messages) | 5 | GossipMessage enum, bincode serialization |
| gossip (network) | 8 | libp2p gossipsub swarm, topic management |
| gossip_validation | 16 | Commit/reveal/batch validation, quota tracking, evidence |
| batch_state | 12 | Per-batch lifecycle, phase transitions, matching integration |
| chain (extensions) | 10 | Block poller, PubkeyCache, MarketConfigCache |
| integration_tests | 5 | Cross-crate batch cycle integration |

**Workspace constraint:** gossip/batch_state/gossip_validation/integration_tests crates excluded from workspace during B4/B5 dev (libp2p requires Rust 1.83+, container has 1.75). Crates compile and test independently. Re-integration tracked separately.

### 3A–3E: All phases ✅

**Build 3 done = ✅ gossip + matching working. 80 tests passing.**

---

## BUILD 4: Settlement + Balance ✅ COMPLETE

**Goal:** Full settlement pipeline: match → submit settle_match tx → handle abort/confirm → track escrow projected balance → withdrawal handling → profit transfer.

**Result:** 84 new tests across 6 crates + extensions. Settlement manager with abort code handling (15 variants). EscrowTracker with projected balance. Withdrawal lifecycle (7-state FSM). Profit transfer with safety guards. Scope-audited against all 4 scope documents.

| Crate | Tests | What it does |
|-------|-------|-------------|
| settlement | 25 | SettlementManager, SettlementSubmitter, abort handling, backstop, isolation detection |
| escrow_tracker | 24 | Projected balance, outflow/inflow, earmarks, multi-token, crash recovery |
| withdrawal | 7 | 7-state FSM (Idle→Requested→HoldActive→...→Completed), activity guards |
| profit | 8 | Threshold check, 3 transfer modes, safety guards, bond reclaim |
| chain (extended) | +8 | BatchTradeSettledEvent (14 fields), 9 new event variants |
| storage (extended) | +7 | settlements + balance_snapshots tables |
| b4_integration | 5 | Full settle loop, crash recovery, backstop, profit trigger |

| Phase | Scope | Tests |
|-------|-------|-------|
| 1 | EscrowTracker (projected balance, outflow/inflow/earmark) | 24 |
| 2 | SettlementManager (register, confirm, abort handling) | 17 |
| 3 | SettlementSubmitter (tx construction, gas, retry) | 8 |
| 4 | Withdrawal (7-state FSM, activity guard, hold expiry) | 7 |
| 5 | Profit (threshold, modes, safety, bond reclaim) | 8 |
| 6 | Storage + integration (persistence, crash recovery) | 12 |

**Scope audit (post-build):**
- Gap 1 (patched): BatchTradeSettledEvent extended to 14 fields (added buyer_price, seller_price, buyer_order_hash, seller_order_hash)
- Gap 2 (patched): AbortCode enum updated from 10 to 15 variants in spec
- Gaps 4–5 (deferred): E_ALREADY_SETTLED 3-block fallback, profit retry logic → testnet hardening
- Gaps 6–8 (minor): Test coverage for E_NFT_BURNED own path, E_MARKET_NOT_ACTIVE, profit→tracker update

**Build 4 done = ✅ full settlement pipeline, projected balance, withdrawal handling, profit transfer. Scope-audited.**

---

## BUILD 5: Strategy + Docker ⚠️ PARTIAL

**Goal:** Ship the product. `docker run` gets you a trading bot on testnet. Strategy connects via WebSocket, node runs the full batch cycle autonomously, starter bot works out of the box.

**Status:** Subsystems built and unit-tested. Setup wizard proven on live testnet (NFT mint + trader registration succeed). **Run loop is a stub** — `Command::Run` loads config but doesn't start subsystems. Chain client and settlement submitter carry Aptos-era conventions that will fail on Supra (see TESTNET_FINDINGS.md R64–R70).

| Crate | Tests | What it does |
|-------|-------|-------------|
| strategy | 33 | WS server (auth, timeout, single conn), events (24 variants), actions, order conversion + validation |
| node_state | 13 | State machine (Setup→Connected→Observing→Trading⇄Paused/Limited→Shutdown) |
| gas_manager | 10 | Two-tier thresholds (Normal/Low/Critical), bond reclaim check |
| orchestrator | 13 | Batch loop coordinator, phase handlers, chain event routing, governance |
| chain (extended) | +5 | 5 governance ChainEvent variants (ParameterChange*, PoolAdjust*) |
| binary CLI | 6 | clap subcommands: run/setup/status/escrow/show-token/version |
| b5_integration | 1 | Full cycle: WS auth → batch_start → commit → reveal → match → settle → notify |
| starter_bot (Python) | 4 | Spread strategy, reveal-all, balance-aware quantity sizing |

| Phase | Scope | Tests |
|-------|-------|-------|
| 1 | Strategy types + conversion (events, actions, decimal→fixed, validation) | 23 |
| 2 | Strategy WS server (auth, timeout, single conn, event dispatch) | 10 |
| 3 | Node state machine (7 states, event-driven transitions, guards) | 13 |
| 4 | Gas manager (two-tier thresholds, bond reclaim) | 10 |
| 5 | Chain governance events + orchestrator (phase handlers, routing) | 18 |
| 6 | CLI + starter bot + integration + Docker | 11 + 4 Python |

**Critical design decisions implemented:**
- CD-13: Strategy switches only at batch boundaries
- CD-14: WebSocket auth first message, 5-second deadline
- CD-15: Single active strategy connection (second replaces first)
- CD-16: Timeout = skip commit; reveal all as safe default
- CD-21: Strategy→canonical conversion (price×1e8, qty×10^decimals)
- CD-22: Order validation (pair active, price>0, qty≥min, balance sufficient)
- Risk #9: No floating point in price conversion (string→integer parsing)

**Non-Rust deliverables:**
- `starter_bot/strategy.py` — Spread bot using Decimal arithmetic
- `strategy_wrapper/bridge.py` — WS bridge with hot reload
- `Dockerfile` — Multi-stage (Rust builder + Python runtime)
- `entrypoint.sh` — Process manager (node + optional wrapper)

**Build 5 status = ⚠️ subsystems tested, setup wizard live on testnet. Run loop and chain client fixes needed (B5.0.1).**

---

## BUILD 6: Batch Data API

**Goal:** Indexer + API serving batch data to external consumers.

**Status:** Not started. All dependencies met (B5 complete).

### 6A: Indexer
- On-chain source: poll BatchTradeSettled events
- Off-chain source: P2P observer node (subscribe to ALL pool topics)
- TimescaleDB: batches hypertable, reveals table
- Aggregation: per-match midpoints → pool avg → global avg
- OHLCV candle generation from settlement prices

### 6B: WebSocket hubs
- Channels: batch:{symbol}, pool:{symbol}:{pool_id}, pools:global, system
- Premium: reveal:{symbol}, trustees:{symbol}
- Redis pub/sub fanout, API key authentication

### 6C: REST API
- GET /v1/batches, /v1/candles, /v1/pools, /v1/stats, /v1/markets, /v1/status
- Premium: /v1/reveal, /v1/trustees, /v1/export
- Pagination, filtering, rate limiting per tier

**Build 6 done = API serving live and historical data. Revenue-generating product.**

---

## Build Order Summary

```
BUILD   DELIVERS                                STATUS        TESTS    DEPENDS ON
─────   ────────────────────────────────────    ──────────    ─────    ──────────
1       Contracts on testnet                    ✅ COMPLETE    146      Nothing
2       Rust binary talks to chain              ✅ COMPLETE     96      Build 1
3       Multi-node gossip + matching            ✅ COMPLETE     80      Build 2
4       Settlement + balance tracking           ✅ COMPLETE     84      Build 1+3
5       Subsystems + setup wizard               ⚠️ PARTIAL      85      Build 4
5.5     Testnet wiring + run loop               ⬜ READY         —      Build 5
6       Batch Data API (closed, revenue)        ⬜ READY         —      Build 5.5
```

**Total tests: 445 (146 Move + 295 Rust + 4 Python)**

---

## Workspace Crate Map (current — B5 complete)

```
deadmkt-node/
├── crates/
│   ├── config/             B2    9 tests   config.json load/save/validate
│   ├── keystore/           B2    9 tests   AES-256-GCM + Argon2id encryption
│   ├── crypto/             B2   16 tests   Ed25519, BCS, hashing, signing (+3 interop ignored)
│   ├── chain/              B2–5 54 tests   REST client, poller, events (24 variants), governance
│   ├── storage/            B2+4 17 tests   SQLite: nfts, markets, settlements, balance_snapshots
│   ├── setup/              B2   27 tests   First-boot wizard (+2 testnet ignored)
│   ├── matching/           B3   17 tests   Deterministic matching, partial fills
│   ├── escrow_tracker/     B4   24 tests   Projected balance, outflow/inflow/earmark
│   ├── settlement/         B4   25 tests   Manager, submitter, abort handling, backstop
│   ├── withdrawal/         B4    7 tests   7-state FSM, activity guards
│   ├── profit/             B4    8 tests   Threshold, transfer modes, safety
│   ├── strategy/           B5   33 tests   WS server, events, actions, conversion, validation
│   ├── node_state/         B5   13 tests   State machine (7 states)
│   ├── gas_manager/        B5   10 tests   Two-tier gas thresholds
│   └── orchestrator/       B5   13 tests   Batch loop coordinator + governance routing
├── src/
│   ├── main.rs                             Binary with clap CLI
│   └── cli.rs                    6 tests   Subcommand parsing
├── tests/
│   ├── b4_settlement_integration.rs  5 tests  Settlement + profit end-to-end
│   └── b5_integration.rs            1 test   Full batch cycle via WS
├── starter_bot/
│   └── strategy.py                         Default spread strategy (Python)
├── strategy_wrapper/
│   └── bridge.py                           WS bridge with hot reload (Python)
├── Dockerfile                              Multi-stage: Rust + Python
└── entrypoint.sh                           Process manager
```

**Excluded from workspace (libp2p Rust 1.83+ requirement):**
```
    ├── batch_state/        B3   12 tests   Per-batch lifecycle + matching
    ├── gossip/             B3   13 tests   Messages + libp2p network
    ├── gossip_validation/  B3   16 tests   Commit/reveal validation + quota
    └── integration_tests/  B3    5 tests   Cross-crate batch cycle
```

Re-integration requires Rust upgrade from 1.75 → 1.83+. The orchestrator uses trait abstractions (`GossipPort`, `BatchStatePort`) that gossip/batch_state will implement when reconnected.

---

## Known Technical Debt

| Item | Build | Impact | Resolution |
|------|-------|--------|------------|
| **Chain client uses Aptos conventions (R65–R67, R70)** | **B2** | **Critical — all live trading broken** | **Fix in B5.0.1: endpoint, content-type, envelope, status parsing** |
| **Settlement missing SupraTransaction envelope (R67)** | **B4** | **Critical — settle_match will fail** | **Prepend 0x01 in B5.0.1** |
| **Run loop is a stub** | **B5** | **Critical — node doesn't actually trade** | **Wire in B5.0.1** |
| Abort code numeric constants are placeholder | B4 | Low — mapping unchanged until testnet testing | Remap `from_code()` after first testnet settlement |
| E_ALREADY_SETTLED 3-block fallback not tested | B4 | Medium — affects stuck settlement recovery | Add orchestrator timer in testnet hardening |
| Profit retry logic (max 3) not tested | B4 | Low — single attempt works, retry is safety net | Add in testnet hardening |
| libp2p crates excluded from workspace | B3 | None — traits abstract the boundary | Rust upgrade to 1.83+ |
| Strategy wrapper depends on `websockets` pip package | B5 | Low — installed in Docker image | Pinned in Dockerfile |
| Orchestrator phase handlers use simplified matching | B5 | Medium — real impl needs to wire to matching crate | Wire when gossip re-integrated |

---

## Next Steps

1. **B5.0.1 — Testnet wiring (BLOCKING):**
   - Fix chain client for Supra conventions (R65–R67, R70 in TESTNET_FINDINGS.md)
   - Add SupraTransaction envelope to settlement submitter
   - Wire run loop: keystore decrypt → chain poller → orchestrator → strategy WS → gossip
   - End-to-end: two Docker nodes complete a full batch cycle on testnet
2. **Rust upgrade (1.75 → 1.83+):** Re-integrate gossip/batch_state/gossip_validation crates. Wire GossipPort + BatchStatePort trait impls.
3. **B6 kickoff:** Indexer + API can proceed in parallel with testnet hardening.
