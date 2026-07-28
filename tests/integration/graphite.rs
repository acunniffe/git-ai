/// Graphite (`gt` CLI) test suite for git-ai attribution preservation.
///
/// These tests verify that git-ai attribution (line-level blame tracking of AI vs human authorship)
/// is correctly preserved across all local Graphite CLI operations.
///
/// ## Requirements
/// - The `gt` CLI must be installed and available in PATH
/// - When the `CI` environment variable is set, tests will FAIL if `gt` is not available
/// - When not in CI, tests will be SKIPPED if `gt` is not available
///
/// ## Graphite's `commit-tree` + `update-ref` plumbing path
///
/// Graphite's restack/move/absorb/split operations internally use `git commit-tree` +
/// `git update-ref` (low-level plumbing commands) instead of `git rebase`.
///
/// git-ai receives Graphite's `update-ref` trace2 events, detects the
/// non-fast-forward rewrite, and remaps authorship notes to the new commit SHAs.
/// This covers the core operations: restack, move, modify (with child restacking),
/// and full stack workflows.
///
/// Remaining known issues (still `#[ignore]`):
///   - `gt absorb` and `gt split --by-file` lose attribution (update-ref hook cannot
///     reconstruct the mapping for these more complex rewrite patterns)
///   - `gt delete --force` and `gt undo` require interactive mode even with `--no-interactive`
///
/// ## Commands NOT tested (require GitHub authentication / remote):
/// - `gt submit` - Pushes to GitHub, creates/updates PRs
/// - `gt sync` - Syncs branches with remote
/// - `gt get` - Syncs branches from remote
/// - `gt merge` - Merges PRs via Graphite
/// - `gt pr` - Opens PR page in browser
/// - `gt dash` - Opens Graphite dashboard
/// - `gt auth` - Authentication
/// - `gt feedback` - Sends feedback to Graphite team
/// - `gt freeze` / `gt unfreeze` - Primarily useful with remote sync
/// - `gt reorder` - Requires interactive editor
/// - `gt revert` - Experimental, requires specific trunk commit setup
/// - `gt split --by-commit` / `gt split --by-hunk` - Requires interactive input
///
/// ## Commands NOT tested (require interactive terminal):
/// - `gt undo` - Requires interactive mode even with `--no-interactive` flag
///
/// ## Commands tested:
/// - `gt init` - Initialize Graphite in a repo
/// - `gt create` - Create new branch with commit
/// - `gt modify` - Amend/new commit with automatic restack
/// - `gt squash` - Squash all commits in branch into one
/// - `gt restack` - Rebase stack to ensure parent lineage
/// - `gt fold` - Fold branch into parent
/// - `gt move` - Move branch to new parent
/// - `gt split --by-file` - Split branch by file (KNOWN_ISSUE: loses attribution)
/// - `gt absorb` - Absorb staged changes into stack (KNOWN_ISSUE: loses attribution)
/// - `gt checkout` / `gt up` / `gt down` / `gt top` / `gt bottom` - Navigation
/// - `gt delete` - Delete branch, restack children (KNOWN_ISSUE: requires interactive mode)
/// - `gt pop` - Delete branch, retain working tree
/// - `gt rename` - Rename branch
/// - `gt track` / `gt untrack` - Metadata tracking
use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{TestRepo, real_git_executable};

use serde::Deserialize;
use std::{fs, path::PathBuf, process::Command, sync::OnceLock};

const DETERMINISTIC_GIT_NAME: &str = "Graphite Test";
const DETERMINISTIC_GIT_EMAIL: &str = "graphite-test@example.com";
const DETERMINISTIC_GIT_DATE: &str = "2000-01-01T00:00:00+00:00";

// ---------------------------------------------------------------------------
// Helper utilities
// ---------------------------------------------------------------------------

/// Resolve and cache the absolute path to the `gt` CLI binary.
/// On Windows, npm installs `gt` as `gt.cmd` (a batch wrapper), which Rust's
/// `Command::new("gt")` cannot find because it only searches for `.exe` files.
/// By resolving the full path once via `where`/`which`, we can use the absolute
/// path in all subsequent Command invocations.
static GT_BINARY_PATH: OnceLock<Option<String>> = OnceLock::new();

fn find_gt_binary() -> Option<&'static str> {
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
        if find_gt_binary().is_none() {
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

fn assert_head_branch(repo: &TestRepo, expected_branch: &str) {
    let current = repo.current_branch();
    assert_eq!(
        current, expected_branch,
        "expected HEAD branch {expected_branch}, found {current}"
    );
}

fn assert_worktree_clean(repo: &TestRepo) {
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
fn gt(repo: &TestRepo, args: &[&str]) -> Result<String, String> {
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
fn gt_init(repo: &TestRepo) {
    gt(repo, &["init", "--trunk", "main"]).expect("gt init should succeed");
}

/// Create an initial commit so the repo is not empty (required for most gt operations).
fn setup_initial_commit(repo: &TestRepo) {
    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Test Repo"]);
    repo.stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");
}

// ===========================================================================
// Group 1: gt create — Branch creation with attribution
// ===========================================================================

#[test]
fn test_gt_create_preserves_ai_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Add AI-authored content
    let mut file = repo.filename("feature.txt");
    file.set_contents(crate::lines![
        "human line 1",
        "ai line 2".ai(),
        "ai line 3".ai(),
        "human line 4",
    ]);

    // Use gt create to create a branch and commit
    repo.git(&["add", "-A"]).unwrap();
    gt(
        &repo,
        &["create", "feature-branch", "-m", "add feature with AI"],
    )
    .expect("gt create should succeed");
    assert_head_branch(&repo, "feature-branch");
    assert_worktree_clean(&repo);

    // Verify attribution is preserved
    file.assert_lines_and_blame(crate::lines![
        "human line 1".human(),
        "ai line 2".ai(),
        "ai line 3".ai(),
        "human line 4".human(),
    ]);
}

#[test]
fn test_gt_create_stacked_branches_preserve_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // First branch with AI content
    let mut file1 = repo.filename("file1.txt");
    file1.set_contents(crate::lines!["file1 ai line".ai(), "file1 human line"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "branch-1", "-m", "first branch"])
        .expect("gt create branch-1 should succeed");

    // Second branch stacked on first, with more AI content
    let mut file2 = repo.filename("file2.txt");
    file2.set_contents(crate::lines!["file2 ai line".ai(), "file2 human line"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "branch-2", "-m", "second branch"])
        .expect("gt create branch-2 should succeed");
    assert_head_branch(&repo, "branch-2");
    assert_worktree_clean(&repo);

    // Verify attribution on both files from the tip of the stack
    file1.assert_lines_and_blame(crate::lines![
        "file1 ai line".ai(),
        "file1 human line".human(),
    ]);
    file2.assert_lines_and_blame(crate::lines![
        "file2 ai line".ai(),
        "file2 human line".human(),
    ]);

    // Navigate down and verify attribution still correct on branch-1
    gt(&repo, &["checkout", "branch-1"]).expect("gt checkout should succeed");
    assert_head_branch(&repo, "branch-1");
    file1.assert_lines_and_blame(crate::lines![
        "file1 ai line".ai(),
        "file1 human line".human(),
    ]);
}

#[test]
fn test_gt_create_empty_branch() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create a file with AI attribution
    let mut file = repo.filename("existing.txt");
    file.set_contents(crate::lines!["ai content".ai(), "human content"]);
    repo.stage_all_and_commit("add file").unwrap();

    // Create empty branch (no changes)
    gt(&repo, &["create", "empty-branch", "-m", "empty branch"])
        .expect("gt create empty branch should succeed");
    assert_head_branch(&repo, "empty-branch");
    assert_worktree_clean(&repo);

    // Attribution on existing file should be unchanged
    file.assert_lines_and_blame(crate::lines!["ai content".ai(), "human content".human(),]);
}

// ===========================================================================
// Group 2: gt modify — Amend/new commit with restack
// ===========================================================================

#[test]
fn test_gt_modify_amend_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create a branch with AI content
    let mut file = repo.filename("modify.txt");
    file.set_contents(crate::lines![
        "original ai line".ai(),
        "original human line"
    ]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "modify-branch", "-m", "initial"]).expect("gt create should succeed");

    // Add more content and amend via gt modify
    let mut file2 = repo.filename("modify2.txt");
    file2.set_contents(crate::lines!["new ai line".ai()]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["modify", "-m", "amended with more AI"]).expect("gt modify should succeed");
    assert_head_branch(&repo, "modify-branch");
    assert_worktree_clean(&repo);

    // Both files should have correct attribution
    file.assert_lines_and_blame(crate::lines![
        "original ai line".ai(),
        "original human line".human(),
    ]);
    file2.assert_lines_and_blame(crate::lines!["new ai line".ai(),]);
}

#[test]
fn test_gt_modify_new_commit_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create branch with AI content
    let mut file = repo.filename("modify_commit.txt");
    file.set_contents(crate::lines!["ai line 1".ai(), "human line 1"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "modify-commit-branch", "-m", "initial"])
        .expect("gt create should succeed");

    // Add more changes with --commit (new commit, not amend)
    let mut file2 = repo.filename("modify_commit2.txt");
    file2.set_contents(crate::lines!["ai line 2".ai()]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["modify", "--commit", "-m", "second commit"])
        .expect("gt modify --commit should succeed");
    assert_head_branch(&repo, "modify-commit-branch");
    assert_worktree_clean(&repo);

    // Both files should have correct attribution
    file.assert_lines_and_blame(crate::lines!["ai line 1".ai(), "human line 1".human(),]);
    file2.assert_lines_and_blame(crate::lines!["ai line 2".ai(),]);
}

/// `gt modify` amends via `commit-tree` when restacking children.
/// The daemon observes the `update-ref` trace2 event and remaps authorship notes.
#[test]
fn test_gt_modify_restacks_children_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create parent branch
    let mut parent_file = repo.filename("parent.txt");
    parent_file.set_contents(crate::lines!["parent ai".ai(), "parent human"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "parent-branch", "-m", "parent"])
        .expect("gt create parent should succeed");

    // Create child branch
    let mut child_file = repo.filename("child.txt");
    child_file.set_contents(crate::lines!["child ai".ai(), "child human"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "child-branch", "-m", "child"]).expect("gt create child should succeed");

    // Go back to parent and modify it (should trigger restack of child)
    gt(&repo, &["checkout", "parent-branch"]).unwrap();
    let mut parent_file2 = repo.filename("parent2.txt");
    parent_file2.set_contents(crate::lines!["parent2 ai".ai()]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["modify", "-m", "modified parent"]).expect("gt modify parent should succeed");

    // Verify parent attribution
    parent_file.assert_lines_and_blame(crate::lines!["parent ai".ai(), "parent human".human(),]);

    // Check child branch attribution after restack
    gt(&repo, &["checkout", "child-branch"]).unwrap();
    child_file.assert_lines_and_blame(crate::lines!["child ai".ai(), "child human".human(),]);
}

#[test]
fn test_gt_modify_into_downstack_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    let parent_path = repo.path().join("parent.txt");
    fs::write(&parent_path, "alpha\nbravo\ncharlie\ndelta\necho\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "parent.txt"])
        .unwrap();
    repo.git(&["add", "--all"]).unwrap();
    gt(&repo, &["create", "parent-branch", "--message", "parent"])
        .expect("gt create parent should succeed");

    let mut parent_file = repo.filename("parent.txt");
    parent_file.assert_committed_lines(crate::lines![
        "alpha".ai(),
        "bravo".ai(),
        "charlie".ai(),
        "delta".ai(),
        "echo".ai(),
    ]);

    let child_path = repo.path().join("child.txt");
    fs::write(&child_path, "child-only\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "child.txt"])
        .unwrap();
    repo.git(&["add", "--all"]).unwrap();
    gt(&repo, &["create", "child-branch", "--message", "child"])
        .expect("gt create child should succeed");

    parent_file.assert_committed_lines(crate::lines![
        "alpha".ai(),
        "bravo".ai(),
        "charlie".ai(),
        "delta".ai(),
        "echo".ai(),
    ]);
    let mut child_file = repo.filename("child.txt");
    child_file.assert_committed_lines(crate::lines!["child-only".ai()]);

    // Match an AI agent's pre/post-edit checkpoint flow while remaining on the
    // child branch and sending the resulting change into the parent branch.
    repo.git_ai(&["checkpoint", "human", "parent.txt"]).unwrap();
    fs::write(
        &parent_path,
        "alpha\nBRAVO edited by AI\ncharlie\nDELTA edited by AI\necho\nfoxtrot added by AI\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "parent.txt"])
        .unwrap();

    gt(&repo, &["modify", "--all", "--into", "parent-branch"])
        .expect("gt modify --into should succeed without Graphite authentication");

    assert_head_branch(&repo, "child-branch");
    assert_worktree_clean(&repo);
    parent_file.assert_committed_lines(crate::lines![
        "alpha".ai(),
        "BRAVO edited by AI".ai(),
        "charlie".ai(),
        "DELTA edited by AI".ai(),
        "echo".ai(),
        "foxtrot added by AI".ai(),
    ]);
    child_file.assert_committed_lines(crate::lines!["child-only".ai()]);

    gt(&repo, &["checkout", "parent-branch"]).expect("gt checkout parent should succeed");
    parent_file.assert_committed_lines(crate::lines![
        "alpha".ai(),
        "BRAVO edited by AI".ai(),
        "charlie".ai(),
        "DELTA edited by AI".ai(),
        "echo".ai(),
        "foxtrot added by AI".ai(),
    ]);
}

#[test]
fn test_graphite_modify_into_plumbing_preserves_transplanted_attribution() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    repo.git(&["switch", "-c", "parent-branch"]).unwrap();
    let parent_path = repo.path().join("parent.txt");
    fs::write(&parent_path, "alpha\nbravo\ncharlie\ndelta\necho\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "parent.txt"])
        .unwrap();
    repo.git(&["add", "--all"]).unwrap();
    repo.commit("parent").unwrap();

    let parent_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let parent_base = repo
        .git(&["rev-parse", "HEAD^"])
        .unwrap()
        .trim()
        .to_string();
    let mut parent_file = repo.filename("parent.txt");
    parent_file.assert_committed_lines(crate::lines![
        "alpha".ai(),
        "bravo".ai(),
        "charlie".ai(),
        "delta".ai(),
        "echo".ai(),
    ]);

    repo.git(&["switch", "-c", "child-branch"]).unwrap();
    let child_path = repo.path().join("child.txt");
    fs::write(&child_path, "child-only\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "child.txt"])
        .unwrap();
    repo.git(&["add", "--all"]).unwrap();
    repo.commit("child").unwrap();

    let child_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    parent_file.assert_committed_lines(crate::lines![
        "alpha".ai(),
        "bravo".ai(),
        "charlie".ai(),
        "delta".ai(),
        "echo".ai(),
    ]);
    let mut child_file = repo.filename("child.txt");
    child_file.assert_committed_lines(crate::lines!["child-only".ai()]);

    repo.git_ai(&["checkpoint", "human", "parent.txt"]).unwrap();
    fs::write(
        &parent_path,
        "alpha\nBRAVO edited by AI\ncharlie\nDELTA edited by AI\necho\nfoxtrot added by AI\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "parent.txt"])
        .unwrap();

    // Exact attribution-relevant plumbing captured in Graphite 1.8.6's debug
    // log for `gt modify --all --into <downstack-branch>`.
    repo.git(&["add", "--all"]).unwrap();
    repo.git(&[
        "commit",
        "-m",
        "graphite (temporary): staged changes",
        "--allow-empty",
    ])
    .unwrap();
    let temporary_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    parent_file.assert_committed_lines(crate::lines![
        "alpha".ai(),
        "BRAVO edited by AI".ai(),
        "charlie".ai(),
        "DELTA edited by AI".ai(),
        "echo".ai(),
        "foxtrot added by AI".ai(),
    ]);
    child_file.assert_committed_lines(crate::lines!["child-only".ai()]);

    let parent_tree = format!("{parent_commit}^{{tree}}");
    let merge_target = repo
        .git(&["commit-tree", &parent_tree, "-p", &child_commit, "-m", "_"])
        .unwrap()
        .trim()
        .to_string();
    let merged_tree = repo
        .git(&["merge-tree", &merge_target, &temporary_commit])
        .unwrap()
        .trim()
        .to_string();
    let rewritten_parent = repo
        .git(&[
            "commit-tree",
            &merged_tree,
            "-p",
            &parent_base,
            "-m",
            "parent",
        ])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["update-ref", "refs/heads/parent-branch", &rewritten_parent])
        .unwrap();

    repo.git(&["switch", "parent-branch"]).unwrap();
    parent_file.assert_committed_lines(crate::lines![
        "alpha".ai(),
        "BRAVO edited by AI".ai(),
        "charlie".ai(),
        "DELTA edited by AI".ai(),
        "echo".ai(),
        "foxtrot added by AI".ai(),
    ]);
}

#[test]
fn test_graphite_temporary_commit_does_not_attribute_a_different_rewrite() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    repo.git(&["switch", "-c", "parent-branch"]).unwrap();
    let parent_path = repo.path().join("parent.txt");
    fs::write(&parent_path, "alpha\nbravo\ncharlie\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "parent.txt"])
        .unwrap();
    repo.stage_all_and_commit("parent").unwrap();

    let parent_base = repo
        .git(&["rev-parse", "HEAD^"])
        .unwrap()
        .trim()
        .to_string();
    let mut parent_file = repo.filename("parent.txt");
    parent_file.assert_committed_lines(crate::lines!["alpha".ai(), "bravo".ai(), "charlie".ai(),]);

    // Prepare a different target tree before creating the Graphite temporary
    // commit. Its delta must not receive the temporary commit's AI note.
    repo.git(&["switch", "-c", "different-rewrite"]).unwrap();
    repo.git_ai(&["checkpoint", "human", "parent.txt"]).unwrap();
    fs::write(&parent_path, "alpha\ndifferent target rewrite\ncharlie\n").unwrap();
    repo.stage_all_and_commit("different rewrite").unwrap();
    let different_tree = repo
        .git(&["rev-parse", "HEAD^{tree}"])
        .unwrap()
        .trim()
        .to_string();
    parent_file.assert_committed_lines(crate::lines![
        "alpha".ai(),
        "different target rewrite".unattributed_human(),
        "charlie".ai(),
    ]);

    repo.git(&["switch", "parent-branch"]).unwrap();
    repo.git(&["switch", "-c", "child-branch"]).unwrap();
    repo.git_ai(&["checkpoint", "human", "parent.txt"]).unwrap();
    fs::write(&parent_path, "alpha\nAI transplant candidate\ncharlie\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "parent.txt"])
        .unwrap();
    repo.git(&["add", "--all"]).unwrap();
    repo.git(&[
        "commit",
        "-m",
        "graphite (temporary): staged changes",
        "--allow-empty",
    ])
    .unwrap();
    parent_file.assert_committed_lines(crate::lines![
        "alpha".ai(),
        "AI transplant candidate".ai(),
        "charlie".ai(),
    ]);

    let rewritten_parent = repo
        .git(&[
            "commit-tree",
            &different_tree,
            "-p",
            &parent_base,
            "-m",
            "parent",
        ])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["update-ref", "refs/heads/parent-branch", &rewritten_parent])
        .unwrap();

    repo.git(&["switch", "parent-branch"]).unwrap();
    parent_file.assert_committed_lines(crate::lines![
        "alpha".ai(),
        "different target rewrite".unattributed_human(),
        "charlie".ai(),
    ]);
}

// ===========================================================================
// Group 3: gt squash — Squash commits in branch
// ===========================================================================

#[test]
fn test_gt_squash_preserves_ai_lines() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create branch with first commit
    let mut file = repo.filename("squash.txt");
    file.set_contents(crate::lines!["ai line 1".ai(), "human line 1"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "squash-branch", "-m", "first commit"])
        .expect("gt create should succeed");

    // Add more content via gt modify --commit (creates a second commit on the branch)
    file.set_contents(crate::lines![
        "ai line 1".ai(),
        "human line 1",
        "ai line 2".ai(),
        "human line 2",
    ]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["modify", "--commit", "-m", "second commit"])
        .expect("gt modify --commit should succeed");

    // Squash all commits in the branch
    gt(&repo, &["squash", "-m", "squashed"]).expect("gt squash should succeed");
    assert_head_branch(&repo, "squash-branch");
    assert_worktree_clean(&repo);

    // Verify all attribution is preserved after squash
    file.assert_lines_and_blame(crate::lines![
        "ai line 1".ai(),
        "human line 1".human(),
        "ai line 2".ai(),
        "human line 2".human(),
    ]);
}

#[test]
fn test_gt_squash_mixed_ai_human_across_commits() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // First commit: all human
    let file_path = repo.path().join("squash_mixed.txt");
    std::fs::write(&file_path, "human only line 1\nhuman only line 2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "squash_mixed.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "squash-mixed", "-m", "human commit"]).expect("gt create should succeed");
    let mut file = crate::repos::test_file::TestFile::from_existing_file(file_path, &repo);

    // Second commit: add AI lines
    file.set_contents(crate::lines![
        "human only line 1",
        "human only line 2",
        "ai line 3".ai(),
        "ai line 4".ai(),
    ]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["modify", "--commit", "-m", "ai commit"])
        .expect("gt modify --commit should succeed");

    // Squash
    gt(&repo, &["squash", "-m", "squashed mixed"]).expect("gt squash should succeed");
    assert_head_branch(&repo, "squash-mixed");
    assert_worktree_clean(&repo);

    file.assert_lines_and_blame(crate::lines![
        "human only line 1".human(),
        "human only line 2".human(),
        "ai line 3".ai(),
        "ai line 4".ai(),
    ]);
}

// ===========================================================================
// Group 4: gt restack — Rebase stack operations
// ===========================================================================

/// `gt restack` uses `git commit-tree` + `git update-ref`.
/// The daemon observes the `update-ref` trace2 event and remaps authorship notes.
#[test]
fn test_gt_restack_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create branch-1
    let mut file1 = repo.filename("restack1.txt");
    file1.set_contents(crate::lines!["branch1 ai".ai(), "branch1 human"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "restack-1", "-m", "branch 1"]).expect("gt create should succeed");

    // Create branch-2 stacked on branch-1
    let mut file2 = repo.filename("restack2.txt");
    file2.set_contents(crate::lines!["branch2 ai".ai(), "branch2 human"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "restack-2", "-m", "branch 2"]).expect("gt create should succeed");

    // Go to main and add a commit to simulate trunk advancing
    repo.git(&["checkout", "main"]).unwrap();
    let mut main_file = repo.filename("main_update.txt");
    main_file.set_contents(crate::lines!["main update"]);
    repo.stage_all_and_commit("main update").unwrap();

    // Go back to the top of the stack and restack
    gt(&repo, &["checkout", "restack-2"]).unwrap();
    gt(&repo, &["restack"]).expect("gt restack should succeed");

    // Verify attribution on both branches
    file2.assert_lines_and_blame(crate::lines!["branch2 ai".ai(), "branch2 human".human(),]);

    gt(&repo, &["checkout", "restack-1"]).unwrap();
    file1.assert_lines_and_blame(crate::lines!["branch1 ai".ai(), "branch1 human".human(),]);
}

/// `gt restack` with a 3-branch stack — verifies attribution is preserved across
/// the full stack after restacking via `commit-tree` + `update-ref`.
#[test]
fn test_gt_restack_three_branch_stack() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create 3-branch stack, each with mixed AI/human content
    let mut file_a = repo.filename("a.txt");
    file_a.set_contents(crate::lines!["a ai".ai(), "a human"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "stack-a", "-m", "branch a"]).expect("gt create a should succeed");

    let mut file_b = repo.filename("b.txt");
    file_b.set_contents(crate::lines!["b ai".ai(), "b human"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "stack-b", "-m", "branch b"]).expect("gt create b should succeed");

    let mut file_c = repo.filename("c.txt");
    file_c.set_contents(crate::lines!["c ai".ai(), "c human"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "stack-c", "-m", "branch c"]).expect("gt create c should succeed");

    // Restack from the top
    gt(&repo, &["restack"]).expect("gt restack should succeed");

    // Verify all branches
    file_c.assert_lines_and_blame(crate::lines!["c ai".ai(), "c human".human()]);

    gt(&repo, &["checkout", "stack-b"]).unwrap();
    file_b.assert_lines_and_blame(crate::lines!["b ai".ai(), "b human".human()]);

    gt(&repo, &["checkout", "stack-a"]).unwrap();
    file_a.assert_lines_and_blame(crate::lines!["a ai".ai(), "a human".human()]);
}

// ===========================================================================
// Group 5: gt fold — Fold branch into parent
// ===========================================================================

#[test]
fn test_gt_fold_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create parent branch with human content
    let mut parent_file = repo.filename("fold_parent.txt");
    parent_file.set_contents(crate::lines!["parent human line 1", "parent human line 2"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "fold-parent", "-m", "parent branch"])
        .expect("gt create parent should succeed");

    // Create child branch with AI content
    let mut child_file = repo.filename("fold_child.txt");
    child_file.set_contents(crate::lines!["child ai line".ai(), "child human line"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "fold-child", "-m", "child branch"])
        .expect("gt create child should succeed");

    // Fold child into parent
    gt(&repo, &["fold"]).expect("gt fold should succeed");
    assert_head_branch(&repo, "fold-parent");
    assert_worktree_clean(&repo);

    // After fold, we should be on fold-parent with both files
    // and all attribution preserved
    parent_file.assert_lines_and_blame(crate::lines![
        "parent human line 1".human(),
        "parent human line 2".human(),
    ]);
    child_file.assert_lines_and_blame(crate::lines![
        "child ai line".ai(),
        "child human line".human(),
    ]);
}

#[test]
fn test_gt_fold_with_mixed_content() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create parent branch with a file containing mixed content
    let file_path = repo.path().join("fold_mixed.txt");
    std::fs::write(&file_path, "parent line 1\nparent line 2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "fold_mixed.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "fold-mixed-parent", "-m", "parent"]).expect("gt create should succeed");
    let mut file = crate::repos::test_file::TestFile::from_existing_file(file_path, &repo);

    // Create child that modifies the same file
    file.set_contents(crate::lines![
        "parent line 1",
        "parent line 2",
        "child ai addition".ai(),
    ]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "fold-mixed-child", "-m", "child"]).expect("gt create should succeed");

    // Fold child into parent
    gt(&repo, &["fold"]).expect("gt fold should succeed");
    assert_head_branch(&repo, "fold-mixed-parent");
    assert_worktree_clean(&repo);

    file.assert_lines_and_blame(crate::lines![
        "parent line 1".human(),
        "parent line 2".human(),
        "child ai addition".ai(),
    ]);
}

// ===========================================================================
// Group 6: gt move — Move branch to new parent
// ===========================================================================

/// `gt move` uses `git commit-tree` + `git update-ref`.
/// The daemon observes the `update-ref` trace2 event and remaps authorship notes.
#[test]
fn test_gt_move_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create branch-a off main
    let mut file_a = repo.filename("move_a.txt");
    file_a.set_contents(crate::lines!["a ai line".ai(), "a human line"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "move-a", "-m", "branch a"]).expect("gt create a should succeed");

    // Go back to main
    gt(&repo, &["checkout", "main"]).unwrap();

    // Create branch-b off main
    let mut file_b = repo.filename("move_b.txt");
    file_b.set_contents(crate::lines!["b ai line".ai(), "b human line"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "move-b", "-m", "branch b"]).expect("gt create b should succeed");

    // Move branch-b onto branch-a (rebase b onto a)
    gt(&repo, &["move", "--onto", "move-a"]).expect("gt move should succeed");

    // Verify attribution on branch-b after move
    file_b.assert_lines_and_blame(crate::lines!["b ai line".ai(), "b human line".human(),]);

    // Verify branch-a attribution is unchanged
    gt(&repo, &["checkout", "move-a"]).unwrap();
    file_a.assert_lines_and_blame(crate::lines!["a ai line".ai(), "a human line".human(),]);
}

// ===========================================================================
// Group 7: gt split --by-file — Split branch by files
// ===========================================================================

/// KNOWN_ISSUE:COMMIT_TREE — `gt split` uses `git commit-tree` + `git update-ref`
/// instead of `git rebase`, bypassing git-ai attribution tracking entirely.
#[test]
#[ignore]
fn test_gt_split_by_file_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create branch with two files (one AI, one human)
    let mut ai_file = repo.filename("split_ai.txt");
    ai_file.set_contents(crate::lines!["ai content 1".ai(), "ai content 2".ai()]);
    let mut human_file = repo.filename("split_human.txt");
    human_file.set_contents(crate::lines!["human content 1", "human content 2"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(
        &repo,
        &["create", "split-branch", "-m", "branch with two files"],
    )
    .expect("gt create should succeed");

    // Split by file — extract the AI file into a new parent branch
    gt(&repo, &["split", "--by-file", "split_ai.txt"]).expect("gt split --by-file should succeed");

    // After split, we should be able to verify the AI file on the new parent
    // and the human file remains on the current branch
    // The exact branch structure depends on gt's behavior, but attribution
    // on both files should be preserved regardless of which branch we're on
    ai_file.assert_lines_and_blame(crate::lines!["ai content 1".ai(), "ai content 2".ai(),]);
}

// ===========================================================================
// Group 8: gt absorb — Absorb changes into stack
// ===========================================================================

/// KNOWN_ISSUE:COMMIT_TREE — `gt absorb` uses `git commit-tree` + `git update-ref`
/// to amend earlier commits, bypassing git-ai attribution tracking entirely.
#[test]
#[ignore]
fn test_gt_absorb_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create a branch with a file
    let mut file = repo.filename("absorb.txt");
    file.set_contents(crate::lines!["line 1", "line 2", "line 3",]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "absorb-branch", "-m", "initial file"])
        .expect("gt create should succeed");

    // Create a second branch stacked on top
    let mut file2 = repo.filename("absorb2.txt");
    file2.set_contents(crate::lines!["other file content".ai()]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "absorb-child", "-m", "child branch"])
        .expect("gt create child should succeed");

    // Modify absorb.txt (change that should be absorbed into absorb-branch)
    file.set_contents(crate::lines!["line 1 modified".ai(), "line 2", "line 3",]);
    repo.git(&["add", "-A"]).unwrap();

    // Absorb the change into the correct commit
    gt(&repo, &["absorb", "--force"]).expect("gt absorb should succeed");

    // Verify attribution after absorb
    file.assert_lines_and_blame(crate::lines![
        "line 1 modified".ai(),
        "line 2".human(),
        "line 3".human(),
    ]);
}

// ===========================================================================
// Group 9: Navigation commands — Verify no attribution side effects
// ===========================================================================

#[test]
fn test_gt_navigation_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create a 3-branch stack
    let mut file1 = repo.filename("nav1.txt");
    file1.set_contents(crate::lines!["nav1 ai".ai(), "nav1 human"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "nav-1", "-m", "nav branch 1"]).expect("gt create should succeed");

    let mut file2 = repo.filename("nav2.txt");
    file2.set_contents(crate::lines!["nav2 ai".ai(), "nav2 human"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "nav-2", "-m", "nav branch 2"]).expect("gt create should succeed");

    let mut file3 = repo.filename("nav3.txt");
    file3.set_contents(crate::lines!["nav3 ai".ai(), "nav3 human"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "nav-3", "-m", "nav branch 3"]).expect("gt create should succeed");

    // Navigate with gt down
    gt(&repo, &["down"]).expect("gt down should succeed");
    file2.assert_lines_and_blame(crate::lines!["nav2 ai".ai(), "nav2 human".human()]);

    // Navigate with gt down again
    gt(&repo, &["down"]).expect("gt down should succeed");
    file1.assert_lines_and_blame(crate::lines!["nav1 ai".ai(), "nav1 human".human()]);

    // Navigate with gt up
    gt(&repo, &["up"]).expect("gt up should succeed");
    file2.assert_lines_and_blame(crate::lines!["nav2 ai".ai(), "nav2 human".human()]);

    // Navigate with gt top
    gt(&repo, &["top"]).expect("gt top should succeed");
    file3.assert_lines_and_blame(crate::lines!["nav3 ai".ai(), "nav3 human".human()]);

    // Navigate with gt bottom
    gt(&repo, &["bottom"]).expect("gt bottom should succeed");
    file1.assert_lines_and_blame(crate::lines!["nav1 ai".ai(), "nav1 human".human()]);

    // Navigate with gt checkout
    gt(&repo, &["checkout", "nav-2"]).expect("gt checkout should succeed");
    file2.assert_lines_and_blame(crate::lines!["nav2 ai".ai(), "nav2 human".human()]);
}

// ===========================================================================
// Group 10: gt delete / gt pop — Branch deletion
// ===========================================================================

/// `gt delete` requires an interactive terminal even with `--force`.
#[test]
#[ignore]
fn test_gt_delete_restacks_children_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create 3-branch stack: main -> branch-a -> branch-b -> branch-c
    let mut file_a = repo.filename("del_a.txt");
    file_a.set_contents(crate::lines!["a content"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "del-a", "-m", "branch a"]).expect("gt create a should succeed");

    let mut file_b = repo.filename("del_b.txt");
    file_b.set_contents(crate::lines!["b ai content".ai()]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "del-b", "-m", "branch b"]).expect("gt create b should succeed");

    let mut file_c = repo.filename("del_c.txt");
    file_c.set_contents(crate::lines!["c ai content".ai(), "c human content"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "del-c", "-m", "branch c"]).expect("gt create c should succeed");

    // Delete the middle branch (branch-b)
    gt(&repo, &["checkout", "del-b"]).unwrap();
    gt(&repo, &["delete", "--force"]).expect("gt delete should succeed");

    // branch-c should have been restacked onto branch-a
    // Navigate to branch-c and verify attribution
    gt(&repo, &["checkout", "del-c"]).unwrap();
    file_c.assert_lines_and_blame(crate::lines![
        "c ai content".ai(),
        "c human content".human(),
    ]);
}

#[test]
fn test_gt_pop_retains_working_tree() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create branch with AI content
    let mut file = repo.filename("pop.txt");
    file.set_contents(crate::lines!["pop ai line".ai(), "pop human line"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "pop-branch", "-m", "pop branch"]).expect("gt create should succeed");

    // Pop the branch — should delete branch but retain file changes in working tree
    gt(&repo, &["pop"]).expect("gt pop should succeed");

    // The file should still exist in working tree
    let content = repo.read_file("pop.txt");
    assert!(content.is_some(), "File should still exist after gt pop");

    // Re-commit the changes and verify attribution
    repo.stage_all_and_commit("re-commit after pop").unwrap();
    file.assert_lines_and_blame(crate::lines!["pop ai line".ai(), "pop human line".human(),]);
}

// ===========================================================================
// Group 11: gt rename — Branch rename
// ===========================================================================

#[test]
fn test_gt_rename_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create branch with AI content
    let mut file = repo.filename("rename.txt");
    file.set_contents(crate::lines!["rename ai".ai(), "rename human"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "old-name", "-m", "branch to rename"]).expect("gt create should succeed");

    // Rename the branch
    gt(&repo, &["rename", "new-name"]).expect("gt rename should succeed");
    assert_head_branch(&repo, "new-name");
    assert_worktree_clean(&repo);

    // Verify attribution is unchanged
    file.assert_lines_and_blame(crate::lines!["rename ai".ai(), "rename human".human(),]);

    // Verify we're on the new branch name.
    assert_head_branch(&repo, "new-name");
}

// ===========================================================================
// Group 12: gt track / gt untrack — Metadata operations
// ===========================================================================

#[test]
fn test_gt_track_untrack_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create a regular git branch (not via gt)
    repo.git(&["checkout", "-b", "manual-branch"]).unwrap();
    let mut file = repo.filename("track.txt");
    file.set_contents(crate::lines!["track ai".ai(), "track human"]);
    repo.stage_all_and_commit("manual branch commit").unwrap();

    // Track it with Graphite
    gt(&repo, &["track", "--parent", "main"]).expect("gt track should succeed");
    assert_head_branch(&repo, "manual-branch");
    assert_worktree_clean(&repo);

    // Verify attribution after tracking
    file.assert_lines_and_blame(crate::lines!["track ai".ai(), "track human".human(),]);

    // Untrack
    gt(&repo, &["untrack"]).expect("gt untrack should succeed");
    assert_head_branch(&repo, "manual-branch");
    assert_worktree_clean(&repo);

    // Verify attribution after untracking
    file.assert_lines_and_blame(crate::lines!["track ai".ai(), "track human".human(),]);
}

// ===========================================================================
// Group 13: gt undo — Undo operations
// ===========================================================================

/// `gt undo` requires interactive terminal even when `--no-interactive` is passed.
/// Cannot be tested in automated CI.
#[test]
#[ignore]
fn test_gt_undo_create_preserves_attribution() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create base file with AI content
    let mut base_file = repo.filename("undo_base.txt");
    base_file.set_contents(crate::lines!["base ai".ai(), "base human"]);
    repo.stage_all_and_commit("base commit").unwrap();

    // Create a new branch
    let mut new_file = repo.filename("undo_new.txt");
    new_file.set_contents(crate::lines!["new ai".ai()]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "undo-branch", "-m", "branch to undo"])
        .expect("gt create should succeed");

    // Undo the create
    gt(&repo, &["undo"]).expect("gt undo should succeed");

    // Base file attribution should still be correct
    base_file.assert_lines_and_blame(crate::lines!["base ai".ai(), "base human".human(),]);
}

// ===========================================================================
// Group 14: Complex multi-operation workflows
// ===========================================================================

/// Full stack workflow: create 3-branch stack, modify middle branch (triggering child restack
/// via `commit-tree`), and verify attribution is preserved across the entire stack.
#[test]
fn test_gt_full_stack_workflow() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Step 1: Create a 3-branch stack with mixed AI/human content
    let mut file1 = repo.filename("workflow1.txt");
    file1.set_contents(crate::lines![
        "workflow1 ai line 1".ai(),
        "workflow1 human line 1",
        "workflow1 ai line 2".ai(),
    ]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "wf-1", "-m", "workflow branch 1"])
        .expect("gt create wf-1 should succeed");

    let mut file2 = repo.filename("workflow2.txt");
    file2.set_contents(crate::lines![
        "workflow2 human line 1",
        "workflow2 ai line 1".ai(),
    ]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "wf-2", "-m", "workflow branch 2"])
        .expect("gt create wf-2 should succeed");

    let mut file3 = repo.filename("workflow3.txt");
    file3.set_contents(crate::lines![
        "workflow3 ai line 1".ai(),
        "workflow3 ai line 2".ai(),
        "workflow3 human line 1",
    ]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "wf-3", "-m", "workflow branch 3"])
        .expect("gt create wf-3 should succeed");

    // Step 2: Navigate down, modify a middle branch
    gt(&repo, &["checkout", "wf-1"]).unwrap();
    let mut file1_extra = repo.filename("workflow1_extra.txt");
    file1_extra.set_contents(crate::lines!["extra ai".ai()]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["modify", "-m", "modified wf-1 with extra file"])
        .expect("gt modify should succeed");

    // Step 3: Verify the entire stack has correct attribution
    gt(&repo, &["checkout", "wf-3"]).unwrap();

    file1.assert_lines_and_blame(crate::lines![
        "workflow1 ai line 1".ai(),
        "workflow1 human line 1".human(),
        "workflow1 ai line 2".ai(),
    ]);
    file2.assert_lines_and_blame(crate::lines![
        "workflow2 human line 1".human(),
        "workflow2 ai line 1".ai(),
    ]);
    file3.assert_lines_and_blame(crate::lines![
        "workflow3 ai line 1".ai(),
        "workflow3 ai line 2".ai(),
        "workflow3 human line 1".human(),
    ]);
}

#[test]
fn test_gt_create_then_squash_then_fold() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create parent with human content
    let mut parent_file = repo.filename("csf_parent.txt");
    parent_file.set_contents(crate::lines!["parent line 1", "parent line 2"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "csf-parent", "-m", "parent"]).expect("gt create should succeed");

    // Create child with AI content (two commits)
    let mut child_file = repo.filename("csf_child.txt");
    child_file.set_contents(crate::lines!["child ai 1".ai()]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "csf-child", "-m", "child commit 1"]).expect("gt create should succeed");

    child_file.set_contents(crate::lines!["child ai 1".ai(), "child ai 2".ai()]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["modify", "--commit", "-m", "child commit 2"])
        .expect("gt modify --commit should succeed");

    // Squash child commits
    gt(&repo, &["squash", "-m", "squashed child"]).expect("gt squash should succeed");

    // Fold child into parent
    gt(&repo, &["fold"]).expect("gt fold should succeed");

    // Verify all attribution on the now-combined branch
    parent_file.assert_lines_and_blame(crate::lines![
        "parent line 1".human(),
        "parent line 2".human(),
    ]);
    child_file.assert_lines_and_blame(crate::lines!["child ai 1".ai(), "child ai 2".ai(),]);
}

#[test]
fn test_gt_create_with_all_flag() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Make changes without staging
    let mut file = repo.filename("all_flag.txt");
    file.set_contents(crate::lines![
        "human line 1",
        "ai line 1".ai(),
        "human line 2",
    ]);

    // Use gt create -a to auto-stage all changes
    gt(
        &repo,
        &["create", "all-flag-branch", "-a", "-m", "auto stage"],
    )
    .expect("gt create -a should succeed");
    assert_head_branch(&repo, "all-flag-branch");
    assert_worktree_clean(&repo);

    file.assert_lines_and_blame(crate::lines![
        "human line 1".human(),
        "ai line 1".ai(),
        "human line 2".human(),
    ]);
}

#[test]
fn test_gt_modify_all_flag() {
    require_gt!();
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    gt_init(&repo);

    // Create initial branch
    let mut file = repo.filename("modify_all.txt");
    file.set_contents(crate::lines!["initial line"]);
    repo.git(&["add", "-A"]).unwrap();
    gt(&repo, &["create", "modify-all-branch", "-m", "initial"]).expect("gt create should succeed");

    // Modify with unstaged changes, use -a flag
    file.set_contents(crate::lines!["initial line", "ai addition".ai()]);
    gt(&repo, &["modify", "-a", "-m", "modified with ai"]).expect("gt modify -a should succeed");
    assert_head_branch(&repo, "modify-all-branch");
    assert_worktree_clean(&repo);

    file.assert_lines_and_blame(crate::lines!["initial line".human(), "ai addition".ai(),]);
}
