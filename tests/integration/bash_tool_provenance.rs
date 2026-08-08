//! Integration tests for AI provenance tracking via bash tool pre/post snapshots.
//!
//! Each test simulates what happens when an AI coding agent executes a bash
//! command: the system takes a pre-snapshot of filesystem metadata, the bash
//! command runs, and then a post-snapshot detects which files changed. This
//! validates that the stat-diff mechanism correctly identifies created,
//! modified, and deleted files across a wide variety of real-world shell
//! commands.

use crate::bash_tool_common::{add_and_commit, post_hook, pre_hook, repo_root};
use crate::repos::test_repo::TestRepo;
#[cfg(unix)]
use crate::repos::write_executable_script;
use git_ai::operations::commands::checkpoint_agent::bash_tool::{
    BashCheckpointAction, BashPostHookResult, diff, git_status_fallback, snapshot,
};
#[cfg(unix)]
use std::fs;
use std::process::Command;
use std::thread;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run a bash command in the repo and assert it succeeds.
fn run_bash(repo: &TestRepo, program: &str, args: &[&str]) -> std::process::Output {
    let output = Command::new(program)
        .args(args)
        .current_dir(repo.path())
        .output()
        .unwrap_or_else(|e| panic!("{} {:?} failed to start: {}", program, args, e));
    assert!(
        output.status.success(),
        "{} {:?} failed: {}",
        program,
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Assert that a BashCheckpointAction::Checkpoint contains the expected path.
fn assert_checkpoint_contains(result: &BashPostHookResult, expected_path: &str) {
    match &result.action {
        BashCheckpointAction::Checkpoint(paths) => {
            assert!(
                paths.iter().any(|p| p.contains(expected_path)),
                "Expected checkpoint to contain '{}'; got {:?}",
                expected_path,
                paths
            );
        }
        BashCheckpointAction::NoChanges => {
            panic!(
                "Expected Checkpoint containing '{}', got NoChanges",
                expected_path
            );
        }
        other => {
            panic!("Expected Checkpoint, got {:?}", other);
        }
    }
}

/// Assert that a BashCheckpointAction::Checkpoint does NOT contain a path.
fn assert_checkpoint_excludes(result: &BashPostHookResult, excluded_path: &str) {
    if let BashCheckpointAction::Checkpoint(paths) = &result.action {
        assert!(
            !paths.iter().any(|p| p.contains(excluded_path)),
            "Expected checkpoint NOT to contain '{}'; got {:?}",
            excluded_path,
            paths
        );
    }
}

/// Assert that a BashCheckpointAction is NoChanges.
fn assert_no_changes(result: &BashPostHookResult) {
    match &result.action {
        BashCheckpointAction::NoChanges => {}
        other => {
            panic!("Expected NoChanges, got {:?}", other);
        }
    }
}

/// Get the checkpoint paths from an action, panicking if not a Checkpoint.
fn checkpoint_paths(result: &BashPostHookResult) -> &[String] {
    match &result.action {
        BashCheckpointAction::Checkpoint(paths) => paths,
        other => panic!("Expected Checkpoint, got {:?}", other),
    }
}

// ===========================================================================
// Category 1: File creation commands
// ===========================================================================

#[test]
fn test_bash_provenance_echo_redirect_creates_file() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "echo-sess", "echo-t1");

    run_bash(&repo, "sh", &["-c", "echo 'hello world' > created.txt"]);

    let post_action = post_hook(&root, "echo-sess", "echo-t1");
    assert_checkpoint_contains(&post_action, "created.txt");
}

#[test]
fn test_bash_provenance_printf_redirect_creates_file() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "printf-sess", "printf-t1");

    run_bash(
        &repo,
        "sh",
        &["-c", "printf 'formatted content' > printf_out.txt"],
    );

    let post_action = post_hook(&root, "printf-sess", "printf-t1");
    assert_checkpoint_contains(&post_action, "printf_out.txt");
}

#[test]
fn test_bash_provenance_heredoc_creates_file() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "heredoc-sess", "heredoc-t1");

    run_bash(
        &repo,
        "sh",
        &[
            "-c",
            "cat > heredoc.txt <<'EOF'\nheredoc content\nline two\nEOF",
        ],
    );

    let post_action = post_hook(&root, "heredoc-sess", "heredoc-t1");
    assert_checkpoint_contains(&post_action, "heredoc.txt");
}

#[test]
fn test_bash_provenance_touch_creates_empty_file() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "touch-sess", "touch-t1");

    run_bash(&repo, "touch", &["newfile.txt"]);

    let post_action = post_hook(&root, "touch-sess", "touch-t1");
    assert_checkpoint_contains(&post_action, "newfile.txt");
}

#[test]
fn test_bash_provenance_cp_creates_copy() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "existing.txt", "original content", "initial commit");

    pre_hook(&root, "cp-sess", "cp-t1");

    run_bash(&repo, "cp", &["existing.txt", "copy.txt"]);

    let post_action = post_hook(&root, "cp-sess", "cp-t1");
    assert_checkpoint_contains(&post_action, "copy.txt");
    assert_checkpoint_excludes(&post_action, "existing.txt");
}

#[test]
fn test_bash_provenance_tee_creates_file() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "tee-sess", "tee-t1");

    run_bash(
        &repo,
        "sh",
        &["-c", "echo content | tee output.txt > /dev/null"],
    );

    let post_action = post_hook(&root, "tee-sess", "tee-t1");
    assert_checkpoint_contains(&post_action, "output.txt");
}

#[test]
fn test_bash_provenance_nested_directory_creation() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "nested-sess", "nested-t1");

    run_bash(
        &repo,
        "sh",
        &[
            "-c",
            "mkdir -p src/deep/nested && touch src/deep/nested/mod.rs",
        ],
    );

    let post_action = post_hook(&root, "nested-sess", "nested-t1");
    assert_checkpoint_contains(&post_action, "mod.rs");
}

// ===========================================================================
// Category 2: File modification commands
// ===========================================================================

#[test]
fn test_bash_provenance_sed_in_place_edit() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "target.txt", "old value here", "initial commit");

    pre_hook(&root, "sed-sess", "sed-t1");

    thread::sleep(Duration::from_millis(50));
    run_bash(
        &repo,
        "sh",
        &[
            "-c",
            "sed -i.bak 's/old/new/g' target.txt && rm -f target.txt.bak",
        ],
    );

    let post_action = post_hook(&root, "sed-sess", "sed-t1");
    assert_checkpoint_contains(&post_action, "target.txt");
}

#[test]
fn test_bash_provenance_append_with_redirect() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "log.txt", "line one\n", "initial commit");

    pre_hook(&root, "append-sess", "append-t1");

    thread::sleep(Duration::from_millis(50));
    run_bash(&repo, "sh", &["-c", "echo 'appended line' >> log.txt"]);

    let post_action = post_hook(&root, "append-sess", "append-t1");
    assert_checkpoint_contains(&post_action, "log.txt");
}

#[test]
fn test_bash_provenance_truncate_to_zero() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(
        &repo,
        "data.txt",
        "lots of data here that will be erased",
        "initial commit",
    );

    pre_hook(&root, "trunc-sess", "trunc-t1");

    thread::sleep(Duration::from_millis(50));
    run_bash(&repo, "sh", &["-c", ": > data.txt"]);

    let post_action = post_hook(&root, "trunc-sess", "trunc-t1");
    assert_checkpoint_contains(&post_action, "data.txt");
}

#[cfg(unix)]
#[test]
fn test_bash_provenance_chmod_permission_change() {
    use git_ai::operations::commands::checkpoint_agent::bash_tool::diff;
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "script.sh", "#!/bin/bash\necho hi", "initial commit");

    let pre = snapshot(&root, "chmod-sess", "chmod-t1", None).unwrap();

    run_bash(&repo, "chmod", &["+x", "script.sh"]);

    let post = snapshot(&root, "chmod-sess", "chmod-t2", None).unwrap();
    let result = diff(&pre, &post);
    assert!(
        result
            .modified
            .iter()
            .any(|p| p.display().to_string().contains("script.sh")),
        "chmod should be detected via stat-tuple diff; got created={:?} modified={:?}",
        result.created,
        result.modified,
    );
}

#[test]
fn test_bash_provenance_mv_rename() {
    use git_ai::operations::commands::checkpoint_agent::bash_tool::diff;
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "old_name.txt", "rename me", "initial commit");

    let pre = snapshot(&root, "mv-sess", "mv-t1", None).unwrap();

    run_bash(&repo, "mv", &["old_name.txt", "new_name.txt"]);

    let post = snapshot(&root, "mv-sess", "mv-t2", None).unwrap();
    let result = diff(&pre, &post);
    assert!(
        result
            .created
            .iter()
            .any(|p| p.display().to_string().contains("new_name.txt")),
        "new_name.txt should appear as created after rename; got created={:?}",
        result.created,
    );
}

// ===========================================================================
// Category 3: File deletion commands
// ===========================================================================

// ===========================================================================
// Category 4: Build/compile tool simulations
// ===========================================================================

#[test]
fn test_bash_provenance_simulated_cargo_init() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "cargo-sess", "cargo-t1");

    run_bash(
        &repo,
        "sh",
        &[
            "-c",
            "mkdir -p myproject/src && echo 'fn main() {}' > myproject/src/main.rs && printf '[package]\\nname=\"myproject\"' > myproject/Cargo.toml",
        ],
    );

    let post_action = post_hook(&root, "cargo-sess", "cargo-t1");
    assert_checkpoint_contains(&post_action, "main.rs");
    // On macOS, paths are case-normalized to lowercase, so check for lowercase.
    let paths = checkpoint_paths(&post_action);
    assert!(
        paths
            .iter()
            .any(|p| p.to_lowercase().contains("cargo.toml")),
        "Cargo.toml (case-insensitive) should appear in checkpoint; got {:?}",
        paths
    );
}

#[test]
fn test_bash_provenance_simulated_npm_init() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "npm-sess", "npm-t1");

    run_bash(
        &repo,
        "sh",
        &[
            "-c",
            r#"echo '{"name":"test","version":"1.0.0"}' > package.json"#,
        ],
    );

    let post_action = post_hook(&root, "npm-sess", "npm-t1");
    assert_checkpoint_contains(&post_action, "package.json");
}

// ===========================================================================
// Category 5: Git commands (that modify working tree)
// ===========================================================================

#[test]
fn test_bash_provenance_git_checkout_restore() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(
        &repo,
        "restorable.txt",
        "original content",
        "initial commit",
    );

    // Modify the file so git checkout -- will revert it
    thread::sleep(Duration::from_millis(50));
    repo.write_file("restorable.txt", "modified content");

    pre_hook(&root, "checkout-sess", "checkout-t1");

    thread::sleep(Duration::from_millis(50));
    // Use git_og to bypass hooks, simulating what a bash command would do
    repo.git_og(&["checkout", "--", "restorable.txt"])
        .expect("git checkout should succeed");

    let post_action = post_hook(&root, "checkout-sess", "checkout-t1");
    assert_checkpoint_contains(&post_action, "restorable.txt");
}

#[test]
fn test_bash_provenance_git_stash_pop() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "stashed.txt", "original", "initial commit");

    // Modify and stash
    thread::sleep(Duration::from_millis(50));
    repo.write_file("stashed.txt", "modified for stash");
    repo.git_og(&["add", "stashed.txt"])
        .expect("git add should succeed");
    repo.git_og(&["stash", "push", "-m", "test stash"])
        .expect("git stash should succeed");

    pre_hook(&root, "stash-sess", "stash-t1");

    thread::sleep(Duration::from_millis(50));
    repo.git_og(&["stash", "pop"])
        .expect("git stash pop should succeed");

    let post_action = post_hook(&root, "stash-sess", "stash-t1");
    assert_checkpoint_contains(&post_action, "stashed.txt");
}

#[test]
fn test_bash_provenance_git_apply_patch() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(
        &repo,
        "patchme.txt",
        "line one\nline two\nline three\n",
        "initial",
    );

    // Create a patch file
    let patch_content = "\
--- a/patchme.txt
+++ b/patchme.txt
@@ -1,3 +1,3 @@
 line one
-line two
+line TWO PATCHED
 line three
";
    repo.write_file("fix.patch", patch_content);

    pre_hook(&root, "patch-sess", "patch-t1");

    thread::sleep(Duration::from_millis(50));
    repo.git_og(&["apply", "fix.patch"])
        .expect("git apply should succeed");

    let post_action = post_hook(&root, "patch-sess", "patch-t1");
    assert_checkpoint_contains(&post_action, "patchme.txt");
}

// ===========================================================================
// Category 6: Multi-command pipelines
// ===========================================================================

#[test]
fn test_bash_provenance_loop_creating_multiple_files() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "loop-sess", "loop-t1");

    run_bash(
        &repo,
        "sh",
        &[
            "-c",
            "for f in a.txt b.txt c.txt; do echo 'content' > $f; done",
        ],
    );

    let post_action = post_hook(&root, "loop-sess", "loop-t1");
    assert_checkpoint_contains(&post_action, "a.txt");
    assert_checkpoint_contains(&post_action, "b.txt");
    assert_checkpoint_contains(&post_action, "c.txt");
}

#[test]
fn test_bash_provenance_grep_sed_pipeline() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "file1.txt", "old pattern here", "add file1");
    add_and_commit(&repo, "file2.txt", "old pattern there", "add file2");
    add_and_commit(&repo, "file3.txt", "no match", "add file3");

    pre_hook(&root, "pipeline-sess", "pipeline-t1");

    thread::sleep(Duration::from_millis(50));
    run_bash(
        &repo,
        "sh",
        &[
            "-c",
            "grep -rl 'old' --include='*.txt' . | xargs sed -i.bak 's/old/new/g' && find . -name '*.bak' -delete",
        ],
    );

    let post_action = post_hook(&root, "pipeline-sess", "pipeline-t1");
    assert_checkpoint_contains(&post_action, "file1.txt");
    assert_checkpoint_contains(&post_action, "file2.txt");
    assert_checkpoint_excludes(&post_action, "file3.txt");
}

// ===========================================================================
// Category 7: Read-only commands (should produce NoChanges)
// ===========================================================================

#[test]
fn test_bash_provenance_cat_is_readonly() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "readable.txt", "read me", "initial commit");

    pre_hook(&root, "cat-sess", "cat-t1");

    run_bash(&repo, "cat", &["readable.txt"]);

    let post_action = post_hook(&root, "cat-sess", "cat-t1");
    assert_no_changes(&post_action);
}

#[test]
fn test_bash_provenance_ls_is_readonly() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "visible.txt", "content", "initial commit");

    pre_hook(&root, "ls-sess", "ls-t1");

    run_bash(&repo, "ls", &["-la"]);

    let post_action = post_hook(&root, "ls-sess", "ls-t1");
    assert_no_changes(&post_action);
}

#[test]
#[cfg(not(target_os = "windows"))] // Windows `find` is not POSIX find
fn test_bash_provenance_find_is_readonly() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "src/main.rs", "fn main() {}", "initial commit");

    pre_hook(&root, "find-sess", "find-t1");

    run_bash(&repo, "find", &[".", "-name", "*.rs"]);

    let post_action = post_hook(&root, "find-sess", "find-t1");
    assert_no_changes(&post_action);
}

#[test]
fn test_bash_provenance_grep_is_readonly() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(
        &repo,
        "searchable.txt",
        "pattern match here",
        "initial commit",
    );

    pre_hook(&root, "grep-sess", "grep-t1");

    // grep may exit non-zero if no match, so use sh -c with || true
    run_bash(
        &repo,
        "sh",
        &["-c", "grep 'pattern' searchable.txt || true"],
    );

    let post_action = post_hook(&root, "grep-sess", "grep-t1");
    assert_no_changes(&post_action);
}

#[test]
fn test_bash_provenance_wc_is_readonly() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "countme.txt", "one\ntwo\nthree\n", "initial commit");

    pre_hook(&root, "wc-sess", "wc-t1");

    run_bash(&repo, "wc", &["-l", "countme.txt"]);

    let post_action = post_hook(&root, "wc-sess", "wc-t1");
    assert_no_changes(&post_action);
}

#[test]
fn test_bash_provenance_head_is_readonly() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(
        &repo,
        "longfile.txt",
        "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\n",
        "initial commit",
    );

    pre_hook(&root, "head-sess", "head-t1");

    run_bash(&repo, "head", &["-5", "longfile.txt"]);

    let post_action = post_hook(&root, "head-sess", "head-t1");
    assert_no_changes(&post_action);
}

#[test]
fn test_bash_provenance_diff_is_readonly() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "file1.txt", "alpha\nbeta\n", "add file1");
    add_and_commit(&repo, "file2.txt", "alpha\ngamma\n", "add file2");

    pre_hook(&root, "diff-sess", "diff-t1");

    // diff returns non-zero when files differ, so use || true
    run_bash(&repo, "sh", &["-c", "diff file1.txt file2.txt || true"]);

    let post_action = post_hook(&root, "diff-sess", "diff-t1");
    assert_no_changes(&post_action);
}

#[test]
fn test_bash_provenance_git_log_is_readonly() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "gitlog-sess", "gitlog-t1");

    repo.git_og(&["log", "--oneline"])
        .expect("git log should succeed");

    let post_action = post_hook(&root, "gitlog-sess", "gitlog-t1");
    assert_no_changes(&post_action);
}

#[test]
fn test_bash_provenance_git_diff_is_readonly() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "gitdiff-sess", "gitdiff-t1");

    repo.git_og(&["diff"]).expect("git diff should succeed");

    let post_action = post_hook(&root, "gitdiff-sess", "gitdiff-t1");
    assert_no_changes(&post_action);
}

#[test]
fn test_bash_provenance_git_status_is_readonly() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "gitstatus-sess", "gitstatus-t1");

    repo.git_og(&["status"]).expect("git status should succeed");

    let post_action = post_hook(&root, "gitstatus-sess", "gitstatus-t1");
    assert_no_changes(&post_action);
}

#[test]
fn test_bash_provenance_compound_readonly() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "compound-sess", "compound-t1");

    run_bash(&repo, "sh", &["-c", "pwd && ls"]);

    let post_action = post_hook(&root, "compound-sess", "compound-t1");
    assert_no_changes(&post_action);
}

// ===========================================================================
// Category 8: Symlink operations (unix only)
// ===========================================================================

#[cfg(unix)]
#[test]
fn test_bash_provenance_symlink_creation() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "target.txt", "symlink target", "initial commit");

    pre_hook(&root, "symlink-sess", "symlink-t1");

    run_bash(&repo, "ln", &["-s", "target.txt", "link.txt"]);

    let post_action = post_hook(&root, "symlink-sess", "symlink-t1");
    assert_checkpoint_contains(&post_action, "link.txt");
}

#[cfg(unix)]
#[test]
fn test_bash_provenance_symlink_target_change() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "target_a.txt", "target a", "add target a");
    add_and_commit(&repo, "target_b.txt", "target b", "add target b");

    // Create the symlink pointing to target_a
    run_bash(&repo, "ln", &["-s", "target_a.txt", "mylink.txt"]);
    // Commit the symlink so it is tracked
    repo.git_og(&["add", "mylink.txt"])
        .expect("git add symlink should succeed");
    repo.git_og(&["commit", "-m", "add symlink"])
        .expect("git commit symlink should succeed");

    pre_hook(&root, "symtgt-sess", "symtgt-t1");

    // Re-point the symlink to target_b
    run_bash(
        &repo,
        "sh",
        &["-c", "rm mylink.txt && ln -s target_b.txt mylink.txt"],
    );

    let post_action = post_hook(&root, "symtgt-sess", "symtgt-t1");
    assert_checkpoint_contains(&post_action, "mylink.txt");
}

// ===========================================================================
// Category 9: Large/batch operations
// ===========================================================================

#[test]
fn test_bash_provenance_create_50_files() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "batch50-sess", "batch50-t1");

    run_bash(
        &repo,
        "sh",
        &[
            "-c",
            "for i in $(seq 1 50); do echo \"file $i\" > \"batch_$i.txt\"; done",
        ],
    );

    let post_action = post_hook(&root, "batch50-sess", "batch50-t1");
    let paths = checkpoint_paths(&post_action);
    assert!(
        paths.len() >= 50,
        "Expected at least 50 created files in checkpoint; got {} paths: {:?}",
        paths.len(),
        paths
    );
    // Spot-check a few
    assert_checkpoint_contains(&post_action, "batch_1.txt");
    assert_checkpoint_contains(&post_action, "batch_25.txt");
    assert_checkpoint_contains(&post_action, "batch_50.txt");
}

#[test]
fn test_bash_provenance_modify_20_of_50_tracked() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    // Create and commit 50 files
    for i in 1..=50 {
        let name = format!("tracked_{}.txt", i);
        add_and_commit(
            &repo,
            &name,
            &format!("original {}", i),
            &format!("add {}", name),
        );
    }

    pre_hook(&root, "mod20-sess", "mod20-t1");

    thread::sleep(Duration::from_millis(50));
    // Modify only files 1-20
    run_bash(
        &repo,
        "sh",
        &[
            "-c",
            "for i in $(seq 1 20); do echo 'modified' > \"tracked_$i.txt\"; done",
        ],
    );

    let post_action = post_hook(&root, "mod20-sess", "mod20-t1");
    let paths = checkpoint_paths(&post_action);

    // Exactly 20 files should be modified
    assert_eq!(
        paths.len(),
        20,
        "Expected exactly 20 modified files; got {} paths: {:?}",
        paths.len(),
        paths
    );

    // Verify modified files are present
    assert_checkpoint_contains(&post_action, "tracked_1.txt");
    assert_checkpoint_contains(&post_action, "tracked_20.txt");

    // Verify unmodified files are NOT present
    assert_checkpoint_excludes(&post_action, "tracked_21.txt");
    assert_checkpoint_excludes(&post_action, "tracked_50.txt");
}

// ===========================================================================
// Category 10: Edge cases
// ===========================================================================

#[test]
fn test_bash_provenance_failed_command_with_partial_output() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "fail-sess", "fail-t1");

    // Command that creates a file then fails. We use || true so run_bash
    // does not panic, but the file is still created.
    run_bash(
        &repo,
        "sh",
        &["-c", "echo 'partial' > partial.txt && false || true"],
    );

    let post_action = post_hook(&root, "fail-sess", "fail-t1");
    assert_checkpoint_contains(&post_action, "partial.txt");
}

#[test]
fn test_bash_provenance_file_with_spaces_in_name() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "spaces-sess", "spaces-t1");

    run_bash(&repo, "sh", &["-c", "echo 'x' > 'file with spaces.txt'"]);

    let post_action = post_hook(&root, "spaces-sess", "spaces-t1");
    assert_checkpoint_contains(&post_action, "file with spaces.txt");
}

#[test]
fn test_bash_provenance_file_with_special_characters() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "special-sess", "special-t1");

    run_bash(
        &repo,
        "sh",
        &["-c", "echo 'x' > 'file-with-dashes_and_underscores.txt'"],
    );

    let post_action = post_hook(&root, "special-sess", "special-t1");
    assert_checkpoint_contains(&post_action, "file-with-dashes_and_underscores.txt");
}

#[test]
fn test_bash_provenance_hidden_file_creation() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "hidden-sess", "hidden-t1");

    run_bash(
        &repo,
        "sh",
        &["-c", "echo 'secret config' > .hidden_config"],
    );

    let post_action = post_hook(&root, "hidden-sess", "hidden-t1");
    assert_checkpoint_contains(&post_action, ".hidden_config");
}

#[test]
fn test_bash_provenance_touch_then_write_shows_created() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    pre_hook(&root, "touchwrite-sess", "touchwrite-t1");

    run_bash(
        &repo,
        "sh",
        &[
            "-c",
            "touch empty.txt && echo 'now has content' > empty.txt",
        ],
    );

    let post_action = post_hook(&root, "touchwrite-sess", "touchwrite-t1");
    assert_checkpoint_contains(&post_action, "empty.txt");
}

#[test]
fn test_bash_provenance_overwrite_identical_content_detects_mtime_change() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "same.txt", "identical", "initial commit");

    pre_hook(&root, "identical-sess", "identical-t1");

    // Wait so mtime advances even though content is the same
    thread::sleep(Duration::from_millis(50));
    // Write exact same content but file metadata (mtime) will change
    run_bash(&repo, "sh", &["-c", "echo 'identical' > same.txt"]);

    let post_action = post_hook(&root, "identical-sess", "identical-t1");
    // The stat tuple should differ because mtime changed, even if content is the same.
    // Note: echo adds a trailing newline, so content actually differs from "identical"
    // to "identical\n". Regardless, the stat-tuple approach detects this.
    assert_checkpoint_contains(&post_action, "same.txt");
}

#[test]
fn test_bash_provenance_sequential_tool_uses_same_session() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    // --- First cycle: create alpha.txt ---
    pre_hook(&root, "seq-sess", "seq-use1");

    run_bash(&repo, "sh", &["-c", "echo 'alpha' > alpha.txt"]);

    let post1 = post_hook(&root, "seq-sess", "seq-use1");
    assert_checkpoint_contains(&post1, "alpha.txt");
    assert_checkpoint_excludes(&post1, "beta.txt");

    // --- Second cycle: create beta.txt ---
    pre_hook(&root, "seq-sess", "seq-use2");

    run_bash(&repo, "sh", &["-c", "echo 'beta' > beta.txt"]);

    let post2 = post_hook(&root, "seq-sess", "seq-use2");
    assert_checkpoint_contains(&post2, "beta.txt");
    // alpha.txt was created in the first cycle; it should NOT appear in the second
    // cycle since the second pre-snapshot includes it.
    assert_checkpoint_excludes(&post2, "alpha.txt");
}

// ===========================================================================
// Category 11: Tar/archive operations
// ===========================================================================

#[test]
fn test_bash_provenance_create_tarball() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "archive/one.txt", "one", "add one");
    add_and_commit(&repo, "archive/two.txt", "two", "add two");

    pre_hook(&root, "tar-create-sess", "tar-create-t1");

    run_bash(&repo, "tar", &["czf", "archive.tar.gz", "archive"]);

    let post_action = post_hook(&root, "tar-create-sess", "tar-create-t1");
    assert_checkpoint_contains(&post_action, "archive.tar.gz");
}

#[test]
fn test_bash_provenance_extract_tarball() {
    use git_ai::operations::commands::checkpoint_agent::bash_tool::diff;
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "pkg/alpha.txt", "alpha", "add alpha");
    add_and_commit(&repo, "pkg/beta.txt", "beta", "add beta");

    // Create the tarball first
    run_bash(&repo, "tar", &["czf", "pkg.tar.gz", "pkg"]);
    repo.git_og(&["add", "pkg.tar.gz"])
        .expect("git add tarball should succeed");
    repo.git_og(&["commit", "-m", "add tarball"])
        .expect("git commit tarball should succeed");

    // Remove original directory
    run_bash(&repo, "rm", &["-rf", "pkg"]);
    repo.git_og(&["add", "-A"])
        .expect("git add removal should succeed");
    repo.git_og(&["commit", "-m", "remove pkg dir"])
        .expect("git commit removal should succeed");

    let pre = snapshot(&root, "tar-extract-sess", "tar-extract-t1", None).unwrap();

    run_bash(&repo, "tar", &["xzf", "pkg.tar.gz"]);

    let post = snapshot(&root, "tar-extract-sess", "tar-extract-t2", None).unwrap();
    let result = diff(&pre, &post);
    assert!(
        result
            .created
            .iter()
            .any(|p| p.display().to_string().contains("alpha.txt")),
        "alpha.txt should appear as created after tarball extract; got created={:?}",
        result.created,
    );
    assert!(
        result
            .created
            .iter()
            .any(|p| p.display().to_string().contains("beta.txt")),
        "beta.txt should appear as created after tarball extract; got created={:?}",
        result.created,
    );
}

// ===========================================================================
// Category 12: Compiler/tool output simulation
// ===========================================================================

#[test]
fn test_bash_provenance_simulated_compile() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(
        &repo,
        "hello.c",
        "#include <stdio.h>\nint main() { printf(\"hello\\n\"); return 0; }\n",
        "initial commit",
    );

    pre_hook(&root, "compile-sess", "compile-t1");

    // Simulate compilation by creating an output binary
    run_bash(&repo, "sh", &["-c", "echo 'compiled binary' > hello"]);

    let post_action = post_hook(&root, "compile-sess", "compile-t1");
    assert_checkpoint_contains(&post_action, "hello");
}

// ===========================================================================
// Additional: Direct snapshot/diff API tests with real commands
// ===========================================================================

#[test]
fn test_bash_provenance_snapshot_diff_echo_redirect() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    let pre = snapshot(&root, "snap-echo", "t1", None).expect("pre-snapshot should succeed");

    run_bash(&repo, "sh", &["-c", "echo 'snap test' > snap_created.txt"]);

    let post = snapshot(&root, "snap-echo", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    let created: Vec<String> = result
        .created
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        created.iter().any(|p| p.contains("snap_created.txt")),
        "snap_created.txt should appear in created via direct snapshot/diff; got {:?}",
        created
    );
    assert!(
        result.modified.is_empty(),
        "no files should be modified; got {:?}",
        result.modified
    );
}

#[test]
fn test_bash_provenance_snapshot_diff_sed_modification() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "editable.txt", "old text old text", "initial commit");

    let pre = snapshot(&root, "snap-sed", "t1", None).expect("pre-snapshot should succeed");

    thread::sleep(Duration::from_millis(50));
    run_bash(
        &repo,
        "sh",
        &[
            "-c",
            "sed -i.bak 's/old/new/g' editable.txt && rm -f editable.txt.bak",
        ],
    );

    let post = snapshot(&root, "snap-sed", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    let modified: Vec<String> = result
        .modified
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        modified.iter().any(|p| p.contains("editable.txt")),
        "editable.txt should appear in modified via direct snapshot/diff; got {:?}",
        modified
    );
}

// ───────────────────────────────────────────────────────────────────
// 13. git_status_fallback parsing correctness
// ───────────────────────────────────────────────────────────────────

#[test]
fn test_git_status_fallback_files_with_spaces() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    // Create and track a file with spaces in its name
    add_and_commit(&repo, "file with spaces.txt", "original", "add spaced file");

    // Modify it so git status reports it
    repo.write_file("file with spaces.txt", "modified");

    let changed = git_status_fallback(&root).unwrap();
    assert!(
        changed.iter().any(|p| p == "file with spaces.txt"),
        "git_status_fallback should return full path with spaces; got {:?}",
        changed
    );
}

#[test]
fn test_git_status_fallback_new_untracked_with_spaces() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    // Create an untracked file with spaces
    repo.write_file("my new file.rs", "content");

    let changed = git_status_fallback(&root).unwrap();
    assert!(
        changed.iter().any(|p| p == "my new file.rs"),
        "git_status_fallback should return full untracked path with spaces; got {:?}",
        changed
    );
}

#[test]
fn test_git_status_fallback_rename_reports_both_paths() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    // Create and track a file, then rename it (staged rename)
    add_and_commit(&repo, "before.txt", "content", "add file");
    std::fs::rename(root.join("before.txt"), root.join("after.txt")).unwrap();
    repo.git_og(&["add", "-A"]).expect("git add should succeed");

    let changed = git_status_fallback(&root).unwrap();
    assert!(
        changed.iter().any(|p| p == "after.txt"),
        "git_status_fallback should report new rename path; got {:?}",
        changed
    );
    assert!(
        changed.iter().any(|p| p == "before.txt"),
        "git_status_fallback should report original rename path for attribution preservation; got {:?}",
        changed
    );
}

#[test]
fn test_bash_provenance_mv_directory_rename() {
    use git_ai::operations::commands::checkpoint_agent::bash_tool::diff;
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    // Create files in a subdirectory and track them
    add_and_commit(&repo, "src/lib.rs", "fn main() {}", "add src");
    add_and_commit(&repo, "src/utils.rs", "fn helper() {}", "add utils");

    let pre = snapshot(&root, "mvdir-sess", "mvdir-t1", None).unwrap();

    std::fs::rename(root.join("src"), root.join("lib")).unwrap();

    let post = snapshot(&root, "mvdir-sess", "mvdir-t2", None).unwrap();
    let result = diff(&pre, &post);
    assert!(
        result.created.iter().any(|p| p
            .to_string_lossy()
            .replace('\\', "/")
            .contains("lib/lib.rs")),
        "lib/lib.rs should appear as created after directory rename; got created={:?}",
        result.created,
    );
}

// ===========================================================================
// Category 14: Pre-commit hook formatter attribution
//
// Verifies that when a git commit runs inside an AI agent's bash tool call,
// and git's pre-commit hook runs a formatter (or any tool that modifies files),
// those changes are properly detected by the stat-diff mechanism and attributed
// to the AI agent.
// ===========================================================================

/// Install a git pre-commit hook script in the test repo.
/// The hook must be executable and located at `.git/hooks/pre-commit`.
#[cfg(unix)]
fn install_pre_commit_hook(repo: &TestRepo, script: &str) {
    let git_dir = repo.path().join(".git");
    // For linked worktrees, .git is a file pointing to the real git dir
    let hooks_dir = if git_dir.is_file() {
        let content = fs::read_to_string(&git_dir).expect("read .git file");
        let real_git_dir = content
            .trim()
            .strip_prefix("gitdir: ")
            .expect("parse gitdir");
        std::path::PathBuf::from(real_git_dir).join("hooks")
    } else {
        git_dir.join("hooks")
    };
    fs::create_dir_all(&hooks_dir).expect("create hooks dir");
    let hook_path = hooks_dir.join("pre-commit");
    write_executable_script(&hook_path, script).expect("write pre-commit hook");
}

/// Run a raw git command in the repo (without bypassing hooks).
/// Unlike `run_bash`, this returns the full output including exit status
/// without asserting success, so we can check for hook failures.
#[cfg(unix)]
fn run_git_with_hooks(repo: &TestRepo, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {:?} failed to start: {}", args, e))
}

#[cfg(unix)]
#[test]
fn test_bash_provenance_precommit_hook_formatter_modifies_staged_file() {
    // Scenario: AI agent creates a file, commits it, and the pre-commit hook
    // reformats the file (modifies it without re-staging). The stat-diff should
    // detect the formatter's modification.
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    // Install a pre-commit hook that appends a formatter comment to .py files.
    // It modifies the working tree file but does NOT re-stage, so the commit
    // contains the original content and the working tree has the formatted version.
    install_pre_commit_hook(
        &repo,
        r#"#!/bin/sh
for f in $(git diff --cached --name-only --diff-filter=ACM -- '*.py'); do
    echo '# auto-formatted' >> "$f"
done
exit 0
"#,
    );

    // Pre-snapshot: AI agent's bash tool starts
    pre_hook(&root, "fmt-sess", "fmt-t1");

    // AI agent creates and stages a Python file
    repo.write_file("main.py", "print('hello')\n");
    run_git_with_hooks(&repo, &["add", "main.py"]);

    // AI agent commits — the pre-commit hook will modify main.py
    let commit_output = run_git_with_hooks(&repo, &["commit", "-m", "add main.py"]);
    assert!(
        commit_output.status.success(),
        "git commit should succeed: {}",
        String::from_utf8_lossy(&commit_output.stderr)
    );

    // Verify the formatter actually ran
    let content = fs::read_to_string(repo.path().join("main.py")).unwrap();
    assert!(
        content.contains("# auto-formatted"),
        "pre-commit hook should have appended formatter comment; got: {:?}",
        content
    );

    // Post-snapshot: stat-diff should detect the formatter's modification
    let post_action = post_hook(&root, "fmt-sess", "fmt-t1");
    assert_checkpoint_contains(&post_action, "main.py");
}

#[cfg(unix)]
#[test]
fn test_bash_provenance_precommit_hook_formatter_restages_file() {
    // Scenario: pre-commit hook formats AND re-stages the file (common pattern
    // with tools like prettier --write + git add). The commit contains the
    // formatted content. The stat-diff should still detect the file change.
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    install_pre_commit_hook(
        &repo,
        r#"#!/bin/sh
for f in $(git diff --cached --name-only --diff-filter=ACM -- '*.py'); do
    # Simulate a formatter that normalizes whitespace
    sed -i.bak 's/[[:space:]]*$//' "$f" && rm -f "$f.bak"
    echo '# formatted-and-staged' >> "$f"
    git add "$f"
done
exit 0
"#,
    );

    pre_hook(&root, "fmtstage-sess", "fmtstage-t1");

    repo.write_file("app.py", "x = 1   \ny = 2   \n");
    run_git_with_hooks(&repo, &["add", "app.py"]);

    let commit_output = run_git_with_hooks(&repo, &["commit", "-m", "add app.py"]);
    assert!(
        commit_output.status.success(),
        "git commit should succeed: {}",
        String::from_utf8_lossy(&commit_output.stderr)
    );

    let content = fs::read_to_string(repo.path().join("app.py")).unwrap();
    assert!(
        content.contains("# formatted-and-staged"),
        "formatter should have modified the file; got: {:?}",
        content
    );

    let post_action = post_hook(&root, "fmtstage-sess", "fmtstage-t1");
    assert_checkpoint_contains(&post_action, "app.py");
}

#[cfg(unix)]
#[test]
fn test_bash_provenance_precommit_hook_creates_new_file() {
    // Scenario: pre-commit hook creates a new file (e.g., a lint report or
    // generated manifest). The stat-diff should detect the new file.
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    install_pre_commit_hook(
        &repo,
        r#"#!/bin/sh
# Generate a timestamp file on every commit
echo "last-commit: $(date -u +%Y-%m-%dT%H:%M:%S)" > .commit-metadata
exit 0
"#,
    );

    pre_hook(&root, "hooknew-sess", "hooknew-t1");

    repo.write_file("feature.py", "def feature(): pass\n");
    run_git_with_hooks(&repo, &["add", "feature.py"]);

    let commit_output = run_git_with_hooks(&repo, &["commit", "-m", "add feature"]);
    assert!(
        commit_output.status.success(),
        "git commit should succeed: {}",
        String::from_utf8_lossy(&commit_output.stderr)
    );

    // Verify the hook created the metadata file
    assert!(
        repo.path().join(".commit-metadata").exists(),
        "pre-commit hook should have created .commit-metadata"
    );

    let post_action = post_hook(&root, "hooknew-sess", "hooknew-t1");

    // Both the agent's file and the hook-created file should be detected
    assert_checkpoint_contains(&post_action, "feature.py");
    assert_checkpoint_contains(&post_action, ".commit-metadata");
}

#[cfg(unix)]
#[test]
fn test_bash_provenance_precommit_hook_modifies_untouched_file() {
    // Scenario: pre-commit hook modifies a file that the AI agent did NOT touch.
    // For example, a hook that updates a version timestamp in a config file
    // whenever any commit is made. The stat-diff should detect this change.
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");
    add_and_commit(&repo, "build-stamp.txt", "build: 0\n", "add build stamp");

    install_pre_commit_hook(
        &repo,
        r#"#!/bin/sh
# Increment build number on every commit (modifies a file the agent didn't touch)
current=$(grep -o '[0-9]*' build-stamp.txt)
next=$((current + 1))
echo "build: $next" > build-stamp.txt
exit 0
"#,
    );

    pre_hook(&root, "hookother-sess", "hookother-t1");

    // AI agent creates a completely different file
    thread::sleep(Duration::from_millis(50));
    repo.write_file("new-feature.rs", "fn new_feature() {}\n");
    run_git_with_hooks(&repo, &["add", "new-feature.rs"]);

    let commit_output = run_git_with_hooks(&repo, &["commit", "-m", "add new feature"]);
    assert!(
        commit_output.status.success(),
        "git commit should succeed: {}",
        String::from_utf8_lossy(&commit_output.stderr)
    );

    // Verify the hook modified build-stamp.txt
    let content = fs::read_to_string(repo.path().join("build-stamp.txt")).unwrap();
    assert!(
        content.contains("build: 1"),
        "hook should have incremented build number; got: {:?}",
        content
    );

    let post_action = post_hook(&root, "hookother-sess", "hookother-t1");

    // Both files should be detected: the agent's new file and the hook-modified file
    assert_checkpoint_contains(&post_action, "new-feature.rs");
    assert_checkpoint_contains(&post_action, "build-stamp.txt");
}

// test_bash_provenance_precommit_hook_with_agent_context_attribution was removed:
// checkpoint_context_from_active_bash has been deleted from the codebase.

#[cfg(unix)]
#[test]
fn test_bash_provenance_precommit_hook_modifies_multiple_files() {
    // Scenario: pre-commit hook runs a formatter on multiple staged files.
    // All modified files should be detected by the stat-diff.
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    install_pre_commit_hook(
        &repo,
        r#"#!/bin/sh
for f in $(git diff --cached --name-only --diff-filter=ACM -- '*.py'); do
    echo '# lint-pass' >> "$f"
done
exit 0
"#,
    );

    pre_hook(&root, "multi-fmt-sess", "multi-fmt-t1");

    // AI agent creates multiple Python files
    repo.write_file("src/api.py", "def api(): pass\n");
    repo.write_file("src/models.py", "class Model: pass\n");
    repo.write_file("src/utils.py", "def util(): pass\n");
    repo.write_file("readme.md", "# Project\n"); // Not .py — won't be formatted

    run_git_with_hooks(&repo, &["add", "."]);

    let commit_output = run_git_with_hooks(&repo, &["commit", "-m", "add src"]);
    assert!(
        commit_output.status.success(),
        "git commit should succeed: {}",
        String::from_utf8_lossy(&commit_output.stderr)
    );

    // Verify all .py files were formatted
    for py_file in &["src/api.py", "src/models.py", "src/utils.py"] {
        let content = fs::read_to_string(repo.path().join(py_file)).unwrap();
        assert!(
            content.contains("# lint-pass"),
            "{} should have been formatted; got: {:?}",
            py_file,
            content
        );
    }
    // readme.md should NOT have been formatted
    let readme = fs::read_to_string(repo.path().join("readme.md")).unwrap();
    assert!(
        !readme.contains("# lint-pass"),
        "readme.md should not have been formatted"
    );

    let post_action = post_hook(&root, "multi-fmt-sess", "multi-fmt-t1");

    // All created/modified files should be detected
    assert_checkpoint_contains(&post_action, "api.py");
    assert_checkpoint_contains(&post_action, "models.py");
    assert_checkpoint_contains(&post_action, "utils.py");
    assert_checkpoint_contains(&post_action, "readme.md");
}

#[cfg(unix)]
#[test]
fn test_bash_provenance_precommit_hook_fails_and_modifies_files() {
    // Scenario: pre-commit hook modifies files but then exits with non-zero
    // (e.g., a linter that fixes formatting but reports errors). The commit
    // fails, but the stat-diff should still detect the modified files because
    // the working tree was changed.
    let repo = TestRepo::new();
    let root = repo_root(&repo);
    add_and_commit(&repo, "init.txt", "seed", "initial commit");

    install_pre_commit_hook(
        &repo,
        r#"#!/bin/sh
# Fix formatting but report failure (like a strict linter)
for f in $(git diff --cached --name-only --diff-filter=ACM -- '*.py'); do
    echo '# auto-fixed' >> "$f"
done
exit 1
"#,
    );

    pre_hook(&root, "hookfail-sess", "hookfail-t1");

    repo.write_file("broken.py", "x=1\n");
    run_git_with_hooks(&repo, &["add", "broken.py"]);

    // The commit will FAIL because the hook exits 1
    let commit_output = run_git_with_hooks(&repo, &["commit", "-m", "try commit"]);
    assert!(
        !commit_output.status.success(),
        "git commit should fail due to hook exit 1"
    );

    // But the hook still modified the file
    let content = fs::read_to_string(repo.path().join("broken.py")).unwrap();
    assert!(
        content.contains("# auto-fixed"),
        "hook should have modified the file even though it failed; got: {:?}",
        content
    );

    // The stat-diff should detect the modification
    let post_action = post_hook(&root, "hookfail-sess", "hookfail-t1");
    assert_checkpoint_contains(&post_action, "broken.py");
}
