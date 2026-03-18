# SPDX-License-Identifier: MIT
"""
strategies/clockwise.py
Circular market-maker: sells around all three pairs in one direction.

Flow: EMM -> KAY -> TEE -> EMM (clockwise)
  EMM/KAY: SELL EMM for KAY
  KAY/TEE: SELL KAY for TEE
  TEE/EMM: SELL TEE for EMM

Designed to run against counter_clockwise.py on another node.
Each batch, the sell price drifts randomly by up to 0.5% from a
drifting midpoint. Prices clamp within 3% of 0.05 base to stay
in crossing range. Creates visible chart movement.

Order sizes are 0.5% of the relevant balance per batch.
"""

import random
from decimal import Decimal

PAIRS = [
    {"pair": "EMM/KAY", "side": "sell", "base": "EMM", "quote": "KAY"},
    {"pair": "KAY/TEE", "side": "sell", "base": "KAY", "quote": "TEE"},
    {"pair": "TEE/EMM", "side": "sell", "base": "TEE", "quote": "EMM"},
]

BASE_PRICE = Decimal("0.05")
SPREAD = Decimal("0.001")       # 0.1% below midpoint for sells
DRIFT_MAX = Decimal("0.005")    # up to 0.5% drift per pair per batch
ALLOCATION_PCT = Decimal("0.005")
MIN_BALANCE = Decimal("1.0")
MIN_QTY = Decimal("0.00001")

# Per-pair drifting midpoint (persists across batches within session)
_pair_mid = {}


def _get_mid(pair_name):
    """Random walk the midpoint for a pair. Clamped within 3% of base."""
    if pair_name not in _pair_mid:
        _pair_mid[pair_name] = BASE_PRICE

    drift_pct = Decimal(str(random.uniform(-float(DRIFT_MAX), float(DRIFT_MAX))))
    _pair_mid[pair_name] = _pair_mid[pair_name] * (1 + drift_pct)

    lower = BASE_PRICE * Decimal("0.97")
    upper = BASE_PRICE * Decimal("1.03")
    if _pair_mid[pair_name] < lower:
        _pair_mid[pair_name] = lower
    elif _pair_mid[pair_name] > upper:
        _pair_mid[pair_name] = upper

    return _pair_mid[pair_name]


def on_batch_start(ctx):
    escrow = ctx.get("escrow", {})
    orders = []

    for market in PAIRS:
        base_balance = Decimal(escrow.get(market["base"], "0"))
        if base_balance < MIN_BALANCE:
            continue

        mid = _get_mid(market["pair"])
        sell_price = mid * (1 - SPREAD)

        qty = (base_balance * ALLOCATION_PCT).quantize(Decimal("0.00001"))
        if qty < MIN_QTY:
            continue

        orders.append({
            "pair": market["pair"],
            "side": market["side"],
            "price": str(sell_price.quantize(Decimal("0.00000001"))),
            "quantity": str(qty),
        })

    return orders


def on_reveal(ctx, my_commits):
    return list(range(len(my_commits)))


def on_match(ctx, matches):
    pass


def on_settlement(ctx, result):
    pass
