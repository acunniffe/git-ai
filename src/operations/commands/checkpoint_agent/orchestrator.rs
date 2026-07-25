use crate::checkpoint_content_budget::CheckpointContentBudget;
use crate::config;
use crate::error::GitAiError;
use crate::model::authorship_log_serialization::generate_trace_id;
#[cfg(test)]
use crate::model::checkpoint_delivery::CHECKPOINT_DELIVERY_MAX_FILES;
use crate::model::working_log::{AgentId, CheckpointKind};
use crate::operations::commands::checkpoint_agent::authorization::{
    CheckpointFileSnapshot, authorize_events, authorize_repository_path,
    authorize_repository_workdir, read_checkpoint_file_snapshot, validate_checkpoint_file_count,
};
use crate::operations::commands::checkpoint_agent::presets::{
    KnownHumanEdit, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    StreamSource, UntrackedEdit,
};
use crate::operations::git::repo_state::{read_head_state_for_worktree, worktree_root_for_path};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// Re-export the types that moved to model so the 11 existing consumers that
// import via `orchestrator::` keep working without import-path changes.
pub use crate::model::checkpoint_request::BaseCommit;
pub use crate::model::checkpoint_request::CheckpointFile;
pub use crate::model::checkpoint_request::CheckpointRequest;
pub use crate::model::checkpoint_request::PreparedPathRole;
pub use crate::operations::commands::checkpoint_agent::authorization::{
    CheckpointAuthorizationDenial, CheckpointPresetOutcome,
};

struct RepoContext {
    repo_work_dir: PathBuf,
    base_commit: BaseCommit,
}

pub fn authorize_checkpoint_status_discovery(
    cwd: &Path,
) -> Result<(), CheckpointAuthorizationDenial> {
    authorize_repository_path(cwd, config::Config::get())
}

fn apply_checkpoint_content_budget(files: &mut [CheckpointFile]) {
    let mut budget = CheckpointContentBudget::from_config(config::Config::get());
    for file in files {
        let Some(content) = file.content.as_ref() else {
            continue;
        };
        if !budget.reserve(file.path.display(), content) {
            file.content = None;
        }
    }
}

fn apply_dirty_file_overrides(
    files: &mut [CheckpointFile],
    dirty_files: &HashMap<PathBuf, String>,
) {
    for file in &mut *files {
        if let Some(override_content) = dirty_files.get(&file.path) {
            file.content = Some(override_content.clone());
        }
    }
    apply_checkpoint_content_budget(files);
}

fn build_checkpoint_files(file_paths: &[PathBuf]) -> Result<Vec<CheckpointFile>, GitAiError> {
    let perf = std::env::var("GIT_AI_DEBUG_PERFORMANCE").is_ok_and(|v| !v.is_empty() && v != "0");

    validate_checkpoint_file_count(file_paths.len())
        .map_err(|denial| GitAiError::PresetError(denial.user_message().to_string()))?;

    let mut repo_cache: HashMap<PathBuf, RepoContext> = HashMap::new();
    let mut files = Vec::new();
    let mut content_budget = CheckpointContentBudget::from_config(config::Config::get());
    let max_size = content_budget.max_file_size_bytes();

    for path in file_paths {
        if !path.is_absolute() {
            return Err(GitAiError::PresetError(format!(
                "file path must be absolute: {}",
                path.display()
            )));
        }

        let ctx = {
            let t_discover = std::time::Instant::now();
            let repo_work_dir = worktree_root_for_path(path).ok_or_else(|| {
                GitAiError::Generic(format!(
                    "No git repository found for path: {}",
                    path.display()
                ))
            })?;
            if !repo_cache.contains_key(&repo_work_dir) {
                let t_head = std::time::Instant::now();
                let base_commit = match read_head_state_for_worktree(&repo_work_dir) {
                    Some(state) => match state.head {
                        Some(sha) => BaseCommit::Sha(sha),
                        None => BaseCommit::Initial,
                    },
                    None => BaseCommit::Initial,
                };
                let head_ms = t_head.elapsed().as_secs_f64() * 1000.0;

                if perf {
                    eprintln!(
                        "[perf] build_checkpoint_files: discover={:.1}ms head={:.1}ms (repo={})",
                        t_discover.elapsed().as_secs_f64() * 1000.0,
                        head_ms,
                        repo_work_dir.display(),
                    );
                }

                let key = repo_work_dir.clone();
                repo_cache.insert(
                    key,
                    RepoContext {
                        repo_work_dir: repo_work_dir.clone(),
                        base_commit,
                    },
                );
            }
            repo_cache.get(&repo_work_dir).unwrap()
        };

        let t_read = std::time::Instant::now();
        let content = match read_checkpoint_file_snapshot(path, max_size)? {
            CheckpointFileSnapshot::Oversized(size) => {
                tracing::warn!(
                    "skipping file larger than max_checkpoint_file_size_bytes: {} ({} bytes)",
                    path.display(),
                    size,
                );
                continue;
            }
            CheckpointFileSnapshot::Missing => Some(String::new()),
            CheckpointFileSnapshot::Read(content) => content,
        };
        if perf {
            eprintln!(
                "[perf] build_checkpoint_files: read_file={:.1}ms (path={}, size={})",
                t_read.elapsed().as_secs_f64() * 1000.0,
                path.display(),
                content.as_ref().map(|c| c.len()).unwrap_or(0),
            );
        }

        let content = content.filter(|content| content_budget.reserve(path.display(), content));

        files.push(CheckpointFile {
            path: path.clone(),
            content,
            repo_work_dir: ctx.repo_work_dir.clone(),
            base_commit: ctx.base_commit.clone(),
        });
    }

    Ok(files)
}

pub fn execute_preset_checkpoint(
    preset_name: &str,
    hook_input: &str,
) -> Result<CheckpointPresetOutcome, GitAiError> {
    let perf = std::env::var("GIT_AI_DEBUG_PERFORMANCE").is_ok_and(|v| !v.is_empty() && v != "0");
    let t0 = std::time::Instant::now();

    let trace_id = generate_trace_id();
    let preset = super::presets::resolve_preset(preset_name)?;
    let mut events = preset.parse(hook_input, &trace_id)?;
    events.retain(event_has_checkpoint_target);
    if events.is_empty() {
        return Ok(CheckpointPresetOutcome::Authorized(Vec::new()));
    }
    let events_len = events.len();
    if let Err(denial) = authorize_events(&mut events, config::Config::get()) {
        return Ok(CheckpointPresetOutcome::Denied(denial));
    }
    preset.enrich_authorized_events(hook_input, &mut events)?;
    ensure_bash_events_daemon_ready(&events)?;

    if perf {
        eprintln!(
            "[perf] orchestrator: parse={:.1}ms (events={})",
            t0.elapsed().as_secs_f64() * 1000.0,
            events_len,
        );
    }

    let mut requests = Vec::new();
    for event in events {
        let t_event = std::time::Instant::now();
        let event_name = format!("{:?}", std::mem::discriminant(&event));
        let new_requests = execute_event(event, preset_name)?;
        if perf {
            eprintln!(
                "[perf] orchestrator: execute_event({})={:.1}ms (requests={})",
                event_name,
                t_event.elapsed().as_secs_f64() * 1000.0,
                new_requests.len(),
            );
        }
        requests.extend(new_requests);
    }

    if config::Config::get()
        .get_feature_flags()
        .checkpoint_debug_log
    {
        super::debug_log::write_checkpoint_debug_log(
            preset_name,
            hook_input,
            &trace_id,
            events_len,
            &requests,
        );
    }

    Ok(CheckpointPresetOutcome::Authorized(requests))
}

fn ensure_bash_events_daemon_ready(events: &[ParsedHookEvent]) -> Result<(), GitAiError> {
    if !events.iter().any(|event| {
        matches!(
            event,
            ParsedHookEvent::PreBashCall(_) | ParsedHookEvent::PostBashCall(_)
        )
    }) {
        return Ok(());
    }

    super::delivery_runtime::ensure_checkpoint_daemon_running()
        .map(|_| ())
        .map_err(|_| GitAiError::PresetError("background worker unavailable".to_string()))
}

fn event_has_checkpoint_target(event: &ParsedHookEvent) -> bool {
    match event {
        ParsedHookEvent::PreFileEdit(event) => !event.file_paths.is_empty(),
        ParsedHookEvent::PostFileEdit(event) => !event.file_paths.is_empty(),
        ParsedHookEvent::PreBashCall(_) | ParsedHookEvent::PostBashCall(_) => true,
        ParsedHookEvent::KnownHumanEdit(event) => !event.file_paths.is_empty(),
        ParsedHookEvent::UntrackedEdit(event) => !event.file_paths.is_empty(),
    }
}

fn execute_event(
    event: ParsedHookEvent,
    preset_name: &str,
) -> Result<Vec<CheckpointRequest>, GitAiError> {
    match event {
        ParsedHookEvent::PreFileEdit(e) => execute_pre_file_edit(e),
        ParsedHookEvent::PostFileEdit(e) => execute_post_file_edit(e, preset_name),
        ParsedHookEvent::PreBashCall(e) => execute_pre_bash_call(e),
        ParsedHookEvent::PostBashCall(e) => execute_post_bash_call(e),
        ParsedHookEvent::KnownHumanEdit(e) => execute_known_human_edit(e),
        ParsedHookEvent::UntrackedEdit(e) => execute_untracked_edit(e),
    }
}

fn split_files_into_requests(
    all_files: Vec<CheckpointFile>,
    trace_id: String,
    checkpoint_kind: CheckpointKind,
    agent_id: Option<AgentId>,
    path_role: PreparedPathRole,
    stream_source: Option<StreamSource>,
    metadata: HashMap<String, String>,
) -> Vec<CheckpointRequest> {
    let all_files: Vec<CheckpointFile> = all_files
        .into_iter()
        .filter(|f| f.content.is_some())
        .collect();
    let mut by_repo: HashMap<PathBuf, Vec<CheckpointFile>> = HashMap::new();
    for f in all_files {
        by_repo.entry(f.repo_work_dir.clone()).or_default().push(f);
    }

    by_repo
        .into_values()
        .map(|files| CheckpointRequest {
            trace_id: trace_id.clone(),
            checkpoint_kind,
            agent_id: agent_id.clone(),
            files,
            path_role,
            stream_source: stream_source.clone(),
            metadata: metadata.clone(),
        })
        .collect()
}

fn execute_pre_file_edit(e: PreFileEdit) -> Result<Vec<CheckpointRequest>, GitAiError> {
    let mut files = build_checkpoint_files(&e.file_paths)?;
    if let Some(ref dirty) = e.dirty_files {
        apply_dirty_file_overrides(&mut files, dirty);
    }
    let mut metadata = e.context.metadata;
    if let Some(tuid) = e.tool_use_id {
        metadata.entry("tool_use_id".to_string()).or_insert(tuid);
    }
    Ok(split_files_into_requests(
        files,
        e.context.trace_id,
        CheckpointKind::Human,
        Some(e.context.agent_id),
        PreparedPathRole::WillEdit,
        None,
        metadata,
    ))
}

fn execute_post_file_edit(
    e: PostFileEdit,
    preset_name: &str,
) -> Result<Vec<CheckpointRequest>, GitAiError> {
    let mut files = build_checkpoint_files(&e.file_paths)?;
    if let Some(ref dirty) = e.dirty_files {
        apply_dirty_file_overrides(&mut files, dirty);
    }
    let checkpoint_kind = match preset_name {
        "ai_tab" => CheckpointKind::AiTab,
        _ => CheckpointKind::AiAgent,
    };
    let mut metadata = e.context.metadata;
    if let Some(tuid) = e.tool_use_id {
        metadata.entry("tool_use_id".to_string()).or_insert(tuid);
    }
    metadata
        .entry("edit_kind".to_string())
        .or_insert_with(|| "file_edit".to_string());
    Ok(split_files_into_requests(
        files,
        e.context.trace_id,
        checkpoint_kind,
        Some(e.context.agent_id),
        PreparedPathRole::Edited,
        e.stream_source,
        metadata,
    ))
}

fn execute_known_human_edit(e: KnownHumanEdit) -> Result<Vec<CheckpointRequest>, GitAiError> {
    let mut files = build_checkpoint_files(&e.file_paths)?;
    if let Some(ref dirty) = e.dirty_files {
        apply_dirty_file_overrides(&mut files, dirty);
    }
    Ok(split_files_into_requests(
        files,
        e.trace_id,
        CheckpointKind::KnownHuman,
        None,
        PreparedPathRole::Edited,
        None,
        e.editor_metadata,
    ))
}

fn execute_untracked_edit(e: UntrackedEdit) -> Result<Vec<CheckpointRequest>, GitAiError> {
    let files = build_checkpoint_files(&e.file_paths)?;
    Ok(split_files_into_requests(
        files,
        e.trace_id,
        CheckpointKind::Human,
        None,
        PreparedPathRole::WillEdit,
        None,
        HashMap::new(),
    ))
}

fn execute_pre_bash_call(e: PreBashCall) -> Result<Vec<CheckpointRequest>, GitAiError> {
    use crate::operations::commands::checkpoint_agent::bash_tool::{
        self, BashHookAttemptPhase, BashHookAttemptSignal,
    };

    let started_at_ns = crate::model::repository::bash_history_db::unix_time_ns();
    let repo_work_dir =
        authorize_repository_workdir(e.context.cwd.as_path(), config::Config::get())
            .map_err(|denial| GitAiError::PresetError(denial.user_message().to_string()))?;

    if config::Config::get()
        .get_feature_flags()
        .bash_checkpoints_v2
    {
        bash_tool::signal_daemon_bash_hook_attempt(
            BashHookAttemptPhase::Start,
            BashHookAttemptSignal {
                original_cwd: e.context.cwd.as_path(),
                discovered_repo_work_dir: Some(&repo_work_dir),
                repo_discovery_error: None,
                session_id: &e.context.external_session_id,
                tool_use_id: &e.tool_use_id,
                agent_id: &e.context.agent_id,
                metadata: &e.context.metadata,
                trace_id: &e.context.trace_id,
                timestamp_ns: started_at_ns,
                command: e.command.as_deref(),
            },
        );
        return Ok(vec![]);
    }

    let dirty_paths = match bash_tool::handle_bash_pre_tool_use_with_context_and_cwd(
        &repo_work_dir,
        e.context.cwd.as_path(),
        bash_tool::BashToolHookContext {
            session_id: &e.context.external_session_id,
            tool_use_id: &e.tool_use_id,
            agent_id: &e.context.agent_id,
            agent_metadata: Some(&e.context.metadata),
            trace_id: &e.context.trace_id,
            command: e.command.as_deref(),
        },
    ) {
        Ok(result) => result.dirty_paths,
        Err(error) => {
            tracing::debug!(
                "Bash pre-hook snapshot failed for {} session {}: {}",
                e.context.agent_id.tool,
                e.context.external_session_id,
                error
            );
            return Ok(vec![]);
        }
    };

    if dirty_paths.is_empty() {
        return Ok(vec![]);
    }

    let files = build_checkpoint_files(&dirty_paths)?;
    let mut metadata = e.context.metadata;
    metadata
        .entry("tool_use_id".to_string())
        .or_insert(e.tool_use_id);
    Ok(split_files_into_requests(
        files,
        e.context.trace_id,
        CheckpointKind::Human,
        None,
        PreparedPathRole::WillEdit,
        None,
        metadata,
    ))
}

fn execute_post_bash_call(e: PostBashCall) -> Result<Vec<CheckpointRequest>, GitAiError> {
    use crate::operations::commands::checkpoint_agent::bash_tool::{
        self, BashHookAttemptPhase, BashHookAttemptSignal,
    };

    let ended_at_ns = crate::model::repository::bash_history_db::unix_time_ns();
    let repo_work_dir =
        authorize_repository_workdir(e.context.cwd.as_path(), config::Config::get())
            .map_err(|denial| GitAiError::PresetError(denial.user_message().to_string()))?;

    if config::Config::get()
        .get_feature_flags()
        .bash_checkpoints_v2
    {
        bash_tool::signal_daemon_bash_hook_attempt(
            BashHookAttemptPhase::End,
            BashHookAttemptSignal {
                original_cwd: e.context.cwd.as_path(),
                discovered_repo_work_dir: Some(&repo_work_dir),
                repo_discovery_error: None,
                session_id: &e.context.external_session_id,
                tool_use_id: &e.tool_use_id,
                agent_id: &e.context.agent_id,
                metadata: &e.context.metadata,
                trace_id: &e.context.trace_id,
                timestamp_ns: ended_at_ns,
                command: e.command.as_deref(),
            },
        );
        return Ok(vec![]);
    }

    let bash_result = bash_tool::handle_bash_post_tool_use_with_cwd(
        &repo_work_dir,
        e.context.cwd.as_path(),
        bash_tool::BashToolHookContext {
            session_id: &e.context.external_session_id,
            tool_use_id: &e.tool_use_id,
            agent_id: &e.context.agent_id,
            agent_metadata: Some(&e.context.metadata),
            trace_id: &e.context.trace_id,
            command: e.command.as_deref(),
        },
    );

    let file_paths: Vec<PathBuf> = match &bash_result {
        Ok(result) => match &result.action {
            bash_tool::BashCheckpointAction::Checkpoint(paths) => paths
                .iter()
                .map(|p| {
                    let joined = repo_work_dir.join(p);
                    crate::operations::git::canonicalize::canonicalize_or_self(&joined)
                })
                .collect(),
            _ => vec![],
        },
        Err(err) => {
            tracing::debug!("Bash tool post-hook error: {}", err);
            vec![]
        }
    };

    let files = build_checkpoint_files(&file_paths)?;
    let mut metadata = e.context.metadata;
    metadata
        .entry("tool_use_id".to_string())
        .or_insert(e.tool_use_id);
    metadata
        .entry("edit_kind".to_string())
        .or_insert_with(|| "bash".to_string());
    Ok(split_files_into_requests(
        files,
        e.context.trace_id,
        CheckpointKind::AiAgent,
        Some(e.context.agent_id),
        PreparedPathRole::Edited,
        e.stream_source,
        metadata,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_builder_rejects_oversized_batch_before_path_processing() {
        let paths = vec![
            PathBuf::from("relative-path-must-not-be-processed");
            CHECKPOINT_DELIVERY_MAX_FILES + 1
        ];

        let Err(error) = build_checkpoint_files(&paths) else {
            panic!("an oversized batch must fail closed");
        };
        assert!(
            error
                .to_string()
                .contains(CheckpointAuthorizationDenial::FileLimitExceeded.user_message())
        );
    }
}
