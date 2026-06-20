// crates/escrow_tracker/src/lib.rs
//
// Build 4C: Projected balance tracking.
//
// Formula: projected() = confirmed - pending_out - earmarked
//
// CD-6: Outflows are worst-case, inflows are confirmed-only.
//   - Buyer outflow: fill_quantity * buyer_price / 1e8 (worst-case)
//   - Seller outflow: fill_quantity (base token, exact)
//   - Inflows NOT counted until BatchTradeSettled event confirmed on-chain.
//
// CD-11: Each match touches TWO tokens. apply_match() handles both sides.
//
// Safety invariant: projected() NEVER goes negative (saturating sub).
// On confirmation: pending_out removed, pending_in applied to confirmed.
// On failed/expired: pending_out reversed, pending_in dropped.

use std::collections::HashMap;
use thiserror::Error;

pub mod is_active_cache;
pub use is_active_cache::IsActiveCache;

// =========================================================================
// Errors
// =========================================================================

#[derive(Error, Debug, PartialEq)]
pub enum EscrowError {
    #[error("insufficient projected balance: need {need}, have {have}")]
    InsufficientBalance { need: u64, have: u64 },

    #[error("unknown token: {0}")]
    UnknownToken(String),

    #[error("duplicate pending entry: {0}")]
    DuplicatePending(String),

    #[error("pending entry not found: {0}")]
    PendingNotFound(String),

    #[error("earmark exceeds projected: need {need}, have {have}")]
    EarmarkExceedsProjected { need: u64, have: u64 },
}

// =========================================================================
// Side enum (buyer or seller)
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Side {
    Buy,
    Sell,
}

// =========================================================================
// TokenBalance
// =========================================================================

/// Per-token balance state.
#[derive(Debug, Clone, Default)]
pub struct TokenBalance {
    /// On-chain confirmed balance (from escrow view function).
    pub confirmed: u64,
    /// Outflows from matched-but-unconfirmed settlements.
    /// Key: match_hash hex, Value: worst-case outflow amount.
    pub pending_out: HashMap<String, u64>,
    /// Inflows from matched-but-unconfirmed settlements.
    /// Tracked but NOT counted in projected() until confirmed.
    /// Key: match_hash hex, Value: expected inflow amount.
    pub pending_in: HashMap<String, u64>,
    /// Earmarked for pending withdrawals.
    pub earmarked: u64,
}

impl TokenBalance {
    /// Projected available balance.
    /// confirmed - sum(pending_out) - earmarked
    /// Always saturating — never negative.
    pub fn projected(&self) -> u64 {
        let total_pending_out: u64 = self.pending_out.values().sum();
        self.confirmed
            .saturating_sub(total_pending_out)
            .saturating_sub(self.earmarked)
    }

    /// Total pending outflow.
    pub fn total_pending_out(&self) -> u64 {
        self.pending_out.values().sum()
    }

    /// Total pending inflow (not yet counted).
    pub fn total_pending_in(&self) -> u64 {
        self.pending_in.values().sum()
    }
}

// =========================================================================
// EscrowTracker
// =========================================================================

/// Tracks projected balances across all tokens for this node's NFT.
#[derive(Debug, Default)]
pub struct EscrowTracker {
    /// Per-token balance tracking. Key: token identifier (e.g. "EMM", "KAY").
    balances: HashMap<String, TokenBalance>,
}

impl EscrowTracker {
    pub fn new() -> Self {
        Self {
            balances: HashMap::new(),
        }
    }

    /// Set confirmed balance from chain read.
    /// Called during startup sync and after settlement confirmations.
    pub fn set_confirmed(&mut self, token: &str, amount: u64) {
        self.balances
            .entry(token.to_string())
            .or_default()
            .confirmed = amount;
    }

    /// Reserve outflow for a matched trade (pending settlement).
    /// Returns error if projected would go negative.
    pub fn reserve_outflow(
        &mut self,
        token: &str,
        match_hash: &str,
        amount: u64,
    ) -> Result<(), EscrowError> {
        let bal = self.balances.entry(token.to_string()).or_default();

        // Reject duplicate
        if bal.pending_out.contains_key(match_hash) {
            return Err(EscrowError::DuplicatePending(match_hash.to_string()));
        }

        // Check affordability
        if amount > bal.projected() {
            return Err(EscrowError::InsufficientBalance {
                need: amount,
                have: bal.projected(),
            });
        }

        bal.pending_out.insert(match_hash.to_string(), amount);
        Ok(())
    }

    /// Track expected inflow for a matched trade.
    /// NOT counted in projected() until confirm_settlement().
    pub fn track_inflow(
        &mut self,
        token: &str,
        match_hash: &str,
        amount: u64,
    ) -> Result<(), EscrowError> {
        let bal = self.balances.entry(token.to_string()).or_default();

        if bal.pending_in.contains_key(match_hash) {
            return Err(EscrowError::DuplicatePending(match_hash.to_string()));
        }

        bal.pending_in.insert(match_hash.to_string(), amount);
        Ok(())
    }

    /// Settlement confirmed on-chain.
    /// Remove pending_out across all tokens for this match_hash.
    /// Apply pending_in to confirmed across all tokens for this match_hash.
    pub fn confirm_settlement(
        &mut self,
        match_hash: &str,
    ) -> Result<(), EscrowError> {
        let mut found = false;

        for bal in self.balances.values_mut() {
            if bal.pending_out.remove(match_hash).is_some() {
                found = true;
            }
            if let Some(inflow) = bal.pending_in.remove(match_hash) {
                bal.confirmed += inflow;
                found = true;
            }
        }

        if !found {
            return Err(EscrowError::PendingNotFound(match_hash.to_string()));
        }
        Ok(())
    }

    /// Settlement failed/expired: reverse pending_out, drop pending_in.
    pub fn release_pending(
        &mut self,
        match_hash: &str,
    ) -> Result<(), EscrowError> {
        let mut found = false;

        for bal in self.balances.values_mut() {
            if bal.pending_out.remove(match_hash).is_some() {
                found = true;
            }
            if bal.pending_in.remove(match_hash).is_some() {
                found = true;
            }
        }

        if !found {
            return Err(EscrowError::PendingNotFound(match_hash.to_string()));
        }
        Ok(())
    }

    /// Earmark funds for a pending withdrawal.
    pub fn earmark_withdrawal(
        &mut self,
        token: &str,
        amount: u64,
    ) -> Result<(), EscrowError> {
        let bal = self.balances.entry(token.to_string()).or_default();

        if amount > bal.projected() {
            return Err(EscrowError::EarmarkExceedsProjected {
                need: amount,
                have: bal.projected(),
            });
        }

        bal.earmarked += amount;
        Ok(())
    }

    /// Release earmark.
    /// If executed=false: cancelled — just remove earmark.
    /// If executed=true: withdrawal executed on-chain — reduce confirmed too.
    pub fn release_earmark(
        &mut self,
        token: &str,
        amount: u64,
        executed: bool,
    ) -> Result<(), EscrowError> {
        let bal = self.balances.get_mut(token)
            .ok_or_else(|| EscrowError::UnknownToken(token.to_string()))?;

        bal.earmarked = bal.earmarked.saturating_sub(amount);

        if executed {
            bal.confirmed = bal.confirmed.saturating_sub(amount);
        }

        Ok(())
    }

    /// Get projected balance for a token (0 if unknown token).
    pub fn projected(&self, token: &str) -> u64 {
        self.balances.get(token).map_or(0, |b| b.projected())
    }

    /// Get full balance state for a token.
    pub fn get_balance(&self, token: &str) -> Option<&TokenBalance> {
        self.balances.get(token)
    }

    /// Check if a trade outflow is affordable.
    pub fn can_afford(&self, token: &str, amount: u64) -> bool {
        self.projected(token) >= amount
    }

    /// Compute max safe transfer amount for profit-taking (CD-10).
    /// safe_max = projected - base_capital
    /// Returns 0 if unsafe.
    pub fn safe_transfer_max(&self, token: &str, base_capital: u64) -> u64 {
        self.projected(token).saturating_sub(base_capital)
    }

    /// Apply a Match to the tracker for this node's side (CD-11).
    ///
    /// Buyer of BASE/QUOTE:
    ///   outflow = fill_qty * buyer_price / 1e8 on QUOTE token
    ///   inflow  = fill_qty on BASE token
    /// Seller of BASE/QUOTE:
    ///   outflow = fill_qty on BASE token
    ///   inflow  = fill_qty * settlement_price / 1e8 on QUOTE token
    pub fn apply_match(
        &mut self,
        match_hash: &str,
        my_side: Side,
        base_token: &str,
        quote_token: &str,
        fill_quantity: u64,
        buyer_price: u64,
        settlement_price: u64,
        base_decimals: u8,
        quote_decimals: u8,
    ) -> Result<(), EscrowError> {
        const PRICE_DECIMALS: u32 = 8; // prices are always 8-decimal
        let exponent = base_decimals as u32 + PRICE_DECIMALS - quote_decimals as u32;
        let divisor = 10u128.pow(exponent);
        eprintln!("  [escrow] apply_match: base_dec={} quote_dec={} price_dec={} exponent={} divisor={}",
            base_decimals, quote_decimals, PRICE_DECIMALS, exponent, divisor);

        match my_side {
            Side::Buy => {
                let quote_outflow = (fill_quantity as u128 * buyer_price as u128
                    / divisor) as u64;
                eprintln!("  [escrow] buy outflow: {} * {} / {} = {} (token: {})",
                    fill_quantity, buyer_price, divisor, quote_outflow, quote_token);
                self.reserve_outflow(quote_token, match_hash, quote_outflow)?;
                self.track_inflow(base_token, match_hash, fill_quantity)?;
            }
            Side::Sell => {
                let quote_inflow = (fill_quantity as u128 * settlement_price as u128
                    / divisor) as u64;
                self.reserve_outflow(base_token, match_hash, fill_quantity)?;
                self.track_inflow(quote_token, match_hash, quote_inflow)?;
            }
        }
        Ok(())
    }

    /// List all token identifiers being tracked.
    pub fn tracked_tokens(&self) -> Vec<String> {
        self.balances.keys().cloned().collect()
    }

    /// Check if any pending entries exist for a given match_hash.
    pub fn has_pending(&self, match_hash: &str) -> bool {
        self.balances.values().any(|b| {
            b.pending_out.contains_key(match_hash) || b.pending_in.contains_key(match_hash)
        })
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // T_ESCROW_01: set_confirmed initializes token balance
    #[test]
    fn t_escrow_01_set_confirmed() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 100_000_000);
        assert_eq!(tracker.projected("KAY"), 100_000_000);
    }

    // T_ESCROW_02: reserve_outflow reduces projected
    #[test]
    fn t_escrow_02_reserve_outflow() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 100_000_000);
        tracker.reserve_outflow("KAY", "m1", 30_000_000).unwrap();
        assert_eq!(tracker.projected("KAY"), 70_000_000);
    }

    // T_ESCROW_03: reserve_outflow rejects insufficient balance
    #[test]
    fn t_escrow_03_reserve_outflow_insufficient() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 10_000_000);
        let err = tracker.reserve_outflow("KAY", "m1", 50_000_000).unwrap_err();
        assert!(matches!(err, EscrowError::InsufficientBalance { need: 50_000_000, have: 10_000_000 }));
    }

    // T_ESCROW_04: track_inflow does NOT increase projected
    #[test]
    fn t_escrow_04_inflow_not_counted() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("EMM", 500);
        tracker.track_inflow("EMM", "m1", 200).unwrap();
        assert_eq!(tracker.projected("EMM"), 500);
        assert_eq!(tracker.get_balance("EMM").unwrap().total_pending_in(), 200);
    }

    // T_ESCROW_05: confirm_settlement moves pending → confirmed
    #[test]
    fn t_escrow_05_confirm_settlement() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 100);
        tracker.reserve_outflow("KAY", "m1", 30).unwrap();
        tracker.track_inflow("EMM", "m1", 500).unwrap();

        assert_eq!(tracker.projected("KAY"), 70);
        assert_eq!(tracker.projected("EMM"), 0);

        tracker.confirm_settlement("m1").unwrap();

        assert_eq!(tracker.projected("KAY"), 100);
        assert_eq!(tracker.projected("EMM"), 500);
        assert!(tracker.get_balance("KAY").unwrap().pending_out.is_empty());
        assert!(tracker.get_balance("EMM").unwrap().pending_in.is_empty());
    }

    // T_ESCROW_06: release_pending reverses outflow, drops inflow
    #[test]
    fn t_escrow_06_release_pending() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 100);
        tracker.reserve_outflow("KAY", "m1", 30).unwrap();
        tracker.track_inflow("EMM", "m1", 500).unwrap();

        tracker.release_pending("m1").unwrap();

        assert_eq!(tracker.projected("KAY"), 100);
        assert_eq!(tracker.get_balance("EMM").unwrap().pending_in.len(), 0);
    }

    // T_ESCROW_07: earmark_withdrawal reduces projected
    #[test]
    fn t_escrow_07_earmark_withdrawal() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 100);
        tracker.earmark_withdrawal("KAY", 30).unwrap();
        assert_eq!(tracker.projected("KAY"), 70);
    }

    // T_ESCROW_08: release_earmark restores projected (cancelled)
    #[test]
    fn t_escrow_08_release_earmark_cancelled() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 100);
        tracker.earmark_withdrawal("KAY", 30).unwrap();
        tracker.release_earmark("KAY", 30, false).unwrap();
        assert_eq!(tracker.projected("KAY"), 100);
    }

    // T_ESCROW_09: release_earmark executed reduces confirmed
    #[test]
    fn t_escrow_09_release_earmark_executed() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 100);
        tracker.earmark_withdrawal("KAY", 30).unwrap();
        tracker.release_earmark("KAY", 30, true).unwrap();
        assert_eq!(tracker.get_balance("KAY").unwrap().confirmed, 70);
        assert_eq!(tracker.get_balance("KAY").unwrap().earmarked, 0);
        assert_eq!(tracker.projected("KAY"), 70);
    }

    // T_ESCROW_10: projected never negative (saturating)
    #[test]
    fn t_escrow_10_projected_saturating() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 10);
        tracker.balances.get_mut("KAY").unwrap().pending_out.insert("m1".into(), 100);
        assert_eq!(tracker.projected("KAY"), 0);
    }

    // T_ESCROW_11: unknown token returns 0
    #[test]
    fn t_escrow_11_unknown_token() {
        let tracker = EscrowTracker::new();
        assert_eq!(tracker.projected("NONEXISTENT"), 0);
    }

    // T_ESCROW_12: apply_match as BUYER — quote outflow, base inflow
    #[test]
    fn t_escrow_12_apply_match_buyer() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 10_000);
        tracker.set_confirmed("EMM", 0);

        tracker.apply_match("m1", Side::Buy, "EMM", "KAY", 1000, 5_000_000, 4_900_000, 5, 5).unwrap();

        // outflow = 1000 * 5_000_000 / 100_000_000 = 50 KAY
        assert_eq!(tracker.projected("KAY"), 10_000 - 50);
        assert_eq!(tracker.projected("EMM"), 0); // inflow not counted
        assert_eq!(tracker.get_balance("EMM").unwrap().total_pending_in(), 1000);
    }

    // T_ESCROW_13: apply_match as SELLER — base outflow, quote inflow
    #[test]
    fn t_escrow_13_apply_match_seller() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("EMM", 5000);
        tracker.set_confirmed("KAY", 0);

        tracker.apply_match("m1", Side::Sell, "EMM", "KAY", 1000, 5_000_000, 4_900_000, 5, 5).unwrap();

        assert_eq!(tracker.projected("EMM"), 5000 - 1000);
        assert_eq!(tracker.projected("KAY"), 0);
        // inflow = 1000 * 4_900_000 / 100_000_000 = 49
        assert_eq!(tracker.get_balance("KAY").unwrap().total_pending_in(), 49);
    }

    // T_ESCROW_14: multiple partial fills — cumulative outflow
    #[test]
    fn t_escrow_14_multiple_partial_fills() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 200);

        tracker.apply_match("m1", Side::Buy, "EMM", "KAY", 1000, 5_000_000, 4_900_000, 5, 5).unwrap();
        tracker.apply_match("m2", Side::Buy, "EMM", "KAY", 800, 5_200_000, 5_100_000, 5, 5).unwrap();

        // m1 outflow = 50, m2 outflow = 800*5200000/1e8 = 41
        assert_eq!(tracker.projected("KAY"), 200 - 50 - 41);
        assert_eq!(tracker.get_balance("KAY").unwrap().pending_out.len(), 2);
    }

    // T_ESCROW_15: safe_transfer_max respects all deductions
    #[test]
    fn t_escrow_15_safe_transfer_max() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 1000);
        tracker.reserve_outflow("KAY", "m1", 300).unwrap();
        tracker.earmark_withdrawal("KAY", 100).unwrap();
        // projected = 600, safe_max = 600 - 400 = 200
        assert_eq!(tracker.safe_transfer_max("KAY", 400), 200);
    }

    // T_ESCROW_16: safe_transfer_max returns 0 when unsafe
    #[test]
    fn t_escrow_16_safe_transfer_max_zero() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 500);
        tracker.reserve_outflow("KAY", "m1", 300).unwrap();
        tracker.earmark_withdrawal("KAY", 100).unwrap();
        // projected = 100, safe_max = 100 - 400 → 0
        assert_eq!(tracker.safe_transfer_max("KAY", 400), 0);
    }

    // T_ESCROW_17: can_afford checks projected
    #[test]
    fn t_escrow_17_can_afford() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 100);
        tracker.reserve_outflow("KAY", "m1", 40).unwrap();
        tracker.earmark_withdrawal("KAY", 10).unwrap();
        // projected = 50
        assert!(tracker.can_afford("KAY", 50));
        assert!(!tracker.can_afford("KAY", 51));
    }

    // T_ESCROW_18: duplicate pending entry rejected
    #[test]
    fn t_escrow_18_duplicate_pending() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 1000);
        tracker.reserve_outflow("KAY", "m1", 30).unwrap();
        let err = tracker.reserve_outflow("KAY", "m1", 20).unwrap_err();
        assert!(matches!(err, EscrowError::DuplicatePending(_)));
    }

    // ─── TokenBalance unit tests ───────────────────────────────────

    #[test]
    fn test_token_balance_projected_basic() {
        let mut bal = TokenBalance::default();
        bal.confirmed = 100;
        assert_eq!(bal.projected(), 100);
    }

    #[test]
    fn test_token_balance_projected_with_pending() {
        let mut bal = TokenBalance::default();
        bal.confirmed = 100;
        bal.pending_out.insert("m1".into(), 30);
        bal.pending_out.insert("m2".into(), 20);
        bal.earmarked = 10;
        assert_eq!(bal.projected(), 40);
    }

    #[test]
    fn test_token_balance_saturating_sub() {
        let mut bal = TokenBalance::default();
        bal.confirmed = 10;
        bal.pending_out.insert("m1".into(), 100);
        assert_eq!(bal.projected(), 0);
    }

    // ─── Extra: utility helpers ────────────────────────────────────

    #[test]
    fn test_has_pending() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 1000);
        assert!(!tracker.has_pending("m1"));
        tracker.reserve_outflow("KAY", "m1", 50).unwrap();
        assert!(tracker.has_pending("m1"));
        tracker.release_pending("m1").unwrap();
        assert!(!tracker.has_pending("m1"));
    }

    #[test]
    fn test_confirm_nonexistent() {
        let mut tracker = EscrowTracker::new();
        let err = tracker.confirm_settlement("phantom").unwrap_err();
        assert!(matches!(err, EscrowError::PendingNotFound(_)));
    }

    #[test]
    fn test_full_lifecycle() {
        let mut tracker = EscrowTracker::new();
        tracker.set_confirmed("KAY", 5000);
        tracker.set_confirmed("EMM", 100_000);

        tracker.apply_match("m1", Side::Buy, "EMM", "KAY", 1000, 5_000_000, 4_900_000, 5, 5).unwrap();
        assert_eq!(tracker.projected("KAY"), 4950);
        assert_eq!(tracker.projected("EMM"), 100_000);

        tracker.confirm_settlement("m1").unwrap();
        assert_eq!(tracker.projected("KAY"), 5000);
        assert_eq!(tracker.projected("EMM"), 101_000);

        assert_eq!(tracker.safe_transfer_max("EMM", 100_000), 1000);
    }
}
