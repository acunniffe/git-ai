//! Windsurf agent implementation with sweep discovery.

use crate::streams::agent::{Agent, PathResolverKind, StreamDescriptor};
use crate::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::streams::types::{StreamBatch, StreamError};
use crate::streams::watermark::{ByteOffsetWatermark, WatermarkStrategy};
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::Path;
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
}

impl Default for WindsurfAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for WindsurfAgent {
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
        let byte_watermark = watermark
            .as_any()
            .downcast_ref::<ByteOffsetWatermark>()
            .ok_or_else(|| StreamError::Fatal {
                message: format!(
                    "Windsurf reader requires ByteOffsetWatermark, got incompatible type for session {}",
                    session_id
                ),
            })?;

        let start_offset = byte_watermark.0;

        let file = File::open(path).map_err(|e| {
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
                    message: format!("Failed to open transcript file: {}", e),
                    retry_after: Duration::from_secs(5),
                }
            }
        })?;

        let mut reader = BufReader::new(file);

        reader
            .seek(SeekFrom::Start(start_offset))
            .map_err(|e| StreamError::Transient {
                message: format!("Failed to seek to offset {}: {}", start_offset, e),
                retry_after: Duration::from_secs(5),
            })?;

        let batch_limit = self.batch_size_hint();
        let mut events = Vec::with_capacity(batch_limit);
        let mut batch_bytes: usize = 0;
        let mut current_offset = start_offset;
        let mut line_number = 0;

        let mut line = String::new();
        loop {
            match crate::streams::types::read_jsonl_line(&mut reader, &mut line).map_err(|e| {
                StreamError::Transient {
                    message: format!("I/O error reading line: {}", e),
                    retry_after: Duration::from_secs(5),
                }
            })? {
                crate::streams::types::JsonlLineState::Eof => break,
                crate::streams::types::JsonlLineState::Partial => break,
                crate::streams::types::JsonlLineState::Complete(bytes_read) => {
                    line_number += 1;
                    current_offset += bytes_read as u64;
                }
                crate::streams::types::JsonlLineState::Oversized(bytes_read) => {
                    line_number += 1;
                    current_offset += bytes_read as u64;
                    tracing::warn!(
                        line = line_number,
                        path = %path.display(),
                        max_bytes = crate::streams::types::MAX_JSONL_LINE_BYTES,
                        "skipping oversized transcript line"
                    );
                    continue;
                }
            }

            if line.trim().is_empty() {
                continue;
            }

            let entry: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        line = line_number,
                        path = %path.display(),
                        error = %e,
                        "skipping malformed JSON line"
                    );
                    continue;
                }
            };

            batch_bytes += line.len();
            events.push(entry);
            if events.len() >= batch_limit || batch_bytes >= crate::streams::types::MAX_BATCH_BYTES
            {
                break;
            }
        }

        let new_watermark = Box::new(ByteOffsetWatermark::new(current_offset));

        Ok(StreamBatch {
            events,
            new_watermark,
        })
    }

    fn extract_event_timestamp(
        &self,
        _event: &serde_json::Value,
        file_meta: &std::fs::Metadata,
        is_first_event: bool,
    ) -> u32 {
        crate::streams::agent::file_time_fallback(file_meta, is_first_event)
    }

    fn streams(&self) -> Vec<StreamDescriptor> {
        let format = StreamFormat::WindsurfJsonl;
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

    fn drain_all(
        agent: &WindsurfAgent,
        path: &Path,
    ) -> (Vec<serde_json::Value>, Box<dyn WatermarkStrategy>) {
        let mut all = Vec::new();
        let mut wm: Box<dyn WatermarkStrategy> = Box::new(ByteOffsetWatermark::new(0));
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
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        for i in 0..5 {
            writeln!(file, "{}", make_jsonl_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = WindsurfAgent::with_batch_size(2);
        let (events, _) = drain_all(&agent, file.path());

        assert_eq!(events.len(), 5);
        let ids: Vec<u64> = events.iter().map(|e| e["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_append_one_record_after_full_read() {
        use std::fs::OpenOptions;
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        for i in 0..3 {
            writeln!(file, "{}", make_jsonl_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = WindsurfAgent::with_batch_size(2);
        let (all, wm) = drain_all(&agent, file.path());
        assert_eq!(all.len(), 3);

        let mut f = OpenOptions::new().append(true).open(file.path()).unwrap();
        writeln!(f, "{}", make_jsonl_line(3)).unwrap();
        f.flush().unwrap();

        let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["id"].as_u64().unwrap(), 3);
    }

    #[test]
    fn test_append_several_records_after_full_read() {
        use std::fs::OpenOptions;
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        for i in 0..3 {
            writeln!(file, "{}", make_jsonl_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = WindsurfAgent::with_batch_size(2);
        let (_, mut wm) = drain_all(&agent, file.path());

        let mut f = OpenOptions::new().append(true).open(file.path()).unwrap();
        for i in 3..6 {
            writeln!(f, "{}", make_jsonl_line(i)).unwrap();
        }
        f.flush().unwrap();

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
