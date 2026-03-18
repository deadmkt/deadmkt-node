// crates/withdrawal/src/lib.rs
//
// Build 4D: Withdrawal event handling.
//
// Detects chain events:
//   WithdrawalRequested → earmark in EscrowTracker
//   WithdrawalCancelled → release earmark
//   WithdrawalExecuted  → release earmark, update confirmed balance
//   HoldingPeriodStarted → proactive activity_guard (stop committing)
//   HoldingPeriodCancelled → resume trading
//   ClaimExecuted → balance zeroed, node enters LIMITED/SHUTDOWN
//
// activity_guard: stop committing new orders when holding period is
// approaching expiry (within 2 × blocks_per_batch), so no in-flight
// trades get caught.
//
// LIMITED mode: triggered when our NFT is detected as inactive on-chain
// (ClaimExecuted = all funds withdrawn, NFT deactivated).

use deadmkt_escrow_tracker::EscrowTracker;
use std::sync::{Arc, Mutex};
use thiserror::Error;

// =========================================================================
// Errors
// =========================================================================

#[derive(Error, Debug)]
pub enum WithdrawalError {
    #[error("chain error: {0}")]
    Chain(String),

    #[error("escrow tracking error: {0}")]
    Escrow(String),
}

// =========================================================================
// Withdrawal event types
// =========================================================================

/// Withdrawal event types detected from chain.
#[derive(Debug, Clone)]
pub enum WithdrawalEvent {
    /// Rushed withdrawal requested — earmark funds immediately.
    Requested {
        nft_id: u64,
        token: String,
        amount: u64,
    },
    /// Withdrawal cancelled — release earmark.
    Cancelled {
        nft_id: u64,
        token: String,
        amount: u64,
    },
    /// Withdrawal executed — funds left escrow.
    Executed {
        nft_id: u64,
        token: String,
        amount: u64,
    },
    /// Holding period started — beneficiary initiated graceful exit.
    HoldingPeriodStarted {
        nft_id: u64,
        expires_at_batch: u64,
    },
    /// Holding period cancelled — resume trading.
    HoldingPeriodCancelled {
        nft_id: u64,
    },
    /// Claim executed — all funds withdrawn, NFT deactivated.
    ClaimExecuted {
        nft_id: u64,
    },
}

// =========================================================================
// Withdrawal action (returned to caller)
// =========================================================================

/// What happened as a result of processing a withdrawal event.
#[derive(Debug, Clone, PartialEq)]
pub enum WithdrawalAction {
    /// Earmark applied — strategy should see reduced projected balance.
    EarmarkApplied { token: String, amount: u64 },
    /// Earmark released (cancelled).
    EarmarkReleased { token: String, amount: u64 },
    /// Withdrawal executed — confirmed balance reduced on-chain.
    BalanceReduced { token: String, amount: u64 },
    /// Holding period started — activity guard should activate near expiry.
    HoldingPeriodStarted { expires_at_batch: u64 },
    /// Holding period cancelled — resume normal trading.
    HoldingPeriodCancelled,
    /// Claim executed — node should shutdown/enter LIMITED.
    ClaimExecuted,
    /// Event was for a different NFT — ignored.
    Ignored,
}

// =========================================================================
// WithdrawalHandler
// =========================================================================

/// Manages withdrawal detection and response.
pub struct WithdrawalHandler {
    tracker: Arc<Mutex<EscrowTracker>>,
    own_nft_id: u64,
    /// Set when holding period is active for our NFT.
    holding_period_expires_at_batch: Option<u64>,
    /// Set when ClaimExecuted received for our NFT.
    claim_executed: bool,
    /// blocks_per_batch — needed for activity guard calculation.
    blocks_per_batch: u64,
}

impl WithdrawalHandler {
    pub fn new(
        tracker: Arc<Mutex<EscrowTracker>>,
        own_nft_id: u64,
        blocks_per_batch: u64,
    ) -> Self {
        Self {
            tracker,
            own_nft_id,
            holding_period_expires_at_batch: None,
            claim_executed: false,
            blocks_per_batch,
        }
    }

    /// Process a withdrawal event from chain.
    /// Returns the action taken (or Ignored if not our NFT).
    pub fn handle_event(
        &mut self,
        event: WithdrawalEvent,
    ) -> Result<WithdrawalAction, WithdrawalError> {
        match event {
            WithdrawalEvent::Requested { nft_id, token, amount } => {
                if nft_id != self.own_nft_id {
                    return Ok(WithdrawalAction::Ignored);
                }
                let mut tracker = self.tracker.lock().unwrap();
                tracker.earmark_withdrawal(&token, amount)
                    .map_err(|e| WithdrawalError::Escrow(e.to_string()))?;
                Ok(WithdrawalAction::EarmarkApplied { token, amount })
            }

            WithdrawalEvent::Cancelled { nft_id, token, amount } => {
                if nft_id != self.own_nft_id {
                    return Ok(WithdrawalAction::Ignored);
                }
                let mut tracker = self.tracker.lock().unwrap();
                tracker.release_earmark(&token, amount, false)
                    .map_err(|e| WithdrawalError::Escrow(e.to_string()))?;
                Ok(WithdrawalAction::EarmarkReleased { token, amount })
            }

            WithdrawalEvent::Executed { nft_id, token, amount } => {
                if nft_id != self.own_nft_id {
                    return Ok(WithdrawalAction::Ignored);
                }
                let mut tracker = self.tracker.lock().unwrap();
                tracker.release_earmark(&token, amount, true)
                    .map_err(|e| WithdrawalError::Escrow(e.to_string()))?;
                Ok(WithdrawalAction::BalanceReduced { token, amount })
            }

            WithdrawalEvent::HoldingPeriodStarted { nft_id, expires_at_batch } => {
                if nft_id != self.own_nft_id {
                    return Ok(WithdrawalAction::Ignored);
                }
                self.holding_period_expires_at_batch = Some(expires_at_batch);
                Ok(WithdrawalAction::HoldingPeriodStarted { expires_at_batch })
            }

            WithdrawalEvent::HoldingPeriodCancelled { nft_id } => {
                if nft_id != self.own_nft_id {
                    return Ok(WithdrawalAction::Ignored);
                }
                self.holding_period_expires_at_batch = None;
                Ok(WithdrawalAction::HoldingPeriodCancelled)
            }

            WithdrawalEvent::ClaimExecuted { nft_id } => {
                if nft_id != self.own_nft_id {
                    return Ok(WithdrawalAction::Ignored);
                }
                self.claim_executed = true;
                Ok(WithdrawalAction::ClaimExecuted)
            }
        }
    }

    /// Check if we should stop committing (holding period approaching).
    /// Returns true when current_batch >= expires_at_batch - (2 × blocks_per_batch).
    /// This gives enough runway to avoid in-flight trades being caught.
    pub fn should_guard(&self, current_batch: u64) -> bool {
        if let Some(expires) = self.holding_period_expires_at_batch {
            let guard_buffer = 2 * self.blocks_per_batch;
            if expires >= guard_buffer {
                current_batch >= expires - guard_buffer
            } else {
                true
            }
        } else {
            false
        }
    }

    /// Check if node should enter LIMITED mode (our NFT inactive).
    pub fn should_enter_limited(&self) -> bool {
        self.claim_executed
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_handler(own_nft_id: u64, bpb: u64) -> (WithdrawalHandler, Arc<Mutex<EscrowTracker>>) {
        let tracker = Arc::new(Mutex::new(EscrowTracker::new()));
        let handler = WithdrawalHandler::new(tracker.clone(), own_nft_id, bpb);
        (handler, tracker)
    }

    // ─── T_WITHDRAW_01: WithdrawalRequested for own NFT → earmark applied

    #[test]
    fn t_withdraw_01_requested_own_nft_earmarks() {
        let (mut handler, tracker) = make_handler(42, 5);
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 10_000);
        }

        let action = handler.handle_event(WithdrawalEvent::Requested {
            nft_id: 42, token: "KAY".into(), amount: 2_000,
        }).unwrap();

        assert_eq!(action, WithdrawalAction::EarmarkApplied {
            token: "KAY".into(), amount: 2_000,
        });
        assert_eq!(tracker.lock().unwrap().projected("KAY"), 8_000);
    }

    // ─── T_WITHDRAW_02: WithdrawalRequested for OTHER NFT → ignored

    #[test]
    fn t_withdraw_02_requested_other_nft_ignored() {
        let (mut handler, tracker) = make_handler(42, 5);
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 10_000);
        }

        let action = handler.handle_event(WithdrawalEvent::Requested {
            nft_id: 99, token: "KAY".into(), amount: 2_000,
        }).unwrap();

        assert_eq!(action, WithdrawalAction::Ignored);
        assert_eq!(tracker.lock().unwrap().projected("KAY"), 10_000);
    }

    // ─── T_WITHDRAW_03: WithdrawalCancelled → earmark released

    #[test]
    fn t_withdraw_03_cancelled_releases_earmark() {
        let (mut handler, tracker) = make_handler(42, 5);
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 10_000);
        }

        handler.handle_event(WithdrawalEvent::Requested {
            nft_id: 42, token: "KAY".into(), amount: 2_000,
        }).unwrap();
        assert_eq!(tracker.lock().unwrap().projected("KAY"), 8_000);

        let action = handler.handle_event(WithdrawalEvent::Cancelled {
            nft_id: 42, token: "KAY".into(), amount: 2_000,
        }).unwrap();

        assert_eq!(action, WithdrawalAction::EarmarkReleased {
            token: "KAY".into(), amount: 2_000,
        });
        assert_eq!(tracker.lock().unwrap().projected("KAY"), 10_000);
    }

    // ─── T_WITHDRAW_04: WithdrawalExecuted → confirmed reduced, earmark cleared

    #[test]
    fn t_withdraw_04_executed_reduces_confirmed() {
        let (mut handler, tracker) = make_handler(42, 5);
        {
            let mut t = tracker.lock().unwrap();
            t.set_confirmed("KAY", 10_000);
        }

        handler.handle_event(WithdrawalEvent::Requested {
            nft_id: 42, token: "KAY".into(), amount: 2_000,
        }).unwrap();

        let action = handler.handle_event(WithdrawalEvent::Executed {
            nft_id: 42, token: "KAY".into(), amount: 2_000,
        }).unwrap();

        assert_eq!(action, WithdrawalAction::BalanceReduced {
            token: "KAY".into(), amount: 2_000,
        });

        let t = tracker.lock().unwrap();
        let bal = t.get_balance("KAY").unwrap();
        assert_eq!(bal.confirmed, 8_000);
        assert_eq!(bal.earmarked, 0);
        assert_eq!(t.projected("KAY"), 8_000);
    }

    // ─── T_WITHDRAW_05: HoldingPeriodStarted → guard activates near expiry

    #[test]
    fn t_withdraw_05_holding_period_guard() {
        let (mut handler, _tracker) = make_handler(42, 5);

        handler.handle_event(WithdrawalEvent::HoldingPeriodStarted {
            nft_id: 42, expires_at_batch: 50100,
        }).unwrap();

        // Guard buffer = 2 * 5 = 10. Activates at batch >= 50090.
        assert!(!handler.should_guard(50089));
        assert!(handler.should_guard(50090));
        assert!(handler.should_guard(50091));
        assert!(handler.should_guard(50100));
    }

    // ─── T_WITHDRAW_06: HoldingPeriodCancelled → guard deactivated

    #[test]
    fn t_withdraw_06_holding_period_cancelled() {
        let (mut handler, _tracker) = make_handler(42, 5);

        handler.handle_event(WithdrawalEvent::HoldingPeriodStarted {
            nft_id: 42, expires_at_batch: 50100,
        }).unwrap();
        assert!(handler.should_guard(50095));

        let action = handler.handle_event(WithdrawalEvent::HoldingPeriodCancelled {
            nft_id: 42,
        }).unwrap();

        assert_eq!(action, WithdrawalAction::HoldingPeriodCancelled);
        assert!(!handler.should_guard(50095));
        assert!(!handler.should_guard(99999));
    }

    // ─── T_WITHDRAW_07: ClaimExecuted → LIMITED mode

    #[test]
    fn t_withdraw_07_claim_executed_limited() {
        let (mut handler, _tracker) = make_handler(42, 5);

        assert!(!handler.should_enter_limited());

        let action = handler.handle_event(WithdrawalEvent::ClaimExecuted {
            nft_id: 42,
        }).unwrap();

        assert_eq!(action, WithdrawalAction::ClaimExecuted);
        assert!(handler.should_enter_limited());
    }
}
