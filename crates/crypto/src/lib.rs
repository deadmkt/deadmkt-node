// deadmkt-crypto: BCS encoding, Ed25519 signing, order/commit hashing
//
// This is the most critical module in the node. If BCS field ordering
// or Ed25519 signature format differs between Rust and Move, all
// settlement transactions will fail.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

// =========================================================================
// Error types
// =========================================================================

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("BCS serialization failed: {0}")]
    BcsError(String),

    #[error("signature verification failed")]
    VerificationFailed,

    #[error("invalid key length: expected {expected}, got {got}")]
    InvalidKeyLength { expected: usize, got: usize },
}

// =========================================================================
// Order struct -- MUST match settlement.move field order exactly
// =========================================================================

/// Order struct for BCS serialization.
///
/// CRITICAL: Field order MUST match settlement.move's Order struct:
///   nft_id: u64, symbol: vector<u8>, side: u8,
///   price: u64, quantity: u64, batch_id: u64, nonce: vector<u8>
///
/// BCS serializes structs in field declaration order. If a field is added,
/// removed, or reordered on either side, every signature will fail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Order {
    pub nft_id: u64,
    pub symbol: Vec<u8>,
    pub side: u8,
    pub price: u64,
    pub quantity: u64,
    pub batch_id: u64,
    pub nonce: Vec<u8>,
}

// =========================================================================
// Key generation
// =========================================================================

/// Generate a new Ed25519 keypair using OS randomness.
pub fn generate_keypair() -> (SigningKey, VerifyingKey) {
    let secret = SigningKey::generate(&mut OsRng);
    let public = secret.verifying_key();
    (secret, public)
}

// =========================================================================
// Signing and verification
// =========================================================================

/// Sign raw bytes with an Ed25519 signing key.
pub fn sign(secret: &SigningKey, message: &[u8]) -> Vec<u8> {
    let sig: Signature = secret.sign(message);
    sig.to_bytes().to_vec()
}

/// Verify an Ed25519 signature against a public key and message.
pub fn verify(public: &VerifyingKey, message: &[u8], signature: &[u8]) -> bool {
    if signature.len() != 64 {
        return false;
    }
    let sig_bytes: [u8; 64] = signature.try_into().unwrap();
    let sig = match Signature::from_bytes(&sig_bytes) {
        sig => sig,
    };
    public.verify(message, &sig).is_ok()
}

// =========================================================================
// Order operations
// =========================================================================

/// BCS-encode an Order struct.
pub fn encode_order(order: &Order) -> Result<Vec<u8>, CryptoError> {
    bcs::to_bytes(order).map_err(|e| CryptoError::BcsError(e.to_string()))
}

/// Compute order hash: SHA256(BCS(order))
pub fn order_hash(order: &Order) -> Vec<u8> {
    let bytes = encode_order(order).expect("BCS encoding of Order should never fail");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    hasher.finalize().to_vec()
}

/// Commit hash is identical to order hash.
/// Separate function to ensure the codebase never accidentally diverges.
pub fn commit_hash(order: &Order) -> Vec<u8> {
    order_hash(order)
}

/// Sign an order: Ed25519.sign(sk, BCS(order))
pub fn sign_order(secret: &SigningKey, order: &Order) -> Vec<u8> {
    let bytes = encode_order(order).expect("BCS encoding of Order should never fail");
    sign(secret, &bytes)
}

/// Sign a commit: Ed25519.sign(sk, batch_id_le || pool_id_le || order_hash)
///
/// Message format matches settlement.move's commit verification:
///   BCS(batch_id: u64) + BCS(pool_id: u64) + commit_hash (32 bytes)
pub fn sign_commit(
    secret: &SigningKey,
    batch_id: u64,
    pool_id: u64,
    hash: &[u8],
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8 + 8 + hash.len());
    msg.extend_from_slice(&batch_id.to_le_bytes());
    msg.extend_from_slice(&pool_id.to_le_bytes());
    msg.extend_from_slice(hash);
    sign(secret, &msg)
}

/// Compute match_hash: SHA256(batch_id_le || buyer_order_hash || seller_order_hash || fill_quantity_le)
pub fn compute_match_hash(
    batch_id: u64,
    buyer_order_hash: &[u8],
    seller_order_hash: &[u8],
    fill_quantity: u64,
) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(batch_id.to_le_bytes());
    hasher.update(buyer_order_hash);
    hasher.update(seller_order_hash);
    hasher.update(fill_quantity.to_le_bytes());
    hasher.finalize().to_vec()
}

/// Sign a BatchComplete message: Ed25519 over
///   (batch_id || pool_id || symbol || avg_settlement_price || volume || match_count)
///
/// Message format:
///   batch_id (8 bytes LE) || pool_id (8 bytes LE) ||
///   symbol_len (1 byte ULEB128) || symbol (raw bytes) ||
///   avg_settlement_price (8 bytes LE) || volume (8 bytes LE) || match_count (8 bytes LE)
///
/// Symbol uses BCS-style length prefix for unambiguous variable-length encoding.
pub fn sign_batch_complete(
    secret: &SigningKey,
    batch_id: u64,
    pool_id: u64,
    symbol: &[u8],
    avg_settlement_price: u64,
    volume: u64,
    match_count: u64,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8 + 8 + 1 + symbol.len() + 8 + 8 + 8);
    msg.extend_from_slice(&batch_id.to_le_bytes());
    msg.extend_from_slice(&pool_id.to_le_bytes());
    // BCS-style length-prefixed symbol (ULEB128 length + raw bytes)
    let symbol_bcs = bcs::to_bytes(&symbol.to_vec())
        .expect("BCS encoding of Vec<u8> should not fail");
    msg.extend_from_slice(&symbol_bcs);
    msg.extend_from_slice(&avg_settlement_price.to_le_bytes());
    msg.extend_from_slice(&volume.to_le_bytes());
    msg.extend_from_slice(&match_count.to_le_bytes());
    sign(secret, &msg)
}

/// Build the raw bytes for verifying a BatchComplete signature.
pub fn batch_complete_message(
    batch_id: u64,
    pool_id: u64,
    symbol: &[u8],
    avg_settlement_price: u64,
    volume: u64,
    match_count: u64,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8 + 8 + 1 + symbol.len() + 8 + 8 + 8);
    msg.extend_from_slice(&batch_id.to_le_bytes());
    msg.extend_from_slice(&pool_id.to_le_bytes());
    let symbol_bcs = bcs::to_bytes(&symbol.to_vec())
        .expect("BCS encoding of Vec<u8> should not fail");
    msg.extend_from_slice(&symbol_bcs);
    msg.extend_from_slice(&avg_settlement_price.to_le_bytes());
    msg.extend_from_slice(&volume.to_le_bytes());
    msg.extend_from_slice(&match_count.to_le_bytes());
    msg
}

/// Sign a SettlementReport message: Ed25519 over
///   (batch_id || match_hash || settled || bailer_nft)
///
/// Message format:
///   batch_id (8 bytes LE) || match_hash (32 bytes raw) ||
///   settled (1 byte: 0=false, 1=true) || bailer_nft (8 bytes LE)
pub fn sign_settlement_report(
    secret: &SigningKey,
    batch_id: u64,
    match_hash: &[u8],
    settled: bool,
    bailer_nft: u64,
) -> Vec<u8> {
    let msg = settlement_report_message(batch_id, match_hash, settled, bailer_nft);
    sign(secret, &msg)
}

/// Build the raw bytes for verifying a SettlementReport signature.
pub fn settlement_report_message(
    batch_id: u64,
    match_hash: &[u8],
    settled: bool,
    bailer_nft: u64,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8 + 32 + 1 + 8);
    msg.extend_from_slice(&batch_id.to_le_bytes());
    msg.extend_from_slice(match_hash);
    msg.push(if settled { 1u8 } else { 0u8 });
    msg.extend_from_slice(&bailer_nft.to_le_bytes());
    msg
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_keypair() -> (SigningKey, VerifyingKey) {
        let secret = SigningKey::from_bytes(&[1u8; 32]);
        let public = secret.verifying_key();
        (secret, public)
    }

    fn make_test_order() -> Order {
        Order {
            nft_id: 42,
            symbol: b"EMM/KAY".to_vec(),
            side: 1, // BUY
            price: 5_000_000,
            quantity: 50_000_000_000,
            batch_id: 100,
            nonce: vec![0xAA; 32],
        }
    }

    // T_CRYPTO_01
    #[test]
    fn test_keypair_generation() {
        let (secret, public) = generate_keypair();
        assert_eq!(public.as_bytes().len(), 32);
        assert_eq!(secret.as_bytes().len(), 32);

        let msg = b"test message";
        let sig = sign(&secret, msg);
        assert!(verify(&public, msg, &sig));
    }

    // T_CRYPTO_02
    #[test]
    fn test_order_bcs_field_order() {
        let order = Order {
            nft_id: 1,
            symbol: b"EMM/KAY".to_vec(),
            side: 1,
            price: 5_000_000,
            quantity: 50_000_000_000,
            batch_id: 100,
            nonce: vec![0u8; 32],
        };

        let bytes = encode_order(&order).unwrap();

        // 8 + (1+10) + 1 + 8 + 8 + 8 + (1+32) = 77
        assert_eq!(bytes.len(), 77);
        assert_eq!(&bytes[0..8], &[1, 0, 0, 0, 0, 0, 0, 0]); // nft_id=1 LE
        assert_eq!(bytes[8], 10); // symbol length
        assert_eq!(&bytes[9..19], b"EMM/KAY");
        assert_eq!(bytes[19], 1); // side=BUY
    }

    // T_CRYPTO_03
    #[test]
    fn test_bcs_determinism() {
        let order = make_test_order();
        let bytes1 = encode_order(&order).unwrap();
        let bytes2 = encode_order(&order).unwrap();
        assert_eq!(bytes1, bytes2);
    }

    // T_CRYPTO_04
    #[test]
    fn test_order_hash() {
        let order = make_test_order();
        let hash = order_hash(&order);
        assert_eq!(hash.len(), 32);

        let hash2 = order_hash(&order);
        assert_eq!(hash, hash2);

        let mut order2 = make_test_order();
        order2.nonce = vec![1u8; 32];
        let hash3 = order_hash(&order2);
        assert_ne!(hash, hash3);
    }

    // T_CRYPTO_05
    #[test]
    fn test_order_signature() {
        let (secret, public) = generate_keypair();
        let order = make_test_order();

        let sig = sign_order(&secret, &order);
        assert_eq!(sig.len(), 64);

        let order_bytes = encode_order(&order).unwrap();
        assert!(verify(&public, &order_bytes, &sig));
    }

    // T_CRYPTO_06
    #[test]
    fn test_commit_hash_equals_order_hash() {
        let order = make_test_order();
        let oh = order_hash(&order);
        let ch = commit_hash(&order);
        assert_eq!(oh, ch);
    }

    // T_CRYPTO_07
    #[test]
    fn test_commit_signature() {
        let (secret, public) = generate_keypair();
        let batch_id: u64 = 100;
        let pool_id: u64 = 3;
        let hash = [0xABu8; 32];

        let sig = sign_commit(&secret, batch_id, pool_id, &hash);
        assert_eq!(sig.len(), 64);

        let mut msg = Vec::new();
        msg.extend_from_slice(&batch_id.to_le_bytes());
        msg.extend_from_slice(&pool_id.to_le_bytes());
        msg.extend_from_slice(&hash);
        assert!(verify(&public, &msg, &sig));
    }

    // T_CRYPTO_09
    #[test]
    fn test_bcs_vector_encoding() {
        let data: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let encoded = bcs::to_bytes(&data).unwrap();
        assert_eq!(encoded[0], 4);
        assert_eq!(&encoded[1..], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    // T_CRYPTO_10
    #[test]
    fn test_bcs_u8_encoding() {
        let side: u8 = 1;
        let encoded = bcs::to_bytes(&side).unwrap();
        assert_eq!(encoded.len(), 1);
        assert_eq!(encoded[0], 1);
    }

    // T_CRYPTO_11
    #[test]
    fn test_match_hash_computation() {
        let batch_id: u64 = 100;
        let buyer_hash = [0xAAu8; 32];
        let seller_hash = [0xBBu8; 32];
        let fill_qty: u64 = 500_000_000;

        let mh = compute_match_hash(batch_id, &buyer_hash, &seller_hash, fill_qty);
        assert_eq!(mh.len(), 32);

        let mh2 = compute_match_hash(batch_id, &buyer_hash, &seller_hash, fill_qty);
        assert_eq!(mh, mh2);

        let mh3 = compute_match_hash(batch_id, &seller_hash, &buyer_hash, fill_qty);
        assert_ne!(mh, mh3);
    }

    // Extra: wrong key
    #[test]
    fn test_verify_wrong_key() {
        let (secret1, _) = test_keypair();
        let (_, public2) = generate_keypair();
        let sig = sign(&secret1, b"test message");
        assert!(!verify(&public2, b"test message", &sig));
    }

    // Extra: wrong message
    #[test]
    fn test_verify_wrong_message() {
        let (secret, public) = test_keypair();
        let sig = sign(&secret, b"original message");
        assert!(!verify(&public, b"tampered message", &sig));
    }

    // Extra: bad signature length
    #[test]
    fn test_verify_bad_signature_length() {
        let (_, public) = test_keypair();
        assert!(!verify(&public, b"msg", &[0u8; 63]));
        assert!(!verify(&public, b"msg", &[0u8; 65]));
    }

    // Cross-check: BCS bytes match B1 PyNaCl fixtures
    #[test]
    fn test_order_bcs_matches_b1_fixture() {
        let order = Order {
            nft_id: 1,
            symbol: b"KAY/TEE".to_vec(),
            side: 1, // BUY
            price: 5_000_000_000_000,
            quantity: 100_000_000,
            batch_id: 0,
            nonce: vec![0x00, 0x01],
        };

        let bytes = encode_order(&order).unwrap();
        let expected = hex::decode(
            "0100000000000000084254432d5553444301005039278c04000000e1f505000000000000000000000000020001"
        ).unwrap();

        assert_eq!(bytes, expected, "BCS bytes must match B1 PyNaCl fixture exactly");
    }

    // T_CRYPTO_EXT_01
    #[test]
    fn test_sign_batch_complete_roundtrip() {
        let (secret, public) = generate_keypair();
        let batch_id: u64 = 500;
        let pool_id: u64 = 2;
        let symbol = b"EMM/KAY";
        let avg_price: u64 = 49_000_000;
        let volume: u64 = 10_000_000_000;
        let match_count: u64 = 5;

        let sig = sign_batch_complete(
            &secret, batch_id, pool_id, symbol, avg_price, volume, match_count,
        );
        assert_eq!(sig.len(), 64);

        // Verify using the same message construction
        let msg = batch_complete_message(
            batch_id, pool_id, symbol, avg_price, volume, match_count,
        );
        assert!(verify(&public, &msg, &sig));

        // Wrong symbol fails
        let wrong_msg = batch_complete_message(
            batch_id, pool_id, b"KAY/TEE", avg_price, volume, match_count,
        );
        assert!(!verify(&public, &wrong_msg, &sig));
    }

    // T_CRYPTO_EXT_02
    #[test]
    fn test_sign_settlement_report_roundtrip() {
        let (secret, public) = generate_keypair();
        let batch_id: u64 = 500;
        let match_hash = [0xABu8; 32];
        let settled = true;
        let bailer_nft: u64 = 0;

        let sig = sign_settlement_report(&secret, batch_id, &match_hash, settled, bailer_nft);
        assert_eq!(sig.len(), 64);

        // Verify
        let msg = settlement_report_message(batch_id, &match_hash, settled, bailer_nft);
        assert!(verify(&public, &msg, &sig));

        // Different settled flag fails
        let wrong_msg = settlement_report_message(batch_id, &match_hash, false, bailer_nft);
        assert!(!verify(&public, &wrong_msg, &sig));

        // Different bailer_nft fails
        let wrong_msg2 = settlement_report_message(batch_id, &match_hash, settled, 42);
        assert!(!verify(&public, &wrong_msg2, &sig));
    }
}
