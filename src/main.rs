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
    let command = cli.resolve_command();

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
                    eprintln!("You can re-run 'deadmkt-node setup' to try again.");
                }
            }
        }
        Command::Status => {
            let dir = data_dir();
            let config_path = dir.join("config.json");
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
