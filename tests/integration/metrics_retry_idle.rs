use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::metrics::db::MetricsDatabase;
use git_ai::sqlite::open_with_memory_limits;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

fn seed_version_4_metrics_db(path: &Path) {
    let conn = open_with_memory_limits(path).unwrap();
    conn.execute_batch(
        r#"
        PRAGMA journal_mode=WAL;
        CREATE TABLE schema_metadata (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        );
        INSERT INTO schema_metadata (key, value) VALUES ('version', '4');
        CREATE TABLE metrics (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_json TEXT NOT NULL,
            delivered_ts INTEGER,
            attempts INTEGER NOT NULL DEFAULT 0,
            last_sync_error TEXT,
            last_sync_at INTEGER,
            next_retry_at INTEGER NOT NULL DEFAULT 0,
            processing_started_at INTEGER,
            event_ts INTEGER DEFAULT NULL,
            event_kind INTEGER DEFAULT NULL,
            trace_id TEXT DEFAULT NULL,
            session_id TEXT DEFAULT NULL,
            parent_session_id TEXT DEFAULT NULL,
            tool TEXT DEFAULT NULL,
            external_session_id TEXT DEFAULT NULL,
            external_parent_session_id TEXT DEFAULT NULL,
            external_event_id TEXT DEFAULT NULL,
            external_parent_event_id TEXT DEFAULT NULL,
            external_tool_use_id TEXT DEFAULT NULL
        );
        CREATE INDEX metrics_pending_retry
            ON metrics (delivered_ts, next_retry_at, id)
            WHERE delivered_ts IS NULL;
        CREATE INDEX metrics_processing_started_at
            ON metrics (processing_started_at)
            WHERE delivered_ts IS NULL AND processing_started_at IS NOT NULL;
        WITH RECURSIVE exhausted(n) AS (
            VALUES(1)
            UNION ALL
            SELECT n + 1 FROM exhausted WHERE n < 20000
        )
        INSERT INTO metrics (
            event_json,
            attempts,
            last_sync_error,
            next_retry_at,
            event_ts,
            event_kind,
            tool
        )
        SELECT
            '{"t":1,"e":1,"v":{},"a":{}}',
            6,
            'Generic error: Unauthorized',
            0,
            unixepoch(),
            1,
            'codex'
        FROM exhausted;
        "#,
    )
    .unwrap();
}

fn wait_for_schema_migration(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let conn = open_with_memory_limits(path).unwrap();
        let version: String = conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if version == "5" {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "metrics database did not migrate to schema v5"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn idle_metrics_flush_skips_git_identity_lookup() {
    let spawn_log_dir = tempfile::tempdir().unwrap();
    let spawn_log_path = spawn_log_dir.path().join("git-spawns.log");
    let spawn_log = spawn_log_path.to_string_lossy().to_string();
    let metrics_path = spawn_log_dir.path().join("metrics.db");
    let metrics_db = metrics_path.to_string_lossy().to_string();
    MetricsDatabase::open_at_path(&metrics_path).unwrap();
    let repo = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_API_KEY", "test-api-key"),
        ("GIT_AI_API_BASE_URL", "http://127.0.0.1:1"),
        ("GIT_AI_DAEMON_LOG_UPLOAD", "false"),
        ("GIT_AI_SPAWN_LOG", spawn_log.as_str()),
        ("GIT_AI_TEST_METRICS_DB_PATH", metrics_db.as_str()),
        ("GIT_AI_TRANSCRIPT_STREAMING", "false"),
    ]);

    // Drain startup work before observing two complete idle flush intervals.
    repo.sync_daemon();
    open_with_memory_limits(&metrics_path)
        .unwrap()
        .execute("DELETE FROM metrics", [])
        .unwrap();
    fs::write(&spawn_log_path, "").unwrap();
    std::thread::sleep(Duration::from_secs(7));

    let git_spawns = fs::read_to_string(&spawn_log_path).unwrap_or_default();
    assert!(
        !git_spawns.lines().any(|command| command == "var"),
        "idle metrics flush unexpectedly resolved Git identity:\n{git_spawns}"
    );
}

#[test]
fn daemon_remains_responsive_after_exhausted_metrics_migration() {
    let metrics_dir = tempfile::tempdir().unwrap();
    let metrics_path = metrics_dir.path().join("metrics.db");
    seed_version_4_metrics_db(&metrics_path);
    let metrics_path_str = metrics_path.to_string_lossy().to_string();

    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_METRICS_DB_PATH",
        metrics_path_str.as_str(),
    )]);
    wait_for_schema_migration(&metrics_path);

    let file_path = repo.path().join("example.md");
    fs::write(&file_path, "Untracked line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("example.md");
    file.assert_committed_lines(lines!["Untracked line".unattributed_human()]);

    std::thread::sleep(Duration::from_secs(7));
    repo.sync_daemon();

    fs::write(&file_path, "Untracked line\nHuman line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "example.md"])
        .unwrap();
    repo.stage_all_and_commit("Human edit").unwrap();
    file.assert_committed_lines(lines![
        "Untracked line".unattributed_human(),
        "Human line".human(),
    ]);

    let conn = open_with_memory_limits(&metrics_path).unwrap();
    let terminal_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM metrics WHERE delivered_ts IS NULL AND attempts >= 6",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let retryable_index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'metrics_retryable'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(terminal_rows, 20000);
    assert_eq!(retryable_index, 1);
}
