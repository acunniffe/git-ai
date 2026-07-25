//! Droid agent implementation with sweep discovery.

use crate::model::authorship_log_serialization::generate_session_id;
use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::{HybridWatermark, WatermarkStrategy};
use crate::operations::streams::agent::{Agent, PathResolverKind, StreamDescriptor};
use crate::operations::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Droid agent that discovers conversations from Droid storage.
pub struct DroidAgent {
    batch_size: usize,
}

impl DroidAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }

    fn conversation_roots() -> Vec<PathBuf> {
        dirs::home_dir()
            .into_iter()
            .map(|path| path.join(".factory/sessions"))
            .collect()
    }

    /// Scan for Droid conversation files in standard locations.
    fn scan_conversation_files() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        // Droid transcripts are stored in ~/.factory/sessions/<project-dir>/<uuid>.jsonl.
        for sessions_dir in Self::conversation_roots() {
            if sessions_dir.exists() {
                // Recursively scan all project directories under sessions/
                Self::scan_jsonl_recursive(&sessions_dir, &mut paths);
            }
        }

        paths
    }

    /// Recursively scan directory for *.jsonl files (excluding .settings.json).
    fn scan_jsonl_recursive(dir: &Path, paths: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::scan_jsonl_recursive(&path, paths);
            } else if path.is_file()
                && path.extension().map(|ext| ext == "jsonl").unwrap_or(false)
                && !path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.contains(".settings."))
                    .unwrap_or(false)
            {
                paths.push(path);
            }
        }
    }
}

impl Default for DroidAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for DroidAgent {
    fn trusted_stream_roots(&self) -> Vec<PathBuf> {
        Self::conversation_roots()
    }

    fn validate_checkpoint_stream(
        &self,
        source: &crate::model::checkpoint_request::StreamSource,
    ) -> Result<DiscoveredSession, StreamError> {
        crate::operations::streams::agent::validate_checkpoint_stream_file(
            source,
            "droid",
            crate::model::checkpoint_request::StreamFormat::DroidJsonl,
            Self::conversation_roots(),
            |path| {
                if !crate::operations::streams::agent::checkpoint_stream_has_extension(
                    path, "jsonl",
                ) {
                    return None;
                }
                let name = path.file_name()?.to_str()?;
                if name.contains(".settings.") {
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
        // Poll every 30 minutes for new Droid conversations
        SweepStrategy::Periodic(Duration::from_secs(30 * 60))
    }

    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError> {
        let paths = Self::scan_conversation_files();
        let mut sessions = Vec::new();

        for path in paths {
            // Droid session_id from the hook payload matches the file stem
            let Some(external_session_id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
            else {
                continue;
            };
            let session_id = generate_session_id(&external_session_id, "droid");

            let session = DiscoveredSession {
                session_id,
                tool: "droid".to_string(),
                stream_path: path,
                external_session_id,
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
        use std::fs::File;
        use std::io::{BufReader, Seek, SeekFrom};

        // Downcast watermark to HybridWatermark
        let hybrid_watermark = watermark
            .as_any()
            .downcast_ref::<HybridWatermark>()
            .ok_or_else(|| StreamError::Fatal {
                message: format!(
                    "Droid reader requires HybridWatermark, got incompatible type for session {}",
                    session_id
                ),
            })?;

        let start_offset = hybrid_watermark.offset;
        let mut record_count = hybrid_watermark.record;

        // Open file
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
                    retry_after: std::time::Duration::from_secs(5),
                }
            }
        })?;

        let mut reader = BufReader::new(file);

        // Seek to watermark position
        reader
            .seek(SeekFrom::Start(start_offset))
            .map_err(|e| StreamError::Transient {
                message: format!("Failed to seek to offset {}: {}", start_offset, e),
                retry_after: std::time::Duration::from_secs(5),
            })?;

        let batch_limit = self.batch_size_hint();
        let mut events = Vec::with_capacity(batch_limit);
        let mut current_offset = start_offset;
        let mut line_number = 0;
        let mut latest_timestamp: Option<chrono::DateTime<chrono::Utc>> =
            hybrid_watermark.timestamp;

        // Read lines from watermark position
        let mut line = String::new();
        loop {
            match crate::model::stream_types::read_jsonl_line(&mut reader, &mut line).map_err(
                |e| StreamError::Transient {
                    message: format!("I/O error reading line: {}", e),
                    retry_after: std::time::Duration::from_secs(5),
                },
            )? {
                crate::model::stream_types::JsonlLineState::Eof => break,
                crate::model::stream_types::JsonlLineState::Partial => break,
                crate::model::stream_types::JsonlLineState::Complete(bytes_read) => {
                    line_number += 1;
                    current_offset += bytes_read as u64;
                }
            }

            // Skip empty lines
            if line.trim().is_empty() {
                continue;
            }

            // Parse JSONL entry
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

            // Only process "message" entries; skip session_start, todo_state, etc.
            if entry["type"].as_str() != Some("message") {
                continue;
            }

            // Track record count for hybrid watermark
            record_count += 1;

            // Update latest_timestamp for hybrid watermark
            if let Some(ts_str) = entry["timestamp"].as_str()
                && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str)
            {
                let utc_dt = dt.with_timezone(&chrono::Utc);
                if latest_timestamp.is_none() || Some(utc_dt) > latest_timestamp {
                    latest_timestamp = Some(utc_dt);
                }
            }

            // Push raw JSON entry
            events.push(entry);
            if events.len() >= batch_limit {
                break;
            }
        }

        // Create new hybrid watermark with updated offset, record count, and timestamp
        let new_watermark = Box::new(HybridWatermark::new(
            current_offset,
            record_count,
            latest_timestamp,
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
        crate::operations::daemon::stream_worker::extract_event_timestamp(event).unwrap_or_else(
            || crate::operations::streams::agent::file_time_fallback(file_meta, is_first_event),
        )
    }

    fn streams(&self) -> Vec<StreamDescriptor> {
        let format = StreamFormat::DroidJsonl;
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

    fn make_droid_line(i: usize) -> String {
        format!(
            r#"{{"type":"message","id":{},"timestamp":"2025-01-01T00:00:{:02}Z","message":{{"role":"user","content":[{{"type":"text","text":"msg-{}"}}]}}}}"#,
            i, i, i
        )
    }

    fn drain_all(
        agent: &DroidAgent,
        path: &Path,
    ) -> (Vec<serde_json::Value>, Box<dyn WatermarkStrategy>) {
        let mut all = Vec::new();
        let mut wm: Box<dyn WatermarkStrategy> = Box::new(HybridWatermark::new(0, 0, None));
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
            writeln!(file, "{}", make_droid_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = DroidAgent::with_batch_size(2);
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
            writeln!(file, "{}", make_droid_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = DroidAgent::with_batch_size(2);
        let (all, wm) = drain_all(&agent, file.path());
        assert_eq!(all.len(), 3);

        let mut f = OpenOptions::new().append(true).open(file.path()).unwrap();
        writeln!(f, "{}", make_droid_line(3)).unwrap();
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
            writeln!(file, "{}", make_droid_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = DroidAgent::with_batch_size(2);
        let (_, mut wm) = drain_all(&agent, file.path());

        let mut f = OpenOptions::new().append(true).open(file.path()).unwrap();
        for i in 3..6 {
            writeln!(f, "{}", make_droid_line(i)).unwrap();
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
    fn test_sweep_strategy() {
        let agent = DroidAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    #[test]
    fn test_read_incremental_basic() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"message","timestamp":"2025-01-01T00:00:00Z","message":{{"role":"user","content":[{{"type":"text","text":"Hello"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"message","timestamp":"2025-01-01T00:00:01Z","message":{{"role":"assistant","content":[{{"type":"text","text":"Hi there"}}]}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = DroidAgent::new();
        let watermark = Box::new(HybridWatermark::new(0, 0, None));
        let result = agent
            .read_incremental(file.path(), watermark, "test-session")
            .unwrap();

        assert_eq!(result.events.len(), 2);

        // Verify raw JSON events
        assert_eq!(result.events[0]["type"], "message");
        assert_eq!(result.events[0]["message"]["role"], "user");
        assert_eq!(result.events[1]["type"], "message");
        assert_eq!(result.events[1]["message"]["role"], "assistant");

        // Verify hybrid watermark was updated
        let new_watermark = result
            .new_watermark
            .as_any()
            .downcast_ref::<HybridWatermark>()
            .unwrap();
        assert!(new_watermark.offset > 0); // Byte offset advanced
        assert_eq!(new_watermark.record, 2); // Two message records processed
        assert!(new_watermark.timestamp.is_some()); // Timestamp captured
    }
}
