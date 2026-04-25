# DeadMKT Contract API Reference

Contract address: `0x4a239389433f51450765244d9891c6628b619e41a8aac82cdda4653353fb4a36`
Chain: Supra Testnet (chain_id: 6)

All modules deployed under a single address. 5 modules: `nft`, `escrow`, `settlement`, `pool_config`, `tokens`.

---

## Pricing & Decimals

| Token | Decimals | 1.0 token = raw |
|-------|----------|-----------------|
| EMM   | 5        | 100,000         |
| KAY   | 5        | 100,000         |
| TEE   | 5        | 100,000         |
| SUPRA | 8        | 100,000,000     |

**SUPRA_PER_TOKEN_UNIT = 100** (raw SUPRA per raw token unit)

| Tokens (human) | Raw tokens | SUPRA cost (raw) | SUPRA cost (human) |
|----------------|-----------|-------------------|---------------------|
| 1.00000        | 100,000   | 10,000,000        | 0.1 SUPRA           |
| 10.00000       | 1,000,000 | 100,000,000       | 1.0 SUPRA           |
| 50.00000       | 5,000,000 | 500,000,000       | 5.0 SUPRA           |
| 100.00000      | 10,000,000| 1,000,000,000     | 10.0 SUPRA          |
| 140.00000      | 14,000,000| 1,400,000,000     | 14.0 SUPRA          |

**Cost formula:** `supra_cost = (m + k + t) * 100` raw SUPRA

**Burn returns:** `supra_return = burn_amount * 3 * 100` raw SUPRA (burns equal from all 3)

---

## tokens.move — Trippples Token System

### Mint State Machine

```
AWAITING_TRIGGER (0) ──request_mint──→ DVRF_PENDING (1)
                                           │
                           VRF callback    │
                          ┌────────────────┘
                          │
                     roll 1-6                roll 7
                          │                    │
                          ▼                    ▼
                    OPEN (2)             BLOCKED (3)
                    (mint window)        (cooldown)
                          │                    │
                     period_end           block_end
                          │                    │
                          └────────┬───────────┘
                                   ▼
                          AWAITING_TRIGGER (0)
```

**Roll 1-6:** OPEN window. Hold duration = roll × 86400 seconds (1-6 days).
**Roll 7:** BLOCKED. Refund trigger's SUPRA. Block duration = base + surplus hours.

### validate_amounts(m, k, t) — Mint Ratio Rules

Strategies can mint unequal amounts, but within bounds:

1. **All positive:** `m > 0 && k > 0 && t > 0`
2. **Divisible by 10 tokens:** `(m + k + t) % 1_000_000 == 0`
3. **Base ratio:** `2 * (min + mid) >= 3 * max` — no single token can exceed ~40% of total
4. **Delta ratio:** `3 * (mid + max - 2*min) <= min` — spread between amounts is bounded

**Examples (raw amounts):**

| m | k | t | total | valid? | why |
|---|---|---|-------|--------|-----|
| 5000000 | 5000000 | 5000000 | 15000000 | ✅ | equal, divisible |
| 6000000 | 5000000 | 4000000 | 15000000 | ✅ | within ratio limits |
| 8000000 | 1000000 | 1000000 | 10000000 | ❌ | base ratio violated (80% in one) |
| 3333333 | 3333333 | 3333334 | 10000000 | ❌ | not divisible by 1000000 |
| 4000000 | 3000000 | 3000000 | 10000000 | ✅ | 40/30/30 is within limits |

**Practical max skew:** roughly 40/30/30 split. Strategy can favor whichever token is lowest in escrow.

### Entry Functions

#### `request_mint(caller, m_amount, k_amount, t_amount)`
Start a mint. Transfers SUPRA bond to PendingMintEscrow.

- **First mint (per trustee):** Bypasses global state machine. Hold = `first_mint_hold_secs` (testnet: 480s). Claimable after hold.
- **Subsequent mints:** Depends on global state:
  - `AWAITING_TRIGGER`: Caller becomes VRF trigger. SUPRA locked. VRF fires.
  - `OPEN`: Mint window open. Immediate pending with current hold duration.
  - `DVRF_PENDING`: **Blocked.** Must wait for VRF to resolve.
  - `BLOCKED`: **Blocked.** Must wait for block_end.

**Preconditions:** Registered escrow, no existing pending mint, validate_amounts passes.
**Cost:** `(m + k + t) * 100` raw SUPRA transferred to escrow.
**Strategy JSON:** `{"action": "mint", "m": <raw>, "k": <raw>, "t": <raw>}`

#### `claim_mint(caller)`
Claim a pending mint after hold period expires. Mints FA tokens to caller's wallet.

**Preconditions:** Has pending mint, `now >= claimable_at`.
**Effect:** Tokens minted to trustee wallet. SUPRA moved from PendingEscrow to Treasury.
**Strategy JSON:** `{"action": "claim_mint"}`

**Note:** After claim, tokens are in the WALLET, not escrow. The node's token_worker (B5_9.1) auto-deposits wallet→escrow after claim.

#### `burn_mkt(caller, burn_amount)`
Burn equal amounts of all 3 tokens, receive SUPRA back from treasury.

**Preconditions:** Wallet has `burn_amount` of each of EMM, KAY, TEE.
**Returns:** `burn_amount * 3 * 100` raw SUPRA from treasury.
**Strategy JSON:** `{"action": "burn", "amount": <raw>}`

#### `lock_tokens(caller, symbol, amount, min_duration_secs)`
Lock tokens in private vault. Removed from wallet.

- `symbol`: 0=EMM, 1=KAY, 2=TEE
- Duration bounds: `min_lock_duration_secs` to `max_lock_duration_secs` (default 1hr–30d)

**Strategy JSON:** `{"action": "lock", "symbol": "EMM"|"KAY"|"TEE", "amount": <raw>, "duration_secs": <u64>}`

#### `unlock_tokens(caller, lock_index)`
Unlock tokens after minimum duration. Returns to wallet.

**Preconditions:** Lock exists, not already claimed, `now >= minimum_unlock_at`.
**Strategy JSON:** `{"action": "unlock", "lock_index": <u64>}`

#### `cancel_pending_mint(caller)`
Cancel own pending mint after VRF timeout. Refunds SUPRA. Only callable by the trigger minter after `dvrf_timeout_blocks`.

#### `cancel_expired_dvrf(_caller)`
Permissionless cleanup. Anyone can call to unblock minting when VRF callback hasn't arrived after timeout. Refunds SUPRA to original trigger trustee.

### View Functions

| Function | Returns | Description |
|----------|---------|-------------|
| `get_mint_state()` | (state, period_end, hold_duration, block_end, rotation_index) | Global mint state machine |
| `get_mint_config()` | (first_mint_hold_secs, dvrf_timeout_blocks, block_base_duration_secs, rotation_hour_step, dvrf_enabled) | Admin-configurable timing |
| `get_circulating_supply()` | (m, k, t) | Total circulating raw amounts |
| `get_total_minted()` | (m, k, t) | Total ever minted |
| `get_treasury_balance()` | u64 | SUPRA in treasury |
| `get_pending_escrow_balance()` | u64 | SUPRA locked for pending mints |
| `is_minting_open()` | bool | True if state=OPEN and not expired |
| `get_nft_mint_state(addr)` | (first_mint_completed, has_pending) | Per-trustee mint state |
| `get_pending_mint(addr)` | Option\<PendingMint\> | Pending mint details (amounts, claimable_at) |
| `get_vault_locks(addr)` | vector\<TokenLock\> | All locks for trustee |
| `get_metadata_address(symbol)` | address | FA metadata address (0=EMM, 1=KAY, 2=TEE) |
| `get_all_metadata_addresses()` | (emm, kay, tee) | All three in one call |
| `get_token_balance(addr, symbol)` | u64 | FA wallet balance |
| `has_pending_mint(addr)` | bool | Has unclaimed mint |
| `is_dvrf_trigger(addr)` | bool | Is current VRF trigger minter |
| `has_active_locks(addr)` | bool | Has unclaimed locks |

---

## escrow.move — Token Escrow & Inactivity System

### Key Concepts

- **Escrow** holds tokens for trading. Deposits update `last_activity_at`.
- **Inactivity:** If `now > last_activity_at + holding_period_days * 86400`, NFT is inactive. Heartbeats and trades are rejected.
- **Heartbeat** keeps NFT active. Requires at least 1 token in escrow.
- **Deposit** reactivates inactive NFTs (sets `last_activity_at = now`).

### Entry Functions

#### `register_trader(trustee, nft_id, holding_period_days, rushed_withdrawal_enabled)`
Register for trading. Creates escrow and withdrawal config.

#### `deregister_trader(beneficiary, nft_id)`
Unregister. Withdraws all escrow to beneficiary. Use `tokens::safe_deregister` instead (checks pending mints/locks).

#### `deposit(trustee, token_metadata, amount)`
Deposit FA tokens from wallet into escrow. **Updates `last_activity_at` — reactivates inactive NFTs.**

**This is the key reactivation mechanism.** No active check on deposit.

#### `heartbeat(trustee)`
Update `last_activity_at` to prevent inactivity timeout. **Requires at least 1 token in any escrow balance.** Fails if escrow is empty.

#### `transfer_to_beneficiary(trustee, nft_id, token_metadata, amount)`
Send escrow tokens to beneficiary address.

#### `withdraw_triples_to_wallet(trustee, nft_id, amount)`
Withdraw equal amounts of all 3 tokens from escrow to trustee wallet. Used before `burn_mkt`.

#### `request_rushed_withdrawal(beneficiary, nft_id, token_metadata, amount)` / `execute_rushed_withdrawal` / `cancel_rushed_withdrawal`
Emergency withdrawal path through beneficiary.

#### `start_holding_period(beneficiary, nft_id)` / `cancel_holding_period` / `claim_all`
Normal withdrawal: start holding period, wait, claim everything.

### View Functions

| Function | Returns | Description |
|----------|---------|-------------|
| `get_balance(nft_id, token_metadata)` | u64 | Escrow balance for one token |
| `is_active(nft_id)` | bool | Not expired by inactivity |
| `get_last_activity(nft_id)` | u64 | Timestamp of last activity |
| `get_withdrawal_config(nft_id)` | (nft_id, holding_days, rushed, holding_started, start_time, trigger) | Withdrawal state |
| `has_pending_rushed(nft_id)` | bool | Rushed withdrawal pending |
| `get_vault_address()` | address | Escrow vault address |
| `is_registered(trustee_addr)` | bool | Has WithdrawalConfig |
| `has_escrow(addr)` | bool | Has Escrow resource |

---

## settlement.move — Trade Settlement

### Entry Functions

#### `settle_match(submitter, batch_id, pool_id, buyer_nft_id, seller_nft_id, pair_symbol, price, quantity, buyer_sig, seller_sig, match_hash)`
Settle a matched trade on-chain. Transfers tokens between escrows.

**Preconditions:** Both NFTs active, valid signatures, not already settled, batch not too old, market active, sufficient escrow balances.

#### `add_market_pair(admin, symbol, base_metadata, quote_metadata, min_quantity)`
Add a trading pair. Admin only.

#### `report_commit_violation(reporter, accused_nft_id, batch_id, ...)` / `unblock_nft(admin, nft_id)`
Enforcement for protocol violations.

### View Functions

| Function | Returns | Description |
|----------|---------|-------------|
| `get_trade(trade_id)` | TradeRecord | Trade details by match hash |
| `get_total_trade_count()` | u64 | Total settled trades |
| `is_match_settled(match_hash)` | bool | Already settled |
| `get_market_pair(symbol)` | MarketPair | Market config |
| `get_min_quantity(symbol)` | u64 | Minimum order quantity |
| `get_all_market_symbols()` | vector\<vector\<u8\>\> | All market symbols |
| `get_order_fills(order_hash)` | (filled_qty, filled_quote) | Partial fill state |
| `is_paused()` | bool | Settlement paused |

---

## nft.move — Trustee/Beneficiary NFT Pairs

### Entry Functions

#### `mint_pair(trustee, ed25519_pubkey, beneficiary)`
Mint a trustee+beneficiary NFT pair. Costs SUPRA bond (default 1 SUPRA testnet).

#### `reclaim_mint_bond(trustee)`
Reclaim bond after lock period expires.

#### `burn_pair(beneficiary, nft_id)`
Burn both NFTs. Must deregister first.

### View Functions

| Function | Returns | Description |
|----------|---------|-------------|
| `get_beneficiary(nft_id)` | address | Current beneficiary |
| `get_trustee_pubkey(nft_id)` | vector\<u8\> | ED25519 pubkey |
| `get_trustee_address(nft_id)` | address | Trustee address |
| `is_trustee(addr)` | bool | Has TrusteeNFT |
| `get_total_minted()` | u64 | Total NFTs minted |
| `get_nft_config()` | (bond_amount, lock_secs, cooldown_secs, admin) | Config |
| `get_mint_bond(addr)` | (exists, amount, unlock_at, bond_block) | Bond state |
| `get_bond_vault_balance()` | u64 | Total SUPRA in bonds |

---

## pool_config.move — Batch & Pool Configuration

### View Functions

| Function | Returns | Description |
|----------|---------|-------------|
| `get_batch_params()` | BatchParams | blocks_per_batch, commit/reveal/match/swap blocks |
| `get_current_batch_id()` | u64 | Current batch number |
| `get_batch_epoch()` | (anchor_block, anchor_batch_id) | Epoch reference |
| `get_num_pools()` | u64 | Active pool count |
| `get_live_nft_count()` | u64 | Registered active NFTs |
| `get_target_pool_size()` | u64 | Target nodes per pool |

---

## Strategy Decision Data (from BatchStart)

The node sends `batch_start` every commit phase. Fields available to strategy:

| Field | Type | Description | Status |
|-------|------|-------------|--------|
| `batch_id` | u64 | Current batch | ✅ |
| `pool_id` | u64 | Assigned pool | ✅ |
| `escrow` | {token: balance} | Projected escrow | ✅ (stale — startup only) |
| `escrow_confirmed` | {token: balance} | On-chain escrow | ✅ (= escrow currently) |
| `wallet` | {token: balance} | Wallet balances | ✅ (stale — startup only) |
| `gas_balance` | string | SUPRA balance | ❌ hardcoded "0" |
| `last_batch` | {matches, volume} | Previous batch | ❌ always null |
| `pending_settlements` | [{hash, batch, status}] | Pending settles | ❌ always empty |
| `peers_in_pool` | u64 | Gossip peers | ❌ hardcoded 0 |
| `mint_state` | {state, hold_duration, period_end, block_end, has_pending_mint, pending_claimable_at} | Mint state | ✅ |
| `circulating` | {token: supply} | Circulating supply | ✅ |
| `vault_locks` | [{symbol, amount, unlock_at, claimed}] | Locked tokens | ✅ |

### Strategy Actions (via WS)

| Action | JSON | Handler | When |
|--------|------|---------|------|
| commit | `{"action": "commit", "orders": [...]}` | orchestrator | batch_start response |
| reveal | `{"action": "reveal", "reveal_indices": [...]}` | orchestrator | reveal_start response |
| mint | `{"action": "mint", "m": <raw>, "k": <raw>, "t": <raw>}` | token_worker | anytime |
| claim_mint | `{"action": "claim_mint"}` | token_worker | anytime |
| burn | `{"action": "burn", "amount": <raw>}` | token_worker | anytime |
| lock | `{"action": "lock", "symbol": "EMM", "amount": <raw>, "duration_secs": <u64>}` | token_worker | anytime |
| unlock | `{"action": "unlock", "lock_index": <u64>}` | token_worker | anytime |

### Strategy Mint Decision Flowchart

```
1. Read escrow balances → which tokens are low?
2. Read gas_balance → how much SUPRA available?
3. Calculate: available = gas_balance - reserve (5 SUPRA)
4. Calculate: max_total_raw = (available * 10^8) / 100
5. Decide ratio: favor low tokens, respect validate_amounts rules
6. Check mint_state:
   - AWAITING_TRIGGER or OPEN → can mint
   - DVRF_PENDING → blocked, wait
   - BLOCKED → blocked, wait for block_end
7. Check has_pending_mint → if true, wait for claimable_at → claim_mint
8. Send mint action with calculated amounts
```

---

*Generated from contract source. Last updated: 2026-03-17*
