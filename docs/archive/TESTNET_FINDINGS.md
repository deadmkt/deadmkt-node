# Testnet Integration Findings (R64–R70)

**Date:** 2026-02-23
**Scope:** First live testnet submission cycle — setup wizard → mint_pair → register_trader
**Result:** All transactions succeeding after fixes. Three critical bugs found and resolved.

---

## R64: Signing Prefix — SUPRA, not APTOS

**Severity:** Critical (all transactions rejected)
**Affected:** `setup_bridge.rs`, `crates/settlement/src/lib.rs`
**Root cause:** Supra forked from Aptos but changed the signing prefix.

```
WRONG:  SHA3-256("APTOS::RawTransaction")  → 0xb5e97db0...
RIGHT:  SHA3-256("SUPRA::RawTransaction")  → 0x88d16ed1...
```

**How discovered:** Byte-by-byte comparison of hand-rolled BCS against Python SDK.
Both produced identical BCS and identical Ed25519 signatures, but the SDK
succeeded and Rust failed. Monkey-patching `Account.sign()` showed the SDK
never called it during `create_signed_transaction` — instead,
`RawTransaction.prehash()` builds the prefix internally. Reading the SDK
source revealed `b"SUPRA::RawTransaction"`.

**Fix applied:** Both `setup_bridge.rs` and `settlement/lib.rs` updated.

**Spec impact:** Any documentation referencing "APTOS::RawTransaction" as the
signing prefix for Supra must be corrected. The Supra SDK source is the
authority: `supra_sdk/transactions.py:135`.

---

## R65: Transaction Lookup Endpoint

**Severity:** Critical (all tx confirmations timed out)
**Affected:** `setup_bridge.rs` (fixed), `crates/chain/src/client.rs` (NOT YET FIXED)

```
WRONG:  GET /rpc/v1/transactions/by_hash/{hash}        → 404 empty
WRONG:  GET /rpc/v3/transactions/by_hash/{hash}        → 404 empty
RIGHT:  GET /rpc/v3/transactions/{hash}?type=user      → 200 + full tx data
```

Supra's API does not have a `by_hash` path segment. The hash goes directly
after `/transactions/`. The `?type=user` query param is needed for user
transactions (vs meta/record).

**Masked by R64:** During initial debugging, all transactions were rejected
with INVALID_SIGNATURE, so the polling endpoint was never reached. Once R64
was fixed, transactions submitted (HTTP 200 + hash returned) but confirmations
timed out — revealing this second bug.

**Fix status:**
- ✅ `setup_bridge.rs` — fixed to `/rpc/v3/transactions/{hash}?type=user`
- ❌ `crates/chain/src/client.rs:286` — still uses `/rpc/v1/transactions/by_hash/{hash}`

---

## R66: Transaction Status Polling — "Pending" State

**Severity:** Medium (premature failure on valid transactions)
**Affected:** `setup_bridge.rs` (fixed), `crates/chain/src/client.rs` (NOT YET FIXED)

Supra returns three status values:
- `"Pending"` — accepted but not committed (keep polling)
- `"Success"` — committed and executed successfully
- `"Fail"` — committed but Move execution failed

Additionally, Supra uses `"status": "Success"` (string), not Aptos-style
`"success": true` (bool). The `vm_status` lives at `output.Move.vm_status`.

**Fix status:**
- ✅ `setup_bridge.rs` — handles Pending, reads output.Move.vm_status
- ❌ `crates/chain/src/client.rs` — expects Aptos-style `"success": bool`

---

## R67: SupraTransaction Envelope

**Severity:** Critical for live trading (not yet hit — affects settlement)
**Affected:** `crates/chain/src/client.rs`, `crates/settlement/src/lib.rs`

The v3 submit endpoint expects the BCS payload wrapped in a SupraTransaction
enum: `ULEB128(variant) + BCS(SignedTransaction)`.

For Move transactions: variant = 1 → single byte `0x01` prefix.

```
Submission payload = 0x01 || BCS(SignedTransaction)
Content-Type: application/x.supra.signed_transaction+bcs
```

The settlement crate builds `SignedTransaction` BCS correctly but does NOT
prepend the 0x01 envelope byte. The chain client uses `application/x-bcs`
content type and v1 endpoint.

**Fix needed in chain client:**
1. Add SupraTransaction envelope (0x01 prefix)
2. Switch to v3 submit endpoint
3. Use correct content type: `application/x.supra.signed_transaction+bcs`

---

## R68: Contract Module Names

**Severity:** Medium (setup wizard called wrong function)
**Affected:** `setup_bridge.rs` (fixed)

```
WRONG:  pool::register(nft_id)
RIGHT:  escrow::register_trader(nft_id, holding_period_days, rushed_withdrawal_enabled)
```

The setup wizard was calling a non-existent `pool::register` module.
The actual entry function is `escrow::register_trader` with three arguments.
This caused `LINKER_ERROR` on-chain.

**Fix applied:** `submit_register()` now calls `escrow::register_trader`
with all three BCS-encoded arguments. Trait signature updated to pass
`holding_period_days: u64` and `rushed_withdrawal_enabled: bool`.

---

## R69: Expiration ≠ Signature Validation

**Severity:** Informational (debugging methodology)

When submitting expired transaction bytes via curl, Supra returned
"Transaction expiration timestamp in the past" instead of INVALID_SIGNATURE.
This was initially interpreted as "signature is valid, just expired."

**Correction (per Gemini analysis):** Supra's validation pipeline checks
expiration BEFORE signature. Cheap checks first, expensive crypto second.
An expiration error tells you nothing about signature validity.

This was a critical red herring that delayed finding R64 by several hours.

**Lesson:** Never assume validation order. The only way to test signature
validity is to submit a transaction that passes ALL other checks.

---

## R70: Chain Client — Outstanding Fixes for Live Trading

The `crates/chain/src/client.rs` SupraClient was written against Aptos
conventions. It will fail identically to how setup_bridge failed before
R64–R66 fixes. These must be fixed before the run loop goes live:

| Method | Current (broken) | Required |
|--------|-----------------|----------|
| `submit_raw()` | POST `/rpc/v1/transactions/submit` | POST `/rpc/v3/transactions/submit` |
| `submit_raw()` | `Content-Type: application/x-bcs` | `application/x.supra.signed_transaction+bcs` |
| `submit_raw()` | No SupraTransaction envelope | Prepend `0x01` byte |
| `submit_raw()` | Expects JSON `{"hash": "..."}` | Response is plain string `"0x..."` |
| `simulate_raw()` | POST `/rpc/v1/transactions/simulate` | POST `/rpc/v3/transactions/simulate` |
| `simulate_raw()` | Expects `"success": bool` | Check `"status": "Success"` string |
| `wait_for_tx()` | GET `/rpc/v1/transactions/by_hash/{hash}` | GET `/rpc/v3/transactions/{hash}?type=user` |
| `wait_for_tx()` | Expects `"success": bool` | Check `"status"` string, skip `"Pending"` |

**Settlement submitter** (`crates/settlement/src/lib.rs`):
- `build_signed_tx()` returns `BCS(SignedTransaction)` without envelope
- Must wrap: `0x01 || BCS(SignedTransaction)` before passing to `submit_raw()`

---

## Impact on Build Plan

### B5 Status: Partially Complete

The BUILD_PLAN.md marks B5 as "✅ COMPLETE" but the **run loop is a stub**.
`Command::Run` prints "Node ready" without starting any subsystems.

Proposed status: **B5 = ✅ COMPLETE (setup + subsystems) / ⚠️ run loop unwired**

### New Work Item: B5.0.1 — Testnet Wiring

| Task | Effort | Blocks |
|------|--------|--------|
| Fix chain client (R65, R66, R67, R70) | 1 session | Live trading |
| Wire run loop (poller → orchestrator → strategy WS → gossip) | 1-2 sessions | Live trading |
| Decrypt keystore in run path | Small | Run loop |
| SupraTransaction envelope in settlement | Small | Settlement |
| Rust 1.83+ upgrade for libp2p gossip | 1 session | Multi-node P2P |
| End-to-end: two nodes complete a batch cycle | 1 session | Validation |

### B6 Status: Unchanged

Batch Data API is independent and can proceed in parallel.

---

## Verified Supra-Specific Conventions

For reference — confirmed against live testnet and SDK source:

| Convention | Value |
|-----------|-------|
| Signing prefix | `SHA3-256("SUPRA::RawTransaction")` |
| Address derivation | `SHA3-256(ed25519_pubkey \|\| 0x00)` |
| Submit endpoint (BCS) | `POST /rpc/v3/transactions/submit` |
| Submit content type | `application/x.supra.signed_transaction+bcs` |
| Tx lookup | `GET /rpc/v3/transactions/{hash}?type=user` |
| BCS envelope | `ULEB128(1) + BCS(SignedTransaction)` for Move txns |
| Status field | `"status": "Success"/"Fail"/"Pending"` (string, not bool) |
| VM status | `output.Move.vm_status` (nested) |
| Chain ID (testnet) | 6 |
| Sequence number | `/rpc/v1/accounts/{addr}` → `sequence_number` |
| Ed25519 | Standard (not Ed25519ph), via `ed25519-dalek` or PyNaCl |
| Authenticator BCS | `ULEB128(0)` (Ed25519 variant) + length-prefixed pubkey + length-prefixed sig |
