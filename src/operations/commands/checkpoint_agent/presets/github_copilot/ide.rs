use super::super::parse;
use super::super::{
    ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit, PresetContext,
    StreamFormat, StreamSource,
};
use crate::error::GitAiError;
use crate::model::authorship_log_serialization::generate_session_id;
use crate::model::working_log::AgentId;
use crate::operations::commands::checkpoint_agent::bash_tool::ToolClass;
use std::collections::HashMap;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Legacy extension path (before_edit / after_edit)
// ---------------------------------------------------------------------------

pub(super) fn parse_legacy_extension_hooks(
    data: &serde_json::Value,
    hook_event_name: &str,
    trace_id: &str,
) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    let cwd = parse::optional_str_multi(data, &["workspace_folder", "workspaceFolder"])
        .ok_or_else(|| {
            GitAiError::PresetError(
                "workspace_folder or workspaceFolder not found in hook_input for GitHub Copilot preset".to_string(),
            )
        })?;

    let dirty_files = super::dirty_files_from_hook_data(data, cwd);

    let session_id = super::extract_session_id(data);

    if hook_event_name == "before_edit" {
        let will_edit_filepaths = data
            .get("will_edit_filepaths")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| parse::resolve_absolute(s, cwd))
                    .collect::<Vec<PathBuf>>()
            })
            .ok_or_else(|| {
                GitAiError::PresetError(
                    "will_edit_filepaths is required for before_edit hook_event_name".to_string(),
                )
            })?;

        if will_edit_filepaths.is_empty() {
            return Err(GitAiError::PresetError(
                "will_edit_filepaths cannot be empty for before_edit hook_event_name".to_string(),
            ));
        }

        let context = PresetContext {
            agent_id: AgentId {
                tool: "github-copilot".to_string(),
                id: session_id.clone(),
                model: "unknown".to_string(),
            },
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
            metadata: HashMap::new(),
        };

        return Ok(vec![ParsedHookEvent::PreFileEdit(PreFileEdit {
            context,
            file_paths: will_edit_filepaths,
            dirty_files,
            tool_use_id: None,
        })]);
    }

    // after_edit path
    let chat_session_path =
        parse::optional_str_multi(data, &["chat_session_path", "chatSessionPath"]).ok_or_else(
            || {
                GitAiError::PresetError(
                    "chat_session_path or chatSessionPath not found in hook_input for after_edit"
                        .to_string(),
                )
            },
        )?;

    let edited_filepaths = data
        .get("edited_filepaths")
        .and_then(|val| val.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| parse::resolve_absolute(s, cwd))
                .collect::<Vec<PathBuf>>()
        })
        .unwrap_or_default();

    let mut metadata = HashMap::new();
    metadata.insert(
        "chat_session_path".to_string(),
        chat_session_path.to_string(),
    );

    let context = PresetContext {
        agent_id: AgentId {
            tool: "github-copilot".to_string(),
            id: session_id.clone(),
            model: "unknown".to_string(),
        },
        external_session_id: session_id,
        trace_id: trace_id.to_string(),
        cwd: PathBuf::from(cwd),
        metadata,
    };

    let stream_source = Some(StreamSource {
        path: PathBuf::from(chat_session_path),
        format: StreamFormat::CopilotSessionJson,
        session_id: generate_session_id(&context.external_session_id, "github-copilot"),
        external_session_id: context.external_session_id.clone(),
        external_parent_session_id: None,
    });

    Ok(vec![ParsedHookEvent::PostFileEdit(PostFileEdit {
        context,
        file_paths: edited_filepaths,
        dirty_files,
        stream_source,
        tool_use_id: None,
    })])
}

// ---------------------------------------------------------------------------
// VS Code native path (PreToolUse / PostToolUse)
// ---------------------------------------------------------------------------

pub(super) fn parse_vscode_native_hooks(
    data: &serde_json::Value,
    hook_event_name: &str,
    trace_id: &str,
) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    let cwd = parse::optional_str_multi(data, &["cwd", "workspace_folder", "workspaceFolder"])
        .ok_or_else(|| GitAiError::PresetError("cwd not found in hook_input".to_string()))?;

    let dirty_files = super::dirty_files_from_hook_data(data, cwd);

    let session_id = super::extract_session_id(data);

    let tool_name =
        parse::optional_str_multi(data, &["tool_name", "toolName"]).unwrap_or("unknown");

    // Enforce tool filtering to avoid creating checkpoints for read/search tools
    if !is_supported_vscode_edit_tool_name(tool_name) {
        return Err(GitAiError::PresetError(format!(
            "Skipping VS Code hook for unsupported tool_name '{}' (non-edit tool).",
            tool_name
        )));
    }

    let tool_input = data.get("tool_input").or_else(|| data.get("toolInput"));
    let tool_response = data
        .get("tool_response")
        .or_else(|| data.get("toolResponse"));

    // Extract file paths from tool_input and tool_response only (not session-level data)
    let extracted_paths =
        super::extract_filepaths_from_vscode_hook_payload(tool_input, tool_response, cwd);

    let transcript_path = transcript_path_from_hook_data(data).map(|s| s.to_string());

    if let Some(ref path) = transcript_path
        && looks_like_claude_transcript_path(path)
    {
        return Err(GitAiError::PresetError(
            "Skipping VS Code hook because transcript_path looks like a Claude transcript path."
                .to_string(),
        ));
    }

    if !is_likely_copilot_native_hook(transcript_path.as_deref()) {
        return Err(GitAiError::PresetError(format!(
            "Skipping VS Code hook for non-Copilot session (tool_name: {}).",
            tool_name,
        )));
    }

    let tool_class = classify_copilot_tool(tool_name);
    let is_bash = tool_class == ToolClass::Bash;
    let bash_command = parse::bash_command_from_hook_input(data);

    let tool_use_id = parse::optional_str_multi(data, &["tool_use_id", "toolUseId"])
        .unwrap_or("unknown")
        .to_string();

    let mut metadata = HashMap::new();
    if let Some(ref path) = transcript_path {
        metadata.insert("transcript_path".to_string(), path.clone());
        metadata.insert("chat_session_path".to_string(), path.clone());
    }

    // Determine transcript format: newer native uses EventStreamJsonl
    let transcript_format = super::transcript_format(transcript_path.as_deref().unwrap());

    let context = PresetContext {
        agent_id: AgentId {
            tool: "github-copilot".to_string(),
            id: session_id.clone(),
            model: "unknown".to_string(),
        },
        external_session_id: session_id,
        trace_id: trace_id.to_string(),
        cwd: PathBuf::from(cwd),
        metadata,
    };

    let stream_source = transcript_path.map(|tp| StreamSource {
        path: PathBuf::from(tp),
        format: transcript_format,
        session_id: generate_session_id(&context.external_session_id, "github-copilot"),
        external_session_id: context.external_session_id.clone(),
        external_parent_session_id: None,
    });

    if hook_event_name == "PreToolUse" {
        if is_bash {
            return Ok(vec![ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id,
                command: bash_command,
            })]);
        }

        if tool_name.eq_ignore_ascii_case("create_file") {
            if extracted_paths.is_empty() {
                return Err(GitAiError::PresetError(
                    "No file path found in create_file PreToolUse tool_input".to_string(),
                ));
            }

            let mut empty_dirty_files: HashMap<PathBuf, String> = HashMap::new();
            for path in &extracted_paths {
                empty_dirty_files.insert(path.clone(), String::new());
            }
            return Ok(vec![ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths: extracted_paths,
                dirty_files: Some(empty_dirty_files),
                tool_use_id: Some(tool_use_id),
            })]);
        }

        if extracted_paths.is_empty() {
            return Err(GitAiError::PresetError(format!(
                "No editable file paths found in VS Code hook input (tool_name: {}). Skipping checkpoint.",
                tool_name
            )));
        }

        return Ok(vec![ParsedHookEvent::PreFileEdit(PreFileEdit {
            context,
            file_paths: extracted_paths,
            dirty_files,
            tool_use_id: Some(tool_use_id),
        })]);
    }

    // PostToolUse
    if is_bash {
        return Ok(vec![ParsedHookEvent::PostBashCall(PostBashCall {
            context,
            tool_use_id,
            command: bash_command,
            stream_source,
        })]);
    }

    if extracted_paths.is_empty() {
        return Err(GitAiError::PresetError(format!(
            "No editable file paths found in VS Code PostToolUse hook input (tool_name: {}). Skipping checkpoint.",
            tool_name
        )));
    }

    Ok(vec![ParsedHookEvent::PostFileEdit(PostFileEdit {
        context,
        file_paths: extracted_paths,
        dirty_files,
        stream_source,
        tool_use_id: Some(tool_use_id),
    })])
}

// ---------------------------------------------------------------------------
// IDE-specific helpers
// ---------------------------------------------------------------------------

pub(super) fn transcript_path_from_hook_data(data: &serde_json::Value) -> Option<&str> {
    parse::optional_str_multi(
        data,
        &[
            "transcript_path",
            "transcriptPath",
            "chat_session_path",
            "chatSessionPath",
        ],
    )
}

fn looks_like_claude_transcript_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    normalized.contains("/.claude/") || normalized.contains("/claude/projects/")
}

fn looks_like_copilot_transcript_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    normalized.contains("/github.copilot-chat/transcripts/")
        || normalized.contains("vscode-chat-session")
        || normalized.contains("copilot_session")
        || (normalized.contains("/workspacestorage/") && normalized.contains("/chatsessions/"))
}

fn is_likely_copilot_native_hook(transcript_path: Option<&str>) -> bool {
    let Some(path) = transcript_path else {
        return false;
    };
    if looks_like_claude_transcript_path(path) {
        return false;
    }
    looks_like_copilot_transcript_path(path)
}

fn is_supported_vscode_edit_tool_name(tool_name: &str) -> bool {
    let lower = tool_name.to_ascii_lowercase();

    // Explicit bash/terminal tools
    let bash_tools = ["run_in_terminal"];
    if bash_tools.iter().any(|name| lower == *name) {
        return true;
    }

    let non_edit_keywords = [
        "find", "search", "read", "grep", "glob", "list", "ls", "fetch", "web", "open", "todo",
    ];
    if non_edit_keywords.iter().any(|kw| lower.contains(kw)) {
        return false;
    }

    let exact_edit_tools = [
        "write",
        "edit",
        "multiedit",
        "applypatch",
        "apply_patch",
        "copilot_insertedit",
        "copilot_replacestring",
        "vscode_editfile_internal",
        "create_file",
        "delete_file",
        "rename_file",
        "move_file",
        "replace_string_in_file",
        "insert_edit_into_file",
    ];
    if exact_edit_tools.iter().any(|name| lower == *name) {
        return true;
    }

    lower.contains("edit") || lower.contains("write") || lower.contains("replace")
}

/// Classify GitHub Copilot tool for bash vs file edit handling.
/// GithubCopilot is not in the `Agent` enum, so we implement classification locally.
fn classify_copilot_tool(tool_name: &str) -> ToolClass {
    let lower = tool_name.to_ascii_lowercase();
    match lower.as_str() {
        "run_in_terminal" => ToolClass::Bash,
        "create_file"
        | "replace_string_in_file"
        | "apply_patch"
        | "delete_file"
        | "rename_file"
        | "move_file" => ToolClass::FileEdit,
        _ if lower.contains("edit") || lower.contains("write") || lower.contains("replace") => {
            ToolClass::FileEdit
        }
        _ => ToolClass::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::AgentPreset;
    use super::super::GithubCopilotPreset;
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Legacy extension path tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_copilot_legacy_before_edit() {
        let input = json!({
            "hook_event_name": "before_edit",
            "workspace_folder": "/home/user/project",
            "will_edit_filepaths": ["/home/user/project/src/main.rs"],
            "chat_session_id": "sess-123",
            "dirty_files": {"/home/user/project/src/main.rs": "old content"}
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "github-copilot");
                assert_eq!(e.context.external_session_id, "sess-123");
                assert_eq!(e.context.cwd, PathBuf::from("/home/user/project"));
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                assert!(e.dirty_files.is_some());
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_copilot_dirty_files_camel_case() {
        let input = json!({
            "hook_event_name": "before_edit",
            "workspace_folder": "/home/user/project",
            "will_edit_filepaths": ["/home/user/project/src/main.rs"],
            "chat_session_id": "sess-123",
            "dirtyFiles": {"/home/user/project/src/main.rs": "content"}
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert!(e.dirty_files.is_some());
                let df = e.dirty_files.as_ref().unwrap();
                assert!(df.contains_key(&PathBuf::from("/home/user/project/src/main.rs")));
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_copilot_legacy_after_edit() {
        let input = json!({
            "hook_event_name": "after_edit",
            "workspace_folder": "/home/user/project",
            "chat_session_path": "/home/user/.vscode/sessions/sess-123.json",
            "session_id": "sess-123",
            "edited_filepaths": ["src/main.rs"]
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "github-copilot");
                assert_eq!(e.context.external_session_id, "sess-123");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                assert!(matches!(
                    e.stream_source,
                    Some(StreamSource {
                        format: StreamFormat::CopilotSessionJson,
                        ..
                    })
                ));
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_copilot_legacy_before_edit_empty_filepaths() {
        let input = json!({
            "hook_event_name": "before_edit",
            "workspace_folder": "/home/user/project",
            "will_edit_filepaths": [],
            "chat_session_id": "sess-123"
        })
        .to_string();
        let result = GithubCopilotPreset.parse(&input, "t_test123456789a");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // VS Code native path tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_copilot_native_pre_file_edit() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user/project",
            "tool_name": "replace_string_in_file",
            "session_id": "sess-456",
            "tool_use_id": "tu-1",
            "tool_input": {"file_path": "/home/user/project/src/main.rs"},
            "transcript_path": "/home/user/.vscode/data/github.copilot-chat/transcripts/sess-456.json"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "github-copilot");
                assert_eq!(e.context.external_session_id, "sess-456");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_copilot_native_post_file_edit() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "cwd": "/home/user/project",
            "tool_name": "create_file",
            "session_id": "sess-456",
            "tool_use_id": "tu-2",
            "tool_input": {"file_path": "/home/user/project/src/new.rs"},
            "transcript_path": "/home/user/.vscode/data/github.copilot-chat/transcripts/sess-456.json"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "github-copilot");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/new.rs")]
                );
                assert!(matches!(
                    e.stream_source,
                    Some(StreamSource {
                        format: StreamFormat::CopilotSessionJson,
                        ..
                    })
                ));
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_copilot_native_model_prefers_otel_selected_model_over_models_json_default() {
        let dir = tempfile::tempdir().unwrap();
        let user_dir = dir.path().join("User");
        let transcript_path = user_dir
            .join("workspaceStorage")
            .join("workspace-1")
            .join("GitHub.copilot-chat")
            .join("transcripts")
            .join("session-abc.jsonl");
        std::fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript_path,
            r#"{"type":"session.start","data":{"sessionId":"session-abc"}}"#,
        )
        .unwrap();

        let models_path = user_dir
            .join("workspaceStorage")
            .join("workspace-1")
            .join("GitHub.copilot-chat")
            .join("debug-logs")
            .join("session-abc")
            .join("models.json");
        std::fs::create_dir_all(models_path.parent().unwrap()).unwrap();
        std::fs::write(&models_path, r#"[{"id":"gpt-4.1","is_chat_default":true}]"#).unwrap();

        let otel_db_path = user_dir
            .join("globalStorage")
            .join("github.copilot-chat")
            .join("agent-traces.db");
        std::fs::create_dir_all(otel_db_path.parent().unwrap()).unwrap();
        let conn =
            crate::model::repository::sqlite::open_with_memory_limits(&otel_db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE spans (
                span_id TEXT PRIMARY KEY,
                chat_session_id TEXT,
                request_model TEXT,
                response_model TEXT,
                end_time_ms REAL NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO spans (span_id, chat_session_id, request_model, response_model, end_time_ms)
             VALUES ('span-1', 'session-abc', 'claude-sonnet-4', 'claude-sonnet-4-20250514', 1000)",
            [],
        )
        .unwrap();
        drop(conn);

        let input = json!({
            "hook_event_name": "PostToolUse",
            "cwd": "/home/user/project",
            "tool_name": "create_file",
            "session_id": "session-abc",
            "tool_use_id": "tu-2",
            "tool_input": {"file_path": "/home/user/project/src/new.rs"},
            "transcript_path": transcript_path
        })
        .to_string();
        let mut events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(
            events[0].preset_context_mut().unwrap().agent_id.model,
            "unknown"
        );
        GithubCopilotPreset
            .enrich_authorized_events(&input, &mut events)
            .unwrap();

        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.model, "claude-sonnet-4");
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_copilot_native_pre_bash_call() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user/project",
            "tool_name": "run_in_terminal",
            "session_id": "sess-456",
            "tool_use_id": "tu-3",
            "transcript_path": "/home/user/.vscode/data/github.copilot-chat/transcripts/sess-456.json"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "github-copilot");
                assert_eq!(e.tool_use_id, "tu-3");
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_copilot_native_post_bash_call() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "cwd": "/home/user/project",
            "tool_name": "run_in_terminal",
            "session_id": "sess-456",
            "tool_use_id": "tu-3",
            "transcript_path": "/home/user/.vscode/data/github.copilot-chat/transcripts/sess-456.json"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "github-copilot");
                assert_eq!(e.tool_use_id, "tu-3");
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_copilot_native_create_file_pre_empty_dirty() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user/project",
            "tool_name": "create_file",
            "session_id": "sess-456",
            "tool_input": {"file_path": "/home/user/project/src/new_file.rs"},
            "transcript_path": "/home/user/.vscode/data/github.copilot-chat/transcripts/sess-456.json"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/new_file.rs")]
                );
                assert_eq!(
                    e.dirty_files
                        .as_ref()
                        .unwrap()
                        .get(&PathBuf::from("/home/user/project/src/new_file.rs")),
                    Some(&String::new())
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_copilot_skips_non_edit_tools() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user/project",
            "tool_name": "search_files",
            "session_id": "sess-456",
            "transcript_path": "/home/user/.vscode/data/github.copilot-chat/transcripts/sess-456.json"
        })
        .to_string();
        let result = GithubCopilotPreset.parse(&input, "t_test123456789a");
        assert!(result.is_err());
    }

    #[test]
    fn test_copilot_skips_claude_transcript() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user/project",
            "tool_name": "create_file",
            "session_id": "sess-456",
            "tool_input": {"file_path": "src/main.rs"},
            "transcript_path": "/home/user/.claude/projects/test.json"
        })
        .to_string();
        let result = GithubCopilotPreset.parse(&input, "t_test123456789a");
        assert!(result.is_err());
    }

    #[test]
    fn test_copilot_session_id_fallback() {
        let input = json!({
            "hook_event_name": "before_edit",
            "workspace_folder": "/home/user/project",
            "will_edit_filepaths": ["/home/user/project/src/main.rs"],
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.external_session_id, "unknown");
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    // -----------------------------------------------------------------------
    // Helper function tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_copilot_tool_bash() {
        assert_eq!(classify_copilot_tool("run_in_terminal"), ToolClass::Bash);
    }

    #[test]
    fn test_classify_copilot_tool_file_edit() {
        assert_eq!(classify_copilot_tool("create_file"), ToolClass::FileEdit);
        assert_eq!(
            classify_copilot_tool("replace_string_in_file"),
            ToolClass::FileEdit
        );
        assert_eq!(classify_copilot_tool("apply_patch"), ToolClass::FileEdit);
        assert_eq!(classify_copilot_tool("delete_file"), ToolClass::FileEdit);
    }

    #[test]
    fn test_classify_copilot_tool_heuristic() {
        assert_eq!(
            classify_copilot_tool("custom_edit_tool"),
            ToolClass::FileEdit
        );
        assert_eq!(classify_copilot_tool("write_changes"), ToolClass::FileEdit);
    }

    #[test]
    fn test_classify_copilot_tool_skip() {
        assert_eq!(classify_copilot_tool("search_files"), ToolClass::Skip);
        assert_eq!(classify_copilot_tool("unknown_tool"), ToolClass::Skip);
    }

    #[test]
    fn test_collect_apply_patch_paths() {
        let text = "*** Update File: /home/user/src/main.rs\n--- some diff ---\n*** Add File: /home/user/src/new.rs\n";
        let mut paths = Vec::new();
        parse::collect_apply_patch_paths_from_text(text, &mut paths);
        assert_eq!(
            paths,
            vec!["/home/user/src/main.rs", "/home/user/src/new.rs"]
        );
    }

    #[test]
    fn test_looks_like_copilot_transcript_path() {
        assert!(looks_like_copilot_transcript_path(
            "/home/user/.vscode/data/github.copilot-chat/transcripts/test.json"
        ));
        assert!(looks_like_copilot_transcript_path(
            "/path/to/vscode-chat-session-123.json"
        ));
        assert!(!looks_like_copilot_transcript_path(
            "/home/user/.claude/projects/test.json"
        ));
    }

    #[test]
    fn test_is_supported_vscode_edit_tool_name() {
        assert!(is_supported_vscode_edit_tool_name("create_file"));
        assert!(is_supported_vscode_edit_tool_name("run_in_terminal"));
        assert!(is_supported_vscode_edit_tool_name("replace_string_in_file"));
        assert!(is_supported_vscode_edit_tool_name("custom_edit_tool"));
        assert!(!is_supported_vscode_edit_tool_name("search_files"));
        assert!(!is_supported_vscode_edit_tool_name("read_file"));
    }

    #[test]
    fn test_copilot_camel_case_keys() {
        let input = json!({
            "hookEventName": "before_edit",
            "workspaceFolder": "/home/user/project",
            "will_edit_filepaths": ["/home/user/project/src/main.rs"],
            "chatSessionId": "sess-789"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.external_session_id, "sess-789");
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_copilot_default_after_edit_when_no_hook_event_name() {
        // When hook_event_name is missing, defaults to "after_edit"
        let input = json!({
            "workspace_folder": "/home/user/project",
            "chat_session_path": "/home/user/.vscode/sessions/sess-123.json",
            "session_id": "sess-123",
            "edited_filepaths": ["src/main.rs"]
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ParsedHookEvent::PostFileEdit(_)));
    }

    #[test]
    fn test_copilot_native_workspace_storage_format() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "cwd": "/home/user/project",
            "tool_name": "create_file",
            "session_id": "sess-456",
            "tool_input": {"file_path": "/home/user/project/src/new.rs"},
            "transcript_path": "/home/user/.vscode/data/workspaceStorage/abc/chatSessions/sess-456.json"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert!(matches!(
                    e.stream_source,
                    Some(StreamSource {
                        format: StreamFormat::CopilotEventStreamJsonl,
                        ..
                    })
                ));
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_vscode_apply_patch_real_payload() {
        let pre_input = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "bad0027f-a716-4b05-82dc-c186eb655967",
            "transcript_path": "/Users/svarlamov/Library/Application Support/Code/User/workspaceStorage/e89dd309cf385022c02e2f1c9e8c403f/GitHub.copilot-chat/transcripts/bad0027f-a716-4b05-82dc-c186eb655967.jsonl",
            "tool_name": "apply_patch",
            "tool_input": {
                "explanation": "Change the warning message from 'oops' to 'oopsies'",
                "input": "*** Begin Patch\n*** Update File: /Users/svarlamov/testing-git-ai-sessions-v2-apr-20/testing-git-1/jokes-cli.ts\n@@ rl.question(\"Which joke do you want to hear (1-3)? (Press Enter for a random joke) \", (answer) => {\n-      console.warn(\"oops\");\n+      console.warn(\"oopsies\");\n*** End Patch"
            },
            "tool_use_id": "call_lEov1CG9mTy45oPQYT0VST80__vscode-1778541016875",
            "cwd": "/Users/svarlamov/testing-git-ai-sessions-v2-apr-20/testing-git-1"
        })
        .to_string();

        let events = GithubCopilotPreset
            .parse(&pre_input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from(
                        "/Users/svarlamov/testing-git-ai-sessions-v2-apr-20/testing-git-1/jokes-cli.ts"
                    )]
                );
                assert_eq!(
                    e.tool_use_id.as_deref(),
                    Some("call_lEov1CG9mTy45oPQYT0VST80__vscode-1778541016875")
                );
            }
            other => panic!("Expected PreFileEdit, got {:?}", other),
        }

        let post_input = json!({
            "hook_event_name": "PostToolUse",
            "session_id": "bad0027f-a716-4b05-82dc-c186eb655967",
            "transcript_path": "/Users/svarlamov/Library/Application Support/Code/User/workspaceStorage/e89dd309cf385022c02e2f1c9e8c403f/GitHub.copilot-chat/transcripts/bad0027f-a716-4b05-82dc-c186eb655967.jsonl",
            "tool_name": "apply_patch",
            "tool_input": {
                "explanation": "Change the warning message from 'oops' to 'oopsies'",
                "input": "*** Begin Patch\n*** Update File: /Users/svarlamov/testing-git-ai-sessions-v2-apr-20/testing-git-1/jokes-cli.ts\n@@ rl.question(\"Which joke do you want to hear (1-3)? (Press Enter for a random joke) \", (answer) => {\n-      console.warn(\"oops\");\n+      console.warn(\"oopsies\");\n*** End Patch"
            },
            "tool_response": "",
            "tool_use_id": "call_lEov1CG9mTy45oPQYT0VST80__vscode-1778541016875",
            "cwd": "/Users/svarlamov/testing-git-ai-sessions-v2-apr-20/testing-git-1"
        })
        .to_string();

        let events = GithubCopilotPreset
            .parse(&post_input, "t_test123456789a")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from(
                        "/Users/svarlamov/testing-git-ai-sessions-v2-apr-20/testing-git-1/jokes-cli.ts"
                    )]
                );
                assert!(e.stream_source.is_some());
            }
            other => panic!("Expected PostFileEdit, got {:?}", other),
        }
    }
}
