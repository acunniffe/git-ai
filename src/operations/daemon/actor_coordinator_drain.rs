#[allow(unused_imports)]
use super::*;
use crate::error::GitAiError;
use crate::model::checkpoint_request::{CheckpointRequest, PreparedPathRole};
use crate::model::repository::error::PersistenceError;
use crate::model::working_log::CheckpointKind;
use std::collections::BTreeMap;
use std::path::Path;
use tokio::sync::oneshot;

impl ActorDaemonCoordinator {
    pub(crate) async fn append_checkpoint_to_family_sequencer(
        &self,
        family: &str,
        request: CheckpointRequest,
        respond_to: Option<oneshot::Sender<Result<u64, GitAiError>>>,
    ) -> Result<(), GitAiError> {
        // Causal drain fence: ensure already-visible trace2 work has reached
        // the family sequencer before inserting this checkpoint.
        self.wait_for_trace_ingest_processed_through().await;

        let exec_lock = self.side_effect_exec_lock(family)?;
        let _guard = exec_lock.lock().await;

        {
            let mut sequencers = self.family_sequencers_by_family.lock().map_err(|_| {
                PersistenceError::LockPoisoned {
                    what: "family sequencer map",
                }
            })?;
            let state =
                sequencers
                    .entry(family.to_string())
                    .or_insert_with(|| FamilySequencerState {
                        next_ordinal: 1,
                        entries: BTreeMap::new(),
                    });
            let order = FamilySequencerOrder {
                started_at_ns: now_unix_nanos(),
                ordinal: state.next_ordinal,
            };
            state.next_ordinal = state.next_ordinal.saturating_add(1);
            state.entries.insert(
                order,
                FamilySequencerEntry::Checkpoint {
                    request: Box::new(request),
                    respond_to,
                },
            );
        }

        self.drain_ready_family_sequencer_entries_locked(family)
            .await
    }

    pub(crate) async fn drain_ready_family_sequencer_entries_locked(
        &self,
        family: &str,
    ) -> Result<(), GitAiError> {
        let mut ready: Vec<(u64, FamilySequencerEntry)> = Vec::new();
        let mut progressed = false;
        {
            let mut map = self.family_sequencers_by_family.lock().map_err(|_| {
                PersistenceError::LockPoisoned {
                    what: "family sequencer map",
                }
            })?;
            let state = map
                .entry(family.to_string())
                .or_insert_with(|| FamilySequencerState {
                    next_ordinal: 1,
                    entries: BTreeMap::new(),
                });
            while let Some(first_entry) = state.entries.first_entry() {
                if matches!(first_entry.get(), FamilySequencerEntry::PendingRoot) {
                    break;
                }
                let entry_root_sid = match first_entry.get() {
                    FamilySequencerEntry::ReadyCommand(command) => Some(command.root_sid.as_str()),
                    _ => None,
                };
                if self.family_entry_blocked_by_prior_open_trace_root(
                    family,
                    first_entry.key().started_at_ns,
                    entry_root_sid,
                )? {
                    break;
                }
                let (order, entry) = first_entry.remove_entry();
                match entry {
                    FamilySequencerEntry::PendingRoot => {
                        unreachable!("pending root should not be removed from sequencer front");
                    }
                    other => {
                        ready.push((order.ordinal, other));
                        progressed = true;
                    }
                }
            }
        }

        if ready.is_empty() {
            return Ok(());
        }

        let _ = self.begin_family_effect(family);
        for (order, ready_entry) in ready {
            match ready_entry {
                FamilySequencerEntry::ReadyCommand(command) => {
                    // Wrap the entire command + side-effect pipeline in catch_unwind
                    // so that a panic (e.g. from UTF-8 boundary issues in diff parsing)
                    // does not kill the daemon process.
                    let side_effect_result = {
                        let future = async {
                            let root_sid = command.root_sid.clone();
                            let mut commit_file_timestamp_snapshots =
                                self.take_cached_commit_file_timestamp_snapshots(&root_sid)?;
                            let applied = self.coordinator.route_command(*command).await?;
                            let side_effect = self
                                .maybe_apply_side_effects_for_applied_command(
                                    Some(family),
                                    &applied,
                                    &mut commit_file_timestamp_snapshots,
                                )
                                .await;
                            Ok::<_, GitAiError>((applied, side_effect))
                        };
                        let caught = std::panic::AssertUnwindSafe(future);
                        futures::FutureExt::catch_unwind(caught).await
                    };
                    match side_effect_result {
                        Ok(Ok((applied, side_effect_result))) => {
                            if let Err(error) = &side_effect_result {
                                let _ = self.record_side_effect_error(family, order, error);
                                tracing::error!(
                                    %error,
                                    %family,
                                    seq = applied.seq,
                                    "command side effect failed"
                                );
                            }
                            if let Err(error) = self.append_command_completion_log(
                                family,
                                &applied,
                                &side_effect_result,
                                order,
                            ) {
                                let _ = self.record_side_effect_error(family, order, &error);
                                tracing::error!(
                                    %error,
                                    %family,
                                    order,
                                    "command completion log write failed"
                                );
                            }
                        }
                        Ok(Err(error)) => {
                            let _ = self.record_side_effect_error(family, order, &error);
                            tracing::error!(
                                %error,
                                %family,
                                order,
                                "command apply failed"
                            );
                        }
                        Err(panic_payload) => {
                            let panic_msg = if let Some(s) = panic_payload.downcast_ref::<String>()
                            {
                                s.clone()
                            } else if let Some(s) = panic_payload.downcast_ref::<&str>() {
                                s.to_string()
                            } else {
                                "unknown panic".to_string()
                            };
                            let error = GitAiError::Generic(format!(
                                "daemon command side effect panic: {}",
                                panic_msg
                            ));
                            let _ = self.record_side_effect_error(family, order, &error);
                            tracing::error!(
                                component = "daemon",
                                phase = "command_side_effect",
                                reason = "panic_in_side_effect",
                                panic_msg = %panic_msg,
                                %family,
                                order,
                                "command side effect panic"
                            );
                        }
                    }
                }
                FamilySequencerEntry::Checkpoint {
                    mut request,
                    respond_to,
                } => {
                    let repo_wd = request
                        .files
                        .first()
                        .map(|f| f.repo_work_dir.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let checkpoint_file_paths: Vec<String> = request
                        .files
                        .iter()
                        .map(|f| f.path.to_string_lossy().to_string())
                        .collect();
                    let checkpoint_kind = request.checkpoint_kind;
                    let checkpoint_path_role = request.path_role;
                    let checkpoint_has_agent = request.agent_id.is_some();
                    let checkpoint_kind_str = format!("{:?}", checkpoint_kind);
                    let is_human_checkpoint = checkpoint_kind == CheckpointKind::Human;

                    // Register pending AI edit state when an AI agent fires its
                    // pre-edit snapshot. This signals that an AI edit is in-flight.
                    // Identified by: WillEdit path_role + agent_id present (only AI
                    // agent presets have an agent_id on their pre-edit checkpoints).
                    if checkpoint_path_role == PreparedPathRole::WillEdit && checkpoint_has_agent {
                        self.register_pending_ai_edits(family, &checkpoint_file_paths);
                    }

                    // Filter out files with pending AI edits from KnownHuman checkpoints.
                    // These are spurious IDE save events that fire between pre/post-edit.
                    if checkpoint_kind == CheckpointKind::KnownHuman {
                        let pending_files: Vec<String> = checkpoint_file_paths
                            .iter()
                            .filter(|f| self.file_has_pending_ai_edit(family, f))
                            .cloned()
                            .collect();
                        if !pending_files.is_empty() {
                            request.files.retain(|f| {
                                let path_str = f.path.to_string_lossy().to_string();
                                !pending_files.contains(&path_str)
                            });
                            tracing::debug!(
                                "[KnownHuman] Filtered {} file(s) with pending AI edits",
                                pending_files.len()
                            );
                            if request.files.is_empty() {
                                let log_entry = TestCompletionLogEntry {
                                    seq: 0,
                                    family_key: family.to_string(),
                                    kind: "checkpoint".to_string(),
                                    primary_command: Some("checkpoint".to_string()),
                                    test_sync_session: None,
                                    exit_code: None,
                                    sync_tracked: true,
                                    status: "suppressed".to_string(),
                                    error: None,
                                    semantic_events: Vec::new(),
                                    commit_shas: Vec::new(),
                                    commit_skip_reason: None,
                                };
                                let _ = self.maybe_append_test_completion_log(family, &log_entry);
                                if let Some(respond_to) = respond_to {
                                    let _ = respond_to.send(Ok(0));
                                }
                                continue;
                            }
                        }
                    }

                    // Recompute file paths after potential KnownHuman filtering so
                    // watermark computation and clear_pending_ai_edits use the actual
                    // files that will be checkpointed.
                    let checkpoint_file_paths: Vec<String> = request
                        .files
                        .iter()
                        .map(|f| f.path.to_string_lossy().to_string())
                        .collect();

                    let should_log_completion = true; // Always log for test sync
                    tracing::info!(kind = %checkpoint_kind_str, repo = %repo_wd, "checkpoint start");
                    let checkpoint_start = std::time::Instant::now();
                    let checkpoint_request = {
                        let future = async {
                            if !repo_wd.is_empty() {
                                let ack =
                                    self.coordinator.apply_checkpoint(Path::new(&repo_wd)).await;
                                match ack {
                                    Ok(ack) => {
                                        apply_checkpoint_side_effect(*request).map(|_| ack.seq)
                                    }
                                    Err(error) => Err(error),
                                }
                            } else {
                                apply_checkpoint_side_effect(*request).map(|_| 0)
                            }
                        };
                        let caught = std::panic::AssertUnwindSafe(future);
                        futures::FutureExt::catch_unwind(caught).await
                    };
                    let result = match checkpoint_request {
                        Ok(inner) => inner,
                        Err(panic_payload) => {
                            let panic_msg = if let Some(s) = panic_payload.downcast_ref::<String>()
                            {
                                s.clone()
                            } else if let Some(s) = panic_payload.downcast_ref::<&str>() {
                                s.to_string()
                            } else {
                                "unknown panic".to_string()
                            };
                            tracing::error!(
                                component = "daemon",
                                phase = "checkpoint_side_effect",
                                reason = "panic_in_side_effect",
                                panic_msg = %panic_msg,
                                %family,
                                order,
                                "checkpoint side effect panic"
                            );
                            Err(GitAiError::Generic(format!(
                                "daemon checkpoint panic: {}",
                                panic_msg
                            )))
                        }
                    };
                    let checkpoint_duration_ms = checkpoint_start.elapsed().as_millis();
                    if result.is_ok() {
                        tracing::info!(
                            kind = %checkpoint_kind_str,
                            repo = %repo_wd,
                            duration_ms = checkpoint_duration_ms as u64,
                            "checkpoint done"
                        );
                    } else {
                        tracing::warn!(
                            kind = %checkpoint_kind_str,
                            repo = %repo_wd,
                            duration_ms = checkpoint_duration_ms as u64,
                            "checkpoint failed"
                        );
                    }
                    if result.is_ok() {
                        // Clear pending AI edit state once the PostFileEdit completes.
                        if checkpoint_kind.is_ai()
                            && checkpoint_path_role == PreparedPathRole::Edited
                        {
                            self.clear_pending_ai_edits(family, &checkpoint_file_paths);
                        }
                        let per_file = if !checkpoint_file_paths.is_empty() {
                            compute_watermarks_from_stat(&repo_wd, &checkpoint_file_paths)
                        } else {
                            std::collections::HashMap::new()
                        };
                        let per_worktree = if is_human_checkpoint {
                            let now_ns = crate::model::clock::now_nanos();
                            std::collections::HashMap::from([(
                                Self::worktree_state_key(Path::new(&repo_wd)),
                                now_ns,
                            )])
                        } else {
                            std::collections::HashMap::new()
                        };
                        if !per_file.is_empty() || !per_worktree.is_empty() {
                            let _ = self
                                .coordinator
                                .update_watermarks_family(
                                    Path::new(&repo_wd),
                                    crate::model::domain::WatermarkState {
                                        per_file,
                                        per_worktree,
                                    },
                                )
                                .await;
                        }
                    }
                    // Removed captured_checkpoint_id cleanup - no more captured checkpoints
                    if let Err(error) = &result {
                        let _ = self.record_side_effect_error(family, order, error);
                        tracing::error!(
                            %error,
                            %family,
                            order,
                            "checkpoint side effect failed"
                        );
                    }
                    if should_log_completion {
                        let log_entry = TestCompletionLogEntry {
                            seq: result.as_ref().copied().unwrap_or(0),
                            family_key: family.to_string(),
                            kind: "checkpoint".to_string(),
                            primary_command: Some("checkpoint".to_string()),
                            test_sync_session: None,
                            exit_code: None,
                            sync_tracked: true,
                            status: if result.is_ok() {
                                "ok".to_string()
                            } else {
                                "error".to_string()
                            },
                            error: result.as_ref().err().map(|error| error.to_string()),
                            semantic_events: Vec::new(),
                            commit_shas: Vec::new(),
                            commit_skip_reason: None,
                        };
                        if let Err(error) =
                            self.maybe_append_test_completion_log(family, &log_entry)
                        {
                            let _ = self.record_side_effect_error(family, order, &error);
                            tracing::error!(
                                %error,
                                %family,
                                order,
                                "checkpoint completion log write failed"
                            );
                        }
                    }
                    if let Some(respond_to) = respond_to {
                        let _ = respond_to.send(result);
                    }
                }
                FamilySequencerEntry::Canceled => {}
                FamilySequencerEntry::PendingRoot => {}
            }
        }
        let _ = self.end_family_effect(family);

        let _ = progressed;
        Ok(())
    }
}
