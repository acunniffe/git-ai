//! Handle flush-metrics-db command (kept for manual human use).
//!
//! Uploads pending metrics database rows to the API.

use crate::api::{ApiClient, ApiContext, metrics_upload_allowed, upload_metrics_with_retry};
use crate::metrics::db::MetricsDatabase;
use crate::metrics::{MetricEvent, MetricsBatch};

/// Max events per batch upload
const MAX_BATCH_SIZE: usize = 1000;

/// Handle the flush-metrics-db command
pub fn handle_flush_metrics_db(_args: &[String]) {
    let context = ApiContext::new(None);
    let api_base_url = context.base_url.clone();
    let client = ApiClient::new(context);

    if !metrics_upload_allowed(&api_base_url, &client) {
        eprintln!("flush-metrics-db: skipping (requires an API key or login)");
        return;
    }

    // Get database connection
    let db = match MetricsDatabase::global() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("flush-metrics-db: failed to open metrics database: {}", e);
            return;
        }
    };

    let mut total_uploaded = 0usize;
    let mut total_batches = 0usize;
    let mut total_invalid = 0usize;
    let mut total_undeliverable = 0usize;

    loop {
        // Get batch from DB
        let batch = {
            let mut db_lock = match db.lock() {
                Ok(lock) => lock,
                Err(e) => {
                    eprintln!("flush-metrics-db: failed to acquire db lock: {}", e);
                    break;
                }
            };
            match db_lock.dequeue_pending_batch(
                MAX_BATCH_SIZE,
                crate::config::Config::get().max_metrics_flush_chunk_bytes(),
            ) {
                Ok(batch) => batch,
                Err(e) => {
                    eprintln!("flush-metrics-db: failed to read batch: {}", e);
                    break;
                }
            }
        };

        // If batch is empty, we're done
        if batch.is_empty() {
            break;
        }

        // Parse events and build MetricsBatch
        let mut events = Vec::new();
        let mut record_ids = Vec::new();

        for record in &batch {
            if let Ok(event) = serde_json::from_str::<MetricEvent>(&record.event_json) {
                events.push(event);
                record_ids.push(record.id);
            } else {
                total_invalid += 1;
                // Invalid JSON cannot upload successfully. Mark it delivered so
                // future flushes can continue past the malformed historical row.
                if let Ok(mut db_lock) = db.lock() {
                    let _ = db_lock.mark_records_delivered(&[record.id], current_unix_ts());
                }
            }
        }

        if events.is_empty() {
            continue;
        }

        let event_count = events.len();
        let metrics_batch = MetricsBatch::new(events);

        // Upload with the HTTP helper's short retry, then persist DB backoff on failure.
        match upload_metrics_with_retry(&client, &metrics_batch, "flush_metrics_db") {
            Ok(response) => {
                if let Err(e) = response.validate_error_indices(record_ids.len()) {
                    eprintln!(
                        "  ✗ batch upload response invalid ({} events kept for retry): {}",
                        event_count, e
                    );
                    if let Ok(mut db_lock) = db.lock() {
                        let now = current_unix_ts();
                        let error = e.to_string();
                        let _ = db_lock.mark_records_failed(&record_ids, &error, now);
                    }
                    break;
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

                total_uploaded += successful_ids.len();
                total_batches += 1;
                total_undeliverable += undeliverable_records.len();
                eprintln!(
                    "  ✓ batch {} - uploaded {} events{}",
                    total_batches,
                    successful_ids.len(),
                    if undeliverable_records.is_empty() {
                        String::new()
                    } else {
                        format!(" ({} marked undeliverable)", undeliverable_records.len())
                    }
                );
                // Keep rows as history: mark successful rows delivered and
                // server-rejected rows permanently undeliverable.
                if let Ok(mut db_lock) = db.lock() {
                    let now = current_unix_ts();
                    let _ = db_lock.mark_records_delivered(&successful_ids, now);
                    let _ = db_lock.mark_records_undeliverable(&undeliverable_records, now);
                }
            }
            Err(e) => {
                // All retries failed - keep records in DB for a later queued retry.
                eprintln!(
                    "  ✗ batch upload failed ({} events kept for retry): {}",
                    event_count, e
                );
                if let Ok(mut db_lock) = db.lock() {
                    let now = current_unix_ts();
                    let error = e.to_string();
                    let _ = db_lock.mark_records_failed(&record_ids, &error, now);
                }
                break;
            }
        }
    }

    if total_invalid > 0 {
        eprintln!(
            "flush-metrics-db: marked {} invalid record(s) delivered",
            total_invalid
        );
    }
    if total_undeliverable > 0 {
        eprintln!(
            "flush-metrics-db: marked {} server-rejected record(s) undeliverable",
            total_undeliverable
        );
    }

    eprintln!(
        "flush-metrics-db: uploaded {} events in {} batch(es)",
        total_uploaded, total_batches
    );
}

fn current_unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
