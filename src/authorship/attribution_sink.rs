use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::GitAiError;

/// A single attribution event emitted when a note is sealed.
///
/// Matches the JSON schema (v2) sent to `post_notes_updated` hooks,
/// extended with structured `attributions` when fingerprinting is enabled.
#[derive(Debug, Clone, Serialize)]
pub struct AttributionEvent {
    pub schema_version: u32,
    pub commit_sha: String,
    pub repo_url: String,
    pub repo_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    pub git_dir: String,
    pub branch: String,
    pub is_default_branch: bool,
    pub note_content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributions: Option<Vec<FileAttribution>>,
}

/// Per-file attribution with ordered fingerprints for squash-safe matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAttribution {
    pub file: String,
    pub session_id: String,
    pub model: String,
    pub tool: String,
    pub line_ranges: Vec<[u32; 2]>,
    pub fingerprints: Vec<String>,
    pub fingerprints_complete: bool,
}

/// Configuration for a single attribution sink.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SinkConfig {
    Stdout,
    File {
        path: String,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        allow_insecure: bool,
    },
}

/// Trait for pluggable attribution delivery.
///
/// Implementations are best-effort: failures are logged but never block
/// the git commit path. git-ai takes no transport opinion — routing to
/// a specific system (Kafka, AIFX, etc.) is the consumer's adapter
/// behind `http` or the shell hook.
pub trait AttributionSink {
    fn emit(&self, events: &[AttributionEvent]) -> Result<(), GitAiError>;
}

pub struct StdoutSink;

impl AttributionSink for StdoutSink {
    fn emit(&self, events: &[AttributionEvent]) -> Result<(), GitAiError> {
        let json = serde_json::to_string(events).map_err(GitAiError::JsonError)?;
        println!("{}", json);
        Ok(())
    }
}

pub struct FileSink {
    path: PathBuf,
}

impl FileSink {
    pub fn new(path: &str) -> Self {
        Self {
            path: PathBuf::from(path),
        }
    }
}

impl AttributionSink for FileSink {
    fn emit(&self, events: &[AttributionEvent]) -> Result<(), GitAiError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(GitAiError::IoError)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(GitAiError::IoError)?;
        for event in events {
            let json = serde_json::to_string(event).map_err(GitAiError::JsonError)?;
            writeln!(file, "{}", json).map_err(GitAiError::IoError)?;
        }
        Ok(())
    }
}

pub struct HttpSink {
    url: String,
    headers: HashMap<String, String>,
    allow_insecure: bool,
}

impl HttpSink {
    pub fn new(url: &str, headers: HashMap<String, String>, allow_insecure: bool) -> Self {
        Self {
            url: url.to_string(),
            headers,
            allow_insecure,
        }
    }
}

impl AttributionSink for HttpSink {
    fn emit(&self, events: &[AttributionEvent]) -> Result<(), GitAiError> {
        if !self.allow_insecure && !self.url.starts_with("https://") {
            return Err(GitAiError::Generic(format!(
                "HTTP attribution sink requires an https:// URL; set allow_insecure=true only for local development: {}",
                self.url
            )));
        }
        let json = serde_json::to_string(events).map_err(GitAiError::JsonError)?;
        let agent = crate::http::build_agent(Some(10));
        let mut request = agent.post(&self.url);
        for (key, value) in &self.headers {
            request = request.header(key.as_str(), value.as_str());
        }
        request = request.header("Content-Type", "application/json");
        match crate::http::send_with_body(request, &json) {
            Ok(resp) if resp.status_code >= 400 => {
                tracing::debug!(
                    "[attribution_sink] HTTP sink POST to {} returned status {}",
                    self.url,
                    resp.status_code
                );
            }
            Err(e) => {
                tracing::debug!(
                    "[attribution_sink] HTTP sink POST to {} failed: {}",
                    self.url,
                    e
                );
            }
            _ => {}
        }
        Ok(())
    }
}

/// Build sink instances from config.
pub fn build_sinks(configs: &[SinkConfig]) -> Vec<Box<dyn AttributionSink + Send>> {
    configs
        .iter()
        .filter_map(|cfg| -> Option<Box<dyn AttributionSink + Send>> {
            match cfg {
                SinkConfig::Stdout => Some(Box::new(StdoutSink)),
                SinkConfig::File { path } => Some(Box::new(FileSink::new(path))),
                SinkConfig::Http {
                    url,
                    headers,
                    allow_insecure,
                } => Some(Box::new(HttpSink::new(
                    url,
                    headers.clone(),
                    *allow_insecure,
                ))),
            }
        })
        .collect()
}

/// Dispatch attribution events to all configured sinks. Failures are logged,
/// never propagated — the commit path must not be blocked.
pub fn dispatch_to_sinks(events: &[AttributionEvent]) {
    let sink_configs = Config::get().attribution_sinks().clone();
    if sink_configs.is_empty() {
        return;
    }
    let sinks = build_sinks(&sink_configs);
    for sink in &sinks {
        if let Err(e) = sink.emit(events) {
            tracing::debug!("[attribution_sink] Sink dispatch failed: {}", e);
        }
    }
}

/// Build `AttributionEvent` structs from the same data used for shell hooks.
pub fn events_from_hook_payload(payload: &[serde_json::Value]) -> Vec<AttributionEvent> {
    payload
        .iter()
        .filter_map(|entry| {
            let attributions = entry
                .get("attributions")
                .and_then(|value| serde_json::from_value(value.clone()).ok());
            Some(AttributionEvent {
                schema_version: entry.get("schema_version")?.as_u64()? as u32,
                commit_sha: entry.get("commit_sha")?.as_str()?.to_string(),
                repo_url: entry.get("repo_url")?.as_str()?.to_string(),
                repo_name: entry.get("repo_name")?.as_str()?.to_string(),
                repo_path: entry
                    .get("repo_path")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                git_dir: entry.get("git_dir")?.as_str()?.to_string(),
                branch: entry.get("branch")?.as_str()?.to_string(),
                is_default_branch: entry.get("is_default_branch")?.as_bool()?,
                note_content: entry.get("note_content")?.as_str()?.to_string(),
                attributions,
            })
        })
        .collect()
}

use crate::config::Config;

/// Get the configured attribution sink path (if a file sink is the first configured sink).
/// Used internally for testing.
pub fn configured_file_sink_path() -> Option<PathBuf> {
    let configs = Config::get().attribution_sinks().clone();
    configs.into_iter().find_map(|c| match c {
        SinkConfig::File { path } => Some(PathBuf::from(path)),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event() -> AttributionEvent {
        AttributionEvent {
            schema_version: 2,
            commit_sha: "abc123".to_string(),
            repo_url: "https://github.com/org/repo.git".to_string(),
            repo_name: "repo".to_string(),
            repo_path: Some("/home/user/repo".to_string()),
            git_dir: "/home/user/repo/.git".to_string(),
            branch: "main".to_string(),
            is_default_branch: true,
            note_content: "test note".to_string(),
            attributions: None,
        }
    }

    #[test]
    fn test_event_serialization_includes_all_fields() {
        let event = sample_event();
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema_version"], 2);
        assert_eq!(parsed["commit_sha"], "abc123");
        assert_eq!(parsed["repo_path"], "/home/user/repo");
        assert_eq!(parsed["git_dir"], "/home/user/repo/.git");
    }

    #[test]
    fn test_event_serialization_omits_none_fields() {
        let mut event = sample_event();
        event.repo_path = None;
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("repo_path").is_none());
    }

    #[test]
    fn test_file_sink_writes_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let sink = FileSink::new(path.to_str().unwrap());
        let events = vec![sample_event(), sample_event()];
        sink.emit(&events).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["schema_version"], 2);
    }

    #[test]
    fn test_events_from_hook_payload() {
        let payload = vec![serde_json::json!({
            "schema_version": 2,
            "commit_sha": "abc",
            "repo_url": "url",
            "repo_name": "name",
            "repo_path": "/path",
            "git_dir": "/path/.git",
            "branch": "main",
            "is_default_branch": true,
            "note_content": "note"
        })];
        let events = events_from_hook_payload(&payload);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].commit_sha, "abc");
        assert_eq!(events[0].repo_path, Some("/path".to_string()));
    }

    #[test]
    fn test_sink_config_deserialization() {
        let json = r#"[
            {"type": "stdout"},
            {"type": "file", "path": "/tmp/test.jsonl"},
            {"type": "http", "url": "https://example.com", "headers": {"X-Key": "val"}}
        ]"#;
        let configs: Vec<SinkConfig> = serde_json::from_str(json).unwrap();
        assert_eq!(configs.len(), 3);
        let sinks = build_sinks(&configs);
        assert_eq!(sinks.len(), 3);
    }

    #[test]
    fn test_http_sink_rejects_insecure_url_by_default() {
        let sink = HttpSink::new("http://example.com", HashMap::new(), false);
        let error = sink.emit(&[sample_event()]).unwrap_err();
        assert!(error.to_string().contains("requires an https:// URL"));
    }
}
