use crate::model::authorship_log::{LineRange, SessionRecord};
use crate::model::authorship_log_serialization::{
    AuthorshipLog, generate_session_id, generate_trace_id,
};
use crate::operations::authorship::bash_candidate::{BashCandidate, distance_to_call_window};
use crate::operations::authorship::recovery_stores::RecoveryStores;
use crate::operations::git::repository::Repository;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::timestamp_collection::collect_unknown_file_timestamps;
use super::{
    FileTimestampsByPath, RecoveryMetricInput, add_attestation, existing_commit_session_ids,
    record_recovery_metric, repo_worktree_key, unknown_lines_by_file,
};

const BASH_RECOVERY_WINDOW_NS: u128 = 3_000_000_000;
const BASH_RECOVERY_COARSE_TIMESTAMP_NS: u128 = 1_000_000_000;

pub(super) struct BashCandidateSelection<'a> {
    pub(super) candidate: &'a BashCandidate,
    pub(super) distance_ns: u128,
    pub(super) tier: BashCandidateTier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BashCandidateTier {
    ExistingCommitSession,
    WorkdirAncestor,
    CwdAncestor,
    TimeOnly,
}

impl BashCandidateTier {
    pub(super) fn score(self) -> u8 {
        match self {
            Self::ExistingCommitSession => 0,
            Self::WorkdirAncestor => 1,
            Self::CwdAncestor => 2,
            Self::TimeOnly => 3,
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::ExistingCommitSession => "existing_commit_session",
            Self::WorkdirAncestor => "workdir_ancestor",
            Self::CwdAncestor => "cwd_ancestor",
            Self::TimeOnly => "time_only",
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn recover_bash_mtime(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    human_author: &str,
    authorship_log: &mut AuthorshipLog,
    committed_hunks: &HashMap<String, Vec<LineRange>>,
    captured_file_timestamps: Option<&FileTimestampsByPath>,
    stores: RecoveryStores,
) -> Result<(), crate::error::GitAiError> {
    let repo_work_dir = repo_worktree_key(repo)?;
    let workdir = repo.workdir()?;
    let unknown_by_file = unknown_lines_by_file(authorship_log, committed_hunks);
    if unknown_by_file.is_empty() {
        return Ok(());
    }

    let timestamp_collection =
        collect_unknown_file_timestamps(&workdir, unknown_by_file, captured_file_timestamps);
    if timestamp_collection.unique_timestamps.is_empty() {
        return Ok(());
    }

    let candidates: Vec<BashCandidate> = match stores.bash_history.and_then(|db| db.lock().ok()) {
        Some(db) => db
            .candidates_near_timestamps(
                &timestamp_collection.unique_timestamps,
                BASH_RECOVERY_WINDOW_NS,
            )?
            .into_iter()
            .map(BashCandidate::from)
            .collect(),
        None => Vec::new(),
    };
    if candidates.is_empty() {
        return Ok(());
    }

    let existing_commit_sessions = existing_commit_session_ids(authorship_log);
    for (file_path, unknown_lines) in timestamp_collection.unknown_by_file {
        let Some(timestamps) = timestamp_collection.timestamps_by_file.get(&file_path) else {
            continue;
        };
        let Some(selection) = select_best_bash_candidate(
            &candidates,
            timestamps,
            &existing_commit_sessions,
            &repo_work_dir,
        ) else {
            continue;
        };
        let candidate = selection.candidate;
        let distance_ns = selection.distance_ns;
        if distance_ns > BASH_RECOVERY_WINDOW_NS {
            continue;
        }

        let trace_id = generate_trace_id();
        let session_id = bash_candidate_session_id(candidate);
        let author_id = format!("{}::{}", session_id, trace_id);
        insert_session_record(authorship_log, &session_id, candidate, human_author);
        add_attestation(authorship_log, &file_path, &author_id, &unknown_lines);

        let metadata = json!({
            "solver": "bash_mtime",
            "file_path": file_path,
            "unknown_lines": unknown_lines,
            "target_repo_work_dir": repo_work_dir.as_str(),
            "file_timestamps_ns": timestamps,
            "selected_bash_recency_ordinal": candidate.recency_ordinal,
            "selected_bash_original_cwd": candidate.original_cwd.as_str(),
            "selected_bash_repo_work_dir": candidate.repo_work_dir.as_deref(),
            "selected_bash_repo_discovery_error": candidate.repo_discovery_error.as_deref(),
            "selected_tool_use_id": candidate.tool_use_id,
            "selected_command": candidate.command,
            "distance_ns": distance_ns,
            "window_ns": BASH_RECOVERY_WINDOW_NS,
            "start_time_ns": candidate.start_time_ns,
            "end_time_ns": candidate.end_time_ns,
            "selection_tier": selection.tier.as_str(),
            "candidate_count": candidates.len(),
        });
        record_recovery_metric(RecoveryMetricInput {
            repo,
            parent_sha,
            commit_sha,
            file_path: &file_path,
            author_id: &author_id,
            session_id: &session_id,
            trace_id: &trace_id,
            tool: &candidate.agent_id.tool,
            model: &candidate.agent_id.model,
            external_session_id: &candidate.agent_id.id,
            external_tool_use_id: Some(&candidate.tool_use_id),
            edit_kind: "bash",
            checkpoint_type: "recovered_bash",
            recovered_line_count: unknown_lines.len() as u32,
            metadata,
            event_ts: Some((candidate.start_time_ns / 1_000_000_000) as u32),
        });
    }

    Ok(())
}

pub(super) fn select_best_bash_candidate<'a>(
    candidates: &'a [BashCandidate],
    timestamps: &[u128],
    existing_commit_sessions: &HashSet<String>,
    target_repo_work_dir: &str,
) -> Option<BashCandidateSelection<'a>> {
    candidates
        .iter()
        .filter_map(|candidate| {
            let distance = timestamps
                .iter()
                .filter_map(|ts| recovery_distance_to_call_window(*ts, candidate))
                .min()?;
            Some(BashCandidateSelection {
                candidate,
                distance_ns: distance,
                tier: bash_candidate_tier(
                    candidate,
                    existing_commit_sessions,
                    target_repo_work_dir,
                ),
            })
        })
        .min_by(|left, right| {
            left.tier
                .score()
                .cmp(&right.tier.score())
                .then_with(|| left.distance_ns.cmp(&right.distance_ns))
                .then_with(|| {
                    right
                        .candidate
                        .end_time_ns
                        .is_some()
                        .cmp(&left.candidate.end_time_ns.is_some())
                })
                .then_with(|| {
                    right
                        .candidate
                        .command
                        .is_some()
                        .cmp(&left.candidate.command.is_some())
                })
                .then_with(|| {
                    right
                        .candidate
                        .recency_ordinal
                        .cmp(&left.candidate.recency_ordinal)
                })
        })
}

fn bash_candidate_tier(
    candidate: &BashCandidate,
    existing_commit_sessions: &HashSet<String>,
    target_repo_work_dir: &str,
) -> BashCandidateTier {
    let session_id = bash_candidate_session_id(candidate);
    if existing_commit_sessions.contains(&session_id) {
        return BashCandidateTier::ExistingCommitSession;
    }

    if let Some(repo_work_dir) = candidate.repo_work_dir.as_deref()
        && path_is_equal_or_child(target_repo_work_dir, repo_work_dir)
    {
        return BashCandidateTier::WorkdirAncestor;
    }

    if path_is_equal_or_child(target_repo_work_dir, &candidate.original_cwd) {
        return BashCandidateTier::CwdAncestor;
    }

    BashCandidateTier::TimeOnly
}

fn path_is_equal_or_child(child: &str, parent: &str) -> bool {
    if child.is_empty() || parent.is_empty() {
        return false;
    }

    let child = Path::new(child);
    let parent = Path::new(parent);
    child == parent || child.starts_with(parent)
}

pub(super) fn recovery_distance_to_call_window(
    timestamp_ns: u128,
    call: &BashCandidate,
) -> Option<u128> {
    let start = call.start_time_ns;
    let end = call
        .end_time_ns
        .unwrap_or_else(|| start.saturating_add(BASH_RECOVERY_WINDOW_NS));

    if timestamp_ns > end {
        return None;
    }

    if timestamp_ns < start {
        let start_skew_ns = start.saturating_sub(timestamp_ns);
        // Low-resolution filesystems can truncate mtimes to whole seconds.
        // Only grant start-side grace to timestamps that look truncated.
        if start_skew_ns >= BASH_RECOVERY_COARSE_TIMESTAMP_NS
            || !timestamp_ns.is_multiple_of(BASH_RECOVERY_COARSE_TIMESTAMP_NS)
        {
            return None;
        }
    }

    Some(distance_to_call_window(timestamp_ns, call))
}

fn bash_candidate_session_id(candidate: &BashCandidate) -> String {
    generate_session_id(&candidate.agent_id.id, &candidate.agent_id.tool)
}

fn insert_session_record(
    authorship_log: &mut AuthorshipLog,
    session_id: &str,
    candidate: &BashCandidate,
    human_author: &str,
) {
    authorship_log
        .metadata
        .sessions
        .entry(session_id.to_string())
        .or_insert_with(|| SessionRecord {
            agent_id: candidate.agent_id.clone(),
            human_author: Some(human_author.to_string()),
            custom_attributes: None,
        });
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::model::authorship_log_serialization::generate_session_id;
    use crate::model::working_log::AgentId;

    fn test_agent(external_session_id: &str) -> AgentId {
        AgentId {
            tool: "codex".to_string(),
            id: external_session_id.to_string(),
            model: "gpt-5".to_string(),
        }
    }

    pub(in super::super) fn bash_call(
        id: i64,
        external_session_id: &str,
        tool_use_id: &str,
        repo_work_dir: &str,
        start_time_ns: u128,
        end_time_ns: Option<u128>,
    ) -> BashCandidate {
        BashCandidate {
            recency_ordinal: id,
            agent_id: test_agent(external_session_id),
            original_cwd: repo_work_dir.to_string(),
            repo_work_dir: Some(repo_work_dir.to_string()),
            repo_discovery_error: None,
            tool_use_id: tool_use_id.to_string(),
            start_time_ns,
            end_time_ns,
            command: Some("true".to_string()),
        }
    }

    pub(in super::super) fn unresolved_bash_attempt(
        id: i64,
        external_session_id: &str,
        tool_use_id: &str,
        original_cwd: &str,
        start_time_ns: u128,
        end_time_ns: Option<u128>,
    ) -> BashCandidate {
        BashCandidate {
            recency_ordinal: id,
            agent_id: test_agent(external_session_id),
            original_cwd: original_cwd.to_string(),
            repo_work_dir: None,
            repo_discovery_error: Some("No git repository found".to_string()),
            tool_use_id: tool_use_id.to_string(),
            start_time_ns,
            end_time_ns,
            command: Some("cd repo && true".to_string()),
        }
    }

    #[test]
    fn bash_candidate_ranking_prefers_session_already_in_commit() {
        let existing_session = generate_session_id("existing-session", "codex");
        let existing_sessions = HashSet::from([existing_session]);
        let candidates = vec![
            bash_call(1, "closer-session", "tool-closer", "/repo", 1_040, None),
            bash_call(2, "existing-session", "tool-existing", "/other", 900, None),
        ];

        let selection =
            select_best_bash_candidate(&candidates, &[1_050], &existing_sessions, "/repo")
                .expect("expected candidate");

        assert_eq!(selection.candidate.tool_use_id, "tool-existing");
        assert_eq!(selection.tier, BashCandidateTier::ExistingCommitSession);
    }

    #[test]
    fn bash_candidate_ranking_uses_time_within_existing_commit_sessions() {
        let existing_sessions = HashSet::from([
            generate_session_id("existing-far", "codex"),
            generate_session_id("existing-near", "codex"),
        ]);
        let candidates = vec![
            bash_call(1, "existing-far", "tool-far", "/other", 900, None),
            bash_call(2, "existing-near", "tool-near", "/other", 1_000, None),
        ];

        let selection =
            select_best_bash_candidate(&candidates, &[1_050], &existing_sessions, "/repo")
                .expect("expected candidate");

        assert_eq!(selection.candidate.tool_use_id, "tool-near");
        assert_eq!(selection.distance_ns, 50);
    }

    #[test]
    fn bash_candidate_ranking_prefers_workdir_ancestor_when_no_session_matches() {
        let candidates = vec![
            bash_call(
                1,
                "ancestor-session",
                "tool-ancestor",
                "/tmp/work",
                900,
                None,
            ),
            bash_call(2, "closer-session", "tool-closer", "/other", 1_040, None),
        ];

        let selection =
            select_best_bash_candidate(&candidates, &[1_050], &HashSet::new(), "/tmp/work/repo")
                .expect("expected candidate");

        assert_eq!(selection.candidate.tool_use_id, "tool-ancestor");
        assert_eq!(selection.tier, BashCandidateTier::WorkdirAncestor);
    }

    #[test]
    fn bash_candidate_ranking_prefers_workdir_ancestor_over_cwd_ancestor() {
        let candidates = vec![
            bash_call(
                1,
                "workdir-ancestor-session",
                "tool-workdir-ancestor",
                "/tmp/work",
                900,
                None,
            ),
            unresolved_bash_attempt(
                2,
                "cwd-ancestor-session",
                "tool-cwd-ancestor",
                "/tmp/work",
                1_040,
                None,
            ),
        ];

        let selection =
            select_best_bash_candidate(&candidates, &[1_050], &HashSet::new(), "/tmp/work/repo")
                .expect("expected candidate");

        assert_eq!(selection.candidate.tool_use_id, "tool-workdir-ancestor");
        assert_eq!(selection.tier, BashCandidateTier::WorkdirAncestor);
    }

    #[test]
    fn bash_candidate_ranking_prefers_cwd_ancestor_over_time_only() {
        let candidates = vec![
            unresolved_bash_attempt(
                1,
                "cwd-ancestor-session",
                "tool-cwd-ancestor",
                "/tmp/work",
                900,
                None,
            ),
            bash_call(2, "closer-session", "tool-closer", "/other", 1_040, None),
        ];

        let selection =
            select_best_bash_candidate(&candidates, &[1_050], &HashSet::new(), "/tmp/work/repo")
                .expect("expected candidate");

        assert_eq!(selection.candidate.tool_use_id, "tool-cwd-ancestor");
        assert_eq!(selection.tier, BashCandidateTier::CwdAncestor);
    }

    #[test]
    fn bash_candidate_ranking_falls_back_to_closest_time() {
        let candidates = vec![
            bash_call(1, "far-session", "tool-far", "/other-a", 900, None),
            bash_call(2, "near-session", "tool-near", "/other-b", 1_040, None),
        ];

        let selection = select_best_bash_candidate(&candidates, &[1_050], &HashSet::new(), "/repo")
            .expect("expected candidate");

        assert_eq!(selection.candidate.tool_use_id, "tool-near");
        assert_eq!(selection.tier, BashCandidateTier::TimeOnly);
        assert_eq!(selection.distance_ns, 10);
    }

    #[test]
    fn bash_candidate_ranking_requires_matching_time_range() {
        let existing_sessions = HashSet::from([generate_session_id("existing-session", "codex")]);
        let candidates = vec![
            bash_call(
                1,
                "existing-session",
                "tool-completed",
                "/repo",
                1_000,
                Some(1_100),
            ),
            bash_call(2, "open-session", "tool-open", "/repo", 2_000, None),
        ];

        let selection =
            select_best_bash_candidate(&candidates, &[1_500], &existing_sessions, "/repo");

        assert!(
            selection.is_none(),
            "nearby candidates outside their matching bash time range must not recover attribution"
        );
    }

    #[test]
    fn bash_candidate_ranking_allows_coarse_timestamp_rounded_before_start() {
        let candidates = vec![bash_call(
            1,
            "coarse-session",
            "tool-coarse",
            "/repo",
            2_500_000_000,
            Some(3_000_000_000),
        )];

        let selection =
            select_best_bash_candidate(&candidates, &[2_000_000_000], &HashSet::new(), "/repo")
                .expect("expected coarse timestamp to match");

        assert_eq!(selection.candidate.tool_use_id, "tool-coarse");
        assert_eq!(selection.distance_ns, 500_000_000);
    }
}
