use super::opencode::OpenCodePreset;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct ClinePreset;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
#[serde(rename_all = "camelCase")]
struct ClineHookInput {
    hook_name: String,
    cline_version: String,
    timestamp: Option<String>,
    task_id: String,
    workspace_roots: Vec<String>,
    user_id: Option<String>,
    #[serde(default)]
    model: Option<ClineModel>,
    #[serde(rename = "preToolUse")]
    pre_tool_use: Option<ClineToolInvocation>,
    #[serde(rename = "postToolUse")]
    post_tool_use: Option<ClineToolInvocation>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClineModel {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    slug: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
#[serde(rename_all = "camelCase")]
struct ClineToolInvocation {
    tool_name: String,
    #[serde(default)]
    parameters: Value,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    execution_time_ms: Option<u64>,
}

impl ClinePreset {
    fn normalize_parameters(parameters: &Value) -> Value {
        let Some(obj) = parameters.as_object() else {
            return Value::Object(serde_json::Map::new());
        };

        let mut out = serde_json::Map::new();
        for (key, value) in obj {
            let key_lower = key.to_ascii_lowercase();
            if is_multi_value_key(&key_lower)
                && let Some(s) = value.as_str()
                && let Ok(parsed) = serde_json::from_str::<Value>(s)
            {
                out.insert(key.clone(), parsed);
                continue;
            }
            out.insert(key.clone(), value.clone());
        }
        Value::Object(out)
    }

    fn filter_content_keys(parameters: &Value) -> Value {
        let Some(obj) = parameters.as_object() else {
            return Value::Object(serde_json::Map::new());
        };

        const CONTENT_KEYS: &[&str] = &["content", "new_text", "old_text", "newtext", "oldtext"];
        let mut out = serde_json::Map::new();
        for (key, value) in obj {
            if !CONTENT_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
                out.insert(key.clone(), value.clone());
            }
        }
        Value::Object(out)
    }

    fn extract_paths_from_parameters(parameters: &Value, cwd: &str) -> Vec<PathBuf> {
        let Some(paths) = parameters.get("paths") else {
            return vec![];
        };

        let arr = if let Some(a) = paths.as_array() {
            a
        } else if let Some(s) = paths.as_str() {
            if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(s) {
                return Self::extract_paths_from_value_array(&a, cwd);
            }
            return vec![];
        } else {
            return vec![];
        };

        Self::extract_paths_from_value_array(arr, cwd)
    }

    fn extract_paths_from_value_array(arr: &[Value], cwd: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for item in arr {
            if let Some(s) = item.as_str() {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    out.push(Self::resolve_hook_path(trimmed, cwd));
                }
            } else if let Some(obj) = item.as_object()
                && let Some(s) = obj.get("path").and_then(|v| v.as_str())
            {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    out.push(Self::resolve_hook_path(trimmed, cwd));
                }
            }
        }
        out
    }

    fn resolve_hook_path(raw: &str, cwd: &str) -> PathBuf {
        let trimmed = raw.trim();
        let path = Path::new(trimmed);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            Path::new(cwd).join(path)
        }
    }

    fn extract_bash_command(parameters: &Value) -> Option<String> {
        if let Some(cmd) = parameters
            .get("command")
            .or_else(|| parameters.get("cmd"))
            .and_then(|v| v.as_str())
        {
            let trimmed = cmd.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }

        if let Some(commands) = parameters.get("commands") {
            return Self::extract_bash_commands_value(commands);
        }

        None
    }

    fn extract_bash_commands_value(commands: &Value) -> Option<String> {
        if let Some(arr) = commands.as_array() {
            let mut cmds = Vec::new();
            for item in arr {
                if let Some(s) = item.as_str() {
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        cmds.push(trimmed.to_string());
                    }
                } else if let Some(obj) = item.as_object()
                    && let Some(s) = obj
                        .get("command")
                        .or_else(|| obj.get("cmd"))
                        .and_then(|v| v.as_str())
                {
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        cmds.push(trimmed.to_string());
                    }
                }
            }
            if !cmds.is_empty() {
                return Some(cmds.join("; "));
            }
        } else if let Some(s) = commands.as_str() {
            if let Ok(parsed) = serde_json::from_str::<Value>(s)
                && parsed.is_array()
            {
                return Self::extract_bash_commands_value(&parsed);
            }
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        None
    }

    fn resolve_model(model: Option<&ClineModel>) -> String {
        model
            .and_then(|m| {
                if !m.slug.is_empty() {
                    Some(m.slug.clone())
                } else if !m.provider.is_empty() {
                    Some(m.provider.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "unknown".to_string())
    }

    fn deterministic_tool_use_id(task_id: &str, tool_name: &str, parameters: &Value) -> String {
        let mut filtered = parameters.clone();
        if let Value::Object(map) = &mut filtered {
            map.remove("result");
            map.remove("success");
            map.remove("execution_time_ms");
            map.remove("executionTimeMs");
        }

        let mut hasher = Sha256::new();
        hasher.update(task_id.as_bytes());
        hasher.update(b":");
        hasher.update(tool_name.as_bytes());
        hasher.update(b":");
        hasher.update(filtered.to_string().as_bytes());
        let hash = hasher.finalize();
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        hex[..16.min(hex.len())].to_string()
    }
}

fn is_multi_value_key(key: &str) -> bool {
    matches!(
        key,
        "files" | "file_paths" | "filepaths" | "paths" | "commands"
    )
}

impl AgentPreset for ClinePreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let input: ClineHookInput = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        if !matches!(input.hook_name.as_str(), "PreToolUse" | "PostToolUse") {
            return Ok(vec![]);
        }

        let tool = input
            .pre_tool_use
            .as_ref()
            .or(input.post_tool_use.as_ref())
            .ok_or_else(|| {
                GitAiError::PresetError(
                    "Cline hook input must contain preToolUse or postToolUse".to_string(),
                )
            })?;

        let tool_class = bash_tool::classify_tool(Agent::Cline, &tool.tool_name);
        if tool_class == ToolClass::Skip {
            return Ok(vec![]);
        }

        let cwd = input.workspace_roots.first().cloned().ok_or_else(|| {
            GitAiError::PresetError("Cline hook input workspaceRoots is empty".to_string())
        })?;

        let parameters = Self::normalize_parameters(&tool.parameters);
        let filtered_for_paths = Self::filter_content_keys(&parameters);
        let mut file_paths =
            OpenCodePreset::extract_filepaths_from_tool_input(Some(&filtered_for_paths), &cwd);
        for path in Self::extract_paths_from_parameters(&parameters, &cwd) {
            if !file_paths.contains(&path) {
                file_paths.push(path);
            }
        }
        let bash_command = Self::extract_bash_command(&parameters);

        let model = Self::resolve_model(input.model.as_ref());
        let tool_use_id =
            Self::deterministic_tool_use_id(&input.task_id, &tool.tool_name, &parameters);

        let mut metadata = HashMap::new();
        metadata.insert("session_id".to_string(), input.task_id.clone());
        metadata.insert("tool_name".to_string(), tool.tool_name.clone());

        let context = PresetContext {
            agent_id: AgentId {
                tool: "cline".to_string(),
                id: input.task_id.clone(),
                model,
            },
            external_session_id: input.task_id.clone(),
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
            metadata,
        };

        let is_pre = input.hook_name == "PreToolUse";
        let event = match (is_pre, tool_class) {
            (true, ToolClass::Bash) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id,
                command: bash_command,
            }),
            (true, ToolClass::FileEdit) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                dirty_files: None,
                tool_use_id: Some(tool_use_id),
            }),
            (false, ToolClass::Bash) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id,
                command: bash_command,
                stream_source: None,
            }),
            (false, ToolClass::FileEdit) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                dirty_files: None,
                stream_source: None,
                tool_use_id: Some(tool_use_id),
            }),
            _ => return Ok(vec![]),
        };

        Ok(vec![event])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::checkpoint_agent::presets::*;
    use serde_json::json;

    fn make_cline_input(event: &str, tool: &str, parameters: Value) -> String {
        json!({
            "hookName": event,
            "clineVersion": "3.0.0",
            "taskId": "cline-task-123",
            "workspaceRoots": ["/home/user/project"],
            "userId": "user-1",
            "model": { "provider": "anthropic", "slug": "claude-sonnet-4-6" },
            "preToolUse": {
                "toolName": tool,
                "parameters": parameters
            }
        })
        .to_string()
    }

    fn make_post_cline_input(tool: &str, parameters: Value, result: &str) -> String {
        json!({
            "hookName": "PostToolUse",
            "clineVersion": "3.0.0",
            "taskId": "cline-task-123",
            "workspaceRoots": ["/home/user/project"],
            "model": { "provider": "anthropic", "slug": "claude-sonnet-4-6" },
            "postToolUse": {
                "toolName": tool,
                "parameters": parameters,
                "result": result,
                "success": true,
                "executionTimeMs": 123
            }
        })
        .to_string()
    }

    #[test]
    fn test_cline_pre_file_edit_editor() {
        let input = make_cline_input(
            "PreToolUse",
            "editor",
            json!({ "path": "src/main.rs", "old_text": "old", "new_text": "new" }),
        );
        let events = ClinePreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "cline");
                assert_eq!(e.context.external_session_id, "cline-task-123");
                assert_eq!(e.context.cwd, PathBuf::from("/home/user/project"));
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                assert_eq!(e.context.agent_id.model, "claude-sonnet-4-6");
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_cline_post_file_edit_apply_patch() {
        let input = make_post_cline_input(
            "apply_patch",
            json!({
                "input": "*** Update File: src/main.rs\n@@ old\n+new\n"
            }),
            "done",
        );
        let events = ClinePreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "cline");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                assert!(e.stream_source.is_none());
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_cline_post_file_edit_write_to_file() {
        let input = make_post_cline_input(
            "write_to_file",
            json!({
                "path": "src/lib.rs",
                "content": "fn main() {}"
            }),
            "done",
        );
        let events = ClinePreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/lib.rs")]
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_cline_pre_bash_execute_command() {
        let input = make_cline_input(
            "PreToolUse",
            "execute_command",
            json!({ "command": "cargo test" }),
        );
        let events = ClinePreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "cline");
                assert_eq!(e.command.as_deref(), Some("cargo test"));
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_cline_pre_bash_run_commands_array() {
        let input = make_cline_input(
            "PreToolUse",
            "run_commands",
            json!({ "commands": ["echo a", { "command": "echo b", "cwd": "/tmp" }] }),
        );
        let events = ClinePreset.parse(&input, "t_test").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.command.as_deref(), Some("echo a; echo b"));
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_cline_skips_unsupported_tool() {
        let input = make_cline_input(
            "PreToolUse",
            "read_files",
            json!({ "files": ["src/main.rs"] }),
        );
        let events = ClinePreset.parse(&input, "t_test").unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_cline_skips_non_tool_hooks() {
        let input = json!({
            "hookName": "TaskComplete",
            "clineVersion": "3.0.0",
            "taskId": "cline-task-123",
            "workspaceRoots": ["/home/user/project"],
        })
        .to_string();
        let events = ClinePreset.parse(&input, "t_test").unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_cline_model_fallback_to_provider() {
        let input = json!({
            "hookName": "PreToolUse",
            "clineVersion": "3.0.0",
            "taskId": "cline-task-123",
            "workspaceRoots": ["/home/user/project"],
            "model": { "provider": "openai", "slug": "" },
            "preToolUse": { "toolName": "editor", "parameters": { "path": "x.rs" } }
        })
        .to_string();
        let events = ClinePreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.model, "openai");
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_cline_model_allows_missing_fields() {
        for (model, expected) in [
            (json!({ "provider": "openai" }), "openai"),
            (json!({ "slug": "gpt-5" }), "gpt-5"),
        ] {
            let input = json!({
                "hookName": "PreToolUse",
                "clineVersion": "3.0.0",
                "taskId": "cline-task-123",
                "workspaceRoots": ["/home/user/project"],
                "model": model,
                "preToolUse": {
                    "toolName": "editor",
                    "parameters": { "path": "x.rs" }
                }
            })
            .to_string();

            let events = ClinePreset.parse(&input, "t_test").unwrap();
            match &events[0] {
                ParsedHookEvent::PreFileEdit(e) => {
                    assert_eq!(e.context.agent_id.model, expected);
                }
                _ => panic!("Expected PreFileEdit"),
            }
        }
    }

    #[test]
    fn test_cline_parameters_stringified_array() {
        let input = json!({
            "hookName": "PreToolUse",
            "clineVersion": "3.0.0",
            "taskId": "cline-task-123",
            "workspaceRoots": ["/home/user/project"],
            "preToolUse": {
                "toolName": "editor",
                "parameters": {
                    "files": "[\"src/a.rs\", \"src/b.rs\"]"
                }
            }
        })
        .to_string();
        let events = ClinePreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.file_paths.len(), 2);
                assert!(
                    e.file_paths
                        .contains(&PathBuf::from("/home/user/project/src/a.rs"))
                );
                assert!(
                    e.file_paths
                        .contains(&PathBuf::from("/home/user/project/src/b.rs"))
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_cline_extracts_paths_array() {
        let input = json!({
            "hookName": "PreToolUse",
            "clineVersion": "3.0.0",
            "taskId": "cline-task-123",
            "workspaceRoots": ["/home/user/project"],
            "preToolUse": {
                "toolName": "editor",
                "parameters": {
                    "paths": ["src/a.rs", { "path": "src/b.rs" }]
                }
            }
        })
        .to_string();
        let events = ClinePreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.file_paths.len(), 2);
                assert!(
                    e.file_paths
                        .contains(&PathBuf::from("/home/user/project/src/a.rs"))
                );
                assert!(
                    e.file_paths
                        .contains(&PathBuf::from("/home/user/project/src/b.rs"))
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_cline_content_not_treated_as_patch() {
        let input = make_post_cline_input(
            "write_to_file",
            json!({
                "path": "src/main.rs",
                "content": "*** Update File: src/other.rs\n@@ old\n+new\n"
            }),
            "done",
        );
        let events = ClinePreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.file_paths.len(), 1);
                assert_eq!(
                    e.file_paths[0],
                    PathBuf::from("/home/user/project/src/main.rs")
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_cline_tool_use_id_is_deterministic() {
        let input = make_cline_input("PreToolUse", "editor", json!({ "path": "src/main.rs" }));
        let id1 = match &ClinePreset.parse(&input, "t1").unwrap()[0] {
            ParsedHookEvent::PreFileEdit(e) => e.tool_use_id.clone().unwrap(),
            _ => panic!(),
        };
        let id2 = match &ClinePreset.parse(&input, "t2").unwrap()[0] {
            ParsedHookEvent::PreFileEdit(e) => e.tool_use_id.clone().unwrap(),
            _ => panic!(),
        };
        assert_eq!(id1, id2);
    }
}
