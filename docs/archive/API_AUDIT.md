# Supra RPC API Audit — Code vs Documentation

**Date:** 2026-02-24
**Docs source:** https://docs.supra.com/network/move/rest-api/testnet/

---

## Summary

| # | Endpoint in Code | API Version | Status | Severity |
|---|---|---|---|---|
| 1 | `GET /rpc/v2/block` | v2 | ✅ OK | — |
| 2 | `POST /rpc/v1/view` | v1 (deprecated) | ⚠️ DEPRECATED | Medium |
| 3 | `GET /rpc/v1/accounts/{addr}` | v1 (deprecated) | ❌ BROKEN | Critical |
| 4 | `POST /rpc/v3/transactions/submit` | v3 | ✅ OK | — |
| 5 | `POST /rpc/v3/transactions/simulate` | v3 | ✅ OK | — |
| 6 | `GET /rpc/v3/transactions/{hash}` | v3 | ✅ OK | — |
| 7 | `GET /rpc/v1/accounts/{addr}/events/{n}` | ❌ NONEXISTENT | ❌ WRONG | Critical |

---

## 1. `get_ledger_info()` — ✅ OK

**Code:** `GET /rpc/v2/block`
**Docs:** `GET /rpc/v2/block` → `{"height": int, "timestamp": {"timestamp": int}, "author": "...", ...}`

**Code parses:** `height` (int or string), `timestamp.timestamp` (int or string)

**Verdict:** Fixed this session. Matches docs perfectly.

---

## 2. `view_raw()` — ⚠️ DEPRECATED, LIKELY TO BREAK

**Code:** `POST /rpc/v1/view`
**Docs:** v1 is **deprecated**. v2 exists at `POST /rpc/v2/view`. v3 at `POST /rpc/v3/view`.

### Request body (all versions are the same):
```json
{"function": "0x1::module::func", "type_arguments": [], "arguments": []}
```
Code sends this correctly.

### Response format differs by version:

| Version | Response format |
|---------|----------------|
| v1 (deprecated) | Unknown/deprecated MoveValue — likely raw strings: `["10", "4", "3"]` |
| v2 | `{"result": [{"U64": "10"}, {"U64": "4"}]}` — values wrapped in type tags |
| v3 | Same as v2: `{"result": [{"U64": "10"}]}` |

**Code expects:** `json.get("result")` → raw array like `["10", "4", "3"]`

**Problem:** If we upgrade to v2/v3, the response wraps values in type tags (`{"U64": "10"}`) which would break `BatchParams::from_view_result()` and every other view parser. The v1 likely still returns raw strings, but it's deprecated and could be removed.

**Fix needed:** Either stay on v1 (risky) or upgrade to v2/v3 and unwrap type tags in `view_raw()`.

**Files affected:** `client.rs:116`, `setup_bridge.rs:424`, plus all 6 view mock tests

---

## 3. `get_account()` — ❌ CRITICAL: Will fail on live API

**Code:** `GET /rpc/v1/accounts/{address}`
**Docs:** v1 is **deprecated**. v2 exists at `GET /rpc/v2/accounts/{address}`.

### Response format:
```json
// v1 (deprecated): nullable, fields may differ
null | {"auth_key": "...", "sequence_number": ...}

// v2: guaranteed object
{"sequence_number": 1, "authentication_key": "text"}

// v3: same as v2
{"sequence_number": 1, "authentication_key": "text"}
```

### Bug 1: `sequence_number` type mismatch
Code does:
```rust
json.get("sequence_number").and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok())
```
But v2/v3 return `sequence_number` as **integer** (`1`), not string (`"1"`).
`.as_str()` returns `None` on integers → parse fails → `DeserializationError`.

### Bug 2: v1 may return `null`
v1 docs say response is `null | object`. Code doesn't handle null.

### Bug 3: Field name mismatch
v1 docs reference `auth_key`, code reads `authentication_key` (which is the v2 name).
Not critical since we only use it for display, but it's wrong for v1.

**Fix needed:** Upgrade to v2, parse `sequence_number` as int OR string.

**Files affected:** `client.rs:202-227`, `setup_bridge.rs:172`

---

## 4. `submit_raw()` — ✅ OK

**Code:** `POST /rpc/v3/transactions/submit`
**Content-Type:** `application/x.supra.signed_transaction+bcs`

**Docs:** v3 accepts both `application/json` and `application/x.supra.signed_transaction+bcs`.

Code sends BCS with 0x01 prefix (Move variant). Response is hex hash string.

**Verdict:** Matches docs.

---

## 5. `simulate_raw()` — ✅ OK

**Code:** `POST /rpc/v3/transactions/simulate`
**Content-Type:** `application/x.supra.signed_transaction+bcs`

**Docs:** v3 accepts both JSON and BCS for simulate.

Code sends BCS with 0x01 prefix. Parses `status`, `output.Move.gas_used`, `output.Move.vm_status`.

**Note:** Docs example response shows `"output": {"Dkg": "Success"}` which doesn't match `output.Move.gas_used`. The actual simulate response for Move transactions likely has a different shape than the docs example (which seems to show a DKG transaction). May need runtime testing to verify exact field paths, but endpoint and content-type are correct.

**Verdict:** Endpoint correct. Response parsing may need tuning.

---

## 6. `wait_for_tx()` — ✅ OK

**Code:** `GET /rpc/v3/transactions/{hash}?type=user`

**Docs:** `GET /rpc/v3/transactions/{hash}?type=user` — returns transaction info.

Code strips `0x` prefix from hash, appends `?type=user`. Parses `status` field.

**Verdict:** Matches docs.

---

## 7. `get_events()` — ❌ CRITICAL: Endpoint doesn't exist

**Code:** `GET /rpc/v1/accounts/{address}/events/{creation_number}?start={seq}&limit={n}`

**Docs:** This endpoint **does not exist** in any Supra API version. This is an Aptos-style event endpoint.

### Supra's actual events API:
```
// v1:
GET /rpc/v1/events/{event_type}?start={block_height}&end={block_height}
// event_type = "0xADDR::module::EventStructName"
// start/end = block height range (max 10 blocks)

// v3:
GET /rpc/v3/events/{event_type}?start_height={}&end_height={}&limit={}
// More flexible, supports cursor pagination
```

### Response format:
```json
// v1:
{"data": [{"guid": "...", "sequence_number": 1, "type": "...", "data": {...}}]}

// v3:
{"data": [{"event": {"guid": "...", "sequence_number": 1, "type": "...", "data": {...}}, "block_height": 1, "transaction_hash": "..."}]}
```

### What needs to change:
- Query by **event type string** (e.g. `0xCONTRACT::settlement::BatchTradeSettled`), not by account address
- Use **block height range**, not sequence number + limit
- Parse the `data` wrapper in response
- Probably use v3 for cursor pagination support

**Files affected:** `client.rs:383-407`, `run.rs` event poller loop, `ChainEvent::from_json()` parser

---

## Also in `setup_bridge.rs` (same issues):

| Endpoint | Status |
|----------|--------|
| `GET /rpc/v1/accounts/{addr}` | ❌ Same as #3 — seq_number type mismatch |
| `POST /rpc/v3/transactions/submit` | ✅ OK |
| `GET /rpc/v3/transactions/{hash}?type=user` | ✅ OK |
| `POST /rpc/v1/view` | ⚠️ Same as #2 — deprecated |

---

## Fix Priority

### P0 — Will crash on startup:
1. **`get_account()`** → Upgrade to v2, parse `sequence_number` as int or string
2. **`get_events()`** → Rewrite to use `/rpc/v3/events/{event_type}` with block height range

### P1 — Works now but deprecated (will break eventually):
3. **`view_raw()`** → Decide: stay v1 (risk removal) or upgrade to v2 (must unwrap type tags)

### P2 — Cosmetic:
4. Update test mocks that still reference v1 account endpoints
