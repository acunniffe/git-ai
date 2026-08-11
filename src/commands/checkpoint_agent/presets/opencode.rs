use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext, StreamFormat, StreamSource,
};
use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
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

    fn resolve_stream_source(session_id: &str) -> Option<(StreamSource, PathBuf)> {
        let opencode_paths = if let Ok(test_path) = std::env::var("GIT_AI_OPENCODE_STORAGE_PATH") {
            vec![PathBuf::from(test_path)]
        } else {
            Self::opencode_data_paths().ok()?
        };

        for opencode_path in opencode_paths {
            let Some(db_path) = Self::resolve_sqlite_db_path(&opencode_path) else {
                continue;
            };
            let parent_id = Self::lookup_parent_session(&db_path, session_id);
            return Some((
                StreamSource {
                    path: db_path,
                    format: StreamFormat::OpenCodeSqlite,
                    session_id: generate_session_id(session_id, "opencode"),
                    external_session_id: session_id.to_string(),
                    external_parent_session_id: parent_id,
                },
                opencode_path,
            ));
        }

        None
    }

    fn lookup_parent_session(db_path: &Path, session_id: &str) -> Option<String> {
        let conn = crate::streams::agents::opencode::open_sqlite_readonly(db_path).ok()?;
        conn.query_row(
            "SELECT parent_id FROM session WHERE id = ?",
            [session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    }

    fn opencode_data_paths() -> Result<Vec<PathBuf>, GitAiError> {
        #[cfg(target_os = "macos")]
        {
            let home = dirs::home_dir().ok_or_else(|| {
                GitAiError::Generic("Could not determine home directory".to_string())
            })?;
            Ok(vec![home.join(".local").join("share").join("opencode")])
        }

        #[cfg(target_os = "linux")]
        {
            if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
                Ok(vec![PathBuf::from(xdg_data).join("opencode")])
            } else {
                let home = dirs::home_dir().ok_or_else(|| {
                    GitAiError::Generic("Could not determine home directory".to_string())
                })?;
                Ok(vec![home.join(".local").join("share").join("opencode")])
            }
        }

        #[cfg(target_os = "windows")]
        {
            Ok(Self::windows_data_paths(
                crate::mdm::utils::home_dir(),
                std::env::var_os("APPDATA").map(PathBuf::from),
                std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
            ))
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Err(GitAiError::PresetError(
                "OpenCode storage path not supported on this platform".to_string(),
            ))
        }
    }

    #[cfg(any(target_os = "windows", test))]
    fn windows_data_paths(
        home: PathBuf,
        app_data: Option<PathBuf>,
        local_app_data: Option<PathBuf>,
    ) -> Vec<PathBuf> {
        let mut paths = vec![home.join(".local").join("share").join("opencode")];
        paths.extend(app_data.into_iter().map(|path| path.join("opencode")));
        paths.extend(local_app_data.into_iter().map(|path| path.join("opencode")));
        paths
    }

    fn resolve_sqlite_db_path(path: &Path) -> Option<PathBuf> {
        if path.is_file() {
            return path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| *name == "opencode.db")
                .map(|_| path.to_path_buf());
        }

        if !path.is_dir() {
            return None;
        }

        let direct_db = path.join("opencode.db");
        if direct_db.exists() {
            return Some(direct_db);
        }

        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "storage")
        {
            let sibling_db = path.parent()?.join("opencode.db");
            if sibling_db.exists() {
                return Some(sibling_db);
            }
        }

        None
    }
}

impl AgentPreset for OpenCodePreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let hook_input: OpenCodeHookInput = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

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

        // Resolve transcript source
        let transcript_result = Self::resolve_stream_source(&session_id);

        let extracted_model = transcript_result.as_ref().and_then(|(ts, _)| {
            crate::streams::model_extraction::extract_model(
                &ts.path,
                crate::streams::sweep::StreamFormat::OpenCodeSqlite,
                Some(session_id.as_str()),
            )
            .ok()
            .flatten()
        });

        let context = PresetContext {
            agent_id: AgentId {
                tool: "opencode".to_string(),
                id: session_id.clone(),
                model: extracted_model.unwrap_or_else(|| "unknown".to_string()),
            },
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(&cwd),
            metadata,
        };

        let stream_source = transcript_result.map(|(source, _)| source);

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
                tool_use_id: Some(tool_use_id_str),
            }),
            (false, true) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id: tool_use_id_str,
                command: bash_command,
                stream_source,
            }),
            (false, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                dirty_files: None,
                stream_source,
                tool_use_id: Some(tool_use_id_str),
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
    fn test_windows_opencode_data_paths_prefer_xdg_layout() {
        let home = PathBuf::from(r"C:\Users\test");
        let app_data = PathBuf::from(r"C:\Users\test\AppData\Roaming");
        let local_app_data = PathBuf::from(r"C:\Users\test\AppData\Local");

        assert_eq!(
            OpenCodePreset::windows_data_paths(
                home.clone(),
                Some(app_data.clone()),
                Some(local_app_data.clone()),
            ),
            vec![
                home.join(".local").join("share").join("opencode"),
                app_data.join("opencode"),
                local_app_data.join("opencode"),
            ]
        );
    }
}
