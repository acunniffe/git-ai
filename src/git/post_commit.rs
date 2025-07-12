use crate::error::GitAiError;
use crate::git::refs::put_reference;
use crate::log_fmt::authorship_log::AuthorshipLog;
use crate::log_fmt::working_log::Checkpoint;
use crate::utils::debug_log;
use git2::Repository;
use serde_json;

pub fn post_commit(repo: &Repository, force: bool) -> Result<(String, AuthorshipLog), GitAiError> {
    // Get the current commit SHA (the commit that was just made)
    let head = repo.head()?;
    let commit_sha = match head.target() {
        Some(oid) => oid.to_string(),
        None => {
            return Err(GitAiError::Generic(
                "No HEAD commit found. Cannot run post-commit hook.".to_string(),
            ));
        }
    };

    // Verify the working directory is clean (commit was successful)
    let mut status_opts = git2::StatusOptions::new();
    status_opts.include_untracked(false);
    status_opts.include_ignored(false);
    status_opts.include_unmodified(false);

    let statuses = repo.statuses(Some(&mut status_opts))?;
    if !statuses.is_empty() {
        if force {
            println!("Warning: Working directory is not clean, but proceeding due to --force flag");
        } else {
            return Err(GitAiError::Generic(
                "Working directory is not clean after commit. Something went wrong. Use --force to bypass this check.".to_string(),
            ));
        }
    }

    let current_commit = repo.find_commit(head.target().unwrap())?;

    // Get the parent commit (base commit this was made on top of)
    let parent_sha = match current_commit.parent(0) {
        Ok(parent) => parent.id().to_string(),
        Err(_) => "initial".to_string(), // No parent commit found, use "initial" like in checkpoint.rs
    };

    // Pull all working log entries from the parent commit
    let parent_working_log = get_working_log(repo, &parent_sha)?;

    // Filter out untracked files from the working log
    let filtered_working_log = filter_untracked_files(repo, &parent_working_log)?;

    debug_log(&format!(
        "Working log entries: {}",
        filtered_working_log.len()
    ));

    // --- NEW: Serialize authorship log and store it in refs/ai/authorship/{commit_sha} ---
    let authorship_log = AuthorshipLog::from_working_log(&filtered_working_log);

    // Use pretty formatting in debug builds, single-line in release builds
    let authorship_json = if cfg!(debug_assertions) {
        serde_json::to_string_pretty(&authorship_log)?
    } else {
        serde_json::to_string(&authorship_log)?
    };

    let ref_name = format!("ai/authorship/{}", commit_sha);
    put_reference(
        repo,
        &ref_name,
        &authorship_json,
        &format!("AI authorship attestation for commit {}", commit_sha),
    )?;

    debug_log(&format!(
        "Authorship log written to refs/ai/authorship/{}",
        commit_sha
    ));

    Ok((ref_name, authorship_log))
}

/// Filter out working log entries for untracked files
fn filter_untracked_files(
    repo: &Repository,
    working_log: &[Checkpoint],
) -> Result<Vec<Checkpoint>, GitAiError> {
    // Get the current commit tree to see which files are currently tracked
    let head = repo.head()?;
    let current_commit = repo.find_commit(head.target().unwrap())?;
    let current_tree = current_commit.tree()?;

    // Filter the working log
    let mut filtered_checkpoints = Vec::new();

    for checkpoint in working_log {
        let mut filtered_entries = Vec::new();

        for entry in &checkpoint.entries {
            // Check if this file is currently tracked in the current commit
            if current_tree
                .get_path(std::path::Path::new(&entry.file))
                .is_ok()
            {
                filtered_entries.push(entry.clone());
            }
        }

        // Only include checkpoints that have at least one tracked file entry
        if !filtered_entries.is_empty() {
            let mut filtered_checkpoint = checkpoint.clone();
            filtered_checkpoint.entries = filtered_entries;
            filtered_checkpoints.push(filtered_checkpoint);
        }
    }

    Ok(filtered_checkpoints)
}

fn get_working_log(repo: &Repository, base_commit: &str) -> Result<Vec<Checkpoint>, GitAiError> {
    use crate::git::refs::get_reference;

    match get_reference(repo, &format!("ai-working-log/{}", base_commit)) {
        Ok(content) => {
            let working_log: Vec<Checkpoint> = serde_json::from_str(&content)?;
            Ok(working_log)
        }
        Err(_) => Ok(Vec::new()), // No working log exists yet
    }
}
