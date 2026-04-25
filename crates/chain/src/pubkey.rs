// deadmkt-chain::pubkey
//
// Pubkey cache with chain fallback.
// Pubkeys are immutable once registered — cached permanently.
//
// Lookup flow: SQLite cache → chain view call → cache result.

use crate::client::{ChainError, SupraClient};
use deadmkt_storage::Storage;

// =========================================================================
// PubkeyCache
// =========================================================================

pub struct PubkeyCache<'a> {
    storage: &'a Storage,
    client: &'a SupraClient,
}

impl<'a> PubkeyCache<'a> {
    pub fn new(storage: &'a Storage, client: &'a SupraClient) -> Self {
        Self { storage, client }
    }

    /// Get pubkey, checking cache first, falling back to chain view call.
    /// Returns the 32-byte Ed25519 public key.
    pub async fn get_pubkey(&self, nft_id: u64) -> Result<[u8; 32], ChainError> {
        // 1. Check cache
        if let Ok(Some(cached)) = self.storage.get_pubkey(nft_id) {
            if cached.pubkey.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(&cached.pubkey);
                return Ok(key);
            }
        }

        // 2. Fetch from chain
        let pubkey_bytes = self.client.get_trustee_pubkey(nft_id).await?;

        if pubkey_bytes.len() != 32 {
            return Err(ChainError::DeserializationError(format!(
                "pubkey for nft {} has invalid length: {} (expected 32)",
                nft_id,
                pubkey_bytes.len()
            )));
        }

        // 3. Cache it
        let _ = self.storage.cache_pubkey(
            nft_id,
            &pubkey_bytes,
            &format!("nft_{}", nft_id), // placeholder address
        );

        let mut key = [0u8; 32];
        key.copy_from_slice(&pubkey_bytes);
        Ok(key)
    }

    /// Pre-warm cache for predicted pool members.
    /// For each known nft_id, checks if they belong to our pool via
    /// pool assignment, then fetches and caches their pubkey.
    pub async fn warm_pool(
        &self,
        batch_id: u64,
        num_pools: u64,
        pool_id: u64,
        known_nfts: &[u64],
    ) {
        use crate::batch::compute_pool_assignment;

        for &nft_id in known_nfts {
            if compute_pool_assignment(nft_id, batch_id, num_pools) == pool_id {
                let _ = self.get_pubkey(nft_id).await;
            }
        }
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn setup() -> (MockServer, Storage) {
        let mock_server = MockServer::start().await;
        let dir = tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        // Leak the tempdir so it isn't dropped while storage is in use
        std::mem::forget(dir);
        (mock_server, storage)
    }

    // T_PK_01: Pubkey cache hit — no chain call
    #[tokio::test]
    async fn test_pk_01_cache_hit() {
        let (mock_server, storage) = setup().await;

        // Pre-populate cache
        let pubkey = vec![0xABu8; 32];
        storage.cache_pubkey(42, &pubkey, "0xADDR").unwrap();

        // Mock should NOT be called (we'll verify with expect(0))
        Mock::given(method("POST"))
            .and(path("/rpc/v2/view"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": ["0xDEAD"]
            })))
            .expect(0) // Must NOT be called
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let cache = PubkeyCache::new(&storage, &client);

        let result = cache.get_pubkey(42).await.unwrap();
        assert_eq!(result, [0xAB; 32]);
    }

    // T_PK_02: Pubkey cache miss — fetches from chain, stores in cache
    #[tokio::test]
    async fn test_pk_02_cache_miss_fetches() {
        let (mock_server, storage) = setup().await;

        let pubkey_hex = "ab".repeat(32); // 32 bytes of 0xAB
        Mock::given(method("POST"))
            .and(path("/rpc/v2/view"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": [format!("0x{}", pubkey_hex)]
            })))
            .expect(1) // Called exactly once
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let cache = PubkeyCache::new(&storage, &client);

        // First call: cache miss → chain call
        let result = cache.get_pubkey(42).await.unwrap();
        assert_eq!(result, [0xAB; 32]);

        // Second call: cache hit — mock expects exactly 1 call
        let result2 = cache.get_pubkey(42).await.unwrap();
        assert_eq!(result2, [0xAB; 32]);
    }

    // T_PK_03: Pubkey for unknown nft_id — returns error
    #[tokio::test]
    async fn test_pk_03_unknown_nft() {
        let (mock_server, storage) = setup().await;

        // Chain returns an error for unknown NFT
        Mock::given(method("POST"))
            .and(path("/rpc/v2/view"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let cache = PubkeyCache::new(&storage, &client);

        let result = cache.get_pubkey(999).await;
        assert!(result.is_err());
    }

    // T_PK_04: Warm pool pre-fetches pubkeys for pool members
    #[tokio::test]
    async fn test_pk_04_warm_pool() {
        let (mock_server, storage) = setup().await;

        let pubkey_hex = "cd".repeat(32);
        Mock::given(method("POST"))
            .and(path("/rpc/v2/view"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": [format!("0x{}", pubkey_hex)]
            })))
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let cache = PubkeyCache::new(&storage, &client);

        // Find which nft_ids land in pool 0 for batch 100
        use crate::batch::compute_pool_assignment;
        let mut pool_0_members = Vec::new();
        for nft_id in 1..=20u64 {
            if compute_pool_assignment(nft_id, 100, 4) == 0 {
                pool_0_members.push(nft_id);
            }
        }
        assert!(!pool_0_members.is_empty(), "should have some pool 0 members");

        // Warm pool 0
        cache.warm_pool(100, 4, 0, &(1..=20).collect::<Vec<u64>>()).await;

        // Verify they're cached — subsequent lookups shouldn't need chain
        for &nft_id in &pool_0_members {
            let cached = storage.get_pubkey(nft_id).unwrap();
            assert!(cached.is_some(), "nft {} should be cached", nft_id);
        }
    }
}
