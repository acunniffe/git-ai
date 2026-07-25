use super::super::{ParsedHookEvent, StreamFormat, StreamSource, parse};
use crate::error::GitAiError;
use crate::model::authorship_log_serialization::generate_session_id;
use crate::operations::streams::model_extraction;
use std::path::{Path, PathBuf};

pub(super) fn enrich_authorized_events(
    hook_input: &str,
    events: &mut [ParsedHookEvent],
) -> Result<(), GitAiError> {
    let data = parse::hook_json(hook_input)?;
    let hook_event_name = parse::optional_str_multi(&data, &["hook_event_name", "hookEventName"])
        .unwrap_or("after_edit");

    if events.iter_mut().any(is_cli_event) {
        enrich_cli_events(events);
    } else if hook_event_name == "after_edit" {
        enrich_legacy_events(&data, events);
    } else if matches!(hook_event_name, "PreToolUse" | "PostToolUse") {
        if hook_event_name == "PostToolUse"
            && events
                .iter()
                .any(|event| matches!(event, ParsedHookEvent::PostFileEdit(_)))
        {
            // VS Code Copilot fires PostToolUse before the file is written to disk.
            // Delay only after repository authorization, immediately before the
            // file snapshot and transcript/model enrichment.
            // https://github.com/microsoft/vscode/issues/315926
            tracing::debug!(
                "Sleeping 80ms for VS Code Copilot PostToolUse file-write race (vscode#315926)"
            );
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
        enrich_native_events(&data, events);
    }

    Ok(())
}

fn is_cli_event(event: &mut ParsedHookEvent) -> bool {
    event
        .preset_context_mut()
        .map(|context| context.agent_id.tool == "github-copilot-cli")
        .unwrap_or(false)
}

fn enrich_cli_events(events: &mut [ParsedHookEvent]) {
    let Some(home) = dirs::home_dir() else {
        return;
    };

    for event in events {
        let Some(session_id) = event
            .preset_context_mut()
            .map(|context| context.external_session_id.clone())
        else {
            continue;
        };
        let path = copilot_cli_session_path(&home, &session_id);
        if !path.exists() {
            continue;
        }

        if let Some(model) = extract_model(&path, StreamFormat::CopilotEventStreamJsonl)
            && let Some(context) = event.preset_context_mut()
        {
            context.agent_id.model = model;
        }

        event.set_post_stream_source(StreamSource {
            path,
            format: StreamFormat::CopilotEventStreamJsonl,
            session_id: generate_session_id(&session_id, "github-copilot-cli"),
            external_session_id: session_id,
            external_parent_session_id: None,
        });
    }
}

fn copilot_cli_session_path(home: &Path, session_id: &str) -> PathBuf {
    home.join(".copilot/session-state")
        .join(session_id)
        .join("events.jsonl")
}

fn enrich_legacy_events(data: &serde_json::Value, events: &mut [ParsedHookEvent]) {
    let Some(path) =
        parse::optional_str_multi(data, &["chat_session_path", "chatSessionPath"]).map(Path::new)
    else {
        return;
    };
    let Some(model) = extract_model(path, StreamFormat::CopilotSessionJson) else {
        return;
    };
    set_model(events, &model);
}

fn enrich_native_events(data: &serde_json::Value, events: &mut [ParsedHookEvent]) {
    let Some(path) = super::ide::transcript_path_from_hook_data(data) else {
        return;
    };
    let format = super::transcript_format(path);
    let session_id = super::extract_session_id(data);
    let sweep_format = match format {
        StreamFormat::CopilotEventStreamJsonl => {
            crate::operations::streams::sweep::StreamFormat::CopilotEventStreamJsonl
        }
        _ => crate::operations::streams::sweep::StreamFormat::CopilotSessionJson,
    };
    let Some(model) = model_extraction::extract_model_from_copilot_vscode_transcript(
        Path::new(path),
        sweep_format,
        &session_id,
    )
    .ok()
    .flatten() else {
        return;
    };
    set_model(events, &model);
}

fn extract_model(path: &Path, format: StreamFormat) -> Option<String> {
    let sweep_format = match format {
        StreamFormat::CopilotEventStreamJsonl => {
            crate::operations::streams::sweep::StreamFormat::CopilotEventStreamJsonl
        }
        _ => crate::operations::streams::sweep::StreamFormat::CopilotSessionJson,
    };
    model_extraction::extract_model(path, sweep_format, None)
        .ok()
        .flatten()
}

fn set_model(events: &mut [ParsedHookEvent], model: &str) {
    for event in events {
        if let Some(context) = event.preset_context_mut() {
            context.agent_id.model = model.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::AgentPreset;
    use super::super::GithubCopilotPreset;
    use super::*;
    use crate::model::checkpoint_request::StreamFormat;
    use crate::operations::mdm::test_env::with_temp_home;
    use serde_json::json;
    use std::path::PathBuf;

    fn context_model(events: &[ParsedHookEvent]) -> &str {
        match &events[0] {
            ParsedHookEvent::PreFileEdit(event) => &event.context.agent_id.model,
            ParsedHookEvent::PostFileEdit(event) => &event.context.agent_id.model,
            ParsedHookEvent::PreBashCall(event) => &event.context.agent_id.model,
            ParsedHookEvent::PostBashCall(event) => &event.context.agent_id.model,
            event => panic!("expected agent event, got {event:?}"),
        }
    }

    #[test]
    fn legacy_parse_is_pure_and_authorized_enrichment_reads_model() {
        let dir = tempfile::tempdir().unwrap();
        let session_path = dir.path().join("session.json");
        std::fs::write(
            &session_path,
            r#"{"requests":[{"modelId":"copilot/claude-sonnet-4"}]}"#,
        )
        .unwrap();
        let input = json!({
            "hook_event_name": "after_edit",
            "workspace_folder": dir.path(),
            "chat_session_path": session_path,
            "session_id": "legacy-session",
            "edited_filepaths": ["src/main.rs"]
        })
        .to_string();

        let mut events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(context_model(&events), "unknown");

        GithubCopilotPreset
            .enrich_authorized_events(&input, &mut events)
            .unwrap();
        assert_eq!(context_model(&events), "copilot/claude-sonnet-4");
    }

    #[test]
    fn native_parse_is_pure_and_authorized_enrichment_reads_model() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir
            .path()
            .join("workspaceStorage")
            .join("workspace")
            .join("GitHub.copilot-chat")
            .join("transcripts")
            .join("native-session.jsonl");
        std::fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript_path,
            r#"{"type":"session.model_change","data":{"newModel":"gpt-5"}}"#,
        )
        .unwrap();
        let input = json!({
            "hook_event_name": "PostToolUse",
            "cwd": dir.path(),
            "tool_name": "create_file",
            "session_id": "native-session",
            "tool_use_id": "tool-1",
            "tool_input": {"file_path": dir.path().join("src/main.rs")},
            "transcript_path": transcript_path
        })
        .to_string();

        let mut events = GithubCopilotPreset
            .parse(&input, "t_test123456789a")
            .unwrap();
        assert_eq!(context_model(&events), "unknown");

        GithubCopilotPreset
            .enrich_authorized_events(&input, &mut events)
            .unwrap();
        assert_eq!(context_model(&events), "gpt-5");
    }

    #[test]
    #[serial_test::serial]
    fn cli_parse_has_no_discovered_source_and_authorized_enrichment_restores_it() {
        with_temp_home(|home| {
            let session_path = home
                .join(".copilot/session-state")
                .join("cli-session")
                .join("events.jsonl");
            std::fs::create_dir_all(session_path.parent().unwrap()).unwrap();
            std::fs::write(
                &session_path,
                r#"{"type":"session.model_change","data":{"newModel":"gpt-4.1"}}"#,
            )
            .unwrap();
            let input = json!({
                "hook_event_name": "PostToolUse",
                "cwd": home.join("project"),
                "tool_name": "create",
                "session_id": "cli-session",
                "tool_input": {"path": home.join("project/main.rs")}
            })
            .to_string();

            let mut events = GithubCopilotPreset
                .parse(&input, "t_test123456789a")
                .unwrap();
            assert_eq!(context_model(&events), "unknown");
            match &events[0] {
                ParsedHookEvent::PostFileEdit(event) => {
                    assert!(event.stream_source.is_none());
                }
                event => panic!("expected post-file event, got {event:?}"),
            }

            GithubCopilotPreset
                .enrich_authorized_events(&input, &mut events)
                .unwrap();
            assert_eq!(context_model(&events), "gpt-4.1");
            match &events[0] {
                ParsedHookEvent::PostFileEdit(event) => {
                    let source = event.stream_source.as_ref().unwrap();
                    assert_eq!(source.path, PathBuf::from(&session_path));
                    assert_eq!(source.format, StreamFormat::CopilotEventStreamJsonl);
                }
                event => panic!("expected post-file event, got {event:?}"),
            }
        });
    }
}
