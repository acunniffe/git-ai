use std::future::Future;
use std::sync::OnceLock;

use crate::error::GitAiError;

const HELPER_RUNTIME_WORKER_THREADS: usize = 2;
// File-level helpers can fan out to 30 tasks, but activating a thread for every
// task creates one allocator arena per thread. Large checkpoints then multiply
// their high-water memory across those arenas. Queue excess blocking tasks on a
// small pool instead; this work is downstream of trace ingestion.
const HELPER_RUNTIME_MAX_BLOCKING_THREADS: usize = 4;

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

        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(HELPER_RUNTIME_WORKER_THREADS)
            .max_blocking_threads(HELPER_RUNTIME_MAX_BLOCKING_THREADS)
            .thread_keep_alive(std::time::Duration::from_secs(60))
            .enable_all()
            .build()
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
}
