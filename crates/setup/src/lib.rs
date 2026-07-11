// deadmkt-setup: First boot wizard.
// Interactive 10-step setup: network, payout, keygen, funding,
// NFT mint/import, withdrawal config, register+deposit, profit config,
// auth token, bootstrap peers.

// MR1a: non-interactive setup schema + validation + structured JSON
// output. See `noninteractive::run_setup_noninteractive_fresh` for the
// fresh-setup entry point and `noninteractive::SetupConfig` for the
// JSON schema. MR1b/c/d build on this foundation.
pub mod noninteractive;

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

    // ---------------------------------------------------------------------
    // MR7: typed prompts + spinners. The interactive `RealWizardIO`
    // implements these via dialoguer/indicatif; `SilentIO` reads from
    // its already-validated `SetupConfig`; `MockWizardIO` (tests) pops
    // from per-method queues.
    //
    // Free-form `print` + `read_line` stay -- they cover the wizard's
    // section headers / step prints that don't fit a typed prompt.
    // ---------------------------------------------------------------------

    /// Styled section / info line. Default delegates to `print`.
    fn print_info(&mut self, msg: &str)    { self.print(msg); self.print("\n"); }
    /// Styled "success" line. Default delegates to `print`.
    fn print_success(&mut self, msg: &str) { self.print(msg); self.print("\n"); }
    /// Styled "warning" line. Default delegates to `print`.
    fn print_warning(&mut self, msg: &str) { self.print(msg); self.print("\n"); }

    /// Free-text input with optional default + optional validator.
    /// Validator returns `Ok(())` to accept or `Err(message)` to show
    /// the message and re-prompt. Default impl falls back to a
    /// print + read_line loop so legacy mocks keep working.
    fn prompt_input(
        &mut self,
        prompt: &str,
        default: Option<&str>,
        validate: Option<&dyn Fn(&str) -> Result<(), String>>,
    ) -> Result<String, SetupError> {
        loop {
            if let Some(d) = default {
                self.print(&format!("{} [{}]: ", prompt, d));
            } else {
                self.print(&format!("{}: ", prompt));
            }
            let raw = self.read_line()?;
            let trimmed = raw.trim();
            let value = if trimmed.is_empty() {
                default.unwrap_or("").to_string()
            } else {
                trimmed.to_string()
            };
            match validate {
                Some(f) => match f(&value) {
                    Ok(()) => return Ok(value),
                    Err(e) => { self.print(&format!("  {}\n", e)); }
                },
                None => return Ok(value),
            }
        }
    }

    /// Password input. `with_confirm=true` asks twice and re-prompts
    /// on mismatch. Default impl uses `read_line` (echoed); the real
    /// interactive impl uses dialoguer's no-echo Password.
    fn prompt_password(
        &mut self,
        prompt: &str,
        with_confirm: bool,
    ) -> Result<String, SetupError> {
        loop {
            self.print(&format!("{}: ", prompt));
            let first = self.read_line()?;
            let first = first.trim_end_matches(|c: char| c == '\n' || c == '\r').to_string();
            if !with_confirm {
                return Ok(first);
            }
            self.print(&format!("Confirm {}: ", prompt));
            let second = self.read_line()?;
            let second = second.trim_end_matches(|c: char| c == '\n' || c == '\r').to_string();
            if first == second {
                return Ok(first);
            }
            self.print("  passwords did not match; try again\n");
        }
    }

    /// Single-choice menu. Returns the chosen index. Default impl
    /// prints items with numbers and re-prompts until valid.
    fn prompt_select(
        &mut self,
        prompt: &str,
        items: &[&str],
        default_idx: usize,
    ) -> Result<usize, SetupError> {
        loop {
            self.print(&format!("{}\n", prompt));
            for (i, item) in items.iter().enumerate() {
                let marker = if i == default_idx { ">" } else { " " };
                self.print(&format!("  {}{}. {}\n", marker, i + 1, item));
            }
            self.print(&format!("Choice [1-{}, default {}]: ", items.len(), default_idx + 1));
            let raw = self.read_line()?;
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(default_idx);
            }
            if let Ok(n) = trimmed.parse::<usize>() {
                if n >= 1 && n <= items.len() {
                    return Ok(n - 1);
                }
            }
            self.print("  invalid choice; try again\n");
        }
    }

    /// Yes/no confirmation. Enter accepts the default.
    fn prompt_confirm(
        &mut self,
        prompt: &str,
        default_yes: bool,
    ) -> Result<bool, SetupError> {
        let hint = if default_yes { "Y/n" } else { "y/N" };
        loop {
            self.print(&format!("{} [{}]: ", prompt, hint));
            let raw = self.read_line()?;
            match raw.trim().to_lowercase().as_str() {
                "" => return Ok(default_yes),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => self.print("  please answer y or n\n"),
            }
        }
    }

    /// Start a spinner for a slow op. Caller drops the handle (or
    /// calls finish_*) when the op completes. Default impl prints
    /// "label..." once and the handle's drop prints "  done".
    fn spinner(&mut self, label: &str) -> SpinnerHandle {
        self.print(&format!("{}... ", label));
        SpinnerHandle::noop()
    }
}

// =========================================================================
// MR7: spinner handle
// =========================================================================

/// RAII handle for a slow-op spinner. Concrete `RealWizardIO::spinner`
/// hands back one with `bar: Some(ProgressBar)`; other impls hand
/// back `bar: None` and the drop is silent.
///
/// Inner option lets `finish_*` methods .take() the bar out before
/// Drop runs, working around Rust's "no moving out of Drop types"
/// rule cleanly.
pub struct SpinnerHandle {
    bar: Option<indicatif::ProgressBar>,
}

impl SpinnerHandle {
    pub fn real(bar: indicatif::ProgressBar) -> Self { SpinnerHandle { bar: Some(bar) } }
    pub fn noop() -> Self { SpinnerHandle { bar: None } }

    /// Update the spinner's message in place.
    pub fn set_message(&self, msg: &str) {
        if let Some(bar) = &self.bar {
            bar.set_message(msg.to_string());
        }
    }

    /// Replace the spinner with a final line of text + stop animating.
    pub fn finish_with_message(mut self, msg: &str) {
        if let Some(bar) = self.bar.take() {
            bar.finish_with_message(msg.to_string());
        } else {
            println!("  {}", msg);
        }
    }

    /// Stop animating and erase the spinner line.
    pub fn finish_and_clear(mut self) {
        if let Some(bar) = self.bar.take() {
            bar.finish_and_clear();
        }
    }
}

impl Drop for SpinnerHandle {
    fn drop(&mut self) {
        if let Some(bar) = self.bar.take() {
            // Idempotent if the user already called finish_*.
            bar.finish_and_clear();
        }
    }
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

/// Burn-exit revision: bond fields removed. The contract now exposes
/// `burn_cooldown_seconds` (R51) and `admin`. Mint fee + decay constants
/// live in ops_treasury and are fetched via separate getters.
#[derive(Debug, Clone)]
pub struct NftConfigInfo {
    pub burn_cooldown_seconds: u64,
    pub admin: String,
}

#[derive(Debug, Clone, Default)]
pub struct TxResultInfo {
    pub success: bool,
    pub gas_used: u64,
    pub vm_status: String,
    /// MR3: on-chain transaction hash for callers that need to surface
    /// it (action commands, indexer integration). Empty string when the
    /// submission path didn't observe a hash (e.g. mock test client).
    pub tx_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistingNft {
    Found { nft_id: u64 },
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

    fn get_nft_config(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<NftConfigInfo, SetupError>> + Send + '_>>;

    /// DMKT14: nft::mint_trustee_nft(trustee, ed25519_pubkey, sponsor).
    /// The beneficiary arg is gone (no on-chain beneficiary anymore).
    fn submit_mint(
        &self,
        pubkey: &[u8],
        sponsor: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>;

    /// Burn-exit revision: fetch the current ops_treasury mint_fee
    /// (AOE5 calibration constant, default 1,000 SUPRA = 10^11 raw).
    fn get_mint_fee(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64, SetupError>> + Send + '_>>;

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

    /// Call tokens::burn_for_profit(burn_amount, recipient). Burns triples
    /// from escrow, returns SUPRA to `recipient` (the off-chain payout
    /// address). Profit distribution path. DMKT14: was burn_to_beneficiary.
    fn submit_burn_for_profit(
        &self,
        _amount: u64,
        _recipient: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_burn_for_profit not implemented".into())) })
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

    // -----------------------------------------------------------------
    // MR3: withdrawal entry functions (SUPRA-only exits)
    //
    // All six take nft_id implicitly via the configured signer. The
    // per-token wallet paths (escrow::execute_rushed_withdrawal,
    // escrow::claim_all) are NOT exposed -- DMKT13 contract item
    // C-NO-PT-WD removes them entirely. Tokens never pass through a
    // wallet; beneficiaries exit as SUPRA via tokens.move.
    // -----------------------------------------------------------------

    /// Call escrow::request_rushed_withdrawal(nft_id). Starts the rushed
    /// grace clock; after `rushed_grace_batches` elapses, the operator
    /// can call `rushed_withdrawal_as_supra` to actually exit.
    /// IMPORTANT: signer must be the beneficiary for the given nft_id.
    /// On a self-funded node trustee == beneficiary and the trustee
    /// keystore works directly; otherwise the contract returns
    /// E_NOT_BENEFICIARY and the CLI surfaces it in the JSON envelope.
    fn submit_request_rushed_withdrawal(
        &self,
        _nft_id: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_request_rushed_withdrawal not implemented".into())) })
    }

    /// Call escrow::cancel_rushed_withdrawal(nft_id). Backs out of a
    /// pending rushed request before grace expires.
    fn submit_cancel_rushed_withdrawal(
        &self,
        _nft_id: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_cancel_rushed_withdrawal not implemented".into())) })
    }

    /// Call tokens::rushed_withdrawal_as_supra(nft_id, recipient). Burns the
    /// max equal triple from escrow and returns SUPRA to `recipient` (the
    /// off-chain payout address). Requires a prior request_rushed_withdrawal
    /// + elapsed grace. DMKT14: gained the `recipient` arg.
    fn submit_rushed_withdrawal_as_supra(
        &self,
        _nft_id: u64,
        _recipient: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_rushed_withdrawal_as_supra not implemented".into())) })
    }

    /// Call escrow::start_holding_period(nft_id). Begins the end-of-life
    /// holding countdown. After `holding_period_days` elapses, the
    /// operator can call `claim_all_as_supra`.
    fn submit_start_holding_period(
        &self,
        _nft_id: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_start_holding_period not implemented".into())) })
    }

    /// Call escrow::cancel_holding_period(nft_id). Returns the node to
    /// active state from the holding-period countdown.
    fn submit_cancel_holding_period(
        &self,
        _nft_id: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_cancel_holding_period not implemented".into())) })
    }

    /// Call tokens::claim_all_as_supra(nft_id, recipient). Burns the max
    /// equal triple from escrow after holding period expires and returns
    /// SUPRA to `recipient` (the off-chain payout address). DMKT14: gained
    /// the `recipient` arg.
    fn submit_claim_all_as_supra(
        &self,
        _nft_id: u64,
        _recipient: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_claim_all_as_supra not implemented".into())) })
    }

    /// Call exits::burn_trustee_nft(nft_id, recipient). DMKT14: single
    /// trustee-signed burn-exit that replaces the old two-step
    /// request_burn_pair + execute_burn_pair. Refund SUPRA goes to
    /// `recipient` (the off-chain payout address).
    fn submit_burn_trustee_nft(
        &self,
        _nft_id: u64,
        _recipient: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async { Err(SetupError::ChainError("submit_burn_trustee_nft not implemented".into())) })
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

/// Step 2c: sponsor address (burn-exit revision). The sponsor is the
/// capital provider who funded the mint and receives the time-decayed
/// refund when the NFT is later burned via exits::execute_burn_pair.
///
/// `trustee_address` is the address that's about to mint; offering it as
/// the default supports the self-funded operator pattern (testnet + casual
/// mainnet). On mainnet, operators should prefer three distinct addresses
/// (sponsor / trustee / beneficiary) for cold/hot-key separation; the
/// downstream mint flow warns when sponsor == trustee.
pub fn prompt_sponsor(io: &mut dyn WizardIO, trustee_address: &str) -> Result<String, SetupError> {
    io.print_info(
        "Sponsor address (capital provider; receives the burn-exit refund).\n\
         For self-funded operators, this is typically your trustee address.\n\
         For mainnet best practice, use a separate cold wallet.",
    );
    let validator: &dyn Fn(&str) -> Result<(), String> = &|input: &str| {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            Err("Sponsor address is required.".to_string())
        } else if !trimmed.starts_with("0x") || trimmed.len() < 10 {
            Err("Invalid address. Must start with 0x and be at least 10 chars.".to_string())
        } else if trimmed == "0x0" || trimmed == "0x00" {
            Err("Sponsor cannot be the zero address.".to_string())
        } else {
            Ok(())
        }
    };
    io.prompt_input(
        "Sponsor address (0x..., default = trustee address)",
        Some(trustee_address),
        Some(validator),
    )
    .map(|s| s.trim().to_string())
}

/// Step 2: payout address (required). DMKT14: this is the off-chain
/// address where exit/profit/withdrawal SUPRA is sent (passed as the
/// `recipient` arg on every exit/withdraw/profit call). It replaces the
/// removed on-chain beneficiary NFT. MR7: uses dialoguer-backed validated
/// input on real terminals; legacy print+read_line elsewhere.
pub fn prompt_payout(io: &mut dyn WizardIO) -> Result<String, SetupError> {
    io.print_info("Supra blockchain payout address (receives profits + exit/withdrawal SUPRA).");
    io.print_info("Can be your own address if self-funded.");
    let validator: &dyn Fn(&str) -> Result<(), String> = &|input: &str| {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            Err("Payout address is required.".to_string())
        } else if !trimmed.starts_with("0x") || trimmed.len() < 10 {
            Err("Invalid address. Must start with 0x and be at least 10 chars.".to_string())
        } else {
            Ok(())
        }
    };
    io.prompt_input(
        "Payout address (0x...)",
        None,
        Some(validator),
    )
    .map(|s| s.trim().to_string())
}

/// Step 2b: node role selection. MR7: uses dialoguer Select on real
/// terminals (arrows + Enter); legacy numbered menu elsewhere.
/// Returns true if this is a bootstrap/relay-only node (no trading).
pub fn prompt_node_role(io: &mut dyn WizardIO) -> Result<bool, SetupError> {
    let items = [
        "Trading node (full setup -- NFT + tokens + escrow)",
        "Bootstrap/relay node (NFT only -- no trading)",
    ];
    let choice = io.prompt_select("Node role:", &items, 0)?;
    let bootstrap_only = choice == 1;
    if bootstrap_only {
        io.print_info("Bootstrap mode: will mint NFT only, no token minting or escrow deposit.");
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
        io.print_info("Existing keystore found.");
        // MR7: no-echo password input.
        let password = io.prompt_password("Keystore password", false)?;

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

    io.print_info("Generating Ed25519 keypair...");
    io.print_info("Password protects your trustee key on disk. Recommended: 12+ mixed alpha-numeric and special chars.");
    io.print_warning("There is NO recovery: lose this password = lose the NFT.");

    let (secret, public) = deadmkt_crypto::generate_keypair();

    let mut hasher = Sha3_256::new();
    hasher.update(public.as_bytes());
    hasher.update(&[0x00]); // Ed25519 scheme identifier
    let hash = hasher.finalize();
    let address = format!("0x{}", hex::encode(hash));

    // MR7: no-echo password input with built-in confirmation (asks twice,
    // re-prompts on mismatch via dialoguer).
    let password = io.prompt_password("Keystore password", true)?;
    save_keystore(keystore_path, &secret, &public, password.trim())?;
    io.print_success(&format!("Trustee wallet: {}", address));
    Ok((address, public, secret))
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
    // MR7: spinner during the funding wait. SilentIO / mock fall back
    // to print(label) once; real terminal shows an animated spinner.
    let spinner = io.spinner(&format!("Waiting for SUPRA funding to {}", address));

    // Wait for SUPRA (gas money)
    let supra_balance = loop {
        let balance = chain.get_all_balances(address).await?;
        if balance.supra > 0 {
            break balance.supra;
        }
        if !balance.tokens.is_empty() {
            break balance.supra;
        }
        spinner.set_message("Polling chain for SUPRA balance...");
        cancellable_sleep(poll_interval).await?;
    };

    // Testnet: just report SUPRA received, tokens will be minted after registration
    if *network == Network::Testnet {
        spinner.finish_with_message(&format!("SUPRA received ({} raw)", supra_balance));
        return Ok(WalletBalance {
            supra: supra_balance,
            tokens: vec![],
        });
    }

    spinner.set_message("SUPRA received. Waiting for token funding (mainnet manual)...");

    // Mainnet: wait for manual token funding
    loop {
        let balance = chain.get_all_balances(address).await?;
        if balance.supra > 0 && !balance.tokens.is_empty() {
            spinner.finish_with_message("Funding complete");
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
    Ok(ExistingNft::Found { nft_id })
}

/// Step 5b: mint NFT pair
///
/// Burn-exit revision: the legacy refundable bond is gone. mint_pair now
/// withdraws `mint_fee` (AOE5 calibration constant from ops_treasury) and
/// stores a sponsor address that receives the time-decayed refund when the
/// NFT is later burned via exits::execute_burn_pair. See
/// planning/specs/individual-nft-burn-exit.md for the full design.
pub async fn mint_nft_pair(
    io: &mut dyn WizardIO,
    chain: &dyn ChainClient,
    pubkey: &[u8],
    sponsor: &str,
    trustee_address: &str,
) -> Result<u64, SetupError> {
    let mint_fee_supra = chain.get_mint_fee().await? / 100_000_000; // 8 decimals
    // rev5: max refund caps at 95% of mint_fee at day 0; decays from there.
    let max_refund_supra = mint_fee_supra * 95 / 100;
    io.print("\nMembership deposit:\n");
    io.print(&format!(
        "  {} SUPRA (refundable on burn-exit up to {} SUPRA at day 0,\n",
        format_with_commas(mint_fee_supra),
        format_with_commas(max_refund_supra),
    ));
    io.print("  then decaying 50 SUPRA per 30 days; floor at 0 after ~19 months.\n");
    io.print("  A day-0 mint-and-burn costs at least 5% of the deposit.)\n");
    io.print("\nSponsor address (receives the burn-exit refund):\n");
    io.print(&format!("  {}\n", sponsor));
    if sponsor == trustee_address {
        io.print_warning(
            "Sponsor address matches trustee. This is acceptable on testnet but on\n\
             mainnet you should use distinct addresses (sponsor / trustee) for\n\
             cold/hot-key separation.",
        );
    }
    let accepted = io.prompt_confirm(
        "Proceed with mint?",
        false,
    )?;
    if !accepted {
        return Err(SetupError::UserDeclined);
    }

    // MR7: spinner during the multi-second mint submission + tx wait.
    let spin = io.spinner("Submitting NFT mint_trustee_nft...");
    let result = chain.submit_mint(pubkey, sponsor).await?;
    spin.finish_with_message(if result.success { "NFT minted" } else { "NFT mint FAILED" });
    if !result.success {
        return Err(SetupError::ChainError(result.vm_status));
    }

    // #14 fix: use per-address lookup, NOT the racy global total_minted counter.
    // Concurrent wizards both read the same global and both claim the same nft_id,
    // causing the loser to fail register_trader with E_NFT_MISMATCH.
    let nft_id = chain.get_nft_id(trustee_address).await?;
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

    // MR7: typed input with validator + typed confirm.
    let validator: &dyn Fn(&str) -> Result<(), String> = &|input: &str| {
        let trimmed = input.trim();
        match trimmed.parse::<u32>() {
            Ok(d) if (1..=367).contains(&d) => Ok(()),
            _ => Err("Enter a number between 1 and 367.".to_string()),
        }
    };
    let days_str = io.prompt_input(
        "Path 1 holding_period_days (1-367)",
        Some("90"),
        Some(validator),
    )?;
    let days: u32 = days_str.trim().parse().unwrap_or(90);

    let rushed = io.prompt_confirm("Enable rushed withdrawal? (Path 2)", true)?;

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

    // MR7: typed numeric input with sensible default.
    let validator: &dyn Fn(&str) -> Result<(), String> = &|input: &str| {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(()); // accepted -> caller treats empty as "skip"
        }
        match trimmed.parse::<u32>() {
            Ok(n) if n <= 1000 => Ok(()),
            Ok(_) => Err("Threshold over 1000% is almost certainly a typo.".into()),
            Err(_) => Err("Enter a whole number (e.g. 20 for 20%).".into()),
        }
    };
    // Empty -> skip; otherwise typed value (validator accepts both).
    // Don't pass a default to prompt_input or empty input will resolve
    // to it, defeating the skip behaviour.
    let input = io.prompt_input(
        "Profit-taking threshold % (Enter to skip/disable, or e.g. 20)",
        None,
        Some(validator),
    )?;
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
    let payout = prompt_payout(io)?;

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
            // Burn-exit rev: ask for sponsor address right before mint.
            let sponsor = prompt_sponsor(io, &address)?;
            mint_nft_pair(io, chain, public.as_bytes(), &sponsor, &address).await?
        }
    };

    // Step 6: register with escrow
    let withdrawal = prompt_withdrawal_config(io)?;
    // MR7: spinner around the chain submit; register_and_deposit doesn't
    // take a WizardIO so wrap it here at the call site.
    let spin = io.spinner("Registering trader on-chain...");
    let reg_result = register_and_deposit(chain, nft_id, &withdrawal, &funding).await;
    match &reg_result {
        Ok(()) => spin.finish_with_message("Trader registered"),
        Err(_) => spin.finish_with_message("Trader registration FAILED"),
    }
    reg_result?;

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
    // Agent handles profit distribution via burn_for_profit.
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
        payout_address: payout,
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
                nft_id_val: 42,
                nft_config: NftConfigInfo {
                    burn_cooldown_seconds: 3600,
                    admin: "0xADMIN".into(),
                },
                mint_result: TxResultInfo {
                    success: true,
                    gas_used: 500,
                    vm_status: "ok".into(),
                    tx_hash: String::new(),
                },
                total_minted: 42,
                register_result: TxResultInfo {
                    success: true,
                    gas_used: 800,
                    vm_status: "ok".into(),
                    tx_hash: String::new(),
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

        fn get_nft_config(&self)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<NftConfigInfo, SetupError>> + Send + '_>>
        {
            let val = self.nft_config.clone();
            Box::pin(async move { Ok(val) })
        }

        fn submit_mint(&self, _pubkey: &[u8], _sponsor: &str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>
        {
            let val = self.mint_result.clone();
            Box::pin(async move { Ok(val) })
        }

        fn get_mint_fee(&self)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64, SetupError>> + Send + '_>>
        {
            Box::pin(async move { Ok(100_000_000_000) })  // 1,000 SUPRA default
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
            Box::pin(async { Ok(TxResultInfo { success: true, gas_used: 200, vm_status: "ok".into(), tx_hash: String::new() }) })
        }

        fn submit_request_mint(&self, _m: u64, _k: u64, _t: u64)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>
        {
            Box::pin(async { Ok(TxResultInfo { success: true, gas_used: 400, vm_status: "ok".into(), tx_hash: String::new() }) })
        }

        fn submit_claim_mint(&self)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>
        {
            Box::pin(async { Ok(TxResultInfo { success: true, gas_used: 500, vm_status: "ok".into(), tx_hash: String::new() }) })
        }

        // #19: override default (which returns 0). The wizard's first-mint
        // claim path polls get_escrow_balance up to 30 times at 3s intervals
        // waiting for tokens to appear. Returning 0 forever causes a 90s
        // timeout in tests. Simulate a successfully-funded escrow so the
        // wizard exits the polling loop on first check.
        fn get_escrow_balance(&self, _nft_id: u64, _metadata_address: &str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64, SetupError>> + Send + '_>>
        {
            Box::pin(async { Ok(100_000) }) // 1 token, satisfies e > 0 && k > 0 && t > 0
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
            Box::pin(async { Ok(TxResultInfo { success: true, gas_used: 350, vm_status: "ok".into(), tx_hash: String::new() }) })
        }

        fn submit_unlock_to_escrow(&self, _lock_index: u64)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>>
        {
            Box::pin(async { Ok(TxResultInfo { success: true, gas_used: 300, vm_status: "ok".into(), tx_hash: String::new() }) })
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
    fn test_payout_address_explicit() {
        let mut io = MockIO::new();
        io.queue_input("0xC1234PAYOUT");
        let payout = prompt_payout(&mut io).unwrap();
        assert_eq!(payout, "0xC1234PAYOUT");
        assert!(io.output_contains("payout"));
    }

    // T_SETUP_02b
    #[test]
    fn test_payout_address_required() {
        let mut io = MockIO::new();
        io.queue_input("");                       // empty - rejected
        io.queue_input("0xVALIDADDRESS1234");    // valid
        let payout = prompt_payout(&mut io).unwrap();
        assert_eq!(payout, "0xVALIDADDRESS1234");
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
        // MR7: prompt_password's default trait impl (used by MockIO since
        // dialoguer is only wired up in the real StdIO) re-prompts with
        // "passwords did not match" on mismatch.
        assert!(io.output_contains("passwords did not match"));
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
        // MR7: progress is now reported via the spinner (printed via
        // println! in the Noop case). The "Waiting for SUPRA funding"
        // label still goes through io.print on spinner creation, so we
        // can check that as the proxy for "spinner was started".
        assert!(io.output_contains("Waiting for SUPRA funding"));
    }

    // T_SETUP_05
    #[tokio::test]
    async fn test_existing_nft_detection() {
        let mut mock = MockChainClient::default_success();
        mock.is_trustee_val = true;
        mock.nft_id_val = 42;

        let result = check_existing_nft(&mock, "0xTRUSTEE").await.unwrap();
        assert_eq!(
            result,
            ExistingNft::Found {
                nft_id: 42,
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

        // DMKT14: mint_nft_pair drops beneficiary; args are
        // (pubkey, sponsor, trustee_address). Self-sponsored pattern:
        // sponsor == trustee_address.
        let result = mint_nft_pair(
            &mut io, &mock, &pubkey, "0xTRUSTEE", "0xTRUSTEE",
        ).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
        assert!(io.output_contains("Membership deposit"));
        assert!(io.output_contains("1,000"));  // default mint_fee (raw / 1e8)
        assert!(io.output_contains("TrusteeNFT #42"));
    }

    // T_SETUP_06b
    #[tokio::test]
    async fn test_nft_mint_declined() {
        let mut io = MockIO::new();
        io.queue_input("n");

        let mock = MockChainClient::default_success();

        let result = mint_nft_pair(
            &mut io, &mock, &[1u8; 32], "0xTRUSTEE", "0xTRUSTEE",
        ).await;
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
        io.queue_input("0xPAYOUT1234");           // payout address
        io.queue_input("1");                      // node role: trading
        io.queue_input("test-pass");              // password
        io.queue_input("test-pass");              // confirm
        io.queue_input("0xSPONSOR1234");          // sponsor address (burn-exit rev)
        io.queue_input("y");                      // mint confirm
        io.queue_input("");                       // holding period default
        io.queue_input("y");                      // rushed
        io.queue_input("");                       // profit threshold default

        // Reorder: insert sponsor input between password-confirm and the mint
        // proceed-confirm. We do this above before run_wizard runs (input
        // queue is FIFO).
        // The queued order above was:
        //   "", beneficiary, node_role, password, password,
        //   "y" (mint), "" (holding), "y" (rushed), "" (profit)
        // Burn-exit rev adds prompt_sponsor (1 input) before the mint
        // confirm. The simplest way to reflect that here is to put a
        // sponsor address in front of the "y" mint confirm.

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
        assert_eq!(config.payout_address, "0xPAYOUT1234");
        assert_eq!(config.withdrawal_rules.holding_period_days, 90);
        assert!(config.withdrawal_rules.rushed_withdrawal_enabled);
    }

    // T_SETUP_13b
    #[tokio::test]
    async fn test_wizard_bootstrap_only_skips_minting() {
        let dir = tempfile::tempdir().unwrap();

        let mut io = MockIO::new();
        io.queue_input("");                       // press enter to begin
        io.queue_input("0xPAYOUT1234");           // payout address
        io.queue_input("2");                      // node role: bootstrap/relay
        io.queue_input("test-pass");              // password
        io.queue_input("test-pass");              // confirm
        io.queue_input("0xSPONSOR1234");          // sponsor address (burn-exit rev)
        io.queue_input("y");                      // mint confirm
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
        io.queue_abort();     // abort during payout

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
