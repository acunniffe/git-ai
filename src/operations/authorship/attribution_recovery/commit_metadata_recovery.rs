use crate::error::GitAiError;
use crate::model::authorship_log::{LineRange, SessionRecord};
use crate::model::authorship_log_serialization::{AuthorshipLog, generate_trace_id};
use crate::model::session_recovery_candidate::SessionEventRecoveryCandidate;
use crate::model::working_log::AgentId;
use crate::operations::authorship::recovery_stores::RecoveryStores;
use crate::operations::git::repository::Repository;
use serde_json::json;
use std::collections::HashMap;

use super::commit_agent_metadata::{
    CommitAgentDetection, CommitAgentKind, CommitMetadata, detect_commit_metadata_agents,
};
use super::session_event_recovery::{session_event_distance, session_event_model};
use super::timestamp_collection::collect_unknown_file_timestamps;
use super::{
    FileTimestampsByPath, RecoveryMetricInput, SESSION_EVENT_RECOVERY_WINDOW_NS, add_attestation,
    ai_session_key, record_recovery_metric, unknown_lines_by_file,
};

#[derive(Clone, Debug)]
pub(super) struct CommitMetadataSessionSelection {
    pub(super) session_id: String,
    pub(super) agent_id: AgentId,
    pub(super) tier: &'static str,
    pub(super) metric_row_id: Option<i64>,
    pub(super) distance_ns: Option<u128>,
    pub(super) event_ts: Option<u32>,
    pub(super) repo_url: Option<String>,
    pub(super) external_tool_use_id: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn recover_commit_metadata(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    human_author: &str,
    authorship_log: &mut AuthorshipLog,
    committed_hunks: &HashMap<String, Vec<LineRange>>,
    captured_file_timestamps: Option<&FileTimestampsByPath>,
    stores: RecoveryStores,
) -> Result<(), GitAiError> {
    let unknown_by_file = unknown_lines_by_file(authorship_log, committed_hunks);
    if unknown_by_file.is_empty() {
        return Ok(());
    }

    let commit_metadata = read_commit_metadata(repo, commit_sha)?;
    let detections = detect_commit_metadata_agents(&commit_metadata);
    if detections.is_empty() {
        return Ok(());
    }

    let workdir = repo.workdir()?;
    let target_repo_url = crate::repo_url::resolve_repo_url_from_repo(repo);
    let timestamp_collection =
        collect_unknown_file_timestamps(&workdir, unknown_by_file, captured_file_timestamps);
    let latest_timestamps = latest_cohort_timestamps(&timestamp_collection.timestamps_by_file);
    let Some(selection) = select_commit_metadata_session(
        authorship_log,
        &detections,
        &latest_timestamps,
        target_repo_url.as_deref(),
        stores,
    )?
    else {
        return Ok(());
    };

    authorship_log
        .metadata
        .sessions
        .entry(selection.session_id.clone())
        .or_insert_with(|| SessionRecord {
            agent_id: selection.agent_id.clone(),
            human_author: Some(human_author.to_string()),
            custom_attributes: None,
        });

    let detected_agents = detections
        .iter()
        .map(|detection| {
            json!({
                "agent": detection.kind.key,
                "source": detection.source,
                "marker": detection.marker,
            })
        })
        .collect::<Vec<_>>();

    for (file_path, unknown_lines) in timestamp_collection.unknown_by_file {
        let trace_id = generate_trace_id();
        let author_id = format!("{}::{}", selection.session_id, trace_id);
        add_attestation(authorship_log, &file_path, &author_id, &unknown_lines);

        let file_timestamps = timestamp_collection
            .timestamps_by_file
            .get(&file_path)
            .cloned()
            .unwrap_or_default();
        let metadata = json!({
            "solver": "commit_metadata",
            "file_path": file_path.as_str(),
            "unknown_lines": &unknown_lines,
            "detected_agents": &detected_agents,
            "commit_author_name": commit_metadata.author_name.as_str(),
            "commit_author_email": commit_metadata.author_email.as_str(),
            "selection_tier": selection.tier,
            "selected_session_id": selection.session_id.as_str(),
            "selected_tool": selection.agent_id.tool.as_str(),
            "selected_model": selection.agent_id.model.as_str(),
            "selected_external_session_id": selection.agent_id.id.as_str(),
            "selected_external_tool_use_id": selection.external_tool_use_id.as_deref(),
            "selected_repo_url": selection.repo_url.as_deref(),
            "target_repo_url": target_repo_url.as_deref(),
            "selected_metric_row_id": selection.metric_row_id,
            "selected_event_ts": selection.event_ts,
            "distance_ns": selection.distance_ns,
            "file_timestamps_ns": file_timestamps,
            "latest_file_timestamps_ns": latest_timestamps,
        });
        record_recovery_metric(RecoveryMetricInput {
            repo,
            parent_sha,
            commit_sha,
            file_path: &file_path,
            author_id: &author_id,
            session_id: &selection.session_id,
            trace_id: &trace_id,
            tool: &selection.agent_id.tool,
            model: &selection.agent_id.model,
            external_session_id: &selection.agent_id.id,
            external_tool_use_id: selection.external_tool_use_id.as_deref(),
            edit_kind: "attribution_recovery_commit_metadata",
            checkpoint_type: "recovered_commit_metadata",
            recovered_line_count: unknown_lines.len() as u32,
            metadata,
            event_ts: selection.event_ts,
        });
    }

    Ok(())
}

fn read_commit_metadata(repo: &Repository, commit_sha: &str) -> Result<CommitMetadata, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "show".to_string(),
        "-s".to_string(),
        "--format=%an%x00%ae%x00%B".to_string(),
        commit_sha.to_string(),
    ]);
    let output = crate::clients::git_cli::exec_git(&args)?;
    let raw = String::from_utf8(output.stdout)?;
    let mut parts = raw.splitn(3, '\0');
    let author_name = parts.next().unwrap_or_default().trim().to_string();
    let author_email = parts.next().unwrap_or_default().trim().to_string();
    let message = parts.next().unwrap_or_default().to_string();

    Ok(CommitMetadata {
        message,
        author_name,
        author_email,
    })
}

fn latest_cohort_timestamps(timestamps_by_file: &FileTimestampsByPath) -> Vec<u128> {
    timestamps_by_file
        .values()
        .filter_map(|timestamps| timestamps.iter().copied().max())
        .max()
        .map(|latest| {
            let mut timestamps = timestamps_by_file
                .values()
                .filter(|file_timestamps| file_timestamps.iter().copied().max() == Some(latest))
                .flat_map(|file_timestamps| file_timestamps.iter().copied())
                .collect::<Vec<_>>();
            timestamps.sort_unstable();
            timestamps.dedup();
            timestamps
        })
        .unwrap_or_default()
}

pub(super) fn select_commit_metadata_session(
    authorship_log: &AuthorshipLog,
    detections: &[CommitAgentDetection],
    latest_timestamps: &[u128],
    target_repo_url: Option<&str>,
    stores: RecoveryStores,
) -> Result<Option<CommitMetadataSessionSelection>, GitAiError> {
    if detections.is_empty() {
        return Ok(None);
    }

    if let Some(selection) = select_existing_commit_metadata_session(authorship_log, detections) {
        return Ok(Some(selection));
    }
    if let Some(selection) = select_nearest_commit_metadata_metric_session(
        detections,
        latest_timestamps,
        target_repo_url,
        stores,
    )? {
        return Ok(Some(selection));
    }
    if let Some(selection) =
        select_latest_commit_metadata_metric_session(detections, target_repo_url, stores)?
    {
        return Ok(Some(selection));
    }

    Ok(Some(synthesized_commit_metadata_session(
        &detections[0].kind,
    )))
}

fn select_existing_commit_metadata_session(
    authorship_log: &AuthorshipLog,
    detections: &[CommitAgentDetection],
) -> Option<CommitMetadataSessionSelection> {
    let mut best: Option<(usize, usize, String, AgentId)> = None;
    for (detection_index, detection) in detections.iter().enumerate() {
        for (session_id, session) in &authorship_log.metadata.sessions {
            if !agent_kind_matches_tool(&detection.kind, &session.agent_id.tool) {
                continue;
            }
            let attested_count = attested_line_count_for_session(authorship_log, session_id);
            let replace = best.as_ref().is_none_or(
                |(best_detection_index, best_attested_count, best_session_id, _)| {
                    detection_index < *best_detection_index
                        || (detection_index == *best_detection_index
                            && (attested_count > *best_attested_count
                                || (attested_count == *best_attested_count
                                    && session_id < best_session_id)))
                },
            );
            if replace {
                best = Some((
                    detection_index,
                    attested_count,
                    session_id.clone(),
                    session.agent_id.clone(),
                ));
            }
        }
    }

    best.map(
        |(_, _, session_id, agent_id)| CommitMetadataSessionSelection {
            session_id,
            agent_id,
            tier: "existing_commit_session",
            metric_row_id: None,
            distance_ns: None,
            event_ts: None,
            repo_url: None,
            external_tool_use_id: None,
        },
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitMetadataMetricRepoTier {
    SameRepoUrl,
    UnknownRepoUrl,
}

impl CommitMetadataMetricRepoTier {
    fn score(self) -> u8 {
        match self {
            Self::SameRepoUrl => 0,
            Self::UnknownRepoUrl => 1,
        }
    }
}

fn commit_metadata_metric_repo_tier(
    candidate: &SessionEventRecoveryCandidate,
    target_repo_url: Option<&str>,
) -> Option<CommitMetadataMetricRepoTier> {
    match (target_repo_url, candidate.repo_url.as_deref()) {
        (Some(target), Some(candidate_url)) if candidate_url == target => {
            Some(CommitMetadataMetricRepoTier::SameRepoUrl)
        }
        (Some(_), Some(_)) => None,
        _ => Some(CommitMetadataMetricRepoTier::UnknownRepoUrl),
    }
}

fn attested_line_count_for_session(authorship_log: &AuthorshipLog, session_id: &str) -> usize {
    authorship_log
        .attestations
        .iter()
        .flat_map(|attestation| &attestation.entries)
        .filter(|entry| ai_session_key(&entry.hash) == session_id)
        .flat_map(|entry| entry.line_ranges.iter().flat_map(LineRange::expand))
        .count()
}

fn select_nearest_commit_metadata_metric_session(
    detections: &[CommitAgentDetection],
    latest_timestamps: &[u128],
    target_repo_url: Option<&str>,
    stores: RecoveryStores,
) -> Result<Option<CommitMetadataSessionSelection>, GitAiError> {
    if latest_timestamps.is_empty() {
        return Ok(None);
    }

    let candidates = match stores.metrics.and_then(|db| db.lock().ok()) {
        Some(db) => db.session_event_candidates_near_timestamps(
            latest_timestamps,
            SESSION_EVENT_RECOVERY_WINDOW_NS,
        )?,
        None => Vec::new(),
    };

    let mut best: Option<(
        usize,
        CommitMetadataMetricRepoTier,
        u128,
        i64,
        CommitMetadataSessionSelection,
    )> = None;
    for candidate in &candidates {
        let Some((detection_index, _)) = detections
            .iter()
            .enumerate()
            .find(|(_, detection)| agent_kind_matches_tool(&detection.kind, &candidate.tool))
        else {
            continue;
        };
        let Some(distance_ns) = session_event_distance(candidate, latest_timestamps) else {
            continue;
        };
        if distance_ns > SESSION_EVENT_RECOVERY_WINDOW_NS {
            continue;
        }
        let Some(repo_tier) = commit_metadata_metric_repo_tier(candidate, target_repo_url) else {
            continue;
        };

        let selection = commit_metadata_selection_from_metric_candidate(
            candidate,
            "nearest_file_timestamp",
            Some(distance_ns),
        );
        let replace = best.as_ref().is_none_or(
            |(best_detection_index, best_repo_tier, best_distance_ns, best_row_id, _)| {
                detection_index < *best_detection_index
                    || (detection_index == *best_detection_index
                        && (repo_tier.score() < best_repo_tier.score()
                            || (repo_tier.score() == best_repo_tier.score()
                                && (distance_ns < *best_distance_ns
                                    || (distance_ns == *best_distance_ns
                                        && candidate.row_id > *best_row_id)))))
            },
        );
        if replace {
            best = Some((
                detection_index,
                repo_tier,
                distance_ns,
                candidate.row_id,
                selection,
            ));
        }
    }

    Ok(best.map(|(_, _, _, _, selection)| selection))
}

fn select_latest_commit_metadata_metric_session(
    detections: &[CommitAgentDetection],
    target_repo_url: Option<&str>,
    stores: RecoveryStores,
) -> Result<Option<CommitMetadataSessionSelection>, GitAiError> {
    let Some(db) = stores.metrics.and_then(|db| db.lock().ok()) else {
        return Ok(None);
    };

    for detection in detections {
        let candidates = db.latest_session_event_candidates_for_tools(detection.kind.tools)?;
        if let Some((candidate, _)) = candidates
            .iter()
            .filter_map(|candidate| {
                let repo_tier = commit_metadata_metric_repo_tier(candidate, target_repo_url)?;
                Some((candidate, repo_tier))
            })
            .min_by(
                |(left_candidate, left_tier), (right_candidate, right_tier)| {
                    left_tier
                        .score()
                        .cmp(&right_tier.score())
                        .then_with(|| right_candidate.event_ts.cmp(&left_candidate.event_ts))
                        .then_with(|| right_candidate.row_id.cmp(&left_candidate.row_id))
                },
            )
        {
            return Ok(Some(commit_metadata_selection_from_metric_candidate(
                candidate,
                "latest_matching_tool_session",
                None,
            )));
        }
    }

    Ok(None)
}

fn commit_metadata_selection_from_metric_candidate(
    candidate: &SessionEventRecoveryCandidate,
    tier: &'static str,
    distance_ns: Option<u128>,
) -> CommitMetadataSessionSelection {
    CommitMetadataSessionSelection {
        session_id: candidate.session_id.clone(),
        agent_id: AgentId {
            tool: candidate.tool.clone(),
            id: candidate.external_session_id.clone(),
            model: session_event_model(candidate),
        },
        tier,
        metric_row_id: Some(candidate.row_id),
        distance_ns,
        event_ts: Some(candidate.event_ts),
        repo_url: candidate.repo_url.clone(),
        external_tool_use_id: candidate.external_tool_use_id.clone(),
    }
}

fn synthesized_commit_metadata_session(kind: &CommitAgentKind) -> CommitMetadataSessionSelection {
    let session_id = generate_random_session_id();
    CommitMetadataSessionSelection {
        session_id: session_id.clone(),
        agent_id: AgentId {
            tool: kind.tools.first().copied().unwrap_or(kind.key).to_string(),
            id: session_id,
            model: "unknown".to_string(),
        },
        tier: "synthesized_session",
        metric_row_id: None,
        distance_ns: None,
        event_ts: None,
        repo_url: None,
        external_tool_use_id: None,
    }
}

fn generate_random_session_id() -> String {
    let trace_id = generate_trace_id();
    format!("s_{}", &trace_id[2..])
}

pub(super) fn agent_kind_matches_tool(kind: &CommitAgentKind, tool: &str) -> bool {
    kind.tools
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(tool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn latest_cohort_keeps_every_timestamp_from_files_at_the_latest_time() {
        let unknown_by_file = BTreeMap::from([
            ("alpha.rs".to_string(), vec![1]),
            ("beta.rs".to_string(), vec![2]),
            ("older.rs".to_string(), vec![3]),
        ]);
        let captured_timestamps = HashMap::from([
            ("alpha.rs".to_string(), vec![9, 1]),
            ("beta.rs".to_string(), vec![3, 9, 3]),
            ("older.rs".to_string(), vec![7]),
        ]);

        let timestamp_collection = collect_unknown_file_timestamps(
            std::path::Path::new("/definitely-missing-workdir"),
            unknown_by_file,
            Some(&captured_timestamps),
        );

        assert_eq!(timestamp_collection.timestamps_by_file.len(), 3);
        let latest_timestamps = latest_cohort_timestamps(&timestamp_collection.timestamps_by_file);
        assert_eq!(latest_timestamps, vec![1, 3, 9]);
    }
}
