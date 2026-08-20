//! Daemon peak-RSS memory watchdog.
//!
//! When `daemon_memory_limit_mb` is configured, a dedicated OS thread samples
//! the process's peak RSS once per poll interval — entirely off the trace2
//! ingestion path — and aborts the daemon at 85% of the configured limit,
//! preserving headroom before the hard ceiling. Before aborting it flushes a
//! durable stderr diagnostic and gives one direct daemon-log upload a bounded
//! window to complete. It deliberately does not drain in-flight work or
//! restart the daemon: normal demand starts a fresh daemon later, and the
//! checkpoint outbox preserves durability across the abort.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use crate::model::api_types::{DaemonLogEvent, DaemonLogFieldValue, DaemonLogKind, DaemonLogLevel};

use super::ActorDaemonCoordinator;
use super::telemetry_worker::{EmergencyLogUploadStatus, upload_emergency_daemon_log};

const EMERGENCY_PERCENT: u64 = 85;
const EMERGENCY_LOG_UPLOAD_TIMEOUT: Duration = Duration::from_millis(500);
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(feature = "test-support")]
pub(super) const TEST_PEAK_RSS_SEQUENCE_ENV: &str = "GIT_AI_TEST_DAEMON_PEAK_RSS_MB_SEQUENCE";
#[cfg(feature = "test-support")]
pub(super) const TEST_POLL_INTERVAL_ENV: &str = "GIT_AI_TEST_DAEMON_MEMORY_POLL_MS";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MemoryThresholds {
    pub(super) emergency_bytes: u64,
    pub(super) limit_bytes: u64,
}

impl MemoryThresholds {
    pub(super) fn from_limit_bytes(limit_bytes: u64) -> Self {
        let emergency_bytes =
            ((u128::from(limit_bytes) * u128::from(EMERGENCY_PERCENT)).div_ceil(100)) as u64;
        Self {
            emergency_bytes,
            limit_bytes,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MemoryWatchdogDecision {
    Continue,
    Abort,
}

pub(super) fn start(coordinator: Arc<ActorDaemonCoordinator>, limit_bytes: u64) -> io::Result<()> {
    let thresholds = MemoryThresholds::from_limit_bytes(limit_bytes);
    std::thread::Builder::new()
        .name("memory-watchdog".to_string())
        .spawn(move || {
            run_watchdog(coordinator, thresholds);
        })
        .map(|_| ())
}

fn run_watchdog(coordinator: Arc<ActorDaemonCoordinator>, thresholds: MemoryThresholds) {
    let mut sampler = PeakRssSampler::new();
    let mut measurement_failed = false;
    let poll_interval = watchdog_poll_interval();

    tracing::info!(
        memory_limit_bytes = thresholds.limit_bytes,
        memory_emergency_threshold_bytes = thresholds.emergency_bytes,
        memory_poll_interval_ms = poll_interval.as_millis() as u64,
        "daemon memory watchdog started"
    );

    loop {
        std::thread::sleep(poll_interval);
        if coordinator.is_shutting_down() {
            return;
        }

        let peak_rss_bytes = match sampler.sample() {
            Ok(bytes) => {
                if measurement_failed {
                    tracing::info!("daemon peak-RSS measurement recovered");
                    measurement_failed = false;
                }
                bytes
            }
            Err(error) => {
                if !measurement_failed {
                    tracing::warn!(%error, "failed measuring daemon peak RSS; watchdog will retry");
                    measurement_failed = true;
                }
                continue;
            }
        };

        match decision_for_peak_rss(peak_rss_bytes, thresholds) {
            MemoryWatchdogDecision::Continue => {}
            MemoryWatchdogDecision::Abort => {
                record_memory_emergency(peak_rss_bytes, thresholds, "abort");
                std::process::abort();
            }
        }
    }
}

fn record_memory_emergency(
    peak_rss_bytes: u64,
    thresholds: MemoryThresholds,
    action: &'static str,
) {
    tracing::error!(
        peak_rss_bytes,
        memory_emergency_threshold_bytes = thresholds.emergency_bytes,
        memory_limit_bytes = thresholds.limit_bytes,
        action,
        "daemon memory emergency threshold reached"
    );
    eprintln!(
        "[git-ai] daemon memory emergency threshold reached (peak RSS {peak_rss_bytes} bytes, emergency threshold {} bytes, hard limit {} bytes); {action}ing immediately without draining",
        thresholds.emergency_bytes, thresholds.limit_bytes
    );
    let _ = io::stderr().flush();

    let mut fields = BTreeMap::new();
    fields.insert(
        "peak_rss_bytes".to_string(),
        DaemonLogFieldValue::from(peak_rss_bytes),
    );
    fields.insert(
        "memory_emergency_threshold_bytes".to_string(),
        DaemonLogFieldValue::from(thresholds.emergency_bytes),
    );
    fields.insert(
        "memory_limit_bytes".to_string(),
        DaemonLogFieldValue::from(thresholds.limit_bytes),
    );
    fields.insert("action".to_string(), DaemonLogFieldValue::from(action));
    let event = DaemonLogEvent {
        id: Some(crate::uuid::generate_v4()),
        kind: DaemonLogKind::Log,
        timestamp: chrono::Utc::now().to_rfc3339(),
        level: DaemonLogLevel::Error,
        target: Some("git_ai::daemon::memory_watchdog".to_string()),
        message: "daemon memory emergency threshold reached".to_string(),
        fields,
        repo_url: None,
        git_ai_version: None,
    };
    match upload_emergency_daemon_log(event, EMERGENCY_LOG_UPLOAD_TIMEOUT) {
        EmergencyLogUploadStatus::Completed => {}
        EmergencyLogUploadStatus::TimedOut => {
            eprintln!("[git-ai] emergency daemon log upload timed out; continuing shutdown");
        }
        EmergencyLogUploadStatus::ThreadUnavailable => {
            eprintln!("[git-ai] emergency daemon log upload could not start; continuing shutdown");
        }
    }
    let _ = io::stderr().flush();
}

fn watchdog_poll_interval() -> Duration {
    #[cfg(feature = "test-support")]
    if let Ok(raw) = std::env::var(TEST_POLL_INTERVAL_ENV)
        && let Ok(milliseconds) = raw.parse::<u64>()
        && milliseconds > 0
    {
        return Duration::from_millis(milliseconds);
    }

    WATCHDOG_POLL_INTERVAL
}

struct PeakRssSampler {
    #[cfg(feature = "test-support")]
    test_samples: Option<std::collections::VecDeque<u64>>,
    #[cfg(feature = "test-support")]
    last_test_sample: Option<u64>,
}

impl PeakRssSampler {
    fn new() -> Self {
        #[cfg(feature = "test-support")]
        {
            let test_samples = std::env::var(TEST_PEAK_RSS_SEQUENCE_ENV)
                .ok()
                .and_then(|raw| {
                    raw.split(',')
                        .map(|part| part.trim().parse::<u64>().ok())
                        .collect::<Option<std::collections::VecDeque<_>>>()
                })
                .filter(|samples| !samples.is_empty());
            Self {
                test_samples,
                last_test_sample: None,
            }
        }

        #[cfg(not(feature = "test-support"))]
        Self {}
    }

    fn sample(&mut self) -> io::Result<u64> {
        #[cfg(feature = "test-support")]
        if let Some(samples) = self.test_samples.as_mut() {
            let sample_mb = samples
                .pop_front()
                .or(self.last_test_sample)
                .expect("test RSS sequence is non-empty");
            self.last_test_sample = Some(sample_mb);
            return sample_mb
                .checked_mul(crate::config::MEBIBYTE_BYTES)
                .ok_or_else(|| io::Error::other("test peak RSS sample overflowed bytes"));
        }

        peak_rss_bytes()
    }
}

pub(super) fn decision_for_peak_rss(
    peak_rss_bytes: u64,
    thresholds: MemoryThresholds,
) -> MemoryWatchdogDecision {
    if peak_rss_bytes >= thresholds.emergency_bytes {
        return MemoryWatchdogDecision::Abort;
    }
    MemoryWatchdogDecision::Continue
}

#[cfg(unix)]
pub(super) fn peak_rss_bytes() -> io::Result<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let max_rss = unsafe { usage.assume_init() }.ru_maxrss;
    let max_rss = u64::try_from(max_rss)
        .map_err(|_| io::Error::other("getrusage returned a negative peak RSS"))?;

    // macOS reports ru_maxrss in bytes; other Unixes report kibibytes.
    #[cfg(target_os = "macos")]
    return Ok(max_rss);

    #[cfg(not(target_os = "macos"))]
    max_rss
        .checked_mul(1024)
        .ok_or_else(|| io::Error::other("peak RSS overflowed bytes"))
}

#[cfg(windows)]
pub(super) fn peak_rss_bytes() -> io::Result<u64> {
    type Handle = *mut std::ffi::c_void;

    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    unsafe extern "system" {
        fn GetCurrentProcess() -> Handle;
    }

    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(
            process: Handle,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
    }

    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool_usage: 0,
        quota_paged_pool_usage: 0,
        quota_peak_non_paged_pool_usage: 0,
        quota_non_paged_pool_usage: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
    };
    let result = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<ProcessMemoryCounters>() as u32,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    u64::try_from(counters.peak_working_set_size)
        .map_err(|_| io::Error::other("peak working set does not fit in u64"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: u64 = 1024 * 1024 * 1024;

    #[test]
    fn memory_limit_thresholds_use_eighty_five_percent_headroom() {
        let thresholds = MemoryThresholds::from_limit_bytes(LIMIT);

        assert_eq!(thresholds.emergency_bytes, 912_680_551);
        assert_eq!(thresholds.limit_bytes, LIMIT);
    }

    #[test]
    fn watchdog_aborts_at_the_emergency_threshold() {
        let thresholds = MemoryThresholds::from_limit_bytes(LIMIT);
        assert_eq!(
            decision_for_peak_rss(thresholds.emergency_bytes - 1, thresholds),
            MemoryWatchdogDecision::Continue
        );
        assert_eq!(
            decision_for_peak_rss(thresholds.emergency_bytes, thresholds),
            MemoryWatchdogDecision::Abort
        );
    }

    #[test]
    fn watchdog_aborts_at_the_hard_threshold() {
        let thresholds = MemoryThresholds::from_limit_bytes(LIMIT);
        assert_eq!(
            decision_for_peak_rss(thresholds.emergency_bytes - 1, thresholds),
            MemoryWatchdogDecision::Continue
        );
        assert_eq!(
            decision_for_peak_rss(thresholds.limit_bytes, thresholds),
            MemoryWatchdogDecision::Abort
        );
    }

    #[test]
    fn peak_rss_sampler_reports_nonzero_memory() {
        assert!(peak_rss_bytes().expect("peak RSS should be readable") > 0);
    }
}
