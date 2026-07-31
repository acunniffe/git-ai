//! Claude Code agent implementation with sweep discovery.

use super::discovery::{collect_files_recursively, transcript_file_stem};
use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::WatermarkStrategy;
use crate::operations::streams::agent::{Agent, StreamDescriptor, discover_path_sessions};
use crate::operations::streams::reader::read_jsonl_byte_stream;
use crate::operations::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::operations::streams::timestamp::event_timestamp_or_file_time;
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

    fn conversation_roots() -> Vec<PathBuf> {
        // Check CLAUDE_CONFIG_DIR override first
        let base_dir = if let Ok(config_dir) = std::env::var("CLAUDE_CONFIG_DIR") {
            Some(PathBuf::from(config_dir))
        } else {
            dirs::home_dir().map(|p| p.join(".claude"))
        };

        [
            base_dir.as_ref().map(|p| p.join("projects")),
            dirs::config_dir().map(|p| p.join("claude/projects")),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Scan for Claude conversation files in standard locations.
    fn scan_conversation_files() -> Vec<PathBuf> {
        collect_files_recursively(Self::conversation_roots(), |path| {
            path.extension()
                .map(|extension| extension == "jsonl")
                .unwrap_or(false)
        })
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
    fn trusted_stream_roots(&self) -> Vec<PathBuf> {
        Self::conversation_roots()
    }

    fn validate_checkpoint_stream(
        &self,
        source: &crate::model::checkpoint_request::StreamSource,
    ) -> Result<DiscoveredSession, StreamError> {
        crate::operations::streams::agent::validate_checkpoint_stream_file(
            source,
            "claude",
            crate::model::checkpoint_request::StreamFormat::ClaudeJsonl,
            Self::conversation_roots(),
            |path| {
                if !crate::operations::streams::agent::checkpoint_stream_has_extension(
                    path, "jsonl",
                ) {
                    return None;
                }
                let external_id = transcript_file_stem(path)?;
                Some((external_id, Self::detect_subagent_parent(path)))
            },
        )
    }

    fn batch_size_hint(&self) -> usize {
        self.batch_size
    }

    fn sweep_strategy(&self) -> SweepStrategy {
        // Poll every 30 minutes for new Claude conversations
        SweepStrategy::Periodic(Duration::from_secs(30 * 60))
    }

    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError> {
        Ok(discover_path_sessions(
            "claude",
            Self::scan_conversation_files(),
            |path| {
                Some((
                    transcript_file_stem(path)?,
                    Self::detect_subagent_parent(path),
                ))
            },
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
            "Claude",
            "open",
        )
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
        event_timestamp_or_file_time(event, file_meta, is_first_event)
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
        vec![StreamDescriptor::identity_transcript(
            StreamFormat::ClaudeJsonl,
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

    #[test]
    fn test_stream_adapter_contract() {
        use tempfile::NamedTempFile;

        let file = NamedTempFile::new().unwrap();
        let mut fixture = jsonl_fixture(file.path(), make_jsonl_line);
        let agent = ClaudeAgent::with_batch_size(2);
        assert_stream_adapter_contract(
            &agent,
            &mut fixture,
            || Box::new(ByteOffsetWatermark::new(0)),
            |event| event["id"].as_u64().unwrap().to_string(),
            2,
            "test",
            StreamAdapterContractCapabilities::APPEND_ALL,
        );
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
