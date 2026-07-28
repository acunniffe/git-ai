//! Working-log trace: an append-only side-channel for debugging daemon
//! concurrency and attribution loss in tests.
//!
//! When the `GIT_AI_WLTRACE` environment variable names a file, every call
//! appends one line tagging timestamp, pid, thread, operation, and path, so a
//! post-run script can reconstruct per-working-log timelines across the test
//! process, CLI subprocesses, and the daemon. This is how the held-exec-lock
//! GC eviction race was pinned: overlapping `drain.exec` windows and torn
//! `checkpoints.jsonl` reads are invisible in ordinary logs but obvious here.
//!
//! Compiled only with the `test-support` feature; release builds get a no-op.
//! With the feature on but the env var unset, the cost per call is one atomic
//! load (the closure is never invoked).

#[cfg(feature = "test-support")]
mod imp {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    fn trace_path() -> Option<&'static PathBuf> {
        static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
        PATH.get_or_init(|| std::env::var_os("GIT_AI_WLTRACE").map(PathBuf::from))
            .as_ref()
    }

    pub fn wltrace(op: &str, path: &Path, detail: impl FnOnce() -> String) {
        let Some(trace_path) = trace_path() else {
            return;
        };
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let thread = std::thread::current();
        let line = format!(
            "{} pid={} tid={:?} tname={} op={} path={} {}\n",
            ts,
            std::process::id(),
            thread.id(),
            thread.name().unwrap_or("-").replace(' ', "_"),
            op,
            path.display(),
            detail()
        );
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(trace_path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

#[cfg(not(feature = "test-support"))]
mod imp {
    use std::path::Path;

    #[inline(always)]
    pub fn wltrace(_op: &str, _path: &Path, _detail: impl FnOnce() -> String) {}
}

pub use imp::wltrace;
