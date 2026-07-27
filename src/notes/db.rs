//! Dedicated notes-backend storage at `~/.git-ai/internal/notes-db`.
//!
//! Single `notes` table that doubles as cache and sync queue:
//!  - `synced = 0` — the row is pending upload to the remote backend
//!  - `synced = 1` — the row has been uploaded (kept for local read cache)
//!
//! Rows are NEVER deleted on successful upload — they are retained as the local
//! read cache so that subsequent reads can be served without git or a network call.
//!
//! This database is SEPARATE from `src/authorship/internal_db.rs`. Adding columns
//! or migrations to `internal_db` for this feature is explicitly not what we do here.

use crate::error::GitAiError;
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Current schema version (must equal MIGRATIONS.len()).
const SCHEMA_VERSION: usize = 1;

/// Database migrations — each entry upgrades the schema by one version.
const MIGRATIONS: &[&str] = &[
    // Migration 0 → 1: single notes table with synced flag
    r#"
    CREATE TABLE IF NOT EXISTS notes (
        commit_sha              TEXT PRIMARY KEY NOT NULL,
        content                 TEXT NOT NULL,
        synced                  INTEGER NOT NULL DEFAULT 0,
        attempts                INTEGER NOT NULL DEFAULT 0,
        last_sync_error         TEXT,
        last_sync_at            INTEGER,
        next_retry_at           INTEGER NOT NULL DEFAULT 0,
        processing_started_at   INTEGER,
        created_at              INTEGER NOT NULL,
        updated_at              INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_notes_pending
        ON notes(synced, next_retry_at) WHERE synced = 0;
    "#,
];

/// Global singleton for the notes database.
static NOTES_DB: OnceLock<Mutex<NotesDatabase>> = OnceLock::new();

/// A pending note returned from `dequeue_pending`.
#[derive(Debug, Clone)]
pub struct PendingNote {
    pub commit_sha: String,
    pub content: String,
    pub attempts: i64,
}

/// SQLite wrapper for notes storage and queue.
pub struct NotesDatabase {
    conn: Connection,
}

impl NotesDatabase {
    /// Return (or lazily initialize) the global database mutex.
    ///
    /// In test builds with `GIT_AI_TEST_NOTES_DB_PATH` set, the OnceLock singleton
    /// cannot be re-initialized per-test. Tests requiring isolated DB instances should
    /// use `open_at_path()` directly instead of relying on this singleton.
    pub fn global() -> Result<&'static Mutex<NotesDatabase>, GitAiError> {
        let db_mutex = NOTES_DB.get_or_init(|| match Self::new() {
            Ok(db) => Mutex::new(db),
            Err(e) => {
                eprintln!("[Error] Failed to initialize notes database: {}", e);
                // Fall back to a temp file so the process can continue running.
                let temp_path = std::env::temp_dir().join("git-ai-notes-db-failed");
                let conn = crate::sqlite::open_with_memory_limits(&temp_path)
                    .expect("Failed to create temp DB");
                Mutex::new(NotesDatabase { conn })
            }
        });
        Ok(db_mutex)
    }

    /// Open a database at an explicit path. Useful for tests that need an isolated
    /// DB instance without relying on the process-global OnceLock singleton.
    pub fn open_at_path(path: &std::path::Path) -> Result<Self, GitAiError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = crate::sqlite::open_with_memory_limits(path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA temp_store=MEMORY;
            "#,
        )?;
        let mut db = Self { conn };
        db.initialize_schema()?;
        Ok(db)
    }

    /// Open (or create) the database at the configured path.
    fn new() -> Result<Self, GitAiError> {
        let db_path = Self::database_path()?;

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = crate::sqlite::open_with_memory_limits(&db_path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA temp_store=MEMORY;
            "#,
        )?;

        let mut db = Self { conn };
        db.initialize_schema()?;
        Ok(db)
    }

    /// Resolve the on-disk path for the notes database.
    ///
    /// In tests, the `GIT_AI_TEST_NOTES_DB_PATH` environment variable overrides
    /// the default location so that test runs are isolated.
    fn database_path() -> Result<PathBuf, GitAiError> {
        #[cfg(any(test, feature = "test-support"))]
        if let Ok(test_path) = std::env::var("GIT_AI_TEST_NOTES_DB_PATH") {
            return Ok(PathBuf::from(test_path));
        }

        let home = dirs::home_dir()
            .ok_or_else(|| GitAiError::Generic("Could not determine home directory".to_string()))?;
        Ok(home.join(".git-ai").join("internal").join("notes-db"))
    }

    /// Apply schema migrations until the DB is at `SCHEMA_VERSION`.
    fn initialize_schema(&mut self) -> Result<(), GitAiError> {
        // Fast path: if the metadata table and version already match, skip migrations.
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
                return Ok(());
            }
            if current_version > SCHEMA_VERSION {
                return Err(GitAiError::Generic(format!(
                    "Notes database schema version {} is newer than supported version {}. \
                     Please upgrade git-ai.",
                    current_version, SCHEMA_VERSION
                )));
            }
        }

        // Ensure schema_metadata table exists.
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_metadata (
                key   TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            "#,
        )?;

        // Read current version (0 for a brand-new database).
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
            .unwrap_or(0);

        // Apply each missing migration in sequence.
        for target_version in current_version..SCHEMA_VERSION {
            self.apply_migration(target_version)?;

            // Use an upsert so concurrent initializers do not race on the version row.
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
        }

        Ok(())
    }

    fn apply_migration(&mut self, from_version: usize) -> Result<(), GitAiError> {
        if from_version >= MIGRATIONS.len() {
            return Err(GitAiError::Generic(format!(
                "No migration defined for version {} -> {}",
                from_version,
                from_version + 1
            )));
        }

        let migration_sql = MIGRATIONS[from_version];
        let tx = self.conn.transaction()?;
        tx.execute_batch(migration_sql)?;
        tx.commit()?;

        Ok(())
    }

    // ----- Write operations -----

    /// Upsert a note.
    ///
    /// - New rows are inserted with `synced = 0` (pending upload).
    /// - If the row already exists with *the same content*, the `synced` flag and
    ///   attempt count are preserved.
    /// - If the content changed, `synced` and `attempts` are reset to 0 so the
    ///   updated note is queued for re-upload.
    pub fn upsert_note(&mut self, commit_sha: &str, content: &str) -> Result<(), GitAiError> {
        let now = unix_now();
        self.conn.execute(
            r#"
            INSERT INTO notes (commit_sha, content, synced, created_at, updated_at, next_retry_at)
            VALUES (?1, ?2, 0, ?3, ?3, ?3)
            ON CONFLICT(commit_sha) DO UPDATE SET
                content        = excluded.content,
                synced         = CASE WHEN notes.content = excluded.content THEN notes.synced ELSE 0 END,
                attempts       = CASE WHEN notes.content = excluded.content THEN notes.attempts ELSE 0 END,
                next_retry_at  = CASE WHEN notes.content = excluded.content THEN notes.next_retry_at ELSE excluded.next_retry_at END,
                updated_at     = excluded.updated_at
            "#,
            params![commit_sha, content, now],
        )?;
        Ok(())
    }

    /// Upsert a batch of notes inside a single transaction.
    pub fn upsert_notes_batch(&mut self, entries: &[(String, String)]) -> Result<(), GitAiError> {
        if entries.is_empty() {
            return Ok(());
        }
        let now = unix_now();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                r#"
                INSERT INTO notes (commit_sha, content, synced, created_at, updated_at, next_retry_at)
                VALUES (?1, ?2, 0, ?3, ?3, ?3)
                ON CONFLICT(commit_sha) DO UPDATE SET
                    content        = excluded.content,
                    synced         = CASE WHEN notes.content = excluded.content THEN notes.synced ELSE 0 END,
                    attempts       = CASE WHEN notes.content = excluded.content THEN notes.attempts ELSE 0 END,
                    next_retry_at  = CASE WHEN notes.content = excluded.content THEN notes.next_retry_at ELSE excluded.next_retry_at END,
                    updated_at     = excluded.updated_at
                "#,
            )?;
            for (sha, content) in entries {
                stmt.execute(params![sha, content, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Insert already-synced rows (used for cache-warming on pull and migration).
    ///
    /// Rows are inserted with `synced = 1`; they act as read cache but are not
    /// enqueued for upload.
    pub fn cache_synced_notes(&mut self, entries: &[(String, String)]) -> Result<(), GitAiError> {
        if entries.is_empty() {
            return Ok(());
        }
        let now = unix_now();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                r#"
                INSERT INTO notes (commit_sha, content, synced, created_at, updated_at, last_sync_at, next_retry_at)
                VALUES (?1, ?2, 1, ?3, ?3, ?3, 0)
                ON CONFLICT(commit_sha) DO UPDATE SET
                    content      = excluded.content,
                    synced       = 1,
                    last_sync_at = excluded.last_sync_at,
                    updated_at   = excluded.updated_at
                "#,
            )?;
            for (sha, content) in entries {
                stmt.execute(params![sha, content, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ----- Queue operations -----

    /// Lock and return a batch of pending notes for upload.
    ///
    /// Sets `processing_started_at` on selected rows so concurrent workers do not
    /// pick up the same rows. Stale locks (older than 10 minutes) are automatically
    /// released before the batch is selected.
    ///
    /// Rows with `attempts >= 6` are skipped (permanent failure backoff).
    pub fn dequeue_pending(&mut self, batch_size: usize) -> Result<Vec<PendingNote>, GitAiError> {
        let now = unix_now();
        let stale_cutoff = now - 600; // 10 minutes

        // Release stale processing locks so they can be retried.
        self.conn.execute(
            r#"UPDATE notes
               SET processing_started_at = NULL
               WHERE synced = 0
                 AND processing_started_at IS NOT NULL
                 AND processing_started_at < ?1"#,
            params![stale_cutoff],
        )?;

        // Select eligible rows first, then lock them. Two-step approach avoids
        // UPDATE ... RETURNING which requires SQLite 3.35+.
        let shas: Vec<String> = {
            let mut stmt = self.conn.prepare(
                r#"SELECT commit_sha FROM notes
                   WHERE synced = 0
                     AND processing_started_at IS NULL
                     AND next_retry_at <= ?1
                     AND attempts < 6
                   ORDER BY next_retry_at
                   LIMIT ?2"#,
            )?;
            let rows = stmt.query_map(params![now, batch_size as i64], |row| {
                row.get::<_, String>(0)
            })?;
            rows.filter_map(|r| r.ok()).collect()
        };

        if shas.is_empty() {
            return Ok(Vec::new());
        }

        // Lock the selected rows.
        let placeholders: String = shas
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(",");
        let update_sql = format!(
            "UPDATE notes SET processing_started_at = ?1 WHERE commit_sha IN ({})",
            placeholders
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now)];
        for sha in &shas {
            params_vec.push(Box::new(sha.clone()));
        }
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
        self.conn.execute(&update_sql, param_refs.as_slice())?;

        // Read back the locked rows.
        let select_sql = format!(
            "SELECT commit_sha, content, attempts FROM notes WHERE commit_sha IN ({})",
            shas.iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut stmt = self.conn.prepare(&select_sql)?;
        let sha_params: Vec<Box<dyn rusqlite::ToSql>> = shas
            .iter()
            .map(|s| Box::new(s.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        let sha_param_refs: Vec<&dyn rusqlite::ToSql> =
            sha_params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(sha_param_refs.as_slice(), |row| {
            Ok(PendingNote {
                commit_sha: row.get(0)?,
                content: row.get(1)?,
                attempts: row.get(2)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Mark a set of notes as successfully synced.
    ///
    /// Sets `synced = 1` and clears `processing_started_at`. Returns the number
    /// of rows updated.
    pub fn mark_synced(&mut self, commit_shas: &[String]) -> Result<usize, GitAiError> {
        if commit_shas.is_empty() {
            return Ok(0);
        }
        let now = unix_now();

        // Build a parameterised `IN (...)` clause.
        let placeholders: String = commit_shas
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "UPDATE notes SET synced = 1, last_sync_at = ?1, processing_started_at = NULL \
             WHERE commit_sha IN ({})",
            placeholders
        );

        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now)];
        for sha in commit_shas {
            params_vec.push(Box::new(sha.clone()));
        }

        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let updated = self.conn.execute(&sql, params_refs.as_slice())?;
        Ok(updated)
    }

    /// Mark a batch as failed: release the lock, increment attempts, and schedule
    /// exponential-backoff retry.
    pub fn mark_failed(&mut self, commit_shas: &[String], error: &str) -> Result<(), GitAiError> {
        if commit_shas.is_empty() {
            return Ok(());
        }
        let now = unix_now();
        let tx = self.conn.transaction()?;
        for sha in commit_shas {
            tx.execute(
                r#"UPDATE notes
                   SET processing_started_at = NULL,
                       attempts              = attempts + 1,
                       last_sync_error       = ?1,
                       last_sync_at          = ?2,
                       next_retry_at         = ?2 + (1 << MIN(attempts + 1, 8)) * 5
                   WHERE commit_sha = ?3 AND synced = 0"#,
                params![error, now, sha],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ----- Read operations -----

    /// Count unsynced notes that can be dequeued for upload right now.
    pub fn count_pending_uploadable(&self) -> Result<usize, GitAiError> {
        let now = unix_now();
        let count: i64 = self.conn.query_row(
            r#"SELECT COUNT(*) FROM notes
               WHERE synced = 0
                 AND processing_started_at IS NULL
                 AND next_retry_at <= ?1
                 AND attempts < 6"#,
            params![now],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Retrieve the note content for a single commit SHA.
    pub fn get_note(&self, commit_sha: &str) -> Result<Option<String>, GitAiError> {
        match self.conn.query_row(
            "SELECT content FROM notes WHERE commit_sha = ?1",
            params![commit_sha],
            |row| row.get::<_, String>(0),
        ) {
            Ok(c) => Ok(Some(c)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Return the subset of `commit_shas` that exist in the DB with `synced = 1`.
    pub fn get_synced_shas(&self, commit_shas: &[&str]) -> Result<HashSet<String>, GitAiError> {
        if commit_shas.is_empty() {
            return Ok(HashSet::new());
        }
        let placeholders: String = commit_shas
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT commit_sha FROM notes WHERE synced = 1 AND commit_sha IN ({})",
            placeholders
        );
        let params_vec: Vec<&dyn rusqlite::ToSql> = commit_shas
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_vec.as_slice(), |row| row.get::<_, String>(0))?;
        let mut result = HashSet::new();
        for row in rows {
            result.insert(row?);
        }
        Ok(result)
    }

    /// Retrieve note content for a slice of commit SHAs.
    ///
    /// Only SHAs that exist in the database are returned; missing SHAs are absent
    /// from the result map.
    pub fn get_notes(&self, commit_shas: &[&str]) -> Result<HashMap<String, String>, GitAiError> {
        if commit_shas.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders: String = commit_shas
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT commit_sha, content FROM notes WHERE commit_sha IN ({})",
            placeholders
        );
        let params_vec: Vec<&dyn rusqlite::ToSql> = commit_shas
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_vec.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut result = HashMap::new();
        for row in rows {
            let (sha, content) = row?;
            result.insert(sha, content);
        }
        Ok(result)
    }

    /// Return the commit SHAs of notes whose content contains `needle` as a
    /// literal substring (LIKE wildcards in the needle are escaped).
    pub fn search_notes_content(&self, needle: &str) -> Result<Vec<String>, GitAiError> {
        let escaped = needle
            .replace('\\', r"\\")
            .replace('%', r"\%")
            .replace('_', r"\_");
        let pattern = format!("%{}%", escaped);

        let mut stmt = self
            .conn
            .prepare(r"SELECT commit_sha FROM notes WHERE content LIKE ?1 ESCAPE '\'")?;
        let rows = stmt.query_map(params![pattern], |row| row.get::<_, String>(0))?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Evict synced cache entries older than `max_age_secs` when the total row
    /// count exceeds `max_rows`. Returns the number of rows deleted.
    pub fn evict_stale_cache(
        &mut self,
        max_rows: usize,
        max_age_secs: i64,
    ) -> Result<usize, GitAiError> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM notes WHERE synced = 1", [], |row| {
                    row.get(0)
                })?;
        if (count as usize) <= max_rows {
            return Ok(0);
        }
        let cutoff = unix_now() - max_age_secs;
        let deleted = self.conn.execute(
            "DELETE FROM notes WHERE synced = 1 AND last_sync_at < ?1",
            params![cutoff],
        )?;
        Ok(deleted)
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Open a fresh in-memory database (via a temp file) without using the global singleton.
    fn create_test_db() -> (NotesDatabase, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test-notes.db");

        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            "#,
        )
        .unwrap();

        let mut db = NotesDatabase { conn };
        db.initialize_schema().unwrap();

        (db, temp_dir)
    }

    // --- Schema tests ---

    #[test]
    fn test_fresh_db_creates_notes_table() {
        let (db, _tmp) = create_test_db();

        let table_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='notes'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 1, "notes table should exist after init");

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "1");
    }

    #[test]
    fn test_notes_table_has_expected_columns() {
        let (db, _tmp) = create_test_db();

        // PRAGMA table_info returns one row per column
        let mut stmt = db.conn.prepare("PRAGMA table_info(notes)").unwrap();
        let column_names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        let required = [
            "commit_sha",
            "content",
            "synced",
            "attempts",
            "last_sync_error",
            "last_sync_at",
            "next_retry_at",
            "processing_started_at",
            "created_at",
            "updated_at",
        ];
        for col in &required {
            assert!(
                column_names.iter().any(|c| c == col),
                "column '{}' is missing; found: {:?}",
                col,
                column_names
            );
        }
    }

    // --- Upsert / round-trip ---

    #[test]
    fn test_upsert_and_get_note_roundtrip() {
        let (mut db, _tmp) = create_test_db();

        db.upsert_note("abc123", "content1").unwrap();
        let retrieved = db.get_note("abc123").unwrap();
        assert_eq!(retrieved, Some("content1".to_string()));
    }

    #[test]
    fn test_upsert_missing_sha_returns_none() {
        let (db, _tmp) = create_test_db();
        assert_eq!(db.get_note("nonexistent").unwrap(), None);
    }

    #[test]
    fn test_upsert_new_content_resets_synced() {
        let (mut db, _tmp) = create_test_db();

        db.upsert_note("sha1", "original").unwrap();
        // Mark as synced manually
        db.conn
            .execute("UPDATE notes SET synced = 1 WHERE commit_sha = 'sha1'", [])
            .unwrap();

        // Upsert with different content → should reset synced
        db.upsert_note("sha1", "updated").unwrap();
        let synced: i64 = db
            .conn
            .query_row(
                "SELECT synced FROM notes WHERE commit_sha = 'sha1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(synced, 0, "synced should be reset when content changes");
    }

    #[test]
    fn test_upsert_same_content_preserves_synced() {
        let (mut db, _tmp) = create_test_db();

        db.upsert_note("sha1", "same content").unwrap();
        db.conn
            .execute("UPDATE notes SET synced = 1 WHERE commit_sha = 'sha1'", [])
            .unwrap();

        // Upsert with identical content → synced should stay 1
        db.upsert_note("sha1", "same content").unwrap();
        let synced: i64 = db
            .conn
            .query_row(
                "SELECT synced FROM notes WHERE commit_sha = 'sha1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            synced, 1,
            "synced should be preserved when content is unchanged"
        );
    }

    // --- Dequeue / mark_synced round-trip ---

    #[test]
    fn test_dequeue_returns_pending_notes() {
        let (mut db, _tmp) = create_test_db();

        db.upsert_note("sha_a", "content_a").unwrap();
        db.upsert_note("sha_b", "content_b").unwrap();

        let batch = db.dequeue_pending(10).unwrap();
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn test_dequeue_mark_synced_roundtrip() {
        let (mut db, _tmp) = create_test_db();

        db.upsert_note("sha1", "data").unwrap();

        // First dequeue should return the row.
        let batch = db.dequeue_pending(10).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].commit_sha, "sha1");

        // Mark synced
        let shas: Vec<String> = batch.iter().map(|p| p.commit_sha.clone()).collect();
        let updated = db.mark_synced(&shas).unwrap();
        assert_eq!(updated, 1);

        // Second dequeue should return nothing (row is now synced = 1).
        let batch2 = db.dequeue_pending(10).unwrap();
        assert!(batch2.is_empty(), "no pending rows after mark_synced");
    }

    #[test]
    fn test_dequeue_does_not_return_synced_rows() {
        let (mut db, _tmp) = create_test_db();

        db.cache_synced_notes(&[("sha_synced".to_string(), "cached".to_string())])
            .unwrap();

        let batch = db.dequeue_pending(10).unwrap();
        assert!(
            batch.is_empty(),
            "cache_synced_notes rows must not appear in dequeue_pending"
        );
    }

    #[test]
    fn test_count_pending_uploadable_excludes_deferred_and_processing_rows() {
        let (mut db, _tmp) = create_test_db();

        for sha in ["ready", "backoff", "processing", "permanent"] {
            db.upsert_note(sha, "content").unwrap();
        }
        db.conn
            .execute(
                "UPDATE notes SET attempts = 1, next_retry_at = ?1 WHERE commit_sha = 'backoff'",
                params![unix_now() + 3_600],
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE notes SET processing_started_at = ?1 WHERE commit_sha = 'processing'",
                params![unix_now()],
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE notes SET attempts = 6 WHERE commit_sha = 'permanent'",
                [],
            )
            .unwrap();

        assert_eq!(db.count_pending_uploadable().unwrap(), 1);
    }

    // --- cache_synced_notes ---

    #[test]
    fn test_cache_synced_notes_inserts_with_synced_1() {
        let (mut db, _tmp) = create_test_db();

        db.cache_synced_notes(&[
            ("commit1".to_string(), "note1".to_string()),
            ("commit2".to_string(), "note2".to_string()),
        ])
        .unwrap();

        // Verify both rows exist and are synced = 1
        let synced: i64 = db
            .conn
            .query_row(
                "SELECT synced FROM notes WHERE commit_sha = 'commit1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(synced, 1);

        let synced2: i64 = db
            .conn
            .query_row(
                "SELECT synced FROM notes WHERE commit_sha = 'commit2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(synced2, 1);
    }

    // --- mark_failed ---

    #[test]
    fn test_mark_failed_increments_attempts_and_schedules_retry() {
        let (mut db, _tmp) = create_test_db();

        db.upsert_note("sha_fail", "data").unwrap();

        // Dequeue so processing_started_at is set
        let _ = db.dequeue_pending(10).unwrap();

        let before_time = unix_now();
        db.mark_failed(&["sha_fail".to_string()], "connection timeout")
            .unwrap();

        let (attempts, next_retry_at, error): (i64, i64, String) = db
            .conn
            .query_row(
                "SELECT attempts, next_retry_at, last_sync_error FROM notes WHERE commit_sha = 'sha_fail'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert_eq!(attempts, 1, "attempts should be incremented");
        assert!(
            next_retry_at > before_time,
            "next_retry_at should be in the future"
        );
        assert_eq!(error, "connection timeout");
    }

    #[test]
    fn test_mark_failed_processing_started_cleared() {
        let (mut db, _tmp) = create_test_db();

        db.upsert_note("sha_lock", "data").unwrap();
        let _ = db.dequeue_pending(10).unwrap(); // sets processing_started_at

        db.mark_failed(&["sha_lock".to_string()], "err").unwrap();

        let processing_started_at: Option<i64> = db
            .conn
            .query_row(
                "SELECT processing_started_at FROM notes WHERE commit_sha = 'sha_lock'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            processing_started_at.is_none(),
            "processing_started_at should be cleared after mark_failed"
        );
    }

    // --- get_notes (batch) ---

    #[test]
    fn test_get_notes_batch() {
        let (mut db, _tmp) = create_test_db();

        db.upsert_note("sha1", "c1").unwrap();
        db.upsert_note("sha2", "c2").unwrap();

        let results = db.get_notes(&["sha1", "sha2", "sha_missing"]).unwrap();
        assert_eq!(results.get("sha1"), Some(&"c1".to_string()));
        assert_eq!(results.get("sha2"), Some(&"c2".to_string()));
        assert!(
            !results.contains_key("sha_missing"),
            "missing SHA should not be in result"
        );
    }

    // --- database_path ---

    #[test]
    #[serial_test::serial(notes_db_env)]
    fn test_database_path_contains_expected_segments() {
        // Without the test env var set we expect the path to include .git-ai/internal/notes-db
        // (this test verifies the non-override branch at the schema level; in CI the HOME is
        // always set so dirs::home_dir() returns Some).
        unsafe {
            std::env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
        }
        let path = NotesDatabase::database_path().unwrap();
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains(".git-ai"),
            "path should contain .git-ai: {}",
            path_str
        );
        assert!(
            path_str.contains("internal"),
            "path should contain internal: {}",
            path_str
        );
        assert!(
            path_str.ends_with("notes-db"),
            "path should end with notes-db: {}",
            path_str
        );
    }
}
