// crates/settlement/src/lib.rs
//
// Build 4A: Transaction submission — build settle_match tx from Match,
//           sign with trustee Ed25519, submit to Supra REST API.
//
// Build 4B: Confirmation + abort handling — poll for BatchTradeSettled events,
//           detect abort codes, retry logic, backstop submission.
//
// settle_match(submitter, batch_id, pool_id,
//   buyer_nft_id, buyer_side, buyer_price, buyer_quantity, buyer_batch_id, buyer_nonce, buyer_signature,
//   seller_nft_id, seller_side, seller_price, seller_quantity, seller_batch_id, seller_nonce, seller_signature,
//   symbol, fill_quantity)
//
// Contract computes settlement_price = (buyer_price + seller_price) / 2 on-chain.
// Emits BatchTradeSettled event.
//
// CD-8: E_ALREADY_SETTLED is not failure — trade DID settle. Don't reverse outflow.
// CD-9: Backstop grace = blocks_per_batch from SWAP start.

pub mod worker;
pub use worker::{SettleRequest, SettleResult, spawn_worker};

use deadmkt_chain::client::{ChainError, SupraClient};
use deadmkt_chain::types::BatchTradeSettledEvent;
use deadmkt_escrow_tracker::{EscrowTracker, Side};
use deadmkt_matching::Match;
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;
use sha3::{Digest as Sha3Digest, Sha3_256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;

// =========================================================================
// Errors
// =========================================================================

#[derive(Error, Debug)]
pub enum SettlementError {
    #[error("chain error: {0}")]
    Chain(#[from] ChainError),

    #[error("bcs serialization error: {0}")]
    BcsError(String),

    #[error("tx simulation failed: {0}")]
    SimulationFailed(String),

    #[error("tx submission failed: {0}")]
    SubmissionFailed(String),

    #[error("invalid contract address: {0}")]
    InvalidAddress(String),

    #[error("timeout waiting for confirmation: {0}")]
    ConfirmationTimeout(String),

    #[error("backstop window expired without settlement: {0}")]
    BackstopExpired(String),
}

// =========================================================================
// Abort codes — on-chain error codes from settlement.move
// =========================================================================

/// On-chain abort codes from settle_match().
/// Maps to the E_* constants in SETTLEMENT_CONTRACT_SCOPE.
#[derive(Debug, Clone, PartialEq)]
pub enum AbortCode {
    AlreadySettled,
    InsufficientBalance,
    BuyerInactive,
    SellerInactive,
    BuyerOverfill,
    SellerOverfill,
    NftBlocked,
    NftBurned,
    BatchTooOld,
    QuantityTooSmall,
    SelfTrade,
    InvalidSide,
    SignatureVerifyFailed,
    MarketNotActive,
    QuoteAmountZero,
    Other(u64),
}

impl std::fmt::Display for AbortCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AbortCode::AlreadySettled => write!(f, "E_ALREADY_SETTLED"),
            AbortCode::InsufficientBalance => write!(f, "E_INSUFFICIENT_BALANCE"),
            AbortCode::BuyerInactive => write!(f, "E_BUYER_INACTIVE"),
            AbortCode::SellerInactive => write!(f, "E_SELLER_INACTIVE"),
            AbortCode::BuyerOverfill => write!(f, "E_BUYER_OVERFILL"),
            AbortCode::SellerOverfill => write!(f, "E_SELLER_OVERFILL"),
            AbortCode::NftBlocked => write!(f, "E_NFT_BLOCKED"),
            AbortCode::NftBurned => write!(f, "E_NFT_BURNED"),
            AbortCode::BatchTooOld => write!(f, "E_BATCH_TOO_OLD"),
            AbortCode::QuantityTooSmall => write!(f, "E_QUANTITY_TOO_SMALL"),
            AbortCode::SelfTrade => write!(f, "E_SELF_TRADE"),
            AbortCode::InvalidSide => write!(f, "E_INVALID_SIDE"),
            AbortCode::SignatureVerifyFailed => write!(f, "E_SIGNATURE_VERIFY_FAILED"),
            AbortCode::MarketNotActive => write!(f, "E_MARKET_NOT_ACTIVE"),
            AbortCode::QuoteAmountZero => write!(f, "E_QUOTE_AMOUNT_ZERO"),
            AbortCode::Other(code) => write!(f, "E_UNKNOWN({})", code),
        }
    }
}

// Reason codes — MUST match settlement.move E_* constants exactly.
// Supra wraps these as (category << 16) | reason; from_code() strips the category.
//
// Source of truth: settlement.move error constants block.
const ABORT_BATCH_TOO_OLD: u64 = 0;        // E_BATCH_TOO_OLD
// const ABORT_NOT_ADMIN: u64 = 1;          // E_NOT_ADMIN (never from settle_match)
// const ABORT_CONTRACT_PAUSED: u64 = 2;    // E_CONTRACT_PAUSED
const ABORT_INVALID_SIDE: u64 = 3;          // E_INVALID_SIDE
// const ABORT_INVALID_PRICE: u64 = 5;      // E_INVALID_PRICE
// const ABORT_INVALID_QUANTITY: u64 = 6;    // E_INVALID_QUANTITY
// const ABORT_BATCH_MISMATCH: u64 = 7;     // E_BATCH_MISMATCH
const ABORT_SELF_TRADE: u64 = 8;            // E_SELF_TRADE
// const ABORT_BUYER_PRICE_TOO_LOW: u64 = 9;// E_BUYER_PRICE_TOO_LOW
const ABORT_INVALID_BUYER_SIG: u64 = 11;    // E_INVALID_BUYER_SIG
const ABORT_INVALID_SELLER_SIG: u64 = 12;   // E_INVALID_SELLER_SIG
const ABORT_BUYER_INACTIVE: u64 = 13;       // E_BUYER_INACTIVE
const ABORT_SELLER_INACTIVE: u64 = 14;      // E_SELLER_INACTIVE
const ABORT_ALREADY_SETTLED: u64 = 15;      // E_ALREADY_SETTLED
// const ABORT_MARKET_NOT_FOUND: u64 = 16;  // E_MARKET_NOT_FOUND
const ABORT_MARKET_NOT_ACTIVE: u64 = 17;    // E_MARKET_NOT_ACTIVE
const ABORT_QUANTITY_TOO_SMALL: u64 = 18;   // E_QUANTITY_TOO_SMALL
const ABORT_QUOTE_AMOUNT_ZERO: u64 = 20;    // E_QUOTE_AMOUNT_ZERO
const ABORT_BUYER_OVERFILL: u64 = 21;       // E_BUYER_OVERFILL
const ABORT_SELLER_OVERFILL: u64 = 22;      // E_SELLER_OVERFILL
const ABORT_NFT_BURNED: u64 = 23;           // E_NFT_BURNED
// const ABORT_QUOTE_OVERFLOW: u64 = 24;    // E_QUOTE_AMOUNT_OVERFLOW
const ABORT_NFT_BLOCKED: u64 = 28;          // E_NFT_BLOCKED

impl AbortCode {
    /// Create from numeric abort code.
    ///
    /// Supra VM encodes Move errors as `(category << 16) | reason`.
    /// We extract the reason (low 16 bits) and match against settlement.move constants.
    pub fn from_code(code: u64) -> Self {
        let reason = if code > 0xFFFF { code & 0xFFFF } else { code };
        match reason {
            ABORT_BATCH_TOO_OLD => AbortCode::BatchTooOld,
            ABORT_INVALID_SIDE => AbortCode::InvalidSide,
            ABORT_SELF_TRADE => AbortCode::SelfTrade,
            ABORT_INVALID_BUYER_SIG | ABORT_INVALID_SELLER_SIG => AbortCode::SignatureVerifyFailed,
            ABORT_BUYER_INACTIVE => AbortCode::BuyerInactive,
            ABORT_SELLER_INACTIVE => AbortCode::SellerInactive,
            ABORT_ALREADY_SETTLED => AbortCode::AlreadySettled,
            ABORT_MARKET_NOT_ACTIVE => AbortCode::MarketNotActive,
            ABORT_QUANTITY_TOO_SMALL => AbortCode::QuantityTooSmall,
            ABORT_QUOTE_AMOUNT_ZERO => AbortCode::QuoteAmountZero,
            ABORT_BUYER_OVERFILL => AbortCode::BuyerOverfill,
            ABORT_SELLER_OVERFILL => AbortCode::SellerOverfill,
            ABORT_NFT_BURNED => AbortCode::NftBurned,
            ABORT_NFT_BLOCKED => AbortCode::NftBlocked,
            _ => AbortCode::Other(code),
        }
    }

    /// Parse from Supra VM status string.
    ///
    /// Expected formats:
    ///   "Move abort in 0xDEADMKT::settlement: 0x10001"
    ///   "Move abort in 0xDEADMKT::settlement: 65537"
    ///   "Execution failed"  → None
    ///   "Executed successfully" → None
    pub fn from_vm_status(status: &str) -> Option<Self> {
        if !status.contains("Move abort") {
            return None;
        }
        // Supra VM status formats:
        //   "Move abort in 0x...::settlement: 65536"
        //   "Move abort in 0x...::settlement: 0x10000"
        //   "Move abort in 0x...::settlement: E_BATCH_TOO_OLD(0x10000): "
        //
        // Strategy: find any hex (0x...) or decimal number in the string.
        // Try the parenthesized hex first (named format), then fall back
        // to the last colon-separated segment (legacy format).

        // Try named format: extract from parentheses e.g. E_NAME(0x10000)
        if let Some(paren_start) = status.rfind('(') {
            if let Some(paren_end) = status[paren_start..].find(')') {
                let inner = &status[paren_start + 1..paren_start + paren_end].trim();
                if let Some(hex_str) = inner.strip_prefix("0x") {
                    if let Ok(code) = u64::from_str_radix(hex_str, 16) {
                        return Some(Self::from_code(code));
                    }
                } else if let Ok(code) = inner.parse::<u64>() {
                    return Some(Self::from_code(code));
                }
            }
        }

        // Legacy format: last segment after colon
        let code_str = status.rsplit(':').next()?.trim();
        if code_str.is_empty() {
            // Try second-to-last segment (trailing colon in Supra format)
            let segments: Vec<&str> = status.split(':').collect();
            if segments.len() >= 2 {
                let prev = segments[segments.len() - 2].trim();
                if let Some(hex_str) = prev.strip_prefix("0x") {
                    if let Ok(code) = u64::from_str_radix(hex_str, 16) {
                        return Some(Self::from_code(code));
                    }
                } else if let Ok(code) = prev.parse::<u64>() {
                    return Some(Self::from_code(code));
                }
            }
            return None;
        }
        let code = if let Some(hex_str) = code_str.strip_prefix("0x") {
            u64::from_str_radix(hex_str, 16).ok()?
        } else {
            code_str.parse::<u64>().ok()?
        };
        Some(Self::from_code(code))
    }
}

// =========================================================================
// Outcome types
// =========================================================================

/// Outcome of a settlement attempt.
#[derive(Debug, Clone)]
pub enum SettlementOutcome {
    Confirmed { tx_hash: String },
    AlreadySettled,
    Aborted { code: AbortCode },
    Pending { tx_hash: String },
}

/// Gas estimation result.
#[derive(Debug, Clone)]
pub struct GasEstimate {
    pub gas_units: u64,
    pub success: bool,
    pub abort_code: Option<AbortCode>,
    pub vm_status: String,
}

// =========================================================================
// BCS Transaction Types — Supra/Aptos compatible
// =========================================================================

pub type AccountAddress = [u8; 32];

#[derive(Debug, Clone, Serialize)]
pub struct ModuleId {
    pub address: AccountAddress,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryFunction {
    pub module: ModuleId,
    pub function: String,
    pub ty_args: Vec<TypeTagStub>,
    pub args: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TypeTagStub;

#[derive(Debug, Clone, Serialize)]
pub enum TransactionPayload {
    #[allow(dead_code)]
    Script,
    #[allow(dead_code)]
    ModuleBundle,
    EntryFunction(EntryFunction),
}

#[derive(Debug, Clone, Serialize)]
pub struct RawTransaction {
    pub sender: AccountAddress,
    pub sequence_number: u64,
    pub payload: TransactionPayload,
    pub max_gas_amount: u64,
    pub gas_unit_price: u64,
    pub expiration_timestamp_secs: u64,
    pub chain_id: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ed25519Authenticator {
    pub public_key: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub enum TransactionAuthenticator {
    Ed25519(Ed25519Authenticator),
}

#[derive(Debug, Clone, Serialize)]
pub struct SignedTransaction {
    pub raw_txn: RawTransaction,
    pub authenticator: TransactionAuthenticator,
}

// =========================================================================
// Settle args builder (pure logic)
// =========================================================================

/// Build the 18 BCS-encoded args for settle_match from a Match.
///
/// settle_match(submitter, batch_id, pool_id,
///   buyer_nft_id, buyer_side, buyer_price, buyer_quantity,
///   buyer_batch_id, buyer_nonce, buyer_signature,
///   seller_nft_id, seller_side, seller_price, seller_quantity,
///   seller_batch_id, seller_nonce, seller_signature,
///   symbol, fill_quantity)
///
/// `submitter` is the signer (handled by authenticator). Args start at batch_id.
pub fn build_settle_args(m: &Match) -> Result<Vec<Vec<u8>>, SettlementError> {
    let bcs_err = |e: bcs::Error| SettlementError::BcsError(e.to_string());
    let mut args = Vec::with_capacity(18);

    // 1-2: batch_id, pool_id
    args.push(bcs::to_bytes(&m.batch_id).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.pool_id).map_err(bcs_err)?);

    // 3-9: buyer order fields
    args.push(bcs::to_bytes(&m.buyer.order.nft_id).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.buyer.order.side).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.buyer.order.price).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.buyer.order.quantity).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.buyer.order.batch_id).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.buyer.order.nonce).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.buyer.signature).map_err(bcs_err)?);

    // 10-16: seller order fields
    args.push(bcs::to_bytes(&m.seller.order.nft_id).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.seller.order.side).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.seller.order.price).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.seller.order.quantity).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.seller.order.batch_id).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.seller.order.nonce).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.seller.signature).map_err(bcs_err)?);

    // 17-18: match terms
    args.push(bcs::to_bytes(&m.symbol).map_err(bcs_err)?);
    args.push(bcs::to_bytes(&m.fill_quantity).map_err(bcs_err)?);

    debug_assert_eq!(args.len(), 18);
    Ok(args)
}

/// Parse a hex address string into a 32-byte AccountAddress.
/// Handles: "0x1", "0x0001", "0xDEADMKT", full 64-char hex, etc.
pub fn parse_address(addr: &str) -> Result<AccountAddress, SettlementError> {
    let hex_str = addr.strip_prefix("0x").unwrap_or(addr);
    // Left-pad to even length for hex::decode
    let padded = if hex_str.len() % 2 != 0 {
        format!("0{}", hex_str)
    } else {
        hex_str.to_string()
    };
    let bytes = hex::decode(&padded)
        .map_err(|e| SettlementError::InvalidAddress(e.to_string()))?;
    if bytes.len() > 32 {
        return Err(SettlementError::InvalidAddress(
            format!("address too long: {} bytes", bytes.len()),
        ));
    }
    let mut addr_bytes = [0u8; 32];
    let offset = 32 - bytes.len();
    addr_bytes[offset..].copy_from_slice(&bytes);
    Ok(addr_bytes)
}

/// Compute Supra signing prefix: SHA3-256("SUPRA::RawTransaction").
pub fn signing_prefix() -> [u8; 32] {
    let mut hasher = Sha3_256::new();
    hasher.update(b"SUPRA::RawTransaction");
    let result = hasher.finalize();
    let mut prefix = [0u8; 32];
    prefix.copy_from_slice(&result);
    prefix
}

/// Build and sign a generic entry function transaction.
/// Reusable for deposit, settle_match, or any entry function call.
pub fn build_and_sign_entry_function(
    signing_key: &SigningKey,
    sender_addr: AccountAddress,
    contract_addr: AccountAddress,
    module_name: &str,
    function_name: &str,
    args: Vec<Vec<u8>>,
    sequence_number: u64,
    expiration_secs: u64,
    chain_id: u8,
    max_gas: u64,
    gas_price: u64,
) -> Result<Vec<u8>, SettlementError> {
    let raw_tx = RawTransaction {
        sender: sender_addr,
        sequence_number,
        payload: TransactionPayload::EntryFunction(EntryFunction {
            module: ModuleId {
                address: contract_addr,
                name: module_name.to_string(),
            },
            function: function_name.to_string(),
            ty_args: vec![],
            args,
        }),
        max_gas_amount: max_gas,
        gas_unit_price: gas_price,
        expiration_timestamp_secs: expiration_secs,
        chain_id,
    };

    let raw_tx_bytes = bcs::to_bytes(&raw_tx)
        .map_err(|e| SettlementError::BcsError(e.to_string()))?;

    let prefix = signing_prefix();
    let mut signing_msg = Vec::with_capacity(32 + raw_tx_bytes.len());
    signing_msg.extend_from_slice(&prefix);
    signing_msg.extend_from_slice(&raw_tx_bytes);
    let signature = signing_key.sign(&signing_msg);

    let signed_tx = SignedTransaction {
        raw_txn: raw_tx,
        authenticator: TransactionAuthenticator::Ed25519(Ed25519Authenticator {
            public_key: signing_key.verifying_key().to_bytes().to_vec(),
            signature: signature.to_bytes().to_vec(),
        }),
    };

    bcs::to_bytes(&signed_tx)
        .map_err(|e| SettlementError::BcsError(e.to_string()))
}

/// Build the same transaction but with zeroed signature for simulation.
/// Supra's simulate endpoint rejects transactions with valid signatures.
pub fn build_simulation_entry_function(
    signing_key: &SigningKey,
    sender_addr: AccountAddress,
    contract_addr: AccountAddress,
    module_name: &str,
    function_name: &str,
    args: Vec<Vec<u8>>,
    sequence_number: u64,
    expiration_secs: u64,
    chain_id: u8,
    max_gas: u64,
    gas_price: u64,
) -> Result<Vec<u8>, SettlementError> {
    let raw_tx = RawTransaction {
        sender: sender_addr,
        sequence_number,
        payload: TransactionPayload::EntryFunction(EntryFunction {
            module: ModuleId {
                address: contract_addr,
                name: module_name.to_string(),
            },
            function: function_name.to_string(),
            ty_args: vec![],
            args,
        }),
        max_gas_amount: max_gas,
        gas_unit_price: gas_price,
        expiration_timestamp_secs: expiration_secs,
        chain_id,
    };

    let signed_tx = SignedTransaction {
        raw_txn: raw_tx,
        authenticator: TransactionAuthenticator::Ed25519(Ed25519Authenticator {
            public_key: signing_key.verifying_key().to_bytes().to_vec(),
            signature: vec![0u8; 64], // zeroed signature for simulation
        }),
    };

    bcs::to_bytes(&signed_tx)
        .map_err(|e| SettlementError::BcsError(e.to_string()))
}

// =========================================================================
// SettlementSubmitter
// =========================================================================

/// Builds and submits settle_match transactions (4A).
pub struct SettlementSubmitter {
    client: SupraClient,
    signing_key: SigningKey,
    contract_addr: AccountAddress,
    sender_addr: AccountAddress,
    chain_id: u8,
    max_gas: u64,
    gas_price: u64,
}

impl SettlementSubmitter {
    pub fn new(
        client: SupraClient,
        signing_key: SigningKey,
        contract_addr: &str,
        sender_addr: &str,
        chain_id: u8,
    ) -> Result<Self, SettlementError> {
        Ok(Self {
            client,
            signing_key,
            contract_addr: parse_address(contract_addr)?,
            sender_addr: parse_address(sender_addr)?,
            chain_id,
            max_gas: 200,
            gas_price: 100_000,
        })
    }

    /// Override gas config (called from run.rs with values from config.json).
    pub fn set_gas_config(&mut self, max_gas: u64, gas_price: u64) {
        self.max_gas = max_gas;
        self.gas_price = gas_price;
    }

    /// Build BCS-encoded EntryFunction payload for settle_match.
    pub fn build_entry_function_payload(
        &self,
        m: &Match,
    ) -> Result<Vec<u8>, SettlementError> {
        let args = build_settle_args(m)?;
        let payload = TransactionPayload::EntryFunction(EntryFunction {
            module: ModuleId {
                address: self.contract_addr,
                name: "settlement".to_string(),
            },
            function: "settle_match".to_string(),
            ty_args: vec![],
            args,
        });
        bcs::to_bytes(&payload)
            .map_err(|e| SettlementError::BcsError(e.to_string()))
    }

    /// Build a complete BCS-encoded SignedTransaction for settle_match.
    pub fn build_signed_tx(
        &self,
        m: &Match,
        sequence_number: u64,
        expiration_secs: u64,
    ) -> Result<Vec<u8>, SettlementError> {
        let args = build_settle_args(m)?;

        let raw_tx = RawTransaction {
            sender: self.sender_addr,
            sequence_number,
            payload: TransactionPayload::EntryFunction(EntryFunction {
                module: ModuleId {
                    address: self.contract_addr,
                    name: "settlement".to_string(),
                },
                function: "settle_match".to_string(),
                ty_args: vec![],
                args,
            }),
            max_gas_amount: self.max_gas,
            gas_unit_price: self.gas_price,
            expiration_timestamp_secs: expiration_secs,
            chain_id: self.chain_id,
        };

        let raw_tx_bytes = bcs::to_bytes(&raw_tx)
            .map_err(|e| SettlementError::BcsError(e.to_string()))?;

        // Sign: prefix || raw_tx_bytes
        let prefix = signing_prefix();
        let mut signing_msg = Vec::with_capacity(32 + raw_tx_bytes.len());
        signing_msg.extend_from_slice(&prefix);
        signing_msg.extend_from_slice(&raw_tx_bytes);
        let signature = self.signing_key.sign(&signing_msg);

        let signed_tx = SignedTransaction {
            raw_txn: raw_tx,
            authenticator: TransactionAuthenticator::Ed25519(Ed25519Authenticator {
                public_key: self.signing_key.verifying_key().to_bytes().to_vec(),
                signature: signature.to_bytes().to_vec(),
            }),
        };

        bcs::to_bytes(&signed_tx)
            .map_err(|e| SettlementError::BcsError(e.to_string()))
    }

    /// Build a BCS-encoded tx with zeroed signature for simulation.
    /// Supra's simulate endpoint rejects transactions with valid signatures.
    pub fn build_sim_tx(
        &self,
        m: &Match,
        sequence_number: u64,
        expiration_secs: u64,
    ) -> Result<Vec<u8>, SettlementError> {
        let args = build_settle_args(m)?;

        let raw_tx = RawTransaction {
            sender: self.sender_addr,
            sequence_number,
            payload: TransactionPayload::EntryFunction(EntryFunction {
                module: ModuleId {
                    address: self.contract_addr,
                    name: "settlement".to_string(),
                },
                function: "settle_match".to_string(),
                ty_args: vec![],
                args,
            }),
            max_gas_amount: self.max_gas,
            gas_unit_price: self.gas_price,
            expiration_timestamp_secs: expiration_secs,
            chain_id: self.chain_id,
        };

        let signed_tx = SignedTransaction {
            raw_txn: raw_tx,
            authenticator: TransactionAuthenticator::Ed25519(Ed25519Authenticator {
                public_key: self.signing_key.verifying_key().to_bytes().to_vec(),
                signature: vec![0u8; 64], // zeroed signature for simulation
            }),
        };

        bcs::to_bytes(&signed_tx)
            .map_err(|e| SettlementError::BcsError(e.to_string()))
    }

    /// Estimate gas via simulate endpoint (4A).
    /// Detects abort codes BEFORE spending real gas.
    pub async fn estimate_gas(
        &self,
        sim_tx_bytes: &[u8],
    ) -> Result<GasEstimate, SettlementError> {
        let sim = self.client.simulate_raw(sim_tx_bytes).await?;
        let abort_code = if !sim.success {
            AbortCode::from_vm_status(&sim.vm_status)
        } else {
            None
        };
        Ok(GasEstimate {
            gas_units: sim.gas_used,
            success: sim.success,
            abort_code,
            vm_status: sim.vm_status,
        })
    }

    /// Submit a signed transaction to chain (4A). Returns tx hash.
    pub async fn submit(
        &self,
        signed_tx_bytes: &[u8],
    ) -> Result<String, SettlementError> {
        self.client.submit_raw(signed_tx_bytes).await
            .map_err(|e| SettlementError::SubmissionFailed(e.to_string()))
    }

    /// Full 4A pipeline: build → sign → submit.
    ///
    /// 1. Get sequence number from chain
    /// 2. Build signed tx
    /// 3. Simulate first
    /// 4. Submit if simulation passes
    pub async fn submit_settlement(
        &self,
        m: &Match,
    ) -> Result<SettlementOutcome, SettlementError> {
        // 1. Get sequence number
        let sender_hex = format!("0x{}", hex::encode(self.sender_addr));
        let account = self.client.get_account(&sender_hex).await?;

        // 2. Build signed tx (expiry = 60 seconds from now)
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 60;

        let signed_tx_bytes = self.build_signed_tx(m, account.sequence_number, expiry)?;
        let sim_tx_bytes = self.build_sim_tx(m, account.sequence_number, expiry)?;

        // 3. Simulate (zeroed signature — Supra rejects valid signatures on simulate)
        let estimate = self.estimate_gas(&sim_tx_bytes).await?;

        if !estimate.success {
            if let Some(code) = estimate.abort_code {
                if code == AbortCode::AlreadySettled {
                    return Ok(SettlementOutcome::AlreadySettled);
                }
                return Ok(SettlementOutcome::Aborted { code });
            }
            // Simulation failed but vm_status could not be parsed as a Move abort.
            // Include the raw vm_status so the operator can diagnose.
            return Err(SettlementError::SimulationFailed(
                format!("unparseable vm_status: {}", estimate.vm_status)
            ));
        }

        // 4. Simulation passed — submit with real signature
        let tx_hash = self.submit(&signed_tx_bytes).await?;
        Ok(SettlementOutcome::Pending { tx_hash })
    }

    /// Direct submission — skip simulation, submit immediately.
    ///
    /// Saves 1-3 seconds of RPC round-trip. Use for the first settlement
    /// in a batch where we're confident the match is valid (just computed it).
    /// Falls back to E_ALREADY_SETTLED or other on-chain errors naturally.
    ///
    /// If `seq_override` is Some, uses that sequence number instead of
    /// fetching from chain. Caller must increment after each call.
    pub async fn submit_settlement_direct(
        &self,
        m: &Match,
        seq_override: Option<u64>,
    ) -> Result<(SettlementOutcome, u64), SettlementError> {
        let sender_hex = format!("0x{}", hex::encode(self.sender_addr));

        let seq = match seq_override {
            Some(s) => s,
            None => {
                let account = self.client.get_account(&sender_hex).await?;
                account.sequence_number
            }
        };

        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 60;

        let signed_tx_bytes = self.build_signed_tx(m, seq, expiry)?;
        let tx_hash = self.submit(&signed_tx_bytes).await?;
        Ok((SettlementOutcome::Pending { tx_hash }, seq + 1))
    }

    /// Fetch current sequence number from chain.
    pub async fn get_sequence_number(&self) -> Result<u64, SettlementError> {
        let sender_hex = format!("0x{}", hex::encode(self.sender_addr));
        let account = self.client.get_account(&sender_hex).await?;
        Ok(account.sequence_number)
    }

    /// Access the chain client (for 4B confirmation polling).
    pub fn client(&self) -> &SupraClient {
        &self.client
    }
}

// =========================================================================
// 4B: Settlement Manager — confirmation, abort handling, backstop
// =========================================================================

/// What to do after a settlement abort.
#[derive(Debug, Clone, PartialEq)]
pub enum SettlementAction {
    /// Keep pending, wait for backstop or event (E_ALREADY_SETTLED, E_INSUFFICIENT_BALANCE).
    KeepPending,
    /// Release outflow, trade won't settle (E_OVERFILL, E_INACTIVE counterparty, etc).
    ReleasePending,
    /// Enter LIMITED mode (own NFT inactive/blocked/burned).
    EnterLimited,
}

/// A settlement that has been registered but not yet confirmed on-chain.
#[derive(Debug, Clone)]
pub struct PendingSettlement {
    pub m: Match,
    pub is_gas_payer: bool,
    pub submitted_at: Option<u64>,
    pub tx_hash: Option<String>,
    /// Block height at which counterparty becomes eligible to backstop-submit.
    pub backstop_eligible_at: u64,
    pub created_at_batch: u64,
}

/// Reason a match was registered but never submitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// Counterparty NFT was known-inactive at submit time (#10 Path 1).
    Inactive { nft_id: u64 },
}

/// Manages in-flight settlements: tracking, confirmation, abort handling, backstop.
///
/// Wires together SettlementSubmitter (4A) + EscrowTracker (4C).
/// Owns the pending map and isolation detection counters.
pub struct SettlementManager {
    tracker: Arc<Mutex<EscrowTracker>>,
    /// In-flight settlements keyed by match_hash hex.
    pending: HashMap<String, PendingSettlement>,
    /// Our NFT ID (to determine buyer/seller side).
    own_nft_id: u64,
    // ─── Isolation detection (CD-8 / P2P_NODE_SCOPE §5.3) ───
    already_settled_count: u32,
    total_settlement_count: u32,
    /// How many consecutive batches of >50% E_ALREADY_SETTLED to trigger.
    #[allow(dead_code)]
    isolation_threshold_batches: u32,
    // ─── #10: matches skipped at submit time because a counterparty was
    // cached-inactive. Lifetime counter for telemetry; last reason map for
    // debug / strategy surfacing.
    skipped_inactive_total: u64,
    skipped_last: HashMap<String, SkipReason>,
}

impl SettlementManager {
    pub fn new(
        tracker: Arc<Mutex<EscrowTracker>>,
        own_nft_id: u64,
    ) -> Self {
        Self {
            tracker,
            pending: HashMap::new(),
            own_nft_id,
            already_settled_count: 0,
            total_settlement_count: 0,
            isolation_threshold_batches: 5,
            skipped_inactive_total: 0,
            skipped_last: HashMap::new(),
        }
    }

    /// #10: record a match that was intentionally not submitted because the
    /// counterparty was cached-inactive. Releases any escrow outflow that
    /// register_match applied. Idempotent on match_hash.
    pub fn register_skipped(
        &mut self,
        match_hash: &str,
        reason: SkipReason,
    ) {
        // If we already called register_match for this hash, remove its
        // pending entry and release the escrow outflow so projected() is
        // restored. If we never registered it (the caller filtered BEFORE
        // register_match), this is a no-op on pending and still records
        // the skip for telemetry.
        if self.pending.remove(match_hash).is_some() {
            if let Ok(mut tracker) = self.tracker.lock() {
                let _ = tracker.release_pending(match_hash);
            }
        }
        self.skipped_inactive_total += 1;
        self.skipped_last.insert(match_hash.to_string(), reason);
    }

    /// #10: lifetime count of matches skipped due to cached-inactive
    /// counterparty. Used by the run-loop's [settle-stats] line.
    pub fn skipped_inactive_total(&self) -> u64 {
        self.skipped_inactive_total
    }

    /// Register a new match for settlement tracking.
    /// Applies outflow to escrow tracker, stores in pending map.
    ///
    /// `swap_start_block` + `blocks_per_batch` = backstop eligible block.
    pub fn register_match(
        &mut self,
        m: Match,
        is_gas_payer: bool,
        swap_start_block: u64,
        blocks_per_batch: u64,
        base_token: &str,
        quote_token: &str,
        base_decimals: u8,
        quote_decimals: u8,
    ) -> Result<(), SettlementError> {
        let match_hash_hex = hex::encode(m.match_hash);

        // Determine our side
        let my_side = if m.buyer.order.nft_id == self.own_nft_id {
            Side::Buy
        } else {
            Side::Sell
        };

        // Apply to escrow tracker
        {
            let mut tracker = self.tracker.lock().unwrap();
            tracker.apply_match(
                &match_hash_hex,
                my_side,
                base_token,
                quote_token,
                m.fill_quantity,
                m.buyer.order.price,
                m.settlement_price,
                base_decimals,
                quote_decimals,
            ).map_err(|e| SettlementError::BcsError(format!("escrow error: {}", e)))?;
        }

        let ps = PendingSettlement {
            created_at_batch: m.batch_id,
            m,
            is_gas_payer,
            submitted_at: None,
            tx_hash: None,
            backstop_eligible_at: swap_start_block + blocks_per_batch,
        };

        self.pending.insert(match_hash_hex, ps);
        Ok(())
    }

    /// Process a confirmed BatchTradeSettled event from chain poller.
    /// Updates escrow tracker (confirm_settlement).
    /// Returns true if this was one of our pending settlements.
    pub fn on_settlement_confirmed(
        &mut self,
        event: &BatchTradeSettledEvent,
    ) -> bool {
        // Find matching pending entry: match on trade_id or field scan
        let match_key = self.find_pending_for_event(event);
        let key = match match_key {
            Some(k) => k,
            None => return false,
        };

        // Update escrow tracker
        {
            let mut tracker = self.tracker.lock().unwrap();
            let _ = tracker.confirm_settlement(&key);
        }

        self.pending.remove(&key);
        true
    }

    /// Find the pending map key that matches a BatchTradeSettledEvent.
    /// Tries trade_id as hex match_hash first, then scans by fields.
    fn find_pending_for_event(&self, event: &BatchTradeSettledEvent) -> Option<String> {
        // Direct lookup: trade_id might be the match_hash hex
        if self.pending.contains_key(&event.trade_id) {
            return Some(event.trade_id.clone());
        }

        // Field scan fallback
        for (key, ps) in &self.pending {
            if ps.m.batch_id == event.batch_id
                && ps.m.buyer.order.nft_id == event.buyer_nft_id
                && ps.m.seller.order.nft_id == event.seller_nft_id
                && ps.m.fill_quantity == event.base_amount
            {
                return Some(key.clone());
            }
        }
        None
    }

    /// Process a failed settlement (abort code detected).
    /// Maps abort code → handling strategy per INTERFACE_MAP §4.1.
    pub fn on_settlement_failed(
        &mut self,
        match_hash: &str,
        code: AbortCode,
    ) -> SettlementAction {
        // Track isolation counters
        self.total_settlement_count += 1;
        if code == AbortCode::AlreadySettled {
            self.already_settled_count += 1;
        }

        match code {
            // CD-8: E_ALREADY_SETTLED — trade DID settle. Keep pending, wait for event.
            AbortCode::AlreadySettled => SettlementAction::KeepPending,

            // E_INSUFFICIENT_BALANCE — counterparty may backstop after balance changes.
            AbortCode::InsufficientBalance => SettlementAction::KeepPending,

            // E_BUYER_INACTIVE — check if it's OUR nft
            AbortCode::BuyerInactive => {
                if let Some(ps) = self.pending.get(match_hash) {
                    if ps.m.buyer.order.nft_id == self.own_nft_id {
                        self.release_pending_internal(match_hash);
                        return SettlementAction::EnterLimited;
                    }
                }
                self.release_pending_internal(match_hash);
                SettlementAction::ReleasePending
            }

            // E_SELLER_INACTIVE — check if it's OUR nft
            AbortCode::SellerInactive => {
                if let Some(ps) = self.pending.get(match_hash) {
                    if ps.m.seller.order.nft_id == self.own_nft_id {
                        self.release_pending_internal(match_hash);
                        return SettlementAction::EnterLimited;
                    }
                }
                self.release_pending_internal(match_hash);
                SettlementAction::ReleasePending
            }

            // E_NFT_BLOCKED / E_NFT_BURNED — could be either side
            AbortCode::NftBlocked | AbortCode::NftBurned => {
                let is_own = self.pending.get(match_hash).map_or(false, |ps| {
                    ps.m.buyer.order.nft_id == self.own_nft_id
                        || ps.m.seller.order.nft_id == self.own_nft_id
                });
                self.release_pending_internal(match_hash);
                if is_own {
                    SettlementAction::EnterLimited
                } else {
                    SettlementAction::ReleasePending
                }
            }

            // Overfill — concurrent fills exceeded quantity. Release.
            AbortCode::BuyerOverfill | AbortCode::SellerOverfill => {
                self.release_pending_internal(match_hash);
                SettlementAction::ReleasePending
            }

            // Batch too old, quantity too small, self-trade, etc — release.
            _ => {
                self.release_pending_internal(match_hash);
                SettlementAction::ReleasePending
            }
        }
    }

    /// Internal: release a pending settlement's outflow in the escrow tracker.
    fn release_pending_internal(&mut self, match_hash: &str) {
        if self.pending.remove(match_hash).is_some() {
            let mut tracker = self.tracker.lock().unwrap();
            let _ = tracker.release_pending(match_hash);
        }
    }

    /// Check if any pending settlements are eligible for backstop submission.
    /// Returns match_hash hex values for settlements where:
    ///   - We are NOT the gas payer
    ///   - current_block >= backstop_eligible_at
    ///   - Not yet submitted by us
    pub fn check_backstop_eligible(
        &self,
        current_block: u64,
    ) -> Vec<String> {
        self.pending.iter()
            .filter(|(_, ps)| {
                !ps.is_gas_payer
                    && ps.tx_hash.is_none()
                    && current_block >= ps.backstop_eligible_at
            })
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Submit backstop for a pending settlement where we are counterparty.
    /// Uses the provided submitter to submit the transaction.
    pub async fn backstop_submit(
        &mut self,
        match_hash: &str,
        submitter: &SettlementSubmitter,
    ) -> Result<SettlementOutcome, SettlementError> {
        let ps = self.pending.get(match_hash)
            .ok_or_else(|| SettlementError::BackstopExpired(
                format!("no pending settlement for {}", match_hash),
            ))?;

        let outcome = submitter.submit_settlement(&ps.m).await?;

        // Update pending with tx_hash if submitted
        match &outcome {
            SettlementOutcome::Pending { tx_hash } => {
                if let Some(ps) = self.pending.get_mut(match_hash) {
                    ps.tx_hash = Some(tx_hash.clone());
                }
            }
            SettlementOutcome::AlreadySettled => {
                // Trade settled by gas payer — keep pending, wait for event
                self.total_settlement_count += 1;
                self.already_settled_count += 1;
            }
            SettlementOutcome::Confirmed { .. } => {
                // Shouldn't happen from submit_settlement (returns Pending),
                // but handle gracefully
            }
            SettlementOutcome::Aborted { .. } => {
                // Will be handled by caller via on_settlement_failed
            }
        }

        Ok(outcome)
    }

    /// Expire stale pending settlements (batch_id too old).
    /// Releases pending_out in escrow tracker for each expired entry.
    pub fn expire_stale(&mut self, current_batch_id: u64, settlement_max_age: u64) {
        let expired: Vec<String> = self.pending.iter()
            .filter(|(_, ps)| {
                current_batch_id > ps.created_at_batch + settlement_max_age
            })
            .map(|(k, _)| k.clone())
            .collect();

        for key in expired {
            self.release_pending_internal(&key);
        }
    }

    /// Check if we're potentially isolated from the gossip mesh.
    /// Triggers when >50% of settlement attempts return E_ALREADY_SETTLED.
    /// Per P2P_NODE_SCOPE §5.3: threshold is >50% across 5+ settlements.
    pub fn is_potentially_isolated(&self) -> bool {
        if self.total_settlement_count < 5 {
            return false;
        }
        let ratio = self.already_settled_count as f64 / self.total_settlement_count as f64;
        ratio > 0.5
    }

    /// Reset isolation detection counters (e.g., after a successful batch cycle).
    pub fn reset_isolation_counters(&mut self) {
        self.already_settled_count = 0;
        self.total_settlement_count = 0;
    }

    /// Get count of pending settlements.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Check if a match_hash is pending.
    pub fn is_pending(&self, match_hash: &str) -> bool {
        self.pending.contains_key(match_hash)
    }

    /// Get a reference to a pending settlement.
    pub fn get_pending(&self, match_hash: &str) -> Option<&PendingSettlement> {
        self.pending.get(match_hash)
    }

    /// Summary of all pending settlements for strategy visibility.
    /// Returns (match_hash, created_at_batch, status) tuples.
    pub fn pending_summary(&self) -> Vec<(String, u64, String)> {
        self.pending.iter().map(|(hash, ps)| {
            let status = if ps.tx_hash.is_some() { "submitted" } else { "queued" };
            (hash.clone(), ps.created_at_batch, status.to_string())
        }).collect()
    }

    /// Mark a pending settlement as submitted (tx sent, awaiting confirmation).
    pub fn mark_submitted(&mut self, match_hash: &str, tx_hash: String, block_height: u64) {
        if let Some(ps) = self.pending.get_mut(match_hash) {
            ps.tx_hash = Some(tx_hash);
            ps.submitted_at = Some(block_height);
        }
    }

    /// Process an async settlement result from the background worker.
    ///
    /// Maps each `SettleResult` variant to existing manager methods:
    /// - `Submitted` → mark_submitted (tx in-flight, await chain event)
    /// - `AlreadySettled` → on_settlement_failed (keep pending, peer settled)
    /// - `Failed` → on_settlement_failed (abort code routing)
    /// - `RpcError` → log + keep pending (will retry or expire)
    pub fn process_result(&mut self, result: SettleResult) -> Option<SettlementAction> {
        match result {
            SettleResult::Submitted { match_hash, tx_hash } => {
                self.mark_submitted(&match_hash, tx_hash, 0);
                None
            }
            SettleResult::AlreadySettled { match_hash } => {
                Some(self.on_settlement_failed(&match_hash, AbortCode::AlreadySettled))
            }
            SettleResult::Failed { match_hash, code } => {
                Some(self.on_settlement_failed(&match_hash, code))
            }
            SettleResult::RpcError { match_hash, error } => {
                eprintln!("[settle] RPC error for {}: {}", &match_hash[..12.min(match_hash.len())], error);
                None // keep pending — will be retried via backstop or expired
            }
        }
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use deadmkt_crypto::Order;
    use deadmkt_matching::{Match, RevealedOrder};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ─── Test helpers ──────────────────────────────────────────────

    fn make_test_match() -> Match {
        let buyer_order = Order {
            nft_id: 42,
            symbol: b"EMM/KAY".to_vec(),
            side: 1,
            price: 5_000_000,
            quantity: 10_000,
            batch_id: 100,
            nonce: vec![0xAA; 32],
        };
        let seller_order = Order {
            nft_id: 99,
            symbol: b"EMM/KAY".to_vec(),
            side: 0,
            price: 4_800_000,
            quantity: 10_000,
            batch_id: 100,
            nonce: vec![0xBB; 32],
        };
        Match {
            batch_id: 100,
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
            fill_quantity: 5_000,
            settlement_price: 4_900_000,
            match_hash: [0xCC; 32],
            gas_payer_nft_id: 42,
        }
    }

    fn make_test_submitter(base_url: &str) -> SettlementSubmitter {
        let signing_key = SigningKey::from_bytes(&[1u8; 32]);
        let client = SupraClient::new(vec![base_url.to_string()], "0xDEAD".into());
        SettlementSubmitter::new(client, signing_key, "0xDEAD", "0x0001", 4).unwrap()
    }

    fn mock_account_endpoint() -> (wiremock::matchers::MethodExactMatcher, wiremock::matchers::PathExactMatcher, ResponseTemplate) {
        (
            method("GET"),
            path("/rpc/v2/accounts/0x0000000000000000000000000000000000000000000000000000000000000001"),
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sequence_number": 42,
                "authentication_key": "0x0001"
            })),
        )
    }

    // ─── T_SETTLE_01: build_settle_args → 18 fields, all correct ──

    #[test]
    fn t_settle_01_build_settle_args_18_fields() {
        let m = make_test_match();
        let args = build_settle_args(&m).unwrap();
        assert_eq!(args.len(), 18);

        // Decode each arg back and verify
        assert_eq!(bcs::from_bytes::<u64>(&args[0]).unwrap(), 100);      // batch_id
        assert_eq!(bcs::from_bytes::<u64>(&args[1]).unwrap(), 3);        // pool_id
        assert_eq!(bcs::from_bytes::<u64>(&args[2]).unwrap(), 42);       // buyer_nft_id
        assert_eq!(bcs::from_bytes::<u8>(&args[3]).unwrap(), 1);         // buyer_side=BUY
        assert_eq!(bcs::from_bytes::<u64>(&args[4]).unwrap(), 5_000_000);// buyer_price
        assert_eq!(bcs::from_bytes::<u64>(&args[5]).unwrap(), 10_000);   // buyer_quantity
        assert_eq!(bcs::from_bytes::<u64>(&args[6]).unwrap(), 100);      // buyer_batch_id
        assert_eq!(bcs::from_bytes::<Vec<u8>>(&args[7]).unwrap(), vec![0xAA; 32]); // buyer_nonce
        assert_eq!(bcs::from_bytes::<Vec<u8>>(&args[8]).unwrap(), vec![0x11; 64]); // buyer_sig
        assert_eq!(bcs::from_bytes::<u64>(&args[9]).unwrap(), 99);       // seller_nft_id
        assert_eq!(bcs::from_bytes::<u8>(&args[10]).unwrap(), 0);        // seller_side=SELL
        assert_eq!(bcs::from_bytes::<u64>(&args[11]).unwrap(), 4_800_000);// seller_price
        assert_eq!(bcs::from_bytes::<u64>(&args[12]).unwrap(), 10_000);  // seller_quantity
        assert_eq!(bcs::from_bytes::<u64>(&args[13]).unwrap(), 100);     // seller_batch_id
        assert_eq!(bcs::from_bytes::<Vec<u8>>(&args[14]).unwrap(), vec![0xBB; 32]); // seller_nonce
        assert_eq!(bcs::from_bytes::<Vec<u8>>(&args[15]).unwrap(), vec![0x22; 64]); // seller_sig
        assert_eq!(bcs::from_bytes::<Vec<u8>>(&args[16]).unwrap(), b"EMM/KAY".to_vec()); // symbol
        assert_eq!(bcs::from_bytes::<u64>(&args[17]).unwrap(), 5_000);   // fill_quantity
    }

    // ─── T_SETTLE_02: symbol preserved as raw bytes ───────────────

    #[test]
    fn t_settle_02_symbol_preserved_as_raw_bytes() {
        let m = make_test_match();
        let args = build_settle_args(&m).unwrap();
        let symbol: Vec<u8> = bcs::from_bytes(&args[16]).unwrap();
        assert_eq!(symbol, b"EMM/KAY".to_vec());
        assert_eq!(symbol.len(), 10);
    }

    // ─── T_SETTLE_03: estimate_gas success (wiremock) ─────────────

    #[tokio::test]
    async fn t_settle_03_estimate_gas_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/simulate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "Success",
                "output": { "Move": { "gas_used": 5000, "vm_status": "Executed successfully" } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let sub = make_test_submitter(&server.uri());
        let est = sub.estimate_gas(b"fake-signed-tx").await.unwrap();
        assert!(est.success);
        assert_eq!(est.gas_units, 5000);
        assert!(est.abort_code.is_none());
    }

    // ─── T_SETTLE_04: estimate_gas detects abort (wiremock) ───────

    #[tokio::test]
    async fn t_settle_04_estimate_gas_detects_abort() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/simulate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "Fail",
                "output": { "Move": { "gas_used": 100, "vm_status": "Move abort in 0xDEAD::settlement: 13" } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let sub = make_test_submitter(&server.uri());
        let est = sub.estimate_gas(b"fake-signed-tx").await.unwrap();
        assert!(!est.success);
        assert_eq!(est.abort_code, Some(AbortCode::BuyerInactive));
    }

    // ─── T_SETTLE_05: submit returns tx hash (wiremock) ───────────

    #[tokio::test]
    async fn t_settle_05_submit_returns_tx_hash() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/submit"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "\"0xabc123def456\"",
            ))
            .expect(1)
            .mount(&server)
            .await;

        let sub = make_test_submitter(&server.uri());
        let hash = sub.submit(b"fake-signed-tx").await.unwrap();
        assert_eq!(hash, "0xabc123def456");
    }

    // ─── T_SETTLE_06: full pipeline success (wiremock) ────────────

    #[tokio::test]
    async fn t_settle_06_submit_settlement_full_pipeline() {
        let server = MockServer::start().await;

        let (m, p, r) = mock_account_endpoint();
        Mock::given(m).and(p).respond_with(r).mount(&server).await;

        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/simulate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "Success", "output": {"Move": {"gas_used": 3000, "vm_status": "Executed successfully"}}
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/submit"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "\"0xsettled_123\"",
            ))
            .mount(&server)
            .await;

        let sub = make_test_submitter(&server.uri());
        let outcome = sub.submit_settlement(&make_test_match()).await.unwrap();
        match outcome {
            SettlementOutcome::Pending { tx_hash } => assert_eq!(tx_hash, "0xsettled_123"),
            other => panic!("Expected Pending, got {:?}", other),
        }
    }

    // ─── T_SETTLE_07: simulation failure → no submit ──────────────

    #[tokio::test]
    async fn t_settle_07_simulation_failure_skips_submit() {
        let server = MockServer::start().await;

        let (m, p, r) = mock_account_endpoint();
        Mock::given(m).and(p).respond_with(r).mount(&server).await;

        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/simulate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "Fail", "output": { "Move": { "gas_used": 50, "vm_status": "Move abort in 0xDEAD::settlement: 13" } }
            })))
            .mount(&server)
            .await;

        // Submit must NOT be called
        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/submit"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let sub = make_test_submitter(&server.uri());
        let outcome = sub.submit_settlement(&make_test_match()).await.unwrap();
        match outcome {
            SettlementOutcome::Aborted { code } => assert_eq!(code, AbortCode::BuyerInactive),
            other => panic!("Expected Aborted, got {:?}", other),
        }
    }

    // ─── T_SETTLE_08: AbortCode::from_vm_status parsing ──────────

    #[test]
    fn t_settle_08_abort_code_parsing() {
        // Reason codes matching settlement.move E_* constants
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 0"),
            Some(AbortCode::BatchTooOld));       // E_BATCH_TOO_OLD = 0
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 3"),
            Some(AbortCode::InvalidSide));        // E_INVALID_SIDE = 3
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 8"),
            Some(AbortCode::SelfTrade));          // E_SELF_TRADE = 8
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 11"),
            Some(AbortCode::SignatureVerifyFailed)); // E_INVALID_BUYER_SIG = 11
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 12"),
            Some(AbortCode::SignatureVerifyFailed)); // E_INVALID_SELLER_SIG = 12
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 13"),
            Some(AbortCode::BuyerInactive));      // E_BUYER_INACTIVE = 13
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 14"),
            Some(AbortCode::SellerInactive));      // E_SELLER_INACTIVE = 14
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 15"),
            Some(AbortCode::AlreadySettled));      // E_ALREADY_SETTLED = 15
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 21"),
            Some(AbortCode::BuyerOverfill));       // E_BUYER_OVERFILL = 21
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 22"),
            Some(AbortCode::SellerOverfill));      // E_SELLER_OVERFILL = 22

        // Hex format
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 0x1c"),
            Some(AbortCode::NftBlocked));          // E_NFT_BLOCKED = 28 = 0x1c

        // Module-qualified (category << 16 | reason)
        // error::invalid_state(E_BATCH_TOO_OLD) → (3 << 16) | 0 = 0x30000
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 0x30000"),
            Some(AbortCode::BatchTooOld));
        // error::already_exists(E_ALREADY_SETTLED) → (8 << 16) | 15 = 0x8000F
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 0x8000F"),
            Some(AbortCode::AlreadySettled));
        // error::invalid_argument(E_SELF_TRADE) → (1 << 16) | 8 = 0x10008
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 0x10008"),
            Some(AbortCode::SelfTrade));

        // Not an abort → None
        assert_eq!(AbortCode::from_vm_status("Executed successfully"), None);
        assert_eq!(AbortCode::from_vm_status(""), None);

        // Unknown code
        assert_eq!(AbortCode::from_vm_status("Move abort in 0xDEADMKT::settlement: 999"),
            Some(AbortCode::Other(999)));
    }

    // ─── Extra: build_signed_tx produces valid bytes ──────────────

    #[test]
    fn t_settle_extra_build_signed_tx() {
        let sub = make_test_submitter("http://localhost:1234");
        let bytes = sub.build_signed_tx(&make_test_match(), 42, 9999999999).unwrap();
        assert!(!bytes.is_empty());
        assert!(bytes.len() > 100, "got {} bytes", bytes.len());
    }

    // ─── Extra: entry function payload variant tag ─────────────────

    #[test]
    fn t_settle_extra_entry_function_payload() {
        let sub = make_test_submitter("http://localhost:1234");
        let payload = sub.build_entry_function_payload(&make_test_match()).unwrap();
        assert!(!payload.is_empty());
        // TransactionPayload::EntryFunction is variant 2
        assert_eq!(payload[0], 2);
    }

    // ─── Extra: address parsing ───────────────────────────────────

    #[test]
    fn t_settle_extra_parse_address() {
        let addr = parse_address("0x1").unwrap();
        assert_eq!(addr[31], 1);
        assert_eq!(addr[0], 0);

        let full = "0x000000000000000000000000000000000000000000000000000000000000DEAD";
        let addr = parse_address(full).unwrap();
        assert_eq!(addr[30], 0xDE);
        assert_eq!(addr[31], 0xAD);
    }

    // ─── Extra: E_ALREADY_SETTLED in simulation → AlreadySettled ──

    #[tokio::test]
    async fn t_settle_extra_already_settled_simulation() {
        let server = MockServer::start().await;

        let (m, p, r) = mock_account_endpoint();
        Mock::given(m).and(p).respond_with(r).mount(&server).await;

        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/simulate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "Fail", "output": { "Move": { "gas_used": 50, "vm_status": "Move abort in 0xDEAD::settlement: 15" } }
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/submit"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let sub = make_test_submitter(&server.uri());
        let outcome = sub.submit_settlement(&make_test_match()).await.unwrap();
        assert!(matches!(outcome, SettlementOutcome::AlreadySettled));
    }

    // =====================================================================
    // Phase 3 (4B): SettlementManager tests
    // =====================================================================

    fn make_test_manager(own_nft_id: u64) -> (SettlementManager, Arc<Mutex<EscrowTracker>>) {
        let tracker = Arc::new(Mutex::new(EscrowTracker::new()));
        let mgr = SettlementManager::new(tracker.clone(), own_nft_id);
        (mgr, tracker)
    }

    /// Register the standard test match into a manager.
    /// Sets up confirmed balances so the outflow succeeds.
    fn register_standard_match(
        mgr: &mut SettlementManager,
        tracker: &Arc<Mutex<EscrowTracker>>,
    ) -> String {
        let m = make_test_match(); // buyer=42, seller=99, fill=5000, buyer_price=5M
        let match_hash_hex = hex::encode(m.match_hash);
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 10_000_000);
            t.set_confirmed("EMM", 100_000);
        }
        // gas_payer is buyer (nft 42). swap_start=1000, bpb=50 → backstop at 1050.
        mgr.register_match(m, true, 1000, 50, "EMM", "KAY", 5, 5).unwrap();
        match_hash_hex
    }

    fn make_event_for_match(m: &Match) -> BatchTradeSettledEvent {
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

    // ─── T_SETTLE_09: on_settlement_confirmed — pending cleared, tracker updated

    #[test]
    fn t_settle_09_on_settlement_confirmed() {
        let (mut mgr, tracker) = make_test_manager(42); // we are buyer
        let _hash = register_standard_match(&mut mgr, &tracker);

        // Before: pending exists, outflow reserved
        assert_eq!(mgr.pending_count(), 1);
        {
            let t = tracker.lock().unwrap();
            // buyer outflow = 5000 * 5_000_000 / 1e8 = 250 KAY
            assert_eq!(t.projected("KAY"), 10_000_000 - 250);
        }

        // Confirm via event
        let event = make_event_for_match(&make_test_match());
        let was_ours = mgr.on_settlement_confirmed(&event);

        assert!(was_ours);
        assert_eq!(mgr.pending_count(), 0);
        {
            let t = tracker.lock().unwrap();
            // pending_out cleared → projected KAY restored
            assert_eq!(t.projected("KAY"), 10_000_000);
            // inflow applied: EMM confirmed += 5000
            assert_eq!(t.projected("EMM"), 100_000 + 5_000);
        }
    }

    // ─── T_SETTLE_10: on_settlement_confirmed — unrelated event ignored

    #[test]
    fn t_settle_10_unrelated_event_ignored() {
        let (mut mgr, tracker) = make_test_manager(42);
        register_standard_match(&mut mgr, &tracker);

        let unrelated = BatchTradeSettledEvent {
            batch_id: 999,
            pool_id: 1,
            trade_id: "0xUNRELATED".to_string(),
            buyer_nft_id: 1000,
            seller_nft_id: 2000,
            symbol: b"OTHER/TOKEN".to_vec(),
            buyer_price: 200,
            seller_price: 100,
            clearing_price: 100,
            base_amount: 50,
            quote_amount: 50,
            buyer_order_hash: "0xBH".to_string(),
            seller_order_hash: "0xSH".to_string(),
            gas_payer: "0x0002".to_string(),
            timestamp: 0,
        };

        let was_ours = mgr.on_settlement_confirmed(&unrelated);
        assert!(!was_ours);
        assert_eq!(mgr.pending_count(), 1); // still pending
    }

    // ─── T_SETTLE_11: E_ALREADY_SETTLED → KeepPending

    #[test]
    fn t_settle_11_already_settled_keep_pending() {
        let (mut mgr, tracker) = make_test_manager(42);
        let hash = register_standard_match(&mut mgr, &tracker);

        let action = mgr.on_settlement_failed(&hash, AbortCode::AlreadySettled);
        assert_eq!(action, SettlementAction::KeepPending);
        assert_eq!(mgr.pending_count(), 1); // NOT removed
        assert_eq!(mgr.already_settled_count, 1);
    }

    // ─── T_SETTLE_12: E_INSUFFICIENT_BALANCE → KeepPending

    #[test]
    fn t_settle_12_insufficient_balance_keep_pending() {
        let (mut mgr, tracker) = make_test_manager(42);
        let hash = register_standard_match(&mut mgr, &tracker);

        let action = mgr.on_settlement_failed(&hash, AbortCode::InsufficientBalance);
        assert_eq!(action, SettlementAction::KeepPending);
        assert_eq!(mgr.pending_count(), 1);
    }

    // ─── T_SETTLE_13: E_BUYER_INACTIVE (own nft) → EnterLimited

    #[test]
    fn t_settle_13_buyer_inactive_own_nft() {
        let (mut mgr, tracker) = make_test_manager(42); // buyer IS us
        let hash = register_standard_match(&mut mgr, &tracker);

        let action = mgr.on_settlement_failed(&hash, AbortCode::BuyerInactive);
        assert_eq!(action, SettlementAction::EnterLimited);
        assert_eq!(mgr.pending_count(), 0); // released
        {
            let t = tracker.lock().unwrap();
            // outflow reversed
            assert_eq!(t.projected("KAY"), 10_000_000);
        }
    }

    // ─── T_SETTLE_14: E_SELLER_INACTIVE (counterparty) → ReleasePending

    #[test]
    fn t_settle_14_seller_inactive_counterparty() {
        let (mut mgr, tracker) = make_test_manager(42); // seller is 99, not us
        let hash = register_standard_match(&mut mgr, &tracker);

        let action = mgr.on_settlement_failed(&hash, AbortCode::SellerInactive);
        assert_eq!(action, SettlementAction::ReleasePending);
        assert_eq!(mgr.pending_count(), 0); // released
    }

    // ─── T_SETTLE_15: E_BUYER_OVERFILL → ReleasePending

    #[test]
    fn t_settle_15_overfill_releases() {
        let (mut mgr, tracker) = make_test_manager(42);
        let hash = register_standard_match(&mut mgr, &tracker);

        let action = mgr.on_settlement_failed(&hash, AbortCode::BuyerOverfill);
        assert_eq!(action, SettlementAction::ReleasePending);
        assert_eq!(mgr.pending_count(), 0);
    }

    // ─── T_SETTLE_16: check_backstop_eligible — timing logic

    #[test]
    fn t_settle_16_backstop_eligible_timing() {
        let (mut mgr, tracker) = make_test_manager(99); // we are SELLER, not gas payer
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 10_000_000);
            t.set_confirmed("EMM", 100_000);
        }
        let m = make_test_match(); // gas_payer = 42 (buyer)
        let hash = hex::encode(m.match_hash);
        // is_gas_payer = false (we're 99, gas_payer is 42)
        // swap_start=1000, bpb=50 → backstop_eligible_at=1050
        mgr.register_match(m, false, 1000, 50, "EMM", "KAY", 5, 5).unwrap();

        // Before grace period: not eligible
        assert!(mgr.check_backstop_eligible(1049).is_empty());

        // At grace period boundary: eligible
        let eligible = mgr.check_backstop_eligible(1050);
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0], hash);

        // After: still eligible
        assert_eq!(mgr.check_backstop_eligible(1100).len(), 1);
    }

    // ─── T_SETTLE_17: backstop_submit — submits and updates pending (wiremock)

    #[tokio::test]
    async fn t_settle_17_backstop_submit() {
        let server = MockServer::start().await;

        let (m_matcher, p_matcher, r_template) = mock_account_endpoint();
        Mock::given(m_matcher).and(p_matcher).respond_with(r_template).mount(&server).await;

        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/simulate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "Success", "output": {"Move": {"gas_used": 3000, "vm_status": "Executed successfully"}}
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/rpc/v3/transactions/submit"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "\"0xbackstop_hash\"",
            ))
            .mount(&server)
            .await;

        let (mut mgr, tracker) = make_test_manager(99); // seller, not gas payer
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 10_000_000);
            t.set_confirmed("EMM", 100_000);
        }
        let m = make_test_match();
        let hash = hex::encode(m.match_hash);
        mgr.register_match(m, false, 1000, 50, "EMM", "KAY", 5, 5).unwrap();

        let submitter = make_test_submitter(&server.uri());
        let outcome = mgr.backstop_submit(&hash, &submitter).await.unwrap();

        match outcome {
            SettlementOutcome::Pending { tx_hash } => assert_eq!(tx_hash, "0xbackstop_hash"),
            other => panic!("Expected Pending, got {:?}", other),
        }

        // Pending should now have tx_hash set
        let ps = mgr.get_pending(&hash).unwrap();
        assert_eq!(ps.tx_hash.as_deref(), Some("0xbackstop_hash"));
    }

    // ─── T_SETTLE_18: expire_stale — releases old pending settlements

    #[test]
    fn t_settle_18_expire_stale() {
        let (mut mgr, tracker) = make_test_manager(42);
        register_standard_match(&mut mgr, &tracker); // batch_id=100

        // Not expired yet (current=105, max_age=10 → 105 > 100+10 = false)
        mgr.expire_stale(105, 10);
        assert_eq!(mgr.pending_count(), 1);

        // Expired (current=111, max_age=10 → 111 > 100+10 = true)
        mgr.expire_stale(111, 10);
        assert_eq!(mgr.pending_count(), 0);
        {
            let t = tracker.lock().unwrap();
            // outflow reversed
            assert_eq!(t.projected("KAY"), 10_000_000);
        }
    }

    // ─── T_SETTLE_19: isolation detection — triggers above threshold

    #[test]
    fn t_settle_19_isolation_detected() {
        let (mut mgr, tracker) = make_test_manager(42);

        // Not enough data points yet
        assert!(!mgr.is_potentially_isolated());

        // Simulate 6 settlements: 4 E_ALREADY_SETTLED, 2 confirmed
        // We need to register + fail matches. Create unique matches.
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 100_000_000);
            t.set_confirmed("EMM", 100_000_000);
        }

        for i in 0u8..6 {
            let mut m = make_test_match();
            m.match_hash = [i + 1; 32];
            let hash = hex::encode(m.match_hash);
            mgr.register_match(m, true, 1000, 50, "EMM", "KAY", 5, 5).unwrap();
            if i < 4 {
                // E_ALREADY_SETTLED → KeepPending, increments counters
                mgr.on_settlement_failed(&hash, AbortCode::AlreadySettled);
            } else {
                // Confirm normally
                mgr.total_settlement_count += 1; // simulate confirm tracking
            }
        }

        // 4/6 = 66% > 50%, and total >= 5
        assert!(mgr.is_potentially_isolated());
    }

    // ─── T_SETTLE_20: normal backstop races don't trigger isolation

    #[test]
    fn t_settle_20_normal_backstop_no_isolation() {
        let (mut mgr, tracker) = make_test_manager(42);
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 100_000_000);
            t.set_confirmed("EMM", 100_000_000);
        }

        // 10 settlements: 2 E_ALREADY_SETTLED, 8 confirmed
        for i in 0u8..10 {
            let mut m = make_test_match();
            m.match_hash = [i + 1; 32];
            let hash = hex::encode(m.match_hash);
            mgr.register_match(m, true, 1000, 50, "EMM", "KAY", 5, 5).unwrap();
            if i < 2 {
                mgr.on_settlement_failed(&hash, AbortCode::AlreadySettled);
            } else {
                mgr.total_settlement_count += 1;
            }
        }

        // 2/10 = 20% < 50%
        assert!(!mgr.is_potentially_isolated());
    }

    // ─── T_SETTLE_21: register_match applies outflow to escrow tracker

    #[test]
    fn t_settle_21_register_match_applies_outflow() {
        let (mut mgr, tracker) = make_test_manager(42); // we are buyer
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 10_000);
            t.set_confirmed("EMM", 0);
        }

        let m = make_test_match(); // fill=5000, buyer_price=5_000_000
        let hash = hex::encode(m.match_hash);
        mgr.register_match(m, true, 1000, 50, "EMM", "KAY", 5, 5).unwrap();

        // buyer outflow = 5000 * 5_000_000 / 100_000_000 = 250
        {
            let t = tracker.lock().unwrap();
            assert_eq!(t.projected("KAY"), 10_000 - 250);
            // inflow not counted in projected
            assert_eq!(t.projected("EMM"), 0);
        }

        assert!(mgr.is_pending(&hash));
        assert_eq!(mgr.pending_count(), 1);
    }

    // ─── #10: register_skipped increments counter + releases outflow ────

    #[test]
    fn t_settle_30_register_skipped_after_register_match() {
        let (mut mgr, tracker) = make_test_manager(42);
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 10_000);
        }
        let m = make_test_match();
        let hash = hex::encode(m.match_hash);
        mgr.register_match(m, true, 1000, 50, "EMM", "KAY", 5, 5).unwrap();
        assert_eq!(tracker.lock().unwrap().projected("KAY"), 9_750);

        mgr.register_skipped(&hash, SkipReason::Inactive { nft_id: 7 });
        assert_eq!(mgr.skipped_inactive_total(), 1);
        assert!(!mgr.is_pending(&hash), "pending should be cleared");
        // Outflow released — full balance restored.
        assert_eq!(tracker.lock().unwrap().projected("KAY"), 10_000);
    }

    #[test]
    fn t_settle_31_register_skipped_without_prior_register() {
        // Caller filtered before register_match — should still count.
        let (mut mgr, _tracker) = make_test_manager(42);
        mgr.register_skipped("ffff", SkipReason::Inactive { nft_id: 7 });
        assert_eq!(mgr.skipped_inactive_total(), 1);
    }

    // ─── T_SETTLE_22: process_result Submitted → mark_submitted ───

    #[test]
    fn t_settle_22_process_result_submitted() {
        let (mut mgr, tracker) = make_test_manager(42);
        let hash = register_standard_match(&mut mgr, &tracker);

        let action = mgr.process_result(SettleResult::Submitted {
            match_hash: hash.clone(),
            tx_hash: "0xdeadbeef".to_string(),
        });
        assert!(action.is_none()); // no action needed
        assert!(mgr.is_pending(&hash)); // still pending
        assert_eq!(mgr.get_pending(&hash).unwrap().tx_hash.as_deref(), Some("0xdeadbeef"));
    }

    // ─── T_SETTLE_23: process_result AlreadySettled → KeepPending ─

    #[test]
    fn t_settle_23_process_result_already_settled() {
        let (mut mgr, tracker) = make_test_manager(42);
        let hash = register_standard_match(&mut mgr, &tracker);

        let action = mgr.process_result(SettleResult::AlreadySettled {
            match_hash: hash.clone(),
        });
        assert_eq!(action, Some(SettlementAction::KeepPending));
        assert!(mgr.is_pending(&hash)); // kept pending, waiting for event
    }

    // ─── T_SETTLE_24: process_result Failed → routes abort code ───

    #[test]
    fn t_settle_24_process_result_failed() {
        let (mut mgr, tracker) = make_test_manager(42);
        let hash = register_standard_match(&mut mgr, &tracker);

        let action = mgr.process_result(SettleResult::Failed {
            match_hash: hash.clone(),
            code: AbortCode::BuyerOverfill,
        });
        assert_eq!(action, Some(SettlementAction::ReleasePending));
        assert!(!mgr.is_pending(&hash)); // released
        {
            let t = tracker.lock().unwrap();
            assert_eq!(t.projected("KAY"), 10_000_000); // outflow reversed
        }
    }

    // ─── T_SETTLE_25: process_result RpcError → keep pending ──────

    #[test]
    fn t_settle_25_process_result_rpc_error() {
        let (mut mgr, tracker) = make_test_manager(42);
        let hash = register_standard_match(&mut mgr, &tracker);

        let action = mgr.process_result(SettleResult::RpcError {
            match_hash: hash.clone(),
            error: "connection refused".to_string(),
        });
        assert!(action.is_none()); // no action — keep pending
        assert!(mgr.is_pending(&hash)); // still there for retry/expiry
    }
}
