//! Persistent registry of the repository families this daemon has seen, with
//! each worktree's untraced-commit fixup cursor. In memory the daemon only
//! knows families it has heard from since it started; this store is what lets
//! it find and attribute commits made while it was off.
//!
//! Written only by the fixup pass (`fixup.scan` and the periodic worker) —
//! never on the trace ingestion or checkpoint paths.

use crate::daemon::ref_cursor::{ReflogAnchor, UntracedReflogCursor};
use crate::error::GitAiError;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

/// A family whose common dir has been missing this long is forgotten.
pub const MISSING_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;
/// A family not seen for this long is forgotten (it may live on a detached
/// volume; if it comes back it is a first sighting again).
pub const UNSEEN_RETENTION_SECS: u64 = 90 * 24 * 60 * 60;
/// Hard cap on remembered families; the least recently seen go first.
pub const MAX_FAMILIES: usize = 1000;

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
CREATE TABLE repo_families (
    common_dir TEXT PRIMARY KEY,
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    missing_since INTEGER
);
CREATE TABLE worktree_fixup_cursors (
    git_dir TEXT PRIMARY KEY,
    common_dir TEXT NOT NULL,
    worktree TEXT NOT NULL,
    reflog_offset INTEGER NOT NULL,
    anchor_old TEXT,
    anchor_new TEXT,
    anchor_message TEXT,
    updated_at INTEGER NOT NULL
);
CREATE INDEX idx_worktree_fixup_cursors_common_dir ON worktree_fixup_cursors(common_dir);
INSERT INTO schema_version (version) VALUES (1);
"#];

pub struct RepoFamilyStore {
    conn: Arc<Mutex<Connection>>,
}

impl RepoFamilyStore {
    pub fn open(path: &Path) -> Result<Self, GitAiError> {
        let conn = crate::sqlite::open_with_memory_limits(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
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
        if current_version > MIGRATIONS.len() as u32 {
            return Err(GitAiError::Generic(format!(
                "repo-families database schema v{current_version} is newer than this binary \
                 (supports up to v{}); upgrade git-ai or delete the database",
                MIGRATIONS.len()
            )));
        }
        for (version, migration_sql) in MIGRATIONS.iter().enumerate() {
            if current_version < (version + 1) as u32 {
                let tx = conn.unchecked_transaction()?;
                tx.execute_batch(migration_sql)?;
                tx.commit()?;
            }
        }
        Ok(())
    }

    /// Remembers that `common_dir` exists on this machine right now.
    pub fn record_family_seen(&self, common_dir: &str, now_secs: u64) -> Result<(), GitAiError> {
        Self::record_family_seen_in(&self.lock(), common_dir, now_secs)
    }

    fn record_family_seen_in(
        conn: &Connection,
        common_dir: &str,
        now_secs: u64,
    ) -> Result<(), GitAiError> {
        conn.execute(
            "INSERT INTO repo_families (common_dir, first_seen_at, last_seen_at, missing_since)
             VALUES (?1, ?2, ?2, NULL)
             ON CONFLICT(common_dir) DO UPDATE SET
                 last_seen_at = excluded.last_seen_at,
                 missing_since = NULL",
            params![common_dir, now_secs as i64],
        )?;
        Ok(())
    }

    /// Records that `common_dir` is gone (kept for `MISSING_RETENTION_SECS`
    /// in case a volume comes back); a family already marked keeps its
    /// original `missing_since`.
    pub fn record_family_missing(&self, common_dir: &str, now_secs: u64) -> Result<(), GitAiError> {
        self.lock().execute(
            "UPDATE repo_families SET missing_since = COALESCE(missing_since, ?2)
             WHERE common_dir = ?1",
            params![common_dir, now_secs as i64],
        )?;
        Ok(())
    }

    /// Every remembered family, most recently seen first.
    pub fn known_families(&self) -> Result<Vec<String>, GitAiError> {
        let conn = self.lock();
        let mut statement =
            conn.prepare("SELECT common_dir FROM repo_families ORDER BY last_seen_at DESC")?;
        let families = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(families)
    }

    pub fn family_count(&self) -> Result<usize, GitAiError> {
        let count: i64 =
            self.lock()
                .query_row("SELECT COUNT(*) FROM repo_families", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn cursor(&self, git_dir: &str) -> Result<Option<UntracedReflogCursor>, GitAiError> {
        let cursor = self
            .lock()
            .query_row(
                "SELECT reflog_offset, anchor_old, anchor_new, anchor_message
                 FROM worktree_fixup_cursors WHERE git_dir = ?1",
                params![git_dir],
                |row| {
                    let offset: i64 = row.get(0)?;
                    let anchor = match (
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ) {
                        (Some(old), Some(new), Some(message)) => Some(ReflogAnchor {
                            old,
                            new,
                            message,
                            end_offset: offset as u64,
                        }),
                        _ => None,
                    };
                    Ok(UntracedReflogCursor {
                        offset: offset as u64,
                        anchor,
                    })
                },
            )
            .optional()?;
        Ok(cursor)
    }

    pub fn save_cursor(
        &self,
        common_dir: &str,
        git_dir: &str,
        worktree: &str,
        cursor: &UntracedReflogCursor,
        now_secs: u64,
    ) -> Result<(), GitAiError> {
        Self::save_cursor_in(
            &self.lock(),
            common_dir,
            git_dir,
            worktree,
            cursor,
            now_secs,
        )
    }

    fn save_cursor_in(
        conn: &Connection,
        common_dir: &str,
        git_dir: &str,
        worktree: &str,
        cursor: &UntracedReflogCursor,
        now_secs: u64,
    ) -> Result<(), GitAiError> {
        let anchor = cursor.anchor.as_ref();
        conn.execute(
            "INSERT INTO worktree_fixup_cursors
                 (git_dir, common_dir, worktree, reflog_offset, anchor_old, anchor_new, anchor_message, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(git_dir) DO UPDATE SET
                 common_dir = excluded.common_dir,
                 worktree = excluded.worktree,
                 reflog_offset = excluded.reflog_offset,
                 anchor_old = excluded.anchor_old,
                 anchor_new = excluded.anchor_new,
                 anchor_message = excluded.anchor_message,
                 updated_at = excluded.updated_at",
            params![
                git_dir,
                common_dir,
                worktree,
                cursor.offset as i64,
                anchor.map(|anchor| anchor.old.as_str()),
                anchor.map(|anchor| anchor.new.as_str()),
                anchor.map(|anchor| anchor.message.as_str()),
                now_secs as i64,
            ],
        )?;
        Ok(())
    }

    /// What a completed fixup pass leaves behind, in one transaction: the
    /// family is seen now and this worktree's cursor is settled here. A crash
    /// can never leave a remembered family whose cursor is missing (which a
    /// cold actor would read as "first sighting" and skip its history).
    pub fn record_pass_completed(
        &self,
        common_dir: &str,
        git_dir: &str,
        worktree: &str,
        cursor: &UntracedReflogCursor,
        now_secs: u64,
    ) -> Result<(), GitAiError> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        Self::record_family_seen_in(&tx, common_dir, now_secs)?;
        Self::save_cursor_in(&tx, common_dir, git_dir, worktree, cursor, now_secs)?;
        tx.commit()?;
        Ok(())
    }

    /// The worktrees a family has been scanned in, as `(git_dir, worktree)`
    /// pairs. This is how a main worktree that the filesystem cannot lead to
    /// (`git init --separate-git-dir`: the git dir holds no pointer back) is
    /// still found after a restart; callers drop pairs whose paths are gone.
    pub fn known_worktrees(&self, common_dir: &str) -> Result<Vec<(String, String)>, GitAiError> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT git_dir, worktree FROM worktree_fixup_cursors
             WHERE common_dir = ?1 ORDER BY git_dir",
        )?;
        let worktrees = statement
            .query_map(params![common_dir], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(worktrees)
    }

    /// Forgets families missing or unseen for too long, then the least
    /// recently seen beyond `MAX_FAMILIES`, with their cursors. Returns how
    /// many families were removed.
    pub fn prune(&self, now_secs: u64) -> Result<usize, GitAiError> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let mut removed = tx.execute(
            "DELETE FROM repo_families
             WHERE (missing_since IS NOT NULL AND missing_since <= ?1)
                OR last_seen_at <= ?2",
            params![
                now_secs.saturating_sub(MISSING_RETENTION_SECS) as i64,
                now_secs.saturating_sub(UNSEEN_RETENTION_SECS) as i64,
            ],
        )?;
        removed += tx.execute(
            "DELETE FROM repo_families WHERE common_dir IN (
                 SELECT common_dir FROM repo_families
                 ORDER BY last_seen_at DESC, common_dir
                 LIMIT -1 OFFSET ?1
             )",
            params![MAX_FAMILIES as i64],
        )?;
        tx.execute(
            "DELETE FROM worktree_fixup_cursors
             WHERE common_dir NOT IN (SELECT common_dir FROM repo_families)",
            [],
        )?;
        tx.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";
    const NOW: u64 = 1_800_000_000;

    fn store() -> (tempfile::TempDir, RepoFamilyStore) {
        let temp = tempfile::tempdir().unwrap();
        let store = RepoFamilyStore::open(&temp.path().join("repo-families-db")).unwrap();
        (temp, store)
    }

    fn cursor_with_anchor(offset: u64) -> UntracedReflogCursor {
        UntracedReflogCursor {
            offset,
            anchor: Some(ReflogAnchor {
                old: A.to_string(),
                new: B.to_string(),
                message: "commit: anchored".to_string(),
                end_offset: offset,
            }),
        }
    }

    #[test]
    fn open_migrates_once_and_reopens_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("repo-families-db");
        RepoFamilyStore::open(&path).unwrap();
        let reopened = RepoFamilyStore::open(&path).unwrap();
        let version: u32 = reopened
            .lock()
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as u32);
        assert_eq!(reopened.family_count().unwrap(), 0);
    }

    #[test]
    fn newer_schema_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("repo-families-db");
        {
            let store = RepoFamilyStore::open(&path).unwrap();
            store
                .lock()
                .execute("INSERT INTO schema_version (version) VALUES (99)", [])
                .unwrap();
        }
        let error = RepoFamilyStore::open(&path)
            .err()
            .expect("newer schema must fail");
        assert!(
            error.to_string().contains("newer than this binary"),
            "{error}"
        );
    }

    #[test]
    fn record_family_seen_keeps_first_seen_and_clears_missing() {
        let (_temp, store) = store();
        store.record_family_seen("/repos/a/.git", NOW).unwrap();
        store
            .record_family_missing("/repos/a/.git", NOW + 10)
            .unwrap();
        store.record_family_seen("/repos/a/.git", NOW + 20).unwrap();
        store.record_family_seen("/repos/b/.git", NOW + 5).unwrap();

        let (first, last, missing): (i64, i64, Option<i64>) = store
            .lock()
            .query_row(
                "SELECT first_seen_at, last_seen_at, missing_since FROM repo_families
                 WHERE common_dir = '/repos/a/.git'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (first, last, missing),
            (NOW as i64, (NOW + 20) as i64, None)
        );
        assert_eq!(
            store.known_families().unwrap(),
            vec!["/repos/a/.git".to_string(), "/repos/b/.git".to_string()],
            "most recently seen first"
        );
        assert_eq!(store.family_count().unwrap(), 2);
    }

    #[test]
    fn record_family_missing_keeps_the_first_missing_timestamp() {
        let (_temp, store) = store();
        store.record_family_seen("/repos/a/.git", NOW).unwrap();
        store
            .record_family_missing("/repos/a/.git", NOW + 10)
            .unwrap();
        store
            .record_family_missing("/repos/a/.git", NOW + 20)
            .unwrap();
        let missing: i64 = store
            .lock()
            .query_row(
                "SELECT missing_since FROM repo_families WHERE common_dir = '/repos/a/.git'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(missing, (NOW + 10) as i64);
    }

    #[test]
    fn cursor_roundtrips_with_and_without_anchor() {
        let (_temp, store) = store();
        store.record_family_seen("/repos/a/.git", NOW).unwrap();
        assert_eq!(store.cursor("/repos/a/.git").unwrap(), None);

        let anchored = cursor_with_anchor(4_096);
        store
            .save_cursor("/repos/a/.git", "/repos/a/.git", "/repos/a", &anchored, NOW)
            .unwrap();
        assert_eq!(store.cursor("/repos/a/.git").unwrap(), Some(anchored));

        let bare = UntracedReflogCursor {
            offset: 0,
            anchor: None,
        };
        store
            .save_cursor("/repos/a/.git", "/repos/a/.git", "/repos/a", &bare, NOW + 1)
            .unwrap();
        assert_eq!(store.cursor("/repos/a/.git").unwrap(), Some(bare));
    }

    #[test]
    fn record_pass_completed_writes_family_and_cursor_together() {
        let (_temp, store) = store();
        let cursor = cursor_with_anchor(2_048);
        store
            .record_pass_completed("/repos/a/.git", "/repos/a/.git", "/repos/a", &cursor, NOW)
            .unwrap();
        store
            .record_pass_completed(
                "/repos/a/.git",
                "/repos/a/.git/worktrees/wt",
                "/repos/a-wt",
                &cursor,
                NOW,
            )
            .unwrap();

        assert_eq!(
            store.known_families().unwrap(),
            vec!["/repos/a/.git".to_string()]
        );
        assert_eq!(store.cursor("/repos/a/.git").unwrap(), Some(cursor));
        assert_eq!(
            store.known_worktrees("/repos/a/.git").unwrap(),
            vec![
                ("/repos/a/.git".to_string(), "/repos/a".to_string()),
                (
                    "/repos/a/.git/worktrees/wt".to_string(),
                    "/repos/a-wt".to_string()
                ),
            ]
        );
    }

    #[test]
    fn prune_forgets_missing_and_unseen_families_with_their_cursors() {
        let (_temp, store) = store();
        store.record_family_seen("/repos/gone/.git", NOW).unwrap();
        store
            .save_cursor(
                "/repos/gone/.git",
                "/repos/gone/.git",
                "/repos/gone",
                &cursor_with_anchor(1),
                NOW,
            )
            .unwrap();
        store
            .record_family_missing("/repos/gone/.git", NOW)
            .unwrap();
        store
            .record_family_seen("/repos/stale/.git", NOW - UNSEEN_RETENTION_SECS - 1)
            .unwrap();
        store.record_family_seen("/repos/fresh/.git", NOW).unwrap();
        store
            .record_family_seen("/repos/recently-missing/.git", NOW)
            .unwrap();
        store
            .record_family_missing("/repos/recently-missing/.git", NOW + 3600)
            .unwrap();

        let removed = store.prune(NOW + MISSING_RETENTION_SECS + 1).unwrap();

        assert_eq!(removed, 2);
        assert_eq!(
            store.known_families().unwrap(),
            vec![
                "/repos/fresh/.git".to_string(),
                "/repos/recently-missing/.git".to_string()
            ]
        );
        assert_eq!(store.cursor("/repos/gone/.git").unwrap(), None);
    }

    #[test]
    fn prune_caps_the_number_of_families_by_recency() {
        let (_temp, store) = store();
        for index in 0..(MAX_FAMILIES + 5) {
            store
                .record_family_seen(&format!("/repos/{index}/.git"), NOW + index as u64)
                .unwrap();
        }

        let removed = store.prune(NOW + MAX_FAMILIES as u64 + 10).unwrap();

        assert_eq!(removed, 5);
        assert_eq!(store.family_count().unwrap(), MAX_FAMILIES);
        let families = store.known_families().unwrap();
        assert!(!families.contains(&"/repos/0/.git".to_string()));
        assert!(!families.contains(&"/repos/4/.git".to_string()));
        assert!(families.contains(&"/repos/5/.git".to_string()));
    }
}
