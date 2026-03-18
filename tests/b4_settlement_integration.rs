// tests/b4_settlement_integration.rs
//
// B4 Phase 6: Integration tests
//
// These tests wire together multiple crates to verify the full settlement
// lifecycle works end-to-end:
//   - EscrowTracker ↔ SettlementManager ↔ SettlementSubmitter
//   - WithdrawalHandler ↔ EscrowTracker
//   - ProfitManager ↔ EscrowTracker
//   - Storage ↔ crash recovery
//
// All chain interactions use wiremock for deterministic testing.

use std::sync::{Arc, Mutex};

use deadmkt_chain::client::SupraClient;
use deadmkt_chain::types::BatchTradeSettledEvent;
use deadmkt_crypto::Order;
use deadmkt_escrow_tracker::EscrowTracker;
use deadmkt_matching::{Match, RevealedOrder};
use deadmkt_settlement::{
    AbortCode, SettlementAction, SettlementManager, SettlementOutcome, SettlementSubmitter,
};
use deadmkt_storage::Storage;
use ed25519_dalek::SigningKey;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// =========================================================================
// Test helpers
// =========================================================================

fn make_test_match(batch_id: u64, buyer_nft: u64, seller_nft: u64, fill_qty: u64) -> Match {
    let buyer_order = Order {
        nft_id: buyer_nft,
        symbol: b"EMM/KAY".to_vec(),
        side: 1,
        price: 5_000_000,
        quantity: 10_000,
        batch_id,
        nonce: vec![0xAA; 32],
    };
    let seller_order = Order {
        nft_id: seller_nft,
        symbol: b"EMM/KAY".to_vec(),
        side: 0,
        price: 4_800_000,
        quantity: 10_000,
        batch_id,
        nonce: vec![0xBB; 32],
    };

    // Unique match_hash based on batch_id and fill_qty
    let mut hash = [0u8; 32];
    hash[0] = (batch_id & 0xFF) as u8;
    hash[1] = (fill_qty & 0xFF) as u8;
    hash[2] = (buyer_nft & 0xFF) as u8;
    hash[3] = (seller_nft & 0xFF) as u8;

    Match {
        batch_id,
        pool_id: 3,
        symbol: b"EMM/KAY".to_vec(),
        buyer: RevealedOrder {
            order: buyer_order,
            signature: vec![0x11; 64],
            order_hash: [0xAA; 32],
        },
        seller: RevealedOrder {
            order: seller_order,
            signature: vec![0x22; 64],
            order_hash: [0xBB; 32],
        },
        fill_quantity: fill_qty,
        settlement_price: 4_900_000,
        match_hash: hash,
        gas_payer_nft_id: buyer_nft,
    }
}

fn make_event_for(m: &Match) -> BatchTradeSettledEvent {
    BatchTradeSettledEvent {
        batch_id: m.batch_id,
        pool_id: m.pool_id,
        trade_id: hex::encode(m.match_hash),
        buyer_nft_id: m.buyer.order.nft_id,
        seller_nft_id: m.seller.order.nft_id,
        symbol: m.symbol.clone(),
        buyer_price: m.buyer.order.price,
        seller_price: m.seller.order.price,
        clearing_price: m.settlement_price,
        base_amount: m.fill_quantity,
        quote_amount: (m.fill_quantity as u128 * m.settlement_price as u128 / 100_000_000) as u64,
        buyer_order_hash: hex::encode(m.buyer.order_hash),
        seller_order_hash: hex::encode(m.seller.order_hash),
        gas_payer: "0x0001".to_string(),
        timestamp: 1234567890,
    }
}

fn make_submitter(base_url: &str) -> SettlementSubmitter {
    let signing_key = SigningKey::from_bytes(&[1u8; 32]);
    let client = SupraClient::new(vec![base_url.to_string()], "0xDEAD".into());
    SettlementSubmitter::new(client, signing_key, "0xDEAD", "0x0001", 4).unwrap()
}

async fn setup_mock_server() -> MockServer {
    let server = MockServer::start().await;

    // Account endpoint (sequence number)
    Mock::given(method("GET"))
        .and(path("/rpc/v2/accounts/0x0000000000000000000000000000000000000000000000000000000000000001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sequence_number": "42",
            "authentication_key": "0x0001"
        })))
        .mount(&server)
        .await;

    // Simulate success
    Mock::given(method("POST"))
        .and(path("/rpc/v3/transactions/simulate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "Success", "output": { "Move": { "gas_used": 3000, "vm_status": "Executed successfully" } }
        })))
        .mount(&server)
        .await;

    // Submit success
    Mock::given(method("POST"))
        .and(path("/rpc/v3/transactions/submit"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "\"0xSETTLED_TX\"",
        ))
        .mount(&server)
        .await;

    server
}

// =========================================================================
// T_INT_B4_01: Match → register → submit → confirm → tracker reconciled
// =========================================================================

#[tokio::test]
async fn t_int_b4_01_full_settle_loop() {
    let server = setup_mock_server().await;
    let tracker = Arc::new(Mutex::new(EscrowTracker::new()));

    // We are buyer (nft 42)
    let own_nft = 42;
    let mut mgr = SettlementManager::new(tracker.clone(), own_nft);

    // Set initial balances
    {
        let mut t = tracker.lock().unwrap();
        t.set_confirmed("KAY", 10_000);
        t.set_confirmed("EMM", 50_000);
    }

    // Step 1: Create and register match
    let m = make_test_match(100, 42, 99, 1_000);
    let match_hash_hex = hex::encode(m.match_hash);
    mgr.register_match(m.clone(), true, 1000, 50, "EMM", "KAY", 5, 5).unwrap();

    // Verify outflow applied
    // buyer outflow = 1000 * 5_000_000 / 1e8 = 50 KAY
    {
        let t = tracker.lock().unwrap();
        assert_eq!(t.projected("KAY"), 10_000 - 50);
        assert_eq!(t.projected("EMM"), 50_000); // inflow not counted yet
    }
    assert_eq!(mgr.pending_count(), 1);

    // Step 2: Submit settlement
    let submitter = make_submitter(&server.uri());
    let outcome = submitter.submit_settlement(&m).await.unwrap();
    match &outcome {
        SettlementOutcome::Pending { tx_hash } => {
            mgr.mark_submitted(&match_hash_hex, tx_hash.clone(), 1005);
        }
        other => panic!("Expected Pending, got {:?}", other),
    }

    // Step 3: Confirm via chain event
    let event = make_event_for(&m);
    let was_ours = mgr.on_settlement_confirmed(&event);
    assert!(was_ours);

    // Step 4: Verify tracker reconciled
    assert_eq!(mgr.pending_count(), 0);
    {
        let t = tracker.lock().unwrap();
        // pending_out cleared → KAY restored
        assert_eq!(t.projected("KAY"), 10_000);
        // inflow applied → EMM += fill_quantity
        assert_eq!(t.projected("EMM"), 50_000 + 1_000);
    }
}

// =========================================================================
// T_INT_B4_02: Match → submit fails E_ALREADY_SETTLED → event confirms
// =========================================================================

#[tokio::test]
async fn t_int_b4_02_already_settled_then_event_confirms() {
    let tracker = Arc::new(Mutex::new(EscrowTracker::new()));
    let own_nft = 42;
    let mut mgr = SettlementManager::new(tracker.clone(), own_nft);

    {
        let mut t = tracker.lock().unwrap();
        t.set_confirmed("KAY", 10_000);
        t.set_confirmed("EMM", 50_000);
    }

    let m = make_test_match(100, 42, 99, 1_000);
    let match_hash_hex = hex::encode(m.match_hash);
    mgr.register_match(m.clone(), true, 1000, 50, "EMM", "KAY", 5, 5).unwrap();

    // Submit fails with E_ALREADY_SETTLED
    let action = mgr.on_settlement_failed(&match_hash_hex, AbortCode::AlreadySettled);
    assert_eq!(action, SettlementAction::KeepPending);

    // Pending NOT removed — trade DID settle (CD-8)
    assert_eq!(mgr.pending_count(), 1);

    // Outflow still reserved (not reversed)
    {
        let t = tracker.lock().unwrap();
        assert_eq!(t.projected("KAY"), 10_000 - 50);
    }

    // Later: BatchTradeSettled event arrives
    let event = make_event_for(&m);
    let was_ours = mgr.on_settlement_confirmed(&event);
    assert!(was_ours);

    // Now fully reconciled
    assert_eq!(mgr.pending_count(), 0);
    {
        let t = tracker.lock().unwrap();
        assert_eq!(t.projected("KAY"), 10_000);
        assert_eq!(t.projected("EMM"), 50_000 + 1_000);
    }
}

// =========================================================================
// T_INT_B4_03: Match → backstop submission
// =========================================================================

#[tokio::test]
async fn t_int_b4_03_backstop_submission() {
    let server = setup_mock_server().await;
    let tracker = Arc::new(Mutex::new(EscrowTracker::new()));

    // We are SELLER (nft 99) — NOT gas payer
    let own_nft = 99;
    let mut mgr = SettlementManager::new(tracker.clone(), own_nft);

    {
        let mut t = tracker.lock().unwrap();
        t.set_confirmed("KAY", 0);
        t.set_confirmed("EMM", 50_000);
    }

    let m = make_test_match(100, 42, 99, 1_000);
    let match_hash_hex = hex::encode(m.match_hash);

    // Register as non-gas-payer. swap_start=1000, bpb=50 → backstop at 1050.
    mgr.register_match(m.clone(), false, 1000, 50, "EMM", "KAY", 5, 5).unwrap();

    // Seller outflow = 1000 EMM
    {
        let t = tracker.lock().unwrap();
        assert_eq!(t.projected("EMM"), 50_000 - 1_000);
    }

    // Before backstop window: not eligible
    assert!(mgr.check_backstop_eligible(1049).is_empty());

    // At backstop window: eligible
    let eligible = mgr.check_backstop_eligible(1050);
    assert_eq!(eligible.len(), 1);
    assert_eq!(eligible[0], match_hash_hex);

    // Submit backstop
    let submitter = make_submitter(&server.uri());
    let outcome = mgr.backstop_submit(&match_hash_hex, &submitter).await.unwrap();
    match outcome {
        SettlementOutcome::Pending { tx_hash } => assert_eq!(tx_hash, "0xSETTLED_TX"),
        other => panic!("Expected Pending, got {:?}", other),
    }

    // tx_hash should be set on pending
    assert!(mgr.get_pending(&match_hash_hex).unwrap().tx_hash.is_some());

    // Confirm via event
    let event = make_event_for(&m);
    mgr.on_settlement_confirmed(&event);

    assert_eq!(mgr.pending_count(), 0);
    {
        let t = tracker.lock().unwrap();
        assert_eq!(t.projected("EMM"), 50_000); // outflow cleared
        // seller inflow = 1000 * 4_900_000 / 1e8 = 49 KAY
        assert_eq!(t.projected("KAY"), 49);
    }
}

// =========================================================================
// T_INT_B4_04: Crash recovery — pending settlements reconciled on restart
// =========================================================================

#[test]
fn t_int_b4_04_crash_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let db = Storage::open(dir.path()).unwrap();

    // Simulate pre-crash state: 3 settlements in storage
    // 1: confirmed on-chain (we'll simulate finding it)
    // 2: still pending (not on chain yet)
    // 3: expired (batch too old)

    let m1 = make_test_match(100, 42, 99, 1_000);
    let m2 = make_test_match(101, 42, 88, 2_000);
    let m3 = make_test_match(50, 42, 77, 500); // old batch

    db.insert_settlement(
        &m1.match_hash, 100, 3, 42, 99, "EMM/KAY", 1_000, 4_900_000,
        "KAY", 50, "EMM", 1_000, true,
    ).unwrap();
    db.update_settlement_status(&m1.match_hash, "submitted", Some("0xTX1"), None).unwrap();

    db.insert_settlement(
        &m2.match_hash, 101, 3, 42, 88, "EMM/KAY", 2_000, 4_900_000,
        "KAY", 100, "EMM", 2_000, true,
    ).unwrap();

    db.insert_settlement(
        &m3.match_hash, 50, 3, 42, 77, "EMM/KAY", 500, 4_900_000,
        "KAY", 25, "EMM", 500, true,
    ).unwrap();

    // Step 1: Load pending from storage (crash recovery query)
    let pending = db.get_pending_settlements_v2().unwrap();
    assert_eq!(pending.len(), 3); // all 3 are pending/submitted

    // Step 2: Rebuild tracker from pending records
    let tracker = Arc::new(Mutex::new(EscrowTracker::new()));
    {
        let mut t = tracker.lock().unwrap();
        // Simulate chain read of confirmed balances on restart
        t.set_confirmed("KAY", 10_000);
        t.set_confirmed("EMM", 50_000);

        // Re-apply outflows for pending settlements
        for rec in &pending {
            t.reserve_outflow(&rec.outflow_token, &hex::encode(&rec.match_hash), rec.outflow_amount).unwrap();
            t.track_inflow(&rec.inflow_token, &hex::encode(&rec.match_hash), rec.inflow_amount).unwrap();
        }
    }

    // Step 3: Simulate chain reconciliation
    // current_batch=111, settlement_max_age=10
    // m1 (batch 100): confirmed on chain → confirm in tracker
    {
        let mut t = tracker.lock().unwrap();
        t.confirm_settlement(&hex::encode(m1.match_hash)).unwrap();
    }
    db.update_settlement_status(&m1.match_hash, "confirmed", None, None).unwrap();

    // m2 (batch 101): still pending, within age → keep pending
    // (current_batch 111 - batch 101 = 10, not > max_age 10)
    // No action needed — stays pending

    // m3 (batch 50): expired (111 - 50 = 61 > 10) → release
    {
        let mut t = tracker.lock().unwrap();
        t.release_pending(&hex::encode(m3.match_hash)).unwrap();
    }
    db.update_settlement_status(&m3.match_hash, "expired", None, None).unwrap();

    // Step 4: Verify final state
    {
        let t = tracker.lock().unwrap();
        // m1 confirmed: outflow cleared, inflow applied (EMM += 1000)
        // m2 still pending: outflow 100 KAY still reserved
        // m3 expired: outflow reversed
        // KAY: 10000 - 100 (m2 pending) = 9900
        assert_eq!(t.projected("KAY"), 10_000 - 100);
        // EMM: 50000 + 1000 (m1 inflow) = 51000
        assert_eq!(t.projected("EMM"), 50_000 + 1_000);
    }

    // Storage state: 1 confirmed, 1 pending, 1 expired
    let still_pending = db.get_pending_settlements_v2().unwrap();
    assert_eq!(still_pending.len(), 1);
    assert_eq!(still_pending[0].match_hash, m2.match_hash.to_vec());

    let rec1 = db.get_settlement(&m1.match_hash).unwrap().unwrap();
    assert_eq!(rec1.status, "confirmed");

    let rec3 = db.get_settlement(&m3.match_hash).unwrap().unwrap();
    assert_eq!(rec3.status, "expired");
}

// T_INT_B4_05 removed: tested dead ProfitManager crate (L1)
