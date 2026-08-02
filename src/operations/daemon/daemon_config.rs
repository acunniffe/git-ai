use crate::config;
use crate::error::GitAiError;
use crate::model::repository::lock_file::LockFile;
#[cfg(not(windows))]
use interprocess::local_socket::prelude::*;
#[cfg(windows)]
use named_pipe::PipeClient as WindowsPipeClient;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
#[cfg(windows)]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::time::Duration;

/// Prefix for every Windows named-pipe path the daemon creates.
///
/// The full pipe names are `\\.\pipe\git-ai-<hash16>-trace2` and
/// `\\.\pipe\git-ai-<hash16>-control`.  `revert_trace2_config` checks this
/// prefix to recognise git-ai-owned `trace2.eventTarget` values on Windows.
pub const WINDOWS_PIPE_PREFIX: &str = r"\\.\pipe\git-ai-";

pub const TRACE_INGEST_SEQ_FIELD: &str = "git_ai_ingest_seq";
pub const TRACE_ROOT_ARGV_FIELD: &str = "git_ai_root_argv";
pub const TRACE_ROOT_STARTED_AT_NS_FIELD: &str = "git_ai_root_started_at_ns";
pub const TRACE_ROOT_WORKTREE_FIELD: &str = "git_ai_root_worktree";
pub const TRACE_ROOT_REFLOG_START_OFFSETS_FIELD: &str = "git_ai_root_reflog_start_offsets";
pub const TRACE_CONNECTION_CLOSED_EVENT: &str = "git_ai_connection_closed";
pub const DAEMON_CONTROL_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
pub const DAEMON_CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
pub const DAEMON_CHECKPOINT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);
pub const DAEMON_SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(100);
// Trace2 frames are written synchronously by Git to the daemon's Unix socket.
// With small kernel socket buffers (macOS defaults to ~8 KiB), a bursty trace2
// stream can fill the buffer and block the raw `git` process in `write()` until
// the daemon drains it. A larger receive buffer absorbs those bursts. Starts at
// a conservative 512 KiB and can be raised toward 1 MiB via the env override
// without a code change. This is a mitigation, not a guarantee: any finite
// buffer can still fill if the daemon genuinely stops draining.
#[cfg(not(windows))]
pub const TRACE_SOCKET_RECV_BUFFER_BYTES: usize = 512 * 1024;
pub const TRACE_INGEST_QUEUE_CAPACITY: usize = 16_384;
#[cfg(not(windows))]
pub const TRACE_CONNECTION_BOOTSTRAP_READ_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(windows)]
pub const WINDOWS_TRACE_PIPE_WORKERS: usize = 16;
#[cfg(windows)]
pub const WINDOWS_CONTROL_PIPE_WORKERS: usize = 8;
static DAEMON_PROCESS_ACTIVE: AtomicBool = AtomicBool::new(false);

#[cfg(not(windows))]
pub type DaemonClientStream = LocalSocketStream;

#[cfg(windows)]
pub enum DaemonClientStream {
    WindowsPipe(WindowsPipeClient),
}

#[cfg(windows)]
impl Read for DaemonClientStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::WindowsPipe(stream) => stream.read(buf),
        }
    }
}

#[cfg(windows)]
impl Write for DaemonClientStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::WindowsPipe(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::WindowsPipe(stream) => stream.flush(),
        }
    }
}

pub fn daemon_process_active() -> bool {
    DAEMON_PROCESS_ACTIVE.load(Ordering::SeqCst)
}

/// Result returned by the `await` control request.
#[derive(Debug, Serialize, Deserialize)]
pub struct AwaitResult {
    pub(crate) done: bool,
    pub(crate) timed_out: bool,
    pub(crate) metrics_remaining: usize,
    pub(crate) notes_remaining: usize,
}

pub struct DaemonProcessActiveGuard;

impl DaemonProcessActiveGuard {
    pub(crate) fn enter() -> Self {
        DAEMON_PROCESS_ACTIVE.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for DaemonProcessActiveGuard {
    fn drop(&mut self) {
        DAEMON_PROCESS_ACTIVE.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub internal_dir: PathBuf,
    pub lock_path: PathBuf,
    pub trace_socket_path: PathBuf,
    pub control_socket_path: PathBuf,
}

pub(super) fn test_completion_log_path(dir: &Path, family_key: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(family_key.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    dir.join(format!("{}.jsonl", &digest[..16]))
}

impl DaemonConfig {
    fn from_internal_dir(internal_dir: PathBuf) -> Self {
        let daemon_dir = internal_dir.join("daemon");
        #[cfg(unix)]
        let (lock_path, trace_socket_path, control_socket_path) = {
            let mut lock_path = daemon_dir.join("daemon.lock");
            let mut trace_socket_path = daemon_dir.join("trace2.sock");
            let mut control_socket_path = daemon_dir.join("control.sock");
            let too_long = |path: &Path| path.to_string_lossy().len() >= 100;

            if too_long(&trace_socket_path) || too_long(&control_socket_path) {
                let mut hasher = Sha256::new();
                hasher.update(internal_dir.to_string_lossy().as_bytes());
                let digest = format!("{:x}", hasher.finalize());
                let short = &digest[..16];
                let short_dir = std::env::temp_dir().join(format!("git-ai-d-{}", short));
                lock_path = short_dir.join("daemon.lock");
                trace_socket_path = short_dir.join("trace.sock");
                control_socket_path = short_dir.join("control.sock");
            }

            (lock_path, trace_socket_path, control_socket_path)
        };

        #[cfg(not(unix))]
        let (lock_path, trace_socket_path, control_socket_path) = {
            let mut hasher = Sha256::new();
            hasher.update(internal_dir.to_string_lossy().as_bytes());
            let digest = format!("{:x}", hasher.finalize());
            let short = &digest[..16];
            (
                daemon_dir.join("daemon.lock"),
                PathBuf::from(format!(r"\\.\pipe\git-ai-{}-trace2", short)),
                PathBuf::from(format!(r"\\.\pipe\git-ai-{}-control", short)),
            )
        };

        Self {
            internal_dir,
            lock_path,
            trace_socket_path,
            control_socket_path,
        }
    }

    pub fn from_home(home: &Path) -> Self {
        let internal_dir = home.join(".git-ai").join("internal");
        Self::from_internal_dir(internal_dir)
    }

    pub fn from_default_paths() -> Result<Self, GitAiError> {
        let internal_dir = config::internal_dir_path().ok_or_else(|| {
            GitAiError::Generic("Unable to determine ~/.git-ai/internal path".to_string())
        })?;
        Ok(Self::from_internal_dir(internal_dir))
    }

    pub fn from_env_or_default_paths() -> Result<Self, GitAiError> {
        let mut config = if let Ok(home) = std::env::var("GIT_AI_DAEMON_HOME")
            && !home.trim().is_empty()
        {
            Self::from_home(Path::new(&home))
        } else {
            Self::from_default_paths()?
        };

        if let Ok(path) = std::env::var("GIT_AI_DAEMON_CONTROL_SOCKET")
            && !path.trim().is_empty()
        {
            config.control_socket_path = PathBuf::from(path);
        }

        if let Ok(path) = std::env::var("GIT_AI_DAEMON_TRACE_SOCKET")
            && !path.trim().is_empty()
        {
            config.trace_socket_path = PathBuf::from(path);
        }

        Ok(config)
    }

    pub fn ensure_parent_dirs(&self) -> Result<(), GitAiError> {
        let daemon_dir = self
            .lock_path
            .parent()
            .ok_or_else(|| GitAiError::Generic("daemon lock path has no parent".to_string()))?;
        fs::create_dir_all(daemon_dir)?;
        fs::create_dir_all(&self.internal_dir)?;
        Ok(())
    }

    pub fn trace2_event_target(&self) -> String {
        Self::trace2_event_target_for_path(&self.trace_socket_path)
    }

    pub fn test_completion_log_dir(&self) -> PathBuf {
        self.internal_dir.join("daemon").join("test-completions")
    }

    pub fn test_completion_log_path_for_family(&self, family_key: &str) -> PathBuf {
        test_completion_log_path(&self.test_completion_log_dir(), family_key)
    }

    pub fn trace2_event_target_for_path(path: &Path) -> String {
        #[cfg(unix)]
        {
            format!("af_unix:stream:{}", path.to_string_lossy())
        }
        #[cfg(not(unix))]
        {
            path.to_string_lossy().to_string()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCompletionLogEntry {
    pub(crate) seq: u64,
    pub(crate) family_key: String,
    pub(crate) kind: String,
    pub(crate) primary_command: Option<String>,
    #[serde(default)]
    pub(crate) test_sync_session: Option<String>,
    pub(crate) exit_code: Option<i32>,
    #[serde(default)]
    pub(crate) sync_tracked: bool,
    pub(crate) status: String,
    pub(crate) error: Option<String>,
    /// Machine-readable semantic event kinds the analyzer produced for this
    /// command (e.g. `"CommitCreated"`, `"OpaqueCommand"`). Diagnostic only:
    /// empty for checkpoint entries and for command entries logged by a
    /// daemon binary predating this field (the shared test daemon pool can
    /// replay entries across binary versions mid-migration, so this must
    /// stay optional -- `#[serde(default)]` -- for backward compatibility).
    #[serde(default)]
    pub(crate) semantic_events: Vec<String>,
    /// `new_head` SHAs pulled from `CommitCreated`/`CommitAmended` events
    /// seen for this command, in event order. A `commit`-family command
    /// whose entry has non-empty `semantic_events` but empty `commit_shas`
    /// produced no HEAD-transition event at all -- see `commit_skip_reason`.
    #[serde(default)]
    pub(crate) commit_shas: Vec<String>,
    /// Set when the analyzer's only event for this command was
    /// `OpaqueCommand` (i.e. it fell back to the no-op-classification
    /// default because ref-change enrichment produced no HEAD/branch
    /// transition). Currently the only value is `"opaque_command"`; this is
    /// the reflog-cursor race documented in the daemon-trace2-ingestion
    /// spec, not a note-write or filesystem-visibility problem.
    #[serde(default)]
    pub(crate) commit_skip_reason: Option<String>,
}

pub struct DaemonLock {
    _lock: LockFile,
}

impl DaemonLock {
    pub fn acquire(path: &Path) -> Result<Self, GitAiError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock = LockFile::try_acquire(path).ok_or_else(|| {
            GitAiError::Generic(
                "git-ai background service is already running (lock held)".to_string(),
            )
        })?;
        Ok(Self { _lock: lock })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completion_log_path_has_stable_family_hash() {
        let config = DaemonConfig::from_home(Path::new("test-home"));

        assert_eq!(
            config.test_completion_log_path_for_family("family-key"),
            PathBuf::from("test-home")
                .join(".git-ai/internal/daemon/test-completions")
                .join("02e15fa3779eb41b.jsonl")
        );
    }
}
