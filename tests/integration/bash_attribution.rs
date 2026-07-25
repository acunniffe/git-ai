use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::{fixture_path, isolated_bash_history_db_path};
use git_ai::model::repository::bash_history_db::BashHistoryDatabase;
use git_ai::model::working_log::AgentId;
use git_ai::operations::commands::checkpoint_agent::bash_tool::{
    BashCheckpointAction, handle_bash_post_tool_use, handle_bash_pre_tool_use_with_context,
    reset_timeout_overrides_for_test, set_daemon_socket_for_test, set_walk_timeout_ms_for_test,
};
use serde_json::json;
use std::fs;
use std::thread;
use std::time::Duration;

#[test]
fn test_bash_pre_legacy_checkpoint_recovers_dirty_edge_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("example.txt");

    let initial = "original line\n";
    fs::write(&file_path, initial).unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut file = repo.filename("example.txt");
    file.assert_committed_lines(lines!["original line".unattributed_human()]);

    let after_dirty_edit = "original line\ndirty pre-bash line\n";
    fs::write(&file_path, after_dirty_edit).unwrap();
    repo.git_ai(&["checkpoint", "human", "example.txt"])
        .unwrap();

    let after_bash = "original line\ndirty pre-bash line\nai bash line\n";
    fs::write(&file_path, after_bash).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.txt"])
        .unwrap();

    repo.stage_all_and_commit("After bash").unwrap();
    file.assert_committed_lines(lines![
        "original line".unattributed_human(),
        "dirty pre-bash line".ai(),
        "ai bash line".ai(),
    ]);
}

#[test]
fn test_bash_clean_files_only_bash_changes_get_ai_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("clean.txt");

    let initial = "committed line\n";
    fs::write(&file_path, initial).unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut file = repo.filename("clean.txt");
    file.assert_committed_lines(lines!["committed line".unattributed_human()]);

    repo.git_ai(&["checkpoint", "human", "clean.txt"]).unwrap();

    let after_bash = "committed line\nbash added this\n";
    fs::write(&file_path, after_bash).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "clean.txt"])
        .unwrap();

    repo.stage_all_and_commit("After bash").unwrap();
    file.assert_committed_lines(lines![
        "committed line".unattributed_human(),
        "bash added this".ai(),
    ]);
}

#[test]
fn test_bash_multiple_files_mixed_dirty_state() {
    let repo = TestRepo::new();
    let a_path = repo.path().join("a.txt");
    let b_path = repo.path().join("b.txt");

    fs::write(&a_path, "line a\n").unwrap();
    fs::write(&b_path, "line b\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut file_a = repo.filename("a.txt");
    let mut file_b = repo.filename("b.txt");
    file_a.assert_committed_lines(lines!["line a".unattributed_human()]);
    file_b.assert_committed_lines(lines!["line b".unattributed_human()]);

    fs::write(&a_path, "line a\ndirty touched a\n").unwrap();

    repo.git_ai(&["checkpoint", "human", "a.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "human", "b.txt"]).unwrap();

    fs::write(&a_path, "line a\ndirty touched a\nbash touched a\n").unwrap();
    fs::write(&b_path, "line b\nbash touched b\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "a.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "b.txt"]).unwrap();

    repo.stage_all_and_commit("After bash").unwrap();
    file_a.assert_committed_lines(lines![
        "line a".unattributed_human(),
        "dirty touched a".ai(),
        "bash touched a".ai(),
    ]);
    file_b.assert_committed_lines(lines!["line b".unattributed_human(), "bash touched b".ai()]);
}

/// Orchestrator-level regression test: fires through the real codex
/// preset/orchestrator path (not manual `git-ai checkpoint human` CLI
/// calls). The bash history recovery pass intentionally minimizes untracked
/// lines, so pre-bash dirty content committed with the bash result is recovered
/// as AI when the bash invocation is the nearest candidate.
#[test]
fn test_codex_preset_bash_recovery_minimizes_dirty_untracked_attribution() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let repo = TestRepo::new_with_daemon_env(&env);
    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("example.txt");

    fs::write(&file_path, "original line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut file = repo.filename("example.txt");
    file.assert_committed_lines(lines!["original line".unattributed_human()]);

    // Dirty untracked content exists before the AI bash tool runs.
    fs::write(&file_path, "original line\ndirty pre-bash line\n").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-transcript.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "attr-pre-sess",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "attr-bash-1",
        "tool_input": { "command": "echo hello" },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.git_ai(&["checkpoint", "codex", "--hook-input", &pre_hook_input])
        .expect("codex pre-hook checkpoint should succeed");

    // AI bash tool edits the file.
    fs::write(
        &file_path,
        "original line\ndirty pre-bash line\nai bash line\n",
    )
    .unwrap();

    let post_hook_input = json!({
        "session_id": "attr-pre-sess",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "attr-bash-1",
        "tool_input": { "command": "echo hello" },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.git_ai(&["checkpoint", "codex", "--hook-input", &post_hook_input])
        .expect("codex post-hook checkpoint should succeed");

    repo.stage_all_and_commit("After codex bash").unwrap();
    file.assert_committed_lines(lines![
        "original line".unattributed_human(),
        "dirty pre-bash line".ai(),
        "ai bash line".ai(),
    ]);
}

#[test]
fn test_bash_checkpoints_v2_records_for_recovery_without_working_log_checkpoints() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let mut repo = TestRepo::new_with_daemon_env(&env);
    repo.patch_git_ai_config(|patch| {
        patch.feature_flags = Some(json!({"bash_checkpoints_v2": true}));
    });
    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("example.txt");

    fs::write(&file_path, "original line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut file = repo.filename("example.txt");
    file.assert_committed_lines(lines!["original line".unattributed_human()]);

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-transcript.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "bash-v2-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-v2-tool",
        "tool_input": { "command": "printf 'written by bash\\n' >> example.txt" },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.git_ai(&["checkpoint", "codex", "--hook-input", &pre_hook_input])
        .expect("codex pre-hook checkpoint should succeed");

    fs::write(&file_path, "original line\nwritten by bash\n").unwrap();

    let post_hook_input = json!({
        "session_id": "bash-v2-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-v2-tool",
        "tool_input": { "command": "printf 'written by bash\\n' >> example.txt" },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.git_ai(&["checkpoint", "codex", "--hook-input", &post_hook_input])
        .expect("codex post-hook checkpoint should succeed");

    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    assert!(
        checkpoints.is_empty(),
        "bash checkpoints v2 should only record recovery metadata, not normal checkpoints"
    );

    repo.stage_all_and_commit("After bash v2").unwrap();
    file.assert_committed_lines(lines![
        "original line".unattributed_human(),
        "written by bash".ai(),
    ]);

    let db = BashHistoryDatabase::open_at_path(std::path::Path::new(&bash_db_path)).unwrap();
    let calls = db.all_calls_for_test().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].session_id, "bash-v2-session");
    assert_eq!(calls[0].tool_use_id, "bash-v2-tool");
    assert_eq!(
        calls[0].repo_work_dir.as_deref(),
        Some(repo_root.to_string_lossy().as_ref())
    );
    assert!(calls[0].start_trace_id.is_some());
    assert!(calls[0].end_trace_id.is_some());
}

#[test]
fn test_bash_checkpoints_v2_denies_before_attempt_persistence() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let mut repo = TestRepo::new_with_daemon_env_and_patch(&env, |patch| {
        patch.feature_flags = Some(json!({
            "bash_checkpoints_v2": true,
            "checkpoint_debug_log": true
        }));
    });
    let malformed = repo.path().join("malformed");
    fs::create_dir_all(&malformed).unwrap();
    fs::write(malformed.join(".git"), "not a gitdir pointer\n").unwrap();
    let hook_input = json!({
        "session_id": "malformed-bash-session",
        "cwd": malformed.to_string_lossy(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "malformed-bash-tool",
        "tool_input": { "command": "printf sensitive >> private.txt" }
    })
    .to_string();

    let output = repo
        .git_ai(&["checkpoint", "codex", "--hook-input", &hook_input])
        .expect("an authorization denial should preserve the hook exit-zero contract");

    assert!(output.contains("repository authorization could not be verified"));
    let db = BashHistoryDatabase::open_at_path(std::path::Path::new(&bash_db_path)).unwrap();
    assert!(
        db.all_calls_for_test().unwrap().is_empty(),
        "a malformed-repository bash hook must not persist an attempt"
    );

    repo.patch_git_ai_config(|patch| {
        patch.allowed_repositories = Some(Vec::new());
    });
    let hook_input = json!({
        "session_id": "denied-bash-session",
        "cwd": repo.canonical_path().to_string_lossy(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "denied-bash-tool",
        "tool_input": { "command": "printf sensitive >> private.txt" }
    })
    .to_string();
    let output = repo
        .git_ai(&["checkpoint", "codex", "--hook-input", &hook_input])
        .expect("an authorization denial should preserve the hook exit-zero contract");

    assert!(output.contains("no repositories are allowed"));
    let db = BashHistoryDatabase::open_at_path(std::path::Path::new(&bash_db_path)).unwrap();
    assert!(
        db.all_calls_for_test().unwrap().is_empty(),
        "an empty-allowlist bash hook must not persist an attempt"
    );
    assert!(
        !repo
            .test_home_path()
            .join(".git-ai/internal/checkpoint-debug-logs")
            .exists(),
        "a malformed-repository bash hook must not persist its raw hook input"
    );
}

#[test]
fn test_bash_recovery_uses_commit_time_file_timestamps_when_processing_is_delayed() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [
        ("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str()),
        (
            "GIT_AI_TEST_DELAY_SIDE_EFFECT_MS_FOR_COMMAND",
            "remote=6500",
        ),
    ];
    let repo = TestRepo::new_with_daemon_env(&env);
    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("delayed.txt");

    fs::write(&file_path, "dirty pre-bash line\n").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-transcript.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "delayed-commit-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "delayed-commit-tool",
        "tool_input": { "command": "printf 'ai bash line\\n' >> delayed.txt" },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();
    repo.git_ai(&["checkpoint", "codex", "--hook-input", &pre_hook_input])
        .expect("codex pre-hook checkpoint should succeed");

    fs::write(&file_path, "dirty pre-bash line\nai bash line\n").unwrap();

    let post_hook_input = json!({
        "session_id": "delayed-commit-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "delayed-commit-tool",
        "tool_input": { "command": "printf 'ai bash line\\n' >> delayed.txt" },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();
    repo.git_ai(&["checkpoint", "codex", "--hook-input", &post_hook_input])
        .expect("codex post-hook checkpoint should succeed");

    repo.git_without_test_sync_for_test(
        &[
            "remote",
            "add",
            "delayed-side-effect",
            "https://example.invalid/repo.git",
        ],
        &[],
    )
    .expect("remote add should succeed");
    repo.git_without_test_sync_for_test(&["add", "-A"], &[])
        .expect("add should succeed");
    repo.git_without_test_sync_for_test(&["commit", "-m", "Delayed commit"], &[])
        .expect("commit should succeed");

    thread::sleep(Duration::from_millis(3500));
    fs::write(
        &file_path,
        "dirty pre-bash line\nai bash line\nmanual edit after commit\n",
    )
    .unwrap();

    let mut file = repo.filename("delayed.txt");
    file.assert_committed_lines(lines!["dirty pre-bash line".ai(), "ai bash line".ai(),]);
}

#[test]
fn test_codex_parent_cwd_bash_attempt_is_denied_before_persistence() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let repo = TestRepo::new_with_daemon_env_and_patch(&env, |patch| {
        patch.feature_flags = Some(json!({"checkpoint_debug_log": true}));
    });
    let repo_root = repo.canonical_path();
    let parent_cwd = repo_root.parent().unwrap().to_path_buf();
    let repo_name = repo_root.file_name().unwrap().to_string_lossy().to_string();

    fs::write(repo_root.join("README.md"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::create_dir_all(repo_root.join("src")).unwrap();
    let command = format!("cd {repo_name} && printf x >> src/parent-cwd.txt");
    let pre_hook_input = json!({
        "session_id": "parent-cwd-session",
        "cwd": parent_cwd.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "parent-cwd-tool",
        "tool_input": { "command": command },
        "model": "gpt-5"
    })
    .to_string();

    let pre_output = repo
        .git_ai_from_working_dir(
            &parent_cwd,
            &["checkpoint", "codex", "--hook-input", &pre_hook_input],
        )
        .expect("parent-cwd authorization denial should preserve hook exit zero");
    assert!(pre_output.contains("repository authorization could not be verified"));

    fs::write(repo_root.join("src/parent-cwd.txt"), "x\n").unwrap();

    let post_hook_input = json!({
        "session_id": "parent-cwd-session",
        "cwd": parent_cwd.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "parent-cwd-tool",
        "tool_input": { "command": command },
        "model": "gpt-5"
    })
    .to_string();

    let post_output = repo
        .git_ai_from_working_dir(
            &parent_cwd,
            &["checkpoint", "codex", "--hook-input", &post_hook_input],
        )
        .expect("parent-cwd authorization denial should preserve hook exit zero");
    assert!(post_output.contains("repository authorization could not be verified"));

    let commit = repo
        .stage_all_and_commit("Parent cwd bash write")
        .expect("commit should succeed");

    let mut file = repo.filename("src/parent-cwd.txt");
    file.assert_committed_lines(lines!["x".unattributed_human()]);
    assert!(
        commit.authorship_log.metadata.sessions.is_empty(),
        "a denied parent-cwd hook must not create false AI session attribution"
    );

    let db = BashHistoryDatabase::open_at_path(std::path::Path::new(&bash_db_path)).unwrap();
    assert!(
        db.all_calls_for_test().unwrap().is_empty(),
        "a denied parent-cwd hook must not persist BashHookAttempt metadata"
    );
    assert!(
        !repo
            .test_home_path()
            .join(".git-ai/internal/checkpoint-debug-logs")
            .exists(),
        "a denied parent-cwd hook must not persist raw debug input"
    );
}

#[test]
fn test_bash_recovery_does_not_attribute_manual_edit_after_unrelated_bash() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let repo = TestRepo::new_with_daemon_env(&env);
    let repo_root = repo.canonical_path();
    set_daemon_socket_for_test(repo.daemon_control_socket_path());

    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial commit should succeed");

    let agent = AgentId {
        tool: "codex".to_string(),
        id: "manual-after-bash-session".to_string(),
        model: "gpt-5".to_string(),
    };

    handle_bash_pre_tool_use_with_context(
        &repo_root,
        "manual-after-bash-session",
        "manual-after-bash-tool-1",
        &agent,
        None,
        "t_manualpre000",
        Some("true"),
    )
    .expect("pre bash hook should record durable start");

    let post_result = handle_bash_post_tool_use(
        &repo_root,
        "manual-after-bash-session",
        "manual-after-bash-tool-1",
        &agent,
        None,
        "t_manualpost00",
        Some("true"),
    )
    .expect("post bash hook should record durable end");
    assert!(
        matches!(post_result.action, BashCheckpointAction::NoChanges),
        "bash call should not emit a normal checkpoint"
    );

    fs::write(repo_root.join("manual-after.txt"), "manual after bash\n").unwrap();
    repo.stage_all_and_commit("Manual edit after bash").unwrap();

    let mut file = repo.filename("manual-after.txt");
    file.assert_committed_lines(lines!["manual after bash".unattributed_human()]);
}

#[test]
fn test_bash_recovery_does_not_attribute_manual_edit_before_unrelated_bash() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let repo = TestRepo::new_with_daemon_env(&env);
    let repo_root = repo.canonical_path();
    set_daemon_socket_for_test(repo.daemon_control_socket_path());

    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial commit should succeed");

    fs::write(repo_root.join("manual-before.txt"), "manual before bash\n").unwrap();

    let agent = AgentId {
        tool: "codex".to_string(),
        id: "manual-before-bash-session".to_string(),
        model: "gpt-5".to_string(),
    };

    handle_bash_pre_tool_use_with_context(
        &repo_root,
        "manual-before-bash-session",
        "manual-before-bash-tool-1",
        &agent,
        None,
        "t_manualbefpre",
        Some("true"),
    )
    .expect("pre bash hook should record durable start");

    let post_result = handle_bash_post_tool_use(
        &repo_root,
        "manual-before-bash-session",
        "manual-before-bash-tool-1",
        &agent,
        None,
        "t_manualbefpst",
        Some("true"),
    )
    .expect("post bash hook should record durable end");
    assert!(
        matches!(post_result.action, BashCheckpointAction::NoChanges),
        "bash call should not emit a normal checkpoint"
    );

    repo.stage_all_and_commit("Manual edit before bash")
        .unwrap();

    let mut file = repo.filename("manual-before.txt");
    file.assert_committed_lines(lines!["manual before bash".unattributed_human()]);
}

#[test]
fn test_bash_history_recovers_untracked_lines_when_post_snapshot_fails() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let repo = TestRepo::new_with_daemon_env(&env);
    let repo_root = repo.canonical_path();
    set_daemon_socket_for_test(repo.daemon_control_socket_path());

    let initial_path = repo_root.join("base.txt");
    fs::write(&initial_path, "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let agent = AgentId {
        tool: "codex".to_string(),
        id: "recover-bash-session".to_string(),
        model: "gpt-5".to_string(),
    };

    handle_bash_pre_tool_use_with_context(
        &repo_root,
        "recover-bash-session",
        "recover-tool-1",
        &agent,
        None,
        "t_recoverpre000",
        Some("printf recovered > recovered.txt"),
    )
    .expect("pre bash hook should record durable start");

    let recovered_path = repo_root.join("recovered.txt");
    fs::write(&recovered_path, "recovered by bash\n").unwrap();

    set_walk_timeout_ms_for_test(0);
    let post_result = handle_bash_post_tool_use(
        &repo_root,
        "recover-bash-session",
        "recover-tool-1",
        &agent,
        None,
        "t_recoverpost00",
        Some("printf recovered > recovered.txt"),
    )
    .expect("post bash hook should degrade gracefully");
    reset_timeout_overrides_for_test();
    assert!(
        matches!(post_result.action, BashCheckpointAction::SnapshotFailed),
        "post hook should not emit a normal checkpoint in this regression setup"
    );

    repo.stage_all_and_commit("Recover bash attribution")
        .unwrap();

    let mut recovered = repo.filename("recovered.txt");
    recovered.assert_committed_lines(lines!["recovered by bash".ai()]);
}

#[test]
fn test_bash_history_recovers_when_bash_checkpoint_was_recorded_elsewhere() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let source_repo = TestRepo::new_with_daemon_env(&env);
    let target_repo = TestRepo::new_with_daemon_env(&env);

    let source_root = source_repo.canonical_path();
    let target_root = target_repo.canonical_path();
    set_daemon_socket_for_test(source_repo.daemon_control_socket_path());

    let target_file = target_root.join("elsewhere.txt");
    fs::write(&target_file, "base\n").unwrap();
    target_repo
        .stage_all_and_commit("Initial target commit")
        .unwrap();

    let mut target = target_repo.filename("elsewhere.txt");
    target.assert_committed_lines(lines!["base".unattributed_human()]);

    let agent = AgentId {
        tool: "codex".to_string(),
        id: "cross-repo-bash-session".to_string(),
        model: "gpt-5".to_string(),
    };
    let command = format!("printf 'from elsewhere\\n' >> {}", target_file.display());

    handle_bash_pre_tool_use_with_context(
        &source_root,
        "cross-repo-bash-session",
        "cross-repo-tool-1",
        &agent,
        None,
        "t_crosspre000",
        Some(&command),
    )
    .expect("pre bash hook should record durable start from source repo");

    fs::write(&target_file, "base\nfrom elsewhere\n").unwrap();

    set_walk_timeout_ms_for_test(0);
    let post_result = handle_bash_post_tool_use(
        &source_root,
        "cross-repo-bash-session",
        "cross-repo-tool-1",
        &agent,
        None,
        "t_crosspost00",
        Some(&command),
    )
    .expect("post bash hook should degrade gracefully");
    reset_timeout_overrides_for_test();
    assert!(
        matches!(post_result.action, BashCheckpointAction::SnapshotFailed),
        "post hook should not emit a normal checkpoint in this regression setup"
    );

    target_repo
        .stage_all_and_commit("Recover cross-repo bash attribution")
        .unwrap();
    target.assert_committed_lines(lines!["base".unattributed_human(), "from elsewhere".ai()]);
}

#[test]
fn test_bash_history_recovers_dirty_lines_present_before_bash() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let repo = TestRepo::new_with_daemon_env(&env);
    let repo_root = repo.canonical_path();
    set_daemon_socket_for_test(repo.daemon_control_socket_path());

    let file_path = repo_root.join("mixed.txt");
    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::write(&file_path, "base\ndirty before bash\n").unwrap();
    repo.git_ai(&["checkpoint", "human", "mixed.txt"])
        .expect("legacy pre-bash checkpoint should record dirty untracked content");

    let agent = AgentId {
        tool: "codex".to_string(),
        id: "recover-mixed-bash-session".to_string(),
        model: "gpt-5".to_string(),
    };

    handle_bash_pre_tool_use_with_context(
        &repo_root,
        "recover-mixed-bash-session",
        "recover-mixed-tool-1",
        &agent,
        None,
        "t_mixedpre0000",
        Some("printf recovered >> mixed.txt"),
    )
    .expect("pre bash hook should record durable start");

    fs::write(&file_path, "base\ndirty before bash\nbash recovered line\n").unwrap();

    set_walk_timeout_ms_for_test(0);
    let post_result = handle_bash_post_tool_use(
        &repo_root,
        "recover-mixed-bash-session",
        "recover-mixed-tool-1",
        &agent,
        None,
        "t_mixedpost000",
        Some("printf recovered >> mixed.txt"),
    )
    .expect("post bash hook should degrade gracefully");
    reset_timeout_overrides_for_test();
    assert!(
        matches!(post_result.action, BashCheckpointAction::SnapshotFailed),
        "post hook should not emit a normal checkpoint in this regression setup"
    );

    repo.stage_all_and_commit("Recover dirty and bash lines")
        .unwrap();

    let mut file = repo.filename("mixed.txt");
    file.assert_committed_lines(lines![
        "base".unattributed_human(),
        "dirty before bash".ai(),
        "bash recovered line".ai(),
    ]);
}

#[test]
fn test_bash_history_recovers_shifted_dirty_lines_present_before_bash() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let repo = TestRepo::new_with_daemon_env(&env);
    let repo_root = repo.canonical_path();
    set_daemon_socket_for_test(repo.daemon_control_socket_path());

    let file_path = repo_root.join("shifted.txt");
    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::write(&file_path, "base\ndirty before bash\n").unwrap();
    repo.git_ai(&["checkpoint", "human", "shifted.txt"])
        .expect("legacy pre-bash checkpoint should record dirty untracked content");

    let agent = AgentId {
        tool: "codex".to_string(),
        id: "recover-shifted-bash-session".to_string(),
        model: "gpt-5".to_string(),
    };

    handle_bash_pre_tool_use_with_context(
        &repo_root,
        "recover-shifted-bash-session",
        "recover-shifted-tool-1",
        &agent,
        None,
        "t_shiftpre000",
        Some("python - <<'PY'\nfrom pathlib import Path\np = Path('shifted.txt')\np.write_text('bash recovered line\\n' + p.read_text())\nPY"),
    )
    .expect("pre bash hook should record durable start");

    fs::write(&file_path, "bash recovered line\nbase\ndirty before bash\n").unwrap();

    set_walk_timeout_ms_for_test(0);
    let post_result = handle_bash_post_tool_use(
        &repo_root,
        "recover-shifted-bash-session",
        "recover-shifted-tool-1",
        &agent,
        None,
        "t_shiftpost00",
        Some("python - <<'PY'\nfrom pathlib import Path\np = Path('shifted.txt')\np.write_text('bash recovered line\\n' + p.read_text())\nPY"),
    )
    .expect("post bash hook should degrade gracefully");
    reset_timeout_overrides_for_test();
    assert!(
        matches!(post_result.action, BashCheckpointAction::SnapshotFailed),
        "post hook should not emit a normal checkpoint in this regression setup"
    );

    repo.stage_all_and_commit("Recover shifted dirty and bash lines")
        .unwrap();

    let mut file = repo.filename("shifted.txt");
    file.assert_committed_lines(lines![
        "bash recovered line".ai(),
        "base".unattributed_human(),
        "dirty before bash".ai(),
    ]);
}

#[test]
fn test_edge_extension_recovers_unknown_gap_between_ai_attributions() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("edge.txt");

    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::write(&file_path, "base\nai before\nunknown gap\nai after\n").unwrap();
    repo.git_ai(&["checkpoint", "human", "edge.txt"])
        .expect("legacy human checkpoint should mark current content untracked");

    fs::write(
        &file_path,
        "base\nai before edited\nunknown gap\nai after edited\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "edge.txt"])
        .expect("AI checkpoint should only directly attribute the edited lines");

    repo.stage_all_and_commit("Recover edge attribution")
        .unwrap();

    let mut file = repo.filename("edge.txt");
    file.assert_committed_lines(lines![
        "base".unattributed_human(),
        "ai before edited".ai(),
        "unknown gap".ai(),
        "ai after edited".ai(),
    ]);
}

#[test]
fn test_edge_extension_recovers_leading_and_trailing_unknown_lines_near_ai_block() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("edge-fringes.txt");

    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::write(
        &file_path,
        "\
base
leading dirty
ai one placeholder
ai two placeholder
ai three placeholder
trailing dirty 1
trailing dirty 2
trailing dirty 3
trailing dirty 4
",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "human", "edge-fringes.txt"])
        .expect("legacy human checkpoint should mark current content untracked");

    fs::write(
        &file_path,
        "\
base
leading dirty
ai one
ai two
ai three
trailing dirty 1
trailing dirty 2
trailing dirty 3
trailing dirty 4
",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "edge-fringes.txt"])
        .expect("AI checkpoint should only directly attribute the edited block");

    repo.stage_all_and_commit("Recover edge fringes").unwrap();

    let mut file = repo.filename("edge-fringes.txt");
    file.assert_committed_lines(lines![
        "base".unattributed_human(),
        "leading dirty".ai(),
        "ai one".ai(),
        "ai two".ai(),
        "ai three".ai(),
        "trailing dirty 1".ai(),
        "trailing dirty 2".ai(),
        "trailing dirty 3".ai(),
        "trailing dirty 4".unattributed_human(),
    ]);
}
