//! Windsurf agent implementation with sweep discovery.

use super::discovery::stem_session_binding;
use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::WatermarkStrategy;
use crate::operations::streams::agent::{Agent, StreamDescriptor};
use crate::operations::streams::reader::read_jsonl_byte_stream;
use crate::operations::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::operations::streams::timestamp::file_time_fallback;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Windsurf agent that reads Windsurf JSONL transcript files.
pub struct WindsurfAgent {
    batch_size: usize,
}

impl WindsurfAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }

    fn transcript_roots() -> Vec<PathBuf> {
        dirs::home_dir()
            .into_iter()
            .map(|home| home.join(".windsurf/transcripts"))
            .collect()
    }
}

impl Default for WindsurfAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for WindsurfAgent {
    fn validate_checkpoint_stream(
        &self,
        source: &crate::model::checkpoint_request::StreamSource,
    ) -> Result<DiscoveredSession, StreamError> {
        crate::operations::streams::agent::validate_checkpoint_stream_file(
            source,
            "windsurf",
            crate::model::checkpoint_request::StreamFormat::WindsurfJsonl,
            Self::transcript_roots(),
            |path| {
                if !crate::operations::streams::agent::checkpoint_stream_has_extension(
                    path, "jsonl",
                ) {
                    return None;
                }
                stem_session_binding(path)
            },
        )
    }

    fn batch_size_hint(&self) -> usize {
        self.batch_size
    }

    fn sweep_strategy(&self) -> SweepStrategy {
        SweepStrategy::Periodic(Duration::from_secs(30 * 60))
    }

    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError> {
        // Sweep not fully implemented for Windsurf yet — discovery comes from presets
        Ok(Vec::new())
    }

    fn read_incremental(
        &self,
        path: &Path,
        watermark: Box<dyn WatermarkStrategy>,
        session_id: &str,
    ) -> Result<StreamBatch, StreamError> {
        read_jsonl_byte_stream(
            path,
            watermark,
            session_id,
            self.batch_size_hint(),
            "Windsurf",
            "open",
        )
    }

    fn extract_event_timestamp(
        &self,
        _event: &serde_json::Value,
        file_meta: &std::fs::Metadata,
        is_first_event: bool,
    ) -> u32 {
        file_time_fallback(file_meta, is_first_event)
    }

    fn streams(&self) -> Vec<StreamDescriptor> {
        vec![StreamDescriptor::identity_transcript(
            StreamFormat::WindsurfJsonl,
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::stream_watermark::ByteOffsetWatermark;
    use crate::operations::streams::agents::test_support::{
        StreamAdapterContractCapabilities, assert_stream_adapter_contract, jsonl_fixture,
    };

    #[test]
    fn test_sweep_strategy() {
        let agent = WindsurfAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    fn make_jsonl_line(i: usize) -> String {
        format!(
            r#"{{"type":"user_input","id":{},"user_input":{{"user_response":"msg-{}"}}}}  "#,
            i, i
        )
    }

    #[test]
    fn test_stream_adapter_contract() {
        use tempfile::NamedTempFile;

        let file = NamedTempFile::new().unwrap();
        let mut fixture = jsonl_fixture(file.path(), make_jsonl_line);
        let agent = WindsurfAgent::with_batch_size(2);
        assert_stream_adapter_contract(
            &agent,
            &mut fixture,
            || Box::new(ByteOffsetWatermark::new(0)),
            |event| event["id"].as_u64().unwrap().to_string(),
            2,
            "test",
            StreamAdapterContractCapabilities::APPEND_ALL,
        );
    }

    #[test]
    fn test_read_incremental_basic() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user_input","user_input":{{"user_response":"Hello"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"planner_response","planner_response":{{"response":"Hi there"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = WindsurfAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0]["type"].as_str(), Some("user_input"));
        assert_eq!(result.events[1]["type"].as_str(), Some("planner_response"));
    }

    #[test]
    fn test_read_incremental_tool_actions() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"code_action","code_action":{{"path":"test.rs","new_content":"fn main()"}}}}"#).unwrap();
        writeln!(
            file,
            r#"{{"type":"run_command","run_command":{{"command":"cargo test"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = WindsurfAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0]["type"].as_str(), Some("code_action"));
        assert_eq!(result.events[1]["type"].as_str(), Some("run_command"));
    }

    #[test]
    fn test_read_incremental_resumes_from_offset() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        let line1 = r#"{"type":"user_input","user_input":{"user_response":"First"}}"#;
        let line2 = r#"{"type":"user_input","user_input":{"user_response":"Second"}}"#;
        writeln!(file, "{}", line1).unwrap();
        writeln!(file, "{}", line2).unwrap();
        file.flush().unwrap();

        let agent = WindsurfAgent::new();

        // First read gets both
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();
        assert_eq!(result.events.len(), 2);

        // Second read from new watermark gets nothing
        let result2 = agent
            .read_incremental(file.path(), result.new_watermark, "test")
            .unwrap();
        assert_eq!(result2.events.len(), 0);
    }
}
