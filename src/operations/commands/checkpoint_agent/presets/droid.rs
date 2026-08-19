use super::parse;
use super::{AgentPreset, ParsedHookEvent, PresetContext, StreamFormat, StreamSource, claude_wire};
use crate::error::GitAiError;
use crate::model::authorship_log_serialization::generate_session_id;
use crate::model::working_log::AgentId;
use crate::operations::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct DroidPreset;

impl AgentPreset for DroidPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = parse::hook_json(hook_input)?;

        // session_id is optional — generate a fallback if not present
        let session_id =
            parse::optional_str_multi(&data, &["session_id", "sessionId"]).map(|s| s.to_string());
        let session_id = session_id.unwrap_or_else(|| {
            use std::time::{SystemTime, UNIX_EPOCH};
            format!(
                "droid-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis()
            )
        });

        let cwd = parse::required_str(&data, "cwd")?;

        let hook_event_name =
            parse::optional_str_multi(&data, &["hookEventName", "hook_event_name"]).ok_or_else(
                || GitAiError::PresetError("hookEventName not found in hook_input".to_string()),
            )?;

        // Extract tool_name and classify it
        let tool_name = parse::optional_str_multi(&data, &["tool_name", "toolName"]);
        let tool_class = bash_tool::classify_optional_tool(Agent::Droid, tool_name);
        if tool_class == ToolClass::Skip {
            return Ok(Vec::new());
        }

        // Extract file paths from tool_input
        let tool_input = data.get("tool_input").or_else(|| data.get("toolInput"));

        let mut file_paths: Vec<PathBuf> = tool_input
            .and_then(|ti| {
                ti.get("file_path")
                    .or_else(|| ti.get("filePath"))
                    .and_then(|v| v.as_str())
                    .map(|path| vec![parse::resolve_absolute(path, cwd)])
            })
            .unwrap_or_default();

        // For ApplyPatch, extract file paths from the patch text
        if file_paths.is_empty() && tool_name == Some("ApplyPatch") {
            let mut patch_paths = Vec::new();

            if let Some(ti) = tool_input {
                let patch_text = ti
                    .as_str()
                    .or_else(|| ti.get("patch").and_then(|v| v.as_str()));

                if let Some(text) = patch_text {
                    let mut raw_paths = Vec::new();
                    parse::collect_apply_patch_paths_from_text(text, &mut raw_paths);
                    patch_paths.extend(raw_paths.iter().map(|p| parse::resolve_absolute(p, cwd)));
                }
            }

            // For PostToolUse, also try parsing tool_response for file_path
            if patch_paths.is_empty()
                && hook_event_name == "PostToolUse"
                && let Some(tool_response) = data
                    .get("tool_response")
                    .or_else(|| data.get("toolResponse"))
            {
                let response_obj = if let Some(s) = tool_response.as_str() {
                    serde_json::from_str::<serde_json::Value>(s).ok()
                } else {
                    Some(tool_response.clone())
                };
                if let Some(obj) = response_obj
                    && let Some(path) = obj
                        .get("file_path")
                        .or_else(|| obj.get("filePath"))
                        .and_then(|v| v.as_str())
                {
                    patch_paths.push(parse::resolve_absolute(path, cwd));
                }
            }

            file_paths = patch_paths;
        }

        // Resolve transcript and settings paths
        let transcript_path =
            parse::optional_str_multi(&data, &["transcript_path", "transcriptPath"]);

        let (resolved_transcript_path, resolved_settings_path) = if let Some(tp) = transcript_path {
            let settings = tp.replace(".jsonl", ".settings.json");
            (tp.to_string(), settings)
        } else {
            let (jsonl_p, settings_p) = droid_session_paths(&session_id, cwd);
            (
                crate::operations::git::path_format::normalize_to_posix(&jsonl_p.to_string_lossy()),
                crate::operations::git::path_format::normalize_to_posix(
                    &settings_p.to_string_lossy(),
                ),
            )
        };

        // Determine if this is a bash tool invocation
        let is_bash = tool_class == ToolClass::Bash;

        let tool_use_id = parse::optional_str_multi(&data, &["tool_use_id", "toolUseId"])
            .unwrap_or("bash")
            .to_string();

        let mut metadata = HashMap::new();
        metadata.insert(
            "transcript_path".to_string(),
            resolved_transcript_path.clone(),
        );
        metadata.insert("settings_path".to_string(), resolved_settings_path);
        if let Some(name) = tool_name {
            metadata.insert("tool_name".to_string(), name.to_string());
        }

        let context = PresetContext {
            agent_id: AgentId {
                tool: "droid".to_string(),
                id: session_id.clone(),
                model: "unknown".to_string(),
            },
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
            metadata,
        };

        let stream_source = Some(StreamSource {
            path: PathBuf::from(&resolved_transcript_path),
            format: StreamFormat::DroidJsonl,
            session_id: generate_session_id(&context.external_session_id, "droid"),
            external_session_id: context.external_session_id.clone(),
            external_parent_session_id: None,
        });

        let bash_command = parse::bash_command_from_hook_input(&data);
        let is_pre = hook_event_name == "PreToolUse";

        Ok(vec![claude_wire::build_wire_event(
            is_pre,
            is_bash,
            context,
            tool_use_id,
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
            let Some(path) = context.metadata.get("settings_path") else {
                continue;
            };
            context.agent_id.model =
                crate::operations::streams::model_extraction::extract_model_from_droid_settings(
                    Path::new(path),
                )
                .ok()
                .flatten()
                .unwrap_or_else(|| "unknown".to_string());
        }

        Ok(())
    }
}

/// Derive JSONL and settings.json paths from a session_id and cwd.
/// Droid stores sessions at ~/.factory/sessions/{encoded_cwd}/{session_id}.jsonl
/// where encoded_cwd replaces '/' with '-'.
fn droid_session_paths(session_id: &str, cwd: &str) -> (PathBuf, PathBuf) {
    let encoded_cwd = cwd.replace('/', "-");
    let base = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".factory")
        .join("sessions")
        .join(&encoded_cwd);
    let jsonl_path = base.join(format!("{}.jsonl", session_id));
    let settings_path = base.join(format!("{}.settings.json", session_id));
    (jsonl_path, settings_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::commands::checkpoint_agent::presets::*;
    use serde_json::json;

    fn make_droid_hook_input(event: &str, tool: &str) -> String {
        json!({
            "cwd": "/home/user/project",
            "hookEventName": event,
            "tool_name": tool,
            "session_id": "droid-sess-1",
            "tool_use_id": "tu-1",
            "transcript_path": "/home/user/.factory/sessions/test.jsonl",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string()
    }

    #[test]
    fn test_droid_pre_file_edit() {
        let input = make_droid_hook_input("PreToolUse", "Write");
        let events = DroidPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "droid");
                assert_eq!(e.context.external_session_id, "droid-sess-1");
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
    fn test_droid_post_file_edit() {
        let input = make_droid_hook_input("PostToolUse", "Write");
        let events = DroidPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "droid");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                assert!(e.stream_source.is_some());
                if let Some(ts) = &e.stream_source {
                    assert_eq!(ts.format, StreamFormat::DroidJsonl);
                }
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_droid_pre_bash_call() {
        let input = make_droid_hook_input("PreToolUse", "Bash");
        let events = DroidPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "droid");
                assert_eq!(e.tool_use_id, "tu-1");
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_droid_post_bash_call() {
        let input = make_droid_hook_input("PostToolUse", "Bash");
        let events = DroidPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "droid");
                assert_eq!(e.tool_use_id, "tu-1");
                assert!(e.stream_source.is_some());
                if let Some(ts) = &e.stream_source {
                    assert_eq!(ts.format, StreamFormat::DroidJsonl);
                }
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_droid_apply_patch_extracts_paths() {
        let input = json!({
            "cwd": "/home/user/project",
            "hookEventName": "PreToolUse",
            "tool_name": "ApplyPatch",
            "session_id": "droid-sess-1",
            "transcript_path": "/home/user/.factory/sessions/test.jsonl",
            "tool_input": "*** Update File: src/main.rs\n--- old\n+++ new\n*** Add File: src/new.rs\n"
        })
        .to_string();
        let events = DroidPreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.file_paths.len(), 2);
                assert_eq!(
                    e.file_paths[0],
                    PathBuf::from("/home/user/project/src/main.rs")
                );
                assert_eq!(
                    e.file_paths[1],
                    PathBuf::from("/home/user/project/src/new.rs")
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_droid_apply_patch_from_patch_field() {
        let input = json!({
            "cwd": "/home/user/project",
            "hookEventName": "PreToolUse",
            "tool_name": "ApplyPatch",
            "session_id": "droid-sess-1",
            "transcript_path": "/home/user/.factory/sessions/test.jsonl",
            "tool_input": {"patch": "*** Update File: src/lib.rs\nsome diff content"}
        })
        .to_string();
        let events = DroidPreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/lib.rs")]
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_droid_post_apply_patch_fallback_to_tool_response() {
        let input = json!({
            "cwd": "/home/user/project",
            "hookEventName": "PostToolUse",
            "tool_name": "ApplyPatch",
            "session_id": "droid-sess-1",
            "transcript_path": "/home/user/.factory/sessions/test.jsonl",
            "tool_input": "no paths here",
            "tool_response": {"file_path": "/home/user/project/src/main.rs"}
        })
        .to_string();
        let events = DroidPreset.parse(&input, "t_test123456789a").unwrap();
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
    fn test_droid_session_id_fallback() {
        let input = json!({
            "cwd": "/home/user/project",
            "hookEventName": "PreToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "src/main.rs"},
            "transcript_path": "/home/user/.factory/sessions/test.jsonl"
        })
        .to_string();
        let events = DroidPreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert!(e.context.external_session_id.starts_with("droid-"));
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_droid_requires_cwd() {
        let input = json!({
            "hookEventName": "PreToolUse",
            "tool_name": "Write",
            "session_id": "droid-sess-1",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        let result = DroidPreset.parse(&input, "t_test123456789a");
        assert!(result.is_err());
    }

    #[test]
    fn test_droid_requires_hook_event_name() {
        let input = json!({
            "cwd": "/home/user/project",
            "tool_name": "Write",
            "session_id": "droid-sess-1",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        let result = DroidPreset.parse(&input, "t_test123456789a");
        assert!(result.is_err());
    }

    #[test]
    fn test_droid_camel_case_keys() {
        let input = json!({
            "cwd": "/home/user/project",
            "hookEventName": "PreToolUse",
            "toolName": "Write",
            "sessionId": "droid-sess-2",
            "toolUseId": "tu-2",
            "transcriptPath": "/home/user/.factory/sessions/test.jsonl",
            "toolInput": {"filePath": "src/main.rs"}
        })
        .to_string();
        let events = DroidPreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.external_session_id, "droid-sess-2");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_droid_metadata_includes_transcript_and_settings() {
        let input = make_droid_hook_input("PostToolUse", "Write");
        let events = DroidPreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert!(e.context.metadata.contains_key("transcript_path"));
                assert!(e.context.metadata.contains_key("settings_path"));
                assert!(e.context.metadata.contains_key("tool_name"));
                assert_eq!(
                    e.context.metadata.get("settings_path").unwrap(),
                    "/home/user/.factory/sessions/test.settings.json"
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_droid_derived_session_paths() {
        let input = json!({
            "cwd": "/home/user/project",
            "hookEventName": "PostToolUse",
            "tool_name": "Write",
            "session_id": "my-session",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        let events = DroidPreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                let tp = e.context.metadata.get("transcript_path").unwrap();
                assert!(tp.contains(".factory/sessions/"));
                assert!(tp.ends_with("my-session.jsonl"));
                let sp = e.context.metadata.get("settings_path").unwrap();
                assert!(sp.ends_with("my-session.settings.json"));
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_droid_session_paths_helper() {
        let (jsonl, settings) = droid_session_paths("test-sess", "/home/user/project");
        let jsonl_s =
            crate::operations::git::path_format::normalize_to_posix(&jsonl.to_string_lossy());
        let settings_s =
            crate::operations::git::path_format::normalize_to_posix(&settings.to_string_lossy());
        assert!(jsonl_s.contains(".factory/sessions/"));
        assert!(jsonl_s.ends_with("test-sess.jsonl"));
        assert!(settings_s.ends_with("test-sess.settings.json"));
    }

    #[test]
    fn parse_does_not_read_model_before_authorized_enrichment() {
        let temp = tempfile::tempdir().unwrap();
        let transcript_path = temp.path().join("session.jsonl");
        let settings_path = temp.path().join("session.settings.json");
        std::fs::write(&settings_path, r#"{"model":"model-from-disk"}"#).unwrap();
        let input = json!({
            "cwd": "/project",
            "hookEventName": "PostToolUse",
            "tool_name": "Write",
            "session_id": "session-pure",
            "transcript_path": transcript_path,
            "tool_input": {"file_path": "/project/main.rs"},
        })
        .to_string();

        let mut events = DroidPreset.parse(&input, "t_test").unwrap();
        let ParsedHookEvent::PostFileEdit(event) = &events[0] else {
            panic!("Expected PostFileEdit");
        };
        assert_eq!(event.context.agent_id.model, "unknown");

        DroidPreset
            .enrich_authorized_events(&input, &mut events)
            .unwrap();
        let ParsedHookEvent::PostFileEdit(event) = &events[0] else {
            panic!("Expected PostFileEdit");
        };
        assert_eq!(event.context.agent_id.model, "model-from-disk");
    }

    #[test]
    fn test_droid_ignores_read_only_and_unsupported_tools() {
        for hook_event in ["PreToolUse", "PostToolUse"] {
            for tool_name in ["Read", "Grep", "Glob", "Task", "UnknownTool"] {
                let input = make_droid_hook_input(hook_event, tool_name);
                let events = DroidPreset.parse(&input, "t_test123456789a").unwrap();
                assert!(
                    events.is_empty(),
                    "{hook_event} {tool_name} unexpectedly produced events"
                );
            }
        }
    }

    #[test]
    fn test_droid_legacy_payload_without_tool_name_still_checkpoints() {
        let input = json!({
            "cwd": "/home/user/project",
            "hookEventName": "PostToolUse",
            "session_id": "droid-sess-1",
            "transcript_path": "/home/user/.factory/sessions/test.jsonl",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        let events = DroidPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ParsedHookEvent::PostFileEdit(_)));
    }
}
