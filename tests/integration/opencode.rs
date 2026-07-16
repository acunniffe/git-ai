use crate::test_utils::fixture_path;
use git_ai::authorship::authorship_log_serialization::generate_session_id;
use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::error::GitAiError;
use git_ai::metrics::attrs::attr_pos;
use git_ai::metrics::db::MetricsDatabase;
use git_ai::metrics::types::MetricEventId;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn parse_opencode(hook_input: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    resolve_preset("opencode")?.parse(hook_input, "t_test")
}

fn opencode_sqlite_fixture_path() -> std::path::PathBuf {
    fixture_path("opencode-sqlite")
}

#[test]
fn test_opencode_raw_event_fidelity() {
    use chrono::{DateTime, Utc};
    use git_ai::streams::agent::Agent;
    use git_ai::streams::agents::OpenCodeAgent;
    use git_ai::streams::watermark::TimestampWatermark;
    use rusqlite::OpenFlags;

    let opencode_root = opencode_sqlite_fixture_path();
    let fixture = opencode_root.join("opencode.db");
    let session_id = "test-session-123";

    let agent = OpenCodeAgent::new();
    let watermark = Box::new(TimestampWatermark::new(DateTime::<Utc>::UNIX_EPOCH));
    let result = agent
        .read_incremental(&fixture, watermark, session_id)
        .unwrap();

    // Independently query the SQLite DB to construct the same expected events.
    let conn = git_ai::sqlite::open_with_flags_and_memory_limits(
        &fixture,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();

    let watermark_millis = DateTime::<Utc>::UNIX_EPOCH.timestamp_millis();

    // Read full message rows for this session with time_updated > watermark (same filter as the agent)
    let mut msg_stmt = conn
        .prepare(
            "SELECT id, session_id, time_created, time_updated, data FROM message \
             WHERE session_id = ? AND time_updated > ? \
             ORDER BY time_updated ASC, id ASC",
        )
        .unwrap();
    let messages: Vec<(String, serde_json::Value)> = msg_stmt
        .query_map(rusqlite::params![session_id, watermark_millis], |row| {
            let id: String = row.get(0)?;
            let row_session_id: String = row.get(1)?;
            let time_created: i64 = row.get(2)?;
            let time_updated: i64 = row.get(3)?;
            let data: String = row.get(4)?;
            Ok((id, row_session_id, time_created, time_updated, data))
        })
        .unwrap()
        .map(|r| {
            let (id, row_session_id, time_created, time_updated, data) = r.unwrap();
            let parsed_data: serde_json::Value = serde_json::from_str(&data).unwrap();
            let row_json = json!({
                "id": id,
                "session_id": row_session_id,
                "time_created": time_created,
                "time_updated": time_updated,
                "data": parsed_data,
            });
            (id, row_json)
        })
        .collect();

    // Read parts only for matched messages via IN-subquery (same query as the agent)
    let mut part_stmt = conn
        .prepare(
            "SELECT id, message_id, session_id, time_created, time_updated, data FROM part \
             WHERE message_id IN ( \
                 SELECT id FROM message WHERE session_id = ? AND time_updated > ? \
             ) \
             ORDER BY message_id ASC, time_updated ASC, id ASC",
        )
        .unwrap();
    let parts_rows: Vec<(String, serde_json::Value)> = part_stmt
        .query_map(rusqlite::params![session_id, watermark_millis], |row| {
            let id: String = row.get(0)?;
            let message_id: String = row.get(1)?;
            let row_session_id: String = row.get(2)?;
            let time_created: i64 = row.get(3)?;
            let time_updated: i64 = row.get(4)?;
            let data: String = row.get(5)?;
            Ok((
                id,
                message_id,
                row_session_id,
                time_created,
                time_updated,
                data,
            ))
        })
        .unwrap()
        .map(|r| {
            let (id, message_id, row_session_id, time_created, time_updated, data) = r.unwrap();
            let parsed_data: serde_json::Value = serde_json::from_str(&data).unwrap();
            let row_json = json!({
                "id": id,
                "message_id": message_id,
                "session_id": row_session_id,
                "time_created": time_created,
                "time_updated": time_updated,
                "data": parsed_data,
            });
            (message_id, row_json)
        })
        .collect();

    let mut parts_by_msg: std::collections::HashMap<String, Vec<serde_json::Value>> =
        std::collections::HashMap::new();
    for (msg_id, row_json) in parts_rows {
        parts_by_msg.entry(msg_id).or_default().push(row_json);
    }

    let expected: Vec<serde_json::Value> = messages
        .iter()
        .map(|(id, row_json)| {
            if let Some(parts) = parts_by_msg.get(id) {
                json!({"message": row_json, "parts": parts})
            } else {
                json!({"message": row_json})
            }
        })
        .collect();

    assert_eq!(result.events.len(), expected.len());
    assert_eq!(result.events, expected);
}

#[test]
#[serial_test::serial]
fn test_opencode_preset_pretooluse_returns_human_checkpoint() {
    let storage_path = opencode_sqlite_fixture_path();

    let hook_input = json!({
        "hook_event_name": "PreToolUse",
        "session_id": "test-session-123",
        "cwd": "/Users/test/project",
        "tool_input": {
            "filePath": "/Users/test/project/index.ts"
        }
    })
    .to_string();

    unsafe {
        std::env::set_var(
            "GIT_AI_OPENCODE_STORAGE_PATH",
            storage_path.to_str().unwrap(),
        );
    }

    let events = parse_opencode(&hook_input).expect("Failed to run OpenCodePreset");

    unsafe {
        std::env::remove_var("GIT_AI_OPENCODE_STORAGE_PATH");
    }

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.context.cwd, PathBuf::from("/Users/test/project"));
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("index.ts")),
                "will_edit_filepaths should contain the target file"
            );
        }
        _ => panic!("Expected PreFileEdit for PreToolUse"),
    }
}

#[test]
#[serial_test::serial]
fn test_opencode_preset_posttooluse_returns_ai_checkpoint() {
    let storage_path = opencode_sqlite_fixture_path();

    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "session_id": "test-session-123",
        "cwd": "/Users/test/project",
        "tool_input": {
            "filePath": "/Users/test/project/index.ts"
        }
    })
    .to_string();

    unsafe {
        std::env::set_var(
            "GIT_AI_OPENCODE_STORAGE_PATH",
            storage_path.to_str().unwrap(),
        );
    }

    let events = parse_opencode(&hook_input).expect("Failed to run OpenCodePreset");

    unsafe {
        std::env::remove_var("GIT_AI_OPENCODE_STORAGE_PATH");
    }

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(
                e.stream_source.is_some(),
                "Transcript should be present for AI checkpoint"
            );
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("index.ts")),
                "edited_filepaths should contain the target file"
            );
            assert_eq!(e.context.agent_id.tool, "opencode");
            assert_eq!(e.context.agent_id.id, "test-session-123");
            // Model is extracted from the OpenCode SQLite fixture at parse time
            assert_eq!(e.context.agent_id.model, "gpt-5");
        }
        _ => panic!("Expected PostFileEdit for PostToolUse"),
    }
}

#[test]
#[serial_test::serial]
fn test_opencode_preset_stores_session_id_in_metadata() {
    let storage_path = opencode_sqlite_fixture_path();

    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "session_id": "test-session-123",
        "cwd": "/Users/test/project",
        "tool_input": {
            "filePath": "/Users/test/project/index.ts"
        }
    })
    .to_string();

    unsafe {
        std::env::set_var(
            "GIT_AI_OPENCODE_STORAGE_PATH",
            storage_path.to_str().unwrap(),
        );
    }

    let events = parse_opencode(&hook_input).expect("Failed to run OpenCodePreset");

    unsafe {
        std::env::remove_var("GIT_AI_OPENCODE_STORAGE_PATH");
    }

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(
                e.context.metadata.contains_key("session_id"),
                "Metadata should contain session_id"
            );
            assert_eq!(e.context.metadata["session_id"], "test-session-123");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
#[serial_test::serial]
fn test_opencode_preset_sets_repo_working_dir() {
    let storage_path = opencode_sqlite_fixture_path();

    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "session_id": "test-session-123",
        "cwd": "/Users/test/my-project",
        "tool_input": {
            "filePath": "/Users/test/my-project/src/main.ts"
        }
    })
    .to_string();

    unsafe {
        std::env::set_var(
            "GIT_AI_OPENCODE_STORAGE_PATH",
            storage_path.to_str().unwrap(),
        );
    }

    let events = parse_opencode(&hook_input).expect("Failed to run OpenCodePreset");

    unsafe {
        std::env::remove_var("GIT_AI_OPENCODE_STORAGE_PATH");
    }

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.cwd, PathBuf::from("/Users/test/my-project"));
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
#[serial_test::serial]
fn test_opencode_preset_extracts_apply_patch_paths() {
    let storage_path = opencode_sqlite_fixture_path();

    let patch_text = "*** Begin Patch\n*** Update File: src/main.ts\n@@\n-old\n+new\n*** End Patch";
    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "session_id": "test-session-123",
        "cwd": "/Users/test/my-project",
        "tool_name": "apply_patch",
        "tool_input": {
            "patchText": patch_text
        }
    })
    .to_string();

    unsafe {
        std::env::set_var(
            "GIT_AI_OPENCODE_STORAGE_PATH",
            storage_path.to_str().unwrap(),
        );
    }

    let events = parse_opencode(&hook_input).expect("Failed to run OpenCodePreset");

    unsafe {
        std::env::remove_var("GIT_AI_OPENCODE_STORAGE_PATH");
    }

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            let path_strs: Vec<String> = e
                .file_paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            assert!(
                path_strs.iter().any(|p| p.contains("src/main.ts")),
                "Should extract file paths from apply_patch, got: {:?}",
                path_strs
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
#[serial_test::serial]
fn test_opencode_e2e_checkpoint_and_commit() {
    use crate::repos::test_repo::TestRepo;

    let metrics_dir = tempfile::tempdir().unwrap();
    let metrics_db_path = metrics_dir.path().join("metrics.db");
    let mut repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_METRICS_DB_PATH",
        metrics_db_path.to_str().unwrap(),
    )]);

    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();

    let src_dir = repo_root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    let file_path = src_dir.join("main.ts");
    fs::write(&file_path, "// initial\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let temp_storage = tempfile::tempdir().unwrap();
    let storage_path = temp_storage.path();

    // Copy the sqlite fixture's opencode.db to the temp storage directory
    let fixture_db = opencode_sqlite_fixture_path().join("opencode.db");
    fs::copy(&fixture_db, storage_path.join("opencode.db")).unwrap();

    unsafe {
        std::env::set_var(
            "GIT_AI_OPENCODE_STORAGE_PATH",
            storage_path.to_str().unwrap(),
        );
    }

    let pre_hook_input = json!({
        "hook_event_name": "PreToolUse",
        "session_id": "test-session-123",
        "cwd": repo_root.to_string_lossy().to_string(),
        "tool_input": {
            "filePath": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();

    repo.git_ai(&["checkpoint", "opencode", "--hook-input", &pre_hook_input])
        .unwrap();

    fs::write(&file_path, "// initial\n// Hello World\n").unwrap();

    let post_hook_input = json!({
        "hook_event_name": "PostToolUse",
        "session_id": "test-session-123",
        "cwd": repo_root.to_string_lossy().to_string(),
        "tool_input": {
            "filePath": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();

    repo.git_ai(&["checkpoint", "opencode", "--hook-input", &post_hook_input])
        .unwrap();

    let session_id = generate_session_id("test-session-123", "opencode");
    repo.sync_daemon_force();
    let deadline = Instant::now() + Duration::from_secs(10);
    let emitted_session_event = loop {
        let metrics = MetricsDatabase::open_at_path(Path::new(&metrics_db_path)).unwrap();
        let session_events = metrics
            .get_metric_history(0, None, &[MetricEventId::SessionEvent as u16])
            .unwrap();
        if session_events.iter().any(|record| {
            record
                .event
                .attrs
                .get(&attr_pos::SESSION_ID.to_string())
                .and_then(|value| value.as_str())
                == Some(session_id.as_str())
        }) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        emitted_session_event,
        "OpenCode post-checkpoint should emit transcript events for its generated session ID"
    );

    unsafe {
        std::env::remove_var("GIT_AI_OPENCODE_STORAGE_PATH");
    }

    let commit = repo.stage_all_and_commit("Add AI line").unwrap();

    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Should have at least one session record"
    );

    let session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Session record should exist");

    assert_eq!(
        session_record.agent_id.tool, "opencode",
        "Agent tool should be opencode"
    );
    assert_eq!(
        session_record.agent_id.model, "gpt-5",
        "Session record model should be extracted from OpenCode SQLite fixture"
    );
}

#[test]
fn test_opencode_transcript_ids_extracted_from_fixture() {
    use chrono::{DateTime, Utc};
    use git_ai::streams::agent::Agent;
    use git_ai::streams::agents::OpenCodeAgent;
    use git_ai::streams::watermark::TimestampWatermark;

    let fixture = fixture_path("opencode-sqlite/opencode.db");
    let agent = OpenCodeAgent::new();
    let watermark = Box::new(TimestampWatermark::new(DateTime::<Utc>::UNIX_EPOCH));
    let batch = agent
        .read_incremental(&fixture, watermark, "test-session-123")
        .unwrap();

    assert_eq!(batch.events.len(), 2, "Fixture has 2 messages");

    // Event 0: user message — has id, no parentID in data, no tool parts with callID
    let (eid, pid, tid) = agent.extract_event_ids(&batch.events[0]);
    assert_eq!(eid, Some("msg-user-sql-001".to_string()));
    assert_eq!(pid, None);
    assert_eq!(tid, None);

    // Event 1: assistant message — has id, parentID points to user msg, tool part has callID
    let (eid, pid, tid) = agent.extract_event_ids(&batch.events[1]);
    assert_eq!(eid, Some("msg-assistant-sql-001".to_string()));
    assert_eq!(pid, Some("msg-user-sql-001".to_string()));
    assert_eq!(tid, Some("call-sql-001".to_string()));
}

#[test]
fn test_opencode_tool_use_id_matches_hook_and_transcript() {
    use chrono::{DateTime, Utc};
    use git_ai::streams::agent::Agent;
    use git_ai::streams::agents::OpenCodeAgent;
    use git_ai::streams::watermark::TimestampWatermark;

    let fixture = fixture_path("opencode-sqlite/opencode.db");
    let agent = OpenCodeAgent::new();
    let watermark = Box::new(TimestampWatermark::new(DateTime::<Utc>::UNIX_EPOCH));
    let batch = agent
        .read_incremental(&fixture, watermark, "test-session-123")
        .unwrap();

    let assistant_event = &batch.events[1];
    let (_, _, tool_use_id_from_transcript) = agent.extract_event_ids(assistant_event);

    let hook_tool_use_id = "call-sql-001";
    assert_eq!(
        tool_use_id_from_transcript,
        Some(hook_tool_use_id.to_string()),
        "Transcript callID must match what the hook sends as tool_use_id"
    );
}

#[test]
#[serial_test::serial]
fn test_opencode_checkpoint_tool_use_id_matches_transcript_callid() {
    use crate::repos::test_repo::TestRepo;
    use chrono::{DateTime, Utc};
    use git_ai::streams::agent::Agent;
    use git_ai::streams::agents::OpenCodeAgent;
    use git_ai::streams::watermark::TimestampWatermark;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("index.ts");
    std::fs::write(&file_path, "// initial\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let temp_storage = tempfile::tempdir().unwrap();
    let storage_path = temp_storage.path();
    let fixture_db = fixture_path("opencode-sqlite/opencode.db");
    std::fs::copy(&fixture_db, storage_path.join("opencode.db")).unwrap();

    unsafe {
        std::env::set_var(
            "GIT_AI_OPENCODE_STORAGE_PATH",
            storage_path.to_str().unwrap(),
        );
    }

    let pre_hook_input = json!({
        "hook_event_name": "PreToolUse",
        "session_id": "test-session-123",
        "tool_use_id": "call-sql-001",
        "cwd": repo_root.to_string_lossy().to_string(),
        "tool_name": "edit",
        "tool_input": {
            "filePath": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();

    repo.git_ai(&["checkpoint", "opencode", "--hook-input", &pre_hook_input])
        .unwrap();

    std::fs::write(&file_path, "// initial\n// AI edit\n").unwrap();

    let post_hook_input = json!({
        "hook_event_name": "PostToolUse",
        "session_id": "test-session-123",
        "tool_use_id": "call-sql-001",
        "cwd": repo_root.to_string_lossy().to_string(),
        "tool_name": "edit",
        "tool_input": {
            "filePath": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();

    repo.git_ai(&["checkpoint", "opencode", "--hook-input", &post_hook_input])
        .unwrap();

    unsafe {
        std::env::remove_var("GIT_AI_OPENCODE_STORAGE_PATH");
    }

    let commit = repo.stage_all_and_commit("Add AI line").unwrap();

    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Should have at least one session record"
    );

    let agent = OpenCodeAgent::new();
    let watermark = Box::new(TimestampWatermark::new(DateTime::<Utc>::UNIX_EPOCH));
    let batch = agent
        .read_incremental(&fixture_db, watermark, "test-session-123")
        .unwrap();

    let (_, _, tid) = agent.extract_event_ids(&batch.events[1]);
    assert_eq!(
        tid,
        Some("call-sql-001".to_string()),
        "Transcript callID must equal the tool_use_id sent in the checkpoint hook"
    );
}

#[test]
#[serial_test::serial]
fn test_opencode_checkpoint_sets_parent_session_id_from_db() {
    let temp_storage = tempfile::tempdir().unwrap();
    let storage_path = temp_storage.path();
    let fixture_db = fixture_path("opencode-sqlite/opencode.db");
    std::fs::copy(&fixture_db, storage_path.join("opencode.db")).unwrap();

    unsafe {
        std::env::set_var(
            "GIT_AI_OPENCODE_STORAGE_PATH",
            storage_path.to_str().unwrap(),
        );
    }

    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "session_id": "test-session-123",
        "cwd": "/Users/test/project",
        "tool_name": "edit",
        "tool_use_id": "call-sql-001",
        "tool_input": {
            "filePath": "/Users/test/project/index.ts"
        }
    })
    .to_string();

    let preset = resolve_preset("opencode").unwrap();
    let events = preset.parse(&hook_input, "t_test").unwrap();

    unsafe {
        std::env::remove_var("GIT_AI_OPENCODE_STORAGE_PATH");
    }

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            let ts = e
                .stream_source
                .as_ref()
                .expect("should have transcript source");
            assert_eq!(
                ts.external_parent_session_id,
                Some("parent-session-456".to_string()),
                "OpenCode checkpoint should look up parent_id from session table"
            );
            assert_eq!(ts.external_session_id, "test-session-123",);
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

crate::reuse_tests_in_worktree!(test_opencode_raw_event_fidelity,);
