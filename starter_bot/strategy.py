# SPDX-License-Identifier: MIT
"""
starter_bot/strategy.py
Default market-making strategy — works out of the box.
Places buy and sell limit orders around a midpoint price.
Not designed to be profitable — designed to demonstrate the system.

WARNING: This bot WILL lose money on testnet. It exists purely to
verify the full pipeline: strategy → WS → node → chain.
"""

from decimal import Decimal

# Constants
BUY = "buy"
SELL = "sell"
DEFAULT_PAIR = "EMM/KAY"
DEFAULT_PRICE_TOLERANCE = Decimal("0.002")  # 0.2% — how far from midpoint to place limit prices
DEFAULT_ALLOCATION_PCT = Decimal("0.01")  # 1% of balance per order
DEFAULT_PRICE = Decimal("0.05")  # Fallback price if no history


def get_last_price(ctx):
    """Extract last clearing price from batch context."""
    last_batch = ctx.get("last_batch")
    if last_batch and last_batch.get("matches", 0) > 0:
        # In real impl: would track last clearing price from settlement events.
        # For starter bot: use a simple default.
        pass
    return DEFAULT_PRICE


def compute_quantity(ctx, side, price):
    """Compute order quantity based on projected balance and allocation."""
    if side == BUY:
        # Buy: spending quote token (KAY)
        balance_str = ctx.get("escrow", {}).get("KAY", "0")
        balance = Decimal(balance_str)
        if balance <= 0 or price <= 0:
            return Decimal("0")
        # Allocate DEFAULT_ALLOCATION_PCT of balance
        spend = balance * DEFAULT_ALLOCATION_PCT
        qty = spend / price
        return qty.quantize(Decimal("0.00001"))
    else:
        # Sell: spending base token (EMM)
        balance_str = ctx.get("escrow", {}).get("EMM", "0")
        balance = Decimal(balance_str)
        if balance <= 0:
            return Decimal("0")
        qty = balance * DEFAULT_ALLOCATION_PCT
        return qty.quantize(Decimal("0.00001"))


def has_quote_balance(ctx):
    """Check if we have KAY to buy with."""
    balance_str = ctx.get("escrow", {}).get("KAY", "0")
    return Decimal(balance_str) > Decimal("0.01")


def has_base_balance(ctx):
    """Check if we have EMM to sell."""
    balance_str = ctx.get("escrow", {}).get("EMM", "0")
    return Decimal(balance_str) > Decimal("1")


def on_batch_start(ctx):
    """Place buy/sell orders around fair value estimate.

    Settlement price is always the midpoint of crossing prices —
    no one pays a spread. The tolerance just controls how far
    from your estimate you're willing to trade.

    Returns list of order dicts suitable for WS commit action.
    """
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
    """Reveal all — no selective reveal."""
    return list(range(len(my_commits)))


def on_match(ctx, matches):
    """No action on match — informational only."""
    pass


def on_settlement(ctx, result):
    """No action on settlement — informational only."""
    pass
