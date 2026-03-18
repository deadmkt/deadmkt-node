// src/token_worker.rs
//
// Background task that processes token management actions from the strategy
// WebSocket. Runs continuously, independent of the batch commit cycle.
//
// Actions: Mint, ClaimMint, Burn, Lock, Unlock
// Each action submits a chain transaction via SupraSetupClient and sends
// a TokenActionResult event back to the strategy over WebSocket.
//
// B5_9.1: After ClaimMint succeeds, auto-deposit all wallet tokens into
// escrow. This closes the mint→trade loop without needing the setup wizard.

use deadmkt_setup::ChainClient;
use deadmkt_strategy::{StrategyAction, StrategyEvent, TokenActionResultData};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Spawn a background tokio task that processes token actions.
///
/// Reads `StrategyAction`s from `rx` (only token actions arrive here —
/// the strategy server routes them to a separate channel).
///
/// Uses `client` (which implements `ChainClient`) to submit transactions.
/// Sends `TokenActionResult` events back via `event_tx` so the strategy
/// knows whether its action succeeded or failed.
pub fn spawn_token_worker(
    client: Arc<dyn ChainClient>,
    rx: mpsc::Receiver<StrategyAction>,
    event_tx: mpsc::Sender<StrategyEvent>,
    tx_lock: Arc<tokio::sync::Mutex<()>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(token_worker_loop(client, rx, event_tx, tx_lock))
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

/// Token symbol indices: EMM=0, KAY=1, TEE=2
const TOKEN_SYMBOLS: [(u8, &str); 3] = [(0, "EMM"), (1, "KAY"), (2, "TEE")];

/// After a successful claim_mint, check wallet balances and deposit any
/// tokens into escrow. This reactivates inactive NFTs (deposit updates
/// last_activity_at on-chain) and makes tokens available for trading.
///
/// Called while holding tx_lock, so sequence numbers won't collide with
/// settlements or heartbeats.
async fn auto_deposit_wallet_to_escrow(
    client: &Arc<dyn ChainClient>,
    event_tx: &mpsc::Sender<StrategyEvent>,
) {
    println!("[token_worker] auto-deposit: checking wallet balances...");

    for (sym_id, sym_name) in &TOKEN_SYMBOLS {
        // 1. Resolve metadata address for this token
        let metadata_addr = match client.get_trippples_metadata(*sym_id).await {
            Ok(addr) => addr,
            Err(e) => {
                eprintln!("[token_worker] auto-deposit: failed to get {} metadata: {}", sym_name, e);
                continue;
            }
        };

        // 2. Check wallet balance
        let balance = match client.get_fa_balance(&metadata_addr).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[token_worker] auto-deposit: failed to get {} balance: {}", sym_name, e);
                continue;
            }
        };

        if balance == 0 {
            println!("[token_worker] auto-deposit: {} wallet=0, skip", sym_name);
            continue;
        }

        // 3. Deposit entire wallet balance into escrow
        println!("[token_worker] auto-deposit: {} wallet={}, depositing...", sym_name, balance);
        match client.submit_deposit(&metadata_addr, balance).await {
            Ok(r) if r.success => {
                println!("[token_worker] auto-deposit: {} OK — {} deposited (gas={})",
                    sym_name, balance, r.gas_used);
                notify(event_tx, "auto_deposit", true,
                    format!("{}={} deposited", sym_name, balance)).await;
            }
            Ok(r) => {
                eprintln!("[token_worker] auto-deposit: {} FAILED: {}", sym_name, r.vm_status);
                notify(event_tx, "auto_deposit", false,
                    format!("{} deposit failed: {}", sym_name, r.vm_status)).await;
            }
            Err(e) => {
                eprintln!("[token_worker] auto-deposit: {} ERROR: {}", sym_name, e);
                notify(event_tx, "auto_deposit", false,
                    format!("{} deposit error: {}", sym_name, e)).await;
            }
        }
    }
}

async fn token_worker_loop(
    client: Arc<dyn ChainClient>,
    mut rx: mpsc::Receiver<StrategyAction>,
    event_tx: mpsc::Sender<StrategyEvent>,
    tx_lock: Arc<tokio::sync::Mutex<()>>,
) {
    println!("[token_worker] started — listening for token actions");

    // ── N11: Startup wallet sweep ──
    // On startup, sweep any tokens sitting in the trustee wallet into escrow.
    // Handles legacy state from DMKT8 or earlier versions where claim_mint
    // left tokens in the wallet. Once C1 (atomic claim_mint→deposit) is
    // deployed, this only triggers for migrating old state.
    {
        let _guard = tx_lock.lock().await;
        println!("[token_worker] startup: sweeping wallet → escrow");
        auto_deposit_wallet_to_escrow(&client, &event_tx).await;
    }

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
                        println!("[token_worker] claim OK (gas={})", r.gas_used);
                        notify(&event_tx, "claim_mint", true, format!("gas={}", r.gas_used)).await;

                        // ── B5_9.1: Auto-deposit claimed tokens into escrow ──
                        // Tokens land in the trustee wallet after claim_mint.
                        // Deposit them into escrow so they're available for trading
                        // and to reactivate the NFT (deposit updates last_activity_at).
                        auto_deposit_wallet_to_escrow(&client, &event_tx).await;
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

            StrategyAction::Burn { amount } => {
                println!("[token_worker] burn_mkt({})", amount);
                match client.submit_burn_mkt(amount).await {
                    Ok(r) if r.success => {
                        println!("[token_worker] burn OK (gas={})", r.gas_used);
                        notify(&event_tx, "burn", true, format!("gas={}", r.gas_used)).await;
                    }
                    Ok(r) => {
                        eprintln!("[token_worker] burn FAILED: {}", r.vm_status);
                        notify(&event_tx, "burn", false, r.vm_status).await;
                    }
                    Err(e) => {
                        eprintln!("[token_worker] burn ERROR: {}", e);
                        notify(&event_tx, "burn", false, e.to_string()).await;
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

            StrategyAction::Lock { symbol, amount, duration_secs } => {
                println!("[token_worker] lock_tokens({}, {}, {}s)", symbol, amount, duration_secs);
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
                match client.submit_lock_tokens(sym_id, amount, duration_secs).await {
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
                println!("[token_worker] unlock_tokens({})", lock_index);
                match client.submit_unlock_tokens(lock_index).await {
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

            // Batch-cycle actions should never arrive here (routing error).
            other => {
                eprintln!("[token_worker] unexpected action: {:?}", other);
            }
        }
        drop(_guard); // Release tx lock before waiting for next action
    }

    println!("[token_worker] channel closed — exiting");
}
