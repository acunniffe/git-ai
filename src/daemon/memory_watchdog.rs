use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use crate::api::types::{DaemonLogEvent, DaemonLogFieldValue, DaemonLogKind, DaemonLogLevel};

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
    let mut sampler = RssSampler::new();
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

        let rss_bytes = match sampler.sample() {
            Ok(bytes) => {
                if measurement_failed {
                    tracing::info!("daemon RSS measurement recovered");
                    measurement_failed = false;
                }
                bytes
            }
            Err(error) => {
                if !measurement_failed {
                    tracing::warn!(%error, "failed measuring daemon RSS; watchdog will retry");
                    measurement_failed = true;
                }
                continue;
            }
        };

        match decision_for_rss(rss_bytes, thresholds) {
            MemoryWatchdogDecision::Continue => {}
            MemoryWatchdogDecision::Abort => {
                record_memory_emergency(rss_bytes, thresholds, "abort");
                std::process::abort();
            }
        }
    }
}

fn record_memory_emergency(rss_bytes: u64, thresholds: MemoryThresholds, action: &'static str) {
    // Lifetime high-water mark for context; the DECISION is made on current
    // RSS. Deciding on the peak condemned the process after one transient
    // spike even when the memory had already been freed (#2244).
    let peak_rss = peak_rss_bytes().unwrap_or(rss_bytes);
    tracing::error!(
        rss_bytes,
        peak_rss_bytes = peak_rss,
        memory_emergency_threshold_bytes = thresholds.emergency_bytes,
        memory_limit_bytes = thresholds.limit_bytes,
        action,
        "daemon memory emergency threshold reached"
    );
    eprintln!(
        "[git-ai] daemon memory emergency threshold reached (current RSS {rss_bytes} bytes, peak RSS {peak_rss} bytes, emergency threshold {} bytes, hard limit {} bytes); {action}ing immediately without draining",
        thresholds.emergency_bytes, thresholds.limit_bytes
    );
    let _ = io::stderr().flush();

    let mut fields = BTreeMap::new();
    fields.insert(
        "rss_bytes".to_string(),
        DaemonLogFieldValue::from(rss_bytes),
    );
    fields.insert(
        "peak_rss_bytes".to_string(),
        DaemonLogFieldValue::from(peak_rss),
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

struct RssSampler {
    #[cfg(feature = "test-support")]
    test_samples: Option<std::collections::VecDeque<u64>>,
    #[cfg(feature = "test-support")]
    last_test_sample: Option<u64>,
}

impl RssSampler {
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
                .ok_or_else(|| io::Error::other("test RSS sample overflowed bytes"));
        }

        current_rss_bytes()
    }
}

pub(super) fn decision_for_rss(
    rss_bytes: u64,
    thresholds: MemoryThresholds,
) -> MemoryWatchdogDecision {
    if rss_bytes >= thresholds.emergency_bytes {
        return MemoryWatchdogDecision::Abort;
    }
    MemoryWatchdogDecision::Continue
}

/// Current resident set size. The watchdog decides on THIS, not on
/// [`peak_rss_bytes`]: `getrusage`'s `ru_maxrss` is a monotonic lifetime
/// high-water mark, so one transient allocation spike would otherwise
/// condemn the process on every later poll even after the memory was
/// freed (#2244).
#[cfg(target_os = "macos")]
pub(super) fn current_rss_bytes() -> io::Result<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            std::process::id() as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written <= 0 {
        return Err(io::Error::last_os_error());
    }
    if written != size {
        return Err(io::Error::other("short proc_pidinfo read"));
    }
    Ok(unsafe { info.assume_init() }.pti_resident_size)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn current_rss_bytes() -> io::Result<u64> {
    // /proc/self/statm: second field is resident pages.
    let statm = std::fs::read_to_string("/proc/self/statm")?;
    let resident_pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| io::Error::other("unparseable /proc/self/statm"))?;
    let page_size = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .map_err(|_| io::Error::other("negative page size"))?;
    resident_pages
        .checked_mul(page_size)
        .ok_or_else(|| io::Error::other("current RSS overflowed bytes"))
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

    #[cfg(target_os = "macos")]
    return Ok(max_rss);

    #[cfg(not(target_os = "macos"))]
    max_rss
        .checked_mul(1024)
        .ok_or_else(|| io::Error::other("peak RSS overflowed bytes"))
}

#[cfg(windows)]
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

#[cfg(windows)]
pub(super) fn peak_rss_bytes() -> io::Result<u64> {
    let counters = process_memory_counters()?;
    u64::try_from(counters.peak_working_set_size)
        .map_err(|_| io::Error::other("peak working set does not fit in u64"))
}

/// See the unix variant for why the watchdog decides on current RSS (#2244).
#[cfg(windows)]
pub(super) fn current_rss_bytes() -> io::Result<u64> {
    let counters = process_memory_counters()?;
    u64::try_from(counters.working_set_size)
        .map_err(|_| io::Error::other("working set does not fit in u64"))
}

#[cfg(windows)]
fn process_memory_counters() -> io::Result<ProcessMemoryCounters> {
    type Handle = *mut std::ffi::c_void;

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
    Ok(counters)
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
            decision_for_rss(thresholds.emergency_bytes - 1, thresholds),
            MemoryWatchdogDecision::Continue
        );
        assert_eq!(
            decision_for_rss(thresholds.emergency_bytes, thresholds),
            MemoryWatchdogDecision::Abort
        );
    }

    #[test]
    fn watchdog_aborts_when_startup_is_already_high() {
        let thresholds = MemoryThresholds::from_limit_bytes(LIMIT);
        assert_eq!(
            decision_for_rss(thresholds.emergency_bytes, thresholds),
            MemoryWatchdogDecision::Abort
        );
    }

    #[test]
    fn watchdog_aborts_at_the_hard_threshold() {
        let thresholds = MemoryThresholds::from_limit_bytes(LIMIT);
        assert_eq!(
            decision_for_rss(thresholds.emergency_bytes - 1, thresholds),
            MemoryWatchdogDecision::Continue
        );
        assert_eq!(
            decision_for_rss(thresholds.limit_bytes, thresholds),
            MemoryWatchdogDecision::Abort
        );
    }

    #[test]
    fn peak_rss_sampler_reports_nonzero_memory() {
        assert!(peak_rss_bytes().expect("peak RSS should be readable") > 0);
    }

    #[test]
    fn current_rss_reports_nonzero_and_at_most_peak() {
        let current = current_rss_bytes().expect("current RSS should be readable");
        let peak = peak_rss_bytes().expect("peak RSS should be readable");
        assert!(current > 0);
        // The lifetime high-water mark can never be below the current value.
        assert!(current <= peak, "current {current} > peak {peak}");
    }
}
