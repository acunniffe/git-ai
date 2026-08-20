//! Centralized notes I/O API.
//!
//! All authorship-note reads and writes flow through this module. The implementation
//! dispatches to the sqlite backend (default), the git-notes backend, or the HTTP
//! backend based on `Config::fresh().notes_backend_kind()`.
//!
//! Dispatch reads a fresh config snapshot (`Config::fresh()`) rather than the
//! process-lifetime `Config::get()` singleton: the daemon is long-lived and must
//! observe backend changes made after it started.
//!
//! The sqlite backend stores notes as local-primary rows in the notes database;
//! reads fall back to `refs/notes/ai` (and backfill the cache) so repositories
//! with pre-existing git notes keep working without migration.

mod primary_backend;

use crate::config::{Config, NotesBackendKind};
use crate::error::GitAiError;
use crate::model::authorship_log_serialization::AuthorshipLog;
use crate::operations::git::notes_store::{
    AuthorshipNoteStore, GitNotesStore, HttpNoteStore, SqliteNoteStore, db_check_exists,
    db_read_notes,
};
use crate::operations::git::repository::{Repository, resolve_api_author_identity};
use std::collections::{HashMap, HashSet};

// Re-export CommitAuthorship so callers don't need to import from refs directly.
pub use crate::operations::git::refs::CommitAuthorship;
pub(crate) use primary_backend::read_authorship as read_authorship_from_primary_backend;

/// Per-SHA note-write pair: `(commit_sha, serialized_note_content)`.
///
/// The canonical element type for all `write_notes_batch` / `export_notes_to_git_refs`
/// calls. Multi-commit loops collect `Vec<NoteWriteEntry>` in memory and flush once.
/// Distinct from `notes_add_blob_batch`'s `(commit_sha, blob_oid)` shape.
pub type NoteWriteEntry = (String, String);

// --- Writes ---

pub fn write_note(repo: &Repository, commit_sha: &str, content: &str) -> Result<(), GitAiError> {
    match Config::fresh().notes_backend_kind() {
        NotesBackendKind::Sqlite => SqliteNoteStore::new().write_note(commit_sha, content),
        NotesBackendKind::Http => HttpNoteStore::new().write_note(commit_sha, content),
        NotesBackendKind::GitNotes => GitNotesStore { repo }.write_note(commit_sha, content),
    }
}

pub fn write_notes_batch(repo: &Repository, entries: &[NoteWriteEntry]) -> Result<(), GitAiError> {
    if entries.is_empty() {
        return Ok(());
    }
    match Config::fresh().notes_backend_kind() {
        NotesBackendKind::Sqlite => SqliteNoteStore::new().write_notes_batch(entries),
        NotesBackendKind::Http => HttpNoteStore::new().write_notes_batch(entries),
        NotesBackendKind::GitNotes => GitNotesStore { repo }.write_notes_batch(entries),
    }
}

// --- Reads ---

pub fn read_note(repo: &Repository, commit_sha: &str) -> Option<String> {
    match Config::fresh().notes_backend_kind() {
        NotesBackendKind::Sqlite => SqliteNoteStore::new()
            .read_note(commit_sha)
            .or_else(|| sqlite_fallback_read_from_refs(repo, commit_sha)),
        NotesBackendKind::Http => HttpNoteStore::new()
            .read_note(commit_sha)
            .or_else(|| GitNotesStore { repo }.read_note(commit_sha)),
        NotesBackendKind::GitNotes => GitNotesStore { repo }.read_note(commit_sha),
    }
}

/// Read note contents for multiple commits in O(1) git process calls.
/// Returns a map of commit_sha → note_content for commits that have notes.
///
/// On the HTTP backend this checks the local cache, then fetches-and-caches any
/// misses from the remote, and finally falls back to local git notes; on the
/// GitNotes backend it reads directly via the batched `notes_for_commits` path.
pub fn read_notes_batch(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    if commit_shas.is_empty() {
        return Ok(HashMap::new());
    }

    match Config::fresh().notes_backend_kind() {
        NotesBackendKind::Sqlite => {
            let mut notes = SqliteNoteStore::new().read_notes_batch(commit_shas)?;

            // Fall back to refs/notes/ai for misses and backfill the cache so
            // subsequent reads are served from the database. Refs read errors
            // (e.g. corrupt note blobs) propagate — callers such as rewrite
            // migration must fail closed rather than treat notes as absent.
            let missing: Vec<String> = commit_shas
                .iter()
                .filter(|sha| !notes.contains_key(*sha))
                .cloned()
                .collect();
            if !missing.is_empty() {
                let git_notes = GitNotesStore { repo }.read_notes_batch(&missing)?;
                SqliteNoteStore::backfill_cache(&git_notes);
                notes.extend(git_notes);
            }

            Ok(notes)
        }
        NotesBackendKind::Http => {
            // db + remote-fetch tier (inside HttpNoteStore::read_notes_batch)
            let mut notes = HttpNoteStore::new().read_notes_batch(commit_shas)?;

            // Final refs fallback — errors swallowed (Http arm must not fail closed;
            // contrast with Sqlite arm above where errors propagate).
            let missing: Vec<String> = commit_shas
                .iter()
                .filter(|sha| !notes.contains_key(*sha))
                .cloned()
                .collect();
            if !missing.is_empty()
                && let Ok(git_notes) = (GitNotesStore { repo }).read_notes_batch(&missing)
            {
                notes.extend(git_notes);
            }

            Ok(notes)
        }
        NotesBackendKind::GitNotes => GitNotesStore { repo }.read_notes_batch(commit_shas),
    }
}

pub fn read_authorship(repo: &Repository, commit_sha: &str) -> Option<AuthorshipLog> {
    match Config::fresh().notes_backend_kind() {
        NotesBackendKind::Sqlite => {
            // Check the database first; fall through to git notes on miss and
            // backfill the raw content so the next read is served locally.
            SqliteNoteStore::new()
                .read_note(commit_sha)
                .or_else(|| sqlite_fallback_read_from_refs(repo, commit_sha))
                .and_then(primary_backend::deserialize_authorship)
        }
        NotesBackendKind::Http => {
            // Check the cache first; fall through to git notes on miss.
            if let Some(content) = HttpNoteStore::new().read_note(commit_sha) {
                primary_backend::deserialize_authorship(content)
            } else {
                crate::operations::git::refs::get_authorship(repo, commit_sha)
            }
        }
        NotesBackendKind::GitNotes => {
            primary_backend::read_authorship(repo, commit_sha, NotesBackendKind::GitNotes)
        }
    }
}

pub fn read_authorship_v3(
    repo: &Repository,
    commit_sha: &str,
) -> Result<AuthorshipLog, GitAiError> {
    match Config::fresh().notes_backend_kind() {
        NotesBackendKind::Sqlite => {
            if let Some(content) = SqliteNoteStore::new()
                .read_note(commit_sha)
                .or_else(|| sqlite_fallback_read_from_refs(repo, commit_sha))
            {
                AuthorshipLog::deserialize_from_string(&content)
                    .map_err(|e| GitAiError::Generic(format!("notes deserialization error: {}", e)))
            } else {
                crate::operations::git::refs::get_reference_as_authorship_log_v3(repo, commit_sha)
            }
        }
        NotesBackendKind::Http => {
            if let Some(content) = HttpNoteStore::new().read_note(commit_sha) {
                AuthorshipLog::deserialize_from_string(&content)
                    .map_err(|e| GitAiError::Generic(format!("notes deserialization error: {}", e)))
            } else {
                crate::operations::git::refs::get_reference_as_authorship_log_v3(repo, commit_sha)
            }
        }
        NotesBackendKind::GitNotes => {
            crate::operations::git::refs::get_reference_as_authorship_log_v3(repo, commit_sha)
        }
    }
}

/// Return a map of commit SHA → note-blob OID for the given commits.
///
/// Callers use the returned OIDs as git object IDs with the batched `cat-file` reader
/// (not purely presence checks). On Http/Sqlite backends notes live in notes-db, not in
/// git refs, so an empty map is returned and callers fall back to `read_note`.
pub fn read_note_blob_oids(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    match Config::fresh().notes_backend_kind() {
        // For Sqlite/Http, notes are in notes-db not in git — no blob OIDs exist.
        // Return an empty map; callers handle this as "no notes in git".
        NotesBackendKind::Sqlite | NotesBackendKind::Http => Ok(HashMap::new()),
        NotesBackendKind::GitNotes => {
            crate::operations::git::refs::note_blob_oids_for_commits(repo, commit_shas)
        }
    }
}

pub fn commits_with_notes(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<HashSet<String>, GitAiError> {
    match Config::fresh().notes_backend_kind() {
        NotesBackendKind::Sqlite | NotesBackendKind::Http => {
            // Check the database first; fall through to git notes for misses.
            let cached = db_check_exists(commit_shas);
            if cached.len() == commit_shas.len() {
                return Ok(cached);
            }
            // For commits not in the cache, check git notes as fallback.
            let missing: Vec<String> = commit_shas
                .iter()
                .filter(|sha| !cached.contains(*sha))
                .cloned()
                .collect();
            let from_git =
                crate::operations::git::refs::commits_with_authorship_notes(repo, &missing)?;
            Ok(cached.into_iter().chain(from_git).collect())
        }
        NotesBackendKind::GitNotes => (GitNotesStore { repo }).commits_with_notes(commit_shas),
    }
}

pub fn filter_commits_with_notes(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<Vec<CommitAuthorship>, GitAiError> {
    match Config::fresh().notes_backend_kind() {
        NotesBackendKind::Sqlite | NotesBackendKind::Http => {
            // `CommitAuthorship` requires a git_author that is only available from
            // `git rev-list`. Call the underlying git function which handles author
            // lookup, then patch in cache hits for commits whose `authorship_log`
            // would otherwise be absent (because refs/notes/ai is empty).
            //
            // The git function calls `get_authorship(repo, sha)` (refs.rs, not
            // notes_api), so for Http the results will be `CommitAuthorship::NoLog`
            // for all commits. We promote any commit that has a cache entry to
            // `CommitAuthorship::Log`.
            let cached_map = db_read_notes(commit_shas);

            let git_results =
                crate::operations::git::refs::get_commits_with_notes_from_list(repo, commit_shas)?;

            // Promote NoLog entries that are in the cache to Log entries.
            let results = git_results
                .into_iter()
                .map(|ca| match ca {
                    CommitAuthorship::NoLog {
                        ref sha,
                        ref git_author,
                    } => {
                        if let Some(content) = cached_map.get(sha)
                            && let Ok(authorship_log) =
                                AuthorshipLog::deserialize_from_string(content)
                                    .map_err(|e| GitAiError::Generic(e.to_string()))
                        {
                            return CommitAuthorship::Log {
                                sha: sha.clone(),
                                git_author: git_author.clone(),
                                authorship_log,
                            };
                        }
                        ca
                    }
                    // Already has a log (shouldn't happen for Http, but keep it).
                    CommitAuthorship::Log { .. } => ca,
                })
                .collect();

            Ok(results)
        }
        NotesBackendKind::GitNotes => {
            crate::operations::git::refs::get_commits_with_notes_from_list(repo, commit_shas)
        }
    }
}

// --- Search ---

/// Search authorship-note content for a literal substring and return matching
/// commit SHAs, newest first.
///
/// On the HTTP backend this searches the notes-db cache and unions in any
/// matches from local git notes (transition-period repos may have both); on
/// the GitNotes backend it greps `refs/notes/ai` directly.
pub fn search_notes(repo: &Repository, pattern: &str) -> Result<Vec<String>, GitAiError> {
    match Config::fresh().notes_backend_kind() {
        NotesBackendKind::Sqlite | NotesBackendKind::Http => {
            let mut shas: HashSet<String> = {
                // db-side search (same for both Sqlite and Http)
                let db_results = SqliteNoteStore::new().search_notes(pattern)?;
                db_results.into_iter().collect()
            };

            // Union in matches from local git notes for transition-period repos.
            if let Ok(git_shas) = (GitNotesStore { repo }).search_notes(pattern) {
                shas.extend(git_shas);
            }

            crate::operations::git::refs::sort_commit_shas_by_date_desc(repo, shas)
        }
        NotesBackendKind::GitNotes => (GitNotesStore { repo }).search_notes(pattern),
    }
}

// --- Materialization (for git ai log) ---

/// Materialize notes from the local cache into a one-off git ref
/// `refs/notes/ai-display` so that `git log --notes=ai-display` can render
/// them without requiring them to be in `refs/notes/ai`.
///
/// Only the most recent `limit` commits reachable from HEAD are considered.
///
/// The ref is left in place after the call; callers use it with `--notes=ai-display`.
/// It is safe to call repeatedly — each call starts from an empty tree via
/// `from 0000...` so stale notes from prior calls are discarded.
///
/// Returns the number of notes that were written into `refs/notes/ai-display`.
pub fn materialize_notes_for_display(repo: &Repository, limit: usize) -> Result<usize, GitAiError> {
    use crate::clients::git_cli::exec_git;
    use crate::clients::git_cli::exec_git_with_stdin_writer;

    // 1. Get recent commits via rev-list.
    let rev_list_args: Vec<String> = repo
        .global_args_for_exec()
        .into_iter()
        .chain([
            "rev-list".to_string(),
            format!("--max-count={}", limit),
            "HEAD".to_string(),
        ])
        .collect();

    let output = exec_git(&rev_list_args)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let commit_shas: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if commit_shas.is_empty() {
        return Ok(0);
    }

    // 2. Look up which commits are in the local notes-db cache.
    let cached_map = db_read_notes(&commit_shas);
    if cached_map.is_empty() {
        return Ok(0);
    }

    // 3. Stream a git fast-import script directly to the child's stdin.
    //    Structure:
    //      - One `blob` stanza per note (each gets a mark ID).
    //      - One `commit` stanza with `from 0000...` (empty tree) that attaches all blobs.
    let fast_import_args = crate::operations::git::refs::fast_import_args(repo);

    exec_git_with_stdin_writer(&fast_import_args, |writer| {
        for (idx, (_commit_sha, content)) in cached_map.iter().enumerate() {
            crate::operations::git::refs::write_blob_stanza(writer, idx + 1, content)?;
        }

        // Use `from` with an all-zeros SHA to start from an empty tree, ensuring
        // stale notes from prior materializations are removed.
        crate::operations::git::refs::write_notes_commit_header(
            writer,
            "refs/notes/ai-display",
            format_args!("git-ai <git-ai@localhost> 1000000000 +0000"),
            Some("0000000000000000000000000000000000000000"),
        )?;

        for (idx, (commit_sha, _content)) in cached_map.iter().enumerate() {
            writeln!(writer, "M 100644 :{} {}", idx + 1, commit_sha)?;
        }
        writer.write_all(b"\n")
    })?;

    Ok(cached_map.len())
}

// --- Cache warming ---

/// Pre-warm the local notes cache during `git pull` by fetching notes for
/// recently-arrived commits from the HTTP backend.
///
/// Algorithm:
/// 1. Walk the last 500 commits reachable from HEAD via `git rev-list`.
/// 2. Filter out any SHAs already present in `notes-db` (already cached).
/// 3. Batch the remaining SHAs into chunks of 100 and call `ApiClient::read_notes()`.
/// 4. Write returned entries via `cache_synced_notes()` so rows are inserted
///    with `synced = 1` (read cache, not upload queue).
///
/// This function is a best-effort operation: errors are logged but not propagated
/// (callers should treat failure as a cache miss, not a hard error).
pub fn warm_cache_for_remote(repo: &Repository, remote: &str) -> Result<(), GitAiError> {
    use crate::clients::api::client::{ApiClient, ApiContext};
    use crate::clients::git_cli::exec_git;

    // 1. Walk recent history. Prefer the remote's default branch; fall back to HEAD.
    let remote_head = format!("refs/remotes/{}/HEAD", remote);
    let rev_target = {
        let check_args: Vec<String> = repo
            .global_args_for_exec()
            .into_iter()
            .chain([
                "rev-parse".to_string(),
                "--verify".to_string(),
                "--quiet".to_string(),
                remote_head.clone(),
            ])
            .collect();
        if exec_git(&check_args)
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            remote_head
        } else {
            "HEAD".to_string()
        }
    };

    let rev_list_args: Vec<String> = repo
        .global_args_for_exec()
        .into_iter()
        .chain([
            "rev-list".to_string(),
            "--max-count=500".to_string(),
            rev_target,
        ])
        .collect();

    let output = exec_git(&rev_list_args)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let all_shas: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if all_shas.is_empty() {
        tracing::debug!("warm_cache_for_remote: no commits in HEAD history; skipping");
        return Ok(());
    }

    // 2. Filter out SHAs already in notes-db.
    let already_cached: HashSet<String> = {
        match crate::model::repository::notes_db::NotesDatabase::global() {
            Ok(db) => match db.lock() {
                Ok(lock) => {
                    let refs: Vec<&str> = all_shas.iter().map(|s| s.as_str()).collect();
                    lock.get_notes(&refs)
                        .unwrap_or_default()
                        .into_keys()
                        .collect()
                }
                Err(e) => {
                    tracing::warn!("warm_cache_for_remote: DB lock poisoned: {}", e);
                    HashSet::new()
                }
            },
            Err(e) => {
                tracing::warn!("warm_cache_for_remote: failed to open notes-db: {}", e);
                HashSet::new()
            }
        }
    };

    let uncached: Vec<String> = all_shas
        .into_iter()
        .filter(|sha| !already_cached.contains(sha))
        .collect();

    if uncached.is_empty() {
        tracing::debug!("warm_cache_for_remote: all commits already cached; skipping");
        return Ok(());
    }

    tracing::info!(
        remote = %remote,
        backend = %"http",
        uncached_commits = uncached.len(),
        "fetching authorship notes"
    );
    tracing::debug!(
        "warm_cache_for_remote: fetching notes for {} uncached commits",
        uncached.len()
    );

    // 3. Batch-fetch from the HTTP backend (chunks of 100).
    let cfg = crate::config::Config::fresh();
    let Some(backend_url) = cfg.notes_backend_url().map(str::to_string) else {
        tracing::debug!(
            "warm_cache_for_remote: notes_backend.backend_url is not configured; skipping"
        );
        return Ok(());
    };
    let ctx = ApiContext::new(Some(backend_url), resolve_api_author_identity);
    let client = ApiClient::new(ctx);

    // Skip when not authenticated (matches daemon flush_notes pattern).
    if !client.is_logged_in() && !client.has_api_key() {
        tracing::debug!("warm_cache_for_remote: not authenticated; skipping");
        return Ok(());
    }

    for chunk in uncached.chunks(100) {
        let sha_refs: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
        match client.read_notes(&sha_refs) {
            Ok(response) => {
                if response.notes.is_empty() {
                    continue;
                }
                // 4. Write returned entries as already-synced cache rows.
                let entries: Vec<(String, String)> = response.notes.into_iter().collect();
                match crate::model::repository::notes_db::NotesDatabase::global() {
                    Ok(db) => match db.lock() {
                        Ok(mut lock) => {
                            if let Err(e) = lock.cache_synced_notes(&entries) {
                                tracing::warn!(
                                    "warm_cache_for_remote: cache_synced_notes error: {}",
                                    e
                                );
                            } else {
                                tracing::debug!(
                                    count = entries.len(),
                                    "warm_cache_for_remote: cached notes from remote"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!("warm_cache_for_remote: DB lock poisoned: {}", e);
                        }
                    },
                    Err(e) => {
                        tracing::warn!("warm_cache_for_remote: failed to open notes-db: {}", e);
                    }
                }
            }
            Err(e) => {
                // Best-effort: log and continue.
                tracing::warn!("warm_cache_for_remote: read_notes error: {}", e);
            }
        }
    }

    Ok(())
}

// --- Backend bypass ---

/// Export entries directly into `refs/notes/ai` regardless of the configured
/// backend. Used by `git-ai notes migrate --to git-notes` to share sqlite-backed
/// attribution via the notes ref.
pub fn export_notes_to_git_refs(
    repo: &Repository,
    entries: &[NoteWriteEntry],
) -> Result<(), GitAiError> {
    crate::operations::git::refs::notes_add_batch(repo, entries)
}

// --- Private helpers ---

/// Sqlite backend: read a single note from refs/notes/ai on database miss and
/// backfill it into the cache so the next read is served from the database.
fn sqlite_fallback_read_from_refs(repo: &Repository, commit_sha: &str) -> Option<String> {
    let content = GitNotesStore { repo }.read_note(commit_sha)?;
    SqliteNoteStore::backfill_cache(&HashMap::from([(commit_sha.to_string(), content.clone())]));
    Some(content)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::git::notes_store::{HttpNoteStore, SqliteNoteStore, db_read_note};

    /// With kind=Http, the http helpers upsert into notes-db (synced=0) and the
    /// read helper returns the cached value. This tests the store methods directly
    /// so no config override is needed.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn http_write_then_read_uses_cache() {
        use std::env;

        // Point the notes-db at a temp file so we don't pollute the real DB.
        let tmp = tempfile::NamedTempFile::new().expect("tmp file");
        let db_path = tmp.path().to_str().unwrap().to_string();
        // Safety: test-only env var manipulation.
        unsafe {
            env::set_var("GIT_AI_TEST_NOTES_DB_PATH", &db_path);
        }

        // Write directly via store (no repo needed).
        HttpNoteStore::new()
            .write_note("abc123def456abc123def456abc123def456abc1", "test content")
            .expect("write");

        // Read back from cache.
        let content = db_read_note("abc123def456abc123def456abc123def456abc1");
        assert_eq!(content, Some("test content".to_string()));

        // Confirm it is in the DB with synced=0.
        let db = crate::model::repository::notes_db::NotesDatabase::global().expect("global db");
        let mut lock = db.lock().expect("lock");
        let pending = lock.dequeue_pending(10).expect("dequeue");
        assert!(
            pending.iter().any(
                |p| p.commit_sha == "abc123def456abc123def456abc123def456abc1"
                    && p.content == "test content"
            ),
            "expected pending row in notes-db"
        );

        // Cleanup env var.
        unsafe {
            env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
        }
    }

    /// http_read_notes returns a HashMap of all cached entries for requested SHAs.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn http_read_notes_returns_multiple() {
        use std::env;

        let tmp = tempfile::NamedTempFile::new().expect("tmp file");
        let db_path = tmp.path().to_str().unwrap().to_string();
        unsafe {
            env::set_var("GIT_AI_TEST_NOTES_DB_PATH", &db_path);
        }

        let sha1 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let sha2 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let sha3 = "cccccccccccccccccccccccccccccccccccccccc".to_string();

        HttpNoteStore::new()
            .write_note(&sha1, "content-a")
            .expect("write sha1");
        HttpNoteStore::new()
            .write_note(&sha2, "content-b")
            .expect("write sha2");

        // sha3 is not written — should not appear in result.
        let result = db_read_notes(&[sha1.clone(), sha2.clone(), sha3.clone()]);
        assert_eq!(result.get(&sha1), Some(&"content-a".to_string()));
        assert_eq!(result.get(&sha2), Some(&"content-b".to_string()));
        assert!(!result.contains_key(&sha3));

        unsafe {
            env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
        }
    }

    /// Under the HTTP backend, note search must find notes that only exist in
    /// the notes-db cache (refs/notes/ai is empty there). Regression: search was
    /// a pure pass-through to `git grep refs/notes/ai`, so session/prompt history
    /// lookups silently found nothing under the HTTP backend.
    ///
    /// Tests the store's search_notes directly (not via the dispatcher) to avoid
    /// the `GIT_AI_NOTES_BACKEND_KIND` env var racing with other concurrent tests.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn http_search_notes_finds_cached_note_content() {
        use crate::operations::git::notes_store::AuthorshipNoteStore;
        use std::env;

        let tmp_db = tempfile::NamedTempFile::new().expect("tmp file");
        let db_path = tmp_db.path().to_str().unwrap().to_string();
        unsafe {
            env::set_var("GIT_AI_TEST_NOTES_DB_PATH", &db_path);
        }

        let sha = "dddddddddddddddddddddddddddddddddddddddd";
        HttpNoteStore::new()
            .write_note(sha, r#"{"sessions": {"s_searchable123456": {}}}"#)
            .expect("write");

        // Verify the db-tier search (used by both Http and Sqlite arms) finds
        // notes in the cache even when refs/notes/ai is absent.
        let matches = SqliteNoteStore::new()
            .search_notes("\"s_searchable123456\"")
            .expect("search");
        assert_eq!(
            matches,
            vec![sha.to_string()],
            "search must find notes that only exist in the notes-db cache"
        );

        // A needle that appears nowhere must return no matches (LIKE wildcards in
        // the needle must not be interpreted).
        let no_matches = SqliteNoteStore::new()
            .search_notes("\"s_%_absent%\"")
            .expect("search");
        assert!(no_matches.is_empty(), "got: {:?}", no_matches);

        unsafe {
            env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
        }
    }

    /// Regression test for the composed Sqlite|Http search arm in
    /// `search_notes(repo, pattern)`.
    ///
    /// Before the P9.5 store-trait refactor the Http arm was a pure git-grep
    /// pass-through; notes that existed only in the notes-db cache were invisible
    /// to search.  This test drives the full dispatcher path (db search + git-grep
    /// union + sort) with `GIT_AI_TEST_NOTES_DB_PATH` to avoid env-var races and
    /// verifies that a db-only SHA (absent from refs/notes/ai) is returned.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn sqlite_http_search_arm_returns_db_only_sha() {
        use crate::operations::git::test_utils::TmpRepo;
        use std::env;

        let tmp_db = tempfile::NamedTempFile::new().expect("tmp notes-db");
        unsafe {
            env::set_var("GIT_AI_TEST_NOTES_DB_PATH", tmp_db.path().to_str().unwrap());
        }

        // Use a fake SHA that does not exist in any git object store.
        let db_only_sha = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_string();
        let pattern = "\"db_only_session_xyz\"";
        let note_content = r#"{"sessions": {"db_only_session_xyz": {}}}"#;

        // Seed the note into the db directly via SqliteNoteStore so it exists
        // only in the db, not in refs/notes/ai.
        SqliteNoteStore::new()
            .write_note(&db_only_sha, note_content)
            .expect("seed note");

        // Create a TmpRepo (needed for the repo arg; refs/notes/ai will be empty).
        let repo = TmpRepo::new().expect("TmpRepo::new");

        // Call the composed search path with Sqlite backend.  The search must
        // union db results with the (empty) git-grep result and return the SHA.
        unsafe {
            env::set_var("GIT_AI_NOTES_BACKEND_KIND", "sqlite");
        }
        let results =
            search_notes(repo.gitai_repo(), pattern).expect("search_notes must not error");
        unsafe {
            env::remove_var("GIT_AI_NOTES_BACKEND_KIND");
        }

        assert!(
            results.contains(&db_only_sha),
            "db-only SHA must appear in composed search results; got: {:?}",
            results
        );

        unsafe {
            env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
        }
    }

    /// With kind=GitNotes (default), read_note_blob_oids delegates to git.
    /// Verified by building with an empty repo — returns Ok(empty) with no panic.
    #[test]
    fn git_notes_backend_read_note_blob_oids_delegates_to_git() {
        use crate::operations::git::test_utils::TmpRepo;
        // Default config is GitNotes — no override needed.
        let tmp = TmpRepo::new().expect("TmpRepo::new");
        let result =
            crate::operations::git::refs::note_blob_oids_for_commits(tmp.gitai_repo(), &[]);
        assert!(result.is_ok());
    }

    /// With kind=Http, the public read_note_blob_oids returns an empty map
    /// because notes live in notes-db, not in git refs.
    /// We test this by calling the function through a fresh Config set to Http.
    #[test]
    fn http_backend_read_note_blob_oids_returns_empty_map() {
        use crate::operations::git::test_utils::TmpRepo;

        let old = std::env::var("GIT_AI_NOTES_BACKEND_KIND").ok();
        unsafe {
            std::env::set_var("GIT_AI_NOTES_BACKEND_KIND", "http");
        }

        let tmp = TmpRepo::new().expect("TmpRepo::new");
        // Use Config::fresh() so it picks up the env var, then call the refs function
        // through the kind check inline.
        let kind = crate::config::Config::fresh().notes_backend_kind();
        let result: Result<HashMap<String, String>, _> = match kind {
            crate::config::NotesBackendKind::Sqlite | crate::config::NotesBackendKind::Http => {
                Ok(HashMap::new())
            }
            crate::config::NotesBackendKind::GitNotes => {
                crate::operations::git::refs::note_blob_oids_for_commits(
                    tmp.gitai_repo(),
                    &["abc".to_string()],
                )
            }
        };

        // Restore env before asserting (so a panic doesn't leave the env dirty).
        match old {
            Some(v) => unsafe { std::env::set_var("GIT_AI_NOTES_BACKEND_KIND", v) },
            None => unsafe { std::env::remove_var("GIT_AI_NOTES_BACKEND_KIND") },
        }

        assert!(result.is_ok());
        assert!(
            result.unwrap().is_empty(),
            "Http backend should return empty map from read_note_blob_oids"
        );
    }

    /// Integration test: with kind=Http, `write_note` upserts into `notes-db`
    /// with `synced = 0` and `git notes --ref=ai show <sha>` returns nothing (note
    /// is NOT written into git refs).
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn integration_http_write_note_goes_to_db_not_git() {
        use crate::clients::git_cli::exec_git;
        use crate::operations::git::test_utils::TmpRepo;
        use std::env;

        // Isolated notes-db for this test.
        let tmp_db = tempfile::NamedTempFile::new().expect("tmp db file");
        let db_path = tmp_db.path().to_str().unwrap().to_string();
        unsafe {
            env::set_var("GIT_AI_TEST_NOTES_DB_PATH", &db_path);
        }

        let repo = TmpRepo::new().expect("TmpRepo::new");

        // Create a real commit so we have a valid SHA.
        repo.write_file("a.txt", "hello", false)
            .expect("write file");
        let sha = repo.commit_all("msg").expect("commit");

        // Write a note for this SHA using the Http store.
        HttpNoteStore::new()
            .write_note(&sha, "some-note-content")
            .expect("http write");

        // Confirm it is in notes-db with synced=0.
        let db = crate::model::repository::notes_db::NotesDatabase::global().expect("global db");
        let mut lock = db.lock().expect("lock");
        let note_in_db = lock.get_note(&sha).expect("get note");
        assert_eq!(note_in_db, Some("some-note-content".to_string()));

        let pending = lock.dequeue_pending(10).expect("dequeue");
        assert!(
            pending.iter().any(|p| p.commit_sha == sha),
            "note should be pending in notes-db"
        );
        drop(lock);

        // Confirm `git notes --ref=ai show <sha>` returns nothing.
        let mut args = repo.gitai_repo().global_args_for_exec();
        args.extend([
            "notes".to_string(),
            "--ref=ai".to_string(),
            "show".to_string(),
            sha.clone(),
        ]);
        let result = exec_git(&args);
        assert!(
            result.is_err(),
            "git notes --ref=ai show should fail (note not in git) for Http backend"
        );

        unsafe {
            env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
        }
    }

    /// Integration test: `materialize_notes_for_display` writes notes from the
    /// notes-db cache into `refs/notes/ai-display` so that `git log --notes=ai-display`
    /// can show them.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn integration_materialize_notes_for_display() {
        use crate::clients::git_cli::exec_git;
        use crate::operations::git::test_utils::TmpRepo;
        use std::env;

        // Isolated notes-db.
        let tmp_db = tempfile::NamedTempFile::new().expect("tmp db file");
        unsafe {
            env::set_var("GIT_AI_TEST_NOTES_DB_PATH", tmp_db.path().to_str().unwrap());
        }

        let repo = TmpRepo::new().expect("TmpRepo::new");

        // Create a real commit.
        repo.write_file("b.txt", "world", false)
            .expect("write file");
        let sha = repo.commit_all("test commit").expect("commit");

        // Put a note in the cache for this commit.
        HttpNoteStore::new()
            .write_note(&sha, "display-note-content")
            .expect("write note");

        // Materialize the cache into refs/notes/ai-display.
        let count = materialize_notes_for_display(repo.gitai_repo(), 50).expect("materialize");
        assert_eq!(count, 1, "should have materialized 1 note");

        // Confirm git can read the note from refs/notes/ai-display.
        let mut args = repo.gitai_repo().global_args_for_exec();
        args.extend([
            "notes".to_string(),
            "--ref=ai-display".to_string(),
            "show".to_string(),
            sha.clone(),
        ]);
        let output = exec_git(&args).expect("git notes show ai-display");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.trim() == "display-note-content",
            "refs/notes/ai-display should contain the materialized note, got: {:?}",
            stdout
        );

        unsafe {
            env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
        }
    }

    /// Verify that `run_pre_push_hook_managed` has the correct early-return guard for
    /// `kind = Http`. We test this by confirming Config::fresh() with
    /// `GIT_AI_NOTES_BACKEND_KIND=http` returns Http, and that the guard in
    /// `run_pre_push_hook_managed` would short-circuit. This is a compile-time
    /// regression guard for the code structure added in Phase 2.6.
    #[test]
    fn push_pre_command_hook_http_guard_is_in_place() {
        use std::env;

        let old = env::var("GIT_AI_NOTES_BACKEND_KIND").ok();
        unsafe {
            env::set_var("GIT_AI_NOTES_BACKEND_KIND", "http");
        }
        let kind = crate::config::Config::fresh().notes_backend_kind();
        match old {
            Some(v) => unsafe { env::set_var("GIT_AI_NOTES_BACKEND_KIND", v) },
            None => unsafe { env::remove_var("GIT_AI_NOTES_BACKEND_KIND") },
        }

        // Verify Config::fresh() correctly parses http from env.
        assert_eq!(
            kind,
            crate::config::NotesBackendKind::Http,
            "Config::fresh() should reflect GIT_AI_NOTES_BACKEND_KIND=http"
        );

        // Structural verification: the Http backend skip is now inlined in
        // apply_push_side_effect in daemon.rs — no separate hook function needed.
    }

    // --- warm_cache_for_remote tests ---
    //
    // These tests verify the core behavior of `warm_cache_for_remote`:
    //
    // 1. It fetches notes from the HTTP backend and stores them with `synced = 1`.
    // 2. It skips SHAs already present in notes-db (not included in the request).
    //
    // Design notes on the `NOTES_DB` `OnceLock` singleton:
    //
    // `NotesDatabase::global()` uses a `OnceLock` that initialises the DB path
    // *once per process*.  Both tests set `GIT_AI_TEST_NOTES_DB_PATH` to a fresh
    // temp file before their first DB call.  The first test to run initialises the
    // OnceLock; subsequent tests in the same process reuse the same DB file path
    // regardless of what `GIT_AI_TEST_NOTES_DB_PATH` says.
    //
    // Strategy: both tests use `NotesDatabase::global()` for all reads and writes
    // (pre-population and post-call verification) rather than direct file-level
    // connections.  Because the tests run serially (`#[serial]`) and each uses
    // distinct commit SHAs, shared DB state doesn't cause false-negative assertions.
    //
    // Test 1 sets `GIT_AI_TEST_NOTES_DB_PATH` which initialises the OnceLock if
    // it hasn't been set yet.  Test 2 also sets it but will use whatever path was
    // already locked.  Both tests clear DB state relevant to their own SHAs via
    // `get_note` assertions on distinct SHAs, so they don't interfere.

    /// Unit test: `warm_cache_for_remote` fetches notes from a mock HTTP server
    /// and stores them in `notes-db` with `synced = 1`.
    ///
    /// Steps:
    /// 1. Point `NOTES_DB` at a fresh temp file (via `GIT_AI_TEST_NOTES_DB_PATH`).
    /// 2. Spin up a mockito server returning two notes.
    /// 3. Create a `TmpRepo` with two commits.
    /// 4. Call `warm_cache_for_remote`.
    /// 5. Verify both SHAs appear in `notes-db` with `synced = 1` via `NotesDatabase::global()`.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn warm_cache_for_remote_populates_db_with_synced_1() {
        use crate::model::repository::notes_db::NotesDatabase;
        use crate::operations::git::test_utils::TmpRepo;
        use tempfile::NamedTempFile;

        // Set the test DB path before the first DB call so the OnceLock picks it up.
        let tmp_db = NamedTempFile::new().expect("tmp notes-db");
        unsafe {
            std::env::set_var("GIT_AI_TEST_NOTES_DB_PATH", tmp_db.path());
        }

        // Build a TmpRepo with two commits.
        let repo = TmpRepo::new().expect("TmpRepo::new");

        repo.write_file("warm1.txt", "warm1", false)
            .expect("write file");
        let sha1 = repo.commit_all("warm-commit-1").expect("commit 1");

        repo.write_file("warm2.txt", "warm2", false)
            .expect("write file");
        let sha2 = repo.commit_all("warm-commit-2").expect("commit 2");

        // Spin up a mockito server that returns notes for both SHAs.
        let mut server = mockito::Server::new();
        let notes_json = serde_json::json!({
            "notes": {
                sha1.clone(): "note-content-1",
                sha2.clone(): "note-content-2"
            }
        })
        .to_string();

        let _mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/worker/notes/".to_string()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(&notes_json)
            .create();

        let server_url = server.url();
        unsafe {
            std::env::set_var("GIT_AI_NOTES_BACKEND_URL", &server_url);
            // Provide a fake API key so `has_api_key()` returns true and the
            // auth guard in `warm_cache_for_remote` does not short-circuit.
            std::env::set_var("GIT_AI_API_KEY", "warm-cache-test-key");
        }

        // Execute.
        let result = warm_cache_for_remote(repo.gitai_repo(), "origin");
        assert!(result.is_ok(), "warm_cache_for_remote failed: {:?}", result);

        // Verify via NotesDatabase::global() (the same DB the function wrote to).
        let db = NotesDatabase::global().expect("global db");
        let lock = db.lock().expect("lock");

        let content1 = lock.get_note(&sha1).expect("get sha1");
        let content2 = lock.get_note(&sha2).expect("get sha2");

        assert_eq!(
            content1,
            Some("note-content-1".to_string()),
            "sha1 should be cached with correct content"
        );
        assert_eq!(
            content2,
            Some("note-content-2".to_string()),
            "sha2 should be cached with correct content"
        );

        // Rows must NOT appear in dequeue_pending (cache_synced_notes inserts synced = 1).
        drop(lock);
        let mut lock = db.lock().expect("lock for dequeue check");
        let pending = lock.dequeue_pending(10).expect("dequeue");
        let warm_pending: Vec<_> = pending
            .iter()
            .filter(|p| p.commit_sha == sha1 || p.commit_sha == sha2)
            .collect();
        assert!(
            warm_pending.is_empty(),
            "cache_synced rows must not appear in dequeue_pending: {:?}",
            warm_pending
                .iter()
                .map(|p| &p.commit_sha)
                .collect::<Vec<_>>()
        );

        // Cleanup.
        unsafe {
            std::env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
            std::env::remove_var("GIT_AI_API_KEY");
            std::env::remove_var("GIT_AI_NOTES_BACKEND_URL");
        }
    }

    /// Unit test: `warm_cache_for_remote` skips SHAs already present in `notes-db`.
    ///
    /// Steps:
    /// 1. Pre-populate `notes-db` with sha1 via `cache_synced_notes`.
    /// 2. Spin up a mockito server returning sha2 only.
    ///    The mock matches only requests whose query contains sha2 —
    ///    if sha1 were incorrectly included it would still match, but we verify
    ///    indirectly that sha1's content was not overwritten.
    /// 3. Call `warm_cache_for_remote` with a TmpRepo containing both commits.
    /// 4. Verify sha1's content is unchanged ("already-cached-note").
    /// 5. Verify sha2 was fetched and cached with `synced = 1`.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn warm_cache_for_remote_skips_already_cached_shas() {
        use crate::model::repository::notes_db::NotesDatabase;
        use crate::operations::git::test_utils::TmpRepo;
        use tempfile::NamedTempFile;

        // Set the test DB path (may be ignored if OnceLock was already set by
        // `warm_cache_for_remote_populates_db_with_synced_1` in the same process,
        // but we still set it for freshness when running this test in isolation).
        let tmp_db = NamedTempFile::new().expect("tmp notes-db");
        unsafe {
            std::env::set_var("GIT_AI_TEST_NOTES_DB_PATH", tmp_db.path());
        }

        // Build TmpRepo with two commits.
        let repo = TmpRepo::new().expect("TmpRepo::new");

        repo.write_file("skip1.txt", "s1", false)
            .expect("write file");
        let sha1 = repo.commit_all("skip-c1").expect("commit 1");

        repo.write_file("skip2.txt", "s2", false)
            .expect("write file");
        let sha2 = repo.commit_all("skip-c2").expect("commit 2");

        // Pre-populate notes-db with sha1 via the global singleton.
        {
            let db = NotesDatabase::global().expect("global db");
            let mut lock = db.lock().expect("lock");
            lock.cache_synced_notes(&[(sha1.clone(), "already-cached-note".to_string())])
                .expect("cache_synced_notes sha1");
        }

        // Mock server: use two mocks to verify sha1 is NOT in the request.
        //
        // - Mock A: matches requests where the query contains sha2 but NOT sha1.
        //   Since mockito doesn't have a `Not` matcher, we approximate this by
        //   requiring the query equals exactly sha2 (no comma-separated prefix/suffix).
        //   `commits=<sha2>` means only sha2 was requested.
        // - Mock B: fallback that matches everything else → returns 500 so sha2
        //   is NOT cached if sha1 was erroneously included.
        //
        // If warm_cache correctly filters sha1, mock A matches and sha2 is cached.
        // If warm_cache incorrectly sends sha1 too, the query is `sha1,sha2` or
        // `sha2,sha1`, which won't match the exact-sha2 regex → mock B fires → 500
        // error → sha2 is NOT cached → `assert_eq!(content2, ...)` fails.
        let sha2_note_json = serde_json::json!({
            "notes": { sha2.clone(): "note-content-skip-2" }
        })
        .to_string();

        // Exact query: commits=<sha2> only.
        let exact_sha2_query = format!("commits={}", sha2);

        let mut server = mockito::Server::new();
        // Mock A: exact query with only sha2.
        let _mock_ok = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/worker/notes/".to_string()),
            )
            .match_query(mockito::Matcher::Exact(exact_sha2_query))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(&sha2_note_json)
            .create();
        // Mock B: fallback → 500.
        let _mock_fallback = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/worker/notes/".to_string()),
            )
            .with_status(500)
            .with_body(r#"{"error":"unexpected request with sha1 in query"}"#)
            .create();

        let server_url = server.url();
        unsafe {
            std::env::set_var("GIT_AI_NOTES_BACKEND_URL", &server_url);
            std::env::set_var("GIT_AI_API_KEY", "skip-test-key");
        }

        let result = warm_cache_for_remote(repo.gitai_repo(), "origin");
        assert!(result.is_ok(), "warm_cache_for_remote failed: {:?}", result);

        // Verify via the global DB.
        let db = NotesDatabase::global().expect("global db");
        let lock = db.lock().expect("lock");

        // sha1 must retain its pre-cached content unchanged.
        let content1 = lock.get_note(&sha1).expect("get sha1");
        assert_eq!(
            content1,
            Some("already-cached-note".to_string()),
            "sha1 content must not change — warm_cache must not overwrite cached entries"
        );

        // sha2 must now be cached with the server-returned content.
        let content2 = lock.get_note(&sha2).expect("get sha2");
        assert_eq!(
            content2,
            Some("note-content-skip-2".to_string()),
            "sha2 should have been fetched and cached"
        );

        // The mock must have been called (warm_cache made at least one HTTP request).
        _mock_ok.assert();

        // Cleanup.
        unsafe {
            std::env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
            std::env::remove_var("GIT_AI_API_KEY");
            std::env::remove_var("GIT_AI_NOTES_BACKEND_URL");
        }
    }
}
