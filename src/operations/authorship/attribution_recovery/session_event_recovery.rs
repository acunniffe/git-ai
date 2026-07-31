use crate::error::GitAiError;
use crate::model::authorship_log::{LineRange, SessionRecord};
use crate::model::authorship_log_serialization::{AuthorshipLog, generate_trace_id};
use crate::model::session_recovery_candidate::SessionEventRecoveryCandidate;
use crate::model::working_log::AgentId;
use crate::operations::authorship::recovery_stores::RecoveryStores;
use crate::operations::git::repository::Repository;
use serde_json::json;
use std::collections::HashMap;

use super::timestamp_collection::collect_unknown_file_timestamps;
use super::{
    FileTimestampsByPath, RecoveryMetricInput, SESSION_EVENT_RECOVERY_WINDOW_NS, add_attestation,
    record_recovery_metric, unknown_lines_by_file,
};

const NS_PER_SECOND: u128 = 1_000_000_000;

pub(super) struct SessionEventCandidateSelection<'a> {
    pub(super) candidate: &'a SessionEventRecoveryCandidate,
    pub(super) distance_ns: u128,
    pub(super) tier: SessionEventCandidateTier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionEventCandidateTier {
    SameRepoUrl,
}

impl SessionEventCandidateTier {
    pub(super) fn score(self) -> u8 {
        match self {
            Self::SameRepoUrl => 0,
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::SameRepoUrl => "same_repo_url",
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn recover_session_event_mtime(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    human_author: &str,
    authorship_log: &mut AuthorshipLog,
    committed_hunks: &HashMap<String, Vec<LineRange>>,
    captured_file_timestamps: Option<&FileTimestampsByPath>,
    stores: RecoveryStores,
) -> Result<(), GitAiError> {
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

    let candidates = match stores.metrics.and_then(|db| db.lock().ok()) {
        Some(db) => db.session_event_candidates_near_timestamps(
            &timestamp_collection.unique_timestamps,
            SESSION_EVENT_RECOVERY_WINDOW_NS,
        )?,
        None => Vec::new(),
    };
    if candidates.is_empty() {
        return Ok(());
    }

    let Some(target_repo_url) = crate::repo_url::resolve_repo_url_from_repo(repo) else {
        return Ok(());
    };
    for (file_path, unknown_lines) in timestamp_collection.unknown_by_file {
        let Some(timestamps) = timestamp_collection.timestamps_by_file.get(&file_path) else {
            continue;
        };
        let Some(selection) =
            select_best_session_event_candidate(&candidates, timestamps, &target_repo_url)
        else {
            continue;
        };
        if selection.distance_ns > SESSION_EVENT_RECOVERY_WINDOW_NS {
            continue;
        }

        let candidate = selection.candidate;
        let trace_id = generate_trace_id();
        let author_id = format!("{}::{}", candidate.session_id, trace_id);
        insert_session_event_record(authorship_log, candidate, human_author);
        add_attestation(authorship_log, &file_path, &author_id, &unknown_lines);

        let selected_model = session_event_model(candidate);
        let metadata = json!({
            "solver": "session_event_mtime",
            "file_path": file_path.as_str(),
            "unknown_lines": &unknown_lines,
            "file_timestamps_ns": timestamps,
            "selected_metric_row_id": candidate.row_id,
            "selected_event_ts": candidate.event_ts,
            "selected_session_id": candidate.session_id.as_str(),
            "selected_external_session_id": candidate.external_session_id.as_str(),
            "selected_external_tool_use_id": candidate.external_tool_use_id.as_deref(),
            "selected_tool": candidate.tool.as_str(),
            "selected_model": selected_model.as_str(),
            "selected_repo_url": candidate.repo_url.as_deref(),
            "target_repo_url": target_repo_url.as_str(),
            "distance_ns": selection.distance_ns,
            "window_ns": SESSION_EVENT_RECOVERY_WINDOW_NS,
            "selection_tier": selection.tier.as_str(),
            "candidate_count": candidates.len(),
        });
        record_recovery_metric(RecoveryMetricInput {
            repo,
            parent_sha,
            commit_sha,
            file_path: &file_path,
            author_id: &author_id,
            session_id: &candidate.session_id,
            trace_id: &trace_id,
            tool: &candidate.tool,
            model: &selected_model,
            external_session_id: &candidate.external_session_id,
            external_tool_use_id: candidate.external_tool_use_id.as_deref(),
            edit_kind: "attribution_recovery_session_event",
            checkpoint_type: "recovered_session_event_mtime",
            recovered_line_count: unknown_lines.len() as u32,
            metadata,
            event_ts: Some(candidate.event_ts),
        });
    }

    Ok(())
}

pub(super) fn select_best_session_event_candidate<'a>(
    candidates: &'a [SessionEventRecoveryCandidate],
    timestamps: &[u128],
    target_repo_url: &str,
) -> Option<SessionEventCandidateSelection<'a>> {
    let matching_candidates = candidates
        .iter()
        .filter_map(|candidate| {
            let distance_ns = session_event_distance(candidate, timestamps)?;
            if distance_ns > SESSION_EVENT_RECOVERY_WINDOW_NS {
                return None;
            }
            Some((candidate, distance_ns))
        })
        .collect::<Vec<_>>();
    if matching_candidates.is_empty() {
        return None;
    }

    matching_candidates
        .into_iter()
        .filter_map(|(candidate, distance_ns)| {
            let tier = session_event_candidate_tier(candidate, target_repo_url)?;
            Some(SessionEventCandidateSelection {
                candidate,
                distance_ns,
                tier,
            })
        })
        .min_by(|left, right| {
            left.tier
                .score()
                .cmp(&right.tier.score())
                .then_with(|| left.distance_ns.cmp(&right.distance_ns))
                .then_with(|| right.candidate.row_id.cmp(&left.candidate.row_id))
        })
}

fn session_event_candidate_tier(
    candidate: &SessionEventRecoveryCandidate,
    target_repo_url: &str,
) -> Option<SessionEventCandidateTier> {
    (candidate.repo_url.as_deref() == Some(target_repo_url))
        .then_some(SessionEventCandidateTier::SameRepoUrl)
}

pub(super) fn session_event_distance(
    candidate: &SessionEventRecoveryCandidate,
    timestamps: &[u128],
) -> Option<u128> {
    timestamps
        .iter()
        .map(|timestamp_ns| distance_to_event_second(*timestamp_ns, candidate.event_ts))
        .min()
}

fn distance_to_event_second(timestamp_ns: u128, event_ts: u32) -> u128 {
    let start_ns = event_ts as u128 * NS_PER_SECOND;
    let end_ns = start_ns.saturating_add(NS_PER_SECOND - 1);
    if timestamp_ns < start_ns {
        start_ns - timestamp_ns
    } else {
        timestamp_ns.saturating_sub(end_ns)
    }
}

pub(super) fn insert_session_event_record(
    authorship_log: &mut AuthorshipLog,
    candidate: &SessionEventRecoveryCandidate,
    human_author: &str,
) {
    authorship_log
        .metadata
        .sessions
        .entry(candidate.session_id.clone())
        .or_insert_with(|| SessionRecord {
            agent_id: AgentId {
                tool: candidate.tool.clone(),
                id: candidate.external_session_id.clone(),
                model: session_event_model(candidate),
            },
            human_author: Some(human_author.to_string()),
            custom_attributes: None,
        });
}

pub(super) fn session_event_model(candidate: &SessionEventRecoveryCandidate) -> String {
    candidate
        .model
        .clone()
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::session_recovery_candidate::SessionEventRecoveryCandidate;

    fn session_event_candidate(
        row_id: i64,
        session_id: &str,
        external_session_id: &str,
        event_ts: u32,
        repo_url: Option<&str>,
    ) -> SessionEventRecoveryCandidate {
        SessionEventRecoveryCandidate {
            row_id,
            event_ts,
            session_id: session_id.to_string(),
            trace_id: Some(format!("trace-{row_id}")),
            tool: "codex".to_string(),
            model: Some("gpt-5".to_string()),
            external_session_id: external_session_id.to_string(),
            external_tool_use_id: Some(format!("tool-use-{row_id}")),
            repo_url: repo_url.map(ToString::to_string),
        }
    }

    #[test]
    fn session_event_candidate_ranking_prefers_matching_repo_url() {
        let timestamp_ns = 1_700_000_001_500_000_000;
        let candidates = vec![
            session_event_candidate(
                1,
                "s_closer_time_only",
                "external-closer",
                1_700_000_001,
                None,
            ),
            session_event_candidate(
                2,
                "s_matching_repo",
                "external-matching",
                1_700_000_000,
                Some("https://github.com/acme/repo"),
            ),
        ];

        let selection = select_best_session_event_candidate(
            &candidates,
            &[timestamp_ns],
            "https://github.com/acme/repo",
        )
        .expect("expected session-event candidate");

        assert_eq!(selection.candidate.session_id, "s_matching_repo");
        assert_eq!(selection.tier, SessionEventCandidateTier::SameRepoUrl);
    }

    #[test]
    fn session_event_candidate_ranking_rejects_time_only_sessions() {
        let timestamp_ns = 1_700_000_001_500_000_000;
        let candidates = vec![session_event_candidate(
            1,
            "s_first",
            "external-first",
            1_700_000_001,
            None,
        )];

        let selection = select_best_session_event_candidate(
            &candidates,
            &[timestamp_ns],
            "https://github.com/acme/repo",
        );

        assert!(
            selection.is_none(),
            "session-event recovery must not attribute without a matching repo URL"
        );
    }

    #[test]
    fn session_event_candidate_distance_uses_event_second_bucket() {
        let event_ts = 1_700_000_000;
        let timestamp_ns = event_ts as u128 * NS_PER_SECOND + 3_500_000_000;
        let candidates = vec![session_event_candidate(
            1,
            "s_bucket",
            "external-bucket",
            event_ts,
            Some("https://github.com/acme/repo"),
        )];

        let selection = select_best_session_event_candidate(
            &candidates,
            &[timestamp_ns],
            "https://github.com/acme/repo",
        )
        .expect("expected second-bucket timestamp to match");

        assert_eq!(selection.candidate.session_id, "s_bucket");
        assert_eq!(selection.distance_ns, 2_500_000_001);
    }
}
