// deadmkt-storage: SQLite persistence.
// Pubkey cache, batch history, pending settlements, reputation, schema versioning.

use rusqlite::{params, Connection};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("SQLite error: {0}")]
    SqliteError(#[from] rusqlite::Error),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

const CURRENT_SCHEMA_VERSION: u32 = 3;

// =========================================================================
// Cached pubkey record
// =========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedPubkey {
    pub nft_id: u64,
    pub pubkey: Vec<u8>,
    pub address: String,
}

// =========================================================================
// Market pair record
// =========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketPairRecord {
    pub symbol: String,
    pub min_quantity: u64,
    pub base_token_metadata: String,
    pub quote_token_metadata: String,
    pub base_decimals: u32,
    pub quote_decimals: u32,
    pub active: bool,
}

// =========================================================================
// Settlement record (B4)
// =========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementRecord {
    pub match_hash: Vec<u8>,
    pub batch_id: u64,
    pub pool_id: u64,
    pub buyer_nft_id: u64,
    pub seller_nft_id: u64,
    pub symbol: String,
    pub fill_quantity: u64,
    pub settlement_price: u64,
    pub outflow_token: String,
    pub outflow_amount: u64,
    pub inflow_token: String,
    pub inflow_amount: u64,
    pub tx_hash: Option<String>,
    pub is_gas_payer: bool,
    pub status: String,
    pub abort_code: Option<String>,
    pub submitted_at: Option<u64>,
    pub confirmed_at: Option<u64>,
    pub created_at: u64,
}

// =========================================================================
// Balance snapshot record (B4)
// =========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceSnapshot {
    pub id: u64,
    pub token: String,
    pub confirmed: u64,
    pub pending_out_total: u64,
    pub pending_in_total: u64,
    pub earmarked: u64,
    pub projected: u64,
    pub snapshot_at: u64,
}

// =========================================================================
// Storage
// =========================================================================

pub struct Storage {
    conn: Connection,
}

impl Storage {
    pub fn open(dir: &Path) -> Result<Self, StorageError> {
        let db_path = dir.join("deadmkt.db");
        let conn = Connection::open(db_path)?;
        let storage = Storage { conn };
        storage.create_schema()?;
        Ok(storage)
    }

    fn create_schema(&self) -> Result<(), StorageError> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS pubkeys (
                nft_id INTEGER PRIMARY KEY,
                pubkey BLOB NOT NULL,
                address TEXT NOT NULL,
                cached_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );

            CREATE TABLE IF NOT EXISTS batches (
                batch_id INTEGER PRIMARY KEY,
                pool_id INTEGER NOT NULL,
                phase TEXT NOT NULL,
                block_height INTEGER NOT NULL,
                timestamp INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS commits (
                batch_id INTEGER NOT NULL,
                pool_id INTEGER NOT NULL,
                nft_id INTEGER NOT NULL,
                commit_hash BLOB NOT NULL,
                signature BLOB NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                PRIMARY KEY (batch_id, pool_id, nft_id)
            );

            CREATE TABLE IF NOT EXISTS pending (
                match_hash BLOB PRIMARY KEY,
                batch_id INTEGER NOT NULL,
                buyer_nft_id INTEGER NOT NULL,
                seller_nft_id INTEGER NOT NULL,
                symbol TEXT NOT NULL,
                fill_quantity INTEGER NOT NULL,
                tx_hash TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );

            CREATE TABLE IF NOT EXISTS reputation (
                nft_id INTEGER PRIMARY KEY,
                commits_seen INTEGER NOT NULL DEFAULT 0,
                commits_missed INTEGER NOT NULL DEFAULT 0,
                last_updated INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );

            CREATE TABLE IF NOT EXISTS registered_nfts (
                nft_id INTEGER PRIMARY KEY,
                registered_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS blocked_nfts (
                nft_id INTEGER PRIMARY KEY,
                blocked_at TEXT NOT NULL,
                enforcement_at_batch INTEGER
            );

            CREATE TABLE IF NOT EXISTS market_pairs (
                symbol TEXT PRIMARY KEY,
                min_quantity INTEGER NOT NULL,
                base_token_metadata TEXT NOT NULL,
                quote_token_metadata TEXT NOT NULL,
                base_decimals INTEGER NOT NULL,
                quote_decimals INTEGER NOT NULL,
                active INTEGER NOT NULL DEFAULT 1
            );

            CREATE TABLE IF NOT EXISTS settlements (
                match_hash BLOB PRIMARY KEY,
                batch_id INTEGER NOT NULL,
                pool_id INTEGER NOT NULL,
                buyer_nft_id INTEGER NOT NULL,
                seller_nft_id INTEGER NOT NULL,
                symbol TEXT NOT NULL,
                fill_quantity INTEGER NOT NULL,
                settlement_price INTEGER NOT NULL,
                outflow_token TEXT NOT NULL,
                outflow_amount INTEGER NOT NULL,
                inflow_token TEXT NOT NULL,
                inflow_amount INTEGER NOT NULL,
                tx_hash TEXT,
                is_gas_payer INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'pending',
                abort_code TEXT,
                submitted_at INTEGER,
                confirmed_at INTEGER,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );

            CREATE TABLE IF NOT EXISTS balance_snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                token TEXT NOT NULL,
                confirmed INTEGER NOT NULL,
                pending_out_total INTEGER NOT NULL,
                pending_in_total INTEGER NOT NULL,
                earmarked INTEGER NOT NULL,
                projected INTEGER NOT NULL,
                snapshot_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );
            ",
        )?;

        // Set schema version if not present
        let count: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM schema_version",
            [],
            |row| row.get(0),
        )?;
        if count == 0 {
            self.conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![CURRENT_SCHEMA_VERSION],
            )?;
        }

        Ok(())
    }

    pub fn list_tables(&self) -> Result<Vec<String>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
        )?;
        let tables = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(tables)
    }

    pub fn get_schema_version(&self) -> Result<u32, StorageError> {
        let version: u32 = self.conn.query_row(
            "SELECT version FROM schema_version LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        Ok(version)
    }

    // =====================================================================
    // Pubkey cache
    // =====================================================================

    pub fn cache_pubkey(
        &self,
        nft_id: u64,
        pubkey: &[u8],
        address: &str,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO pubkeys (nft_id, pubkey, address) VALUES (?1, ?2, ?3)",
            params![nft_id as i64, pubkey, address],
        )?;
        Ok(())
    }

    pub fn get_pubkey(&self, nft_id: u64) -> Result<Option<CachedPubkey>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT nft_id, pubkey, address FROM pubkeys WHERE nft_id = ?1",
        )?;

        let result = stmt.query_row(params![nft_id as i64], |row| {
            Ok(CachedPubkey {
                nft_id: row.get::<_, i64>(0)? as u64,
                pubkey: row.get(1)?,
                address: row.get(2)?,
            })
        });

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::SqliteError(e)),
        }
    }

    // =====================================================================
    // Batch history
    // =====================================================================

    pub fn insert_batch(
        &self,
        batch_id: u64,
        pool_id: u64,
        phase: &str,
        block_height: u64,
        timestamp: u64,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO batches (batch_id, pool_id, phase, block_height, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![batch_id as i64, pool_id as i64, phase, block_height as i64, timestamp as i64],
        )?;
        Ok(())
    }

    pub fn get_batch(
        &self,
        batch_id: u64,
    ) -> Result<Option<(u64, String, u64, u64)>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT pool_id, phase, block_height, timestamp FROM batches WHERE batch_id = ?1",
        )?;
        let result = stmt.query_row(params![batch_id as i64], |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, i64>(3)? as u64,
            ))
        });
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::SqliteError(e)),
        }
    }

    // =====================================================================
    // Pending settlements
    // =====================================================================

    pub fn insert_pending(
        &self,
        match_hash: &[u8],
        batch_id: u64,
        buyer_nft_id: u64,
        seller_nft_id: u64,
        symbol: &str,
        fill_quantity: u64,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT INTO pending (match_hash, batch_id, buyer_nft_id, seller_nft_id, symbol, fill_quantity) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![match_hash, batch_id as i64, buyer_nft_id as i64, seller_nft_id as i64, symbol, fill_quantity as i64],
        )?;
        Ok(())
    }

    pub fn get_pending_settlements(&self) -> Result<Vec<(Vec<u8>, u64, String)>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT match_hash, batch_id, status FROM pending WHERE status = 'pending'",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // =====================================================================
    // Registered NFTs (R53)
    // =====================================================================

    pub fn add_registered_nft(
        &self,
        nft_id: u64,
        registered_at: &str,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO registered_nfts (nft_id, registered_at) VALUES (?1, ?2)",
            params![nft_id as i64, registered_at],
        )?;
        Ok(())
    }

    pub fn remove_registered_nft(&self, nft_id: u64) -> Result<(), StorageError> {
        self.conn.execute(
            "DELETE FROM registered_nfts WHERE nft_id = ?1",
            params![nft_id as i64],
        )?;
        Ok(())
    }

    pub fn is_registered_nft(&self, nft_id: u64) -> Result<bool, StorageError> {
        let count: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM registered_nfts WHERE nft_id = ?1",
            params![nft_id as i64],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    // =====================================================================
    // Blocked NFTs
    // =====================================================================

    pub fn add_blocked_nft(
        &self,
        nft_id: u64,
        blocked_at: &str,
        enforcement_at_batch: Option<u64>,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO blocked_nfts (nft_id, blocked_at, enforcement_at_batch) VALUES (?1, ?2, ?3)",
            params![nft_id as i64, blocked_at, enforcement_at_batch.map(|v| v as i64)],
        )?;
        Ok(())
    }

    pub fn is_blocked_nft(&self, nft_id: u64) -> Result<bool, StorageError> {
        let count: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM blocked_nfts WHERE nft_id = ?1",
            params![nft_id as i64],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn list_blocked_nfts(&self) -> Result<Vec<u64>, StorageError> {
        let mut stmt = self.conn.prepare("SELECT nft_id FROM blocked_nfts")?;
        let rows = stmt
            .query_map([], |row| Ok(row.get::<_, i64>(0)? as u64))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // =====================================================================
    // Market pairs
    // =====================================================================

    pub fn upsert_market_pair(
        &self,
        symbol: &str,
        min_quantity: u64,
        base_token_metadata: &str,
        quote_token_metadata: &str,
        base_decimals: u32,
        quote_decimals: u32,
        active: bool,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO market_pairs (symbol, min_quantity, base_token_metadata, quote_token_metadata, base_decimals, quote_decimals, active) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![symbol, min_quantity as i64, base_token_metadata, quote_token_metadata, base_decimals, quote_decimals, active as i32],
        )?;
        Ok(())
    }

    pub fn get_market_pair(
        &self,
        symbol: &str,
    ) -> Result<Option<MarketPairRecord>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT symbol, min_quantity, base_token_metadata, quote_token_metadata, base_decimals, quote_decimals, active FROM market_pairs WHERE symbol = ?1",
        )?;
        let result = stmt.query_row(params![symbol], |row| {
            Ok(MarketPairRecord {
                symbol: row.get(0)?,
                min_quantity: row.get::<_, i64>(1)? as u64,
                base_token_metadata: row.get(2)?,
                quote_token_metadata: row.get(3)?,
                base_decimals: row.get(4)?,
                quote_decimals: row.get(5)?,
                active: row.get::<_, i32>(6)? != 0,
            })
        });
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::SqliteError(e)),
        }
    }

    pub fn list_active_market_pairs(&self) -> Result<Vec<MarketPairRecord>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT symbol, min_quantity, base_token_metadata, quote_token_metadata, base_decimals, quote_decimals, active FROM market_pairs WHERE active = 1 ORDER BY symbol",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(MarketPairRecord {
                    symbol: row.get(0)?,
                    min_quantity: row.get::<_, i64>(1)? as u64,
                    base_token_metadata: row.get(2)?,
                    quote_token_metadata: row.get(3)?,
                    base_decimals: row.get(4)?,
                    quote_decimals: row.get(5)?,
                    active: row.get::<_, i32>(6)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // =====================================================================
    // B4: Settlement records
    // =====================================================================

    pub fn insert_settlement(
        &self,
        match_hash: &[u8],
        batch_id: u64,
        pool_id: u64,
        buyer_nft_id: u64,
        seller_nft_id: u64,
        symbol: &str,
        fill_quantity: u64,
        settlement_price: u64,
        outflow_token: &str,
        outflow_amount: u64,
        inflow_token: &str,
        inflow_amount: u64,
        is_gas_payer: bool,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO settlements (
                match_hash, batch_id, pool_id, buyer_nft_id, seller_nft_id,
                symbol, fill_quantity, settlement_price,
                outflow_token, outflow_amount, inflow_token, inflow_amount,
                is_gas_payer
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                match_hash,
                batch_id as i64,
                pool_id as i64,
                buyer_nft_id as i64,
                seller_nft_id as i64,
                symbol,
                fill_quantity as i64,
                settlement_price as i64,
                outflow_token,
                outflow_amount as i64,
                inflow_token,
                inflow_amount as i64,
                is_gas_payer as i32,
            ],
        )?;
        Ok(())
    }

    /// Update settlement status. Optionally set tx_hash, abort_code, timestamps.
    pub fn update_settlement_status(
        &self,
        match_hash: &[u8],
        status: &str,
        tx_hash: Option<&str>,
        abort_code: Option<&str>,
    ) -> Result<(), StorageError> {
        // Set submitted_at when transitioning to "submitted"
        // Set confirmed_at when transitioning to "confirmed"
        let submitted_at: Option<i64> = if status == "submitted" {
            Some(chrono_now_secs())
        } else {
            None
        };
        let confirmed_at: Option<i64> = if status == "confirmed" {
            Some(chrono_now_secs())
        } else {
            None
        };

        self.conn.execute(
            "UPDATE settlements SET
                status = ?2,
                tx_hash = COALESCE(?3, tx_hash),
                abort_code = ?4,
                submitted_at = COALESCE(?5, submitted_at),
                confirmed_at = COALESCE(?6, confirmed_at)
            WHERE match_hash = ?1",
            params![
                match_hash,
                status,
                tx_hash,
                abort_code,
                submitted_at,
                confirmed_at,
            ],
        )?;
        Ok(())
    }

    /// Get all settlements with status 'pending' or 'submitted' (for crash recovery).
    pub fn get_pending_settlements_v2(&self) -> Result<Vec<SettlementRecord>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT match_hash, batch_id, pool_id, buyer_nft_id, seller_nft_id,
                    symbol, fill_quantity, settlement_price,
                    outflow_token, outflow_amount, inflow_token, inflow_amount,
                    tx_hash, is_gas_payer, status, abort_code,
                    submitted_at, confirmed_at, created_at
             FROM settlements WHERE status IN ('pending', 'submitted')
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| settlement_from_row(row))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get all settlements for a given batch_id.
    pub fn get_settlements_by_batch(&self, batch_id: u64) -> Result<Vec<SettlementRecord>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT match_hash, batch_id, pool_id, buyer_nft_id, seller_nft_id,
                    symbol, fill_quantity, settlement_price,
                    outflow_token, outflow_amount, inflow_token, inflow_amount,
                    tx_hash, is_gas_payer, status, abort_code,
                    submitted_at, confirmed_at, created_at
             FROM settlements WHERE batch_id = ?1
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![batch_id as i64], |row| settlement_from_row(row))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get a single settlement by match_hash.
    pub fn get_settlement(&self, match_hash: &[u8]) -> Result<Option<SettlementRecord>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT match_hash, batch_id, pool_id, buyer_nft_id, seller_nft_id,
                    symbol, fill_quantity, settlement_price,
                    outflow_token, outflow_amount, inflow_token, inflow_amount,
                    tx_hash, is_gas_payer, status, abort_code,
                    submitted_at, confirmed_at, created_at
             FROM settlements WHERE match_hash = ?1",
        )?;
        let result = stmt.query_row(params![match_hash], |row| settlement_from_row(row));
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::SqliteError(e)),
        }
    }

    // =====================================================================
    // B4: Balance snapshots
    // =====================================================================

    pub fn insert_balance_snapshot(
        &self,
        token: &str,
        confirmed: u64,
        pending_out_total: u64,
        pending_in_total: u64,
        earmarked: u64,
        projected: u64,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT INTO balance_snapshots (
                token, confirmed, pending_out_total, pending_in_total,
                earmarked, projected
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                token,
                confirmed as i64,
                pending_out_total as i64,
                pending_in_total as i64,
                earmarked as i64,
                projected as i64,
            ],
        )?;
        Ok(())
    }

    /// Get the most recent balance snapshot for a given token.
    pub fn get_latest_snapshot(&self, token: &str) -> Result<Option<BalanceSnapshot>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, token, confirmed, pending_out_total, pending_in_total,
                    earmarked, projected, snapshot_at
             FROM balance_snapshots
             WHERE token = ?1
             ORDER BY snapshot_at DESC, id DESC
             LIMIT 1",
        )?;
        let result = stmt.query_row(params![token], |row| {
            Ok(BalanceSnapshot {
                id: row.get::<_, i64>(0)? as u64,
                token: row.get(1)?,
                confirmed: row.get::<_, i64>(2)? as u64,
                pending_out_total: row.get::<_, i64>(3)? as u64,
                pending_in_total: row.get::<_, i64>(4)? as u64,
                earmarked: row.get::<_, i64>(5)? as u64,
                projected: row.get::<_, i64>(6)? as u64,
                snapshot_at: row.get::<_, i64>(7)? as u64,
            })
        });
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::SqliteError(e)),
        }
    }
}

// =========================================================================
// Helpers
// =========================================================================

/// Current time in seconds since epoch.
fn chrono_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Deserialize a row into a SettlementRecord.
fn settlement_from_row(row: &rusqlite::Row) -> rusqlite::Result<SettlementRecord> {
    Ok(SettlementRecord {
        match_hash: row.get(0)?,
        batch_id: row.get::<_, i64>(1)? as u64,
        pool_id: row.get::<_, i64>(2)? as u64,
        buyer_nft_id: row.get::<_, i64>(3)? as u64,
        seller_nft_id: row.get::<_, i64>(4)? as u64,
        symbol: row.get(5)?,
        fill_quantity: row.get::<_, i64>(6)? as u64,
        settlement_price: row.get::<_, i64>(7)? as u64,
        outflow_token: row.get(8)?,
        outflow_amount: row.get::<_, i64>(9)? as u64,
        inflow_token: row.get(10)?,
        inflow_amount: row.get::<_, i64>(11)? as u64,
        tx_hash: row.get(12)?,
        is_gas_payer: row.get::<_, i32>(13)? != 0,
        status: row.get(14)?,
        abort_code: row.get(15)?,
        submitted_at: row.get::<_, Option<i64>>(16)?.map(|v| v as u64),
        confirmed_at: row.get::<_, Option<i64>>(17)?.map(|v| v as u64),
        created_at: row.get::<_, i64>(18)? as u64,
    })
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // T_STORE_01
    #[test]
    fn test_create_database() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        let tables = db.list_tables().unwrap();
        assert!(tables.contains(&"pubkeys".into()));
        assert!(tables.contains(&"batches".into()));
        assert!(tables.contains(&"commits".into()));
        assert!(tables.contains(&"pending".into()));
        assert!(tables.contains(&"reputation".into()));
        assert!(tables.contains(&"schema_version".into()));
        assert!(tables.contains(&"registered_nfts".into()));
        assert!(tables.contains(&"blocked_nfts".into()));
        assert!(tables.contains(&"market_pairs".into()));
        // B4 tables
        assert!(tables.contains(&"settlements".into()));
        assert!(tables.contains(&"balance_snapshots".into()));
    }

    // T_STORE_02
    #[test]
    fn test_pubkey_cache() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        // Miss
        assert!(db.get_pubkey(42).unwrap().is_none());

        // Insert
        let pubkey = vec![0xABu8; 32];
        db.cache_pubkey(42, &pubkey, "0xTRUSTEE_ADDR").unwrap();

        // Hit
        let cached = db.get_pubkey(42).unwrap().unwrap();
        assert_eq!(cached.pubkey, pubkey);
        assert_eq!(cached.address, "0xTRUSTEE_ADDR");
    }

    // T_STORE_03: cache hit means no chain call needed
    #[test]
    fn test_pubkey_cache_hit_avoids_chain() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        db.cache_pubkey(42, &vec![0xABu8; 32], "0xADDR").unwrap();

        // Subsequent lookups return cached value
        let cached1 = db.get_pubkey(42).unwrap().unwrap();
        let cached2 = db.get_pubkey(42).unwrap().unwrap();
        assert_eq!(cached1, cached2);
    }

    // T_STORE_04
    #[test]
    fn test_pubkey_cache_persists() {
        let dir = tempdir().unwrap();

        // First session
        {
            let db = Storage::open(dir.path()).unwrap();
            db.cache_pubkey(42, &vec![0xABu8; 32], "0xADDR").unwrap();
        }

        // Second session
        {
            let db = Storage::open(dir.path()).unwrap();
            let cached = db.get_pubkey(42).unwrap().unwrap();
            assert_eq!(cached.pubkey, vec![0xABu8; 32]);
        }
    }

    // T_STORE_05
    #[test]
    fn test_batch_history() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        db.insert_batch(100, 3, "COMMIT", 1003, 1708100000).unwrap();

        let (pool_id, phase, block, ts) = db.get_batch(100).unwrap().unwrap();
        assert_eq!(pool_id, 3);
        assert_eq!(phase, "COMMIT");
        assert_eq!(block, 1003);
        assert_eq!(ts, 1708100000);
    }

    // T_STORE_06
    #[test]
    fn test_pending_settlements() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        let hash = vec![0xAA; 32];
        db.insert_pending(&hash, 100, 42, 99, "EMM/KAY", 500_000_000).unwrap();

        let pending = db.get_pending_settlements().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, hash);
        assert_eq!(pending[0].1, 100);
        assert_eq!(pending[0].2, "pending");
    }

    // T_STORE_07
    #[test]
    fn test_schema_version() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();
        assert_eq!(db.get_schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
    }

    // T_STORE_EXT_01: registered_nfts CRUD
    #[test]
    fn test_registered_nfts_crud() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        // Initially not registered
        assert!(!db.is_registered_nft(42).unwrap());

        // Add
        db.add_registered_nft(42, "2026-02-22T00:00:00Z").unwrap();
        assert!(db.is_registered_nft(42).unwrap());

        // Other nft not affected
        assert!(!db.is_registered_nft(99).unwrap());

        // Remove
        db.remove_registered_nft(42).unwrap();
        assert!(!db.is_registered_nft(42).unwrap());

        // Remove non-existent — no error
        db.remove_registered_nft(999).unwrap();
    }

    // T_STORE_EXT_02: blocked_nfts CRUD
    #[test]
    fn test_blocked_nfts_crud() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        assert!(!db.is_blocked_nft(42).unwrap());
        assert!(db.list_blocked_nfts().unwrap().is_empty());

        db.add_blocked_nft(42, "2026-02-22T00:00:00Z", Some(500)).unwrap();
        db.add_blocked_nft(99, "2026-02-22T01:00:00Z", None).unwrap();

        assert!(db.is_blocked_nft(42).unwrap());
        assert!(db.is_blocked_nft(99).unwrap());
        assert!(!db.is_blocked_nft(1).unwrap());

        let blocked = db.list_blocked_nfts().unwrap();
        assert_eq!(blocked.len(), 2);
        assert!(blocked.contains(&42));
        assert!(blocked.contains(&99));
    }

    // T_STORE_EXT_03: market_pairs CRUD
    #[test]
    fn test_market_pairs_crud() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        // Initially empty
        assert!(db.get_market_pair("EMM/KAY").unwrap().is_none());
        assert!(db.list_active_market_pairs().unwrap().is_empty());

        // Add
        db.upsert_market_pair(
            "EMM/KAY", 1_000_000,
            "0xEMM_META", "0xKAY_META",
            5, 5, true,
        ).unwrap();

        let pair = db.get_market_pair("EMM/KAY").unwrap().unwrap();
        assert_eq!(pair.symbol, "EMM/KAY");
        assert_eq!(pair.min_quantity, 1_000_000);
        assert_eq!(pair.base_decimals, 5);
        assert_eq!(pair.quote_decimals, 5);
        assert!(pair.active);

        // Add another, inactive
        db.upsert_market_pair(
            "KAY/TEE", 100_000,
            "0xTEE_META", "0xKAY_META",
            5, 5, false,
        ).unwrap();

        // list_active only returns active ones
        let active = db.list_active_market_pairs().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].symbol, "EMM/KAY");

        // Upsert to activate
        db.upsert_market_pair(
            "KAY/TEE", 100_000,
            "0xTEE_META", "0xKAY_META",
            5, 5, true,
        ).unwrap();
        let active = db.list_active_market_pairs().unwrap();
        assert_eq!(active.len(), 2);
    }

    // =====================================================================
    // B4 storage extension tests
    // =====================================================================

    // T_STORE_B4_01: Insert settlement record — all fields stored
    #[test]
    fn t_store_b4_01_insert_settlement() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        let hash = vec![0xCC; 32];
        db.insert_settlement(
            &hash, 100, 3, 42, 99, "EMM/KAY", 5000, 4_900_000,
            "KAY", 250, "EMM", 5000, true,
        ).unwrap();

        let pending = db.get_pending_settlements_v2().unwrap();
        assert_eq!(pending.len(), 1);
        let rec = &pending[0];
        assert_eq!(rec.match_hash, hash);
        assert_eq!(rec.batch_id, 100);
        assert_eq!(rec.pool_id, 3);
        assert_eq!(rec.buyer_nft_id, 42);
        assert_eq!(rec.seller_nft_id, 99);
        assert_eq!(rec.symbol, "EMM/KAY");
        assert_eq!(rec.fill_quantity, 5000);
        assert_eq!(rec.settlement_price, 4_900_000);
        assert_eq!(rec.outflow_token, "KAY");
        assert_eq!(rec.outflow_amount, 250);
        assert_eq!(rec.inflow_token, "EMM");
        assert_eq!(rec.inflow_amount, 5000);
        assert!(rec.is_gas_payer);
        assert_eq!(rec.status, "pending");
        assert!(rec.tx_hash.is_none());
        assert!(rec.abort_code.is_none());
    }

    // T_STORE_B4_02: Update settlement status transitions
    #[test]
    fn t_store_b4_02_update_settlement_status() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        let hash = vec![0xCC; 32];
        db.insert_settlement(
            &hash, 100, 3, 42, 99, "EMM/KAY", 5000, 4_900_000,
            "KAY", 250, "EMM", 5000, true,
        ).unwrap();

        // pending → submitted (with tx_hash)
        db.update_settlement_status(&hash, "submitted", Some("0xTX123"), None).unwrap();
        let rec = db.get_settlement(&hash).unwrap().unwrap();
        assert_eq!(rec.status, "submitted");
        assert_eq!(rec.tx_hash.as_deref(), Some("0xTX123"));
        assert!(rec.submitted_at.is_some());
        assert!(rec.confirmed_at.is_none());

        // submitted → confirmed
        db.update_settlement_status(&hash, "confirmed", None, None).unwrap();
        let rec = db.get_settlement(&hash).unwrap().unwrap();
        assert_eq!(rec.status, "confirmed");
        assert!(rec.confirmed_at.is_some());
        // tx_hash preserved from earlier update
        assert_eq!(rec.tx_hash.as_deref(), Some("0xTX123"));
    }

    // T_STORE_B4_03: Get pending settlements for crash recovery
    #[test]
    fn t_store_b4_03_get_pending_for_recovery() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        // Insert 3 settlements with different statuses
        db.insert_settlement(
            &[0x01; 32], 100, 3, 42, 99, "EMM/KAY", 1000, 4_900_000,
            "KAY", 50, "EMM", 1000, true,
        ).unwrap();
        db.insert_settlement(
            &[0x02; 32], 100, 3, 42, 88, "EMM/KAY", 2000, 4_900_000,
            "KAY", 100, "EMM", 2000, false,
        ).unwrap();
        db.insert_settlement(
            &[0x03; 32], 100, 3, 42, 77, "EMM/KAY", 3000, 4_900_000,
            "KAY", 150, "EMM", 3000, true,
        ).unwrap();

        // Mark one as submitted, one as confirmed
        db.update_settlement_status(&[0x02; 32], "submitted", Some("0xTX2"), None).unwrap();
        db.update_settlement_status(&[0x03; 32], "confirmed", Some("0xTX3"), None).unwrap();

        // Crash recovery: get pending + submitted (not confirmed)
        let recoverable = db.get_pending_settlements_v2().unwrap();
        assert_eq!(recoverable.len(), 2);
        assert_eq!(recoverable[0].match_hash, vec![0x01; 32]); // pending
        assert_eq!(recoverable[1].match_hash, vec![0x02; 32]); // submitted
    }

    // T_STORE_B4_04: Get settlements by batch_id
    #[test]
    fn t_store_b4_04_get_settlements_by_batch() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        // Insert across 2 batches
        db.insert_settlement(
            &[0x01; 32], 100, 3, 42, 99, "EMM/KAY", 1000, 4_900_000,
            "KAY", 50, "EMM", 1000, true,
        ).unwrap();
        db.insert_settlement(
            &[0x02; 32], 100, 3, 42, 88, "EMM/KAY", 2000, 4_900_000,
            "KAY", 100, "EMM", 2000, false,
        ).unwrap();
        db.insert_settlement(
            &[0x03; 32], 101, 3, 42, 77, "EMM/KAY", 3000, 4_900_000,
            "KAY", 150, "EMM", 3000, true,
        ).unwrap();

        let batch_100 = db.get_settlements_by_batch(100).unwrap();
        assert_eq!(batch_100.len(), 2);

        let batch_101 = db.get_settlements_by_batch(101).unwrap();
        assert_eq!(batch_101.len(), 1);
        assert_eq!(batch_101[0].match_hash, vec![0x03; 32]);

        let batch_999 = db.get_settlements_by_batch(999).unwrap();
        assert!(batch_999.is_empty());
    }

    // T_STORE_B4_05: Insert balance snapshot
    #[test]
    fn t_store_b4_05_insert_balance_snapshot() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        db.insert_balance_snapshot("KAY", 10_000, 300, 0, 100, 9_600).unwrap();

        let snap = db.get_latest_snapshot("KAY").unwrap().unwrap();
        assert_eq!(snap.token, "KAY");
        assert_eq!(snap.confirmed, 10_000);
        assert_eq!(snap.pending_out_total, 300);
        assert_eq!(snap.pending_in_total, 0);
        assert_eq!(snap.earmarked, 100);
        assert_eq!(snap.projected, 9_600);
        assert!(snap.snapshot_at > 0);
    }

    // T_STORE_B4_06: Get latest balance snapshot per token
    #[test]
    fn t_store_b4_06_latest_snapshot_per_token() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        // Insert 3 snapshots for KAY (IDs will be 1, 2, 3)
        db.insert_balance_snapshot("KAY", 10_000, 300, 0, 100, 9_600).unwrap();
        db.insert_balance_snapshot("KAY", 11_000, 200, 0, 50, 10_750).unwrap();
        db.insert_balance_snapshot("KAY", 12_000, 0, 0, 0, 12_000).unwrap();

        // Also insert one for EMM
        db.insert_balance_snapshot("EMM", 50_000, 1_000, 0, 0, 49_000).unwrap();

        // Latest KAY should be the third one
        let snap = db.get_latest_snapshot("KAY").unwrap().unwrap();
        assert_eq!(snap.confirmed, 12_000);
        assert_eq!(snap.projected, 12_000);

        // Latest EMM should be the only one
        let snap = db.get_latest_snapshot("EMM").unwrap().unwrap();
        assert_eq!(snap.confirmed, 50_000);

        // Nonexistent token → None
        assert!(db.get_latest_snapshot("NONEXISTENT").unwrap().is_none());
    }

    // T_STORE_B4_07: Expired settlements queryable and updatable
    #[test]
    fn t_store_b4_07_expired_settlement_status() {
        let dir = tempdir().unwrap();
        let db = Storage::open(dir.path()).unwrap();

        db.insert_settlement(
            &[0x01; 32], 100, 3, 42, 99, "EMM/KAY", 1000, 4_900_000,
            "KAY", 50, "EMM", 1000, true,
        ).unwrap();

        // Mark as expired
        db.update_settlement_status(&[0x01; 32], "expired", None, None).unwrap();

        // Expired settlements NOT returned by get_pending_settlements_v2
        let pending = db.get_pending_settlements_v2().unwrap();
        assert!(pending.is_empty());

        // But still queryable via get_settlement for audit
        let rec = db.get_settlement(&[0x01; 32]).unwrap().unwrap();
        assert_eq!(rec.status, "expired");

        // And via batch query
        let batch = db.get_settlements_by_batch(100).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].status, "expired");
    }
}
