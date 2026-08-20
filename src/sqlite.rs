use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::time::Duration;

/// Negative SQLite cache_size values are kibibytes. Keep each connection's
/// page cache capped at 2 MiB unless a caller deliberately changes it later.
pub const MEMORY_LIMIT_CACHE_SIZE_KIB: i32 = -2000;

/// How long a connection retries on SQLITE_BUSY before surfacing the error.
/// Without this, cross-process contention on a shared database file fails
/// immediately instead of riding out a short writer.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub fn apply_memory_limits(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "cache_size", MEMORY_LIMIT_CACHE_SIZE_KIB)
}

pub fn open_with_memory_limits(path: impl AsRef<Path>) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    apply_memory_limits(&conn)?;
    Ok(conn)
}

pub fn open_with_flags_and_memory_limits(
    path: impl AsRef<Path>,
    flags: OpenFlags,
) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    apply_memory_limits(&conn)?;
    Ok(conn)
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
    fn open_with_memory_limits_sets_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("busy.db");

        let conn = open_with_memory_limits(&db_path).unwrap();

        let busy_timeout_ms: i64 = conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(busy_timeout_ms, BUSY_TIMEOUT.as_millis() as i64);
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
}
