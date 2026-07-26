//! Metrics storage for local history and offline buffering.
//!
//! Every metric event is stored here. `delivered_ts IS NULL` means the row is
//! still pending upload; delivered rows are retained as the local history.
//! Server handles idempotency.

use crate::error::GitAiError;
use crate::metrics::attrs::attr_pos;
use crate::metrics::events::{checkpoint_pos, otel_trace_pos, session_event_pos};
use crate::metrics::pos_encoded::sparse_get_string;
use crate::metrics::types::{MetricEvent, MetricEventId};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Current schema version (must match MIGRATIONS.len())
const SCHEMA_VERSION: usize = 5;

// This value is part of the metrics retry index schema. Changing it requires a
// migration that rebuilds `metrics_retryable` with the same literal used by
// the retry queries below; SQLite cannot prove a parameterized predicate
// implies a partial-index predicate.
const MAX_METRIC_UPLOAD_ATTEMPTS: u32 = 6;
const METRIC_PROCESSING_LOCK_TIMEOUT_SECS: u64 = 10 * 60;
pub(crate) const METADATA_BACKFILL_BATCH_SIZE: usize = 1000;
const NS_PER_SECOND: u128 = 1_000_000_000;

const RETRYABLE_METRIC_IDS_SQL: &str = "SELECT id FROM metrics \
     WHERE delivered_ts IS NULL \
       AND processing_started_at IS NULL \
       AND next_retry_at <= ?1 \
       AND attempts < 6 \
     ORDER BY next_retry_at ASC, id DESC \
     LIMIT ?2";

/// Database migrations - each migration upgrades the schema by one version
const MIGRATIONS: &[&str] = &[
    // Migration 0 -> 1: Initial schema with metrics table
    r#"
    CREATE TABLE IF NOT EXISTS metrics (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        event_json TEXT NOT NULL
    );
    "#,
    // Migration 1 -> 2: Persistent rate limiter state for agent_usage events
    r#"
    CREATE TABLE IF NOT EXISTS agent_usage_throttle (
        prompt_id TEXT PRIMARY KEY,
        last_sent_ts INTEGER NOT NULL
    );
    "#,
    // Migration 2 -> 3: Keep delivered metrics and add row-level retry state.
    r#"
    CREATE INDEX IF NOT EXISTS metrics_pending_retry
        ON metrics (delivered_ts, next_retry_at, id)
        WHERE delivered_ts IS NULL;

    CREATE INDEX IF NOT EXISTS metrics_processing_started_at
        ON metrics (processing_started_at)
        WHERE delivered_ts IS NULL AND processing_started_at IS NOT NULL;
    "#,
    // Migration 3 -> 4: Cache event metadata for efficient history/backfill queries.
    r#"
    CREATE INDEX IF NOT EXISTS metrics_event_ts_kind
        ON metrics (event_ts, event_kind, id)
        WHERE event_ts IS NOT NULL AND event_kind IS NOT NULL;

    CREATE INDEX IF NOT EXISTS metrics_session_kind_ts
        ON metrics (session_id, event_kind, event_ts, id)
        WHERE session_id IS NOT NULL
            AND event_kind IS NOT NULL
            AND event_ts IS NOT NULL;

    CREATE INDEX IF NOT EXISTS metrics_parent_session_kind_ts
        ON metrics (parent_session_id, event_kind, event_ts, id)
        WHERE parent_session_id IS NOT NULL
            AND event_kind IS NOT NULL
            AND event_ts IS NOT NULL;
    "#,
    // Migration 4 -> 5: Keep terminal history out of retry lookups. The
    // predicate and ordering intentionally match dequeue/count queries.
    r#"
    CREATE INDEX IF NOT EXISTS metrics_retryable
        ON metrics (next_retry_at ASC, id DESC)
        WHERE delivered_ts IS NULL
            AND processing_started_at IS NULL
            AND attempts < 6;

    DROP INDEX IF EXISTS metrics_pending_retry;
    "#,
];

/// Global database singleton
static METRICS_DB: OnceLock<Mutex<MetricsDatabase>> = OnceLock::new();

/// Record returned from database queries
#[derive(Debug, Clone)]
pub struct MetricRecord {
    pub id: i64,
    pub event_json: String,
    pub attempts: u32,
    pub next_retry_at: u64,
}

/// Record returned for local usage aggregation from the metrics table.
#[derive(Debug, Clone)]
pub struct MetricHistoryRecord {
    pub event_id: u16,
    pub ts: u32,
    pub repo_url: Option<String>,
    pub event: MetricEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionEventRecoveryCandidate {
    pub row_id: i64,
    pub event_ts: u32,
    pub session_id: String,
    pub trace_id: Option<String>,
    pub tool: String,
    pub model: Option<String>,
    pub external_session_id: String,
    pub external_tool_use_id: Option<String>,
    pub repo_url: Option<String>,
}

/// Point-in-time status summary for local metric delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsStatus {
    pub total: usize,
    pub delivered: usize,
    pub not_delivered: usize,
    pub pending_retryable: usize,
    pub waiting_retry: usize,
    pub processing: usize,
    pub stopped_after_errors: usize,
    pub rows_with_errors: usize,
    pub latest_error: Option<String>,
}

/// Summary returned by event metadata backfill work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetricMetadataBackfillSummary {
    pub scanned: usize,
    pub updated: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetricEventMetadata {
    event_ts: u32,
    event_kind: u16,
    trace_id: Option<String>,
    session_id: Option<String>,
    parent_session_id: Option<String>,
    tool: Option<String>,
    external_session_id: Option<String>,
    external_parent_session_id: Option<String>,
    external_event_id: Option<String>,
    external_parent_event_id: Option<String>,
    external_tool_use_id: Option<String>,
}

/// Database wrapper for metrics storage
pub struct MetricsDatabase {
    conn: Connection,
}

impl MetricsDatabase {
    /// How long metric rows are retained for local history/offline retry (365 days).
    const METRICS_RETENTION_SECS: u64 = 365 * 24 * 3600;
    /// Minimum interval between prune passes (24 hours).
    const METRICS_PRUNE_INTERVAL_SECS: u64 = 24 * 3600;

    /// Get or initialize the global database
    pub fn global() -> Result<&'static Mutex<MetricsDatabase>, GitAiError> {
        let db_mutex = METRICS_DB.get_or_init(|| match Self::new() {
            Ok(db) => Mutex::new(db),
            Err(e) => {
                eprintln!("[Error] Failed to initialize metrics database: {}", e);
                Mutex::new(
                    Self::new_fallback().expect("Failed to create fallback metrics database"),
                )
            }
        });

        Ok(db_mutex)
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

        let mut db = Self { conn };
        db.initialize_schema()?;

        Ok(db)
    }

    fn new_fallback() -> Result<Self, GitAiError> {
        let temp_path = std::env::temp_dir().join("git-ai-metrics-db-failed");
        Self::new_fallback_at_path(&temp_path)
    }

    fn new_fallback_at_path(path: &std::path::Path) -> Result<Self, GitAiError> {
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

    #[cfg(test)]
    pub(crate) fn new_temp_for_tests() -> Result<(Self, tempfile::TempDir), GitAiError> {
        let temp_dir = tempfile::TempDir::new()?;
        let db_path = temp_dir.path().join("metrics.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            "#,
        )?;

        let mut db = Self { conn };
        db.initialize_schema()?;

        Ok((db, temp_dir))
    }

    #[cfg(any(test, feature = "test-support"))]
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

    /// Get database path: ~/.git-ai/internal/metrics-db
    fn database_path() -> Result<PathBuf, GitAiError> {
        // Allow test override via environment variable
        #[cfg(any(test, feature = "test-support"))]
        if let Ok(test_path) = std::env::var("GIT_AI_TEST_METRICS_DB_PATH") {
            return Ok(PathBuf::from(test_path));
        }

        let internal = crate::config::internal_dir_path().ok_or_else(|| {
            GitAiError::Generic("Could not determine git-ai internal directory".to_string())
        })?;
        Ok(internal.join("metrics-db"))
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
    fn initialize_schema(&mut self) -> Result<(), GitAiError> {
        // FAST PATH: Check if database is already at current version
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
                    "Metrics database schema version {} is newer than supported version {}. \
                     Please upgrade git-ai to the latest version.",
                    current_version, SCHEMA_VERSION
                )));
            }
        }

        // Create schema_metadata table
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            "#,
        )?;

        // Get current schema version (0 if brand new database)
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

        // Apply all missing migrations sequentially
        for target_version in current_version..SCHEMA_VERSION {
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
        }

        Ok(())
    }

    /// Apply a single migration
    fn apply_migration(&mut self, from_version: usize) -> Result<(), GitAiError> {
        if from_version >= MIGRATIONS.len() {
            return Err(GitAiError::Generic(format!(
                "No migration defined for version {} -> {}",
                from_version,
                from_version + 1
            )));
        }

        if from_version == 2 {
            self.add_row_level_retry_columns()?;
        }
        if from_version == 3 {
            self.add_event_metadata_columns()?;
        }

        let migration_sql = MIGRATIONS[from_version];
        let tx = self.conn.transaction()?;
        tx.execute_batch(migration_sql)?;
        tx.commit()?;

        Ok(())
    }

    fn add_row_level_retry_columns(&mut self) -> Result<(), GitAiError> {
        for (name, sql) in [
            (
                "delivered_ts",
                "ALTER TABLE metrics ADD COLUMN delivered_ts INTEGER",
            ),
            (
                "attempts",
                "ALTER TABLE metrics ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "last_sync_error",
                "ALTER TABLE metrics ADD COLUMN last_sync_error TEXT",
            ),
            (
                "last_sync_at",
                "ALTER TABLE metrics ADD COLUMN last_sync_at INTEGER",
            ),
            (
                "next_retry_at",
                "ALTER TABLE metrics ADD COLUMN next_retry_at INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "processing_started_at",
                "ALTER TABLE metrics ADD COLUMN processing_started_at INTEGER",
            ),
        ] {
            self.add_column_if_missing("metrics", name, sql)?;
        }
        Ok(())
    }

    fn add_event_metadata_columns(&mut self) -> Result<(), GitAiError> {
        for (name, sql) in [
            (
                "event_ts",
                "ALTER TABLE metrics ADD COLUMN event_ts INTEGER DEFAULT NULL",
            ),
            (
                "event_kind",
                "ALTER TABLE metrics ADD COLUMN event_kind INTEGER DEFAULT NULL",
            ),
            (
                "trace_id",
                "ALTER TABLE metrics ADD COLUMN trace_id TEXT DEFAULT NULL",
            ),
            (
                "session_id",
                "ALTER TABLE metrics ADD COLUMN session_id TEXT DEFAULT NULL",
            ),
            (
                "parent_session_id",
                "ALTER TABLE metrics ADD COLUMN parent_session_id TEXT DEFAULT NULL",
            ),
            (
                "tool",
                "ALTER TABLE metrics ADD COLUMN tool TEXT DEFAULT NULL",
            ),
            (
                "external_session_id",
                "ALTER TABLE metrics ADD COLUMN external_session_id TEXT DEFAULT NULL",
            ),
            (
                "external_parent_session_id",
                "ALTER TABLE metrics ADD COLUMN external_parent_session_id TEXT DEFAULT NULL",
            ),
            (
                "external_event_id",
                "ALTER TABLE metrics ADD COLUMN external_event_id TEXT DEFAULT NULL",
            ),
            (
                "external_parent_event_id",
                "ALTER TABLE metrics ADD COLUMN external_parent_event_id TEXT DEFAULT NULL",
            ),
            (
                "external_tool_use_id",
                "ALTER TABLE metrics ADD COLUMN external_tool_use_id TEXT DEFAULT NULL",
            ),
        ] {
            self.add_column_if_missing("metrics", name, sql)?;
        }
        Ok(())
    }

    fn add_column_if_missing(
        &mut self,
        table: &str,
        column: &str,
        alter_sql: &str,
    ) -> Result<(), GitAiError> {
        if self.column_exists(table, column)? {
            return Ok(());
        }

        match self.conn.execute(alter_sql, []) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(_, Some(message)))
                if message.contains("duplicate column name") =>
            {
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    fn column_exists(&self, table: &str, column: &str) -> Result<bool, GitAiError> {
        let count: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
            params![column],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Insert undelivered events as JSON strings.
    pub fn insert_events(&mut self, events: &[String]) -> Result<Vec<i64>, GitAiError> {
        self.insert_events_with_delivered_ts(events, None)
    }

    /// Insert events as JSON strings, optionally marking them delivered immediately.
    pub fn insert_events_with_delivered_ts(
        &mut self,
        events: &[String],
        delivered_ts: Option<u64>,
    ) -> Result<Vec<i64>, GitAiError> {
        if events.is_empty() {
            return Ok(Vec::new());
        }

        let tx = self.conn.transaction()?;
        let mut ids = Vec::with_capacity(events.len());

        {
            let mut stmt = tx.prepare_cached(
                r#"
                INSERT INTO metrics (
                    event_json,
                    delivered_ts,
                    event_ts,
                    event_kind,
                    trace_id,
                    session_id,
                    parent_session_id,
                    tool,
                    external_session_id,
                    external_parent_session_id,
                    external_event_id,
                    external_parent_event_id,
                    external_tool_use_id
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                "#,
            )?;

            for event_json in events {
                let metadata = extract_metric_event_metadata(event_json);
                let event_ts = metadata.as_ref().map(|m| i64::from(m.event_ts));
                let event_kind = metadata.as_ref().map(|m| i64::from(m.event_kind));
                let delivered_ts = delivered_ts.map(|ts| ts as i64);

                stmt.execute(params![
                    event_json,
                    delivered_ts,
                    event_ts,
                    event_kind,
                    metadata.as_ref().and_then(|m| m.trace_id.as_deref()),
                    metadata.as_ref().and_then(|m| m.session_id.as_deref()),
                    metadata
                        .as_ref()
                        .and_then(|m| m.parent_session_id.as_deref()),
                    metadata.as_ref().and_then(|m| m.tool.as_deref()),
                    metadata
                        .as_ref()
                        .and_then(|m| m.external_session_id.as_deref()),
                    metadata
                        .as_ref()
                        .and_then(|m| m.external_parent_session_id.as_deref()),
                    metadata
                        .as_ref()
                        .and_then(|m| m.external_event_id.as_deref()),
                    metadata
                        .as_ref()
                        .and_then(|m| m.external_parent_event_id.as_deref()),
                    metadata
                        .as_ref()
                        .and_then(|m| m.external_tool_use_id.as_deref()),
                ])?;
                ids.push(tx.last_insert_rowid());
            }
        }

        tx.commit()?;
        self.prune_old_metrics_if_due()?;
        Ok(ids)
    }

    /// Atomically claim a due batch of pending metrics for upload.
    pub fn dequeue_pending_batch(&mut self, limit: usize) -> Result<Vec<MetricRecord>, GitAiError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let now = current_unix_ts();
        self.release_stale_processing_locks(now)?;

        let tx = self.conn.transaction()?;
        let ids = {
            let mut stmt = tx.prepare(RETRYABLE_METRIC_IDS_SQL)?;
            let rows = stmt.query_map(params![now as i64, limit as i64], |row| {
                row.get::<_, i64>(0)
            })?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row?);
            }
            ids
        };

        if ids.is_empty() {
            tx.commit()?;
            return Ok(Vec::new());
        }

        let mut locked_ids = Vec::with_capacity(ids.len());
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE metrics \
                 SET processing_started_at = ?1 \
                 WHERE id = ?2 \
                   AND delivered_ts IS NULL \
                   AND processing_started_at IS NULL",
            )?;
            for id in ids {
                if stmt.execute(params![now as i64, id])? > 0 {
                    locked_ids.push(id);
                }
            }
        }

        let mut records = Vec::with_capacity(locked_ids.len());
        {
            let mut stmt = tx.prepare_cached(
                "SELECT id, event_json, attempts, next_retry_at FROM metrics WHERE id = ?1",
            )?;
            for id in locked_ids {
                records.push(stmt.query_row(params![id], |row| {
                    Ok(MetricRecord {
                        id: row.get(0)?,
                        event_json: row.get(1)?,
                        attempts: row.get::<_, i64>(2)?.max(0) as u32,
                        next_retry_at: row.get::<_, i64>(3)?.max(0) as u64,
                    })
                })?);
            }
        }

        tx.commit()?;
        Ok(records)
    }

    /// Mark records as delivered after a successful upload.
    pub fn mark_records_delivered(
        &mut self,
        ids: &[i64],
        delivered_ts: u64,
    ) -> Result<(), GitAiError> {
        if ids.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;

        {
            let mut stmt = tx.prepare_cached(
                "UPDATE metrics \
                 SET delivered_ts = ?1, processing_started_at = NULL \
                 WHERE id = ?2 AND delivered_ts IS NULL",
            )?;

            for id in ids {
                stmt.execute(params![delivered_ts as i64, id])?;
            }
        }

        tx.commit()?;
        self.prune_old_metrics_if_due()?;
        Ok(())
    }

    /// Mark records as failed and schedule their next row-level retry.
    pub fn mark_records_failed(
        &mut self,
        ids: &[i64],
        error: &str,
        failed_at: u64,
    ) -> Result<(), GitAiError> {
        if ids.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                r#"
                UPDATE metrics
                SET processing_started_at = NULL,
                    attempts = attempts + 1,
                    last_sync_error = ?1,
                    last_sync_at = ?2,
                    next_retry_at = ?2 + CASE
                        WHEN attempts + 1 <= 1 THEN 300
                        WHEN attempts + 1 = 2 THEN 1800
                        WHEN attempts + 1 = 3 THEN 7200
                        WHEN attempts + 1 = 4 THEN 21600
                        WHEN attempts + 1 = 5 THEN 43200
                        ELSE 86400
                    END
                WHERE id = ?3 AND delivered_ts IS NULL
                "#,
            )?;

            for id in ids {
                stmt.execute(params![error, failed_at as i64, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Mark records as permanently undeliverable while retaining them in history.
    pub fn mark_records_undeliverable(
        &mut self,
        records: &[(i64, String)],
        failed_at: u64,
    ) -> Result<(), GitAiError> {
        if records.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE metrics \
                 SET processing_started_at = NULL, \
                     attempts = ?1, \
                     last_sync_error = ?2, \
                     last_sync_at = ?3, \
                     next_retry_at = ?3 \
                 WHERE id = ?4 AND delivered_ts IS NULL",
            )?;

            for (id, error) in records {
                stmt.execute(params![
                    MAX_METRIC_UPLOAD_ATTEMPTS as i64,
                    error,
                    failed_at as i64,
                    id
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Get count of pending metrics that are currently eligible for upload.
    pub fn count_retryable(&self) -> Result<usize, GitAiError> {
        let now = current_unix_ts();
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM metrics \
             WHERE delivered_ts IS NULL \
               AND processing_started_at IS NULL \
               AND next_retry_at <= ?1 \
               AND attempts < 6",
            params![now as i64],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Summarize local metrics delivery state for user-facing diagnostics.
    pub fn status(&self) -> Result<MetricsStatus, GitAiError> {
        let now = current_unix_ts();
        let (
            total,
            delivered,
            not_delivered,
            pending_retryable,
            waiting_retry,
            processing,
            stopped_after_errors,
            rows_with_errors,
        ): (i64, i64, i64, i64, i64, i64, i64, i64) = self.conn.query_row(
            r#"
            SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN delivered_ts IS NOT NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN delivered_ts IS NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE
                    WHEN delivered_ts IS NULL
                     AND processing_started_at IS NULL
                     AND next_retry_at <= ?1
                     AND attempts < ?2 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE
                    WHEN delivered_ts IS NULL
                     AND processing_started_at IS NULL
                     AND next_retry_at > ?1
                     AND attempts < ?2 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE
                    WHEN delivered_ts IS NULL
                     AND processing_started_at IS NOT NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE
                    WHEN delivered_ts IS NULL
                     AND attempts >= ?2 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE
                    WHEN delivered_ts IS NULL
                     AND last_sync_error IS NOT NULL
                     AND last_sync_error != '' THEN 1 ELSE 0 END), 0)
            FROM metrics
            "#,
            params![now as i64, MAX_METRIC_UPLOAD_ATTEMPTS as i64],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )?;

        let latest_error: Option<String> = self
            .conn
            .query_row(
                "SELECT last_sync_error FROM metrics \
                 WHERE delivered_ts IS NULL \
                   AND last_sync_error IS NOT NULL \
                   AND last_sync_error != '' \
                 ORDER BY COALESCE(last_sync_at, 0) DESC, id DESC \
                 LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;

        Ok(MetricsStatus {
            total: total as usize,
            delivered: delivered as usize,
            not_delivered: not_delivered as usize,
            pending_retryable: pending_retryable as usize,
            waiting_retry: waiting_retry as usize,
            processing: processing as usize,
            stopped_after_errors: stopped_after_errors as usize,
            rows_with_errors: rows_with_errors as usize,
            latest_error,
        })
    }

    fn release_stale_processing_locks(&mut self, now: u64) -> Result<(), GitAiError> {
        let stale_before = now.saturating_sub(METRIC_PROCESSING_LOCK_TIMEOUT_SECS);
        self.conn.execute(
            "UPDATE metrics \
             SET processing_started_at = NULL \
             WHERE delivered_ts IS NULL \
               AND processing_started_at IS NOT NULL \
               AND processing_started_at < ?1",
            params![stale_before as i64],
        )?;
        Ok(())
    }

    /// Delete metric rows outside the local retention window.
    ///
    /// Valid rows are pruned by event timestamp, regardless of delivery state. Malformed
    /// rows cannot be aged by event timestamp, so delivered malformed rows fall back to
    /// `delivered_ts`.
    fn prune_old_metrics_if_due(&mut self) -> Result<(), GitAiError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let last_prune: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'metrics_last_prune_ts'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .and_then(|v: String| v.parse().ok());

        if let Some(last) = last_prune
            && now.saturating_sub(last as u64) < Self::METRICS_PRUNE_INTERVAL_SECS
        {
            return Ok(());
        }

        let cutoff = now.saturating_sub(Self::METRICS_RETENTION_SECS);
        let rows_to_prune = self.old_metric_row_ids(cutoff)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_metadata (key, value) VALUES ('metrics_last_prune_ts', ?1)",
            params![now.to_string()],
        )?;
        {
            let mut stmt = tx.prepare_cached("DELETE FROM metrics WHERE id = ?1")?;
            for id in rows_to_prune {
                stmt.execute(params![id])?;
            }
        }
        tx.commit()?;

        Ok(())
    }

    fn old_metric_row_ids(&self, cutoff: u64) -> Result<Vec<i64>, GitAiError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_json, event_ts, delivered_ts FROM metrics ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?;

        let mut ids = Vec::new();
        for row in rows {
            let (id, event_json, event_ts, delivered_ts) = row?;
            if metric_row_is_older_than_cutoff(&event_json, event_ts, delivered_ts, cutoff) {
                ids.push(id);
            }
        }

        Ok(ids)
    }

    /// Get count of pending metrics.
    pub fn count(&self) -> Result<usize, GitAiError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM metrics WHERE delivered_ts IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Query persisted metric rows since `since_ts` (Unix seconds).
    ///
    /// When `repo_filter` is `Some(url)`, only events matching that repo_url are returned.
    /// An empty string `""` is a sentinel meaning "events with no repo_url (NULL)".
    /// When `None`, all events are returned regardless of repo.
    pub fn get_metric_history(
        &self,
        since_ts: u32,
        repo_filter: Option<&str>,
        event_ids: &[u16],
    ) -> Result<Vec<MetricHistoryRecord>, GitAiError> {
        let mut stmt = self
            .conn
            .prepare("SELECT event_json, event_ts, event_kind FROM metrics WHERE event_ts IS NULL OR event_ts >= ?1 ORDER BY id ASC")?;
        let rows = stmt.query_map(params![since_ts as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })?;

        let mut records = Vec::new();
        for row in rows {
            let (event_json, _cached_ts, cached_kind) = row?;
            if let Some(kind) = cached_kind
                && (0..=u16::MAX as i64).contains(&kind)
                && !event_ids.contains(&(kind as u16))
            {
                continue;
            }

            let Ok(event) = serde_json::from_str::<MetricEvent>(&event_json) else {
                continue;
            };

            if event.timestamp < since_ts || !event_ids.contains(&event.event_id) {
                continue;
            }

            let repo_url = sparse_get_string(&event.attrs, attr_pos::REPO_URL).flatten();
            let repo_matches = match repo_filter {
                None => true,
                Some("") => repo_url.is_none(),
                Some(filter) => repo_url.as_deref().is_some_and(|url| url.contains(filter)),
            };
            if !repo_matches {
                continue;
            }

            records.push(MetricHistoryRecord {
                event_id: event.event_id,
                ts: event.timestamp,
                repo_url,
                event,
            });
        }

        Ok(records)
    }

    pub(crate) fn session_event_candidates_near_timestamps(
        &self,
        timestamps_ns: &[u128],
        window_ns: u128,
    ) -> Result<Vec<SessionEventRecoveryCandidate>, GitAiError> {
        if timestamps_ns.is_empty() {
            return Ok(Vec::new());
        }

        let Some((min_event_ts, max_event_ts)) =
            event_ts_bounds_for_ns_windows(timestamps_ns, window_ns)
        else {
            return Ok(Vec::new());
        };

        let mut stmt = self.conn.prepare(
            r#"
            SELECT
                id,
                event_json,
                event_ts,
                session_id,
                trace_id,
                tool,
                external_session_id,
                external_tool_use_id
            FROM metrics
            WHERE event_kind = ?1
              AND event_ts >= ?2
              AND event_ts <= ?3
              AND session_id IS NOT NULL
              AND session_id != ''
              AND tool IS NOT NULL
              AND tool != ''
              AND tool != 'mock_ai'
              AND external_session_id IS NOT NULL
              AND external_session_id != ''
            ORDER BY id ASC
            "#,
        )?;
        let rows = stmt.query_map(
            params![
                MetricEventId::SessionEvent as i64,
                min_event_ts as i64,
                max_event_ts as i64
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            },
        )?;

        let mut candidates = Vec::new();
        for row in rows {
            let (
                row_id,
                event_json,
                event_ts,
                session_id,
                trace_id,
                tool,
                external_session_id,
                external_tool_use_id,
            ) = row?;
            if event_ts < 0 || event_ts > u32::MAX as i64 {
                continue;
            }
            let event_ts = event_ts as u32;
            if min_distance_to_event_ts(timestamps_ns, event_ts)
                .is_none_or(|distance| distance > window_ns)
            {
                continue;
            }

            let (repo_url, model) = recovery_attrs_from_event_json(&event_json);
            candidates.push(SessionEventRecoveryCandidate {
                row_id,
                event_ts,
                session_id,
                trace_id,
                tool,
                model,
                external_session_id,
                external_tool_use_id,
                repo_url,
            });
        }

        Ok(candidates)
    }

    pub(crate) fn latest_session_event_candidates_for_tools(
        &self,
        tools: &[&str],
    ) -> Result<Vec<SessionEventRecoveryCandidate>, GitAiError> {
        if tools.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = std::iter::repeat_n("?", tools.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            r#"
            SELECT
                id,
                event_json,
                event_ts,
                session_id,
                trace_id,
                tool,
                external_session_id,
                external_tool_use_id
            FROM metrics
            WHERE event_kind = ?1
              AND tool IN ({placeholders})
              AND event_ts IS NOT NULL
              AND session_id IS NOT NULL
              AND session_id != ''
              AND tool IS NOT NULL
              AND tool != ''
              AND tool != 'mock_ai'
              AND external_session_id IS NOT NULL
              AND external_session_id != ''
            ORDER BY event_ts DESC, id DESC
            LIMIT 100
            "#
        );

        let mut values = Vec::with_capacity(tools.len() + 1);
        values.push(rusqlite::types::Value::Integer(
            MetricEventId::SessionEvent as i64,
        ));
        values.extend(
            tools
                .iter()
                .map(|tool| rusqlite::types::Value::Text((*tool).to_string())),
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;

        let mut candidates = Vec::new();
        for row in rows {
            let (
                row_id,
                event_json,
                event_ts,
                session_id,
                trace_id,
                tool,
                external_session_id,
                external_tool_use_id,
            ) = row?;
            if event_ts < 0 || event_ts > u32::MAX as i64 {
                continue;
            }

            let (repo_url, model) = recovery_attrs_from_event_json(&event_json);
            candidates.push(SessionEventRecoveryCandidate {
                row_id,
                event_ts: event_ts as u32,
                session_id,
                trace_id,
                tool,
                model,
                external_session_id,
                external_tool_use_id,
                repo_url,
            });
        }

        Ok(candidates)
    }

    /// Backfill cached event metadata for one bounded batch of legacy rows.
    pub fn backfill_event_metadata_batch(
        &mut self,
        limit: usize,
    ) -> Result<MetricMetadataBackfillSummary, GitAiError> {
        self.backfill_event_metadata_batch_after(0, limit)
            .map(|(summary, _)| summary)
    }

    /// Backfill cached event metadata for all currently eligible legacy rows.
    pub fn backfill_event_metadata(&mut self) -> Result<MetricMetadataBackfillSummary, GitAiError> {
        let mut total = MetricMetadataBackfillSummary::default();
        let mut after_id = 0;

        loop {
            let (summary, last_id) =
                self.backfill_event_metadata_batch_after(after_id, METADATA_BACKFILL_BATCH_SIZE)?;
            total.scanned += summary.scanned;
            total.updated += summary.updated;

            let Some(id) = last_id else {
                break;
            };
            after_id = id;

            if summary.scanned < METADATA_BACKFILL_BATCH_SIZE {
                break;
            }
        }

        Ok(total)
    }

    pub(crate) fn backfill_event_metadata_batch_after(
        &mut self,
        after_id: i64,
        limit: usize,
    ) -> Result<(MetricMetadataBackfillSummary, Option<i64>), GitAiError> {
        if limit == 0 {
            return Ok((MetricMetadataBackfillSummary::default(), None));
        }

        let rows = {
            let mut stmt = self.conn.prepare(
                "SELECT id, event_json FROM metrics \
                 WHERE id > ?1 AND (event_ts IS NULL OR event_kind IS NULL) \
                 ORDER BY id ASC \
                 LIMIT ?2",
            )?;
            let mapped = stmt.query_map(params![after_id, limit as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };

        let mut summary = MetricMetadataBackfillSummary {
            scanned: rows.len(),
            updated: 0,
        };
        let last_id = rows.last().map(|(id, _)| *id);
        if rows.is_empty() {
            return Ok((summary, last_id));
        }

        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                r#"
                UPDATE metrics
                SET event_ts = ?1,
                    event_kind = ?2,
                    trace_id = ?3,
                    session_id = ?4,
                    parent_session_id = ?5,
                    tool = ?6,
                    external_session_id = ?7,
                    external_parent_session_id = ?8,
                    external_event_id = ?9,
                    external_parent_event_id = ?10,
                    external_tool_use_id = ?11
                WHERE id = ?12
                "#,
            )?;

            for (id, event_json) in rows {
                let Some(metadata) = extract_metric_event_metadata(&event_json) else {
                    continue;
                };

                stmt.execute(params![
                    i64::from(metadata.event_ts),
                    i64::from(metadata.event_kind),
                    metadata.trace_id.as_deref(),
                    metadata.session_id.as_deref(),
                    metadata.parent_session_id.as_deref(),
                    metadata.tool.as_deref(),
                    metadata.external_session_id.as_deref(),
                    metadata.external_parent_session_id.as_deref(),
                    metadata.external_event_id.as_deref(),
                    metadata.external_parent_event_id.as_deref(),
                    metadata.external_tool_use_id.as_deref(),
                    id,
                ])?;
                summary.updated += 1;
            }
        }
        tx.commit()?;

        Ok((summary, last_id))
    }

    /// Returns whether an `agent_usage` event should be emitted for this prompt_id.
    ///
    /// If emitted, this method also updates the prompt's last-sent timestamp.
    pub fn should_emit_agent_usage(
        &mut self,
        prompt_id: &str,
        now_ts: u64,
        min_interval_secs: u64,
    ) -> Result<bool, GitAiError> {
        if prompt_id.is_empty() {
            return Ok(true);
        }

        let tx = self.conn.transaction()?;
        let existing_ts: Option<i64> = tx
            .query_row(
                "SELECT last_sent_ts FROM agent_usage_throttle WHERE prompt_id = ?1",
                params![prompt_id],
                |row| row.get(0),
            )
            .optional()?;

        let should_emit = existing_ts
            .map(|prev_ts| now_ts.saturating_sub(prev_ts as u64) >= min_interval_secs)
            .unwrap_or(true);

        if should_emit {
            tx.execute(
                r#"
                INSERT INTO agent_usage_throttle (prompt_id, last_sent_ts)
                VALUES (?1, ?2)
                ON CONFLICT(prompt_id) DO UPDATE SET last_sent_ts = excluded.last_sent_ts
                "#,
                params![prompt_id, now_ts as i64],
            )?;
        }

        tx.commit()?;
        Ok(should_emit)
    }
}

fn current_unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn event_ts_bounds_for_ns_windows(timestamps_ns: &[u128], window_ns: u128) -> Option<(u32, u32)> {
    let mut min_ts: Option<u32> = None;
    let mut max_ts: Option<u32> = None;
    for timestamp_ns in timestamps_ns {
        let start = timestamp_ns.saturating_sub(window_ns) / NS_PER_SECOND;
        let end = timestamp_ns
            .saturating_add(window_ns)
            .min(u32::MAX as u128 * NS_PER_SECOND)
            / NS_PER_SECOND;
        let start = start.min(u32::MAX as u128) as u32;
        let end = end.min(u32::MAX as u128) as u32;
        min_ts = Some(min_ts.map_or(start, |current| current.min(start)));
        max_ts = Some(max_ts.map_or(end, |current| current.max(end)));
    }
    min_ts.zip(max_ts)
}

fn min_distance_to_event_ts(timestamps_ns: &[u128], event_ts: u32) -> Option<u128> {
    timestamps_ns
        .iter()
        .map(|timestamp_ns| distance_to_event_second(*timestamp_ns, event_ts))
        .min()
}

fn distance_to_event_second(timestamp_ns: u128, event_ts: u32) -> u128 {
    let start_ns = event_ts as u128 * NS_PER_SECOND;
    let end_ns = start_ns.saturating_add(NS_PER_SECOND - 1);
    if timestamp_ns < start_ns {
        start_ns - timestamp_ns
    } else {
        timestamp_ns.saturating_sub(end_ns)
    }
}

fn recovery_attrs_from_event_json(event_json: &str) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_str::<Value>(event_json) else {
        return (None, None);
    };
    let attrs = value.get("a").and_then(Value::as_object);
    (
        sparse_object_string(attrs, attr_pos::REPO_URL),
        sparse_object_string(attrs, attr_pos::MODEL),
    )
}

fn metric_row_is_older_than_cutoff(
    event_json: &str,
    event_ts: Option<i64>,
    delivered_ts: Option<i64>,
    cutoff: u64,
) -> bool {
    if let Some(ts) = event_ts
        && ts >= 0
    {
        return (ts as u64) < cutoff;
    }

    if let Some(ts) = extract_metric_event_ts(event_json) {
        return u64::from(ts) < cutoff;
    }

    delivered_ts.is_some_and(|ts| ts >= 0 && (ts as u64) < cutoff)
}

fn extract_metric_event_ts(event_json: &str) -> Option<u32> {
    let value: Value = serde_json::from_str(event_json).ok()?;
    extract_metric_event_ts_from_value(&value)
}

fn extract_metric_event_ts_from_value(value: &Value) -> Option<u32> {
    value
        .get("t")
        .and_then(Value::as_u64)
        .filter(|ts| *ts <= u32::MAX as u64)
        .map(|ts| ts as u32)
}

fn extract_metric_event_metadata(event_json: &str) -> Option<MetricEventMetadata> {
    let value: Value = serde_json::from_str(event_json).ok()?;
    let event_ts = extract_metric_event_ts_from_value(&value)?;
    let event_kind = value
        .get("e")
        .and_then(Value::as_u64)
        .filter(|kind| *kind <= u16::MAX as u64)? as u16;

    let attrs = value.get("a").and_then(Value::as_object);
    let values = value.get("v").and_then(Value::as_object);

    Some(MetricEventMetadata {
        event_ts,
        event_kind,
        trace_id: sparse_object_string(attrs, attr_pos::TRACE_ID),
        session_id: sparse_object_string(attrs, attr_pos::SESSION_ID),
        parent_session_id: sparse_object_string(attrs, attr_pos::PARENT_SESSION_ID),
        tool: sparse_object_string(attrs, attr_pos::TOOL),
        external_session_id: sparse_object_string(attrs, attr_pos::EXTERNAL_SESSION_ID),
        external_parent_session_id: sparse_object_string(
            attrs,
            attr_pos::EXTERNAL_PARENT_SESSION_ID,
        ),
        external_event_id: event_specific_external_event_id(event_kind, values),
        external_parent_event_id: event_specific_external_parent_event_id(event_kind, values),
        external_tool_use_id: event_specific_external_tool_use_id(event_kind, values),
    })
}

fn sparse_object_string(object: Option<&Map<String, Value>>, pos: usize) -> Option<String> {
    object?
        .get(&pos.to_string())
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn event_specific_external_event_id(
    event_kind: u16,
    values: Option<&Map<String, Value>>,
) -> Option<String> {
    if event_kind == MetricEventId::SessionEvent as u16 {
        return sparse_object_string(values, session_event_pos::EXTERNAL_EVENT_ID);
    }
    if event_kind == MetricEventId::OtelTrace as u16 {
        return sparse_object_string(values, otel_trace_pos::EXTERNAL_EVENT_ID);
    }
    None
}

fn event_specific_external_parent_event_id(
    event_kind: u16,
    values: Option<&Map<String, Value>>,
) -> Option<String> {
    if event_kind == MetricEventId::SessionEvent as u16 {
        return sparse_object_string(values, session_event_pos::EXTERNAL_PARENT_EVENT_ID);
    }
    if event_kind == MetricEventId::OtelTrace as u16 {
        return sparse_object_string(values, otel_trace_pos::EXTERNAL_PARENT_EVENT_ID);
    }
    None
}

fn event_specific_external_tool_use_id(
    event_kind: u16,
    values: Option<&Map<String, Value>>,
) -> Option<String> {
    if event_kind == MetricEventId::Checkpoint as u16 {
        return sparse_object_string(values, checkpoint_pos::TOOL_USE_ID);
    }
    if event_kind == MetricEventId::SessionEvent as u16 {
        return sparse_object_string(values, session_event_pos::EXTERNAL_TOOL_USE_ID);
    }
    if event_kind == MetricEventId::OtelTrace as u16 {
        return sparse_object_string(values, otel_trace_pos::EXTERNAL_TOOL_USE_ID);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::StatementStatus;
    use tempfile::TempDir;

    fn create_test_db() -> (MetricsDatabase, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test-metrics.db");

        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();

        let mut db = MetricsDatabase { conn };
        db.initialize_schema().unwrap();

        (db, temp_dir)
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn days_ago(days: u64) -> u32 {
        seconds_ago(days * 24 * 3600)
    }

    fn seconds_ago(seconds: u64) -> u32 {
        unix_now().saturating_sub(seconds).min(u32::MAX as u64) as u32
    }

    fn event_json(ts: u32) -> String {
        format!(r#"{{"t":{ts},"e":1,"v":{{}},"a":{{}}}}"#)
    }

    fn event_json_with_repo(ts: u32, event_id: u16, repo: &str) -> String {
        format!(r#"{{"t":{ts},"e":{event_id},"v":{{}},"a":{{"1":"{repo}"}}}}"#)
    }

    fn pending_event_jsons(db: &MetricsDatabase) -> Vec<String> {
        let mut stmt = db
            .conn
            .prepare("SELECT event_json FROM metrics WHERE delivered_ts IS NULL ORDER BY id DESC")
            .unwrap();
        let rows = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    fn assert_metric_index_exists(db: &MetricsDatabase, index: &str) {
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                params![index],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "missing index {index}");
    }

    fn assert_metric_index_missing(db: &MetricsDatabase, index: &str) {
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                params![index],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "unexpected index {index}");
    }

    fn metric_metadata_rows(db: &MetricsDatabase) -> Vec<(Option<i64>, Option<i64>)> {
        let mut stmt = db
            .conn
            .prepare("SELECT event_ts, event_kind FROM metrics ORDER BY id ASC")
            .unwrap();
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    #[derive(Debug, PartialEq, Eq)]
    struct MetricIdentifierRow {
        trace_id: Option<String>,
        session_id: Option<String>,
        parent_session_id: Option<String>,
        tool: Option<String>,
        external_session_id: Option<String>,
        external_parent_session_id: Option<String>,
        external_event_id: Option<String>,
        external_parent_event_id: Option<String>,
        external_tool_use_id: Option<String>,
    }

    fn metric_identifier_rows(db: &MetricsDatabase) -> Vec<MetricIdentifierRow> {
        let mut stmt = db
            .conn
            .prepare(
                "SELECT trace_id, session_id, parent_session_id, tool, \
                        external_session_id, external_parent_session_id, \
                        external_event_id, external_parent_event_id, external_tool_use_id \
                 FROM metrics ORDER BY id ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok(MetricIdentifierRow {
                    trace_id: row.get(0)?,
                    session_id: row.get(1)?,
                    parent_session_id: row.get(2)?,
                    tool: row.get(3)?,
                    external_session_id: row.get(4)?,
                    external_parent_session_id: row.get(5)?,
                    external_event_id: row.get(6)?,
                    external_parent_event_id: row.get(7)?,
                    external_tool_use_id: row.get(8)?,
                })
            })
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    fn event_json_with_all_common_metadata(ts: u32, event_kind: u16) -> String {
        format!(
            r#"{{
                "t":{ts},
                "e":{event_kind},
                "v":{{}},
                "a":{{
                    "20":"codex",
                    "23":"external-session-1",
                    "24":"session-1",
                    "25":"trace-1",
                    "26":"parent-session-1",
                    "27":"external-parent-session-1"
                }}
            }}"#
        )
    }

    #[test]
    fn test_initialize_schema() {
        let (db, _temp_dir) = create_test_db();

        // Verify metrics table exists
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='metrics'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Verify schema_metadata exists with correct version
        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "5");

        for column in [
            "delivered_ts",
            "attempts",
            "last_sync_error",
            "last_sync_at",
            "next_retry_at",
            "processing_started_at",
            "event_ts",
            "event_kind",
            "trace_id",
            "session_id",
            "parent_session_id",
            "tool",
            "external_session_id",
            "external_parent_session_id",
            "external_event_id",
            "external_parent_event_id",
            "external_tool_use_id",
        ] {
            let column_count: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('metrics') WHERE name = ?1",
                    params![column],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(column_count, 1, "missing column {column}");
        }

        for index in [
            "metrics_retryable",
            "metrics_event_ts_kind",
            "metrics_session_kind_ts",
            "metrics_parent_session_kind_ts",
        ] {
            assert_metric_index_exists(&db, index);
        }
    }

    #[test]
    fn test_fallback_database_initializes_schema() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("fallback-metrics.db");
        let mut db = MetricsDatabase::new_fallback_at_path(&db_path).unwrap();

        db.insert_events(&[event_json(days_ago(1))]).unwrap();

        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_initialize_schema_handles_preexisting_agent_usage_table() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("concurrent-init.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();

        // Simulate a partial migration state from a concurrent process:
        // schema version indicates agent_usage_throttle is missing, but it already exists.
        conn.execute_batch(
            r#"
            CREATE TABLE schema_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            INSERT INTO schema_metadata (key, value) VALUES ('version', '1');
            CREATE TABLE metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_json TEXT NOT NULL
            );
            CREATE TABLE agent_usage_throttle (
                tool TEXT PRIMARY KEY NOT NULL,
                agent_last_seen_at INTEGER NOT NULL,
                command_last_seen_at INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();

        let mut db = MetricsDatabase { conn };
        db.initialize_schema().unwrap();

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "5");
    }

    #[test]
    fn test_migrates_version_2_to_row_level_retry_schema() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("v2.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            INSERT INTO schema_metadata (key, value) VALUES ('version', '2');
            CREATE TABLE metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_json TEXT NOT NULL
            );
            INSERT INTO metrics (event_json) VALUES ('{"t":1,"e":1,"v":{},"a":{}}');
            CREATE TABLE agent_usage_throttle (
                prompt_id TEXT PRIMARY KEY,
                last_sent_ts INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();

        let mut db = MetricsDatabase { conn };
        db.initialize_schema().unwrap();

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "5");
        assert_eq!(db.count().unwrap(), 1);
        assert_eq!(db.count_retryable().unwrap(), 1);
    }

    #[test]
    fn test_migrates_version_2_with_preexisting_retry_columns() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("v2-partial-retry.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            INSERT INTO schema_metadata (key, value) VALUES ('version', '2');
            CREATE TABLE metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_json TEXT NOT NULL,
                delivered_ts INTEGER,
                attempts INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO metrics (event_json) VALUES ('{"t":1,"e":1,"v":{},"a":{}}');
            CREATE TABLE agent_usage_throttle (
                prompt_id TEXT PRIMARY KEY,
                last_sent_ts INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();

        let mut db = MetricsDatabase { conn };
        db.initialize_schema().unwrap();

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "5");

        for column in [
            "delivered_ts",
            "attempts",
            "last_sync_error",
            "last_sync_at",
            "next_retry_at",
            "processing_started_at",
            "event_ts",
            "event_kind",
            "trace_id",
            "session_id",
            "parent_session_id",
            "tool",
            "external_session_id",
            "external_parent_session_id",
            "external_event_id",
            "external_parent_event_id",
            "external_tool_use_id",
        ] {
            assert!(db.column_exists("metrics", column).unwrap());
        }
        assert_eq!(db.count_retryable().unwrap(), 1);
    }

    #[test]
    fn test_migrates_version_3_to_event_metadata_schema_without_sync_backfill() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("v3.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            INSERT INTO schema_metadata (key, value) VALUES ('version', '3');
            CREATE TABLE metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_json TEXT NOT NULL,
                delivered_ts INTEGER,
                attempts INTEGER NOT NULL DEFAULT 0,
                last_sync_error TEXT,
                last_sync_at INTEGER,
                next_retry_at INTEGER NOT NULL DEFAULT 0,
                processing_started_at INTEGER
            );
            INSERT INTO metrics (event_json)
            VALUES ('{"t":1700000000,"e":4,"v":{},"a":{}}');
            CREATE TABLE agent_usage_throttle (
                prompt_id TEXT PRIMARY KEY,
                last_sent_ts INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();

        let mut db = MetricsDatabase { conn };
        db.initialize_schema().unwrap();

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "5");
        assert!(db.column_exists("metrics", "event_ts").unwrap());
        assert!(db.column_exists("metrics", "event_kind").unwrap());
        for index in [
            "metrics_event_ts_kind",
            "metrics_session_kind_ts",
            "metrics_parent_session_kind_ts",
        ] {
            assert_metric_index_exists(&db, index);
        }
        assert_eq!(metric_metadata_rows(&db), vec![(None, None)]);
        assert_eq!(
            metric_identifier_rows(&db),
            vec![MetricIdentifierRow {
                trace_id: None,
                session_id: None,
                parent_session_id: None,
                tool: None,
                external_session_id: None,
                external_parent_session_id: None,
                external_event_id: None,
                external_parent_event_id: None,
                external_tool_use_id: None,
            }]
        );
    }

    #[test]
    fn test_migrates_version_4_to_retryable_only_index() {
        let (mut db, _temp_dir) = create_test_db();
        let ids = db.insert_events(&[event_json(days_ago(1))]).unwrap();
        db.conn
            .execute(
                "UPDATE metrics SET attempts = 6 WHERE id = ?1",
                params![ids[0]],
            )
            .unwrap();
        db.conn
            .execute_batch(
                r#"
                DROP INDEX metrics_retryable;
                CREATE INDEX metrics_pending_retry
                    ON metrics (delivered_ts, next_retry_at, id)
                    WHERE delivered_ts IS NULL;
                UPDATE schema_metadata SET value = '4' WHERE key = 'version';
                "#,
            )
            .unwrap();

        db.initialize_schema().unwrap();

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "5");
        assert_metric_index_exists(&db, "metrics_retryable");
        assert_metric_index_missing(&db, "metrics_pending_retry");
        assert_eq!(db.count().unwrap(), 1);
        assert_eq!(db.status().unwrap().stopped_after_errors, 1);
    }

    #[test]
    fn test_insert_events() {
        let (mut db, _temp_dir) = create_test_db();
        let ts1 = days_ago(2);
        let ts2 = days_ago(1);

        let events = vec![
            format!(r#"{{"t":{ts1},"e":1,"v":{{"0":"abc123"}},"a":{{"0":"1.0.0"}}}}"#),
            format!(r#"{{"t":{ts2},"e":1,"v":{{"0":"def456"}},"a":{{"0":"1.0.0"}}}}"#),
        ];

        let ids = db.insert_events(&events).unwrap();

        let count = db.count().unwrap();
        assert_eq!(count, 2);
        assert_eq!(db.count_retryable().unwrap(), 2);
        assert_eq!(ids.len(), 2);
        assert_eq!(
            metric_metadata_rows(&db),
            vec![(Some(ts1 as i64), Some(1)), (Some(ts2 as i64), Some(1))]
        );
    }

    #[test]
    fn test_insert_events_populates_existing_common_metadata_from_attrs() {
        let (mut db, _temp_dir) = create_test_db();
        let event_ts = days_ago(1);
        db.insert_events(&[event_json_with_all_common_metadata(event_ts, 5)])
            .unwrap();

        let row: (Option<i64>, Option<i64>) = db
            .conn
            .query_row("SELECT event_ts, event_kind FROM metrics", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(row, (Some(event_ts as i64), Some(5)));
        assert_eq!(
            metric_identifier_rows(&db),
            vec![MetricIdentifierRow {
                trace_id: Some("trace-1".to_string()),
                session_id: Some("session-1".to_string()),
                parent_session_id: Some("parent-session-1".to_string()),
                tool: Some("codex".to_string()),
                external_session_id: Some("external-session-1".to_string()),
                external_parent_session_id: Some("external-parent-session-1".to_string()),
                external_event_id: None,
                external_parent_event_id: None,
                external_tool_use_id: None,
            }]
        );
    }

    #[test]
    fn test_insert_events_with_delivered_ts_populates_event_metadata() {
        let (mut db, _temp_dir) = create_test_db();
        let delivered_ts = unix_now();
        let event_ts = days_ago(1);
        db.insert_events_with_delivered_ts(
            &[event_json_with_all_common_metadata(event_ts, 6)],
            Some(delivered_ts),
        )
        .unwrap();

        let row: (Option<i64>, Option<i64>, Option<i64>, Option<String>) = db
            .conn
            .query_row(
                "SELECT event_ts, event_kind, delivered_ts, trace_id FROM metrics",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                Some(event_ts as i64),
                Some(6),
                Some(delivered_ts as i64),
                Some("trace-1".to_string())
            )
        );
    }

    #[test]
    fn test_insert_events_populates_event_specific_external_ids() {
        let (mut db, _temp_dir) = create_test_db();
        let session_event_ts = days_ago(2);
        let otel_trace_ts = days_ago(1);
        let checkpoint_ts = unix_now().min(u32::MAX as u64) as u32;
        let events = vec![
            format!(
                r#"{{
                    "t":{session_event_ts},
                    "e":5,
                    "v":{{"1":"legacy-event","2":"legacy-parent","3":"legacy-tool"}},
                    "a":{{"24":"session-from-attrs"}}
                }}"#
            ),
            format!(
                r#"{{
                    "t":{otel_trace_ts},
                    "e":6,
                    "v":{{"1":"otel-event","2":"otel-parent","3":"otel-tool"}},
                    "a":{{"25":"trace-from-attrs"}}
                }}"#
            ),
            format!(
                r#"{{
                    "t":{checkpoint_ts},
                    "e":4,
                    "v":{{"7":"checkpoint-tool-use"}},
                    "a":{{"20":"claude-code"}}
                }}"#
            ),
        ];

        db.insert_events(&events).unwrap();

        assert_eq!(
            metric_identifier_rows(&db),
            vec![
                MetricIdentifierRow {
                    trace_id: None,
                    session_id: Some("session-from-attrs".to_string()),
                    parent_session_id: None,
                    tool: None,
                    external_session_id: None,
                    external_parent_session_id: None,
                    external_event_id: Some("legacy-event".to_string()),
                    external_parent_event_id: Some("legacy-parent".to_string()),
                    external_tool_use_id: Some("legacy-tool".to_string()),
                },
                MetricIdentifierRow {
                    trace_id: Some("trace-from-attrs".to_string()),
                    session_id: None,
                    parent_session_id: None,
                    tool: None,
                    external_session_id: None,
                    external_parent_session_id: None,
                    external_event_id: Some("otel-event".to_string()),
                    external_parent_event_id: Some("otel-parent".to_string()),
                    external_tool_use_id: Some("otel-tool".to_string()),
                },
                MetricIdentifierRow {
                    trace_id: None,
                    session_id: None,
                    parent_session_id: None,
                    tool: Some("claude-code".to_string()),
                    external_session_id: None,
                    external_parent_session_id: None,
                    external_event_id: None,
                    external_parent_event_id: None,
                    external_tool_use_id: Some("checkpoint-tool-use".to_string()),
                },
            ]
        );
    }

    fn session_event_json(
        ts: u32,
        session_id: &str,
        external_session_id: &str,
        tool: &str,
        repo_url: Option<&str>,
    ) -> String {
        let repo_attr = repo_url
            .map(|url| format!(r#","{}":"{}""#, attr_pos::REPO_URL, url))
            .unwrap_or_default();
        format!(
            r#"{{
                "t":{ts},
                "e":5,
                "v":{{"0":{{"type":"assistant"}},"1":"event-{session_id}","3":"tool-use-{session_id}"}},
                "a":{{
                    "20":"{tool}",
                    "21":"gpt-5",
                    "23":"{external_session_id}",
                    "24":"{session_id}",
                    "25":"trace-{session_id}"
                    {repo_attr}
                }}
            }}"#
        )
    }

    #[test]
    fn test_session_event_candidates_near_timestamps_filters_kind_and_window() {
        let (mut db, _temp_dir) = create_test_db();
        let base_ts = seconds_ago(60);
        let events = vec![
            session_event_json(
                base_ts,
                "session-near",
                "external-near",
                "codex",
                Some("https://github.com/acme/repo"),
            ),
            session_event_json(
                base_ts + 10,
                "session-far",
                "external-far",
                "codex",
                Some("https://github.com/acme/repo"),
            ),
            format!(
                r#"{{
                    "t":{base_ts},
                    "e":4,
                    "v":{{"7":"checkpoint-tool-use"}},
                    "a":{{"20":"codex","23":"external-checkpoint","24":"session-checkpoint"}}
                }}"#
            ),
        ];
        db.insert_events(&events).unwrap();

        let timestamp_ns = (base_ts as u128 * 1_000_000_000) + 500_000_000;
        let candidates = db
            .session_event_candidates_near_timestamps(&[timestamp_ns], 3_000_000_000)
            .unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].event_ts, base_ts);
        assert_eq!(candidates[0].session_id, "session-near");
        assert_eq!(candidates[0].external_session_id, "external-near");
    }

    #[test]
    fn test_session_event_candidates_treat_event_ts_as_second_bucket() {
        let (mut db, _temp_dir) = create_test_db();
        let base_ts = seconds_ago(60);
        db.insert_events(&[session_event_json(
            base_ts,
            "session-bucket",
            "external-bucket",
            "codex",
            Some("https://github.com/acme/repo"),
        )])
        .unwrap();

        let timestamp_ns = base_ts as u128 * NS_PER_SECOND + 3_500_000_000;
        let candidates = db
            .session_event_candidates_near_timestamps(&[timestamp_ns], 3_000_000_000)
            .unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "session-bucket");
    }

    #[test]
    fn test_session_event_candidates_parse_required_and_optional_metadata() {
        let (mut db, _temp_dir) = create_test_db();
        let ts = seconds_ago(30);
        db.insert_events(&[
            session_event_json(
                ts,
                "session-complete",
                "external-complete",
                "claude-code",
                Some("https://github.com/acme/repo"),
            ),
            format!(
                r#"{{
                    "t":{ts},
                    "e":5,
                    "v":{{"0":{{"type":"assistant"}}}},
                    "a":{{"20":"codex","24":"missing-external-session"}}
                }}"#
            ),
        ])
        .unwrap();

        let timestamp_ns = ts as u128 * 1_000_000_000;
        let candidates = db
            .session_event_candidates_near_timestamps(&[timestamp_ns], 3_000_000_000)
            .unwrap();

        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.session_id, "session-complete");
        assert_eq!(
            candidate.trace_id.as_deref(),
            Some("trace-session-complete")
        );
        assert_eq!(candidate.tool, "claude-code");
        assert_eq!(candidate.model.as_deref(), Some("gpt-5"));
        assert_eq!(candidate.external_session_id, "external-complete");
        assert_eq!(
            candidate.external_tool_use_id.as_deref(),
            Some("tool-use-session-complete")
        );
        assert_eq!(
            candidate.repo_url.as_deref(),
            Some("https://github.com/acme/repo")
        );
    }

    #[test]
    fn test_insert_events_leaves_event_metadata_null_for_invalid_json() {
        let (mut db, _temp_dir) = create_test_db();
        let recent_event_ts = days_ago(1);
        let events = vec![
            "not-json".to_string(),
            format!(r#"{{"t":{recent_event_ts},"v":{{}},"a":{{}}}}"#),
            format!(r#"{{"t":{recent_event_ts},"e":null,"v":{{}},"a":{{}}}}"#),
        ];

        db.insert_events(&events).unwrap();

        assert_eq!(
            metric_metadata_rows(&db),
            vec![(None, None), (None, None), (None, None)]
        );
        assert_eq!(
            metric_identifier_rows(&db),
            vec![
                MetricIdentifierRow {
                    trace_id: None,
                    session_id: None,
                    parent_session_id: None,
                    tool: None,
                    external_session_id: None,
                    external_parent_session_id: None,
                    external_event_id: None,
                    external_parent_event_id: None,
                    external_tool_use_id: None,
                },
                MetricIdentifierRow {
                    trace_id: None,
                    session_id: None,
                    parent_session_id: None,
                    tool: None,
                    external_session_id: None,
                    external_parent_session_id: None,
                    external_event_id: None,
                    external_parent_event_id: None,
                    external_tool_use_id: None,
                },
                MetricIdentifierRow {
                    trace_id: None,
                    session_id: None,
                    parent_session_id: None,
                    tool: None,
                    external_session_id: None,
                    external_parent_session_id: None,
                    external_event_id: None,
                    external_parent_event_id: None,
                    external_tool_use_id: None,
                },
            ]
        );
        assert_eq!(db.count().unwrap(), 3);
    }

    #[test]
    fn test_backfill_event_metadata_batch_updates_valid_legacy_rows_only() {
        let (mut db, _temp_dir) = create_test_db();
        let ts1 = days_ago(3);
        let ts2 = days_ago(2);
        db.conn
            .execute(
                "INSERT INTO metrics (event_json) VALUES (?1), (?2), (?3)",
                params![
                    event_json_with_all_common_metadata(ts1, 1),
                    format!(
                        r#"{{"t":{ts2},"e":5,"v":{{"1":"legacy-event","2":"legacy-parent","3":"legacy-tool"}},"a":{{"1":"https://github.com/acme/project"}}}}"#
                    ),
                    "not-json",
                ],
            )
            .unwrap();

        let summary = db.backfill_event_metadata_batch(100).unwrap();

        assert_eq!(summary.scanned, 3);
        assert_eq!(summary.updated, 2);
        assert_eq!(
            metric_metadata_rows(&db),
            vec![
                (Some(ts1 as i64), Some(1)),
                (Some(ts2 as i64), Some(5)),
                (None, None),
            ]
        );
        assert_eq!(
            metric_identifier_rows(&db),
            vec![
                MetricIdentifierRow {
                    trace_id: Some("trace-1".to_string()),
                    session_id: Some("session-1".to_string()),
                    parent_session_id: Some("parent-session-1".to_string()),
                    tool: Some("codex".to_string()),
                    external_session_id: Some("external-session-1".to_string()),
                    external_parent_session_id: Some("external-parent-session-1".to_string()),
                    external_event_id: None,
                    external_parent_event_id: None,
                    external_tool_use_id: None,
                },
                MetricIdentifierRow {
                    trace_id: None,
                    session_id: None,
                    parent_session_id: None,
                    tool: None,
                    external_session_id: None,
                    external_parent_session_id: None,
                    external_event_id: Some("legacy-event".to_string()),
                    external_parent_event_id: Some("legacy-parent".to_string()),
                    external_tool_use_id: Some("legacy-tool".to_string()),
                },
                MetricIdentifierRow {
                    trace_id: None,
                    session_id: None,
                    parent_session_id: None,
                    tool: None,
                    external_session_id: None,
                    external_parent_session_id: None,
                    external_event_id: None,
                    external_parent_event_id: None,
                    external_tool_use_id: None,
                },
            ]
        );
    }

    #[test]
    fn test_backfill_event_metadata_batch_after_advances_cursor() {
        let (mut db, _temp_dir) = create_test_db();
        let ts1 = days_ago(3);
        let ts2 = days_ago(2);
        let ts3 = days_ago(1);
        db.conn
            .execute(
                "INSERT INTO metrics (event_json) VALUES (?1), (?2), (?3)",
                params![event_json(ts1), event_json(ts2), event_json(ts3)],
            )
            .unwrap();

        let (first_summary, first_last_id) = db.backfill_event_metadata_batch_after(0, 2).unwrap();

        assert_eq!(
            first_summary,
            MetricMetadataBackfillSummary {
                scanned: 2,
                updated: 2,
            }
        );
        assert_eq!(
            metric_metadata_rows(&db),
            vec![
                (Some(ts1 as i64), Some(1)),
                (Some(ts2 as i64), Some(1)),
                (None, None),
            ]
        );

        let first_last_id = first_last_id.unwrap();
        let (second_summary, second_last_id) = db
            .backfill_event_metadata_batch_after(first_last_id, 2)
            .unwrap();

        assert_eq!(
            second_summary,
            MetricMetadataBackfillSummary {
                scanned: 1,
                updated: 1,
            }
        );
        assert!(second_last_id.is_some_and(|id| id > first_last_id));
        assert_eq!(
            metric_metadata_rows(&db),
            vec![
                (Some(ts1 as i64), Some(1)),
                (Some(ts2 as i64), Some(1)),
                (Some(ts3 as i64), Some(1)),
            ]
        );

        let (empty_summary, empty_last_id) = db
            .backfill_event_metadata_batch_after(second_last_id.unwrap(), 2)
            .unwrap();
        assert_eq!(empty_summary, MetricMetadataBackfillSummary::default());
        assert_eq!(empty_last_id, None);
    }

    #[test]
    fn test_dequeue_pending_batch_locks_rows() {
        let (mut db, _temp_dir) = create_test_db();
        let events = vec![event_json(days_ago(2)), event_json(days_ago(1))];
        db.insert_events(&events).unwrap();

        let batch = db.dequeue_pending_batch(1).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(db.count().unwrap(), 2);
        assert_eq!(db.count_retryable().unwrap(), 1);

        db.mark_records_delivered(&[batch[0].id], unix_now())
            .unwrap();
        assert_eq!(db.count().unwrap(), 1);
        assert_eq!(db.count_retryable().unwrap(), 1);
    }

    #[test]
    fn test_dequeue_pending_batch_prefers_newest_retryable_rows() {
        let (mut db, _temp_dir) = create_test_db();
        let oldest_ts = days_ago(3);
        let middle_ts = days_ago(2);
        let newest_ts = days_ago(1);
        db.insert_events(&[
            event_json(oldest_ts),
            event_json(middle_ts),
            event_json(newest_ts),
        ])
        .unwrap();

        let batch = db.dequeue_pending_batch(2).unwrap();
        assert_eq!(batch.len(), 2);
        assert!(batch[0].id > batch[1].id);
        assert!(batch[0].event_json.contains(&format!("\"t\":{newest_ts}")));
        assert!(batch[1].event_json.contains(&format!("\"t\":{middle_ts}")));
    }

    #[test]
    fn test_retryable_query_work_is_independent_of_exhausted_history() {
        let (db, _temp_dir) = create_test_db();
        let now = unix_now() as i64;

        db.conn
            .execute(
                "INSERT INTO metrics (event_json, next_retry_at) VALUES (?1, 0)",
                params![event_json(days_ago(1))],
            )
            .unwrap();
        db.conn
            .execute(
                r#"
                WITH RECURSIVE exhausted(n) AS (
                    VALUES(1)
                    UNION ALL
                    SELECT n + 1 FROM exhausted WHERE n < 20000
                )
                INSERT INTO metrics (event_json, attempts, next_retry_at)
                SELECT '{"t":1,"e":1,"v":{},"a":{}}', 6, 0 FROM exhausted
                "#,
                [],
            )
            .unwrap();

        let mut stmt = db.conn.prepare(RETRYABLE_METRIC_IDS_SQL).unwrap();
        let ids = stmt
            .query_map(params![now, 100], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(ids, vec![1]);
        assert_eq!(stmt.get_status(StatementStatus::FullscanStep), 0);
        assert_eq!(stmt.get_status(StatementStatus::Sort), 0);
        assert!(
            stmt.get_status(StatementStatus::VmStep) < 1_000,
            "retryable lookup must not scale with exhausted history"
        );
    }

    #[test]
    fn test_failed_records_do_not_block_unfailed_retryable_rows() {
        let (mut db, _temp_dir) = create_test_db();
        db.insert_events(&[event_json(days_ago(2)), event_json(days_ago(1))])
            .unwrap();

        let batch = db.dequeue_pending_batch(1).unwrap();
        let failed_id = batch[0].id;
        let failed_at = unix_now();
        db.mark_records_failed(&[failed_id], "upload failed", failed_at)
            .unwrap();

        assert_eq!(db.count().unwrap(), 2);
        assert_eq!(db.count_retryable().unwrap(), 1);

        let retryable_batch = db.dequeue_pending_batch(10).unwrap();
        assert_eq!(retryable_batch.len(), 1);
        assert_ne!(retryable_batch[0].id, failed_id);

        let (attempts, next_retry_at): (i64, i64) = db
            .conn
            .query_row(
                "SELECT attempts, next_retry_at FROM metrics WHERE id = ?1",
                params![failed_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(attempts, 1);
        assert!(next_retry_at > failed_at as i64);
    }

    #[test]
    fn test_dequeue_releases_stale_processing_locks() {
        let (mut db, _temp_dir) = create_test_db();
        db.insert_events(&[event_json(days_ago(1))]).unwrap();

        let first_batch = db.dequeue_pending_batch(1).unwrap();
        assert_eq!(first_batch.len(), 1);
        assert_eq!(db.count_retryable().unwrap(), 0);

        let stale_started_at = unix_now().saturating_sub(METRIC_PROCESSING_LOCK_TIMEOUT_SECS + 1);
        db.conn
            .execute(
                "UPDATE metrics SET processing_started_at = ?1 WHERE id = ?2",
                params![stale_started_at as i64, first_batch[0].id],
            )
            .unwrap();

        let second_batch = db.dequeue_pending_batch(1).unwrap();
        assert_eq!(second_batch.len(), 1);
        assert_eq!(second_batch[0].id, first_batch[0].id);
    }

    #[test]
    fn test_max_attempts_are_not_retryable() {
        let (mut db, _temp_dir) = create_test_db();
        let ids = db.insert_events(&[event_json(days_ago(1))]).unwrap();
        db.conn
            .execute(
                "UPDATE metrics SET attempts = ?1 WHERE id = ?2",
                params![MAX_METRIC_UPLOAD_ATTEMPTS as i64, ids[0]],
            )
            .unwrap();

        assert_eq!(db.count().unwrap(), 1);
        assert_eq!(db.count_retryable().unwrap(), 0);
        assert!(db.dequeue_pending_batch(1).unwrap().is_empty());
    }

    #[test]
    fn test_status_counts_delivery_buckets() {
        let (mut db, _temp_dir) = create_test_db();
        let now = unix_now();

        let delivered_ids = db
            .insert_events_with_delivered_ts(&[event_json(days_ago(5))], Some(now))
            .unwrap();
        let delivered_id = delivered_ids[0];
        let ids = db
            .insert_events(&[
                event_json(days_ago(4)),
                event_json(days_ago(3)),
                event_json(days_ago(2)),
                event_json(days_ago(1)),
            ])
            .unwrap();
        let pending_id = ids[0];
        let waiting_id = ids[1];
        let processing_id = ids[2];
        let stopped_id = ids[3];

        db.conn
            .execute(
                "UPDATE metrics \
                 SET last_sync_error = ?1, last_sync_at = ?2 \
                 WHERE id = ?3",
                params![
                    "delivered retry recovered",
                    now.saturating_add(60) as i64,
                    delivered_id
                ],
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE metrics \
                 SET attempts = 1, last_sync_error = ?1, last_sync_at = ?2, next_retry_at = ?3 \
                 WHERE id = ?4",
                params![
                    "temporary outage",
                    now.saturating_sub(10) as i64,
                    now.saturating_add(600) as i64,
                    waiting_id
                ],
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE metrics SET processing_started_at = ?1 WHERE id = ?2",
                params![now as i64, processing_id],
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE metrics \
                 SET attempts = ?1, last_sync_error = ?2, last_sync_at = ?3, next_retry_at = ?3 \
                 WHERE id = ?4",
                params![
                    MAX_METRIC_UPLOAD_ATTEMPTS as i64,
                    "validation failed",
                    now as i64,
                    stopped_id
                ],
            )
            .unwrap();

        assert_ne!(pending_id, waiting_id);
        let status = db.status().unwrap();
        assert_eq!(status.total, 5);
        assert_eq!(status.delivered, 1);
        assert_eq!(status.not_delivered, 4);
        assert_eq!(status.pending_retryable, 1);
        assert_eq!(status.waiting_retry, 1);
        assert_eq!(status.processing, 1);
        assert_eq!(status.stopped_after_errors, 1);
        assert_eq!(status.rows_with_errors, 2);
        assert_eq!(status.latest_error.as_deref(), Some("validation failed"));
    }

    #[test]
    fn test_mark_records_undeliverable_keeps_history_without_retrying() {
        let (mut db, _temp_dir) = create_test_db();
        let event_ts = days_ago(1);
        let ids = db.insert_events(&[event_json(event_ts)]).unwrap();

        let batch = db.dequeue_pending_batch(1).unwrap();
        assert_eq!(batch.len(), 1);
        db.mark_records_undeliverable(&[(ids[0], "validation failed".to_string())], unix_now())
            .unwrap();

        assert_eq!(db.count().unwrap(), 1);
        assert_eq!(db.count_retryable().unwrap(), 0);
        assert!(db.dequeue_pending_batch(1).unwrap().is_empty());
        assert_eq!(db.get_metric_history(0, None, &[1]).unwrap().len(), 1);

        let (delivered_ts, attempts, last_sync_error): (Option<i64>, i64, Option<String>) = db
            .conn
            .query_row(
                "SELECT delivered_ts, attempts, last_sync_error FROM metrics WHERE id = ?1",
                params![ids[0]],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(delivered_ts.is_none());
        assert_eq!(attempts, MAX_METRIC_UPLOAD_ATTEMPTS as i64);
        assert_eq!(last_sync_error.as_deref(), Some("validation failed"));
    }

    #[test]
    fn test_mark_records_delivered() {
        let (mut db, _temp_dir) = create_test_db();
        let ts1 = days_ago(3);
        let ts2 = days_ago(2);
        let ts3 = days_ago(1);

        let events = vec![event_json(ts1), event_json(ts2), event_json(ts3)];

        db.insert_events(&events).unwrap();

        // Dequeue newest rows and mark them delivered.
        let batch = db.dequeue_pending_batch(2).unwrap();
        let ids: Vec<i64> = batch.iter().map(|r| r.id).collect();

        db.mark_records_delivered(&ids, unix_now()).unwrap();

        // Verify only one remains pending.
        let count = db.count().unwrap();
        assert_eq!(count, 1);

        // Verify remaining pending row is the oldest one.
        let remaining = pending_event_jsons(&db);
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].contains(&format!("\"t\":{ts1}")));

        // Verify delivered rows are retained.
        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 3);
    }

    #[test]
    fn test_insert_events_with_delivered_ts_skips_batch() {
        let (mut db, _temp_dir) = create_test_db();

        let delivered_ts = unix_now();
        let delivered_event_ts = days_ago(2);
        let pending_event_ts = days_ago(1);
        let delivered = vec![event_json(delivered_event_ts)];
        let pending = vec![event_json(pending_event_ts)];

        db.insert_events_with_delivered_ts(&delivered, Some(delivered_ts))
            .unwrap();
        db.insert_events(&pending).unwrap();

        let batch = pending_event_jsons(&db);
        assert_eq!(batch.len(), 1);
        assert!(batch[0].contains(&format!("\"t\":{pending_event_ts}")));
        assert_eq!(db.count().unwrap(), 1);

        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 2);
    }

    #[test]
    fn test_get_metric_history_reads_authoritative_metrics_table() {
        let (mut db, _temp_dir) = create_test_db();

        let delivered_ts = unix_now();
        let ts1 = days_ago(4);
        let ts2 = days_ago(3);
        let ts3 = days_ago(2);
        let ts4 = days_ago(1);
        let delivered = vec![event_json_with_repo(
            ts1,
            1,
            "https://github.com/acme/project",
        )];
        let pending = vec![
            event_json_with_repo(ts2, 4, "https://github.com/acme/project"),
            event_json_with_repo(ts3, 2, "https://github.com/acme/project"),
            event_json_with_repo(ts4, 5, "https://github.com/other/repo"),
        ];

        db.insert_events_with_delivered_ts(&delivered, Some(delivered_ts))
            .unwrap();
        db.insert_events(&pending).unwrap();

        let records = db
            .get_metric_history(0, Some("acme/project"), &[1, 4, 5])
            .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].event_id, 1);
        assert_eq!(records[0].ts, ts1);
        assert_eq!(records[1].event_id, 4);
        assert_eq!(records[1].ts, ts2);

        // Delivered rows are retained for history, but only undelivered rows flush.
        assert_eq!(db.count().unwrap(), 3);
    }

    #[test]
    fn test_get_metric_history_reads_legacy_rows_before_and_after_metadata_backfill() {
        let (mut db, _temp_dir) = create_test_db();
        let ts1 = days_ago(2);
        let ts2 = days_ago(1);
        db.conn
            .execute(
                "INSERT INTO metrics (event_json) VALUES (?1), (?2)",
                params![
                    event_json_with_repo(ts1, 4, "https://github.com/acme/project"),
                    event_json_with_repo(ts2, 5, "https://github.com/acme/project"),
                ],
            )
            .unwrap();

        let before = db
            .get_metric_history(0, Some("acme/project"), &[4, 5])
            .unwrap();
        assert_eq!(
            before
                .iter()
                .map(|record| (record.event_id, record.ts))
                .collect::<Vec<_>>(),
            vec![(4, ts1), (5, ts2)]
        );

        let summary = db.backfill_event_metadata_batch(100).unwrap();
        assert_eq!(summary.scanned, 2);
        assert_eq!(summary.updated, 2);

        let after = db
            .get_metric_history(0, Some("acme/project"), &[4, 5])
            .unwrap();
        assert_eq!(
            after
                .iter()
                .map(|record| (record.event_id, record.ts))
                .collect::<Vec<_>>(),
            vec![(4, ts1), (5, ts2)]
        );
    }

    #[test]
    fn test_prunes_metric_rows_older_than_retention_by_event_timestamp() {
        let (mut db, _temp_dir) = create_test_db();

        let delivered_ts = unix_now();
        let old_event_ts = seconds_ago(MetricsDatabase::METRICS_RETENTION_SECS + 1);
        let recent_event_ts = seconds_ago(MetricsDatabase::METRICS_RETENTION_SECS - 1);
        let events = vec![event_json(old_event_ts), event_json(recent_event_ts)];

        db.insert_events_with_delivered_ts(&events, Some(delivered_ts))
            .unwrap();

        let total_after_prune: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total_after_prune, 1);

        let records = db.get_metric_history(0, None, &[1]).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].ts, recent_event_ts);
    }

    #[test]
    fn test_prunes_metric_rows_older_than_retention_by_cached_event_timestamp() {
        let (mut db, _temp_dir) = create_test_db();

        let old_event_ts = seconds_ago(MetricsDatabase::METRICS_RETENTION_SECS + 1);
        let recent_json_ts = days_ago(1);
        db.conn
            .execute(
                "INSERT INTO metrics (event_json, event_ts, event_kind) VALUES (?1, ?2, ?3)",
                params![event_json(recent_json_ts), old_event_ts as i64, 1],
            )
            .unwrap();

        db.prune_old_metrics_if_due().unwrap();

        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn test_prunes_old_pending_metric_rows() {
        let (mut db, _temp_dir) = create_test_db();

        let old_event_ts = seconds_ago(MetricsDatabase::METRICS_RETENTION_SECS + 1);
        let recent_event_ts = days_ago(1);
        let pending = vec![event_json(old_event_ts), event_json(recent_event_ts)];

        db.insert_events(&pending).unwrap();

        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(db.count().unwrap(), 1);

        let batch = pending_event_jsons(&db);
        assert_eq!(batch.len(), 1);
        assert!(batch[0].contains(&format!("\"t\":{recent_event_ts}")));
    }

    #[test]
    fn test_prunes_pending_rows_with_timestamp_even_when_kind_is_missing() {
        let (mut db, _temp_dir) = create_test_db();

        let old_event_ts = seconds_ago(MetricsDatabase::METRICS_RETENTION_SECS + 1);
        let recent_event_ts = days_ago(1);
        let pending = vec![
            format!(r#"{{"t":{old_event_ts},"v":{{}},"a":{{}}}}"#),
            format!(r#"{{"t":{recent_event_ts},"v":{{}},"a":{{}}}}"#),
        ];

        db.insert_events(&pending).unwrap();

        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 1);
        let remaining: String = db
            .conn
            .query_row("SELECT event_json FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert!(remaining.contains(&format!("\"t\":{recent_event_ts}")));
    }

    #[test]
    fn test_prunes_malformed_delivered_rows_by_delivered_timestamp() {
        let (mut db, _temp_dir) = create_test_db();

        let old_delivered_ts =
            unix_now().saturating_sub(MetricsDatabase::METRICS_RETENTION_SECS + 1);
        db.insert_events_with_delivered_ts(&["not-json".to_string()], Some(old_delivered_ts))
            .unwrap();

        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn test_empty_operations() {
        let (mut db, _temp_dir) = create_test_db();

        // Insert empty should succeed
        db.insert_events(&[]).unwrap();

        // Dequeue from empty should return empty.
        let batch = db.dequeue_pending_batch(10).unwrap();
        assert!(batch.is_empty());

        // Marking an empty set delivered should succeed.
        db.mark_records_delivered(&[], 1_700_000_000).unwrap();

        // Count empty should return 0
        let count = db.count().unwrap();
        assert_eq!(count, 0);

        let status = db.status().unwrap();
        assert_eq!(status.total, 0);
        assert_eq!(status.delivered, 0);
        assert_eq!(status.not_delivered, 0);
        assert_eq!(status.pending_retryable, 0);
        assert_eq!(status.waiting_retry, 0);
        assert_eq!(status.processing, 0);
        assert_eq!(status.stopped_after_errors, 0);
        assert_eq!(status.rows_with_errors, 0);
        assert_eq!(status.latest_error, None);
    }

    #[test]
    fn test_database_path() {
        let path = MetricsDatabase::database_path().unwrap();
        assert!(path.to_string_lossy().contains(".git-ai"));
        assert!(path.to_string_lossy().contains("internal"));
        assert!(path.to_string_lossy().ends_with("metrics-db"));
    }

    #[test]
    #[serial_test::serial]
    fn test_database_path_honors_git_ai_internal_dir() {
        // Save and clear the env vars this test mutates so it can't leak state into
        // other in-process tests (env mutation is process-global). Restored on exit.
        let prev_internal = std::env::var_os("GIT_AI_INTERNAL_DIR");
        let prev_metrics_override = std::env::var_os("GIT_AI_TEST_METRICS_DB_PATH");
        // SAFETY: this test is serialized via #[serial], so no other test is reading
        // or writing these env vars concurrently.
        unsafe {
            std::env::remove_var("GIT_AI_TEST_METRICS_DB_PATH");
            std::env::remove_var("GIT_AI_INTERNAL_DIR");
        }

        // When GIT_AI_INTERNAL_DIR is set to an absolute machine-local dir, the metrics
        // DB path must be rerouted to <that dir>/metrics-db (no .git-ai/internal segments).
        let internal_dir = std::env::temp_dir().join("git-ai-metrics-db-reroute-test");
        unsafe { std::env::set_var("GIT_AI_INTERNAL_DIR", &internal_dir) };
        let path = MetricsDatabase::database_path().unwrap();
        assert_eq!(path, internal_dir.join("metrics-db"));

        // Precedence: when BOTH GIT_AI_INTERNAL_DIR and the per-DB test override are set,
        // the test override must still win.
        let override_path = std::env::temp_dir().join("git-ai-metrics-db-explicit-override");
        unsafe { std::env::set_var("GIT_AI_TEST_METRICS_DB_PATH", &override_path) };
        let path = MetricsDatabase::database_path().unwrap();
        assert_eq!(path, override_path);

        // Restore the prior environment so we don't leak into other in-process tests.
        unsafe {
            match prev_internal {
                Some(v) => std::env::set_var("GIT_AI_INTERNAL_DIR", v),
                None => std::env::remove_var("GIT_AI_INTERNAL_DIR"),
            }
            match prev_metrics_override {
                Some(v) => std::env::set_var("GIT_AI_TEST_METRICS_DB_PATH", v),
                None => std::env::remove_var("GIT_AI_TEST_METRICS_DB_PATH"),
            }
        }
    }

    #[test]
    fn test_should_emit_agent_usage_rate_limit() {
        let (mut db, _temp_dir) = create_test_db();
        let prompt_id = "prompt-123";

        // First event for a prompt should be allowed.
        assert!(
            db.should_emit_agent_usage(prompt_id, 1_700_000_000, 300)
                .unwrap()
        );
        // Subsequent event inside the window should be throttled.
        assert!(
            !db.should_emit_agent_usage(prompt_id, 1_700_000_120, 300)
                .unwrap()
        );
        // Event outside the window should be allowed again.
        assert!(
            db.should_emit_agent_usage(prompt_id, 1_700_000_301, 300)
                .unwrap()
        );
    }
}
