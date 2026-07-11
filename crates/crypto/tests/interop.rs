// T_CRYPTO_08: ★ INTEROP — Rust sign → Move verify
//
// THE SINGLE MOST IMPORTANT TEST IN BUILD 2.
//
// This test proves that:
//   1. Ed25519 keypairs generated in Rust are accepted by Supra's Move VM
//   2. BCS encoding in Rust matches Move's BCS encoding exactly
//   3. Signatures produced by ed25519-dalek pass Move's ed25519::verify
//
// Run with:
//   DEADMKT_CONTRACT_ADDR=0x... \
//   DEADMKT_FUNDER_KEY=<hex-encoded-private-key> \
//   cargo test -p deadmkt-crypto --features integration --test interop -- --ignored --nocapture
//
// Prerequisites:
//   - B1 contract deployed on testnet (address in DEADMKT_CONTRACT_ADDR)
//   - A funded testnet account (private key in DEADMKT_FUNDER_KEY)
//   - The funded account must have enough SUPRA for gas + bond (10K SUPRA)
//
// What this test does:
//   Phase A: Key Format Verification
//     1. Generate Ed25519 keypair in Rust
//     2. Mint NFT with the Rust pubkey via supra CLI or REST
//     3. Read pubkey back from chain via get_trustee_pubkey(nft_id)
//     4. Verify pubkey matches byte-for-byte
//
//   Phase B: Full Signature Interop (settle_match)
//     5. Generate a second keypair (seller)
//     6. Mint second NFT, register both traders, deposit tokens
//     7. Sign orders in Rust with BCS encoding
//     8. Submit settle_match — if it succeeds, interop is proven
//     9. If it aborts with E_INVALID_BUYER_SIG or E_INVALID_SELLER_SIG,
//        interop is BROKEN and we must debug BCS/signature format

use deadmkt_crypto::{encode_order, generate_keypair, sign_order, Order};
use ed25519_dalek::SigningKey;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

// =========================================================================
// Env helpers
// =========================================================================

fn contract_addr() -> String {
    std::env::var("DEADMKT_CONTRACT_ADDR").expect(
        "Set DEADMKT_CONTRACT_ADDR to the deployed contract address",
    )
}

#[allow(dead_code)]
fn funder_key() -> SigningKey {
    let hex_key = std::env::var("DEADMKT_FUNDER_KEY").expect(
        "Set DEADMKT_FUNDER_KEY to a hex-encoded Ed25519 private key with funds",
    );
    let bytes = hex::decode(&hex_key).expect("DEADMKT_FUNDER_KEY must be valid hex");
    assert_eq!(bytes.len(), 32, "private key must be 32 bytes");
    SigningKey::from_bytes(bytes.as_slice().try_into().unwrap())
}

fn rpc_url() -> String {
    std::env::var("DEADMKT_RPC_URL")
        .unwrap_or_else(|_| "https://rpc-testnet.supra.com".into())
}

// =========================================================================
// REST helpers
// =========================================================================

async fn view_function(
    client: &Client,
    module: &str,
    function: &str,
    args: Vec<Value>,
) -> Value {
    let addr = contract_addr();
    let url = format!("{}/rpc/v2/view", rpc_url());
    let body = json!({
        "function": format!("{}::{}::{}", addr, module, function),
        "type_arguments": [],
        "arguments": args,
    });

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .expect("view function request failed");

    let json: Value = resp.json().await.expect("view function response not JSON");
    json.get("result")
        .cloned()
        .unwrap_or_else(|| panic!("view function returned no result: {:?}", json))
}

// =========================================================================
// Phase A: Key Format Verification
// =========================================================================

#[tokio::test]
#[ignore]
async fn test_interop_phase_a_key_format() {
    println!("\n========================================");
    println!("T_CRYPTO_08 Phase A: Key Format Verification");
    println!("========================================\n");

    let addr = contract_addr();
    let http = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    // Step 1: Generate keypair in Rust
    let (_secret, public) = generate_keypair();
    let pubkey_hex = hex::encode(public.as_bytes());
    println!("Generated Rust Ed25519 pubkey: {}", pubkey_hex);

    // Step 2: Read current nft config to know bond amount
    let nft_config = view_function(&http, "nft", "get_nft_config", vec![]).await;
    println!("NFT config: {:?}", nft_config);

    // Step 3: Read current next_nft_id
    let next_id = view_function(&http, "nft", "get_next_nft_id", vec![]).await;
    println!("Next NFT ID: {:?}", next_id);

    // Step 4: Mint NFT pair
    // NOTE: This requires submitting a transaction. Since we don't have a
    // full Supra Rust SDK for BCS transaction encoding, this step uses
    // the supra CLI tool. Run this manually:
    println!("\n--- MANUAL STEP ---");
    println!("Run the following command to mint an NFT with the Rust pubkey:");
    println!(
        "  supra move tool run \\",
    );
    println!("    --function-id {}::nft::mint_trustee_nft \\", addr);
    println!("    --args 'hex:{}' 'address:{}' \\", pubkey_hex, addr);  // ed25519_pubkey, sponsor
    println!("    --rpc-url {}", rpc_url());
    println!();
    println!(
        "Then set DEADMKT_TEST_NFT_ID=<nft_id> and re-run Phase B."
    );
    println!("--- END MANUAL STEP ---\n");

    // If NFT_ID is set, verify the pubkey was stored correctly
    if let Ok(nft_id_str) = std::env::var("DEADMKT_TEST_NFT_ID") {
        let nft_id: u64 = nft_id_str.parse().expect("invalid NFT ID");
        println!("Verifying pubkey for NFT #{}", nft_id);

        let stored_pubkey = view_function(
            &http,
            "nft",
            "get_trustee_pubkey",
            vec![json!(nft_id.to_string())],
        )
        .await;

        println!("Stored pubkey on chain: {:?}", stored_pubkey);

        // The view function returns the pubkey as a hex string (0x-prefixed)
        // Compare byte-for-byte
        let stored_hex = stored_pubkey
            .as_str()
            .or_else(|| stored_pubkey.get(0).and_then(|v| v.as_str()))
            .expect("pubkey not returned as string");
        let stored_clean = stored_hex.trim_start_matches("0x");

        assert_eq!(
            stored_clean.to_lowercase(),
            pubkey_hex.to_lowercase(),
            "CRITICAL: Pubkey stored on chain doesn't match Rust-generated pubkey!"
        );
        println!("✓ Pubkey verified: Rust Ed25519 key format is compatible with Move");
    }
}

// =========================================================================
// Phase B: Full Signature Interop (settle_match)
// =========================================================================

#[tokio::test]
#[ignore]
async fn test_interop_phase_b_settle_match() {
    println!("\n========================================");
    println!("T_CRYPTO_08 Phase B: Signature Interop");
    println!("========================================\n");

    // This test requires:
    //   DEADMKT_BUYER_NFT_ID  — NFT minted with Rust-generated buyer keypair
    //   DEADMKT_SELLER_NFT_ID — NFT minted with Rust-generated seller keypair
    //   DEADMKT_BUYER_KEY     — hex private key for buyer
    //   DEADMKT_SELLER_KEY    — hex private key for seller
    //
    // Both NFTs must be registered as traders with escrow deposits.

    let buyer_key_hex = match std::env::var("DEADMKT_BUYER_KEY") {
        Ok(k) => k,
        Err(_) => {
            println!("DEADMKT_BUYER_KEY not set. Skipping Phase B.");
            println!("To run Phase B, set up two funded identities:");
            println!("  1. Generate two keypairs (Phase A above)");
            println!("  2. Mint NFTs for both");
            println!("  3. Register both as traders");
            println!("  4. Deposit tokens to both escrows");
            println!("  5. Set DEADMKT_BUYER_KEY, DEADMKT_SELLER_KEY, etc.");
            return;
        }
    };

    let seller_key_hex = std::env::var("DEADMKT_SELLER_KEY")
        .expect("DEADMKT_SELLER_KEY required for Phase B");
    let buyer_nft_id: u64 = std::env::var("DEADMKT_BUYER_NFT_ID")
        .expect("DEADMKT_BUYER_NFT_ID required")
        .parse()
        .unwrap();
    let seller_nft_id: u64 = std::env::var("DEADMKT_SELLER_NFT_ID")
        .expect("DEADMKT_SELLER_NFT_ID required")
        .parse()
        .unwrap();

    let buyer_key = SigningKey::from_bytes(
        hex::decode(&buyer_key_hex).unwrap().as_slice().try_into().unwrap(),
    );
    let seller_key = SigningKey::from_bytes(
        hex::decode(&seller_key_hex).unwrap().as_slice().try_into().unwrap(),
    );

    let http = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    // Read current batch_id
    let batch_result = view_function(&http, "pool_config", "get_current_batch_id", vec![]).await;
    let batch_id: u64 = batch_result
        .as_str()
        .or_else(|| batch_result.get(0).and_then(|v| v.as_str()))
        .unwrap()
        .parse()
        .unwrap();
    println!("Current batch_id: {}", batch_id);

    // Create and sign orders in Rust
    let buyer_nonce: Vec<u8> = (0..32).map(|i| i as u8).collect();
    let seller_nonce: Vec<u8> = (32..64).map(|i| i as u8).collect();

    let buyer_order = Order {
        nft_id: buyer_nft_id,
        symbol: b"EMM/KAY".to_vec(),
        side: 1, // BUY
        price: 5_000_000_000_000,      // 50,000 at 8 decimals
        quantity: 100_000_000,          // 1 token at 8 decimals
        batch_id,
        nonce: buyer_nonce.clone(),
    };

    let seller_order = Order {
        nft_id: seller_nft_id,
        symbol: b"EMM/KAY".to_vec(),
        side: 0, // SELL
        price: 5_000_000_000_000,
        quantity: 100_000_000,
        batch_id,
        nonce: seller_nonce.clone(),
    };

    let buyer_sig = sign_order(&buyer_key, &buyer_order);
    let seller_sig = sign_order(&seller_key, &seller_order);

    println!("Buyer order BCS ({} bytes): {}", encode_order(&buyer_order).unwrap().len(),
        hex::encode(encode_order(&buyer_order).unwrap()));
    println!("Buyer signature: {}", hex::encode(&buyer_sig));
    println!("Seller order BCS ({} bytes): {}", encode_order(&seller_order).unwrap().len(),
        hex::encode(encode_order(&seller_order).unwrap()));
    println!("Seller signature: {}", hex::encode(&seller_sig));

    // Print the supra CLI command to submit settle_match
    let addr = contract_addr();
    println!("\n--- SETTLE_MATCH COMMAND ---");
    println!("Submit this transaction to verify Rust signatures on-chain:");
    println!(
        "  supra move tool run \\
    --function-id {}::settlement::settle_match \\
    --args \\
      u64:{} \\
      u64:0 \\
      u64:{} \\
      u8:1 \\
      u64:{} \\
      u64:{} \\
      u64:{} \\
      'hex:{}' \\
      'hex:{}' \\
      u64:{} \\
      u8:0 \\
      u64:{} \\
      u64:{} \\
      u64:{} \\
      'hex:{}' \\
      'hex:{}' \\
      'hex:{}' \\
      u64:{} \\
    --rpc-url {}",
        addr,
        batch_id,                               // batch_id
        buyer_nft_id,                           // buyer_nft_id
        buyer_order.price,                      // buyer_price
        buyer_order.quantity,                   // buyer_quantity
        buyer_order.batch_id,                   // buyer_batch_id
        hex::encode(&buyer_nonce),              // buyer_nonce
        hex::encode(&buyer_sig),                // buyer_signature
        seller_nft_id,                          // seller_nft_id
        seller_order.price,                     // seller_price
        seller_order.quantity,                  // seller_quantity
        seller_order.batch_id,                  // seller_batch_id
        hex::encode(&seller_nonce),             // seller_nonce
        hex::encode(&seller_sig),               // seller_signature
        hex::encode(b"EMM/KAY"),             // symbol
        buyer_order.quantity,                   // fill_quantity
        rpc_url(),
    );
    println!("--- END ---\n");

    println!("If settle_match succeeds: ✓ INTEROP PROVEN");
    println!("If E_INVALID_BUYER_SIG or E_INVALID_SELLER_SIG: ✗ BCS/SIGNATURE MISMATCH");
    println!("Any other error (E_INSUFFICIENT_BALANCE, etc): Setup issue, not interop issue");
}

// =========================================================================
// Offline BCS verification (always runs, not #[ignore])
// =========================================================================

/// This test verifies BCS encoding matches B1 fixtures exactly.
/// It runs without network access and serves as a confidence check
/// even when the full interop test can't run.
#[test]
fn test_bcs_cross_check_b1_fixtures() {
    // Buyer order from B1 settlement tests (test_settle_match_happy):
    // nft_id=1, symbol="BTC-USDC", side=BUY(1), price=5000000000000,
    // quantity=100000000, batch_id=0, nonce=x"0001"
    let buyer_order = Order {
        nft_id: 1,
        symbol: b"BTC-USDC".to_vec(),
        side: 1,
        price: 5_000_000_000_000,
        quantity: 100_000_000,
        batch_id: 0,
        nonce: vec![0x00, 0x01],
    };

    let buyer_bcs = encode_order(&buyer_order).unwrap();

    // Expected BCS from Python/B1 (verified on-chain in test_settle_match_happy):
    let expected = hex::decode(
        "0100000000000000084254432d5553444301005039278c04000000e1f505000000000000000000000000020001"
    ).unwrap();

    assert_eq!(
        buyer_bcs, expected,
        "BCS encoding MUST match B1 fixture. If this fails, settle_match will fail on-chain."
    );

    // Seller order: same but side=SELL(0), nonce=x"0002", nft_id=2
    let seller_order = Order {
        nft_id: 2,
        symbol: b"BTC-USDC".to_vec(),
        side: 0,
        price: 5_000_000_000_000,
        quantity: 100_000_000,
        batch_id: 0,
        nonce: vec![0x00, 0x02],
    };

    let seller_bcs = encode_order(&seller_order).unwrap();
    let expected_seller = hex::decode(
        "0200000000000000084254432d5553444300005039278c04000000e1f505000000000000000000000000020002"
    ).unwrap();

    assert_eq!(seller_bcs, expected_seller);

    println!("✓ BCS encoding matches B1 on-chain verified fixtures");
}
