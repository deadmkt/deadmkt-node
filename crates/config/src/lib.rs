// deadmkt-config: config.json load/save/validate.
// Node configuration, first-boot detection, profit/withdrawal rules.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("validation error: {0}")]
    ValidationError(String),
}

// =========================================================================
// Network enum
// =========================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Testnet,
    Mainnet,
}

// =========================================================================
// Config structs
// =========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractAddresses {
    pub settlement: String,
    pub escrow: String,
    pub nft: String,
    pub pool_config: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfitConfig {
    pub base_capital: HashMap<String, u64>,
    pub threshold_pct: u32,
    pub transfer_mode: String,
    pub transfer_mode_value: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WithdrawalConfig {
    pub holding_period_days: u32,
    pub rushed_withdrawal_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub network: Network,
    pub rpc_urls: Vec<String>,
    pub nft_id: u64,
    pub trustee_address: String,
    pub beneficiary_address: String,
    pub sponsor_address: String,
    pub contracts: ContractAddresses,
    pub bootstrap_peers: Vec<String>,
    pub strategy_auth_token: String,
    pub markets: Vec<String>,
    pub token_decimals: HashMap<String, u8>,
    pub profit_taking: ProfitConfig,
    pub withdrawal_rules: WithdrawalConfig,
    #[serde(default = "default_strategy_port")]
    pub strategy_port: u16,
    #[serde(default = "default_gossip_port")]
    pub gossip_port: u16,
    #[serde(default = "default_chain_id")]
    pub chain_id: u8,
    #[serde(default = "default_max_gas")]
    pub max_gas_amount: u64,
    #[serde(default = "default_gas_price")]
    pub gas_unit_price: u64,
    pub created_at: String,
}

fn default_strategy_port() -> u16 { 9090 }
fn default_gossip_port() -> u16 { 9191 }
fn default_chain_id() -> u8 { 6 } // Supra testnet
fn default_max_gas() -> u64 { 200 }
fn default_gas_price() -> u64 { 100000 }

// =========================================================================
// Default bootstrap peers
// =========================================================================

fn default_bootstrap_peers(network: &Network) -> Vec<String> {
    match network {
        Network::Testnet => vec![
            "peer1.testnet.deadmkt.com:9191".into(),
            "peer2.testnet.deadmkt.com:9191".into(),
            "peer3.testnet.deadmkt.com:9191".into(),
            "peer4.testnet.deadmkt.com:9191".into(),
            "peer5.testnet.deadmkt.com:9191".into(),
        ],
        Network::Mainnet => vec![
            "peer1.deadmkt.com:9191".into(),
            "peer2.deadmkt.com:9191".into(),
            "peer3.deadmkt.com:9191".into(),
            "peer4.deadmkt.com:9191".into(),
            "peer5.deadmkt.com:9191".into(),
        ],
    }
}

fn default_rpc_urls(network: &Network) -> Vec<String> {
    match network {
        Network::Testnet => vec![
            "https://rpc-testnet.supra.com".into(),
            "https://rpc-testnet2.supra.com".into(),
        ],
        Network::Mainnet => vec![
            "https://rpc-mainnet.supra.com".into(),
        ],
    }
}

/// Default on-chain contract (package) addresses per network.
/// All modules live under a single deployer address.
pub fn default_contract_addresses(network: &Network) -> ContractAddresses {
    let addr = match network {
        Network::Testnet => "0xc4b49db5a93d5cc419b2a2af168b553016e8d509b6aff74d2d5e29f8e7c74e64",
        Network::Mainnet => "", // TBD at mainnet launch
    };
    ContractAddresses {
        settlement: addr.into(),
        escrow: addr.into(),
        nft: addr.into(),
        pool_config: addr.into(),
    }
}

// =========================================================================
// Implementation
// =========================================================================

impl NodeConfig {
    /// Create a new config from wizard inputs.
    pub fn new(
        network: Network,
        nft_id: u64,
        trustee_address: String,
        beneficiary_address: String,
        sponsor_address: String,
        markets: Vec<String>,
    ) -> Self {
        let mut rng = rand::thread_rng();
        let mut token_bytes = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rng, &mut token_bytes);

        let contracts = default_contract_addresses(&network);

        let mut config = NodeConfig {
            rpc_urls: default_rpc_urls(&network),
            bootstrap_peers: default_bootstrap_peers(&network),
            network,
            nft_id,
            trustee_address,
            beneficiary_address,
            sponsor_address,
            contracts,
            strategy_auth_token: hex::encode(token_bytes),
            markets,
            token_decimals: HashMap::new(),
            profit_taking: ProfitConfig {
                base_capital: HashMap::new(),
                threshold_pct: 20,
                transfer_mode: "all_excess".into(),
                transfer_mode_value: None,
            },
            withdrawal_rules: WithdrawalConfig {
                holding_period_days: 90,
                rushed_withdrawal_enabled: true,
            },
            strategy_port: default_strategy_port(),
            gossip_port: default_gossip_port(),
            chain_id: default_chain_id(),
            max_gas_amount: default_max_gas(),
            gas_unit_price: default_gas_price(),
            created_at: chrono_now(),
        };

        config.apply_env_overrides();
        config
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)?;
        let mut config: NodeConfig = serde_json::from_str(&contents)?;

        // Migrate: backfill empty contract addresses from network defaults
        let defaults = default_contract_addresses(&config.network);
        if config.contracts.settlement.is_empty() {
            config.contracts.settlement = defaults.settlement;
        }
        if config.contracts.escrow.is_empty() {
            config.contracts.escrow = defaults.escrow;
        }
        if config.contracts.nft.is_empty() {
            config.contracts.nft = defaults.nft;
        }
        if config.contracts.pool_config.is_empty() {
            config.contracts.pool_config = defaults.pool_config;
        }

        // Env override: highest priority — allows redeployments without rebuild
        config.apply_env_overrides();

        Ok(config)
    }

    /// Apply environment variable overrides to contract addresses.
    ///
    /// `DEADMKT_CONTRACT_ADDR` overrides ALL four contract module addresses
    /// (settlement, escrow, nft, pool_config) since they share a single
    /// deployer address. This lets operators point at a new deployment
    /// via docker-compose env without editing config.json or rebuilding.
    ///
    /// Individual overrides (`DEADMKT_SETTLEMENT_ADDR`, etc.) are checked
    /// second and take precedence over the blanket override.
    pub fn apply_env_overrides(&mut self) {
        // Blanket override: all modules share one deployer address
        if let Ok(addr) = std::env::var("DEADMKT_CONTRACT_ADDR") {
            if !addr.is_empty() {
                self.contracts.settlement = addr.clone();
                self.contracts.escrow = addr.clone();
                self.contracts.nft = addr.clone();
                self.contracts.pool_config = addr;
            }
        }

        // Per-module overrides (rare, but useful for split deployments)
        if let Ok(addr) = std::env::var("DEADMKT_SETTLEMENT_ADDR") {
            if !addr.is_empty() { self.contracts.settlement = addr; }
        }
        if let Ok(addr) = std::env::var("DEADMKT_ESCROW_ADDR") {
            if !addr.is_empty() { self.contracts.escrow = addr; }
        }
        if let Ok(addr) = std::env::var("DEADMKT_NFT_ADDR") {
            if !addr.is_empty() { self.contracts.nft = addr; }
        }
        if let Ok(addr) = std::env::var("DEADMKT_POOL_CONFIG_ADDR") {
            if !addr.is_empty() { self.contracts.pool_config = addr; }
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        // withdrawal_rules
        if self.withdrawal_rules.holding_period_days < 1
            || self.withdrawal_rules.holding_period_days > 367
        {
            return Err(ConfigError::ValidationError(
                "holding_period_days must be 1-367".into(),
            ));
        }

        // profit_taking
        let valid_modes = ["all_excess", "fixed_amount", "percentage"];
        if !valid_modes.contains(&self.profit_taking.transfer_mode.as_str()) {
            return Err(ConfigError::ValidationError(format!(
                "invalid transfer_mode: {}",
                self.profit_taking.transfer_mode
            )));
        }

        if self.profit_taking.transfer_mode == "fixed_amount"
            && self.profit_taking.transfer_mode_value.is_none()
        {
            return Err(ConfigError::ValidationError(
                "fixed_amount mode requires transfer_mode_value".into(),
            ));
        }

        // base_capital must have at least one token (unless threshold is 0 = disabled)
        if self.profit_taking.threshold_pct > 0
            && self.profit_taking.base_capital.is_empty()
        {
            return Err(ConfigError::ValidationError(
                "base_capital must have at least one token when profit taking is enabled".into(),
            ));
        }

        Ok(())
    }
}

/// Check if this is first boot (no config + no keystore).
pub fn is_first_boot(config_path: &Path, keystore_path: &Path) -> bool {
    !config_path.exists() || !keystore_path.exists()
}

fn chrono_now() -> String {
    // Simple UTC timestamp without chrono dependency
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{secs}")
}

/// Generate a 256-bit random strategy auth token (hex-encoded).
pub fn generate_strategy_auth_token() -> String {
    let mut rng = rand::thread_rng();
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rng, &mut bytes);
    hex::encode(bytes)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_default_config() -> NodeConfig {
        NodeConfig {
            network: Network::Testnet,
            rpc_urls: vec!["https://rpc-testnet.supra.com".into()],
            nft_id: 42,
            trustee_address: "0xTRUSTEE".into(),
            beneficiary_address: "0xBENEF".into(),
            sponsor_address: "0xSPONSOR".into(),
            contracts: ContractAddresses {
                settlement: "0xS".into(),
                escrow: "0xE".into(),
                nft: "0xN".into(),
                pool_config: "0xP".into(),
            },
            bootstrap_peers: vec!["peer1.deadmkt.com:9191".into()],
            strategy_auth_token: "a".repeat(64),
            markets: vec!["EMM/KAY".into(), "KAY/TEE".into(), "TEE/EMM".into()],
            token_decimals: [("EMM".into(), 5), ("KAY".into(), 5), ("TEE".into(), 5)].into(),
            profit_taking: ProfitConfig {
                base_capital: [
                    ("EMM".into(), 33_000_000u64),  // 330 tokens (5 decimals)
                    ("KAY".into(), 33_000_000u64),
                    ("TEE".into(), 33_000_000u64),
                ].into(),
                threshold_pct: 20,
                transfer_mode: "all_excess".into(),
                transfer_mode_value: None,
            },
            withdrawal_rules: WithdrawalConfig {
                holding_period_days: 90,
                rushed_withdrawal_enabled: true,
            },
            strategy_port: 9090,
            gossip_port: 9191,
            chain_id: 6,
            max_gas_amount: 200,
            gas_unit_price: 100000,
            created_at: "2026-02-09T12:00:00Z".into(),
        }
    }

    // T_CFG_01
    #[test]
    fn test_create_config() {
        let config = NodeConfig::new(
            Network::Testnet,
            42,
            "0xTRUSTEE".into(),
            "0xBENEFICIARY".into(),
            "0xSPONSOR".into(),
            vec!["EMM/KAY".into(), "KAY/TEE".into(), "TEE/EMM".into()],
        );

        assert_eq!(config.network, Network::Testnet);
        assert_eq!(config.nft_id, 42);
        assert_eq!(config.rpc_urls.len(), 2);
        assert_eq!(config.rpc_urls[0], "https://rpc-testnet.supra.com");
        assert_eq!(config.bootstrap_peers.len(), 5);
        assert!(config.strategy_auth_token.len() >= 32);
        assert!(!config.created_at.is_empty());
    }

    // T_CFG_02
    #[test]
    fn test_load_config() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");

        std::fs::write(&path, r#"{
            "network": "testnet",
            "rpc_urls": ["https://rpc-testnet.supra.com"],
            "nft_id": 42,
            "trustee_address": "0xTRUSTEE",
            "beneficiary_address": "0xBENEF",
            "sponsor_address": "0xSPONSOR",
            "contracts": {
                "settlement": "0xS", "escrow": "0xE",
                "nft": "0xN", "pool_config": "0xP"
            },
            "bootstrap_peers": [],
            "strategy_auth_token": "abc123",
            "markets": ["EMM/KAY", "KAY/TEE", "TEE/EMM"],
            "token_decimals": {"EMM": 5, "KAY": 5, "TEE": 5},
            "profit_taking": {
                "base_capital": {"EMM": 33000000, "KAY": 33000000, "TEE": 33000000},
                "threshold_pct": 20,
                "transfer_mode": "all_excess",
                "transfer_mode_value": null
            },
            "withdrawal_rules": {
                "holding_period_days": 90,
                "rushed_withdrawal_enabled": true
            },
            "created_at": "2026-02-09T12:00:00Z"
        }"#).unwrap();

        let config = NodeConfig::load(&path).unwrap();
        assert_eq!(config.nft_id, 42);
        assert_eq!(config.network, Network::Testnet);
    }

    // T_CFG_03
    #[test]
    fn test_config_validation_missing_fields() {
        let json = r#"{"network": "testnet"}"#;
        let result: Result<NodeConfig, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // T_CFG_04
    #[test]
    fn test_config_validation_unknown_network() {
        let json = r#"{"network": "devnet", "nft_id": 42}"#;
        let result: Result<NodeConfig, _> = serde_json::from_str(json);
        assert!(result.is_err());

        let testnet: Result<Network, _> = serde_json::from_str(r#""testnet""#);
        let mainnet: Result<Network, _> = serde_json::from_str(r#""mainnet""#);
        assert!(testnet.is_ok());
        assert!(mainnet.is_ok());
    }

    // T_CFG_05
    #[test]
    fn test_detect_first_boot() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let keystore_path = dir.path().join("keystore.json");

        assert!(is_first_boot(&config_path, &keystore_path));

        std::fs::write(&config_path, "{}").unwrap();
        std::fs::write(&keystore_path, "{}").unwrap();
        assert!(!is_first_boot(&config_path, &keystore_path));
    }

    // T_CFG_06
    #[test]
    fn test_config_save_reload_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");

        let config = NodeConfig::new(
            Network::Testnet, 42,
            "0xTRUSTEE".into(), "0xBENEF".into(), "0xSPONSOR".into(),
            vec!["EMM/KAY".into(), "KAY/TEE".into(), "TEE/EMM".into()],
        );

        config.save(&path).unwrap();
        let reloaded = NodeConfig::load(&path).unwrap();

        assert_eq!(config.network, reloaded.network);
        assert_eq!(config.nft_id, reloaded.nft_id);
        assert_eq!(config.trustee_address, reloaded.trustee_address);
        assert_eq!(config.beneficiary_address, reloaded.beneficiary_address);
        assert_eq!(config.sponsor_address, reloaded.sponsor_address);
        assert_eq!(config.markets, reloaded.markets);
        assert_eq!(config.strategy_auth_token, reloaded.strategy_auth_token);
        assert_eq!(config.profit_taking.threshold_pct, reloaded.profit_taking.threshold_pct);
        assert_eq!(config.withdrawal_rules.holding_period_days,
                   reloaded.withdrawal_rules.holding_period_days);
    }

    // T_CFG_07
    #[test]
    fn test_profit_taking_validation() {
        let mut config = make_default_config();
        config.profit_taking.threshold_pct = 0;
        assert!(config.validate().is_ok());

        config.profit_taking.threshold_pct = 20;
        assert!(config.validate().is_ok());

        config.profit_taking.transfer_mode = "all_excess".into();
        assert!(config.validate().is_ok());

        config.profit_taking.transfer_mode = "fixed_amount".into();
        config.profit_taking.transfer_mode_value = Some(1000);
        assert!(config.validate().is_ok());

        config.profit_taking.transfer_mode = "invalid_mode".into();
        assert!(config.validate().is_err());

        config.profit_taking.transfer_mode = "fixed_amount".into();
        config.profit_taking.transfer_mode_value = None;
        assert!(config.validate().is_err());

        config.profit_taking.transfer_mode = "all_excess".into();
        config.profit_taking.transfer_mode_value = None;
        config.profit_taking.base_capital.clear();
        assert!(config.validate().is_err());
    }

    // T_CFG_08
    #[test]
    fn test_withdrawal_rules_validation() {
        let mut config = make_default_config();

        config.withdrawal_rules.holding_period_days = 90;
        assert!(config.validate().is_ok());

        config.withdrawal_rules.holding_period_days = 1;
        assert!(config.validate().is_ok());

        config.withdrawal_rules.holding_period_days = 367;
        assert!(config.validate().is_ok());

        config.withdrawal_rules.holding_period_days = 0;
        assert!(config.validate().is_err());

        config.withdrawal_rules.holding_period_days = 368;
        assert!(config.validate().is_err());

        config.withdrawal_rules.holding_period_days = 90;
        config.withdrawal_rules.rushed_withdrawal_enabled = false;
        assert!(config.validate().is_ok());
    }

    // Extra: strategy auth token generation
    #[test]
    fn test_strategy_auth_token_generation() {
        let token = generate_strategy_auth_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

        let token2 = generate_strategy_auth_token();
        assert_ne!(token, token2);
    }

    // T_CFG_09: DEADMKT_CONTRACT_ADDR blanket override
    #[test]
    fn test_env_override_blanket_contract_addr() {
        let mut config = make_default_config();
        assert_eq!(config.contracts.settlement, "0xS");

        std::env::set_var("DEADMKT_CONTRACT_ADDR", "0xNEW_DEPLOY");
        config.apply_env_overrides();

        assert_eq!(config.contracts.settlement, "0xNEW_DEPLOY");
        assert_eq!(config.contracts.escrow, "0xNEW_DEPLOY");
        assert_eq!(config.contracts.nft, "0xNEW_DEPLOY");
        assert_eq!(config.contracts.pool_config, "0xNEW_DEPLOY");

        std::env::remove_var("DEADMKT_CONTRACT_ADDR");
    }

    // T_CFG_10: Per-module override takes precedence over blanket
    #[test]
    fn test_env_override_per_module_precedence() {
        let mut config = make_default_config();

        std::env::set_var("DEADMKT_CONTRACT_ADDR", "0xBLANKET");
        std::env::set_var("DEADMKT_SETTLEMENT_ADDR", "0xSETTLE_OVERRIDE");
        config.apply_env_overrides();

        // Settlement gets per-module override; others get blanket
        assert_eq!(config.contracts.settlement, "0xSETTLE_OVERRIDE");
        assert_eq!(config.contracts.escrow, "0xBLANKET");
        assert_eq!(config.contracts.nft, "0xBLANKET");
        assert_eq!(config.contracts.pool_config, "0xBLANKET");

        std::env::remove_var("DEADMKT_CONTRACT_ADDR");
        std::env::remove_var("DEADMKT_SETTLEMENT_ADDR");
    }

    // T_CFG_11: Empty env var does not override
    #[test]
    fn test_env_override_empty_ignored() {
        let mut config = make_default_config();
        assert_eq!(config.contracts.settlement, "0xS");

        std::env::set_var("DEADMKT_CONTRACT_ADDR", "");
        config.apply_env_overrides();

        // Should remain unchanged
        assert_eq!(config.contracts.settlement, "0xS");
        assert_eq!(config.contracts.escrow, "0xE");

        std::env::remove_var("DEADMKT_CONTRACT_ADDR");
    }
}
