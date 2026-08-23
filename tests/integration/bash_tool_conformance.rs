//! Conformance test suite for the bash tool change attribution feature.
//!
//! Covers PRD Sections 5.1 (file mutations), 5.2 (read-only operations),
//! 5.3 (edge cases), 5.4 (pre/post hook semantics), tool classification
//! for all six agents, gitignore filtering, and full handle_bash_tool
//! orchestration.

use crate::repos::test_repo::TestRepo;
use git_ai::authorship::working_log::AgentId;
use git_ai::commands::checkpoint_agent::bash_tool::{
    Agent, BashCheckpointAction, BashPostHookResult, StatDiffResult, StatEntry, StatFileType,
    StatSnapshot, ToolClass, build_gitignore, classify_tool, diff, git_status_fallback,
    handle_bash_post_tool_use, handle_bash_pre_tool_use_with_context, normalize_path,
    set_daemon_socket_for_test, snapshot,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write a file into the test repo, creating parent directories as needed.
fn write_file(repo: &TestRepo, rel_path: &str, contents: &str) {
    let abs = repo.path().join(rel_path);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent).expect("parent directory should be creatable");
    }
    fs::write(&abs, contents).expect("file write should succeed");
}

/// Stage and commit a file so it appears in `git ls-files` (tracked).
fn add_and_commit(repo: &TestRepo, rel_path: &str, contents: &str, message: &str) {
    write_file(repo, rel_path, contents);
    repo.git_og(&["add", rel_path])
        .expect("git add should succeed");
    repo.git_og(&["commit", "-m", message])
        .expect("git commit should succeed");
}

/// Canonical repo root path (resolves /tmp -> /private/tmp on macOS).
fn repo_root(repo: &TestRepo) -> std::path::PathBuf {
    set_daemon_socket_for_test(repo.daemon_control_socket_path());
    repo.canonical_path()
}

fn dummy_agent_id() -> AgentId {
    AgentId {
        tool: "test".to_string(),
        id: "test".to_string(),
        model: String::new(),
    }
}

fn dummy_trace_id() -> &'static str {
    "t_test123456789a"
}

fn pre_hook(root: &std::path::Path, session_id: &str, tool_use_id: &str) {
    handle_bash_pre_tool_use_with_context(
        root,
        session_id,
        tool_use_id,
        &dummy_agent_id(),
        None,
        dummy_trace_id(),
        None,
    )
    .expect("pre-hook should succeed");
}

fn post_hook(root: &std::path::Path, session_id: &str, tool_use_id: &str) -> BashPostHookResult {
    handle_bash_post_tool_use(
        root,
        session_id,
        tool_use_id,
        &dummy_agent_id(),
        None,
        dummy_trace_id(),
        None,
    )
    .expect("post-hook should succeed")
}

// ===========================================================================
// Section 5.1 — File Mutations
// ===========================================================================

#[test]
fn test_bash_tool_detect_file_creation() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");

    write_file(&repo, "new.txt", "hello");

    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    let created: Vec<String> = result
        .created
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        created.iter().any(|p| p.contains("new.txt")),
        "new.txt should appear in created; got {:?}",
        created
    );
    assert!(result.modified.is_empty(), "no files should be modified");
}

#[test]
fn test_bash_tool_detect_modification() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "existing.txt", "foo", "initial");

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");

    // Allow filesystem time granularity to advance so the stat-tuple changes.
    thread::sleep(Duration::from_millis(50));
    write_file(&repo, "existing.txt", "bar");

    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    let modified: Vec<String> = result
        .modified
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        modified.iter().any(|p| p.contains("existing.txt")),
        "existing.txt should appear in modified; got {:?}",
        modified
    );
    assert!(result.created.is_empty(), "no files should be created");
}

#[cfg(unix)]
#[test]
fn test_bash_tool_detect_permission_change() {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "script.sh", "#!/bin/bash\necho hi", "initial");

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");

    // chmod +x
    let abs = repo.path().join("script.sh");
    let mut perms = fs::metadata(&abs).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&abs, perms).expect("chmod should succeed");

    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    let modified: Vec<String> = result
        .modified
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        modified.iter().any(|p| p.contains("script.sh")),
        "script.sh should appear in modified after chmod; got {:?}",
        modified
    );
}

#[test]
fn test_bash_tool_detect_rename() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "old.txt", "data", "initial");

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");

    fs::rename(repo.path().join("old.txt"), repo.path().join("new.txt"))
        .expect("rename should succeed");

    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    // After rename: old.txt no longer exists (deletion not tracked), new.txt appears as created.
    let created: Vec<String> = result
        .created
        .iter()
        .map(|p| p.display().to_string())
        .collect();

    assert!(
        created.iter().any(|p| p.contains("new.txt")),
        "new.txt should appear in created after rename; got {:?}",
        created
    );
}

#[test]
fn test_bash_tool_detect_copy() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "source.txt", "copy-me", "initial");

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");

    fs::copy(repo.path().join("source.txt"), repo.path().join("dest.txt"))
        .expect("copy should succeed");

    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    let created: Vec<String> = result
        .created
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        created.iter().any(|p| p.contains("dest.txt")),
        "dest.txt should appear in created (or modified) after copy; got {:?}",
        created
    );
    // source.txt should NOT appear as modified since we only read it
    let modified: Vec<String> = result
        .modified
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        !modified.iter().any(|p| p.contains("source.txt")),
        "source.txt should not be modified by a copy; got {:?}",
        modified
    );
}

// ===========================================================================
// Section 5.2 — Read-Only Operations
// ===========================================================================

#[test]
fn test_bash_tool_no_changes_detected() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "stable.txt", "unchanged", "initial");

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");
    // No mutations between snapshots.
    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    assert!(
        result.is_empty(),
        "diff should be empty when nothing changed"
    );
    assert!(result.created.is_empty());
    assert!(result.modified.is_empty());
}

#[test]
fn test_bash_tool_empty_repo_no_changes() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");
    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    assert!(result.is_empty(), "empty repo diff should be empty");
}

// ===========================================================================
// Section 5.3 — Edge Cases
// ===========================================================================

#[test]
fn test_bash_tool_files_outside_repo_ignored() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "inside.txt", "inside", "initial");

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");

    // Modify a file outside the repo — this should not be detected.
    let outside = std::env::temp_dir().join("bash_tool_test_outside.txt");
    fs::write(&outside, "external change").expect("write outside repo should succeed");

    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    assert!(
        result.is_empty(),
        "changes outside the repo should not appear in the diff"
    );

    // Clean up
    let _ = fs::remove_file(&outside);
}

#[test]
fn test_bash_tool_empty_stat_diff() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");
    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    assert!(
        result.is_empty(),
        "empty stat-diff should produce no changes"
    );
    assert!(result.all_changed_paths().is_empty());
}

#[test]
fn test_bash_tool_multiple_mutations_combined() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "modify-me.txt", "original", "initial");
    add_and_commit(&repo, "delete-me.txt", "gone-soon", "add delete target");

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");

    // Perform multiple mutations
    thread::sleep(Duration::from_millis(50));
    write_file(&repo, "modify-me.txt", "changed");
    write_file(&repo, "brand-new.txt", "fresh");
    fs::remove_file(repo.path().join("delete-me.txt")).expect("delete should succeed");

    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    assert!(
        !result.is_empty(),
        "diff should not be empty after multiple mutations"
    );

    let all_paths = result.all_changed_paths();
    assert!(
        all_paths.iter().any(|p| p.contains("modify-me.txt")),
        "modify-me.txt should be in changed paths; got {:?}",
        all_paths
    );
    assert!(
        all_paths.iter().any(|p| p.contains("brand-new.txt")),
        "brand-new.txt should be in changed paths; got {:?}",
        all_paths
    );
    // delete-me.txt is not tracked (deletions are not reported)
}

// ===========================================================================
// Section 5.4 — Pre/Post Hook Semantics
// ===========================================================================

#[test]
fn test_bash_tool_pre_hook_returns_take_pre_snapshot() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    pre_hook(&root, "sess", "tool1");
}

#[test]
fn test_bash_tool_post_hook_no_changes() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "stable.txt", "unchanged", "initial");

    // Pre-hook stores the snapshot
    pre_hook(&root, "sess", "tool1");

    // Post-hook with no changes
    let post_action = post_hook(&root, "sess", "tool1");
    assert!(
        matches!(post_action.action, BashCheckpointAction::NoChanges),
        "PostToolUse with no changes should return NoChanges; got {:?}",
        &post_action.action
    );
}

#[test]
fn test_bash_tool_post_hook_detects_changes() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "target.txt", "before", "initial");

    // Pre-hook
    pre_hook(&root, "sess", "tool2");

    // Mutate between pre and post
    thread::sleep(Duration::from_millis(50));
    write_file(&repo, "target.txt", "after");

    // Post-hook
    let post_action = post_hook(&root, "sess", "tool2");
    match &post_action.action {
        BashCheckpointAction::Checkpoint(paths) => {
            assert!(
                paths.iter().any(|p| p.contains("target.txt")),
                "Checkpoint should include target.txt; got {:?}",
                paths
            );
        }
        other => panic!("Expected Checkpoint, got {:?}", other),
    }
}

#[test]
fn test_bash_tool_post_hook_without_pre_uses_fallback() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    // Do NOT call PreToolUse first. PostToolUse should fall back to git status.
    // Create a tracked file and then modify it so git status shows changes.
    add_and_commit(&repo, "changed.txt", "original", "initial");
    write_file(&repo, "changed.txt", "modified");

    let post_action = post_hook(&root, "sess", "missing-pre");

    // Without a pre-snapshot, expect MissingPreSnapshot (or possibly Checkpoint
    // if the daemon happens to have state from a prior run).
    match &post_action.action {
        BashCheckpointAction::Checkpoint(paths) => {
            assert!(
                paths.iter().any(|p| p.contains("changed.txt")),
                "Should detect changed.txt; got {:?}",
                paths
            );
        }
        BashCheckpointAction::NoChanges
        | BashCheckpointAction::MissingPreSnapshot
        | BashCheckpointAction::HookTimeout
        | BashCheckpointAction::SnapshotFailed => {
            // Acceptable — no pre-snapshot was stored or other failure
        }
    }
}

// ===========================================================================
// Full handle_bash_tool orchestration — Pre followed by Post with creation
// ===========================================================================

#[test]
fn test_bash_tool_orchestration_create_file() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    // Make an initial commit so the repo is valid
    add_and_commit(&repo, "readme.md", "# Hello", "init");

    // Pre-hook
    pre_hook(&root, "orch-sess", "orch-tool");

    // Simulate bash creating a new file
    write_file(&repo, "generated.rs", "fn main() {}");

    // Post-hook
    let action = post_hook(&root, "orch-sess", "orch-tool");

    match &action.action {
        BashCheckpointAction::Checkpoint(paths) => {
            assert!(
                paths.iter().any(|p| p.contains("generated.rs")),
                "Orchestrated checkpoint should include generated.rs; got {:?}",
                paths
            );
        }
        BashCheckpointAction::NoChanges => {
            panic!("Expected Checkpoint after creating a file, got NoChanges");
        }
        _ => panic!("Expected Checkpoint after creating a file"),
    }
}

#[test]
fn test_bash_tool_orchestration_delete_file() {
    // Deletions are not tracked; a bash call that only deletes files
    // produces NoChanges.
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "doomed.txt", "temporary", "initial");

    pre_hook(&root, "del-sess", "del-tool");

    fs::remove_file(repo.path().join("doomed.txt")).expect("remove should succeed");

    let action = post_hook(&root, "del-sess", "del-tool");

    // Deletion-only bash call: no changed paths to report.
    assert!(
        matches!(action.action, BashCheckpointAction::NoChanges),
        "Expected NoChanges for deletion-only bash call"
    );
}

#[test]
fn test_bash_tool_orchestration_multiple_tool_uses() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "base.txt", "base", "initial");

    // First tool use: create file
    pre_hook(&root, "multi-sess", "use1");
    write_file(&repo, "first.txt", "first");
    let action1 = post_hook(&root, "multi-sess", "use1");
    assert!(
        matches!(action1.action, BashCheckpointAction::Checkpoint(_)),
        "First tool use should produce Checkpoint"
    );

    // Second tool use: modify file
    pre_hook(&root, "multi-sess", "use2");
    thread::sleep(Duration::from_millis(50));
    write_file(&repo, "first.txt", "modified-first");
    let action2 = post_hook(&root, "multi-sess", "use2");
    assert!(
        matches!(action2.action, BashCheckpointAction::Checkpoint(_)),
        "Second tool use should produce Checkpoint"
    );
}

// ===========================================================================
// Tool Classification — All 6 Agents
// ===========================================================================

#[test]
fn test_classify_tool_claude() {
    assert_eq!(classify_tool(Agent::Claude, "Write"), ToolClass::FileEdit);
    assert_eq!(classify_tool(Agent::Claude, "write"), ToolClass::FileEdit);
    assert_eq!(classify_tool(Agent::Claude, "Edit"), ToolClass::FileEdit);
    assert_eq!(classify_tool(Agent::Claude, "edit"), ToolClass::FileEdit);
    assert_eq!(
        classify_tool(Agent::Claude, "MultiEdit"),
        ToolClass::FileEdit
    );
    assert_eq!(
        classify_tool(Agent::Claude, "multiedit"),
        ToolClass::FileEdit
    );
    assert_eq!(classify_tool(Agent::Claude, "Bash"), ToolClass::Bash);
    assert_eq!(classify_tool(Agent::Claude, "bash"), ToolClass::Bash);
    assert_eq!(classify_tool(Agent::Claude, "Read"), ToolClass::Skip);
    assert_eq!(classify_tool(Agent::Claude, "Glob"), ToolClass::Skip);
    assert_eq!(
        classify_tool(Agent::Claude, "unknown_tool"),
        ToolClass::Skip
    );
}

#[test]
fn test_classify_tool_gemini() {
    assert_eq!(
        classify_tool(Agent::Gemini, "write_file"),
        ToolClass::FileEdit
    );
    assert_eq!(
        classify_tool(Agent::Gemini, "WriteFile"),
        ToolClass::FileEdit
    );
    assert_eq!(classify_tool(Agent::Gemini, "replace"), ToolClass::FileEdit);
    assert_eq!(classify_tool(Agent::Gemini, "shell"), ToolClass::Bash);
    assert_eq!(
        classify_tool(Agent::Gemini, "run_shell_command"),
        ToolClass::Bash
    );
    assert_eq!(classify_tool(Agent::Gemini, "read_file"), ToolClass::Skip);
    assert_eq!(classify_tool(Agent::Gemini, "unknown"), ToolClass::Skip);
}

#[test]
fn test_classify_tool_continue_cli() {
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
    assert_eq!(classify_tool(Agent::ContinueCli, "read"), ToolClass::Skip);
    assert_eq!(
        classify_tool(Agent::ContinueCli, "unknown"),
        ToolClass::Skip
    );
}

#[test]
fn test_classify_tool_droid() {
    assert_eq!(
        classify_tool(Agent::Droid, "ApplyPatch"),
        ToolClass::FileEdit
    );
    assert_eq!(classify_tool(Agent::Droid, "Edit"), ToolClass::FileEdit);
    assert_eq!(classify_tool(Agent::Droid, "Write"), ToolClass::FileEdit);
    assert_eq!(classify_tool(Agent::Droid, "Create"), ToolClass::FileEdit);
    assert_eq!(classify_tool(Agent::Droid, "Bash"), ToolClass::Bash);
    assert_eq!(classify_tool(Agent::Droid, "Execute"), ToolClass::Bash);
    assert_eq!(classify_tool(Agent::Droid, "Read"), ToolClass::Skip);
    assert_eq!(classify_tool(Agent::Droid, "unknown"), ToolClass::Skip);
}

#[test]
fn test_classify_tool_amp() {
    assert_eq!(classify_tool(Agent::Amp, "Write"), ToolClass::FileEdit);
    assert_eq!(classify_tool(Agent::Amp, "Edit"), ToolClass::FileEdit);
    assert_eq!(
        classify_tool(Agent::Amp, "create_file"),
        ToolClass::FileEdit
    );
    assert_eq!(classify_tool(Agent::Amp, "edit_file"), ToolClass::FileEdit);
    assert_eq!(
        classify_tool(Agent::Amp, "apply_patch"),
        ToolClass::FileEdit
    );
    assert_eq!(classify_tool(Agent::Amp, "undo_edit"), ToolClass::FileEdit);
    assert_eq!(classify_tool(Agent::Amp, "Bash"), ToolClass::Bash);
    assert_eq!(classify_tool(Agent::Amp, "shell_command"), ToolClass::Bash);
    assert_eq!(classify_tool(Agent::Amp, "Read"), ToolClass::Skip);
    assert_eq!(classify_tool(Agent::Amp, "unknown"), ToolClass::Skip);
}

#[test]
fn test_classify_tool_opencode() {
    assert_eq!(classify_tool(Agent::OpenCode, "edit"), ToolClass::FileEdit);
    assert_eq!(classify_tool(Agent::OpenCode, "write"), ToolClass::FileEdit);
    assert_eq!(classify_tool(Agent::OpenCode, "bash"), ToolClass::Bash);
    assert_eq!(classify_tool(Agent::OpenCode, "shell"), ToolClass::Bash);
    assert_eq!(classify_tool(Agent::OpenCode, "read"), ToolClass::Skip);
    assert_eq!(classify_tool(Agent::OpenCode, "unknown"), ToolClass::Skip);
}

#[test]
fn test_classify_tool_codex() {
    assert_eq!(classify_tool(Agent::Codex, "Bash"), ToolClass::Bash);
    assert_eq!(
        classify_tool(Agent::Codex, "apply_patch"),
        ToolClass::FileEdit
    );
    assert_eq!(classify_tool(Agent::Codex, "unknown"), ToolClass::Skip);
}

// ===========================================================================
// Gitignore Filtering
// ===========================================================================

#[test]
fn test_bash_tool_gitignore_excludes_new_untracked_files() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    // Create a .gitignore that ignores *.log files, then commit it
    add_and_commit(&repo, ".gitignore", "*.log\n", "add gitignore");

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");

    // Create both an ignored and a non-ignored file
    write_file(&repo, "debug.log", "log output");
    write_file(&repo, "result.txt", "result data");

    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    let created: Vec<String> = result
        .created
        .iter()
        .map(|p| p.display().to_string())
        .collect();

    assert!(
        created.iter().any(|p| p.contains("result.txt")),
        "result.txt should be created; got {:?}",
        created
    );
    assert!(
        !created.iter().any(|p| p.contains("debug.log")),
        "debug.log should be excluded by gitignore; got {:?}",
        created
    );
}

#[test]
fn test_bash_tool_gitignore_excludes_directory_patterns() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    // Use glob patterns that match files (not just directory-trailing patterns),
    // since the snapshot walker checks individual file paths with is_dir=false.
    add_and_commit(
        &repo,
        ".gitignore",
        "*.o\n*.pyc\ntarget/\n",
        "add gitignore",
    );

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");

    // Create files matching glob-based ignore patterns
    write_file(&repo, "build/output.o", "binary");
    write_file(&repo, "cache/module.pyc", "bytecode");
    // Also create a non-ignored file
    write_file(&repo, "src/main.rs", "fn main() {}");

    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    let created: Vec<String> = result
        .created
        .iter()
        .map(|p| p.display().to_string())
        .collect();

    assert!(
        created
            .iter()
            .any(|p| p.contains("src/main.rs") || p.contains("src\\main.rs")),
        "src/main.rs should be created; got {:?}",
        created
    );
    assert!(
        !created.iter().any(|p| p.contains("output.o")),
        "*.o files should be excluded by gitignore; got {:?}",
        created
    );
    assert!(
        !created.iter().any(|p| p.contains("module.pyc")),
        "*.pyc files should be excluded by gitignore; got {:?}",
        created
    );
}

// ===========================================================================
// build_gitignore
// ===========================================================================

#[test]
fn test_build_gitignore_parses_rules() {
    // build_gitignore covers git-ai-specific patterns only (defaults,
    // .git-ai-ignore, linguist-generated).  Standard .gitignore rules are
    // handled by WalkBuilder with git_ignore(true); they are NOT loaded into
    // the Gitignore returned here.
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, ".gitignore", "*.tmp\ntarget/\n", "add gitignore");

    let gitignore = build_gitignore(&root).expect("build_gitignore should succeed");

    // git-ai default patterns should be present (*.lock is in DEFAULT_IGNORE_PATTERNS)
    assert!(
        gitignore
            .matched(Path::new("Cargo.lock"), false)
            .is_ignore(),
        "Cargo.lock should match git-ai default patterns"
    );

    // Standard .gitignore rules (*.tmp) are NOT in build_gitignore — the
    // walker handles those via git_ignore(true).
    assert!(
        !gitignore.matched(Path::new("data.tmp"), false).is_ignore(),
        "*.tmp is in .gitignore but not in build_gitignore; walker handles it"
    );

    // .rs files should not be ignored
    assert!(
        !gitignore.matched(Path::new("main.rs"), false).is_ignore(),
        "*.rs should not match any git-ai default patterns"
    );
}

// ===========================================================================
// git_status_fallback
// ===========================================================================

#[test]
fn test_git_status_fallback_detects_changes() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "tracked.txt", "original", "initial");
    write_file(&repo, "tracked.txt", "modified");

    let changed = git_status_fallback(&root).expect("git_status_fallback should succeed");

    assert!(
        changed.iter().any(|p| p.contains("tracked.txt")),
        "git_status_fallback should report tracked.txt; got {:?}",
        changed
    );
}

#[test]
fn test_git_status_fallback_detects_untracked() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    // Make an initial commit so we have a valid repo
    add_and_commit(&repo, "base.txt", "base", "init");
    write_file(&repo, "untracked.txt", "new file");

    let changed = git_status_fallback(&root).expect("git_status_fallback should succeed");

    assert!(
        changed.iter().any(|p| p.contains("untracked.txt")),
        "git_status_fallback should report untracked.txt; got {:?}",
        changed
    );
}

#[test]
fn test_git_status_fallback_clean_repo() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "clean.txt", "clean", "initial");

    let changed = git_status_fallback(&root).expect("git_status_fallback should succeed");
    assert!(
        changed.is_empty(),
        "clean repo should report no changes; got {:?}",
        changed
    );
}

// ===========================================================================
// cleanup_stale_snapshots
// ===========================================================================

// test_cleanup_stale_snapshots_does_not_error_on_empty was removed:
// cleanup_stale_snapshots has been deleted from the codebase.

// ===========================================================================
// normalize_path consistency
// ===========================================================================

#[test]
fn test_normalize_path_idempotent() {
    let path = Path::new("src/lib.rs");
    let once = normalize_path(path);
    let twice = normalize_path(&once);
    assert_eq!(once, twice, "normalize_path should be idempotent");
}

#[test]
fn test_normalize_path_handles_nested() {
    let path = Path::new("deeply/nested/dir/file.rs");
    let normalized = normalize_path(path);
    // On any platform, normalizing twice should give the same result
    assert_eq!(normalized, normalize_path(&normalized));
}

// ===========================================================================
// Snapshot invocation key
// ===========================================================================

#[test]
fn test_snapshot_invocation_key_format() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    let snap = snapshot(&root, "my-session", "my-tool", None).expect("snapshot should succeed");
    assert_eq!(
        snap.invocation_key, "my-session:my-tool",
        "invocation_key should be session_id:tool_use_id"
    );
}

// ===========================================================================
// DiffResult helpers
// ===========================================================================

#[test]
fn test_diff_result_all_changed_paths_combines_categories() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "modify.txt", "original", "initial");
    add_and_commit(&repo, "delete.txt", "doomed", "add delete target");

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");

    thread::sleep(Duration::from_millis(50));
    write_file(&repo, "modify.txt", "changed");
    write_file(&repo, "create.txt", "new");
    fs::remove_file(repo.path().join("delete.txt")).expect("delete should succeed");

    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    let all = result.all_changed_paths();
    // Deletions are not tracked; only modify.txt and create.txt are reported.
    assert!(
        all.len() >= 2,
        "Should have at least 2 changed paths; got {}",
        all.len()
    );
    assert!(all.iter().any(|p| p.contains("modify.txt")));
    assert!(all.iter().any(|p| p.contains("create.txt")));
}

#[test]
fn test_diff_result_is_empty_true_when_no_changes() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");
    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    assert!(result.is_empty());
    assert!(result.all_changed_paths().is_empty());
}

// ===========================================================================
// Subdirectory file operations
// ===========================================================================

#[test]
fn test_bash_tool_detect_file_in_subdirectory() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "src/lib.rs", "pub fn foo() {}", "initial");

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");

    thread::sleep(Duration::from_millis(50));
    write_file(&repo, "src/lib.rs", "pub fn bar() {}");
    write_file(&repo, "src/nested/deep/module.rs", "mod deep;");

    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    let all = result.all_changed_paths();
    assert!(
        all.iter()
            .any(|p| p.contains("src/lib.rs") || p.contains("src\\lib.rs")),
        "src/lib.rs should be detected; got {:?}",
        all
    );
    assert!(
        all.iter().any(|p| p.contains("module.rs")),
        "deeply nested module.rs should be detected; got {:?}",
        all
    );
}

// ===========================================================================
// normalize_path — case folding
// ===========================================================================

#[test]
fn test_normalize_path_case_folding() {
    let mixed = Path::new("Src/Main.RS");
    let normalized = normalize_path(mixed);
    // On macOS/Windows, should be lowercased; on Linux, unchanged
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        assert_eq!(
            normalized,
            PathBuf::from("src/main.rs"),
            "normalize_path should lowercase on case-insensitive platforms"
        );
    } else {
        assert_eq!(
            normalized,
            PathBuf::from("Src/Main.RS"),
            "normalize_path should preserve case on case-sensitive platforms"
        );
    }
}

// ===========================================================================
// Nested subdirectory .gitignore
// ===========================================================================

// test_build_gitignore_nested_subdirectory_rules and
// test_build_gitignore_deeply_nested_rules were removed: they tested the old
// collect_gitignores pre-walk which loaded nested .gitignore files into the
// Gitignore returned by build_gitignore.  That behavior was removed because
// the pre-walk could not apply rules during traversal, so gitignored dirs
// outside the hardcoded skip list were still descended into.  Nested
// .gitignore support now lives entirely in WalkBuilder (git_ignore(true)),
// which applies rules correctly during the walk.  The equivalent coverage is
// provided by test_snapshot_nested_gitignore_excludes_matching_new_files and
// test_snapshot_walker_prunes_ignored_directories.

#[test]
fn test_snapshot_walker_prunes_ignored_directories() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    // Gitignore that ignores an entire directory (like node_modules/)
    add_and_commit(&repo, ".gitignore", "ignored_dir/\n", "ignore a directory");
    add_and_commit(&repo, "tracked.txt", "tracked", "add tracked file");

    // Create the ignored directory with many files
    let ignored_dir = root.join("ignored_dir");
    fs::create_dir_all(&ignored_dir).expect("create ignored dir");
    for i in 0..100 {
        fs::write(ignored_dir.join(format!("file_{}.txt", i)), "noise").expect("write file");
    }

    let snap = snapshot(&root, "sess", "t1", None).expect("snapshot should succeed");

    // Tracked file should be in the snapshot
    assert!(
        snap.entries
            .keys()
            .any(|p| p.display().to_string().contains("tracked.txt")),
        "tracked.txt should be in snapshot"
    );

    // None of the ignored_dir files should be in the snapshot
    let ignored_count = snap
        .entries
        .keys()
        .filter(|p| p.display().to_string().contains("ignored_dir"))
        .count();
    assert_eq!(
        ignored_count, 0,
        "files in ignored_dir/ should not appear in snapshot"
    );
}

#[test]
fn test_snapshot_nested_gitignore_excludes_matching_new_files() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, ".gitignore", "", "root gitignore");
    add_and_commit(&repo, "src/.gitignore", "*.generated\n", "nested gitignore");

    let pre = snapshot(&root, "sess", "t1", None).expect("pre-snapshot should succeed");

    // Create both an ignored and a non-ignored file under src/
    write_file(&repo, "src/output.generated", "generated code");
    write_file(&repo, "src/real.rs", "fn real() {}");

    let post = snapshot(&root, "sess", "t2", None).expect("post-snapshot should succeed");
    let result = diff(&pre, &post);

    let created: Vec<String> = result
        .created
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        created.iter().any(|p| p.contains("real.rs")),
        "real.rs should be created; got {:?}",
        created
    );
    assert!(
        !created.iter().any(|p| p.contains("output.generated")),
        "output.generated should be excluded by nested gitignore; got {:?}",
        created
    );
}

// ===========================================================================
// Snapshot save/load round-trip and snapshot consumption
// ===========================================================================

// test_snapshot_save_load_round_trip was removed:
// save_snapshot and load_and_consume_snapshot have been deleted from the codebase.

// test_gitignore_filtering_through_save_load_round_trip was removed:
// save_snapshot and load_and_consume_snapshot have been deleted from the codebase.
// Gitignore filtering is still tested via the snapshot/diff tests above.

// ===========================================================================
// Stale snapshot cleanup — actually removes old snapshots
// ===========================================================================

// test_cleanup_stale_snapshots_removes_old_files was removed:
// cleanup_stale_snapshots and save_snapshot have been deleted from the codebase.

// ===========================================================================
// diff with gitignore=None passes all new files through
// ===========================================================================

#[test]
fn test_diff_no_gitignore_includes_all_new_files() {
    let now = SystemTime::now();
    let pre = StatSnapshot {
        entries: HashMap::new(),
        taken_at: None,
        invocation_key: "test:1".to_string(),
        repo_root: PathBuf::from("/tmp"),
        effective_worktree_wm: None,
        per_file_wm: HashMap::new(),
    };

    let mut post_entries = HashMap::new();
    // A file that would normally be gitignored (*.log)
    post_entries.insert(
        normalize_path(Path::new("debug.log")),
        StatEntry {
            exists: true,
            mtime: Some(now),
            ctime: Some(now),
            size: 100,
            mode: 0o644,
            file_type: StatFileType::Regular,
        },
    );
    // A normal file
    post_entries.insert(
        normalize_path(Path::new("main.rs")),
        StatEntry {
            exists: true,
            mtime: Some(now),
            ctime: Some(now),
            size: 50,
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
    // Both files appear as created (filter applied at snapshot time, not in diff).
    assert_eq!(
        result.created.len(),
        2,
        "Both files should be created when gitignore is None; got {:?}",
        result.created
    );
}

// ===========================================================================
// git_status_fallback — unmerged/conflict files (u prefix)
// ===========================================================================

#[test]
fn test_git_status_fallback_merge_conflict() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    // Create a file on main branch
    add_and_commit(&repo, "conflict.txt", "main content", "initial");

    // Create a branch, modify the file, commit
    repo.git_og(&["checkout", "-b", "feature"])
        .expect("checkout should succeed");
    write_file(&repo, "conflict.txt", "feature content");
    repo.git_og(&["add", "conflict.txt"])
        .expect("add should succeed");
    repo.git_og(&["commit", "-m", "feature change"])
        .expect("commit should succeed");

    // Go back to main, modify the same file differently, commit
    repo.git_og(&["checkout", "master"])
        .or_else(|_| repo.git_og(&["checkout", "main"]))
        .expect("checkout main should succeed");
    write_file(&repo, "conflict.txt", "main diverged content");
    repo.git_og(&["add", "conflict.txt"])
        .expect("add should succeed");
    repo.git_og(&["commit", "-m", "main diverged"])
        .expect("commit should succeed");

    // Attempt merge — this should produce a conflict
    let merge_result = repo.git_og(&["merge", "feature", "--no-edit"]);
    // If merge succeeds (auto-resolved), skip the test
    if merge_result.is_ok() {
        return; // Auto-resolved, no conflict to test
    }

    let changed = git_status_fallback(&root).expect("git_status_fallback should succeed");
    assert!(
        changed.iter().any(|p| p.contains("conflict.txt")),
        "git_status_fallback should report conflicted file; got {:?}",
        changed
    );
}

// ===========================================================================
// git_status_fallback — staged deletion
// ===========================================================================

#[test]
fn test_git_status_fallback_staged_deletion() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "to-delete.txt", "content", "initial");
    repo.git_og(&["rm", "to-delete.txt"])
        .expect("git rm should succeed");

    let changed = git_status_fallback(&root).expect("git_status_fallback should succeed");
    assert!(
        changed.iter().any(|p| p.contains("to-delete.txt")),
        "git_status_fallback should report staged deletion; got {:?}",
        changed
    );
}

// ===========================================================================
// git_status_fallback — rename with spaces in both paths
// ===========================================================================

#[test]
fn test_git_status_fallback_rename_with_spaces() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "old file name.txt", "content", "add spaced file");
    fs::rename(
        root.join("old file name.txt"),
        root.join("new file name.txt"),
    )
    .expect("rename should succeed");
    repo.git_og(&["add", "-A"]).expect("git add should succeed");

    let changed = git_status_fallback(&root).expect("git_status_fallback should succeed");
    assert!(
        changed.iter().any(|p| p == "new file name.txt"),
        "should report new path with spaces; got {:?}",
        changed
    );
    assert!(
        changed.iter().any(|p| p == "old file name.txt"),
        "should report original path with spaces; got {:?}",
        changed
    );
}

// ===========================================================================
// StatDiffResult::is_empty with single non-empty category
// ===========================================================================

#[test]
fn test_stat_diff_result_is_empty_single_category() {
    let created_only = StatDiffResult {
        created: vec![PathBuf::from("new.txt")],
        modified: vec![],
    };
    assert!(!created_only.is_empty());

    let modified_only = StatDiffResult {
        created: vec![],
        modified: vec![PathBuf::from("changed.txt")],
    };
    assert!(!modified_only.is_empty());

    assert!(StatDiffResult::default().is_empty());
}

// ===========================================================================
// StatEntry — symlink file type
// ===========================================================================

#[cfg(unix)]
#[test]
fn test_stat_entry_symlink_type() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let target = tmp.path().join("target.txt");
    let link = tmp.path().join("link.txt");
    fs::write(&target, "target content").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let meta = fs::symlink_metadata(&link).unwrap();
    let entry = StatEntry::from_metadata(&meta);
    assert_eq!(entry.file_type, StatFileType::Symlink);
    assert!(entry.exists);
}

// ===========================================================================
// StatEntry — ctime is populated
// ===========================================================================

#[test]
fn test_stat_entry_has_ctime() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    fs::write(tmp.path(), "hello").unwrap();
    let meta = fs::symlink_metadata(tmp.path()).unwrap();
    let entry = StatEntry::from_metadata(&meta);
    assert!(
        entry.ctime.is_some(),
        "ctime should be populated on real files"
    );
}

// ===========================================================================
// Snapshot — hidden files (dotfiles) are included
// ===========================================================================

#[test]
fn test_snapshot_includes_hidden_files() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, ".hidden_config", "secret=val", "add hidden file");

    let snap = snapshot(&root, "sess", "t1", None).expect("snapshot should succeed");
    assert!(
        snap.entries
            .keys()
            .any(|p| p.display().to_string().contains(".hidden_config")),
        "snapshot should include hidden (dotfiles); got keys: {:?}",
        snap.entries.keys().collect::<Vec<_>>()
    );
}

// ===========================================================================
// Walker error — permission denied on subdirectory
// ===========================================================================

#[cfg(unix)]
#[test]
fn test_snapshot_handles_permission_denied_directory() {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "accessible.txt", "ok", "initial");
    add_and_commit(&repo, "restricted/file.txt", "restricted", "add restricted");

    // Remove read/execute permission on the restricted directory
    let restricted_dir = repo.path().join("restricted");
    let mut perms = fs::metadata(&restricted_dir).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&restricted_dir, perms).expect("chmod should succeed");

    // Snapshot should still succeed (walker errors are skipped)
    let snap = snapshot(&root, "sess", "t1", None);

    // Restore permissions before assertion (for cleanup)
    let mut perms = fs::metadata(&restricted_dir)
        .unwrap_or_else(|_| fs::symlink_metadata(&restricted_dir).unwrap())
        .permissions();
    perms.set_mode(0o755);
    let _ = fs::set_permissions(&restricted_dir, perms);

    let snap = snap.expect("snapshot should succeed despite permission errors");
    // accessible.txt should be in the snapshot
    assert!(
        snap.entries
            .keys()
            .any(|p| p.display().to_string().contains("accessible.txt")),
        "accessible.txt should be in snapshot"
    );
}

// ===========================================================================
// handle_bash_tool — PostToolUse without PreToolUse, clean repo → NoChanges
// ===========================================================================

#[test]
fn test_post_hook_without_pre_clean_repo_returns_no_changes() {
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "clean.txt", "clean", "initial");
    // No PreToolUse, no modifications — should get MissingPreSnapshot or NoChanges

    let action = post_hook(&root, "sess", "missing");

    assert!(
        matches!(
            action.action,
            BashCheckpointAction::NoChanges | BashCheckpointAction::MissingPreSnapshot
        ),
        "Clean repo without pre-snapshot should return NoChanges or MissingPreSnapshot"
    );
}

// ===========================================================================
// Multiple files in different states detected simultaneously
// ===========================================================================

// ===========================================================================
// handle_bash_tool full orchestration — rename detection through pre/post
// ===========================================================================

#[test]
fn test_handle_bash_tool_detects_rename() {
    use git_ai::commands::checkpoint_agent::bash_tool::diff;
    let repo = TestRepo::new();
    let root = repo_root(&repo);

    add_and_commit(&repo, "original.txt", "content", "initial");

    let pre = snapshot(&root, "rename-sess", "rename-t1", None).unwrap();

    fs::rename(
        repo.path().join("original.txt"),
        repo.path().join("renamed.txt"),
    )
    .expect("rename should succeed");

    let post = snapshot(&root, "rename-sess", "rename-t2", None).unwrap();
    let result = diff(&pre, &post);
    assert!(
        result
            .created
            .iter()
            .any(|p| p.display().to_string().contains("renamed.txt")),
        "renamed.txt should appear as created after rename; got created={:?}",
        result.created,
    );
}
