use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use crate::mdm::utils::home_dir;
use crate::streams::model_extraction::extract_model_from_zcode_rollout;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct ZcodePreset;

impl ZcodePreset {
    /// Locate the live rollout transcript for a zcode session under `home`.
    ///
    /// ZCode stores rollouts at
    /// `~/.zcode/cli/rollout/model-io-sess_<session_id>.jsonl`, deterministically
    /// named by session id (subagents get their own files). The hook payload's
    /// own `transcript_path` is only a temp snapshot, so the rollout is derived
    /// from the session id instead. Session ids that could escape the rollout
    /// directory are rejected.
    pub fn find_rollout_path_in_home(session_id: &str, home: &Path) -> Option<PathBuf> {
        if session_id.is_empty()
            || session_id.contains('/')
            || session_id.contains('\\')
            || session_id.contains("..")
        {
            return None;
        }
        let path = home
            .join(".zcode")
            .join("cli")
            .join("rollout")
            .join(format!("model-io-sess_{session_id}.jsonl"));
        path.is_file().then_some(path)
    }

    /// Real Claude Code payloads carry a persistent transcript under ~/.claude;
    /// ZCode's transcript_path is always a temp snapshot under the OS tmpdir.
    fn is_claude_code_hook_payload(data: &serde_json::Value) -> bool {
        parse::optional_str_multi(data, &["transcript_path", "transcriptPath"])
            .map(|p| p.replace('\\', "/").contains("/.claude/"))
            .unwrap_or(false)
    }

    /// File paths for a tool event. Write/Edit carry `tool_input.file_path`,
    /// but patch-style ApplyPatch payloads embed the edited paths in the patch
    /// text, and PostToolUse responses may carry the canonical path instead —
    /// without those fallbacks the checkpoint would carry no files and the
    /// edit would lose AI attribution.
    fn file_paths_for_event(
        data: &serde_json::Value,
        cwd: &str,
        tool_name: Option<&str>,
        is_post: bool,
    ) -> Vec<PathBuf> {
        let mut paths = parse::file_paths_from_tool_input(data, cwd);
        if !paths.is_empty() || tool_name != Some("ApplyPatch") {
            return paths;
        }

        let tool_input = data.get("tool_input").or_else(|| data.get("toolInput"));
        if let Some(ti) = tool_input
            && let Some(text) = ti
                .as_str()
                .or_else(|| ti.get("patch").and_then(|v| v.as_str()))
        {
            let mut raw = Vec::new();
            parse::collect_apply_patch_paths_from_text(text, &mut raw);
            paths.extend(raw.iter().map(|p| parse::resolve_absolute(p, cwd)));
        }

        if paths.is_empty()
            && is_post
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
                paths.push(parse::resolve_absolute(path, cwd));
            }
        }

        paths
    }
}

impl AgentPreset for ZcodePreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        if Self::is_claude_code_hook_payload(&data) {
            return Err(GitAiError::PresetError(
                "Skipping Claude Code hook payload in zcode preset; use claude hooks.".to_string(),
            ));
        }

        let tool_name = parse::optional_str_multi(&data, &["tool_name", "toolName"]);
        let tool_class = tool_name
            .map(|name| bash_tool::classify_tool(Agent::Zcode, name))
            .unwrap_or(ToolClass::FileEdit);
        if tool_class == ToolClass::Skip {
            return Ok(Vec::new());
        }

        let cwd = parse::required_str(&data, "cwd")?;
        let session_id = parse::optional_str(&data, "session_id").unwrap_or("unknown");

        // Only PreToolUse/PostToolUse carry tool state. PostToolUseFailure means
        // the tool errored (nothing landed on disk), PermissionRequest fires
        // before PreToolUse (which captures the pre state), and the non-tool
        // events (SessionStart/UserPromptSubmit/Stop) have no tool input.
        let hook_event = parse::optional_str_multi(&data, &["hook_event_name", "hookEventName"]);
        let is_pre = hook_event == Some("PreToolUse");
        let is_post = hook_event == Some("PostToolUse");
        if !is_pre && !is_post {
            return Ok(Vec::new());
        }

        let rollout_path = Self::find_rollout_path_in_home(session_id, &home_dir());
        let model = rollout_path
            .as_deref()
            .and_then(|p| extract_model_from_zcode_rollout(p).ok().flatten())
            .unwrap_or_else(|| "unknown".to_string());

        let mut metadata = HashMap::new();
        if let Some(transcript_path) =
            parse::optional_str_multi(&data, &["transcript_path", "transcriptPath"])
        {
            metadata.insert("transcript_path".to_string(), transcript_path.to_string());
        }
        if let Some(rollout) = &rollout_path {
            metadata.insert("rollout_path".to_string(), rollout.display().to_string());
        }

        let context = PresetContext {
            agent_id: AgentId {
                tool: "zcode".to_string(),
                id: session_id.to_string(),
                model,
            },
            external_session_id: session_id.to_string(),
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
            metadata,
        };

        let tool_use_id =
            parse::str_or_default_multi(&data, &["tool_use_id", "toolUseId"], "bash").to_string();
        let bash_command = parse::bash_command_from_hook_input(&data);
        let file_paths = Self::file_paths_for_event(&data, cwd, tool_name, is_post);

        let event = if tool_class == ToolClass::Bash {
            if is_pre {
                ParsedHookEvent::PreBashCall(PreBashCall {
                    context,
                    tool_use_id,
                    command: bash_command,
                })
            } else {
                ParsedHookEvent::PostBashCall(PostBashCall {
                    context,
                    tool_use_id,
                    command: bash_command,
                    stream_source: None,
                })
            }
        } else if is_pre {
            ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                dirty_files: None,
                tool_use_id: Some(tool_use_id),
            })
        } else {
            ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                dirty_files: None,
                stream_source: None,
                tool_use_id: Some(tool_use_id),
            })
        };

        Ok(vec![event])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::checkpoint_agent::presets::*;
    use serde_json::json;

    fn make_zcode_hook_input(event: &str, tool: &str) -> String {
        let tool_input = if tool == "Bash" {
            json!({"command": "echo hello"})
        } else {
            json!({"file_path": "src/main.rs"})
        };
        json!({
            "transcript_path": "/tmp/zcode-claude-hook-abcd/transcript.jsonl",
            "cwd": "/home/user/project",
            "hook_event_name": event,
            "tool_name": tool,
            "session_id": "sess-1",
            "tool_use_id": "tu-1",
            "tool_input": tool_input
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
                assert_eq!(e.tool_use_id.as_deref(), Some("tu-1"));
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
                // v1 has no transcript streaming for zcode: no live
                // Claude-style transcript exists for the payload's temp path.
                assert!(e.stream_source.is_none());
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
                assert_eq!(e.command.as_deref(), Some("echo hello"));
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
                assert!(e.stream_source.is_none());
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_zcode_ignores_read_only_and_unsupported_tools() {
        for hook_event in ["PreToolUse", "PostToolUse"] {
            for tool_name in [
                "Read",
                "Grep",
                "Glob",
                "Task",
                "Agent",
                "Skill",
                "WebFetch",
                "WebSearch",
                "TodoWrite",
                "UnknownTool",
            ] {
                let input = make_zcode_hook_input(hook_event, tool_name);
                let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
                assert!(
                    events.is_empty(),
                    "{hook_event} {tool_name} unexpectedly produced events"
                );
            }
        }
    }

    #[test]
    fn test_zcode_post_tool_use_failure_ignored() {
        // A failed Write/Edit never landed on disk, so no checkpoint should fire.
        let input = make_zcode_hook_input("PostToolUseFailure", "Write");
        let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_zcode_non_tool_events_ignored() {
        // PermissionRequest precedes PreToolUse (which captures pre-state), and
        // SessionStart/UserPromptSubmit/Stop carry no tool state at all.
        for event in [
            "PermissionRequest",
            "SessionStart",
            "UserPromptSubmit",
            "Stop",
        ] {
            let input = make_zcode_hook_input(event, "Write");
            let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
            assert!(events.is_empty(), "{event} unexpectedly produced events");
        }
    }

    #[test]
    fn test_zcode_apply_patch_classified_as_file_edit() {
        // zcode's matcher aliases ApplyPatch to Write/Edit.
        let pre = make_zcode_hook_input("PreToolUse", "ApplyPatch");
        assert!(matches!(
            ZcodePreset.parse(&pre, "t_test123456789a").unwrap()[..],
            [ParsedHookEvent::PreFileEdit(_)]
        ));

        let post = make_zcode_hook_input("PostToolUse", "ApplyPatch");
        assert!(matches!(
            ZcodePreset.parse(&post, "t_test123456789a").unwrap()[..],
            [ParsedHookEvent::PostFileEdit(_)]
        ));
    }

    #[test]
    fn test_zcode_apply_patch_extracts_paths_from_patch_text() {
        // Patch-style payloads carry the edited paths inside the patch body
        // instead of a file_path key; without this extraction the checkpoint
        // would carry no files and the edit would lose AI attribution.
        let input = json!({
            "transcript_path": "/tmp/zcode-claude-hook-abcd/transcript.jsonl",
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "ApplyPatch",
            "session_id": "sess-1",
            "tool_use_id": "tu-1",
            "tool_input": {
                "patch": "*** Update File: src/main.rs\n@@\n-old\n+new\n*** Add File: src/new.rs\n@@\n+content"
            }
        })
        .to_string();
        let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![
                        PathBuf::from("/home/user/project/src/main.rs"),
                        PathBuf::from("/home/user/project/src/new.rs")
                    ]
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_zcode_apply_patch_pre_extracts_paths_from_patch_text() {
        // The pre-edit snapshot needs the paths too, or the before-state of a
        // patch-style edit is never captured.
        let input = json!({
            "transcript_path": "/tmp/zcode-claude-hook-abcd/transcript.jsonl",
            "cwd": "/home/user/project",
            "hook_event_name": "PreToolUse",
            "tool_name": "ApplyPatch",
            "session_id": "sess-1",
            "tool_use_id": "tu-1",
            "tool_input": {
                "patch": "*** Update File: src/main.rs\n@@\n-old\n+new"
            }
        })
        .to_string();
        let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_zcode_apply_patch_post_falls_back_to_tool_response_path() {
        let input = json!({
            "transcript_path": "/tmp/zcode-claude-hook-abcd/transcript.jsonl",
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "ApplyPatch",
            "session_id": "sess-1",
            "tool_use_id": "tu-1",
            "tool_input": {"patch": "no parsable path markers here"},
            "tool_response": {"file_path": "src/from-response.rs"}
        })
        .to_string();
        let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/from-response.rs")]
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_zcode_preserves_all_mutating_file_tools() {
        for tool_name in ["Write", "Edit", "MultiEdit", "NotebookEdit"] {
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
    fn test_zcode_missing_cwd_errors() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "session_id": "sess-1",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        assert!(ZcodePreset.parse(&input, "t_test123456789a").is_err());
    }

    #[test]
    fn test_zcode_session_id_defaults_to_unknown() {
        let input = json!({
            "transcript_path": "/tmp/zcode-claude-hook-abcd/transcript.jsonl",
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.external_session_id, "unknown");
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_zcode_skips_claude_code_payload() {
        // Real Claude Code payloads carry a persistent ~/.claude transcript;
        // zcode's transcript_path is always a temp snapshot under the OS tmpdir.
        let input = json!({
            "transcript_path": "/home/user/.claude/projects/abc123.jsonl",
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        assert!(ZcodePreset.parse(&input, "t_test123456789a").is_err());
    }

    #[test]
    fn test_zcode_model_unknown_when_rollout_missing() {
        // No rollout exists for this session id in the test HOME, so the model
        // falls back to "unknown" instead of failing the checkpoint.
        let input = make_zcode_hook_input("PostToolUse", "Write");
        let events = ZcodePreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.model, "unknown");
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_ignored_zcode_hook_produces_no_checkpoint_requests() {
        let input = make_zcode_hook_input("PostToolUse", "Read");
        let requests = crate::commands::checkpoint_agent::orchestrator::execute_preset_checkpoint(
            "zcode", &input,
        )
        .unwrap();
        assert!(requests.is_empty());
    }

    #[test]
    fn test_find_zcode_rollout_path_in_home() {
        let temp = tempfile::tempdir().unwrap();
        let rollout_dir = temp.path().join(".zcode").join("cli").join("rollout");

        // Missing rollout resolves to None.
        assert_eq!(
            ZcodePreset::find_rollout_path_in_home("sess-1", temp.path()),
            None
        );

        // Existing rollout resolves by deterministic filename.
        std::fs::create_dir_all(&rollout_dir).unwrap();
        let rollout_path = rollout_dir.join("model-io-sess_sess-1.jsonl");
        std::fs::write(&rollout_path, "{}\n").unwrap();
        assert_eq!(
            ZcodePreset::find_rollout_path_in_home("sess-1", temp.path()),
            Some(rollout_path)
        );
    }

    #[test]
    fn test_find_zcode_rollout_path_rejects_path_traversal() {
        let temp = tempfile::tempdir().unwrap();
        for malicious in ["../escape", "a/b", "..", "sess\\slash"] {
            assert_eq!(
                ZcodePreset::find_rollout_path_in_home(malicious, temp.path()),
                None,
                "session id {malicious:?} should be rejected"
            );
        }
    }
}
