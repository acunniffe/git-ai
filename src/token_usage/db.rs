//! Token-usage state database.
//!
//! One SQLite database (`~/.git-ai/internal/token-usage-db`) holds everything
//! the token-usage pipeline needs to stay consistent:
//!
//! - `tracked_files`: the authoritative read cursor (byte offset) and
//!   serialized extractor state per transcript file,
//! - `usage_entries`: deduplicated per-entry token usage,
//! - `bucket_state`: the fingerprint and revision of the last emitted
//!   aggregate per `(session_id, model, bucket_ts)`, so unchanged buckets are
//!   never re-emitted.
//!
//! Keeping the cursor here (rather than in the streams database) matters:
//! [`TokenUsageDatabase::commit_batch`] writes the entries, the extractor
//! state, and the advanced cursor in a single transaction, so a crash can
//! never replay lines against post-batch parser state.
//!
//! Entry deduplication is **global across sessions**, matching ccusage's
//! whole-files dedup: `claude --resume`/`--continue`/fork writes a new
//! transcript whose leading lines are copies of the parent conversation with
//! the original message/request ids, and git-ai tracks that file as a new
//! session. A session-scoped dedup would re-count the copied history on
//! every resume. The first-seen row keeps the entry (and its session
//! attribution); the replacement policy can move a row to the replacing
//! session, in which case [`TokenUsageDatabase::commit_batch`] durably flags
//! the previous session (`needs_reconcile`, same transaction) so its buckets
//! are re-reconciled — without needing that session's files — even across
//! crashes or after its transcripts were deleted.
//!
//! Changed buckets are found by *reconciliation*, not change tracking:
//! [`TokenUsageDatabase::changed_buckets`] aggregates every bucket the
//! session has entries or emission state for in one pass and returns those
//! whose fingerprint differs from the last emitted one. Because no
//! pending-emission state lives in memory, a crash or failed emission is
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
    // Version 2: global dedup (resume/fork dedup crosses sessions),
    // pending-flush marker + error backoff timestamp + rollup identity on
    // tracked files, a durable cross-session reconcile flag, and a
    // per-bucket emission revision. A v1 database may hold cross-session
    // duplicate keys (its dedup was session-scoped); the first-seen row wins
    // so the unique index can be created and later replacements cannot
    // collide on the primary key.
    r#"
    DELETE FROM usage_entries WHERE rowid NOT IN (
        SELECT MIN(rowid) FROM usage_entries GROUP BY entry_key
    );
    CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_entries_key
        ON usage_entries(entry_key);
    CREATE INDEX IF NOT EXISTS idx_usage_entries_message_global
        ON usage_entries(message_id) WHERE message_id IS NOT NULL;

    ALTER TABLE tracked_files ADD COLUMN pending_flush INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE tracked_files ADD COLUMN last_error_at INTEGER;
    ALTER TABLE tracked_files ADD COLUMN external_session_id TEXT NOT NULL DEFAULT '';
    ALTER TABLE tracked_files ADD COLUMN needs_reconcile INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE bucket_state ADD COLUMN emit_seq INTEGER NOT NULL DEFAULT 0;

    INSERT INTO schema_version (version) VALUES (2);
    "#,
];

const TRACKED_FILE_COLUMNS: &str = "session_id, stream_path, tool, byte_offset, state_json, \
     last_known_size, last_modified, processing_errors, last_error_at, pending_flush, \
     external_session_id, needs_reconcile";

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
    pub last_error_at: Option<i64>,
    /// The extractor reported buffered entries at the end of the last pass;
    /// the file must be re-processed even if its bytes have not changed.
    pub pending_flush: bool,
    /// External id of the rollup session (for emission attributes when the
    /// session is reconciled without reading any file).
    pub external_session_id: String,
    /// A cross-session replacement changed this session's buckets; it must
    /// be re-reconciled even if none of its files change (or still exist).
    pub needs_reconcile: bool,
}

fn read_tracked_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrackedFile> {
    Ok(TrackedFile {
        session_id: row.get(0)?,
        stream_path: row.get(1)?,
        tool: row.get(2)?,
        byte_offset: row.get::<_, i64>(3)?.max(0) as u64,
        state_json: row.get(4)?,
        last_known_size: row.get(5)?,
        last_modified: row.get(6)?,
        processing_errors: row.get(7)?,
        last_error_at: row.get(8)?,
        pending_flush: row.get(9)?,
        external_session_id: row.get(10)?,
        needs_reconcile: row.get(11)?,
    })
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

/// One reconciliation candidate: a bucket whose current aggregate differs
/// from the last emitted fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedBucket {
    pub model: String,
    pub bucket_ts: u32,
    pub aggregate: BucketAggregate,
    /// Revision of the last emission for this bucket (0 when never emitted).
    /// The next emission carries `emit_seq + 1` so the server can order
    /// same-second re-emissions.
    pub emit_seq: u64,
}

/// One extracted batch to persist atomically.
pub struct BatchCommit<'a> {
    pub session_id: &'a str,
    pub stream_path: &'a str,
    pub entries: &'a [UsageEntry],
    pub new_offset: u64,
    pub state_json: Option<&'a str>,
    /// The extractor still holds buffered entries (see
    /// `UsageExtractor::has_pending`).
    pub pending_flush: bool,
    /// Retention cutoff: entries whose bucket falls before this are dropped
    /// instead of inserted, so backfill never uploads history the next prune
    /// would delete.
    pub min_bucket_ts: u32,
}

/// Ceiling for any stored token count: one trillion tokens per entry, far
/// beyond anything real. Clamping to a small fraction of i64::MAX (rather
/// than i64::MAX itself) keeps `SUM()` over a bucket from overflowing SQLite
/// integer arithmetic even with millions of clamped rows.
const TOKEN_VALUE_CEILING: u64 = 1_000_000_000_000;

/// Clamp a token count for an INTEGER column (defensive: corrupt transcripts
/// can carry absurd values).
fn to_db_i64(value: u64) -> i64 {
    value.min(TOKEN_VALUE_CEILING) as i64
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

    /// Best-effort removal of the database files (main + WAL/SHM). Used when
    /// the feature flag is off so no collected data is retained.
    pub fn remove_database_files(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut file = path.as_os_str().to_owned();
            file.push(suffix);
            let file = std::path::PathBuf::from(file);
            if file.exists()
                && let Err(e) = std::fs::remove_file(&file)
            {
                tracing::warn!(error = %e, path = %file.display(), "failed to remove token-usage database file");
            }
        }
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
                // Each migration commits atomically: a crash between the
                // statements of a partially applied script (e.g. after one
                // ALTER but before the version row) would otherwise make
                // every later open() fail forever on re-application.
                let tx = conn.unchecked_transaction()?;
                tx.execute_batch(migration_sql)?;
                tx.commit()?;
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
    /// The rollup session's external id is recorded (or backfilled on rows
    /// from before it was tracked) so the session can later be reconciled
    /// without reading any file.
    pub fn ensure_file(
        &self,
        session_id: &str,
        stream_path: &str,
        tool: &str,
        external_session_id: &str,
    ) -> Result<TrackedFile, GitAiError> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR IGNORE INTO tracked_files (session_id, stream_path, tool, external_session_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, stream_path, tool, external_session_id],
        )?;
        conn.execute(
            "UPDATE tracked_files SET external_session_id = ?3
             WHERE session_id = ?1 AND stream_path = ?2 AND external_session_id = ''",
            params![session_id, stream_path, external_session_id],
        )?;
        Ok(conn.query_row(
            &format!(
                "SELECT {TRACKED_FILE_COLUMNS} FROM tracked_files
                 WHERE session_id = ?1 AND stream_path = ?2"
            ),
            params![session_id, stream_path],
            read_tracked_file,
        )?)
    }

    /// All tracked files (sweep enumeration).
    pub fn all_files(&self) -> Result<Vec<TrackedFile>, GitAiError> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare(&format!("SELECT {TRACKED_FILE_COLUMNS} FROM tracked_files"))?;
        let rows = stmt.query_map([], read_tracked_file)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Persist one extracted batch atomically: deduplicated entries, the
    /// advanced read cursor, and the extractor state. Clears any recorded
    /// processing error. When resume/fork dedup moved an entry away from
    /// another session, that session's `needs_reconcile` flag is set in the
    /// same transaction, so the pending re-reconciliation survives crashes,
    /// interrupts, and even the deletion of that session's transcripts.
    pub fn commit_batch(&self, batch: &BatchCommit<'_>) -> Result<(), GitAiError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for entry in batch.entries {
            if bucket_ts(entry.ts) < batch.min_bucket_ts {
                continue;
            }
            if let Some(previous_session) = upsert_entry(&tx, batch.session_id, entry)? {
                tx.execute(
                    "UPDATE tracked_files SET needs_reconcile = 1 WHERE session_id = ?1",
                    params![previous_session],
                )?;
            }
        }
        tx.execute(
            "UPDATE tracked_files
             SET byte_offset = ?1, state_json = ?2, pending_flush = ?3,
                 processing_errors = 0, last_error = NULL, last_error_at = NULL
             WHERE session_id = ?4 AND stream_path = ?5",
            params![
                batch.new_offset as i64,
                batch.state_json,
                batch.pending_flush,
                batch.session_id,
                batch.stream_path
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Sessions flagged for re-reconciliation by a cross-session replacement,
    /// with the identity needed to emit without reading any file.
    pub fn sessions_needing_reconcile(&self) -> Result<Vec<(String, String, String)>, GitAiError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT session_id, external_session_id, tool
             FROM tracked_files WHERE needs_reconcile = 1",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Clear the reconcile flag after the session's buckets were reconciled.
    pub fn clear_needs_reconcile(&self, session_id: &str) -> Result<(), GitAiError> {
        self.lock().execute(
            "UPDATE tracked_files SET needs_reconcile = 0 WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Reconcile the session in one pass: aggregate every bucket it has
    /// entries or emission state for, and return those whose fingerprint
    /// differs from the last emitted one (including buckets that emptied to
    /// zero, which are present only in `bucket_state`).
    pub fn changed_buckets(&self, session_id: &str) -> Result<Vec<ChangedBucket>, GitAiError> {
        let conn = self.lock();

        // (model, bucket_ts) -> (fingerprint, emit_seq) of the last emission.
        let mut stmt = conn.prepare(
            "SELECT model, bucket_ts, emitted_fingerprint, emit_seq
             FROM bucket_state WHERE session_id = ?1",
        )?;
        let emitted: Vec<(String, u32, String, i64)> = stmt
            .query_map(params![session_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut emitted: std::collections::HashMap<(String, u32), (String, u64)> = emitted
            .into_iter()
            .map(|(model, bucket, fp, seq)| ((model, bucket), (fp, seq.max(0) as u64)))
            .collect();

        let mut stmt = conn.prepare(
            "SELECT model, bucket_ts,
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_tokens), 0),
                    COUNT(reasoning_output_tokens),
                    COALESCE(SUM(reasoning_output_tokens), 0),
                    COALESCE(SUM(total_tokens), 0),
                    COALESCE(SUM(cost_micro_usd), 0),
                    COUNT(*)
             FROM usage_entries
             WHERE session_id = ?1
             GROUP BY model, bucket_ts",
        )?;
        let aggregates: Vec<(String, u32, BucketAggregate)> = stmt
            .query_map(params![session_id], |row| {
                let reasoning_entries: i64 = row.get(6)?;
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    BucketAggregate {
                        input: row.get::<_, i64>(2)? as u64,
                        output: row.get::<_, i64>(3)? as u64,
                        cache_read: row.get::<_, i64>(4)? as u64,
                        cache_write: row.get::<_, i64>(5)? as u64,
                        reasoning_output: (reasoning_entries > 0)
                            .then(|| row.get::<_, i64>(7).map(|v| v as u64))
                            .transpose()?,
                        total: row.get::<_, i64>(8)? as u64,
                        cost_micro_usd: row.get::<_, i64>(9)? as u64,
                        message_count: row.get::<_, i64>(10)? as u32,
                    },
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut changed = Vec::new();
        for (model, bucket, aggregate) in aggregates {
            let last = emitted.remove(&(model.clone(), bucket));
            let emit_seq = last.as_ref().map_or(0, |(_, seq)| *seq);
            if last.map(|(fp, _)| fp).as_deref() != Some(aggregate.fingerprint().as_str()) {
                changed.push(ChangedBucket {
                    model,
                    bucket_ts: bucket,
                    aggregate,
                    emit_seq,
                });
            }
        }
        // Buckets with emission state but no remaining entries: emptied, and
        // re-emitted as zero unless zero was already emitted.
        for ((model, bucket), (fingerprint, emit_seq)) in emitted {
            let aggregate = BucketAggregate::default();
            if fingerprint != aggregate.fingerprint() {
                changed.push(ChangedBucket {
                    model,
                    bucket_ts: bucket,
                    aggregate,
                    emit_seq,
                });
            }
        }
        Ok(changed)
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
        now_ts: i64,
    ) -> Result<(), GitAiError> {
        self.lock().execute(
            "UPDATE tracked_files
             SET processing_errors = processing_errors + 1, last_error = ?1, last_error_at = ?2
             WHERE session_id = ?3 AND stream_path = ?4",
            params![error, now_ts, session_id, stream_path],
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

    /// Reserve emission revisions *before* events are handed to the sink:
    /// once a revision may exist in the metrics queue it must never be
    /// reused, or a crash between sink and fingerprint write would produce
    /// two payloads with equal revisions and re-open the tie the revision
    /// exists to eliminate. The fingerprint is deliberately left untouched
    /// (a placeholder '' on first contact) so a failed sink still
    /// re-reconciles; a wasted revision number is harmless.
    pub fn reserve_emit_seqs(
        &self,
        session_id: &str,
        reservations: &[(String, u32, u64)],
    ) -> Result<(), GitAiError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for (model, bucket, emit_seq) in reservations {
            tx.execute(
                "INSERT INTO bucket_state (session_id, model, bucket_ts, emitted_fingerprint, emit_seq, last_emitted_at)
                 VALUES (?1, ?2, ?3, '', ?4, 0)
                 ON CONFLICT(session_id, model, bucket_ts)
                 DO UPDATE SET emit_seq = ?4",
                params![session_id, model, bucket, to_db_i64(*emit_seq)],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Record that the bucket's current aggregate was handed to the metrics
    /// pipeline as revision `emit_seq`.
    pub fn mark_emitted(
        &self,
        session_id: &str,
        model: &str,
        bucket: u32,
        fingerprint: &str,
        emit_seq: u64,
        now_ts: i64,
    ) -> Result<(), GitAiError> {
        self.lock().execute(
            "INSERT INTO bucket_state (session_id, model, bucket_ts, emitted_fingerprint, emit_seq, last_emitted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id, model, bucket_ts)
             DO UPDATE SET emitted_fingerprint = ?4, emit_seq = ?5, last_emitted_at = ?6",
            params![
                session_id,
                model,
                bucket,
                fingerprint,
                to_db_i64(emit_seq),
                now_ts
            ],
        )?;
        Ok(())
    }

    /// Retention prune: drop entries and bucket state older than the cutoff
    /// bucket, atomically (a partial prune would leave orphan bucket_state
    /// rows that re-emit zero over real historical data). Cursors are
    /// monotonic, so pruned history is never re-read.
    pub fn prune_buckets_before(&self, cutoff_bucket_ts: u32) -> Result<usize, GitAiError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let entries = tx.execute(
            "DELETE FROM usage_entries WHERE bucket_ts < ?1",
            params![cutoff_bucket_ts],
        )?;
        tx.execute(
            "DELETE FROM bucket_state WHERE bucket_ts < ?1",
            params![cutoff_bucket_ts],
        )?;
        tx.commit()?;
        Ok(entries)
    }
}

/// A stored entry's dedup-relevant fields.
struct ExistingRow {
    rowid: i64,
    session_id: String,
    replacement: ReplacementCandidate,
}

/// Insert one entry with ccusage's dedup semantics: exact `(message_id,
/// request_id)` identity first (encoded in `entry_key`), then the
/// message-id-only fallback for sidechain replays, with the replacement
/// policy deciding winners. Both lookups are global (see the module docs).
/// Returns the previous owner's session id when a replacement moved the row
/// between sessions.
fn upsert_entry(
    tx: &Transaction<'_>,
    session_id: &str,
    entry: &UsageEntry,
) -> Result<Option<String>, GitAiError> {
    let bucket = bucket_ts(entry.ts);
    let existing = find_dedupe_target(tx, entry)?;
    match existing {
        Some(row) => {
            if should_replace(entry.into(), row.replacement) {
                tx.execute(
                    "UPDATE usage_entries SET
                        session_id = ?1, entry_key = ?2, message_id = ?3, model = ?4,
                        bucket_ts = ?5, input_tokens = ?6, output_tokens = ?7,
                        cache_read_tokens = ?8, cache_write_tokens = ?9,
                        reasoning_output_tokens = ?10, total_tokens = ?11,
                        cost_micro_usd = ?12, is_sidechain = ?13, has_speed = ?14
                     WHERE rowid = ?15",
                    params![
                        session_id,
                        entry.entry_key,
                        entry.message_id,
                        entry.model,
                        bucket,
                        to_db_i64(entry.tokens.input),
                        to_db_i64(entry.tokens.output),
                        to_db_i64(entry.tokens.cache_read),
                        to_db_i64(entry.tokens.cache_write),
                        entry.tokens.reasoning_output.map(to_db_i64),
                        to_db_i64(entry.tokens.total),
                        entry_cost_micro_usd(entry).map(to_db_i64),
                        entry.is_sidechain,
                        entry.has_speed,
                        row.rowid,
                    ],
                )?;
                if row.session_id != session_id {
                    return Ok(Some(row.session_id));
                }
            }
            Ok(None)
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
                    to_db_i64(entry.tokens.input),
                    to_db_i64(entry.tokens.output),
                    to_db_i64(entry.tokens.cache_read),
                    to_db_i64(entry.tokens.cache_write),
                    entry.tokens.reasoning_output.map(to_db_i64),
                    to_db_i64(entry.tokens.total),
                    entry_cost_micro_usd(entry).map(to_db_i64),
                    entry.is_sidechain,
                    entry.has_speed,
                ],
            )?;
            Ok(None)
        }
    }
}

fn find_dedupe_target(
    tx: &Transaction<'_>,
    entry: &UsageEntry,
) -> Result<Option<ExistingRow>, GitAiError> {
    let read_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<ExistingRow> {
        let token_total = (row.get::<_, i64>(2)? as u64)
            .saturating_add(row.get::<_, i64>(3)? as u64)
            .saturating_add(row.get::<_, i64>(4)? as u64)
            .saturating_add(row.get::<_, i64>(5)? as u64);
        Ok(ExistingRow {
            rowid: row.get(0)?,
            session_id: row.get(1)?,
            replacement: ReplacementCandidate {
                token_total,
                is_sidechain: row.get(6)?,
                has_speed: row.get(7)?,
            },
        })
    };
    const ROW_COLUMNS: &str = "rowid, session_id, input_tokens, output_tokens, \
                               cache_read_tokens, cache_write_tokens, is_sidechain, has_speed";

    let exact = tx
        .query_row(
            &format!(
                "SELECT {ROW_COLUMNS} FROM usage_entries WHERE entry_key = ?1
                 ORDER BY rowid LIMIT 1"
            ),
            params![entry.entry_key],
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
         WHERE message_id = ?1 AND (?2 OR is_sidechain)
         ORDER BY rowid LIMIT 1"
    ))?;
    Ok(stmt
        .query_row(params![message_id, entry.is_sidechain], read_row)
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

    fn commit_for(db: &TokenUsageDatabase, session: &str, entries: &[UsageEntry]) {
        let path = format!("/{session}.jsonl");
        db.ensure_file(session, &path, "claude", &format!("{session}-ext"))
            .unwrap();
        db.commit_batch(&BatchCommit {
            session_id: session,
            stream_path: &path,
            entries,
            new_offset: 0,
            state_json: None,
            pending_flush: false,
            min_bucket_ts: 0,
        })
        .unwrap()
    }

    fn commit(db: &TokenUsageDatabase, entries: &[UsageEntry]) {
        commit_for(db, "s1", entries);
    }

    #[test]
    fn migrations_apply_and_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-usage-db");
        let db = TokenUsageDatabase::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 2);
        drop(db);
        let db = TokenUsageDatabase::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 2);
    }

    #[test]
    fn ensure_file_creates_zero_cursor_and_is_stable() {
        let (_dir, db) = db();
        let file = db
            .ensure_file("s1", "/t.jsonl", "claude", "s1-ext")
            .unwrap();
        assert_eq!(file.byte_offset, 0);
        assert_eq!(file.state_json, None);
        assert!(!file.pending_flush);
        db.commit_batch(&BatchCommit {
            session_id: "s1",
            stream_path: "/t.jsonl",
            entries: &[],
            new_offset: 42,
            state_json: Some("{\"x\":1}"),
            pending_flush: true,
            min_bucket_ts: 0,
        })
        .unwrap();
        let file = db
            .ensure_file("s1", "/t.jsonl", "claude", "s1-ext")
            .unwrap();
        assert_eq!(file.byte_offset, 42);
        assert_eq!(file.state_json.as_deref(), Some("{\"x\":1}"));
        assert!(file.pending_flush);
    }

    #[test]
    fn commit_batch_persists_entries_and_reports_changed_buckets() {
        let (_dir, db) = db();
        commit(&db, &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        let changed = db.changed_buckets("s1").unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].model, "claude-sonnet-4-20250514");
        assert_eq!(changed[0].bucket_ts, 600);
        assert_eq!(changed[0].emit_seq, 0);
        let agg = changed[0].aggregate;
        assert_eq!(agg.input, 10);
        assert_eq!(agg.output, 5);
        assert_eq!(agg.message_count, 1);
        assert_eq!(agg.cost_micro_usd, 1_000);
        assert_eq!(agg.reasoning_output, None);
    }

    #[test]
    fn resumed_session_does_not_recount_copied_history() {
        // claude --resume copies the parent conversation into a NEW session
        // file with the original message/request ids; dedup must be global.
        let (_dir, db) = db();
        commit_for(&db, "s1", &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        commit_for(&db, "s2", &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        // Identical copy must not replace, so nothing needs reconciling.
        assert!(db.sessions_needing_reconcile().unwrap().is_empty());
        // The entry stays attributed to the first-seen session.
        assert_eq!(
            db.aggregate_bucket("s1", "claude-sonnet-4-20250514", 600)
                .unwrap()
                .message_count,
            1
        );
        assert_eq!(
            db.aggregate_bucket("s2", "claude-sonnet-4-20250514", 600)
                .unwrap()
                .message_count,
            0
        );
    }

    #[test]
    fn cross_session_replacement_flags_the_previous_owner_durably() {
        // The resumed copy carries larger totals (the original was a partial
        // streaming re-emit): the replacement moves the entry to the new
        // session, and the old session is durably flagged for
        // reconciliation in the same transaction — identity included, so no
        // file of that session is ever needed again.
        let (_dir, db) = db();
        commit_for(&db, "s1", &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        commit_for(
            &db,
            "s2",
            &[entry("m1|r1", Some("m1"), 600, tokens(10, 50))],
        );
        assert_eq!(
            db.sessions_needing_reconcile().unwrap(),
            vec![("s1".to_string(), "s1-ext".to_string(), "claude".to_string())]
        );
        db.clear_needs_reconcile("s1").unwrap();
        assert!(db.sessions_needing_reconcile().unwrap().is_empty());
        assert_eq!(
            db.aggregate_bucket("s1", "claude-sonnet-4-20250514", 600)
                .unwrap()
                .message_count,
            0
        );
        assert_eq!(
            db.aggregate_bucket("s2", "claude-sonnet-4-20250514", 600)
                .unwrap()
                .output,
            50
        );
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
        assert_eq!(before.message_count, 1);
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
    fn changed_buckets_skips_unchanged_and_reports_emptied() {
        let (_dir, db) = db();
        let model = "claude-sonnet-4-20250514";
        commit(&db, &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        let changed = db.changed_buckets("s1").unwrap();
        assert_eq!(changed.len(), 1);
        db.mark_emitted(
            "s1",
            model,
            600,
            &changed[0].aggregate.fingerprint(),
            changed[0].emit_seq + 1,
            1,
        )
        .unwrap();
        // Nothing changed: no candidates.
        assert!(db.changed_buckets("s1").unwrap().is_empty());

        // The entry moves to the next bucket: the emptied bucket re-emits
        // zero (with the bumped revision) and the new bucket emits.
        commit(&db, &[entry("m1|r1", Some("m1"), 900, tokens(10, 50))]);
        let mut changed = db.changed_buckets("s1").unwrap();
        changed.sort_by_key(|c| c.bucket_ts);
        assert_eq!(changed.len(), 2);
        assert_eq!(changed[0].bucket_ts, 600);
        assert_eq!(changed[0].aggregate, BucketAggregate::default());
        assert_eq!(changed[0].emit_seq, 1);
        assert_eq!(changed[1].bucket_ts, 900);
        assert_eq!(changed[1].aggregate.output, 50);
        assert_eq!(changed[1].emit_seq, 0);

        // Emit the zero once; it never comes back.
        db.mark_emitted(
            "s1",
            model,
            600,
            &BucketAggregate::default().fingerprint(),
            2,
            2,
        )
        .unwrap();
        let changed = db.changed_buckets("s1").unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].bucket_ts, 900);
    }

    #[test]
    fn entries_before_the_retention_cutoff_are_dropped_at_insert() {
        let (_dir, db) = db();
        db.ensure_file("s1", "/t.jsonl", "claude", "s1-ext")
            .unwrap();
        db.commit_batch(&BatchCommit {
            session_id: "s1",
            stream_path: "/t.jsonl",
            entries: &[
                entry("m1|r1", Some("m1"), 600, tokens(10, 5)),
                entry("m2|r2", Some("m2"), 999_900, tokens(1, 1)),
            ],
            new_offset: 0,
            state_json: None,
            pending_flush: false,
            min_bucket_ts: 900,
        })
        .unwrap();
        let changed = db.changed_buckets("s1").unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].bucket_ts, 999_900);
    }

    #[test]
    fn v1_databases_with_legacy_cross_session_duplicates_upgrade_cleanly() {
        // A v1 database (session-scoped dedup) can hold the same entry_key
        // under several sessions. The v2 migration keeps the first-seen row
        // and enforces global uniqueness, so a later cross-session
        // replacement can never collide on the (session_id, entry_key)
        // primary key and poison the transcript.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-usage-db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            for session in ["s1", "s2", "s3"] {
                conn.execute(
                    "INSERT INTO usage_entries (session_id, entry_key, message_id, model, bucket_ts,
                         input_tokens, output_tokens, total_tokens)
                     VALUES (?1, 'm1|r1', 'm1', 'claude-sonnet-4-20250514', 600, 10, 5, 15)",
                    params![session],
                )
                .unwrap();
            }
        }

        let db = TokenUsageDatabase::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 2);
        // First-seen row (s1) survives; the duplicates are gone.
        assert_eq!(
            db.aggregate_bucket("s1", "claude-sonnet-4-20250514", 600)
                .unwrap()
                .message_count,
            1
        );
        assert_eq!(
            db.aggregate_bucket("s2", "claude-sonnet-4-20250514", 600)
                .unwrap()
                .message_count,
            0
        );
        // A cross-session replacement over the legacy key works (this used
        // to violate the primary key when a duplicate existed).
        commit_for(
            &db,
            "s2",
            &[entry("m1|r1", Some("m1"), 600, tokens(10, 50))],
        );
        assert_eq!(
            db.aggregate_bucket("s2", "claude-sonnet-4-20250514", 600)
                .unwrap()
                .output,
            50
        );
    }

    #[test]
    fn reserved_revisions_are_never_reused() {
        // Reservation happens before the sink: a crash between sink and
        // fingerprint write must not lead to a second payload with an equal
        // revision.
        let (_dir, db) = db();
        let model = "claude-sonnet-4-20250514";
        commit(&db, &[entry("m1|r1", Some("m1"), 600, tokens(10, 5))]);
        let changed = db.changed_buckets("s1").unwrap();
        assert_eq!(changed[0].emit_seq, 0);
        db.reserve_emit_seqs("s1", &[(model.to_string(), 600, 1)])
            .unwrap();

        // Fingerprint untouched by the reservation: the bucket still
        // reconciles (as if the sink crashed), now at the next revision.
        let changed = db.changed_buckets("s1").unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].emit_seq, 1);

        // Success path completes the emission; nothing left to reconcile.
        db.reserve_emit_seqs("s1", &[(model.to_string(), 600, 2)])
            .unwrap();
        db.mark_emitted("s1", model, 600, &changed[0].aggregate.fingerprint(), 2, 9)
            .unwrap();
        assert!(db.changed_buckets("s1").unwrap().is_empty());
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
    fn absurd_token_values_clamp_instead_of_wrapping() {
        let (_dir, db) = db();
        // Two clamped rows in one bucket: the aggregate SUM must not
        // overflow SQLite integer arithmetic (which would make the session's
        // reconciliation error forever).
        for (key, msg) in [("m1|r1", "m1"), ("m2|r2", "m2")] {
            let mut e = entry(key, Some(msg), 600, tokens(0, 1));
            e.tokens.input = u64::MAX;
            e.tokens.total = u64::MAX;
            e.transcript_cost_micro_usd = Some(1);
            commit(&db, &[e]);
        }
        let agg = db
            .aggregate_bucket("s1", "claude-sonnet-4-20250514", 600)
            .unwrap();
        assert_eq!(agg.input, 2 * TOKEN_VALUE_CEILING);
        assert_eq!(agg.total, 2 * TOKEN_VALUE_CEILING);
        assert_eq!(db.changed_buckets("s1").unwrap().len(), 1);
    }

    #[test]
    fn record_error_tracks_and_commit_clears() {
        let (_dir, db) = db();
        db.ensure_file("s1", "/t.jsonl", "claude", "s1-ext")
            .unwrap();
        db.record_error("s1", "/t.jsonl", "boom", 100).unwrap();
        db.record_error("s1", "/t.jsonl", "boom again", 200)
            .unwrap();
        let file = db
            .ensure_file("s1", "/t.jsonl", "claude", "s1-ext")
            .unwrap();
        assert_eq!(file.processing_errors, 2);
        assert_eq!(file.last_error_at, Some(200));
        db.commit_batch(&BatchCommit {
            session_id: "s1",
            stream_path: "/t.jsonl",
            entries: &[],
            new_offset: 10,
            state_json: None,
            pending_flush: false,
            min_bucket_ts: 0,
        })
        .unwrap();
        let file = db
            .ensure_file("s1", "/t.jsonl", "claude", "s1-ext")
            .unwrap();
        assert_eq!(file.processing_errors, 0);
        assert_eq!(file.last_error_at, None);
    }

    #[test]
    fn file_metadata_roundtrip() {
        let (_dir, db) = db();
        db.ensure_file("s1", "/t.jsonl", "claude", "s1-ext")
            .unwrap();
        db.update_file_metadata("s1", "/t.jsonl", 1234, Some(99))
            .unwrap();
        let file = db
            .ensure_file("s1", "/t.jsonl", "claude", "s1-ext")
            .unwrap();
        assert_eq!(file.last_known_size, 1234);
        assert_eq!(file.last_modified, Some(99));
    }

    #[test]
    fn prune_drops_old_buckets_and_state_atomically() {
        let (_dir, db) = db();
        commit(
            &db,
            &[
                entry("m1|r1", Some("m1"), 600, tokens(10, 5)),
                entry("m2|r2", Some("m2"), 999_900, tokens(1, 1)),
            ],
        );
        let model = "claude-sonnet-4-20250514";
        db.mark_emitted("s1", model, 600, "fp", 1, 1).unwrap();
        assert_eq!(db.prune_buckets_before(999_000).unwrap(), 1);
        assert_eq!(
            db.aggregate_bucket("s1", model, 600).unwrap().message_count,
            0
        );
        assert_eq!(db.emitted_fingerprint("s1", model, 600).unwrap(), None);
        // No orphan bucket_state row: the emptied old bucket is NOT a
        // reconciliation candidate after the prune.
        let changed = db.changed_buckets("s1").unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].bucket_ts, 999_900);
    }

    #[test]
    fn remove_database_files_deletes_main_and_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-usage-db");
        let db = TokenUsageDatabase::open(&path).unwrap();
        drop(db);
        assert!(path.exists());
        TokenUsageDatabase::remove_database_files(&path);
        assert!(!path.exists());
        assert!(!path.with_file_name("token-usage-db-wal").exists());
    }
}
