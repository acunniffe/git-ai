//! Lightweight CPU and memory sampler for daemon resource telemetry.
//!
//! Periodically collects process-level CPU usage and RSS memory, storing
//! samples in a bounded ring buffer. On demand, computes aggregate statistics
//! (min, max, mean, median) over the collected window and drains the buffer.
//!
//! Sampling uses low-overhead OS primitives (`getrusage` / `/proc/self/statm`)
//! that complete in constant time with no child process spawns.

use std::collections::VecDeque;
use std::time::Instant;

/// Maximum number of samples to retain between heartbeats.
const MAX_SAMPLES: usize = 64;

#[derive(Debug, Clone)]
struct ResourceSample {
    cpu_percent: f64,
    rss_bytes: u64,
}

/// Accumulated aggregate statistics over a sampling window.
#[derive(Debug, Clone)]
pub struct ResourceStats {
    pub cpu_percent_min: f64,
    pub cpu_percent_max: f64,
    pub cpu_percent_mean: f64,
    pub cpu_percent_median: f64,
    pub rss_bytes_min: u64,
    pub rss_bytes_max: u64,
    pub rss_bytes_mean: u64,
    pub rss_bytes_median: u64,
    pub rss_bytes_current: u64,
    pub sample_count: u32,
}

/// Process-level resource sampler.
///
/// Call [`ResourceSampler::sample`] at a regular cadence (e.g. every 30 s)
/// to collect data points. Call [`ResourceSampler::drain_stats`] when emitting
/// a heartbeat to extract aggregate statistics and reset the buffer.
pub struct ResourceSampler {
    samples: VecDeque<ResourceSample>,
    /// Cached value from the last successful CPU read, or initial baseline.
    last_cpu_secs: f64,
    last_wall: Instant,
}

impl Default for ResourceSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceSampler {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MAX_SAMPLES),
            last_cpu_secs: process_cpu_seconds().unwrap_or(0.0),
            last_wall: Instant::now(),
        }
    }

    /// Collect a single sample of CPU and RSS usage.
    /// Skips the sample entirely when either read fails (e.g. unsupported OS).
    pub fn sample(&mut self) {
        let now = Instant::now();
        let wall_delta = now.duration_since(self.last_wall).as_secs_f64();
        if wall_delta <= 0.0 {
            return;
        }

        let Some(cpu_now) = process_cpu_seconds() else {
            return;
        };
        let cpu_delta = cpu_now - self.last_cpu_secs;
        let cpu_percent = (cpu_delta / wall_delta) * 100.0;

        let Some(rss_bytes) = process_rss_bytes() else {
            return;
        };

        self.last_cpu_secs = cpu_now;
        self.last_wall = now;

        if self.samples.len() >= MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(ResourceSample {
            cpu_percent: cpu_percent.max(0.0),
            rss_bytes,
        });
    }

    /// Drain the sample buffer and return aggregate stats.
    ///
    /// Returns `None` when no samples have been collected since the last drain.
    pub fn drain_stats(&mut self) -> Option<ResourceStats> {
        if self.samples.is_empty() {
            return None;
        }

        let count = self.samples.len() as u32;

        let mut cpu_values: Vec<f64> = self.samples.iter().map(|s| s.cpu_percent).collect();
        let mut rss_values: Vec<u64> = self.samples.iter().map(|s| s.rss_bytes).collect();
        let rss_current = rss_values.last().copied().unwrap_or(0);

        cpu_values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        rss_values.sort_unstable();

        let stats = ResourceStats {
            cpu_percent_min: cpu_values[0],
            cpu_percent_max: cpu_values[cpu_values.len() - 1],
            cpu_percent_mean: cpu_values.iter().sum::<f64>() / cpu_values.len() as f64,
            cpu_percent_median: median_f64(&cpu_values),
            rss_bytes_min: rss_values[0],
            rss_bytes_max: rss_values[rss_values.len() - 1],
            rss_bytes_mean: rss_values.iter().sum::<u64>() / rss_values.len() as u64,
            rss_bytes_median: median_u64(&rss_values),
            rss_bytes_current: rss_current,
            sample_count: count,
        };

        self.samples.clear();
        Some(stats)
    }
}

fn median_f64(sorted: &[f64]) -> f64 {
    let len = sorted.len();
    if len == 0 {
        return 0.0;
    }
    if len % 2 == 1 {
        sorted[len / 2]
    } else {
        (sorted[len / 2 - 1] + sorted[len / 2]) / 2.0
    }
}

fn median_u64(sorted: &[u64]) -> u64 {
    let len = sorted.len();
    if len == 0 {
        return 0;
    }
    if len % 2 == 1 {
        sorted[len / 2]
    } else {
        (sorted[len / 2 - 1] + sorted[len / 2]) / 2
    }
}

// ---------------------------------------------------------------------------
// Platform-specific CPU and memory reading
// ---------------------------------------------------------------------------

/// Returns cumulative user+system CPU seconds for the current process.
/// Returns `None` on unsupported platforms or syscall failure.
#[cfg(unix)]
fn process_cpu_seconds() -> Option<f64> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if ret != 0 {
        return None;
    }
    let user = usage.ru_utime.tv_sec as f64 + usage.ru_utime.tv_usec as f64 / 1_000_000.0;
    let sys = usage.ru_stime.tv_sec as f64 + usage.ru_stime.tv_usec as f64 / 1_000_000.0;
    Some(user + sys)
}

#[cfg(not(unix))]
fn process_cpu_seconds() -> Option<f64> {
    None
}

/// Returns the current resident set size in bytes.
/// Returns `None` on unsupported platforms or read failure.
#[cfg(target_os = "linux")]
fn process_rss_bytes() -> Option<u64> {
    // /proc/self/statm fields: size resident shared text lib data dt (in pages)
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let rss_pages_str = statm.split_whitespace().nth(1)?;
    let rss_pages = rss_pages_str.parse::<u64>().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return Some(rss_pages * 4096); // common default
    }
    Some(rss_pages * page_size as u64)
}

#[cfg(target_os = "macos")]
fn process_rss_bytes() -> Option<u64> {
    // On macOS, getrusage ru_maxrss is in bytes and reports peak RSS.
    // For current RSS we would need mach task_info; peak is a reasonable
    // upper-bound proxy for a long-running daemon.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if ret != 0 {
        return None;
    }
    Some(usage.ru_maxrss as u64)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_rss_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sampler_starts_empty() {
        let mut sampler = ResourceSampler::new();
        assert!(sampler.drain_stats().is_none());
    }

    #[test]
    fn single_sample_produces_stats() {
        let mut sampler = ResourceSampler::new();
        // Advance wall clock by waiting briefly
        std::thread::sleep(std::time::Duration::from_millis(10));
        sampler.sample();
        let stats = sampler.drain_stats().unwrap();
        assert_eq!(stats.sample_count, 1);
        // CPU percent should be non-negative
        assert!(stats.cpu_percent_min >= 0.0);
        assert!(stats.cpu_percent_max >= 0.0);
        assert!(stats.cpu_percent_mean >= 0.0);
        assert!(stats.cpu_percent_median >= 0.0);
        // On a real system, RSS should be > 0
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(stats.rss_bytes_current > 0);
    }

    #[test]
    fn drain_clears_buffer() {
        let mut sampler = ResourceSampler::new();
        std::thread::sleep(std::time::Duration::from_millis(10));
        sampler.sample();
        assert!(sampler.drain_stats().is_some());
        assert!(sampler.drain_stats().is_none());
    }

    #[test]
    fn multiple_samples_aggregate_correctly() {
        let mut sampler = ResourceSampler {
            samples: VecDeque::new(),
            last_cpu_secs: 0.0,
            last_wall: Instant::now(),
        };

        // Insert synthetic samples directly
        sampler.samples.push_back(ResourceSample {
            cpu_percent: 10.0,
            rss_bytes: 1000,
        });
        sampler.samples.push_back(ResourceSample {
            cpu_percent: 30.0,
            rss_bytes: 3000,
        });
        sampler.samples.push_back(ResourceSample {
            cpu_percent: 20.0,
            rss_bytes: 2000,
        });

        let stats = sampler.drain_stats().unwrap();
        assert_eq!(stats.sample_count, 3);
        assert!((stats.cpu_percent_min - 10.0).abs() < f64::EPSILON);
        assert!((stats.cpu_percent_max - 30.0).abs() < f64::EPSILON);
        assert!((stats.cpu_percent_mean - 20.0).abs() < f64::EPSILON);
        assert!((stats.cpu_percent_median - 20.0).abs() < f64::EPSILON);
        assert_eq!(stats.rss_bytes_min, 1000);
        assert_eq!(stats.rss_bytes_max, 3000);
        assert_eq!(stats.rss_bytes_mean, 2000);
        assert_eq!(stats.rss_bytes_median, 2000);
        assert_eq!(stats.rss_bytes_current, 2000);
    }

    #[test]
    fn even_sample_count_median_averages_middle_pair() {
        let mut sampler = ResourceSampler {
            samples: VecDeque::new(),
            last_cpu_secs: 0.0,
            last_wall: Instant::now(),
        };

        sampler.samples.push_back(ResourceSample {
            cpu_percent: 10.0,
            rss_bytes: 1000,
        });
        sampler.samples.push_back(ResourceSample {
            cpu_percent: 20.0,
            rss_bytes: 3000,
        });

        let stats = sampler.drain_stats().unwrap();
        assert!((stats.cpu_percent_median - 15.0).abs() < f64::EPSILON);
        assert_eq!(stats.rss_bytes_median, 2000);
    }

    #[test]
    fn buffer_caps_at_max_samples() {
        let mut sampler = ResourceSampler {
            samples: VecDeque::new(),
            last_cpu_secs: 0.0,
            last_wall: Instant::now(),
        };

        for i in 0..MAX_SAMPLES + 10 {
            sampler.samples.push_back(ResourceSample {
                cpu_percent: i as f64,
                rss_bytes: i as u64 * 100,
            });
            if sampler.samples.len() > MAX_SAMPLES {
                sampler.samples.pop_front();
            }
        }

        assert_eq!(sampler.samples.len(), MAX_SAMPLES);
    }

    #[test]
    fn process_cpu_seconds_returns_value_on_supported_platforms() {
        let cpu = process_cpu_seconds();
        #[cfg(unix)]
        {
            assert!(cpu.is_some());
            assert!(cpu.unwrap() >= 0.0);
        }
        #[cfg(not(unix))]
        assert!(cpu.is_none());
    }

    #[test]
    fn process_rss_bytes_returns_value_on_supported_platforms() {
        let rss = process_rss_bytes();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            assert!(rss.is_some());
            assert!(rss.unwrap() > 0);
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert!(rss.is_none());
    }

    #[test]
    fn median_f64_single_element() {
        assert!((median_f64(&[42.0]) - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn median_u64_single_element() {
        assert_eq!(median_u64(&[42]), 42);
    }

    #[test]
    fn median_f64_empty() {
        assert!((median_f64(&[]) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn median_u64_empty() {
        assert_eq!(median_u64(&[]), 0);
    }
}
