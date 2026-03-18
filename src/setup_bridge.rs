// =========================================================================
// setup_bridge.rs: Real WizardIO + ChainClient for live setup wizard
// =========================================================================

use deadmkt_chain::client::SupraClient;
use deadmkt_setup::{
    ChainClient, NftConfigInfo, SetupError, TxResultInfo, WalletBalance, WizardIO,
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
    pub fn new(rpc_urls: Vec<String>, contract_addr: String) -> Self {
        let parsed = parse_address(&contract_addr).unwrap_or([0u8; 32]);
        Self {
            client: SupraClient::new(rpc_urls, contract_addr),
            contract_addr: parsed,
            signer: Mutex::new(None),
            chain_id: 6,          // Supra testnet default
            max_gas_amount: 200,
            gas_unit_price: 100000,
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
                    vm_status: format!("HTTP {} — {}", status, &body[..body.len().min(200)]),
                });
            }
        };

        if let Some(status_str) = json.get("status").and_then(|v| v.as_str()) {
            let success = status_str == "Success";
            return Ok(TxResultInfo {
                success,
                gas_used: 0,
                vm_status: status_str.to_string(),
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

    fn get_beneficiary(
        &self,
        nft_id: u64,
    ) -> Pin<Box<dyn Future<Output = Result<String, SetupError>> + Send + '_>> {
        Box::pin(async move {
            match self.client
                .view_raw("nft", "get_beneficiary", vec![], vec![Value::String(nft_id.to_string())])
                .await
            {
                Ok(val) => {
                    let ben = val.get(0).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    Ok(ben)
                }
                Err(_) => Ok(String::new()),
            }
        })
    }

    fn get_nft_config(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<NftConfigInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            match self.client
                .view_raw("nft", "get_config", vec![], vec![])
                .await
            {
                Ok(val) => Ok(NftConfigInfo {
                    bond_amount: val.get(0).map(|v| Self::parse_u64(v)).unwrap_or(100_000_000_000),
                    bond_lock_seconds: val.get(1).map(|v| Self::parse_u64(v)).unwrap_or(86400),
                    admin: val.get(2).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                }),
                Err(_) => {
                    // Contract may not be deployed yet — use testnet defaults
                    // Bond: 1 SUPRA (8 decimals), Lock: 90 days
                    Ok(NftConfigInfo {
                        bond_amount: 100_000_000,
                        bond_lock_seconds: 7_776_000,
                        admin: String::new(),
                    })
                }
            }
        })
    }

    fn submit_mint(
        &self,
        pubkey: &[u8],
        beneficiary: &str,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        let pubkey_vec = pubkey.to_vec();
        let ben = beneficiary.to_string();
        Box::pin(async move {
            let ben_addr = parse_address(&ben)?;
            let args = vec![
                bcs::to_bytes(&pubkey_vec)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
                bcs::to_bytes(&ben_addr)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("nft", "mint_pair", args).await
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

    fn submit_lock_tokens(
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
            self.submit_entry_function("tokens", "lock_tokens", args).await
        })
    }

    fn submit_unlock_tokens(
        &self,
        lock_index: u64,
    ) -> Pin<Box<dyn Future<Output = Result<TxResultInfo, SetupError>> + Send + '_>> {
        Box::pin(async move {
            let args = vec![
                bcs::to_bytes(&lock_index)
                    .map_err(|e| SetupError::ChainError(e.to_string()))?,
            ];
            self.submit_entry_function("tokens", "unlock_tokens", args).await
        })
    }
}
