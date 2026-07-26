//! Pi agent implementation with sweep discovery.

use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::WatermarkStrategy;
use crate::operations::streams::agent::{
    Agent, PathResolverKind, StreamDescriptor, read_jsonl_byte_stream,
};
use crate::operations::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::operations::streams::timestamp::event_timestamp_or_file_time;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Pi agent that reads Pi JSONL session files.
pub struct PiAgent {
    batch_size: usize,
}

impl PiAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }

    fn session_roots() -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Ok(path) = std::env::var("PI_CODING_AGENT_SESSION_DIR")
            && !path.trim().is_empty()
        {
            roots.push(PathBuf::from(path));
        }
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".pi/agent/sessions"));
        }
        roots
    }
}

impl Default for PiAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for PiAgent {
    fn validate_checkpoint_stream(
        &self,
        source: &crate::model::checkpoint_request::StreamSource,
    ) -> Result<DiscoveredSession, StreamError> {
        const MAX_HEADER_BYTES: u64 = 64 * 1024;
        crate::operations::streams::agent::validate_checkpoint_stream_file(
            source,
            "pi",
            crate::model::checkpoint_request::StreamFormat::PiJsonl,
            Self::session_roots(),
            |path| {
                if !crate::operations::streams::agent::checkpoint_stream_has_extension(
                    path, "jsonl",
                ) {
                    return None;
                }
                let file = std::fs::File::open(path).ok()?;
                let mut reader = BufReader::new(file).take(MAX_HEADER_BYTES + 1);
                let mut header_line = String::new();
                let header_bytes = reader.read_line(&mut header_line).ok()? as u64;
                if header_bytes > MAX_HEADER_BYTES {
                    return None;
                }
                let header: serde_json::Value = serde_json::from_str(&header_line).ok()?;
                if header.get("type")?.as_str()? != "session" {
                    return None;
                }
                Some((header.get("id")?.as_str()?.to_string(), None))
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
        // Discovery happens via presets, not filesystem scanning
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
            "Pi",
            "open",
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
        let format = StreamFormat::PiJsonl;
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
        let agent = PiAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    fn make_jsonl_line(i: usize) -> String {
        format!(
            r#"{{"type":"message","id":{},"message":{{"role":"user","content":"msg-{}"}}}}"#,
            i, i
        )
    }

    fn drain_all(
        agent: &PiAgent,
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

        let agent = PiAgent::with_batch_size(2);
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

        let agent = PiAgent::with_batch_size(2);
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

        let agent = PiAgent::with_batch_size(2);
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
        writeln!(file, r#"{{"type":"session","id":"s1"}}"#).unwrap();
        writeln!(
            file,
            r#"{{"type":"message","message":{{"role":"user","content":"Hello","timestamp":1704067200000}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"message","message":{{"role":"assistant","content":[{{"type":"text","text":"Hi"}}],"model":"claude-sonnet-4-20250514"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = PiAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        assert_eq!(result.events.len(), 3);
        assert_eq!(result.events[0]["type"].as_str(), Some("session"));
        assert_eq!(result.events[1]["type"].as_str(), Some("message"));
        assert_eq!(result.events[1]["message"]["role"].as_str(), Some("user"));
        assert_eq!(
            result.events[2]["message"]["role"].as_str(),
            Some("assistant")
        );
    }

    #[test]
    fn test_read_incremental_resumes_from_offset() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        let first_line = r#"{"type":"session","id":"s1"}"#;
        writeln!(file, "{}", first_line).unwrap();
        writeln!(
            file,
            r#"{{"type":"message","message":{{"role":"user","content":"Hello"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = PiAgent::new();
        // Set offset past the first line to simulate resuming
        let offset = (first_line.len() + 1) as u64;
        let watermark = Box::new(ByteOffsetWatermark::new(offset));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0]["type"].as_str(), Some("message"));
    }

    #[test]
    fn test_read_incremental_thinking_and_tool_call() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"session","id":"s1"}}"#).unwrap();
        writeln!(
            file,
            r#"{{"type":"message","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"hmm"}},{{"type":"toolCall","name":"bash","arguments":{{}}}}]}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = PiAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        // Raw: session header + the message entry (both content blocks in single JSON line)
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0]["type"].as_str(), Some("session"));
        assert_eq!(result.events[1]["type"].as_str(), Some("message"));
    }
}
