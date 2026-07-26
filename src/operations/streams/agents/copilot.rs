//! GitHub Copilot agent implementation with sweep discovery.

use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::{WatermarkStrategy, WatermarkType};
use crate::operations::streams::agent::{
    Agent, PathResolverKind, StreamDescriptor, discover_path_sessions,
};
use crate::operations::streams::reader::{
    JsonArrayStreamSpec, RecordIndexAdvancePolicy, read_json_array_stream, read_jsonl_byte_stream,
};
use crate::operations::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::operations::streams::timestamp::event_timestamp_or_file_time;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// GitHub Copilot agent that discovers conversations from Copilot storage.
pub struct CopilotAgent {
    batch_size: usize,
}

impl CopilotAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }

    fn legacy_transcript_roots() -> Vec<PathBuf> {
        dirs::config_dir()
            .into_iter()
            .flat_map(|path| {
                [
                    path.join("github-copilot/sessions"),
                    path.join("github-copilot/events"),
                ]
            })
            .collect()
    }

    fn transcript_roots() -> Vec<PathBuf> {
        let mut roots = Self::legacy_transcript_roots();
        roots.extend(Self::vscode_workspace_storage_roots());
        roots
    }

    /// Scan for Copilot transcript files in standard locations.
    ///
    /// Discovers BOTH session.json files and .jsonl event streams.
    fn scan_transcript_files() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        // Standard locations for Copilot transcripts (legacy)
        for dir in Self::legacy_transcript_roots() {
            if dir.exists()
                && let Ok(entries) = fs::read_dir(&dir)
            {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        let ext = path.extension().and_then(|s| s.to_str());
                        if ext == Some("json") || ext == Some("jsonl") {
                            paths.push(path);
                        }
                    }
                }
            }
        }

        // VS Code workspace storage: event stream JSONL transcripts
        for workspace_storage_root in Self::vscode_workspace_storage_roots() {
            if !workspace_storage_root.exists() {
                continue;
            }
            let Ok(workspaces) = fs::read_dir(&workspace_storage_root) else {
                continue;
            };
            for entry in workspaces.flatten() {
                let transcripts_dir = entry.path().join("GitHub.copilot-chat").join("transcripts");
                if !transcripts_dir.is_dir() {
                    continue;
                }
                let Ok(files) = fs::read_dir(&transcripts_dir) else {
                    continue;
                };
                for file_entry in files.flatten() {
                    let path = file_entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                        paths.push(path);
                    }
                }
            }
        }

        paths
    }

    /// Returns the VS Code workspace storage root directories to scan.
    fn vscode_workspace_storage_roots() -> Vec<PathBuf> {
        let mut roots = Vec::new();
        let products = ["Code", "Code - Insiders"];

        #[cfg(target_os = "macos")]
        if let Some(home) = dirs::home_dir() {
            for product in &products {
                roots.push(
                    home.join("Library/Application Support")
                        .join(product)
                        .join("User/workspaceStorage"),
                );
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(config) = dirs::config_dir() {
            for product in &products {
                roots.push(config.join(product).join("User/workspaceStorage"));
            }
        }
        #[cfg(target_os = "windows")]
        {
            let appdata = std::env::var("APPDATA")
                .map(PathBuf::from)
                .ok()
                .or_else(dirs::config_dir);
            if let Some(appdata) = appdata {
                for product in &products {
                    roots.push(appdata.join(product).join("User/workspaceStorage"));
                }
            }
        }
        roots
    }

    /// Infer CWD from a VS Code workspace storage transcript path by reading
    /// the sibling `workspace.json` file.
    fn infer_cwd_from_workspace_json(stream_path: &Path) -> Option<PathBuf> {
        // transcript: .../workspaceStorage/{hash}/GitHub.copilot-chat/transcripts/{id}.jsonl
        // workspace.json: .../workspaceStorage/{hash}/workspace.json
        let workspace_hash_dir = stream_path.parent()?.parent()?.parent()?;
        let workspace_json = workspace_hash_dir.join("workspace.json");
        let content = fs::read_to_string(&workspace_json).ok()?;
        let json: serde_json::Value = serde_json::from_str(&content).ok()?;
        let folder_uri = json.get("folder")?.as_str()?;
        let path_str = folder_uri.strip_prefix("file://")?;
        let decoded = percent_decode_path(path_str);
        // On Windows, file URIs are file:///C:/... which leaves /C:/... after stripping.
        // Strip the leading slash if followed by a drive letter.
        let normalized =
            if decoded.len() >= 3 && decoded.as_bytes()[0] == b'/' && decoded.as_bytes()[2] == b':'
            {
                &decoded[1..]
            } else {
                &decoded
            };
        Some(PathBuf::from(normalized))
    }

    /// Determine transcript format from file path.
    #[cfg(test)]
    fn determine_format(path: &Path) -> StreamFormat {
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            StreamFormat::CopilotEventStreamJsonl
        } else {
            StreamFormat::CopilotSessionJson
        }
    }

    /// Resolve the path to the Copilot OTEL traces database.
    ///
    /// The DB lives in VS Code's globalStorage for the Copilot extension.
    /// Transcript path: .../workspaceStorage/{hash}/GitHub.copilot-chat/transcripts/{id}.jsonl
    /// OTEL DB: .../globalStorage/github.copilot-chat/agent-traces.db
    fn resolve_otel_db_path(stream_path: &Path) -> Option<PathBuf> {
        if let Ok(path) = std::env::var("GIT_AI_COPILOT_OTEL_DB_PATH") {
            let p = PathBuf::from(path);
            if p.exists() {
                return Some(p);
            }
        }

        // Navigate from transcript path to globalStorage
        // transcript: .../workspaceStorage/{hash}/GitHub.copilot-chat/transcripts/{id}.jsonl
        let workspace_storage_root = stream_path
            .parent()? // transcripts/
            .parent()? // GitHub.copilot-chat/
            .parent()? // {hash}/
            .parent()?; // workspaceStorage/

        let user_dir = workspace_storage_root.parent()?; // User/
        let otel_db = user_dir
            .join("globalStorage")
            .join("github.copilot-chat")
            .join("agent-traces.db");

        if otel_db.exists() {
            Some(otel_db)
        } else {
            None
        }
    }
}

/// Decode percent-encoded characters in a URI path (e.g., %20 -> space).
fn percent_decode_path(input: &str) -> String {
    let mut decoded_bytes = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&input[i + 1..i + 3], 16)
        {
            decoded_bytes.push(byte);
            i += 3;
            continue;
        }
        decoded_bytes.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(decoded_bytes).unwrap_or_else(|_| input.to_string())
}

impl Default for CopilotAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for CopilotAgent {
    fn trusted_stream_roots(&self) -> Vec<PathBuf> {
        Self::transcript_roots()
    }

    fn validate_checkpoint_stream(
        &self,
        source: &crate::model::checkpoint_request::StreamSource,
    ) -> Result<DiscoveredSession, StreamError> {
        let (expected_format, expected_extension) = match source
            .path
            .extension()
            .and_then(|extension| extension.to_str())
        {
            Some("json") => (
                crate::model::checkpoint_request::StreamFormat::CopilotSessionJson,
                "json",
            ),
            Some("jsonl") => (
                crate::model::checkpoint_request::StreamFormat::CopilotEventStreamJsonl,
                "jsonl",
            ),
            _ => {
                return Err(crate::operations::streams::agent::checkpoint_stream_denied());
            }
        };
        crate::operations::streams::agent::validate_checkpoint_stream_file(
            source,
            "github-copilot",
            expected_format,
            Self::transcript_roots(),
            |path| {
                if !crate::operations::streams::agent::checkpoint_stream_has_extension(
                    path,
                    expected_extension,
                ) {
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
        // Poll every 30 minutes for new Copilot transcripts
        SweepStrategy::Periodic(Duration::from_secs(30 * 60))
    }

    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError> {
        Ok(discover_path_sessions(
            "github-copilot",
            Self::scan_transcript_files(),
            |path| Some((path.file_stem()?.to_str()?.to_string(), None)),
        ))
    }

    fn streams(&self) -> Vec<StreamDescriptor> {
        vec![
            StreamDescriptor {
                stream_kind: "transcript",
                format: StreamFormat::CopilotEventStreamJsonl,
                watermark_type: WatermarkType::ByteOffset,
                path_resolver: PathResolverKind::Identity,
                shared: false,
                watermark_type_resolver: Some(Box::new(|path| {
                    if path.extension().and_then(|e| e.to_str()) == Some("json") {
                        WatermarkType::RecordIndex
                    } else {
                        WatermarkType::ByteOffset
                    }
                })),
                format_resolver: Some(Box::new(|path| {
                    if path.extension().and_then(|e| e.to_str()) == Some("json") {
                        StreamFormat::CopilotSessionJson
                    } else {
                        StreamFormat::CopilotEventStreamJsonl
                    }
                })),
            },
            StreamDescriptor {
                stream_kind: "otel_traces",
                format: StreamFormat::CopilotOtelSqlite,
                watermark_type: WatermarkType::TimestampCursor,
                path_resolver: PathResolverKind::Custom(Box::new(Self::resolve_otel_db_path)),
                shared: true,
                watermark_type_resolver: None,
                format_resolver: None,
            },
        ]
    }

    fn read_incremental(
        &self,
        path: &Path,
        watermark: Box<dyn WatermarkStrategy>,
        session_id: &str,
    ) -> Result<StreamBatch, StreamError> {
        // OTEL SQLite DB files
        if path.extension().and_then(|e| e.to_str()) == Some("db") {
            return super::copilot_otel::read_otel_spans_incremental(
                path,
                watermark,
                self.batch_size,
            );
        }
        // Existing transcript reading logic
        let batch_limit = self.batch_size_hint();
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            read_jsonl_byte_stream(
                path,
                watermark,
                session_id,
                batch_limit,
                "Copilot event stream",
                "open",
            )
        } else {
            read_session_json(path, watermark, session_id, batch_limit)
        }
    }

    fn extract_event_ids(
        &self,
        event: &serde_json::Value,
    ) -> (Option<String>, Option<String>, Option<String>) {
        // OTEL events have a "span" key at top level
        if event.get("span").is_some() {
            return super::copilot_otel::extract_otel_event_ids(event);
        }
        // Existing copilot transcript event ID extraction
        let id = event.get("id").and_then(|v| v.as_str()).map(String::from);
        let parent_id = event
            .get("parentId")
            .and_then(|v| v.as_str())
            .map(String::from);
        (id, parent_id, None)
    }

    fn extract_event_session_id(&self, event: &serde_json::Value) -> Option<String> {
        let span = event.get("span")?;
        span.get("chat_session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                span.get("conversation_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
            })
            .map(String::from)
    }

    fn extract_event_timestamp(
        &self,
        event: &serde_json::Value,
        file_meta: &std::fs::Metadata,
        is_first_event: bool,
    ) -> u32 {
        // OTEL events have their own timestamp extraction
        if event.get("span").is_some()
            && let Some(ts) = super::copilot_otel::extract_otel_event_timestamp(event)
        {
            return ts;
        }
        event_timestamp_or_file_time(event, file_meta, is_first_event)
    }

    fn infer_cwd(&self, stream_path: &Path) -> Option<PathBuf> {
        // Try workspace.json first (VS Code workspace storage layout)
        if let Some(cwd) = Self::infer_cwd_from_workspace_json(stream_path) {
            return Some(cwd);
        }

        // Fallback: scan first few lines for file paths in tool calls
        let file = fs::File::open(stream_path).ok()?;
        let reader = BufReader::new(file);
        for line in reader.lines().take(20).map_while(Result::ok) {
            let Some(json) = serde_json::from_str::<serde_json::Value>(&line).ok() else {
                continue;
            };
            if json.get("type").and_then(|v| v.as_str()) == Some("tool.execution_start")
                && let Some(file_path) = json
                    .get("data")
                    .and_then(|d| d.get("arguments"))
                    .and_then(|a| a.get("filePath"))
                    .and_then(|v| v.as_str())
            {
                let p = Path::new(file_path);
                let mut candidate = p.parent();
                while let Some(dir) = candidate {
                    if dir.join(".git").exists() {
                        return Some(dir.to_path_buf());
                    }
                    candidate = dir.parent();
                }
            }
        }
        None
    }
}

/// Read Copilot session JSON incrementally.
fn read_session_json(
    path: &Path,
    watermark: Box<dyn WatermarkStrategy>,
    session_id: &str,
    batch_limit: usize,
) -> Result<StreamBatch, StreamError> {
    read_json_array_stream(
        path,
        watermark,
        session_id,
        JsonArrayStreamSpec::new(
            "Copilot session",
            "requests",
            batch_limit,
            RecordIndexAdvancePolicy::ConvertedUsize,
        ),
        |_| StreamError::Parse {
            line: 0,
            message: "requests array not found in Copilot session JSON".to_string(),
        },
        || {
            // VS Code does not expose local transcript files in these remote modes.
            let is_codespaces = std::env::var("CODESPACES").ok().as_deref() == Some("true");
            let is_remote_containers =
                std::env::var("REMOTE_CONTAINERS").ok().as_deref() == Some("true");
            is_codespaces || is_remote_containers
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::stream_watermark::{ByteOffsetWatermark, RecordIndexWatermark};

    #[test]
    fn test_sweep_strategy() {
        let agent = CopilotAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    #[test]
    fn test_determine_format() {
        let json_path = PathBuf::from("/path/to/session.json");
        assert_eq!(
            CopilotAgent::determine_format(&json_path),
            StreamFormat::CopilotSessionJson
        );

        let jsonl_path = PathBuf::from("/path/to/events.jsonl");
        assert_eq!(
            CopilotAgent::determine_format(&jsonl_path),
            StreamFormat::CopilotEventStreamJsonl
        );
    }

    // -- Event stream (JSONL / ByteOffset) batch-resume tests --

    fn make_event_stream_line(i: usize) -> String {
        format!(
            r#"{{"type":"user.message","id":{},"data":{{"content":"msg-{}"}},"timestamp":"2025-01-01T00:00:{:02}Z"}}"#,
            i, i, i
        )
    }

    fn drain_event_stream(
        agent: &CopilotAgent,
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
    fn test_event_stream_batch_resume_no_loss_or_repeat() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::with_suffix(".jsonl").unwrap();
        for i in 0..5 {
            writeln!(file, "{}", make_event_stream_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = CopilotAgent::with_batch_size(2);
        let (events, _) = drain_event_stream(&agent, file.path());

        assert_eq!(events.len(), 5);
        let ids: Vec<u64> = events.iter().map(|e| e["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_event_stream_append_one_after_full_read() {
        use std::fs::OpenOptions;
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::with_suffix(".jsonl").unwrap();
        for i in 0..3 {
            writeln!(file, "{}", make_event_stream_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = CopilotAgent::with_batch_size(2);
        let (all, wm) = drain_event_stream(&agent, file.path());
        assert_eq!(all.len(), 3);

        let mut f = OpenOptions::new().append(true).open(file.path()).unwrap();
        writeln!(f, "{}", make_event_stream_line(3)).unwrap();
        f.flush().unwrap();

        let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["id"].as_u64().unwrap(), 3);
    }

    #[test]
    fn test_event_stream_append_several_after_full_read() {
        use std::fs::OpenOptions;
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::with_suffix(".jsonl").unwrap();
        for i in 0..3 {
            writeln!(file, "{}", make_event_stream_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = CopilotAgent::with_batch_size(2);
        let (_, mut wm) = drain_event_stream(&agent, file.path());

        let mut f = OpenOptions::new().append(true).open(file.path()).unwrap();
        for i in 3..6 {
            writeln!(f, "{}", make_event_stream_line(i)).unwrap();
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

    // -- Session JSON (RecordIndex) batch-resume tests --

    fn make_session_json(request_count: usize) -> String {
        let requests: Vec<String> = (0..request_count)
            .map(|i| {
                format!(
                    r#"{{"id":{},"message":{{"text":"msg-{}"}},"response":[{{"kind":"markdownContent","value":"reply-{}"}}]}}"#,
                    i, i, i
                )
            })
            .collect();
        format!(r#"{{"requests":[{}]}}"#, requests.join(","))
    }

    fn drain_session_json(
        agent: &CopilotAgent,
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
    fn test_session_json_batch_resume_no_loss_or_repeat() {
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        std::io::Write::write_all(&mut file, make_session_json(5).as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();

        let agent = CopilotAgent::with_batch_size(2);
        let (events, _) = drain_session_json(&agent, file.path());

        assert_eq!(events.len(), 5);
        let ids: Vec<u64> = events.iter().map(|e| e["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_session_json_append_one_after_full_read() {
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        std::io::Write::write_all(&mut file, make_session_json(3).as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();

        let agent = CopilotAgent::with_batch_size(2);
        let (all, wm) = drain_session_json(&agent, file.path());
        assert_eq!(all.len(), 3);

        // Rewrite file with 4 requests (simulating append in JSON format)
        std::fs::write(file.path(), make_session_json(4)).unwrap();

        let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["id"].as_u64().unwrap(), 3);
    }

    #[test]
    fn test_session_json_append_several_after_full_read() {
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        std::io::Write::write_all(&mut file, make_session_json(3).as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();

        let agent = CopilotAgent::with_batch_size(2);
        let (_, mut wm) = drain_session_json(&agent, file.path());

        // Rewrite file with 6 requests
        std::fs::write(file.path(), make_session_json(6)).unwrap();

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
    fn test_read_session_json_basic() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        let json = r#"{
            "requests": [
                {
                    "timestamp": 1704067200000,
                    "message": {"text": "Hello"},
                    "response": [
                        {"kind": "markdownContent", "value": "Hi there"}
                    ]
                }
            ],
            "inputState": {
                "selectedModel": {"identifier": "copilot/gpt-4"}
            }
        }"#;
        write!(file, "{}", json).unwrap();
        file.flush().unwrap();

        let agent = CopilotAgent::new();
        let watermark = Box::new(RecordIndexWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test-session")
            .unwrap();

        // Each request object is returned as a raw JSON event
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0]["message"]["text"], "Hello");
        assert_eq!(result.events[0]["response"][0]["kind"], "markdownContent");
    }

    #[test]
    fn test_read_event_stream_basic() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a .jsonl file
        let mut file = NamedTempFile::with_suffix(".jsonl").unwrap();
        writeln!(
            file,
            r#"{{"type":"user.message","data":{{"content":"Hello"}},"timestamp":"2025-01-01T00:00:00Z"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant.message","data":{{"content":"Hi there","modelId":"copilot/gpt-4"}},"timestamp":"2025-01-01T00:00:01Z"}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = CopilotAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test-session")
            .unwrap();

        // Both JSONL lines are returned as raw JSON
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0]["type"], "user.message");
        assert_eq!(result.events[0]["data"]["content"], "Hello");
        assert_eq!(result.events[1]["type"], "assistant.message");
        assert_eq!(result.events[1]["data"]["modelId"], "copilot/gpt-4");
    }

    #[test]
    fn test_extract_event_ids() {
        let agent = CopilotAgent::new();
        let event: serde_json::Value = serde_json::from_str(
            r#"{"type":"user.message","id":"ev-123","parentId":"ev-000","timestamp":"2026-05-11T00:00:00Z"}"#,
        )
        .unwrap();
        let (id, parent_id, third) = agent.extract_event_ids(&event);
        assert_eq!(id, Some("ev-123".to_string()));
        assert_eq!(parent_id, Some("ev-000".to_string()));
        assert_eq!(third, None);
    }

    #[test]
    fn test_extract_event_ids_null_parent() {
        let agent = CopilotAgent::new();
        let event: serde_json::Value =
            serde_json::from_str(r#"{"type":"session.start","id":"ev-001","parentId":null}"#)
                .unwrap();
        let (id, parent_id, _) = agent.extract_event_ids(&event);
        assert_eq!(id, Some("ev-001".to_string()));
        assert_eq!(parent_id, None);
    }

    #[test]
    fn test_infer_cwd_from_workspace_json() {
        use std::path::PathBuf;

        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/copilot_vscode_workspace/GitHub.copilot-chat/transcripts/test-session-abc.jsonl");
        let result = CopilotAgent::infer_cwd_from_workspace_json(&fixture);
        assert_eq!(result, Some(PathBuf::from("/Users/test/project")));
    }

    #[test]
    fn test_infer_cwd_trait_method() {
        use std::path::PathBuf;

        let agent = CopilotAgent::new();
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/copilot_vscode_workspace/GitHub.copilot-chat/transcripts/test-session-abc.jsonl");
        let result = agent.infer_cwd(&fixture);
        // workspace.json says file:///Users/test/project
        assert_eq!(result, Some(PathBuf::from("/Users/test/project")));
    }

    #[test]
    fn test_percent_decode_path() {
        assert_eq!(
            percent_decode_path("/Users/test%20user/my%20project"),
            "/Users/test user/my project"
        );
        assert_eq!(percent_decode_path("/normal/path"), "/normal/path");
        assert_eq!(
            percent_decode_path("/path%2Fwith%2Fencoded"),
            "/path/with/encoded"
        );
        // Multi-byte UTF-8: é = %C3%A9
        assert_eq!(percent_decode_path("/caf%C3%A9/project"), "/café/project");
    }

    #[test]
    fn test_read_vscode_event_stream_fixture() {
        use std::path::PathBuf;

        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/copilot_vscode_event_stream.jsonl");
        let agent = CopilotAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(&fixture, watermark, "test-session")
            .unwrap();

        assert_eq!(result.events.len(), 13);
        assert_eq!(result.events[0]["type"], "session.start");
        assert_eq!(
            result.events[0]["data"]["sessionId"],
            "5fcddc54-3ba1-4fc4-88ac-86bd2ce74c19"
        );
        assert_eq!(result.events[1]["type"], "user.message");
        assert_eq!(result.events[6]["type"], "tool.execution_start");
        assert_eq!(result.events[6]["data"]["toolName"], "read_file");
    }
}
