//! Bash tool change attribution via pre/post stat-tuple snapshots.
//!
//! Detects file changes made by bash/shell tool calls by comparing filesystem
//! metadata snapshots taken before and after tool execution.

use crate::authorship::ignore::{
    default_ignore_patterns, load_git_ai_ignore_patterns_from_path,
    load_linguist_generated_patterns_from_path,
};
use crate::authorship::working_log::AgentId;
use crate::daemon::control_api::{BashSnapshotQueryResponse, ControlRequest};
use crate::daemon::{DaemonConfig, send_control_request, send_control_request_with_timeout};
use crate::error::GitAiError;
use crate::utils::normalize_to_posix;
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

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

// ---------------------------------------------------------------------------
// Test-only timeout overrides (thread-local so parallel tests don't interfere)
// ---------------------------------------------------------------------------

// Thread-local overrides for WALK_TIMEOUT_MS and HOOK_TIMEOUT_MS, injected
// by tests via `set_walk_timeout_ms_for_test` / `set_hook_timeout_ms_for_test`.
// Setting either to 0 causes the corresponding timeout to fire immediately.
// Thread-local (not global) so parallel tests in other modules are unaffected.
#[cfg(any(test, feature = "test-support"))]
std::thread_local! {
    static TEST_WALK_TIMEOUT_MS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static TEST_HOOK_TIMEOUT_MS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static TEST_DAEMON_SOCKET: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Return the walk timeout, honouring any test-time thread-local override.
fn effective_walk_timeout_ms() -> u64 {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(v) = TEST_WALK_TIMEOUT_MS.with(|c| c.get()) {
        return v;
    }
    WALK_TIMEOUT_MS
}

/// Return the hook timeout, honouring any test-time thread-local override.
fn effective_hook_timeout_ms() -> u64 {
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

/// Resolve the daemon control socket path, preferring the thread-local test
/// override over the env-based `DaemonConfig`.
fn effective_daemon_socket() -> Option<std::path::PathBuf> {
    #[cfg(any(test, feature = "test-support"))]
    {
        let tl = TEST_DAEMON_SOCKET.with(|c| c.borrow().clone());
        if tl.is_some() {
            return tl;
        }
    }
    DaemonConfig::from_env_or_default_paths()
        .ok()
        .map(|c| c.control_socket_path)
}

/// Grace window in nanoseconds for low-resolution filesystem mtime comparison.
const MTIME_GRACE_WINDOW_NS: u128 = (MTIME_GRACE_WINDOW_SECS as u128) * 1_000_000_000;

/// Maximum number of files to track in a snapshot.  Repos larger than this
/// skip the stat-diff system entirely (returning SnapshotFailed) to avoid adding
/// seconds of latency to every Bash tool call.
const MAX_TRACKED_FILES: usize = 50_000;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// Metadata fingerprint for a single file, collected via `lstat()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatEntry {
    pub exists: bool,
    pub mtime: Option<SystemTime>,
    pub ctime: Option<SystemTime>,
    pub size: u64,
    pub mode: u32,
    pub file_type: StatFileType,
}

/// File type enumeration (symlink-aware, no following).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatFileType {
    Regular,
    Directory,
    Symlink,
    Other,
}

impl StatEntry {
    /// Build a `StatEntry` from `std::fs::Metadata` (from `symlink_metadata` / `lstat`).
    pub fn from_metadata(meta: &fs::Metadata) -> Self {
        let file_type = if meta.file_type().is_symlink() {
            StatFileType::Symlink
        } else if meta.file_type().is_dir() {
            StatFileType::Directory
        } else if meta.file_type().is_file() {
            StatFileType::Regular
        } else {
            StatFileType::Other
        };

        let mtime = meta.modified().ok();
        let size = meta.len();
        let mode = Self::extract_mode(meta);
        let ctime = Self::extract_ctime(meta);

        StatEntry {
            exists: true,
            mtime,
            ctime,
            size,
            mode,
            file_type,
        }
    }

    #[cfg(unix)]
    fn extract_mode(meta: &fs::Metadata) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode()
    }

    #[cfg(not(unix))]
    fn extract_mode(meta: &fs::Metadata) -> u32 {
        if meta.permissions().readonly() {
            0o444
        } else {
            0o644
        }
    }

    #[cfg(unix)]
    fn extract_ctime(meta: &fs::Metadata) -> Option<SystemTime> {
        use std::os::unix::fs::MetadataExt;
        let ctime_secs = meta.ctime();
        let ctime_nsecs = meta.ctime_nsec() as u32;
        if ctime_secs >= 0 {
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::new(ctime_secs as u64, ctime_nsecs))
        } else {
            None
        }
    }

    #[cfg(not(unix))]
    fn extract_ctime(meta: &fs::Metadata) -> Option<SystemTime> {
        // On Windows, use creation time as a proxy for ctime
        meta.created().ok()
    }
}

/// A complete filesystem snapshot: stat-tuples keyed by normalized path.
///
/// Only stores entries for files that pass the git-ai ignore filter AND have
/// `mtime > effective_worktree_wm + GRACE` (i.e., not covered by any watermark).
/// Filtering is applied uniformly to all files — there is no special treatment
/// for git-tracked vs untracked files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatSnapshot {
    /// File metadata for files that passed the ignore filter and are not
    /// covered by any watermark at snapshot time.
    pub entries: HashMap<PathBuf, StatEntry>,
    /// When this snapshot was taken.
    #[serde(skip)]
    pub taken_at: Option<Instant>,
    /// Unique invocation key: "{session_id}:{tool_use_id}".
    pub invocation_key: String,
    /// Repo root path.
    pub repo_root: PathBuf,
    /// Effective worktree-level watermark at snapshot time.
    /// Either the real daemon worktree watermark (warm start) or the mtime
    /// of `.git/index` (cold-start proxy).  `None` if neither was available.
    #[serde(default)]
    pub effective_worktree_wm: Option<u128>,
    /// Per-file watermarks from the daemon at snapshot time.
    /// Used for Tier-1 stale detection in `find_stale_files`.
    #[serde(default)]
    pub per_file_wm: HashMap<String, u128>,
}

/// Result of diffing two snapshots.
#[derive(Debug, Default)]
pub struct StatDiffResult {
    pub created: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
}

impl StatDiffResult {
    /// All changed paths (created + modified) as Strings.
    pub fn all_changed_paths(&self) -> Vec<String> {
        self.created
            .iter()
            .chain(self.modified.iter())
            .map(|p| normalize_to_posix(&p.to_string_lossy()))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.created.is_empty() && self.modified.is_empty()
    }
}

/// What the bash post-hook decided to do.
#[derive(Debug)]
pub enum BashCheckpointAction {
    /// Files changed — emit a checkpoint with these paths.
    Checkpoint(Vec<String>),
    /// Stat-diff ran but found nothing.
    NoChanges,
    /// The post-hook exceeded its time budget.
    HookTimeout,
    /// The post-snapshot filesystem walk failed (walk timeout, too many files, IO error).
    SnapshotFailed,
    /// The daemon had no pre-snapshot for this tool-use ID.
    MissingPreSnapshot,
}

/// Result from `handle_bash_pre_tool_use_with_context`.
pub struct BashPreHookResult {
    /// Files with mtime > watermark at pre-snapshot time (absolute paths).
    pub dirty_paths: Vec<PathBuf>,
}

/// Result from `handle_bash_post_tool_use`.
pub struct BashPostHookResult {
    /// The checkpoint action.
    pub action: BashCheckpointAction,
}

/// Per-agent tool classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// A known file-edit tool (Write, Edit, etc.) — handled by existing preset logic.
    FileEdit,
    /// A bash/shell tool — handled by the stat-diff system.
    Bash,
    /// Unrecognized tool — skip checkpoint.
    Skip,
}

// ---------------------------------------------------------------------------
// Tool classification per agent (Section 8.2 of PRD)
// ---------------------------------------------------------------------------

/// Strip the `functions.` namespace prefix that Codex/OpenAI function-call
/// tools add to hook tool names (e.g. `functions.apply_patch` -> `apply_patch`).
fn normalize_tool_name(tool_name: &str) -> &str {
    tool_name.strip_prefix("functions.").unwrap_or(tool_name)
}

/// Classify a tool name for a given agent.
pub fn classify_tool(agent: Agent, tool_name: &str) -> ToolClass {
    match agent {
        Agent::Claude => match tool_name {
            "Write" | "Edit" | "MultiEdit" => ToolClass::FileEdit,
            "Bash" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Gemini => match tool_name {
            "write_file" | "replace" => ToolClass::FileEdit,
            "shell" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::ContinueCli => match tool_name {
            "edit" => ToolClass::FileEdit,
            "terminal" | "local_shell_call" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Droid => match tool_name {
            "ApplyPatch" | "Edit" | "Write" | "Create" => ToolClass::FileEdit,
            "Bash" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Amp => match tool_name {
            "Write" | "Edit" => ToolClass::FileEdit,
            "Bash" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::OpenCode => match tool_name {
            "edit" | "write" => ToolClass::FileEdit,
            "bash" | "shell" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Firebender => match tool_name {
            "Write" | "Edit" | "Delete" | "RenameSymbol" | "DeleteSymbol" => ToolClass::FileEdit,
            "Bash" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Codex => {
            // Codex Desktop (OpenAI function-calling) prefixes tool names with
            // "functions."; strip it before matching the bare tool name.
            let tool_name = normalize_tool_name(tool_name);
            match tool_name {
                "apply_patch" => ToolClass::FileEdit,
                "Bash" | "exec_command" | "shell" | "shell_command" => ToolClass::Bash,
                "multi_tool_use.parallel" => ToolClass::Bash,
                _ => ToolClass::Skip,
            }
        }
        Agent::Pi => match tool_name {
            "edit" | "write" | "replace" | "rename" => ToolClass::FileEdit,
            "bash" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Windsurf => match tool_name {
            "code_action" => ToolClass::FileEdit,
            "run_command" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Cursor => match tool_name {
            "Write" | "Delete" | "StrReplace" | "ApplyPatch" => ToolClass::FileEdit,
            "Shell" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Cline => {
            let tool_name = normalize_tool_name(tool_name);
            match tool_name {
                "replace_in_file" | "write_to_file" | "apply_patch" | "editor" | "edit"
                | "write" => ToolClass::FileEdit,
                "execute_command" | "bash" | "shell" | "run_commands" | "run_command" => {
                    ToolClass::Bash
                }
                _ => ToolClass::Skip,
            }
        }
    }
}

/// Supported AI agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Gemini,
    ContinueCli,
    Droid,
    Amp,
    OpenCode,
    Firebender,
    Codex,
    Pi,
    Windsurf,
    Cursor,
    Cline,
}

// ---------------------------------------------------------------------------
// Path normalization
// ---------------------------------------------------------------------------

/// Normalize a path for use as HashMap key.
/// On case-insensitive filesystems (macOS, Windows), case-fold to lowercase.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn normalize_path(p: &Path) -> PathBuf {
    PathBuf::from(p.to_string_lossy().to_lowercase())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn normalize_path(p: &Path) -> PathBuf {
    p.to_path_buf()
}

// ---------------------------------------------------------------------------
// Git-dir / index helpers
// ---------------------------------------------------------------------------

/// Resolve the `.git` directory path for a repo (handles worktrees).
fn get_git_dir(repo_root: &Path) -> Result<PathBuf, GitAiError> {
    let args = vec![
        "-C".to_string(),
        repo_root.to_string_lossy().into_owned(),
        "rev-parse".to_string(),
        "--git-dir".to_string(),
    ];
    let output = crate::git::repository::exec_git_allow_nonzero(&args)?;
    if !output.status.success() {
        return Err(GitAiError::Generic(
            "git rev-parse --git-dir failed".to_string(),
        ));
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if Path::new(&s).is_absolute() {
        Ok(PathBuf::from(s))
    } else {
        Ok(repo_root.join(s))
    }
}

/// Return the mtime of `.git/index` as nanoseconds since the UNIX epoch.
///
/// Used as a cold-start watermark proxy when the daemon has no worktree
/// watermark yet.  Only called when `wm = Some(w)` with `w.worktree = None`,
/// so passing `wm = None` (tests, non-daemon mode) always bypasses this.
pub fn git_index_mtime_ns(repo_root: &Path) -> Option<u128> {
    let git_dir = get_git_dir(repo_root).ok()?;
    let index_path = git_dir.join("index");
    let mtime = fs::metadata(&index_path).ok()?.modified().ok()?;
    Some(system_time_to_nanos(mtime))
}

/// Test whether a file is covered by the current watermarks, meaning it has
/// not been modified since the last known-good baseline and does not need to
/// be stored in the snapshot.
///
/// A file is covered when:
/// - It has a per-file watermark AND `mtime ≤ file_wm + GRACE`, OR
/// - No per-file watermark but an effective worktree wm exists AND
///   `mtime ≤ effective_wm + GRACE`.
fn is_wm_covered(
    mtime_ns: u128,
    effective_wm: Option<u128>,
    per_file_wm: &HashMap<String, u128>,
    posix_key: &str,
) -> bool {
    if let Some(&file_wm) = per_file_wm.get(posix_key) {
        return mtime_ns <= file_wm + MTIME_GRACE_WINDOW_NS;
    }
    effective_wm.is_some_and(|ewm| mtime_ns <= ewm + MTIME_GRACE_WINDOW_NS)
}

// ---------------------------------------------------------------------------
// Path filtering
// ---------------------------------------------------------------------------

/// Build the git-ai ignore ruleset for use in `filter_entry` on the snapshot walker.
///
/// Only covers the git-ai-specific patterns:
/// - Default ignore patterns (lock files, node_modules, etc.)
/// - Patterns from `.git-ai-ignore` at the repo root
/// - Linguist-generated patterns from `.gitattributes` at the repo root
///
/// Standard `.gitignore` handling — including nested `.gitignore` files throughout
/// the repo tree — is left to `WalkBuilder` with `git_ignore(true)`, which discovers
/// and applies them natively as it descends. Adding them here too would be redundant
/// and would require a separate pre-walk that can't apply rules during traversal.
pub fn build_gitignore(repo_root: &Path) -> Result<Gitignore, GitAiError> {
    let mut builder = GitignoreBuilder::new(repo_root);

    // git-ai-specific patterns: same source of truth as non-bash checkpoints.
    let shared_patterns: Vec<String> = default_ignore_patterns()
        .into_iter()
        .chain(load_git_ai_ignore_patterns_from_path(repo_root))
        .chain(load_linguist_generated_patterns_from_path(repo_root))
        .collect();
    for pattern in &shared_patterns {
        if let Err(e) = builder.add_line(None, pattern) {
            tracing::debug!("Warning: failed to add ignore pattern '{}': {}", pattern, e);
        }
    }

    builder
        .build()
        .map_err(|e| GitAiError::Generic(format!("Failed to build gitignore rules: {}", e)))
}

/// Check whether a newly created (untracked) file should be included.
/// Returns true if the file is NOT ignored by .gitignore rules.
pub fn should_include_new_file(gitignore: &Gitignore, path: &Path, is_dir: bool) -> bool {
    // Use matched_path_or_any_parents so directory patterns like `secrets/` also
    // exclude files nested inside that directory (e.g. `secrets/token.txt`).
    let matched = gitignore.matched_path_or_any_parents(path, is_dir);
    !matched.is_ignore()
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Take a stat snapshot of the repo working tree.
///
/// Only stores entries for files that pass the git-ai ignore filter (gitignore
/// + defaults + .git-ai-ignore + linguist) AND have `mtime > effective_wm + GRACE`.
///
/// Filtering is applied uniformly to all files — there is no special treatment
/// for git-tracked vs untracked files.
///
/// `wm` should be the result of a recent daemon watermark query.  Pass
/// `None` to skip watermark filtering entirely (no daemon context, or direct
/// `snapshot()` callers such as tests and `git_status_fallback`).
pub fn snapshot(
    repo_root: &Path,
    session_id: &str,
    tool_use_id: &str,
    wm: Option<&DaemonWatermarks>,
) -> Result<StatSnapshot, GitAiError> {
    let start = Instant::now();
    let invocation_key = format!("{}:{}", session_id, tool_use_id);

    // Compute the effective worktree-level watermark:
    //   wm = Some(w) with real worktree wm → use it directly (warm start).
    //   wm = Some(w) with no worktree wm → daemon up but hasn't seen a full
    //                Human checkpoint yet; use .git/index mtime as proxy.
    //   wm = None   → no filtering (caller opted out or direct snapshot() call
    //                without daemon context).
    //
    // Note: the cold-start proxy (git_index_mtime_ns) is injected by
    // handle_bash_pre_tool_use_with_context when no daemon is running, not here, so direct
    // snapshot() callers (e.g. tests, git_status_fallback) are unaffected.
    let effective_worktree_wm: Option<u128> = match wm {
        Some(w) if w.worktree.is_some() => w.worktree,
        Some(_) => git_index_mtime_ns(repo_root),
        None => None,
    };

    let per_file_wm: HashMap<String, u128> = wm.map(|w| w.per_file.clone()).unwrap_or_default();

    // Build the git-ai ignore ruleset: gitignore + defaults + .git-ai-ignore + linguist.
    // Arc is needed because filter_entry requires 'static, preventing a borrow.
    // The closure takes sole ownership; no post-walker use of the ruleset is needed.
    let gitignore_filter = Arc::new(build_gitignore(repo_root)?);

    let mut entries = HashMap::new();

    // Pass the git-ai ignore ruleset directly into the walker via filter_entry.
    // This prunes entire ignored directories (node_modules/, target/, etc.)
    // before the walker descends into them — including directories that are in
    // default_ignore_patterns() but not yet in the repo's .gitignore (a common
    // case for node_modules that the user hasn't gitignored yet).
    // git_ignore(true) handles the standard .gitignore case; filter_entry
    // catches the rest (defaults, .git-ai-ignore, linguist-generated).
    let repo_root_buf = repo_root.to_path_buf();
    let walker = WalkBuilder::new(repo_root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(move |entry| {
            if entry.file_name() == ".git" {
                return false;
            }
            let abs = entry.path();
            let Ok(rel) = abs.strip_prefix(&repo_root_buf) else {
                return true; // outside repo root — let walker handle it
            };
            if rel.as_os_str().is_empty() {
                return true; // repo root itself — always include
            }
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            should_include_new_file(&gitignore_filter, rel, is_dir)
        })
        .build();

    let walk_timeout = Duration::from_millis(effective_walk_timeout_ms());
    for result in walker {
        let elapsed = start.elapsed();
        if elapsed >= walk_timeout {
            let elapsed_ms = elapsed.as_millis();
            let timeout_ms = walk_timeout.as_millis();
            let msg = format!(
                "bash_tool: snapshot walk exceeded {}ms limit ({}ms elapsed, {} entries so far); abandoning stat-diff",
                timeout_ms,
                elapsed_ms,
                entries.len()
            );
            tracing::debug!("{}", msg);
            crate::observability::log_message(
                &msg,
                "warning",
                Some(serde_json::json!({
                    "elapsed_ms": elapsed_ms,
                    "entries_so_far": entries.len(),
                    "walk_timeout_ms": timeout_ms,
                })),
            );
            return Err(GitAiError::Generic(msg));
        }

        let entry = match result {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!("Walker error: {}", e);
                continue;
            }
        };

        let abs_path = entry.path();

        // Skip directories — filter_entry already pruned ignored dirs; this
        // guard drops any remaining directory entries (e.g. the repo root).
        if entry
            .file_type()
            .map(|ft| ft.is_dir())
            .unwrap_or_else(|| abs_path.is_dir())
        {
            continue;
        }

        let rel_path = match abs_path.strip_prefix(repo_root) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // filter_entry already applied should_include_new_file for files too,
        // so no secondary check is needed here.

        let normalized = normalize_path(rel_path);

        match fs::symlink_metadata(abs_path) {
            Ok(meta) => {
                let stat = StatEntry::from_metadata(&meta);
                let mtime_ns = stat.mtime.map(system_time_to_nanos).unwrap_or(0);
                let posix_key = normalize_to_posix(&normalized.to_string_lossy());
                if !is_wm_covered(mtime_ns, effective_worktree_wm, &per_file_wm, &posix_key) {
                    entries.insert(normalized, stat);
                    if entries.len() > MAX_TRACKED_FILES {
                        tracing::debug!(
                            "Snapshot: exceeded MAX_TRACKED_FILES ({}), skipping stat-diff",
                            MAX_TRACKED_FILES
                        );
                        return Err(GitAiError::Generic(format!(
                            "repo has more than {} recently-modified files; skipping stat-diff",
                            MAX_TRACKED_FILES
                        )));
                    }
                }
            }
            Err(e) => {
                tracing::debug!("Failed to stat {}: {}", abs_path.display(), e);
            }
        }
    }

    tracing::debug!(
        "Snapshot: {} files scanned in {}ms",
        entries.len(),
        start.elapsed().as_millis()
    );

    Ok(StatSnapshot {
        entries,
        taken_at: Some(Instant::now()),
        invocation_key,
        repo_root: repo_root.to_path_buf(),
        effective_worktree_wm,
        per_file_wm,
    })
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

/// Diff two snapshots to find created and modified files.
///
/// Both snapshots apply the same git-ai ignore filter at snapshot time, so
/// any file in `post.entries` already passed that filter. No secondary
/// filtering is needed here.
///
/// Files in post but not pre are reported as **created** (either genuinely
/// new, or previously wm-covered and now modified by bash — both are changed
/// files that need attribution).  Files in both with a changed stat-tuple are
/// reported as **modified**.  Deletions are not tracked.
pub fn diff(pre: &StatSnapshot, post: &StatSnapshot) -> StatDiffResult {
    let mut result = StatDiffResult::default();

    // Files in post but not pre: new files or previously wm-covered files
    // now modified by bash. Both need attribution; the distinction doesn't
    // matter since all_changed_paths() merges created + modified.
    for path in post.entries.keys() {
        if !pre.entries.contains_key(path) {
            result.created.push(path.clone());
        }
    }

    // Files in both but stat-tuple differs.
    for (path, post_entry) in &post.entries {
        if let Some(pre_entry) = pre.entries.get(path)
            && pre_entry != post_entry
        {
            result.modified.push(path.clone());
        }
    }

    result.created.sort();
    result.modified.sort();

    result
}

// ---------------------------------------------------------------------------
// Git status fallback
// ---------------------------------------------------------------------------

/// Fall back to `git status --porcelain=v2` to detect changed files.
/// Used when the pre-snapshot is lost (process restart) or on very large repos.
fn git_status_fallback_args(repo_root: &Path) -> Vec<String> {
    vec![
        "-C".to_string(),
        repo_root.to_string_lossy().into_owned(),
        "--no-optional-locks".to_string(),
        "status".to_string(),
        "--porcelain=v2".to_string(),
        "-z".to_string(),
        "--untracked-files=all".to_string(),
    ]
}

pub fn git_status_fallback(repo_root: &Path) -> Result<Vec<String>, GitAiError> {
    let args = git_status_fallback_args(repo_root);
    let output = crate::git::repository::exec_git_allow_nonzero(&args)?;

    if !output.status.success() {
        return Err(GitAiError::Generic(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let mut changed_files = Vec::new();
    let parts: Vec<&[u8]> = output.stdout.split(|&b| b == 0).collect();
    let mut i = 0;
    while i < parts.len() {
        let part = parts[i];
        if part.is_empty() {
            i += 1;
            continue;
        }

        let line = String::from_utf8_lossy(part);

        if line.starts_with("1 ") || line.starts_with("u ") {
            // Ordinary entry: 8 fields before path; unmerged: 10 fields before path
            let n = if line.starts_with("u ") { 11 } else { 9 };
            let fields: Vec<&str> = line.splitn(n, ' ').collect();
            if let Some(path) = fields.last() {
                changed_files.push(normalize_to_posix(path));
            }
        } else if line.starts_with("2 ") {
            // Rename/copy: 9 fields before new path, then NUL-delimited original path
            let fields: Vec<&str> = line.splitn(10, ' ').collect();
            if let Some(path) = fields.last() {
                changed_files.push(normalize_to_posix(path));
            }
            // Also include the original path (next NUL-delimited entry)
            if i + 1 < parts.len() {
                let orig = String::from_utf8_lossy(parts[i + 1]);
                if !orig.is_empty() {
                    changed_files.push(normalize_to_posix(&orig));
                }
            }
            i += 1;
        } else if let Some(path) = line.strip_prefix("? ") {
            // Untracked: path follows "? "
            changed_files.push(normalize_to_posix(path));
        }

        i += 1;
    }

    Ok(changed_files)
}

// ---------------------------------------------------------------------------
// Captured-checkpoint helpers
// ---------------------------------------------------------------------------

/// Convert a `SystemTime` to nanoseconds since UNIX epoch for watermark comparison.
fn system_time_to_nanos(t: SystemTime) -> u128 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

// ---------------------------------------------------------------------------
// Daemon watermark query + stale file detection
// ---------------------------------------------------------------------------

/// Query the daemon for per-file mtime watermarks for a given repository.
///
/// Returns `None` on any failure (daemon not running, socket error, parse
/// error, etc.) for graceful degradation — the caller simply skips the
/// captured-checkpoint path when watermarks are unavailable.
/// Watermarks returned by the daemon for a single worktree.
pub struct DaemonWatermarks {
    /// Per-file mtime watermarks from scoped checkpoints.
    per_file: HashMap<String, u128>,
    /// Timestamp of the last full (non-scoped) Human checkpoint, if any.
    /// `None` on cold start (daemon has never processed a full checkpoint).
    worktree: Option<u128>,
}

fn query_daemon_watermarks(repo_working_dir: &str) -> Option<DaemonWatermarks> {
    let socket = effective_daemon_socket()?;
    if !socket.exists() {
        return None;
    }
    let request = ControlRequest::SnapshotWatermarks {
        repo_working_dir: repo_working_dir.to_string(),
    };
    let response =
        send_control_request_with_timeout(&socket, &request, Duration::from_millis(500)).ok()?;

    if !response.ok {
        tracing::debug!(
            "Daemon watermark query returned error: {}",
            response.error.as_deref().unwrap_or("unknown")
        );
        return None;
    }

    let data = response.data?;
    let per_file: HashMap<String, u128> = data
        .get("watermarks")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let worktree: Option<u128> = data
        .get("worktree_watermark")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    if per_file.is_empty() && worktree.is_none() {
        return None;
    }
    Some(DaemonWatermarks { per_file, worktree })
}

// ---------------------------------------------------------------------------
// Daemon-based bash session query
// ---------------------------------------------------------------------------

/// Query the daemon for the pre-snapshot stored during `BashSessionStart`.
///
/// Returns `None` if the daemon is not running, the session is not found,
/// or any communication error occurs.
fn query_daemon_bash_snapshot(session_id: &str, tool_use_id: &str) -> Option<StatSnapshot> {
    let socket = effective_daemon_socket()?;
    if !socket.exists() {
        return None;
    }
    let request = ControlRequest::BashSnapshotQuery {
        session_id: session_id.to_string(),
        tool_use_id: tool_use_id.to_string(),
    };
    let response =
        send_control_request_with_timeout(&socket, &request, Duration::from_millis(500)).ok()?;

    if !response.ok {
        tracing::debug!(
            "Daemon bash snapshot query returned error: {}",
            response.error.as_deref().unwrap_or("unknown")
        );
        return None;
    }

    let data = response.data?;
    let snapshot_response: BashSnapshotQueryResponse = serde_json::from_value(data).ok()?;
    snapshot_response.stat_snapshot
}

#[derive(Debug, Clone, Copy)]
pub enum BashHookAttemptPhase {
    Start,
    End,
}

pub struct BashHookAttemptSignal<'a> {
    pub original_cwd: &'a Path,
    pub discovered_repo_work_dir: Option<&'a Path>,
    pub repo_discovery_error: Option<&'a str>,
    pub session_id: &'a str,
    pub tool_use_id: &'a str,
    pub agent_id: &'a AgentId,
    pub metadata: &'a HashMap<String, String>,
    pub trace_id: &'a str,
    pub timestamp_ns: u128,
    pub command: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct BashToolHookContext<'a> {
    pub session_id: &'a str,
    pub tool_use_id: &'a str,
    pub agent_id: &'a AgentId,
    pub agent_metadata: Option<&'a HashMap<String, String>>,
    pub trace_id: &'a str,
    pub command: Option<&'a str>,
}

pub fn signal_daemon_bash_hook_attempt(
    phase: BashHookAttemptPhase,
    signal: BashHookAttemptSignal<'_>,
) {
    let Some(socket) = effective_daemon_socket() else {
        return;
    };
    if !socket.exists() {
        return;
    }
    let original_cwd = signal.original_cwd.to_string_lossy().to_string();
    let discovered_repo_work_dir = signal
        .discovered_repo_work_dir
        .map(|path| path.to_string_lossy().to_string());
    let repo_discovery_error = signal.repo_discovery_error.map(ToString::to_string);
    let session_id = signal.session_id.to_string();
    let tool_use_id = signal.tool_use_id.to_string();
    let agent_id = signal.agent_id.clone();
    let metadata = signal.metadata.clone();
    let trace_id = signal.trace_id.to_string();
    let command = signal.command.map(ToString::to_string);

    let request = match phase {
        BashHookAttemptPhase::Start => ControlRequest::BashHookAttemptStart {
            original_cwd,
            discovered_repo_work_dir,
            repo_discovery_error,
            session_id,
            tool_use_id,
            agent_id,
            metadata,
            trace_id,
            started_at_ns: signal.timestamp_ns,
            command,
        },
        BashHookAttemptPhase::End => ControlRequest::BashHookAttemptEnd {
            original_cwd,
            discovered_repo_work_dir,
            repo_discovery_error,
            session_id,
            tool_use_id,
            agent_id,
            metadata,
            trace_id,
            ended_at_ns: signal.timestamp_ns,
            command,
        },
    };
    if let Err(e) = send_control_request_with_timeout(&socket, &request, Duration::from_millis(500))
    {
        tracing::debug!("Failed to signal bash hook attempt {:?}: {}", phase, e);
    }
}

struct BashSessionEndSignal<'a> {
    repo_work_dir: &'a str,
    original_cwd: &'a Path,
    session_id: &'a str,
    tool_use_id: &'a str,
    agent_id: &'a AgentId,
    metadata: &'a HashMap<String, String>,
    trace_id: &'a str,
    ended_at_ns: u128,
    command: Option<&'a str>,
}

/// Signal the daemon that a bash session has ended.
fn signal_daemon_bash_session_end(signal: BashSessionEndSignal<'_>) {
    let Some(socket) = effective_daemon_socket() else {
        return;
    };
    if !socket.exists() {
        return;
    }
    let request = ControlRequest::BashSessionEnd {
        repo_work_dir: signal.repo_work_dir.to_string(),
        original_cwd: Some(signal.original_cwd.to_string_lossy().to_string()),
        session_id: signal.session_id.to_string(),
        tool_use_id: signal.tool_use_id.to_string(),
        agent_id: signal.agent_id.clone(),
        metadata: signal.metadata.clone(),
        trace_id: signal.trace_id.to_string(),
        ended_at_ns: signal.ended_at_ns,
        command: signal.command.map(ToString::to_string),
    };
    if let Err(e) = send_control_request_with_timeout(&socket, &request, Duration::from_millis(500))
    {
        tracing::debug!("Failed to signal bash session end: {}", e);
    }
}

// ---------------------------------------------------------------------------
// Pre/post hook orchestration
// ---------------------------------------------------------------------------

/// Handle the pre-tool-use hook with full agent context.
///
/// Takes a filesystem snapshot and sends it to the daemon via `BashSessionStart`.
/// The daemon stores the snapshot in memory for retrieval at post-hook time.
pub fn handle_bash_pre_tool_use_with_context(
    repo_root: &Path,
    session_id: &str,
    tool_use_id: &str,
    agent_id: &AgentId,
    agent_metadata: Option<&HashMap<String, String>>,
    trace_id: &str,
    command: Option<&str>,
) -> Result<BashPreHookResult, GitAiError> {
    handle_bash_pre_tool_use_with_context_and_cwd(
        repo_root,
        repo_root,
        BashToolHookContext {
            session_id,
            tool_use_id,
            agent_id,
            agent_metadata,
            trace_id,
            command,
        },
    )
}

/// Handle the pre-tool-use hook with a separately tracked original cwd.
pub fn handle_bash_pre_tool_use_with_context_and_cwd(
    repo_root: &Path,
    original_cwd: &Path,
    context: BashToolHookContext<'_>,
) -> Result<BashPreHookResult, GitAiError> {
    let started_at_ns = crate::daemon::bash_history_db::unix_time_ns();
    let repo_working_dir = repo_root.to_string_lossy().to_string();

    let wm = query_daemon_watermarks(&repo_working_dir).or_else(|| {
        git_index_mtime_ns(repo_root).map(|ts| DaemonWatermarks {
            per_file: HashMap::new(),
            worktree: Some(ts),
        })
    });
    let snap = snapshot(
        repo_root,
        context.session_id,
        context.tool_use_id,
        wm.as_ref(),
    )?;

    // When watermarks are unavailable (no daemon + no .git/index), the snapshot
    // contains every non-ignored file in the repo. Using that as dirty_paths
    // would trigger per-file repo discovery + file reads in
    // build_checkpoint_files — catastrophic on large repos. Fall back to git
    // status which only reports actually changed files.
    let dirty_paths: Vec<PathBuf> = if wm.is_none() {
        match git_status_fallback(repo_root) {
            Ok(paths) => paths.into_iter().map(|p| repo_root.join(p)).collect(),
            Err(_) => vec![],
        }
    } else {
        snap.entries.keys().map(|rel| repo_root.join(rel)).collect()
    };

    let socket = effective_daemon_socket().ok_or_else(|| {
        GitAiError::Generic("no daemon socket available for BashSessionStart".into())
    })?;

    let request = ControlRequest::BashSessionStart {
        repo_work_dir: repo_working_dir,
        original_cwd: Some(original_cwd.to_string_lossy().to_string()),
        session_id: context.session_id.to_string(),
        tool_use_id: context.tool_use_id.to_string(),
        agent_id: context.agent_id.clone(),
        metadata: context.agent_metadata.cloned().unwrap_or_default(),
        stat_snapshot: Box::new(snap),
        trace_id: context.trace_id.to_string(),
        started_at_ns,
        command: context.command.map(ToString::to_string),
    };

    send_control_request(&socket, &request)?;

    Ok(BashPreHookResult { dirty_paths })
}

/// Handle the post-tool-use hook for a bash tool invocation.
///
/// Queries the daemon for the pre-snapshot (stored during `BashSessionStart`),
/// takes a post-snapshot, diffs them, signals `BashSessionEnd`, and returns
/// the list of changed files.
pub fn handle_bash_post_tool_use(
    repo_root: &Path,
    session_id: &str,
    tool_use_id: &str,
    agent_id: &AgentId,
    agent_metadata: Option<&HashMap<String, String>>,
    trace_id: &str,
    command: Option<&str>,
) -> Result<BashPostHookResult, GitAiError> {
    handle_bash_post_tool_use_with_cwd(
        repo_root,
        repo_root,
        BashToolHookContext {
            session_id,
            tool_use_id,
            agent_id,
            agent_metadata,
            trace_id,
            command,
        },
    )
}

/// Handle the post-tool-use hook with a separately tracked original cwd.
pub fn handle_bash_post_tool_use_with_cwd(
    repo_root: &Path,
    original_cwd: &Path,
    context: BashToolHookContext<'_>,
) -> Result<BashPostHookResult, GitAiError> {
    let invocation_key = format!("{}:{}", context.session_id, context.tool_use_id);

    let hook_start = Instant::now();
    let ended_at_ns = crate::daemon::bash_history_db::unix_time_ns();
    let hook_timeout = Duration::from_millis(effective_hook_timeout_ms());
    let repo_working_dir = repo_root.to_string_lossy().to_string();
    let metadata = context.agent_metadata.cloned().unwrap_or_default();

    macro_rules! hook_timeout_fallback {
        ($label:expr) => {{
            let elapsed_ms = hook_start.elapsed().as_millis();
            let msg = format!(
                "bash_tool: {} exceeded {}ms hook limit ({}ms elapsed); abandoning",
                $label, hook_timeout.as_millis(), elapsed_ms
            );
            tracing::debug!("{}", msg);
            crate::observability::log_message(
                &msg,
                "warning",
                Some(serde_json::json!({
                    "label": $label,
                    "elapsed_ms": elapsed_ms,
                    "hook_timeout_ms": hook_timeout.as_millis(),
                })),
            );
            signal_daemon_bash_session_end(BashSessionEndSignal {
                repo_work_dir: &repo_working_dir,
                original_cwd,
                session_id: context.session_id,
                tool_use_id: context.tool_use_id,
                agent_id: context.agent_id,
                metadata: &metadata,
                trace_id: context.trace_id,
                ended_at_ns,
                command: context.command,
            });
            return Ok(BashPostHookResult {
                action: BashCheckpointAction::HookTimeout,
            });
        }};
    }

    let pre_snapshot = query_daemon_bash_snapshot(context.session_id, context.tool_use_id);

    match pre_snapshot {
        Some(pre) => {
            if hook_start.elapsed() >= hook_timeout {
                hook_timeout_fallback!("post-hook before snapshot");
            }

            let post_wm: Option<DaemonWatermarks> =
                if pre.effective_worktree_wm.is_some() || !pre.per_file_wm.is_empty() {
                    Some(DaemonWatermarks {
                        per_file: pre.per_file_wm.clone(),
                        worktree: pre.effective_worktree_wm,
                    })
                } else {
                    None
                };
            let result = match snapshot(
                repo_root,
                context.session_id,
                context.tool_use_id,
                post_wm.as_ref(),
            ) {
                Ok(post) => {
                    let diff_result = diff(&pre, &post);

                    if diff_result.is_empty() {
                        tracing::debug!("Bash tool {}: no changes detected", invocation_key);
                        Ok(BashPostHookResult {
                            action: BashCheckpointAction::NoChanges,
                        })
                    } else {
                        let paths = diff_result.all_changed_paths();
                        tracing::debug!(
                            "Bash tool {}: {} files changed ({} created, {} modified)",
                            invocation_key,
                            paths.len(),
                            diff_result.created.len(),
                            diff_result.modified.len(),
                        );

                        Ok(BashPostHookResult {
                            action: BashCheckpointAction::Checkpoint(paths),
                        })
                    }
                }
                Err(e) => {
                    tracing::debug!("Post-snapshot failed: {}; returning SnapshotFailed", e);
                    Ok(BashPostHookResult {
                        action: BashCheckpointAction::SnapshotFailed,
                    })
                }
            };

            signal_daemon_bash_session_end(BashSessionEndSignal {
                repo_work_dir: &repo_working_dir,
                original_cwd,
                session_id: context.session_id,
                tool_use_id: context.tool_use_id,
                agent_id: context.agent_id,
                metadata: &metadata,
                trace_id: context.trace_id,
                ended_at_ns,
                command: context.command,
            });

            result
        }
        None => {
            tracing::debug!(
                "Pre-snapshot not found in daemon for {}; returning MissingPreSnapshot",
                invocation_key
            );
            signal_daemon_bash_session_end(BashSessionEndSignal {
                repo_work_dir: &repo_working_dir,
                original_cwd,
                session_id: context.session_id,
                tool_use_id: context.tool_use_id,
                agent_id: context.agent_id,
                metadata: &metadata,
                trace_id: context.trace_id,
                ended_at_ns,
                command: context.command,
            });
            Ok(BashPostHookResult {
                action: BashCheckpointAction::MissingPreSnapshot,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn test_git_status_fallback_disables_optional_index_locks() {
        let args = git_status_fallback_args(Path::new("/repo"));

        assert!(
            args.iter().any(|arg| arg == "--no-optional-locks"),
            "git status fallback should not opportunistically refresh the user's index"
        );
    }

    #[test]
    fn test_stat_entry_from_metadata() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmp.path(), "hello world").unwrap();
        let meta = fs::symlink_metadata(tmp.path()).unwrap();
        let entry = StatEntry::from_metadata(&meta);

        assert!(entry.exists);
        assert!(entry.mtime.is_some());
        assert_eq!(entry.size, 11);
        assert_eq!(entry.file_type, StatFileType::Regular);
    }

    #[test]
    fn test_stat_entry_equality() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmp.path(), "hello").unwrap();
        let meta = fs::symlink_metadata(tmp.path()).unwrap();
        let entry1 = StatEntry::from_metadata(&meta);
        let entry2 = StatEntry::from_metadata(&meta);
        assert_eq!(entry1, entry2);
    }

    #[test]
    fn test_stat_entry_modification_detected() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmp.path(), "hello").unwrap();
        let meta1 = fs::symlink_metadata(tmp.path()).unwrap();
        let entry1 = StatEntry::from_metadata(&meta1);

        // Modify the file
        std::thread::sleep(Duration::from_millis(50));
        fs::write(tmp.path(), "hello world").unwrap();
        let meta2 = fs::symlink_metadata(tmp.path()).unwrap();
        let entry2 = StatEntry::from_metadata(&meta2);

        assert_ne!(entry1, entry2);
        assert_ne!(entry1.size, entry2.size);
    }

    #[test]
    fn test_normalize_path_consistency() {
        let path = Path::new("src/main.rs");
        let normalized = normalize_path(path);
        let normalized2 = normalize_path(path);
        assert_eq!(normalized, normalized2);
    }

    #[test]
    fn test_diff_empty_snapshots() {
        let pre = StatSnapshot {
            entries: HashMap::new(),
            taken_at: None,
            invocation_key: "test:1".to_string(),
            repo_root: PathBuf::from("/tmp"),
            effective_worktree_wm: None,
            per_file_wm: HashMap::new(),
        };
        let post = StatSnapshot {
            entries: HashMap::new(),
            taken_at: None,
            invocation_key: "test:2".to_string(),
            repo_root: PathBuf::from("/tmp"),
            effective_worktree_wm: None,
            per_file_wm: HashMap::new(),
        };

        let result = diff(&pre, &post);
        assert!(result.is_empty());
    }

    #[test]
    fn test_diff_detects_creation() {
        let pre = StatSnapshot {
            entries: HashMap::new(),
            taken_at: None,
            invocation_key: "test:1".to_string(),
            repo_root: PathBuf::from("/tmp"),
            effective_worktree_wm: None,
            per_file_wm: HashMap::new(),
        };

        let mut post_entries = HashMap::new();
        post_entries.insert(
            normalize_path(Path::new("new_file.txt")),
            StatEntry {
                exists: true,
                mtime: Some(SystemTime::now()),
                ctime: Some(SystemTime::now()),
                size: 100,
                mode: 0o644,
                file_type: StatFileType::Regular,
            },
        );

        let post = StatSnapshot {
            entries: post_entries,
            taken_at: None,
            invocation_key: "test:2".to_string(),
            repo_root: PathBuf::from("/tmp"),
            effective_worktree_wm: None,
            per_file_wm: HashMap::new(),
        };

        let result = diff(&pre, &post);
        assert_eq!(result.created.len(), 1);
        assert!(result.modified.is_empty());
    }

    #[test]
    fn test_diff_detects_modification() {
        let path = normalize_path(Path::new("modified.txt"));
        let now = SystemTime::now();
        let later = now + Duration::from_secs(1);

        let mut pre_entries = HashMap::new();
        pre_entries.insert(
            path.clone(),
            StatEntry {
                exists: true,
                mtime: Some(now),
                ctime: Some(now),
                size: 50,
                mode: 0o644,
                file_type: StatFileType::Regular,
            },
        );

        let mut post_entries = HashMap::new();
        post_entries.insert(
            path.clone(),
            StatEntry {
                exists: true,
                mtime: Some(later),
                ctime: Some(later),
                size: 75,
                mode: 0o644,
                file_type: StatFileType::Regular,
            },
        );

        let pre = StatSnapshot {
            entries: pre_entries,
            taken_at: None,
            invocation_key: "test:1".to_string(),
            repo_root: PathBuf::from("/tmp"),
            effective_worktree_wm: None,
            per_file_wm: HashMap::new(),
        };

        let post = StatSnapshot {
            entries: post_entries,
            taken_at: None,
            invocation_key: "test:2".to_string(),
            repo_root: PathBuf::from("/tmp"),
            effective_worktree_wm: None,
            per_file_wm: HashMap::new(),
        };

        let result = diff(&pre, &post);
        assert!(result.created.is_empty());
        assert_eq!(result.modified.len(), 1);
    }

    #[test]
    fn test_tool_classification_claude() {
        assert_eq!(classify_tool(Agent::Claude, "Write"), ToolClass::FileEdit);
        assert_eq!(classify_tool(Agent::Claude, "Edit"), ToolClass::FileEdit);
        assert_eq!(
            classify_tool(Agent::Claude, "MultiEdit"),
            ToolClass::FileEdit
        );
        assert_eq!(classify_tool(Agent::Claude, "Bash"), ToolClass::Bash);
        assert_eq!(classify_tool(Agent::Claude, "Read"), ToolClass::Skip);
        assert_eq!(classify_tool(Agent::Claude, "unknown"), ToolClass::Skip);
    }

    #[test]
    fn test_tool_classification_all_agents() {
        // Gemini
        assert_eq!(
            classify_tool(Agent::Gemini, "write_file"),
            ToolClass::FileEdit
        );
        assert_eq!(classify_tool(Agent::Gemini, "shell"), ToolClass::Bash);

        // Continue CLI
        assert_eq!(
            classify_tool(Agent::ContinueCli, "edit"),
            ToolClass::FileEdit
        );
        assert_eq!(
            classify_tool(Agent::ContinueCli, "terminal"),
            ToolClass::Bash
        );
        assert_eq!(
            classify_tool(Agent::ContinueCli, "local_shell_call"),
            ToolClass::Bash
        );

        // Droid
        assert_eq!(
            classify_tool(Agent::Droid, "ApplyPatch"),
            ToolClass::FileEdit
        );
        assert_eq!(classify_tool(Agent::Droid, "Bash"), ToolClass::Bash);

        // Amp
        assert_eq!(classify_tool(Agent::Amp, "Write"), ToolClass::FileEdit);
        assert_eq!(classify_tool(Agent::Amp, "Bash"), ToolClass::Bash);

        // OpenCode
        assert_eq!(classify_tool(Agent::OpenCode, "edit"), ToolClass::FileEdit);
        assert_eq!(classify_tool(Agent::OpenCode, "bash"), ToolClass::Bash);
        assert_eq!(classify_tool(Agent::OpenCode, "shell"), ToolClass::Bash);

        // Cursor
        assert_eq!(classify_tool(Agent::Cursor, "Write"), ToolClass::FileEdit);
        assert_eq!(classify_tool(Agent::Cursor, "Delete"), ToolClass::FileEdit);
        assert_eq!(
            classify_tool(Agent::Cursor, "StrReplace"),
            ToolClass::FileEdit
        );
        assert_eq!(
            classify_tool(Agent::Cursor, "ApplyPatch"),
            ToolClass::FileEdit
        );
        assert_eq!(classify_tool(Agent::Cursor, "Shell"), ToolClass::Bash);
        assert_eq!(classify_tool(Agent::Cursor, "Read"), ToolClass::Skip);
    }

    #[test]
    fn test_tool_classification_codex_namespaced() {
        // Codex Desktop/OpenAI function calls namespace tools with "functions." prefix.
        assert_eq!(
            classify_tool(Agent::Codex, "functions.apply_patch"),
            ToolClass::FileEdit
        );
        assert_eq!(
            classify_tool(Agent::Codex, "functions.Bash"),
            ToolClass::Bash
        );
        assert_eq!(
            classify_tool(Agent::Codex, "functions.exec_command"),
            ToolClass::Bash
        );
        assert_eq!(
            classify_tool(Agent::Codex, "functions.shell"),
            ToolClass::Bash
        );
        assert_eq!(
            classify_tool(Agent::Codex, "functions.shell_command"),
            ToolClass::Bash
        );
        // Unqualified names still work as before.
        assert_eq!(
            classify_tool(Agent::Codex, "apply_patch"),
            ToolClass::FileEdit
        );
        assert_eq!(classify_tool(Agent::Codex, "Bash"), ToolClass::Bash);
        assert_eq!(classify_tool(Agent::Codex, "exec_command"), ToolClass::Bash);
        // The parallel tool wrapper must be routed through the bash/stat-diff path.
        assert_eq!(
            classify_tool(Agent::Codex, "multi_tool_use.parallel"),
            ToolClass::Bash
        );
        // Unknown names are still skipped.
        assert_eq!(
            classify_tool(Agent::Codex, "functions.Read"),
            ToolClass::Skip
        );
        assert_eq!(classify_tool(Agent::Codex, "Read"), ToolClass::Skip);
    }

    #[test]
    fn test_stat_diff_result_all_changed_paths() {
        let result = StatDiffResult {
            created: vec![PathBuf::from("new.txt")],
            modified: vec![PathBuf::from("changed.txt")],
        };
        let paths = result.all_changed_paths();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"new.txt".to_string()));
        assert!(paths.contains(&"changed.txt".to_string()));
    }

    // -----------------------------------------------------------------------
    // system_time_to_nanos tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_system_time_to_nanos() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        assert_eq!(system_time_to_nanos(t), 1_000_000_000);
    }

    #[test]
    fn test_system_time_to_nanos_epoch() {
        assert_eq!(system_time_to_nanos(SystemTime::UNIX_EPOCH), 0);
    }

    // -----------------------------------------------------------------------
    // build_gitignore tests
    // -----------------------------------------------------------------------

    fn init_git_repo(dir: &Path) {
        Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    /// Default ignore patterns (e.g. node_modules, lock files) are applied even
    /// when no .gitignore exists in the repo.
    #[test]
    fn test_build_gitignore_applies_default_patterns() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());

        let gitignore = build_gitignore(dir.path()).unwrap();

        // node_modules and lock files must be excluded by default
        assert!(
            !should_include_new_file(&gitignore, Path::new("node_modules/react/index.js"), false),
            "node_modules should be ignored by default"
        );
        assert!(
            !should_include_new_file(&gitignore, Path::new("package-lock.json"), false),
            "package-lock.json should be ignored by default"
        );
        assert!(
            !should_include_new_file(&gitignore, Path::new("yarn.lock"), false),
            "yarn.lock should be ignored by default"
        );

        // Normal source files must not be excluded
        assert!(
            should_include_new_file(&gitignore, Path::new("src/main.rs"), false),
            "src/main.rs should not be ignored"
        );
    }

    /// Patterns in .git-ai-ignore are respected, suppressing untracked files
    /// that aren't covered by .gitignore.
    #[test]
    fn test_build_gitignore_reads_git_ai_ignore() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());

        fs::write(dir.path().join(".git-ai-ignore"), "secrets/\n*.pem\n").unwrap();

        let gitignore = build_gitignore(dir.path()).unwrap();

        assert!(
            !should_include_new_file(&gitignore, Path::new("secrets/token.txt"), false),
            "secrets/ should be ignored via .git-ai-ignore"
        );
        assert!(
            !should_include_new_file(&gitignore, Path::new("server.pem"), false),
            "*.pem should be ignored via .git-ai-ignore"
        );
        assert!(
            should_include_new_file(&gitignore, Path::new("README.md"), false),
            "README.md should not be ignored"
        );
    }

    /// Files marked linguist-generated in .gitattributes are excluded from
    /// the Tier 2 snapshot.
    #[test]
    fn test_build_gitignore_reads_linguist_generated_from_gitattributes() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());

        fs::write(
            dir.path().join(".gitattributes"),
            "generated/*.pb.go linguist-generated=true\ndocs/api.md linguist-generated\n",
        )
        .unwrap();

        let gitignore = build_gitignore(dir.path()).unwrap();

        assert!(
            !should_include_new_file(&gitignore, Path::new("generated/foo.pb.go"), false),
            "linguist-generated glob should be ignored"
        );
        assert!(
            !should_include_new_file(&gitignore, Path::new("docs/api.md"), false),
            "linguist-generated exact file should be ignored"
        );
        assert!(
            should_include_new_file(&gitignore, Path::new("generated/manual.go"), false),
            "non-generated file in generated/ should not be ignored"
        );
    }
}
