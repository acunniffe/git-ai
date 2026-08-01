use crate::error::GitAiError;
use crate::operations::authorship::rewrite::{
    DiffTreeResult, RewriteMetricCommit, compute_diff_trees_batch,
};
use crate::operations::git::repository::Repository;
use std::collections::HashMap;

pub(crate) struct ParentDiffBatch {
    commit_parent_pairs: Vec<(String, String)>,
    diff_results_by_pair: Vec<Option<DiffTreeResult>>,
}

pub(crate) struct ParentDiff {
    pub(crate) parent_sha: String,
    pub(crate) diff: Option<DiffTreeResult>,
}

pub(crate) type ParentDiffsByCommit = HashMap<String, ParentDiff>;

impl ParentDiffBatch {
    pub(crate) fn compute(
        repo: &Repository,
        commit_parent_pairs: Vec<(String, String)>,
    ) -> Result<Self, GitAiError> {
        let qualifying_indexes = commit_parent_pairs
            .iter()
            .enumerate()
            .filter_map(|(index, (_, parent_sha))| {
                repo.storage.has_working_log(parent_sha).then_some(index)
            })
            .collect::<Vec<_>>();
        let diff_pairs = qualifying_indexes
            .iter()
            .map(|&index| {
                let (commit_sha, parent_sha) = &commit_parent_pairs[index];
                (parent_sha.clone(), commit_sha.clone())
            })
            .collect::<Vec<_>>();

        // `compute_diff_trees_batch` returns without spawning Git for an empty
        // input, so every caller reaches this single batch operation exactly once.
        let diff_results = compute_diff_trees_batch(repo, &diff_pairs)?;

        Ok(Self::from_parts(
            commit_parent_pairs,
            qualifying_indexes,
            diff_results,
        ))
    }

    fn from_parts(
        commit_parent_pairs: Vec<(String, String)>,
        qualifying_indexes: Vec<usize>,
        diff_results: Vec<DiffTreeResult>,
    ) -> Self {
        debug_assert_eq!(qualifying_indexes.len(), diff_results.len());

        let mut diff_results_by_pair = std::iter::repeat_with(|| None)
            .take(commit_parent_pairs.len())
            .collect::<Vec<Option<DiffTreeResult>>>();
        for (index, diff_result) in qualifying_indexes.into_iter().zip(diff_results) {
            diff_results_by_pair[index] = Some(diff_result);
        }

        Self {
            commit_parent_pairs,
            diff_results_by_pair,
        }
    }

    pub(crate) fn commit_parent_pairs(&self) -> &[(String, String)] {
        &self.commit_parent_pairs
    }

    pub(crate) fn borrowed_diffs_by_commit(&self) -> HashMap<&str, &DiffTreeResult> {
        self.commit_parent_pairs
            .iter()
            .zip(&self.diff_results_by_pair)
            .filter_map(|((commit_sha, _), diff_result)| {
                diff_result
                    .as_ref()
                    .map(|diff_result| (commit_sha.as_str(), diff_result))
            })
            .collect()
    }

    pub(crate) fn into_owned_by_commit(self) -> ParentDiffsByCommit {
        self.commit_parent_pairs
            .into_iter()
            .zip(self.diff_results_by_pair)
            .map(|((commit_sha, parent_sha), diff)| (commit_sha, ParentDiff { parent_sha, diff }))
            .collect()
    }
}

pub(crate) fn rewrite_metric_commits_with_parent_diffs(
    metric_commits: Vec<RewriteMetricCommit>,
    parent_diffs_by_commit: ParentDiffsByCommit,
) -> Vec<RewriteMetricCommit> {
    metric_commits
        .into_iter()
        .map(|mut commit| {
            if let Some(parent_diff) = parent_diffs_by_commit.get(&commit.new_sha) {
                commit = commit.with_parent_sha(parent_diff.parent_sha.clone());
                if let Some(diff) = &parent_diff.diff {
                    commit = commit.with_parent_diff(diff.clone());
                }
            }
            commit
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::authorship::rewrite::DiffTreeResult;
    use std::collections::HashMap;

    fn diff(marker: &str) -> DiffTreeResult {
        DiffTreeResult {
            added_lines_by_file: HashMap::from([(marker.to_string(), vec![1])]),
            ..DiffTreeResult::default()
        }
    }

    #[test]
    fn parent_diff_batch_exposes_borrowed_diffs_only_for_qualifying_pairs() {
        let batch = ParentDiffBatch::from_parts(
            vec![
                ("first".to_string(), "base".to_string()),
                ("second".to_string(), "first".to_string()),
                ("third".to_string(), "second".to_string()),
            ],
            vec![1, 2],
            vec![diff("second"), diff("third")],
        );

        let diffs = batch.borrowed_diffs_by_commit();

        assert_eq!(diffs.len(), 2);
        assert!(!diffs.contains_key("first"));
        assert_eq!(diffs["second"].added_lines_by_file["second"], vec![1]);
        assert_eq!(diffs["third"].added_lines_by_file["third"], vec![1]);
    }

    #[test]
    fn parent_diff_batch_moves_parent_and_diff_data_into_owned_view() {
        let batch = ParentDiffBatch::from_parts(
            vec![
                ("first".to_string(), "base".to_string()),
                ("second".to_string(), "first".to_string()),
            ],
            vec![1],
            vec![diff("second")],
        );

        let parents_and_diffs = batch.into_owned_by_commit();

        assert_eq!(parents_and_diffs["first"].parent_sha, "base");
        assert!(parents_and_diffs["first"].diff.is_none());
        assert_eq!(
            parents_and_diffs["second"]
                .diff
                .as_ref()
                .unwrap()
                .added_lines_by_file["second"],
            vec![1]
        );
    }
}
