#[allow(unused_imports)]
use super::*;
use crate::error::GitAiError;
use crate::model::repository::error::PersistenceError;
use crate::operations::git::repo_state::common_dir_for_worktree;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex as AsyncMutex;

impl ActorDaemonCoordinator {
    pub(crate) fn enqueue_trace_payload(&self, payload: Value) -> Result<(), GitAiError> {
        let tx =
            self.trace_ingest_tx.get().cloned().ok_or_else(|| {
                GitAiError::Generic("trace ingest worker not started".to_string())
            })?;
        let permit = match tx.try_reserve() {
            Ok(permit) => permit,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(())) => {
                tracing::error!(
                    component = "daemon",
                    phase = "enqueue_trace_payload",
                    reason = "ingest_worker_channel_closed",
                    "trace ingest queue send failed: worker may have crashed"
                );
                self.request_shutdown();
                return Err(GitAiError::Generic(
                    "trace ingest queue send failed: worker may have crashed".to_string(),
                ));
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(())) => {
                tracing::error!(
                    component = "daemon",
                    phase = "enqueue_trace_payload",
                    reason = "ingest_worker_queue_full",
                    "trace ingest queue is full"
                );
                self.request_shutdown();
                return Err(GitAiError::Generic(
                    "trace ingest queue is full; daemon shutting down".to_string(),
                ));
            }
        };
        self.record_trace_payload_enqueued(&payload)?;
        let mut payload = payload;
        if let Some(object) = payload.as_object_mut()
            && object.get(TRACE_INGEST_SEQ_FIELD).is_none()
        {
            object.insert(
                TRACE_INGEST_SEQ_FIELD.to_string(),
                json!(self.next_trace_ingest_seq()),
            );
        }
        // Relaxed: this counter tracks in-flight count for monitoring; no
        // ordering dependency with any other atomic.
        self.queued_trace_payloads.fetch_add(1, Ordering::Relaxed);
        permit.send(payload);
        Ok(())
    }

    /// Waits until all trace payloads enqueued up to now have been processed
    /// by the ingest worker, and any identified trace root that may mutate refs
    /// has closed. This is a causal drain fence: it guarantees that trace2 data
    /// already visible to the daemon for prior mutating git operations has
    /// reached the family sequencer before returning.
    ///
    /// Accepted sockets with no complete trace2 root are not causal evidence for
    /// any repository family. They are tracked for connection cleanup, but must
    /// not globally block checkpoint/sync control requests.
    ///
    /// Used by checkpoint entry to ensure ordering: a checkpoint must not be
    /// processed until all causally-prior git operations have been ingested
    /// through their root `atexit`/connection-close boundary.
    pub(crate) async fn wait_for_trace_ingest_processed_through(&self) {
        self.wait_for_trace_ingest_drained(|coordinator| {
            coordinator.has_open_trace_roots_that_may_mutate_refs()
        })
        .await
    }

    /// Family-scoped drain fence for sync requests: waits for the ingest
    /// worker to catch up, but only blocks on open mutating roots that could
    /// affect `family` (unattributed roots fail closed). A long-running
    /// mutating command in one repository must not stall syncs of unrelated
    /// repositories served by the same daemon.
    pub(crate) async fn wait_for_trace_ingest_processed_through_family(&self, family: &str) {
        self.wait_for_trace_ingest_drained(|coordinator| {
            coordinator.has_open_trace_roots_that_may_mutate_family(family)
        })
        .await
    }

    async fn wait_for_trace_ingest_drained(&self, has_blocking_open_roots: impl Fn(&Self) -> bool) {
        loop {
            // Read the current high-water mark. Any payload enqueued before this
            // point has a seq <= this value. We need to wait until the ingest
            // worker has processed through at least this seq.
            let target = self.next_trace_ingest_seq.load(Ordering::Acquire) as u64;
            loop {
                let processed = self.processed_trace_ingest_seq.load(Ordering::Acquire) as u64;
                if processed >= target {
                    break;
                }
                let progress = self.trace_ingest_progress_notify.notified();
                tokio::select! {
                    _ = progress => {}
                    _ = self.wait_for_shutdown() => return,
                }
            }

            if !has_blocking_open_roots(self) {
                return;
            }

            let progress = self.trace_ingest_progress_notify.notified();
            if !has_blocking_open_roots(self) {
                return;
            }
            tokio::select! {
                _ = progress => {}
                _ = self.wait_for_shutdown() => return,
            }
        }
    }

    /// Prepares `payload` for ingestion and returns whether it should be
    /// enqueued.
    ///
    /// - `true`  — payload is for a mutating command; the caller MUST call
    ///   `enqueue_trace_payload`.
    /// - `false` — payload is for a definitely-read-only invocation; it was
    ///   handled inline and the caller MUST NOT enqueue it.
    ///
    /// Sequence numbers are allocated only after `enqueue_trace_payload` has
    /// reserved queue capacity, so the `processed_trace_ingest_seq` watermark
    /// used by checkpoint drain waits advances without unqueued gaps.
    pub(crate) fn prepare_trace_payload_for_ingest(&self, payload: &mut Value) -> bool {
        // Check read-only status BEFORE allocating a sequence number so that
        // read-only invocations never perturb the ingest sequence counter.
        let is_read_only = self.track_trace_payload_for_ingest(payload);
        if is_read_only {
            return false;
        }
        true
    }

    /// Tracks trace2 root metadata needed for ordering and read-only fast paths.
    /// This deliberately does not read mutable repository state or inject
    /// daemon-derived repository/ref snapshots into the trace payload.
    pub(crate) fn track_trace_payload_for_ingest(&self, payload: &mut Value) -> bool {
        let event = payload
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let sid = payload
            .get("sid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if sid.is_empty() {
            return false;
        }

        let root = trace_root_sid(&sid).to_string();
        let argv = trace_payload_argv(payload);
        let worktree_hint = trace_payload_worktree_hint(payload);
        let started_at_ns = trace_payload_time_ns(payload);
        let early_primary =
            trace_payload_primary_command(payload).or_else(|| trace_argv_primary_command(&argv));
        let event_is_read_only =
            trace_invocation_is_definitely_read_only(early_primary.as_deref(), &argv);

        let mut ingress = match self.trace_ingress_state.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        ingress
            .root_last_activity_ns
            .insert(root.clone(), now_unix_nanos() as u64);

        if event == "start" && sid == root {
            let started_at_ns = started_at_ns.unwrap_or_else(now_unix_nanos);
            ingress
                .root_started_at_ns
                .entry(root.clone())
                .or_insert(started_at_ns);
        }

        if let Some(worktree) = worktree_hint.clone() {
            if let Some(common_dir) = common_dir_for_worktree(&worktree) {
                let family =
                    crate::operations::git::canonicalize::canonicalize_or_self(&common_dir);
                ingress
                    .root_families
                    .insert(root.clone(), family.to_string_lossy().to_string());
            }
            ingress.root_worktrees.insert(root.clone(), worktree);
        }

        if event == "start" && sid == root && !argv.is_empty() {
            ingress.root_argv.insert(root.clone(), argv.clone());
            if event_is_read_only {
                ingress.root_definitely_read_only.insert(root.clone());
            }
        }

        let effective_argv = if argv.is_empty() {
            ingress.root_argv.get(&root).cloned().unwrap_or_default()
        } else {
            argv
        };
        let effective_primary =
            early_primary.or_else(|| trace_argv_primary_command(&effective_argv));
        let command_mutates_refs =
            trace_invocation_may_mutate_refs(effective_primary.as_deref(), &effective_argv);
        if let Some(primary) = effective_primary.as_deref() {
            ingress
                .root_mutating
                .entry(root.clone())
                .or_insert(command_mutates_refs);
            let target_repo_only = trace_command_uses_target_repo_context_only(Some(primary));
            ingress
                .root_target_repo_only
                .entry(root.clone())
                .or_insert(target_repo_only);
        }

        let terminal = is_terminal_root_trace_event(&event, &sid, &root);
        if command_mutates_refs
            && !terminal
            && !ingress.root_reflog_start_offsets.contains_key(&root)
            && let Some(worktree) = worktree_hint
                .clone()
                .or_else(|| ingress.root_worktrees.get(&root).cloned())
        {
            let offsets =
                crate::operations::daemon::ref_cursor::capture_reflog_start_offsets_for_worktree(
                    &worktree,
                );
            ingress
                .root_reflog_start_offsets
                .insert(root.clone(), offsets);
        }

        let read_only_root =
            event_is_read_only || ingress.root_definitely_read_only.contains(&root);
        let inherited = (
            ingress.root_argv.get(&root).cloned(),
            ingress.root_started_at_ns.get(&root).copied(),
            ingress.root_reflog_start_offsets.get(&root).cloned(),
            ingress.root_worktrees.get(&root).cloned(),
        );
        if terminal {
            ingress.root_worktrees.remove(&root);
            ingress.root_families.remove(&root);
            ingress.root_argv.remove(&root);
            ingress.root_started_at_ns.remove(&root);
            ingress.root_reflog_start_offsets.remove(&root);
            ingress.root_mutating.remove(&root);
            ingress.root_target_repo_only.remove(&root);
            ingress.root_last_activity_ns.remove(&root);
            ingress.root_definitely_read_only.remove(&root);
        }

        drop(ingress);

        if let Some(object) = payload.as_object_mut() {
            if object.get("argv").is_none()
                && let Some(root_argv) = inherited.0
            {
                object.insert(TRACE_ROOT_ARGV_FIELD.to_string(), json!(root_argv));
            }
            if object.get(TRACE_ROOT_STARTED_AT_NS_FIELD).is_none()
                && let Some(started_at_ns) = inherited.1
            {
                let started_at_ns = u64::try_from(started_at_ns).unwrap_or(u64::MAX);
                object.insert(
                    TRACE_ROOT_STARTED_AT_NS_FIELD.to_string(),
                    json!(started_at_ns),
                );
            }
            if object.get(TRACE_ROOT_REFLOG_START_OFFSETS_FIELD).is_none()
                && let Some(offsets) = inherited.2
            {
                object.insert(
                    TRACE_ROOT_REFLOG_START_OFFSETS_FIELD.to_string(),
                    json!(offsets),
                );
            }
            if object.get(TRACE_ROOT_WORKTREE_FIELD).is_none()
                && object.get("worktree").is_none()
                && object.get("repo_working_dir").is_none()
                && let Some(worktree) = inherited.3
            {
                object.insert(
                    TRACE_ROOT_WORKTREE_FIELD.to_string(),
                    json!(worktree.to_string_lossy().to_string()),
                );
            }
        }

        read_only_root
    }

    pub(crate) fn side_effect_exec_lock(
        &self,
        family: &str,
    ) -> Result<Arc<AsyncMutex<()>>, GitAiError> {
        let mut map =
            self.side_effect_exec_locks
                .lock()
                .map_err(|_| PersistenceError::LockPoisoned {
                    what: "side effect lock map",
                })?;
        Ok(map
            .entry(family.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone())
    }
}
