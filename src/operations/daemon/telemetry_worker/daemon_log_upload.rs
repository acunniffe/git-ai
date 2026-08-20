//! Daemon diagnostic log batching and fire-and-forget upload.

use super::metrics_flush::default_api_base_and_client;
use crate::clients::api::logs::daemon_logs_upload_allowed;
use crate::config::Config;
use crate::model::api_types::{
    DAEMON_LOGS_UPLOAD_VERSION, DaemonLogEvent, DaemonLogFieldValue, DaemonLogKind, DaemonLogLevel,
    DaemonLogsUploadRequest,
};
use crate::model::authorship_log_serialization::GIT_AI_VERSION;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::time::Duration;

pub(super) const MAX_DAEMON_LOG_EVENTS_PER_UPLOAD: usize = 1000;
pub(super) const DAEMON_LOG_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15 * 60);

static DAEMON_LOG_UPLOAD_IN_FLIGHT: std::sync::OnceLock<Arc<AtomicBool>> =
    std::sync::OnceLock::new();
static DAEMON_RUN_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// A stable id for this daemon process, shared by the periodic telemetry
/// worker and the emergency upload path so their events correlate.
pub(super) fn daemon_run_id() -> &'static str {
    DAEMON_RUN_ID.get_or_init(crate::uuid::generate_v4).as_str()
}

struct DaemonLogUploadInFlightGuard {
    in_flight: Arc<AtomicBool>,
}

impl Drop for DaemonLogUploadInFlightGuard {
    fn drop(&mut self) {
        self.in_flight.store(false, Ordering::Release);
    }
}

pub(super) fn daemon_log_upload_enabled() -> bool {
    let config = Config::fresh();
    // The feature flag is a granular kill-switch under the master telemetry
    // switch: both must be on for heartbeats and daemon-log uploads.
    config.telemetry_enabled() && config.get_feature_flags().daemon_log_upload
}

pub(super) fn daemon_heartbeat_event(uptime: std::time::Duration) -> DaemonLogEvent {
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

pub(super) fn dispatch_daemon_log_upload(
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

pub(super) fn dispatch_daemon_log_upload_with<Upload>(
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

    let (api_base_url, client) = default_api_base_and_client();

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EmergencyLogUploadStatus {
    Completed,
    TimedOut,
    ThreadUnavailable,
}

/// Give one emergency diagnostic a bounded chance to reach the log endpoint.
///
/// Normal daemon-log delivery is deliberately fire-and-forget. Memory-limit
/// shutdown cannot rely on that worker surviving process exit, so this path
/// waits briefly for its own upload without allowing a stuck network request
/// to keep an unhealthy daemon alive.
pub(crate) fn upload_emergency_daemon_log(
    event: DaemonLogEvent,
    timeout: std::time::Duration,
) -> EmergencyLogUploadStatus {
    upload_emergency_daemon_log_with(event, timeout, |event| {
        let install_id = crate::config::get_or_create_distinct_id();
        let _ = flush_daemon_logs(vec![event], daemon_run_id(), &install_id);
    })
}

pub(super) fn upload_emergency_daemon_log_with<Upload>(
    event: DaemonLogEvent,
    timeout: std::time::Duration,
    upload: Upload,
) -> EmergencyLogUploadStatus
where
    Upload: FnOnce(DaemonLogEvent) + Send + 'static,
{
    let (completion_tx, completion_rx) = std::sync::mpsc::sync_channel(0);
    let spawn_result = std::thread::Builder::new()
        .name("git-ai-emergency-log-upload".to_string())
        .spawn(move || {
            upload(event);
            let _ = completion_tx.send(());
        });

    if spawn_result.is_err() {
        return EmergencyLogUploadStatus::ThreadUnavailable;
    }

    match completion_rx.recv_timeout(timeout) {
        Ok(()) => EmergencyLogUploadStatus::Completed,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => EmergencyLogUploadStatus::TimedOut,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            EmergencyLogUploadStatus::ThreadUnavailable
        }
    }
}

pub(super) fn upload_daemon_log_chunk<Upload>(
    events: Vec<DaemonLogEvent>,
    daemon_id: &str,
    install_id: &str,
    mut upload: Upload,
) -> usize
where
    Upload: FnMut(&DaemonLogsUploadRequest) -> Result<(), crate::error::GitAiError>,
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

#[cfg(test)]
mod tests {
    use super::super::worker_tests::sample_daemon_log_event;
    use super::*;

    #[test]
    fn upload_daemon_log_chunk_counts_events_past_per_upload_cap() {
        let events = (0..MAX_DAEMON_LOG_EVENTS_PER_UPLOAD + 2)
            .map(|index| sample_daemon_log_event(index.to_string()))
            .collect::<Vec<_>>();
        use std::cell::RefCell;
        use std::rc::Rc;
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
