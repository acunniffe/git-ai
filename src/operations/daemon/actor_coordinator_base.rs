use super::actor_types::{ActorDaemonCoordinator, DaemonExitAction, TraceIngressState};
use crate::error::GitAiError;
use crate::model::repository::error::PersistenceError;
use crate::operations::daemon::DaemonConfig;
use crate::operations::daemon::actor_types::{
    SESSION_EVENT_RECOVERY_PREFLIGHT_POLL, SESSION_EVENT_RECOVERY_PREFLIGHT_WAIT,
};
use crate::operations::daemon::log_setup::now_unix_nanos;
use crate::operations::daemon::side_effect_helpers::capture_commit_file_timestamps;
use crate::operations::git::repository::Repository;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, Notify};

impl ActorDaemonCoordinator {
    /// Store a weak self-reference so family drains can be scheduled on
    /// their own tasks. Called once, right after the coordinator is wrapped
    /// in its `Arc`. Coordinators that never register keep the historical
    /// inline-drain behavior.
    pub(crate) fn register_self(self: &Arc<Self>) {
        let _ = self.self_ref.set(Arc::downgrade(self));
    }

    pub(crate) fn upgraded_self(&self) -> Option<Arc<Self>> {
        self.self_ref.get().and_then(std::sync::Weak::upgrade)
    }

    pub(crate) fn new() -> Self {
        let backend = Arc::new(crate::operations::daemon::git_backend::SystemGitBackend::new());
        Self {
            coordinator: Arc::new(crate::operations::daemon::coordinator::Coordinator::new(
                backend.clone(),
            )),
            normalizer: AsyncMutex::new(
                crate::operations::daemon::trace_normalizer::TraceNormalizer::new(backend.clone()),
            ),
            backend,
            pending_rebase_original_head_by_worktree: Mutex::new(HashMap::new()),
            pending_cherry_pick_sources_by_worktree: Mutex::new(HashMap::new()),
            pending_cherry_pick_no_commit_by_worktree: Mutex::new(HashMap::new()),
            pending_squash_merge_by_worktree: Mutex::new(HashMap::new()),
            inflight_effects_by_family: Mutex::new(HashMap::new()),
            pending_ai_edits_by_family: Mutex::new(HashMap::new()),
            family_sequencers_by_family: Mutex::new(HashMap::new()),
            pending_root_slots_by_root: Mutex::new(HashMap::new()),
            commit_file_timestamp_snapshots_by_root: Mutex::new(HashMap::new()),
            recent_replay_prerequisites_by_family: Mutex::new(HashMap::new()),
            side_effect_errors_by_family: Mutex::new(HashMap::new()),
            side_effect_exec_locks: Mutex::new(HashMap::new()),
            self_ref: std::sync::OnceLock::new(),
            scheduled_family_drains: Mutex::new(HashMap::new()),
            bash_sessions: Mutex::new(
                crate::operations::daemon::bash_sessions::BashSessionState::new(),
            ),
            test_completion_log_dir: std::env::var("GIT_AI_TEST_DB_PATH")
                .ok()
                .or_else(|| std::env::var("GITAI_TEST_DB_PATH").ok())
                .map(|_| {
                    DaemonConfig::from_env_or_default_paths()
                        .map(|config| config.test_completion_log_dir())
                        .unwrap_or_else(|_| {
                            std::env::temp_dir().join("git-ai-daemon-test-completions-fallback")
                        })
                }),
            test_completion_log_lock: Mutex::new(()),
            trace_ingest_tx: std::sync::OnceLock::new(),
            telemetry_worker: None,
            stream_worker: None,
            transcript_shutdown_notify: std::sync::OnceLock::new(),
            streams_db: None,
            bash_history_db: None,
            metrics_db: None,
            next_trace_ingest_seq: AtomicUsize::new(0),
            queued_trace_payloads: AtomicUsize::new(0),
            queued_trace_payloads_by_root: Mutex::new(HashMap::new()),
            processed_trace_ingest_seq: AtomicUsize::new(0),
            trace_ingest_progress_notify: Notify::new(),
            trace_ingress_state: Mutex::new(TraceIngressState::default()),
            shutting_down: AtomicBool::new(false),
            shutdown_action: AtomicU8::new(DaemonExitAction::Stop.as_u8()),
            shutdown_notify: Notify::new(),
            shutdown_condvar: std::sync::Condvar::new(),
            shutdown_condvar_mutex: Mutex::new(()),
        }
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        // Acquire pairs with the Release store in request_shutdown so all
        // writes made before shutdown is requested are visible to the caller.
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Return the injected bash-history handle, falling back to global() for unit tests
    /// (where the field is None because new() initializes it to None).
    pub(crate) fn bash_history_db(
        &self,
    ) -> Result<
        &'static std::sync::Mutex<crate::model::repository::bash_history_db::BashHistoryDatabase>,
        crate::error::GitAiError,
    > {
        match self.bash_history_db {
            Some(db) => Ok(db),
            None => crate::model::repository::bash_history_db::BashHistoryDatabase::global(),
        }
    }

    /// Build a [`RecoveryStores`] from injected handles, falling back to `resolve()` for
    /// fields that are `None` (unit-test constructions where handles are not injected).
    pub(crate) fn recovery_stores(
        &self,
    ) -> crate::operations::authorship::recovery_stores::RecoveryStores {
        use crate::operations::authorship::recovery_stores::RecoveryStores;
        // When both fields are populated (daemon path), no ::global() is called.
        // When either is None (unit tests), fall back to the global singleton.
        RecoveryStores {
            metrics: self
                .metrics_db
                .or_else(|| crate::model::repository::metrics_db::MetricsDatabase::global().ok()),
            bash_history: self.bash_history_db.or_else(|| {
                crate::model::repository::bash_history_db::BashHistoryDatabase::global().ok()
            }),
        }
    }

    pub(crate) fn trigger_transcript_sweep(
        &self,
        trigger: crate::operations::daemon::stream_worker::SweepTrigger,
    ) {
        let Some(worker) = &self.stream_worker else {
            tracing::debug!(trigger = %trigger, "transcript sweep trigger skipped; worker is not running");
            return;
        };

        if worker.trigger_sweep(trigger) {
            tracing::info!(trigger = %trigger, "transcript sweep trigger enqueued");
        } else {
            tracing::debug!(trigger = %trigger, "transcript sweep trigger not enqueued");
        }
    }

    pub(crate) fn trigger_transcript_sweep_for_recovery(
        &self,
        trigger: crate::operations::daemon::stream_worker::SweepTrigger,
    ) -> Option<std::sync::mpsc::Receiver<Result<(), String>>> {
        let Some(worker) = &self.stream_worker else {
            tracing::debug!(trigger = %trigger, "recovery transcript sweep skipped; worker is not running");
            return None;
        };

        let completion = worker.trigger_sweep_for_recovery(trigger);
        if completion.is_some() {
            tracing::info!(trigger = %trigger, "recovery transcript sweep enqueued");
        } else {
            tracing::debug!(trigger = %trigger, "recovery transcript sweep not enqueued");
        }
        completion
    }

    pub(crate) fn wait_for_session_event_recovery_candidate(
        &self,
        repo: &Repository,
        commit_sha: &str,
        recovery_file_timestamps: Option<
            &crate::operations::authorship::attribution_recovery::FileTimestampsByPath,
        >,
        unknown_by_file: &crate::operations::authorship::attribution_recovery::UnknownLinesByFile,
    ) {
        if unknown_by_file.is_empty() {
            return;
        }
        let unknown_files = unknown_by_file
            .keys()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let mut timestamps = recovery_file_timestamps
            .map(|recovery_file_timestamps| {
                recovery_file_timestamps
                    .iter()
                    .filter(|(file_path, _)| unknown_files.contains(file_path.as_str()))
                    .flat_map(|(_, values)| values.iter().copied())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if timestamps.is_empty()
            && let Ok(workdir) = repo.workdir()
            && let Ok(fallback_timestamps) = capture_commit_file_timestamps(&workdir, commit_sha)
        {
            timestamps = fallback_timestamps
                .iter()
                .filter(|(file_path, _)| unknown_files.contains(file_path.as_str()))
                .flat_map(|(_, values)| values.iter().copied())
                .collect::<Vec<_>>();
        }
        if timestamps.is_empty() {
            timestamps = recovery_file_timestamps
                .map(|recovery_file_timestamps| {
                    recovery_file_timestamps
                        .values()
                        .flat_map(|values| values.iter().copied())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
        }
        if timestamps.is_empty()
            && let Ok(workdir) = repo.workdir()
            && let Ok(fallback_timestamps) = capture_commit_file_timestamps(&workdir, commit_sha)
        {
            timestamps = fallback_timestamps
                .values()
                .flat_map(|values| values.iter().copied())
                .collect::<Vec<_>>();
        }
        if timestamps.is_empty() {
            return;
        }
        timestamps.sort_unstable();
        timestamps.dedup();

        let Some(target_repo_url) = crate::repo_url::resolve_repo_url_from_repo(repo) else {
            return;
        };

        let stores = self.recovery_stores();
        let has_candidate = || {
            crate::operations::authorship::attribution_recovery::matching_session_event_candidate_exists(
                &timestamps,
                &target_repo_url,
                stores,
            )
            .unwrap_or_else(|error| {
                tracing::debug!(%error, "failed checking session-event recovery candidates");
                false
            })
        };
        if has_candidate() {
            return;
        }

        let deadline = std::time::Instant::now() + SESSION_EVENT_RECOVERY_PREFLIGHT_WAIT;
        let sweep_completion = self.trigger_transcript_sweep_for_recovery(
            crate::operations::daemon::stream_worker::SweepTrigger::PostCommit,
        );

        let Some(sweep_completion) = sweep_completion else {
            return;
        };

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            tracing::debug!(
                wait_ms = SESSION_EVENT_RECOVERY_PREFLIGHT_WAIT.as_millis() as u64,
                "recovery transcript sweep wait expired"
            );
            return;
        }
        match sweep_completion.recv_timeout(remaining) {
            Ok(Ok(())) => {
                tracing::debug!("recovery transcript sweep completed before post-commit");
            }
            Ok(Err(error)) => {
                tracing::debug!(
                    %error,
                    "recovery transcript sweep failed before post-commit"
                );
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                tracing::debug!(
                    wait_ms = SESSION_EVENT_RECOVERY_PREFLIGHT_WAIT.as_millis() as u64,
                    "recovery transcript sweep wait expired"
                );
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::debug!("recovery transcript sweep completion channel disconnected");
                return;
            }
        }

        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                tracing::debug!(
                    wait_ms = SESSION_EVENT_RECOVERY_PREFLIGHT_WAIT.as_millis() as u64,
                    "session-event recovery preflight wait expired"
                );
                return;
            }
            std::thread::sleep(remaining.min(SESSION_EVENT_RECOVERY_PREFLIGHT_POLL));
            if has_candidate() {
                tracing::debug!(
                    "session-event recovery candidate became visible before post-commit"
                );
                return;
            }
            if std::time::Instant::now() >= deadline {
                tracing::debug!(
                    wait_ms = SESSION_EVENT_RECOVERY_PREFLIGHT_WAIT.as_millis() as u64,
                    "session-event recovery preflight wait expired"
                );
                return;
            }
        }
    }

    pub(crate) fn request_shutdown(&self) {
        // Release ensures that any writes made before this store are visible to
        // threads that subsequently load with Acquire (is_shutting_down).
        self.shutting_down.store(true, Ordering::Release);
        // The ingest worker exits via its select! shutdown arm (watching
        // shutdown_notify); we no longer rely on channel closure to stop it.
        self.shutdown_notify.notify_waiters();
        if let Some(transcript_shutdown) = self.transcript_shutdown_notify.get() {
            transcript_shutdown.notify_one();
        }
        // Hold the condvar mutex so notify_all cannot race with the
        // check-then-wait sequence in daemon_update_check_loop.
        let _guard = crate::model::repository::sqlite::poisoned_lock(&self.shutdown_condvar_mutex);
        self.shutdown_condvar.notify_all();
    }

    pub(crate) fn request_stop(&self) {
        self.shutdown_action
            .store(DaemonExitAction::Stop.as_u8(), Ordering::SeqCst);
        self.request_shutdown();
    }

    pub(crate) fn request_restart(&self) {
        self.shutdown_action
            .store(DaemonExitAction::Restart.as_u8(), Ordering::SeqCst);
        self.request_shutdown();
    }

    pub(crate) fn request_restart_after_update(&self) {
        self.shutdown_action.store(
            DaemonExitAction::RestartAfterUpdate.as_u8(),
            Ordering::SeqCst,
        );
        self.request_shutdown();
    }

    pub(crate) fn shutdown_action(&self) -> DaemonExitAction {
        DaemonExitAction::from_u8(self.shutdown_action.load(Ordering::SeqCst))
    }

    pub(crate) async fn wait_for_shutdown(&self) {
        // Register the Notified future BEFORE checking the flag so that a
        // request_shutdown() racing between the check and the await cannot
        // slip through without waking us (notify_waiters only wakes futures
        // that are already registered).
        let notified = self.shutdown_notify.notified();
        if self.is_shutting_down() {
            return;
        }
        notified.await;
    }

    pub(crate) fn begin_family_effect(&self, family: &str) -> Result<(), GitAiError> {
        let mut map =
            self.inflight_effects_by_family
                .lock()
                .map_err(|_| PersistenceError::LockPoisoned {
                    what: "inflight effects map",
                })?;
        let entry = map.entry(family.to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
        Ok(())
    }

    pub(crate) fn end_family_effect(&self, family: &str) -> Result<(), GitAiError> {
        let mut map =
            self.inflight_effects_by_family
                .lock()
                .map_err(|_| PersistenceError::LockPoisoned {
                    what: "inflight effects map",
                })?;
        if let Some(entry) = map.get_mut(family) {
            if *entry <= 1 {
                map.remove(family);
            } else {
                *entry -= 1;
            }
        }
        Ok(())
    }

    /// Garbage-collect empty or idle entries from per-family and per-root maps
    /// to prevent unbounded memory growth in long-running daemon processes.
    pub(crate) fn gc_stale_family_state(&self) {
        // NOTE: Do NOT call normalizer.sweep_orphans() here — it removes ALL
        // pending/deferred roots unconditionally which destroys in-flight trace
        // state.  sweep_orphans() is only safe at daemon shutdown.
        if let Ok(mut map) = self.recent_replay_prerequisites_by_family.lock() {
            map.retain(|_, entries| !entries.is_empty());
        }
        if let Ok(mut map) = self.side_effect_errors_by_family.lock() {
            map.retain(|_, errors| !errors.is_empty());
        }
        if let Ok(mut map) = self.family_sequencers_by_family.lock() {
            map.retain(|_, state| !state.entries.is_empty());
        }
        if let Ok(mut map) = self.side_effect_exec_locks.lock() {
            map.retain(|_, lock| Arc::strong_count(lock) <= 1);
        }
        if let Ok(mut map) = self.pending_rebase_original_head_by_worktree.lock() {
            map.shrink_to_fit();
        }
        if let Ok(mut map) = self.pending_cherry_pick_sources_by_worktree.lock() {
            map.retain(|_, sources| !sources.is_empty());
        }
        if let Ok(mut map) = self.pending_squash_merge_by_worktree.lock() {
            map.retain(|_, pending| {
                !pending.source_head.trim().is_empty() && !pending.onto.trim().is_empty()
            });
        }
        if let Ok(mut map) = self.queued_trace_payloads_by_root.lock() {
            map.retain(|_, count| *count > 0);
        }
        // Clean expired pending AI edit entries (older than 10s).
        {
            const PENDING_AI_EDIT_TIMEOUT_NS: u128 = 10_000_000_000;
            let gc_now_ns = now_unix_nanos();
            if let Ok(mut map) = self.pending_ai_edits_by_family.lock() {
                for family_map in map.values_mut() {
                    family_map.retain(|_, registered_at| {
                        gc_now_ns.saturating_sub(*registered_at) < PENDING_AI_EDIT_TIMEOUT_NS
                    });
                }
                map.retain(|_, family_map| !family_map.is_empty());
            }
        }
    }

    pub(crate) fn canonicalize_path(path: &str) -> String {
        crate::operations::git::canonicalize::canonicalize_or_self(std::path::Path::new(path))
            .to_string_lossy()
            .to_string()
    }

    pub(super) fn panic_message(panic_payload: Box<dyn std::any::Any + Send>) -> String {
        panic_payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic_payload.downcast_ref::<&str>().copied())
            .unwrap_or("unknown panic")
            .to_string()
    }

    pub(crate) fn register_pending_ai_edits(&self, family: &str, file_paths: &[String]) {
        let now_ns = now_unix_nanos();
        if let Ok(mut map) = self.pending_ai_edits_by_family.lock() {
            let family_map = map.entry(family.to_string()).or_default();
            for file in file_paths {
                family_map.insert(Self::canonicalize_path(file), now_ns);
            }
        }
    }

    pub(crate) fn clear_pending_ai_edits(&self, family: &str, file_paths: &[String]) {
        if let Ok(mut map) = self.pending_ai_edits_by_family.lock()
            && let Some(family_map) = map.get_mut(family)
        {
            for file in file_paths {
                family_map.remove(&Self::canonicalize_path(file));
            }
            if family_map.is_empty() {
                map.remove(family);
            }
        }
    }

    pub(crate) fn file_has_pending_ai_edit(&self, family: &str, file_path: &str) -> bool {
        const PENDING_AI_EDIT_TIMEOUT_NS: u128 = 10_000_000_000; // 10 seconds
        let now_ns = now_unix_nanos();
        let canonical = Self::canonicalize_path(file_path);
        if let Ok(map) = self.pending_ai_edits_by_family.lock()
            && let Some(family_map) = map.get(family)
        {
            return family_map.get(&canonical).is_some_and(|registered_at| {
                now_ns.saturating_sub(*registered_at) < PENDING_AI_EDIT_TIMEOUT_NS
            });
        }
        false
    }
}
