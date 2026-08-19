//! Bash tool change attribution via pre/post stat-tuple snapshots.
//!
//! Detects file changes made by bash/shell tool calls by comparing filesystem
//! metadata snapshots taken before and after tool execution.

mod daemon_api;
mod hooks;
mod path_filter;
mod snapshot;
mod tool_class;
mod types;

#[cfg(test)]
mod unit_tests;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Grace window for low-resolution filesystem detection (seconds).
#[cfg(not(any(test, feature = "test-support")))]
const MTIME_GRACE_WINDOW_SECS: u64 = 2;
#[cfg(any(test, feature = "test-support"))]
const MTIME_GRACE_WINDOW_SECS: u64 = 0;

/// Hard limit for the filesystem stat-diff walk.  If the walk exceeds this,
/// the snapshot is abandoned (returning Err) and the hook falls back gracefully.
const WALK_TIMEOUT_MS: u64 = 1500;

/// Hard limit for the entire post-hook execution.  If this is exceeded
/// at any checkpoint, the hook returns HookTimeout immediately.
const HOOK_TIMEOUT_MS: u64 = 4000;

/// Grace window in nanoseconds for low-resolution filesystem mtime comparison.
pub(crate) const MTIME_GRACE_WINDOW_NS: u128 = (MTIME_GRACE_WINDOW_SECS as u128) * 1_000_000_000;

/// Maximum number of files to track in a snapshot.  Repos larger than this
/// skip the stat-diff system entirely (returning SnapshotFailed) to avoid adding
/// seconds of latency to every Bash tool call.
pub(crate) const MAX_TRACKED_FILES: usize = 50_000;

// ---------------------------------------------------------------------------
// Test-only timeout overrides (thread-local so parallel tests don't interfere)
// ---------------------------------------------------------------------------

// Thread-local overrides for WALK_TIMEOUT_MS and HOOK_TIMEOUT_MS, injected
// by tests via `set_walk_timeout_ms_for_test` / `set_hook_timeout_ms_for_test`.
// Setting either to 0 causes the corresponding timeout to fire immediately.
// Thread-local (not global) so parallel tests in other modules are unaffected.
#[cfg(any(test, feature = "test-support"))]
std::thread_local! {
    pub(crate) static TEST_WALK_TIMEOUT_MS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    pub(crate) static TEST_HOOK_TIMEOUT_MS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    pub(crate) static TEST_DAEMON_SOCKET: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Return the walk timeout, honouring any test-time thread-local override.
pub(crate) fn effective_walk_timeout_ms() -> u64 {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(v) = TEST_WALK_TIMEOUT_MS.with(|c| c.get()) {
        return v;
    }
    WALK_TIMEOUT_MS
}

/// Return the hook timeout, honouring any test-time thread-local override.
pub(crate) fn effective_hook_timeout_ms() -> u64 {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(v) = TEST_HOOK_TIMEOUT_MS.with(|c| c.get()) {
        return v;
    }
    HOOK_TIMEOUT_MS
}

/// Override the walk timeout for the current thread.  Call
/// `reset_timeout_overrides_for_test()` at the end of the test.
#[cfg(any(test, feature = "test-support"))]
pub fn set_walk_timeout_ms_for_test(ms: u64) {
    TEST_WALK_TIMEOUT_MS.with(|c| c.set(Some(ms)));
}

/// Override the hook timeout for the current thread.  Call
/// `reset_timeout_overrides_for_test()` at the end of the test.
#[cfg(any(test, feature = "test-support"))]
pub fn set_hook_timeout_ms_for_test(ms: u64) {
    TEST_HOOK_TIMEOUT_MS.with(|c| c.set(Some(ms)));
}

/// Override the daemon control socket path for the current thread.
/// This avoids process-global env vars that race in parallel tests.
#[cfg(any(test, feature = "test-support"))]
pub fn set_daemon_socket_for_test(path: std::path::PathBuf) {
    TEST_DAEMON_SOCKET.with(|c| c.borrow_mut().replace(path));
}

/// Clear test-time timeout overrides for the current thread.
/// Does NOT clear the daemon socket override — that is managed separately
/// via `set_daemon_socket_for_test`.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_timeout_overrides_for_test() {
    TEST_WALK_TIMEOUT_MS.with(|c| c.set(None));
    TEST_HOOK_TIMEOUT_MS.with(|c| c.set(None));
}

// ---------------------------------------------------------------------------
// Public re-exports (compatibility surface — no behavior change)
// ---------------------------------------------------------------------------

pub use daemon_api::{
    BashHookAttemptPhase, BashHookAttemptSignal, BashToolHookContext, DaemonWatermarks,
    signal_daemon_bash_hook_attempt,
};
pub use hooks::{
    handle_bash_post_tool_use, handle_bash_post_tool_use_with_cwd,
    handle_bash_pre_tool_use_with_context, handle_bash_pre_tool_use_with_context_and_cwd,
};
pub use path_filter::{
    build_gitignore, git_index_mtime_ns, normalize_path, should_include_new_file,
};
pub use snapshot::{diff, git_status_fallback, snapshot};
pub use tool_class::{Agent, classify_optional_tool, classify_tool};
pub use types::{
    BashCheckpointAction, BashPostHookResult, BashPreHookResult, StatDiffResult, StatEntry,
    StatFileType, StatSnapshot, ToolClass,
};
