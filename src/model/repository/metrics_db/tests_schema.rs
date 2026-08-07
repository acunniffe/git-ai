use super::schema::column_exists;
use super::test_support::*;
use super::*;
use crate::model::repository::sqlite::assert_persisted_schema_version;
use rusqlite::params;
use tempfile::TempDir;

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
    assert_persisted_schema_version(&db.conn, "5");

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
    let conn = crate::model::repository::sqlite::open_with_memory_limits(&db_path).unwrap();

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

    assert_persisted_schema_version(&db.conn, "5");
}

#[test]
fn test_migrates_version_2_to_row_level_retry_schema() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("v2.db");
    let conn = crate::model::repository::sqlite::open_with_memory_limits(&db_path).unwrap();
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

    assert_persisted_schema_version(&db.conn, "5");
    assert_eq!(db.count().unwrap(), 1);
    assert_eq!(db.count_retryable().unwrap(), 1);
}

#[test]
fn test_migrates_version_2_with_preexisting_retry_columns() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("v2-partial-retry.db");
    let conn = crate::model::repository::sqlite::open_with_memory_limits(&db_path).unwrap();
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

    assert_persisted_schema_version(&db.conn, "5");

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
        assert!(column_exists(&db.conn, "metrics", column).unwrap());
    }
    assert_eq!(db.count_retryable().unwrap(), 1);
}

#[test]
fn test_migrates_version_3_to_event_metadata_schema_without_sync_backfill() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("v3.db");
    let conn = crate::model::repository::sqlite::open_with_memory_limits(&db_path).unwrap();
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

    assert_persisted_schema_version(&db.conn, "5");
    assert!(column_exists(&db.conn, "metrics", "event_ts").unwrap());
    assert!(column_exists(&db.conn, "metrics", "event_kind").unwrap());
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

    assert_persisted_schema_version(&db.conn, "5");
    assert_metric_index_exists(&db, "metrics_retryable");
    assert_metric_index_missing(&db, "metrics_pending_retry");
    assert_eq!(db.count().unwrap(), 1);
    assert_eq!(db.status().unwrap().stopped_after_errors, 1);
}

#[test]
fn test_database_path() {
    let path = MetricsDatabase::database_path().unwrap();
    assert!(path.to_string_lossy().contains(".git-ai"));
    assert!(path.to_string_lossy().contains("internal"));
    assert!(path.to_string_lossy().ends_with("metrics-db"));
}
