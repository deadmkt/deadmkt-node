// =========================================================================
// convert.rs: Strategy ↔ canonical conversion + order validation
// =========================================================================
//
// CD-21: Strategy sends human-readable decimals. Node converts at commit boundary.
// CD-22: Orders validated before signing (pair active, price>0, qty≥min, balance).
// Risk #9: No floating point. Parse decimal strings as fixed-point directly.

use crate::{OrderSpec, StrategyError};
use std::collections::{HashMap, HashSet};

// =========================================================================
// Canonical order (output of conversion)
// =========================================================================

#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalOrder {
    pub symbol: Vec<u8>,
    pub side: u8,           // 1=buy, 0=sell
    pub price: u64,         // price × 1e8
    pub quantity: u64,      // quantity × 10^decimals
    pub nft_id: u64,
    pub batch_id: u64,
    pub nonce: Vec<u8>,     // 32 random bytes
}

// =========================================================================
// Validation context
// =========================================================================

#[derive(Debug, Clone)]
pub struct OrderValidationContext {
    pub active_pairs: HashSet<String>,
    pub min_quantities: HashMap<String, u64>,   // pair → canonical min_qty
    pub token_decimals: HashMap<String, u8>,    // "EMM" → 8
    pub projected_balances: HashMap<String, u64>,
    pub commits_per_batch: u32,
    pub nft_id: u64,
    pub batch_id: u64,
}

// =========================================================================
// Validated order result (includes warning info)
// =========================================================================

#[derive(Debug)]
pub struct ValidationResult {
    pub orders: Vec<CanonicalOrder>,
    pub warnings: Vec<String>,
}

// =========================================================================
// Decimal string → u64 fixed-point (NO FLOATING POINT)
// =========================================================================

/// Parse a decimal string like "0.0499" into a fixed-point u64 with `target_decimals`
/// decimal places. E.g. "0.0499" with 8 decimals → 4_990_000.
///
/// Handles: "500" → 500 * 10^8, "0.0499" → 4_990_000, "123.456" → 12_345_600_000.
/// Rejects: "", "abc", negative numbers.
fn parse_decimal_to_fixed(s: &str, target_decimals: u8) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty string".into());
    }

    // Split on decimal point
    let (integer_part, frac_part) = if let Some(dot_pos) = s.find('.') {
        (&s[..dot_pos], &s[dot_pos + 1..])
    } else {
        (s, "")
    };

    // Validate chars
    if !integer_part.chars().all(|c| c.is_ascii_digit()) || integer_part.is_empty() {
        return Err(format!("invalid integer part: '{}'", integer_part));
    }
    if !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("invalid fractional part: '{}'", frac_part));
    }

    let int_val: u128 = integer_part.parse::<u128>()
        .map_err(|e| format!("integer overflow: {}", e))?;

    let td = target_decimals as usize;

    // Pad or truncate fractional part to target_decimals
    let frac_val: u128 = if frac_part.is_empty() {
        0
    } else if frac_part.len() <= td {
        // Pad with trailing zeros: "049" with 8 decimals → "04900000"
        let padded = format!("{:0<width$}", frac_part, width = td);
        padded.parse::<u128>()
            .map_err(|e| format!("fractional overflow: {}", e))?
    } else {
        // Truncate to target_decimals (drop extra precision)
        frac_part[..td].parse::<u128>()
            .map_err(|e| format!("fractional parse: {}", e))?
    };

    let factor: u128 = 10u128.pow(target_decimals as u32);
    let result = int_val
        .checked_mul(factor)
        .and_then(|v| v.checked_add(frac_val))
        .ok_or_else(|| "overflow in fixed-point conversion".to_string())?;

    if result > u64::MAX as u128 {
        return Err("result exceeds u64 range".into());
    }

    Ok(result as u64)
}

// =========================================================================
// Single order conversion
// =========================================================================

/// Convert a single OrderSpec from strategy format to canonical format.
/// Does NOT validate against market context — just parses and converts types.
pub fn convert_order(
    spec: &OrderSpec,
    token_decimals: &HashMap<String, u8>,
    nft_id: u64,
    batch_id: u64,
    nonce: Vec<u8>,
) -> Result<CanonicalOrder, StrategyError> {
    // Side
    let side = match spec.side.as_str() {
        "buy" => 1u8,
        "sell" => 0u8,
        other => return Err(StrategyError::InvalidSide(other.to_string())),
    };

    // Price: always 8 decimal places (× 1e8)
    let price = parse_decimal_to_fixed(&spec.price, 8)
        .map_err(|e| StrategyError::InvalidPrice(format!("{}: {}", spec.price, e)))?;

    // Quantity: decimals depend on token
    // Extract base token from pair "EMM/KAY" → "EMM"
    let base_token = spec.pair.split('/').next()
        .ok_or_else(|| StrategyError::UnknownPair(spec.pair.clone()))?;
    let decimals = token_decimals.get(base_token)
        .copied()
        .unwrap_or(5); // default 5 (Trippples tokens)

    let quantity = parse_decimal_to_fixed(&spec.quantity, decimals)
        .map_err(|e| StrategyError::InvalidQuantity(format!("{}: {}", spec.quantity, e)))?;

    Ok(CanonicalOrder {
        symbol: spec.pair.as_bytes().to_vec(),
        side,
        price,
        quantity,
        nft_id,
        batch_id,
        nonce,
    })
}

// =========================================================================
// Batch order validation (CD-22)
// =========================================================================

/// Extract quote token from pair "EMM/KAY" → "KAY"
fn quote_token(pair: &str) -> &str {
    pair.split('/').nth(1).unwrap_or("")
}

/// Extract base token from pair "EMM/KAY" → "EMM"
fn base_token(pair: &str) -> &str {
    pair.split('/').next().unwrap_or("")
}

/// Compute outflow for a single order (worst-case).
/// Buyer: qty × price / 1e8 in QUOTE. Seller: qty in BASE.
fn compute_outflow(
    order: &CanonicalOrder,
    pair: &str,
    token_decimals: &HashMap<String, u8>,
) -> (String, u64) {
    if order.side == 1 {
        // Buyer: outflow in quote token = qty_raw * price_raw / 10^(base_dec + price_dec - quote_dec)
        let base = base_token(pair);
        let quote = quote_token(pair);
        let base_dec = token_decimals.get(base).copied().unwrap_or(5) as u32;
        let quote_dec = token_decimals.get(quote).copied().unwrap_or(5) as u32;
        let price_dec: u32 = 8; // prices are always 8-decimal
        let divisor = 10u128.pow(base_dec + price_dec - quote_dec);
        let outflow = (order.quantity as u128 * order.price as u128 / divisor) as u64;
        (quote.to_string(), outflow)
    } else {
        // Seller: outflow in base token (quantity is already in base decimals)
        (base_token(pair).to_string(), order.quantity)
    }
}

/// Validate and convert a batch of orders from strategy.
///
/// Steps:
/// 1. Truncate to commits_per_batch (warn if exceeded)
/// 2. Convert each order
/// 3. Validate: pair active, price > 0, qty ≥ min_quantity
/// 4. Check cumulative balance sufficiency
/// 5. Return valid orders + warnings for rejected ones
pub fn validate_orders(
    specs: &[OrderSpec],
    ctx: &OrderValidationContext,
    nonce_fn: impl Fn() -> Vec<u8>,
) -> ValidationResult {
    let mut result = ValidationResult {
        orders: Vec::new(),
        warnings: Vec::new(),
    };

    // Step 1: Truncate
    let effective_specs = if specs.len() > ctx.commits_per_batch as usize {
        result.warnings.push(format!(
            "truncated {} orders to commits_per_batch={}",
            specs.len(), ctx.commits_per_batch
        ));
        &specs[..ctx.commits_per_batch as usize]
    } else {
        specs
    };

    // Track cumulative outflows per token for balance check
    let mut cumulative_outflows: HashMap<String, u64> = HashMap::new();

    for (i, spec) in effective_specs.iter().enumerate() {
        // Step 2: Convert
        let canonical = match convert_order(
            spec, &ctx.token_decimals, ctx.nft_id, ctx.batch_id, nonce_fn(),
        ) {
            Ok(c) => c,
            Err(e) => {
                result.warnings.push(format!("order[{}] conversion failed: {}", i, e));
                continue;
            }
        };

        // Step 3a: Pair active
        if !ctx.active_pairs.contains(&spec.pair) {
            result.warnings.push(format!("order[{}] pair '{}' not active", i, spec.pair));
            continue;
        }

        // Step 3b: Price > 0
        if canonical.price == 0 {
            result.warnings.push(format!("order[{}] price is zero", i));
            continue;
        }

        // Step 3c: Quantity ≥ min_quantity
        if let Some(&min_qty) = ctx.min_quantities.get(&spec.pair) {
            if canonical.quantity < min_qty {
                result.warnings.push(format!(
                    "order[{}] quantity {} < min_quantity {}",
                    i, canonical.quantity, min_qty
                ));
                continue;
            }
        }

        // Step 4: Balance sufficiency (cumulative)
        let (outflow_token, outflow_amount) = compute_outflow(&canonical, &spec.pair, &ctx.token_decimals);
        let cumulative = cumulative_outflows.entry(outflow_token.clone()).or_insert(0);
        let new_cumulative = *cumulative + outflow_amount;

        let projected = ctx.projected_balances.get(&outflow_token).copied().unwrap_or(0);
        if new_cumulative > projected {
            result.warnings.push(format!(
                "order[{}] cumulative outflow {} exceeds projected {} for {}",
                i, new_cumulative, projected, outflow_token
            ));
            continue;
        }

        *cumulative_outflows.get_mut(&outflow_token).unwrap() = new_cumulative;
        result.orders.push(canonical);
    }

    result
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn default_decimals() -> HashMap<String, u8> {
        let mut m = HashMap::new();
        m.insert("EMM".to_string(), 5);
        m.insert("KAY".to_string(), 5);
        m
    }

    fn zero_nonce() -> Vec<u8> {
        vec![0u8; 32]
    }

    fn make_ctx() -> OrderValidationContext {
        let mut active_pairs = HashSet::new();
        active_pairs.insert("EMM/KAY".to_string());

        let mut min_quantities = HashMap::new();
        // min 100 EMM = 100 * 10^5 = 10_000_000 canonical
        min_quantities.insert("EMM/KAY".to_string(), 10_000_000u64);

        let mut projected_balances = HashMap::new();
        projected_balances.insert("KAY".to_string(), 1_000_000u64); // 10 KAY (5 dec)
        projected_balances.insert("EMM".to_string(), 100_000_000u64); // 1000 EMM (5 dec)

        OrderValidationContext {
            active_pairs,
            min_quantities,
            token_decimals: default_decimals(),
            projected_balances,
            commits_per_batch: 3,
            nft_id: 42,
            batch_id: 100,
        }
    }

    // ── T_STRAT_11: convert_order buy — price×1e8, qty×10^dec, side=1

    #[test]
    fn t_strat_11_convert_order_buy() {
        let spec = OrderSpec {
            pair: "EMM/KAY".to_string(),
            side: "buy".to_string(),
            price: "0.0499".to_string(),
            quantity: "500".to_string(),
        };
        let result = convert_order(&spec, &default_decimals(), 42, 100, zero_nonce()).unwrap();

        assert_eq!(result.symbol, b"EMM/KAY");
        assert_eq!(result.side, 1); // buy
        assert_eq!(result.price, 4_990_000); // 0.0499 × 1e8
        assert_eq!(result.quantity, 50_000_000); // 500 × 10^5
        assert_eq!(result.nft_id, 42);
        assert_eq!(result.batch_id, 100);
    }

    // ── T_STRAT_12: convert_order sell — side=0

    #[test]
    fn t_strat_12_convert_order_sell() {
        let spec = OrderSpec {
            pair: "EMM/KAY".to_string(),
            side: "sell".to_string(),
            price: "0.0510".to_string(),
            quantity: "300".to_string(),
        };
        let result = convert_order(&spec, &default_decimals(), 42, 100, zero_nonce()).unwrap();

        assert_eq!(result.side, 0); // sell
        assert_eq!(result.price, 5_100_000); // 0.0510 × 1e8
        assert_eq!(result.quantity, 30_000_000); // 300 × 10^5
    }

    // ── T_STRAT_13: convert_order invalid price → Err

    #[test]
    fn t_strat_13_convert_order_invalid_price() {
        let spec = OrderSpec {
            pair: "EMM/KAY".to_string(),
            side: "buy".to_string(),
            price: "not_a_number".to_string(),
            quantity: "500".to_string(),
        };
        let result = convert_order(&spec, &default_decimals(), 42, 100, zero_nonce());
        assert!(result.is_err());
        match result.unwrap_err() {
            StrategyError::InvalidPrice(_) => {}
            other => panic!("expected InvalidPrice, got {:?}", other),
        }
    }

    // ── T_STRAT_14: validate_orders 3 valid, cpb=3 → all pass

    #[test]
    fn t_strat_14_validate_orders_all_valid() {
        let ctx = make_ctx();
        let specs = vec![
            OrderSpec { pair: "EMM/KAY".into(), side: "buy".into(),
                        price: "0.00001".into(), quantity: "500".into() },
            OrderSpec { pair: "EMM/KAY".into(), side: "buy".into(),
                        price: "0.00001".into(), quantity: "500".into() },
            OrderSpec { pair: "EMM/KAY".into(), side: "buy".into(),
                        price: "0.00001".into(), quantity: "500".into() },
        ];

        let result = validate_orders(&specs, &ctx, zero_nonce);
        assert_eq!(result.orders.len(), 3);
        assert!(result.warnings.is_empty(), "warnings: {:?}", result.warnings);
    }

    // ── T_STRAT_15: 5 orders, cpb=3 → truncated to 3 + warning

    #[test]
    fn t_strat_15_validate_orders_truncated() {
        let ctx = make_ctx();
        let specs: Vec<OrderSpec> = (0..5).map(|_| OrderSpec {
            pair: "EMM/KAY".into(), side: "buy".into(),
            price: "0.00001".into(), quantity: "500".into(),
        }).collect();

        let result = validate_orders(&specs, &ctx, zero_nonce);
        assert_eq!(result.orders.len(), 3);
        assert!(result.warnings.iter().any(|w| w.contains("truncated")));
    }

    // ── T_STRAT_16: order with price=0 → rejected, others pass

    #[test]
    fn t_strat_16_validate_orders_price_zero() {
        let ctx = make_ctx();
        let specs = vec![
            OrderSpec { pair: "EMM/KAY".into(), side: "buy".into(),
                        price: "0.00001".into(), quantity: "500".into() },
            OrderSpec { pair: "EMM/KAY".into(), side: "buy".into(),
                        price: "0".into(), quantity: "500".into() },
        ];

        let result = validate_orders(&specs, &ctx, zero_nonce);
        assert_eq!(result.orders.len(), 1);
        assert!(result.warnings.iter().any(|w| w.contains("price is zero")));
    }

    // ── T_STRAT_17: order with quantity < min_quantity → rejected

    #[test]
    fn t_strat_17_validate_orders_below_min_quantity() {
        let ctx = make_ctx();
        // min_quantity for EMM/KAY = 10_000_000 (100 EMM × 10^5)
        // Order for 50 EMM = 5_000_000 < 10_000_000
        let specs = vec![
            OrderSpec { pair: "EMM/KAY".into(), side: "buy".into(),
                        price: "0.00001".into(), quantity: "50".into() },
        ];

        let result = validate_orders(&specs, &ctx, zero_nonce);
        assert_eq!(result.orders.len(), 0);
        assert!(result.warnings.iter().any(|w| w.contains("min_quantity")));
    }

    // ── T_STRAT_18: buyer outflow exceeds projected balance → later orders rejected

    #[test]
    fn t_strat_18_validate_orders_balance_exceeded() {
        let mut ctx = make_ctx();
        // With 5/5 decimals:
        // "200" EMM at 5 dec = 20_000_000 qty, price "0.000002" at 8 dec = 200
        // exponent = 5 + 8 - 5 = 8, divisor = 100_000_000
        // outflow = 20_000_000 * 200 / 100_000_000 = 40 KAY base units
        // Set projected KAY to 50 so first order fits but second doesn't
        ctx.projected_balances.insert("KAY".to_string(), 50);

        let specs = vec![
            OrderSpec { pair: "EMM/KAY".into(), side: "buy".into(),
                        price: "0.000002".into(), quantity: "200".into() },
            OrderSpec { pair: "EMM/KAY".into(), side: "buy".into(),
                        price: "0.000002".into(), quantity: "200".into() },
        ];

        let result = validate_orders(&specs, &ctx, zero_nonce);
        // First order: outflow 40, cumulative 40 ≤ 50 → pass
        // Second order: outflow 40, cumulative 80 > 50 → rejected
        assert_eq!(result.orders.len(), 1);
        assert!(result.warnings.iter().any(|w| w.contains("exceeds projected")));
    }

    // ── Extra: parse_decimal_to_fixed edge cases

    #[test]
    fn test_parse_decimal_whole_number() {
        assert_eq!(parse_decimal_to_fixed("500", 8).unwrap(), 50_000_000_000);
    }

    #[test]
    fn test_parse_decimal_with_trailing_zeros() {
        assert_eq!(parse_decimal_to_fixed("0.04990000", 8).unwrap(), 4_990_000);
    }

    #[test]
    fn test_parse_decimal_short_fraction() {
        // "0.05" with 8 decimals → 5_000_000
        assert_eq!(parse_decimal_to_fixed("0.05", 8).unwrap(), 5_000_000);
    }

    #[test]
    fn test_parse_decimal_empty_rejects() {
        assert!(parse_decimal_to_fixed("", 8).is_err());
    }

    #[test]
    fn test_parse_decimal_letters_reject() {
        assert!(parse_decimal_to_fixed("abc", 8).is_err());
    }
}
