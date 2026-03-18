// =========================================================================
// run.rs: Node run loop — wires all subsystems together
// =========================================================================
//
// Startup:  config → keystore → chain client → gossip → batch_state →
//           strategy WS → orchestrator → poll loop
//
// Poll loop: get_ledger_info → compute phase → drive orchestrator →
//            route events → settle → profit check

use deadmkt_chain::batch::{compute_batch_id, compute_phase, compute_pool_assignment, Phase};
use deadmkt_chain::client::SupraClient;
use deadmkt_chain::types::{BatchEpoch, BatchParams};
use deadmkt_config::NodeConfig;
use deadmkt_crypto::{commit_hash, encode_order, sign_order, Order};
use deadmkt_escrow_tracker::EscrowTracker;
use deadmkt_gas_manager::GasManager;
use deadmkt_gossip::messages::{self as gossip_messages, GossipMessage};
use deadmkt_gossip::network::GossipNode;
use deadmkt_keystore::{load_keystore, load_keystore_from_env, KeystoreMode};
use deadmkt_matching::MarketConfig;
use deadmkt_node_state::{NodeEvent, NodeState, NodeStateMachine};
use deadmkt_orchestrator::{
    BatchStatePort, CommitMessage, GossipPort, Orchestrator, Phase as OrcPhase, RevealMessage,
    ValidatedOrder,
};
use deadmkt_settlement::{SettlementManager, SettlementSubmitter};
use deadmkt_settlement::worker::{SettleRequest, SettleResult, spawn_worker};
use deadmkt_storage::Storage;
use deadmkt_strategy::convert::{validate_orders, OrderValidationContext};
use deadmkt_strategy::server::StrategyServer;
use deadmkt_strategy::{StrategyAction, StrategyEvent};
use deadmkt_setup::ChainClient;
use ed25519_dalek::{SigningKey, VerifyingKey};
use libp2p::futures::StreamExt;
use libp2p::identity::Keypair;
use libp2p::swarm::SwarmEvent;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::signal;
use tokio::time::Duration;



// =========================================================================
// Bridge: GossipNode → GossipPort
// =========================================================================

/// Wraps the real GossipNode to satisfy the orchestrator's GossipPort trait.
/// Gossip is async but the trait is sync — we buffer outbound messages and
/// flush them in the async poll loop.
pub struct GossipBridge {
    /// Outbound commit queue (drained by poll loop).
    pub pending_commits: std::sync::Mutex<Vec<(u64, GossipMessage)>>,
    /// Outbound reveal queue (drained by poll loop).
    pub pending_reveals: std::sync::Mutex<Vec<(u64, GossipMessage)>>,
    /// Inbound commits received from peers, keyed by batch_id.
    pub inbound_commits: std::collections::HashMap<u64, Vec<CommitMessage>>,
    /// Inbound reveals received from peers, keyed by batch_id.
    pub inbound_reveals: std::collections::HashMap<u64, Vec<RevealMessage>>,
}

impl GossipBridge {
    pub fn new() -> Self {
        Self {
            pending_commits: std::sync::Mutex::new(Vec::new()),
            pending_reveals: std::sync::Mutex::new(Vec::new()),
            inbound_commits: std::collections::HashMap::new(),
            inbound_reveals: std::collections::HashMap::new(),
        }
    }
}

impl GossipPort for GossipBridge {
    fn publish_commit(&self, pool_id: u64, commit: CommitMessage) -> Result<(), String> {
        let msg = GossipMessage::Commitment {
            batch_id: commit.batch_id,
            pool_id: commit.pool_id,
            hash: {
                let mut arr = [0u8; 32];
                let len = commit.commit_hash.len().min(32);
                arr[..len].copy_from_slice(&commit.commit_hash[..len]);
                arr
            },
            nft_id: commit.nft_id,
            signature: vec![0u8; 64],
        };
        self.pending_commits.lock().unwrap().push((pool_id, msg));
        Ok(())
    }

    fn publish_reveal(&self, pool_id: u64, reveal: RevealMessage) -> Result<(), String> {
        // Try to deserialize real order bytes from BCS; fall back to placeholder
        let order = if let Ok(decoded) = bcs::from_bytes::<deadmkt_crypto::Order>(&reveal.order_bytes) {
            decoded
        } else {
            deadmkt_crypto::Order {
                nft_id: reveal.nft_id,
                symbol: b"UNKNOWN".to_vec(),
                side: 0,
                price: 0,
                quantity: 0,
                batch_id: reveal.batch_id,
                nonce: reveal.order_bytes.clone(),
            }
        };
        let msg = GossipMessage::Reveal {
            batch_id: reveal.batch_id,
            pool_id: reveal.pool_id,
            order,
            signature: reveal.signature.clone(),
        };
        self.pending_reveals.lock().unwrap().push((pool_id, msg));
        Ok(())
    }

    fn received_commits(&self, batch_id: u64) -> Vec<CommitMessage> {
        self.inbound_commits.get(&batch_id).cloned().unwrap_or_default()
    }

    fn received_reveals(&self, batch_id: u64) -> Vec<RevealMessage> {
        self.inbound_reveals.get(&batch_id).cloned().unwrap_or_default()
    }
}

// =========================================================================
// Bridge: Chain batch computation → BatchStatePort
// =========================================================================

// =========================================================================
// SEC-1/SEC-2: Gossip inbound validation (B5.4)
// =========================================================================
//
// Validates inbound gossip messages before storing:
//   - Batch bounds:  reject messages outside current_batch ± BATCH_WINDOW
//   - Deduplication: reject duplicate (nft_id, batch_id) pairs
//   - Signature:     verify Ed25519 on reveals against cached on-chain pubkeys
//   - Unknown NFTs:  reject reveals from unrecognized NFTs, queue pubkey fetch

/// Maximum batch ID distance from current to accept inbound gossip messages.
const BATCH_WINDOW: u64 = 5;

/// How often (in batches) to attempt fetching pubkeys for unknown NFTs.
const PUBKEY_FETCH_INTERVAL: u64 = 3;

/// Maximum commits+reveals accepted per (nft_id, batch_id) pair.
/// Generous limit: commits_per_batch is typically 3, so 6 messages
/// (3 commits + 3 reveals) is normal. We allow 10 for tolerance.
const MAX_MSGS_PER_NFT_PER_BATCH: u32 = 10;

struct GossipValidator {
    /// Cached NFT ID \u{2192} Ed25519 public key (fetched from on-chain nft::get_trustee_pubkey)
    nft_pubkeys: HashMap<u64, VerifyingKey>,
    /// Content-hash dedup: stores commit_hash values we've already accepted
    seen_commit_hashes: HashSet<[u8; 32]>,
    /// Content-hash dedup: stores SHA256(order_bytes) for reveals we've already accepted
    seen_reveal_hashes: HashSet<[u8; 32]>,
    /// Rate limit: message count per (nft_id, batch_id) \u{2014} prevents flooding
    msg_counts: HashMap<(u64, u64), u32>,
    /// NFT IDs we've seen in gossip but don't have pubkeys for yet
    unknown_nfts: HashSet<u64>,
    /// Current batch ID (updated each chain tick for bounds checking)
    current_batch_id: u64,
    /// Last batch we attempted pubkey fetches
    last_pubkey_fetch_batch: u64,
    // \u{2500}\u{2500} Rejection counters (for diagnostics) \u{2500}\u{2500}
    rejected_bounds: u64,
    rejected_dedup: u64,
    rejected_rate: u64,
    rejected_sig: u64,
    rejected_unknown: u64,
    accepted: u64,
}

impl GossipValidator {
    fn new(initial_batch_id: u64) -> Self {
        Self {
            nft_pubkeys: HashMap::new(),
            seen_commit_hashes: HashSet::new(),
            seen_reveal_hashes: HashSet::new(),
            msg_counts: HashMap::new(),
            unknown_nfts: HashSet::new(),
            current_batch_id: initial_batch_id,
            last_pubkey_fetch_batch: 0,
            rejected_bounds: 0,
            rejected_dedup: 0,
            rejected_rate: 0,
            rejected_sig: 0,
            rejected_unknown: 0,
            accepted: 0,
        }
    }

    /// Check if a batch_id falls within the acceptable window.
    fn is_batch_in_range(&self, batch_id: u64) -> bool {
        if self.current_batch_id == 0 {
            return true;
        }
        let lower = self.current_batch_id.saturating_sub(BATCH_WINDOW);
        let upper = self.current_batch_id.saturating_add(BATCH_WINDOW);
        batch_id >= lower && batch_id <= upper
    }

    /// Check per-(nft_id, batch_id) rate limit. Returns true if under limit.
    fn check_rate_limit(&mut self, nft_id: u64, batch_id: u64) -> bool {
        let count = self.msg_counts.entry((nft_id, batch_id)).or_insert(0);
        if *count >= MAX_MSGS_PER_NFT_PER_BATCH {
            self.rejected_rate += 1;
            return false;
        }
        *count += 1;
        true
    }

    /// Validate an inbound commit. Returns true if accepted.
    fn accept_commit(&mut self, nft_id: u64, batch_id: u64, commit_hash: &[u8]) -> bool {
        // 1. Batch bounds
        if !self.is_batch_in_range(batch_id) {
            self.rejected_bounds += 1;
            return false;
        }
        // 2. Content-hash dedup (commit_hash is already a unique 32-byte hash per order)
        if commit_hash.len() == 32 {
            let mut hash_arr = [0u8; 32];
            hash_arr.copy_from_slice(commit_hash);
            if !self.seen_commit_hashes.insert(hash_arr) {
                self.rejected_dedup += 1;
                return false;
            }
        }
        // 3. Rate limit
        if !self.check_rate_limit(nft_id, batch_id) {
            return false;
        }
        self.accepted += 1;
        true
    }

    /// Validate an inbound reveal: bounds + dedup + rate limit + Ed25519 signature.
    /// Returns true if the reveal should be stored.
    fn accept_reveal(
        &mut self,
        nft_id: u64,
        batch_id: u64,
        order_bytes: &[u8],
        signature: &[u8],
    ) -> bool {
        // 1. Batch bounds
        if !self.is_batch_in_range(batch_id) {
            self.rejected_bounds += 1;
            return false;
        }
        // 2. Content-hash dedup (hash the order_bytes for uniqueness)
        let content_hash = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(order_bytes);
            let result = hasher.finalize();
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&result);
            arr
        };
        if !self.seen_reveal_hashes.insert(content_hash) {
            self.rejected_dedup += 1;
            return false;
        }
        // 3. Rate limit
        if !self.check_rate_limit(nft_id, batch_id) {
            self.seen_reveal_hashes.remove(&content_hash);
            return false;
        }
        // 4. Signature verification (requires cached pubkey)
        if let Some(pubkey) = self.nft_pubkeys.get(&nft_id) {
            if !deadmkt_crypto::verify(pubkey, order_bytes, signature) {
                self.rejected_sig += 1;
                self.seen_reveal_hashes.remove(&content_hash);
                let count = self.msg_counts.get_mut(&(nft_id, batch_id));
                if let Some(c) = count { *c = c.saturating_sub(1); }
                eprintln!("[gossip] \u{2717} INVALID signature from nft={} batch={}", nft_id, batch_id);
                return false;
            }
            self.accepted += 1;
            true
        } else {
            // Unknown NFT \u{2014} queue for async pubkey fetch, reject for now
            self.unknown_nfts.insert(nft_id);
            self.rejected_unknown += 1;
            self.seen_reveal_hashes.remove(&content_hash);
            let count = self.msg_counts.get_mut(&(nft_id, batch_id));
            if let Some(c) = count { *c = c.saturating_sub(1); }
            false
        }
    }

    /// Update current batch ID (called from chain tick).
    fn update_batch(&mut self, batch_id: u64) {
        if batch_id > self.current_batch_id {
            self.current_batch_id = batch_id;
        }
    }

    /// Expire dedup entries and rate limits for batches outside the window.
    fn expire_old_batches(&mut self) {
        let lower = self.current_batch_id.saturating_sub(BATCH_WINDOW);
        self.msg_counts.retain(|&(_, batch), _| batch >= lower);
        // Periodic full clear of content hashes to bound memory.
        // Safe because old batches are already outside the window.
        if self.current_batch_id % BATCH_WINDOW == 0 {
            self.seen_commit_hashes.clear();
            self.seen_reveal_hashes.clear();
        }
    }

    /// Cache a pubkey for an NFT ID.
    fn cache_pubkey(&mut self, nft_id: u64, key: VerifyingKey) {
        self.nft_pubkeys.insert(nft_id, key);
        self.unknown_nfts.remove(&nft_id);
    }

    /// Take the set of unknown NFT IDs that need pubkey fetching.
    fn take_unknown_nfts(&mut self) -> Vec<u64> {
        self.unknown_nfts.drain().collect()
    }

    /// Whether it's time to attempt pubkey fetches.
    fn should_fetch_pubkeys(&self, current_batch: u64) -> bool {
        !self.unknown_nfts.is_empty()
            && current_batch >= self.last_pubkey_fetch_batch + PUBKEY_FETCH_INTERVAL
    }

    /// Log rejection stats if any rejections occurred, then reset counters.
    fn log_and_reset_stats(&mut self) {
        let total_rejected = self.rejected_bounds + self.rejected_dedup
            + self.rejected_rate + self.rejected_sig + self.rejected_unknown;
        if total_rejected > 0 || self.accepted > 0 {
            eprintln!(
                "[gossip-validator] accepted={} rejected: bounds={} dedup={} rate={} sig={} unknown_nft={} | cached_keys={}",
                self.accepted, self.rejected_bounds, self.rejected_dedup,
                self.rejected_rate, self.rejected_sig, self.rejected_unknown,
                self.nft_pubkeys.len(),
            );
        }
        self.rejected_bounds = 0;
        self.rejected_dedup = 0;
        self.rejected_rate = 0;
        self.rejected_sig = 0;
        self.rejected_unknown = 0;
        self.accepted = 0;
    }
}

// =========================================================================
// GOSSIP-1: Swarm command channel types (B5.4)
// =========================================================================

/// Commands sent from the main loop to the dedicated gossip task.
enum SwarmCommand {
    /// Publish a gossip message to a pool topic.
    Publish { pool_id: u64, msg: GossipMessage },
    /// Subscribe to a pool topic.
    SubscribePool(u64),
    /// Unsubscribe from a pool topic.
    UnsubscribePool(u64),
}

/// Wraps chain batch computation to satisfy the orchestrator's BatchStatePort.
pub struct BatchStateBridge {
    pub block_height: u64,
    pub epoch: BatchEpoch,
    pub params: BatchParams,
    pub num_pools: u64,
    pub nft_id: u64,
}

impl BatchStateBridge {
    pub fn new(epoch: BatchEpoch, params: BatchParams, num_pools: u64, nft_id: u64) -> Self {
        Self {
            block_height: epoch.anchor_block,
            epoch,
            params,
            num_pools,
            nft_id,
        }
    }
}

impl BatchStatePort for BatchStateBridge {
    fn current_phase(&self) -> OrcPhase {
        match compute_phase(self.block_height, &self.epoch, &self.params) {
            Phase::Commit => OrcPhase::Commit,
            Phase::Reveal => OrcPhase::Reveal,
            Phase::Match => OrcPhase::Match,
            Phase::Swap => OrcPhase::Swap,
        }
    }

    fn current_batch_id(&self) -> u64 {
        compute_batch_id(self.block_height, &self.epoch, &self.params)
    }

    fn current_pool_id(&self, nft_id: u64) -> u64 {
        let batch_id = self.current_batch_id();
        compute_pool_assignment(nft_id, batch_id, self.num_pools.max(1))
    }

    fn advance_to_block(&mut self, block_height: u64) {
        self.block_height = block_height;
    }

    fn num_pools(&self) -> u64 {
        self.num_pools
    }

    fn set_num_pools(&mut self, n: u64) {
        self.num_pools = n;
    }

    fn reload_params(&mut self, params: BatchParams) {
        self.params = params;
    }
}

// =========================================================================
// Run
// =========================================================================

pub async fn run(data_dir: &Path, keystore_mode: KeystoreMode) -> Result<(), Box<dyn std::error::Error>> {
    // ── 1. Load config ───────────────────────────────────────────────
    let config_path = data_dir.join("config.json");
    let config = NodeConfig::load(&config_path)?;
    println!("  Network:  {:?}", config.network);
    println!("  NFT ID:   {}", config.nft_id);
    println!("  Markets:  {:?}", config.markets);

    // ── 2. Decrypt keystore ──────────────────────────────────────────
    let keystore_path = data_dir.join("keystore.json");
    let (signing_key, _verifying_key) = match keystore_mode {
        KeystoreMode::Encrypted => {
            // Try env var first (Docker), fall back to interactive prompt
            match load_keystore_from_env(&keystore_path) {
                Ok(keys) => {
                    println!("  Keystore: unlocked (env)");
                    keys
                }
                Err(_) => {
                    print!("  Keystore password: ");
                    io::stdout().flush()?;
                    let password = read_password()?;
                    let keys = load_keystore(&keystore_path, &password)?;
                    println!("  Keystore: unlocked");
                    keys
                }
            }
        }
        KeystoreMode::Insecure => {
            println!("  WARNING: Insecure keystore (unencrypted)");
            let keys = deadmkt_keystore::load_keystore_insecure(&keystore_path)?;
            keys
        }
        KeystoreMode::Missing => {
            return Err("Keystore not found. Run 'deadmkt-node setup' first.".into());
        }
    };

    let trustee_addr = config.trustee_address.clone();
    println!("  Trustee:  {}...{}", &trustee_addr[..8], &trustee_addr[trustee_addr.len()-6..]);

    // ── 3. Chain client ──────────────────────────────────────────────
    let chain = SupraClient::new(
        config.rpc_urls.clone(),
        config.contracts.settlement.clone(),
    );
    // Separate client for pool_config view functions (uses different contract addr)
    let pool_config_client = SupraClient::new(
        config.rpc_urls.clone(),
        config.contracts.pool_config.clone(),
    );
    // Separate client for escrow entry functions (heartbeat)
    let escrow_client = SupraClient::new(
        config.rpc_urls.clone(),
        config.contracts.escrow.clone(),
    );

    // Fetch initial chain state
    print!("  Chain:    connecting...");
    io::stdout().flush()?;
    let ledger = chain.get_ledger_info().await?;
    println!(" block {}", ledger.block_height);

    // Fetch batch parameters from chain
    let mut params = fetch_batch_params(&pool_config_client, &config.contracts.pool_config).await?;
    println!("  Batch:    {} blocks/batch, {} commits/batch",
             params.blocks_per_batch, params.commits_per_batch);

    // Fetch epoch (anchor block/batch)
    let mut epoch = fetch_batch_epoch(&pool_config_client, &config.contracts.pool_config).await?;
    let batch_id = compute_batch_id(ledger.block_height, &epoch, &params);
    let phase = compute_phase(ledger.block_height, &epoch, &params);
    println!("  Current:  batch={}, phase={}", batch_id, phase);

    // Check on-chain protocol version matches node
    match fetch_protocol_version(&pool_config_client, &config.contracts.pool_config).await {
        Ok(chain_version) => {
            let node_version = deadmkt_gossip::PROTOCOL_VERSION as u64;
            if chain_version != node_version {
                return Err(format!(
                    "Protocol version mismatch: chain=v{}, node=v{}. Update your node binary.",
                    chain_version, node_version
                ).into());
            }
        }
        Err(e) => {
            eprintln!("  WARNING: Could not fetch protocol version from chain: {} (continuing)", e);
        }
    }

    // ── 4. Storage + Escrow tracker ──────────────────────────────────
    let _storage = Storage::open(data_dir)?;
    let _tracker = Arc::new(Mutex::new(EscrowTracker::new()));

    // Fetch token decimals from on-chain FA metadata
    let token_decimals = fetch_token_decimals(&chain, &config.markets).await;
    if token_decimals.is_empty() {
        eprintln!("  WARNING: Could not fetch token decimals from chain, using defaults");
    }

    let escrow_balances = fetch_escrow_balances(&chain, config.nft_id, &config.markets, &token_decimals).await;
    if !escrow_balances.is_empty() {
        println!("  Escrow:   {:?}", escrow_balances);
        // Seed escrow tracker with confirmed balances
        let mut tracker = _tracker.lock().unwrap();
        for (token, human_str) in &escrow_balances {
            let dec = token_decimals.get(token.as_str()).copied().unwrap_or(5);
            if let Ok(human_val) = human_str.parse::<f64>() {
                let raw = (human_val * 10f64.powi(dec as i32)) as u64;
                tracker.set_confirmed(token, raw);
                eprintln!("  [escrow] Tracker seeded: {} = {} raw", token, raw);
            }
        }
        drop(tracker);
    } else {
        println!("  Escrow:   no balances found (strategy will skip orders)");
    }

    // Fetch Trippples token state
    let wallet_balances = fetch_wallet_balances(&chain, &config.trustee_address, &config.contracts.settlement, &token_decimals).await;
    if !wallet_balances.is_empty() {
        println!("  Wallet:   {:?}", wallet_balances);
    }
    let mint_state = fetch_mint_state(&chain, &config.trustee_address).await;
    println!("  MintState: {} (hold={}s, pending={})", mint_state.state, mint_state.hold_duration_secs, mint_state.has_pending_mint);
    let circulating = fetch_circulating_supply(&chain, &token_decimals).await;
    if !circulating.is_empty() {
        println!("  Circulating: {:?}", circulating);
    }
    let vault_locks = fetch_vault_locks(&chain, &config.trustee_address, &token_decimals).await;
    if !vault_locks.is_empty() {
        println!("  Vault locks: {} active", vault_locks.iter().filter(|l| !l.claimed).count());
    }

    println!("  Storage:  opened at {}", data_dir.display());

    // ── 5. Gossip network ────────────────────────────────────────────
    let gossip_keypair = ed25519_to_libp2p(&signing_key);
    let mut gossip_node = GossipNode::new(gossip_keypair)?;
    gossip_node.listen_on_port(config.gossip_port)?;

    // Subscribe to our initial pool
    let mut num_pools = fetch_num_pools(&pool_config_client, &config.contracts.pool_config).await
        .unwrap_or(1);
    let mut my_pool = compute_pool_assignment(config.nft_id, batch_id, num_pools.max(1));
    gossip_node.subscribe_pool(my_pool)?;
    println!("  Gossip:   listening, peer_id={}", gossip_node.local_peer_id());
    println!("            pool={}, num_pools={}, protocol=v{}", my_pool, num_pools, deadmkt_gossip::PROTOCOL_VERSION);

    // Heartbeat config from chain
    let mut heartbeat_interval = match fetch_heartbeat_interval(
        &pool_config_client, &config.contracts.pool_config
    ).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  [heartbeat] failed to fetch interval: {}, using default 10", e);
            10
        }
    };
    if heartbeat_interval == 0 {
        eprintln!("  [heartbeat] WARNING: chain returned interval 0, using fallback 10");
        heartbeat_interval = 10;
    }
    let mut heartbeat_timeout = match fetch_heartbeat_timeout(
        &pool_config_client, &config.contracts.pool_config
    ).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  [heartbeat] failed to fetch timeout: {}, using default 50", e);
            50
        }
    };
    let mut consecutive_hb_failures: u64 = 0;
    let mut last_settlement_batch: u64 = 0;
    let mut last_sweep_batch: u64 = 0;
    let sweep_interval: u64 = 50; // check every 50 batches
    let mut gas_low: bool = false; // set on INSUFFICIENT_BALANCE, cleared on heartbeat success
    println!("  Heartbeat: every {} batches, timeout {} batches (offset by nft_id={})",
        heartbeat_interval, heartbeat_timeout, config.nft_id);
    println!("  Gas:       max_gas={}, gas_price={}", config.max_gas_amount, config.gas_unit_price);

    // Dial bootstrap peers
    for peer_addr in &config.bootstrap_peers {
        // Try multiaddr first (e.g. /ip4/1.2.3.4/tcp/9191)
        if peer_addr.starts_with('/') {
            match peer_addr.parse() {
                Ok(addr) => {
                    if let Err(e) = gossip_node.dial(addr) {
                        eprintln!("  WARNING: failed to dial {}: {}", peer_addr, e);
                    } else {
                        println!("  Dialing:  {}", peer_addr);
                    }
                }
                Err(e) => eprintln!("  WARNING: invalid multiaddr '{}': {}", peer_addr, e),
            }
        } else {
            // host:port format (e.g. peer1.testnet.deadmkt.com:9191)
            let parts: Vec<&str> = peer_addr.split(':').collect();
            if parts.len() == 2 {
                match resolve_and_dial(&mut gossip_node, parts[0], parts[1]) {
                    Ok(()) => println!("  Dialing:  {}", peer_addr),
                    Err(e) => eprintln!("  WARNING: failed to dial {}: {}", peer_addr, e),
                }
            } else {
                eprintln!("  WARNING: invalid peer address '{}' (expected host:port)", peer_addr);
            }
        }
    }

    // Extra dial peers from env (for Docker Compose peer discovery)
    // Format: comma-separated, e.g. "deadmkt-node2:9191" or "/ip4/172.18.0.3/tcp/9191"
    if let Ok(dial_peers) = std::env::var("DEADMKT_DIAL_PEERS") {
        for peer_spec in dial_peers.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            // If it's already a multiaddr, try parsing directly
            if peer_spec.starts_with('/') {
                // Resolve /dns4/hostname/tcp/port → /ip4/x.x.x.x/tcp/port
                if peer_spec.starts_with("/dns4/") || peer_spec.starts_with("/dns6/") {
                    // Extract hostname and port: /dns4/hostname/tcp/port
                    let parts: Vec<&str> = peer_spec.split('/').collect();
                    if parts.len() >= 5 {
                        let hostname = parts[2];
                        let port = parts[4];
                        match resolve_and_dial(&mut gossip_node, hostname, port) {
                            Ok(()) => println!("  Dialing:  {} (resolved from {})", hostname, peer_spec),
                            Err(e) => eprintln!("  WARNING: failed to dial {}: {}", peer_spec, e),
                        }
                    }
                } else {
                    match peer_spec.parse() {
                        Ok(addr) => {
                            if let Err(e) = gossip_node.dial(addr) {
                                eprintln!("  WARNING: failed to dial {}: {}", peer_spec, e);
                            } else {
                                println!("  Dialing:  {}", peer_spec);
                            }
                        }
                        Err(e) => eprintln!("  WARNING: invalid multiaddr '{}': {}", peer_spec, e),
                    }
                }
            } else {
                // Simple host:port format
                let parts: Vec<&str> = peer_spec.split(':').collect();
                if parts.len() == 2 {
                    match resolve_and_dial(&mut gossip_node, parts[0], parts[1]) {
                        Ok(()) => println!("  Dialing:  {}", peer_spec),
                        Err(e) => eprintln!("  WARNING: failed to dial {}: {}", peer_spec, e),
                    }
                }
            }
        }
    }

    // ── 5b. GOSSIP-1: Spawn dedicated swarm task ────────────────────
    //
    // The swarm is now driven in its own tokio task, independent of
    // chain polling. This eliminates the race condition where gossip
    // messages sit unprocessed in the libp2p buffer while the main
    // loop is doing chain tick work (async fetches, strategy timeout,
    // phase processing). The swarm task continuously deserializes
    // inbound messages and feeds them to the main loop via mpsc channel.
    //
    // Outbound messages (commit/reveal publishes, pool subscribe/
    // unsubscribe) are sent to the swarm task via a command channel.

    let (swarm_cmd_tx, mut swarm_cmd_rx) = tokio::sync::mpsc::channel::<SwarmCommand>(64);
    let (gossip_inbound_tx, mut gossip_inbound_rx) = tokio::sync::mpsc::channel::<GossipMessage>(8192);
    let gossip_peer_count = Arc::new(AtomicU64::new(0));
    let gossip_peer_count_writer = gossip_peer_count.clone();

    let _gossip_task = tokio::spawn(async move {
        let mut drop_count: u64 = 0;
        loop {
            tokio::select! {
                event = gossip_node.swarm_mut().select_next_some() => {
                    match event {
                        SwarmEvent::Behaviour(libp2p::gossipsub::Event::Message {
                            message, ..
                        }) => {
                            match gossip_messages::deserialize(&message.data) {
                                Ok(msg) => {
                                    // Non-blocking send — if main loop is behind, drop oldest
                                    if let Err(_) = gossip_inbound_tx.try_send(msg) {
                                        drop_count += 1;
                                        if drop_count == 1 || drop_count % 1000 == 0 {
                                            eprintln!("[gossip-task] inbound channel full, {} messages dropped", drop_count);
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[gossip-task] rejected inbound message: {}", e);
                                }
                            }
                        }
                        SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                            println!("[gossip] peer connected: {}", peer_id);
                            gossip_node.add_explicit_peer(&peer_id);
                            gossip_peer_count_writer.fetch_add(1, Ordering::Relaxed);
                        }
                        SwarmEvent::ConnectionClosed { peer_id, .. } => {
                            eprintln!("[gossip] peer disconnected: {}", peer_id);
                            gossip_peer_count_writer.fetch_sub(1, Ordering::Relaxed);
                        }
                        _ => {} // Ignore other swarm events
                    }
                }
                Some(cmd) = swarm_cmd_rx.recv() => {
                    match cmd {
                        SwarmCommand::Publish { pool_id, msg } => {
                            if let Err(e) = gossip_node.publish_to_pool(pool_id, &msg) {
                                eprintln!("[gossip-task] publish error: {}", e);
                            }
                        }
                        SwarmCommand::SubscribePool(pool_id) => {
                            if let Err(e) = gossip_node.subscribe_pool(pool_id) {
                                eprintln!("[gossip-task] subscribe error for pool {}: {}", pool_id, e);
                            }
                        }
                        SwarmCommand::UnsubscribePool(pool_id) => {
                            if let Err(e) = gossip_node.unsubscribe_pool(pool_id) {
                                eprintln!("[gossip-task] unsubscribe error for pool {}: {}", pool_id, e);
                            }
                        }
                    }
                }
            }
        }
    });

    // ── 6. Strategy WebSocket server ─────────────────────────────────
    // Prefer env var (set by entrypoint) so node + bridge agree on token
    let auth_token = std::env::var("DEADMKT_AUTH_TOKEN")
        .unwrap_or_else(|_| config.strategy_auth_token.clone());
    let network_str = format!("{:?}", config.network).to_lowercase();
    let strategy_addr = format!("0.0.0.0:{}", config.strategy_port);
    let auth_details = deadmkt_strategy::AuthOkDetails {
        trustee_address: config.trustee_address.clone(),
        beneficiary_address: config.beneficiary_address.clone(),
        markets: config.markets.clone(),
        token_decimals: token_decimals.clone(),
        price_decimals: 8,
        contract_address: config.contracts.settlement.clone(),
    };
    let strategy_server = StrategyServer::start(
        &strategy_addr,
        auth_token.clone(),
        config.nft_id,
        network_str,
        Some(auth_details),
    )
    .await?;
    println!("  Strategy: ws://0.0.0.0:{}", config.strategy_port);
    println!("  Token:    {}", auth_token);

    // ── 6b. Token action worker (background chain submissions) ───────
    // Shared lock prevents sequence number collisions between token worker
    // and settlement worker (both submit from the same address).
    let tx_lock = Arc::new(tokio::sync::Mutex::new(()));

    let _token_worker_handle = {
        let mut token_client = crate::setup_bridge::SupraSetupClient::new(
            config.rpc_urls.clone(),
            config.contracts.settlement.clone(), // all modules at same address
        );
        token_client.set_gas_config(config.chain_id, config.max_gas_amount, config.gas_unit_price);
        token_client.set_signer(
            &signing_key.to_bytes(),
            signing_key.verifying_key().as_bytes(),
            &config.trustee_address,
        );
        let token_rx = strategy_server.take_token_action_rx().await
            .expect("token_action_rx already taken");
        crate::token_worker::spawn_token_worker(
            Arc::new(token_client),
            token_rx,
            strategy_server.event_sender(),
            tx_lock.clone(),
        )
    };
    println!("  Tokens:   worker started");

    // ── 7. Subsystem construction ────────────────────────────────────
    let mut node_state = NodeStateMachine::new();
    let _ = node_state.transition(NodeEvent::SetupComplete); // Setup → Connected

    let mut gas_manager = GasManager::new(8); // SUPRA has 8 decimals
    {
        let supra_bal = fetch_supra_balance(&chain, &config.trustee_address).await;
        gas_manager.update_balance(supra_bal);
        println!("  Gas bal:   {} SUPRA (status: {:?})", gas_manager.balance_display(), gas_manager.check_status());
    }

    let batch_bridge = BatchStateBridge::new(epoch.clone(), params.clone(), num_pools, config.nft_id);
    let gossip_bridge = GossipBridge::new();

    // Market configs for matching engine — fetch min_quantity from chain
    let mut market_configs = Vec::new();
    for pair in &config.markets {
        let symbol_hex = format!("0x{}", hex::encode(pair.as_bytes()));
        let min_qty = match chain.view_raw(
            "settlement", "get_market_pair", vec![],
            vec![serde_json::json!(symbol_hex)],
        ).await {
            Ok(r) => {
                let pair_obj = r.get(0).unwrap_or(&r);
                pair_obj.get("min_quantity")
                    .or_else(|| pair_obj.get(3)) // 4th field in array form
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(1) // floor: at least 1 base unit
            }
            Err(e) => {
                eprintln!("  [market] Failed to fetch min_quantity for {}: {}, using 1", pair, e);
                1
            }
        };
        market_configs.push(MarketConfig {
            symbol: pair.as_bytes().to_vec(),
            min_quantity: min_qty,
        });
        eprintln!("  [market] {} min_quantity={}", pair, min_qty);
    }

    let mut orchestrator = Orchestrator::new(
        gossip_bridge,
        batch_bridge,
        config.nft_id,
        params.commits_per_batch as u32,
    );
    orchestrator.market_configs = market_configs;

    // Settlement submitter (Build 4A) — wrapped in Arc for background worker
    let settlement_submitter = SettlementSubmitter::new(
        SupraClient::new(
            config.rpc_urls.clone(),
            config.contracts.settlement.clone(),
        ),
        signing_key.clone(),
        &config.contracts.settlement,
        &config.trustee_address,
        config.chain_id,
    ).ok().map(|mut sub| {
        sub.set_gas_config(config.max_gas_amount, config.gas_unit_price);
        Arc::new(sub)
    });
    if settlement_submitter.is_some() {
        println!("  Settle:   submitter ready");
    } else {
        eprintln!("  Settle:   WARNING: submitter init failed, settlements disabled");
    }

    // Async settlement worker channels + spawn
    let (settle_tx, settle_rx) = tokio::sync::mpsc::channel::<SettleRequest>(64);
    let (settle_result_tx, mut settle_result_rx) = tokio::sync::mpsc::channel(64);
    let _settle_worker = if let Some(ref sub) = settlement_submitter {
        Some(spawn_worker(sub.clone(), settle_rx, settle_result_tx, tx_lock.clone()))
    } else {
        None
    };

    // Settlement manager (4B: confirmation, abort handling, backstop)
    let settlement_mgr = Arc::new(Mutex::new(
        SettlementManager::new(_tracker.clone(), config.nft_id)
    ));

    // ── SEC-1/SEC-2: Gossip validator + NFT pubkey cache ─────────────
    let nft_client = SupraClient::new(
        config.rpc_urls.clone(),
        config.contracts.nft.clone(),
    );
    let mut gossip_validator = GossipValidator::new(batch_id);

    // Strategy connection tracking for state transitions
    let mut strategy_was_connected = false;
    let mut first_batch_entered = false;

    // Event polling state
    let mut last_event_block: u64 = ledger.block_height;

    println!("\n  Node ready. Entering poll loop...\n");
    println!("────────────────────────────────────────");

    // ── 8. Main event loop ───────────────────────────────────────────
    //
    // GOSSIP-1 architecture:
    //   - Gossip swarm runs in a dedicated tokio::spawn task (above)
    //   - Inbound gossip messages arrive via gossip_inbound_rx channel
    //   - Outbound messages sent via swarm_cmd_tx channel
    //
    // Three select arms:
    //   1. Gossip inbound — eagerly processes messages as they arrive
    //   2. Block update from dedicated chain poller — drives phase handling
    //   3. Ctrl-C — graceful shutdown
    //
    // N9: Chain polling runs in a dedicated tokio::spawn task. The RPC call
    // to get_ledger_info() no longer blocks the main select loop, so gossip
    // messages are processed without delay even during slow/stalled RPCs.
    // This is the root cause fix for the commit timeout / WS disconnect loop.

    let mut last_block = ledger.block_height;
    let mut last_phase = phase;
    let mut last_batch_id = batch_id;
    let mut last_new_block_time = std::time::Instant::now();
    let stale_warn_secs = 60; // warn after 60s of no new blocks

    // N9: Spawn dedicated chain poller task
    let (block_tx, mut block_rx) = tokio::sync::watch::channel(ledger.block_height);
    {
        let poller_client = SupraClient::new(
            config.rpc_urls.clone(),
            config.contracts.settlement.clone(),
        );
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            loop {
                tick.tick().await;
                match poller_client.get_ledger_info().await {
                    Ok(l) => { let _ = block_tx.send(l.block_height); }
                    Err(e) => { eprintln!("[chain-poller] error: {}", e); }
                }
            }
        });
    }

    // SP5: Track last batch stats for strategy visibility
    let mut last_batch_data: Option<deadmkt_strategy::LastBatchData> = None;
    let mut cur_batch_matches: u64 = 0;

    // SP8: Node health counters
    let mut uptime_batches: u64 = 0;
    let mut settle_failed_recent: u64 = 0;

    loop {
        tokio::select! {
            // ── Graceful shutdown ────────────────────────────────────
            _ = signal::ctrl_c() => {
                println!("\nShutting down...");
                break;
            }

            // ── ARM 1: Eagerly process inbound gossip ────────────────
            //
            // Messages arrive from the dedicated swarm task. We validate
            // and store them immediately so they're available when the
            // next phase handler runs.
            Some(msg) = gossip_inbound_rx.recv() => {
                process_inbound_gossip(
                    msg, &mut gossip_validator, &mut orchestrator,
                );
            }

            // ── ARM 2: Block update from dedicated chain poller ─────
            //
            // N9: The RPC call runs in a separate task. This arm only fires
            // when a new block height is available — no await, no blocking.
            Ok(()) = block_rx.changed() => {
                let block = *block_rx.borrow();

                // GOSSIP-1: Drain ALL buffered gossip before phase evaluation.
                while let Ok(msg) = gossip_inbound_rx.try_recv() {
                    process_inbound_gossip(
                        msg, &mut gossip_validator, &mut orchestrator,
                    );
                }

                if block <= last_block {
                    let stale_secs = last_new_block_time.elapsed().as_secs();
                    if stale_secs > 0 && stale_secs % stale_warn_secs == 0 {
                        eprintln!("[poll] WARNING: no new blocks for {}s (stuck at {})", stale_secs, last_block);
                    }
                    continue; // no new blocks
                }
                last_new_block_time = std::time::Instant::now();

                // ── Strategy connection state tracking ────────────────
                let strategy_connected_now = strategy_server.is_connected();
                if strategy_connected_now && !strategy_was_connected {
                    match node_state.transition(NodeEvent::StrategyConnected) {
                        Ok(new_state) => println!("[state] StrategyConnected \u{2192} {:?}", new_state),
                        Err(e) => eprintln!("[state] StrategyConnected failed: {}", e),
                    }
                } else if !strategy_connected_now && strategy_was_connected {
                    match node_state.transition(NodeEvent::StrategyDisconnected) {
                        Ok(new_state) => println!("[state] StrategyDisconnected \u{2192} {:?}", new_state),
                        Err(e) => eprintln!("[state] StrategyDisconnected failed: {}", e),
                    }
                    first_batch_entered = false;
                }
                strategy_was_connected = strategy_connected_now;

                // 8b. Compute new phase/batch
                let new_phase = compute_phase(block, &epoch, &params);
                let new_batch_id = compute_batch_id(block, &epoch, &params);

                // Advance the batch state bridge
                orchestrator.batch_state.advance_to_block(block);

                // 8c. Detect phase transition
                if new_phase != last_phase || new_batch_id != last_batch_id {
                    // Job 5: Skip-ahead safety net — if we missed >2 batches,
                    // skip forward without processing stale phases
                    if new_batch_id > last_batch_id + 2 {
                        eprintln!("[skip] jumped {} \u{2192} {} ({} batches), skipping to current",
                                  last_batch_id, new_batch_id, new_batch_id - last_batch_id);
                        last_block = block;
                        last_phase = new_phase;
                        last_batch_id = new_batch_id;
                        continue;
                    }

                    println!("[block {}] batch={} phase={} (was batch={} phase={})",
                             block, new_batch_id, new_phase, last_batch_id, last_phase);

                    // New batch boundary
                    if new_batch_id != last_batch_id {
                        // SP5: Snapshot last batch stats before resetting
                        if last_batch_id > 0 {
                            last_batch_data = Some(deadmkt_strategy::LastBatchData {
                                batch_id: last_batch_id,
                                matches: cur_batch_matches,
                                volume: "0".to_string(), // TODO: track volume
                            });
                        }
                        cur_batch_matches = 0;
                        uptime_batches += 1;
                        settle_failed_recent = settle_failed_recent.saturating_sub(1); // natural decay

                        // ── SEC-1/SEC-2: Gossip validator maintenance ────
                        gossip_validator.update_batch(new_batch_id);
                        gossip_validator.expire_old_batches();
                        gossip_validator.log_and_reset_stats();

                        // Expire old inbound gossip data outside batch window
                        let gc_lower = new_batch_id.saturating_sub(BATCH_WINDOW);
                        orchestrator.gossip.inbound_commits.retain(|&b, _| b >= gc_lower);
                        orchestrator.gossip.inbound_reveals.retain(|&b, _| b >= gc_lower);

                        // Async fetch pubkeys for any unknown NFTs we've encountered
                        if gossip_validator.should_fetch_pubkeys(new_batch_id) {
                            let unknown = gossip_validator.take_unknown_nfts();
                            for nft_id in unknown {
                                match nft_client.get_trustee_pubkey(nft_id).await {
                                    Ok(key_bytes) => {
                                        if key_bytes.len() == 32 {
                                            let key_arr: [u8; 32] = key_bytes.try_into().unwrap();
                                            match VerifyingKey::from_bytes(&key_arr) {
                                                Ok(vk) => {
                                                    println!("[gossip-validator] cached pubkey for nft={}", nft_id);
                                                    gossip_validator.cache_pubkey(nft_id, vk);
                                                }
                                                Err(e) => {
                                                    eprintln!("[gossip-validator] invalid pubkey for nft={}: {}", nft_id, e);
                                                }
                                            }
                                        } else {
                                            eprintln!("[gossip-validator] unexpected pubkey length {} for nft={}", key_bytes.len(), nft_id);
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("[gossip-validator] failed to fetch pubkey for nft={}: {}", nft_id, e);
                                        // Re-add to unknown so we retry later
                                        gossip_validator.unknown_nfts.insert(nft_id);
                                    }
                                }
                            }
                            gossip_validator.last_pubkey_fetch_batch = new_batch_id;
                        }

                        // Re-fetch params + epoch from chain (governance may have applied)
                        if let Ok(new_params) = fetch_batch_params(
                            &pool_config_client, &config.contracts.pool_config
                        ).await {
                            if new_params.blocks_per_batch != params.blocks_per_batch {
                                println!("[params] updated: {} blocks/batch ({}/{}/{}/{})",
                                    new_params.blocks_per_batch,
                                    new_params.commit_blocks, new_params.reveal_blocks,
                                    new_params.match_blocks, new_params.swap_blocks);
                            }
                            params = new_params;
                        }
                        if let Ok(new_epoch) = fetch_batch_epoch(
                            &pool_config_client, &config.contracts.pool_config
                        ).await {
                            epoch = new_epoch;
                        }

                        // Re-fetch pool count (mitosis may have split/merged)
                        if let Ok(new_num_pools) = fetch_num_pools(
                            &pool_config_client, &config.contracts.pool_config
                        ).await {
                            if new_num_pools != num_pools {
                                println!("[pools] {} -> {} pools", num_pools, new_num_pools);
                                num_pools = new_num_pools;
                                orchestrator.batch_state.set_num_pools(num_pools);
                            }
                        }

                        // Heartbeat on NFT-offset schedule
                        // ── Periodic gas balance refresh ──────────────
                        if new_batch_id % 10 == 0 {
                            let supra_bal = fetch_supra_balance(&chain, &config.trustee_address).await;
                            let (old_status, new_status) = gas_manager.update_balance(supra_bal);
                            if new_status != old_status {
                                match new_status {
                                    deadmkt_gas_manager::GasStatus::Low =>
                                        eprintln!("[gas] WARNING: balance low ({})", gas_manager.balance_display()),
                                    deadmkt_gas_manager::GasStatus::Critical =>
                                        eprintln!("[gas] CRITICAL: balance depleted ({})", gas_manager.balance_display()),
                                    deadmkt_gas_manager::GasStatus::Normal =>
                                        println!("[gas] balance recovered ({})", gas_manager.balance_display()),
                                }
                            }
                        }

                        // ── Heartbeat ──────────────────────────────────
                        // Skip if a settlement recently refreshed the heartbeat on-chain
                        let has_capital = escrow_balances.values().any(|v| {
                            v.parse::<f64>().unwrap_or(0.0) > 0.0
                        });
                        let settlement_refreshed = last_settlement_batch > 0
                            && new_batch_id.saturating_sub(last_settlement_batch) < heartbeat_interval;
                        if has_capital
                            && !settlement_refreshed
                            && heartbeat_interval > 0
                            && new_batch_id % heartbeat_interval == (config.nft_id % heartbeat_interval)
                        {
                            eprintln!("[heartbeat] submitting with max_gas={}, gas_price={}, chain_id={}",
                                config.max_gas_amount, config.gas_unit_price, config.chain_id);
                            match submit_heartbeat(
                                &escrow_client,
                                &signing_key,
                                &config.contracts.escrow,
                                &config.trustee_address,
                                config.chain_id,
                                config.max_gas_amount,
                                config.gas_unit_price,
                            ).await {
                                Ok(_) => {
                                    if consecutive_hb_failures > 0 {
                                        println!("[heartbeat] recovered after {} consecutive failures",
                                            consecutive_hb_failures);
                                    }
                                    consecutive_hb_failures = 0;
                                    gas_low = false;
                                    println!("[heartbeat] sent at batch {}", new_batch_id);
                                }
                                Err(e) => {
                                    consecutive_hb_failures += 1;
                                    let err_str = e.to_string();
                                    if err_str.contains("INSUFFICIENT_BALANCE") {
                                        gas_low = true;
                                        eprintln!("[heartbeat] FAILED: out of gas (sweep disabled)");
                                    }
                                    let est_missed = consecutive_hb_failures * heartbeat_interval;
                                    eprintln!("[heartbeat] FAILED (attempt {}): {}",
                                        consecutive_hb_failures, e);
                                    if est_missed > heartbeat_timeout / 2 {
                                        eprintln!("[heartbeat] WARNING: at risk of reap in ~{} batches",
                                            heartbeat_timeout.saturating_sub(est_missed));
                                    }
                                }
                            }
                        }

                        // Re-fetch heartbeat config every 100 batches, dispersed by NFT ID
                        if new_batch_id % 100 == (config.nft_id % 100) {
                            if let Ok(new_interval) = fetch_heartbeat_interval(
                                &pool_config_client, &config.contracts.pool_config
                            ).await {
                                let clamped = if new_interval == 0 { 10 } else { new_interval };
                                if clamped != heartbeat_interval {
                                    println!("[heartbeat] interval updated: {} -> {}",
                                        heartbeat_interval, clamped);
                                    heartbeat_interval = clamped;
                                }
                            }
                            if let Ok(new_timeout) = fetch_heartbeat_timeout(
                                &pool_config_client, &config.contracts.pool_config
                            ).await {
                                if new_timeout != heartbeat_timeout {
                                    println!("[heartbeat] timeout updated: {} -> {}",
                                        heartbeat_timeout, new_timeout);
                                    heartbeat_timeout = new_timeout;
                                }
                            }
                        }

                        // Profit sweep: periodically check and sweep excess to beneficiary
                        if config.profit_taking.threshold_pct > 0
                            && !gas_low
                            && new_batch_id.saturating_sub(last_sweep_batch) >= sweep_interval
                        {
                            last_sweep_batch = new_batch_id;
                            match check_and_sweep_profits(
                                &escrow_client,
                                &signing_key,
                                &config,
                                &escrow_balances,
                                &token_decimals,
                            ).await {
                                Ok(()) => {},
                                Err(e) => eprintln!("[sweep] error: {}", e),
                            }
                        }

                        // Clear reveals from previous batch.
                        // Commits/committed_orders preserved until next COMMIT phase.
                        orchestrator.published_reveals.clear();

                        // Re-compute pool assignment (via command channel to swarm task)
                        let new_pool = compute_pool_assignment(
                            config.nft_id, new_batch_id, orchestrator.batch_state.num_pools().max(1)
                        );
                        if new_pool != my_pool {
                            let _ = swarm_cmd_tx.try_send(SwarmCommand::UnsubscribePool(my_pool));
                            let _ = swarm_cmd_tx.try_send(SwarmCommand::SubscribePool(new_pool));
                            my_pool = new_pool;
                        }

                        // Notify strategy of new batch
                        let (esc_proj, esc_conf) = build_escrow_views(&_tracker, &token_decimals);
                        let pending_setts: Vec<deadmkt_strategy::PendingSettlementData> = {
                            let mgr = settlement_mgr.lock().unwrap();
                            mgr.pending_summary().iter().map(|(h, b, s)| {
                                deadmkt_strategy::PendingSettlementData {
                                    match_hash: h.clone(), batch_id: *b, status: s.clone(),
                                }
                            }).collect()
                        };
                        let peers = gossip_peer_count.load(Ordering::Relaxed);
                        let health = deadmkt_strategy::NodeHealthData {
                            gas_status: format!("{:?}", gas_manager.check_status()),
                            gossip_connected: peers > 0,
                            gossip_peers: peers,
                            settle_pending_count: pending_setts.len() as u64,
                            settle_failed_recent,
                            uptime_batches,
                            block_height: block,
                            timestamp: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs()).unwrap_or(0),
                        };
                        let batch_start = StrategyEvent::BatchStart {
                            data: make_batch_start(new_batch_id, new_pool, &params, num_pools, &esc_proj, &esc_conf, &wallet_balances, &gas_manager.balance_display(), peers, &last_batch_data, pending_setts, Some(health), &mint_state, &circulating, &vault_locks),
                        };
                        let _ = strategy_server.send_event(batch_start).await;
                    }

                    // ── Phase handlers ────────────────────────────────
                    match new_phase {
                        Phase::Commit => {
                            // Fresh commit cycle
                            orchestrator.published_commits.clear();
                            orchestrator.committed_orders.clear();

                            // Transition Observing \u{2192} Trading on first COMMIT
                            if !first_batch_entered && node_state.state() == NodeState::Observing {
                                match node_state.transition(NodeEvent::FirstBatchEntered) {
                                    Ok(new_state) => {
                                        println!("[state] FirstBatchEntered \u{2192} {:?}", new_state);
                                        first_batch_entered = true;
                                    }
                                    Err(e) => eprintln!("[state] FirstBatchEntered failed: {}", e),
                                }
                            }

                            let gas_ok = gas_manager.check_status() != deadmkt_gas_manager::GasStatus::Critical;
                            let can_commit = node_state.state() == NodeState::Trading && gas_ok;
                            if !gas_ok {
                                eprintln!("[commit] SKIPPED — gas critical ({})", gas_manager.balance_display());
                            }
                            let orders = if can_commit && strategy_server.is_connected() {
                                let pool_id = orchestrator.batch_state.current_pool_id(config.nft_id);
                                let (esc_proj2, esc_conf2) = build_escrow_views(&_tracker, &token_decimals);
                                let pending_setts2: Vec<deadmkt_strategy::PendingSettlementData> = {
                                    let mgr = settlement_mgr.lock().unwrap();
                                    mgr.pending_summary().iter().map(|(h, b, s)| {
                                        deadmkt_strategy::PendingSettlementData {
                                            match_hash: h.clone(), batch_id: *b, status: s.clone(),
                                        }
                                    }).collect()
                                };
                                let peers2 = gossip_peer_count.load(Ordering::Relaxed);
                                let health2 = deadmkt_strategy::NodeHealthData {
                                    gas_status: format!("{:?}", gas_manager.check_status()),
                                    gossip_connected: peers2 > 0,
                                    gossip_peers: peers2,
                                    settle_pending_count: pending_setts2.len() as u64,
                                    settle_failed_recent,
                                    uptime_batches,
                                    block_height: block,
                                    timestamp: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_secs()).unwrap_or(0),
                                };
                                let _ = strategy_server.send_event(StrategyEvent::BatchStart {
                                    data: make_batch_start(new_batch_id, pool_id, &params, num_pools, &esc_proj2, &esc_conf2, &wallet_balances, &gas_manager.balance_display(), peers2, &last_batch_data, pending_setts2, Some(health2), &mint_state, &circulating, &vault_locks),
                                }).await;

                                match strategy_server.receive_action_with_timeout(
                                    Duration::from_secs(3)
                                ).await {
                                    Some(StrategyAction::Commit { orders: order_specs }) => {
                                        let validated = convert_strategy_orders(
                                            &order_specs,
                                            &signing_key,
                                            config.nft_id,
                                            new_batch_id,
                                            &params,
                                            &_tracker,
                                            &token_decimals,
                                            &orchestrator.market_configs,
                                        );
                                        if validated.is_empty() {
                                            println!("[commit] all orders rejected during validation");
                                            None
                                        } else {
                                            println!("[commit] {} orders validated from strategy", validated.len());
                                            Some(validated)
                                        }
                                    }
                                    Some(_) => None, // Auth/Reveal — not expected during commit. Token actions routed to worker.
                                    None => None,
                                }
                            } else {
                                None
                            };

                            let n = orchestrator.on_commit_phase(can_commit, orders);
                            if n > 0 {
                                println!("[commit] published {} commits", n);
                            }

                            // Flush outbound commits to gossip via command channel
                            flush_gossip_via_channel(&orchestrator.gossip, &swarm_cmd_tx);
                        }
                        Phase::Reveal => {
                            let my_commits = orchestrator.published_commits.len();

                            // SP6a: Send reveal_start to strategy, allow selective reveal
                            let strategy_indices = if my_commits > 0 && strategy_server.is_connected() {
                                let commit_summaries: Vec<String> = orchestrator.published_commits
                                    .iter().map(|c| hex::encode(&c.commit_hash)).collect();
                                let reveal_event = StrategyEvent::RevealStart {
                                    data: deadmkt_strategy::RevealStartData {
                                        batch_id: new_batch_id,
                                        my_commits: commit_summaries,
                                    },
                                };
                                let _ = strategy_server.send_event(reveal_event).await;
                                match strategy_server.receive_action_with_timeout(
                                    Duration::from_secs(5)
                                ).await {
                                    Some(StrategyAction::Reveal { reveal_indices }) =>
                                        Some(reveal_indices),
                                    _ => None, // timeout or other → reveal all
                                }
                            } else {
                                None
                            };

                            let n = orchestrator.on_reveal_phase(my_commits, strategy_indices);
                            if n > 0 {
                                println!("[reveal] published {} reveals", n);
                            }

                            // Flush outbound reveals to gossip via command channel
                            flush_gossip_via_channel(&orchestrator.gossip, &swarm_cmd_tx);
                        }
                        Phase::Match => {
                            // GOSSIP-1: Final drain before matching — catch anything
                            // that arrived during the reveal phase processing above.
                            while let Ok(msg) = gossip_inbound_rx.try_recv() {
                                process_inbound_gossip(
                                    msg, &mut gossip_validator, &mut orchestrator,
                                );
                            }

                            let n = orchestrator.on_match_phase();
                            if n > 0 {
                                println!("[match] {} matches found", n);
                            }
                            cur_batch_matches = n as u64;

                            // SP6b: Send match_result to strategy
                            if strategy_server.is_connected() {
                                let match_summaries: Vec<deadmkt_strategy::MatchSummary> =
                                    orchestrator.current_matches.iter().map(|m| {
                                        let sym = String::from_utf8_lossy(&m.symbol).to_string();
                                        let is_buyer = m.buyer.order.nft_id == config.nft_id;
                                        deadmkt_strategy::MatchSummary {
                                            pair: sym,
                                            side: if is_buyer { "buy".to_string() } else { "sell".to_string() },
                                            price: m.settlement_price.to_string(),
                                            quantity: m.fill_quantity.to_string(),
                                            counterparty_nft_id: if is_buyer {
                                                m.seller.order.nft_id
                                            } else {
                                                m.buyer.order.nft_id
                                            },
                                        }
                                    }).collect();
                                let _ = strategy_server.send_event(StrategyEvent::MatchResult {
                                    data: deadmkt_strategy::MatchResultData {
                                        batch_id: new_batch_id,
                                        matches: match_summaries,
                                    },
                                }).await;
                            }
                        }
                        Phase::Swap => {
                            let n = orchestrator.on_swap_phase();
                            if n > 0 {
                                println!("[swap] {} matches to settle", n);

                                if settlement_submitter.is_some() {
                                    let matches = orchestrator.take_matches();
                                    let swap_start_block = block;

                                    for m in &matches {
                                        let match_hash_hex = hex::encode(m.match_hash);
                                        let is_gas_payer = m.gas_payer_nft_id == config.nft_id;

                                        let symbol_str = String::from_utf8_lossy(&m.symbol);
                                        let (base_token, quote_token) = if let Some(slash) = symbol_str.find('/') {
                                            (&symbol_str[..slash], &symbol_str[slash+1..])
                                        } else {
                                            (symbol_str.as_ref(), "UNKNOWN")
                                        };

                                        {
                                            let mut mgr = settlement_mgr.lock().unwrap();
                                            let base_dec = token_decimals.get(base_token).copied().unwrap_or(5);
                                            let quote_dec = token_decimals.get(quote_token).copied().unwrap_or(5);
                                            if let Err(e) = mgr.register_match(
                                                m.clone(),
                                                is_gas_payer,
                                                swap_start_block,
                                                params.blocks_per_batch,
                                                base_token,
                                                quote_token,
                                                base_dec,
                                                quote_dec,
                                            ) {
                                                eprintln!("[swap] register_match error: {}", e);
                                                continue;
                                            }
                                        }

                                        if is_gas_payer {
                                            let req = SettleRequest {
                                                match_data: m.clone(),
                                                match_hash: match_hash_hex.clone(),
                                                is_gas_payer: true,
                                            };
                                            if let Err(e) = settle_tx.try_send(req) {
                                                eprintln!("[swap] failed to queue settlement {}: {}", &match_hash_hex[..12], e);
                                            } else {
                                                println!("[swap] queued {} for async submission", &match_hash_hex[..12]);
                                            }
                                        } else {
                                            println!("[swap] {} \u{2014} counterparty pays gas", &match_hash_hex[..12]);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // 8d. Drain async settlement results from background worker
                while let Ok(result) = settle_result_rx.try_recv() {
                    let mut mgr = settlement_mgr.lock().unwrap();
                    match result {
                        SettleResult::Submitted { match_hash, tx_hash } => {
                            let short = if match_hash.len() >= 12 { &match_hash[..12] } else { &match_hash };
                            mgr.mark_submitted(&match_hash, tx_hash.clone(), block);
                            println!("[settle] submitted {} \u{2192} {}", short, tx_hash);
                        }
                        other => {
                            let hash = other.match_hash().to_string();
                            let short = if hash.len() >= 12 { &hash[..12] } else { &hash };
                            let short = short.to_string();
                            // Detect gas exhaustion from settlement failures
                            let mut error_reason = String::new();
                            if let SettleResult::RpcError { ref error, .. } = other {
                                if error.contains("INSUFFICIENT_BALANCE") {
                                    gas_low = true;
                                }
                                error_reason = error.clone();
                            }
                            if let SettleResult::Failed { ref code, .. } = other {
                                error_reason = format!("{:?}", code);
                            }

                            // SP6c: Notify strategy of settlement failure
                            if !error_reason.is_empty() {
                                settle_failed_recent += 1;
                                orchestrator.sent_events.push(StrategyEvent::SettlementFailed {
                                    data: deadmkt_strategy::SettlementFailedData {
                                        batch_id: new_batch_id,
                                        match_hash: hash.clone(),
                                        reason: error_reason,
                                    },
                                });
                            }

                            if let Some(action) = mgr.process_result(other) {
                                println!("[settle] {} \u{2192} {:?}", short, action);
                            }
                        }
                    }
                }

                // 8e. Poll for chain events (settlements, governance)
                if block > last_event_block {
                    let event_type = format!(
                        "{}::settlement::BatchTradeSettled",
                        config.contracts.settlement
                    );
                    match chain.get_events(&event_type, last_event_block + 1, block, 50).await {
                        Ok(events) => {
                            for event in &events {
                                orchestrator.on_chain_event(event);
                                if let deadmkt_chain::types::ChainEvent::BatchTradeSettled(ref evt) = event {
                                    let mut mgr = settlement_mgr.lock().unwrap();
                                    if mgr.on_settlement_confirmed(evt) {
                                        println!("[event] settlement confirmed: batch={} trade={}",
                                                 evt.batch_id, evt.trade_id);
                                        // Settlement is an implicit heartbeat on-chain.
                                        // Track it so we can skip redundant explicit heartbeats.
                                        last_settlement_batch = evt.batch_id;
                                    }
                                }
                            }
                            if !events.is_empty() {
                                println!("[event] processed {} chain events", events.len());
                            }
                        }
                        Err(e) => {
                            eprintln!("[event] poll error: {}", e);
                        }
                    }
                    last_event_block = block;
                }

                // Route queued orchestrator events to strategy WS
                drain_strategy_events(&mut orchestrator, &strategy_server).await;

                last_block = block;
                last_phase = new_phase;
                last_batch_id = new_batch_id;
            }
        }
    }

    println!("Node stopped.");
    Ok(())
}

// =========================================================================
// Helpers
// =========================================================================

/// GOSSIP-1: Process a single inbound gossip message — validate and store.
///
/// Extracted as a helper to avoid code duplication between the eager
/// select arm (ARM 1) and the batch drain at chain tick start.
fn process_inbound_gossip(
    msg: GossipMessage,
    validator: &mut GossipValidator,
    orchestrator: &mut Orchestrator<GossipBridge, BatchStateBridge>,
) {
    match msg {
        GossipMessage::Commitment { batch_id, pool_id, hash, nft_id, .. } => {
            // SEC-2: Bounds + content-hash dedup + rate limit
            if validator.accept_commit(nft_id, batch_id, &hash) {
                let commit = CommitMessage {
                    nft_id,
                    pool_id,
                    batch_id,
                    commit_hash: hash.to_vec(),
                };
                orchestrator.gossip.inbound_commits
                    .entry(batch_id)
                    .or_default()
                    .push(commit);
                eprintln!("[gossip] \u{2190} commit from nft={} batch={}", nft_id, batch_id);
            }
        }
        GossipMessage::Reveal { batch_id, pool_id, order, signature } => {
            // SEC-1/SEC-2: Bounds + dedup + signature verification
            let order_bytes = deadmkt_crypto::encode_order(&order)
                .unwrap_or_default();
            let nft_id = order.nft_id;
            if validator.accept_reveal(nft_id, batch_id, &order_bytes, &signature) {
                let reveal = RevealMessage {
                    nft_id,
                    pool_id,
                    batch_id,
                    order_bytes,
                    signature,
                };
                orchestrator.gossip.inbound_reveals
                    .entry(batch_id)
                    .or_default()
                    .push(reveal);
                eprintln!("[gossip] \u{2190} reveal from nft={} batch={} (sig \u{2713})", nft_id, batch_id);
            }
        }
        GossipMessage::BatchComplete { .. } |
        GossipMessage::SettlementReport { .. } => {
            // Handled by gossip_validation in full impl
        }
    }
}

/// GOSSIP-1: Flush outbound gossip messages via the swarm command channel.
///
/// Sends pending commits and reveals to the dedicated gossip task
/// for publishing. Non-blocking — if the channel is full (64 capacity),
/// log an error (indicates the gossip task is stalled).
fn flush_gossip_via_channel(
    bridge: &GossipBridge,
    cmd_tx: &tokio::sync::mpsc::Sender<SwarmCommand>,
) {
    let commits: Vec<_> = bridge.pending_commits.lock().unwrap().drain(..).collect();
    for (pool_id, msg) in commits {
        if let Err(e) = cmd_tx.try_send(SwarmCommand::Publish { pool_id, msg }) {
            eprintln!("[gossip] commit publish error (channel): {}", e);
        }
    }

    let reveals: Vec<_> = bridge.pending_reveals.lock().unwrap().drain(..).collect();
    for (pool_id, msg) in reveals {
        if let Err(e) = cmd_tx.try_send(SwarmCommand::Publish { pool_id, msg }) {
            eprintln!("[gossip] reveal publish error (channel): {}", e);
        }
    }
}

/// Drain queued strategy events from orchestrator and send to WS.
async fn drain_strategy_events(
    orchestrator: &mut Orchestrator<GossipBridge, BatchStateBridge>,
    server: &StrategyServer,
) {
    for event in orchestrator.sent_events.drain(..) {
        if let Err(e) = server.send_event(event).await {
            // Strategy not connected — events are lost (by design, CD-16).
            let _ = e;
        }
    }
}

/// Poll gossip swarm for inbound messages (non-blocking).
/// Convert ed25519-dalek signing key to libp2p keypair.
fn ed25519_to_libp2p(signing_key: &SigningKey) -> Keypair {
    let secret_bytes = signing_key.to_bytes();
    let ed_keypair = libp2p::identity::ed25519::Keypair::try_from_bytes(
        &mut [secret_bytes.as_slice(), signing_key.verifying_key().as_bytes().as_slice()].concat()
    ).expect("valid ed25519 keypair");
    Keypair::from(ed_keypair)
}

/// Fetch batch params from pool_config contract via view function.
async fn fetch_batch_params(
    chain: &SupraClient,
    _pool_config_addr: &str,
) -> Result<BatchParams, Box<dyn std::error::Error>> {
    let json = chain.view_raw(
        "pool_config",
        "get_batch_params",
        vec![],
        vec![],
    ).await?;

    Ok(BatchParams::from_view_result(&json)?)
}

/// Fetch batch epoch from pool_config contract via view function.
async fn fetch_batch_epoch(
    chain: &SupraClient,
    _pool_config_addr: &str,
) -> Result<BatchEpoch, Box<dyn std::error::Error>> {
    let json = chain.view_raw(
        "pool_config",
        "get_batch_epoch",
        vec![],
        vec![],
    ).await?;

    Ok(BatchEpoch::from_view_result(&json)?)
}

async fn fetch_num_pools(
    chain: &SupraClient,
    _pool_config_addr: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    let json = chain.view_raw(
        "pool_config",
        "get_num_pools",
        vec![],
        vec![],
    ).await?;

    let count_str = json.as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .ok_or("unexpected num_pools response format")?;
    Ok(count_str.parse::<u64>()?)
}

async fn fetch_protocol_version(
    chain: &SupraClient,
    _pool_config_addr: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    let json = chain.view_raw(
        "pool_config",
        "get_protocol_version",
        vec![],
        vec![],
    ).await?;

    // Result is ["1"] — single u64 string
    let version_str = json.as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .ok_or("unexpected protocol version response format")?;
    Ok(version_str.parse::<u64>()?)
}

async fn fetch_heartbeat_interval(
    chain: &SupraClient,
    _pool_config_addr: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    let json = chain.view_raw(
        "pool_config",
        "get_recommended_heartbeat_interval",
        vec![],
        vec![],
    ).await?;

    let val_str = json.as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .ok_or("unexpected heartbeat interval response format")?;
    Ok(val_str.parse::<u64>()?)
}

async fn fetch_heartbeat_timeout(
    chain: &SupraClient,
    _pool_config_addr: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    let json = chain.view_raw(
        "pool_config",
        "get_heartbeat_timeout",
        vec![],
        vec![],
    ).await?;

    let val_str = json.as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .ok_or("unexpected heartbeat timeout response format")?;
    Ok(val_str.parse::<u64>()?)
}

async fn submit_heartbeat(
    client: &SupraClient,
    signing_key: &SigningKey,
    escrow_contract_addr: &str,
    sender_addr: &str,
    chain_id: u8,
    max_gas_amount: u64,
    gas_unit_price: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    let contract_addr = deadmkt_settlement::parse_address(escrow_contract_addr)
        .map_err(|e| format!("invalid escrow contract address: {:?}", e))?;
    let sender = deadmkt_settlement::parse_address(sender_addr)
        .map_err(|e| format!("invalid sender address: {:?}", e))?;

    // Get sequence number
    let account = client.get_account(&format!("0x{}", hex::encode(sender))).await?;

    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + 60;

    // heartbeat() takes no args -- signer is implicit from tx sender
    let signed_tx = deadmkt_settlement::build_and_sign_entry_function(
        signing_key,
        sender,
        contract_addr,
        "escrow",
        "heartbeat",
        vec![],  // no args
        account.sequence_number,
        expiry,
        chain_id,
        max_gas_amount,
        gas_unit_price,
    ).map_err(|e| format!("heartbeat build failed: {:?}", e))?;

    let tx_hash = client.submit_raw(&signed_tx).await?;
    Ok(tx_hash)
}

/// Check escrow balances and sweep profits to beneficiary as SUPRA.
///
/// Logic:
///   1. Parse escrow balances, compare against base_capital + threshold
///   2. If ALL tokens above threshold → calculate min excess above base_capital
///   3. withdraw_triples_to_wallet(nft_id, min_excess)  → tokens to trustee wallet
///   4. burn_mkt(min_excess)                             → burn triples, receive SUPRA
///   5. transfer SUPRA to beneficiary
async fn check_and_sweep_profits(
    client: &SupraClient,
    signing_key: &SigningKey,
    config: &NodeConfig,
    escrow_balances: &HashMap<String, String>,
    token_decimals: &HashMap<String, u8>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Skip if profit-taking is disabled
    if config.profit_taking.threshold_pct == 0 {
        return Ok(());
    }

    let threshold_mul = 1.0 + (config.profit_taking.threshold_pct as f64 / 100.0);

    // Parse escrow balances back to raw u64
    let tokens = ["EMM", "KAY", "TEE"];
    let mut raw_balances: Vec<u64> = Vec::new();
    let mut base_capitals: Vec<u64> = Vec::new();

    for token in &tokens {
        let dec = token_decimals.get(*token).copied().unwrap_or(5) as u32;
        let divisor = 10u64.pow(dec) as f64;

        // Current escrow balance
        let human = escrow_balances.get(*token)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let raw = (human * divisor) as u64;
        raw_balances.push(raw);

        // Base capital (set during setup)
        let base = config.profit_taking.base_capital
            .get(*token)
            .copied()
            .unwrap_or(0);
        base_capitals.push(base);
    }

    // Check: all 3 above threshold?
    for i in 0..3 {
        let target = (base_capitals[i] as f64 * threshold_mul) as u64;
        if raw_balances[i] < target {
            return Ok(()); // Not all above threshold — do nothing
        }
    }

    // Calculate min excess above BASE (not threshold)
    let min_excess = (0..3)
        .map(|i| raw_balances[i] - base_capitals[i])
        .min()
        .unwrap_or(0);

    if min_excess == 0 {
        return Ok(());
    }

    println!("[sweep] All tokens above {}% threshold. Sweeping {} per token...",
        config.profit_taking.threshold_pct, min_excess);

    let contract_addr = deadmkt_settlement::parse_address(&config.contracts.escrow)
        .map_err(|e| format!("bad contract address: {:?}", e))?;
    let sender = deadmkt_settlement::parse_address(&config.trustee_address)
        .map_err(|e| format!("bad sender address: {:?}", e))?;
    let framework_addr = deadmkt_settlement::parse_address("0x0000000000000000000000000000000000000000000000000000000000000001")
        .map_err(|e| format!("bad framework address: {:?}", e))?;

    let sender_hex = format!("0x{}", hex::encode(sender));

    // --- TX 1: withdraw_triples_to_wallet(nft_id, amount) ---
    let account = client.get_account(&sender_hex).await?;
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() + 60;

    let args_withdraw = vec![
        bcs::to_bytes(&config.nft_id).map_err(|e| format!("bcs: {}", e))?,
        bcs::to_bytes(&min_excess).map_err(|e| format!("bcs: {}", e))?,
    ];
    let tx1 = deadmkt_settlement::build_and_sign_entry_function(
        signing_key, sender, contract_addr,
        "escrow", "withdraw_triples_to_wallet",
        args_withdraw, account.sequence_number, expiry,
        config.chain_id, config.max_gas_amount, config.gas_unit_price,
    ).map_err(|e| format!("withdraw tx build failed: {:?}", e))?;

    client.submit_raw(&tx1).await?;
    println!("[sweep] withdraw_triples_to_wallet submitted");

    // --- TX 2: burn_mkt(amount) ---
    let account2 = client.get_account(&sender_hex).await?;
    let args_burn = vec![
        bcs::to_bytes(&min_excess).map_err(|e| format!("bcs: {}", e))?,
    ];
    let tx2 = deadmkt_settlement::build_and_sign_entry_function(
        signing_key, sender, contract_addr,
        "tokens", "burn_mkt",
        args_burn, account2.sequence_number, expiry,
        config.chain_id, config.max_gas_amount, config.gas_unit_price,
    ).map_err(|e| format!("burn tx build failed: {:?}", e))?;

    client.submit_raw(&tx2).await?;
    println!("[sweep] burn_mkt submitted");

    // --- TX 3: transfer SUPRA to beneficiary ---
    // SUPRA returned = min_excess * 3 * SUPRA_PER_TOKEN_UNIT (100)
    let supra_amount = min_excess * 300; // 3 tokens × 100 quants per base unit
    let beneficiary = deadmkt_settlement::parse_address(&config.beneficiary_address)
        .map_err(|e| format!("bad beneficiary address: {:?}", e))?;

    let account3 = client.get_account(&sender_hex).await?;
    let args_transfer = vec![
        bcs::to_bytes(&beneficiary).map_err(|e| format!("bcs: {}", e))?,
        bcs::to_bytes(&supra_amount).map_err(|e| format!("bcs: {}", e))?,
    ];
    let tx3 = deadmkt_settlement::build_and_sign_entry_function(
        signing_key, sender, framework_addr,
        "supra_account", "transfer",
        args_transfer, account3.sequence_number, expiry,
        config.chain_id, config.max_gas_amount, config.gas_unit_price,
    ).map_err(|e| format!("transfer tx build failed: {:?}", e))?;

    client.submit_raw(&tx3).await?;

    let supra_human = supra_amount as f64 / 100_000_000.0;
    println!("[sweep] Swept {:.8} SUPRA to beneficiary {}", supra_human, &config.beneficiary_address[..12]);

    Ok(())
}

/// Fetch SUPRA (gas) coin balance for an address.
async fn fetch_supra_balance(
    chain: &deadmkt_chain::client::SupraClient,
    address: &str,
) -> u64 {
    match chain.view_absolute(
        "0x1::coin::balance",
        vec!["0x1::supra_coin::SupraCoin".to_string()],
        vec![serde_json::json!(address)],
    ).await {
        Ok(val) => {
            val.get(0)
                .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                .unwrap_or(0)
        }
        Err(_) => 0,
    }
}

/// Build projected and confirmed escrow views from the tracker.
fn build_escrow_views(
    tracker: &Arc<Mutex<EscrowTracker>>,
    token_decimals: &HashMap<String, u8>,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let t = tracker.lock().unwrap();
    let mut projected = HashMap::new();
    let mut confirmed = HashMap::new();
    for token in t.tracked_tokens() {
        let dec = token_decimals.get(token.as_str()).copied().unwrap_or(5);
        let factor = 10f64.powi(dec as i32);

        let raw_proj = t.projected(&token);
        projected.insert(token.clone(), format!("{:.width$}", raw_proj as f64 / factor, width = dec as usize));

        if let Some(bal) = t.get_balance(&token) {
            confirmed.insert(token.clone(), format!("{:.width$}", bal.confirmed as f64 / factor, width = dec as usize));
        }
    }
    (projected, confirmed)
}

/// Build a BatchStartData for strategy notification.
fn make_batch_start(
    batch_id: u64,
    pool_id: u64,
    params: &BatchParams,
    num_pools: u64,
    escrow_projected: &HashMap<String, String>,
    escrow_confirmed: &HashMap<String, String>,
    wallet: &HashMap<String, String>,
    gas_balance_display: &str,
    peers_in_pool: u64,
    last_batch: &Option<deadmkt_strategy::LastBatchData>,
    pending_settlements: Vec<deadmkt_strategy::PendingSettlementData>,
    node_health: Option<deadmkt_strategy::NodeHealthData>,
    mint_state: &deadmkt_strategy::MintStateData,
    circulating: &HashMap<String, String>,
    vault_locks: &[deadmkt_strategy::VaultLockData],
) -> deadmkt_strategy::BatchStartData {
    deadmkt_strategy::BatchStartData {
        batch_id,
        pool_id,
        escrow: escrow_projected.clone(),
        escrow_confirmed: escrow_confirmed.clone(),
        wallet: wallet.clone(),
        gas_balance: gas_balance_display.to_string(),
        last_batch: last_batch.clone(),
        batch_params: deadmkt_strategy::BatchParamsData {
            blocks_per_batch: params.blocks_per_batch,
            commits_per_batch: params.commits_per_batch as u32,
            num_pools,
        },
        pending_settlements,
        peers_in_pool,
        mint_state: mint_state.clone(),
        circulating: circulating.clone(),
        vault_locks: vault_locks.to_vec(),
        node_health,
    }
}

/// Resolve a hostname to IP and dial via libp2p.
fn resolve_and_dial(
    gossip_node: &mut GossipNode,
    hostname: &str,
    port: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::net::ToSocketAddrs;
    let socket_addr = format!("{}:{}", hostname, port);
    let resolved = socket_addr.to_socket_addrs()?.next()
        .ok_or_else(|| format!("DNS resolution failed for {}", hostname))?;
    let multiaddr: libp2p::Multiaddr = format!("/ip4/{}/tcp/{}", resolved.ip(), resolved.port())
        .parse()?;
    gossip_node.dial(multiaddr)?;
    Ok(())
}

/// Read password from terminal with echo disabled.
fn read_password() -> Result<String, io::Error> {
    // In a real impl, use rpassword or similar. For now, just read a line.
    let mut password = String::new();
    io::stdin().read_line(&mut password)?;
    Ok(password.trim().to_string())
}

/// Fetch token decimals from on-chain FA metadata for all configured markets.
/// Queries 0x1::fungible_asset::decimals(metadata) for each token.
/// Returns token symbol → decimal places (e.g. {"EMM": 5, "KAY": 5, "TEE": 5}).
async fn fetch_token_decimals(
    chain: &deadmkt_chain::client::SupraClient,
    markets: &[String],
) -> HashMap<String, u8> {
    let mut decimals = HashMap::new();

    for market in markets {
        let symbol_hex = format!("0x{}", hex::encode(market.as_bytes()));
        let pair_result = match chain.view_raw(
            "settlement",
            "get_market_pair",
            vec![],
            vec![serde_json::json!(symbol_hex)],
        ).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  [decimals] Failed to query market pair {}: {}", market, e);
                continue;
            }
        };

        let pair_obj = pair_result.get(0).unwrap_or(&pair_result);
        let base_meta = pair_obj.get("base_metadata").and_then(|v| v.as_str()).unwrap_or("");
        let quote_meta = pair_obj.get("quote_metadata").and_then(|v| v.as_str()).unwrap_or("");

        let parts: Vec<&str> = market.split('/').collect();
        if parts.len() != 2 { continue; }
        let (base_sym, quote_sym) = (parts[0], parts[1]);

        for (sym, meta) in [(base_sym, base_meta), (quote_sym, quote_meta)] {
            if meta.is_empty() || decimals.contains_key(sym) { continue; }

            // Call 0x1::fungible_asset::decimals(metadata_object)
            match chain.view_absolute(
                "0x1::fungible_asset::decimals",
                vec!["0x1::fungible_asset::Metadata".to_string()],
                vec![serde_json::json!(meta)],
            ).await {
                Ok(result) => {
                    // Result is typically [8] or ["8"]
                    let dec = match result.get(0) {
                        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(5) as u8,
                        Some(serde_json::Value::String(s)) => s.parse::<u8>().unwrap_or(5),
                        _ => {
                            // Try parsing result directly
                            match &result {
                                serde_json::Value::Number(n) => n.as_u64().unwrap_or(5) as u8,
                                serde_json::Value::String(s) => s.parse::<u8>().unwrap_or(5),
                                _ => 8
                            }
                        }
                    };
                    eprintln!("  [decimals] {} = {} (from chain, meta={})", sym, dec, meta);
                    decimals.insert(sym.to_string(), dec);
                }
                Err(e) => {
                    eprintln!("  [decimals] Failed to query decimals for {} (meta={}): {}", sym, meta, e);
                    eprintln!("  [decimals] Falling back to default 5 for {}", sym);
                    decimals.insert(sym.to_string(), 5);
                }
            }
        }
    }

    decimals
}

/// Fetch escrow balances from chain for all configured markets.
/// Returns token symbol → human-readable balance string (e.g. "10000.0").
async fn fetch_escrow_balances(
    chain: &deadmkt_chain::client::SupraClient,
    nft_id: u64,
    markets: &[String],
    token_decimals: &HashMap<String, u8>,
) -> HashMap<String, String> {
    let mut balances = HashMap::new();

    for market in markets {
        // Query market pair metadata addresses: get_market_pair(symbol) → (base_meta, quote_meta)
        let symbol_hex = format!("0x{}", hex::encode(market.as_bytes()));
        eprintln!("  [escrow] Querying market pair {} (hex: {})", market, symbol_hex);
        match chain.view_raw(
            "settlement",
            "get_market_pair",
            vec![],
            vec![serde_json::json!(symbol_hex)],
        ).await {
            Ok(pair_result) => {
                eprintln!("  [escrow] Market pair response: {}", pair_result);
                // Result is [{ base_metadata, quote_metadata, ... }]
                let pair_obj = pair_result.get(0).unwrap_or(&pair_result);
                let base_meta = pair_obj.get("base_metadata").and_then(|v| v.as_str()).unwrap_or("");
                let quote_meta = pair_obj.get("quote_metadata").and_then(|v| v.as_str()).unwrap_or("");
                eprintln!("  [escrow] base_meta={}, quote_meta={}", base_meta, quote_meta);

                let parts: Vec<&str> = market.split('/').collect();
                if parts.len() != 2 { continue; }
                let (base_sym, quote_sym) = (parts[0], parts[1]);

                // Query escrow balance for each token
                for (sym, meta) in [(base_sym, base_meta), (quote_sym, quote_meta)] {
                    if meta.is_empty() {
                        eprintln!("  [escrow] Skipping {} — empty metadata", sym);
                        continue;
                    }
                    eprintln!("  [escrow] Querying balance for {} (nft={}, meta={})", sym, nft_id, meta);
                    match chain.view_raw(
                        "escrow",
                        "get_balance",
                        vec![],
                        vec![
                            serde_json::json!(nft_id.to_string()),
                            serde_json::json!(meta),
                        ],
                    ).await {
                        Ok(bal_result) => {
                            eprintln!("  [escrow] Balance response for {}: {}", sym, bal_result);
                            let raw_str = match bal_result.get(0) {
                                Some(serde_json::Value::String(s)) => s.clone(),
                                Some(v) => v.to_string().trim_matches('"').to_string(),
                                None => "0".to_string(),
                            };

                            if let Ok(raw_val) = raw_str.parse::<u64>() {
                                let dec = token_decimals.get(sym).copied().unwrap_or(5);
                                let divisor = 10u64.pow(dec as u32) as f64;
                                let human = raw_val as f64 / divisor;
                                balances.insert(sym.to_string(), format!("{}", human));
                            } else {
                                eprintln!("  [escrow] Failed to parse balance '{}' for {}", raw_str, sym);
                            }
                        }
                        Err(e) => {
                            eprintln!("  [escrow] Failed to query balance for {}: {}", sym, e);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("  [escrow] Failed to query market pair {}: {}", market, e);
            }
        }
    }

    balances
}

// =========================================================================
// Trippples token state fetchers
// =========================================================================

/// Fetch global mint state + per-trustee pending mint from tokens module.
async fn fetch_mint_state(
    chain: &deadmkt_chain::client::SupraClient,
    trustee_address: &str,
) -> deadmkt_strategy::MintStateData {
    let default = deadmkt_strategy::MintStateData {
        state: "UNKNOWN".to_string(),
        hold_duration_secs: 0, period_end: 0, block_end: 0,
        has_pending_mint: false, pending_claimable_at: 0,
    };

    // tokens::get_mint_state() -> (state, period_end, hold_duration, block_end, rotation_index)
    let global = match chain.view_raw("tokens", "get_mint_state", vec![], vec![]).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  [mint_state] Failed to fetch: {}", e);
            return default;
        }
    };

    let state_u8 = global.get(0).and_then(|v| v.as_str()).and_then(|s| s.parse::<u8>().ok()).unwrap_or(0);
    let state_str = match state_u8 {
        0 => "AWAITING_TRIGGER",
        1 => "VDRF_PENDING",
        2 => "OPEN",
        3 => "BLOCKED",
        _ => "UNKNOWN",
    }.to_string();
    let period_end = global.get(1).and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let hold_duration = global.get(2).and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let block_end = global.get(3).and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);

    // tokens::get_nft_mint_state(addr) -> (first_mint_completed, has_pending)
    let (has_pending, claimable_at) = match chain.view_raw(
        "tokens", "get_nft_mint_state", vec![],
        vec![serde_json::json!(trustee_address)],
    ).await {
        Ok(r) => {
            let pending = r.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
            let claimable = if pending {
                // Fetch claimable_at from get_pending_mint(addr)
                // Returns Option<PendingMint> → JSON may be struct or empty
                match chain.view_raw(
                    "tokens", "get_pending_mint", vec![],
                    vec![serde_json::json!(trustee_address)],
                ).await {
                    Ok(pm) => {
                        // Try to extract claimable_at from the struct
                        // Supra view returns it as a nested object or array
                        pm.get(0)
                            .and_then(|v| v.get("claimable_at"))
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse::<u64>().ok())
                            .or_else(|| {
                                pm.get(0)
                                    .and_then(|v| v.get(4)) // 5th field: claimable_at
                                    .and_then(|v| v.as_str())
                                    .and_then(|s| s.parse::<u64>().ok())
                            })
                            .unwrap_or(0)
                    }
                    Err(_) => 0,
                }
            } else { 0 };
            (pending, claimable)
        }
        Err(_) => (false, 0),
    };

    deadmkt_strategy::MintStateData {
        state: state_str,
        hold_duration_secs: hold_duration,
        period_end,
        block_end,
        has_pending_mint: has_pending,
        pending_claimable_at: claimable_at,
    }
}

/// Fetch wallet balances for EMM, KAY, TEE using primary_fungible_store::balance.
async fn fetch_wallet_balances(
    chain: &deadmkt_chain::client::SupraClient,
    trustee_address: &str,
    contract_addr: &str,
    token_decimals: &HashMap<String, u8>,
) -> HashMap<String, String> {
    let mut balances = HashMap::new();

    // Deterministic metadata addresses: sha3_256(contract_addr || seed || 0xFE)
    let contract_bytes = hex::decode(contract_addr.trim_start_matches("0x")).unwrap_or_default();
    let seeds: &[(&str, &[u8])] = &[
        ("EMM", b"deadmkt_emm"),
        ("KAY", b"deadmkt_kay"),
        ("TEE", b"deadmkt_tee"),
    ];

    for (name, seed) in seeds {
        use sha3::{Sha3_256, Digest};
        let mut hasher = Sha3_256::new();
        hasher.update(&contract_bytes);
        hasher.update(*seed);
        hasher.update(&[0xFE]);
        let hash = hasher.finalize();
        let meta_addr = format!("0x{}", hex::encode(hash));

        match chain.view_absolute(
            "0x1::primary_fungible_store::balance",
            vec!["0x1::fungible_asset::Metadata".to_string()],
            vec![
                serde_json::json!(trustee_address),
                serde_json::json!(meta_addr),
            ],
        ).await {
            Ok(r) => {
                let raw_str = match r.get(0) {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(v) => v.to_string().trim_matches('"').to_string(),
                    None => "0".to_string(),
                };
                if let Ok(raw_val) = raw_str.parse::<u64>() {
                    let dec = token_decimals.get(*name).copied().unwrap_or(5);
                    let divisor = 10u64.pow(dec as u32) as f64;
                    let human = raw_val as f64 / divisor;
                    balances.insert(name.to_string(), format!("{:.05}", human));
                }
            }
            Err(e) => {
                eprintln!("  [wallet] Failed to fetch {} balance: {}", name, e);
                balances.insert(name.to_string(), "0.00000".to_string());
            }
        }
    }
    balances
}

/// Fetch circulating supply for EMM, KAY, TEE from tokens module.
async fn fetch_circulating_supply(
    chain: &deadmkt_chain::client::SupraClient,
    token_decimals: &HashMap<String, u8>,
) -> HashMap<String, String> {
    let mut supply = HashMap::new();

    // tokens::get_circulating_supply() -> (m, k, t)
    match chain.view_raw("tokens", "get_circulating_supply", vec![], vec![]).await {
        Ok(r) => {
            let names = ["EMM", "KAY", "TEE"];
            for (i, name) in names.iter().enumerate() {
                let raw_str = match r.get(i) {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(v) => v.to_string().trim_matches('"').to_string(),
                    None => "0".to_string(),
                };
                if let Ok(raw_val) = raw_str.parse::<u64>() {
                    let dec = token_decimals.get(*name).copied().unwrap_or(5);
                    let divisor = 10u64.pow(dec as u32) as f64;
                    let human = raw_val as f64 / divisor;
                    supply.insert(name.to_string(), format!("{:.05}", human));
                }
            }
        }
        Err(e) => {
            eprintln!("  [circulating] Failed to fetch: {}", e);
        }
    }
    supply
}

/// Fetch vault locks for the trustee from tokens module.
async fn fetch_vault_locks(
    chain: &deadmkt_chain::client::SupraClient,
    trustee_address: &str,
    token_decimals: &HashMap<String, u8>,
) -> Vec<deadmkt_strategy::VaultLockData> {
    let mut locks = Vec::new();
    let symbol_names = ["EMM", "KAY", "TEE"];

    // tokens::get_vault_locks(addr) -> vector<TokenLock>
    // TokenLock { symbol: u8, amount: u64, minimum_unlock_at: u64, claimed: bool }
    match chain.view_raw(
        "tokens", "get_vault_locks", vec![],
        vec![serde_json::json!(trustee_address)],
    ).await {
        Ok(r) => {
            // Result may be an array of structs
            if let Some(arr) = r.as_array() {
                for lock in arr {
                    let sym_id = lock.get("symbol")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<u8>().ok())
                        .or_else(|| lock.get(0).and_then(|v| v.as_str()).and_then(|s| s.parse::<u8>().ok()))
                        .unwrap_or(0);
                    let amount_raw = lock.get("amount")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<u64>().ok())
                        .or_else(|| lock.get(1).and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok()))
                        .unwrap_or(0);
                    let unlock_at = lock.get("minimum_unlock_at")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<u64>().ok())
                        .or_else(|| lock.get(2).and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok()))
                        .unwrap_or(0);
                    let claimed = lock.get("claimed")
                        .and_then(|v| v.as_bool())
                        .or_else(|| lock.get(3).and_then(|v| v.as_bool()))
                        .unwrap_or(false);

                    let sym_name = symbol_names.get(sym_id as usize).unwrap_or(&"UNK");
                    let dec = token_decimals.get(*sym_name).copied().unwrap_or(5);
                    let divisor = 10u64.pow(dec as u32) as f64;
                    let human = amount_raw as f64 / divisor;

                    locks.push(deadmkt_strategy::VaultLockData {
                        symbol: sym_name.to_string(),
                        amount: format!("{:.05}", human),
                        unlock_at,
                        claimed,
                    });
                }
            }
        }
        Err(e) => {
            eprintln!("  [vault_locks] Failed to fetch: {}", e);
        }
    }
    locks
}

/// Convert strategy OrderSpecs to ValidatedOrders ready for commit.
///
/// Steps:
/// 1. Build validation context (active pairs, min quantities, balances)
/// 2. Run validate_orders() to get CanonicalOrders
/// 3. For each canonical order: build Order struct, compute commit hash, sign, wrap
fn convert_strategy_orders(
    order_specs: &[deadmkt_strategy::OrderSpec],
    signing_key: &SigningKey,
    nft_id: u64,
    batch_id: u64,
    params: &BatchParams,
    tracker: &Arc<Mutex<EscrowTracker>>,
    token_decimals: &HashMap<String, u8>,
    market_configs: &[MarketConfig],
) -> Vec<ValidatedOrder> {
    // Build validation context from configured markets
    let mut active_pairs = HashSet::new();
    let mut min_quantities = HashMap::new();
    for mc in market_configs {
        let sym = String::from_utf8_lossy(&mc.symbol).to_string();
        active_pairs.insert(sym.clone());
        min_quantities.insert(sym, mc.min_quantity);
    }

    // Read projected balances from escrow tracker
    let projected_balances = {
        let t = tracker.lock().unwrap();
        let mut balances = HashMap::new();
        for token in t.tracked_tokens() {
            balances.insert(token.clone(), t.projected(&token));
        }
        balances
    };

    if projected_balances.is_empty() {
        eprintln!("[commit] WARNING: escrow tracker has no tracked tokens, orders may be rejected");
    }

    let ctx = OrderValidationContext {
        active_pairs,
        min_quantities,
        token_decimals: token_decimals.clone(),
        projected_balances,
        commits_per_batch: params.commits_per_batch as u32,
        nft_id,
        batch_id,
    };

    // Generate random nonces
    let nonce_fn = || {
        let mut nonce = vec![0u8; 32];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut nonce);
        nonce
    };

    let result = validate_orders(order_specs, &ctx, nonce_fn);

    for w in &result.warnings {
        eprintln!("[commit] validation warning: {}", w);
    }

    // Convert CanonicalOrders → ValidatedOrders
    result.orders.iter().map(|canonical| {
        let order = Order {
            nft_id: canonical.nft_id,
            symbol: canonical.symbol.clone(),
            side: canonical.side,
            price: canonical.price,
            quantity: canonical.quantity,
            batch_id: canonical.batch_id,
            nonce: canonical.nonce.clone(),
        };

        let hash = commit_hash(&order);
        let order_bytes = encode_order(&order).expect("BCS encoding should not fail");
        let signature = sign_order(signing_key, &order);

        ValidatedOrder {
            commit_hash: hash,
            order_bytes,
            signature,
        }
    }).collect()
}
