//! Metrics persistence and upload flush logic.

use super::{MetricsDbHandle, TelemetryStores};
use crate::clients::api::metrics::{MetricsUploadResponse, metrics_upload_allowed};
use crate::clients::api::{ApiClient, ApiContext};
use crate::error::GitAiError;
use crate::metrics::{MetricEvent, MetricsBatch};
use crate::model::repository::error::PersistenceError;
use crate::model::repository::metrics_db::{METADATA_BACKFILL_BATCH_SIZE, MetricRecord};
use crate::observability::MAX_METRICS_PER_ENVELOPE;
use crate::operations::git::repository::resolve_api_author_identity;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) static METRICS_UPLOAD_AVAILABLE: AtomicBool = AtomicBool::new(false);
static METRICS_METADATA_BACKFILL_STARTED: AtomicBool = AtomicBool::new(false);

/// Build the default API client, returning its resolved base URL alongside it.
pub(super) fn default_api_base_and_client() -> (String, ApiClient) {
    let context = ApiContext::new(None, resolve_api_author_identity);
    (context.base_url.clone(), ApiClient::new(context))
}

pub(super) fn spawn_metrics_metadata_backfill(stores: TelemetryStores) {
    if METRICS_METADATA_BACKFILL_STARTED.swap(true, Ordering::Relaxed) {
        return;
    }

    std::mem::drop(tokio::task::spawn_blocking(move || {
        if let Err(e) = backfill_metrics_event_metadata(stores.metrics) {
            tracing::warn!(%e, "telemetry: failed to backfill metrics event metadata");
        }
    }));
}

fn backfill_metrics_event_metadata(db: MetricsDbHandle) -> Result<(), GitAiError> {
    let mut after_id = 0;

    loop {
        let (summary, last_id) = {
            let mut db_lock = db
                .lock()
                .map_err(|_| PersistenceError::LockPoisoned { what: "metrics DB" })?;
            db_lock.backfill_event_metadata_batch_once(after_id, METADATA_BACKFILL_BATCH_SIZE)?
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

// Fall back to global() when stores is None (noop/test handles only).
pub(super) fn metrics_store(
    stores: Option<TelemetryStores>,
) -> Result<MetricsDbHandle, GitAiError> {
    stores.map_or_else(
        crate::model::repository::metrics_db::MetricsDatabase::global,
        |s| Ok(s.metrics),
    )
}

pub(super) fn store_metrics_in_db_with(
    db: Result<MetricsDbHandle, GitAiError>,
    events: &[MetricEvent],
) -> Result<Vec<i64>, GitAiError> {
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let event_jsons: Vec<String> = events
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<_, _>>()?;
    let mut db_lock = db?
        .lock()
        .map_err(|_| PersistenceError::LockPoisoned { what: "metrics DB" })?;
    db_lock.insert_events(&event_jsons)
}

pub(super) fn flush_metrics(events: &[MetricEvent], stores: TelemetryStores) {
    let (api_base_url, client) = default_api_base_and_client();

    let should_upload = metrics_upload_allowed(&api_base_url, &client);
    METRICS_UPLOAD_AVAILABLE.store(should_upload, Ordering::Relaxed);

    let mut upload_failed = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);

    for chunk in events.chunks(MAX_METRICS_PER_ENVELOPE) {
        if let Err(e) = store_metrics_in_db_with(Ok(stores.metrics), chunk) {
            tracing::warn!(%e, "telemetry: failed to persist metrics before upload");
            continue;
        }

        if should_upload && !upload_failed && std::time::Instant::now() < deadline {
            match flush_pending_metrics_from_db(&client, deadline, stores) {
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(%e, "telemetry: failed to upload pending metrics");
                    upload_failed = true;
                }
            }
        }
    }
}

pub(super) fn flush_pending_metrics(stores: TelemetryStores) {
    let (api_base_url, client) = default_api_base_and_client();

    let should_upload = metrics_upload_allowed(&api_base_url, &client);
    METRICS_UPLOAD_AVAILABLE.store(should_upload, Ordering::Relaxed);
    if !should_upload {
        return;
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    if let Err(e) = flush_pending_metrics_from_db(&client, deadline, stores) {
        tracing::warn!(%e, "telemetry: failed to upload pending metrics");
    }
}

pub(super) fn count_pending_metrics_for_await(stores: TelemetryStores) -> usize {
    if !METRICS_UPLOAD_AVAILABLE.load(Ordering::Relaxed) {
        return 0;
    }

    stores
        .metrics
        .lock()
        .map_err(|_| GitAiError::from(PersistenceError::LockPoisoned { what: "metrics DB" }))
        .and_then(|db| db.count_retryable())
        .unwrap_or(0)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::operations::daemon) struct PendingMetricsFlushResult {
    uploaded_events: usize,
    uploaded_batches: usize,
    invalid_records: usize,
}

fn flush_pending_metrics_from_db(
    client: &ApiClient,
    deadline: std::time::Instant,
    stores: TelemetryStores,
) -> Result<PendingMetricsFlushResult, GitAiError> {
    let db = stores.metrics;
    let lock_db = || {
        db.lock()
            .map_err(|_| GitAiError::from(PersistenceError::LockPoisoned { what: "metrics DB" }))
    };
    flush_pending_metric_records_with(
        |limit| lock_db().and_then(|mut l| l.dequeue_pending_batch(limit)),
        |ids| lock_db().and_then(|mut l| l.mark_records_delivered(ids, current_unix_ts())),
        |ids, error| {
            let now = current_unix_ts();
            lock_db().and_then(|mut l| l.mark_records_failed(ids, &error.to_string(), now))
        },
        |records| {
            lock_db().and_then(|mut l| l.mark_records_undeliverable(records, current_unix_ts()))
        },
        |batch| client.upload_metrics(batch),
        deadline,
        MAX_METRICS_PER_ENVELOPE,
    )
}

pub fn flush_pending_metric_records_with<
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
                if e.is_terminal_api_error() {
                    let u: Vec<_> = record_ids.iter().map(|&id| (id, e.to_string())).collect();
                    mark_undeliverable(&u)?;
                } else {
                    mark_failed(&record_ids, &e)?;
                }
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

pub(super) fn current_unix_ts() -> u64 {
    crate::model::clock::now_secs()
}

#[cfg(test)]
#[path = "metrics_flush_tests.rs"]
mod metrics_flush_tests;
