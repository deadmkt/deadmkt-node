// deadmkt-matching: Deterministic matching engine
//
// CRITICAL: Every node with the same set of revealed orders MUST produce
// the exact same match list. Sorting is by price then by order_hash.
// Any deviation = different settlements = network split.
//
// Matching runs independently per trading pair (symbol). Orders never
// cross-match between symbols.

use std::cmp::min;
use std::collections::HashMap;

use deadmkt_crypto::{compute_match_hash, Order};
use sha2::{Digest, Sha256};

// =========================================================================
// Types
// =========================================================================

/// A revealed order ready for matching (order + its signature + cached hash).
#[derive(Clone, Debug)]
pub struct RevealedOrder {
    pub order: Order,
    pub signature: Vec<u8>,     // Ed25519 over BCS(order), 64 bytes
    pub order_hash: [u8; 32],   // SHA256(BCS(order)) — cached, not recomputed
}

/// A computed match between two orders.
#[derive(Clone, Debug)]
pub struct Match {
    pub batch_id: u64,
    pub pool_id: u64,
    pub symbol: Vec<u8>,
    pub buyer: RevealedOrder,
    pub seller: RevealedOrder,
    pub fill_quantity: u64,
    pub settlement_price: u64,   // (buyer_price + seller_price) / 2, local only
    pub match_hash: [u8; 32],    // SHA256(batch_id || buyer_hash || seller_hash || fill_qty)
    pub gas_payer_nft_id: u64,   // lower order_hash pays
}

/// Market config needed for matching (from chain, cached).
#[derive(Clone, Debug)]
pub struct MarketConfig {
    pub symbol: Vec<u8>,
    pub min_quantity: u64,
}

/// Top-level matching input — all reveals from a batch + market configs.
#[derive(Clone, Debug)]
pub struct MatchingInput {
    pub batch_id: u64,
    pub pool_id: u64,
    pub reveals: Vec<RevealedOrder>,
    pub markets: Vec<MarketConfig>,
}

// =========================================================================
// Public API
// =========================================================================

/// Top-level entry point: group reveals by symbol, run matching per symbol.
/// Returns all matches across all symbols. Deterministic.
///
/// Reveals for symbols not in `markets` are silently dropped (unknown pair).
pub fn run_matching(input: &MatchingInput) -> Vec<Match> {
    // Build symbol → min_quantity lookup
    let market_map: HashMap<&[u8], u64> = input
        .markets
        .iter()
        .map(|m| (m.symbol.as_slice(), m.min_quantity))
        .collect();

    // Group reveals by symbol
    let mut by_symbol: HashMap<Vec<u8>, Vec<RevealedOrder>> = HashMap::new();
    for reveal in &input.reveals {
        by_symbol
            .entry(reveal.order.symbol.clone())
            .or_default()
            .push(reveal.clone());
    }

    // Sort symbols for deterministic iteration order
    let mut symbols: Vec<Vec<u8>> = by_symbol.keys().cloned().collect();
    symbols.sort();

    let mut all_matches = Vec::new();

    for symbol in &symbols {
        let min_quantity = match market_map.get(symbol.as_slice()) {
            Some(&mq) => mq,
            None => continue, // Unknown symbol, skip
        };

        let reveals = by_symbol.get(symbol).unwrap();

        // Split into buys and sells
        let mut buys: Vec<RevealedOrder> = reveals
            .iter()
            .filter(|r| r.order.side == 1) // BUY
            .cloned()
            .collect();
        let mut sells: Vec<RevealedOrder> = reveals
            .iter()
            .filter(|r| r.order.side == 0) // SELL
            .cloned()
            .collect();

        sort_orders(&mut buys, &mut sells);

        let matches = generate_matches(
            input.batch_id,
            input.pool_id,
            symbol,
            &buys,
            &sells,
            min_quantity,
        );

        all_matches.extend(matches);
    }

    all_matches
}

/// Sort buys descending by price, sells ascending by price.
/// Tiebreaker: lower order_hash = higher priority (sorted first).
pub fn sort_orders(buys: &mut [RevealedOrder], sells: &mut [RevealedOrder]) {
    // Buys: highest price first. Tiebreak: lower hash first.
    buys.sort_by(|a, b| {
        b.order
            .price
            .cmp(&a.order.price)
            .then_with(|| a.order_hash.cmp(&b.order_hash))
    });

    // Sells: lowest price first. Tiebreak: lower hash first.
    sells.sort_by(|a, b| {
        a.order
            .price
            .cmp(&b.order.price)
            .then_with(|| a.order_hash.cmp(&b.order_hash))
    });
}

/// Greedy matching on pre-sorted, single-symbol order lists.
///
/// Walks sorted buys (desc) and sells (asc), matching any crossing pair
/// (buyer_price >= seller_price). Handles partial fills, self-trade skips,
/// and min_quantity enforcement.
///
/// Settlement price per match = (buyer_price + seller_price) / 2 (integer division).
pub fn generate_matches(
    batch_id: u64,
    pool_id: u64,
    symbol: &[u8],
    buys: &[RevealedOrder],
    sells: &[RevealedOrder],
    min_quantity: u64,
) -> Vec<Match> {
    let mut matches = Vec::new();

    if buys.is_empty() || sells.is_empty() {
        return matches;
    }

    let mut buy_idx: usize = 0;
    let mut sell_idx: usize = 0;
    let mut buy_remaining: u64 = buys[0].order.quantity;
    let mut sell_remaining: u64 = sells[0].order.quantity;

    while buy_idx < buys.len() && sell_idx < sells.len() {
        // Orders stop crossing — no more matches possible
        if buys[buy_idx].order.price < sells[sell_idx].order.price {
            break;
        }

        // Self-trade skip: same nft_id on both sides.
        // Advance the side with less remaining quantity.
        if buys[buy_idx].order.nft_id == sells[sell_idx].order.nft_id {
            if buy_remaining <= sell_remaining {
                sell_remaining -= buy_remaining;
                buy_idx += 1;
                buy_remaining = buys.get(buy_idx).map(|o| o.order.quantity).unwrap_or(0);
                if sell_remaining == 0 {
                    sell_idx += 1;
                    sell_remaining =
                        sells.get(sell_idx).map(|o| o.order.quantity).unwrap_or(0);
                }
            } else {
                buy_remaining -= sell_remaining;
                sell_idx += 1;
                sell_remaining =
                    sells.get(sell_idx).map(|o| o.order.quantity).unwrap_or(0);
                if buy_remaining == 0 {
                    buy_idx += 1;
                    buy_remaining =
                        buys.get(buy_idx).map(|o| o.order.quantity).unwrap_or(0);
                }
            }
            continue;
        }

        let fill_qty = min(buy_remaining, sell_remaining);

        // Skip fills below market minimum
        if fill_qty < min_quantity {
            if buy_remaining <= sell_remaining {
                sell_remaining -= buy_remaining;
                buy_idx += 1;
                buy_remaining = buys.get(buy_idx).map(|o| o.order.quantity).unwrap_or(0);
                if sell_remaining == 0 {
                    sell_idx += 1;
                    sell_remaining =
                        sells.get(sell_idx).map(|o| o.order.quantity).unwrap_or(0);
                }
            } else {
                buy_remaining -= sell_remaining;
                sell_idx += 1;
                sell_remaining =
                    sells.get(sell_idx).map(|o| o.order.quantity).unwrap_or(0);
                if buy_remaining == 0 {
                    buy_idx += 1;
                    buy_remaining =
                        buys.get(buy_idx).map(|o| o.order.quantity).unwrap_or(0);
                }
            }
            continue;
        }

        // Settlement price = midpoint (integer division)
        let settlement_price =
            (buys[buy_idx].order.price + sells[sell_idx].order.price) / 2;

        // Compute match_hash
        let mh = compute_match_hash(
            batch_id,
            &buys[buy_idx].order_hash,
            &sells[sell_idx].order_hash,
            fill_qty,
        );
        let mut match_hash = [0u8; 32];
        match_hash.copy_from_slice(&mh);

        // Determine gas payer
        let gas_payer_nft_id = determine_gas_payer(
            &buys[buy_idx].order_hash,
            &sells[sell_idx].order_hash,
            buys[buy_idx].order.nft_id,
            sells[sell_idx].order.nft_id,
        );

        matches.push(Match {
            batch_id,
            pool_id,
            symbol: symbol.to_vec(),
            buyer: buys[buy_idx].clone(),
            seller: sells[sell_idx].clone(),
            fill_quantity: fill_qty,
            settlement_price,
            match_hash,
            gas_payer_nft_id,
        });

        buy_remaining -= fill_qty;
        sell_remaining -= fill_qty;

        if buy_remaining == 0 {
            buy_idx += 1;
            buy_remaining = buys.get(buy_idx).map(|o| o.order.quantity).unwrap_or(0);
        }
        if sell_remaining == 0 {
            sell_idx += 1;
            sell_remaining =
                sells.get(sell_idx).map(|o| o.order.quantity).unwrap_or(0);
        }
    }

    matches
}

/// Lower order_hash pays gas.
pub fn determine_gas_payer(
    buyer_hash: &[u8; 32],
    seller_hash: &[u8; 32],
    buyer_nft_id: u64,
    seller_nft_id: u64,
) -> u64 {
    if buyer_hash < seller_hash {
        buyer_nft_id
    } else {
        seller_nft_id
    }
}

// =========================================================================
// Internal helpers
// =========================================================================

/// Compute order hash from an Order (SHA256(BCS(order))).
/// Used when constructing RevealedOrder from raw orders.
pub fn compute_order_hash(order: &Order) -> [u8; 32] {
    let bytes = deadmkt_crypto::encode_order(order).expect("BCS encoding should not fail");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let result = hasher.finalize();
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&result);
    hash
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Test helpers --

    fn make_order(nft_id: u64, symbol: &[u8], side: u8, price: u64, quantity: u64) -> Order {
        Order {
            nft_id,
            symbol: symbol.to_vec(),
            side,
            price,
            quantity,
            batch_id: 100,
            nonce: vec![0u8; 32],
        }
    }

    fn make_order_with_nonce(
        nft_id: u64,
        symbol: &[u8],
        side: u8,
        price: u64,
        quantity: u64,
        nonce_byte: u8,
    ) -> Order {
        Order {
            nft_id,
            symbol: symbol.to_vec(),
            side,
            price,
            quantity,
            batch_id: 100,
            nonce: vec![nonce_byte; 32],
        }
    }

    fn make_revealed(order: Order) -> RevealedOrder {
        let order_hash = compute_order_hash(&order);
        RevealedOrder {
            order,
            signature: vec![0u8; 64], // Dummy sig — matching doesn't verify
            order_hash,
        }
    }

    fn emm_kay_market() -> MarketConfig {
        MarketConfig {
            symbol: b"EMM/KAY".to_vec(),
            min_quantity: 1,
        }
    }

    fn kay_tee_market() -> MarketConfig {
        MarketConfig {
            symbol: b"KAY/TEE".to_vec(),
            min_quantity: 1,
        }
    }

    const BATCH_ID: u64 = 100;
    const POOL_ID: u64 = 3;
    const EMM_KAY: &[u8] = b"EMM/KAY";
    const BUY: u8 = 1;
    const SELL: u8 = 0;

    // -- T_MATCH_01: Empty orders --

    #[test]
    fn test_match_01_empty_orders() {
        // Both empty
        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &[], &[], 1);
        assert!(matches.is_empty());

        // Buys only
        let buy = make_revealed(make_order(1, EMM_KAY, BUY, 50, 100));
        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &[buy], &[], 1);
        assert!(matches.is_empty());

        // Sells only
        let sell = make_revealed(make_order(2, EMM_KAY, SELL, 48, 100));
        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &[], &[sell], 1);
        assert!(matches.is_empty());
    }

    // -- T_MATCH_02: Single crossing pair — full fill --

    #[test]
    fn test_match_02_single_crossing_full_fill() {
        let buy = make_revealed(make_order(1, EMM_KAY, BUY, 50, 100));
        let sell = make_revealed(make_order(2, EMM_KAY, SELL, 48, 100));

        let mut buys = vec![buy];
        let mut sells = vec![sell];
        sort_orders(&mut buys, &mut sells);

        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &buys, &sells, 1);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].fill_quantity, 100);
        assert_eq!(matches[0].settlement_price, 49); // (50 + 48) / 2
        assert_eq!(matches[0].batch_id, BATCH_ID);
        assert_eq!(matches[0].pool_id, POOL_ID);
        assert_eq!(matches[0].symbol, EMM_KAY);
    }

    // -- T_MATCH_03: Non-crossing orders --

    #[test]
    fn test_match_03_non_crossing() {
        let buy = make_revealed(make_order(1, EMM_KAY, BUY, 48, 100));
        let sell = make_revealed(make_order(2, EMM_KAY, SELL, 50, 100));

        let mut buys = vec![buy];
        let mut sells = vec![sell];
        sort_orders(&mut buys, &mut sells);

        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &buys, &sells, 1);
        assert!(matches.is_empty());
    }

    // -- T_MATCH_04: Partial fill — buyer larger --

    #[test]
    fn test_match_04_partial_fill_buyer_larger() {
        let buy = make_revealed(make_order(1, EMM_KAY, BUY, 50, 1000));
        let sell = make_revealed(make_order(2, EMM_KAY, SELL, 48, 600));

        let mut buys = vec![buy];
        let mut sells = vec![sell];
        sort_orders(&mut buys, &mut sells);

        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &buys, &sells, 1);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].fill_quantity, 600);
        assert_eq!(matches[0].settlement_price, 49);
    }

    // -- T_MATCH_05: Partial fill — seller larger --

    #[test]
    fn test_match_05_partial_fill_seller_larger() {
        let buy = make_revealed(make_order(1, EMM_KAY, BUY, 50, 600));
        let sell = make_revealed(make_order(2, EMM_KAY, SELL, 48, 1000));

        let mut buys = vec![buy];
        let mut sells = vec![sell];
        sort_orders(&mut buys, &mut sells);

        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &buys, &sells, 1);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].fill_quantity, 600);
        assert_eq!(matches[0].settlement_price, 49);
    }

    // -- T_MATCH_06: Multiple partial fills — one buy, two sells --

    #[test]
    fn test_match_06_one_buy_two_sells() {
        let buy = make_revealed(make_order(1, EMM_KAY, BUY, 50, 1000));
        let sell_a = make_revealed(make_order(2, EMM_KAY, SELL, 47, 400));
        let sell_b = make_revealed(make_order(3, EMM_KAY, SELL, 48, 300));

        let mut buys = vec![buy];
        let mut sells = vec![sell_a, sell_b];
        sort_orders(&mut buys, &mut sells);

        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &buys, &sells, 1);

        assert_eq!(matches.len(), 2);
        // First match: buy@50 × sell_a@47
        assert_eq!(matches[0].fill_quantity, 400);
        assert_eq!(matches[0].settlement_price, 48); // (50+47)/2 = 48 (integer)
        // Second match: buy@50 × sell_b@48
        assert_eq!(matches[1].fill_quantity, 300);
        assert_eq!(matches[1].settlement_price, 49); // (50+48)/2
    }

    // -- T_MATCH_07: Multiple partial fills — two buys, one sell --

    #[test]
    fn test_match_07_two_buys_one_sell() {
        let buy_a = make_revealed(make_order(1, EMM_KAY, BUY, 52, 400));
        let buy_b = make_revealed(make_order(2, EMM_KAY, BUY, 50, 300));
        let sell = make_revealed(make_order(3, EMM_KAY, SELL, 48, 1000));

        let mut buys = vec![buy_a, buy_b];
        let mut sells = vec![sell];
        sort_orders(&mut buys, &mut sells);

        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &buys, &sells, 1);

        assert_eq!(matches.len(), 2);
        // First: buy_a@52 × sell@48
        assert_eq!(matches[0].fill_quantity, 400);
        assert_eq!(matches[0].settlement_price, 50); // (52+48)/2
        // Second: buy_b@50 × sell@48
        assert_eq!(matches[1].fill_quantity, 300);
        assert_eq!(matches[1].settlement_price, 49); // (50+48)/2
        // Sell has 300 unfilled — not carried over
    }

    // -- T_MATCH_08: Self-trade skip — same nft_id --

    #[test]
    fn test_match_08_self_trade_skip() {
        let buy = make_revealed(make_order(42, EMM_KAY, BUY, 50, 100));
        let sell = make_revealed(make_order(42, EMM_KAY, SELL, 48, 100));

        let mut buys = vec![buy];
        let mut sells = vec![sell];
        sort_orders(&mut buys, &mut sells);

        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &buys, &sells, 1);
        assert!(matches.is_empty());
    }

    // -- T_MATCH_09: Self-trade skip with passthrough --

    #[test]
    fn test_match_09_self_trade_passthrough() {
        // buy_a nft=42 would self-trade with sell nft=42, but buy_b nft=99 can match
        let buy_a = make_revealed(make_order_with_nonce(42, EMM_KAY, BUY, 50, 100, 0xAA));
        let buy_b = make_revealed(make_order_with_nonce(99, EMM_KAY, BUY, 50, 100, 0xBB));
        let sell = make_revealed(make_order(42, EMM_KAY, SELL, 48, 100));

        let mut buys = vec![buy_a, buy_b];
        let mut sells = vec![sell];
        sort_orders(&mut buys, &mut sells);

        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &buys, &sells, 1);

        // One of the buys matches the sell (not the self-trade one)
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].fill_quantity, 100);

        // The match must NOT be a self-trade
        assert_ne!(
            matches[0].buyer.order.nft_id,
            matches[0].seller.order.nft_id
        );
    }

    // -- T_MATCH_10: Price sort — highest buy matches lowest sell --

    #[test]
    fn test_match_10_price_sort() {
        let buy_a = make_revealed(make_order(1, EMM_KAY, BUY, 52, 100));
        let buy_b = make_revealed(make_order(2, EMM_KAY, BUY, 50, 100));
        let sell_a = make_revealed(make_order(3, EMM_KAY, SELL, 47, 100));
        let sell_b = make_revealed(make_order(4, EMM_KAY, SELL, 49, 100));

        let mut buys = vec![buy_a, buy_b];
        let mut sells = vec![sell_a, sell_b];
        sort_orders(&mut buys, &mut sells);

        // After sort: buys=[52, 50], sells=[47, 49]
        assert_eq!(buys[0].order.price, 52);
        assert_eq!(buys[1].order.price, 50);
        assert_eq!(sells[0].order.price, 47);
        assert_eq!(sells[1].order.price, 49);

        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &buys, &sells, 1);

        assert_eq!(matches.len(), 2);
        // buy_a(52) × sell_a(47)
        assert_eq!(matches[0].buyer.order.price, 52);
        assert_eq!(matches[0].seller.order.price, 47);
        assert_eq!(matches[0].settlement_price, 49); // (52+47)/2 = 49
        // buy_b(50) × sell_b(49)
        assert_eq!(matches[1].buyer.order.price, 50);
        assert_eq!(matches[1].seller.order.price, 49);
        assert_eq!(matches[1].settlement_price, 49); // (50+49)/2 = 49
    }

    // -- T_MATCH_11: Hash tiebreaker --

    #[test]
    fn test_match_11_hash_tiebreaker() {
        // Two buys at same price, different nonces → different hashes
        let buy_a = make_revealed(make_order_with_nonce(1, EMM_KAY, BUY, 50, 100, 0x01));
        let buy_b = make_revealed(make_order_with_nonce(2, EMM_KAY, BUY, 50, 100, 0x02));
        let sell = make_revealed(make_order(3, EMM_KAY, SELL, 48, 100));

        // Determine which has lower hash
        let lower_hash_nft = if buy_a.order_hash < buy_b.order_hash {
            buy_a.order.nft_id
        } else {
            buy_b.order.nft_id
        };

        let mut buys = vec![buy_a, buy_b];
        let mut sells = vec![sell];
        sort_orders(&mut buys, &mut sells);

        // After sort, the buy with lower hash should be first
        assert_eq!(buys[0].order.nft_id, lower_hash_nft);

        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &buys, &sells, 1);

        assert_eq!(matches.len(), 1);
        // The buy with lower hash gets priority
        assert_eq!(matches[0].buyer.order.nft_id, lower_hash_nft);
    }

    // -- T_MATCH_12: min_quantity enforcement --

    #[test]
    fn test_match_12_min_quantity() {
        let buy = make_revealed(make_order(1, EMM_KAY, BUY, 50, 100));
        let sell = make_revealed(make_order(2, EMM_KAY, SELL, 48, 5));

        let mut buys = vec![buy];
        let mut sells = vec![sell];
        sort_orders(&mut buys, &mut sells);

        // min_quantity = 10, but sell only has 5
        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &buys, &sells, 10);
        assert!(matches.is_empty());
    }

    // -- T_MATCH_13: Gas payer — lower hash pays --

    #[test]
    fn test_match_13_gas_payer() {
        let mut low_hash = [0u8; 32];
        low_hash[31] = 1;
        let high_hash = [0xFFu8; 32];

        // Buyer has lower hash → buyer pays
        assert_eq!(determine_gas_payer(&low_hash, &high_hash, 10, 20), 10);

        // Seller has lower hash → seller pays
        assert_eq!(determine_gas_payer(&high_hash, &low_hash, 10, 20), 20);

        // Verify gas_payer is set correctly in a full match
        let buy = make_revealed(make_order_with_nonce(10, EMM_KAY, BUY, 50, 100, 0x01));
        let sell = make_revealed(make_order_with_nonce(20, EMM_KAY, SELL, 48, 100, 0x02));

        let expected_payer = if buy.order_hash < sell.order_hash {
            10
        } else {
            20
        };

        let mut buys = vec![buy];
        let mut sells = vec![sell];
        sort_orders(&mut buys, &mut sells);

        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &buys, &sells, 1);
        assert_eq!(matches[0].gas_payer_nft_id, expected_payer);
    }

    // -- T_MATCH_14: Determinism --

    #[test]
    fn test_match_14_determinism() {
        use rand::Rng;

        let mut rng = rand::thread_rng();

        // Generate a fixed set of random orders
        let mut reveals = Vec::new();
        for i in 0..20 {
            let side = if i % 2 == 0 { BUY } else { SELL };
            let price = rng.gen_range(40..60);
            let quantity = rng.gen_range(50..500);
            let mut nonce = vec![0u8; 32];
            rng.fill(&mut nonce[..]);

            let order = Order {
                nft_id: i as u64 + 1,
                symbol: EMM_KAY.to_vec(),
                side,
                price,
                quantity,
                batch_id: BATCH_ID,
                nonce,
            };
            reveals.push(make_revealed(order));
        }

        let input = MatchingInput {
            batch_id: BATCH_ID,
            pool_id: POOL_ID,
            reveals: reveals.clone(),
            markets: vec![emm_kay_market()],
        };

        // Run 100 times — must produce identical result every time
        let baseline = run_matching(&input);

        for _ in 0..100 {
            let result = run_matching(&input);
            assert_eq!(result.len(), baseline.len(), "match count differs");

            for (i, (a, b)) in baseline.iter().zip(result.iter()).enumerate() {
                assert_eq!(a.fill_quantity, b.fill_quantity, "fill_qty differs at {i}");
                assert_eq!(
                    a.settlement_price, b.settlement_price,
                    "price differs at {i}"
                );
                assert_eq!(a.match_hash, b.match_hash, "match_hash differs at {i}");
                assert_eq!(
                    a.gas_payer_nft_id, b.gas_payer_nft_id,
                    "gas_payer differs at {i}"
                );
                assert_eq!(
                    a.buyer.order.nft_id, b.buyer.order.nft_id,
                    "buyer differs at {i}"
                );
                assert_eq!(
                    a.seller.order.nft_id, b.seller.order.nft_id,
                    "seller differs at {i}"
                );
            }
        }
    }

    // -- T_MATCH_15: Multi-symbol via run_matching --

    #[test]
    fn test_match_15_multi_symbol() {
        let emm_buy = make_revealed(make_order(1, EMM_KAY, BUY, 50, 100));
        let emm_sell = make_revealed(make_order(2, EMM_KAY, SELL, 48, 100));

        let kay_tee = b"KAY/TEE";
        let kay_tee_buy = make_revealed(make_order(3, kay_tee, BUY, 60000, 1));
        let kay_tee_sell = make_revealed(make_order(4, kay_tee, SELL, 59000, 1));

        let input = MatchingInput {
            batch_id: BATCH_ID,
            pool_id: POOL_ID,
            reveals: vec![emm_buy, emm_sell, kay_tee_buy, kay_tee_sell],
            markets: vec![emm_kay_market(), kay_tee_market()],
        };

        let matches = run_matching(&input);

        assert_eq!(matches.len(), 2);

        // Find each symbol's match
        let emm_match = matches.iter().find(|m| m.symbol == EMM_KAY).unwrap();
        let kay_tee_match = matches.iter().find(|m| m.symbol == kay_tee).unwrap();

        assert_eq!(emm_match.fill_quantity, 100);
        assert_eq!(emm_match.settlement_price, 49);

        assert_eq!(kay_tee_match.fill_quantity, 1);
        assert_eq!(kay_tee_match.settlement_price, 59500); // (60000+59000)/2
    }

    // -- Extra: match_hash is correctly computed --

    #[test]
    fn test_match_hash_populated() {
        let buy = make_revealed(make_order(1, EMM_KAY, BUY, 50, 100));
        let sell = make_revealed(make_order(2, EMM_KAY, SELL, 48, 100));

        let mut buys = vec![buy.clone()];
        let mut sells = vec![sell.clone()];
        sort_orders(&mut buys, &mut sells);

        let matches = generate_matches(BATCH_ID, POOL_ID, EMM_KAY, &buys, &sells, 1);
        assert_eq!(matches.len(), 1);

        // Verify match_hash matches manual computation
        let expected = compute_match_hash(
            BATCH_ID,
            &buys[0].order_hash,
            &sells[0].order_hash,
            100,
        );
        assert_eq!(matches[0].match_hash[..], expected[..]);
    }

    // -- Extra: unknown symbol dropped by run_matching --

    #[test]
    fn test_unknown_symbol_dropped() {
        let unknown_buy = make_revealed(make_order(1, b"XXX/YYY", BUY, 50, 100));
        let unknown_sell = make_revealed(make_order(2, b"XXX/YYY", SELL, 48, 100));

        let input = MatchingInput {
            batch_id: BATCH_ID,
            pool_id: POOL_ID,
            reveals: vec![unknown_buy, unknown_sell],
            markets: vec![emm_kay_market()], // Only EMM/KAY configured, not XXX/YYY
        };

        let matches = run_matching(&input);
        assert!(matches.is_empty());
    }
}
