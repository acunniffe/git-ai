use super::{AgentPreset, ParsedHookEvent, PresetContext, claude_wire};
use crate::error::GitAiError;
use crate::model::working_log::AgentId;
use crate::operations::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct OpenCodePreset;

#[derive(Debug, Deserialize)]
struct OpenCodeHookInput {
    hook_event_name: String,
    session_id: String,
    cwd: String,
    tool_input: Option<serde_json::Value>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default, alias = "toolUseId")]
    tool_use_id: Option<String>,
}

impl OpenCodePreset {
    pub(crate) fn extract_filepaths_from_tool_input(
        tool_input: Option<&serde_json::Value>,
        cwd: &str,
    ) -> Vec<PathBuf> {
        let mut raw_paths = Vec::new();

        if let Some(value) = tool_input {
            Self::collect_tool_paths(value, &mut raw_paths);
        }

        let mut normalized_paths = Vec::new();
        for raw in raw_paths {
            if let Some(path) = Self::normalize_hook_path(&raw, cwd) {
                let pb = PathBuf::from(&path);
                if !normalized_paths.contains(&pb) {
                    normalized_paths.push(pb);
                }
            }
        }

        normalized_paths
    }

    fn collect_tool_paths(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, val) in map {
                    let key_lower = key.to_ascii_lowercase();
                    let is_single_path_key = key_lower == "file_path"
                        || key_lower == "filepath"
                        || key_lower == "path"
                        || key_lower == "fspath";

                    let is_multi_path_key = key_lower == "files"
                        || key_lower == "filepaths"
                        || key_lower == "file_paths";

                    if is_single_path_key {
                        if let Some(path) = val.as_str() {
                            out.push(path.to_string());
                        }
                    } else if is_multi_path_key {
                        match val {
                            serde_json::Value::String(path) => out.push(path.to_string()),
                            serde_json::Value::Array(paths) => {
                                for path_value in paths {
                                    if let Some(path) = path_value.as_str() {
                                        out.push(path.to_string());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                    Self::collect_tool_paths(val, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    Self::collect_tool_paths(item, out);
                }
            }
            serde_json::Value::String(s) => {
                if s.starts_with("file://") {
                    out.push(s.to_string());
                }
                super::parse::collect_apply_patch_paths_from_text(s, out);
            }
            _ => {}
        }
    }

    fn normalize_hook_path(raw_path: &str, cwd: &str) -> Option<String> {
        let trimmed = raw_path.trim();
        if trimmed.is_empty() {
            return None;
        }

        let path_without_scheme = trimmed
            .strip_prefix("file://localhost")
            .or_else(|| trimmed.strip_prefix("file://"))
            .unwrap_or(trimmed);

        let path = Path::new(path_without_scheme);
        let joined = if path.is_absolute()
            || path_without_scheme.starts_with("\\\\")
            || path_without_scheme
                .as_bytes()
                .get(1)
                .map(|b| *b == b':')
                .unwrap_or(false)
        {
            PathBuf::from(path_without_scheme)
        } else {
            Path::new(cwd).join(path_without_scheme)
        };

        Some(joined.to_string_lossy().replace('\\', "/"))
    }
}

impl AgentPreset for OpenCodePreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let hook_input: OpenCodeHookInput = super::parse::hook_json(hook_input)?;

        let is_bash = hook_input
            .tool_name
            .as_deref()
            .map(|name| bash_tool::classify_tool(Agent::OpenCode, name) == ToolClass::Bash)
            .unwrap_or(false);

        let is_pre = hook_input.hook_event_name == "PreToolUse";

        let OpenCodeHookInput {
            hook_event_name: _,
            session_id,
            cwd,
            tool_input,
            tool_name: _,
            tool_use_id,
        } = hook_input;

        let file_paths = Self::extract_filepaths_from_tool_input(tool_input.as_ref(), &cwd);
        let bash_command = tool_input
            .as_ref()
            .and_then(|value| {
                value
                    .get("command")
                    .or_else(|| value.get("cmd"))
                    .and_then(|v| v.as_str())
            })
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);
        let tool_use_id_str = tool_use_id.as_deref().unwrap_or("bash").to_string();

        // Build metadata
        let mut metadata = HashMap::new();
        metadata.insert("session_id".to_string(), session_id.clone());
        if let Ok(test_path) = std::env::var("GIT_AI_OPENCODE_STORAGE_PATH") {
            metadata.insert("__test_storage_path".to_string(), test_path);
        }

        let context = PresetContext {
            agent_id: AgentId {
                tool: "opencode".to_string(),
                id: session_id.clone(),
                model: "unknown".to_string(),
            },
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(&cwd),
            metadata,
        };

        Ok(vec![claude_wire::build_wire_event(
            is_pre,
            is_bash,
            context,
            tool_use_id_str,
            bash_command,
            file_paths,
            None,
            None,
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
            let Some((source, model)) =
                super::opencode_enrichment::stream_source_and_model(&context.external_session_id)
            else {
                continue;
            };
            context.agent_id.model = model;
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

    fn make_opencode_input(event: &str, tool: &str) -> String {
        json!({
            "hook_event_name": event,
            "session_id": "oc-sess-123",
            "cwd": "/home/user/project",
            "tool_name": tool,
            "tool_use_id": "tu-1",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string()
    }

    #[test]
    fn test_opencode_pre_file_edit() {
        let input = make_opencode_input("PreToolUse", "edit");
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "opencode");
                assert_eq!(e.context.external_session_id, "oc-sess-123");
                assert_eq!(e.context.cwd, PathBuf::from("/home/user/project"));
                assert!(!e.file_paths.is_empty());
                assert_eq!(
                    e.context.metadata.get("session_id").map(String::as_str),
                    Some("oc-sess-123")
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_opencode_post_file_edit() {
        let input = make_opencode_input("PostToolUse", "write");
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "opencode");
                // Transcript source depends on whether the storage path exists
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_opencode_pre_bash_call() {
        let input = make_opencode_input("PreToolUse", "bash");
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "opencode");
                assert_eq!(e.tool_use_id, "tu-1");
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_opencode_post_bash_call() {
        let input = make_opencode_input("PostToolUse", "shell");
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "opencode");
                assert_eq!(e.tool_use_id, "tu-1");
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_opencode_extracts_file_paths_from_tool_input() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-1",
            "cwd": "/project",
            "tool_name": "edit",
            "tool_input": {
                "file_path": "src/main.rs",
                "fspath": "/project/src/lib.rs"
            }
        })
        .to_string();
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert!(!e.file_paths.is_empty());
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_opencode_normalize_hook_path_absolute() {
        assert_eq!(
            OpenCodePreset::normalize_hook_path("/home/user/file.rs", "/project"),
            Some("/home/user/file.rs".to_string())
        );
    }

    #[test]
    fn test_opencode_normalize_hook_path_relative() {
        assert_eq!(
            OpenCodePreset::normalize_hook_path("src/main.rs", "/project"),
            Some("/project/src/main.rs".to_string())
        );
    }

    #[test]
    fn test_opencode_normalize_hook_path_file_uri() {
        assert_eq!(
            OpenCodePreset::normalize_hook_path("file:///home/user/file.rs", "/project"),
            Some("/home/user/file.rs".to_string())
        );
    }

    #[test]
    fn test_opencode_normalize_hook_path_empty() {
        assert_eq!(OpenCodePreset::normalize_hook_path("", "/project"), None);
    }

    #[test]
    fn test_opencode_collect_apply_patch_paths() {
        let mut out = Vec::new();
        super::super::parse::collect_apply_patch_paths_from_text(
            "*** Update File: src/main.rs\n*** Add File: src/new.rs",
            &mut out,
        );
        assert_eq!(out, vec!["src/main.rs", "src/new.rs"]);
    }

    #[test]
    fn test_opencode_extracts_file_paths_from_patch_key() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-1",
            "cwd": "/project",
            "tool_name": "edit",
            "tool_input": {
                "patch": "*** Update File: /project/src/main.rs\n@@ old\n+new\n"
            }
        })
        .to_string();
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.file_paths, vec![PathBuf::from("/project/src/main.rs")]);
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_opencode_default_tool_use_id() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "sess-1",
            "cwd": "/project",
            "tool_name": "bash"
        })
        .to_string();
        let events = OpenCodePreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.tool_use_id, "bash");
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn parse_does_not_open_storage_before_authorized_enrichment() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("opencode.db");
        let connection =
            crate::model::repository::sqlite::open_with_memory_limits(&db_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT);
                 CREATE TABLE message (id TEXT, session_id TEXT, data TEXT);
                 INSERT INTO session VALUES ('session-pure', 'parent-session');
                 INSERT INTO message VALUES (
                     'message-1',
                     'session-pure',
                     '{\"model\":{\"modelID\":\"model-from-disk\"}}'
                 );",
            )
            .unwrap();
        drop(connection);
        unsafe {
            std::env::set_var("GIT_AI_OPENCODE_STORAGE_PATH", temp.path());
        }
        let input = json!({
            "hook_event_name": "PostToolUse",
            "session_id": "session-pure",
            "cwd": "/project",
            "tool_name": "edit",
            "tool_input": {"file_path": "/project/main.rs"},
        })
        .to_string();

        let mut events = OpenCodePreset.parse(&input, "t_test").unwrap();
        let ParsedHookEvent::PostFileEdit(event) = &events[0] else {
            panic!("Expected PostFileEdit");
        };
        assert_eq!(event.context.agent_id.model, "unknown");
        assert!(event.stream_source.is_none());

        OpenCodePreset
            .enrich_authorized_events(&input, &mut events)
            .unwrap();
        unsafe {
            std::env::remove_var("GIT_AI_OPENCODE_STORAGE_PATH");
        }
        let ParsedHookEvent::PostFileEdit(event) = &events[0] else {
            panic!("Expected PostFileEdit");
        };
        assert_eq!(event.context.agent_id.model, "model-from-disk");
        let source = event.stream_source.as_ref().expect("stream source");
        assert_eq!(source.path, db_path);
        assert_eq!(
            source.external_parent_session_id.as_deref(),
            Some("parent-session")
        );
    }
}
