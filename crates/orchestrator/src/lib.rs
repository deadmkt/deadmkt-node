// =========================================================================
// deadmkt-orchestrator: Batch loop coordinator
// =========================================================================
//
// Routes events between: chain poller, gossip, strategy WS, settlement,
// withdrawal, profit, gas monitor, node state machine.
//
// Uses trait abstractions for gossip/batch_state (B3 crates excluded from
// workspace due to libp2p Rust version requirement).

use deadmkt_chain::types::BatchParams;
use deadmkt_chain::types::ChainEvent;
use deadmkt_crypto::order_hash as compute_order_hash;
use deadmkt_matching::{run_matching, MarketConfig, Match as EngineMatch, MatchingInput, RevealedOrder};
use deadmkt_strategy::{
    MarketAddedData, ParamChangeData,
    PausedData, PoolAdjustedData, ResumedData, SettlementData,
    StrategyEvent, WithdrawalRequestedData,
};
use std::time::Duration;

// =========================================================================
// Port traits
// =========================================================================

/// Phase in the batch cycle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    Commit,
    Reveal,
    Match,
    Swap,
}

/// Gossip message types (simplified for trait boundary).
#[derive(Debug, Clone)]
pub struct CommitMessage {
    pub nft_id: u64,
    pub pool_id: u64,
    pub batch_id: u64,
    pub commit_hash: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct RevealMessage {
    pub nft_id: u64,
    pub pool_id: u64,
    pub batch_id: u64,
    pub order_bytes: Vec<u8>,
    pub signature: Vec<u8>,
}

/// Gossip layer abstraction.
pub trait GossipPort: Send + Sync {
    fn publish_commit(&self, pool_id: u64, commit: CommitMessage) -> Result<(), String>;
    fn publish_reveal(&self, pool_id: u64, reveal: RevealMessage) -> Result<(), String>;
    fn received_commits(&self, batch_id: u64) -> Vec<CommitMessage>;
    fn received_reveals(&self, batch_id: u64) -> Vec<RevealMessage>;
}

/// Batch state abstraction.
pub trait BatchStatePort: Send + Sync {
    fn current_phase(&self) -> Phase;
    fn current_batch_id(&self) -> u64;
    fn current_pool_id(&self, nft_id: u64) -> u64;
    fn advance_to_block(&mut self, block_height: u64);
    fn num_pools(&self) -> u64;
    fn set_num_pools(&mut self, n: u64);
    fn reload_params(&mut self, params: BatchParams);
}

// =========================================================================
// Orchestrator
// =========================================================================

/// Main batch loop coordinator.
///
/// Holds references to all subsystems. Phase handlers are called by the
/// outer run loop (not implemented here — that's main.rs / integration).
pub struct Orchestrator<G: GossipPort, B: BatchStatePort> {
    pub gossip: G,
    pub batch_state: B,
    pub node_nft_id: u64,
    pub commits_per_batch: u32,
    pub action_timeout: Duration,
    // Track strategy notifications for testing
    pub sent_events: Vec<StrategyEvent>,
    // Track gossip publishes for testing
    pub published_commits: Vec<CommitMessage>,
    pub published_reveals: Vec<RevealMessage>,
    // Store full order data from commit phase so reveals can use real bytes
    pub committed_orders: Vec<ValidatedOrder>,
    // Track settlement registrations
    pub registered_matches: Vec<MatchRegistration>,
    // Store actual matches from matching engine for settlement
    pub current_matches: Vec<EngineMatch>,
    // Market configs for matching engine
    pub market_configs: Vec<MarketConfig>,
    // Governance tracking
    pub pending_param_change: Option<PendingParamChange>,
    pub pending_pool_adjustment: Option<PendingPoolAdjustment>,
}

#[derive(Debug, Clone)]
pub struct MatchRegistration {
    pub batch_id: u64,
    pub match_count: usize,
}

#[derive(Debug, Clone)]
pub struct PendingParamChange {
    pub effective_at_batch: u64,
    pub param_name: String,
    pub old_value: u64,
    pub new_value: u64,
}

#[derive(Debug, Clone)]
pub struct PendingPoolAdjustment {
    pub effective_at_batch: u64,
    pub new_num_pools: u64,
}

impl<G: GossipPort, B: BatchStatePort> Orchestrator<G, B> {
    pub fn new(
        gossip: G,
        batch_state: B,
        node_nft_id: u64,
        commits_per_batch: u32,
    ) -> Self {
        Self {
            gossip,
            batch_state,
            node_nft_id,
            commits_per_batch,
            action_timeout: Duration::from_secs(5),
            sent_events: Vec::new(),
            published_commits: Vec::new(),
            published_reveals: Vec::new(),
            committed_orders: Vec::new(),
            registered_matches: Vec::new(),
            current_matches: Vec::new(),
            market_configs: Vec::new(),
            pending_param_change: None,
            pending_pool_adjustment: None,
        }
    }

    // ── Phase handlers ───────────────────────────────────────────────

    /// COMMIT phase: ask strategy for orders, convert, sign, publish commits.
    /// Returns number of commits published.
    pub fn on_commit_phase(
        &mut self,
        can_commit: bool,
        strategy_orders: Option<Vec<ValidatedOrder>>,
    ) -> usize {
        if !can_commit {
            return 0;
        }

        let orders = match strategy_orders {
            Some(orders) => orders,
            None => {
                // Strategy timeout — no orders this batch (CD-16).
                self.sent_events.push(StrategyEvent::Disconnected {
                    reason: "commit timeout".to_string(),
                });
                return 0;
            }
        };

        let batch_id = self.batch_state.current_batch_id();
        let pool_id = self.batch_state.current_pool_id(self.node_nft_id);
        let mut count = 0;

        for order in &orders {
            let commit = CommitMessage {
                nft_id: self.node_nft_id,
                pool_id,
                batch_id,
                commit_hash: order.commit_hash.clone(),
            };
            if self.gossip.publish_commit(pool_id, commit.clone()).is_ok() {
                self.published_commits.push(commit);
                self.committed_orders.push(order.clone());
                count += 1;
            }
        }

        count
    }

    /// REVEAL phase: ask strategy which commits to reveal, publish reveals.
    /// If strategy times out, reveal ALL (safe default, CD-16).
    pub fn on_reveal_phase(
        &mut self,
        my_commit_count: usize,
        strategy_response: Option<Vec<u32>>,
    ) -> usize {
        let batch_id = self.batch_state.current_batch_id();
        let pool_id = self.batch_state.current_pool_id(self.node_nft_id);

        // Determine which indices to reveal
        let indices: Vec<u32> = match strategy_response {
            Some(indices) => indices,
            None => {
                // Timeout → reveal ALL (CD-16 safe default)
                (0..my_commit_count as u32).collect()
            }
        };

        let mut count = 0;
        for idx in &indices {
            let i = *idx as usize;
            let (order_bytes, signature) = if i < self.committed_orders.len() {
                (
                    self.committed_orders[i].order_bytes.clone(),
                    self.committed_orders[i].signature.clone(),
                )
            } else {
                // Fallback for indices beyond committed orders
                (vec![*idx as u8], vec![0u8; 64])
            };
            let reveal = RevealMessage {
                nft_id: self.node_nft_id,
                pool_id,
                batch_id,
                order_bytes,
                signature,
            };
            if self.gossip.publish_reveal(pool_id, reveal.clone()).is_ok() {
                self.published_reveals.push(reveal);
                count += 1;
            }
        }

        count
    }

    /// MATCH phase: collect reveals, run matching, store results.
    /// Returns number of matches found.
    pub fn on_match_phase(&mut self) -> usize {
        let batch_id = self.batch_state.current_batch_id();
        let pool_id = self.batch_state.current_pool_id(self.node_nft_id);
        let reveals = self.gossip.received_reveals(batch_id);

        eprintln!("[match] batch={} my_committed={} peer_reveals={}", 
            batch_id, self.committed_orders.len(), reveals.len());

        // Also include our own committed orders as reveals
        let mut all_revealed: Vec<RevealedOrder> = Vec::new();

        // Convert our committed orders to RevealedOrders
        for order_data in &self.committed_orders {
            if let Ok(order) = bcs::from_bytes::<deadmkt_crypto::Order>(&order_data.order_bytes) {
                let hash = compute_order_hash(&order);
                let mut order_hash = [0u8; 32];
                order_hash.copy_from_slice(&hash[..32]);
                all_revealed.push(RevealedOrder {
                    order,
                    signature: order_data.signature.clone(),
                    order_hash,
                });
            }
        }

        // Convert peer reveals to RevealedOrders
        for reveal in &reveals {
            if let Ok(order) = bcs::from_bytes::<deadmkt_crypto::Order>(&reveal.order_bytes) {
                let hash = compute_order_hash(&order);
                let mut order_hash = [0u8; 32];
                order_hash.copy_from_slice(&hash[..32]);
                all_revealed.push(RevealedOrder {
                    order,
                    signature: reveal.signature.clone(),
                    order_hash,
                });
            }
        }

        if all_revealed.is_empty() {
            self.current_matches.clear();
            return 0;
        }

        // Debug: show what's going into matching
        for r in &all_revealed {
            eprintln!("[match]   nft={} side={} price={} qty={} symbol={:?}",
                r.order.nft_id, r.order.side, r.order.price, r.order.quantity,
                String::from_utf8_lossy(&r.order.symbol));
        }

        // Run deterministic matching engine
        let input = MatchingInput {
            batch_id,
            pool_id,
            reveals: all_revealed,
            markets: self.market_configs.clone(),
        };

        let matches = run_matching(&input);
        let match_count = matches.len();

        if match_count > 0 {
            self.registered_matches.push(MatchRegistration {
                batch_id,
                match_count,
            });
        }

        self.current_matches = matches;
        match_count
    }

    /// SWAP phase: return match count for settlement submission.
    /// The caller (run loop) handles actual settlement submission using current_matches.
    pub fn on_swap_phase(&mut self) -> usize {
        self.current_matches.len()
    }

    /// Take the current matches out of the orchestrator for settlement processing.
    /// Returns owned Vec and clears internal state.
    pub fn take_matches(&mut self) -> Vec<EngineMatch> {
        std::mem::take(&mut self.current_matches)
    }

    // ── Chain event routing ──────────────────────────────────────────

    /// Route a chain event to the appropriate handler + strategy notification.
    pub fn on_chain_event(&mut self, event: &ChainEvent) {
        match event {
            // ── Pause / Resume ───────────────────────────────────────
            ChainEvent::PauseQueued { after_batch } => {
                self.sent_events.push(StrategyEvent::Paused {
                    data: PausedData {
                        after_batch: *after_batch,
                    },
                });
            }
            ChainEvent::Unpaused => {
                self.sent_events.push(StrategyEvent::Resumed {
                    data: ResumedData {
                        batch_id: self.batch_state.current_batch_id(),
                    },
                });
            }

            // ── Withdrawal lifecycle ─────────────────────────────────
            ChainEvent::WithdrawalRequested { nft_id, token, amount } => {
                if *nft_id == self.node_nft_id {
                    self.sent_events.push(StrategyEvent::RushedWithdrawalRequested {
                        data: WithdrawalRequestedData {
                            token: token.clone(),
                            amount: amount.to_string(),
                            nft_id: *nft_id,
                        },
                    });
                }
            }

            // ── Governance: Parameter changes ────────────────────────
            ChainEvent::ParameterChangeQueued { effective_at_batch, param_name, old_value, new_value } => {
                self.pending_param_change = Some(PendingParamChange {
                    effective_at_batch: *effective_at_batch,
                    param_name: param_name.clone(),
                    old_value: *old_value,
                    new_value: *new_value,
                });
                self.sent_events.push(StrategyEvent::ParamChangeQueued {
                    data: ParamChangeData {
                        param_name: param_name.clone(),
                        old_value: old_value.to_string(),
                        new_value: new_value.to_string(),
                        effective_at_batch: Some(*effective_at_batch),
                    },
                });
            }
            ChainEvent::ParameterChangeApplied { batch_id, param_name, new_value } => {
                // Reload batch params in batch_state
                let mut params = BatchParams {
                    blocks_per_batch: 10,
                    commit_blocks: 3,
                    reveal_blocks: 3,
                    match_blocks: 2,
                    swap_blocks: 2,
                    commits_per_batch: 3,
                };
                // Apply the specific parameter
                match param_name.as_str() {
                    "blocks_per_batch" => params.blocks_per_batch = *new_value,
                    "commits_per_batch" => params.commits_per_batch = *new_value,
                    _ => {}
                }
                self.batch_state.reload_params(params);
                self.pending_param_change = None;

                self.sent_events.push(StrategyEvent::ParamChangeApplied {
                    data: ParamChangeData {
                        param_name: param_name.clone(),
                        old_value: String::new(), // not available in Applied event
                        new_value: new_value.to_string(),
                        effective_at_batch: Some(*batch_id),
                    },
                });
            }

            // ── Governance: Pool adjustments ─────────────────────────
            ChainEvent::PoolAdjustmentQueued { effective_at_batch, new_num_pools } => {
                self.pending_pool_adjustment = Some(PendingPoolAdjustment {
                    effective_at_batch: *effective_at_batch,
                    new_num_pools: *new_num_pools,
                });
            }
            ChainEvent::PoolAdjusted { batch_id, new_num_pools } => {
                let old_pools = self.batch_state.num_pools();
                self.batch_state.set_num_pools(*new_num_pools);
                self.pending_pool_adjustment = None;

                self.sent_events.push(StrategyEvent::PoolAdjusted {
                    data: PoolAdjustedData {
                        old_num_pools: old_pools,
                        new_num_pools: *new_num_pools,
                        effective_at_batch: Some(*batch_id),
                    },
                });
            }
            ChainEvent::PoolAdjustmentCancelled => {
                self.pending_pool_adjustment = None;
            }

            // ── Settlement confirmed ─────────────────────────────────
            ChainEvent::BatchTradeSettled(evt) => {
                self.on_settlement_confirmed(evt);
            }

            // ── Market events ────────────────────────────────────────
            ChainEvent::MarketPairAdded { symbol } => {
                let sym_str = String::from_utf8_lossy(symbol).to_string();
                self.sent_events.push(StrategyEvent::MarketAdded {
                    data: MarketAddedData {
                        symbol: sym_str,
                        min_quantity: "0".to_string(), // full data from chain read
                    },
                });
            }

            _ => {
                // Other events: HoldingPeriod, NftBlocked, etc.
                // Handled by withdrawal_handler / node_state in real impl.
            }
        }
    }

    /// Handle a confirmed settlement event.
    fn on_settlement_confirmed(
        &mut self,
        evt: &deadmkt_chain::types::BatchTradeSettledEvent,
    ) {
        // In real impl: update escrow tracker, check profit, etc.
        // Here: notify strategy.
        let is_buyer = evt.buyer_nft_id == self.node_nft_id;
        let (side, price) = if is_buyer {
            ("buy".to_string(), evt.clearing_price.to_string())
        } else {
            ("sell".to_string(), evt.clearing_price.to_string())
        };

        self.sent_events.push(StrategyEvent::Settlement {
            data: SettlementData {
                batch_id: evt.batch_id,
                match_hash: evt.trade_id.clone(),
                status: "confirmed".to_string(),
                pair: String::from_utf8_lossy(&evt.symbol).to_string(),
                side,
                price,
                quantity: evt.base_amount.to_string(),
            },
        });
    }
}

/// Validated order ready for commit (output of strategy conversion + validation).
#[derive(Debug, Clone)]
pub struct ValidatedOrder {
    pub commit_hash: Vec<u8>,
    pub order_bytes: Vec<u8>,
    pub signature: Vec<u8>,
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use deadmkt_chain::types::{BatchParams, BatchTradeSettledEvent};
    use std::sync::Mutex;

    // ── Mock GossipPort ──────────────────────────────────────────────

    struct MockGossip {
        commits: Mutex<Vec<CommitMessage>>,
        reveals: Mutex<Vec<RevealMessage>>,
    }

    impl MockGossip {
        fn new() -> Self {
            Self {
                commits: Mutex::new(Vec::new()),
                reveals: Mutex::new(Vec::new()),
            }
        }

        fn with_reveals(reveals: Vec<RevealMessage>) -> Self {
            Self {
                commits: Mutex::new(Vec::new()),
                reveals: Mutex::new(reveals),
            }
        }
    }

    impl GossipPort for MockGossip {
        fn publish_commit(&self, _pool_id: u64, commit: CommitMessage) -> Result<(), String> {
            self.commits.lock().unwrap().push(commit);
            Ok(())
        }
        fn publish_reveal(&self, _pool_id: u64, reveal: RevealMessage) -> Result<(), String> {
            self.reveals.lock().unwrap().push(reveal);
            Ok(())
        }
        fn received_commits(&self, _batch_id: u64) -> Vec<CommitMessage> {
            self.commits.lock().unwrap().clone()
        }
        fn received_reveals(&self, _batch_id: u64) -> Vec<RevealMessage> {
            self.reveals.lock().unwrap().clone()
        }
    }

    // ── Mock BatchStatePort ──────────────────────────────────────────

    struct MockBatchState {
        phase: Phase,
        batch_id: u64,
        pool_id: u64,
        n_pools: u64,
        params_reloaded: bool,
        last_params: Option<BatchParams>,
    }

    impl MockBatchState {
        fn new(batch_id: u64) -> Self {
            Self {
                phase: Phase::Commit,
                batch_id,
                pool_id: 2,
                n_pools: 4,
                params_reloaded: false,
                last_params: None,
            }
        }
    }

    impl BatchStatePort for MockBatchState {
        fn current_phase(&self) -> Phase { self.phase }
        fn current_batch_id(&self) -> u64 { self.batch_id }
        fn current_pool_id(&self, _nft_id: u64) -> u64 { self.pool_id }
        fn advance_to_block(&mut self, _block_height: u64) {}
        fn num_pools(&self) -> u64 { self.n_pools }
        fn set_num_pools(&mut self, n: u64) { self.n_pools = n; }
        fn reload_params(&mut self, params: BatchParams) {
            self.params_reloaded = true;
            self.last_params = Some(params);
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────

    fn make_orch(batch_id: u64) -> Orchestrator<MockGossip, MockBatchState> {
        Orchestrator::new(
            MockGossip::new(),
            MockBatchState::new(batch_id),
            42, // nft_id
            3,  // commits_per_batch
        )
    }

    fn make_orch_with_reveals(batch_id: u64, n_reveals: usize) -> Orchestrator<MockGossip, MockBatchState> {
        // Build crossing orders: alternating buy/sell at crossing prices
        let mut reveals: Vec<RevealMessage> = Vec::new();
        for i in 0..n_reveals {
            let side = if i % 2 == 0 { 1 } else { 0 }; // alternate BUY/SELL
            let price = if side == 1 { 5_200_000 } else { 4_800_000 };
            let order = deadmkt_crypto::Order {
                nft_id: (100 + i) as u64,
                symbol: b"EMM/KAY".to_vec(),
                side,
                price,
                quantity: 50_000_000_000,
                batch_id,
                nonce: vec![i as u8; 32],
            };
            let order_bytes = deadmkt_crypto::encode_order(&order).unwrap();
            reveals.push(RevealMessage {
                nft_id: (100 + i) as u64,
                pool_id: 2,
                batch_id,
                order_bytes,
                signature: vec![0u8; 64],
            });
        }

        let mut orch = Orchestrator::new(
            MockGossip::with_reveals(reveals),
            MockBatchState::new(batch_id),
            42,
            3,
        );
        orch.market_configs = vec![
            deadmkt_matching::MarketConfig { symbol: b"EMM/KAY".to_vec(), min_quantity: 1 },
        ];
        orch
    }

    fn make_orders(n: usize) -> Vec<ValidatedOrder> {
        (0..n).map(|i| ValidatedOrder {
            commit_hash: vec![i as u8; 32],
            order_bytes: vec![i as u8; 64],
            signature: vec![i as u8; 64],
        }).collect()
    }

    // ── T_ORCH_01: Commit phase — 2 valid orders → 2 commits published

    #[test]
    fn t_orch_01_commit_phase_success() {
        let mut orch = make_orch(100);
        let count = orch.on_commit_phase(true, Some(make_orders(2)));
        assert_eq!(count, 2);
        assert_eq!(orch.published_commits.len(), 2);
        assert_eq!(orch.published_commits[0].batch_id, 100);
        assert_eq!(orch.published_commits[0].nft_id, 42);
    }

    // ── T_ORCH_02: Commit phase — strategy timeout → 0 commits

    #[test]
    fn t_orch_02_commit_phase_timeout() {
        let mut orch = make_orch(100);
        let count = orch.on_commit_phase(true, None);
        assert_eq!(count, 0);
        assert!(orch.published_commits.is_empty());
    }

    // ── T_ORCH_03: Commit phase — gas critical → skip

    #[test]
    fn t_orch_03_commit_phase_gas_critical() {
        let mut orch = make_orch(100);
        let count = orch.on_commit_phase(false, Some(make_orders(2)));
        assert_eq!(count, 0);
        assert!(orch.published_commits.is_empty());
    }

    // ── T_ORCH_04: Reveal phase — strategy returns [0,1] → 2 reveals

    #[test]
    fn t_orch_04_reveal_phase_selective() {
        let mut orch = make_orch(100);
        let count = orch.on_reveal_phase(3, Some(vec![0, 1]));
        assert_eq!(count, 2);
        assert_eq!(orch.published_reveals.len(), 2);
    }

    // ── T_ORCH_05: Reveal phase — timeout → reveal ALL (CD-16)

    #[test]
    fn t_orch_05_reveal_phase_timeout_reveals_all() {
        let mut orch = make_orch(100);
        let count = orch.on_reveal_phase(3, None);
        assert_eq!(count, 3); // all 3 commits revealed
        assert_eq!(orch.published_reveals.len(), 3);
    }

    // ── T_ORCH_06: Match phase — 4 reveals → 2 matches

    #[test]
    fn t_orch_06_match_phase() {
        let mut orch = make_orch_with_reveals(100, 4);
        let match_count = orch.on_match_phase();
        assert_eq!(match_count, 2); // 4 reveals / 2 = 2 matches
        assert_eq!(orch.registered_matches.len(), 1);
        assert_eq!(orch.registered_matches[0].match_count, 2);
    }

    // ── T_ORCH_07: Swap phase — matches registered → settlements

    #[test]
    fn t_orch_07_swap_phase() {
        let mut orch = make_orch_with_reveals(100, 4);
        orch.on_match_phase(); // register 2 matches
        let settled = orch.on_swap_phase();
        assert_eq!(settled, 2);
    }

    // ── T_ORCH_08: PauseDetected → strategy notified

    #[test]
    fn t_orch_08_pause_detected() {
        let mut orch = make_orch(100);
        orch.on_chain_event(&ChainEvent::PauseQueued { after_batch: 105 });

        assert_eq!(orch.sent_events.len(), 1);
        match &orch.sent_events[0] {
            StrategyEvent::Paused { data } => {
                assert_eq!(data.after_batch, 105);
            }
            other => panic!("expected Paused, got {:?}", other),
        }
    }

    // ── T_ORCH_09: WithdrawalRequested for own NFT → strategy notified

    #[test]
    fn t_orch_09_withdrawal_own_nft() {
        let mut orch = make_orch(100);
        orch.on_chain_event(&ChainEvent::WithdrawalRequested {
            nft_id: 42, // our nft
            token: "KAY".to_string(),
            amount: 5_000_000,
        });

        assert_eq!(orch.sent_events.len(), 1);
        match &orch.sent_events[0] {
            StrategyEvent::RushedWithdrawalRequested { data } => {
                assert_eq!(data.token, "KAY");
                assert_eq!(data.nft_id, 42);
            }
            other => panic!("expected RushedWithdrawalRequested, got {:?}", other),
        }

        // Other NFT → no notification
        orch.sent_events.clear();
        orch.on_chain_event(&ChainEvent::WithdrawalRequested {
            nft_id: 99,
            token: "KAY".to_string(),
            amount: 1_000_000,
        });
        assert!(orch.sent_events.is_empty());
    }

    // ── T_ORCH_10: Settlement confirmed → strategy notified

    #[test]
    fn t_orch_10_settlement_confirmed() {
        let mut orch = make_orch(100);
        let evt = BatchTradeSettledEvent {
            batch_id: 100,
            pool_id: 2,
            trade_id: "0xABC".to_string(),
            buyer_nft_id: 42, // our nft
            seller_nft_id: 99,
            symbol: b"EMM/KAY".to_vec(),
            buyer_price: 5_000_000,
            seller_price: 4_900_000,
            clearing_price: 4_950_000,
            base_amount: 1000,
            quote_amount: 49,
            buyer_order_hash: "0xBH".to_string(),
            seller_order_hash: "0xSH".to_string(),
            gas_payer: "0x01".to_string(),
            timestamp: 123,
        };

        orch.on_chain_event(&ChainEvent::BatchTradeSettled(evt));

        assert_eq!(orch.sent_events.len(), 1);
        match &orch.sent_events[0] {
            StrategyEvent::Settlement { data } => {
                assert_eq!(data.batch_id, 100);
                assert_eq!(data.match_hash, "0xABC");
                assert_eq!(data.status, "confirmed");
                assert_eq!(data.side, "buy");
                assert_eq!(data.pair, "EMM/KAY");
            }
            other => panic!("expected Settlement, got {:?}", other),
        }
    }

    // ── T_ORCH_11: ParameterChangeApplied → reload_params + strategy notify

    #[test]
    fn t_orch_11_parameter_change_applied() {
        let mut orch = make_orch(100);
        orch.on_chain_event(&ChainEvent::ParameterChangeApplied {
            batch_id: 500,
            param_name: "commits_per_batch".to_string(),
            new_value: 5,
        });

        // batch_state.reload_params() was called
        assert!(orch.batch_state.params_reloaded);
        assert_eq!(
            orch.batch_state.last_params.as_ref().unwrap().commits_per_batch,
            5
        );

        // Strategy notified
        assert_eq!(orch.sent_events.len(), 1);
        match &orch.sent_events[0] {
            StrategyEvent::ParamChangeApplied { data } => {
                assert_eq!(data.param_name, "commits_per_batch");
                assert_eq!(data.new_value, "5");
            }
            other => panic!("expected ParamChangeApplied, got {:?}", other),
        }
    }

    // ── T_ORCH_12: PoolAdjusted → set_num_pools + strategy notify

    #[test]
    fn t_orch_12_pool_adjusted() {
        let mut orch = make_orch(100);
        assert_eq!(orch.batch_state.num_pools(), 4); // initial

        orch.on_chain_event(&ChainEvent::PoolAdjusted {
            batch_id: 600,
            new_num_pools: 8,
        });

        assert_eq!(orch.batch_state.num_pools(), 8);

        assert_eq!(orch.sent_events.len(), 1);
        match &orch.sent_events[0] {
            StrategyEvent::PoolAdjusted { data } => {
                assert_eq!(data.old_num_pools, 4);
                assert_eq!(data.new_num_pools, 8);
                assert_eq!(data.effective_at_batch, Some(600));
            }
            other => panic!("expected PoolAdjusted, got {:?}", other),
        }
    }

    // ── Extra: PoolAdjustmentCancelled clears pending

    #[test]
    fn test_pool_adjustment_cancelled() {
        let mut orch = make_orch(100);
        orch.on_chain_event(&ChainEvent::PoolAdjustmentQueued {
            effective_at_batch: 700,
            new_num_pools: 16,
        });
        assert!(orch.pending_pool_adjustment.is_some());

        orch.on_chain_event(&ChainEvent::PoolAdjustmentCancelled);
        assert!(orch.pending_pool_adjustment.is_none());
    }
}
