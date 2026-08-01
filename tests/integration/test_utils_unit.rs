use crate::repos::test_repo::TestRepo;
use crate::test_utils::CodexHookInput;
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
