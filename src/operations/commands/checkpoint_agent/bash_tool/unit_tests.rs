//! Unit tests for bash-tool snapshot, diff, path filtering, and tool classification.

use super::path_filter::{build_gitignore, normalize_path, should_include_new_file};
use super::snapshot::{diff, git_status_fallback_args, system_time_to_nanos};
use super::tool_class::{Agent, classify_tool};
use super::types::{StatDiffResult, StatEntry, StatFileType, StatSnapshot};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

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
    assert_eq!(
        classify_tool(Agent::Claude, "Write"),
        super::types::ToolClass::FileEdit
    );
    assert_eq!(
        classify_tool(Agent::Claude, "Edit"),
        super::types::ToolClass::FileEdit
    );
    assert_eq!(
        classify_tool(Agent::Claude, "MultiEdit"),
        super::types::ToolClass::FileEdit
    );
    assert_eq!(
        classify_tool(Agent::Claude, "NotebookEdit"),
        super::types::ToolClass::FileEdit
    );
    assert_eq!(
        classify_tool(Agent::Claude, "Bash"),
        super::types::ToolClass::Bash
    );
    assert_eq!(
        classify_tool(Agent::Claude, "Read"),
        super::types::ToolClass::Skip
    );
    assert_eq!(
        classify_tool(Agent::Claude, "unknown"),
        super::types::ToolClass::Skip
    );
}

#[test]
fn test_tool_classification_all_agents() {
    use super::types::ToolClass;

    // Gemini
    assert_eq!(
        classify_tool(Agent::Gemini, "write_file"),
        ToolClass::FileEdit
    );
    assert_eq!(classify_tool(Agent::Gemini, "shell"), ToolClass::Bash);
    assert_eq!(
        classify_tool(Agent::Gemini, "WriteFile"),
        ToolClass::FileEdit
    );
    assert_eq!(
        classify_tool(Agent::Gemini, "run_shell_command"),
        ToolClass::Bash
    );

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
    assert_eq!(classify_tool(Agent::Droid, "Execute"), ToolClass::Bash);

    // Amp
    assert_eq!(classify_tool(Agent::Amp, "Write"), ToolClass::FileEdit);
    assert_eq!(classify_tool(Agent::Amp, "Bash"), ToolClass::Bash);
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
    assert_eq!(classify_tool(Agent::Amp, "shell_command"), ToolClass::Bash);

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
fn test_classify_optional_tool_preserves_legacy_payloads() {
    use super::tool_class::classify_optional_tool;
    use super::types::ToolClass;

    assert_eq!(
        classify_optional_tool(Agent::Claude, None),
        ToolClass::FileEdit
    );
    assert_eq!(
        classify_optional_tool(Agent::Claude, Some("Read")),
        ToolClass::Skip
    );
    assert_eq!(
        classify_optional_tool(Agent::Gemini, Some("run_shell_command")),
        ToolClass::Bash
    );
}

#[test]
fn test_tool_classification_codex_namespaced() {
    use super::types::ToolClass;

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
