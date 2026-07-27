use crate::repos::test_repo::TestRepo;
use git_ai::authorship::stats::CommitStats;
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StatusOutput {
    stats: CommitStats,
    checkpoints: Vec<serde_json::Value>,
}

fn extract_json_object(output: &str) -> String {
    let start = output.find('{').unwrap_or(0);
    let end = output.rfind('}').unwrap_or(output.len().saturating_sub(1));
    output[start..=end].to_string()
}

fn status_json(repo: &TestRepo) -> StatusOutput {
    let raw = repo
        .git_ai(&["status", "--json"])
        .expect("git-ai status --json should succeed");
    let json = extract_json_object(&raw);
    serde_json::from_str(&json).expect("valid status json")
}

/// Mirror of `StatusOutput` where `checkpoints` is optional, so tests can
/// distinguish "field present" (default mode) from "field omitted" (--diff-only).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct DiffOnlyStatusOutput {
    stats: CommitStats,
    checkpoints: Option<Vec<serde_json::Value>>,
}

fn status_json_with_args(repo: &TestRepo, args: &[&str]) -> DiffOnlyStatusOutput {
    let raw = repo.git_ai(args).expect("git-ai status should succeed");
    let json = extract_json_object(&raw);
    serde_json::from_str(&json).expect("valid status json")
}

fn write_file(repo: &TestRepo, path: &str, contents: &str) {
    let abs_path = repo.path().join(path);
    if let Some(parent) = abs_path.parent() {
        fs::create_dir_all(parent).expect("parent directory should be creatable");
    }
    fs::write(abs_path, contents).expect("file write should succeed");
}

/// Migrated from src/commands/status.rs test_get_working_dir_diff_stats_post_filter_equivalence
///
/// Creates two files, commits them, then modifies both. A checkpoint is created
/// covering only a.txt. The status command should report the correct diff stats
/// for the checkpointed file (a.txt adds 2 lines).
#[test]
fn test_working_dir_diff_stats_single_file_checkpoint() {
    let repo = TestRepo::new();

    write_file(&repo, "a.txt", "L1\nL2\nL3\n");
    write_file(&repo, "b.txt", "hello\n");
    repo.stage_all_and_commit("initial").unwrap();

    // Modify both in working dir
    write_file(&repo, "a.txt", "L1\nL2\nL3\nL4\nL5\n");
    write_file(&repo, "b.txt", "hello\nworld\n");

    // Checkpoint only a.txt -- the status command scopes its diff to checkpointed files
    repo.git_ai(&["checkpoint", "mock_ai", "a.txt"]).unwrap();

    let status = status_json(&repo);

    // a.txt adds 2 lines (L4, L5). b.txt is not in the checkpoint, so excluded.
    assert_eq!(
        status.stats.git_diff_added_lines, 2,
        "should count only a.txt additions (2 lines)"
    );
}

/// Migrated from src/commands/status.rs test_get_working_dir_diff_stats_post_filter_exclusion
///
/// Verifies that only checkpointed files are counted in status diff stats.
/// When only a.txt is checkpointed but both a.txt and b.txt are modified,
/// only a.txt's additions should appear.
#[test]
fn test_working_dir_diff_stats_exclusion_by_checkpoint() {
    let repo = TestRepo::new();

    write_file(&repo, "a.txt", "L1\nL2\nL3\n");
    write_file(&repo, "b.txt", "hello\n");
    repo.stage_all_and_commit("initial").unwrap();

    // Modify both in working dir
    write_file(&repo, "a.txt", "L1\nL2\nL3\nL4\nL5\n");
    write_file(&repo, "b.txt", "hello\nworld\n");

    // Checkpoint only a.txt
    repo.git_ai(&["checkpoint", "mock_ai", "a.txt"]).unwrap();

    let status = status_json(&repo);

    // a.txt adds 2 lines; b.txt adds 1 line but should be excluded
    assert_eq!(
        status.stats.git_diff_added_lines, 2,
        "should only count a.txt additions, not b.txt"
    );
}

/// Migrated from src/commands/status.rs test_get_working_dir_diff_stats_none_pathspecs
///
/// When all modified files are checkpointed (equivalent to None pathspecs in the
/// original test), the status should count additions from all files.
#[test]
fn test_working_dir_diff_stats_all_files_checkpointed() {
    let repo = TestRepo::new();

    write_file(&repo, "a.txt", "L1\nL2\nL3\n");
    write_file(&repo, "b.txt", "hello\n");
    repo.stage_all_and_commit("initial").unwrap();

    // Modify both in working dir
    write_file(&repo, "a.txt", "L1\nL2\nL3\nL4\nL5\n");
    write_file(&repo, "b.txt", "hello\nworld\n");

    // Checkpoint both files -- pathspecs include all modified files
    repo.git_ai(&["checkpoint", "mock_ai", "a.txt", "b.txt"])
        .unwrap();

    let status = status_json(&repo);

    // a.txt adds 2 lines + b.txt adds 1 line = 3 total
    assert_eq!(
        status.stats.git_diff_added_lines, 3,
        "all-files checkpoint should count all additions"
    );
}

/// Migrated from src/commands/status.rs test_get_working_dir_diff_stats_empty_pathspecs_returns_zero
///
/// When there are no working directory modifications, the status command should
/// report zero diff stats.
#[test]
fn test_working_dir_diff_stats_no_changes_returns_zero() {
    let repo = TestRepo::new();

    write_file(&repo, "a.txt", "L1\nL2\n");
    repo.stage_all_and_commit("initial").unwrap();

    // No modifications to the working directory after commit

    let status = status_json(&repo);

    // No changes means zero diff stats
    assert_eq!(status.stats.git_diff_added_lines, 0);
    assert_eq!(status.stats.git_diff_deleted_lines, 0);
}

/// Migrated from src/commands/status.rs test_get_working_dir_diff_stats_post_filter_with_rename
///
/// Verifies that renamed files are handled correctly in the status diff stats.
/// A file is renamed (staged) and another file is modified. Both are checkpointed.
/// With --no-renames (used internally by the diff stats function), old_name.txt
/// is reported as a delete and new_name.txt as a new file with all 4 lines added.
#[test]
fn test_working_dir_diff_stats_with_rename() {
    let repo = TestRepo::new();

    write_file(&repo, "old_name.txt", "L1\nL2\nL3\n");
    repo.stage_all_and_commit("initial").unwrap();

    // Rename old_name.txt -> new_name.txt and add a line.
    // Stage the rename so git diff HEAD sees it.
    fs::remove_file(repo.path().join("old_name.txt")).unwrap();
    write_file(&repo, "new_name.txt", "L1\nL2\nL3\nL4\n");
    // Stage everything so git diff HEAD picks up the rename
    repo.git(&["add", "-A"]).unwrap();

    // Checkpoint the new file name
    repo.git_ai(&["checkpoint", "mock_ai", "new_name.txt"])
        .unwrap();

    let status = status_json(&repo);

    // With --no-renames, new_name.txt is a new file with 4 added lines.
    // old_name.txt deletion is not in the pathspec set (only new_name.txt was
    // checkpointed and old_name.txt no longer exists), so deleted lines are 0.
    assert_eq!(
        status.stats.git_diff_added_lines, 4,
        "should count new_name.txt as 4 added lines (new file after --no-renames)"
    );
    assert_eq!(
        status.stats.git_diff_deleted_lines, 0,
        "old_name.txt deletion is filtered out because it is not in the checkpoint pathspecs"
    );
}

/// Migrated from src/commands/status.rs test_get_working_dir_diff_stats_respects_ignore_patterns
///
/// Verifies that default ignore patterns (which include *.lock) cause lock files
/// to be excluded from the status diff stats, even when they are checkpointed.
#[test]
fn test_working_dir_diff_stats_respects_ignore_patterns() {
    let repo = TestRepo::new();

    write_file(&repo, "src/lib.rs", "pub fn a() {}\n");
    write_file(&repo, "Cargo.lock", "# lock\n");
    repo.stage_all_and_commit("initial").unwrap();

    write_file(&repo, "src/lib.rs", "pub fn a() {}\npub fn b() {}\n");
    write_file(&repo, "Cargo.lock", "# lock\n# lock-2\n# lock-3\n");

    // Checkpoint both files -- Cargo.lock should be ignored by default patterns
    repo.git_ai(&["checkpoint", "mock_ai", "src/lib.rs", "Cargo.lock"])
        .unwrap();

    let status = status_json(&repo);

    // Only src/lib.rs adds 1 line; Cargo.lock additions (2 lines) should be excluded
    assert_eq!(
        status.stats.git_diff_added_lines, 1,
        "Cargo.lock additions should be ignored by default ignore patterns"
    );
}

/// Migrated from src/commands/status.rs test_count_ai_lines_from_initial_respects_ignore_patterns
///
/// Verifies that AI line counting (ai_accepted) excludes files matching default
/// ignore patterns. When Cargo.lock has AI-attributed lines, they should not be
/// counted in ai_accepted because *.lock is in the default ignore list.
#[test]
fn test_ai_accepted_respects_ignore_patterns() {
    let repo = TestRepo::new();

    write_file(&repo, "src/lib.rs", "pub fn a() {}\n");
    write_file(&repo, "Cargo.lock", "# lock\n");
    repo.stage_all_and_commit("initial").unwrap();

    // Modify both files
    write_file(&repo, "src/lib.rs", "pub fn a() {}\npub fn b() {}\n");
    write_file(&repo, "Cargo.lock", "# lock\n# lock-2\n# lock-3\n");

    // Checkpoint both as AI edits
    repo.git_ai(&["checkpoint", "mock_ai", "src/lib.rs", "Cargo.lock"])
        .unwrap();

    let status = status_json(&repo);

    // ai_accepted should only count src/lib.rs (1 line), not Cargo.lock
    // The exact ai_accepted value depends on the attribution pipeline, but
    // git_diff_added_lines should exclude Cargo.lock
    assert_eq!(
        status.stats.git_diff_added_lines, 1,
        "Cargo.lock should be excluded from diff stats by default ignore patterns"
    );
    // Verify Cargo.lock is not counted in AI stats either
    assert_eq!(
        status.stats.ai_accepted, 1,
        "AI accepted should only count src/lib.rs, ignoring Cargo.lock"
    );
}

#[test]
fn test_status_preserves_lowercase_agent_identifier() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("status-agent.txt");
    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    let repo_dir = repo.path().to_string_lossy().to_string();
    let pre_payload = serde_json::json!({
        "type": "human",
        "repo_working_dir": repo_dir,
        "will_edit_filepaths": ["status-agent.txt"]
    })
    .to_string();
    repo.git_ai(&["checkpoint", "agent-v1", "--hook-input", &pre_payload])
        .unwrap();

    fs::write(&file_path, "base\nAI line\n").unwrap();
    let post_payload = serde_json::json!({
        "type": "ai_agent",
        "repo_working_dir": repo_dir,
        "edited_filepaths": ["status-agent.txt"],
        "agent_name": "cline",
        "model": "test-model",
        "conversation_id": "cline-status-test"
    })
    .to_string();
    repo.git_ai(&["checkpoint", "agent-v1", "--hook-input", &post_payload])
        .unwrap();

    let status = status_json(&repo);
    assert!(
        status
            .checkpoints
            .iter()
            .any(|checkpoint| { checkpoint["tool_model"].as_str() == Some("cline test-model") })
    );
}

/// `--diff-only` must keep the same diff-scoped `stats` as a plain `--json`
/// run, while omitting the per-checkpoint breakdown.
#[test]
fn test_diff_only_omits_checkpoints_but_keeps_stats() {
    let repo = TestRepo::new();

    write_file(&repo, "a.txt", "L1\nL2\nL3\n");
    repo.stage_all_and_commit("initial").unwrap();

    write_file(&repo, "a.txt", "L1\nL2\nL3\nL4\nL5\n");
    repo.git_ai(&["checkpoint", "mock_ai", "a.txt"]).unwrap();

    // Default mode includes the checkpoints array.
    let default = status_json_with_args(&repo, &["status", "--json"]);
    assert!(
        default.checkpoints.is_some(),
        "default --json should include the checkpoints array"
    );
    assert!(
        !default.checkpoints.as_ref().unwrap().is_empty(),
        "default --json should list the recorded checkpoint"
    );

    // --diff-only omits the checkpoints field entirely.
    let diff_only = status_json_with_args(&repo, &["status", "--json", "--diff-only"]);
    assert!(
        diff_only.checkpoints.is_none(),
        "--diff-only should omit the checkpoints field"
    );

    // Diff-scoped stats are identical between the two modes.
    assert_eq!(
        diff_only.stats.git_diff_added_lines, default.stats.git_diff_added_lines,
        "--diff-only must not change the diff-scoped stats"
    );
    assert_eq!(
        diff_only.stats.git_diff_added_lines, 2,
        "a.txt adds 2 lines (L4, L5)"
    );
}

/// `--diff-only` also omits checkpoints in the no-changes empty state.
#[test]
fn test_diff_only_no_changes_omits_checkpoints() {
    let repo = TestRepo::new();

    write_file(&repo, "a.txt", "L1\nL2\n");
    repo.stage_all_and_commit("initial").unwrap();

    let diff_only = status_json_with_args(&repo, &["status", "--json", "--diff-only"]);
    assert!(
        diff_only.checkpoints.is_none(),
        "--diff-only should omit checkpoints even with no changes"
    );
    assert_eq!(diff_only.stats.git_diff_added_lines, 0);
    assert_eq!(diff_only.stats.git_diff_deleted_lines, 0);
}

crate::reuse_tests_in_worktree!(
    test_working_dir_diff_stats_single_file_checkpoint,
    test_working_dir_diff_stats_exclusion_by_checkpoint,
    test_working_dir_diff_stats_all_files_checkpointed,
    test_working_dir_diff_stats_no_changes_returns_zero,
    test_working_dir_diff_stats_with_rename,
    test_working_dir_diff_stats_respects_ignore_patterns,
    test_ai_accepted_respects_ignore_patterns,
    test_status_preserves_lowercase_agent_identifier,
    test_diff_only_omits_checkpoints_but_keeps_stats,
    test_diff_only_no_changes_omits_checkpoints,
);
