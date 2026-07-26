use crate::model::authorship_log_serialization::generate_session_id;
use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::WatermarkStrategy;
use crate::operations::streams::agent::{
    Agent, PathResolverKind, StreamDescriptor, read_jsonl_byte_stream,
};
use crate::operations::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::operations::streams::timestamp::event_timestamp_or_file_time;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct CopilotCliAgent {
    batch_size: usize,
}

impl CopilotCliAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }

    fn session_state_base_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".copilot/session-state"))
    }
}

impl Default for CopilotCliAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for CopilotCliAgent {
    fn trusted_stream_roots(&self) -> Vec<PathBuf> {
        Self::session_state_base_dir().into_iter().collect()
    }

    fn validate_checkpoint_stream(
        &self,
        source: &crate::model::checkpoint_request::StreamSource,
    ) -> Result<DiscoveredSession, StreamError> {
        crate::operations::streams::agent::validate_checkpoint_stream_file(
            source,
            "github-copilot-cli",
            crate::model::checkpoint_request::StreamFormat::CopilotEventStreamJsonl,
            Self::session_state_base_dir().into_iter().collect(),
            |path| {
                if path.file_name()?.to_str()? != "events.jsonl" {
                    return None;
                }
                Some((path.parent()?.file_name()?.to_str()?.to_string(), None))
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
        let Some(base_dir) = Self::session_state_base_dir() else {
            return Ok(Vec::new());
        };

        if !base_dir.exists() {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(&base_dir).map_err(|e| StreamError::Transient {
            message: format!("Failed to read copilot session-state dir: {}", e),
            retry_after: Duration::from_secs(60),
        })?;

        let mut sessions = Vec::new();

        for entry in entries.flatten() {
            let dir_path = entry.path();
            if !dir_path.is_dir() {
                continue;
            }

            let events_path = dir_path.join("events.jsonl");
            if !events_path.exists() {
                continue;
            }

            let Some(external_session_id) = dir_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
            else {
                continue;
            };

            let session_id = generate_session_id(&external_session_id, "github-copilot-cli");

            sessions.push(DiscoveredSession {
                session_id,
                tool: "github-copilot-cli".to_string(),
                stream_path: events_path,
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
        read_jsonl_byte_stream(
            path,
            watermark,
            session_id,
            self.batch_size_hint(),
            "Copilot event stream",
            "open",
        )
    }

    fn extract_event_ids(
        &self,
        event: &serde_json::Value,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let id = event.get("id").and_then(|v| v.as_str()).map(String::from);
        let parent_id = event
            .get("parentId")
            .and_then(|v| v.as_str())
            .map(String::from);
        (id, parent_id, None)
    }

    fn extract_event_timestamp(
        &self,
        event: &serde_json::Value,
        file_meta: &std::fs::Metadata,
        is_first_event: bool,
    ) -> u32 {
        event_timestamp_or_file_time(event, file_meta, is_first_event)
    }

    fn infer_cwd(&self, stream_path: &Path) -> Option<PathBuf> {
        use std::io::{BufRead, BufReader};

        let file = fs::File::open(stream_path).ok()?;
        let reader = BufReader::new(file);

        for line in reader.lines().map_while(Result::ok).take(5) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Some(json) = serde_json::from_str::<serde_json::Value>(trimmed).ok() else {
                continue;
            };
            if json.get("type").and_then(|v| v.as_str()) == Some("session.start") {
                return json
                    .get("data")
                    .and_then(|d| d.get("context"))
                    .and_then(|c| c.get("cwd"))
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from);
            }
        }
        None
    }

    fn streams(&self) -> Vec<StreamDescriptor> {
        let format = StreamFormat::CopilotEventStreamJsonl;
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
    use crate::model::stream_watermark::ByteOffsetWatermark;

    #[test]
    fn test_sweep_strategy() {
        let agent = CopilotCliAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    fn make_event_line(i: usize) -> String {
        format!(
            r#"{{"type":"user.message","data":{{"content":"msg-{}"}},"id":"evt-{}","timestamp":"2026-05-12T00:00:{:02}Z","parentId":null}}"#,
            i, i, i
        )
    }

    fn drain_all(
        agent: &CopilotCliAgent,
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

        let mut file = NamedTempFile::with_suffix(".jsonl").unwrap();
        for i in 0..5 {
            writeln!(file, "{}", make_event_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = CopilotCliAgent::with_batch_size(2);
        let (events, _) = drain_all(&agent, file.path());

        assert_eq!(events.len(), 5);
        let ids: Vec<String> = events
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["evt-0", "evt-1", "evt-2", "evt-3", "evt-4"]);
    }

    #[test]
    fn test_append_after_full_read() {
        use std::fs::OpenOptions;
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::with_suffix(".jsonl").unwrap();
        for i in 0..3 {
            writeln!(file, "{}", make_event_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = CopilotCliAgent::with_batch_size(2);
        let (all, wm) = drain_all(&agent, file.path());
        assert_eq!(all.len(), 3);

        let mut f = OpenOptions::new().append(true).open(file.path()).unwrap();
        writeln!(f, "{}", make_event_line(3)).unwrap();
        writeln!(f, "{}", make_event_line(4)).unwrap();
        f.flush().unwrap();

        let mut new_events = Vec::new();
        let mut wm = wm;
        loop {
            let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
            wm = batch.new_watermark;
            if batch.events.is_empty() {
                break;
            }
            new_events.extend(batch.events);
        }
        assert_eq!(new_events.len(), 2);
        let ids: Vec<String> = new_events
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["evt-3", "evt-4"]);
    }

    #[test]
    fn test_infer_cwd() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::with_suffix(".jsonl").unwrap();
        writeln!(
            file,
            r#"{{"type":"session.start","data":{{"sessionId":"test","context":{{"cwd":"/Users/test/myproject","gitRoot":"/Users/test/myproject"}}}},"id":"e1","timestamp":"2026-01-01T00:00:00Z","parentId":null}}"#
        )
        .unwrap();
        writeln!(file, "{}", make_event_line(0)).unwrap();
        file.flush().unwrap();

        let agent = CopilotCliAgent::new();
        let cwd = agent.infer_cwd(file.path());
        assert_eq!(cwd, Some(PathBuf::from("/Users/test/myproject")));
    }

    #[test]
    fn test_extract_event_ids() {
        let agent = CopilotCliAgent::new();
        let event = serde_json::json!({
            "type": "user.message",
            "id": "evt-123",
            "parentId": "evt-122",
            "data": {}
        });
        let (id, parent_id, tool_use_id) = agent.extract_event_ids(&event);
        assert_eq!(id, Some("evt-123".to_string()));
        assert_eq!(parent_id, Some("evt-122".to_string()));
        assert_eq!(tool_use_id, None);
    }

    #[test]
    fn test_extract_event_ids_null_parent() {
        let agent = CopilotCliAgent::new();
        let event = serde_json::json!({
            "type": "session.start",
            "id": "evt-001",
            "parentId": null,
            "data": {}
        });
        let (id, parent_id, _) = agent.extract_event_ids(&event);
        assert_eq!(id, Some("evt-001".to_string()));
        assert_eq!(parent_id, None);
    }
}
