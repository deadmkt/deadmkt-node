# SPDX-License-Identifier: MIT
"""
adaptive_wash.py — Adaptive wash-trade strategy for N-node testnet.

Each node deterministically picks BUY or SELL per pair per batch using
SHA256(batch_id : pair : nft_id : epoch).  No coordination protocol
needed — nodes independently take opposite sides and produce real
cross-node trades.

v5 changes:
- Fixed 1-token order quantity (min_trade_quantity) instead of % allocation
- Removed emergency mode — triangle rebalancing + mint handles recovery
- Simplified balance checks: can I afford 1 token? If not, skip.
- Designed for long-running wash trading at minimum volume

v4 changes:
- Per-pair base prices: EMM/KAY=0.05, KAY/TEE=1.0, TEE/EMM=20.0
- Read min_trade_quantity from batch_start instead of hardcoded MIN_QTY

v3.1 changes:
- REMOVED per-pair LOW_RATIO override (caused market freezes)

v3 changes:
- Epoch rotation every 50 batches for faster bias cycling
- Triangle rebalancing at forced buy when token < 60% of average

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

# Which pair to buy to acquire a specific token
ACQUIRE_PAIR = {
    "EMM": "EMM/KAY",
    "KAY": "KAY/TEE",
    "TEE": "TEE/EMM",
}

PAIR_BASE_PRICES = {
    "EMM/KAY": Decimal("0.05"),    # 1 EMM costs 0.05 KAY
    "KAY/TEE": Decimal("1.0"),     # 1 KAY costs 1.0 TEE
    "TEE/EMM": Decimal("20.0"),    # 1 TEE costs 20.0 EMM
}
SPREAD = Decimal("0.003")
DRIFT_CLAMP = Decimal("0.05")

# ── Triangle Rebalancing ──────────────────────────────────────────────
REBALANCE_THRESHOLD = Decimal("0.6")   # force buy if token < 60% of avg

# ── Epoch rotation ────────────────────────────────────────────────────
EPOCH_LENGTH = 50                      # batches per epoch

# ── Mint Config ──────────────────────────────────────────────────────
SUPRA_PER_TOKEN = Decimal("0.1")
GAS_RESERVE = Decimal("5.0")
MIN_MINT_TOKENS = Decimal("10.0")
MINT_COOLDOWN_SECS = 300
ESCROW_THRESHOLD = Decimal("5.0")

# ── State ─────────────────────────────────────────────────────────────
_nft_id = None
_mint_sent_at = 0
_mint_pending = False
_min_qty = Decimal("1.00000")          # updated from batch_start min_trade_quantity


# ── Deterministic role assignment (with epoch rotation) ───────────────
def _role_for(batch_id, pair_name, nft_id):
    """Deterministic BUY/SELL role from shared public data."""
    epoch = batch_id // EPOCH_LENGTH
    h = hashlib.sha256(f"{batch_id}:{pair_name}:{nft_id}:{epoch}".encode()).digest()
    return "buy" if h[0] % 2 == 0 else "sell"


def _deterministic_mid(batch_id, pair_name):
    """Deterministic mid-price all nodes agree on (per-pair base price)."""
    base = PAIR_BASE_PRICES.get(pair_name, Decimal("0.05"))
    h = hashlib.sha256(f"price:{batch_id}:{pair_name}".encode()).digest()
    raw = int.from_bytes(h[:4], "big")
    frac = Decimal(raw) / Decimal(2**32)
    drift = DRIFT_CLAMP * (2 * frac - 1)
    return base * (1 + drift)


# ── Triangle rebalancing ─────────────────────────────────────────────
def _find_depleted_token(escrow):
    """Check if any token is critically below the triangle average."""
    emm = Decimal(escrow.get("EMM", "0"))
    kay = Decimal(escrow.get("KAY", "0"))
    tee = Decimal(escrow.get("TEE", "0"))
    total = emm + kay + tee
    if total == 0:
        return None
    avg = total / 3
    threshold = avg * REBALANCE_THRESHOLD
    candidates = []
    if emm < threshold:
        candidates.append(("EMM", emm))
    if kay < threshold:
        candidates.append(("KAY", kay))
    if tee < threshold:
        candidates.append(("TEE", tee))
    if not candidates:
        return None
    candidates.sort(key=lambda x: x[1])
    return candidates[0][0]


# ── Mint helpers ──────────────────────────────────────────────────────
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
    raw = int(per_token * 100000)
    # Round down to nearest 1_000_000 (whole token) to satisfy
    # E_TOTAL_NOT_DIVISIBLE_BY_10 contract constraint
    raw = (raw // 1_000_000) * 1_000_000
    if raw == 0:
        return 0
    return raw


# ── Order building ────────────────────────────────────────────────────
def _build_orders(escrow, batch_id):
    orders = []

    # ── Triangle rebalancing ──
    depleted = _find_depleted_token(escrow)
    force_buy_pair = None
    if depleted:
        force_buy_pair = ACQUIRE_PAIR[depleted]
        log.info(f"Batch {batch_id}: REBALANCE — {depleted} depleted, "
                 f"forcing buy on {force_buy_pair}")

    # ── Fixed 1-token orders on each pair ──
    for market in PAIRS:
        pair = market["pair"]
        base = market["base"]
        quote = market["quote"]
        base_bal = Decimal(escrow.get(base, "0"))
        quote_bal = Decimal(escrow.get(quote, "0"))

        if pair == force_buy_pair:
            role = "buy"
        else:
            role = _role_for(batch_id, pair, _nft_id)

        mid = _deterministic_mid(batch_id, pair)
        qty = _min_qty

        if role == "sell":
            sell_price = mid * (Decimal("1") - SPREAD)
            if base_bal >= qty:
                orders.append({
                    "pair": pair,
                    "side": "sell",
                    "price": str(sell_price.quantize(Decimal("0.00000001"))),
                    "quantity": str(qty.quantize(Decimal("0.00001"))),
                })
            else:
                log.info(f"Batch {batch_id}: {pair} SKIP sell — "
                         f"{base} bal {base_bal:.2f} < {qty}")

        elif role == "buy":
            buy_price = mid * (Decimal("1") + SPREAD)
            cost = qty * buy_price
            if quote_bal >= cost:
                orders.append({
                    "pair": pair,
                    "side": "buy",
                    "price": str(buy_price.quantize(Decimal("0.00000001"))),
                    "quantity": str(qty.quantize(Decimal("0.00001"))),
                })
            else:
                log.info(f"Batch {batch_id}: {pair} SKIP buy — "
                         f"{quote} bal {quote_bal:.2f} < cost {cost:.2f}")

    return orders


# ── on_auth ───────────────────────────────────────────────────────────
def on_auth(data):
    global _nft_id
    _nft_id = data.get("nft_id")
    log.info(f"Authenticated as NFT {_nft_id}")
    return []


# ── on_batch_start ────────────────────────────────────────────────────
def on_batch_start(ctx):
    global _mint_sent_at, _mint_pending, _min_qty

    escrow = ctx.get("escrow", {})
    mint_state = ctx.get("mint_state", {})
    gas_balance = ctx.get("gas_balance", "0")
    batch_id = ctx.get("batch_id", 0)
    token_actions = []

    # Read protocol minimum from node (DMKT11+)
    # Node sends raw units (100000 = 1.0 token, 5 decimals)
    min_trade_str = ctx.get("min_trade_quantity", "")
    if min_trade_str:
        try:
            raw_min = Decimal(min_trade_str)
            if raw_min > 0:
                _min_qty = raw_min / Decimal("100000")
        except Exception:
            pass

    # ── Token management (non-blocking) ──
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

    # ── Trade at fixed 1-token volume ──
    orders = _build_orders(escrow, batch_id)

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
