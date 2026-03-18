// crates/settlement/src/worker.rs
//
// Async settlement worker — types and background task.
//
// The poll loop sends SettleRequest over a channel; the worker calls
// SettlementSubmitter::submit_settlement() off the hot path and sends
// SettleResult back. This keeps the select! loop responsive.

use deadmkt_matching::Match;
use crate::{AbortCode, SettlementOutcome, SettlementSubmitter};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

// =========================================================================
// Channel message types
// =========================================================================

/// Request from poll loop → background worker.
///
/// Created after `register_match()` has already reserved escrow outflows.
/// The worker only needs to do the RPC calls (get_account, simulate, submit).
#[derive(Debug, Clone)]
pub struct SettleRequest {
    /// The matched trade to settle.
    pub match_data: Match,
    /// Hex-encoded match_hash (key into SettlementManager.pending).
    pub match_hash: String,
    /// Whether this node is responsible for submitting (gas payer).
    /// If false, the worker skips submission (counterparty pays).
    pub is_gas_payer: bool,
}

/// Result from background worker → poll loop.
///
/// Processed by `SettlementManager::process_result()` to update
/// pending state and escrow tracker.
#[derive(Debug, Clone)]
pub enum SettleResult {
    /// Transaction submitted successfully. Awaiting on-chain confirmation
    /// via BatchTradeSettled event polling.
    Submitted {
        match_hash: String,
        tx_hash: String,
    },
    /// Simulation returned E_ALREADY_SETTLED — trade was settled by peer.
    /// Keep pending, wait for event to confirm and update escrow.
    AlreadySettled {
        match_hash: String,
    },
    /// Simulation returned a terminal abort code (overfill, inactive, etc).
    Failed {
        match_hash: String,
        code: AbortCode,
    },
    /// RPC or network error — submission couldn't complete.
    /// Keep pending for retry or expiry.
    RpcError {
        match_hash: String,
        error: String,
    },
}

impl SettleResult {
    /// Extract the match_hash regardless of variant.
    pub fn match_hash(&self) -> &str {
        match self {
            SettleResult::Submitted { match_hash, .. } => match_hash,
            SettleResult::AlreadySettled { match_hash } => match_hash,
            SettleResult::Failed { match_hash, .. } => match_hash,
            SettleResult::RpcError { match_hash, .. } => match_hash,
        }
    }
}

// =========================================================================
// Background worker
// =========================================================================

/// Spawn a background tokio task that processes settlement submissions.
///
/// The task reads `SettleRequest`s from `rx`, calls `submit_settlement_direct()`
/// for fast submission (skipping simulation), and sends `SettleResult`s back
/// via `result_tx`.
///
/// Sequence numbers are fetched once and incremented locally for consecutive
/// submissions. On any error, the sequence is refreshed from chain.
///
/// Exits cleanly when the request channel is closed (poll loop dropped).
pub fn spawn_worker(
    submitter: Arc<SettlementSubmitter>,
    mut rx: mpsc::Receiver<SettleRequest>,
    result_tx: mpsc::Sender<SettleResult>,
    tx_lock: Arc<tokio::sync::Mutex<()>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut next_seq: Option<u64> = None; // local sequence tracker

        while let Some(req) = rx.recv().await {
            // Only gas payer submits; counterparty waits for backstop
            if !req.is_gas_payer {
                continue;
            }

            let hash_short = if req.match_hash.len() >= 12 {
                &req.match_hash[..12]
            } else {
                &req.match_hash
            };

            // Acquire tx lock to prevent sequence number collisions
            // with the token worker (both submit from the same address).
            let _guard = tx_lock.lock().await;

            // Direct submission — skip simulation, submit immediately.
            // Uses locally tracked sequence number when available.
            let result = match submitter.submit_settlement_direct(
                &req.match_data, next_seq
            ).await {
                Ok((SettlementOutcome::Pending { tx_hash }, new_seq)) => {
                    next_seq = Some(new_seq); // increment for next tx
                    eprintln!("[settle-worker] submitted {} → {}", hash_short, &tx_hash);
                    SettleResult::Submitted {
                        match_hash: req.match_hash,
                        tx_hash,
                    }
                }
                Ok((outcome, _)) => {
                    // Unexpected variant from direct — treat as submitted
                    next_seq = None; // refresh on next call
                    match outcome {
                        SettlementOutcome::Confirmed { tx_hash } => {
                            SettleResult::Submitted {
                                match_hash: req.match_hash,
                                tx_hash,
                            }
                        }
                        _ => {
                            // Should not happen from submit_direct
                            SettleResult::RpcError {
                                match_hash: req.match_hash,
                                error: "unexpected outcome from direct submit".into(),
                            }
                        }
                    }
                }
                Err(e) => {
                    next_seq = None; // force refresh on next attempt
                    let err_str = e.to_string();
                    // If it's a sequence number error, log specifically
                    if err_str.contains("SEQUENCE_NUMBER") {
                        eprintln!("[settle-worker] {} seq stale, will refresh: {}", hash_short, err_str);
                    } else {
                        eprintln!("[settle-worker] {} RPC error: {}", hash_short, err_str);
                    }
                    SettleResult::RpcError {
                        match_hash: req.match_hash,
                        error: err_str,
                    }
                }
            };
            drop(_guard); // Release tx lock before sending result

            // If the result channel is closed, the poll loop has shut down — exit
            if result_tx.send(result).await.is_err() {
                eprintln!("[settle-worker] result channel closed, shutting down");
                break;
            }
        }
        eprintln!("[settle-worker] request channel closed, exiting");
    })
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_match() -> Match {
        Match {
            batch_id: 100,
            pool_id: 0,
            buyer: deadmkt_matching::RevealedOrder {
                order: deadmkt_crypto::Order {
                    nft_id: 42,
                    symbol: b"EMM/KAY".to_vec(),
                    side: 1,
                    price: 5_000_000,
                    quantity: 1000,
                    batch_id: 100,
                    nonce: vec![0u8; 32],
                },
                signature: vec![0u8; 64],
                order_hash: [0x11; 32],
            },
            seller: deadmkt_matching::RevealedOrder {
                order: deadmkt_crypto::Order {
                    nft_id: 99,
                    symbol: b"EMM/KAY".to_vec(),
                    side: 0,
                    price: 4_900_000,
                    quantity: 1000,
                    batch_id: 100,
                    nonce: vec![1u8; 32],
                },
                signature: vec![0u8; 64],
                order_hash: [0x22; 32],
            },
            symbol: b"EMM/KAY".to_vec(),
            fill_quantity: 1000,
            settlement_price: 4_950_000,
            match_hash: [0xAB; 32],
            gas_payer_nft_id: 42,
        }
    }

    #[test]
    fn settle_request_construction() {
        let m = dummy_match();
        let req = SettleRequest {
            match_hash: hex::encode(m.match_hash),
            match_data: m,
            is_gas_payer: true,
        };
        assert!(req.is_gas_payer);
        assert_eq!(req.match_hash.len(), 64); // 32 bytes hex
    }

    #[test]
    fn settle_result_match_hash() {
        let hash = "abcdef1234567890".to_string();

        let r1 = SettleResult::Submitted {
            match_hash: hash.clone(),
            tx_hash: "0xdeadbeef".to_string(),
        };
        assert_eq!(r1.match_hash(), "abcdef1234567890");

        let r2 = SettleResult::AlreadySettled {
            match_hash: hash.clone(),
        };
        assert_eq!(r2.match_hash(), "abcdef1234567890");

        let r3 = SettleResult::Failed {
            match_hash: hash.clone(),
            code: AbortCode::BatchTooOld,
        };
        assert_eq!(r3.match_hash(), "abcdef1234567890");

        let r4 = SettleResult::RpcError {
            match_hash: hash.clone(),
            error: "connection refused".to_string(),
        };
        assert_eq!(r4.match_hash(), "abcdef1234567890");
    }

    #[test]
    fn settle_result_debug_format() {
        let r = SettleResult::Failed {
            match_hash: "abc123".to_string(),
            code: AbortCode::BuyerOverfill,
        };
        let debug = format!("{:?}", r);
        assert!(debug.contains("Failed"));
        assert!(debug.contains("BuyerOverfill"));
    }

    // ─── Integration: spawn_worker with wiremock ──────────────────

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use deadmkt_chain::client::SupraClient;
    use ed25519_dalek::SigningKey;

    fn make_test_submitter(base_url: &str) -> SettlementSubmitter {
        let signing_key = SigningKey::from_bytes(&[1u8; 32]);
        let client = SupraClient::new(vec![base_url.to_string()], "0xDEAD".into());
        SettlementSubmitter::new(client, signing_key, "0xDEAD", "0x0001", 4).unwrap()
    }

    async fn mount_account_mock(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/rpc/v2/accounts/0x0000000000000000000000000000000000000000000000000000000000000001"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sequence_number": 42,
                "authentication_key": "0x0001"
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn spawn_worker_submit_success() {
        let server = MockServer::start().await;
        mount_account_mock(&server).await;

        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/simulate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "Success",
                "output": { "Move": { "gas_used": 3000, "vm_status": "Executed successfully" } }
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/submit"))
            .respond_with(ResponseTemplate::new(200).set_body_string("\"0xhash_abc\""))
            .mount(&server)
            .await;

        let submitter = Arc::new(make_test_submitter(&server.uri()));
        let (req_tx, req_rx) = mpsc::channel(8);
        let (result_tx, mut result_rx) = mpsc::channel(8);

        let handle = spawn_worker(submitter, req_rx, result_tx, Arc::new(tokio::sync::Mutex::new(())));

        let m = dummy_match();
        req_tx.send(SettleRequest {
            match_hash: hex::encode(m.match_hash),
            match_data: m,
            is_gas_payer: true,
        }).await.unwrap();

        let result = result_rx.recv().await.unwrap();
        match result {
            SettleResult::Submitted { tx_hash, .. } => assert_eq!(tx_hash, "0xhash_abc"),
            other => panic!("Expected Submitted, got {:?}", other),
        }

        drop(req_tx);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn spawn_worker_simulation_abort() {
        let server = MockServer::start().await;
        mount_account_mock(&server).await;

        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/simulate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "Fail",
                "output": { "Move": { "gas_used": 50, "vm_status": "Move abort in 0xDEAD::settlement: 21" } }
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/submit"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let submitter = Arc::new(make_test_submitter(&server.uri()));
        let (req_tx, req_rx) = mpsc::channel(8);
        let (result_tx, mut result_rx) = mpsc::channel(8);

        let handle = spawn_worker(submitter, req_rx, result_tx, Arc::new(tokio::sync::Mutex::new(())));

        let m = dummy_match();
        req_tx.send(SettleRequest {
            match_hash: hex::encode(m.match_hash),
            match_data: m,
            is_gas_payer: true,
        }).await.unwrap();

        let result = result_rx.recv().await.unwrap();
        match result {
            SettleResult::Failed { code, .. } => assert_eq!(code, AbortCode::BuyerOverfill),
            other => panic!("Expected Failed, got {:?}", other),
        }

        drop(req_tx);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn spawn_worker_skips_non_gas_payer() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let submitter = Arc::new(make_test_submitter(&server.uri()));
        let (req_tx, req_rx) = mpsc::channel(8);
        let (result_tx, mut result_rx) = mpsc::channel(8);

        let handle = spawn_worker(submitter, req_rx, result_tx, Arc::new(tokio::sync::Mutex::new(())));

        let m = dummy_match();
        req_tx.send(SettleRequest {
            match_hash: hex::encode(m.match_hash),
            match_data: m,
            is_gas_payer: false,
        }).await.unwrap();

        drop(req_tx);

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            result_rx.recv(),
        ).await;

        match result {
            Err(_) => {}
            Ok(None) => {}
            Ok(Some(r)) => panic!("Expected no result for non-gas-payer, got {:?}", r),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn spawn_worker_rpc_error() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/rpc/v2/accounts/0x0000000000000000000000000000000000000000000000000000000000000001"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
            .mount(&server)
            .await;

        let submitter = Arc::new(make_test_submitter(&server.uri()));
        let (req_tx, req_rx) = mpsc::channel(8);
        let (result_tx, mut result_rx) = mpsc::channel(8);

        let handle = spawn_worker(submitter, req_rx, result_tx, Arc::new(tokio::sync::Mutex::new(())));

        let m = dummy_match();
        req_tx.send(SettleRequest {
            match_hash: hex::encode(m.match_hash),
            match_data: m,
            is_gas_payer: true,
        }).await.unwrap();

        let result = result_rx.recv().await.unwrap();
        match result {
            SettleResult::RpcError { error, .. } => assert!(!error.is_empty()),
            other => panic!("Expected RpcError, got {:?}", other),
        }

        drop(req_tx);
        handle.await.unwrap();
    }
}
