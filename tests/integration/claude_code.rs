use crate::test_utils::{fixture_path, read_jsonl_fixture};
use git_ai::model::stream_watermark::ByteOffsetWatermark;
use git_ai::operations::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::operations::streams::agent::Agent;
use git_ai::operations::streams::agents::ClaudeAgent;
use serde_json::json;
use std::fs;
use std::io::Write;

#[test]
fn test_claude_code_raw_event_fidelity() {
    let fixture = fixture_path("example-claude-code.jsonl");
    let agent = ClaudeAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Failed to parse JSONL");

    let expected = read_jsonl_fixture(&fixture).unwrap();

    assert_eq!(result.events, expected);
}

#[test]
fn test_claude_preset_extracts_edited_filepath() {
    let hook_input = r##"{
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PostToolUse",
        "permission_mode": "default",
        "session_id": "23aad27c-175d-427f-ac5f-a6830b8e6e65",
        "tool_input": {
            "file_path": "/Users/svarlamov/projects/testing-git/README.md",
            "new_string": "# Testing Git Repository",
            "old_string": "# Testing Git"
        },
        "tool_name": "Edit",
        "transcript_path": "tests/fixtures/example-claude-code.jsonl"
    }"##;

    let events = resolve_preset("claude")
        .unwrap()
        .parse(hook_input, "t_test")
        .expect("Failed to run ClaudePreset");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(!e.file_paths.is_empty());
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("README.md"))
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_claude_preset_no_filepath_when_tool_input_missing() {
    let hook_input = r##"{
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PostToolUse",
        "session_id": "23aad27c-175d-427f-ac5f-a6830b8e6e65",
        "tool_name": "Read",
        "transcript_path": "tests/fixtures/example-claude-code.jsonl"
    }"##;

    let events = resolve_preset("claude")
        .unwrap()
        .parse(hook_input, "t_test")
        .expect("Failed to run ClaudePreset");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.file_paths.is_empty());
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_claude_preset_ignores_vscode_copilot_payload() {
    let hook_input = json!({
        "hookEventName": "PreToolUse",
        "cwd": "/Users/test/project",
        "toolName": "copilot_replaceString",
        "transcript_path": "/Users/test/Library/Application Support/Code/User/workspaceStorage/workspace-id/GitHub.copilot-chat/transcripts/copilot-session-1.jsonl",
        "toolInput": {
            "file_path": "/Users/test/project/src/main.ts"
        },
        "sessionId": "copilot-session-1",
        "model": "copilot/claude-sonnet-4"
    })
    .to_string();

    let result = resolve_preset("claude")
        .unwrap()
        .parse(&hook_input, "t_test");
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Skipping VS Code hook payload in Claude preset")
    );
}

#[test]
fn test_claude_preset_ignores_cursor_payload() {
    let hook_input = json!({
        "conversation_id": "dff2bf79-6a53-446c-be41-f33512532fb0",
        "model": "default",
        "tool_name": "Write",
        "tool_input": {
            "file_path": "/Users/test/project/jokes.csv"
        },
        "transcript_path": "/Users/test/.cursor/projects/Users-test-project/agent-transcripts/dff2bf79-6a53-446c-be41-f33512532fb0/dff2bf79-6a53-446c-be41-f33512532fb0.jsonl",
        "hook_event_name": "postToolUse",
        "cursor_version": "2.5.26",
        "workspace_roots": ["/Users/test/project"]
    })
    .to_string();

    let result = resolve_preset("claude")
        .unwrap()
        .parse(&hook_input, "t_test");
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Skipping Cursor hook payload in Claude preset")
    );
}

#[test]
fn test_claude_preset_does_not_ignore_when_transcript_path_is_claude() {
    let temp = tempfile::tempdir().unwrap();
    let claude_dir = temp.path().join(".claude").join("projects");
    fs::create_dir_all(&claude_dir).unwrap();

    let transcript_path = claude_dir.join("session.jsonl");
    let fixture = fixture_path("example-claude-code.jsonl");
    let mut dst = std::fs::File::create(&transcript_path).unwrap();
    let src = std::fs::read(fixture).unwrap();
    dst.write_all(&src).unwrap();

    let hook_input = json!({
        "hookEventName": "PostToolUse",
        "cwd": "/Users/test/project",
        "toolName": "copilot_replaceString",
        "toolInput": {
            "file_path": "/Users/test/project/src/main.ts"
        },
        "sessionId": "copilot-session-2",
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    let events = resolve_preset("claude")
        .unwrap()
        .parse(&hook_input, "t_test")
        .expect("Expected native Claude preset handling");

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "claude");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_claude_e2e_prefers_latest_checkpoint_for_prompts() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();

    // Enable prompt sharing for all repositories (empty blacklist = no exclusions)
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]); // No exclusions = share everywhere
    });

    let repo_root = repo.canonical_path();

    // Create initial file and commit
    let src_dir = repo_root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    let file_path = src_dir.join("main.rs");
    fs::write(&file_path, "fn main() {}\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Use a stable transcript path so both checkpoints share the same agent_id
    let transcript_path = repo_root.join("claude-session.jsonl");

    // First checkpoint: empty transcript (simulates race where data isn't ready yet)
    fs::write(&transcript_path, "").unwrap();
    let hook_input = json!({
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();

    // First AI edit and checkpoint with empty transcript/model
    fs::write(&file_path, "fn main() {}\n// ai line one\n").unwrap();
    repo.checkpoint_with_hook_input("claude", &hook_input)
        .unwrap();

    // Second AI edit with the real transcript content
    let fixture = fixture_path("example-claude-code.jsonl");
    fs::copy(&fixture, &transcript_path).unwrap();
    fs::write(&file_path, "fn main() {}\n// ai line one\n// ai line two\n").unwrap();
    repo.checkpoint_with_hook_input("claude", &hook_input)
        .unwrap();

    // Commit the changes
    let commit = repo.stage_all_and_commit("Add AI lines").unwrap();

    // We should have exactly one session record keyed by the claude agent_id
    assert_eq!(
        commit.authorship_log.metadata.sessions.len(),
        1,
        "Expected a single session record"
    );
    let session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Session record should exist");

    // Model is extracted from the real transcript fixture copied in the second checkpoint
    assert_eq!(
        session_record.agent_id.model, "claude-sonnet-4-20250514",
        "Session record model should come from the latest checkpoint's transcript"
    );
}

#[test]
fn test_claude_code_thinking_raw_event_fidelity() {
    let fixture = fixture_path("claude-code-with-thinking.jsonl");
    let agent = ClaudeAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Failed to parse JSONL");

    let expected = read_jsonl_fixture(&fixture).unwrap();

    assert_eq!(result.events, expected);
}

#[test]
fn test_claude_code_plan_raw_event_fidelity() {
    let fixture = fixture_path("claude-code-with-plan.jsonl");
    let agent = ClaudeAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Failed to parse JSONL");

    let expected = read_jsonl_fixture(&fixture).unwrap();

    assert_eq!(result.events, expected);
}

#[test]
#[serial_test::serial]
fn test_claude_subagent_checkpoint_sets_parent_session_id() {
    use crate::repos::test_repo::TestRepo;

    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("src").join("main.rs");
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    std::fs::write(&file_path, "fn main() {}\n").unwrap();

    let subagent_path = repo_root
        .join(".claude")
        .join("projects")
        .join("proj")
        .join("parent-uuid-abc")
        .join("subagents")
        .join("agent-xyz123.jsonl");
    std::fs::create_dir_all(subagent_path.parent().unwrap()).unwrap();
    std::fs::write(&subagent_path, "{\"type\":\"user\",\"message\":{\"content\":\"test\"},\"timestamp\":\"2026-01-01T00:00:00Z\"}\n").unwrap();

    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "session_id": "agent-xyz123",
        "cwd": repo_root.to_string_lossy().to_string(),
        "transcript_path": subagent_path.to_string_lossy().to_string(),
        "tool_name": "Edit",
        "tool_use_id": "toolu_test_001",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();

    let preset = resolve_preset("claude").unwrap();
    let events = preset.parse(&hook_input, "t_test").unwrap();

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            let ts = e
                .stream_source
                .as_ref()
                .expect("should have transcript source");
            assert_eq!(
                ts.external_parent_session_id,
                Some("parent-uuid-abc".to_string()),
                "Claude subagent checkpoint should detect parent session from path"
            );
            assert_eq!(ts.external_session_id, "agent-xyz123",);
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_claude_normal_session_checkpoint_has_no_parent() {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let transcript_path = temp_dir.path().join("normal-session-uuid.jsonl");
    std::fs::write(&transcript_path, "{\"type\":\"user\",\"message\":{\"content\":\"test\"},\"timestamp\":\"2026-01-01T00:00:00Z\"}\n").unwrap();

    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "session_id": "normal-session-uuid",
        "cwd": "/tmp/project",
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_name": "Edit",
        "tool_use_id": "toolu_test_002",
        "tool_input": {
            "file_path": "/tmp/project/src/main.rs"
        }
    })
    .to_string();

    let preset = resolve_preset("claude").unwrap();
    let events = preset.parse(&hook_input, "t_test").unwrap();

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            let ts = e
                .stream_source
                .as_ref()
                .expect("should have transcript source");
            assert_eq!(
                ts.external_parent_session_id, None,
                "Normal (non-subagent) Claude session should have no parent"
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

crate::reuse_tests_in_worktree!(
    test_claude_code_raw_event_fidelity,
    test_claude_code_thinking_raw_event_fidelity,
    test_claude_code_plan_raw_event_fidelity,
    test_claude_preset_extracts_edited_filepath,
    test_claude_preset_no_filepath_when_tool_input_missing,
    test_claude_preset_ignores_vscode_copilot_payload,
    test_claude_preset_ignores_cursor_payload,
    test_claude_preset_does_not_ignore_when_transcript_path_is_claude,
    test_claude_e2e_prefers_latest_checkpoint_for_prompts,
);
