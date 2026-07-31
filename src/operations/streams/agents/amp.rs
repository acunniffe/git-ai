//! Amp agent implementation with sweep discovery.

use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::WatermarkStrategy;
use crate::operations::streams::agent::{Agent, StreamDescriptor, discover_path_sessions};
use crate::operations::streams::reader::{
    JsonArrayStreamSpec, RecordIndexAdvancePolicy, read_json_array_stream,
};
use crate::operations::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::operations::streams::timestamp::event_timestamp_or_file_time;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Amp agent that discovers conversations from Amp thread JSON files.
pub struct AmpAgent {
    batch_size: usize,
}

impl AmpAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }

    /// Returns the path to Amp thread files.
    ///
    /// Checks `GIT_AI_AMP_THREADS_PATH` env var first, then falls back to
    /// platform-specific default locations.
    pub fn amp_threads_path() -> Result<PathBuf, StreamError> {
        if let Ok(path) = std::env::var("GIT_AI_AMP_THREADS_PATH") {
            return Ok(PathBuf::from(path));
        }

        #[cfg(target_os = "macos")]
        {
            if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
                return Ok(PathBuf::from(xdg).join("amp/threads"));
            }
            if let Some(home) = dirs::home_dir() {
                return Ok(home.join(".local/share/amp/threads"));
            }
        }

        #[cfg(target_os = "linux")]
        {
            if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
                return Ok(PathBuf::from(xdg).join("amp/threads"));
            }
            if let Some(home) = dirs::home_dir() {
                return Ok(home.join(".local/share/amp/threads"));
            }
        }

        #[cfg(target_os = "windows")]
        {
            if let Ok(local) = std::env::var("LOCALAPPDATA") {
                return Ok(PathBuf::from(local).join("amp/threads"));
            }
            if let Ok(appdata) = std::env::var("APPDATA") {
                return Ok(PathBuf::from(appdata).join("amp/threads"));
            }
        }

        Err(StreamError::Fatal {
            message: "Could not determine Amp threads path".to_string(),
        })
    }
}

impl Default for AmpAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for AmpAgent {
    fn trusted_stream_roots(&self) -> Vec<PathBuf> {
        Self::amp_threads_path().into_iter().collect()
    }

    fn validate_checkpoint_stream(
        &self,
        source: &crate::model::checkpoint_request::StreamSource,
    ) -> Result<DiscoveredSession, StreamError> {
        crate::operations::streams::agent::validate_checkpoint_stream_file(
            source,
            "amp",
            crate::model::checkpoint_request::StreamFormat::AmpThreadJson,
            Self::amp_threads_path().into_iter().collect(),
            |path| {
                if !crate::operations::streams::agent::checkpoint_stream_has_extension(path, "json")
                {
                    return None;
                }
                Some((path.file_stem()?.to_str()?.to_string(), None))
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
        let threads_dir = match Self::amp_threads_path() {
            Ok(p) => p,
            Err(_) => return Ok(Vec::new()),
        };

        if !threads_dir.exists() {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(&threads_dir).map_err(|e| StreamError::Transient {
            message: format!("Failed to read Amp threads directory: {}", e),
            retry_after: Duration::from_secs(30),
        })?;

        let paths = entries.flatten().map(|entry| entry.path()).filter(|path| {
            path.is_file()
                && path.extension().and_then(|extension| extension.to_str()) == Some("json")
        });
        Ok(discover_path_sessions("amp", paths, |path| {
            Some((path.file_stem()?.to_str()?.to_string(), None))
        }))
    }

    fn read_incremental(
        &self,
        path: &Path,
        watermark: Box<dyn WatermarkStrategy>,
        session_id: &str,
    ) -> Result<StreamBatch, StreamError> {
        read_json_array_stream(
            path,
            watermark,
            session_id,
            JsonArrayStreamSpec::new(
                "Amp",
                "messages",
                self.batch_size_hint(),
                RecordIndexAdvancePolicy::ConvertedUsize,
            ),
            |path| StreamError::Fatal {
                message: format!(
                    "Missing 'messages' array in Amp thread file: {}",
                    path.display()
                ),
            },
            || false,
        )
    }

    fn extract_event_timestamp(
        &self,
        event: &serde_json::Value,
        file_meta: &std::fs::Metadata,
        is_first_event: bool,
    ) -> u32 {
        event_timestamp_or_file_time(event, file_meta, is_first_event)
    }

    fn streams(&self) -> Vec<StreamDescriptor> {
        vec![StreamDescriptor::identity_transcript(
            StreamFormat::AmpThreadJson,
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::stream_watermark::RecordIndexWatermark;
    use crate::operations::streams::agents::test_support::{
        StreamAdapterContractCapabilities, assert_stream_adapter_contract, rewritten_file_fixture,
    };

    #[test]
    fn test_sweep_strategy() {
        let agent = AmpAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    fn make_amp_json(message_count: usize) -> String {
        let messages: Vec<String> = (0..message_count)
            .map(|i| {
                format!(
                    r#"{{"role":"user","id":{},"content":[{{"type":"text","text":"msg-{}"}}]}}"#,
                    i, i
                )
            })
            .collect();
        format!(
            r#"{{"id":"thread-test","messages":[{}]}}"#,
            messages.join(",")
        )
    }

    #[test]
    fn test_stream_adapter_contract() {
        use tempfile::NamedTempFile;

        let file = NamedTempFile::new().unwrap();
        let mut fixture = rewritten_file_fixture(file.path(), make_amp_json);

        assert_stream_adapter_contract(
            &AmpAgent::with_batch_size(2),
            &mut fixture,
            || Box::new(RecordIndexWatermark::new(0)),
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

        let json = serde_json::json!({
            "id": "thread-123",
            "messages": [
                {
                    "role": "user",
                    "content": [{"type": "text", "text": "Hello"}],
                    "meta": {"sentAt": 1704067200000i64}
                },
                {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "Hi"}],
                    "usage": {"model": "claude-sonnet-4-20250514", "timestamp": "2025-01-01T00:00:01Z"}
                }
            ]
        });

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", json).unwrap();
        file.flush().unwrap();

        let agent = AmpAgent::new();
        let watermark = Box::new(RecordIndexWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0]["role"], "user");
        assert_eq!(result.events[1]["role"], "assistant");
        assert_eq!(
            result.events[1]["usage"]["model"],
            "claude-sonnet-4-20250514"
        );
    }

    #[test]
    fn test_read_incremental_skips_processed() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let json = serde_json::json!({
            "id": "thread-123",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Old"}]},
                {"role": "user", "content": [{"type": "text", "text": "New"}]}
            ]
        });

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", json).unwrap();
        file.flush().unwrap();

        let agent = AmpAgent::new();
        let watermark = Box::new(RecordIndexWatermark::new(1));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0]["content"][0]["text"], "New");
    }

    #[test]
    fn test_read_incremental_thinking_and_tool_use() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let json = serde_json::json!({
            "id": "thread-456",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "Let me think..."},
                        {"type": "text", "text": "Here's the result"},
                        {"type": "tool_use", "id": "tu-1", "name": "bash", "input": {}}
                    ]
                }
            ]
        });

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", json).unwrap();
        file.flush().unwrap();

        let agent = AmpAgent::new();
        let watermark = Box::new(RecordIndexWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        // One raw message containing all content items
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0]["role"], "assistant");
        let content = result.events[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[2]["type"], "tool_use");
    }
}
