// deadmkt-gossip-validation: Message validation for gossip layer.
//
// Security perimeter: invalid commits/reveals must be dropped silently
// and NEVER forwarded. A single bad message forwarded to mesh poisons
// all downstream matching.
//
// Commit violation evidence is collected but NOT submitted on-chain (B4 scope).

use std::collections::{HashMap, HashSet};

use deadmkt_chain::batch::compute_pool_assignment;
use deadmkt_crypto::{
    batch_complete_message, encode_order, verify,
};
use ed25519_dalek::VerifyingKey;
use sha2::{Digest, Sha256};

// =========================================================================
// Provider traits (mockable for testing)
// =========================================================================

/// Resolve Ed25519 pubkey for an nft_id.
pub trait PubkeyProvider {
    fn get_pubkey(&self, nft_id: u64) -> Option<VerifyingKey>;
}

/// Check if an nft_id is registered as a trader (has Escrow resource).
pub trait RegistrationProvider {
    fn is_registered(&self, nft_id: u64) -> bool;
}

/// Check if an nft_id is in the BlockedRegistry.
pub trait BlockedProvider {
    fn is_blocked(&self, nft_id: u64) -> bool;
}

// =========================================================================
// Validation context
// =========================================================================

/// Immutable context for the current batch.
#[derive(Clone, Debug)]
pub struct ValidationContext {
    pub current_batch_id: u64,
    pub my_pool_id: u64,
    pub num_pools: u64,
    pub commits_per_batch: u64,
}

// =========================================================================
// Commit validation result
// =========================================================================

#[derive(Debug, PartialEq)]
pub enum CommitResult {
    Accepted,
    Rejected(CommitRejectReason),
}

#[derive(Debug, PartialEq)]
pub enum CommitRejectReason {
    WrongBatch,
    WrongPool,
    BadPoolAssignment,
    BlockedNft,
    UnregisteredNft,
    InvalidSignature,
    DuplicateHash,
    OverQuota,
}

// =========================================================================
// Reveal validation result
// =========================================================================

#[derive(Debug, PartialEq)]
pub enum RevealResult {
    Accepted,
    Rejected(RevealRejectReason),
    DuplicateIgnored,
}

#[derive(Debug, PartialEq)]
pub enum RevealRejectReason {
    WrongBatch,
    NoMatchingCommit,
    NftIdMismatch,
    InvalidSignature,
}

// =========================================================================
// BatchComplete validation result
// =========================================================================

#[derive(Debug, PartialEq)]
pub enum BatchCompleteResult {
    Accepted,
    Rejected(BatchCompleteRejectReason),
}

#[derive(Debug, PartialEq)]
pub enum BatchCompleteRejectReason {
    WrongBatch,
    BadPoolAssignment,
    InvalidSignature,
    DuplicateSender,
}

// =========================================================================
// Commit violation evidence (CD-5)
// =========================================================================

/// Evidence of over-quota commits from a single nft_id.
#[derive(Clone, Debug)]
pub struct CommitViolationEvidence {
    pub accused_nft_id: u64,
    pub batch_id: u64,
    pub pool_id: u64,
    pub commit_hashes: Vec<[u8; 32]>,
    pub commit_signatures: Vec<Vec<u8>>,
}

// =========================================================================
// Commit record (stored in pending commits)
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
// Commit quota tracker
// =========================================================================

pub struct CommitQuotaTracker {
    counts: HashMap<u64, u64>,
    evidence: HashMap<u64, CommitViolationEvidence>,
    commits_per_batch: u64,
}

impl CommitQuotaTracker {
    pub fn new(commits_per_batch: u64) -> Self {
        Self {
            counts: HashMap::new(),
            evidence: HashMap::new(),
            commits_per_batch,
        }
    }

    /// Check if nft_id is under quota. If over, stores evidence. Returns true if accepted.
    pub fn check_and_record(
        &mut self,
        nft_id: u64,
        batch_id: u64,
        pool_id: u64,
        hash: [u8; 32],
        signature: Vec<u8>,
    ) -> bool {
        let count = self.counts.entry(nft_id).or_insert(0);

        // Always store in evidence (even accepted commits — needed for proof)
        let evidence = self.evidence.entry(nft_id).or_insert_with(|| {
            CommitViolationEvidence {
                accused_nft_id: nft_id,
                batch_id,
                pool_id,
                commit_hashes: Vec::new(),
                commit_signatures: Vec::new(),
            }
        });
        evidence.commit_hashes.push(hash);
        evidence.commit_signatures.push(signature);

        if *count >= self.commits_per_batch {
            // Over quota — reject but evidence is already stored
            return false;
        }

        *count += 1;
        true
    }

    /// Get evidence for nft_ids that exceeded quota (count > commits_per_batch).
    pub fn get_reportable_evidence(&self) -> Vec<&CommitViolationEvidence> {
        self.evidence
            .values()
            .filter(|e| e.commit_hashes.len() as u64 > self.commits_per_batch)
            .collect()
    }

    /// Reset for new batch.
    pub fn reset(&mut self) {
        self.counts.clear();
        self.evidence.clear();
    }
}

// =========================================================================
// Gossip Validator
// =========================================================================

pub struct GossipValidator<P: PubkeyProvider, R: RegistrationProvider, B: BlockedProvider> {
    pub pubkeys: P,
    pub registration: R,
    pub blocked: B,
    pub context: ValidationContext,
    pub pending_commits: HashMap<[u8; 32], CommitRecord>,
    pub quota_tracker: CommitQuotaTracker,
    pub revealed_hashes: HashSet<[u8; 32]>,
    pub batch_complete_seen: HashSet<(u64, Vec<u8>)>, // (sender_nft_id, symbol)
}

impl<P: PubkeyProvider, R: RegistrationProvider, B: BlockedProvider> GossipValidator<P, R, B> {
    pub fn new(pubkeys: P, registration: R, blocked: B, context: ValidationContext) -> Self {
        let commits_per_batch = context.commits_per_batch;
        Self {
            pubkeys,
            registration,
            blocked,
            context,
            pending_commits: HashMap::new(),
            quota_tracker: CommitQuotaTracker::new(commits_per_batch),
            revealed_hashes: HashSet::new(),
            batch_complete_seen: HashSet::new(),
        }
    }

    /// Validate a Commitment message. If accepted, stores in pending_commits.
    pub fn validate_commit(
        &mut self,
        batch_id: u64,
        pool_id: u64,
        hash: [u8; 32],
        nft_id: u64,
        signature: Vec<u8>,
    ) -> CommitResult {
        // 1. batch_id check
        if batch_id != self.context.current_batch_id {
            return CommitResult::Rejected(CommitRejectReason::WrongBatch);
        }

        // 2. pool_id check
        if pool_id != self.context.my_pool_id {
            return CommitResult::Rejected(CommitRejectReason::WrongPool);
        }

        // 3. Pool assignment verification
        let expected_pool = compute_pool_assignment(
            nft_id,
            batch_id,
            self.context.num_pools,
        );
        if expected_pool != pool_id {
            return CommitResult::Rejected(CommitRejectReason::BadPoolAssignment);
        }

        // 4. Blocked check
        if self.blocked.is_blocked(nft_id) {
            return CommitResult::Rejected(CommitRejectReason::BlockedNft);
        }

        // 5. Registration check (R53)
        if !self.registration.is_registered(nft_id) {
            return CommitResult::Rejected(CommitRejectReason::UnregisteredNft);
        }

        // 6. Signature verification
        let pubkey = match self.pubkeys.get_pubkey(nft_id) {
            Some(pk) => pk,
            None => return CommitResult::Rejected(CommitRejectReason::InvalidSignature),
        };

        let mut msg = Vec::with_capacity(48);
        msg.extend_from_slice(&batch_id.to_le_bytes());
        msg.extend_from_slice(&pool_id.to_le_bytes());
        msg.extend_from_slice(&hash);

        if !verify(&pubkey, &msg, &signature) {
            return CommitResult::Rejected(CommitRejectReason::InvalidSignature);
        }

        // 7. Quota check (stores evidence on over-quota)
        if !self.quota_tracker.check_and_record(
            nft_id,
            batch_id,
            pool_id,
            hash,
            signature.clone(),
        ) {
            return CommitResult::Rejected(CommitRejectReason::OverQuota);
        }

        // 8. Duplicate hash check
        if self.pending_commits.contains_key(&hash) {
            return CommitResult::Rejected(CommitRejectReason::DuplicateHash);
        }

        // Store in pending commits
        self.pending_commits.insert(hash, CommitRecord {
            batch_id,
            pool_id,
            hash,
            nft_id,
            signature,
            revealed: false,
        });

        CommitResult::Accepted
    }

    /// Validate a Reveal message. If accepted, marks the commit as revealed.
    pub fn validate_reveal(
        &mut self,
        batch_id: u64,
        _pool_id: u64,
        order: &deadmkt_crypto::Order,
        signature: &[u8],
    ) -> RevealResult {
        // 1. batch_id check
        if batch_id != self.context.current_batch_id {
            return RevealResult::Rejected(RevealRejectReason::WrongBatch);
        }

        // 2. Compute reveal hash = SHA256(BCS(order))
        let order_bytes = encode_order(order).expect("BCS encoding should not fail");
        let reveal_hash_vec = {
            let mut hasher = Sha256::new();
            hasher.update(&order_bytes);
            hasher.finalize().to_vec()
        };
        let mut reveal_hash = [0u8; 32];
        reveal_hash.copy_from_slice(&reveal_hash_vec);

        // 3. Look up in pending commits
        let commit = match self.pending_commits.get(&reveal_hash) {
            Some(c) => c,
            None => return RevealResult::Rejected(RevealRejectReason::NoMatchingCommit),
        };

        // 6. Already revealed — idempotent
        if self.revealed_hashes.contains(&reveal_hash) {
            return RevealResult::DuplicateIgnored;
        }

        // 4. nft_id match
        if order.nft_id != commit.nft_id {
            return RevealResult::Rejected(RevealRejectReason::NftIdMismatch);
        }

        // 5. Order signature verification
        let pubkey = match self.pubkeys.get_pubkey(order.nft_id) {
            Some(pk) => pk,
            None => return RevealResult::Rejected(RevealRejectReason::InvalidSignature),
        };

        if !verify(&pubkey, &order_bytes, signature) {
            return RevealResult::Rejected(RevealRejectReason::InvalidSignature);
        }

        // Mark as revealed
        self.revealed_hashes.insert(reveal_hash);
        if let Some(commit) = self.pending_commits.get_mut(&reveal_hash) {
            commit.revealed = true;
        }

        RevealResult::Accepted
    }

    /// Validate a BatchComplete message.
    pub fn validate_batch_complete(
        &mut self,
        batch_id: u64,
        pool_id: u64,
        symbol: &[u8],
        avg_settlement_price: u64,
        volume: u64,
        match_count: u64,
        sender_nft_id: u64,
        signature: &[u8],
    ) -> BatchCompleteResult {
        // 1. batch_id check
        if batch_id != self.context.current_batch_id {
            return BatchCompleteResult::Rejected(BatchCompleteRejectReason::WrongBatch);
        }

        // 2. Pool assignment verification
        let expected_pool = compute_pool_assignment(
            sender_nft_id,
            batch_id,
            self.context.num_pools,
        );
        if expected_pool != pool_id {
            return BatchCompleteResult::Rejected(BatchCompleteRejectReason::BadPoolAssignment);
        }

        // 3. Signature verification
        let pubkey = match self.pubkeys.get_pubkey(sender_nft_id) {
            Some(pk) => pk,
            None => return BatchCompleteResult::Rejected(BatchCompleteRejectReason::InvalidSignature),
        };

        let msg = batch_complete_message(
            batch_id,
            pool_id,
            symbol,
            avg_settlement_price,
            volume,
            match_count,
        );

        if !verify(&pubkey, &msg, signature) {
            return BatchCompleteResult::Rejected(BatchCompleteRejectReason::InvalidSignature);
        }

        // 4. Duplicate sender check (per pool+symbol)
        let key = (sender_nft_id, symbol.to_vec());
        if self.batch_complete_seen.contains(&key) {
            return BatchCompleteResult::Rejected(BatchCompleteRejectReason::DuplicateSender);
        }
        self.batch_complete_seen.insert(key);

        BatchCompleteResult::Accepted
    }

    /// Reset all state for a new batch.
    pub fn reset_for_new_batch(&mut self, new_context: ValidationContext) {
        self.context = new_context;
        self.pending_commits.clear();
        self.quota_tracker.reset();
        self.revealed_hashes.clear();
        self.batch_complete_seen.clear();
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use deadmkt_crypto::{generate_keypair, sign_commit, sign_order, sign_batch_complete, Order};

    // -- Mock providers --

    struct MockPubkeys {
        keys: HashMap<u64, VerifyingKey>,
    }

    impl MockPubkeys {
        fn new() -> Self {
            Self { keys: HashMap::new() }
        }
        fn add(&mut self, nft_id: u64, pubkey: VerifyingKey) {
            self.keys.insert(nft_id, pubkey);
        }
    }

    impl PubkeyProvider for MockPubkeys {
        fn get_pubkey(&self, nft_id: u64) -> Option<VerifyingKey> {
            self.keys.get(&nft_id).copied()
        }
    }

    struct MockRegistration {
        registered: HashSet<u64>,
    }

    impl MockRegistration {
        fn new(nfts: &[u64]) -> Self {
            Self { registered: nfts.iter().copied().collect() }
        }
    }

    impl RegistrationProvider for MockRegistration {
        fn is_registered(&self, nft_id: u64) -> bool {
            self.registered.contains(&nft_id)
        }
    }

    struct MockBlocked {
        blocked: HashSet<u64>,
    }

    impl MockBlocked {
        fn new(nfts: &[u64]) -> Self {
            Self { blocked: nfts.iter().copied().collect() }
        }
    }

    impl BlockedProvider for MockBlocked {
        fn is_blocked(&self, nft_id: u64) -> bool {
            self.blocked.contains(&nft_id)
        }
    }

    // -- Test helpers --

    const BATCH_ID: u64 = 100;
    const NUM_POOLS: u64 = 4;

    /// Find a pool_id that compute_pool_assignment(nft_id, batch_id, num_pools) returns.
    fn pool_for(nft_id: u64) -> u64 {
        compute_pool_assignment(nft_id, BATCH_ID, NUM_POOLS)
    }

    fn make_context(pool_id: u64) -> ValidationContext {
        ValidationContext {
            current_batch_id: BATCH_ID,
            my_pool_id: pool_id,
            num_pools: NUM_POOLS,
            commits_per_batch: 3,
        }
    }

    fn make_order(nft_id: u64) -> Order {
        Order {
            nft_id,
            symbol: b"EMM/KAY".to_vec(),
            side: 1,
            price: 5_000_000,
            quantity: 50_000_000_000,
            batch_id: BATCH_ID,
            nonce: vec![0xAA; 32],
        }
    }

    fn make_validator(
        nft_id: u64,
        pubkey: VerifyingKey,
    ) -> GossipValidator<MockPubkeys, MockRegistration, MockBlocked> {
        let pool_id = pool_for(nft_id);
        let mut pubkeys = MockPubkeys::new();
        pubkeys.add(nft_id, pubkey);
        let reg = MockRegistration::new(&[nft_id]);
        let blocked = MockBlocked::new(&[]);
        GossipValidator::new(pubkeys, reg, blocked, make_context(pool_id))
    }

    // -- T_VAL_01: Valid commit accepted --

    #[test]
    fn test_val_01_valid_commit() {
        let (secret, public) = generate_keypair();
        let nft_id = 42;
        let mut v = make_validator(nft_id, public);

        let order = make_order(nft_id);
        let hash_vec = deadmkt_crypto::order_hash(&order);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_vec);

        let sig = sign_commit(&secret, BATCH_ID, pool_for(nft_id), &hash);

        let result = v.validate_commit(BATCH_ID, pool_for(nft_id), hash, nft_id, sig);
        assert_eq!(result, CommitResult::Accepted);
        assert!(v.pending_commits.contains_key(&hash));
    }

    // -- T_VAL_02: Invalid signature --

    #[test]
    fn test_val_02_invalid_signature() {
        let (_secret, public) = generate_keypair();
        let nft_id = 42;
        let mut v = make_validator(nft_id, public);

        let hash = [0xCC; 32];
        let bad_sig = vec![0xFF; 64]; // Not a valid signature

        let result = v.validate_commit(BATCH_ID, pool_for(nft_id), hash, nft_id, bad_sig);
        assert_eq!(result, CommitResult::Rejected(CommitRejectReason::InvalidSignature));
    }

    // -- T_VAL_03: Wrong pool assignment --

    #[test]
    fn test_val_03_wrong_pool_assignment() {
        let (secret, public) = generate_keypair();
        let nft_id = 42;
        let correct_pool = pool_for(nft_id);
        let wrong_pool = (correct_pool + 1) % NUM_POOLS;

        // Validator is on the wrong pool
        let mut pubkeys = MockPubkeys::new();
        pubkeys.add(nft_id, public);
        let reg = MockRegistration::new(&[nft_id]);
        let blocked = MockBlocked::new(&[]);
        let mut v = GossipValidator::new(
            pubkeys, reg, blocked, make_context(wrong_pool),
        );

        let hash = [0xCC; 32];
        let sig = sign_commit(&secret, BATCH_ID, wrong_pool, &hash);

        // Commit claims wrong_pool but nft_id's assignment is correct_pool
        let result = v.validate_commit(BATCH_ID, wrong_pool, hash, nft_id, sig);
        assert_eq!(result, CommitResult::Rejected(CommitRejectReason::BadPoolAssignment));
    }

    // -- T_VAL_04: Over quota --

    #[test]
    fn test_val_04_over_quota() {
        let (secret, public) = generate_keypair();
        let nft_id = 42;
        let mut v = make_validator(nft_id, public);
        let pool_id = pool_for(nft_id);

        // Submit commits_per_batch (3) commits — all accepted
        for i in 0..3 {
            let order = Order {
                nft_id,
                symbol: b"EMM/KAY".to_vec(),
                side: 1,
                price: 5_000_000,
                quantity: 50_000_000_000,
                batch_id: BATCH_ID,
                nonce: vec![i as u8; 32],
            };
            let hash_vec = deadmkt_crypto::order_hash(&order);
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&hash_vec);
            let sig = sign_commit(&secret, BATCH_ID, pool_id, &hash);

            let result = v.validate_commit(BATCH_ID, pool_id, hash, nft_id, sig);
            assert_eq!(result, CommitResult::Accepted, "commit {i} should be accepted");
        }

        // 4th commit — over quota
        let order4 = Order {
            nft_id,
            symbol: b"EMM/KAY".to_vec(),
            side: 1,
            price: 5_000_000,
            quantity: 50_000_000_000,
            batch_id: BATCH_ID,
            nonce: vec![0xFF; 32],
        };
        let hash_vec = deadmkt_crypto::order_hash(&order4);
        let mut hash4 = [0u8; 32];
        hash4.copy_from_slice(&hash_vec);
        let sig4 = sign_commit(&secret, BATCH_ID, pool_id, &hash4);

        let result = v.validate_commit(BATCH_ID, pool_id, hash4, nft_id, sig4);
        assert_eq!(result, CommitResult::Rejected(CommitRejectReason::OverQuota));
    }

    // -- T_VAL_05: Duplicate hash --

    #[test]
    fn test_val_05_duplicate_hash() {
        let (secret, public) = generate_keypair();
        let nft_id = 42;
        let mut v = make_validator(nft_id, public);
        let pool_id = pool_for(nft_id);

        let hash = [0xCC; 32];
        let sig = sign_commit(&secret, BATCH_ID, pool_id, &hash);

        // First: accepted
        let result = v.validate_commit(BATCH_ID, pool_id, hash, nft_id, sig.clone());
        assert_eq!(result, CommitResult::Accepted);

        // Same hash again: duplicate
        let result = v.validate_commit(BATCH_ID, pool_id, hash, nft_id, sig);
        assert_eq!(result, CommitResult::Rejected(CommitRejectReason::DuplicateHash));
    }

    // -- T_VAL_06: Blocked nft --

    #[test]
    fn test_val_06_blocked_nft() {
        let (secret, public) = generate_keypair();
        let nft_id = 42;
        let pool_id = pool_for(nft_id);

        let mut pubkeys = MockPubkeys::new();
        pubkeys.add(nft_id, public);
        let reg = MockRegistration::new(&[nft_id]);
        let blocked = MockBlocked::new(&[nft_id]); // BLOCKED
        let mut v = GossipValidator::new(
            pubkeys, reg, blocked, make_context(pool_id),
        );

        let hash = [0xCC; 32];
        let sig = sign_commit(&secret, BATCH_ID, pool_id, &hash);

        let result = v.validate_commit(BATCH_ID, pool_id, hash, nft_id, sig);
        assert_eq!(result, CommitResult::Rejected(CommitRejectReason::BlockedNft));
    }

    // -- T_VAL_07: Unregistered nft (R53) --

    #[test]
    fn test_val_07_unregistered_nft() {
        let (secret, public) = generate_keypair();
        let nft_id = 42;
        let pool_id = pool_for(nft_id);

        let mut pubkeys = MockPubkeys::new();
        pubkeys.add(nft_id, public);
        let reg = MockRegistration::new(&[]); // NOT registered
        let blocked = MockBlocked::new(&[]);
        let mut v = GossipValidator::new(
            pubkeys, reg, blocked, make_context(pool_id),
        );

        let hash = [0xCC; 32];
        let sig = sign_commit(&secret, BATCH_ID, pool_id, &hash);

        let result = v.validate_commit(BATCH_ID, pool_id, hash, nft_id, sig);
        assert_eq!(result, CommitResult::Rejected(CommitRejectReason::UnregisteredNft));
    }

    // -- T_VAL_08: Valid reveal --

    #[test]
    fn test_val_08_valid_reveal() {
        let (secret, public) = generate_keypair();
        let nft_id = 42;
        let mut v = make_validator(nft_id, public);
        let pool_id = pool_for(nft_id);

        let order = make_order(nft_id);
        let hash_vec = deadmkt_crypto::order_hash(&order);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_vec);

        // First commit
        let commit_sig = sign_commit(&secret, BATCH_ID, pool_id, &hash);
        let cr = v.validate_commit(BATCH_ID, pool_id, hash, nft_id, commit_sig);
        assert_eq!(cr, CommitResult::Accepted);

        // Then reveal
        let order_sig = sign_order(&secret, &order);
        let rr = v.validate_reveal(BATCH_ID, pool_id, &order, &order_sig);
        assert_eq!(rr, RevealResult::Accepted);
    }

    // -- T_VAL_09: Reveal with no matching commit --

    #[test]
    fn test_val_09_reveal_no_commit() {
        let (secret, public) = generate_keypair();
        let nft_id = 42;
        let mut v = make_validator(nft_id, public);
        let pool_id = pool_for(nft_id);

        // Reveal without prior commit
        let order = make_order(nft_id);
        let order_sig = sign_order(&secret, &order);
        let rr = v.validate_reveal(BATCH_ID, pool_id, &order, &order_sig);
        assert_eq!(rr, RevealResult::Rejected(RevealRejectReason::NoMatchingCommit));
    }

    // -- T_VAL_10: Reveal with invalid order signature --

    #[test]
    fn test_val_10_reveal_invalid_sig() {
        let (secret, public) = generate_keypair();
        let nft_id = 42;
        let mut v = make_validator(nft_id, public);
        let pool_id = pool_for(nft_id);

        let order = make_order(nft_id);
        let hash_vec = deadmkt_crypto::order_hash(&order);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_vec);

        let commit_sig = sign_commit(&secret, BATCH_ID, pool_id, &hash);
        v.validate_commit(BATCH_ID, pool_id, hash, nft_id, commit_sig);

        // Reveal with bad signature
        let bad_sig = vec![0xFF; 64];
        let rr = v.validate_reveal(BATCH_ID, pool_id, &order, &bad_sig);
        assert_eq!(rr, RevealResult::Rejected(RevealRejectReason::InvalidSignature));
    }

    // -- T_VAL_11: Reveal nft_id mismatch --

    #[test]
    fn test_val_11_reveal_nft_mismatch() {
        let (secret, public) = generate_keypair();
        let (secret2, public2) = generate_keypair();
        let nft_id = 42;
        let other_nft = 99;

        // Validator knows both keys
        let pool_id = pool_for(nft_id);
        let mut pubkeys = MockPubkeys::new();
        pubkeys.add(nft_id, public);
        pubkeys.add(other_nft, public2);
        let reg = MockRegistration::new(&[nft_id, other_nft]);
        let blocked = MockBlocked::new(&[]);
        let mut v = GossipValidator::new(
            pubkeys, reg, blocked, make_context(pool_id),
        );

        // Commit from nft_id=42
        let order_42 = make_order(nft_id);
        let hash_vec = deadmkt_crypto::order_hash(&order_42);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_vec);
        let commit_sig = sign_commit(&secret, BATCH_ID, pool_id, &hash);
        v.validate_commit(BATCH_ID, pool_id, hash, nft_id, commit_sig);

        // Reveal with order that has nft_id=99 but same hash
        // Actually this won't have the same hash since nft_id is in the order.
        // So we need to create an order with nft_id=99 but same nonce to get
        // a different hash. The reveal hash won't match the commit.
        // Let me rethink: the real scenario is a commit from nft=42,
        // then a reveal where the order has nft_id=42 in the order struct
        // but we've tampered the commit's nft_id field to be different.
        //
        // Actually in practice, the commit stores nft_id from the Commitment message,
        // and the reveal's order contains its own nft_id. They must match.
        // To test mismatch: commit says nft_id=42, reveal order has nft_id=42
        // BUT we stored the commit record with a different nft_id.
        //
        // Simplest test: manually insert a commit record with mismatched nft_id.
        let order_99 = Order {
            nft_id: other_nft, // different nft
            symbol: b"EMM/KAY".to_vec(),
            side: 1,
            price: 5_000_000,
            quantity: 50_000_000_000,
            batch_id: BATCH_ID,
            nonce: vec![0xBB; 32],
        };
        let hash_99_vec = deadmkt_crypto::order_hash(&order_99);
        let mut hash_99 = [0u8; 32];
        hash_99.copy_from_slice(&hash_99_vec);

        // Manually insert a commit that claims nft_id=42 but the order is for nft_id=99
        v.pending_commits.insert(hash_99, CommitRecord {
            batch_id: BATCH_ID,
            pool_id,
            hash: hash_99,
            nft_id: nft_id, // commit says 42
            signature: vec![0; 64],
            revealed: false,
        });

        // Reveal with order for nft_id=99 — hash matches but nft_id mismatch
        let order_sig = sign_order(&secret2, &order_99);
        let rr = v.validate_reveal(BATCH_ID, pool_id, &order_99, &order_sig);
        assert_eq!(rr, RevealResult::Rejected(RevealRejectReason::NftIdMismatch));
    }

    // -- T_VAL_12: Duplicate reveal ignored --

    #[test]
    fn test_val_12_duplicate_reveal() {
        let (secret, public) = generate_keypair();
        let nft_id = 42;
        let mut v = make_validator(nft_id, public);
        let pool_id = pool_for(nft_id);

        let order = make_order(nft_id);
        let hash_vec = deadmkt_crypto::order_hash(&order);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_vec);

        let commit_sig = sign_commit(&secret, BATCH_ID, pool_id, &hash);
        v.validate_commit(BATCH_ID, pool_id, hash, nft_id, commit_sig);

        let order_sig = sign_order(&secret, &order);

        // First reveal: accepted
        let rr1 = v.validate_reveal(BATCH_ID, pool_id, &order, &order_sig);
        assert_eq!(rr1, RevealResult::Accepted);

        // Second reveal: idempotent
        let rr2 = v.validate_reveal(BATCH_ID, pool_id, &order, &order_sig);
        assert_eq!(rr2, RevealResult::DuplicateIgnored);
    }

    // -- T_VAL_13: BatchComplete valid --

    #[test]
    fn test_val_13_batch_complete_valid() {
        let (secret, public) = generate_keypair();
        let nft_id = 42;
        let pool_id = pool_for(nft_id);
        let mut v = make_validator(nft_id, public);

        let symbol = b"EMM/KAY";
        let avg_price = 49_000_000u64;
        let volume = 10_000_000_000u64;
        let match_count = 5u64;

        let sig = sign_batch_complete(
            &secret, BATCH_ID, pool_id, symbol, avg_price, volume, match_count,
        );

        let result = v.validate_batch_complete(
            BATCH_ID, pool_id, symbol, avg_price, volume, match_count, nft_id, &sig,
        );
        assert_eq!(result, BatchCompleteResult::Accepted);
    }

    // -- T_VAL_14: BatchComplete wrong pool assignment --

    #[test]
    fn test_val_14_batch_complete_bad_pool() {
        let (secret, public) = generate_keypair();
        let nft_id = 42;
        let correct_pool = pool_for(nft_id);
        let wrong_pool = (correct_pool + 1) % NUM_POOLS;

        // Validator is on wrong pool
        let mut pubkeys = MockPubkeys::new();
        pubkeys.add(nft_id, public);
        let reg = MockRegistration::new(&[nft_id]);
        let blocked = MockBlocked::new(&[]);
        let mut v = GossipValidator::new(
            pubkeys, reg, blocked, make_context(wrong_pool),
        );

        let symbol = b"EMM/KAY";
        let sig = sign_batch_complete(
            &secret, BATCH_ID, wrong_pool, symbol, 49_000_000, 10_000_000_000, 5,
        );

        let result = v.validate_batch_complete(
            BATCH_ID, wrong_pool, symbol, 49_000_000, 10_000_000_000, 5, nft_id, &sig,
        );
        assert_eq!(result, BatchCompleteResult::Rejected(BatchCompleteRejectReason::BadPoolAssignment));
    }

    // -- T_VAL_15: Commit violation evidence --

    #[test]
    fn test_val_15_commit_violation_evidence() {
        let mut tracker = CommitQuotaTracker::new(3);

        // 3 accepted
        for i in 0..3 {
            let hash = [i as u8; 32];
            assert!(tracker.check_and_record(42, BATCH_ID, 0, hash, vec![i; 64]));
        }

        // No reportable evidence yet (exactly at limit)
        assert!(tracker.get_reportable_evidence().is_empty());

        // 4th: rejected, evidence stored
        let hash4 = [0xFF; 32];
        assert!(!tracker.check_and_record(42, BATCH_ID, 0, hash4, vec![0xFF; 64]));

        // Now we have evidence with 4 entries
        let evidence = tracker.get_reportable_evidence();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].accused_nft_id, 42);
        assert_eq!(evidence[0].commit_hashes.len(), 4);
        assert_eq!(evidence[0].commit_signatures.len(), 4);
    }

    // -- T_VAL_16: Quota tracker reset --

    #[test]
    fn test_val_16_quota_tracker_reset() {
        let mut tracker = CommitQuotaTracker::new(3);

        // Fill up
        for i in 0..4 {
            let hash = [i as u8; 32];
            tracker.check_and_record(42, BATCH_ID, 0, hash, vec![i; 64]);
        }
        assert_eq!(tracker.get_reportable_evidence().len(), 1);

        // Reset
        tracker.reset();
        assert!(tracker.get_reportable_evidence().is_empty());

        // Can accept again
        let hash = [0xAA; 32];
        assert!(tracker.check_and_record(42, BATCH_ID + 1, 0, hash, vec![0xAA; 64]));
    }
}
