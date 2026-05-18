// deadmkt-node: Binary entry point
//
// CLI subcommands:
//   (none)       → run node (default)
//   setup        → first-boot wizard
//   status       → show node state summary
//   escrow       → show escrow balances
//   show-token   → print strategy auth token
//   version      → print version

mod cli;
mod run;
mod setup_bridge;
mod token_worker;

use clap::Parser;
use cli::{Cli, Command};
use deadmkt_config::{default_contract_addresses, is_first_boot, Network, NodeConfig};
use deadmkt_keystore::{detect_keystore_mode, KeystoreMode};
use std::path::PathBuf;

fn data_dir() -> PathBuf {
    let dir = dirs_or_default();
    std::fs::create_dir_all(&dir).expect("failed to create data directory");
    dir
}

fn dirs_or_default() -> PathBuf {
    std::env::var("DEADMKT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".deadmkt")
        })
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // MR1a/b: --config triggers non-interactive setup BEFORE subcommand
    // routing. Routes to fresh (no keystore) or LLM-assisted (keystore
    // present + interactive password prompt) depending on data-dir state.
    if let Some(config_path) = cli.config.as_ref() {
        run_noninteractive_setup_and_exit(config_path).await;
    }

    let command = cli.resolve_command();

    // MR1c: restore-from-backup. Triggered when the operator runs plain
    // `deadmkt-node` (default Run) with a keystore in the data dir but
    // no config.json -- i.e. they restored their keystore backup and
    // need the node to reconstruct the config from chain state. Other
    // subcommands (status, escrow, setup, etc.) are not intercepted.
    if matches!(command, Command::Run) {
        let dir = data_dir();
        let keystore_path = dir.join("keystore.json");
        let config_path = dir.join("config.json");
        if keystore_path.exists() && !config_path.exists() {
            run_restore_setup_and_exit(&dir).await;
        }
    }

    match command {
        Command::Version => {
            println!("deadmkt-node v{}", env!("CARGO_PKG_VERSION"));
        }
        Command::Setup => {
            println!("deadmkt-node v{} — Setup Wizard\n", env!("CARGO_PKG_VERSION"));
            let dir = data_dir();

            let config_path = dir.join("config.json");
            let keystore_path = dir.join("keystore.json");
            if !is_first_boot(&config_path, &keystore_path) {
                println!("Node already configured at {}", dir.display());
                println!("Delete config.json and keystore.json to re-run setup.");
                return;
            }

            let mut io = setup_bridge::StdIO;
            // Single source of truth: default_contract_addresses() in deadmkt-config.
            // DEADMKT_CONTRACT_ADDR env override is applied later by NodeConfig::apply_env_overrides().
            let defaults = default_contract_addresses(&Network::Testnet);
            let contract_addr = std::env::var("DEADMKT_CONTRACT_ADDR")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or(defaults.settlement);
            let chain = setup_bridge::SupraSetupClient::new(
                vec!["https://rpc-testnet.supra.com".into()],
                contract_addr,
            );

            match deadmkt_setup::run_wizard(&mut io, &chain, &dir).await {
                Ok(config) => {
                    println!("\nSetup complete!");
                    println!("  Network: {:?}", config.network);
                    println!("  NFT ID:  {}", config.nft_id);
                    println!("  Config:  {}", config_path.display());
                    println!("\nRun 'deadmkt-node' to start trading.");
                }
                Err(e) => {
                    eprintln!("\nSetup failed: {}", e);
                    // #11: actionable hint when the wizard hits an on-chain
                    // gas ceiling. vm_status from the chain contains the
                    // phrase "Out of gas" on OOG aborts; bumping the env
                    // override is usually the fix until the code default
                    // catches up with testnet gas drift.
                    let msg = e.to_string();
                    if msg.to_lowercase().contains("out of gas") {
                        let current_max = std::env::var("DEADMKT_MAX_GAS")
                            .unwrap_or_else(|_| "5000 (default)".into());
                        eprintln!("\nThis looks like a gas ceiling hit. Try:");
                        eprintln!("  DEADMKT_MAX_GAS=20000 deadmkt-node setup ...");
                        eprintln!("  (current max_gas_amount: {})", current_max);
                        eprintln!("If the error repeats at higher values, Supra testnet's gas");
                        eprintln!("schedule may have drifted -- simulate the failing tx or");
                        eprintln!("check a recent successful tx's gas_used to calibrate.");
                    }
                    eprintln!("\nYou can re-run 'deadmkt-node setup' to try again.");
                }
            }
        }
        Command::Status { json } => {
            let dir = data_dir();
            let config_path = dir.join("config.json");

            // MR2: --json takes precedence and goes through the
            // status endpoint with a disk-only fallback. Exit code
            // is always 0; consumers branch on `node_running`.
            if json {
                run_status_json_and_exit(&config_path).await;
            }

            // Default human-readable path (unchanged behaviour).
            match NodeConfig::load(&config_path) {
                Ok(config) => {
                    println!("Network: {:?}", config.network);
                    println!("NFT ID: {}", config.nft_id);
                    println!("Markets: {:?}", config.markets);
                }
                Err(_) => {
                    println!("Node not configured. Run 'deadmkt-node setup' first.");
                }
            }
        }
        Command::Escrow => {
            println!("Escrow balances:");
            println!("  (Requires running node — escrow tracker not loaded)");
        }
        Command::ShowToken => {
            println!("Strategy auth token:");
            println!("  (Token generated at node startup)");
        }
        Command::Deposit { token, amount } => {
            println!("deadmkt-node — Deposit to Escrow\n");

            let dir = data_dir();
            let config_path = dir.join("config.json");
            let keystore_path = dir.join("keystore.json");

            // Load config
            let config = NodeConfig::load(&config_path).expect("failed to load config");
            println!("  NFT ID:   {}", config.nft_id);
            println!("  Trustee:  {}", config.trustee_address);

            // Load keystore
            let mode = detect_keystore_mode(&keystore_path).unwrap_or(KeystoreMode::Missing);
            let (signing_key, _) = match mode {
                KeystoreMode::Insecure => {
                    deadmkt_keystore::load_keystore_insecure(&keystore_path)
                        .expect("failed to load keystore")
                }
                KeystoreMode::Encrypted => {
                    let password = std::env::var("DEADMKT_KEYSTORE_PASSWORD")
                        .unwrap_or_else(|_| {
                            use std::io::{self, Write};
                            print!("  Password: ");
                            io::stdout().flush().unwrap();
                            let mut pw = String::new();
                            io::stdin().read_line(&mut pw).unwrap();
                            pw.trim().to_string()
                        });
                    deadmkt_keystore::load_keystore(&keystore_path, &password)
                        .expect("failed to decrypt keystore")
                }
                KeystoreMode::Missing => {
                    eprintln!("No keystore found. Run 'deadmkt-node setup' first.");
                    std::process::exit(1);
                }
            };

            // Parse addresses
            let sender_addr = deadmkt_settlement::parse_address(&config.trustee_address)
                .expect("invalid trustee address");
            let contract_addr = deadmkt_settlement::parse_address(&config.contracts.escrow)
                .expect("invalid contract address");
            let token_addr = deadmkt_settlement::parse_address(&token)
                .expect("invalid token metadata address");

            // Build deposit args: (token_metadata: address, amount: u64)
            let args = vec![
                bcs::to_bytes(&token_addr).expect("bcs encode token addr"),
                bcs::to_bytes(&amount).expect("bcs encode amount"),
            ];

            // Chain ID
            let chain_id = match config.network {
                deadmkt_config::Network::Testnet => 6u8,
                deadmkt_config::Network::Mainnet => 1u8,
            };

            let client = deadmkt_chain::client::SupraClient::new(
                config.rpc_urls.clone(),
                config.contracts.escrow.clone(),
            );

            let sender_hex = format!("0x{}", hex::encode(sender_addr));
            let account = client.get_account(&sender_hex).await
                .expect("failed to get account info");

            let expiry = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() + 60;

            let signed_tx = deadmkt_settlement::build_and_sign_entry_function(
                &signing_key,
                sender_addr,
                contract_addr,
                "escrow",
                "deposit",
                args,
                account.sequence_number,
                expiry,
                chain_id,
                config.max_gas_amount,
                config.gas_unit_price,
            ).expect("failed to build transaction");

            println!("  Depositing {} units of {} into escrow...", amount, token);

            match client.submit_raw(&signed_tx).await {
                Ok(hash) => {
                    println!("  Transaction submitted: {}", hash);
                    println!("  Deposit complete.");
                }
                Err(e) => {
                    eprintln!("  Transaction failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Command::Reactivate => {
            println!("deadmkt-node — Reactivate Inactive Node\n");

            let dir = data_dir();
            let config_path = dir.join("config.json");
            let keystore_path = dir.join("keystore.json");

            let config = NodeConfig::load(&config_path).expect("failed to load config");
            println!("  NFT ID:   {}", config.nft_id);
            println!("  Trustee:  {}", config.trustee_address);

            let client = deadmkt_chain::client::SupraClient::new(
                config.rpc_urls.clone(),
                config.contracts.settlement.clone(),
            );

            // Step 1: Check if active
            let is_active = match client.view_raw(
                "escrow", "is_active", vec![],
                vec![serde_json::json!(config.nft_id.to_string())],
            ).await {
                Ok(r) => r.get(0).and_then(|v| v.as_bool()).unwrap_or(false),
                Err(e) => {
                    eprintln!("  Failed to check active status: {}", e);
                    std::process::exit(1);
                }
            };

            if is_active {
                println!("  Status: ACTIVE — no reactivation needed.");
                return;
            }
            println!("  Status: INACTIVE");

            // Load keystore + addresses once -- shared by reactivate and mint paths.
            let mode = detect_keystore_mode(&keystore_path).unwrap_or(KeystoreMode::Missing);
            let (signing_key, _) = match mode {
                KeystoreMode::Insecure => {
                    deadmkt_keystore::load_keystore_insecure(&keystore_path)
                        .expect("failed to load keystore")
                }
                KeystoreMode::Encrypted => {
                    let password = std::env::var("DEADMKT_KEYSTORE_PASSWORD")
                        .unwrap_or_else(|_| {
                            use std::io::{self, Write};
                            print!("  Password: ");
                            io::stdout().flush().unwrap();
                            let mut pw = String::new();
                            io::stdin().read_line(&mut pw).unwrap();
                            pw.trim().to_string()
                        });
                    deadmkt_keystore::load_keystore(&keystore_path, &password)
                        .expect("failed to decrypt keystore")
                }
                KeystoreMode::Missing => {
                    eprintln!("No keystore found.");
                    std::process::exit(1);
                }
            };

            let sender_addr = deadmkt_settlement::parse_address(&config.trustee_address)
                .expect("invalid trustee address");
            let contract_addr = deadmkt_settlement::parse_address(&config.contracts.settlement)
                .expect("invalid contract address");
            let chain_id = match config.network {
                deadmkt_config::Network::Testnet => 6u8,
                deadmkt_config::Network::Mainnet => 1u8,
            };
            let sender_hex = format!("0x{}", hex::encode(sender_addr));

            // Step 2: try escrow::reactivate() for instant recovery.
            println!("  Attempting escrow::reactivate()...");

            let account = client.get_account(&sender_hex).await
                .expect("failed to get account info");
            let expiry = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() + 60;
            let signed_tx = deadmkt_settlement::build_and_sign_entry_function(
                &signing_key,
                sender_addr,
                contract_addr,
                "escrow",
                "reactivate",
                vec![],
                account.sequence_number,
                expiry,
                chain_id,
                config.max_gas_amount,
                config.gas_unit_price,
            ).expect("failed to build transaction");

            let mut fall_back_to_mint = false;
            match client.submit_raw(&signed_tx).await {
                Ok(hash) => {
                    println!("  reactivate submitted: {}", hash);
                    match client.wait_for_tx(&hash, std::time::Duration::from_secs(30)).await {
                        Ok(r) if r.success => {
                            println!("  Reactivated — node is active.");
                            return;
                        }
                        Ok(r) => {
                            let vs = r.vm_status.as_str();
                            // E_INSUFFICIENT_ESCROW(22): escrow is empty, only mint can recover.
                            // E_GRACE_NOT_MET(19): past reactivate grace window, only mint can recover.
                            if vs.contains("E_INSUFFICIENT_ESCROW") || vs.contains("E_GRACE_NOT_MET") {
                                println!("  reactivate() rejected ({}) — falling back to mint.", vs);
                                fall_back_to_mint = true;
                            } else if vs.contains("E_HOLDING_EXPIRED") {
                                eprintln!("  Wind-down holding period has expired.");
                                eprintln!("  Node is exiting — use claim_all or rushed_withdrawal, not reactivate.");
                                std::process::exit(1);
                            } else if vs.contains("E_ALREADY_ACTIVE") {
                                // Race: activity between our is_active view and the tx.
                                println!("  Node became active between check and submit — done.");
                                return;
                            } else {
                                eprintln!("  reactivate failed: {}", vs);
                                std::process::exit(1);
                            }
                        }
                        Err(e) => {
                            eprintln!("  reactivate tx status check failed: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("  reactivate submit failed: {}", e);
                    std::process::exit(1);
                }
            }

            if !fall_back_to_mint {
                return;
            }

            // Step 3: mint fallback -- either claim an existing pending mint or start a new one.
            let pending_mint = match client.view_raw(
                "tokens", "get_pending_mint", vec![],
                vec![serde_json::json!(config.trustee_address)],
            ).await {
                Ok(r) => {
                    r.get(0)
                        .and_then(|v| v.get("vec"))
                        .and_then(|v| v.get(0))
                        .cloned()
                },
                Err(_) => None,
            };

            if let Some(pm) = pending_mint {
                let claimable_at = pm.get("claimable_at")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                if claimable_at > 0 && now >= claimable_at {
                    println!("  Pending mint is CLAIMABLE — submitting claim_mint...");

                    let account = client.get_account(&sender_hex).await
                        .expect("failed to get account info");
                    let expiry = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() + 60;
                    let signed_tx = deadmkt_settlement::build_and_sign_entry_function(
                        &signing_key,
                        sender_addr,
                        contract_addr,
                        "tokens",
                        "claim_mint",
                        vec![],
                        account.sequence_number,
                        expiry,
                        chain_id,
                        config.max_gas_amount,
                        config.gas_unit_price,
                    ).expect("failed to build transaction");

                    match client.submit_raw(&signed_tx).await {
                        Ok(hash) => {
                            println!("  claim_mint submitted: {}", hash);
                            println!("  Node should reactivate once deposit confirms.");
                        }
                        Err(e) => {
                            eprintln!("  claim_mint failed: {}", e);
                            std::process::exit(1);
                        }
                    }
                } else if claimable_at > 0 {
                    let wait = claimable_at - now;
                    println!("  Pending mint not yet claimable — {}h {}m remaining.",
                        wait / 3600, (wait % 3600) / 60);
                    println!("  Run this command again after the hold period expires.");
                } else {
                    println!("  Pending mint found but claimable_at unknown.");
                }
            } else {
                println!("  No pending mint — submitting minimum mint (10 tokens, 1 SUPRA)...");

                let args = vec![
                    bcs::to_bytes(&400000u64).expect("bcs"),
                    bcs::to_bytes(&300000u64).expect("bcs"),
                    bcs::to_bytes(&300000u64).expect("bcs"),
                ];

                let account = client.get_account(&sender_hex).await
                    .expect("failed to get account info");
                let expiry = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() + 60;
                let signed_tx = deadmkt_settlement::build_and_sign_entry_function(
                    &signing_key,
                    sender_addr,
                    contract_addr,
                    "tokens",
                    "request_mint",
                    args,
                    account.sequence_number,
                    expiry,
                    chain_id,
                    config.max_gas_amount,
                    config.gas_unit_price,
                ).expect("failed to build transaction");

                match client.submit_raw(&signed_tx).await {
                    Ok(hash) => {
                        println!("  request_mint submitted: {}", hash);
                        println!("  Mint hold period will apply. Run 'deadmkt-node reactivate'");
                        println!("  again after hold expires to claim and reactivate.");
                    }
                    Err(e) => {
                        eprintln!("  request_mint failed: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Command::ApplyParams => {
            println!("deadmkt-node — Apply Pending Params\n");

            let dir = data_dir();
            let config_path = dir.join("config.json");
            let keystore_path = dir.join("keystore.json");

            let config = NodeConfig::load(&config_path).expect("failed to load config");

            let mode = detect_keystore_mode(&keystore_path).unwrap_or(KeystoreMode::Missing);
            let (signing_key, _) = match mode {
                KeystoreMode::Insecure => {
                    deadmkt_keystore::load_keystore_insecure(&keystore_path)
                        .expect("failed to load keystore")
                }
                KeystoreMode::Encrypted => {
                    let password = std::env::var("DEADMKT_KEYSTORE_PASSWORD")
                        .unwrap_or_else(|_| {
                            use std::io::{self, Write};
                            print!("  Password: ");
                            io::stdout().flush().unwrap();
                            let mut pw = String::new();
                            io::stdin().read_line(&mut pw).unwrap();
                            pw.trim().to_string()
                        });
                    deadmkt_keystore::load_keystore(&keystore_path, &password)
                        .expect("failed to decrypt keystore")
                }
                KeystoreMode::Missing => {
                    eprintln!("No keystore found. Run 'deadmkt-node setup' first.");
                    std::process::exit(1);
                }
            };

            let sender_addr = deadmkt_settlement::parse_address(&config.trustee_address)
                .expect("invalid trustee address");
            let contract_addr = deadmkt_settlement::parse_address(&config.contracts.pool_config)
                .expect("invalid pool_config address");

            let chain_id = match config.network {
                deadmkt_config::Network::Testnet => 6u8,
                deadmkt_config::Network::Mainnet => 1u8,
            };

            let client = deadmkt_chain::client::SupraClient::new(
                config.rpc_urls.clone(),
                config.contracts.pool_config.clone(),
            );

            let sender_hex = format!("0x{}", hex::encode(sender_addr));
            let account = client.get_account(&sender_hex).await
                .expect("failed to get account info");

            let expiry = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() + 60;

            let signed_tx = deadmkt_settlement::build_and_sign_entry_function(
                &signing_key,
                sender_addr,
                contract_addr,
                "pool_config",
                "auto_adjust_pools",
                vec![],  // no args
                account.sequence_number,
                expiry,
                chain_id,
                config.max_gas_amount,
                config.gas_unit_price,
            ).expect("failed to build transaction");

            println!("  Calling auto_adjust_pools()...");

            match client.submit_raw(&signed_tx).await {
                Ok(hash) => {
                    println!("  Transaction submitted: {}", hash);
                    println!("  Pending params applied.");
                }
                Err(e) => {
                    eprintln!("  Transaction failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Command::UpdateParams {
            blocks_per_batch, commit_blocks, reveal_blocks,
            match_blocks, swap_blocks, commits_per_batch,
            settlement_max_age, rushed_grace_batches,
        } => {
            println!("deadmkt-node — Update Batch Params (admin)\n");

            let dir = data_dir();
            let config_path = dir.join("config.json");
            let keystore_path = dir.join("keystore.json");

            let config = NodeConfig::load(&config_path).expect("failed to load config");
            println!("  Trustee:  {}", config.trustee_address);

            // Validate locally
            let sum = commit_blocks + reveal_blocks + match_blocks + swap_blocks;
            assert_eq!(sum, blocks_per_batch, "phase blocks must sum to blocks_per_batch");
            println!("  New params: {}/batch ({}/{}/{}/{})",
                blocks_per_batch, commit_blocks, reveal_blocks, match_blocks, swap_blocks);

            // Load keystore
            let mode = detect_keystore_mode(&keystore_path).unwrap_or(KeystoreMode::Missing);
            let (signing_key, _) = match mode {
                KeystoreMode::Insecure => {
                    deadmkt_keystore::load_keystore_insecure(&keystore_path)
                        .expect("failed to load keystore")
                }
                KeystoreMode::Encrypted => {
                    let password = std::env::var("DEADMKT_KEYSTORE_PASSWORD")
                        .unwrap_or_else(|_| {
                            use std::io::{self, Write};
                            print!("  Password: ");
                            io::stdout().flush().unwrap();
                            let mut pw = String::new();
                            io::stdin().read_line(&mut pw).unwrap();
                            pw.trim().to_string()
                        });
                    deadmkt_keystore::load_keystore(&keystore_path, &password)
                        .expect("failed to decrypt keystore")
                }
                KeystoreMode::Missing => {
                    eprintln!("No keystore found. Run 'deadmkt-node setup' first.");
                    std::process::exit(1);
                }
            };

            let sender_addr = deadmkt_settlement::parse_address(&config.trustee_address)
                .expect("invalid trustee address");
            let contract_addr = deadmkt_settlement::parse_address(&config.contracts.pool_config)
                .expect("invalid pool_config address");

            let chain_id = match config.network {
                deadmkt_config::Network::Testnet => 6u8,
                deadmkt_config::Network::Mainnet => 1u8,
            };

            let client = deadmkt_chain::client::SupraClient::new(
                config.rpc_urls.clone(),
                config.contracts.pool_config.clone(),
            );

            // Query current batch_id
            let batch_json = client.view_raw(
                "pool_config", "get_current_batch_id", vec![], vec![],
            ).await.expect("failed to get current batch_id");

            let current_batch: u64 = batch_json.get(0)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .or_else(|| batch_json.get(0).and_then(|v| v.as_u64()))
                .expect("failed to parse batch_id");

            let effective_at_batch = current_batch + 2;
            println!("  Current batch: {}", current_batch);
            println!("  Effective at:  {} (current + 2)", effective_at_batch);

            // Build args: all u64 values
            let args = vec![
                bcs::to_bytes(&blocks_per_batch).unwrap(),
                bcs::to_bytes(&commit_blocks).unwrap(),
                bcs::to_bytes(&reveal_blocks).unwrap(),
                bcs::to_bytes(&match_blocks).unwrap(),
                bcs::to_bytes(&swap_blocks).unwrap(),
                bcs::to_bytes(&commits_per_batch).unwrap(),
                bcs::to_bytes(&settlement_max_age).unwrap(),
                bcs::to_bytes(&rushed_grace_batches).unwrap(),
                bcs::to_bytes(&effective_at_batch).unwrap(),
            ];

            let sender_hex = format!("0x{}", hex::encode(sender_addr));
            let account = client.get_account(&sender_hex).await
                .expect("failed to get account info");

            let expiry = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() + 60;

            let signed_tx = deadmkt_settlement::build_and_sign_entry_function(
                &signing_key,
                sender_addr,
                contract_addr,
                "pool_config",
                "queue_param_update",
                args,
                account.sequence_number,
                expiry,
                chain_id,
                config.max_gas_amount,
                config.gas_unit_price,
            ).expect("failed to build transaction");

            println!("  Submitting queue_param_update...");

            match client.submit_raw(&signed_tx).await {
                Ok(hash) => {
                    println!("  Transaction submitted: {}", hash);
                    println!("  Param update queued. Takes effect at batch {}.", effective_at_batch);
                }
                Err(e) => {
                    eprintln!("  Transaction failed: {}", e);
                    eprintln!("  (Is this node's trustee the pool_config admin?)");
                    std::process::exit(1);
                }
            }
        }
        Command::Run => {
            println!("deadmkt-node v{}\n", env!("CARGO_PKG_VERSION"));

            let dir = data_dir();
            let config_path = dir.join("config.json");
            let keystore_path = dir.join("keystore.json");

            if is_first_boot(&config_path, &keystore_path) {
                println!("First boot detected. Run: deadmkt-node setup");
                return;
            }

            let mode = detect_keystore_mode(&keystore_path).unwrap_or(KeystoreMode::Missing);

            if let Err(e) = run::run(&dir, mode).await {
                eprintln!("\nFATAL: {}", e);
                std::process::exit(1);
            }
        }
    }
}

// =========================================================================
// MR1a: --config non-interactive setup entry point
//
// Decision matrix (per non-interactive-setup.md startup flow):
//   keystore.json exists?    has --config?    action
//   ----------------------   --------------   ----------------------------
//   no                       yes              MR1a: fresh setup (this fn)
//   yes                      yes              MR1b (LLM-assisted)  -- TODO
//   yes                      no               MR1c (restore)       -- TODO
//   no                       no               existing interactive wizard
//
// For MR1a, only the (no, yes) cell is wired. Other (yes, *) cells emit
// a clear error JSON pointing operators at the wizard.
// =========================================================================
async fn run_noninteractive_setup_and_exit(config_path: &std::path::Path) -> ! {
    use deadmkt_setup::noninteractive::{
        enforce_setup_config_permissions, load_setup_config,
        run_setup_noninteractive_fresh, run_setup_noninteractive_llm_assisted,
        scrub_password_from_setup_file, SetupResult, SetupMode,
    };

    let dir = data_dir();
    let keystore_path = dir.join("keystore.json");

    // Load config from disk first -- if this fails the decision matrix
    // doesn't matter and we get a clean step=load_config error.
    let cfg = match load_setup_config(config_path) {
        Ok(c) => c,
        Err(e) => {
            // We don't know yet which mode the operator intended; default to
            // Fresh for the failure record (this is a config-load failure).
            let mode_for_failure = if keystore_path.exists() {
                SetupMode::ConfigWithKeystore
            } else {
                SetupMode::Fresh
            };
            let r = SetupResult {
                success: false, mode: mode_for_failure,
                nft_id: None, trustee_address: None, beneficiary_address: None,
                network: String::new(), node_role: String::new(),
                escrow_balances: None, gas_balance_supra: None,
                steps_performed: Vec::new(), steps_skipped: Vec::new(),
                warnings: Vec::new(),
                error: Some(format!("{}", e)),
                step: Some("load_config".into()),
            };
            println!("{}", r.to_json());
            std::process::exit(1);
        }
    };

    // MR1d: refuse to proceed if setup.json carries a keystore_password
    // but is world/group-readable. Mirrors SSH's "permissions too open"
    // stance: a secret-bearing file must be 0600.
    if cfg.keystore_password.is_some() {
        if let Err(e) = enforce_setup_config_permissions(config_path) {
            let mode_for_failure = if keystore_path.exists() {
                SetupMode::ConfigWithKeystore
            } else {
                SetupMode::Fresh
            };
            let r = SetupResult {
                success: false, mode: mode_for_failure,
                nft_id: None, trustee_address: None, beneficiary_address: None,
                network: String::new(), node_role: String::new(),
                escrow_balances: None, gas_balance_supra: None,
                steps_performed: Vec::new(), steps_skipped: Vec::new(),
                warnings: Vec::new(),
                error: Some(format!("{}", e)),
                step: Some("enforce_config_permissions".into()),
            };
            println!("{}", r.to_json());
            std::process::exit(1);
        }
    }

    // Build chain client (mirrors the interactive Setup branch above).
    let defaults = default_contract_addresses(&Network::Testnet);
    let contract_addr = std::env::var("DEADMKT_CONTRACT_ADDR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or(defaults.settlement);
    let chain = setup_bridge::SupraSetupClient::new(
        vec!["https://rpc-testnet.supra.com".into()],
        contract_addr,
    );

    // Branch on (keystore exists, config has password) per the spec
    // startup-flow decision matrix.
    let result = if keystore_path.exists() {
        // MR1b path: keystore on disk -> prompt for password interactively
        // and unlock. The LLM that wrote setup.json never sees the password.
        //
        // Early-reject if the config tries to ship a password while the
        // keystore exists -- that crosses the LLM security boundary and
        // would also confuse the operator who's about to be prompted.
        if cfg.keystore_password.is_some() {
            let r = SetupResult {
                success: false, mode: SetupMode::ConfigWithKeystore,
                nft_id: None, trustee_address: None, beneficiary_address: None,
                network: String::new(), node_role: String::new(),
                escrow_balances: None, gas_balance_supra: None,
                steps_performed: Vec::new(), steps_skipped: Vec::new(),
                warnings: Vec::new(),
                error: Some("setup.json contains keystore_password but a keystore already exists at the data dir. MR1b (LLM-assisted) refuses password-in-config for the existing-keystore path -- the operator types the password interactively. Remove keystore_password from setup.json.".into()),
                step: Some("validate_for_llm_assisted".into()),
            };
            println!("{}", r.to_json());
            std::process::exit(1);
        }
        let password = match prompt_keystore_password() {
            Ok(p) => p,
            Err(e) => {
                let mut r = SetupResult {
                    success: false, mode: SetupMode::ConfigWithKeystore,
                    nft_id: None, trustee_address: None, beneficiary_address: None,
                    network: String::new(), node_role: String::new(),
                    escrow_balances: None, gas_balance_supra: None,
                    steps_performed: Vec::new(), steps_skipped: Vec::new(),
                    warnings: Vec::new(),
                    error: None, step: None,
                };
                r = r.fail("prompt_password", e);
                println!("{}", r.to_json());
                std::process::exit(1);
            }
        };
        run_setup_noninteractive_llm_assisted(&cfg, &password, &chain, &dir).await
    } else {
        // MR1a path: no keystore -> generate from password supplied in config.
        run_setup_noninteractive_fresh(&cfg, &chain, &dir).await
    };

    // MR1d: if setup succeeded AND the config carried a keystore_password,
    // rewrite the file with the password field stripped. The keystore is
    // already on disk encrypted, so the plaintext copy in setup.json is
    // now a liability. A scrub failure does not flip the result to
    // failure -- we surface it as a warning so the operator can clean
    // up manually.
    let mut result = result;
    if result.success && cfg.keystore_password.is_some() {
        match scrub_password_from_setup_file(config_path) {
            Ok(()) => result.steps_performed.push("scrub_password_from_config".into()),
            Err(e) => result.warnings.push(format!(
                "failed to scrub keystore_password from {}: {} -- delete the file manually or chmod 600 it",
                config_path.display(), e
            )),
        }
    }

    println!("{}", result.to_json());
    std::process::exit(if result.success { 0 } else { 1 });
}

// =========================================================================
// MR2: `deadmkt-node status --json`
//
// Talks to the running node's SP8 health endpoint on 127.0.0.1:9292.
// On any connection / read / parse failure, falls back to a disk-only
// payload built from config.json so the consumer (LLM operator,
// dashboard, etc.) always gets a v1-shaped JSON response. Exit code is
// always 0 from this path -- consumers branch on `node_running`.
// =========================================================================
async fn run_status_json_and_exit(config_path: &std::path::Path) -> ! {
    // Try the live endpoint first.
    match fetch_status_v1_from_endpoint().await {
        Ok(body) => {
            println!("{}", body);
            std::process::exit(0);
        }
        Err(connect_err) => {
            // Fall back to disk-only: read config.json (if present) to
            // populate identity, and emit the v1 disk-only shape. If
            // config.json is also missing, emit a minimal shell so the
            // consumer still gets a parseable response.
            let body = match NodeConfig::load(config_path) {
                Ok(cfg) => crate::run::build_status_v1_disk_only_json(
                    &format!("{:?}", cfg.network).to_lowercase(),
                    // Best-effort role inference: the explicit env flag is
                    // the only signal a non-running binary has.
                    if std::env::var("DEADMKT_NO_STRATEGY").map(|v| v == "1").unwrap_or(false) {
                        "bootstrap"
                    } else {
                        "trading"
                    },
                    cfg.nft_id,
                    &cfg.trustee_address,
                    &cfg.beneficiary_address,
                    &format!("could not connect to running node on 127.0.0.1:9292: {}", connect_err),
                ),
                Err(_) => crate::run::build_status_v1_disk_only_json(
                    "unknown", "unknown", 0, "", "",
                    "node not configured and not running",
                ),
            };
            println!("{}", body);
            std::process::exit(0);
        }
    }
}

/// MR2: hand-written minimal HTTP/1.0 GET to the SP8 endpoint.
/// Avoids pulling in `reqwest` / `hyper` for a 15-line client.
async fn fetch_status_v1_from_endpoint() -> Result<String, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let timeout = std::time::Duration::from_secs(1);
    let stream = tokio::time::timeout(
        timeout,
        tokio::net::TcpStream::connect("127.0.0.1:9292"),
    )
    .await
    .map_err(|_| "connect timeout".to_string())?
    .map_err(|e| format!("connect: {}", e))?;

    let mut stream = stream;
    let req = b"GET /status HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    tokio::time::timeout(timeout, stream.write_all(req))
        .await
        .map_err(|_| "write timeout".to_string())?
        .map_err(|e| format!("write: {}", e))?;

    let mut buf = Vec::with_capacity(8192);
    tokio::time::timeout(timeout, stream.read_to_end(&mut buf))
        .await
        .map_err(|_| "read timeout".to_string())?
        .map_err(|e| format!("read: {}", e))?;

    // Split on the HTTP header/body boundary.
    let pos = buf.windows(4).position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "no body delimiter".to_string())?;
    let body = String::from_utf8_lossy(&buf[pos + 4..]).into_owned();

    // Sanity check: ensure the response is JSON-shaped before handing back.
    let trimmed = body.trim();
    if !trimmed.starts_with('{') {
        return Err(format!("non-JSON body: {}", trimmed.chars().take(80).collect::<String>()));
    }
    Ok(trimmed.to_string())
}

/// MR1b: prompt the operator for the keystore password on stderr,
/// read one line from stdin. Echoes characters as the operator types
/// (we don't pull in `rpassword` for this slice -- the existing
/// interactive wizard's wizard_generate_keypair also echoes). Future
/// MR1d / hardening can swap to a no-echo input.
fn prompt_keystore_password() -> Result<String, String> {
    use std::io::{BufRead, Write};
    eprint!("Keystore password: ");
    std::io::stderr().flush().map_err(|e| format!("flush stderr: {}", e))?;
    let stdin = std::io::stdin();
    let mut line = String::new();
    let n = stdin.lock().read_line(&mut line)
        .map_err(|e| format!("read stdin: {}", e))?;
    if n == 0 {
        return Err("stdin closed without password input (need a TTY or piped password)".into());
    }
    let trimmed = line.trim_end_matches(|c| c == '\n' || c == '\r').to_string();
    if trimmed.is_empty() {
        return Err("empty password".into());
    }
    Ok(trimmed)
}

// =========================================================================
// MR1c: restore-from-backup
//
// Operator drops their backed-up keystore.json into the data dir and runs
// `deadmkt-node` (no flags). We detect the (keystore present, config
// missing) state, prompt for the password, unlock, query chain for the
// trustee's NFT + beneficiary + escrow, and write a fresh config.json.
// Operator runs `deadmkt-node` again to start trading -- that second
// invocation skips this branch because config.json now exists.
// =========================================================================
async fn run_restore_setup_and_exit(data_dir: &std::path::Path) -> ! {
    use deadmkt_setup::noninteractive::{
        run_setup_noninteractive_restore, SetupResult, SetupMode,
    };

    // Prompt for password before we touch chain state.
    let password = match prompt_keystore_password() {
        Ok(p) => p,
        Err(e) => {
            let mut r = SetupResult {
                success: false, mode: SetupMode::Restore,
                nft_id: None, trustee_address: None, beneficiary_address: None,
                network: String::new(), node_role: String::new(),
                escrow_balances: None, gas_balance_supra: None,
                steps_performed: Vec::new(), steps_skipped: Vec::new(),
                warnings: Vec::new(),
                error: None, step: None,
            };
            r = r.fail("prompt_password", e);
            println!("{}", r.to_json());
            std::process::exit(1);
        }
    };

    // Network: default to testnet, allow DEADMKT_NETWORK env override.
    let network = match std::env::var("DEADMKT_NETWORK").unwrap_or_default().as_str() {
        "mainnet" => Network::Mainnet,
        _ => Network::Testnet,
    };

    // Build chain client mirroring the interactive Setup branch.
    let defaults = default_contract_addresses(&network);
    let contract_addr = std::env::var("DEADMKT_CONTRACT_ADDR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or(defaults.settlement);
    let rpc_url = match network {
        Network::Testnet => "https://rpc-testnet.supra.com",
        Network::Mainnet => "https://rpc-mainnet.supra.com",
    };
    let chain = setup_bridge::SupraSetupClient::new(
        vec![rpc_url.into()],
        contract_addr,
    );

    let result = run_setup_noninteractive_restore(&password, network, &chain, data_dir).await;
    println!("{}", result.to_json());
    std::process::exit(if result.success { 0 } else { 1 });
}
