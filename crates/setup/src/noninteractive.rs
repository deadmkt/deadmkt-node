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

    /// MR1b: tag a result that was created via `fresh_in_progress` with
    /// the correct mode after the fact (e.g. when a validation failure
    /// happens before we know which entry point we're in).
    pub fn with_mode(mut self, mode: SetupMode) -> Self {
        self.mode = mode;
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

/// Validated common fields shared by MR1a (fresh) and MR1b (LLM-assisted)
/// post-keystore. Keystore password is intentionally NOT here -- fresh
/// pulls it from config, LLM-assisted prompts interactively at the CLI
/// boundary; the chain orchestration doesn't need either after the
/// keystore is unlocked.
#[derive(Debug, Clone)]
pub struct ValidatedSetupConfig {
    pub network: deadmkt_config::Network,
    pub beneficiary_address: String, // normalised (lowercase, trimmed)
    pub bootstrap_only: bool,        // node_role == "bootstrap"
    pub mint_amounts: MintAmounts,
    pub withdrawal_rules: WithdrawalRules,
    pub faucet: FaucetCfg,
}

/// Common-field validation. Pure -- no IO.
fn validate_common(cfg: &SetupConfig) -> Result<ValidatedSetupConfig, SetupError> {
    let network = validate_network(&cfg.network)?;
    let beneficiary_address = validate_address(&cfg.beneficiary_address)?;
    let bootstrap_only = validate_node_role(&cfg.node_role)?;
    validate_mint_amounts(&cfg.mint_amounts)?;
    validate_withdrawal_rules(&cfg.withdrawal_rules)?;
    Ok(ValidatedSetupConfig {
        network,
        beneficiary_address,
        bootstrap_only,
        mint_amounts: cfg.mint_amounts.clone(),
        withdrawal_rules: cfg.withdrawal_rules.clone(),
        faucet: cfg.faucet.clone(),
    })
}

/// MR1a (fresh) validation. Returns (common fields, keystore_password).
/// The password is required and must be >= 8 chars.
pub fn validate_for_fresh_setup(
    cfg: &SetupConfig,
) -> Result<(ValidatedSetupConfig, String), SetupError> {
    let v = validate_common(cfg)?;
    let keystore_password = cfg.keystore_password.clone().ok_or_else(|| {
        SetupError::IoError(
            "keystore_password is required for fresh setup (MR1a). For LLM-assisted (existing keystore) the password comes from an interactive prompt -- omit this field and the node will ask."
                .to_string(),
        )
    })?;
    validate_password(&keystore_password)?;
    Ok((v, keystore_password))
}

/// MR1b (LLM-assisted) validation. Keystore is already on disk; the
/// password is collected interactively at the CLI boundary, not from
/// the config (so the LLM that writes setup.json never sees it).
/// **Rejects** configs that DO include keystore_password -- the
/// security boundary is the whole point of MR1b. Use MR1a fresh setup
/// instead in that case.
pub fn validate_for_llm_assisted(
    cfg: &SetupConfig,
) -> Result<ValidatedSetupConfig, SetupError> {
    if cfg.keystore_password.is_some() {
        return Err(SetupError::IoError(
            "MR1b (LLM-assisted) rejects setup.json that contains keystore_password. The LLM-safe path requires the operator to type the password interactively. Remove the field from the config (or use MR1a fresh setup if you intentionally want password-on-disk)."
                .to_string(),
        ));
    }
    validate_common(cfg)
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
    if keystore_path.exists() {
        return Err(SetupError::IoError(format!(
            "keystore already exists at {} -- MR1a is fresh-setup only; for LLM-assisted (existing keystore) flow see MR1b, for restore see MR1c",
            keystore_path.display()
        )));
    }

    let (secret, public) = deadmkt_crypto::generate_keypair();
    let address = derive_trustee_address(&public);
    deadmkt_keystore::save_keystore(keystore_path, &secret, &public, password)?;

    Ok((address, public, secret))
}

/// MR1b: unlock an existing keystore file with the supplied password.
/// Returns the trustee address + keys derived from the stored keypair.
///
/// FAILS cleanly with `KeystoreError` if the file is missing or the
/// password is wrong -- the caller should report `step: "unlock_keystore"`
/// to the operator.
pub fn unlock_existing_keystore(
    keystore_path: &std::path::Path,
    password: &str,
) -> Result<(String, ed25519_dalek::VerifyingKey, ed25519_dalek::SigningKey), SetupError> {
    if !keystore_path.exists() {
        return Err(SetupError::IoError(format!(
            "no keystore at {} -- MR1b expects an existing keystore (use MR1a fresh setup if you have no keystore yet)",
            keystore_path.display()
        )));
    }
    let (secret, public) = deadmkt_keystore::load_keystore(keystore_path, password)?;
    let address = derive_trustee_address(&public);
    Ok((address, public, secret))
}

/// Derive the trustee on-chain address from an ed25519 public key.
/// Matches the scheme used by wizard_generate_keypair and
/// generate_keystore_noninteractive: Sha3_256(public || 0x00).
fn derive_trustee_address(public: &ed25519_dalek::VerifyingKey) -> String {
    use sha3::{Digest, Sha3_256};
    let mut hasher = Sha3_256::new();
    hasher.update(public.as_bytes());
    hasher.update(&[0x00]);
    let hash = hasher.finalize();
    format!("0x{}", hex::encode(hash))
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
/// Generates the keystore from the supplied password, then runs the
/// shared post-keystore orchestration.
pub async fn run_setup_noninteractive_fresh(
    cfg: &SetupConfig,
    chain: &dyn crate::ChainClient,
    data_dir: &std::path::Path,
) -> SetupResult {
    // 1. Validate ------------------------------------------------------------
    let (v, password) = match validate_for_fresh_setup(cfg) {
        Ok(t) => t,
        Err(e) => {
            return SetupResult::fresh_in_progress(&cfg.network, &cfg.node_role)
                .fail("validate", e);
        }
    };

    let mut result = init_result(&v, SetupMode::Fresh);
    result.steps_performed.push("validate".into());

    // 2. Generate keystore --------------------------------------------------
    let keystore_path = data_dir.join("keystore.json");
    let (address, public, secret) = match generate_keystore_noninteractive(
        &keystore_path, &password,
    ) {
        Ok(t) => t,
        Err(e) => return result.fail("generate_keystore", e),
    };
    result.trustee_address = Some(address.clone());
    result.steps_performed.push("generate_keystore".into());
    eprintln!("[setup] Keystore generated. Trustee address: {}", address);

    // 3. Hand off to shared post-keystore work.
    run_setup_after_keystore(v, address, public, secret, chain, data_dir, result).await
}

/// MR1c: restore-from-backup entry point. No setup.json -- the operator
/// dropped their backed-up `keystore.json` into the data directory and
/// ran the node. We prompt for the keystore password, unlock the keys,
/// query the chain for the trustee's NFT + beneficiary + escrow state,
/// and reconstruct `config.json` from network defaults plus the chain's
/// answers. **The trustee's only backup responsibility is keystore.json
/// + password.** Everything else is recoverable from chain state.
///
/// Returns a SetupResult tagged `mode: restore`. The caller writes JSON
/// to stdout and exits; the operator runs `deadmkt-node` again to begin
/// trading with the newly-written config.
///
/// On-chain branches:
///   - NFT + registered + escrow > 0 → write config, success
///   - NFT + registered, escrow empty → write config + warning (operator
///     can request a mint via strategy / `deadmkt-node deposit`)
///   - NFT exists, NOT registered → write config + warning (operator
///     should run interactive setup / supply a setup.json)
///   - No NFT for this address → error: this keystore has never minted;
///     can't restore -- need beneficiary address (use MR1a fresh setup
///     instead)
///   - Heartbeat reaped → write config + warning suggesting
///     `deadmkt-node reactivate`. The node's own #13b liveness check
///     will also attempt auto-reactivate on startup.
pub async fn run_setup_noninteractive_restore(
    keystore_password: &str,
    network: deadmkt_config::Network,
    chain: &dyn crate::ChainClient,
    data_dir: &std::path::Path,
) -> SetupResult {
    let network_str = match network {
        deadmkt_config::Network::Testnet => "testnet",
        deadmkt_config::Network::Mainnet => "mainnet",
    };
    let mut result = SetupResult::fresh_in_progress(network_str, "trading")
        .with_mode(SetupMode::Restore);

    // 1. Unlock keystore -----------------------------------------------------
    let keystore_path = data_dir.join("keystore.json");
    let (address, public, secret) = match unlock_existing_keystore(
        &keystore_path, keystore_password,
    ) {
        Ok(t) => t,
        Err(e) => return result.fail("unlock_keystore", e),
    };
    result.trustee_address = Some(address.clone());
    result.steps_performed.push("unlock_keystore".into());
    eprintln!("[restore] Keystore unlocked. Trustee address: {}", address);

    // 2. Set chain signer (needed if we ever submit a tx -- restore mainly
    // reads, but having the signer set keeps the client consistent) -------
    chain.set_signer(secret.as_bytes(), public.as_bytes(), &address);

    // 3. Chain state verification -------------------------------------------
    let nft_id = match crate::check_existing_nft(chain, &address).await {
        Ok(crate::ExistingNft::Found { nft_id, beneficiary }) => {
            result.beneficiary_address = Some(beneficiary.clone());
            eprintln!(
                "[restore] On-chain: NFT #{} owned, beneficiary {}",
                nft_id, beneficiary
            );
            nft_id
        }
        Ok(crate::ExistingNft::NotFound) => {
            return result.fail(
                "check_existing_nft",
                "no NFT on-chain for this keystore -- a restore needs an already-minted NFT (beneficiary is read from chain). Use MR1a fresh setup with a setup.json to mint a new NFT against a chosen beneficiary."
            );
        }
        Err(e) => return result.fail("check_existing_nft", e),
    };
    result.nft_id = Some(nft_id);
    result.steps_performed.push("check_existing_nft".into());

    // 4. Escrow + registration snapshot -------------------------------------
    let token_metas = network_token_metadata_addresses(&network);
    let mut balances = EscrowBalances::default();
    let mut any_balance = false;
    for (sym, meta) in &token_metas {
        match chain.get_escrow_balance(nft_id, meta).await {
            Ok(bal) => {
                if bal > 0 { any_balance = true; }
                match sym.as_str() {
                    "EMM" => balances.emm = bal,
                    "KAY" => balances.kay = bal,
                    "TEE" => balances.tee = bal,
                    _ => {}
                }
            }
            Err(e) => {
                result.warnings.push(format!(
                    "get_escrow_balance({}) failed: {} -- restore continues without escrow snapshot",
                    sym, e
                ));
                // Don't abort restore -- escrow read failure shouldn't block
                // config reconstruction.
                break;
            }
        }
    }
    result.escrow_balances = Some(balances);
    result.steps_performed.push("read_escrow".into());
    if !any_balance {
        result.warnings.push(
            "escrow is empty -- if the NFT was previously trading this likely means tokens were withdrawn or the heartbeat lapsed. Trading will start with no inventory; request a mint via your strategy or `deadmkt-node deposit`.".to_string(),
        );
    } else {
        eprintln!("[restore] On-chain escrow has tokens -- ready to trade.");
    }

    // 5. Reconstruct config from network defaults ---------------------------
    let beneficiary_address = result.beneficiary_address.clone().unwrap_or_default();
    let v = ValidatedSetupConfig {
        network: network.clone(),
        beneficiary_address: beneficiary_address.clone(),
        bootstrap_only: false,
        mint_amounts: MintAmounts::default(),
        withdrawal_rules: WithdrawalRules::default(),
        faucet: FaucetCfg::default(),
    };
    let config_path = data_dir.join("config.json");
    if config_path.exists() {
        result.warnings.push(format!(
            "config.json already exists at {} -- it will be overwritten with the restored snapshot",
            config_path.display()
        ));
    }
    let node_config = build_node_config_from_chain(&v, nft_id, &address);
    if let Err(e) = node_config.save(&config_path) {
        return result.fail("write_config", e);
    }
    result.steps_performed.push("write_config".into());
    eprintln!(
        "[restore] Wrote config.json -> {}. Run `deadmkt-node` again to start trading.",
        config_path.display()
    );

    result.success = true;
    result
}

/// MR1b: LLM-assisted entry point. Keystore is already on disk; the
/// password was prompted at the CLI boundary (so the LLM that wrote
/// setup.json never saw it). Validates the config (rejects if it
/// contains a password), unlocks the keystore with the supplied
/// password, then runs the shared post-keystore orchestration.
pub async fn run_setup_noninteractive_llm_assisted(
    cfg: &SetupConfig,
    keystore_password: &str,
    chain: &dyn crate::ChainClient,
    data_dir: &std::path::Path,
) -> SetupResult {
    // 1. Validate ------------------------------------------------------------
    let v = match validate_for_llm_assisted(cfg) {
        Ok(v) => v,
        Err(e) => {
            return SetupResult::fresh_in_progress(&cfg.network, &cfg.node_role)
                .fail("validate", e)
                .with_mode(SetupMode::ConfigWithKeystore);
        }
    };

    let mut result = init_result(&v, SetupMode::ConfigWithKeystore);
    result.steps_performed.push("validate".into());

    // 2. Unlock keystore -----------------------------------------------------
    let keystore_path = data_dir.join("keystore.json");
    let (address, public, secret) = match unlock_existing_keystore(
        &keystore_path, keystore_password,
    ) {
        Ok(t) => t,
        Err(e) => return result.fail("unlock_keystore", e),
    };
    result.trustee_address = Some(address.clone());
    result.steps_performed.push("unlock_keystore".into());
    eprintln!("[setup] Keystore unlocked. Trustee address: {}", address);

    // 3. Hand off to shared post-keystore work.
    run_setup_after_keystore(v, address, public, secret, chain, data_dir, result).await
}

fn init_result(v: &ValidatedSetupConfig, mode: SetupMode) -> SetupResult {
    let network = match v.network {
        deadmkt_config::Network::Testnet => "testnet",
        deadmkt_config::Network::Mainnet => "mainnet",
    };
    let node_role = if v.bootstrap_only { "bootstrap" } else { "trading" };
    let mut r = SetupResult::fresh_in_progress(network, node_role);
    r.mode = mode;
    r.beneficiary_address = Some(v.beneficiary_address.clone());
    r
}

/// Shared post-keystore orchestration used by fresh (MR1a) and
/// LLM-assisted (MR1b) entry points. Idempotent: each chain step
/// either skips work that's already done or surfaces the underlying
/// "already X" error gracefully (see register_and_deposit catching
/// E_ALREADY_REGISTERED).
async fn run_setup_after_keystore(
    v: ValidatedSetupConfig,
    address: String,
    public: ed25519_dalek::VerifyingKey,
    secret: ed25519_dalek::SigningKey,
    chain: &dyn crate::ChainClient,
    data_dir: &std::path::Path,
    mut result: SetupResult,
) -> SetupResult {
    let config_path = data_dir.join("config.json");
    if config_path.exists() {
        result.warnings.push(format!(
            "config.json already exists at {} -- it will be overwritten",
            config_path.display()
        ));
    }

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
            result.warnings.push(
                "Mainnet: token funding/deposit must be done manually (MR1a/b do not auto-mint on mainnet).".to_string(),
            );
        }
    }

    // 8. Build + write config.json (shared helper used by MR1a/b/c) --------
    let node_config = build_node_config_from_chain(&v, nft_id, &address);

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
// Shared helpers (used by fresh/llm-assisted/restore)
// =========================================================================

/// Build a `NodeConfig` from network defaults + the chain-confirmed
/// trustee data. Used by all three non-interactive entry points so
/// they produce identical config.json shapes for the same trustee.
pub fn build_node_config_from_chain(
    v: &ValidatedSetupConfig,
    nft_id: u64,
    trustee_address: &str,
) -> deadmkt_config::NodeConfig {
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
    deadmkt_config::NodeConfig {
        network: v.network.clone(),
        rpc_urls,
        nft_id,
        trustee_address: trustee_address.to_string(),
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
    }
}

/// Derive token metadata addresses from network + contract address.
/// Used by MR1c restore for `get_escrow_balance(nft_id, metadata)`
/// queries. Matches `fetch_wallet_balances` in `src/run.rs` -- the same
/// SHA3-256(contract || seed || 0xFE) scheme primary_fungible_store uses.
pub fn network_token_metadata_addresses(
    network: &deadmkt_config::Network,
) -> Vec<(String, String)> {
    use sha3::{Digest, Sha3_256};
    let contract_addr = deadmkt_config::default_contract_addresses(network).settlement;
    let contract_bytes = hex::decode(contract_addr.trim_start_matches("0x")).unwrap_or_default();
    let seeds: &[(&str, &[u8])] = &[
        ("EMM", b"deadmkt_emm"),
        ("KAY", b"deadmkt_kay"),
        ("TEE", b"deadmkt_tee"),
    ];
    seeds.iter().map(|(sym, seed)| {
        let mut h = Sha3_256::new();
        h.update(&contract_bytes);
        h.update(seed);
        h.update(&[0xFE]); // primary_fungible_store FA metadata derivation byte
        let digest = h.finalize();
        (sym.to_string(), format!("0x{}", hex::encode(digest)))
    }).collect()
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
        let (v, password) = validate_for_fresh_setup(&cfg).unwrap();
        assert_eq!(v.network, deadmkt_config::Network::Testnet);
        assert_eq!(v.beneficiary_address, "0xabcdef0123"); // lowercased
        assert!(!v.bootstrap_only);
        assert_eq!(password, "strongpass");
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

    // ---- MR1b validation ------------------------------------------------

    #[test]
    fn t_mr1b_10_validate_for_llm_assisted_happy_path() {
        let cfg = SetupConfig {
            network: "testnet".into(),
            beneficiary_address: "0xABCDef0123".into(),
            node_role: "trading".into(),
            keystore_password: None, // LLM safety: no password in config
            mint_amounts: MintAmounts::default(),
            withdrawal_rules: WithdrawalRules::default(),
            faucet: FaucetCfg::default(),
        };
        let v = validate_for_llm_assisted(&cfg).unwrap();
        assert_eq!(v.network, deadmkt_config::Network::Testnet);
        assert_eq!(v.beneficiary_address, "0xabcdef0123");
    }

    #[test]
    fn t_mr1b_11_validate_for_llm_assisted_rejects_password_in_config() {
        let cfg = SetupConfig {
            network: "testnet".into(),
            beneficiary_address: "0xABCDef0123".into(),
            node_role: "trading".into(),
            keystore_password: Some("oops".into()), // present -> rejected
            mint_amounts: MintAmounts::default(),
            withdrawal_rules: WithdrawalRules::default(),
            faucet: FaucetCfg::default(),
        };
        let err = validate_for_llm_assisted(&cfg).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("rejects setup.json that contains keystore_password"), "unexpected error: {}", msg);
    }

    // ---- MR1b unlock_existing_keystore -----------------------------------

    #[test]
    fn t_mr1b_20_unlock_existing_keystore_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("keystore.json");
        // Create a keystore with MR1a's generator.
        let (addr_a, pub_a, _sec_a) = generate_keystore_noninteractive(&path, "strongpass").unwrap();
        // Unlock via MR1b's helper.
        let (addr_b, pub_b, _sec_b) = unlock_existing_keystore(&path, "strongpass").unwrap();
        // Same keys -> same derived address.
        assert_eq!(addr_a, addr_b);
        assert_eq!(pub_a.as_bytes(), pub_b.as_bytes());
    }

    #[test]
    fn t_mr1b_21_unlock_existing_keystore_wrong_password() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("keystore.json");
        generate_keystore_noninteractive(&path, "rightpass").unwrap();
        let err = unlock_existing_keystore(&path, "wrongpass").unwrap_err();
        let msg = format!("{}", err);
        // Underlying keystore lib surfaces a "keystore error" -- key check is
        // that we get an Err and the failure path is the same as bad-password,
        // not a panic.
        assert!(msg.contains("keystore") || msg.contains("decrypt") || msg.contains("password"),
            "unexpected error: {}", msg);
    }

    // ---- MR1c helpers ----------------------------------------------------

    #[test]
    fn t_mr1c_10_build_node_config_from_chain_round_trips_fields() {
        let v = ValidatedSetupConfig {
            network: deadmkt_config::Network::Testnet,
            beneficiary_address: "0xbeef".into(),
            bootstrap_only: false,
            mint_amounts: MintAmounts::default(),
            withdrawal_rules: WithdrawalRules {
                holding_period_days: 90,
                rushed_withdrawal_enabled: true,
            },
            faucet: FaucetCfg::default(),
        };
        let cfg = build_node_config_from_chain(&v, 42, "0xfeedface");
        assert_eq!(cfg.nft_id, 42);
        assert_eq!(cfg.trustee_address, "0xfeedface");
        assert_eq!(cfg.beneficiary_address, "0xbeef");
        assert_eq!(cfg.network, deadmkt_config::Network::Testnet);
        assert_eq!(cfg.contracts.settlement, "0x9b8fd778b08131297d22b577c1f4e2f6ed85d04479cd73e0eb14bbf41fc6731c");
        assert!(cfg.rpc_urls[0].contains("rpc-testnet"));
        assert_eq!(cfg.bootstrap_peers.len(), 5);
        assert_eq!(cfg.withdrawal_rules.holding_period_days, 90);
        assert!(cfg.withdrawal_rules.rushed_withdrawal_enabled);
        assert!(!cfg.strategy_auth_token.is_empty());
    }

    #[test]
    fn t_mr1c_11_network_token_metadata_addresses_are_deterministic() {
        let net = deadmkt_config::Network::Testnet;
        let a = network_token_metadata_addresses(&net);
        let b = network_token_metadata_addresses(&net);
        assert_eq!(a, b);
        assert_eq!(a.len(), 3);
        let symbols: Vec<&str> = a.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(symbols, vec!["EMM", "KAY", "TEE"]);
        // All three should be 0x + 64 hex (sha3_256 digest)
        for (_, addr) in &a {
            assert!(addr.starts_with("0x"));
            assert_eq!(addr.len(), 66);
        }
        // EMM/KAY/TEE must derive to distinct addresses
        assert_ne!(a[0].1, a[1].1);
        assert_ne!(a[1].1, a[2].1);
        assert_ne!(a[0].1, a[2].1);
    }

    #[test]
    fn t_mr1b_22_unlock_existing_keystore_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        let err = unlock_existing_keystore(&path, "anything").unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("no keystore at"), "unexpected error: {}", msg);
        assert!(msg.contains("MR1a")); // hint to use fresh setup
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
