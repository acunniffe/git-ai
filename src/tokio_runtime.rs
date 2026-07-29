use std::future::Future;
use std::sync::OnceLock;

use crate::error::GitAiError;

const HELPER_RUNTIME_WORKER_THREADS: usize = 2;
// File-level helpers can fan out to 30 tasks, but activating a thread for every
// task creates one allocator arena per thread. Large checkpoints then multiply
// their high-water memory across those arenas. Queue excess blocking tasks on a
// small pool instead; this work is downstream of trace ingestion.
const HELPER_RUNTIME_MAX_BLOCKING_THREADS: usize = 4;
// The daemon's own queues already bound useful async concurrency. Keeping this
// runtime small prevents CPU-count-sized worker and allocator arena growth.
const DAEMON_RUNTIME_WORKER_THREADS: usize = 4;
const DAEMON_RUNTIME_MAX_BLOCKING_THREADS: usize = 16;
const BLOCKING_THREAD_KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(10);

/// Constrain glibc's per-thread allocation arenas before the daemon creates
/// worker threads. Large checkpoint buffers otherwise leave hundreds of MiB
/// resident in arenas that glibc keeps for the lifetime of the daemon.
pub(crate) fn configure_daemon_allocator() -> Result<(), String> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        const DEFAULT_ARENA_MAX: i32 = 2;
        let arena_max = std::env::var("MALLOC_ARENA_MAX")
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|value| *value > 0)
            .map_or(DEFAULT_ARENA_MAX, |value| value.min(DEFAULT_ARENA_MAX));

        // SAFETY: mallopt is process-global and this runs before either Tokio
        // runtime creates worker threads. M_ARENA_MAX accepts a positive int.
        if unsafe { libc::mallopt(libc::M_ARENA_MAX, arena_max) } == 0 {
            return Err("failed to configure glibc allocator arena limit".to_string());
        }

        #[cfg(feature = "test-support")]
        if let Some(path) = std::env::var_os("GIT_AI_TEST_ALLOCATOR_POLICY_LOG") {
            use std::io::Write;

            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("failed opening test allocator policy log");
            writeln!(log, "arena_max={arena_max}")
                .expect("failed writing test allocator policy log");
        }
    }

    Ok(())
}

fn build_bounded_runtime(
    worker_threads: usize,
    max_blocking_threads: usize,
) -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .max_blocking_threads(max_blocking_threads)
        .thread_keep_alive(BLOCKING_THREAD_KEEP_ALIVE)
        .enable_all()
        .build()
        .map_err(|err| err.to_string())
}

pub(crate) fn build_daemon_runtime() -> Result<tokio::runtime::Runtime, String> {
    #[cfg(feature = "test-support")]
    let worker_threads = std::env::var("GIT_AI_TEST_DAEMON_RUNTIME_WORKER_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DAEMON_RUNTIME_WORKER_THREADS);
    #[cfg(not(feature = "test-support"))]
    let worker_threads = DAEMON_RUNTIME_WORKER_THREADS;

    build_bounded_runtime(worker_threads, DAEMON_RUNTIME_MAX_BLOCKING_THREADS)
}

// Post-commit attribution calls this helper from inside the daemon runtime.
// Recreating a CPU-sized runtime for every call leaves allocator arenas at
// their high-water marks in the long-lived daemon, so keep one small pool.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        #[cfg(feature = "test-support")]
        if let Some(path) = std::env::var_os("GIT_AI_TEST_TOKIO_RUNTIME_BUILD_LOG") {
            use std::io::Write;

            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("failed opening test Tokio runtime build log");
            writeln!(log, "runtime").expect("failed writing test Tokio runtime build log");
        }

        build_bounded_runtime(
            HELPER_RUNTIME_WORKER_THREADS,
            HELPER_RUNTIME_MAX_BLOCKING_THREADS,
        )
        .expect("failed to create Tokio runtime")
    })
}

pub fn initialize() {
    let _ = runtime();
}

pub fn block_on<F>(future: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            scope
                .spawn(move || runtime().block_on(future))
                .join()
                .expect("Tokio helper thread panicked")
        })
    } else {
        runtime().block_on(future)
    }
}

pub async fn spawn_blocking_result<F, T>(task: F) -> Result<T, GitAiError>
where
    F: FnOnce() -> Result<T, GitAiError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|err| GitAiError::Generic(format!("Tokio blocking task failed: {err}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn peak_blocking_concurrency(
        runtime: &tokio::runtime::Runtime,
        tasks: usize,
        expected_limit: usize,
    ) -> usize {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));

        runtime.block_on(async {
            let mut handles = Vec::new();
            for _ in 0..tasks {
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                let release = Arc::clone(&release);
                handles.push(tokio::task::spawn_blocking(move || {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(current, Ordering::SeqCst);
                    while !release.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                }));
            }

            let deadline = Instant::now() + Duration::from_secs(2);
            while peak.load(Ordering::SeqCst) < expected_limit && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            release.store(true, Ordering::SeqCst);
            for handle in handles {
                handle.await.unwrap();
            }
        });

        peak.load(Ordering::SeqCst)
    }

    #[test]
    fn helper_runtime_worker_pool_is_bounded() {
        assert_eq!(
            runtime().metrics().num_workers(),
            HELPER_RUNTIME_WORKER_THREADS
        );
    }

    #[test]
    fn helper_runtime_is_reused() {
        assert!(std::ptr::eq(runtime(), runtime()));
    }

    #[test]
    fn helper_runtime_blocking_pool_is_memory_bounded() {
        assert_eq!(
            peak_blocking_concurrency(runtime(), 8, 4),
            4,
            "helper runtime must activate exactly four blocking threads under load"
        );
    }

    #[test]
    fn daemon_runtime_worker_pool_is_bounded() {
        assert_eq!(
            build_daemon_runtime().unwrap().metrics().num_workers(),
            DAEMON_RUNTIME_WORKER_THREADS
        );
    }

    #[test]
    fn daemon_runtime_blocking_pool_is_memory_bounded() {
        let runtime = build_daemon_runtime().unwrap();
        assert_eq!(
            peak_blocking_concurrency(&runtime, 20, DAEMON_RUNTIME_MAX_BLOCKING_THREADS),
            DAEMON_RUNTIME_MAX_BLOCKING_THREADS,
            "daemon runtime must enforce the shared blocking-thread policy"
        );
    }
}
