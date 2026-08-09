#[allow(unused_imports)]
use super::*;
use crate::error::GitAiError;
use crate::model::repository::error::PersistenceError;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
#[cfg(feature = "test-support")]
use std::time::Duration;
use tokio::sync::mpsc;

impl ActorDaemonCoordinator {
    pub(crate) fn trace_root_connection_opened(&self, root_sid: &str) -> Result<(), GitAiError> {
        let mut ingress =
            self.trace_ingress_state
                .lock()
                .map_err(|_| PersistenceError::LockPoisoned {
                    what: "trace ingress state",
                })?;
        *ingress
            .root_open_connections
            .entry(root_sid.to_string())
            .or_insert(0) += 1;
        Ok(())
    }

    pub(crate) fn trace_root_needs_close_marker(
        ingress: &TraceIngressState,
        root_sid: &str,
    ) -> bool {
        if ingress.root_definitely_read_only.contains(root_sid) {
            return false;
        }
        ingress
            .root_mutating
            .get(root_sid)
            .copied()
            .unwrap_or(false)
            || ingress.root_reflog_start_offsets.contains_key(root_sid)
    }

    pub(crate) fn clear_trace_ingress_root_locked(ingress: &mut TraceIngressState, root_sid: &str) {
        ingress.root_worktrees.remove(root_sid);
        ingress.root_families.remove(root_sid);
        ingress.root_argv.remove(root_sid);
        ingress.root_started_at_ns.remove(root_sid);
        ingress.root_reflog_start_offsets.remove(root_sid);
        ingress.root_mutating.remove(root_sid);
        ingress.root_target_repo_only.remove(root_sid);
        ingress.root_last_activity_ns.remove(root_sid);
        ingress.root_definitely_read_only.remove(root_sid);
        ingress.root_open_connections.remove(root_sid);
        ingress.root_close_markers_enqueued.remove(root_sid);
    }

    pub(crate) fn record_trace_connection_close(
        &self,
        roots: &[String],
    ) -> Result<Vec<String>, GitAiError> {
        let mut close_marker_candidates = Vec::new();
        let mut ingress =
            self.trace_ingress_state
                .lock()
                .map_err(|_| PersistenceError::LockPoisoned {
                    what: "trace ingress state",
                })?;
        for root_sid in roots {
            if let Some(count) = ingress.root_open_connections.get_mut(root_sid) {
                if *count > 1 {
                    *count -= 1;
                    continue;
                }
                ingress.root_open_connections.remove(root_sid);
            }
            if !Self::trace_root_needs_close_marker(&ingress, root_sid) {
                Self::clear_trace_ingress_root_locked(&mut ingress, root_sid);
                continue;
            }
            if ingress.root_close_markers_enqueued.contains(root_sid) {
                continue;
            }
            ingress.root_close_markers_enqueued.insert(root_sid.clone());
            close_marker_candidates.push(root_sid.clone());
        }
        self.trace_ingest_progress_notify.notify_waiters();
        Ok(close_marker_candidates)
    }

    pub(crate) fn enqueue_trace_connection_close_markers(
        &self,
        roots: Vec<String>,
    ) -> Result<(), GitAiError> {
        for root_sid in roots {
            self.enqueue_trace_payload(json!({
                "event": TRACE_CONNECTION_CLOSED_EVENT,
                "sid": root_sid,
                "time_ns": now_unix_nanos() as u64,
            }))?;
        }
        Ok(())
    }

    pub(crate) fn trace_unidentified_connection_opened(&self) -> Result<(), GitAiError> {
        let mut ingress =
            self.trace_ingress_state
                .lock()
                .map_err(|_| PersistenceError::LockPoisoned {
                    what: "trace ingress state",
                })?;
        ingress.unidentified_open_connections =
            ingress.unidentified_open_connections.saturating_add(1);
        self.trace_ingest_progress_notify.notify_waiters();
        Ok(())
    }

    pub(crate) fn trace_unidentified_connection_identified_or_closed(
        &self,
    ) -> Result<(), GitAiError> {
        let mut ingress =
            self.trace_ingress_state
                .lock()
                .map_err(|_| PersistenceError::LockPoisoned {
                    what: "trace ingress state",
                })?;
        ingress.unidentified_open_connections =
            ingress.unidentified_open_connections.saturating_sub(1);
        self.trace_ingest_progress_notify.notify_waiters();
        Ok(())
    }

    pub(crate) fn trace_payload_root_sid(payload: &Value) -> Option<String> {
        payload
            .get("sid")
            .and_then(Value::as_str)
            .map(|sid| trace_root_sid(sid).to_string())
    }

    pub(crate) fn record_trace_payload_enqueued(&self, payload: &Value) -> Result<(), GitAiError> {
        self.record_trace_payload_enqueued_root(Self::trace_payload_root_sid(payload).as_deref())
    }

    pub(crate) fn record_trace_payload_enqueued_root(
        &self,
        root_sid: Option<&str>,
    ) -> Result<(), GitAiError> {
        let Some(root_sid) = root_sid else {
            return Ok(());
        };
        let mut queued = self.queued_trace_payloads_by_root.lock().map_err(|_| {
            PersistenceError::LockPoisoned {
                what: "queued trace payloads by root",
            }
        })?;
        *queued.entry(root_sid.to_string()).or_insert(0) += 1;
        Ok(())
    }

    pub(crate) fn record_trace_payload_processed_root(
        &self,
        root_sid: Option<&str>,
    ) -> Result<(), GitAiError> {
        let Some(root_sid) = root_sid else {
            return Ok(());
        };
        let mut queued = self.queued_trace_payloads_by_root.lock().map_err(|_| {
            PersistenceError::LockPoisoned {
                what: "queued trace payloads by root",
            }
        })?;
        if let Some(count) = queued.get_mut(root_sid) {
            if *count > 1 {
                *count -= 1;
            } else {
                queued.remove(root_sid);
            }
        }
        Ok(())
    }

    pub(crate) fn clear_trace_root_tracking(&self, root_sid: &str) -> Result<(), GitAiError> {
        {
            let mut ingress =
                self.trace_ingress_state
                    .lock()
                    .map_err(|_| PersistenceError::LockPoisoned {
                        what: "trace ingress state",
                    })?;
            Self::clear_trace_ingress_root_locked(&mut ingress, root_sid);
        }
        let mut queued = self.queued_trace_payloads_by_root.lock().map_err(|_| {
            PersistenceError::LockPoisoned {
                what: "queued trace payloads by root",
            }
        })?;
        queued.remove(root_sid);
        self.trace_ingest_progress_notify.notify_waiters();
        Ok(())
    }

    pub(crate) fn has_open_trace_roots_that_may_mutate_refs(&self) -> bool {
        let Ok(ingress) = self.trace_ingress_state.lock() else {
            return false;
        };
        ingress.root_open_connections.iter().any(|(root, count)| {
            *count > 0
                && !ingress.root_definitely_read_only.contains(root)
                && ingress.root_mutating.get(root).copied().unwrap_or(true)
        })
    }

    pub(crate) fn next_trace_ingest_seq(&self) -> u64 {
        // Relaxed: we only need fetch_add atomicity (unique monotone values),
        // not ordering w.r.t. any other atomic.
        (self.next_trace_ingest_seq.fetch_add(1, Ordering::Relaxed) as u64) + 1
    }

    pub(crate) fn trace_ingest_queue_capacity() -> usize {
        #[cfg(feature = "test-support")]
        if let Ok(raw) = std::env::var("GIT_AI_TEST_TRACE_INGEST_QUEUE_CAPACITY")
            && let Ok(capacity) = raw.parse::<usize>()
            && capacity > 0
        {
            return capacity;
        }

        TRACE_INGEST_QUEUE_CAPACITY
    }

    pub(crate) fn start_trace_ingest_worker(self: &Arc<Self>) -> Result<(), GitAiError> {
        // Idempotent: if OnceLock is already set, worker is already running.
        if self.trace_ingest_tx.get().is_some() {
            return Ok(());
        }

        let queue_capacity = Self::trace_ingest_queue_capacity();
        let (tx, mut rx) = mpsc::channel::<Value>(queue_capacity);
        // OnceLock::set fails if another thread raced us to initialize — that
        // means the worker is already running; just drop our channel ends.
        if self.trace_ingest_tx.set(tx).is_err() {
            return Ok(());
        }

        let coordinator = self.clone();
        tokio::spawn(async move {
            #[cfg(feature = "test-support")]
            if let Ok(raw_delay_ms) =
                std::env::var("GIT_AI_TEST_TRACE_INGEST_WORKER_START_DELAY_MS")
                && let Ok(delay_ms) = raw_delay_ms.parse::<u64>()
                && delay_ms > 0
            {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }

            let mut next_seq: u64 = 1;
            let mut pending_by_seq: BTreeMap<u64, Value> = BTreeMap::new();
            let mut gc_counter: u64 = 0;
            const GC_INTERVAL: u64 = 500;

            // Previously: `while let Some(payload) = rx.recv().await { … }`
            //
            // The ingest worker used to exit when the sender was dropped by
            // `request_shutdown`.  With OnceLock the sender is never dropped
            // during the coordinator's lifetime, so we use select! to also
            // respond to the explicit shutdown signal.
            loop {
                let payload = tokio::select! {
                    biased; // prefer draining queued work over shutdown
                    maybe = rx.recv() => match maybe {
                        Some(p) => p,
                        None => break, // channel closed (coordinator dropped)
                    },
                    _ = coordinator.wait_for_shutdown() => break,
                };
                let Some(seq) = payload.get(TRACE_INGEST_SEQ_FIELD).and_then(Value::as_u64) else {
                    tracing::error!(
                        component = "daemon",
                        phase = "trace_ingest_worker",
                        reason = "missing_ingest_seq",
                        "trace ingest payload missing ingress sequence"
                    );
                    coordinator.request_shutdown();
                    break;
                };

                if pending_by_seq.len() >= queue_capacity {
                    tracing::error!(
                        component = "daemon",
                        phase = "trace_ingest_worker",
                        reason = "reorder_buffer_overflow",
                        buffered_count = pending_by_seq.len(),
                        next_seq,
                        received_seq = seq,
                        "trace ingest reorder buffer overflow"
                    );
                    coordinator.request_shutdown();
                    break;
                }

                if pending_by_seq.insert(seq, payload).is_some() {
                    tracing::error!(
                        component = "daemon",
                        phase = "trace_ingest_worker",
                        reason = "duplicate_ingest_seq",
                        sequence = seq,
                        "duplicate trace ingest sequence received"
                    );
                    coordinator.request_shutdown();
                    break;
                }

                while let Some(mut ordered_payload) = pending_by_seq.remove(&next_seq) {
                    let processed_seq = next_seq;
                    if let Some(object) = ordered_payload.as_object_mut() {
                        object.remove(TRACE_INGEST_SEQ_FIELD);
                    }
                    let ordered_payload_root = Self::trace_payload_root_sid(&ordered_payload);

                    let ingest_result = {
                        let coord = coordinator.clone();
                        let future = coord.ingest_trace_payload_fast(ordered_payload);
                        let caught = std::panic::AssertUnwindSafe(future);
                        match futures::FutureExt::catch_unwind(caught).await {
                            Ok(Ok(())) => Ok(()),
                            Ok(Err(error)) => {
                                tracing::error!(
                                    component = "daemon",
                                    phase = "trace_ingest_worker",
                                    reason = "ingest_error",
                                    sequence = processed_seq,
                                    root_sid = ?ordered_payload_root,
                                    %error,
                                    "trace ingest error"
                                );
                                Err(error)
                            }
                            Err(panic_payload) => {
                                let panic_msg = Self::panic_message(panic_payload);
                                tracing::error!(
                                    component = "daemon",
                                    phase = "trace_ingest_worker",
                                    reason = "panic_in_ingest",
                                    panic_msg = %panic_msg,
                                    sequence = processed_seq,
                                    "trace ingest panic"
                                );
                                // This exact panic text is part of the trace catch-boundary contract.
                                Err(GitAiError::Generic(format!(
                                    "trace ingest worker panic: {}",
                                    panic_msg
                                )))
                            }
                        }
                    };
                    let _ = ingest_result;
                    let _ = coordinator.queued_trace_payloads.fetch_update(
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                        |current| Some(current.saturating_sub(1)),
                    );
                    if let Err(error) = coordinator
                        .record_trace_payload_processed_root(ordered_payload_root.as_deref())
                    {
                        tracing::debug!(
                            %error,
                            "trace payload accounting error after ingest"
                        );
                    }
                    // Release: pairs with Acquire loads in wait_for_trace_ingest_processed_through
                    // so waiters observe all ingest side-effects when seq advances.
                    coordinator
                        .processed_trace_ingest_seq
                        .store(processed_seq as usize, Ordering::Release);
                    coordinator.trace_ingest_progress_notify.notify_waiters();
                    next_seq = next_seq.saturating_add(1);
                    gc_counter += 1;
                    if gc_counter.is_multiple_of(GC_INTERVAL) {
                        coordinator.gc_stale_family_state();
                    }
                }
            }

            if !pending_by_seq.is_empty() {
                tracing::error!(
                    component = "daemon",
                    phase = "trace_ingest_worker",
                    reason = "unflushed_buffer_on_shutdown",
                    buffered_count = pending_by_seq.len(),
                    next_seq,
                    min_buffered_seq = ?pending_by_seq.keys().next().copied(),
                    max_buffered_seq = ?pending_by_seq.keys().last().copied(),
                    "trace ingest worker exiting with buffered out-of-order frames"
                );
            }
        });
        Ok(())
    }
}
