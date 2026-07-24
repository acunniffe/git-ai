use std::collections::{HashMap, HashSet};

use crate::config::Config;
use crate::error::GitAiError;
use crate::operations::git::repository::Repository;

mod diff_tree;
mod note_shift;
mod range_diff;
mod squash_merge;

pub(crate) use diff_tree::compute_diff_trees_batch;
pub(crate) use note_shift::shift_authorship_notes_merging_existing_with_notes;
pub use note_shift::{shift_authorship_notes, shift_authorship_notes_merging_existing};
pub(crate) use range_diff::list_commits_in_range;

pub use crate::model::domain::RewriteEvent;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DiffTreeResult {
    pub hunks_by_file: HashMap<String, Vec<crate::model::hunk_shift::DiffHunk>>,
    pub added_lines_by_file: HashMap<String, Vec<u32>>,
    pub renames: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RewriteMetricOperation {
    Rebase,
    SquashMerge,
    CherryPick,
    CherryPickNoCommit,
    Amend,
    Revert,
    UpdateRef,
    NonFastForward,
}

impl RewriteMetricOperation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Rebase => "rebase",
            Self::SquashMerge => "squash_merge",
            Self::CherryPick => "cherry_pick",
            Self::CherryPickNoCommit => "cherry_pick_no_commit",
            Self::Amend => "amend",
            Self::Revert => "revert",
            Self::UpdateRef => "update_ref",
            Self::NonFastForward => "non_fast_forward",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RewriteMetricCommit {
    pub new_sha: String,
    pub original_shas: Vec<String>,
    pub operation: RewriteMetricOperation,
    pub branch: Option<String>,
    pub parent_sha: Option<String>,
    pub authorship_note: Option<String>,
    pub parent_diff: Option<DiffTreeResult>,
}

impl RewriteMetricCommit {
    pub(crate) fn new(
        new_sha: impl Into<String>,
        original_shas: Vec<String>,
        operation: RewriteMetricOperation,
    ) -> Self {
        let mut deduped = Vec::with_capacity(original_shas.len());
        for sha in original_shas {
            if !sha.is_empty() && !deduped.contains(&sha) {
                deduped.push(sha);
            }
        }
        Self {
            new_sha: new_sha.into(),
            original_shas: deduped,
            operation,
            branch: None,
            parent_sha: None,
            authorship_note: None,
            parent_diff: None,
        }
    }

    pub(crate) fn with_branch(mut self, branch: impl Into<String>) -> Self {
        let branch = branch.into();
        if !branch.is_empty() {
            self.branch = Some(branch);
        }
        self
    }

    pub(crate) fn with_parent_sha(mut self, parent_sha: impl Into<String>) -> Self {
        let parent_sha = parent_sha.into();
        if !parent_sha.is_empty() {
            self.parent_sha = Some(parent_sha);
        }
        self
    }

    pub(crate) fn with_authorship_note(mut self, authorship_note: impl Into<String>) -> Self {
        self.authorship_note = Some(authorship_note.into());
        self
    }

    pub(crate) fn with_parent_diff(mut self, parent_diff: DiffTreeResult) -> Self {
        self.parent_diff = Some(parent_diff);
        self
    }
}

pub(crate) fn branch_name_from_ref(reference: &str) -> Option<String> {
    reference
        .strip_prefix("refs/heads/")
        .filter(|branch| !branch.is_empty())
        .map(ToOwned::to_owned)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RewriteOutcome {
    pub(crate) metric_commits: Vec<RewriteMetricCommit>,
}

impl RewriteOutcome {
    fn empty() -> Self {
        Self::default()
    }

    fn from_metric_commits(metric_commits: Vec<RewriteMetricCommit>) -> Self {
        Self { metric_commits }
    }
}

pub(crate) fn rewrite_metrics_enabled() -> bool {
    Config::get().get_feature_flags().rewrite_metrics_events
}

pub(crate) fn metric_commits_from_mappings(
    mappings: &[(String, String)],
    operation: RewriteMetricOperation,
) -> Vec<RewriteMetricCommit> {
    let mut order: Vec<String> = Vec::new();
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen_pairs: HashSet<(String, String)> = HashSet::new();
    for (source_sha, new_sha) in mappings {
        if source_sha.is_empty() || new_sha.is_empty() {
            continue;
        }
        if !seen_pairs.insert((new_sha.clone(), source_sha.clone())) {
            continue;
        }
        if !grouped.contains_key(new_sha) {
            order.push(new_sha.clone());
        }
        grouped
            .entry(new_sha.clone())
            .or_default()
            .push(source_sha.clone());
    }

    order
        .into_iter()
        .filter_map(|new_sha| {
            grouped
                .remove(&new_sha)
                .map(|original_shas| RewriteMetricCommit::new(new_sha, original_shas, operation))
        })
        .collect()
}

fn attach_authorship_notes(
    metric_commits: Vec<RewriteMetricCommit>,
    notes: Vec<(String, String)>,
) -> Vec<RewriteMetricCommit> {
    if notes.is_empty() {
        return metric_commits;
    }
    let mut notes_by_commit: HashMap<String, String> = notes.into_iter().collect();
    metric_commits
        .into_iter()
        .map(|mut commit| {
            if let Some(note) = notes_by_commit.remove(&commit.new_sha) {
                commit = commit.with_authorship_note(note);
            }
            commit
        })
        .collect()
}

pub fn handle_rewrite_event(repo: &Repository, event: RewriteEvent) -> Result<(), GitAiError> {
    handle_rewrite_event_with_metrics(repo, event).map(|_| ())
}

pub(crate) fn handle_rewrite_event_with_metrics(
    repo: &Repository,
    event: RewriteEvent,
) -> Result<RewriteOutcome, GitAiError> {
    match event {
        RewriteEvent::SquashMerge {
            ref source_head,
            ref squash_commit,
            ref onto,
        } => squash_merge::handle_squash_merge(repo, source_head, squash_commit, onto),
        RewriteEvent::NonFastForward {
            ref old_tip,
            ref new_tip,
            ref onto,
        } => handle_non_fast_forward_rewrite_with_operation(
            repo,
            old_tip,
            new_tip,
            onto.as_deref(),
            RewriteMetricOperation::NonFastForward,
        ),
        RewriteEvent::CherryPickComplete {
            sources,
            new_commits,
        } => {
            let mappings: Vec<(String, String)> = sources.into_iter().zip(new_commits).collect();
            if mappings.is_empty() {
                return Ok(RewriteOutcome::empty());
            }
            let source_shas: Vec<String> = mappings.iter().map(|(src, _)| src.clone()).collect();
            crate::operations::git::sync_authorship::fetch_missing_notes_for_commits(
                repo,
                &source_shas,
            )?;
            let shifted_notes =
                note_shift::shift_authorship_notes_merging_existing_with_notes(repo, &mappings)?;
            if !rewrite_metrics_enabled() {
                return Ok(RewriteOutcome::empty());
            }
            let metric_commits =
                metric_commits_from_mappings(&mappings, RewriteMetricOperation::CherryPick);
            Ok(RewriteOutcome::from_metric_commits(
                attach_authorship_notes(metric_commits, shifted_notes),
            ))
        }
    }
}

pub(crate) fn handle_non_fast_forward_rewrite_with_operation(
    repo: &Repository,
    old_tip: &str,
    new_tip: &str,
    onto: Option<&str>,
    operation: RewriteMetricOperation,
) -> Result<RewriteOutcome, GitAiError> {
    let mappings = range_diff::derive_mappings_from_range_diff(repo, old_tip, new_tip, onto)?;
    if mappings.is_empty() {
        return Ok(RewriteOutcome::empty());
    }
    let source_shas: Vec<String> = mappings.iter().map(|(src, _)| src.clone()).collect();
    crate::operations::git::sync_authorship::fetch_missing_notes_for_commits(repo, &source_shas)?;
    let shifted_notes =
        note_shift::shift_authorship_notes_merging_existing_with_notes(repo, &mappings)?;
    if !rewrite_metrics_enabled() {
        return Ok(RewriteOutcome::empty());
    }
    let metric_commits = metric_commits_from_mappings(&mappings, operation);
    Ok(RewriteOutcome::from_metric_commits(
        attach_authorship_notes(metric_commits, shifted_notes),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_commits_from_mappings_groups_squashed_sources() {
        let mappings = vec![
            ("old1".to_string(), "new".to_string()),
            ("old2".to_string(), "new".to_string()),
            ("old1".to_string(), "new".to_string()),
        ];

        let commits = metric_commits_from_mappings(&mappings, RewriteMetricOperation::Rebase);

        assert_eq!(
            commits,
            vec![RewriteMetricCommit::new(
                "new",
                vec!["old1".to_string(), "old2".to_string()],
                RewriteMetricOperation::Rebase,
            )]
        );
    }

    #[test]
    fn branch_name_from_ref_only_accepts_local_branch_refs() {
        assert_eq!(
            branch_name_from_ref("refs/heads/feature").as_deref(),
            Some("feature")
        );
        assert_eq!(branch_name_from_ref("HEAD"), None);
        assert_eq!(branch_name_from_ref("refs/tags/v1"), None);
    }

    #[test]
    fn rewrite_metric_operation_strings_are_stable() {
        assert_eq!(RewriteMetricOperation::Rebase.as_str(), "rebase");
        assert_eq!(RewriteMetricOperation::SquashMerge.as_str(), "squash_merge");
        assert_eq!(RewriteMetricOperation::CherryPick.as_str(), "cherry_pick");
        assert_eq!(
            RewriteMetricOperation::CherryPickNoCommit.as_str(),
            "cherry_pick_no_commit"
        );
        assert_eq!(RewriteMetricOperation::Amend.as_str(), "amend");
        assert_eq!(RewriteMetricOperation::Revert.as_str(), "revert");
        assert_eq!(RewriteMetricOperation::UpdateRef.as_str(), "update_ref");
        assert_eq!(
            RewriteMetricOperation::NonFastForward.as_str(),
            "non_fast_forward"
        );
    }
}
