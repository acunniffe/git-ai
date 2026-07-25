//! Daemon-side telemetry worker that batches and dispatches events.
//!
//! Runs inside the daemon process using tokio. Accumulates telemetry envelopes
//! and CAS payloads, then flushes them to their destinations every 3 seconds.

use crate::api::logs::daemon_logs_upload_allowed;
use crate::api::metrics::{MetricsUploadResponse, metrics_upload_allowed};
use crate::api::types::{
    DAEMON_LOGS_UPLOAD_VERSION, DaemonLogEvent, DaemonLogFieldValue, DaemonLogKind, DaemonLogLevel,
    DaemonLogsUploadRequest,
};
use crate::api::{ApiClient, ApiContext, CasObject, CasUploadRequest};
use crate::authorship::authorship_log_serialization::GIT_AI_VERSION;
use crate::config::{Config, get_or_create_distinct_id};
use crate::daemon::control_api::{CasSyncPayload, TelemetryEnvelope};
use crate::error::GitAiError;
use crate::metrics::db::{METADATA_BACKFILL_BATCH_SIZE, MetricRecord, MetricsDatabase};
use crate::metrics::{MetricEvent, MetricsBatch};
use crate::observability::MAX_METRICS_PER_ENVELOPE;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, sleep_until};

const FLUSH_INTERVAL: Duration = Duration::from_secs(3);
const DAEMON_LOG_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15 * 60);
const MAX_DAEMON_LOG_EVENTS_PER_UPLOAD: usize = 1000;
const MAX_DAEMON_LOG_BUFFER_EVENTS: usize = 5000;

static METRICS_UPLOAD_AVAILABLE: AtomicBool = AtomicBool::new(false);
static METRICS_METADATA_BACKFILL_STARTED: AtomicBool = AtomicBool::new(false);
static DAEMON_LOG_UPLOAD_IN_FLIGHT: std::sync::OnceLock<Arc<AtomicBool>> =
    std::sync::OnceLock::new();

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

/// Accumulated telemetry events waiting to be flushed.
struct TelemetryBuffer {
    errors: Vec<ErrorEvent>,
    performances: Vec<PerformanceEvent>,
    messages: Vec<MessageEvent>,
    metrics: Vec<MetricEvent>,
    cas_records: Vec<CasSyncPayload>,
    daemon_logs: Vec<DaemonLogEvent>,
}

struct ErrorEvent {
    timestamp: String,
    message: String,
    context: Option<Value>,
}

struct PerformanceEvent {
    timestamp: String,
    operation: String,
    duration_ms: u128,
    context: Option<Value>,
    tags: Option<std::collections::HashMap<String, String>>,
}

struct MessageEvent {
    timestamp: String,
    message: String,
    level: String,
    context: Option<Value>,
}

impl TelemetryBuffer {
    fn new() -> Self {
        Self {
            errors: Vec::new(),
            performances: Vec::new(),
            messages: Vec::new(),
            metrics: Vec::new(),
            cas_records: Vec::new(),
            daemon_logs: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.errors.is_empty()
            && self.performances.is_empty()
            && self.messages.is_empty()
            && self.metrics.is_empty()
            && self.cas_records.is_empty()
            && self.daemon_logs.is_empty()
    }

    fn ingest_envelopes(&mut self, envelopes: Vec<TelemetryEnvelope>) {
        for envelope in envelopes {
            match envelope {
                TelemetryEnvelope::Error {
                    timestamp,
                    message,
                    context,
                } => {
                    self.errors.push(ErrorEvent {
                        timestamp,
                        message,
                        context,
                    });
                }
                TelemetryEnvelope::Performance {
                    timestamp,
                    operation,
                    duration_ms,
                    context,
                    tags,
                } => {
                    self.performances.push(PerformanceEvent {
                        timestamp,
                        operation,
                        duration_ms,
                        context,
                        tags,
                    });
                }
                TelemetryEnvelope::Message {
                    timestamp,
                    message,
                    level,
                    context,
                } => {
                    self.messages.push(MessageEvent {
                        timestamp,
                        message,
                        level,
                        context,
                    });
                }
                TelemetryEnvelope::Metrics { events } => {
                    self.metrics.extend(events);
                }
            }
        }
    }

    fn ingest_cas(&mut self, records: Vec<CasSyncPayload>) {
        self.cas_records.extend(records);
    }

    fn ingest_daemon_logs(&mut self, events: Vec<DaemonLogEvent>) {
        self.daemon_logs.extend(events);
        self.cap_daemon_logs();
    }

    fn requeue_failed_daemon_logs(&mut self, mut failed_events: Vec<DaemonLogEvent>) {
        failed_events.append(&mut self.daemon_logs);
        self.daemon_logs = failed_events;
        self.cap_daemon_logs();
    }

    fn cap_daemon_logs(&mut self) {
        let overflow = self
            .daemon_logs
            .len()
            .saturating_sub(MAX_DAEMON_LOG_BUFFER_EVENTS);
        if overflow > 0 {
            self.daemon_logs.drain(0..overflow);
        }
    }

    fn take(&mut self) -> TelemetryBuffer {
        TelemetryBuffer {
            errors: std::mem::take(&mut self.errors),
            performances: std::mem::take(&mut self.performances),
            messages: std::mem::take(&mut self.messages),
            metrics: std::mem::take(&mut self.metrics),
            cas_records: std::mem::take(&mut self.cas_records),
            daemon_logs: std::mem::take(&mut self.daemon_logs),
        }
    }
}

/// Handle for submitting telemetry directly within the daemon process.
#[derive(Clone)]
pub struct DaemonTelemetryWorkerHandle {
    buffer: Arc<Mutex<TelemetryBuffer>>,
    flush_tx: tokio::sync::mpsc::UnboundedSender<FlushRequest>,
}

impl DaemonTelemetryWorkerHandle {
    #[cfg(test)]
    pub fn new_noop() -> Self {
        let (flush_tx, _flush_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            buffer: Arc::new(Mutex::new(TelemetryBuffer::new())),
            flush_tx,
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
            std::mem::drop(tokio::task::spawn_blocking(move || {
                if let Err(e) = store_metrics_in_db(&metric_events) {
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

        if !METRICS_UPLOAD_AVAILABLE.load(Ordering::Relaxed) {
            return buffered;
        }

        let pending = match MetricsDatabase::global() {
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
        store_metrics_in_db(events)
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
            && let Err(e) = store_metrics_in_db(&metric_events)
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
/// The worker runs a flush loop every 3 seconds, sending accumulated events
/// to their respective destinations (Sentry, PostHog, metrics API, CAS API).
pub fn spawn_telemetry_worker() -> DaemonTelemetryWorkerHandle {
    let buffer = Arc::new(Mutex::new(TelemetryBuffer::new()));
    let (flush_tx, flush_rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = DaemonTelemetryWorkerHandle {
        buffer: buffer.clone(),
        flush_tx,
    };
    let daemon_id = crate::uuid::generate_v4();

    spawn_metrics_metadata_backfill();

    tokio::spawn(async move {
        telemetry_flush_loop(buffer, daemon_id, flush_rx).await;
    });

    handle
}

fn spawn_metrics_metadata_backfill() {
    if METRICS_METADATA_BACKFILL_STARTED.swap(true, Ordering::Relaxed) {
        return;
    }

    std::mem::drop(tokio::task::spawn_blocking(|| {
        if let Err(e) = backfill_metrics_event_metadata() {
            tracing::warn!(%e, "telemetry: failed to backfill metrics event metadata");
        }
    }));
}

fn backfill_metrics_event_metadata() -> Result<(), GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut after_id = 0;

    loop {
        let (summary, last_id) = {
            let mut db_lock = db
                .lock()
                .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
            db_lock.backfill_event_metadata_batch_after(after_id, METADATA_BACKFILL_BATCH_SIZE)?
        };

        let Some(id) = last_id else {
            break;
        };
        after_id = id;

        if summary.scanned < METADATA_BACKFILL_BATCH_SIZE {
            break;
        }
    }

    Ok(())
}

async fn telemetry_flush_loop(
    buffer: Arc<Mutex<TelemetryBuffer>>,
    daemon_id: String,
    mut flush_rx: tokio::sync::mpsc::UnboundedReceiver<FlushRequest>,
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
                flush_telemetry_batch(snapshot, &daemon_id_for_flush)
            } else {
                flush_pending_metrics();
                Vec::new()
            };
            let await_status = collect_await_flush_status(flush_mode);
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

fn flush_telemetry_batch(batch: TelemetryBuffer, daemon_id: &str) -> Vec<DaemonLogEvent> {
    let config = Config::get();
    let distinct_id = get_or_create_distinct_id();

    // Flush metrics (always processed — uploaded or stored in SQLite)
    if !batch.metrics.is_empty() {
        flush_metrics(&batch.metrics);
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
        flush_cas(batch.cas_records);
    }

    // Flush pending notes (reads directly from notes-db; no-op when kind != Http).
    flush_notes();

    flush_pending_metrics();

    if batch.daemon_logs.is_empty() {
        Vec::new()
    } else {
        dispatch_daemon_log_upload(batch.daemon_logs, daemon_id, &distinct_id)
    }
}

fn collect_await_flush_status(flush_mode: FlushMode) -> Option<FlushStatus> {
    collect_await_flush_status_with(
        flush_mode,
        flush_notes_for_await,
        count_pending_metrics_for_await,
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

fn count_pending_metrics_for_await() -> usize {
    if !METRICS_UPLOAD_AVAILABLE.load(Ordering::Relaxed) {
        return 0;
    }

    MetricsDatabase::global()
        .and_then(|db| {
            db.lock()
                .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))
        })
        .and_then(|db| db.count_retryable())
        .unwrap_or(0)
}

fn flush_metrics(events: &[MetricEvent]) {
    let context = ApiContext::new(None);
    let api_base_url = context.base_url.clone();
    let client = ApiClient::new(context);

    let should_upload = metrics_upload_allowed(&api_base_url, &client);
    METRICS_UPLOAD_AVAILABLE.store(should_upload, Ordering::Relaxed);

    let mut upload_failed = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);

    for chunk in events.chunks(MAX_METRICS_PER_ENVELOPE) {
        if let Err(e) = store_metrics_in_db(chunk) {
            tracing::warn!(%e, "telemetry: failed to persist metrics before upload");
            continue;
        }

        if should_upload && !upload_failed && std::time::Instant::now() < deadline {
            match flush_pending_metrics_from_db(&client, deadline) {
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(%e, "telemetry: failed to upload pending metrics");
                    upload_failed = true;
                }
            }
        }
    }
}

fn flush_pending_metrics() {
    let context = ApiContext::new(None);
    let api_base_url = context.base_url.clone();
    let client = ApiClient::new(context);

    let should_upload = metrics_upload_allowed(&api_base_url, &client);
    METRICS_UPLOAD_AVAILABLE.store(should_upload, Ordering::Relaxed);
    if !should_upload {
        return;
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    if let Err(e) = flush_pending_metrics_from_db(&client, deadline) {
        tracing::warn!(%e, "telemetry: failed to upload pending metrics");
    }
}

fn store_metrics_in_db(events: &[MetricEvent]) -> Result<Vec<i64>, GitAiError> {
    if events.is_empty() {
        return Ok(Vec::new());
    }

    let event_jsons: Vec<String> = events
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<_, _>>()?;

    if event_jsons.is_empty() {
        return Ok(Vec::new());
    }

    let db = MetricsDatabase::global()?;
    let mut db_lock = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    db_lock.insert_events(&event_jsons)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PendingMetricsFlushResult {
    uploaded_events: usize,
    uploaded_batches: usize,
    invalid_records: usize,
}

fn flush_pending_metrics_from_db(
    client: &ApiClient,
    deadline: std::time::Instant,
) -> Result<PendingMetricsFlushResult, GitAiError> {
    flush_pending_metric_records_with(
        read_pending_metrics_batch,
        mark_metric_records_delivered,
        mark_metric_records_failed,
        mark_metric_records_undeliverable,
        |batch| client.upload_metrics(batch),
        deadline,
        MAX_METRICS_PER_ENVELOPE,
    )
}

fn read_pending_metrics_batch(limit: usize) -> Result<Vec<MetricRecord>, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db_lock = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    db_lock.dequeue_pending_batch(limit)
}

fn mark_metric_records_delivered(ids: &[i64]) -> Result<(), GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db_lock = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    db_lock.mark_records_delivered(ids, current_unix_ts())
}

fn mark_metric_records_failed(ids: &[i64], error: &GitAiError) -> Result<(), GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db_lock = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    let now = current_unix_ts();
    db_lock.mark_records_failed(ids, &error.to_string(), now)
}

fn mark_metric_records_undeliverable(records: &[(i64, String)]) -> Result<(), GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db_lock = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    db_lock.mark_records_undeliverable(records, current_unix_ts())
}

fn flush_pending_metric_records_with<
    DequeueBatch,
    MarkDelivered,
    MarkFailed,
    MarkUndeliverable,
    UploadBatch,
>(
    mut dequeue_batch: DequeueBatch,
    mut mark_delivered: MarkDelivered,
    mut mark_failed: MarkFailed,
    mut mark_undeliverable: MarkUndeliverable,
    mut upload_batch: UploadBatch,
    deadline: std::time::Instant,
    max_batch_size: usize,
) -> Result<PendingMetricsFlushResult, GitAiError>
where
    DequeueBatch: FnMut(usize) -> Result<Vec<MetricRecord>, GitAiError>,
    MarkDelivered: FnMut(&[i64]) -> Result<(), GitAiError>,
    MarkFailed: FnMut(&[i64], &GitAiError) -> Result<(), GitAiError>,
    MarkUndeliverable: FnMut(&[(i64, String)]) -> Result<(), GitAiError>,
    UploadBatch: FnMut(&MetricsBatch) -> Result<MetricsUploadResponse, GitAiError>,
{
    let mut result = PendingMetricsFlushResult::default();

    while std::time::Instant::now() < deadline {
        let batch = dequeue_batch(max_batch_size)?;
        if batch.is_empty() {
            break;
        }

        let mut events = Vec::new();
        let mut record_ids = Vec::new();
        let mut invalid_ids = Vec::new();

        for record in &batch {
            match serde_json::from_str::<MetricEvent>(&record.event_json) {
                Ok(event) => {
                    events.push(event);
                    record_ids.push(record.id);
                }
                Err(_) => {
                    invalid_ids.push(record.id);
                }
            }
        }

        let batch_min_id = record_ids.iter().chain(invalid_ids.iter()).min().copied();
        let batch_max_id = record_ids.iter().chain(invalid_ids.iter()).max().copied();

        if !invalid_ids.is_empty() {
            result.invalid_records += invalid_ids.len();
            mark_delivered(&invalid_ids)?;
        }

        if events.is_empty() {
            continue;
        }

        let metrics_batch = MetricsBatch::new(events);
        tracing::info!(
            min_id = ?batch_min_id,
            max_id = ?batch_max_id,
            events = record_ids.len(),
            invalid_records = invalid_ids.len(),
            "metrics upload batch sending"
        );
        let response = match upload_batch(&metrics_batch) {
            Ok(response) => response,
            Err(e) => {
                tracing::info!(
                    min_id = ?batch_min_id,
                    max_id = ?batch_max_id,
                    events = record_ids.len(),
                    error = %e,
                    "metrics upload batch failed"
                );
                mark_failed(&record_ids, &e)?;
                return Err(e);
            }
        };

        if let Err(e) = response.validate_error_indices(record_ids.len()) {
            tracing::info!(
                min_id = ?batch_min_id,
                max_id = ?batch_max_id,
                events = record_ids.len(),
                error = %e,
                "metrics upload batch returned invalid response"
            );
            mark_failed(&record_ids, &e)?;
            return Err(e);
        }

        let successful_ids: Vec<i64> = response
            .successful_indices(record_ids.len())
            .into_iter()
            .map(|index| record_ids[index])
            .collect();
        let undeliverable_records: Vec<(i64, String)> = response
            .errors
            .iter()
            .map(|error| (record_ids[error.index], error.error.clone()))
            .collect();

        tracing::info!(
            min_id = ?batch_min_id,
            max_id = ?batch_max_id,
            events = record_ids.len(),
            delivered_events = successful_ids.len(),
            errored_events = undeliverable_records.len(),
            errors = ?response.errors,
            "metrics upload batch result"
        );

        mark_delivered(&successful_ids)?;
        mark_undeliverable(&undeliverable_records)?;

        result.uploaded_events += successful_ids.len();
        result.uploaded_batches += 1;
    }

    Ok(result)
}

fn current_unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn daemon_log_upload_enabled() -> bool {
    Config::fresh().get_feature_flags().daemon_log_upload
}

fn daemon_heartbeat_event(uptime: std::time::Duration) -> DaemonLogEvent {
    let mut fields = BTreeMap::new();
    fields.insert(
        "uptime_seconds".to_string(),
        DaemonLogFieldValue::from(uptime.as_secs()),
    );
    fields.insert(
        "os".to_string(),
        DaemonLogFieldValue::from(std::env::consts::OS),
    );
    fields.insert(
        "arch".to_string(),
        DaemonLogFieldValue::from(std::env::consts::ARCH),
    );

    DaemonLogEvent {
        id: Some(crate::uuid::generate_v4()),
        kind: DaemonLogKind::Heartbeat,
        timestamp: chrono::Utc::now().to_rfc3339(),
        level: DaemonLogLevel::Info,
        target: Some("git_ai::daemon".to_string()),
        message: "alive".to_string(),
        fields,
        repo_url: None,
        git_ai_version: None,
    }
}

fn daemon_log_upload_in_flight_flag() -> Arc<AtomicBool> {
    DAEMON_LOG_UPLOAD_IN_FLIGHT
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

struct DaemonLogUploadInFlightGuard {
    in_flight: Arc<AtomicBool>,
}

impl Drop for DaemonLogUploadInFlightGuard {
    fn drop(&mut self) {
        self.in_flight.store(false, Ordering::Release);
    }
}

fn dispatch_daemon_log_upload(
    events: Vec<DaemonLogEvent>,
    daemon_id: &str,
    install_id: &str,
) -> Vec<DaemonLogEvent> {
    let daemon_id = daemon_id.to_string();
    let install_id = install_id.to_string();

    dispatch_daemon_log_upload_with(events, daemon_log_upload_in_flight_flag(), move |events| {
        let failed_events = flush_daemon_logs(events, &daemon_id, &install_id);
        if failed_events > 0 {
            tracing::debug!(
                failed_events,
                "daemon log upload failed after fire-and-forget dispatch"
            );
        }
    })
}

fn dispatch_daemon_log_upload_with<Upload>(
    events: Vec<DaemonLogEvent>,
    in_flight: Arc<AtomicBool>,
    upload: Upload,
) -> Vec<DaemonLogEvent>
where
    Upload: FnOnce(Vec<DaemonLogEvent>) + Send + 'static,
{
    if events.is_empty() {
        return Vec::new();
    }

    if in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return events;
    }

    let in_flight_for_task = in_flight.clone();
    let spawn_result = std::thread::Builder::new()
        .name("git-ai-daemon-log-upload".to_string())
        .spawn(move || {
            let _guard = DaemonLogUploadInFlightGuard {
                in_flight: in_flight_for_task,
            };
            upload(events);
        });

    if let Err(error) = spawn_result {
        in_flight.store(false, Ordering::Release);
        tracing::debug!(%error, "failed to spawn daemon log upload task");
    }

    Vec::new()
}

fn flush_daemon_logs(events: Vec<DaemonLogEvent>, daemon_id: &str, install_id: &str) -> usize {
    if !daemon_log_upload_enabled() {
        return 0;
    }

    let context = ApiContext::new(None);
    let api_base_url = context.base_url.clone();
    let client = ApiClient::new(context);

    if !daemon_logs_upload_allowed(&api_base_url, &client) {
        // These diagnostics are intentionally best-effort and only live in memory.
        // If the current API/auth setup cannot upload, do not keep re-flushing the
        // same buffered events every few seconds.
        return 0;
    }

    upload_daemon_log_chunk(events, daemon_id, install_id, |request| {
        client.upload_daemon_logs(request).map(|_| ())
    })
}

fn upload_daemon_log_chunk<Upload>(
    events: Vec<DaemonLogEvent>,
    daemon_id: &str,
    install_id: &str,
    mut upload: Upload,
) -> usize
where
    Upload: FnMut(&DaemonLogsUploadRequest) -> Result<(), GitAiError>,
{
    let Some(chunk) = events.chunks(MAX_DAEMON_LOG_EVENTS_PER_UPLOAD).next() else {
        return 0;
    };

    let mut failed_events = events.len().saturating_sub(chunk.len());
    let request = DaemonLogsUploadRequest {
        version: DAEMON_LOGS_UPLOAD_VERSION,
        git_ai_version: Some(GIT_AI_VERSION.to_string()),
        daemon_id: Some(daemon_id.to_string()),
        install_id: Some(install_id.to_string()),
        repo_url: None,
        events: chunk.to_vec(),
    };

    if upload(&request).is_err() {
        failed_events += chunk.len();
    }

    failed_events
}

fn flush_sentry_and_posthog(
    config: &Config,
    distinct_id: &str,
    errors: &[ErrorEvent],
    performances: &[PerformanceEvent],
    messages: &[MessageEvent],
) {
    // Check for Enterprise DSN
    let enterprise_dsn = config
        .telemetry_enterprise_dsn()
        .map(|s| s.to_string())
        .or_else(|| {
            std::env::var("SENTRY_ENTERPRISE")
                .ok()
                .or_else(|| option_env!("SENTRY_ENTERPRISE").map(|s| s.to_string()))
                .filter(|s| !s.is_empty())
        });

    // Check for OSS DSN
    let oss_dsn = if config.is_telemetry_oss_disabled() {
        None
    } else {
        std::env::var("SENTRY_OSS")
            .ok()
            .or_else(|| option_env!("SENTRY_OSS").map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
    };

    // Check for PostHog configuration
    let posthog_api_key = if config.is_telemetry_oss_disabled() {
        None
    } else {
        std::env::var("POSTHOG_API_KEY")
            .ok()
            .or_else(|| option_env!("POSTHOG_API_KEY").map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
    };

    let posthog_host = std::env::var("POSTHOG_HOST")
        .ok()
        .or_else(|| option_env!("POSTHOG_HOST").map(|s| s.to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://us.i.posthog.com".to_string());

    // Build Sentry clients
    let oss_client = oss_dsn.and_then(|dsn| SentryClient::from_dsn(&dsn));
    let enterprise_client = enterprise_dsn.and_then(|dsn| SentryClient::from_dsn(&dsn));

    // Build base tags
    let mut base_tags = BTreeMap::new();
    base_tags.insert("os".to_string(), json!(std::env::consts::OS));
    base_tags.insert("arch".to_string(), json!(std::env::consts::ARCH));
    base_tags.insert("distinct_id".to_string(), json!(distinct_id));

    // Send errors
    for error in errors {
        let mut extra = BTreeMap::new();
        if let Some(ctx) = &error.context
            && let Some(obj) = ctx.as_object()
        {
            for (key, value) in obj {
                extra.insert(key.clone(), value.clone());
            }
        }

        let event = json!({
            "message": error.message,
            "level": "error",
            "timestamp": error.timestamp,
            "platform": "other",
            "tags": base_tags,
            "extra": extra,
            "release": format!("git-ai@{}", env!("CARGO_PKG_VERSION")),
        });

        if let Some(client) = &oss_client {
            let _ = client.send_event(event.clone());
        }
        if let Some(client) = &enterprise_client {
            let _ = client.send_event(event);
        }
    }

    // Send performance events
    for perf in performances {
        let mut extra = BTreeMap::new();
        extra.insert("operation".to_string(), json!(perf.operation));
        extra.insert("duration_ms".to_string(), json!(perf.duration_ms));
        if let Some(ctx) = &perf.context
            && let Some(obj) = ctx.as_object()
        {
            for (key, value) in obj {
                extra.insert(key.clone(), value.clone());
            }
        }

        let mut perf_tags = base_tags.clone();
        if let Some(tags) = &perf.tags {
            for (key, value) in tags {
                perf_tags.insert(key.clone(), json!(value));
            }
        }

        let event = json!({
            "message": format!("Performance: {} ({}ms)", perf.operation, perf.duration_ms),
            "level": "info",
            "timestamp": perf.timestamp,
            "platform": "other",
            "tags": perf_tags,
            "extra": extra,
            "release": format!("git-ai@{}", env!("CARGO_PKG_VERSION")),
        });

        if let Some(client) = &oss_client {
            let _ = client.send_event(event.clone());
        }
        if let Some(client) = &enterprise_client {
            let _ = client.send_event(event);
        }
    }

    // Send messages (to Sentry + PostHog)
    for msg in messages {
        let mut extra = BTreeMap::new();
        if let Some(ctx) = &msg.context
            && let Some(obj) = ctx.as_object()
        {
            for (key, value) in obj {
                extra.insert(key.clone(), value.clone());
            }
        }

        let sentry_event = json!({
            "message": msg.message,
            "level": msg.level,
            "timestamp": msg.timestamp,
            "platform": "other",
            "tags": base_tags,
            "extra": extra,
            "release": format!("git-ai@{}", env!("CARGO_PKG_VERSION")),
        });

        if let Some(client) = &oss_client {
            let _ = client.send_event(sentry_event.clone());
        }
        if let Some(client) = &enterprise_client {
            let _ = client.send_event(sentry_event);
        }

        // PostHog only gets messages
        if let Some(api_key) = &posthog_api_key {
            let mut properties = BTreeMap::new();
            properties.insert("os".to_string(), json!(std::env::consts::OS));
            properties.insert("arch".to_string(), json!(std::env::consts::ARCH));
            properties.insert("version".to_string(), json!(env!("CARGO_PKG_VERSION")));
            properties.insert("message".to_string(), json!(msg.message));
            properties.insert("level".to_string(), json!(msg.level));
            if let Some(ctx) = &msg.context
                && let Some(obj) = ctx.as_object()
            {
                for (key, value) in obj {
                    properties.insert(key.clone(), value.clone());
                }
            }

            let endpoint = format!("{}/capture/", posthog_host.trim_end_matches('/'));
            let mut ph_event = json!({
                "api_key": api_key,
                "event": msg.message,
                "properties": properties,
                "distinct_id": distinct_id,
            });
            ph_event["timestamp"] = json!(msg.timestamp);

            let agent = crate::http::build_agent(Some(30));
            let request = agent
                .post(&endpoint)
                .header("Content-Type", "application/json");
            let _ = crate::http::send_with_body(
                request,
                &serde_json::to_string(&ph_event).unwrap_or_default(),
            );
        }
    }
}

/// Flush pending notes from `notes-db` to the remote HTTP backend.
///
/// Skips silently when:
/// - `notes_backend.kind != Http`
/// - Not authenticated (no API key and not logged in)
pub fn flush_notes() {
    use crate::api::types::{NoteEntry, NotesUploadRequest};
    use crate::config::NotesBackendKind;

    let cfg = Config::fresh();
    if cfg.notes_backend_kind() != NotesBackendKind::Http {
        tracing::debug!("notes: skipping flush, backend is not Http");
        return;
    }

    let backend_url = match cfg.notes_backend_url() {
        Some(url) => url.to_string(),
        None => {
            tracing::debug!("notes: skipping flush, notes_backend.backend_url is not configured");
            return;
        }
    };
    let context = ApiContext::new(Some(backend_url));
    let client = ApiClient::new(context);

    if !client.is_logged_in() && !client.has_api_key() {
        tracing::debug!("notes: skipping flush, not authenticated");
        return;
    }

    // Dequeue up to 50 pending notes.
    let pending = match crate::notes::db::NotesDatabase::global() {
        Ok(db) => match db.lock() {
            Ok(mut lock) => match lock.dequeue_pending(50) {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!(%e, "notes: failed to dequeue pending rows");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!("notes: DB lock poisoned: {}", e);
                return;
            }
        },
        Err(e) => {
            tracing::warn!(%e, "notes: failed to get notes DB");
            return;
        }
    };

    if pending.is_empty() {
        return;
    }

    let commit_shas: Vec<String> = pending.iter().map(|p| p.commit_sha.clone()).collect();

    let entries: Vec<NoteEntry> = pending
        .iter()
        .map(|p| NoteEntry {
            commit_sha: p.commit_sha.clone(),
            content: p.content.clone(),
        })
        .collect();

    let request = NotesUploadRequest { entries };

    match client.upload_notes(request) {
        Ok(resp) => {
            tracing::debug!(
                success = resp.success_count,
                failure = resp.failure_count,
                "notes: uploaded batch"
            );
            if let Ok(db) = crate::notes::db::NotesDatabase::global()
                && let Ok(mut lock) = db.lock()
            {
                if resp.failure_count == 0 {
                    let _ = lock.mark_synced(&commit_shas);
                } else {
                    // Server reported partial failures but doesn't identify which
                    // entries failed. Mark the entire batch as failed so all entries
                    // are retried on the next flush cycle.
                    let _ = lock.mark_failed(
                        &commit_shas,
                        &format!(
                            "partial failure: {}/{} entries failed",
                            resp.failure_count,
                            commit_shas.len()
                        ),
                    );
                }
            }
        }
        Err(e) => {
            tracing::warn!(%e, "notes: upload error");
            if let Ok(db) = crate::notes::db::NotesDatabase::global()
                && let Ok(mut lock) = db.lock()
            {
                let _ = lock.mark_failed(&commit_shas, &e.to_string());
            }
        }
    }

    // Opportunistic cache eviction (~every 5 minutes at 3s flush interval).
    use std::sync::atomic::{AtomicU32, Ordering};
    static FLUSH_COUNT: AtomicU32 = AtomicU32::new(0);
    if FLUSH_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(100)
        && let Ok(db) = crate::notes::db::NotesDatabase::global()
        && let Ok(mut lock) = db.lock()
    {
        let _ = lock.evict_stale_cache(10_000, 90 * 24 * 3600);
    }
}

fn flush_notes_for_await() -> usize {
    use crate::config::NotesBackendKind;

    let cfg = Config::fresh();
    if cfg.notes_backend_kind() != NotesBackendKind::Http || cfg.notes_backend_url().is_none() {
        return 0;
    }

    let client = ApiClient::new(ApiContext::new(cfg.notes_backend_url().map(str::to_string)));
    if !client.is_logged_in() && !client.has_api_key() {
        return 0;
    }

    for _ in 0..1_000 {
        let remaining = count_pending_notes_for_await();
        if remaining == 0 {
            return 0;
        }
        flush_notes();
    }

    count_pending_notes_for_await()
}

fn count_pending_notes_for_await() -> usize {
    match crate::notes::db::NotesDatabase::global() {
        Ok(db) => match db.lock() {
            Ok(lock) => lock.count_pending_uploadable().unwrap_or(0),
            Err(_) => 0,
        },
        Err(_) => 0,
    }
}

fn flush_cas(records: Vec<CasSyncPayload>) {
    let context = ApiContext::new(None);
    let api_base_url = context.base_url.clone();
    let client = ApiClient::new(context);

    let using_default_api = api_base_url == crate::config::DEFAULT_API_BASE_URL;
    if using_default_api && !client.is_logged_in() && !client.has_api_key() {
        tracing::debug!("telemetry: skipping CAS flush, not logged in");
        return;
    }

    // Build upload request
    let mut cas_objects = Vec::new();
    for record in &records {
        let content: Value = match serde_json::from_str(&record.data) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%e, "telemetry: CAS parse error");
                continue;
            }
        };
        // Convert serialized JSON metadata string to HashMap
        let metadata = record
            .metadata
            .as_ref()
            .and_then(|m| serde_json::from_str::<std::collections::HashMap<String, String>>(m).ok())
            .unwrap_or_default();
        cas_objects.push(CasObject {
            content,
            hash: record.hash.clone(),
            metadata,
        });
    }

    if cas_objects.is_empty() {
        return;
    }

    for chunk in cas_objects.chunks(50) {
        let hashes: Vec<String> = chunk.iter().map(|o| o.hash.clone()).collect();
        let request = CasUploadRequest {
            objects: chunk.to_vec(),
        };
        match client.upload_cas(request) {
            Ok(_response) => {
                // Delete successfully uploaded records from the internal DB queue
                // so they don't accumulate as stale entries.
                if let Ok(db) = crate::authorship::internal_db::InternalDatabase::global()
                    && let Ok(mut db_lock) = db.lock()
                {
                    let _ = db_lock.delete_cas_by_hashes(&hashes);
                }
                tracing::debug!(count = chunk.len(), "telemetry: uploaded CAS objects");
            }
            Err(e) => {
                tracing::warn!(%e, "telemetry: CAS upload error");
            }
        }
    }
}

/// Minimal Sentry client (mirrors flush.rs SentryClient)
struct SentryClient {
    endpoint: String,
    public_key: String,
}

impl SentryClient {
    fn from_dsn(dsn: &str) -> Option<Self> {
        let url = url::Url::parse(dsn).ok()?;
        let public_key = url.username().to_string();
        let host = url.host_str()?;
        let project_id = url.path().trim_start_matches('/');
        let scheme = url.scheme();
        let endpoint = format!("{}://{}/api/{}/store/", scheme, host, project_id);
        Some(SentryClient {
            endpoint,
            public_key,
        })
    }

    fn send_event(&self, event: Value) -> Result<(), Box<dyn std::error::Error>> {
        let auth_header = format!(
            "Sentry sentry_version=7, sentry_key={}, sentry_client=git-ai/{}",
            self.public_key,
            env!("CARGO_PKG_VERSION")
        );

        let body = serde_json::to_string(&event)?;
        let agent = crate::http::build_agent(Some(30));
        let request = agent
            .post(&self.endpoint)
            .header("X-Sentry-Auth", &auth_header)
            .header("Content-Type", "application/json");
        let response = crate::http::send_with_body(request, &body)?;

        let status = response.status_code;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("Sentry returned status {}", status).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::metrics::MetricsUploadError;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    fn event_json(ts: u32) -> String {
        format!(r#"{{"t":{ts},"e":1,"v":{{}},"a":{{}}}}"#)
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    #[test]
    fn telemetry_flush_schedule_is_measured_from_completion() {
        let completed_at = tokio::time::Instant::now();

        assert_eq!(
            next_telemetry_flush_at(completed_at),
            completed_at + FLUSH_INTERVAL
        );
    }

    #[test]
    fn empty_periodic_flush_preserves_pending_metrics_only_fast_path() {
        let mut buffer = TelemetryBuffer::new();

        assert!(take_telemetry_flush_snapshot(&mut buffer, FlushMode::Periodic).is_none());
    }

    #[test]
    fn await_flush_forces_a_snapshot_even_when_the_buffer_is_empty() {
        let mut buffer = TelemetryBuffer::new();

        assert!(take_telemetry_flush_snapshot(&mut buffer, FlushMode::Await).is_some());
    }

    #[test]
    fn periodic_flush_skips_await_only_notes_and_metrics_status_work() {
        let status = collect_await_flush_status_with(
            FlushMode::Periodic,
            || panic!("periodic flush must not fully flush or count notes"),
            || panic!("periodic flush must not count metrics"),
        );

        assert!(status.is_none());
    }

    #[test]
    fn await_flush_collects_notes_and_metrics_status() {
        let status = collect_await_flush_status_with(FlushMode::Await, || 7, || 11);

        assert_eq!(
            status,
            Some(FlushStatus {
                metrics_remaining: 11,
                notes_remaining: 7,
            })
        );
    }

    fn now_ts() -> u32 {
        unix_now().min(u32::MAX as u64) as u32
    }

    fn test_message_envelope(message: &str) -> TelemetryEnvelope {
        TelemetryEnvelope::Message {
            timestamp: chrono::Utc::now().to_rfc3339(),
            message: message.to_string(),
            level: "info".to_string(),
            context: None,
        }
    }

    #[tokio::test]
    async fn submit_daemon_internal_telemetry_spawns_when_runtime_exists() {
        let handle = DaemonTelemetryWorkerHandle::new_noop();
        let guard = handle.buffer.lock().await;

        submit_daemon_internal_telemetry_with_handle(
            handle.clone(),
            vec![test_message_envelope("runtime")],
        );

        assert!(guard.messages.is_empty());
        drop(guard);

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if handle.buffer.lock().await.messages.len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn submit_daemon_internal_telemetry_waits_without_runtime() {
        let handle = DaemonTelemetryWorkerHandle::new_noop();

        submit_daemon_internal_telemetry_with_handle(
            handle.clone(),
            vec![test_message_envelope("sync")],
        );

        let guard = handle.buffer.try_lock().unwrap();
        assert_eq!(guard.messages.len(), 1);
    }

    #[test]
    fn flush_pending_metric_records_uploads_from_db_and_marks_delivered() {
        let (metrics_db, _metrics_db_dir) = MetricsDatabase::new_temp_for_tests().unwrap();
        let db = Rc::new(RefCell::new(metrics_db));
        let ts1 = now_ts().saturating_sub(2);
        let ts2 = now_ts().saturating_sub(1);
        db.borrow_mut()
            .insert_events(&[event_json(ts1), event_json(ts2)])
            .unwrap();

        let uploaded = Rc::new(RefCell::new(Vec::<Vec<u32>>::new()));
        let result = flush_pending_metric_records_with(
            {
                let db = Rc::clone(&db);
                move |limit| db.borrow_mut().dequeue_pending_batch(limit)
            },
            {
                let db = Rc::clone(&db);
                move |ids| db.borrow_mut().mark_records_delivered(ids, unix_now())
            },
            {
                let db = Rc::clone(&db);
                move |ids, err| {
                    let now = unix_now();
                    db.borrow_mut()
                        .mark_records_failed(ids, &err.to_string(), now)
                }
            },
            {
                let db = Rc::clone(&db);
                move |records| {
                    db.borrow_mut()
                        .mark_records_undeliverable(records, unix_now())
                }
            },
            {
                let uploaded = Rc::clone(&uploaded);
                move |batch| {
                    uploaded
                        .borrow_mut()
                        .push(batch.events.iter().map(|event| event.timestamp).collect());
                    Ok(MetricsUploadResponse { errors: vec![] })
                }
            },
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            1,
        )
        .unwrap();

        assert_eq!(
            result,
            PendingMetricsFlushResult {
                uploaded_events: 2,
                uploaded_batches: 2,
                invalid_records: 0,
            }
        );
        assert_eq!(*uploaded.borrow(), vec![vec![ts2], vec![ts1]]);
        assert_eq!(db.borrow().count().unwrap(), 0);
        assert_eq!(
            db.borrow().get_metric_history(0, None, &[1]).unwrap().len(),
            2
        );
    }

    #[test]
    fn flush_pending_metric_records_marks_invalid_rows_delivered() {
        let (metrics_db, _metrics_db_dir) = MetricsDatabase::new_temp_for_tests().unwrap();
        let db = Rc::new(RefCell::new(metrics_db));
        let ts = now_ts();
        db.borrow_mut()
            .insert_events(&["not-json".to_string(), event_json(ts)])
            .unwrap();

        let uploaded = Rc::new(RefCell::new(Vec::<u32>::new()));
        let result = flush_pending_metric_records_with(
            {
                let db = Rc::clone(&db);
                move |limit| db.borrow_mut().dequeue_pending_batch(limit)
            },
            {
                let db = Rc::clone(&db);
                move |ids| db.borrow_mut().mark_records_delivered(ids, unix_now())
            },
            {
                let db = Rc::clone(&db);
                move |ids, err| {
                    let now = unix_now();
                    db.borrow_mut()
                        .mark_records_failed(ids, &err.to_string(), now)
                }
            },
            {
                let db = Rc::clone(&db);
                move |records| {
                    db.borrow_mut()
                        .mark_records_undeliverable(records, unix_now())
                }
            },
            {
                let uploaded = Rc::clone(&uploaded);
                move |batch| {
                    uploaded
                        .borrow_mut()
                        .extend(batch.events.iter().map(|event| event.timestamp));
                    Ok(MetricsUploadResponse { errors: vec![] })
                }
            },
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            10,
        )
        .unwrap();

        assert_eq!(
            result,
            PendingMetricsFlushResult {
                uploaded_events: 1,
                uploaded_batches: 1,
                invalid_records: 1,
            }
        );
        assert_eq!(*uploaded.borrow(), vec![ts]);
        assert_eq!(db.borrow().count().unwrap(), 0);
        assert_eq!(
            db.borrow().get_metric_history(0, None, &[1]).unwrap().len(),
            1
        );
    }

    #[test]
    fn flush_pending_metric_records_marks_partial_server_errors_undeliverable() {
        let (metrics_db, _metrics_db_dir) = MetricsDatabase::new_temp_for_tests().unwrap();
        let db = Rc::new(RefCell::new(metrics_db));
        let ts1 = now_ts().saturating_sub(3);
        let ts2 = now_ts().saturating_sub(2);
        let ts3 = now_ts().saturating_sub(1);
        db.borrow_mut()
            .insert_events(&[event_json(ts1), event_json(ts2), event_json(ts3)])
            .unwrap();

        let uploaded = Rc::new(RefCell::new(Vec::<u32>::new()));
        let result = flush_pending_metric_records_with(
            {
                let db = Rc::clone(&db);
                move |limit| db.borrow_mut().dequeue_pending_batch(limit)
            },
            {
                let db = Rc::clone(&db);
                move |ids| db.borrow_mut().mark_records_delivered(ids, unix_now())
            },
            {
                let db = Rc::clone(&db);
                move |ids, err| {
                    let now = unix_now();
                    db.borrow_mut()
                        .mark_records_failed(ids, &err.to_string(), now)
                }
            },
            {
                let db = Rc::clone(&db);
                move |records| {
                    db.borrow_mut()
                        .mark_records_undeliverable(records, unix_now())
                }
            },
            {
                let uploaded = Rc::clone(&uploaded);
                move |batch| {
                    uploaded
                        .borrow_mut()
                        .extend(batch.events.iter().map(|event| event.timestamp));
                    Ok(MetricsUploadResponse {
                        errors: vec![MetricsUploadError {
                            index: 1,
                            error: "validation failed".to_string(),
                        }],
                    })
                }
            },
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            10,
        )
        .unwrap();

        assert_eq!(
            result,
            PendingMetricsFlushResult {
                uploaded_events: 2,
                uploaded_batches: 1,
                invalid_records: 0,
            }
        );
        assert_eq!(*uploaded.borrow(), vec![ts3, ts2, ts1]);
        assert_eq!(db.borrow().count().unwrap(), 1);
        assert_eq!(db.borrow().count_retryable().unwrap(), 0);
        assert!(
            db.borrow_mut()
                .dequeue_pending_batch(10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.borrow().get_metric_history(0, None, &[1]).unwrap().len(),
            3
        );
    }

    #[test]
    fn flush_pending_metric_records_marks_all_server_errors_undeliverable() {
        let (metrics_db, _metrics_db_dir) = MetricsDatabase::new_temp_for_tests().unwrap();
        let db = Rc::new(RefCell::new(metrics_db));
        let ts1 = now_ts().saturating_sub(2);
        let ts2 = now_ts().saturating_sub(1);
        db.borrow_mut()
            .insert_events(&[event_json(ts1), event_json(ts2)])
            .unwrap();

        let result = flush_pending_metric_records_with(
            {
                let db = Rc::clone(&db);
                move |limit| db.borrow_mut().dequeue_pending_batch(limit)
            },
            {
                let db = Rc::clone(&db);
                move |ids| db.borrow_mut().mark_records_delivered(ids, unix_now())
            },
            {
                let db = Rc::clone(&db);
                move |ids, err| {
                    let now = unix_now();
                    db.borrow_mut()
                        .mark_records_failed(ids, &err.to_string(), now)
                }
            },
            {
                let db = Rc::clone(&db);
                move |records| {
                    db.borrow_mut()
                        .mark_records_undeliverable(records, unix_now())
                }
            },
            |_batch| {
                Ok(MetricsUploadResponse {
                    errors: vec![
                        MetricsUploadError {
                            index: 0,
                            error: "first failed".to_string(),
                        },
                        MetricsUploadError {
                            index: 1,
                            error: "second failed".to_string(),
                        },
                    ],
                })
            },
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            10,
        )
        .unwrap();

        assert_eq!(
            result,
            PendingMetricsFlushResult {
                uploaded_events: 0,
                uploaded_batches: 1,
                invalid_records: 0,
            }
        );
        assert_eq!(db.borrow().count().unwrap(), 2);
        assert_eq!(db.borrow().count_retryable().unwrap(), 0);
        assert!(
            db.borrow_mut()
                .dequeue_pending_batch(10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.borrow().get_metric_history(0, None, &[1]).unwrap().len(),
            2
        );
    }

    #[test]
    fn flush_pending_metric_records_retries_batch_for_invalid_server_error_index() {
        let (metrics_db, _metrics_db_dir) = MetricsDatabase::new_temp_for_tests().unwrap();
        let db = Rc::new(RefCell::new(metrics_db));
        db.borrow_mut()
            .insert_events(&[event_json(now_ts().saturating_sub(1))])
            .unwrap();

        let result = flush_pending_metric_records_with(
            {
                let db = Rc::clone(&db);
                move |limit| db.borrow_mut().dequeue_pending_batch(limit)
            },
            {
                let db = Rc::clone(&db);
                move |ids| db.borrow_mut().mark_records_delivered(ids, unix_now())
            },
            {
                let db = Rc::clone(&db);
                move |ids, err| {
                    let now = unix_now();
                    db.borrow_mut()
                        .mark_records_failed(ids, &err.to_string(), now)
                }
            },
            {
                let db = Rc::clone(&db);
                move |records| {
                    db.borrow_mut()
                        .mark_records_undeliverable(records, unix_now())
                }
            },
            |_batch| {
                Ok(MetricsUploadResponse {
                    errors: vec![MetricsUploadError {
                        index: 1,
                        error: "out of bounds".to_string(),
                    }],
                })
            },
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            10,
        );

        assert!(result.is_err());
        assert_eq!(db.borrow().count().unwrap(), 1);
        assert_eq!(db.borrow().count_retryable().unwrap(), 0);
        assert_eq!(
            db.borrow().get_metric_history(0, None, &[1]).unwrap().len(),
            1
        );
    }

    #[test]
    fn flush_pending_metric_records_keeps_rows_pending_after_upload_failure() {
        let (metrics_db, _metrics_db_dir) = MetricsDatabase::new_temp_for_tests().unwrap();
        let db = Rc::new(RefCell::new(metrics_db));
        let ts = now_ts();
        db.borrow_mut().insert_events(&[event_json(ts)]).unwrap();

        let result = flush_pending_metric_records_with(
            {
                let db = Rc::clone(&db);
                move |limit| db.borrow_mut().dequeue_pending_batch(limit)
            },
            {
                let db = Rc::clone(&db);
                move |ids| db.borrow_mut().mark_records_delivered(ids, unix_now())
            },
            {
                let db = Rc::clone(&db);
                move |ids, err| {
                    let now = unix_now();
                    db.borrow_mut()
                        .mark_records_failed(ids, &err.to_string(), now)
                }
            },
            {
                let db = Rc::clone(&db);
                move |records| {
                    db.borrow_mut()
                        .mark_records_undeliverable(records, unix_now())
                }
            },
            |_batch| Err(GitAiError::Generic("upload failed".to_string())),
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            10,
        );

        assert!(result.is_err());
        assert_eq!(db.borrow().count().unwrap(), 1);
        assert_eq!(db.borrow().count_retryable().unwrap(), 0);
    }

    #[test]
    fn flush_pending_metric_records_uploads_new_rows_after_old_failure() {
        let (metrics_db, _metrics_db_dir) = MetricsDatabase::new_temp_for_tests().unwrap();
        let db = Rc::new(RefCell::new(metrics_db));
        let old_ts = now_ts().saturating_sub(10);
        db.borrow_mut()
            .insert_events(&[event_json(old_ts)])
            .unwrap();

        let failed = flush_pending_metric_records_with(
            {
                let db = Rc::clone(&db);
                move |limit| db.borrow_mut().dequeue_pending_batch(limit)
            },
            {
                let db = Rc::clone(&db);
                move |ids| db.borrow_mut().mark_records_delivered(ids, unix_now())
            },
            {
                let db = Rc::clone(&db);
                move |ids, err| {
                    let now = unix_now();
                    db.borrow_mut()
                        .mark_records_failed(ids, &err.to_string(), now)
                }
            },
            {
                let db = Rc::clone(&db);
                move |records| {
                    db.borrow_mut()
                        .mark_records_undeliverable(records, unix_now())
                }
            },
            |_batch| Err(GitAiError::Generic("upload failed".to_string())),
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            1,
        );
        assert!(failed.is_err());
        assert_eq!(db.borrow().count_retryable().unwrap(), 0);

        let new_ts = now_ts();
        db.borrow_mut()
            .insert_events(&[event_json(new_ts)])
            .unwrap();
        assert_eq!(db.borrow().count_retryable().unwrap(), 1);

        let uploaded = Rc::new(RefCell::new(Vec::<Vec<u32>>::new()));
        let result = flush_pending_metric_records_with(
            {
                let db = Rc::clone(&db);
                move |limit| db.borrow_mut().dequeue_pending_batch(limit)
            },
            {
                let db = Rc::clone(&db);
                move |ids| db.borrow_mut().mark_records_delivered(ids, unix_now())
            },
            {
                let db = Rc::clone(&db);
                move |ids, err| {
                    let now = unix_now();
                    db.borrow_mut()
                        .mark_records_failed(ids, &err.to_string(), now)
                }
            },
            {
                let db = Rc::clone(&db);
                move |records| {
                    db.borrow_mut()
                        .mark_records_undeliverable(records, unix_now())
                }
            },
            {
                let uploaded = Rc::clone(&uploaded);
                move |batch| {
                    uploaded
                        .borrow_mut()
                        .push(batch.events.iter().map(|event| event.timestamp).collect());
                    Ok(MetricsUploadResponse { errors: vec![] })
                }
            },
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            1,
        )
        .unwrap();

        assert_eq!(
            result,
            PendingMetricsFlushResult {
                uploaded_events: 1,
                uploaded_batches: 1,
                invalid_records: 0,
            }
        );
        assert_eq!(*uploaded.borrow(), vec![vec![new_ts]]);
        assert_eq!(db.borrow().count().unwrap(), 1);
        let history = db.borrow().get_metric_history(0, None, &[1]).unwrap();
        assert!(history.iter().any(|record| record.ts == old_ts));
    }

    fn sample_daemon_log_event(message: impl Into<String>) -> DaemonLogEvent {
        DaemonLogEvent {
            id: Some(crate::uuid::generate_v4()),
            kind: DaemonLogKind::Log,
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: DaemonLogLevel::Info,
            target: Some("git_ai::test".to_string()),
            message: message.into(),
            fields: BTreeMap::new(),
            repo_url: None,
            git_ai_version: None,
        }
    }

    #[test]
    fn upload_daemon_log_chunk_counts_events_past_per_upload_cap() {
        let events = (0..MAX_DAEMON_LOG_EVENTS_PER_UPLOAD + 2)
            .map(|index| sample_daemon_log_event(index.to_string()))
            .collect::<Vec<_>>();
        let uploaded_batch_sizes = Rc::new(RefCell::new(Vec::new()));

        let failed_events = upload_daemon_log_chunk(events, "daemon-id", "install-id", {
            let uploaded_batch_sizes = Rc::clone(&uploaded_batch_sizes);
            move |request| {
                uploaded_batch_sizes.borrow_mut().push(request.events.len());
                Ok(())
            }
        });

        assert_eq!(
            *uploaded_batch_sizes.borrow(),
            vec![MAX_DAEMON_LOG_EVENTS_PER_UPLOAD]
        );
        assert_eq!(failed_events, 2);
    }

    #[test]
    fn daemon_log_dispatch_requeues_when_upload_is_already_in_flight() {
        let in_flight = Arc::new(AtomicBool::new(true));
        let events = vec![sample_daemon_log_event("queued")];

        let retry_events = dispatch_daemon_log_upload_with(events, in_flight, |_events| {
            panic!("upload task should not run while another upload is running")
        });

        assert_eq!(retry_events.len(), 1);
        assert_eq!(retry_events[0].message, "queued");
    }

    #[test]
    fn daemon_log_dispatch_does_not_wait_for_upload_task() {
        let (upload_started_tx, upload_started_rx) = std::sync::mpsc::channel();
        let (release_upload_tx, release_upload_rx) = std::sync::mpsc::channel();
        let in_flight = Arc::new(AtomicBool::new(false));

        let started_at = std::time::Instant::now();
        let retry_events = dispatch_daemon_log_upload_with(
            vec![sample_daemon_log_event("blocked")],
            Arc::clone(&in_flight),
            move |_events| {
                upload_started_tx.send(()).unwrap();
                let _ = release_upload_rx.recv_timeout(Duration::from_secs(2));
            },
        );

        assert!(retry_events.is_empty());
        assert!(
            started_at.elapsed() < Duration::from_millis(500),
            "dispatch should return promptly while daemon log upload is blocked"
        );

        upload_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("upload task should start");
        assert!(in_flight.load(Ordering::Acquire));

        release_upload_tx.send(()).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while in_flight.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!in_flight.load(Ordering::Acquire));
    }

    #[test]
    fn telemetry_buffer_caps_daemon_logs_to_latest_events() {
        let mut buffer = TelemetryBuffer::new();
        let total = MAX_DAEMON_LOG_BUFFER_EVENTS + 2;
        let events = (0..total)
            .map(|index| sample_daemon_log_event(index.to_string()))
            .collect();

        buffer.ingest_daemon_logs(events);

        assert_eq!(buffer.daemon_logs.len(), MAX_DAEMON_LOG_BUFFER_EVENTS);
        assert_eq!(buffer.daemon_logs.first().unwrap().message, "2");
        assert_eq!(
            buffer.daemon_logs.last().unwrap().message,
            (total - 1).to_string()
        );
    }

    #[test]
    fn telemetry_buffer_requeues_failed_daemon_logs_without_dropping_newer_events() {
        let mut buffer = TelemetryBuffer::new();
        buffer.ingest_daemon_logs(vec![
            sample_daemon_log_event("new-1"),
            sample_daemon_log_event("new-2"),
        ]);

        let failed_events = (0..MAX_DAEMON_LOG_BUFFER_EVENTS)
            .map(|index| sample_daemon_log_event(format!("old-{index}")))
            .collect();

        buffer.requeue_failed_daemon_logs(failed_events);

        assert_eq!(buffer.daemon_logs.len(), MAX_DAEMON_LOG_BUFFER_EVENTS);
        assert_eq!(buffer.daemon_logs.first().unwrap().message, "old-2");
        assert_eq!(
            buffer.daemon_logs[MAX_DAEMON_LOG_BUFFER_EVENTS - 2].message,
            "new-1"
        );
        assert_eq!(
            buffer.daemon_logs[MAX_DAEMON_LOG_BUFFER_EVENTS - 1].message,
            "new-2"
        );
    }

    #[test]
    fn daemon_heartbeat_event_uses_upload_contract_shape() {
        let event = daemon_heartbeat_event(std::time::Duration::from_secs(900));

        assert!(event.id.is_some());
        assert_eq!(event.kind, DaemonLogKind::Heartbeat);
        assert_eq!(event.level, DaemonLogLevel::Info);
        assert_eq!(event.target.as_deref(), Some("git_ai::daemon"));
        assert_eq!(event.message, "alive");
        assert_eq!(
            event.fields.get("uptime_seconds"),
            Some(&DaemonLogFieldValue::from(900_u64))
        );
        assert!(event.fields.contains_key("os"));
        assert!(event.fields.contains_key("arch"));
    }
}
