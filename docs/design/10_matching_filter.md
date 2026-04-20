# #10 — Matching filter for inactive NFTs (design review)

Status: **Proposed**. Targets v0.1.11. Review before coding.

## Problem

The matching engine pairs orders without checking whether the issuing NFT is
`escrow::is_active()` on-chain. When it pairs with an inactive counterparty,
the resulting `settle_match` tx aborts with `E_BUYER_INACTIVE` /
`E_SELLER_INACTIVE` — wasting a tx fee (the designated `gas_payer_nft_id`
node pays), producing no trade, and flooding logs.

Observed in two runs:

- **2026-04-17 Hetzner restore test** — NFTs 2/3/4/5 inactive, matched with
  active NFTs, every settlement aborted.
- **2026-04-18 fresh 2-node test** — NFT 5 (ghost), then 2/3/4 (local
  nodeB/C/D after stop), still in gossip mesh → 100% settle-abort rate until
  those nodes were stopped at the gossip source.

## The consensus constraint (why this is non-trivial)

The matching engine MUST be deterministic across every node that sees the
same reveal set. If node A drops NFT-5 from matching because its local
`is_active` cache says inactive, and node B keeps NFT-5 because its cache is
still fresh (says active), they produce **different match lists** →
divergent settle txs → network split. The matching crate header (`CRITICAL`
comment) states this explicitly.

Therefore any filter that influences the *match list itself* requires a
canonical input shared across all nodes. A local per-node cache cannot drive
matching directly without breaking consensus.

## Proposed scope: Path 1 — settle-side filter

Accept the consensus constraint. Keep matching unchanged. Instead, filter at
**settlement submission time**, where only the `gas_payer_nft_id` node acts,
so local state divergence is harmless.

### Flow

1. Matching runs as today on every node, producing an identical match list.
2. For each match where `m.gas_payer_nft_id == self.nft_id`, before queuing
   the `SettleRequest`, check `is_active(m.buyer.nft_id)` and
   `is_active(m.seller.nft_id)` against a local cache.
3. If either returns `Some(false)`, skip submission, log
   `[settle-filter] skipped <match_hash> — nft=<id> inactive`, and register
   the match as `Skipped(Inactive)` in `SettlementManager` so strategy /
   telemetry see the outcome.
4. If the cache lookup is `None` (unknown or expired), either (a) fetch
   synchronously (blocks settle path; simple) or (b) submit optimistically
   and let the on-chain abort be caught by #13c post-submit verification.
   **Recommended**: (b) — optimistic. A cache miss today is not worse than
   no filter, and #13c already logs the abort with code.
5. The cache refreshes lazily on reveal ingest and on observed settle
   aborts (negative cache).

### Why this is safe

- Matching output is byte-identical on every node regardless of cache state.
- Only one node (the gas payer) decides whether to submit. If its cache
  returns a stale-positive, the contract aborts — same as today, no
  regression. If its cache returns a stale-negative, the trade is skipped
  — new failure mode, bounded by TTL.
- No gossip traffic change. No contract change. No change to reveal or
  commit acceptance rules.

### Cache design

```rust
struct IsActiveCache {
    // nft_id -> (is_active, fetched_at)
    entries: HashMap<u64, (bool, Instant)>,
    ttl: Duration,          // default 30s
    max_entries: usize,     // LRU eviction at ~10_000 to bound memory
}
```

Methods:

- `get(nft_id) -> Option<bool>` — returns cached value iff age < TTL.
- `insert(nft_id, is_active)` — records observation with current `Instant`.
- `invalidate(nft_id)` — forces next `get` to miss (used on reactivate
  events).
- `active_count() / inactive_count()` — for the `[settle-stats]` line.

### Population

- **Eager on new gossip nft_id**: when `GossipValidator::cache_pubkey()`
  fires for an unknown nft (already a hot path for pubkey fetch), also
  kick off an `is_active` fetch in the same background task. One extra
  view call per new peer, once.
- **Lazy refresh on expiry**: inside the gas-payer settle check, if the
  entry is absent or stale and the optimistic submit path is disabled,
  fetch synchronously with a 2s timeout; on timeout, fall through to
  optimistic submit.
- **Invalidate on `escrow::Reactivated` event**: tack onto the existing
  chain-event poll loop (`src/run.rs` event-type builder, ~line 1899).
  When a `Reactivated { nft_id, trustee, timestamp }` event arrives,
  call `cache.invalidate(nft_id)` so the next check sees the new state.
- **Invalidate on `HeartbeatSent` for reaped-then-recovered**: not
  strictly needed — `Reactivated` is the DMKT12 fast path; mint-recovery
  fires `NftMinted` which already triggers a fresh cache insert.

### Wiring points

- **`crates/matching/src/lib.rs`**: unchanged. Do NOT add any filter here.
- **`src/run.rs`** (~line 1775, the gas-payer check):

  ```rust
  if is_gas_payer {
      let buyer_active = is_active_cache.get(m.buyer.order.nft_id);
      let seller_active = is_active_cache.get(m.seller.order.nft_id);
      if buyer_active == Some(false) || seller_active == Some(false) {
          // register skip + continue
          continue;
      }
      // else: queue SettleRequest as today
  }
  ```

- **`crates/settlement/src/lib.rs`** — `SettlementManager`: add a new
  `register_skipped(..., reason: SkipReason)` helper and a `SkipReason`
  enum with `Inactive { nft_id: u64 }` as the first variant. Keeps the
  state machine honest — a skipped match is neither pending nor
  submitted.
- **`src/run.rs` event loop** — inside the chain-event poll, subscribe
  to `escrow::Reactivated` events and call `cache.invalidate()`.
- **`[settle-stats]` line** — extend with `skipped_inactive=<n>` so the
  100-block report surfaces the filter's impact.

### Acceptance tests

| Scenario | Expected |
|---|---|
| 30% of gossip NFTs inactive, fresh cache | Zero `E_*_INACTIVE` aborts in 100 batches |
| Cache TTL expires mid-batch for an inactive NFT | Optimistic submit, one abort, then cache auto-refreshes via #13c's verify path |
| NFT reactivates via `escrow::reactivate()` | Within 1 batch of the `Reactivated` event, cache invalidates and the NFT's next match submits |
| Unknown NFT appears in gossip | `is_active` fetched once eagerly in the cache_pubkey pipeline |
| No RPC amplification | `<1` extra view call per unique NFT per TTL window, aggregate across all matches |

### Expected impact

In the 2026-04-18 run, 100% of settles against NFT-5 aborted. With Path 1
and a 30s cache populated eagerly on first gossip, the gas-payer node
skips every match involving an inactive counterparty after the first
observation window (~30s), dropping the abort rate to zero for the
remainder of the run. Gas per stuck-peer saved ≈ `abort_gas_cost × matches_against_inactive_per_batch`.

### Risks & downsides

- **False negatives**: if an NFT flips active→inactive→active within the
  TTL window, we miss real trades. Mitigated by invalidating on
  `Reactivated` events (and the fact that heartbeat interval ≫ TTL).
- **Operator surprise**: a fast-exiting-then-rejoining node sees trades
  skipped for up to 30s after rejoining. Acceptable.
- **Observability gap before rollout**: until #13c's abort-code rollups
  separate `E_*_INACTIVE` from other codes, we can't distinguish
  filter-worthy aborts from genuine logic aborts. Low risk, one-line
  addition to `AbortCode::Display`.

## Out of scope (lands as new ticket — see below)

Any filter that alters the *match list itself* — i.e. drops an inactive
NFT's orders before matching runs — is **not** in this design because
it requires a canonical active-set input across all nodes. See the new
follow-up ticket below.

## Implementation plan (v0.1.11)

1. `crates/escrow_tracker/src/is_active_cache.rs` — new module with the
   cache struct and tests (TTL expiry, LRU eviction, invalidate).
2. `crates/settlement/src/lib.rs` — add `SkipReason` + `register_skipped`.
3. `src/run.rs` — wire the cache: populate eagerly in the unknown-nft
   pubkey path; query in the `is_gas_payer` branch; invalidate on
   `Reactivated` events; roll into `[settle-stats]`.
4. Integration test: mock two reveals, one from an inactive NFT, assert
   zero SettleRequests queued and one Skipped(Inactive) registered.

Estimate: ~300 lines across 3 files. One feature branch `fix/10-*`. No
breaking interface changes.

---

## NEW TICKET — #XX Deterministic pre-match active-set filter (Path 2)

**Repo:** `deadmkt-node` (may require `deadmkt-supra-contracts` change)
**Depends on:** #10 (Path 1 settle-side filter merged first)
**Priority:** low — #10 captures 95% of the value without touching
consensus.

### Goal

Drop orders from inactive NFTs **before matching runs**, so matches are
never computed against known-dead peers. This saves all the hashing /
sorting work the matching engine does for doomed pairs, and surfaces a
cleaner match list to strategy.

### Why a new ticket

Path 1 (settle-side filter) is consensus-safe because only one node
(the gas payer) acts on its local cache view. Path 2 requires every
node to arrive at the **same active-set input** to the matching
function. Different local caches → different match lists → network
split. This cannot be done with per-node caches alone.

### Design options (any ONE needed to enable Path 2)

1. **Block-pinned view** — if Supra exposes `view(.., at_block=N)` we
   can have every node query `is_active(nft_id)` at the batch anchor
   block and get identical results. **Status: need to verify Supra API
   supports this** — the Aptos framework's `MoveResolver` does support
   historical reads, but the REST view endpoint may not expose it.
   First action: RFC in the Supra discord / check
   `rest-api/testnet/view` docs.

2. **Contract-side `get_active_nft_set()` view** — have the `escrow`
   module export a view that returns `vector<u64>` of currently-active
   nft_ids (or a bloom/compact representation for scale). Every node
   calls it once per batch at the same anchor block and uses the
   result as the canonical input filter. **DMKT13+ contract work**,
   ~30 LOC in `escrow.move`.

3. **Gossip-validated active set** — nodes gossip their local
   observations, run a consensus round (2f+1 threshold), and use the
   agreed set. Heavy — adds a second consensus step per batch. Not
   recommended.

### Acceptance

- With 30% inactive NFTs producing gossip, 0 matches computed against
  them (not just 0 settlements submitted).
- Match list byte-identical across all nodes (existing integration
  test `crates/integration_tests` proves this — extend to cover the
  filtered case).
- RPC amplification ≤ 1 view call per node per batch (scales with
  batch count, not match count).

### Scope / estimate

- Option 1: ~80 LOC node-side (modify `SupraClient::view_raw` to
  accept optional block), zero contract work. **Cheapest if Supra
  supports it.**
- Option 2: ~100 LOC contract + ~80 LOC node-side. Requires DMKT13
  deploy.
- Option 3: not recommended unless 1 + 2 both blocked.

### Relationship to #10 (Path 1)

Path 2 subsumes Path 1 — once the match list itself is clean, the
settle-side filter becomes redundant and can be deleted. But Path 1
should ship first because (a) it fixes the observed pain now, (b) it
validates the cache infrastructure, and (c) Path 2 may be blocked on
Supra API or contract deploy cadence.
