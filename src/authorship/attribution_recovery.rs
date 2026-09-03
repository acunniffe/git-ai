use crate::authorship::authorship_log::{HumanRecord, LineRange, SessionRecord};
use crate::authorship::authorship_log_serialization::{
    AuthorshipLog, generate_human_short_hash, generate_session_id, generate_trace_id,
};
use crate::authorship::working_log::{AgentId, CheckpointKind};
use crate::commands::checkpoint_agent::bash_tool::StatEntry;
use crate::daemon::bash_history_db::{BashCheckpointCall, distance_to_call_window};
use crate::error::GitAiError;
use crate::git::repo_state::worktree_root_for_path;
use crate::git::repository::{Repository, exec_git};
use crate::metrics::db::SessionEventRecoveryCandidate;
use crate::metrics::{CheckpointValues, EventAttributes, MetricEvent, PosEncoded};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const BASH_RECOVERY_WINDOW_NS: u128 = 3_000_000_000;
const BASH_RECOVERY_COARSE_TIMESTAMP_NS: u128 = 1_000_000_000;
pub(crate) const SESSION_EVENT_RECOVERY_WINDOW_NS: u128 = 3_000_000_000;
const EDGE_EXTENSION_MAX_LINES: usize = 3;
const NS_PER_SECOND: u128 = 1_000_000_000;

const CODEX_TOOLS: &[&str] = &["codex", "codex-cloud"];
const CLAUDE_TOOLS: &[&str] = &["claude", "claude-web"];
const CURSOR_TOOLS: &[&str] = &["cursor", "cursor-agent"];
const COPILOT_TOOLS: &[&str] = &[
    "github-copilot",
    "github-copilot-cli",
    "github-copilot-agent",
    "copilot",
];
const DEVIN_TOOLS: &[&str] = &["devin"];
const DROID_TOOLS: &[&str] = &["droid"];
const WINDSURF_TOOLS: &[&str] = &["windsurf"];
const AMP_TOOLS: &[&str] = &["amp"];
const OPENCODE_TOOLS: &[&str] = &["opencode"];
const GEMINI_TOOLS: &[&str] = &["gemini"];
const CONTINUE_TOOLS: &[&str] = &["continue-cli"];

const CODEX_EMAILS: &[&str] = &["codex@openai.com"];
const CLAUDE_EMAILS: &[&str] = &[];
const CURSOR_EMAILS: &[&str] = &["cursoragent@cursor.com"];
const COPILOT_EMAILS: &[&str] = &["+copilot@users.noreply.github.com"];
const DEVIN_EMAILS: &[&str] = &["+devin-ai-integration[bot]@users.noreply.github.com"];
const DROID_EMAILS: &[&str] = &["+factory-droid[bot]@users.noreply.github.com"];
const WINDSURF_EMAILS: &[&str] = &["noreply@windsurf.com", "noreply@codeium.com"];
const AMP_EMAILS: &[&str] = &[];
const OPENCODE_EMAILS: &[&str] = &[];
const GEMINI_EMAILS: &[&str] = &[];
const CONTINUE_EMAILS: &[&str] = &[];

const CODEX_MARKER_EMAILS: &[&str] = &["noreply@openai.com"];
const CLAUDE_MARKER_EMAILS: &[&str] = &["noreply@anthropic.com"];
const CURSOR_MARKER_EMAILS: &[&str] = &[];
const COPILOT_MARKER_EMAILS: &[&str] = &[];
const DEVIN_MARKER_EMAILS: &[&str] = &[];
const DROID_MARKER_EMAILS: &[&str] = &[];
const WINDSURF_MARKER_EMAILS: &[&str] = &[];
const AMP_MARKER_EMAILS: &[&str] = &[];
const OPENCODE_MARKER_EMAILS: &[&str] = &[];
const GEMINI_MARKER_EMAILS: &[&str] = &[];
const CONTINUE_MARKER_EMAILS: &[&str] = &[];

const CODEX_MARKERS: &[&str] = &["codex"];
const CLAUDE_MARKERS: &[&str] = &["claude"];
const CURSOR_MARKERS: &[&str] = &["cursor"];
const COPILOT_MARKERS: &[&str] = &["copilot", "github copilot"];
const DEVIN_MARKERS: &[&str] = &["devin"];
const DROID_MARKERS: &[&str] = &["droid", "factory-droid"];
const WINDSURF_MARKERS: &[&str] = &["windsurf"];
const AMP_MARKERS: &[&str] = &["ampcode", "amp code"];
const OPENCODE_MARKERS: &[&str] = &["opencode", "open code"];
const GEMINI_MARKERS: &[&str] = &["gemini"];
const CONTINUE_MARKERS: &[&str] = &["continue-cli"];

const KNOWN_COMMIT_AGENT_KINDS: &[CommitAgentKind] = &[
    CommitAgentKind {
        key: "codex",
        tools: CODEX_TOOLS,
        emails: CODEX_EMAILS,
        marker_emails: CODEX_MARKER_EMAILS,
        markers: CODEX_MARKERS,
    },
    CommitAgentKind {
        key: "claude",
        tools: CLAUDE_TOOLS,
        emails: CLAUDE_EMAILS,
        marker_emails: CLAUDE_MARKER_EMAILS,
        markers: CLAUDE_MARKERS,
    },
    CommitAgentKind {
        key: "cursor",
        tools: CURSOR_TOOLS,
        emails: CURSOR_EMAILS,
        marker_emails: CURSOR_MARKER_EMAILS,
        markers: CURSOR_MARKERS,
    },
    CommitAgentKind {
        key: "github-copilot",
        tools: COPILOT_TOOLS,
        emails: COPILOT_EMAILS,
        marker_emails: COPILOT_MARKER_EMAILS,
        markers: COPILOT_MARKERS,
    },
    CommitAgentKind {
        key: "devin",
        tools: DEVIN_TOOLS,
        emails: DEVIN_EMAILS,
        marker_emails: DEVIN_MARKER_EMAILS,
        markers: DEVIN_MARKERS,
    },
    CommitAgentKind {
        key: "droid",
        tools: DROID_TOOLS,
        emails: DROID_EMAILS,
        marker_emails: DROID_MARKER_EMAILS,
        markers: DROID_MARKERS,
    },
    CommitAgentKind {
        key: "windsurf",
        tools: WINDSURF_TOOLS,
        emails: WINDSURF_EMAILS,
        marker_emails: WINDSURF_MARKER_EMAILS,
        markers: WINDSURF_MARKERS,
    },
    CommitAgentKind {
        key: "amp",
        tools: AMP_TOOLS,
        emails: AMP_EMAILS,
        marker_emails: AMP_MARKER_EMAILS,
        markers: AMP_MARKERS,
    },
    CommitAgentKind {
        key: "opencode",
        tools: OPENCODE_TOOLS,
        emails: OPENCODE_EMAILS,
        marker_emails: OPENCODE_MARKER_EMAILS,
        markers: OPENCODE_MARKERS,
    },
    CommitAgentKind {
        key: "gemini",
        tools: GEMINI_TOOLS,
        emails: GEMINI_EMAILS,
        marker_emails: GEMINI_MARKER_EMAILS,
        markers: GEMINI_MARKERS,
    },
    CommitAgentKind {
        key: "continue-cli",
        tools: CONTINUE_TOOLS,
        emails: CONTINUE_EMAILS,
        marker_emails: CONTINUE_MARKER_EMAILS,
        markers: CONTINUE_MARKERS,
    },
];

pub(crate) type FileTimestampsByPath = HashMap<String, Vec<u128>>;
pub(crate) type UnknownLinesByFile = BTreeMap<String, Vec<u32>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CommitAgentKind {
    key: &'static str,
    tools: &'static [&'static str],
    emails: &'static [&'static str],
    marker_emails: &'static [&'static str],
    markers: &'static [&'static str],
}

#[derive(Clone, Debug)]
struct CommitAgentDetection {
    kind: CommitAgentKind,
    source: &'static str,
    marker: String,
}

#[derive(Debug)]
struct CommitMetadata {
    message: String,
    author_name: String,
    author_email: String,
}

#[derive(Clone, Debug)]
struct CommitMetadataSessionSelection {
    session_id: String,
    agent_id: AgentId,
    tier: &'static str,
    metric_row_id: Option<i64>,
    distance_ns: Option<u128>,
    event_ts: Option<u32>,
    repo_url: Option<String>,
    external_tool_use_id: Option<String>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct AttributionRecoveryContext<'a> {
    pub(crate) file_timestamps: Option<&'a FileTimestampsByPath>,
    pub(crate) before_external_recovery: Option<&'a dyn Fn(&UnknownLinesByFile)>,
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

    recover_bash_mtime(
        repo,
        parent_sha,
        commit_sha,
        human_author,
        authorship_log,
        committed_hunks,
        context.file_timestamps,
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

    let mut metrics_db_unavailable = false;
    recover_session_event_mtime(
        repo,
        parent_sha,
        commit_sha,
        human_author,
        authorship_log,
        committed_hunks,
        context.file_timestamps,
        &mut metrics_db_unavailable,
    )?;

    let unknown_after_session_events = unknown_lines_by_file(authorship_log, committed_hunks);
    if unknown_after_session_events.is_empty() {
        return Ok(());
    }

    if recover_commit_metadata(
        repo,
        parent_sha,
        commit_sha,
        human_author,
        authorship_log,
        &unknown_after_session_events,
        context.file_timestamps,
        &mut metrics_db_unavailable,
    )? {
        return Ok(());
    }

    // The metrics DB could not be consulted (busy/broken): "no matching AI
    // session" was never established, so assigning the remaining lines to the
    // human author would be misattribution, not a conservative default. Leave
    // them unattributed instead; losing attribution is acceptable, inventing
    // it is not.
    if metrics_db_unavailable {
        tracing::warn!(
            commit = %commit_sha,
            "metrics DB unavailable during attribution recovery; leaving unknown lines unattributed"
        );
        return Ok(());
    }

    recover_remaining_as_known_human(authorship_log, human_author, unknown_after_session_events);
    Ok(())
}

/// Budget for acquiring the metrics DB mutex on the post-commit path. Session
/// event recovery is best-effort: when telemetry holds the database busy,
/// give up quickly (the caller treats "no candidate" conservatively) instead
/// of stalling commit processing behind telemetry work.
const SESSION_EVENT_CANDIDATE_LOCK_BUDGET: std::time::Duration =
    std::time::Duration::from_millis(500);

/// Try to acquire a mutex within a bounded budget, polling instead of
/// blocking, so a contended lock cannot stall the caller indefinitely.
pub(crate) fn try_lock_with_budget<T>(
    mutex: &std::sync::Mutex<T>,
    budget: std::time::Duration,
) -> Option<std::sync::MutexGuard<'_, T>> {
    let started = std::time::Instant::now();
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Some(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                return Some(poisoned.into_inner());
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        if started.elapsed() >= budget {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Whether a matching session-event recovery candidate exists.
///
/// `Some(true)`/`Some(false)` are definitive answers; `None` means the
/// metrics DB could not be consulted (lock budget expired because telemetry
/// is holding it busy, the DB failed to open, or the query errored) and the
/// caller must not treat that as "no candidate" — doing so would trigger
/// sweep-and-wait work exactly when the commit path should not stall behind
/// telemetry.
pub(crate) fn matching_session_event_candidate_exists(
    timestamps_ns: &[u128],
    target_repo_url: &str,
) -> Option<bool> {
    if timestamps_ns.is_empty() || target_repo_url.is_empty() {
        return Some(false);
    }

    let db = crate::metrics::db::MetricsDatabase::global().ok()?;
    let candidates = try_lock_with_budget(db, SESSION_EVENT_CANDIDATE_LOCK_BUDGET)?
        .session_event_candidates_near_timestamps(timestamps_ns, SESSION_EVENT_RECOVERY_WINDOW_NS)
        .map_err(|error| {
            tracing::debug!(%error, "session-event candidate query failed");
        })
        .ok()?;

    Some(select_best_session_event_candidate(&candidates, timestamps_ns, target_repo_url).is_some())
}

fn recover_bash_mtime(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    human_author: &str,
    authorship_log: &mut AuthorshipLog,
    committed_hunks: &HashMap<String, Vec<LineRange>>,
    captured_file_timestamps: Option<&FileTimestampsByPath>,
) -> Result<(), GitAiError> {
    let repo_work_dir = repo_worktree_key(repo)?;
    let workdir = repo.workdir()?;
    let unknown_by_file = unknown_lines_by_file(authorship_log, committed_hunks);
    if unknown_by_file.is_empty() {
        return Ok(());
    }

    let mut timestamps_by_file = HashMap::new();
    let mut all_timestamps = Vec::new();
    for file_path in unknown_by_file.keys() {
        let timestamps = captured_file_timestamps
            .and_then(|timestamps| timestamps.get(file_path))
            .filter(|timestamps| !timestamps.is_empty())
            .cloned()
            .unwrap_or_else(|| file_timestamps_ns(&workdir, file_path));
        if !timestamps.is_empty() {
            all_timestamps.extend(timestamps.iter().copied());
            timestamps_by_file.insert(file_path.clone(), timestamps);
        }
    }
    if all_timestamps.is_empty() {
        return Ok(());
    }
    all_timestamps.sort_unstable();
    all_timestamps.dedup();

    let candidates = match crate::daemon::bash_history_db::BashHistoryDatabase::global() {
        Ok(db) => match db.lock() {
            Ok(db) => db.candidates_near_timestamps(&all_timestamps, BASH_RECOVERY_WINDOW_NS)?,
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    if candidates.is_empty() {
        return Ok(());
    }

    let existing_commit_sessions = existing_commit_session_ids(authorship_log);
    for (file_path, unknown_lines) in unknown_by_file {
        let Some(timestamps) = timestamps_by_file.get(&file_path) else {
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

        let mut agent_id = candidate.agent_id.clone();
        crate::streams::model_extraction::enrich_copilot_agent_model(
            &mut agent_id,
            &candidate.metadata,
        );
        let trace_id = generate_trace_id();
        let session_id = generate_session_id(&agent_id.id, &agent_id.tool);
        let author_id = format!("{}::{}", session_id, trace_id);
        insert_session_record(authorship_log, &session_id, &agent_id, human_author);
        add_attestation(authorship_log, &file_path, &author_id, &unknown_lines);

        let metadata = json!({
            "solver": "bash_mtime",
            "file_path": file_path,
            "unknown_lines": unknown_lines,
            "target_repo_work_dir": repo_work_dir.as_str(),
            "file_timestamps_ns": timestamps,
            "selected_bash_call_id": candidate.id,
            "selected_bash_original_cwd": candidate.original_cwd.as_str(),
            "selected_bash_repo_work_dir": candidate.repo_work_dir.as_deref(),
            "selected_bash_repo_discovery_error": candidate.repo_discovery_error.as_deref(),
            "selected_tool_use_id": candidate.tool_use_id,
            "selected_command": candidate.command,
            "distance_ns": distance_ns,
            "window_ns": BASH_RECOVERY_WINDOW_NS,
            "start_time_ns": candidate.start_time_ns,
            "end_time_ns": candidate.end_time_ns,
            "start_trace_id": candidate.start_trace_id,
            "end_trace_id": candidate.end_trace_id,
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
            tool: &agent_id.tool,
            model: &agent_id.model,
            external_session_id: &agent_id.id,
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

#[allow(clippy::too_many_arguments)]
fn recover_session_event_mtime(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    human_author: &str,
    authorship_log: &mut AuthorshipLog,
    committed_hunks: &HashMap<String, Vec<LineRange>>,
    captured_file_timestamps: Option<&FileTimestampsByPath>,
    metrics_db_unavailable: &mut bool,
) -> Result<(), GitAiError> {
    let workdir = repo.workdir()?;
    let unknown_by_file = unknown_lines_by_file(authorship_log, committed_hunks);
    if unknown_by_file.is_empty() {
        return Ok(());
    }

    let mut timestamps_by_file = HashMap::new();
    let mut all_timestamps = Vec::new();
    for file_path in unknown_by_file.keys() {
        let timestamps = captured_file_timestamps
            .and_then(|timestamps| timestamps.get(file_path))
            .filter(|timestamps| !timestamps.is_empty())
            .cloned()
            .unwrap_or_else(|| file_timestamps_ns(&workdir, file_path));
        if !timestamps.is_empty() {
            all_timestamps.extend(timestamps.iter().copied());
            timestamps_by_file.insert(file_path.clone(), timestamps);
        }
    }
    if all_timestamps.is_empty() {
        return Ok(());
    }
    all_timestamps.sort_unstable();
    all_timestamps.dedup();

    let candidates = match crate::metrics::db::MetricsDatabase::global() {
        // Bounded acquire: recovery is best-effort and must not stall commit
        // processing behind telemetry holding the metrics DB. An expired
        // budget is recorded as "unavailable", never as "no candidates".
        Ok(db) => match try_lock_with_budget(db, SESSION_EVENT_CANDIDATE_LOCK_BUDGET) {
            Some(db) => match db.session_event_candidates_near_timestamps(
                &all_timestamps,
                SESSION_EVENT_RECOVERY_WINDOW_NS,
            ) {
                Ok(candidates) => candidates,
                Err(error) => {
                    tracing::debug!(%error, "session-event candidate query failed");
                    *metrics_db_unavailable = true;
                    return Ok(());
                }
            },
            None => {
                *metrics_db_unavailable = true;
                return Ok(());
            }
        },
        Err(_) => {
            *metrics_db_unavailable = true;
            return Ok(());
        }
    };
    if candidates.is_empty() {
        return Ok(());
    }

    let Some(target_repo_url) = crate::repo_url::resolve_repo_url_from_repo(repo) else {
        return Ok(());
    };
    for (file_path, unknown_lines) in unknown_by_file {
        let Some(timestamps) = timestamps_by_file.get(&file_path) else {
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

        let selected_model = recovery_model_for_session(authorship_log, &candidate.session_id)
            .unwrap_or_else(|| session_event_model(candidate));
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

#[allow(clippy::too_many_arguments)]
fn recover_commit_metadata(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    human_author: &str,
    authorship_log: &mut AuthorshipLog,
    unknown_by_file: &UnknownLinesByFile,
    captured_file_timestamps: Option<&FileTimestampsByPath>,
    metrics_db_unavailable: &mut bool,
) -> Result<bool, GitAiError> {
    if unknown_by_file.is_empty() {
        return Ok(true);
    }

    let commit_metadata = read_commit_metadata(repo, commit_sha)?;
    let detections = detect_commit_metadata_agents(&commit_metadata);
    if detections.is_empty() {
        return Ok(false);
    }

    let workdir = repo.workdir()?;
    let target_repo_url = crate::repo_url::resolve_repo_url_from_repo(repo);
    let (timestamps_by_file, latest_timestamps) =
        latest_timestamps_for_unknown_files(&workdir, unknown_by_file, captured_file_timestamps);
    let Some(selection) = select_commit_metadata_session(
        authorship_log,
        &detections,
        &latest_timestamps,
        target_repo_url.as_deref(),
        metrics_db_unavailable,
    )?
    else {
        return Ok(false);
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
    let selected_model = recovery_model_for_session(authorship_log, &selection.session_id)
        .unwrap_or_else(|| selection.agent_id.model.clone());

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

    for (file_path, unknown_lines) in unknown_by_file {
        let trace_id = generate_trace_id();
        let author_id = format!("{}::{}", selection.session_id, trace_id);
        add_attestation(authorship_log, file_path, &author_id, unknown_lines);

        let file_timestamps = timestamps_by_file
            .get(file_path)
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
            "selected_model": selected_model.as_str(),
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
            file_path,
            author_id: &author_id,
            session_id: &selection.session_id,
            trace_id: &trace_id,
            tool: &selection.agent_id.tool,
            model: &selected_model,
            external_session_id: &selection.agent_id.id,
            external_tool_use_id: selection.external_tool_use_id.as_deref(),
            edit_kind: "attribution_recovery_commit_metadata",
            checkpoint_type: "recovered_commit_metadata",
            recovered_line_count: unknown_lines.len() as u32,
            metadata,
            event_ts: selection.event_ts,
        });
    }

    Ok(true)
}

fn recover_remaining_as_known_human(
    authorship_log: &mut AuthorshipLog,
    human_author: &str,
    unknown_by_file: UnknownLinesByFile,
) {
    if !should_recover_remaining_as_known_human(authorship_log) {
        return;
    }

    let human_id = generate_human_short_hash(human_author);
    authorship_log
        .metadata
        .humans
        .entry(human_id.clone())
        .or_insert_with(|| HumanRecord {
            author: human_author.to_string(),
        });

    for (file_path, unknown_lines) in unknown_by_file {
        add_attestation_ranges(
            authorship_log,
            &file_path,
            &human_id,
            LineRange::compress_lines(&unknown_lines),
        );
    }
}

fn should_recover_remaining_as_known_human(authorship_log: &AuthorshipLog) -> bool {
    let mut has_ai_attribution = false;
    for entry in authorship_log
        .attestations
        .iter()
        .flat_map(|attestation| &attestation.entries)
    {
        if entry.hash.starts_with("h_") {
            return true;
        }
        has_ai_attribution |= is_ai_attestation(&entry.hash);
    }
    !has_ai_attribution
}

fn read_commit_metadata(repo: &Repository, commit_sha: &str) -> Result<CommitMetadata, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "show".to_string(),
        "-s".to_string(),
        "--format=%an%x00%ae%x00%B".to_string(),
        commit_sha.to_string(),
    ]);
    let output = exec_git(&args)?;
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

fn detect_commit_metadata_agents(metadata: &CommitMetadata) -> Vec<CommitAgentDetection> {
    let mut detections = Vec::new();
    for line in metadata.message.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if let Some((name, value)) = trimmed.split_once(':') {
            let name_lower = name.trim().to_ascii_lowercase();
            if name_lower == "co-authored-by" {
                if let Some(kind) = detect_agent_from_identity(value) {
                    push_commit_agent_detection(
                        &mut detections,
                        kind,
                        "co_authored_by",
                        value.trim(),
                    );
                }
                continue;
            }
        }

        if lower.starts_with("claude-session:") {
            push_commit_agent_detection(
                &mut detections,
                commit_agent_kind_by_key("claude"),
                "session_trailer",
                trimmed,
            );
        } else if lower.starts_with("codex-session:") {
            push_commit_agent_detection(
                &mut detections,
                commit_agent_kind_by_key("codex"),
                "session_trailer",
                trimmed,
            );
        } else if lower.starts_with("cursor-session:") {
            push_commit_agent_detection(
                &mut detections,
                commit_agent_kind_by_key("cursor"),
                "session_trailer",
                trimmed,
            );
        }
    }

    let author_identity = format!("{} <{}>", metadata.author_name, metadata.author_email);
    if let Some(kind) = detect_agent_from_identity(&author_identity) {
        push_commit_agent_detection(&mut detections, kind, "author_identity", &author_identity);
    }

    detections
}

fn commit_agent_kind_by_key(key: &str) -> CommitAgentKind {
    KNOWN_COMMIT_AGENT_KINDS
        .iter()
        .copied()
        .find(|kind| kind.key == key)
        .expect("known commit agent key should exist")
}

fn push_commit_agent_detection(
    detections: &mut Vec<CommitAgentDetection>,
    kind: CommitAgentKind,
    source: &'static str,
    marker: &str,
) {
    if detections
        .iter()
        .any(|detection| detection.kind.key == kind.key)
    {
        return;
    }
    detections.push(CommitAgentDetection {
        kind,
        source,
        marker: marker.to_string(),
    });
}

fn detect_agent_from_identity(identity: &str) -> Option<CommitAgentKind> {
    let lower = identity.to_ascii_lowercase();
    let email = email_from_identity(identity);
    if let Some(email) = email.as_deref()
        && let Some(kind) = detect_agent_from_email(email)
    {
        return Some(kind);
    }

    KNOWN_COMMIT_AGENT_KINDS.iter().copied().find(|kind| {
        let marker_matches = kind
            .markers
            .iter()
            .any(|marker| contains_identity_marker(&lower, marker));
        if !marker_matches {
            return false;
        }

        match email.as_deref() {
            Some(email) => kind
                .marker_emails
                .iter()
                .any(|pattern| email_matches_pattern(email, pattern)),
            None => true,
        }
    })
}

fn contains_identity_marker(identity_lower: &str, marker: &str) -> bool {
    let marker_lower = marker.to_ascii_lowercase();
    let mut search_start = 0;
    while let Some(relative_start) = identity_lower[search_start..].find(&marker_lower) {
        let start = search_start + relative_start;
        let end = start + marker_lower.len();
        let before = identity_lower[..start].chars().next_back();
        let after = identity_lower[end..].chars().next();
        let before_boundary = before.is_none_or(|ch| !ch.is_ascii_alphanumeric());
        let after_boundary = after.is_none_or(|ch| !ch.is_ascii_alphanumeric());
        if before_boundary && after_boundary {
            return true;
        }
        search_start = end;
    }
    false
}

fn detect_agent_from_email(email: &str) -> Option<CommitAgentKind> {
    let email = email.trim().trim_matches('<').trim_matches('>');
    if email.is_empty() {
        return None;
    }

    KNOWN_COMMIT_AGENT_KINDS.iter().copied().find(|kind| {
        kind.emails
            .iter()
            .any(|pattern| email_matches_pattern(email, pattern))
    })
}

fn email_matches_pattern(email: &str, pattern: &str) -> bool {
    let email_lower = email.trim().to_ascii_lowercase();
    let pattern_lower = pattern.to_ascii_lowercase();
    if pattern_lower.starts_with('+') {
        email_lower.ends_with(&pattern_lower)
    } else {
        email_lower == pattern_lower
    }
}

fn email_from_identity(identity: &str) -> Option<String> {
    let start = identity.find('<')?;
    let end = identity[start + 1..].find('>')? + start + 1;
    Some(identity[start + 1..end].trim().to_string())
}

fn latest_timestamps_for_unknown_files(
    workdir: &std::path::Path,
    unknown_by_file: &UnknownLinesByFile,
    captured_file_timestamps: Option<&FileTimestampsByPath>,
) -> (HashMap<String, Vec<u128>>, Vec<u128>) {
    let mut timestamps_by_file = HashMap::new();
    let mut latest_timestamp = None;
    for file_path in unknown_by_file.keys() {
        let timestamps = captured_file_timestamps
            .and_then(|timestamps| timestamps.get(file_path))
            .filter(|timestamps| !timestamps.is_empty())
            .cloned()
            .unwrap_or_else(|| file_timestamps_ns(workdir, file_path));
        if let Some(file_latest) = timestamps.iter().copied().max() {
            latest_timestamp = Some(
                latest_timestamp.map_or(file_latest, |current: u128| current.max(file_latest)),
            );
            timestamps_by_file.insert(file_path.clone(), timestamps);
        }
    }

    let latest_timestamps = latest_timestamp
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
        .unwrap_or_default();

    (timestamps_by_file, latest_timestamps)
}

fn select_commit_metadata_session(
    authorship_log: &AuthorshipLog,
    detections: &[CommitAgentDetection],
    latest_timestamps: &[u128],
    target_repo_url: Option<&str>,
    metrics_db_unavailable: &mut bool,
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
        metrics_db_unavailable,
    )? {
        return Ok(Some(selection));
    }
    if let Some(selection) = select_latest_commit_metadata_metric_session(
        detections,
        target_repo_url,
        metrics_db_unavailable,
    )? {
        return Ok(Some(selection));
    }

    // The metrics DB could not be consulted: "no matching session" was never
    // established, so minting a synthesized session would be misattribution.
    // Reporting no-selection lets recover_attribution's unavailable guard
    // leave the lines unattributed instead.
    if *metrics_db_unavailable {
        return Ok(None);
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
    metrics_db_unavailable: &mut bool,
) -> Result<Option<CommitMetadataSessionSelection>, GitAiError> {
    if latest_timestamps.is_empty() {
        return Ok(None);
    }

    let candidates = match crate::metrics::db::MetricsDatabase::global() {
        // Bounded acquire: recovery is best-effort and must not stall commit
        // processing behind telemetry holding the metrics DB. An expired
        // budget is recorded as "unavailable", never as "no candidates".
        Ok(db) => match try_lock_with_budget(db, SESSION_EVENT_CANDIDATE_LOCK_BUDGET) {
            Some(db) => match db.session_event_candidates_near_timestamps(
                latest_timestamps,
                SESSION_EVENT_RECOVERY_WINDOW_NS,
            ) {
                Ok(candidates) => candidates,
                Err(error) => {
                    tracing::debug!(%error, "session-event candidate query failed");
                    *metrics_db_unavailable = true;
                    return Ok(None);
                }
            },
            None => {
                *metrics_db_unavailable = true;
                return Ok(None);
            }
        },
        Err(_) => {
            *metrics_db_unavailable = true;
            return Ok(None);
        }
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
    metrics_db_unavailable: &mut bool,
) -> Result<Option<CommitMetadataSessionSelection>, GitAiError> {
    // Only an exact repo_url match is safe here: this fallback has no time
    // window, so an unscoped session (no repo_url) could belong to any
    // commit on the machine. Without a target repo there is nothing to
    // match against, and the metrics DB does not need to be consulted.
    let Some(target_repo_url) = target_repo_url else {
        return Ok(None);
    };

    let db = match crate::metrics::db::MetricsDatabase::global() {
        Ok(db) => db,
        Err(_) => {
            *metrics_db_unavailable = true;
            return Ok(None);
        }
    };
    // Bounded acquire: recovery is best-effort and must not stall commit
    // processing behind telemetry holding the metrics DB. An expired budget
    // is recorded as "unavailable", never as "no candidates".
    let Some(db) = try_lock_with_budget(db, SESSION_EVENT_CANDIDATE_LOCK_BUDGET) else {
        *metrics_db_unavailable = true;
        return Ok(None);
    };

    for detection in detections {
        let candidate = match db
            .latest_session_event_candidate_for_repo(detection.kind.tools, target_repo_url)
        {
            Ok(candidate) => candidate,
            Err(error) => {
                tracing::debug!(%error, "latest session-event candidate query failed");
                *metrics_db_unavailable = true;
                return Ok(None);
            }
        };
        if let Some(candidate) = candidate {
            return Ok(Some(commit_metadata_selection_from_metric_candidate(
                &candidate,
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

fn agent_kind_matches_tool(kind: &CommitAgentKind, tool: &str) -> bool {
    kind.tools
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(tool))
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
            let (tool, model, external_session_id) = authorship_log
                .metadata
                .sessions
                .get(&source_session)
                .map(|session| {
                    (
                        session.agent_id.tool.clone(),
                        session.agent_id.model.clone(),
                        session.agent_id.id.clone(),
                    )
                })
                .unwrap_or_default();
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
                tool: &tool,
                model: &model,
                external_session_id: &external_session_id,
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

fn repo_worktree_key(repo: &Repository) -> Result<String, GitAiError> {
    let workdir = repo.workdir()?;
    let normalized = worktree_root_for_path(&workdir).unwrap_or(workdir);
    Ok(normalized
        .canonicalize()
        .unwrap_or(normalized)
        .to_string_lossy()
        .to_string())
}

fn file_timestamps_ns(workdir: &std::path::Path, file_path: &str) -> Vec<u128> {
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

fn select_best_bash_candidate<'a>(
    candidates: &'a [BashCheckpointCall],
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
                .then_with(|| right.candidate.id.cmp(&left.candidate.id))
        })
}

struct BashCandidateSelection<'a> {
    candidate: &'a BashCheckpointCall,
    distance_ns: u128,
    tier: BashCandidateTier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BashCandidateTier {
    ExistingCommitSession,
    WorkdirAncestor,
    CwdAncestor,
    TimeOnly,
}

impl BashCandidateTier {
    fn score(self) -> u8 {
        match self {
            Self::ExistingCommitSession => 0,
            Self::WorkdirAncestor => 1,
            Self::CwdAncestor => 2,
            Self::TimeOnly => 3,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ExistingCommitSession => "existing_commit_session",
            Self::WorkdirAncestor => "workdir_ancestor",
            Self::CwdAncestor => "cwd_ancestor",
            Self::TimeOnly => "time_only",
        }
    }
}

fn bash_candidate_tier(
    candidate: &BashCheckpointCall,
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

fn bash_candidate_session_id(candidate: &BashCheckpointCall) -> String {
    generate_session_id(&candidate.agent_id.id, &candidate.agent_id.tool)
}

fn select_best_session_event_candidate<'a>(
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

struct SessionEventCandidateSelection<'a> {
    candidate: &'a SessionEventRecoveryCandidate,
    distance_ns: u128,
    tier: SessionEventCandidateTier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionEventCandidateTier {
    SameRepoUrl,
}

impl SessionEventCandidateTier {
    fn score(self) -> u8 {
        match self {
            Self::SameRepoUrl => 0,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::SameRepoUrl => "same_repo_url",
        }
    }
}

fn session_event_candidate_tier(
    candidate: &SessionEventRecoveryCandidate,
    target_repo_url: &str,
) -> Option<SessionEventCandidateTier> {
    (candidate.repo_url.as_deref() == Some(target_repo_url))
        .then_some(SessionEventCandidateTier::SameRepoUrl)
}

fn session_event_distance(
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

fn recovery_distance_to_call_window(timestamp_ns: u128, call: &BashCheckpointCall) -> Option<u128> {
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

fn existing_commit_session_ids(authorship_log: &AuthorshipLog) -> HashSet<String> {
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

fn path_is_equal_or_child(child: &str, parent: &str) -> bool {
    if child.is_empty() || parent.is_empty() {
        return false;
    }

    let child = Path::new(child);
    let parent = Path::new(parent);
    child == parent || child.starts_with(parent)
}

fn insert_session_record(
    authorship_log: &mut AuthorshipLog,
    session_id: &str,
    agent_id: &AgentId,
    human_author: &str,
) {
    authorship_log
        .metadata
        .sessions
        .entry(session_id.to_string())
        .or_insert_with(|| SessionRecord {
            agent_id: agent_id.clone(),
            human_author: Some(human_author.to_string()),
            custom_attributes: None,
        });
}

fn insert_session_event_record(
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

fn session_event_model(candidate: &SessionEventRecoveryCandidate) -> String {
    candidate
        .model
        .clone()
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn recovery_model_for_session(authorship_log: &AuthorshipLog, session_id: &str) -> Option<String> {
    authorship_log
        .metadata
        .sessions
        .get(session_id)
        .map(|session| session.agent_id.model.clone())
        .filter(|model| !model.is_empty() && model != "unknown")
}

fn unknown_lines_by_file(
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

fn is_ai_attestation(author: &str) -> bool {
    author != CheckpointKind::Human.to_str() && !author.starts_with("h_")
}

fn ai_session_key(author: &str) -> &str {
    author.split("::").next().unwrap_or(author)
}

fn add_attestation(
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
    add_attestation_ranges(
        authorship_log,
        file_path,
        author_id,
        LineRange::compress_lines(&sorted),
    );
}

fn add_attestation_ranges(
    authorship_log: &mut AuthorshipLog,
    file_path: &str,
    author_id: &str,
    ranges: Vec<LineRange>,
) {
    if ranges.is_empty() {
        return;
    }
    let entry = crate::authorship::authorship_log_serialization::AttestationEntry::new(
        author_id.to_string(),
        ranges,
    );
    authorship_log
        .get_or_create_file(file_path)
        .add_entry(entry);
}

struct RecoveryMetricInput<'a> {
    repo: &'a Repository,
    parent_sha: &'a str,
    commit_sha: &'a str,
    file_path: &'a str,
    author_id: &'a str,
    session_id: &'a str,
    trace_id: &'a str,
    tool: &'a str,
    model: &'a str,
    external_session_id: &'a str,
    external_tool_use_id: Option<&'a str>,
    edit_kind: &'a str,
    checkpoint_type: &'a str,
    recovered_line_count: u32,
    metadata: serde_json::Value,
    event_ts: Option<u32>,
}

fn record_recovery_metric(input: RecoveryMetricInput<'_>) {
    if input.tool == "mock_ai" {
        return;
    }

    let checkpoint_ts = input.event_ts.map(u64::from).unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    });
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
    use crate::authorship::authorship_log_serialization::{
        AttestationEntry, AuthorshipLog, FileAttestation,
    };
    use crate::authorship::working_log::AgentId;

    #[test]
    #[serial_test::serial]
    fn preflight_reports_unknown_when_metrics_db_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: serialized via #[serial]; the metrics DB singleton is
        // pointed at a temp path before its first initialization.
        unsafe {
            std::env::set_var("GIT_AI_TEST_METRICS_DB_PATH", dir.path().join("metrics-db"));
        }
        let db = crate::metrics::db::MetricsDatabase::global().unwrap();
        // The singleton keeps this path for the process lifetime; leak the
        // temp dir so the open connection never points at a deleted file.
        std::mem::forget(dir);

        let _held = db.lock().unwrap();
        let checker = std::thread::spawn(|| {
            matching_session_event_candidate_exists(
                &[1_000_000_000u128],
                "https://example.com/repo.git",
            )
        });
        let result = checker.join().unwrap();
        // SAFETY: serialized via #[serial].
        unsafe {
            std::env::remove_var("GIT_AI_TEST_METRICS_DB_PATH");
        }
        assert_eq!(
            result, None,
            "a busy metrics DB must report unknown, not a definitive absence"
        );
    }

    fn claude_marker_detections() -> Vec<CommitAgentDetection> {
        let detections = detect_commit_metadata_agents(&CommitMetadata {
            message: "Add feature\n\nCo-Authored-By: Claude <noreply@anthropic.com>".to_string(),
            author_name: "Dev".to_string(),
            author_email: "dev@example.com".to_string(),
        });
        assert!(
            !detections.is_empty(),
            "the marker commit message should produce agent detections"
        );
        detections
    }

    fn point_metrics_db_at_temp_path()
    -> &'static std::sync::Mutex<crate::metrics::db::MetricsDatabase> {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: callers are serialized via #[serial]; the metrics DB
        // singleton is pointed at a temp path before its first initialization.
        unsafe {
            std::env::set_var("GIT_AI_TEST_METRICS_DB_PATH", dir.path().join("metrics-db"));
        }
        let db = crate::metrics::db::MetricsDatabase::global().unwrap();
        // The singleton keeps this path for the process lifetime; leak the
        // temp dir so the open connection never points at a deleted file.
        std::mem::forget(dir);
        db
    }

    fn clear_metrics_db_env() {
        // SAFETY: callers are serialized via #[serial].
        unsafe {
            std::env::remove_var("GIT_AI_TEST_METRICS_DB_PATH");
        }
    }

    /// A busy metrics DB means "no matching session" was never established:
    /// minting a synthesized session anyway would credit lines to a session
    /// that never existed — misattribution, strictly worse than leaving the
    /// lines unattributed.
    #[test]
    #[serial_test::serial]
    fn commit_metadata_session_is_not_synthesized_when_metrics_db_is_busy() {
        let db = point_metrics_db_at_temp_path();
        let detections = claude_marker_detections();

        let _held = db.lock().unwrap();
        let checker = std::thread::spawn(move || {
            let log = AuthorshipLog::new();
            let mut unavailable = false;
            let selection = select_commit_metadata_session(
                &log,
                &detections,
                &[1_000_000_000u128],
                None,
                &mut unavailable,
            );
            (selection, unavailable)
        });
        let (selection, unavailable) = checker.join().unwrap();
        clear_metrics_db_env();

        assert!(unavailable, "the busy DB must be reported as unavailable");
        assert!(
            selection.unwrap().is_none(),
            "a busy metrics DB must not mint a synthesized session"
        );
    }

    /// The synthesized fallback is legitimate when the DB was consulted and
    /// genuinely has no candidate rows.
    #[test]
    #[serial_test::serial]
    fn commit_metadata_session_synthesized_when_db_is_healthy_but_empty() {
        let _db = point_metrics_db_at_temp_path();
        let detections = claude_marker_detections();

        let log = AuthorshipLog::new();
        let mut unavailable = false;
        let selection = select_commit_metadata_session(
            &log,
            &detections,
            &[1_000_000_000u128],
            None,
            &mut unavailable,
        )
        .unwrap();
        clear_metrics_db_env();

        assert!(!unavailable, "an uncontended DB must count as consulted");
        let selection =
            selection.expect("a consulted-but-empty DB must still mint the synthesized fallback");
        assert_eq!(selection.tier, "synthesized_session");
    }

    #[test]
    #[serial_test::serial]
    fn commit_metadata_selector_reports_db_unavailable_when_busy() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: serialized via #[serial]; the metrics DB singleton is
        // pointed at a temp path before its first initialization.
        unsafe {
            std::env::set_var("GIT_AI_TEST_METRICS_DB_PATH", dir.path().join("metrics-db"));
        }
        let db = crate::metrics::db::MetricsDatabase::global().unwrap();
        // The singleton keeps this path for the process lifetime; leak the
        // temp dir so the open connection never points at a deleted file.
        std::mem::forget(dir);

        // A target repo is required for the DB to be consulted at all: with
        // no target repo an exact repo_url match can never succeed, so the
        // busy-DB path below would never be reached otherwise.
        let target_repo_url = Some("https://github.com/acme/repo");

        // Uncontended: the DB is consulted and the flag stays clear.
        let mut unavailable = false;
        let selection =
            select_latest_commit_metadata_metric_session(&[], target_repo_url, &mut unavailable)
                .unwrap();
        assert!(selection.is_none());
        assert!(!unavailable, "an uncontended DB must count as consulted");

        // Contended: the selector must report "unavailable", never let the
        // caller mistake a busy DB for "no matching AI session" (which would
        // end in AI lines being attributed to the human author).
        let _held = db.lock().unwrap();
        let checker = std::thread::spawn(move || {
            let mut unavailable = false;
            let selection = select_latest_commit_metadata_metric_session(
                &[],
                target_repo_url,
                &mut unavailable,
            );
            (selection, unavailable)
        });
        let (selection, unavailable) = checker.join().unwrap();
        // SAFETY: serialized via #[serial].
        unsafe {
            std::env::remove_var("GIT_AI_TEST_METRICS_DB_PATH");
        }
        assert!(selection.unwrap().is_none());
        assert!(
            unavailable,
            "a busy metrics DB must be reported as unavailable"
        );
    }

    #[test]
    fn try_lock_with_budget_returns_within_budget_when_contended() {
        let mutex = std::sync::Arc::new(std::sync::Mutex::new(()));
        let held = std::sync::Arc::clone(&mutex);
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            let _ = acquired_tx.send(());
            let _ = release_rx.recv();
        });
        acquired_rx
            .recv()
            .expect("holder thread should acquire the lock");

        let started = std::time::Instant::now();
        let budget = std::time::Duration::from_millis(100);
        assert!(
            try_lock_with_budget(&mutex, budget).is_none(),
            "a contended lock must not be acquired"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= budget && elapsed < std::time::Duration::from_millis(1000),
            "bounded acquire took {elapsed:?}, expected roughly the {budget:?} budget"
        );

        let _ = release_tx.send(());
        holder.join().unwrap();
        assert!(
            try_lock_with_budget(&mutex, budget).is_some(),
            "an uncontended lock must be acquired immediately"
        );
    }

    fn test_agent(external_session_id: &str) -> AgentId {
        AgentId {
            tool: "codex".to_string(),
            id: external_session_id.to_string(),
            model: "gpt-5".to_string(),
        }
    }

    fn bash_call(
        id: i64,
        external_session_id: &str,
        tool_use_id: &str,
        repo_work_dir: &str,
        start_time_ns: u128,
        end_time_ns: Option<u128>,
    ) -> BashCheckpointCall {
        BashCheckpointCall {
            id,
            invocation_key: format!("{}:{}", external_session_id, tool_use_id),
            original_cwd: repo_work_dir.to_string(),
            repo_work_dir: Some(repo_work_dir.to_string()),
            repo_discovery_error: None,
            session_id: external_session_id.to_string(),
            tool_use_id: tool_use_id.to_string(),
            agent_id: test_agent(external_session_id),
            start_trace_id: Some(format!("t_start_{}", id)),
            end_trace_id: end_time_ns.map(|_| format!("t_end_{}", id)),
            start_time_ns,
            end_time_ns,
            command: Some("true".to_string()),
            metadata: HashMap::new(),
        }
    }

    fn unresolved_bash_attempt(
        id: i64,
        external_session_id: &str,
        tool_use_id: &str,
        original_cwd: &str,
        start_time_ns: u128,
        end_time_ns: Option<u128>,
    ) -> BashCheckpointCall {
        BashCheckpointCall {
            id,
            invocation_key: format!("{}:{}", external_session_id, tool_use_id),
            original_cwd: original_cwd.to_string(),
            repo_work_dir: None,
            repo_discovery_error: Some("No git repository found".to_string()),
            session_id: external_session_id.to_string(),
            tool_use_id: tool_use_id.to_string(),
            agent_id: test_agent(external_session_id),
            start_trace_id: Some(format!("t_start_{}", id)),
            end_trace_id: end_time_ns.map(|_| format!("t_end_{}", id)),
            start_time_ns,
            end_time_ns,
            command: Some("cd repo && true".to_string()),
            metadata: HashMap::new(),
        }
    }

    fn session_event_candidate(
        row_id: i64,
        session_id: &str,
        external_session_id: &str,
        event_ts: u32,
        repo_url: Option<&str>,
    ) -> crate::metrics::db::SessionEventRecoveryCandidate {
        crate::metrics::db::SessionEventRecoveryCandidate {
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

    #[test]
    fn terminal_human_recovery_uses_attestations_instead_of_metadata() {
        let mut log = AuthorshipLog::new();
        log.metadata.sessions.insert(
            "s_metadata_only".to_string(),
            SessionRecord {
                agent_id: test_agent("metadata-only"),
                human_author: None,
                custom_attributes: None,
            },
        );
        assert!(
            should_recover_remaining_as_known_human(&log),
            "AI metadata without landed attribution must not block recovery"
        );

        log.attestations.push(FileAttestation {
            file_path: "file.txt".to_string(),
            entries: vec![AttestationEntry::new(
                "s_ai::t_1".to_string(),
                vec![LineRange::Single(1)],
            )],
        });
        log.metadata.humans.insert(
            "h_metadata_only".to_string(),
            HumanRecord {
                author: "Human <human@example.com>".to_string(),
            },
        );
        assert!(
            !should_recover_remaining_as_known_human(&log),
            "unattested human metadata must not override landed AI attribution"
        );

        log.attestations[0].entries.push(AttestationEntry::new(
            "h_landed".to_string(),
            vec![LineRange::Single(2)],
        ));
        assert!(
            should_recover_remaining_as_known_human(&log),
            "a landed known-human attestation must enable recovery"
        );
    }

    /// Session-event candidates for Cursor rarely carry a model, but the
    /// session they belong to is often already known from other lines in
    /// the same commit (recorded via a normal checkpoint). Recovery must
    /// reuse that already-known model instead of writing "unknown" into the
    /// recovery metric when a real model is on record for the session.
    #[test]
    fn recovery_model_for_session_reuses_known_model() {
        let mut log = AuthorshipLog::new();
        log.metadata.sessions.insert(
            "s_known".to_string(),
            SessionRecord {
                agent_id: AgentId {
                    tool: "cursor".to_string(),
                    id: "cursor-session".to_string(),
                    model: "grok-4.5".to_string(),
                },
                human_author: None,
                custom_attributes: None,
            },
        );

        assert_eq!(
            recovery_model_for_session(&log, "s_known"),
            Some("grok-4.5".to_string()),
            "a session with a known model must be reused instead of falling back to unknown"
        );
    }

    #[test]
    fn recovery_model_for_session_falls_back_when_model_is_unknown_or_missing() {
        let mut log = AuthorshipLog::new();
        log.metadata.sessions.insert(
            "s_unknown".to_string(),
            SessionRecord {
                agent_id: AgentId {
                    tool: "cursor".to_string(),
                    id: "cursor-session".to_string(),
                    model: "unknown".to_string(),
                },
                human_author: None,
                custom_attributes: None,
            },
        );
        log.metadata.sessions.insert(
            "s_empty".to_string(),
            SessionRecord {
                agent_id: AgentId {
                    tool: "cursor".to_string(),
                    id: "cursor-session".to_string(),
                    model: String::new(),
                },
                human_author: None,
                custom_attributes: None,
            },
        );

        assert_eq!(
            recovery_model_for_session(&log, "s_unknown"),
            None,
            "a session recorded as unknown must not shadow the candidate's own model lookup"
        );
        assert_eq!(
            recovery_model_for_session(&log, "s_empty"),
            None,
            "a session with an empty model must not shadow the candidate's own model lookup"
        );
        assert_eq!(
            recovery_model_for_session(&log, "s_missing"),
            None,
            "a session that has not been recorded yet must fall back to the candidate's model"
        );
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
