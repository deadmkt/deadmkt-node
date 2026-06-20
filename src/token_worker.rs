// src/token_worker.rs
//
// Background task that processes token management actions from the strategy
// WebSocket. Runs continuously, independent of the batch commit cycle.
//
// Actions: Mint, ClaimMint, BurnFromEscrow, BurnForProfit, Lock, Unlock, DonateDust
//
// DMKT11: Auto-deposit removed. Tokens never leave escrow to wallet.
// claim_mint deposits directly to escrow (C1). lock/unlock operate
// escrow<->vault directly. burn operates escrow->burn directly.

use deadmkt_setup::ChainClient;
use deadmkt_strategy::{StrategyAction, StrategyEvent, TokenActionResultData};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Spawn a background tokio task that processes token actions.
///
/// `payout_address` is the off-chain recipient for profit-takeout burns
/// (DMKT14 `burn_for_profit`); the node always signs as the trustee.
pub fn spawn_token_worker(
    client: Arc<dyn ChainClient>,
    rx: mpsc::Receiver<StrategyAction>,
    event_tx: mpsc::Sender<StrategyEvent>,
    tx_lock: Arc<tokio::sync::Mutex<()>>,
    payout_address: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(token_worker_loop(client, rx, event_tx, tx_lock, payout_address))
}

/// Send a TokenActionResult event. Best-effort — if the channel is full, drop it.
async fn notify(tx: &mpsc::Sender<StrategyEvent>, action: &str, success: bool, message: String) {
    let event = StrategyEvent::TokenActionResult {
        data: TokenActionResultData {
            action: action.to_string(),
            success,
            message,
        },
    };
    let _ = tx.send(event).await;
}

async fn token_worker_loop(
    client: Arc<dyn ChainClient>,
    mut rx: mpsc::Receiver<StrategyAction>,
    event_tx: mpsc::Sender<StrategyEvent>,
    tx_lock: Arc<tokio::sync::Mutex<()>>,
    payout_address: String,
) {
    println!("[token_worker] started — listening for token actions");

    while let Some(action) = rx.recv().await {
        // Acquire tx lock to prevent sequence number collisions
        // with the settlement worker (both submit from the same address).
        let _guard = tx_lock.lock().await;
        match action {
            StrategyAction::Mint { m_amount, k_amount, t_amount } => {
                println!("[token_worker] request_mint({}, {}, {})", m_amount, k_amount, t_amount);
                match client.submit_request_mint(m_amount, k_amount, t_amount).await {
                    Ok(r) if r.success => {
                        println!("[token_worker] mint OK (gas={})", r.gas_used);
                        notify(&event_tx, "mint", true, format!("gas={}", r.gas_used)).await;
                    }
                    Ok(r) => {
                        eprintln!("[token_worker] mint FAILED: {}", r.vm_status);
                        notify(&event_tx, "mint", false, r.vm_status).await;
                    }
                    Err(e) => {
                        eprintln!("[token_worker] mint ERROR: {}", e);
                        notify(&event_tx, "mint", false, e.to_string()).await;
                    }
                }
            }

            StrategyAction::ClaimMint => {
                println!("[token_worker] claim_mint");
                match client.submit_claim_mint().await {
                    Ok(r) if r.success => {
                        // C1: claim_mint deposits directly to escrow. No auto-deposit needed.
                        println!("[token_worker] claim OK (gas={})", r.gas_used);
                        notify(&event_tx, "claim_mint", true, format!("gas={}", r.gas_used)).await;
                    }
                    Ok(r) => {
                        eprintln!("[token_worker] claim FAILED: {}", r.vm_status);
                        notify(&event_tx, "claim_mint", false, r.vm_status).await;
                    }
                    Err(e) => {
                        eprintln!("[token_worker] claim ERROR: {}", e);
                        notify(&event_tx, "claim_mint", false, e.to_string()).await;
                    }
                }
            }

            StrategyAction::BurnFromEscrow { amount } => {
                println!("[token_worker] burn_from_escrow({})", amount);
                match client.submit_burn_from_escrow(amount).await {
                    Ok(r) if r.success => {
                        println!("[token_worker] burn_from_escrow OK (gas={})", r.gas_used);
                        notify(&event_tx, "burn_from_escrow", true, format!("gas={}", r.gas_used)).await;
                    }
                    Ok(r) => {
                        eprintln!("[token_worker] burn_from_escrow FAILED: {}", r.vm_status);
                        notify(&event_tx, "burn_from_escrow", false, r.vm_status).await;
                    }
                    Err(e) => {
                        eprintln!("[token_worker] burn_from_escrow ERROR: {}", e);
                        notify(&event_tx, "burn_from_escrow", false, e.to_string()).await;
                    }
                }
            }

            StrategyAction::BurnForProfit { amount } => {
                println!("[token_worker] burn_for_profit({}, recipient={})", amount, payout_address);
                match client.submit_burn_for_profit(amount, payout_address.clone()).await {
                    Ok(r) if r.success => {
                        println!("[token_worker] burn_for_profit OK (gas={})", r.gas_used);
                        notify(&event_tx, "burn_for_profit", true, format!("gas={}", r.gas_used)).await;
                    }
                    Ok(r) => {
                        eprintln!("[token_worker] burn_for_profit FAILED: {}", r.vm_status);
                        notify(&event_tx, "burn_for_profit", false, r.vm_status).await;
                    }
                    Err(e) => {
                        eprintln!("[token_worker] burn_for_profit ERROR: {}", e);
                        notify(&event_tx, "burn_for_profit", false, e.to_string()).await;
                    }
                }
            }

            StrategyAction::Lock { symbol, amount, duration_secs } => {
                println!("[token_worker] lock_from_escrow({}, {}, {}s)", symbol, amount, duration_secs);
                let sym_id = match symbol.as_str() {
                    "EMM" => 0u8,
                    "KAY" => 1u8,
                    "TEE" => 2u8,
                    other => {
                        eprintln!("[token_worker] unknown symbol: {}", other);
                        notify(&event_tx, "lock", false, format!("unknown symbol: {}", other)).await;
                        continue;
                    }
                };
                match client.submit_lock_from_escrow(sym_id, amount, duration_secs).await {
                    Ok(r) if r.success => {
                        println!("[token_worker] lock OK (gas={})", r.gas_used);
                        notify(&event_tx, "lock", true, format!("gas={}", r.gas_used)).await;
                    }
                    Ok(r) => {
                        eprintln!("[token_worker] lock FAILED: {}", r.vm_status);
                        notify(&event_tx, "lock", false, r.vm_status).await;
                    }
                    Err(e) => {
                        eprintln!("[token_worker] lock ERROR: {}", e);
                        notify(&event_tx, "lock", false, e.to_string()).await;
                    }
                }
            }

            StrategyAction::Unlock { lock_index } => {
                println!("[token_worker] unlock_to_escrow({})", lock_index);
                match client.submit_unlock_to_escrow(lock_index).await {
                    Ok(r) if r.success => {
                        println!("[token_worker] unlock OK (gas={})", r.gas_used);
                        notify(&event_tx, "unlock", true, format!("gas={}", r.gas_used)).await;
                    }
                    Ok(r) => {
                        eprintln!("[token_worker] unlock FAILED: {}", r.vm_status);
                        notify(&event_tx, "unlock", false, r.vm_status).await;
                    }
                    Err(e) => {
                        eprintln!("[token_worker] unlock ERROR: {}", e);
                        notify(&event_tx, "unlock", false, e.to_string()).await;
                    }
                }
            }

            StrategyAction::DonateDust { recipient_nft_id, symbol, amount } => {
                println!("[token_worker] donate_dust(nft={}, {}, {})", recipient_nft_id, symbol, amount);
                let sym_id = match symbol.as_str() {
                    "EMM" => 0u8,
                    "KAY" => 1u8,
                    "TEE" => 2u8,
                    other => {
                        eprintln!("[token_worker] unknown symbol: {}", other);
                        notify(&event_tx, "donate_dust", false, format!("unknown symbol: {}", other)).await;
                        continue;
                    }
                };
                match client.submit_donate_dust(recipient_nft_id, sym_id, amount).await {
                    Ok(r) if r.success => {
                        println!("[token_worker] donate_dust OK (gas={})", r.gas_used);
                        notify(&event_tx, "donate_dust", true, format!("gas={}", r.gas_used)).await;
                    }
                    Ok(r) => {
                        eprintln!("[token_worker] donate_dust FAILED: {}", r.vm_status);
                        notify(&event_tx, "donate_dust", false, r.vm_status).await;
                    }
                    Err(e) => {
                        eprintln!("[token_worker] donate_dust ERROR: {}", e);
                        notify(&event_tx, "donate_dust", false, e.to_string()).await;
                    }
                }
            }

            // Batch-cycle actions should never arrive here (routing error).
            other => {
                eprintln!("[token_worker] unexpected action: {:?}", other);
            }
        }
        drop(_guard); // Release tx lock before waiting for next action
    }

    println!("[token_worker] channel closed — exiting");
}
