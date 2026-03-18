// =========================================================================
// T_INT_B5_01: Full batch cycle integration test
// =========================================================================
//
// Strategy connects via WS → receives batch_start → sends commit →
// orchestrator publishes commits → reveal phase → match phase →
// settlement registered → strategy notified.

use deadmkt_chain::types::{BatchParams, BatchTradeSettledEvent, ChainEvent};
use deadmkt_crypto::{self, Order, encode_order};
use deadmkt_matching::MarketConfig;
use deadmkt_orchestrator::{
    BatchStatePort, CommitMessage, GossipPort, Orchestrator, Phase, RevealMessage, ValidatedOrder,
};
use deadmkt_strategy::server::StrategyServer;
use deadmkt_strategy::{
    BatchParamsData, BatchStartData, MintStateData, StrategyAction, StrategyEvent,
};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

// ── Test order helpers ────────────────────────────────────────────────────

fn make_test_order(nft_id: u64, side: u8, price: u64, quantity: u64) -> Order {
    Order {
        nft_id,
        symbol: b"EMM/KAY".to_vec(),
        side,
        price,
        quantity,
        batch_id: 100,
        nonce: vec![nft_id as u8; 32],
    }
}

// ── Mock GossipPort ──────────────────────────────────────────────────────

struct MockGossip {
    commits: Mutex<Vec<CommitMessage>>,
    reveals: Mutex<Vec<RevealMessage>>,
}

impl MockGossip {
    fn new() -> Self {
        Self {
            commits: Mutex::new(Vec::new()),
            reveals: Mutex::new(Vec::new()),
        }
    }
}

impl GossipPort for MockGossip {
    fn publish_commit(&self, _pool_id: u64, commit: CommitMessage) -> Result<(), String> {
        self.commits.lock().unwrap().push(commit);
        Ok(())
    }
    fn publish_reveal(&self, _pool_id: u64, reveal: RevealMessage) -> Result<(), String> {
        self.reveals.lock().unwrap().push(reveal);
        Ok(())
    }
    fn received_commits(&self, _batch_id: u64) -> Vec<CommitMessage> {
        self.commits.lock().unwrap().clone()
    }
    fn received_reveals(&self, _batch_id: u64) -> Vec<RevealMessage> {
        // Return 2 crossing counterparty orders (1 buy, 1 sell) as BCS-encoded reveals
        let orders = vec![
            make_test_order(101, 1, 5_200_000, 50_000_000_000), // BUY at 5.2
            make_test_order(102, 0, 4_800_000, 50_000_000_000), // SELL at 4.8
        ];
        orders.iter().enumerate().map(|(i, order)| {
            let order_bytes = encode_order(order).unwrap();
            RevealMessage {
                nft_id: 101 + i as u64,
                pool_id: 2,
                batch_id: 100,
                order_bytes,
                signature: vec![0u8; 64],
            }
        }).collect()
    }
}

// ── Mock BatchStatePort ──────────────────────────────────────────────────

struct MockBatchState {
    batch_id: u64,
    n_pools: u64,
}

impl BatchStatePort for MockBatchState {
    fn current_phase(&self) -> Phase {
        Phase::Commit
    }
    fn current_batch_id(&self) -> u64 {
        self.batch_id
    }
    fn current_pool_id(&self, _nft_id: u64) -> u64 {
        2
    }
    fn advance_to_block(&mut self, _block_height: u64) {}
    fn num_pools(&self) -> u64 {
        self.n_pools
    }
    fn set_num_pools(&mut self, n: u64) {
        self.n_pools = n;
    }
    fn reload_params(&mut self, _params: BatchParams) {}
}

// ── Helper ───────────────────────────────────────────────────────────────

async fn connect_client(
    addr: std::net::SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>> {
    let url = format!("ws://{}", addr);
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws
}

async fn recv_json(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
) -> serde_json::Value {
    loop {
        match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                return serde_json::from_str(&text).unwrap();
            }
            Ok(Some(Ok(_))) => continue,
            other => panic!("expected text message, got {:?}", other),
        }
    }
}

// ── T_INT_B5_01: Full batch cycle ────────────────────────────────────────

#[tokio::test]
async fn t_int_b5_01_full_batch_cycle() {
    // 1. Start strategy server
    let server = StrategyServer::start("127.0.0.1:0", "test_token".into(), 42, "testnet".into(), None)
        .await
        .unwrap();

    // 2. Connect strategy client (simulating Python wrapper)
    let mut ws = connect_client(server.addr()).await;

    // 3. Authenticate
    ws.send(Message::Text(
        r#"{"action":"auth","token":"test_token"}"#.into(),
    ))
    .await
    .unwrap();

    let auth_resp = recv_json(&mut ws).await;
    assert_eq!(auth_resp["event"], "auth_ok");
    assert_eq!(auth_resp["data"]["nft_id"], 42);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(server.is_connected());

    // 4. Server sends batch_start event
    let mut escrow = HashMap::new();
    escrow.insert("KAY".to_string(), "500.00000000".to_string());
    escrow.insert("EMM".to_string(), "10000.00000000".to_string());

    server
        .send_event(StrategyEvent::BatchStart {
            data: BatchStartData {
                batch_id: 100,
                pool_id: 2,
                escrow,
                escrow_confirmed: HashMap::new(),
                wallet: HashMap::new(),
                gas_balance: "50.00000000".into(),
                last_batch: None,
                batch_params: BatchParamsData {
                    blocks_per_batch: 10,
                    commits_per_batch: 3,
                    num_pools: 4,
                },
                pending_settlements: vec![],
                peers_in_pool: 5,
                mint_state: MintStateData {
                    state: "AWAITING_TRIGGER".to_string(),
                    hold_duration_secs: 0, period_end: 0, block_end: 0,
                    has_pending_mint: false, pending_claimable_at: 0,
                },
                circulating: HashMap::new(),
                vault_locks: vec![],
                node_health: None,
            },
        })
        .await
        .unwrap();

    // 5. Client receives batch_start
    let batch_start = recv_json(&mut ws).await;
    assert_eq!(batch_start["event"], "batch_start");
    assert_eq!(batch_start["data"]["batch_id"], 100);

    // 6. Client sends commit action (simulating strategy response)
    ws.send(Message::Text(serde_json::json!({
        "action": "commit",
        "orders": [
            {"pair": "EMM/KAY", "side": "buy", "price": "0.04990000", "quantity": "500.00000000"},
            {"pair": "EMM/KAY", "side": "sell", "price": "0.05100000", "quantity": "300.00000000"},
        ]
    }).to_string()))
    .await
    .unwrap();

    // 7. Server receives commit action
    let action = server
        .receive_action_with_timeout(Duration::from_secs(2))
        .await
        .unwrap();
    let order_count = match &action {
        StrategyAction::Commit { orders } => {
            assert_eq!(orders.len(), 2);
            assert_eq!(orders[0].pair, "EMM/KAY");
            assert_eq!(orders[0].side, "buy");
            assert_eq!(orders[1].side, "sell");
            orders.len()
        }
        other => panic!("expected Commit, got {:?}", other),
    };

    // 8. Orchestrator processes commit phase
    let mut orch = Orchestrator::new(
        MockGossip::new(),
        MockBatchState {
            batch_id: 100,
            n_pools: 4,
        },
        42,
        3,
    );
    orch.market_configs = vec![
        MarketConfig { symbol: b"EMM/KAY".to_vec(), min_quantity: 1 },
    ];

    let validated: Vec<ValidatedOrder> = (0..order_count)
        .map(|i| ValidatedOrder {
            commit_hash: vec![i as u8; 32],
            order_bytes: vec![i as u8; 64],
            signature: vec![i as u8; 64],
        })
        .collect();

    let commits = orch.on_commit_phase(true, Some(validated));
    assert_eq!(commits, 2);

    // 9. Reveal phase — strategy reveals all
    let reveals = orch.on_reveal_phase(2, Some(vec![0, 1]));
    assert_eq!(reveals, 2);

    // 10. Match phase — crossing buy/sell from counterparties
    let matches = orch.on_match_phase();
    assert_eq!(matches, 1); // 1 crossing pair: buy@5.2 × sell@4.8

    // 11. Swap phase — settlements to submit
    let settled = orch.on_swap_phase();
    assert_eq!(settled, 1);

    // 12. Settlement confirmed on chain → strategy notified
    let evt = BatchTradeSettledEvent {
        batch_id: 100,
        pool_id: 2,
        trade_id: "0xABC".to_string(),
        buyer_nft_id: 42,
        seller_nft_id: 99,
        symbol: b"EMM/KAY".to_vec(),
        buyer_price: 5_000_000,
        seller_price: 4_900_000,
        clearing_price: 4_950_000,
        base_amount: 50_000_000_000,
        quote_amount: 2_475_000,
        buyer_order_hash: "0xBH".to_string(),
        seller_order_hash: "0xSH".to_string(),
        gas_payer: "0x01".to_string(),
        timestamp: 123,
    };
    orch.on_chain_event(&ChainEvent::BatchTradeSettled(evt));

    // Verify strategy would be notified
    assert_eq!(orch.sent_events.len(), 1);
    match &orch.sent_events[0] {
        StrategyEvent::Settlement { data } => {
            assert_eq!(data.batch_id, 100);
            assert_eq!(data.status, "confirmed");
            assert_eq!(data.side, "buy"); // we are buyer (nft 42)
            assert_eq!(data.pair, "EMM/KAY");
        }
        other => panic!("expected Settlement, got {:?}", other),
    }

    // 13. Forward settlement event to WS client
    server
        .send_event(orch.sent_events[0].clone())
        .await
        .unwrap();

    let settlement_msg = recv_json(&mut ws).await;
    assert_eq!(settlement_msg["event"], "settlement");
    assert_eq!(settlement_msg["data"]["batch_id"], 100);
    assert_eq!(settlement_msg["data"]["status"], "confirmed");
    assert_eq!(settlement_msg["data"]["side"], "buy");
}
