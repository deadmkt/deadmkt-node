// On-chain type definitions: Rust representations of Move structs
// for deserializing view function results.
//
// Supra's REST API returns view results as JSON arrays with string-encoded
// u64 values: ["10", "4", "3", "1", "2", "3"] for BatchParams.

use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TypesError {
    #[error("deserialization failed: {0}")]
    DeserializationError(String),

    #[error("missing field at index {0}")]
    MissingField(usize),

    #[error("invalid number format: {0}")]
    InvalidNumber(String),

    #[error("unknown token: {0}")]
    UnknownToken(String),
}

// =========================================================================
// Helper: parse string-encoded u64 from JSON Value
// =========================================================================

fn parse_u64(val: &serde_json::Value, index: usize) -> Result<u64, TypesError> {
    val.as_str()
        .ok_or_else(|| TypesError::MissingField(index))?
        .parse::<u64>()
        .map_err(|e| TypesError::InvalidNumber(e.to_string()))
}

fn parse_obj_u64(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Result<u64, TypesError> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| TypesError::DeserializationError(format!("missing field: {}", key)))?
        .parse::<u64>()
        .map_err(|e| TypesError::InvalidNumber(e.to_string()))
}

fn parse_bool(val: &serde_json::Value, index: usize) -> Result<bool, TypesError> {
    val.as_bool()
        .ok_or_else(|| TypesError::MissingField(index))
}

fn parse_str(val: &serde_json::Value, index: usize) -> Result<String, TypesError> {
    val.as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| TypesError::MissingField(index))
}

fn get_element(arr: &serde_json::Value, index: usize) -> Result<&serde_json::Value, TypesError> {
    arr.get(index).ok_or(TypesError::MissingField(index))
}

// =========================================================================
// BatchParams
// =========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchParams {
    pub blocks_per_batch: u64,
    pub commit_blocks: u64,
    pub reveal_blocks: u64,
    pub match_blocks: u64,
    pub swap_blocks: u64,
    pub commits_per_batch: u64,
}

impl BatchParams {
    pub fn from_view_result(json: &serde_json::Value) -> Result<Self, TypesError> {
        // Supra returns view results in two possible formats:
        //   1. Named object inside array: [{"blocks_per_batch": "10", ...}]
        //   2. Positional array: ["10", "4", "3", "1", "2", "3"]
        // Try named object first (live API), fall back to positional (mock/legacy).

        let obj = json.get(0).and_then(|v| v.as_object());
        if let Some(obj) = obj {
            return Ok(BatchParams {
                blocks_per_batch: parse_obj_u64(obj, "blocks_per_batch")?,
                commit_blocks: parse_obj_u64(obj, "commit_blocks")?,
                reveal_blocks: parse_obj_u64(obj, "reveal_blocks")?,
                match_blocks: parse_obj_u64(obj, "match_blocks")?,
                swap_blocks: parse_obj_u64(obj, "swap_blocks")?,
                commits_per_batch: parse_obj_u64(obj, "commits_per_batch")?,
            });
        }

        // Positional fallback
        Ok(BatchParams {
            blocks_per_batch: parse_u64(get_element(json, 0)?, 0)?,
            commit_blocks: parse_u64(get_element(json, 1)?, 1)?,
            reveal_blocks: parse_u64(get_element(json, 2)?, 2)?,
            match_blocks: parse_u64(get_element(json, 3)?, 3)?,
            swap_blocks: parse_u64(get_element(json, 4)?, 4)?,
            commits_per_batch: parse_u64(get_element(json, 5)?, 5)?,
        })
    }
}

// =========================================================================
// BatchEpoch
// =========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchEpoch {
    pub anchor_block: u64,
    pub anchor_batch_id: u64,
}

impl BatchEpoch {
    pub fn from_view_result(json: &serde_json::Value) -> Result<Self, TypesError> {
        Ok(BatchEpoch {
            anchor_block: parse_u64(get_element(json, 0)?, 0)?,
            anchor_batch_id: parse_u64(get_element(json, 1)?, 1)?,
        })
    }
}

// =========================================================================
// NftConfig (burn-exit rev: bond fields removed; burn_cooldown_seconds + admin)
// Contract view: nft::get_nft_config() -> (u64, address)
// =========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NftConfig {
    pub burn_cooldown_seconds: u64,
    pub admin: String,
}

impl NftConfig {
    pub fn from_view_result(json: &serde_json::Value) -> Result<Self, TypesError> {
        Ok(NftConfig {
            burn_cooldown_seconds: parse_u64(get_element(json, 0)?, 0)?,
            admin: parse_str(get_element(json, 1)?, 1)?,
        })
    }
}

// =========================================================================
// OpsTreasuryConfig (AOE5 calibration constants + automation owner + admin)
// Contract view: ops_treasury::get_*() accessors (7 typed view fns total)
// This struct is constructed by calling each view fn -- there's no single
// "get_treasury_config" view that returns the whole tuple. The struct is
// the node-side aggregate used by the wizard + status JSON.
// =========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpsTreasuryConfig {
    pub mint_fee: u64,
    pub dvrf_top_up_amount: u64,
    pub donation_cap: u64,
    pub decay_per_period: u64,
    pub decay_period_secs: u64,
    pub automation_owner_address: String,
    pub low_balance_threshold: u64,
    pub admin: String,
    pub admin_burned: bool,
}

// =========================================================================
// MarketPair
// =========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketPair {
    pub symbol: Vec<u8>,
    pub base_metadata: String,
    pub quote_metadata: String,
    pub min_quantity: u64,
    pub is_active: bool,
}

impl MarketPair {
    pub fn from_view_result(json: &serde_json::Value) -> Result<Self, TypesError> {
        let symbol_hex = parse_str(get_element(json, 0)?, 0)?;
        let symbol = hex::decode(symbol_hex.trim_start_matches("0x"))
            .map_err(|e| TypesError::DeserializationError(e.to_string()))?;

        Ok(MarketPair {
            symbol,
            base_metadata: parse_str(get_element(json, 1)?, 1)?,
            quote_metadata: parse_str(get_element(json, 2)?, 2)?,
            min_quantity: parse_u64(get_element(json, 3)?, 3)?,
            is_active: parse_bool(get_element(json, 4)?, 4)?,
        })
    }
}

// =========================================================================
// PoolState
// =========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolState {
    pub num_pools: u64,
    pub target_pool_size: u64,
    pub live_nft_count: u64,
    pub min_pool_size: u64,
    pub max_pool_size: u64,
}

impl PoolState {
    pub fn from_view_result(json: &serde_json::Value) -> Result<Self, TypesError> {
        Ok(PoolState {
            num_pools: parse_u64(get_element(json, 0)?, 0)?,
            target_pool_size: parse_u64(get_element(json, 1)?, 1)?,
            live_nft_count: parse_u64(get_element(json, 2)?, 2)?,
            min_pool_size: parse_u64(get_element(json, 3)?, 3)?,
            max_pool_size: parse_u64(get_element(json, 4)?, 4)?,
        })
    }
}

// =========================================================================
// MintBond removed (burn-exit rev): the refundable bond is gone from the
// contract; the mint_fee flows into ops_treasury as a one-way deposit and
// is recovered (with time decay) via exits::execute_burn_pair.
// See planning/specs/individual-nft-burn-exit.md.
// =========================================================================

// =========================================================================
// Token decimals
// =========================================================================

pub fn parse_decimals_result(json: &serde_json::Value) -> Result<u8, TypesError> {
    let val = parse_u64(get_element(json, 0)?, 0)?;
    if val > 255 {
        return Err(TypesError::InvalidNumber(format!("decimals too large: {val}")));
    }
    Ok(val as u8)
}

#[derive(Debug, Clone)]
pub struct TokenDecimalsMap {
    map: HashMap<String, u8>,
}

impl TokenDecimalsMap {
    pub fn new() -> Self {
        Self { map: HashMap::new() }
    }

    pub fn insert(&mut self, symbol: String, decimals: u8) {
        self.map.insert(symbol, decimals);
    }

    pub fn to_smallest(&self, symbol: &str, human: f64) -> u64 {
        let decimals = self.map.get(symbol).copied().unwrap_or(5);
        let factor = 10u64.pow(decimals as u32);
        (human * factor as f64) as u64
    }

    pub fn to_human(&self, symbol: &str, smallest: u64) -> f64 {
        let decimals = self.map.get(symbol).copied().unwrap_or(5);
        let factor = 10u64.pow(decimals as u32);
        smallest as f64 / factor as f64
    }
}

// =========================================================================
// Events
// =========================================================================

#[derive(Debug, Clone, PartialEq)]
pub struct BatchTradeSettledEvent {
    pub batch_id: u64,
    pub pool_id: u64,
    pub trade_id: String,
    pub buyer_nft_id: u64,
    pub seller_nft_id: u64,
    pub symbol: Vec<u8>,
    pub buyer_price: u64,
    pub seller_price: u64,
    pub clearing_price: u64,
    pub base_amount: u64,
    pub quote_amount: u64,
    pub buyer_order_hash: String,
    pub seller_order_hash: String,
    pub gas_payer: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChainEvent {
    // B2/B3 events:
    BatchTradeSettled(BatchTradeSettledEvent),
    MarketPairAdded { symbol: Vec<u8> },
    PauseQueued { after_batch: u64 },
    Unpaused,

    // NFT lifecycle (burn-exit rev):
    // NftPairMinted gains sponsor + cohort_started fields; loses bond_amount/unlock_at.
    // NFTPairBurned removed (burn_pair deleted from contract).
    // MintBondReclaimed removed (bond deleted from contract).
    NftPairMinted {
        nft_id: u64,
        sponsor: String,
        mint_fee_paid: u64,
        cohort_started: bool,
    },

    // Burn-exit events (from exits.move):
    BurnRequested {
        nft_id: u64,
        beneficiary: String,
        requested_at_batch: u64,
    },
    BurnExecuted {
        nft_id: u64,
        executor: String,
        sponsor: String,
        beneficiary: String,
        refund_supra: u64,
        treasury_before: u64,
        treasury_after: u64,
        active_nfts_before_burn: u64,
        supra_to_beneficiary: u64,
        is_fallback: bool,
        is_last_nft: bool,
        nominal_refund: u64,
    },

    // ops_treasury events (AOE1/2/3/9):
    DonationReceived { donor: String, amount: u64, treasury_after: u64 },
    DonationRejected { donor: String, amount_attempted: u64, treasury_balance: u64 },
    DvrfFunded { amount: u64, treasury_after: u64, dvrf_balance_after: u64 },
    AutomationOwnerTopped {
        amount: u64,
        owner: String,
        treasury_after: u64,
        owner_balance_after: u64,
    },
    TreasuryLowBalance { current_balance: u64, threshold: u64, days_of_runway: u64 },
    TreasuryRecovered { current_balance: u64, threshold: u64 },
    OpsTreasuryConfigUpdated,
    AdminKeyBurned { previous_admin: String },

    // B4 events — withdrawal lifecycle:
    WithdrawalRequested { nft_id: u64, token: String, amount: u64 },
    WithdrawalCancelled { nft_id: u64, token: String, amount: u64 },
    // C-NO-PT-WD (2026-05-19): WithdrawalExecuted + ClaimExecuted variants
    // removed. The per-token wallet exit paths that emitted them are gone
    // from the contract; SUPRA-only exits now flow through MktBurned with
    // reason=2 (rushed) or reason=3 (claim_all) or reason=4 (burn_exit).
    HoldingPeriodStarted { nft_id: u64, expires_at_batch: u64 },
    HoldingPeriodCancelled { nft_id: u64 },

    // B4 events — enforcement + registration:
    NftBlocked { nft_id: u64, enforcement_at_batch: u64 },
    TraderRegistered { nft_id: u64 },
    TraderDeregistered { nft_id: u64 },

    // B5 events — governance:
    ParameterChangeQueued { effective_at_batch: u64, param_name: String, old_value: u64, new_value: u64 },
    ParameterChangeApplied { batch_id: u64, param_name: String, new_value: u64 },
    PoolAdjustmentQueued { effective_at_batch: u64, new_num_pools: u64 },
    PoolAdjusted { batch_id: u64, new_num_pools: u64 },
    PoolAdjustmentCancelled,

    Unknown(String),
}

impl ChainEvent {
    pub fn from_json(json: &serde_json::Value) -> Result<Self, TypesError> {
        let type_str = json.get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| TypesError::DeserializationError("missing event type".into()))?;
        let data = json.get("data")
            .ok_or_else(|| TypesError::DeserializationError("missing event data".into()))?;

        if type_str.contains("BatchTradeSettled") {
            let symbol_hex = data.get("symbol")
                .and_then(|v| v.as_str())
                .unwrap_or("0x");
            let symbol = hex::decode(symbol_hex.trim_start_matches("0x"))
                .unwrap_or_default();

            Ok(ChainEvent::BatchTradeSettled(BatchTradeSettledEvent {
                batch_id: data.get("batch_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                pool_id: data.get("pool_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                trade_id: data.get("trade_id").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                buyer_nft_id: data.get("buyer_nft_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                seller_nft_id: data.get("seller_nft_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                symbol,
                buyer_price: data.get("buyer_price").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                seller_price: data.get("seller_price").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                clearing_price: data.get("clearing_price").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                base_amount: data.get("base_amount").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                quote_amount: data.get("quote_amount").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                buyer_order_hash: data.get("buyer_order_hash").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                seller_order_hash: data.get("seller_order_hash").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                gas_payer: data.get("gas_payer").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                timestamp: data.get("timestamp").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            }))

        // ── B4: Withdrawal lifecycle events ────────────────────────
        } else if type_str.contains("WithdrawalRequested") {
            Ok(ChainEvent::WithdrawalRequested {
                nft_id: data.get("nft_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                token: data.get("token_metadata").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                amount: data.get("amount").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("WithdrawalCancelled") {
            Ok(ChainEvent::WithdrawalCancelled {
                nft_id: data.get("nft_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                token: data.get("token_metadata").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                amount: data.get("amount").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("HoldingPeriodStarted") {
            Ok(ChainEvent::HoldingPeriodStarted {
                nft_id: data.get("nft_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                expires_at_batch: data.get("expires_at_batch").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("HoldingPeriodCancelled") {
            Ok(ChainEvent::HoldingPeriodCancelled {
                nft_id: data.get("nft_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        // ── B4: Enforcement + registration events ──────────────────
        } else if type_str.contains("NFTBlocked") || type_str.contains("NftBlocked") {
            Ok(ChainEvent::NftBlocked {
                nft_id: data.get("accused_nft_id")
                    .or_else(|| data.get("nft_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                enforcement_at_batch: data.get("enforcement_at_batch").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("TraderRegistered") {
            Ok(ChainEvent::TraderRegistered {
                nft_id: data.get("nft_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("TraderDeregistered") {
            Ok(ChainEvent::TraderDeregistered {
                nft_id: data.get("nft_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })

        // ── B5: Governance events ──────────────────────────────────
        } else if type_str.contains("ParameterChangeQueued") {
            Ok(ChainEvent::ParameterChangeQueued {
                effective_at_batch: data.get("effective_at_batch").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                param_name: data.get("param_name").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                old_value: data.get("old_value").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                new_value: data.get("new_value").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("ParameterChangeApplied") {
            Ok(ChainEvent::ParameterChangeApplied {
                batch_id: data.get("batch_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                param_name: data.get("param_name").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                new_value: data.get("new_value").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("PoolAdjustmentQueued") {
            Ok(ChainEvent::PoolAdjustmentQueued {
                effective_at_batch: data.get("effective_at_batch").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                new_num_pools: data.get("new_num_pools").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("PoolAdjustmentCancelled") {
            Ok(ChainEvent::PoolAdjustmentCancelled)
        } else if type_str.contains("PoolAdjusted") {
            Ok(ChainEvent::PoolAdjusted {
                batch_id: data.get("batch_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                new_num_pools: data.get("new_num_pools").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("PoolAdjustmentCancelled") {
            Ok(ChainEvent::PoolAdjustmentCancelled)

        // ── NFT lifecycle (burn-exit rev) ──────────────────────────
        } else if type_str.contains("NFTPairMinted") {
            Ok(ChainEvent::NftPairMinted {
                nft_id: data.get("nft_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                sponsor: data.get("sponsor").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                mint_fee_paid: data.get("mint_fee_paid").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                cohort_started: data.get("cohort_started")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })

        // ── Burn-exit events (from exits.move) ─────────────────────
        } else if type_str.contains("BurnRequested") {
            Ok(ChainEvent::BurnRequested {
                nft_id: data.get("nft_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                beneficiary: data.get("beneficiary").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                requested_at_batch: data.get("requested_at_batch").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("BurnExecuted") {
            Ok(ChainEvent::BurnExecuted {
                nft_id: data.get("nft_id").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                executor: data.get("executor").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                sponsor: data.get("sponsor").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                beneficiary: data.get("beneficiary").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                refund_supra: data.get("refund_supra").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                treasury_before: data.get("treasury_before").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                treasury_after: data.get("treasury_after").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                active_nfts_before_burn: data.get("active_nfts_before_burn").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                supra_to_beneficiary: data.get("supra_to_beneficiary").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                is_fallback: data.get("is_fallback").and_then(|v| v.as_bool()).unwrap_or(false),
                is_last_nft: data.get("is_last_nft").and_then(|v| v.as_bool()).unwrap_or(false),
                nominal_refund: data.get("nominal_refund").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })

        // ── ops_treasury events (AOE1/2/3/9) ───────────────────────
        } else if type_str.contains("DonationReceived") {
            Ok(ChainEvent::DonationReceived {
                donor: data.get("donor").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                amount: data.get("amount").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                treasury_after: data.get("treasury_after").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("DonationRejected") {
            Ok(ChainEvent::DonationRejected {
                donor: data.get("donor").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                amount_attempted: data.get("amount_attempted").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                treasury_balance: data.get("treasury_balance").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("DvrfFunded") {
            Ok(ChainEvent::DvrfFunded {
                amount: data.get("amount").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                treasury_after: data.get("treasury_after").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                dvrf_balance_after: data.get("dvrf_balance_after").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("AutomationOwnerTopped") {
            Ok(ChainEvent::AutomationOwnerTopped {
                amount: data.get("amount").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                owner: data.get("owner").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
                treasury_after: data.get("treasury_after").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                owner_balance_after: data.get("owner_balance_after").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("TreasuryLowBalance") {
            Ok(ChainEvent::TreasuryLowBalance {
                current_balance: data.get("current_balance").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                threshold: data.get("threshold").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                days_of_runway: data.get("days_of_runway").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("TreasuryRecovered") {
            Ok(ChainEvent::TreasuryRecovered {
                current_balance: data.get("current_balance").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
                threshold: data.get("threshold").and_then(|v| v.as_str())
                    .unwrap_or("0").parse().unwrap_or(0),
            })
        } else if type_str.contains("OpsTreasuryConfigUpdated") {
            Ok(ChainEvent::OpsTreasuryConfigUpdated)
        } else if type_str.contains("AdminKeyBurned") {
            Ok(ChainEvent::AdminKeyBurned {
                previous_admin: data.get("previous_admin").and_then(|v| v.as_str())
                    .unwrap_or("").to_string(),
            })

        } else {
            Ok(ChainEvent::Unknown(type_str.to_string()))
        }
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // T_TYPES_01
    #[test]
    fn test_batch_params_from_json() {
        let json = json!(["10", "4", "3", "1", "2", "3"]);
        let params = BatchParams::from_view_result(&json).unwrap();
        assert_eq!(params.blocks_per_batch, 10);
        assert_eq!(params.commit_blocks, 4);
        assert_eq!(params.reveal_blocks, 3);
        assert_eq!(params.match_blocks, 1);
        assert_eq!(params.swap_blocks, 2);
        assert_eq!(params.commits_per_batch, 3);
    }

    // T_TYPES_01b: Named object format (live Supra API)
    #[test]
    fn test_batch_params_from_json_named_object() {
        let json = json!([{
            "blocks_per_batch": "10",
            "commit_blocks": "4",
            "reveal_blocks": "3",
            "match_blocks": "1",
            "swap_blocks": "2",
            "commits_per_batch": "3",
            "rushed_grace_batches": "10",
            "settlement_max_age": "10"
        }]);
        let params = BatchParams::from_view_result(&json).unwrap();
        assert_eq!(params.blocks_per_batch, 10);
        assert_eq!(params.commit_blocks, 4);
        assert_eq!(params.reveal_blocks, 3);
        assert_eq!(params.match_blocks, 1);
        assert_eq!(params.swap_blocks, 2);
        assert_eq!(params.commits_per_batch, 3);
    }

    // T_TYPES_02
    #[test]
    fn test_batch_epoch_from_json() {
        let json = json!(["1000", "50"]);
        let epoch = BatchEpoch::from_view_result(&json).unwrap();
        assert_eq!(epoch.anchor_block, 1000);
        assert_eq!(epoch.anchor_batch_id, 50);
    }

    // T_TYPES_03 (burn-exit rev: NftConfig is now 2-tuple)
    #[test]
    fn test_nft_config_from_json() {
        let json = json!(["3600", "0xADMIN"]);
        let config = NftConfig::from_view_result(&json).unwrap();
        assert_eq!(config.burn_cooldown_seconds, 3600);
        assert_eq!(config.admin, "0xADMIN");
    }

    // T_TYPES_04
    #[test]
    fn test_market_pair_from_json() {
        let json = json!([
            "0x454d4d2f4b4159",
            "0xBASE_META",
            "0xQUOTE_META",
            "100000000",
            true
        ]);
        let pair = MarketPair::from_view_result(&json).unwrap();
        assert_eq!(pair.symbol, b"EMM/KAY");
        assert_eq!(pair.min_quantity, 100_000_000);
        assert!(pair.is_active);
    }

    // T_TYPES_05
    #[test]
    fn test_pool_state_from_json() {
        let json = json!(["5", "1300", "3000", "800", "1800"]);
        let state = PoolState::from_view_result(&json).unwrap();
        assert_eq!(state.num_pools, 5);
        assert_eq!(state.target_pool_size, 1300);
        assert_eq!(state.live_nft_count, 3000);
    }

    // T_TYPES_06
    #[test]
    fn test_decimals_from_json() {
        let json = json!(["8"]);
        let decimals = parse_decimals_result(&json).unwrap();
        assert_eq!(decimals, 8);
    }

    #[test]
    fn test_token_decimals_map() {
        let mut map = TokenDecimalsMap::new();
        map.insert("EMM".into(), 5);
        map.insert("KAY".into(), 5);

        assert_eq!(map.to_smallest("EMM", 330.0), 33_000_000u64);
        assert_eq!(map.to_smallest("KAY", 330.0), 33_000_000u64);
        assert_eq!(map.to_human("EMM", 33_000_000u64), 330.0);
        assert_eq!(map.to_human("KAY", 33_000_000u64), 330.0);
    }

    // T_TYPES_07 deleted: MintBond struct removed in burn-exit revision
    // (bond mechanism replaced by mint_fee -> ops_treasury -> time-decay
    // refund on burn-exit). See planning/specs/individual-nft-burn-exit.md.

    // T_TYPES_07b (new): NFTPairMinted event roundtrip with sponsor fields.
    #[test]
    fn test_nft_pair_minted_from_json() {
        let json = json!({
            "type": "0xDEADMKT::nft::NFTPairMinted",
            "data": {
                "nft_id": "42",
                "trustee": "0xT",
                "beneficiary": "0xB",
                "sponsor": "0xS",
                "ed25519_pubkey": "0xABCD",
                "mint_fee_paid": "100000000000",
                "timestamp": "1000",
                "cohort_started": true
            }
        });
        let ev = ChainEvent::from_json(&json).unwrap();
        match ev {
            ChainEvent::NftPairMinted { nft_id, sponsor, mint_fee_paid, cohort_started } => {
                assert_eq!(nft_id, 42);
                assert_eq!(sponsor, "0xS");
                assert_eq!(mint_fee_paid, 100_000_000_000);
                assert!(cohort_started);
            }
            other => panic!("expected NftPairMinted, got {:?}", other),
        }
    }

    // T_TYPES_07c: BurnRequested event roundtrip.
    #[test]
    fn test_burn_requested_from_json() {
        let json = json!({
            "type": "0xDEADMKT::exits::BurnRequested",
            "data": {
                "nft_id": "5",
                "beneficiary": "0xB",
                "requested_at_batch": "999",
                "timestamp": "2000"
            }
        });
        let ev = ChainEvent::from_json(&json).unwrap();
        match ev {
            ChainEvent::BurnRequested { nft_id, beneficiary, requested_at_batch } => {
                assert_eq!(nft_id, 5);
                assert_eq!(beneficiary, "0xB");
                assert_eq!(requested_at_batch, 999);
            }
            other => panic!("expected BurnRequested, got {:?}", other),
        }
    }

    // T_TYPES_07d: BurnExecuted event roundtrip with all 12 fields.
    #[test]
    fn test_burn_executed_from_json() {
        let json = json!({
            "type": "0xDEADMKT::exits::BurnExecuted",
            "data": {
                "nft_id": "5",
                "executor": "0xE",
                "sponsor": "0xS",
                "beneficiary": "0xB",
                "refund_supra": "97500000000",
                "treasury_before": "500000000000",
                "treasury_after": "402500000000",
                "active_nfts_before_burn": "5",
                "supra_to_beneficiary": "0",
                "is_fallback": false,
                "is_last_nft": false,
                "nominal_refund": "97500000000",
                "timestamp": "3000"
            }
        });
        let ev = ChainEvent::from_json(&json).unwrap();
        match ev {
            ChainEvent::BurnExecuted {
                nft_id, refund_supra, is_last_nft, nominal_refund, active_nfts_before_burn, ..
            } => {
                assert_eq!(nft_id, 5);
                assert_eq!(refund_supra, 97_500_000_000);
                assert_eq!(nominal_refund, 97_500_000_000);
                assert_eq!(active_nfts_before_burn, 5);
                assert!(!is_last_nft);
            }
            other => panic!("expected BurnExecuted, got {:?}", other),
        }
    }

    // T_TYPES_07e: TreasuryLowBalance event roundtrip.
    #[test]
    fn test_treasury_low_balance_from_json() {
        let json = json!({
            "type": "0xDEADMKT::ops_treasury::TreasuryLowBalance",
            "data": {
                "current_balance": "1500000000000",
                "threshold": "1728000000000",
                "days_of_runway": "52"
            }
        });
        let ev = ChainEvent::from_json(&json).unwrap();
        match ev {
            ChainEvent::TreasuryLowBalance { current_balance, threshold, days_of_runway } => {
                assert_eq!(current_balance, 1_500_000_000_000);
                assert_eq!(threshold, 1_728_000_000_000);
                assert_eq!(days_of_runway, 52);
            }
            other => panic!("expected TreasuryLowBalance, got {:?}", other),
        }
    }

    // T_TYPES_07f: DonationReceived + DonationRejected roundtrip.
    #[test]
    fn test_donation_events_from_json() {
        let received = json!({
            "type": "0xDEADMKT::ops_treasury::DonationReceived",
            "data": {
                "donor": "0xD",
                "amount": "1000000000000",
                "treasury_after": "5000000000000"
            }
        });
        let ev = ChainEvent::from_json(&received).unwrap();
        match ev {
            ChainEvent::DonationReceived { amount, treasury_after, .. } => {
                assert_eq!(amount, 1_000_000_000_000);
                assert_eq!(treasury_after, 5_000_000_000_000);
            }
            other => panic!("expected DonationReceived, got {:?}", other),
        }

        let rejected = json!({
            "type": "0xDEADMKT::ops_treasury::DonationRejected",
            "data": {
                "donor": "0xD",
                "amount_attempted": "1000000000000",
                "treasury_balance": "60000000000000"
            }
        });
        let ev = ChainEvent::from_json(&rejected).unwrap();
        match ev {
            ChainEvent::DonationRejected { amount_attempted, treasury_balance, .. } => {
                assert_eq!(amount_attempted, 1_000_000_000_000);
                assert_eq!(treasury_balance, 60_000_000_000_000);
            }
            other => panic!("expected DonationRejected, got {:?}", other),
        }
    }

    // T_TYPES_07g: AutomationOwnerTopped + DvrfFunded roundtrip.
    #[test]
    fn test_top_up_events_from_json() {
        let auto_topped = json!({
            "type": "0xDEADMKT::ops_treasury::AutomationOwnerTopped",
            "data": {
                "amount": "1000000000000",
                "owner": "0xOwner",
                "treasury_after": "4000000000000",
                "owner_balance_after": "2000000000000"
            }
        });
        let ev = ChainEvent::from_json(&auto_topped).unwrap();
        match ev {
            ChainEvent::AutomationOwnerTopped { amount, owner, .. } => {
                assert_eq!(amount, 1_000_000_000_000);
                assert_eq!(owner, "0xOwner");
            }
            other => panic!("expected AutomationOwnerTopped, got {:?}", other),
        }

        let dvrf = json!({
            "type": "0xDEADMKT::ops_treasury::DvrfFunded",
            "data": {
                "amount": "100000000000",
                "treasury_after": "4900000000000",
                "dvrf_balance_after": "5100000000000"
            }
        });
        let ev = ChainEvent::from_json(&dvrf).unwrap();
        match ev {
            ChainEvent::DvrfFunded { amount, .. } => {
                assert_eq!(amount, 100_000_000_000);
            }
            other => panic!("expected DvrfFunded, got {:?}", other),
        }
    }

    // T_TYPES_08
    #[test]
    fn test_batch_trade_settled_event() {
        let json = json!({
            "sequence_number": "0",
            "type": "0xDEADMKT::settlement::BatchTradeSettled",
            "data": {
                "batch_id": "100",
                "pool_id": "3",
                "trade_id": "0xABCD",
                "buyer_nft_id": "42",
                "seller_nft_id": "99",
                "symbol": "0x454d4d2f4b4159",
                "buyer_price": "5000000",
                "seller_price": "4900000",
                "clearing_price": "4950000",
                "base_amount": "100000000",
                "quote_amount": "495000000",
                "buyer_order_hash": "0xBUYER_HASH",
                "seller_order_hash": "0xSELLER_HASH",
                "gas_payer": "0xGAS_PAYER",
                "timestamp": "1708100000"
            }
        });
        let event = ChainEvent::from_json(&json).unwrap();
        match event {
            ChainEvent::BatchTradeSettled(e) => {
                assert_eq!(e.batch_id, 100);
                assert_eq!(e.buyer_nft_id, 42);
                assert_eq!(e.buyer_price, 5_000_000);
                assert_eq!(e.seller_price, 4_900_000);
                assert_eq!(e.clearing_price, 4_950_000);
                assert_eq!(e.symbol, b"EMM/KAY");
                assert_eq!(e.buyer_order_hash, "0xBUYER_HASH");
                assert_eq!(e.seller_order_hash, "0xSELLER_HASH");
            }
            _ => panic!("wrong event type"),
        }
    }

    // Extra: missing fields
    #[test]
    fn test_batch_params_missing_field() {
        let json = json!(["10", "4"]);
        assert!(BatchParams::from_view_result(&json).is_err());
    }

    // Extra: unknown event type
    #[test]
    fn test_unknown_event_type() {
        let json = json!({
            "type": "0xDEADMKT::nft::SomeNewEvent",
            "data": {}
        });
        let event = ChainEvent::from_json(&json).unwrap();
        assert!(matches!(event, ChainEvent::Unknown(_)));
    }

    // ── B4 event tests ─────────────────────────────────────────

    // T_EVENT_01: WithdrawalRequested
    #[test]
    fn test_withdrawal_requested_event() {
        let json = json!({
            "type": "0xDEADMKT::escrow::WithdrawalRequested",
            "data": {
                "nft_id": "42",
                "token_metadata": "0xABC",
                "amount": "1000000"
            }
        });
        let event = ChainEvent::from_json(&json).unwrap();
        match event {
            ChainEvent::WithdrawalRequested { nft_id, token, amount } => {
                assert_eq!(nft_id, 42);
                assert_eq!(token, "0xABC");
                assert_eq!(amount, 1_000_000);
            }
            _ => panic!("wrong event type"),
        }
    }

    // T_EVENT_02: HoldingPeriodStarted
    #[test]
    fn test_holding_period_started_event() {
        let json = json!({
            "type": "0xDEADMKT::escrow::HoldingPeriodStarted",
            "data": {
                "nft_id": "42",
                "expires_at_batch": "50100"
            }
        });
        let event = ChainEvent::from_json(&json).unwrap();
        match event {
            ChainEvent::HoldingPeriodStarted { nft_id, expires_at_batch } => {
                assert_eq!(nft_id, 42);
                assert_eq!(expires_at_batch, 50100);
            }
            _ => panic!("wrong event type"),
        }
    }

    // T_EVENT_03: ClaimExecuted -- deleted alongside the ChainEvent
    // variant in C-NO-PT-WD (2026-05-19).

    // T_EVENT_04: NftBlocked (uses accused_nft_id field name from contract)
    #[test]
    fn test_nft_blocked_event() {
        let json = json!({
            "type": "0xDEADMKT::pool_config::NFTBlocked",
            "data": {
                "accused_nft_id": "99",
                "enforcement_at_batch": "50200"
            }
        });
        let event = ChainEvent::from_json(&json).unwrap();
        match event {
            ChainEvent::NftBlocked { nft_id, enforcement_at_batch } => {
                assert_eq!(nft_id, 99);
                assert_eq!(enforcement_at_batch, 50200);
            }
            _ => panic!("wrong event type"),
        }
    }

    // T_EVENT_05: Unknown event still works (regression)
    #[test]
    fn test_unknown_event_b4_regression() {
        let json = json!({
            "type": "0xDEADMKT::future::SomethingNew",
            "data": { "foo": "bar" }
        });
        let event = ChainEvent::from_json(&json).unwrap();
        match event {
            ChainEvent::Unknown(s) => assert!(s.contains("SomethingNew")),
            _ => panic!("should be Unknown"),
        }
    }

    // T_EVENT_06: WithdrawalCancelled
    #[test]
    fn test_withdrawal_cancelled_event() {
        let json = json!({
            "type": "0xDEADMKT::escrow::WithdrawalCancelled",
            "data": {
                "nft_id": "42",
                "token_metadata": "0xABC",
                "amount": "1000000"
            }
        });
        let event = ChainEvent::from_json(&json).unwrap();
        match event {
            ChainEvent::WithdrawalCancelled { nft_id, token, amount } => {
                assert_eq!(nft_id, 42);
                assert_eq!(token, "0xABC");
                assert_eq!(amount, 1_000_000);
            }
            _ => panic!("wrong event type"),
        }
    }

    // T_EVENT_07: TraderRegistered
    #[test]
    fn test_trader_registered_event() {
        let json = json!({
            "type": "0xDEADMKT::escrow::TraderRegistered",
            "data": { "nft_id": "55" }
        });
        let event = ChainEvent::from_json(&json).unwrap();
        match event {
            ChainEvent::TraderRegistered { nft_id } => assert_eq!(nft_id, 55),
            _ => panic!("wrong event type"),
        }
    }

    // T_EVENT_08: TraderDeregistered
    #[test]
    fn test_trader_deregistered_event() {
        let json = json!({
            "type": "0xDEADMKT::escrow::TraderDeregistered",
            "data": { "nft_id": "55" }
        });
        let event = ChainEvent::from_json(&json).unwrap();
        match event {
            ChainEvent::TraderDeregistered { nft_id } => assert_eq!(nft_id, 55),
            _ => panic!("wrong event type"),
        }
    }

    // ── B5: Governance event tests ───────────────────────────────────

    // T_CHAIN_GOV_01: ParameterChangeQueued
    #[test]
    fn t_chain_gov_01_parameter_change_queued() {
        let json = json!({
            "type": "0xDEADMKT::pool_config::ParameterChangeQueued",
            "data": {
                "effective_at_batch": "500",
                "param_name": "commits_per_batch",
                "old_value": "3",
                "new_value": "5"
            }
        });
        let event = ChainEvent::from_json(&json).unwrap();
        match event {
            ChainEvent::ParameterChangeQueued { effective_at_batch, param_name, old_value, new_value } => {
                assert_eq!(effective_at_batch, 500);
                assert_eq!(param_name, "commits_per_batch");
                assert_eq!(old_value, 3);
                assert_eq!(new_value, 5);
            }
            _ => panic!("wrong event type"),
        }
    }

    // T_CHAIN_GOV_02: ParameterChangeApplied
    #[test]
    fn t_chain_gov_02_parameter_change_applied() {
        let json = json!({
            "type": "0xDEADMKT::pool_config::ParameterChangeApplied",
            "data": {
                "batch_id": "500",
                "param_name": "blocks_per_batch",
                "new_value": "20"
            }
        });
        let event = ChainEvent::from_json(&json).unwrap();
        match event {
            ChainEvent::ParameterChangeApplied { batch_id, param_name, new_value } => {
                assert_eq!(batch_id, 500);
                assert_eq!(param_name, "blocks_per_batch");
                assert_eq!(new_value, 20);
            }
            _ => panic!("wrong event type"),
        }
    }

    // T_CHAIN_GOV_03: PoolAdjusted
    #[test]
    fn t_chain_gov_03_pool_adjusted() {
        let json = json!({
            "type": "0xDEADMKT::pool_config::PoolAdjusted",
            "data": {
                "batch_id": "600",
                "new_num_pools": "8"
            }
        });
        let event = ChainEvent::from_json(&json).unwrap();
        match event {
            ChainEvent::PoolAdjusted { batch_id, new_num_pools } => {
                assert_eq!(batch_id, 600);
                assert_eq!(new_num_pools, 8);
            }
            _ => panic!("wrong event type"),
        }
    }

    // T_CHAIN_GOV_04: PoolAdjustmentQueued
    #[test]
    fn t_chain_gov_04_pool_adjustment_queued() {
        let json = json!({
            "type": "0xDEADMKT::pool_config::PoolAdjustmentQueued",
            "data": {
                "effective_at_batch": "700",
                "new_num_pools": "16"
            }
        });
        let event = ChainEvent::from_json(&json).unwrap();
        match event {
            ChainEvent::PoolAdjustmentQueued { effective_at_batch, new_num_pools } => {
                assert_eq!(effective_at_batch, 700);
                assert_eq!(new_num_pools, 16);
            }
            _ => panic!("wrong event type"),
        }
    }

    // T_CHAIN_GOV_05: PoolAdjustmentCancelled
    #[test]
    fn t_chain_gov_05_pool_adjustment_cancelled() {
        let json = json!({
            "type": "0xDEADMKT::pool_config::PoolAdjustmentCancelled",
            "data": {}
        });
        let event = ChainEvent::from_json(&json).unwrap();
        assert_eq!(event, ChainEvent::PoolAdjustmentCancelled);
    }
}
