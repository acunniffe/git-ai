use super::types::VirtualAttributions;
use crate::error::GitAiError;
use crate::model::attribution_tracker::{
    Attribution, LineAttribution, attributions_to_line_attributions,
    line_attributions_to_attributions,
};
use crate::model::authorship_log::{HumanRecord, PromptRecord, SessionRecord};
use crate::model::authorship_log_serialization::{
    generate_human_short_hash, generate_session_id, generate_short_hash,
};
use crate::model::working_log::{Checkpoint, CheckpointKind, InitialAttributions, WorkingLogEntry};
use crate::operations::git::repo_storage::PersistedWorkingLog;
use crate::operations::git::repository::Repository;
use std::collections::{BTreeMap, HashMap, HashSet};

type AttributionMap = HashMap<String, (Vec<Attribution>, Vec<LineAttribution>)>;

pub(super) enum WorkingLogContentSource<'a> {
    /// Resolve each attributed file from the repository's current worktree.
    LiveWorktree,
    /// Keep INITIAL's stored version, but let the captured final state win for checkpoints.
    CapturedSnapshot(&'a HashMap<String, String>),
    /// Require every attributed version to exist in working-log blob storage.
    Persisted,
}

#[derive(Default)]
struct WorkingLogState {
    attributions: AttributionMap,
    file_contents: HashMap<String, String>,
    prompts: BTreeMap<String, BTreeMap<String, PromptRecord>>,
    humans: BTreeMap<String, HumanRecord>,
    initial_only_prompt_ids: HashSet<String>,
    sessions: BTreeMap<String, SessionRecord>,
    session_additions: HashMap<String, u32>,
    session_deletions: HashMap<String, u32>,
}

pub(super) fn load_working_log_state(
    repo: Repository,
    base_commit: String,
    human_author: Option<String>,
    source: WorkingLogContentSource<'_>,
) -> Result<VirtualAttributions, GitAiError> {
    let working_log = repo.storage.working_log_for_base_commit(&base_commit)?;
    let initial_attributions = working_log.read_initial_attributions();
    let checkpoints = working_log.read_all_checkpoints().unwrap_or_default();
    let mut state = WorkingLogState::default();

    seed_initial_metadata(&mut state, &initial_attributions);
    apply_initial_attributions(
        &mut state,
        &repo,
        &working_log,
        &initial_attributions,
        &source,
    )?;

    for checkpoint in &checkpoints {
        apply_checkpoint_metadata(&mut state, checkpoint, &human_author);

        for entry in &checkpoint.entries {
            if entry.line_attributions.is_empty() && entry.attributions.is_empty() {
                continue;
            }
            apply_checkpoint_entry(&mut state, &repo, &working_log, entry, &source)?;
        }
    }

    VirtualAttributions::calculate_and_update_prompt_metrics(
        &mut state.prompts,
        &state.attributions,
        &state.session_additions,
        &state.session_deletions,
    );

    Ok(VirtualAttributions {
        repo,
        base_commit,
        attributions: state.attributions,
        file_contents: state.file_contents,
        prompts: state.prompts,
        ts: 0,
        blame_start_commit: None,
        humans: state.humans,
        initial_only_prompt_ids: state.initial_only_prompt_ids,
        sessions: state.sessions,
    })
}

fn seed_initial_metadata(state: &mut WorkingLogState, initial: &InitialAttributions) {
    for (prompt_id, prompt_record) in &initial.prompts {
        state
            .prompts
            .entry(prompt_id.clone())
            .or_default()
            .insert(String::new(), prompt_record.clone());
        state.initial_only_prompt_ids.insert(prompt_id.clone());
    }

    for (hash, human_record) in &initial.humans {
        state
            .humans
            .entry(hash.clone())
            .or_insert_with(|| human_record.clone());
    }

    for (session_id, session_record) in &initial.sessions {
        state
            .sessions
            .entry(session_id.clone())
            .or_insert_with(|| session_record.clone());
    }
}

fn apply_initial_attributions(
    state: &mut WorkingLogState,
    repo: &Repository,
    working_log: &PersistedWorkingLog,
    initial: &InitialAttributions,
    source: &WorkingLogContentSource<'_>,
) -> Result<(), GitAiError> {
    for (file_path, line_attributions) in &initial.files {
        let Some(file_content) =
            initial_file_content(repo, working_log, initial, file_path, source)?
        else {
            continue;
        };

        let char_attributions =
            line_attributions_to_attributions(line_attributions, &file_content, 0);
        state.file_contents.insert(file_path.clone(), file_content);
        state.attributions.insert(
            file_path.clone(),
            (char_attributions, line_attributions.clone()),
        );
    }

    Ok(())
}

fn apply_checkpoint_metadata(
    state: &mut WorkingLogState,
    checkpoint: &Checkpoint,
    human_author: &Option<String>,
) {
    if let Some(agent_id) = &checkpoint.agent_id {
        if checkpoint.trace_id.is_some() {
            let session_id = generate_session_id(&agent_id.id, &agent_id.tool);
            state.sessions.insert(
                session_id.clone(),
                SessionRecord {
                    agent_id: agent_id.clone(),
                    human_author: human_author.clone(),
                    custom_attributes: None,
                },
            );
            *state
                .session_additions
                .entry(session_id.clone())
                .or_insert(0) += checkpoint.line_stats.additions;
            *state.session_deletions.entry(session_id).or_insert(0) +=
                checkpoint.line_stats.deletions;
        } else {
            let author_id = generate_short_hash(&agent_id.id, &agent_id.tool);
            state.prompts.entry(author_id.clone()).or_default().insert(
                String::new(),
                PromptRecord {
                    agent_id: agent_id.clone(),
                    human_author: human_author.clone(),
                    total_additions: 0,
                    total_deletions: 0,
                    accepted_lines: 0,
                    overriden_lines: 0,
                    custom_attributes: None,
                    messages_url: None,
                },
            );
            state.initial_only_prompt_ids.remove(&author_id);
            *state
                .session_additions
                .entry(author_id.clone())
                .or_insert(0) += checkpoint.line_stats.additions;
            *state.session_deletions.entry(author_id).or_insert(0) +=
                checkpoint.line_stats.deletions;
        }
    }

    if checkpoint.kind == CheckpointKind::KnownHuman {
        let hash = generate_human_short_hash(&checkpoint.author);
        state.humans.entry(hash).or_insert_with(|| HumanRecord {
            author: checkpoint.author.clone(),
        });
    }
}

fn apply_checkpoint_entry(
    state: &mut WorkingLogState,
    repo: &Repository,
    working_log: &PersistedWorkingLog,
    entry: &WorkingLogEntry,
    source: &WorkingLogContentSource<'_>,
) -> Result<(), GitAiError> {
    if let Some(file_content) = checkpoint_file_content(repo, working_log, entry, source)? {
        state.file_contents.insert(entry.file.clone(), file_content);
    }

    let file_content = state
        .file_contents
        .get(&entry.file)
        .cloned()
        .unwrap_or_default();
    let line_attributions = if entry.line_attributions.is_empty() {
        attributions_to_line_attributions(&entry.attributions, &file_content)
    } else {
        entry.line_attributions.clone()
    };

    if line_attributions.is_empty() {
        state.attributions.remove(&entry.file);
        return Ok(());
    }

    let char_attributions = line_attributions_to_attributions(&line_attributions, &file_content, 0);
    state
        .attributions
        .insert(entry.file.clone(), (char_attributions, line_attributions));
    Ok(())
}

fn initial_file_content(
    repo: &Repository,
    working_log: &PersistedWorkingLog,
    initial: &InitialAttributions,
    file_path: &str,
    source: &WorkingLogContentSource<'_>,
) -> Result<Option<String>, GitAiError> {
    match source {
        WorkingLogContentSource::LiveWorktree => {
            let Ok(workdir) = repo.workdir() else {
                return Ok(None);
            };
            let absolute_path = workdir.join(file_path);
            let content = if absolute_path.exists() {
                std::fs::read_to_string(absolute_path).unwrap_or_default()
            } else {
                String::new()
            };
            Ok(Some(content))
        }
        WorkingLogContentSource::CapturedSnapshot(snapshot) => Ok(Some(
            working_log
                .stored_initial_file_content_from(initial, file_path)
                .or_else(|| snapshot.get(file_path).cloned())
                .unwrap_or_default(),
        )),
        WorkingLogContentSource::Persisted => working_log
            .stored_initial_file_content_from(initial, file_path)
            .map(Some)
            .ok_or_else(|| {
                // Load-bearing persisted-state contract; callers and tests match this exact text.
                GitAiError::Generic(format!(
                    "INITIAL missing persisted file snapshot for {}",
                    file_path
                ))
            }),
    }
}

fn checkpoint_file_content(
    repo: &Repository,
    working_log: &PersistedWorkingLog,
    entry: &WorkingLogEntry,
    source: &WorkingLogContentSource<'_>,
) -> Result<Option<String>, GitAiError> {
    match source {
        WorkingLogContentSource::LiveWorktree => {
            let Ok(workdir) = repo.workdir() else {
                return Ok(None);
            };
            let absolute_path = workdir.join(&entry.file);
            let content = if absolute_path.exists() {
                std::fs::read_to_string(absolute_path).unwrap_or_default()
            } else {
                String::new()
            };
            Ok(Some(content))
        }
        WorkingLogContentSource::CapturedSnapshot(snapshot) => Ok(Some(
            snapshot.get(&entry.file).cloned().unwrap_or_else(|| {
                working_log
                    .get_file_version(&entry.blob_sha)
                    .unwrap_or_default()
            }),
        )),
        WorkingLogContentSource::Persisted => {
            working_log.get_file_version(&entry.blob_sha).map(Some)
        }
    }
}
