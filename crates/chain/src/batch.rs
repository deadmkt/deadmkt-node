// Batch phase computation (pure, no network).
// Mirrors pool_config.move batch_id / phase math exactly.

use crate::types::{BatchEpoch, BatchParams};
use sha2::{Digest, Sha256};

// =========================================================================
// Phase enum
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    Commit,
    Reveal,
    Match,
    Swap,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Phase::Commit => write!(f, "COMMIT"),
            Phase::Reveal => write!(f, "REVEAL"),
            Phase::Match => write!(f, "MATCH"),
            Phase::Swap => write!(f, "SWAP"),
        }
    }
}

// =========================================================================
// Core computations
// =========================================================================

/// Compute batch_id from block height + epoch.
/// Formula: anchor_batch_id + (block_height - anchor_block) / blocks_per_batch
pub fn compute_batch_id(block_height: u64, epoch: &BatchEpoch, params: &BatchParams) -> u64 {
    let blocks_since = block_height.saturating_sub(epoch.anchor_block);
    epoch.anchor_batch_id + (blocks_since / params.blocks_per_batch)
}

/// Compute the current phase from block height.
///
/// Within a batch (blocks_per_batch blocks):
///   [0..commit_blocks)                         -> Commit
///   [commit_blocks..commit+reveal)             -> Reveal
///   [commit+reveal..commit+reveal+match)       -> Match
///   [commit+reveal+match..blocks_per_batch)    -> Swap
pub fn compute_phase(block_height: u64, epoch: &BatchEpoch, params: &BatchParams) -> Phase {
    let blocks_since = block_height.saturating_sub(epoch.anchor_block);
    let block_in_batch = blocks_since % params.blocks_per_batch;

    let commit_end = params.commit_blocks;
    let reveal_end = commit_end + params.reveal_blocks;
    let match_end = reveal_end + params.match_blocks;

    if block_in_batch < commit_end {
        Phase::Commit
    } else if block_in_batch < reveal_end {
        Phase::Reveal
    } else if block_in_batch < match_end {
        Phase::Match
    } else {
        Phase::Swap
    }
}

/// Detect if a phase boundary occurs between two consecutive blocks.
pub fn is_phase_boundary(
    old_block: u64,
    new_block: u64,
    epoch: &BatchEpoch,
    params: &BatchParams,
) -> bool {
    compute_phase(old_block, epoch, params) != compute_phase(new_block, epoch, params)
}

/// Compute a new epoch after a parameter change.
/// new_anchor_block = old_anchor + (effective_at_batch - old_anchor_batch_id) * old_blocks_per_batch
pub fn compute_new_epoch(
    old_epoch: &BatchEpoch,
    old_params: &BatchParams,
    effective_at_batch: u64,
) -> BatchEpoch {
    let batches_ahead = effective_at_batch.saturating_sub(old_epoch.anchor_batch_id);
    BatchEpoch {
        anchor_block: old_epoch.anchor_block + batches_ahead * old_params.blocks_per_batch,
        anchor_batch_id: effective_at_batch,
    }
}

/// Compute pool assignment: SHA256(nft_id_le || batch_id_le) -> first 8 bytes as u64 -> % num_pools
pub fn compute_pool_assignment(nft_id: u64, batch_id: u64, num_pools: u64) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(nft_id.to_le_bytes());
    hasher.update(batch_id.to_le_bytes());
    let hash = hasher.finalize();
    let val = u64::from_le_bytes(hash[0..8].try_into().unwrap());
    val % num_pools
}

/// Check whether the next batch should start (respects pause state).
pub fn should_start_next_batch(is_paused: bool) -> bool {
    !is_paused
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(blocks: u64) -> BatchParams {
        assert!(blocks >= 7, "need at least 7 blocks");
        BatchParams {
            blocks_per_batch: blocks,
            commit_blocks: blocks - 6,
            reveal_blocks: 3,
            match_blocks: 1,
            swap_blocks: 2,
            commits_per_batch: 3,
        }
    }

    // T_BATCH_01
    #[test]
    fn test_batch_id_computation() {
        let epoch = BatchEpoch { anchor_block: 1000, anchor_batch_id: 50 };
        let params = make_params(10);

        assert_eq!(compute_batch_id(1000, &epoch, &params), 50);
        assert_eq!(compute_batch_id(1009, &epoch, &params), 50);
        assert_eq!(compute_batch_id(1010, &epoch, &params), 51);
        assert_eq!(compute_batch_id(1019, &epoch, &params), 51);
        assert_eq!(compute_batch_id(1030, &epoch, &params), 53);
    }

    // T_BATCH_02
    #[test]
    fn test_phase_computation() {
        let epoch = BatchEpoch { anchor_block: 1000, anchor_batch_id: 50 };
        let params = BatchParams {
            blocks_per_batch: 10,
            commit_blocks: 4,
            reveal_blocks: 3,
            match_blocks: 1,
            swap_blocks: 2,
            commits_per_batch: 3,
        };

        // Batch 50: COMMIT 1000-1003, REVEAL 1004-1006, MATCH 1007, SWAP 1008-1009
        assert_eq!(compute_phase(1000, &epoch, &params), Phase::Commit);
        assert_eq!(compute_phase(1003, &epoch, &params), Phase::Commit);
        assert_eq!(compute_phase(1004, &epoch, &params), Phase::Reveal);
        assert_eq!(compute_phase(1006, &epoch, &params), Phase::Reveal);
        assert_eq!(compute_phase(1007, &epoch, &params), Phase::Match);
        assert_eq!(compute_phase(1008, &epoch, &params), Phase::Swap);
        assert_eq!(compute_phase(1009, &epoch, &params), Phase::Swap);

        // Batch 51
        assert_eq!(compute_phase(1010, &epoch, &params), Phase::Commit);
        assert_eq!(compute_phase(1013, &epoch, &params), Phase::Commit);
        assert_eq!(compute_phase(1014, &epoch, &params), Phase::Reveal);
    }

    // T_BATCH_03
    #[test]
    fn test_epoch_transition() {
        let old_params = make_params(10);
        let old_epoch = BatchEpoch { anchor_block: 1000, anchor_batch_id: 50 };

        let new_epoch = compute_new_epoch(&old_epoch, &old_params, 55);
        assert_eq!(new_epoch.anchor_block, 1050);
        assert_eq!(new_epoch.anchor_batch_id, 55);
    }

    // T_BATCH_04
    #[test]
    fn test_batch_id_monotonic_across_epochs() {
        let old_epoch = BatchEpoch { anchor_block: 1000, anchor_batch_id: 50 };
        let old_params = make_params(10);

        let new_epoch = BatchEpoch { anchor_block: 1050, anchor_batch_id: 55 };
        let new_params = make_params(20);

        assert_eq!(compute_batch_id(1049, &old_epoch, &old_params), 54);
        assert_eq!(compute_batch_id(1050, &new_epoch, &new_params), 55);
        // 55 > 54: monotonic, no gap
    }

    // T_BATCH_05
    #[test]
    fn test_pool_assignment() {
        let num_pools = 5;

        let pool_a = compute_pool_assignment(42, 100, num_pools);
        let pool_b = compute_pool_assignment(42, 100, num_pools);
        assert_eq!(pool_a, pool_b); // deterministic

        assert!(pool_a < num_pools);
        let pool_next = compute_pool_assignment(42, 101, num_pools);
        assert!(pool_next < num_pools);
    }

    // T_BATCH_06
    #[test]
    fn test_phase_boundary_detection() {
        let epoch = BatchEpoch { anchor_block: 1000, anchor_batch_id: 50 };
        let params = BatchParams {
            blocks_per_batch: 10,
            commit_blocks: 4,
            reveal_blocks: 3,
            match_blocks: 1,
            swap_blocks: 2,
            commits_per_batch: 3,
        };

        assert!(is_phase_boundary(1003, 1004, &epoch, &params));  // COMMIT->REVEAL
        assert!(!is_phase_boundary(1001, 1002, &epoch, &params)); // COMMIT->COMMIT

        // Batch boundary = phase boundary (SWAP->COMMIT)
        assert!(is_phase_boundary(1009, 1010, &epoch, &params));
        assert_eq!(compute_phase(1009, &epoch, &params), Phase::Swap);
        assert_eq!(compute_phase(1010, &epoch, &params), Phase::Commit);

        // Within SWAP: NOT a boundary
        assert!(!is_phase_boundary(1008, 1009, &epoch, &params));
    }

    // T_BATCH_07
    #[test]
    fn test_should_start_next_batch_respects_pause() {
        assert!(should_start_next_batch(false));  // not paused -> should start
        assert!(!should_start_next_batch(true));  // paused -> should not start
    }
}
