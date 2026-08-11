use std::collections::{HashMap, HashSet};

use crate::clients::git_cli::{exec_git_allow_nonzero, exec_git_stdin_streaming};
use crate::error::GitAiError;
use crate::operations::git::notes_api;
use crate::operations::git::oid::is_full_oid;
use crate::operations::git::repository::Repository;

use super::range_diff::list_commits_in_range;

#[derive(Debug, Clone)]
struct CommitMetadata {
    tree: String,
    parents: Vec<String>,
}

#[derive(Debug, Clone)]
struct SplitCandidate {
    source_sha: String,
    extracted_sha: String,
    destination_sha: String,
}

/// Find the extra parent commit produced by Graphite's non-interactive
/// `split --by-file` rewrite.
///
/// `range-diff` may match the old commit to either side of the split, or may
/// leave both rewritten commits unmatched. The extracted parent is
/// identifiable without Git ancestry queries: it is a deletion-only subset of
/// the old tree on the rewritten commit chain, and the remaining-files child
/// restores the old tree. All metadata and tree comparisons are batched.
pub(crate) fn derive_split_commit_mappings(
    repo: &Repository,
    base: &str,
    old_tip: &str,
    new_tip: &str,
    existing_mappings: &[(String, String)],
) -> Result<Vec<(String, String)>, GitAiError> {
    let old_commits = list_commits_in_range(repo, base, old_tip);
    let new_commits = list_commits_in_range(repo, base, new_tip);
    if old_commits.is_empty() || new_commits.is_empty() {
        return Ok(Vec::new());
    }

    let commits_with_notes = notes_api::commits_with_notes(repo, &old_commits)?;
    if commits_with_notes.is_empty() {
        return Ok(Vec::new());
    }

    let all_commits = unique_commits(old_commits.iter().chain(new_commits.iter()));
    let metadata = get_commit_metadata_batch(repo, &all_commits);
    if metadata.is_empty() {
        return Ok(Vec::new());
    }

    let mapped_new: HashSet<&str> = existing_mappings
        .iter()
        .map(|(_, new_sha)| new_sha.as_str())
        .collect();
    let mut completion_mappings = Vec::new();

    // `range-diff` can match the old commit to the extracted parent instead
    // of the remaining-files child. Complete that mapping when a descendant
    // restores the old tree.
    for old_sha in &old_commits {
        if !commits_with_notes.contains(old_sha) {
            continue;
        }
        let Some(old_metadata) = metadata.get(old_sha) else {
            continue;
        };
        for (_, destination_sha) in existing_mappings
            .iter()
            .filter(|(source_sha, _)| source_sha == old_sha)
        {
            let Some(destination_metadata) = metadata.get(destination_sha) else {
                continue;
            };
            if destination_metadata.tree == old_metadata.tree {
                continue;
            }
            let Some(completion_sha) = new_commits.iter().find(|candidate_sha| {
                !mapped_new.contains(candidate_sha.as_str())
                    && metadata
                        .get(*candidate_sha)
                        .is_some_and(|candidate| candidate.tree == old_metadata.tree)
                    && is_on_parent_chain(destination_sha, candidate_sha, &metadata)
            }) else {
                continue;
            };
            let completion = (old_sha.clone(), completion_sha.clone());
            if !completion_mappings.contains(&completion) {
                completion_mappings.push(completion);
            }
        }
    }
    let mut candidates = Vec::new();

    for old_sha in &old_commits {
        if !commits_with_notes.contains(old_sha) {
            continue;
        }
        let Some(old_metadata) = metadata.get(old_sha) else {
            continue;
        };
        let Some(old_parent) = old_metadata.parents.first() else {
            continue;
        };

        let destinations = existing_mappings
            .iter()
            .filter(|(source_sha, _)| source_sha == old_sha)
            .map(|(_, destination_sha)| destination_sha);
        let destinations: Vec<&String> = destinations.collect();

        for new_sha in &new_commits {
            if mapped_new.contains(new_sha.as_str()) {
                continue;
            }
            let Some(new_metadata) = metadata.get(new_sha) else {
                continue;
            };
            let Some(new_parent) = new_metadata.parents.first() else {
                continue;
            };

            if !parent_mapping_exists(old_parent, new_parent, existing_mappings) {
                continue;
            }
            if old_metadata.tree == new_metadata.tree {
                continue;
            }

            let destination_sha = if let Some(destination) = destinations
                .iter()
                .find(|destination| is_on_parent_chain(new_sha, destination, &metadata))
            {
                (*destination).clone()
            } else if destinations.is_empty() {
                let Some(destination) = new_commits.iter().find(|destination| {
                    !mapped_new.contains(destination.as_str())
                        && metadata
                            .get(*destination)
                            .is_some_and(|commit| commit.tree == old_metadata.tree)
                        && is_on_parent_chain(new_sha, destination, &metadata)
                }) else {
                    continue;
                };
                destination.clone()
            } else {
                continue;
            };

            let candidate = SplitCandidate {
                source_sha: old_sha.clone(),
                extracted_sha: new_sha.clone(),
                destination_sha,
            };
            if !candidates.iter().any(|existing: &SplitCandidate| {
                existing.source_sha == candidate.source_sha
                    && existing.extracted_sha == candidate.extracted_sha
            }) {
                candidates.push(candidate);
            }
        }
    }

    if candidates.is_empty() {
        return Ok(completion_mappings);
    }

    let candidate_pairs: Vec<(String, String)> = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.source_sha.clone(),
                candidate.extracted_sha.clone(),
            )
        })
        .collect();
    let deletion_only = summarize_tree_diffs_batch(repo, &candidate_pairs, &metadata)?;
    let mut mappings = completion_mappings;
    mappings.extend(
        candidates
            .into_iter()
            .zip(deletion_only)
            .filter_map(|(candidate, is_deletion_only)| {
                if !is_deletion_only {
                    return None;
                }
                let destination_mapping =
                    (!existing_mappings.iter().any(|(source, destination)| {
                        source == &candidate.source_sha && destination == &candidate.destination_sha
                    }))
                    .then(|| {
                        (
                            candidate.source_sha.clone(),
                            candidate.destination_sha.clone(),
                        )
                    });
                Some(
                    destination_mapping
                        .into_iter()
                        .chain(std::iter::once((
                            candidate.source_sha,
                            candidate.extracted_sha,
                        )))
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .collect::<Vec<_>>(),
    );
    Ok(mappings)
}

fn unique_commits<'a>(commits: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for commit in commits {
        if seen.insert(commit.as_str()) {
            unique.push(commit.clone());
        }
    }
    unique
}

fn parent_mapping_exists(
    old_parent: &str,
    new_parent: &str,
    mappings: &[(String, String)],
) -> bool {
    old_parent == new_parent
        || mappings
            .iter()
            .any(|(old_sha, new_sha)| old_sha == old_parent && new_sha == new_parent)
}

fn is_on_parent_chain(
    candidate: &str,
    descendant: &str,
    metadata: &HashMap<String, CommitMetadata>,
) -> bool {
    let mut current = descendant;
    let mut visited = HashSet::new();
    loop {
        if current == candidate {
            return true;
        }
        if !visited.insert(current) {
            return false;
        }
        let Some(commit) = metadata.get(current) else {
            return false;
        };
        let Some(parent) = commit.parents.first() else {
            return false;
        };
        current = parent;
    }
}

fn get_commit_metadata_batch(
    repo: &Repository,
    shas: &[String],
) -> HashMap<String, CommitMetadata> {
    if shas.is_empty() {
        return HashMap::new();
    }
    let mut args = repo.global_args_for_exec();
    args.extend([
        "show".to_string(),
        "-s".to_string(),
        "--format=%H %T %P".to_string(),
        "--no-walk".to_string(),
    ]);
    args.extend(shas.iter().cloned());

    let Ok(output) = exec_git_allow_nonzero(&args) else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let sha = parts.next()?.to_string();
            let tree = parts.next()?.to_string();
            let parents = parts.map(ToOwned::to_owned).collect::<Vec<_>>();
            Some((sha, CommitMetadata { tree, parents }))
        })
        .collect()
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TreeDiffSummary {
    changed: bool,
    only_deletions: bool,
}

struct TreeDiffSummaryParser {
    summaries: Vec<TreeDiffSummary>,
    current: Option<TreeDiffSummary>,
}

impl TreeDiffSummaryParser {
    fn new() -> Self {
        Self {
            summaries: Vec::new(),
            current: None,
        }
    }

    fn feed_line(&mut self, line: &str) {
        if is_tree_pair_separator(line) {
            if let Some(summary) = self.current.take() {
                self.summaries.push(summary);
            }
            self.current = Some(TreeDiffSummary {
                changed: false,
                only_deletions: true,
            });
        } else if let Some(summary) = self.current.as_mut()
            && line.starts_with(':')
        {
            let status = line.split_whitespace().nth(4).unwrap_or_default();
            summary.changed = true;
            summary.only_deletions &= status == "D";
        }
    }

    fn finish(mut self) -> Vec<TreeDiffSummary> {
        if let Some(summary) = self.current.take() {
            self.summaries.push(summary);
        }
        self.summaries
    }
}

fn is_tree_pair_separator(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let Some(first) = parts.next() else {
        return false;
    };
    let Some(second) = parts.next() else {
        return false;
    };
    parts.next().is_none() && is_full_oid(first) && is_full_oid(second)
}

fn summarize_tree_diffs_batch(
    repo: &Repository,
    pairs: &[(String, String)],
    metadata: &HashMap<String, CommitMetadata>,
) -> Result<Vec<bool>, GitAiError> {
    let mut stdin_data = String::new();
    for (old_sha, new_sha) in pairs {
        let Some(old_tree) = metadata.get(old_sha).map(|commit| commit.tree.as_str()) else {
            return Ok(vec![false; pairs.len()]);
        };
        let Some(new_tree) = metadata.get(new_sha).map(|commit| commit.tree.as_str()) else {
            return Ok(vec![false; pairs.len()]);
        };
        stdin_data.push_str(old_tree);
        stdin_data.push(' ');
        stdin_data.push_str(new_tree);
        stdin_data.push('\n');
    }

    let mut args = repo.global_args_for_exec();
    args.extend([
        "diff-tree".to_string(),
        "--stdin".to_string(),
        "-p".to_string(),
        "--raw".to_string(),
        "-r".to_string(),
        "--no-renames".to_string(),
        "--no-color".to_string(),
    ]);
    let mut parser = TreeDiffSummaryParser::new();
    exec_git_stdin_streaming(&args, stdin_data.as_bytes(), |line| parser.feed_line(line))?;
    let summaries = parser.finish();
    if summaries.len() != pairs.len() {
        return Err(GitAiError::Generic(format!(
            "diff-tree returned {} summaries for {} split candidates",
            summaries.len(),
            pairs.len()
        )));
    }
    Ok(summaries
        .into_iter()
        .map(|summary| summary.changed && summary.only_deletions)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_diff_summary_parser_accepts_deletions_only() {
        let old_tree = "a".repeat(40);
        let new_tree = "b".repeat(40);
        let output = format!(
            "{old_tree} {new_tree}\n:100644 000000 {} {} D\tremoved.txt\n",
            "c".repeat(40),
            "0".repeat(40),
        );

        let mut parser = TreeDiffSummaryParser::new();
        for line in output.lines() {
            parser.feed_line(line);
        }

        assert_eq!(
            parser.finish(),
            vec![TreeDiffSummary {
                changed: true,
                only_deletions: true,
            }]
        );
    }

    #[test]
    fn tree_diff_summary_parser_rejects_additions_and_modifications() {
        let old_tree = "a".repeat(40);
        let new_tree = "b".repeat(40);
        let output = format!(
            "{old_tree} {new_tree}\n:100644 100644 {} {} M\tchanged.txt\n",
            "c".repeat(40),
            "d".repeat(40),
        );

        let mut parser = TreeDiffSummaryParser::new();
        for line in output.lines() {
            parser.feed_line(line);
        }

        assert_eq!(
            parser.finish(),
            vec![TreeDiffSummary {
                changed: true,
                only_deletions: false,
            }]
        );
    }
}
