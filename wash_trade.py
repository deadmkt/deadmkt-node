# SPDX-License-Identifier: MIT
"""
wash_trade.py — Continuous wash-trade strategy for two-node testnet.

Both nodes run this SAME file. Every batch, each node places a BUY and
SELL on all 3 pairs at prices guaranteed to cross with the other node.

B5_9.1: Auto-mint when escrow is empty. Reads gas_balance from BatchStart,
calculates affordable mint amount based on on-chain pricing:
  SUPRA_PER_TOKEN_UNIT = 100 (contract constant)
  1 token (100000 raw) costs 0.1 SUPRA
  cost = (m + k + t) * 100 raw SUPRA

Mount as strategy.py on both Node1 and Node2:
  docker run ... -v ~/wash_trade.py:/data/strategy.py ...
"""

import random
import time
import logging
from decimal import Decimal

log = logging.getLogger("wash_trade")

# ── Trading Config ────────────────────────────────────────────────────
PAIRS = [
    {"pair": "EMM/KAY", "base": "EMM", "quote": "KAY"},
    {"pair": "KAY/TEE", "base": "KAY", "quote": "TEE"},
    {"pair": "TEE/EMM", "base": "TEE", "quote": "EMM"},
]

BASE_PRICE = Decimal("0.05")
SPREAD = Decimal("0.003")
DRIFT_MAX = Decimal("0.004")
DRIFT_CLAMP = Decimal("0.05")
ALLOC_PCT = Decimal("0.005")
MIN_BALANCE = Decimal("1.0")
MIN_QTY = Decimal("0.00001")

# ── Mint Pricing (from tokens.move) ───────────────────────────────────
# SUPRA_PER_TOKEN_UNIT = 100 raw SUPRA per raw token unit
# Token has 5 decimals, SUPRA has 8 decimals
# 1.00000 token = 100000 raw → costs 100000 * 100 = 10_000_000 raw SUPRA = 0.1 SUPRA
SUPRA_PER_TOKEN = Decimal("0.1")     # cost per 1.0 token in SUPRA
GAS_RESERVE = Decimal("5.0")         # keep 5 SUPRA for gas (deposits, heartbeats, settlements)
MIN_MINT_TOKENS = Decimal("10.0")    # don't bother minting less than 10 tokens each
MINT_COOLDOWN_SECS = 300             # don't re-mint within 5 min
ESCROW_THRESHOLD = Decimal("5.0")    # trigger mint if ANY token below this

# ── State ─────────────────────────────────────────────────────────────
_mid = {}
_mint_sent_at = 0
_mint_pending = False
_claim_sent = False
_funded = False


# ── Price drift ───────────────────────────────────────────────────────
def _get_mid(pair_name):
    if pair_name not in _mid:
        _mid[pair_name] = BASE_PRICE
    drift = Decimal(str(random.uniform(-float(DRIFT_MAX), float(DRIFT_MAX))))
    _mid[pair_name] = _mid[pair_name] * (Decimal("1") + drift)
    lower = BASE_PRICE * (Decimal("1") - DRIFT_CLAMP)
    upper = BASE_PRICE * (Decimal("1") + DRIFT_CLAMP)
    _mid[pair_name] = max(lower, min(upper, _mid[pair_name]))
    return _mid[pair_name]


def _needs_mint(escrow):
    for t in ["EMM", "KAY", "TEE"]:
        if Decimal(escrow.get(t, "0")) < ESCROW_THRESHOLD:
            return True
    return False


def _calc_mint_amount(gas_balance_str):
    """Calculate affordable equal mint amount from SUPRA balance.
    Returns raw token amount per symbol, or 0 if can't afford."""
    gas = Decimal(gas_balance_str or "0")
    available = gas - GAS_RESERVE
    if available <= 0:
        return 0

    # Total tokens we can afford across all 3
    max_total = available / SUPRA_PER_TOKEN
    per_token = max_total / 3

    if per_token < MIN_MINT_TOKENS:
        return 0

    # Convert to raw (5 decimals) and floor
    raw = int(per_token * 100000)
    return raw


def _build_orders(escrow):
    orders = []
    for market in PAIRS:
        pair = market["pair"]
        base = market["base"]
        quote = market["quote"]
        base_bal = Decimal(escrow.get(base, "0"))
        quote_bal = Decimal(escrow.get(quote, "0"))
        mid = _get_mid(pair)

        if base_bal >= MIN_BALANCE:
            sell_price = mid * (Decimal("1") - SPREAD)
            qty = (base_bal * ALLOC_PCT).quantize(Decimal("0.00001"))
            if qty >= MIN_QTY:
                orders.append({
                    "pair": pair,
                    "side": "sell",
                    "price": str(sell_price.quantize(Decimal("0.00000001"))),
                    "quantity": str(qty),
                })

        if quote_bal >= MIN_BALANCE:
            buy_price = mid * (Decimal("1") + SPREAD)
            spend = quote_bal * ALLOC_PCT
            qty = (spend / buy_price).quantize(Decimal("0.00001"))
            if qty >= MIN_QTY:
                orders.append({
                    "pair": pair,
                    "side": "buy",
                    "price": str(buy_price.quantize(Decimal("0.00000001"))),
                    "quantity": str(qty),
                })
    return orders


# ── on_auth: fire mint immediately if we can ──────────────────────────
# Backup path — fires before we have gas_balance info.
# Uses a conservative fixed amount. on_batch_start is preferred.

def on_auth(data):
    global _mint_sent_at, _mint_pending
    if _funded or _mint_pending:
        return []
    now = time.time()
    if (now - _mint_sent_at) < MINT_COOLDOWN_SECS:
        return []

    # Conservative blind mint: 50 tokens each = 15 SUPRA total
    # Safe for any trustee with 20+ SUPRA
    amount = 5000000  # 50.00000 raw
    log.info(f"on_auth: blind mint {amount} each (15 SUPRA) — no gas_balance yet")
    _mint_sent_at = now
    _mint_pending = True
    return [{"action": "mint", "m": amount, "k": amount, "t": amount}]


# ── on_batch_start ────────────────────────────────────────────────────

def on_batch_start(ctx):
    global _mint_sent_at, _mint_pending, _claim_sent, _funded

    escrow = ctx.get("escrow", {})
    mint_state = ctx.get("mint_state", {})
    gas_balance = ctx.get("gas_balance", "0")
    token_actions = []

    # Check if escrow is funded
    total = sum(Decimal(escrow.get(t, "0")) for t in ["EMM", "KAY", "TEE"])
    if total >= MIN_BALANCE * 3:
        _funded = True
        _mint_pending = False
        _claim_sent = False

    # ── Claim pending mint ──
    if mint_state.get("has_pending_mint", False):
        claimable_at = mint_state.get("pending_claimable_at", 0)
        now = int(time.time())
        if claimable_at > 0 and now >= claimable_at:
            log.info("Mint claimable — sending claim_mint")
            token_actions.append({"action": "claim_mint"})
            _claim_sent = True
            return {"orders": [], "token_actions": token_actions}
        else:
            wait = claimable_at - now if claimable_at > 0 else "?"
            if int(now) % 30 == 0:
                log.info(f"Mint pending, claimable in {wait}s")
            return []

    # ── Request mint if needed ──
    if _needs_mint(escrow) and not _mint_pending:
        now = time.time()
        if (now - _mint_sent_at) > MINT_COOLDOWN_SECS:
            raw = _calc_mint_amount(gas_balance)
            if raw > 0:
                human = raw / 100000
                cost_supra = (raw * 3 * 100) / 100000000  # raw SUPRA / 10^8
                log.info(f"Escrow low — minting {human:.5f} each "
                         f"(cost ~{cost_supra:.2f} SUPRA, gas_balance={gas_balance})")
                token_actions.append({"action": "mint", "m": raw, "k": raw, "t": raw})
                _mint_sent_at = now
                _mint_pending = True
                return {"orders": [], "token_actions": token_actions}
            else:
                log.warning(f"Escrow low but can't afford mint (gas_balance={gas_balance})")
        return []

    # ── Not funded yet, just wait ──
    if not _funded:
        return []

    # ── Normal trading ──
    return _build_orders(escrow)


def on_reveal(ctx, my_commits):
    return list(range(len(my_commits)))


def on_match(ctx, matches):
    pass


def on_settlement(ctx, result):
    pass


def on_token_result(data):
    global _mint_pending, _funded
    action = data.get("data", {}).get("action", data.get("action", "?"))
    success = data.get("data", {}).get("success", data.get("success", False))
    message = data.get("data", {}).get("message", data.get("message", ""))
    if success:
        log.info(f"Token OK: {action} — {message}")
        if action == "auto_deposit":
            _funded = True
    else:
        log.warning(f"Token FAILED: {action} — {message}")
        if action == "mint":
            _mint_pending = False
