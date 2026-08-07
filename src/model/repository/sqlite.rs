use crate::error::GitAiError;
use crate::model::repository::error::PersistenceError;
use rusqlite::{Connection, OpenFlags, params};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Recover a poisoned mutex by taking its inner guard anyway.
///
/// A panic while holding the lock cannot corrupt an in-memory connection or
/// counter the way it could corrupt on-disk state, so callers that just need
/// continued access (as opposed to `PersistenceError::LockPoisoned`'s
/// fail-closed sites) recover rather than propagate.
pub fn poisoned_lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Negative SQLite cache_size values are kibibytes. Keep each connection's
/// page cache capped at 2 MiB unless a caller deliberately changes it later.
pub const MEMORY_LIMIT_CACHE_SIZE_KIB: i32 = -2000;

pub fn apply_memory_limits(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "cache_size", MEMORY_LIMIT_CACHE_SIZE_KIB)
}

pub fn open_with_memory_limits(path: impl AsRef<Path>) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    apply_memory_limits(&conn)?;
    Ok(conn)
}

pub fn open_writable_with_memory_limits(path: impl AsRef<Path>) -> rusqlite::Result<Connection> {
    let conn = open_with_memory_limits(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;",
    )?;
    Ok(conn)
}

pub fn open_with_flags_and_memory_limits(
    path: impl AsRef<Path>,
    flags: OpenFlags,
) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(path, flags)?;
    apply_memory_limits(&conn)?;
    Ok(conn)
}

// ----- Singleton store scaffolding -----
//
// `notes_db`, `internal_db`, `bash_history_db`, and `metrics_db` are four
// independent SQLite-backed singleton stores (see the module-level doc on
// `super` for why their migrations stay unconsolidated). The helpers below
// factor out only the mechanical parts that are code-identical across them (modulo comments/whitespace):
// opening a connection at a path, reading the current schema version, and
// running the compare-then-apply migration loop. Each store keeps its own
// `SCHEMA_VERSION`, `MIGRATIONS`, and `apply_migration` body concrete.
//
// `internal_db` does not call `migration_runner`: its fast path treats a
// newer on-disk schema as forward-compatible (returns `Ok(())`) instead of
// failing closed with `PersistenceError::schema_version`, and it runs an
// extra post-loop verification step. It still uses
// `ensure_schema_metadata_table`/`read_schema_version` for the mechanical
// reads that don't differ.

/// Create parent directories, open a writable connection with the standard
/// memory limits, and hand it to `init` to build and schema-initialize the
/// caller's database wrapper.
///
/// Shared by the singleton stores' `open_at_path`/`new` constructors — each
/// store's own shape (extra fields, `enabled` flags, singleton caching, …)
/// stays in `init`, which is why this takes a closure rather than returning
/// a bare `Connection`.
pub fn open_at_path<T>(
    path: &Path,
    init: impl FnOnce(Connection) -> Result<T, GitAiError>,
) -> Result<T, GitAiError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = open_writable_with_memory_limits(path)?;
    init(conn)
}

/// Idempotently create the `schema_metadata` table each store's migration
/// scaffold relies on.
pub fn ensure_schema_metadata_table(conn: &Connection) -> Result<(), GitAiError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS schema_metadata (
            key   TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

/// Read the current schema version from `schema_metadata`, or `None` if the
/// table doesn't exist yet or holds no `version` row (a brand-new database,
/// or a database opened before the table was created).
pub fn read_schema_version(conn: &Connection) -> Option<usize> {
    conn.query_row(
        "SELECT value FROM schema_metadata WHERE key = 'version'",
        [],
        |row| {
            let version_str: String = row.get(0)?;
            version_str
                .parse::<usize>()
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        },
    )
    .ok()
}

#[cfg(test)]
#[track_caller]
pub(crate) fn assert_persisted_schema_version(conn: &Connection, expected: &str) {
    let actual: String = conn
        .query_row(
            "SELECT value FROM schema_metadata WHERE key = 'version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(actual, expected);
}

/// Runs the version-guard-and-loop scaffold shared by the singleton stores'
/// `initialize_schema`: succeeds immediately when `current` already matches
/// `target`, fails closed with the standard schema-version error when the
/// on-disk schema is newer than this binary supports (an older binary
/// opening a database written by a newer one), and otherwise applies each
/// missing migration in order, recording the new version after each
/// successful step.
///
/// `db_label` is the store's own error-text label (e.g. `"notes"`,
/// `"metrics"`), preserved verbatim in the `PersistenceError::Migration`
/// text. `apply` applies a single migration `from_version -> from_version +
/// 1` against `conn`; any per-step side effects a store needs (e.g.
/// metrics' column backfills) run inside `apply`, before its own migration
/// transaction.
pub fn migration_runner(
    conn: &mut Connection,
    db_label: &'static str,
    current: usize,
    target: usize,
    mut apply: impl FnMut(&mut Connection, usize) -> Result<(), GitAiError>,
) -> Result<(), GitAiError> {
    if current == target {
        return Ok(());
    }
    if current > target {
        return Err(PersistenceError::schema_version(db_label, current, target));
    }
    for from_version in current..target {
        apply(conn, from_version)?;
        // Upsert so concurrent initializers do not race on the version row.
        conn.execute(
            r#"
            INSERT INTO schema_metadata (key, value)
            VALUES ('version', ?1)
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value
            WHERE CAST(schema_metadata.value AS INTEGER) < CAST(excluded.value AS INTEGER)
            "#,
            params![(from_version + 1).to_string()],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_with_memory_limits_sets_cache_size() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("limited.db");

        let conn = open_with_memory_limits(&db_path).unwrap();

        let cache_size: i32 = conn
            .pragma_query_value(None, "cache_size", |row| row.get(0))
            .unwrap();
        assert_eq!(cache_size, MEMORY_LIMIT_CACHE_SIZE_KIB);
    }

    #[test]
    fn open_writable_with_memory_limits_sets_write_policy() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("writable.db");

        let conn = open_writable_with_memory_limits(&db_path).unwrap();

        let cache_size: i32 = conn
            .pragma_query_value(None, "cache_size", |row| row.get(0))
            .unwrap();
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        let synchronous: i32 = conn
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        let temp_store: i32 = conn
            .pragma_query_value(None, "temp_store", |row| row.get(0))
            .unwrap();

        assert_eq!(cache_size, MEMORY_LIMIT_CACHE_SIZE_KIB);
        assert_eq!(journal_mode, "wal");
        assert_eq!(synchronous, 1);
        assert_eq!(temp_store, 2);
    }

    #[test]
    fn open_with_flags_and_memory_limits_sets_cache_size() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("limited-readonly.db");
        drop(open_with_memory_limits(&db_path).unwrap());

        let conn =
            open_with_flags_and_memory_limits(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();

        let cache_size: i32 = conn
            .pragma_query_value(None, "cache_size", |row| row.get(0))
            .unwrap();
        assert_eq!(cache_size, MEMORY_LIMIT_CACHE_SIZE_KIB);
    }

    // ----- open_at_path -----

    #[test]
    fn open_at_path_creates_parent_dirs_and_runs_init() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nested").join("deep").join("store.db");

        let marker = open_at_path(&db_path, |conn| {
            conn.execute_batch("CREATE TABLE t (id INTEGER);")?;
            Ok(42)
        })
        .unwrap();

        assert_eq!(marker, 42);
        assert!(db_path.exists());
    }

    #[test]
    fn open_at_path_propagates_init_error() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("store.db");

        let result: Result<(), GitAiError> = open_at_path(&db_path, |_conn| {
            Err(GitAiError::Generic("boom".to_string()))
        });

        assert!(result.is_err());
    }

    // ----- ensure_schema_metadata_table / read_schema_version -----

    #[test]
    fn read_schema_version_none_before_table_exists() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_with_memory_limits(dir.path().join("fresh.db")).unwrap();
        assert_eq!(read_schema_version(&conn), None);
    }

    #[test]
    fn ensure_schema_metadata_table_is_idempotent_and_read_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_with_memory_limits(dir.path().join("meta.db")).unwrap();

        ensure_schema_metadata_table(&conn).unwrap();
        ensure_schema_metadata_table(&conn).unwrap(); // second call must not error
        assert_eq!(read_schema_version(&conn), None, "no version row yet");

        conn.execute(
            "INSERT INTO schema_metadata (key, value) VALUES ('version', '3')",
            [],
        )
        .unwrap();
        assert_eq!(read_schema_version(&conn), Some(3));
    }

    fn persisted_schema_version_assertion_panics(conn: &Connection) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_persisted_schema_version(conn, "3");
        }))
        .is_err()
    }

    #[test]
    fn persisted_schema_version_assertion_rejects_missing_table_and_row() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(persisted_schema_version_assertion_panics(&conn));

        ensure_schema_metadata_table(&conn).unwrap();
        assert!(persisted_schema_version_assertion_panics(&conn));
    }

    #[test]
    fn persisted_schema_version_assertion_compares_exact_text() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema_metadata_table(&conn).unwrap();

        for (value, should_panic) in [("3", false), ("03", true), ("malformed", true)] {
            conn.execute(
                "INSERT OR REPLACE INTO schema_metadata (key, value) VALUES ('version', ?1)",
                [value],
            )
            .unwrap();
            assert_eq!(
                persisted_schema_version_assertion_panics(&conn),
                should_panic,
                "stored value {value:?}"
            );
        }
    }

    // ----- migration_runner -----

    #[test]
    fn migration_runner_fast_path_skips_apply_when_current() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = open_with_memory_limits(dir.path().join("current.db")).unwrap();
        ensure_schema_metadata_table(&conn).unwrap();

        let mut applied = Vec::new();
        migration_runner(&mut conn, "test", 3, 3, |_conn, v| {
            applied.push(v);
            Ok(())
        })
        .unwrap();

        assert!(applied.is_empty(), "already-current schema must not apply");
    }

    #[test]
    fn migration_runner_guard_matches_persistence_error_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = open_with_memory_limits(dir.path().join("ahead.db")).unwrap();
        ensure_schema_metadata_table(&conn).unwrap();

        let err = migration_runner(&mut conn, "widgets", 9, 5, |_conn, _v| Ok(())).unwrap_err();

        assert_eq!(
            err.to_string(),
            PersistenceError::schema_version("widgets", 9, 5).to_string()
        );
    }

    #[test]
    fn migration_runner_applies_each_missing_version_in_order_and_records_final_version() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = open_with_memory_limits(dir.path().join("upgrade.db")).unwrap();
        ensure_schema_metadata_table(&conn).unwrap();

        let mut applied = Vec::new();
        migration_runner(&mut conn, "test", 0, 3, |_conn, v| {
            applied.push(v);
            Ok(())
        })
        .unwrap();

        assert_eq!(applied, vec![0, 1, 2]);
        assert_eq!(read_schema_version(&conn), Some(3));
    }

    #[test]
    fn migration_runner_stops_at_first_apply_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = open_with_memory_limits(dir.path().join("partial.db")).unwrap();
        ensure_schema_metadata_table(&conn).unwrap();

        let mut applied = Vec::new();
        let result = migration_runner(&mut conn, "test", 0, 3, |_conn, v| {
            applied.push(v);
            if v == 1 {
                return Err(GitAiError::Generic("migration 1 failed".to_string()));
            }
            Ok(())
        });

        assert!(result.is_err());
        assert_eq!(applied, vec![0, 1]);
        assert_eq!(
            read_schema_version(&conn),
            Some(1),
            "version row should reflect the last successfully applied migration"
        );
    }
}
