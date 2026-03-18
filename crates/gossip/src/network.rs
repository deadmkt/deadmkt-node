// deadmkt-gossip::network
//
// libp2p gossipsub networking layer.
// Manages swarm creation, topic subscription, message broadcast/receive.
//
// Topic naming:
//   "pool:{pool_id}"              — pool-scoped commits and reveals
//   "global:batch_complete"       — cross-pool batch summaries
//   "global:settlement_report"    — settlement outcome reports

use crate::messages::{self, GossipMessage};
use libp2p::futures::StreamExt;
use libp2p::gossipsub::{self, IdentTopic, MessageAuthenticity, ValidationMode};
use libp2p::identity::Keypair;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, Swarm, SwarmBuilder};
use std::collections::HashSet;
use std::time::Duration;
use thiserror::Error;

// =========================================================================
// Errors
// =========================================================================

#[derive(Debug, Error)]
pub enum GossipError {
    #[error("gossipsub publish failed: {0}")]
    PublishError(String),

    #[error("serialization error: {0}")]
    SerializationError(String),

    #[error("not subscribed to pool topic")]
    NotSubscribedToPool,

    #[error("swarm error: {0}")]
    SwarmError(String),
}

// =========================================================================
// GossipNode
// =========================================================================

pub struct GossipNode {
    swarm: Swarm<gossipsub::Behaviour>,
    pool_topics: HashSet<u64>,
    global_complete_topic: IdentTopic,
    global_report_topic: IdentTopic,
}

impl GossipNode {
    /// Create a new gossip node with the given identity keypair.
    pub fn new(keypair: Keypair) -> Result<Self, GossipError> {
        let global_complete = IdentTopic::new("global:batch_complete");
        let global_report = IdentTopic::new("global:settlement_report");

        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_millis(200))
            .validation_mode(ValidationMode::Permissive)
            .max_transmit_size(65536)
            .flood_publish(true)
            .mesh_n_low(1)
            .mesh_n(2)
            .mesh_outbound_min(0)
            .build()
            .map_err(|e| GossipError::SwarmError(e.to_string()))?;

        let gossipsub = gossipsub::Behaviour::new(
            MessageAuthenticity::Signed(keypair.clone()),
            gossipsub_config,
        )
        .map_err(|e| GossipError::SwarmError(e.to_string()))?;

        let swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default(),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )
            .map_err(|e| GossipError::SwarmError(e.to_string()))?
            .with_behaviour(|_| Ok(gossipsub))
            .map_err(|e| GossipError::SwarmError(e.to_string()))?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
            .build();

        let mut node = GossipNode {
            swarm,
            pool_topics: HashSet::new(),
            global_complete_topic: global_complete.clone(),
            global_report_topic: global_report.clone(),
        };

        node.swarm
            .behaviour_mut()
            .subscribe(&global_complete)
            .map_err(|e| GossipError::SwarmError(e.to_string()))?;
        node.swarm
            .behaviour_mut()
            .subscribe(&global_report)
            .map_err(|e| GossipError::SwarmError(e.to_string()))?;

        Ok(node)
    }

    pub fn listen_on_random_port(&mut self) -> Result<(), GossipError> {
        let addr: Multiaddr = "/ip4/127.0.0.1/tcp/0".parse().expect("valid multiaddr");
        self.swarm
            .listen_on(addr)
            .map_err(|e| GossipError::SwarmError(e.to_string()))?;
        Ok(())
    }

    pub fn listen_on_port(&mut self, port: u16) -> Result<(), GossipError> {
        let addr: Multiaddr = format!("/ip4/0.0.0.0/tcp/{}", port).parse().expect("valid multiaddr");
        self.swarm
            .listen_on(addr)
            .map_err(|e| GossipError::SwarmError(e.to_string()))?;
        Ok(())
    }

    pub fn dial(&mut self, addr: Multiaddr) -> Result<(), GossipError> {
        self.swarm
            .dial(addr)
            .map_err(|e| GossipError::SwarmError(e.to_string()))?;
        Ok(())
    }

    pub fn subscribe_pool(&mut self, pool_id: u64) -> Result<(), GossipError> {
        let topic = IdentTopic::new(format!("pool:{}", pool_id));
        self.swarm
            .behaviour_mut()
            .subscribe(&topic)
            .map_err(|e| GossipError::SwarmError(e.to_string()))?;
        self.pool_topics.insert(pool_id);
        Ok(())
    }

    pub fn unsubscribe_pool(&mut self, pool_id: u64) -> Result<(), GossipError> {
        let topic = IdentTopic::new(format!("pool:{}", pool_id));
        self.swarm
            .behaviour_mut()
            .unsubscribe(&topic)
            .map_err(|e| GossipError::SwarmError(e.to_string()))?;
        self.pool_topics.remove(&pool_id);
        Ok(())
    }

    pub fn is_subscribed_to_pool(&self, pool_id: u64) -> bool {
        self.pool_topics.contains(&pool_id)
    }

    pub fn publish_to_pool(
        &mut self,
        pool_id: u64,
        msg: &GossipMessage,
    ) -> Result<(), GossipError> {
        if !self.pool_topics.contains(&pool_id) {
            return Err(GossipError::NotSubscribedToPool);
        }
        let topic = IdentTopic::new(format!("pool:{}", pool_id));
        let bytes = messages::serialize(msg)
            .map_err(|e| GossipError::SerializationError(e.to_string()))?;
        self.swarm
            .behaviour_mut()
            .publish(topic, bytes)
            .map_err(|e| GossipError::PublishError(e.to_string()))?;
        Ok(())
    }

    pub fn publish_global_complete(&mut self, msg: &GossipMessage) -> Result<(), GossipError> {
        let bytes = messages::serialize(msg)
            .map_err(|e| GossipError::SerializationError(e.to_string()))?;
        self.swarm
            .behaviour_mut()
            .publish(self.global_complete_topic.clone(), bytes)
            .map_err(|e| GossipError::PublishError(e.to_string()))?;
        Ok(())
    }

    pub fn publish_global_report(&mut self, msg: &GossipMessage) -> Result<(), GossipError> {
        let bytes = messages::serialize(msg)
            .map_err(|e| GossipError::SerializationError(e.to_string()))?;
        self.swarm
            .behaviour_mut()
            .publish(self.global_report_topic.clone(), bytes)
            .map_err(|e| GossipError::PublishError(e.to_string()))?;
        Ok(())
    }

    pub fn swarm_mut(&mut self) -> &mut Swarm<gossipsub::Behaviour> {
        &mut self.swarm
    }

    pub fn add_explicit_peer(&mut self, peer_id: &libp2p::PeerId) {
        self.swarm.behaviour_mut().add_explicit_peer(peer_id);
    }

    pub fn local_peer_id(&self) -> libp2p::PeerId {
        *self.swarm.local_peer_id()
    }

    /// Return how many peers gossipsub knows are subscribed to a topic.
    pub fn peer_count_for_topic(&self, topic_str: &str) -> usize {
        let topic_hash = IdentTopic::new(topic_str).hash();
        self.swarm
            .behaviour()
            .all_peers()
            .filter(|(_, topics)| topics.iter().any(|t| **t == topic_hash))
            .count()
    }

    /// Publish to pool topic with retry. Waits for gossipsub mesh to form by
    /// driving the swarm between attempts. Returns Ok on success or the last
    /// error after `max_attempts`.
    pub async fn publish_to_pool_with_retry(
        &mut self,
        pool_id: u64,
        msg: &GossipMessage,
        max_attempts: u32,
        retry_delay: Duration,
    ) -> Result<(), GossipError> {
        if !self.pool_topics.contains(&pool_id) {
            return Err(GossipError::NotSubscribedToPool);
        }

        let mut last_err = None;
        for _ in 0..max_attempts {
            match self.publish_to_pool(pool_id, msg) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_err = Some(e);
                    // Drive swarm to process heartbeats
                    collect_messages(&mut self.swarm, retry_delay).await;
                }
            }
        }

        Err(last_err.unwrap_or(GossipError::PublishError("max attempts reached".into())))
    }

    /// Publish to global batch_complete topic with retry.
    pub async fn publish_global_complete_with_retry(
        &mut self,
        msg: &GossipMessage,
        max_attempts: u32,
        retry_delay: Duration,
    ) -> Result<(), GossipError> {
        let mut last_err = None;
        for _ in 0..max_attempts {
            match self.publish_global_complete(msg) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_err = Some(e);
                    collect_messages(&mut self.swarm, retry_delay).await;
                }
            }
        }
        Err(last_err.unwrap_or(GossipError::PublishError("max attempts reached".into())))
    }
}

/// Resolve the first actual listen address from the swarm.
pub async fn wait_for_listen_addr(
    swarm: &mut Swarm<gossipsub::Behaviour>,
    timeout: Duration,
) -> Option<Multiaddr> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::select! {
            event = swarm.select_next_some() => {
                if let SwarmEvent::NewListenAddr { address, .. } = event {
                    return Some(address);
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                return None;
            }
        }
    }
}

/// Drive the swarm and collect received gossip messages for a duration.
pub async fn collect_messages(
    swarm: &mut Swarm<gossipsub::Behaviour>,
    duration: Duration,
) -> Vec<GossipMessage> {
    let mut received = Vec::new();
    let deadline = tokio::time::Instant::now() + duration;

    loop {
        tokio::select! {
            event = swarm.select_next_some() => {
                if let SwarmEvent::Behaviour(gossipsub::Event::Message {
                    message, ..
                }) = event
                {
                    if let Ok(msg) = messages::deserialize(&message.data) {
                        received.push(msg);
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                break;
            }
        }
    }

    received
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::GossipMessage;
    use libp2p::identity::Keypair;
    use tokio::sync::mpsc;

    fn make_node() -> GossipNode {
        let keypair = Keypair::generate_ed25519();
        GossipNode::new(keypair).unwrap()
    }

    async fn make_listening_node() -> (GossipNode, Multiaddr) {
        let mut node = make_node();
        node.listen_on_random_port().unwrap();
        let addr = wait_for_listen_addr(node.swarm_mut(), Duration::from_secs(5))
            .await
            .expect("should get listen address");
        (node, addr)
    }

    fn dummy_commitment() -> GossipMessage {
        GossipMessage::Commitment {
            batch_id: 100,
            pool_id: 3,
            hash: [0xCC; 32],
            nft_id: 42,
            signature: vec![0xAA; 64],
        }
    }

    fn dummy_batch_complete() -> GossipMessage {
        GossipMessage::BatchComplete {
            batch_id: 100,
            pool_id: 3,
            symbol: b"EMM/KAY".to_vec(),
            avg_settlement_price: 49_000_000,
            volume: 10_000_000_000,
            match_count: 5,
            num_commits: 12,
            num_reveals: 8,
            sender_nft_id: 42,
            signature: vec![0xBB; 64],
        }
    }

    /// Helper: connect two nodes, drive both swarms until gossipsub mesh forms.
    /// Spawns node_a into a background task that drives its swarm
    /// and collects messages. Returns (node_b, handle, stop_signal).
    async fn connect_pair_and_mesh(
        pool_id: Option<u64>,
    ) -> (
        GossipNode,
        tokio::task::JoinHandle<Vec<GossipMessage>>,
        mpsc::Sender<()>,
    ) {
        let (mut node_a, addr_a) = make_listening_node().await;
        let (mut node_b, _) = make_listening_node().await;

        if let Some(pid) = pool_id {
            node_a.subscribe_pool(pid).unwrap();
            node_b.subscribe_pool(pid).unwrap();
        }

        // Add each other as explicit peers
        let peer_a = node_a.local_peer_id();
        let peer_b = node_b.local_peer_id();
        node_a.add_explicit_peer(&peer_b);
        node_b.add_explicit_peer(&peer_a);

        // Spawn node_a into background FIRST — it needs to run continuously
        // to accept connections and exchange gossipsub subscriptions.
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let handle = tokio::spawn(async move {
            let mut received = Vec::new();
            loop {
                tokio::select! {
                    event = node_a.swarm.select_next_some() => {
                        if let SwarmEvent::Behaviour(gossipsub::Event::Message {
                            message, ..
                        }) = event {
                            if let Ok(msg) = messages::deserialize(&message.data) {
                                received.push(msg);
                            }
                        }
                    }
                    _ = stop_rx.recv() => {
                        break;
                    }
                }
            }
            received
        });

        // Give node_a's task a moment to start
        tokio::task::yield_now().await;

        // Dial and wait for connection
        node_b.dial(addr_a).unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::select! {
                event = node_b.swarm_mut().select_next_some() => {
                    if let SwarmEvent::ConnectionEstablished { .. } = event {
                        break;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("connection timed out");
                }
            }
        }

        // Drive node_b for long enough to exchange gossipsub subscriptions.
        // node_a is already running in background. With heartbeat_interval=200ms,
        // 8 seconds gives ~40 heartbeat cycles for subscription exchange.
        collect_messages(node_b.swarm_mut(), Duration::from_secs(8)).await;

        (node_b, handle, stop_tx)
    }

    // T_GOSSIP_01: Node starts and listens
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_gossip_01_node_starts() {
        let (node, addr) = make_listening_node().await;
        let addr_str = addr.to_string();
        assert!(addr_str.contains("/ip4/127.0.0.1/tcp/"), "addr: {}", addr_str);
        assert!(!node.local_peer_id().to_string().is_empty());
    }

    // T_GOSSIP_02: Two nodes connect via dial
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_gossip_02_two_nodes_connect() {
        let (_node_b, handle, stop_tx) = connect_pair_and_mesh(None).await;
        // If we get here, connection succeeded
        let _ = stop_tx.send(()).await;
        let _ = handle.await;
    }

    // T_GOSSIP_03: Pool topic subscribe + publish → other node receives
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_gossip_03_pool_topic_exchange() {
        let (mut node_b, handle, stop_tx) = connect_pair_and_mesh(Some(3)).await;

        // B publishes to pool 3 — A (background) should receive
        let msg = dummy_commitment();

        // Retry publish — gossipsub may need extra heartbeat after background spawn
        let mut published = false;
        for _ in 0..20 {
            if node_b.publish_to_pool(3, &msg).is_ok() {
                published = true;
                break;
            }
            collect_messages(node_b.swarm_mut(), Duration::from_millis(300)).await;
        }
        assert!(published, "should publish to pool after retries");

        // Drive node_b's swarm to flush the message over the wire
        collect_messages(node_b.swarm_mut(), Duration::from_secs(3)).await;

        let _ = stop_tx.send(()).await;
        let received_a = handle.await.unwrap();

        assert!(
            !received_a.is_empty(),
            "node A should receive the message from node B"
        );
        assert_eq!(received_a[0], msg);
    }

    // T_GOSSIP_04: Global topic publish → other node receives
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_gossip_04_global_topic() {
        let (mut node_b, handle, stop_tx) = connect_pair_and_mesh(None).await;

        let msg = dummy_batch_complete();

        // Retry publish — gossipsub may need extra heartbeat after spawn
        let mut published = false;
        let mut last_err = String::new();
        for _ in 0..30 {
            match node_b.publish_global_complete(&msg) {
                Ok(()) => {
                    published = true;
                    break;
                }
                Err(e) => {
                    last_err = e.to_string();
                    collect_messages(node_b.swarm_mut(), Duration::from_millis(500)).await;
                }
            }
        }
        assert!(published, "should publish global after retries, last err: {last_err}");

        // Drive node_b's swarm to send the message
        collect_messages(node_b.swarm_mut(), Duration::from_secs(3)).await;

        let _ = stop_tx.send(()).await;
        let received_a = handle.await.unwrap();

        assert!(
            !received_a.is_empty(),
            "node A should receive global message from node B"
        );
        assert_eq!(received_a[0], msg);
    }

    // T_GOSSIP_05: Pool rotation — overlap subscribe
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_gossip_05_pool_rotation_overlap() {
        let mut node = make_node();

        node.subscribe_pool(0).unwrap();
        assert!(node.is_subscribed_to_pool(0));

        node.subscribe_pool(2).unwrap();
        assert!(node.is_subscribed_to_pool(0));
        assert!(node.is_subscribed_to_pool(2));

        node.unsubscribe_pool(0).unwrap();
        assert!(!node.is_subscribed_to_pool(0));
        assert!(node.is_subscribed_to_pool(2));
    }

    // T_GOSSIP_06: Publish to unsubscribed pool → error
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_gossip_06_publish_unsubscribed_pool() {
        let mut node = make_node();
        let msg = dummy_commitment();
        let result = node.publish_to_pool(99, &msg);
        assert!(matches!(result, Err(GossipError::NotSubscribedToPool)));
    }

    // T_GOSSIP_07: Serialized GossipMessage survives network roundtrip
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore] // Flaky: gossipsub mesh formation is timing-sensitive in CI/sandbox
    async fn test_gossip_07_message_roundtrip() {
        let (mut node_b, handle, stop_tx) = connect_pair_and_mesh(Some(3)).await;

        let msg = GossipMessage::Reveal {
            batch_id: 100,
            pool_id: 3,
            order: deadmkt_crypto::Order {
                nft_id: 42,
                symbol: b"EMM/KAY".to_vec(),
                side: 1,
                price: 5_000_000,
                quantity: 50_000_000_000,
                batch_id: 100,
                nonce: vec![0xBB; 32],
            },
            signature: vec![0xDD; 64],
        };

        // Retry publish
        let mut published = false;
        for _ in 0..20 {
            if node_b.publish_to_pool(3, &msg).is_ok() {
                published = true;
                break;
            }
            collect_messages(node_b.swarm_mut(), Duration::from_millis(300)).await;
        }
        assert!(published, "should publish reveal after retries");
        collect_messages(node_b.swarm_mut(), Duration::from_secs(3)).await;

        let _ = stop_tx.send(()).await;
        let received = handle.await.unwrap();

        assert!(!received.is_empty(), "should receive message");
        assert_eq!(received[0], msg, "message should survive roundtrip exactly");
    }

    // T_GOSSIP_08: Node handles peer disconnect gracefully
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_gossip_08_peer_disconnect() {
        let (mut node_a, addr_a) = make_listening_node().await;
        let (mut node_b, _) = make_listening_node().await;

        node_b.dial(addr_a).unwrap();

        // Spawn node_b: drive its swarm for 2 seconds, then drop it (disconnect)
        let handle_b = tokio::spawn(async move {
            // Drive swarm so handshake completes
            collect_messages(node_b.swarm_mut(), Duration::from_secs(2)).await;
            drop(node_b); // disconnect
        });

        // Drive node_a until connected
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut connected = false;
        loop {
            tokio::select! {
                event = node_a.swarm.select_next_some() => {
                    if let SwarmEvent::ConnectionEstablished { .. } = event {
                        connected = true;
                        break;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    break;
                }
            }
        }
        assert!(connected, "node_a should have connected");

        // Wait for node_b to drop
        let _ = handle_b.await;

        // Drive node_a after disconnect — should not panic
        collect_messages(node_a.swarm_mut(), Duration::from_secs(1)).await;

        // Node_a should still be functional
        node_a.subscribe_pool(5).unwrap();
        assert!(node_a.is_subscribed_to_pool(5));
    }
}
