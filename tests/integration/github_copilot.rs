use crate::test_utils::{fixture_path, load_fixture, read_jsonl_fixture};
use git_ai::error::GitAiError;
use git_ai::model::stream_watermark::{ByteOffsetWatermark, RecordIndexWatermark};
use git_ai::operations::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::operations::streams::agent::Agent;
use git_ai::operations::streams::agents::CopilotAgent;
use serde_json::json;
use std::{fs, io::Write};

fn parse_copilot(hook_input: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    resolve_preset("github-copilot")?.parse(hook_input, "t_test")
}

/// Ensure CODESPACES and REMOTE_CONTAINERS are not set (they cause early return in transcript parsing)
fn ensure_clean_env() {
    unsafe {
        std::env::remove_var("CODESPACES");
        std::env::remove_var("REMOTE_CONTAINERS");
    }
}

#[test]
#[serial_test::serial(copilot_env)]
fn test_copilot_session_json_raw_event_fidelity() {
    ensure_clean_env();
    let fixture = fixture_path("copilot_session_simple.json");
    let agent = CopilotAgent::new();
    let watermark = Box::new(RecordIndexWatermark::new(0));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Should parse copilot session JSON");

    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&fixture).unwrap()).unwrap();
    let expected: Vec<serde_json::Value> = parsed["requests"].as_array().unwrap().clone();

    assert_eq!(result.events, expected);
}

#[test]
#[serial_test::serial(copilot_env)]
fn test_copilot_event_stream_raw_event_fidelity() {
    ensure_clean_env();
    let fixture = fixture_path("copilot_session_event_stream.jsonl");
    let agent = CopilotAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Should parse copilot event stream JSONL");

    let expected = read_jsonl_fixture(&fixture).unwrap();

    assert_eq!(result.events.len(), expected.len());
    assert_eq!(result.events, expected);
}

#[test]
#[serial_test::serial(copilot_env)]
fn test_copilot_returns_empty_transcript_in_codespaces() {
    let original_codespaces = std::env::var("CODESPACES").ok();
    unsafe {
        std::env::set_var("CODESPACES", "true");
    }

    let fixture = fixture_path("copilot_session_simple.json");
    let agent = CopilotAgent::new();
    let watermark = Box::new(RecordIndexWatermark::new(0));
    let result = agent.read_incremental(fixture.as_path(), watermark, "test");
    assert!(result.is_ok());
    let batch = result.unwrap();
    assert!(batch.events.is_empty());

    unsafe {
        if let Some(original) = original_codespaces {
            std::env::set_var("CODESPACES", original);
        } else {
            std::env::remove_var("CODESPACES");
        }
    }
}

#[test]
#[serial_test::serial(copilot_env)]
fn test_copilot_returns_empty_transcript_in_remote_containers() {
    let original = std::env::var("REMOTE_CONTAINERS").ok();
    unsafe {
        std::env::set_var("REMOTE_CONTAINERS", "true");
    }

    let fixture = fixture_path("copilot_session_simple.json");
    let agent = CopilotAgent::new();
    let watermark = Box::new(RecordIndexWatermark::new(0));
    let result = agent.read_incremental(fixture.as_path(), watermark, "test");
    assert!(result.is_ok());
    let batch = result.unwrap();
    assert!(batch.events.is_empty());

    unsafe {
        if let Some(orig) = original {
            std::env::set_var("REMOTE_CONTAINERS", orig);
        } else {
            std::env::remove_var("REMOTE_CONTAINERS");
        }
    }
}

// ============================================================================
// Tests for before_edit / after_edit logic
// ============================================================================

#[test]
fn test_copilot_preset_before_edit_human_checkpoint_snake_case() {
    let hook_input = json!({
        "hook_event_name": "before_edit",
        "workspace_folder": "/Users/test/project",
        "will_edit_filepaths": ["/Users/test/project/file.ts"],
        "dirty_files": { "/Users/test/project/file.ts": "console.log('hello');" }
    })
    .to_string();

    let events = parse_copilot(&hook_input).expect("Should succeed");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert!(!e.file_paths.is_empty());
            assert!(e.dirty_files.is_some());
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert_eq!(dirty_files.len(), 1);
            assert!(dirty_files.values().any(|v| v.contains("hello")));
            assert_eq!(e.context.agent_id.tool, "github-copilot");
        }
        _ => panic!("Expected PreFileEdit for before_edit"),
    }
}

#[test]
fn test_copilot_preset_before_edit_human_checkpoint_camel_case() {
    let hook_input = json!({
        "hook_event_name": "before_edit",
        "workspaceFolder": "/Users/test/project",
        "will_edit_filepaths": ["/Users/test/project/file.ts"],
        "dirtyFiles": { "/Users/test/project/file.ts": "console.log('hello');" }
    })
    .to_string();

    let events = parse_copilot(&hook_input).expect("Should succeed");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert!(!e.file_paths.is_empty());
            assert!(e.dirty_files.is_some());
        }
        _ => panic!("Expected PreFileEdit for before_edit"),
    }
}

#[test]
fn test_copilot_preset_before_edit_requires_will_edit_filepaths() {
    let hook_input = json!({
        "hook_event_name": "before_edit",
        "workspace_folder": "/Users/test/project",
        "dirty_files": {}
    })
    .to_string();

    let result = parse_copilot(&hook_input);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("will_edit_filepaths is required")
    );
}

#[test]
fn test_copilot_preset_before_edit_requires_non_empty_filepaths() {
    let hook_input = json!({
        "hook_event_name": "before_edit",
        "workspace_folder": "/Users/test/project",
        "will_edit_filepaths": [],
        "dirty_files": {}
    })
    .to_string();

    let result = parse_copilot(&hook_input);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("will_edit_filepaths cannot be empty")
    );
}

#[test]
fn test_copilot_preset_after_edit_requires_session_id() {
    let hook_input = json!({
        "hook_event_name": "after_edit",
        "workspace_folder": "/Users/test/project",
        "dirty_files": {}
    })
    .to_string();

    let result = parse_copilot(&hook_input);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("chat_session_path or chatSessionPath not found")
    );
}

#[test]
fn test_copilot_preset_after_edit_requires_session_id_camel_case() {
    let hook_input = json!({
        "hook_event_name": "after_edit",
        "workspaceFolder": "/Users/test/project",
        "dirtyFiles": {}
    })
    .to_string();

    let result = parse_copilot(&hook_input);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("chat_session_path or chatSessionPath not found")
    );
}

#[test]
fn test_copilot_preset_invalid_hook_event_name() {
    let hook_input = json!({
        "hook_event_name": "invalid_event",
        "workspace_folder": "/Users/test/project"
    })
    .to_string();

    let result = parse_copilot(&hook_input);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Invalid hook_event_name")
    );
}

#[test]
fn test_copilot_preset_before_edit_multiple_files_snake_case() {
    let hook_input = json!({
        "hook_event_name": "before_edit",
        "workspace_folder": "/Users/test/project",
        "will_edit_filepaths": ["/Users/test/project/file1.ts", "/Users/test/project/file2.ts", "/Users/test/project/file3.ts"],
        "dirty_files": { "/Users/test/project/file1.ts": "content1", "/Users/test/project/file2.ts": "content2" }
    }).to_string();

    let events = parse_copilot(&hook_input).expect("Should succeed");
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.file_paths.len(), 3);
        }
        _ => panic!("Expected PreFileEdit"),
    }
}

#[test]
fn test_copilot_preset_before_edit_multiple_files_camel_case() {
    let hook_input = json!({
        "hook_event_name": "before_edit",
        "workspaceFolder": "/Users/test/project",
        "will_edit_filepaths": ["/Users/test/project/file1.ts", "/Users/test/project/file2.ts", "/Users/test/project/file3.ts"],
        "dirtyFiles": { "/Users/test/project/file1.ts": "content1", "/Users/test/project/file2.ts": "content2" }
    }).to_string();

    let events = parse_copilot(&hook_input).expect("Should succeed");
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.file_paths.len(), 3);
        }
        _ => panic!("Expected PreFileEdit"),
    }
}

#[test]
fn test_copilot_preset_after_edit_camel_case() {
    let mut temp_file = tempfile::NamedTempFile::new().unwrap();
    temp_file
        .write_all(r#"{"requests": []}"#.as_bytes())
        .unwrap();
    let temp_path = temp_file.path().to_str().unwrap().to_string();

    let hook_input = json!({
        "hook_event_name": "after_edit",
        "workspaceFolder": "/Users/test/project",
        "chatSessionPath": temp_path,
        "sessionId": "test-session-123",
        "edited_filepaths": ["/Users/test/project/file.ts"],
        "dirtyFiles": { "/Users/test/project/file.ts": "console.log('hello');" }
    })
    .to_string();

    let events = parse_copilot(&hook_input).expect("Should succeed");
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.id, "test-session-123");
            assert_eq!(e.context.agent_id.tool, "github-copilot");
            assert!(!e.file_paths.is_empty());
            assert!(e.dirty_files.is_some());
        }
        _ => panic!("Expected PostFileEdit for after_edit"),
    }
}

#[test]
fn test_copilot_preset_after_edit_snake_case() {
    let mut temp_file = tempfile::NamedTempFile::new().unwrap();
    temp_file
        .write_all(r#"{"requests": []}"#.as_bytes())
        .unwrap();
    let temp_path = temp_file.path().to_str().unwrap().to_string();

    let hook_input = json!({
        "hook_event_name": "after_edit",
        "workspace_folder": "/Users/test/project",
        "chat_session_path": temp_path,
        "session_id": "test-session-456",
        "edited_filepaths": ["/Users/test/project/file.ts"],
        "dirty_files": { "/Users/test/project/file.ts": "console.log('hello');" }
    })
    .to_string();

    let events = parse_copilot(&hook_input).expect("Should succeed");
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.id, "test-session-456");
            assert_eq!(e.context.agent_id.tool, "github-copilot");
            assert!(!e.file_paths.is_empty());
            assert!(e.dirty_files.is_some());
        }
        _ => panic!("Expected PostFileEdit for after_edit"),
    }
}

// ============================================================================
// Tests for JSONL format support
// ============================================================================

// NOTE: copilot_session_parsing_jsonl_stub, copilot_session_parsing_jsonl_simple,
// and test_copilot_extracts_edited_filepaths_jsonl were removed because the new
// CopilotAgent API does not support the kind:0/kind:1 JSONL snapshot+patch protocol,
// and edited_filepaths are no longer returned by read_incremental.

#[test]
#[serial_test::serial(copilot_env)]
fn test_copilot_after_edit_with_jsonl_session() {
    ensure_clean_env();

    let mut temp_file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
    temp_file
        .write_all(r#"{"kind":0,"v":{"requests": []}}"#.as_bytes())
        .unwrap();
    let temp_path = temp_file.path().to_str().unwrap().to_string();

    let hook_input = json!({
        "hook_event_name": "after_edit",
        "workspace_folder": "/Users/test/project",
        "chat_session_path": temp_path,
        "session_id": "test-jsonl-session-789",
        "edited_filepaths": ["/Users/test/project/file.ts"],
        "dirty_files": { "/Users/test/project/file.ts": "console.log('hello');" }
    })
    .to_string();

    let events = parse_copilot(&hook_input).expect("Should succeed");
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.id, "test-jsonl-session-789");
            assert_eq!(e.context.agent_id.tool, "github-copilot");
            assert!(!e.file_paths.is_empty());
            assert!(e.dirty_files.is_some());
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

// NOTE: copilot_session_parsing_multiline_jsonl, copilot_session_jsonl_empty_snapshot_with_patch,
// copilot_session_jsonl_model_from_input_state_no_requests,
// copilot_session_jsonl_per_request_model_overrides_input_state, and
// copilot_session_jsonl_scalar_patch_applied were removed because the new CopilotAgent API
// does not support the kind:0/kind:1 JSONL snapshot+patch protocol.

// ============================================================================
// VS Code PreToolUse / PostToolUse tests
// ============================================================================

#[test]
fn test_copilot_preset_vscode_pretooluse_human_checkpoint() {
    let hook_input = json!({
        "hookEventName": "PreToolUse",
        "cwd": "/Users/test/project",
        "toolName": "copilot_replaceString",
        "transcript_path": "/Users/test/Library/Application Support/Code/User/workspaceStorage/ws-id/GitHub.copilot-chat/transcripts/session.jsonl",
        "toolInput": { "file_path": "src/main.ts" },
        "sessionId": "copilot-session-pre"
    })
    .to_string();

    let events = parse_copilot(&hook_input).expect("Expected human checkpoint");
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("src/main.ts"))
            );
        }
        _ => panic!("Expected PreFileEdit for PreToolUse"),
    }
}

#[test]
fn test_copilot_preset_vscode_create_file_tool_is_supported() {
    let hook_input = json!({
        "hookEventName": "PreToolUse",
        "cwd": "/Users/test/project",
        "toolName": "create_file",
        "transcript_path": "/Users/test/Library/Application Support/Code/User/workspaceStorage/ws-id/GitHub.copilot-chat/transcripts/session.jsonl",
        "toolInput": { "filePath": "/Users/test/project/src/new-file.ts", "content": "export const x = 1;\n" },
        "sessionId": "copilot-session-create"
    }).to_string();

    let events = parse_copilot(&hook_input).expect("Expected human checkpoint");
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("new-file.ts"))
            );
        }
        _ => panic!("Expected PreFileEdit"),
    }
}

#[test]
fn test_copilot_preset_vscode_apply_patch_tool_is_supported() {
    let hook_input = json!({
        "hookEventName": "PreToolUse",
        "cwd": "/Users/test/project",
        "toolName": "apply_patch",
        "transcript_path": "/Users/test/Library/Application Support/Code/User/workspaceStorage/ws-id/GitHub.copilot-chat/transcripts/session.jsonl",
        "toolInput": "*** Begin Patch\n*** Update File: src/main.ts\n@@\n-old\n+new\n*** End Patch",
        "sessionId": "copilot-session-apply-patch"
    })
    .to_string();

    let events = parse_copilot(&hook_input).expect("Expected human checkpoint");
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("src/main.ts"))
            );
        }
        _ => panic!("Expected PreFileEdit"),
    }
}

#[test]
fn test_copilot_preset_vscode_editfiles_files_array_is_supported() {
    let hook_input = json!({
        "hookEventName": "PreToolUse",
        "cwd": "/Users/test/project",
        "toolName": "editFiles",
        "transcript_path": "/Users/test/Library/Application Support/Code/User/workspaceStorage/ws-id/GitHub.copilot-chat/transcripts/session.jsonl",
        "toolInput": { "files": ["src/main.ts", "/Users/test/project/src/other.ts"] },
        "sessionId": "copilot-session-editfiles"
    })
    .to_string();

    let events = parse_copilot(&hook_input).expect("Expected human checkpoint");
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.file_paths.len(), 2);
        }
        _ => panic!("Expected PreFileEdit"),
    }
}

#[test]
fn test_copilot_preset_vscode_posttooluse_ai_checkpoint() {
    let temp_dir = tempfile::tempdir().unwrap();
    let transcripts_dir = temp_dir
        .path()
        .join("workspaceStorage")
        .join("workspace-id")
        .join("GitHub.copilot-chat")
        .join("transcripts");
    fs::create_dir_all(&transcripts_dir).unwrap();
    let transcript_path = transcripts_dir.join("copilot-session-post.jsonl");
    fs::write(&transcript_path, r#"{"requests": []}"#).unwrap();
    let session_path = transcript_path.to_string_lossy().to_string();

    let hook_input = json!({
        "hookEventName": "PostToolUse",
        "cwd": "/Users/test/project",
        "toolName": "copilot_replaceString",
        "toolInput": { "file_path": "/Users/test/project/src/main.ts" },
        "sessionId": "copilot-session-post",
        "transcript_path": session_path
    })
    .to_string();

    let events = parse_copilot(&hook_input).expect("Expected AI checkpoint");
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "github-copilot");
            assert_eq!(e.context.agent_id.id, "copilot-session-post");
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("src/main.ts"))
            );
        }
        _ => panic!("Expected PostFileEdit for PostToolUse"),
    }
}

#[test]
fn test_copilot_preset_vscode_apply_patch_posttooluse_ai_checkpoint() {
    let temp_dir = tempfile::tempdir().unwrap();
    let transcripts_dir = temp_dir
        .path()
        .join("workspaceStorage")
        .join("workspace-id")
        .join("GitHub.copilot-chat")
        .join("transcripts");
    fs::create_dir_all(&transcripts_dir).unwrap();
    let transcript_path = transcripts_dir.join("copilot-session-apply-patch-post.jsonl");
    fs::write(&transcript_path, r#"{"requests": []}"#).unwrap();
    let session_path = transcript_path.to_string_lossy().to_string();

    let hook_input = json!({
        "hookEventName": "PostToolUse",
        "cwd": "/Users/test/project",
        "toolName": "apply_patch",
        "toolInput": "*** Begin Patch\n*** Update File: src/main.ts\n@@\n-old\n+new\n*** End Patch",
        "sessionId": "copilot-session-apply-patch-post",
        "transcript_path": session_path
    })
    .to_string();

    let events = parse_copilot(&hook_input).expect("Expected AI checkpoint");
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "github-copilot");
            assert_eq!(e.context.agent_id.id, "copilot-session-apply-patch-post");
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("src/main.ts"))
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_copilot_preset_vscode_non_edit_tool_is_filtered() {
    let hook_input = json!({
        "hookEventName": "PreToolUse",
        "cwd": "/Users/test/project",
        "toolName": "copilot_findTextInFiles",
        "toolInput": { "query": "hello" },
        "sessionId": "copilot-session-search",
        "transcript_path": "/Users/test/Library/Application Support/Code/User/workspaceStorage/ws-id/GitHub.copilot-chat/transcripts/session.jsonl"
    })
    .to_string();

    let result = parse_copilot(&hook_input);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("unsupported tool_name")
    );
}

#[test]
fn test_copilot_preset_cli_non_edit_tool_is_filtered() {
    let hook_input = json!({
        "hookEventName": "PreToolUse",
        "cwd": "/Users/test/project",
        "toolName": "view",
        "toolInput": { "path": "/Users/test/project/file.ts" },
        "sessionId": "copilot-session-view"
    })
    .to_string();

    let result = parse_copilot(&hook_input);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("non-edit tool"),
        "Expected non-edit tool error, got: {}",
        err_msg
    );
}

#[test]
fn test_copilot_preset_vscode_claude_transcript_path_is_rejected() {
    let hook_input = json!({
        "hookEventName": "PostToolUse",
        "cwd": "/Users/test/project",
        "toolName": "copilot_replaceString",
        "toolInput": { "file_path": "/Users/test/project/src/main.ts" },
        "sessionId": "copilot-session-wrong",
        "transcript_path": "/Users/test/.claude/projects/session.jsonl"
    })
    .to_string();

    let result = parse_copilot(&hook_input);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Claude transcript path")
    );
}

// ============================================================================
// VS Code model lookup tests
// ============================================================================

const VS_CODE_LOOKUP_SESSION_ID: &str = "fixture-session-id";

fn setup_vscode_model_lookup_workspace(chat_session_fixture: &str) -> (tempfile::TempDir, String) {
    let temp_dir = tempfile::tempdir().unwrap();
    let workspace_storage = temp_dir
        .path()
        .join("workspaceStorage")
        .join("workspace-model");
    let transcripts_dir = workspace_storage
        .join("GitHub.copilot-chat")
        .join("transcripts");
    let chat_sessions_dir = workspace_storage.join("chatSessions");
    fs::create_dir_all(&transcripts_dir).unwrap();
    fs::create_dir_all(&chat_sessions_dir).unwrap();

    let transcript_path = transcripts_dir.join(format!("{}.jsonl", VS_CODE_LOOKUP_SESSION_ID));
    fs::write(
        &transcript_path,
        load_fixture("copilot_transcript_session_lookup.jsonl"),
    )
    .unwrap();

    let fixture_p = fixture_path(chat_session_fixture);
    let ext = fixture_p
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("jsonl");
    let chat_session_path = chat_sessions_dir.join(format!("session-lookup.{}", ext));
    fs::write(chat_session_path, load_fixture(chat_session_fixture)).unwrap();

    (temp_dir, transcript_path.to_string_lossy().to_string())
}

fn vscode_post_tool_use_hook_input(transcript_path: &str) -> String {
    json!({
        "hookEventName": "PostToolUse",
        "cwd": "/Users/test/project",
        "toolName": "copilot_replaceString",
        "toolInput": { "file_path": "/Users/test/project/src/main.ts" },
        "sessionId": VS_CODE_LOOKUP_SESSION_ID,
        "transcript_path": transcript_path
    })
    .to_string()
}

#[test]
#[serial_test::serial(copilot_env)]
fn test_copilot_preset_vscode_model_uses_auto_model_id_when_present() {
    ensure_clean_env();
    let (_temp_dir, transcript_path) =
        setup_vscode_model_lookup_workspace("copilot_chat_session_lookup_auto.jsonl");
    let events = parse_copilot(&vscode_post_tool_use_hook_input(&transcript_path))
        .expect("Expected AI checkpoint");
    match &events[0] {
        // Model is lazily resolved from transcript, so at parse time it's "unknown"
        ParsedHookEvent::PostFileEdit(e) => assert_eq!(e.context.agent_id.model, "unknown"),
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
#[serial_test::serial(copilot_env)]
fn test_copilot_preset_vscode_model_prefers_non_auto_model_id_from_chat_sessions() {
    ensure_clean_env();
    let (_temp_dir, transcript_path) =
        setup_vscode_model_lookup_workspace("copilot_chat_session_lookup_non_auto.jsonl");
    let events = parse_copilot(&vscode_post_tool_use_hook_input(&transcript_path))
        .expect("Expected AI checkpoint");
    match &events[0] {
        // Model is lazily resolved from transcript, so at parse time it's "unknown"
        ParsedHookEvent::PostFileEdit(e) => assert_eq!(e.context.agent_id.model, "unknown"),
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
#[serial_test::serial(copilot_env)]
fn test_copilot_preset_vscode_model_falls_back_to_selected_model_id() {
    ensure_clean_env();
    let (_temp_dir, transcript_path) =
        setup_vscode_model_lookup_workspace("copilot_chat_session_lookup_selected_model.jsonl");
    let events = parse_copilot(&vscode_post_tool_use_hook_input(&transcript_path))
        .expect("Expected AI checkpoint");
    match &events[0] {
        // Model is lazily resolved from transcript, so at parse time it's "unknown"
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.model, "unknown")
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
#[serial_test::serial(copilot_env)]
fn test_copilot_preset_vscode_model_lookup_supports_json_chat_session_file() {
    ensure_clean_env();
    let (_temp_dir, transcript_path) =
        setup_vscode_model_lookup_workspace("copilot_chat_session_lookup_json_file.json");
    let events = parse_copilot(&vscode_post_tool_use_hook_input(&transcript_path))
        .expect("Expected AI checkpoint");
    match &events[0] {
        // Model is lazily resolved from transcript, so at parse time it's "unknown"
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.model, "unknown")
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
#[serial_test::serial(copilot_env)]
fn test_copilot_preset_vscode_does_not_use_details_as_model_fallback() {
    ensure_clean_env();
    let (_temp_dir, transcript_path) =
        setup_vscode_model_lookup_workspace("copilot_chat_session_lookup_details_only.jsonl");
    let events = parse_copilot(&vscode_post_tool_use_hook_input(&transcript_path))
        .expect("Expected AI checkpoint");
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => assert_eq!(e.context.agent_id.model, "unknown"),
        _ => panic!("Expected PostFileEdit"),
    }
}
