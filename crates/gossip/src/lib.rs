// deadmkt-gossip: P2P gossip layer for commit/reveal/match exchange.
//
// Phase 2: Message types + bincode serialization.
// Phase 5: libp2p gossipsub networking.
// UPG-1: Versioned envelope on all wire messages.

pub mod messages;
pub mod network;

// Re-export for convenience
pub use messages::PROTOCOL_VERSION;
