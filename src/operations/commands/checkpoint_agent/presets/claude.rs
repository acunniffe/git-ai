use super::parse;
use super::{AgentPreset, ParsedHookEvent, PresetContext, StreamFormat, StreamSource, claude_wire};
use crate::error::GitAiError;
use crate::model::authorship_log_serialization::generate_session_id;
use crate::model::working_log::AgentId;
use crate::operations::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use std::collections::HashMap;
use std::path::PathBuf;

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

impl AgentPreset for ClaudePreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = parse::hook_json(hook_input)?;

        if Self::is_vscode_copilot_hook_payload(&data) {
            return Err(GitAiError::PresetError(
                "Skipping VS Code hook payload in Claude preset; use github-copilot hooks."
                    .to_string(),
            ));
        }
        if Self::is_cursor_hook_payload(&data) {
            return Err(GitAiError::PresetError(
                "Skipping Cursor hook payload in Claude preset; use cursor hooks.".to_string(),
            ));
        }

        let cwd = parse::required_str(&data, "cwd")?;
        let transcript_path = parse::required_str(&data, "transcript_path")?;

        let session_id = parse::optional_str(&data, "session_id")
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                parse::required_file_stem(&data, "transcript_path")
                    .unwrap_or_else(|_| "unknown".to_string())
            });

        let tool_name = parse::optional_str_multi(&data, &["tool_name", "toolName"]);
        let hook_event = parse::optional_str_multi(&data, &["hook_event_name", "hookEventName"]);
        let tool_use_id = parse::str_or_default_multi(&data, &["tool_use_id", "toolUseId"], "bash");

        let is_bash = tool_name
            .map(|n| bash_tool::classify_tool(Agent::Claude, n) == ToolClass::Bash)
            .unwrap_or(false);

        let context = PresetContext {
            agent_id: AgentId {
                tool: "claude".to_string(),
                id: session_id.clone(),
                model: "unknown".to_string(),
            },
            external_session_id: session_id.clone(),
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
            metadata: HashMap::from([("transcript_path".to_string(), transcript_path.to_string())]),
        };

        let transcript_path_buf = PathBuf::from(transcript_path);
        let external_parent_session_id =
            crate::operations::streams::agents::claude::ClaudeAgent::detect_subagent_parent(
                &transcript_path_buf,
            );
        let stream_source = Some(StreamSource {
            path: transcript_path_buf,
            format: StreamFormat::ClaudeJsonl,
            session_id: generate_session_id(&session_id, "claude"),
            external_session_id: session_id.clone(),
            external_parent_session_id,
        });

        let is_pre = hook_event == Some("PreToolUse");
        let bash_command = parse::bash_command_from_hook_input(&data);
        let file_paths = parse::file_paths_from_tool_input(&data, cwd);

        Ok(vec![claude_wire::build_wire_event(
            is_pre,
            is_bash,
            context,
            tool_use_id.to_string(),
            bash_command,
            file_paths,
            None,
            stream_source,
        )])
    }

    fn enrich_authorized_events(
        &self,
        _hook_input: &str,
        events: &mut [ParsedHookEvent],
    ) -> Result<(), GitAiError> {
        for event in events {
            let Some(context) = event.preset_context_mut() else {
                continue;
            };
            let Some(path) = context.metadata.get("transcript_path") else {
                continue;
            };
            context.agent_id.model = crate::operations::streams::model_extraction::extract_model(
                std::path::Path::new(path),
                crate::operations::streams::sweep::StreamFormat::ClaudeJsonl,
                None,
            )
            .ok()
            .flatten()
            .unwrap_or_else(|| "unknown".to_string());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::commands::checkpoint_agent::presets::*;
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

    #[test]
    fn parse_does_not_read_model_before_authorized_enrichment() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), r#"{"model":"model-from-disk"}"#).unwrap();
        let input = json!({
            "transcript_path": temp.path(),
            "cwd": "/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "session_id": "session-pure",
            "tool_input": {"file_path": "/project/main.rs"},
        })
        .to_string();

        let mut events = ClaudePreset.parse(&input, "t_test").unwrap();
        let ParsedHookEvent::PostFileEdit(event) = &events[0] else {
            panic!("Expected PostFileEdit");
        };
        assert_eq!(event.context.agent_id.model, "unknown");

        ClaudePreset
            .enrich_authorized_events(&input, &mut events)
            .unwrap();
        let ParsedHookEvent::PostFileEdit(event) = &events[0] else {
            panic!("Expected PostFileEdit");
        };
        assert_eq!(event.context.agent_id.model, "model-from-disk");
    }
}
