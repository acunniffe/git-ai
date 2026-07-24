//! Gemini agent implementation with sweep discovery.

use crate::model::authorship_log_serialization::generate_session_id;
use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::WatermarkStrategy;
use crate::operations::mdm::paths::gemini_config_dir;
use crate::operations::streams::agent::{
    Agent, PathResolverKind, StreamDescriptor, read_jsonl_byte_stream,
};
use crate::operations::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
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

    /// Scan for Gemini session files in standard locations.
    ///
    /// Searches `.gemini/tmp/*/chats/session-*.jsonl` under the configured Gemini CLI home.
    fn scan_session_files() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        let gemini_tmp = gemini_config_dir().join("tmp");
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
    fn batch_size_hint(&self) -> usize {
        self.batch_size
    }

    fn sweep_strategy(&self) -> SweepStrategy {
        SweepStrategy::Periodic(Duration::from_secs(30 * 60))
    }

    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError> {
        let paths = Self::scan_session_files();
        let mut sessions = Vec::new();

        for path in paths {
            // Gemini session_id from the hook payload matches the file stem
            let Some(external_session_id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
            else {
                continue;
            };
            let session_id = generate_session_id(&external_session_id, "gemini");

            let session = DiscoveredSession {
                session_id,
                tool: "gemini".to_string(),
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
        crate::operations::daemon::stream_worker::extract_event_timestamp(event).unwrap_or_else(
            || crate::operations::streams::agent::file_time_fallback(file_meta, is_first_event),
        )
    }

    fn streams(&self) -> Vec<StreamDescriptor> {
        let format = StreamFormat::GeminiJsonl;
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

    fn drain_all(
        agent: &GeminiAgent,
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
            writeln!(file, "{}", make_gemini_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = GeminiAgent::with_batch_size(2);
        let (events, _) = drain_all(&agent, file.path());

        assert_eq!(events.len(), 5);
        let ids: Vec<&str> = events.iter().map(|e| e["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["msg-0", "msg-1", "msg-2", "msg-3", "msg-4"]);
    }

    #[test]
    fn test_append_one_record_after_full_read() {
        use std::fs::OpenOptions;
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        for i in 0..3 {
            writeln!(file, "{}", make_gemini_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = GeminiAgent::with_batch_size(2);
        let (all, wm) = drain_all(&agent, file.path());
        assert_eq!(all.len(), 3);

        let mut f = OpenOptions::new().append(true).open(file.path()).unwrap();
        writeln!(f, "{}", make_gemini_line(3)).unwrap();
        f.flush().unwrap();

        let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["id"].as_str().unwrap(), "msg-3");
    }

    #[test]
    fn test_append_several_records_after_full_read() {
        use std::fs::OpenOptions;
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        for i in 0..3 {
            writeln!(file, "{}", make_gemini_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = GeminiAgent::with_batch_size(2);
        let (_, mut wm) = drain_all(&agent, file.path());

        let mut f = OpenOptions::new().append(true).open(file.path()).unwrap();
        for i in 3..6 {
            writeln!(f, "{}", make_gemini_line(i)).unwrap();
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
        let ids: Vec<&str> = new_events
            .iter()
            .map(|e| e["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["msg-3", "msg-4", "msg-5"]);
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
