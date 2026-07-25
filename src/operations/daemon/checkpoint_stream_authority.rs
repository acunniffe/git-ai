use crate::config::Config;
use crate::error::GitAiError;
use crate::model::checkpoint_request::CheckpointRequest;
use crate::operations::git::repository::{
    discover_repository_policy_location_no_git_exec, load_repository_policy_context_no_git_exec,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub(crate) const STREAM_AUTHORITY_DENIAL: &str =
    "checkpoint stream source authority could not be verified";

pub(crate) fn authorize_checkpoint_stream_source(
    request: &mut CheckpointRequest,
) -> Result<(), GitAiError> {
    if request.stream_source.is_none() {
        return Ok(());
    }

    authorize_request_repository(request, &Config::fresh())?;
    match validate_trusted_stream_source(request) {
        Ok(session) => {
            let source = request
                .stream_source
                .as_mut()
                .expect("stream source was checked above");
            source.path = session.stream_path;
            source.session_id = session.session_id;
            source.external_session_id = session.external_session_id;
            source.external_parent_session_id = session.external_parent_session_id;
        }
        Err(_) => {
            tracing::warn!(
                "{}; continuing without optional transcript enrichment",
                STREAM_AUTHORITY_DENIAL
            );
            request.stream_source = None;
        }
    }
    Ok(())
}

fn authorize_request_repository(
    request: &CheckpointRequest,
    config: &Config,
) -> Result<(), GitAiError> {
    if request.files.is_empty() || !config.has_allowed_repositories() {
        return Err(denial());
    }

    let mut request_root = None;
    let mut checked_roots = HashSet::new();
    for file in &request.files {
        let location = discover_repository_policy_location_no_git_exec(&file.repo_work_dir)
            .map_err(|_| denial())?;
        let root = location.repository_root().to_path_buf();

        if request_root
            .as_ref()
            .is_some_and(|expected| expected != &root)
        {
            return Err(denial());
        }
        request_root.get_or_insert_with(|| root.clone());

        if checked_roots.insert(root.clone()) {
            let policy =
                load_repository_policy_context_no_git_exec(&location).map_err(|_| denial())?;
            if !policy.is_collection_allowed(config) {
                return Err(denial());
            }
        }

        let file_path = resolve_file_path(&file.path, &root);
        let file_location =
            discover_repository_policy_location_no_git_exec(&file_path).map_err(|_| denial())?;
        if file_location.repository_root() != root {
            return Err(denial());
        }
    }
    Ok(())
}

fn validate_trusted_stream_source(
    request: &CheckpointRequest,
) -> Result<crate::operations::streams::sweep::DiscoveredSession, GitAiError> {
    let source = request.stream_source.as_ref().ok_or_else(denial)?;
    let agent_id = request.agent_id.as_ref().ok_or_else(denial)?;
    if agent_id.id != source.external_session_id {
        return Err(denial());
    }
    let agent = crate::operations::streams::agent::get_agent(&agent_id.tool).ok_or_else(denial)?;
    agent
        .validate_checkpoint_stream(source)
        .map_err(|_| denial())
}

fn resolve_file_path(path: &Path, repository_root: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repository_root.join(path)
    }
}

fn denial() -> GitAiError {
    GitAiError::PresetError(STREAM_AUTHORITY_DENIAL.to_string())
}
