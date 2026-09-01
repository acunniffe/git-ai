use crate::authorship::working_log::AgentId;
use crate::streams::sweep::StreamFormat;
use crate::streams::types::StreamError;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_JSONL_SCAN_BYTES: u64 = 50 * 1024;
const MAX_JSONL_HEAD_SCAN_BYTES: usize = 1024 * 1024;
const MAX_JSONL_HEAD_LINES: usize = 20;
const MAX_CODEX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_COPILOT_SESSION_SCAN_BYTES: u64 = 1024 * 1024;
const COPILOT_MODEL_CACHE_CAPACITY: usize = 1024;
const COPILOT_MODEL_RETRY_INTERVAL: Duration = Duration::from_secs(5);

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
        StreamFormat::GrokJsonl => extract_model_from_grok_updates(path),
        // Droid uses extract_model_from_droid_settings() with the settings path instead
        _ => Ok(None),
    }
}

fn extract_model_from_grok_updates(updates_path: &Path) -> Result<Option<String>, StreamError> {
    let (model, _) =
        extract_model_from_jsonl_tail_with(updates_path, extract_model_from_grok_updates_line)?;
    Ok(model)
}

fn extract_model_from_grok_updates_line(line: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let update = json.get("params")?.get("update")?;
    update
        .get("_meta")
        .and_then(|meta| meta.get("modelId"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| update.get("model").and_then(serde_json::Value::as_str))
        .and_then(normalize_model)
        .filter(|model| !model.eq_ignore_ascii_case("unknown"))
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum CopilotModelEvidence {
    Concrete(String),
    Auto,
}

impl CopilotModelEvidence {
    fn from_model(model: &str) -> Option<Self> {
        let model = normalize_model(model)?;
        if model.eq_ignore_ascii_case("copilot/auto") {
            Some(Self::Auto)
        } else if model.eq_ignore_ascii_case("unknown") {
            None
        } else {
            Some(Self::Concrete(model))
        }
    }

    fn model(&self) -> &str {
        match self {
            Self::Concrete(model) => model,
            Self::Auto => "copilot/auto",
        }
    }

    fn is_concrete(&self) -> bool {
        matches!(self, Self::Concrete(_))
    }
}

#[derive(Default)]
struct CopilotModelCandidates {
    latest_request: Option<CopilotModelEvidence>,
    selected: Option<CopilotModelEvidence>,
}

impl CopilotModelCandidates {
    fn record_request(&mut self, request: CopilotRequestModel) {
        let request_model = request
            .model_id
            .as_deref()
            .and_then(CopilotModelEvidence::from_model);
        let resolved_model = request
            .result
            .metadata
            .resolved_model
            .as_deref()
            .and_then(CopilotModelEvidence::from_model);

        let latest_request = match request_model {
            Some(CopilotModelEvidence::Auto) => resolved_model.or(Some(CopilotModelEvidence::Auto)),
            Some(model) => Some(model),
            None => resolved_model,
        };
        if latest_request.is_some() {
            self.latest_request = latest_request;
        }
    }

    fn record_selected(&mut self, model: Option<&str>) {
        if let Some(model) = model.and_then(CopilotModelEvidence::from_model) {
            self.selected = Some(model);
        }
    }

    fn best(self) -> Option<CopilotModelEvidence> {
        self.latest_request.or(self.selected)
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CopilotChatSessionState {
    input_state: CopilotInputState,
    requests: CopilotRequestCandidates,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CopilotInputState {
    selected_model: CopilotSelectedModel,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CopilotSelectedModel {
    identifier: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CopilotRequestModel {
    model_id: Option<String>,
    result: CopilotRequestResult,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CopilotRequestResult {
    metadata: CopilotRequestMetadata,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CopilotRequestMetadata {
    resolved_model: Option<String>,
}

#[derive(Default)]
struct CopilotRequestCandidates {
    latest: Option<CopilotModelEvidence>,
}

impl<'de> Deserialize<'de> for CopilotRequestCandidates {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct RequestVisitor;

        impl<'de> serde::de::Visitor<'de> for RequestVisitor {
            type Value = CopilotRequestCandidates;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a Copilot request array")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut candidates = CopilotModelCandidates::default();
                while let Some(request) = sequence.next_element::<CopilotRequestModel>()? {
                    candidates.record_request(request);
                }
                Ok(CopilotRequestCandidates {
                    latest: candidates.latest_request,
                })
            }
        }

        deserializer.deserialize_seq(RequestVisitor)
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CopilotPatchHeader {
    kind: Option<u8>,
    k: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct CopilotPatchValue<T> {
    v: T,
}

fn collect_copilot_session_state(
    state: CopilotChatSessionState,
    candidates: &mut CopilotModelCandidates,
) {
    candidates.record_selected(state.input_state.selected_model.identifier.as_deref());
    if state.requests.latest.is_some() {
        candidates.latest_request = state.requests.latest;
    }
}

fn copilot_patch_path_matches(path: &[serde_json::Value], expected: &[&str]) -> bool {
    path.len() == expected.len()
        && path
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.as_str() == Some(*expected))
}

fn collect_copilot_jsonl_line(line: &str, candidates: &mut CopilotModelCandidates) {
    let Ok(header) = serde_json::from_str::<CopilotPatchHeader>(line) else {
        return;
    };

    match header.kind {
        Some(0) => {
            if let Ok(patch) =
                serde_json::from_str::<CopilotPatchValue<CopilotChatSessionState>>(line)
            {
                collect_copilot_session_state(patch.v, candidates);
            }
        }
        Some(2) if copilot_patch_path_matches(&header.k, &["requests"]) => {
            if let Ok(patch) =
                serde_json::from_str::<CopilotPatchValue<CopilotRequestCandidates>>(line)
                && patch.v.latest.is_some()
            {
                candidates.latest_request = patch.v.latest;
            }
        }
        Some(1)
            if copilot_patch_path_matches(
                &header.k,
                &["inputState", "selectedModel", "identifier"],
            ) =>
        {
            if let Ok(patch) = serde_json::from_str::<CopilotPatchValue<String>>(line) {
                candidates.record_selected(Some(&patch.v));
            }
        }
        Some(1) if copilot_patch_path_matches(&header.k, &["inputState", "selectedModel"]) => {
            if let Ok(patch) = serde_json::from_str::<CopilotPatchValue<CopilotSelectedModel>>(line)
            {
                candidates.record_selected(patch.v.identifier.as_deref());
            }
        }
        Some(1) if header.k.last().and_then(serde_json::Value::as_str) == Some("modelId") => {
            if let Ok(patch) = serde_json::from_str::<CopilotPatchValue<String>>(line) {
                candidates.record_request(CopilotRequestModel {
                    model_id: Some(patch.v),
                    ..Default::default()
                });
            }
        }
        Some(1) if header.k.last().and_then(serde_json::Value::as_str) == Some("resolvedModel") => {
            if let Ok(patch) = serde_json::from_str::<CopilotPatchValue<String>>(line) {
                candidates.record_request(CopilotRequestModel {
                    result: CopilotRequestResult {
                        metadata: CopilotRequestMetadata {
                            resolved_model: Some(patch.v),
                        },
                    },
                    ..Default::default()
                });
            }
        }
        None => {
            if let Ok(state) = serde_json::from_str::<CopilotChatSessionState>(line) {
                collect_copilot_session_state(state, candidates);
            }
        }
        _ => {}
    }
}

fn extract_model_from_copilot_chat_session(
    path: &Path,
) -> Result<Option<CopilotModelEvidence>, StreamError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let mut candidates = CopilotModelCandidates::default();

    if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
        if let Ok(state) =
            serde_json::from_reader::<_, CopilotChatSessionState>(BufReader::new(file))
        {
            collect_copilot_session_state(state, &mut candidates);
        }
        return Ok(candidates.best());
    }

    let file_size = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return Ok(None),
    };
    let read_size = file_size.min(MAX_COPILOT_SESSION_SCAN_BYTES);
    let seek_pos = file_size.saturating_sub(read_size);
    if file.seek(SeekFrom::Start(seek_pos)).is_err() {
        return Ok(None);
    }
    let mut tail = Vec::with_capacity(read_size as usize);
    if file.take(read_size).read_to_end(&mut tail).is_err() {
        return Ok(None);
    }
    let tail = if seek_pos > 0 {
        let Some(first_newline) = tail.iter().position(|byte| *byte == b'\n') else {
            return Ok(None);
        };
        &tail[first_newline + 1..]
    } else {
        &tail
    };
    let Ok(tail) = std::str::from_utf8(tail) else {
        return Ok(None);
    };
    for line in tail.lines() {
        let line = line.trim();
        if !line.is_empty() {
            collect_copilot_jsonl_line(line, &mut candidates);
        }
    }

    Ok(candidates.best())
}

fn copilot_chat_session_paths(
    stream_path: &Path,
    chat_session_id: &str,
) -> Option<(PathBuf, PathBuf)> {
    let session_component = Path::new(chat_session_id);
    if chat_session_id.is_empty()
        || session_component.file_name().and_then(|name| name.to_str()) != Some(chat_session_id)
    {
        return None;
    }

    let transcripts_dir = stream_path.parent()?;
    if !transcripts_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("transcripts"))
    {
        return None;
    }
    let copilot_dir = transcripts_dir.parent()?;
    if !copilot_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("github.copilot-chat"))
    {
        return None;
    }

    let chat_sessions_dir = copilot_dir.parent()?.join("chatSessions");
    Some((
        chat_sessions_dir.join(format!("{chat_session_id}.jsonl")),
        chat_sessions_dir.join(format!("{chat_session_id}.json")),
    ))
}

fn load_copilot_vscode_model(
    stream_path: &Path,
    format: StreamFormat,
    chat_session_id: &str,
) -> Result<Option<CopilotModelEvidence>, StreamError> {
    let session_evidence = if let Some((jsonl_path, json_path)) =
        copilot_chat_session_paths(stream_path, chat_session_id)
    {
        extract_model_from_copilot_chat_session(&jsonl_path)?
            .or(extract_model_from_copilot_chat_session(&json_path)?)
    } else {
        None
    };

    if session_evidence
        .as_ref()
        .is_some_and(CopilotModelEvidence::is_concrete)
    {
        return Ok(session_evidence);
    }

    if let Some(model) = extract_model(stream_path, format, None)?
        .as_deref()
        .and_then(CopilotModelEvidence::from_model)
        && model.is_concrete()
    {
        return Ok(Some(model));
    }

    if let Some(model) =
        extract_model_from_copilot_otel_for_transcript(stream_path, chat_session_id)?
            .as_deref()
            .and_then(CopilotModelEvidence::from_model)
        && model.is_concrete()
    {
        return Ok(Some(model));
    }

    Ok(session_evidence)
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct CopilotModelCacheKey {
    chat_session_id: String,
    stream_path: PathBuf,
}

#[derive(Default)]
struct CopilotModelCacheEntry {
    concrete_model: Option<String>,
    retryable_model: Option<String>,
    retry_after: Option<Instant>,
}

impl CopilotModelCacheEntry {
    fn cached_model(&self, now: Instant) -> Option<Option<String>> {
        if let Some(model) = &self.concrete_model {
            return Some(Some(model.clone()));
        }
        self.retry_after
            .filter(|retry_after| *retry_after > now)
            .map(|_| self.retryable_model.clone())
    }

    fn store(&mut self, model: Option<CopilotModelEvidence>, now: Instant) -> Option<String> {
        match model {
            Some(CopilotModelEvidence::Concrete(model)) => {
                self.concrete_model = Some(model.clone());
                self.retryable_model = None;
                self.retry_after = None;
                Some(model)
            }
            retryable => {
                let model = retryable.map(|model| model.model().to_string());
                self.retryable_model = model.clone();
                self.retry_after = Some(now + COPILOT_MODEL_RETRY_INTERVAL);
                model
            }
        }
    }
}

struct CopilotModelCache {
    capacity: usize,
    entries: HashMap<CopilotModelCacheKey, Arc<Mutex<CopilotModelCacheEntry>>>,
    insertion_order: VecDeque<CopilotModelCacheKey>,
}

impl CopilotModelCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::with_capacity(capacity),
            insertion_order: VecDeque::with_capacity(capacity),
        }
    }

    fn entry(&mut self, key: CopilotModelCacheKey) -> Arc<Mutex<CopilotModelCacheEntry>> {
        if let Some(entry) = self.entries.get(&key) {
            return Arc::clone(entry);
        }

        while self.entries.len() >= self.capacity.max(1) {
            let Some(oldest) = self.insertion_order.pop_front() else {
                self.entries.clear();
                break;
            };
            self.entries.remove(&oldest);
        }

        let entry = Arc::new(Mutex::new(CopilotModelCacheEntry::default()));
        self.entries.insert(key.clone(), Arc::clone(&entry));
        self.insertion_order.push_back(key);
        entry
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

static COPILOT_MODEL_CACHE: OnceLock<Mutex<CopilotModelCache>> = OnceLock::new();

fn copilot_model_cache_entry(
    stream_path: &Path,
    chat_session_id: &str,
) -> Arc<Mutex<CopilotModelCacheEntry>> {
    let key = CopilotModelCacheKey {
        chat_session_id: chat_session_id.to_string(),
        stream_path: stream_path.to_path_buf(),
    };
    let cache = COPILOT_MODEL_CACHE
        .get_or_init(|| Mutex::new(CopilotModelCache::new(COPILOT_MODEL_CACHE_CAPACITY)));
    cache.lock().map_or_else(
        |_| Arc::new(Mutex::new(CopilotModelCacheEntry::default())),
        |mut cache| cache.entry(key),
    )
}

fn copilot_vscode_transcript_format(stream_path: &Path) -> StreamFormat {
    let path = stream_path.to_string_lossy();
    if stream_path
        .extension()
        .and_then(|extension| extension.to_str())
        == Some("json")
    {
        StreamFormat::CopilotSessionJson
    } else if stream_path
        .extension()
        .and_then(|extension| extension.to_str())
        == Some("jsonl")
        || path.contains("/workspaceStorage/")
        || path.contains("\\workspaceStorage\\")
    {
        StreamFormat::CopilotEventStreamJsonl
    } else {
        StreamFormat::CopilotSessionJson
    }
}

pub(crate) fn extract_cached_copilot_vscode_model(
    stream_path: &Path,
    chat_session_id: &str,
) -> Result<Option<String>, StreamError> {
    let entry = copilot_model_cache_entry(stream_path, chat_session_id);
    let format = copilot_vscode_transcript_format(stream_path);
    let Ok(mut entry) = entry.lock() else {
        return load_copilot_vscode_model(stream_path, format, chat_session_id)
            .map(|model| model.map(|model| model.model().to_string()));
    };
    let now = Instant::now();
    if let Some(model) = entry.cached_model(now) {
        return Ok(model);
    }

    let model = load_copilot_vscode_model(stream_path, format, chat_session_id)?;
    Ok(entry.store(model, now))
}

pub(crate) fn enrich_copilot_agent_model(
    agent_id: &mut AgentId,
    metadata: &HashMap<String, String>,
) {
    if agent_id.tool != "github-copilot"
        || !matches!(
            agent_id.model.trim().to_ascii_lowercase().as_str(),
            "" | "unknown" | "copilot/auto"
        )
    {
        return;
    }
    let Some(stream_path) = metadata
        .get("transcript_path")
        .or_else(|| metadata.get("chat_session_path"))
    else {
        return;
    };
    if let Ok(Some(model)) =
        extract_cached_copilot_vscode_model(Path::new(stream_path), &agent_id.id)
    {
        agent_id.model = model;
    }
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
    load_copilot_vscode_model(stream_path, format, chat_session_id)
        .map(|model| model.map(|model| model.model().to_string()))
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
    extract_model_from_copilot_chat_session(path)
        .map(|model| model.map(|model| model.model().to_string()))
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
    fn test_extract_model_grok_updates() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("updates.jsonl");
        std::fs::write(
            &path,
            [
                r#"{"params":{"update":{"_meta":{"modelId":"grok-4.5"}}}}"#,
                r#"{"params":{"update":{"content":{"text":"hi"},"_meta":{"modelId":"grok-4.6"}}}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let result = extract_model(&path, StreamFormat::GrokJsonl, None).unwrap();
        assert_eq!(result, Some("grok-4.6".to_string()));
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
    fn test_extract_model_copilot_vscode_transcript_ignores_models_json_default() {
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
    fn test_extract_model_copilot_vscode_transcript_reads_request_patch() {
        let (_dir, transcript_path, _otel_db_path) = create_copilot_vscode_workspace();
        let (chat_session_path, _) =
            copilot_chat_session_paths(&transcript_path, "session-abc").unwrap();
        std::fs::create_dir_all(chat_session_path.parent().unwrap()).unwrap();
        std::fs::write(
            chat_session_path,
            concat!(
                r#"{"kind":0,"v":{"inputState":{"selectedModel":{"identifier":"copilot/auto"}},"requests":[]}}"#,
                "\n",
                r#"{"kind":2,"k":["requests"],"v":[{"modelId":"copilot/claude-sonnet-5","result":{"details":"GPT-5.3 Codex"}}]}"#,
                "\n"
            ),
        )
        .unwrap();

        let result = extract_model_from_copilot_vscode_transcript(
            &transcript_path,
            StreamFormat::CopilotEventStreamJsonl,
            "session-abc",
        )
        .unwrap();
        assert_eq!(result, Some("copilot/claude-sonnet-5".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_vscode_keeps_model_before_empty_request() {
        let (_dir, transcript_path, _otel_db_path) = create_copilot_vscode_workspace();
        let (chat_session_path, _) =
            copilot_chat_session_paths(&transcript_path, "session-abc").unwrap();
        std::fs::create_dir_all(chat_session_path.parent().unwrap()).unwrap();
        std::fs::write(
            chat_session_path,
            concat!(
                r#"{"kind":0,"v":{"inputState":{"selectedModel":{"identifier":"copilot/auto"}},"requests":[]}}"#,
                "\n",
                r#"{"kind":2,"k":["requests"],"v":[{"modelId":"copilot/claude-sonnet-5"}]}"#,
                "\n",
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"pending-request"}]}"#,
                "\n"
            ),
        )
        .unwrap();

        let result = extract_model_from_copilot_vscode_transcript(
            &transcript_path,
            StreamFormat::CopilotEventStreamJsonl,
            "session-abc",
        )
        .unwrap();
        assert_eq!(result, Some("copilot/claude-sonnet-5".to_string()));
    }

    #[test]
    fn test_cached_copilot_vscode_model_reads_legacy_json_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir
            .path()
            .join("workspaceStorage/workspace-id/chatSessions/legacy-session.json");
        std::fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript_path,
            r#"{
                "requests": [
                    {"modelId": "copilot/claude-sonnet-5"}
                ]
            }"#,
        )
        .unwrap();

        let result =
            extract_cached_copilot_vscode_model(&transcript_path, "legacy-session").unwrap();
        assert_eq!(result, Some("copilot/claude-sonnet-5".to_string()));
    }

    #[test]
    fn test_copilot_chat_session_json_streams_large_document() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("oversized-session.json");
        std::fs::write(
            &transcript_path,
            serde_json::json!({
                "padding": "x".repeat(MAX_JSONL_HEAD_SCAN_BYTES),
                "requests": [{"modelId": "copilot/claude-sonnet-5"}]
            })
            .to_string(),
        )
        .unwrap();

        let result = extract_model_from_copilot_chat_session(&transcript_path).unwrap();
        assert_eq!(
            result,
            Some(CopilotModelEvidence::Concrete(
                "copilot/claude-sonnet-5".to_string()
            ))
        );
    }

    #[test]
    fn test_copilot_chat_session_jsonl_skips_oversized_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("large-session.jsonl");
        let oversized_snapshot = serde_json::json!({
            "kind": 0,
            "v": {"padding": "x".repeat(MAX_COPILOT_SESSION_SCAN_BYTES as usize)}
        });
        let request = serde_json::json!({
            "kind": 2,
            "k": ["requests"],
            "v": [{"modelId": "copilot/claude-sonnet-5"}]
        });
        std::fs::write(
            &transcript_path,
            format!("{oversized_snapshot}\n{request}\n"),
        )
        .unwrap();

        let result = extract_model_from_copilot_chat_session(&transcript_path).unwrap();
        assert_eq!(
            result,
            Some(CopilotModelEvidence::Concrete(
                "copilot/claude-sonnet-5".to_string()
            ))
        );
    }

    #[test]
    fn test_copilot_chat_session_jsonl_handles_utf8_at_tail_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("unicode-session.jsonl");
        let request = format!(
            "{}\n",
            serde_json::json!({
                "kind": 2,
                "k": ["requests"],
                "v": [{"modelId": "copilot/claude-sonnet-5"}]
            })
        );
        let oversized_line_len = MAX_COPILOT_SESSION_SCAN_BYTES as usize + 10 - request.len();
        let mut oversized_line = vec![b'x'; oversized_line_len];
        oversized_line[8..12].copy_from_slice("😀".as_bytes());
        oversized_line.push(b'\n');
        oversized_line.extend_from_slice(request.as_bytes());
        std::fs::write(&transcript_path, oversized_line).unwrap();

        let result = extract_model_from_copilot_chat_session(&transcript_path).unwrap();
        assert_eq!(
            result,
            Some(CopilotModelEvidence::Concrete(
                "copilot/claude-sonnet-5".to_string()
            ))
        );
    }

    #[test]
    fn test_extract_model_copilot_vscode_auto_uses_resolved_model() {
        let (_dir, transcript_path, _otel_db_path) = create_copilot_vscode_workspace();
        let (chat_session_path, _) =
            copilot_chat_session_paths(&transcript_path, "session-abc").unwrap();
        std::fs::create_dir_all(chat_session_path.parent().unwrap()).unwrap();
        std::fs::write(
            chat_session_path,
            concat!(
                r#"{"kind":0,"v":{"inputState":{"selectedModel":{"identifier":"copilot/auto"}},"requests":[]}}"#,
                "\n",
                r#"{"kind":2,"k":["requests"],"v":[{"modelId":"copilot/auto","result":{"metadata":{"resolvedModel":"claude-sonnet-5"}}}]}"#,
                "\n"
            ),
        )
        .unwrap();

        let result = extract_model_from_copilot_vscode_transcript(
            &transcript_path,
            StreamFormat::CopilotEventStreamJsonl,
            "session-abc",
        )
        .unwrap();
        assert_eq!(result, Some("claude-sonnet-5".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_vscode_supports_plain_json_state() {
        let (_dir, transcript_path, _otel_db_path) = create_copilot_vscode_workspace();
        let (_, chat_session_path) =
            copilot_chat_session_paths(&transcript_path, "session-abc").unwrap();
        std::fs::create_dir_all(chat_session_path.parent().unwrap()).unwrap();
        std::fs::write(
            chat_session_path,
            r#"{"requests":[{"modelId":"copilot/claude-sonnet-5"}]}"#,
        )
        .unwrap();

        let result = extract_model_from_copilot_vscode_transcript(
            &transcript_path,
            StreamFormat::CopilotEventStreamJsonl,
            "session-abc",
        )
        .unwrap();
        assert_eq!(result, Some("copilot/claude-sonnet-5".to_string()));
    }

    #[test]
    fn test_extract_model_copilot_vscode_uses_exact_session_file() {
        let (_dir, transcript_path, _otel_db_path) = create_copilot_vscode_workspace();
        let (chat_session_path, _) =
            copilot_chat_session_paths(&transcript_path, "other-session").unwrap();
        std::fs::create_dir_all(chat_session_path.parent().unwrap()).unwrap();
        std::fs::write(
            chat_session_path,
            r#"{"inputState":{"selectedModel":{"identifier":"copilot/claude-sonnet-5"}}}"#,
        )
        .unwrap();

        let result = extract_model_from_copilot_vscode_transcript(
            &transcript_path,
            StreamFormat::CopilotEventStreamJsonl,
            "session-abc",
        )
        .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_copilot_model_cache_bounds_entries() {
        let key = |session: &str| CopilotModelCacheKey {
            chat_session_id: session.to_string(),
            stream_path: PathBuf::from(format!("/{session}.jsonl")),
        };
        let mut cache = CopilotModelCache::new(2);
        cache.entry(key("one"));
        cache.entry(key("two"));
        cache.entry(key("three"));

        assert_eq!(cache.len(), 2);
        assert!(!cache.entries.contains_key(&key("one")));
        assert!(cache.entries.contains_key(&key("two")));
        assert!(cache.entries.contains_key(&key("three")));
    }

    #[test]
    fn test_copilot_model_cache_retries_non_concrete_results() {
        let now = Instant::now();
        let mut entry = CopilotModelCacheEntry::default();
        assert_eq!(
            entry.store(Some(CopilotModelEvidence::Auto), now),
            Some("copilot/auto".to_string())
        );
        assert_eq!(
            entry.cached_model(now + Duration::from_secs(4)),
            Some(Some("copilot/auto".to_string()))
        );
        assert_eq!(entry.cached_model(now + Duration::from_secs(5)), None);

        entry.store(
            Some(CopilotModelEvidence::Concrete(
                "copilot/claude-sonnet-5".to_string(),
            )),
            now + Duration::from_secs(5),
        );
        assert_eq!(
            entry.cached_model(now + Duration::from_secs(500)),
            Some(Some("copilot/claude-sonnet-5".to_string()))
        );
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
