# deadmkt-node B5.5 — Deployment Summary

**Date:** 2026-03-03
**Build:** v0.1.3 (versioned gossip envelope + UPG-1)
**Parent:** B5.4 (gossip hardening + dedicated swarm task)
**Network:** Supra Testnet
**Contract:** `0xb38e179b2822331922fbe4a65c738d74f7441dee989375deeb336b9df388522c`

---

## What B5.5 Delivers

B5.5 adds UPG-1 (versioned gossip envelope) on top of B5.4. B5.4 implemented SEC-1, SEC-2, and GOSSIP-1 in `src/run.rs`. B5.5 adds the wire-level versioning in `crates/gossip/src/messages.rs`.

### UPG-1 ✅ — Versioned Gossip Envelope

Every gossip message on the wire is now wrapped in a `GossipEnvelope { version: u16, message: GossipMessage }`. Nodes reject messages from incompatible protocol versions before attempting to process the inner message.

**How it works:** `serialize()` wraps in envelope with `PROTOCOL_VERSION = 1`. `deserialize()` checks the version field first — mismatches produce a `GossipCodecError::VersionMismatch` that is logged distinctly from corrupt data. The gossip task now logs: `[gossip-task] rejected inbound message: gossip version mismatch: got v999, expected v1`.

**Impact:** Safe future upgrades. Old nodes reject new-format messages cleanly instead of silently failing or crashing. Startup banner now shows `protocol=v1`.

**New tests:** T_MSG_06 (envelope version correct), T_MSG_07 (wrong version rejected), T_MSG_08 (bare pre-envelope message rejected).

### SEC-1 ✅ — Ed25519 Signature Verification on Inbound Gossip Reveals

Every inbound `Reveal` message has its Ed25519 signature verified against the on-chain public key for the claiming NFT ID before the reveal is stored.

**How it works:** `GossipValidator` maintains a lazy pubkey cache (`nft_pubkeys: HashMap<u64, VerifyingKey>`). Unknown NFTs are queued for async on-chain fetch via `nft::get_trustee_pubkey()`. Cache warms within 3 batches. Verification uses `deadmkt_crypto::verify()` — same function the on-chain contract uses.

**Impact:** Fake reveals rejected. Gas-draining via fraudulent settlements eliminated.

### SEC-2 ✅ — Deduplication, Rate Limiting, Batch Bounds, Memory Cleanup

| Defense | Commits | Reveals | Implementation |
|---|---|---|---|
| **Batch bounds** | ✅ | ✅ | Reject where `\|batch_id - current\| > 5` |
| **Content-hash dedup** | ✅ (commit_hash) | ✅ (SHA256(order_bytes)) | `HashSet<[u8; 32]>` |
| **Rate limiting** | ✅ | ✅ | Max 10 messages per (nft_id, batch_id) |
| **Memory cleanup** | ✅ | ✅ | Old batches expired at boundary; periodic hash clear |

Content-hash dedup correctly handles `commits_per_batch = 3` (multiple distinct orders per node per batch).

### GOSSIP-1 ✅ — Dedicated Tokio Task for Swarm Processing

**The problem:** In B5.3, gossip messages and phase handling shared a single `tokio::select!` loop. When the chain tick arm ran async work (param fetches, strategy timeout, settlement processing), the swarm wasn't polled. Reveals sat in the libp2p buffer, invisible at MATCH evaluation time. This caused 30–40% reveal misses.

**The fix:** The libp2p swarm now runs in its own `tokio::spawn` task that polls continuously, independent of chain tick work. Deserialized messages flow to the main loop via an mpsc channel (capacity 256). Outbound publishes, subscribe/unsubscribe commands flow back via a command channel (capacity 64).

**Architecture:**

```
┌─────────────────────────────────────┐
│  Gossip Task (tokio::spawn)         │
│                                     │
│  loop {                             │
│    select! {                        │
│      swarm event => deserialize,    │
│        send to gossip_inbound_tx    │
│      command => publish/sub/unsub   │
│    }                                │
│  }                                  │
└────────┬────────────────┬───────────┘
         │ GossipMessage  │ SwarmCommand
         ▼                ▲
┌────────────────────────────────────┐
│  Main Loop                         │
│                                    │
│  select! {                         │
│    gossip_inbound_rx => validate,  │
│      store (eager ARM 1)           │
│    chain_tick => {                  │
│      drain remaining gossip;       │
│      phases: COMMIT/REVEAL/MATCH/  │
│        SWAP (unchanged)            │
│    }                                │
│  }                                  │
└────────────────────────────────────┘
```

**Three gossip drain points:**
1. **ARM 1 (eager):** `select!` arm processes messages as they arrive between ticks
2. **Chain tick start:** `while try_recv()` — bulk drain before any phase evaluation
3. **MATCH phase entry:** Additional `while try_recv()` — catches anything that arrived during REVEAL phase processing

**Outbound messages** (commit/reveal publishes, pool subscribe/unsubscribe) flow via `SwarmCommand` enum through the command channel. `flush_gossip_via_channel()` replaces the old `flush_gossip()`.

**Pool reassignment fix:** `my_pool` is now properly updated (`my_pool = new_pool`) at batch boundaries. Previously it was never reassigned — harmless with 1 pool but would have caused stale unsubscribes with multi-pool.

**Expected impact:** Match rate ~85–95% with 2 nodes (up from ~60–70%). The reveal window becomes actual propagation time rather than propagation time + poll latency.

### FIX-1 ✅ — WebSocket Keepalive (already in B5.3)

Server-side 30-second ping interval was already implemented in `crates/strategy/src/server.rs` (lines 281–336). The connection handler has a three-arm `select!` loop: events→client, client→actions, and a 30s ping timer. Inbound `Ping` frames are responded to with `Pong`. The Python `websockets` library handles Ping/Pong automatically on the client side. No changes needed.

---

## Phase 1 Complete + UPG-1

All Phase 1 items (SEC-1, SEC-2, GOSSIP-1, FIX-1) plus UPG-1 are done. Tagged as B5.5 v0.1.3.

---

## Constants

| Constant | Value | Purpose |
|---|---|---|
| `BATCH_WINDOW` | 5 | Max batch_id distance for bounds check |
| `PUBKEY_FETCH_INTERVAL` | 3 batches | Min batches between pubkey fetch attempts |
| `MAX_MSGS_PER_NFT_PER_BATCH` | 10 | Rate limit per (nft_id, batch_id) |
| Gossip inbound channel | 256 | Buffer for deserialized inbound messages |
| Swarm command channel | 64 | Buffer for outbound publish/sub commands |

---

## Files Changed (from B5.3)

| File | Change |
|---|---|
| `src/run.rs` | +375 lines. Added: `GossipValidator` (~200 lines), `SwarmCommand` enum, `process_inbound_gossip()` helper, `flush_gossip_via_channel()`, spawned gossip task (~40 lines), rewritten main loop with 2-arm select + triple drain. B5.5: gossip task logs version mismatches, startup banner shows `protocol=v1` |
| `src/main.rs` | Version bump to v0.1.3 |
| `crates/gossip/src/messages.rs` | B5.5: Added `PROTOCOL_VERSION`, `GossipEnvelope`, `GossipCodecError`. `serialize()`/`deserialize()` now envelope-aware. 3 new tests (T_MSG_06–08) |
| `crates/gossip/src/lib.rs` | B5.5: Re-exports `PROTOCOL_VERSION` |
| `Cargo.toml` | Version bump to 0.1.3 |

---

## Diagnostic Output

Validator stats at each batch boundary:

```
[gossip-validator] accepted=6 rejected: bounds=0 dedup=0 rate=0 sig=0 unknown_nft=0 | cached_keys=1
```

Gossip task channel overflow (should never appear under normal operation):

```
[gossip-task] inbound channel full, dropping message: ...
```

---

## Testing Plan

### Immediate (Docker Compose, 2 nodes)

1. `docker compose up -d` — verify both nodes start and trade normally
2. Watch for `[gossip] ← reveal from nft=X batch=Y (sig ✓)` — confirms SEC-1 + GOSSIP-1 working
3. **Measure match rate** over 50+ batches — expect ~85–95% (up from ~60–70% in B5.3)
4. Verify `[gossip-task]` errors do NOT appear (channel not overflowing)
5. Restart one node — verify pubkey cache warms within 3 batches, trading resumes

### Architecture Verification

6. During REVEAL phase, add a `sleep(5s)` to the chain tick handler. Verify reveals still arrive (they're buffered by the gossip task, not blocked by the main loop).
7. Kill the API (if any) — verify pure-gossip mode still works with improved match rate.

### Known Startup Behavior

- First 1–3 batches: `rejected_unknown` while pubkey cache warms (normal)
- First batch: `InsufficientPeers` while gossipsub mesh forms (unchanged from B5.3)

---

## What Carries Forward from B5.3

Everything in the B5.3 and B5.4 summaries remains current (settlement pipeline, restart resilience, contract readiness, security audit findings, upgrade gaps, scaling projections). B5.5 is purely additive.

The gossip scaling projections from B5.3 should now use the **"with GOSSIP-1"** column:

| Nodes per Pool | Expected Match Rate (GOSSIP-1) |
|---|---|
| 2 | ~85–95% |
| 4–6 | ~70–85% |
| 8–10 | ~55–70% |
| 15–20 | ~40–55% |

---

## Updated Implementation Order

### Phase 1: Node Gossip Hardening (no contract changes)

```
1.1  SEC-1: Verify Ed25519 signatures on inbound gossip reveals    ✅ B5.4
1.2  SEC-2: Deduplicate + rate limit + bounds check gossip          ✅ B5.4
1.3  GOSSIP-1: Move gossip to dedicated tokio task                  ✅ B5.4
1.4  FIX-1: Fix WebSocket keepalive timeout                         ✅ Already in B5.3
1.5  Test, tag as B5.5, deploy to testnet                           ✅ Tagged
```

### Phases 2–6: Unchanged from B5.3

### Updated Backlog

- [x] **SEC-1:** Verify Ed25519 signatures on inbound gossip reveals
- [x] **SEC-2:** Deduplicate + rate limit + bounds check on gossip messages
- [x] **GOSSIP-1:** Dedicated tokio task for swarm processing
- [x] **SEC-3:** Separate gossip identity keypair from trading/signing keypair (random Ed25519 keypair per boot, already in B5.3)
- [~] **SEC-4:** ~~Bind strategy WebSocket to `127.0.0.1` (or add TLS)~~ — Not doing. Strategy WS is Docker-internal only; container networking provides isolation.
- [ ] **SEC-5:** Implement gossip peer allowlist based on on-chain NFT registrations
- [ ] **SEC-6:** Memory bounds on gossip storage — *partially done* (batch expiry + rate limiting + channel bounds)
- [x] **FIX-1:** Fix WebSocket keepalive timeout (30s server-side ping, already in B5.3)
- [ ] **GOSSIP-2:** Extended reveal window — params to be decided during testing
- [x] **UPG-1:** Add versioned envelope to gossip messages — `GossipEnvelope { version: u16, message }` wraps all wire messages. Nodes reject version mismatches with diagnostic logging. `PROTOCOL_VERSION = 1`.
- [x] **UPG-2:** Add `PROTOCOL_VERSION` constant to both Rust node and Move contract — `pool_config::get_protocol_version()` view function, node checks on startup and refuses to run if mismatched.
- [x] **UPG-3:** Support config overrides via environment variables (`apply_env_overrides()` in config crate, already in B5.3)
- [x] **FEAT-1:** Settlement confirmation → update escrow tracker in real-time (re-fetches on-chain balances after confirm, already in B5.3)
- [x] **FEAT-2:** Strategy context: feed settlement results back via `StrategyEvent::Settlement` (already in B5.3)
- [ ] **FEAT-3:** Multi-pair support
- [ ] **OPS-1:** Log-level filtering
- [ ] **OPS-2:** Metrics endpoint
- [ ] **OPS-3:** Docker healthcheck endpoint
- [ ] **OPS-4:** Graceful shutdown — drain pending settlements before exit
