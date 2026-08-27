//! Shared harness for driving the Graphite (`gt`) CLI inside a `TestRepo`.
//!
//! `gt` shells out to `git` for commits, rebases, and pushes. To make those
//! invocations visible to the git-ai daemon, every `gt` call runs with a shim
//! directory first on `PATH`; the shim logs tracked git invocations and then
//! delegates to real git. After the `gt` process exits, the logged sessions are
//! handed to `TestRepo::sync_daemon_external_completion_sessions` so assertions
//! observe a fully-drained daemon.

use crate::repos::test_repo::{TestRepo, real_git_executable};

use serde::Deserialize;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

const DETERMINISTIC_GIT_NAME: &str = "Graphite Test";
const DETERMINISTIC_GIT_EMAIL: &str = "graphite-test@example.com";
const DETERMINISTIC_GIT_DATE: &str = "2000-01-01T00:00:00+00:00";

/// Resolve and cache the absolute path to the `gt` CLI binary.
/// On Windows, npm installs `gt` as `gt.cmd` (a batch wrapper), which Rust's
/// `Command::new("gt")` cannot find because it only searches for `.exe` files.
/// By resolving the full path once via `where`/`which`, we can use the absolute
/// path in all subsequent Command invocations.
static GT_BINARY_PATH: OnceLock<Option<String>> = OnceLock::new();

pub fn find_gt_binary() -> Option<&'static str> {
    GT_BINARY_PATH
        .get_or_init(|| {
            #[cfg(windows)]
            let which_cmd = "where";
            #[cfg(not(windows))]
            let which_cmd = "which";

            let output = Command::new(which_cmd).arg("gt").output().ok()?;
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                // `where` on Windows may return multiple lines; take the first.
                let first = path.lines().next().unwrap_or(&path).to_string();
                if first.is_empty() { None } else { Some(first) }
            } else {
                None
            }
        })
        .as_deref()
}

/// Guard that skips the test when `gt` is not installed (local dev),
/// or panics when running in CI (where `gt` MUST be available).
macro_rules! require_gt {
    () => {{
        if $crate::graphite::graphite_test_harness::find_gt_binary().is_none() {
            if std::env::var("CI").is_ok() {
                panic!(
                    "Graphite CLI (`gt`) is required in CI but was not found. \
                     Install it with: npm install -g @withgraphite/graphite-cli@stable"
                );
            } else {
                eprintln!("SKIP: `gt` CLI not found — skipping Graphite test");
                return;
            }
        }
    }};
}

pub(crate) use require_gt;

/// Create a shim directory containing a `git` symlink (or copy on Windows)
/// that points to the test-only git shim binary. The shim logs tracked git
/// invocations for external tools like Graphite, then delegates to real git.
static GT_GIT_SHIM_DIR: OnceLock<PathBuf> = OnceLock::new();

fn gt_git_shim_dir() -> &'static PathBuf {
    GT_GIT_SHIM_DIR.get_or_init(|| {
        let shim_binary = PathBuf::from(env!("CARGO_BIN_EXE_git-ai-test-git-shim"));
        let shim_dir =
            std::env::temp_dir().join(format!("git-ai-gt-git-shim-{}", std::process::id()));
        std::fs::create_dir_all(&shim_dir).expect("create shim dir");

        #[cfg(unix)]
        {
            let link_path = shim_dir.join("git");
            // Remove stale symlink if it exists
            let _ = std::fs::remove_file(&link_path);
            std::os::unix::fs::symlink(shim_binary, &link_path).expect("create git symlink");
        }

        #[cfg(windows)]
        {
            let link_path = shim_dir.join("git.exe");
            let _ = std::fs::remove_file(&link_path);
            std::fs::copy(shim_binary, &link_path).expect("copy shim as git.exe");
        }

        shim_dir
    })
}

/// Build a PATH string that has the shim directory first,
/// followed by the original system PATH.
fn gt_git_path() -> String {
    let shim_dir = gt_git_shim_dir();
    let original_path = std::env::var("PATH").unwrap_or_default();
    let sep = if cfg!(windows) { ";" } else { ":" };
    format!("{}{}{}", shim_dir.display(), sep, original_path)
}

fn gt_git_target() -> String {
    real_git_executable().to_string()
}

fn new_gt_started_log_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "git-ai-gt-started-{}-{}.jsonl",
        std::process::id(),
        git_ai::uuid::generate_v4()
    ))
}

#[derive(Deserialize)]
struct GtStartedLogEntry {
    #[serde(default)]
    test_sync_session: Option<String>,
}

fn gt_started_sessions(log_path: &PathBuf) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(log_path) else {
        return Vec::new();
    };

    let mut sessions = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: GtStartedLogEntry = serde_json::from_str(line).unwrap_or_else(|error| {
            panic!(
                "failed to parse Graphite shim start log entry {} in {}: {}",
                idx + 1,
                log_path.display(),
                error
            )
        });
        if let Some(session) = entry.test_sync_session {
            sessions.push(session);
        }
    }

    sessions
}

fn apply_deterministic_git_env(command: &mut Command, repo: &TestRepo) {
    command.env("HOME", repo.test_home_path());
    command.env(
        "GIT_CONFIG_GLOBAL",
        repo.test_home_path().join(".gitconfig"),
    );
    command.env("XDG_CONFIG_HOME", repo.test_home_path().join(".config"));

    command.env("GIT_AUTHOR_NAME", DETERMINISTIC_GIT_NAME);
    command.env("GIT_AUTHOR_EMAIL", DETERMINISTIC_GIT_EMAIL);
    command.env("GIT_AUTHOR_DATE", DETERMINISTIC_GIT_DATE);
    command.env("GIT_COMMITTER_NAME", DETERMINISTIC_GIT_NAME);
    command.env("GIT_COMMITTER_EMAIL", DETERMINISTIC_GIT_EMAIL);
    command.env("GIT_COMMITTER_DATE", DETERMINISTIC_GIT_DATE);
    command.env("TZ", "UTC");
    command.env("LC_ALL", "C");
    command.env("LANG", "C");
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_TERMINAL_PROMPT", "0");
}

pub fn assert_head_branch(repo: &TestRepo, expected_branch: &str) {
    let current = repo.current_branch();
    assert_eq!(
        current, expected_branch,
        "expected HEAD branch {expected_branch}, found {current}"
    );
}

pub fn assert_worktree_clean(repo: &TestRepo) {
    let status = repo
        .git(&["status", "--porcelain"])
        .expect("git status should succeed");
    assert!(
        status.trim().is_empty(),
        "expected clean worktree, found:\n{}",
        status
    );
}

/// Execute a `gt` command inside the given TestRepo directory.
///
/// The key insight: `gt` calls `git` internally for commits, rebases, etc.
/// By prepending a shim directory to PATH, all of `gt`'s git operations emit
/// trace2 metadata to the daemon and can be synchronized by the test harness.
///
/// Passes `--no-interactive` to avoid prompts.
/// Returns Ok(stdout+stderr) on success, Err(stderr) on failure.
pub fn gt(repo: &TestRepo, args: &[&str]) -> Result<String, String> {
    let gt_path =
        find_gt_binary().expect("gt binary not found; require_gt! should have been called");

    // On Windows, npm installs `gt` as `gt.cmd` (a batch wrapper). Rust's
    // Command cannot execute `.cmd` files directly — they must be run through
    // `cmd.exe /C`. On Unix, we invoke the binary directly.
    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("cmd");
        c.args(["/C", gt_path]);
        c
    };
    #[cfg(not(windows))]
    let mut command = Command::new(gt_path);

    command
        .current_dir(repo.path())
        .args(args)
        .arg("--no-interactive");

    let started_log_path = new_gt_started_log_path();

    // Put the test shim first in PATH so `gt` calls it instead of raw git. The
    // shim logs tracked git invocations and then delegates to real git.
    command.env("PATH", gt_git_path());
    command.env("GIT_AI_TEST_GIT_SHIM_TARGET", gt_git_target());
    command.env(
        "GIT_AI_TEST_GIT_SHIM_FALLBACK_TARGET",
        real_git_executable(),
    );
    command.env("GIT_AI_TEST_SYNC_START_LOG", &started_log_path);

    // Set deterministic git metadata + isolated config/locale across all gt invocations.
    apply_deterministic_git_env(&mut command, repo);

    let trace_socket = repo.daemon_trace_socket_path();
    let nesting = std::env::var("GIT_AI_TEST_TRACE2_NESTING").unwrap_or_else(|_| "0".to_string());
    command.env(
        "GIT_TRACE2_EVENT",
        git_ai::daemon::DaemonConfig::trace2_event_target_for_path(&trace_socket),
    );
    command.env("GIT_TRACE2_EVENT_NESTING", nesting);
    command.env("GIT_AI_TEST_DB_PATH", repo.test_db_path().to_str().unwrap());
    command.env("GITAI_TEST_DB_PATH", repo.test_db_path().to_str().unwrap());

    if let Some(patch) = repo.config_patch_json() {
        command.env("GIT_AI_TEST_CONFIG_PATCH", patch);
    }

    // Isolate Graphite's config and data directories per test to prevent
    // parallel test corruption of config files and the nuxes SQLite database
    // (race condition in CI).
    command.env("XDG_CONFIG_HOME", repo.test_home_path().join(".config"));
    command.env(
        "XDG_DATA_HOME",
        repo.test_home_path().join(".local").join("share"),
    );
    // Windows equivalents for Graphite config and data isolation.
    // USERPROFILE is read by Node.js os.homedir() on Windows (not HOME).
    command.env("USERPROFILE", repo.test_home_path());
    command.env(
        "LOCALAPPDATA",
        repo.test_home_path().join("AppData").join("Local"),
    );
    command.env(
        "APPDATA",
        repo.test_home_path().join("AppData").join("Roaming"),
    );

    let output = command
        .output()
        .unwrap_or_else(|e| panic!("Failed to execute gt {:?}: {}", args, e));

    let sessions = gt_started_sessions(&started_log_path);
    repo.sync_daemon_external_completion_sessions(&sessions);

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        let combined = if stdout.is_empty() {
            stderr
        } else if stderr.is_empty() {
            stdout
        } else {
            format!("{}{}", stdout, stderr)
        };
        Ok(combined)
    } else {
        let combined_err = format!("{}{}", stderr, stdout);
        Err(combined_err)
    }
}

/// Initialize Graphite in a TestRepo (sets trunk to "main").
pub fn gt_init(repo: &TestRepo) {
    gt(repo, &["init", "--trunk", "main"]).expect("gt init should succeed");
}

/// Create an initial commit so the repo is not empty (required for most gt operations).
pub fn setup_initial_commit(repo: &TestRepo) {
    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Test Repo"]);
    repo.stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");
}
