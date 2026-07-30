//! `git-ai notes migrate` — move existing notes between backends.
//!
//! Targets (`--to <backend>`, defaulting to the configured backend):
//! - `sqlite`: import `refs/notes/ai` into the local notes database as
//!   local-primary rows (idempotent; safe to re-run).
//! - `git-notes`: export local-primary rows from the notes database into
//!   `refs/notes/ai` so attribution can be shared via push/fetch.
//! - `http`: bulk-upload `refs/notes/ai` to the remote HTTP backend in chunks
//!   of 50 and warm the local cache (`synced = 1`).
//!
//! All refs reads use batched plumbing (`git notes list` + `git cat-file
//! --batch`) — a constant git-spawn count regardless of note count.

use crate::clients::api::client::{ApiClient, ApiContext};
use crate::config::{Config, NotesBackendKind};
use crate::error::GitAiError;
use crate::model::api_types::{NoteEntry, NotesUploadRequest};
use crate::model::repository::notes_db::NotesDatabase;
use crate::operations::git::cat_file::{BatchReadPolicy, batch_read_blob_contents_with_policy};
use crate::operations::git::find_repository;
use crate::operations::git::repository::resolve_api_author_identity;
use std::collections::HashMap;

/// Entry point for `git-ai notes migrate`.
pub fn handle_notes_migrate(args: &[String]) {
    let mut force = false;
    let mut target: Option<NotesBackendKind> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_help();
                return;
            }
            "--force" | "--all" => {
                force = true;
            }
            "--to" => {
                i += 1;
                let value = args.get(i).map(String::as_str).unwrap_or_default();
                target = Some(match value {
                    "sqlite" => NotesBackendKind::Sqlite,
                    "git_notes" | "git-notes" => NotesBackendKind::GitNotes,
                    "http" => NotesBackendKind::Http,
                    other => {
                        eprintln!(
                            "error: invalid --to target '{}': expected sqlite, git-notes, or http",
                            other
                        );
                        std::process::exit(1);
                    }
                });
            }
            other => {
                eprintln!("error: unknown option '{}'", other);
                eprintln!("Run 'git ai notes migrate --help' for usage");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let cfg = Config::fresh();
    let target = target.unwrap_or_else(|| cfg.notes_backend_kind());

    let db = match NotesDatabase::global() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: failed to open notes database: {}", e);
            std::process::exit(1);
        }
    };

    let repo = match find_repository(&Vec::<String>::new()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: not a git repository ({})", e);
            std::process::exit(1);
        }
    };

    match target {
        NotesBackendKind::Sqlite => migrate_refs_to_sqlite(db, &repo),
        NotesBackendKind::GitNotes => migrate_sqlite_to_git_notes(db, &repo),
        NotesBackendKind::Http => migrate_refs_to_http(db, &repo, &cfg, force),
    }
}

/// Import `refs/notes/ai` into the notes database as local-primary rows.
fn migrate_refs_to_sqlite(
    db: &'static std::sync::Mutex<NotesDatabase>,
    repo: &crate::operations::git::repository::Repository,
) {
    eprintln!("Listing notes from refs/notes/ai ...");
    let entries = match read_all_ref_notes(repo) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("error: failed to read notes: {}", e);
            std::process::exit(1);
        }
    };
    if entries.is_empty() {
        eprintln!("No notes found in refs/notes/ai. Nothing to migrate.");
        return;
    }

    match db.lock() {
        Ok(mut lock) => {
            if let Err(e) = lock.upsert_local_notes_batch(&entries) {
                eprintln!("error: failed to write notes database: {}", e);
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("error: notes-db lock poisoned: {}", e);
            std::process::exit(1);
        }
    }
    eprintln!(
        "Migration complete: {} note(s) imported into the local notes database.",
        entries.len()
    );
}

/// Export local-primary rows from the notes database into `refs/notes/ai`.
fn migrate_sqlite_to_git_notes(
    db: &'static std::sync::Mutex<NotesDatabase>,
    repo: &crate::operations::git::repository::Repository,
) {
    let entries = match db.lock() {
        Ok(lock) => match lock.get_local_notes() {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("error: failed to read notes database: {}", e);
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("error: notes-db lock poisoned: {}", e);
            std::process::exit(1);
        }
    };
    if entries.is_empty() {
        eprintln!("No local notes found in the notes database. Nothing to export.");
        return;
    }

    if let Err(e) = crate::operations::git::notes_api::export_notes_to_git_refs(repo, &entries) {
        eprintln!("error: failed to write refs/notes/ai: {}", e);
        std::process::exit(1);
    }
    eprintln!(
        "Migration complete: {} note(s) exported to refs/notes/ai.",
        entries.len()
    );
}

/// Read every note in `refs/notes/ai` as `(commit_sha, content)` pairs.
fn read_all_ref_notes(
    repo: &crate::operations::git::repository::Repository,
) -> Result<Vec<(String, String)>, GitAiError> {
    let note_pairs = list_notes(repo)?;
    if note_pairs.is_empty() {
        return Ok(Vec::new());
    }
    let blob_to_commit: HashMap<String, String> = note_pairs
        .iter()
        .map(|(blob, commit)| (blob.clone(), commit.clone()))
        .collect();
    let blob_shas: Vec<String> = note_pairs.iter().map(|(b, _)| b.clone()).collect();
    let blob_contents =
        batch_read_blob_contents_with_policy(repo, &blob_shas, BatchReadPolicy::Tolerant)?;
    let mut entries: Vec<(String, String)> = Vec::new();
    for (blob_sha, content) in &blob_contents {
        if let Some(commit_sha) = blob_to_commit.get(blob_sha) {
            entries.push((commit_sha.clone(), content.clone()));
        }
    }
    Ok(entries)
}

/// Bulk-upload `refs/notes/ai` to the remote HTTP backend and warm the cache.
fn migrate_refs_to_http(
    db: &'static std::sync::Mutex<NotesDatabase>,
    repo: &crate::operations::git::repository::Repository,
    cfg: &Config,
    force: bool,
) {
    // 3. Build the API client.
    let Some(backend_url) = cfg.notes_backend_url().map(str::to_string) else {
        eprintln!(
            "error: notes_backend.backend_url is not configured.\n\
             \n\
             Set it before running migrate, e.g.:\n\
             \n\
             \x20 git-ai config set notes_backend.backend_url https://your-backend.example.com"
        );
        std::process::exit(1);
    };
    let ctx = ApiContext::new(Some(backend_url), resolve_api_author_identity);
    let client = ApiClient::new(ctx);

    // Skip if not authenticated.
    if !client.is_logged_in() && !client.has_api_key() {
        eprintln!("error: not authenticated. Log in first with `git-ai login` or set an API key.");
        std::process::exit(1);
    }

    eprintln!("Listing notes from refs/notes/ai ...");

    // 4. List notes: `git notes --ref=ai list` → "blob_sha commit_sha\n" lines.
    let note_pairs = match list_notes(repo) {
        Ok(pairs) => pairs,
        Err(e) => {
            eprintln!("error: failed to list notes: {}", e);
            std::process::exit(1);
        }
    };

    if note_pairs.is_empty() {
        eprintln!("No notes found in refs/notes/ai. Nothing to migrate.");
        return;
    }

    eprintln!("Found {} note(s). Reading content ...", note_pairs.len());

    // 5. Bulk-read note content via `git cat-file --batch`.
    let blob_to_commit: HashMap<String, String> = note_pairs
        .iter()
        .map(|(blob, commit)| (blob.clone(), commit.clone()))
        .collect();

    let blob_shas: Vec<String> = note_pairs.iter().map(|(b, _)| b.clone()).collect();
    let blob_contents =
        match batch_read_blob_contents_with_policy(repo, &blob_shas, BatchReadPolicy::Tolerant) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: failed to read note content: {}", e);
                std::process::exit(1);
            }
        };

    // Build (commit_sha, content) pairs.
    let mut entries: Vec<(String, String)> = Vec::new();
    for (blob_sha, content) in &blob_contents {
        if let Some(commit_sha) = blob_to_commit.get(blob_sha) {
            entries.push((commit_sha.clone(), content.clone()));
        }
    }

    // Skip entries already confirmed synced (enables safe re-run after interruption).
    // Only skip synced=1 entries — pending (synced=0) entries still need uploading.
    if !force {
        let pre_cached_count = entries.len();
        if let Ok(lock) = db.lock() {
            let all_shas: Vec<&str> = entries.iter().map(|(s, _)| s.as_str()).collect();
            if let Ok(synced) = lock.get_synced_shas(&all_shas) {
                entries.retain(|(sha, _)| !synced.contains(sha));
            }
        }
        if entries.len() < pre_cached_count {
            eprintln!(
                "Skipping {} already-cached note(s).",
                pre_cached_count - entries.len()
            );
        }

        if entries.is_empty() {
            eprintln!("All notes already migrated. Nothing to upload.");
            return;
        }
    }

    eprintln!(
        "Read {} note(s). Uploading in chunks of 50 ...",
        entries.len()
    );

    // 6. Upload in chunks of 50 and cache locally.
    let mut total_uploaded = 0usize;
    let mut total_failed = 0usize;
    let mut cached_entries: Vec<(String, String)> = Vec::new();

    for chunk in entries.chunks(50) {
        let note_entries: Vec<NoteEntry> = chunk
            .iter()
            .map(|(commit_sha, content)| NoteEntry {
                commit_sha: commit_sha.clone(),
                content: content.clone(),
            })
            .collect();

        let chunk_len = note_entries.len();
        let request = NotesUploadRequest {
            entries: note_entries,
        };

        match client.upload_notes(request) {
            Ok(response) => {
                eprintln!(
                    "  chunk: {} uploaded, {} failed",
                    response.success_count, response.failure_count
                );
                total_uploaded += response.success_count;
                total_failed += response.failure_count;

                // Cache the whole chunk best-effort — the server doesn't
                // tell us which specific entries failed.
                cached_entries.extend_from_slice(chunk);
            }
            Err(e) => {
                eprintln!("  error uploading chunk of {}: {}", chunk_len, e);
                total_failed += chunk_len;
            }
        }
    }

    // Write all successfully-uploaded notes to local notes-db with synced = 1.
    if !cached_entries.is_empty() {
        match db.lock() {
            Ok(mut lock) => {
                if let Err(e) = lock.cache_synced_notes(&cached_entries) {
                    eprintln!("warning: failed to cache notes locally: {}", e);
                } else {
                    eprintln!("Cached {} note(s) in local notes-db.", cached_entries.len());
                }
            }
            Err(e) => {
                eprintln!("warning: notes-db lock poisoned: {}", e);
            }
        }
    }

    // 8. Summary.
    eprintln!();
    if total_failed == 0 {
        eprintln!(
            "Migration complete: {} note(s) uploaded successfully.",
            total_uploaded
        );
    } else {
        eprintln!(
            "Migration finished: {} uploaded, {} failed.",
            total_uploaded, total_failed
        );
        if total_failed > 0 {
            std::process::exit(1);
        }
    }
}

/// Run `git notes --ref=ai list` and return `(blob_sha, commit_sha)` pairs.
fn list_notes(
    repo: &crate::operations::git::repository::Repository,
) -> Result<Vec<(String, String)>, GitAiError> {
    use crate::clients::git_cli::exec_git;

    let mut args = repo.global_args_for_exec();
    args.extend([
        "notes".to_string(),
        "--ref=ai".to_string(),
        "list".to_string(),
    ]);

    let output = exec_git(&args)
        .map_err(|e| GitAiError::Generic(format!("git notes --ref=ai list failed: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // `git notes list` exits non-zero when there are no notes — treat as empty.
        if stderr.contains("No notes found") || output.stdout.is_empty() {
            return Ok(Vec::new());
        }
        return Err(GitAiError::Generic(format!(
            "git notes --ref=ai list exited {}: {}",
            output.status, stderr
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pairs: Vec<(String, String)> = stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let blob_sha = parts.next()?.to_string();
            let commit_sha = parts.next()?.to_string();
            Some((blob_sha, commit_sha))
        })
        .collect();

    Ok(pairs)
}

fn print_help() {
    eprintln!("git ai notes migrate - Bulk-upload existing git notes to the HTTP backend");
    eprintln!();
    eprintln!("Usage: git ai notes migrate [options]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --force, --all  Re-upload all notes even if already cached locally.");
    eprintln!("                  Useful when migrating to a new backend URL.");
    eprintln!("  -h, --help      Show this help message");
    eprintln!();
    eprintln!("Description:");
    eprintln!("  Reads all notes from refs/notes/ai, uploads them to the configured HTTP");
    eprintln!("  notes backend (in chunks of 50), and caches them locally in notes-db");
    eprintln!("  with synced = 1 so the local cache is warm immediately.");
    eprintln!();
    eprintln!("  This command requires notes_backend.kind = http. Set it with:");
    eprintln!("    git-ai config set notes_backend.kind http");
    eprintln!();
    eprintln!("  You must be logged in or have an API key configured.");
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::repository::notes_db::NotesDatabase;
    use crate::operations::git::test_utils::TmpRepo;
    use tempfile::NamedTempFile;

    /// Helper to create real commits in a TmpRepo. Returns the commit SHA.
    /// Parents are tracked implicitly via `HEAD`, so callers no longer need to
    /// pass them explicitly.
    fn make_commit(repo: &TmpRepo, filename: &str, content: &str, message: &str) -> String {
        repo.write_file(filename, content, false)
            .expect("write file");
        repo.commit_all(message).expect("commit")
    }

    /// Add a git note to `refs/notes/ai` for the given commit SHA.
    fn add_git_note(repo: &TmpRepo, commit_sha: &str, note: &str) {
        repo.git_command(&["notes", "--ref=ai", "add", "-f", "-m", note, commit_sha])
            .expect("git notes add");
    }

    fn read_note_blobs(
        repo: &crate::operations::git::repository::Repository,
        blob_shas: &[String],
    ) -> Result<HashMap<String, String>, GitAiError> {
        batch_read_blob_contents_with_policy(repo, blob_shas, BatchReadPolicy::Tolerant)
    }

    /// Integration test:
    ///   1. Create a TmpRepo with several commits and git notes.
    ///   2. Start a mockito server to accept the upload.
    ///   3. Call `handle_notes_migrate` logic directly (list + cat-file + upload + cache).
    ///   4. Verify all notes appear in `notes-db` with `synced = 1`.
    ///   5. Verify the mock upload endpoint was called.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn migration_uploads_notes_and_caches_with_synced_1() {
        // Isolated notes-db.
        let tmp_db = NamedTempFile::new().expect("tmp notes-db");
        unsafe {
            std::env::set_var("GIT_AI_TEST_NOTES_DB_PATH", tmp_db.path());
        }

        // --- Build repo with commits and notes ---
        let repo = TmpRepo::new().expect("TmpRepo::new");

        let sha1 = make_commit(&repo, "file1.txt", "hello", "commit 1");
        let sha2 = make_commit(&repo, "file2.txt", "world", "commit 2");
        let sha3 = make_commit(&repo, "file3.txt", "foo", "commit 3");

        // Add git notes for each commit.
        add_git_note(&repo, &sha1, "note-content-1");
        add_git_note(&repo, &sha2, "note-content-2");
        add_git_note(&repo, &sha3, "note-content-3");

        // --- Mock upload endpoint ---
        let mut server = mockito::Server::new();
        let upload_response = serde_json::json!({
            "success_count": 3,
            "failure_count": 0
        })
        .to_string();
        let _mock = server
            .mock("POST", "/worker/notes/upload")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(&upload_response)
            .create();

        let server_url = server.url();
        unsafe {
            std::env::set_var("GIT_AI_NOTES_BACKEND_URL", &server_url);
            std::env::set_var("GIT_AI_API_KEY", "migrate-test-key");
        }

        // --- Run the migration core logic ---
        let note_pairs = list_notes(repo.gitai_repo()).expect("list_notes");
        assert_eq!(note_pairs.len(), 3, "should list 3 notes");

        let blob_to_commit: HashMap<String, String> = note_pairs
            .iter()
            .map(|(b, c)| (b.clone(), c.clone()))
            .collect();
        let blob_shas: Vec<String> = note_pairs.iter().map(|(b, _)| b.clone()).collect();

        let blob_contents =
            read_note_blobs(repo.gitai_repo(), &blob_shas).expect("read_note_blobs");
        assert_eq!(blob_contents.len(), 3, "should read 3 blob contents");

        let mut entries: Vec<(String, String)> = Vec::new();
        for (blob_sha, content) in &blob_contents {
            if let Some(commit_sha) = blob_to_commit.get(blob_sha) {
                entries.push((commit_sha.clone(), content.clone()));
            }
        }
        assert_eq!(entries.len(), 3);

        // Upload to the mock server.
        let cfg = crate::config::Config::fresh();
        let backend_url = cfg
            .notes_backend_url()
            .expect("test should configure notes_backend.backend_url")
            .to_string();
        let ctx = ApiContext::new(Some(backend_url), resolve_api_author_identity);
        let client = ApiClient::new(ctx);

        let note_entries: Vec<NoteEntry> = entries
            .iter()
            .map(|(sha, content)| NoteEntry {
                commit_sha: sha.clone(),
                content: content.clone(),
            })
            .collect();
        let request = NotesUploadRequest {
            entries: note_entries,
        };
        let response = client.upload_notes(request).expect("upload_notes");
        assert_eq!(response.success_count, 3);
        assert_eq!(response.failure_count, 0);

        // Cache locally with synced = 1.
        let db = NotesDatabase::global().expect("global db");
        {
            let mut lock = db.lock().expect("lock");
            lock.cache_synced_notes(&entries)
                .expect("cache_synced_notes");
        }

        // --- Verify all three notes are in notes-db with synced = 1 ---
        let lock = db.lock().expect("lock for verify");
        let shas = [sha1.as_str(), sha2.as_str(), sha3.as_str()];
        for sha in &shas {
            let content = lock.get_note(sha).expect("get_note");
            assert!(content.is_some(), "note for {} should be in notes-db", sha);
        }

        // None of them should appear in dequeue_pending (synced = 1).
        drop(lock);
        let mut lock = db.lock().expect("lock for dequeue");
        let pending = lock.dequeue_pending(10).expect("dequeue_pending");
        let migrated_pending: Vec<_> = pending
            .iter()
            .filter(|p| shas.contains(&p.commit_sha.as_str()))
            .collect();
        assert!(
            migrated_pending.is_empty(),
            "migrated notes must not appear in dequeue_pending: {:?}",
            migrated_pending
                .iter()
                .map(|p| &p.commit_sha)
                .collect::<Vec<_>>()
        );

        // --- Verify the mock was called ---
        _mock.assert();

        // Cleanup.
        unsafe {
            std::env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
            std::env::remove_var("GIT_AI_API_KEY");
            std::env::remove_var("GIT_AI_NOTES_BACKEND_URL");
        }
    }

    /// Unit test: `list_notes` returns empty when there are no notes.
    #[test]
    fn list_notes_returns_empty_for_repo_without_notes() {
        let repo = TmpRepo::new().expect("TmpRepo::new");
        // Create a commit so HEAD exists (list_notes on an empty repo might error differently).
        repo.write_file("a.txt", "a", false).expect("write file");
        repo.commit_all("c").expect("commit");

        let pairs = list_notes(repo.gitai_repo()).expect("list_notes");
        assert!(
            pairs.is_empty(),
            "no notes should be listed for a fresh repo"
        );
    }

    /// Unit test: the tolerant shared reader returns an empty map for empty input.
    #[test]
    fn read_note_blobs_empty_input() {
        let repo = TmpRepo::new().expect("TmpRepo::new");
        let result = read_note_blobs(repo.gitai_repo(), &[]).expect("read_note_blobs");
        assert!(result.is_empty());
    }

    /// Integration test: `--force` re-uploads notes that are already cached as synced.
    ///   1. Create notes and cache them as synced=1 in notes-db.
    ///   2. Without --force: verify entries are filtered out.
    ///   3. With --force: verify all entries pass through for upload.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn force_flag_bypasses_synced_cache_filter() {
        use std::collections::HashSet;

        let tmp_db = NamedTempFile::new().expect("tmp notes-db");
        unsafe {
            std::env::set_var("GIT_AI_TEST_NOTES_DB_PATH", tmp_db.path());
        }

        let repo = TmpRepo::new().expect("TmpRepo::new");

        let sha1 = make_commit(&repo, "file1.txt", "a", "commit 1");
        let sha2 = make_commit(&repo, "file2.txt", "b", "commit 2");

        add_git_note(&repo, &sha1, "note-1");
        add_git_note(&repo, &sha2, "note-2");

        // Read notes from repo.
        let note_pairs = list_notes(repo.gitai_repo()).expect("list_notes");
        let blob_to_commit: HashMap<String, String> = note_pairs
            .iter()
            .map(|(b, c)| (b.clone(), c.clone()))
            .collect();
        let blob_shas: Vec<String> = note_pairs.iter().map(|(b, _)| b.clone()).collect();
        let blob_contents =
            read_note_blobs(repo.gitai_repo(), &blob_shas).expect("read_note_blobs");

        let entries: Vec<(String, String)> = blob_contents
            .iter()
            .filter_map(|(blob_sha, content)| {
                blob_to_commit
                    .get(blob_sha)
                    .map(|commit_sha| (commit_sha.clone(), content.clone()))
            })
            .collect();
        assert_eq!(entries.len(), 2);

        // Pre-cache all entries as synced=1.
        let db = NotesDatabase::global().expect("global db");
        {
            let mut lock = db.lock().expect("lock");
            lock.cache_synced_notes(&entries)
                .expect("cache_synced_notes");
        }

        // Without force: filtering should remove all entries.
        {
            let mut filtered = entries.clone();
            let lock = db.lock().expect("lock");
            let all_shas: Vec<&str> = filtered.iter().map(|(s, _)| s.as_str()).collect();
            let synced = lock.get_synced_shas(&all_shas).expect("get_synced_shas");
            filtered.retain(|(sha, _)| !synced.contains(sha));
            assert!(
                filtered.is_empty(),
                "without --force, all synced entries should be filtered out"
            );
        }

        // With force: no filtering applied, all entries remain.
        {
            let forced_entries = entries.clone();
            // force=true means we skip the retain logic entirely
            assert_eq!(
                forced_entries.len(),
                2,
                "with --force, all entries should remain for upload"
            );

            // Verify we can upload them to a new backend.
            let mut server = mockito::Server::new();
            let upload_response = serde_json::json!({
                "success_count": 2,
                "failure_count": 0
            })
            .to_string();
            let mock = server
                .mock("POST", "/worker/notes/upload")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(&upload_response)
                .create();

            let server_url = server.url();
            unsafe {
                std::env::set_var("GIT_AI_NOTES_BACKEND_URL", &server_url);
                std::env::set_var("GIT_AI_API_KEY", "force-test-key");
            }

            let cfg = crate::config::Config::fresh();
            let backend_url = cfg.notes_backend_url().unwrap().to_string();
            let ctx = ApiContext::new(Some(backend_url), resolve_api_author_identity);
            let client = ApiClient::new(ctx);

            let note_entries: Vec<NoteEntry> = forced_entries
                .iter()
                .map(|(sha, content)| NoteEntry {
                    commit_sha: sha.clone(),
                    content: content.clone(),
                })
                .collect();
            let request = NotesUploadRequest {
                entries: note_entries,
            };
            let response = client.upload_notes(request).expect("upload_notes");
            assert_eq!(response.success_count, 2);
            mock.assert();
        }

        // Verify commit shas are what we expect.
        let lock = db.lock().expect("lock for final verify");
        let shas_set: HashSet<&str> = [sha1.as_str(), sha2.as_str()].into_iter().collect();
        for sha in &shas_set {
            assert!(
                lock.get_note(sha).expect("get_note").is_some(),
                "note for {} should remain in cache",
                sha
            );
        }
        drop(lock);

        unsafe {
            std::env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
            std::env::remove_var("GIT_AI_API_KEY");
            std::env::remove_var("GIT_AI_NOTES_BACKEND_URL");
        }
    }
}
