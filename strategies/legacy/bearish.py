# SPDX-License-Identifier: MIT
"""
strategies/bearish.py
Bearish strategy — lower midpoint than default.
Sell orders cross with bullish strategy's buy orders.

  buy  at 0.049 * 0.998 = 0.048902
  sell at 0.049 * 1.002 = 0.049098

vs bullish:
  buy  at 0.051 * 0.998 = 0.050898

  bullish buy (0.050898) > bearish sell (0.049098) → CROSS ✓

Settlement price: midpoint of crossing prices = (0.050898 + 0.049098) / 2 = 0.049998
Both parties get the fairest match price — no one pays a spread.
"""

from decimal import Decimal

BUY = "buy"
SELL = "sell"
DEFAULT_PAIR = "EMM/KAY"
DEFAULT_PRICE_TOLERANCE = Decimal("0.002")  # 0.2% — how far from midpoint to place limit prices
DEFAULT_ALLOCATION_PCT = Decimal("0.01") # 1% of balance per order
DEFAULT_PRICE = Decimal("0.049")         # Bearish: below 0.05 midpoint


def get_last_price(ctx):
    last_batch = ctx.get("last_batch")
    if last_batch and last_batch.get("matches", 0) > 0:
        pass
    return DEFAULT_PRICE


def compute_quantity(ctx, side, price):
    if side == BUY:
        balance_str = ctx.get("escrow", {}).get("KAY", "0")
        balance = Decimal(balance_str)
        if balance <= 0 or price <= 0:
            return Decimal("0")
        spend = balance * DEFAULT_ALLOCATION_PCT
        qty = spend / price
        return qty.quantize(Decimal("0.00001"))
    else:
        balance_str = ctx.get("escrow", {}).get("EMM", "0")
        balance = Decimal(balance_str)
        if balance <= 0:
            return Decimal("0")
        qty = balance * DEFAULT_ALLOCATION_PCT
        return qty.quantize(Decimal("0.00001"))


def has_quote_balance(ctx):
    balance_str = ctx.get("escrow", {}).get("KAY", "0")
    return Decimal(balance_str) > Decimal("0.01")


def has_base_balance(ctx):
    balance_str = ctx.get("escrow", {}).get("EMM", "0")
    return Decimal(balance_str) > Decimal("1")


def on_batch_start(ctx):
    last_price = get_last_price(ctx)
    tolerance = DEFAULT_PRICE_TOLERANCE

    buy_price = last_price * (1 - tolerance)
    sell_price = last_price * (1 + tolerance)

    orders = []

    if has_quote_balance(ctx):
        qty = compute_quantity(ctx, BUY, buy_price)
        if qty > 0:
            orders.append({
                "pair": DEFAULT_PAIR,
                "side": BUY,
                "price": str(buy_price.quantize(Decimal("0.00000001"))),
                "quantity": str(qty),
            })

    if has_base_balance(ctx):
        qty = compute_quantity(ctx, SELL, sell_price)
        if qty > 0:
            orders.append({
                "pair": DEFAULT_PAIR,
                "side": SELL,
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
