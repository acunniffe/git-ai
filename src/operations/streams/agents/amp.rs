//! Amp agent implementation with sweep discovery.

use crate::model::authorship_log_serialization::generate_session_id;
use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::{RecordIndexWatermark, WatermarkStrategy};
use crate::operations::streams::agent::{Agent, PathResolverKind, StreamDescriptor};
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

        let mut sessions = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            let Some(file_stem) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
            else {
                continue;
            };

            let session_id = generate_session_id(&file_stem, "amp");
            let session = DiscoveredSession {
                session_id,
                tool: "amp".to_string(),
                stream_path: path,
                external_session_id: file_stem,
                external_parent_session_id: None,
            };

            sessions.push(session);
        }

        Ok(sessions)
    }

    fn read_incremental(
        &self,
        path: &Path,
        watermark: Box<dyn WatermarkStrategy>,
        session_id: &str,
    ) -> Result<StreamBatch, StreamError> {
        // Downcast watermark to RecordIndexWatermark
        let record_watermark = watermark
            .as_any()
            .downcast_ref::<RecordIndexWatermark>()
            .ok_or_else(|| StreamError::Fatal {
                message: format!(
                    "Amp reader requires RecordIndexWatermark, got incompatible type for session {}",
                    session_id
                ),
            })?;

        let skip_count = record_watermark.0 as usize;

        let file = fs::File::open(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StreamError::Fatal {
                    message: format!("Transcript file not found: {}", path.display()),
                }
            } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                StreamError::Fatal {
                    message: format!("Permission denied reading transcript: {}", path.display()),
                }
            } else {
                StreamError::Transient {
                    message: format!("Failed to read transcript file: {}", e),
                    retry_after: Duration::from_secs(5),
                }
            }
        })?;

        let reader = std::io::BufReader::new(file);
        let mut parsed: serde_json::Value =
            serde_json::from_reader(reader).map_err(|e| StreamError::Parse {
                line: 0,
                message: format!("Invalid JSON in {}: {}", path.display(), e),
            })?;

        let messages = match parsed
            .as_object_mut()
            .and_then(|obj| obj.remove("messages"))
        {
            Some(serde_json::Value::Array(arr)) => arr,
            _ => {
                return Err(StreamError::Fatal {
                    message: format!(
                        "Missing 'messages' array in Amp thread file: {}",
                        path.display()
                    ),
                });
            }
        };

        let batch_limit = self.batch_size_hint();

        // Skip first `skip_count` messages (already processed), take up to batch_limit
        let events: Vec<serde_json::Value> = messages
            .into_iter()
            .skip(skip_count)
            .take(batch_limit)
            .collect();

        let new_watermark = Box::new(RecordIndexWatermark::new(
            (skip_count + events.len()) as u64,
        ));

        Ok(StreamBatch {
            events,
            new_watermark,
        })
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
        let format = StreamFormat::AmpThreadJson;
        vec![StreamDescriptor {
            stream_kind: "transcript",
            format,
            watermark_type: format.watermark_type(),
            path_resolver: PathResolverKind::Identity,
            shared: false,
            watermark_type_resolver: None,
            format_resolver: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn drain_all(
        agent: &AmpAgent,
        path: &Path,
    ) -> (Vec<serde_json::Value>, Box<dyn WatermarkStrategy>) {
        let mut all = Vec::new();
        let mut wm: Box<dyn WatermarkStrategy> = Box::new(RecordIndexWatermark::new(0));
        loop {
            let batch = agent.read_incremental(path, wm, "test").unwrap();
            if batch.events.is_empty() {
                wm = batch.new_watermark;
                break;
            }
            all.extend(batch.events);
            wm = batch.new_watermark;
        }
        (all, wm)
    }

    #[test]
    fn test_batch_resume_no_loss_or_repeat() {
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, make_amp_json(5).as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();

        let agent = AmpAgent::with_batch_size(2);
        let (events, _) = drain_all(&agent, file.path());

        assert_eq!(events.len(), 5);
        let ids: Vec<u64> = events.iter().map(|e| e["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_append_one_record_after_full_read() {
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, make_amp_json(3).as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();

        let agent = AmpAgent::with_batch_size(2);
        let (all, wm) = drain_all(&agent, file.path());
        assert_eq!(all.len(), 3);

        std::fs::write(file.path(), make_amp_json(4)).unwrap();

        let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["id"].as_u64().unwrap(), 3);
    }

    #[test]
    fn test_append_several_records_after_full_read() {
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, make_amp_json(3).as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();

        let agent = AmpAgent::with_batch_size(2);
        let (_, mut wm) = drain_all(&agent, file.path());

        std::fs::write(file.path(), make_amp_json(6)).unwrap();

        let mut new_events = Vec::new();
        loop {
            let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
            wm = batch.new_watermark;
            if batch.events.is_empty() {
                break;
            }
            new_events.extend(batch.events);
        }
        assert_eq!(new_events.len(), 3);
        let ids: Vec<u64> = new_events
            .iter()
            .map(|e| e["id"].as_u64().unwrap())
            .collect();
        assert_eq!(ids, vec![3, 4, 5]);
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
