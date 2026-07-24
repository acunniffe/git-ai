//! Codex agent implementation with sweep discovery.

use crate::model::authorship_log_serialization::generate_session_id;
use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::WatermarkStrategy;
use crate::operations::mdm::paths::codex_home_dir;
use crate::operations::streams::agent::{
    Agent, PathResolverKind, StreamDescriptor, read_jsonl_byte_stream,
};
use crate::operations::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Codex agent that reads Codex JSONL transcript files.
pub struct CodexAgent {
    batch_size: usize,
}

impl CodexAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }

    /// Search for a rollout file matching the given session ID in the Codex home directory.
    ///
    /// Looks in both `sessions` and `archived_sessions` subdirectories for files
    /// matching `rollout-*{session_id}*.jsonl`. Returns the newest match by
    /// modification time.
    pub fn find_rollout_path_for_session_in_home(
        session_id: &str,
        codex_home: &Path,
    ) -> Result<Option<PathBuf>, StreamError> {
        let mut candidates: Vec<PathBuf> = Vec::new();

        for subdir in &["sessions", "archived_sessions"] {
            let search_dir = codex_home.join(subdir);
            if !search_dir.exists() {
                continue;
            }

            let pattern = format!("{}/**/rollout-*{}*.jsonl", search_dir.display(), session_id);

            let entries = glob::glob(&pattern).map_err(|e| StreamError::Fatal {
                message: format!("Invalid glob pattern for Codex session search: {}", e),
            })?;

            for entry in entries {
                let path = entry.map_err(|e| StreamError::Fatal {
                    message: format!("Error reading glob entry: {}", e),
                })?;
                candidates.push(path);
            }
        }

        if candidates.is_empty() {
            return Ok(None);
        }

        // Return the newest by modification time
        let newest = candidates
            .into_iter()
            .filter_map(|p| {
                p.metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| (p, t))
            })
            .max_by_key(|(_, t)| *t)
            .map(|(p, _)| p);

        Ok(newest)
    }

    fn scan_session_files() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        let codex_home = codex_home_dir();

        for subdir in &["sessions", "archived_sessions"] {
            let search_dir = codex_home.join(subdir);
            if search_dir.exists() {
                Self::scan_rollout_recursive(&search_dir, &mut paths);
            }
        }

        paths
    }

    fn scan_rollout_recursive(dir: &Path, paths: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::scan_rollout_recursive(&path, paths);
            } else if path.is_file()
                && path.extension().map(|ext| ext == "jsonl").unwrap_or(false)
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("rollout-"))
                    .unwrap_or(false)
            {
                paths.push(path);
            }
        }
    }

    /// Detect if a Codex JSONL file is a subagent transcript by reading its first line.
    ///
    /// Subagent transcripts have a `session_meta` first event with
    /// `payload.thread_source == "subagent"` and `payload.forked_from_id` containing
    /// the parent session UUID.
    pub fn detect_subagent_parent(path: &Path) -> Option<String> {
        let file = File::open(path).ok()?;
        let reader = BufReader::new(file);
        let first_line = reader.lines().next()?.ok()?;
        if first_line.trim().is_empty() {
            return None;
        }

        let obj: serde_json::Value = serde_json::from_str(&first_line).ok()?;

        if obj.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
            return None;
        }

        let payload = obj.get("payload")?;

        if payload.get("thread_source").and_then(|t| t.as_str()) != Some("subagent") {
            return None;
        }

        payload
            .get("forked_from_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

impl Default for CodexAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for CodexAgent {
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
            // Codex filename: rollout-2026-02-06T20-35-49-019c35bd-ad8e-7422-834c-3605bc4ee7ac
            // The hook payload sends the UUID as session_id/thread_id (last 36 chars)
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.len() < 36 {
                continue;
            }
            let external_session_id = stem[stem.len() - 36..].to_string();
            let session_id = generate_session_id(&external_session_id, "codex");
            let external_parent_session_id = Self::detect_subagent_parent(&path);

            sessions.push(DiscoveredSession {
                session_id,
                tool: "codex".to_string(),
                stream_path: path,
                external_session_id,
                external_parent_session_id,
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
            "Codex",
            "open",
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

    fn infer_cwd(&self, stream_path: &Path) -> Option<PathBuf> {
        use std::fs::File;
        use std::io::{BufRead, BufReader};

        let file = File::open(stream_path).ok()?;
        let reader = BufReader::new(file);

        // Codex has cwd in session_meta or turn_context payload events
        for line in reader.lines().take(20) {
            let Ok(line) = line else { continue };
            if line.is_empty() {
                continue;
            }
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) {
                // Check payload.cwd (session_meta and turn_context)
                if let Some(cwd) = obj
                    .get("payload")
                    .and_then(|p| p.get("cwd"))
                    .and_then(|v| v.as_str())
                    && !cwd.is_empty()
                {
                    return Some(PathBuf::from(cwd));
                }
            }
        }
        None
    }

    fn streams(&self) -> Vec<StreamDescriptor> {
        let format = StreamFormat::CodexJsonl;
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
        let agent = CodexAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    fn make_jsonl_line(i: usize) -> String {
        format!(
            r#"{{"type":"response_item","id":{},"payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"msg-{}"}}]}}}}"#,
            i, i
        )
    }

    fn drain_all(
        agent: &CodexAgent,
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

        let agent = CodexAgent::with_batch_size(2);
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

        let agent = CodexAgent::with_batch_size(2);
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

        let agent = CodexAgent::with_batch_size(2);
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
            r#"{{"type":"turn_context","payload":{{"model":"gpt-4o"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"Hello"}}]}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = CodexAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        // Both JSONL lines are returned as raw JSON
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0]["type"], "turn_context");
        assert_eq!(result.events[0]["payload"]["model"], "gpt-4o");
        assert_eq!(result.events[1]["type"], "response_item");
        assert_eq!(result.events[1]["payload"]["role"], "assistant");
    }

    #[test]
    fn test_read_incremental_legacy_format() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"user_message","message":"Hello"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"agent_message","message":"Hi there"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = CodexAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        // Both JSONL lines are returned as raw JSON
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0]["type"], "event_msg");
        assert_eq!(result.events[0]["payload"]["type"], "user_message");
        assert_eq!(result.events[1]["payload"]["type"], "agent_message");
    }

    #[test]
    fn test_detect_subagent_parent_found() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"timestamp":"2026-05-12T20:40:23.849Z","type":"session_meta","payload":{{"id":"019e1deb-59ab-7ec3-8731-5825d1566f6d","forked_from_id":"019e1dea-c02a-71b3-b87f-67812459e1d9","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"019e1dea-c02a-71b3-b87f-67812459e1d9","depth":1,"agent_nickname":"Carson"}}}}}},"thread_source":"subagent","agent_nickname":"Carson"}}}}"#).unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"user_message","message":"do something"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let result = CodexAgent::detect_subagent_parent(file.path());
        assert_eq!(
            result,
            Some("019e1dea-c02a-71b3-b87f-67812459e1d9".to_string())
        );
    }

    #[test]
    fn test_detect_subagent_parent_user_session() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"timestamp":"2026-05-12T20:38:00.000Z","type":"session_meta","payload":{{"id":"019e1dea-c02a-71b3-b87f-67812459e1d9","thread_source":"user"}}}}"#).unwrap();
        file.flush().unwrap();

        let result = CodexAgent::detect_subagent_parent(file.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_subagent_parent_no_session_meta() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"user_message","message":"hello"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let result = CodexAgent::detect_subagent_parent(file.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_subagent_parent_empty_file() {
        use tempfile::NamedTempFile;

        let file = NamedTempFile::new().unwrap();

        let result = CodexAgent::detect_subagent_parent(file.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_subagent_parent_nonexistent_file() {
        let result =
            CodexAgent::detect_subagent_parent(Path::new("/nonexistent/path/rollout.jsonl"));
        assert_eq!(result, None);
    }
}
