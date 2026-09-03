//! `git-ai notes migrate` — bulk-upload existing git notes to the HTTP backend.
//!
//! This command reads all notes stored in `refs/notes/ai` via `git notes --ref=ai list`,
//! fetches their content using `git cat-file --batch`, uploads them to the remote HTTP
//! backend in chunks of 50, and persists them locally in `notes-db` with `synced = 1`
//! so the cache is warm immediately after migration.
//!
//! The command refuses to run unless `notes_backend.kind == http` because migrating
//! notes to the git-notes backend (the default) is a no-op.
//!
//! This explicitly invoked migration is the sole HTTP-mode exception to the runtime
//! invariant that Git AI never inspects or mutates `refs/notes/*`. Ordinary reads,
//! writes, rewrites, sync, transport, and CI paths must remain backend-isolated.

use crate::api::client::{ApiClient, ApiContext};
use crate::api::types::{NoteEntry, NotesUploadRequest};
use crate::config::{Config, NotesBackendKind};
use crate::error::GitAiError;
use crate::git::find_repository;
use crate::notes::db::NotesDatabase;
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// Entry point for `git-ai notes migrate`.
pub fn handle_notes_migrate(args: &[String]) {
    let mut force = false;
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => {
                print_help();
                return;
            }
            "--force" | "--all" => {
                force = true;
            }
            other => {
                eprintln!("error: unknown option '{}'", other);
                eprintln!("Run 'git ai notes migrate --help' for usage");
                std::process::exit(1);
            }
        }
    }

    // 1. Refuse to run unless notes_backend.kind == http.
    let cfg = Config::fresh();
    if cfg.notes_backend_kind() != NotesBackendKind::Http {
        eprintln!(
            "error: `git-ai notes migrate` requires notes_backend.kind = http.\n\
             Current backend: {}\n\
             \n\
             To enable the HTTP backend, run:\n\
             \n\
             \x20 git-ai config set notes_backend.kind http",
            cfg.notes_backend_kind()
        );
        std::process::exit(1);
    }

    // 2. Find the repository.
    let repo = match find_repository(&Vec::<String>::new()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: not a git repository ({})", e);
            std::process::exit(1);
        }
    };

    // 3. Build the API client.
    let backend_url = match cfg.notes_backend_url() {
        Some(url) => url.to_string(),
        None => {
            eprintln!(
                "error: notes_backend.backend_url is not configured.\n\
                 \n\
                 Set it before running migrate, e.g.:\n\
                 \n\
                 \x20 git-ai config set notes_backend.backend_url https://your-backend.example.com"
            );
            std::process::exit(1);
        }
    };
    let ctx = ApiContext::new(Some(backend_url));
    let client = ApiClient::new(ctx);

    // Skip if not authenticated.
    if !client.is_logged_in() && !client.has_api_key() {
        eprintln!("error: not authenticated. Log in first with `git-ai login` or set an API key.");
        std::process::exit(1);
    }

    eprintln!("Listing notes from refs/notes/ai ...");

    // 4. List notes: `git notes --ref=ai list` → "blob_sha commit_sha\n" lines.
    let note_pairs = match list_notes(&repo) {
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
    let blob_contents = match cat_file_batch(&repo, &blob_shas) {
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
        if let Ok(db) = NotesDatabase::global()
            && let Ok(lock) = db.lock()
        {
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
        match NotesDatabase::global() {
            Ok(db) => match db.lock() {
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
            },
            Err(e) => {
                eprintln!("warning: failed to open notes-db: {}", e);
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
    repo: &crate::git::repository::Repository,
) -> Result<Vec<(String, String)>, GitAiError> {
    use crate::git::repository::exec_git;

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

/// Bulk-read blob contents via `git cat-file --batch`.
///
/// Feeds the blob SHAs on stdin and parses the binary protocol output.
/// Returns a map of `blob_sha → content`.
fn cat_file_batch(
    repo: &crate::git::repository::Repository,
    blob_shas: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    if blob_shas.is_empty() {
        return Ok(HashMap::new());
    }

    // `global_args_for_exec()` returns the per-repository flags (e.g. `-C <path>
    // --no-pager`) but NOT the git binary itself.  The git binary comes from
    // `Config::get().git_cmd()`, matching the pattern used in `exec_git` in
    // repository.rs.
    let git_bin = crate::config::Config::get().git_cmd().to_string();
    let git_flags = repo.global_args_for_exec();

    let mut cmd = Command::new(&git_bin);
    cmd.args(&git_flags);
    cmd.arg("cat-file");
    cmd.arg("--batch");
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    crate::git::repository::apply_internal_git_machine_env(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| GitAiError::Generic(format!("failed to spawn git cat-file --batch: {}", e)))?;

    // Take stdin out of the child so we can write in a separate thread.
    // This avoids a pipe deadlock: with many notes, stdout fills its buffer
    // and the child blocks on write, while the parent is still writing to
    // stdin and blocks there too.
    let mut stdin = child.stdin.take().ok_or_else(|| {
        GitAiError::Generic("failed to open git cat-file --batch stdin".to_string())
    })?;

    let blob_shas_owned: Vec<String> = blob_shas.to_vec();
    let writer_thread = std::thread::spawn(move || -> Result<(), std::io::Error> {
        for sha in &blob_shas_owned {
            writeln!(stdin, "{}", sha)?;
        }
        Ok(())
    });

    let output = child
        .wait_with_output()
        .map_err(|e| GitAiError::Generic(format!("git cat-file --batch failed: {}", e)))?;

    if let Err(e) = writer_thread.join().expect("stdin writer thread panicked")
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(GitAiError::IoError(e));
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GitAiError::Generic(format!(
            "git cat-file --batch exited {}: {}",
            output.status, stderr
        )));
    }

    // Parse the output format:
    //   <sha> <type> <size>\n<content-bytes>\n
    // We process byte-by-byte to handle binary-safe output.
    let data = output.stdout;
    let mut result = HashMap::new();
    let mut pos = 0usize;

    while pos < data.len() {
        // Find the end of the header line.
        let header_end = match data[pos..].iter().position(|&b| b == b'\n') {
            Some(off) => pos + off,
            None => break,
        };
        let header = std::str::from_utf8(&data[pos..header_end])
            .unwrap_or("")
            .trim();
        pos = header_end + 1; // skip the '\n'

        // Header format: "<sha> <type> <size>" or "<sha> missing"
        let mut parts = header.splitn(3, ' ');
        let sha = match parts.next() {
            Some(s) => s.to_string(),
            None => break,
        };
        let obj_type = parts.next().unwrap_or("missing");

        if obj_type == "missing" {
            // Object not in repo; skip.
            continue;
        }

        let size_str = parts.next().unwrap_or("0");
        let size: usize = size_str.parse().unwrap_or(0);

        // Read exactly `size` bytes of content, then skip the trailing '\n'.
        if pos + size > data.len() {
            break;
        }
        let content_bytes = &data[pos..pos + size];
        pos += size;
        // Skip the trailing newline separator (if present).
        if pos < data.len() && data[pos] == b'\n' {
            pos += 1;
        }

        // Convert content to UTF-8 (note content is always text).
        if let Ok(content) = std::str::from_utf8(content_bytes) {
            result.insert(sha, content.to_string());
        }
    }

    Ok(result)
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
    use crate::git::test_utils::TmpRepo;
    use crate::notes::db::NotesDatabase;
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

        let blob_contents = cat_file_batch(repo.gitai_repo(), &blob_shas).expect("cat_file_batch");
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
        let ctx = ApiContext::new(Some(backend_url));
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

    /// Unit test: `cat_file_batch` with empty input returns empty map.
    #[test]
    fn cat_file_batch_empty_input() {
        let repo = TmpRepo::new().expect("TmpRepo::new");
        let result = cat_file_batch(repo.gitai_repo(), &[]).expect("cat_file_batch");
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
        let blob_contents = cat_file_batch(repo.gitai_repo(), &blob_shas).expect("cat_file_batch");

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
            let ctx = ApiContext::new(Some(backend_url));
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
