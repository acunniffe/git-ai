use super::parse;
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

    fn resolve_transcript_path(
        transcript_path: Option<&str>,
        thread_id: Option<&str>,
        tool_use_id: Option<&str>,
    ) -> Option<PathBuf> {
        // 1. Direct transcript_path field
        if let Some(path) = transcript_path {
            let path = PathBuf::from(path);
            if path.exists() {
                return Some(path);
            }
        }

        // 2. Env var AMP_THREAD_PATH (used for testing)
        if let Ok(env_path) = std::env::var("AMP_THREAD_PATH")
            && !env_path.trim().is_empty()
        {
            let path = PathBuf::from(&env_path);
            if path.exists() {
                return Some(path);
            }
        }

        if let Ok(threads_dir) = Self::amp_threads_dir() {
            // 3a. If threads_dir is actually a file (test override), use it directly
            if threads_dir.is_file()
                && threads_dir
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            {
                return Some(threads_dir);
            }

            // 3b. Platform-specific threads directory + thread_id
            if let Some(thread_id) = thread_id {
                let candidate = threads_dir.join(format!("{}.json", thread_id));
                if candidate.exists() {
                    return Some(candidate);
                }
            }

            // 3c. Search thread files for matching tool_use_id
            if let Some(tool_use_id) = tool_use_id
                && let Some(path) = Self::find_thread_file_by_tool_use_id(&threads_dir, tool_use_id)
            {
                return Some(path);
            }
        }

        None
    }

    /// Scan thread JSON files in `threads_dir` for one containing the given
    /// `tool_use_id`. Returns the newest matching file.
    fn find_thread_file_by_tool_use_id(
        threads_dir: &std::path::Path,
        tool_use_id: &str,
    ) -> Option<PathBuf> {
        let entries = std::fs::read_dir(threads_dir).ok()?;
        let mut newest_match: Option<(PathBuf, std::time::SystemTime)> = None;

        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            {
                continue;
            }

            // Quick string check before full parse
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if !content.contains(tool_use_id) {
                continue;
            }

            // Verify structurally: look for tool_use content block with matching id
            let parsed: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let has_match = parsed
                .get("messages")
                .and_then(|v| v.as_array())
                .map(|msgs| {
                    msgs.iter().any(|msg| {
                        msg.get("content")
                            .and_then(|v| v.as_array())
                            .map(|blocks| {
                                blocks.iter().any(|block| {
                                    block.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                                        && block.get("id").and_then(|v| v.as_str())
                                            == Some(tool_use_id)
                                })
                            })
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);

            if !has_match {
                continue;
            }

            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);

            match &newest_match {
                Some((_, newest_modified)) if modified <= *newest_modified => {}
                _ => newest_match = Some((path, modified)),
            }
        }

        newest_match.map(|(path, _)| path)
    }

    fn amp_threads_dir() -> Result<PathBuf, GitAiError> {
        if let Ok(test_path) = std::env::var("GIT_AI_AMP_THREADS_PATH") {
            return Ok(PathBuf::from(test_path));
        }

        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
                return Ok(PathBuf::from(xdg_data).join("amp").join("threads"));
            }

            let home = dirs::home_dir().ok_or_else(|| {
                GitAiError::Generic("Could not determine home directory".to_string())
            })?;
            Ok(home
                .join(".local")
                .join("share")
                .join("amp")
                .join("threads"))
        }

        #[cfg(target_os = "windows")]
        {
            if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
                return Ok(PathBuf::from(local_app_data).join("amp").join("threads"));
            }
            if let Ok(app_data) = std::env::var("APPDATA") {
                return Ok(PathBuf::from(app_data).join("amp").join("threads"));
            }

            let home = dirs::home_dir().ok_or_else(|| {
                GitAiError::Generic("Could not determine home directory".to_string())
            })?;
            Ok(home
                .join("AppData")
                .join("Local")
                .join("amp")
                .join("threads"))
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Err(GitAiError::Generic(
                "Amp threads path not supported on this platform".to_string(),
            ))
        }
    }
}

impl AgentPreset for AmpPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let hook_input: AmpHookInput = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let tool_class = hook_input
            .tool_name
            .as_deref()
            .map(|name| bash_tool::classify_tool(Agent::Amp, name))
            // Preserve payloads from older Amp integrations that omitted tool_name.
            .unwrap_or(ToolClass::FileEdit);
        if tool_class == ToolClass::Skip {
            return Ok(Vec::new());
        }

        let is_pre = hook_input.hook_event_name == "PreToolUse";

        let is_bash = tool_class == ToolClass::Bash;

        let cwd = hook_input.cwd.as_deref().unwrap_or(".");

        let thread_id = hook_input.thread_id.clone();
        let tool_use_id = hook_input.tool_use_id.clone();
        let tool_use_id_str = tool_use_id.as_deref().unwrap_or("bash").to_string();

        let file_paths = Self::extract_file_paths(&hook_input, cwd);

        // Resolve transcript path for StreamSource
        let resolved_transcript_path = Self::resolve_transcript_path(
            hook_input.transcript_path.as_deref(),
            thread_id.as_deref(),
            tool_use_id.as_deref(),
        );

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
        if let Some(ref path) = resolved_transcript_path {
            metadata.insert(
                "transcript_path".to_string(),
                path.to_string_lossy().to_string(),
            );
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
                model: resolved_transcript_path
                    .as_ref()
                    .and_then(|tp| {
                        crate::streams::model_extraction::extract_model(
                            tp,
                            crate::streams::sweep::StreamFormat::AmpThreadJson,
                            None,
                        )
                        .ok()
                        .flatten()
                    })
                    .unwrap_or_else(|| "unknown".to_string()),
            },
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
            metadata,
        };

        let stream_source = resolved_transcript_path.map(|path| StreamSource {
            path,
            format: StreamFormat::AmpThreadJson,
            session_id: generate_session_id(&context.external_session_id, "amp"),
            external_session_id: context.external_session_id.clone(),
            external_parent_session_id: None,
        });

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
                stream_source,
            }),
            (false, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                dirty_files: None,
                stream_source,
                tool_use_id: tool_use_id.clone(),
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
    fn test_amp_ignores_read_only_and_unsupported_tools() {
        for hook_event in ["PreToolUse", "PostToolUse"] {
            for tool_name in ["Read", "finder", "glob", "Grep", "Task", "unknown_tool"] {
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
    fn test_ignored_amp_hook_produces_no_checkpoint_requests() {
        let input = make_amp_input("PostToolUse", "Read");
        let requests = crate::commands::checkpoint_agent::orchestrator::execute_preset_checkpoint(
            "amp",
            &input,
            &Default::default(),
        )
        .unwrap();
        assert!(requests.is_empty());
    }

    #[test]
    fn test_amp_preserves_all_mutating_tools() {
        for tool_name in [
            "Write",
            "Edit",
            "create_file",
            "edit_file",
            "apply_patch",
            "undo_edit",
        ] {
            let pre = make_amp_input("PreToolUse", tool_name);
            assert!(matches!(
                AmpPreset.parse(&pre, "t_test").unwrap()[..],
                [ParsedHookEvent::PreFileEdit(_)]
            ));

            let post = make_amp_input("PostToolUse", tool_name);
            assert!(matches!(
                AmpPreset.parse(&post, "t_test").unwrap()[..],
                [ParsedHookEvent::PostFileEdit(_)]
            ));
        }

        for tool_name in ["Bash", "shell_command"] {
            let pre = make_amp_input("PreToolUse", tool_name);
            assert!(matches!(
                AmpPreset.parse(&pre, "t_test").unwrap()[..],
                [ParsedHookEvent::PreBashCall(_)]
            ));

            let post = make_amp_input("PostToolUse", tool_name);
            assert!(matches!(
                AmpPreset.parse(&post, "t_test").unwrap()[..],
                [ParsedHookEvent::PostBashCall(_)]
            ));
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
}
