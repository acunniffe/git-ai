use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext, StreamFormat, StreamSource,
};
use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct ClaudePreset;

impl ClaudePreset {
    fn is_vscode_copilot_hook_payload(data: &serde_json::Value) -> bool {
        if let Some(path) = parse::optional_str(data, "transcript_path") {
            let lower = path.to_lowercase();
            (lower.contains("github copilot") || lower.contains("github.copilot"))
                && !lower.contains(".claude")
        } else {
            data.get("extensionId").is_some()
        }
    }

    fn is_cursor_hook_payload(data: &serde_json::Value) -> bool {
        data.get("cursor_version").is_some()
            || parse::optional_str(data, "transcript_path")
                .map(|p| p.contains(".cursor"))
                .unwrap_or(false)
    }
}

/// Shared parser for agents whose hook protocol is identical to Claude Code's
/// (e.g., ZCode). `tool` is the attribution identity (lowercase agent id);
/// `display_name` is used in error messages.
pub(crate) fn parse_claude_like(
    hook_input: &str,
    trace_id: &str,
    tool: &str,
    display_name: &str,
) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    let data: serde_json::Value = serde_json::from_str(hook_input)
        .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

    if ClaudePreset::is_vscode_copilot_hook_payload(&data) {
        return Err(GitAiError::PresetError(format!(
            "Skipping VS Code hook payload in {} preset; use github-copilot hooks.",
            display_name
        )));
    }
    if ClaudePreset::is_cursor_hook_payload(&data) {
        return Err(GitAiError::PresetError(format!(
            "Skipping Cursor hook payload in {} preset; use cursor hooks.",
            display_name
        )));
    }

    // These agents use the same capitalized tool names as Claude Code, so
    // reuse Claude's tool classification.
    let tool_class = parse::optional_str_multi(&data, &["tool_name", "toolName"])
        .map(|name| bash_tool::classify_tool(Agent::Claude, name))
        // Preserve legacy Claude payloads that predate tool_name.
        .unwrap_or(ToolClass::FileEdit);
    if tool_class == ToolClass::Skip {
        return Ok(Vec::new());
    }

    let cwd = parse::required_str(&data, "cwd")?;
    let transcript_path = parse::required_str(&data, "transcript_path")?;
    let session_id = parse::optional_str(&data, "session_id")
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            parse::required_file_stem(&data, "transcript_path")
                .unwrap_or_else(|_| "unknown".to_string())
        });

    let hook_event = parse::optional_str_multi(&data, &["hook_event_name", "hookEventName"]);
    let tool_use_id = parse::str_or_default_multi(&data, &["tool_use_id", "toolUseId"], "bash");

    let is_bash = tool_class == ToolClass::Bash;

    let context = PresetContext {
        agent_id: AgentId {
            tool: tool.to_string(),
            id: session_id.clone(),
            model: crate::streams::model_extraction::extract_model(
                Path::new(transcript_path),
                crate::streams::sweep::StreamFormat::ClaudeJsonl,
                None,
            )
            .ok()
            .flatten()
            .unwrap_or_else(|| "unknown".to_string()),
        },
        external_session_id: session_id.clone(),
        trace_id: trace_id.to_string(),
        cwd: PathBuf::from(cwd),
        metadata: HashMap::from([("transcript_path".to_string(), transcript_path.to_string())]),
    };

    let transcript_path_buf = PathBuf::from(transcript_path);
    let external_parent_session_id =
        crate::streams::agents::claude::ClaudeAgent::detect_subagent_parent(&transcript_path_buf);
    let stream_source = Some(StreamSource {
        path: transcript_path_buf,
        format: StreamFormat::ClaudeJsonl,
        session_id: generate_session_id(&session_id, tool),
        external_session_id: session_id.clone(),
        external_parent_session_id,
    });

    let bash_command = parse::bash_command_from_hook_input(&data);
    let event = match (hook_event, is_bash) {
        (Some("PreToolUse"), true) => ParsedHookEvent::PreBashCall(PreBashCall {
            context,
            tool_use_id: tool_use_id.to_string(),
            command: bash_command,
        }),
        (Some("PreToolUse"), false) => ParsedHookEvent::PreFileEdit(PreFileEdit {
            context,
            file_paths: parse::file_paths_from_tool_input(&data, cwd),
            dirty_files: None,
            tool_use_id: Some(tool_use_id.to_string()),
        }),
        (_, true) => ParsedHookEvent::PostBashCall(PostBashCall {
            context,
            tool_use_id: tool_use_id.to_string(),
            command: bash_command,
            stream_source,
        }),
        (_, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
            context,
            file_paths: parse::file_paths_from_tool_input(&data, cwd),
            dirty_files: None,
            stream_source,
            tool_use_id: Some(tool_use_id.to_string()),
        }),
    };

    Ok(vec![event])
}

impl AgentPreset for ClaudePreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        parse_claude_like(hook_input, trace_id, "claude", "Claude")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::checkpoint_agent::presets::*;
    use serde_json::json;

    fn make_claude_hook_input(event: &str, tool: &str) -> String {
        json!({
            "transcript_path": "/home/user/.claude/projects/abc123.jsonl",
            "cwd": "/home/user/project",
            "hook_event_name": event,
            "tool_name": tool,
            "session_id": "sess-1",
            "tool_use_id": "tu-1",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string()
    }

    #[test]
    fn test_claude_pre_file_edit() {
        let input = make_claude_hook_input("PreToolUse", "Write");
        let events = ClaudePreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "claude");
                assert_eq!(e.context.external_session_id, "sess-1");
                assert_eq!(e.context.trace_id, "t_test123456789a");
                assert_eq!(e.context.cwd, PathBuf::from("/home/user/project"));
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_claude_post_file_edit() {
        let input = make_claude_hook_input("PostToolUse", "Write");
        let events = ClaudePreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "claude");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                assert!(e.stream_source.is_some());
                if let Some(ts) = &e.stream_source {
                    assert_eq!(ts.format, StreamFormat::ClaudeJsonl);
                    assert_eq!(ts.session_id, generate_session_id("sess-1", "claude"));
                    assert_eq!(ts.external_session_id, "sess-1");
                }
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_claude_pre_bash_call() {
        let input = make_claude_hook_input("PreToolUse", "Bash");
        let events = ClaudePreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "claude");
                assert_eq!(e.tool_use_id, "tu-1");
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_claude_post_bash_call() {
        let input = make_claude_hook_input("PostToolUse", "Bash");
        let events = ClaudePreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "claude");
                assert_eq!(e.tool_use_id, "tu-1");
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_claude_ignores_read_only_and_unsupported_tools() {
        for hook_event in ["PreToolUse", "PostToolUse"] {
            for tool_name in ["Read", "Glob", "Grep", "Task", "UnknownTool"] {
                let input = json!({
                    "hook_event_name": hook_event,
                    "tool_name": tool_name,
                    "transcript_path": "/does/not/exist.jsonl"
                })
                .to_string();

                let events = ClaudePreset.parse(&input, "t_test123456789a").unwrap();
                assert!(
                    events.is_empty(),
                    "{hook_event} {tool_name} unexpectedly produced events"
                );
            }
        }
    }

    #[test]
    fn test_ignored_claude_hook_produces_no_checkpoint_requests() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Read",
            "transcript_path": "/does/not/exist.jsonl"
        })
        .to_string();

        let requests = crate::commands::checkpoint_agent::orchestrator::execute_preset_checkpoint(
            "claude", &input,
        )
        .unwrap();
        assert!(requests.is_empty());
    }

    #[test]
    fn test_claude_preserves_all_mutating_file_tools() {
        for tool_name in ["Write", "Edit", "MultiEdit"] {
            let pre = make_claude_hook_input("PreToolUse", tool_name);
            assert!(matches!(
                ClaudePreset.parse(&pre, "t_test123456789a").unwrap()[..],
                [ParsedHookEvent::PreFileEdit(_)]
            ));

            let post = make_claude_hook_input("PostToolUse", tool_name);
            assert!(matches!(
                ClaudePreset.parse(&post, "t_test123456789a").unwrap()[..],
                [ParsedHookEvent::PostFileEdit(_)]
            ));
        }
    }

    #[test]
    fn test_claude_session_id_from_filename() {
        let input = json!({
            "transcript_path": "/home/user/.claude/projects/cb947e5b-246e-4253-a953-631f7e464c6b.jsonl",
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        let events = ClaudePreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.context.external_session_id,
                    "cb947e5b-246e-4253-a953-631f7e464c6b"
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_claude_skips_vscode_copilot_payload() {
        let input = json!({
            "transcript_path": "/home/user/.vscode/extensions/GitHub Copilot/sessions/test.json",
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        assert!(ClaudePreset.parse(&input, "t_test123456789a").is_err());
    }

    #[test]
    fn test_claude_skips_cursor_payload() {
        let input = json!({
            "transcript_path": "/home/user/.cursor/sessions/test.jsonl",
            "cwd": "/home/user/project",
            "cursor_version": "0.43",
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        assert!(ClaudePreset.parse(&input, "t_test123456789a").is_err());
    }
}
