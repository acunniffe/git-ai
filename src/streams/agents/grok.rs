//! Grok Build agent implementation with sweep discovery.

use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::mdm::utils::grok_home_dir;
use crate::streams::agent::{Agent, PathResolverKind, StreamDescriptor};
use crate::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::streams::types::{StreamBatch, StreamError};
use crate::streams::watermark::{ByteOffsetWatermark, WatermarkStrategy};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct GrokAgent {
    batch_size: usize,
}

impl GrokAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }

    fn scan_session_files() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let sessions_root = grok_home_dir().join("sessions");
        let Ok(workspace_dirs) = fs::read_dir(&sessions_root) else {
            return paths;
        };

        for workspace_entry in workspace_dirs.flatten() {
            let workspace_path = workspace_entry.path();
            if !workspace_path.is_dir() {
                continue;
            }
            let Ok(session_dirs) = fs::read_dir(&workspace_path) else {
                continue;
            };
            for session_entry in session_dirs.flatten() {
                let updates = session_entry.path().join("updates.jsonl");
                if updates.is_file() {
                    paths.push(updates);
                }
            }
        }

        paths
    }
}

impl Default for GrokAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for GrokAgent {
    fn batch_size_hint(&self) -> usize {
        self.batch_size
    }

    fn sweep_strategy(&self) -> SweepStrategy {
        SweepStrategy::Periodic(Duration::from_secs(30 * 60))
    }

    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError> {
        let mut sessions = Vec::new();
        for path in Self::scan_session_files() {
            let Some(external_session_id) = path
                .parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                .map(|s| s.to_string())
            else {
                continue;
            };
            let session_id = generate_session_id(&external_session_id, "grok");
            sessions.push(DiscoveredSession {
                session_id,
                tool: "grok".to_string(),
                stream_path: path,
                external_session_id,
                external_parent_session_id: None,
            });
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
                    "Grok reader requires ByteOffsetWatermark, got incompatible type for session {}",
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

        Ok(StreamBatch {
            events,
            new_watermark: Box::new(ByteOffsetWatermark::new(current_offset)),
        })
    }

    fn extract_event_ids(
        &self,
        event: &serde_json::Value,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let params = event.get("params");
        let event_id = params
            .and_then(|p| p.get("_meta"))
            .and_then(|meta| meta.get("eventId"))
            .or_else(|| {
                params
                    .and_then(|p| p.get("update"))
                    .and_then(|update| update.get("_meta"))
                    .and_then(|meta| meta.get("eventId"))
            })
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let tool_use_id = params
            .and_then(|p| p.get("update"))
            .and_then(|update| update.get("toolCallId"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        (event_id, None, tool_use_id)
    }

    fn extract_event_timestamp(
        &self,
        event: &serde_json::Value,
        file_meta: &std::fs::Metadata,
        is_first_event: bool,
    ) -> u32 {
        grok_event_timestamp(event)
            .unwrap_or_else(|| crate::streams::agent::file_time_fallback(file_meta, is_first_event))
    }

    fn infer_cwd(&self, stream_path: &Path) -> Option<PathBuf> {
        let summary_path = stream_path.parent()?.join("summary.json");
        let summary = fs::read_to_string(summary_path).ok()?;
        let data: serde_json::Value = serde_json::from_str(&summary).ok()?;
        data.get("info")
            .and_then(|info| info.get("cwd"))
            .and_then(|v| v.as_str())
            .filter(|cwd| !cwd.is_empty())
            .map(PathBuf::from)
    }

    fn streams(&self) -> Vec<StreamDescriptor> {
        let format = StreamFormat::GrokJsonl;
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

fn grok_event_timestamp(event: &serde_json::Value) -> Option<u32> {
    let ts_val = event.get("timestamp")?;
    if let Some(s) = ts_val.as_str() {
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.timestamp() as u32)
    } else if let Some(n) = ts_val.as_u64() {
        if n >= 1_000_000_000_000 {
            Some((n / 1000) as u32)
        } else {
            Some(n as u32)
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    #[test]
    fn test_sweep_strategy() {
        let agent = GrokAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    #[test]
    fn test_timestamp_treats_unix_seconds_as_seconds() {
        let event = serde_json::json!({"timestamp": 1788274022u64});
        assert_eq!(grok_event_timestamp(&event), Some(1788274022));
    }

    #[test]
    fn test_read_incremental_jsonl() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"timestamp":1788274022,"params":{{"_meta":{{"eventId":"e1"}},"update":{{"toolCallId":"call-1"}}}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"timestamp":1788274023,"params":{{"_meta":{{"eventId":"e2"}}}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = GrokAgent::with_batch_size(10);
        let batch = agent
            .read_incremental(file.path(), Box::new(ByteOffsetWatermark::new(0)), "test")
            .unwrap();
        assert_eq!(batch.events.len(), 2);
        assert_eq!(
            agent.extract_event_ids(&batch.events[0]),
            (Some("e1".to_string()), None, Some("call-1".to_string()))
        );
    }

    #[test]
    #[serial]
    fn test_scan_session_files_respects_grok_home() {
        let temp_dir = TempDir::new().unwrap();
        let session_dir = temp_dir
            .path()
            .join("sessions")
            .join("%2Ftmp%2Fproj")
            .join("sess-1");
        fs::create_dir_all(&session_dir).unwrap();
        let updates = session_dir.join("updates.jsonl");
        fs::write(&updates, "{}\n").unwrap();

        let prev = std::env::var_os("GROK_HOME");
        unsafe {
            std::env::set_var("GROK_HOME", temp_dir.path());
        }
        let paths = GrokAgent::scan_session_files();
        unsafe {
            match prev {
                Some(value) => std::env::set_var("GROK_HOME", value),
                None => std::env::remove_var("GROK_HOME"),
            }
        }
        assert_eq!(paths, vec![updates]);
    }
}
