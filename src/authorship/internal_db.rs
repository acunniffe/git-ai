// DEPRECATED: The internal DB is deprecated and in the process of being removed.
// It has been superseded by use-case-specific databases.

use crate::error::GitAiError;
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Current schema version (must match MIGRATIONS.len())
const SCHEMA_VERSION: usize = 3;

/// Database migrations - each migration upgrades the schema by one version
/// Migration at index N upgrades from version N to version N+1
const MIGRATIONS: &[&str] = &[
    // Migration 0 -> 1: Initial schema with prompts table
    r#"
    CREATE TABLE IF NOT EXISTS prompts (
        id TEXT PRIMARY KEY NOT NULL,
        workdir TEXT,
        tool TEXT NOT NULL,
        model TEXT NOT NULL,
        external_thread_id TEXT NOT NULL,
        messages TEXT NOT NULL,
        commit_sha TEXT,
        agent_metadata TEXT,
        human_author TEXT,
        total_additions INTEGER,
        total_deletions INTEGER,
        accepted_lines INTEGER,
        overridden_lines INTEGER,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_prompts_tool
        ON prompts(tool);
    CREATE INDEX IF NOT EXISTS idx_prompts_external_thread_id
        ON prompts(external_thread_id);
    CREATE INDEX IF NOT EXISTS idx_prompts_workdir
        ON prompts(workdir);
    CREATE INDEX IF NOT EXISTS idx_prompts_commit_sha
        ON prompts(commit_sha);
    CREATE INDEX IF NOT EXISTS idx_prompts_updated_at
        ON prompts(updated_at);
    "#,
    // Migration 1 -> 2: Add CAS sync queue
    r#"
    CREATE TABLE IF NOT EXISTS cas_sync_queue (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        hash TEXT NOT NULL UNIQUE,
        data TEXT NOT NULL,
        metadata TEXT NOT NULL DEFAULT '{}',
        status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending', 'processing')),
        attempts INTEGER NOT NULL DEFAULT 0,
        last_sync_error TEXT,
        last_sync_at INTEGER,
        next_retry_at INTEGER NOT NULL,
        processing_started_at INTEGER,
        created_at INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_cas_sync_queue_status_retry
        ON cas_sync_queue(status, next_retry_at);
    CREATE INDEX IF NOT EXISTS idx_cas_sync_queue_hash
        ON cas_sync_queue(hash);
    CREATE INDEX IF NOT EXISTS idx_cas_sync_queue_stale_processing
        ON cas_sync_queue(processing_started_at) WHERE status = 'processing';
    "#,
    // Migration 2 -> 3: Add CAS cache for fetched prompts
    r#"
    CREATE TABLE IF NOT EXISTS cas_cache (
        hash TEXT PRIMARY KEY NOT NULL,
        messages TEXT NOT NULL,
        cached_at INTEGER NOT NULL
    );
    "#,
];

/// Global database singleton
static INTERNAL_DB: OnceLock<Mutex<InternalDatabase>> = OnceLock::new();

/// CAS sync queue record
#[derive(Debug, Clone)]
pub struct CasSyncRecord {
    pub id: i64,
    pub hash: String,
    pub data: String,
    pub metadata: HashMap<String, String>,
    pub attempts: u32,
}

/// Database wrapper for internal git-ai storage
pub struct InternalDatabase {
    conn: Connection,
    _db_path: PathBuf,
}

impl InternalDatabase {
    /// Get or initialize the global database
    pub fn global() -> Result<&'static Mutex<InternalDatabase>, GitAiError> {
        // Use get_or_init (stable) instead of get_or_try_init (unstable)
        // Errors during initialization will be logged and returned as Err
        let db_mutex = INTERNAL_DB.get_or_init(|| {
            match Self::new() {
                Ok(db) => Mutex::new(db),
                Err(e) => {
                    // Log error during initialization
                    eprintln!("[Error] Failed to initialize internal database: {}", e);
                    crate::observability::log_error(
                        &e,
                        Some(serde_json::json!({"function": "InternalDatabase::global"})),
                    );
                    // Create a dummy connection that will fail on any operation
                    // This allows the program to continue even if DB init fails
                    let temp_path = std::env::temp_dir().join("git-ai-db-failed");
                    let conn = crate::sqlite::open_with_memory_limits(&temp_path)
                        .expect("Failed to create temp DB");
                    Mutex::new(InternalDatabase {
                        conn,
                        _db_path: temp_path,
                    })
                }
            }
        });

        Ok(db_mutex)
    }

    /// Start database initialization in a background thread.
    /// This allows the main thread to continue with other work while
    /// the database connection and schema migrations are prepared.
    ///
    /// The OnceLock guarantees thread-safe initialization - if warmup
    /// completes before any caller needs the DB, they get instant access.
    /// If a caller needs DB before warmup completes, they wait normally.
    pub fn warmup() {
        std::thread::spawn(|| {
            if let Err(e) = Self::global() {
                tracing::debug!("DB warmup failed: {}", e);
            }
        });
    }

    /// Create a new database connection
    fn new() -> Result<Self, GitAiError> {
        let db_path = Self::database_path()?;

        // Ensure parent directory exists
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Open with WAL mode and performance optimizations
        let conn = crate::sqlite::open_with_memory_limits(&db_path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA temp_store=MEMORY;
            "#,
        )?;

        let mut db = Self {
            conn,
            _db_path: db_path,
        };
        db.initialize_schema()?;

        Ok(db)
    }

    /// Get database path: ~/.git-ai/internal/db
    /// In test mode, can be overridden via GIT_AI_TEST_DB_PATH environment variable.
    /// We also support GITAI_TEST_DB_PATH because some git hook execution paths
    /// may scrub custom GIT_* variables.
    fn database_path() -> Result<PathBuf, GitAiError> {
        // Allow test override via environment variable
        #[cfg(any(test, feature = "test-support"))]
        if let Ok(test_path) =
            std::env::var("GIT_AI_TEST_DB_PATH").or_else(|_| std::env::var("GITAI_TEST_DB_PATH"))
        {
            return Ok(PathBuf::from(test_path));
        }

        let internal = crate::config::internal_dir_path().ok_or_else(|| {
            GitAiError::Generic("Could not determine git-ai internal directory".to_string())
        })?;
        Ok(internal.join("db"))
    }

    /// Test-only accessor for the resolved on-disk database path, so the
    /// integration test crate can assert `GIT_AI_INTERNAL_DIR` routing against the
    /// real resolver. `database_path()` is a pure path computation (it creates no
    /// files), so this has no side effects. Mirrors
    /// `DaemonConfig::from_internal_dir_for_test`.
    #[cfg(feature = "test-support")]
    pub fn database_path_for_test() -> Result<PathBuf, GitAiError> {
        Self::database_path()
    }

    /// Initialize schema and handle migrations
    /// This is the ONLY place where schema changes should be made
    /// Failures are FATAL - the program cannot continue without a valid database
    fn initialize_schema(&mut self) -> Result<(), GitAiError> {
        // FAST PATH: Check if database is already at current version
        // This avoids expensive schema operations on every process start
        let version_check: Result<usize, _> = self.conn.query_row(
            "SELECT value FROM schema_metadata WHERE key = 'version'",
            [],
            |row| {
                let version_str: String = row.get(0)?;
                version_str
                    .parse::<usize>()
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            },
        );

        if let Ok(current_version) = version_check {
            if current_version == SCHEMA_VERSION {
                // Database is up-to-date, no migrations needed
                return Ok(());
            }
            if current_version > SCHEMA_VERSION {
                // Forward-compatible: an older binary can still read/write
                // known tables even if a newer binary added extra tables.
                // Just skip migrations and use what we have.
                return Ok(());
            }
            // Fall through to apply missing migrations (current_version < SCHEMA_VERSION)
        }
        // If query failed, table doesn't exist - proceed with full initialization

        // Step 1: Create schema_metadata table (this is the only table we create directly)
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            "#,
        )?;

        // Step 2: Get current schema version (0 if brand new database)
        let current_version: usize = self
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| {
                    let version_str: String = row.get(0)?;
                    version_str
                        .parse::<usize>()
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
                },
            )
            .unwrap_or(0); // Default to version 0 for new databases

        // Step 3: Apply all missing migrations sequentially
        for target_version in current_version..SCHEMA_VERSION {
            tracing::debug!(
                "[Migration] Upgrading database from version {} to {}",
                target_version,
                target_version + 1
            );

            // Apply the migration (FATAL on error)
            self.apply_migration(target_version)?;

            // Use an upsert so concurrent initializers do not race on version row creation.
            self.conn.execute(
                r#"
                INSERT INTO schema_metadata (key, value)
                VALUES ('version', ?1)
                ON CONFLICT(key) DO UPDATE SET
                    value = excluded.value
                WHERE CAST(schema_metadata.value AS INTEGER) < CAST(excluded.value AS INTEGER)
                "#,
                params![(target_version + 1).to_string()],
            )?;

            tracing::debug!(
                "[Migration] Successfully upgraded to version {}",
                target_version + 1
            );
        }

        // Step 5: Verify final version matches expected
        let final_version: usize = self.conn.query_row(
            "SELECT value FROM schema_metadata WHERE key = 'version'",
            [],
            |row| {
                let version_str: String = row.get(0)?;
                version_str
                    .parse::<usize>()
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            },
        )?;

        if final_version != SCHEMA_VERSION {
            return Err(GitAiError::Generic(format!(
                "Migration failed: expected version {} but got version {}",
                SCHEMA_VERSION, final_version
            )));
        }

        Ok(())
    }

    /// Apply a single migration
    /// Migration failures are FATAL - the program cannot continue with a partially migrated database
    fn apply_migration(&mut self, from_version: usize) -> Result<(), GitAiError> {
        if from_version >= MIGRATIONS.len() {
            return Err(GitAiError::Generic(format!(
                "No migration defined for version {} -> {}",
                from_version,
                from_version + 1
            )));
        }

        let migration_sql = MIGRATIONS[from_version];

        // Execute migration in a transaction for atomicity
        let tx = self.conn.transaction()?;
        tx.execute_batch(migration_sql)?;
        tx.commit()?;

        Ok(())
    }

    /// Enqueue a CAS object for syncing
    ///
    /// Takes raw JSON data, canonicalizes it (RFC 8785), computes SHA256 hash,
    /// and stores both in the queue.
    ///
    /// Returns the hash of the canonicalized content.
    pub fn enqueue_cas_object(
        &mut self,
        json_data: &serde_json::Value,
        metadata: Option<&HashMap<String, String>>,
    ) -> Result<String, GitAiError> {
        use sha2::{Digest, Sha256};

        // Canonicalize JSON (RFC 8785)
        let canonical = serde_json_canonicalizer::to_string(json_data)
            .map_err(|e| GitAiError::Generic(format!("Failed to canonicalize JSON: {}", e)))?;

        // Hash the canonicalized content
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        let hash = format!("{:x}", hasher.finalize());

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let metadata_json = serde_json::to_string(metadata.unwrap_or(&HashMap::new()))?;

        self.conn.execute(
            r#"
            INSERT OR IGNORE INTO cas_sync_queue (
                hash, data, metadata, status, attempts, next_retry_at, created_at
            ) VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?4)
            "#,
            params![hash, canonical, metadata_json, now],
        )?;

        Ok(hash)
    }

    /// Dequeue a batch of CAS objects for syncing (with lock acquisition)
    pub fn dequeue_cas_batch(
        &mut self,
        batch_size: usize,
    ) -> Result<Vec<CasSyncRecord>, GitAiError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Step 1: Recover stale locks (processing for >10 minutes)
        let stale_threshold = now - 600; // 10 minutes
        self.conn.execute(
            r#"
            UPDATE cas_sync_queue
            SET status = 'pending', processing_started_at = NULL
            WHERE status = 'processing'
              AND processing_started_at < ?1
            "#,
            params![stale_threshold],
        )?;

        // Step 2: Atomically lock and fetch batch using UPDATE...RETURNING
        // Note: SQLite's UPDATE...RETURNING is atomic
        let mut stmt = self.conn.prepare(
            r#"
            UPDATE cas_sync_queue
            SET status = 'processing', processing_started_at = ?1
            WHERE id IN (
                SELECT id FROM cas_sync_queue
                WHERE status = 'pending'
                  AND next_retry_at <= ?2
                  AND attempts < 6
                ORDER BY next_retry_at
                LIMIT ?3
            )
            RETURNING id, hash, data, metadata, attempts
            "#,
        )?;

        let rows = stmt.query_map(params![now, now, batch_size], |row| {
            let metadata_json: String = row.get(3)?;
            let metadata: HashMap<String, String> =
                serde_json::from_str(&metadata_json).unwrap_or_default();
            let hash: String = row.get(1)?;
            let data: String = row.get(2)?;
            Ok(CasSyncRecord {
                id: row.get(0)?,
                hash,
                data,
                attempts: row.get(4)?,
                metadata,
            })
        })?;

        let mut records = Vec::new();
        for row in rows {
            records.push(row?);
        }

        Ok(records)
    }

    /// Delete a CAS sync record (on successful sync)
    pub fn delete_cas_sync_record(&mut self, id: i64) -> Result<(), GitAiError> {
        self.conn
            .execute("DELETE FROM cas_sync_queue WHERE id = ?", params![id])?;
        Ok(())
    }

    /// Delete CAS sync records by their content hashes (used by daemon after successful upload).
    pub fn delete_cas_by_hashes(&mut self, hashes: &[String]) -> Result<usize, GitAiError> {
        if hashes.is_empty() {
            return Ok(0);
        }
        let placeholders: Vec<&str> = hashes.iter().map(|_| "?").collect();
        let sql = format!(
            "DELETE FROM cas_sync_queue WHERE hash IN ({})",
            placeholders.join(",")
        );
        let params: Vec<&dyn rusqlite::ToSql> =
            hashes.iter().map(|h| h as &dyn rusqlite::ToSql).collect();
        let deleted = self.conn.execute(&sql, params.as_slice())?;
        Ok(deleted)
    }

    /// Get cached CAS messages by hash
    pub fn get_cas_cache(&self, hash: &str) -> Result<Option<String>, GitAiError> {
        let result = self.conn.query_row(
            "SELECT messages FROM cas_cache WHERE hash = ?1",
            params![hash],
            |row| row.get::<_, String>(0),
        );

        match result {
            Ok(messages) => Ok(Some(messages)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Cache CAS messages by hash (INSERT OR REPLACE since content is immutable)
    pub fn set_cas_cache(&mut self, hash: &str, messages_json: &str) -> Result<(), GitAiError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        self.conn.execute(
            "INSERT OR REPLACE INTO cas_cache (hash, messages, cached_at) VALUES (?1, ?2, ?3)",
            params![hash, messages_json, now],
        )?;

        Ok(())
    }

    /// Update CAS sync record on failure (release lock, increment attempts, set next retry)
    pub fn update_cas_sync_failure(&mut self, id: i64, error: &str) -> Result<(), GitAiError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Get current attempts count to calculate next retry
        let attempts: u32 = self.conn.query_row(
            "SELECT attempts FROM cas_sync_queue WHERE id = ?",
            params![id],
            |row| row.get(0),
        )?;

        let next_retry = calculate_next_retry(attempts + 1, now);

        self.conn.execute(
            r#"
            UPDATE cas_sync_queue
            SET status = 'pending',
                processing_started_at = NULL,
                attempts = attempts + 1,
                last_sync_error = ?1,
                last_sync_at = ?2,
                next_retry_at = ?3
            WHERE id = ?4
            "#,
            params![error, now, next_retry, id],
        )?;

        Ok(())
    }
}

/// Calculate next retry timestamp based on attempt number
fn calculate_next_retry(attempts: u32, now: i64) -> i64 {
    let delay_seconds = match attempts {
        1 => 5 * 60,       // 5 minutes
        2 => 30 * 60,      // 30 minutes
        3 => 2 * 60 * 60,  // 2 hours
        4 => 6 * 60 * 60,  // 6 hours
        5 => 12 * 60 * 60, // 12 hours
        _ => 24 * 60 * 60, // 24 hours (attempts >= 6)
    };
    now + delay_seconds
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_db() -> (InternalDatabase, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();

        let mut db = InternalDatabase {
            conn,
            _db_path: db_path.clone(),
        };
        db.initialize_schema().unwrap();

        (db, temp_dir)
    }

    #[test]
    fn test_initialize_schema() {
        let (db, _temp_dir) = create_test_db();

        // Verify tables exist
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='prompts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Verify schema_metadata exists
        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "3");
    }

    #[test]
    fn test_initialize_schema_handles_preexisting_cas_cache_table() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("concurrent-init.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();

        // Simulate a partial migration state from a concurrent process:
        // schema version indicates cas_cache is missing, but the table already exists.
        conn.execute_batch(
            r#"
            CREATE TABLE schema_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            INSERT INTO schema_metadata (key, value) VALUES ('version', '2');
            CREATE TABLE cas_cache (
                hash TEXT PRIMARY KEY NOT NULL,
                messages TEXT NOT NULL,
                cached_at INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();

        let mut db = InternalDatabase {
            conn,
            _db_path: db_path,
        };
        db.initialize_schema().unwrap();

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "3");
    }

    #[test]
    fn test_database_path() {
        let override_path = std::env::var("GIT_AI_TEST_DB_PATH").ok();
        let path = InternalDatabase::database_path().unwrap();
        if let Some(override_path) = override_path {
            assert_eq!(path, PathBuf::from(override_path));
        } else {
            assert!(path.to_string_lossy().contains(".git-ai"));
            assert!(path.to_string_lossy().contains("internal"));
            assert!(path.to_string_lossy().ends_with("db"));
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_database_path_honors_git_ai_internal_dir() {
        // Save and clear the env vars this test mutates so it can't leak state into
        // other in-process tests (env mutation is process-global). Restored on exit.
        // The authorship DB override honors both the new and legacy var names, so clear
        // both to expose the GIT_AI_INTERNAL_DIR reroute branch.
        let prev_internal = std::env::var_os("GIT_AI_INTERNAL_DIR");
        let prev_test_override = std::env::var_os("GIT_AI_TEST_DB_PATH");
        let prev_legacy_override = std::env::var_os("GITAI_TEST_DB_PATH");
        // SAFETY: this test is serialized via #[serial], so no other test is reading
        // or writing these env vars concurrently.
        unsafe {
            std::env::remove_var("GIT_AI_TEST_DB_PATH");
            std::env::remove_var("GITAI_TEST_DB_PATH");
            std::env::remove_var("GIT_AI_INTERNAL_DIR");
        }

        // When GIT_AI_INTERNAL_DIR is set to an absolute machine-local dir, the authorship
        // DB path must be rerouted to <that dir>/db (no .git-ai/internal segments).
        let internal_dir = std::env::temp_dir().join("git-ai-internal-db-reroute-test");
        unsafe { std::env::set_var("GIT_AI_INTERNAL_DIR", &internal_dir) };
        let path = InternalDatabase::database_path().unwrap();
        assert_eq!(path, internal_dir.join("db"));

        // Precedence: when BOTH GIT_AI_INTERNAL_DIR and the per-DB test override are set,
        // the test override must still win.
        let override_path = std::env::temp_dir().join("git-ai-internal-db-explicit-override");
        unsafe { std::env::set_var("GIT_AI_TEST_DB_PATH", &override_path) };
        let path = InternalDatabase::database_path().unwrap();
        assert_eq!(path, override_path);

        // Restore the prior environment so we don't leak into other in-process tests.
        unsafe {
            match prev_internal {
                Some(v) => std::env::set_var("GIT_AI_INTERNAL_DIR", v),
                None => std::env::remove_var("GIT_AI_INTERNAL_DIR"),
            }
            match prev_test_override {
                Some(v) => std::env::set_var("GIT_AI_TEST_DB_PATH", v),
                None => std::env::remove_var("GIT_AI_TEST_DB_PATH"),
            }
            match prev_legacy_override {
                Some(v) => std::env::set_var("GITAI_TEST_DB_PATH", v),
                None => std::env::remove_var("GITAI_TEST_DB_PATH"),
            }
        }
    }

    // CAS sync queue tests

    #[test]
    fn test_cas_sync_queue_schema() {
        let (db, _temp_dir) = create_test_db();

        // Verify cas_sync_queue table exists
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='cas_sync_queue'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Verify status column has correct default and check constraint
        let status: String = db
            .conn
            .query_row(
                "SELECT dflt_value FROM pragma_table_info('cas_sync_queue') WHERE name='status'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "'pending'");
    }

    #[test]
    fn test_enqueue_cas_object_with_metadata() {
        let (mut db, _temp_dir) = create_test_db();

        let mut metadata = HashMap::new();
        metadata.insert("key1".to_string(), "value1".to_string());
        metadata.insert("key2".to_string(), "value2".to_string());

        let json_data = serde_json::json!({"test": "data", "number": 123});

        // Enqueue an object with metadata
        let hash = db.enqueue_cas_object(&json_data, Some(&metadata)).unwrap();

        // Verify metadata was stored correctly
        let metadata_json: String = db
            .conn
            .query_row(
                "SELECT metadata FROM cas_sync_queue WHERE hash = ?",
                params![&hash],
                |row| row.get(0),
            )
            .unwrap();

        let stored_metadata: HashMap<String, String> =
            serde_json::from_str(&metadata_json).unwrap();
        assert_eq!(stored_metadata.get("key1"), Some(&"value1".to_string()));
        assert_eq!(stored_metadata.get("key2"), Some(&"value2".to_string()));

        // Verify dequeue returns metadata correctly
        let batch = db.dequeue_cas_batch(10).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].hash, hash);
        // Data is canonicalized JSON
        let stored_json: serde_json::Value = serde_json::from_str(&batch[0].data).unwrap();
        assert_eq!(stored_json, json_data);
        assert_eq!(batch[0].metadata.get("key1"), Some(&"value1".to_string()));
        assert_eq!(batch[0].metadata.get("key2"), Some(&"value2".to_string()));
    }

    #[test]
    fn test_enqueue_cas_object() {
        let (mut db, _temp_dir) = create_test_db();

        let json_data = serde_json::json!({"key": "value"});

        // Enqueue an object
        let hash = db.enqueue_cas_object(&json_data, None).unwrap();

        // Verify it was inserted with correct defaults
        let (stored_hash, stored_data, metadata, status, attempts): (
            String,
            String,
            String,
            String,
            u32,
        ) = db
            .conn
            .query_row(
                "SELECT hash, data, metadata, status, attempts FROM cas_sync_queue WHERE hash = ?",
                params![&hash],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(stored_hash, hash);
        // Data should be canonicalized JSON
        let stored_json: serde_json::Value = serde_json::from_str(&stored_data).unwrap();
        assert_eq!(stored_json, json_data);
        assert_eq!(status, "pending");
        assert_eq!(attempts, 0);
        assert_eq!(metadata, "{}");
    }

    #[test]
    fn test_enqueue_duplicate_hash() {
        let (mut db, _temp_dir) = create_test_db();

        // Same JSON content should produce same hash
        let json_data = serde_json::json!({"same": "content"});

        // Enqueue the same content twice
        let hash1 = db.enqueue_cas_object(&json_data, None).unwrap();
        let hash2 = db.enqueue_cas_object(&json_data, None).unwrap();

        // Both calls should return the same hash
        assert_eq!(hash1, hash2);

        // Verify only one record exists (INSERT OR IGNORE)
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cas_sync_queue WHERE hash = ?",
                params![&hash1],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_dequeue_cas_batch() {
        let (mut db, _temp_dir) = create_test_db();

        // Enqueue multiple objects with different content
        db.enqueue_cas_object(&serde_json::json!({"id": 1}), None)
            .unwrap();
        db.enqueue_cas_object(&serde_json::json!({"id": 2}), None)
            .unwrap();
        db.enqueue_cas_object(&serde_json::json!({"id": 3}), None)
            .unwrap();

        // Dequeue batch of 2
        let batch = db.dequeue_cas_batch(2).unwrap();
        assert_eq!(batch.len(), 2);

        // Verify records are marked as processing
        let processing_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cas_sync_queue WHERE status = 'processing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(processing_count, 2);

        // Verify one is still pending
        let pending_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cas_sync_queue WHERE status = 'pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending_count, 1);
    }

    #[test]
    fn test_dequeue_respects_next_retry() {
        let (mut db, _temp_dir) = create_test_db();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let hash1 = "hash1";
        let hash2 = "hash2";
        let data1 = "data1";
        let data2 = "data2";

        // Insert one record ready to retry (past)
        db.conn.execute(
            "INSERT INTO cas_sync_queue (hash, data, metadata, status, attempts, next_retry_at, created_at) VALUES (?, ?, '{}', 'pending', 0, ?, ?)",
            params![hash1, data1, now - 100, now],
        ).unwrap();

        // Insert one record not ready yet (future)
        db.conn.execute(
            "INSERT INTO cas_sync_queue (hash, data, metadata, status, attempts, next_retry_at, created_at) VALUES (?, ?, '{}', 'pending', 0, ?, ?)",
            params![hash2, data2, now + 1000, now],
        ).unwrap();

        // Dequeue should only return the first one
        let batch = db.dequeue_cas_batch(10).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].hash, hash1);
    }

    #[test]
    fn test_dequeue_locks_records() {
        let (mut db, _temp_dir) = create_test_db();

        let json_data = serde_json::json!({"test": "lock"});
        let hash = db.enqueue_cas_object(&json_data, None).unwrap();

        // Dequeue
        let batch = db.dequeue_cas_batch(10).unwrap();
        assert_eq!(batch.len(), 1);

        // Verify status is 'processing'
        let status: String = db
            .conn
            .query_row(
                "SELECT status FROM cas_sync_queue WHERE hash = ?",
                params![&hash],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "processing");

        // Verify processing_started_at is set
        let processing_started_at: Option<i64> = db
            .conn
            .query_row(
                "SELECT processing_started_at FROM cas_sync_queue WHERE hash = ?",
                params![&hash],
                |row| row.get(0),
            )
            .unwrap();
        assert!(processing_started_at.is_some());

        // Try to dequeue again - should get empty (already locked)
        let batch2 = db.dequeue_cas_batch(10).unwrap();
        assert_eq!(batch2.len(), 0);
    }

    #[test]
    fn test_stale_lock_recovery() {
        let (mut db, _temp_dir) = create_test_db();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let hash = "hash1";
        let data = "data1";

        // Insert a record in 'processing' state with old timestamp (>10 minutes ago)
        let stale_time = now - 700; // 11+ minutes ago
        db.conn.execute(
            "INSERT INTO cas_sync_queue (hash, data, metadata, status, attempts, next_retry_at, processing_started_at, created_at) VALUES (?, ?, '{}', 'processing', 0, ?, ?, ?)",
            params![hash, data, now, stale_time, now],
        ).unwrap();

        // Dequeue should recover the stale lock
        let batch = db.dequeue_cas_batch(10).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].hash, hash);
    }

    #[test]
    fn test_max_attempts_limit() {
        let (mut db, _temp_dir) = create_test_db();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let hash1 = "hash1";
        let hash2 = "hash2";
        let data1 = "data1";
        let data2 = "data2";

        // Insert a record with 6 attempts (max reached)
        db.conn.execute(
            "INSERT INTO cas_sync_queue (hash, data, metadata, status, attempts, next_retry_at, created_at) VALUES (?, ?, '{}', 'pending', 6, ?, ?)",
            params![hash1, data1, now - 100, now],
        ).unwrap();

        // Insert a record with 5 attempts (still eligible)
        db.conn.execute(
            "INSERT INTO cas_sync_queue (hash, data, metadata, status, attempts, next_retry_at, created_at) VALUES (?, ?, '{}', 'pending', 5, ?, ?)",
            params![hash2, data2, now - 100, now],
        ).unwrap();

        // Dequeue should only return the one with 5 attempts
        let batch = db.dequeue_cas_batch(10).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].hash, hash2);
        assert_eq!(batch[0].attempts, 5);
    }

    #[test]
    fn test_update_cas_sync_failure() {
        let (mut db, _temp_dir) = create_test_db();

        db.enqueue_cas_object(&serde_json::json!({"test": "failure"}), None)
            .unwrap();
        let batch = db.dequeue_cas_batch(10).unwrap();
        let record = &batch[0];

        // Update with failure
        db.update_cas_sync_failure(record.id, "test error").unwrap();

        // Verify status is back to 'pending'
        let status: String = db
            .conn
            .query_row(
                "SELECT status FROM cas_sync_queue WHERE id = ?",
                params![record.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");

        // Verify processing_started_at is cleared
        let processing_started_at: Option<i64> = db
            .conn
            .query_row(
                "SELECT processing_started_at FROM cas_sync_queue WHERE id = ?",
                params![record.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(processing_started_at.is_none());

        // Verify attempts incremented
        let attempts: u32 = db
            .conn
            .query_row(
                "SELECT attempts FROM cas_sync_queue WHERE id = ?",
                params![record.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempts, 1);

        // Verify error recorded
        let error: String = db
            .conn
            .query_row(
                "SELECT last_sync_error FROM cas_sync_queue WHERE id = ?",
                params![record.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(error, "test error");
    }

    #[test]
    fn test_delete_cas_sync_record() {
        let (mut db, _temp_dir) = create_test_db();

        db.enqueue_cas_object(&serde_json::json!({"test": "delete"}), None)
            .unwrap();
        let batch = db.dequeue_cas_batch(10).unwrap();
        let record = &batch[0];

        // Delete the record
        db.delete_cas_sync_record(record.id).unwrap();

        // Verify it's gone
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cas_sync_queue WHERE id = ?",
                params![record.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    // CAS cache tests

    #[test]
    fn test_cas_cache_get_miss() {
        let (db, _temp_dir) = create_test_db();
        let result = db.get_cas_cache("nonexistent_hash").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_cas_cache_set_and_get() {
        let (mut db, _temp_dir) = create_test_db();
        let hash = "abc123def456";
        let messages = r#"[{"type":"user","text":"hello"}]"#;

        db.set_cas_cache(hash, messages).unwrap();

        let result = db.get_cas_cache(hash).unwrap();
        assert_eq!(result, Some(messages.to_string()));
    }

    #[test]
    fn test_cas_cache_overwrite() {
        let (mut db, _temp_dir) = create_test_db();
        let hash = "abc123def456";
        let messages1 = r#"[{"type":"user","text":"v1"}]"#;
        let messages2 = r#"[{"type":"user","text":"v2"}]"#;

        db.set_cas_cache(hash, messages1).unwrap();
        db.set_cas_cache(hash, messages2).unwrap();

        let result = db.get_cas_cache(hash).unwrap();
        assert_eq!(result, Some(messages2.to_string()));
    }

    #[test]
    fn test_exponential_backoff() {
        let now = 1000000i64;

        // Test each attempt's backoff
        assert_eq!(calculate_next_retry(1, now), now + 5 * 60); // 5 min
        assert_eq!(calculate_next_retry(2, now), now + 30 * 60); // 30 min
        assert_eq!(calculate_next_retry(3, now), now + 2 * 60 * 60); // 2 hours
        assert_eq!(calculate_next_retry(4, now), now + 6 * 60 * 60); // 6 hours
        assert_eq!(calculate_next_retry(5, now), now + 12 * 60 * 60); // 12 hours
        assert_eq!(calculate_next_retry(6, now), now + 24 * 60 * 60); // 24 hours
        assert_eq!(calculate_next_retry(7, now), now + 24 * 60 * 60); // 24 hours (max)
    }
}
