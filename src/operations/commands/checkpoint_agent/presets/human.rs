use super::{AgentPreset, ParsedHookEvent, UntrackedEdit, parse};
use crate::error::GitAiError;

pub struct HumanPreset;

impl AgentPreset for HumanPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let (file_paths, cwd) = parse::legacy_file_paths_and_cwd(hook_input)?;

        Ok(vec![ParsedHookEvent::UntrackedEdit(UntrackedEdit {
            trace_id: trace_id.to_string(),
            cwd,
            file_paths,
        })])
    }
}
