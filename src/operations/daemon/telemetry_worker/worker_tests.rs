//! Tests for the telemetry worker core (flush loop, buffer, handle).

use super::*;
use crate::model::api_types::{DaemonLogEvent, DaemonLogKind, DaemonLogLevel};
use std::collections::BTreeMap;

fn test_message_envelope(message: &str) -> TelemetryEnvelope {
    TelemetryEnvelope::Message {
        timestamp: chrono::Utc::now().to_rfc3339(),
        message: message.to_string(),
        level: "info".to_string(),
        context: None,
    }
}

pub(super) fn sample_daemon_log_event(message: impl Into<String>) -> DaemonLogEvent {
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
fn telemetry_buffer_caps_daemon_logs_to_latest_events() {
    use buffer::MAX_DAEMON_LOG_BUFFER_EVENTS;
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
    use buffer::MAX_DAEMON_LOG_BUFFER_EVENTS;
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
fn emergency_daemon_log_upload_waits_for_completion() {
    let (uploaded_tx, uploaded_rx) = std::sync::mpsc::channel();

    let status = daemon_log_upload::upload_emergency_daemon_log_with(
        sample_daemon_log_event("memory emergency"),
        std::time::Duration::from_secs(1),
        move |event| uploaded_tx.send(event.message).unwrap(),
    );

    assert_eq!(status, EmergencyLogUploadStatus::Completed);
    assert_eq!(uploaded_rx.recv().unwrap(), "memory emergency");
}

#[test]
fn emergency_daemon_log_upload_reuses_the_daemon_run_id() {
    let run_id = daemon_run_id().to_string();

    assert_eq!(daemon_run_id(), run_id);
}

#[test]
fn emergency_daemon_log_upload_has_a_hard_wait_bound() {
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let started_at = std::time::Instant::now();

    let status = daemon_log_upload::upload_emergency_daemon_log_with(
        sample_daemon_log_event("memory emergency"),
        std::time::Duration::from_millis(10),
        move |_event| {
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(1));
        },
    );

    assert_eq!(status, EmergencyLogUploadStatus::TimedOut);
    assert!(started_at.elapsed() < std::time::Duration::from_millis(500));
    release_tx.send(()).unwrap();
}
