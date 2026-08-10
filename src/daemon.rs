use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::checkpoint_content_budget::CheckpointContentBudget;
use crate::config;
use crate::daemon::git_backend::GitBackend;
use crate::error::GitAiError;
use crate::git::cli_parser::{
    ParsedGitInvocation, explicit_rebase_branch_arg, parse_git_cli_args, summarize_rebase_args,
};
use crate::git::find_repository_in_path;
use crate::git::repo_state::{
    common_dir_for_worktree, git_dir_for_worktree, worktree_root_for_path,
};
use crate::git::repository::{
    Repository, discover_repository_in_path_no_git_exec, exec_git, exec_git_stdin,
};
use crate::git::sync_authorship::{fetch_authorship_notes, fetch_remote_from_args};
use crate::utils::LockFile;
use crate::{
    authorship::working_log::CheckpointKind,
    commands::checkpoint_agent::orchestrator::CheckpointRequest,
    daemon::checkpoint::PreparedPathRole,
};
use futures::{StreamExt, stream};
#[cfg(not(windows))]
use interprocess::local_socket::ConnectOptions;
#[cfg(not(windows))]
use interprocess::{
    ConnectWaitMode,
    local_socket::{GenericFilePath, ListenerOptions, Name, prelude::*},
};
#[cfg(windows)]
use named_pipe::{
    ConnectingServer as WindowsConnectingServer, OpenMode as WindowsPipeOpenMode,
    PipeClient as WindowsPipeClient, PipeOptions as WindowsPipeOptions,
    PipeServer as WindowsPipeServer,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(not(windows))]
use std::os::fd::{AsFd, AsRawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore, mpsc};
use tokio::time::Duration;

pub mod analyzers;
pub mod bash_history_db;
pub mod bash_sessions;
pub mod checkpoint;
pub mod control_api;
pub mod coordinator;
pub mod daemon_log_layer;
pub mod domain;
pub mod family_actor;
pub mod git_backend;
pub mod global_actor;
mod memory_watchdog;
pub mod reducer;
pub mod ref_cursor;
pub mod rewrite_metrics;
pub mod sentry_layer;
pub mod stream_worker;
pub mod sweep_coordinator;
pub mod telemetry_handle;
pub mod telemetry_worker;
pub mod test_sync;
pub mod trace_normalizer;
pub mod transcript_redaction;

pub use control_api::{
    BashSessionQueryResponse, BashSnapshotQueryResponse, ControlRequest, ControlResponse,
    FamilyStatus, TelemetryEnvelope,
};

const PID_META_FILE: &str = "daemon.pid.json";
const TRACE_INGEST_SEQ_FIELD: &str = "git_ai_ingest_seq";
const TRACE_ROOT_ARGV_FIELD: &str = "git_ai_root_argv";
const TRACE_ROOT_STARTED_AT_NS_FIELD: &str = "git_ai_root_started_at_ns";
const TRACE_ROOT_WORKTREE_FIELD: &str = "git_ai_root_worktree";
pub(crate) const TRACE_ROOT_REFLOG_START_OFFSETS_FIELD: &str = "git_ai_root_reflog_start_offsets";
const TRACE_CONNECTION_CLOSED_EVENT: &str = "git_ai_connection_closed";
const DAEMON_CONTROL_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const DAEMON_CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const DAEMON_CONTROL_RECEIVE_TIMEOUT: Duration = Duration::from_secs(2);
const DAEMON_CONTROL_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const DAEMON_CHECKPOINT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);
const DAEMON_SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(100);
const CHECKPOINT_INGRESS_REQUEST_LIMIT: usize = 1_024;
const CHECKPOINT_INGRESS_BYTE_LIMIT: usize = 64 * 1024 * 1024;
const CHECKPOINT_FAMILY_DRAIN_CONCURRENCY: usize = 2;
// Trace2 frames are written synchronously by Git to the daemon's Unix socket.
// With small kernel socket buffers (macOS defaults to ~8 KiB), a bursty trace2
// stream can fill the buffer and block the raw `git` process in `write()` until
// the daemon drains it. A larger receive buffer absorbs those bursts. Starts at
// a conservative 512 KiB and can be raised toward 1 MiB via the env override
// without a code change. This is a mitigation, not a guarantee: any finite
// buffer can still fill if the daemon genuinely stops draining.
#[cfg(not(windows))]
const TRACE_SOCKET_RECV_BUFFER_BYTES: usize = 512 * 1024;
const TRACE_INGEST_QUEUE_CAPACITY: usize = 16_384;
#[cfg(not(windows))]
const TRACE_CONNECTION_BOOTSTRAP_READ_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(windows)]
const WINDOWS_TRACE_PIPE_WORKERS: usize = 16;
#[cfg(windows)]
const WINDOWS_CONTROL_PIPE_WORKERS: usize = 8;
#[cfg(windows)]
const WINDOWS_STDOUT_HANDLE: u32 = (-11i32) as u32;
#[cfg(windows)]
const WINDOWS_STDERR_HANDLE: u32 = (-12i32) as u32;
static DAEMON_PROCESS_ACTIVE: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Default)]
struct CheckpointIngressQuotaState {
    requests: usize,
    bytes: usize,
}

#[derive(Debug)]
struct CheckpointIngressQuota {
    request_limit: usize,
    byte_limit: usize,
    state: Mutex<CheckpointIngressQuotaState>,
}

#[derive(Debug)]
struct CheckpointIngressQuotaError {
    reason: &'static str,
    requested_bytes: usize,
    outstanding_requests: usize,
    outstanding_bytes: usize,
    request_limit: usize,
    byte_limit: usize,
}

#[derive(Debug)]
struct CheckpointIngressReservation {
    quota: Arc<CheckpointIngressQuota>,
    body_bytes: usize,
}

#[derive(Debug)]
struct AcceptedCheckpoint {
    receipt_seq: u64,
    received_at_ns: u128,
    trace_ingest_target: u64,
    body: Vec<u8>,
    reservation: CheckpointIngressReservation,
}

#[derive(Debug)]
struct PreparedCheckpointAdmission {
    receipt_seq: u64,
    received_at_ns: u128,
    family: String,
    request: CheckpointRequest,
    reservation: CheckpointIngressReservation,
}

impl CheckpointIngressQuota {
    fn new(request_limit: usize, byte_limit: usize) -> Self {
        Self {
            request_limit,
            byte_limit,
            state: Mutex::new(CheckpointIngressQuotaState::default()),
        }
    }

    fn reserve(
        self: &Arc<Self>,
        body_bytes: usize,
    ) -> Result<CheckpointIngressReservation, CheckpointIngressQuotaError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let reason = if state.requests >= self.request_limit {
            Some("request_limit")
        } else if body_bytes > self.byte_limit.saturating_sub(state.bytes) {
            Some("byte_limit")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(CheckpointIngressQuotaError {
                reason,
                requested_bytes: body_bytes,
                outstanding_requests: state.requests,
                outstanding_bytes: state.bytes,
                request_limit: self.request_limit,
                byte_limit: self.byte_limit,
            });
        }

        state.requests += 1;
        state.bytes += body_bytes;
        Ok(CheckpointIngressReservation {
            quota: Arc::clone(self),
            body_bytes,
        })
    }

    fn outstanding(&self) -> (usize, usize) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (state.requests, state.bytes)
    }
}

impl CheckpointIngressReservation {
    fn body_bytes(&self) -> usize {
        self.body_bytes
    }
}

impl Drop for CheckpointIngressReservation {
    fn drop(&mut self) {
        let mut state = self
            .quota
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.requests = state.requests.saturating_sub(1);
        state.bytes = state.bytes.saturating_sub(self.body_bytes);
    }
}

#[cfg(windows)]
unsafe extern "system" {
    fn SetStdHandle(nstdhandle: u32, hhandle: *mut std::ffi::c_void) -> i32;
}

#[cfg(not(windows))]
pub type DaemonClientStream = LocalSocketStream;

#[cfg(windows)]
pub enum DaemonClientStream {
    WindowsPipe(WindowsPipeClient),
}

#[cfg(windows)]
impl Read for DaemonClientStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::WindowsPipe(stream) => stream.read(buf),
        }
    }
}

#[cfg(windows)]
impl Write for DaemonClientStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::WindowsPipe(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
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
struct AwaitResult {
    done: bool,
    timed_out: bool,
    metrics_remaining: usize,
    notes_remaining: usize,
}

struct DaemonProcessActiveGuard;

impl DaemonProcessActiveGuard {
    fn enter() -> Self {
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
        let mut hasher = Sha256::new();
        hasher.update(family_key.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        self.test_completion_log_dir()
            .join(format!("{}.jsonl", &digest[..16]))
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
struct DaemonPidMeta {
    pid: u32,
    started_at_ns: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TestCompletionLogEntry {
    seq: u64,
    family_key: String,
    kind: String,
    primary_command: Option<String>,
    #[serde(default)]
    test_sync_session: Option<String>,
    exit_code: Option<i32>,
    #[serde(default)]
    sync_tracked: bool,
    status: String,
    error: Option<String>,
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

fn is_trace_payload(payload: &Value) -> bool {
    payload.get("event").and_then(Value::as_str).is_some()
}

fn trace_root_sid(sid: &str) -> &str {
    sid.split('/').next().unwrap_or(sid)
}

fn is_terminal_root_trace_event(event: &str, sid: &str, root: &str) -> bool {
    sid == root && event == "atexit"
}

fn daemon_worktree_from_repo_path(repo_path: &Path) -> Option<PathBuf> {
    if repo_path.file_name().and_then(|name| name.to_str()) == Some(".git") {
        return repo_path.parent().map(PathBuf::from);
    }

    let linked_gitdir_file = repo_path.join("gitdir");
    if linked_gitdir_file.is_file() {
        let content = fs::read_to_string(&linked_gitdir_file).ok()?;
        let linked = PathBuf::from(content.trim());
        if linked.file_name().and_then(|name| name.to_str()) == Some(".git") {
            return linked.parent().map(PathBuf::from);
        }
    }

    None
}

fn trace_payload_worktree_hint(payload: &Value) -> Option<PathBuf> {
    let normalize = |path: PathBuf| worktree_root_for_path(&path).unwrap_or(path);
    let argv = trace_payload_argv(payload);
    let event = payload
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event == "def_repo" {
        // Secondary repositories (repo index > 1, e.g. an embedded subrepo git
        // peeked into during a commit) must not hint the command's worktree.
        if crate::daemon::trace_normalizer::def_repo_is_secondary(payload) {
            return None;
        }
        if let Some(path) = payload
            .get("worktree")
            .or_else(|| payload.get("repo_working_dir"))
            .and_then(Value::as_str)
        {
            return Some(normalize(PathBuf::from(path)));
        }
        if let Some(repo_path) = payload.get("repo").and_then(Value::as_str) {
            let candidate = PathBuf::from(repo_path);
            if let Some(worktree) = daemon_worktree_from_repo_path(&candidate) {
                return Some(normalize(worktree));
            }
        }
    }
    if let Some(path) = payload.get("worktree").and_then(Value::as_str) {
        return Some(normalize(PathBuf::from(path)));
    }
    if let Some(path) = payload
        .get(TRACE_ROOT_WORKTREE_FIELD)
        .and_then(Value::as_str)
    {
        return Some(normalize(PathBuf::from(path)));
    }
    if let Some(cwd) = payload.get("cwd").and_then(Value::as_str)
        && let Some(base_dir) = trace_payload_command_base_dir(payload, &argv, Path::new(cwd))
    {
        return Some(normalize(base_dir));
    }
    let parsed = parse_git_cli_args(trace_invocation_args(&argv));
    let mut idx = 0usize;
    while idx < parsed.global_args.len() {
        let token = &parsed.global_args[idx];
        if token == "-C" {
            let path_arg = parsed.global_args.get(idx + 1)?;
            let candidate = PathBuf::from(path_arg);
            if candidate.is_absolute() {
                return Some(normalize(candidate));
            }
            return None;
        }
        if let Some(path_arg) = token.strip_prefix("-C")
            && !path_arg.is_empty()
        {
            let candidate = PathBuf::from(path_arg);
            if candidate.is_absolute() {
                return Some(normalize(candidate));
            }
            return None;
        }
        idx += 1;
    }
    if argv.is_empty() {
        return None;
    }
    None
}

fn trace_payload_command_base_dir(
    _payload: &Value,
    argv: &[String],
    cwd: &Path,
) -> Option<PathBuf> {
    let parsed = parse_git_cli_args(trace_invocation_args(argv));
    let mut base = cwd.to_path_buf();
    let mut idx = 0usize;

    while idx < parsed.global_args.len() {
        let token = &parsed.global_args[idx];

        if token == "-C" {
            let path_arg = parsed.global_args.get(idx + 1)?;
            let next_base = PathBuf::from(path_arg);
            base = if next_base.is_absolute() {
                next_base
            } else {
                base.join(next_base)
            };
            idx += 2;
            continue;
        }

        if let Some(path_arg) = token.strip_prefix("-C") {
            let next_base = PathBuf::from(path_arg);
            base = if next_base.is_absolute() {
                next_base
            } else {
                base.join(next_base)
            };
            idx += 1;
            continue;
        }

        idx += 1;
    }

    Some(base)
}

fn trace_payload_time_ns(payload: &Value) -> Option<u128> {
    payload
        .get("time")
        .and_then(Value::as_str)
        .and_then(rfc3339_to_unix_nanos)
        .or_else(|| {
            payload
                .get("time_ns")
                .and_then(Value::as_u64)
                .map(u128::from)
        })
        .or_else(|| payload.get("ts").and_then(Value::as_u64).map(u128::from))
        .or_else(|| {
            payload
                .get("t_abs")
                .and_then(Value::as_f64)
                .and_then(|seconds| {
                    if seconds.is_sign_negative() {
                        None
                    } else {
                        Some((seconds * 1_000_000_000_f64) as u128)
                    }
                })
        })
}

fn trace_payload_cmd_name(payload: &Value) -> Option<String> {
    payload
        .get("name")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn trace_payload_argv(payload: &Value) -> Vec<String> {
    payload
        .get("argv")
        .and_then(Value::as_array)
        .map(|argv| {
            argv.iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn trace_payload_effective_argv(payload: &Value) -> Vec<String> {
    let argv = trace_payload_argv(payload);
    if !argv.is_empty() {
        return argv;
    }
    payload
        .get(TRACE_ROOT_ARGV_FIELD)
        .and_then(Value::as_array)
        .map(|argv| {
            argv.iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn trace_payload_primary_command(payload: &Value) -> Option<String> {
    trace_payload_cmd_name(payload).or_else(|| {
        let argv = trace_payload_argv(payload);
        trace_argv_primary_command(&argv)
    })
}

fn trace_payload_root_started_at_ns(payload: &Value) -> Option<u128> {
    payload
        .get(TRACE_ROOT_STARTED_AT_NS_FIELD)
        .and_then(Value::as_u64)
        .map(u128::from)
}

fn trace_argv_primary_command(argv: &[String]) -> Option<String> {
    let mut idx = 0;
    if argv
        .first()
        .map(|token| {
            let file_name = Path::new(token)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(token);
            file_name == "git" || file_name == "git.exe"
        })
        .unwrap_or(false)
    {
        idx = 1;
    }
    while idx < argv.len() {
        let token = argv[idx].as_str();
        if token == "-C" {
            idx += 2;
            continue;
        }
        if matches!(
            token,
            "-c" | "--config-env"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--super-prefix"
                | "--exec-path"
                | "--worktree-attributes"
                | "--attr-source"
        ) {
            idx += 2;
            continue;
        }
        if token.starts_with("--") && token.contains('=') {
            idx += 1;
            continue;
        }
        if token.starts_with('-') {
            idx += 1;
            continue;
        }
        return Some(token.to_string());
    }
    None
}

/// Returns true when the trace2 event's command+argument pair is
/// guaranteed to never mutate repository state.
///
/// This extends the simple command check to handle mixed read/write commands
/// such as `branch`, `remote`, `stash`, `tag`, and `worktree`.
fn trace_invocation_is_definitely_read_only(
    primary_command: Option<&str>,
    argv: &[String],
) -> bool {
    use crate::git::command_classification::is_definitely_read_only_git_invocation;
    match primary_command {
        Some(cmd) => is_definitely_read_only_git_invocation(
            cmd,
            &trace_invocation_command_args(Some(cmd), argv),
        ),
        None => false,
    }
}

fn trace_invocation_may_mutate_refs(primary_command: Option<&str>, argv: &[String]) -> bool {
    primary_command.is_some_and(|cmd| {
        crate::git::command_classification::git_invocation_may_mutate_repo_state(
            cmd,
            &trace_invocation_command_args(Some(cmd), argv),
        )
    })
}

fn trace_command_uses_target_repo_context_only(primary_command: Option<&str>) -> bool {
    matches!(primary_command, Some("clone" | "init"))
}

fn trace_invocation_args(argv: &[String]) -> &[String] {
    if argv
        .first()
        .map(|token| {
            Path::new(token)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "git" || name == "git.exe")
        })
        .unwrap_or(false)
    {
        &argv[1..]
    } else {
        argv
    }
}

fn trace_invocation_command_args(primary_command: Option<&str>, argv: &[String]) -> Vec<String> {
    let invocation = trace_invocation_args(argv);
    let parsed = parse_git_cli_args(invocation);
    if parsed.command.as_deref() == primary_command {
        return parsed.command_args;
    }

    let Some(primary) = primary_command else {
        return Vec::new();
    };
    invocation
        .iter()
        .position(|arg| arg == primary)
        .and_then(|idx| invocation.get(idx + 1..))
        .map(|args| args.to_vec())
        .unwrap_or_default()
}

fn matches_any_pathspec(file: &str, pathspecs: &[String]) -> bool {
    pathspecs.iter().any(|pathspec| {
        file == pathspec
            || (pathspec.ends_with('/') && file.starts_with(pathspec))
            || file.starts_with(&format!("{}/", pathspec))
    })
}

fn resolve_stash_sha(cmd: &crate::daemon::domain::NormalizedCommand) -> Option<&str> {
    cmd.stash_target_oid.as_deref().or_else(|| {
        cmd.ref_changes
            .iter()
            .find(|rc| rc.reference == "refs/stash")
            .map(|rc| rc.old.as_str())
            .filter(|s| !s.is_empty() && *s != "0000000000000000000000000000000000000000")
    })
}

fn stash_base_head(repo: &Repository, stash_sha: &str) -> Option<String> {
    repo.find_commit(stash_sha.to_string())
        .ok()
        .and_then(|commit| commit.parent(0).ok())
        .map(|parent| parent.id().to_string())
}

/// After a rebase completes, check if any newly-rebased commits were created
/// from conflict resolution with AI checkpoints. If so, merge those resolution
/// checkpoints into the already-shifted source authorship note for the new commit.
#[derive(Default)]
struct RewriteMetricContext {
    parent_by_commit: HashMap<String, String>,
    parent_diff_by_commit: HashMap<String, crate::authorship::rewrite::DiffTreeResult>,
}

fn process_conflict_resolution_working_logs(
    repo: &Repository,
    new_tip: &str,
    onto: Option<&str>,
) -> Result<RewriteMetricContext, GitAiError> {
    crate::wltrace::wltrace(
        "rebase.conflict_logs",
        &repo.workdir().unwrap_or_default(),
        || format!("new_tip={new_tip} onto={}", onto.unwrap_or("NONE")),
    );
    let onto_sha = match onto {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(RewriteMetricContext::default()),
    };

    // Walk rebased commits between onto and new_tip
    let mut args = repo.global_args_for_exec();
    args.extend([
        "log".to_string(),
        "--format=%H %P".to_string(),
        format!("{}..{}", onto_sha, new_tip),
    ]);
    let output = crate::git::repository::exec_git(&args)?;
    let log_output = String::from_utf8_lossy(&output.stdout);

    let commit_parent_pairs = log_output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            (parts.len() >= 2).then(|| (parts[0].to_string(), parts[1].to_string()))
        })
        .collect::<Vec<_>>();
    let commit_shas = commit_parent_pairs
        .iter()
        .map(|(commit_sha, _)| commit_sha.clone())
        .collect::<Vec<_>>();
    let collect_metric_context = crate::authorship::rewrite::rewrite_metrics_enabled();
    let mut metric_context = if collect_metric_context {
        RewriteMetricContext {
            parent_by_commit: commit_parent_pairs
                .iter()
                .map(|(commit_sha, parent_sha)| (commit_sha.clone(), parent_sha.clone()))
                .collect(),
            parent_diff_by_commit: HashMap::new(),
        }
    } else {
        RewriteMetricContext::default()
    };
    let existing_notes = crate::git::notes_api::read_notes_batch(repo, &commit_shas)?;
    let author = repo.effective_author_identity().formatted_or_unknown();

    // Only commits whose rebased parent still has a working log incur
    // attribution reconstruction; restrict the (expensive) parent->commit diffs
    // to those. Compute ALL of them in ONE batched diff-tree so the per-commit
    // loop below performs no per-commit git spawns.
    let qualifying: Vec<&(String, String)> = commit_parent_pairs
        .iter()
        .filter(|(_, parent_sha)| repo.storage.has_working_log(parent_sha))
        .collect();
    let diff_pairs: Vec<(String, String)> = qualifying
        .iter()
        .map(|(commit_sha, parent_sha)| (parent_sha.clone(), commit_sha.clone()))
        .collect();
    let diff_results = if diff_pairs.is_empty() {
        Vec::new()
    } else {
        crate::authorship::rewrite::compute_diff_trees_batch(repo, &diff_pairs)?
    };
    let diff_by_commit: HashMap<&str, &crate::authorship::rewrite::DiffTreeResult> = qualifying
        .iter()
        .zip(diff_results.iter())
        .map(|((commit_sha, _), result)| (commit_sha.as_str(), result))
        .collect();
    if collect_metric_context {
        metric_context.parent_diff_by_commit = qualifying
            .iter()
            .zip(diff_results.iter())
            .map(|((commit_sha, _), result)| (commit_sha.clone(), result.clone()))
            .collect();
    }

    for (commit_sha, parent_sha) in &commit_parent_pairs {
        let existing_shifted_log = existing_notes
            .get(commit_sha)
            .and_then(|raw| AuthorshipLog::deserialize_from_string(raw).ok());
        post_conflict_resolution_working_log(
            repo,
            parent_sha,
            commit_sha,
            author.clone(),
            existing_shifted_log,
            diff_by_commit.get(commit_sha.as_str()).copied(),
        )?;
    }
    Ok(metric_context)
}

fn rewrite_metric_commits_with_context(
    metric_commits: Vec<crate::authorship::rewrite::RewriteMetricCommit>,
    context: RewriteMetricContext,
) -> Vec<crate::authorship::rewrite::RewriteMetricCommit> {
    metric_commits
        .into_iter()
        .map(|mut commit| {
            if let Some(parent_sha) = context.parent_by_commit.get(&commit.new_sha) {
                commit = commit.with_parent_sha(parent_sha.clone());
            }
            if let Some(diff) = context.parent_diff_by_commit.get(&commit.new_sha) {
                commit = commit.with_parent_diff(diff.clone());
            }
            commit
        })
        .collect()
}

fn post_conflict_resolution_working_log(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    author: String,
    existing_shifted_log: Option<AuthorshipLog>,
    precomputed_parent_diff: Option<&crate::authorship::rewrite::DiffTreeResult>,
) -> Result<(), GitAiError> {
    if !repo.storage.has_working_log(parent_sha) {
        return Ok(());
    }

    let commit_for_transform = commit_sha.to_string();
    crate::authorship::post_commit::post_commit_from_working_log_with_transform_options_and_diff(
        repo,
        Some(parent_sha.to_string()),
        commit_sha.to_string(),
        author,
        crate::authorship::post_commit::PostCommitOptions {
            supress_output: true,
            compute_stats: false,
            recover_attribution: false,
        },
        precomputed_parent_diff,
        move |resolution_log| {
            Ok(
                crate::authorship::conflict_resolution::merge_conflict_resolution_authorship(
                    existing_shifted_log,
                    resolution_log,
                    &commit_for_transform,
                ),
            )
        },
    )
    .map(|_| ())
}

fn rfc3339_to_unix_nanos(value: &str) -> Option<u128> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .and_then(|timestamp| u128::try_from(timestamp.timestamp_nanos_opt()?).ok())
}

#[cfg(feature = "test-support")]
fn checkpoint_test_delay(env_var: &str, trace_id: &str) -> Option<Duration> {
    let spec = std::env::var(env_var).ok()?;
    spec.split(',').find_map(|entry| {
        let (entry_trace_id, delay_ms) = entry.split_once('=')?;
        if entry_trace_id != trace_id {
            return None;
        }
        delay_ms
            .parse::<u64>()
            .ok()
            .filter(|delay_ms| *delay_ms > 0)
            .map(Duration::from_millis)
    })
}

#[cfg(feature = "test-support")]
fn wait_at_checkpoint_test_barrier(trace_id: &str) -> Result<(), GitAiError> {
    let Ok(barrier_dir) = std::env::var("GIT_AI_TEST_CHECKPOINT_SIDE_EFFECT_BARRIER_DIR") else {
        return Ok(());
    };
    let barrier_dir = PathBuf::from(barrier_dir);
    fs::create_dir_all(&barrier_dir)?;
    let marker = format!("{:x}", Sha256::digest(trace_id.as_bytes()));
    fs::write(barrier_dir.join(marker), [])?;

    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        if fs::read_dir(&barrier_dir)?.count() >= 2 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(GitAiError::Generic(format!(
        "checkpoint test barrier timed out for {trace_id}"
    )))
}

fn apply_checkpoint_side_effect(mut request: CheckpointRequest) -> Result<(), GitAiError> {
    #[cfg(feature = "test-support")]
    {
        wait_at_checkpoint_test_barrier(&request.trace_id)?;
        if let Some(delay) = checkpoint_test_delay(
            "GIT_AI_TEST_DELAY_CHECKPOINT_SIDE_EFFECT",
            &request.trace_id,
        ) {
            std::thread::sleep(delay);
        }
        if std::env::var("GIT_AI_TEST_FAIL_CHECKPOINT_SIDE_EFFECT")
            .is_ok_and(|trace_id| trace_id == request.trace_id)
        {
            return Err(GitAiError::Generic(
                "synthetic checkpoint processing failure".to_string(),
            ));
        }
    }

    if request.files.is_empty() {
        return Ok(());
    }

    if request.checkpoint_kind.is_ai()
        && let Some(agent_id) = request.agent_id.as_mut()
    {
        crate::streams::model_extraction::enrich_copilot_agent_model(agent_id, &request.metadata);
    }

    let repo_work_dir = &request.files[0].repo_work_dir;
    let repo = match discover_repository_in_path_no_git_exec(repo_work_dir) {
        Ok(repo) => repo,
        Err(e) => {
            if request.checkpoint_kind.is_ai()
                && let Some(ref agent_id) = request.agent_id
                && crate::daemon::checkpoint::should_emit_agent_usage(agent_id)
            {
                let attrs = crate::daemon::checkpoint::build_agent_usage_attrs(None, agent_id);
                let values = crate::metrics::AgentUsageValues::new();
                crate::metrics::record(values, attrs);
            }
            return Err(e);
        }
    };
    let author = repo.effective_author_identity().formatted_or_unknown();

    if request.checkpoint_kind.is_ai()
        && let Some(ref agent_id) = request.agent_id
        && crate::daemon::checkpoint::should_emit_agent_usage(agent_id)
    {
        let attrs = crate::daemon::checkpoint::build_agent_usage_attrs(Some(&repo), agent_id);
        let values = crate::metrics::AgentUsageValues::new();
        crate::metrics::record(values, attrs);
    }

    let resolved = resolve_checkpoint_request(&repo, &mut request)?;
    let Some(resolved) = resolved else {
        return Ok(());
    };

    crate::daemon::checkpoint::execute_resolved_checkpoint_from_daemon(
        &repo,
        &author,
        request.checkpoint_kind,
        request,
        resolved,
    )
}

fn resolve_checkpoint_request(
    repo: &crate::git::repository::Repository,
    request: &mut CheckpointRequest,
) -> Result<Option<crate::daemon::checkpoint::ResolvedCheckpointExecution>, GitAiError> {
    use crate::authorship::ignore::{
        build_ignore_matcher, effective_ignore_patterns, should_ignore_file_with_matcher,
    };
    use crate::commands::checkpoint_agent::orchestrator::BaseCommit;
    use crate::utils::normalize_to_posix;

    let Some(first_file) = request.files.first() else {
        return Ok(None);
    };
    let base_commit = match &first_file.base_commit {
        BaseCommit::Sha(sha) => sha.clone(),
        BaseCommit::Initial => "initial".to_string(),
    };

    let repo_workdir = repo.workdir()?;
    let canonical_workdir = repo_workdir.canonicalize().unwrap_or(repo_workdir.clone());
    let ignore_patterns = effective_ignore_patterns(repo, &[], &[]);
    let ignore_matcher = build_ignore_matcher(&ignore_patterns);

    let mut files = Vec::new();
    let mut dirty_files: HashMap<String, Arc<str>> = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    let config = config::Config::fresh();
    let mut content_budget = CheckpointContentBudget::from_config(&config);

    for file in &mut request.files {
        let path_str = file.path.to_string_lossy();
        let path_str = path_str.trim();
        if path_str.is_empty() {
            continue;
        }

        let abs_path = if file.path.is_absolute() {
            file.path.clone()
        } else {
            repo_workdir.join(&*file.path)
        };
        if !repo.path_is_in_workdir(&abs_path) {
            continue;
        }

        let relative_path = abs_path
            .canonicalize()
            .unwrap_or(abs_path.clone())
            .strip_prefix(&canonical_workdir)
            .map(|p| normalize_to_posix(&p.to_string_lossy()))
            .unwrap_or_else(|_| {
                abs_path
                    .strip_prefix(&repo_workdir)
                    .map(|p| normalize_to_posix(&p.to_string_lossy()))
                    .unwrap_or_else(|_| normalize_to_posix(path_str))
            });

        if !seen.insert(relative_path.clone()) {
            continue;
        }
        if should_ignore_file_with_matcher(&relative_path, &ignore_matcher) {
            continue;
        }

        if let Some(content) = std::mem::take(&mut file.content) {
            if content.as_bytes().contains(&0) {
                continue;
            }
            if !content_budget.reserve(&relative_path, &content) {
                continue;
            }
            dirty_files.insert(relative_path.clone(), Arc::from(content));
            files.push(relative_path);
        }
    }

    if files.is_empty() {
        return Ok(None);
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    Ok(Some(
        crate::daemon::checkpoint::ResolvedCheckpointExecution {
            base_commit,
            ts,
            files,
            dirty_files,
        },
    ))
}

fn compute_watermarks_from_stat(
    repo_working_dir: &str,
    file_paths: &[String],
) -> std::collections::HashMap<String, u128> {
    let repo_root = std::path::Path::new(repo_working_dir);
    let mut watermarks = std::collections::HashMap::new();
    for path in file_paths {
        let full_path = repo_root.join(path);
        if let Ok(metadata) = std::fs::symlink_metadata(&full_path)
            && let Ok(mtime) = metadata.modified()
        {
            let nanos = mtime
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            // Normalize watermark keys the same way bash_tool::normalize_path does
            // so that case-folded snapshot lookups on macOS/Windows find a match.
            let key = crate::commands::checkpoint_agent::bash_tool::normalize_path(
                std::path::Path::new(path),
            )
            .to_string_lossy()
            .to_string();
            watermarks.insert(key, nanos);
        }
    }
    watermarks
}

fn capture_commit_file_timestamps(
    worktree: &Path,
    commit_sha: &str,
) -> Result<crate::authorship::attribution_recovery::FileTimestampsByPath, GitAiError> {
    let repo = find_repository_in_path(&worktree.to_string_lossy())?;
    let workdir = repo.workdir()?;
    let files = repo.list_commit_files(commit_sha, None)?;
    let mut timestamps_by_path = HashMap::new();
    for file_path in files {
        let timestamps = crate::authorship::attribution_recovery::file_timestamps_for_path(
            &workdir.join(&file_path),
        );
        if !timestamps.is_empty() {
            timestamps_by_path.insert(file_path, timestamps);
        }
    }
    Ok(timestamps_by_path)
}

fn parsed_invocation_for_side_effect(
    command: Option<&str>,
    args: &[String],
) -> ParsedGitInvocation {
    ParsedGitInvocation {
        global_args: Vec::new(),
        command: command.map(ToString::to_string),
        command_args: args.to_vec(),
        saw_end_of_opts: false,
        is_help: command == Some("help") || args.iter().any(|arg| arg == "-h" || arg == "--help"),
    }
}

fn parsed_invocation_for_normalized_command(
    cmd: &crate::daemon::domain::NormalizedCommand,
) -> ParsedGitInvocation {
    if !cmd.raw_argv.is_empty() {
        return parse_git_cli_args(trace_invocation_args(&cmd.raw_argv));
    }

    if cmd.primary_command.is_some() || !cmd.invoked_args.is_empty() {
        return parsed_invocation_for_side_effect(
            cmd.primary_command.as_deref(),
            &cmd.invoked_args,
        );
    }

    ParsedGitInvocation {
        global_args: Vec::new(),
        command: None,
        command_args: Vec::new(),
        saw_end_of_opts: false,
        is_help: false,
    }
}

fn apply_push_side_effect(
    worktree: &str,
    command: Option<&str>,
    args: &[String],
) -> Result<(), GitAiError> {
    use crate::config::NotesBackendKind;
    use crate::git::cli_parser::is_dry_run;
    use crate::git::sync_authorship::{push_authorship_notes, push_remote_from_args};

    if crate::config::Config::get().notes_backend_kind() == NotesBackendKind::Http {
        tracing::debug!("apply_push_side_effect: skipping authorship push (Http backend)");
        return Ok(());
    }

    let repo = find_repository_in_path(worktree)?;
    let parsed = parsed_invocation_for_side_effect(command, args);

    if is_dry_run(&parsed.command_args)
        || parsed
            .command_args
            .iter()
            .any(|a| a == "-d" || a == "--delete")
        || parsed.command_args.iter().any(|a| a == "--mirror")
    {
        return Ok(());
    }

    let remote = push_remote_from_args(&repo, &parsed)?;

    crate::commands::upgrade::maybe_schedule_background_update_check();
    tracing::debug!("started pushing authorship notes to remote: {}", remote);

    push_authorship_notes(&repo, &remote)
}

fn transcript_sweep_triggers_for_events(
    events: &[crate::daemon::domain::SemanticEvent],
) -> Vec<crate::daemon::stream_worker::SweepTrigger> {
    let mut triggers = Vec::new();

    if events.iter().any(|event| {
        matches!(
            event,
            crate::daemon::domain::SemanticEvent::CommitCreated { .. }
                | crate::daemon::domain::SemanticEvent::CommitAmended { .. }
        )
    }) {
        triggers.push(crate::daemon::stream_worker::SweepTrigger::PostCommit);
    }

    if events.iter().any(|event| {
        matches!(
            event,
            crate::daemon::domain::SemanticEvent::PushCompleted { .. }
        )
    }) {
        triggers.push(crate::daemon::stream_worker::SweepTrigger::PostPush);
    }

    triggers
}

fn apply_pull_notes_sync_side_effect(
    worktree: &str,
    command: Option<&str>,
    args: &[String],
) -> Result<(), GitAiError> {
    use crate::config::NotesBackendKind;

    let repo = find_repository_in_path(worktree)?;
    let parsed = parsed_invocation_for_side_effect(command, args);
    let remote = fetch_remote_from_args(&repo, &parsed)?;
    let notes_backend = crate::config::Config::fresh().notes_backend_kind();

    tracing::info!(
        command = command.unwrap_or("pull"),
        remote = %remote,
        backend = %notes_backend,
        worktree = %worktree,
        "handling pull notes sync"
    );

    if notes_backend == NotesBackendKind::Http {
        return crate::git::notes_api::warm_cache_for_remote(&repo, &remote).map(|_| ());
    }

    fetch_authorship_notes(&repo, &remote)?;
    Ok(())
}

fn apply_clone_notes_sync_side_effect(worktree: &str) -> Result<(), GitAiError> {
    use crate::config::NotesBackendKind;

    let repo = find_repository_in_path(worktree)?;
    let remote = "origin";
    let notes_backend = crate::config::Config::fresh().notes_backend_kind();

    tracing::info!(
        command = "clone",
        remote = %remote,
        backend = %notes_backend,
        worktree = %worktree,
        "handling clone notes sync"
    );

    if notes_backend == NotesBackendKind::Http {
        return crate::git::notes_api::warm_cache_for_remote(&repo, remote).map(|_| ());
    }

    fetch_authorship_notes(&repo, remote)?;
    Ok(())
}

fn apply_pull_fast_forward_working_log_side_effect(
    worktree: &str,
    old_head: &str,
    new_head: &str,
) -> Result<(), GitAiError> {
    let repo = find_repository_in_path(worktree)?;
    repo.storage.rename_working_log(old_head, new_head)?;
    Ok(())
}

fn remove_working_log_attributions_for_pathspecs(
    repository: &Repository,
    head: &str,
    pathspecs: &[String],
) -> Result<(), GitAiError> {
    let working_log = repository.storage.working_log_for_base_commit(head)?;

    let initial = working_log.read_initial_attributions();
    if !initial.files.is_empty() {
        let filtered_files = initial
            .files
            .into_iter()
            .filter(|(file, _)| !matches_any_pathspec(file, pathspecs))
            .collect();
        let mut filtered_blobs = initial.file_blobs;
        filtered_blobs.retain(|file, _| !matches_any_pathspec(file, pathspecs));
        working_log.write_initial(crate::git::repo_storage::InitialAttributions {
            files: filtered_files,
            prompts: initial.prompts,
            file_blobs: filtered_blobs,
            humans: initial.humans,
            sessions: initial.sessions,
        })?;
    }

    let checkpoints = working_log.read_all_checkpoints()?;
    let filtered: Vec<_> = checkpoints
        .into_iter()
        .map(|mut checkpoint| {
            checkpoint
                .entries
                .retain(|entry| !matches_any_pathspec(&entry.file, pathspecs));
            checkpoint
        })
        .filter(|checkpoint| !checkpoint.entries.is_empty())
        .collect();
    working_log.write_all_checkpoints(&filtered)?;
    Ok(())
}

fn apply_checkout_switch_working_log_side_effect(
    cmd: &crate::daemon::domain::NormalizedCommand,
) -> Result<(), GitAiError> {
    let Some(worktree) = cmd.worktree.as_ref() else {
        return Ok(());
    };
    let repo = find_repository_in_path(&worktree.to_string_lossy())?;
    let parsed = parsed_invocation_for_normalized_command(cmd);
    let (old_head, new_head) = ActorDaemonCoordinator::resolve_heads_for_command(cmd);

    if cmd.primary_command.as_deref() == Some("checkout") {
        let pathspecs = parsed.pathspecs();
        if !pathspecs.is_empty() {
            if !old_head.is_empty() {
                remove_working_log_attributions_for_pathspecs(&repo, &old_head, &pathspecs)?;
            }
            return Ok(());
        }
    }

    if old_head.is_empty() || new_head.is_empty() || old_head == new_head {
        return Ok(());
    }

    let is_merge = parsed.has_command_flag("--merge") || parsed.has_command_flag("-m");
    let is_force = match cmd.primary_command.as_deref() {
        Some("checkout") => parsed.has_command_flag("--force") || parsed.has_command_flag("-f"),
        Some("switch") => {
            parsed.has_command_flag("--discard-changes")
                || parsed.has_command_flag("--force")
                || parsed.has_command_flag("-f")
        }
        _ => false,
    };

    if is_force {
        repo.storage.delete_working_log_for_base_commit(&old_head)?;
        return Ok(());
    }

    if is_merge {
        let final_state =
            crate::authorship::virtual_attribution::checkout_merge_final_state_snapshot(
                &repo, &old_head, &new_head,
            )?;
        if final_state.is_empty() {
            repo.storage.delete_working_log_for_base_commit(&old_head)?;
            return Ok(());
        }
        let author = repo.effective_author_identity().formatted_or_unknown();
        crate::authorship::virtual_attribution::restore_working_log_carryover(
            &repo,
            &old_head,
            &new_head,
            final_state,
            Some(author),
        )?;
        repo.storage.delete_working_log_for_base_commit(&old_head)?;
        return Ok(());
    }

    repo.storage.rename_working_log(&old_head, &new_head)?;
    Ok(())
}

fn recent_checkout_switch_prerequisite_from_command(
    cmd: &crate::daemon::domain::NormalizedCommand,
) -> Option<RecentReplayPrerequisite> {
    let parsed = parsed_invocation_for_normalized_command(cmd);
    let (old_head, new_head) = ActorDaemonCoordinator::resolve_heads_for_command(cmd);

    if old_head.is_empty() || new_head.is_empty() || old_head == new_head {
        return None;
    }

    if cmd.primary_command.as_deref() == Some("checkout") && !parsed.pathspecs().is_empty() {
        return None;
    }

    let is_force = match cmd.primary_command.as_deref() {
        Some("checkout") => parsed.has_command_flag("--force") || parsed.has_command_flag("-f"),
        Some("switch") => {
            parsed.has_command_flag("--discard-changes")
                || parsed.has_command_flag("--force")
                || parsed.has_command_flag("-f")
        }
        _ => false,
    };
    if is_force {
        return None;
    }

    let is_merge = parsed.has_command_flag("--merge") || parsed.has_command_flag("-m");
    if is_merge {
        return None;
    }

    Some(RecentReplayPrerequisite::CheckoutSwitchRename {
        target_head: new_head,
        old_head,
    })
}
fn family_key_for_repository(repo: &Repository) -> String {
    repo.common_dir()
        .canonicalize()
        .unwrap_or_else(|_| repo.common_dir().to_path_buf())
        .to_string_lossy()
        .to_string()
}
fn is_valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_zero_oid(oid: &str) -> bool {
    is_valid_oid(oid) && oid.chars().all(|c| c == '0')
}

fn is_non_auxiliary_ref(reference: &str) -> bool {
    !(reference.starts_with("refs/notes/")
        || reference.starts_with("refs/tags/")
        || reference.starts_with("refs/replace/"))
}

/// Check whether `ancestor` is an ancestor of `descendant` using
/// `git merge-base --is-ancestor`.
fn is_ancestor_commit(repository: &Repository, ancestor: &str, descendant: &str) -> bool {
    let mut args = repository.global_args_for_exec();
    args.push("merge-base".to_string());
    args.push("--is-ancestor".to_string());
    args.push(ancestor.to_string());
    args.push(descendant.to_string());
    crate::git::repository::exec_git(&args).is_ok()
}

fn repo_is_ancestor(
    repository: &crate::git::repository::Repository,
    ancestor: &str,
    descendant: &str,
) -> bool {
    let mut args = repository.global_args_for_exec();
    args.push("merge-base".to_string());
    args.push("--is-ancestor".to_string());
    args.push(ancestor.to_string());
    args.push(descendant.to_string());
    exec_git(&args).is_ok()
}

fn rebase_is_control_mode(cmd: &crate::daemon::domain::NormalizedCommand) -> bool {
    summarize_rebase_args(&cmd.invoked_args).is_control_mode
}

fn rebase_onto_from_command(
    cmd: &crate::daemon::domain::NormalizedCommand,
    repository: &Repository,
    original_head: &str,
    new_tip: &str,
) -> Option<String> {
    let head_changes = cmd
        .ref_changes
        .iter()
        .filter(|change| {
            change.reference == "HEAD"
                && is_valid_oid(&change.old)
                && !is_zero_oid(&change.old)
                && is_valid_oid(&change.new)
                && !is_zero_oid(&change.new)
                && change.old != change.new
        })
        .collect::<Vec<_>>();

    head_changes
        .iter()
        .find(|change| {
            change.old == original_head
                && change.new != original_head
                && change.new != new_tip
                && is_ancestor_commit(repository, &change.new, new_tip)
        })
        .map(|change| change.new.clone())
        .or_else(|| {
            head_changes
                .iter()
                .find(|change| {
                    change.old != original_head
                        && change.old != new_tip
                        && is_ancestor_commit(repository, &change.old, new_tip)
                })
                .map(|change| change.old.clone())
        })
}

fn valid_non_zero_ref_change(change: &crate::daemon::domain::RefChange) -> bool {
    is_valid_oid(&change.old)
        && !is_zero_oid(&change.old)
        && is_valid_oid(&change.new)
        && !is_zero_oid(&change.new)
        && change.old != change.new
}

fn rewrite_metric_branch_for_ref(reference: &str) -> Option<String> {
    crate::authorship::rewrite::branch_name_from_ref(reference)
}

fn rewrite_metric_branch_for_transition(
    cmd: &crate::daemon::domain::NormalizedCommand,
    old_tip: &str,
    new_tip: &str,
    reference_hint: Option<&str>,
) -> Option<String> {
    reference_hint
        .and_then(rewrite_metric_branch_for_ref)
        .or_else(|| {
            cmd.ref_changes
                .iter()
                .rev()
                .find(|change| {
                    change.reference.starts_with("refs/heads/")
                        && change.old == old_tip
                        && change.new == new_tip
                })
                .and_then(|change| rewrite_metric_branch_for_ref(&change.reference))
        })
}

fn rewrite_metric_commits_with_branch(
    metric_commits: Vec<crate::authorship::rewrite::RewriteMetricCommit>,
    branch: Option<String>,
) -> Vec<crate::authorship::rewrite::RewriteMetricCommit> {
    match branch {
        Some(branch) => metric_commits
            .into_iter()
            .map(|commit| commit.with_branch(branch.clone()))
            .collect(),
        None => metric_commits,
    }
}

fn rebase_new_tip_from_command(
    cmd: &crate::daemon::domain::NormalizedCommand,
    original_head: &str,
) -> Option<String> {
    if let Some(new_tip) = cmd
        .ref_changes
        .iter()
        .rev()
        .find(|change| {
            change.reference.starts_with("refs/heads/")
                && valid_non_zero_ref_change(change)
                && change.old == original_head
        })
        .map(|change| change.new.clone())
    {
        return Some(new_tip);
    }

    if !rebase_is_control_mode(cmd) {
        return None;
    }

    let branch_ref_names = cmd
        .ref_changes
        .iter()
        .filter(|change| {
            change.reference.starts_with("refs/heads/") && valid_non_zero_ref_change(change)
        })
        .map(|change| change.reference.as_str())
        .collect::<std::collections::HashSet<_>>();
    if branch_ref_names.len() == 1
        && let Some(new_tip) = cmd
            .ref_changes
            .iter()
            .rev()
            .find(|change| {
                change.reference.starts_with("refs/heads/") && valid_non_zero_ref_change(change)
            })
            .map(|change| change.new.clone())
    {
        return Some(new_tip);
    }

    cmd.ref_changes
        .iter()
        .rev()
        .find(|change| change.reference == "HEAD" && valid_non_zero_ref_change(change))
        .map(|change| change.new.clone())
}

fn cherry_pick_destination_commits(cmd: &crate::daemon::domain::NormalizedCommand) -> Vec<String> {
    cmd.ref_changes
        .iter()
        .filter(|change| change.reference == "HEAD")
        .filter(|change| {
            is_valid_oid(&change.old)
                && !is_zero_oid(&change.old)
                && is_valid_oid(&change.new)
                && !is_zero_oid(&change.new)
                && change.old != change.new
        })
        .map(|change| change.new.clone())
        .collect()
}

fn first_head_transition_old(cmd: &crate::daemon::domain::NormalizedCommand) -> Option<String> {
    cmd.ref_changes
        .iter()
        .find(|change| {
            change.reference == "HEAD"
                && is_valid_oid(&change.old)
                && !is_zero_oid(&change.old)
                && is_valid_oid(&change.new)
                && !is_zero_oid(&change.new)
                && change.old != change.new
        })
        .map(|change| change.old.clone())
}

fn cherry_pick_original_head(cmd: &crate::daemon::domain::NormalizedCommand) -> Option<String> {
    first_head_transition_old(cmd)
}

fn revert_original_head(cmd: &crate::daemon::domain::NormalizedCommand) -> Option<String> {
    first_head_transition_old(cmd)
}

fn cherry_pick_source_args_for_side_effect(
    cmd: &crate::daemon::domain::NormalizedCommand,
) -> Vec<String> {
    let parsed = parsed_invocation_for_normalized_command(cmd);
    if parsed.command.as_deref() != Some("cherry-pick")
        && cmd.primary_command.as_deref() != Some("cherry-pick")
    {
        return Vec::new();
    }

    cherry_pick_source_args_from_command_args(&parsed.command_args)
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()
}

fn cherry_pick_command_has_flag(
    cmd: &crate::daemon::domain::NormalizedCommand,
    flag: &str,
) -> bool {
    let parsed = parsed_invocation_for_normalized_command(cmd);
    if parsed.command.as_deref() != Some("cherry-pick")
        && cmd.primary_command.as_deref() != Some("cherry-pick")
    {
        return false;
    }

    parsed.command_args.iter().any(|arg| arg == flag)
}

fn cherry_pick_source_args_from_command_args(args: &[String]) -> Vec<&str> {
    let mut sources = Vec::new();
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = args[idx].as_str();
        if arg == "--" {
            sources.extend(args[idx + 1..].iter().map(String::as_str));
            break;
        }
        if matches!(arg, "--abort" | "--continue" | "--quit" | "--skip") {
            return Vec::new();
        }
        if matches!(
            arg,
            "-m" | "--mainline" | "-X" | "--strategy-option" | "--strategy"
        ) {
            idx = idx.saturating_add(2);
            continue;
        }
        if arg.starts_with("--mainline=")
            || arg.starts_with("--strategy=")
            || arg.starts_with("--strategy-option=")
            || arg == "--gpg-sign"
            || arg.starts_with("--gpg-sign=")
            || arg.starts_with("-m")
            || arg.starts_with("-X")
            || arg.starts_with("-S")
        {
            idx += 1;
            continue;
        }
        if arg.starts_with('-') {
            idx += 1;
            continue;
        }
        if !arg.is_empty() {
            sources.push(arg);
        }
        idx += 1;
    }
    sources
}

fn cherry_pick_source_is_range(source: &str) -> bool {
    source.contains("..")
}

fn cherry_pick_range_has_omitted_side(source: &str) -> bool {
    if let Some((left, right)) = source.split_once("...") {
        left.is_empty() || right.is_empty()
    } else if let Some((left, right)) = source.split_once("..") {
        left.is_empty() || right.is_empty()
    } else {
        false
    }
}

fn resolve_cherry_pick_source_args_with_git_in_head_context(
    repo: &Repository,
    source_args: &[String],
    head_context: Option<&str>,
) -> Result<Vec<String>, GitAiError> {
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();

    for source in source_args {
        let source = head_context
            .map(|head| rewrite_head_source_arg_for_side_effect(source, head))
            .unwrap_or_else(|| source.clone());
        let oids = if cherry_pick_source_is_range(&source) {
            if cherry_pick_range_has_omitted_side(&source) {
                Vec::new()
            } else {
                resolve_cherry_pick_range_source_with_git(repo, &source)?
            }
        } else {
            resolve_cherry_pick_single_source_with_git(repo, &source)?
        };

        for oid in oids {
            if seen.insert(oid.clone()) {
                resolved.push(oid);
            }
        }
    }

    Ok(resolved)
}

fn rewrite_head_source_arg_for_side_effect(source: &str, head_context: &str) -> String {
    if head_context.is_empty() || !is_valid_oid(head_context) {
        return source.to_string();
    }
    if let Some((left, right)) = source.split_once("...") {
        return format!(
            "{}...{}",
            rewrite_head_source_term_for_side_effect(left, head_context),
            rewrite_head_source_term_for_side_effect(right, head_context)
        );
    }
    if let Some((left, right)) = source.split_once("..") {
        return format!(
            "{}..{}",
            rewrite_head_source_term_for_side_effect(left, head_context),
            rewrite_head_source_term_for_side_effect(right, head_context)
        );
    }
    rewrite_head_source_term_for_side_effect(source, head_context)
}

fn rewrite_head_source_term_for_side_effect(term: &str, head_context: &str) -> String {
    if term == "HEAD" || term == "@" {
        return head_context.to_string();
    }
    if let Some(suffix) = term.strip_prefix("HEAD")
        && (suffix.starts_with('~') || suffix.starts_with('^'))
    {
        return format!("{head_context}{suffix}");
    }
    if let Some(suffix) = term.strip_prefix('@')
        && (suffix.starts_with('~') || suffix.starts_with('^'))
    {
        return format!("{head_context}{suffix}");
    }
    term.to_string()
}

fn resolve_cherry_pick_single_source_with_git(
    repo: &Repository,
    source: &str,
) -> Result<Vec<String>, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "cat-file".to_string(),
        "--batch-check=%(objectname) %(objecttype)".to_string(),
    ]);
    let stdin_data = format!("{source}^{{commit}}\n");
    let output = exec_git_stdin(&args, stdin_data.as_bytes())?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let oid = parts.next()?;
            (parts.next() == Some("commit") && is_valid_oid(oid)).then(|| oid.to_string())
        })
        .collect())
}

fn resolve_cherry_pick_range_source_with_git(
    repo: &Repository,
    source: &str,
) -> Result<Vec<String>, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "rev-list".to_string(),
        "--reverse".to_string(),
        source.to_string(),
    ]);
    let output = exec_git(&args)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| is_valid_oid(line))
        .map(ToOwned::to_owned)
        .collect())
}

fn resolve_explicit_cherry_pick_sources_for_side_effect(
    repo: &Repository,
    cmd: &crate::daemon::domain::NormalizedCommand,
) -> Result<Vec<String>, GitAiError> {
    let source_args = cherry_pick_source_args_for_side_effect(cmd);
    if source_args.is_empty() {
        return Ok(Vec::new());
    }
    let original_head = cherry_pick_original_head(cmd);
    resolve_cherry_pick_source_args_with_git_in_head_context(
        repo,
        &source_args,
        original_head.as_deref(),
    )
}

fn revert_source_args_for_side_effect(
    cmd: &crate::daemon::domain::NormalizedCommand,
) -> Vec<String> {
    let parsed = parsed_invocation_for_normalized_command(cmd);
    if parsed.command.as_deref() != Some("revert")
        && cmd.primary_command.as_deref() != Some("revert")
    {
        return Vec::new();
    }

    revert_source_args_from_command_args(&parsed.command_args)
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()
}

fn revert_source_args_from_command_args(args: &[String]) -> Vec<&str> {
    let args = if args.first().is_some_and(|arg| arg == "revert") {
        &args[1..]
    } else {
        args
    };
    let mut sources = Vec::new();
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = args[idx].as_str();
        if arg == "--" {
            sources.extend(args[idx + 1..].iter().map(String::as_str));
            break;
        }
        if matches!(arg, "--abort" | "--continue" | "--quit" | "--skip") {
            return Vec::new();
        }
        if matches!(arg, "-m" | "--mainline") {
            idx = idx.saturating_add(2);
            continue;
        }
        if arg.starts_with("--mainline=")
            || arg == "--gpg-sign"
            || arg.starts_with("--gpg-sign=")
            || arg.starts_with("-S")
        {
            idx += 1;
            continue;
        }
        if matches!(arg, "-n" | "--no-commit" | "--no-edit" | "-e" | "--edit") {
            idx += 1;
            continue;
        }
        if arg.starts_with('-') {
            idx += 1;
            continue;
        }
        if !arg.is_empty() {
            sources.push(arg);
        }
        idx += 1;
    }
    sources
}

fn resolve_explicit_revert_sources_for_side_effect(
    repo: &Repository,
    cmd: &crate::daemon::domain::NormalizedCommand,
) -> Result<Vec<String>, GitAiError> {
    let source_args = revert_source_args_for_side_effect(cmd);
    if source_args.is_empty() {
        return Ok(Vec::new());
    }
    let original_head = revert_original_head(cmd);
    resolve_cherry_pick_source_args_with_git_in_head_context(
        repo,
        &source_args,
        original_head.as_deref(),
    )
}

fn cherry_pick_state_exists_for_worktree(worktree: &Path) -> bool {
    git_dir_for_worktree(worktree).is_some_and(|git_dir| {
        git_dir.join("CHERRY_PICK_HEAD").exists() || git_dir.join("sequencer").join("todo").exists()
    })
}

fn revert_destination_changes(
    cmd: &crate::daemon::domain::NormalizedCommand,
) -> Vec<&crate::daemon::domain::RefChange> {
    cmd.ref_changes
        .iter()
        .filter(|change| {
            change.reference == "HEAD"
                && is_valid_oid(&change.old)
                && !is_zero_oid(&change.old)
                && is_valid_oid(&change.new)
                && !is_zero_oid(&change.new)
                && change.old != change.new
        })
        .collect()
}

fn apply_revert_complete_rewrite(
    repo: &crate::git::repository::Repository,
    cmd: &crate::daemon::domain::NormalizedCommand,
    source_oids: &[String],
) -> Result<(), GitAiError> {
    let specs: Vec<crate::authorship::rewrite_revert::RevertSpec> = revert_destination_changes(cmd)
        .into_iter()
        .enumerate()
        .map(
            |(index, change)| crate::authorship::rewrite_revert::RevertSpec {
                revert_commit: change.new.clone(),
                parent: Some(change.old.clone()),
                reverted_commit: source_oids.get(index).cloned(),
            },
        )
        .collect();
    let metric_commits =
        crate::authorship::rewrite_revert::handle_revert_commits_with_metrics(repo, &specs)?;
    crate::daemon::rewrite_metrics::spawn_rewrite_commit_metrics(repo, metric_commits);
    Ok(())
}

fn apply_cherry_pick_complete_rewrite(
    repo: &crate::git::repository::Repository,
    original_head: &str,
    sources: &[String],
    new_commits: &[String],
) -> Result<(), GitAiError> {
    let pairs = crate::authorship::rewrite_cherry_pick::match_cherry_pick_pairs(
        repo,
        sources,
        new_commits,
    )?;
    let mut rewrite_metric_commits = Vec::new();
    if !pairs.is_empty() {
        let (src, dst): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
        let outcome = crate::authorship::rewrite::handle_rewrite_event_with_metrics(
            repo,
            crate::authorship::rewrite::RewriteEvent::CherryPickComplete {
                sources: src,
                new_commits: dst,
            },
        )?;
        rewrite_metric_commits.extend(outcome.metric_commits);
    }

    let existing_notes = crate::git::notes_api::read_notes_batch(repo, new_commits)?;
    let author = repo.effective_author_identity().formatted_or_unknown();

    // The cherry-picked commits form a chain: each commit's parent is the
    // previous one (the first's parent is original_head). Build the
    // (commit, parent) pairs, then batch the parent->commit diffs for the
    // commits that actually need reconstruction into ONE diff-tree so the loop
    // performs no per-commit git spawns.
    let mut commit_parent_pairs: Vec<(String, String)> = Vec::new();
    let mut parent = original_head.to_string();
    for commit_sha in new_commits {
        commit_parent_pairs.push((commit_sha.clone(), parent.clone()));
        parent = commit_sha.clone();
    }
    let qualifying: Vec<&(String, String)> = commit_parent_pairs
        .iter()
        .filter(|(_, parent_sha)| repo.storage.has_working_log(parent_sha))
        .collect();
    let diff_pairs: Vec<(String, String)> = qualifying
        .iter()
        .map(|(commit_sha, parent_sha)| (parent_sha.clone(), commit_sha.clone()))
        .collect();
    let diff_results = if diff_pairs.is_empty() {
        Vec::new()
    } else {
        crate::authorship::rewrite::compute_diff_trees_batch(repo, &diff_pairs)?
    };
    let diff_by_commit: HashMap<&str, &crate::authorship::rewrite::DiffTreeResult> = qualifying
        .iter()
        .zip(diff_results.iter())
        .map(|((commit_sha, _), result)| (commit_sha.as_str(), result))
        .collect();

    for (commit_sha, parent_sha) in &commit_parent_pairs {
        let existing_shifted_log = existing_notes
            .get(commit_sha)
            .and_then(|raw| AuthorshipLog::deserialize_from_string(raw).ok());
        post_conflict_resolution_working_log(
            repo,
            parent_sha,
            commit_sha,
            author.clone(),
            existing_shifted_log,
            diff_by_commit.get(commit_sha.as_str()).copied(),
        )?;
    }

    let rewrite_metric_commits = if rewrite_metric_commits.is_empty() {
        rewrite_metric_commits
    } else {
        let parent_by_commit: HashMap<&str, &str> = commit_parent_pairs
            .iter()
            .map(|(commit_sha, parent_sha)| (commit_sha.as_str(), parent_sha.as_str()))
            .collect();
        rewrite_metric_commits
            .into_iter()
            .map(|mut commit| {
                if let Some(parent_sha) = parent_by_commit.get(commit.new_sha.as_str()) {
                    commit = commit.with_parent_sha((*parent_sha).to_string());
                }
                if let Some(diff) = diff_by_commit.get(commit.new_sha.as_str()) {
                    commit = commit.with_parent_diff((*diff).clone());
                }
                commit
            })
            .collect()
    };
    crate::daemon::rewrite_metrics::spawn_rewrite_commit_metrics(repo, rewrite_metric_commits);

    Ok(())
}

fn apply_cherry_pick_no_commit_rewrite(
    repo: &crate::git::repository::Repository,
    sources: &[String],
    parent_head: &str,
    new_head: &str,
) -> Result<(), GitAiError> {
    if sources.is_empty() || new_head.is_empty() {
        return Ok(());
    }
    let mappings = sources
        .iter()
        .map(|source| (source.clone(), new_head.to_string()))
        .collect::<Vec<_>>();
    crate::git::sync_authorship::fetch_missing_notes_for_commits_best_effort(repo, sources);
    let shifted_notes =
        crate::authorship::rewrite::shift_authorship_notes_merging_existing_with_notes(
            repo, &mappings,
        )?;
    if crate::authorship::rewrite::rewrite_metrics_enabled() {
        let mut metric_commit = crate::authorship::rewrite::RewriteMetricCommit::new(
            new_head.to_string(),
            sources.to_vec(),
            crate::authorship::rewrite::RewriteMetricOperation::CherryPickNoCommit,
        )
        .with_parent_sha(parent_head.to_string());
        if let Some((_, note)) = shifted_notes
            .into_iter()
            .find(|(commit_sha, _)| commit_sha == new_head)
        {
            metric_commit = metric_commit.with_authorship_note(note);
        }
        crate::daemon::rewrite_metrics::spawn_rewrite_commit_metrics(repo, vec![metric_commit]);
    }
    Ok(())
}

fn strict_rebase_original_head_from_command(
    cmd: &crate::daemon::domain::NormalizedCommand,
    semantic_old_head: &str,
) -> Option<String> {
    if let Some(branch_spec) = explicit_rebase_branch_arg(&cmd.invoked_args)
        && let Some(branch_ref) = explicit_rebase_branch_ref_name(&branch_spec)
        && let Some(old_head) = cmd
            .ref_changes
            .iter()
            .find(|change| {
                change.reference == branch_ref
                    && is_valid_oid(&change.old)
                    && !is_zero_oid(&change.old)
            })
            .map(|change| change.old.clone())
    {
        return Some(old_head);
    }

    if is_valid_oid(semantic_old_head) && !is_zero_oid(semantic_old_head) {
        return Some(semantic_old_head.to_string());
    }

    if let Some(old_head) = cmd
        .ref_changes
        .iter()
        .find(|change| {
            change.reference.starts_with("refs/heads/")
                && is_valid_oid(&change.old)
                && !is_zero_oid(&change.old)
        })
        .map(|change| change.old.clone())
    {
        return Some(old_head);
    }

    if let Some(old_head) = cmd
        .ref_changes
        .iter()
        .find(|change| {
            change.reference == "HEAD" && is_valid_oid(&change.old) && !is_zero_oid(&change.old)
        })
        .map(|change| change.old.clone())
    {
        return Some(old_head);
    }

    None
}

fn explicit_rebase_branch_ref_name(branch_spec: &str) -> Option<String> {
    if branch_spec.starts_with("refs/") {
        return Some(branch_spec.to_string());
    }
    if is_valid_oid(branch_spec) || branch_spec == "HEAD" || branch_spec.starts_with("@{") {
        return None;
    }
    Some(format!("refs/heads/{}", branch_spec))
}

fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn remove_socket_if_exists(path: &Path) -> Result<(), GitAiError> {
    #[cfg(unix)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(not(windows))]
fn set_socket_owner_only(path: &Path) -> Result<(), GitAiError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn pid_metadata_path(config: &DaemonConfig) -> PathBuf {
    config
        .lock_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(PID_META_FILE)
}

/// Returns the log file path for the currently running daemon, if any.
/// Reads the PID from daemon.pid.json and constructs the log path.
pub fn daemon_log_file_path(config: &DaemonConfig) -> Result<PathBuf, GitAiError> {
    let meta_path = pid_metadata_path(config);
    let contents = fs::read_to_string(&meta_path).map_err(|e| {
        GitAiError::Generic(format!(
            "failed to read daemon pid metadata at {}: {}",
            meta_path.display(),
            e
        ))
    })?;
    let meta: DaemonPidMeta = serde_json::from_str(&contents)?;
    let log_dir = config.internal_dir.join("daemon").join("logs");
    Ok(log_dir.join(format!("{}.log", meta.pid)))
}

fn write_pid_metadata(config: &DaemonConfig) -> Result<(), GitAiError> {
    let meta = DaemonPidMeta {
        pid: std::process::id(),
        started_at_ns: now_unix_nanos(),
    };
    let path = pid_metadata_path(config);
    fs::write(path, serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}

/// Read the PID of the currently running daemon from the pid metadata file.
pub fn read_daemon_pid(config: &DaemonConfig) -> Result<u32, GitAiError> {
    let meta_path = pid_metadata_path(config);
    let contents = fs::read_to_string(&meta_path).map_err(|e| {
        GitAiError::Generic(format!(
            "failed to read daemon pid metadata at {}: {}",
            meta_path.display(),
            e
        ))
    })?;
    let meta: DaemonPidMeta = serde_json::from_str(&contents)?;
    Ok(meta.pid)
}

fn remove_pid_metadata(config: &DaemonConfig) -> Result<(), GitAiError> {
    let path = pid_metadata_path(config);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Remove daemon artifacts that may be inaccessible due to ownership mismatch
/// (e.g. left by a prior root invocation). Called only from `run_daemon()` at
/// startup — never from probe functions — so it cannot break flock visibility
/// for read-only lock checks.
#[cfg(unix)]
pub(crate) fn remove_stale_daemon_files(config: &DaemonConfig) {
    let pid_path = pid_metadata_path(config);
    for path in [
        config.lock_path.as_path(),
        config.control_socket_path.as_path(),
        config.trace_socket_path.as_path(),
        pid_path.as_path(),
    ] {
        let dominated_by_wrong_owner = match std::fs::metadata(path) {
            Ok(meta) => {
                use std::os::unix::fs::MetadataExt;
                meta.uid() != unsafe { libc::getuid() }
            }
            Err(_) => false,
        };
        if dominated_by_wrong_owner {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn remove_stale_daemon_files(_config: &DaemonConfig) {}

#[cfg(not(windows))]
fn daemon_is_test_mode() -> bool {
    std::env::var_os("GIT_AI_TEST_DB_PATH").is_some()
        || std::env::var_os("GITAI_TEST_DB_PATH").is_some()
}

fn daemon_log_dir(config: &DaemonConfig) -> PathBuf {
    config.internal_dir.join("daemon").join("logs")
}

/// Redirect stdout and stderr to a per-PID log file inside the daemon logs
/// directory. Skipped in test mode to keep test output on the console.
/// Returns a guard that keeps the log file open for the lifetime of the daemon.
#[cfg(unix)]
fn maybe_setup_daemon_log_file(config: &DaemonConfig) -> Option<DaemonLogGuard> {
    if daemon_is_test_mode() {
        return None;
    }
    match setup_daemon_log_file(config) {
        Ok(guard) => Some(guard),
        Err(e) => {
            tracing::error!(%e, "log file setup failed");
            None
        }
    }
}

#[cfg(windows)]
fn maybe_setup_daemon_log_file(config: &DaemonConfig) -> Option<DaemonLogGuard> {
    match setup_daemon_log_file(config) {
        Ok(guard) => Some(guard),
        Err(e) => {
            tracing::error!(%e, "log file setup failed");
            None
        }
    }
}

struct DaemonLogGuard {
    _file: File,
}

#[cfg(unix)]
fn setup_daemon_log_file(config: &DaemonConfig) -> Result<DaemonLogGuard, GitAiError> {
    use std::os::unix::io::AsRawFd;

    let log_dir = daemon_log_dir(config);
    fs::create_dir_all(&log_dir)?;

    let prune_dir = log_dir.clone();
    std::thread::spawn(move || prune_stale_daemon_logs(&prune_dir));

    let log_path = log_dir.join(format!("{}.log", std::process::id()));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let fd = file.as_raw_fd();
    // SAFETY: dup2 is a standard POSIX call; we redirect stdout/stderr to our
    // open log file descriptor. The file is kept alive by the returned guard.
    unsafe {
        if libc::dup2(fd, libc::STDOUT_FILENO) == -1 {
            return Err(GitAiError::Generic("dup2 stdout failed".to_string()));
        }
        if libc::dup2(fd, libc::STDERR_FILENO) == -1 {
            return Err(GitAiError::Generic("dup2 stderr failed".to_string()));
        }
    }

    Ok(DaemonLogGuard { _file: file })
}

#[cfg(windows)]
fn setup_daemon_log_file(config: &DaemonConfig) -> Result<DaemonLogGuard, GitAiError> {
    let log_dir = daemon_log_dir(config);
    fs::create_dir_all(&log_dir)?;

    let prune_dir = log_dir.clone();
    std::thread::spawn(move || prune_stale_daemon_logs(&prune_dir));

    let log_path = log_dir.join(format!("{}.log", std::process::id()));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    redirect_windows_stdio_to_log_file(&file)?;
    eprintln!("[git-ai] daemon log initialized at {}", log_path.display());

    Ok(DaemonLogGuard { _file: file })
}

#[cfg(windows)]
fn redirect_windows_stdio_to_log_file(file: &File) -> Result<(), GitAiError> {
    redirect_windows_stdio_stream(file, 1, WINDOWS_STDOUT_HANDLE)?;
    redirect_windows_stdio_stream(file, 2, WINDOWS_STDERR_HANDLE)?;
    Ok(())
}

#[cfg(windows)]
fn redirect_windows_stdio_stream(
    file: &File,
    std_fd: libc::c_int,
    std_handle: u32,
) -> Result<(), GitAiError> {
    let clone = file.try_clone()?;
    let raw_handle = clone.into_raw_handle();
    let fd = unsafe {
        libc::open_osfhandle(
            raw_handle as libc::intptr_t,
            libc::O_APPEND | libc::O_BINARY,
        )
    };
    if fd == -1 {
        unsafe {
            drop(File::from_raw_handle(raw_handle));
        }
        return Err(GitAiError::Generic(format!(
            "open_osfhandle failed for daemon log stream {}: {}",
            std_fd,
            std::io::Error::last_os_error()
        )));
    }

    let dup_result = unsafe { libc::dup2(fd, std_fd) };
    if dup_result == -1 {
        let err = std::io::Error::last_os_error();
        let _ = unsafe { libc::close(fd) };
        return Err(GitAiError::Generic(format!(
            "dup2 failed for daemon log stream {}: {}",
            std_fd, err
        )));
    }
    if unsafe { libc::close(fd) } == -1 {
        tracing::debug!(
            std_fd,
            error = %std::io::Error::last_os_error(),
            "close failed for log stream after successful redirect"
        );
    }

    let set_handle_result = unsafe { SetStdHandle(std_handle, file.as_raw_handle()) };
    if set_handle_result == 0 {
        return Err(GitAiError::Generic(format!(
            "SetStdHandle failed for daemon log stream {}: {}",
            std_fd,
            std::io::Error::last_os_error()
        )));
    }

    Ok(())
}

/// Remove log files from previous daemon runs that are older than one week and
/// whose PID is no longer alive, to avoid unbounded growth while keeping recent
/// logs available for debugging.
fn prune_stale_daemon_logs(log_dir: &Path) {
    let one_week = std::time::Duration::from_secs(7 * 24 * 60 * 60);
    let entries = match fs::read_dir(log_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        let _pid: u32 = match stem.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let dominated = path
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > one_week);
        if !dominated {
            continue;
        }
        #[cfg(unix)]
        {
            if process_alive(_pid) {
                continue;
            }
        }
        let _ = fs::remove_file(&path);
    }
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // kill(pid, 0) checks existence without sending a signal.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn read_json_line<R: BufRead>(reader: &mut R) -> Result<Option<String>, GitAiError> {
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

fn read_checkpoint_body<R: BufRead>(
    reader: &mut R,
    body_bytes: usize,
) -> Result<Vec<u8>, GitAiError> {
    let mut body = vec![0; body_bytes];
    reader.read_exact(&mut body).map_err(|error| {
        GitAiError::Generic(format!(
            "failed receiving {body_bytes}-byte checkpoint body: {error}"
        ))
    })?;
    let mut delimiter = [0u8; 1];
    reader.read_exact(&mut delimiter).map_err(|error| {
        GitAiError::Generic(format!(
            "failed receiving checkpoint body delimiter: {error}"
        ))
    })?;
    if delimiter != [b'\n'] {
        return Err(GitAiError::Generic(
            "checkpoint body was not followed by a newline delimiter".to_string(),
        ));
    }
    Ok(body)
}

#[derive(Debug)]
enum FamilySequencerEntry {
    PendingRoot,
    ReadyCommand(Box<crate::daemon::domain::NormalizedCommand>),
    Checkpoint {
        request: Box<CheckpointRequest>,
        receipt_seq: u64,
        reservation: CheckpointIngressReservation,
    },
    Canceled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FamilySequencerOrder {
    started_at_ns: u128,
    ordinal: u64,
}

#[derive(Debug, Default)]
struct FamilySequencerState {
    next_ordinal: u64,
    entries: BTreeMap<FamilySequencerOrder, FamilySequencerEntry>,
}

#[derive(Debug, Clone)]
struct PendingRootSlot {
    family: String,
    order: FamilySequencerOrder,
}

type CommitFileTimestampSnapshotHandle =
    tokio::task::JoinHandle<Option<crate::authorship::attribution_recovery::FileTimestampsByPath>>;
type CommitFileTimestampSnapshotHandles = HashMap<String, CommitFileTimestampSnapshotHandle>;

const COMMIT_FILE_TIMESTAMP_SNAPSHOT_WAIT: Duration = Duration::from_millis(500);
const SESSION_EVENT_RECOVERY_PREFLIGHT_WAIT: Duration = Duration::from_secs(2);
const SESSION_EVENT_RECOVERY_PREFLIGHT_POLL: Duration = Duration::from_millis(100);

fn run_blocking_side_effect<T>(operation: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(operation)
    } else {
        operation()
    }
}

#[derive(Debug, Clone)]
struct PendingSquashMerge {
    source_head: String,
    onto: String,
}

#[derive(Debug, Clone)]
struct PendingCherryPickNoCommit {
    source_commits: Vec<String>,
    head: String,
}

#[derive(Debug, Clone)]
struct PendingRebase {
    original_head: String,
    onto: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum RecentReplayPrerequisite {
    CheckoutSwitchRename {
        target_head: String,
        old_head: String,
    },
    CheckoutSwitchMerge {
        target_head: String,
        old_head: String,
        final_state: HashMap<String, String>,
    },
}

#[derive(Debug, Default, Clone)]
struct TraceIngressState {
    root_worktrees: HashMap<String, PathBuf>,
    root_families: HashMap<String, String>,
    root_argv: HashMap<String, Vec<String>>,
    root_started_at_ns: HashMap<String, u128>,
    root_reflog_start_offsets: HashMap<String, HashMap<String, u64>>,
    root_mutating: HashMap<String, bool>,
    root_target_repo_only: HashMap<String, bool>,
    root_last_activity_ns: HashMap<String, u64>,
    /// Roots whose start event was identified as definitely read-only. All
    /// subsequent events for these roots (including exit) take the fast path.
    root_definitely_read_only: HashSet<String>,
    root_open_connections: HashMap<String, usize>,
    unidentified_open_connections: usize,
    root_close_markers_enqueued: HashSet<String>,
}

#[doc(hidden)]
pub struct ActorDaemonCoordinator {
    backend: Arc<crate::daemon::git_backend::SystemGitBackend>,
    coordinator:
        Arc<crate::daemon::coordinator::Coordinator<crate::daemon::git_backend::SystemGitBackend>>,
    normalizer: AsyncMutex<
        crate::daemon::trace_normalizer::TraceNormalizer<
            crate::daemon::git_backend::SystemGitBackend,
        >,
    >,
    pending_rebase_original_head_by_worktree: Mutex<HashMap<String, PendingRebase>>,
    pending_cherry_pick_sources_by_worktree: Mutex<HashMap<String, Vec<String>>>,
    pending_cherry_pick_no_commit_by_worktree: Mutex<HashMap<String, PendingCherryPickNoCommit>>,
    pending_squash_merge_by_worktree: Mutex<HashMap<String, PendingSquashMerge>>,
    inflight_effects_by_family: Mutex<HashMap<String, usize>>,
    /// Files with an in-flight AI edit (PreFileEdit received, PostFileEdit not yet completed).
    /// Outer key: family. Inner key: absolute file path string. Value: registration timestamp (nanos).
    pending_ai_edits_by_family: Mutex<HashMap<String, HashMap<String, u128>>>,
    family_sequencers_by_family: Mutex<HashMap<String, FamilySequencerState>>,
    pending_root_slots_by_root: Mutex<HashMap<String, PendingRootSlot>>,
    commit_file_timestamp_snapshots_by_root:
        Mutex<HashMap<String, CommitFileTimestampSnapshotHandles>>,
    recent_replay_prerequisites_by_family:
        Mutex<HashMap<String, VecDeque<RecentReplayPrerequisite>>>,
    side_effect_errors_by_family: Mutex<HashMap<String, BTreeMap<u64, String>>>,
    side_effect_exec_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    checkpoint_side_effect_semaphore: Semaphore,
    checkpoint_ingress_quota: Arc<CheckpointIngressQuota>,
    checkpoint_ingress_tx: std::sync::OnceLock<mpsc::Sender<AcceptedCheckpoint>>,
    next_checkpoint_receipt_seq: AtomicUsize,
    processed_checkpoint_receipt_seq: AtomicUsize,
    unadmitted_checkpoints: AtomicUsize,
    accepting_checkpoints: AtomicBool,
    checkpoint_progress_notify: Notify,
    bash_sessions: Mutex<crate::daemon::bash_sessions::BashSessionState>,
    test_completion_log_dir: Option<PathBuf>,
    test_completion_log_lock: Mutex<()>,
    // OnceLock: set once at worker start, never cleared. The ingest worker
    // exits via the shutdown select! arm instead of relying on channel closure.
    trace_ingest_tx: std::sync::OnceLock<mpsc::Sender<Value>>,
    telemetry_worker: Option<crate::daemon::telemetry_worker::DaemonTelemetryWorkerHandle>,
    stream_worker: Option<crate::daemon::stream_worker::StreamWorkerHandle>,
    transcript_shutdown_notify: std::sync::OnceLock<Arc<tokio::sync::Notify>>,
    streams_db: Option<Arc<crate::streams::db::StreamsDatabase>>,
    next_trace_ingest_seq: AtomicUsize,
    queued_trace_payloads: AtomicUsize,
    queued_trace_payloads_by_root: Mutex<HashMap<String, usize>>,
    processed_trace_ingest_seq: AtomicUsize,
    trace_ingest_progress_notify: Notify,
    trace_ingress_state: Mutex<TraceIngressState>,
    shutting_down: AtomicBool,
    shutdown_action: AtomicU8,
    shutdown_notify: Notify,
    shutdown_condvar: std::sync::Condvar,
    shutdown_condvar_mutex: Mutex<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DaemonExitAction {
    Stop,
    Restart,
    RestartAfterUpdate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DaemonSelfUpdateOutcome {
    Installed,
    NoUpdate,
    Failed,
}

impl DaemonExitAction {
    fn as_u8(self) -> u8 {
        match self {
            Self::Stop => 0,
            Self::Restart => 1,
            Self::RestartAfterUpdate => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Restart,
            2 => Self::RestartAfterUpdate,
            _ => Self::Stop,
        }
    }
}

enum TracePayloadApplyOutcome {
    None,
    Applied(Box<crate::daemon::domain::AppliedCommand>),
    QueuedFamily,
}

impl ActorDaemonCoordinator {
    fn new() -> Self {
        let backend = Arc::new(crate::daemon::git_backend::SystemGitBackend::new());
        Self {
            coordinator: Arc::new(crate::daemon::coordinator::Coordinator::new(
                backend.clone(),
            )),
            normalizer: AsyncMutex::new(crate::daemon::trace_normalizer::TraceNormalizer::new(
                backend.clone(),
            )),
            backend,
            pending_rebase_original_head_by_worktree: Mutex::new(HashMap::new()),
            pending_cherry_pick_sources_by_worktree: Mutex::new(HashMap::new()),
            pending_cherry_pick_no_commit_by_worktree: Mutex::new(HashMap::new()),
            pending_squash_merge_by_worktree: Mutex::new(HashMap::new()),
            inflight_effects_by_family: Mutex::new(HashMap::new()),
            pending_ai_edits_by_family: Mutex::new(HashMap::new()),
            family_sequencers_by_family: Mutex::new(HashMap::new()),
            pending_root_slots_by_root: Mutex::new(HashMap::new()),
            commit_file_timestamp_snapshots_by_root: Mutex::new(HashMap::new()),
            recent_replay_prerequisites_by_family: Mutex::new(HashMap::new()),
            side_effect_errors_by_family: Mutex::new(HashMap::new()),
            side_effect_exec_locks: Mutex::new(HashMap::new()),
            checkpoint_side_effect_semaphore: Semaphore::new(CHECKPOINT_FAMILY_DRAIN_CONCURRENCY),
            checkpoint_ingress_quota: Arc::new(CheckpointIngressQuota::new(
                CHECKPOINT_INGRESS_REQUEST_LIMIT,
                CHECKPOINT_INGRESS_BYTE_LIMIT,
            )),
            checkpoint_ingress_tx: std::sync::OnceLock::new(),
            next_checkpoint_receipt_seq: AtomicUsize::new(0),
            processed_checkpoint_receipt_seq: AtomicUsize::new(0),
            unadmitted_checkpoints: AtomicUsize::new(0),
            accepting_checkpoints: AtomicBool::new(true),
            checkpoint_progress_notify: Notify::new(),
            bash_sessions: Mutex::new(crate::daemon::bash_sessions::BashSessionState::new()),
            test_completion_log_dir: std::env::var("GIT_AI_TEST_DB_PATH")
                .ok()
                .or_else(|| std::env::var("GITAI_TEST_DB_PATH").ok())
                .map(|_| {
                    DaemonConfig::from_env_or_default_paths()
                        .map(|config| config.test_completion_log_dir())
                        .unwrap_or_else(|_| {
                            std::env::temp_dir().join("git-ai-daemon-test-completions-fallback")
                        })
                }),
            test_completion_log_lock: Mutex::new(()),
            trace_ingest_tx: std::sync::OnceLock::new(),
            telemetry_worker: None,
            stream_worker: None,
            transcript_shutdown_notify: std::sync::OnceLock::new(),
            streams_db: None,
            next_trace_ingest_seq: AtomicUsize::new(0),
            queued_trace_payloads: AtomicUsize::new(0),
            queued_trace_payloads_by_root: Mutex::new(HashMap::new()),
            processed_trace_ingest_seq: AtomicUsize::new(0),
            trace_ingest_progress_notify: Notify::new(),
            trace_ingress_state: Mutex::new(TraceIngressState::default()),
            shutting_down: AtomicBool::new(false),
            shutdown_action: AtomicU8::new(DaemonExitAction::Stop.as_u8()),
            shutdown_notify: Notify::new(),
            shutdown_condvar: std::sync::Condvar::new(),
            shutdown_condvar_mutex: Mutex::new(()),
        }
    }

    fn is_shutting_down(&self) -> bool {
        // Acquire pairs with the Release store in request_shutdown so all
        // writes made before shutdown is requested are visible to the caller.
        self.shutting_down.load(Ordering::Acquire)
    }

    fn trigger_transcript_sweep(&self, trigger: crate::daemon::stream_worker::SweepTrigger) {
        let Some(worker) = &self.stream_worker else {
            tracing::debug!(trigger = %trigger, "transcript sweep trigger skipped; worker is not running");
            return;
        };

        if worker.trigger_sweep(trigger) {
            tracing::info!(trigger = %trigger, "transcript sweep trigger enqueued");
        } else {
            tracing::debug!(trigger = %trigger, "transcript sweep trigger not enqueued");
        }
    }

    fn trigger_transcript_sweep_for_recovery(
        &self,
        trigger: crate::daemon::stream_worker::SweepTrigger,
    ) -> Option<std::sync::mpsc::Receiver<Result<(), String>>> {
        let Some(worker) = &self.stream_worker else {
            tracing::debug!(trigger = %trigger, "recovery transcript sweep skipped; worker is not running");
            return None;
        };

        let completion = worker.trigger_sweep_for_recovery(trigger);
        if completion.is_some() {
            tracing::info!(trigger = %trigger, "recovery transcript sweep enqueued");
        } else {
            tracing::debug!(trigger = %trigger, "recovery transcript sweep not enqueued");
        }
        completion
    }

    fn wait_for_session_event_recovery_candidate(
        &self,
        repo: &Repository,
        commit_sha: &str,
        recovery_file_timestamps: Option<
            &crate::authorship::attribution_recovery::FileTimestampsByPath,
        >,
        unknown_by_file: &crate::authorship::attribution_recovery::UnknownLinesByFile,
    ) {
        if unknown_by_file.is_empty() {
            return;
        }
        let unknown_files = unknown_by_file
            .keys()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let mut timestamps = recovery_file_timestamps
            .map(|recovery_file_timestamps| {
                recovery_file_timestamps
                    .iter()
                    .filter(|(file_path, _)| unknown_files.contains(file_path.as_str()))
                    .flat_map(|(_, values)| values.iter().copied())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if timestamps.is_empty()
            && let Ok(workdir) = repo.workdir()
            && let Ok(fallback_timestamps) = capture_commit_file_timestamps(&workdir, commit_sha)
        {
            timestamps = fallback_timestamps
                .iter()
                .filter(|(file_path, _)| unknown_files.contains(file_path.as_str()))
                .flat_map(|(_, values)| values.iter().copied())
                .collect::<Vec<_>>();
        }
        if timestamps.is_empty() {
            timestamps = recovery_file_timestamps
                .map(|recovery_file_timestamps| {
                    recovery_file_timestamps
                        .values()
                        .flat_map(|values| values.iter().copied())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
        }
        if timestamps.is_empty()
            && let Ok(workdir) = repo.workdir()
            && let Ok(fallback_timestamps) = capture_commit_file_timestamps(&workdir, commit_sha)
        {
            timestamps = fallback_timestamps
                .values()
                .flat_map(|values| values.iter().copied())
                .collect::<Vec<_>>();
        }
        if timestamps.is_empty() {
            return;
        }
        timestamps.sort_unstable();
        timestamps.dedup();

        let Some(target_repo_url) = crate::repo_url::resolve_repo_url_from_repo(repo) else {
            return;
        };

        let has_candidate = || {
            crate::authorship::attribution_recovery::matching_session_event_candidate_exists(
                &timestamps,
                &target_repo_url,
            )
            .unwrap_or_else(|error| {
                tracing::debug!(%error, "failed checking session-event recovery candidates");
                false
            })
        };
        if has_candidate() {
            return;
        }

        let deadline = std::time::Instant::now() + SESSION_EVENT_RECOVERY_PREFLIGHT_WAIT;
        let sweep_completion = self.trigger_transcript_sweep_for_recovery(
            crate::daemon::stream_worker::SweepTrigger::PostCommit,
        );

        let Some(sweep_completion) = sweep_completion else {
            return;
        };

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            tracing::debug!(
                wait_ms = SESSION_EVENT_RECOVERY_PREFLIGHT_WAIT.as_millis() as u64,
                "recovery transcript sweep wait expired"
            );
            return;
        }
        match sweep_completion.recv_timeout(remaining) {
            Ok(Ok(())) => {
                tracing::debug!("recovery transcript sweep completed before post-commit");
            }
            Ok(Err(error)) => {
                tracing::debug!(
                    %error,
                    "recovery transcript sweep failed before post-commit"
                );
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                tracing::debug!(
                    wait_ms = SESSION_EVENT_RECOVERY_PREFLIGHT_WAIT.as_millis() as u64,
                    "recovery transcript sweep wait expired"
                );
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::debug!("recovery transcript sweep completion channel disconnected");
                return;
            }
        }

        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                tracing::debug!(
                    wait_ms = SESSION_EVENT_RECOVERY_PREFLIGHT_WAIT.as_millis() as u64,
                    "session-event recovery preflight wait expired"
                );
                return;
            }
            std::thread::sleep(remaining.min(SESSION_EVENT_RECOVERY_PREFLIGHT_POLL));
            if has_candidate() {
                tracing::debug!(
                    "session-event recovery candidate became visible before post-commit"
                );
                return;
            }
            if std::time::Instant::now() >= deadline {
                tracing::debug!(
                    wait_ms = SESSION_EVENT_RECOVERY_PREFLIGHT_WAIT.as_millis() as u64,
                    "session-event recovery preflight wait expired"
                );
                return;
            }
        }
    }

    fn request_shutdown(&self) {
        // Release ensures that any writes made before this store are visible to
        // threads that subsequently load with Acquire (is_shutting_down).
        self.shutting_down.store(true, Ordering::Release);
        // The ingest worker exits via its select! shutdown arm (watching
        // shutdown_notify); we no longer rely on channel closure to stop it.
        self.shutdown_notify.notify_waiters();
        if let Some(transcript_shutdown) = self.transcript_shutdown_notify.get() {
            transcript_shutdown.notify_one();
        }
        // Hold the condvar mutex so notify_all cannot race with the
        // check-then-wait sequence in daemon_update_check_loop.
        let _guard = self
            .shutdown_condvar_mutex
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.shutdown_condvar.notify_all();
    }

    fn request_stop(&self) {
        self.shutdown_action
            .store(DaemonExitAction::Stop.as_u8(), Ordering::SeqCst);
        self.request_shutdown();
    }

    fn request_restart(&self) {
        self.shutdown_action
            .store(DaemonExitAction::Restart.as_u8(), Ordering::SeqCst);
        self.request_shutdown();
    }

    fn request_restart_after_update(&self) {
        self.shutdown_action.store(
            DaemonExitAction::RestartAfterUpdate.as_u8(),
            Ordering::SeqCst,
        );
        self.request_shutdown();
    }

    fn shutdown_action(&self) -> DaemonExitAction {
        DaemonExitAction::from_u8(self.shutdown_action.load(Ordering::SeqCst))
    }

    async fn wait_for_shutdown(&self) {
        // Register the Notified future BEFORE checking the flag so that a
        // request_shutdown() racing between the check and the await cannot
        // slip through without waking us (notify_waiters only wakes futures
        // that are already registered).
        let notified = self.shutdown_notify.notified();
        if self.is_shutting_down() {
            return;
        }
        notified.await;
    }

    fn begin_family_effect(&self, family: &str) -> Result<(), GitAiError> {
        let mut map = self
            .inflight_effects_by_family
            .lock()
            .map_err(|_| GitAiError::Generic("inflight effects map lock poisoned".to_string()))?;
        let entry = map.entry(family.to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
        Ok(())
    }

    fn end_family_effect(&self, family: &str) -> Result<(), GitAiError> {
        let mut map = self
            .inflight_effects_by_family
            .lock()
            .map_err(|_| GitAiError::Generic("inflight effects map lock poisoned".to_string()))?;
        if let Some(entry) = map.get_mut(family) {
            if *entry <= 1 {
                map.remove(family);
            } else {
                *entry -= 1;
            }
        }
        Ok(())
    }

    /// Garbage-collect empty or idle entries from per-family and per-root maps
    /// to prevent unbounded memory growth in long-running daemon processes.
    fn gc_stale_family_state(&self) {
        // NOTE: Do NOT call normalizer.sweep_orphans() here — it removes ALL
        // pending/deferred roots unconditionally which destroys in-flight trace
        // state.  sweep_orphans() is only safe at daemon shutdown.
        if let Ok(mut map) = self.recent_replay_prerequisites_by_family.lock() {
            map.retain(|_, entries| !entries.is_empty());
        }
        if let Ok(mut map) = self.side_effect_errors_by_family.lock() {
            map.retain(|_, errors| !errors.is_empty());
        }
        if let Ok(mut map) = self.family_sequencers_by_family.lock() {
            map.retain(|_, state| !state.entries.is_empty());
        }
        if let Ok(mut map) = self.side_effect_exec_locks.lock() {
            // Evict only IDLE locks (strong_count == 1: the map holds the sole
            // Arc). A lock with clones out is held or awaited by a drain;
            // evicting it would hand the next drain a fresh unlocked mutex and
            // run two drains concurrently on one family, tearing working-log
            // read-modify-writes. The map mutex makes this check atomic with
            // removal; idle locks are safely recreated on demand.
            map.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
        if let Ok(mut map) = self.pending_rebase_original_head_by_worktree.lock() {
            map.shrink_to_fit();
        }
        if let Ok(mut map) = self.pending_cherry_pick_sources_by_worktree.lock() {
            map.retain(|_, sources| !sources.is_empty());
        }
        if let Ok(mut map) = self.pending_squash_merge_by_worktree.lock() {
            map.retain(|_, pending| {
                !pending.source_head.trim().is_empty() && !pending.onto.trim().is_empty()
            });
        }
        if let Ok(mut map) = self.queued_trace_payloads_by_root.lock() {
            map.retain(|_, count| *count > 0);
        }
        // Clean expired pending AI edit entries (older than 10s).
        {
            const PENDING_AI_EDIT_TIMEOUT_NS: u128 = 10_000_000_000;
            let gc_now_ns = now_unix_nanos();
            if let Ok(mut map) = self.pending_ai_edits_by_family.lock() {
                for family_map in map.values_mut() {
                    family_map.retain(|_, registered_at| {
                        gc_now_ns.saturating_sub(*registered_at) < PENDING_AI_EDIT_TIMEOUT_NS
                    });
                }
                map.retain(|_, family_map| !family_map.is_empty());
            }
        }
    }

    fn canonicalize_path(path: &str) -> String {
        std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string())
    }

    fn register_pending_ai_edits(&self, family: &str, file_paths: &[String]) {
        let now_ns = now_unix_nanos();
        if let Ok(mut map) = self.pending_ai_edits_by_family.lock() {
            let family_map = map.entry(family.to_string()).or_default();
            for file in file_paths {
                family_map.insert(Self::canonicalize_path(file), now_ns);
            }
        }
    }

    fn clear_pending_ai_edits(&self, family: &str, file_paths: &[String]) {
        if let Ok(mut map) = self.pending_ai_edits_by_family.lock()
            && let Some(family_map) = map.get_mut(family)
        {
            for file in file_paths {
                family_map.remove(&Self::canonicalize_path(file));
            }
            if family_map.is_empty() {
                map.remove(family);
            }
        }
    }

    fn file_has_pending_ai_edit(&self, family: &str, file_path: &str) -> bool {
        const PENDING_AI_EDIT_TIMEOUT_NS: u128 = 10_000_000_000; // 10 seconds
        let now_ns = now_unix_nanos();
        let canonical = Self::canonicalize_path(file_path);
        if let Ok(map) = self.pending_ai_edits_by_family.lock()
            && let Some(family_map) = map.get(family)
        {
            return family_map.get(&canonical).is_some_and(|registered_at| {
                now_ns.saturating_sub(*registered_at) < PENDING_AI_EDIT_TIMEOUT_NS
            });
        }
        false
    }

    fn trace_invocation_participates_in_family_sequencer(
        primary_command: Option<&str>,
        argv: &[String],
    ) -> bool {
        primary_command.is_some_and(|cmd| {
            crate::git::command_classification::git_invocation_participates_in_family_sequencer(
                cmd,
                &trace_invocation_command_args(Some(cmd), argv),
            )
        })
    }

    fn append_pending_root_entry(
        &self,
        family: &str,
        root_sid: &str,
        started_at_ns: u128,
    ) -> Result<(), GitAiError> {
        {
            let pending_slots = self.pending_root_slots_by_root.lock().map_err(|_| {
                GitAiError::Generic("pending root slots map lock poisoned".to_string())
            })?;
            if pending_slots.contains_key(root_sid) {
                return Ok(());
            }
        }

        let order = {
            let mut sequencers = self.family_sequencers_by_family.lock().map_err(|_| {
                GitAiError::Generic("family sequencer map lock poisoned".to_string())
            })?;
            let state =
                sequencers
                    .entry(family.to_string())
                    .or_insert_with(|| FamilySequencerState {
                        next_ordinal: 1,
                        entries: BTreeMap::new(),
                    });
            let order = FamilySequencerOrder {
                started_at_ns,
                ordinal: state.next_ordinal,
            };
            state.next_ordinal = state.next_ordinal.saturating_add(1);
            state
                .entries
                .insert(order, FamilySequencerEntry::PendingRoot);
            order
        };

        self.pending_root_slots_by_root
            .lock()
            .map_err(|_| GitAiError::Generic("pending root slots map lock poisoned".to_string()))?
            .insert(
                root_sid.to_string(),
                PendingRootSlot {
                    family: family.to_string(),
                    order,
                },
            );
        Ok(())
    }

    fn take_pending_root_slot(
        &self,
        root_sid: &str,
    ) -> Result<Option<PendingRootSlot>, GitAiError> {
        self.pending_root_slots_by_root
            .lock()
            .map_err(|_| GitAiError::Generic("pending root slots map lock poisoned".to_string()))
            .map(|mut slots| slots.remove(root_sid))
    }

    fn maybe_append_pending_root_from_trace_payload(
        &self,
        payload: &Value,
    ) -> Result<(), GitAiError> {
        let event = payload
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if event == TRACE_CONNECTION_CLOSED_EVENT {
            return Ok(());
        }

        let Some(sid) = payload.get("sid").and_then(Value::as_str) else {
            return Ok(());
        };
        let root_sid = trace_root_sid(sid);
        if root_sid != sid {
            return Ok(());
        }

        let argv = trace_payload_effective_argv(payload);
        let primary_command =
            trace_payload_primary_command(payload).or_else(|| trace_argv_primary_command(&argv));
        if !Self::trace_invocation_participates_in_family_sequencer(
            primary_command.as_deref(),
            &argv,
        ) {
            return Ok(());
        }

        let Some(worktree) = trace_payload_worktree_hint(payload) else {
            return Ok(());
        };
        let Some(common_dir) = common_dir_for_worktree(&worktree) else {
            return Ok(());
        };
        let started_at_ns = trace_payload_root_started_at_ns(payload)
            .or_else(|| trace_payload_time_ns(payload))
            .unwrap_or_else(now_unix_nanos);
        let family = common_dir
            .canonicalize()
            .unwrap_or(common_dir)
            .to_string_lossy()
            .to_string();
        self.append_pending_root_entry(&family, root_sid, started_at_ns)
    }

    async fn append_ready_command_entry(
        &self,
        family: &str,
        command: crate::daemon::domain::NormalizedCommand,
    ) -> Result<(), GitAiError> {
        let exec_lock = self.side_effect_exec_lock(family)?;
        let _guard = exec_lock.lock().await;
        {
            let mut sequencers = self.family_sequencers_by_family.lock().map_err(|_| {
                GitAiError::Generic("family sequencer map lock poisoned".to_string())
            })?;
            let state =
                sequencers
                    .entry(family.to_string())
                    .or_insert_with(|| FamilySequencerState {
                        next_ordinal: 1,
                        entries: BTreeMap::new(),
                    });
            let order = FamilySequencerOrder {
                started_at_ns: command.started_at_ns,
                ordinal: state.next_ordinal,
            };
            state.next_ordinal = state.next_ordinal.saturating_add(1);
            state
                .entries
                .insert(order, FamilySequencerEntry::ReadyCommand(Box::new(command)));
        }
        self.drain_ready_family_sequencer_entries_locked(family)
            .await
    }

    async fn drain_ready_family_sequencer_entries(&self, family: &str) -> Result<(), GitAiError> {
        let exec_lock = self.side_effect_exec_lock(family)?;
        let _guard = exec_lock.lock().await;
        self.drain_ready_family_sequencer_entries_locked(family)
            .await
    }

    async fn drain_all_ready_family_sequencers(&self) -> Result<(), GitAiError> {
        let families = {
            let map = self.family_sequencers_by_family.lock().map_err(|_| {
                GitAiError::Generic("family sequencer map lock poisoned".to_string())
            })?;
            map.iter()
                .filter(|(_, state)| !state.entries.is_empty())
                .map(|(family, _)| family.clone())
                .collect::<Vec<_>>()
        };
        let first_error = stream::iter(families)
            .map(|family| async move { self.drain_ready_family_sequencer_entries(&family).await })
            .buffer_unordered(CHECKPOINT_FAMILY_DRAIN_CONCURRENCY)
            .fold(None, |first_error, result| async move {
                first_error.or_else(|| result.err())
            })
            .await;
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    async fn drain_ready_family_sequencers_after_root_cleared(
        &self,
        family: Option<String>,
    ) -> Result<(), GitAiError> {
        if let Some(family) = family {
            self.drain_ready_family_sequencer_entries(&family).await
        } else {
            self.drain_all_ready_family_sequencers().await
        }
    }

    async fn replace_pending_root_entry(
        &self,
        root_sid: &str,
        replacement: FamilySequencerEntry,
    ) -> Result<Option<String>, GitAiError> {
        let Some(slot) = self.take_pending_root_slot(root_sid)? else {
            return Ok(None);
        };
        let family = slot.family.clone();
        let exec_lock = self.side_effect_exec_lock(&family)?;
        let _guard = exec_lock.lock().await;
        {
            let mut sequencers = self.family_sequencers_by_family.lock().map_err(|_| {
                GitAiError::Generic("family sequencer map lock poisoned".to_string())
            })?;
            let state = sequencers
                .entry(family.clone())
                .or_insert_with(|| FamilySequencerState {
                    next_ordinal: 1,
                    entries: BTreeMap::new(),
                });
            let Some(entry) = state.entries.get_mut(&slot.order) else {
                return Err(GitAiError::Generic(format!(
                    "missing pending root sequencer entry for sid={} family={} order={:?}",
                    root_sid, family, slot.order
                )));
            };
            match entry {
                FamilySequencerEntry::PendingRoot => {
                    *entry = replacement;
                }
                _ => {
                    return Err(GitAiError::Generic(format!(
                        "sequencer entry for sid={} family={} order={:?} was not pending",
                        root_sid, family, slot.order
                    )));
                }
            }
        }
        self.drain_ready_family_sequencer_entries_locked(&family)
            .await?;
        Ok(Some(family))
    }

    fn family_entry_blocked_by_prior_open_trace_root(
        &self,
        family: &str,
        started_at_ns: u128,
        entry_root_sid: Option<&str>,
    ) -> Result<bool, GitAiError> {
        let ingress = self
            .trace_ingress_state
            .lock()
            .map_err(|_| GitAiError::Generic("trace ingress state lock poisoned".to_string()))?;

        for (root_sid, open_count) in &ingress.root_open_connections {
            if *open_count == 0 || entry_root_sid == Some(root_sid.as_str()) {
                continue;
            }
            if ingress.root_definitely_read_only.contains(root_sid) {
                continue;
            }
            if !ingress.root_mutating.get(root_sid).copied().unwrap_or(true) {
                continue;
            }
            if ingress
                .root_started_at_ns
                .get(root_sid)
                .copied()
                .is_some_and(|root_started| root_started > started_at_ns)
            {
                continue;
            }
            if ingress
                .root_families
                .get(root_sid)
                .is_none_or(|root_family| root_family == family)
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn record_side_effect_error(
        &self,
        family: &str,
        seq: u64,
        error: &GitAiError,
    ) -> Result<(), GitAiError> {
        let mut map = self
            .side_effect_errors_by_family
            .lock()
            .map_err(|_| GitAiError::Generic("side effect errors map lock poisoned".to_string()))?;
        let family_errors = map.entry(family.to_string()).or_insert_with(BTreeMap::new);
        family_errors.insert(seq, error.to_string());
        while family_errors.len() > 256 {
            if let Some(oldest) = family_errors.keys().next().copied() {
                family_errors.remove(&oldest);
            } else {
                break;
            }
        }
        Ok(())
    }

    fn latest_side_effect_error(&self, family: &str) -> Result<Option<String>, GitAiError> {
        let map = self
            .side_effect_errors_by_family
            .lock()
            .map_err(|_| GitAiError::Generic("side effect errors map lock poisoned".to_string()))?;
        Ok(map
            .get(family)
            .and_then(|errors| errors.iter().next_back().map(|(_, error)| error.clone())))
    }

    fn record_recent_replay_prerequisite(
        &self,
        family: &str,
        prerequisite: RecentReplayPrerequisite,
    ) -> Result<(), GitAiError> {
        const MAX_RECENT_REPLAY_PREREQUISITES_PER_FAMILY: usize = 256;

        let mut map = self
            .recent_replay_prerequisites_by_family
            .lock()
            .map_err(|_| {
                GitAiError::Generic("recent replay prerequisites map lock poisoned".to_string())
            })?;
        let entries = map.entry(family.to_string()).or_insert_with(VecDeque::new);
        entries.push_back(prerequisite);
        while entries.len() > MAX_RECENT_REPLAY_PREREQUISITES_PER_FAMILY {
            let _ = entries.pop_front();
        }
        Ok(())
    }

    fn maybe_append_test_completion_log(
        &self,
        family: &str,
        entry: &TestCompletionLogEntry,
    ) -> Result<(), GitAiError> {
        let Some(dir) = self.test_completion_log_dir.as_ref() else {
            return Ok(());
        };
        let _guard = self
            .test_completion_log_lock
            .lock()
            .map_err(|_| GitAiError::Generic("test completion log lock poisoned".to_string()))?;

        fs::create_dir_all(dir)?;
        let mut hasher = Sha256::new();
        hasher.update(family.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        let path = dir.join(format!("{}.jsonl", &digest[..16]));
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let line = serde_json::to_string(entry).map_err(GitAiError::from)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }

    fn append_command_completion_log(
        &self,
        family: &str,
        applied: &crate::daemon::domain::AppliedCommand,
        result: &Result<(), GitAiError>,
        error_order: u64,
    ) -> Result<(), GitAiError> {
        let sync_tracked = crate::daemon::test_sync::tracks_primary_command_for_test_sync(
            applied.command.primary_command.as_deref(),
            &applied.command.invoked_args,
        );
        let test_sync_session = crate::daemon::test_sync::test_sync_session_from_invocation(
            &parsed_invocation_for_normalized_command(&applied.command),
        );
        let log_entry = TestCompletionLogEntry {
            seq: applied.seq,
            family_key: family.to_string(),
            kind: "command".to_string(),
            primary_command: applied.command.primary_command.clone(),
            test_sync_session,
            exit_code: Some(applied.command.exit_code),
            sync_tracked,
            status: if result.is_ok() {
                "ok".to_string()
            } else {
                "error".to_string()
            },
            error: result.as_ref().err().map(|error| error.to_string()),
        };
        if let Err(error) = self.maybe_append_test_completion_log(family, &log_entry) {
            let _ = self.record_side_effect_error(family, error_order, &error);
            return Err(error);
        }
        Ok(())
    }

    fn trace_root_connection_opened(&self, root_sid: &str) -> Result<(), GitAiError> {
        let mut ingress = self
            .trace_ingress_state
            .lock()
            .map_err(|_| GitAiError::Generic("trace ingress state lock poisoned".to_string()))?;
        *ingress
            .root_open_connections
            .entry(root_sid.to_string())
            .or_insert(0) += 1;
        Ok(())
    }

    fn trace_root_needs_close_marker(ingress: &TraceIngressState, root_sid: &str) -> bool {
        if ingress.root_definitely_read_only.contains(root_sid) {
            return false;
        }
        ingress
            .root_mutating
            .get(root_sid)
            .copied()
            .unwrap_or(false)
            || ingress.root_reflog_start_offsets.contains_key(root_sid)
    }

    fn clear_trace_ingress_root_locked(ingress: &mut TraceIngressState, root_sid: &str) {
        ingress.root_worktrees.remove(root_sid);
        ingress.root_families.remove(root_sid);
        ingress.root_argv.remove(root_sid);
        ingress.root_started_at_ns.remove(root_sid);
        ingress.root_reflog_start_offsets.remove(root_sid);
        ingress.root_mutating.remove(root_sid);
        ingress.root_target_repo_only.remove(root_sid);
        ingress.root_last_activity_ns.remove(root_sid);
        ingress.root_definitely_read_only.remove(root_sid);
        ingress.root_open_connections.remove(root_sid);
        ingress.root_close_markers_enqueued.remove(root_sid);
    }

    fn record_trace_connection_close(&self, roots: &[String]) -> Result<Vec<String>, GitAiError> {
        let mut close_marker_candidates = Vec::new();
        let mut ingress = self
            .trace_ingress_state
            .lock()
            .map_err(|_| GitAiError::Generic("trace ingress state lock poisoned".to_string()))?;
        for root_sid in roots {
            if let Some(count) = ingress.root_open_connections.get_mut(root_sid) {
                if *count > 1 {
                    *count -= 1;
                    continue;
                }
                ingress.root_open_connections.remove(root_sid);
            }
            if !Self::trace_root_needs_close_marker(&ingress, root_sid) {
                Self::clear_trace_ingress_root_locked(&mut ingress, root_sid);
                continue;
            }
            if ingress.root_close_markers_enqueued.contains(root_sid) {
                continue;
            }
            ingress.root_close_markers_enqueued.insert(root_sid.clone());
            close_marker_candidates.push(root_sid.clone());
        }
        self.trace_ingest_progress_notify.notify_waiters();
        Ok(close_marker_candidates)
    }

    fn enqueue_trace_connection_close_markers(&self, roots: Vec<String>) -> Result<(), GitAiError> {
        for root_sid in roots {
            self.enqueue_trace_payload(json!({
                "event": TRACE_CONNECTION_CLOSED_EVENT,
                "sid": root_sid,
                "time_ns": now_unix_nanos() as u64,
            }))?;
        }
        Ok(())
    }

    fn trace_unidentified_connection_opened(&self) -> Result<(), GitAiError> {
        let mut ingress = self
            .trace_ingress_state
            .lock()
            .map_err(|_| GitAiError::Generic("trace ingress state lock poisoned".to_string()))?;
        ingress.unidentified_open_connections =
            ingress.unidentified_open_connections.saturating_add(1);
        self.trace_ingest_progress_notify.notify_waiters();
        Ok(())
    }

    fn trace_unidentified_connection_identified_or_closed(&self) -> Result<(), GitAiError> {
        let mut ingress = self
            .trace_ingress_state
            .lock()
            .map_err(|_| GitAiError::Generic("trace ingress state lock poisoned".to_string()))?;
        ingress.unidentified_open_connections =
            ingress.unidentified_open_connections.saturating_sub(1);
        self.trace_ingest_progress_notify.notify_waiters();
        Ok(())
    }

    fn trace_payload_root_sid(payload: &Value) -> Option<String> {
        payload
            .get("sid")
            .and_then(Value::as_str)
            .map(|sid| trace_root_sid(sid).to_string())
    }

    fn record_trace_payload_enqueued(&self, payload: &Value) -> Result<(), GitAiError> {
        self.record_trace_payload_enqueued_root(Self::trace_payload_root_sid(payload).as_deref())
    }

    fn record_trace_payload_enqueued_root(&self, root_sid: Option<&str>) -> Result<(), GitAiError> {
        let Some(root_sid) = root_sid else {
            return Ok(());
        };
        let mut queued = self.queued_trace_payloads_by_root.lock().map_err(|_| {
            GitAiError::Generic("queued trace payloads by root lock poisoned".to_string())
        })?;
        *queued.entry(root_sid.to_string()).or_insert(0) += 1;
        Ok(())
    }

    fn record_trace_payload_processed_root(
        &self,
        root_sid: Option<&str>,
    ) -> Result<(), GitAiError> {
        let Some(root_sid) = root_sid else {
            return Ok(());
        };
        let mut queued = self.queued_trace_payloads_by_root.lock().map_err(|_| {
            GitAiError::Generic("queued trace payloads by root lock poisoned".to_string())
        })?;
        if let Some(count) = queued.get_mut(root_sid) {
            if *count > 1 {
                *count -= 1;
            } else {
                queued.remove(root_sid);
            }
        }
        Ok(())
    }

    fn clear_trace_root_tracking(&self, root_sid: &str) -> Result<(), GitAiError> {
        {
            let mut ingress = self.trace_ingress_state.lock().map_err(|_| {
                GitAiError::Generic("trace ingress state lock poisoned".to_string())
            })?;
            Self::clear_trace_ingress_root_locked(&mut ingress, root_sid);
        }
        let mut queued = self.queued_trace_payloads_by_root.lock().map_err(|_| {
            GitAiError::Generic("queued trace payloads by root lock poisoned".to_string())
        })?;
        queued.remove(root_sid);
        self.trace_ingest_progress_notify.notify_waiters();
        Ok(())
    }

    fn has_open_trace_roots_that_may_mutate_refs(&self) -> bool {
        let Ok(ingress) = self.trace_ingress_state.lock() else {
            return false;
        };
        ingress.root_open_connections.iter().any(|(root, count)| {
            *count > 0
                && !ingress.root_definitely_read_only.contains(root)
                && ingress.root_mutating.get(root).copied().unwrap_or(true)
        })
    }

    /// As [`Self::has_open_trace_roots_that_may_mutate_refs`], but scoped to
    /// one family: roots already attributed to a DIFFERENT family (via their
    /// `def_repo` worktree) cannot mutate this family's refs and are ignored.
    /// Roots with no family attribution yet fail closed and block everyone.
    fn has_open_trace_roots_that_may_mutate_family(&self, family: &str) -> bool {
        let Ok(ingress) = self.trace_ingress_state.lock() else {
            return false;
        };
        ingress.root_open_connections.iter().any(|(root, count)| {
            *count > 0
                && !ingress.root_definitely_read_only.contains(root)
                && ingress.root_mutating.get(root).copied().unwrap_or(true)
                && ingress
                    .root_families
                    .get(root)
                    .is_none_or(|root_family| root_family == family)
        })
    }

    fn next_trace_ingest_seq(&self) -> u64 {
        // Relaxed: we only need fetch_add atomicity (unique monotone values),
        // not ordering w.r.t. any other atomic.
        (self.next_trace_ingest_seq.fetch_add(1, Ordering::Relaxed) as u64) + 1
    }

    fn trace_ingest_queue_capacity() -> usize {
        #[cfg(feature = "test-support")]
        if let Ok(raw) = std::env::var("GIT_AI_TEST_TRACE_INGEST_QUEUE_CAPACITY")
            && let Ok(capacity) = raw.parse::<usize>()
            && capacity > 0
        {
            return capacity;
        }

        TRACE_INGEST_QUEUE_CAPACITY
    }

    fn start_trace_ingest_worker(self: &Arc<Self>) -> Result<(), GitAiError> {
        // Idempotent: if OnceLock is already set, worker is already running.
        if self.trace_ingest_tx.get().is_some() {
            return Ok(());
        }

        let queue_capacity = Self::trace_ingest_queue_capacity();
        let (tx, mut rx) = mpsc::channel::<Value>(queue_capacity);
        // OnceLock::set fails if another thread raced us to initialize — that
        // means the worker is already running; just drop our channel ends.
        if self.trace_ingest_tx.set(tx).is_err() {
            return Ok(());
        }

        let coordinator = self.clone();
        tokio::spawn(async move {
            #[cfg(feature = "test-support")]
            if let Ok(raw_delay_ms) =
                std::env::var("GIT_AI_TEST_TRACE_INGEST_WORKER_START_DELAY_MS")
                && let Ok(delay_ms) = raw_delay_ms.parse::<u64>()
                && delay_ms > 0
            {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }

            let mut next_seq: u64 = 1;
            let mut pending_by_seq: BTreeMap<u64, Value> = BTreeMap::new();
            let mut gc_counter: u64 = 0;
            const GC_INTERVAL: u64 = 500;

            // Previously: `while let Some(payload) = rx.recv().await { … }`
            //
            // The ingest worker used to exit when the sender was dropped by
            // `request_shutdown`.  With OnceLock the sender is never dropped
            // during the coordinator's lifetime, so we use select! to also
            // respond to the explicit shutdown signal.
            loop {
                let payload = tokio::select! {
                    biased; // prefer draining queued work over shutdown
                    maybe = rx.recv() => match maybe {
                        Some(p) => p,
                        None => break, // channel closed (coordinator dropped)
                    },
                    _ = coordinator.wait_for_shutdown() => break,
                };
                let Some(seq) = payload.get(TRACE_INGEST_SEQ_FIELD).and_then(Value::as_u64) else {
                    tracing::error!(
                        component = "daemon",
                        phase = "trace_ingest_worker",
                        reason = "missing_ingest_seq",
                        "trace ingest payload missing ingress sequence"
                    );
                    coordinator.request_shutdown();
                    break;
                };

                if pending_by_seq.len() >= queue_capacity {
                    tracing::error!(
                        component = "daemon",
                        phase = "trace_ingest_worker",
                        reason = "reorder_buffer_overflow",
                        buffered_count = pending_by_seq.len(),
                        next_seq,
                        received_seq = seq,
                        "trace ingest reorder buffer overflow"
                    );
                    coordinator.request_shutdown();
                    break;
                }

                if pending_by_seq.insert(seq, payload).is_some() {
                    tracing::error!(
                        component = "daemon",
                        phase = "trace_ingest_worker",
                        reason = "duplicate_ingest_seq",
                        sequence = seq,
                        "duplicate trace ingest sequence received"
                    );
                    coordinator.request_shutdown();
                    break;
                }

                while let Some(mut ordered_payload) = pending_by_seq.remove(&next_seq) {
                    let processed_seq = next_seq;
                    if let Some(object) = ordered_payload.as_object_mut() {
                        object.remove(TRACE_INGEST_SEQ_FIELD);
                    }
                    let ordered_payload_root = Self::trace_payload_root_sid(&ordered_payload);

                    let ingest_result = {
                        let coord = coordinator.clone();
                        let future = coord.ingest_trace_payload_fast(ordered_payload);
                        let caught = std::panic::AssertUnwindSafe(future);
                        match futures::FutureExt::catch_unwind(caught).await {
                            Ok(Ok(())) => Ok(()),
                            Ok(Err(error)) => {
                                tracing::error!(
                                    component = "daemon",
                                    phase = "trace_ingest_worker",
                                    reason = "ingest_error",
                                    sequence = processed_seq,
                                    root_sid = ?ordered_payload_root,
                                    %error,
                                    "trace ingest error"
                                );
                                Err(error)
                            }
                            Err(panic_payload) => {
                                let panic_msg =
                                    if let Some(s) = panic_payload.downcast_ref::<String>() {
                                        s.clone()
                                    } else if let Some(s) = panic_payload.downcast_ref::<&str>() {
                                        s.to_string()
                                    } else {
                                        "unknown panic".to_string()
                                    };
                                tracing::error!(
                                    component = "daemon",
                                    phase = "trace_ingest_worker",
                                    reason = "panic_in_ingest",
                                    panic_msg = %panic_msg,
                                    sequence = processed_seq,
                                    "trace ingest panic"
                                );
                                Err(GitAiError::Generic(format!(
                                    "trace ingest worker panic: {}",
                                    panic_msg
                                )))
                            }
                        }
                    };
                    let _ = ingest_result;
                    let _ = coordinator.queued_trace_payloads.fetch_update(
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                        |current| Some(current.saturating_sub(1)),
                    );
                    if let Err(error) = coordinator
                        .record_trace_payload_processed_root(ordered_payload_root.as_deref())
                    {
                        tracing::debug!(
                            %error,
                            "trace payload accounting error after ingest"
                        );
                    }
                    // Release: pairs with Acquire loads in wait_for_trace_ingest_processed_through
                    // so waiters observe all ingest side-effects when seq advances.
                    coordinator
                        .processed_trace_ingest_seq
                        .store(processed_seq as usize, Ordering::Release);
                    coordinator.trace_ingest_progress_notify.notify_waiters();
                    next_seq = next_seq.saturating_add(1);
                    gc_counter += 1;
                    if gc_counter.is_multiple_of(GC_INTERVAL) {
                        coordinator.gc_stale_family_state();
                    }
                }
            }

            if !pending_by_seq.is_empty() {
                tracing::error!(
                    component = "daemon",
                    phase = "trace_ingest_worker",
                    reason = "unflushed_buffer_on_shutdown",
                    buffered_count = pending_by_seq.len(),
                    next_seq,
                    min_buffered_seq = ?pending_by_seq.keys().next().copied(),
                    max_buffered_seq = ?pending_by_seq.keys().last().copied(),
                    "trace ingest worker exiting with buffered out-of-order frames"
                );
            }
        });
        Ok(())
    }

    fn start_checkpoint_ingress_worker(self: &Arc<Self>) -> Result<(), GitAiError> {
        if self.checkpoint_ingress_tx.get().is_some() {
            return Ok(());
        }

        let (tx, mut rx) = mpsc::channel::<AcceptedCheckpoint>(CHECKPOINT_INGRESS_REQUEST_LIMIT);
        if self.checkpoint_ingress_tx.set(tx).is_err() {
            return Ok(());
        }

        let coordinator = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    biased;
                    maybe = rx.recv() => match maybe {
                        Some(accepted) => accepted,
                        None => {
                            if !coordinator.is_shutting_down() {
                                tracing::error!(
                                    component = "daemon",
                                    phase = "checkpoint_ingress_worker",
                                    reason = "ingress_channel_closed",
                                    "checkpoint ingress channel closed unexpectedly"
                                );
                                coordinator.request_shutdown();
                            }
                            break;
                        }
                    },
                    _ = coordinator.wait_for_shutdown() => break,
                };
                let receipt_seq = accepted.receipt_seq;
                let prepare = coordinator.prepare_checkpoint_admission(accepted);
                let caught = std::panic::AssertUnwindSafe(prepare);
                match futures::FutureExt::catch_unwind(caught).await {
                    Ok(Ok(prepared)) => {
                        if let Err(error) = coordinator.complete_checkpoint_admission(prepared) {
                            tracing::error!(
                                component = "daemon",
                                phase = "checkpoint_admission",
                                reason = "sequencer_admission_failed",
                                receipt_seq,
                                %error,
                                "failed admitting checkpoint to family sequencer"
                            );
                            coordinator.request_shutdown();
                            break;
                        }
                    }
                    Ok(Err(error)) => {
                        tracing::error!(
                            component = "daemon",
                            phase = "checkpoint_admission",
                            reason = "checkpoint_prepare_failed",
                            receipt_seq,
                            %error,
                            "failed preparing accepted checkpoint"
                        );
                        if let Err(accounting_error) =
                            coordinator.complete_failed_checkpoint_admission(receipt_seq)
                        {
                            tracing::error!(
                                component = "daemon",
                                phase = "checkpoint_admission",
                                reason = "failed_admission_accounting_error",
                                receipt_seq,
                                error = %accounting_error,
                                "failed releasing checkpoint admission gate"
                            );
                            coordinator.request_shutdown();
                            break;
                        }
                    }
                    Err(panic_payload) => {
                        let panic_msg =
                            if let Some(message) = panic_payload.downcast_ref::<String>() {
                                message.clone()
                            } else if let Some(message) = panic_payload.downcast_ref::<&str>() {
                                message.to_string()
                            } else {
                                "unknown panic".to_string()
                            };
                        tracing::error!(
                            component = "daemon",
                            phase = "checkpoint_ingress_worker",
                            reason = "worker_panic",
                            receipt_seq,
                            panic_msg = %panic_msg,
                            "checkpoint ingress worker panicked"
                        );
                        let _ = coordinator.complete_failed_checkpoint_admission(receipt_seq);
                        coordinator.request_shutdown();
                        break;
                    }
                }
            }

            let buffered_count = rx.len();
            if buffered_count > 0 {
                tracing::error!(
                    component = "daemon",
                    phase = "checkpoint_ingress_worker",
                    reason = "buffered_receipts_on_exit",
                    buffered_count,
                    "checkpoint ingress worker exited with accepted receipts buffered"
                );
            }
        });
        Ok(())
    }

    async fn prepare_checkpoint_admission(
        &self,
        accepted: AcceptedCheckpoint,
    ) -> Result<PreparedCheckpointAdmission, GitAiError> {
        let request: CheckpointRequest =
            serde_json::from_slice(&accepted.body).map_err(|error| {
                GitAiError::Generic(format!("invalid accepted checkpoint body: {error}"))
            })?;
        #[cfg(feature = "test-support")]
        if let Some(delay) =
            checkpoint_test_delay("GIT_AI_TEST_DELAY_CHECKPOINT_ADMISSION", &request.trace_id)
        {
            tokio::time::sleep(delay).await;
        }
        let trace_id = request.trace_id.clone();
        let file_count = request.files.len();
        let Some(repo_work_dir) = request.files.first().map(|file| file.repo_work_dir.clone())
        else {
            return Err(GitAiError::Generic(
                "accepted checkpoint contains no files".to_string(),
            ));
        };
        let family = self.backend.resolve_family(&repo_work_dir)?.0;
        crate::wltrace::wltrace("checkpoint.admission", Path::new(&family), || {
            format!(
                "receipt_seq={} repo_work_dir={}",
                accepted.receipt_seq,
                repo_work_dir.display()
            )
        });

        self.notify_checkpoint_stream(&request);
        self.wait_for_trace_ingest_seq(accepted.trace_ingest_target)
            .await;

        tracing::info!(
            component = "daemon",
            phase = "checkpoint_admission",
            receipt_seq = accepted.receipt_seq,
            %trace_id,
            %family,
            file_count,
            retained_bytes = accepted.reservation.body_bytes(),
            receipt_to_admission_ms =
                now_unix_nanos().saturating_sub(accepted.received_at_ns) / 1_000_000,
            "checkpoint prepared for family admission"
        );

        Ok(PreparedCheckpointAdmission {
            receipt_seq: accepted.receipt_seq,
            received_at_ns: accepted.received_at_ns,
            family,
            request,
            reservation: accepted.reservation,
        })
    }

    fn notify_checkpoint_stream(&self, request: &CheckpointRequest) {
        if let Some(worker) = &self.stream_worker
            && let Some(stream_source) = &request.stream_source
        {
            let tool = request
                .agent_id
                .as_ref()
                .map(|agent| agent.tool.clone())
                .unwrap_or_else(|| "unknown".to_string());
            worker.notify_checkpoint(
                stream_source.session_id.clone(),
                tool,
                request.trace_id.clone(),
                request.metadata.get("tool_use_id").cloned(),
                stream_source.path.clone(),
                request.files.first().map(|file| file.repo_work_dir.clone()),
                stream_source.external_session_id.clone(),
                stream_source.external_parent_session_id.clone(),
            );
        }
    }

    fn complete_checkpoint_admission(
        self: &Arc<Self>,
        prepared: PreparedCheckpointAdmission,
    ) -> Result<(), GitAiError> {
        let remaining = {
            let mut sequencers = self.family_sequencers_by_family.lock().map_err(|_| {
                GitAiError::Generic("family sequencer map lock poisoned".to_string())
            })?;
            let state = sequencers
                .entry(prepared.family.clone())
                .or_insert_with(|| FamilySequencerState {
                    next_ordinal: 1,
                    entries: BTreeMap::new(),
                });
            let order = FamilySequencerOrder {
                started_at_ns: prepared.received_at_ns,
                ordinal: state.next_ordinal,
            };
            state.next_ordinal = state.next_ordinal.saturating_add(1);
            state.entries.insert(
                order,
                FamilySequencerEntry::Checkpoint {
                    request: Box::new(prepared.request),
                    receipt_seq: prepared.receipt_seq,
                    reservation: prepared.reservation,
                },
            );
            self.unadmitted_checkpoints
                .fetch_sub(1, Ordering::AcqRel)
                .saturating_sub(1)
        };
        self.record_checkpoint_admission_processed(prepared.receipt_seq);
        if remaining == 0 {
            self.schedule_all_ready_family_drains();
        }
        Ok(())
    }

    fn complete_failed_checkpoint_admission(
        self: &Arc<Self>,
        receipt_seq: u64,
    ) -> Result<(), GitAiError> {
        let remaining = {
            let _sequencers = self.family_sequencers_by_family.lock().map_err(|_| {
                GitAiError::Generic("family sequencer map lock poisoned".to_string())
            })?;
            self.unadmitted_checkpoints
                .fetch_sub(1, Ordering::AcqRel)
                .saturating_sub(1)
        };
        self.record_checkpoint_admission_processed(receipt_seq);
        if remaining == 0 {
            self.schedule_all_ready_family_drains();
        }
        Ok(())
    }

    fn record_checkpoint_admission_processed(&self, receipt_seq: u64) {
        self.processed_checkpoint_receipt_seq
            .store(receipt_seq as usize, Ordering::Release);
        self.checkpoint_progress_notify.notify_waiters();
    }

    fn schedule_all_ready_family_drains(self: &Arc<Self>) {
        let coordinator = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = coordinator.drain_all_ready_family_sequencers().await {
                tracing::error!(
                    component = "daemon",
                    phase = "checkpoint_processing",
                    reason = "family_drain_failed",
                    %error,
                    "failed draining family sequencers after checkpoint admission"
                );
            }
        });
    }

    fn enqueue_trace_payload(&self, payload: Value) -> Result<(), GitAiError> {
        let tx =
            self.trace_ingest_tx.get().cloned().ok_or_else(|| {
                GitAiError::Generic("trace ingest worker not started".to_string())
            })?;
        let permit = match tx.try_reserve() {
            Ok(permit) => permit,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(())) => {
                tracing::error!(
                    component = "daemon",
                    phase = "enqueue_trace_payload",
                    reason = "ingest_worker_channel_closed",
                    "trace ingest queue send failed: worker may have crashed"
                );
                self.request_shutdown();
                return Err(GitAiError::Generic(
                    "trace ingest queue send failed: worker may have crashed".to_string(),
                ));
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(())) => {
                tracing::error!(
                    component = "daemon",
                    phase = "enqueue_trace_payload",
                    reason = "ingest_worker_queue_full",
                    "trace ingest queue is full"
                );
                self.request_shutdown();
                return Err(GitAiError::Generic(
                    "trace ingest queue is full; daemon shutting down".to_string(),
                ));
            }
        };
        self.record_trace_payload_enqueued(&payload)?;
        let mut payload = payload;
        if let Some(object) = payload.as_object_mut()
            && object.get(TRACE_INGEST_SEQ_FIELD).is_none()
        {
            object.insert(
                TRACE_INGEST_SEQ_FIELD.to_string(),
                json!(self.next_trace_ingest_seq()),
            );
        }
        // Relaxed: this counter tracks in-flight count for monitoring; no
        // ordering dependency with any other atomic.
        self.queued_trace_payloads.fetch_add(1, Ordering::Relaxed);
        permit.send(payload);
        Ok(())
    }

    async fn wait_for_trace_ingest_seq(&self, target: u64) {
        loop {
            // Enroll in the notification BEFORE checking the condition.
            // `Notify::notify_waiters` only wakes already-enrolled waiters, so
            // checking first would leave a window where the final progress
            // notification is lost and the waiter stalls until shutdown.
            let progress = self.trace_ingest_progress_notify.notified();
            tokio::pin!(progress);
            progress.as_mut().enable();
            let processed = self.processed_trace_ingest_seq.load(Ordering::Acquire) as u64;
            if processed >= target {
                return;
            }
            tokio::select! {
                _ = &mut progress => {}
                _ = self.wait_for_shutdown() => return,
            }
        }
    }

    /// Waits until all trace payloads enqueued up to now have been processed
    /// by the ingest worker, and any identified trace root that may mutate refs
    /// has closed. This is a causal drain fence: it guarantees that trace2 data
    /// already visible to the daemon for prior mutating git operations has
    /// reached the family sequencer before returning.
    ///
    /// Accepted sockets with no complete trace2 root are not causal evidence for
    /// any repository family. They are tracked for connection cleanup, but must
    /// not globally block checkpoint/sync control requests.
    ///
    /// Used by checkpoint entry to ensure ordering: a checkpoint must not be
    /// processed until all causally-prior git operations have been ingested
    /// through their root `atexit`/connection-close boundary.
    async fn wait_for_trace_ingest_processed_through(&self) {
        loop {
            // Read the current high-water mark. Any payload enqueued before this
            // point has a seq <= this value. We need to wait until the ingest
            // worker has processed through at least this seq.
            let target = self.next_trace_ingest_seq.load(Ordering::Acquire) as u64;
            self.wait_for_trace_ingest_seq(target).await;

            if !self.has_open_trace_roots_that_may_mutate_refs() {
                return;
            }

            let progress = self.trace_ingest_progress_notify.notified();
            if !self.has_open_trace_roots_that_may_mutate_refs() {
                return;
            }
            tokio::select! {
                _ = progress => {}
                _ = self.wait_for_shutdown() => return,
            }
        }
    }

    /// As [`Self::wait_for_trace_ingest_processed_through`], but scoped to one
    /// family: open mutating roots already attributed to a different family do
    /// not hold this fence, so a long-running git command in one repository no
    /// longer delays `sync.family` for every other repository. Unattributed
    /// roots still fail closed and block until their `def_repo` arrives.
    async fn wait_for_trace_ingest_processed_through_family(&self, family: &str) {
        loop {
            let target = self.next_trace_ingest_seq.load(Ordering::Acquire) as u64;
            self.wait_for_trace_ingest_seq(target).await;

            // Enroll before checking (see wait_for_trace_ingest_seq): the
            // notify_waiters fired by the root's close/def_repo must not race
            // the condition load.
            let progress = self.trace_ingest_progress_notify.notified();
            tokio::pin!(progress);
            progress.as_mut().enable();
            if !self.has_open_trace_roots_that_may_mutate_family(family) {
                return;
            }
            tokio::select! {
                _ = &mut progress => {}
                _ = self.wait_for_shutdown() => return,
            }
        }
    }

    /// Prepares `payload` for ingestion and returns whether it should be
    /// enqueued.
    ///
    /// - `true`  — payload is for a mutating command; the caller MUST call
    ///   `enqueue_trace_payload`.
    /// - `false` — payload is for a definitely-read-only invocation; it was
    ///   handled inline and the caller MUST NOT enqueue it.
    ///
    /// Sequence numbers are allocated only after `enqueue_trace_payload` has
    /// reserved queue capacity, so the `processed_trace_ingest_seq` watermark
    /// used by checkpoint drain waits advances without unqueued gaps.
    pub(crate) fn prepare_trace_payload_for_ingest(&self, payload: &mut Value) -> bool {
        // Check read-only status BEFORE allocating a sequence number so that
        // read-only invocations never perturb the ingest sequence counter.
        let is_read_only = self.track_trace_payload_for_ingest(payload);
        if is_read_only {
            return false;
        }
        true
    }

    /// Tracks trace2 root metadata needed for ordering and read-only fast paths.
    /// This deliberately does not read mutable repository state or inject
    /// daemon-derived repository/ref snapshots into the trace payload.
    fn track_trace_payload_for_ingest(&self, payload: &mut Value) -> bool {
        let event = payload
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let sid = payload
            .get("sid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if sid.is_empty() {
            return false;
        }

        let root = trace_root_sid(&sid).to_string();
        let argv = trace_payload_argv(payload);
        let worktree_hint = trace_payload_worktree_hint(payload);
        let started_at_ns = trace_payload_time_ns(payload);
        let early_primary =
            trace_payload_primary_command(payload).or_else(|| trace_argv_primary_command(&argv));
        let event_is_read_only =
            trace_invocation_is_definitely_read_only(early_primary.as_deref(), &argv);

        let mut ingress = match self.trace_ingress_state.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        ingress
            .root_last_activity_ns
            .insert(root.clone(), now_unix_nanos() as u64);

        if event == "start" && sid == root {
            let started_at_ns = started_at_ns.unwrap_or_else(now_unix_nanos);
            ingress
                .root_started_at_ns
                .entry(root.clone())
                .or_insert(started_at_ns);
        }

        if let Some(worktree) = worktree_hint.clone() {
            if let Some(common_dir) = common_dir_for_worktree(&worktree) {
                let family = common_dir.canonicalize().unwrap_or(common_dir);
                ingress
                    .root_families
                    .insert(root.clone(), family.to_string_lossy().to_string());
            }
            ingress.root_worktrees.insert(root.clone(), worktree);
        }

        if event == "start" && sid == root && !argv.is_empty() {
            ingress.root_argv.insert(root.clone(), argv.clone());
            if event_is_read_only {
                ingress.root_definitely_read_only.insert(root.clone());
            }
        }

        let effective_argv = if argv.is_empty() {
            ingress.root_argv.get(&root).cloned().unwrap_or_default()
        } else {
            argv
        };
        let effective_primary =
            early_primary.or_else(|| trace_argv_primary_command(&effective_argv));
        let command_mutates_refs =
            trace_invocation_may_mutate_refs(effective_primary.as_deref(), &effective_argv);
        if let Some(primary) = effective_primary.as_deref() {
            ingress
                .root_mutating
                .entry(root.clone())
                .or_insert(command_mutates_refs);
            let target_repo_only = trace_command_uses_target_repo_context_only(Some(primary));
            ingress
                .root_target_repo_only
                .entry(root.clone())
                .or_insert(target_repo_only);
        }

        let terminal = is_terminal_root_trace_event(&event, &sid, &root);
        if command_mutates_refs
            && !terminal
            && !ingress.root_reflog_start_offsets.contains_key(&root)
            && let Some(worktree) = worktree_hint
                .clone()
                .or_else(|| ingress.root_worktrees.get(&root).cloned())
        {
            let offsets =
                crate::daemon::ref_cursor::capture_reflog_start_offsets_for_worktree(&worktree);
            ingress
                .root_reflog_start_offsets
                .insert(root.clone(), offsets);
        }

        let read_only_root =
            event_is_read_only || ingress.root_definitely_read_only.contains(&root);
        let inherited = (
            ingress.root_argv.get(&root).cloned(),
            ingress.root_started_at_ns.get(&root).copied(),
            ingress.root_reflog_start_offsets.get(&root).cloned(),
            ingress.root_worktrees.get(&root).cloned(),
        );
        if terminal {
            ingress.root_worktrees.remove(&root);
            ingress.root_families.remove(&root);
            ingress.root_argv.remove(&root);
            ingress.root_started_at_ns.remove(&root);
            ingress.root_reflog_start_offsets.remove(&root);
            ingress.root_mutating.remove(&root);
            ingress.root_target_repo_only.remove(&root);
            ingress.root_last_activity_ns.remove(&root);
            ingress.root_definitely_read_only.remove(&root);
        }

        drop(ingress);

        if let Some(object) = payload.as_object_mut() {
            if object.get("argv").is_none()
                && let Some(root_argv) = inherited.0
            {
                object.insert(TRACE_ROOT_ARGV_FIELD.to_string(), json!(root_argv));
            }
            if object.get(TRACE_ROOT_STARTED_AT_NS_FIELD).is_none()
                && let Some(started_at_ns) = inherited.1
            {
                let started_at_ns = u64::try_from(started_at_ns).unwrap_or(u64::MAX);
                object.insert(
                    TRACE_ROOT_STARTED_AT_NS_FIELD.to_string(),
                    json!(started_at_ns),
                );
            }
            if object.get(TRACE_ROOT_REFLOG_START_OFFSETS_FIELD).is_none()
                && let Some(offsets) = inherited.2
            {
                object.insert(
                    TRACE_ROOT_REFLOG_START_OFFSETS_FIELD.to_string(),
                    json!(offsets),
                );
            }
            if object.get(TRACE_ROOT_WORKTREE_FIELD).is_none()
                && object.get("worktree").is_none()
                && object.get("repo_working_dir").is_none()
                && let Some(worktree) = inherited.3
            {
                object.insert(
                    TRACE_ROOT_WORKTREE_FIELD.to_string(),
                    json!(worktree.to_string_lossy().to_string()),
                );
            }
        }

        read_only_root
    }

    fn side_effect_exec_lock(&self, family: &str) -> Result<Arc<AsyncMutex<()>>, GitAiError> {
        let mut map = self
            .side_effect_exec_locks
            .lock()
            .map_err(|_| GitAiError::Generic("side effect lock map lock poisoned".to_string()))?;
        Ok(map
            .entry(family.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone())
    }

    async fn drain_ready_family_sequencer_entries_locked(
        &self,
        family: &str,
    ) -> Result<(), GitAiError> {
        let mut ready: Vec<(u64, FamilySequencerEntry)> = Vec::new();
        let mut progressed = false;
        {
            let mut map = self.family_sequencers_by_family.lock().map_err(|_| {
                GitAiError::Generic("family sequencer map lock poisoned".to_string())
            })?;
            if self.unadmitted_checkpoints.load(Ordering::Acquire) > 0 {
                return Ok(());
            }
            let Some(state) = map.get_mut(family) else {
                return Ok(());
            };
            while let Some(first_entry) = state.entries.first_entry() {
                if matches!(first_entry.get(), FamilySequencerEntry::PendingRoot) {
                    break;
                }
                let entry_root_sid = match first_entry.get() {
                    FamilySequencerEntry::ReadyCommand(command) => Some(command.root_sid.as_str()),
                    _ => None,
                };
                if self.family_entry_blocked_by_prior_open_trace_root(
                    family,
                    first_entry.key().started_at_ns,
                    entry_root_sid,
                )? {
                    break;
                }
                let (order, entry) = first_entry.remove_entry();
                match entry {
                    FamilySequencerEntry::PendingRoot => {
                        unreachable!("pending root should not be removed from sequencer front");
                    }
                    other => {
                        ready.push((order.ordinal, other));
                        progressed = true;
                    }
                }
            }
        }

        if ready.is_empty() {
            return Ok(());
        }

        let _ = self.begin_family_effect(family);
        for (order, ready_entry) in ready {
            // Per-family drains must be strictly serialized; overlapping or
            // order-regressing exec windows in a wltrace capture indicate a
            // broken exec-lock (see the GC held-lock eviction regression).
            crate::wltrace::wltrace("drain.exec", Path::new(family), || {
                let entry = match &ready_entry {
                    FamilySequencerEntry::ReadyCommand(command) => format!(
                        "command:{}",
                        command.primary_command.as_deref().unwrap_or("unknown")
                    ),
                    FamilySequencerEntry::Checkpoint { receipt_seq, .. } => {
                        format!("checkpoint:seq={receipt_seq}")
                    }
                    _ => "other".to_string(),
                };
                format!("order={order} entry={entry}")
            });
            match ready_entry {
                FamilySequencerEntry::ReadyCommand(command) => {
                    // Wrap the entire command + side-effect pipeline in catch_unwind
                    // so that a panic (e.g. from UTF-8 boundary issues in diff parsing)
                    // does not kill the daemon process.
                    let side_effect_result = {
                        let future = async {
                            let root_sid = command.root_sid.clone();
                            let mut commit_file_timestamp_snapshots =
                                self.take_cached_commit_file_timestamp_snapshots(&root_sid)?;
                            let applied = self.coordinator.route_command(*command).await?;
                            let side_effect = self
                                .maybe_apply_side_effects_for_applied_command(
                                    Some(family),
                                    &applied,
                                    &mut commit_file_timestamp_snapshots,
                                )
                                .await;
                            Ok::<_, GitAiError>((applied, side_effect))
                        };
                        let caught = std::panic::AssertUnwindSafe(future);
                        futures::FutureExt::catch_unwind(caught).await
                    };
                    match side_effect_result {
                        Ok(Ok((applied, side_effect_result))) => {
                            if let Err(error) = &side_effect_result {
                                let _ = self.record_side_effect_error(family, order, error);
                                tracing::error!(
                                    %error,
                                    %family,
                                    seq = applied.seq,
                                    "command side effect failed"
                                );
                            }
                            if let Err(error) = self.append_command_completion_log(
                                family,
                                &applied,
                                &side_effect_result,
                                order,
                            ) {
                                let _ = self.record_side_effect_error(family, order, &error);
                                tracing::error!(
                                    %error,
                                    %family,
                                    order,
                                    "command completion log write failed"
                                );
                            }
                        }
                        Ok(Err(error)) => {
                            let _ = self.record_side_effect_error(family, order, &error);
                            tracing::error!(
                                %error,
                                %family,
                                order,
                                "command apply failed"
                            );
                        }
                        Err(panic_payload) => {
                            let panic_msg = if let Some(s) = panic_payload.downcast_ref::<String>()
                            {
                                s.clone()
                            } else if let Some(s) = panic_payload.downcast_ref::<&str>() {
                                s.to_string()
                            } else {
                                "unknown panic".to_string()
                            };
                            let error = GitAiError::Generic(format!(
                                "daemon command side effect panic: {}",
                                panic_msg
                            ));
                            let _ = self.record_side_effect_error(family, order, &error);
                            tracing::error!(
                                component = "daemon",
                                phase = "command_side_effect",
                                reason = "panic_in_side_effect",
                                panic_msg = %panic_msg,
                                %family,
                                order,
                                "command side effect panic"
                            );
                        }
                    }
                }
                FamilySequencerEntry::Checkpoint {
                    mut request,
                    receipt_seq,
                    reservation: _reservation,
                } => {
                    let repo_wd = request
                        .files
                        .first()
                        .map(|f| f.repo_work_dir.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let checkpoint_file_paths: Vec<String> = request
                        .files
                        .iter()
                        .map(|f| f.path.to_string_lossy().to_string())
                        .collect();
                    let checkpoint_kind = request.checkpoint_kind;
                    let checkpoint_trace_id = request.trace_id.clone();
                    let checkpoint_path_role = request.path_role;
                    let checkpoint_has_agent = request.agent_id.is_some();
                    let checkpoint_kind_str = format!("{:?}", checkpoint_kind);
                    let is_human_checkpoint = checkpoint_kind == CheckpointKind::Human;

                    // Register pending AI edit state when an AI agent fires its
                    // pre-edit snapshot. This signals that an AI edit is in-flight.
                    // Identified by: WillEdit path_role + agent_id present (only AI
                    // agent presets have an agent_id on their pre-edit checkpoints).
                    if checkpoint_path_role == PreparedPathRole::WillEdit && checkpoint_has_agent {
                        self.register_pending_ai_edits(family, &checkpoint_file_paths);
                    }

                    // Filter out files with pending AI edits from KnownHuman checkpoints.
                    // These are spurious IDE save events that fire between pre/post-edit.
                    if checkpoint_kind == CheckpointKind::KnownHuman {
                        let pending_files: Vec<String> = checkpoint_file_paths
                            .iter()
                            .filter(|f| self.file_has_pending_ai_edit(family, f))
                            .cloned()
                            .collect();
                        if !pending_files.is_empty() {
                            request.files.retain(|f| {
                                let path_str = f.path.to_string_lossy().to_string();
                                !pending_files.contains(&path_str)
                            });
                            tracing::debug!(
                                "[KnownHuman] Filtered {} file(s) with pending AI edits",
                                pending_files.len()
                            );
                            if request.files.is_empty() {
                                let log_entry = TestCompletionLogEntry {
                                    seq: 0,
                                    family_key: family.to_string(),
                                    kind: "checkpoint".to_string(),
                                    primary_command: Some("checkpoint".to_string()),
                                    test_sync_session: None,
                                    exit_code: None,
                                    sync_tracked: true,
                                    status: "suppressed".to_string(),
                                    error: None,
                                };
                                let _ = self.maybe_append_test_completion_log(family, &log_entry);
                                tracing::info!(
                                    component = "daemon",
                                    phase = "checkpoint_processing",
                                    receipt_seq,
                                    trace_id = %checkpoint_trace_id,
                                    %family,
                                    status = "suppressed",
                                    "checkpoint processing completed"
                                );
                                continue;
                            }
                        }
                    }

                    // Recompute file paths after potential KnownHuman filtering so
                    // watermark computation and clear_pending_ai_edits use the actual
                    // files that will be checkpointed.
                    let checkpoint_file_paths: Vec<String> = request
                        .files
                        .iter()
                        .map(|f| f.path.to_string_lossy().to_string())
                        .collect();

                    let should_log_completion = true; // Always log for test sync
                    tracing::info!(kind = %checkpoint_kind_str, repo = %repo_wd, "checkpoint start");
                    let checkpoint_side_effect_permit = self
                        .checkpoint_side_effect_semaphore
                        .acquire()
                        .await
                        .map_err(|_| {
                            GitAiError::Generic(
                                "checkpoint side-effect semaphore closed".to_string(),
                            )
                        })?;
                    let checkpoint_start = std::time::Instant::now();
                    let checkpoint_request = {
                        let future = async {
                            if !repo_wd.is_empty() {
                                let ack =
                                    self.coordinator.apply_checkpoint(Path::new(&repo_wd)).await;
                                match ack {
                                    Ok(ack) => {
                                        crate::tokio_runtime::spawn_blocking_result(move || {
                                            apply_checkpoint_side_effect(*request)
                                        })
                                        .await
                                        .map(|_| ack.seq)
                                    }
                                    Err(error) => Err(error),
                                }
                            } else {
                                crate::tokio_runtime::spawn_blocking_result(move || {
                                    apply_checkpoint_side_effect(*request)
                                })
                                .await
                                .map(|_| 0)
                            }
                        };
                        let caught = std::panic::AssertUnwindSafe(future);
                        futures::FutureExt::catch_unwind(caught).await
                    };
                    let result = match checkpoint_request {
                        Ok(inner) => inner,
                        Err(panic_payload) => {
                            let panic_msg = if let Some(s) = panic_payload.downcast_ref::<String>()
                            {
                                s.clone()
                            } else if let Some(s) = panic_payload.downcast_ref::<&str>() {
                                s.to_string()
                            } else {
                                "unknown panic".to_string()
                            };
                            tracing::error!(
                                component = "daemon",
                                phase = "checkpoint_side_effect",
                                reason = "panic_in_side_effect",
                                panic_msg = %panic_msg,
                                %family,
                                order,
                                "checkpoint side effect panic"
                            );
                            Err(GitAiError::Generic(format!(
                                "daemon checkpoint panic: {}",
                                panic_msg
                            )))
                        }
                    };
                    drop(checkpoint_side_effect_permit);
                    let checkpoint_duration_ms = checkpoint_start.elapsed().as_millis();
                    if result.is_ok() {
                        tracing::info!(
                            kind = %checkpoint_kind_str,
                            repo = %repo_wd,
                            duration_ms = checkpoint_duration_ms as u64,
                            "checkpoint done"
                        );
                    } else {
                        tracing::warn!(
                            kind = %checkpoint_kind_str,
                            repo = %repo_wd,
                            duration_ms = checkpoint_duration_ms as u64,
                            "checkpoint failed"
                        );
                    }
                    if result.is_ok() {
                        // Clear pending AI edit state once the PostFileEdit completes.
                        if checkpoint_kind.is_ai()
                            && checkpoint_path_role == PreparedPathRole::Edited
                        {
                            self.clear_pending_ai_edits(family, &checkpoint_file_paths);
                        }
                        let per_file = if !checkpoint_file_paths.is_empty() {
                            compute_watermarks_from_stat(&repo_wd, &checkpoint_file_paths)
                        } else {
                            std::collections::HashMap::new()
                        };
                        let per_worktree = if is_human_checkpoint {
                            let now_ns = std::time::SystemTime::now()
                                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos();
                            std::collections::HashMap::from([(
                                Self::worktree_state_key(Path::new(&repo_wd)),
                                now_ns,
                            )])
                        } else {
                            std::collections::HashMap::new()
                        };
                        if (!per_file.is_empty() || !per_worktree.is_empty())
                            && let Err(error) = self
                                .coordinator
                                .update_watermarks_family(
                                    Path::new(&repo_wd),
                                    crate::daemon::domain::WatermarkState {
                                        per_file,
                                        per_worktree,
                                    },
                                )
                                .await
                        {
                            let _ = self.record_side_effect_error(family, order, &error);
                            tracing::error!(
                                component = "daemon",
                                phase = "checkpoint_processing",
                                reason = "watermark_update_failed",
                                receipt_seq,
                                %family,
                                order,
                                %error,
                                "checkpoint watermark update failed"
                            );
                        }
                    }
                    // Removed captured_checkpoint_id cleanup - no more captured checkpoints
                    if let Err(error) = &result {
                        let _ = self.record_side_effect_error(family, order, error);
                        tracing::error!(
                            component = "daemon",
                            phase = "checkpoint_processing",
                            reason = "side_effect_failed",
                            receipt_seq,
                            trace_id = %checkpoint_trace_id,
                            %error,
                            %family,
                            order,
                            "checkpoint side effect failed"
                        );
                    }
                    if should_log_completion {
                        let log_entry = TestCompletionLogEntry {
                            seq: result.as_ref().copied().unwrap_or(0),
                            family_key: family.to_string(),
                            kind: "checkpoint".to_string(),
                            primary_command: Some("checkpoint".to_string()),
                            test_sync_session: None,
                            exit_code: None,
                            sync_tracked: true,
                            status: if result.is_ok() {
                                "ok".to_string()
                            } else {
                                "error".to_string()
                            },
                            error: result.as_ref().err().map(|error| error.to_string()),
                        };
                        if let Err(error) =
                            self.maybe_append_test_completion_log(family, &log_entry)
                        {
                            let _ = self.record_side_effect_error(family, order, &error);
                            tracing::error!(
                                component = "daemon",
                                phase = "checkpoint_processing",
                                reason = "completion_log_failed",
                                receipt_seq,
                                trace_id = %checkpoint_trace_id,
                                %error,
                                %family,
                                order,
                                "checkpoint completion log write failed"
                            );
                        }
                    }
                    tracing::info!(
                        component = "daemon",
                        phase = "checkpoint_processing",
                        receipt_seq,
                        trace_id = %checkpoint_trace_id,
                        %family,
                        status = if result.is_ok() { "ok" } else { "error" },
                        duration_ms = checkpoint_duration_ms as u64,
                        "checkpoint processing completed"
                    );
                }
                FamilySequencerEntry::Canceled => {}
                FamilySequencerEntry::PendingRoot => {}
            }
        }
        let _ = self.end_family_effect(family);

        let _ = progressed;
        Ok(())
    }

    fn worktree_state_key(worktree: &Path) -> String {
        let normalized = worktree_root_for_path(worktree).unwrap_or_else(|| worktree.to_path_buf());
        normalized
            .canonicalize()
            .unwrap_or(normalized)
            .to_string_lossy()
            .to_string()
    }

    fn set_pending_rebase_original_head_for_worktree(
        &self,
        worktree: &Path,
        original_head: String,
        onto: Option<String>,
    ) -> Result<(), GitAiError> {
        let mut map = self
            .pending_rebase_original_head_by_worktree
            .lock()
            .map_err(|_| {
                GitAiError::Generic("pending rebase original-head map lock poisoned".to_string())
            })?;
        map.insert(
            Self::worktree_state_key(worktree),
            PendingRebase {
                original_head,
                onto,
            },
        );
        Ok(())
    }

    fn clear_pending_rebase_original_head_for_worktree(
        &self,
        worktree: &Path,
    ) -> Result<(), GitAiError> {
        let mut map = self
            .pending_rebase_original_head_by_worktree
            .lock()
            .map_err(|_| {
                GitAiError::Generic("pending rebase original-head map lock poisoned".to_string())
            })?;
        map.remove(&Self::worktree_state_key(worktree));
        Ok(())
    }

    fn take_pending_rebase_original_head_for_worktree(
        &self,
        worktree: &Path,
    ) -> Result<Option<PendingRebase>, GitAiError> {
        let mut map = self
            .pending_rebase_original_head_by_worktree
            .lock()
            .map_err(|_| {
                GitAiError::Generic("pending rebase original-head map lock poisoned".to_string())
            })?;
        Ok(map.remove(&Self::worktree_state_key(worktree)))
    }

    fn set_pending_cherry_pick_sources_for_worktree(
        &self,
        worktree: &Path,
        sources: Vec<String>,
    ) -> Result<(), GitAiError> {
        let mut map = self
            .pending_cherry_pick_sources_by_worktree
            .lock()
            .map_err(|_| {
                GitAiError::Generic("pending cherry-pick sources map lock poisoned".to_string())
            })?;
        let key = Self::worktree_state_key(worktree);
        if sources.is_empty() {
            map.remove(&key);
        } else {
            map.insert(key, sources);
        }
        Ok(())
    }

    fn clear_pending_cherry_pick_sources_for_worktree(
        &self,
        worktree: &Path,
    ) -> Result<(), GitAiError> {
        let mut map = self
            .pending_cherry_pick_sources_by_worktree
            .lock()
            .map_err(|_| {
                GitAiError::Generic("pending cherry-pick sources map lock poisoned".to_string())
            })?;
        map.remove(&Self::worktree_state_key(worktree));
        Ok(())
    }

    fn take_pending_cherry_pick_sources_for_worktree(
        &self,
        worktree: &Path,
    ) -> Result<Vec<String>, GitAiError> {
        let mut map = self
            .pending_cherry_pick_sources_by_worktree
            .lock()
            .map_err(|_| {
                GitAiError::Generic("pending cherry-pick sources map lock poisoned".to_string())
            })?;
        Ok(map
            .remove(&Self::worktree_state_key(worktree))
            .unwrap_or_default())
    }

    fn pending_cherry_pick_sources_for_worktree(
        &self,
        worktree: &Path,
    ) -> Result<Vec<String>, GitAiError> {
        let map = self
            .pending_cherry_pick_sources_by_worktree
            .lock()
            .map_err(|_| {
                GitAiError::Generic("pending cherry-pick sources map lock poisoned".to_string())
            })?;
        Ok(map
            .get(&Self::worktree_state_key(worktree))
            .cloned()
            .unwrap_or_default())
    }

    fn set_pending_cherry_pick_no_commit_for_worktree(
        &self,
        worktree: &Path,
        source_commits: Vec<String>,
        head: String,
    ) -> Result<(), GitAiError> {
        let mut map = self
            .pending_cherry_pick_no_commit_by_worktree
            .lock()
            .map_err(|_| {
                GitAiError::Generic("pending cherry-pick no-commit map lock poisoned".to_string())
            })?;
        let key = Self::worktree_state_key(worktree);
        if source_commits.is_empty() || head.is_empty() {
            map.remove(&key);
        } else {
            map.insert(
                key,
                PendingCherryPickNoCommit {
                    source_commits,
                    head,
                },
            );
        }
        Ok(())
    }

    fn clear_pending_cherry_pick_no_commit_for_worktree(
        &self,
        worktree: &Path,
    ) -> Result<(), GitAiError> {
        let mut map = self
            .pending_cherry_pick_no_commit_by_worktree
            .lock()
            .map_err(|_| {
                GitAiError::Generic("pending cherry-pick no-commit map lock poisoned".to_string())
            })?;
        map.remove(&Self::worktree_state_key(worktree));
        Ok(())
    }

    fn take_pending_cherry_pick_no_commit_for_worktree(
        &self,
        worktree: &Path,
    ) -> Result<Option<PendingCherryPickNoCommit>, GitAiError> {
        let mut map = self
            .pending_cherry_pick_no_commit_by_worktree
            .lock()
            .map_err(|_| {
                GitAiError::Generic("pending cherry-pick no-commit map lock poisoned".to_string())
            })?;
        Ok(map.remove(&Self::worktree_state_key(worktree)))
    }

    fn set_pending_squash_merge_for_worktree(
        &self,
        worktree: &Path,
        source_head: String,
        onto: String,
    ) -> Result<(), GitAiError> {
        let mut map = self.pending_squash_merge_by_worktree.lock().map_err(|_| {
            GitAiError::Generic("pending squash merge map lock poisoned".to_string())
        })?;
        map.insert(
            Self::worktree_state_key(worktree),
            PendingSquashMerge { source_head, onto },
        );
        Ok(())
    }

    fn take_pending_squash_merge_for_worktree(
        &self,
        worktree: &Path,
    ) -> Result<Option<PendingSquashMerge>, GitAiError> {
        let mut map = self.pending_squash_merge_by_worktree.lock().map_err(|_| {
            GitAiError::Generic("pending squash merge map lock poisoned".to_string())
        })?;
        Ok(map.remove(&Self::worktree_state_key(worktree)))
    }

    fn resolve_heads_for_command(
        cmd: &crate::daemon::domain::NormalizedCommand,
    ) -> (String, String) {
        let old = cmd
            .ref_changes
            .iter()
            .find(|change| change.reference == "HEAD")
            .map(|change| change.old.clone())
            .or_else(|| {
                cmd.ref_changes
                    .iter()
                    .find(|change| change.reference.starts_with("refs/heads/"))
                    .map(|change| change.old.clone())
            })
            .or_else(|| {
                cmd.ref_changes
                    .iter()
                    .find(|change| is_non_auxiliary_ref(&change.reference))
                    .map(|change| change.old.clone())
            })
            .unwrap_or_default();
        let new = cmd
            .ref_changes
            .iter()
            .rfind(|change| change.reference == "HEAD")
            .map(|change| change.new.clone())
            .or_else(|| {
                cmd.ref_changes
                    .iter()
                    .rfind(|change| change.reference.starts_with("refs/heads/"))
                    .map(|change| change.new.clone())
            })
            .or_else(|| {
                cmd.ref_changes
                    .iter()
                    .rfind(|change| is_non_auxiliary_ref(&change.reference))
                    .map(|change| change.new.clone())
            })
            .unwrap_or_default();
        (old, new)
    }

    fn stash_pathspecs_from_command(cmd: &crate::daemon::domain::NormalizedCommand) -> Vec<String> {
        let parsed = parsed_invocation_for_normalized_command(cmd);
        if parsed.command.as_deref() != Some("stash") {
            return Vec::new();
        }

        let mut pathspecs = Vec::new();
        let mut found_separator = false;
        let mut skip_next = false;

        for (i, arg) in parsed.command_args.iter().enumerate() {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg == "--" {
                found_separator = true;
                continue;
            }
            if found_separator {
                pathspecs.push(arg.clone());
                continue;
            }
            if arg.starts_with('-') {
                if matches!(
                    arg.as_str(),
                    "-m" | "--message" | "--pathspec-from-file" | "--pathspec-file-nul"
                ) {
                    skip_next = true;
                }
                continue;
            }
            if i == 0 && matches!(arg.as_str(), "push" | "save" | "pop" | "apply") {
                continue;
            }
            if i == 1 && arg.starts_with("stash@") {
                continue;
            }
            pathspecs.push(arg.clone());
        }

        tracing::debug!("Extracted stash pathspecs: {:?}", pathspecs);
        pathspecs
    }

    /// Detects non-fast-forward ref moves and fires handle_rewrite_event.
    fn detect_and_handle_non_ff_rewrites(
        &self,
        cmd: &crate::daemon::domain::NormalizedCommand,
    ) -> Result<(), GitAiError> {
        let worktree = match cmd.worktree.as_ref() {
            Some(w) => w,
            None => return Ok(()),
        };

        let repo = find_repository_in_path(&worktree.to_string_lossy())?;

        // For rebase --skip/--continue that completes successfully, the trace2 data only shows
        // HEAD moving from onto → new_tip (a fast-forward). The real old_tip (original branch tip
        // before rebase started) was stored when the initial rebase failed. Use it here.
        let is_rebase_cmd = cmd.primary_command.as_deref() == Some("rebase");
        let pending_original_head = if is_rebase_cmd {
            self.take_pending_rebase_original_head_for_worktree(worktree)?
        } else {
            None
        };

        // Collect branch ref changes (skip notes, tags, etc.)
        let mut branch_changes: Vec<_> = cmd
            .ref_changes
            .iter()
            .filter(|rc| rc.reference.starts_with("refs/heads/"))
            .filter(|rc| is_valid_oid(&rc.old) && !is_zero_oid(&rc.old))
            .filter(|rc| is_valid_oid(&rc.new) && !is_zero_oid(&rc.new))
            .cloned()
            .collect();

        // If no branch ref changes found, fall back to HEAD changes (common for reset)
        if branch_changes.is_empty() {
            let head_changes: Vec<_> = cmd
                .ref_changes
                .iter()
                .filter(|rc| rc.reference == "HEAD")
                .filter(|rc| is_valid_oid(&rc.old) && !is_zero_oid(&rc.old))
                .filter(|rc| is_valid_oid(&rc.new) && !is_zero_oid(&rc.new))
                .cloned()
                .collect();
            if !head_changes.is_empty() {
                branch_changes = head_changes;
            }
        }

        if branch_changes.is_empty() && pending_original_head.is_none() {
            return Ok(());
        }

        // Collapse multiple changes to same branch: use (first old, last new)
        let mut collapsed: std::collections::HashMap<&str, (&str, &str)> =
            std::collections::HashMap::new();
        for rc in &branch_changes {
            collapsed
                .entry(rc.reference.as_str())
                .and_modify(|(_old, new)| *new = &rc.new)
                .or_insert((&rc.old, &rc.new));
        }

        // Lite mode keeps working logs aligned with the final ref tips, but does not
        // inspect the commit graph or migrate authorship notes. Everything needed for
        // this bookkeeping comes from the already-normalized trace2/ref transition.
        if config::Config::get().get_feature_flags().lite_mode {
            if let Some(pending) = pending_original_head.as_ref()
                && let Some(new_tip) = rebase_new_tip_from_command(cmd, &pending.original_head)
            {
                if pending.original_head != new_tip {
                    repo.storage
                        .rename_working_log(&pending.original_head, &new_tip)?;
                }
                return Ok(());
            }
            if !matches!(
                cmd.primary_command.as_deref(),
                Some("rebase" | "pull" | "update-ref")
            ) {
                return Ok(());
            }
            let command_moves_checked_out_history =
                matches!(cmd.primary_command.as_deref(), Some("rebase" | "pull"));
            let mut collapsed_head: Option<(&str, &str)> = None;
            for change in cmd.ref_changes.iter().filter(|change| {
                change.reference == "HEAD"
                    && is_valid_oid(&change.old)
                    && !is_zero_oid(&change.old)
                    && is_valid_oid(&change.new)
                    && !is_zero_oid(&change.new)
            }) {
                if let Some((_old, new)) = &mut collapsed_head {
                    *new = &change.new;
                } else {
                    collapsed_head = Some((&change.old, &change.new));
                }
            }
            for (old_tip, new_tip) in collapsed.values() {
                let moves_head = command_moves_checked_out_history
                    || collapsed_head.is_some_and(|(head_old, head_new)| {
                        head_old == *old_tip && head_new == *new_tip
                    });
                if old_tip != new_tip && moves_head {
                    repo.storage.rename_working_log(old_tip, new_tip)?;
                }
            }
            return Ok(());
        }

        // Extract "onto" hint from HEAD ref changes for rebases.
        // During a rebase, the first HEAD change target is the onto commit.
        let onto_hint: Option<String> = cmd
            .ref_changes
            .iter()
            .filter(|rc| rc.reference == "HEAD")
            .filter(|rc| is_valid_oid(&rc.new) && !is_zero_oid(&rc.new))
            .map(|rc| rc.new.clone())
            .next();

        // If we have a pending original head from a failed rebase, use it as old_tip
        // with the branch ref update as new_tip. This handles rebase --skip/--continue
        // where HEAD can contain extra checkout/detach movement that is not the
        // rebased branch tip.
        crate::wltrace::wltrace(
            "rewrite.branch_transition",
            Path::new(cmd.worktree.as_deref().unwrap_or(Path::new(""))),
            || {
                format!(
                    "cmd={} branch_changes={} pending_original_head={} ref_changes={}",
                    cmd.primary_command.as_deref().unwrap_or("unknown"),
                    branch_changes.len(),
                    pending_original_head
                        .as_ref()
                        .map(|pending| pending.original_head.as_str())
                        .unwrap_or("NONE"),
                    cmd.ref_changes.len(),
                )
            },
        );

        if let Some(pending) = pending_original_head
            && let Some(new_tip) = rebase_new_tip_from_command(cmd, &pending.original_head)
        {
            if pending.original_head != new_tip
                && !is_ancestor_commit(&repo, &pending.original_head, &new_tip)
            {
                let command_rebase_onto =
                    rebase_onto_from_command(cmd, &repo, &pending.original_head, &new_tip);
                let rebase_onto = pending
                    .onto
                    .filter(|onto| {
                        onto != &pending.original_head
                            && onto != &new_tip
                            && is_ancestor_commit(&repo, onto, &new_tip)
                    })
                    .or(command_rebase_onto);
                let outcome =
                    crate::authorship::rewrite::handle_non_fast_forward_rewrite_with_operation(
                        &repo,
                        &pending.original_head,
                        &new_tip,
                        rebase_onto.as_deref(),
                        crate::authorship::rewrite::RewriteMetricOperation::Rebase,
                    )?;
                repo.storage
                    .rename_working_log(&pending.original_head, &new_tip)?;
                let conflict_base = rebase_onto.clone();
                let metric_context = process_conflict_resolution_working_logs(
                    &repo,
                    &new_tip,
                    conflict_base.as_deref(),
                )?;
                let metric_commits =
                    rewrite_metric_commits_with_context(outcome.metric_commits, metric_context);
                if !metric_commits.is_empty() {
                    let branch = rewrite_metric_branch_for_transition(
                        cmd,
                        &pending.original_head,
                        &new_tip,
                        None,
                    );
                    crate::daemon::rewrite_metrics::spawn_rewrite_commit_metrics(
                        &repo,
                        rewrite_metric_commits_with_branch(metric_commits, branch),
                    );
                }
            }
            return Ok(());
        }

        for (reference, (old_tip, new_tip)) in &collapsed {
            if *old_tip == *new_tip {
                continue;
            }

            // Fast-forward — not a rewrite
            if is_ancestor_commit(&repo, old_tip, new_tip) {
                continue;
            }

            let rewrite_onto = if is_rebase_cmd {
                rebase_onto_from_command(cmd, &repo, old_tip, new_tip).or_else(|| onto_hint.clone())
            } else {
                onto_hint.clone()
            };
            let outcome = if is_rebase_cmd {
                crate::authorship::rewrite::handle_non_fast_forward_rewrite_with_operation(
                    &repo,
                    old_tip,
                    new_tip,
                    rewrite_onto.as_deref(),
                    crate::authorship::rewrite::RewriteMetricOperation::Rebase,
                )?
            } else if cmd.primary_command.as_deref() == Some("update-ref") {
                crate::authorship::rewrite::handle_non_fast_forward_rewrite_with_operation(
                    &repo,
                    old_tip,
                    new_tip,
                    rewrite_onto.as_deref(),
                    crate::authorship::rewrite::RewriteMetricOperation::UpdateRef,
                )?
            } else {
                crate::authorship::rewrite::handle_non_fast_forward_rewrite_with_operation(
                    &repo,
                    old_tip,
                    new_tip,
                    rewrite_onto.as_deref(),
                    crate::authorship::rewrite::RewriteMetricOperation::NonFastForward,
                )?
            };
            repo.storage.rename_working_log(old_tip, new_tip)?;
            let metric_context = if is_rebase_cmd {
                let conflict_base = rewrite_onto.clone().or_else(|| onto_hint.clone());
                process_conflict_resolution_working_logs(&repo, new_tip, conflict_base.as_deref())?
            } else {
                RewriteMetricContext::default()
            };
            let metric_commits =
                rewrite_metric_commits_with_context(outcome.metric_commits, metric_context);
            if !metric_commits.is_empty() {
                let branch =
                    rewrite_metric_branch_for_transition(cmd, old_tip, new_tip, Some(reference));
                crate::daemon::rewrite_metrics::spawn_rewrite_commit_metrics(
                    &repo,
                    rewrite_metric_commits_with_branch(metric_commits, branch),
                );
            }
        }

        Ok(())
    }

    fn start_commit_file_timestamp_snapshots_for_command(
        command: &crate::daemon::domain::NormalizedCommand,
    ) -> CommitFileTimestampSnapshotHandles {
        if config::Config::get().get_feature_flags().lite_mode
            && command.primary_command.as_deref() == Some("commit")
            && command.invoked_args.iter().any(|arg| arg == "--amend")
        {
            return HashMap::new();
        }
        let Some(worktree) = command.worktree.clone() else {
            return HashMap::new();
        };
        if command.exit_code != 0 || command.primary_command.as_deref() != Some("commit") {
            return HashMap::new();
        }

        let (_, new_head) = Self::resolve_heads_for_command(command);
        if new_head.is_empty() || !is_valid_oid(&new_head) || is_zero_oid(&new_head) {
            return HashMap::new();
        }

        let mut handles = HashMap::new();
        let task_commit_sha = new_head.clone();
        let handle = tokio::task::spawn_blocking(move || {
            match capture_commit_file_timestamps(&worktree, &task_commit_sha) {
                Ok(timestamps) => Some(timestamps),
                Err(error) => {
                    tracing::debug!(
                        %error,
                        commit_sha = %task_commit_sha,
                        "failed to capture commit-time file timestamps"
                    );
                    None
                }
            }
        });
        handles.insert(new_head, handle);

        handles
    }

    fn cache_commit_file_timestamp_snapshots_for_command(
        &self,
        command: &crate::daemon::domain::NormalizedCommand,
    ) -> Result<(), GitAiError> {
        let handles = Self::start_commit_file_timestamp_snapshots_for_command(command);
        if handles.is_empty() {
            return Ok(());
        }
        let mut cache = self
            .commit_file_timestamp_snapshots_by_root
            .lock()
            .map_err(|_| {
                GitAiError::Generic(
                    "commit file timestamp snapshot cache lock poisoned".to_string(),
                )
            })?;
        cache.insert(command.root_sid.clone(), handles);
        Ok(())
    }

    fn take_cached_commit_file_timestamp_snapshots(
        &self,
        root_sid: &str,
    ) -> Result<CommitFileTimestampSnapshotHandles, GitAiError> {
        let mut cache = self
            .commit_file_timestamp_snapshots_by_root
            .lock()
            .map_err(|_| {
                GitAiError::Generic(
                    "commit file timestamp snapshot cache lock poisoned".to_string(),
                )
            })?;
        Ok(cache.remove(root_sid).unwrap_or_default())
    }

    async fn take_commit_file_timestamps(
        handles: &mut CommitFileTimestampSnapshotHandles,
        commit_sha: &str,
    ) -> Option<crate::authorship::attribution_recovery::FileTimestampsByPath> {
        let handle = handles.remove(commit_sha)?;
        match tokio::time::timeout(COMMIT_FILE_TIMESTAMP_SNAPSHOT_WAIT, handle).await {
            Ok(Ok(Some(timestamps))) if !timestamps.is_empty() => Some(timestamps),
            Ok(Ok(_)) => None,
            Ok(Err(error)) => {
                tracing::debug!(
                    %error,
                    %commit_sha,
                    "commit-time file timestamp task failed"
                );
                None
            }
            Err(_) => {
                tracing::debug!(
                    %commit_sha,
                    "commit-time file timestamp task timed out"
                );
                None
            }
        }
    }

    async fn maybe_apply_side_effects_for_applied_command(
        &self,
        family: Option<&str>,
        applied: &crate::daemon::domain::AppliedCommand,
        commit_file_timestamp_snapshots: &mut CommitFileTimestampSnapshotHandles,
    ) -> Result<(), GitAiError> {
        // Test-only: allow inducing a panic in the side-effect pipeline to verify
        // that the daemon's catch_unwind recovery keeps the process alive.
        // Uses a file-based flag so the test can remove the file between commands.
        #[cfg(feature = "test-support")]
        if let Ok(path) = std::env::var("GIT_AI_TEST_PANIC_IN_SIDE_EFFECT_FLAG")
            && std::path::Path::new(&path).exists()
        {
            panic!("test-induced panic in side-effect pipeline");
        }

        let cmd = &applied.command;
        let events = &applied.analysis.events;

        let primary = cmd.primary_command.as_deref().unwrap_or("unknown");
        let lite_mode = config::Config::get().get_feature_flags().lite_mode;

        #[cfg(feature = "test-support")]
        if let Ok(spec) = std::env::var("GIT_AI_TEST_DELAY_SIDE_EFFECT_MS_FOR_COMMAND") {
            for entry in spec.split(',') {
                let Some((command, delay_ms)) = entry.split_once('=') else {
                    continue;
                };
                if command == primary
                    && let Ok(delay_ms) = delay_ms.parse::<u64>()
                    && delay_ms > 0
                {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    break;
                }
            }
        }

        let is_write_op = matches!(
            primary,
            "commit"
                | "rebase"
                | "merge"
                | "cherry-pick"
                | "am"
                | "stash"
                | "reset"
                | "push"
                | "update-ref"
        );
        if is_write_op && cmd.exit_code == 0 {
            let repo_path = cmd
                .worktree
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let post_head = cmd
                .ref_changes
                .iter()
                .rev()
                .find(|change| change.reference == "HEAD")
                .map(|change| change.new.clone())
                .unwrap_or_default();
            tracing::info!(
                op = primary,
                repo = %repo_path,
                new_head = %post_head,
                "git write op completed"
            );
        }

        let saw_pull_event = events.iter().any(|event| {
            matches!(
                event,
                crate::daemon::domain::SemanticEvent::PullCompleted { .. }
            )
        });
        let pull_uses_rebase = events.iter().any(|event| {
            matches!(
                event,
                crate::daemon::domain::SemanticEvent::PullCompleted {
                    strategy: crate::daemon::domain::PullStrategy::Rebase
                        | crate::daemon::domain::PullStrategy::RebaseMerges,
                    ..
                }
            )
        });
        if std::env::var("GIT_AI_DEBUG_DAEMON_TRACE")
            .ok()
            .as_deref()
            .is_some_and(|v| v == "1")
        {
            tracing::debug!(
                command = cmd.invoked_command.clone().unwrap_or_default(),
                primary = cmd.primary_command.clone().unwrap_or_default(),
                seq = applied.seq,
                argv = ?cmd.raw_argv,
                invoked_args = ?cmd.invoked_args,
                ref_changes_len = cmd.ref_changes.len(),
                ref_changes = ?cmd.ref_changes,
                events = ?events,
                exit_code = cmd.exit_code,
                "side-effect trace"
            );
        }
        // Non-FF rewrite detection: fires for commands that rewrite history via ref moves.
        // Skip for: checkout/switch/branch (no rewriting), cherry-pick (handled separately),
        // and plain commit/amend (CommitCreated/CommitAmended events handle those).
        // Do NOT skip for rebase — the CommitCreated events during rebase are intermediate
        // replayed commits; note transfer happens via non-FF detection on the final ref move.
        // But DO skip for rebase --abort, which restores state instead of finishing a rewrite.
        let is_rebase = cmd.primary_command.as_deref() == Some("rebase");
        let is_rebase_abort = is_rebase && cmd.invoked_args.iter().any(|a| a == "--abort");
        let is_completing_rebase = is_rebase && !is_rebase_abort;
        let is_pull_rebase = pull_uses_rebase && cmd.primary_command.as_deref() == Some("pull");
        let skip_non_ff = if is_completing_rebase || is_pull_rebase {
            false
        } else if is_rebase_abort {
            if let Some(worktree) = cmd.worktree.as_ref() {
                self.clear_pending_rebase_original_head_for_worktree(worktree)?;
            }
            true
        } else {
            events.iter().any(|event| {
                matches!(
                    event,
                    crate::daemon::domain::SemanticEvent::CommitAmended { .. }
                        | crate::daemon::domain::SemanticEvent::CommitCreated { .. }
                        | crate::daemon::domain::SemanticEvent::CherryPickComplete { .. }
                        | crate::daemon::domain::SemanticEvent::Reset { .. }
                )
            }) || matches!(
                cmd.primary_command.as_deref(),
                Some("checkout" | "switch" | "branch" | "stash")
            )
        };
        if !skip_non_ff && cmd.exit_code == 0 {
            self.detect_and_handle_non_ff_rewrites(cmd)?;
        }

        if cmd.exit_code != 0 {
            let rebase_start = cmd
                .ref_changes
                .iter()
                .find(|change| {
                    change.reference == "HEAD"
                        && is_valid_oid(&change.old)
                        && !is_zero_oid(&change.old)
                        && is_valid_oid(&change.new)
                        && !is_zero_oid(&change.new)
                })
                .map(|change| (change.old.clone(), change.new.clone()));
            let pull_has_rebase_start =
                cmd.primary_command.as_deref() == Some("pull") && rebase_start.is_some();
            let is_rebase_like = cmd.primary_command.as_deref() == Some("rebase")
                || (cmd.primary_command.as_deref() == Some("pull")
                    && (pull_uses_rebase || pull_has_rebase_start));
            if is_rebase_like {
                let worktree = cmd.worktree.as_ref().ok_or_else(|| {
                    GitAiError::Generic(format!(
                        "rebase side-effect state requires worktree sid={}",
                        cmd.root_sid
                    ))
                })?;
                if cmd.invoked_args.iter().any(|arg| arg == "--abort") {
                    self.clear_pending_rebase_original_head_for_worktree(worktree)?;
                } else if cmd.exit_code != 0 && !rebase_is_control_mode(cmd) {
                    let semantic_old_head = rebase_start
                        .as_ref()
                        .map(|(old, _)| old.as_str())
                        .unwrap_or("");
                    let pending_old_head =
                        strict_rebase_original_head_from_command(cmd, semantic_old_head);
                    crate::wltrace::wltrace(
                        "rebase.pending_head",
                        cmd.worktree.as_deref().unwrap_or(Path::new("")),
                        || {
                            format!(
                                "old_head={} rebase_start={} ref_changes={}",
                                pending_old_head.as_deref().unwrap_or("NONE"),
                                rebase_start
                                    .as_ref()
                                    .map(|(old, new)| format!("{old}->{new}"))
                                    .unwrap_or_else(|| "NONE".to_string()),
                                cmd.ref_changes.len(),
                            )
                        },
                    );
                    if let Some(old_head) = pending_old_head {
                        let rebase_onto = rebase_start.as_ref().map(|(_, new)| new.clone());
                        if std::env::var("GIT_AI_DEBUG_DAEMON_TRACE")
                            .ok()
                            .as_deref()
                            .is_some_and(|v| v == "1")
                        {
                            tracing::debug!(
                                ?family,
                                %old_head,
                                ?rebase_onto,
                                "pending rebase original head set"
                            );
                        }
                        self.set_pending_rebase_original_head_for_worktree(
                            worktree,
                            old_head,
                            rebase_onto,
                        )?;
                    }
                }
            }
            if cmd.primary_command.as_deref() == Some("cherry-pick") {
                let worktree = cmd.worktree.as_ref().ok_or_else(|| {
                    GitAiError::Generic(format!(
                        "cherry-pick side-effect state requires worktree sid={}",
                        cmd.root_sid
                    ))
                })?;
                if cmd.invoked_args.iter().any(|arg| arg == "--abort") {
                    self.clear_pending_cherry_pick_sources_for_worktree(worktree)?;
                    self.clear_pending_cherry_pick_no_commit_for_worktree(worktree)?;
                } else if !lite_mode && cmd.exit_code != 0 {
                    let new_commits = cherry_pick_destination_commits(cmd);
                    let is_continue = cherry_pick_command_has_flag(cmd, "--continue");
                    let is_skip = cherry_pick_command_has_flag(cmd, "--skip");
                    let mut source_oids = cmd.cherry_pick_source_oids.clone();
                    let mut source_oids_from_daemon_pending = false;
                    if source_oids.is_empty()
                        && (!new_commits.is_empty()
                            || cherry_pick_state_exists_for_worktree(worktree))
                    {
                        let repo = find_repository_in_path(&worktree.to_string_lossy())?;
                        source_oids =
                            resolve_explicit_cherry_pick_sources_for_side_effect(&repo, cmd)?;
                    }
                    if source_oids.is_empty() && (is_continue || is_skip) {
                        source_oids = self.pending_cherry_pick_sources_for_worktree(worktree)?;
                        source_oids_from_daemon_pending = !source_oids.is_empty();
                    }
                    let skipped_sources = usize::from(is_skip && source_oids_from_daemon_pending);
                    let applied_source_oids = source_oids
                        .iter()
                        .skip(skipped_sources)
                        .cloned()
                        .collect::<Vec<_>>();
                    if !new_commits.is_empty() && !applied_source_oids.is_empty() {
                        let repo = find_repository_in_path(&worktree.to_string_lossy())?;
                        let original_head = cherry_pick_original_head(cmd).ok_or_else(|| {
                            GitAiError::Generic(format!(
                                "cherry-pick completed commits without original HEAD sid={}",
                                cmd.root_sid
                            ))
                        })?;
                        apply_cherry_pick_complete_rewrite(
                            &repo,
                            &original_head,
                            &applied_source_oids,
                            &new_commits,
                        )?;
                    }
                    if !source_oids.is_empty() || is_continue || is_skip {
                        let applied_sources = new_commits
                            .len()
                            .min(source_oids.len().saturating_sub(skipped_sources));
                        let consumed_sources = skipped_sources + applied_sources;
                        let remaining = source_oids
                            .iter()
                            .skip(consumed_sources.min(source_oids.len()))
                            .cloned()
                            .collect();
                        self.set_pending_cherry_pick_sources_for_worktree(worktree, remaining)?;
                    }
                }
            }
            // Fix #957: `checkout/switch --merge` exits with code 1 when it produces
            // conflict markers but HEAD still moves to the target branch.  We must not
            // return early here — fall through so apply_checkout_switch_working_log_side_effect
            // and recent_checkout_switch_prerequisite_from_command can migrate the working log.
            let is_merge_checkout =
                matches!(cmd.primary_command.as_deref(), Some("checkout" | "switch")) && {
                    let p = parsed_invocation_for_normalized_command(cmd);
                    p.has_command_flag("--merge") || p.has_command_flag("-m")
                };
            // For stash pop/apply/branch with non-zero exit (typically conflict), don't
            // skip processing. The stash may have been partially applied and attribution
            // should still be restored. We cannot rely on `has_stash_conflict_for_repo`
            // because in daemon mode the conflict check runs lazily at sync time -- by
            // which point the user may already have resolved the conflict with `git add`.
            // Instead, always attempt restoration for stash restore operations; if the
            // stash was never applied the restore is a harmless no-op.
            let is_stash_restore = cmd.primary_command.as_deref() == Some("stash")
                && events.iter().any(|event| {
                    matches!(
                        event,
                        crate::daemon::domain::SemanticEvent::StashOperation {
                            kind: crate::daemon::domain::StashOpKind::Pop
                                | crate::daemon::domain::StashOpKind::Apply
                                | crate::daemon::domain::StashOpKind::Branch,
                            ..
                        }
                    )
                });
            let is_merge_squash = cmd.primary_command.as_deref() == Some("merge")
                && events.iter().any(|event| {
                    matches!(
                        event,
                        crate::daemon::domain::SemanticEvent::MergeSquash { .. }
                    )
                });
            if !is_merge_checkout && !is_stash_restore && !is_merge_squash {
                return Ok(());
            }
            if is_stash_restore {
                tracing::debug!(
                    sid = %cmd.root_sid,
                    "stash restore with non-zero exit, continuing to restore attribution"
                );
            }
        }

        if let Some(worktree) = cmd.worktree.as_ref() {
            let worktree = worktree.to_string_lossy().to_string();
            let mut handled_revert_commits = false;
            for event in events {
                match event {
                    crate::daemon::domain::SemanticEvent::CloneCompleted { .. } => {
                        apply_clone_notes_sync_side_effect(&worktree)?;
                    }
                    crate::daemon::domain::SemanticEvent::PullCompleted { .. } => {
                        apply_pull_notes_sync_side_effect(
                            &worktree,
                            cmd.invoked_command.as_deref(),
                            &cmd.invoked_args,
                        )?;
                    }
                    crate::daemon::domain::SemanticEvent::PushCompleted { .. } => {
                        apply_push_side_effect(
                            &worktree,
                            cmd.invoked_command.as_deref(),
                            &cmd.invoked_args,
                        )?;
                    }
                    crate::daemon::domain::SemanticEvent::CherryPickComplete {
                        original_head,
                        new_head,
                        source_commits,
                        new_commits,
                    } => {
                        if lite_mode {
                            if !original_head.is_empty()
                                && !new_head.is_empty()
                                && original_head != new_head
                            {
                                let repo = find_repository_in_path(&worktree)?;
                                repo.storage.rename_working_log(original_head, new_head)?;
                            }
                            self.clear_pending_cherry_pick_sources_for_worktree(worktree.as_ref())?;
                        } else if !new_head.is_empty() {
                            let repo = find_repository_in_path(&worktree)?;
                            let mut sources = source_commits.clone();
                            let is_skip = cherry_pick_command_has_flag(cmd, "--skip");
                            let explicit_source_args = cherry_pick_source_args_for_side_effect(cmd);
                            if !sources.is_empty() {
                                self.clear_pending_cherry_pick_sources_for_worktree(
                                    worktree.as_ref(),
                                )?;
                            } else if !explicit_source_args.is_empty() {
                                let head_context =
                                    (!original_head.is_empty()).then_some(original_head.as_str());
                                sources = resolve_cherry_pick_source_args_with_git_in_head_context(
                                    &repo,
                                    &explicit_source_args,
                                    head_context,
                                )?;
                                self.clear_pending_cherry_pick_sources_for_worktree(
                                    worktree.as_ref(),
                                )?;
                            } else {
                                sources = self.take_pending_cherry_pick_sources_for_worktree(
                                    worktree.as_ref(),
                                )?;
                                if is_skip && !sources.is_empty() {
                                    sources.remove(0);
                                }
                            }
                            let destinations = if new_commits.is_empty() {
                                vec![new_head.clone()]
                            } else {
                                new_commits.clone()
                            };
                            if original_head != new_head {
                                if original_head.is_empty() {
                                    return Err(GitAiError::Generic(format!(
                                        "cherry-pick complete missing original HEAD sid={}",
                                        cmd.root_sid
                                    )));
                                }
                                apply_cherry_pick_complete_rewrite(
                                    &repo,
                                    original_head,
                                    &sources,
                                    &destinations,
                                )?;
                            }
                        }
                    }
                    crate::daemon::domain::SemanticEvent::CherryPickNoCommit {
                        source_commits,
                        head,
                    } => {
                        if !lite_mode {
                            let mut sources = source_commits.clone();
                            if sources.is_empty() {
                                let repo = find_repository_in_path(&worktree)?;
                                sources = resolve_explicit_cherry_pick_sources_for_side_effect(
                                    &repo, cmd,
                                )?;
                            }
                            if !head.is_empty() && !sources.is_empty() {
                                self.set_pending_cherry_pick_no_commit_for_worktree(
                                    worktree.as_ref(),
                                    sources,
                                    head.clone(),
                                )?;
                            }
                        }
                    }
                    crate::daemon::domain::SemanticEvent::MergeSquash { source_head, onto } => {
                        self.set_pending_squash_merge_for_worktree(
                            worktree.as_ref(),
                            source_head.clone(),
                            onto.clone(),
                        )?;
                    }
                    crate::daemon::domain::SemanticEvent::StashOperation { kind, head } => {
                        let repo = find_repository_in_path(&worktree)?;
                        match kind {
                            crate::daemon::domain::StashOpKind::Push
                            | crate::daemon::domain::StashOpKind::Unknown => {
                                let resolved_stash =
                                    cmd.stash_target_oid.as_deref().or_else(|| {
                                        cmd.ref_changes
                                        .iter()
                                        .find(|rc| rc.reference == "refs/stash")
                                        .map(|rc| rc.new.as_str())
                                        .filter(|s| {
                                            !s.is_empty()
                                                && *s != "0000000000000000000000000000000000000000"
                                        })
                                    });
                                if let Some(stash_sha) = resolved_stash {
                                    let push_head =
                                        stash_base_head(&repo, stash_sha).or_else(|| head.clone());
                                    if let Some(head_sha) = push_head.as_deref() {
                                        let pathspecs = Self::stash_pathspecs_from_command(cmd);
                                        crate::authorship::rewrite_stash::handle_stash_create(
                                            &repo, stash_sha, head_sha, pathspecs,
                                        )?;
                                    }
                                }
                            }
                            crate::daemon::domain::StashOpKind::Pop => {
                                if let Some(stash_sha) = resolve_stash_sha(cmd) {
                                    let base_head = stash_base_head(&repo, stash_sha);
                                    let target_head = head.as_deref().or(base_head.as_deref());
                                    crate::authorship::rewrite_stash::handle_stash_pop_or_apply_with_head(
                                        &repo, stash_sha, true, target_head,
                                    )?;
                                }
                            }
                            crate::daemon::domain::StashOpKind::Apply
                            | crate::daemon::domain::StashOpKind::Branch => {
                                if let Some(stash_sha) = resolve_stash_sha(cmd) {
                                    let effective_head = if matches!(
                                        kind,
                                        crate::daemon::domain::StashOpKind::Branch
                                    ) {
                                        stash_base_head(&repo, stash_sha)
                                    } else {
                                        None
                                    };
                                    let base_head = stash_base_head(&repo, stash_sha);
                                    let target_head = effective_head
                                        .as_deref()
                                        .or(head.as_deref())
                                        .or(base_head.as_deref());
                                    crate::authorship::rewrite_stash::handle_stash_pop_or_apply_with_head(
                                        &repo, stash_sha, false, target_head,
                                    )?;
                                }
                            }
                            crate::daemon::domain::StashOpKind::Drop => {
                                if let Some(stash_sha) = resolve_stash_sha(cmd) {
                                    crate::authorship::rewrite_stash::handle_stash_drop(
                                        &repo, stash_sha,
                                    )?;
                                }
                            }
                            _ => {}
                        }
                    }
                    crate::daemon::domain::SemanticEvent::CommitCreated { base, new_head } => {
                        let mut handled_as_squash_merge = false;
                        // DEFERRED (code-review #4): a pending `merge --squash` is
                        // matched to the next commit by `base == pending.onto`
                        // alone. If the user ABORTS the squash (e.g. `git reset
                        // --hard` / `git checkout -- .`) and later makes an
                        // unrelated commit on the same base, that commit is
                        // mistaken for the squash and the source ref's session
                        // metadata leaks into its note (inflating `git-ai stats`;
                        // line-level blame stays correct). A robust fix is
                        // non-trivial: the abandon commands (reset/checkout) are
                        // not currently sequenced into this side-effect layer, so
                        // we cannot clear the pending state on abort here, and a
                        // metadata-prune alternative collides with the intentional
                        // prompt-only-note feature. Left as-is pending one of
                        // those two mechanisms.
                        if !new_head.is_empty()
                            && cmd.primary_command.as_deref() == Some("commit")
                            && let Some(pending) =
                                self.take_pending_squash_merge_for_worktree(worktree.as_ref())?
                        {
                            if base.as_deref().is_some_and(|base| base == pending.onto) {
                                let repo = find_repository_in_path(&worktree)?;
                                let outcome =
                                    crate::authorship::rewrite::handle_rewrite_event_with_metrics(
                                        &repo,
                                        crate::authorship::rewrite::RewriteEvent::SquashMerge {
                                            source_head: pending.source_head,
                                            squash_commit: new_head.clone(),
                                            onto: pending.onto,
                                        },
                                    )?;
                                crate::daemon::rewrite_metrics::spawn_rewrite_commit_metrics(
                                    &repo,
                                    outcome.metric_commits,
                                );
                                handled_as_squash_merge = true;
                            } else {
                                self.set_pending_squash_merge_for_worktree(
                                    worktree.as_ref(),
                                    pending.source_head,
                                    pending.onto,
                                )?;
                            }
                        }

                        if handled_as_squash_merge {
                            // Squash authorship is reconstructed from the source ref captured
                            // in sequenced trace/reflog state at `merge --squash` time.
                        } else if is_completing_rebase || is_pull_rebase {
                            // During rebase, note transfer is handled by non-FF detection.
                            // Skip post-commit note generation to avoid overwriting shifted notes.
                        } else if !new_head.is_empty()
                            && cmd.primary_command.as_deref() == Some("revert")
                        {
                            if !handled_revert_commits {
                                handled_revert_commits = true;
                                if lite_mode {
                                    if let Some(base) = base.as_deref()
                                        && let Some(destination) =
                                            revert_destination_changes(cmd).last()
                                        && base != destination.new
                                    {
                                        let repo = find_repository_in_path(&worktree)?;
                                        repo.storage.rename_working_log(base, &destination.new)?;
                                    }
                                    continue;
                                }
                                // A single `git revert A B` creates one commit per source.
                                // Reconstruct each destination from the matching HEAD transition
                                // instead of treating the command as one final CommitCreated event.
                                let repo = find_repository_in_path(&worktree)?;
                                let mut source_oids = cmd.revert_source_oids.clone();
                                if source_oids.is_empty() {
                                    source_oids = resolve_explicit_revert_sources_for_side_effect(
                                        &repo, cmd,
                                    )?;
                                }
                                apply_revert_complete_rewrite(&repo, cmd, &source_oids)?;
                            }
                        } else if !new_head.is_empty() {
                            let repo = find_repository_in_path(&worktree)?;
                            let author = repo.effective_author_identity().formatted_or_unknown();
                            let base_opt = base.clone().filter(|b| !b.is_empty() && b != "initial");
                            crate::wltrace::wltrace(
                                "commit.post_commit",
                                Path::new(cmd.worktree.as_deref().unwrap_or(Path::new(""))),
                                || {
                                    format!(
                                        "sid={} base={:?} new_head={}",
                                        cmd.root_sid, base_opt, new_head
                                    )
                                },
                            );
                            let recovery_file_timestamps = Self::take_commit_file_timestamps(
                                commit_file_timestamp_snapshots,
                                new_head,
                            )
                            .await;
                            let recovery_preflight = |unknown_by_file: &crate::authorship::attribution_recovery::UnknownLinesByFile| {
                                self.wait_for_session_event_recovery_candidate(
                                    &repo,
                                    new_head,
                                    recovery_file_timestamps.as_ref(),
                                    unknown_by_file,
                                );
                            };

                            // Post-commit note generation does synchronous git/filesystem work
                            // and may briefly wait for transcript recovery. Mark it as blocking
                            // so the transcript worker can process the recovery sweep promptly.
                            run_blocking_side_effect(|| {
                                crate::authorship::post_commit::post_commit_from_working_log_with_recovery_timestamps(
                                    &repo,
                                    base_opt.clone(),
                                    new_head.clone(),
                                    author,
                                    true,
                                    recovery_file_timestamps.as_ref(),
                                    Some(&recovery_preflight),
                                )
                            })?;

                            if !lite_mode
                                && cmd.primary_command.as_deref() == Some("commit")
                                && let Some(pending) = self
                                    .take_pending_cherry_pick_no_commit_for_worktree(
                                        worktree.as_ref(),
                                    )?
                            {
                                if base.as_deref().is_some_and(|base| base == pending.head) {
                                    apply_cherry_pick_no_commit_rewrite(
                                        &repo,
                                        &pending.source_commits,
                                        &pending.head,
                                        new_head,
                                    )?;
                                } else {
                                    self.set_pending_cherry_pick_no_commit_for_worktree(
                                        worktree.as_ref(),
                                        pending.source_commits,
                                        pending.head,
                                    )?;
                                }
                            }
                        }
                    }
                    crate::daemon::domain::SemanticEvent::CommitAmended { old_head, new_head } => {
                        if !old_head.is_empty()
                            && !new_head.is_empty()
                            && old_head != new_head
                            && is_valid_oid(old_head)
                            && !is_zero_oid(old_head)
                            && is_valid_oid(new_head)
                            && !is_zero_oid(new_head)
                        {
                            let repo = find_repository_in_path(&worktree)?;
                            if lite_mode {
                                repo.storage.rename_working_log(old_head, new_head)?;
                                continue;
                            }
                            let author = repo.effective_author_identity().formatted_or_unknown();
                            let recovery_file_timestamps = Self::take_commit_file_timestamps(
                                commit_file_timestamp_snapshots,
                                new_head,
                            )
                            .await;
                            let recovery_preflight = |unknown_by_file: &crate::authorship::attribution_recovery::UnknownLinesByFile| {
                                self.wait_for_session_event_recovery_candidate(
                                    &repo,
                                    new_head,
                                    recovery_file_timestamps.as_ref(),
                                    unknown_by_file,
                                );
                            };
                            // Post-commit note generation does synchronous git/filesystem work
                            // and may briefly wait for transcript recovery. Mark it as blocking
                            // so the transcript worker can process the recovery sweep promptly.
                            let amend_result = run_blocking_side_effect(|| {
                                crate::authorship::post_commit::post_commit_amend_with_recovery_timestamps_detailed(
                                    &repo,
                                    old_head,
                                    new_head,
                                    author,
                                    recovery_file_timestamps.as_ref(),
                                    Some(&recovery_preflight),
                                )
                            })?;
                            if crate::authorship::rewrite::rewrite_metrics_enabled() {
                                crate::daemon::rewrite_metrics::spawn_rewrite_commit_metrics(
                                    &repo,
                                    vec![
                                        crate::authorship::rewrite::RewriteMetricCommit::new(
                                            new_head.to_string(),
                                            vec![old_head.to_string()],
                                            crate::authorship::rewrite::RewriteMetricOperation::Amend,
                                        )
                                        .with_parent_sha(amend_result.parent_sha)
                                        .with_authorship_note(amend_result.authorship_note),
                                    ],
                                );
                            }
                        }
                    }
                    crate::daemon::domain::SemanticEvent::Reset {
                        kind,
                        old_head,
                        new_head,
                    } if !old_head.is_empty() && !new_head.is_empty() && old_head != new_head => {
                        let repo = find_repository_in_path(&worktree)?;
                        match kind {
                            crate::daemon::domain::ResetKind::Hard => {
                                repo.storage.delete_working_log_for_base_commit(old_head)?;
                            }
                            _ => {
                                if is_ancestor_commit(&repo, new_head, old_head) {
                                    crate::authorship::rewrite_reset::reconstruct_working_log_after_backward_reset(
                                        &repo, old_head, new_head,
                                    )?;
                                } else if is_ancestor_commit(&repo, old_head, new_head) {
                                    // Forward reset (e.g. syncing onto a newer upstream
                                    // commit): carry the working log to the new base,
                                    // matching the pull fast-forward side effect.
                                    repo.storage.rename_working_log(old_head, new_head)?;
                                } else if !lite_mode {
                                    let outcome =
                                        crate::authorship::rewrite::handle_rewrite_event_with_metrics(
                                        &repo,
                                        crate::authorship::rewrite::RewriteEvent::NonFastForward {
                                            old_tip: old_head.to_string(),
                                            new_tip: new_head.to_string(),
                                            onto: None,
                                        },
                                    )?;
                                    crate::daemon::rewrite_metrics::spawn_rewrite_commit_metrics(
                                        &repo,
                                        outcome.metric_commits,
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if matches!(cmd.primary_command.as_deref(), Some("checkout" | "switch")) {
            if let Some(prerequisite) = recent_checkout_switch_prerequisite_from_command(cmd) {
                let family = family.map(std::borrow::ToOwned::to_owned).or_else(|| {
                    cmd.worktree.as_ref().and_then(|worktree| {
                        find_repository_in_path(&worktree.to_string_lossy())
                            .ok()
                            .map(|repo| family_key_for_repository(&repo))
                    })
                });
                if let Some(family) = family {
                    self.record_recent_replay_prerequisite(&family, prerequisite)?;
                }
            }
            apply_checkout_switch_working_log_side_effect(cmd)?;
        }

        if saw_pull_event && let Some(worktree) = cmd.worktree.as_ref() {
            let (old_head, new_head) = Self::resolve_heads_for_command(cmd);
            if !old_head.is_empty() && !new_head.is_empty() && old_head != new_head {
                let repo = find_repository_in_path(&worktree.to_string_lossy())?;
                if repo_is_ancestor(&repo, &old_head, &new_head) {
                    apply_pull_fast_forward_working_log_side_effect(
                        &worktree.to_string_lossy(),
                        &old_head,
                        &new_head,
                    )?;
                }
            }
        }

        // Handle update-ref: migrate working logs and authorship notes when the ref
        // update affects the currently checked-out branch.
        if primary == "update-ref"
            && let Some(worktree) = cmd.worktree.as_ref()
        {
            for event in events {
                if let crate::daemon::domain::SemanticEvent::RefUpdated {
                    reference,
                    old,
                    new,
                } = event
                {
                    if reference != "HEAD" && !reference.starts_with("refs/heads/")
                        || !is_valid_oid(old)
                        || is_zero_oid(old)
                        || !is_valid_oid(new)
                        || is_zero_oid(new)
                        || old == new
                    {
                        continue;
                    }
                    if lite_mode {
                        // The trace-derived pass above already moved the working log when this
                        // transition also moved HEAD. Avoid the commit-graph lookup and note write.
                        continue;
                    }
                    let repo = find_repository_in_path(&worktree.to_string_lossy())?;
                    if repo_is_ancestor(&repo, old, new) {
                        let affects_checked_out_branch = reference == "HEAD"
                            || cmd.ref_changes.iter().any(|change| {
                                change.reference == "HEAD"
                                    && change.old == *old
                                    && change.new == *new
                            });
                        if affects_checked_out_branch {
                            if repo.storage.has_working_log(old) {
                                let author =
                                    repo.effective_author_identity().formatted_or_unknown();
                                crate::authorship::post_commit::post_commit_from_working_log(
                                    &repo,
                                    Some(old.to_string()),
                                    new.to_string(),
                                    author,
                                    true,
                                )?;
                            }
                            repo.storage.rename_working_log(old, new)?;
                        }
                    } else {
                        crate::authorship::rewrite::handle_rewrite_event(
                            &repo,
                            crate::authorship::rewrite::RewriteEvent::NonFastForward {
                                old_tip: old.to_string(),
                                new_tip: new.to_string(),
                                onto: None,
                            },
                        )?;
                    }
                }
            }
        }

        let parsed_invocation = parsed_invocation_for_normalized_command(cmd);
        for trigger in transcript_sweep_triggers_for_events(events) {
            if trigger == crate::daemon::stream_worker::SweepTrigger::PostPush
                && crate::git::cli_parser::is_dry_run(&parsed_invocation.command_args)
            {
                tracing::debug!("transcript sweep trigger skipped for dry-run push");
                continue;
            }
            self.trigger_transcript_sweep(trigger);
        }

        Ok(())
    }

    async fn apply_trace_payload_to_state(
        &self,
        payload: Value,
    ) -> Result<TracePayloadApplyOutcome, GitAiError> {
        let payload_root_sid = Self::trace_payload_root_sid(&payload);
        let event = payload
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if event == TRACE_CONNECTION_CLOSED_EVENT {
            let Some(root_sid) = payload_root_sid.as_deref() else {
                return Ok(TracePayloadApplyOutcome::None);
            };
            {
                let mut normalizer = self.normalizer.lock().await;
                let _ = normalizer.sweep_orphans_for_roots(&[root_sid.to_string()]);
            }
            let replaced_family = self
                .replace_pending_root_entry(root_sid, FamilySequencerEntry::Canceled)
                .await?;
            let outcome = if replaced_family.is_some() {
                TracePayloadApplyOutcome::QueuedFamily
            } else {
                TracePayloadApplyOutcome::None
            };
            self.clear_trace_root_tracking(root_sid)?;
            self.drain_ready_family_sequencers_after_root_cleared(replaced_family)
                .await?;
            return Ok(outcome);
        }

        self.maybe_append_pending_root_from_trace_payload(&payload)?;
        let emitted = {
            let mut normalizer = self.normalizer.lock().await;
            normalizer.ingest_payload(&payload)?
        };
        let Some(command) = emitted else {
            if is_terminal_root_trace_event(
                &event,
                payload
                    .get("sid")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                payload_root_sid.as_deref().unwrap_or_default(),
            ) && let Some(root_sid) = payload_root_sid.as_deref()
                && let Some(family) = self
                    .replace_pending_root_entry(root_sid, FamilySequencerEntry::Canceled)
                    .await?
            {
                self.clear_trace_root_tracking(root_sid)?;
                self.drain_ready_family_sequencers_after_root_cleared(Some(family))
                    .await?;
                return Ok(TracePayloadApplyOutcome::QueuedFamily);
            }
            return Ok(TracePayloadApplyOutcome::None);
        };
        let root_sid = command.root_sid.clone();

        let mut family_to_drain_after_clear = None;
        let outcome = if let Some(family) = self
            .replace_pending_root_entry(
                &root_sid,
                FamilySequencerEntry::ReadyCommand(Box::new(command.clone())),
            )
            .await?
        {
            self.cache_commit_file_timestamp_snapshots_for_command(&command)?;
            family_to_drain_after_clear = Some(family);
            TracePayloadApplyOutcome::QueuedFamily
        } else if let Some(family) = command.family_key.as_ref().map(|family| family.0.clone())
            && Self::trace_invocation_participates_in_family_sequencer(
                command.primary_command.as_deref(),
                &command.raw_argv,
            )
        {
            self.cache_commit_file_timestamp_snapshots_for_command(&command)?;
            self.append_ready_command_entry(&family, command).await?;
            family_to_drain_after_clear = Some(family);
            TracePayloadApplyOutcome::QueuedFamily
        } else {
            match self.coordinator.route_command(command).await {
                Ok(applied) => TracePayloadApplyOutcome::Applied(Box::new(applied)),
                Err(error) => {
                    let _ = self.clear_trace_root_tracking(&root_sid);
                    return Err(error);
                }
            }
        };
        self.clear_trace_root_tracking(&root_sid)?;
        self.drain_ready_family_sequencers_after_root_cleared(family_to_drain_after_clear)
            .await?;
        Ok(outcome)
    }

    async fn ingest_trace_payload_fast(self: Arc<Self>, payload: Value) -> Result<(), GitAiError> {
        if !is_trace_payload(&payload) {
            return Ok(());
        }
        match self.apply_trace_payload_to_state(payload).await? {
            TracePayloadApplyOutcome::None | TracePayloadApplyOutcome::QueuedFamily => {}
            TracePayloadApplyOutcome::Applied(applied) => {
                if let Some(family) = applied.command.family_key.as_ref().map(|key| key.0.clone()) {
                    self.begin_family_effect(&family)?;
                    let mut commit_file_timestamp_snapshots =
                        Self::start_commit_file_timestamp_snapshots_for_command(&applied.command);
                    let result = self
                        .maybe_apply_side_effects_for_applied_command(
                            Some(&family),
                            &applied,
                            &mut commit_file_timestamp_snapshots,
                        )
                        .await;
                    let _ = self.end_family_effect(&family);
                    if let Err(error) = &result {
                        let _ = self.record_side_effect_error(&family, applied.seq, error);
                        tracing::error!(
                            %error,
                            %family,
                            seq = applied.seq,
                            "async side-effect error"
                        );
                    }
                    if let Err(error) =
                        self.append_command_completion_log(&family, &applied, &result, applied.seq)
                    {
                        let _ = self.record_side_effect_error(&family, applied.seq, &error);
                        tracing::error!(
                            %error,
                            %family,
                            seq = applied.seq,
                            "async completion log write failed"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    async fn watermarks_for_family(
        &self,
        repo_working_dir: String,
    ) -> Result<crate::daemon::domain::WatermarkState, GitAiError> {
        self.coordinator
            .watermarks_family(Path::new(&repo_working_dir))
            .await
    }

    async fn status_for_family(
        &self,
        repo_working_dir: String,
    ) -> Result<FamilyStatus, GitAiError> {
        let family = self.backend.resolve_family(Path::new(&repo_working_dir))?;
        let status = self
            .coordinator
            .status_family(Path::new(&repo_working_dir))
            .await?;
        let latest_seq = status.applied_seq;
        let family_key = family.0;
        Ok(FamilyStatus {
            family_key: family_key.clone(),
            latest_seq,
            last_error: status
                .last_error
                .or_else(|| self.latest_side_effect_error(&family_key).ok().flatten()),
        })
    }

    async fn wait_for_checkpoint_admission_through(&self, target: u64) {
        loop {
            // Enroll before checking (see wait_for_trace_ingest_seq): the
            // final admission's notify_waiters must not race the load.
            let progress = self.checkpoint_progress_notify.notified();
            tokio::pin!(progress);
            progress.as_mut().enable();
            let processed = self
                .processed_checkpoint_receipt_seq
                .load(Ordering::Acquire) as u64;
            if processed >= target {
                return;
            }
            tokio::select! {
                _ = &mut progress => {}
                _ = self.wait_for_shutdown() => return,
            }
        }
    }

    async fn wait_for_no_unadmitted_checkpoints(&self) {
        loop {
            // Enroll before checking (see wait_for_trace_ingest_seq).
            let progress = self.checkpoint_progress_notify.notified();
            tokio::pin!(progress);
            progress.as_mut().enable();
            if self.unadmitted_checkpoints.load(Ordering::Acquire) == 0 {
                return;
            }
            tokio::select! {
                _ = &mut progress => {}
                _ = self.wait_for_shutdown() => return,
            }
        }
    }

    async fn sync_family(&self, repo_working_dir: String) -> Result<FamilyStatus, GitAiError> {
        let checkpoint_target = self.next_checkpoint_receipt_seq.load(Ordering::Acquire) as u64;
        self.wait_for_checkpoint_admission_through(checkpoint_target)
            .await;
        self.wait_for_no_unadmitted_checkpoints().await;
        let family = self.backend.resolve_family(Path::new(&repo_working_dir))?;
        self.wait_for_trace_ingest_processed_through_family(&family.0)
            .await;

        let exec_lock = self.side_effect_exec_lock(&family.0)?;
        loop {
            self.wait_for_no_unadmitted_checkpoints().await;
            let guard = exec_lock.lock().await;
            self.drain_ready_family_sequencer_entries_locked(&family.0)
                .await?;
            if self.unadmitted_checkpoints.load(Ordering::Acquire) == 0 {
                drop(guard);
                break;
            }
            drop(guard);
        }

        self.status_for_family(repo_working_dir).await
    }

    async fn drain_accepted_checkpoints(&self) -> Result<(), GitAiError> {
        loop {
            let checkpoint_target = self.next_checkpoint_receipt_seq.load(Ordering::Acquire) as u64;
            self.wait_for_checkpoint_admission_through(checkpoint_target)
                .await;
            self.wait_for_no_unadmitted_checkpoints().await;
            self.wait_for_trace_ingest_processed_through().await;
            self.drain_all_ready_family_sequencers().await?;

            if self.outstanding_checkpoint_state().0 == 0 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait for the daemon to finish all in-flight work and telemetry flushing.
    ///
    /// Progress is logged every few seconds. Returns an `AwaitResult` describing
    /// whether the daemon was idle before the timeout and how much telemetry
    /// (if any) is still pending.
    async fn await_completion(&self, timeout_secs: u64) -> AwaitResult {
        use tokio::time::{Duration, Instant, timeout};

        let start = Instant::now();
        let deadline = start + Duration::from_secs(timeout_secs);
        let log_interval = Duration::from_secs(3);
        let mut last_log = start;

        let mut result = AwaitResult {
            done: false,
            timed_out: false,
            metrics_remaining: 0,
            notes_remaining: 0,
        };

        let mut maybe_log = |phase: &str| {
            let now = Instant::now();
            if now - last_log >= log_interval {
                tracing::info!(phase, "await: still waiting");
                eprintln!("await: still waiting for {}...", phase);
                last_log = now;
            }
        };

        // Phase 1: wait for the trace-ingest and family-sequencer work side.
        while !self.is_shutting_down() {
            let now = Instant::now();
            if now >= deadline {
                result.timed_out = true;
                break;
            }
            let remaining = deadline - now;

            maybe_log("daemon work");
            if timeout(remaining, self.wait_for_trace_ingest_processed_through())
                .await
                .is_err()
            {
                result.timed_out = true;
                break;
            }

            if self.is_shutting_down() {
                break;
            }

            let now = Instant::now();
            if now >= deadline {
                result.timed_out = true;
                break;
            }
            let remaining = deadline - now;

            if timeout(remaining, self.drain_all_ready_family_sequencers())
                .await
                .is_err()
            {
                result.timed_out = true;
                break;
            }

            if !self.has_pending_daemon_work() {
                break;
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        if self.is_shutting_down() {
            result.timed_out = true;
        }

        // Phase 2: drain the transcript/stream worker.
        if !result.timed_out
            && let Some(worker) = &self.stream_worker
        {
            let now = Instant::now();
            if now < deadline {
                let remaining = deadline - now;
                maybe_log("transcript processing");
                match timeout(remaining, worker.drain()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "await: transcript drain failed");
                    }
                    Err(_) => {
                        result.timed_out = true;
                    }
                }
            } else {
                result.timed_out = true;
            }
        }

        // Phase 3: flush telemetry and wait for the worker to finish.
        if !result.timed_out
            && let Some(worker) = &self.telemetry_worker
        {
            let now = Instant::now();
            if now < deadline {
                let remaining = deadline - now;
                maybe_log("telemetry flush");
                match timeout(remaining, worker.flush_and_wait()).await {
                    Ok(Ok(status)) => {
                        result.metrics_remaining = status.metrics_remaining;
                        result.notes_remaining = status.notes_remaining;
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "await: telemetry flush failed");
                    }
                    Err(_) => {
                        result.timed_out = true;
                    }
                }
            } else {
                result.timed_out = true;
            }
        }

        result.done = !result.timed_out
            && result.metrics_remaining == 0
            && result.notes_remaining == 0
            && !self.has_pending_daemon_work();
        result
    }

    fn has_pending_daemon_work(&self) -> bool {
        if self.checkpoint_ingress_quota.outstanding().0 > 0 {
            return true;
        }
        if self.queued_trace_payloads.load(Ordering::Acquire) > 0 {
            return true;
        }
        if self.next_trace_ingest_seq.load(Ordering::Acquire)
            > self.processed_trace_ingest_seq.load(Ordering::Acquire)
        {
            return true;
        }
        if self.has_open_trace_roots_that_may_mutate_refs() {
            return true;
        }
        if let Ok(map) = self.inflight_effects_by_family.lock()
            && !map.is_empty()
        {
            return true;
        }
        if let Ok(map) = self.family_sequencers_by_family.lock() {
            for state in map.values() {
                if !state.entries.is_empty() {
                    return true;
                }
            }
        }
        false
    }

    fn outstanding_checkpoint_state(&self) -> (usize, usize) {
        self.checkpoint_ingress_quota.outstanding()
    }

    fn set_checkpoint_acceptance(&self, accepting: bool) -> Result<(), GitAiError> {
        let _sequencers = self
            .family_sequencers_by_family
            .lock()
            .map_err(|_| GitAiError::Generic("family sequencer map lock poisoned".to_string()))?;
        self.accepting_checkpoints
            .store(accepting, Ordering::Release);
        Ok(())
    }

    async fn handle_control_request(&self, request: ControlRequest) -> ControlResponse {
        let result = match request {
            ControlRequest::Ping => Ok(ControlResponse::ok(None, None)),
            ControlRequest::CheckpointRun { .. } => Err(GitAiError::Generic(
                "checkpoint.run requires the framed checkpoint transport".to_string(),
            )),
            ControlRequest::SyncFamily { repo_working_dir } => {
                self.sync_family(repo_working_dir).await.and_then(|status| {
                    serde_json::to_value(status)
                        .map(|v| ControlResponse::ok(None, Some(v)))
                        .map_err(GitAiError::from)
                })
            }
            ControlRequest::StatusFamily { repo_working_dir } => self
                .status_for_family(repo_working_dir)
                .await
                .and_then(|status| {
                    serde_json::to_value(status)
                        .map(|v| ControlResponse::ok(None, Some(v)))
                        .map_err(GitAiError::from)
                }),
            ControlRequest::SnapshotWatermarks { repo_working_dir } => self
                .watermarks_for_family(repo_working_dir.clone())
                .await
                .and_then(|ws| {
                    let worktree_key = Self::worktree_state_key(Path::new(&repo_working_dir));
                    let worktree_wm = ws.per_worktree.get(&worktree_key).copied();
                    serde_json::to_value(json!({
                        "watermarks": ws.per_file,
                        "worktree_watermark": worktree_wm,
                    }))
                    .map(|v| ControlResponse::ok(None, Some(v)))
                    .map_err(GitAiError::from)
                }),
            ControlRequest::SubmitTelemetry { envelopes } => {
                if let Some(worker) = &self.telemetry_worker {
                    worker.submit_telemetry(envelopes).await;
                }
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::SubmitCas { records } => {
                if let Some(worker) = &self.telemetry_worker {
                    worker.submit_cas(records).await;
                }
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::FlushNotes => {
                // Trigger an immediate notes flush in a blocking task.
                // Fire-and-forget: the periodic flush loop is the safety net.
                tokio::task::spawn_blocking(|| {
                    crate::daemon::telemetry_worker::flush_notes();
                });
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::Await { timeout_secs } => {
                let result = self.await_completion(timeout_secs).await;
                serde_json::to_value(result)
                    .map(|v| ControlResponse::ok(None, Some(v)))
                    .map_err(GitAiError::from)
            }
            ControlRequest::BashSessionStart {
                repo_work_dir,
                original_cwd,
                session_id,
                tool_use_id,
                agent_id,
                metadata,
                stat_snapshot,
                trace_id,
                started_at_ns,
                command,
            } => {
                let worktree_key = Self::worktree_state_key(Path::new(&repo_work_dir));
                let original_cwd = original_cwd.unwrap_or_else(|| repo_work_dir.clone());
                if let Ok(db) = crate::daemon::bash_history_db::BashHistoryDatabase::global()
                    && let Ok(mut db_lock) = db.lock()
                    && let Err(e) =
                        db_lock.record_start(&crate::daemon::bash_history_db::BashCallStart {
                            original_cwd: Self::worktree_state_key(Path::new(&original_cwd)),
                            repo_work_dir: Some(worktree_key.clone()),
                            repo_discovery_error: None,
                            session_id: session_id.clone(),
                            tool_use_id: tool_use_id.clone(),
                            agent_id: agent_id.clone(),
                            start_trace_id: trace_id.clone(),
                            started_at_ns,
                            command: command.clone(),
                            metadata: metadata.clone(),
                        })
                {
                    tracing::debug!("failed to persist bash session start: {}", e);
                }

                let mut state = self.bash_sessions.lock().unwrap();
                state.start_session(crate::daemon::bash_sessions::BashSessionStart {
                    session_id,
                    tool_use_id,
                    repo_work_dir: worktree_key,
                    agent_id,
                    metadata,
                    stat_snapshot: *stat_snapshot,
                    start_trace_id: trace_id,
                    started_at_ns,
                    command,
                });
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::BashSessionEnd {
                repo_work_dir,
                original_cwd,
                session_id,
                tool_use_id,
                agent_id,
                metadata,
                trace_id,
                ended_at_ns,
                command,
            } => {
                let mut state = self.bash_sessions.lock().unwrap();
                let session = state.end_session(&session_id, &tool_use_id);
                drop(state);

                let worktree_key = session
                    .as_ref()
                    .map(|s| s.repo_work_dir.clone())
                    .unwrap_or_else(|| Self::worktree_state_key(Path::new(&repo_work_dir)));
                let original_cwd = original_cwd
                    .map(|cwd| Self::worktree_state_key(Path::new(&cwd)))
                    .unwrap_or_else(|| worktree_key.clone());
                let start_trace_id = session.as_ref().map(|s| s.start_trace_id.clone());
                let started_at_ns = session.as_ref().map(|s| s.started_at_ns);
                let command = command.or_else(|| session.as_ref().and_then(|s| s.command.clone()));
                let agent_id = session
                    .as_ref()
                    .map(|s| s.agent_id.clone())
                    .unwrap_or(agent_id);
                let metadata = if metadata.is_empty() {
                    session
                        .as_ref()
                        .map(|s| s.metadata.clone())
                        .unwrap_or_default()
                } else {
                    metadata
                };
                if let Ok(db) = crate::daemon::bash_history_db::BashHistoryDatabase::global()
                    && let Ok(mut db_lock) = db.lock()
                    && let Err(e) =
                        db_lock.record_end(&crate::daemon::bash_history_db::BashCallEnd {
                            original_cwd,
                            repo_work_dir: Some(worktree_key),
                            repo_discovery_error: None,
                            session_id,
                            tool_use_id,
                            agent_id,
                            start_trace_id,
                            end_trace_id: trace_id,
                            started_at_ns,
                            ended_at_ns,
                            command,
                            metadata,
                        })
                {
                    tracing::debug!("failed to persist bash session end: {}", e);
                }
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::BashHookAttemptStart {
                original_cwd,
                discovered_repo_work_dir,
                repo_discovery_error,
                session_id,
                tool_use_id,
                agent_id,
                metadata,
                trace_id,
                started_at_ns,
                command,
            } => {
                let discovered_repo_work_dir = discovered_repo_work_dir
                    .as_deref()
                    .map(Path::new)
                    .map(Self::worktree_state_key);
                if let Ok(db) = crate::daemon::bash_history_db::BashHistoryDatabase::global()
                    && let Ok(mut db_lock) = db.lock()
                    && let Err(e) =
                        db_lock.record_start(&crate::daemon::bash_history_db::BashCallStart {
                            original_cwd: Self::worktree_state_key(Path::new(&original_cwd)),
                            repo_work_dir: discovered_repo_work_dir,
                            repo_discovery_error,
                            session_id,
                            tool_use_id,
                            agent_id,
                            start_trace_id: trace_id,
                            started_at_ns,
                            command,
                            metadata,
                        })
                {
                    tracing::debug!("failed to persist bash hook attempt start: {}", e);
                }
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::BashHookAttemptEnd {
                original_cwd,
                discovered_repo_work_dir,
                repo_discovery_error,
                session_id,
                tool_use_id,
                agent_id,
                metadata,
                trace_id,
                ended_at_ns,
                command,
            } => {
                let discovered_repo_work_dir = discovered_repo_work_dir
                    .as_deref()
                    .map(Path::new)
                    .map(Self::worktree_state_key);
                if let Ok(db) = crate::daemon::bash_history_db::BashHistoryDatabase::global()
                    && let Ok(mut db_lock) = db.lock()
                    && let Err(e) =
                        db_lock.record_end(&crate::daemon::bash_history_db::BashCallEnd {
                            original_cwd: Self::worktree_state_key(Path::new(&original_cwd)),
                            repo_work_dir: discovered_repo_work_dir,
                            repo_discovery_error,
                            session_id,
                            tool_use_id,
                            agent_id,
                            start_trace_id: None,
                            end_trace_id: trace_id,
                            started_at_ns: None,
                            ended_at_ns,
                            command,
                            metadata,
                        })
                {
                    tracing::debug!("failed to persist bash hook attempt end: {}", e);
                }
                Ok(ControlResponse::ok(None, None))
            }
            ControlRequest::BashSessionQuery { repo_work_dir } => {
                let state = self.bash_sessions.lock().unwrap();
                let repo_work_dir = Self::worktree_state_key(Path::new(&repo_work_dir));
                let response = match state.query_active_for_repo(&repo_work_dir) {
                    Some((key, session)) => {
                        let data = serde_json::to_value(BashSessionQueryResponse {
                            active: true,
                            agent_id: Some(session.agent_id.clone()),
                            session_id: Some(key.0.clone()),
                            tool_use_id: Some(key.1.clone()),
                            metadata: Some(session.metadata.clone()),
                        })
                        .ok();
                        ControlResponse::ok(None, data)
                    }
                    None => {
                        let data = serde_json::to_value(BashSessionQueryResponse {
                            active: false,
                            agent_id: None,
                            session_id: None,
                            tool_use_id: None,
                            metadata: None,
                        })
                        .ok();
                        ControlResponse::ok(None, data)
                    }
                };
                Ok(response)
            }
            ControlRequest::BashSnapshotQuery {
                session_id,
                tool_use_id,
            } => {
                let state = self.bash_sessions.lock().unwrap();
                let response = match state.get_snapshot(&session_id, &tool_use_id) {
                    Some(snapshot) => {
                        let data = serde_json::to_value(BashSnapshotQueryResponse {
                            found: true,
                            stat_snapshot: Some(snapshot.clone()),
                        })
                        .ok();
                        ControlResponse::ok(None, data)
                    }
                    None => {
                        let data = serde_json::to_value(BashSnapshotQueryResponse {
                            found: false,
                            stat_snapshot: None,
                        })
                        .ok();
                        ControlResponse::ok(None, data)
                    }
                };
                Ok(response)
            }
            ControlRequest::Shutdown => match self.set_checkpoint_acceptance(false) {
                Err(error) => Err(error),
                Ok(()) => {
                    tracing::info!(
                        component = "daemon",
                        phase = "shutdown",
                        "checkpoint acceptance closed for graceful shutdown"
                    );
                    match self.drain_accepted_checkpoints().await {
                        Ok(()) => Ok(ControlResponse::ok(None, None)),
                        Err(error) => {
                            if let Err(reopen_error) = self.set_checkpoint_acceptance(true) {
                                tracing::error!(
                                    %reopen_error,
                                    "failed reopening checkpoint acceptance after shutdown error"
                                );
                            }
                            Err(error)
                        }
                    }
                }
            },
        };

        match result {
            Ok(response) => response,
            Err(error) => ControlResponse::err(error.to_string()),
        }
    }
}

fn control_listener_loop_actor(
    control_socket_path: PathBuf,
    coordinator: Arc<ActorDaemonCoordinator>,
    runtime_handle: tokio::runtime::Handle,
) -> Result<(), GitAiError> {
    #[cfg(not(windows))]
    {
        remove_socket_if_exists(&control_socket_path)?;
        let listener = ListenerOptions::new()
            .name(local_socket_name(&control_socket_path)?)
            .create_sync()
            .map_err(|e| GitAiError::Generic(format!("failed binding control socket: {}", e)))?;
        set_socket_owner_only(&control_socket_path)?;
        for stream in listener.incoming() {
            if coordinator.is_shutting_down() {
                break;
            }
            let Ok(stream) = stream else {
                continue;
            };
            let coord = coordinator.clone();
            let handle = runtime_handle.clone();
            if std::thread::Builder::new()
                .spawn(move || {
                    if let Err(e) = handle_control_connection_actor(stream, coord, handle) {
                        tracing::error!(
                            component = "daemon",
                            phase = "control_receive",
                            reason = "connection_error",
                            error = %e,
                            "daemon control connection failed"
                        );
                    }
                })
                .is_err()
            {
                tracing::error!("control listener: failed to spawn handler thread");
                break;
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    {
        let mut workers = Vec::new();
        let worker_count = windows_control_pipe_worker_count();
        let first_connecting = windows_pipe_connecting_server(&control_socket_path, true)?;
        {
            let path = control_socket_path.clone();
            let coord = coordinator.clone();
            let handle = runtime_handle.clone();
            workers.push(std::thread::spawn(move || {
                let result =
                    windows_control_pipe_worker_loop(path, first_connecting, coord.clone(), handle);
                if let Err(error) = &result {
                    tracing::error!(%error, "control worker error");
                    coord.request_shutdown();
                }
                result
            }));
        }
        for _ in 1..worker_count {
            let path = control_socket_path.clone();
            let coord = coordinator.clone();
            let handle = runtime_handle.clone();
            let connecting = windows_pipe_connecting_server(&path, false)?;
            workers.push(std::thread::spawn(move || {
                let result =
                    windows_control_pipe_worker_loop(path, connecting, coord.clone(), handle);
                if let Err(error) = &result {
                    tracing::error!(%error, "control worker error");
                    coord.request_shutdown();
                }
                result
            }));
        }

        while !coordinator.is_shutting_down() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        wake_windows_pipe_workers(&control_socket_path, worker_count);

        for worker in workers {
            let result = worker
                .join()
                .map_err(|_| GitAiError::Generic("daemon control worker panicked".to_string()))?;
            result?;
        }

        Ok(())
    }
}

#[cfg(windows)]
fn windows_pipe_connecting_server(
    pipe_path: &Path,
    first_instance: bool,
) -> Result<WindowsConnectingServer, GitAiError> {
    let mut options = WindowsPipeOptions::new(pipe_path.as_os_str());
    options
        .first(first_instance)
        .open_mode(WindowsPipeOpenMode::Duplex);
    options.single().map_err(|e| {
        GitAiError::Generic(format!(
            "failed binding windows daemon pipe {}: {}",
            pipe_path.display(),
            e
        ))
    })
}

#[cfg(windows)]
fn windows_trace_pipe_worker_count() -> usize {
    #[cfg(feature = "test-support")]
    if let Ok(raw) = std::env::var("GIT_AI_TEST_WINDOWS_TRACE_PIPE_WORKERS")
        && let Ok(count) = raw.parse::<usize>()
        && count > 0
    {
        return count;
    }

    WINDOWS_TRACE_PIPE_WORKERS
}

#[cfg(windows)]
fn windows_control_pipe_worker_count() -> usize {
    #[cfg(feature = "test-support")]
    if let Ok(raw) = std::env::var("GIT_AI_TEST_WINDOWS_CONTROL_PIPE_WORKERS")
        && let Ok(count) = raw.parse::<usize>()
        && count > 0
    {
        return count;
    }

    WINDOWS_CONTROL_PIPE_WORKERS
}

#[cfg(windows)]
fn wake_windows_pipe_workers(pipe_path: &Path, worker_count: usize) {
    for _ in 0..worker_count {
        let _ = WindowsPipeClient::connect_ms(pipe_path.as_os_str(), 100);
    }
}

#[cfg(windows)]
fn windows_control_pipe_worker_loop(
    control_socket_path: PathBuf,
    mut connecting: WindowsConnectingServer,
    coordinator: Arc<ActorDaemonCoordinator>,
    runtime_handle: tokio::runtime::Handle,
) -> Result<(), GitAiError> {
    loop {
        let server = connecting.wait().map_err(|e| {
            GitAiError::Generic(format!(
                "failed accepting control pipe {}: {}",
                control_socket_path.display(),
                e
            ))
        })?;

        if coordinator.is_shutting_down() {
            let _ = server.disconnect();
            break;
        }

        connecting = windows_pipe_connecting_server(&control_socket_path, false)?;

        let coord = coordinator.clone();
        let handle = runtime_handle.clone();
        std::thread::Builder::new()
            .spawn(move || {
                handle_windows_control_pipe_connection(server, coord, handle);
            })
            .map_err(|e| {
                GitAiError::Generic(format!(
                    "failed spawning control pipe handler for {}: {}",
                    control_socket_path.display(),
                    e
                ))
            })?;
    }

    Ok(())
}

#[cfg(windows)]
fn handle_windows_control_pipe_connection(
    server: WindowsPipeServer,
    coordinator: Arc<ActorDaemonCoordinator>,
    runtime_handle: tokio::runtime::Handle,
) {
    let mut reader = BufReader::new(server);
    if let Err(e) = handle_control_connection_actor_reader(&mut reader, coordinator, runtime_handle)
    {
        tracing::error!(
            component = "daemon",
            phase = "control_receive",
            reason = "connection_error",
            error = %e,
            "daemon control connection failed"
        );
    }
}

trait ControlConnection: Read + Write {
    fn set_receive_timeout(&mut self, timeout: Option<Duration>) -> Result<(), GitAiError>;
}

#[cfg(windows)]
impl ControlConnection for WindowsPipeServer {
    fn set_receive_timeout(&mut self, timeout: Option<Duration>) -> Result<(), GitAiError> {
        self.set_read_timeout(timeout);
        Ok(())
    }
}

#[cfg(not(windows))]
impl ControlConnection for LocalSocketStream {
    fn set_receive_timeout(&mut self, timeout: Option<Duration>) -> Result<(), GitAiError> {
        self.set_recv_timeout(timeout).map_err(|error| {
            GitAiError::Generic(format!(
                "failed setting daemon control receive timeout: {error}"
            ))
        })
    }
}

fn control_receive_timed_out(error: &GitAiError) -> bool {
    matches!(
        error,
        GitAiError::IoError(io_error)
            if matches!(
                io_error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
    )
}

#[cfg(not(windows))]
fn handle_control_connection_actor(
    stream: LocalSocketStream,
    coordinator: Arc<ActorDaemonCoordinator>,
    runtime_handle: tokio::runtime::Handle,
) -> Result<(), GitAiError> {
    let mut reader = BufReader::new(stream);
    handle_control_connection_actor_reader(&mut reader, coordinator, runtime_handle)
}

fn handle_control_connection_actor_reader<R: ControlConnection>(
    reader: &mut BufReader<R>,
    coordinator: Arc<ActorDaemonCoordinator>,
    runtime_handle: tokio::runtime::Handle,
) -> Result<(), GitAiError> {
    reader
        .get_mut()
        .set_receive_timeout(Some(DAEMON_CONTROL_RECEIVE_TIMEOUT))?;
    let mut uses_idle_timeout = false;
    loop {
        let line = match read_json_line(reader) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) if control_receive_timed_out(&error) => break,
            Err(error) => return Err(error),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed = serde_json::from_str::<ControlRequest>(trimmed);
        if !uses_idle_timeout
            && matches!(
                &parsed,
                Ok(request) if !matches!(request, ControlRequest::CheckpointRun { .. })
            )
        {
            reader
                .get_mut()
                .set_receive_timeout(Some(DAEMON_CONTROL_IDLE_TIMEOUT))?;
            uses_idle_timeout = true;
        }
        let mut shutdown_after_response = false;
        let response = match parsed {
            Ok(ControlRequest::CheckpointRun { body_bytes }) => {
                if !coordinator.accepting_checkpoints.load(Ordering::Acquire) {
                    let acceptance_closed = {
                        let _sequencers =
                            coordinator
                                .family_sequencers_by_family
                                .lock()
                                .map_err(|_| {
                                    GitAiError::Generic(
                                        "family sequencer map lock poisoned".to_string(),
                                    )
                                })?;
                        !coordinator.accepting_checkpoints.load(Ordering::Acquire)
                    };
                    if acceptance_closed {
                        write_control_response(
                            reader.get_mut(),
                            &ControlResponse::err("daemon is shutting down"),
                        )?;
                        continue;
                    }
                }
                let body_bytes = match usize::try_from(body_bytes) {
                    Ok(body_bytes) => body_bytes,
                    Err(error) => {
                        tracing::error!(
                            component = "daemon",
                            phase = "checkpoint_receive",
                            reason = "body_length_overflow",
                            declared_body_bytes = body_bytes,
                            %error,
                            "checkpoint body length does not fit this platform"
                        );
                        write_control_response(
                            reader.get_mut(),
                            &ControlResponse::err("checkpoint body length is too large"),
                        )?;
                        continue;
                    }
                };
                let reservation = match coordinator.checkpoint_ingress_quota.reserve(body_bytes) {
                    Ok(reservation) => reservation,
                    Err(error) => {
                        tracing::error!(
                            component = "daemon",
                            phase = "checkpoint_receive",
                            reason = error.reason,
                            requested_bytes = error.requested_bytes,
                            outstanding_requests = error.outstanding_requests,
                            outstanding_bytes = error.outstanding_bytes,
                            request_limit = error.request_limit,
                            byte_limit = error.byte_limit,
                            "checkpoint ingress quota exhausted"
                        );
                        write_control_response(
                            reader.get_mut(),
                            &ControlResponse::err(format!(
                                "checkpoint ingress busy: {}",
                                error.reason
                            )),
                        )?;
                        continue;
                    }
                };

                if let Err(error) = write_control_response(
                    reader.get_mut(),
                    &ControlResponse::ok(None, Some(json!({ "ready": true }))),
                ) {
                    tracing::error!(
                        component = "daemon",
                        phase = "checkpoint_receive",
                        reason = "ready_response_write_failed",
                        body_bytes,
                        %error,
                        "failed writing checkpoint ready response"
                    );
                    return Err(error);
                }

                if uses_idle_timeout {
                    reader
                        .get_mut()
                        .set_receive_timeout(Some(DAEMON_CONTROL_RECEIVE_TIMEOUT))?;
                }
                let body = match read_checkpoint_body(reader, reservation.body_bytes()) {
                    Ok(body) => body,
                    Err(error) => {
                        tracing::error!(
                            component = "daemon",
                            phase = "checkpoint_receive",
                            reason = "body_receive_failed",
                            body_bytes,
                            %error,
                            "failed receiving checkpoint body"
                        );
                        return Err(error);
                    }
                };
                if uses_idle_timeout {
                    reader
                        .get_mut()
                        .set_receive_timeout(Some(DAEMON_CONTROL_IDLE_TIMEOUT))?;
                }
                let Some(checkpoint_tx) = coordinator.checkpoint_ingress_tx.get().cloned() else {
                    tracing::error!(
                        component = "daemon",
                        phase = "checkpoint_receive",
                        reason = "ingress_worker_not_started",
                        body_bytes,
                        "checkpoint ingress worker is unavailable"
                    );
                    coordinator.request_shutdown();
                    write_control_response(
                        reader.get_mut(),
                        &ControlResponse::err("checkpoint ingress worker is unavailable"),
                    )?;
                    continue;
                };
                let permit = match checkpoint_tx.try_reserve_owned() {
                    Ok(permit) => permit,
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        tracing::error!(
                            component = "daemon",
                            phase = "checkpoint_receive",
                            reason = "ingress_queue_full",
                            body_bytes,
                            queue_limit = CHECKPOINT_INGRESS_REQUEST_LIMIT,
                            "checkpoint ingress queue is full despite quota reservation"
                        );
                        write_control_response(
                            reader.get_mut(),
                            &ControlResponse::err("checkpoint ingress busy: queue_full"),
                        )?;
                        continue;
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        tracing::error!(
                            component = "daemon",
                            phase = "checkpoint_receive",
                            reason = "ingress_channel_closed",
                            body_bytes,
                            "checkpoint ingress channel is closed"
                        );
                        coordinator.request_shutdown();
                        write_control_response(
                            reader.get_mut(),
                            &ControlResponse::err("checkpoint ingress worker is unavailable"),
                        )?;
                        continue;
                    }
                };
                let receipt_seq = {
                    let _sequencers =
                        coordinator
                            .family_sequencers_by_family
                            .lock()
                            .map_err(|_| {
                                GitAiError::Generic(
                                    "family sequencer map lock poisoned".to_string(),
                                )
                            })?;
                    if coordinator.accepting_checkpoints.load(Ordering::Acquire) {
                        let receipt_seq = coordinator
                            .next_checkpoint_receipt_seq
                            .fetch_add(1, Ordering::Relaxed)
                            as u64
                            + 1;
                        let received_at_ns = now_unix_nanos();
                        let trace_ingest_target =
                            coordinator.next_trace_ingest_seq.load(Ordering::Acquire) as u64;
                        coordinator
                            .unadmitted_checkpoints
                            .fetch_add(1, Ordering::Release);
                        permit.send(AcceptedCheckpoint {
                            receipt_seq,
                            received_at_ns,
                            trace_ingest_target,
                            body,
                            reservation,
                        });
                        Some(receipt_seq)
                    } else {
                        None
                    }
                };
                match receipt_seq {
                    Some(receipt_seq) => {
                        tracing::info!(
                            component = "daemon",
                            phase = "checkpoint_receive",
                            receipt_seq,
                            retained_bytes = body_bytes,
                            "checkpoint received into bounded ingress"
                        );
                        ControlResponse::ok(Some(receipt_seq), None)
                    }
                    None => ControlResponse::err("daemon is shutting down"),
                }
            }
            Ok(req) => {
                let is_shutdown = matches!(req, ControlRequest::Shutdown);
                let response = runtime_handle
                    .block_on(async { coordinator.handle_control_request(req).await });
                shutdown_after_response = is_shutdown && response.ok;
                response
            }
            Err(error) => {
                tracing::error!(
                    component = "daemon",
                    phase = "control_receive",
                    reason = "request_decode_failed",
                    %error,
                    "failed decoding daemon control request"
                );
                ControlResponse::err(format!("invalid control request: {error}"))
            }
        };
        let write_result = write_control_response(reader.get_mut(), &response);
        if let Err(error) = &write_result
            && let Some(receipt_seq) = response.seq
        {
            tracing::error!(
                component = "daemon",
                phase = "checkpoint_receive",
                reason = "receipt_ack_write_failed",
                receipt_seq,
                %error,
                "failed writing checkpoint receipt acknowledgement"
            );
        }
        if shutdown_after_response {
            coordinator.request_stop();
        }
        write_result?;
    }
    Ok(())
}

fn write_control_response<W: Write>(
    writer: &mut W,
    response: &ControlResponse,
) -> Result<(), GitAiError> {
    let raw = serde_json::to_string(response)?;
    writer.write_all(raw.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn trace_listener_loop_actor(
    trace_socket_path: PathBuf,
    coordinator: Arc<ActorDaemonCoordinator>,
) -> Result<(), GitAiError> {
    #[cfg(not(windows))]
    {
        remove_socket_if_exists(&trace_socket_path)?;
        let listener = ListenerOptions::new()
            .name(local_socket_name(&trace_socket_path)?)
            .create_sync()
            .map_err(|e| GitAiError::Generic(format!("failed binding trace socket: {}", e)))?;
        set_socket_owner_only(&trace_socket_path)?;
        for stream in listener.incoming() {
            if coordinator.is_shutting_down() {
                break;
            }
            let Ok(stream) = stream else {
                continue;
            };
            // Raise the receive buffer on each accepted connection. Unlike TCP,
            // a Unix-domain listener's SO_RCVBUF is not inherited by accepted
            // connections, so this per-connection call is what takes effect.
            if let Err(error) = set_trace_socket_recv_buffer(&stream) {
                tracing::debug!(%error, "trace connection recv buffer setup failed");
            }
            if let Err(error) = coordinator.trace_unidentified_connection_opened() {
                tracing::debug!(%error, "trace connection open bookkeeping error");
                continue;
            }
            if let Err(error) =
                stream.set_recv_timeout(Some(TRACE_CONNECTION_BOOTSTRAP_READ_TIMEOUT))
            {
                tracing::debug!(%error, "trace connection bootstrap timeout setup failed");
            }
            let mut reader = BufReader::new(stream);
            let mut observed_roots = std::collections::BTreeSet::new();
            match bootstrap_trace_connection_actor_reader(
                &mut reader,
                coordinator.clone(),
                &mut observed_roots,
            ) {
                Ok(TraceConnectionBootstrap::Eof) => {
                    if let Err(error) =
                        finalize_trace_connection_roots(coordinator.clone(), observed_roots)
                    {
                        tracing::debug!(
                            %error,
                            "trace connection close bookkeeping error"
                        );
                    }
                    continue;
                }
                Ok(TraceConnectionBootstrap::Stop) => {
                    if let Err(error) =
                        finalize_trace_connection_roots(coordinator.clone(), observed_roots)
                    {
                        tracing::debug!(
                            %error,
                            "trace connection close bookkeeping error"
                        );
                    }
                    continue;
                }
                Ok(TraceConnectionBootstrap::Continue) => {}
                Err(error) => {
                    tracing::debug!(%error, "trace connection bootstrap error");
                    if let Err(error) =
                        finalize_trace_connection_roots(coordinator.clone(), observed_roots)
                    {
                        tracing::debug!(
                            %error,
                            "trace connection close bookkeeping error"
                        );
                    }
                    continue;
                }
            }
            if let Err(error) = reader.get_ref().set_recv_timeout(None) {
                tracing::debug!(%error, "trace connection bootstrap timeout clear failed");
            }
            #[cfg(feature = "test-support")]
            if let Ok(raw_delay_ms) =
                std::env::var("GIT_AI_TEST_TRACE_LISTENER_WORKER_SPAWN_DELAY_MS")
                && let Ok(delay_ms) = raw_delay_ms.parse::<u64>()
                && delay_ms > 0
            {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            let coord = coordinator.clone();
            let observed_roots_on_spawn_failure = observed_roots.clone();
            if std::thread::Builder::new()
                .spawn(move || {
                    if let Err(e) =
                        handle_trace_connection_actor_reader(reader, coord, observed_roots)
                    {
                        tracing::debug!(%e, "trace connection error");
                    }
                })
                .is_err()
            {
                tracing::error!("trace listener: failed to spawn handler thread");
                if let Err(error) = finalize_trace_connection_roots(
                    coordinator.clone(),
                    observed_roots_on_spawn_failure,
                ) {
                    tracing::debug!(
                        %error,
                        "trace connection close bookkeeping error"
                    );
                }
                break;
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    {
        let mut workers = Vec::new();
        let worker_count = windows_trace_pipe_worker_count();
        let first_connecting = windows_pipe_connecting_server(&trace_socket_path, true)?;
        {
            let path = trace_socket_path.clone();
            let coord = coordinator.clone();
            workers.push(std::thread::spawn(move || {
                let result = windows_trace_pipe_worker_loop(path, first_connecting, coord.clone());
                if let Err(error) = &result {
                    tracing::error!(%error, "trace worker error");
                    coord.request_shutdown();
                }
                result
            }));
        }
        for _ in 1..worker_count {
            let path = trace_socket_path.clone();
            let coord = coordinator.clone();
            let connecting = windows_pipe_connecting_server(&path, false)?;
            workers.push(std::thread::spawn(move || {
                let result = windows_trace_pipe_worker_loop(path, connecting, coord.clone());
                if let Err(error) = &result {
                    tracing::error!(%error, "trace worker error");
                    coord.request_shutdown();
                }
                result
            }));
        }

        while !coordinator.is_shutting_down() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        wake_windows_pipe_workers(&trace_socket_path, worker_count);

        for worker in workers {
            let result = worker
                .join()
                .map_err(|_| GitAiError::Generic("daemon trace worker panicked".to_string()))?;
            result?;
        }

        Ok(())
    }
}

#[cfg(windows)]
fn windows_trace_pipe_worker_loop(
    trace_socket_path: PathBuf,
    mut connecting: WindowsConnectingServer,
    coordinator: Arc<ActorDaemonCoordinator>,
) -> Result<(), GitAiError> {
    loop {
        let server = connecting.wait().map_err(|e| {
            GitAiError::Generic(format!(
                "failed accepting trace pipe {}: {}",
                trace_socket_path.display(),
                e
            ))
        })?;

        if coordinator.is_shutting_down() {
            let _ = server.disconnect();
            break;
        }

        connecting = windows_pipe_connecting_server(&trace_socket_path, false)?;

        let coord = coordinator.clone();
        std::thread::Builder::new()
            .spawn(move || {
                handle_windows_trace_pipe_connection(server, coord);
            })
            .map_err(|e| {
                GitAiError::Generic(format!(
                    "failed spawning trace pipe handler for {}: {}",
                    trace_socket_path.display(),
                    e
                ))
            })?;
    }

    Ok(())
}

#[cfg(windows)]
fn handle_windows_trace_pipe_connection(
    mut server: WindowsPipeServer,
    coordinator: Arc<ActorDaemonCoordinator>,
) {
    if let Err(e) = coordinator.trace_unidentified_connection_opened() {
        tracing::debug!(%e, "trace connection open bookkeeping error");
        return;
    }
    let reader = BufReader::new(&mut server);
    if let Err(e) =
        handle_trace_connection_actor_reader(reader, coordinator, std::collections::BTreeSet::new())
    {
        tracing::debug!(%e, "trace connection error");
    }
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn handle_trace_connection_actor(
    stream: LocalSocketStream,
    coordinator: Arc<ActorDaemonCoordinator>,
) -> Result<(), GitAiError> {
    coordinator.trace_unidentified_connection_opened()?;
    let reader = BufReader::new(stream);
    handle_trace_connection_actor_reader(reader, coordinator, std::collections::BTreeSet::new())
}

#[cfg(not(windows))]
enum TraceConnectionBootstrap {
    Continue,
    Stop,
    Eof,
}

struct TraceLineOutcome {
    continue_reading: bool,
    #[cfg(not(windows))]
    bootstrap_complete: bool,
}

#[cfg(not(windows))]
const TRACE_CONNECTION_BOOTSTRAP_MAX_LINES: usize = 8;

#[cfg(not(windows))]
fn bootstrap_trace_connection_actor_reader<R: Read>(
    reader: &mut BufReader<R>,
    coordinator: Arc<ActorDaemonCoordinator>,
    observed_roots: &mut std::collections::BTreeSet<String>,
) -> Result<TraceConnectionBootstrap, GitAiError> {
    for _ in 0..TRACE_CONNECTION_BOOTSTRAP_MAX_LINES {
        let line = match read_json_line(reader) {
            Ok(Some(line)) => line,
            Ok(None) => return Ok(TraceConnectionBootstrap::Eof),
            Err(error) if trace_bootstrap_read_timed_out(&error) => {
                return Ok(TraceConnectionBootstrap::Continue);
            }
            Err(error) => return Err(error),
        };
        let Some(outcome) =
            process_trace_connection_line(&line, coordinator.clone(), observed_roots)?
        else {
            continue;
        };
        if !outcome.continue_reading {
            return Ok(TraceConnectionBootstrap::Stop);
        }
        if outcome.bootstrap_complete {
            return Ok(TraceConnectionBootstrap::Continue);
        }
    }
    Ok(TraceConnectionBootstrap::Continue)
}

#[cfg(not(windows))]
fn trace_bootstrap_read_timed_out(error: &GitAiError) -> bool {
    matches!(
        error,
        GitAiError::IoError(io_error)
            if matches!(
                io_error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
    )
}

fn handle_trace_connection_actor_reader<R: Read>(
    mut reader: BufReader<R>,
    coordinator: Arc<ActorDaemonCoordinator>,
    mut observed_roots: std::collections::BTreeSet<String>,
) -> Result<(), GitAiError> {
    while let Some(line) = read_json_line(&mut reader)? {
        if process_trace_connection_line(&line, coordinator.clone(), &mut observed_roots)?
            .is_some_and(|outcome| !outcome.continue_reading)
        {
            break;
        }
    }

    finalize_trace_connection_roots(coordinator, observed_roots)
}

fn process_trace_connection_line(
    line: &str,
    coordinator: Arc<ActorDaemonCoordinator>,
    observed_roots: &mut std::collections::BTreeSet<String>,
) -> Result<Option<TraceLineOutcome>, GitAiError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut parsed: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    #[cfg(not(windows))]
    let event = parsed
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    #[cfg(not(windows))]
    let mut bootstrap_complete = false;
    if let Some(sid) = parsed.get("sid").and_then(Value::as_str) {
        let was_unidentified = observed_roots.is_empty();
        let root_sid = trace_root_sid(sid).to_string();
        // `start` carries argv but not the worktree. Keep bootstrapping on the
        // listener thread until the root `def_repo` event has been processed;
        // that is the first point where trace augmentation can capture reflog
        // start offsets with a concrete worktree.
        #[cfg(not(windows))]
        if event == "def_repo" && sid == root_sid {
            bootstrap_complete = true;
        }
        if observed_roots.insert(root_sid.clone()) {
            let _ = coordinator.trace_root_connection_opened(&root_sid);
        }
        if was_unidentified {
            coordinator.trace_unidentified_connection_identified_or_closed()?;
        }
    }
    // Only enqueue payloads for mutating commands.  Read-only invocations
    // (status, diff, stash list, worktree list, …) are handled inline by
    // prepare_trace_payload_for_ingest and must not enter the serial ingest
    // queue — doing so causes the >1-minute backlog seen with IDEs that
    // issue dozens of read-only git commands per second.
    let continue_reading = !(coordinator.prepare_trace_payload_for_ingest(&mut parsed)
        && coordinator.enqueue_trace_payload(parsed).is_err());
    Ok(Some(TraceLineOutcome {
        continue_reading,
        #[cfg(not(windows))]
        bootstrap_complete,
    }))
}

fn finalize_trace_connection_roots(
    coordinator: Arc<ActorDaemonCoordinator>,
    observed_roots: std::collections::BTreeSet<String>,
) -> Result<(), GitAiError> {
    if observed_roots.is_empty() {
        coordinator.trace_unidentified_connection_identified_or_closed()?;
        return Ok(());
    }

    let roots = observed_roots.into_iter().collect::<Vec<_>>();
    let close_marker_roots = coordinator.record_trace_connection_close(&roots)?;
    coordinator.enqueue_trace_connection_close_markers(close_marker_roots)
}

/// Git environment variables that must not leak into the daemon process.
///
/// The daemon is a long-lived, repository-agnostic process that serves requests
/// for many different repositories. Environment variables like `GIT_DIR` and
/// `GIT_WORK_TREE` pin git operations to a single repository and override the
/// `-C <path>` flag that the daemon uses to target each repository individually.
///
/// When a daemon is spawned by a git wrapper invocation (e.g. `git add`), the
/// parent process may have these variables set by git itself (hook context) or
/// by test harnesses. Clearing them at daemon startup prevents incorrect
/// repository resolution that manifests as `fatal: not a git repository: '/dev/null'`.
///
/// This list is used in two places:
/// - `spawn_daemon_run_detached` strips them from the child process via `env_remove`.
/// - `sanitize_git_env_for_daemon` clears them from the current process at daemon startup
///   as a belt-and-suspenders defence (the daemon may be launched by another mechanism).
pub const GIT_ENV_VARS_TO_SANITIZE: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_CEILING_DIRECTORIES",
    "GIT_QUARANTINE_PATH",
    "GIT_NAMESPACE",
];

fn sanitize_git_env_for_daemon() {
    for var in GIT_ENV_VARS_TO_SANITIZE {
        // SAFETY: daemon startup is single-threaded at this point -- the tokio
        // runtime is not yet running and no other threads exist.
        unsafe {
            std::env::remove_var(var);
        }
    }
}

fn disable_trace2_for_daemon_process() {
    // The daemon executes internal git commands while processing events and control requests.
    // If trace2.eventTarget points at this daemon socket globally, those internal git
    // commands can recursively feed trace2 events back into the daemon and starve progress.
    // Force-disable trace2 emission for the daemon process and all of its child git commands.
    unsafe {
        std::env::set_var("GIT_TRACE2_EVENT", "0");
    }
}

/// How often the daemon wakes up to evaluate whether an update check is due.
const DAEMON_UPDATE_CHECK_INTERVAL_SECS: u64 = 3600;

/// Maximum daemon uptime before a proactive restart (24.5 hours).
/// Deliberately offset from the 24h update-check cadence so the uptime restart
/// never races with an update-triggered shutdown.
const DAEMON_MAX_UPTIME_SECS: u64 = 24 * 3600 + 30 * 60;

/// Returns the update check interval, respecting an env var override for testing.
fn daemon_update_check_interval() -> u64 {
    std::env::var("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DAEMON_UPDATE_CHECK_INTERVAL_SECS)
}

/// Returns the maximum uptime in nanoseconds, respecting an env var override for testing.
fn daemon_max_uptime_ns() -> u128 {
    let secs = std::env::var("GIT_AI_DAEMON_MAX_UPTIME_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DAEMON_MAX_UPTIME_SECS);
    secs as u128 * 1_000_000_000
}

const DAEMON_SOCKET_HEALTH_CHECK_SECS: u64 = 30;

fn daemon_socket_health_check_interval() -> u64 {
    std::env::var("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DAEMON_SOCKET_HEALTH_CHECK_SECS)
}

/// Spawn a detached `git-ai bg restart --hard` process that will reap the
/// current (zombie) daemon and start a fresh one.  The child inherits the
/// daemon env vars (GIT_AI_DAEMON_HOME, etc.) so it targets the same
/// instance.  Returns Ok if the process was spawned; the caller should
/// still request_shutdown so the current daemon exits promptly.
fn spawn_self_restart() -> Result<(), String> {
    let exe = crate::utils::current_git_ai_exe().map_err(|e| e.to_string())?;
    tracing::info!(?exe, "spawning detached restart process");

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["bg", "restart", "--hard"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    for var in GIT_ENV_VARS_TO_SANITIZE {
        cmd.env_remove(var);
    }
    cmd.env_remove("GIT_AI");

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }

    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to spawn restart process: {}", e))
}

const DAEMON_MIN_UPTIME_FOR_SELF_RESTART_SECS: u64 = 60;

fn daemon_min_uptime_for_self_restart() -> u64 {
    std::env::var("GIT_AI_DAEMON_MIN_UPTIME_FOR_RESTART_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DAEMON_MIN_UPTIME_FOR_SELF_RESTART_SECS)
}

/// Background loop that verifies the daemon's sockets are reachable. The
/// control probe performs a bounded Ping request so it also proves a handler
/// can receive and respond, while the write-only trace2 socket is verified by
/// connecting to its listener. If either probe fails (deleted file, stale
/// socket, hung listener), the daemon spawns a detached restart process and
/// shuts down.
///
/// To prevent restart loops when the underlying issue is systemic (e.g.
/// filesystem permissions, broken paths), the daemon only self-restarts if
/// it has been up for at least 60 seconds.  If sockets fail before that,
/// it shuts down without restart — the next wrapper invocation will attempt
/// to start a fresh daemon.
fn daemon_socket_health_check_loop(
    coordinator: Arc<ActorDaemonCoordinator>,
    control_socket_path: PathBuf,
    trace_socket_path: PathBuf,
) {
    let started = std::time::Instant::now();
    let interval = daemon_socket_health_check_interval().max(1);
    tracing::info!(
        interval,
        control = %control_socket_path.display(),
        trace = %trace_socket_path.display(),
        "socket health check started"
    );

    loop {
        {
            let guard = coordinator
                .shutdown_condvar_mutex
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if coordinator.is_shutting_down() {
                return;
            }
            let _ = coordinator
                .shutdown_condvar
                .wait_timeout(guard, std::time::Duration::from_secs(interval));
        }

        if coordinator.is_shutting_down() {
            return;
        }

        let control_ok = send_control_request_with_timeouts(
            &control_socket_path,
            &ControlRequest::Ping,
            DAEMON_SOCKET_PROBE_TIMEOUT,
            DAEMON_CONTROL_RESPONSE_TIMEOUT,
        )
        .and_then(|response| {
            if response.ok {
                Ok(())
            } else {
                Err(GitAiError::Generic(response.error.unwrap_or_else(|| {
                    "daemon Ping returned an error".to_string()
                })))
            }
        });
        let trace_ok =
            local_socket_connects_with_timeout(&trace_socket_path, DAEMON_SOCKET_PROBE_TIMEOUT);

        if control_ok.is_err() || trace_ok.is_err() {
            let (outstanding_checkpoints, retained_checkpoint_bytes) =
                coordinator.outstanding_checkpoint_state();
            if outstanding_checkpoints > 0 {
                tracing::error!(
                    component = "daemon",
                    phase = "socket_health",
                    reason = "restart_deferred_for_checkpoints",
                    outstanding_checkpoints,
                    retained_checkpoint_bytes,
                    control = %control_ok.err().map(|e| e.to_string()).unwrap_or_else(|| "ok".into()),
                    trace = %trace_ok.err().map(|e| e.to_string()).unwrap_or_else(|| "ok".into()),
                    "socket health restart deferred while accepted checkpoints remain"
                );
                continue;
            }
            let uptime = started.elapsed();
            let min_uptime = std::time::Duration::from_secs(daemon_min_uptime_for_self_restart());

            if uptime >= min_uptime {
                tracing::warn!(
                    control = %control_ok.err().map(|e| e.to_string()).unwrap_or_else(|| "ok".into()),
                    trace = %trace_ok.err().map(|e| e.to_string()).unwrap_or_else(|| "ok".into()),
                    "socket health check failed, spawning restart and shutting down"
                );
                if let Err(e) = spawn_self_restart() {
                    tracing::error!("failed to spawn self-restart: {}", e);
                }
            } else {
                tracing::warn!(
                    control = %control_ok.err().map(|e| e.to_string()).unwrap_or_else(|| "ok".into()),
                    trace = %trace_ok.err().map(|e| e.to_string()).unwrap_or_else(|| "ok".into()),
                    uptime_secs = uptime.as_secs(),
                    "socket health check failed within minimum uptime, shutting down without restart"
                );
            }
            coordinator.request_shutdown();
            return;
        }
    }
}

/// Background loop that periodically checks for available updates.
///
/// Sleeps in short increments so it can exit promptly when the coordinator
/// signals shutdown.  When an update is detected, it requests a graceful
/// shutdown so the daemon can self-update after draining in-flight work.
fn daemon_update_check_loop(coordinator: Arc<ActorDaemonCoordinator>, started_at_ns: u128) {
    use crate::commands::upgrade::{DaemonUpdateCheckResult, check_for_update_available};

    let interval = daemon_update_check_interval().max(1);

    loop {
        {
            let guard = coordinator
                .shutdown_condvar_mutex
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if coordinator.is_shutting_down() {
                return;
            }
            let _ = coordinator
                .shutdown_condvar
                .wait_timeout(guard, std::time::Duration::from_secs(interval));
        }

        if coordinator.is_shutting_down() {
            return;
        }

        coordinator.gc_stale_family_state();

        match check_for_update_available() {
            Ok(DaemonUpdateCheckResult::UpdateReady) => {
                let (outstanding_checkpoints, retained_checkpoint_bytes) =
                    coordinator.outstanding_checkpoint_state();
                if outstanding_checkpoints > 0 {
                    tracing::info!(
                        outstanding_checkpoints,
                        retained_checkpoint_bytes,
                        "update restart deferred while accepted checkpoints remain"
                    );
                } else {
                    tracing::info!("update check: newer version available, requesting shutdown");
                    coordinator.request_restart_after_update();
                    return;
                }
            }
            Ok(DaemonUpdateCheckResult::NoUpdate) => {
                tracing::info!("update check: no update needed");
            }
            Err(err) => {
                tracing::warn!(%err, "update check failed");
            }
        }

        let uptime_ns = now_unix_nanos().saturating_sub(started_at_ns);
        if uptime_ns >= daemon_max_uptime_ns() {
            let (outstanding_checkpoints, retained_checkpoint_bytes) =
                coordinator.outstanding_checkpoint_state();
            if outstanding_checkpoints > 0 {
                tracing::info!(
                    outstanding_checkpoints,
                    retained_checkpoint_bytes,
                    "uptime restart deferred while accepted checkpoints remain"
                );
            } else {
                tracing::info!("uptime exceeded max, requesting restart");
                coordinator.request_restart();
                return;
            }
        }
    }
}

/// After the daemon has fully shut down, attempt to install any pending update.
///
/// On Unix the installer atomically replaces the binary via `mv`; on Windows
/// the installer is spawned as a detached process that polls until the exe is
/// unlocked.
pub(crate) fn daemon_run_pending_self_update() -> DaemonSelfUpdateOutcome {
    use crate::commands::upgrade::{
        DaemonUpdateCheckResult, check_and_install_update_if_available,
    };

    match check_and_install_update_if_available() {
        Ok(DaemonUpdateCheckResult::UpdateReady) => {
            tracing::info!("self-update: installation completed successfully");
            DaemonSelfUpdateOutcome::Installed
        }
        Ok(DaemonUpdateCheckResult::NoUpdate) => {
            tracing::info!("self-update: no update to install");
            DaemonSelfUpdateOutcome::NoUpdate
        }
        Err(err) => {
            tracing::warn!(%err, "self-update: installation failed");
            crate::commands::upgrade::clear_cached_update_state();
            DaemonSelfUpdateOutcome::Failed
        }
    }
}

pub(crate) async fn run_daemon(config: DaemonConfig) -> Result<DaemonExitAction, GitAiError> {
    sanitize_git_env_for_daemon();
    disable_trace2_for_daemon_process();
    config.ensure_parent_dirs()?;
    remove_stale_daemon_files(&config);
    let _lock = DaemonLock::acquire(&config.lock_path)?;
    let _active_guard = DaemonProcessActiveGuard::enter();
    write_pid_metadata(&config)?;

    // Initialize tracing subscriber before log file redirect so the fmt layer
    // captures stderr (fd 2). After dup2, writes go to the daemon log file.
    {
        use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

        let env_filter = if std::env::var("GIT_AI_DEBUG").as_deref() == Ok("1") {
            EnvFilter::new("debug")
        } else {
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
        };

        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(false)
                    .with_thread_ids(false)
                    .with_ansi(false)
                    .with_writer(std::io::stderr),
            )
            .with(crate::daemon::sentry_layer::SentryLayer)
            .with(crate::daemon::daemon_log_layer::DaemonLogUploadLayer)
            .init();
    }

    let _log_guard = maybe_setup_daemon_log_file(&config);

    tracing::info!(
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "daemon started"
    );

    remove_socket_if_exists(&config.trace_socket_path)?;
    remove_socket_if_exists(&config.control_socket_path)?;

    let mut coordinator_inner = ActorDaemonCoordinator::new();

    // Spawn the telemetry worker inside the daemon's tokio runtime.
    let telemetry_handle = crate::daemon::telemetry_worker::spawn_telemetry_worker();
    crate::daemon::telemetry_worker::set_daemon_internal_telemetry(telemetry_handle.clone());
    coordinator_inner.telemetry_worker = Some(telemetry_handle.clone());

    // Spawn the transcript worker BEFORE wrapping coordinator in Arc
    if config::Config::get()
        .get_feature_flags()
        .transcript_streaming
    {
        // Named "transcripts-db" for backwards compatibility with existing installations.
        // TODO: rename to "streams-db" with a migration that moves the file.
        let streams_db_path = config.internal_dir.join("transcripts-db");
        match crate::streams::db::StreamsDatabase::open(&streams_db_path) {
            Ok(streams_db) => {
                let streams_db = std::sync::Arc::new(streams_db);
                let shutdown_notify = Arc::new(tokio::sync::Notify::new());
                let transcript_handle = crate::daemon::stream_worker::spawn_stream_worker(
                    streams_db.clone(),
                    telemetry_handle.clone(),
                    shutdown_notify.clone(),
                );
                coordinator_inner.streams_db = Some(streams_db);
                coordinator_inner.stream_worker = Some(transcript_handle);
                let _ = coordinator_inner
                    .transcript_shutdown_notify
                    .set(shutdown_notify);
                tracing::info!("transcript worker spawned");
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to open transcripts database, transcript worker not started");
            }
        }
    }

    let coordinator = Arc::new(coordinator_inner);
    coordinator.start_trace_ingest_worker()?;
    coordinator.start_checkpoint_ingress_worker()?;
    if let Some(limit_mb) = config::Config::get().daemon_memory_limit_mb()
        && let Some(limit_bytes) = limit_mb.checked_mul(config::MEBIBYTE_BYTES)
    {
        memory_watchdog::start(Arc::clone(&coordinator), limit_bytes)?;
    }
    let rt_handle = tokio::runtime::Handle::current();
    let control_socket_path = config.control_socket_path.clone();
    let trace_socket_path = config.trace_socket_path.clone();

    let control_coord = coordinator.clone();
    let control_shutdown_coord = coordinator.clone();
    let control_handle = rt_handle.clone();
    let control_thread = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            control_listener_loop_actor(control_socket_path, control_coord, control_handle)
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(%e, "control listener exited with error");
            }
            Err(_) => {
                tracing::error!("control listener panicked");
            }
        }
        // Always request shutdown so the daemon doesn't stay half-alive.
        control_shutdown_coord.request_shutdown();
    });

    let trace_coord = coordinator.clone();
    let trace_shutdown_coord = coordinator.clone();
    let trace_thread = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            trace_listener_loop_actor(trace_socket_path, trace_coord)
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(%e, "trace listener exited with error");
            }
            Err(_) => {
                tracing::error!("trace listener panicked");
            }
        }
        // Always request shutdown so the daemon doesn't stay half-alive.
        trace_shutdown_coord.request_shutdown();
    });

    let started_at_ns = now_unix_nanos();
    let update_coord = coordinator.clone();
    let update_thread = std::thread::spawn(move || {
        daemon_update_check_loop(update_coord, started_at_ns);
    });

    let health_coord = coordinator.clone();
    let health_control = config.control_socket_path.clone();
    let health_trace = config.trace_socket_path.clone();
    let health_thread = std::thread::spawn(move || {
        daemon_socket_health_check_loop(health_coord, health_control, health_trace);
    });

    coordinator.wait_for_shutdown().await;

    // Best-effort wake listeners to allow clean process exit.
    // Connect to each socket to unblock `accept()`.  If the socket files
    // were deleted (which is exactly what the health-check detects), the
    // connection will fail — fall back to a timed join so the process still
    // exits instead of hanging forever.
    let _ = local_socket_connects_with_timeout(
        &config.control_socket_path,
        DAEMON_SOCKET_PROBE_TIMEOUT,
    );
    let _ =
        local_socket_connects_with_timeout(&config.trace_socket_path, DAEMON_SOCKET_PROBE_TIMEOUT);

    let join_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    for (name, thread) in [
        ("control", control_thread),
        ("trace", trace_thread),
        ("update", update_thread),
        ("health", health_thread),
    ] {
        let remaining = join_deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            tracing::debug!("skipping join for {} thread (deadline exceeded)", name);
            continue;
        }
        let handle = std::thread::spawn(move || {
            let _ = thread.join();
        });
        let poll_until =
            std::time::Instant::now() + remaining.min(std::time::Duration::from_millis(500));
        loop {
            if handle.is_finished() {
                let _ = handle.join();
                break;
            }
            if std::time::Instant::now() >= poll_until {
                tracing::debug!("{} thread did not join in time, proceeding", name);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    remove_socket_if_exists(&config.trace_socket_path)?;
    remove_socket_if_exists(&config.control_socket_path)?;
    remove_pid_metadata(&config)?;

    let action = coordinator.shutdown_action();
    tracing::info!(?action, "daemon shutdown complete");

    Ok(action)
}

fn checkpoint_control_timeout_uses_ci_or_test_budget() -> bool {
    std::env::var_os("GIT_AI_TEST_DB_PATH").is_some()
        || std::env::var_os("GITAI_TEST_DB_PATH").is_some()
        || std::env::var_os("CI").is_some()
}

fn checkpoint_control_response_timeout(
    request: &ControlRequest,
    use_ci_or_test_budget: bool,
) -> Duration {
    match request {
        // Queued checkpoint requests can block behind trace-ingest ordering. In
        // CI/test we allow the longer budget so replay-heavy daemon tests don't
        // tear down captured state mid-request. Product mode keeps the short
        // control timeout so a wedged prior Git root fails the checkpoint rather
        // than making the caller wait indefinitely.
        ControlRequest::CheckpointRun { .. } if use_ci_or_test_budget => {
            DAEMON_CHECKPOINT_RESPONSE_TIMEOUT
        }
        ControlRequest::CheckpointRun { .. } => DAEMON_CONTROL_RESPONSE_TIMEOUT,
        ControlRequest::SyncFamily { .. } if use_ci_or_test_budget => {
            DAEMON_CHECKPOINT_RESPONSE_TIMEOUT
        }
        ControlRequest::SyncFamily { .. } => DAEMON_CHECKPOINT_RESPONSE_TIMEOUT,
        ControlRequest::SnapshotWatermarks { .. } => Duration::from_millis(500),
        // Await blocks until the requested timeout is reached; give the daemon
        // a small grace period over the requested limit so the caller sees a
        // response rather than a client-side socket timeout.
        ControlRequest::Await { timeout_secs } => {
            Duration::from_secs(timeout_secs.saturating_add(5))
        }
        ControlRequest::Shutdown => DAEMON_CHECKPOINT_RESPONSE_TIMEOUT,
        _ => DAEMON_CONTROL_RESPONSE_TIMEOUT,
    }
}

fn control_request_response_timeout(request: &ControlRequest) -> Duration {
    checkpoint_control_response_timeout(
        request,
        checkpoint_control_timeout_uses_ci_or_test_budget(),
    )
}

#[cfg(not(windows))]
fn local_socket_name<'a>(socket_path: &'a Path) -> Result<Name<'a>, GitAiError> {
    socket_path
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| GitAiError::Generic(format!("invalid daemon socket path: {}", e)))
}

/// Target trace socket receive buffer size in bytes.
///
/// Defaults to `TRACE_SOCKET_RECV_BUFFER_BYTES` and can be overridden via
/// `GIT_AI_TRACE_SOCKET_RECV_BUFFER_BYTES` to ramp toward 1 MiB (or larger)
/// without a code change. A value of `0` disables the buffer bump entirely.
#[cfg(not(windows))]
fn trace_socket_recv_buffer_bytes() -> usize {
    std::env::var("GIT_AI_TRACE_SOCKET_RECV_BUFFER_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(TRACE_SOCKET_RECV_BUFFER_BYTES)
}

#[cfg(not(windows))]
fn set_trace_socket_recv_buffer(stream: &LocalSocketStream) -> io::Result<()> {
    match stream {
        LocalSocketStream::UdSocket(stream) => {
            set_socket_recv_buffer(stream, trace_socket_recv_buffer_bytes())
        }
    }
}

/// Raise a socket's kernel receive buffer to `bytes` via `SO_RCVBUF`.
///
/// A `bytes` of `0` is a no-op (buffer bump disabled). The kernel may clamp the
/// request to `net.core.rmem_max` on Linux, so the effective value can be lower
/// than requested; that is fine -- this only ever raises capacity.
#[cfg(not(windows))]
fn set_socket_recv_buffer<S: AsFd>(socket: &S, bytes: usize) -> io::Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    let value = bytes.min(libc::c_int::MAX as usize) as libc::c_int;
    let result = unsafe {
        libc::setsockopt(
            socket.as_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &value as *const libc::c_int as *const libc::c_void,
            std::mem::size_of_val(&value) as libc::socklen_t,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(all(test, not(windows)))]
fn socket_recv_buffer<S: AsFd>(socket: &S) -> io::Result<usize> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of_val(&value) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            socket.as_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &mut value as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value.max(0) as usize)
    }
}

pub fn open_local_socket_stream_with_timeout(
    socket_path: &Path,
    timeout: Duration,
) -> Result<DaemonClientStream, GitAiError> {
    #[cfg(windows)]
    {
        let stream = open_windows_named_pipe_client_with_timeout(socket_path, timeout)?;
        Ok(DaemonClientStream::WindowsPipe(stream))
    }

    #[cfg(not(windows))]
    {
        ConnectOptions::new()
            .name(local_socket_name(socket_path)?)
            .wait_mode(ConnectWaitMode::Timeout(timeout))
            .connect_sync()
            .map_err(|e| {
                GitAiError::Generic(format!(
                    "timed out after {:?} connecting daemon socket {}: {}",
                    timeout,
                    socket_path.display(),
                    e
                ))
            })
    }
}

#[cfg(windows)]
fn open_windows_named_pipe_client_with_timeout(
    socket_path: &Path,
    timeout: Duration,
) -> Result<WindowsPipeClient, GitAiError> {
    let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
    WindowsPipeClient::connect_ms(socket_path.as_os_str(), timeout_ms).map_err(|e| {
        GitAiError::Generic(format!(
            "timed out after {:?} connecting daemon socket {}: {}",
            timeout,
            socket_path.display(),
            e
        ))
    })
}

fn set_daemon_client_stream_timeouts(
    stream: &mut DaemonClientStream,
    socket_path: &Path,
    timeout: Duration,
) -> Result<(), GitAiError> {
    #[cfg(windows)]
    {
        let _ = socket_path;
        match stream {
            DaemonClientStream::WindowsPipe(pipe) => {
                pipe.set_read_timeout(Some(timeout));
                pipe.set_write_timeout(Some(timeout));
                Ok(())
            }
        }
    }

    #[cfg(not(windows))]
    {
        stream.set_recv_timeout(Some(timeout)).map_err(|e| {
            GitAiError::Generic(format!(
                "failed to set daemon socket {} recv timeout: {}",
                socket_path.display(),
                e
            ))
        })?;
        stream.set_send_timeout(Some(timeout)).map_err(|e| {
            GitAiError::Generic(format!(
                "failed to set daemon socket {} send timeout: {}",
                socket_path.display(),
                e
            ))
        })
    }
}

fn write_all_daemon_client_stream(
    stream: &mut DaemonClientStream,
    socket_path: &Path,
    payload: &[u8],
) -> Result<(), GitAiError> {
    stream.write_all(payload).map_err(|e| {
        GitAiError::Generic(format!(
            "failed writing daemon request to {}: {}",
            socket_path.display(),
            e
        ))
    })?;
    stream.flush().map_err(|e| {
        GitAiError::Generic(format!(
            "failed flushing daemon request to {}: {}",
            socket_path.display(),
            e
        ))
    })?;
    Ok(())
}

fn read_daemon_client_line(
    reader: &mut BufReader<DaemonClientStream>,
    socket_path: &Path,
    response_timeout: Duration,
) -> Result<String, GitAiError> {
    let mut line = String::new();
    let deadline = std::time::Instant::now() + response_timeout;
    loop {
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err(GitAiError::Generic(format!(
                    "daemon socket {} closed without a response",
                    socket_path.display()
                )));
            }
            Ok(_) => return Ok(line),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if std::time::Instant::now() >= deadline {
                    return Err(GitAiError::Generic(format!(
                        "timed out after {:?} reading daemon response from {}",
                        response_timeout,
                        socket_path.display()
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => {
                return Err(GitAiError::Generic(format!(
                    "failed reading daemon response from {}: {}",
                    socket_path.display(),
                    error
                )));
            }
        }
    }
}

#[cfg(windows)]
fn send_control_request_with_timeouts_windows(
    socket_path: &Path,
    request: &ControlRequest,
    connect_timeout: Duration,
    response_timeout: Duration,
) -> Result<ControlResponse, GitAiError> {
    let mut stream = open_local_socket_stream_with_timeout(socket_path, connect_timeout)?;
    set_daemon_client_stream_timeouts(&mut stream, socket_path, response_timeout)?;
    let mut body = serde_json::to_vec(request)?;
    body.push(b'\n');
    write_all_daemon_client_stream(&mut stream, socket_path, &body)?;

    let mut response_reader = BufReader::new(stream);
    let line = read_daemon_client_line(&mut response_reader, socket_path, response_timeout)?;
    if line.trim().is_empty() {
        return Err(GitAiError::Generic(
            "empty daemon control response".to_string(),
        ));
    }
    serde_json::from_str(line.trim()).map_err(GitAiError::from)
}

#[cfg(not(windows))]
fn send_control_request_with_timeouts_unix(
    socket_path: &Path,
    request: &ControlRequest,
    connect_timeout: Duration,
    response_timeout: Duration,
) -> Result<ControlResponse, GitAiError> {
    let mut stream = open_local_socket_stream_with_timeout(socket_path, connect_timeout)?;
    set_daemon_client_stream_timeouts(&mut stream, socket_path, response_timeout)?;
    let mut body = serde_json::to_vec(request)?;
    body.push(b'\n');
    write_all_daemon_client_stream(&mut stream, socket_path, &body)?;

    let mut response_reader = BufReader::new(stream);
    let line = read_daemon_client_line(&mut response_reader, socket_path, response_timeout)?;
    if line.trim().is_empty() {
        return Err(GitAiError::Generic(
            "empty daemon control response".to_string(),
        ));
    }
    serde_json::from_str(line.trim()).map_err(GitAiError::from)
}

pub fn local_socket_connects_with_timeout(
    socket_path: &Path,
    timeout: Duration,
) -> Result<(), GitAiError> {
    let _stream = open_local_socket_stream_with_timeout(socket_path, timeout)?;
    Ok(())
}

pub fn send_control_request_with_timeout(
    socket_path: &Path,
    request: &ControlRequest,
    timeout: Duration,
) -> Result<ControlResponse, GitAiError> {
    send_control_request_with_timeouts(socket_path, request, timeout, timeout)
}

fn send_control_request_with_timeouts(
    socket_path: &Path,
    request: &ControlRequest,
    connect_timeout: Duration,
    response_timeout: Duration,
) -> Result<ControlResponse, GitAiError> {
    #[cfg(windows)]
    {
        send_control_request_with_timeouts_windows(
            socket_path,
            request,
            connect_timeout,
            response_timeout,
        )
    }

    #[cfg(not(windows))]
    {
        send_control_request_with_timeouts_unix(
            socket_path,
            request,
            connect_timeout,
            response_timeout,
        )
    }
}

pub fn send_control_request(
    socket_path: &Path,
    request: &ControlRequest,
) -> Result<ControlResponse, GitAiError> {
    send_control_request_with_timeouts(
        socket_path,
        request,
        DAEMON_CONTROL_CONNECT_TIMEOUT,
        control_request_response_timeout(request),
    )
}

pub fn send_checkpoint_request_with_timeout(
    socket_path: &Path,
    request: &CheckpointRequest,
    timeout: Duration,
) -> Result<ControlResponse, GitAiError> {
    send_checkpoint_request_with_timeouts(socket_path, request, timeout, timeout)
}

pub fn send_checkpoint_request(
    socket_path: &Path,
    request: &CheckpointRequest,
) -> Result<ControlResponse, GitAiError> {
    send_checkpoint_request_with_timeouts(
        socket_path,
        request,
        DAEMON_CONTROL_CONNECT_TIMEOUT,
        checkpoint_control_response_timeout(
            &ControlRequest::CheckpointRun { body_bytes: 0 },
            checkpoint_control_timeout_uses_ci_or_test_budget(),
        ),
    )
}

fn send_checkpoint_request_with_timeouts(
    socket_path: &Path,
    request: &CheckpointRequest,
    connect_timeout: Duration,
    response_timeout: Duration,
) -> Result<ControlResponse, GitAiError> {
    let body = serde_json::to_vec(request)?;
    let body_bytes = u64::try_from(body.len())
        .map_err(|_| GitAiError::Generic("checkpoint body length exceeds u64".to_string()))?;
    let header = ControlRequest::CheckpointRun { body_bytes };

    let mut stream = open_local_socket_stream_with_timeout(socket_path, connect_timeout)?;
    set_daemon_client_stream_timeouts(&mut stream, socket_path, response_timeout)?;
    let mut header_bytes = serde_json::to_vec(&header)?;
    header_bytes.push(b'\n');
    write_all_daemon_client_stream(&mut stream, socket_path, &header_bytes)?;

    let mut response_reader = BufReader::new(stream);
    let ready_line = read_daemon_client_line(&mut response_reader, socket_path, response_timeout)?;
    let ready: ControlResponse =
        serde_json::from_str(ready_line.trim()).map_err(GitAiError::from)?;
    if !ready.ok {
        return Ok(ready);
    }
    if ready
        .data
        .as_ref()
        .and_then(|data| data.get("ready"))
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err(GitAiError::Generic(
            "daemon checkpoint handshake omitted ready confirmation".to_string(),
        ));
    }

    let mut framed_body = body;
    framed_body.push(b'\n');
    write_all_daemon_client_stream(response_reader.get_mut(), socket_path, &framed_body)?;
    let response_line =
        read_daemon_client_line(&mut response_reader, socket_path, response_timeout)?;
    if response_line.trim().is_empty() {
        return Err(GitAiError::Generic(
            "empty daemon checkpoint response".to_string(),
        ));
    }
    serde_json::from_str(response_line.trim()).map_err(GitAiError::from)
}

pub fn send_control_request_fire_and_forget(
    socket_path: &Path,
    request: &ControlRequest,
) -> Result<(), GitAiError> {
    let mut stream =
        open_local_socket_stream_with_timeout(socket_path, DAEMON_CONTROL_CONNECT_TIMEOUT)?;
    let write_timeout = Duration::from_millis(500);
    set_daemon_client_stream_timeouts(&mut stream, socket_path, write_timeout)?;
    let mut body = serde_json::to_vec(request)?;
    body.push(b'\n');
    write_all_daemon_client_stream(&mut stream, socket_path, &body)?;
    Ok(())
}

#[cfg(test)]
mod stream_worker_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::ffi::OsString;
    use std::io::Write;

    #[test]
    fn secondary_def_repo_does_not_hint_worktree() {
        let primary = serde_json::json!({
            "event": "def_repo",
            "sid": "s1",
            "repo": 1,
            "worktree": "/repo",
        });
        assert_eq!(
            trace_payload_worktree_hint(&primary),
            Some(PathBuf::from("/repo"))
        );

        // A nested/embedded repo git peeked into (repo index > 1) must not
        // retarget the command's worktree at that repo.
        let secondary = serde_json::json!({
            "event": "def_repo",
            "sid": "s1",
            "repo": 2,
            "worktree": "/repo/nested",
        });
        assert_eq!(trace_payload_worktree_hint(&secondary), None);
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var_os(key);
            // SAFETY: these tests are serialized via #[serial], so mutating the
            // process environment is isolated for the duration of each test.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, original }
        }

        fn unset(key: &'static str) -> Self {
            let original = std::env::var_os(key);
            // SAFETY: these tests are serialized via #[serial], so mutating the
            // process environment is isolated for the duration of each test.
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => {
                    // SAFETY: these tests are serialized via #[serial], so restoring
                    // process environment state is isolated for the duration of each test.
                    unsafe {
                        std::env::set_var(self.key, value);
                    }
                }
                None => {
                    // SAFETY: these tests are serialized via #[serial], so restoring
                    // process environment state is isolated for the duration of each test.
                    unsafe {
                        std::env::remove_var(self.key);
                    }
                }
            }
        }
    }

    fn sample_checkpoint_request() -> ControlRequest {
        ControlRequest::CheckpointRun { body_bytes: 128 }
    }

    fn run_git_for_test(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("git {:?} failed to spawn: {}", args, error));
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git stdout should be utf8")
            .trim()
            .to_string()
    }

    fn run_git_stdin_for_test(repo: &Path, args: &[&str], stdin: &str) -> String {
        let mut child = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("git {:?} failed to spawn: {}", args, error));
        child
            .stdin
            .take()
            .expect("stdin should be piped")
            .write_all(stdin.as_bytes())
            .expect("write git stdin");
        let output = child
            .wait_with_output()
            .unwrap_or_else(|error| panic!("git {:?} failed to wait: {}", args, error));
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git stdout should be utf8")
            .trim()
            .to_string()
    }

    #[test]
    fn conflict_resolution_note_read_errors_are_not_silently_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let repo_path = temp.path().join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        run_git_for_test(&repo_path, &["init"]);
        run_git_for_test(&repo_path, &["config", "user.name", "Test User"]);
        run_git_for_test(&repo_path, &["config", "user.email", "test@example.com"]);

        std::fs::write(repo_path.join("file.txt"), "onto\n").unwrap();
        run_git_for_test(&repo_path, &["add", "file.txt"]);
        run_git_for_test(&repo_path, &["commit", "-m", "onto"]);
        let onto = run_git_for_test(&repo_path, &["rev-parse", "HEAD"]);

        std::fs::write(repo_path.join("file.txt"), "onto\nnew\n").unwrap();
        run_git_for_test(&repo_path, &["add", "file.txt"]);
        run_git_for_test(&repo_path, &["commit", "-m", "new"]);
        let new_tip = run_git_for_test(&repo_path, &["rev-parse", "HEAD"]);

        let missing_blob = "2222222222222222222222222222222222222222";
        let prefix = &new_tip[..2];
        let suffix = &new_tip[2..];
        let leaf_tree = run_git_stdin_for_test(
            &repo_path,
            &["mktree", "--missing"],
            &format!("100644 blob {missing_blob}\t{suffix}\n"),
        );
        let root_tree = run_git_stdin_for_test(
            &repo_path,
            &["mktree"],
            &format!("040000 tree {leaf_tree}\t{prefix}\n"),
        );
        run_git_for_test(&repo_path, &["update-ref", "refs/notes/ai", &root_tree]);

        let repo = crate::git::find_repository_in_path(repo_path.to_str().unwrap())
            .expect("find test repository");
        let result = process_conflict_resolution_working_logs(&repo, &new_tip, Some(&onto));
        assert!(
            result.is_err(),
            "corrupt destination notes must fail closed instead of being treated as absent"
        );
    }

    #[test]
    fn revert_source_args_do_not_treat_bare_gpg_sign_as_value_option() {
        assert_eq!(
            revert_source_args_from_command_args(&["--gpg-sign".to_string(), "HEAD~1".to_string()]),
            vec!["HEAD~1"]
        );
        assert_eq!(
            revert_source_args_from_command_args(&["-S".to_string(), "HEAD~1".to_string()]),
            vec!["HEAD~1"]
        );
        assert_eq!(
            revert_source_args_from_command_args(&["-Smy-key".to_string(), "HEAD~1".to_string()]),
            vec!["HEAD~1"]
        );
    }

    #[test]
    fn cherry_pick_source_args_do_not_treat_bare_gpg_sign_as_value_option() {
        assert_eq!(
            cherry_pick_source_args_from_command_args(&[
                "--gpg-sign".to_string(),
                "HEAD~1".to_string()
            ]),
            vec!["HEAD~1"]
        );
        assert_eq!(
            cherry_pick_source_args_from_command_args(&["-S".to_string(), "HEAD~1".to_string()]),
            vec!["HEAD~1"]
        );
        assert_eq!(
            cherry_pick_source_args_from_command_args(&[
                "-Smy-key".to_string(),
                "HEAD~1".to_string()
            ]),
            vec!["HEAD~1"]
        );
    }

    #[test]
    fn checkpoint_requests_use_long_timeout_in_ci_or_test_env() {
        assert_eq!(
            checkpoint_control_response_timeout(&sample_checkpoint_request(), true),
            DAEMON_CHECKPOINT_RESPONSE_TIMEOUT
        );
    }

    #[test]
    fn checkpoint_requests_use_short_timeout_in_product_env() {
        assert_eq!(
            checkpoint_control_response_timeout(&sample_checkpoint_request(), false),
            DAEMON_CONTROL_RESPONSE_TIMEOUT
        );
    }

    #[test]
    fn shutdown_requests_allow_checkpoint_drain_timeout() {
        assert_eq!(
            checkpoint_control_response_timeout(&ControlRequest::Shutdown, false),
            DAEMON_CHECKPOINT_RESPONSE_TIMEOUT
        );
    }

    #[tokio::test]
    async fn gc_keeps_held_family_exec_locks_and_evicts_only_idle_ones() {
        let coord = ActorDaemonCoordinator::new();

        let held_lock = coord
            .side_effect_exec_lock("family-held")
            .expect("held family lock");
        let _held_guard = held_lock.lock().await;
        coord
            .side_effect_exec_lock("family-idle")
            .expect("idle family lock");

        coord.gc_stale_family_state();

        let map = coord.side_effect_exec_locks.lock().unwrap();
        let surviving = map.get("family-held").expect(
            "GC must never evict a held exec lock: a re-created lock would let a \
             second drain run concurrently on the same family",
        );
        assert!(
            Arc::ptr_eq(surviving, &held_lock),
            "GC must keep the SAME lock instance while it is held"
        );
        assert!(
            !map.contains_key("family-idle"),
            "GC should evict idle exec locks to bound the map"
        );
    }

    #[tokio::test]
    async fn draining_an_unknown_family_does_not_retain_empty_sequencer_state() {
        let coord = ActorDaemonCoordinator::new();

        coord
            .drain_ready_family_sequencer_entries_locked("family-with-no-work")
            .await
            .expect("empty family drain");

        let map = coord.family_sequencers_by_family.lock().unwrap();
        assert!(
            !map.contains_key("family-with-no-work"),
            "global drains must not grow the sequencer map with empty families"
        );
    }

    #[test]
    fn checkpoint_ingress_quota_bounds_count_and_bytes_and_releases_on_drop() {
        let quota = Arc::new(CheckpointIngressQuota::new(2, 10));
        let first = quota.reserve(4).expect("first reservation");
        let second = quota.reserve(6).expect("second reservation");

        let count_error = quota.reserve(0).expect_err("count limit must reject");
        assert_eq!(count_error.reason, "request_limit");

        drop(second);
        let byte_error = quota.reserve(7).expect_err("byte limit must reject");
        assert_eq!(byte_error.reason, "byte_limit");

        drop(first);
        let replacement = quota.reserve(10).expect("released quota must be reusable");
        assert_eq!(replacement.body_bytes(), 10);
    }

    #[test]
    fn checkpoint_body_reader_requires_exact_length_and_delimiter() {
        let mut valid = std::io::BufReader::new(std::io::Cursor::new(b"body\n".to_vec()));
        assert_eq!(
            read_checkpoint_body(&mut valid, 4).expect("valid framed body"),
            b"body"
        );

        let mut truncated = std::io::BufReader::new(std::io::Cursor::new(b"bod".to_vec()));
        assert!(read_checkpoint_body(&mut truncated, 4).is_err());

        let mut missing_delimiter =
            std::io::BufReader::new(std::io::Cursor::new(b"body!".to_vec()));
        assert!(read_checkpoint_body(&mut missing_delimiter, 4).is_err());
    }

    #[test]
    fn transcript_sweep_triggers_for_commit_amend_and_push_events() {
        use crate::daemon::domain::SemanticEvent;
        use crate::daemon::stream_worker::SweepTrigger;

        assert_eq!(
            transcript_sweep_triggers_for_events(&[SemanticEvent::CommitCreated {
                base: Some("base".to_string()),
                new_head: "new".to_string(),
            }]),
            vec![SweepTrigger::PostCommit]
        );
        assert_eq!(
            transcript_sweep_triggers_for_events(&[SemanticEvent::CommitAmended {
                old_head: "old".to_string(),
                new_head: "new".to_string(),
            }]),
            vec![SweepTrigger::PostCommit]
        );
        assert_eq!(
            transcript_sweep_triggers_for_events(&[SemanticEvent::PushCompleted {
                remote: Some("origin".to_string()),
            }]),
            vec![SweepTrigger::PostPush]
        );
        assert_eq!(
            transcript_sweep_triggers_for_events(&[
                SemanticEvent::CommitCreated {
                    base: Some("base".to_string()),
                    new_head: "new".to_string(),
                },
                SemanticEvent::PushCompleted {
                    remote: Some("origin".to_string()),
                },
            ]),
            vec![SweepTrigger::PostCommit, SweepTrigger::PostPush]
        );
    }

    fn test_rebase_command(
        invoked_args: &[&str],
        ref_changes: Vec<crate::daemon::domain::RefChange>,
    ) -> crate::daemon::domain::NormalizedCommand {
        crate::daemon::domain::NormalizedCommand {
            scope: crate::daemon::domain::CommandScope::Family(crate::daemon::domain::FamilyKey(
                "/repo/.git".to_string(),
            )),
            family_key: Some(crate::daemon::domain::FamilyKey("/repo/.git".to_string())),
            worktree: Some(PathBuf::from("/repo")),
            root_sid: "rebase-test".to_string(),
            raw_argv: std::iter::once("git")
                .chain(std::iter::once("rebase"))
                .chain(invoked_args.iter().copied())
                .map(str::to_string)
                .collect(),
            primary_command: Some("rebase".to_string()),
            invoked_command: Some("rebase".to_string()),
            invoked_args: invoked_args.iter().map(|arg| (*arg).to_string()).collect(),
            observed_child_commands: Vec::new(),
            exit_code: 0,
            started_at_ns: 1,
            finished_at_ns: 2,
            reflog_start_offsets: HashMap::new(),
            stash_target_oid: None,
            cherry_pick_source_oids: Vec::new(),
            revert_source_oids: Vec::new(),
            ref_changes,
            confidence: crate::daemon::domain::Confidence::High,
        }
    }

    fn ref_change(reference: &str, old: &str, new: &str) -> crate::daemon::domain::RefChange {
        crate::daemon::domain::RefChange {
            reference: reference.to_string(),
            old: old.to_string(),
            new: new.to_string(),
        }
    }

    #[test]
    fn explicit_branch_rebase_original_head_prefers_branch_ref_over_head() {
        const MAIN: &str = "1111111111111111111111111111111111111111";
        const FEATURE: &str = "2222222222222222222222222222222222222222";
        const ONTO: &str = "3333333333333333333333333333333333333333";

        let cmd = test_rebase_command(
            &["master", "scenario-3-multi-file-conflict"],
            vec![
                ref_change("HEAD", MAIN, FEATURE),
                ref_change("HEAD", FEATURE, ONTO),
                ref_change(
                    "refs/heads/scenario-3-multi-file-conflict",
                    FEATURE,
                    FEATURE,
                ),
            ],
        );

        assert_eq!(
            strict_rebase_original_head_from_command(&cmd, MAIN),
            Some(FEATURE.to_string()),
            "explicit branch rebase must store the target branch tip, not the caller's original HEAD"
        );
    }

    #[test]
    fn pending_rebase_new_tip_prefers_matching_branch_ref_over_later_head_noise() {
        const ORIGINAL: &str = "1111111111111111111111111111111111111111";
        const ONTO: &str = "2222222222222222222222222222222222222222";
        const NEW_TIP: &str = "3333333333333333333333333333333333333333";
        const UNRELATED_HEAD: &str = "4444444444444444444444444444444444444444";

        let cmd = test_rebase_command(
            &["--continue"],
            vec![
                ref_change("HEAD", ONTO, NEW_TIP),
                ref_change(
                    "refs/heads/scenario-3-multi-file-conflict",
                    ORIGINAL,
                    NEW_TIP,
                ),
                ref_change("HEAD", NEW_TIP, UNRELATED_HEAD),
            ],
        );

        assert_eq!(
            rebase_new_tip_from_command(&cmd, ORIGINAL),
            Some(NEW_TIP.to_string()),
            "pending rebase completion must use the branch ref update that rewrote the original tip"
        );
    }

    #[test]
    #[serial]
    fn checkpoint_control_timeout_uses_ci_env_var() {
        let _unset_test = EnvVarGuard::unset("GIT_AI_TEST_DB_PATH");
        let _unset_legacy_test = EnvVarGuard::unset("GITAI_TEST_DB_PATH");
        let _set_ci = EnvVarGuard::set("CI", "true");

        assert!(checkpoint_control_timeout_uses_ci_or_test_budget());
    }

    #[test]
    #[serial]
    fn checkpoint_control_timeout_uses_test_db_env_var() {
        let _unset_ci = EnvVarGuard::unset("CI");
        let _unset_legacy_test = EnvVarGuard::unset("GITAI_TEST_DB_PATH");
        let _set_test = EnvVarGuard::set("GIT_AI_TEST_DB_PATH", "/tmp/git-ai-test.db");

        assert!(checkpoint_control_timeout_uses_ci_or_test_budget());
    }

    #[test]
    #[serial]
    fn checkpoint_control_timeout_false_when_no_ci_or_test_vars() {
        let _unset_ci = EnvVarGuard::unset("CI");
        let _unset_test = EnvVarGuard::unset("GIT_AI_TEST_DB_PATH");
        let _unset_legacy_test = EnvVarGuard::unset("GITAI_TEST_DB_PATH");

        assert!(!checkpoint_control_timeout_uses_ci_or_test_budget());
    }

    #[test]
    fn compute_watermarks_uses_symlink_metadata_not_target_mtime() {
        // Verify that compute_watermarks_from_stat uses lstat (symlink's own mtime)
        // not stat (target file's mtime), consistent with snapshot's symlink_metadata.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // Create a target file
        let target = dir.join("target.txt");
        std::fs::write(&target, b"hello").unwrap();

        // Create a symlink pointing to the target
        let link = dir.join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).unwrap();

        // Watermark the symlink
        let wm = compute_watermarks_from_stat(dir.to_str().unwrap(), &["link.txt".to_string()]);

        // The watermark should match symlink_metadata mtime, not target metadata mtime.
        let symlink_meta = std::fs::symlink_metadata(&link).unwrap();
        let target_meta = std::fs::metadata(&link).unwrap(); // follows symlink

        let symlink_mtime = symlink_meta
            .modified()
            .unwrap()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let target_mtime = target_meta
            .modified()
            .unwrap()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let recorded = *wm.get("link.txt").unwrap();

        assert_eq!(
            recorded, symlink_mtime,
            "watermark should match lstat mtime of the symlink itself"
        );
        // This assertion documents the intent: if symlink and target mtimes differ,
        // the watermark must track the symlink, not the target.
        let _ = target_mtime; // used only as documentation; may equal symlink_mtime on some FS
    }

    #[test]
    fn explicit_stop_overrides_prior_restart_intent() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let coordinator = ActorDaemonCoordinator::new();

            coordinator.request_restart_after_update();
            assert_eq!(
                coordinator.shutdown_action(),
                DaemonExitAction::RestartAfterUpdate
            );

            coordinator.request_stop();

            assert!(coordinator.is_shutting_down());
            assert_eq!(coordinator.shutdown_action(), DaemonExitAction::Stop);
        });
    }

    // -----------------------------------------------------------------------
    // Readonly command ingress fast-path tests
    //
    // These tests verify that prepare_trace_payload_for_ingest returns false
    // (do-not-enqueue) for read-only commands and true for mutating ones, and
    // that the queued_trace_payloads counter is not incremented for read-only
    // events.
    //
    // ActorDaemonCoordinator::new() spawns Tokio tasks internally, so all
    // tests that construct one must run inside a Tokio runtime.
    // -----------------------------------------------------------------------

    fn make_start_payload(argv: &[&str]) -> Value {
        serde_json::json!({
            "event": "start",
            "sid": "20260411T120000.000000-Psid1",
            "argv": argv,
        })
    }

    fn make_atexit_payload(sid: &str) -> Value {
        serde_json::json!({
            "event": "atexit",
            "sid": sid,
            "code": 0,
        })
    }

    #[test]
    fn exit_is_not_a_root_completion_boundary() {
        let sid = "20260411T120000.000000-Psid1";

        assert!(
            !is_terminal_root_trace_event("exit", sid, sid),
            "trace2 exit can fire before Git atexit cleanup and must not complete root processing"
        );
        assert!(is_terminal_root_trace_event("atexit", sid, sid));
    }

    #[tokio::test]
    async fn readonly_start_event_is_not_enqueued() {
        let coord = ActorDaemonCoordinator::new();
        let mut payload = make_start_payload(&["git", "status", "--short"]);
        let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
        assert!(
            !should_enqueue,
            "status start event should not be enqueued (readonly)"
        );
        assert_eq!(
            coord.queued_trace_payloads.load(Ordering::Relaxed),
            0,
            "queued_trace_payloads should stay 0 for readonly start event"
        );
        // Readonly events must NOT receive an ingest sequence number
        assert!(
            payload.get(TRACE_INGEST_SEQ_FIELD).is_none(),
            "readonly start event must not receive an ingest sequence number"
        );
    }

    #[tokio::test]
    async fn stash_list_start_event_is_not_enqueued() {
        let coord = ActorDaemonCoordinator::new();
        let mut payload = make_start_payload(&[
            "git",
            "-c",
            "core.fsmonitor=false",
            "--no-pager",
            "stash",
            "list",
            "--pretty=format:%gd%x00%H%x00%ct%x00%s",
        ]);
        let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
        assert!(
            !should_enqueue,
            "stash list start event should not be enqueued (readonly invocation)"
        );
        assert!(
            payload.get(TRACE_INGEST_SEQ_FIELD).is_none(),
            "stash list start event must not receive an ingest sequence number"
        );
    }

    #[tokio::test]
    async fn worktree_list_start_event_is_not_enqueued() {
        let coord = ActorDaemonCoordinator::new();
        let mut payload = make_start_payload(&[
            "git",
            "--no-pager",
            "--no-optional-locks",
            "worktree",
            "list",
            "--porcelain",
        ]);
        let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
        assert!(
            !should_enqueue,
            "worktree list start event should not be enqueued (readonly invocation)"
        );
        assert!(
            payload.get(TRACE_INGEST_SEQ_FIELD).is_none(),
            "worktree list start event must not receive an ingest sequence number"
        );
    }

    #[tokio::test]
    async fn branch_show_current_start_event_is_not_enqueued() {
        let coord = ActorDaemonCoordinator::new();
        let mut payload = make_start_payload(&["git", "branch", "--show-current"]);
        let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
        assert!(
            !should_enqueue,
            "branch --show-current start event should not be enqueued"
        );
        assert!(
            payload.get(TRACE_INGEST_SEQ_FIELD).is_none(),
            "branch --show-current must not receive an ingest sequence number"
        );
    }

    #[tokio::test]
    async fn diff_numstat_start_event_is_not_enqueued() {
        let coord = ActorDaemonCoordinator::new();
        let mut payload = make_start_payload(&[
            "git",
            "-c",
            "core.fsmonitor=false",
            "--no-pager",
            "diff",
            "--numstat",
            "--no-renames",
            "HEAD",
        ]);
        let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
        assert!(
            !should_enqueue,
            "diff --numstat start event should not be enqueued"
        );
    }

    #[tokio::test]
    async fn for_each_ref_start_event_is_not_enqueued() {
        let coord = ActorDaemonCoordinator::new();
        let mut payload = make_start_payload(&[
            "git",
            "--no-pager",
            "for-each-ref",
            "refs/heads/**/*",
            "refs/remotes/**/*",
            "--format",
            "%(HEAD)%00%(objectname)",
        ]);
        let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
        assert!(
            !should_enqueue,
            "for-each-ref start event should not be enqueued"
        );
    }

    #[tokio::test]
    async fn cat_file_start_event_is_not_enqueued() {
        let coord = ActorDaemonCoordinator::new();
        let mut payload = make_start_payload(&[
            "git",
            "--no-optional-locks",
            "cat-file",
            "--batch-check=%(objectname)",
        ]);
        let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
        assert!(
            !should_enqueue,
            "cat-file start event should not be enqueued"
        );
    }

    #[tokio::test]
    async fn show_commit_start_event_is_not_enqueued() {
        let coord = ActorDaemonCoordinator::new();
        let mut payload = make_start_payload(&[
            "git",
            "--no-optional-locks",
            "show",
            "--no-patch",
            "--format=%H%x00%B%x00%at",
            "07270e1489439d6b36fcb2a4198d2fb68e37727c",
        ]);
        let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
        assert!(!should_enqueue, "show start event should not be enqueued");
    }

    #[tokio::test]
    async fn mutating_commit_start_event_is_enqueued() {
        let coord = Arc::new(ActorDaemonCoordinator::new());
        coord.start_trace_ingest_worker().unwrap();
        let mut payload = make_start_payload(&["git", "commit", "-m", "test commit"]);
        let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
        assert!(
            should_enqueue,
            "commit start event should be enqueued (mutating)"
        );
        assert!(
            payload.get(TRACE_INGEST_SEQ_FIELD).is_none(),
            "mutating event must not receive an ingest sequence number before enqueue capacity is reserved"
        );
        assert_eq!(
            coord.next_trace_ingest_seq.load(Ordering::Acquire),
            0,
            "prepare must not allocate an ingest sequence"
        );
        coord
            .enqueue_trace_payload(payload)
            .expect("mutating event should enqueue");
        assert!(
            coord.next_trace_ingest_seq.load(Ordering::Acquire) > 0,
            "enqueue must allocate an ingest sequence number"
        );
        coord.request_shutdown();
    }

    #[tokio::test]
    async fn mutating_pending_root_is_created_when_repo_and_argv_arrive_on_different_events() {
        let coord = ActorDaemonCoordinator::new();
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(temp.path())
            .arg("init")
            .arg("repo")
            .output()
            .expect("git init should run");
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        let sid = "20260411T120000.000000-Psid-split-metadata";
        let mut def_repo = serde_json::json!({
            "event": "def_repo",
            "sid": sid,
            "worktree": repo,
            "time_ns": 1u64,
        });
        assert!(coord.prepare_trace_payload_for_ingest(&mut def_repo));
        coord
            .apply_trace_payload_to_state(def_repo)
            .await
            .expect("def_repo should ingest");
        assert!(
            !coord
                .pending_root_slots_by_root
                .lock()
                .unwrap()
                .contains_key(sid),
            "repo-only metadata is not enough to sequence a command"
        );

        let mut start = serde_json::json!({
            "event": "start",
            "sid": sid,
            "argv": ["git", "reset", "--soft", "HEAD~1"],
            "time_ns": 2u64,
        });
        assert!(coord.prepare_trace_payload_for_ingest(&mut start));
        coord
            .apply_trace_payload_to_state(start)
            .await
            .expect("start should ingest");

        assert!(
            coord
                .pending_root_slots_by_root
                .lock()
                .unwrap()
                .contains_key(sid),
            "mutating roots must be sequenced once argv and repo metadata are both known, even when they arrive on different events"
        );
    }

    #[tokio::test]
    async fn mutating_trace_payload_captures_repo_reflog_start_offsets() {
        let coord = ActorDaemonCoordinator::new();
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let git_dir = repo.join(".git");
        let head_log = git_dir.join("logs/HEAD");
        let stash_log = repo.join(".git/logs/refs/stash");
        let branch_log = repo.join(".git/logs/refs/heads/main");
        std::fs::create_dir_all(head_log.parent().unwrap()).unwrap();
        std::fs::create_dir_all(stash_log.parent().unwrap()).unwrap();
        std::fs::create_dir_all(branch_log.parent().unwrap()).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let old_head_reflog = b"old HEAD reflog entry\n";
        let old_reflog = b"old stash reflog entry\n";
        let old_branch_reflog = b"old branch reflog entry\n";
        std::fs::write(&head_log, old_head_reflog).unwrap();
        std::fs::write(&stash_log, old_reflog).unwrap();
        std::fs::write(&branch_log, old_branch_reflog).unwrap();
        let mut payload = serde_json::json!({
            "event": "start",
            "sid": "20260411T120000.000000-Psid-reflog",
            "argv": ["git", "reset", "--hard", "HEAD~1"],
            "worktree": repo,
        });

        assert!(coord.prepare_trace_payload_for_ingest(&mut payload));

        let offsets = payload
            .get(TRACE_ROOT_REFLOG_START_OFFSETS_FIELD)
            .and_then(Value::as_object)
            .expect("mutating trace payload should include reflog start offsets");
        let head_key = format!(
            "worktree:{}:HEAD",
            git_dir.canonicalize().unwrap().to_string_lossy()
        );
        assert_eq!(
            offsets.get(&head_key).and_then(Value::as_u64),
            Some(old_head_reflog.len() as u64)
        );
        assert_eq!(
            offsets.get("common:refs/stash").and_then(Value::as_u64),
            Some(old_reflog.len() as u64)
        );
        assert_eq!(
            offsets
                .get("common:refs/heads/main")
                .and_then(Value::as_u64),
            Some(old_branch_reflog.len() as u64)
        );
    }

    #[tokio::test]
    async fn checkpoint_fence_waits_for_open_mutating_trace_root() {
        let coord = Arc::new(ActorDaemonCoordinator::new());
        let sid = "20260411T120000.000000-Psid1";
        coord.trace_root_connection_opened(sid).unwrap();
        let mut payload = make_start_payload(&["git", "commit", "-m", "test commit"]);
        assert!(
            coord.prepare_trace_payload_for_ingest(&mut payload),
            "commit start should mark the root as mutating"
        );

        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                coord.wait_for_trace_ingest_processed_through()
            )
            .await
            .is_err(),
            "checkpoint fence must not pass while a mutating trace root is still open"
        );

        coord
            .record_trace_connection_close(&[sid.to_string()])
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            coord.wait_for_trace_ingest_processed_through(),
        )
        .await
        .expect("checkpoint fence should pass once the mutating trace root closes");
    }

    #[tokio::test]
    async fn family_fence_ignores_open_mutating_roots_of_other_families() {
        let coord = Arc::new(ActorDaemonCoordinator::new());
        let temp = tempfile::tempdir().unwrap();
        let other_repo = temp.path().join("other-repo");
        std::fs::create_dir_all(other_repo.join(".git")).unwrap();
        std::fs::write(
            other_repo.join(".git").join("HEAD"),
            "ref: refs/heads/main\n",
        )
        .unwrap();

        let sid = "20260411T120000.000000-Psid1";
        coord.trace_root_connection_opened(sid).unwrap();
        let mut start = make_start_payload(&["git", "commit", "-m", "other repo commit"]);
        assert!(coord.prepare_trace_payload_for_ingest(&mut start));

        // Before the root is attributed to a repository, it must block every
        // family (fail closed: it could belong to any of them).
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                coord.wait_for_trace_ingest_processed_through_family("/some/unrelated/family")
            )
            .await
            .is_err(),
            "an unattributed mutating root must block all family fences"
        );

        // def_repo attributes the root to other-repo's family; unrelated
        // families must no longer wait on it.
        let mut def_repo = serde_json::json!({
            "event": "def_repo",
            "sid": sid,
            "worktree": other_repo.to_string_lossy(),
        });
        assert!(coord.prepare_trace_payload_for_ingest(&mut def_repo));

        tokio::time::timeout(
            Duration::from_millis(250),
            coord.wait_for_trace_ingest_processed_through_family("/some/unrelated/family"),
        )
        .await
        .expect("a mutating root attributed to another family must not block this fence");

        // The root's own family still waits until the connection closes.
        let own_family = coord.backend.resolve_family(&other_repo).unwrap().0;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                coord.wait_for_trace_ingest_processed_through_family(&own_family)
            )
            .await
            .is_err(),
            "the root's own family fence must still wait for the open root"
        );

        coord
            .record_trace_connection_close(&[sid.to_string()])
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            coord.wait_for_trace_ingest_processed_through_family(&own_family),
        )
        .await
        .expect("own family fence should pass once the root closes");
    }

    #[tokio::test]
    async fn trace_connection_close_without_atexit_cancels_pending_root() {
        let coord = Arc::new(ActorDaemonCoordinator::new());
        coord.start_trace_ingest_worker().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = worktree.join(".git");
        std::fs::create_dir_all(git_dir.join("logs")).unwrap();

        let sid = "20260411T120000.000000-Psid-close";
        coord.trace_root_connection_opened(sid).unwrap();
        let mut start = serde_json::json!({
            "event": "start",
            "sid": sid,
            "argv": ["git", "commit", "-m", "test commit"],
            "worktree": worktree,
            "time_ns": 1u64,
        });
        assert!(coord.prepare_trace_payload_for_ingest(&mut start));
        coord.enqueue_trace_payload(start).unwrap();

        finalize_trace_connection_roots(coord.clone(), [sid.to_string()].into_iter().collect())
            .unwrap();
        coord.wait_for_trace_ingest_processed_through().await;

        assert!(
            !coord
                .pending_root_slots_by_root
                .lock()
                .unwrap()
                .contains_key(sid),
            "closing the trace stream without root atexit must not leave the family sequencer wedged"
        );
        coord.request_shutdown();
    }

    #[tokio::test]
    async fn readonly_trace_connection_close_without_atexit_clears_tracking() {
        let coord = ActorDaemonCoordinator::new();
        let sid = "20260411T120000.000000-Psid-readonly-close";
        coord.trace_root_connection_opened(sid).unwrap();
        let mut start = make_start_payload(&["git", "status", "--short"]);
        start["sid"] = serde_json::json!(sid);
        assert!(!coord.prepare_trace_payload_for_ingest(&mut start));

        let close_marker_roots = coord
            .record_trace_connection_close(&[sid.to_string()])
            .unwrap();

        assert!(
            close_marker_roots.is_empty(),
            "read-only roots should not enqueue synthetic close markers"
        );
        let ingress = coord.trace_ingress_state.lock().unwrap();
        assert!(!ingress.root_argv.contains_key(sid));
        assert!(!ingress.root_definitely_read_only.contains(sid));
        assert!(!ingress.root_open_connections.contains_key(sid));
    }

    #[tokio::test]
    async fn checkpoint_fence_does_not_wait_for_unidentified_trace_connection() {
        let coord = Arc::new(ActorDaemonCoordinator::new());
        coord.trace_unidentified_connection_opened().unwrap();

        tokio::time::timeout(
            Duration::from_secs(1),
            coord.wait_for_trace_ingest_processed_through(),
        )
        .await
        .expect("checkpoint fence must not wait for an accepted trace connection with no root");

        coord
            .trace_unidentified_connection_identified_or_closed()
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            coord.wait_for_trace_ingest_processed_through(),
        )
        .await
        .expect("checkpoint fence should pass once the unidentified connection is resolved");
    }

    #[tokio::test]
    async fn checkpoint_fence_waits_for_open_branch_mutation_root() {
        let coord = Arc::new(ActorDaemonCoordinator::new());
        let sid = "20260411T120000.000000-Psid1";
        coord.trace_root_connection_opened(sid).unwrap();
        let mut payload = make_start_payload(&["git", "branch", "-D", "feature"]);
        assert!(
            coord.prepare_trace_payload_for_ingest(&mut payload),
            "branch delete start should be enqueued because it mutates refs"
        );

        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                coord.wait_for_trace_ingest_processed_through()
            )
            .await
            .is_err(),
            "checkpoint fence must not pass while an accepted branch mutation root is still open"
        );

        coord
            .record_trace_connection_close(&[sid.to_string()])
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            coord.wait_for_trace_ingest_processed_through(),
        )
        .await
        .expect("checkpoint fence should pass once the branch mutation root closes");
    }

    #[tokio::test]
    async fn checkpoint_fence_does_not_wait_for_open_branch_readonly_root() {
        let coord = Arc::new(ActorDaemonCoordinator::new());
        let sid = "20260411T120000.000000-Psid-readonly-branch";
        coord.trace_root_connection_opened(sid).unwrap();
        let mut payload = make_start_payload(&["git", "branch", "--show-current"]);
        payload["sid"] = serde_json::json!(sid);
        assert!(
            !coord.prepare_trace_payload_for_ingest(&mut payload),
            "branch --show-current should be classified as read-only"
        );

        tokio::time::timeout(
            Duration::from_secs(1),
            coord.wait_for_trace_ingest_processed_through(),
        )
        .await
        .expect("checkpoint fence must not wait for an open read-only branch root");
    }

    #[tokio::test]
    async fn mutating_stash_pop_start_event_is_enqueued() {
        let coord = ActorDaemonCoordinator::new();
        let mut payload = make_start_payload(&["git", "stash", "pop"]);
        let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
        assert!(
            should_enqueue,
            "stash pop start event should be enqueued (mutating)"
        );
    }

    #[tokio::test]
    async fn mutating_worktree_add_start_event_is_enqueued() {
        let coord = ActorDaemonCoordinator::new();
        let mut payload = make_start_payload(&["git", "worktree", "add", "/tmp/branch", "branch"]);
        let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
        assert!(
            should_enqueue,
            "worktree add start event should be enqueued (mutating)"
        );
    }

    #[tokio::test]
    async fn readonly_atexit_event_is_not_enqueued_after_readonly_start() {
        let coord = ActorDaemonCoordinator::new();
        let sid = "20260411T120000.000000-Psid1";

        // Process start event first — marks root as read-only
        let mut start = make_start_payload(&["git", "status"]);
        // Override sid to match
        start["sid"] = serde_json::json!(sid);
        coord.prepare_trace_payload_for_ingest(&mut start);

        // atexit for same root should also be skipped
        let mut atexit = make_atexit_payload(sid);
        let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut atexit);
        assert!(
            !should_enqueue,
            "atexit for readonly root should not be enqueued"
        );
    }

    /// Performance invariant: 10,000 readonly start events must be processed
    /// (and discarded) in under 200ms.  This guards against regressions that
    /// re-introduce the >1-minute backlog seen with Zed's ~40 invocations/sec.
    #[tokio::test]
    async fn readonly_flood_1000_events_processed_in_under_200ms() {
        let coord = ActorDaemonCoordinator::new();
        let start = std::time::Instant::now();
        for i in 0..1000u64 {
            let sid = format!("20260411T120000.000000-P{:016x}", i);
            let mut payload = serde_json::json!({
                "event": "start",
                "sid": sid,
                "argv": ["git", "-c", "core.fsmonitor=false", "--no-pager",
                         "--no-optional-locks", "status", "--porcelain=v1",
                         "--untracked-files=all", "--no-renames", "-z", "."],
            });
            let enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
            assert!(!enqueue, "status must never be enqueued");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 200,
            "processing 1000 readonly events took {}ms (> 200ms budget)",
            elapsed.as_millis()
        );
        assert_eq!(
            coord.queued_trace_payloads.load(Ordering::Relaxed),
            0,
            "no readonly events should reach the ingest queue"
        );
    }

    /// Ensure a stash-list flood (3208 real-world invocations from Zed)
    /// leaves the ingest queue empty.
    #[tokio::test]
    async fn stash_list_flood_leaves_queue_empty() {
        let coord = ActorDaemonCoordinator::new();
        for i in 0..1000u64 {
            let sid = format!("20260411T120000.000000-P{:016x}", i);
            let mut payload = serde_json::json!({
                "event": "start",
                "sid": sid,
                "argv": ["git", "-c", "core.fsmonitor=false", "--no-pager",
                         "stash", "list", "--pretty=format:%gd%x00%H%x00%ct%x00%s"],
            });
            let _ = coord.prepare_trace_payload_for_ingest(&mut payload);
        }
        assert_eq!(
            coord.queued_trace_payloads.load(Ordering::Relaxed),
            0,
            "stash list flood must not fill the ingest queue"
        );
    }

    /// Ensure a worktree-list flood leaves the ingest queue empty.
    #[tokio::test]
    async fn worktree_list_flood_leaves_queue_empty() {
        let coord = ActorDaemonCoordinator::new();
        for i in 0..1000u64 {
            let sid = format!("20260411T120000.000000-P{:016x}", i);
            let mut payload = serde_json::json!({
                "event": "start",
                "sid": sid,
                "argv": ["git", "--no-pager", "--no-optional-locks",
                         "worktree", "list", "--porcelain"],
            });
            let _ = coord.prepare_trace_payload_for_ingest(&mut payload);
        }
        assert_eq!(
            coord.queued_trace_payloads.load(Ordering::Relaxed),
            0,
            "worktree list flood must not fill the ingest queue"
        );
    }

    // -----------------------------------------------------------------------
    // OnceLock / shutdown / atomic-ordering tests
    // -----------------------------------------------------------------------

    /// `enqueue_trace_payload` must return an error when the ingest worker has
    /// not been started yet.  This is the "no-sender" fast-fail path and is
    /// unchanged by the OnceLock refactor.
    #[tokio::test]
    async fn enqueue_before_worker_start_returns_error() {
        let coord = ActorDaemonCoordinator::new();
        // Worker never started → OnceLock is empty → enqueue must fail
        let payload = serde_json::json!({
            "event": "start",
            "sid": "20260411T120000.000000-Ptest0001",
            "__git_ai_ingest_seq": 1_u64,
            "argv": ["git", "commit", "-m", "test"],
        });
        assert!(
            coord.enqueue_trace_payload(payload).is_err(),
            "enqueue before worker start must return an error"
        );
    }

    #[tokio::test]
    async fn enqueue_accounting_error_does_not_allocate_ingest_sequence() {
        let coord = Arc::new(ActorDaemonCoordinator::new());
        coord.start_trace_ingest_worker().unwrap();
        let poison_coord = coord.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poison_coord
                .queued_trace_payloads_by_root
                .lock()
                .expect("mutex should be lockable before intentional poison");
            panic!("intentional queue accounting mutex poison");
        })
        .join();

        let payload = serde_json::json!({
            "event": "start",
            "sid": "20260411T120000.000000-Paccounting",
            "argv": ["git", "commit", "-m", "test"],
        });
        assert!(
            coord.enqueue_trace_payload(payload).is_err(),
            "poisoned queue accounting must fail enqueue"
        );
        assert_eq!(
            coord.next_trace_ingest_seq.load(Ordering::Acquire),
            0,
            "failed enqueue must not allocate an ingest sequence that can block checkpoint drains"
        );
        coord.request_shutdown();
    }

    /// After `request_shutdown()`, `is_shutting_down()` returns true and the
    /// coordinator stays in a consistent state.  The ingest worker (started
    /// via `start_trace_ingest_worker`) must exit cleanly even when the sender
    /// is no longer dropped by `request_shutdown` (OnceLock never drops it).
    #[tokio::test]
    async fn request_shutdown_is_idempotent_and_consistent() {
        let coord = Arc::new(ActorDaemonCoordinator::new());
        coord.start_trace_ingest_worker().unwrap();
        assert!(!coord.is_shutting_down());
        coord.request_shutdown();
        assert!(coord.is_shutting_down());
        // Second call must not panic.
        coord.request_shutdown();
        assert!(coord.is_shutting_down());
        // Allow tokio to run the ingest worker's shutdown select arm.
        tokio::task::yield_now().await;
    }

    #[tokio::test]
    async fn checkpoint_trace_ingest_drain_returns_on_shutdown() {
        let coord = ActorDaemonCoordinator::new();
        coord.next_trace_ingest_seq.store(1, Ordering::Release);
        coord.processed_trace_ingest_seq.store(0, Ordering::Release);
        coord.request_shutdown();

        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            coord.wait_for_trace_ingest_processed_through(),
        )
        .await
        .expect("checkpoint trace ingest drain must return when daemon shutdown is requested");
    }

    /// The trace socket receive-buffer helper must raise a socket's `SO_RCVBUF`
    /// capacity toward the configured target.
    #[test]
    #[cfg(not(windows))]
    fn trace_socket_recv_buffer_helper_raises_socket_capacity() {
        let (server, _client) =
            std::os::unix::net::UnixStream::pair().expect("create connected unix socket pair");
        let before = socket_recv_buffer(&server).expect("read baseline receive buffer");
        set_socket_recv_buffer(&server, TRACE_SOCKET_RECV_BUFFER_BYTES)
            .expect("set trace socket receive buffer");
        let after = socket_recv_buffer(&server).expect("read trace socket receive buffer");
        // Linux clamps SO_RCVBUF to net.core.rmem_max, so `after` can land below
        // the target on hosts with a small rmem_max (e.g. CI's ~208 KiB
        // default). The helper is still correct as long as it raised capacity
        // toward the target: it either reached the target or grew past the
        // default buffer.
        assert!(
            after >= TRACE_SOCKET_RECV_BUFFER_BYTES || after > before,
            "trace socket receive buffer should reach {} bytes or exceed the {}-byte baseline, got {}",
            TRACE_SOCKET_RECV_BUFFER_BYTES,
            before,
            after
        );
    }

    /// A zero target is a no-op: the helper must not error and must not shrink
    /// the socket's existing receive buffer.
    #[test]
    #[cfg(not(windows))]
    fn trace_socket_recv_buffer_helper_zero_is_noop() {
        let (server, _client) =
            std::os::unix::net::UnixStream::pair().expect("create connected unix socket pair");
        let before = socket_recv_buffer(&server).expect("read baseline receive buffer");
        set_socket_recv_buffer(&server, 0).expect("zero target must be a no-op");
        let after = socket_recv_buffer(&server).expect("read receive buffer after no-op");
        assert_eq!(
            before, after,
            "a zero target must not change the socket receive buffer"
        );
    }

    /// Concurrent enqueues from multiple threads must never deadlock or
    /// corrupt the accounting counter.
    #[tokio::test]
    async fn concurrent_mutating_enqueues_do_not_deadlock() {
        use std::sync::Arc;
        let coord = Arc::new(ActorDaemonCoordinator::new());
        coord.start_trace_ingest_worker().unwrap();

        const TASKS: usize = 8;
        const PER_TASK: usize = 20;

        // Use prepare_trace_payload_for_ingest + enqueue_trace_payload from
        // multiple tasks concurrently.
        let mut handles = Vec::with_capacity(TASKS);
        for task_id in 0..TASKS {
            let c = coord.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..PER_TASK {
                    let sid = format!("20260411T120000.000000-P{:08x}", task_id * 1000 + i);
                    let mut payload = serde_json::json!({
                        "event": "start",
                        "sid": sid,
                        "argv": ["git", "commit", "-m", "msg"],
                    });
                    if c.prepare_trace_payload_for_ingest(&mut payload) {
                        c.enqueue_trace_payload(payload)
                            .expect("mutating event should enqueue");
                    }
                }
            }));
        }
        for h in handles {
            h.await.expect("task must not panic");
        }
        // Give the ingest worker time to drain the queue.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while coord.queued_trace_payloads.load(Ordering::Acquire) > 0 {
            if tokio::time::Instant::now() >= deadline {
                break; // don't fail the test on CI slowness; just stop waiting
            }
            tokio::task::yield_now().await;
        }
        coord.request_shutdown();
    }
}
