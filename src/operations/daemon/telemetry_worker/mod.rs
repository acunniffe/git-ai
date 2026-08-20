//! Daemon-side telemetry worker that batches and dispatches events.
//!
//! Runs inside the daemon process using tokio. Accumulates telemetry envelopes
//! and CAS payloads, then flushes them to their destinations every 3 seconds.

mod buffer;
mod cas_flush;
mod daemon_log_upload;
mod metrics_flush;
mod notes_flush;
mod sentry_posthog;

use crate::config::get_or_create_distinct_id;
use crate::error::GitAiError;
use crate::metrics::MetricEvent;
use crate::model::api_types::DaemonLogEvent;
use crate::model::daemon_control::CasSyncPayload;
use crate::model::telemetry::TelemetryEnvelope;
use buffer::TelemetryBuffer;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, sleep_until};

use cas_flush::flush_cas;
use daemon_log_upload::{
    DAEMON_LOG_HEARTBEAT_INTERVAL, daemon_heartbeat_event, daemon_log_upload_enabled,
    daemon_run_id, dispatch_daemon_log_upload,
};
pub(crate) use daemon_log_upload::{EmergencyLogUploadStatus, upload_emergency_daemon_log};
use metrics_flush::{
    METRICS_UPLOAD_AVAILABLE, count_pending_metrics_for_await, flush_metrics,
    flush_pending_metrics, metrics_store, spawn_metrics_metadata_backfill,
    store_metrics_in_db_with,
};
use notes_flush::{flush_notes_for_await, flush_notes_with};
use sentry_posthog::flush_sentry_and_posthog;

// Re-export for `telemetry_worker_tests.rs` (accessed via `super::telemetry_worker::flush_pending_metric_records_with`).
#[cfg(test)]
pub(super) use metrics_flush::flush_pending_metric_records_with;
// Re-export for `actor_coordinator_control.rs` (accessed via `crate::operations::daemon::telemetry_worker::flush_notes_global`).
pub use notes_flush::flush_notes_global;

const FLUSH_INTERVAL: Duration = Duration::from_secs(3);

// Type aliases for the static mutex handles used throughout this module and its sub-modules.
type MetricsDbHandle =
    &'static std::sync::Mutex<crate::model::repository::metrics_db::MetricsDatabase>;
type NotesDbHandle = &'static std::sync::Mutex<crate::model::repository::notes_db::NotesDatabase>;
type InternalDbHandle =
    &'static std::sync::Mutex<crate::model::repository::internal_db::InternalDatabase>;

/// Result of a telemetry flush cycle.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FlushStatus {
    /// Approximate buffered + pending metrics still awaiting upload.
    pub metrics_remaining: usize,
    /// Approximate number of notes still eligible for upload.
    pub notes_remaining: usize,
}

struct FlushRequest {
    completion: tokio::sync::oneshot::Sender<FlushStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlushMode {
    Periodic,
    Await,
}

/// Resolved store handles passed to `spawn_telemetry_worker`.
///
/// Constructed once in `run_daemon` so the flush loop never calls `::global()`.
/// `Copy` because all fields are `'static` references.
#[derive(Clone, Copy)]
pub struct TelemetryStores {
    pub metrics: MetricsDbHandle,
    pub notes: NotesDbHandle,
    pub internal: InternalDbHandle,
}

/// Handle for submitting telemetry directly within the daemon process.
#[derive(Clone)]
pub struct DaemonTelemetryWorkerHandle {
    buffer: Arc<Mutex<TelemetryBuffer>>,
    flush_tx: tokio::sync::mpsc::UnboundedSender<FlushRequest>,
    // None only for new_noop() in tests; production always sets Some.
    stores: Option<TelemetryStores>,
}

impl DaemonTelemetryWorkerHandle {
    #[cfg(test)]
    pub fn new_noop() -> Self {
        let (flush_tx, _flush_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            buffer: Arc::new(Mutex::new(TelemetryBuffer::new())),
            flush_tx,
            stores: None,
        }
    }

    /// Submit telemetry envelopes for batched processing.
    pub async fn submit_telemetry(&self, envelopes: Vec<TelemetryEnvelope>) {
        let (buffered_envelopes, metric_events) = split_metric_envelopes(envelopes);
        if !buffered_envelopes.is_empty() {
            self.buffer
                .lock()
                .await
                .ingest_envelopes(buffered_envelopes);
        }

        if !metric_events.is_empty() {
            let stores = self.stores;
            std::mem::drop(tokio::task::spawn_blocking(move || {
                if let Err(e) = store_metrics_in_db_with(metrics_store(stores), &metric_events) {
                    tracing::warn!(%e, "telemetry: failed to persist metrics locally");
                }
            }));
        }
    }

    /// Submit CAS records for batched upload.
    pub async fn submit_cas(&self, records: Vec<CasSyncPayload>) {
        self.buffer.lock().await.ingest_cas(records);
    }

    /// Submit daemon diagnostic events for batched upload.
    pub async fn submit_daemon_logs(&self, events: Vec<DaemonLogEvent>) {
        if events.is_empty() {
            return;
        }
        self.buffer.lock().await.ingest_daemon_logs(events);
    }

    /// Request an immediate telemetry flush and wait for the worker to complete.
    pub async fn flush_and_wait(&self) -> Result<FlushStatus, GitAiError> {
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        self.flush_tx
            .send(FlushRequest {
                completion: completion_tx,
            })
            .map_err(|_| GitAiError::Generic("telemetry worker has stopped".to_string()))?;
        completion_rx
            .await
            .map_err(|_| GitAiError::Generic("telemetry flush was cancelled".to_string()))
    }

    /// Returns the current number of metrics waiting for upload.
    ///
    /// Used by the transcript worker for backpressure: if SQLite pending rows
    /// or the legacy in-memory buffer are above a threshold, the worker yields
    /// to let the flush loop drain them. Returns `usize::MAX` when the buffer
    /// lock is contended, so callers default to "wait" rather than "push more".
    pub fn metrics_buffer_len(&self) -> usize {
        let buffered = self
            .buffer
            .try_lock()
            .map(|buf| buf.metrics.len())
            .unwrap_or(usize::MAX);
        if buffered == usize::MAX {
            return usize::MAX;
        }

        if !METRICS_UPLOAD_AVAILABLE.load(std::sync::atomic::Ordering::Relaxed) {
            return buffered;
        }

        let pending = match metrics_store(self.stores) {
            Ok(db) => match db.try_lock() {
                Ok(db) => db.count_retryable().unwrap_or(usize::MAX),
                Err(_) => usize::MAX,
            },
            Err(_) => 0,
        };
        buffered.saturating_add(pending)
    }

    /// Persist metrics directly from an existing blocking worker.
    ///
    /// Transcript sweeps can emit many batches in a tight loop. Routing those
    /// through the async telemetry entrypoint creates one fire-and-forget
    /// `spawn_blocking` task per batch, so a fast producer can retain many raw
    /// transcript events while SQLite writes catch up. This path keeps the
    /// producer coupled to the metrics DB write and bounds peak memory.
    pub fn persist_metrics_blocking(&self, events: &[MetricEvent]) -> Result<Vec<i64>, GitAiError> {
        store_metrics_in_db_with(metrics_store(self.stores), events)
    }

    /// Flush pending notes synchronously (for the FlushNotes control-request arm).
    pub fn flush_notes_sync(&self) {
        flush_notes_with(self.stores);
    }

    /// Submit telemetry envelopes synchronously (best-effort, non-blocking).
    ///
    /// Used by the daemon process's own `observability::log_*()` calls which
    /// cannot go through the control socket (the daemon can't connect to itself).
    /// Uses `try_lock()` to avoid blocking the caller if the buffer is contested.
    pub fn submit_telemetry_sync(&self, envelopes: Vec<TelemetryEnvelope>) {
        let (buffered_envelopes, metric_events) = split_metric_envelopes(envelopes);
        if !buffered_envelopes.is_empty()
            && let Ok(mut buf) = self.buffer.try_lock()
        {
            buf.ingest_envelopes(buffered_envelopes);
        }

        if !metric_events.is_empty()
            && let Err(e) = store_metrics_in_db_with(metrics_store(self.stores), &metric_events)
        {
            tracing::warn!(%e, "telemetry: failed to persist daemon metrics locally");
        }
    }

    /// Submit CAS records synchronously (best-effort, non-blocking).
    ///
    /// Used by daemon-owned post-commit paths that cannot route through the
    /// control socket because the daemon cannot connect to itself.
    pub fn submit_cas_sync(&self, records: Vec<CasSyncPayload>) {
        if let Ok(mut buf) = self.buffer.try_lock() {
            buf.ingest_cas(records);
        }
    }

    /// Submit daemon diagnostic events synchronously (best-effort, non-blocking).
    pub fn submit_daemon_logs_sync(&self, events: Vec<DaemonLogEvent>) {
        if events.is_empty() {
            return;
        }
        if let Ok(mut buf) = self.buffer.try_lock() {
            buf.ingest_daemon_logs(events);
        }
    }
}

/// Global handle for the daemon's in-process telemetry worker.
///
/// Set once when the daemon spawns its telemetry worker, allowing
/// `observability::log_*()` functions to route events directly into
/// the worker buffer when running inside the daemon process.
static DAEMON_INTERNAL_TELEMETRY: std::sync::OnceLock<DaemonTelemetryWorkerHandle> =
    std::sync::OnceLock::new();

/// Register the daemon's in-process telemetry worker handle.
/// Called once during daemon startup after `spawn_telemetry_worker()`.
pub fn set_daemon_internal_telemetry(handle: DaemonTelemetryWorkerHandle) {
    let _ = DAEMON_INTERNAL_TELEMETRY.set(handle);
}

/// Submit telemetry from within the daemon process.
/// Returns true if the handle was available and envelopes were submitted.
pub fn submit_daemon_internal_telemetry(envelopes: Vec<TelemetryEnvelope>) -> bool {
    if let Some(handle) = DAEMON_INTERNAL_TELEMETRY.get() {
        submit_daemon_internal_telemetry_with_handle(handle.clone(), envelopes);
        true
    } else {
        false
    }
}

fn submit_daemon_internal_telemetry_with_handle(
    handle: DaemonTelemetryWorkerHandle,
    envelopes: Vec<TelemetryEnvelope>,
) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            handle.submit_telemetry(envelopes).await;
        });
    } else {
        handle.submit_telemetry_sync(envelopes);
    }
}

fn split_metric_envelopes(
    envelopes: Vec<TelemetryEnvelope>,
) -> (Vec<TelemetryEnvelope>, Vec<MetricEvent>) {
    let mut buffered_envelopes = Vec::new();
    let mut metric_events = Vec::new();

    for envelope in envelopes {
        match envelope {
            TelemetryEnvelope::Metrics { events } => metric_events.extend(events),
            other => buffered_envelopes.push(other),
        }
    }

    (buffered_envelopes, metric_events)
}

/// Submit CAS records from within the daemon process (sync, best-effort).
/// Returns true if the handle was available and records were submitted.
pub fn submit_daemon_internal_cas(records: Vec<CasSyncPayload>) -> bool {
    if let Some(handle) = DAEMON_INTERNAL_TELEMETRY.get() {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let handle = handle.clone();
            runtime.spawn(async move {
                handle.submit_cas(records).await;
            });
        } else {
            handle.submit_cas_sync(records);
        }
        true
    } else {
        false
    }
}

/// Submit daemon diagnostic events from within the daemon process.
/// Returns true if the handle was available and events were submitted.
pub fn submit_daemon_internal_daemon_logs(events: Vec<DaemonLogEvent>) -> bool {
    if let Some(handle) = DAEMON_INTERNAL_TELEMETRY.get() {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let handle = handle.clone();
            runtime.spawn(async move {
                handle.submit_daemon_logs(events).await;
            });
        } else {
            handle.submit_daemon_logs_sync(events);
        }
        true
    } else {
        false
    }
}

/// Spawn the telemetry worker task. Returns a handle for submitting events.
///
/// The worker flushes every 3 seconds. `stores` is resolved once by the caller
/// so the flush loop never calls `::global()` at runtime.
pub fn spawn_telemetry_worker(stores: TelemetryStores) -> DaemonTelemetryWorkerHandle {
    let buffer = Arc::new(Mutex::new(TelemetryBuffer::new()));
    let (flush_tx, flush_rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = DaemonTelemetryWorkerHandle {
        buffer: buffer.clone(),
        flush_tx,
        stores: Some(stores),
    };
    let daemon_id = daemon_run_id().to_string();

    spawn_metrics_metadata_backfill(stores);

    tokio::spawn(async move {
        telemetry_flush_loop(buffer, daemon_id, flush_rx, stores).await;
    });

    handle
}

async fn telemetry_flush_loop(
    buffer: Arc<Mutex<TelemetryBuffer>>,
    daemon_id: String,
    mut flush_rx: tokio::sync::mpsc::UnboundedReceiver<FlushRequest>,
    stores: TelemetryStores,
) {
    let started_at = std::time::Instant::now();
    let mut next_heartbeat_at = started_at + DAEMON_LOG_HEARTBEAT_INTERVAL;
    let mut flush_requests: Vec<FlushRequest> = Vec::new();

    loop {
        tokio::select! {
            _ = sleep_until(next_telemetry_flush_at(Instant::now())) => {}
            Some(request) = flush_rx.recv() => {
                flush_requests.push(request);
            }
        }

        let now = std::time::Instant::now();
        let heartbeat = if now >= next_heartbeat_at && daemon_log_upload_enabled() {
            while next_heartbeat_at <= now {
                next_heartbeat_at += DAEMON_LOG_HEARTBEAT_INTERVAL;
            }
            Some(daemon_heartbeat_event(started_at.elapsed()))
        } else {
            None
        };

        let flush_mode = if flush_requests.is_empty() {
            FlushMode::Periodic
        } else {
            FlushMode::Await
        };
        let snapshot = {
            let mut buf = buffer.lock().await;
            if let Some(event) = heartbeat {
                buf.ingest_daemon_logs(vec![event]);
            }
            take_telemetry_flush_snapshot(&mut buf, flush_mode)
        };

        // Flush in a blocking task since the underlying HTTP clients are synchronous.
        let daemon_id_for_flush = daemon_id.clone();
        let flush_started_at = std::time::Instant::now();
        let flush_result = tokio::task::spawn_blocking(move || {
            let requeue_daemon_logs = if let Some(snapshot) = snapshot {
                flush_telemetry_batch(snapshot, &daemon_id_for_flush, stores)
            } else {
                flush_pending_metrics(stores);
                Vec::new()
            };
            let await_status = collect_await_flush_status(flush_mode, stores);
            (requeue_daemon_logs, await_status)
        })
        .await
        .unwrap_or_else(|e| {
            tracing::error!(%e, "telemetry flush task panicked");
            (Vec::new(), None)
        });
        let flush_elapsed = flush_started_at.elapsed();
        if flush_elapsed > FLUSH_INTERVAL {
            tracing::warn!(
                elapsed_ms = flush_elapsed.as_millis(),
                interval_ms = FLUSH_INTERVAL.as_millis(),
                "telemetry flush exceeded its scheduling interval"
            );
        }

        let (requeue_daemon_logs, await_status) = flush_result;

        for request in flush_requests.drain(..) {
            let _ = request.completion.send(await_status.unwrap_or_default());
        }

        if !requeue_daemon_logs.is_empty() {
            buffer
                .lock()
                .await
                .requeue_failed_daemon_logs(requeue_daemon_logs);
        }
    }
}

fn take_telemetry_flush_snapshot(
    buffer: &mut TelemetryBuffer,
    flush_mode: FlushMode,
) -> Option<TelemetryBuffer> {
    if flush_mode == FlushMode::Periodic && buffer.is_empty() {
        None
    } else {
        Some(buffer.take())
    }
}

fn next_telemetry_flush_at(completed_at: Instant) -> Instant {
    completed_at + FLUSH_INTERVAL
}

fn flush_telemetry_batch(
    batch: TelemetryBuffer,
    daemon_id: &str,
    stores: TelemetryStores,
) -> Vec<DaemonLogEvent> {
    let config = crate::config::Config::get();
    let distinct_id = get_or_create_distinct_id();

    // Flush metrics (always processed — uploaded or stored in SQLite)
    if !batch.metrics.is_empty() {
        flush_metrics(&batch.metrics, stores);
    }

    // Flush Sentry events (errors, performance, messages)
    let has_sentry_or_posthog =
        !batch.errors.is_empty() || !batch.performances.is_empty() || !batch.messages.is_empty();

    if has_sentry_or_posthog {
        flush_sentry_and_posthog(
            config,
            &distinct_id,
            &batch.errors,
            &batch.performances,
            &batch.messages,
        );
    }

    // Flush CAS records
    if !batch.cas_records.is_empty() {
        flush_cas(batch.cas_records, stores);
    }

    // Flush pending notes (reads directly from notes-db; no-op when kind != Http).
    flush_notes_with(Some(stores));

    flush_pending_metrics(stores);

    if batch.daemon_logs.is_empty() {
        Vec::new()
    } else {
        dispatch_daemon_log_upload(batch.daemon_logs, daemon_id, &distinct_id)
    }
}

fn collect_await_flush_status(
    flush_mode: FlushMode,
    stores: TelemetryStores,
) -> Option<FlushStatus> {
    collect_await_flush_status_with(
        flush_mode,
        || flush_notes_for_await(stores),
        || count_pending_metrics_for_await(stores),
    )
}

fn collect_await_flush_status_with<Notes, Metrics>(
    flush_mode: FlushMode,
    flush_notes: Notes,
    count_metrics: Metrics,
) -> Option<FlushStatus>
where
    Notes: FnOnce() -> usize,
    Metrics: FnOnce() -> usize,
{
    if flush_mode == FlushMode::Periodic {
        return None;
    }

    Some(FlushStatus {
        metrics_remaining: count_metrics(),
        notes_remaining: flush_notes(),
    })
}

#[cfg(test)]
mod worker_tests;
