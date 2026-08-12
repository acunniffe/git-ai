#![allow(dead_code, unused_imports)]

mod cleanup;
mod command_runner;
mod completion_log;
mod daemon_process;
mod daemon_sync;
mod environment;
mod git_ai_command;
mod git_command;
mod lifecycle;
mod repository;
mod shared_daemon;
mod templates;
mod worktree;

use command_runner::*;
pub(crate) use command_runner::{RawGitCommand, run_raw_git_plumbing};
pub(crate) use completion_log::DaemonTestCompletionLogEntry;
use completion_log::*;
use daemon_process::*;
pub(crate) use daemon_process::{
    DAEMON_SPAWN_LOADER_RETRY_ATTEMPTS, is_windows_loader_init_failure,
};
pub(crate) use daemon_sync::new_daemon_test_sync_session_id;
use daemon_sync::*;
use environment::*;
pub use repository::NewCommit;
use shared_daemon::*;
pub(crate) use templates::real_git_executable;
use templates::*;
pub use templates::{default_branchname, get_binary_path};
pub use worktree::with_worktree_mode;
use worktree::*;

use git_ai::config::ConfigPatch;
use git_ai::feature_flags::FeatureFlags;
use git_ai::model::authorship_log_serialization::AuthorshipLog;
#[cfg(windows)]
use git_ai::model::repository::lock_file::LockFile;
use git_ai::operations::authorship::stats::CommitStats;
use git_ai::operations::daemon::{
    ControlRequest, DaemonConfig, local_socket_connects_with_timeout, send_control_request,
    send_control_request_with_timeout,
};
use git_ai::operations::git::cli_parser::{ParsedGitInvocation, extract_clone_target_directory};
use git_ai::operations::git::path_format::normalize_to_posix;
use git_ai::operations::git::repo_storage::PersistedWorkingLog;
use git_ai::operations::git::repository as GitAiRepository;
// BenchmarkResult for performance testing
#[derive(Debug, Clone)]
pub struct BenchmarkResult {
    pub total_duration: Duration,
    pub git_duration: Duration,
    pub post_command_duration: Duration,
    pub pre_command_duration: Duration,
}
use insta::{Settings, assert_debug_snapshot};
use rand::RngExt;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

use super::test_file::TestFile;

const DAEMON_TEST_PROBE_TIMEOUT: Duration = Duration::from_millis(100);
const DAEMON_TEST_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(windows)]
const DAEMON_TEST_READY_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(not(windows))]
const DAEMON_TEST_READY_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
const DAEMON_TEST_READY_CONTROL_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(windows)]
const DAEMON_TEST_SYNC_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(not(windows))]
const DAEMON_TEST_SYNC_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(windows)]
const DAEMON_TEST_SYNC_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
#[cfg(not(windows))]
const DAEMON_TEST_SYNC_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
const DAEMON_TEST_TRACE_READY_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(windows)]
const DAEMON_TEST_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(windows)]
const TEST_SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(not(windows))]
const TEST_SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonTestScope {
    Shared,
    Dedicated,
    /// Create a repo configured for daemon mode but do NOT auto-start a daemon.
    /// Use this for tests that manually manage their own daemon lifecycle.
    NoDaemon,
}

#[derive(Debug)]
pub struct TestRepo {
    path: PathBuf,
    pub feature_flags: FeatureFlags,
    pub(crate) config_patch: Option<ConfigPatch>,
    test_db_path: PathBuf,
    test_home: PathBuf,
    daemon_scope: DaemonTestScope,
    daemon_process: Option<Arc<DaemonProcess>>,
    /// When this TestRepo is backed by a linked worktree, holds the base repo path
    /// so we can clean it up on drop.
    _base_repo_path: Option<PathBuf>,
    /// Base repo's test DB path for cleanup.
    _base_test_db_path: Option<PathBuf>,
    daemon_family_key: OnceLock<String>,
}

#[allow(dead_code)]
impl Default for TestRepo {
    fn default() -> Self {
        Self::new()
    }
}
