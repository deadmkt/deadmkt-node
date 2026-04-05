// =========================================================================
// deadmkt-node-state: Node state machine
// =========================================================================
//
// States: Setup → Connected → Observing → Trading ⇄ Paused / Limited → Shutdown
// Pure logic — no I/O, no async.

// =========================================================================
// Types
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NodeState {
    Setup,
    Connected,     // P2P live, no strategy
    Observing,     // Strategy loaded, waiting for first batch
    Trading,       // Full batch loop active
    Paused,        // Chain paused, current batch completed
    Limited,       // NFT inactive — gossip only, no trading
    Shutdown,      // Graceful exit in progress
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeEvent {
    SetupComplete,
    StrategyConnected,
    StrategyDisconnected,
    FirstBatchEntered,
    PauseDetected { after_batch: u64 },
    ResumeDetected,
    InactiveDetected,
    Reactivated,
    ShutdownRequested,
    GasCritical,
    GasReplenished,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("invalid transition: {from:?} + {event:?}")]
    InvalidTransition { from: NodeState, event: NodeEvent },
}

pub struct NodeStateMachine {
    state: NodeState,
    previous_state: Option<NodeState>,
    gas_critical: bool,
}

// =========================================================================
// Implementation
// =========================================================================

impl NodeStateMachine {
    pub fn new() -> Self {
        Self {
            state: NodeState::Setup,
            previous_state: None,
            gas_critical: false,
        }
    }

    pub fn state(&self) -> NodeState {
        self.state
    }

    pub fn gas_critical(&self) -> bool {
        self.gas_critical
    }

    /// Apply an event and transition to the next state.
    /// Returns the new state, or Err if the transition is invalid.
    pub fn transition(&mut self, event: NodeEvent) -> Result<NodeState, StateError> {
        match (&self.state, &event) {
            // ── Happy path ───────────────────────────────────────────
            (NodeState::Setup, NodeEvent::SetupComplete) => {
                self.state = NodeState::Connected;
            }
            (NodeState::Connected, NodeEvent::StrategyConnected) => {
                self.state = NodeState::Observing;
            }
            (NodeState::Observing, NodeEvent::FirstBatchEntered) => {
                self.state = NodeState::Trading;
            }

            // ── Strategy disconnect ──────────────────────────────────
            (NodeState::Observing, NodeEvent::StrategyDisconnected) => {
                self.state = NodeState::Connected;
            }
            (NodeState::Trading, NodeEvent::StrategyDisconnected) => {
                self.state = NodeState::Connected;
            }

            // ── Pause / Resume ───────────────────────────────────────
            (NodeState::Trading, NodeEvent::PauseDetected { .. }) => {
                self.previous_state = Some(NodeState::Trading);
                self.state = NodeState::Paused;
            }
            (NodeState::Observing, NodeEvent::PauseDetected { .. }) => {
                self.previous_state = Some(NodeState::Observing);
                self.state = NodeState::Paused;
            }
            (NodeState::Paused, NodeEvent::ResumeDetected) => {
                self.state = self.previous_state.unwrap_or(NodeState::Trading);
                self.previous_state = None;
            }

            // ── Inactive / Reactivated ───────────────────────────────
            (NodeState::Trading, NodeEvent::InactiveDetected) => {
                self.state = NodeState::Limited;
            }
            (NodeState::Observing, NodeEvent::InactiveDetected) => {
                self.state = NodeState::Limited;
            }
            (NodeState::Limited, NodeEvent::Reactivated) => {
                self.state = NodeState::Trading;
            }

            // ── Gas ──────────────────────────────────────────────────
            (_, NodeEvent::GasCritical) => {
                self.gas_critical = true;
                // Don't change state — gas_critical is a flag, not a state.
                // can_commit() checks this flag.
            }
            (_, NodeEvent::GasReplenished) => {
                self.gas_critical = false;
            }

            // ── Shutdown (from any state) ────────────────────────────
            (_, NodeEvent::ShutdownRequested) => {
                self.state = NodeState::Shutdown;
            }

            // ── Strategy reconnect while paused/limited ──────────────
            (NodeState::Paused, NodeEvent::StrategyConnected) => {
                // Stay paused, just note strategy is back
            }
            (NodeState::Limited, NodeEvent::StrategyConnected) => {
                // Stay limited
            }
            (NodeState::Connected, NodeEvent::FirstBatchEntered) => {
                // Can't enter batch without strategy — ignore
                return Err(StateError::InvalidTransition {
                    from: self.state,
                    event,
                });
            }

            _ => {
                return Err(StateError::InvalidTransition {
                    from: self.state,
                    event,
                });
            }
        }

        Ok(self.state)
    }

    // ── Guards ───────────────────────────────────────────────────────────

    /// Can the node commit orders this batch?
    pub fn can_commit(&self) -> bool {
        self.state == NodeState::Trading && !self.gas_critical
    }

    /// Can the node participate in gossip?
    pub fn can_gossip(&self) -> bool {
        matches!(
            self.state,
            NodeState::Connected
                | NodeState::Observing
                | NodeState::Trading
                | NodeState::Paused
                | NodeState::Limited
        )
    }

    /// Is the node actively running batches?
    pub fn is_active(&self) -> bool {
        self.state == NodeState::Trading
    }
}

impl Default for NodeStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── T_STATE_01: Setup → SetupComplete → Connected

    #[test]
    fn t_state_01_setup_to_connected() {
        let mut sm = NodeStateMachine::new();
        assert_eq!(sm.state(), NodeState::Setup);

        let new = sm.transition(NodeEvent::SetupComplete).unwrap();
        assert_eq!(new, NodeState::Connected);
    }

    // ── T_STATE_02: Connected → StrategyConnected → Observing

    #[test]
    fn t_state_02_connected_to_observing() {
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();

        let new = sm.transition(NodeEvent::StrategyConnected).unwrap();
        assert_eq!(new, NodeState::Observing);
    }

    // ── T_STATE_03: Observing → FirstBatchEntered → Trading

    #[test]
    fn t_state_03_observing_to_trading() {
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();
        sm.transition(NodeEvent::StrategyConnected).unwrap();

        let new = sm.transition(NodeEvent::FirstBatchEntered).unwrap();
        assert_eq!(new, NodeState::Trading);
    }

    // ── T_STATE_04: Trading → PauseDetected → Paused (previous=Trading)

    #[test]
    fn t_state_04_trading_to_paused() {
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();
        sm.transition(NodeEvent::StrategyConnected).unwrap();
        sm.transition(NodeEvent::FirstBatchEntered).unwrap();

        let new = sm.transition(NodeEvent::PauseDetected { after_batch: 100 }).unwrap();
        assert_eq!(new, NodeState::Paused);
        assert_eq!(sm.previous_state, Some(NodeState::Trading));
    }

    // ── T_STATE_05: Paused → ResumeDetected → Trading (restored)

    #[test]
    fn t_state_05_paused_to_trading() {
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();
        sm.transition(NodeEvent::StrategyConnected).unwrap();
        sm.transition(NodeEvent::FirstBatchEntered).unwrap();
        sm.transition(NodeEvent::PauseDetected { after_batch: 100 }).unwrap();

        let new = sm.transition(NodeEvent::ResumeDetected).unwrap();
        assert_eq!(new, NodeState::Trading);
        assert_eq!(sm.previous_state, None);
    }

    // ── T_STATE_06: Trading → InactiveDetected → Limited

    #[test]
    fn t_state_06_trading_to_limited() {
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();
        sm.transition(NodeEvent::StrategyConnected).unwrap();
        sm.transition(NodeEvent::FirstBatchEntered).unwrap();

        let new = sm.transition(NodeEvent::InactiveDetected).unwrap();
        assert_eq!(new, NodeState::Limited);
    }

    // ── T_STATE_07: Limited → Reactivated → Trading

    #[test]
    fn t_state_07_limited_to_trading() {
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();
        sm.transition(NodeEvent::StrategyConnected).unwrap();
        sm.transition(NodeEvent::FirstBatchEntered).unwrap();
        sm.transition(NodeEvent::InactiveDetected).unwrap();

        let new = sm.transition(NodeEvent::Reactivated).unwrap();
        assert_eq!(new, NodeState::Trading);
    }

    // ── T_STATE_08: can_commit — Trading + gas ok → true

    #[test]
    fn t_state_08_can_commit_trading() {
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();
        sm.transition(NodeEvent::StrategyConnected).unwrap();
        sm.transition(NodeEvent::FirstBatchEntered).unwrap();

        assert!(sm.can_commit());
        assert!(!sm.gas_critical());
    }

    // ── T_STATE_09: can_commit — Trading + gas critical → false

    #[test]
    fn t_state_09_can_commit_gas_critical() {
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();
        sm.transition(NodeEvent::StrategyConnected).unwrap();
        sm.transition(NodeEvent::FirstBatchEntered).unwrap();
        sm.transition(NodeEvent::GasCritical).unwrap();

        assert!(!sm.can_commit());
        assert!(sm.gas_critical());
    }

    // ── T_STATE_10: can_commit — Paused / Limited / Connected → false

    #[test]
    fn t_state_10_can_commit_non_trading() {
        // Paused
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();
        sm.transition(NodeEvent::StrategyConnected).unwrap();
        sm.transition(NodeEvent::FirstBatchEntered).unwrap();
        sm.transition(NodeEvent::PauseDetected { after_batch: 100 }).unwrap();
        assert!(!sm.can_commit());

        // Limited
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();
        sm.transition(NodeEvent::StrategyConnected).unwrap();
        sm.transition(NodeEvent::FirstBatchEntered).unwrap();
        sm.transition(NodeEvent::InactiveDetected).unwrap();
        assert!(!sm.can_commit());

        // Connected
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();
        assert!(!sm.can_commit());
    }

    // ── Extra: Shutdown from any state

    #[test]
    fn test_shutdown_from_trading() {
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();
        sm.transition(NodeEvent::StrategyConnected).unwrap();
        sm.transition(NodeEvent::FirstBatchEntered).unwrap();

        let new = sm.transition(NodeEvent::ShutdownRequested).unwrap();
        assert_eq!(new, NodeState::Shutdown);
    }

    // ── Extra: Gas replenished clears flag

    #[test]
    fn test_gas_replenished() {
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();
        sm.transition(NodeEvent::StrategyConnected).unwrap();
        sm.transition(NodeEvent::FirstBatchEntered).unwrap();
        sm.transition(NodeEvent::GasCritical).unwrap();
        assert!(!sm.can_commit());

        sm.transition(NodeEvent::GasReplenished).unwrap();
        assert!(sm.can_commit());
    }

    // ── Extra: Strategy disconnect from Trading → Connected

    #[test]
    fn test_strategy_disconnect_trading() {
        let mut sm = NodeStateMachine::new();
        sm.transition(NodeEvent::SetupComplete).unwrap();
        sm.transition(NodeEvent::StrategyConnected).unwrap();
        sm.transition(NodeEvent::FirstBatchEntered).unwrap();

        let new = sm.transition(NodeEvent::StrategyDisconnected).unwrap();
        assert_eq!(new, NodeState::Connected);
    }
}
