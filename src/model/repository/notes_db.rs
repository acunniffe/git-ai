//! Dedicated notes-backend storage at `~/.git-ai/internal/notes-db`.
//!
//! Single `notes` table that serves three roles, distinguished by `origin`:
//!  - `origin = 'local'` — primary storage for the sqlite notes backend; these
//!    rows are never uploaded and never evicted
//!  - `origin = 'queue'` — HTTP-backend rows: `synced = 0` pending upload,
//!    `synced = 1` uploaded (kept as read cache)
//!  - `origin = 'cache'` — read-cache rows imported from the remote backend or
//!    from `refs/notes/ai`; evictable
//!
//! Rows are NEVER deleted on successful upload — they are retained as the local
//! read cache so that subsequent reads can be served without git or a network call.
//!
//! This database is SEPARATE from `src/authorship/internal_db.rs`. Adding columns
//! or migrations to `internal_db` for this feature is explicitly not what we do here.

use crate::error::GitAiError;
use crate::model::repository::error::PersistenceError;
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Current schema version (must equal MIGRATIONS.len()).
const SCHEMA_VERSION: usize = 2;

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
    // Migration 1 → 2: origin column distinguishing local-primary rows
    // (sqlite backend), HTTP queue rows, and evictable cache rows. Existing
    // rows were all written by the HTTP queue path.
    r#"
    ALTER TABLE notes ADD COLUMN origin TEXT NOT NULL DEFAULT 'queue';
    "#,
];

/// Shared upsert SQL for the queue-backend note path (single writes and
/// batches): preserves `synced`/`attempts`/`next_retry_at` when content is
/// unchanged, otherwise resets them so the updated note is re-queued for
/// upload.
const UPSERT_NOTE_SQL: &str = r#"
INSERT INTO notes (commit_sha, content, synced, created_at, updated_at, next_retry_at)
VALUES (?1, ?2, 0, ?3, ?3, ?3)
ON CONFLICT(commit_sha) DO UPDATE SET
    content        = excluded.content,
    synced         = CASE WHEN notes.content = excluded.content THEN notes.synced ELSE 0 END,
    attempts       = CASE WHEN notes.content = excluded.content THEN notes.attempts ELSE 0 END,
    next_retry_at  = CASE WHEN notes.content = excluded.content THEN notes.next_retry_at ELSE excluded.next_retry_at END,
    updated_at     = excluded.updated_at
"#;

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
                let conn = crate::model::repository::sqlite::open_with_memory_limits(&temp_path)
                    .expect("Failed to create temp DB");
                Mutex::new(NotesDatabase { conn })
            }
        });
        Ok(db_mutex)
    }

    /// Open a database at an explicit path. Useful for tests that need an isolated
    /// DB instance without relying on the process-global OnceLock singleton.
    pub fn open_at_path(path: &std::path::Path) -> Result<Self, GitAiError> {
        crate::model::repository::sqlite::open_at_path(path, |conn| {
            let mut db = Self { conn };
            db.initialize_schema()?;
            Ok(db)
        })
    }

    /// Open (or create) the database at the configured path.
    fn new() -> Result<Self, GitAiError> {
        Self::open_at_path(&Self::database_path()?)
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

        let home = dirs::home_dir().ok_or_else(PersistenceError::home_dir_not_found)?;
        Ok(home.join(".git-ai").join("internal").join("notes-db"))
    }

    /// Apply schema migrations until the DB is at `SCHEMA_VERSION`.
    fn initialize_schema(&mut self) -> Result<(), GitAiError> {
        use crate::model::repository::sqlite;

        sqlite::ensure_schema_metadata_table(&self.conn)?;
        let current_version = sqlite::read_schema_version(&self.conn).unwrap_or(0);
        sqlite::migration_runner(
            &mut self.conn,
            "notes",
            current_version,
            SCHEMA_VERSION,
            Self::apply_migration,
        )
    }

    fn apply_migration(conn: &mut Connection, from_version: usize) -> Result<(), GitAiError> {
        if from_version >= MIGRATIONS.len() {
            return Err(PersistenceError::no_migration_path(
                "notes",
                from_version,
                from_version + 1,
            ));
        }

        let migration_sql = MIGRATIONS[from_version];
        let tx = conn.transaction()?;
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
        self.conn
            .execute(UPSERT_NOTE_SQL, params![commit_sha, content, now])?;
        Ok(())
    }

    /// Upsert a batch of notes inside a single transaction.
    pub fn upsert_notes_batch(&mut self, entries: &[(String, String)]) -> Result<(), GitAiError> {
        self.batch_upsert(entries, UPSERT_NOTE_SQL)
    }

    /// Upsert a note as sqlite-backend primary storage (`origin = 'local'`).
    ///
    /// Local-primary rows are terminal: never enqueued for upload and never
    /// evicted by cache maintenance.
    pub fn upsert_local_note(&mut self, commit_sha: &str, content: &str) -> Result<(), GitAiError> {
        self.upsert_local_notes_batch(&[(commit_sha.to_string(), content.to_string())])
    }

    /// Upsert a batch of local-primary notes inside a single transaction.
    pub fn upsert_local_notes_batch(
        &mut self,
        entries: &[(String, String)],
    ) -> Result<(), GitAiError> {
        self.batch_upsert(
            entries,
            r#"
            INSERT INTO notes (commit_sha, content, synced, origin, created_at, updated_at, next_retry_at)
            VALUES (?1, ?2, 1, 'local', ?3, ?3, 0)
            ON CONFLICT(commit_sha) DO UPDATE SET
                content    = excluded.content,
                origin     = 'local',
                synced     = 1,
                updated_at = excluded.updated_at
            "#,
        )
    }

    /// Read all local-primary notes (used by `git-ai notes migrate --to git-notes`).
    pub fn get_local_notes(&self) -> Result<Vec<(String, String)>, GitAiError> {
        let mut stmt = self
            .conn
            .prepare("SELECT commit_sha, content FROM notes WHERE origin = 'local'")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Insert already-synced rows (used for cache-warming on pull and migration).
    ///
    /// Rows are inserted with `synced = 1` and `origin = 'cache'`; they act as
    /// read cache but are not enqueued for upload. Local-primary rows are never
    /// overwritten by cache imports.
    pub fn cache_synced_notes(&mut self, entries: &[(String, String)]) -> Result<(), GitAiError> {
        self.batch_upsert(
            entries,
            r#"
            INSERT INTO notes (commit_sha, content, synced, origin, created_at, updated_at, last_sync_at, next_retry_at)
            VALUES (?1, ?2, 1, 'cache', ?3, ?3, ?3, 0)
            ON CONFLICT(commit_sha) DO UPDATE SET
                content      = excluded.content,
                synced       = 1,
                last_sync_at = excluded.last_sync_at,
                updated_at   = excluded.updated_at
            WHERE notes.origin != 'local'
            "#,
        )
    }

    /// Run `sql` as an `INSERT ... ON CONFLICT` upsert for each `(commit_sha,
    /// content)` entry inside a single transaction, binding `(sha, content,
    /// now)` as `?1, ?2, ?3`. Shared skeleton for the three note-upsert-batch
    /// variants above, which differ only in the SQL (and thus which columns/
    /// origin get written).
    fn batch_upsert(&mut self, entries: &[(String, String)], sql: &str) -> Result<(), GitAiError> {
        if entries.is_empty() {
            return Ok(());
        }
        let now = unix_now();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(sql)?;
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
                     AND origin = 'queue'
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
        // Local-primary rows (sqlite backend) are never counted or evicted.
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE synced = 1 AND origin != 'local'",
            [],
            |row| row.get(0),
        )?;
        if (count as usize) <= max_rows {
            return Ok(0);
        }
        let cutoff = unix_now() - max_age_secs;
        let deleted = self.conn.execute(
            "DELETE FROM notes WHERE synced = 1 AND origin != 'local' AND last_sync_at < ?1",
            params![cutoff],
        )?;
        Ok(deleted)
    }
}

fn unix_now() -> i64 {
    crate::model::clock::now_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Open a fresh in-memory database (via a temp file) without using the global singleton.
    fn create_test_db() -> (NotesDatabase, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test-notes.db");

        let conn = crate::model::repository::sqlite::open_with_memory_limits(&db_path).unwrap();
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
        assert_eq!(version, "2");
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

    #[test]
    fn test_local_notes_are_not_dequeued_or_evicted() {
        let (mut db, _tmp) = create_test_db();

        db.upsert_local_note("a".repeat(40).as_str(), "local-note")
            .unwrap();
        db.upsert_note("b".repeat(40).as_str(), "queued-note")
            .unwrap();

        // Only the queue-origin row is dequeued for upload.
        let pending = db.dequeue_pending(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].commit_sha, "b".repeat(40));

        // Eviction never touches local-primary rows even at zero thresholds.
        let evicted = db.evict_stale_cache(0, -1).unwrap();
        assert_eq!(evicted, 0);
        assert_eq!(
            db.get_note("a".repeat(40).as_str()).unwrap().as_deref(),
            Some("local-note")
        );
    }

    #[test]
    fn test_cache_import_does_not_clobber_local_note() {
        let (mut db, _tmp) = create_test_db();
        let sha = "c".repeat(40);

        db.upsert_local_note(&sha, "local-truth").unwrap();
        db.cache_synced_notes(&[(sha.clone(), "stale-remote-copy".to_string())])
            .unwrap();

        assert_eq!(
            db.get_note(&sha).unwrap().as_deref(),
            Some("local-truth"),
            "cache imports must not overwrite local-primary rows"
        );
    }

    #[test]
    fn test_get_local_notes_returns_only_local_rows() {
        let (mut db, _tmp) = create_test_db();
        db.upsert_local_note("d".repeat(40).as_str(), "local")
            .unwrap();
        db.upsert_note("e".repeat(40).as_str(), "queued").unwrap();
        db.cache_synced_notes(&[("f".repeat(40), "cached".to_string())])
            .unwrap();

        let local = db.get_local_notes().unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].0, "d".repeat(40));
    }

    #[test]
    fn test_migration_v1_to_v2_marks_existing_rows_as_queue() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("notes-db");

        // Create a v1 database by hand.
        {
            let conn = crate::model::repository::sqlite::open_with_memory_limits(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE schema_metadata (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
                INSERT INTO schema_metadata (key, value) VALUES ('version', '1');
                CREATE TABLE notes (
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
                INSERT INTO notes (commit_sha, content, synced, created_at, updated_at)
                VALUES ('legacy-sha', 'legacy-content', 0, 1, 1);
                "#,
            )
            .unwrap();
        }

        let db = NotesDatabase::open_at_path(&path).unwrap();
        let origin: String = db
            .conn
            .query_row(
                "SELECT origin FROM notes WHERE commit_sha = 'legacy-sha'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(origin, "queue", "legacy rows belong to the HTTP queue");
    }
}
