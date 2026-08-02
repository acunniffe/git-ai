use crate::error::GitAiError;
use crate::model::authorship_log_serialization::AuthorshipLog;
use crate::model::hunk_shift::apply_hunk_shifts_to_file_attestation;
use crate::operations::git::notes_api;
use crate::operations::git::repository::Repository;

use super::diff_tree::compute_diff_trees_batch;
use super::note_shift::{merge_authorship_logs, write_authorship_log};
use super::range_diff::{find_merge_base, list_commits_in_range};
use super::{RewriteMetricCommit, RewriteMetricOperation, RewriteOutcome, rewrite_metrics_enabled};

pub(super) fn handle_squash_merge(
    repo: &Repository,
    source_head: &str,
    squash_commit: &str,
    onto: &str,
) -> Result<RewriteOutcome, GitAiError> {
    let target_notes = notes_api::read_notes_batch(repo, &[squash_commit.to_string()])?;
    let existing_target_log = target_notes
        .get(squash_commit)
        .and_then(|raw| AuthorshipLog::deserialize_from_string(raw).ok())
        .filter(|log| !log.attestations.is_empty());

    let base = find_merge_base(repo, source_head, onto).unwrap_or_else(|| onto.to_string());
    let source_commits = list_commits_in_range(repo, &base, source_head);
    let sources = if source_commits.is_empty() {
        vec![source_head.to_string()]
    } else {
        source_commits
    };

    crate::operations::git::sync_authorship::fetch_missing_notes_for_commits_best_effort(
        repo, &sources,
    );

    // Batch-read all source notes in O(1) git calls
    let source_notes_map = notes_api::read_notes_batch(repo, &sources)?;

    // Collect which source commits have parseable notes and need intermediate diffs
    struct SourceNote {
        log: AuthorshipLog,
        diff_idx: Option<usize>,
    }

    let mut source_notes: Vec<SourceNote> = Vec::new();
    let mut diff_pairs: Vec<(String, String)> = Vec::new();

    for src_sha in &sources {
        let Some(raw) = source_notes_map.get(src_sha) else {
            continue;
        };
        let Ok(log) = AuthorshipLog::deserialize_from_string(raw) else {
            continue;
        };

        let diff_idx = if src_sha.as_str() != source_head {
            let idx = diff_pairs.len();
            diff_pairs.push((src_sha.clone(), source_head.to_string()));
            Some(idx)
        } else {
            None
        };

        source_notes.push(SourceNote { log, diff_idx });
    }

    if source_notes.is_empty() {
        if let Some(existing_log) = existing_target_log.as_ref()
            && !repo.storage.has_working_log(onto)
        {
            let note = write_authorship_log_for_metrics(repo, squash_commit, existing_log)?;
            return Ok(squash_metric_outcome(squash_commit, &sources, onto, note));
        }
        let note =
            post_squash_resolution_working_log(repo, onto, squash_commit, existing_target_log)?;
        return Ok(squash_metric_outcome(squash_commit, &sources, onto, note));
    }

    // Add the final source_head→squash_commit pair
    let final_diff_idx = diff_pairs.len();
    diff_pairs.push((source_head.to_string(), squash_commit.to_string()));

    // Single batched diff-tree call for ALL intermediate shifts + final shift
    let diff_results = compute_diff_trees_batch(repo, &diff_pairs)?;

    // Phase 1: Shift intermediate notes to source_head's coordinate space and merge
    let mut merged_log: Option<AuthorshipLog> = None;

    for note in source_notes {
        let mut log = note.log;

        if let Some(idx) = note.diff_idx {
            let diff_to_tip = &diff_results[idx];
            for (old_path, new_path) in &diff_to_tip.renames {
                for attestation in &mut log.attestations {
                    if attestation.file_path == *old_path {
                        attestation.file_path = new_path.clone();
                    }
                }
            }
            if !diff_to_tip.hunks_by_file.is_empty() {
                log.attestations = log
                    .attestations
                    .iter()
                    .filter_map(|fa| match diff_to_tip.hunks_by_file.get(&fa.file_path) {
                        Some(hunks) => apply_hunk_shifts_to_file_attestation(fa, hunks),
                        None => Some(fa.clone()),
                    })
                    .collect();
            }
        }

        match merged_log.as_mut() {
            Some(existing) => merge_authorship_logs(existing, &log),
            None => merged_log = Some(log),
        }
    }

    let Some(mut final_log) = merged_log else {
        return Ok(RewriteOutcome::empty());
    };

    // Phase 2: Shift merged log from source_head to squash_commit
    let diff_result = &diff_results[final_diff_idx];

    for (old_path, new_path) in &diff_result.renames {
        for attestation in &mut final_log.attestations {
            if attestation.file_path == *old_path {
                attestation.file_path = new_path.clone();
            }
        }
    }

    if !diff_result.hunks_by_file.is_empty() {
        final_log.attestations = final_log
            .attestations
            .iter()
            .filter_map(|fa| match diff_result.hunks_by_file.get(&fa.file_path) {
                Some(hunks) => apply_hunk_shifts_to_file_attestation(fa, hunks),
                None => Some(fa.clone()),
            })
            .collect();
    }

    final_log.metadata.base_commit_sha = squash_commit.to_string();

    let shifted_log = match existing_target_log {
        Some(existing) => {
            crate::operations::authorship::conflict_resolution::merge_conflict_resolution_authorship(
                Some(final_log),
                existing,
                squash_commit,
            )
        }
        None => final_log,
    };

    if repo.storage.has_working_log(onto) {
        let note =
            post_squash_resolution_working_log(repo, onto, squash_commit, Some(shifted_log))?;
        Ok(squash_metric_outcome(squash_commit, &sources, onto, note))
    } else {
        let note = write_authorship_log_for_metrics(repo, squash_commit, &shifted_log)?;
        Ok(squash_metric_outcome(squash_commit, &sources, onto, note))
    }
}

fn attach_authorship_note(
    mut metric_commit: RewriteMetricCommit,
    note: Option<String>,
) -> RewriteMetricCommit {
    if let Some(note) = note {
        metric_commit = metric_commit.with_authorship_note(note);
    }
    metric_commit
}

fn squash_metric_outcome(
    squash_commit: &str,
    sources: &[String],
    onto: &str,
    note: Option<String>,
) -> RewriteOutcome {
    if !rewrite_metrics_enabled() {
        return RewriteOutcome::empty();
    }
    let mut metric_commit = RewriteMetricCommit::new(
        squash_commit.to_string(),
        sources.to_vec(),
        RewriteMetricOperation::SquashMerge,
    )
    .with_parent_sha(onto.to_string());
    metric_commit = attach_authorship_note(metric_commit, note);
    RewriteOutcome::from_metric_commits(vec![metric_commit])
}

fn post_squash_metric_note_from_result(
    result: crate::operations::authorship::post_commit::PostCommitDetailedResult,
) -> Option<String> {
    if rewrite_metrics_enabled() {
        Some(result.authorship_note)
    } else {
        None
    }
}

fn write_authorship_log_for_metrics(
    repo: &Repository,
    commit_sha: &str,
    log: &AuthorshipLog,
) -> Result<Option<String>, GitAiError> {
    let serialized = write_authorship_log(repo, commit_sha, log)?;
    if rewrite_metrics_enabled() {
        Ok(Some(serialized))
    } else {
        Ok(None)
    }
}

fn post_squash_resolution_working_log(
    repo: &Repository,
    onto: &str,
    squash_commit: &str,
    existing_shifted_log: Option<AuthorshipLog>,
) -> Result<Option<String>, GitAiError> {
    if !repo.storage.has_working_log(onto) {
        if let Some(log) = existing_shifted_log {
            return write_authorship_log_for_metrics(repo, squash_commit, &log);
        }
        return Ok(None);
    }

    let commit_for_transform = squash_commit.to_string();
    let author = repo.effective_author_identity().formatted_or_unknown();
    let post_commit_result =
        crate::operations::authorship::post_commit::post_commit_from_working_log_with_transform_and_options_detailed(
            repo,
            Some(onto.to_string()),
            squash_commit.to_string(),
            author,
            crate::operations::authorship::post_commit::PostCommitOptions {
                supress_output: true,
                compute_stats: false,
                recover_attribution: false,
                defer_note_write: false,
            },
            move |resolution_log| {
                Ok(
                    crate::operations::authorship::conflict_resolution::merge_conflict_resolution_authorship(
                        existing_shifted_log,
                        resolution_log,
                        &commit_for_transform,
                    ),
                )
            },
        )?;
    Ok(post_squash_metric_note_from_result(post_commit_result))
}
