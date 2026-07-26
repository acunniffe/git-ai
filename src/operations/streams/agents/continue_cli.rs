//! Continue CLI agent implementation with sweep discovery.

use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::WatermarkStrategy;
use crate::operations::streams::agent::{Agent, StreamDescriptor, discover_path_sessions};
use crate::operations::streams::reader::{
    JsonArrayStreamSpec, RecordIndexAdvancePolicy, read_json_array_stream,
};
use crate::operations::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::operations::streams::timestamp::file_time_fallback;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Continue CLI agent that reads Continue JSON transcript files.
///
/// Uses `RecordIndexWatermark` because the format has no timestamps at all.
/// We track how many history entries we've already processed and skip that
/// many on re-read.
pub struct ContinueAgent {
    batch_size: usize,
}

impl ContinueAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }

    fn session_roots() -> Vec<PathBuf> {
        dirs::home_dir()
            .into_iter()
            .map(|path| path.join(".continue/sessions"))
            .collect()
    }

    /// Scan for Continue session files in `~/.continue/sessions/**/*.json`.
    fn scan_session_files() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        if let Some(root) = Self::session_roots().into_iter().next() {
            let pattern = root.join("**/*.json").to_string_lossy().to_string();

            if let Ok(entries) = glob::glob(&pattern) {
                for entry in entries.flatten() {
                    if entry.is_file() {
                        paths.push(entry);
                    }
                }
            }
        }

        paths
    }
}

impl Default for ContinueAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for ContinueAgent {
    fn trusted_stream_roots(&self) -> Vec<PathBuf> {
        Self::session_roots()
    }

    fn validate_checkpoint_stream(
        &self,
        source: &crate::model::checkpoint_request::StreamSource,
    ) -> Result<DiscoveredSession, StreamError> {
        crate::operations::streams::agent::validate_checkpoint_stream_file(
            source,
            "continue-cli",
            crate::model::checkpoint_request::StreamFormat::ContinueJson,
            Self::session_roots(),
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
        Ok(discover_path_sessions(
            "continue-cli",
            Self::scan_session_files(),
            |path| Some((path.file_stem()?.to_str()?.to_string(), None)),
        ))
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
                "Continue",
                "history",
                self.batch_size_hint(),
                RecordIndexAdvancePolicy::OriginalU64,
            ),
            |path| StreamError::Fatal {
                message: format!(
                    "Missing 'history' array in Continue transcript: {}",
                    path.display()
                ),
            },
            || false,
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
            StreamFormat::ContinueJson,
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::stream_watermark::RecordIndexWatermark;

    #[test]
    fn test_sweep_strategy() {
        let agent = ContinueAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    fn make_continue_json(count: usize) -> String {
        let items: Vec<String> = (0..count)
            .map(|i| {
                format!(
                    r#"{{"id":{},"message":{{"role":"user","content":"msg-{}"}}}}"#,
                    i, i
                )
            })
            .collect();
        format!(r#"{{"history":[{}]}}"#, items.join(","))
    }

    fn drain_all(
        agent: &ContinueAgent,
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
        std::io::Write::write_all(&mut file, make_continue_json(5).as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();

        let agent = ContinueAgent::with_batch_size(2);
        let (events, _) = drain_all(&agent, file.path());

        assert_eq!(events.len(), 5);
        let ids: Vec<u64> = events.iter().map(|e| e["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_append_one_record_after_full_read() {
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, make_continue_json(3).as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();

        let agent = ContinueAgent::with_batch_size(2);
        let (all, wm) = drain_all(&agent, file.path());
        assert_eq!(all.len(), 3);

        std::fs::write(file.path(), make_continue_json(4)).unwrap();

        let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["id"].as_u64().unwrap(), 3);
    }

    #[test]
    fn test_append_several_records_after_full_read() {
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, make_continue_json(3).as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();

        let agent = ContinueAgent::with_batch_size(2);
        let (_, mut wm) = drain_all(&agent, file.path());

        std::fs::write(file.path(), make_continue_json(6)).unwrap();

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
            "history": [
                {"message": {"role": "user", "content": "Hello"}},
                {"message": {"role": "assistant", "content": "Hi there"}}
            ]
        });

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", json).unwrap();
        file.flush().unwrap();

        let agent = ContinueAgent::new();
        let watermark = Box::new(RecordIndexWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        assert_eq!(result.events.len(), 2);
        // Raw history items are returned
        assert_eq!(result.events[0]["message"]["role"], "user");
        assert_eq!(result.events[1]["message"]["role"], "assistant");
    }

    #[test]
    fn test_read_incremental_skips_already_processed() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let json = serde_json::json!({
            "history": [
                {"message": {"role": "user", "content": "Old"}},
                {"message": {"role": "assistant", "content": "Old reply"}},
                {"message": {"role": "user", "content": "New"}}
            ]
        });

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", json).unwrap();
        file.flush().unwrap();

        let agent = ContinueAgent::new();
        let watermark = Box::new(RecordIndexWatermark::new(2)); // Already processed 2
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        assert_eq!(result.events.len(), 1); // Only the new message
        assert_eq!(result.events[0]["message"]["content"], "New");
    }

    #[test]
    fn test_read_incremental_with_context_items() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let json = serde_json::json!({
            "history": [
                {
                    "message": {"role": "assistant", "content": "Let me check"},
                    "contextItems": [
                        {"name": "file_reader", "content": "some data"}
                    ]
                }
            ]
        });

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", json).unwrap();
        file.flush().unwrap();

        let agent = ContinueAgent::new();
        let watermark = Box::new(RecordIndexWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        // One raw history item containing both message and contextItems
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0]["message"]["content"], "Let me check");
        assert!(result.events[0]["contextItems"].is_array());
    }
}
