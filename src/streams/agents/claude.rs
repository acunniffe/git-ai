//! Claude Code agent implementation with sweep discovery.

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

/// Claude Code agent that discovers conversations from Claude Code storage.
pub struct ClaudeAgent {
    batch_size: usize,
}

impl ClaudeAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }

    /// Scan for Claude conversation files in standard locations.
    fn scan_conversation_files() -> Vec<PathBuf> {
        let config = Config::fresh();
        let mut roots = agent_profile_roots(AgentProfile::Claude, &config);
        if let Some(config_root) = dirs::config_dir().map(|path| path.join("claude"))
            && !roots.contains(&config_root)
        {
            roots.push(config_root);
        }
        Self::scan_conversation_files_in_roots(&roots)
    }

    fn scan_conversation_files_in_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for root in roots {
            let dir = root.join("projects");
            if dir.exists() {
                // Recursively scan for *.jsonl files
                Self::scan_jsonl_recursive(&dir, &mut paths);
            }
        }

        paths
    }

    /// Recursively scan directory for *.jsonl files.
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

    /// Extract session ID from a Claude conversation file path.
    ///
    /// Detect if a path is a subagent transcript and extract the parent session UUID.
    ///
    /// Subagent path pattern: `<project>/<parent-uuid>/subagents/agent-<id>.jsonl`
    pub fn detect_subagent_parent(path: &Path) -> Option<String> {
        let components: Vec<_> = path.components().collect();
        for (i, component) in components.iter().enumerate() {
            if let std::path::Component::Normal(s) = component
                && s.to_str() == Some("subagents")
                && i > 0
                && let std::path::Component::Normal(parent) = components[i - 1]
            {
                return parent.to_str().map(|s| s.to_string());
            }
        }
        None
    }
}

impl Default for ClaudeAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for ClaudeAgent {
    fn batch_size_hint(&self) -> usize {
        self.batch_size
    }

    fn sweep_strategy(&self) -> SweepStrategy {
        // Poll every 30 minutes for new Claude conversations
        SweepStrategy::Periodic(Duration::from_secs(30 * 60))
    }

    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError> {
        let paths = Self::scan_conversation_files();
        let mut sessions = Vec::new();

        for path in paths {
            let Some(external_session_id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
            else {
                continue;
            };
            let session_id = generate_session_id(&external_session_id, "claude");
            let external_parent_session_id = Self::detect_subagent_parent(&path);

            let session = DiscoveredSession {
                session_id,
                tool: "claude".to_string(),
                stream_path: path,
                external_session_id,
                external_parent_session_id,
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
                    "Claude reader requires ByteOffsetWatermark, got incompatible type for session {}",
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

    fn extract_event_ids(
        &self,
        event: &serde_json::Value,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let event_id = event.get("uuid").and_then(|v| v.as_str()).map(String::from);
        let parent_id = event
            .get("parentUuid")
            .and_then(|v| v.as_str())
            .map(String::from);

        let tool_use_id = event
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find_map(|block| match block.get("type").and_then(|t| t.as_str()) {
                        Some("tool_use") => {
                            block.get("id").and_then(|v| v.as_str()).map(String::from)
                        }
                        Some("tool_result") => block
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        _ => None,
                    })
            });

        (event_id, parent_id, tool_use_id)
    }

    fn extract_event_timestamp(
        &self,
        event: &serde_json::Value,
        file_meta: &std::fs::Metadata,
        is_first_event: bool,
    ) -> u32 {
        crate::daemon::stream_worker::extract_event_timestamp(event)
            .unwrap_or_else(|| crate::streams::agent::file_time_fallback(file_meta, is_first_event))
    }

    fn infer_cwd(&self, stream_path: &Path) -> Option<PathBuf> {
        use std::fs::File;
        use std::io::{BufRead, BufReader};

        let file = File::open(stream_path).ok()?;
        let reader = BufReader::new(file);

        // Check up to 50 lines for a top-level "cwd" field
        for line in reader.lines().take(50) {
            let Ok(line) = line else { continue };
            if line.is_empty() {
                continue;
            }
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line)
                && let Some(cwd) = obj.get("cwd").and_then(|v| v.as_str())
                && !cwd.is_empty()
            {
                return Some(PathBuf::from(cwd));
            }
        }
        None
    }

    fn streams(&self) -> Vec<StreamDescriptor> {
        let format = StreamFormat::ClaudeJsonl;
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
        let profile = temp.path().join(".claude-work");
        let transcript = profile.join("projects/repo/session.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, "{}\n").unwrap();

        assert_eq!(
            ClaudeAgent::scan_conversation_files_in_roots(&[profile]),
            vec![transcript]
        );
    }

    #[test]
    fn test_detect_subagent_parent() {
        let subagent_path = PathBuf::from(
            "/home/user/.claude/projects/-home-user-myproject/cf28d639-11e1-4851-b914-d16eb53d907b/subagents/agent-a20c8d201882f84b6.jsonl",
        );
        assert_eq!(
            ClaudeAgent::detect_subagent_parent(&subagent_path),
            Some("cf28d639-11e1-4851-b914-d16eb53d907b".to_string())
        );

        let main_session_path = PathBuf::from(
            "/home/user/.claude/projects/-home-user-myproject/cf28d639-11e1-4851-b914-d16eb53d907b.jsonl",
        );
        assert_eq!(
            ClaudeAgent::detect_subagent_parent(&main_session_path),
            None
        );

        let no_parent_path = PathBuf::from("/subagents/agent-xyz.jsonl");
        assert_eq!(ClaudeAgent::detect_subagent_parent(&no_parent_path), None);
    }

    #[test]
    fn test_sweep_strategy() {
        let agent = ClaudeAgent::new();
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
            r#"{{"type":"user","message":{{"content":"Hello"}},"timestamp":"2025-01-01T00:00:00Z"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Hi there"}}],"model":"claude-sonnet-4"}},"timestamp":"2025-01-01T00:00:01Z"}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = ClaudeAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test-session")
            .unwrap();

        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0]["type"].as_str(), Some("user"));
        assert_eq!(
            result.events[1]["message"]["model"].as_str(),
            Some("claude-sonnet-4")
        );
    }

    #[test]
    fn test_scan_discovers_real_claude_files() {
        let paths = ClaudeAgent::scan_conversation_files();
        // On this machine we have files in ~/.claude/projects/
        if dirs::home_dir()
            .map(|h| h.join(".claude/projects").exists())
            .unwrap_or(false)
        {
            assert!(
                !paths.is_empty(),
                "Should discover files in ~/.claude/projects/"
            );
            for path in &paths {
                assert!(path.extension().and_then(|s| s.to_str()) == Some("jsonl"));
            }
        }
    }

    #[test]
    fn test_read_incremental_with_token_usage() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Response"}}],"model":"claude-sonnet-4","usage":{{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":200,"cache_creation_input_tokens":300}}}},"timestamp":"2025-01-01T00:00:01Z"}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = ClaudeAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test-session")
            .unwrap();

        assert_eq!(result.events.len(), 1);
        let event = &result.events[0];
        let usage = &event["message"]["usage"];
        assert_eq!(usage["input_tokens"].as_u64(), Some(100));
        assert_eq!(usage["output_tokens"].as_u64(), Some(50));
        assert_eq!(usage["cache_read_input_tokens"].as_u64(), Some(200));
        assert_eq!(usage["cache_creation_input_tokens"].as_u64(), Some(300));
    }

    fn make_jsonl_line(i: usize) -> String {
        format!(
            r#"{{"type":"user","id":{},"message":{{"content":"msg-{}"}}}}"#,
            i, i
        )
    }

    fn drain_all(agent: &ClaudeAgent, path: &Path) -> Vec<serde_json::Value> {
        let mut all = Vec::new();
        let mut wm: Box<dyn WatermarkStrategy> = Box::new(ByteOffsetWatermark::new(0));
        loop {
            let batch = agent.read_incremental(path, wm, "test").unwrap();
            if batch.events.is_empty() {
                break;
            }
            all.extend(batch.events);
            wm = batch.new_watermark;
        }
        all
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

        let agent = ClaudeAgent::with_batch_size(2);
        let events = drain_all(&agent, file.path());

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

        let agent = ClaudeAgent::with_batch_size(2);
        let mut wm: Box<dyn WatermarkStrategy> = Box::new(ByteOffsetWatermark::new(0));
        let mut all = Vec::new();
        loop {
            let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
            if batch.events.is_empty() {
                wm = batch.new_watermark;
                break;
            }
            all.extend(batch.events);
            wm = batch.new_watermark;
        }
        assert_eq!(all.len(), 3);

        // Append one record
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

        let agent = ClaudeAgent::with_batch_size(2);
        let mut wm: Box<dyn WatermarkStrategy> = Box::new(ByteOffsetWatermark::new(0));
        loop {
            let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
            wm = batch.new_watermark;
            if batch.events.is_empty() {
                break;
            }
        }

        // Append 3 more records
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
    fn test_extract_event_ids_assistant_with_tool_use() {
        let agent = ClaudeAgent::new();
        let event = serde_json::json!({
            "type": "assistant",
            "uuid": "e55c8481-4ee9-429d-a11a-2cbf9a87b688",
            "parentUuid": "d75bc9bf-0326-433e-9f4f-1e5fc8c415d0",
            "message": {
                "content": [
                    {"type": "tool_use", "id": "toolu_013JnBoRSqxCShSX", "name": "Edit", "input": {}}
                ]
            }
        });
        let (eid, pid, tid) = agent.extract_event_ids(&event);
        assert_eq!(
            eid,
            Some("e55c8481-4ee9-429d-a11a-2cbf9a87b688".to_string())
        );
        assert_eq!(
            pid,
            Some("d75bc9bf-0326-433e-9f4f-1e5fc8c415d0".to_string())
        );
        assert_eq!(tid, Some("toolu_013JnBoRSqxCShSX".to_string()));
    }

    #[test]
    fn test_extract_event_ids_user_with_tool_result() {
        let agent = ClaudeAgent::new();
        let event = serde_json::json!({
            "type": "user",
            "uuid": "abc-123",
            "parentUuid": "def-456",
            "message": {
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_xyz", "content": "ok"}
                ]
            }
        });
        let (eid, pid, tid) = agent.extract_event_ids(&event);
        assert_eq!(eid, Some("abc-123".to_string()));
        assert_eq!(pid, Some("def-456".to_string()));
        assert_eq!(tid, Some("toolu_xyz".to_string()));
    }

    #[test]
    fn test_extract_event_ids_text_only() {
        let agent = ClaudeAgent::new();
        let event = serde_json::json!({
            "type": "assistant",
            "uuid": "msg-1",
            "parentUuid": null,
            "message": {
                "content": [
                    {"type": "text", "text": "Hello"}
                ]
            }
        });
        let (eid, pid, tid) = agent.extract_event_ids(&event);
        assert_eq!(eid, Some("msg-1".to_string()));
        assert_eq!(pid, None);
        assert_eq!(tid, None);
    }

    #[test]
    fn test_extract_event_ids_summary_event() {
        let agent = ClaudeAgent::new();
        let event = serde_json::json!({
            "type": "summary",
            "summary": "Did something",
            "leafUuid": "leaf-1"
        });
        let (eid, pid, tid) = agent.extract_event_ids(&event);
        assert_eq!(eid, None);
        assert_eq!(pid, None);
        assert_eq!(tid, None);
    }
}
