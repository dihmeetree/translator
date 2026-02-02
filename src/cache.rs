//! Persistent translation cache using SQLite.
//!
//! Stores translations in a local SQLite database with automatic size management.
//! When the database exceeds the configured size limit, oldest entries are purged.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// Number of inserts between cache size checks.
/// Avoids running a SQLite size query on every single insert.
const CLEANUP_CHECK_INTERVAL: u32 = 50;

/// Percentage of entries to remove when cache is full (20%).
const CLEANUP_PERCENTAGE: f64 = 0.20;

/// Number of cleanup operations before running VACUUM.
/// VACUUM is expensive, so we batch it to run less frequently.
const VACUUM_FREQUENCY: u32 = 5;

/// Result of a translation lookup.
#[derive(Debug, Clone)]
pub struct CachedTranslation {
    /// The translated text.
    pub translation: String,
    /// Romanization/pronunciation of the source text (if available).
    pub romanization: Option<String>,
}

/// Persistent translation cache backed by SQLite.
#[derive(Debug, Clone)]
pub struct TranslationCache {
    /// Database connection (shared across clones).
    conn: Arc<Mutex<Connection>>,
    /// Maximum cache size in bytes.
    max_size_bytes: u64,
    /// Counter for cleanup operations to batch VACUUM calls.
    cleanup_count: Arc<AtomicU32>,
    /// Counter for inserts to rate-limit cleanup size checks.
    insert_count: Arc<AtomicU32>,
}

impl TranslationCache {
    /// Creates a new translation cache with the specified size limit.
    ///
    /// The database is stored in the current directory as `translation_cache.db`.
    ///
    /// # Arguments
    ///
    /// * `max_size_bytes` - Maximum cache size in bytes. When exceeded, oldest entries are purged.
    pub fn with_max_size(max_size_bytes: u64) -> Result<Self> {
        let db_path = get_cache_path()?;

        // Ensure parent directory exists
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).context("Failed to create cache directory")?;
        }

        let conn =
            Connection::open(&db_path).context("Failed to open translation cache database")?;

        // Enable WAL mode for better concurrent read/write performance:
        // readers don't block writers and vice versa.
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        // NORMAL sync is sufficient for a cache — losing the last
        // transaction on a power failure is acceptable.
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;

        // Initialize schema
        // Note: UNIQUE constraint on (source_text, source_lang) automatically creates an index,
        // so we don't need a separate idx_translations_lookup index.
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS translations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_text TEXT NOT NULL,
                source_lang TEXT NOT NULL,
                translation TEXT NOT NULL,
                romanization TEXT,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
                last_accessed INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
                UNIQUE(source_text, source_lang)
            );

            CREATE INDEX IF NOT EXISTS idx_translations_last_accessed
                ON translations(last_accessed);
            ",
        )
        .context("Failed to initialize cache schema")?;

        info!("Translation cache initialized at {:?}", db_path);

        let cache = Self {
            conn: Arc::new(Mutex::new(conn)),
            max_size_bytes,
            cleanup_count: Arc::new(AtomicU32::new(0)),
            insert_count: Arc::new(AtomicU32::new(0)),
        };

        // Check size on startup and cleanup if needed
        if let Err(e) = cache.cleanup_if_needed() {
            warn!("Failed to cleanup cache on startup: {}", e);
        }

        Ok(cache)
    }

    /// Gets a cached translation if it exists.
    ///
    /// Updates the last_accessed timestamp on hit.
    pub fn get(&self, text: &str, source_lang: &str) -> Option<CachedTranslation> {
        let conn = self.conn.lock().expect("cache mutex poisoned");

        // Try to fetch and update last_accessed in one go
        let result = conn.query_row(
            "UPDATE translations
             SET last_accessed = strftime('%s', 'now')
             WHERE source_text = ?1 AND source_lang = ?2
             RETURNING translation, romanization",
            params![text, source_lang],
            |row| {
                Ok(CachedTranslation {
                    translation: row.get(0)?,
                    romanization: row.get(1)?,
                })
            },
        );

        match result {
            Ok(cached) => {
                debug!("Cache hit for '{}' ({})", text, source_lang);
                Some(cached)
            }
            Err(_) => None,
        }
    }

    /// Inserts a translation into the cache.
    ///
    /// If the entry already exists, it updates the translation and timestamps.
    pub fn insert(
        &self,
        text: &str,
        source_lang: &str,
        translation: &str,
        romanization: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("cache mutex poisoned");

        conn.execute(
            "INSERT INTO translations (source_text, source_lang, translation, romanization)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(source_text, source_lang) DO UPDATE SET
                translation = excluded.translation,
                romanization = excluded.romanization,
                last_accessed = strftime('%s', 'now')",
            params![text, source_lang, translation, romanization],
        )
        .context("Failed to insert translation into cache")?;

        drop(conn);

        // Rate-limit cleanup size checks to every CLEANUP_CHECK_INTERVAL inserts
        // to avoid running a SQLite pragma query on every single insert.
        if self
            .insert_count
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(CLEANUP_CHECK_INTERVAL)
        {
            if let Err(e) = self.cleanup_if_needed() {
                warn!("Failed to cleanup cache: {}", e);
            }
        }

        Ok(())
    }

    /// Returns the current size of the cache database in bytes.
    pub fn size_bytes(&self) -> Result<u64> {
        let conn = self.conn.lock().expect("cache mutex poisoned");

        let size: i64 = conn
            .query_row(
                "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                [],
                |row| row.get(0),
            )
            .context("Failed to get cache size")?;

        Ok(size as u64)
    }

    /// Cleans up old entries if the cache exceeds the size limit.
    ///
    /// VACUUM is batched to run every `VACUUM_FREQUENCY` cleanups to reduce overhead.
    fn cleanup_if_needed(&self) -> Result<()> {
        let current_size = self.size_bytes()?;

        if current_size <= self.max_size_bytes {
            return Ok(());
        }

        debug!(
            "Cache size ({:.2} MB) exceeds limit ({:.2} MB), cleaning up...",
            current_size as f64 / 1024.0 / 1024.0,
            self.max_size_bytes as f64 / 1024.0 / 1024.0
        );

        let conn = self.conn.lock().expect("cache mutex poisoned");

        // Get total count
        let total_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM translations", [], |row| row.get(0))?;

        // Calculate how many entries to remove
        let remove_count = (total_count as f64 * CLEANUP_PERCENTAGE).ceil() as i64;

        if remove_count > 0 {
            // Delete oldest entries by last_accessed
            conn.execute(
                "DELETE FROM translations WHERE id IN (
                    SELECT id FROM translations
                    ORDER BY last_accessed ASC
                    LIMIT ?1
                )",
                params![remove_count],
            )?;

            // Increment cleanup counter and VACUUM only every VACUUM_FREQUENCY cleanups
            let count = self.cleanup_count.fetch_add(1, Ordering::Relaxed) + 1;
            if count.is_multiple_of(VACUUM_FREQUENCY) {
                conn.execute("VACUUM", [])?;
                debug!(
                    "Removed {} old cache entries and ran VACUUM (cleanup #{})",
                    remove_count, count
                );
            } else {
                let remaining = VACUUM_FREQUENCY - (count % VACUUM_FREQUENCY);
                debug!(
                    "Removed {} old cache entries (cleanup #{}, VACUUM in {})",
                    remove_count, count, remaining
                );
            }
        }

        Ok(())
    }
}

/// Gets the path to the cache database file.
///
/// Uses `./data/translation_cache.db` if the `data/` directory exists
/// (e.g., a mounted volume), otherwise falls back to the current directory.
fn get_cache_path() -> Result<PathBuf> {
    let data_dir = PathBuf::from("data");
    if data_dir.is_dir() {
        Ok(data_dir.join("translation_cache.db"))
    } else {
        Ok(PathBuf::from("translation_cache.db"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a test cache using an in-memory SQLite database.
    fn create_test_cache() -> TranslationCache {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS translations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_text TEXT NOT NULL,
                source_lang TEXT NOT NULL,
                translation TEXT NOT NULL,
                romanization TEXT,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
                last_accessed INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
                UNIQUE(source_text, source_lang)
            );
            ",
        )
        .unwrap();

        TranslationCache {
            conn: Arc::new(Mutex::new(conn)),
            max_size_bytes: 50 * 1024 * 1024, // 50 MB
            cleanup_count: Arc::new(AtomicU32::new(0)),
            insert_count: Arc::new(AtomicU32::new(0)),
        }
    }

    #[test]
    fn test_insert_and_get() {
        let cache = create_test_cache();

        cache
            .insert("привет", "ru", "hello", Some("privet"))
            .unwrap();

        let result = cache.get("привет", "ru");
        assert!(result.is_some());

        let cached = result.unwrap();
        assert_eq!(cached.translation, "hello");
        assert_eq!(cached.romanization, Some("privet".to_string()));
    }

    #[test]
    fn test_cache_miss() {
        let cache = create_test_cache();

        let result = cache.get("nonexistent", "en");
        assert!(result.is_none());
    }

    #[test]
    fn test_update_existing() {
        let cache = create_test_cache();

        cache.insert("test", "en", "first", None).unwrap();
        cache.insert("test", "en", "second", Some("rom")).unwrap();

        let result = cache.get("test", "en").unwrap();
        assert_eq!(result.translation, "second");
        assert_eq!(result.romanization, Some("rom".to_string()));
    }
}
