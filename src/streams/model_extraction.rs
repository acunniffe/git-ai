use crate::streams::sweep::StreamFormat;
use crate::streams::types::StreamError;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const MAX_JSONL_SCAN_BYTES: u64 = 50 * 1024;
const MAX_JSONL_HEAD_SCAN_BYTES: usize = 1024 * 1024;
const MAX_JSONL_HEAD_LINES: usize = 20;
const MAX_CODEX_CONFIG_BYTES: u64 = 1024 * 1024;

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
    let (model, tail_was_truncated) =
        extract_model_from_jsonl_tail_with(path, extract_model_from_jsonl_line)?;
    if model.is_some() {
        return Ok(model);
    }

    // Tail didn't contain the model — check the head (Copilot CLI emits
    // session.model_change only at session start, which may fall outside the tail window).
    if tail_was_truncated
        && let Some(model) = extract_model_from_jsonl_head_with(path, extract_model_from_jsonl_line)
    {
        return Ok(Some(model));
    }

    Ok(None)
}

fn extract_model_from_jsonl_tail_with(
    path: &Path,
    extract_from_line: fn(&str) -> Option<String>,
) -> Result<(Option<String>, bool), StreamError> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((None, false)),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok((None, false));
        }
        Err(_) => return Ok((None, false)),
    };

    let file_size = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Ok((None, false)),
    };

    if file_size == 0 {
        return Ok((None, false));
    }

    let read_size = std::cmp::min(MAX_JSONL_SCAN_BYTES, file_size);
    let seek_pos = file_size - read_size;

    if file.seek(SeekFrom::Start(seek_pos)).is_err() {
        return Ok((None, false));
    }

    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();

    for line in lines.iter().rev() {
        if let Some(model) = extract_from_line(line) {
            return Ok((Some(model), seek_pos > 0));
        }
    }

    Ok((None, seek_pos > 0))
}

fn extract_model_from_codex_jsonl(path: &Path) -> Result<Option<String>, StreamError> {
    let (model, _) = extract_model_from_jsonl_tail_with(path, extract_model_from_codex_jsonl_line)?;
    if model.is_some() {
        return Ok(model);
    }

    if let Some(model) =
        extract_model_from_jsonl_head_with(path, extract_model_from_codex_jsonl_line)
    {
        return Ok(Some(model));
    }

    Ok(extract_model_from_codex_config(path))
}

fn extract_model_from_jsonl_head_with(
    path: &Path,
    extract_from_line: fn(&str) -> Option<String>,
) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut line_is_oversized = false;
    let mut lines_scanned = 0;
    let mut bytes_scanned = 0;

    while lines_scanned < MAX_JSONL_HEAD_LINES && bytes_scanned < MAX_JSONL_HEAD_SCAN_BYTES {
        let bytes_remaining = MAX_JSONL_HEAD_SCAN_BYTES - bytes_scanned;
        let (consumed, reached_newline) = {
            let buffer = reader.fill_buf().ok()?;
            if buffer.is_empty() {
                if !line_is_oversized
                    && let Ok(line) = std::str::from_utf8(&line)
                    && let Some(model) = extract_from_line(line)
                {
                    return Some(model);
                }
                break;
            }

            let searchable = &buffer[..buffer.len().min(bytes_remaining)];
            let newline = searchable.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(searchable.len(), |index| index + 1);

            if !line_is_oversized {
                if line.len() + consumed <= MAX_JSONL_SCAN_BYTES as usize {
                    line.extend_from_slice(&searchable[..consumed]);
                } else {
                    line.clear();
                    line_is_oversized = true;
                }
            }

            (consumed, newline.is_some())
        };

        reader.consume(consumed);
        bytes_scanned += consumed;

        if reached_newline {
            lines_scanned += 1;
            if !line_is_oversized
                && let Ok(line) = std::str::from_utf8(&line)
                && let Some(model) = extract_from_line(line)
            {
                return Some(model);
            }
            line.clear();
            line_is_oversized = false;
        }
    }

    None
}

fn extract_model_from_codex_jsonl_line(line: &str) -> Option<String> {
    let json = serde_json::from_str::<serde_json::Value>(line.trim()).ok()?;
    if !matches!(
        json.get("type").and_then(|v| v.as_str()),
        Some("session_meta" | "turn_context")
    ) {
        return None;
    }

    let payload = json.get("payload")?;
    string_candidate(payload.get("model"))
        .or_else(|| string_candidate(payload.get("model_id")))
        .or_else(|| string_candidate(payload.get("modelId")))
}

fn extract_model_from_codex_config(path: &Path) -> Option<String> {
    let codex_home = codex_home_from_transcript_path(path)?;
    let config_path = codex_home.join("config.toml");
    let file = File::open(config_path).ok()?;
    let mut content = String::new();
    file.take(MAX_CODEX_CONFIG_BYTES + 1)
        .read_to_string(&mut content)
        .ok()?;
    if content.len() as u64 > MAX_CODEX_CONFIG_BYTES {
        return None;
    }
    let config: toml::Value = toml::from_str(&content).ok()?;

    config
        .get("profile")
        .and_then(toml::Value::as_str)
        .and_then(|profile| config.get("profiles")?.get(profile)?.get("model"))
        .and_then(|model| toml_string_candidate(Some(model)))
        .or_else(|| toml_string_candidate(config.get("model")))
}

fn codex_home_from_transcript_path(path: &Path) -> Option<PathBuf> {
    let configured_home = crate::mdm::utils::codex_home_dir();
    if path.starts_with(&configured_home) {
        return Some(configured_home);
    }

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

    // Deliberately no models.json fallback: its `is_chat_default` entry is the
    // workspace-wide default model, not the user's per-session selection, so it
    // mislabels sessions. Better to report no model than a wrong one.
    Ok(extract_model_from_copilot_chat_sessions(
        stream_path,
        chat_session_id,
    ))
}

/// Reads the user's selected model for a session from VS Code's own
/// `workspaceStorage/{hash}/chatSessions/` store. Chat session files record the
/// picker selection (`inputState.selectedModel.identifier`) and per-request
/// `modelId`s; requests are matched to the transcript session via the
/// `sessionId` their results reference.
fn extract_model_from_copilot_chat_sessions(
    stream_path: &Path,
    chat_session_id: &str,
) -> Option<String> {
    if chat_session_id.is_empty() {
        return None;
    }

    let chat_sessions_dir = copilot_chat_sessions_dir(stream_path)?;
    let entries = std::fs::read_dir(chat_sessions_dir).ok()?;
    let mut candidates = CopilotModelCandidates::default();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext != "json" && ext != "jsonl" {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(_) => continue,
        };

        if !content.contains(chat_session_id) {
            continue;
        }

        collect_model_candidates_from_chat_session_content(
            &content,
            chat_session_id,
            &mut candidates,
        );

        if candidates.request_non_auto_model_id.is_some() {
            break;
        }
    }

    candidates.best()
}

// transcript:     .../workspaceStorage/{hash}/GitHub.copilot-chat/transcripts/{id}.jsonl
// chat sessions:  .../workspaceStorage/{hash}/chatSessions/
fn copilot_chat_sessions_dir(stream_path: &Path) -> Option<PathBuf> {
    let transcripts_dir = stream_path.parent()?;
    if !dir_name_matches(transcripts_dir, "transcripts") {
        return None;
    }

    let copilot_dir = transcripts_dir.parent()?;
    if !dir_name_matches(copilot_dir, "github.copilot-chat") {
        return None;
    }

    let chat_sessions_dir = copilot_dir.parent()?.join("chatSessions");
    chat_sessions_dir.is_dir().then_some(chat_sessions_dir)
}

fn dir_name_matches(dir: &Path, expected: &str) -> bool {
    dir.file_name()
        .and_then(|v| v.to_str())
        .map(|name| name.eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}

#[derive(Default)]
struct CopilotModelCandidates {
    request_non_auto_model_id: Option<String>,
    request_model_id: Option<String>,
    session_non_auto_model_id: Option<String>,
    session_model_id: Option<String>,
}

impl CopilotModelCandidates {
    fn best(self) -> Option<String> {
        self.request_non_auto_model_id
            .or(self.request_model_id)
            .or(self.session_non_auto_model_id)
            .or(self.session_model_id)
    }
}

fn collect_model_candidates_from_chat_session_content(
    content: &str,
    chat_session_id: &str,
    candidates: &mut CopilotModelCandidates,
) {
    // Chat session files are either a single JSON document or JSONL where kind 0
    // carries the session object and kind 2 carries request batches.
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) {
        collect_model_candidates_from_session_object(&parsed, chat_session_id, candidates);
        return;
    }

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let parsed_line: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => continue,
        };

        match parsed_line.get("kind").and_then(|v| v.as_u64()) {
            Some(0) => {
                if let Some(session_obj) = parsed_line.get("v") {
                    collect_model_candidates_from_session_object(
                        session_obj,
                        chat_session_id,
                        candidates,
                    );
                }
            }
            Some(2) => {
                if let Some(requests) = parsed_line.get("v").and_then(|v| v.as_array()) {
                    for request in requests {
                        collect_model_candidates_from_request(request, chat_session_id, candidates);
                    }
                }
            }
            _ => {
                collect_model_candidates_from_session_object(
                    &parsed_line,
                    chat_session_id,
                    candidates,
                );
            }
        }
    }
}

fn collect_model_candidates_from_session_object(
    session_obj: &serde_json::Value,
    chat_session_id: &str,
    candidates: &mut CopilotModelCandidates,
) {
    if let Some(selected_model) = session_obj
        .get("inputState")
        .and_then(|v| v.get("selectedModel"))
        .and_then(|v| v.get("identifier"))
        .and_then(|v| v.as_str())
    {
        record_model_candidate(
            selected_model,
            &mut candidates.session_model_id,
            &mut candidates.session_non_auto_model_id,
        );
    }

    if let Some(requests) = session_obj.get("requests").and_then(|v| v.as_array()) {
        for request in requests {
            collect_model_candidates_from_request(request, chat_session_id, candidates);
        }
    }
}

fn collect_model_candidates_from_request(
    request: &serde_json::Value,
    chat_session_id: &str,
    candidates: &mut CopilotModelCandidates,
) {
    if !request_matches_transcript_session(request, chat_session_id) {
        return;
    }

    if let Some(model_id) = request.get("modelId").and_then(|v| v.as_str()) {
        record_model_candidate(
            model_id,
            &mut candidates.request_model_id,
            &mut candidates.request_non_auto_model_id,
        );
    }
}

fn request_matches_transcript_session(request: &serde_json::Value, chat_session_id: &str) -> bool {
    [
        request.get("result").and_then(|v| v.get("metadata")),
        request.get("result"),
        Some(request),
    ]
    .iter()
    .flatten()
    .any(|obj| {
        obj.get("sessionId")
            .and_then(|v| v.as_str())
            .map(|session_id| session_id == chat_session_id)
            .unwrap_or(false)
    })
}

fn record_model_candidate(
    model_id: &str,
    slot: &mut Option<String>,
    non_auto_slot: &mut Option<String>,
) {
    let model = model_id.trim();
    if model.is_empty() {
        return;
    }

    if slot.is_none() {
        *slot = Some(model.to_string());
    }

    if !model.eq_ignore_ascii_case("copilot/auto") && non_auto_slot.is_none() {
        *non_auto_slot = Some(model.to_string());
    }
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

    fn extract_codex_model_with_config(config: &str) -> Option<String> {
        let dir = tempfile::TempDir::new().unwrap();
        let codex_home = dir.path().join(".codex");
        let session_dir = codex_home.join("sessions/2026/06/30");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(codex_home.join("config.toml"), config).unwrap();

        let transcript = session_dir.join("rollout-test.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"session_meta","payload":{"model":null}}"#,
        )
        .unwrap();

        extract_model(&transcript, StreamFormat::CodexJsonl, None).unwrap()
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
        let workspace_dir = user_dir.join("workspaceStorage").join("workspace-1");
        let transcript_path = workspace_dir
            .join("GitHub.copilot-chat")
            .join("transcripts")
            .join("session-abc.jsonl");
        std::fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript_path,
            r#"{"type":"session.start","data":{"sessionId":"session-abc"}}"#,
        )
        .unwrap();

        // The chat default in models.json is workspace-wide, not session-specific;
        // it must never be reported as the session's model.
        let models_path = workspace_dir
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

    fn write_chat_session_file(transcript_path: &Path, name: &str, content: &str) {
        let chat_sessions_dir = transcript_path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("chatSessions");
        std::fs::create_dir_all(&chat_sessions_dir).unwrap();
        std::fs::write(chat_sessions_dir.join(name), content).unwrap();
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
    fn test_extract_model_codex_turn_context_model() {
        let path = fixture_path("codex-session-simple.jsonl");
        let result = extract_model(&path, StreamFormat::CodexJsonl, None).unwrap();
        assert_eq!(result, Some("gpt-5-codex".to_string()));
    }

    #[test]
    fn test_extract_model_codex_latest_turn_context_model_wins() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        writeln!(
            file,
            r#"{{"type":"turn_context","payload":{{"model":"initial-model"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"turn_context","payload":{{"model":"switched-model"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let result = extract_model(file.path(), StreamFormat::CodexJsonl, None).unwrap();
        assert_eq!(result, Some("switched-model".to_string()));
    }

    #[test]
    fn test_extract_model_codex_head_skips_oversized_record() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        let oversized_record = serde_json::json!({ "padding": "x".repeat(51_200) });
        writeln!(file, "{oversized_record}").unwrap();
        writeln!(
            file,
            r#"{{"type":"turn_context","payload":{{"model":"model-after-limit"}}}}"#
        )
        .unwrap();
        writeln!(file, "{oversized_record}").unwrap();
        file.flush().unwrap();

        let result = extract_model(file.path(), StreamFormat::CodexJsonl, None).unwrap();
        assert_eq!(result, Some("model-after-limit".to_string()));
    }

    #[test]
    fn test_extract_model_jsonl_head_skips_oversized_record() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        let oversized_record = serde_json::json!({ "padding": "x".repeat(51_200) });
        writeln!(file, "{oversized_record}").unwrap();
        writeln!(file, r#"{{"model":"model-after-limit"}}"#).unwrap();
        writeln!(file, "{oversized_record}").unwrap();
        file.flush().unwrap();

        let result = extract_model(file.path(), StreamFormat::ClaudeJsonl, None).unwrap();
        assert_eq!(result, Some("model-after-limit".to_string()));
    }

    #[test]
    fn test_extract_model_jsonl_bounds_total_head_scan() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        let oversized_record =
            serde_json::json!({ "padding": "x".repeat(MAX_JSONL_HEAD_SCAN_BYTES) });
        writeln!(file, "{oversized_record}").unwrap();
        writeln!(file, r#"{{"model":"model-after-total-limit"}}"#).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({ "padding": "x".repeat(51_200) })
        )
        .unwrap();
        file.flush().unwrap();

        let result = extract_model(file.path(), StreamFormat::ClaudeJsonl, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_codex_rejects_oversized_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let codex_home = dir.path().join(".codex");
        let session_dir = codex_home.join("sessions/2026/06/30");
        std::fs::create_dir_all(&session_dir).unwrap();

        let mut config = String::from("model = \"oversized-config-model\"\n# ");
        config.push_str(&"x".repeat(1024 * 1024));
        std::fs::write(codex_home.join("config.toml"), config).unwrap();

        let transcript = session_dir.join("rollout-test.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"session_meta","payload":{"model":null}}"#,
        )
        .unwrap();

        let result = extract_model(&transcript, StreamFormat::CodexJsonl, None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    #[serial_test::serial]
    fn test_extract_model_codex_config_fallback_respects_codex_home() {
        let dir = tempfile::TempDir::new().unwrap();
        let codex_home = dir.path().join("custom-codex-home");
        let session_dir = codex_home.join("sessions/2026/06/30");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            codex_home.join("config.toml"),
            r#"model = "custom-home-model""#,
        )
        .unwrap();

        let transcript = session_dir.join("rollout-test.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"session_meta","payload":{"model":null}}"#,
        )
        .unwrap();

        let previous_codex_home = std::env::var_os("CODEX_HOME");
        unsafe {
            std::env::set_var("CODEX_HOME", &codex_home);
        }
        let result = extract_model(&transcript, StreamFormat::CodexJsonl, None).unwrap();
        unsafe {
            match previous_codex_home {
                Some(value) => std::env::set_var("CODEX_HOME", value),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }

        assert_eq!(result, Some("custom-home-model".to_string()));
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
    fn test_extract_model_codex_selected_profile_overrides_root_model() {
        let result = extract_codex_model_with_config(
            r#"model = "root-model"
profile = "work"

[profiles.work]
model = "profile-model"
"#,
        );

        assert_eq!(result, Some("profile-model".to_string()));
    }

    #[test]
    fn test_extract_model_codex_selected_profile_can_supply_model() {
        let result = extract_codex_model_with_config(
            r#"profile = "work"

[profiles.work]
model = "profile-only-model"
"#,
        );

        assert_eq!(result, Some("profile-only-model".to_string()));
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
    fn test_extract_model_copilot_vscode_transcript_prefers_otel_over_chat_sessions() {
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
        write_chat_session_file(
            &transcript_path,
            "chat-1.json",
            r#"{"sessionId":"chat-1","inputState":{"selectedModel":{"identifier":"copilot/gpt-4o"}},"requests":[{"result":{"metadata":{"sessionId":"session-abc"}}}]}"#,
        );

        let result = extract_model_from_copilot_vscode_transcript(
            &transcript_path,
            StreamFormat::CopilotEventStreamJsonl,
            "session-abc",
        )
        .unwrap();
        assert_eq!(result, Some("claude-sonnet-4".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_vscode_transcript_falls_back_to_chat_sessions() {
        let (_dir, transcript_path, _otel_db_path) = create_copilot_vscode_workspace();
        write_chat_session_file(
            &transcript_path,
            "chat-1.json",
            r#"{"sessionId":"chat-1","inputState":{"selectedModel":{"identifier":"copilot/claude-sonnet-5"}},"requests":[{"result":{"metadata":{"sessionId":"session-abc"}}}]}"#,
        );

        let result = extract_model_from_copilot_vscode_transcript(
            &transcript_path,
            StreamFormat::CopilotEventStreamJsonl,
            "session-abc",
        )
        .unwrap();
        assert_eq!(result, Some("copilot/claude-sonnet-5".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_vscode_transcript_never_uses_models_json_default() {
        let (_dir, transcript_path, _otel_db_path) = create_copilot_vscode_workspace();

        let result = extract_model_from_copilot_vscode_transcript(
            &transcript_path,
            StreamFormat::CopilotEventStreamJsonl,
            "session-abc",
        )
        .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_model_copilot_chat_sessions_prefers_request_model_over_selected() {
        let (_dir, transcript_path, _otel_db_path) = create_copilot_vscode_workspace();
        write_chat_session_file(
            &transcript_path,
            "chat-1.jsonl",
            concat!(
                r#"{"kind":0,"v":{"sessionId":"chat-1","inputState":{"selectedModel":{"identifier":"copilot/auto"}},"requests":[]}}"#,
                "\n",
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r1","modelId":"copilot/gpt-4o","result":{"metadata":{"sessionId":"session-abc"}}}]}"#,
                "\n",
            ),
        );

        let result = extract_model_from_copilot_vscode_transcript(
            &transcript_path,
            StreamFormat::CopilotEventStreamJsonl,
            "session-abc",
        )
        .unwrap();
        assert_eq!(result, Some("copilot/gpt-4o".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_chat_sessions_ignores_other_sessions() {
        let (_dir, transcript_path, _otel_db_path) = create_copilot_vscode_workspace();
        write_chat_session_file(
            &transcript_path,
            "chat-1.json",
            r#"{"sessionId":"chat-1","inputState":{"selectedModel":{"identifier":"copilot/gpt-4o"}},"requests":[{"result":{"metadata":{"sessionId":"session-other"}}}]}"#,
        );

        let result = extract_model_from_copilot_vscode_transcript(
            &transcript_path,
            StreamFormat::CopilotEventStreamJsonl,
            "session-abc",
        )
        .unwrap();
        assert_eq!(result, None);
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
