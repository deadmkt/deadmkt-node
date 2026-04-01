use deadmkt_strategy::server::StrategyServer;
use deadmkt_strategy::*;
use std::collections::HashMap;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let server = StrategyServer::start("127.0.0.1:9098", "real_token".into(), 42, "testnet".into(), None)
        .await.unwrap();
    println!("[rust-server] Listening on {}", server.addr());
    
    // Wait for client to connect + auth
    tokio::time::sleep(Duration::from_secs(1)).await;
    
    // Send batch_start
    let mut escrow = HashMap::new();
    escrow.insert("KAY".to_string(), "500.00000000".to_string());
    escrow.insert("EMM".to_string(), "10000.00000000".to_string());
    
    server.send_event(StrategyEvent::BatchStart {
        data: BatchStartData {
            batch_id: 200, pool_id: 3,
            escrow, escrow_confirmed: HashMap::new(),
            wallet: HashMap::new(),
            gas_balance: "50.00000000".into(),
            last_batch: None,
            batch_params: BatchParamsData { blocks_per_batch: 10, commits_per_batch: 3, num_pools: 4 },
            pending_settlements: vec![], peers_in_pool: 8,
            mint_state: MintStateData {
                state: "AWAITING_TRIGGER".to_string(),
                hold_duration_secs: 0, period_end: 0, block_end: 0,
                has_pending_mint: false, pending_claimable_at: 0,
            },
            circulating: HashMap::new(),
            vault_locks: vec![],
            node_health: None,
            min_trade_quantity: "0".to_string(),
        }
    }).await.unwrap();
    println!("[rust-server] Sent batch_start");
    
    // Receive commit action
    let action = server.receive_action_with_timeout(Duration::from_secs(3)).await;
    match action {
        Some(StrategyAction::Commit { orders }) => {
            println!("[rust-server] Got {} orders from strategy", orders.len());
        }
        other => println!("[rust-server] Unexpected: {:?}", other),
    }
    
    // Send settlement
    server.send_event(StrategyEvent::Settlement {
        data: SettlementData {
            batch_id: 200, match_hash: "0xDEF".into(),
            status: "confirmed".into(), pair: "EMM/KAY".into(),
            side: "buy".into(), price: "0.04950000".into(), quantity: "500".into(),
        }
    }).await.unwrap();
    println!("[rust-server] Sent settlement");
    
    tokio::time::sleep(Duration::from_secs(1)).await;
    println!("[rust-server] Done");
}
