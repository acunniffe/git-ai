use crate::authorship::attribution_tracker::{
    Attribution, AttributionTracker, INITIAL_ATTRIBUTION_TS, LineAttribution,
};
use crate::authorship::authorship_log_serialization::generate_session_id;
#[cfg(not(any(test, feature = "test-support")))]
use crate::authorship::authorship_log_serialization::generate_short_hash;
use crate::authorship::imara_diff_utils::{
    LineChangeTag, compute_line_changes, content_eq_ignoring_line_endings,
};
use crate::authorship::working_log::CheckpointKind;
use crate::authorship::working_log::{Checkpoint, WorkingLogEntry};
use crate::commands::checkpoint_agent::orchestrator::CheckpointRequest;
use crate::error::GitAiError;
use crate::git::repo_storage::PersistedWorkingLog;
use crate::git::repository::Repository;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
#[cfg(not(any(test, feature = "test-support")))]
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-file line statistics (in-memory only, not persisted)
#[derive(Debug, Clone, Default)]
#[doc(hidden)]
pub struct FileLineStats {
    pub additions: u32,
    pub deletions: u32,
    pub additions_sloc: u32,
    pub deletions_sloc: u32,
}

/// Latest checkpoint state needed to process a file in the next checkpoint.
#[derive(Debug, Clone)]
struct PreviousFileState {
    blob_sha: String,
    attributions: Vec<Attribution>,
}

use crate::authorship::working_log::AgentId;

#[cfg_attr(any(test, feature = "test-support"), allow(dead_code))]
const AGENT_USAGE_MIN_INTERVAL_SECS: u64 = 150;

#[cfg(not(any(test, feature = "test-support")))]
const KNOWN_HUMAN_MIN_SECS_AFTER_AI: u64 = 1;

#[cfg(not(any(test, feature = "test-support")))]
fn should_emit_real_agent_usage(agent_id: &AgentId) -> bool {
    let prompt_id = generate_short_hash(&agent_id.id, &agent_id.tool);
    let now_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let Ok(db) = crate::metrics::db::MetricsDatabase::global() else {
        return true;
    };
    let Ok(mut db_lock) = db.lock() else {
        return true;
    };

    db_lock
        .should_emit_agent_usage(&prompt_id, now_ts, AGENT_USAGE_MIN_INTERVAL_SECS)
        .unwrap_or(true)
}

#[cfg(any(test, feature = "test-support"))]
fn should_emit_real_agent_usage(_agent_id: &AgentId) -> bool {
    false
}

pub(crate) fn should_emit_agent_usage(agent_id: &AgentId) -> bool {
    agent_id.tool != "mock_ai" && should_emit_real_agent_usage(agent_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparedPathRole {
    Edited,
    WillEdit,
}

#[derive(Debug, Clone)]
pub struct ResolvedCheckpointExecution {
    pub base_commit: String,
    pub ts: u128,
    pub files: Vec<String>,
    pub dirty_files: HashMap<String, Arc<str>>,
}

/// Build EventAttributes for AgentUsage events.
/// When repo is available, includes repo_url and branch. Always includes tool, model,
/// session_id, and custom attributes.
pub fn build_agent_usage_attrs(
    repo: Option<&Repository>,
    agent_id: &AgentId,
) -> crate::metrics::EventAttributes {
    let session_id = generate_session_id(&agent_id.id, &agent_id.tool);

    let mut attrs = crate::metrics::EventAttributes::with_version(env!("CARGO_PKG_VERSION"))
        .session_id(session_id)
        .tool(&agent_id.tool)
        .model(&agent_id.model)
        .external_session_id(&agent_id.id)
        .custom_attributes_map(crate::config::Config::fresh().custom_attributes());

    if let Some(repo) = repo {
        if let Some(url) = crate::repo_url::resolve_repo_url_from_repo(repo) {
            attrs = attrs.repo_url(url);
        }

        if let Ok(head_ref) = repo.head()
            && let Ok(short_branch) = head_ref.shorthand()
        {
            attrs = attrs.branch(short_branch);
        }
    }

    attrs
}

/// Build EventAttributes with repo metadata.
/// Reused for both AgentUsage and Checkpoint events.
fn build_checkpoint_attrs(
    repo: &Repository,
    base_commit: &str,
    agent_id: Option<&AgentId>,
) -> crate::metrics::EventAttributes {
    // Extract session_id from agent_id if available
    let session_id = agent_id
        .as_ref()
        .map(|aid| generate_session_id(&aid.id, &aid.tool))
        .unwrap_or_default();

    let mut attrs = crate::metrics::EventAttributes::with_version(env!("CARGO_PKG_VERSION"))
        .session_id(session_id)
        .base_commit_sha(base_commit);

    // Add AI-specific attributes
    if let Some(agent_id) = agent_id {
        attrs = attrs
            .tool(&agent_id.tool)
            .model(&agent_id.model)
            .external_session_id(&agent_id.id);
    }

    // Attach custom attributes using Config::fresh() to support runtime config updates
    attrs = attrs.custom_attributes_map(crate::config::Config::fresh().custom_attributes());

    // Add repo URL
    if let Some(url) = crate::repo_url::resolve_repo_url_from_repo(repo) {
        attrs = attrs.repo_url(url);
    }

    // Add branch
    if let Ok(head_ref) = repo.head()
        && let Ok(short_branch) = head_ref.shorthand()
    {
        attrs = attrs.branch(short_branch);
    }

    attrs
}

pub fn execute_resolved_checkpoint_from_daemon(
    repo: &Repository,
    author: &str,
    kind: CheckpointKind,
    checkpoint_request: CheckpointRequest,
    resolved: ResolvedCheckpointExecution,
) -> Result<(), GitAiError> {
    let checkpoint_start = Instant::now();
    tracing::debug!("[BENCHMARK] Starting daemon replay checkpoint");
    execute_resolved_checkpoint(
        repo,
        author,
        kind,
        true,
        checkpoint_request,
        resolved,
        checkpoint_start,
    )
    .map(|_| ())
}

fn execute_resolved_checkpoint(
    repo: &Repository,
    author: &str,
    kind: CheckpointKind,
    quiet: bool,
    checkpoint_request: CheckpointRequest,
    mut resolved: ResolvedCheckpointExecution,
    checkpoint_start: Instant,
) -> Result<(usize, usize, usize), GitAiError> {
    if kind.is_ai() && checkpoint_request.agent_id.is_none() {
        return Err(GitAiError::Generic(
            "AI checkpoint is missing agent_id".to_string(),
        ));
    }

    let mut working_log = repo
        .storage
        .working_log_for_base_commit(&resolved.base_commit)?;

    if !resolved.dirty_files.is_empty() {
        working_log.set_dirty_files(Some(std::mem::take(&mut resolved.dirty_files)));
    }

    let read_checkpoints_start = Instant::now();
    let mut checkpoints = working_log.read_all_checkpoints()?;
    tracing::debug!(
        "[BENCHMARK] Reading {} checkpoints took {:?}",
        checkpoints.len(),
        read_checkpoints_start.elapsed()
    );

    // Reject KnownHuman checkpoints that arrive within KNOWN_HUMAN_MIN_SECS_AFTER_AI
    // seconds of an AI checkpoint on any of the same files. These are likely spurious
    // IDE save events triggered by the AI completing its edit, not genuine human keystrokes.
    // Only compiled in non-test builds where the constant is non-zero; under --all-targets
    // clippy would otherwise flag the comparisons as always-false for u64.
    #[cfg(not(any(test, feature = "test-support")))]
    if kind == CheckpointKind::KnownHuman {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let too_soon = checkpoints.iter().rev().any(|cp| {
            cp.kind.is_ai()
                && now_secs.saturating_sub(cp.timestamp) < KNOWN_HUMAN_MIN_SECS_AFTER_AI
                && cp.entries.iter().any(|e| resolved.files.contains(&e.file))
        });
        if too_soon {
            tracing::debug!(
                "[KnownHuman] Rejected: fired within {}s of an AI checkpoint on the same file",
                KNOWN_HUMAN_MIN_SECS_AFTER_AI
            );
            return Ok((0, 0, 0));
        }
    }

    let save_states_start = Instant::now();
    let file_content_hashes = save_current_file_states(&working_log, &resolved.files)?;
    tracing::debug!(
        "[BENCHMARK] save_current_file_states for {} files took {:?}",
        resolved.files.len(),
        save_states_start.elapsed()
    );

    let hash_compute_start = Instant::now();
    let mut ordered_hashes: Vec<_> = file_content_hashes.iter().collect();
    ordered_hashes.sort_by_key(|(file_path, _)| *file_path);

    let mut combined_hasher = Sha256::new();
    for (file_path, hash) in ordered_hashes {
        combined_hasher.update(file_path.as_bytes());
        combined_hasher.update(hash.as_bytes());
    }
    let combined_hash = format!("{:x}", combined_hasher.finalize());
    tracing::debug!(
        "[BENCHMARK] Hash computation took {:?}",
        hash_compute_start.elapsed()
    );

    let trace_id = checkpoint_request.trace_id.clone();

    let entries_start = Instant::now();
    let (entries, file_stats) = crate::tokio_runtime::block_on(get_checkpoint_entries(
        kind,
        author,
        repo,
        &working_log,
        &resolved.files,
        &file_content_hashes,
        &checkpoints,
        &checkpoint_request,
        resolved.ts,
        Some(resolved.base_commit.as_str()),
        trace_id.clone(),
    ))?;
    tracing::debug!(
        "[BENCHMARK] get_checkpoint_entries generated {} entries, took {:?}",
        entries.len(),
        entries_start.elapsed()
    );

    if !entries.is_empty() {
        let checkpoint_create_start = Instant::now();
        let mut checkpoint = Checkpoint::new(
            kind,
            combined_hash.clone(),
            author.to_string(),
            entries.clone(),
        );
        checkpoint.timestamp = (resolved.ts / 1000) as u64;
        checkpoint.line_stats = compute_line_stats(&file_stats)?;
        checkpoint.trace_id = Some(trace_id.clone());

        if kind.is_ai() {
            checkpoint.agent_id = checkpoint_request.agent_id.clone();
            checkpoint.agent_metadata = if checkpoint_request.metadata.is_empty() {
                None
            } else {
                Some(checkpoint_request.metadata.clone())
            };
        } else if kind == CheckpointKind::KnownHuman && !checkpoint_request.metadata.is_empty() {
            let editor = checkpoint_request
                .metadata
                .get("kh_editor")
                .cloned()
                .unwrap_or_default();
            let editor_version = checkpoint_request
                .metadata
                .get("kh_editor_version")
                .cloned()
                .unwrap_or_default();
            let extension_version = checkpoint_request
                .metadata
                .get("kh_extension_version")
                .cloned()
                .unwrap_or_default();
            if !editor.is_empty() {
                use crate::authorship::working_log::KnownHumanMetadata;
                checkpoint.known_human_metadata = Some(KnownHumanMetadata {
                    editor,
                    editor_version,
                    extension_version,
                });
            }
        }
        tracing::debug!(
            "[BENCHMARK] Checkpoint creation took {:?}",
            checkpoint_create_start.elapsed()
        );

        let append_start = Instant::now();
        working_log.append_checkpoint(&checkpoint)?;
        tracing::debug!(
            "[BENCHMARK] Appending checkpoint to working log took {:?}",
            append_start.elapsed()
        );
        checkpoints.push(checkpoint.clone());

        let mut attrs =
            build_checkpoint_attrs(repo, &resolved.base_commit, checkpoint.agent_id.as_ref());

        // Add trace_id to attributes - links all checkpoint events together
        if let Some(ref tid) = checkpoint.trace_id {
            attrs = attrs.trace_id(tid);
        }

        // Extract tool_use_id from metadata if available
        // tool_use_id tracks specific tool invocations (e.g., bash tool calls from AI agents)
        // Allows linking checkpoint events to the exact tool use that triggered them
        let tool_use_id = checkpoint_request
            .metadata
            .get("tool_use_id")
            .map(|s| s.as_str());

        let edit_kind = checkpoint_request
            .metadata
            .get("edit_kind")
            .map(|s| s.as_str());

        for (entry, file_stat) in entries.iter().zip(file_stats.iter()) {
            let mut values = crate::metrics::CheckpointValues::new()
                .checkpoint_ts(checkpoint.timestamp)
                .kind(checkpoint.kind.to_str().to_string())
                .file_path(entry.file.clone())
                .lines_added(file_stat.additions)
                .lines_deleted(file_stat.deletions)
                .lines_added_sloc(file_stat.additions_sloc)
                .lines_deleted_sloc(file_stat.deletions_sloc);

            if let Some(tuid) = tool_use_id {
                values = values.external_tool_use_id(tuid);
            }
            if let Some(ek) = edit_kind {
                values = values.edit_kind(ek);
            }

            let file_attrs = attrs.clone().author(&checkpoint.author);
            crate::metrics::record(values, file_attrs);
        }
    }

    let agent_tool = if kind.is_ai() {
        checkpoint_request
            .agent_id
            .as_ref()
            .map(|aid| aid.tool.as_str())
    } else {
        None
    };

    let label = if entries.len() > 1 {
        "checkpoint"
    } else {
        "commit"
    };

    if !quiet {
        let log_author = agent_tool.unwrap_or(author);
        let files_with_entries = entries.len();
        let total_uncommitted_files = resolved.files.len();

        if files_with_entries == total_uncommitted_files {
            eprintln!(
                "{} {} changed {} file(s) that have changed since the last {}",
                kind.to_str(),
                log_author,
                files_with_entries,
                label
            );
        } else {
            eprintln!(
                "{} {} changed {} of the {} file(s) that have changed since the last {} ({} already checkpointed)",
                kind.to_str(),
                log_author,
                files_with_entries,
                total_uncommitted_files,
                label,
                total_uncommitted_files - files_with_entries
            );
        }
    }

    tracing::debug!(
        "[BENCHMARK] Total checkpoint run took {:?}",
        checkpoint_start.elapsed()
    );
    Ok((entries.len(), resolved.files.len(), checkpoints.len()))
}

fn save_current_file_states(
    working_log: &PersistedWorkingLog,
    files: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    let _read_start = Instant::now();

    let blobs_dir = working_log.dir.join("blobs");
    let dirty_files = working_log.dirty_files.clone();
    let files = files.to_vec();

    let file_content_hashes = crate::tokio_runtime::block_on(async {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(8));
        let blobs_dir = Arc::new(blobs_dir);
        let dirty_files = Arc::new(dirty_files);

        let mut futures = Vec::with_capacity(files.len());
        for file_path in files {
            let blobs_dir = Arc::clone(&blobs_dir);
            let dirty_files = Arc::clone(&dirty_files);
            let semaphore = Arc::clone(&semaphore);

            futures.push(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("file state semaphore was closed");

                // Read file content - check dirty_files first, then filesystem
                let content = if let Some(ref dirty_map) = *dirty_files {
                    dirty_map.get(&file_path).cloned()
                } else {
                    None
                }
                .ok_or_else(|| {
                    GitAiError::Generic(format!(
                        "save_current_file_states: file '{}' not found in dirty_files snapshot (filesystem fallback is not allowed in checkpoint flow)",
                        file_path
                    ))
                })?;

                crate::tokio_runtime::spawn_blocking_result(move || {
                    // Create SHA256 hash of the content
                    let mut hasher = Sha256::new();
                    hasher.update(content.as_bytes());
                    let sha = format!("{:x}", hasher.finalize());

                    // Ensure blobs directory exists
                    std::fs::create_dir_all(&*blobs_dir)?;

                    // Write content to blob file
                    let blob_path = blobs_dir.join(&sha);
                    std::fs::write(blob_path, content.as_bytes())?;

                    Ok::<(String, String), GitAiError>((file_path, sha))
                })
                .await
            });
        }

        // Collect results from all concurrent operations
        let results: Vec<Result<(String, String), GitAiError>> =
            stream::iter(futures).buffer_unordered(8).collect().await;

        // Convert results into HashMap
        let mut file_content_hashes = HashMap::new();
        for result in results {
            let (file_path, content_hash) = result?;
            file_content_hashes.insert(file_path, content_hash);
        }

        Ok::<HashMap<String, String>, GitAiError>(file_content_hashes)
    })?;

    Ok(file_content_hashes)
}

fn get_previous_content_from_head(
    repo: &Repository,
    file_path: &str,
    head_tree_id: &Option<String>,
) -> Arc<str> {
    let Some(tree_id) = head_tree_id.as_ref() else {
        return Arc::from("");
    };
    match repo.read_file_blob_at_tree(tree_id, std::path::Path::new(file_path)) {
        Ok(content) => {
            let text = String::from_utf8_lossy(&content);
            Arc::from(text.into_owned())
        }
        Err(_) => Arc::from(""),
    }
}

#[doc(hidden)]
pub fn is_ai_author_id(author_id: &str) -> bool {
    author_id != "human" && !author_id.starts_with("h_")
}

fn working_log_entry_has_non_human_attribution(entry: &WorkingLogEntry) -> bool {
    entry
        .line_attributions
        .iter()
        .any(|attr| is_ai_author_id(&attr.author_id))
        || entry
            .attributions
            .iter()
            .any(|attr| is_ai_author_id(&attr.author_id))
}

fn build_previous_file_state_maps(
    previous_checkpoints: &[Checkpoint],
    initial_attributions: &HashMap<String, Vec<LineAttribution>>,
) -> (HashMap<String, PreviousFileState>, HashSet<String>) {
    let mut previous_file_state_by_file: HashMap<String, PreviousFileState> = HashMap::new();
    let mut ai_touched_files: HashSet<String> = initial_attributions.keys().cloned().collect();

    // Keep only the latest entry for each file.
    for checkpoint in previous_checkpoints {
        for entry in &checkpoint.entries {
            previous_file_state_by_file.insert(
                entry.file.clone(),
                PreviousFileState {
                    blob_sha: entry.blob_sha.clone(),
                    attributions: entry.attributions.clone(),
                },
            );

            if checkpoint.kind.is_ai() || working_log_entry_has_non_human_attribution(entry) {
                ai_touched_files.insert(entry.file.clone());
            }
        }
    }

    (previous_file_state_by_file, ai_touched_files)
}

#[allow(clippy::too_many_arguments)]
fn get_checkpoint_entry_for_file(
    file_path: String,
    kind: CheckpointKind,
    repo: Repository,
    working_log: PersistedWorkingLog,
    previous_file_state_by_file: Arc<HashMap<String, PreviousFileState>>,
    ai_touched_files: Arc<HashSet<String>>,
    file_content_hash: String,
    author_id: Arc<String>,
    head_tree_id: Arc<Option<String>>,
    initial_attributions: Arc<HashMap<String, Vec<LineAttribution>>>,
    initial_snapshot_contents: Arc<HashMap<String, Arc<str>>>,
    parent_note_attributions: Arc<HashMap<String, Vec<LineAttribution>>>,
    ts: u128,
) -> Result<Option<(WorkingLogEntry, FileLineStats)>, GitAiError> {
    let file_start = Instant::now();
    let initial_attrs_for_file = initial_attributions
        .get(&file_path)
        .cloned()
        .unwrap_or_default();
    let initial_snapshot_content = initial_snapshot_contents.get(&file_path).cloned();

    let previous_state = previous_file_state_by_file.get(&file_path).cloned();
    let has_prior_ai_edits = ai_touched_files.contains(&file_path);

    let current_content = working_log
        .read_current_file_content(&file_path)
        .unwrap_or_else(|_| Arc::<str>::from(""));

    // Non-pre-commit fast path:
    // Preserve existing `git-ai checkpoint` behavior for human-only files by writing an
    // attribution-empty entry while still capturing line stats.
    // KnownHuman checkpoints must bypass this path so they record h_<hash> attributions
    // that later AI checkpoints can use to identify human-written lines.
    if kind == CheckpointKind::Human && !has_prior_ai_edits && initial_attrs_for_file.is_empty() {
        let previous_content = if let Some(state) = previous_state.as_ref() {
            Arc::<str>::from(
                working_log
                    .get_file_version(&state.blob_sha)
                    .unwrap_or_default(),
            )
        } else {
            get_previous_content_from_head(&repo, &file_path, head_tree_id.as_ref())
        };

        if content_eq_ignoring_line_endings(&current_content, &previous_content) {
            return Ok(None);
        }

        let stats = compute_file_line_stats(&previous_content, &current_content);
        let entry = WorkingLogEntry::new(file_path, file_content_hash, Vec::new(), Vec::new());
        return Ok(Some((entry, stats)));
    }

    let from_checkpoint = previous_state.as_ref().map(|state| {
        (
            Arc::<str>::from(
                working_log
                    .get_file_version(&state.blob_sha)
                    .unwrap_or_default(),
            ),
            state.attributions.clone(),
        )
    });

    let is_from_checkpoint = from_checkpoint.is_some();
    let (previous_content, prev_attributions) = if let Some((content, attrs)) = from_checkpoint {
        (content, attrs)
    } else {
        // File doesn't exist in any previous checkpoint - need to initialize from git + INITIAL
        let previous_content =
            get_previous_content_from_head(&repo, &file_path, head_tree_id.as_ref());

        // Skip if no changes, UNLESS we have INITIAL attributions for this file
        // (in which case we need to create an entry to record those attributions)
        if content_eq_ignoring_line_endings(&current_content, &previous_content)
            && initial_attrs_for_file.is_empty()
        {
            return Ok(None);
        }

        // Build a set of lines covered by INITIAL attributions
        let mut initial_covered_lines: HashSet<u32> = HashSet::new();
        for attr in &initial_attrs_for_file {
            for line in attr.start_line..=attr.end_line {
                initial_covered_lines.insert(line);
            }
        }

        // Start with INITIAL attributions (they win), augmented by parent note
        let mut prev_line_attributions = initial_attrs_for_file.clone();

        // Parent note seeding removed — handled at post-commit via inheritance.
        let _ = &parent_note_attributions;

        let mut blamed_lines: HashSet<u32> = HashSet::new();

        // Default all previous-content lines to "human" (no cross-commit blame).
        // When INITIAL has a snapshot that DIFFERS from current content, use its
        // line count (that's what the diff will compare against). When the snapshot
        // matches current content (no edits after INITIAL), use the HEAD content
        // line count so the AI fallback can fire for uncovered lines.
        let effective_prev_content = if !initial_attrs_for_file.is_empty() {
            let snapshot = initial_snapshot_content
                .as_deref()
                .unwrap_or(&previous_content);
            if content_eq_ignoring_line_endings(snapshot, &current_content) {
                &previous_content
            } else {
                snapshot
            }
        } else {
            &previous_content
        };
        let prev_total_lines = effective_prev_content.lines().count() as u32;
        for line_num in 1..=prev_total_lines {
            blamed_lines.insert(line_num);
        }

        // For AI checkpoints, attribute any lines NOT in INITIAL and NOT returned by ai_blame
        if kind.is_ai() {
            let total_lines = current_content.lines().count() as u32;
            for line_num in 1..=total_lines {
                if !initial_covered_lines.contains(&line_num) && !blamed_lines.contains(&line_num) {
                    prev_line_attributions.push(LineAttribution {
                        start_line: line_num,
                        end_line: line_num,
                        author_id: author_id.as_ref().clone(),
                        overrode: None,
                    });
                }
            }
        }

        // INITIAL line numbers refer to the file state at the moment INITIAL was written.
        // Snapshot-aware INITIAL storage preserves that exact content; older INITIAL files
        // fall back to the legacy "current content" behavior.
        let content_for_line_conversion = if !initial_attrs_for_file.is_empty() {
            initial_snapshot_content
                .as_deref()
                .unwrap_or(&current_content)
        } else {
            &previous_content
        };

        // Convert any line attributions to character attributions
        let prev_attributions =
            crate::authorship::attribution_tracker::line_attributions_to_attributions(
                &prev_line_attributions,
                content_for_line_conversion,
                INITIAL_ATTRIBUTION_TS,
            );

        // When INITIAL has a persisted snapshot, use that as the previous content so later
        // edits after a restore/squash are tracked correctly. Older INITIAL files fall back
        // to the legacy current-content behavior.
        let adjusted_previous = if !initial_attrs_for_file.is_empty() {
            initial_snapshot_content.unwrap_or_else(|| current_content.clone())
        } else {
            previous_content
        };

        (adjusted_previous, prev_attributions)
    };

    // Skip if no changes (but we already checked this earlier, accounting for INITIAL attributions)
    // For files from previous checkpoints, check if content has changed
    if is_from_checkpoint && content_eq_ignoring_line_endings(&current_content, &previous_content) {
        if current_content == previous_content {
            // Byte-identical — truly no change.
            return Ok(None);
        }
        // Content differs only in line endings (CRLF ↔ LF). Update the stored blob
        // to the current content so future diffs compare LF-vs-LF. Without this,
        // the stale CRLF blob causes capture_diff_slices to see every line as changed,
        // and AI checkpoints (force_split=true) would re-attribute all lines to AI.
        // Remap attributions through line-number space to adjust byte offsets.
        let line_attributions =
            crate::authorship::attribution_tracker::attributions_to_line_attributions_for_checkpoint(
                &prev_attributions,
                &previous_content,
                kind.is_ai(),
            );
        let remapped_attributions =
            crate::authorship::attribution_tracker::line_attributions_to_attributions(
                &line_attributions,
                &current_content,
                ts,
            );
        let entry = WorkingLogEntry::new(
            file_path,
            file_content_hash,
            remapped_attributions,
            line_attributions,
        );
        return Ok(Some((entry, FileLineStats::default())));
    }

    let (entry, stats) = make_entry_for_file(FileEntryInput {
        file_path: &file_path,
        blob_sha: &file_content_hash,
        author_id: author_id.as_ref(),
        is_ai_checkpoint: kind.is_ai(),
        previous_content: &previous_content,
        previous_attributions: &prev_attributions,
        content: &current_content,
        ts,
    })?;

    tracing::debug!(
        "[BENCHMARK] Processing file {} took {:?}",
        file_path,
        file_start.elapsed()
    );
    Ok(Some((entry, stats)))
}

#[allow(clippy::too_many_arguments)]
async fn get_checkpoint_entries(
    kind: CheckpointKind,
    author: &str,
    repo: &Repository,
    working_log: &PersistedWorkingLog,
    files: &[String],
    file_content_hashes: &HashMap<String, String>,
    previous_checkpoints: &[Checkpoint],
    checkpoint_request: &CheckpointRequest,
    ts: u128,
    head_commit_override: Option<&str>,
    trace_id: String,
) -> Result<(Vec<WorkingLogEntry>, Vec<FileLineStats>), GitAiError> {
    let entries_fn_start = Instant::now();

    // Read INITIAL attributions from working log (empty if file doesn't exist)
    let initial_read_start = Instant::now();
    let initial_data = working_log.read_initial_attributions();
    let initial_snapshot_contents: HashMap<String, Arc<str>> = {
        let mut map = HashMap::new();
        for file_path in initial_data.files.keys() {
            if let Some(content) =
                working_log.initial_file_content_from(&initial_data, file_path)?
            {
                map.insert(file_path.clone(), Arc::<str>::from(content));
            }
        }
        map
    };
    let initial_attributions = initial_data.files;
    tracing::debug!(
        "[BENCHMARK] Reading initial attributions took {:?}",
        initial_read_start.elapsed()
    );

    let precompute_start = Instant::now();
    let (previous_file_state_by_file, ai_touched_files) =
        build_previous_file_state_maps(previous_checkpoints, &initial_attributions);
    tracing::debug!(
        "[BENCHMARK] Precomputing previous state maps took {:?}",
        precompute_start.elapsed()
    );

    // Determine author_id based on checkpoint kind and agent_id
    let author_id = match kind {
        CheckpointKind::Human => kind.to_str(), // "human" — stripped, never attested
        CheckpointKind::KnownHuman => {
            crate::authorship::authorship_log_serialization::generate_human_short_hash(author)
        }
        _ => {
            // AI kinds: compose session_id::trace_id
            checkpoint_request
                .agent_id
                .as_ref()
                .map(|aid| {
                    let session_id = generate_session_id(&aid.id, &aid.tool);
                    format!("{}::{}", session_id, trace_id)
                })
                .unwrap_or_else(|| kind.to_str())
        }
    };

    // Get HEAD commit info for git operations
    let head_commit = head_commit_override
        .map(str::trim)
        .filter(|sha| !sha.is_empty() && *sha != "initial")
        .and_then(|sha| repo.find_commit(sha.to_string()).ok())
        .or_else(|| {
            repo.head()
                .ok()
                .and_then(|h| h.target().ok())
                .and_then(|oid| repo.find_commit(oid).ok())
        });
    let head_tree_id = head_commit
        .as_ref()
        .and_then(|c| c.tree().ok())
        .map(|t| t.id().to_string());

    let parent_note_attributions: HashMap<String, Vec<LineAttribution>> = HashMap::new();

    const MAX_CONCURRENT: usize = 30;

    // Create a semaphore to limit concurrent tasks
    let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT));

    // Move other repeated allocations outside the loop
    let previous_file_state_by_file = Arc::new(previous_file_state_by_file);
    let ai_touched_files = Arc::new(ai_touched_files);
    let author_id = Arc::new(author_id);
    let head_tree_id = Arc::new(head_tree_id);
    let initial_attributions = Arc::new(initial_attributions);
    let initial_snapshot_contents = Arc::new(initial_snapshot_contents);
    let parent_note_attributions = Arc::new(parent_note_attributions);

    // Spawn tasks for each file
    let spawn_start = Instant::now();
    let mut tasks = Vec::new();

    for file_path in files {
        let file_path = file_path.clone();
        let repo = repo.clone();
        let working_log = working_log.clone();
        let previous_file_state_by_file = Arc::clone(&previous_file_state_by_file);
        let ai_touched_files = Arc::clone(&ai_touched_files);
        let author_id = Arc::clone(&author_id);
        let head_tree_id = Arc::clone(&head_tree_id);
        let blob_sha = file_content_hashes
            .get(&file_path)
            .cloned()
            .unwrap_or_default();
        let initial_attributions = Arc::clone(&initial_attributions);
        let initial_snapshot_contents = Arc::clone(&initial_snapshot_contents);
        let parent_note_attributions = Arc::clone(&parent_note_attributions);
        let semaphore = Arc::clone(&semaphore);

        let task = async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("checkpoint entry semaphore was closed");

            crate::tokio_runtime::spawn_blocking_result(move || {
                get_checkpoint_entry_for_file(
                    file_path,
                    kind,
                    repo,
                    working_log,
                    previous_file_state_by_file,
                    ai_touched_files,
                    blob_sha,
                    author_id.clone(),
                    head_tree_id.clone(),
                    initial_attributions.clone(),
                    initial_snapshot_contents.clone(),
                    parent_note_attributions.clone(),
                    ts,
                )
            })
            .await
        };

        tasks.push(task);
    }
    tracing::debug!(
        "[BENCHMARK] Spawning {} tasks took {:?}",
        tasks.len(),
        spawn_start.elapsed()
    );

    // Await all tasks concurrently
    let await_start = Instant::now();
    let results = futures::future::join_all(tasks).await;
    tracing::debug!(
        "[BENCHMARK] Awaiting {} tasks took {:?}",
        results.len(),
        await_start.elapsed()
    );

    // Process results
    let process_start = Instant::now();
    let results_count = results.len();
    let mut entries = Vec::new();
    let mut file_stats = Vec::new();
    for result in results {
        match result {
            Ok(Some((entry, stats))) => {
                entries.push(entry);
                file_stats.push(stats);
            }
            Ok(None) => {} // File had no changes
            Err(e) => return Err(e),
        }
    }
    tracing::debug!(
        "[BENCHMARK] Processing {} results took {:?}",
        results_count,
        process_start.elapsed()
    );
    tracing::debug!(
        "[BENCHMARK] get_checkpoint_entries function total took {:?}",
        entries_fn_start.elapsed()
    );

    Ok((entries, file_stats))
}

struct FileEntryInput<'a> {
    file_path: &'a str,
    blob_sha: &'a str,
    author_id: &'a str,
    is_ai_checkpoint: bool,
    previous_content: &'a str,
    previous_attributions: &'a [Attribution],
    content: &'a str,
    ts: u128,
}

fn make_entry_for_file(
    input: FileEntryInput<'_>,
) -> Result<(WorkingLogEntry, FileLineStats), GitAiError> {
    let FileEntryInput {
        file_path,
        blob_sha,
        author_id,
        is_ai_checkpoint,
        previous_content,
        previous_attributions,
        content,
        ts,
    } = input;

    let tracker = AttributionTracker::new();

    let fill_start = Instant::now();
    let filled_in_prev_attributions = tracker.attribute_unattributed_ranges(
        previous_content,
        previous_attributions,
        &CheckpointKind::Human.to_str(),
        ts - 1,
    );
    tracing::debug!(
        "[BENCHMARK]   attribute_unattributed_ranges for {} took {:?}",
        file_path,
        fill_start.elapsed()
    );

    let update_start = Instant::now();
    let new_attributions = tracker.update_attributions_for_checkpoint(
        previous_content,
        content,
        &filled_in_prev_attributions,
        author_id,
        ts,
        is_ai_checkpoint,
    )?;
    tracing::debug!(
        "[BENCHMARK]   update_attributions for {} took {:?}",
        file_path,
        update_start.elapsed()
    );

    // TODO Consider discarding any "uncontentious" attributions for the human author. Any human attributions that do not share a line with any other author's attributions can be discarded.
    // let filtered_attributions = crate::authorship::attribution_tracker::discard_uncontentious_attributions_for_author(&new_attributions, &CheckpointKind::Human.to_str());

    let line_attr_start = Instant::now();
    let line_attributions =
        crate::authorship::attribution_tracker::attributions_to_line_attributions_for_checkpoint(
            &new_attributions,
            content,
            is_ai_checkpoint,
        );
    tracing::debug!(
        "[BENCHMARK]   attributions_to_line_attributions for {} took {:?}",
        file_path,
        line_attr_start.elapsed()
    );

    // Compute line stats while we already have both contents in memory
    let stats_start = Instant::now();
    let line_stats = compute_file_line_stats(previous_content, content);
    tracing::debug!(
        "[BENCHMARK]   compute_file_line_stats for {} took {:?}",
        file_path,
        stats_start.elapsed()
    );

    let entry = WorkingLogEntry::new(
        file_path.to_string(),
        blob_sha.to_string(),
        new_attributions,
        line_attributions,
    );

    Ok((entry, line_stats))
}

/// Compute line statistics for a single file by diffing previous and current content
#[doc(hidden)]
pub fn compute_file_line_stats(previous_content: &str, current_content: &str) -> FileLineStats {
    let mut stats = FileLineStats::default();

    // Use imara_diff to count line changes (matches git's diff algorithm)
    let changes = compute_line_changes(previous_content, current_content);
    for change in changes {
        match change.tag() {
            LineChangeTag::Insert => {
                let non_whitespace_lines = change
                    .value()
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .count() as u32;
                stats.additions += change.value().lines().count() as u32;
                stats.additions_sloc += non_whitespace_lines;
            }
            LineChangeTag::Delete => {
                let non_whitespace_lines = change
                    .value()
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .count() as u32;
                stats.deletions += change.value().lines().count() as u32;
                stats.deletions_sloc += non_whitespace_lines;
            }
            LineChangeTag::Equal => {}
        }
    }

    stats
}

/// Aggregate line statistics from individual file stats
/// This avoids redundant diff computation since stats are already computed during entry creation
fn compute_line_stats(
    file_stats: &[FileLineStats],
) -> Result<crate::authorship::working_log::CheckpointLineStats, GitAiError> {
    let mut stats = crate::authorship::working_log::CheckpointLineStats::default();

    // Aggregate line stats from all files
    for file_stat in file_stats {
        stats.additions += file_stat.additions;
        stats.deletions += file_stat.deletions;
        stats.additions_sloc += file_stat.additions_sloc;
        stats.deletions_sloc += file_stat.deletions_sloc;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_ai_skips_agent_usage_throttle() {
        let agent_id = AgentId {
            tool: "mock_ai".to_string(),
            id: "debug-self-check".to_string(),
            model: "unknown".to_string(),
        };

        assert!(!should_emit_agent_usage(&agent_id));
    }
}
