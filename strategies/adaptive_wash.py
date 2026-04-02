# SPDX-License-Identifier: MIT
"""
adaptive_wash.py — Adaptive wash-trade strategy for N-node testnet.

Each node deterministically picks BUY or SELL per pair per batch using
SHA256(batch_id : pair : nft_id : epoch).  No coordination protocol
needed — nodes independently take opposite sides and produce real
cross-node trades.

v4 changes:
- Per-pair base prices: EMM/KAY=0.05, KAY/TEE=1.0, TEE/EMM=20.0
  (was single BASE_PRICE=0.05 for all pairs — wrong by 20-400x on 2 pairs)
- Read min_trade_quantity from batch_start instead of hardcoded MIN_QTY
- Fallback to hardcoded MIN_QTY if node doesn't provide min_trade_quantity

v3.1 changes:
- REMOVED per-pair LOW_RATIO override entirely. It was synchronizing
  all nodes to the same side when system-wide drift created similar
  balance patterns (e.g. all nodes low EMM relative to KAY), causing
  complete market freezes with zero matches. Triangle rebalancer +
  emergency mode handle drift without synchronizing.

v3 changes:
- Fixed allocation math: flat 2% ALLOC_PCT, not divided by node count.
- Lowered MIN_QTY from 1.0 to 0.1 for tiny recovery orders.
- Triangle rebalancing at 4% alloc for faster correction.
- Epoch rotation every 50 batches for faster bias cycling.
- Emergency mode: if any token < 1.0, focus all trading on recovery.

v2 changes:
- Rotating epoch prevents persistent directional bias per NFT ID.
- Global triangle rebalancing checks all three tokens together.

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

# Which pair to sell to dump a specific token
SELL_PAIR = {
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
ALLOC_PCT = Decimal("0.02")           # flat 2% — NOT divided by node count
MIN_BALANCE = Decimal("0.1")           # lowered from 1.0
MIN_QTY = Decimal("0.10000")          # fallback if node doesn't provide min_trade_quantity
# Per-pair override REMOVED in v3.1 — it synchronized all nodes to the
# same side when system-wide drift created similar balance patterns,
# causing complete market freezes. Triangle rebalancer + emergency mode
# handle drift correction without synchronizing nodes.

# ── Triangle Rebalancing ──────────────────────────────────────────────
REBALANCE_THRESHOLD = Decimal("0.6")   # force buy if token < 60% of avg
REBALANCE_ALLOC = Decimal("0.04")      # 4% for rebalancing (2x normal)

# ── Emergency mode ────────────────────────────────────────────────────
EMERGENCY_THRESHOLD = Decimal("1.0")   # if any token < 1.0, enter emergency mode
EMERGENCY_ALLOC = Decimal("0.08")      # 8% emergency allocation

# ── Epoch rotation ────────────────────────────────────────────────────
EPOCH_LENGTH = 50                      # batches per epoch (was 100)

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
_min_qty = MIN_QTY                     # updated from batch_start min_trade_quantity


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


# _maybe_override_role REMOVED in v3.1 — see note above


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


def _find_emergency(escrow):
    """Check if any token is critically low (< 1.0).
    Returns (depleted_symbol, healthiest_symbol) or None."""
    balances = {
        "EMM": Decimal(escrow.get("EMM", "0")),
        "KAY": Decimal(escrow.get("KAY", "0")),
        "TEE": Decimal(escrow.get("TEE", "0")),
    }
    depleted = [(s, b) for s, b in balances.items() if b < EMERGENCY_THRESHOLD]
    if not depleted:
        return None
    # Find the most depleted
    depleted.sort(key=lambda x: x[1])
    worst = depleted[0][0]
    # Find the healthiest token to pay with
    healthy = [(s, b) for s, b in balances.items() if s != worst]
    healthy.sort(key=lambda x: x[1], reverse=True)
    best = healthy[0][0] if healthy[0][1] > MIN_BALANCE else None
    if best is None:
        return None
    return (worst, best)


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
def _build_orders(escrow, batch_id, peers_in_pool):
    orders = []

    # ── Emergency mode: one token critically low ──
    emergency = _find_emergency(escrow)
    if emergency:
        worst, best = emergency
        # Find the pair where worst is base and best is quote (buy worst with best)
        # Or where best is base (sell best to get worst as quote)
        target_pair = None
        target_role = None
        for market in PAIRS:
            if market["base"] == worst and market["quote"] == best:
                target_pair = market
                target_role = "buy"
                break
            if market["base"] == best and market["quote"] == worst:
                target_pair = market
                target_role = "sell"
                break

        if target_pair:
            pair = target_pair["pair"]
            base = target_pair["base"]
            quote = target_pair["quote"]
            base_bal = Decimal(escrow.get(base, "0"))
            quote_bal = Decimal(escrow.get(quote, "0"))
            mid = _deterministic_mid(batch_id, pair)

            log.info(f"Batch {batch_id}: EMERGENCY — {worst} critical "
                     f"({Decimal(escrow.get(worst, '0')):.5f}), "
                     f"using {best} ({Decimal(escrow.get(best, '0')):.5f}) "
                     f"→ {target_role} {pair}")

            if target_role == "buy" and quote_bal > MIN_BALANCE:
                buy_price = mid * (Decimal("1") + SPREAD)
                spend = quote_bal * EMERGENCY_ALLOC
                qty = (spend / buy_price).quantize(Decimal("0.00001"))
                if qty >= _min_qty:
                    orders.append({
                        "pair": pair,
                        "side": "buy",
                        "price": str(buy_price.quantize(Decimal("0.00000001"))),
                        "quantity": str(qty),
                    })
            elif target_role == "sell" and base_bal > MIN_BALANCE:
                sell_price = mid * (Decimal("1") - SPREAD)
                qty = (base_bal * EMERGENCY_ALLOC).quantize(Decimal("0.00001"))
                if qty >= _min_qty:
                    orders.append({
                        "pair": pair,
                        "side": "sell",
                        "price": str(sell_price.quantize(Decimal("0.00000001"))),
                        "quantity": str(qty),
                    })

            # In emergency, ONLY trade the recovery pair — skip everything else
            return orders

    # ── Triangle rebalancing ──
    depleted = _find_depleted_token(escrow)
    force_buy_pair = None
    if depleted:
        force_buy_pair = ACQUIRE_PAIR[depleted]
        log.info(f"Batch {batch_id}: REBALANCE — {depleted} depleted, "
                 f"forcing buy on {force_buy_pair}")

    # ── Normal order building ──
    for market in PAIRS:
        pair = market["pair"]
        base = market["base"]
        quote = market["quote"]
        base_bal = Decimal(escrow.get(base, "0"))
        quote_bal = Decimal(escrow.get(quote, "0"))

        if pair == force_buy_pair:
            role = "buy"
            effective_alloc = REBALANCE_ALLOC
        else:
            role = _role_for(batch_id, pair, _nft_id)
            effective_alloc = ALLOC_PCT

        mid = _deterministic_mid(batch_id, pair)

        log.info(f"Batch {batch_id}: {pair} → {role} "
                 f"(mid={mid:.8f}, base={base_bal:.2f}, quote={quote_bal:.2f})")

        if role == "sell" and base_bal >= MIN_BALANCE:
            sell_price = mid * (Decimal("1") - SPREAD)
            qty = (base_bal * effective_alloc).quantize(Decimal("0.00001"))
            if qty >= _min_qty:
                orders.append({
                    "pair": pair,
                    "side": "sell",
                    "price": str(sell_price.quantize(Decimal("0.00000001"))),
                    "quantity": str(qty),
                })

        elif role == "buy" and quote_bal >= MIN_BALANCE:
            buy_price = mid * (Decimal("1") + SPREAD)
            spend = quote_bal * effective_alloc
            qty = (spend / buy_price).quantize(Decimal("0.00001"))
            if qty >= _min_qty:
                orders.append({
                    "pair": pair,
                    "side": "buy",
                    "price": str(buy_price.quantize(Decimal("0.00000001"))),
                    "quantity": str(qty),
                })

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
    peers_in_pool = ctx.get("peers_in_pool", 0)
    token_actions = []

    # Read protocol minimum from node (DMKT11+)
    min_trade_str = ctx.get("min_trade_quantity", "")
    if min_trade_str:
        try:
            protocol_min = Decimal(min_trade_str)
            if protocol_min > 0:
                _min_qty = protocol_min
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
