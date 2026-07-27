use crate::authorship::authorship_log::PromptRecord;
use crate::error::GitAiError;
use crate::git::notes_api::{read_authorship, search_notes};
use crate::git::repository::Repository;

/// Find a prompt in the repository history
///
/// If `commit` is provided, look only in that specific commit.
/// Otherwise, search through history and skip `offset` occurrences (0 = most recent).
pub fn find_prompt(
    repo: &Repository,
    prompt_id: &str,
    commit: Option<&str>,
    offset: usize,
) -> Result<(String, PromptRecord), GitAiError> {
    if let Some(commit_rev) = commit {
        // Look in specific commit
        find_prompt_in_commit(repo, prompt_id, commit_rev)
    } else {
        // Search through history with offset
        find_prompt_in_history(repo, prompt_id, offset)
    }
}

/// Find a prompt in a specific commit (searches both prompts and sessions)
pub fn find_prompt_in_commit(
    repo: &Repository,
    prompt_id: &str,
    commit_rev: &str,
) -> Result<(String, PromptRecord), GitAiError> {
    // Resolve the revision to a commit SHA
    let commit = repo.revparse_single(commit_rev)?;
    let commit_sha = commit.id();

    // Get the authorship log for this commit
    let authorship_log = read_authorship(repo, &commit_sha).ok_or_else(|| {
        GitAiError::Generic(format!(
            "No authorship data found for commit: {}",
            commit_rev
        ))
    })?;

    // Look for the prompt in the prompts map first
    if let Some(prompt) = authorship_log.metadata.prompts.get(prompt_id) {
        return Ok((commit_sha, prompt.clone()));
    }

    // Fall back to sessions map (session IDs start with "s_")
    // Strip ::t_ trace suffix if present — attestation hashes use s_xxx::t_yyy but session keys are just s_xxx
    let session_key = if prompt_id.starts_with("s_") {
        prompt_id.split("::").next().unwrap_or(prompt_id)
    } else {
        prompt_id
    };
    if let Some(session) = authorship_log.metadata.sessions.get(session_key) {
        return Ok((commit_sha, session.to_prompt_record()));
    }

    Err(GitAiError::Generic(format!(
        "Prompt '{}' not found in commit {}",
        prompt_id, commit_rev
    )))
}

/// Find a prompt in history, skipping `offset` occurrences
/// Returns the (N+1)th occurrence where N = offset (0 = most recent)
pub fn find_prompt_in_history(
    repo: &Repository,
    prompt_id: &str,
    offset: usize,
) -> Result<(String, PromptRecord), GitAiError> {
    // Strip ::t_ trace suffix for session lookups — attestation hashes use s_xxx::t_yyy
    // but session keys in metadata are just s_xxx
    let session_key = if prompt_id.starts_with("s_") {
        prompt_id.split("::").next().unwrap_or(prompt_id)
    } else {
        prompt_id
    };

    // Use git grep to search for the prompt ID in authorship notes
    // search_notes returns commits sorted by date (newest first)
    let shas = search_notes(repo, &format!("\"{}\"", session_key)).unwrap_or_default();

    if shas.is_empty() {
        return Err(GitAiError::Generic(format!(
            "Prompt not found in history: {}",
            prompt_id
        )));
    }

    // Iterate through commits, looking for the prompt and counting occurrences
    let mut found_count = 0;
    for sha in &shas {
        if let Some(authorship_log) = read_authorship(repo, sha) {
            // Check prompts map first
            if let Some(prompt) = authorship_log.metadata.prompts.get(prompt_id) {
                if found_count == offset {
                    return Ok((sha.clone(), prompt.clone()));
                }
                found_count += 1;
            // Then check sessions map
            } else if let Some(session) = authorship_log.metadata.sessions.get(session_key) {
                if found_count == offset {
                    return Ok((sha.clone(), session.to_prompt_record()));
                }
                found_count += 1;
            }
        }
    }

    // If we get here, we didn't find enough occurrences
    if found_count == 0 {
        Err(GitAiError::Generic(format!(
            "Prompt not found in history: {}",
            prompt_id
        )))
    } else {
        Err(GitAiError::Generic(format!(
            "Prompt '{}' found {} time(s), but offset {} requested (max offset: {})",
            prompt_id,
            found_count,
            offset,
            found_count - 1
        )))
    }
}
