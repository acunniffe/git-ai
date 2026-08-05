use crate::git::refs::{
    AI_AUTHORSHIP_PUSH_REFSPEC, copy_ref, fallback_merge_notes_ours, merge_notes_from_ref,
    ref_exists, tracking_ref_for_remote,
};
use crate::{
    error::GitAiError,
    git::{cli_parser::ParsedGitInvocation, repository::exec_git},
};

use super::repository::Repository;

#[cfg(windows)]
fn disabled_hooks_config() -> &'static str {
    "core.hooksPath=NUL"
}

#[cfg(not(windows))]
fn disabled_hooks_config() -> &'static str {
    "core.hooksPath=/dev/null"
}

/// Result of checking for authorship notes on a remote
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotesExistence {
    /// Notes were found and fetched from the remote
    Found,
    /// Confirmed that no notes exist on the remote
    NotFound,
}

pub fn fetch_remote_from_args(
    repository: &Repository,
    parsed_args: &ParsedGitInvocation,
) -> Result<String, GitAiError> {
    let remotes = repository.remotes().ok();
    let remote_names: Vec<String> = remotes
        .as_ref()
        .map(|r| {
            (0..r.len())
                .filter_map(|i| r.get(i).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // 2) Fetch authorship refs from the appropriate remote
    // Try to detect remote (named remote, URL, or local path) from args first
    let positional_remote = extract_repository_arg_from_args(&parsed_args.command_args);
    let specified_remote = positional_remote.or_else(|| {
        parsed_args
            .command_args
            .iter()
            .find(|a| remote_names.iter().any(|r| r == *a))
            .cloned()
    });

    let remote = specified_remote
        .or_else(|| repository.upstream_remote().ok().flatten())
        .or_else(|| repository.get_default_remote().ok().flatten());

    remote.map(|r| r.to_string()).ok_or_else(|| {
        GitAiError::Generic(
            "Could not determine a remote for fetch/push operation. \
                 No remote was specified in args, no upstream is configured, \
                 and no default remote was found."
                .to_string(),
        )
    })
}

pub fn push_remote_from_args(
    repository: &Repository,
    parsed_args: &ParsedGitInvocation,
) -> Result<String, GitAiError> {
    let remotes = repository.remotes().ok();
    let remote_names: Vec<String> = remotes
        .as_ref()
        .map(|r| {
            (0..r.len())
                .filter_map(|i| r.get(i).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let specified_remote =
        extract_repository_arg_from_args(&parsed_args.command_args).or_else(|| {
            parsed_args
                .command_args
                .iter()
                .find(|a| remote_names.iter().any(|r| r == *a))
                .cloned()
        });

    let remote = specified_remote
        .or_else(|| repository.upstream_remote().ok().flatten())
        .or_else(|| repository.get_default_remote().ok().flatten());

    remote.map(|r| r.to_string()).ok_or_else(|| {
        GitAiError::Generic(
            "Could not determine a remote for push operation. \
                 No remote was specified in args, no upstream is configured, \
                 and no default remote was found."
                .to_string(),
        )
    })
}

/// Try to fetch authorship notes from all remotes for source commits that are missing
/// local notes. Used before rewrite attribution so remote source notes are available
/// locally before we copy/shift them.
///
/// Uses the safe fetch pattern (tracking ref + merge with `-s ours`) so local notes
/// are never overwritten. If one or more fetches fail, keep trying the remaining
/// remotes; only return an error if the requested source notes are still missing
/// afterward.
pub fn fetch_missing_notes_for_commits(
    repository: &Repository,
    source_commits: &[String],
) -> Result<(), GitAiError> {
    use std::collections::HashSet;

    fn noted_commits(repository: &Repository, commit_shas: &[String]) -> HashSet<String> {
        // Backend-aware presence check: on the HTTP backend notes live in the
        // notes-db cache, not refs/notes/ai — without this, every rewrite of
        // cached-note commits triggered pointless notes fetches from all remotes.
        crate::git::notes_api::commits_with_notes(repository, commit_shas).unwrap_or_default()
    }

    let noted_before_fetch = noted_commits(repository, source_commits);

    let missing: Vec<&String> = source_commits
        .iter()
        .filter(|sha| !noted_before_fetch.contains(sha.as_str()))
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    // On the HTTP notes backend the server — not refs/notes/ai — is the
    // source of truth: pull the missing source notes straight from it into
    // the local cache first. The git refs fetch below requires working SSH
    // auth in the daemon's environment and only helps repos whose notes live
    // in refs; rewrites were stranding notes whenever that fetch failed
    // (e.g. `Permission denied (publickey)`) even though the backend held
    // every source note.
    if crate::config::Config::fresh().notes_backend_enabled() {
        let missing_owned: Vec<String> = missing.iter().map(|sha| sha.to_string()).collect();
        match crate::git::notes_api::warm_cache_for_commits(&missing_owned) {
            Ok(cached) => {
                let cached: HashSet<String> = cached.into_iter().collect();
                if missing_owned.iter().all(|sha| cached.contains(sha)) {
                    return Ok(());
                }
            }
            Err(e) => {
                tracing::debug!("HTTP backend fetch for missing source notes failed: {}", e);
            }
        }

        // On the HTTP backend the server is the sole source of truth:
        // refs/notes/ai is never pushed there, so the refs fetch below
        // cannot produce these notes by construction — it only costs a
        // full negotiation per remote (minutes on large repos) while
        // daemon op-processing blocks behind it. A note the server does
        // not have is missing, period.
        let still_missing: Vec<&String> = {
            let noted = noted_commits(repository, source_commits);
            source_commits
                .iter()
                .filter(|sha| !noted.contains(sha.as_str()))
                .collect()
        };
        if still_missing.is_empty() {
            return Ok(());
        }
        return Err(GitAiError::Generic(format!(
            "authorship notes for source commits {:?} are missing from the notes backend \
             (refs fetch skipped: refs/notes/ai is not populated on the http backend)",
            still_missing
        )));
    }

    tracing::debug!(
        "Source commits missing notes: {:?}, trying to fetch from remotes",
        missing
    );

    let mut first_fetch_error: Option<GitAiError> = None;
    if let Ok(remotes) = repository.remotes_with_urls() {
        for (remote_name, _) in remotes {
            tracing::debug!("Attempting safe notes fetch from remote {}", remote_name);
            match fetch_authorship_notes(repository, &remote_name) {
                Ok(_) => tracing::debug!("Fetched and merged notes from remote {}", remote_name),
                Err(e) => {
                    tracing::debug!("Notes fetch from remote {} failed: {}", remote_name, e);
                    if first_fetch_error.is_none() {
                        first_fetch_error = Some(e);
                    }
                }
            }
        }
    }

    if let Some(error) = first_fetch_error {
        let noted_after_fetch = noted_commits(repository, source_commits);
        let still_missing: Vec<&String> = source_commits
            .iter()
            .filter(|sha| !noted_after_fetch.contains(sha.as_str()))
            .collect();
        if !still_missing.is_empty() {
            return Err(GitAiError::Generic(format!(
                "failed to fetch authorship notes for source commits {:?}: {}",
                still_missing, error
            )));
        }
    }

    Ok(())
}

/// Best-effort variant of [`fetch_missing_notes_for_commits`] for rewrite
/// attribution. A failed fetch must not abort the note shift: sources whose
/// notes are available locally still deserve migration, and sources that stay
/// missing end up without a note either way — aborting only widens the loss
/// to every commit in the rewrite.
pub fn fetch_missing_notes_for_commits_best_effort(
    repository: &Repository,
    source_commits: &[String],
) {
    if let Err(error) = fetch_missing_notes_for_commits(repository, source_commits) {
        tracing::warn!(
            %error,
            "source-note fetch failed; shifting the notes that are available locally"
        );
    }
}

// for use with post-fetch and post-pull and post-clone hooks
// Returns Ok(NotesExistence::Found) if notes were found and fetched,
// Ok(NotesExistence::NotFound) if confirmed no notes exist on remote,
// Err(...) for actual errors (network, permissions, etc.)
pub fn fetch_authorship_notes(
    repository: &Repository,
    remote_name: &str,
) -> Result<NotesExistence, GitAiError> {
    // Generate tracking ref for this remote
    let tracking_ref = tracking_ref_for_remote(remote_name);

    tracing::info!(
        remote = %remote_name,
        backend = %"git_notes",
        tracking_ref = %tracking_ref,
        "fetching authorship notes"
    );
    tracing::debug!(
        "fetching authorship notes for remote '{}' to tracking ref '{}'",
        remote_name,
        tracking_ref
    );

    // Fetch notes to tracking ref with explicit refspec.
    // If the remote does not have refs/notes/ai yet, treat that as NotFound.
    let fetch_refspec = format!("+refs/notes/ai:{}", tracking_ref);

    // Build the internal authorship fetch with explicit flags and disabled hooks.
    // IMPORTANT: use repository.global_args_for_exec() to ensure -C flag is present for bare repos.
    let fetch_authorship = build_authorship_fetch_args(
        repository.global_args_for_exec(),
        remote_name,
        &fetch_refspec,
    );

    tracing::debug!("fetch command: {:?}", fetch_authorship);

    match exec_git(&fetch_authorship) {
        Ok(output) => {
            tracing::debug!(
                "fetch stdout: '{}'",
                String::from_utf8_lossy(&output.stdout)
            );
            tracing::debug!(
                "fetch stderr: '{}'",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(e) => {
            if is_missing_remote_notes_ref_error(&e) {
                tracing::debug!(
                    "no authorship notes found on remote '{}', nothing to sync",
                    remote_name
                );
                return Ok(NotesExistence::NotFound);
            }
            tracing::debug!("authorship fetch failed: {}", e);
            return Err(e);
        }
    }

    // After successful fetch, merge the tracking ref into refs/notes/ai
    let local_notes_ref = "refs/notes/ai";

    if crate::git::refs::ref_exists(repository, &tracking_ref) {
        if crate::git::refs::ref_exists(repository, local_notes_ref) {
            // Both exist - merge them
            tracing::debug!(
                "merging authorship notes from {} into {}",
                tracking_ref,
                local_notes_ref
            );
            if let Err(e) = merge_notes_from_ref(repository, &tracking_ref) {
                tracing::debug!("notes merge failed: {}", e);
                // Fallback: manually merge notes when git notes merge crashes
                if let Err(e2) = fallback_merge_notes_ours(repository, &tracking_ref) {
                    tracing::debug!("fallback merge also failed: {}", e2);
                    return Err(e2);
                }
            }
        } else {
            // Only tracking ref exists - copy it to local
            tracing::debug!(
                "initializing {} from tracking ref {}",
                local_notes_ref,
                tracking_ref
            );
            if let Err(e) = copy_ref(repository, &tracking_ref, local_notes_ref) {
                tracing::debug!("notes copy failed: {}", e);
                return Err(e);
            }
        }
    } else {
        tracing::debug!("tracking ref {} was not created after fetch", tracking_ref);
    }

    Ok(NotesExistence::Found)
}

fn is_missing_remote_notes_ref_error(error: &GitAiError) -> bool {
    let GitAiError::GitCliError { stderr, .. } = error else {
        return false;
    };

    let stderr_lower = stderr.to_ascii_lowercase();
    stderr_lower.contains("refs/notes/ai")
        && (stderr_lower.contains("couldn't find remote ref")
            || stderr_lower.contains("could not find remote ref")
            || stderr_lower.contains("remote ref does not exist")
            || stderr_lower.contains("not our ref"))
}
/// Maximum number of fetch-merge-push attempts before giving up.
/// On busy monorepos, concurrent pushers can cause non-fast-forward rejections
/// even after a successful merge, so we retry the full cycle.
const PUSH_NOTES_MAX_ATTEMPTS: usize = 3;

// for use with post-push hook
pub fn push_authorship_notes(repository: &Repository, remote_name: &str) -> Result<(), GitAiError> {
    // Belt-and-suspenders: when the HTTP backend is active, notes are not stored
    // in refs/notes/ai so there is nothing to push.
    if crate::config::Config::get().notes_backend_kind() == crate::config::NotesBackendKind::Http {
        tracing::debug!("push_authorship_notes: skipping refs/notes/ai push (Http backend active)");
        return Ok(());
    }

    let mut last_error = None;

    for attempt in 0..PUSH_NOTES_MAX_ATTEMPTS {
        if attempt > 0 {
            tracing::debug!(
                "retrying notes push (attempt {}/{})",
                attempt + 1,
                PUSH_NOTES_MAX_ATTEMPTS
            );
        }

        fetch_and_merge_tracking_notes(repository, remote_name);

        // Push notes without force (requires fast-forward)
        let push_args = build_authorship_push_args(repository.global_args_for_exec(), remote_name);

        tracing::debug!("pushing authorship refs (no force): {:?}", &push_args);

        match exec_git(&push_args) {
            Ok(_) => return Ok(()),
            Err(e) => {
                tracing::debug!("authorship push failed: {}", e);
                if is_non_fast_forward_error(&e) && attempt + 1 < PUSH_NOTES_MAX_ATTEMPTS {
                    // Another pusher updated remote notes between our merge and push.
                    // Retry the full fetch-merge-push cycle.
                    last_error = Some(e);
                    continue;
                }
                return Err(e);
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| GitAiError::Generic("notes push exhausted retries".to_string())))
}

/// Fetch remote notes into a tracking ref and merge into local refs/notes/ai.
fn fetch_and_merge_tracking_notes(repository: &Repository, remote_name: &str) {
    let tracking_ref = tracking_ref_for_remote(remote_name);
    let fetch_refspec = format!("+refs/notes/ai:{}", tracking_ref);

    let fetch_args = build_authorship_fetch_args(
        repository.global_args_for_exec(),
        remote_name,
        &fetch_refspec,
    );

    tracing::debug!("pre-push authorship fetch: {:?}", &fetch_args);

    // Fetch is best-effort; if it fails (e.g., no remote notes yet), continue
    if exec_git(&fetch_args).is_err() {
        return;
    }

    let local_notes_ref = "refs/notes/ai";

    if !ref_exists(repository, &tracking_ref) {
        return;
    }

    if !ref_exists(repository, local_notes_ref) {
        // Only tracking ref exists - copy it to local
        tracing::debug!(
            "pre-push: initializing {} from {}",
            local_notes_ref,
            tracking_ref
        );
        if let Err(e) = copy_ref(repository, &tracking_ref, local_notes_ref) {
            tracing::debug!("pre-push notes copy failed: {}", e);
        }
        return;
    }

    // Both exist - merge them
    tracing::debug!(
        "pre-push: merging {} into {}",
        tracking_ref,
        local_notes_ref
    );
    if let Err(e) = merge_notes_from_ref(repository, &tracking_ref) {
        tracing::debug!("pre-push notes merge failed: {}", e);
        // Fallback: manually merge notes when git notes merge crashes
        // (e.g., due to corrupted/mixed-fanout notes trees, or git bugs
        // with fanout-level mismatches on older git versions like macOS)
        if let Err(e2) = fallback_merge_notes_ours(repository, &tracking_ref) {
            tracing::debug!("pre-push fallback merge also failed: {}", e2);
        }
    }
}

fn is_non_fast_forward_error(error: &GitAiError) -> bool {
    let GitAiError::GitCliError { stderr, .. } = error else {
        return false;
    };
    stderr.contains("non-fast-forward")
}

fn extract_repository_arg_from_args(args: &[String]) -> Option<String> {
    let mut after_double_dash = false;

    for arg in args {
        if !after_double_dash {
            if arg == "--" {
                after_double_dash = true;
                continue;
            }
            if arg.starts_with('-') {
                // Option; skip
                continue;
            }
        }

        // Candidate positional arg; determine if it's a repository URL/path
        let s = arg.as_str();

        // 1) URL forms (https://, ssh://, file://, git://, etc.)
        if s.contains("://") || s.starts_with("file://") {
            return Some(arg.clone());
        }

        // 2) SCP-like syntax: user@host:path
        if s.contains('@') && s.contains(':') && !s.contains("://") {
            return Some(arg.clone());
        }

        // 3) Local path forms
        if s.starts_with('/') || s.starts_with("./") || s.starts_with("../") || s.starts_with("~/")
        {
            return Some(arg.clone());
        }

        // Heuristic: bare repo directories often end with .git
        if s.ends_with(".git") {
            return Some(arg.clone());
        }

        // 4) As a last resort, if the path exists on disk, treat as local path
        if std::path::Path::new(s).exists() {
            return Some(arg.clone());
        }

        // Otherwise, do not treat this positional token as a repository; likely a refspec
        break;
    }

    None
}

fn with_disabled_hooks(mut args: Vec<String>) -> Vec<String> {
    args.push("-c".to_string());
    args.push(disabled_hooks_config().to_string());
    args
}

fn build_authorship_fetch_args(
    global_args: Vec<String>,
    remote_name: &str,
    fetch_refspec: &str,
) -> Vec<String> {
    let mut args = with_disabled_hooks(global_args);
    args.push("fetch".to_string());
    args.push("--no-tags".to_string());
    args.push("--recurse-submodules=no".to_string());
    args.push("--no-write-fetch-head".to_string());
    args.push("--no-write-commit-graph".to_string());
    args.push("--no-auto-maintenance".to_string());
    args.push(remote_name.to_string());
    args.push(fetch_refspec.to_string());
    args
}

fn build_authorship_push_args(global_args: Vec<String>, remote_name: &str) -> Vec<String> {
    let mut args = with_disabled_hooks(global_args);
    args.push("push".to_string());
    args.push("--quiet".to_string());
    args.push("--no-recurse-submodules".to_string());
    args.push("--no-verify".to_string());
    args.push("--no-signed".to_string());
    args.push(remote_name.to_string());
    args.push(AI_AUTHORSHIP_PUSH_REFSPEC.to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_http_backend_env(db_path: &str, server_url: &str) {
        // Safety: test-only env var manipulation, serialized on notes_db_env.
        unsafe {
            std::env::set_var("GIT_AI_TEST_NOTES_DB_PATH", db_path);
            std::env::set_var("GIT_AI_NOTES_BACKEND_KIND", "http");
            std::env::set_var("GIT_AI_NOTES_BACKEND_URL", server_url);
            std::env::set_var("GIT_AI_API_KEY", "sync-authorship-test-key");
        }
    }

    fn clear_http_backend_env() {
        unsafe {
            std::env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
            std::env::remove_var("GIT_AI_NOTES_BACKEND_KIND");
            std::env::remove_var("GIT_AI_NOTES_BACKEND_URL");
            std::env::remove_var("GIT_AI_API_KEY");
        }
    }

    /// Regression: a rewrite's missing source notes must be fetched from the
    /// HTTP notes backend when the git refs fetch cannot succeed (e.g. the
    /// daemon's environment has no SSH auth to the remote). This path was
    /// previously refs-fetch-only, so rebases/restacks stranded authorship
    /// notes with `failed to fetch authorship notes for source commits`
    /// even though the backend held every source note.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn fetch_missing_notes_pulls_sources_from_http_backend_when_refs_fetch_fails() {
        use crate::git::test_utils::TmpRepo;
        use tempfile::NamedTempFile;

        let tmp_db = NamedTempFile::new().expect("tmp notes-db");

        let repo = TmpRepo::new().expect("TmpRepo::new");
        repo.write_file("src.txt", "content", false).expect("write");
        let sha = repo.commit_all("source commit").expect("commit");

        // A remote whose fetch always fails, standing in for broken SSH auth.
        repo.git_command(&["remote", "add", "origin", "/nonexistent/git-ai-test-remote"])
            .expect("remote add");

        // The backend holds the note the local cache and refs are missing.
        let mut server = mockito::Server::new();
        let notes_json =
            serde_json::json!({ "notes": { sha.clone(): "server-note-content" } }).to_string();
        let _mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/worker/notes/".to_string()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(&notes_json)
            .create();

        set_http_backend_env(tmp_db.path().to_str().unwrap(), &server.url());

        let result = fetch_missing_notes_for_commits(repo.gitai_repo(), std::slice::from_ref(&sha));

        // The note must now be in the local cache as an already-synced row.
        let cached = {
            let db = crate::notes::db::NotesDatabase::global().expect("global db");
            let lock = db.lock().expect("lock");
            lock.get_note(&sha).expect("get_note")
        };
        clear_http_backend_env();

        assert!(
            result.is_ok(),
            "fetch_missing_notes_for_commits should succeed via the HTTP backend: {:?}",
            result
        );
        assert_eq!(cached, Some("server-note-content".to_string()));
    }

    /// On the HTTP backend the server is the sole source of truth:
    /// refs/notes/ai is never pushed there, so the git refs fetch cannot
    /// succeed by construction and only costs a full negotiation per remote
    /// (minutes on large repos) while daemon op-processing blocks behind it.
    /// When the backend lacks a note, the refs fetch must be SKIPPED — even
    /// if a remote happens to carry a refs/notes/ai ref (non-HTTP history).
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn fetch_missing_notes_skips_refs_fetch_on_http_backend() {
        use crate::git::test_utils::TmpRepo;
        use tempfile::NamedTempFile;

        let tmp_db = NamedTempFile::new().expect("tmp notes-db");

        let repo = TmpRepo::new().expect("TmpRepo::new");
        repo.write_file("src.txt", "content", false).expect("write");
        let sha = repo.commit_all("source commit").expect("commit");

        // A REAL local origin that carries the note in refs/notes/ai: if the
        // refs fetch runs, it would succeed and find the note — which is how
        // this test detects the (unwanted) fall-through.
        let origin = tempfile::tempdir().expect("origin dir");
        let origin_path = origin.path().to_str().unwrap().to_string();
        std::process::Command::new("git")
            .args(["init", "--bare", &origin_path])
            .output()
            .expect("init bare");
        repo.git_command(&["remote", "add", "origin", &origin_path])
            .expect("remote add");
        repo.git_command(&["push", "-q", "origin", "HEAD:refs/heads/main"])
            .expect("push branch");
        repo.git_command(&["notes", "--ref=ai", "add", "-m", "{}", &sha])
            .expect("add note");
        repo.git_command(&["push", "-q", "origin", "refs/notes/ai:refs/notes/ai"])
            .expect("push notes ref");
        repo.git_command(&["update-ref", "-d", "refs/notes/ai"])
            .expect("drop local notes ref");

        // The HTTP backend does NOT have the note.
        let mut server = mockito::Server::new();
        let _mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/worker/notes/".to_string()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"notes": {}}"#)
            .create();

        set_http_backend_env(tmp_db.path().to_str().unwrap(), &server.url());
        let result = fetch_missing_notes_for_commits(repo.gitai_repo(), std::slice::from_ref(&sha));
        let tracking_ref_after = repo
            .git_command(&[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/notes/ai-remote/origin",
            ])
            .ok();
        clear_http_backend_env();

        assert!(
            tracking_ref_after.is_none() || tracking_ref_after.as_deref() == Some(""),
            "refs fetch must not run on the HTTP backend (tracking ref was created)"
        );
        let err =
            result.expect_err("missing on the backend must stay missing, not be refs-fetched");
        assert!(
            err.to_string().contains(&sha),
            "error should name the missing sha: {err}"
        );
    }

    /// The error contract is preserved: when neither the HTTP backend nor
    /// any remote can produce the missing source notes, the fetch still
    /// fails loudly with the missing SHAs.
    #[test]
    #[serial_test::serial(notes_db_env)]
    fn fetch_missing_notes_still_errors_when_backend_lacks_note() {
        use crate::git::test_utils::TmpRepo;
        use tempfile::NamedTempFile;

        let tmp_db = NamedTempFile::new().expect("tmp notes-db");

        let repo = TmpRepo::new().expect("TmpRepo::new");
        repo.write_file("src.txt", "missing content", false)
            .expect("write");
        let sha = repo.commit_all("missing source commit").expect("commit");
        repo.git_command(&["remote", "add", "origin", "/nonexistent/git-ai-test-remote"])
            .expect("remote add");

        let mut server = mockito::Server::new();
        let _mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/worker/notes/".to_string()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"notes": {}}"#)
            .create();

        set_http_backend_env(tmp_db.path().to_str().unwrap(), &server.url());
        let result = fetch_missing_notes_for_commits(repo.gitai_repo(), std::slice::from_ref(&sha));
        clear_http_backend_env();

        let err = result.expect_err("missing everywhere should still be an error");
        assert!(
            err.to_string().contains(&sha),
            "error should name the missing sha: {err}"
        );
    }

    #[test]
    fn authorship_fetch_args_always_disable_hooks() {
        let disabled_hooks = disabled_hooks_config();
        let args = build_authorship_fetch_args(
            vec!["-C".to_string(), "/tmp/repo".to_string()],
            "origin",
            "+refs/notes/ai:refs/notes/ai-remote/origin",
        );

        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "-c" && pair[1] == disabled_hooks)
        );
        assert!(args.contains(&"fetch".to_string()));
    }

    #[test]
    fn authorship_push_args_always_disable_hooks() {
        let disabled_hooks = disabled_hooks_config();
        let args =
            build_authorship_push_args(vec!["-C".to_string(), "/tmp/repo".to_string()], "origin");

        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "-c" && pair[1] == disabled_hooks)
        );
        assert!(args.contains(&"push".to_string()));
    }

    #[test]
    fn repository_arg_extractor_recognizes_explicit_paths_and_urls() {
        assert_eq!(
            extract_repository_arg_from_args(&[
                "/tmp/remote.git".to_string(),
                "HEAD:refs/heads/main".to_string()
            ]),
            Some("/tmp/remote.git".to_string())
        );
        assert_eq!(
            extract_repository_arg_from_args(&[
                "https://example.com/repo.git".to_string(),
                "main".to_string()
            ]),
            Some("https://example.com/repo.git".to_string())
        );
        assert_eq!(
            extract_repository_arg_from_args(&["HEAD:refs/heads/main".to_string()]),
            None
        );
    }

    #[test]
    fn missing_remote_notes_ref_error_is_detected() {
        let err = GitAiError::GitCliError {
            code: Some(128),
            stderr: "fatal: couldn't find remote ref refs/notes/ai".to_string(),
            args: vec!["fetch".to_string(), "origin".to_string()],
        };
        assert!(is_missing_remote_notes_ref_error(&err));
    }

    #[test]
    fn missing_remote_notes_ref_error_ignores_unrelated_git_errors() {
        let err = GitAiError::GitCliError {
            code: Some(128),
            stderr: "fatal: Authentication failed for 'https://github.com/org/repo.git/'"
                .to_string(),
            args: vec!["fetch".to_string(), "origin".to_string()],
        };
        assert!(!is_missing_remote_notes_ref_error(&err));
    }
}
