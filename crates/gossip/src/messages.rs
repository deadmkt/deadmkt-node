// deadmkt-gossip::messages
//
// Wire protocol message types for gossip exchange.
// Serialized with bincode for gossip (fast, compact).
// BCS is only used for order signing/settlement interop.
//
// UPG-1: All wire messages are wrapped in a GossipEnvelope with a
// protocol version. Nodes reject messages from incompatible versions.

use deadmkt_crypto::Order;
use serde::{Deserialize, Serialize};
use std::fmt;

// =========================================================================
// Protocol version
// =========================================================================

/// Current gossip wire protocol version.
/// Bump this when the GossipMessage enum changes in a breaking way.
pub const PROTOCOL_VERSION: u16 = 1;

// =========================================================================
// Codec error
// =========================================================================

#[derive(Debug)]
pub enum GossipCodecError {
    Bincode(bincode::Error),
    VersionMismatch { got: u16, expected: u16 },
}

impl fmt::Display for GossipCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GossipCodecError::Bincode(e) => write!(f, "bincode: {}", e),
            GossipCodecError::VersionMismatch { got, expected } => {
                write!(f, "gossip version mismatch: got v{}, expected v{}", got, expected)
            }
        }
    }
}

impl std::error::Error for GossipCodecError {}

impl From<bincode::Error> for GossipCodecError {
    fn from(e: bincode::Error) -> Self {
        GossipCodecError::Bincode(e)
    }
}

// =========================================================================
// Versioned envelope (wire format)
// =========================================================================

/// Wire-level envelope. Every gossip message on the network is wrapped in this.
/// The version field allows nodes to reject incompatible protocol versions
/// without attempting to deserialize the inner message.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GossipEnvelope {
    pub version: u16,
    pub message: GossipMessage,
}

// =========================================================================
// Gossip message types
// =========================================================================

/// All messages exchanged over the gossip network.
///
/// Commitment and Reveal flow on pool topics ("pool:{pool_id}").
/// BatchComplete flows on "global:batch_complete".
/// SettlementReport flows on "global:settlement_report".
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum GossipMessage {
    /// COMMIT phase: blind order commitment (hash only, order hidden).
    Commitment {
        batch_id: u64,
        pool_id: u64,
        hash: [u8; 32],        // SHA256(BCS(order))
        nft_id: u64,
        signature: Vec<u8>,    // 64 bytes, Ed25519 over (batch_id_LE || pool_id_LE || hash)
    },

    /// REVEAL phase: full order + signature (proves commit was honest).
    Reveal {
        batch_id: u64,
        pool_id: u64,
        order: Order,           // Canonical order — nonce is Vec<u8>
        signature: Vec<u8>,     // 64 bytes, Ed25519 over BCS(order)
    },

    /// SWAP phase: cross-pool summary. One per symbol per pool per batch.
    BatchComplete {
        batch_id: u64,
        pool_id: u64,
        symbol: Vec<u8>,
        avg_settlement_price: u64,
        volume: u64,
        match_count: u64,
        num_commits: u32,
        num_reveals: u32,
        sender_nft_id: u64,
        signature: Vec<u8>,    // 64 bytes
    },

    /// Settlement outcome report (any phase, async).
    SettlementReport {
        batch_id: u64,
        match_hash: [u8; 32],
        settled: bool,
        bailer_nft: u64,       // 0 if settled=true or abort code (not a bail)
        reporter_nft_id: u64,
        signature: Vec<u8>,    // 64 bytes
    },
}

// =========================================================================
// Serialization helpers (envelope-aware)
// =========================================================================

/// Serialize a GossipMessage to bytes, wrapped in a versioned envelope.
pub fn serialize(msg: &GossipMessage) -> Result<Vec<u8>, GossipCodecError> {
    let envelope = GossipEnvelope {
        version: PROTOCOL_VERSION,
        message: msg.clone(),
    };
    Ok(bincode::serialize(&envelope)?)
}

/// Deserialize bytes to a GossipMessage. Rejects version mismatches.
pub fn deserialize(bytes: &[u8]) -> Result<GossipMessage, GossipCodecError> {
    let envelope: GossipEnvelope = bincode::deserialize(bytes)?;
    if envelope.version != PROTOCOL_VERSION {
        return Err(GossipCodecError::VersionMismatch {
            got: envelope.version,
            expected: PROTOCOL_VERSION,
        });
    }
    Ok(envelope.message)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_sig() -> Vec<u8> {
        vec![0xAA; 64]
    }

    fn dummy_order() -> Order {
        Order {
            nft_id: 42,
            symbol: b"EMM/KAY".to_vec(),
            side: 1,
            price: 5_000_000,
            quantity: 50_000_000_000,
            batch_id: 100,
            nonce: vec![0xBB; 32],
        }
    }

    // T_MSG_01: Commitment roundtrip (through versioned envelope)
    #[test]
    fn test_msg_01_commitment_roundtrip() {
        let msg = GossipMessage::Commitment {
            batch_id: 100,
            pool_id: 3,
            hash: [0xCC; 32],
            nft_id: 42,
            signature: dummy_sig(),
        };

        let bytes = serialize(&msg).unwrap();
        let decoded = deserialize(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    // T_MSG_02: Reveal roundtrip — Order.nonce survives as Vec<u8>
    #[test]
    fn test_msg_02_reveal_roundtrip() {
        let order = dummy_order();
        let msg = GossipMessage::Reveal {
            batch_id: 100,
            pool_id: 3,
            order: order.clone(),
            signature: dummy_sig(),
        };

        let bytes = serialize(&msg).unwrap();
        let decoded = deserialize(&bytes).unwrap();
        assert_eq!(msg, decoded);

        // Verify nonce survived
        if let GossipMessage::Reveal { order: decoded_order, .. } = &decoded {
            assert_eq!(decoded_order.nonce, vec![0xBB; 32]);
            assert_eq!(decoded_order.nft_id, 42);
            assert_eq!(decoded_order.symbol, b"EMM/KAY");
        } else {
            panic!("expected Reveal variant");
        }
    }

    // T_MSG_03: BatchComplete roundtrip
    #[test]
    fn test_msg_03_batch_complete_roundtrip() {
        let msg = GossipMessage::BatchComplete {
            batch_id: 100,
            pool_id: 3,
            symbol: b"EMM/KAY".to_vec(),
            avg_settlement_price: 49_000_000,
            volume: 10_000_000_000,
            match_count: 5,
            num_commits: 12,
            num_reveals: 8,
            sender_nft_id: 42,
            signature: dummy_sig(),
        };

        let bytes = serialize(&msg).unwrap();
        let decoded = deserialize(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    // T_MSG_04: SettlementReport roundtrip
    #[test]
    fn test_msg_04_settlement_report_roundtrip() {
        let msg = GossipMessage::SettlementReport {
            batch_id: 100,
            match_hash: [0xDD; 32],
            settled: true,
            bailer_nft: 0,
            reporter_nft_id: 42,
            signature: dummy_sig(),
        };

        let bytes = serialize(&msg).unwrap();
        let decoded = deserialize(&bytes).unwrap();
        assert_eq!(msg, decoded);

        // Also test settled=false case
        let msg2 = GossipMessage::SettlementReport {
            batch_id: 100,
            match_hash: [0xDD; 32],
            settled: false,
            bailer_nft: 99,
            reporter_nft_id: 42,
            signature: dummy_sig(),
        };

        let bytes2 = serialize(&msg2).unwrap();
        let decoded2 = deserialize(&bytes2).unwrap();
        assert_eq!(msg2, decoded2);
    }

    // T_MSG_05: Corrupt bytes → deserialization error
    #[test]
    fn test_msg_05_corrupt_bytes() {
        let garbage = vec![0xFF, 0xFE, 0xFD, 0xFC, 0x00];
        let result = deserialize(&garbage);
        assert!(result.is_err());

        // Truncated valid message
        let msg = GossipMessage::Commitment {
            batch_id: 100,
            pool_id: 3,
            hash: [0xCC; 32],
            nft_id: 42,
            signature: dummy_sig(),
        };
        let bytes = serialize(&msg).unwrap();
        let truncated = &bytes[..bytes.len() / 2];
        assert!(deserialize(truncated).is_err());
    }

    // T_MSG_06: Envelope contains correct version
    #[test]
    fn test_msg_06_envelope_version() {
        let msg = GossipMessage::Commitment {
            batch_id: 100,
            pool_id: 3,
            hash: [0xCC; 32],
            nft_id: 42,
            signature: dummy_sig(),
        };

        let bytes = serialize(&msg).unwrap();

        // Deserialize as raw envelope to inspect version
        let envelope: GossipEnvelope = bincode::deserialize(&bytes).unwrap();
        assert_eq!(envelope.version, PROTOCOL_VERSION);
        assert_eq!(envelope.message, msg);
    }

    // T_MSG_07: Version mismatch rejected
    #[test]
    fn test_msg_07_version_mismatch_rejected() {
        let msg = GossipMessage::Commitment {
            batch_id: 100,
            pool_id: 3,
            hash: [0xCC; 32],
            nft_id: 42,
            signature: dummy_sig(),
        };

        // Manually create envelope with wrong version
        let bad_envelope = GossipEnvelope {
            version: 999,
            message: msg,
        };
        let bytes = bincode::serialize(&bad_envelope).unwrap();

        let result = deserialize(&bytes);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("version mismatch"));
        assert!(err_msg.contains("999"));
    }

    // T_MSG_08: Version 0 (pre-envelope bare message) rejected
    #[test]
    fn test_msg_08_bare_message_rejected() {
        // Simulate a pre-UPG-1 node sending a bare GossipMessage (no envelope)
        let msg = GossipMessage::Commitment {
            batch_id: 100,
            pool_id: 3,
            hash: [0xCC; 32],
            nft_id: 42,
            signature: dummy_sig(),
        };
        let bare_bytes = bincode::serialize(&msg).unwrap();

        // This will either fail to deserialize as an envelope, or
        // produce a garbage version number — either way, rejected.
        let result = deserialize(&bare_bytes);
        assert!(result.is_err());
    }
}
