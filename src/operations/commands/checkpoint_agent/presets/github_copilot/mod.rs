use super::parse;
use super::{AgentPreset, ParsedHookEvent};
use crate::error::GitAiError;
use std::collections::HashMap;
use std::path::PathBuf;

mod cli;
mod enrichment;
mod ide;

pub struct GithubCopilotPreset;

impl AgentPreset for GithubCopilotPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = parse::hook_json(hook_input)?;

        let hook_event_name =
            parse::optional_str_multi(&data, &["hook_event_name", "hookEventName"])
                .unwrap_or("after_edit");

        if hook_event_name == "before_edit" || hook_event_name == "after_edit" {
            return ide::parse_legacy_extension_hooks(&data, hook_event_name, trace_id);
        }

        if hook_event_name == "PreToolUse" || hook_event_name == "PostToolUse" {
            let has_transcript_path = parse::optional_str_multi(
                &data,
                &[
                    "transcript_path",
                    "transcriptPath",
                    "chat_session_path",
                    "chatSessionPath",
                ],
            )
            .is_some();

            if !has_transcript_path {
                return cli::parse_cli_hooks(&data, hook_event_name, trace_id);
            }
            return ide::parse_vscode_native_hooks(&data, hook_event_name, trace_id);
        }

        Err(GitAiError::PresetError(format!(
            "Invalid hook_event_name: {}. Expected one of 'before_edit', 'after_edit', 'PreToolUse', or 'PostToolUse'",
            hook_event_name
        )))
    }

    fn enrich_authorized_events(
        &self,
        hook_input: &str,
        events: &mut [ParsedHookEvent],
    ) -> Result<(), GitAiError> {
        enrichment::enrich_authorized_events(hook_input, events)
    }
}

// ---------------------------------------------------------------------------
// Shared helpers (used by both ide.rs and cli.rs)
// ---------------------------------------------------------------------------

pub(super) fn extract_session_id(data: &serde_json::Value) -> String {
    parse::optional_str_multi(
        data,
        &[
            "chat_session_id",
            "session_id",
            "chatSessionId",
            "sessionId",
        ],
    )
    .unwrap_or("unknown")
    .to_string()
}

pub(super) fn dirty_files_from_hook_data(
    data: &serde_json::Value,
    cwd: &str,
) -> Option<HashMap<PathBuf, String>> {
    let obj = data
        .get("dirty_files")
        .and_then(|v| v.as_object())
        .or_else(|| data.get("dirtyFiles").and_then(|v| v.as_object()))?;

    let mut result = HashMap::new();
    for (key, value) in obj {
        if let Some(content) = value.as_str() {
            let path = parse::resolve_absolute(key, cwd);
            result.insert(path, content.to_string());
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Extract file paths from VS Code / CLI hook payload (tool_input + tool_response/tool_result).
/// Only paths from the current tool call are extracted — no session-level data.
pub(super) fn extract_filepaths_from_vscode_hook_payload(
    tool_input: Option<&serde_json::Value>,
    tool_response: Option<&serde_json::Value>,
    cwd: &str,
) -> Vec<PathBuf> {
    parse::nested_tool_file_paths(tool_input.into_iter().chain(tool_response), cwd)
}

pub(super) fn transcript_format(path: &str) -> super::StreamFormat {
    if path.contains("/workspaceStorage/") || path.contains("\\workspaceStorage\\") {
        super::StreamFormat::CopilotEventStreamJsonl
    } else {
        super::StreamFormat::CopilotSessionJson
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::commands::checkpoint_agent::presets::ParsedHookEvent;
    use serde_json::json;

    #[test]
    fn vscode_payload_extracts_path_from_response_without_input() {
        let response = json!({"nested": {"file_path": "src/from-response.rs"}});

        assert_eq!(
            extract_filepaths_from_vscode_hook_payload(None, Some(&response), "/repo"),
            vec![PathBuf::from("/repo/src/from-response.rs")]
        );
    }

    // -----------------------------------------------------------------------
    // Top-level fork dispatch tests
    // -----------------------------------------------------------------------

    /// CLI shape (no transcript_path, tool_name="create") routes to cli::parse_cli_hooks
    /// — visible by the synthesized "source=copilot-cli" metadata entry.
    #[test]
    fn dispatches_cli_when_tool_name_is_cli_shape() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user/project",
            "tool_name": "create",
            "session_id": "sess-cli",
            "tool_input": {"path": "/home/user/project/new.md", "file_text": "hi"}
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(
                    e.context.metadata.get("source"),
                    Some(&"copilot-cli".to_string())
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    /// IDE shape (transcript_path present, tool_name="create_file") routes to
    /// ide::parse_vscode_native_hooks — verified by the absence of the CLI marker.
    #[test]
    fn dispatches_ide_when_tool_name_is_ide_shape() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user/project",
            "tool_name": "create_file",
            "session_id": "sess-ide",
            "tool_input": {"file_path": "/home/user/project/new.md"},
            "transcript_path": "/home/user/.vscode/data/github.copilot-chat/transcripts/sess-ide.json"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert!(!e.context.metadata.contains_key("source"));
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    /// Legacy IDE shape (before_edit) always routes to ide::parse_legacy_extension_hooks
    /// regardless of any other fields.
    #[test]
    fn dispatches_ide_legacy_for_before_edit() {
        let input = json!({
            "hook_event_name": "before_edit",
            "workspace_folder": "/home/user/project",
            "will_edit_filepaths": ["/home/user/project/src/main.rs"],
            "chat_session_id": "sess-legacy"
        })
        .to_string();
        let events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert!(matches!(events[0], ParsedHookEvent::PreFileEdit(_)));
    }
}
