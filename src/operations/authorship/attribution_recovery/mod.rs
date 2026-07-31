mod bash_recovery;
mod commit_agent_metadata;
mod commit_metadata_recovery;
mod session_event_recovery;
mod timestamp_collection;

use crate::error::GitAiError;
use crate::metrics::{CheckpointValues, EventAttributes, MetricEvent, PosEncoded};
use crate::model::authorship_log::LineRange;
use crate::model::authorship_log_serialization::{AuthorshipLog, generate_trace_id};
use crate::model::stat_snapshot::StatEntry;
use crate::model::working_log::CheckpointKind;
use crate::operations::authorship::recovery_stores::RecoveryStores;
use crate::operations::git::repo_state::worktree_root_for_path;
use crate::operations::git::repository::Repository;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const SESSION_EVENT_RECOVERY_WINDOW_NS: u128 = 3_000_000_000;
const EDGE_EXTENSION_MAX_LINES: usize = 3;

pub(crate) type FileTimestampsByPath = HashMap<String, Vec<u128>>;
pub(crate) type UnknownLinesByFile = BTreeMap<String, Vec<u32>>;

#[derive(Clone, Copy)]
pub(crate) struct AttributionRecoveryContext<'a> {
    pub(crate) file_timestamps: Option<&'a FileTimestampsByPath>,
    pub(crate) before_external_recovery: Option<&'a dyn Fn(&UnknownLinesByFile)>,
    pub(crate) stores: RecoveryStores,
}

pub(crate) fn recover_attribution(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    human_author: &str,
    authorship_log: &mut AuthorshipLog,
    committed_hunks: &HashMap<String, Vec<LineRange>>,
    context: AttributionRecoveryContext<'_>,
) -> Result<(), GitAiError> {
    if committed_hunks.is_empty() {
        return Ok(());
    }

    if unknown_lines_by_file(authorship_log, committed_hunks).is_empty() {
        return Ok(());
    }

    bash_recovery::recover_bash_mtime(
        repo,
        parent_sha,
        commit_sha,
        human_author,
        authorship_log,
        committed_hunks,
        context.file_timestamps,
        context.stores,
    )?;

    recover_adjacent_edges(
        repo,
        parent_sha,
        commit_sha,
        authorship_log,
        committed_hunks,
    );
    let unknown_after_edges = unknown_lines_by_file(authorship_log, committed_hunks);
    if unknown_after_edges.is_empty() {
        return Ok(());
    }

    if let Some(before_external_recovery) = context.before_external_recovery {
        before_external_recovery(&unknown_after_edges);
    }

    session_event_recovery::recover_session_event_mtime(
        repo,
        parent_sha,
        commit_sha,
        human_author,
        authorship_log,
        committed_hunks,
        context.file_timestamps,
        context.stores,
    )?;

    commit_metadata_recovery::recover_commit_metadata(
        repo,
        parent_sha,
        commit_sha,
        human_author,
        authorship_log,
        committed_hunks,
        context.file_timestamps,
        context.stores,
    )?;
    Ok(())
}

pub(crate) fn matching_session_event_candidate_exists(
    timestamps_ns: &[u128],
    target_repo_url: &str,
    stores: RecoveryStores,
) -> Result<bool, GitAiError> {
    if timestamps_ns.is_empty() || target_repo_url.is_empty() {
        return Ok(false);
    }

    let candidates = match stores.metrics.and_then(|db| db.lock().ok()) {
        Some(db) => db.session_event_candidates_near_timestamps(
            timestamps_ns,
            SESSION_EVENT_RECOVERY_WINDOW_NS,
        )?,
        None => Vec::new(),
    };

    Ok(session_event_recovery::select_best_session_event_candidate(
        &candidates,
        timestamps_ns,
        target_repo_url,
    )
    .is_some())
}

fn recover_adjacent_edges(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    authorship_log: &mut AuthorshipLog,
    committed_hunks: &HashMap<String, Vec<LineRange>>,
) {
    let unknown = unknown_lines_by_file(authorship_log, committed_hunks);
    for (file_path, unknown_lines) in unknown {
        let line_to_author = line_author_map(authorship_log, &file_path);
        let runs = contiguous_runs(&unknown_lines);
        for run in runs {
            let Some(recovery) = edge_recovery_for_run(&line_to_author, &run) else {
                continue;
            };
            let trace_id = generate_trace_id();
            let source_session = recovery
                .source_author
                .split("::")
                .next()
                .unwrap_or(&recovery.source_author)
                .to_string();
            let recovered_author = if source_session.starts_with("s_") {
                format!("{}::{}", source_session, trace_id)
            } else {
                recovery.source_author.clone()
            };
            let recovered_line_count = recovery.lines.len() as u32;
            add_attestation(
                authorship_log,
                &file_path,
                &recovered_author,
                &recovery.lines,
            );

            let metadata = json!({
                "solver": "edge_extension",
                "file_path": file_path,
                "source_author": &recovery.source_author,
                "recovered_lines": &recovery.lines,
            });
            record_recovery_metric(RecoveryMetricInput {
                repo,
                parent_sha,
                commit_sha,
                file_path: &file_path,
                author_id: &recovered_author,
                session_id: &source_session,
                trace_id: &trace_id,
                tool: "",
                model: "",
                external_session_id: "",
                external_tool_use_id: None,
                edit_kind: "attribution_recovery_edge",
                checkpoint_type: "recovered_edge_extension",
                recovered_line_count,
                metadata,
                event_ts: None,
            });
        }
    }
}

pub(super) fn repo_worktree_key(repo: &Repository) -> Result<String, GitAiError> {
    let workdir = repo.workdir()?;
    let normalized = worktree_root_for_path(&workdir).unwrap_or(workdir);
    Ok(
        crate::operations::git::canonicalize::canonicalize_or_self(&normalized)
            .to_string_lossy()
            .to_string(),
    )
}

pub(super) fn file_timestamps_ns(workdir: &std::path::Path, file_path: &str) -> Vec<u128> {
    file_timestamps_for_path(&workdir.join(file_path))
}

pub(crate) fn file_timestamps_for_path(path: &Path) -> Vec<u128> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Vec::new();
    };
    let stat = StatEntry::from_metadata(&meta);
    let mut timestamps = Vec::new();
    if let Some(mtime) = stat.mtime {
        timestamps.push(system_time_to_ns(mtime));
    }
    if let Some(ctime) = stat.ctime {
        timestamps.push(system_time_to_ns(ctime));
    }
    timestamps.sort_unstable();
    timestamps.dedup();
    timestamps
}

fn system_time_to_ns(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

pub(super) fn existing_commit_session_ids(authorship_log: &AuthorshipLog) -> HashSet<String> {
    let mut sessions = HashSet::new();
    for file_attestation in &authorship_log.attestations {
        for entry in &file_attestation.entries {
            let session = ai_session_key(&entry.hash);
            if session.starts_with("s_") {
                sessions.insert(session.to_string());
            }
        }
    }
    sessions
}

pub(super) fn unknown_lines_by_file(
    authorship_log: &AuthorshipLog,
    committed_hunks: &HashMap<String, Vec<LineRange>>,
) -> UnknownLinesByFile {
    let covered = covered_lines_by_file(authorship_log);
    let mut result = BTreeMap::new();
    for (file_path, ranges) in committed_hunks {
        let covered_lines = covered.get(file_path);
        let mut unknown = Vec::new();
        for line in ranges.iter().flat_map(LineRange::expand) {
            if !covered_lines.is_some_and(|lines| lines.contains(&line)) {
                unknown.push(line);
            }
        }
        unknown.sort_unstable();
        unknown.dedup();
        if !unknown.is_empty() {
            result.insert(file_path.clone(), unknown);
        }
    }
    result
}

fn covered_lines_by_file(authorship_log: &AuthorshipLog) -> HashMap<String, HashSet<u32>> {
    let mut covered = HashMap::new();
    for file_attestation in &authorship_log.attestations {
        let lines = covered
            .entry(file_attestation.file_path.clone())
            .or_insert_with(HashSet::new);
        for entry in &file_attestation.entries {
            for line in entry.line_ranges.iter().flat_map(LineRange::expand) {
                lines.insert(line);
            }
        }
    }
    covered
}

fn line_author_map(authorship_log: &AuthorshipLog, file_path: &str) -> BTreeMap<u32, String> {
    let mut map = BTreeMap::new();
    let Some(file_attestation) = authorship_log
        .attestations
        .iter()
        .find(|att| att.file_path == file_path)
    else {
        return map;
    };
    for entry in &file_attestation.entries {
        for line in entry.line_ranges.iter().flat_map(LineRange::expand) {
            map.insert(line, entry.hash.clone());
        }
    }
    map
}

fn contiguous_runs(lines: &[u32]) -> Vec<Vec<u32>> {
    if lines.is_empty() {
        return Vec::new();
    }
    let mut sorted = lines.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut runs: Vec<Vec<u32>> = Vec::new();
    let mut current = vec![sorted[0]];
    for line in sorted.into_iter().skip(1) {
        if line == current.last().copied().unwrap_or(line) + 1 {
            current.push(line);
        } else {
            runs.push(current);
            current = vec![line];
        }
    }
    runs.push(current);
    runs
}

struct EdgeRecovery {
    source_author: String,
    lines: Vec<u32>,
}

fn edge_recovery_for_run(
    line_to_author: &BTreeMap<u32, String>,
    run: &[u32],
) -> Option<EdgeRecovery> {
    let first = *run.first()?;
    let last = *run.last()?;
    let prev = first
        .checked_sub(1)
        .and_then(|line| line_to_author.get(&line));
    let next = line_to_author.get(&(last + 1));

    match (prev, next) {
        (Some(left), Some(right))
            if is_ai_attestation(left)
                && is_ai_attestation(right)
                && ai_session_key(left) == ai_session_key(right) =>
        {
            let mut lines = run
                .iter()
                .take(EDGE_EXTENSION_MAX_LINES)
                .copied()
                .collect::<Vec<_>>();
            lines.extend(run.iter().rev().take(EDGE_EXTENSION_MAX_LINES).copied());
            lines.sort_unstable();
            lines.dedup();
            Some(EdgeRecovery {
                source_author: left.clone(),
                lines,
            })
        }
        (Some(left), None) if is_ai_attestation(left) => Some(EdgeRecovery {
            source_author: left.clone(),
            lines: run.iter().take(EDGE_EXTENSION_MAX_LINES).copied().collect(),
        }),
        (None, Some(right)) if is_ai_attestation(right) => Some(EdgeRecovery {
            source_author: right.clone(),
            lines: run
                .iter()
                .rev()
                .take(EDGE_EXTENSION_MAX_LINES)
                .copied()
                .collect(),
        }),
        _ => None,
    }
}

#[cfg(test)]
fn edge_recovered_lines(line_to_author: &BTreeMap<u32, String>, run: &[u32]) -> Option<Vec<u32>> {
    edge_recovery_for_run(line_to_author, run).map(|mut recovery| {
        recovery.lines.sort_unstable();
        recovery.lines
    })
}

pub(super) fn is_ai_attestation(author: &str) -> bool {
    author != CheckpointKind::Human.to_str() && !author.starts_with("h_")
}

pub(super) fn ai_session_key(author: &str) -> &str {
    author.split("::").next().unwrap_or(author)
}

pub(super) fn add_attestation(
    authorship_log: &mut AuthorshipLog,
    file_path: &str,
    author_id: &str,
    lines: &[u32],
) {
    let mut sorted = lines.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.is_empty() {
        return;
    }
    let ranges = LineRange::compress_lines(&sorted);
    let entry = crate::model::authorship_log_serialization::AttestationEntry::new(
        author_id.to_string(),
        ranges,
    );
    authorship_log
        .get_or_create_file(file_path)
        .add_entry(entry);
}

pub(super) struct RecoveryMetricInput<'a> {
    pub(super) repo: &'a Repository,
    pub(super) parent_sha: &'a str,
    pub(super) commit_sha: &'a str,
    pub(super) file_path: &'a str,
    pub(super) author_id: &'a str,
    pub(super) session_id: &'a str,
    pub(super) trace_id: &'a str,
    pub(super) tool: &'a str,
    pub(super) model: &'a str,
    pub(super) external_session_id: &'a str,
    pub(super) external_tool_use_id: Option<&'a str>,
    pub(super) edit_kind: &'a str,
    pub(super) checkpoint_type: &'a str,
    pub(super) recovered_line_count: u32,
    pub(super) metadata: serde_json::Value,
    pub(super) event_ts: Option<u32>,
}

pub(super) fn record_recovery_metric(input: RecoveryMetricInput<'_>) {
    if input.tool == "mock_ai" {
        return;
    }

    let checkpoint_ts = input
        .event_ts
        .map(u64::from)
        .unwrap_or_else(crate::model::clock::now_secs);
    let mut values = CheckpointValues::new()
        .checkpoint_ts(checkpoint_ts)
        .kind(CheckpointKind::AiAgent.to_str())
        .file_path(input.file_path)
        .lines_added(input.recovered_line_count)
        .lines_deleted(0)
        .lines_added_sloc(input.recovered_line_count)
        .lines_deleted_sloc(0)
        .edit_kind(input.edit_kind)
        .checkpoint_type(input.checkpoint_type)
        .attribution_recovery_metadata(input.metadata.to_string());
    if let Some(tool_use_id) = input.external_tool_use_id {
        values = values.external_tool_use_id(tool_use_id);
    }

    let mut attrs = EventAttributes::with_version(env!("CARGO_PKG_VERSION"))
        .base_commit_sha(input.parent_sha)
        .commit_sha(input.commit_sha)
        .session_id(input.session_id)
        .trace_id(input.trace_id);

    if !input.tool.is_empty() {
        attrs = attrs.tool(input.tool);
    }
    if !input.model.is_empty() {
        attrs = attrs.model(input.model);
    }
    if !input.external_session_id.is_empty() {
        attrs = attrs.external_session_id(input.external_session_id);
    }
    if let Some(url) = crate::repo_url::resolve_repo_url_from_repo(input.repo) {
        attrs = attrs.repo_url(url);
    }
    if let Ok(head_ref) = input.repo.head()
        && let Ok(short_branch) = head_ref.shorthand()
    {
        attrs = attrs.branch(short_branch);
    }
    attrs = attrs.author(input.author_id);

    let event = MetricEvent::from_values_with_timestamp(values, attrs.to_sparse(), input.event_ts);
    crate::observability::log_metrics(vec![event]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::authorship_log_serialization::{
        AttestationEntry, AuthorshipLog, FileAttestation,
    };

    #[test]
    fn unknown_lines_exclude_existing_attestations() {
        let mut log = AuthorshipLog::new();
        log.attestations.push(FileAttestation {
            file_path: "a.txt".to_string(),
            entries: vec![AttestationEntry::new(
                "s_abc::t_def".to_string(),
                vec![LineRange::Single(2)],
            )],
        });
        let committed = HashMap::from([(
            "a.txt".to_string(),
            vec![LineRange::Range(1, 3), LineRange::Single(5)],
        )]);

        let unknown = unknown_lines_by_file(&log, &committed);
        assert_eq!(unknown.get("a.txt").unwrap(), &vec![1, 3, 5]);
    }

    #[test]
    fn edge_recovery_extends_one_sided_runs_and_bridges_matching_ai_neighbors() {
        let map = BTreeMap::from([
            (1, "s_a::t_1".to_string()),
            (3, "s_a::t_2".to_string()),
            (10, "s_a::t_3".to_string()),
            (20, "s_b::t_1".to_string()),
        ]);

        assert_eq!(
            edge_recovered_lines(&map, &[2]).as_deref(),
            Some(&[2][..]),
            "different trace ids for the same session should extend by session"
        );
        assert_eq!(
            edge_recovered_lines(&map, &[4, 5, 6, 7, 8, 9]).as_deref(),
            Some(&[4, 5, 6, 7, 8, 9][..]),
            "matching AI neighbors should recover up to three lines from each side"
        );
        assert_eq!(
            edge_recovered_lines(&map, &[11, 12, 13, 14]).as_deref(),
            Some(&[11, 12, 13][..]),
            "trailing edge extension should recover at most three lines"
        );
        assert_eq!(
            edge_recovered_lines(&map, &[16, 17, 18, 19]).as_deref(),
            Some(&[17, 18, 19][..]),
            "leading edge extension should recover the three lines nearest the AI block"
        );
    }

    #[test]
    fn edge_recovery_keeps_human_and_different_session_guardrails() {
        let map = BTreeMap::from([
            (1, "s_a::t_1".to_string()),
            (3, "s_b::t_1".to_string()),
            (5, "h_human::t_1".to_string()),
        ]);

        assert_eq!(
            edge_recovered_lines(&map, &[2]).as_deref(),
            None,
            "different sessions must not be bridged"
        );
        assert_eq!(
            edge_recovered_lines(&map, &[4]).as_deref(),
            None,
            "known-human neighbors must not be used for edge extension"
        );
    }
}
