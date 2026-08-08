use crate::repos::test_repo::TestRepo;
use crate::test_utils::{
    CodexHookInput, DurationStatistics, read_jsonl_fixture, write_executable_script,
};
use git_ai::model::working_log::{AgentId, CheckpointKind};
use git_ai::operations::commands::checkpoint_agent::orchestrator::{
    BaseCommit, CheckpointFile, CheckpointRequest,
};
use git_ai::operations::daemon::checkpoint::PreparedPathRole;
use git_ai::operations::git::find_repository_in_path;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn build_scoped_human_checkpoint_request(
    repo_path: &str,
    scope_paths: Vec<String>,
) -> CheckpointRequest {
    static TEST_HUMAN_SCOPE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let session = TEST_HUMAN_SCOPE_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    CheckpointRequest {
        trace_id: format!("test-human-scope-{}", session),
        checkpoint_kind: CheckpointKind::Human,
        agent_id: Some(AgentId {
            tool: "test_harness".to_string(),
            id: format!("test-human-scope-{}", session),
            model: "test_model".to_string(),
        }),
        files: scope_paths
            .into_iter()
            .map(|p| CheckpointFile {
                path: PathBuf::from(&p),
                content: None,
                repo_work_dir: PathBuf::from(repo_path),
                base_commit: BaseCommit::Sha(
                    "0000000000000000000000000000000000000000".to_string(),
                ),
            })
            .collect(),
        path_role: PreparedPathRole::WillEdit,
        stream_source: None,
        metadata: HashMap::new(),
    }
}

fn apply_default_checkpoint_scope(
    repo_path: &str,
    scope_paths: Vec<String>,
    checkpoint_request: Option<CheckpointRequest>,
    checkpoint_kind: CheckpointKind,
) -> Option<CheckpointRequest> {
    match checkpoint_request {
        Some(mut result) => {
            let has_explicit_scope = !result.files.is_empty();

            if !has_explicit_scope {
                result.files = scope_paths
                    .into_iter()
                    .map(|p| CheckpointFile {
                        path: PathBuf::from(&p),
                        content: None,
                        repo_work_dir: PathBuf::from(repo_path),
                        base_commit: BaseCommit::Sha(
                            "0000000000000000000000000000000000000000".to_string(),
                        ),
                    })
                    .collect();
                if checkpoint_kind == CheckpointKind::Human {
                    result.path_role = PreparedPathRole::WillEdit;
                } else {
                    result.path_role = PreparedPathRole::Edited;
                }
            }

            Some(result)
        }
        None => {
            if scope_paths.is_empty() {
                None
            } else {
                Some(build_scoped_human_checkpoint_request(
                    repo_path,
                    scope_paths,
                ))
            }
        }
    }
}

#[test]
fn test_build_scoped_human_agent_run_result_uses_current_changed_paths() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("tracked.txt"), "base\n").unwrap();
    repo.git_og(&["add", "."]).unwrap();
    repo.git_og(&["commit", "-m", "base commit"]).unwrap();

    fs::write(repo.path().join("tracked.txt"), "base\nchanged\n").unwrap();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let mut paths: Vec<String> = gitai_repo
        .get_staged_and_unstaged_filenames()
        .unwrap()
        .into_iter()
        .collect();
    paths.sort();

    assert!(!paths.is_empty(), "changed file should produce scope paths");

    let scoped = build_scoped_human_checkpoint_request(repo.path().to_str().unwrap(), paths);

    assert_eq!(scoped.checkpoint_kind, CheckpointKind::Human);
    assert_eq!(scoped.path_role, PreparedPathRole::WillEdit);
    let file_paths: Vec<PathBuf> = scoped.files.iter().map(|f| f.path.clone()).collect();
    assert_eq!(file_paths, vec![PathBuf::from("tracked.txt")]);
    assert_eq!(
        scoped.files[0].repo_work_dir,
        PathBuf::from(repo.path().to_string_lossy().to_string())
    );
}

#[test]
fn test_apply_default_checkpoint_scope_preserves_existing_explicit_scope() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("tracked.txt"), "base\n").unwrap();
    repo.git_og(&["add", "."]).unwrap();
    repo.git_og(&["commit", "-m", "base commit"]).unwrap();

    fs::write(repo.path().join("tracked.txt"), "base\nchanged\n").unwrap();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let mut scope_paths: Vec<String> = gitai_repo
        .get_staged_and_unstaged_filenames()
        .unwrap()
        .into_iter()
        .collect();
    scope_paths.sort();

    let original = CheckpointRequest {
        trace_id: "test-session".to_string(),
        checkpoint_kind: CheckpointKind::Human,
        agent_id: Some(AgentId {
            tool: "test-tool".to_string(),
            id: "test-session".to_string(),
            model: "test-model".to_string(),
        }),
        files: vec![CheckpointFile {
            path: PathBuf::from("custom.txt"),
            content: None,
            repo_work_dir: PathBuf::new(),
            base_commit: BaseCommit::Sha("0000000000000000000000000000000000000000".to_string()),
        }],
        path_role: PreparedPathRole::WillEdit,
        stream_source: None,
        metadata: HashMap::new(),
    };

    let applied = apply_default_checkpoint_scope(
        repo.path().to_str().unwrap(),
        scope_paths,
        Some(original.clone()),
        CheckpointKind::Human,
    )
    .expect("explicit scope should be preserved");

    let applied_paths: Vec<PathBuf> = applied.files.iter().map(|f| f.path.clone()).collect();
    let original_paths: Vec<PathBuf> = original.files.iter().map(|f| f.path.clone()).collect();
    assert_eq!(applied_paths, original_paths);
    assert_eq!(
        applied.files[0].repo_work_dir,
        original.files[0].repo_work_dir
    );
}

#[test]
fn codex_hook_input_builder_serializes_the_stable_file_edit_shape() {
    let hook_input = CodexHookInput::pre_file_edit(
        "session-123",
        Path::new("/tmp/repo"),
        "patch-1",
        Path::new("/tmp/repo/src/lib.rs"),
    )
    .with_model("gpt-5")
    .with_transcript_path(Path::new("/tmp/codex-session.jsonl"))
    .build();

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&hook_input).unwrap(),
        serde_json::json!({
            "session_id": "session-123",
            "cwd": "/tmp/repo",
            "hook_event_name": "PreToolUse",
            "tool_name": "apply_patch",
            "tool_use_id": "patch-1",
            "model": "gpt-5",
            "tool_input": {
                "patch": "*** Update File: /tmp/repo/src/lib.rs\n"
            },
            "transcript_path": "/tmp/codex-session.jsonl"
        })
    );
}

#[test]
fn codex_hook_input_builder_serializes_bash_commands_and_responses() {
    let hook_input = CodexHookInput::post_bash(
        "session-123",
        Path::new("/tmp/repo"),
        "bash-1",
        "echo hello",
    )
    .with_tool_response("hello\n")
    .build();

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&hook_input).unwrap(),
        serde_json::json!({
            "session_id": "session-123",
            "cwd": "/tmp/repo",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": "bash-1",
            "tool_input": { "command": "echo hello" },
            "tool_response": "hello\n"
        })
    );
}

#[test]
fn codex_hook_input_builder_preserves_explicit_patch_content() {
    let hook_input = CodexHookInput::post_file_edit(
        "session-123",
        Path::new("/tmp/repo"),
        "patch-1",
        Path::new("/tmp/repo/src/lib.rs"),
    )
    .with_patch("*** Update File: /tmp/repo/src/lib.rs\n+fn added() {}\n")
    .build();

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&hook_input).unwrap(),
        serde_json::json!({
            "session_id": "session-123",
            "cwd": "/tmp/repo",
            "hook_event_name": "PostToolUse",
            "tool_name": "apply_patch",
            "tool_use_id": "patch-1",
            "tool_input": {
                "patch": "*** Update File: /tmp/repo/src/lib.rs\n+fn added() {}\n"
            }
        })
    );
}

#[test]
fn duration_statistics_handles_empty_and_single_samples() {
    let empty = DurationStatistics::from_durations(&[]);
    assert_eq!(empty.count(), 0);
    assert_eq!(empty.min(), None);
    assert_eq!(empty.max(), None);
    assert_eq!(empty.average(), None);
    assert_eq!(empty.percentile_nearest_rank(0.95), None);
    assert_eq!(empty.percentile_upper_index(0.95), None);
    assert_eq!(empty.std_dev_ms(), None);

    let single_duration = Duration::from_millis(7);
    let single = DurationStatistics::from_durations(&[single_duration]);
    assert_eq!(single.count(), 1);
    assert_eq!(single.min(), Some(single_duration));
    assert_eq!(single.max(), Some(single_duration));
    assert_eq!(single.average(), Some(single_duration));
    assert_eq!(single.percentile_nearest_rank(0.95), Some(single_duration));
    assert_eq!(single.percentile_upper_index(0.95), Some(single_duration));
    assert_eq!(single.std_dev_ms(), Some(0.0));
}

#[test]
fn duration_statistics_sorts_samples_and_preserves_percentile_boundaries() {
    let stats = DurationStatistics::from_durations(&[
        Duration::from_millis(40),
        Duration::from_millis(10),
        Duration::from_millis(30),
        Duration::from_millis(20),
    ]);

    assert_eq!(
        stats.sorted_samples(),
        &[
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(30),
            Duration::from_millis(40),
        ],
    );
    assert_eq!(stats.min(), Some(Duration::from_millis(10)));
    assert_eq!(stats.max(), Some(Duration::from_millis(40)));
    assert_eq!(stats.average(), Some(Duration::from_millis(25)));
    assert_eq!(
        stats.percentile_nearest_rank(0.0),
        Some(Duration::from_millis(10))
    );
    assert_eq!(
        stats.percentile_nearest_rank(1.0),
        Some(Duration::from_millis(40))
    );
    assert_eq!(
        stats.percentile_upper_index(0.5),
        Some(Duration::from_millis(30))
    );
    assert_eq!(
        stats.percentile_upper_index(1.0),
        Some(Duration::from_millis(40))
    );
    assert!((stats.std_dev_ms().unwrap() - 125.0_f64.sqrt()).abs() < 1e-12);
}

#[test]
fn read_jsonl_fixture_treats_empty_input_as_no_values() {
    let fixture = tempfile::NamedTempFile::new().unwrap();

    assert!(read_jsonl_fixture(fixture.path()).unwrap().is_empty());
}

#[test]
fn read_jsonl_fixture_skips_blank_lines_and_rejects_malformed_jsonl() {
    let fixture = tempfile::NamedTempFile::new().unwrap();
    fs::write(
        fixture.path(),
        "\n  \n{\"event\":\"first\"}\n\t\n{\"event\":\"second\"}\n",
    )
    .unwrap();

    assert_eq!(
        read_jsonl_fixture(fixture.path()).unwrap(),
        vec![
            serde_json::json!({ "event": "first" }),
            serde_json::json!({ "event": "second" }),
        ],
    );

    fs::write(fixture.path(), "{\"event\":\"valid\"}\nnot-json\n").unwrap();
    let error = read_jsonl_fixture(fixture.path()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn write_executable_script_writes_exact_bytes_and_sets_unix_mode() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = temp.path().join("fixture.sh");

    write_executable_script(&fixture, b"#!/bin/sh\nprintf 'long fixture'\n").unwrap();
    assert_eq!(
        fs::read(&fixture).unwrap(),
        b"#!/bin/sh\nprintf 'long fixture'\n"
    );

    write_executable_script(&fixture, b"#!/bin/sh\ntrue\n").unwrap();
    assert_eq!(fs::read(&fixture).unwrap(), b"#!/bin/sh\ntrue\n");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = fs::metadata(&fixture).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }
}
