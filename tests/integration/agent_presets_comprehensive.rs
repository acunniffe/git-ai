use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::error::GitAiError;
use git_ai::streams::agent::Agent;
use git_ai::streams::agents::{ClaudeAgent, GeminiAgent};
use git_ai::streams::watermark::ByteOffsetWatermark;
use serde_json::json;
use std::fs;

// ==============================================================================
// ClaudePreset Error Cases
// ==============================================================================

#[test]
fn test_claude_preset_invalid_json() {
    let preset = resolve_preset("claude").unwrap();
    let result = preset.parse("not valid json", "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("Invalid JSON"));
        }
        _ => panic!("Expected PresetError for invalid JSON"),
    }
}

#[test]
fn test_claude_preset_missing_transcript_path() {
    let preset = resolve_preset("claude").unwrap();
    let hook_input = json!({
        "cwd": "/some/path",
        "hook_event_name": "PostToolUse"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("transcript_path not found"));
        }
        _ => panic!("Expected PresetError for missing transcript_path"),
    }
}

#[test]
fn test_claude_preset_missing_cwd() {
    let preset = resolve_preset("claude").unwrap();
    let hook_input = json!({
        "transcript_path": "tests/fixtures/example-claude-code.jsonl",
        "hook_event_name": "PostToolUse"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("cwd not found"));
        }
        _ => panic!("Expected PresetError for missing cwd"),
    }
}

#[test]
fn test_claude_preset_pretooluse_checkpoint() {
    let preset = resolve_preset("claude").unwrap();
    let hook_input = json!({
        "cwd": "/some/path",
        "hook_event_name": "PreToolUse",
        "transcript_path": "tests/fixtures/example-claude-code.jsonl",
        "tool_input": {
            "file_path": "/some/file.rs"
        }
    })
    .to_string();

    let events = preset
        .parse(&hook_input, "t_test")
        .expect("Should succeed for PreToolUse");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(
                e.file_paths,
                vec![std::path::PathBuf::from("/some/file.rs")]
            );
        }
        _ => panic!("Expected PreFileEdit for PreToolUse"),
    }
}

#[test]
fn test_claude_preset_invalid_transcript_path() {
    let preset = resolve_preset("claude").unwrap();
    let hook_input = json!({
        "cwd": "/some/path",
        "hook_event_name": "PostToolUse",
        "transcript_path": "/nonexistent/path/to/transcript.jsonl"
    })
    .to_string();

    let events = preset.parse(&hook_input, "t_test");

    // Should succeed - parse doesn't read the transcript, it just records the path
    assert!(events.is_ok());
    let events = events.unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.stream_source.is_some());
        }
        _ => panic!("Expected PostFileEdit for PostToolUse"),
    }
}

#[test]
fn test_claude_transcript_parsing_empty_file() {
    let temp_file = std::env::temp_dir().join("empty_claude.jsonl");
    fs::write(&temp_file, "").expect("Failed to write temp file");

    let result = ClaudeAgent::new().read_incremental(
        &temp_file,
        Box::new(ByteOffsetWatermark::new(0)),
        "test",
    );

    assert!(result.is_ok());
    let batch = result.unwrap();
    assert!(batch.events.is_empty());
    // StreamBatch no longer has a model field

    fs::remove_file(temp_file).ok();
}

#[test]
fn test_claude_transcript_parsing_malformed_json() {
    let temp_file = std::env::temp_dir().join("malformed_claude.jsonl");
    fs::write(&temp_file, "{invalid json}\n").expect("Failed to write temp file");

    let result = ClaudeAgent::new().read_incremental(
        &temp_file,
        Box::new(ByteOffsetWatermark::new(0)),
        "test",
    );

    // Malformed JSON lines are skipped, not fatal errors
    let batch = result.expect("malformed lines should be skipped, not cause errors");
    assert_eq!(batch.events.len(), 0);
    fs::remove_file(temp_file).ok();
}

#[test]
fn test_claude_transcript_parsing_with_empty_lines() {
    let temp_file = std::env::temp_dir().join("empty_lines_claude.jsonl");
    let content = r#"
{"type":"user","timestamp":"2025-01-01T00:00:00Z","message":{"content":"test"}}

{"type":"assistant","timestamp":"2025-01-01T00:00:01Z","message":{"model":"claude-3","content":[{"type":"text","text":"response"}]}}
    "#;
    fs::write(&temp_file, content).expect("Failed to write temp file");

    let result = ClaudeAgent::new().read_incremental(
        &temp_file,
        Box::new(ByteOffsetWatermark::new(0)),
        "test",
    );

    assert!(result.is_ok());
    let batch = result.unwrap();
    assert_eq!(batch.events.len(), 2);
    // Model is in the raw event data, not on StreamBatch
    let model = batch
        .events
        .iter()
        .find_map(|e| e["message"]["model"].as_str());
    assert_eq!(model, Some("claude-3"));

    fs::remove_file(temp_file).ok();
}

#[test]
fn test_claude_vscode_copilot_detection() {
    let preset = resolve_preset("claude").unwrap();
    let hook_input = json!({
        "hookEventName": "PostToolUse",
        "toolName": "copilot",
        "sessionId": "test-session",
        "cwd": "/some/path",
        "transcript_path": "/Users/test/Library/Application Support/Code/User/workspaceStorage/workspace-id/GitHub.copilot-chat/transcripts/test-session.jsonl"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("Skipping VS Code hook payload in Claude preset"));
        }
        _ => panic!("Expected PresetError for VS Code Copilot payload in Claude preset"),
    }
}

#[test]
fn test_claude_cursor_detection() {
    let preset = resolve_preset("claude").unwrap();
    let hook_input = json!({
        "conversation_id": "cursor-session-1",
        "hook_event_name": "postToolUse",
        "tool_name": "Write",
        "tool_input": {
            "file_path": "/Users/test/project/src/main.ts"
        },
        "workspace_roots": ["/Users/test/project"],
        "transcript_path": "/Users/test/.cursor/projects/Users-test-project/agent-transcripts/cursor-session-1/cursor-session-1.jsonl",
        "cursor_version": "2.5.26"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("Skipping Cursor hook payload in Claude preset"));
        }
        _ => panic!("Expected PresetError for Cursor payload in Claude preset"),
    }
}

// ==============================================================================
// GeminiPreset Error Cases
// ==============================================================================

#[test]
fn test_gemini_preset_invalid_json() {
    let preset = resolve_preset("gemini").unwrap();
    let result = preset.parse("invalid{json", "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("Invalid JSON"));
        }
        _ => panic!("Expected PresetError"),
    }
}

#[test]
fn test_gemini_preset_missing_session_id() {
    let preset = resolve_preset("gemini").unwrap();
    let hook_input = json!({
        "transcript_path": "tests/fixtures/gemini-session-simple.jsonl",
        "cwd": "/path"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("session_id not found"));
        }
        _ => panic!("Expected PresetError"),
    }
}

#[test]
fn test_gemini_preset_missing_transcript_path() {
    let preset = resolve_preset("gemini").unwrap();
    let hook_input = json!({
        "session_id": "test-session",
        "cwd": "/path"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("transcript_path not found"));
        }
        _ => panic!("Expected PresetError"),
    }
}

#[test]
fn test_gemini_preset_missing_cwd() {
    let preset = resolve_preset("gemini").unwrap();
    let hook_input = json!({
        "session_id": "test-session",
        "transcript_path": "tests/fixtures/gemini-session-simple.jsonl"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("cwd not found"));
        }
        _ => panic!("Expected PresetError"),
    }
}

#[test]
fn test_gemini_preset_beforetool_checkpoint() {
    let preset = resolve_preset("gemini").unwrap();
    let hook_input = json!({
        "session_id": "test-session",
        "transcript_path": "tests/fixtures/gemini-session-simple.jsonl",
        "cwd": "/path",
        "hook_event_name": "BeforeTool",
        "tool_input": {
            "file_path": "/file.js"
        }
    })
    .to_string();

    let events = preset
        .parse(&hook_input, "t_test")
        .expect("Should succeed for BeforeTool");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.file_paths, vec![std::path::PathBuf::from("/file.js")]);
        }
        _ => panic!("Expected PreFileEdit for BeforeTool"),
    }
}

#[test]
fn test_gemini_transcript_parsing_invalid_path() {
    let result = GeminiAgent::new().read_incremental(
        std::path::Path::new("/nonexistent/path.jsonl"),
        Box::new(ByteOffsetWatermark::new(0)),
        "test",
    );

    assert!(result.is_err());
    match result {
        Err(git_ai::streams::StreamError::Fatal { .. }) => {}
        _ => panic!("Expected Fatal error for nonexistent path"),
    }
}

#[test]
fn test_gemini_transcript_parsing_empty_file() {
    let temp_file = std::env::temp_dir().join("gemini_empty.jsonl");
    fs::write(&temp_file, "").expect("Failed to write temp file");

    let result = GeminiAgent::new().read_incremental(
        &temp_file,
        Box::new(ByteOffsetWatermark::new(0)),
        "test",
    );

    assert!(result.is_ok());
    let batch = result.unwrap();
    assert!(batch.events.is_empty());

    fs::remove_file(temp_file).ok();
}

#[test]
fn test_gemini_transcript_parsing_invalid_json_line() {
    let temp_file = std::env::temp_dir().join("gemini_invalid_line.jsonl");
    fs::write(&temp_file, "this is not valid json\n").expect("Failed to write temp file");

    let result = GeminiAgent::new().read_incremental(
        &temp_file,
        Box::new(ByteOffsetWatermark::new(0)),
        "test",
    );

    // Malformed JSON lines are skipped, not fatal errors
    let batch = result.expect("malformed lines should be skipped, not cause errors");
    assert_eq!(batch.events.len(), 0);

    fs::remove_file(temp_file).ok();
}

// ==============================================================================
// ContinueCliPreset Error Cases
// ==============================================================================

#[test]
fn test_continue_preset_invalid_json() {
    let preset = resolve_preset("continue-cli").unwrap();
    let result = preset.parse("not json", "t_test");

    assert!(result.is_err());
}

#[test]
fn test_continue_preset_missing_session_id() {
    let preset = resolve_preset("continue-cli").unwrap();
    let hook_input = json!({
        "transcript_path": "tests/fixtures/continue-cli-session-simple.json",
        "cwd": "/path",
        "model": "gpt-4"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("session_id not found"));
        }
        _ => panic!("Expected PresetError"),
    }
}

#[test]
fn test_continue_preset_missing_transcript_path() {
    let preset = resolve_preset("continue-cli").unwrap();
    let hook_input = json!({
        "session_id": "test-session",
        "cwd": "/path",
        "model": "gpt-4"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("transcript_path not found"));
        }
        _ => panic!("Expected PresetError"),
    }
}

#[test]
fn test_continue_preset_missing_model_defaults_to_unknown() {
    let preset = resolve_preset("continue-cli").unwrap();
    let hook_input = json!({
        "session_id": "test-session",
        "transcript_path": "tests/fixtures/continue-cli-session-simple.json",
        "cwd": "/path"
    })
    .to_string();

    let events = preset
        .parse(&hook_input, "t_test")
        .expect("Should succeed with default model");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.model, "unknown");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_continue_preset_pretooluse_checkpoint() {
    let preset = resolve_preset("continue-cli").unwrap();
    let hook_input = json!({
        "session_id": "test-session",
        "transcript_path": "tests/fixtures/continue-cli-session-simple.json",
        "cwd": "/path",
        "model": "gpt-4",
        "hook_event_name": "PreToolUse",
        "tool_input": {
            "file_path": "/file.py"
        }
    })
    .to_string();

    let events = preset
        .parse(&hook_input, "t_test")
        .expect("Should succeed for PreToolUse");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.file_paths, vec![std::path::PathBuf::from("/file.py")]);
        }
        _ => panic!("Expected PreFileEdit for PreToolUse"),
    }
}

// ==============================================================================
// CodexPreset Error Cases
// ==============================================================================

#[test]
fn test_codex_preset_invalid_json() {
    let preset = resolve_preset("codex").unwrap();
    let result = preset.parse("{bad json", "t_test");

    assert!(result.is_err());
}

#[test]
fn test_codex_preset_missing_session_id() {
    let preset = resolve_preset("codex").unwrap();
    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "tool_name": "apply_patch",
        "transcript_path": "tests/fixtures/codex-session-simple.jsonl",
        "cwd": "/path"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("session_id") || msg.contains("thread_id"));
        }
        _ => panic!("Expected PresetError for missing session_id/thread_id"),
    }
}

#[test]
fn test_codex_preset_invalid_transcript_path() {
    let preset = resolve_preset("codex").unwrap();
    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "tool_name": "apply_patch",
        "tool_use_id": "patch-1",
        "session_id": "test-session-12345",
        "transcript_path": "/nonexistent/path/transcript.jsonl",
        "cwd": "/path"
    })
    .to_string();

    let events = preset
        .parse(&hook_input, "t_test")
        .expect("Should succeed with fallback to empty transcript");

    // parse() doesn't read the transcript, it just records the path
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.stream_source.is_some());
            assert_eq!(e.context.agent_id.model, "unknown");
            assert_eq!(e.context.agent_id.id, "test-session-12345");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

// ==============================================================================
// CursorPreset Error Cases
// ==============================================================================

#[test]
fn test_cursor_preset_invalid_json() {
    let preset = resolve_preset("cursor").unwrap();
    let result = preset.parse("invalid", "t_test");

    assert!(result.is_err());
}

#[test]
fn test_cursor_preset_missing_conversation_id() {
    let preset = resolve_preset("cursor").unwrap();
    let hook_input = json!({
        "type": "composer_turn_complete",
        "cwd": "/path"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("conversation_id not found"));
        }
        _ => panic!("Expected PresetError"),
    }
}

#[test]
fn test_cursor_preset_missing_workspace_roots() {
    let preset = resolve_preset("cursor").unwrap();
    let hook_input = json!({
        "type": "composer_turn_complete",
        "conversation_id": "test-conv",
        "hook_event_name": "afterFileEdit"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("workspace_roots not found"));
        }
        _ => panic!("Expected PresetError for missing workspace_roots"),
    }
}

// ==============================================================================
// GithubCopilotPreset Error Cases
// ==============================================================================

#[test]
fn test_github_copilot_preset_invalid_json() {
    let preset = resolve_preset("github-copilot").unwrap();
    let result = preset.parse("not json", "t_test");

    assert!(result.is_err());
}

#[test]
fn test_github_copilot_preset_invalid_hook_event_name() {
    let preset = resolve_preset("github-copilot").unwrap();
    let hook_input = json!({
        "hook_event_name": "invalid_event_name",
        "sessionId": "test-session",
        "transcriptPath": "tests/fixtures/copilot_session_simple.jsonl"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("Invalid hook_event_name"));
            assert!(msg.contains("before_edit") || msg.contains("after_edit"));
        }
        _ => panic!("Expected PresetError for invalid hook_event_name"),
    }
}

// ==============================================================================
// DroidPreset Error Cases
// ==============================================================================

#[test]
fn test_droid_preset_invalid_json() {
    let preset = resolve_preset("droid").unwrap();
    let result = preset.parse("{invalid", "t_test");

    assert!(result.is_err());
}

#[test]
fn test_droid_preset_generates_fallback_session_id() {
    let preset = resolve_preset("droid").unwrap();
    let hook_input = json!({
        "transcript_path": "tests/fixtures/droid-session.jsonl",
        "cwd": "/path",
        "hookEventName": "PostToolUse",
        "toolName": "Edit"
    })
    .to_string();

    let events = preset
        .parse(&hook_input, "t_test")
        .expect("Should succeed with generated session_id");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.context.agent_id.id.starts_with("droid-"));
            assert_eq!(e.context.agent_id.tool, "droid");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

// ==============================================================================
// AiTabPreset Error Cases
// ==============================================================================

#[test]
fn test_aitab_preset_invalid_json() {
    let preset = resolve_preset("ai_tab").unwrap();
    let result = preset.parse("bad json", "t_test");

    assert!(result.is_err());
}

#[test]
fn test_aitab_preset_invalid_hook_event_name() {
    let preset = resolve_preset("ai_tab").unwrap();
    let hook_input = json!({
        "hook_event_name": "invalid_event",
        "tool": "test_tool",
        "model": "test_model"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("Unsupported hook_event_name"));
            assert!(msg.contains("expected 'before_edit' or 'after_edit'"));
        }
        _ => panic!("Expected PresetError"),
    }
}

#[test]
fn test_aitab_preset_empty_tool() {
    let preset = resolve_preset("ai_tab").unwrap();
    let hook_input = json!({
        "hook_event_name": "after_edit",
        "tool": "  ",
        "model": "test_model"
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("tool must be a non-empty string"));
        }
        _ => panic!("Expected PresetError"),
    }
}

#[test]
fn test_aitab_preset_empty_model() {
    let preset = resolve_preset("ai_tab").unwrap();
    let hook_input = json!({
        "hook_event_name": "after_edit",
        "tool": "test_tool",
        "model": "  "
    })
    .to_string();

    let result = preset.parse(&hook_input, "t_test");

    assert!(result.is_err());
    match result {
        Err(GitAiError::PresetError(msg)) => {
            assert!(msg.contains("model must be a non-empty string"));
        }
        _ => panic!("Expected PresetError"),
    }
}

#[test]
fn test_aitab_preset_before_edit_checkpoint() {
    let preset = resolve_preset("ai_tab").unwrap();
    let hook_input = json!({
        "hook_event_name": "before_edit",
        "tool": "test_tool",
        "model": "gpt-4",
        "repo_working_dir": "/project",
        "will_edit_filepaths": ["/file1.rs", "/file2.rs"]
    })
    .to_string();

    let events = preset
        .parse(&hook_input, "t_test")
        .expect("Should succeed for before_edit");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "test_tool");
            assert_eq!(e.context.agent_id.model, "gpt-4");
            assert_eq!(
                e.file_paths,
                vec![
                    std::path::PathBuf::from("/file1.rs"),
                    std::path::PathBuf::from("/file2.rs"),
                ]
            );
        }
        _ => panic!("Expected PreFileEdit for before_edit"),
    }
}

#[test]
fn test_aitab_preset_after_edit_checkpoint() {
    let preset = resolve_preset("ai_tab").unwrap();
    let hook_input = json!({
        "hook_event_name": "after_edit",
        "tool": "test_tool",
        "model": "gpt-4",
        "repo_working_dir": "/project",
        "edited_filepaths": ["/file1.rs"]
    })
    .to_string();

    let events = preset
        .parse(&hook_input, "t_test")
        .expect("Should succeed for after_edit");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.stream_source.is_none());
            assert_eq!(e.file_paths, vec![std::path::PathBuf::from("/file1.rs")]);
        }
        _ => panic!("Expected PostFileEdit for after_edit"),
    }
}

#[test]
fn test_aitab_preset_with_dirty_files() {
    let preset = resolve_preset("ai_tab").unwrap();
    let mut dirty_files = std::collections::HashMap::new();
    dirty_files.insert("/file1.rs".to_string(), "content1".to_string());
    dirty_files.insert("/file2.rs".to_string(), "content2".to_string());

    let hook_input = json!({
        "hook_event_name": "after_edit",
        "tool": "test_tool",
        "model": "gpt-4",
        "dirty_files": dirty_files
    })
    .to_string();

    let events = preset
        .parse(&hook_input, "t_test")
        .expect("Should succeed with dirty_files");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.dirty_files.is_some());
            let dirty = e.dirty_files.as_ref().unwrap();
            assert_eq!(dirty.len(), 2);
            assert_eq!(
                dirty.get(&std::path::PathBuf::from("/file1.rs")),
                Some(&"content1".to_string())
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_aitab_preset_empty_repo_working_dir_filtered() {
    let preset = resolve_preset("ai_tab").unwrap();
    let hook_input = json!({
        "hook_event_name": "after_edit",
        "tool": "test_tool",
        "model": "gpt-4",
        "repo_working_dir": "   "
    })
    .to_string();

    let events = preset.parse(&hook_input, "t_test").expect("Should succeed");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            // Empty/whitespace-only repo_working_dir should fall back to "."
            assert_eq!(e.context.cwd, std::path::PathBuf::from("."));
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

// ==============================================================================
// Integration Tests - Cross-Preset Behavior
// ==============================================================================

#[test]
fn test_all_presets_handle_invalid_json_consistently() {
    let preset_names = vec![
        "claude",
        "gemini",
        "continue-cli",
        "codex",
        "cursor",
        "github-copilot",
        "amp",
        "droid",
        "ai_tab",
        "grok",
    ];

    for name in preset_names {
        let preset = resolve_preset(name).unwrap();
        let result = preset.parse("{invalid json}", "t_test");
        assert!(
            result.is_err(),
            "Preset '{}' should fail with invalid JSON",
            name,
        );
    }
}

// ==============================================================================
// Edge Cases - Unusual but Valid Inputs
// ==============================================================================

#[test]
fn test_claude_preset_with_tool_input_no_file_path() {
    let preset = resolve_preset("claude").unwrap();
    let hook_input = json!({
        "cwd": "/path",
        "hook_event_name": "PostToolUse",
        "transcript_path": "tests/fixtures/example-claude-code.jsonl",
        "tool_input": {
            "other_field": "value"
        }
    })
    .to_string();

    let events = preset.parse(&hook_input, "t_test").expect("Should succeed");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.file_paths.is_empty());
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_gemini_preset_with_tool_input_no_file_path() {
    let preset = resolve_preset("gemini").unwrap();
    let hook_input = json!({
        "session_id": "test",
        "transcript_path": "tests/fixtures/gemini-session-simple.jsonl",
        "cwd": "/path",
        "tool_input": {
            "other": "value"
        }
    })
    .to_string();

    let events = preset.parse(&hook_input, "t_test").expect("Should succeed");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.file_paths.is_empty());
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_continue_preset_with_tool_input_no_file_path() {
    let preset = resolve_preset("continue-cli").unwrap();
    let hook_input = json!({
        "session_id": "test",
        "transcript_path": "tests/fixtures/continue-cli-session-simple.json",
        "cwd": "/path",
        "model": "gpt-4",
        "tool_input": {}
    })
    .to_string();

    let events = preset.parse(&hook_input, "t_test").expect("Should succeed");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.file_paths.is_empty());
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_claude_preset_with_unicode_in_path() {
    let preset = resolve_preset("claude").unwrap();
    let hook_input = json!({
        "cwd": "/Users/测试/项目",
        "hook_event_name": "PostToolUse",
        "transcript_path": "tests/fixtures/example-claude-code.jsonl",
        "tool_input": {
            "file_path": "/Users/测试/项目/文件.rs"
        }
    })
    .to_string();

    let events = preset
        .parse(&hook_input, "t_test")
        .expect("Should handle unicode paths");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(!e.file_paths.is_empty());
            assert_eq!(
                e.file_paths[0],
                std::path::PathBuf::from("/Users/测试/项目/文件.rs")
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_gemini_transcript_with_unknown_message_types() {
    use std::io::Write;
    let temp_file = std::env::temp_dir().join("gemini_unknown_types.jsonl");
    let mut f = fs::File::create(&temp_file).unwrap();
    writeln!(f, r#"{{"type":"user","content":"test"}}"#).unwrap();
    writeln!(
        f,
        r#"{{"type":"unknown_type","content":"should still be included"}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"info","content":"should also be included"}}"#
    )
    .unwrap();
    writeln!(f, r#"{{"type":"gemini","content":"response"}}"#).unwrap();

    let batch = GeminiAgent::new()
        .read_incremental(&temp_file, Box::new(ByteOffsetWatermark::new(0)), "test")
        .expect("Should parse successfully");

    assert_eq!(batch.events.len(), 4);

    fs::remove_file(temp_file).ok();
}

#[test]
fn test_claude_transcript_with_tool_result_in_user_content() {
    let temp_file = std::env::temp_dir().join("claude_tool_result.jsonl");
    let content = r#"{"type":"user","timestamp":"2025-01-01T00:00:00Z","message":{"content":[{"type":"tool_result","content":"should be skipped"},{"type":"text","text":"actual user input"}]}}
{"type":"assistant","timestamp":"2025-01-01T00:00:01Z","message":{"model":"claude-3","content":[{"type":"text","text":"response"}]}}"#;
    fs::write(&temp_file, content).expect("Failed to write temp file");

    let batch = ClaudeAgent::new()
        .read_incremental(&temp_file, Box::new(ByteOffsetWatermark::new(0)), "test")
        .expect("Should parse successfully");

    // Events are raw JSONL entries. The user entry is a single event.
    let user_events: Vec<_> = batch
        .events
        .iter()
        .filter(|e| e["type"] == "user")
        .collect();
    assert_eq!(user_events.len(), 1);

    fs::remove_file(temp_file).ok();
}

#[test]
fn test_gemini_transcript_with_empty_tool_calls() {
    use std::io::Write;
    let temp_file = std::env::temp_dir().join("gemini_empty_tools.jsonl");
    let mut f = fs::File::create(&temp_file).unwrap();
    writeln!(f, r#"{{"type":"gemini","content":"test","toolCalls":[]}}"#).unwrap();

    let batch = GeminiAgent::new()
        .read_incremental(&temp_file, Box::new(ByteOffsetWatermark::new(0)), "test")
        .expect("Should parse successfully");

    assert_eq!(batch.events.len(), 1);

    fs::remove_file(temp_file).ok();
}

#[test]
fn test_gemini_transcript_tool_call_without_args() {
    use std::io::Write;
    let temp_file = std::env::temp_dir().join("gemini_tool_no_args.jsonl");
    let mut f = fs::File::create(&temp_file).unwrap();
    writeln!(
        f,
        r#"{{"type":"gemini","toolCalls":[{{"name":"read_file"}}]}}"#
    )
    .unwrap();

    let batch = GeminiAgent::new()
        .read_incremental(&temp_file, Box::new(ByteOffsetWatermark::new(0)), "test")
        .expect("Should parse successfully");

    let tool_messages: Vec<_> = batch
        .events
        .iter()
        .filter(|e| e["toolCalls"].as_array().is_some_and(|a| !a.is_empty()))
        .collect();
    assert_eq!(tool_messages.len(), 1);

    fs::remove_file(temp_file).ok();
}
