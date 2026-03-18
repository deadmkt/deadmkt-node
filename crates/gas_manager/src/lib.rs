// =========================================================================
// deadmkt-gas-manager: Gas balance tracking + bond reclaim
// =========================================================================
//
// Two-tier thresholds:
//   Normal: balance ≥ warn_threshold
//   Low:    critical_threshold ≤ balance < warn_threshold
//   Critical: balance < critical_threshold → auto-pause trading

// =========================================================================
// Types
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GasStatus {
    Normal,
    Low,
    Critical,
}

pub struct BondReclaimCheck {
    pub reclaimable: bool,
    pub unlock_at: u64,
    pub current_block: u64,
}

pub struct GasManager {
    balance: u64,
    warn_threshold: u64,
    critical_threshold: u64,
    decimals: u8,
}

// =========================================================================
// Implementation
// =========================================================================

impl GasManager {
    /// Create with thresholds in human-readable units.
    /// Default: warn=10 SUPRA, critical=1 SUPRA.
    pub fn new(decimals: u8) -> Self {
        let factor = 10u64.pow(decimals as u32);
        Self {
            balance: 0,
            warn_threshold: 10 * factor,
            critical_threshold: 1 * factor,
            decimals,
        }
    }

    /// Create with custom thresholds (in smallest units).
    pub fn with_thresholds(
        decimals: u8,
        warn_threshold: u64,
        critical_threshold: u64,
    ) -> Self {
        Self {
            balance: 0,
            warn_threshold,
            critical_threshold,
            decimals,
        }
    }

    pub fn balance(&self) -> u64 {
        self.balance
    }

    pub fn decimals(&self) -> u8 {
        self.decimals
    }

    /// Current gas status based on thresholds.
    pub fn check_status(&self) -> GasStatus {
        if self.balance < self.critical_threshold {
            GasStatus::Critical
        } else if self.balance < self.warn_threshold {
            GasStatus::Low
        } else {
            GasStatus::Normal
        }
    }

    /// Update balance (e.g. from chain read). Returns previous and new status.
    pub fn update_balance(&mut self, new_balance: u64) -> (GasStatus, GasStatus) {
        let old_status = self.check_status();
        self.balance = new_balance;
        let new_status = self.check_status();
        (old_status, new_status)
    }

    /// Format balance as human-readable string (e.g. "50.000000000").
    pub fn balance_display(&self) -> String {
        let factor = 10u64.pow(self.decimals as u32);
        let whole = self.balance / factor;
        let frac = self.balance % factor;
        format!("{}.{:0>width$}", whole, frac, width = self.decimals as usize)
    }

    /// Check if mint bond is reclaimable.
    pub fn check_bond_reclaim(unlock_at: u64, current_block: u64) -> BondReclaimCheck {
        BondReclaimCheck {
            reclaimable: current_block >= unlock_at,
            unlock_at,
            current_block,
        }
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // SUPRA has 9 decimals
    const DECIMALS: u8 = 9;
    const SUPRA: u64 = 1_000_000_000; // 10^9

    // ── T_GAS_01: 50 SUPRA → Normal

    #[test]
    fn t_gas_01_normal_status() {
        let mut gm = GasManager::new(DECIMALS);
        gm.update_balance(50 * SUPRA);
        assert_eq!(gm.check_status(), GasStatus::Normal);
    }

    // ── T_GAS_02: 5 SUPRA → Low

    #[test]
    fn t_gas_02_low_status() {
        let mut gm = GasManager::new(DECIMALS);
        gm.update_balance(5 * SUPRA);
        assert_eq!(gm.check_status(), GasStatus::Low);
    }

    // ── T_GAS_03: 0.5 SUPRA → Critical

    #[test]
    fn t_gas_03_critical_status() {
        let mut gm = GasManager::new(DECIMALS);
        gm.update_balance(SUPRA / 2); // 0.5 SUPRA
        assert_eq!(gm.check_status(), GasStatus::Critical);
    }

    // ── T_GAS_04: Critical → deposit 15 → Normal

    #[test]
    fn t_gas_04_critical_to_normal() {
        let mut gm = GasManager::new(DECIMALS);
        gm.update_balance(SUPRA / 2); // 0.5 SUPRA, Critical
        assert_eq!(gm.check_status(), GasStatus::Critical);

        let (old, new) = gm.update_balance(15 * SUPRA);
        assert_eq!(old, GasStatus::Critical);
        assert_eq!(new, GasStatus::Normal);
    }

    // ── T_GAS_05: Normal → drain to 0.8 → Critical

    #[test]
    fn t_gas_05_normal_to_critical() {
        let mut gm = GasManager::new(DECIMALS);
        gm.update_balance(50 * SUPRA);
        assert_eq!(gm.check_status(), GasStatus::Normal);

        let (old, new) = gm.update_balance(800_000_000); // 0.8 SUPRA
        assert_eq!(old, GasStatus::Normal);
        assert_eq!(new, GasStatus::Critical);
    }

    // ── T_GAS_06: Bond reclaimable

    #[test]
    fn t_gas_06_bond_reclaimable() {
        let check = GasManager::check_bond_reclaim(1000, 1001);
        assert!(check.reclaimable);
        assert_eq!(check.unlock_at, 1000);
        assert_eq!(check.current_block, 1001);
    }

    // ── T_GAS_07: Bond not reclaimable

    #[test]
    fn t_gas_07_bond_not_reclaimable() {
        let check = GasManager::check_bond_reclaim(1000, 999);
        assert!(!check.reclaimable);
    }

    // ── Extra: balance display

    #[test]
    fn test_balance_display() {
        let mut gm = GasManager::new(DECIMALS);
        gm.update_balance(50_123_456_789);
        assert_eq!(gm.balance_display(), "50.123456789");
    }

    // ── Extra: zero balance

    #[test]
    fn test_zero_balance() {
        let gm = GasManager::new(DECIMALS);
        assert_eq!(gm.check_status(), GasStatus::Critical);
        assert_eq!(gm.balance(), 0);
    }

    // ── Extra: exact threshold boundary

    #[test]
    fn test_exact_threshold_boundary() {
        let mut gm = GasManager::new(DECIMALS);

        // Exactly at critical → Low (not Critical)
        gm.update_balance(1 * SUPRA);
        assert_eq!(gm.check_status(), GasStatus::Low);

        // Exactly at warn → Normal (not Low)
        gm.update_balance(10 * SUPRA);
        assert_eq!(gm.check_status(), GasStatus::Normal);
    }
}
