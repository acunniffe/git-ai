//! Per-agent usage extraction trait.

use super::types::UsageEntry;

/// Incremental, line-oriented extractor of token-usage entries from an agent
/// transcript. Fed complete JSONL lines in file order; may keep per-session
/// state between lines, persisted across runs via `state_json`.
pub trait UsageExtractor: Send {
    /// Cheap substring prefilter run before any JSON parsing. Must return
    /// true for every line that could yield usage or affect extractor state.
    fn wants_line(&self, line: &str) -> bool;

    /// Consume one raw JSONL line, returning any usage entries it completes.
    fn extract_line(&mut self, line: &str) -> Vec<UsageEntry>;

    /// Serialized parser state to persist between incremental runs. `None`
    /// for stateless extractors.
    fn state_json(&self) -> Option<String> {
        None
    }

    /// Restore state persisted by an earlier `state_json` call. Unknown or
    /// corrupt state must reset to defaults rather than fail.
    fn restore_state(&mut self, _json: &str) {}
}

/// Extractor for the given git-ai tool id, if token usage is supported.
pub fn extractor_for_tool(tool: &str) -> Option<Box<dyn UsageExtractor>> {
    match tool {
        "claude" => Some(Box::new(super::claude::ClaudeUsageExtractor)),
        "codex" => Some(Box::new(super::codex::CodexUsageExtractor::default())),
        _ => None,
    }
}
