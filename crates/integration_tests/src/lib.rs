// deadmkt-integration-tests
//
// Full batch cycle integration tests (Phase 6).
// These tests verify end-to-end behavior across multiple crates:
// crypto + gossip + validation + matching + batch_state.

#[cfg(test)]
mod tests {
    use deadmkt_batch_state::BatchState;
    use deadmkt_chain::batch::{compute_pool_assignment, Phase};
    use deadmkt_chain::types::BatchParams;
    use deadmkt_crypto::{
        generate_keypair, order_hash, sign_commit, sign_order, Order,
    };
    use deadmkt_gossip::messages::{self, GossipMessage};
    use deadmkt_gossip::network::{wait_for_listen_addr, GossipNode};
    use deadmkt_matching::{
        compute_order_hash, run_matching, MarketConfig, MatchingInput, RevealedOrder,
    };
    use libp2p::identity::Keypair;
    use std::time::Duration;

    // =====================================================================
    // Helpers
    // =====================================================================

    const BATCH_ID: u64 = 100;
    const POOL_ID: u64 = 3;
    const EMM_KAY: &[u8] = b"EMM/KAY";
    const BUY: u8 = 1;
    const SELL: u8 = 0;

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
            symbol: EMM_KAY.to_vec(),
            min_quantity: 1,
        }
    }

    fn make_order(nft_id: u64, side: u8, price: u64, quantity: u64) -> Order {
        Order {
            nft_id,
            symbol: EMM_KAY.to_vec(),
            side,
            price,
            quantity,
            batch_id: BATCH_ID,
            nonce: vec![nft_id as u8; 32],
        }
    }

    // =====================================================================
    // T_INT_01: Cross-crate commit integration
    // =====================================================================
    //
    // crypto-signed commit → gossip serialization → deserialization →
    // signature verification → BatchState acceptance.
    // Gossipsub transport proven by Phase 5 (T_GOSSIP_03, T_GOSSIP_07).

    #[tokio::test]
    async fn test_int_01_commit_exchange_integration() {
        let (secret_b, pub_b) = generate_keypair();
        let order_b = make_order(42, BUY, 50, 100);
        let hash_vec = order_hash(&order_b);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_vec);
        let sig = sign_commit(&secret_b, BATCH_ID, POOL_ID, &hash);

        let commit_msg = GossipMessage::Commitment {
            batch_id: BATCH_ID,
            pool_id: POOL_ID,
            hash,
            nft_id: 42,
            signature: sig.clone(),
        };

        // Serialize → deserialize (gossipsub wire path)
        let wire_bytes = messages::serialize(&commit_msg).expect("serialize");
        let received = messages::deserialize(&wire_bytes).expect("deserialize");

        if let GossipMessage::Commitment {
            batch_id,
            pool_id,
            nft_id,
            hash: rx_hash,
            signature: rx_sig,
        } = &received
        {
            assert_eq!(*batch_id, BATCH_ID);
            assert_eq!(*pool_id, POOL_ID);
            assert_eq!(*nft_id, 42);
            assert_eq!(*rx_hash, hash);

            // Verify commit signature: batch_id_LE || pool_id_LE || hash
            let mut verify_msg = Vec::with_capacity(48);
            verify_msg.extend_from_slice(&batch_id.to_le_bytes());
            verify_msg.extend_from_slice(&pool_id.to_le_bytes());
            verify_msg.extend_from_slice(rx_hash);
            assert!(
                deadmkt_crypto::verify(&pub_b, &verify_msg, rx_sig),
                "commit signature should verify on receiving node"
            );
        } else {
            panic!("expected Commitment, got: {:?}", received);
        }

        // Commitment flows into BatchState
        let mut state = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Commit, make_params(), vec![emm_kay_market()],
        );
        state.add_commit(BATCH_ID, POOL_ID, hash, 42, sig).unwrap();
        assert_eq!(state.commit_count(), 1);
    }

    // =====================================================================
    // T_INT_02: Full batch cycle — COMMIT → REVEAL → MATCH
    // =====================================================================

    #[tokio::test]
    async fn test_int_02_full_batch_cycle() {
        let (secret_a, _pub_a) = generate_keypair();
        let (secret_b, _pub_b) = generate_keypair();

        let order_a = make_order(1, BUY, 50, 1000);
        let order_a_sig = sign_order(&secret_a, &order_a);
        let order_a_hash = compute_order_hash(&order_a);

        let order_b = make_order(2, SELL, 48, 1000);
        let order_b_sig = sign_order(&secret_b, &order_b);
        let order_b_hash = compute_order_hash(&order_b);

        // COMMIT phase: both nodes collect both commits
        let mut state_a = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Commit, make_params(), vec![emm_kay_market()],
        );
        let mut state_b = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Commit, make_params(), vec![emm_kay_market()],
        );

        state_a.add_commit(BATCH_ID, POOL_ID, order_a_hash, 1, vec![0; 64]).unwrap();
        state_a.add_commit(BATCH_ID, POOL_ID, order_b_hash, 2, vec![0; 64]).unwrap();
        state_b.add_commit(BATCH_ID, POOL_ID, order_a_hash, 1, vec![0; 64]).unwrap();
        state_b.add_commit(BATCH_ID, POOL_ID, order_b_hash, 2, vec![0; 64]).unwrap();
        assert_eq!(state_a.commit_count(), 2);
        assert_eq!(state_b.commit_count(), 2);

        // REVEAL phase
        state_a.transition_to(Phase::Reveal).unwrap();
        state_b.transition_to(Phase::Reveal).unwrap();

        state_a.add_reveal(order_a.clone(), order_a_sig.to_vec()).unwrap();
        state_a.add_reveal(order_b.clone(), order_b_sig.to_vec()).unwrap();
        state_b.add_reveal(order_a.clone(), order_a_sig.to_vec()).unwrap();
        state_b.add_reveal(order_b.clone(), order_b_sig.to_vec()).unwrap();
        assert_eq!(state_a.reveal_count(), 2);
        assert_eq!(state_b.reveal_count(), 2);

        // MATCH phase: both compute matches
        state_a.transition_to(Phase::Match).unwrap();
        state_b.transition_to(Phase::Match).unwrap();

        assert_eq!(state_a.match_count(), 1);
        assert_eq!(state_b.match_count(), 1);

        let ma = &state_a.matches[0];
        let mb = &state_b.matches[0];

        // Byte-identical match results (CD-1)
        assert_eq!(ma.batch_id, mb.batch_id);
        assert_eq!(ma.pool_id, mb.pool_id);
        assert_eq!(ma.symbol, mb.symbol);
        assert_eq!(ma.buyer.order.nft_id, mb.buyer.order.nft_id);
        assert_eq!(ma.seller.order.nft_id, mb.seller.order.nft_id);
        assert_eq!(ma.fill_quantity, mb.fill_quantity);
        assert_eq!(ma.settlement_price, mb.settlement_price);
        assert_eq!(ma.match_hash, mb.match_hash);
        assert_eq!(ma.gas_payer_nft_id, mb.gas_payer_nft_id);

        // Verify correctness
        assert_eq!(ma.settlement_price, 49); // (50+48)/2
        assert_eq!(ma.fill_quantity, 1000);
    }

    // =====================================================================
    // T_INT_03: 3 nodes — determinism proof (CD-1)
    // =====================================================================

    #[tokio::test]
    async fn test_int_03_determinism_three_nodes() {
        let orders: Vec<Order> = vec![
            make_order(1, BUY, 55, 500),
            make_order(2, BUY, 52, 300),
            make_order(3, BUY, 48, 700),
            make_order(4, SELL, 50, 400),
            make_order(5, SELL, 53, 200),
            make_order(6, SELL, 47, 600),
        ];

        let reveals: Vec<RevealedOrder> = orders
            .iter()
            .map(|o| RevealedOrder {
                order: o.clone(),
                signature: vec![0u8; 64],
                order_hash: compute_order_hash(o),
            })
            .collect();

        let mut all_results = Vec::new();
        for _ in 0..3 {
            let input = MatchingInput {
                batch_id: BATCH_ID,
                pool_id: POOL_ID,
                reveals: reveals.clone(),
                markets: vec![emm_kay_market()],
            };
            all_results.push(run_matching(&input));
        }

        assert_eq!(all_results.len(), 3);
        assert!(!all_results[0].is_empty(), "should have at least one match");

        for i in 1..3 {
            assert_eq!(
                all_results[0].len(), all_results[i].len(),
                "match count differs: node 0 vs node {}", i
            );
            for (j, (m0, mi)) in all_results[0].iter().zip(all_results[i].iter()).enumerate() {
                assert_eq!(m0.buyer.order.nft_id, mi.buyer.order.nft_id, "match {} buyer", j);
                assert_eq!(m0.seller.order.nft_id, mi.seller.order.nft_id, "match {} seller", j);
                assert_eq!(m0.fill_quantity, mi.fill_quantity, "match {} fill_qty", j);
                assert_eq!(m0.settlement_price, mi.settlement_price, "match {} price", j);
                assert_eq!(m0.match_hash, mi.match_hash, "match {} hash (CD-1 violation!)", j);
                assert_eq!(m0.gas_payer_nft_id, mi.gas_payer_nft_id, "match {} gas_payer", j);
            }
        }
    }

    // =====================================================================
    // T_INT_04: Missed commit phase — graceful skip
    // =====================================================================

    #[tokio::test]
    async fn test_int_04_missed_commit_graceful_skip() {
        let mut state = BatchState::new(
            BATCH_ID, POOL_ID, Phase::Reveal, make_params(), vec![emm_kay_market()],
        );

        assert_eq!(state.commit_count(), 0);

        // Reveal without commit → error
        let order = make_order(1, BUY, 50, 100);
        let result = state.add_reveal(order, vec![0; 64]);
        assert!(result.is_err(), "reveal without commit should fail");

        // MATCH with no reveals → 0 matches
        state.transition_to(Phase::Match).unwrap();
        assert_eq!(state.match_count(), 0);

        // SWAP → still fine
        state.transition_to(Phase::Swap).unwrap();
        assert_eq!(state.phase, Phase::Swap);

        // Rollover to next batch
        state.reset_for_new_batch(BATCH_ID + 1, POOL_ID, Phase::Commit, vec![emm_kay_market()]);
        assert_eq!(state.batch_id, BATCH_ID + 1);
        assert_eq!(state.phase, Phase::Commit);
        assert_eq!(state.commit_count(), 0);

        // Now participate normally
        let order2 = make_order(42, BUY, 50, 100);
        let hash = compute_order_hash(&order2);
        state.add_commit(BATCH_ID + 1, POOL_ID, hash, 42, vec![0; 64]).unwrap();
        assert_eq!(state.commit_count(), 1);
    }

    // =====================================================================
    // T_INT_05: Pool rotation across batches (CD-3)
    // =====================================================================

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_int_05_pool_rotation() {
        let keypair = Keypair::generate_ed25519();
        let mut node = GossipNode::new(keypair).unwrap();
        node.listen_on_random_port().unwrap();
        let _ = wait_for_listen_addr(node.swarm_mut(), Duration::from_secs(5)).await;

        let nft_id = 42u64;
        let num_pools = 4u64;

        let pool_100 = compute_pool_assignment(nft_id, 100, num_pools);
        node.subscribe_pool(pool_100).unwrap();
        assert!(node.is_subscribed_to_pool(pool_100));

        let pool_101 = compute_pool_assignment(nft_id, 101, num_pools);

        // CD-3: Overlap subscription
        node.subscribe_pool(pool_101).unwrap();
        assert!(node.is_subscribed_to_pool(pool_100), "old pool still subscribed during overlap");
        assert!(node.is_subscribed_to_pool(pool_101), "new pool subscribed");

        node.unsubscribe_pool(pool_100).unwrap();
        assert!(!node.is_subscribed_to_pool(pool_100));
        assert!(node.is_subscribed_to_pool(pool_101));

        // BatchState rotation
        let mut state = BatchState::new(
            100, pool_100, Phase::Commit, make_params(), vec![emm_kay_market()],
        );
        assert_eq!(state.pool_id, pool_100);

        state.reset_for_new_batch(101, pool_101, Phase::Commit, vec![emm_kay_market()]);
        assert_eq!(state.batch_id, 101);
        assert_eq!(state.pool_id, pool_101);
    }
}
