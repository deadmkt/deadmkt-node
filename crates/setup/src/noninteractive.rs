// =========================================================================
// crates/setup/src/noninteractive.rs
//
// MR1a: non-interactive setup config schema + validation + structured
// JSON output. Foundation that MR1b (LLM-assisted) and MR1c (restore)
// will layer onto.
//
// Scope (MR1a only):
//   - SetupConfig struct: serde-deserialized JSON schema
//   - Pure validation functions (beneficiary, password, mint amounts,
//     network)
//   - SetupResult struct: serde-serialized JSON output to stdout
//   - run_setup_noninteractive_fresh(): orchestrates fresh setup from
//     a SetupConfig (no existing keystore -> generate from password ->
//     run full chain setup -> write config.json -> return result).
//
// MR1b/c/d add: LLM-assisted prompt path, restore-from-backup flow,
// production gate + chmod 600 + password scrubbing + examples.
// =========================================================================

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::SetupError;

// =========================================================================
// Input schema: SetupConfig
// =========================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct SetupConfig {
    /// "testnet" or "mainnet". Defaults to "testnet" if omitted.
    #[serde(default = "default_network")]
    pub network: String,

    /// Required for fresh setup. Hex-prefixed account address.
    pub beneficiary_address: String,

    /// "trading" or "bootstrap". Defaults to "trading".
    #[serde(default = "default_node_role")]
    pub node_role: String,

    /// MR1a (fresh setup): required. Used to encrypt the new keystore.
    /// MR1b (LLM-assisted) leaves this absent and prompts the operator.
    /// Validated >=8 chars when present.
    pub keystore_password: Option<String>,

    /// Initial mint amounts (raw token units, 5 decimals).
    /// Defaults to 50000 each (= 0.50000 EMM/KAY/TEE).
    #[serde(default)]
    pub mint_amounts: MintAmounts,

    /// Holding period + rushed withdrawal config.
    #[serde(default)]
    pub withdrawal_rules: WithdrawalRules,

    /// Faucet behaviour (testnet only).
    #[serde(default)]
    pub faucet: FaucetCfg,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MintAmounts {
    #[serde(default = "default_mint_amount")]
    pub emm: u64,
    #[serde(default = "default_mint_amount")]
    pub kay: u64,
    #[serde(default = "default_mint_amount")]
    pub tee: u64,
}

impl Default for MintAmounts {
    fn default() -> Self {
        Self {
            emm: default_mint_amount(),
            kay: default_mint_amount(),
            tee: default_mint_amount(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WithdrawalRules {
    #[serde(default = "default_holding_days")]
    pub holding_period_days: u64,
    #[serde(default = "default_true")]
    pub rushed_withdrawal_enabled: bool,
}

impl Default for WithdrawalRules {
    fn default() -> Self {
        Self {
            holding_period_days: default_holding_days(),
            rushed_withdrawal_enabled: default_true(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FaucetCfg {
    #[serde(default = "default_true")]
    pub auto_fund: bool,
    #[serde(default = "default_faucet_min")]
    pub min_balance_supra: u64,
}

impl Default for FaucetCfg {
    fn default() -> Self {
        Self {
            auto_fund: default_true(),
            min_balance_supra: default_faucet_min(),
        }
    }
}

fn default_network() -> String { "testnet".into() }
fn default_node_role() -> String { "trading".into() }
fn default_mint_amount() -> u64 { 50000 }
fn default_holding_days() -> u64 { 90 }
fn default_true() -> bool { true }
fn default_faucet_min() -> u64 { 1000 }

// =========================================================================
// Output schema: SetupResult
// =========================================================================

/// Mode the noninteractive setup entered. Future MR1b/c add the other
/// variants; MR1a only produces Fresh.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupMode {
    Fresh,
    ConfigWithKeystore,
    Restore,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct EscrowBalances {
    pub emm: u64,
    pub kay: u64,
    pub tee: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetupResult {
    pub success: bool,
    pub mode: SetupMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nft_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trustee_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beneficiary_address: Option<String>,
    pub network: String,
    pub node_role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escrow_balances: Option<EscrowBalances>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_balance_supra: Option<u64>,
    pub steps_performed: Vec<String>,
    pub steps_skipped: Vec<String>,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
}

impl SetupResult {
    pub fn fresh_in_progress(network: &str, node_role: &str) -> Self {
        Self {
            success: false,
            mode: SetupMode::Fresh,
            nft_id: None,
            trustee_address: None,
            beneficiary_address: None,
            network: network.to_string(),
            node_role: node_role.to_string(),
            escrow_balances: None,
            gas_balance_supra: None,
            steps_performed: Vec::new(),
            steps_skipped: Vec::new(),
            warnings: Vec::new(),
            error: None,
            step: None,
        }
    }

    pub fn fail(mut self, step: &str, error: impl ToString) -> Self {
        self.success = false;
        self.step = Some(step.to_string());
        self.error = Some(error.to_string());
        self
    }

    /// Serialize to JSON string for stdout emission.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| {
            format!("{{\"success\":false,\"error\":\"failed to serialize SetupResult: {}\"}}", e)
        })
    }
}

// =========================================================================
// Pure validation
//
// These functions deliberately have NO IO. The interactive wizard's
// existing prompt_* functions can call into these once we refactor;
// for MR1a they validate the deserialized SetupConfig before any
// chain operation runs.
// =========================================================================

/// Validate a hex-prefixed address (beneficiary, trustee, etc).
/// Rules: must start with `0x`, hex chars only, length >= 10.
pub fn validate_address(addr: &str) -> Result<String, SetupError> {
    let lower = addr.trim().to_lowercase();
    if !lower.starts_with("0x") {
        return Err(SetupError::IoError(
            "address must start with 0x".to_string(),
        ));
    }
    if lower.len() < 10 {
        return Err(SetupError::IoError(
            "address too short (need at least 8 hex chars after 0x)".to_string(),
        ));
    }
    if !lower[2..].chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(SetupError::IoError(
            "address contains non-hex characters after 0x".to_string(),
        ));
    }
    Ok(lower)
}

/// Validate keystore password. Spec: min 8 chars.
pub fn validate_password(password: &str) -> Result<(), SetupError> {
    if password.len() < 8 {
        return Err(SetupError::IoError(
            "keystore_password must be at least 8 characters".to_string(),
        ));
    }
    Ok(())
}

/// Validate the network string -- must be "testnet" or "mainnet".
pub fn validate_network(network: &str) -> Result<deadmkt_config::Network, SetupError> {
    match network.to_lowercase().as_str() {
        "testnet" => Ok(deadmkt_config::Network::Testnet),
        "mainnet" => Ok(deadmkt_config::Network::Mainnet),
        other => Err(SetupError::IoError(format!(
            "invalid network '{}': expected 'testnet' or 'mainnet'",
            other
        ))),
    }
}

/// Validate node role -- must be "trading" or "bootstrap".
/// Returns true if bootstrap, false if trading (matches existing
/// `prompt_node_role` return semantics).
pub fn validate_node_role(role: &str) -> Result<bool, SetupError> {
    match role.to_lowercase().as_str() {
        "trading" => Ok(false),
        "bootstrap" => Ok(true),
        other => Err(SetupError::IoError(format!(
            "invalid node_role '{}': expected 'trading' or 'bootstrap'",
            other
        ))),
    }
}

/// Validate mint amounts. All three must be positive.
pub fn validate_mint_amounts(amounts: &MintAmounts) -> Result<(), SetupError> {
    if amounts.emm == 0 || amounts.kay == 0 || amounts.tee == 0 {
        return Err(SetupError::IoError(
            "mint_amounts: all three (emm, kay, tee) must be > 0".to_string(),
        ));
    }
    Ok(())
}

/// Validate withdrawal rules. holding_period_days must be 1..=367 per
/// the contract's escrow::register_trader bounds.
pub fn validate_withdrawal_rules(rules: &WithdrawalRules) -> Result<(), SetupError> {
    if rules.holding_period_days == 0 || rules.holding_period_days > 367 {
        return Err(SetupError::IoError(format!(
            "withdrawal_rules.holding_period_days must be in 1..=367, got {}",
            rules.holding_period_days
        )));
    }
    Ok(())
}

/// Validate the full SetupConfig before running any chain operations.
/// Returns the validated normalised values, ready to use.
#[derive(Debug)]
pub struct ValidatedSetupConfig {
    pub network: deadmkt_config::Network,
    pub beneficiary_address: String, // normalised (lowercase, trimmed)
    pub bootstrap_only: bool,        // node_role == "bootstrap"
    pub keystore_password: String,   // MR1a requires this
    pub mint_amounts: MintAmounts,
    pub withdrawal_rules: WithdrawalRules,
    pub faucet: FaucetCfg,
}

pub fn validate_for_fresh_setup(cfg: &SetupConfig) -> Result<ValidatedSetupConfig, SetupError> {
    let network = validate_network(&cfg.network)?;
    let beneficiary_address = validate_address(&cfg.beneficiary_address)?;
    let bootstrap_only = validate_node_role(&cfg.node_role)?;
    let keystore_password = cfg.keystore_password.clone().ok_or_else(|| {
        SetupError::IoError(
            "keystore_password is required for fresh setup (MR1a). For LLM-assisted (existing keystore) or restore flows, see MR1b/MR1c."
                .to_string(),
        )
    })?;
    validate_password(&keystore_password)?;
    validate_mint_amounts(&cfg.mint_amounts)?;
    validate_withdrawal_rules(&cfg.withdrawal_rules)?;
    Ok(ValidatedSetupConfig {
        network,
        beneficiary_address,
        bootstrap_only,
        keystore_password,
        mint_amounts: cfg.mint_amounts.clone(),
        withdrawal_rules: cfg.withdrawal_rules.clone(),
        faucet: cfg.faucet.clone(),
    })
}

// =========================================================================
// Config loading
// =========================================================================

/// Read a SetupConfig from a JSON file on disk.
pub fn load_setup_config(path: &Path) -> Result<SetupConfig, SetupError> {
    let body = std::fs::read_to_string(path)
        .map_err(|e| SetupError::IoError(format!("read {}: {}", path.display(), e)))?;
    let cfg: SetupConfig = serde_json::from_str(&body)
        .map_err(|e| SetupError::IoError(format!("parse {}: {}", path.display(), e)))?;
    Ok(cfg)
}

// =========================================================================
// Keystore generation (non-interactive)
// =========================================================================

/// Non-interactive keystore generation. Mirrors `wizard_generate_keypair`
/// but with no IO: takes the password directly, writes the encrypted
/// keystore, returns the trustee address + keys.
///
/// FAILS if the keystore file already exists. MR1a is the fresh-setup
/// path; existing keystore means caller wanted MR1b/c flow instead.
pub fn generate_keystore_noninteractive(
    keystore_path: &std::path::Path,
    password: &str,
) -> Result<(String, ed25519_dalek::VerifyingKey, ed25519_dalek::SigningKey), SetupError> {
    use sha3::{Digest, Sha3_256};

    if keystore_path.exists() {
        return Err(SetupError::IoError(format!(
            "keystore already exists at {} -- MR1a is fresh-setup only; for LLM-assisted (existing keystore) flow see MR1b, for restore see MR1c",
            keystore_path.display()
        )));
    }

    let (secret, public) = deadmkt_crypto::generate_keypair();

    // Address = Sha3_256(public || 0x00). Same scheme used by
    // wizard_generate_keypair.
    let mut hasher = Sha3_256::new();
    hasher.update(public.as_bytes());
    hasher.update(&[0x00]);
    let hash = hasher.finalize();
    let address = format!("0x{}", hex::encode(hash));

    deadmkt_keystore::save_keystore(keystore_path, &secret, &public, password)?;

    Ok((address, public, secret))
}

// =========================================================================
// SilentIO: WizardIO impl for non-interactive setup
//
// The existing wizard steps (mint_nft_pair, wait_for_funding, etc) take a
// &mut dyn WizardIO for prompts. Non-interactive setup reuses those steps
// to avoid duplicating chain logic; SilentIO forwards print() to stderr
// (so the operator sees progress) and answers read() with canned values.
//
// The only `read_line` we actually hit in the MR1a happy path is the y/n
// bond confirmation in mint_nft_pair, which we always answer "y" -- the
// operator opted in by providing the config.
// =========================================================================

pub struct SilentIO;

impl crate::WizardIO for SilentIO {
    fn print(&mut self, msg: &str) {
        // Forward to stderr so it doesn't contaminate the stdout JSON output.
        eprint!("{}", msg);
    }
    fn read_line(&mut self) -> Result<String, SetupError> {
        // Always accept. The only call site reached in the MR1a fresh-setup
        // happy path is mint_nft_pair's "Proceed? (y/n)" bond confirmation.
        Ok("y".to_string())
    }
    fn is_aborted(&self) -> bool { false }
}

// =========================================================================
// Orchestrator: run_setup_noninteractive_fresh
//
// MR1a happy path: no keystore on disk + setup.json with keystore_password.
// Sequentially: validate -> keystore -> funding -> mint NFT -> register+
// deposit -> testnet auto-mint -> write config.json. Each step is recorded
// in steps_performed / steps_skipped so the structured JSON output tells
// the caller exactly what happened.
//
// On any error, returns a SetupResult with success=false, step=<failed
// step name>, error=<message>. Caller serialises to stdout and exits 1.
// =========================================================================

/// Fresh-setup entry point: no keystore, has setup.json with password.
///
/// Caller is responsible for:
///   - emitting `result.to_json()` to stdout
///   - exiting with status code 1 if `result.success == false`
///
/// This function intentionally does not touch stdout, so the caller can
/// position the JSON output cleanly.
pub async fn run_setup_noninteractive_fresh(
    cfg: &SetupConfig,
    chain: &dyn crate::ChainClient,
    data_dir: &std::path::Path,
) -> SetupResult {
    // 1. Validate ------------------------------------------------------------
    let v = match validate_for_fresh_setup(cfg) {
        Ok(v) => v,
        Err(e) => {
            return SetupResult::fresh_in_progress(&cfg.network, &cfg.node_role)
                .fail("validate", e);
        }
    };

    let mut result = SetupResult::fresh_in_progress(
        match v.network { deadmkt_config::Network::Testnet => "testnet", deadmkt_config::Network::Mainnet => "mainnet" },
        if v.bootstrap_only { "bootstrap" } else { "trading" },
    );
    result.beneficiary_address = Some(v.beneficiary_address.clone());
    result.steps_performed.push("validate".into());

    // 2. Generate keystore --------------------------------------------------
    let keystore_path = data_dir.join("keystore.json");
    let config_path = data_dir.join("config.json");
    if config_path.exists() {
        result.warnings.push(format!(
            "config.json already exists at {} -- it will be overwritten",
            config_path.display()
        ));
    }
    let (address, public, secret) = match generate_keystore_noninteractive(
        &keystore_path,
        &v.keystore_password,
    ) {
        Ok(t) => t,
        Err(e) => return result.fail("generate_keystore", e),
    };
    result.trustee_address = Some(address.clone());
    result.steps_performed.push("generate_keystore".into());
    eprintln!("[setup] Keystore generated. Trustee address: {}", address);

    // 3. Set chain signer ---------------------------------------------------
    chain.set_signer(secret.as_bytes(), public.as_bytes(), &address);

    // 4. Wait for funding (uses SilentIO so chain polling does not prompt) -
    let mut io = SilentIO;
    eprintln!("[setup] Waiting for SUPRA funding to {}...", address);
    let funding = match crate::wait_for_funding(
        &mut io, chain, &address, &v.network, std::time::Duration::from_secs(5),
    ).await {
        Ok(b) => b,
        Err(e) => return result.fail("wait_for_funding", e),
    };
    result.gas_balance_supra = Some(funding.supra);
    result.steps_performed.push("wait_for_funding".into());
    eprintln!("[setup] Funded: {} SUPRA (raw).", funding.supra);

    // 5. NFT: reuse existing or mint -----------------------------------------
    let nft_id = match crate::check_existing_nft(chain, &address).await {
        Ok(crate::ExistingNft::Found { nft_id, .. }) => {
            result.steps_skipped.push("mint_nft".into());
            eprintln!("[setup] NFT #{} already minted for this address (idempotent skip).", nft_id);
            nft_id
        }
        Ok(crate::ExistingNft::NotFound) => {
            match crate::mint_nft_pair(
                &mut io, chain, public.as_bytes(), &v.beneficiary_address, &address,
            ).await {
                Ok(id) => {
                    result.steps_performed.push("mint_nft".into());
                    eprintln!("[setup] NFT #{} minted.", id);
                    id
                }
                Err(e) => return result.fail("mint_nft", e),
            }
        }
        Err(e) => return result.fail("check_existing_nft", e),
    };
    result.nft_id = Some(nft_id);

    // 6. Register trader + deposit (idempotent on register) -----------------
    if v.bootstrap_only {
        result.steps_skipped.push("register_and_deposit".into());
        eprintln!("[setup] Bootstrap mode: skipping register_and_deposit and token mint.");
    } else {
        let wcfg = deadmkt_config::WithdrawalConfig {
            holding_period_days: v.withdrawal_rules.holding_period_days as u32,
            rushed_withdrawal_enabled: v.withdrawal_rules.rushed_withdrawal_enabled,
        };
        if let Err(e) = crate::register_and_deposit(chain, nft_id, &wcfg, &funding).await {
            return result.fail("register_and_deposit", e);
        }
        result.steps_performed.push("register_and_deposit".into());
        eprintln!("[setup] Registered trader + deposited any pre-existing token balances.");

        // 7. Testnet auto-mint tokens -----------------------------------------
        if v.network == deadmkt_config::Network::Testnet {
            match crate::auto_mint_and_deposit(
                &mut io, chain, funding.supra, nft_id, &address,
            ).await {
                Ok(_tokens) => {
                    result.steps_performed.push("auto_mint_and_deposit".into());
                    eprintln!("[setup] Testnet: auto-minted EMM/KAY/TEE into escrow.");
                }
                Err(e) => return result.fail("auto_mint_and_deposit", e),
            }
        } else {
            // Mainnet: tokens must be funded externally and deposited before
            // trading. MR1a doesn't handle mainnet token funding flow yet --
            // operator can use the interactive wizard or extend setup.json.
            result.warnings.push(
                "Mainnet: token funding/deposit must be done manually (MR1a fresh setup does not auto-mint on mainnet).".to_string(),
            );
        }
    }

    // 8. Build + write config.json ------------------------------------------
    // Mirror the construction pattern used in `run_wizard` (see lib.rs:988+).
    let contracts = deadmkt_config::default_contract_addresses(&v.network);
    let rpc_urls = match v.network {
        deadmkt_config::Network::Testnet => vec!["https://rpc-testnet.supra.com".to_string()],
        deadmkt_config::Network::Mainnet => vec!["https://rpc-mainnet.supra.com".to_string()],
    };
    let bootstrap_peers = match v.network {
        deadmkt_config::Network::Testnet => vec![
            "peer1.testnet.deadmkt.com:9191".to_string(),
            "peer2.testnet.deadmkt.com:9191".to_string(),
            "peer3.testnet.deadmkt.com:9191".to_string(),
            "peer4.testnet.deadmkt.com:9191".to_string(),
            "peer5.testnet.deadmkt.com:9191".to_string(),
        ],
        deadmkt_config::Network::Mainnet => vec![
            "peer1.deadmkt.com:9191".to_string(),
            "peer2.deadmkt.com:9191".to_string(),
            "peer3.deadmkt.com:9191".to_string(),
            "peer4.deadmkt.com:9191".to_string(),
            "peer5.deadmkt.com:9191".to_string(),
        ],
    };
    let auth_token = deadmkt_config::generate_strategy_auth_token();
    let node_config = deadmkt_config::NodeConfig {
        network: v.network.clone(),
        rpc_urls,
        nft_id,
        trustee_address: address.clone(),
        beneficiary_address: v.beneficiary_address.clone(),
        sponsor_address: String::new(),
        contracts,
        bootstrap_peers,
        strategy_auth_token: auth_token,
        markets: vec!["EMM/KAY".into(), "KAY/TEE".into(), "TEE/EMM".into()],
        token_decimals: std::collections::HashMap::new(),
        profit_taking: deadmkt_config::ProfitConfig {
            base_capital: std::collections::HashMap::new(),
            threshold_pct: 0,
            transfer_mode: "all_excess".into(),
            transfer_mode_value: None,
        },
        withdrawal_rules: deadmkt_config::WithdrawalConfig {
            holding_period_days: v.withdrawal_rules.holding_period_days as u32,
            rushed_withdrawal_enabled: v.withdrawal_rules.rushed_withdrawal_enabled,
        },
        strategy_port: 9090,
        gossip_port: 9191,
        chain_id: 6,
        max_gas_amount: 5000,
        gas_unit_price: 100000,
        created_at: format!("{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()),
    };

    if let Err(e) = node_config.save(&config_path) {
        return result.fail("write_config", e);
    }
    result.steps_performed.push("write_config".into());
    eprintln!("[setup] Wrote config.json -> {}", config_path.display());

    // 9. Final escrow snapshot for the JSON output --------------------------
    if !v.bootstrap_only {
        match chain.get_all_balances(&address).await {
            Ok(b) => {
                // wallet balances (post auto-mint they should be 0 -- everything
                // went to escrow). Skip; just record gas.
                result.gas_balance_supra = Some(b.supra);
            }
            Err(e) => {
                result.warnings.push(format!("final balance fetch failed: {}", e));
            }
        }
    }

    result.success = true;
    result
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- SetupConfig deserialization ------------------------------------

    #[test]
    fn t_mr1a_01_setup_config_minimal_parses() {
        // Only required fields supplied. Everything else takes defaults.
        let json = r#"{
            "beneficiary_address": "0x1234567890abcdef",
            "keystore_password": "verysecret"
        }"#;
        let cfg: SetupConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.network, "testnet");
        assert_eq!(cfg.node_role, "trading");
        assert_eq!(cfg.beneficiary_address, "0x1234567890abcdef");
        assert_eq!(cfg.keystore_password.as_deref(), Some("verysecret"));
        assert_eq!(cfg.mint_amounts.emm, 50000);
        assert_eq!(cfg.mint_amounts.kay, 50000);
        assert_eq!(cfg.mint_amounts.tee, 50000);
        assert_eq!(cfg.withdrawal_rules.holding_period_days, 90);
        assert!(cfg.withdrawal_rules.rushed_withdrawal_enabled);
        assert!(cfg.faucet.auto_fund);
        assert_eq!(cfg.faucet.min_balance_supra, 1000);
    }

    #[test]
    fn t_mr1a_02_setup_config_full_parses() {
        let json = r#"{
            "network": "mainnet",
            "beneficiary_address": "0xabc1234567",
            "node_role": "bootstrap",
            "keystore_password": "abcdefgh",
            "mint_amounts": { "emm": 1, "kay": 2, "tee": 3 },
            "withdrawal_rules": { "holding_period_days": 365, "rushed_withdrawal_enabled": false },
            "faucet": { "auto_fund": false, "min_balance_supra": 500 }
        }"#;
        let cfg: SetupConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.network, "mainnet");
        assert_eq!(cfg.node_role, "bootstrap");
        assert_eq!(cfg.mint_amounts.emm, 1);
        assert_eq!(cfg.mint_amounts.kay, 2);
        assert_eq!(cfg.mint_amounts.tee, 3);
        assert_eq!(cfg.withdrawal_rules.holding_period_days, 365);
        assert!(!cfg.withdrawal_rules.rushed_withdrawal_enabled);
        assert!(!cfg.faucet.auto_fund);
        assert_eq!(cfg.faucet.min_balance_supra, 500);
    }

    #[test]
    fn t_mr1a_03_setup_config_missing_beneficiary_fails() {
        let json = r#"{ "keystore_password": "verysecret" }"#;
        let r: Result<SetupConfig, _> = serde_json::from_str(json);
        assert!(r.is_err());
    }

    // ---- Validation -----------------------------------------------------

    #[test]
    fn t_mr1a_10_validate_address_ok() {
        // Minimum length is 10 chars (0x + 8 hex); accept all-zeros.
        assert!(validate_address("0x12345678").is_ok());
        assert!(validate_address("0xdeadbeef").is_ok());
        assert!(validate_address("0xdeadbeef00").is_ok());
        // Whitespace tolerated; case normalised.
        assert_eq!(validate_address("  0xABCDef01  ").unwrap(), "0xabcdef01");
    }

    #[test]
    fn t_mr1a_11_validate_address_normalises_case() {
        let out = validate_address("0xABCDEF0123").unwrap();
        assert_eq!(out, "0xabcdef0123");
    }

    #[test]
    fn t_mr1a_12_validate_address_rejects_invalid() {
        assert!(validate_address("0xnothex").is_err());
        assert!(validate_address("no0xprefix").is_err());
        assert!(validate_address("0x").is_err());
        assert!(validate_address("0x12345").is_err()); // too short
    }

    #[test]
    fn t_mr1a_20_validate_password_ok() {
        assert!(validate_password("12345678").is_ok());
        assert!(validate_password("aVeryLongPassword").is_ok());
    }

    #[test]
    fn t_mr1a_21_validate_password_rejects_short() {
        assert!(validate_password("").is_err());
        assert!(validate_password("1234567").is_err());
    }

    #[test]
    fn t_mr1a_30_validate_network() {
        assert!(validate_network("testnet").is_ok());
        assert!(validate_network("Testnet").is_ok());
        assert!(validate_network("MAINNET").is_ok());
        assert!(validate_network("foonet").is_err());
        assert!(validate_network("").is_err());
    }

    #[test]
    fn t_mr1a_40_validate_node_role() {
        assert_eq!(validate_node_role("trading").unwrap(), false);
        assert_eq!(validate_node_role("bootstrap").unwrap(), true);
        assert_eq!(validate_node_role("Trading").unwrap(), false);
        assert!(validate_node_role("agent").is_err());
    }

    #[test]
    fn t_mr1a_50_validate_mint_amounts() {
        assert!(validate_mint_amounts(&MintAmounts { emm: 1, kay: 1, tee: 1 }).is_ok());
        assert!(validate_mint_amounts(&MintAmounts { emm: 0, kay: 1, tee: 1 }).is_err());
        assert!(validate_mint_amounts(&MintAmounts { emm: 1, kay: 0, tee: 1 }).is_err());
        assert!(validate_mint_amounts(&MintAmounts { emm: 1, kay: 1, tee: 0 }).is_err());
    }

    #[test]
    fn t_mr1a_60_validate_withdrawal_rules_bounds() {
        assert!(validate_withdrawal_rules(&WithdrawalRules { holding_period_days: 1, rushed_withdrawal_enabled: true }).is_ok());
        assert!(validate_withdrawal_rules(&WithdrawalRules { holding_period_days: 90, rushed_withdrawal_enabled: false }).is_ok());
        assert!(validate_withdrawal_rules(&WithdrawalRules { holding_period_days: 367, rushed_withdrawal_enabled: true }).is_ok());
        assert!(validate_withdrawal_rules(&WithdrawalRules { holding_period_days: 0, rushed_withdrawal_enabled: true }).is_err());
        assert!(validate_withdrawal_rules(&WithdrawalRules { holding_period_days: 368, rushed_withdrawal_enabled: true }).is_err());
    }

    #[test]
    fn t_mr1a_70_validate_for_fresh_setup_happy_path() {
        let cfg = SetupConfig {
            network: "testnet".into(),
            beneficiary_address: "0xABCDef0123".into(),
            node_role: "trading".into(),
            keystore_password: Some("strongpass".into()),
            mint_amounts: MintAmounts::default(),
            withdrawal_rules: WithdrawalRules::default(),
            faucet: FaucetCfg::default(),
        };
        let v = validate_for_fresh_setup(&cfg).unwrap();
        assert_eq!(v.network, deadmkt_config::Network::Testnet);
        assert_eq!(v.beneficiary_address, "0xabcdef0123"); // lowercased
        assert!(!v.bootstrap_only);
        assert_eq!(v.keystore_password, "strongpass");
    }

    #[test]
    fn t_mr1a_71_validate_for_fresh_setup_requires_password() {
        let cfg = SetupConfig {
            network: "testnet".into(),
            beneficiary_address: "0xABCDef0123".into(),
            node_role: "trading".into(),
            keystore_password: None, // missing -> MR1a should reject
            mint_amounts: MintAmounts::default(),
            withdrawal_rules: WithdrawalRules::default(),
            faucet: FaucetCfg::default(),
        };
        let err = validate_for_fresh_setup(&cfg).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("keystore_password is required"), "unexpected error: {}", msg);
    }

    // ---- SetupResult serialization --------------------------------------

    #[test]
    fn t_mr1a_80_setup_result_serializes_minimal() {
        let r = SetupResult::fresh_in_progress("testnet", "trading");
        let json = r.to_json();
        assert!(json.contains("\"mode\": \"fresh\""), "got: {}", json);
        assert!(json.contains("\"network\": \"testnet\""));
        assert!(json.contains("\"node_role\": \"trading\""));
        // Optional Nones must be omitted
        assert!(!json.contains("nft_id"));
        assert!(!json.contains("trustee_address"));
    }

    #[test]
    fn t_mr1a_81_setup_result_fail_carries_step_and_error() {
        let r = SetupResult::fresh_in_progress("testnet", "trading")
            .fail("unlock_keystore", "bad password");
        let json = r.to_json();
        assert!(json.contains("\"success\": false"));
        assert!(json.contains("\"step\": \"unlock_keystore\""));
        assert!(json.contains("\"error\": \"bad password\""));
    }

    // ---- Config loading -------------------------------------------------

    #[test]
    fn t_mr1a_90_load_setup_config_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("setup.json");
        std::fs::write(
            &path,
            r#"{ "beneficiary_address": "0x1234567890", "keystore_password": "abcdefgh" }"#,
        ).unwrap();
        let cfg = load_setup_config(&path).unwrap();
        assert_eq!(cfg.beneficiary_address, "0x1234567890");
    }

    #[test]
    fn t_mr1a_91_load_setup_config_missing_file_fails() {
        let r = load_setup_config(Path::new("/nonexistent/setup-mr1a-test.json"));
        assert!(r.is_err());
    }

    // ---- Keystore generation -------------------------------------------

    #[test]
    fn t_mr1a_95_generate_keystore_writes_file_and_returns_address() {
        let tmp = tempfile::tempdir().unwrap();
        let keystore_path = tmp.path().join("keystore.json");
        let (addr, _pub, _sec) = generate_keystore_noninteractive(
            &keystore_path,
            "strongpassword",
        ).unwrap();
        assert!(keystore_path.exists());
        assert!(addr.starts_with("0x"));
        // Address is 0x + 64 hex chars (SHA3-256)
        assert_eq!(addr.len(), 66);
    }

    #[test]
    fn t_mr1a_96_generate_keystore_refuses_to_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let keystore_path = tmp.path().join("keystore.json");
        generate_keystore_noninteractive(&keystore_path, "first-password").unwrap();
        // Second attempt must fail
        let err = generate_keystore_noninteractive(&keystore_path, "second-password").unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("already exists"), "got: {}", msg);
        assert!(msg.contains("MR1b") || msg.contains("MR1c"));
    }

    #[test]
    fn t_mr1a_92_load_setup_config_malformed_json_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("setup.json");
        std::fs::write(&path, "not json").unwrap();
        let r = load_setup_config(&path);
        assert!(r.is_err());
    }
}
