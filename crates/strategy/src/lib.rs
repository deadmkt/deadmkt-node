// =========================================================================
// deadmkt-strategy: Strategy protocol types + conversion + validation
// =========================================================================
//
// Phase 1: Pure logic — events, actions, auth, conversion, validation.
// Phase 2 will add: server.rs (WebSocket), context.rs (batch context builder).

pub mod convert;
pub mod server;

use serde_json::Value;
use std::collections::HashMap;

// =========================================================================
// Errors
// =========================================================================

#[derive(Debug, thiserror::Error)]
pub enum StrategyError {
    #[error("parse error: {0}")]
    ParseError(String),

    #[error("auth failed: {0}")]
    AuthFailed(String),

    #[error("invalid price: {0}")]
    InvalidPrice(String),

    #[error("invalid quantity: {0}")]
    InvalidQuantity(String),

    #[error("invalid side: {0}")]
    InvalidSide(String),

    #[error("unknown pair: {0}")]
    UnknownPair(String),

    #[error("validation error: {0}")]
    ValidationError(String),
}

// =========================================================================
// Strategy Events (node → strategy) — 24 variants
// =========================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum StrategyEvent {
    AuthOk { nft_id: u64, network: String, auth_details: Option<AuthOkDetails> },
    AuthFailed { reason: String },
    BatchStart { data: BatchStartData },
    RevealStart { data: RevealStartData },
    MatchResult { data: MatchResultData },
    Settlement { data: SettlementData },
    SettlementFailed { data: SettlementFailedData },
    ProfitTransfer { data: ProfitTransferData },
    RushedWithdrawalRequested { data: WithdrawalRequestedData },
    WithdrawalCancelled { data: WithdrawalCancelledData },
    HoldingPeriodStarted { data: HoldingPeriodData },
    HoldingPeriodCancelled,
    InactiveDetected { data: InactiveData },
    Reactivated { data: ReactivatedData },
    HoldExpiryApproaching { data: HoldExpiryData },
    Paused { data: PausedData },
    Resumed { data: ResumedData },
    ParamChangeQueued { data: ParamChangeData },
    ParamChangeApplied { data: ParamChangeData },
    ParamChangeCancelled { data: ParamChangeData },
    PoolAdjusted { data: PoolAdjustedData },
    MarketAdded { data: MarketAddedData },
    MarketDeactivated { data: MarketDeactivatedData },
    TokenActionResult { data: TokenActionResultData },
    Disconnected { reason: String },
}

// ── Event data structs ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct AuthOkDetails {
    pub trustee_address: String,
    pub beneficiary_address: String,
    pub markets: Vec<String>,
    pub token_decimals: HashMap<String, u8>,
    pub price_decimals: u8,
    pub contract_address: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchStartData {
    pub batch_id: u64,
    pub pool_id: u64,
    pub escrow: HashMap<String, String>,
    pub escrow_confirmed: HashMap<String, String>,
    pub wallet: HashMap<String, String>,
    pub gas_balance: String,
    pub last_batch: Option<LastBatchData>,
    pub batch_params: BatchParamsData,
    pub pending_settlements: Vec<PendingSettlementData>,
    pub peers_in_pool: u64,
    pub mint_state: MintStateData,
    pub circulating: HashMap<String, String>,
    pub vault_locks: Vec<VaultLockData>,
    pub node_health: Option<NodeHealthData>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeHealthData {
    pub gas_status: String,
    pub gossip_connected: bool,
    pub gossip_peers: u64,
    pub settle_pending_count: u64,
    pub settle_failed_recent: u64,
    pub uptime_batches: u64,
    pub block_height: u64,
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MintStateData {
    pub state: String,          // "AWAITING_TRIGGER", "VDRF_PENDING", "OPEN", "BLOCKED"
    pub hold_duration_secs: u64,
    pub period_end: u64,
    pub block_end: u64,
    pub has_pending_mint: bool,
    pub pending_claimable_at: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VaultLockData {
    pub symbol: String,
    pub amount: String,
    pub unlock_at: u64,
    pub claimed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LastBatchData {
    pub batch_id: u64,
    pub matches: u64,
    pub volume: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchParamsData {
    pub blocks_per_batch: u64,
    pub commits_per_batch: u32,
    pub num_pools: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingSettlementData {
    pub match_hash: String,
    pub batch_id: u64,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevealStartData {
    pub batch_id: u64,
    pub my_commits: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchResultData {
    pub batch_id: u64,
    pub matches: Vec<MatchSummary>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchSummary {
    pub pair: String,
    pub side: String,
    pub price: String,
    pub quantity: String,
    pub counterparty_nft_id: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettlementData {
    pub batch_id: u64,
    pub match_hash: String,
    pub status: String,
    pub pair: String,
    pub side: String,
    pub price: String,
    pub quantity: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettlementFailedData {
    pub batch_id: u64,
    pub match_hash: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProfitTransferData {
    pub token: String,
    pub amount: String,
    pub tx_hash: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WithdrawalRequestedData {
    pub token: String,
    pub amount: String,
    pub nft_id: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WithdrawalCancelledData {
    pub token: String,
    pub amount: String,
    pub nft_id: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HoldingPeriodData {
    pub expires_at_batch: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InactiveData {
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReactivatedData {
    pub batch_id: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HoldExpiryData {
    pub expires_at_batch: u64,
    pub batches_remaining: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PausedData {
    pub after_batch: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResumedData {
    pub batch_id: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParamChangeData {
    pub param_name: String,
    pub old_value: String,
    pub new_value: String,
    pub effective_at_batch: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoolAdjustedData {
    pub old_num_pools: u64,
    pub new_num_pools: u64,
    pub effective_at_batch: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MarketAddedData {
    pub symbol: String,
    pub min_quantity: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MarketDeactivatedData {
    pub symbol: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TokenActionResultData {
    pub action: String,     // "mint", "claim_mint", "burn", "lock", "unlock"
    pub success: bool,
    pub message: String,    // gas used on success, error message on failure
}

// ── Event serialization ──────────────────────────────────────────────────

impl StrategyEvent {
    /// Event type name for JSON "event" field (snake_case).
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::AuthOk { .. } => "auth_ok",
            Self::AuthFailed { .. } => "auth_failed",
            Self::BatchStart { .. } => "batch_start",
            Self::RevealStart { .. } => "reveal_start",
            Self::MatchResult { .. } => "match_result",
            Self::Settlement { .. } => "settlement",
            Self::SettlementFailed { .. } => "settlement_failed",
            Self::ProfitTransfer { .. } => "profit_transfer",
            Self::RushedWithdrawalRequested { .. } => "rushed_withdrawal_requested",
            Self::WithdrawalCancelled { .. } => "withdrawal_cancelled",
            Self::HoldingPeriodStarted { .. } => "holding_period_started",
            Self::HoldingPeriodCancelled => "holding_period_cancelled",
            Self::InactiveDetected { .. } => "inactive_detected",
            Self::Reactivated { .. } => "reactivated",
            Self::HoldExpiryApproaching { .. } => "hold_expiry_approaching",
            Self::Paused { .. } => "paused",
            Self::Resumed { .. } => "resumed",
            Self::ParamChangeQueued { .. } => "param_change_queued",
            Self::ParamChangeApplied { .. } => "param_change_applied",
            Self::ParamChangeCancelled { .. } => "param_change_cancelled",
            Self::PoolAdjusted { .. } => "pool_adjusted",
            Self::MarketAdded { .. } => "market_added",
            Self::MarketDeactivated { .. } => "market_deactivated",
            Self::TokenActionResult { .. } => "token_action_result",
            Self::Disconnected { .. } => "disconnected",
        }
    }

    /// Serialize event to JSON Value: {"event": "...", "data": {...}}
    pub fn to_json(&self) -> Value {
        let data = match self {
            Self::AuthOk { nft_id, network, auth_details } => {
                let mut obj = serde_json::json!({
                    "nft_id": nft_id,
                    "network": network,
                });
                if let Some(details) = auth_details {
                    obj["trustee_address"] = serde_json::json!(details.trustee_address);
                    obj["beneficiary_address"] = serde_json::json!(details.beneficiary_address);
                    obj["markets"] = serde_json::json!(details.markets);
                    obj["token_decimals"] = serde_json::json!(details.token_decimals);
                    obj["price_decimals"] = serde_json::json!(details.price_decimals);
                    obj["contract_address"] = serde_json::json!(details.contract_address);
                }
                obj
            },
            Self::AuthFailed { reason } => serde_json::json!({
                "reason": reason,
            }),
            Self::BatchStart { data } => {
                let mut obj = serde_json::json!({
                    "batch_id": data.batch_id,
                    "pool_id": data.pool_id,
                    "escrow": data.escrow,
                    "escrow_confirmed": data.escrow_confirmed,
                    "wallet": data.wallet,
                    "gas_balance": data.gas_balance,
                    "batch_params": {
                        "blocks_per_batch": data.batch_params.blocks_per_batch,
                        "commits_per_batch": data.batch_params.commits_per_batch,
                        "num_pools": data.batch_params.num_pools,
                    },
                    "pending_settlements": data.pending_settlements.iter().map(|p| {
                        serde_json::json!({
                            "match_hash": p.match_hash,
                            "batch_id": p.batch_id,
                            "status": p.status,
                        })
                    }).collect::<Vec<_>>(),
                    "peers_in_pool": data.peers_in_pool,
                    "mint_state": {
                        "state": data.mint_state.state,
                        "hold_duration_secs": data.mint_state.hold_duration_secs,
                        "period_end": data.mint_state.period_end,
                        "block_end": data.mint_state.block_end,
                        "has_pending_mint": data.mint_state.has_pending_mint,
                        "pending_claimable_at": data.mint_state.pending_claimable_at,
                    },
                    "circulating": data.circulating,
                    "vault_locks": data.vault_locks.iter().map(|l| {
                        serde_json::json!({
                            "symbol": l.symbol,
                            "amount": l.amount,
                            "unlock_at": l.unlock_at,
                            "claimed": l.claimed,
                        })
                    }).collect::<Vec<_>>(),
                });
                if let Some(lb) = &data.last_batch {
                    obj["last_batch"] = serde_json::json!({
                        "batch_id": lb.batch_id,
                        "matches": lb.matches,
                        "volume": lb.volume,
                    });
                }
                if let Some(nh) = &data.node_health {
                    obj["node_health"] = serde_json::json!({
                        "gas_status": nh.gas_status,
                        "gossip_connected": nh.gossip_connected,
                        "gossip_peers": nh.gossip_peers,
                        "settle_pending_count": nh.settle_pending_count,
                        "settle_failed_recent": nh.settle_failed_recent,
                        "uptime_batches": nh.uptime_batches,
                        "block_height": nh.block_height,
                        "timestamp": nh.timestamp,
                    });
                }
                obj
            }
            Self::RevealStart { data } => serde_json::json!({
                "batch_id": data.batch_id,
                "my_commits": data.my_commits,
            }),
            Self::MatchResult { data } => serde_json::json!({
                "batch_id": data.batch_id,
                "matches": data.matches.iter().map(|m| serde_json::json!({
                    "pair": m.pair,
                    "side": m.side,
                    "price": m.price,
                    "quantity": m.quantity,
                    "counterparty_nft_id": m.counterparty_nft_id,
                })).collect::<Vec<_>>(),
            }),
            Self::Settlement { data } => serde_json::json!({
                "batch_id": data.batch_id,
                "match_hash": data.match_hash,
                "status": data.status,
                "pair": data.pair,
                "side": data.side,
                "price": data.price,
                "quantity": data.quantity,
            }),
            Self::SettlementFailed { data } => serde_json::json!({
                "batch_id": data.batch_id,
                "match_hash": data.match_hash,
                "reason": data.reason,
            }),
            Self::ProfitTransfer { data } => serde_json::json!({
                "token": data.token,
                "amount": data.amount,
                "tx_hash": data.tx_hash,
            }),
            Self::RushedWithdrawalRequested { data } => serde_json::json!({
                "token": data.token,
                "amount": data.amount,
                "nft_id": data.nft_id,
            }),
            Self::WithdrawalCancelled { data } => serde_json::json!({
                "token": data.token,
                "amount": data.amount,
                "nft_id": data.nft_id,
            }),
            Self::HoldingPeriodStarted { data } => serde_json::json!({
                "expires_at_batch": data.expires_at_batch,
            }),
            Self::HoldingPeriodCancelled => serde_json::json!({}),
            Self::InactiveDetected { data } => serde_json::json!({
                "reason": data.reason,
            }),
            Self::Reactivated { data } => serde_json::json!({
                "batch_id": data.batch_id,
            }),
            Self::HoldExpiryApproaching { data } => serde_json::json!({
                "expires_at_batch": data.expires_at_batch,
                "batches_remaining": data.batches_remaining,
            }),
            Self::Paused { data } => serde_json::json!({
                "after_batch": data.after_batch,
            }),
            Self::Resumed { data } => serde_json::json!({
                "batch_id": data.batch_id,
            }),
            Self::ParamChangeQueued { data }
            | Self::ParamChangeApplied { data }
            | Self::ParamChangeCancelled { data } => {
                let mut obj = serde_json::json!({
                    "param_name": data.param_name,
                    "old_value": data.old_value,
                    "new_value": data.new_value,
                });
                if let Some(eab) = data.effective_at_batch {
                    obj["effective_at_batch"] = serde_json::json!(eab);
                }
                obj
            }
            Self::PoolAdjusted { data } => {
                let mut obj = serde_json::json!({
                    "old_num_pools": data.old_num_pools,
                    "new_num_pools": data.new_num_pools,
                });
                if let Some(eab) = data.effective_at_batch {
                    obj["effective_at_batch"] = serde_json::json!(eab);
                }
                obj
            }
            Self::MarketAdded { data } => serde_json::json!({
                "symbol": data.symbol,
                "min_quantity": data.min_quantity,
            }),
            Self::MarketDeactivated { data } => serde_json::json!({
                "symbol": data.symbol,
            }),
            Self::TokenActionResult { data } => serde_json::json!({
                "action": data.action,
                "success": data.success,
                "message": data.message,
            }),
            Self::Disconnected { reason } => serde_json::json!({
                "reason": reason,
            }),
        };

        serde_json::json!({
            "event": self.event_name(),
            "data": data,
        })
    }
}

// =========================================================================
// Strategy Actions (strategy → node) — 3 variants
// =========================================================================

#[derive(Debug, Clone, PartialEq)]
pub struct OrderSpec {
    pub pair: String,
    pub side: String,
    pub price: String,
    pub quantity: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StrategyAction {
    Auth { token: String },
    Commit { orders: Vec<OrderSpec> },
    Reveal { reveal_indices: Vec<u32> },
    // Token actions (out-of-band, not part of batch cycle)
    Mint { m_amount: u64, k_amount: u64, t_amount: u64 },
    ClaimMint,
    Burn { amount: u64 },
    Lock { symbol: String, amount: u64, duration_secs: u64 },
    Unlock { lock_index: u64 },
}

impl StrategyAction {
    /// Returns true if this is a token management action (mint/burn/lock/unlock).
    /// These are handled by the background token worker, not the batch commit handler.
    pub fn is_token_action(&self) -> bool {
        matches!(self,
            StrategyAction::Mint { .. } |
            StrategyAction::ClaimMint |
            StrategyAction::Burn { .. } |
            StrategyAction::Lock { .. } |
            StrategyAction::Unlock { .. }
        )
    }

    /// Parse a StrategyAction from JSON Value.
    pub fn from_json(json: &Value) -> Result<Self, StrategyError> {
        let action = json.get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| StrategyError::ParseError("missing 'action' field".into()))?;

        match action {
            "auth" => {
                let token = json.get("token")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| StrategyError::ParseError("missing 'token' field".into()))?;
                Ok(StrategyAction::Auth { token: token.to_string() })
            }
            "commit" => {
                let orders_arr = json.get("orders")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| StrategyError::ParseError("missing 'orders' array".into()))?;
                let mut orders = Vec::new();
                for o in orders_arr {
                    orders.push(OrderSpec {
                        pair: o.get("pair").and_then(|v| v.as_str())
                            .unwrap_or("").to_string(),
                        side: o.get("side").and_then(|v| v.as_str())
                            .unwrap_or("").to_string(),
                        price: o.get("price").and_then(|v| v.as_str())
                            .unwrap_or("").to_string(),
                        quantity: o.get("quantity").and_then(|v| v.as_str())
                            .unwrap_or("").to_string(),
                    });
                }
                Ok(StrategyAction::Commit { orders })
            }
            "reveal" => {
                let indices = json.get("reveal_indices")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| StrategyError::ParseError("missing 'reveal_indices'".into()))?;
                let reveal_indices: Vec<u32> = indices.iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u32))
                    .collect();
                Ok(StrategyAction::Reveal { reveal_indices })
            }
            "mint" => {
                let m = json.get("m").and_then(|v| v.as_u64())
                    .ok_or_else(|| StrategyError::ParseError("missing 'm' field".into()))?;
                let k = json.get("k").and_then(|v| v.as_u64())
                    .ok_or_else(|| StrategyError::ParseError("missing 'k' field".into()))?;
                let t = json.get("t").and_then(|v| v.as_u64())
                    .ok_or_else(|| StrategyError::ParseError("missing 't' field".into()))?;
                Ok(StrategyAction::Mint { m_amount: m, k_amount: k, t_amount: t })
            }
            "claim_mint" => Ok(StrategyAction::ClaimMint),
            "burn" => {
                let amount = json.get("amount").and_then(|v| v.as_u64())
                    .ok_or_else(|| StrategyError::ParseError("missing 'amount' field".into()))?;
                Ok(StrategyAction::Burn { amount })
            }
            "lock" => {
                let symbol = json.get("symbol").and_then(|v| v.as_str())
                    .ok_or_else(|| StrategyError::ParseError("missing 'symbol' field".into()))?;
                let amount = json.get("amount").and_then(|v| v.as_u64())
                    .ok_or_else(|| StrategyError::ParseError("missing 'amount' field".into()))?;
                let duration = json.get("duration_secs").and_then(|v| v.as_u64())
                    .ok_or_else(|| StrategyError::ParseError("missing 'duration_secs' field".into()))?;
                Ok(StrategyAction::Lock {
                    symbol: symbol.to_string(), amount, duration_secs: duration,
                })
            }
            "unlock" => {
                let index = json.get("index").and_then(|v| v.as_u64())
                    .ok_or_else(|| StrategyError::ParseError("missing 'index' field".into()))?;
                Ok(StrategyAction::Unlock { lock_index: index })
            }
            other => Err(StrategyError::ParseError(format!("unknown action: {}", other))),
        }
    }
}

// =========================================================================
// Auth validation
// =========================================================================

/// Validate an auth token against the expected token.
pub fn validate_auth(token: &str, expected: &str) -> Result<(), StrategyError> {
    if token == expected {
        Ok(())
    } else {
        Err(StrategyError::AuthFailed("invalid token".into()))
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── T_STRAT_01: BatchStart serializes with event name + decimal strings

    #[test]
    fn t_strat_01_batch_start_to_json() {
        let mut escrow = HashMap::new();
        escrow.insert("KAY".to_string(), "500.00000000".to_string());
        escrow.insert("EMM".to_string(), "10000.00000000".to_string());

        let mut escrow_confirmed = HashMap::new();
        escrow_confirmed.insert("KAY".to_string(), "500.00000000".to_string());

        let event = StrategyEvent::BatchStart {
            data: BatchStartData {
                batch_id: 100,
                pool_id: 2,
                escrow,
                escrow_confirmed,
                wallet: {
                    let mut w = HashMap::new();
                    w.insert("EMM".to_string(), "100.00000".to_string());
                    w.insert("KAY".to_string(), "50.00000".to_string());
                    w.insert("TEE".to_string(), "0.00000".to_string());
                    w
                },
                gas_balance: "50.00000000".to_string(),
                last_batch: Some(LastBatchData {
                    batch_id: 99,
                    matches: 3,
                    volume: "1500.00000000".to_string(),
                }),
                batch_params: BatchParamsData {
                    blocks_per_batch: 10,
                    commits_per_batch: 3,
                    num_pools: 4,
                },
                pending_settlements: vec![
                    PendingSettlementData {
                        match_hash: "0xabc".to_string(),
                        batch_id: 98,
                        status: "submitted".to_string(),
                    },
                ],
                peers_in_pool: 7,
                mint_state: MintStateData {
                    state: "OPEN".to_string(),
                    hold_duration_secs: 259200,
                    period_end: 1772900000,
                    block_end: 0,
                    has_pending_mint: false,
                    pending_claimable_at: 0,
                },
                circulating: {
                    let mut c = HashMap::new();
                    c.insert("EMM".to_string(), "150000.00000".to_string());
                    c.insert("KAY".to_string(), "148000.00000".to_string());
                    c.insert("TEE".to_string(), "152000.00000".to_string());
                    c
                },
                vault_locks: vec![],
                node_health: None,
            },
        };

        let json = event.to_json();
        assert_eq!(json["event"], "batch_start");
        assert_eq!(json["data"]["batch_id"], 100);
        assert_eq!(json["data"]["pool_id"], 2);
        assert_eq!(json["data"]["escrow"]["KAY"], "500.00000000");
        assert_eq!(json["data"]["gas_balance"], "50.00000000");
        assert_eq!(json["data"]["last_batch"]["batch_id"], 99);
        assert_eq!(json["data"]["batch_params"]["commits_per_batch"], 3);
        assert_eq!(json["data"]["pending_settlements"][0]["status"], "submitted");
        assert_eq!(json["data"]["peers_in_pool"], 7);
        // New Trippples fields
        assert_eq!(json["data"]["wallet"]["EMM"], "100.00000");
        assert_eq!(json["data"]["wallet"]["TEE"], "0.00000");
        assert_eq!(json["data"]["mint_state"]["state"], "OPEN");
        assert_eq!(json["data"]["mint_state"]["hold_duration_secs"], 259200);
        assert_eq!(json["data"]["mint_state"]["has_pending_mint"], false);
        assert_eq!(json["data"]["circulating"]["EMM"], "150000.00000");
        assert_eq!(json["data"]["circulating"]["KAY"], "148000.00000");
        assert!(json["data"]["vault_locks"].as_array().unwrap().is_empty());
    }

    // ── T_STRAT_02: AuthOk serializes correctly

    #[test]
    fn t_strat_02_auth_ok_to_json() {
        let event = StrategyEvent::AuthOk {
            nft_id: 42,
            network: "testnet".to_string(),
            auth_details: None,
        };
        let json = event.to_json();
        assert_eq!(json["event"], "auth_ok");
        assert_eq!(json["data"]["nft_id"], 42);
        assert_eq!(json["data"]["network"], "testnet");
    }

    // ── T_STRAT_03: ParamChangeApplied serializes

    #[test]
    fn t_strat_03_param_change_applied_to_json() {
        let event = StrategyEvent::ParamChangeApplied {
            data: ParamChangeData {
                param_name: "commits_per_batch".to_string(),
                old_value: "3".to_string(),
                new_value: "5".to_string(),
                effective_at_batch: Some(200),
            },
        };
        let json = event.to_json();
        assert_eq!(json["event"], "param_change_applied");
        assert_eq!(json["data"]["param_name"], "commits_per_batch");
        assert_eq!(json["data"]["old_value"], "3");
        assert_eq!(json["data"]["new_value"], "5");
        assert_eq!(json["data"]["effective_at_batch"], 200);
    }

    // ── T_STRAT_04: Governance events serialize (PoolAdjusted, MarketAdded, MarketDeactivated)

    #[test]
    fn t_strat_04_governance_events_to_json() {
        // PoolAdjusted
        let pool = StrategyEvent::PoolAdjusted {
            data: PoolAdjustedData {
                old_num_pools: 4,
                new_num_pools: 8,
                effective_at_batch: Some(500),
            },
        };
        let json = pool.to_json();
        assert_eq!(json["event"], "pool_adjusted");
        assert_eq!(json["data"]["old_num_pools"], 4);
        assert_eq!(json["data"]["new_num_pools"], 8);

        // MarketAdded
        let market = StrategyEvent::MarketAdded {
            data: MarketAddedData {
                symbol: "TEE/EMM".to_string(),
                min_quantity: "100.00000000".to_string(),
            },
        };
        let json = market.to_json();
        assert_eq!(json["event"], "market_added");
        assert_eq!(json["data"]["symbol"], "TEE/EMM");

        // MarketDeactivated
        let deactivated = StrategyEvent::MarketDeactivated {
            data: MarketDeactivatedData {
                symbol: "OLD/PAIR".to_string(),
            },
        };
        let json = deactivated.to_json();
        assert_eq!(json["event"], "market_deactivated");
        assert_eq!(json["data"]["symbol"], "OLD/PAIR");
    }

    // ── T_STRAT_05: Auth action parses

    #[test]
    fn t_strat_05_auth_action_from_json() {
        let json = serde_json::json!({"action": "auth", "token": "abc123"});
        let action = StrategyAction::from_json(&json).unwrap();
        assert_eq!(action, StrategyAction::Auth { token: "abc123".to_string() });
    }

    // ── T_STRAT_06: Commit action parses with orders

    #[test]
    fn t_strat_06_commit_action_from_json() {
        let json = serde_json::json!({
            "action": "commit",
            "orders": [
                {"pair": "EMM/KAY", "side": "buy", "price": "0.04990000", "quantity": "500.00000000"},
                {"pair": "EMM/KAY", "side": "sell", "price": "0.05100000", "quantity": "300.00000000"},
            ]
        });
        let action = StrategyAction::from_json(&json).unwrap();
        match action {
            StrategyAction::Commit { orders } => {
                assert_eq!(orders.len(), 2);
                assert_eq!(orders[0].pair, "EMM/KAY");
                assert_eq!(orders[0].side, "buy");
                assert_eq!(orders[0].price, "0.04990000");
                assert_eq!(orders[1].side, "sell");
            }
            _ => panic!("expected Commit"),
        }
    }

    // ── T_STRAT_07: Reveal action parses

    #[test]
    fn t_strat_07_reveal_action_from_json() {
        let json = serde_json::json!({"action": "reveal", "reveal_indices": [0, 1]});
        let action = StrategyAction::from_json(&json).unwrap();
        assert_eq!(action, StrategyAction::Reveal { reveal_indices: vec![0, 1] });
    }

    // ── T_STRAT_08: Garbage JSON → ParseError

    #[test]
    fn t_strat_08_garbage_json_parse_error() {
        let json = serde_json::json!({"garbage": true});
        let result = StrategyAction::from_json(&json);
        assert!(result.is_err());
        match result.unwrap_err() {
            StrategyError::ParseError(_) => {}
            other => panic!("expected ParseError, got {:?}", other),
        }
    }

    // ── T_STRAT_08b: Mint action parses

    #[test]
    fn t_strat_08b_mint_action_from_json() {
        let json = serde_json::json!({"action": "mint", "m": 33000000, "k": 33000000, "t": 33000000});
        let action = StrategyAction::from_json(&json).unwrap();
        assert_eq!(action, StrategyAction::Mint { m_amount: 33000000, k_amount: 33000000, t_amount: 33000000 });
    }

    // ── T_STRAT_08c: Claim mint action parses

    #[test]
    fn t_strat_08c_claim_mint_action_from_json() {
        let json = serde_json::json!({"action": "claim_mint"});
        let action = StrategyAction::from_json(&json).unwrap();
        assert_eq!(action, StrategyAction::ClaimMint);
    }

    // ── T_STRAT_08d: Burn action parses

    #[test]
    fn t_strat_08d_burn_action_from_json() {
        let json = serde_json::json!({"action": "burn", "amount": 10000000});
        let action = StrategyAction::from_json(&json).unwrap();
        assert_eq!(action, StrategyAction::Burn { amount: 10000000 });
    }

    // ── T_STRAT_08e: Lock action parses

    #[test]
    fn t_strat_08e_lock_action_from_json() {
        let json = serde_json::json!({"action": "lock", "symbol": "EMM", "amount": 50000000, "duration_secs": 86400});
        let action = StrategyAction::from_json(&json).unwrap();
        assert_eq!(action, StrategyAction::Lock { symbol: "EMM".to_string(), amount: 50000000, duration_secs: 86400 });
    }

    // ── T_STRAT_08f: Unlock action parses

    #[test]
    fn t_strat_08f_unlock_action_from_json() {
        let json = serde_json::json!({"action": "unlock", "index": 2});
        let action = StrategyAction::from_json(&json).unwrap();
        assert_eq!(action, StrategyAction::Unlock { lock_index: 2 });
    }

    // ── T_STRAT_08g: Mint missing field → ParseError

    #[test]
    fn t_strat_08g_mint_missing_field() {
        let json = serde_json::json!({"action": "mint", "m": 100, "k": 100});
        assert!(StrategyAction::from_json(&json).is_err());
    }

    // ── T_STRAT_08h: Lock missing symbol → ParseError

    #[test]
    fn t_strat_08h_lock_missing_symbol() {
        let json = serde_json::json!({"action": "lock", "amount": 100, "duration_secs": 3600});
        assert!(StrategyAction::from_json(&json).is_err());
    }

    // ── T_STRAT_08i: is_token_action routing

    #[test]
    fn t_strat_08i_is_token_action() {
        assert!(!StrategyAction::Auth { token: "x".into() }.is_token_action());
        assert!(!StrategyAction::Commit { orders: vec![] }.is_token_action());
        assert!(!StrategyAction::Reveal { reveal_indices: vec![] }.is_token_action());
        assert!(StrategyAction::Mint { m_amount: 1, k_amount: 1, t_amount: 1 }.is_token_action());
        assert!(StrategyAction::ClaimMint.is_token_action());
        assert!(StrategyAction::Burn { amount: 1 }.is_token_action());
        assert!(StrategyAction::Lock { symbol: "EMM".into(), amount: 1, duration_secs: 1 }.is_token_action());
        assert!(StrategyAction::Unlock { lock_index: 0 }.is_token_action());
    }

    // ── T_STRAT_09: validate_auth correct token

    #[test]
    fn t_strat_09_validate_auth_correct() {
        assert!(validate_auth("secret123", "secret123").is_ok());
    }

    // ── T_STRAT_10: validate_auth wrong token

    #[test]
    fn t_strat_10_validate_auth_wrong() {
        let result = validate_auth("wrong", "secret123");
        assert!(result.is_err());
        match result.unwrap_err() {
            StrategyError::AuthFailed(_) => {}
            other => panic!("expected AuthFailed, got {:?}", other),
        }
    }
}
