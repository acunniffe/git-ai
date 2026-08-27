//! Cursor agent implementation with sweep discovery.

use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::config::Config;
use crate::mdm::profile_roots::{AgentProfile, agent_profile_roots};
use crate::streams::agent::{Agent, PathResolverKind, StreamDescriptor};
use crate::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::streams::types::{StreamBatch, StreamError};
use crate::streams::watermark::{ByteOffsetWatermark, WatermarkStrategy};
use std::fs;
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

    /// Scan for Cursor conversation files in standard locations.
    fn scan_conversation_files() -> Vec<PathBuf> {
        let config = Config::fresh();
        let roots = agent_profile_roots(AgentProfile::Cursor, &config);
        Self::scan_conversation_files_in_roots(&roots)
    }

    fn scan_conversation_files_in_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for root in roots {
            let dir = root.join("projects");
            if dir.exists() {
                Self::scan_jsonl_recursive(&dir, &mut paths);
            }
        }

        paths
    }

    fn scan_jsonl_recursive(dir: &Path, paths: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::scan_jsonl_recursive(&path, paths);
            } else if path.is_file() && path.extension().map(|ext| ext == "jsonl").unwrap_or(false)
            {
                paths.push(path);
            }
        }
    }
}

impl Default for CursorAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for CursorAgent {
    fn batch_size_hint(&self) -> usize {
        self.batch_size
    }

    fn sweep_strategy(&self) -> SweepStrategy {
        // Poll every 30 minutes for new Cursor conversations
        SweepStrategy::Periodic(Duration::from_secs(30 * 60))
    }

    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError> {
        let paths = Self::scan_conversation_files();
        let mut sessions = Vec::new();

        for path in paths {
            // Cursor conversation_id is the file stem
            let Some(external_session_id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
            else {
                continue;
            };
            let session_id = generate_session_id(&external_session_id, "cursor");

            let session = DiscoveredSession {
                session_id,
                tool: "cursor".to_string(),
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

        let byte_watermark = watermark
            .as_any()
            .downcast_ref::<ByteOffsetWatermark>()
            .ok_or_else(|| StreamError::Fatal {
                message: format!(
                    "Cursor reader requires ByteOffsetWatermark, got incompatible type for session {}",
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
                    retry_after: std::time::Duration::from_secs(5),
                }
            }
        })?;

        let mut reader = BufReader::new(file);

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

        let mut line = String::new();
        loop {
            match crate::streams::types::read_jsonl_line(&mut reader, &mut line).map_err(|e| {
                StreamError::Transient {
                    message: format!("I/O error reading line: {}", e),
                    retry_after: std::time::Duration::from_secs(5),
                }
            })? {
                crate::streams::types::JsonlLineState::Eof => break,
                crate::streams::types::JsonlLineState::Partial => break,
                crate::streams::types::JsonlLineState::Complete(bytes_read) => {
                    line_number += 1;
                    current_offset += bytes_read as u64;
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

            events.push(entry);
            if events.len() >= batch_limit {
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
        let format = StreamFormat::CursorJsonl;
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
    fn configured_profile_roots_are_scanned() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".cursor-work");
        let transcript = profile.join("projects/repo/session.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, "{}\n").unwrap();

        assert_eq!(
            CursorAgent::scan_conversation_files_in_roots(&[profile]),
            vec![transcript]
        );
    }

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

    fn drain_all(
        agent: &CursorAgent,
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

        let agent = CursorAgent::with_batch_size(2);
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

        let agent = CursorAgent::with_batch_size(2);
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

        let agent = CursorAgent::with_batch_size(2);
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
