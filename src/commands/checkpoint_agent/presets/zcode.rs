use super::claude::parse_claude_like;
use super::{AgentPreset, ParsedHookEvent};
use crate::error::GitAiError;

pub struct ZcodePreset;

impl AgentPreset for ZcodePreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        // ZCode speaks the Claude Code hook protocol, so parsing is shared with
        // the Claude preset; only the attribution identity differs.
        parse_claude_like(hook_input, trace_id, "zcode", "ZCode")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::authorship_log_serialization::generate_session_id;
    use crate::commands::checkpoint_agent::presets::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn make_zcode_hook_input(event: &str, tool: &str) -> String {
        json!({
            "transcript_path": "C:/Users/me/AppData/Local/Temp/zcode-claude-hook-abc123/transcript.jsonl",
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
    fn test_zcode_pre_file_edit() {
        let input = make_zcode_hook_input("PreToolUse", "Write");
        let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "zcode");
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
    fn test_zcode_post_file_edit() {
        let input = make_zcode_hook_input("PostToolUse", "Write");
        let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "zcode");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                assert!(e.stream_source.is_some());
                if let Some(ts) = &e.stream_source {
                    assert_eq!(ts.format, StreamFormat::ClaudeJsonl);
                    assert_eq!(ts.session_id, generate_session_id("sess-1", "zcode"));
                    assert_eq!(ts.external_session_id, "sess-1");
                }
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_zcode_pre_bash_call() {
        let input = make_zcode_hook_input("PreToolUse", "Bash");
        let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "zcode");
                assert_eq!(e.tool_use_id, "tu-1");
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_zcode_post_bash_call() {
        let input = make_zcode_hook_input("PostToolUse", "Bash");
        let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "zcode");
                assert_eq!(e.tool_use_id, "tu-1");
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_zcode_ignores_read_only_and_unsupported_tools() {
        for hook_event in ["PreToolUse", "PostToolUse"] {
            for tool_name in ["Read", "Glob", "Grep", "Task", "UnknownTool"] {
                let input = json!({
                    "hook_event_name": hook_event,
                    "tool_name": tool_name,
                    "transcript_path": "/does/not/exist.jsonl"
                })
                .to_string();

                let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
                assert!(
                    events.is_empty(),
                    "{hook_event} {tool_name} unexpectedly produced events"
                );
            }
        }
    }

    #[test]
    fn test_ignored_zcode_hook_produces_no_checkpoint_requests() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Read",
            "transcript_path": "/does/not/exist.jsonl"
        })
        .to_string();

        let requests = crate::commands::checkpoint_agent::orchestrator::execute_preset_checkpoint(
            "zcode", &input,
        )
        .unwrap();
        assert!(requests.is_empty());
    }

    #[test]
    fn test_zcode_preserves_all_mutating_file_tools() {
        for tool_name in ["Write", "Edit", "MultiEdit"] {
            let pre = make_zcode_hook_input("PreToolUse", tool_name);
            assert!(matches!(
                ZcodePreset.parse(&pre, "t_test123456789a").unwrap()[..],
                [ParsedHookEvent::PreFileEdit(_)]
            ));

            let post = make_zcode_hook_input("PostToolUse", tool_name);
            assert!(matches!(
                ZcodePreset.parse(&post, "t_test123456789a").unwrap()[..],
                [ParsedHookEvent::PostFileEdit(_)]
            ));
        }
    }

    #[test]
    fn test_zcode_session_id_from_filename() {
        let input = json!({
            "transcript_path": "C:/Users/me/AppData/Local/Temp/zcode-claude-hook-abc123/cb947e5b-246e-4253-a953-631f7e464c6b.jsonl",
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
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
    fn test_zcode_skips_vscode_copilot_payload() {
        let input = json!({
            "transcript_path": "/home/user/.vscode/extensions/GitHub Copilot/sessions/test.json",
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        assert!(ZcodePreset.parse(&input, "t_test123456789a").is_err());
    }

    #[test]
    fn test_zcode_skips_cursor_payload() {
        let input = json!({
            "transcript_path": "/home/user/.cursor/sessions/test.jsonl",
            "cwd": "/home/user/project",
            "cursor_version": "0.43",
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        assert!(ZcodePreset.parse(&input, "t_test123456789a").is_err());
    }
}
