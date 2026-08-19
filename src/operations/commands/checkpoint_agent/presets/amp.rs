use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext, StreamFormat, StreamSource,
};
use crate::error::GitAiError;
use crate::model::authorship_log_serialization::generate_session_id;
use crate::model::working_log::AgentId;
use crate::operations::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

pub struct AmpPreset;

#[derive(Debug, Deserialize)]
struct AmpHookInput {
    hook_event_name: String,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    edited_filepaths: Option<Vec<String>>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
    #[serde(default)]
    tool_name: Option<String>,
}

impl AmpPreset {
    fn extract_file_paths(hook_input: &AmpHookInput, cwd: &str) -> Vec<PathBuf> {
        if let Some(paths) = &hook_input.edited_filepaths
            && !paths.is_empty()
        {
            return paths
                .iter()
                .map(|p| parse::resolve_absolute(p, cwd))
                .collect();
        }

        if let Some(tool_input) = &hook_input.tool_input {
            let mut files = Vec::new();

            for key in ["path", "filePath", "file_path"] {
                if let Some(path) = tool_input.get(key).and_then(|value| value.as_str())
                    && !path.trim().is_empty()
                {
                    files.push(parse::resolve_absolute(path, cwd));
                }
            }

            if let Some(paths) = tool_input.get("paths").and_then(|value| value.as_array()) {
                for path in paths {
                    if let Some(path) = path.as_str()
                        && !path.trim().is_empty()
                    {
                        files.push(parse::resolve_absolute(path, cwd));
                    }
                }
            }

            if !files.is_empty() {
                return files;
            }
        }

        vec![]
    }
}

impl AgentPreset for AmpPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let hook_input: AmpHookInput = parse::hook_json(hook_input)?;

        let is_pre = hook_input.hook_event_name == "PreToolUse";

        let tool_class =
            bash_tool::classify_optional_tool(Agent::Amp, hook_input.tool_name.as_deref());
        if tool_class == ToolClass::Skip {
            return Ok(Vec::new());
        }
        let is_bash = tool_class == ToolClass::Bash;

        let cwd = hook_input.cwd.as_deref().unwrap_or(".");

        let thread_id = hook_input.thread_id.clone();
        let tool_use_id = hook_input.tool_use_id.clone();
        let tool_use_id_str = tool_use_id.as_deref().unwrap_or("bash").to_string();

        let file_paths = Self::extract_file_paths(&hook_input, cwd);

        // Build metadata
        let mut metadata = HashMap::new();
        if let Some(ref tool_use_id) = tool_use_id {
            metadata.insert("tool_use_id".to_string(), tool_use_id.clone());
        }
        if let Some(ref thread_id) = thread_id {
            metadata.insert("thread_id".to_string(), thread_id.clone());
        }
        if let Ok(threads_path) = std::env::var("GIT_AI_AMP_THREADS_PATH")
            && !threads_path.trim().is_empty()
        {
            metadata.insert("__test_amp_threads_path".to_string(), threads_path);
        }

        // Determine session_id: thread_id preferred, falls back to tool_use_id
        let session_id = thread_id
            .clone()
            .or(tool_use_id.clone())
            .unwrap_or_else(|| "unknown".to_string());

        let context = PresetContext {
            agent_id: AgentId {
                tool: "amp".to_string(),
                id: session_id.clone(),
                model: "unknown".to_string(),
            },
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
            metadata,
        };

        let bash_command = hook_input
            .tool_input
            .as_ref()
            .and_then(|tool_input| {
                tool_input
                    .get("command")
                    .or_else(|| tool_input.get("cmd"))
                    .and_then(|v| v.as_str())
            })
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);
        let event = match (is_pre, is_bash) {
            (true, true) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id: tool_use_id_str,
                command: bash_command,
            }),
            (true, false) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                dirty_files: None,
                tool_use_id: tool_use_id.clone(),
            }),
            (false, true) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id: tool_use_id_str,
                command: bash_command,
                stream_source: None,
            }),
            (false, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                dirty_files: None,
                stream_source: None,
                tool_use_id: tool_use_id.clone(),
            }),
        };

        Ok(vec![event])
    }

    fn enrich_authorized_events(
        &self,
        hook_input: &str,
        events: &mut [ParsedHookEvent],
    ) -> Result<(), GitAiError> {
        let hook_input: AmpHookInput = parse::hook_json(hook_input)?;
        let Some(path) = super::amp_enrichment::resolve_transcript_path(
            hook_input.transcript_path.as_deref(),
            hook_input.thread_id.as_deref(),
            hook_input.tool_use_id.as_deref(),
        ) else {
            return Ok(());
        };
        let model = crate::operations::streams::model_extraction::extract_model(
            &path,
            crate::operations::streams::sweep::StreamFormat::AmpThreadJson,
            None,
        )
        .ok()
        .flatten()
        .unwrap_or_else(|| "unknown".to_string());

        for event in events {
            let Some(context) = event.preset_context_mut() else {
                continue;
            };
            context.agent_id.model = model.clone();
            context.metadata.insert(
                "transcript_path".to_string(),
                path.to_string_lossy().to_string(),
            );
            let source = StreamSource {
                path: path.clone(),
                format: StreamFormat::AmpThreadJson,
                session_id: generate_session_id(&context.external_session_id, "amp"),
                external_session_id: context.external_session_id.clone(),
                external_parent_session_id: None,
            };
            event.set_post_stream_source(source);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::commands::checkpoint_agent::presets::*;
    use serde_json::json;

    fn make_amp_input(event: &str, tool: &str) -> String {
        json!({
            "hook_event_name": event,
            "tool_name": tool,
            "thread_id": "T-thread-123",
            "tool_use_id": "tu-abc",
            "cwd": "/home/user/project",
            "tool_input": {"path": "src/main.rs"}
        })
        .to_string()
    }

    #[test]
    fn test_amp_pre_file_edit() {
        let input = make_amp_input("PreToolUse", "Write");
        let events = AmpPreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "amp");
                assert_eq!(e.context.external_session_id, "T-thread-123");
                assert_eq!(e.context.cwd, PathBuf::from("/home/user/project"));
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                assert_eq!(
                    e.context.metadata.get("tool_use_id").map(String::as_str),
                    Some("tu-abc")
                );
                assert_eq!(
                    e.context.metadata.get("thread_id").map(String::as_str),
                    Some("T-thread-123")
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_amp_post_file_edit() {
        let input = make_amp_input("PostToolUse", "Edit");
        let events = AmpPreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "amp");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                // No existing transcript file, so stream_source is None
                assert!(e.stream_source.is_none());
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_amp_pre_bash_call() {
        let input = make_amp_input("PreToolUse", "Bash");
        let events = AmpPreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "amp");
                assert_eq!(e.tool_use_id, "tu-abc");
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_amp_post_bash_call() {
        let input = make_amp_input("PostToolUse", "Bash");
        let events = AmpPreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "amp");
                assert_eq!(e.tool_use_id, "tu-abc");
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_amp_session_id_from_thread_id() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "thread_id": "T-thread-456",
            "cwd": "/tmp"
        })
        .to_string();
        let events = AmpPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.external_session_id, "T-thread-456");
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_amp_session_id_falls_back_to_tool_use_id() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "tool_use_id": "tu-fallback",
            "cwd": "/tmp"
        })
        .to_string();
        let events = AmpPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.external_session_id, "tu-fallback");
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_amp_edited_filepaths_takes_priority() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "cwd": "/home/user/project",
            "edited_filepaths": ["/home/user/project/src/edited.rs"],
            "tool_input": {"path": "src/from_tool_input.rs"}
        })
        .to_string();
        let events = AmpPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/edited.rs")]
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_amp_file_paths_from_tool_input_multiple_keys() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "cwd": "/project",
            "tool_input": {
                "filePath": "src/a.rs",
                "paths": ["src/b.rs", "src/c.rs"]
            }
        })
        .to_string();
        let events = AmpPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.file_paths.len(), 3);
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn parse_does_not_read_transcript_before_authorized_enrichment() {
        let temp = tempfile::tempdir().unwrap();
        let transcript_path = temp.path().join("thread.json");
        std::fs::write(
            &transcript_path,
            r#"{"messages":[{"usage":{"model":"model-from-disk"}}]}"#,
        )
        .unwrap();
        let input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "thread_id": "T-pure-parse",
            "cwd": "/project",
            "edited_filepaths": ["/project/main.rs"],
            "transcript_path": transcript_path,
        })
        .to_string();

        let mut events = AmpPreset.parse(&input, "t_test").unwrap();
        let ParsedHookEvent::PostFileEdit(event) = &events[0] else {
            panic!("Expected PostFileEdit");
        };
        assert_eq!(event.context.agent_id.model, "unknown");
        assert!(event.stream_source.is_none());
        assert!(!event.context.metadata.contains_key("transcript_path"));

        AmpPreset
            .enrich_authorized_events(&input, &mut events)
            .unwrap();
        let ParsedHookEvent::PostFileEdit(event) = &events[0] else {
            panic!("Expected PostFileEdit");
        };
        assert_eq!(event.context.agent_id.model, "model-from-disk");
        assert_eq!(
            event
                .stream_source
                .as_ref()
                .map(|source| source.path.as_path()),
            Some(transcript_path.as_path())
        );
        assert_eq!(
            event.context.metadata.get("transcript_path"),
            Some(&transcript_path.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn test_amp_ignores_read_only_and_unsupported_tools() {
        for hook_event in ["PreToolUse", "PostToolUse"] {
            for tool_name in ["Read", "Grep", "glob", "UnknownTool"] {
                let input = make_amp_input(hook_event, tool_name);
                let events = AmpPreset.parse(&input, "t_test").unwrap();
                assert!(
                    events.is_empty(),
                    "{hook_event} {tool_name} unexpectedly produced events"
                );
            }
        }
    }

    #[test]
    fn test_amp_legacy_payload_without_tool_name_still_checkpoints() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "thread_id": "T-thread-123",
            "tool_use_id": "tu-abc",
            "cwd": "/home/user/project",
            "tool_input": {"path": "src/main.rs"}
        })
        .to_string();
        let events = AmpPreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ParsedHookEvent::PostFileEdit(_)));
    }
}
