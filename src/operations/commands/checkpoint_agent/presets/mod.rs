pub mod parse;

mod agent_v1;
mod ai_tab;
mod amp;
mod amp_enrichment;
mod claude;
mod claude_wire;
mod cline;
mod codex;
mod continue_cli;
mod cursor;
mod droid;
mod firebender;
mod gemini;
mod github_copilot;
mod human;
mod known_human;
mod mock_ai;
mod mock_known_human;
mod opencode;
mod opencode_enrichment;
mod pi;
mod windsurf;

use crate::error::GitAiError;
use crate::model::working_log::AgentId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// Re-export the checkpoint-wire stream types from model so that preset files
// that import via `super::StreamFormat` / `super::StreamSource` keep working.
pub use crate::model::checkpoint_request::StreamFormat;
pub use crate::model::checkpoint_request::StreamSource;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetContext {
    pub agent_id: AgentId,
    pub external_session_id: String,
    pub trace_id: String,
    pub cwd: PathBuf,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ParsedHookEvent {
    PreFileEdit(PreFileEdit),
    PostFileEdit(PostFileEdit),
    PreBashCall(PreBashCall),
    PostBashCall(PostBashCall),
    KnownHumanEdit(KnownHumanEdit),
    UntrackedEdit(UntrackedEdit),
}

impl ParsedHookEvent {
    pub(crate) fn preset_context_mut(&mut self) -> Option<&mut PresetContext> {
        match self {
            Self::PreFileEdit(event) => Some(&mut event.context),
            Self::PostFileEdit(event) => Some(&mut event.context),
            Self::PreBashCall(event) => Some(&mut event.context),
            Self::PostBashCall(event) => Some(&mut event.context),
            Self::KnownHumanEdit(_) | Self::UntrackedEdit(_) => None,
        }
    }

    pub(crate) fn set_post_stream_source(&mut self, source: StreamSource) {
        match self {
            Self::PostFileEdit(event) => event.stream_source = Some(source),
            Self::PostBashCall(event) => event.stream_source = Some(source),
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreFileEdit {
    pub context: PresetContext,
    pub file_paths: Vec<PathBuf>,
    pub dirty_files: Option<HashMap<PathBuf, String>>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostFileEdit {
    pub context: PresetContext,
    pub file_paths: Vec<PathBuf>,
    pub dirty_files: Option<HashMap<PathBuf, String>>,
    pub stream_source: Option<StreamSource>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownHumanEdit {
    pub trace_id: String,
    pub cwd: PathBuf,
    pub file_paths: Vec<PathBuf>,
    pub dirty_files: Option<HashMap<PathBuf, String>>,
    pub editor_metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UntrackedEdit {
    pub trace_id: String,
    pub cwd: PathBuf,
    pub file_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreBashCall {
    pub context: PresetContext,
    pub tool_use_id: String,
    #[serde(default)]
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostBashCall {
    pub context: PresetContext,
    pub tool_use_id: String,
    #[serde(default)]
    pub command: Option<String>,
    pub stream_source: Option<StreamSource>,
}

pub trait AgentPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError>;

    fn enrich_authorized_events(
        &self,
        hook_input: &str,
        events: &mut [ParsedHookEvent],
    ) -> Result<(), GitAiError> {
        let _ = (hook_input, events);
        Ok(())
    }
}

pub fn resolve_preset(name: &str) -> Result<Box<dyn AgentPreset>, GitAiError> {
    match name {
        "claude" => Ok(Box::new(claude::ClaudePreset)),
        "cline" => Ok(Box::new(cline::ClinePreset)),
        "codex" => Ok(Box::new(codex::CodexPreset)),
        "gemini" => Ok(Box::new(gemini::GeminiPreset)),
        "windsurf" => Ok(Box::new(windsurf::WindsurfPreset)),
        "continue-cli" => Ok(Box::new(continue_cli::ContinueCliPreset)),
        "cursor" => Ok(Box::new(cursor::CursorPreset)),
        "cursor-background" => Ok(Box::new(cursor::CursorBackgroundPreset)),
        "github-copilot" => Ok(Box::new(github_copilot::GithubCopilotPreset)),
        "amp" => Ok(Box::new(amp::AmpPreset)),
        "ai_tab" => Ok(Box::new(ai_tab::AiTabPreset)),
        "firebender" => Ok(Box::new(firebender::FirebenderPreset)),
        "agent-v1" => Ok(Box::new(agent_v1::AgentV1Preset)),
        "droid" => Ok(Box::new(droid::DroidPreset)),
        "opencode" => Ok(Box::new(opencode::OpenCodePreset)),
        "pi" => Ok(Box::new(pi::PiPreset)),
        "human" => Ok(Box::new(human::HumanPreset)),
        "mock_ai" => Ok(Box::new(mock_ai::MockAiPreset)),
        "known_human" => Ok(Box::new(known_human::KnownHumanPreset)),
        "mock_known_human" => Ok(Box::new(mock_known_human::MockKnownHumanPreset)),
        _ => Err(GitAiError::PresetError(format!("Unknown preset: {}", name))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_presets_share_legacy_path_payload_semantics() {
        let payload = r#"{"file_paths":["src/lib.rs",42,"","src/lib.rs"],"cwd":"relative/root"}"#;
        let expected_paths = vec![
            PathBuf::from("src/lib.rs"),
            PathBuf::from(""),
            PathBuf::from("src/lib.rs"),
        ];

        for preset_name in ["human", "mock_ai", "mock_known_human"] {
            let events = resolve_preset(preset_name)
                .unwrap()
                .parse(payload, "trace")
                .unwrap();
            let (cwd, file_paths) = match &events[0] {
                ParsedHookEvent::UntrackedEdit(event) => (&event.cwd, &event.file_paths),
                ParsedHookEvent::PostFileEdit(event) => (&event.context.cwd, &event.file_paths),
                ParsedHookEvent::KnownHumanEdit(event) => (&event.cwd, &event.file_paths),
                event => panic!("unexpected event for {preset_name}: {event:?}"),
            };

            assert_eq!(cwd, &PathBuf::from("relative/root"), "{preset_name}");
            assert_eq!(file_paths, &expected_paths, "{preset_name}");
        }
    }
}
