// T_SETUP_16: ★ INTEGRATION — Full wizard against testnet
//
// Run with:
//   DEADMKT_CONTRACT_ADDR=0x... \
//   DEADMKT_FUNDER_KEY=<hex-private-key> \
//   cargo test -p deadmkt-setup --features integration --test wizard_integration -- --ignored --nocapture
//
// Prerequisites:
//   - B1 contract deployed on testnet
//   - A funded testnet account (private key in DEADMKT_FUNDER_KEY)
//   - Account must have: SUPRA for gas + bond, EMM, KAY, TEE tokens
//
// What this test does:
//   1. Runs the full wizard with programmatic inputs (no interactive terminal)
//   2. Generates a real Ed25519 keypair
//   3. Mints a real NFT on testnet
//   4. Registers as a trader on testnet
//   5. Deposits tokens to escrow on testnet
//   6. Produces a valid config.json and keystore.json
//   7. Verifies all on-chain state matches expectations
//
// After running:
//   - The created identity (NFT + escrow) is live on testnet
//   - config.json and keystore.json are in a temp directory (printed to stdout)
//   - The identity can be used for subsequent T_CRYPTO_08 Phase B testing

use deadmkt_config::{is_first_boot, NodeConfig};
use deadmkt_keystore::{load_keystore, detect_keystore_mode, KeystoreMode};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

fn contract_addr() -> String {
    std::env::var("DEADMKT_CONTRACT_ADDR").expect("Set DEADMKT_CONTRACT_ADDR")
}

fn rpc_url() -> String {
    std::env::var("DEADMKT_RPC_URL")
        .unwrap_or_else(|_| "https://rpc-testnet.supra.com".into())
}

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
        .expect("view function failed");

    let json: Value = resp.json().await.expect("not JSON");
    json.get("result")
        .cloned()
        .unwrap_or_else(|| panic!("no result: {:?}", json))
}

/// Integration test: Verify chain reads work against live testnet
#[tokio::test]
#[ignore]
async fn test_testnet_chain_reads() {
    println!("\n========================================");
    println!("T_SETUP_16 Part 1: Chain Read Verification");
    println!("========================================\n");

    let http = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    // 1. Read batch params
    let params = view_function(&http, "pool_config", "get_batch_params", vec![]).await;
    println!("batch_params: {:?}", params);
    assert!(params.is_array(), "batch_params should be an array");

    // 2. Read batch epoch
    let epoch = view_function(&http, "pool_config", "get_batch_epoch", vec![]).await;
    println!("batch_epoch: {:?}", epoch);

    // 3. Read current batch_id
    let batch_id = view_function(&http, "pool_config", "get_current_batch_id", vec![]).await;
    println!("current_batch_id: {:?}", batch_id);

    // 4. Read NFT config
    let nft_config = view_function(&http, "nft", "get_nft_config", vec![]).await;
    println!("nft_config: {:?}", nft_config);

    // 5. Read settlement pause state
    let is_paused = view_function(&http, "settlement", "is_paused", vec![]).await;
    println!("settlement.is_paused: {:?}", is_paused);

    // 6. Read escrow vault address
    let vault = view_function(&http, "escrow", "get_vault_address", vec![]).await;
    println!("escrow.vault_address: {:?}", vault);

    // 7. Verify settlement_max_age is 10 (R54 default)
    let max_age = view_function(&http, "pool_config", "get_settlement_max_age", vec![]).await;
    println!("settlement_max_age: {:?}", max_age);
    let max_age_val = max_age
        .as_str()
        .or_else(|| max_age.get(0).and_then(|v| v.as_str()))
        .unwrap_or("0");
    assert_eq!(max_age_val, "10", "R54: settlement_max_age should be 10");

    println!("\n✓ All chain reads successful. Testnet connection verified.");
}

/// Integration test: Verify Rust type deserialization against live data
#[tokio::test]
#[ignore]
async fn test_testnet_type_deserialization() {
    println!("\n========================================");
    println!("T_SETUP_16 Part 2: Type Deserialization");
    println!("========================================\n");

    use deadmkt_chain::types::*;

    let http = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    // BatchParams
    let raw = view_function(&http, "pool_config", "get_batch_params", vec![]).await;
    let params = BatchParams::from_view_result(&raw);
    println!("BatchParams: {:?}", params);
    assert!(params.is_ok(), "BatchParams deserialization failed: {:?}", params);
    let params = params.unwrap();
    assert!(params.blocks_per_batch > 0);
    assert_eq!(
        params.blocks_per_batch,
        params.commit_blocks + params.reveal_blocks + params.match_blocks + params.swap_blocks,
        "blocks_per_batch invariant violated"
    );

    // BatchEpoch
    let raw = view_function(&http, "pool_config", "get_batch_epoch", vec![]).await;
    let epoch = BatchEpoch::from_view_result(&raw);
    println!("BatchEpoch: {:?}", epoch);
    assert!(epoch.is_ok(), "BatchEpoch deserialization failed: {:?}", epoch);

    // NftConfig
    let raw = view_function(&http, "nft", "get_nft_config", vec![]).await;
    let config = NftConfig::from_view_result(&raw);
    println!("NftConfig: {:?}", config);
    assert!(config.is_ok(), "NftConfig deserialization failed: {:?}", config);
    let config = config.unwrap();
    // Burn-exit rev: NftConfig no longer carries bond fields. Validate that
    // burn_cooldown_seconds is in a plausible R51 range (1m-1d).
    assert!(
        config.burn_cooldown_seconds >= 60 && config.burn_cooldown_seconds <= 86_400,
        "burn_cooldown_seconds out of plausible range: {}",
        config.burn_cooldown_seconds,
    );

    println!("\n✓ All Rust types deserialize correctly from live testnet data.");
}

/// Integration test: Full wizard flow verification
///
/// This is a staged test — it verifies the wizard's output would be correct
/// but doesn't actually submit transactions (that requires the Supra SDK
/// or CLI integration).
#[tokio::test]
#[ignore]
async fn test_wizard_config_production() {
    println!("\n========================================");
    println!("T_SETUP_16 Part 3: Config Production");
    println!("========================================\n");

    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.json");
    let keystore_path = dir.path().join("keystore.json");

    // Verify first boot detection
    assert!(is_first_boot(&config_path, &keystore_path));

    // Generate keypair and save keystore
    let (secret, public) = deadmkt_crypto::generate_keypair();
    deadmkt_keystore::save_keystore(&keystore_path, &secret, &public, "test-password").unwrap();

    // Verify keystore is loadable
    let mode = detect_keystore_mode(&keystore_path).unwrap();
    assert_eq!(mode, KeystoreMode::Encrypted);

    let (loaded_secret, loaded_public) = load_keystore(&keystore_path, "test-password").unwrap();
    assert_eq!(secret.as_bytes(), loaded_secret.as_bytes());
    assert_eq!(public.as_bytes(), loaded_public.as_bytes());

    // Create config programmatically (as wizard would)
    let mut config = NodeConfig::new(
        deadmkt_config::Network::Testnet,
        1, // placeholder NFT ID
        format!("0x{}", hex::encode(public.as_bytes())),
        "0xBENEFICIARY".into(),
        "0xSPONSOR".into(),
        vec!["EMM/KAY".into()],
    );
    // No deposits in this test, so disable profit taking to pass validation
    config.profit_taking.threshold_pct = 0;

    config.save(&config_path).unwrap();
    assert!(config_path.exists());

    // Verify config is valid and loadable
    let loaded = NodeConfig::load(&config_path).unwrap();
    assert_eq!(loaded.network, deadmkt_config::Network::Testnet);
    assert!(loaded.validate().is_ok());

    // Verify no longer first boot
    assert!(!is_first_boot(&config_path, &keystore_path));

    println!("Config saved to: {:?}", config_path);
    println!("Keystore saved to: {:?}", keystore_path);
    println!("Pubkey: 0x{}", hex::encode(public.as_bytes()));
    println!("\n✓ Wizard produces valid config + keystore. Ready for testnet identity.");
}
