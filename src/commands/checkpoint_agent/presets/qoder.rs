use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;

/// Preset for [Qoder Desktop](https://docs.qoder.com/zh/extensions/hooks).
///
/// Qoder's hook protocol is Claude Code-compatible: it emits `PreToolUse` /
/// `PostToolUse` events over stdin as JSON, accepts the same tool names
/// (`Bash`/`Write`/`Edit`) alongside its own native names
/// (`run_in_terminal`/`create_file`/`search_replace`/`delete_file`), and uses
/// the same `session_id`/`cwd`/`hook_event_name`/`transcript_path` common
/// fields.
///
/// Unlike Claude Code, Qoder does **not** supply a `tool_use_id` in the hook
/// payload, so this preset derives a deterministic one from the session id,
/// tool name, and tool input. That lets the pre- and post-tool hooks of a
/// single invocation pair up for bash snapshot diffing.
pub struct QoderPreset;

impl QoderPreset {
    /// Derive a stable tool-use id from fields that are identical across the
    /// pre- and post-tool hooks of one invocation. Qoder provides none, but
    /// the bash snapshot system needs a correlating key.
    fn deterministic_tool_use_id(session_id: &str, tool_name: &str, tool_input: &Value) -> String {
        let mut hasher = Sha256::new();
        hasher.update(session_id.as_bytes());
        hasher.update(b":");
        hasher.update(tool_name.as_bytes());
        hasher.update(b":");
        hasher.update(tool_input.to_string().as_bytes());
        let hash = hasher.finalize();
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        hex[..16.min(hex.len())].to_string()
    }
}

impl AgentPreset for QoderPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        // Only tool-use hooks produce file/bash checkpoints. Qoder's other
        // events (UserPromptSubmit, PostToolUseFailure, Stop) are not edits.
        let is_pre = match parse::optional_str_multi(&data, &["hook_event_name", "hookEventName"]) {
            Some("PreToolUse") => true,
            Some("PostToolUse") => false,
            _ => return Ok(vec![]),
        };

        let cwd = parse::required_str(&data, "cwd")?;

        let session_id = parse::optional_str(&data, "session_id")
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                parse::required_file_stem(&data, "transcript_path")
                    .unwrap_or_else(|_| "unknown".to_string())
            });

        let tool_name = parse::optional_str_multi(&data, &["tool_name", "toolName"]).ok_or_else(
            || GitAiError::PresetError("Qoder hook input missing tool_name".to_string()),
        )?;

        let tool_class = bash_tool::classify_tool(Agent::Qoder, tool_name);
        if tool_class == ToolClass::Skip {
            return Ok(vec![]);
        }
        let is_bash = tool_class == ToolClass::Bash;

        let tool_input = data
            .get("tool_input")
            .or_else(|| data.get("toolInput"))
            .cloned()
            .unwrap_or(Value::Null);
        let tool_use_id = Self::deterministic_tool_use_id(&session_id, tool_name, &tool_input);

        let mut metadata = HashMap::new();
        if let Some(tp) = parse::optional_str(&data, "transcript_path") {
            metadata.insert("transcript_path".to_string(), tp.to_string());
        }

        let context = PresetContext {
            agent_id: AgentId {
                tool: "qoder".to_string(),
                id: session_id.clone(),
                // Qoder hooks carry no model field and its transcript format is
                // not yet streamed, so the model is unknown until a stream
                // reader is added (see StreamFormat).
                model: "unknown".to_string(),
            },
            external_session_id: session_id.clone(),
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
            metadata,
        };

        let bash_command = parse::bash_command_from_hook_input(&data);
        let event = match (is_pre, is_bash) {
            (true, true) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id,
                command: bash_command,
            }),
            (true, false) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths: parse::file_paths_from_tool_input(&data, cwd),
                dirty_files: None,
                tool_use_id: Some(tool_use_id),
            }),
            (false, true) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id,
                command: bash_command,
                stream_source: None,
            }),
            (false, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths: parse::file_paths_from_tool_input(&data, cwd),
                dirty_files: None,
                stream_source: None,
                tool_use_id: Some(tool_use_id),
            }),
        };

        Ok(vec![event])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::checkpoint_agent::presets::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn make_qoder_input(event: &str, tool: &str, tool_input: Value) -> String {
        json!({
            "session_id": "qoder-sess-1",
            "cwd": "/home/user/project",
            "hook_event_name": event,
            "transcript_path": "/home/user/.qoder/sessions/qoder-sess-1.json",
            "tool_name": tool,
            "tool_input": tool_input,
        })
        .to_string()
    }

    #[test]
    fn test_qoder_pre_file_edit_write() {
        let input = make_qoder_input("PreToolUse", "Write", json!({ "file_path": "src/main.rs" }));
        let events = QoderPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "qoder");
                assert_eq!(e.context.external_session_id, "qoder-sess-1");
                assert_eq!(e.context.trace_id, "t_test123456789a");
                assert_eq!(e.context.cwd, PathBuf::from("/home/user/project"));
                assert_eq!(e.context.agent_id.model, "unknown");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                assert!(e.tool_use_id.is_some());
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_qoder_post_file_edit_create_file_native_name() {
        let input = make_qoder_input(
            "PostToolUse",
            "create_file",
            json!({ "file_path": "src/lib.rs", "content": "fn main() {}" }),
        );
        let events = QoderPreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "qoder");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/lib.rs")]
                );
                assert!(e.stream_source.is_none());
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_qoder_search_replace_edit_paths() {
        let input =
            make_qoder_input("PostToolUse", "search_replace", json!({ "file_path": "src/app.ts" }));
        let events = QoderPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/app.ts")]
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_qoder_pre_bash_call_compatible_name() {
        let input = make_qoder_input("PreToolUse", "Bash", json!({ "command": "cargo test" }));
        let events = QoderPreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "qoder");
                assert_eq!(e.command.as_deref(), Some("cargo test"));
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_qoder_pre_bash_call_native_name() {
        let input =
            make_qoder_input("PreToolUse", "run_in_terminal", json!({ "command": "npm run build" }));
        let events = QoderPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.command.as_deref(), Some("npm run build"));
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_qoder_post_bash_call() {
        let input = make_qoder_input("PostToolUse", "Bash", json!({ "command": "echo hi" }));
        let events = QoderPreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "qoder");
                assert!(e.stream_source.is_none());
                assert_eq!(e.command.as_deref(), Some("echo hi"));
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_qoder_skips_non_edit_tools() {
        // Read/Grep/Glob/list_dir/search_web are neither file edits nor bash,
        // so they produce no checkpoint.
        for tool in ["Read", "Grep", "Glob", "list_dir", "search_web"] {
            let input = make_qoder_input("PreToolUse", tool, json!({}));
            let events = QoderPreset.parse(&input, "t_test").unwrap();
            assert!(events.is_empty(), "{tool} should produce no events");
        }
    }

    #[test]
    fn test_qoder_skips_non_tool_events() {
        // UserPromptSubmit / Stop / PostToolUseFailure are not checkpoints.
        for event in ["UserPromptSubmit", "Stop", "PostToolUseFailure"] {
            let input = make_qoder_input(event, "Bash", json!({ "command": "ls" }));
            let events = QoderPreset.parse(&input, "t_test").unwrap();
            assert!(events.is_empty(), "{event} should produce no events");
        }
    }

    #[test]
    fn test_qoder_session_id_fallback_to_transcript_stem() {
        let input = json!({
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "transcript_path": "/home/user/.qoder/sessions/abc-123-def.json",
            "tool_name": "Write",
            "tool_input": { "file_path": "src/main.rs" },
        })
        .to_string();
        let events = QoderPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.external_session_id, "abc-123-def");
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_qoder_tool_use_id_is_deterministic() {
        let input = make_qoder_input("PreToolUse", "Write", json!({ "file_path": "src/a.rs" }));
        let id1 = match &QoderPreset.parse(&input, "t1").unwrap()[0] {
            ParsedHookEvent::PreFileEdit(e) => e.tool_use_id.clone().unwrap(),
            _ => panic!(),
        };
        // A different trace_id must not change the derived tool_use_id.
        let id2 = match &QoderPreset.parse(&input, "t2").unwrap()[0] {
            ParsedHookEvent::PreFileEdit(e) => e.tool_use_id.clone().unwrap(),
            _ => panic!(),
        };
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_qoder_tool_use_id_pairs_pre_and_post() {
        // Pre and post of the same tool call share tool_name + tool_input, so
        // they must derive the same tool_use_id for bash snapshot pairing.
        let pre = make_qoder_input("PreToolUse", "Bash", json!({ "command": "make" }));
        let post = make_qoder_input("PostToolUse", "Bash", json!({ "command": "make" }));
        let pre_id = match &QoderPreset.parse(&pre, "t").unwrap()[0] {
            ParsedHookEvent::PreBashCall(e) => e.tool_use_id.clone(),
            _ => panic!(),
        };
        let post_id = match &QoderPreset.parse(&post, "t").unwrap()[0] {
            ParsedHookEvent::PostBashCall(e) => e.tool_use_id.clone(),
            _ => panic!(),
        };
        assert_eq!(pre_id, post_id);
    }

    #[test]
    fn test_qoder_transcript_path_recorded_in_metadata() {
        let input = make_qoder_input("PreToolUse", "Write", json!({ "file_path": "x.rs" }));
        let events = QoderPreset.parse(&input, "t").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(
                    e.context.metadata.get("transcript_path").map(String::as_str),
                    Some("/home/user/.qoder/sessions/qoder-sess-1.json")
                );
            }
            _ => panic!(),
        }
    }

    #[test]
    fn test_qoder_missing_tool_name_errors() {
        let input = json!({
            "session_id": "s",
            "cwd": "/home/user/project",
            "hook_event_name": "PreToolUse",
            "tool_input": { "file_path": "x.rs" },
        })
        .to_string();
        assert!(QoderPreset.parse(&input, "t").is_err());
    }

    #[test]
    fn test_qoder_missing_cwd_errors() {
        let input = json!({
            "session_id": "s",
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "tool_input": { "file_path": "x.rs" },
        })
        .to_string();
        assert!(QoderPreset.parse(&input, "t").is_err());
    }
}
