use crate::clients::git_cli::{exec_git, exec_git_allow_nonzero, exec_git_stdin};
use crate::error::GitAiError;
use crate::model::authorship_log_serialization::{AUTHORSHIP_LOG_VERSION, AuthorshipLog};
use crate::model::working_log::Checkpoint;
use crate::operations::git::cat_file::batch_read_blob_contents;
use crate::operations::git::oid::is_full_oid;
use crate::operations::git::repository::Repository;
use serde_json;
use std::collections::{HashMap, HashSet};

// Modern refspecs without force to enable proper merging
pub const AI_AUTHORSHIP_REFNAME: &str = "ai";
pub const AI_AUTHORSHIP_FULL_REF: &str = "refs/notes/ai";
pub const AI_AUTHORSHIP_FORK_TRACKING_REF: &str = "refs/notes/ai-remote/fork";
pub const AI_AUTHORSHIP_PUSH_REFSPEC: &str = "refs/notes/ai:refs/notes/ai";

fn parse_output_error(what: &str) -> GitAiError {
    GitAiError::Generic(format!("Failed to parse {} output", what))
}

fn unexpected_note_object_type(entry: &LsTreeNoteEntry, notes_ref: &str) -> GitAiError {
    GitAiError::Generic(format!(
        "authorship note path {} in {} is {}, expected blob",
        entry.path, notes_ref, entry.object_type
    ))
}

fn malformed_ls_tree_output_error(missing: &str) -> GitAiError {
    GitAiError::Generic(format!("Malformed ls-tree output: missing {}", missing))
}

pub(in crate::operations::git) fn notes_add(
    repo: &Repository,
    commit_sha: &str,
    note_content: &str,
) -> Result<(), GitAiError> {
    // Route through notes_add_batch to ensure consistent fanout tree format.
    // Using git's native `notes add` can produce flat entries for small trees,
    // leading to mixed-fanout trees that trigger assertion failures in
    // `git notes merge` (notes-merge.c diff_tree_remote).
    notes_add_batch(repo, &[(commit_sha.to_string(), note_content.to_string())])
}

#[doc(hidden)]
pub fn notes_path_for_object(oid: &str) -> String {
    if oid.len() <= 2 {
        oid.to_string()
    } else {
        format!("{}/{}", &oid[..2], &oid[2..])
    }
}

#[doc(hidden)]
pub fn flat_note_pathspec_for_commit(commit_sha: &str) -> String {
    flat_note_pathspec_for_ref(AI_AUTHORSHIP_FULL_REF, commit_sha)
}

#[doc(hidden)]
pub fn fanout_note_pathspec_for_commit(commit_sha: &str) -> String {
    fanout_note_pathspec_for_ref(AI_AUTHORSHIP_FULL_REF, commit_sha)
}

#[doc(hidden)]
pub fn flat_note_pathspec_for_ref(notes_ref: &str, commit_sha: &str) -> String {
    format!("{}:{}", notes_ref, commit_sha)
}

#[doc(hidden)]
pub fn fanout_note_pathspec_for_ref(notes_ref: &str, commit_sha: &str) -> String {
    format!("{}:{}", notes_ref, notes_path_for_object(commit_sha))
}

#[doc(hidden)]
pub fn parse_batch_check_blob_oid(line: &str) -> Option<String> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let oid = parts.first().copied().unwrap_or_default();
    if parts.len() >= 2 && parts[1] == "blob" && is_full_oid(oid) {
        Some(oid.to_string())
    } else {
        None
    }
}

/// Resolve authorship note blob OIDs for a set of commits using one batched cat-file call.
///
/// Returns a map of commit SHA -> note blob SHA for commits that currently have notes.
pub(in crate::operations::git) fn note_blob_oids_for_commits(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    note_blob_oids_for_commits_from_ref(repo, AI_AUTHORSHIP_FULL_REF, commit_shas)
}

/// Read authorship note contents for a set of commits in batch.
///
/// Returns a map of commit SHA -> raw note content for commits that currently
/// have notes in `refs/notes/ai`.
pub(in crate::operations::git) fn notes_for_commits(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    if commit_shas.is_empty() {
        return Ok(HashMap::new());
    }

    let note_blob_oids = note_blob_oids_for_commits(repo, commit_shas)?;
    if note_blob_oids.is_empty() {
        return Ok(HashMap::new());
    }

    let unique_blob_oids: Vec<String> = note_blob_oids
        .values()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let blob_contents = batch_read_blob_contents(repo, &unique_blob_oids)?;

    Ok(note_blob_oids
        .into_iter()
        .filter_map(|(commit_sha, blob_oid)| {
            blob_contents
                .get(&blob_oid)
                .map(|content| (commit_sha, content.clone()))
        })
        .collect())
}

/// Resolve authorship note blob OIDs for a set of commits from a specific notes ref.
///
/// Returns a map of commit SHA -> note blob SHA for commits that have notes on
/// `notes_ref`. The destination `refs/notes/ai` is not consulted.
pub fn note_blob_oids_for_commits_from_ref(
    repo: &Repository,
    notes_ref: &str,
    commit_shas: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    if commit_shas.is_empty() {
        return Ok(HashMap::new());
    }

    let mut path_to_commit = HashMap::new();
    let mut fanout_prefixes = HashSet::new();
    for commit_sha in commit_shas {
        let flat_path = commit_sha.clone();
        path_to_commit.insert(flat_path, commit_sha.clone());

        let fanout_path = notes_path_for_object(commit_sha);
        path_to_commit.insert(fanout_path, commit_sha.clone());
        if commit_sha.len() > 2 {
            fanout_prefixes.insert(commit_sha[..2].to_string());
        }
    }

    let mut result = HashMap::new();
    let Some(root_entries) = ls_tree_note_entries(repo, notes_ref, false, &[])? else {
        return Ok(HashMap::new());
    };

    for entry in root_entries {
        if let Some(commit_sha) = path_to_commit.get(&entry.path) {
            if entry.object_type != "blob" {
                return Err(unexpected_note_object_type(&entry, notes_ref));
            }
            result.entry(commit_sha.clone()).or_insert(entry.oid);
        }
    }

    let mut prefixes = fanout_prefixes.into_iter().collect::<Vec<_>>();
    prefixes.sort();
    if let Some(entries) = ls_tree_note_entries(repo, notes_ref, true, &prefixes)? {
        for entry in entries {
            if let Some(commit_sha) = path_to_commit.get(&entry.path) {
                if entry.object_type != "blob" {
                    return Err(unexpected_note_object_type(&entry, notes_ref));
                }
                result.entry(commit_sha.clone()).or_insert(entry.oid);
            }
        }
    }

    Ok(result)
}

#[derive(Debug)]
struct LsTreeNoteEntry {
    object_type: String,
    oid: String,
    path: String,
}

fn ls_tree_note_entries(
    repo: &Repository,
    notes_ref: &str,
    recursive: bool,
    pathspecs: &[String],
) -> Result<Option<Vec<LsTreeNoteEntry>>, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.push("ls-tree".to_string());
    args.push("-z".to_string());
    args.push("--full-tree".to_string());
    if recursive {
        args.push("-r".to_string());
    }
    args.push(notes_ref.to_string());
    if !pathspecs.is_empty() {
        args.push("--".to_string());
        args.extend(pathspecs.iter().cloned());
    }

    let output = match exec_git(&args) {
        Ok(output) => output,
        Err(GitAiError::GitCliError {
            code: Some(128),
            stderr,
            ..
        }) if stderr.contains("Not a valid object name") && stderr.contains(notes_ref) => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };

    parse_ls_tree_note_entries(&output.stdout).map(Some)
}

fn parse_ls_tree_note_entries(data: &[u8]) -> Result<Vec<LsTreeNoteEntry>, GitAiError> {
    let mut entries = Vec::new();
    for raw in data.split(|byte| *byte == 0).filter(|raw| !raw.is_empty()) {
        let Some(tab_idx) = raw.iter().position(|byte| *byte == b'\t') else {
            return Err(malformed_ls_tree_output_error("path separator"));
        };
        let meta = std::str::from_utf8(&raw[..tab_idx])?;
        let path = std::str::from_utf8(&raw[tab_idx + 1..])?.to_string();
        let mut parts = meta.split_whitespace();
        let Some(_mode) = parts.next() else {
            return Err(malformed_ls_tree_output_error("mode"));
        };
        let Some(object_type) = parts.next() else {
            return Err(malformed_ls_tree_output_error("object type"));
        };
        let Some(oid) = parts.next() else {
            return Err(malformed_ls_tree_output_error("object id"));
        };
        entries.push(LsTreeNoteEntry {
            object_type: object_type.to_string(),
            oid: oid.to_string(),
            path,
        });
    }
    Ok(entries)
}

/// Copy missing notes for a bounded commit set from `source_ref` into `refs/notes/ai`.
///
/// This deliberately does not merge `source_ref` wholesale. The source ref may
/// contain untrusted notes from a fork, so callers must pass the exact commits
/// whose notes are allowed to enter the local authorship ref. Existing local
/// notes win on conflicts, matching the `git notes merge -s ours` behavior used
/// for trusted tracking refs.
pub fn copy_missing_notes_for_commits_from_ref(
    repo: &Repository,
    source_ref: &str,
    commit_shas: &[String],
) -> Result<usize, GitAiError> {
    if commit_shas.is_empty() || !ref_exists(repo, source_ref) {
        return Ok(0);
    }

    let source_note_oids = note_blob_oids_for_commits_from_ref(repo, source_ref, commit_shas)?;
    if source_note_oids.is_empty() {
        return Ok(0);
    }

    let local_note_oids = note_blob_oids_for_commits(repo, commit_shas)?;
    let entries: Vec<(String, String)> = commit_shas
        .iter()
        .filter(|commit_sha| !local_note_oids.contains_key(*commit_sha))
        .filter_map(|commit_sha| {
            source_note_oids
                .get(commit_sha)
                .map(|blob_oid| (commit_sha.clone(), blob_oid.clone()))
        })
        .collect();

    let copied = entries.len();
    notes_add_blob_batch(repo, &entries)?;
    Ok(copied)
}

/// Shared prologue for `notes_add_batch`/`notes_add_blob_batch`: resolve the
/// current `refs/notes/ai` tip (if any), collapse `entries` to one
/// `(commit_sha, value)` pair per commit (last write wins, original relative
/// order preserved), and stamp the fast-import commit time.
///
/// Returns `None` when `entries` is empty — callers should return `Ok(())`.
/// `(existing_notes_tip, deduped_entries, commit_timestamp)` from
/// [`prepare_notes_batch_write`]; `None` when there is nothing to write.
type PreparedNotesBatch = Option<(Option<String>, Vec<(String, String)>, u64)>;

fn prepare_notes_batch_write(
    repo: &Repository,
    entries: &[(String, String)],
) -> Result<PreparedNotesBatch, GitAiError> {
    if entries.is_empty() {
        return Ok(None);
    }

    let mut args = repo.global_args_for_exec();
    args.push("rev-parse".to_string());
    args.push("--verify".to_string());
    args.push("refs/notes/ai".to_string());
    let existing_notes_tip = match exec_git(&args) {
        Ok(output) => Some(String::from_utf8(output.stdout)?.trim().to_string()),
        Err(GitAiError::GitCliError {
            code: Some(128), ..
        })
        | Err(GitAiError::GitCliError { code: Some(1), .. }) => None,
        Err(e) => return Err(e),
    };

    let mut deduped_entries: Vec<(String, String)> = Vec::new();
    let mut seen = HashSet::new();
    for (commit_sha, value) in entries.iter().rev() {
        if seen.insert(commit_sha.as_str()) {
            deduped_entries.push((commit_sha.clone(), value.clone()));
        }
    }
    deduped_entries.reverse();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| GitAiError::Generic(format!("System clock before epoch: {}", e)))?
        .as_secs();

    Ok(Some((existing_notes_tip, deduped_entries, now)))
}

pub(in crate::operations::git) fn notes_add_batch(
    repo: &Repository,
    entries: &[(String, String)],
) -> Result<(), GitAiError> {
    let Some((existing_notes_tip, deduped_entries, now)) =
        prepare_notes_batch_write(repo, entries)?
    else {
        return Ok(());
    };

    let mut script = Vec::<u8>::new();

    for (idx, (_commit_sha, note_content)) in deduped_entries.iter().enumerate() {
        script.extend_from_slice(b"blob\n");
        script.extend_from_slice(format!("mark :{}\n", idx + 1).as_bytes());
        script.extend_from_slice(format!("data {}\n", note_content.len()).as_bytes());
        script.extend_from_slice(note_content.as_bytes());
        script.extend_from_slice(b"\n");
    }

    script.extend_from_slice(b"commit refs/notes/ai\n");
    script.extend_from_slice(format!("committer git-ai <git-ai@local> {} +0000\n", now).as_bytes());
    script.extend_from_slice(b"data 0\n");
    if let Some(existing_tip) = existing_notes_tip {
        script.extend_from_slice(format!("from {}\n", existing_tip).as_bytes());
    }

    for (idx, (commit_sha, _note_content)) in deduped_entries.iter().enumerate() {
        let fanout_path = notes_path_for_object(commit_sha);
        let flat_path = commit_sha.clone();
        if flat_path != fanout_path {
            script.extend_from_slice(format!("D {}\n", flat_path).as_bytes());
        }
        script.extend_from_slice(format!("D {}\n", fanout_path).as_bytes());
        script.extend_from_slice(format!("M 100644 :{} {}\n", idx + 1, fanout_path).as_bytes());
    }
    script.extend_from_slice(b"\n");

    let mut fast_import_args = repo.global_args_for_exec();
    fast_import_args.push("fast-import".to_string());
    fast_import_args.push("--quiet".to_string());
    exec_git_stdin(&fast_import_args, &script)?;
    crate::operations::authorship::git_ai_hooks::post_notes_updated(repo, &deduped_entries);

    Ok(())
}

/// Batch-attach existing note blobs to commits without rewriting blob contents.
///
/// Each entry is (commit_sha, existing_note_blob_oid).
pub(in crate::operations::git) fn notes_add_blob_batch(
    repo: &Repository,
    entries: &[(String, String)],
) -> Result<(), GitAiError> {
    let Some((existing_notes_tip, deduped_entries, now)) =
        prepare_notes_batch_write(repo, entries)?
    else {
        return Ok(());
    };

    let mut script = Vec::<u8>::new();
    script.extend_from_slice(b"commit refs/notes/ai\n");
    script.extend_from_slice(format!("committer git-ai <git-ai@local> {} +0000\n", now).as_bytes());
    script.extend_from_slice(b"data 0\n");
    if let Some(existing_tip) = existing_notes_tip {
        script.extend_from_slice(format!("from {}\n", existing_tip).as_bytes());
    }

    for (commit_sha, blob_oid) in &deduped_entries {
        let fanout_path = notes_path_for_object(commit_sha);
        let flat_path = commit_sha.clone();
        if flat_path != fanout_path {
            script.extend_from_slice(format!("D {}\n", flat_path).as_bytes());
        }
        script.extend_from_slice(format!("D {}\n", fanout_path).as_bytes());
        script.extend_from_slice(format!("M 100644 {} {}\n", blob_oid, fanout_path).as_bytes());
    }
    script.extend_from_slice(b"\n");

    let mut fast_import_args = repo.global_args_for_exec();
    fast_import_args.push("fast-import".to_string());
    fast_import_args.push("--quiet".to_string());
    exec_git_stdin(&fast_import_args, &script)?;

    let has_post_notes_updated_hooks = crate::config::Config::get()
        .git_ai_hook_commands("post_notes_updated")
        .is_some_and(|commands| !commands.is_empty());
    if has_post_notes_updated_hooks {
        let hook_entries = (|| -> Result<Vec<(String, String)>, GitAiError> {
            let mut unique_blob_oids: Vec<String> = deduped_entries
                .iter()
                .map(|(_commit_sha, blob_oid)| blob_oid.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            unique_blob_oids.sort();
            let blob_contents = batch_read_blob_contents(repo, &unique_blob_oids)?;

            Ok(deduped_entries
                .iter()
                .filter_map(|(commit_sha, blob_oid)| {
                    blob_contents
                        .get(blob_oid)
                        .map(|note_content| (commit_sha.clone(), note_content.clone()))
                })
                .collect())
        })();
        match hook_entries {
            Ok(entries) if !entries.is_empty() => {
                crate::operations::authorship::git_ai_hooks::post_notes_updated(repo, &entries)
            }
            Ok(_) => {}
            Err(e) => tracing::debug!(
                "Failed to prepare post_notes_updated payload for notes_add_blob_batch: {}",
                e
            ),
        }
    }

    Ok(())
}

// Check which commits from the given list have authorship notes.
// Uses git cat-file --batch-check to efficiently check multiple commits in one invocation.
// Returns a Vec of CommitAuthorship for each commit.
#[derive(Debug, Clone)]

pub enum CommitAuthorship {
    NoLog {
        sha: String,
        git_author: String,
    },
    Log {
        sha: String,
        git_author: String,
        authorship_log: AuthorshipLog,
    },
}
pub(in crate::operations::git) fn get_commits_with_notes_from_list(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<Vec<CommitAuthorship>, GitAiError> {
    if commit_shas.is_empty() {
        return Ok(Vec::new());
    }

    // Get the git authors for all commits using git rev-list
    // This approach works in both bare and normal repositories
    let mut args = repo.global_args_for_exec();
    args.push("rev-list".to_string());
    args.push("--no-walk".to_string());
    args.push("--pretty=format:%H%n%an%n%ae".to_string());
    for sha in commit_shas {
        args.push(sha.clone());
    }

    let output = exec_git(&args)?;
    let stdout =
        String::from_utf8(output.stdout).map_err(|_| parse_output_error("git rev-list"))?;

    let mut commit_authors = HashMap::new();
    let lines: Vec<&str> = stdout.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // Skip commit headers (start with "commit ")
        if line.starts_with("commit ") {
            i += 1;
            if i + 2 < lines.len() {
                let sha = lines[i].to_string();
                let name = lines[i + 1].to_string();
                let email = lines[i + 2].to_string();
                let author = format!("{} <{}>", name, email);
                commit_authors.insert(sha, author);
                i += 3;
            } else {
                break;
            }
        } else {
            i += 1;
        }
    }

    let note_blob_oids = note_blob_oids_for_commits(repo, commit_shas)?;
    let mut unique_blob_oids = Vec::new();
    let mut seen_blob_oids = HashSet::new();
    for blob_oid in note_blob_oids.values() {
        if seen_blob_oids.insert(blob_oid.clone()) {
            unique_blob_oids.push(blob_oid.clone());
        }
    }
    let note_blob_contents = batch_read_blob_contents(repo, &unique_blob_oids)?;

    // Build the result Vec
    let mut result = Vec::new();
    for sha in commit_shas {
        let git_author = commit_authors
            .get(sha)
            .cloned()
            .unwrap_or_else(|| "Unknown".to_string());

        if let Some(blob_oid) = note_blob_oids.get(sha)
            && let Some(content) = note_blob_contents.get(blob_oid)
            && let Ok(mut authorship_log) = AuthorshipLog::deserialize_from_string(content)
        {
            authorship_log.metadata.base_commit_sha = sha.clone();
            result.push(CommitAuthorship::Log {
                sha: sha.clone(),
                git_author,
                authorship_log,
            });
        } else {
            result.push(CommitAuthorship::NoLog {
                sha: sha.clone(),
                git_author,
            });
        }
    }

    Ok(result)
}

// Show an authorship note and return its JSON content if found, or None if it doesn't exist.
pub(in crate::operations::git) fn show_authorship_note(
    repo: &Repository,
    commit_sha: &str,
) -> Option<String> {
    let mut args = repo.global_args_for_exec();
    args.push("notes".to_string());
    args.push("--ref=ai".to_string());
    args.push("show".to_string());
    args.push(commit_sha.to_string());

    match exec_git(&args) {
        Ok(output) => String::from_utf8(output.stdout)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        Err(GitAiError::GitCliError { code: Some(1), .. }) => None,
        Err(_) => None,
    }
}

/// Return the subset of `commit_shas` that currently has an authorship note.
///
/// This uses a single `git notes --ref=ai list` invocation instead of one
/// `git notes show` call per commit.
pub(in crate::operations::git) fn commits_with_authorship_notes(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<HashSet<String>, GitAiError> {
    Ok(note_blob_oids_for_commits(repo, commit_shas)?
        .into_keys()
        .collect())
}

// Show an authorship note and return its JSON content if found, or None if it doesn't exist.
pub(in crate::operations::git) fn get_authorship(
    repo: &Repository,
    commit_sha: &str,
) -> Option<AuthorshipLog> {
    let content = show_authorship_note(repo, commit_sha)?;
    let mut authorship_log = AuthorshipLog::deserialize_from_string(&content).ok()?;
    // Keep metadata aligned with the commit where this note is attached.
    authorship_log.metadata.base_commit_sha = commit_sha.to_string();
    Some(authorship_log)
}

#[allow(dead_code)]
pub fn get_reference_as_working_log(
    repo: &Repository,
    commit_sha: &str,
) -> Result<Vec<Checkpoint>, GitAiError> {
    let content = show_authorship_note(repo, commit_sha)
        .ok_or_else(|| GitAiError::Generic("No authorship note found".to_string()))?;
    let working_log = serde_json::from_str(&content)?;
    Ok(working_log)
}

pub(in crate::operations::git) fn get_reference_as_authorship_log_v3(
    repo: &Repository,
    commit_sha: &str,
) -> Result<AuthorshipLog, GitAiError> {
    let content = show_authorship_note(repo, commit_sha)
        .ok_or_else(|| GitAiError::Generic("No authorship note found".to_string()))?;

    // Try to deserialize as AuthorshipLog
    let mut authorship_log = match AuthorshipLog::deserialize_from_string(&content) {
        Ok(log) => log,
        Err(_) => {
            return Err(GitAiError::Generic(
                "Failed to parse authorship log".to_string(),
            ));
        }
    };

    // Check version compatibility
    if authorship_log.metadata.schema_version != AUTHORSHIP_LOG_VERSION {
        return Err(GitAiError::Generic(format!(
            "Unsupported authorship log version: {} (expected: {})",
            authorship_log.metadata.schema_version, AUTHORSHIP_LOG_VERSION
        )));
    }

    // Keep metadata aligned with the commit where this note is attached.
    authorship_log.metadata.base_commit_sha = commit_sha.to_string();

    Ok(authorship_log)
}

/// Sanitize a remote name to create a safe ref name
/// Replaces special characters with underscores to ensure valid ref names
#[doc(hidden)]
pub fn sanitize_remote_name(remote: &str) -> String {
    remote
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Generate a tracking ref name for notes from a specific remote
/// Returns a ref like "refs/notes/ai-remote/origin"
///
/// SAFETY: These tracking refs are stored under refs/notes/ai-remote/* which:
/// - Won't be pushed by `git push` (only pushes refs/heads/* by default)
/// - Won't be pushed by `git push --all` (only pushes refs/heads/*)
/// - Won't be pushed by `git push --tags` (only pushes refs/tags/*)
/// - **WILL** be pushed by `git push --mirror` (usually only used for backups, etc.)
/// - **WILL** be pushed if user explicitly specifies refs/notes/ai-remote/* (extremely rare)
pub fn tracking_ref_for_remote(remote_name: &str) -> String {
    format!("refs/notes/ai-remote/{}", sanitize_remote_name(remote_name))
}

/// Check if a ref exists in the repository
pub fn ref_exists(repo: &Repository, ref_name: &str) -> bool {
    let mut args = repo.global_args_for_exec();
    args.push("show-ref".to_string());
    args.push("--verify".to_string());
    args.push("--quiet".to_string());
    args.push(ref_name.to_string());

    exec_git(&args).is_ok()
}

/// Merge notes from a source ref into refs/notes/ai
/// Uses the 'ours' strategy to combine notes without data loss
pub fn merge_notes_from_ref(repo: &Repository, source_ref: &str) -> Result<(), GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.push("notes".to_string());
    args.push(format!("--ref={}", AI_AUTHORSHIP_REFNAME));
    args.push("merge".to_string());
    args.push("-s".to_string());
    args.push("ours".to_string());
    args.push("--quiet".to_string());
    args.push(source_ref.to_string());

    tracing::debug!("Merging notes from {} into refs/notes/ai", source_ref);
    exec_git(&args)?;
    Ok(())
}

/// Fallback merge when `git notes merge -s ours` fails (e.g., due to git assertion
/// failures on corrupted/mixed-fanout notes trees). Implements the "ours" strategy
/// using a single `git fast-import` invocation that:
///   1. Creates a merge commit with both local and source as parents
///   2. Emits all notes via `N <blob> <object>` commands (source first, then local —
///      last writer wins, so local takes precedence on conflicts = "ours" strategy)
///   3. Produces a clean notes tree with correct fanout regardless of input tree format
///
/// This is O(1) git process invocations regardless of note count, which matters on
/// large monorepos with thousands of notes.
pub fn fallback_merge_notes_ours(repo: &Repository, source_ref: &str) -> Result<(), GitAiError> {
    let local_ref = format!("refs/notes/{}", AI_AUTHORSHIP_REFNAME);

    // 1. List notes from both refs
    let source_notes = list_all_notes(repo, source_ref)?;
    let local_notes = list_all_notes(repo, &local_ref)?;

    // 2. Resolve parent commit SHAs for the merge commit
    let local_commit = rev_parse(repo, &local_ref)?;
    let source_commit = rev_parse(repo, source_ref)?;

    // Nothing to merge if both refs point to the same commit.
    if local_commit == source_commit {
        tracing::debug!("notes refs already at same commit, nothing to merge");
        return Ok(());
    }

    // 3. Build the fast-import stream.
    //    Use explicit `M` (filemodify) commands instead of `N` (notemodify) because
    //    `N` validates that the annotated object exists locally, which fails when
    //    merging notes from a remote that annotates commits not yet fetched to this
    //    repo (e.g., notes from another developer's push on a monorepo).
    //
    //    Emit source (remote) notes first, then local notes. fast-import uses
    //    last-writer-wins for duplicate paths, so local notes take precedence —
    //    this implements the "ours" merge strategy.
    let mut stream = String::new();
    stream.push_str(&format!("commit {}\n", local_ref));
    stream.push_str("committer git-ai <git-ai@noreply> 0 +0000\n");
    stream.push_str("data 23\nMerge notes (fallback)\n");
    stream.push_str(&format!("from {}\n", local_commit));
    stream.push_str(&format!("merge {}\n", source_commit));
    // Start with a clean tree to avoid mixed-fanout issues
    stream.push_str("deleteall\n");

    // Source notes first (will be overwritten by local on conflict)
    for (blob, object) in &source_notes {
        let path = notes_path_for_object(object);
        stream.push_str(&format!("M 100644 {} {}\n", blob, path));
    }
    // Local notes second (wins on conflict)
    for (blob, object) in &local_notes {
        let path = notes_path_for_object(object);
        stream.push_str(&format!("M 100644 {} {}\n", blob, path));
    }
    stream.push_str("done\n");

    // 4. Run fast-import
    let mut args = repo.global_args_for_exec();
    args.extend_from_slice(&[
        "fast-import".to_string(),
        "--quiet".to_string(),
        "--done".to_string(),
    ]);
    exec_git_stdin(&args, stream.as_bytes())?;

    tracing::debug!("fallback merge via fast-import completed successfully");
    Ok(())
}

/// List all notes on a given ref. Returns Vec<(note_blob_sha, annotated_object_sha)>.
fn list_all_notes(repo: &Repository, notes_ref: &str) -> Result<Vec<(String, String)>, GitAiError> {
    // `git notes list` uses --ref to specify which notes ref.
    // The --ref option prepends "refs/notes/" automatically, so for full refs
    // like "refs/notes/ai-remote/origin" we need to strip the prefix.
    let ref_arg = notes_ref.strip_prefix("refs/notes/").unwrap_or(notes_ref);

    let mut args = repo.global_args_for_exec();
    args.extend_from_slice(&[
        "notes".to_string(),
        format!("--ref={}", ref_arg),
        "list".to_string(),
    ]);

    let output = exec_git(&args)?;
    let stdout = String::from_utf8(output.stdout).map_err(|_| parse_output_error("notes list"))?;

    Ok(stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() == 2 {
                Some((parts[0].to_string(), parts[1].to_string()))
            } else {
                None
            }
        })
        .collect())
}

/// Parse a revision to its SHA
fn rev_parse(repo: &Repository, rev: &str) -> Result<String, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend_from_slice(&["rev-parse".to_string(), rev.to_string()]);
    let output = exec_git(&args)?;
    String::from_utf8(output.stdout)
        .map_err(|_| parse_output_error("rev-parse"))
        .map(|s| s.trim().to_string())
}

/// Copy a ref to another location (used for initial setup of local notes from tracking ref)
pub fn copy_ref(repo: &Repository, source_ref: &str, dest_ref: &str) -> Result<(), GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.push("update-ref".to_string());
    args.push(dest_ref.to_string());
    args.push(source_ref.to_string());

    tracing::debug!("Copying ref {} to {}", source_ref, dest_ref);
    exec_git(&args)?;
    Ok(())
}

/// Search AI notes for a pattern and return matching commit SHAs ordered by commit date (newest first)
/// Uses git grep to search through refs/notes/ai
pub(in crate::operations::git) fn grep_ai_notes(
    repo: &Repository,
    pattern: &str,
) -> Result<Vec<String>, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.push("--no-pager".to_string());
    args.push("grep".to_string());
    args.push("-nI".to_string());
    args.push(pattern.to_string());
    args.push("refs/notes/ai".to_string());

    let output = exec_git(&args)?;
    let stdout = String::from_utf8(output.stdout).map_err(|_| parse_output_error("git grep"))?;

    // Parse output format: refs/notes/ai:ab/cdef123...:line_number:matched_content
    // Extract the commit SHA from the path
    let mut shas = HashSet::new();
    for line in stdout.lines() {
        if let Some(path_and_rest) = line.strip_prefix("refs/notes/ai:")
            && let Some(path_end) = path_and_rest.find(':')
        {
            let path = &path_and_rest[..path_end];
            // Path is in format "ab/cdef123..." - combine to get full SHA
            let sha = path.replace('/', "");
            shas.insert(sha);
        }
    }

    // If we have multiple results, sort by commit date (newest first)
    sort_commit_shas_by_date_desc(repo, shas)
}

/// Sort commit SHAs by commit date, newest first, using a single `git log
/// --no-walk` call. Best-effort: if git cannot resolve every SHA (e.g. a
/// cache-only note references a commit that was never fetched locally), the
/// input is returned in lexicographic order instead so callers still get a
/// deterministic result.
pub(crate) fn sort_commit_shas_by_date_desc(
    repo: &Repository,
    shas: HashSet<String>,
) -> Result<Vec<String>, GitAiError> {
    if shas.len() <= 1 {
        return Ok(shas.into_iter().collect());
    }

    let mut sha_vec: Vec<String> = shas.into_iter().collect();
    let mut args = repo.global_args_for_exec();
    args.push("log".to_string());
    args.push("--format=%H".to_string());
    args.push("--date-order".to_string());
    args.push("--no-walk".to_string());
    for sha in &sha_vec {
        args.push(sha.clone());
    }

    if let Ok(output) = exec_git_allow_nonzero(&args)
        && output.status.success()
        && let Ok(stdout) = String::from_utf8(output.stdout)
    {
        let sorted: Vec<String> = stdout.lines().map(|s| s.to_string()).collect();
        if sorted.len() == sha_vec.len() {
            return Ok(sorted);
        }
    }

    sha_vec.sort();
    Ok(sha_vec)
}

/// Direct access to the raw git-notes backend, for TESTS ONLY.
///
/// Production code must go through `crate::operations::git::notes_api`, which dispatches on
/// the configured notes backend — calling these directly silently ignores the
/// HTTP backend (notes live in the notes-db cache there, not refs/notes/ai).
/// The underlying functions are `pub(in crate::operations::git)` to enforce that; these
/// wrappers exist so backend unit tests can exercise the raw git
/// implementation regardless of backend configuration.
#[cfg(feature = "test-support")]
pub mod git_backend_for_tests {
    use super::*;

    pub fn notes_add(
        repo: &Repository,
        commit_sha: &str,
        note_content: &str,
    ) -> Result<(), GitAiError> {
        super::notes_add(repo, commit_sha, note_content)
    }

    pub fn show_authorship_note(repo: &Repository, commit_sha: &str) -> Option<String> {
        super::show_authorship_note(repo, commit_sha)
    }

    pub fn commits_with_authorship_notes(
        repo: &Repository,
        commit_shas: &[String],
    ) -> Result<HashSet<String>, GitAiError> {
        super::commits_with_authorship_notes(repo, commit_shas)
    }

    pub fn get_commits_with_notes_from_list(
        repo: &Repository,
        commit_shas: &[String],
    ) -> Result<Vec<CommitAuthorship>, GitAiError> {
        super::get_commits_with_notes_from_list(repo, commit_shas)
    }

    pub fn grep_ai_notes(repo: &Repository, pattern: &str) -> Result<Vec<String>, GitAiError> {
        super::grep_ai_notes(repo, pattern)
    }

    pub fn note_blob_oids_for_commits(
        repo: &Repository,
        commit_shas: &[String],
    ) -> Result<HashMap<String, String>, GitAiError> {
        super::note_blob_oids_for_commits(repo, commit_shas)
    }

    pub fn notes_add_batch(
        repo: &Repository,
        entries: &[(String, String)],
    ) -> Result<(), GitAiError> {
        super::notes_add_batch(repo, entries)
    }

    pub fn notes_add_blob_batch(
        repo: &Repository,
        entries: &[(String, String)],
    ) -> Result<(), GitAiError> {
        super::notes_add_blob_batch(repo, entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_batch_check_blob_oid_accepts_sha1_and_sha256() {
        let sha1 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa blob 10";
        let sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb blob 20";
        let invalid = "cccccccc blob 10";

        assert_eq!(
            parse_batch_check_blob_oid(sha1),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        );
        assert_eq!(
            parse_batch_check_blob_oid(sha256),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string())
        );
        assert_eq!(parse_batch_check_blob_oid(invalid), None);
    }

    #[test]
    fn test_notes_path_for_object() {
        // Short SHA (edge case)
        assert_eq!(notes_path_for_object("a"), "a");
        assert_eq!(notes_path_for_object("ab"), "ab");

        // Normal SHA (40 chars)
        assert_eq!(
            notes_path_for_object("abcdef1234567890abcdef1234567890abcdef12"),
            "ab/cdef1234567890abcdef1234567890abcdef12"
        );

        // SHA-256 (64 chars)
        assert_eq!(
            notes_path_for_object(
                "abc1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcd"
            ),
            "ab/c1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcd"
        );
    }

    #[test]
    fn test_flat_note_pathspec_for_commit() {
        let sha = "abcdef1234567890abcdef1234567890abcdef12";
        let pathspec = flat_note_pathspec_for_commit(sha);
        assert_eq!(
            pathspec,
            "refs/notes/ai:abcdef1234567890abcdef1234567890abcdef12"
        );
    }

    #[test]
    fn test_fanout_note_pathspec_for_commit() {
        let sha = "abcdef1234567890abcdef1234567890abcdef12";
        let pathspec = fanout_note_pathspec_for_commit(sha);
        assert_eq!(
            pathspec,
            "refs/notes/ai:ab/cdef1234567890abcdef1234567890abcdef12"
        );
    }

    #[test]
    fn test_sanitize_remote_name() {
        assert_eq!(sanitize_remote_name("origin"), "origin");
        assert_eq!(sanitize_remote_name("my-remote"), "my-remote");
        assert_eq!(sanitize_remote_name("remote_123"), "remote_123");
        assert_eq!(
            sanitize_remote_name("remote/with/slashes"),
            "remote_with_slashes"
        );
        assert_eq!(
            sanitize_remote_name("remote@with#special$chars"),
            "remote_with_special_chars"
        );
        assert_eq!(sanitize_remote_name("has spaces"), "has_spaces");
    }

    #[test]
    fn test_tracking_ref_for_remote() {
        assert_eq!(
            tracking_ref_for_remote("origin"),
            "refs/notes/ai-remote/origin"
        );
        assert_eq!(
            tracking_ref_for_remote("upstream"),
            "refs/notes/ai-remote/upstream"
        );
        assert_eq!(
            tracking_ref_for_remote("my-fork"),
            "refs/notes/ai-remote/my-fork"
        );
        // Special characters get sanitized
        assert_eq!(
            tracking_ref_for_remote("remote/with/slashes"),
            "refs/notes/ai-remote/remote_with_slashes"
        );
    }
}
