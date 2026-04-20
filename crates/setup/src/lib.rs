// deadmkt-setup: First boot wizard.
// Interactive 10-step setup: network, beneficiary, keygen, funding,
// NFT mint/import, withdrawal config, register+deposit, profit config,
// auth token, bootstrap peers.

use deadmkt_config::{
    default_contract_addresses, generate_strategy_auth_token,
    ConfigError, Network, NodeConfig, ProfitConfig, WithdrawalConfig,
};
use deadmkt_keystore::{save_keystore, KeystoreError};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

/// Sleep that can be interrupted by Ctrl+C. Returns Err(Aborted) on signal.
async fn cancellable_sleep(duration: std::time::Duration) -> Result<(), SetupError> {
    tokio::select! {
        _ = tokio::time::sleep(duration) => Ok(()),
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\nSetup interrupted by user.");
            Err(SetupError::Aborted)
        }
    }
}

#[derive(Debug, Error)]
pub enum SetupError {
    #[error("config error: {0}")]
    ConfigError(#[from] ConfigError),

    #[error("keystore error: {0}")]
    KeystoreError(#[from] KeystoreError),

    #[error("chain error: {0}")]
    ChainError(String),

    #[error("user declined")]
    UserDeclined,

    #[error("wizard aborted")]
    Aborted,

    #[error("IO error: {0}")]
    IoError(String),
}

// =========================================================================
// IO trait for testable prompts
// =========================================================================

pub trait WizardIO {
    fn print(&mut self, msg: &str);
    fn read_line(&mut self) -> Result<String, SetupError>;
    fn is_aborted(&self) -> bool;
}

// =========================================================================
// Chain client trait for testable chain calls
// =========================================================================

#[derive(Debug, Clone)]
pub struct WalletBalance {
    pub supra: u64,
    pub tokens: Vec<TokenBalance>,
}

#[derive(Debug, Clone)]
pub struct TokenBalance {
    pub symbol: String,
    pub amount: u64,
    pub metadata_address: String,
}

#[derive(Debug, Clone)]
pub struct NftConfigInfo {
    pub bond_amount: u64,
    pub bond_lock_seconds: u64,
    pub admin: String,
}

#[derive(Debug, Clone)]
pub struct TxResultInfo {
    pub success: bool,
    pub gas_used: u64,
    pub vm_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistingNft {
    Found { nft_id: u64, beneficiary: String },
    NotFound,
}

pub trait ChainClient: Send + Sync {
    fn get_all_balances(
        &self,
        address: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<WalletBalance, SetupError>> + Send + '_>>;

    fn is_trustee(
        &self,
        address: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, SetupError>> + Send + '_>>;

    fn get_nft_id(
        &self,
        address: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64, SetupError>> + Send + '_>>;

    fn get_beneficiary(
        &self,
        nft_id: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, SetupError>> + Send + '_>>;

    fn get_nft_config(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<NftConfigInfo, SetupError>> + Send + '_>>;

    fn submit_mint(
        &self,
        pubkey: &[u8],
        beneficiary: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>;

    fn get_total_minted(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64, SetupError>> + Send + '_>>;

    fn submit_register(
        &self,
        nft_id: u64,
        holding_period_days: u64,
        rushed_withdrawal_enabled: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>;

    fn submit_deposit(
        &self,
        metadata_address: &str,
        amount: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>;

    /// Provide signing key to client after keypair generation (step 3).
    /// Default no-op for test mocks.
    fn set_signer(&self, _secret_key: &[u8; 32], _public_key: &[u8; 32], _address: &str) {}

    // -----------------------------------------------------------------
    // Trippples token methods (tokens.move)
    // -----------------------------------------------------------------

    /// Call tokens::request_mint(m_amount, k_amount, t_amount).
    /// SUPRA is transferred to PendingMintEscrow on-chain.
    fn submit_request_mint(
        &self,
        _m_amount: u64,
        _k_amount: u64,
        _t_amount: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_request_mint not implemented".into())) })
    }

    /// Call tokens::claim_mint(). Moves SUPRA from escrow to treasury, mints tokens.
    fn submit_claim_mint(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_claim_mint not implemented".into())) })
    }

    /// Call tokens::get_metadata_address(symbol) where symbol is 0=EMM, 1=KAY, 2=TEE.
    fn get_trippples_metadata(
        &self,
        _symbol: u8,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("get_trippples_metadata not implemented".into())) })
    }

    /// Get fungible asset balance for a metadata address at the signer's address.
    fn get_fa_balance(
        &self,
        _metadata_address: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64, SetupError>> + Send + '_>> {
        Box::pin(async { Ok(0) })
    }

    /// Get escrow balance for an nft_id + token metadata.
    fn get_escrow_balance(
        &self,
        _nft_id: u64,
        _metadata_address: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64, SetupError>> + Send + '_>> {
        Box::pin(async { Ok(0) })
    }

    /// Get per-trustee mint state: (first_mint_completed, has_pending).
    fn get_nft_mint_state(
        &self,
        _address: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(bool, bool), SetupError>> + Send + '_>> {
        Box::pin(async { Ok((false, false)) })
    }

    /// Get pending mint details: Option<claimable_at timestamp>.
    fn get_pending_mint_claimable_at(
        &self,
        _address: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<u64>, SetupError>> + Send + '_>> {
        Box::pin(async { Ok(None) })
    }

    /// Call tokens::burn_from_escrow(amount). Withdraws triples from escrow,
    /// burns them, returns SUPRA. C2 contract function.
    fn submit_burn_from_escrow(
        &self,
        _amount: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_burn_from_escrow not implemented".into())) })
    }

    /// Call tokens::burn_to_beneficiary(amount). Burns triples from escrow,
    /// returns SUPRA to beneficiary. Profit distribution path.
    fn submit_burn_to_beneficiary(
        &self,
        _amount: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_burn_to_beneficiary not implemented".into())) })
    }

    /// Call tokens::lock_from_escrow(symbol, amount, min_duration_secs).
    fn submit_lock_from_escrow(
        &self,
        _symbol: u8,
        _amount: u64,
        _duration_secs: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_lock_from_escrow not implemented".into())) })
    }

    /// Call tokens::unlock_to_escrow(lock_index).
    fn submit_unlock_to_escrow(
        &self,
        _lock_index: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_unlock_to_escrow not implemented".into())) })
    }

    /// Call tokens::donate_dust(recipient_nft_id, symbol, amount).
    fn submit_donate_dust(
        &self,
        _recipient_nft_id: u64,
        _symbol: u8,
        _amount: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_donate_dust not implemented".into())) })
    }
}

// =========================================================================
// Wizard step functions
// =========================================================================

/// Step 1: network selection (testnet only for now)
pub fn prompt_network(io: &mut dyn WizardIO) -> Result<Network, SetupError> {
    io.print("\nNetwork: Testnet (mainnet not yet available)\n");
    Ok(Network::Testnet)
}

/// Step 2: beneficiary address (required)
pub fn prompt_beneficiary(io: &mut dyn WizardIO) -> Result<String, SetupError> {
    loop {
        io.print("\nEnter Supra blockchain beneficiary address\n");
        io.print("(The address that receives profits and can withdraw from escrow)\n");
        io.print("(This can be your own address if self-funded):\n> ");
        let input = io.read_line()?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            io.print("Beneficiary address is required.\n");
            continue;
        }
        if !trimmed.starts_with("0x") || trimmed.len() < 10 {
            io.print("Invalid address. Must start with 0x.\n");
            continue;
        }
        return Ok(trimmed.to_string());
    }
}

/// Step 2b: node role selection
/// Returns true if this is a bootstrap/relay-only node (no trading).
pub fn prompt_node_role(io: &mut dyn WizardIO) -> Result<bool, SetupError> {
    io.print("\nNode role:\n");
    io.print("  1) Trading node (full setup — NFT + tokens + escrow)\n");
    io.print("  2) Bootstrap/relay node (NFT only — no trading)\n");
    io.print("> ");
    let input = io.read_line()?;
    let bootstrap_only = input.trim() == "2";
    if bootstrap_only {
        io.print("Bootstrap mode: will mint NFT only, no token minting or escrow deposit.\n");
    }
    Ok(bootstrap_only)
}

/// Step 3: keypair generation + keystore save (or load existing)
pub fn wizard_generate_keypair(
    io: &mut dyn WizardIO,
    keystore_path: &Path,
) -> Result<(String, ed25519_dalek::VerifyingKey, ed25519_dalek::SigningKey), SetupError> {
    use sha3::{Sha3_256, Digest};

    // Check for existing keystore
    if keystore_path.exists() {
        io.print("\nExisting keystore found. Enter password to unlock:\n> ");
        let password = io.read_line()?;

        match deadmkt_keystore::load_keystore(keystore_path, password.trim()) {
            Ok((secret, public)) => {
                let mut hasher = Sha3_256::new();
                hasher.update(public.as_bytes());
                hasher.update(&[0x00]);
                let hash = hasher.finalize();
                let address = format!("0x{}", hex::encode(hash));
                io.print(&format!("Keystore unlocked. Trustee wallet: {}\n", address));
                return Ok((address, public, secret));
            }
            Err(e) => {
                io.print(&format!("Failed to unlock keystore: {}. Generating new keypair.\n", e));
                // Fall through to generate new keypair
            }
        }
    }

    io.print("\nGenerating Ed25519 keypair...\n");
    io.print("(Password that secures your node on multiple levels, recommended to be\n");
    io.print("at least 12 mixed alpha-numeric and special chars)\n");

    let (secret, public) = deadmkt_crypto::generate_keypair();

    let mut hasher = Sha3_256::new();
    hasher.update(public.as_bytes());
    hasher.update(&[0x00]); // Ed25519 scheme identifier
    let hash = hasher.finalize();
    let address = format!("0x{}", hex::encode(hash));

    loop {
        io.print("Enter keystore password:\n> ");
        let pass1 = io.read_line()?;
        io.print("Confirm password:\n> ");
        let pass2 = io.read_line()?;

        if pass1.trim() == pass2.trim() {
            save_keystore(keystore_path, &secret, &public, pass1.trim())?;
            io.print(&format!("Trustee wallet: {}\n", address));
            return Ok((address, public, secret));
        } else {
            io.print("Passwords don't match. Try again.\n");
        }
    }
}

/// Trippples: exchange rate is 10 tokens per 1 SUPRA.
/// 1 SUPRA = 100_000_000 quants. 1 token = 100_000 base units (5 decimals).
/// SUPRA_PER_TOKEN_UNIT = 100 quants per base unit.
const SUPRA_PER_TOKEN_UNIT: u64 = 100;

/// Use 70% of SUPRA for initial token minting, reserve 30% for gas.
/// Strategy can mint more on-demand if needed.
const MINT_SUPRA_FRACTION_PCT: u64 = 70;

/// Step 4: wait for funding
/// On testnet, waits for SUPRA then mints Trippples tokens (EMM/KAY/TEE).
/// On mainnet, waits for SUPRA + tokens (manual funding).
pub async fn wait_for_funding(
    io: &mut dyn WizardIO,
    chain: &dyn ChainClient,
    address: &str,
    network: &Network,
    poll_interval: std::time::Duration,
) -> Result<WalletBalance, SetupError> {
    // Wait for SUPRA (gas money)
    let supra_balance = loop {
        let balance = chain.get_all_balances(address).await?;
        if balance.supra > 0 {
            break balance.supra;
        }
        if !balance.tokens.is_empty() {
            break balance.supra;
        }
        cancellable_sleep(poll_interval).await?;
    };

    // Testnet: just report SUPRA received, tokens will be minted after registration
    if *network == Network::Testnet {
        io.print("  SUPRA received.\n");
        return Ok(WalletBalance {
            supra: supra_balance,
            tokens: vec![],
        });
    }

    // Mainnet: wait for manual token funding
    loop {
        let balance = chain.get_all_balances(address).await?;
        if balance.supra > 0 && !balance.tokens.is_empty() {
            return Ok(balance);
        }
        cancellable_sleep(poll_interval).await?;
    }
}

/// Step 5a: check for existing NFT
pub async fn check_existing_nft(
    chain: &dyn ChainClient,
    address: &str,
) -> Result<ExistingNft, SetupError> {
    let is_trustee = chain.is_trustee(address).await?;
    if !is_trustee {
        return Ok(ExistingNft::NotFound);
    }
    let nft_id = chain.get_nft_id(address).await?;
    let beneficiary = chain.get_beneficiary(nft_id).await?;
    Ok(ExistingNft::Found { nft_id, beneficiary })
}

/// Step 5b: mint NFT pair
pub async fn mint_nft_pair(
    io: &mut dyn WizardIO,
    chain: &dyn ChainClient,
    pubkey: &[u8],
    beneficiary: &str,
) -> Result<u64, SetupError> {
    let config = chain.get_nft_config().await?;
    let bond_display = config.bond_amount / 100_000_000; // 8 decimals
    let lock_seconds = config.bond_lock_seconds;

    io.print("\nBond required:\n");
    if lock_seconds < 86400 {
        io.print(&format!(
            "  {} SUPRA (refundable after {} seconds)\n",
            format_with_commas(bond_display),
            lock_seconds
        ));
    } else {
        let days = lock_seconds / 86400;
        io.print(&format!(
            "  {} SUPRA (refundable after {} days)\n",
            format_with_commas(bond_display),
            days
        ));
    }
    io.print("We need something to deter NFT churn... we want to encourage valuing your\n");
    io.print("trustee/beneficiary NFT pair... otherwise we may have to start selling them,\n");
    io.print("so you value it...\n");
    io.print("Proceed? (y/n) (you need to accept this bond to proceed...)\n> ");
    let answer = io.read_line()?;

    if answer.trim().to_lowercase() != "y" {
        return Err(SetupError::UserDeclined);
    }

    let result = chain.submit_mint(pubkey, beneficiary).await?;
    if !result.success {
        return Err(SetupError::ChainError(result.vm_status));
    }

    let nft_id = chain.get_total_minted().await?;
    io.print(&format!("TrusteeNFT #{} minted successfully!\n", nft_id));
    Ok(nft_id)
}

/// Step 6a: withdrawal config
pub fn prompt_withdrawal_config(io: &mut dyn WizardIO) -> Result<WithdrawalConfig, SetupError> {
    io.print("\nConfigure limitations on Beneficiary exit path...\n\n");

    io.print("Exit Path 1: Holding Period -> claim_all\n");
    io.print("- Who initiates: Beneficiary\n");
    io.print("- Scope: Everything (sweeps all tokens)\n");
    io.print("- Delay: holding_period_days in days (slow)\n");
    io.print("- Auto-trigger: Yes, if trustee goes inactive\n");
    io.print("- Cancellable: Yes, by beneficiary (before expiry)\n");
    io.print("- Effect: Gets all non-zero balances\n\n");

    io.print("Exit Path 2: Rushed Withdrawal\n");
    io.print("- Who initiates: Beneficiary\n");
    io.print("- Scope: Partial (specific token + amount)\n");
    io.print("- Delay: Grace period in batches (fast)\n");
    io.print("- Auto-trigger: No\n");
    io.print("- Cancellable: Yes, by beneficiary\n");
    io.print("- Effect: Gets min(requested, available)\n\n");

    io.print("Paths are mutually exclusive - can't have both active at the same time.\n");
    io.print("Whichever is active blocks the other.\n\n");

    io.print("Path 1 Explained: Beneficiary starts the holding period countdown. After\n");
    io.print("holding_period_days expires, beneficiary calls claim_all and sweeps everything\n");
    io.print("in escrow. This is the nuclear option — full exit. Also triggers automatically\n");
    io.print("if the trustee goes inactive for longer than holding_period_days (protects the\n");
    io.print("beneficiary if the trustee disappears). This is really asking the trustee:\n");
    io.print("\"How long should the beneficiary have to wait before they can claim all your\n");
    io.print("funds?\" - that's the holding period. It protects the trustee's trading\n");
    io.print("operation. Higher = more trading runway before the beneficiary can pull the\n");
    io.print("plug. The beneficiary is the one who activates the holding period when they\n");
    io.print("want their money back. The trustee sets the holding duration\n");
    io.print("now.\n\n");

    io.print("Path 2 Explained: This allows the beneficiary to request a specific amount of\n");
    io.print("token, no penalty - just a small delay so the trustee isn't blindsided\n");
    io.print("mid-trade. This is really asking the trustee: \"Can the beneficiary do rushed\n");
    io.print("(partial) withdrawals at all?\" — if no, the beneficiary's only option is the\n");
    io.print("full holding period + claim_all.\n\n");

    let days = loop {
        io.print("Path 1 holding_period_days (1-367, measured in days, default 90):\n> ");
        let input = io.read_line()?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            break 90;
        }
        match trimmed.parse::<u32>() {
            Ok(d) if (1..=367).contains(&d) => break d,
            _ => io.print("Invalid. Enter a number between 1 and 367.\n"),
        }
    };

    io.print("Enable rushed withdrawal? (Path 2) (y/n, default y):\n> ");
    let answer = io.read_line()?;
    let rushed = answer.trim().to_lowercase() != "n";

    Ok(WithdrawalConfig {
        holding_period_days: days,
        rushed_withdrawal_enabled: rushed,
    })
}

/// Step 6b: register trader + deposit
pub async fn register_and_deposit(
    chain: &dyn ChainClient,
    nft_id: u64,
    withdrawal: &WithdrawalConfig,
    funding: &WalletBalance,
) -> Result<(), SetupError> {
    // Register (skip if already registered from a previous run)
    let reg = chain.submit_register(
        nft_id,
        withdrawal.holding_period_days as u64,
        withdrawal.rushed_withdrawal_enabled,
    ).await?;
    if !reg.success {
        if reg.vm_status.contains("ALREADY_REGISTERED") || reg.vm_status.contains("E_ALREADY_REGISTERED") {
            // Already registered from a previous wizard run -- continue
        } else {
            return Err(SetupError::ChainError(reg.vm_status));
        }
    }

    // Deposit each non-SUPRA token (if any already minted)
    for token in &funding.tokens {
        chain.submit_deposit(&token.metadata_address, token.amount).await?;
    }

    Ok(())
}

/// Auto-mint Trippples tokens and deposit to escrow.
/// Called AFTER registration so request_mint can verify the caller is registered.
///
/// Checks in order:
/// 1. Tokens already in escrow → skip minting entirely, strategy manages from here
/// 2. Tokens in wallet → deposit to escrow
/// 3. Pending mint claimable → claim, deposit
/// 4. Pending mint not yet claimable → skip, strategy will claim later
/// 5. No pending mint → request first mint, wait for claim (first mint ~8 min)
pub async fn auto_mint_and_deposit(
    io: &mut dyn WizardIO,
    chain: &dyn ChainClient,
    supra_balance: u64,
    nft_id: u64,
    trustee_address: &str,
) -> Result<Vec<TokenBalance>, SetupError> {
    let emm_meta = chain.get_trippples_metadata(0).await?;
    let kay_meta = chain.get_trippples_metadata(1).await?;
    let tee_meta = chain.get_trippples_metadata(2).await?;

    // ── Check 1: tokens already in escrow ──
    let esc_emm = chain.get_escrow_balance(nft_id, &emm_meta).await.unwrap_or(0);
    let esc_kay = chain.get_escrow_balance(nft_id, &kay_meta).await.unwrap_or(0);
    let esc_tee = chain.get_escrow_balance(nft_id, &tee_meta).await.unwrap_or(0);

    if esc_emm > 0 || esc_kay > 0 || esc_tee > 0 {
        io.print(&format!("  Escrow already funded: EMM={:.5}, KAY={:.5}, TEE={:.5}\n",
            esc_emm as f64 / 100_000.0, esc_kay as f64 / 100_000.0, esc_tee as f64 / 100_000.0));
        io.print("  Strategy will manage token minting from here.\n");
        return Ok(vec![
            TokenBalance { symbol: "EMM".into(), amount: esc_emm, metadata_address: emm_meta },
            TokenBalance { symbol: "KAY".into(), amount: esc_kay, metadata_address: kay_meta },
            TokenBalance { symbol: "TEE".into(), amount: esc_tee, metadata_address: tee_meta },
        ]);
    }

    // ── Check 2: tokens in wallet ──
    let wal_emm = chain.get_fa_balance(&emm_meta).await.unwrap_or(0);
    let wal_kay = chain.get_fa_balance(&kay_meta).await.unwrap_or(0);
    let wal_tee = chain.get_fa_balance(&tee_meta).await.unwrap_or(0);

    io.print(&format!("  Balance check: wallet EMM={}, KAY={}, TEE={}\n", wal_emm, wal_kay, wal_tee));

    if wal_emm > 0 && wal_kay > 0 && wal_tee > 0 {
        io.print("  Tokens in wallet — depositing to escrow...\n");
        let dep1 = chain.submit_deposit(&emm_meta, wal_emm).await?;
        if !dep1.success {
            return Err(SetupError::ChainError(format!("EMM deposit failed: {}", dep1.vm_status)));
        }
        let dep2 = chain.submit_deposit(&kay_meta, wal_kay).await?;
        if !dep2.success {
            return Err(SetupError::ChainError(format!("KAY deposit failed: {}", dep2.vm_status)));
        }
        let dep3 = chain.submit_deposit(&tee_meta, wal_tee).await?;
        if !dep3.success {
            return Err(SetupError::ChainError(format!("TEE deposit failed: {}", dep3.vm_status)));
        }
        io.print("    Deposited!\n");
        return Ok(vec![
            TokenBalance { symbol: "EMM".into(), amount: wal_emm, metadata_address: emm_meta },
            TokenBalance { symbol: "KAY".into(), amount: wal_kay, metadata_address: kay_meta },
            TokenBalance { symbol: "TEE".into(), amount: wal_tee, metadata_address: tee_meta },
        ]);
    }

    // ── Check 3/4: pending mint state ──
    let (first_mint_done, has_pending) = chain.get_nft_mint_state(trustee_address).await.unwrap_or((false, false));

    if has_pending {
        // Check if claimable now
        let claimable_at = chain.get_pending_mint_claimable_at(trustee_address).await.unwrap_or(None);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();

        if let Some(at) = claimable_at {
            if now >= at {
                io.print("  Pending mint is claimable — claiming...\n");
                let claim = chain.submit_claim_mint().await?;
                if !claim.success {
                    return Err(SetupError::ChainError(format!("Claim failed: {}", claim.vm_status)));
                }
                io.print("    Claimed! Checking escrow...\n");
                // C1: claim_mint deposits directly to escrow — check escrow, not wallet
                for poll in 1..=30 {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let e = chain.get_escrow_balance(nft_id, &emm_meta).await.unwrap_or(0);
                    let k = chain.get_escrow_balance(nft_id, &kay_meta).await.unwrap_or(0);
                    let t = chain.get_escrow_balance(nft_id, &tee_meta).await.unwrap_or(0);
                    if e > 0 && k > 0 && t > 0 {
                        io.print(&format!("    Escrow funded: EMM={:.5}, KAY={:.5}, TEE={:.5}\n",
                            e as f64 / 100_000.0, k as f64 / 100_000.0, t as f64 / 100_000.0));
                        return Ok(vec![
                            TokenBalance { symbol: "EMM".into(), amount: e, metadata_address: emm_meta },
                            TokenBalance { symbol: "KAY".into(), amount: k, metadata_address: kay_meta },
                            TokenBalance { symbol: "TEE".into(), amount: t, metadata_address: tee_meta },
                        ]);
                    }
                    if poll % 5 == 0 {
                        io.print(&format!("    Waiting for escrow... ({}/30)\n", poll));
                    }
                }
                return Err(SetupError::ChainError(
                    "Claim succeeded but escrow not funded after 90s. Re-run setup.".into(),
                ));
            }
        }

        // Pending but not yet claimable — strategy will handle it
        let wait_str = claimable_at.map(|at| {
            let remaining = at.saturating_sub(now);
            let hours = remaining / 3600;
            let mins = (remaining % 3600) / 60;
            format!("~{}h {}m remaining", hours, mins)
        }).unwrap_or_else(|| "unknown wait time".into());
        io.print(&format!("  Pending mint found ({}) — strategy will claim when ready.\n", wait_str));
        io.print("  Node can start trading once tokens are claimed and deposited.\n");
        return Ok(vec![]);
    }

    // ── Check 5: no pending mint — request first mint ──
    if first_mint_done {
        // Second+ mint goes through global dVRF — could be days
        io.print("  First mint already completed. Requesting additional tokens...\n");
        io.print("  NOTE: subsequent mints use the global hold period (may take days).\n");
    } else {
        io.print("  Minting Trippples tokens (first mint ~8 min hold)...\n");
    }

    // Ask which token to weight (creates the initial trading imbalance)
    io.print("\nWhich token do you want more of? This creates your initial\n");
    io.print("trading position (you'll trade the surplus for what you need).\n");
    io.print("  [E]MM  [K]AY  [T]EE  (default: TEE)\n> ");
    let weight_input = io.read_line()?;
    let weight_choice = weight_input.trim().to_uppercase();
    let weighted_index: usize = match weight_choice.as_str() {
        "E" | "EMM" => 0,
        "K" | "KAY" => 1,
        _ => 2, // default TEE
    };
    let weight_label = ["EMM", "KAY", "TEE"][weighted_index];
    io.print(&format!("  Weighting towards {}\n", weight_label));

    let available_supra = supra_balance * MINT_SUPRA_FRACTION_PCT / 100;
    let total_base_units = available_supra / SUPRA_PER_TOKEN_UNIT;
    // Round total down to nearest 1_000_000 (divisible by 10 tokens)
    let total = (total_base_units / 1_000_000) * 1_000_000;

    if total < 1_000_000 {
        return Err(SetupError::ChainError(
            "Insufficient SUPRA to mint. Fund with more SUPRA and try again.".into(),
        ));
    }

    // Compute skewed split: weighted token gets 40%, others get 30% each.
    // For total=1_000_000 (10 tokens): 300000/300000/400000 (3/3/4 pattern).
    // Round each to nearest 100_000 (1 token) then adjust remainder.
    let heavy = (total * 4 / 10 / 100_000) * 100_000;
    let light = (total * 3 / 10 / 100_000) * 100_000;
    let remainder = total - heavy - 2 * light;
    // Add remainder to heavy token (keeps it within ratio)
    let heavy = heavy + remainder;

    let (m_amount, k_amount, t_amount) = match weighted_index {
        0 => (heavy, light, light),
        1 => (light, heavy, light),
        _ => (light, light, heavy),
    };

    let m_display = m_amount as f64 / 100_000.0;
    let k_display = k_amount as f64 / 100_000.0;
    let t_display = t_amount as f64 / 100_000.0;
    let supra_display = (total * SUPRA_PER_TOKEN_UNIT) as f64 / 100_000_000.0;
    io.print(&format!("    Minting {:.5} EMM, {:.5} KAY, {:.5} TEE ({:.8} SUPRA)\n",
        m_display, k_display, t_display, supra_display));

    let mint_result = chain.submit_request_mint(m_amount, k_amount, t_amount).await?;
    if !mint_result.success {
        if mint_result.vm_status.contains("HAS_PENDING_MINT") || mint_result.vm_status.contains("E_HAS_PENDING_MINT") {
            io.print("    Pending mint exists — strategy will claim when ready.\n");
            return Ok(vec![]);
        }
        return Err(SetupError::ChainError(format!("Failed to request mint: {}", mint_result.vm_status)));
    }

    // First mint: wait for ~8 min hold period
    if !first_mint_done {
        io.print("    Mint requested. Waiting for first-mint hold period...\n");
        let max_attempts = 60; // 60 * 10s = 10 min
        for attempt in 1..=max_attempts {
            cancellable_sleep(std::time::Duration::from_secs(10)).await?;
            let claim_result = chain.submit_claim_mint().await?;
            if claim_result.success {
                io.print("    Tokens claimed!\n");
                // C1: claim_mint deposits directly to escrow — check escrow, not wallet
                io.print("    Waiting for escrow to be funded...\n");
                for poll in 1..=30 {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let e = chain.get_escrow_balance(nft_id, &emm_meta).await.unwrap_or(0);
                    let k = chain.get_escrow_balance(nft_id, &kay_meta).await.unwrap_or(0);
                    let t = chain.get_escrow_balance(nft_id, &tee_meta).await.unwrap_or(0);
                    if e > 0 && k > 0 && t > 0 {
                        io.print(&format!("    Escrow funded: EMM={:.5}, KAY={:.5}, TEE={:.5}\n",
                            e as f64 / 100_000.0, k as f64 / 100_000.0, t as f64 / 100_000.0));
                        return Ok(vec![
                            TokenBalance { symbol: "EMM".into(), amount: e, metadata_address: emm_meta },
                            TokenBalance { symbol: "KAY".into(), amount: k, metadata_address: kay_meta },
                            TokenBalance { symbol: "TEE".into(), amount: t, metadata_address: tee_meta },
                        ]);
                    }
                    if poll % 5 == 0 {
                        io.print(&format!("    Waiting for escrow... ({}/30)\n", poll));
                    }
                }
                return Err(SetupError::ChainError(
                    "Claim succeeded but escrow not funded after 90s. Re-run setup.".into(),
                ));
            }
            let status = &claim_result.vm_status;
            if status.contains("HOLD_PERIOD_ACTIVE") {
                io.print(&format!("    Hold period active, retrying in 10s... ({}/{})\n", attempt, max_attempts));
            } else if status.contains("INSUFFICIENT_BALANCE_FOR_TRANSACTION_FEE") {
                return Err(SetupError::ChainError(
                    "Wallet out of gas. Fund the trustee address with more SUPRA and re-run setup.".into(),
                ));
            } else {
                io.print(&format!("    Claim failed: {} — retrying ({}/{})\n", status, attempt, max_attempts));
            }
        }
        return Err(SetupError::ChainError(
            "Timed out waiting for first mint claim. Re-run setup to try again.".into(),
        ));
    }

    // Non-first mint: don't wait, let strategy handle it
    io.print("    Mint requested — strategy will claim when hold period expires.\n");
    Ok(vec![])
}

/// Step 7: profit-taking config
pub fn prompt_profit_config(
    io: &mut dyn WizardIO,
    deposits: &[TokenBalance],
) -> Result<ProfitConfig, SetupError> {
    io.print("\nHow much profit before we start sweeping SUPRA to the beneficiary?\n\n");

    io.print("Example:\n");
    io.print("  You deposit 99 EMM, 99 KAY, 99 TEE into escrow. Threshold = 20%.\n");
    io.print("  Gate: all 3 must exceed 99 x 1.20 = 118.8 before any sweep.\n");
    io.print("  After trading, escrow holds 119 EMM, 155 KAY, 127 TEE:\n");
    io.print("    All above 118.8? EMM 119 yes, KAY 155 yes, TEE 127 yes.\n");
    io.print("    Excess above base: EMM=20, KAY=56, TEE=28\n");
    io.print("    Min excess = 20 (EMM is bottleneck)\n");
    io.print("    Burn: 20 EMM + 20 KAY + 20 TEE = 60 MKT -> 6 SUPRA\n");
    io.print("    6 SUPRA sent to beneficiary.\n");
    io.print("  Escrow after sweep: 99 EMM, 135 KAY, 107 TEE (base capital preserved)\n");
    io.print("  Beneficiary receives only SUPRA — never raw tokens.\n\n");

    io.print("All non-SUPRA tokens are deposited into escrow for trading.\n");
    io.print("SUPRA stays in the wallet for gas.\n\n");

    io.print("Profit-taking threshold %\n(default 20, press enter to skip/disable):\n> ");
    let input = io.read_line()?;
    let trimmed = input.trim();

    if trimmed.is_empty() && deposits.is_empty() {
        return Ok(ProfitConfig {
            base_capital: HashMap::new(),
            threshold_pct: 0,
            transfer_mode: "all_excess".into(),
            transfer_mode_value: None,
        });
    }

    let threshold = if trimmed.is_empty() {
        20
    } else {
        trimmed.parse::<u32>().unwrap_or(20)
    };

    // Auto-detect base_capital from deposits
    let mut base_capital = HashMap::new();
    for token in deposits {
        base_capital.insert(token.symbol.clone(), token.amount);
    }

    Ok(ProfitConfig {
        base_capital,
        threshold_pct: threshold,
        transfer_mode: "all_excess".into(),
        transfer_mode_value: None,
    })
}

/// Step 8: strategy auth token
pub fn wizard_generate_auth_token(io: &mut dyn WizardIO) -> String {
    let token = generate_strategy_auth_token();
    io.print(&format!("\nStrategy auth token generated: {}\n", token));
    io.print("(use this token to connect openclaw/bot/strategy.py to deadmkt node)\n");
    token
}

/// Step 9: bootstrap peers (placeholder)
pub fn wizard_bootstrap_peers(io: &mut dyn WizardIO, network: &Network) -> Vec<String> {
    io.print("P2P bootstrap peers loaded from network defaults.\n");
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

// =========================================================================
// Full wizard runner
// =========================================================================

pub async fn run_wizard(
    io: &mut dyn WizardIO,
    chain: &dyn ChainClient,
    data_dir: &Path,
) -> Result<NodeConfig, SetupError> {
    let config_path = data_dir.join("config.json");
    let keystore_path = data_dir.join("keystore.json");

    // Give docker attach time to connect before showing prompts
    std::thread::sleep(std::time::Duration::from_secs(2));
    io.print("\n========================================\n");
    io.print("  deadmkt-node setup wizard\n");
    io.print("========================================\n\n");
    io.print("Press enter to begin setup...\n> ");
    let _ = io.read_line()?;

    // Step 1
    let network = prompt_network(io)?;

    // Step 2
    let beneficiary = prompt_beneficiary(io)?;

    // Step 2b
    let bootstrap_only = prompt_node_role(io)?;

    // Step 3
    let (address, public, secret) = wizard_generate_keypair(io, &keystore_path)?;

    // Provide signing key to chain client for tx submission
    chain.set_signer(
        secret.as_bytes(),
        public.as_bytes(),
        &address,
    );

    // Step 4
    io.print(&format!("\nWaiting for funding to {}...\n", address));
    io.print("(send some Supra to this address for capital and gas)...\n");
    if network == Network::Testnet && !bootstrap_only {
        io.print("(testnet: send SUPRA only -- EMM/KAY/TEE will be auto-minted from 30% of your SUPRA)\n");
    }
    let funding = wait_for_funding(io, chain, &address, &network, std::time::Duration::from_secs(5)).await?;

    // Step 5
    let nft_id = match check_existing_nft(chain, &address).await? {
        ExistingNft::Found { nft_id, .. } => {
            io.print(&format!("Found existing TrusteeNFT #{}\n", nft_id));
            nft_id
        }
        ExistingNft::NotFound => {
            mint_nft_pair(io, chain, public.as_bytes(), &beneficiary).await?
        }
    };

    // Step 6: register with escrow
    let withdrawal = prompt_withdrawal_config(io)?;
    register_and_deposit(chain, nft_id, &withdrawal, &funding).await?;

    // Step 6b: mint tokens and deposit (testnet: auto-mint after registration)
    // Bootstrap nodes skip token minting entirely.
    let tokens = if bootstrap_only {
        io.print("  Bootstrap mode: skipping token minting and escrow deposit.\n");
        vec![]
    } else if network == Network::Testnet {
        auto_mint_and_deposit(io, chain, funding.supra, nft_id, &address).await?
    } else {
        funding.tokens.clone()
    };

    // Step 7: Profit config removed in DMKT11.
    // Agent handles profit distribution via burn_to_beneficiary.
    let profit = ProfitConfig {
        base_capital: HashMap::new(),
        threshold_pct: 0,
        transfer_mode: "all_excess".into(),
        transfer_mode_value: None,
    };

    // Step 8 (generate token silently for bootstrap — node config expects it)
    let auth_token = if bootstrap_only {
        generate_strategy_auth_token()
    } else {
        wizard_generate_auth_token(io)
    };

    // Step 9
    let bootstrap_peers = wizard_bootstrap_peers(io, &network);

    // Build config
    let contracts = default_contract_addresses(&network);
    let chain_id = match &network { Network::Testnet => 6u8, Network::Mainnet => 8u8 };
    let config = NodeConfig {
        network,
        rpc_urls: vec!["https://rpc-testnet.supra.com".into()],
        nft_id,
        trustee_address: address,
        beneficiary_address: beneficiary,
        sponsor_address: String::new(),
        contracts,
        bootstrap_peers,
        strategy_auth_token: auth_token,
        markets: vec!["EMM/KAY".into(), "KAY/TEE".into(), "TEE/EMM".into()],
        token_decimals: HashMap::new(),
        profit_taking: profit,
        withdrawal_rules: withdrawal,
        strategy_port: 9090,
        gossip_port: 9191,
        chain_id,
        max_gas_amount: 5000,
        gas_unit_price: 100000,
        created_at: format!("{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()),
    };

    config.save(&config_path)?;
    io.print("Configuration saved. Setup complete!\n");

    Ok(config)
}

// =========================================================================
// Helpers
// =========================================================================

fn format_with_commas(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use deadmkt_config::is_first_boot;
    use std::collections::VecDeque;

    // =====================================================================
    // MockIO
    // =====================================================================

    struct MockIO {
        inputs: VecDeque<String>,
        output: String,
        aborted: bool,
    }

    impl MockIO {
        fn new() -> Self {
            Self {
                inputs: VecDeque::new(),
                output: String::new(),
                aborted: false,
            }
        }

        fn queue_input(&mut self, s: &str) {
            self.inputs.push_back(s.to_string());
        }

        fn queue_abort(&mut self) {
            self.aborted = true;
        }

        fn output_contains(&self, s: &str) -> bool {
            self.output.contains(s)
        }
    }

    impl WizardIO for MockIO {
        fn print(&mut self, msg: &str) {
            self.output.push_str(msg);
        }

        fn read_line(&mut self) -> Result<String, SetupError> {
            if self.aborted {
                return Err(SetupError::Aborted);
            }
            self.inputs
                .pop_front()
                .ok_or_else(|| SetupError::IoError("no more mock input".into()))
        }

        fn is_aborted(&self) -> bool {
            self.aborted
        }
    }

    // =====================================================================
    // MockChainClient
    // =====================================================================

    struct MockChainClient {
        balance_responses: std::sync::Mutex<VecDeque<WalletBalance>>,
        is_trustee_val: bool,
        nft_id_val: u64,
        beneficiary_val: String,
        nft_config: NftConfigInfo,
        mint_result: TxResultInfo,
        total_minted: u64,
        register_result: TxResultInfo,
    }

    impl MockChainClient {
        fn default_success() -> Self {
            Self {
                balance_responses: std::sync::Mutex::new(VecDeque::new()),
                is_trustee_val: false,
                nft_id_val: 0,
                beneficiary_val: String::new(),
                nft_config: NftConfigInfo {
                    bond_amount: 1_000_000_000_000,
                    bond_lock_seconds: 2_592_000,
                    admin: "0xADMIN".into(),
                },
                mint_result: TxResultInfo {
                    success: true,
                    gas_used: 500,
                    vm_status: "ok".into(),
                },
                total_minted: 42,
                register_result: TxResultInfo {
                    success: true,
                    gas_used: 800,
                    vm_status: "ok".into(),
                },
            }
        }
    }

    impl ChainClient for MockChainClient {
        fn get_all_balances(&self, _addr: &str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<WalletBalance, SetupError>> + Send + '_>>
        {
            Box::pin(async move {
                let mut q = self.balance_responses.lock().unwrap();
                Ok(q.pop_front().unwrap_or(WalletBalance { supra: 0, tokens: vec![] }))
            })
        }

        fn is_trustee(&self, _addr: &str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, SetupError>> + Send + '_>>
        {
            let val = self.is_trustee_val;
            Box::pin(async move { Ok(val) })
        }

        fn get_nft_id(&self, _addr: &str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64, SetupError>> + Send + '_>>
        {
            let val = self.nft_id_val;
            Box::pin(async move { Ok(val) })
        }

        fn get_beneficiary(&self, _nft_id: u64)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, SetupError>> + Send + '_>>
        {
            let val = self.beneficiary_val.clone();
            Box::pin(async move { Ok(val) })
        }

        fn get_nft_config(&self)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<NftConfigInfo, SetupError>> + Send + '_>>
        {
            let val = self.nft_config.clone();
            Box::pin(async move { Ok(val) })
        }

        fn submit_mint(&self, _pubkey: &[u8], _beneficiary: &str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>
        {
            let val = self.mint_result.clone();
            Box::pin(async move { Ok(val) })
        }

        fn get_total_minted(&self)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64, SetupError>> + Send + '_>>
        {
            let val = self.total_minted;
            Box::pin(async move { Ok(val) })
        }

        fn submit_register(&self, _nft_id: u64, _holding_period_days: u64, _rushed_withdrawal_enabled: bool)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>
        {
            let val = self.register_result.clone();
            Box::pin(async move { Ok(val) })
        }

        fn submit_deposit(&self, _metadata_address: &str, _amount: u64)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>
        {
            Box::pin(async { Ok(TxResultInfo { success: true, gas_used: 200, vm_status: "ok".into() }) })
        }

        fn submit_request_mint(&self, _m: u64, _k: u64, _t: u64)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>
        {
            Box::pin(async { Ok(TxResultInfo { success: true, gas_used: 400, vm_status: "ok".into() }) })
        }

        fn submit_claim_mint(&self)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>
        {
            Box::pin(async { Ok(TxResultInfo { success: true, gas_used: 500, vm_status: "ok".into() }) })
        }

        fn get_trippples_metadata(&self, symbol: u8)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, SetupError>> + Send + '_>>
        {
            let addr = match symbol {
                0 => "0xEMM_METADATA_MOCK".to_string(),
                1 => "0xKAY_METADATA_MOCK".to_string(),
                2 => "0xTEE_METADATA_MOCK".to_string(),
                _ => "0xUNKNOWN".to_string(),
            };
            Box::pin(async move { Ok(addr) })
        }


        fn submit_lock_from_escrow(&self, _symbol: u8, _amount: u64, _duration: u64)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>
        {
            Box::pin(async { Ok(TxResultInfo { success: true, gas_used: 350, vm_status: "ok".into() }) })
        }

        fn submit_unlock_to_escrow(&self, _lock_index: u64)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>
        {
            Box::pin(async { Ok(TxResultInfo { success: true, gas_used: 300, vm_status: "ok".into() }) })
        }
    }

    // =====================================================================
    // Tests
    // =====================================================================

    // T_SETUP_01
    #[test]
    fn test_network_selection() {
        let mut io = MockIO::new();
        let network = prompt_network(&mut io).unwrap();
        assert_eq!(network, Network::Testnet);
        assert!(io.output_contains("Testnet"));
    }

    // T_SETUP_02
    #[test]
    fn test_beneficiary_address_explicit() {
        let mut io = MockIO::new();
        io.queue_input("0xC1234BENEFICIARY");
        let beneficiary = prompt_beneficiary(&mut io).unwrap();
        assert_eq!(beneficiary, "0xC1234BENEFICIARY");
        assert!(io.output_contains("beneficiary"));
    }

    // T_SETUP_02b
    #[test]
    fn test_beneficiary_address_required() {
        let mut io = MockIO::new();
        io.queue_input("");                       // empty - rejected
        io.queue_input("0xVALIDADDRESS1234");    // valid
        let beneficiary = prompt_beneficiary(&mut io).unwrap();
        assert_eq!(beneficiary, "0xVALIDADDRESS1234");
        assert!(io.output_contains("required"));
    }

    // T_SETUP_03
    #[test]
    fn test_keypair_generation_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let keystore_path = dir.path().join("keystore.json");

        let mut io = MockIO::new();
        io.queue_input("test-password");
        io.queue_input("test-password");

        let (address, public, _secret) = wizard_generate_keypair(&mut io, &keystore_path).unwrap();

        assert!(keystore_path.exists());
        assert!(!address.is_empty());
        assert_eq!(public.as_bytes().len(), 32);
        assert!(io.output_contains("Generating Ed25519 keypair"));
        assert!(io.output_contains("Trustee wallet"));
    }

    #[test]
    fn test_keypair_password_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let keystore_path = dir.path().join("keystore.json");

        let mut io = MockIO::new();
        io.queue_input("password1");
        io.queue_input("password2"); // mismatch
        io.queue_input("password3");
        io.queue_input("password3"); // match

        let result = wizard_generate_keypair(&mut io, &keystore_path);
        assert!(result.is_ok());
        assert!(io.output_contains("Passwords don't match"));
    }

    // T_SETUP_04
    #[tokio::test]
    async fn test_funding_wait_testnet_auto_mint() {
        let mock = MockChainClient::default_success();
        {
            let mut q = mock.balance_responses.lock().unwrap();
            q.push_back(WalletBalance { supra: 0, tokens: vec![] });
            q.push_back(WalletBalance { supra: 0, tokens: vec![] });
            // Testnet: only SUPRA needed, tokens auto-minted
            q.push_back(WalletBalance { supra: 50_000_000_000, tokens: vec![] });
        }

        let mut io = MockIO::new();
        let result =
            wait_for_funding(&mut io, &mock, "0xTRUSTEE", &Network::Testnet, std::time::Duration::from_millis(10)).await;
        assert!(result.is_ok());
        let balance = result.unwrap();
        // Testnet: wait_for_funding returns SUPRA balance only.
        // Minting happens later in auto_mint_and_deposit.
        assert_eq!(balance.supra, 50_000_000_000);
        assert_eq!(balance.tokens.len(), 0);
        assert!(io.output_contains("SUPRA received"));
    }

    // T_SETUP_05
    #[tokio::test]
    async fn test_existing_nft_detection() {
        let mut mock = MockChainClient::default_success();
        mock.is_trustee_val = true;
        mock.nft_id_val = 42;
        mock.beneficiary_val = "0xEXISTING_BENEF".into();

        let result = check_existing_nft(&mock, "0xTRUSTEE").await.unwrap();
        assert_eq!(
            result,
            ExistingNft::Found {
                nft_id: 42,
                beneficiary: "0xEXISTING_BENEF".into()
            }
        );
    }

    #[tokio::test]
    async fn test_no_existing_nft() {
        let mock = MockChainClient::default_success();
        let result = check_existing_nft(&mock, "0xTRUSTEE").await.unwrap();
        assert_eq!(result, ExistingNft::NotFound);
    }

    // T_SETUP_06
    #[tokio::test]
    async fn test_nft_mint_flow() {
        let mut io = MockIO::new();
        io.queue_input("y");

        let mock = MockChainClient::default_success();
        let pubkey = [1u8; 32];

        let result = mint_nft_pair(&mut io, &mock, &pubkey, "0xBENEFICIARY").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
        assert!(io.output_contains("Bond required"));
        assert!(io.output_contains("10,000"));
        assert!(io.output_contains("refundable after 30 days"));
        assert!(io.output_contains("TrusteeNFT #42"));
    }

    // T_SETUP_06b
    #[tokio::test]
    async fn test_nft_mint_declined() {
        let mut io = MockIO::new();
        io.queue_input("n");

        let mock = MockChainClient::default_success();

        let result = mint_nft_pair(&mut io, &mock, &[1u8; 32], "0xBENEF").await;
        assert!(matches!(result, Err(SetupError::UserDeclined)));
    }

    // T_SETUP_07
    #[test]
    fn test_withdrawal_config_prompts() {
        let mut io = MockIO::new();
        io.queue_input(""); // default 90
        io.queue_input("y");

        let config = prompt_withdrawal_config(&mut io).unwrap();
        assert_eq!(config.holding_period_days, 90);
        assert!(config.rushed_withdrawal_enabled);
        assert!(io.output_contains("holding_period_days"));
        assert!(io.output_contains("1-367"));
    }

    #[test]
    fn test_withdrawal_config_custom() {
        let mut io = MockIO::new();
        io.queue_input("30");
        io.queue_input("n");

        let config = prompt_withdrawal_config(&mut io).unwrap();
        assert_eq!(config.holding_period_days, 30);
        assert!(!config.rushed_withdrawal_enabled);
    }

    #[test]
    fn test_withdrawal_config_validation() {
        let mut io = MockIO::new();
        io.queue_input("0");   // invalid
        io.queue_input("400"); // invalid
        io.queue_input("90");  // valid
        io.queue_input("y");

        let config = prompt_withdrawal_config(&mut io).unwrap();
        assert_eq!(config.holding_period_days, 90);
    }

    // T_SETUP_08
    #[tokio::test]
    async fn test_register_and_deposit() {
        let mock = MockChainClient::default_success();
        let funding = WalletBalance {
            supra: 5_000_000_000,
            tokens: vec![
                TokenBalance { symbol: "EMM".into(), amount: 150_000_000, metadata_address: "0xEMM_METADATA_MOCK".into() },
                TokenBalance { symbol: "KAY".into(), amount: 150_000_000, metadata_address: "0xKAY_METADATA_MOCK".into() },
                TokenBalance { symbol: "TEE".into(), amount: 150_000_000, metadata_address: "0xTEE_METADATA_MOCK".into() },
            ],
        };

        let result = register_and_deposit(&mock, 42, &WithdrawalConfig {
            holding_period_days: 90,
            rushed_withdrawal_enabled: true,
        }, &funding).await;
        assert!(result.is_ok());
    }

    // T_SETUP_09
    #[test]
    fn test_profit_config_defaults() {
        let mut io = MockIO::new();
        io.queue_input(""); // threshold default 20

        let deposits = vec![
            TokenBalance { symbol: "EMM".into(), amount: 150_000_000, metadata_address: "0xEMM_METADATA_MOCK".into() },
            TokenBalance { symbol: "KAY".into(), amount: 150_000_000, metadata_address: "0xKAY_METADATA_MOCK".into() },
            TokenBalance { symbol: "TEE".into(), amount: 150_000_000, metadata_address: "0xTEE_METADATA_MOCK".into() },
        ];
        let config = prompt_profit_config(&mut io, &deposits).unwrap();

        assert_eq!(config.threshold_pct, 20);
        assert_eq!(config.transfer_mode, "all_excess");
        assert_eq!(*config.base_capital.get("EMM").unwrap(), 150_000_000u64);
        assert_eq!(*config.base_capital.get("KAY").unwrap(), 150_000_000u64);
        assert_eq!(*config.base_capital.get("TEE").unwrap(), 150_000_000u64);
    }

    #[test]
    fn test_profit_config_custom_threshold() {
        let mut io = MockIO::new();
        io.queue_input("10");

        let deposits = vec![TokenBalance { symbol: "TEE".into(), amount: 150_000_000, metadata_address: "0xTEE_METADATA_MOCK".into() }];
        let config = prompt_profit_config(&mut io, &deposits).unwrap();

        assert_eq!(config.threshold_pct, 10);
        assert_eq!(config.transfer_mode, "all_excess");
        assert_eq!(config.transfer_mode_value, None);
    }

    #[test]
    fn test_profit_config_skip() {
        let mut io = MockIO::new();
        io.queue_input("");

        let config = prompt_profit_config(&mut io, &[]).unwrap();
        assert_eq!(config.threshold_pct, 0);
    }

    // T_SETUP_10
    #[test]
    fn test_strategy_auth_token_generation() {
        let mut io = MockIO::new();
        let token = wizard_generate_auth_token(&mut io);
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // T_SETUP_11
    #[test]
    fn test_p2p_bootstrap_placeholder() {
        let mut io = MockIO::new();
        let peers = wizard_bootstrap_peers(&mut io, &Network::Testnet);
        assert!(!peers.is_empty());
        assert!(peers[0].contains("deadmkt.com"));
    }

    // T_SETUP_12
    #[test]
    fn test_strategy_detection_placeholder() {
        let config = NodeConfig::new(
            Network::Testnet, 42,
            "0xTRUSTEE".into(), "0xBENEF".into(), String::new(),
            vec!["EMM/KAY".into(), "KAY/TEE".into(), "TEE/EMM".into()],
        );
        assert!(!config.strategy_auth_token.is_empty());
    }

    // T_SETUP_13
    #[tokio::test]
    async fn test_wizard_produces_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");

        let mut io = MockIO::new();
        io.queue_input("");                       // press enter to begin
        io.queue_input("0xBENEFICIARY1234");      // beneficiary
        io.queue_input("1");                      // node role: trading
        io.queue_input("test-pass");              // password
        io.queue_input("test-pass");              // confirm
        io.queue_input("y");                      // mint bond confirm
        io.queue_input("");                       // holding period default
        io.queue_input("y");                      // rushed
        io.queue_input("");                       // profit threshold default

        let mut mock = MockChainClient::default_success();
        mock.is_trustee_val = false;
        {
            let mut q = mock.balance_responses.lock().unwrap();
            // Testnet: just SUPRA needed. Trippples tokens auto-minted.
            q.push_back(WalletBalance {
                supra: 50_000_000_000,
                tokens: vec![],
            });
        }

        let result = run_wizard(&mut io, &mock, dir.path()).await;
        assert!(result.is_ok());

        assert!(config_path.exists());
        let config = NodeConfig::load(&config_path).unwrap();
        assert_eq!(config.network, Network::Testnet);
        assert_eq!(config.nft_id, 42);
        assert!(!config.trustee_address.is_empty());
        assert_eq!(config.beneficiary_address, "0xBENEFICIARY1234");
        assert!(config.sponsor_address.is_empty());
        assert_eq!(config.withdrawal_rules.holding_period_days, 90);
        assert!(config.withdrawal_rules.rushed_withdrawal_enabled);
    }

    // T_SETUP_13b
    #[tokio::test]
    async fn test_wizard_bootstrap_only_skips_minting() {
        let dir = tempfile::tempdir().unwrap();

        let mut io = MockIO::new();
        io.queue_input("");                       // press enter to begin
        io.queue_input("0xBENEFICIARY1234");      // beneficiary
        io.queue_input("2");                      // node role: bootstrap/relay
        io.queue_input("test-pass");              // password
        io.queue_input("test-pass");              // confirm
        io.queue_input("y");                      // mint bond confirm
        io.queue_input("");                       // holding period default
        io.queue_input("y");                      // rushed

        let mut mock = MockChainClient::default_success();
        mock.is_trustee_val = false;
        {
            let mut q = mock.balance_responses.lock().unwrap();
            q.push_back(WalletBalance {
                supra: 50_000_000_000,
                tokens: vec![],
            });
        }

        let result = run_wizard(&mut io, &mock, dir.path()).await;
        assert!(result.is_ok());

        let config = result.unwrap();
        assert_eq!(config.profit_taking.threshold_pct, 0);
        assert!(config.profit_taking.base_capital.is_empty());
        // Auth token still generated (needed for config)
        assert!(!config.strategy_auth_token.is_empty());
    }

    // T_SETUP_14
    #[test]
    fn test_subsequent_boot_skips_wizard() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let keystore_path = dir.path().join("keystore.json");

        let config = NodeConfig::new(
            Network::Testnet, 42,
            "0xTRUSTEE".into(), "0xBENEF".into(), String::new(),
            vec!["EMM/KAY".into(), "KAY/TEE".into(), "TEE/EMM".into()],
        );
        config.save(&config_path).unwrap();

        let (secret, public) = deadmkt_crypto::generate_keypair();
        save_keystore(&keystore_path, &secret, &public, "password").unwrap();

        assert!(!is_first_boot(&config_path, &keystore_path));
        let loaded = NodeConfig::load(&config_path).unwrap();
        assert_eq!(loaded.nft_id, config.nft_id);
    }

    // T_SETUP_15
    #[tokio::test]
    async fn test_wizard_abort_no_partial_state() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let _keystore_path = dir.path().join("keystore.json");

        let mut io = MockIO::new();
        io.queue_input("");   // press enter to begin
        io.queue_input("1"); // network
        io.queue_abort();     // abort during beneficiary

        let mock = MockChainClient::default_success();
        let result = run_wizard(&mut io, &mock, dir.path()).await;
        assert!(result.is_err());

        // Config should not exist; keystore may or may not depending on abort point
        assert!(!config_path.exists());
    }

    // Extra: format_with_commas
    #[test]
    fn test_format_with_commas() {
        assert_eq!(format_with_commas(0), "0");
        assert_eq!(format_with_commas(999), "999");
        assert_eq!(format_with_commas(1000), "1,000");
        assert_eq!(format_with_commas(10000), "10,000");
        assert_eq!(format_with_commas(1000000), "1,000,000");
    }
}
