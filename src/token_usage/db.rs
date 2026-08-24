//! Token-usage state database.
//!
//! One SQLite database (`~/.git-ai/internal/token-usage-db`) holds everything
//! the token-usage pipeline needs to stay consistent:
//!
//! - `tracked_files`: the authoritative read cursor (byte offset) and
//!   serialized extractor state per transcript file,
//! - `usage_entries`: deduplicated per-entry token usage,
//! - `bucket_state`: the fingerprint of the last emitted aggregate per
//!   `(session_id, model, bucket_ts)`, so unchanged buckets are never
//!   re-emitted.
//!
//! Keeping the cursor here (rather than in the streams database) matters:
//! [`TokenUsageDatabase::commit_batch`] writes the entries, the extractor
//! state, and the advanced cursor in a single transaction, so a crash can
//! never replay lines against post-batch parser state.
//!
//! Changed buckets are found by *reconciliation*, not change tracking:
//! [`TokenUsageDatabase::session_buckets`] enumerates every bucket the
//! session has entries or emission state for, and the caller re-emits those
//! whose current aggregate fingerprint differs from the emitted one. Because
//! no pending-emission state lives in memory, a crash or failed emission is
//! healed by the next pass over the session.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::claude::{ReplacementCandidate, should_replace};
use super::cost::entry_cost_micro_usd;
use super::types::{UsageEntry, bucket_ts};
use crate::error::GitAiError;

/// Schema migrations - each entry is SQL to apply for that version.
const MIGRATIONS: &[&str] = &[
    // Version 1: Initial schema
    r#"
    CREATE TABLE IF NOT EXISTS schema_version (
        version INTEGER PRIMARY KEY
    );

    CREATE TABLE IF NOT EXISTS tracked_files (
        session_id  TEXT NOT NULL,
        stream_path TEXT NOT NULL,
        tool        TEXT NOT NULL,
        byte_offset INTEGER NOT NULL DEFAULT 0,
        state_json  TEXT,
        last_known_size INTEGER NOT NULL DEFAULT 0,
        last_modified INTEGER,
        processing_errors INTEGER NOT NULL DEFAULT 0,
        last_error TEXT,
        PRIMARY KEY (session_id, stream_path)
    );

    CREATE TABLE IF NOT EXISTS usage_entries (
        session_id TEXT NOT NULL,
        entry_key  TEXT NOT NULL,
        message_id TEXT,
        model      TEXT NOT NULL,
        bucket_ts  INTEGER NOT NULL,
        input_tokens INTEGER NOT NULL DEFAULT 0,
        output_tokens INTEGER NOT NULL DEFAULT 0,
        cache_read_tokens INTEGER NOT NULL DEFAULT 0,
        cache_write_tokens INTEGER NOT NULL DEFAULT 0,
        reasoning_output_tokens INTEGER,
        total_tokens INTEGER NOT NULL DEFAULT 0,
        cost_micro_usd INTEGER,
        is_sidechain INTEGER NOT NULL DEFAULT 0,
        has_speed INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (session_id, entry_key)
    );

    CREATE INDEX IF NOT EXISTS idx_usage_entries_bucket
        ON usage_entries(session_id, model, bucket_ts);
    CREATE INDEX IF NOT EXISTS idx_usage_entries_message
        ON usage_entries(session_id, message_id) WHERE message_id IS NOT NULL;

    CREATE TABLE IF NOT EXISTS bucket_state (
        session_id TEXT NOT NULL,
        model      TEXT NOT NULL,
        bucket_ts  INTEGER NOT NULL,
        emitted_fingerprint TEXT NOT NULL,
        last_emitted_at INTEGER NOT NULL,
        PRIMARY KEY (session_id, model, bucket_ts)
    );

    INSERT INTO schema_version (version) VALUES (1);
    "#,
];

/// A tracked transcript file: read cursor plus extractor state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedFile {
    pub session_id: String,
    pub stream_path: String,
    pub tool: String,
    pub byte_offset: u64,
    pub state_json: Option<String>,
    pub last_known_size: i64,
    pub last_modified: Option<i64>,
    pub processing_errors: i64,
}

/// The aggregate of one `(session_id, model, bucket_ts)` bucket, i.e. exactly
/// the values a TokenUsage event carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketAggregate {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// `Some` iff any entry in the bucket reported reasoning tokens.
    pub reasoning_output: Option<u64>,
    pub total: u64,
    pub cost_micro_usd: u64,
    pub message_count: u32,
}

impl BucketAggregate {
    /// Emission fingerprint: any change to any emitted value - including a
    /// drop to zero - changes the fingerprint.
    pub fn fingerprint(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}:{}",
            self.input,
            self.output,
            self.cache_read,
            self.cache_write,
            self.reasoning_output
                .map_or_else(|| "-".to_string(), |v| v.to_string()),
            self.total,
            self.cost_micro_usd,
            self.message_count
        )
    }
}

/// SQLite database for token-usage state.
pub struct TokenUsageDatabase {
    conn: Arc<Mutex<Connection>>,
}

impl TokenUsageDatabase {
    /// Open or create the token-usage database at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, GitAiError> {
        let conn = crate::sqlite::open_with_memory_limits(path.as_ref())?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.migrate()?;
        Ok(db)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn migrate(&self) -> Result<(), GitAiError> {
        let conn = self.lock();
        let table_exists: bool = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |row| Ok(row.get::<_, i64>(0)? > 0),
        )?;
        let current_version: u32 = if table_exists {
            conn.query_row(
                "SELECT version FROM schema_version ORDER BY version DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0)
        } else {
            0
        };
        for (version, migration_sql) in MIGRATIONS.iter().enumerate() {
            if current_version < (version + 1) as u32 {
                conn.execute_batch(migration_sql)?;
            }
        }
        Ok(())
    }

    /// Current schema version (for tests).
    #[cfg(test)]
    fn schema_version(&self) -> Result<u32, GitAiError> {
        Ok(self.lock().query_row(
            "SELECT version FROM schema_version ORDER BY version DESC LIMIT 1",
            [],
            |row| row.get(0),
        )?)
    }

    /// Fetch the tracked file row, creating it with a zero cursor when new.
    pub fn ensure_file(
        &self,
        session_id: &str,
        stream_path: &str,
        tool: &str,
    ) -> Result<TrackedFile, GitAiError> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR IGNORE INTO tracked_files (session_id, stream_path, tool) VALUES (?1, ?2, ?3)",
            params![session_id, stream_path, tool],
        )?;
        Ok(conn.query_row(
            "SELECT session_id, stream_path, tool, byte_offset, state_json,
                    last_known_size, last_modified, processing_errors
             FROM tracked_files WHERE session_id = ?1 AND stream_path = ?2",
            params![session_id, stream_path],
            |row| {
                Ok(TrackedFile {
                    session_id: row.get(0)?,
                    stream_path: row.get(1)?,
                    tool: row.get(2)?,
                    byte_offset: row.get::<_, i64>(3)?.max(0) as u64,
                    state_json: row.get(4)?,
                    last_known_size: row.get(5)?,
                    last_modified: row.get(6)?,
                    processing_errors: row.get(7)?,
                })
            },
        )?)
    }

    /// All tracked files (sweep enumeration).
    pub fn all_files(&self) -> Result<Vec<TrackedFile>, GitAiError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT session_id, stream_path, tool, byte_offset, state_json,
                    last_known_size, last_modified, processing_errors
             FROM tracked_files",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(TrackedFile {
                session_id: row.get(0)?,
                stream_path: row.get(1)?,
                tool: row.get(2)?,
                byte_offset: row.get::<_, i64>(3)?.max(0) as u64,
                state_json: row.get(4)?,
                last_known_size: row.get(5)?,
                last_modified: row.get(6)?,
                processing_errors: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Persist one extracted batch atomically: deduplicated entries, the
    /// advanced read cursor, and the extractor state. Clears any recorded
    /// processing error.
    pub fn commit_batch(
        &self,
        session_id: &str,
        stream_path: &str,
        entries: &[UsageEntry],
        new_offset: u64,
        state_json: Option<&str>,
    ) -> Result<(), GitAiError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for entry in entries {
            upsert_entry(&tx, session_id, entry)?;
        }
        tx.execute(
            "UPDATE tracked_files
             SET byte_offset = ?1, state_json = ?2, processing_errors = 0, last_error = NULL
             WHERE session_id = ?3 AND stream_path = ?4",
            params![new_offset as i64, state_json, session_id, stream_path],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every `(model, bucket_ts)` the session has entries or emission state
    /// for — the reconciliation candidates. Buckets present only in
    /// `bucket_state` (all their entries were replaced away or pruned) are
    /// included so an emptied bucket can re-emit as zero.
    pub fn session_buckets(&self, session_id: &str) -> Result<Vec<(String, u32)>, GitAiError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT model, bucket_ts FROM usage_entries WHERE session_id = ?1
             UNION
             SELECT model, bucket_ts FROM bucket_state WHERE session_id = ?1",
        )?;
        let rows = stmt.query_map(params![session_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Update the file-size/mtime snapshot used to skip unchanged files.
    pub fn update_file_metadata(
        &self,
        session_id: &str,
        stream_path: &str,
        size: u64,
        modified: Option<i64>,
    ) -> Result<(), GitAiError> {
        self.lock().execute(
            "UPDATE tracked_files SET last_known_size = ?1, last_modified = ?2
             WHERE session_id = ?3 AND stream_path = ?4",
            params![size as i64, modified, session_id, stream_path],
        )?;
        Ok(())
    }

    /// Record a processing failure for backoff and diagnostics.
    pub fn record_error(
        &self,
        session_id: &str,
        stream_path: &str,
        error: &str,
    ) -> Result<(), GitAiError> {
        self.lock().execute(
            "UPDATE tracked_files
             SET processing_errors = processing_errors + 1, last_error = ?1
             WHERE session_id = ?2 AND stream_path = ?3",
            params![error, session_id, stream_path],
        )?;
        Ok(())
    }

    /// Aggregate one bucket. An empty bucket returns all zeros.
    pub fn aggregate_bucket(
        &self,
        session_id: &str,
        model: &str,
        bucket: u32,
    ) -> Result<BucketAggregate, GitAiError> {
        Ok(self.lock().query_row(
            "SELECT COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_tokens), 0),
                    COUNT(reasoning_output_tokens),
                    COALESCE(SUM(reasoning_output_tokens), 0),
                    COALESCE(SUM(total_tokens), 0),
                    COALESCE(SUM(cost_micro_usd), 0),
                    COUNT(*)
             FROM usage_entries
             WHERE session_id = ?1 AND model = ?2 AND bucket_ts = ?3",
            params![session_id, model, bucket],
            |row| {
                let reasoning_entries: i64 = row.get(4)?;
                Ok(BucketAggregate {
                    input: row.get::<_, i64>(0)? as u64,
                    output: row.get::<_, i64>(1)? as u64,
                    cache_read: row.get::<_, i64>(2)? as u64,
                    cache_write: row.get::<_, i64>(3)? as u64,
                    reasoning_output: (reasoning_entries > 0)
                        .then(|| row.get::<_, i64>(5).map(|v| v as u64))
                        .transpose()?,
                    total: row.get::<_, i64>(6)? as u64,
                    cost_micro_usd: row.get::<_, i64>(7)? as u64,
                    message_count: row.get::<_, i64>(8)? as u32,
                })
            },
        )?)
    }

    /// Fingerprint of the last emitted aggregate for the bucket, if any.
    pub fn emitted_fingerprint(
        &self,
        session_id: &str,
        model: &str,
        bucket: u32,
    ) -> Result<Option<String>, GitAiError> {
        Ok(self
            .lock()
            .query_row(
                "SELECT emitted_fingerprint FROM bucket_state
                 WHERE session_id = ?1 AND model = ?2 AND bucket_ts = ?3",
                params![session_id, model, bucket],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Record that the bucket's current aggregate was handed to the metrics
    /// pipeline.
    pub fn mark_emitted(
        &self,
        session_id: &str,
        model: &str,
        bucket: u32,
        fingerprint: &str,
        now_ts: i64,
    ) -> Result<(), GitAiError> {
        self.lock().execute(
            "INSERT INTO bucket_state (session_id, model, bucket_ts, emitted_fingerprint, last_emitted_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id, model, bucket_ts)
             DO UPDATE SET emitted_fingerprint = ?4, last_emitted_at = ?5",
            params![session_id, model, bucket, fingerprint, now_ts],
        )?;
        Ok(())
    }

    /// Retention prune: drop entries and bucket state older than the cutoff
    /// bucket. Cursors are monotonic, so pruned history is never re-read.
    pub fn prune_buckets_before(&self, cutoff_bucket_ts: u32) -> Result<usize, GitAiError> {
        let conn = self.lock();
        let entries = conn.execute(
            "DELETE FROM usage_entries WHERE bucket_ts < ?1",
            params![cutoff_bucket_ts],
        )?;
        conn.execute(
            "DELETE FROM bucket_state WHERE bucket_ts < ?1",
            params![cutoff_bucket_ts],
        )?;
        Ok(entries)
    }
}

/// A stored entry's dedup-relevant fields.
struct ExistingRow {
    rowid: i64,
    replacement: ReplacementCandidate,
}

/// Insert one entry with ccusage's dedup semantics: exact `(message_id,
/// request_id)` identity first (encoded in `entry_key`), then the
/// message-id-only fallback for sidechain replays, with the replacement
/// policy deciding winners.
fn upsert_entry(
    tx: &Transaction<'_>,
    session_id: &str,
    entry: &UsageEntry,
) -> Result<(), GitAiError> {
    let bucket = bucket_ts(entry.ts);
    let existing = find_dedupe_target(tx, session_id, entry)?;
    match existing {
        Some(row) => {
            if should_replace(entry.into(), row.replacement) {
                tx.execute(
                    "UPDATE usage_entries SET
                        entry_key = ?1, message_id = ?2, model = ?3, bucket_ts = ?4,
                        input_tokens = ?5, output_tokens = ?6, cache_read_tokens = ?7,
                        cache_write_tokens = ?8, reasoning_output_tokens = ?9,
                        total_tokens = ?10, cost_micro_usd = ?11, is_sidechain = ?12,
                        has_speed = ?13
                     WHERE rowid = ?14",
                    params![
                        entry.entry_key,
                        entry.message_id,
                        entry.model,
                        bucket,
                        entry.tokens.input as i64,
                        entry.tokens.output as i64,
                        entry.tokens.cache_read as i64,
                        entry.tokens.cache_write as i64,
                        entry.tokens.reasoning_output.map(|v| v as i64),
                        entry.tokens.total as i64,
                        entry_cost_micro_usd(entry).map(|v| v as i64),
                        entry.is_sidechain,
                        entry.has_speed,
                        row.rowid,
                    ],
                )?;
            }
        }
        None => {
            tx.execute(
                "INSERT INTO usage_entries (
                    session_id, entry_key, message_id, model, bucket_ts,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_output_tokens, total_tokens, cost_micro_usd,
                    is_sidechain, has_speed
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    session_id,
                    entry.entry_key,
                    entry.message_id,
                    entry.model,
                    bucket,
                    entry.tokens.input as i64,
                    entry.tokens.output as i64,
                    entry.tokens.cache_read as i64,
                    entry.tokens.cache_write as i64,
                    entry.tokens.reasoning_output.map(|v| v as i64),
                    entry.tokens.total as i64,
                    entry_cost_micro_usd(entry).map(|v| v as i64),
                    entry.is_sidechain,
                    entry.has_speed,
                ],
            )?;
        }
    }
    Ok(())
}

fn find_dedupe_target(
    tx: &Transaction<'_>,
    session_id: &str,
    entry: &UsageEntry,
) -> Result<Option<ExistingRow>, GitAiError> {
    let read_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<ExistingRow> {
        let token_total = row.get::<_, i64>(1)? as u64
            + row.get::<_, i64>(2)? as u64
            + row.get::<_, i64>(3)? as u64
            + row.get::<_, i64>(4)? as u64;
        Ok(ExistingRow {
            rowid: row.get(0)?,
            replacement: ReplacementCandidate {
                token_total,
                is_sidechain: row.get(5)?,
                has_speed: row.get(6)?,
            },
        })
    };
    const ROW_COLUMNS: &str = "rowid, input_tokens, output_tokens, \
                               cache_read_tokens, cache_write_tokens, is_sidechain, has_speed";

    let exact = tx
        .query_row(
            &format!(
                "SELECT {ROW_COLUMNS} FROM usage_entries WHERE session_id = ?1 AND entry_key = ?2"
            ),
            params![session_id, entry.entry_key],
            read_row,
        )
        .optional()?;
    if exact.is_some() {
        return Ok(exact);
    }

    // Sidechain logs can replay parent messages with new request ids, so a
    // message-id-only match dedups when either side is a sidechain entry
    // (ccusage `loaded_entry_matches_sidechain_dedupe_key`).
    let Some(message_id) = entry.message_id.as_deref() else {
        return Ok(None);
    };
    let mut stmt = tx.prepare(&format!(
        "SELECT {ROW_COLUMNS} FROM usage_entries
         WHERE session_id = ?1 AND message_id = ?2 AND (?3 OR is_sidechain)
         ORDER BY rowid LIMIT 1"
    ))?;
    Ok(stmt
        .query_row(
            params![session_id, message_id, entry.is_sidechain],
            read_row,
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_usage::types::TokenCounts;
    use std::collections::HashSet;

    fn db() -> (tempfile::TempDir, TokenUsageDatabase) {
        let dir = tempfile::tempdir().unwrap();
        let db = TokenUsageDatabase::open(dir.path().join("token-usage-db")).unwrap();
        (dir, db)
    }

    fn entry(key: &str, message_id: Option<&str>, ts: u32, tokens: TokenCounts) -> UsageEntry {
        UsageEntry {
            entry_key: key.to_string(),
            message_id: message_id.map(str::to_string),
            ts,
            model: "claude-sonnet-4-20250514".to_string(),
            tokens,
            cache_write_1h: 0,
            transcript_cost_micro_usd: Some(1_000),
            is_sidechain: false,
            has_speed: false,
        }
    }

    fn tokens(input: u64, output: u64) -> TokenCounts {
        TokenCounts {
            input,
            output,
            total: input + output,
            ..Default::default()
        }
    }

    fn commit(db: &TokenUsageDatabase, entries: &[UsageEntry]) {
        db.ensure_file("s1", "/t.jsonl", "claude").unwrap();
        db.commit_batch("s1", "/t.jsonl", entries, 0, None).unwrap()
    }

    #[test]
    fn migrations_apply_and_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-usage-db");
        let db = TokenUsageDatabase::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 1);
        drop(db);
        let db = TokenUsageDatabase::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 1);
    }

    #[test]
    fn ensure_file_creates_zero_cursor_and_is_stable() {
        let (_dir, db) = db();
        let file = db.ensure_file("s1", "/t.jsonl", "claude").unwrap();
        assert_eq!(file.byte_offset, 0);
        assert_eq!(file.state_json, None);
        db.commit_batch("s1", "/t.jsonl", &[], 42, Some("{\"x\":1}"))
            .unwrap();
        let file = db.ensure_file("s1", "/t.jsonl", "claude").unwrap();
        assert_eq!(file.byte_offset, 42);
        assert_eq!(file.state_json.as_deref(), Some("{\"x\":1}"));
    }

    #[test]
    fn commit_batch_is_atomic_for_cursor_state_and_entries() {
        let (_dir, db) = db();
        commit(&db, &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        assert_eq!(
            db.session_buckets("s1").unwrap(),
            vec![("claude-sonnet-4-20250514".to_string(), 600)]
        );
        let agg = db
            .aggregate_bucket("s1", "claude-sonnet-4-20250514", 600)
            .unwrap();
        assert_eq!(agg.input, 10);
        assert_eq!(agg.output, 5);
        assert_eq!(agg.message_count, 1);
        assert_eq!(agg.cost_micro_usd, 1_000);
        assert_eq!(agg.reasoning_output, None);
    }

    #[test]
    fn exact_duplicate_replay_is_a_noop() {
        let (_dir, db) = db();
        let e = entry("m1|r1", Some("m1"), 600, tokens(10, 5));
        commit(&db, std::slice::from_ref(&e));
        let model = "claude-sonnet-4-20250514";
        let before = db.aggregate_bucket("s1", model, 600).unwrap();
        // Identical replay: policy keeps the existing row, aggregate (and
        // therefore the emission fingerprint) is unchanged.
        commit(&db, &[e]);
        assert_eq!(db.aggregate_bucket("s1", model, 600).unwrap(), before);
        let agg = db
            .aggregate_bucket("s1", "claude-sonnet-4-20250514", 600)
            .unwrap();
        assert_eq!(agg.message_count, 1);
    }

    #[test]
    fn streaming_reemit_with_larger_totals_replaces() {
        let (_dir, db) = db();
        commit(&db, &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        commit(&db, &[entry("m1|r1", Some("m1"), 600, tokens(10, 50))]);
        let agg = db
            .aggregate_bucket("s1", "claude-sonnet-4-20250514", 600)
            .unwrap();
        assert_eq!(agg.output, 50);
        assert_eq!(agg.message_count, 1);
        // Smaller totals never replace.
        commit(&db, &[entry("m1|r1", Some("m1"), 600, tokens(1, 1))]);
        assert_eq!(
            db.aggregate_bucket("s1", "claude-sonnet-4-20250514", 600)
                .unwrap(),
            agg
        );
    }

    #[test]
    fn sidechain_replay_with_new_request_id_dedups_to_parent() {
        let (_dir, db) = db();
        commit(&db, &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        let mut replay = entry("m1|r2", Some("m1"), 600, tokens(50_000, 5));
        replay.is_sidechain = true;
        // The sidechain replay matches the parent's row via message id and
        // loses to it despite larger totals.
        commit(&db, &[replay]);
        let agg = db
            .aggregate_bucket("s1", "claude-sonnet-4-20250514", 600)
            .unwrap();
        assert_eq!(agg.input, 10);
        assert_eq!(agg.message_count, 1);
    }

    #[test]
    fn parent_replaces_earlier_sidechain_replay() {
        let (_dir, db) = db();
        let mut replay = entry("m1|r-side", Some("m1"), 600, tokens(50_000, 5));
        replay.is_sidechain = true;
        commit(&db, &[replay]);
        commit(&db, &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        let agg = db
            .aggregate_bucket("s1", "claude-sonnet-4-20250514", 600)
            .unwrap();
        assert_eq!(agg.input, 10);
        assert_eq!(agg.message_count, 1);
        // The winner's identity is now the parent's exact key: replaying the
        // parent entry again leaves the aggregate untouched.
        commit(&db, &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        assert_eq!(
            db.aggregate_bucket("s1", "claude-sonnet-4-20250514", 600)
                .unwrap(),
            agg
        );
    }

    #[test]
    fn distinct_non_sidechain_request_ids_stay_separate() {
        // ccusage parity: the message-id fallback only applies when one side
        // is a sidechain entry.
        let (_dir, db) = db();
        commit(&db, &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        commit(&db, &[entry("m1|r2", Some("m1"), 600, tokens(7, 3))]);
        let agg = db
            .aggregate_bucket("s1", "claude-sonnet-4-20250514", 600)
            .unwrap();
        assert_eq!(agg.message_count, 2);
        assert_eq!(agg.input, 17);
    }

    #[test]
    fn replacement_across_buckets_keeps_both_reconciliation_candidates() {
        let (_dir, db) = db();
        commit(&db, &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        let model = "claude-sonnet-4-20250514".to_string();
        db.mark_emitted("s1", &model, 600, "fp-old", 1).unwrap();
        // Streaming re-emit landed in the next bucket with larger totals: the
        // emptied bucket stays visible via bucket_state, the new one via its
        // entry.
        commit(&db, &[entry("m1|r1", Some("m1"), 900, tokens(10, 50))]);
        let mut buckets = db.session_buckets("s1").unwrap();
        buckets.sort();
        assert_eq!(buckets, vec![(model.clone(), 600), (model.clone(), 900)]);
        assert_eq!(
            db.aggregate_bucket("s1", &model, 600)
                .unwrap()
                .message_count,
            0
        );
        assert_eq!(
            db.aggregate_bucket("s1", &model, 900)
                .unwrap()
                .message_count,
            1
        );
    }

    #[test]
    fn emptied_bucket_aggregates_to_zero_with_changed_fingerprint() {
        let (_dir, db) = db();
        commit(&db, &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        let model = "claude-sonnet-4-20250514";
        let before = db.aggregate_bucket("s1", model, 600).unwrap();
        db.mark_emitted("s1", model, 600, &before.fingerprint(), 1)
            .unwrap();

        commit(&db, &[entry("m1|r1", Some("m1"), 900, tokens(10, 50))]);
        let after = db.aggregate_bucket("s1", model, 600).unwrap();
        assert_eq!(after, BucketAggregate::default());
        assert_ne!(after.fingerprint(), before.fingerprint());
        assert_eq!(
            db.emitted_fingerprint("s1", model, 600).unwrap().as_deref(),
            Some(before.fingerprint().as_str())
        );
    }

    #[test]
    fn fingerprint_covers_every_emitted_field() {
        let base = BucketAggregate {
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write: 4,
            reasoning_output: None,
            total: 10,
            cost_micro_usd: 5,
            message_count: 6,
        };
        let mut seen = HashSet::from([base.fingerprint()]);
        for mutate in [
            (|a: &mut BucketAggregate| a.input += 1) as fn(&mut BucketAggregate),
            |a| a.output += 1,
            |a| a.cache_read += 1,
            |a| a.cache_write += 1,
            |a| a.reasoning_output = Some(0),
            |a| a.total += 1,
            |a| a.cost_micro_usd += 1,
            |a| a.message_count += 1,
        ] {
            let mut variant = base;
            mutate(&mut variant);
            assert!(seen.insert(variant.fingerprint()), "fingerprint collision");
        }
    }

    #[test]
    fn reasoning_tokens_aggregate_when_present() {
        let (_dir, db) = db();
        let mut codex = entry("codex:1", None, 600, tokens(10, 5));
        codex.tokens.reasoning_output = Some(4);
        codex.transcript_cost_micro_usd = None;
        codex.model = "gpt-5".to_string();
        commit(&db, &[codex]);
        let agg = db.aggregate_bucket("s1", "gpt-5", 600).unwrap();
        assert_eq!(agg.reasoning_output, Some(4));
        // Cost falls back to the pricing catalog (gpt-5 is in the snapshot).
        assert!(agg.cost_micro_usd > 0);
    }

    #[test]
    fn record_error_tracks_and_commit_clears() {
        let (_dir, db) = db();
        db.ensure_file("s1", "/t.jsonl", "claude").unwrap();
        db.record_error("s1", "/t.jsonl", "boom").unwrap();
        db.record_error("s1", "/t.jsonl", "boom again").unwrap();
        assert_eq!(
            db.ensure_file("s1", "/t.jsonl", "claude")
                .unwrap()
                .processing_errors,
            2
        );
        db.commit_batch("s1", "/t.jsonl", &[], 10, None).unwrap();
        assert_eq!(
            db.ensure_file("s1", "/t.jsonl", "claude")
                .unwrap()
                .processing_errors,
            0
        );
    }

    #[test]
    fn file_metadata_roundtrip() {
        let (_dir, db) = db();
        db.ensure_file("s1", "/t.jsonl", "claude").unwrap();
        db.update_file_metadata("s1", "/t.jsonl", 1234, Some(99))
            .unwrap();
        let file = db.ensure_file("s1", "/t.jsonl", "claude").unwrap();
        assert_eq!(file.last_known_size, 1234);
        assert_eq!(file.last_modified, Some(99));
    }

    #[test]
    fn prune_drops_old_buckets_and_state() {
        let (_dir, db) = db();
        commit(
            &db,
            &[
                entry("m1|r1", Some("m1"), 600, tokens(10, 5)),
                entry("m2|r2", Some("m2"), 999_900, tokens(1, 1)),
            ],
        );
        let model = "claude-sonnet-4-20250514";
        db.mark_emitted("s1", model, 600, "fp", 1).unwrap();
        assert_eq!(db.prune_buckets_before(999_000).unwrap(), 1);
        assert_eq!(
            db.aggregate_bucket("s1", model, 600).unwrap().message_count,
            0
        );
        assert_eq!(db.emitted_fingerprint("s1", model, 600).unwrap(), None);
        assert_eq!(
            db.aggregate_bucket("s1", model, 999_900)
                .unwrap()
                .message_count,
            1
        );
    }
}
