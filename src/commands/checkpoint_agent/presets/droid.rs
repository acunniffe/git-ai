use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext, PresetRuntimeContext, StreamFormat, StreamSource,
};
use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct DroidPreset;

const RUNTIME_PROFILE_ROOT_KEY: &str = "_git_ai_agent_profile_root";

impl AgentPreset for DroidPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let tool_name = parse::optional_str_multi(&data, &["tool_name", "toolName"]);
        let tool_class = tool_name
            .map(|name| bash_tool::classify_tool(Agent::Droid, name))
            // Preserve payloads from older Droid integrations that omitted tool_name.
            .unwrap_or(ToolClass::FileEdit);
        if tool_class == ToolClass::Skip {
            return Ok(Vec::new());
        }

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
            let profile_root = parse::optional_str(&data, RUNTIME_PROFILE_ROOT_KEY).map(Path::new);
            let (jsonl_p, settings_p) = droid_session_paths_at(profile_root, &session_id, cwd);
            (
                crate::utils::normalize_to_posix(&jsonl_p.to_string_lossy()),
                crate::utils::normalize_to_posix(&settings_p.to_string_lossy()),
            )
        };

        // Determine if this is a bash tool invocation
        let is_bash = tool_class == ToolClass::Bash;

        let tool_use_id = parse::optional_str_multi(&data, &["tool_use_id", "toolUseId"])
            .unwrap_or("bash")
            .to_string();

        // Build metadata
        let extracted_model = crate::streams::model_extraction::extract_model_from_droid_settings(
            Path::new(&resolved_settings_path),
        )
        .ok()
        .flatten()
        .unwrap_or_else(|| "unknown".to_string());

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
                model: extracted_model,
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

        // PreToolUse
        if hook_event_name == "PreToolUse" {
            if is_bash {
                return Ok(vec![ParsedHookEvent::PreBashCall(PreBashCall {
                    context,
                    tool_use_id,
                    command: bash_command,
                })]);
            }
            return Ok(vec![ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                dirty_files: None,
                tool_use_id: Some(tool_use_id.clone()),
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

        Ok(vec![ParsedHookEvent::PostFileEdit(PostFileEdit {
            context,
            file_paths,
            dirty_files: None,
            stream_source,
            tool_use_id: Some(tool_use_id.clone()),
        })])
    }

    fn parse_with_context(
        &self,
        hook_input: &str,
        trace_id: &str,
        runtime_context: &PresetRuntimeContext,
    ) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let Some(profile_root) = runtime_context.agent_profile_root.as_ref() else {
            return self.parse(hook_input, trace_id);
        };
        let mut data: serde_json::Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;
        let object = data.as_object_mut().ok_or_else(|| {
            GitAiError::PresetError("Droid hook_input must be a JSON object".to_string())
        })?;
        object.insert(
            RUNTIME_PROFILE_ROOT_KEY.to_string(),
            serde_json::Value::String(profile_root.to_string_lossy().into_owned()),
        );
        self.parse(&data.to_string(), trace_id)
    }
}

/// Derive JSONL and settings.json paths from a session_id and cwd.
/// Droid stores sessions at ~/.factory/sessions/{encoded_cwd}/{session_id}.jsonl
/// where encoded_cwd replaces '/' with '-'.
#[cfg(test)]
fn droid_session_paths(session_id: &str, cwd: &str) -> (PathBuf, PathBuf) {
    droid_session_paths_at(None, session_id, cwd)
}

fn droid_session_paths_at(
    profile_root: Option<&Path>,
    session_id: &str,
    cwd: &str,
) -> (PathBuf, PathBuf) {
    let encoded_cwd = cwd.replace('/', "-");
    let profile_root = profile_root.map(Path::to_path_buf).unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(".factory")
    });
    let base = profile_root.join("sessions").join(&encoded_cwd);
    let jsonl_path = base.join(format!("{}.jsonl", session_id));
    let settings_path = base.join(format!("{}.settings.json", session_id));
    (jsonl_path, settings_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::checkpoint_agent::presets::*;
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
    fn test_ignored_droid_hook_produces_no_checkpoint_requests() {
        let input = make_droid_hook_input("PostToolUse", "Read");
        let requests = crate::commands::checkpoint_agent::orchestrator::execute_preset_checkpoint(
            "droid", &input,
        )
        .unwrap();
        assert!(requests.is_empty());
    }

    #[test]
    fn test_droid_preserves_all_mutating_tools() {
        for tool_name in ["ApplyPatch", "Edit", "Write", "Create"] {
            let pre = make_droid_hook_input("PreToolUse", tool_name);
            assert!(matches!(
                DroidPreset.parse(&pre, "t_test123456789a").unwrap()[..],
                [ParsedHookEvent::PreFileEdit(_)]
            ));

            let post = make_droid_hook_input("PostToolUse", tool_name);
            assert!(matches!(
                DroidPreset.parse(&post, "t_test123456789a").unwrap()[..],
                [ParsedHookEvent::PostFileEdit(_)]
            ));
        }

        for tool_name in ["Bash", "Execute"] {
            let pre = make_droid_hook_input("PreToolUse", tool_name);
            assert!(matches!(
                DroidPreset.parse(&pre, "t_test123456789a").unwrap()[..],
                [ParsedHookEvent::PreBashCall(_)]
            ));

            let post = make_droid_hook_input("PostToolUse", tool_name);
            assert!(matches!(
                DroidPreset.parse(&post, "t_test123456789a").unwrap()[..],
                [ParsedHookEvent::PostBashCall(_)]
            ));
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
    fn test_droid_runtime_profile_root_controls_derived_session_path() {
        let profile_root = PathBuf::from("/profiles/factory-work");
        let input = json!({
            "cwd": "/home/user/project",
            "hookEventName": "PostToolUse",
            "tool_name": "Write",
            "session_id": "my-session",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();

        let events = DroidPreset
            .parse_with_context(
                &input,
                "t_test123456789a",
                &PresetRuntimeContext {
                    agent_profile_root: Some(profile_root.clone()),
                },
            )
            .unwrap();

        match &events[0] {
            ParsedHookEvent::PostFileEdit(event) => assert!(
                Path::new(event.context.metadata.get("transcript_path").unwrap())
                    .starts_with(profile_root)
            ),
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_droid_session_paths_helper() {
        let (jsonl, settings) = droid_session_paths("test-sess", "/home/user/project");
        let jsonl_s = crate::utils::normalize_to_posix(&jsonl.to_string_lossy());
        let settings_s = crate::utils::normalize_to_posix(&settings.to_string_lossy());
        assert!(jsonl_s.contains(".factory/sessions/"));
        assert!(jsonl_s.ends_with("test-sess.jsonl"));
        assert!(settings_s.ends_with("test-sess.settings.json"));
    }
}
