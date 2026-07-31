//! Cursor agent implementation with sweep discovery.

use super::discovery::{collect_files_recursively, stem_session_binding};
use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::WatermarkStrategy;
use crate::operations::streams::agent::{Agent, StreamDescriptor, discover_path_sessions};
use crate::operations::streams::reader::read_jsonl_byte_stream;
use crate::operations::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::operations::streams::timestamp::file_time_fallback;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Cursor agent that discovers conversations from Cursor storage.
pub struct CursorAgent {
    batch_size: usize,
}

impl CursorAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }

    fn conversation_roots() -> Vec<PathBuf> {
        let base_dir = if let Ok(config_dir) = std::env::var("CURSOR_CONFIG_DIR") {
            Some(PathBuf::from(config_dir))
        } else {
            dirs::home_dir().map(|p| p.join(".cursor"))
        };
        base_dir
            .into_iter()
            .map(|path| path.join("projects"))
            .collect()
    }

    /// Scan for Cursor conversation files in standard locations.
    fn scan_conversation_files() -> Vec<PathBuf> {
        collect_files_recursively(Self::conversation_roots(), |path| {
            path.extension()
                .map(|extension| extension == "jsonl")
                .unwrap_or(false)
        })
    }
}

impl Default for CursorAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for CursorAgent {
    fn trusted_stream_roots(&self) -> Vec<PathBuf> {
        Self::conversation_roots()
    }

    fn validate_checkpoint_stream(
        &self,
        source: &crate::model::checkpoint_request::StreamSource,
    ) -> Result<DiscoveredSession, StreamError> {
        crate::operations::streams::agent::validate_checkpoint_stream_file(
            source,
            "cursor",
            crate::model::checkpoint_request::StreamFormat::CursorJsonl,
            Self::conversation_roots(),
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
        // Poll every 30 minutes for new Cursor conversations
        SweepStrategy::Periodic(Duration::from_secs(30 * 60))
    }

    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError> {
        Ok(discover_path_sessions(
            "cursor",
            Self::scan_conversation_files(),
            stem_session_binding,
        ))
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
            "Cursor",
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
            StreamFormat::CursorJsonl,
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
        let agent = CursorAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    fn make_jsonl_line(i: usize) -> String {
        format!(
            r#"{{"role":"user","id":{},"message":{{"content":[{{"type":"text","text":"msg-{}"}}]}}}}"#,
            i, i
        )
    }

    #[test]
    fn test_stream_adapter_contract() {
        use tempfile::NamedTempFile;

        let file = NamedTempFile::new().unwrap();
        let mut fixture = jsonl_fixture(file.path(), make_jsonl_line);
        let agent = CursorAgent::with_batch_size(2);
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
            r#"{{"role":"user","message":{{"content":[{{"type":"text","text":"Hello"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"role":"assistant","message":{{"content":[{{"type":"text","text":"Hi there"}}]}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = CursorAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test-session")
            .unwrap();

        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0]["role"].as_str(), Some("user"));
        assert_eq!(result.events[1]["role"].as_str(), Some("assistant"));
    }
}
