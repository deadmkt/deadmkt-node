# SPDX-License-Identifier: MIT
"""
adaptive_wash.py — Adaptive wash-trade strategy for N-node testnet.

Each node deterministically picks BUY or SELL per pair per batch using
SHA256(batch_id : pair : nft_id).  No coordination protocol needed —
nodes independently take opposite sides and produce real cross-node
trades.  Volume scales inversely with peer count so total market
activity stays roughly constant.

Deterministic mid-price derived from SHA256 ensures all nodes agree
on pricing and orders always cross.

Balance-aware override prevents token drift: if a node's base token
drops below 30% of its (base+quote), it forces a BUY to rebalance.

Mount as strategy.py on every trading node:
  docker run ... -v ~/strategies/adaptive_wash.py:/data/strategy.py ...
"""

import hashlib
import time
import logging
from decimal import Decimal

log = logging.getLogger("adaptive_wash")

# ── Trading Config ────────────────────────────────────────────────────
PAIRS = [
    {"pair": "EMM/KAY", "base": "EMM", "quote": "KAY"},
    {"pair": "KAY/TEE", "base": "KAY", "quote": "TEE"},
    {"pair": "TEE/EMM", "base": "TEE", "quote": "EMM"},
]

BASE_PRICE = Decimal("0.05")
SPREAD = Decimal("0.003")
DRIFT_CLAMP = Decimal("0.05")
ALLOC_PCT = Decimal("0.04")
MIN_BALANCE = Decimal("1.0")
MIN_QTY = Decimal("1.0")
LOW_RATIO = Decimal("0.3")         # balance bias trigger

# ── Mint Pricing (from tokens.move) ───────────────────────────────────
SUPRA_PER_TOKEN = Decimal("0.1")
GAS_RESERVE = Decimal("5.0")
MIN_MINT_TOKENS = Decimal("10.0")
MINT_COOLDOWN_SECS = 300
ESCROW_THRESHOLD = Decimal("5.0")

# ── State ─────────────────────────────────────────────────────────────
_nft_id = None
_mint_sent_at = 0
_mint_pending = False


# ── Deterministic role assignment ─────────────────────────────────────
def _role_for(batch_id, pair_name, nft_id):
    """Deterministic BUY/SELL role from shared public data."""
    h = hashlib.sha256(f"{batch_id}:{pair_name}:{nft_id}".encode()).digest()
    return "buy" if h[0] % 2 == 0 else "sell"


def _deterministic_mid(batch_id, pair_name):
    """Deterministic mid-price all nodes agree on."""
    h = hashlib.sha256(f"price:{batch_id}:{pair_name}".encode()).digest()
    raw = int.from_bytes(h[:4], "big")
    frac = Decimal(raw) / Decimal(2**32)        # [0, 1)
    drift = DRIFT_CLAMP * (2 * frac - 1)        # [-DRIFT_CLAMP, +DRIFT_CLAMP]
    return BASE_PRICE * (1 + drift)


def _maybe_override_role(role, pair, escrow):
    """Override hash role if token balance is dangerously skewed."""
    base_bal = Decimal(escrow.get(pair["base"], "0"))
    quote_bal = Decimal(escrow.get(pair["quote"], "0"))
    total = base_bal + quote_bal
    if total == 0:
        return role
    ratio = base_bal / total
    if ratio < LOW_RATIO:
        return "buy"       # acquire base
    if ratio > (1 - LOW_RATIO):
        return "sell"      # sell base to acquire quote
    return role


# ── Mint helpers (from wash_trade.py) ─────────────────────────────────
def _needs_mint(escrow):
    for t in ["EMM", "KAY", "TEE"]:
        if Decimal(escrow.get(t, "0")) < ESCROW_THRESHOLD:
            return True
    return False


def _calc_mint_amount(gas_balance_str):
    gas = Decimal(gas_balance_str or "0")
    available = gas - GAS_RESERVE
    if available <= 0:
        return 0
    max_total = available / SUPRA_PER_TOKEN
    per_token = max_total / 3
    if per_token < MIN_MINT_TOKENS:
        return 0
    return int(per_token * 100000)


# ── Order building ────────────────────────────────────────────────────
def _build_orders(escrow, batch_id, peers_in_pool):
    n_nodes = max(1, peers_in_pool + 1)
    alloc = ALLOC_PCT / Decimal(n_nodes)
    orders = []

    for market in PAIRS:
        pair = market["pair"]
        base = market["base"]
        quote = market["quote"]
        base_bal = Decimal(escrow.get(base, "0"))
        quote_bal = Decimal(escrow.get(quote, "0"))

        role = _role_for(batch_id, pair, _nft_id)
        role = _maybe_override_role(role, market, escrow)
        mid = _deterministic_mid(batch_id, pair)

        log.info(f"Batch {batch_id}: {pair} → {role} "
                 f"(mid={mid:.8f}, n={n_nodes})")

        if role == "sell" and base_bal >= MIN_BALANCE:
            sell_price = mid * (Decimal("1") - SPREAD)
            qty = (base_bal * alloc).quantize(Decimal("0.00001"))
            if qty >= MIN_QTY:
                orders.append({
                    "pair": pair,
                    "side": "sell",
                    "price": str(sell_price.quantize(Decimal("0.00000001"))),
                    "quantity": str(qty),
                })

        elif role == "buy" and quote_bal >= MIN_BALANCE:
            buy_price = mid * (Decimal("1") + SPREAD)
            spend = quote_bal * alloc
            qty = (spend / buy_price).quantize(Decimal("0.00001"))
            if qty >= MIN_QTY:
                orders.append({
                    "pair": pair,
                    "side": "buy",
                    "price": str(buy_price.quantize(Decimal("0.00000001"))),
                    "quantity": str(qty),
                })

    return orders


# ── on_auth: capture nft_id ────────────────────────────────────────────
def on_auth(data):
    global _nft_id
    _nft_id = data.get("nft_id")
    log.info(f"Authenticated as NFT {_nft_id}")
    return []


# ── on_batch_start ────────────────────────────────────────────────────
def on_batch_start(ctx):
    global _mint_sent_at, _mint_pending

    escrow = ctx.get("escrow", {})
    mint_state = ctx.get("mint_state", {})
    gas_balance = ctx.get("gas_balance", "0")
    batch_id = ctx.get("batch_id", 0)
    peers_in_pool = ctx.get("peers_in_pool", 0)
    token_actions = []

    # ── Token management (non-blocking) ──

    # Claim pending mint if ready
    if mint_state.get("has_pending_mint", False):
        claimable_at = mint_state.get("pending_claimable_at", 0)
        now = int(time.time())
        if claimable_at > 0 and now >= claimable_at:
            log.info("Mint claimable — sending claim_mint")
            token_actions.append({"action": "claim_mint"})
            _mint_pending = False
        else:
            wait = claimable_at - now if claimable_at > 0 else "?"
            if int(now) % 60 == 0:
                log.info(f"Mint pending, claimable in {wait}s — trading continues")
    elif _needs_mint(escrow) and not _mint_pending:
        # Request mint if escrow low and no pending mint
        now = time.time()
        if (now - _mint_sent_at) > MINT_COOLDOWN_SECS:
            raw = _calc_mint_amount(gas_balance)
            if raw > 0:
                human = raw / 100000
                cost_supra = (raw * 3 * 100) / 100000000
                log.info(f"Escrow low — minting {human:.5f} each "
                         f"(cost ~{cost_supra:.2f} SUPRA, gas={gas_balance})")
                token_actions.append({"action": "mint", "m": raw, "k": raw, "t": raw})
                _mint_sent_at = now
                _mint_pending = True
            else:
                log.warning(f"Escrow low but can't afford mint (gas={gas_balance})")

    # ── Always trade with whatever escrow is available ──
    orders = _build_orders(escrow, batch_id, peers_in_pool)

    if token_actions:
        return {"orders": orders, "token_actions": token_actions}
    return orders


def on_reveal(ctx, my_commits):
    return list(range(len(my_commits)))


def on_match(ctx, matches):
    pass


def on_settlement(ctx, result):
    pass


def on_token_result(data):
    global _mint_pending
    action = data.get("data", {}).get("action", data.get("action", "?"))
    success = data.get("data", {}).get("success", data.get("success", False))
    message = data.get("data", {}).get("message", data.get("message", ""))
    if success:
        log.info(f"Token OK: {action} — {message}")
    else:
        log.warning(f"Token FAILED: {action} — {message}")
        if action == "mint":
            _mint_pending = False
