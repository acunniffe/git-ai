use crate::repos::test_repo::TestRepo;
use git_ai::metrics::{EventAttributes, MetricEvent, PosEncoded, SessionEventValues};
use git_ai::repo_url::resolve_repo_url_from_path;
use git_ai::streams::agent::Agent;
use git_ai::streams::agents::ClaudeAgent;
use git_ai::streams::watermark::ByteOffsetWatermark;
use git_ai::streams::{StreamRecord, StreamsDatabase};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// === Helper functions ===

fn write_claude_transcript_with_cwd(path: &Path, cwd: &str) {
    let events = [
        json!({"type":"user","message":{"content":"hello","role":"user"},
               "uuid":"u1","parentUuid":null,"cwd":cwd,"sessionId":"test-sess",
               "timestamp":"2026-05-01T00:00:00Z"}),
        json!({"type":"assistant","message":{"model":"claude-opus-4-6","id":"msg1","role":"assistant",
               "content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn",
               "usage":{"input_tokens":1,"output_tokens":1}},
               "uuid":"a1","parentUuid":"u1","cwd":cwd,"sessionId":"test-sess",
               "timestamp":"2026-05-01T00:00:01Z"}),
    ];
    let content: String = events.iter().map(|e| format!("{}\n", e)).collect();
    fs::write(path, content).unwrap();
}

fn write_claude_transcript_without_cwd(path: &Path) {
    let events = [
        json!({"type":"user","message":{"content":"hello","role":"user"},
               "uuid":"u1","parentUuid":null,"sessionId":"test-sess",
               "timestamp":"2026-05-01T00:00:00Z"}),
        json!({"type":"assistant","message":{"model":"claude-opus-4-6","id":"msg1","role":"assistant",
               "content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn",
               "usage":{"input_tokens":1,"output_tokens":1}},
               "uuid":"a1","parentUuid":"u1","sessionId":"test-sess",
               "timestamp":"2026-05-01T00:00:01Z"}),
    ];
    let content: String = events.iter().map(|e| format!("{}\n", e)).collect();
    fs::write(path, content).unwrap();
}

fn write_claude_transcript_cwd_on_later_line(path: &Path, cwd: &str) {
    let events = [
        json!({"type":"last-prompt","leafUuid":"abc","sessionId":"test-sess"}),
        json!({"type":"permission-mode","permissionMode":"default","sessionId":"test-sess"}),
        json!({"type":"user","message":{"content":"hello","role":"user"},
               "uuid":"u1","parentUuid":null,"cwd":cwd,"sessionId":"test-sess",
               "timestamp":"2026-05-01T00:00:00Z"}),
    ];
    let content: String = events.iter().map(|e| format!("{}\n", e)).collect();
    fs::write(path, content).unwrap();
}

fn setup_test_db(dir: &Path) -> StreamsDatabase {
    let db_path = dir.join("transcripts.db");
    StreamsDatabase::open(&db_path).unwrap()
}

fn make_stream_record(
    session_id: &str,
    tool: &str,
    stream_path: &Path,
    repo_work_dir: Option<&str>,
) -> StreamRecord {
    StreamRecord {
        session_id: session_id.to_string(),
        stream_kind: "transcript".to_string(),
        tool: tool.to_string(),
        stream_path: stream_path.display().to_string(),
        stream_format: "ClaudeJsonl".to_string(),
        watermark_type: "ByteOffset".to_string(),
        watermark_value: "0".to_string(),
        external_session_id: format!("ext-{}", session_id),
        external_parent_session_id: None,
        first_seen_at: chrono::Utc::now().timestamp(),
        last_processed_at: 0,
        last_known_size: 0,
        last_modified: None,
        processing_errors: 0,
        last_error: None,
        repo_work_dir: repo_work_dir.map(|s| s.to_string()),
    }
}

// === Test Group 1: resolve_repo_url_from_path utility ===

#[test]
fn test_resolve_repo_url_from_path_ssh_remote() {
    let repo = TestRepo::new();
    repo.git(&["remote", "add", "origin", "git@github.com:org/project.git"])
        .unwrap();
    let result = resolve_repo_url_from_path(repo.path());
    assert_eq!(
        result,
        Some("https://github.com/org/project".to_string()),
        "SSH remote must be normalized to HTTPS"
    );
}

#[test]
fn test_resolve_repo_url_from_path_https_remote() {
    let repo = TestRepo::new();
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/org/project.git",
    ])
    .unwrap();
    let result = resolve_repo_url_from_path(repo.path());
    assert_eq!(
        result,
        Some("https://github.com/org/project".to_string()),
        "HTTPS remote must strip .git suffix"
    );
}

#[test]
fn test_resolve_repo_url_from_path_no_remote() {
    let repo = TestRepo::new();
    let result = resolve_repo_url_from_path(repo.path());
    assert_eq!(result, None, "Must return None when repo has no remote");
}

#[test]
fn test_resolve_repo_url_from_path_not_a_repo() {
    let temp_dir = TempDir::new().unwrap();
    let result = resolve_repo_url_from_path(temp_dir.path());
    assert_eq!(result, None, "Must return None for non-repo directory");
}

#[test]
fn test_resolve_repo_url_from_path_strips_credentials() {
    let repo = TestRepo::new();
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://user:token@github.com/org/project.git",
    ])
    .unwrap();
    let result = resolve_repo_url_from_path(repo.path());
    assert_eq!(
        result,
        Some("https://github.com/org/project".to_string()),
        "Credentials must be stripped from repo_url"
    );
}

#[test]
fn test_resolve_repo_url_from_path_from_subdirectory() {
    let repo = TestRepo::new();
    repo.git(&["remote", "add", "origin", "git@github.com:org/project.git"])
        .unwrap();
    let subdir = repo.path().join("src/deep/nested");
    fs::create_dir_all(&subdir).unwrap();
    let result = resolve_repo_url_from_path(&subdir);
    assert_eq!(
        result,
        Some("https://github.com/org/project".to_string()),
        "Must resolve repo_url from subdirectory"
    );
}

// === Test Group 2: Claude infer_cwd ===

#[test]
fn test_claude_infer_cwd_from_user_event() {
    let temp_dir = TempDir::new().unwrap();
    let transcript = temp_dir.path().join("session.jsonl");
    write_claude_transcript_with_cwd(&transcript, "/Users/dev/my-project");
    let agent = ClaudeAgent::new();
    let result = agent.infer_cwd(&transcript);
    assert_eq!(
        result,
        Some(PathBuf::from("/Users/dev/my-project")),
        "Must extract cwd from Claude transcript user event"
    );
}

#[test]
fn test_claude_infer_cwd_no_cwd_in_transcript() {
    let temp_dir = TempDir::new().unwrap();
    let transcript = temp_dir.path().join("session.jsonl");
    write_claude_transcript_without_cwd(&transcript);
    let agent = ClaudeAgent::new();
    let result = agent.infer_cwd(&transcript);
    assert_eq!(
        result, None,
        "Must return None when transcript has no cwd field"
    );
}

#[test]
fn test_claude_infer_cwd_empty_file() {
    let temp_dir = TempDir::new().unwrap();
    let transcript = temp_dir.path().join("session.jsonl");
    fs::write(&transcript, "").unwrap();
    let agent = ClaudeAgent::new();
    let result = agent.infer_cwd(&transcript);
    assert_eq!(result, None, "Must return None for empty transcript file");
}

#[test]
fn test_claude_infer_cwd_missing_file() {
    let agent = ClaudeAgent::new();
    let result = agent.infer_cwd(Path::new("/nonexistent/path/session.jsonl"));
    assert_eq!(
        result, None,
        "Must return None for non-existent file without panicking"
    );
}

#[test]
fn test_claude_infer_cwd_not_on_first_line() {
    let temp_dir = TempDir::new().unwrap();
    let transcript = temp_dir.path().join("session.jsonl");
    write_claude_transcript_cwd_on_later_line(&transcript, "/home/user/repo");
    let agent = ClaudeAgent::new();
    let result = agent.infer_cwd(&transcript);
    assert_eq!(
        result,
        Some(PathBuf::from("/home/user/repo")),
        "Must find cwd even when first event lacks it"
    );
}

// === Test Group 3: DB schema and repo_work_dir persistence ===

#[test]
fn test_db_new_schema_has_repo_work_dir() {
    let temp_dir = TempDir::new().unwrap();
    let db = setup_test_db(temp_dir.path());
    let transcript = temp_dir.path().join("t.jsonl");
    fs::write(&transcript, "").unwrap();

    let record = make_stream_record("test-1", "claude", &transcript, Some("/Users/dev/project"));
    db.insert_stream(&record).unwrap();
    let retrieved = db
        .get_stream("test-1", "transcript", &transcript.display().to_string())
        .unwrap()
        .unwrap();
    assert_eq!(
        retrieved.repo_work_dir,
        Some("/Users/dev/project".to_string()),
        "repo_work_dir must round-trip through insert/get"
    );
}

#[test]
fn test_db_insert_session_without_repo_work_dir() {
    let temp_dir = TempDir::new().unwrap();
    let db = setup_test_db(temp_dir.path());
    let transcript = temp_dir.path().join("t.jsonl");
    fs::write(&transcript, "").unwrap();

    let record = make_stream_record("test-2", "claude", &transcript, None);
    db.insert_stream(&record).unwrap();
    let retrieved = db
        .get_stream("test-2", "transcript", &transcript.display().to_string())
        .unwrap()
        .unwrap();
    assert_eq!(
        retrieved.repo_work_dir, None,
        "repo_work_dir must be None when not provided"
    );
}

#[test]
fn test_db_update_repo_work_dir() {
    let temp_dir = TempDir::new().unwrap();
    let db = setup_test_db(temp_dir.path());
    let transcript = temp_dir.path().join("t.jsonl");
    fs::write(&transcript, "").unwrap();

    let record = make_stream_record("test-3", "claude", &transcript, None);
    db.insert_stream(&record).unwrap();

    db.update_repo_work_dir(
        "test-3",
        "transcript",
        &transcript.display().to_string(),
        "/Users/dev/my-project",
    )
    .unwrap();
    let retrieved = db
        .get_stream("test-3", "transcript", &transcript.display().to_string())
        .unwrap()
        .unwrap();
    assert_eq!(
        retrieved.repo_work_dir,
        Some("/Users/dev/my-project".to_string()),
        "update_repo_work_dir must set the column"
    );
}

// === Test Group 4: Session Event repo_url from Hook Path ===

#[test]
fn test_session_events_include_repo_url_from_hook_triggered_checkpoint() {
    let repo = TestRepo::new();
    repo.git(&["remote", "add", "origin", "git@github.com:acme/app.git"])
        .unwrap();

    let temp_dir = TempDir::new().unwrap();
    let transcript = temp_dir.path().join("session.jsonl");
    write_claude_transcript_with_cwd(&transcript, repo.path().to_str().unwrap());

    let db = setup_test_db(temp_dir.path());
    let record = make_stream_record(
        "test-hook-sess",
        "claude",
        &transcript,
        Some(repo.path().to_str().unwrap()),
    );
    db.insert_stream(&record).unwrap();

    let session = db
        .get_stream(
            "test-hook-sess",
            "transcript",
            &transcript.display().to_string(),
        )
        .unwrap()
        .unwrap();
    let repo_work_dir = session.repo_work_dir.as_ref().map(PathBuf::from);
    let resolved_repo_url = repo_work_dir
        .as_ref()
        .and_then(|p| resolve_repo_url_from_path(p));

    assert_eq!(
        resolved_repo_url,
        Some("https://github.com/acme/app".to_string()),
        "Session events from hook-triggered checkpoints MUST include repo_url"
    );

    // Build attrs and verify
    let mut base_attrs = EventAttributes::with_version("test")
        .session_id(session.session_id.clone())
        .tool(&session.tool)
        .external_session_id(session.external_session_id.clone());
    if let Some(url) = &resolved_repo_url {
        base_attrs = base_attrs.repo_url(url.clone());
    }
    let sparse = base_attrs.to_sparse();
    let attrs = EventAttributes::from_sparse(&sparse);

    assert_eq!(
        attrs.repo_url,
        Some(Some("https://github.com/acme/app".to_string())),
        "EventAttributes must carry repo_url through sparse encoding"
    );
    assert_eq!(attrs.tool, Some(Some("claude".to_string())));
    assert_eq!(attrs.session_id, Some(Some("test-hook-sess".to_string())));
}

#[test]
fn test_session_events_no_repo_url_when_no_remote() {
    let repo = TestRepo::new();

    let temp_dir = TempDir::new().unwrap();
    let transcript = temp_dir.path().join("session.jsonl");
    write_claude_transcript_with_cwd(&transcript, repo.path().to_str().unwrap());

    let db = setup_test_db(temp_dir.path());
    let record = make_stream_record(
        "test-no-remote",
        "claude",
        &transcript,
        Some(repo.path().to_str().unwrap()),
    );
    db.insert_stream(&record).unwrap();

    let session = db
        .get_stream(
            "test-no-remote",
            "transcript",
            &transcript.display().to_string(),
        )
        .unwrap()
        .unwrap();
    let repo_work_dir = session.repo_work_dir.as_ref().map(PathBuf::from);
    let resolved = repo_work_dir
        .as_ref()
        .and_then(|p| resolve_repo_url_from_path(p));

    assert_eq!(
        resolved, None,
        "repo_url must be None when repo has no remote"
    );
}

#[test]
fn test_session_events_no_repo_url_when_no_work_dir() {
    let temp_dir = TempDir::new().unwrap();
    let transcript = temp_dir.path().join("session.jsonl");
    write_claude_transcript_without_cwd(&transcript);

    let db = setup_test_db(temp_dir.path());
    let record = make_stream_record("test-no-workdir", "claude", &transcript, None);
    db.insert_stream(&record).unwrap();

    let session = db
        .get_stream(
            "test-no-workdir",
            "transcript",
            &transcript.display().to_string(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(session.repo_work_dir, None);

    let resolved = None::<PathBuf>
        .as_ref()
        .and_then(|p: &PathBuf| resolve_repo_url_from_path(p));
    assert_eq!(
        resolved, None,
        "repo_url must be None when no work_dir and no cwd inference"
    );
}

// === Test Group 5: Session Event repo_url from Sweep Path (cwd inference) ===

#[test]
fn test_session_events_repo_url_from_sweep_inferred_cwd() {
    let repo = TestRepo::new();
    repo.git(&["remote", "add", "origin", "git@github.com:team/service.git"])
        .unwrap();

    let transcript = repo.path().join("transcript.jsonl");
    write_claude_transcript_with_cwd(&transcript, repo.path().to_str().unwrap());

    let temp_dir = TempDir::new().unwrap();
    let db = setup_test_db(temp_dir.path());
    let record = make_stream_record("test-sweep-sess", "claude", &transcript, None);
    db.insert_stream(&record).unwrap();

    let session = db
        .get_stream(
            "test-sweep-sess",
            "transcript",
            &transcript.display().to_string(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        session.repo_work_dir, None,
        "Sweep should not have repo_work_dir initially"
    );

    let agent = ClaudeAgent::new();
    let inferred_cwd = agent.infer_cwd(&PathBuf::from(&session.stream_path));
    assert_eq!(
        inferred_cwd,
        Some(repo.path().to_path_buf()),
        "Claude agent must infer cwd from transcript"
    );

    let resolved = inferred_cwd
        .as_ref()
        .and_then(|p| resolve_repo_url_from_path(p));
    assert_eq!(
        resolved,
        Some("https://github.com/team/service".to_string()),
        "Sweep-discovered sessions MUST resolve repo_url when cwd is inferable"
    );
}

#[test]
fn test_inferred_cwd_persisted_to_db() {
    let repo = TestRepo::new();
    repo.git(&["remote", "add", "origin", "git@github.com:a/b.git"])
        .unwrap();

    let transcript = repo.path().join("session.jsonl");
    write_claude_transcript_with_cwd(&transcript, repo.path().to_str().unwrap());

    let temp_dir = TempDir::new().unwrap();
    let db = setup_test_db(temp_dir.path());
    let record = make_stream_record("test-persist", "claude", &transcript, None);
    db.insert_stream(&record).unwrap();

    let agent = ClaudeAgent::new();
    let inferred = agent.infer_cwd(&transcript).unwrap();
    db.update_repo_work_dir(
        "test-persist",
        "transcript",
        &transcript.display().to_string(),
        &inferred.display().to_string(),
    )
    .unwrap();

    let session = db
        .get_stream(
            "test-persist",
            "transcript",
            &transcript.display().to_string(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        session.repo_work_dir,
        Some(repo.path().display().to_string()),
        "Inferred cwd must be persisted to DB for future processing cycles"
    );
}

// === Test Group 6: Priority Resolution (hook > DB > infer) ===

#[test]
fn test_repo_work_dir_priority_hook_wins_over_db() {
    let repo_a = TestRepo::new();
    repo_a
        .git(&["remote", "add", "origin", "git@github.com:org/old.git"])
        .unwrap();
    let repo_b = TestRepo::new();
    repo_b
        .git(&["remote", "add", "origin", "git@github.com:org/new.git"])
        .unwrap();

    let transcript = repo_b.path().join("session.jsonl");
    write_claude_transcript_with_cwd(&transcript, repo_b.path().to_str().unwrap());

    let temp_dir = TempDir::new().unwrap();
    let db = setup_test_db(temp_dir.path());
    let record = make_stream_record(
        "test-priority",
        "claude",
        &transcript,
        Some(repo_a.path().to_str().unwrap()),
    );
    db.insert_stream(&record).unwrap();

    // Hook provides repo_b's path (should take priority)
    let task_repo_work_dir = Some(repo_b.path().to_path_buf());
    let session = db
        .get_stream(
            "test-priority",
            "transcript",
            &transcript.display().to_string(),
        )
        .unwrap()
        .unwrap();
    let db_repo_work_dir = session.repo_work_dir.as_ref().map(PathBuf::from);

    // Resolution order: task > db > infer
    let resolved_work_dir = task_repo_work_dir.or(db_repo_work_dir);
    let resolved_url = resolved_work_dir
        .as_ref()
        .and_then(|p| resolve_repo_url_from_path(p));

    assert_eq!(
        resolved_url,
        Some("https://github.com/org/new".to_string()),
        "Hook-provided repo_work_dir MUST take priority over DB-stored value"
    );
}

#[test]
fn test_repo_work_dir_priority_db_used_when_no_hook() {
    let repo = TestRepo::new();
    repo.git(&["remote", "add", "origin", "git@github.com:org/stored.git"])
        .unwrap();

    let transcript = repo.path().join("session.jsonl");
    write_claude_transcript_without_cwd(&transcript);

    let temp_dir = TempDir::new().unwrap();
    let db = setup_test_db(temp_dir.path());
    let record = make_stream_record(
        "test-db-prio",
        "claude",
        &transcript,
        Some(repo.path().to_str().unwrap()),
    );
    db.insert_stream(&record).unwrap();

    let task_repo_work_dir: Option<PathBuf> = None;
    let session = db
        .get_stream(
            "test-db-prio",
            "transcript",
            &transcript.display().to_string(),
        )
        .unwrap()
        .unwrap();
    let db_repo_work_dir = session.repo_work_dir.as_ref().map(PathBuf::from);

    let resolved_work_dir = task_repo_work_dir.or(db_repo_work_dir);
    let resolved_url = resolved_work_dir
        .as_ref()
        .and_then(|p| resolve_repo_url_from_path(p));

    assert_eq!(
        resolved_url,
        Some("https://github.com/org/stored".to_string()),
        "DB-stored repo_work_dir must be used when no hook value present"
    );
}

#[test]
fn test_repo_work_dir_priority_infer_fallback() {
    let repo = TestRepo::new();
    repo.git(&["remote", "add", "origin", "git@github.com:org/inferred.git"])
        .unwrap();

    let transcript = repo.path().join("session.jsonl");
    write_claude_transcript_with_cwd(&transcript, repo.path().to_str().unwrap());

    let temp_dir = TempDir::new().unwrap();
    let db = setup_test_db(temp_dir.path());
    let record = make_stream_record("test-infer-prio", "claude", &transcript, None);
    db.insert_stream(&record).unwrap();

    let task_repo_work_dir: Option<PathBuf> = None;
    let session = db
        .get_stream(
            "test-infer-prio",
            "transcript",
            &transcript.display().to_string(),
        )
        .unwrap()
        .unwrap();
    let db_repo_work_dir = session.repo_work_dir.as_ref().map(PathBuf::from);

    let agent = ClaudeAgent::new();
    let inferred_cwd = agent.infer_cwd(&PathBuf::from(&session.stream_path));

    let resolved_work_dir = task_repo_work_dir.or(db_repo_work_dir).or(inferred_cwd);
    let resolved_url = resolved_work_dir
        .as_ref()
        .and_then(|p| resolve_repo_url_from_path(p));

    assert_eq!(
        resolved_url,
        Some("https://github.com/org/inferred".to_string()),
        "infer_cwd must be used as fallback when hook and DB are both None"
    );
}

// === Test Group 7: Cursor cwd inference from tool paths ===

#[test]
fn test_cursor_infer_cwd_returns_none_without_tool_paths() {
    use git_ai::streams::agents::CursorAgent;

    let temp_dir = TempDir::new().unwrap();
    let transcript = temp_dir.path().join("session.jsonl");
    fs::write(
        &transcript,
        r#"{"role":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#,
    )
    .unwrap();
    let agent = CursorAgent::new();
    assert_eq!(
        agent.infer_cwd(&transcript),
        None,
        "CursorAgent must return None when the transcript has no file-path tool inputs"
    );
}

#[test]
fn test_cursor_infer_cwd_from_tool_path() {
    use git_ai::streams::agents::CursorAgent;

    let repo = TestRepo::new();
    let file_path = repo.path().join("src.rs");
    fs::write(&file_path, "fn main() {}\n").unwrap();

    let temp_dir = TempDir::new().unwrap();
    let transcript = temp_dir.path().join("session.jsonl");
    let event = json!({
        "role": "assistant",
        "message": {
            "content": [{
                "type": "tool_use",
                "name": "Read",
                "input": {"path": file_path.to_string_lossy()}
            }]
        }
    });
    fs::write(&transcript, format!("{event}\n")).unwrap();

    let agent = CursorAgent::new();
    assert_eq!(
        agent.infer_cwd(&transcript),
        Some(repo.path().to_path_buf()),
        "CursorAgent should infer the git workdir from tool-use file paths"
    );
}

#[test]
fn test_copilot_infer_cwd_returns_none() {
    use git_ai::streams::agents::CopilotAgent;

    let temp_dir = TempDir::new().unwrap();
    let transcript = temp_dir.path().join("session.json");
    fs::write(&transcript, r#"{"messages":[]}"#).unwrap();
    let agent = CopilotAgent::new();
    assert_eq!(
        agent.infer_cwd(&transcript),
        None,
        "CopilotAgent must return None for infer_cwd"
    );
}

// === Test Group 8: Full E2E pipeline with repo_url ===

#[test]
fn test_full_pipeline_session_events_carry_repo_url() {
    let repo = TestRepo::new();
    repo.git(&["remote", "add", "origin", "git@github.com:myorg/myrepo.git"])
        .unwrap();

    let temp_dir = TempDir::new().unwrap();
    let db = setup_test_db(temp_dir.path());

    let transcript = temp_dir.path().join("session.jsonl");
    write_claude_transcript_with_cwd(&transcript, repo.path().to_str().unwrap());

    let record = make_stream_record(
        "pipeline-test",
        "claude",
        &transcript,
        Some(repo.path().to_str().unwrap()),
    );
    db.insert_stream(&record).unwrap();

    // Replicate process_session_blocking logic
    let retrieved = db
        .get_stream(
            "pipeline-test",
            "transcript",
            &transcript.display().to_string(),
        )
        .unwrap()
        .unwrap();
    let agent = ClaudeAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let batch = agent
        .read_incremental(
            &PathBuf::from(&retrieved.stream_path),
            watermark,
            &retrieved.session_id,
        )
        .unwrap();

    // Resolve repo_url
    let repo_work_dir = retrieved.repo_work_dir.as_ref().map(PathBuf::from);
    let resolved_url = repo_work_dir
        .as_ref()
        .and_then(|p| resolve_repo_url_from_path(p));

    // Build attrs
    let mut base_attrs = EventAttributes::with_version(env!("CARGO_PKG_VERSION"))
        .session_id(retrieved.session_id.clone())
        .tool(&retrieved.tool)
        .external_session_id(retrieved.external_session_id.clone())
        .external_parent_session_id_opt(retrieved.external_parent_session_id.clone());
    if let Some(ref url) = resolved_url {
        base_attrs = base_attrs.repo_url(url.clone());
    }

    // Build events
    let metric_events: Vec<MetricEvent> = batch
        .events
        .into_iter()
        .map(|raw_event| {
            let (eid, pid, tid) = agent.extract_event_ids(&raw_event);
            let sparse = base_attrs
                .clone()
                .trace_id("test-trace".to_string())
                .to_sparse();
            MetricEvent::from_values(
                SessionEventValues::with_ids(raw_event, eid, pid, tid),
                sparse,
            )
        })
        .collect();

    assert!(
        !metric_events.is_empty(),
        "Should have produced session events"
    );

    // STRICT: Every single event must have repo_url
    for (i, event) in metric_events.iter().enumerate() {
        let attrs = EventAttributes::from_sparse(&event.attrs);
        assert_eq!(
            attrs.repo_url,
            Some(Some("https://github.com/myorg/myrepo".to_string())),
            "Event {} must have repo_url set",
            i
        );
        assert_eq!(
            attrs.tool,
            Some(Some("claude".to_string())),
            "Event {} must have tool set",
            i
        );
        assert_eq!(
            attrs.session_id,
            Some(Some("pipeline-test".to_string())),
            "Event {} must have session_id set",
            i
        );
    }
}

#[test]
fn test_full_pipeline_session_events_no_repo_url_when_unavailable() {
    let temp_dir = TempDir::new().unwrap();
    let db = setup_test_db(temp_dir.path());

    let transcript = temp_dir.path().join("session.jsonl");
    write_claude_transcript_without_cwd(&transcript);

    let record = make_stream_record("no-repo-test", "claude", &transcript, None);
    db.insert_stream(&record).unwrap();

    let retrieved = db
        .get_stream(
            "no-repo-test",
            "transcript",
            &transcript.display().to_string(),
        )
        .unwrap()
        .unwrap();
    let agent = ClaudeAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let batch = agent
        .read_incremental(
            &PathBuf::from(&retrieved.stream_path),
            watermark,
            &retrieved.session_id,
        )
        .unwrap();

    let repo_work_dir = retrieved.repo_work_dir.as_ref().map(PathBuf::from);
    let inferred = agent.infer_cwd(&PathBuf::from(&retrieved.stream_path));
    let resolved_work_dir = repo_work_dir.or(inferred);
    let resolved_url = resolved_work_dir
        .as_ref()
        .and_then(|p| resolve_repo_url_from_path(p));

    assert_eq!(
        resolved_url, None,
        "No repo_url should be resolved when unavailable"
    );

    // Build attrs without repo_url
    let base_attrs = EventAttributes::with_version("test")
        .session_id(retrieved.session_id.clone())
        .tool(&retrieved.tool);

    let metric_events: Vec<MetricEvent> = batch
        .events
        .into_iter()
        .map(|raw_event| {
            let (eid, pid, tid) = agent.extract_event_ids(&raw_event);
            let sparse = base_attrs.clone().to_sparse();
            MetricEvent::from_values(
                SessionEventValues::with_ids(raw_event, eid, pid, tid),
                sparse,
            )
        })
        .collect();

    assert!(!metric_events.is_empty(), "Should have produced events");

    for (i, event) in metric_events.iter().enumerate() {
        let attrs = EventAttributes::from_sparse(&event.attrs);
        assert!(
            attrs.repo_url.is_none() || matches!(&attrs.repo_url, Some(None)),
            "Event {} must NOT have repo_url when unavailable, got: {:?}",
            i,
            attrs.repo_url
        );
    }
}

// === Test Group 9: Codex infer_cwd ===

#[test]
fn test_codex_infer_cwd_from_session_meta() {
    use git_ai::streams::agents::CodexAgent;

    let temp_dir = TempDir::new().unwrap();
    let transcript = temp_dir.path().join("session.jsonl");
    let events = [
        json!({"timestamp":"2026-02-11T05:53:33.335Z","type":"session_meta",
               "payload":{"id":"019c4b43","timestamp":"2026-02-11T05:53:33.266Z",
                          "cwd":"/Users/test/projects/my-app","originator":"Codex Desktop",
                          "cli_version":"0.99.0","source":"vscode","model_provider":"openai"}}),
        json!({"timestamp":"2026-02-11T05:53:33.340Z","type":"turn_context",
               "payload":{"turn_id":"turn-1","cwd":"/Users/test/projects/my-app",
                          "model":"gpt-5.1-codex"}}),
    ];
    let content: String = events.iter().map(|e| format!("{}\n", e)).collect();
    fs::write(&transcript, content).unwrap();

    let agent = CodexAgent::new();
    let result = agent.infer_cwd(&transcript);
    assert_eq!(
        result,
        Some(PathBuf::from("/Users/test/projects/my-app")),
        "CodexAgent must extract cwd from session_meta payload"
    );
}

#[test]
fn test_codex_infer_cwd_no_cwd() {
    use git_ai::streams::agents::CodexAgent;

    let temp_dir = TempDir::new().unwrap();
    let transcript = temp_dir.path().join("session.jsonl");
    let events = [
        json!({"timestamp":"2026-02-11T05:53:33.335Z","type":"message",
                             "payload":{"role":"user","content":"hello"}}),
    ];
    let content: String = events.iter().map(|e| format!("{}\n", e)).collect();
    fs::write(&transcript, content).unwrap();

    let agent = CodexAgent::new();
    let result = agent.infer_cwd(&transcript);
    assert_eq!(
        result, None,
        "CodexAgent must return None when no cwd in payload"
    );
}
