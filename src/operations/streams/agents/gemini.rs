//! Gemini agent implementation with sweep discovery.

use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::WatermarkStrategy;
use crate::operations::mdm::paths::gemini_config_dir;
use crate::operations::streams::agent::{Agent, StreamDescriptor, discover_path_sessions};
use crate::operations::streams::reader::read_jsonl_byte_stream;
use crate::operations::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::operations::streams::timestamp::event_timestamp_or_file_time;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Gemini agent that discovers conversations from Gemini CLI session storage.
///
/// Gemini CLI stores JSONL chat transcripts under `.gemini/tmp/<project>/chats/`
/// within the configured Gemini CLI home.
pub struct GeminiAgent {
    batch_size: usize,
}

impl GeminiAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }

    fn session_roots() -> Vec<PathBuf> {
        vec![gemini_config_dir().join("tmp")]
    }

    /// Scan for Gemini session files in standard locations.
    ///
    /// Searches `.gemini/tmp/*/chats/session-*.jsonl` under the configured Gemini CLI home.
    fn scan_session_files() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        let gemini_tmp = Self::session_roots().remove(0);
        if gemini_tmp.exists() {
            let Ok(project_dirs) = fs::read_dir(&gemini_tmp) else {
                return paths;
            };
            for project_entry in project_dirs.flatten() {
                let chats_dir = project_entry.path().join("chats");
                if !chats_dir.is_dir() {
                    continue;
                }
                let Ok(chat_files) = fs::read_dir(&chats_dir) else {
                    continue;
                };
                for file_entry in chat_files.flatten() {
                    let path = file_entry.path();
                    if path.is_file()
                        && path.extension().map(|ext| ext == "jsonl").unwrap_or(false)
                        && path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with("session-"))
                            .unwrap_or(false)
                    {
                        paths.push(path);
                    }
                }
            }
        }

        paths
    }
}

impl Default for GeminiAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for GeminiAgent {
    fn trusted_stream_roots(&self) -> Vec<PathBuf> {
        Self::session_roots()
    }

    fn validate_checkpoint_stream(
        &self,
        source: &crate::model::checkpoint_request::StreamSource,
    ) -> Result<DiscoveredSession, StreamError> {
        crate::operations::streams::agent::validate_checkpoint_stream_file(
            source,
            "gemini",
            crate::model::checkpoint_request::StreamFormat::GeminiJsonl,
            Self::session_roots(),
            |path| {
                if !crate::operations::streams::agent::checkpoint_stream_has_extension(
                    path, "jsonl",
                ) || path.parent()?.file_name()?.to_str()? != "chats"
                {
                    return None;
                }
                let name = path.file_name()?.to_str()?;
                if !name.starts_with("session-") {
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
            "gemini",
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
        read_jsonl_byte_stream(
            path,
            watermark,
            session_id,
            self.batch_size_hint(),
            "Gemini",
            "read",
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
            StreamFormat::GeminiJsonl,
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
    use serial_test::serial;

    #[test]
    fn test_sweep_strategy() {
        let agent = GeminiAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    #[test]
    #[serial]
    fn test_scan_session_files_respects_gemini_cli_home() {
        use std::io::Write;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let gemini_home = temp_dir.path().join("gemini-home");
        let chats_dir = gemini_home
            .join(".gemini")
            .join("tmp")
            .join("project")
            .join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        let session_path = chats_dir.join("session-test.jsonl");
        let mut file = fs::File::create(&session_path).unwrap();
        writeln!(file, "{}", make_gemini_line(0)).unwrap();

        let prev = std::env::var_os("GEMINI_CLI_HOME");
        // SAFETY: tests are serialized via #[serial], so mutating process env is safe.
        unsafe {
            std::env::set_var("GEMINI_CLI_HOME", &gemini_home);
        }
        let paths = GeminiAgent::scan_session_files();
        // SAFETY: tests are serialized via #[serial], so restoring process env is safe.
        unsafe {
            match prev {
                Some(value) => std::env::set_var("GEMINI_CLI_HOME", value),
                None => std::env::remove_var("GEMINI_CLI_HOME"),
            }
        }

        assert_eq!(paths, vec![session_path]);
    }

    fn make_gemini_line(i: usize) -> String {
        format!(
            r#"{{"id":"msg-{}","timestamp":"2026-05-03T02:{:02}:00.000Z","type":"gemini","content":"msg-{}","model":"gemini-3-flash-preview"}}"#,
            i, i, i
        )
    }

    #[test]
    fn test_stream_adapter_contract() {
        use tempfile::NamedTempFile;

        let file = NamedTempFile::new().unwrap();
        let mut fixture = jsonl_fixture(file.path(), make_gemini_line);
        let agent = GeminiAgent::with_batch_size(2);
        assert_stream_adapter_contract(
            &agent,
            &mut fixture,
            || Box::new(ByteOffsetWatermark::new(0)),
            |event| event["id"].as_str().unwrap().to_string(),
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
        writeln!(file, r#"{{"id":"msg-1","timestamp":"2026-05-03T02:36:28.771Z","type":"user","content":[{{"text":"Hello"}}]}}"#).unwrap();
        writeln!(file, r#"{{"id":"msg-2","timestamp":"2026-05-03T02:36:32.428Z","type":"gemini","content":"Hi there","model":"gemini-3-flash-preview"}}"#).unwrap();
        file.flush().unwrap();

        let agent = GeminiAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0]["type"], "user");
        assert_eq!(result.events[1]["type"], "gemini");
        assert_eq!(result.events[1]["model"], "gemini-3-flash-preview");
    }

    #[test]
    fn test_read_incremental_skips_empty_lines() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"user","content":[{{"text":"Hello"}}]}}"#).unwrap();
        writeln!(file).unwrap();
        writeln!(
            file,
            r#"{{"type":"gemini","content":"Hi","model":"gemini-3-flash-preview"}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = GeminiAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        assert_eq!(result.events.len(), 2);
    }

    #[test]
    fn test_read_incremental_resumes_from_offset() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        let line1 = r#"{"type":"user","content":[{"text":"First"}]}"#;
        let line2 = r#"{"type":"gemini","content":"Second","model":"gemini-3-flash-preview"}"#;
        writeln!(file, "{}", line1).unwrap();
        writeln!(file, "{}", line2).unwrap();
        file.flush().unwrap();

        let agent = GeminiAgent::new();

        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();
        assert_eq!(result.events.len(), 2);

        let result2 = agent
            .read_incremental(file.path(), result.new_watermark, "test")
            .unwrap();
        assert_eq!(result2.events.len(), 0);
    }
}
