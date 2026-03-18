// deadmkt-chain::poller
//
// Background task that polls block height and emits phase change events.
// This is the shared clock for all nodes (CD-4: no local timers).
//
// The poller drives the batch_state machine via PollerEvents sent
// over a tokio mpsc channel.

use crate::batch::{compute_batch_id, compute_phase, Phase};
use crate::client::{ChainError, SupraClient};
use crate::types::{BatchEpoch, BatchParams};
use std::time::Duration;
use tokio::sync::mpsc;

// =========================================================================
// Poller events
// =========================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum PollerEvent {
    /// Phase changed within the current batch.
    PhaseChanged {
        batch_id: u64,
        phase: Phase,
        block_height: u64,
    },
    /// New batch started (batch_id incremented).
    NewBatch {
        batch_id: u64,
        block_height: u64,
    },
    /// System is paused — not currently implemented as a detection mechanism,
    /// but emitted if the block height stalls beyond a threshold.
    PauseDetected,
}

// =========================================================================
// Poller state
// =========================================================================

struct PollerState {
    last_block: u64,
    last_batch_id: u64,
    last_phase: Phase,
    stall_count: u32,
}

// =========================================================================
// run_block_poller
// =========================================================================

/// Background task: poll block height, emit phase/batch change events.
///
/// Runs until the channel is closed or an unrecoverable error occurs.
/// On RPC failure, increments a stall counter. After `stall_threshold`
/// consecutive failures, emits PauseDetected. Recovers automatically
/// once RPC starts responding again.
///
/// `max_polls` limits the number of iterations (0 = unlimited, useful for testing).
pub async fn run_block_poller(
    client: &SupraClient,
    epoch: &BatchEpoch,
    params: &BatchParams,
    tx: mpsc::Sender<PollerEvent>,
    poll_interval: Duration,
    stall_threshold: u32,
    max_polls: u64,
) -> Result<(), ChainError> {
    // Initialize state from first successful poll
    let ledger = client.get_ledger_info().await?;
    let initial_batch = compute_batch_id(ledger.block_height, epoch, params);
    let initial_phase = compute_phase(ledger.block_height, epoch, params);

    let mut state = PollerState {
        last_block: ledger.block_height,
        last_batch_id: initial_batch,
        last_phase: initial_phase,
        stall_count: 0,
    };

    let mut polls: u64 = 0;

    loop {
        if max_polls > 0 {
            polls += 1;
            if polls > max_polls {
                break;
            }
        }

        tokio::time::sleep(poll_interval).await;

        let ledger = match client.get_ledger_info().await {
            Ok(l) => {
                state.stall_count = 0;
                l
            }
            Err(_) => {
                state.stall_count += 1;
                if state.stall_count >= stall_threshold {
                    let _ = tx.send(PollerEvent::PauseDetected).await;
                }
                continue;
            }
        };

        if ledger.block_height <= state.last_block {
            continue; // No new block
        }

        let new_batch_id = compute_batch_id(ledger.block_height, epoch, params);
        let new_phase = compute_phase(ledger.block_height, epoch, params);

        // Detect new batch
        if new_batch_id != state.last_batch_id {
            let _ = tx
                .send(PollerEvent::NewBatch {
                    batch_id: new_batch_id,
                    block_height: ledger.block_height,
                })
                .await;
            state.last_batch_id = new_batch_id;
            state.last_phase = new_phase;
            state.last_block = ledger.block_height;
            continue;
        }

        // Detect phase change within same batch
        if new_phase != state.last_phase {
            let _ = tx
                .send(PollerEvent::PhaseChanged {
                    batch_id: new_batch_id,
                    phase: new_phase,
                    block_height: ledger.block_height,
                })
                .await;
            state.last_phase = new_phase;
        }

        state.last_block = ledger.block_height;
    }

    Ok(())
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

    // Custom responder that returns incrementing block heights
    struct IncrementingBlock {
        block: Arc<AtomicU64>,
        increment: u64,
    }

    impl Respond for IncrementingBlock {
        fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
            let current = self.block.fetch_add(self.increment, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({
                "height": current,
                "timestamp": { "timestamp": 1708100000u64 },
                "author": "0x00",
                "hash": "0x00",
                "parent": "0x00",
                "view": { "epoch_id": { "chain_id": 6, "epoch": 1 }, "round": 1 }
            }))
        }
    }

    fn test_epoch() -> BatchEpoch {
        BatchEpoch {
            anchor_block: 1000,
            anchor_batch_id: 50,
        }
    }

    fn test_params() -> BatchParams {
        BatchParams {
            blocks_per_batch: 10,
            commit_blocks: 4,
            reveal_blocks: 3,
            match_blocks: 1,
            swap_blocks: 2,
            commits_per_batch: 3,
        }
    }

    // T_POLL_01: Poller emits PhaseChanged on block increment
    #[tokio::test]
    async fn test_poll_01_phase_changed() {
        let mock_server = MockServer::start().await;

        // Start at block 1000 (COMMIT), increment by 1 each call.
        // First call (init): block 1000 → batch 50, COMMIT
        // Polls 1-3: blocks 1001, 1002, 1003 → still COMMIT
        // Poll 4: block 1004 → REVEAL (phase change!)
        let block = Arc::new(AtomicU64::new(1000));
        Mock::given(method("GET"))
            .and(path("/rpc/v2/block"))
            .respond_with(IncrementingBlock {
                block: block.clone(),
                increment: 1,
            })
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let (tx, mut rx) = mpsc::channel(32);

        // Run for 5 polls (init + 5 ticks)
        let epoch = test_epoch();
        let params = test_params();
        tokio::spawn(async move {
            run_block_poller(
                &client,
                &epoch,
                &params,
                tx,
                Duration::from_millis(10),
                5,
                5,
            )
            .await
            .unwrap();
        });

        // Collect events
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        // Should have at least one PhaseChanged (COMMIT→REVEAL at block 1004)
        let phase_changes: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, PollerEvent::PhaseChanged { .. }))
            .collect();
        assert!(
            !phase_changes.is_empty(),
            "expected PhaseChanged event, got: {:?}",
            events
        );

        // First phase change should be to Reveal
        if let PollerEvent::PhaseChanged { phase, .. } = &phase_changes[0] {
            assert_eq!(*phase, Phase::Reveal);
        }
    }

    // T_POLL_02: Poller emits NewBatch on batch boundary
    #[tokio::test]
    async fn test_poll_02_new_batch() {
        let mock_server = MockServer::start().await;

        // Start at block 1008 (SWAP), jump to 1010 (next batch COMMIT)
        // Init: 1008 → batch 50, SWAP
        // We need to jump past the batch boundary.
        let block = Arc::new(AtomicU64::new(1008));
        Mock::given(method("GET"))
            .and(path("/rpc/v2/block"))
            .respond_with(IncrementingBlock {
                block: block.clone(),
                increment: 1,
            })
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let (tx, mut rx) = mpsc::channel(32);

        let epoch = test_epoch();
        let params = test_params();
        tokio::spawn(async move {
            run_block_poller(
                &client,
                &epoch,
                &params,
                tx,
                Duration::from_millis(10),
                5,
                4,
            )
            .await
            .unwrap();
        });

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        // Should have a NewBatch event (batch 51 at block 1010)
        let new_batches: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, PollerEvent::NewBatch { .. }))
            .collect();
        assert!(
            !new_batches.is_empty(),
            "expected NewBatch event, got: {:?}",
            events
        );

        if let PollerEvent::NewBatch { batch_id, .. } = &new_batches[0] {
            assert_eq!(*batch_id, 51);
        }
    }

    // T_POLL_03: Poller handles RPC failure gracefully
    #[tokio::test]
    async fn test_poll_03_rpc_failure_recovery() {
        let mock_server = MockServer::start().await;

        // First call succeeds (init), then 2 failures, then success again
        let call_count = Arc::new(AtomicU64::new(0));
        let call_count_clone = call_count.clone();

        struct FailThenRecover {
            call_count: Arc<AtomicU64>,
        }

        impl Respond for FailThenRecover {
            fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
                let n = self.call_count.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // Init call: success at block 1000
                    ResponseTemplate::new(200).set_body_json(json!({
                        "height": 1000,
                        "timestamp": { "timestamp": 1708100000u64 },
                        "author": "0x00", "hash": "0x00", "parent": "0x00",
                        "view": { "epoch_id": { "chain_id": 6, "epoch": 1 }, "round": 1 }
                    }))
                } else if n <= 2 {
                    // Failures
                    ResponseTemplate::new(500)
                } else {
                    // Recovery at block 1005 (REVEAL)
                    ResponseTemplate::new(200).set_body_json(json!({
                        "height": 1005,
                        "timestamp": { "timestamp": 1708100005u64 },
                        "author": "0x00", "hash": "0x00", "parent": "0x00",
                        "view": { "epoch_id": { "chain_id": 6, "epoch": 1 }, "round": 1 }
                    }))
                }
            }
        }

        Mock::given(method("GET"))
            .and(path("/rpc/v2/block"))
            .respond_with(FailThenRecover {
                call_count: call_count_clone,
            })
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let (tx, mut rx) = mpsc::channel(32);

        let epoch = test_epoch();
        let params = test_params();
        tokio::spawn(async move {
            run_block_poller(
                &client,
                &epoch,
                &params,
                tx,
                Duration::from_millis(10),
                10, // high threshold so no PauseDetected
                4,
            )
            .await
            .unwrap();
        });

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        // Should still get a PhaseChanged after recovery
        let phase_changes: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, PollerEvent::PhaseChanged { .. }))
            .collect();
        assert!(
            !phase_changes.is_empty(),
            "expected PhaseChanged after recovery, got: {:?}",
            events
        );
    }

    // T_POLL_04: Poller detects pause (stall threshold)
    #[tokio::test]
    async fn test_poll_04_pause_detected() {
        let mock_server = MockServer::start().await;

        let call_count = Arc::new(AtomicU64::new(0));
        let call_count_clone = call_count.clone();

        struct InitThenFail {
            call_count: Arc<AtomicU64>,
        }

        impl Respond for InitThenFail {
            fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
                let n = self.call_count.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "height": 1000,
                        "timestamp": { "timestamp": 1708100000u64 },
                        "author": "0x00", "hash": "0x00", "parent": "0x00",
                        "view": { "epoch_id": { "chain_id": 6, "epoch": 1 }, "round": 1 }
                    }))
                } else {
                    ResponseTemplate::new(500)
                }
            }
        }

        Mock::given(method("GET"))
            .and(path("/rpc/v2/block"))
            .respond_with(InitThenFail {
                call_count: call_count_clone,
            })
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let (tx, mut rx) = mpsc::channel(32);

        let epoch = test_epoch();
        let params = test_params();
        tokio::spawn(async move {
            run_block_poller(
                &client,
                &epoch,
                &params,
                tx,
                Duration::from_millis(10),
                3, // stall after 3 failures
                5,
            )
            .await
            .unwrap();
        });

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        // Should have PauseDetected
        assert!(
            events.iter().any(|e| matches!(e, PollerEvent::PauseDetected)),
            "expected PauseDetected, got: {:?}",
            events
        );
    }
}
