// =========================================================================
// setup_bridge.rs: Real WizardIO + ChainClient for live setup wizard
// =========================================================================

use deadmkt_chain::client::SupraClient;
use deadmkt_setup::{
    ChainClient, NftConfigInfo, SetupError, SpinnerHandle, TxResultInfo, WalletBalance, WizardIO,
};
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;
use serde_json::Value;
use std::io::{self, BufRead, Write};
use std::pin::Pin;
use std::future::Future;
use std::sync::Mutex;

// =========================================================================
// StdIO: Interactive terminal I/O
// =========================================================================

pub struct StdIO;

impl WizardIO for StdIO {
    fn print(&mut self, msg: &str) {
        print!("{}", msg);
        io::stdout().flush().ok();
    }

    fn read_line(&mut self) -> Result<String, SetupError> {
        let stdin = io::stdin();
        let mut line = String::new();
        stdin
            .lock()
            .read_line(&mut line)
            .map_err(|e| SetupError::IoError(e.to_string()))?;
        Ok(line.trim().to_string())
    }

    fn is_aborted(&self) -> bool {
        false
    }

    // ---------------------------------------------------------------------
    // MR7: dialoguer/indicatif-backed implementations. Auto-detect TTY
    // for color; spinners degrade to a plain line on piped stdout.
    // The default trait impls (print + read_line loops) handle the
    // SilentIO / test-mock paths -- StdIO overrides only the methods
    // where dialoguer gives a meaningfully better experience.
    // ---------------------------------------------------------------------

    fn print_info(&mut self, msg: &str) {
        use dialoguer::console::style;
        println!("{} {}", style("==>").cyan().bold(), msg);
    }

    fn print_success(&mut self, msg: &str) {
        use dialoguer::console::style;
        println!("{} {}", style("[ok]").green().bold(), msg);
    }

    fn print_warning(&mut self, msg: &str) {
        use dialoguer::console::style;
        eprintln!("{} {}", style("[warn]").yellow().bold(), msg);
    }

    fn prompt_input(
        &mut self,
        prompt: &str,
        default: Option<&str>,
        validate: Option<&dyn Fn(&str) -> Result<(), String>>,
    ) -> Result<String, SetupError> {
        use dialoguer::{theme::ColorfulTheme, Input};
        let theme = ColorfulTheme::default();
        let mut builder: Input<String> = Input::with_theme(&theme).with_prompt(prompt);
        if let Some(d) = default {
            builder = builder.default(d.to_string());
        }
        // dialoguer's validate_with takes a closure of its own. Bridge
        // through a wrapper that returns the operator-friendly message.
        let result = if let Some(f) = validate {
            builder
                .validate_with(|input: &String| -> Result<(), String> {
                    f(input.as_str())
                })
                .interact_text()
        } else {
            builder.interact_text()
        };
        result.map_err(|e| SetupError::IoError(format!("input prompt: {}", e)))
    }

    fn prompt_password(
        &mut self,
        prompt: &str,
        with_confirm: bool,
    ) -> Result<String, SetupError> {
        use dialoguer::{theme::ColorfulTheme, Password};
        let theme = ColorfulTheme::default();
        let mut builder = Password::with_theme(&theme).with_prompt(prompt);
        if with_confirm {
            builder = builder.with_confirmation(
                format!("Confirm {}", prompt),
                "passwords did not match",
            );
        }
        builder
            .interact()
            .map_err(|e| SetupError::IoError(format!("password prompt: {}", e)))
    }

    fn prompt_select(
        &mut self,
        prompt: &str,
        items: &[&str],
        default_idx: usize,
    ) -> Result<usize, SetupError> {
        use dialoguer::{theme::ColorfulTheme, Select};
        let theme = ColorfulTheme::default();
        Select::with_theme(&theme)
            .with_prompt(prompt)
            .items(items)
            .default(default_idx)
            .interact()
            .map_err(|e| SetupError::IoError(format!("select prompt: {}", e)))
    }

    fn prompt_confirm(
        &mut self,
        prompt: &str,
        default_yes: bool,
    ) -> Result<bool, SetupError> {
        use dialoguer::{theme::ColorfulTheme, Confirm};
        let theme = ColorfulTheme::default();
        Confirm::with_theme(&theme)
            .with_prompt(prompt)
            .default(default_yes)
            .interact()
            .map_err(|e| SetupError::IoError(format!("confirm prompt: {}", e)))
    }

    fn spinner(&mut self, label: &str) -> SpinnerHandle {
        use indicatif::{ProgressBar, ProgressStyle};
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        bar.set_message(label.to_string());
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        SpinnerHandle::real(bar)
    }
}

// =========================================================================
// BCS Transaction types (same as settlement crate)
// =========================================================================

type AccountAddress = [u8; 32];

#[derive(Debug, Clone, Serialize)]
struct ModuleId {
    address: AccountAddress,
    name: String,
}

#[derive(Debug, Clone, Serialize)]
struct EntryFunction {
    module: ModuleId,
    function: String,
    ty_args: Vec<TypeTagStub>,
    args: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize)]
struct TypeTagStub;

#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
enum TransactionPayload {
    Script,
    ModuleBundle,
    EntryFunction(EntryFunction),
}

#[derive(Debug, Clone, Serialize)]
struct RawTransaction {
    sender: AccountAddress,
    sequence_number: u64,
    payload: TransactionPayload,
    max_gas_amount: u64,
    gas_unit_price: u64,
    expiration_timestamp_secs: u64,
    chain_id: u8,
}

#[derive(Debug, Clone, Serialize)]
struct Ed25519Authenticator {
    public_key: Vec<u8>,
    signature: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
enum TransactionAuthenticator {
    Ed25519(Ed25519Authenticator),
}

#[derive(Debug, Clone, Serialize)]
struct SignedTransaction {
    raw_txn: RawTransaction,
    authenticator: TransactionAuthenticator,
}

/// SHA3-256("SUPRA::RawTransaction") prefix for signing.
fn signing_prefix() -> [u8; 32] {
    use sha3::{Sha3_256, Digest};
    let mut hasher = Sha3_256::new();
    hasher.update(b"SUPRA::RawTransaction");
    let result = hasher.finalize();
    let mut prefix = [0u8; 32];
    prefix.copy_from_slice(&result);
    prefix
}

fn parse_address(addr: &str) -> Result<AccountAddress, SetupError> {
    let hex_str = addr.strip_prefix("0x").unwrap_or(addr);
    let padded = if hex_str.len() % 2 != 0 {
        format!("0{}", hex_str)
    } else {
        hex_str.to_string()
    };
    let bytes = hex::decode(&padded)
        .map_err(|e| SetupError::ChainError(format!("invalid address hex: {}", e)))?;
    if bytes.len() > 32 {
        return Err(SetupError::ChainError(
            format!("address too long: {} bytes", bytes.len()),
        ));
    }
    let mut addr_bytes = [0u8; 32];
    let offset = 32 - bytes.len();
    addr_bytes[offset..].copy_from_slice(&bytes);
    Ok(addr_bytes)
}

// =========================================================================
// Signer state (set after keypair generation in step 3)
// =========================================================================

struct SignerState {
    secret_key: SigningKey,
    public_key: [u8; 32],
    address: AccountAddress,
}

// =========================================================================
// SupraSetupClient
// =========================================================================

pub struct SupraSetupClient {
    pub client: SupraClient,
    contract_addr: AccountAddress,
    signer: Mutex<Option<SignerState>>,
    chain_id: u8,
    max_gas_amount: u64,
    gas_unit_price: u64,
}

impl SupraSetupClient {
    /// Burn-exit (DMKT13): read-only preview of what exits::execute_burn_pair
    /// would pay for `nft_id` right now. Returns raw u64 SUPRA (8 decimals).
    /// Reflects bootstrap lockout, last-NFT carve-out, time-decay, pro-rata cap.
    pub async fn preview_burn_refund(&self, nft_id: u64) -> Result<u64, SetupError> {
        match self.client
            .view_raw(
                "exits",
                "preview_burn_refund",
                vec![],
                vec![Value::String(nft_id.to_string())],
            )
            .await
        {
            Ok(val) => {
                let raw = val.as_array()
                    .and_then(|arr| arr.first())
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| SetupError::ChainError(
                        "preview_burn_refund: unexpected view result shape".into(),
                    ))?;
                raw.parse::<u64>().map_err(|e| SetupError::ChainError(e.to_string()))
            }
            Err(e) => Err(SetupError::ChainError(e.to_string())),
        }
    }

    pub fn new(rpc_urls: Vec<String>, contract_addr: String) -> Self {
        let parsed = parse_address(&contract_addr).unwrap_or([0u8; 32]);
        // #12: env overrides apply at wizard time too (before config.json exists).
        // Lets operators respond to testnet gas drift without rebuilding the image.
        let max_gas_amount = std::env::var("DEADMKT_MAX_GAS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(5000);
        let gas_unit_price = std::env::var("DEADMKT_GAS_PRICE")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(100000);
        Self {
            client: SupraClient::new(rpc_urls, contract_addr),
            contract_addr: parsed,
            signer: Mutex::new(None),
            chain_id: 6,          // Supra testnet default
            max_gas_amount,
            gas_unit_price,
        }
    }

    /// Override gas and chain config from NodeConfig.
    pub fn set_gas_config(&mut self, chain_id: u8, max_gas: u64, gas_price: u64) {
        self.chain_id = chain_id;
        self.max_gas_amount = max_gas;
        self.gas_unit_price = gas_price;
    }

    fn parse_u64(val: &Value) -> u64 {
        val.as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| val.as_u64())
            .unwrap_or(0)
    }

    async fn get_sequence_number(&self, address: &str) -> Result<u64, SetupError> {
        let base = self.client.current_base();
        let url = format!("{}/rpc/v2/accounts/{}", base, address);

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| SetupError::ChainError(e.to_string()))?;

        let resp = http.get(&url).send().await
            .map_err(|e| SetupError::ChainError(e.to_string()))?;

        let json: Value = resp.json().await
            .map_err(|e| SetupError::ChainError(e.to_string()))?;

        let seq = json.get("sequence_number")
            .map(|v| Self::parse_u64(v))
            .unwrap_or(0);
        Ok(seq)
    }

    async fn submit_entry_function(
        &self,
        module: &str,
        function: &str,
        args: Vec<Vec<u8>>,
    ) -> Result<TxResultInfo, SetupError> {
        // Clone what we need from signer under lock, then drop lock
        let (secret_key_bytes, public_key, sender_addr, address_hex) = {
            let guard = self.signer.lock().unwrap();
            let signer = guard.as_ref()
                .ok_or_else(|| SetupError::ChainError("Signer not set".into()))?;
            (
                signer.secret_key.to_bytes(),
                signer.public_key,
                signer.address,
                format!("0x{}", hex::encode(signer.address)),
            )
        };

        let sk = SigningKey::from_bytes(&secret_key_bytes);
        let derived_pk = sk.verifying_key();
        if public_key != derived_pk.to_bytes() {
            eprintln!("!!! PUBLIC KEY MISMATCH — stored key != derived key !!!");
        }
        let seq = self.get_sequence_number(&address_hex).await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Build BCS RawTransaction for signing (same struct as before)
        let raw_tx = RawTransaction {
            sender: sender_addr,
            sequence_number: seq,
            payload: TransactionPayload::EntryFunction(EntryFunction {
                module: ModuleId {
                    address: self.contract_addr,
                    name: module.to_string(),
                },
                function: function.to_string(),
                ty_args: vec![],
                args: args.clone(),
            }),
            max_gas_amount: self.max_gas_amount,
            gas_unit_price: self.gas_unit_price,
            expiration_timestamp_secs: now + 60,
            chain_id: self.chain_id,
        };

        // BCS encode for signing
        let raw_tx_bytes = bcs::to_bytes(&raw_tx)
            .map_err(|e| SetupError::ChainError(format!("BCS error: {}", e)))?;

        let prefix = signing_prefix();
        let mut signing_msg = Vec::with_capacity(32 + raw_tx_bytes.len());
        signing_msg.extend_from_slice(&prefix);
        signing_msg.extend_from_slice(&raw_tx_bytes);

        
        // Verify address derivation
        use sha3::{Sha3_256, Digest};
        let mut addr_hasher = Sha3_256::new();
        addr_hasher.update(&public_key);
        addr_hasher.update(&[0x00]);
        let expected_addr = addr_hasher.finalize();
        if sender_addr != expected_addr.as_slice() {
            eprintln!("!!! ADDRESS MISMATCH — sender != SHA3(pubkey||0x00) !!!");
        }

        let signature = sk.sign(&signing_msg);

        let signed_tx = SignedTransaction {
            raw_txn: raw_tx,
            authenticator: TransactionAuthenticator::Ed25519(Ed25519Authenticator {
                public_key: public_key.to_vec(),
                signature: signature.to_bytes().to_vec(),
            }),
        };

        let signed_bytes = bcs::to_bytes(&signed_tx)
            .map_err(|e| SetupError::ChainError(format!("BCS error: {}", e)))?;
        // Wrap in SupraTransaction: ULEB128(1) for MOVE variant + signed_tx bytes
        let mut supra_tx_bytes = Vec::with_capacity(1 + signed_bytes.len());
        supra_tx_bytes.push(0x01); // MOVE variant = 1
        supra_tx_bytes.extend_from_slice(&signed_bytes);

        // Submit as BCS with Supra's custom content type
        let base = self.client.current_base();
        let url = format!("{}/rpc/v3/transactions/submit", base);

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| SetupError::ChainError(e.to_string()))?;

        let resp = http
            .post(&url)
            .header("Content-Type", "application/x.supra.signed_transaction+bcs")
            .body(supra_tx_bytes)
            .send()
            .await
            .map_err(|e| SetupError::ChainError(format!("submit failed: {}", e)))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| SetupError::ChainError(format!("response read error: {}", e)))?;
        // Try to parse as JSON
        let json: Value = match serde_json::from_str(&body) {
            Ok(j) => j,
            Err(_) => {
                // Response might be a plain string (tx hash) or error
                if status.is_success() {
                    let hash = body.trim().trim_matches('"');
                    return self.wait_for_tx(hash).await;
                }
                return Ok(TxResultInfo {
                    success: false,
                    gas_used: 0,
                    vm_status: format!("HTTP {} \u{2014} {}", status, &body[..body.len().min(200)]),
                    tx_hash: String::new(),
                });
            }
        };

        if let Some(status_str) = json.get("status").and_then(|v| v.as_str()) {
            let success = status_str == "Success";
            return Ok(TxResultInfo {
                success,
                gas_used: 0,
                vm_status: status_str.to_string(),
                tx_hash: String::new(),
            });
        }

        if let Some(hash) = json.get("hash").and_then(|v| v.as_str()) {
            self.wait_for_tx(hash).await
        } else if json.is_string() {
            // SDK returns just the tx hash as a JSON string
            let hash = json.as_str().unwrap();
            self.wait_for_tx(hash).await
        } else {
            let fallback = format!("{}", json);
            let msg = json.get("message")
                .or_else(|| json.get("error_code"))
                .and_then(|v| v.as_str())
                .unwrap_or(&fallback);
            Ok(TxResultInfo {
                success: false,
                gas_used: 0,
                vm_status: msg.to_string(),
                tx_hash: String::new(),
            })
        }
    }

    async fn wait_for_tx(&self, tx_hash: &str) -> Result<TxResultInfo, SetupError> {
        let base = self.client.current_base();
        // Strip 0x prefix if present — Supra API might not accept it
        let hash_clean = tx_hash.strip_prefix("0x").unwrap_or(tx_hash);
        let url = format!("{}/rpc/v3/transactions/{}?type=user", base, hash_clean);

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| SetupError::ChainError(e.to_string()))?;

        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            match http.get(&url).send().await {
                Ok(resp) => {
                    let body_text = resp.text().await.unwrap_or_default();
                    if let Ok(json) = serde_json::from_str::<Value>(&body_text) {
                        // Supra uses "status": "Success" or "Fail" (string)
                        if let Some(status_str) = json.get("status").and_then(|v| v.as_str()) {
                            // "Pending" means not committed yet — keep polling
                            if status_str != "Pending" {
                                return Ok(TxResultInfo {
                                    success: status_str == "Success",
                                    gas_used: 0,
                                    vm_status: json.get("output")
                                        .and_then(|o| o.get("Move"))
                                        .and_then(|m| m.get("vm_status"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(status_str)
                                        .to_string(),
                                    tx_hash: tx_hash.to_string(),
                                });
                            }
                        }
                        // Also handle Aptos-style "success" bool as fallback
                        if let Some(success) = json.get("success").and_then(|v| v.as_bool()) {
                            return Ok(TxResultInfo {
                                success,
                                gas_used: json.get("gas_used")
                                    .map(|v| Self::parse_u64(v))
                                    .unwrap_or(0),
                                vm_status: json.get("vm_status")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                tx_hash: tx_hash.to_string(),
                            });
                        }
                    }
                }
                Err(_) => {}
            }
        }

        Err(SetupError::ChainError("Transaction confirmation timeout (30s)".into()))
    }
}

// =========================================================================
// ChainClient implementation
// =========================================================================

impl ChainClient for SupraSetupClient {
    fn set_signer(&self, secret_key: &[u8; 32], public_key: &[u8; 32], address: &str) {
        let sk = SigningKey::from_bytes(secret_key);
        let addr = parse_address(address).unwrap_or([0u8; 32]);
        let mut guard = self.signer.lock().unwrap();
        *guard = Some(SignerState {
            secret_key: sk,
            public_key: *public_key,
            address: addr,
        });
    }

    fn get_all_balances(
        &self,
        address: &str,
    ) -> Pin<Box<dyn Future<Output = Result<WalletBalance, SetupError>> + Send + '_>> {
        let addr = address.to_string();
        Box::pin(async move {
            let base = self.client.current_base();
            let url = format!("{}/rpc/v2/view", base);

            let body = serde_json::json!({
                "function": "0x1::coin::balance",
                "type_arguments": ["0x1::supra_coin::SupraCoin"],
                "arguments": [addr]
            });

            let http = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
            {
                Ok(c) => c,
                Err(_) => return Ok(WalletBalance { supra: 0, tokens: vec![] }),
            };

            let resp = match http.post(&url).json(&body).send().await {
                Ok(r) => r,
                Err(_) => return Ok(WalletBalance { supra: 0, tokens: vec![] }),
            };

            if !resp.status().is_success() {
                // 500 = account doesn't exist yet (no CoinStore), keep polling
                return Ok(WalletBalance { supra: 0, tokens: vec![] });
            }

            let json: Value = match resp.json().await {
                Ok(j) => j,
                Err(_) => return Ok(WalletBalance { supra: 0, tokens: vec![] }),
            };

            let mut supra_balance: u64 = 0;
            if let Some(result) = json.get("result") {
                if let Some(val) = result.get(0) {
                    supra_balance = Self::parse_u64(val);
                }
            }

            Ok(WalletBalance {
                supra: supra_balance,
                tokens: vec![],
            })
        })
    }

    fn is_trustee(
        &self,
        address: &str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SetupError>> + Send + '_>> {
        let addr = address.to_string();
        Box::pin(async move {
            match self.client
                .view_raw("nft", "is_trustee", vec![], vec![Value::String(addr)])
                .await
            {
                Ok(val) => {
                    let is_t = val.get(0).and_then(|v| v.as_bool()).unwrap_or(false);
                    Ok(is_t)
                }
                Err(_) => Ok(false),
            }
        })
    }

    fn get_nft_id(
        &self,
        address: &str,
    ) -> Pin<Box<dyn Future<Output = Result<u64, SetupError>> + Send + '_>> {
        let addr = address.to_string();
        Box::pin(async move {
            match self.client
                .view_raw("nft", "get_trustee_nft_id", vec![], vec![Value::String(addr)])
                .await
            {
                Ok(val) => {
                    // Returns (bool, u64) — first is found, second is nft_id
                    let found = val.get(0).and_then(|v| v.as_bool()).unwrap_or(false);
                    if !found {
                        return Ok(0);
                    }
                    let id = val.get(1).map(|v| Self::parse_u64(v)).unwrap_or(0);
                    Ok(id)
                }
                Err(_) => Ok(0),
            }
        })
    }

    fn get_nft_config(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<NftConfigInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            match self.client
                .view_raw("nft", "get_nft_config", vec![], vec![])
                .await
            {
                Ok(val) => Ok(NftConfigInfo {
                    // Burn-exit rev: NftConfig is (burn_cooldown_seconds, admin).
                    burn_cooldown_seconds: val.get(0).map(|v| Self::parse_u64(v)).unwrap_or(3600),
                    admin: val.get(1).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                }),
                Err(_) => {
                    // Contract may not be deployed yet -- use testnet defaults
                    // (cooldown: 1 hour per nft.move DEFAULT_BURN_COOLDOWN_SECONDS).
                    Ok(NftConfigInfo {
                        burn_cooldown_seconds: 3600,
                        admin: String::new(),
                    })
                }
            }
        })
    }

    fn submit_mint(
        &self,
        pubkey: &[u8],
        sponsor: &str,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        let pubkey_vec = pubkey.to_vec();
        let sp = sponsor.to_string();
        Box::pin(async move {
            let sp_addr = parse_address(&sp)?;
            // DMKT14: mint_trustee_nft(trustee, ed25519_pubkey, sponsor).
            // The beneficiary arg is gone; sponsor is immutable post-mint and
            // receives the time-decayed refund on burn-exit.
            let args = vec![
                bcs::to_bytes(&pubkey_vec)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&sp_addr)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("nft", "mint_trustee_nft", args).await
        })
    }

    fn get_mint_fee(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<u64, SetupError>> + Send + '_>> {
        Box::pin(async move {
            match self.client
                .view_raw("ops_treasury", "get_mint_fee", vec![], vec![])
                .await
            {
                Ok(val) => {
                    // Expected: ["100000000000"] (or similar single-value array)
                    let raw = val.as_array()
                        .and_then(|arr| arr.first())
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| SetupError::ChainError(
                            "get_mint_fee: unexpected view result shape".into(),
                        ))?;
                    raw.parse::<u64>().map_err(|e| SetupError::ChainError(e.to_string()))
                }
                Err(e) => Err(SetupError::ChainError(e.to_string())),
            }
        })
    }

    fn get_total_minted(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<u64, SetupError>> + Send + '_>> {
        Box::pin(async move {
            match self.client
                .view_raw("nft", "get_total_minted", vec![], vec![])
                .await
            {
                Ok(val) => {
                    let count = val.get(0).map(|v| Self::parse_u64(v)).unwrap_or(0);
                    Ok(count)
                }
                Err(_) => Ok(0),
            }
        })
    }

    fn submit_register(
        &self,
        nft_id: u64,
        holding_period_days: u64,
        rushed_withdrawal_enabled: bool,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            let args = vec![
                bcs::to_bytes(&nft_id)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&holding_period_days)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&rushed_withdrawal_enabled)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("escrow", "register_trader", args).await
        })
    }

    fn submit_deposit(
        &self,
        metadata_address: &str,
        amount: u64,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        let meta = metadata_address.to_string();
        Box::pin(async move {
            let meta_addr = parse_address(&meta)?;
            let args = vec![
                bcs::to_bytes(&meta_addr)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&amount)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("escrow", "deposit", args).await
        })
    }

    fn submit_request_mint(
        &self,
        m_amount: u64,
        k_amount: u64,
        t_amount: u64,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            let args = vec![
                bcs::to_bytes(&m_amount)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&k_amount)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&t_amount)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("tokens", "request_mint", args).await
        })
    }

    fn submit_claim_mint(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            self.submit_entry_function("tokens", "claim_mint", vec![]).await
        })
    }

    fn get_trippples_metadata(
        &self,
        symbol: u8,
    ) -> Pin<Box<dyn Future<Output = Result<String, SetupError>> + Send + '_>> {
        let contract_bytes = self.contract_addr;
        Box::pin(async move {
            // Metadata addresses are deterministic named objects:
            // sha3_256(creator_address || seed || 0xFE)
            let seeds: &[&[u8]] = &[b"deadmkt_emm", b"deadmkt_kay", b"deadmkt_tee"];
            let seed = seeds.get(symbol as usize)
                .ok_or_else(|| SetupError::ChainError(format!("Invalid symbol {}", symbol)))?;

            use sha3::{Sha3_256, Digest};
            let mut hasher = Sha3_256::new();
            hasher.update(&contract_bytes);
            hasher.update(*seed);
            hasher.update(&[0xFE]); // named object scheme byte
            let hash = hasher.finalize();
            Ok(format!("0x{}", hex::encode(hash)))
        })
    }

    fn get_fa_balance(
        &self,
        metadata_address: &str,
    ) -> Pin<Box<dyn Future<Output = Result<u64, SetupError>> + Send + '_>> {
        let meta = metadata_address.to_string();
        let signer_addr = {
            let guard = self.signer.lock().unwrap();
            match &*guard {
                Some(s) => format!("0x{}", hex::encode(s.address)),
                None => return Box::pin(async { Ok(0) }),
            }
        };
        Box::pin(async move {
            match self.client
                .view_absolute(
                    "0x1::primary_fungible_store::balance",
                    vec!["0x1::fungible_asset::Metadata".to_string()],
                    vec![Value::String(signer_addr), Value::String(meta)],
                )
                .await
            {
                Ok(val) => {
                    let balance = val.get(0)
                        .and_then(|v| v.as_u64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                        .unwrap_or(0);
                    Ok(balance)
                }
                Err(_) => Ok(0), // No store = 0 balance
            }
        })
    }

    fn get_escrow_balance(
        &self,
        nft_id: u64,
        metadata_address: &str,
    ) -> Pin<Box<dyn Future<Output = Result<u64, SetupError>> + Send + '_>> {
        let meta = metadata_address.to_string();
        Box::pin(async move {
            match self.client
                .view_raw("escrow", "get_balance", vec![],
                    vec![Value::String(nft_id.to_string()), Value::String(meta)])
                .await
            {
                Ok(val) => {
                    let balance = val.get(0).map(|v| Self::parse_u64(v)).unwrap_or(0);
                    Ok(balance)
                }
                Err(_) => Ok(0),
            }
        })
    }

    fn get_nft_mint_state(
        &self,
        address: &str,
    ) -> Pin<Box<dyn Future<Output = Result<(bool, bool), SetupError>> + Send + '_>> {
        let addr = address.to_string();
        Box::pin(async move {
            match self.client
                .view_raw("tokens", "get_nft_mint_state", vec![],
                    vec![Value::String(addr)])
                .await
            {
                Ok(val) => {
                    let first_completed = val.get(0).and_then(|v| v.as_bool()).unwrap_or(false);
                    let has_pending = val.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
                    Ok((first_completed, has_pending))
                }
                Err(_) => Ok((false, false)),
            }
        })
    }

    fn get_pending_mint_claimable_at(
        &self,
        address: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<u64>, SetupError>> + Send + '_>> {
        let addr = address.to_string();
        Box::pin(async move {
            match self.client
                .view_raw("tokens", "get_pending_mint", vec![],
                    vec![Value::String(addr)])
                .await
            {
                Ok(val) => {
                    // Returns Option<PendingMint> as { vec: [...] }
                    let vec_val = val.get(0)
                        .and_then(|v| v.get("vec"))
                        .and_then(|v| v.as_array());
                    match vec_val {
                        Some(arr) if !arr.is_empty() => {
                            // PendingMint has claimable_at field
                            let mint = &arr[0];
                            let claimable = mint.get("claimable_at")
                                .map(|v| Self::parse_u64(v))
                                .unwrap_or(0);
                            Ok(Some(claimable))
                        }
                        _ => Ok(None),
                    }
                }
                Err(_) => Ok(None),
            }
        })
    }

    fn submit_burn_from_escrow(
        &self,
        amount: u64,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            let args = vec![
                bcs::to_bytes(&amount)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("tokens", "burn_from_escrow", args).await
        })
    }

    fn submit_burn_for_profit(
        &self,
        amount: u64,
        recipient: String,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            // DMKT14: burn_for_profit(burn_amount, recipient).
            let recipient_addr = parse_address(&recipient)?;
            let args = vec![
                bcs::to_bytes(&amount)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&recipient_addr)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("tokens", "burn_for_profit", args).await
        })
    }

    fn submit_lock_from_escrow(
        &self,
        symbol: u8,
        amount: u64,
        duration_secs: u64,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            let args = vec![
                bcs::to_bytes(&symbol)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&amount)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&duration_secs)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("tokens", "lock_from_escrow", args).await
        })
    }

    fn submit_unlock_to_escrow(
        &self,
        lock_index: u64,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            let args = vec![
                bcs::to_bytes(&lock_index)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("tokens", "unlock_to_escrow", args).await
        })
    }

    fn submit_donate_dust(
        &self,
        recipient_nft_id: u64,
        symbol: u8,
        amount: u64,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            let args = vec![
                bcs::to_bytes(&recipient_nft_id)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&symbol)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&amount)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("tokens", "donate_dust", args).await
        })
    }

    // ------------------------------------------------------------------
    // MR3: withdrawal entry functions (SUPRA-only exits).
    // DMKT14: these are trustee-signed (the node's signing key IS the
    // trustee). The `recipient` (off-chain payout address) is passed as
    // an explicit arg on the *_as_supra calls; SUPRA is sent there.
    // ------------------------------------------------------------------

    fn submit_request_rushed_withdrawal(
        &self,
        nft_id: u64,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            let args = vec![bcs::to_bytes(&nft_id)
                .map_err(|e| SetupError::ChainError(e.to_string()))?];
            self.submit_entry_function("escrow", "request_rushed_withdrawal", args).await
        })
    }

    fn submit_cancel_rushed_withdrawal(
        &self,
        nft_id: u64,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            let args = vec![bcs::to_bytes(&nft_id)
                .map_err(|e| SetupError::ChainError(e.to_string()))?];
            self.submit_entry_function("escrow", "cancel_rushed_withdrawal", args).await
        })
    }

    fn submit_rushed_withdrawal_as_supra(
        &self,
        nft_id: u64,
        recipient: String,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            // DMKT14: rushed_withdrawal_as_supra(nft_id, recipient).
            let recipient_addr = parse_address(&recipient)?;
            let args = vec![
                bcs::to_bytes(&nft_id)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&recipient_addr)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("tokens", "rushed_withdrawal_as_supra", args).await
        })
    }

    fn submit_start_holding_period(
        &self,
        nft_id: u64,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            let args = vec![bcs::to_bytes(&nft_id)
                .map_err(|e| SetupError::ChainError(e.to_string()))?];
            self.submit_entry_function("escrow", "start_holding_period", args).await
        })
    }

    fn submit_cancel_holding_period(
        &self,
        nft_id: u64,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            let args = vec![bcs::to_bytes(&nft_id)
                .map_err(|e| SetupError::ChainError(e.to_string()))?];
            self.submit_entry_function("escrow", "cancel_holding_period", args).await
        })
    }

    fn submit_claim_all_as_supra(
        &self,
        nft_id: u64,
        recipient: String,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            // DMKT14: claim_all_as_supra(nft_id, recipient).
            let recipient_addr = parse_address(&recipient)?;
            let args = vec![
                bcs::to_bytes(&nft_id)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&recipient_addr)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("tokens", "claim_all_as_supra", args).await
        })
    }

    fn submit_burn_trustee_nft(
        &self,
        nft_id: u64,
        recipient: String,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            // DMKT14: exits::burn_trustee_nft(nft_id, recipient). Single
            // trustee-signed burn-exit (replaces request/execute_burn_pair).
            let recipient_addr = parse_address(&recipient)?;
            let args = vec![
                bcs::to_bytes(&nft_id)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&recipient_addr)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("exits", "burn_trustee_nft", args).await
        })
    }
}
