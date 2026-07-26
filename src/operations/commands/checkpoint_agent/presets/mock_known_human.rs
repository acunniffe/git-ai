use super::{AgentPreset, KnownHumanEdit, ParsedHookEvent, parse};
use crate::error::GitAiError;
use std::collections::HashMap;

pub struct MockKnownHumanPreset;

impl AgentPreset for MockKnownHumanPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let (file_paths, cwd) = parse::legacy_file_paths_and_cwd(hook_input)?;

        Ok(vec![ParsedHookEvent::KnownHumanEdit(KnownHumanEdit {
            trace_id: trace_id.to_string(),
            cwd,
            file_paths,
            dirty_files: None,
            editor_metadata: HashMap::new(),
        })])
    }
}
