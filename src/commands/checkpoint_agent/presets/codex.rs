use super::opencode::OpenCodePreset;
use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext, StreamFormat, StreamSource,
};
use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use crate::mdm::utils::codex_home_dir;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct CodexPreset;

impl CodexPreset {
    fn session_id_from_hook_data(data: &serde_json::Value) -> Result<String, GitAiError> {
        // Try session_id, thread_id (underscore), and thread-id (hyphen)
        parse::optional_str_multi(data, &["session_id", "thread_id"])
            .or_else(|| data.get("thread-id").and_then(|v| v.as_str()))
            .or_else(|| {
                data.get("hook_event")
                    .and_then(|ev| ev.get("thread_id"))
                    .and_then(|v| v.as_str())
            })
            .map(|s| s.to_string())
            .ok_or_else(|| {
                GitAiError::PresetError(
                    "session_id or thread_id not found in hook_input".to_string(),
                )
            })
    }

    fn resolve_transcript_path(data: &serde_json::Value, session_id: &str) -> Option<String> {
        if let Some(tp) = parse::optional_str(data, "transcript_path") {
            return Some(tp.to_string());
        }

        crate::streams::agents::CodexAgent::find_rollout_path_for_session_in_home(
            session_id,
            &codex_home_dir(),
        )
        .ok()
        .flatten()
        .map(|p| p.to_string_lossy().into_owned())
    }

    fn extract_filepaths_from_tool_response(hook_data: &serde_json::Value) -> Vec<PathBuf> {
        let Some(tool_response) = hook_data.get("tool_response") else {
            return vec![];
        };
        let output = if let Some(raw) = tool_response.as_str() {
            serde_json::from_str::<serde_json::Value>(raw)
                .ok()
                .and_then(|value| {
                    value
                        .get("output")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| raw.to_string())
        } else {
            tool_response
                .get("output")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default()
        };

        let mut paths = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.len() > 2
                && trimmed.as_bytes()[1] == b' '
                && matches!(trimmed.as_bytes()[0], b'A' | b'M' | b'D' | b'R' | b'U')
            {
                let path = trimmed[2..].trim();
                if !path.is_empty() {
                    let pb = PathBuf::from(path);
                    if !paths.contains(&pb) {
                        paths.push(pb);
                    }
                }
            }
        }
        paths
    }
}

impl AgentPreset for CodexPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let cwd = parse::required_str(&data, "cwd")?;
        let session_id = Self::session_id_from_hook_data(&data)?;
        let hook_event = parse::optional_str_multi(&data, &["hook_event_name", "hookEventName"]);
        let tool_name = parse::optional_str_multi(&data, &["tool_name", "toolName"]);
        let tool_use_id =
            parse::optional_str_multi(&data, &["tool_use_id", "toolUseId"]).unwrap_or("bash");

        let tool_class = tool_name
            .map(|n| bash_tool::classify_tool(Agent::Codex, n))
            .unwrap_or(ToolClass::Skip);

        let is_bash = tool_class == ToolClass::Bash;
        let is_file_edit = tool_class == ToolClass::FileEdit;

        let transcript_path = Self::resolve_transcript_path(&data, &session_id);

        let mut metadata = HashMap::new();
        if let Some(ref tp) = transcript_path {
            metadata.insert("transcript_path".to_string(), tp.clone());
        }

        let model = parse::optional_str(&data, "model")
            .map(str::to_string)
            .or_else(|| {
                transcript_path.as_ref().and_then(|tp| {
                    crate::streams::model_extraction::extract_model(
                        Path::new(tp),
                        crate::streams::sweep::StreamFormat::CodexJsonl,
                        None,
                    )
                    .ok()
                    .flatten()
                })
            })
            .unwrap_or_else(|| "unknown".to_string());

        let context = PresetContext {
            agent_id: AgentId {
                tool: "codex".to_string(),
                id: session_id.clone(),
                model,
            },
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
            metadata,
        };

        let stream_source = transcript_path.map(|tp| StreamSource {
            path: PathBuf::from(tp),
            format: StreamFormat::CodexJsonl,
            session_id: generate_session_id(&context.external_session_id, "codex"),
            external_session_id: context.external_session_id.clone(),
            external_parent_session_id: None,
        });

        let bash_command = parse::bash_command_from_hook_input(&data);
        let event = match hook_event {
            Some("PreToolUse") => {
                if is_bash {
                    ParsedHookEvent::PreBashCall(PreBashCall {
                        context,
                        tool_use_id: tool_use_id.to_string(),
                        command: bash_command,
                    })
                } else if is_file_edit {
                    ParsedHookEvent::PreFileEdit(PreFileEdit {
                        context,
                        file_paths: vec![],
                        dirty_files: None,
                        tool_use_id: Some(tool_use_id.to_string()),
                    })
                } else {
                    return Err(GitAiError::PresetError(format!(
                        "Skipping Codex PreToolUse for unsupported tool {}",
                        tool_name.unwrap_or("unknown")
                    )));
                }
            }
            Some("PostToolUse") => {
                if is_bash {
                    ParsedHookEvent::PostBashCall(PostBashCall {
                        context,
                        tool_use_id: tool_use_id.to_string(),
                        command: bash_command,
                        stream_source,
                    })
                } else if is_file_edit {
                    let tool_input = data.get("tool_input").or_else(|| data.get("toolInput"));
                    let mut file_paths =
                        OpenCodePreset::extract_filepaths_from_tool_input(tool_input, cwd);

                    if file_paths.is_empty() {
                        file_paths = Self::extract_filepaths_from_tool_response(&data);
                    }

                    ParsedHookEvent::PostFileEdit(PostFileEdit {
                        context,
                        file_paths,
                        dirty_files: None,
                        stream_source,
                        tool_use_id: Some(tool_use_id.to_string()),
                    })
                } else {
                    return Err(GitAiError::PresetError(format!(
                        "Skipping Codex PostToolUse for unsupported tool {}",
                        tool_name.unwrap_or("unknown")
                    )));
                }
            }
            _ => {
                return Err(GitAiError::PresetError(format!(
                    "Unsupported Codex hook_event_name: {}",
                    hook_event.unwrap_or("<missing>")
                )));
            }
        };

        Ok(vec![event])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::checkpoint_agent::presets::*;
    use serde_json::json;

    #[test]
    fn test_codex_pre_bash_call() {
        let input = json!({
            "cwd": "/home/user/project",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "session_id": "codex-sess-1",
            "tool_use_id": "tu-1",
            "model": "o3",
            "tool_input": {"command": "echo hello"},
            "transcript_path": "/home/user/.codex/sessions/test.jsonl"
        })
        .to_string();
        let events = CodexPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "codex");
                assert_eq!(e.context.external_session_id, "codex-sess-1");
                assert_eq!(e.context.agent_id.model, "o3");
                assert_eq!(e.tool_use_id, "tu-1");
                assert_eq!(e.command.as_deref(), Some("echo hello"));
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_codex_post_bash_call() {
        let input = json!({
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "session_id": "codex-sess-1",
            "tool_use_id": "tu-1",
            "tool_input": {"command": "echo hello"},
            "transcript_path": "/home/user/.codex/sessions/test.jsonl"
        })
        .to_string();
        let events = CodexPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "codex");
                assert!(matches!(
                    e.stream_source,
                    Some(StreamSource {
                        format: StreamFormat::CodexJsonl,
                        ..
                    })
                ));
                assert_eq!(e.command.as_deref(), Some("echo hello"));
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_codex_uses_transcript_model_when_hook_model_missing() {
        use std::io::Write;

        let mut transcript = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        transcript
            .write_all(br#"{"type":"session_meta","payload":{"model":"gpt-5.3-codex"}}"#)
            .unwrap();
        transcript.flush().unwrap();

        let transcript_path = transcript.path().to_string_lossy().to_string();

        let input = json!({
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "session_id": "codex-sess-1",
            "tool_use_id": "tu-1",
            "transcript_path": transcript_path
        })
        .to_string();

        let events = CodexPreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.model, "gpt-5.3-codex");
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_codex_thread_id_fallback() {
        let input = json!({
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "thread_id": "thread-abc",
            "tool_use_id": "tu-1"
        })
        .to_string();
        let events = CodexPreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.external_session_id, "thread-abc");
                assert!(e.stream_source.is_none());
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_codex_shell_tool_variants_treated_as_bash() {
        for tool_name in &["exec_command", "shell", "shell_command"] {
            let input = json!({
                "cwd": "/home/user/project",
                "hook_event_name": "PostToolUse",
                "tool_name": tool_name,
                "session_id": "codex-sess-1",
                "tool_use_id": "exec-1"
            })
            .to_string();
            let events = CodexPreset.parse(&input, "t_test123456789a").unwrap();
            assert_eq!(events.len(), 1);
            match &events[0] {
                ParsedHookEvent::PostBashCall(e) => {
                    assert_eq!(e.context.agent_id.tool, "codex");
                }
                _ => panic!("Expected PostBashCall for {}", tool_name),
            }
        }
    }

    #[test]
    fn test_codex_namespaced_tools_are_classified() {
        for tool_name in &[
            "functions.apply_patch",
            "functions.exec_command",
            "functions.Bash",
            "multi_tool_use.parallel",
        ] {
            let input = json!({
                "cwd": "/home/user/project",
                "hook_event_name": "PostToolUse",
                "tool_name": tool_name,
                "session_id": "codex-sess-1",
                "tool_use_id": "tu-1"
            })
            .to_string();
            let events = CodexPreset.parse(&input, "t_test123456789a").unwrap();
            assert_eq!(events.len(), 1, "{} should parse to one event", tool_name);
            match &events[0] {
                ParsedHookEvent::PostBashCall(_) => {
                    assert!(
                        tool_name.ends_with("exec_command")
                            || tool_name.ends_with("Bash")
                            || *tool_name == "multi_tool_use.parallel",
                        "{} should be bash",
                        tool_name
                    );
                }
                ParsedHookEvent::PostFileEdit(_) => {
                    assert_eq!(
                        *tool_name, "functions.apply_patch",
                        "only apply_patch is a file edit"
                    );
                }
                _ => panic!("Expected PostBashCall or PostFileEdit for {}", tool_name),
            }
        }
    }

    #[test]
    fn test_codex_extracts_patch_file_paths_from_patch_and_command_keys() {
        let input = json!({
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "apply_patch",
            "session_id": "codex-sess-1",
            "tool_use_id": "patch-1",
            "tool_input": {
                "patch": "*** Update File: /home/user/project/src/main.rs\n@@ old\n+new\n"
            }
        })
        .to_string();
        let events = CodexPreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }

        let input = json!({
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "functions.apply_patch",
            "session_id": "codex-sess-1",
            "tool_use_id": "patch-2",
            "tool_input": {
                "command": "*** Update File: /home/user/project/src/main.rs\n@@ old\n+new\n"
            }
        })
        .to_string();
        let events = CodexPreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_codex_rejects_non_bash_tool() {
        let input = json!({
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "write_file",
            "session_id": "codex-sess-1"
        })
        .to_string();
        let result = CodexPreset.parse(&input, "t_test123456789a");
        assert!(result.is_err());
    }

    #[test]
    fn test_codex_missing_session_and_thread_id() {
        let input = json!({
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash"
        })
        .to_string();
        let result = CodexPreset.parse(&input, "t_test123456789a");
        assert!(result.is_err());
    }

    #[test]
    fn test_codex_rejects_unknown_event() {
        let input = json!({
            "cwd": "/home/user/project",
            "hook_event_name": "SomeFutureEvent",
            "session_id": "codex-sess-1"
        })
        .to_string();
        let result = CodexPreset.parse(&input, "t_test123456789a");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unsupported"));
    }
}
