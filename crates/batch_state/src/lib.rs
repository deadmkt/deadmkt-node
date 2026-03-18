// deadmkt-batch-state: Per-batch state machine and phase-driven coordination.
//
// Manages the lifecycle of a single batch: collecting commits during COMMIT,
// processing reveals during REVEAL, running matching during MATCH transition,
// and finalizing during SWAP.
//
// This crate does NOT perform gossip validation (that's gossip_validation).
// It receives already-validated data and manages state transitions.

use std::collections::HashMap;

use deadmkt_chain::batch::Phase;
use deadmkt_chain::types::BatchParams;
use deadmkt_crypto::{sign_commit, sign_order, Order};
use deadmkt_gossip_validation::CommitQuotaTracker;
use deadmkt_matching::{
    compute_order_hash, run_matching, MarketConfig, Match, MatchingInput, RevealedOrder,
};
use ed25519_dalek::SigningKey;

// =========================================================================
// Error types
// =========================================================================

#[derive(Debug, PartialEq)]
pub enum BatchStateError {
    /// Attempted to add a commit outside COMMIT phase.
    WrongPhaseForCommit,
    /// Attempted to add a reveal outside REVEAL phase.
    WrongPhaseForReveal,
    /// Reveal hash doesn't match any pending commit.
    NoMatchingCommit,
    /// Matching already ran for this batch.
    AlreadyMatched,
}

// =========================================================================
// Stored commit record
// =========================================================================

#[derive(Clone, Debug)]
pub struct CommitRecord {
    pub batch_id: u64,
    pub pool_id: u64,
    pub hash: [u8; 32],
    pub nft_id: u64,
    pub signature: Vec<u8>,
    pub revealed: bool,
}

// =========================================================================
// Own commit record (our orders)
// =========================================================================

#[derive(Clone, Debug)]
pub struct OwnCommit {
    pub order: Order,
    pub order_signature: Vec<u8>,
    pub commit_signature: Vec<u8>,
    pub order_hash: [u8; 32],
    pub revealed: bool,
}

// =========================================================================
// BatchState
// =========================================================================

pub struct BatchState {
    pub batch_id: u64,
    pub pool_id: u64,
    pub phase: Phase,
    pub params: BatchParams,

    /// Pending commits: hash → record. Populated during COMMIT phase.
    pub commits: HashMap<[u8; 32], CommitRecord>,

    /// Revealed orders: hash → RevealedOrder. Populated during REVEAL phase.
    pub reveals: HashMap<[u8; 32], RevealedOrder>,

    /// Computed matches. Populated during MATCH phase transition.
    pub matches: Vec<Match>,

    /// Our own pending commits (orders we submitted).
    pub my_commits: Vec<OwnCommit>,

    /// Per-nft commit counting for quota enforcement.
    pub quota_tracker: CommitQuotaTracker,

    /// Market configs: symbol → config. Loaded at batch start.
    pub market_configs: HashMap<Vec<u8>, MarketConfig>,

    /// Whether matching has already been run for this batch.
    matched: bool,
}

impl BatchState {
    /// Create a new batch state for the given batch.
    pub fn new(
        batch_id: u64,
        pool_id: u64,
        phase: Phase,
        params: BatchParams,
        market_configs: Vec<MarketConfig>,
    ) -> Self {
        let commits_per_batch = params.commits_per_batch;
        let mc_map = market_configs
            .into_iter()
            .map(|m| (m.symbol.clone(), m))
            .collect();

        Self {
            batch_id,
            pool_id,
            phase,
            params,
            commits: HashMap::new(),
            reveals: HashMap::new(),
            matches: Vec::new(),
            my_commits: Vec::new(),
            quota_tracker: CommitQuotaTracker::new(commits_per_batch),
            market_configs: mc_map,
            matched: false,
        }
    }

    // =====================================================================
    // Commit handling
    // =====================================================================

    /// Add a validated commit to the batch state.
    /// Must be in COMMIT phase.
    pub fn add_commit(
        &mut self,
        batch_id: u64,
        pool_id: u64,
        hash: [u8; 32],
        nft_id: u64,
        signature: Vec<u8>,
    ) -> Result<(), BatchStateError> {
        if self.phase != Phase::Commit {
            return Err(BatchStateError::WrongPhaseForCommit);
        }

        self.commits.insert(hash, CommitRecord {
            batch_id,
            pool_id,
            hash,
            nft_id,
            signature,
            revealed: false,
        });

        Ok(())
    }

    // =====================================================================
    // Reveal handling
    // =====================================================================

    /// Add a validated reveal to the batch state.
    /// Must be in REVEAL phase.
    pub fn add_reveal(
        &mut self,
        order: Order,
        signature: Vec<u8>,
    ) -> Result<(), BatchStateError> {
        if self.phase != Phase::Reveal {
            return Err(BatchStateError::WrongPhaseForReveal);
        }

        // Compute hash
        let order_hash = compute_order_hash(&order);

        // Look up in commits
        let commit = match self.commits.get_mut(&order_hash) {
            Some(c) => c,
            None => return Err(BatchStateError::NoMatchingCommit),
        };
        commit.revealed = true;

        // Store as revealed order
        let revealed = RevealedOrder {
            order,
            signature,
            order_hash,
        };
        self.reveals.insert(order_hash, revealed);

        Ok(())
    }

    // =====================================================================
    // Phase transitions
    // =====================================================================

    /// Transition to a new phase. Runs matching if transitioning to MATCH.
    pub fn transition_to(&mut self, new_phase: Phase) -> Result<(), BatchStateError> {
        self.phase = new_phase;

        if new_phase == Phase::Match && !self.matched {
            self.run_matching();
        }

        Ok(())
    }

    /// Run the deterministic matching engine on all revealed orders.
    fn run_matching(&mut self) {
        if self.matched {
            return;
        }

        let reveals: Vec<RevealedOrder> = self.reveals.values().cloned().collect();
        let markets: Vec<MarketConfig> = self.market_configs.values().cloned().collect();

        let input = MatchingInput {
            batch_id: self.batch_id,
            pool_id: self.pool_id,
            reveals,
            markets,
        };

        self.matches = run_matching(&input);
        self.matched = true;
    }

    // =====================================================================
    // Own order management
    // =====================================================================

    /// Create and store our own order commit.
    /// Signs the order, computes hash, signs the commit.
    pub fn create_own_commit(
        &mut self,
        secret: &SigningKey,
        order: Order,
    ) -> OwnCommit {
        let order_signature = sign_order(secret, &order).to_vec();
        let order_hash = compute_order_hash(&order);
        let commit_signature = sign_commit(
            secret,
            self.batch_id,
            self.pool_id,
            &order_hash,
        ).to_vec();

        let own = OwnCommit {
            order,
            order_signature,
            commit_signature,
            order_hash,
            revealed: false,
        };

        self.my_commits.push(own.clone());
        own
    }

    /// Mark own commits as revealed (during REVEAL phase broadcast).
    pub fn mark_own_revealed(&mut self, order_hash: &[u8; 32]) {
        for c in &mut self.my_commits {
            if c.order_hash == *order_hash {
                c.revealed = true;
            }
        }
    }

    /// Get own commits that haven't been revealed yet.
    pub fn unrevealed_own_commits(&self) -> Vec<&OwnCommit> {
        self.my_commits.iter().filter(|c| !c.revealed).collect()
    }

    // =====================================================================
    // Batch rollover
    // =====================================================================

    /// Reset state for a new batch. Clears all commits, reveals, matches.
    pub fn reset_for_new_batch(
        &mut self,
        batch_id: u64,
        pool_id: u64,
        phase: Phase,
        market_configs: Vec<MarketConfig>,
    ) {
        self.batch_id = batch_id;
        self.pool_id = pool_id;
        self.phase = phase;
        self.commits.clear();
        self.reveals.clear();
        self.matches.clear();
        self.my_commits.clear();
        self.quota_tracker.reset();
        self.matched = false;
        self.market_configs = market_configs
            .into_iter()
            .map(|m| (m.symbol.clone(), m))
            .collect();
    }

    // =====================================================================
    // Accessors
    // =====================================================================

    pub fn commit_count(&self) -> usize {
        self.commits.len()
    }

    pub fn reveal_count(&self) -> usize {
        self.reveals.len()
    }

    pub fn match_count(&self) -> usize {
        self.matches.len()
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use deadmkt_crypto::generate_keypair;

    // -- Helpers --

    fn make_params() -> BatchParams {
        BatchParams {
            blocks_per_batch: 10,
            commit_blocks: 4,
            reveal_blocks: 3,
            match_blocks: 1,
            swap_blocks: 2,
            commits_per_batch: 3,
        }
    }

    fn emm_kay_market() -> MarketConfig {
        MarketConfig {
            symbol: b"EMM/KAY".to_vec(),
            min_quantity: 1,
        }
    }

    fn kay_tee_market() -> MarketConfig {
        MarketConfig {
            symbol: b"KAY/TEE".to_vec(),
            min_quantity: 1,
        }
    }

    fn make_order(nft_id: u64, symbol: &[u8], side: u8, price: u64, quantity: u64) -> Order {
        Order {
            nft_id,
            symbol: symbol.to_vec(),
            side,
            price,
            quantity,
            batch_id: 100,
            nonce: vec![nft_id as u8; 32], // unique per nft
        }
    }

    const BATCH_ID: u64 = 100;
    const POOL_ID: u64 = 3;
    const EMM_KAY: &[u8] = b"EMM/KAY";
    const BUY: u8 = 1;
    const SELL: u8 = 0;

    /// Add a commit to batch state (simulate validated commit).
    fn add_test_commit(state: &mut BatchState, order: &Order) -> [u8; 32] {
        let hash = compute_order_hash(order);
        state
            .add_commit(BATCH_ID, POOL_ID, hash, order.nft_id, vec![0u8; 64])
            .unwrap();
        hash
    }

    /// Add a reveal to batch state (simulate validated reveal).
    fn add_test_reveal(state: &mut BatchState, order: Order) {
        state.add_reveal(order, vec![0u8; 64]).unwrap();
    }

    // -- T_BS_01: New batch initializes empty --

    #[test]
    fn test_bs_01_new_batch_init() {
        let state = BatchState::new(
            BATCH_ID,
            POOL_ID,
            Phase::Commit,
            make_params(),
            vec![emm_kay_market()],
        );

        assert_eq!(state.batch_id, BATCH_ID);
        assert_eq!(state.pool_id, POOL_ID);
        assert_eq!(state.phase, Phase::Commit);
        assert_eq!(state.commit_count(), 0);
        assert_eq!(state.reveal_count(), 0);
        assert_eq!(state.match_count(), 0);
        assert!(state.my_commits.is_empty());
        assert!(state.market_configs.contains_key(EMM_KAY));
    }

    // -- T_BS_02: Add valid commit --

    #[test]
    fn test_bs_02_add_commit() {
        let mut state = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Commit, make_params(), vec![emm_kay_market()],
        );

        let order = make_order(1, EMM_KAY, BUY, 50, 100);
        let hash = add_test_commit(&mut state, &order);

        assert_eq!(state.commit_count(), 1);
        assert!(state.commits.contains_key(&hash));
        assert_eq!(state.commits[&hash].nft_id, 1);
        assert!(!state.commits[&hash].revealed);
    }

    // -- T_BS_03: Add reveal matching commit --

    #[test]
    fn test_bs_03_add_reveal() {
        let mut state = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Commit, make_params(), vec![emm_kay_market()],
        );

        // Add commit in COMMIT phase
        let order = make_order(1, EMM_KAY, BUY, 50, 100);
        let hash = add_test_commit(&mut state, &order);

        // Transition to REVEAL
        state.transition_to(Phase::Reveal).unwrap();

        // Add reveal
        add_test_reveal(&mut state, order);

        assert_eq!(state.reveal_count(), 1);
        assert!(state.commits[&hash].revealed);
        assert!(state.reveals.contains_key(&hash));
    }

    // -- T_BS_04: Transition to MATCH runs matching --

    #[test]
    fn test_bs_04_match_transition() {
        let mut state = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Commit, make_params(), vec![emm_kay_market()],
        );

        // COMMIT: add a crossing pair
        let buy = make_order(1, EMM_KAY, BUY, 50, 100);
        let sell = make_order(2, EMM_KAY, SELL, 48, 100);
        add_test_commit(&mut state, &buy);
        add_test_commit(&mut state, &sell);

        // REVEAL
        state.transition_to(Phase::Reveal).unwrap();
        add_test_reveal(&mut state, buy);
        add_test_reveal(&mut state, sell);
        assert_eq!(state.reveal_count(), 2);

        // MATCH: triggers matching engine
        state.transition_to(Phase::Match).unwrap();

        assert_eq!(state.match_count(), 1);
        assert_eq!(state.matches[0].fill_quantity, 100);
        assert_eq!(state.matches[0].settlement_price, 49); // (50+48)/2
    }

    // -- T_BS_05: Transition to SWAP finalizes --

    #[test]
    fn test_bs_05_swap_finalized() {
        let mut state = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Commit, make_params(), vec![emm_kay_market()],
        );

        let buy = make_order(1, EMM_KAY, BUY, 50, 100);
        let sell = make_order(2, EMM_KAY, SELL, 48, 100);
        add_test_commit(&mut state, &buy);
        add_test_commit(&mut state, &sell);

        state.transition_to(Phase::Reveal).unwrap();
        add_test_reveal(&mut state, buy);
        add_test_reveal(&mut state, sell);

        state.transition_to(Phase::Match).unwrap();
        assert_eq!(state.match_count(), 1);

        // SWAP: matches still available
        state.transition_to(Phase::Swap).unwrap();
        assert_eq!(state.phase, Phase::Swap);
        assert_eq!(state.match_count(), 1);
    }

    // -- T_BS_06: Own commit flow --

    #[test]
    fn test_bs_06_own_commit() {
        let (secret, _public) = generate_keypair();
        let mut state = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Commit, make_params(), vec![emm_kay_market()],
        );

        let order = make_order(42, EMM_KAY, BUY, 50, 100);
        let own = state.create_own_commit(&secret, order.clone());

        assert_eq!(own.order.nft_id, 42);
        assert_eq!(own.order_signature.len(), 64);
        assert_eq!(own.commit_signature.len(), 64);
        assert!(!own.revealed);
        assert_eq!(state.my_commits.len(), 1);

        // Hash matches
        let expected_hash = compute_order_hash(&order);
        assert_eq!(own.order_hash, expected_hash);
    }

    // -- T_BS_07: Own reveal flow --

    #[test]
    fn test_bs_07_own_reveal() {
        let (secret, _public) = generate_keypair();
        let mut state = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Commit, make_params(), vec![emm_kay_market()],
        );

        let order = make_order(42, EMM_KAY, BUY, 50, 100);
        let own = state.create_own_commit(&secret, order);

        // Initially unrevealed
        assert_eq!(state.unrevealed_own_commits().len(), 1);

        // Mark revealed
        state.mark_own_revealed(&own.order_hash);

        assert_eq!(state.unrevealed_own_commits().len(), 0);
        assert!(state.my_commits[0].revealed);
    }

    // -- T_BS_08: Commit during REVEAL → rejected --

    #[test]
    fn test_bs_08_commit_wrong_phase() {
        let mut state = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Reveal, make_params(), vec![emm_kay_market()],
        );

        let result = state.add_commit(BATCH_ID, POOL_ID, [0xCC; 32], 42, vec![0; 64]);
        assert_eq!(result, Err(BatchStateError::WrongPhaseForCommit));
    }

    // -- T_BS_09: Reveal during COMMIT → rejected --

    #[test]
    fn test_bs_09_reveal_wrong_phase() {
        let mut state = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Commit, make_params(), vec![emm_kay_market()],
        );

        let order = make_order(1, EMM_KAY, BUY, 50, 100);
        let result = state.add_reveal(order, vec![0; 64]);
        assert_eq!(result, Err(BatchStateError::WrongPhaseForReveal));
    }

    // -- T_BS_10: Batch rollover --

    #[test]
    fn test_bs_10_batch_rollover() {
        let mut state = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Commit, make_params(), vec![emm_kay_market()],
        );

        // Add some state
        let order = make_order(1, EMM_KAY, BUY, 50, 100);
        add_test_commit(&mut state, &order);
        assert_eq!(state.commit_count(), 1);

        // Rollover to new batch
        state.reset_for_new_batch(
            BATCH_ID + 1,
            POOL_ID + 1,
            Phase::Commit,
            vec![emm_kay_market(), kay_tee_market()],
        );

        assert_eq!(state.batch_id, BATCH_ID + 1);
        assert_eq!(state.pool_id, POOL_ID + 1);
        assert_eq!(state.phase, Phase::Commit);
        assert_eq!(state.commit_count(), 0);
        assert_eq!(state.reveal_count(), 0);
        assert_eq!(state.match_count(), 0);
        assert!(state.my_commits.is_empty());
        assert_eq!(state.market_configs.len(), 2); // EMM/KAY + KAY/TEE
    }

    // -- T_BS_11: Multiple symbols --

    #[test]
    fn test_bs_11_multi_symbol() {
        let mut state = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Commit, make_params(),
            vec![emm_kay_market(), kay_tee_market()],
        );

        // COMMIT phase: orders for both symbols
        let emm_buy = make_order(1, EMM_KAY, BUY, 50, 100);
        let emm_sell = make_order(2, EMM_KAY, SELL, 48, 100);
        let kay_tee = b"KAY/TEE";
        let kay_tee_buy = make_order(3, btc, BUY, 60000, 10);
        let kay_tee_sell = make_order(4, btc, SELL, 59000, 10);

        add_test_commit(&mut state, &emm_buy);
        add_test_commit(&mut state, &emm_sell);
        add_test_commit(&mut state, &kay_tee_buy);
        add_test_commit(&mut state, &kay_tee_sell);

        // REVEAL phase
        state.transition_to(Phase::Reveal).unwrap();
        add_test_reveal(&mut state, emm_buy);
        add_test_reveal(&mut state, emm_sell);
        add_test_reveal(&mut state, kay_tee_buy);
        add_test_reveal(&mut state, kay_tee_sell);

        // MATCH phase
        state.transition_to(Phase::Match).unwrap();

        // 2 matches: one per symbol
        assert_eq!(state.match_count(), 2);

        let emm_kay_match = state.matches.iter().find(|m| m.symbol == EMM_KAY).unwrap();
        let kay_tee_match = state.matches.iter().find(|m| m.symbol == btc).unwrap();

        assert_eq!(emm_kay_match.fill_quantity, 100);
        assert_eq!(emm_kay_match.settlement_price, 49);

        assert_eq!(btc_match.fill_quantity, 10);
        assert_eq!(btc_match.settlement_price, 59500);
    }

    // -- T_BS_12: Commit quota tracking --

    #[test]
    fn test_bs_12_quota_tracking() {
        let mut state = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Commit, make_params(), vec![emm_kay_market()],
        );

        // Add 3 commits (at limit) from same nft
        for i in 0..3u8 {
            let order = Order {
                nft_id: 42,
                symbol: EMM_KAY.to_vec(),
                side: BUY,
                price: 50,
                quantity: 100,
                batch_id: BATCH_ID,
                nonce: vec![i; 32],
            };
            let hash = compute_order_hash(&order);

            // Record in quota tracker (simulating what gossip_validation does)
            let accepted = state.quota_tracker.check_and_record(
                42, BATCH_ID, POOL_ID, hash, vec![0; 64],
            );
            assert!(accepted, "commit {i} should be under quota");
        }

        // 4th commit: over quota
        let order4 = Order {
            nft_id: 42,
            symbol: EMM_KAY.to_vec(),
            side: BUY,
            price: 50,
            quantity: 100,
            batch_id: BATCH_ID,
            nonce: vec![0xFF; 32],
        };
        let hash4 = compute_order_hash(&order4);
        let accepted = state.quota_tracker.check_and_record(
            42, BATCH_ID, POOL_ID, hash4, vec![0; 64],
        );
        assert!(!accepted, "4th commit should be over quota");

        // Evidence is reportable
        let evidence = state.quota_tracker.get_reportable_evidence();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].commit_hashes.len(), 4);

        // Reset clears everything
        state.quota_tracker.reset();
        assert!(state.quota_tracker.get_reportable_evidence().is_empty());
    }
}
