use std::collections::HashMap;
use std::collections::HashSet;

use serde::Deserialize;
use serde::Serialize;

use crate::authorship::diff_ai_accepted::diff_ai_accepted_stats;
use crate::authorship::ignore::{build_ignore_matcher, should_ignore_file_with_matcher};
use crate::authorship::stats::{CommitStats, stats_for_commit_stats, stats_from_authorship_log};
use crate::error::GitAiError;
use crate::git::notes_api::{CommitAuthorship, filter_commits_with_notes};
use crate::git::repository::{CommitRange, InternalGitProfile, Repository, exec_git_with_profile};
use std::io::IsTerminal;

/// The git empty tree hash - represents an empty repository state
/// This is the hash of the empty tree object that git uses internally
#[doc(hidden)]
pub const EMPTY_TREE_HASH: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Check if a file path should be ignored based on the provided patterns
/// Supports both exact matches and glob patterns (e.g., "*.lock", "**/*.generated.js")
#[allow(dead_code)] // Kept for downstream compatibility.
pub fn should_ignore_file(path: &str, ignore_patterns: &[String]) -> bool {
    crate::authorship::ignore::should_ignore_file(path, ignore_patterns)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeAuthorshipStats {
    pub authorship_stats: RangeAuthorshipStatsData,
    pub range_stats: CommitStats,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeAuthorshipStatsData {
    pub total_commits: usize,
    pub commits_with_authorship: usize,
    pub authors_committing_authorship: HashSet<String>,
    pub authors_not_committing_authorship: HashSet<String>,
    pub commits_without_authorship: Vec<String>,
    pub commits_without_authorship_with_authors: Vec<(String, String)>, // (sha, git_author)
}

pub fn range_authorship(
    commit_range: CommitRange,
    pre_fetch_contents: bool,
    ignore_patterns: &[String],
    commit_shas: Option<Vec<String>>,
) -> Result<RangeAuthorshipStats, GitAiError> {
    commit_range.is_valid()?;

    // Fetch the branch if pre_fetch_contents is true
    if pre_fetch_contents {
        let repository = commit_range.repo();
        let refname = &commit_range.refname;

        // Get default remote, fallback to "origin" if not found
        let default_remote = repository
            .get_default_remote()?
            .unwrap_or_else(|| "origin".to_string());

        // Extract remote and branch from refname
        let (remote, fetch_refspec) = if refname.starts_with("refs/remotes/") {
            // Remote branch: refs/remotes/origin/branch-name -> origin, refs/heads/branch-name
            let without_prefix = refname.strip_prefix("refs/remotes/").unwrap();
            let parts: Vec<&str> = without_prefix.splitn(2, '/').collect();
            if parts.len() == 2 {
                (parts[0].to_string(), format!("refs/heads/{}", parts[1]))
            } else {
                (default_remote.clone(), refname.to_string())
            }
        } else if refname.starts_with("refs/heads/") {
            // Local branch: refs/heads/branch-name -> default_remote, refs/heads/branch-name
            (default_remote.clone(), refname.to_string())
        } else if refname.contains('/') && !refname.starts_with("refs/") {
            // Simple remote format: origin/branch-name -> origin, refs/heads/branch-name
            let parts: Vec<&str> = refname.splitn(2, '/').collect();
            if parts.len() == 2 {
                (parts[0].to_string(), format!("refs/heads/{}", parts[1]))
            } else {
                (default_remote.clone(), format!("refs/heads/{}", refname))
            }
        } else {
            // Plain branch name: branch-name -> default_remote, refs/heads/branch-name
            (default_remote.clone(), format!("refs/heads/{}", refname))
        };

        let mut args = repository.global_args_for_exec();
        args.push("fetch".to_string());
        args.push(remote.clone());
        args.push(fetch_refspec.clone());

        let output = exec_git_with_profile(&args, InternalGitProfile::General)?;

        if !output.status.success() {
            return Err(GitAiError::Generic(format!(
                "Failed to fetch {} from {}: {}",
                fetch_refspec,
                remote,
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        tracing::debug!("Fetched {} from {}", fetch_refspec, remote);
    }

    // Clone commit_range before consuming it
    let repository = commit_range.repo();
    let commit_range_clone = commit_range.clone();

    // Use provided commit SHAs or collect them from the range
    let commit_shas: Vec<String> = match commit_shas {
        Some(shas) => shas,
        None => commit_range
            .into_iter()
            .map(|c| c.id().to_string())
            .collect(),
    };
    let commit_authorship = filter_commits_with_notes(repository, &commit_shas)?;

    // Calculate range stats - pass commit_shas directly to avoid re-fetching
    let range_stats = calculate_range_stats_direct(
        repository,
        commit_range_clone,
        &commit_shas,
        ignore_patterns,
    )?;

    Ok(RangeAuthorshipStats {
        authorship_stats: RangeAuthorshipStatsData {
            total_commits: commit_authorship.len(),
            commits_with_authorship: commit_authorship
                .iter()
                .filter(|ca| matches!(ca, CommitAuthorship::Log { .. }))
                .count(),
            authors_committing_authorship: commit_authorship
                .iter()
                .filter_map(|ca| match ca {
                    CommitAuthorship::Log { git_author, .. } => Some(git_author.clone()),
                    _ => None,
                })
                .collect(),
            authors_not_committing_authorship: commit_authorship
                .iter()
                .filter_map(|ca| match ca {
                    CommitAuthorship::NoLog { git_author, .. } => Some(git_author.clone()),
                    _ => None,
                })
                .collect(),
            commits_without_authorship: commit_authorship
                .iter()
                .filter_map(|ca| match ca {
                    CommitAuthorship::NoLog { sha, .. } => Some(sha.clone()),
                    _ => None,
                })
                .collect(),
            commits_without_authorship_with_authors: commit_authorship
                .iter()
                .filter_map(|ca| match ca {
                    CommitAuthorship::NoLog { sha, git_author } => {
                        Some((sha.clone(), git_author.clone()))
                    }
                    _ => None,
                })
                .collect(),
        },
        range_stats,
    })
}

/// Create an in-memory authorship log for a commit range by treating it as a squash
fn create_authorship_log_for_range(
    repo: &Repository,
    start_sha: &str,
    end_sha: &str,
    commit_shas: &[String],
    ignore_patterns: &[String],
) -> Result<crate::authorship::authorship_log_serialization::AuthorshipLog, GitAiError> {
    use crate::authorship::virtual_attribution::{
        VirtualAttributions, merge_attributions_favoring_first,
    };

    tracing::debug!(
        "Calculating authorship log for range: {} -> {}",
        start_sha,
        end_sha
    );

    // Step 1: Get list of changed files between the two commits
    let all_changed_files = repo.diff_changed_files(start_sha, end_sha)?;
    let ignore_matcher = build_ignore_matcher(ignore_patterns);

    // Filter out ignored files from the changed files
    let changed_files: Vec<String> = all_changed_files
        .into_iter()
        .filter(|file| !should_ignore_file_with_matcher(file, &ignore_matcher))
        .collect();

    // Note: We intentionally do NOT filter to AI-touched files here.
    // For range authorship, AI lines may have been introduced in commits BEFORE the range
    // and still exist in the end state. We need to process all changed files and let
    // VirtualAttributions find the correct authorship from git blame history.

    if changed_files.is_empty() {
        // No files changed, return empty authorship log
        tracing::debug!("No files changed in range");
        return Ok(
            crate::authorship::authorship_log_serialization::AuthorshipLog {
                attestations: Vec::new(),
                metadata: crate::authorship::authorship_log_serialization::AuthorshipMetadata {
                    base_commit_sha: end_sha.to_string(),
                    ..crate::authorship::authorship_log_serialization::AuthorshipMetadata::new()
                },
            },
        );
    }

    tracing::debug!(
        "Processing {} changed files for range authorship",
        changed_files.len()
    );

    // Special handling for empty tree: there's no start state to compare against
    // We only need the end state's attributions
    if start_sha == EMPTY_TREE_HASH {
        tracing::debug!("Start is empty tree - using only end commit attributions");

        let repo_clone = repo.clone();
        let mut end_va = crate::tokio_runtime::block_on(async {
            VirtualAttributions::new_for_base_commit(
                repo_clone,
                end_sha.to_string(),
                &changed_files,
                None,
            )
            .await
        })?;

        // Filter to only include prompts from commits in this range
        let commit_set: HashSet<String> = commit_shas.iter().cloned().collect();
        end_va.filter_to_commits(&commit_set);

        // Convert to AuthorshipLog
        let mut authorship_log = end_va.to_authorship_log()?;
        authorship_log.metadata.base_commit_sha = end_sha.to_string();

        tracing::debug!(
            "Created authorship log with {} attestations, {} prompts",
            authorship_log.attestations.len(),
            authorship_log.metadata.prompts.len()
        );

        return Ok(authorship_log);
    }

    // Step 2: Create VirtualAttributions for start commit (older)
    // Pass start_sha as blame_start_commit to limit blame scope to the range,
    // avoiding expensive traversal of the entire repository history
    let repo_clone = repo.clone();
    let start_sha_limit = Some(start_sha.to_string());
    let mut start_va = crate::tokio_runtime::block_on(async {
        VirtualAttributions::new_for_base_commit(
            repo_clone,
            start_sha.to_string(),
            &changed_files,
            start_sha_limit,
        )
        .await
    })?;

    // Step 3: Create VirtualAttributions for end commit (newer)
    // Pass start_sha as blame_start_commit to limit blame scope to the range
    let repo_clone = repo.clone();
    let start_sha_limit = Some(start_sha.to_string());
    let mut end_va = crate::tokio_runtime::block_on(async {
        VirtualAttributions::new_for_base_commit(
            repo_clone,
            end_sha.to_string(),
            &changed_files,
            start_sha_limit,
        )
        .await
    })?;

    // Step 3.5: Filter both VirtualAttributions to only include prompts from commits in this range
    // This ensures we only count AI contributions that happened during these commits,
    // not AI contributions from before the range
    let commit_set: HashSet<String> = commit_shas.iter().cloned().collect();
    start_va.filter_to_commits(&commit_set);
    end_va.filter_to_commits(&commit_set);

    // Step 4: Read committed files from end commit (final state)
    let committed_files = get_committed_files_content(repo, end_sha, &changed_files)?;

    tracing::debug!(
        "Read {} committed files from end commit",
        committed_files.len()
    );

    // Step 5: Merge VirtualAttributions, favoring end commit (newer state)
    let merged_va = merge_attributions_favoring_first(end_va, start_va, committed_files)?;

    // Step 6: Convert to AuthorshipLog
    let mut authorship_log = merged_va.to_authorship_log()?;
    authorship_log.metadata.base_commit_sha = end_sha.to_string();

    tracing::debug!(
        "Created authorship log with {} attestations, {} prompts",
        authorship_log.attestations.len(),
        authorship_log.metadata.prompts.len()
    );

    Ok(authorship_log)
}

/// Get file contents from a commit tree for specified pathspecs
fn get_committed_files_content(
    repo: &Repository,
    commit_sha: &str,
    pathspecs: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    let commit = repo.find_commit(commit_sha.to_string())?;
    let tree = commit.tree()?;

    let mut files = HashMap::new();

    for file_path in pathspecs {
        match tree.get_path(std::path::Path::new(file_path)) {
            Ok(entry) => {
                if let Ok(blob) = repo.find_blob(entry.id()) {
                    let blob_content = blob.content().unwrap_or_default();
                    let content = String::from_utf8_lossy(&blob_content).to_string();
                    files.insert(file_path.clone(), content);
                }
            }
            Err(_) => {
                // File doesn't exist in this commit (could be deleted), skip it
            }
        }
    }

    Ok(files)
}

/// Get git diff statistics for a commit range (start..end)
fn get_git_diff_stats_for_range(
    repo: &Repository,
    start_sha: &str,
    end_sha: &str,
    ignore_patterns: &[String],
) -> Result<(u32, u32), GitAiError> {
    // Use git diff --numstat to get diff statistics for the range
    let mut args = repo.global_args_for_exec();
    args.push("diff".to_string());
    args.push("--numstat".to_string());
    args.push(format!("{}..{}", start_sha, end_sha));

    let output = exec_git_with_profile(&args, InternalGitProfile::NumstatParse)?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut added_lines = 0u32;
    let mut deleted_lines = 0u32;
    let ignore_matcher = build_ignore_matcher(ignore_patterns);

    // Parse numstat output
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        // Parse numstat format: "added\tdeleted\tfilename"
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            // Check if this file should be ignored and skip it
            let filename = parts[2];
            if should_ignore_file_with_matcher(filename, &ignore_matcher) {
                continue;
            }

            // Parse added lines
            if let Ok(added) = parts[0].parse::<u32>() {
                added_lines += added;
            }

            // Parse deleted lines (handle "-" for binary files)
            if parts[1] != "-"
                && let Ok(deleted) = parts[1].parse::<u32>()
            {
                deleted_lines += deleted;
            }
        }
    }

    Ok((added_lines, deleted_lines))
}

/// Calculate AI vs human line contributions for a commit range
/// Uses VirtualAttributions approach to create an in-memory squash
fn calculate_range_stats_direct(
    repo: &Repository,
    commit_range: CommitRange,
    commit_shas: &[String],
    ignore_patterns: &[String],
) -> Result<CommitStats, GitAiError> {
    let start_sha = commit_range.start_oid.clone();
    let end_sha = commit_range.end_oid.clone();
    // Special case: single commit range (start == end)
    if start_sha == end_sha {
        return stats_for_commit_stats(repo, &end_sha, ignore_patterns);
    }

    // Step 1: Get git diff stats between start and end
    let (git_diff_added_lines, git_diff_deleted_lines) =
        get_git_diff_stats_for_range(repo, &start_sha, &end_sha, ignore_patterns)?;

    let diff_ai_stats = diff_ai_accepted_stats(repo, &start_sha, &end_sha, None, ignore_patterns)?;

    // Step 2: Create in-memory authorship log for the range, filtered to only commits in the range
    let authorship_log =
        create_authorship_log_for_range(repo, &start_sha, &end_sha, commit_shas, ignore_patterns)?;

    // Step 3: Calculate stats from the authorship log
    let stats = stats_from_authorship_log(
        Some(&authorship_log),
        git_diff_added_lines,
        git_diff_deleted_lines,
        diff_ai_stats.total_ai_accepted,
        0,
        &diff_ai_stats.per_tool_model,
    );

    Ok(stats)
}

pub fn print_range_authorship_stats(stats: &RangeAuthorshipStats) {
    println!("\n");

    // If there's no AI authorship in the range, show the special message
    if stats.authorship_stats.commits_with_authorship == 0 {
        println!("Committers are not using git-ai");
        return;
    }

    // Use existing stats terminal output
    use crate::authorship::stats::write_stats_to_terminal;

    // Only print stats if we're in an interactive terminal
    let is_interactive = std::io::stdout().is_terminal();
    write_stats_to_terminal(&stats.range_stats, is_interactive);

    // Check if all individual commits have authorship logs (for optional breakdown)
    let all_have_authorship =
        stats.authorship_stats.commits_with_authorship == stats.authorship_stats.total_commits;

    // If not all commits have authorship logs, show the breakdown
    if !all_have_authorship {
        let commits_without =
            stats.authorship_stats.total_commits - stats.authorship_stats.commits_with_authorship;
        let commit_word = if commits_without == 1 {
            "commit"
        } else {
            "commits"
        };
        println!(
            "  {} {} without Authorship Logs",
            commits_without, commit_word
        );

        // Show each commit without authorship
        for (sha, author) in &stats
            .authorship_stats
            .commits_without_authorship_with_authors
        {
            println!("    {} {}", &sha[0..7], author);
        }
    }
}
