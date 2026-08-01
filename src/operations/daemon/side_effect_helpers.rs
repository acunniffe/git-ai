use super::parent_diff_batch::{
    ParentDiffBatch, ParentDiffsByCommit, rewrite_metric_commits_with_parent_diffs,
};
use crate::checkpoint_content_budget::CheckpointContentBudget;
use crate::config;
use crate::error::GitAiError;
use crate::model::authorship_log_serialization::AuthorshipLog;
use crate::model::checkpoint_request::CheckpointRequest;
use crate::operations::git::cli_parser::{ParsedGitInvocation, parse_git_cli_args};
use crate::operations::git::find_repository_in_path;
use crate::operations::git::repository::{Repository, discover_repository_in_path_no_git_exec};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub fn matches_any_pathspec(file: &str, pathspecs: &[String]) -> bool {
    pathspecs.iter().any(|pathspec| {
        file == pathspec
            || (pathspec.ends_with('/') && file.starts_with(pathspec))
            || file.starts_with(&format!("{}/", pathspec))
    })
}

pub fn resolve_stash_sha(cmd: &crate::model::domain::NormalizedCommand) -> Option<&str> {
    cmd.stash_target_oid.as_deref().or_else(|| {
        cmd.ref_changes
            .iter()
            .find(|rc| rc.reference == "refs/stash")
            .map(|rc| rc.old.as_str())
            .filter(|s| !s.is_empty() && *s != "0000000000000000000000000000000000000000")
    })
}

pub fn stash_base_head(repo: &Repository, stash_sha: &str) -> Option<String> {
    repo.find_commit(stash_sha.to_string())
        .ok()
        .and_then(|commit| commit.parent(0).ok())
        .map(|parent| parent.id().to_string())
}

pub(crate) fn commit_parent_pairs_from_log(log_output: &str) -> Vec<(String, String)> {
    log_output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            Some((parts.next()?.to_string(), parts.next()?.to_string()))
        })
        .collect()
}

/// After a rebase completes, check if any newly-rebased commits were created
/// from conflict resolution with AI checkpoints. If so, merge those resolution
/// checkpoints into the already-shifted source authorship note for the new commit.
#[derive(Default)]
pub struct RewriteMetricContext {
    parent_diffs_by_commit: ParentDiffsByCommit,
}

pub fn process_conflict_resolution_working_logs(
    repo: &Repository,
    new_tip: &str,
    onto: Option<&str>,
) -> Result<RewriteMetricContext, GitAiError> {
    let onto_sha = match onto {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(RewriteMetricContext::default()),
    };

    // Walk rebased commits between onto and new_tip
    let mut args = repo.global_args_for_exec();
    args.extend([
        "log".to_string(),
        "--format=%H %P".to_string(),
        format!("{}..{}", onto_sha, new_tip),
    ]);
    let output = crate::clients::git_cli::exec_git(&args)?;
    let log_output = String::from_utf8_lossy(&output.stdout);

    let commit_parent_pairs = commit_parent_pairs_from_log(&log_output);
    let commit_shas = commit_parent_pairs
        .iter()
        .map(|(commit_sha, _)| commit_sha.clone())
        .collect::<Vec<_>>();
    let collect_metric_context = crate::operations::authorship::rewrite::rewrite_metrics_enabled();
    let existing_notes = crate::operations::git::notes_api::read_notes_batch(repo, &commit_shas)?;
    let author = repo.effective_author_identity().formatted_or_unknown();

    // Only commits whose rebased parent still has a working log incur
    // attribution reconstruction; restrict the (expensive) parent->commit diffs
    // to those. Compute ALL of them in ONE batched diff-tree so the per-commit
    // loop below performs no per-commit git spawns.
    let parent_diff_batch = ParentDiffBatch::compute(repo, commit_parent_pairs)?;
    {
        let diff_by_commit = parent_diff_batch.borrowed_diffs_by_commit();
        flush_pending_note_writes(
            repo,
            parent_diff_batch.commit_parent_pairs(),
            &existing_notes,
            author,
            &diff_by_commit,
        )?;
    }

    Ok(if collect_metric_context {
        RewriteMetricContext {
            parent_diffs_by_commit: parent_diff_batch.into_owned_by_commit(),
        }
    } else {
        RewriteMetricContext::default()
    })
}

pub(crate) fn rewrite_metric_commits_with_context(
    metric_commits: Vec<crate::operations::authorship::rewrite::RewriteMetricCommit>,
    context: RewriteMetricContext,
) -> Vec<crate::operations::authorship::rewrite::RewriteMetricCommit> {
    rewrite_metric_commits_with_parent_diffs(metric_commits, context.parent_diffs_by_commit)
}

/// Collect deferred note-writes for conflict-resolved commits and flush them in a
/// single `write_notes_batch` call, then delete the corresponding working logs.
///
/// This is the shared kernel for both the rebase and cherry-pick conflict-resolution
/// loops.  It enforces the durability invariant: working-log deletion for commit K
/// only occurs after K's authorship note has been durably written.
///
/// Error semantics: if collection fails mid-loop, any already-collected pairs are
/// flushed before returning the original collection error (flush failures on this
/// path are logged via `tracing::debug!` rather than promoted, so the original
/// per-commit error is always what the caller sees).
pub(crate) fn flush_pending_note_writes(
    repo: &Repository,
    commit_parent_pairs: &[(String, String)],
    existing_notes: &std::collections::HashMap<String, String>,
    author: String,
    diff_by_commit: &std::collections::HashMap<
        &str,
        &crate::operations::authorship::rewrite::DiffTreeResult,
    >,
) -> Result<(), GitAiError> {
    // Collect (note_entry, parent_sha) pairs; stop on first per-commit error.
    let mut pending: Vec<(crate::operations::git::notes_api::NoteWriteEntry, String)> = Vec::new();
    let mut collection_error: Option<GitAiError> = None;
    for (commit_sha, parent_sha) in commit_parent_pairs {
        let existing_shifted_log = existing_notes
            .get(commit_sha)
            .and_then(|raw| AuthorshipLog::deserialize_from_string(raw).ok());
        match post_conflict_resolution_working_log(
            repo,
            parent_sha,
            commit_sha,
            author.clone(),
            existing_shifted_log,
            diff_by_commit.get(commit_sha.as_str()).copied(),
        ) {
            Ok(Some(entry)) => pending.push((entry, parent_sha.clone())),
            Ok(None) => {}
            Err(e) => {
                collection_error = Some(e);
                break;
            }
        }
    }

    // Flush whatever was collected.  On collection failure this preserves notes
    // for commits processed before the error; on collection success this is the
    // only flush.  Working-log deletion follows only on flush success.
    if !pending.is_empty() {
        let note_entries: Vec<_> = pending.iter().map(|(entry, _)| entry.clone()).collect();
        match crate::operations::git::notes_api::write_notes_batch(repo, &note_entries) {
            Ok(()) => {
                // Notes are durable; it is now safe to delete the working logs.
                for (_, parent_sha) in &pending {
                    repo.storage
                        .delete_working_log_for_base_commit(parent_sha)?;
                }
            }
            Err(flush_err) => {
                if let Some(ref e) = collection_error {
                    // Original collection error takes priority; log the flush failure.
                    tracing::debug!(
                        "write_notes_batch failed after collection error ({}): {}",
                        e,
                        flush_err
                    );
                } else {
                    return Err(flush_err);
                }
            }
        }
    }

    if let Some(e) = collection_error {
        return Err(e);
    }
    Ok(())
}

/// Reconstruct authorship for a single conflict-resolved commit and return the
/// deferred `(commit_sha, serialized_note)` entry without writing it.
///
/// Returns `Ok(None)` when `parent_sha` has no working log (nothing to write).
/// The caller is responsible for flushing collected entries and deleting working
/// logs only after a successful flush (use `flush_pending_note_writes`).
pub(crate) fn post_conflict_resolution_working_log(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    author: String,
    existing_shifted_log: Option<AuthorshipLog>,
    precomputed_parent_diff: Option<&crate::operations::authorship::rewrite::DiffTreeResult>,
) -> Result<Option<crate::operations::git::notes_api::NoteWriteEntry>, GitAiError> {
    if !repo.storage.has_working_log(parent_sha) {
        return Ok(None);
    }

    let commit_for_transform = commit_sha.to_string();
    let result =
        crate::operations::authorship::post_commit::post_commit_from_working_log_with_transform_context_detailed(
            repo,
            Some(parent_sha.to_string()),
            commit_sha.to_string(),
            author,
            crate::operations::authorship::post_commit::PostCommitOptions {
                supress_output: true,
                compute_stats: false,
                recover_attribution: false,
                defer_note_write: true,
            },
            crate::operations::authorship::post_commit::PostCommitContext {
                precomputed_parent_diff,
                recovery_file_timestamps: None,
                before_external_recovery: None,
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
    Ok(Some((result.commit_sha, result.authorship_note)))
}

pub fn apply_checkpoint_side_effect(mut request: CheckpointRequest) -> Result<(), GitAiError> {
    if request.files.is_empty() {
        return Ok(());
    }

    let repo_work_dir = &request.files[0].repo_work_dir;
    let repo = match discover_repository_in_path_no_git_exec(repo_work_dir) {
        Ok(repo) => repo,
        Err(e) => {
            if request.checkpoint_kind.is_ai()
                && let Some(ref agent_id) = request.agent_id
                && crate::operations::daemon::checkpoint::should_emit_agent_usage(agent_id)
            {
                let attrs =
                    crate::operations::daemon::checkpoint::build_agent_usage_attrs(None, agent_id);
                let values = crate::metrics::AgentUsageValues::new();
                crate::metrics::record(values, attrs);
            }
            return Err(e);
        }
    };
    let author = repo.effective_author_identity().formatted_or_unknown();

    if request.checkpoint_kind.is_ai()
        && let Some(ref agent_id) = request.agent_id
        && crate::operations::daemon::checkpoint::should_emit_agent_usage(agent_id)
    {
        let attrs =
            crate::operations::daemon::checkpoint::build_agent_usage_attrs(Some(&repo), agent_id);
        let values = crate::metrics::AgentUsageValues::new();
        crate::metrics::record(values, attrs);
    }

    let resolved = resolve_checkpoint_request(&repo, &mut request)?;
    let Some(resolved) = resolved else {
        return Ok(());
    };

    crate::operations::daemon::checkpoint::execute_resolved_checkpoint_from_daemon(
        &repo,
        &author,
        request.checkpoint_kind,
        request,
        resolved,
    )
}

pub fn resolve_checkpoint_request(
    repo: &crate::operations::git::repository::Repository,
    request: &mut CheckpointRequest,
) -> Result<Option<crate::operations::daemon::checkpoint::ResolvedCheckpointExecution>, GitAiError>
{
    use crate::model::checkpoint_request::BaseCommit;
    use crate::operations::authorship::ignore::{
        build_ignore_matcher, effective_ignore_patterns, should_ignore_file_with_matcher,
    };
    use crate::operations::git::path_format::normalize_to_posix;

    let Some(first_file) = request.files.first() else {
        return Ok(None);
    };
    let base_commit = match &first_file.base_commit {
        BaseCommit::Sha(sha) => sha.clone(),
        BaseCommit::Initial => "initial".to_string(),
    };

    let repo_workdir = repo.workdir()?;
    let canonical_workdir =
        crate::operations::git::canonicalize::canonicalize_or_self(&repo_workdir);
    let ignore_patterns = effective_ignore_patterns(repo, &[], &[]);
    let ignore_matcher = build_ignore_matcher(&ignore_patterns);

    let mut files = Vec::new();
    let mut dirty_files: HashMap<String, Arc<str>> = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    let config = config::Config::fresh();
    let mut content_budget = CheckpointContentBudget::from_config(&config);

    for file in &mut request.files {
        let path_str = file.path.to_string_lossy();
        let path_str = path_str.trim();
        if path_str.is_empty() {
            continue;
        }

        let abs_path = if file.path.is_absolute() {
            file.path.clone()
        } else {
            repo_workdir.join(&*file.path)
        };
        if !repo.path_is_in_workdir(&abs_path) {
            continue;
        }

        let relative_path = crate::operations::git::canonicalize::canonicalize_or_self(&abs_path)
            .strip_prefix(&canonical_workdir)
            .map(|p| normalize_to_posix(&p.to_string_lossy()))
            .unwrap_or_else(|_| {
                abs_path
                    .strip_prefix(&repo_workdir)
                    .map(|p| normalize_to_posix(&p.to_string_lossy()))
                    .unwrap_or_else(|_| normalize_to_posix(path_str))
            });

        if !seen.insert(relative_path.clone()) {
            continue;
        }
        if should_ignore_file_with_matcher(&relative_path, &ignore_matcher) {
            continue;
        }

        if let Some(content) = std::mem::take(&mut file.content) {
            if content.as_bytes().contains(&0) {
                continue;
            }
            if !content_budget.reserve(&relative_path, &content) {
                continue;
            }
            dirty_files.insert(relative_path.clone(), Arc::from(content));
            files.push(relative_path);
        }
    }

    if files.is_empty() {
        return Ok(None);
    }

    let ts = crate::model::clock::now_millis();

    Ok(Some(
        crate::operations::daemon::checkpoint::ResolvedCheckpointExecution {
            base_commit,
            ts,
            files,
            dirty_files,
        },
    ))
}

pub fn compute_watermarks_from_stat(
    repo_working_dir: &str,
    file_paths: &[String],
) -> std::collections::HashMap<String, u128> {
    let repo_root = std::path::Path::new(repo_working_dir);
    let mut watermarks = std::collections::HashMap::new();
    for path in file_paths {
        let full_path = repo_root.join(path);
        if let Ok(metadata) = std::fs::symlink_metadata(&full_path)
            && let Ok(mtime) = metadata.modified()
        {
            let nanos = mtime
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            // Normalize watermark keys the same way bash_tool::normalize_path does
            // so that case-folded snapshot lookups on macOS/Windows find a match.
            let key = crate::operations::commands::checkpoint_agent::bash_tool::normalize_path(
                std::path::Path::new(path),
            )
            .to_string_lossy()
            .to_string();
            watermarks.insert(key, nanos);
        }
    }
    watermarks
}

pub fn capture_commit_file_timestamps(
    worktree: &Path,
    commit_sha: &str,
) -> Result<crate::operations::authorship::attribution_recovery::FileTimestampsByPath, GitAiError> {
    let repo = find_repository_in_path(&worktree.to_string_lossy())?;
    let workdir = repo.workdir()?;
    let files = repo.list_commit_files(commit_sha, None)?;
    let mut timestamps_by_path = HashMap::new();
    for file_path in files {
        let timestamps =
            crate::operations::authorship::attribution_recovery::file_timestamps_for_path(
                &workdir.join(&file_path),
            );
        if !timestamps.is_empty() {
            timestamps_by_path.insert(file_path, timestamps);
        }
    }
    Ok(timestamps_by_path)
}

pub fn parsed_invocation_for_side_effect(
    command: Option<&str>,
    args: &[String],
) -> ParsedGitInvocation {
    ParsedGitInvocation {
        global_args: Vec::new(),
        command: command.map(ToString::to_string),
        command_args: args.to_vec(),
        saw_end_of_opts: false,
        is_help: command == Some("help") || args.iter().any(|arg| arg == "-h" || arg == "--help"),
    }
}

pub fn parsed_invocation_for_normalized_command(
    cmd: &crate::model::domain::NormalizedCommand,
) -> ParsedGitInvocation {
    if !cmd.raw_argv.is_empty() {
        return parse_git_cli_args(super::trace_invocation_args(&cmd.raw_argv));
    }

    if cmd.primary_command.is_some() || !cmd.invoked_args.is_empty() {
        return parsed_invocation_for_side_effect(
            cmd.primary_command.as_deref(),
            &cmd.invoked_args,
        );
    }

    ParsedGitInvocation {
        global_args: Vec::new(),
        command: None,
        command_args: Vec::new(),
        saw_end_of_opts: false,
        is_help: false,
    }
}
