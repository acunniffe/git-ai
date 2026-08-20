#[allow(unused_imports)]
use super::*;
use crate::error::GitAiError;
use crate::operations::daemon::git_backend::GitBackend;
use serde_json::json;
use std::path::Path;
use std::sync::atomic::Ordering;

impl ActorDaemonCoordinator {
    pub(crate) async fn watermarks_for_family(
        &self,
        repo_working_dir: String,
    ) -> Result<crate::model::domain::WatermarkState, GitAiError> {
        self.coordinator
            .watermarks_family(Path::new(&repo_working_dir))
            .await
    }

    pub(crate) async fn status_for_family(
        &self,
        repo_working_dir: String,
    ) -> Result<FamilyStatus, GitAiError> {
        let family = self.backend.resolve_family(Path::new(&repo_working_dir))?;
        let status = self
            .coordinator
            .status_family(Path::new(&repo_working_dir))
            .await?;
        let latest_seq = status.applied_seq;
        let family_key = family.0;
        Ok(FamilyStatus {
            family_key: family_key.clone(),
            latest_seq,
            last_error: status
                .last_error
                .or_else(|| self.latest_side_effect_error(&family_key).ok().flatten()),
        })
    }

    pub(crate) async fn sync_family(
        &self,
        repo_working_dir: String,
    ) -> Result<FamilyStatus, GitAiError> {
        let family = self.backend.resolve_family(Path::new(&repo_working_dir))?;
        // Family-scoped fence: a sync request only has to wait for trace roots
        // that could affect its own repository family. `await_completion` and
        // shutdown keep the global fence.
        self.wait_for_trace_ingest_processed_through_family(&family.0)
            .await;

        let exec_lock = self.side_effect_exec_lock(&family.0)?;
        let _guard = exec_lock.lock().await;
        self.drain_ready_family_sequencer_entries_locked(&family.0)
            .await?;

        self.status_for_family(repo_working_dir).await
    }

    /// Wait for the daemon to finish all in-flight work and telemetry flushing.
    ///
    /// Progress is logged every few seconds. Returns an `AwaitResult` describing
    /// whether the daemon was idle before the timeout and how much telemetry
    /// (if any) is still pending.
    pub(crate) async fn await_completion(&self, timeout_secs: u64) -> AwaitResult {
        use tokio::time::{Duration, Instant, timeout};

        let start = Instant::now();
        let deadline = start + Duration::from_secs(timeout_secs);
        let log_interval = Duration::from_secs(3);
        let mut last_log = start;

        let mut result = AwaitResult {
            done: false,
            timed_out: false,
            metrics_remaining: 0,
            notes_remaining: 0,
        };

        let mut maybe_log = |phase: &str| {
            let now = Instant::now();
            if now - last_log >= log_interval {
                tracing::info!(phase, "await: still waiting");
                eprintln!("await: still waiting for {}...", phase);
                last_log = now;
            }
        };

        // Phase 1: wait for the trace-ingest and family-sequencer work side.
        while !self.is_shutting_down() {
            let now = Instant::now();
            if now >= deadline {
                result.timed_out = true;
                break;
            }
            let remaining = deadline - now;

            maybe_log("daemon work");
            if timeout(remaining, self.wait_for_trace_ingest_processed_through())
                .await
                .is_err()
            {
                result.timed_out = true;
                break;
            }

            if self.is_shutting_down() {
                break;
            }

            let now = Instant::now();
            if now >= deadline {
                result.timed_out = true;
                break;
            }
            let remaining = deadline - now;

            if timeout(remaining, self.drain_all_ready_family_sequencers())
                .await
                .is_err()
            {
                result.timed_out = true;
                break;
            }

            if !self.has_pending_daemon_work() {
                break;
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        if self.is_shutting_down() {
            result.timed_out = true;
        }

        // Phase 2: drain the transcript/stream worker.
        if !result.timed_out
            && let Some(worker) = &self.stream_worker
        {
            let now = Instant::now();
            if now < deadline {
                let remaining = deadline - now;
                maybe_log("transcript processing");
                match timeout(remaining, worker.drain()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "await: transcript drain failed");
                    }
                    Err(_) => {
                        result.timed_out = true;
                    }
                }
            } else {
                result.timed_out = true;
            }
        }

        // Phase 3: flush telemetry and wait for the worker to finish.
        if !result.timed_out
            && let Some(worker) = &self.telemetry_worker
        {
            let now = Instant::now();
            if now < deadline {
                let remaining = deadline - now;
                maybe_log("telemetry flush");
                match timeout(remaining, worker.flush_and_wait()).await {
                    Ok(Ok(status)) => {
                        result.metrics_remaining = status.metrics_remaining;
                        result.notes_remaining = status.notes_remaining;
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "await: telemetry flush failed");
                    }
                    Err(_) => {
                        result.timed_out = true;
                    }
                }
            } else {
                result.timed_out = true;
            }
        }

        result.done = !result.timed_out
            && result.metrics_remaining == 0
            && result.notes_remaining == 0
            && !self.has_pending_daemon_work();
        result
    }

    pub(crate) fn has_pending_daemon_work(&self) -> bool {
        if self.queued_trace_payloads.load(Ordering::Acquire) > 0 {
            return true;
        }
        if self.next_trace_ingest_seq.load(Ordering::Acquire)
            > self.processed_trace_ingest_seq.load(Ordering::Acquire)
        {
            return true;
        }
        if self.has_open_trace_roots_that_may_mutate_refs() {
            return true;
        }
        if let Ok(map) = self.inflight_effects_by_family.lock()
            && !map.is_empty()
        {
            return true;
        }
        if let Ok(map) = self.family_sequencers_by_family.lock() {
            for state in map.values() {
                if !state.entries.is_empty() {
                    return true;
                }
            }
        }
        false
    }

    pub(crate) fn notify_stream_worker_checkpoint(
        &self,
        request: &crate::model::checkpoint_request::CheckpointRequest,
    ) {
        let Some(worker) = &self.stream_worker else {
            return;
        };
        let Some(stream_source) = &request.stream_source else {
            return;
        };
        let tool = request
            .agent_id
            .as_ref()
            .map(|agent_id| agent_id.tool.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let repo_work_dir = request.files.first().map(|file| file.repo_work_dir.clone());

        worker.notify_checkpoint(
            stream_source.session_id.clone(),
            tool,
            request.trace_id.clone(),
            request.metadata.get("tool_use_id").cloned(),
            stream_source.path.clone(),
            stream_source.format,
            repo_work_dir,
            stream_source.external_session_id.clone(),
            stream_source.external_parent_session_id.clone(),
        );
    }

    pub(crate) async fn handle_control_request(&self, request: ControlRequest) -> ControlResponse {
        let result = match request {
            ControlRequest::Ping => Ok(ControlResponse::ok(None, None)),
            ControlRequest::CheckpointRun { request } => {
                self.ingest_checkpoint_control_payload(*request).await
            }
            ControlRequest::CheckpointDeliver { delivery } => {
                self.ingest_checkpoint_delivery(*delivery).await
            }
            ControlRequest::SyncFamily { repo_working_dir } => {
                self.sync_family(repo_working_dir).await.and_then(|status| {
                    serde_json::to_value(status)
                        .map(|v| ControlResponse::ok(None, Some(v)))
                        .map_err(GitAiError::from)
                })
            }
            ControlRequest::StatusFamily { repo_working_dir } => self
                .status_for_family(repo_working_dir)
                .await
                .and_then(|status| {
                    serde_json::to_value(status)
                        .map(|v| ControlResponse::ok(None, Some(v)))
                        .map_err(GitAiError::from)
                }),
            ControlRequest::SnapshotWatermarks { repo_working_dir } => self
                .watermarks_for_family(repo_working_dir.clone())
                .await
                .and_then(|ws| {
                    let worktree_key = Self::worktree_state_key(Path::new(&repo_working_dir));
                    let worktree_wm = ws.per_worktree.get(&worktree_key).copied();
                    serde_json::to_value(json!({
                        "watermarks": ws.per_file,
                        "worktree_watermark": worktree_wm,
                    }))
                    .map(|v| ControlResponse::ok(None, Some(v)))
                    .map_err(GitAiError::from)
                }),
            ControlRequest::SubmitTelemetry { envelopes } => {
                if let Some(worker) = &self.telemetry_worker {
                    worker.submit_telemetry(envelopes).await;
                }
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::SubmitCas { records } => {
                if let Some(worker) = &self.telemetry_worker {
                    worker.submit_cas(records).await;
                }
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::FlushNotes => {
                // Trigger an immediate notes flush in a blocking task.
                // Route through the worker so the injected notes-db handle is used.
                // Fire-and-forget: the periodic flush loop is the safety net.
                if let Some(worker) = self.telemetry_worker.clone() {
                    tokio::task::spawn_blocking(move || {
                        worker.flush_notes_sync();
                    });
                } else {
                    tokio::task::spawn_blocking(|| {
                        crate::operations::daemon::telemetry_worker::flush_notes_global();
                    });
                }
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::Await { timeout_secs } => {
                let result = self.await_completion(timeout_secs).await;
                serde_json::to_value(result)
                    .map(|v| ControlResponse::ok(None, Some(v)))
                    .map_err(GitAiError::from)
            }
            ControlRequest::BashSessionStart {
                repo_work_dir,
                original_cwd,
                session_id,
                tool_use_id,
                agent_id,
                metadata,
                stat_snapshot,
                trace_id,
                started_at_ns,
                command,
            } => {
                let worktree_key = Self::worktree_state_key(Path::new(&repo_work_dir));
                let original_cwd = original_cwd.unwrap_or_else(|| repo_work_dir.clone());
                if let Ok(db) = self.bash_history_db()
                    && let Ok(mut db_lock) = db.lock()
                    && let Err(e) = db_lock.record_start(
                        &crate::model::repository::bash_history_db::BashCallStart {
                            original_cwd: Self::worktree_state_key(Path::new(&original_cwd)),
                            repo_work_dir: Some(worktree_key.clone()),
                            repo_discovery_error: None,
                            session_id: session_id.clone(),
                            tool_use_id: tool_use_id.clone(),
                            agent_id: agent_id.clone(),
                            start_trace_id: trace_id.clone(),
                            started_at_ns,
                            command: command.clone(),
                            metadata: metadata.clone(),
                        },
                    )
                {
                    tracing::debug!("failed to persist bash session start: {}", e);
                }

                let mut state = self.bash_sessions.lock().unwrap();
                state.start_session(crate::operations::daemon::bash_sessions::BashSessionStart {
                    session_id,
                    tool_use_id,
                    repo_work_dir: worktree_key,
                    agent_id,
                    metadata,
                    stat_snapshot: *stat_snapshot,
                    start_trace_id: trace_id,
                    started_at_ns,
                    command,
                });
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::BashSessionEnd {
                repo_work_dir,
                original_cwd,
                session_id,
                tool_use_id,
                agent_id,
                metadata,
                trace_id,
                ended_at_ns,
                command,
            } => {
                let mut state = self.bash_sessions.lock().unwrap();
                let session = state.end_session(&session_id, &tool_use_id);
                drop(state);

                let worktree_key = session
                    .as_ref()
                    .map(|s| s.repo_work_dir.clone())
                    .unwrap_or_else(|| Self::worktree_state_key(Path::new(&repo_work_dir)));
                let original_cwd = original_cwd
                    .map(|cwd| Self::worktree_state_key(Path::new(&cwd)))
                    .unwrap_or_else(|| worktree_key.clone());
                let start_trace_id = session.as_ref().map(|s| s.start_trace_id.clone());
                let started_at_ns = session.as_ref().map(|s| s.started_at_ns);
                let command = command.or_else(|| session.as_ref().and_then(|s| s.command.clone()));
                let agent_id = session
                    .as_ref()
                    .map(|s| s.agent_id.clone())
                    .unwrap_or(agent_id);
                let metadata = if metadata.is_empty() {
                    session
                        .as_ref()
                        .map(|s| s.metadata.clone())
                        .unwrap_or_default()
                } else {
                    metadata
                };
                if let Ok(db) = self.bash_history_db()
                    && let Ok(mut db_lock) = db.lock()
                    && let Err(e) = db_lock.record_end(
                        &crate::model::repository::bash_history_db::BashCallEnd {
                            original_cwd,
                            repo_work_dir: Some(worktree_key),
                            repo_discovery_error: None,
                            session_id,
                            tool_use_id,
                            agent_id,
                            start_trace_id,
                            end_trace_id: trace_id,
                            started_at_ns,
                            ended_at_ns,
                            command,
                            metadata,
                        },
                    )
                {
                    tracing::debug!("failed to persist bash session end: {}", e);
                }
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::BashHookAttemptStart {
                original_cwd,
                discovered_repo_work_dir,
                repo_discovery_error,
                session_id,
                tool_use_id,
                agent_id,
                metadata,
                trace_id,
                started_at_ns,
                command,
            } => {
                let discovered_repo_work_dir = discovered_repo_work_dir
                    .as_deref()
                    .map(Path::new)
                    .map(Self::worktree_state_key);
                if let Ok(db) = self.bash_history_db()
                    && let Ok(mut db_lock) = db.lock()
                    && let Err(e) = db_lock.record_start(
                        &crate::model::repository::bash_history_db::BashCallStart {
                            original_cwd: Self::worktree_state_key(Path::new(&original_cwd)),
                            repo_work_dir: discovered_repo_work_dir,
                            repo_discovery_error,
                            session_id,
                            tool_use_id,
                            agent_id,
                            start_trace_id: trace_id,
                            started_at_ns,
                            command,
                            metadata,
                        },
                    )
                {
                    tracing::debug!("failed to persist bash hook attempt start: {}", e);
                }
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::BashHookAttemptEnd {
                original_cwd,
                discovered_repo_work_dir,
                repo_discovery_error,
                session_id,
                tool_use_id,
                agent_id,
                metadata,
                trace_id,
                ended_at_ns,
                command,
            } => {
                let discovered_repo_work_dir = discovered_repo_work_dir
                    .as_deref()
                    .map(Path::new)
                    .map(Self::worktree_state_key);
                if let Ok(db) = self.bash_history_db()
                    && let Ok(mut db_lock) = db.lock()
                    && let Err(e) = db_lock.record_end(
                        &crate::model::repository::bash_history_db::BashCallEnd {
                            original_cwd: Self::worktree_state_key(Path::new(&original_cwd)),
                            repo_work_dir: discovered_repo_work_dir,
                            repo_discovery_error,
                            session_id,
                            tool_use_id,
                            agent_id,
                            start_trace_id: None,
                            end_trace_id: trace_id,
                            started_at_ns: None,
                            ended_at_ns,
                            command,
                            metadata,
                        },
                    )
                {
                    tracing::debug!("failed to persist bash hook attempt end: {}", e);
                }
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::BashSessionQuery { repo_work_dir } => {
                let state = self.bash_sessions.lock().unwrap();
                let repo_work_dir = Self::worktree_state_key(Path::new(&repo_work_dir));
                let response = match state.query_active_for_repo(&repo_work_dir) {
                    Some((key, session)) => {
                        let data = serde_json::to_value(BashSessionQueryResponse {
                            active: true,
                            agent_id: Some(session.agent_id.clone()),
                            session_id: Some(key.0.clone()),
                            tool_use_id: Some(key.1.clone()),
                            metadata: Some(session.metadata.clone()),
                        })
                        .ok();
                        ControlResponse::ok(None, data)
                    }
                    None => {
                        let data = serde_json::to_value(BashSessionQueryResponse {
                            active: false,
                            agent_id: None,
                            session_id: None,
                            tool_use_id: None,
                            metadata: None,
                        })
                        .ok();
                        ControlResponse::ok(None, data)
                    }
                };
                Ok(response)
            }
            ControlRequest::BashSnapshotQuery {
                session_id,
                tool_use_id,
            } => {
                let state = self.bash_sessions.lock().unwrap();
                let response = match state.get_snapshot(&session_id, &tool_use_id) {
                    Some(snapshot) => {
                        let data = serde_json::to_value(BashSnapshotQueryResponse {
                            found: true,
                            stat_snapshot: Some(snapshot.clone()),
                        })
                        .ok();
                        ControlResponse::ok(None, data)
                    }
                    None => {
                        let data = serde_json::to_value(BashSnapshotQueryResponse {
                            found: false,
                            stat_snapshot: None,
                        })
                        .ok();
                        ControlResponse::ok(None, data)
                    }
                };
                Ok(response)
            }
            ControlRequest::Shutdown => Ok(ControlResponse::ok(None, None)),
        };

        match result {
            Ok(response) => response,
            Err(error) => ControlResponse::err(error.to_string()),
        }
    }
}
