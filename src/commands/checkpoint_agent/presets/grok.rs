use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext, StreamFormat, StreamSource,
};
use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use crate::mdm::utils::grok_home_dir;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub struct GrokPreset;

impl AgentPreset for GrokPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let tool_class = parse::optional_str_multi(&data, &["tool_name", "toolName"])
            .map(|name| bash_tool::classify_tool(Agent::Grok, name))
            .unwrap_or(ToolClass::FileEdit);
        if tool_class == ToolClass::Skip {
            return Ok(Vec::new());
        }

        let cwd = parse::required_str_multi(&data, &["cwd", "workspaceRoot"])?;
        let session_id =
            parse::str_or_default_multi(&data, &["session_id", "sessionId"], "unknown").to_string();
        let hook_event = parse::optional_str_multi(&data, &["hook_event_name", "hookEventName"]);
        let tool_use_id = parse::str_or_default_multi(&data, &["tool_use_id", "toolUseId"], "bash");
        let transcript_path = resolve_transcript_path(&data, cwd, &session_id);
        let model = resolve_model(&data, cwd, &session_id, transcript_path.as_deref());

        let context = PresetContext {
            agent_id: AgentId {
                tool: "grok".to_string(),
                id: session_id.clone(),
                model,
            },
            external_session_id: session_id.clone(),
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
            metadata: HashMap::new(),
        };

        let is_pre = parse::is_pre_hook_event(hook_event);
        let is_bash = tool_class == ToolClass::Bash;
        let file_paths = parse::file_paths_from_tool_input(&data, cwd);
        let bash_command = parse::bash_command_from_hook_input(&data);
        let stream_source = if is_pre {
            None
        } else {
            transcript_path.map(|path| StreamSource {
                path,
                format: StreamFormat::GrokJsonl,
                session_id: generate_session_id(&session_id, "grok"),
                external_session_id: session_id,
                external_parent_session_id: None,
            })
        };

        let event = match (is_pre, is_bash) {
            (true, true) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id: tool_use_id.to_string(),
                command: bash_command,
            }),
            (true, false) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                dirty_files: None,
                tool_use_id: Some(tool_use_id.to_string()),
            }),
            (false, true) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id: tool_use_id.to_string(),
                command: bash_command,
                stream_source,
            }),
            (false, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                dirty_files: None,
                stream_source,
                tool_use_id: Some(tool_use_id.to_string()),
            }),
        };

        Ok(vec![event])
    }
}

fn resolve_transcript_path(
    data: &serde_json::Value,
    cwd: &str,
    session_id: &str,
) -> Option<PathBuf> {
    if let Some(path) = parse::optional_str_multi(data, &["transcriptPath", "transcript_path"]) {
        return Some(PathBuf::from(path));
    }
    grok_session_dir(cwd, session_id).map(|dir| dir.join("updates.jsonl"))
}

fn resolve_model(
    data: &serde_json::Value,
    cwd: &str,
    session_id: &str,
    transcript_path: Option<&Path>,
) -> String {
    if let Some(model) = parse::optional_str_multi(data, &["modelId", "model", "model_name"])
        .map(str::trim)
        .filter(|model| !model.is_empty() && !model.eq_ignore_ascii_case("unknown"))
    {
        return model.to_string();
    }

    if let Some(path) = transcript_path
        && let Some(model) = grok_summary_model(&path.parent().unwrap_or(path).join("summary.json"))
    {
        return model;
    }

    if let Some(model) = grok_session_dir(cwd, session_id)
        .and_then(|dir| grok_summary_model(&dir.join("summary.json")))
    {
        return model;
    }

    if let Some(path) = transcript_path
        && let Ok(Some(model)) = crate::streams::model_extraction::extract_model(
            path,
            crate::streams::sweep::StreamFormat::GrokJsonl,
            None,
        )
    {
        return model;
    }

    "unknown".to_string()
}

fn grok_session_dir(cwd: &str, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty()
        || session_id == "unknown"
        || session_id.contains(['/', '\\'])
        || session_id == "."
        || session_id == ".."
    {
        return None;
    }

    Some(
        grok_home_dir()
            .join("sessions")
            .join(percent_encode_path(cwd.trim_end_matches('/')))
            .join(session_id),
    )
}

fn grok_summary_model(summary_path: &Path) -> Option<String> {
    let summary = fs::read_to_string(summary_path).ok()?;
    let data: serde_json::Value = serde_json::from_str(&summary).ok()?;
    data.get("current_model_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty() && !model.eq_ignore_ascii_case("unknown"))
        .map(str::to_string)
}

fn percent_encode_path(path: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::checkpoint_agent::presets::*;
    use serde_json::json;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_grok_prefers_session_summary_over_transcript_metadata() {
        let temp = TempDir::new().unwrap();
        let updates_path = temp.path().join("updates.jsonl");
        fs::write(
            temp.path().join("summary.json"),
            json!({"current_model_id": "grok-4.6"}).to_string(),
        )
        .unwrap();
        fs::write(
            &updates_path,
            json!({"params":{"update":{"_meta":{"modelId":"grok-4.5"}}}}).to_string(),
        )
        .unwrap();
        let input = json!({
            "hookEventName": "post_tool_use",
            "sessionId": "grok-sess-1",
            "cwd": "/home/user/project",
            "transcriptPath": updates_path,
            "toolName": "search_replace",
            "toolInput": {"file_path": "src/main.rs"}
        })
        .to_string();

        let events = GrokPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => assert_eq!(e.context.agent_id.model, "grok-4.6"),
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_grok_reads_latest_model_from_hook_transcript_path() {
        let temp = TempDir::new().unwrap();
        let updates_path = temp.path().join("updates.jsonl");
        fs::write(
            &updates_path,
            [
                json!({"params":{"update":{"_meta":{"modelId":"grok-4.5"}}}}).to_string(),
                json!({"params":{"update":{"model":"grok-4.6"}}}).to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        let input = json!({
            "hookEventName": "post_tool_use",
            "sessionId": "grok-sess-1",
            "cwd": "/home/user/project",
            "transcriptPath": updates_path,
            "toolName": "search_replace",
            "toolInput": {"file_path": "src/main.rs"}
        })
        .to_string();

        let events = GrokPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => assert_eq!(e.context.agent_id.model, "grok-4.6"),
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    #[serial]
    fn test_grok_reads_current_model_from_session_summary_when_hook_omits_it() {
        let temp = TempDir::new().unwrap();
        let old_grok_home = std::env::var_os("GROK_HOME");
        let session_dir = temp
            .path()
            .join("sessions")
            .join("%2Fhome%2Fuser%2Fproject")
            .join("grok-sess-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("summary.json"),
            json!({"current_model_id": "grok-4.6"}).to_string(),
        )
        .unwrap();
        unsafe { std::env::set_var("GROK_HOME", temp.path()) };

        let input = json!({
            "hookEventName": "post_tool_use",
            "sessionId": "grok-sess-1",
            "cwd": "/home/user/project",
            "toolName": "search_replace",
            "toolInput": {"file_path": "src/main.rs"}
        })
        .to_string();
        let events = GrokPreset.parse(&input, "t_test").unwrap();

        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.model, "grok-4.6");
            }
            _ => panic!("Expected PostFileEdit"),
        }

        unsafe {
            match old_grok_home {
                Some(value) => std::env::set_var("GROK_HOME", value),
                None => std::env::remove_var("GROK_HOME"),
            }
        }
    }
}
