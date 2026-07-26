use super::{AgentPreset, ParsedHookEvent, PostFileEdit, PresetContext, parse};
use crate::error::GitAiError;
use crate::model::working_log::AgentId;
use std::collections::HashMap;

pub struct MockAiPreset;

impl AgentPreset for MockAiPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let mock_agent_id = format!("ai-thread-{}", crate::model::clock::now_nanos());

        let (file_paths, cwd) = parse::legacy_file_paths_and_cwd(hook_input)?;

        let context = PresetContext {
            agent_id: AgentId {
                tool: "mock_ai".to_string(),
                id: mock_agent_id,
                model: "unknown".to_string(),
            },
            external_session_id: "mock_ai_session".to_string(),
            trace_id: trace_id.to_string(),
            cwd,
            metadata: HashMap::new(),
        };

        Ok(vec![ParsedHookEvent::PostFileEdit(PostFileEdit {
            context,
            file_paths,
            dirty_files: None,
            stream_source: None,
            tool_use_id: None,
        })])
    }
}
