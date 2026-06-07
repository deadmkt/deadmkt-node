// deadmkt-chain::market
//
// Market pair config cache with chain fallback.
// Loads active market pairs at startup, caches in SQLite.
// Handles MarketPairAdded events for live updates.

use crate::client::{ChainError, SupraClient};
use deadmkt_storage::Storage;

// =========================================================================
// MarketConfigCache
// =========================================================================

pub struct MarketConfigCache<'a> {
    storage: &'a Storage,
    client: &'a SupraClient,
}

impl<'a> MarketConfigCache<'a> {
    pub fn new(storage: &'a Storage, client: &'a SupraClient) -> Self {
        Self { storage, client }
    }

    /// Load a specific market pair from chain and cache it.
    pub async fn load_market(&self, symbol: &[u8]) -> Result<(), ChainError> {
        let pair = self.client.get_market_pair_config(symbol).await?;

        let symbol_str = String::from_utf8_lossy(symbol).to_string();
        const DEFAULT_TOKEN_DECIMALS: u32 = 5; // Trippples tokens are all 5 decimals
        self.storage
            .upsert_market_pair(
                &symbol_str,
                pair.min_quantity,
                &pair.base_metadata,
                &pair.quote_metadata,
                DEFAULT_TOKEN_DECIMALS, // Trippples tokens are all 5 decimal places
                DEFAULT_TOKEN_DECIMALS,
                pair.is_active,
            )
            .map_err(|e| ChainError::DeserializationError(e.to_string()))?;

        Ok(())
    }

    /// Get market config for a specific symbol (cache-first).
    /// Returns (min_quantity, active) or None if not found.
    pub fn get_market(&self, symbol: &str) -> Option<(u64, bool)> {
        self.storage
            .get_market_pair(symbol)
            .ok()
            .flatten()
            .map(|r| (r.min_quantity, r.active))
    }

    /// Get all active market pairs from cache.
    pub fn list_active(&self) -> Vec<(String, u64)> {
        self.storage
            .list_active_market_pairs()
            .unwrap_or_default()
            .into_iter()
            .map(|r| (r.symbol, r.min_quantity))
            .collect()
    }

    /// Handle a MarketPairAdded event — fetch from chain and cache.
    pub async fn on_market_added(&self, symbol: &[u8]) -> Result<(), ChainError> {
        self.load_market(symbol).await
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
        std::mem::forget(dir);
        (mock_server, storage)
    }

    fn mock_market_pair_response() -> ResponseTemplate {
        // MarketPair::from_view_result expects:
        // [symbol_hex, base_metadata, quote_metadata, min_quantity, is_active]
        ResponseTemplate::new(200).set_body_json(json!({
            "result": [
                "0x454d4d2f4b4159",  // "EMM/KAY" hex
                "0xBASE_META",
                "0xQUOTE_META",
                "1000000",
                true
            ]
        }))
    }

    // T_MKT_01: Market config loaded from chain at startup
    #[tokio::test]
    async fn test_mkt_01_load_from_chain() {
        let (mock_server, storage) = setup().await;

        Mock::given(method("POST"))
            .and(path("/rpc/v2/view"))
            .respond_with(mock_market_pair_response())
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let cache = MarketConfigCache::new(&storage, &client);

        // Load from chain
        cache.load_market(b"EMM/KAY").await.unwrap();

        // Verify cached
        let (min_qty, active) = cache.get_market("EMM/KAY").unwrap();
        assert_eq!(min_qty, 1_000_000);
        assert!(active);

        // Verify list_active
        let active_markets = cache.list_active();
        assert_eq!(active_markets.len(), 1);
        assert_eq!(active_markets[0].0, "EMM/KAY");
        assert_eq!(active_markets[0].1, 1_000_000);
    }

    // T_MKT_02: Market config cache hit — no chain call
    #[tokio::test]
    async fn test_mkt_02_cache_hit() {
        let (mock_server, storage) = setup().await;

        // Pre-populate cache directly
        storage
            .upsert_market_pair(
                "EMM/KAY",
                500_000,
                "0xBASE",
                "0xQUOTE",
                5,
                5,
                true,
            )
            .unwrap();

        // Mock should NOT be called
        Mock::given(method("POST"))
            .and(path("/rpc/v2/view"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock_server)
            .await;

        let client = SupraClient::new(vec![mock_server.uri()], "0xDEADMKT".into());
        let cache = MarketConfigCache::new(&storage, &client);

        // Get from cache
        let (min_qty, active) = cache.get_market("EMM/KAY").unwrap();
        assert_eq!(min_qty, 500_000);
        assert!(active);
    }
}
