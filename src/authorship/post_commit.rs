use crate::authorship::attribution_recovery::{
    AttributionRecoveryContext, FileTimestampsByPath, UnknownLinesByFile,
};
use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::authorship::diff_base::single_commit_diff_base;
use crate::authorship::ignore::{
    build_ignore_matcher, effective_ignore_patterns, should_ignore_file_with_matcher,
};
use crate::authorship::rewrite::DiffTreeResult;
use crate::authorship::stats::{stats_for_commit_stats_from_hunks, write_stats_to_terminal};
use crate::authorship::virtual_attribution::{AuthorshipLogDiffContext, VirtualAttributions};
use crate::authorship::working_log::{Checkpoint, CheckpointKind, WorkingLogEntry};
use crate::config::Config;
use crate::error::GitAiError;
use crate::git::notes_api::write_note;
use crate::git::repository::{Repository, batch_read_paths_at_treeishes, exec_git};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;

/// Skip expensive post-commit stats when this threshold is exceeded.
/// High hunk density is the strongest predictor of slow diff_ai_accepted_stats.
#[doc(hidden)]
pub const STATS_SKIP_MAX_HUNKS: usize = 1000;
/// Skip expensive stats for very large net additions even if hunks are moderate.
#[doc(hidden)]
pub const STATS_SKIP_MAX_ADDED_LINES: usize = 6000;
/// Skip expensive stats for extremely wide commits touching many added-line files.
#[doc(hidden)]
pub const STATS_SKIP_MAX_FILES_WITH_ADDITIONS: usize = 200;
/// Skip expensive stats for commits that delete a large number of lines.
/// Deletion-heavy commits (e.g. removing many files) trigger the same expensive
/// diff-parsing path as large addition commits, but the added-lines estimate is
/// near zero, so the cost was previously invisible to the estimator.
#[doc(hidden)]
pub const STATS_SKIP_MAX_DELETED_LINES: usize = 6000;

#[derive(Debug, Clone, Copy)]
#[doc(hidden)]
pub struct StatsCostEstimate {
    pub files_with_additions: usize,
    pub added_lines: usize,
    pub hunk_ranges: usize,
    pub deleted_lines: usize,
}

fn checkpoint_entry_requires_post_processing(
    checkpoint: &Checkpoint,
    entry: &WorkingLogEntry,
) -> bool {
    if checkpoint.kind != CheckpointKind::Human {
        return true;
    }

    entry
        .line_attributions
        .iter()
        .any(|attr| attr.author_id != CheckpointKind::Human.to_str() || attr.overrode.is_some())
        || entry
            .attributions
            .iter()
            .any(|attr| attr.author_id != CheckpointKind::Human.to_str())
}

pub fn post_commit(
    repo: &Repository,
    base_commit: Option<String>,
    commit_sha: String,
    human_author: String,
    supress_output: bool,
) -> Result<(String, AuthorshipLog), GitAiError> {
    post_commit_from_working_log(repo, base_commit, commit_sha, human_author, supress_output)
}

pub fn post_commit_from_working_log(
    repo: &Repository,
    base_commit: Option<String>,
    commit_sha: String,
    human_author: String,
    supress_output: bool,
) -> Result<(String, AuthorshipLog), GitAiError> {
    post_commit_from_working_log_with_transform(
        repo,
        base_commit,
        commit_sha,
        human_author,
        supress_output,
        Ok,
    )
}

pub(crate) fn post_commit_from_working_log_with_recovery_timestamps(
    repo: &Repository,
    base_commit: Option<String>,
    commit_sha: String,
    human_author: String,
    supress_output: bool,
    recovery_file_timestamps: Option<&FileTimestampsByPath>,
    before_external_recovery: Option<&dyn Fn(&UnknownLinesByFile)>,
) -> Result<(String, AuthorshipLog), GitAiError> {
    post_commit_from_working_log_with_transform_context(
        repo,
        base_commit,
        commit_sha,
        human_author,
        PostCommitOptions {
            supress_output,
            compute_stats: true,
            recover_attribution: true,
        },
        PostCommitContext {
            precomputed_parent_diff: None,
            recovery_file_timestamps,
            before_external_recovery,
        },
        Ok,
    )
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PostCommitOptions {
    pub supress_output: bool,
    pub compute_stats: bool,
    pub recover_attribution: bool,
}

pub(crate) struct PostCommitDetailedResult {
    pub commit_sha: String,
    pub authorship_log: AuthorshipLog,
    pub authorship_note: String,
}

#[derive(Clone, Copy, Default)]
struct PostCommitContext<'a> {
    precomputed_parent_diff: Option<&'a DiffTreeResult>,
    recovery_file_timestamps: Option<&'a FileTimestampsByPath>,
    before_external_recovery: Option<&'a dyn Fn(&UnknownLinesByFile)>,
}

pub fn post_commit_from_working_log_with_transform<F>(
    repo: &Repository,
    base_commit: Option<String>,
    commit_sha: String,
    human_author: String,
    supress_output: bool,
    transform: F,
) -> Result<(String, AuthorshipLog), GitAiError>
where
    F: FnOnce(AuthorshipLog) -> Result<AuthorshipLog, GitAiError>,
{
    post_commit_from_working_log_with_transform_and_options(
        repo,
        base_commit,
        commit_sha,
        human_author,
        PostCommitOptions {
            supress_output,
            compute_stats: true,
            recover_attribution: true,
        },
        transform,
    )
}

pub(crate) fn post_commit_from_working_log_with_transform_and_options<F>(
    repo: &Repository,
    base_commit: Option<String>,
    commit_sha: String,
    human_author: String,
    options: PostCommitOptions,
    transform: F,
) -> Result<(String, AuthorshipLog), GitAiError>
where
    F: FnOnce(AuthorshipLog) -> Result<AuthorshipLog, GitAiError>,
{
    post_commit_from_working_log_with_transform_options_and_diff(
        repo,
        base_commit,
        commit_sha,
        human_author,
        options,
        None,
        transform,
    )
}

pub(crate) fn post_commit_from_working_log_with_transform_and_options_detailed<F>(
    repo: &Repository,
    base_commit: Option<String>,
    commit_sha: String,
    human_author: String,
    options: PostCommitOptions,
    transform: F,
) -> Result<PostCommitDetailedResult, GitAiError>
where
    F: FnOnce(AuthorshipLog) -> Result<AuthorshipLog, GitAiError>,
{
    post_commit_from_working_log_with_transform_context_detailed(
        repo,
        base_commit,
        commit_sha,
        human_author,
        options,
        PostCommitContext {
            precomputed_parent_diff: None,
            recovery_file_timestamps: None,
            before_external_recovery: None,
        },
        transform,
    )
}

/// As [`post_commit_from_working_log_with_transform_and_options`], but accepts a
/// pre-computed parent→commit `DiffTreeResult`. A batched caller (the rebase
/// conflict-resolution driver) computes every qualifying commit's parent→commit
/// diff in ONE `diff-tree` and threads each result here, so this function makes
/// no per-commit `git diff` / `git diff-tree` spawns. With `None` the behavior
/// is identical to the unbatched single-commit path.
pub(crate) fn post_commit_from_working_log_with_transform_options_and_diff<F>(
    repo: &Repository,
    base_commit: Option<String>,
    commit_sha: String,
    human_author: String,
    options: PostCommitOptions,
    precomputed_parent_diff: Option<&DiffTreeResult>,
    transform: F,
) -> Result<(String, AuthorshipLog), GitAiError>
where
    F: FnOnce(AuthorshipLog) -> Result<AuthorshipLog, GitAiError>,
{
    post_commit_from_working_log_with_transform_context(
        repo,
        base_commit,
        commit_sha,
        human_author,
        options,
        PostCommitContext {
            precomputed_parent_diff,
            recovery_file_timestamps: None,
            before_external_recovery: None,
        },
        transform,
    )
}

fn post_commit_from_working_log_with_transform_context<F>(
    repo: &Repository,
    base_commit: Option<String>,
    commit_sha: String,
    human_author: String,
    options: PostCommitOptions,
    context: PostCommitContext<'_>,
    transform: F,
) -> Result<(String, AuthorshipLog), GitAiError>
where
    F: FnOnce(AuthorshipLog) -> Result<AuthorshipLog, GitAiError>,
{
    post_commit_from_working_log_with_transform_context_detailed(
        repo,
        base_commit,
        commit_sha,
        human_author,
        options,
        context,
        transform,
    )
    .map(|result| (result.commit_sha, result.authorship_log))
}

fn post_commit_from_working_log_with_transform_context_detailed<F>(
    repo: &Repository,
    base_commit: Option<String>,
    commit_sha: String,
    human_author: String,
    options: PostCommitOptions,
    context: PostCommitContext<'_>,
    transform: F,
) -> Result<PostCommitDetailedResult, GitAiError>
where
    F: FnOnce(AuthorshipLog) -> Result<AuthorshipLog, GitAiError>,
{
    // Use base_commit parameter if provided, otherwise use "initial" for empty repos
    // This matches the convention in checkpoint.rs
    let parent_sha = base_commit.unwrap_or_else(|| "initial".to_string());

    // Initialize the new storage system
    let repo_storage = &repo.storage;
    let working_log = repo_storage.working_log_for_base_commit(&parent_sha)?;

    let parent_working_log = working_log.read_all_checkpoints()?;

    let observed_snapshot = working_log.observed_file_snapshot()?;
    let working_va = VirtualAttributions::from_persisted_working_log(
        repo.clone(),
        parent_sha.clone(),
        Some(human_author.clone()),
    )?;

    // Build pathspecs from AI-relevant checkpoint entries only.
    // Human-only entries with no AI attribution do not affect authorship output and should not
    // trigger expensive post-commit diff work across large commits.
    let mut pathspecs: HashSet<String> = HashSet::new();
    for checkpoint in &parent_working_log {
        for entry in &checkpoint.entries {
            if checkpoint_entry_requires_post_processing(checkpoint, entry) {
                pathspecs.insert(entry.file.clone());
            }
        }
    }

    // Also include files from INITIAL attributions (uncommitted files from previous commits)
    // These files may not have checkpoints but still need their attribution preserved
    // when they are finally committed. See issue #356.
    let initial_attributions_for_pathspecs = working_log.read_initial_attributions();
    for file_path in initial_attributions_for_pathspecs.files.keys() {
        pathspecs.insert(file_path.clone());
    }

    let committed_diff_base = single_commit_diff_base(&parent_sha, &commit_sha);
    let (mut authorship_log, initial_attributions, initial_file_contents) = working_va
        .to_authorship_log_and_initial_working_log_with_precomputed_diff(
            repo,
            &parent_sha,
            &commit_sha,
            Some(&pathspecs),
            Some(&observed_snapshot),
            AuthorshipLogDiffContext {
                precomputed_parent_diff: context.precomputed_parent_diff,
                fallback_committed_diff_base: Some(&committed_diff_base),
            },
        )?;

    authorship_log.metadata.base_commit_sha = commit_sha.clone();

    // No-hooks background agents (Devin, Codex Cloud, etc.) may not fire checkpoints
    // for all edits. Attribute any committed lines that have no existing attestation
    // ("holes") to the detected agent, preserving explicit attributions.
    if !matches!(
        crate::authorship::background_agent::detect(),
        crate::authorship::background_agent::BackgroundAgent::None
            | crate::authorship::background_agent::BackgroundAgent::WithHooks { .. }
    ) {
        // Prefer the batched parent→commit diff when supplied (no extra spawn);
        // otherwise fall back to a per-commit `git diff`.
        let committed_hunks: Option<
            HashMap<String, Vec<crate::authorship::authorship_log::LineRange>>,
        > = if let Some(diff) = context.precomputed_parent_diff {
            Some(
                crate::authorship::virtual_attribution::committed_hunks_from_diff_result(
                    diff, None,
                ),
            )
        } else {
            // Same bounding as recovery: on the daemon fast-forward `update-ref`
            // path `parent_sha` is the far-behind old branch tip, so diff against
            // the finalized commit's immediate parent to avoid buffering the whole
            // pulled range (PD-23 / #1677). No-hooks agents (Devin/Codex Cloud)
            // can be active during a pull, so this path is exposed too.
            let diff_base = single_commit_diff_base(&parent_sha, &commit_sha);
            repo.diff_added_lines(&diff_base, &commit_sha, None)
                .ok()
                .map(|added_lines| {
                    added_lines
                        .into_iter()
                        .filter(|(_, lines)| !lines.is_empty())
                        .map(|(path, lines)| {
                            (
                                path,
                                crate::authorship::authorship_log::LineRange::compress_lines(
                                    &lines,
                                ),
                            )
                        })
                        .collect()
                })
        };
        if let Some(committed_hunks) = committed_hunks {
            crate::authorship::background_agent::fill_unattributed_lines(
                &mut authorship_log,
                &committed_hunks,
                &human_author,
            );
        }
    }

    authorship_log = transform(authorship_log)?;
    authorship_log.metadata.base_commit_sha = commit_sha.clone();

    let is_debug_self_check = repo.workdir().is_ok_and(|workdir| {
        crate::diagnostic_sentinels::path_is_in_debug_self_check_root(&workdir)
    });
    // Synthetic self-check commits already provide explicit attribution for every line.
    if options.recover_attribution && !is_debug_self_check {
        let recovery_hunks = recovery_committed_hunks(
            repo,
            &parent_sha,
            &commit_sha,
            context.precomputed_parent_diff,
        )?;
        crate::authorship::attribution_recovery::recover_attribution(
            repo,
            &parent_sha,
            &commit_sha,
            &human_author,
            &mut authorship_log,
            &recovery_hunks,
            AttributionRecoveryContext {
                file_timestamps: context.recovery_file_timestamps,
                before_external_recovery: context.before_external_recovery,
            },
        )?;
        authorship_log.metadata.base_commit_sha = commit_sha.clone();
    }

    // Long-lived daemon processes should read a fresh config snapshot.
    // Always use Config::fresh() to support runtime config updates
    // (especially important for daemon mode, but also good for consistency)
    let config = Config::fresh();
    let custom_attrs = config.custom_attributes().clone();

    // Inject custom attributes into all PromptRecords and SessionRecords.
    if !custom_attrs.is_empty() {
        for pr in authorship_log.metadata.prompts.values_mut() {
            pr.custom_attributes = Some(custom_attrs.clone());
        }
        for sr in authorship_log.metadata.sessions.values_mut() {
            sr.custom_attributes = Some(custom_attrs.clone());
        }
    }

    let authorship_note_str = authorship_log
        .serialize_to_string()
        .map_err(|_| GitAiError::Generic("Failed to serialize authorship log".to_string()))?;

    write_note(repo, &commit_sha, &authorship_note_str)?;

    // Compute stats once (needed for both metrics and terminal output), unless preflight
    // estimate predicts this would be too expensive for the commit hook path.
    let mut stats: Option<crate::authorship::stats::CommitStats> = None;
    let mut skip_reason = None;

    if options.compute_stats {
        let stats_diff_base = single_commit_diff_base(&parent_sha, &commit_sha);
        let is_merge_commit = repo
            .find_commit(commit_sha.clone())
            .map(|commit| commit.parent_count().unwrap_or(0) > 1)
            .unwrap_or(false);
        let ignore_patterns = effective_ignore_patterns(repo, &[], &[]);
        skip_reason = if is_merge_commit {
            Some(StatsSkipReason::MergeCommit)
        } else {
            estimate_stats_cost(repo, &stats_diff_base, &commit_sha, &ignore_patterns)
                .ok()
                .and_then(|estimate| {
                    if should_skip_expensive_post_commit_stats(&estimate) {
                        Some(StatsSkipReason::Expensive(estimate))
                    } else {
                        None
                    }
                })
        };

        if skip_reason.is_none() {
            let diff_hunks = crate::commands::diff::get_diff_with_line_numbers(
                repo,
                &stats_diff_base,
                &commit_sha,
            )?;

            let computed = stats_for_commit_stats_from_hunks(
                repo,
                &commit_sha,
                &ignore_patterns,
                &diff_hunks,
                Some(&authorship_log),
            )?;

            let hunks_json = crate::commands::diff::build_diff_artifacts_from_hunks(
                repo,
                diff_hunks,
                &commit_sha,
                Some(&authorship_log),
            )
            .ok()
            .and_then(|artifacts| serde_json::to_string(&artifacts.json_hunks).ok());

            // Record metrics only when we have full stats.
            record_commit_metrics(
                repo,
                &commit_sha,
                // Store the recorded parent as `base_commit_sha`, not the
                // `<commit>^` diff rev-expression used for the diff spawns.
                &parent_sha,
                &human_author,
                &authorship_note_str,
                &computed,
                &parent_working_log,
                hunks_json.as_deref(),
            );
            stats = Some(computed);
        }
    }

    if options.compute_stats && skip_reason.is_some() {
        match skip_reason.as_ref() {
            Some(StatsSkipReason::MergeCommit) => {
                tracing::debug!("Skipping post-commit stats for merge commit {}", commit_sha);
            }
            Some(StatsSkipReason::Expensive(estimate)) => {
                tracing::debug!(
                    "Skipping expensive post-commit stats for {} (files_with_additions={}, added_lines={}, deleted_lines={}, hunks={})",
                    commit_sha,
                    estimate.files_with_additions,
                    estimate.added_lines,
                    estimate.deleted_lines,
                    estimate.hunk_ranges
                );
            }
            None => {}
        }
    }

    // Write INITIAL file for uncommitted AI attributions (if any)
    if !initial_attributions.files.is_empty() {
        let new_working_log = repo_storage.working_log_for_base_commit(&commit_sha)?;
        new_working_log.write_initial_attributions_with_contents(
            initial_attributions.files,
            initial_attributions.prompts,
            initial_attributions.humans,
            initial_file_contents,
            initial_attributions.sessions,
        )?;
    }

    // // Clean up old working log
    repo_storage.delete_working_log_for_base_commit(&parent_sha)?;

    // Use Config::fresh() to support runtime config updates
    if !options.supress_output && !Config::fresh().is_quiet() {
        // Only print stats if we're in an interactive terminal and quiet mode is disabled
        let is_interactive = std::io::stdout().is_terminal();
        if let Some(stats) = stats.as_ref() {
            write_stats_to_terminal(stats, is_interactive);
        } else {
            match skip_reason.as_ref() {
                Some(StatsSkipReason::MergeCommit) => {
                    eprintln!(
                        "[git-ai] Skipped git-ai stats for merge commit {}.",
                        commit_sha
                    );
                }
                Some(StatsSkipReason::Expensive(estimate)) => {
                    eprintln!(
                        "[git-ai] Skipped git-ai stats for large commit (files_with_additions={}, added_lines={}, deleted_lines={}, hunks={}). Run `git-ai stats {}` to compute stats on demand.",
                        estimate.files_with_additions,
                        estimate.added_lines,
                        estimate.deleted_lines,
                        estimate.hunk_ranges,
                        commit_sha
                    );
                }
                None => {}
            }
        }
    }
    Ok(PostCommitDetailedResult {
        commit_sha: commit_sha.to_string(),
        authorship_log,
        authorship_note: authorship_note_str,
    })
}

fn commit_tree_snapshot_for_files(
    repo: &Repository,
    commit_sha: &str,
    file_paths: &HashSet<String>,
) -> Result<HashMap<String, String>, GitAiError> {
    let requests = file_paths
        .iter()
        .map(|file_path| (commit_sha.to_string(), file_path.clone()))
        .collect::<Vec<_>>();
    let contents = batch_read_paths_at_treeishes(repo, &requests)?;
    let mut snapshot = HashMap::with_capacity(file_paths.len());
    for file_path in file_paths {
        snapshot.insert(
            file_path.clone(),
            contents
                .get(&(commit_sha.to_string(), file_path.clone()))
                .cloned()
                .unwrap_or_default(),
        );
    }

    Ok(snapshot)
}

fn recovery_committed_hunks(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    precomputed_parent_diff: Option<&crate::authorship::rewrite::DiffTreeResult>,
) -> Result<HashMap<String, Vec<crate::authorship::authorship_log::LineRange>>, GitAiError> {
    if let Some(diff) = precomputed_parent_diff {
        return Ok(
            crate::authorship::virtual_attribution::committed_hunks_from_diff_result(diff, None),
        );
    }

    // Recovery only attributes lines added by the commit being finalized, so the
    // diff must be bounded to that single commit (see `single_commit_diff_base`).
    let diff_base = single_commit_diff_base(parent_sha, commit_sha);
    let added_lines = repo.diff_added_lines(&diff_base, commit_sha, None)?;
    Ok(added_lines
        .into_iter()
        .filter(|(_, lines)| !lines.is_empty())
        .map(|(path, lines)| {
            (
                path,
                crate::authorship::authorship_log::LineRange::compress_lines(&lines),
            )
        })
        .collect())
}

/// Amend-specific post-commit that merges blame-sourced attributions from the
/// original commit with persisted working-log checkpoint data.
pub fn post_commit_amend(
    repo: &Repository,
    original_commit: &str,
    amended_commit: &str,
    human_author: String,
) -> Result<(String, AuthorshipLog), GitAiError> {
    post_commit_amend_with_recovery_timestamps(
        repo,
        original_commit,
        amended_commit,
        human_author,
        None,
        None,
    )
}

pub(crate) struct PostCommitAmendResult {
    pub commit_sha: String,
    pub authorship_log: AuthorshipLog,
    pub authorship_note: String,
    pub parent_sha: String,
}

pub(crate) fn post_commit_amend_with_recovery_timestamps(
    repo: &Repository,
    original_commit: &str,
    amended_commit: &str,
    human_author: String,
    recovery_file_timestamps: Option<&FileTimestampsByPath>,
    before_external_recovery: Option<&dyn Fn(&UnknownLinesByFile)>,
) -> Result<(String, AuthorshipLog), GitAiError> {
    post_commit_amend_with_recovery_timestamps_detailed(
        repo,
        original_commit,
        amended_commit,
        human_author,
        recovery_file_timestamps,
        before_external_recovery,
    )
    .map(|result| (result.commit_sha, result.authorship_log))
}

pub(crate) fn post_commit_amend_with_recovery_timestamps_detailed(
    repo: &Repository,
    original_commit: &str,
    amended_commit: &str,
    human_author: String,
    recovery_file_timestamps: Option<&FileTimestampsByPath>,
    before_external_recovery: Option<&dyn Fn(&UnknownLinesByFile)>,
) -> Result<PostCommitAmendResult, GitAiError> {
    let repo_storage = &repo.storage;
    let working_log = repo_storage.working_log_for_base_commit(original_commit)?;

    // Compute pathspecs: changed files in the amended commit + working log touched files
    let changed_files = repo.list_commit_files(amended_commit, None)?;
    let mut pathspecs: HashSet<String> = changed_files.into_iter().collect();
    let touched_files = working_log.all_touched_files()?;
    pathspecs.extend(touched_files);
    let initial_attributions_for_pathspecs = working_log.read_initial_attributions();
    for file_path in initial_attributions_for_pathspecs.files.keys() {
        pathspecs.insert(file_path.clone());
    }
    let pathspecs_vec: Vec<String> = pathspecs.iter().cloned().collect();
    let observed_snapshot = working_log.observed_file_snapshot()?;
    let mut final_state_snapshot =
        commit_tree_snapshot_for_files(repo, amended_commit, &pathspecs)?;
    final_state_snapshot.extend(observed_snapshot);

    // Check if original commit has existing authorship data. Read through
    // notes_api so the HTTP notes backend is honored (the note may only exist
    // in the notes-db cache, not refs/notes/ai).
    let has_existing_data = crate::git::notes_api::read_authorship_v3(repo, original_commit)
        .map(|log| {
            !log.metadata.prompts.is_empty()
                || !log.metadata.humans.is_empty()
                || !log.metadata.sessions.is_empty()
        })
        .unwrap_or(false);

    let working_va = crate::tokio_runtime::block_on(async {
        VirtualAttributions::from_working_log_for_commit_snapshot(
            repo.clone(),
            original_commit.to_string(),
            &pathspecs_vec,
            if has_existing_data {
                None
            } else {
                Some(human_author.clone())
            },
            None,
            &final_state_snapshot,
        )
        .await
    })?;

    // Resolve parent of the amended commit for diff base
    let amended_commit_obj = repo.find_commit(amended_commit.to_string())?;
    let parent_sha = if amended_commit_obj.parent_count()? > 0 {
        amended_commit_obj
            .parent(0)
            .map(|p| p.id())
            .unwrap_or_else(|_| "initial".to_string())
    } else {
        "initial".to_string()
    };

    let (mut authorship_log, initial_attributions, initial_file_contents) = working_va
        .to_authorship_log_and_initial_working_log(
            repo,
            &parent_sha,
            amended_commit,
            Some(&pathspecs),
            Some(&final_state_snapshot),
        )?;

    authorship_log.metadata.base_commit_sha = amended_commit.to_string();

    // Fill unattributed lines for background agents
    if !matches!(
        crate::authorship::background_agent::detect(),
        crate::authorship::background_agent::BackgroundAgent::None
            | crate::authorship::background_agent::BackgroundAgent::WithHooks { .. }
    ) {
        let diff_base = if parent_sha == "initial" {
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        } else {
            &parent_sha
        };
        if let Ok(added_lines) = repo.diff_added_lines(diff_base, amended_commit, None) {
            let committed_hunks: HashMap<
                String,
                Vec<crate::authorship::authorship_log::LineRange>,
            > = added_lines
                .into_iter()
                .filter(|(_, lines)| !lines.is_empty())
                .map(|(path, lines)| {
                    (
                        path,
                        crate::authorship::authorship_log::LineRange::compress_lines(&lines),
                    )
                })
                .collect();
            crate::authorship::background_agent::fill_unattributed_lines(
                &mut authorship_log,
                &committed_hunks,
                &human_author,
            );
        }
    }

    let recovery_hunks = recovery_committed_hunks(repo, &parent_sha, amended_commit, None)?;
    crate::authorship::attribution_recovery::recover_attribution(
        repo,
        &parent_sha,
        amended_commit,
        &human_author,
        &mut authorship_log,
        &recovery_hunks,
        AttributionRecoveryContext {
            file_timestamps: recovery_file_timestamps,
            before_external_recovery,
        },
    )?;
    authorship_log.metadata.base_commit_sha = amended_commit.to_string();

    // Preserve human/session metadata from the original commit's note. Read
    // through notes_api so the HTTP notes backend is honored — with refs-only
    // reads the amended note keeps its s_/h_ attestation hashes but silently
    // loses the sessions/humans records they resolve through.
    if let Ok(original_log) = crate::git::notes_api::read_authorship_v3(repo, original_commit) {
        for (id, record) in original_log.metadata.humans {
            authorship_log.metadata.humans.entry(id).or_insert(record);
        }
        let referenced_session_ids: HashSet<String> = authorship_log
            .attestations
            .iter()
            .flat_map(|fa| fa.entries.iter())
            .filter_map(|entry| {
                if entry.hash.starts_with("s_") {
                    Some(
                        entry
                            .hash
                            .split("::")
                            .next()
                            .unwrap_or(&entry.hash)
                            .to_string(),
                    )
                } else {
                    None
                }
            })
            .collect();
        for (id, record) in original_log.metadata.sessions {
            if referenced_session_ids.contains(&id) {
                authorship_log.metadata.sessions.entry(id).or_insert(record);
            }
        }
    }

    // Inject custom attributes
    let custom_attrs = Config::fresh().custom_attributes().clone();
    if !custom_attrs.is_empty() {
        for pr in authorship_log.metadata.prompts.values_mut() {
            pr.custom_attributes = Some(custom_attrs.clone());
        }
        for sr in authorship_log.metadata.sessions.values_mut() {
            sr.custom_attributes = Some(custom_attrs.clone());
        }
    }

    let authorship_note_str = authorship_log
        .serialize_to_string()
        .map_err(|_| GitAiError::Generic("Failed to serialize authorship log".to_string()))?;
    write_note(repo, amended_commit, &authorship_note_str)?;

    // Write INITIAL file for uncommitted attributions
    if !initial_attributions.files.is_empty() {
        let new_working_log = repo_storage.working_log_for_base_commit(amended_commit)?;
        new_working_log.write_initial_attributions_with_contents(
            initial_attributions.files,
            initial_attributions.prompts,
            initial_attributions.humans,
            initial_file_contents,
            initial_attributions.sessions,
        )?;
    }

    // Clean up old working log
    repo_storage.delete_working_log_for_base_commit(original_commit)?;

    Ok(PostCommitAmendResult {
        commit_sha: amended_commit.to_string(),
        authorship_log,
        authorship_note: authorship_note_str,
        parent_sha,
    })
}

#[derive(Debug, Clone)]
enum StatsSkipReason {
    MergeCommit,
    Expensive(StatsCostEstimate),
}

#[doc(hidden)]
pub fn should_skip_expensive_post_commit_stats(estimate: &StatsCostEstimate) -> bool {
    estimate.hunk_ranges >= STATS_SKIP_MAX_HUNKS
        || estimate.added_lines >= STATS_SKIP_MAX_ADDED_LINES
        || estimate.files_with_additions >= STATS_SKIP_MAX_FILES_WITH_ADDITIONS
        || estimate.deleted_lines >= STATS_SKIP_MAX_DELETED_LINES
}

/// Public result of the stats cost estimate for a commit, used by the async
/// wrapper path to decide whether to skip expensive stats computation.
pub struct StatsSkipEstimate {
    should_skip: bool,
}

impl StatsSkipEstimate {
    pub fn should_skip(&self) -> bool {
        self.should_skip
    }
}

/// Estimate whether stats computation for `commit_sha` would be too expensive.
/// Resolves the parent commit automatically. Intended for callers outside the
/// normal post-commit flow (e.g. the async wrapper path).
pub fn estimate_stats_cost_for_head(
    repo: &Repository,
    commit_sha: &str,
    ignore_patterns: &[String],
) -> Result<StatsSkipEstimate, GitAiError> {
    let commit = repo.find_commit(commit_sha.to_string())?;
    let parent_sha = if commit.parent_count().unwrap_or(0) > 0 {
        commit
            .parent(0)
            .map(|p| p.id())
            .unwrap_or_else(|_| "initial".to_string())
    } else {
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()
    };
    estimate_stats_cost_for_commit_range(repo, &parent_sha, commit_sha, ignore_patterns)
}

pub fn estimate_stats_cost_for_commit_range(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    ignore_patterns: &[String],
) -> Result<StatsSkipEstimate, GitAiError> {
    let estimate = estimate_stats_cost(repo, parent_sha, commit_sha, ignore_patterns)?;
    Ok(StatsSkipEstimate {
        should_skip: should_skip_expensive_post_commit_stats(&estimate),
    })
}

fn estimate_stats_cost(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    ignore_patterns: &[String],
) -> Result<StatsCostEstimate, GitAiError> {
    let (mut added_lines_by_file, total_deleted_lines) =
        repo.diff_added_lines_with_deleted_count(parent_sha, commit_sha)?;
    let ignore_matcher = build_ignore_matcher(ignore_patterns);
    added_lines_by_file
        .retain(|file_path, _| !should_ignore_file_with_matcher(file_path, &ignore_matcher));

    let files_with_additions = added_lines_by_file
        .values()
        .filter(|lines| !lines.is_empty())
        .count();

    let mut added_lines = 0usize;
    let mut hunk_ranges = 0usize;

    for (_file, lines) in added_lines_by_file {
        if lines.is_empty() {
            continue;
        }
        added_lines += lines.len();
        hunk_ranges += count_line_ranges(&lines);
    }

    Ok(StatsCostEstimate {
        files_with_additions,
        added_lines,
        hunk_ranges,
        deleted_lines: total_deleted_lines,
    })
}

#[doc(hidden)]
pub fn count_line_ranges(lines: &[u32]) -> usize {
    if lines.is_empty() {
        return 0;
    }

    let mut sorted = lines.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut ranges = 1usize;
    let mut prev = sorted[0];
    for &line in &sorted[1..] {
        if line != prev + 1 {
            ranges += 1;
        }
        prev = line;
    }
    ranges
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MetricToolModelBreakdown {
    pub tool_model_pairs: Vec<String>,
    pub ai_additions: Vec<u32>,
    pub ai_accepted: Vec<u32>,
}

/// Build the metrics tool/model arrays and remove mock_ai test data.
/// Returns None when the entire event would only represent mock_ai data.
pub(crate) fn metric_tool_model_breakdown(
    stats: &crate::authorship::stats::CommitStats,
) -> Option<MetricToolModelBreakdown> {
    let only_mock_ai = !stats.tool_model_breakdown.is_empty()
        && stats
            .tool_model_breakdown
            .keys()
            .all(|k| k.starts_with("mock_ai::"));
    if only_mock_ai {
        return None;
    }

    let mut agg_ai = stats.ai_additions;
    let mut agg_accepted = stats.ai_accepted;
    for (key, ts) in &stats.tool_model_breakdown {
        if key.starts_with("mock_ai::") {
            agg_ai = agg_ai.saturating_sub(ts.ai_additions);
            agg_accepted = agg_accepted.saturating_sub(ts.ai_accepted);
        }
    }

    let mut tool_model_pairs: Vec<String> = vec!["all".to_string()];
    let mut ai_additions: Vec<u32> = vec![agg_ai];
    let mut ai_accepted: Vec<u32> = vec![agg_accepted];

    for (tool_model, tool_stats) in &stats.tool_model_breakdown {
        if tool_model.starts_with("mock_ai::") {
            continue;
        }
        tool_model_pairs.push(tool_model.clone());
        ai_additions.push(tool_stats.ai_additions);
        ai_accepted.push(tool_stats.ai_accepted);
    }

    Some(MetricToolModelBreakdown {
        tool_model_pairs,
        ai_additions,
        ai_accepted,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CommitMetricMetadata {
    pub subject: Option<String>,
    pub body: Option<String>,
    pub author_ts: Option<u64>,
    pub commit_ts: Option<u64>,
}

pub(crate) fn commit_metric_metadata(
    repo: &Repository,
    commit_sha: &str,
) -> Result<CommitMetricMetadata, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "show".to_string(),
        "-s".to_string(),
        "--no-notes".to_string(),
        "--encoding=UTF-8".to_string(),
        "--format=%s%x00%b%x00%at%x00%ct".to_string(),
        commit_sha.to_string(),
    ]);
    let output = exec_git(&args)?;
    let stdout = String::from_utf8(output.stdout)?;
    Ok(parse_commit_metric_metadata_output(&stdout))
}

fn parse_commit_metric_metadata_output(output: &str) -> CommitMetricMetadata {
    let mut parts = output.splitn(4, '\0');
    let Some(subject) = parts.next() else {
        return CommitMetricMetadata::default();
    };
    let Some(body) = parts.next() else {
        return CommitMetricMetadata::default();
    };
    let Some(author_ts) = parts.next() else {
        return CommitMetricMetadata::default();
    };
    let Some(commit_ts) = parts.next() else {
        return CommitMetricMetadata::default();
    };

    let subject = subject.trim().to_string();
    let body = body.trim().to_string();

    CommitMetricMetadata {
        subject: Some(subject),
        body: (!body.is_empty()).then_some(body),
        author_ts: author_ts.trim().parse::<u64>().ok(),
        commit_ts: commit_ts.trim().parse::<u64>().ok(),
    }
}

pub(crate) fn stable_patch_id_for_commit(repo: &Repository, commit_sha: &str) -> Option<String> {
    crate::authorship::rewrite_cherry_pick::stable_patch_ids_for_commits(
        repo,
        &[commit_sha.to_string()],
    )
    .ok()
    .and_then(|patch_ids| patch_ids.get(commit_sha).cloned())
}

pub(crate) fn commit_metric_attrs(
    repo: &Repository,
    commit_sha: &str,
    parent_sha: &str,
    human_author: &str,
) -> crate::metrics::EventAttributes {
    let mut attrs = crate::metrics::EventAttributes::with_version(env!("CARGO_PKG_VERSION"))
        .author(human_author)
        .commit_sha(commit_sha)
        .base_commit_sha(parent_sha);

    if let Ok(Some(remote_name)) = repo.get_default_remote()
        && let Ok(remotes) = repo.remotes_with_urls()
        && let Some((_, url)) = remotes.into_iter().find(|(n, _)| n == &remote_name)
        && let Ok(normalized) = crate::repo_url::normalize_repo_url(&url)
    {
        attrs = attrs.repo_url(normalized);
    }

    if let Ok(head_ref) = repo.head()
        && let Ok(short_branch) = head_ref.shorthand()
    {
        attrs = attrs.branch(short_branch);
    }

    attrs.custom_attributes_map(Config::fresh().custom_attributes())
}

/// Record metrics for a committed change.
/// This is a best-effort operation - failures are silently ignored.
#[allow(clippy::too_many_arguments)]
fn record_commit_metrics(
    repo: &Repository,
    commit_sha: &str,
    parent_sha: &str,
    human_author: &str,
    authorship_note: &str,
    stats: &crate::authorship::stats::CommitStats,
    checkpoints: &[Checkpoint],
    hunks_json: Option<&str>,
) {
    use crate::metrics::{CommittedValues, record};

    let Some(breakdown) = metric_tool_model_breakdown(stats) else {
        return;
    };

    // Build values with all stats
    let values = CommittedValues::new()
        .human_additions(stats.human_additions)
        .git_diff_deleted_lines(stats.git_diff_deleted_lines)
        .git_diff_added_lines(stats.git_diff_added_lines)
        .tool_model_pairs(breakdown.tool_model_pairs)
        .ai_additions(breakdown.ai_additions)
        .ai_accepted(breakdown.ai_accepted);

    // Add first checkpoint timestamp (null if no checkpoints)
    let values = if let Some(first) = checkpoints.first() {
        values.first_checkpoint_ts(first.timestamp)
    } else {
        values.first_checkpoint_ts_null()
    };

    let metadata = commit_metric_metadata(repo, commit_sha).unwrap_or_default();
    let values = match metadata.subject {
        Some(subject) => values.commit_subject(subject),
        None => values.commit_subject_null(),
    };
    let values = match metadata.body {
        Some(body) => values.commit_body(body),
        None => values.commit_body_null(),
    };
    let values = match metadata.author_ts {
        Some(author_ts) => values.author_ts(author_ts),
        None => values.author_ts_null(),
    };
    let values = match metadata.commit_ts {
        Some(commit_ts) => values.commit_ts(commit_ts),
        None => values.commit_ts_null(),
    };
    let values = match stable_patch_id_for_commit(repo, commit_sha) {
        Some(patch_id) => values.patch_id(patch_id),
        None => values.patch_id_null(),
    }
    .authorship_note(authorship_note);

    let values = if let Some(hunks) = hunks_json {
        values.hunks(hunks)
    } else {
        values.hunks_null()
    };

    let attrs = commit_metric_attrs(repo, commit_sha, parent_sha, human_author);

    // Record the metric
    record(values, attrs);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_commit_metric_metadata_output_reads_subject_body_and_timestamps() {
        let metadata = parse_commit_metric_metadata_output(concat!(
            "Subject line",
            "\0",
            "Body line one\n\nBody line two",
            "\0",
            "1704067200",
            "\0",
            "1704067260\n"
        ));

        assert_eq!(metadata.subject, Some("Subject line".to_string()));
        assert_eq!(
            metadata.body,
            Some("Body line one\n\nBody line two".to_string())
        );
        assert_eq!(metadata.author_ts, Some(1_704_067_200));
        assert_eq!(metadata.commit_ts, Some(1_704_067_260));
    }

    #[test]
    fn parse_commit_metric_metadata_output_uses_null_body_for_empty_body() {
        let metadata = parse_commit_metric_metadata_output(concat!(
            "Subject line",
            "\0",
            "",
            "\0",
            "1704067200",
            "\0",
            "1704067260\n"
        ));

        assert_eq!(metadata.subject, Some("Subject line".to_string()));
        assert_eq!(metadata.body, None);
        assert_eq!(metadata.author_ts, Some(1_704_067_200));
        assert_eq!(metadata.commit_ts, Some(1_704_067_260));
    }

    #[test]
    fn test_count_line_ranges_handles_scattered_and_contiguous_lines() {
        assert_eq!(count_line_ranges(&[]), 0);
        assert_eq!(count_line_ranges(&[1]), 1);
        assert_eq!(count_line_ranges(&[1, 2, 3]), 1);
        assert_eq!(count_line_ranges(&[1, 3, 5]), 3);
        // Includes unsorted and duplicate values.
        assert_eq!(count_line_ranges(&[5, 3, 3, 4, 10]), 2);
    }

    #[test]
    fn test_should_skip_expensive_post_commit_stats_thresholds() {
        let below_threshold = StatsCostEstimate {
            files_with_additions: STATS_SKIP_MAX_FILES_WITH_ADDITIONS - 1,
            added_lines: STATS_SKIP_MAX_ADDED_LINES - 1,
            hunk_ranges: STATS_SKIP_MAX_HUNKS - 1,
            deleted_lines: STATS_SKIP_MAX_DELETED_LINES - 1,
        };
        assert!(!should_skip_expensive_post_commit_stats(&below_threshold));

        let by_hunks = StatsCostEstimate {
            files_with_additions: 1,
            added_lines: 1,
            hunk_ranges: STATS_SKIP_MAX_HUNKS,
            deleted_lines: 0,
        };
        assert!(should_skip_expensive_post_commit_stats(&by_hunks));

        let by_added_lines = StatsCostEstimate {
            files_with_additions: 1,
            added_lines: STATS_SKIP_MAX_ADDED_LINES,
            hunk_ranges: 1,
            deleted_lines: 0,
        };
        assert!(should_skip_expensive_post_commit_stats(&by_added_lines));

        let by_files = StatsCostEstimate {
            files_with_additions: STATS_SKIP_MAX_FILES_WITH_ADDITIONS,
            added_lines: 1,
            hunk_ranges: 1,
            deleted_lines: 0,
        };
        assert!(should_skip_expensive_post_commit_stats(&by_files));

        let by_deleted_lines = StatsCostEstimate {
            files_with_additions: 0,
            added_lines: 0,
            hunk_ranges: 0,
            deleted_lines: STATS_SKIP_MAX_DELETED_LINES,
        };
        assert!(should_skip_expensive_post_commit_stats(&by_deleted_lines));
    }

    #[test]
    fn test_count_line_ranges_single_element() {
        assert_eq!(count_line_ranges(&[42]), 1);
    }

    #[test]
    fn test_count_line_ranges_all_contiguous() {
        assert_eq!(count_line_ranges(&[1, 2, 3, 4, 5]), 1);
    }

    #[test]
    fn test_count_line_ranges_all_scattered() {
        assert_eq!(count_line_ranges(&[1, 10, 20, 30]), 4);
    }

    #[test]
    fn test_count_line_ranges_duplicates() {
        assert_eq!(count_line_ranges(&[5, 5, 5]), 1);
    }

    #[test]
    fn test_count_line_ranges_unsorted() {
        // After sort+dedup: [1, 2, 5, 6, 10] -> ranges: [1,2], [5,6], [10]
        assert_eq!(count_line_ranges(&[10, 5, 6, 1, 2]), 3);
    }

    #[test]
    fn test_metric_tool_model_breakdown_filters_mock_ai() {
        use crate::authorship::stats::{CommitStats, ToolModelHeadlineStats};

        let mut tool_model_breakdown = std::collections::BTreeMap::new();
        tool_model_breakdown.insert(
            "mock_ai::unknown".to_string(),
            ToolModelHeadlineStats {
                ai_additions: 4,
                ai_accepted: 3,
            },
        );
        tool_model_breakdown.insert(
            "codex::gpt-5".to_string(),
            ToolModelHeadlineStats {
                ai_additions: 6,
                ai_accepted: 5,
            },
        );
        let stats = CommitStats {
            ai_additions: 10,
            ai_accepted: 8,
            tool_model_breakdown,
            ..Default::default()
        };

        let result = metric_tool_model_breakdown(&stats).unwrap();

        assert_eq!(result.tool_model_pairs, vec!["all", "codex::gpt-5"]);
        assert_eq!(result.ai_additions, vec![6, 6]);
        assert_eq!(result.ai_accepted, vec![5, 5]);
    }

    #[test]
    fn test_metric_tool_model_breakdown_skips_mock_only() {
        use crate::authorship::stats::{CommitStats, ToolModelHeadlineStats};

        let mut tool_model_breakdown = std::collections::BTreeMap::new();
        tool_model_breakdown.insert(
            "mock_ai::unknown".to_string(),
            ToolModelHeadlineStats {
                ai_additions: 4,
                ai_accepted: 3,
            },
        );
        let stats = CommitStats {
            ai_additions: 4,
            ai_accepted: 3,
            tool_model_breakdown,
            ..Default::default()
        };

        assert_eq!(metric_tool_model_breakdown(&stats), None);
    }

    #[test]
    fn test_count_line_ranges_two_ranges() {
        assert_eq!(count_line_ranges(&[1, 2, 3, 10, 11, 12]), 2);
    }

    #[test]
    fn test_should_skip_stats_exactly_at_thresholds() {
        // Exactly at the hunks threshold alone should trigger skip.
        let at_hunks = StatsCostEstimate {
            files_with_additions: 0,
            added_lines: 0,
            hunk_ranges: STATS_SKIP_MAX_HUNKS,
            deleted_lines: 0,
        };
        assert!(
            should_skip_expensive_post_commit_stats(&at_hunks),
            "Exactly at hunk threshold should skip"
        );

        // Exactly at added-lines threshold alone should trigger skip.
        let at_added = StatsCostEstimate {
            files_with_additions: 0,
            added_lines: STATS_SKIP_MAX_ADDED_LINES,
            hunk_ranges: 0,
            deleted_lines: 0,
        };
        assert!(
            should_skip_expensive_post_commit_stats(&at_added),
            "Exactly at added-lines threshold should skip"
        );

        // Exactly at files-with-additions threshold alone should trigger skip.
        let at_files = StatsCostEstimate {
            files_with_additions: STATS_SKIP_MAX_FILES_WITH_ADDITIONS,
            added_lines: 0,
            hunk_ranges: 0,
            deleted_lines: 0,
        };
        assert!(
            should_skip_expensive_post_commit_stats(&at_files),
            "Exactly at files-with-additions threshold should skip"
        );

        // Exactly at deleted-lines threshold alone should trigger skip.
        let at_deleted = StatsCostEstimate {
            files_with_additions: 0,
            added_lines: 0,
            hunk_ranges: 0,
            deleted_lines: STATS_SKIP_MAX_DELETED_LINES,
        };
        assert!(
            should_skip_expensive_post_commit_stats(&at_deleted),
            "Exactly at deleted-lines threshold should skip"
        );

        // All at zero should NOT skip.
        let all_zero = StatsCostEstimate {
            files_with_additions: 0,
            added_lines: 0,
            hunk_ranges: 0,
            deleted_lines: 0,
        };
        assert!(
            !should_skip_expensive_post_commit_stats(&all_zero),
            "All zero values should not skip"
        );
    }
}
