// crates/profit/src/lib.rs
//
// Build 4E: Auto profit transfer to beneficiary.
//
// After settlement confirms:
//   1. Check if confirmed balance exceeds base_capital + threshold
//   2. Safety check: projected stays non-negative after transfer (CD-10)
//   3. Submit transfer_to_beneficiary tx
//
// Bond auto-reclaim: background check hourly, reclaim when lock expires.
//
// Config (from NodeConfig.profit_taking):
//   - base_capital: per-token starting capital
//   - threshold_pct: % above base_capital to trigger transfer
//   - transfer_mode: "all" | "fixed" | "percentage"
//   - transfer_mode_value: amount for fixed/percentage modes

use deadmkt_config::ProfitConfig;
use deadmkt_escrow_tracker::EscrowTracker;
use std::sync::{Arc, Mutex};
use thiserror::Error;

// =========================================================================
// Errors
// =========================================================================

#[derive(Error, Debug)]
pub enum ProfitError {
    #[error("chain error: {0}")]
    Chain(String),

    #[error("transfer would starve in-flight settlements")]
    WouldStarveSettlements,

    #[error("below profit threshold")]
    BelowThreshold,
}

// =========================================================================
// Profit action
// =========================================================================

/// Outcome of a profit check.
#[derive(Debug, Clone, PartialEq)]
pub enum ProfitAction {
    /// No action needed — below threshold.
    NoAction,
    /// Transfer this amount to beneficiary.
    Transfer { token: String, amount: u64 },
    /// Profit exists but unsafe to transfer right now.
    SkippedUnsafe { token: String, reason: String },
}

// =========================================================================
// ProfitManager
// =========================================================================

/// Manages auto profit-taking and bond reclaim.
pub struct ProfitManager {
    config: ProfitConfig,
    tracker: Arc<Mutex<EscrowTracker>>,
}

impl ProfitManager {
    pub fn new(
        config: ProfitConfig,
        tracker: Arc<Mutex<EscrowTracker>>,
    ) -> Self {
        Self { config, tracker }
    }

    /// Check if a token exceeds profit threshold.
    /// Threshold: confirmed > base_capital × (1 + threshold_pct / 100)
    pub fn check_threshold(&self, token: &str, confirmed: u64) -> ProfitAction {
        let base = self.base_capital(token);
        if base == 0 {
            return ProfitAction::NoAction;
        }

        let threshold = base + (base as u128 * self.config.threshold_pct as u128 / 100) as u64;

        if confirmed <= threshold {
            return ProfitAction::NoAction;
        }

        let profit = confirmed - base;
        let amount = self.calculate_transfer_amount(profit);

        if amount == 0 {
            return ProfitAction::NoAction;
        }

        ProfitAction::Transfer {
            token: token.to_string(),
            amount,
        }
    }

    /// Calculate transfer amount based on transfer_mode.
    /// Does NOT apply safety cap yet — that's done in check_and_transfer.
    fn calculate_transfer_amount(&self, profit: u64) -> u64 {
        match self.config.transfer_mode.as_str() {
            "all" => profit,
            "fixed" => {
                let fixed = self.config.transfer_mode_value.unwrap_or(0);
                std::cmp::min(fixed, profit)
            }
            "percentage" => {
                let pct = self.config.transfer_mode_value.unwrap_or(0);
                (profit as u128 * pct as u128 / 100) as u64
            }
            _ => profit, // fallback: transfer all
        }
    }

    /// Full profit check cycle: threshold → calculate → safety → capped amount.
    /// Called after each settlement confirmation.
    pub fn check_and_transfer(&self, token: &str, confirmed: u64) -> ProfitAction {
        // Step 1: threshold check
        let action = self.check_threshold(token, confirmed);
        let raw_amount = match &action {
            ProfitAction::Transfer { amount, .. } => *amount,
            other => return other.clone(),
        };

        // Step 2: safety cap (CD-10)
        let base = self.base_capital(token);
        let safe_max = {
            let tracker = self.tracker.lock().unwrap();
            tracker.safe_transfer_max(token, base)
        };

        if safe_max == 0 {
            return ProfitAction::SkippedUnsafe {
                token: token.to_string(),
                reason: "safe_transfer_max is 0 — pending outflows + earmarked too high".into(),
            };
        }

        let capped = std::cmp::min(raw_amount, safe_max);

        ProfitAction::Transfer {
            token: token.to_string(),
            amount: capped,
        }
    }

    /// Get base_capital for a token from config.
    fn base_capital(&self, token: &str) -> u64 {
        self.config.base_capital.get(token).copied().unwrap_or(0)
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_config(
        base_capital_tusdc: u64,
        threshold_pct: u32,
        mode: &str,
        mode_value: Option<u64>,
    ) -> ProfitConfig {
        let mut base_capital = HashMap::new();
        base_capital.insert("KAY".to_string(), base_capital_tusdc);
        ProfitConfig {
            base_capital,
            threshold_pct,
            transfer_mode: mode.to_string(),
            transfer_mode_value: mode_value,
        }
    }

    fn make_manager(
        config: ProfitConfig,
    ) -> (ProfitManager, Arc<Mutex<EscrowTracker>>) {
        let tracker = Arc::new(Mutex::new(EscrowTracker::new()));
        let mgr = ProfitManager::new(config, tracker.clone());
        (mgr, tracker)
    }

    // ─── T_PROFIT_01: Below threshold → NoAction

    #[test]
    fn t_profit_01_below_threshold() {
        let config = make_config(5_000, 10, "all", None);
        let (mgr, _tracker) = make_manager(config);

        // Threshold = 5000 + 10% = 5500. Confirmed = 5400 < 5500.
        let action = mgr.check_threshold("KAY", 5_400);
        assert_eq!(action, ProfitAction::NoAction);
    }

    // ─── T_PROFIT_02: Above threshold → Transfer

    #[test]
    fn t_profit_02_above_threshold() {
        let config = make_config(5_000, 10, "all", None);
        let (mgr, _tracker) = make_manager(config);

        // Threshold = 5500. Confirmed = 5600 > 5500.
        // Profit = 5600 - 5000 = 600. Mode "all" → transfer 600.
        let action = mgr.check_threshold("KAY", 5_600);
        assert_eq!(action, ProfitAction::Transfer { token: "KAY".into(), amount: 600 });
    }

    // ─── T_PROFIT_03: transfer_mode "all" → transfer all profit above base_capital

    #[test]
    fn t_profit_03_mode_all() {
        let config = make_config(5_000, 10, "all", None);
        let (mgr, _tracker) = make_manager(config);

        // Confirmed = 6000. Profit = 1000.
        let action = mgr.check_threshold("KAY", 6_000);
        assert_eq!(action, ProfitAction::Transfer { token: "KAY".into(), amount: 1_000 });
    }

    // ─── T_PROFIT_04: transfer_mode "fixed" → transfer_mode_value per cycle

    #[test]
    fn t_profit_04_mode_fixed() {
        let config = make_config(5_000, 10, "fixed", Some(200));
        let (mgr, _tracker) = make_manager(config);

        // Profit = 1000, but fixed = 200.
        let action = mgr.check_threshold("KAY", 6_000);
        assert_eq!(action, ProfitAction::Transfer { token: "KAY".into(), amount: 200 });
    }

    // ─── T_PROFIT_05: transfer_mode "percentage" → % of profit

    #[test]
    fn t_profit_05_mode_percentage() {
        let config = make_config(5_000, 10, "percentage", Some(50));
        let (mgr, _tracker) = make_manager(config);

        // Profit = 1000, 50% = 500.
        let action = mgr.check_threshold("KAY", 6_000);
        assert_eq!(action, ProfitAction::Transfer { token: "KAY".into(), amount: 500 });
    }

    // ─── T_PROFIT_06: transfer capped by safe_transfer_max

    #[test]
    fn t_profit_06_capped_by_safe_max() {
        let config = make_config(5_000, 10, "all", None);
        let (mgr, tracker) = make_manager(config);

        // confirmed=6000, pending_out=500, earmarked=100
        // projected = 6000 - 500 - 100 = 5400
        // safe_max = 5400 - 5000(base) = 400
        // profit = 1000, but capped to 400
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 6_000);
            t.reserve_outflow("KAY", "m1", 500).unwrap();
            t.earmark_withdrawal("KAY", 100).unwrap();
        }

        let action = mgr.check_and_transfer("KAY", 6_000);
        assert_eq!(action, ProfitAction::Transfer { token: "KAY".into(), amount: 400 });
    }

    // ─── T_PROFIT_07: safe_max ≤ 0 → SkippedUnsafe

    #[test]
    fn t_profit_07_skipped_unsafe() {
        let config = make_config(5_000, 10, "all", None);
        let (mgr, tracker) = make_manager(config);

        // confirmed=5600, pending_out=500, earmarked=200
        // projected = 5600 - 500 - 200 = 4900
        // safe_max = 4900 - 5000(base) = saturates to 0
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 5_600);
            t.reserve_outflow("KAY", "m1", 500).unwrap();
            t.earmark_withdrawal("KAY", 200).unwrap();
        }

        let action = mgr.check_and_transfer("KAY", 5_600);
        match action {
            ProfitAction::SkippedUnsafe { token, .. } => assert_eq!(token, "KAY"),
            other => panic!("Expected SkippedUnsafe, got {:?}", other),
        }
    }

    // ─── T_PROFIT_08: check_and_transfer full cycle with safe cap

    #[test]
    fn t_profit_08_full_cycle_safe() {
        let config = make_config(5_000, 10, "fixed", Some(300));
        let (mgr, tracker) = make_manager(config);

        // confirmed=7000, no pending, no earmark
        // projected = 7000, safe_max = 7000 - 5000 = 2000
        // profit = 2000, fixed = 300, capped = min(300, 2000) = 300
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 7_000);
        }

        let action = mgr.check_and_transfer("KAY", 7_000);
        assert_eq!(action, ProfitAction::Transfer { token: "KAY".into(), amount: 300 });
    }
}
