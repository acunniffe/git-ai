use super::blame_loader::file_exists_in_commit;
use crate::error::GitAiError;
use crate::model::authorship_log::LineRange;
use crate::model::imara_diff_utils::{
    content_eq_ignoring_line_endings, normalize_line_endings, split_lines_with_terminators,
};
use crate::operations::git::repository::{Repository, batch_read_paths_at_treeishes};
use std::collections::{HashMap, HashSet};

/// Helper function to collect committed line ranges from git diff
pub(super) fn collect_committed_hunks(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    pathspecs: Option<&HashSet<String>>,
) -> Result<HashMap<String, Vec<LineRange>>, GitAiError> {
    let mut committed_hunks: HashMap<String, Vec<LineRange>> = HashMap::new();

    // Handle initial commit (no parent)
    if parent_sha == "initial" {
        // For initial commit, use git diff against the empty tree
        let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"; // Git's empty tree hash
        let added_lines = repo.diff_added_lines(empty_tree, commit_sha, pathspecs)?;

        for (file_path, lines) in added_lines {
            if !lines.is_empty() {
                committed_hunks.insert(file_path, LineRange::compress_lines(&lines));
            }
        }
        return Ok(committed_hunks);
    }

    // Use git diff to get added lines directly
    let added_lines = repo.diff_added_lines(parent_sha, commit_sha, pathspecs)?;

    for (file_path, lines) in added_lines {
        if !lines.is_empty() {
            committed_hunks.insert(file_path, LineRange::compress_lines(&lines));
        }
    }

    Ok(committed_hunks)
}

/// Detect file renames between parent and commit. Returns a map of old_path → new_path.
pub(super) fn detect_renames_in_commit(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
) -> Result<HashMap<String, String>, GitAiError> {
    use crate::clients::git_cli::exec_git_allow_nonzero;

    let mut args = repo.global_args_for_exec();
    args.extend([
        "diff-tree".to_string(),
        "-r".to_string(),
        "-M".to_string(),
        "--diff-filter=R".to_string(),
        parent_sha.to_string(),
        commit_sha.to_string(),
    ]);
    let output = exec_git_allow_nonzero(&args)?;
    let mut renames = HashMap::new();
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            // Format: :old_mode new_mode old_hash new_hash Rxx\told_path\tnew_path
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() == 3 {
                renames.insert(parts[1].to_string(), parts[2].to_string());
            }
        }
    }
    Ok(renames)
}

/// Helper function to collect unstaged line ranges (lines in working directory but not in commit)
/// Returns (unstaged_hunks, pure_insertion_hunks)
/// pure_insertion_hunks contains lines that were purely inserted (old_count=0), not modifications
#[allow(clippy::type_complexity)]
pub(super) fn collect_unstaged_hunks(
    repo: &Repository,
    commit_sha: &str,
    pathspecs: Option<&HashSet<String>>,
) -> Result<
    (
        HashMap<String, Vec<LineRange>>,
        HashMap<String, Vec<LineRange>>,
    ),
    GitAiError,
> {
    let mut unstaged_hunks: HashMap<String, Vec<LineRange>> = HashMap::new();
    let mut pure_insertion_hunks: HashMap<String, Vec<LineRange>> = HashMap::new();

    // Use git diff to get added lines in working directory vs commit, with insertion tracking
    let (added_lines, insertion_lines) =
        repo.diff_workdir_added_lines_with_insertions(commit_sha, pathspecs)?;

    for (file_path, lines) in added_lines {
        if !lines.is_empty() {
            unstaged_hunks.insert(file_path, LineRange::compress_lines(&lines));
        }
    }

    for (file_path, lines) in insertion_lines {
        if !lines.is_empty() {
            pure_insertion_hunks.insert(file_path, LineRange::compress_lines(&lines));
        }
    }

    // Check for untracked files in pathspecs that git diff didn't find
    // These are files that exist in the working directory but aren't tracked by git
    if let Some(paths) = pathspecs
        && let Ok(workdir) = repo.workdir()
    {
        for pathspec in paths {
            // Skip if we already found this file in git diff
            if unstaged_hunks.contains_key(pathspec) {
                continue;
            }

            // Check if file exists in the commit - if it does, it's tracked and git diff should handle it
            // Only process truly untracked files (files that don't exist in the commit tree)
            if file_exists_in_commit(repo, commit_sha, pathspec).unwrap_or(false) {
                continue;
            }

            // Check if file exists in working directory
            let file_path = workdir.join(pathspec);
            if file_path.exists() && file_path.is_file() {
                // Try to read the file
                if let Ok(content) = std::fs::read_to_string(&file_path) {
                    // Count the lines - all lines are "unstaged" since the file is untracked
                    let line_count = content.lines().count() as u32;
                    if line_count > 0 {
                        // Create a range covering all lines (1-indexed)
                        let range = vec![LineRange::Range(1, line_count)];
                        unstaged_hunks.insert(pathspec.clone(), range.clone());
                        // Untracked files are pure insertions (the entire file is new)
                        pure_insertion_hunks.insert(pathspec.clone(), range);
                    }
                }
            }
        }
    }

    Ok((unstaged_hunks, pure_insertion_hunks))
}

#[allow(clippy::type_complexity)]
pub(super) fn collect_unstaged_hunks_from_snapshot(
    repo: &Repository,
    commit_sha: &str,
    pathspecs: Option<&HashSet<String>>,
    final_state_snapshot: &HashMap<String, String>,
) -> Result<
    (
        HashMap<String, Vec<LineRange>>,
        HashMap<String, Vec<LineRange>>,
    ),
    GitAiError,
> {
    let mut unstaged_hunks: HashMap<String, Vec<LineRange>> = HashMap::new();
    let mut pure_insertion_hunks: HashMap<String, Vec<LineRange>> = HashMap::new();

    let file_paths: HashSet<String> = match pathspecs {
        Some(paths) => paths.iter().cloned().collect(),
        None => final_state_snapshot.keys().cloned().collect(),
    };

    // Batch-read committed content for every file in two git spawns instead of
    // one (fast-reader-miss) spawn per file.
    let requests: Vec<(String, String)> = file_paths
        .iter()
        .map(|file_path| (commit_sha.to_string(), file_path.clone()))
        .collect();
    let committed_contents = batch_file_contents(repo, &requests)?;

    for file_path in file_paths {
        let committed_content = committed_contents
            .get(&(commit_sha.to_string(), file_path.clone()))
            .cloned()
            .unwrap_or_default();
        let final_content = final_state_snapshot
            .get(&file_path)
            .cloned()
            .unwrap_or_else(|| committed_content.clone());

        if content_eq_ignoring_line_endings(&committed_content, &final_content) {
            continue;
        }

        let normalized_committed = normalize_line_endings(&committed_content);
        let normalized_final = normalize_line_endings(&final_content);
        let committed_lines = split_lines_with_terminators(&normalized_committed);
        let final_lines = split_lines_with_terminators(&normalized_final);
        let diff_ops =
            crate::model::imara_diff_utils::capture_diff_slices(&committed_lines, &final_lines);

        let mut all_added_lines = Vec::new();
        let mut pure_insertion_lines = Vec::new();

        for op in diff_ops {
            match op {
                crate::model::imara_diff_utils::DiffOp::Insert {
                    new_index, new_len, ..
                } => {
                    let start = new_index as u32 + 1;
                    let end = start + new_len as u32;
                    for line in start..end {
                        all_added_lines.push(line);
                        pure_insertion_lines.push(line);
                    }
                }
                crate::model::imara_diff_utils::DiffOp::Replace {
                    new_index, new_len, ..
                } => {
                    let start = new_index as u32 + 1;
                    let end = start + new_len as u32;
                    for line in start..end {
                        all_added_lines.push(line);
                    }
                }
                crate::model::imara_diff_utils::DiffOp::Equal { .. }
                | crate::model::imara_diff_utils::DiffOp::Delete { .. } => {}
            }
        }

        if !all_added_lines.is_empty() {
            unstaged_hunks.insert(
                file_path.clone(),
                LineRange::compress_lines(&all_added_lines),
            );
        }
        if !pure_insertion_lines.is_empty() {
            pure_insertion_hunks
                .insert(file_path, LineRange::compress_lines(&pure_insertion_lines));
        }
    }

    Ok((unstaged_hunks, pure_insertion_hunks))
}

/// Batch-read the content of many `(treeish, path)` pairs in a CONSTANT number
/// of git spawns (one `cat-file --batch-check` + one `cat-file --batch`),
/// regardless of how many files. Missing paths map to an empty string (the same
/// degradation `get_file_content_at_commit` produces for an absent path).
pub(super) fn batch_file_contents(
    repo: &Repository,
    requests: &[(String, String)],
) -> Result<HashMap<(String, String), String>, GitAiError> {
    if requests.is_empty() {
        return Ok(HashMap::new());
    }
    let mut map = batch_read_paths_at_treeishes(repo, requests)?;
    // Ensure every requested pair has an entry (absent paths → "").
    for req in requests {
        map.entry(req.clone()).or_default();
    }
    Ok(map)
}
