use crate::repos::test_repo::TestRepo;
use serde_json::json;
use std::fs;

/// Verify that tool_use_id from hook input propagates to checkpoint agent_metadata
/// for file edit events (non-bash). This is the data that feeds external_tool_use_id
/// in telemetry metrics.
#[test]
fn test_claude_file_edit_checkpoint_propagates_tool_use_id_to_metadata() {
    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();

    let file_path = repo_root.join("example.ts");
    fs::write(&file_path, "const x = 1;\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let transcript_path = repo_root.join("session.jsonl");
    fs::write(&transcript_path, "{}\n").unwrap();

    let pre_hook = json!({
        "cwd": repo_root.to_string_lossy(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_use_id": "toolu_01ABC123",
        "session_id": "sess-telemetry-test",
        "transcript_path": transcript_path.to_string_lossy(),
        "tool_input": {
            "file_path": file_path.to_string_lossy()
        }
    })
    .to_string();

    repo.checkpoint_with_hook_input("claude", &pre_hook)
        .expect("pre checkpoint should succeed");

    fs::write(&file_path, "const x = 1;\nconst y = 2;\n").unwrap();

    let post_hook = json!({
        "cwd": repo_root.to_string_lossy(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Write",
        "tool_use_id": "toolu_01ABC123",
        "session_id": "sess-telemetry-test",
        "transcript_path": transcript_path.to_string_lossy(),
        "tool_input": {
            "file_path": file_path.to_string_lossy()
        }
    })
    .to_string();

    repo.checkpoint_with_hook_input("claude", &post_hook)
        .expect("post checkpoint should succeed");

    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    let ai_checkpoint = checkpoints
        .iter()
        .find(|c| c.kind == git_ai::model::working_log::CheckpointKind::AiAgent)
        .expect("Should have an AI agent checkpoint");

    let metadata = ai_checkpoint
        .agent_metadata
        .as_ref()
        .expect("AI checkpoint should have agent_metadata");

    assert_eq!(
        metadata.get("tool_use_id"),
        Some(&"toolu_01ABC123".to_string()),
        "tool_use_id must propagate to agent_metadata for telemetry emission"
    );
}

/// Verify tool_use_id propagation for bash tool events (e.g., codex preset).
#[test]
fn test_codex_bash_checkpoint_propagates_tool_use_id_to_metadata() {
    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();

    let file_path = repo_root.join("src/main.rs");
    fs::create_dir_all(repo_root.join("src")).unwrap();
    fs::write(&file_path, "fn main() {}\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let transcript_path = repo_root.join("codex-session.jsonl");
    fs::write(&transcript_path, "{}\n").unwrap();

    let pre_hook = json!({
        "session_id": "codex-telem-sess",
        "cwd": repo_root.to_string_lossy(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "tu-bash-789xyz",
        "tool_input": {
            "command": "echo hello"
        },
        "transcript_path": transcript_path.to_string_lossy()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook)
        .expect("pre bash checkpoint should succeed");

    fs::write(&file_path, "fn main() {}\nfn added() {}\n").unwrap();

    let post_hook = json!({
        "session_id": "codex-telem-sess",
        "cwd": repo_root.to_string_lossy(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "tu-bash-789xyz",
        "tool_input": {
            "command": "echo hello"
        },
        "transcript_path": transcript_path.to_string_lossy()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook)
        .expect("post bash checkpoint should succeed");

    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    let ai_checkpoint = checkpoints
        .iter()
        .find(|c| c.kind == git_ai::model::working_log::CheckpointKind::AiAgent)
        .expect("Should have an AI agent checkpoint");

    let metadata = ai_checkpoint
        .agent_metadata
        .as_ref()
        .expect("AI checkpoint should have agent_metadata");

    assert_eq!(
        metadata.get("tool_use_id"),
        Some(&"tu-bash-789xyz".to_string()),
        "tool_use_id from bash events must propagate to agent_metadata for telemetry"
    );
}

/// Verify that edit_kind="file_edit" is set for file edit checkpoints.
#[test]
fn test_file_edit_checkpoint_has_edit_kind_metadata() {
    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();

    let file_path = repo_root.join("example.ts");
    fs::write(&file_path, "const x = 1;\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let transcript_path = repo_root.join("session.jsonl");
    fs::write(&transcript_path, "{}\n").unwrap();

    let pre_hook = json!({
        "cwd": repo_root.to_string_lossy(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_use_id": "toolu_edit_kind_1",
        "session_id": "sess-edit-kind",
        "transcript_path": transcript_path.to_string_lossy(),
        "tool_input": {
            "file_path": file_path.to_string_lossy()
        }
    })
    .to_string();

    repo.checkpoint_with_hook_input("claude", &pre_hook)
        .expect("pre checkpoint should succeed");

    fs::write(&file_path, "const x = 1;\nconst y = 2;\n").unwrap();

    let post_hook = json!({
        "cwd": repo_root.to_string_lossy(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Write",
        "tool_use_id": "toolu_edit_kind_1",
        "session_id": "sess-edit-kind",
        "transcript_path": transcript_path.to_string_lossy(),
        "tool_input": {
            "file_path": file_path.to_string_lossy()
        }
    })
    .to_string();

    repo.checkpoint_with_hook_input("claude", &post_hook)
        .expect("post checkpoint should succeed");

    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    let ai_checkpoint = checkpoints
        .iter()
        .find(|c| c.kind == git_ai::model::working_log::CheckpointKind::AiAgent)
        .expect("Should have an AI agent checkpoint");

    let metadata = ai_checkpoint
        .agent_metadata
        .as_ref()
        .expect("AI checkpoint should have agent_metadata");

    assert_eq!(
        metadata.get("edit_kind"),
        Some(&"file_edit".to_string()),
        "file edit checkpoints must have edit_kind=file_edit"
    );
}

/// Verify that edit_kind="bash" is set for bash tool checkpoints.
#[test]
fn test_bash_checkpoint_has_edit_kind_metadata() {
    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();

    let file_path = repo_root.join("src/main.rs");
    fs::create_dir_all(repo_root.join("src")).unwrap();
    fs::write(&file_path, "fn main() {}\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let transcript_path = repo_root.join("codex-session.jsonl");
    fs::write(&transcript_path, "{}\n").unwrap();

    let pre_hook = json!({
        "session_id": "codex-edit-kind-sess",
        "cwd": repo_root.to_string_lossy(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "tu-bash-edit-kind",
        "tool_input": {
            "command": "echo hello"
        },
        "transcript_path": transcript_path.to_string_lossy()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook)
        .expect("pre bash checkpoint should succeed");

    fs::write(&file_path, "fn main() {}\nfn added() {}\n").unwrap();

    let post_hook = json!({
        "session_id": "codex-edit-kind-sess",
        "cwd": repo_root.to_string_lossy(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "tu-bash-edit-kind",
        "tool_input": {
            "command": "echo hello"
        },
        "transcript_path": transcript_path.to_string_lossy()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook)
        .expect("post bash checkpoint should succeed");

    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    let ai_checkpoint = checkpoints
        .iter()
        .find(|c| c.kind == git_ai::model::working_log::CheckpointKind::AiAgent)
        .expect("Should have an AI agent checkpoint");

    let metadata = ai_checkpoint
        .agent_metadata
        .as_ref()
        .expect("AI checkpoint should have agent_metadata");

    assert_eq!(
        metadata.get("edit_kind"),
        Some(&"bash".to_string()),
        "bash checkpoints must have edit_kind=bash"
    );
}

/// Verify that tool_use_id propagation works for the gemini preset.
#[test]
fn test_gemini_file_edit_checkpoint_propagates_tool_use_id_to_metadata() {
    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();

    let file_path = repo_root.join("app.py");
    fs::write(&file_path, "print('hello')\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let transcript_path = repo_root.join("gemini-session.jsonl");
    fs::write(&transcript_path, "{}\n").unwrap();

    let pre_hook = json!({
        "cwd": repo_root.to_string_lossy(),
        "hook_event_name": "PreToolUse",
        "tool_name": "WriteFile",
        "tool_use_id": "gemini-tu-456",
        "session_id": "gemini-sess-1",
        "transcript_path": transcript_path.to_string_lossy(),
        "tool_input": {
            "file_path": file_path.to_string_lossy()
        }
    })
    .to_string();

    repo.checkpoint_with_hook_input("gemini", &pre_hook)
        .expect("gemini pre checkpoint should succeed");

    fs::write(&file_path, "print('hello')\nprint('world')\n").unwrap();

    let post_hook = json!({
        "cwd": repo_root.to_string_lossy(),
        "hook_event_name": "PostToolUse",
        "tool_name": "WriteFile",
        "tool_use_id": "gemini-tu-456",
        "session_id": "gemini-sess-1",
        "transcript_path": transcript_path.to_string_lossy(),
        "tool_input": {
            "file_path": file_path.to_string_lossy()
        }
    })
    .to_string();

    repo.checkpoint_with_hook_input("gemini", &post_hook)
        .expect("gemini post checkpoint should succeed");

    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    let ai_checkpoint = checkpoints
        .iter()
        .find(|c| c.kind == git_ai::model::working_log::CheckpointKind::AiAgent)
        .expect("Should have an AI agent checkpoint");

    let metadata = ai_checkpoint
        .agent_metadata
        .as_ref()
        .expect("AI checkpoint should have agent_metadata");

    assert_eq!(
        metadata.get("tool_use_id"),
        Some(&"gemini-tu-456".to_string()),
        "tool_use_id must propagate for gemini preset file edit events"
    );
}
