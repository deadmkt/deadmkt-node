// Supra REST client: view calls, tx submission, simulation,
// block polling, RPC round-robin failover, wait_for_tx.

use crate::types::ChainEvent;
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChainError {
    #[error("network error: {0}")]
    NetworkError(String),

    #[error("deserialization error: {0}")]
    DeserializationError(String),

    #[error("transaction failed: {0}")]
    TransactionFailed(String),

    #[error("timeout waiting for transaction")]
    Timeout,

    #[error("all RPC endpoints failed")]
    AllEndpointsFailed,

    #[error("rate limited (HTTP {status})")]
    RateLimited { status: u16 },
}

impl ChainError {
    /// Returns true if the error indicates RPC rate limiting.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, ChainError::RateLimited { .. })
    }
}

// =========================================================================
// Response types
// =========================================================================

#[derive(Debug, Clone)]
pub struct LedgerInfo {
    pub block_height: u64,
    pub ledger_timestamp: u64,
}

#[derive(Debug, Clone)]
pub struct AccountInfo {
    pub sequence_number: u64,
    pub authentication_key: String,
}

#[derive(Debug, Clone)]
pub struct TxResult {
    pub success: bool,
    pub gas_used: u64,
    pub vm_status: String,
}

#[derive(Debug, Clone)]
pub struct SimulateResult {
    pub success: bool,
    pub gas_used: u64,
    pub vm_status: String,
}

// =========================================================================
// Client
// =========================================================================

pub struct SupraClient {
    http: reqwest::Client,
    endpoints: Vec<String>,
    contract_addr: String,
    current_endpoint: AtomicUsize,
    max_retries: usize,
}

impl SupraClient {
    pub fn new(endpoints: Vec<String>, contract_addr: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            endpoints,
            contract_addr,
            current_endpoint: AtomicUsize::new(0),
            max_retries: 3,
        }
    }

    /// Check an HTTP response for rate limiting before attempting JSON parse.
    fn check_rate_limit(resp: &reqwest::Response) -> Result<(), ChainError> {
        let status = resp.status().as_u16();
        if status == 429 || status == 403 || status == 503 {
            return Err(ChainError::RateLimited { status });
        }
        if let Some(ct) = resp.headers().get("content-type") {
            if let Ok(ct_str) = ct.to_str() {
                if ct_str.contains("text/html") {
                    return Err(ChainError::RateLimited { status });
                }
            }
        }
        Ok(())
    }

    /// Parse response body as JSON, logging raw text on failure.
    async fn parse_json(resp: reqwest::Response) -> Result<Value, ChainError> {
        let status = resp.status();
        let text = resp.text().await.map_err(|e| {
            ChainError::NetworkError(format!("failed to read response body: {}", e))
        })?;
        serde_json::from_str(&text).map_err(|e| {
            let preview: String = text.chars().take(200).collect();
            ChainError::DeserializationError(format!(
                "HTTP {} — not valid JSON: {} — raw: {}",
                status, e, preview
            ))
        })
    }

    fn next_endpoint(&self) -> String {
        let idx = self.current_endpoint.fetch_add(1, Ordering::Relaxed) % self.endpoints.len();
        self.endpoints[idx].clone()
    }

    pub fn current_base(&self) -> String {
        let idx = self.current_endpoint.load(Ordering::Relaxed) % self.endpoints.len();
        self.endpoints[idx].clone()
    }

    // =====================================================================
    // View function
    // =====================================================================

    pub async fn view_raw(
        &self,
        module: &str,
        function: &str,
        type_args: Vec<String>,
        args: Vec<Value>,
    ) -> Result<Value, ChainError> {
        let function_id = format!("{}::{}::{}", self.contract_addr, module, function);
        self.view_absolute(&function_id, type_args, args).await
    }

    /// View call with a fully-qualified function ID (e.g. "0x1::fungible_asset::decimals").
    /// Use this for framework modules or any address other than this client's contract.
    pub async fn view_absolute(
        &self,
        function_id: &str,
        type_args: Vec<String>,
        args: Vec<Value>,
    ) -> Result<Value, ChainError> {
        let body = serde_json::json!({
            "function": function_id,
            "type_arguments": type_args,
            "arguments": args,
        });

        let mut last_error = None;

        for _ in 0..self.max_retries {
            let base = self.next_endpoint();
            let url = format!("{}/rpc/v2/view", base);

            match self.http.post(&url).json(&body).send().await {
                Ok(resp) => {
                    // Check rate limiting before consuming body
                    if let Err(e) = Self::check_rate_limit(&resp) {
                        last_error = Some(e);
                        continue;
                    }

                    if resp.status().is_success() {
                        let json: Value = Self::parse_json(resp).await?;
                        // Supra may return result directly as array, or wrapped in {"result": [...]}
                        if let Some(result) = json.get("result") {
                            return Ok(result.clone());
                        }
                        // If top-level is already an array, return it directly
                        if json.is_array() {
                            return Ok(json);
                        }
                        // Single value response (some view functions return a scalar)
                        return Ok(json);
                    } else if resp.status().is_server_error() {
                        last_error = Some(ChainError::NetworkError(format!(
                            "server error: {}",
                            resp.status()
                        )));
                        continue;
                    } else {
                        let text = resp.text().await.unwrap_or_default();
                        if serde_json::from_str::<Value>(&text).is_err() {
                            return Err(ChainError::DeserializationError(
                                format!("not valid JSON: {}", &text[..text.len().min(100)]),
                            ));
                        }
                        return Err(ChainError::DeserializationError(text));
                    }
                }
                Err(e) => {
                    last_error = Some(ChainError::NetworkError(e.to_string()));
                    continue;
                }
            }
        }

        Err(last_error.unwrap_or(ChainError::AllEndpointsFailed))
    }

    // =====================================================================
    // Ledger info (block height)
    // =====================================================================

    pub async fn get_ledger_info(&self) -> Result<LedgerInfo, ChainError> {
        let base = self.current_base();
        // Supra v2: GET /rpc/v2/block → {"height": int, "timestamp": {"timestamp": int}, ...}
        let url = format!("{}/rpc/v2/block", base);

        let resp = self.http.get(&url).send().await
            .map_err(|e| ChainError::NetworkError(e.to_string()))?;

        Self::check_rate_limit(&resp)?;

        let json: Value = Self::parse_json(resp).await?;

        // v2 returns height as integer (or sometimes string), handle both
        let block_height = json
            .get("height")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .ok_or_else(|| ChainError::DeserializationError(
                format!("missing 'height' in block response: {}", 
                    serde_json::to_string(&json).unwrap_or_default().chars().take(200).collect::<String>())
            ))?;

        // v2 returns timestamp nested: {"timestamp": {"timestamp": int}}
        let ledger_timestamp = json
            .get("timestamp")
            .and_then(|t| t.get("timestamp"))
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0);

        Ok(LedgerInfo {
            block_height,
            ledger_timestamp,
        })
    }

    // =====================================================================
    // Account info
    // =====================================================================

    pub async fn get_account(&self, address: &str) -> Result<AccountInfo, ChainError> {
        let base = self.current_base();
        // v2: GET /rpc/v2/accounts/{address} → {"sequence_number": int, "authentication_key": "hex"}
        let url = format!("{}/rpc/v2/accounts/{}", base, address);

        let resp = self.http.get(&url).send().await
            .map_err(|e| ChainError::NetworkError(e.to_string()))?;

        let json: Value = Self::parse_json(resp).await?;

        // sequence_number: v2 returns integer, but handle string too for safety
        let seq = json
            .get("sequence_number")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .ok_or_else(|| ChainError::DeserializationError(
                format!("missing sequence_number in: {}",
                    serde_json::to_string(&json).unwrap_or_default().chars().take(200).collect::<String>())
            ))?;

        let auth_key = json
            .get("authentication_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        Ok(AccountInfo {
            sequence_number: seq,
            authentication_key: auth_key,
        })
    }

    // =====================================================================
    // Submit transaction
    // =====================================================================

    /// Submit a BCS-encoded SignedTransaction.
    /// Caller provides raw BCS(SignedTransaction) — this method wraps it in
    /// the SupraTransaction envelope (0x01 prefix) before submitting.
    pub async fn submit_raw(&self, signed_tx_bcs: &[u8]) -> Result<String, ChainError> {
        let base = self.current_base();
        let url = format!("{}/rpc/v3/transactions/submit", base);

        // Wrap in SupraTransaction: 0x01 (MOVE variant) + signed_tx_bcs
        let mut payload = Vec::with_capacity(1 + signed_tx_bcs.len());
        payload.push(0x01);
        payload.extend_from_slice(signed_tx_bcs);

        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/x.supra.signed_transaction+bcs")
            .body(payload)
            .send()
            .await
            .map_err(|e| ChainError::NetworkError(e.to_string()))?;

        let status = resp.status();
        let body = resp.text().await
            .map_err(|e| ChainError::DeserializationError(e.to_string()))?;

        if !status.is_success() {
            return Err(ChainError::TransactionFailed(body));
        }

        // Supra v3 returns the tx hash as a plain JSON string: "0xABCD..."
        let hash = body.trim().trim_matches('"').to_string();
        Ok(hash)
    }

    // =====================================================================
    // Simulate transaction
    // =====================================================================

    /// Simulate a BCS-encoded SignedTransaction (signature can be zeroed).
    /// Wraps in SupraTransaction envelope before submitting.
    pub async fn simulate_raw(&self, signed_tx_bcs: &[u8]) -> Result<SimulateResult, ChainError> {
        let base = self.current_base();
        let url = format!("{}/rpc/v3/transactions/simulate", base);

        // Wrap in SupraTransaction envelope
        let mut payload = Vec::with_capacity(1 + signed_tx_bcs.len());
        payload.push(0x01);
        payload.extend_from_slice(signed_tx_bcs);

        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/x.supra.signed_transaction+bcs")
            .body(payload)
            .send()
            .await
            .map_err(|e| ChainError::NetworkError(e.to_string()))?;

        let resp_text = resp.text().await
            .map_err(|e| ChainError::DeserializationError(e.to_string()))?;

        let json: Value = serde_json::from_str(&resp_text)
            .map_err(|e| ChainError::DeserializationError(
                format!("JSON parse error: {}. Raw response: {}", e, &resp_text[..resp_text.len().min(500)])
            ))?;

        // Supra returns: status="Success"/"Fail", output.Move.gas_used, output.Move.vm_status
        let status_str = json.get("status").and_then(|v| v.as_str()).unwrap_or("Fail");
        let output = json.get("output").and_then(|o| o.get("Move"));
        let gas_used = output
            .and_then(|m| m.get("gas_used"))
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0);
        let vm_status = output
            .and_then(|m| m.get("vm_status"))
            .and_then(|v| v.as_str())
            .unwrap_or(status_str)
            .to_string();

        // Debug: log vm_status and abbreviated response on failure
        if status_str != "Success" {
            eprintln!("[simulate] FAILED vm_status: {}", &vm_status);
            eprintln!("[simulate] Raw response: {}", &resp_text[..resp_text.len().min(500)]);
        }

        Ok(SimulateResult {
            gas_used,
            success: status_str == "Success",
            vm_status,
        })
    }

    // =====================================================================
    // Wait for transaction
    // =====================================================================

    pub async fn wait_for_tx(
        &self,
        tx_hash: &str,
        timeout: Duration,
    ) -> Result<TxResult, ChainError> {
        let base = self.current_base();
        let hash_clean = tx_hash.strip_prefix("0x").unwrap_or(tx_hash);
        let url = format!("{}/rpc/v3/transactions/{}?type=user", base, hash_clean);
        let poll_interval = Duration::from_millis(500);
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                return Err(ChainError::Timeout);
            }

            let resp = self.http.get(&url).send().await
                .map_err(|e| ChainError::NetworkError(e.to_string()))?;

            if resp.status() == 404 {
                // Not found yet — keep polling
                tokio::time::sleep(poll_interval).await;
                continue;
            }

            let body = resp.text().await
                .map_err(|e| ChainError::DeserializationError(e.to_string()))?;

            if let Ok(json) = serde_json::from_str::<Value>(&body) {
                // Supra uses "status": "Success"/"Fail"/"Pending"
                if let Some(status_str) = json.get("status").and_then(|v| v.as_str()) {
                    if status_str == "Pending" {
                        tokio::time::sleep(poll_interval).await;
                        continue;
                    }

                    let output = json.get("output").and_then(|o| o.get("Move"));
                    let gas_used = output
                        .and_then(|m| m.get("gas_used"))
                        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                        .unwrap_or(0);
                    let vm_status = output
                        .and_then(|m| m.get("vm_status"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(status_str)
                        .to_string();

                    return Ok(TxResult {
                        success: status_str == "Success",
                        gas_used,
                        vm_status,
                    });
                }
            }

            // Unparseable response — keep polling
            tokio::time::sleep(poll_interval).await;
        }
    }

    // =====================================================================
    // Event polling
    // =====================================================================

    /// Fetch events by type within a block height range.
    ///
    /// Supra events API: `GET /rpc/v3/events/{event_type}?start_height={}&end_height={}&limit={}`
    /// event_type = "contract_addr::module::EventStructName"
    ///
    /// NOTE: The old Aptos-style `/accounts/{addr}/events/{creation_number}` does NOT exist
    /// on Supra. Events are queried by their fully-qualified Move type name.
    pub async fn get_events(
        &self,
        event_type: &str,
        start_height: u64,
        end_height: u64,
        limit: u64,
    ) -> Result<Vec<ChainEvent>, ChainError> {
        let base = self.current_base();
        // v3 events API with block height range
        let url = format!(
            "{}/rpc/v3/events/{}?start_height={}&end_height={}&limit={}",
            base, event_type, start_height, end_height, limit
        );

        let resp = self.http.get(&url).send().await
            .map_err(|e| ChainError::NetworkError(e.to_string()))?;

        Self::check_rate_limit(&resp)?;

        let json: Value = Self::parse_json(resp).await?;

        // v3 response: {"data": [{"event": {..., "type": "...", "data": {...}}, "block_height": int, "transaction_hash": "..."}]}
        let data_arr = json
            .get("data")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut events = Vec::new();
        for item in &data_arr {
            // v3 wraps each event in {"event": {...}, "block_height": ..., "transaction_hash": ...}
            let event_obj = item.get("event").unwrap_or(item);
            match ChainEvent::from_json(event_obj) {
                Ok(event) => events.push(event),
                Err(_) => {} // skip unparseable events
            }
        }

        Ok(events)
    }

    // =====================================================================
    // High-level view calls
    // =====================================================================

    /// Fetch the Ed25519 pubkey stored on-chain for a given NFT ID.
    /// Calls nft::get_trustee_pubkey(nft_id) → returns hex-encoded 32-byte pubkey.
    pub async fn get_trustee_pubkey(&self, nft_id: u64) -> Result<Vec<u8>, ChainError> {
        let result = self
            .view_raw(
                "nft",
                "get_trustee_pubkey",
                vec![],
                vec![Value::String(nft_id.to_string())],
            )
            .await?;

        // Result is ["0x<hex>"] or just the hex string at index 0
        let hex_str = result
            .get(0)
            .and_then(|v| v.as_str())
            .ok_or_else(|| ChainError::DeserializationError("missing pubkey in result".into()))?;

        hex::decode(hex_str.trim_start_matches("0x"))
            .map_err(|e| ChainError::DeserializationError(format!("invalid hex pubkey: {}", e)))
    }

    /// Fetch market pair config for a given symbol.
    /// Calls settlement::get_market_pair(symbol_hex) → MarketPair struct.
    pub async fn get_market_pair_config(
        &self,
        symbol: &[u8],
    ) -> Result<crate::types::MarketPair, ChainError> {
        let symbol_hex = format!("0x{}", hex::encode(symbol));
        let result = self
            .view_raw(
                "settlement",
                "get_market_pair",
                vec![],
                vec![Value::String(symbol_hex)],
            )
            .await?;

        crate::types::MarketPair::from_view_result(&result)
            .map_err(|e| ChainError::DeserializationError(e.to_string()))
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // T_CLIENT_01
    #[tokio::test]
    async fn test_view_function_success() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc/v2/view"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": ["10", "4", "3", "1", "2", "3"]
            })))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let result = client
            .view_raw("pool_config", "get_batch_params", vec![], vec![])
            .await
            .unwrap();

        assert_eq!(result[0].as_str().unwrap(), "10");
        assert_eq!(result[5].as_str().unwrap(), "3");
    }

    // T_CLIENT_02
    #[tokio::test]
    async fn test_view_function_malformed() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc/v2/view"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let result = client
            .view_raw("pool_config", "get_batch_params", vec![], vec![])
            .await;

        assert!(matches!(result, Err(ChainError::DeserializationError(_))));
    }

    // T_CLIENT_03
    #[tokio::test]
    async fn test_view_function_retry_on_error() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc/v2/view"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rpc/v2/view"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": ["10", "4", "3", "1", "2", "3"]
            })))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let result = client
            .view_raw("pool_config", "get_batch_params", vec![], vec![])
            .await
            .unwrap();

        assert_eq!(result[0].as_str().unwrap(), "10");
    }

    // T_CLIENT_04
    #[tokio::test]
    async fn test_submit_transaction_success() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/submit"))
            .respond_with(ResponseTemplate::new(200).set_body_string("\"0xTXHASH123\""))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let tx_hash = client.submit_raw(b"fake-bcs-payload").await.unwrap();
        assert_eq!(tx_hash, "0xTXHASH123");
    }

    // T_CLIENT_06
    #[tokio::test]
    async fn test_simulate_transaction() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/simulate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "Success",
                "output": {
                    "Move": {
                        "gas_used": 1250,
                        "vm_status": "Executed successfully"
                    }
                }
            })))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let gas = client.simulate_raw(b"fake-payload").await.unwrap();
        assert_eq!(gas.gas_used, 1250);
        assert!(gas.success);
    }

    // T_CLIENT_07
    #[tokio::test]
    async fn test_block_height_poll() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rpc/v2/block"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "height": 99500,
                "timestamp": { "timestamp": 1708100000000000u64 },
                "author": "0x00", "hash": "0x00", "parent": "0x00",
                "view": { "epoch_id": { "chain_id": 6, "epoch": 1 }, "round": 1 }
            })))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let info = client.get_ledger_info().await.unwrap();
        assert_eq!(info.block_height, 99500);
    }

    // T_CLIENT_08
    #[tokio::test]
    async fn test_rpc_round_robin() {
        let dead_server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&dead_server)
            .await;

        let live_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc/v2/view"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": ["10", "4", "3", "1", "2", "3"]
            })))
            .mount(&live_server)
            .await;

        let client = SupraClient::new(
            vec![dead_server.uri(), live_server.uri()],
            "0xDEADMKT".into(),
        );
        let result = client
            .view_raw("pool_config", "get_batch_params", vec![], vec![])
            .await
            .unwrap();

        assert_eq!(result[0].as_str().unwrap(), "10");
    }

    // T_CLIENT_09
    #[tokio::test]
    async fn test_get_account() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rpc/v2/accounts/0xTRUSTEE"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "sequence_number": 42,
                "authentication_key": "0xABCD1234"
            })))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let account = client.get_account("0xTRUSTEE").await.unwrap();
        assert_eq!(account.sequence_number, 42);
    }

    // T_CLIENT_10
    #[tokio::test]
    async fn test_wait_for_tx_confirmed() {
        let mock_server = MockServer::start().await;

        // First poll: pending
        Mock::given(method("GET"))
            .and(path_regex(r"/rpc/v3/transactions/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "Pending",
                "hash": "0xTXHASH"
            })))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        // Second poll: confirmed
        Mock::given(method("GET"))
            .and(path_regex(r"/rpc/v3/transactions/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "Success",
                "hash": "0xTXHASH",
                "output": {
                    "Move": {
                        "gas_used": 1250,
                        "vm_status": "Executed successfully"
                    }
                }
            })))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let result = client
            .wait_for_tx("0xTXHASH", Duration::from_secs(5))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.gas_used, 1250);
    }

    // T_CLIENT_11
    #[tokio::test]
    async fn test_wait_for_tx_vm_error() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/rpc/v3/transactions/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "Fail",
                "hash": "0xTXHASH",
                "output": {
                    "Move": {
                        "gas_used": 500,
                        "vm_status": "Move abort: 0xDEADMKT::settlement: E_ALREADY_SETTLED(14)"
                    }
                }
            })))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let result = client
            .wait_for_tx("0xTXHASH", Duration::from_secs(5))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.vm_status.contains("E_ALREADY_SETTLED"));
    }

    // T_CLIENT_12
    #[tokio::test]
    async fn test_event_polling() {
        let mock_server = MockServer::start().await;
        // v3 events endpoint: GET /rpc/v3/events/{event_type}?start_height=...&end_height=...&limit=...
        Mock::given(method("GET"))
            .and(path_regex(r"/rpc/v3/events/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {
                        "event": {
                            "sequence_number": "0",
                            "type": "0xDEADMKT::settlement::BatchTradeSettled",
                            "data": {
                                "batch_id": "100", "pool_id": "3",
                                "trade_id": "0xABCD",
                                "buyer_nft_id": "42", "seller_nft_id": "99",
                                "symbol": "0x4d454d452f5455534443",
                                "buyer_price": "5000000",
                                "seller_price": "4900000",
                                "clearing_price": "4950000",
                                "base_amount": "100000000",
                                "quote_amount": "495000000",
                                "buyer_order_hash": "0xBH",
                                "seller_order_hash": "0xSH",
                                "gas_payer": "0xGAS", "timestamp": "1708100000"
                            }
                        },
                        "block_height": 500,
                        "transaction_hash": "0xTXHASH"
                    }
                ]
            })))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        // New signature: get_events(event_type, start_height, end_height, limit)
        let events = client.get_events(
            "0xDEADMKT::settlement::BatchTradeSettled",
            490, 510, 25
        ).await.unwrap();

        assert_eq!(events.len(), 1);
        match &events[0] {
            ChainEvent::BatchTradeSettled(e) => assert_eq!(e.batch_id, 100),
            _ => panic!("wrong event type"),
        }
    }

    // T_CLIENT_13: block poller phase change (simplified — test the computation path)
    #[tokio::test]
    async fn test_block_poller_phase_detection() {
        use crate::batch::{compute_phase, is_phase_boundary, Phase};
        use crate::types::{BatchEpoch, BatchParams};

        let mock_server = MockServer::start().await;

        // Return incrementing block heights
        Mock::given(method("GET"))
            .and(path("/rpc/v2/block"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "height": 1003,
                "timestamp": { "timestamp": 1708100000000000u64 },
                "author": "0x00", "hash": "0x00", "parent": "0x00",
                "view": { "epoch_id": { "chain_id": 6, "epoch": 1 }, "round": 1 }
            })))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/rpc/v2/block"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "height": 1004,
                "timestamp": { "timestamp": 1708100000000000u64 },
                "author": "0x00", "hash": "0x00", "parent": "0x00",
                "view": { "epoch_id": { "chain_id": 6, "epoch": 1 }, "round": 1 }
            })))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let epoch = BatchEpoch { anchor_block: 1000, anchor_batch_id: 50 };
        let params = BatchParams {
            blocks_per_batch: 10, commit_blocks: 4,
            reveal_blocks: 3, match_blocks: 1, swap_blocks: 2,
            commits_per_batch: 3,
        };

        // Poll 1: block 1003
        let info1 = client.get_ledger_info().await.unwrap();
        let phase1 = compute_phase(info1.block_height, &epoch, &params);

        // Poll 2: block 1004
        let info2 = client.get_ledger_info().await.unwrap();
        let phase2 = compute_phase(info2.block_height, &epoch, &params);

        assert_eq!(phase1, Phase::Commit);
        assert_eq!(phase2, Phase::Reveal);
        assert!(is_phase_boundary(info1.block_height, info2.block_height, &epoch, &params));
    }

    // T_SP2_01: HTML rate-limit page surfaces as RateLimited, not deserialization error
    #[tokio::test]
    async fn test_sp2_01_rate_limited_html_response() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rpc/v2/block"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("content-type", "text/html")
                    .set_body_string("<html>error 1015</html>"),
            )
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let err = client.get_ledger_info().await.unwrap_err();
        assert!(matches!(err, ChainError::RateLimited { status: 403 }));
        assert!(err.is_rate_limited());
    }

    // T_SP2_02: HTTP 429 surfaces as RateLimited
    #[tokio::test]
    async fn test_sp2_02_rate_limited_429() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rpc/v2/block"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let err = client.get_ledger_info().await.unwrap_err();
        assert!(err.is_rate_limited());
        assert!(matches!(err, ChainError::RateLimited { status: 429 }));
    }

    // T_SP2_03: view_absolute retries on a rate-limited endpoint and succeeds on the next attempt
    #[tokio::test]
    async fn test_sp2_03_view_retries_on_rate_limit() {
        let mock_server = MockServer::start().await;

        // First call: rate-limited (consumed once)
        Mock::given(method("POST"))
            .and(path("/rpc/v2/view"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("content-type", "text/html")
                    .set_body_string("<html>error 1015</html>"),
            )
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        // Subsequent calls: success
        Mock::given(method("POST"))
            .and(path("/rpc/v2/view"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": ["42"]
            })))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let result = client
            .view_raw("test", "fn", vec![], vec![])
            .await
            .unwrap();
        assert_eq!(result[0].as_str().unwrap(), "42");
    }

    // T_SP2_04: get_events rate-limit detection
    #[tokio::test]
    async fn test_sp2_04_events_rate_limited() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/rpc/v3/events/.*"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("content-type", "text/html")
                    .set_body_string("<html>rate limited</html>"),
            )
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let err = client
            .get_events("0xDEAD::test::Event", 0, 100, 25)
            .await
            .unwrap_err();
        assert!(err.is_rate_limited());
    }
}
