use crate::streams::sweep::StreamFormat;
use crate::streams::types::StreamError;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub fn extract_model(
    path: &Path,
    format: StreamFormat,
    session_id: Option<&str>,
) -> Result<Option<String>, StreamError> {
    match format {
        StreamFormat::ClaudeJsonl
        | StreamFormat::CopilotEventStreamJsonl
        | StreamFormat::GeminiJsonl => extract_model_from_jsonl_tail(path),
        StreamFormat::CodexJsonl => extract_model_from_codex_jsonl(path),
        StreamFormat::CopilotSessionJson => extract_model_from_copilot_session_json(path),
        StreamFormat::AmpThreadJson => extract_model_from_amp_thread_json(path),
        StreamFormat::OpenCodeSqlite => extract_model_from_opencode_sqlite(path, session_id),
        StreamFormat::CopilotOtelSqlite => extract_model_from_copilot_otel_sqlite(path, session_id),
        // Droid uses extract_model_from_droid_settings() with the settings path instead
        _ => Ok(None),
    }
}

pub fn extract_model_from_droid_settings(
    settings_path: &Path,
) -> Result<Option<String>, StreamError> {
    let content = match std::fs::read_to_string(settings_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(None),
        Err(_) => return Ok(None),
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };

    Ok(json.get("model").and_then(|v| v.as_str()).map(String::from))
}

fn extract_model_from_jsonl_tail(path: &Path) -> Result<Option<String>, StreamError> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(None),
        Err(_) => return Ok(None),
    };

    let file_size = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Ok(None),
    };

    if file_size == 0 {
        return Ok(None);
    }

    let read_size = std::cmp::min(51200, file_size);
    let seek_pos = file_size - read_size;

    if file.seek(SeekFrom::Start(seek_pos)).is_err() {
        return Ok(None);
    }

    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();

    for line in lines.iter().rev() {
        if let Some(model) = extract_model_from_jsonl_line(line) {
            return Ok(Some(model));
        }
    }

    // Tail didn't contain the model — check the head (Copilot CLI emits
    // session.model_change only at session start, which may fall outside the tail window).
    if seek_pos > 0
        && let Some(model) = extract_model_from_jsonl_head(path)
    {
        return Ok(Some(model));
    }

    Ok(None)
}

fn extract_model_from_codex_jsonl(path: &Path) -> Result<Option<String>, StreamError> {
    if let Some(model) = extract_model_from_jsonl_tail(path)? {
        return Ok(Some(model));
    }

    if let Some(model) = extract_model_from_codex_session_meta(path) {
        return Ok(Some(model));
    }

    Ok(extract_model_from_codex_config(path))
}

fn extract_model_from_codex_session_meta(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);

    for line in reader.lines().map_while(Result::ok).take(20) {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };

        if json.get("type").and_then(|v| v.as_str()) != Some("session_meta") {
            continue;
        }

        let Some(payload) = json.get("payload") else {
            continue;
        };
        if let Some(model) = string_candidate(payload.get("model"))
            .or_else(|| string_candidate(payload.get("model_id")))
            .or_else(|| string_candidate(payload.get("modelId")))
        {
            return Some(model);
        }
    }

    None
}

fn extract_model_from_codex_config(path: &Path) -> Option<String> {
    let codex_home = codex_home_from_transcript_path(path)?;
    let config_path = codex_home.join("config.toml");
    let content = std::fs::read_to_string(config_path).ok()?;
    let config: toml::Value = toml::from_str(&content).ok()?;

    toml_string_candidate(config.get("model"))
}

fn codex_home_from_transcript_path(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        if ancestor.file_name().and_then(|name| name.to_str()) == Some(".codex") {
            return Some(ancestor.to_path_buf());
        }
    }

    None
}

fn extract_model_from_jsonl_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(trimmed).ok()?;

    if json.get("type").and_then(|v| v.as_str()) == Some("session.model_change")
        && let Some(model) = json
            .get("data")
            .and_then(|d| d.get("newModel"))
            .and_then(|v| v.as_str())
    {
        return Some(model.to_string());
    }

    let candidate = json
        .get("message")
        .and_then(|m| m.get("model"))
        .and_then(|v| v.as_str())
        .or_else(|| json.get("model").and_then(|v| v.as_str()));

    candidate.and_then(normalize_model)
}

fn string_candidate(value: Option<&serde_json::Value>) -> Option<String> {
    normalize_model(value?.as_str()?)
}

fn toml_string_candidate(value: Option<&toml::Value>) -> Option<String> {
    normalize_model(value?.as_str()?)
}

fn normalize_model(model: &str) -> Option<String> {
    let model = model.trim();
    if model.is_empty() || model == "<synthetic>" {
        return None;
    }
    Some(model.to_string())
}

fn extract_model_from_jsonl_head(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok).take(20) {
        if let Some(model) = extract_model_from_jsonl_line(&line) {
            return Some(model);
        }
    }
    None
}

/// Extracts the model from VS Code Copilot's `models.json` debug log.
/// Given a transcript path like `.../transcripts/{session_id}.jsonl`,
/// derives `.../debug-logs/{session_id}/models.json` and reads the default model.
pub fn extract_model_from_copilot_models_json(
    stream_path: &Path,
) -> Result<Option<String>, StreamError> {
    let session_id = stream_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if session_id.is_empty() {
        return Ok(None);
    }

    // transcript: .../transcripts/{session_id}.jsonl
    // models:     .../debug-logs/{session_id}/models.json
    let transcripts_dir = match stream_path.parent() {
        Some(p) => p,
        None => return Ok(None),
    };
    let copilot_chat_dir = match transcripts_dir.parent() {
        Some(p) => p,
        None => return Ok(None),
    };
    let models_path = copilot_chat_dir
        .join("debug-logs")
        .join(session_id)
        .join("models.json");

    let content = match std::fs::read_to_string(&models_path) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    let models: Vec<serde_json::Value> = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };

    let model = models.iter().find_map(|m| {
        if m.get("is_chat_default").and_then(|v| v.as_bool()) == Some(true) {
            m.get("id").and_then(|v| v.as_str()).map(String::from)
        } else {
            None
        }
    });

    Ok(model)
}

pub fn extract_model_from_copilot_vscode_transcript(
    stream_path: &Path,
    format: StreamFormat,
    chat_session_id: &str,
) -> Result<Option<String>, StreamError> {
    if let Some(model) = extract_model(stream_path, format, None)? {
        return Ok(Some(model));
    }

    if let Some(model) =
        extract_model_from_copilot_otel_for_transcript(stream_path, chat_session_id)?
    {
        return Ok(Some(model));
    }

    extract_model_from_copilot_models_json(stream_path)
}

pub fn extract_model_from_copilot_otel_for_transcript(
    stream_path: &Path,
    chat_session_id: &str,
) -> Result<Option<String>, StreamError> {
    let Some(db_path) = resolve_copilot_otel_db_path(stream_path) else {
        return Ok(None);
    };
    extract_model_from_copilot_otel_sqlite(&db_path, Some(chat_session_id))
}

fn resolve_copilot_otel_db_path(stream_path: &Path) -> Option<PathBuf> {
    if let Ok(path) = std::env::var("GIT_AI_COPILOT_OTEL_DB_PATH") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }

    // transcript: .../User/workspaceStorage/{hash}/GitHub.copilot-chat/transcripts/{id}.jsonl
    // OTEL DB:    .../User/globalStorage/github.copilot-chat/agent-traces.db
    let workspace_storage_root = stream_path.parent()?.parent()?.parent()?.parent()?;
    let user_dir = workspace_storage_root.parent()?;
    let otel_db = user_dir
        .join("globalStorage")
        .join("github.copilot-chat")
        .join("agent-traces.db");

    otel_db.exists().then_some(otel_db)
}

fn extract_model_from_copilot_otel_sqlite(
    path: &Path,
    chat_session_id: Option<&str>,
) -> Result<Option<String>, StreamError> {
    let Some(chat_session_id) = chat_session_id.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    let conn = match crate::streams::agents::opencode::open_sqlite_readonly(path) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    let newest_request_model: Option<String> = conn
        .query_row(
            "SELECT request_model FROM spans \
             WHERE chat_session_id = ?1 AND request_model IS NOT NULL AND request_model != '' \
             ORDER BY end_time_ms DESC, span_id DESC LIMIT 1",
            rusqlite::params![chat_session_id],
            |row| row.get(0),
        )
        .ok();

    if newest_request_model.is_some() {
        return Ok(newest_request_model);
    }

    let newest_response_model: Option<String> = conn
        .query_row(
            "SELECT response_model FROM spans \
             WHERE chat_session_id = ?1 AND response_model IS NOT NULL AND response_model != '' \
             ORDER BY end_time_ms DESC, span_id DESC LIMIT 1",
            rusqlite::params![chat_session_id],
            |row| row.get(0),
        )
        .ok();

    Ok(newest_response_model)
}

fn extract_model_from_copilot_session_json(path: &Path) -> Result<Option<String>, StreamError> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };

    let model = json
        .get("requests")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|req| {
                req.get("modelId")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
        });

    Ok(model)
}

fn extract_model_from_amp_thread_json(path: &Path) -> Result<Option<String>, StreamError> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };

    let model = json
        .get("messages")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|msg| {
                msg.get("usage")
                    .and_then(|u| u.get("model"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
        });

    Ok(model)
}

fn extract_model_from_opencode_sqlite(
    path: &Path,
    session_id: Option<&str>,
) -> Result<Option<String>, StreamError> {
    let conn = match crate::streams::agents::opencode::open_sqlite_readonly(path) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    // OpenCode stores model info in two places depending on message role:
    //   User messages:     data.model.modelID  (nested object)
    //   Assistant messages: data.modelID        (top-level string)
    let (query, params): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = match session_id {
        Some(sid) => (
            "SELECT data FROM message WHERE session_id = ? AND (data LIKE '%\"modelID\"%' OR data LIKE '%\"model\"%') LIMIT 1",
            vec![Box::new(sid.to_string())],
        ),
        None => (
            "SELECT data FROM message WHERE (data LIKE '%\"modelID\"%' OR data LIKE '%\"model\"%') LIMIT 1",
            vec![],
        ),
    };

    let result: Option<String> = conn
        .query_row(query, rusqlite::params_from_iter(params.iter()), |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .and_then(|data| {
            let json: serde_json::Value = serde_json::from_str(&data).ok()?;
            // Try user message format: data.model.modelID
            if let Some(model) = json
                .get("model")
                .and_then(|m| m.get("modelID"))
                .and_then(|v| v.as_str())
            {
                return Some(model.to_string());
            }
            // Try assistant message format: data.modelID
            json.get("modelID")
                .and_then(|v| v.as_str())
                .map(String::from)
        });

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn create_copilot_otel_db(path: &Path) -> rusqlite::Connection {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let conn = crate::sqlite::open_with_memory_limits(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE spans (
                span_id TEXT PRIMARY KEY,
                chat_session_id TEXT,
                request_model TEXT,
                response_model TEXT,
                end_time_ms REAL NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn insert_copilot_otel_model(
        conn: &rusqlite::Connection,
        span_id: &str,
        chat_session_id: &str,
        request_model: Option<&str>,
        response_model: Option<&str>,
        end_time_ms: f64,
    ) {
        conn.execute(
            "INSERT INTO spans (span_id, chat_session_id, request_model, response_model, end_time_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                span_id,
                chat_session_id,
                request_model,
                response_model,
                end_time_ms
            ],
        )
        .unwrap();
    }

    fn create_copilot_vscode_workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let user_dir = dir.path().join("User");
        let transcript_path = user_dir
            .join("workspaceStorage")
            .join("workspace-1")
            .join("GitHub.copilot-chat")
            .join("transcripts")
            .join("session-abc.jsonl");
        std::fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript_path,
            r#"{"type":"session.start","data":{"sessionId":"session-abc"}}"#,
        )
        .unwrap();

        let models_path = user_dir
            .join("workspaceStorage")
            .join("workspace-1")
            .join("GitHub.copilot-chat")
            .join("debug-logs")
            .join("session-abc")
            .join("models.json");
        std::fs::create_dir_all(models_path.parent().unwrap()).unwrap();
        std::fs::write(
            &models_path,
            r#"[
                {"id":"claude-sonnet-4","is_chat_default":false},
                {"id":"gpt-4.1","is_chat_default":true}
            ]"#,
        )
        .unwrap();

        let otel_db_path = user_dir
            .join("globalStorage")
            .join("github.copilot-chat")
            .join("agent-traces.db");

        (dir, transcript_path, otel_db_path)
    }

    #[test]
    fn test_extract_model_claude() {
        let path = fixture_path("example-claude-code.jsonl");
        let result = extract_model(&path, StreamFormat::ClaudeJsonl, None).unwrap();
        assert_eq!(result, Some("claude-sonnet-4-20250514".to_string()));
    }

    #[test]
    fn test_extract_model_droid_settings() {
        let path = fixture_path("droid-session.settings.json");
        let result = extract_model_from_droid_settings(&path).unwrap();
        assert_eq!(result, Some("custom:BYOK-GPT-5-MINI-0".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_session() {
        let path = fixture_path("copilot_session_simple.json");
        let result = extract_model(&path, StreamFormat::CopilotSessionJson, None).unwrap();
        assert_eq!(result, Some("copilot/claude-sonnet-4".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_event_stream() {
        let path = fixture_path("copilot_session_event_stream.jsonl");
        let result = extract_model(&path, StreamFormat::CopilotEventStreamJsonl, None).unwrap();
        // No model field in this fixture
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_gemini() {
        let path = fixture_path("gemini-session-simple.jsonl");
        let result = extract_model(&path, StreamFormat::GeminiJsonl, None).unwrap();
        assert_eq!(result, Some("gemini-2.5-flash".to_string()));
    }

    #[test]
    fn test_extract_model_codex_session_meta_model() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"model":"gpt-5.3-codex","model_provider":"openai_https"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let result = extract_model(file.path(), StreamFormat::CodexJsonl, None).unwrap();
        assert_eq!(result, Some("gpt-5.3-codex".to_string()));
    }

    #[test]
    fn test_extract_model_codex_skips_session_meta_without_payload() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        writeln!(file, r#"{{"type":"session_meta"}}"#).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"model":"gpt-5.3-codex"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let result = extract_model(file.path(), StreamFormat::CodexJsonl, None).unwrap();
        assert_eq!(result, Some("gpt-5.3-codex".to_string()));
    }

    #[test]
    fn test_extract_model_codex_config_fallback_when_session_model_missing() {
        use std::io::Write;

        let dir = tempfile::TempDir::new().unwrap();
        let codex_home = dir.path().join(".codex");
        let session_dir = codex_home.join("sessions/2026/06/30");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            codex_home.join("config.toml"),
            r#"model = "gpt-5.5"
model_provider = "openai_https"

[profiles.default]
model = "wrong-profile-model"
"#,
        )
        .unwrap();

        let transcript = session_dir.join("rollout-test.jsonl");
        let mut file = File::create(&transcript).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"session_id":"sess-1","model":null,"model_provider":"openai_https"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let result = extract_model(&transcript, StreamFormat::CodexJsonl, None).unwrap();
        assert_eq!(result, Some("gpt-5.5".to_string()));
    }

    #[test]
    fn test_extract_model_codex_prefers_transcript_model_over_config() {
        use std::io::Write;

        let dir = tempfile::TempDir::new().unwrap();
        let codex_home = dir.path().join(".codex");
        let session_dir = codex_home.join("sessions/2026/06/30");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(codex_home.join("config.toml"), r#"model = "config-model""#).unwrap();

        let transcript = session_dir.join("rollout-test.jsonl");
        let mut file = File::create(&transcript).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"model":"transcript-model"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let result = extract_model(&transcript, StreamFormat::CodexJsonl, None).unwrap();
        assert_eq!(result, Some("transcript-model".to_string()));
    }

    #[test]
    fn test_extract_model_amp() {
        let path = fixture_path("amp-threads/T-019ca1ce-3ae2-7686-a41e-ccc078837f8a.json");
        let result = extract_model(&path, StreamFormat::AmpThreadJson, None).unwrap();
        assert_eq!(result, Some("claude-opus-4-6".to_string()));
    }

    #[test]
    fn test_extract_model_opencode() {
        let path = fixture_path("opencode-sqlite/opencode.db");
        let result = extract_model(
            &path,
            StreamFormat::OpenCodeSqlite,
            Some("test-session-123"),
        )
        .unwrap();
        assert_eq!(result, Some("gpt-5".to_string()));
    }

    #[test]
    fn test_extract_model_opencode_assistant_message_format() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             INSERT INTO message VALUES ('msg-1', 'sess-1', 1000, 1000, '{\"role\":\"assistant\",\"modelID\":\"claude-opus-4-6\",\"providerID\":\"anthropic\"}');",
        ).unwrap();
        drop(conn);

        let result = extract_model(&db_path, StreamFormat::OpenCodeSqlite, Some("sess-1")).unwrap();
        assert_eq!(result, Some("claude-opus-4-6".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_cli() {
        let path = fixture_path("copilot_cli_session_events.jsonl");
        let result = extract_model(&path, StreamFormat::CopilotEventStreamJsonl, None).unwrap();
        assert_eq!(result, Some("gpt-4.1".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_cli_no_model() {
        let path = fixture_path("copilot_cli_session_no_model.jsonl");
        let result = extract_model(&path, StreamFormat::CopilotEventStreamJsonl, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_missing_file() {
        let path = PathBuf::from("/nonexistent/path/to/file.jsonl");
        let result = extract_model(&path, StreamFormat::ClaudeJsonl, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_empty_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let result = extract_model(file.path(), StreamFormat::ClaudeJsonl, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_droid_settings_missing_file() {
        let path = PathBuf::from("/nonexistent/settings.json");
        let result = extract_model_from_droid_settings(&path).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_unsupported_format_returns_none() {
        let path = fixture_path("example-claude-code.jsonl");
        let result = extract_model(&path, StreamFormat::DroidJsonl, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_claude_model_not_on_last_line() {
        let path = fixture_path("claude-model-not-last.jsonl");
        let result = extract_model(&path, StreamFormat::ClaudeJsonl, None).unwrap();
        assert_eq!(result, Some("claude-opus-4-6".to_string()));
    }

    #[test]
    fn test_extract_model_skips_synthetic_model() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"user","message":{{"content":"hello"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"assistant","message":{{"model":"claude-opus-4-6","content":[{{"type":"text","text":"hi"}}]}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"assistant","message":{{"model":"<synthetic>","content":[{{"type":"text","text":"bye"}}]}}}}"#).unwrap();
        file.flush().unwrap();

        let result = extract_model(file.path(), StreamFormat::ClaudeJsonl, None).unwrap();
        assert_eq!(result, Some("claude-opus-4-6".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_vscode_models_json() {
        let path = fixture_path(
            "copilot_vscode_workspace/GitHub.copilot-chat/transcripts/test-session-abc.jsonl",
        );
        let result = extract_model_from_copilot_models_json(&path).unwrap();
        assert_eq!(result, Some("gpt-4.1".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_vscode_models_json_missing() {
        let path = PathBuf::from("/nonexistent/transcripts/fake-session.jsonl");
        let result = extract_model_from_copilot_models_json(&path).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_copilot_otel_newest_request_model_wins() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("agent-traces.db");
        let conn = create_copilot_otel_db(&db_path);
        insert_copilot_otel_model(
            &conn,
            "span-1",
            "session-abc",
            Some("gpt-4.1"),
            Some("gpt-4.1-2025-04-14"),
            1000.0,
        );
        insert_copilot_otel_model(
            &conn,
            "span-2",
            "session-abc",
            Some("claude-sonnet-4"),
            Some("claude-sonnet-4-20250514"),
            2000.0,
        );
        insert_copilot_otel_model(
            &conn,
            "span-3",
            "session-abc",
            None,
            Some("response-only-newer"),
            3000.0,
        );
        insert_copilot_otel_model(
            &conn,
            "span-4",
            "other-session",
            Some("gpt-5"),
            Some("gpt-5-2026-01-01"),
            4000.0,
        );
        drop(conn);

        let result = extract_model(
            &db_path,
            StreamFormat::CopilotOtelSqlite,
            Some("session-abc"),
        )
        .unwrap();
        assert_eq!(result, Some("claude-sonnet-4".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_otel_falls_back_to_response_model() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("agent-traces.db");
        let conn = create_copilot_otel_db(&db_path);
        insert_copilot_otel_model(
            &conn,
            "span-1",
            "session-abc",
            None,
            Some("gpt-4.1-2025-04-14"),
            1000.0,
        );
        insert_copilot_otel_model(
            &conn,
            "span-2",
            "session-abc",
            None,
            Some("gpt-5-2026-01-01"),
            2000.0,
        );
        drop(conn);

        let result = extract_model(
            &db_path,
            StreamFormat::CopilotOtelSqlite,
            Some("session-abc"),
        )
        .unwrap();
        assert_eq!(result, Some("gpt-5-2026-01-01".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_vscode_transcript_prefers_otel_over_models_json() {
        let (_dir, transcript_path, otel_db_path) = create_copilot_vscode_workspace();
        let conn = create_copilot_otel_db(&otel_db_path);
        insert_copilot_otel_model(
            &conn,
            "span-1",
            "session-abc",
            Some("claude-sonnet-4"),
            Some("claude-sonnet-4-20250514"),
            1000.0,
        );
        drop(conn);

        let result = extract_model_from_copilot_vscode_transcript(
            &transcript_path,
            StreamFormat::CopilotEventStreamJsonl,
            "session-abc",
        )
        .unwrap();
        assert_eq!(result, Some("claude-sonnet-4".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_vscode_transcript_falls_back_to_models_json() {
        let (_dir, transcript_path, _otel_db_path) = create_copilot_vscode_workspace();

        let result = extract_model_from_copilot_vscode_transcript(
            &transcript_path,
            StreamFormat::CopilotEventStreamJsonl,
            "session-abc",
        )
        .unwrap();
        assert_eq!(result, Some("gpt-4.1".to_string()));
    }

    #[test]
    fn test_extract_model_head_fallback_for_large_file() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        // model_change at the start
        writeln!(file, r#"{{"type":"session.start","data":{{"sessionId":"s1"}},"id":"e1","timestamp":"2026-01-01T00:00:00Z","parentId":null}}"#).unwrap();
        writeln!(file, r#"{{"type":"session.model_change","data":{{"newModel":"gpt-4.1"}},"id":"e2","timestamp":"2026-01-01T00:00:01Z","parentId":"e1"}}"#).unwrap();
        // Pad with >50KB of filler events so the model_change falls outside the tail window
        for i in 0..600 {
            writeln!(file, r#"{{"type":"user.message","data":{{"content":"padding message number {} with extra text to make the line longer and push past the fifty kilobyte tail read window boundary"}},"id":"pad-{}","timestamp":"2026-01-01T00:01:{:02}Z","parentId":null}}"#, i, i, i % 60).unwrap();
        }
        file.flush().unwrap();

        let size = std::fs::metadata(file.path()).unwrap().len();
        assert!(
            size > 51200,
            "file must exceed 50KB tail window, got {}",
            size
        );

        let result =
            extract_model(file.path(), StreamFormat::CopilotEventStreamJsonl, None).unwrap();
        assert_eq!(result, Some("gpt-4.1".to_string()));
    }
}
