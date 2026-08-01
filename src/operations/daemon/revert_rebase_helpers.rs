use crate::error::GitAiError;
use crate::model::domain::RewriteEvent;
use crate::operations::daemon::parent_diff_batch::{
    ParentDiffBatch, rewrite_metric_commits_with_parent_diffs,
};
use crate::operations::git::cli_parser::explicit_rebase_branch_arg;
use crate::operations::git::oid::is_non_zero_oid;

fn revert_destination_changes(
    cmd: &crate::model::domain::NormalizedCommand,
) -> Vec<&crate::model::domain::RefChange> {
    cmd.ref_changes
        .iter()
        .filter(|change| {
            change.reference == "HEAD"
                && is_non_zero_oid(&change.old)
                && is_non_zero_oid(&change.new)
                && change.old != change.new
        })
        .collect()
}

pub(crate) fn apply_revert_complete_rewrite(
    repo: &crate::operations::git::repository::Repository,
    cmd: &crate::model::domain::NormalizedCommand,
    source_oids: &[String],
) -> Result<(), GitAiError> {
    let specs: Vec<crate::operations::authorship::rewrite_revert::RevertSpec> =
        revert_destination_changes(cmd)
            .into_iter()
            .enumerate()
            .map(
                |(index, change)| crate::operations::authorship::rewrite_revert::RevertSpec {
                    revert_commit: change.new.clone(),
                    parent: Some(change.old.clone()),
                    reverted_commit: source_oids.get(index).cloned(),
                },
            )
            .collect();
    let metric_commits =
        crate::operations::authorship::rewrite_revert::handle_revert_commits_with_metrics(
            repo, &specs,
        )?;
    crate::operations::daemon::rewrite_metrics::spawn_rewrite_commit_metrics(repo, metric_commits);
    Ok(())
}

pub(crate) fn apply_cherry_pick_complete_rewrite(
    repo: &crate::operations::git::repository::Repository,
    original_head: &str,
    sources: &[String],
    new_commits: &[String],
) -> Result<(), GitAiError> {
    let pairs = crate::operations::authorship::rewrite_cherry_pick::match_cherry_pick_pairs(
        repo,
        sources,
        new_commits,
    )?;
    let mut rewrite_metric_commits = Vec::new();
    if !pairs.is_empty() {
        let (src, dst): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
        let outcome = crate::operations::authorship::rewrite::handle_rewrite_event_with_metrics(
            repo,
            RewriteEvent::CherryPickComplete {
                sources: src,
                new_commits: dst,
            },
        )?;
        rewrite_metric_commits.extend(outcome.metric_commits);
    }

    let existing_notes = crate::operations::git::notes_api::read_notes_batch(repo, new_commits)?;
    let author = repo.effective_author_identity().formatted_or_unknown();

    // The cherry-picked commits form a chain: each commit's parent is the
    // previous one (the first's parent is original_head). Build the
    // (commit, parent) pairs, then batch the parent->commit diffs for the
    // commits that actually need reconstruction into ONE diff-tree so the loop
    // performs no per-commit git spawns.
    let mut commit_parent_pairs: Vec<(String, String)> = Vec::new();
    let mut parent = original_head.to_string();
    for commit_sha in new_commits {
        commit_parent_pairs.push((commit_sha.clone(), parent.clone()));
        parent = commit_sha.clone();
    }
    let parent_diff_batch = ParentDiffBatch::compute(repo, commit_parent_pairs)?;
    {
        let diff_by_commit = parent_diff_batch.borrowed_diffs_by_commit();
        super::flush_pending_note_writes(
            repo,
            parent_diff_batch.commit_parent_pairs(),
            &existing_notes,
            author,
            &diff_by_commit,
        )?;
    }

    let rewrite_metric_commits = if rewrite_metric_commits.is_empty() {
        rewrite_metric_commits
    } else {
        rewrite_metric_commits_with_parent_diffs(
            rewrite_metric_commits,
            parent_diff_batch.into_owned_by_commit(),
        )
    };
    crate::operations::daemon::rewrite_metrics::spawn_rewrite_commit_metrics(
        repo,
        rewrite_metric_commits,
    );

    Ok(())
}

pub(crate) fn apply_cherry_pick_no_commit_rewrite(
    repo: &crate::operations::git::repository::Repository,
    sources: &[String],
    parent_head: &str,
    new_head: &str,
) -> Result<(), GitAiError> {
    if sources.is_empty() || new_head.is_empty() {
        return Ok(());
    }
    let mappings = sources
        .iter()
        .map(|source| (source.clone(), new_head.to_string()))
        .collect::<Vec<_>>();
    crate::operations::git::sync_authorship::fetch_missing_notes_for_commits(repo, sources)?;
    let shifted_notes =
        crate::operations::authorship::rewrite::shift_authorship_notes_merging_existing_with_notes(
            repo, &mappings,
        )?;
    if crate::operations::authorship::rewrite::rewrite_metrics_enabled() {
        let mut metric_commit = crate::operations::authorship::rewrite::RewriteMetricCommit::new(
            new_head.to_string(),
            sources.to_vec(),
            crate::operations::authorship::rewrite::RewriteMetricOperation::CherryPickNoCommit,
        )
        .with_parent_sha(parent_head.to_string());
        if let Some((_, note)) = shifted_notes
            .into_iter()
            .find(|(commit_sha, _)| commit_sha == new_head)
        {
            metric_commit = metric_commit.with_authorship_note(note);
        }
        crate::operations::daemon::rewrite_metrics::spawn_rewrite_commit_metrics(
            repo,
            vec![metric_commit],
        );
    }
    Ok(())
}

pub(crate) fn strict_rebase_original_head_from_command(
    cmd: &crate::model::domain::NormalizedCommand,
    semantic_old_head: &str,
) -> Option<String> {
    if let Some(branch_spec) = explicit_rebase_branch_arg(&cmd.invoked_args)
        && let Some(branch_ref) = explicit_rebase_branch_ref_name(&branch_spec)
        && let Some(old_head) = cmd
            .ref_changes
            .iter()
            .find(|change| change.reference == branch_ref && is_non_zero_oid(&change.old))
            .map(|change| change.old.clone())
    {
        return Some(old_head);
    }

    if is_non_zero_oid(semantic_old_head) {
        return Some(semantic_old_head.to_string());
    }

    if let Some(old_head) = cmd
        .ref_changes
        .iter()
        .find(|change| change.reference.starts_with("refs/heads/") && is_non_zero_oid(&change.old))
        .map(|change| change.old.clone())
    {
        return Some(old_head);
    }

    if let Some(old_head) = cmd
        .ref_changes
        .iter()
        .find(|change| change.reference == "HEAD" && is_non_zero_oid(&change.old))
        .map(|change| change.old.clone())
    {
        return Some(old_head);
    }

    None
}

fn explicit_rebase_branch_ref_name(branch_spec: &str) -> Option<String> {
    if branch_spec.starts_with("refs/") {
        return Some(branch_spec.to_string());
    }
    if super::is_valid_oid(branch_spec) || branch_spec == "HEAD" || branch_spec.starts_with("@{") {
        return None;
    }
    Some(format!("refs/heads/{}", branch_spec))
}
