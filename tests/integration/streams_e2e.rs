//! End-to-end integration tests for transcript processing system.
//!
//! Tests the database integration and session record management.
//! The actual transcript processing and metrics emission are tested via
//! daemon tests and manual verification.

use git_ai::metrics::{
    EventAttributes, MetricEvent, OtelTraceValues, PosEncoded, SessionEventValues,
};
use git_ai::streams::agent::Agent;
use git_ai::streams::agents::{ClaudeAgent, CopilotAgent, OpenCodeAgent};
use git_ai::streams::watermark::{
    ByteOffsetWatermark, TimestampCursorWatermark, TimestampWatermark, WatermarkStrategy,
};
use git_ai::streams::{StreamRecord, StreamsDatabase};
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

#[allow(dead_code)]
fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("transcripts")
        .join("fixtures")
        .join(name)
}

fn test_fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn test_session_database_basic() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("transcripts.db");
    let db = StreamsDatabase::open(&db_path).unwrap();

    let now = chrono::Utc::now().timestamp();
    let session = StreamRecord {
        session_id: "s_test_123".to_string(),
        stream_kind: "transcript".to_string(),
        tool: "claude".to_string(),
        stream_path: "/path/to/transcript.jsonl".to_string(),
        stream_format: "claude-jsonl".to_string(),
        watermark_type: "byte_offset".to_string(),
        watermark_value: "0".to_string(),
        external_session_id: "test-ext-session".to_string(),
        external_parent_session_id: None,
        first_seen_at: now,
        last_processed_at: now,
        last_known_size: 0,
        last_modified: None,
        processing_errors: 0,
        last_error: None,
        repo_work_dir: None,
    };

    // Insert
    db.insert_stream(&session).unwrap();

    // Read
    let retrieved = db
        .get_stream("s_test_123", "transcript", "/path/to/transcript.jsonl")
        .unwrap();
    assert!(retrieved.is_some());
    let retrieved = retrieved.unwrap();
    assert_eq!(retrieved.session_id, "s_test_123");
    assert_eq!(retrieved.tool, "claude");
    assert_eq!(retrieved.processing_errors, 0);

    // Update watermark
    let new_watermark = ByteOffsetWatermark::new(100);
    db.update_watermark(
        "s_test_123",
        "transcript",
        "/path/to/transcript.jsonl",
        &new_watermark,
    )
    .unwrap();
    let retrieved_updated = db
        .get_stream("s_test_123", "transcript", "/path/to/transcript.jsonl")
        .unwrap()
        .unwrap();
    assert_eq!(retrieved_updated.watermark_value, "100");

    // List all sessions
    let all_sessions = db.all_streams().unwrap();
    assert_eq!(all_sessions.len(), 1);
    assert_eq!(all_sessions[0].session_id, "s_test_123");
}

#[test]
fn test_watermark_integration() {
    let temp_dir = TempDir::new().unwrap();
    let transcript_file = temp_dir.path().join("watermark_test.jsonl");

    // Write initial content
    let mut file = File::create(&transcript_file).unwrap();
    writeln!(
        file,
        r#"{{"type":"user","message":{{"content":"First"}},"timestamp":"2025-01-01T00:00:00Z"}}"#
    )
    .unwrap();
    file.flush().unwrap();
    drop(file);

    // Read from start
    let agent = ClaudeAgent::new();
    let watermark1 = Box::new(ByteOffsetWatermark::new(0));
    let result1 = agent
        .read_incremental(&transcript_file, watermark1, "s_test")
        .unwrap();
    assert_eq!(result1.events.len(), 1);

    let offset1: u64 = result1.new_watermark.serialize().parse().unwrap();
    assert!(offset1 > 0, "Watermark should advance");

    // Append more content
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&transcript_file)
        .unwrap();
    writeln!(
        file,
        r#"{{"type":"user","message":{{"content":"Second"}},"timestamp":"2025-01-01T00:00:01Z"}}"#
    )
    .unwrap();
    file.flush().unwrap();
    drop(file);

    // Read from watermark - should only get new line
    let watermark2 = Box::new(ByteOffsetWatermark::new(offset1));
    let result2 = agent
        .read_incremental(&transcript_file, watermark2, "s_test")
        .unwrap();
    assert_eq!(result2.events.len(), 1);
    assert_eq!(
        result2.events[0]["message"]["content"].as_str(),
        Some("Second")
    );

    let offset2: u64 = result2.new_watermark.serialize().parse().unwrap();
    assert!(offset2 > offset1, "Watermark should continue advancing");
}

#[test]
fn test_multiple_sessions_isolation() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("transcripts.db");
    let db = StreamsDatabase::open(&db_path).unwrap();

    let now = chrono::Utc::now().timestamp();

    // Create multiple sessions
    for i in 0..5 {
        let session = StreamRecord {
            session_id: format!("s_session_{}", i),
            stream_kind: "transcript".to_string(),
            tool: "claude".to_string(),
            stream_path: format!("/path/to/transcript_{}.jsonl", i),
            stream_format: "claude-jsonl".to_string(),
            watermark_type: "byte_offset".to_string(),
            watermark_value: (i * 10).to_string(),
            external_session_id: "test-ext-session".to_string(),
            external_parent_session_id: None,
            first_seen_at: now,
            last_processed_at: now,
            last_known_size: 0,
            last_modified: None,
            processing_errors: 0,
            last_error: None,
            repo_work_dir: None,
        };
        db.insert_stream(&session).unwrap();
    }

    // Verify all sessions exist independently
    let all_sessions = db.all_streams().unwrap();
    assert_eq!(all_sessions.len(), 5);

    // Verify each session has correct data
    for i in 0..5 {
        let session = db
            .get_stream(
                &format!("s_session_{}", i),
                "transcript",
                &format!("/path/to/transcript_{}.jsonl", i),
            )
            .unwrap()
            .unwrap();
        assert_eq!(session.watermark_value, (i * 10).to_string());
        assert_eq!(session.processing_errors, 0);
    }
}

#[test]
fn test_database_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("transcripts.db");

    let now = chrono::Utc::now().timestamp();

    // Create and close database
    {
        let db = StreamsDatabase::open(&db_path).unwrap();
        let session = StreamRecord {
            session_id: "s_persist".to_string(),
            stream_kind: "transcript".to_string(),
            tool: "claude".to_string(),
            stream_path: "/path/to/transcript.jsonl".to_string(),
            stream_format: "claude-jsonl".to_string(),
            watermark_type: "byte_offset".to_string(),
            watermark_value: "42".to_string(),
            external_session_id: "test-ext-session".to_string(),
            external_parent_session_id: None,
            first_seen_at: now,
            last_processed_at: now,
            last_known_size: 0,
            last_modified: None,
            processing_errors: 0,
            last_error: None,
            repo_work_dir: None,
        };
        db.insert_stream(&session).unwrap();
    }

    // Reopen database
    {
        let db = StreamsDatabase::open(&db_path).unwrap();
        let retrieved = db
            .get_stream("s_persist", "transcript", "/path/to/transcript.jsonl")
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.session_id, "s_persist");
        assert_eq!(retrieved.watermark_value, "42");
        assert_eq!(retrieved.processing_errors, 0);
    }
}

#[test]
fn test_error_tracking() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("transcripts.db");
    let db = StreamsDatabase::open(&db_path).unwrap();

    let now = chrono::Utc::now().timestamp();
    let session = StreamRecord {
        session_id: "s_errors".to_string(),
        stream_kind: "transcript".to_string(),
        tool: "claude".to_string(),
        stream_path: "/path/to/transcript.jsonl".to_string(),
        stream_format: "claude-jsonl".to_string(),
        watermark_type: "byte_offset".to_string(),
        watermark_value: "0".to_string(),
        external_session_id: "test-ext-session".to_string(),
        external_parent_session_id: None,
        first_seen_at: now,
        last_processed_at: now,
        last_known_size: 0,
        last_modified: None,
        processing_errors: 0,
        last_error: None,
        repo_work_dir: None,
    };

    db.insert_stream(&session).unwrap();

    // Simulate errors
    db.record_error(
        "s_errors",
        "transcript",
        "/path/to/transcript.jsonl",
        "First error",
    )
    .unwrap();
    let retrieved = db
        .get_stream("s_errors", "transcript", "/path/to/transcript.jsonl")
        .unwrap()
        .unwrap();
    assert_eq!(retrieved.processing_errors, 1);
    assert_eq!(retrieved.last_error, Some("First error".to_string()));

    // More errors
    db.record_error(
        "s_errors",
        "transcript",
        "/path/to/transcript.jsonl",
        "Second error",
    )
    .unwrap();
    let retrieved2 = db
        .get_stream("s_errors", "transcript", "/path/to/transcript.jsonl")
        .unwrap()
        .unwrap();
    assert_eq!(retrieved2.processing_errors, 2);
    assert_eq!(retrieved2.last_error, Some("Second error".to_string()));
}

#[test]
fn test_full_pipeline_claude_session_ids_flow_through() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("transcripts.db");
    let db = Arc::new(StreamsDatabase::open(&db_path).unwrap());

    let fixture = fixture_path("claude_with_ids.jsonl");
    let now = chrono::Utc::now().timestamp();

    let session = StreamRecord {
        session_id: "sess-parent-abc".to_string(),
        stream_kind: "transcript".to_string(),
        tool: "claude".to_string(),
        stream_path: fixture.display().to_string(),
        stream_format: "ClaudeJsonl".to_string(),
        watermark_type: "ByteOffset".to_string(),
        watermark_value: "0".to_string(),
        external_session_id: "sess-parent-abc".to_string(),
        external_parent_session_id: None,
        first_seen_at: now,
        last_processed_at: 0,
        last_known_size: 0,
        last_modified: None,
        processing_errors: 0,
        last_error: None,
        repo_work_dir: None,
    };
    db.insert_stream(&session).unwrap();

    let retrieved = db
        .get_stream(
            "sess-parent-abc",
            "transcript",
            &fixture.display().to_string(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(retrieved.external_session_id, "sess-parent-abc".to_string());
    assert_eq!(retrieved.external_parent_session_id, None);

    let agent = ClaudeAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let batch = agent
        .read_incremental(
            &PathBuf::from(&retrieved.stream_path),
            watermark,
            &retrieved.session_id,
        )
        .unwrap();

    let attrs_sparse = EventAttributes::with_version("test")
        .session_id(retrieved.session_id.clone())
        .external_session_id(retrieved.external_session_id.clone())
        .external_parent_session_id_opt(retrieved.external_parent_session_id.clone())
        .to_sparse();

    let metric_events: Vec<MetricEvent> = batch
        .events
        .into_iter()
        .map(|raw_event| {
            let (eid, pid, tid) = agent.extract_event_ids(&raw_event);
            MetricEvent::from_values(
                SessionEventValues::with_ids(raw_event, eid, pid, tid),
                attrs_sparse.clone(),
            )
        })
        .collect();

    assert_eq!(metric_events.len(), 5);

    let attrs = EventAttributes::from_sparse(&metric_events[0].attrs);
    assert_eq!(attrs.session_id, Some(Some("sess-parent-abc".to_string())));
    assert_eq!(
        attrs.external_session_id,
        Some(Some("sess-parent-abc".to_string()))
    );
    assert_eq!(attrs.external_parent_session_id, None);

    let values = SessionEventValues::from_sparse(&metric_events[2].values);
    assert_eq!(
        values.external_event_id,
        Some("ccc33333-3333-3333-3333-333333333333".to_string())
    );
    assert_eq!(
        values.external_parent_event_id,
        Some("bbb22222-2222-2222-2222-222222222222".to_string())
    );
    assert_eq!(
        values.external_tool_use_id,
        Some("toolu_01AbCdEfGhIjKlMnOp".to_string())
    );
}

#[test]
fn test_full_pipeline_opencode_session_ids_flow_through() {
    use chrono::{DateTime, Utc};

    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("transcripts.db");
    let db = Arc::new(StreamsDatabase::open(&db_path).unwrap());

    let fixture = test_fixture_path("opencode-sqlite/opencode.db");
    let now = chrono::Utc::now().timestamp();

    let session = StreamRecord {
        session_id: "test-session-123".to_string(),
        stream_kind: "transcript".to_string(),
        tool: "opencode".to_string(),
        stream_path: fixture.display().to_string(),
        stream_format: "OpenCodeSqlite".to_string(),
        watermark_type: "Timestamp".to_string(),
        watermark_value: DateTime::<Utc>::UNIX_EPOCH.to_rfc3339(),
        external_session_id: "test-session-123".to_string(),
        external_parent_session_id: None,
        first_seen_at: now,
        last_processed_at: 0,
        last_known_size: 0,
        last_modified: None,
        processing_errors: 0,
        last_error: None,
        repo_work_dir: None,
    };
    db.insert_stream(&session).unwrap();

    let agent = OpenCodeAgent::new();
    let watermark = Box::new(TimestampWatermark::new(DateTime::<Utc>::UNIX_EPOCH));
    let batch = agent
        .read_incremental(
            &PathBuf::from(&session.stream_path),
            watermark,
            &session.session_id,
        )
        .unwrap();

    let attrs_sparse = EventAttributes::with_version("test")
        .session_id(session.session_id.clone())
        .external_session_id(session.external_session_id.clone())
        .external_parent_session_id_opt(session.external_parent_session_id.clone())
        .to_sparse();

    let metric_events: Vec<MetricEvent> = batch
        .events
        .into_iter()
        .map(|raw_event| {
            let (eid, pid, tid) = agent.extract_event_ids(&raw_event);
            MetricEvent::from_values(
                SessionEventValues::with_ids(raw_event, eid, pid, tid),
                attrs_sparse.clone(),
            )
        })
        .collect();

    assert_eq!(metric_events.len(), 2);

    let values = SessionEventValues::from_sparse(&metric_events[1].values);
    assert_eq!(
        values.external_event_id,
        Some("msg-assistant-sql-001".to_string())
    );
    assert_eq!(
        values.external_parent_event_id,
        Some("msg-user-sql-001".to_string())
    );
    assert_eq!(
        values.external_tool_use_id,
        Some("call-sql-001".to_string())
    );
}

#[test]
fn test_subagent_session_record_has_parent_link() {
    use git_ai::streams::agents::claude::ClaudeAgent as ClaudeAgentImpl;

    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("transcripts.db");
    let db = StreamsDatabase::open(&db_path).unwrap();

    let subagent_path = PathBuf::from(
        "/home/user/.claude/projects/proj/sess-parent-abc/subagents/agent-a1b2c3d4e5f6.jsonl",
    );
    let parent_id = ClaudeAgentImpl::detect_subagent_parent(&subagent_path);
    assert_eq!(parent_id, Some("sess-parent-abc".to_string()));

    let now = chrono::Utc::now().timestamp();
    let session = StreamRecord {
        session_id: "agent-a1b2c3d4e5f6".to_string(),
        stream_kind: "transcript".to_string(),
        tool: "claude".to_string(),
        stream_path: subagent_path.display().to_string(),
        stream_format: "ClaudeJsonl".to_string(),
        watermark_type: "ByteOffset".to_string(),
        watermark_value: "0".to_string(),
        external_session_id: "agent-a1b2c3d4e5f6".to_string(),
        external_parent_session_id: parent_id.clone(),
        first_seen_at: now,
        last_processed_at: 0,
        last_known_size: 0,
        last_modified: None,
        processing_errors: 0,
        last_error: None,
        repo_work_dir: None,
    };
    db.insert_stream(&session).unwrap();

    let retrieved = db
        .get_stream(
            "agent-a1b2c3d4e5f6",
            "transcript",
            &subagent_path.display().to_string(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        retrieved.external_session_id,
        "agent-a1b2c3d4e5f6".to_string()
    );
    assert_eq!(
        retrieved.external_parent_session_id,
        Some("sess-parent-abc".to_string())
    );

    let attrs = EventAttributes::with_version("test")
        .session_id(retrieved.session_id.clone())
        .external_session_id(retrieved.external_session_id.clone())
        .external_parent_session_id_opt(retrieved.external_parent_session_id.clone())
        .to_sparse();

    let restored = EventAttributes::from_sparse(&attrs);
    assert_eq!(
        restored.external_session_id,
        Some(Some("agent-a1b2c3d4e5f6".to_string()))
    );
    assert_eq!(
        restored.external_parent_session_id,
        Some(Some("sess-parent-abc".to_string()))
    );
}

#[test]
fn test_copilot_otel_stream_reads_spans_with_event_ids() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("transcripts.db");
    let db = Arc::new(StreamsDatabase::open(&db_path).unwrap());

    let fixture = test_fixture_path("copilot-otel/traces.db");
    let now = chrono::Utc::now().timestamp();

    // Create session record for the OTEL stream
    let session = StreamRecord {
        session_id: "copilot-otel-test-session".to_string(),
        stream_kind: "otel_traces".to_string(),
        tool: "github-copilot".to_string(),
        stream_path: fixture.display().to_string(),
        stream_format: "CopilotOtelSqlite".to_string(),
        watermark_type: "TimestampCursor".to_string(),
        watermark_value: TimestampCursorWatermark::initial().serialize(),
        external_session_id: "copilot-ext-session-1".to_string(),
        external_parent_session_id: None,
        first_seen_at: now,
        last_processed_at: 0,
        last_known_size: 0,
        last_modified: None,
        processing_errors: 0,
        last_error: None,
        repo_work_dir: None,
    };
    db.insert_stream(&session).unwrap();

    // Read spans using CopilotAgent (dispatches to copilot_otel reader for .db files)
    let agent = CopilotAgent::new();
    let watermark = Box::new(TimestampCursorWatermark::initial());
    let batch = agent
        .read_incremental(
            &PathBuf::from(&session.stream_path),
            watermark,
            &session.session_id,
        )
        .unwrap();

    // The fixture has 24 spans
    assert!(!batch.events.is_empty(), "expected spans from fixture DB");
    assert!(
        batch.events.len() >= 20,
        "fixture has ~24 spans, got {}",
        batch.events.len()
    );

    // Verify event structure
    let first = &batch.events[0];
    assert!(first.get("span").is_some(), "event should have 'span' key");
    assert!(
        first.get("attributes").is_some(),
        "event should have 'attributes' key"
    );
    assert!(
        first.get("events").is_some(),
        "event should have 'events' key"
    );

    // Verify event ID extraction works for OTEL events
    let (event_id, _parent_id, _tool_use_id) = agent.extract_event_ids(first);
    assert!(
        event_id.is_some(),
        "span_id should be extracted as event_id"
    );

    // Verify we can construct MetricEvents from these
    let attrs_sparse = EventAttributes::with_version("test")
        .session_id(session.session_id.clone())
        .external_session_id(session.external_session_id.clone())
        .to_sparse();

    let metric_events: Vec<MetricEvent> = batch
        .events
        .into_iter()
        .map(|raw_event| {
            let (eid, pid, tid) = agent.extract_event_ids(&raw_event);
            MetricEvent::from_values(
                OtelTraceValues::with_ids(raw_event, eid, pid, tid),
                attrs_sparse.clone(),
            )
        })
        .collect();

    assert!(!metric_events.is_empty());

    // Verify watermark advanced
    let new_wm_serialized = batch.new_watermark.serialize();
    assert_ne!(
        new_wm_serialized, "0|",
        "watermark should have advanced from initial"
    );
}

#[test]
fn test_copilot_otel_stream_watermark_resumes_correctly() {
    let fixture = test_fixture_path("copilot-otel/traces.db");
    let agent = CopilotAgent::new();

    // First read: get all spans from initial cursor
    let watermark1 = Box::new(TimestampCursorWatermark::initial());
    let batch1 = agent
        .read_incremental(&fixture, watermark1, "test-session")
        .unwrap();
    let count1 = batch1.events.len();
    assert!(
        count1 >= 20,
        "expected bulk of spans in first read, got {}",
        count1
    );

    // Watermark should have advanced from initial
    let wm1_str = batch1.new_watermark.serialize();
    assert_ne!(
        wm1_str, "0|",
        "watermark should advance from initial after first read"
    );

    // Second read: keyset pagination guarantees no duplicates
    let batch2 = agent
        .read_incremental(&fixture, batch1.new_watermark, "test-session")
        .unwrap();

    // With keyset pagination, second read should return remaining spans (if any)
    // or be empty if all spans were consumed in the first batch
    assert!(
        batch2.events.len() < count1,
        "second read ({}) should be smaller than first ({})",
        batch2.events.len(),
        count1
    );
}

#[test]
fn test_copilot_agent_streams_declares_otel_stream() {
    let agent = CopilotAgent::new();
    let streams = agent.streams();

    assert_eq!(streams.len(), 2, "CopilotAgent should declare 2 streams");
    assert_eq!(streams[0].stream_kind, "transcript");
    assert_eq!(streams[1].stream_kind, "otel_traces");
}

#[test]
fn test_copilot_otel_events_use_otel_trace_event_type() {
    use git_ai::metrics::OtelTraceValues;
    use git_ai::metrics::events::otel_trace_pos;

    let fixture = test_fixture_path("copilot-otel/traces.db");
    let agent = CopilotAgent::new();

    let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
    let batch = agent
        .read_incremental(&fixture, watermark, "test-session")
        .unwrap();

    assert!(!batch.events.is_empty());

    // Verify OtelTraceValues roundtrip for actual fixture data
    for raw_event in batch.events.iter().take(5) {
        let (eid, pid, tid) = agent.extract_event_ids(raw_event);
        let values =
            OtelTraceValues::with_ids(raw_event.clone(), eid.clone(), pid.clone(), tid.clone());

        // Verify sparse encoding preserves the full nested OTEL structure
        let sparse = git_ai::metrics::PosEncoded::to_sparse(&values);
        let raw_json = sparse.get(&otel_trace_pos::RAW_JSON.to_string()).unwrap();
        assert!(
            raw_json.get("span").is_some(),
            "raw_json must contain 'span' key"
        );
        assert!(
            raw_json.get("attributes").is_some(),
            "raw_json must contain 'attributes' key"
        );
        assert!(
            raw_json.get("events").is_some(),
            "raw_json must contain 'events' key"
        );

        // Verify IDs are preserved
        if let Some(ref id) = eid {
            assert_eq!(
                sparse.get(&otel_trace_pos::EXTERNAL_EVENT_ID.to_string()),
                Some(&serde_json::json!(id))
            );
        }
    }
}

#[test]
fn test_copilot_otel_per_event_session_id_derivation() {
    use git_ai::authorship::authorship_log_serialization::generate_session_id;

    let fixture = test_fixture_path("copilot-otel/traces.db");
    let agent = CopilotAgent::new();

    let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
    let batch = agent
        .read_incremental(&fixture, watermark, "test-session")
        .unwrap();

    // Every event from the fixture should have an extractable session_id
    // (the SQL filter already excludes spans without session IDs)
    for event in &batch.events {
        let session_id = agent.extract_event_session_id(event);
        assert!(
            session_id.is_some(),
            "fixture spans should all have extractable session_id, span: {}",
            event["span"]["span_id"]
        );

        // Verify the derived session_id is deterministic
        let sid = session_id.unwrap();
        let derived1 = generate_session_id(&sid, "github-copilot");
        let derived2 = generate_session_id(&sid, "github-copilot");
        assert_eq!(
            derived1, derived2,
            "session_id derivation must be deterministic"
        );
    }
}

#[test]
fn test_copilot_agent_streams_otel_path_resolution() {
    use git_ai::streams::agent::Agent;

    let agent = CopilotAgent::new();
    let streams = agent.streams();

    // First stream is transcript (identity path)
    let transcript_stream = &streams[0];
    assert_eq!(transcript_stream.stream_kind, "transcript");
    assert!(!transcript_stream.shared);

    let test_path = std::path::PathBuf::from("/fake/path/transcripts/session.jsonl");
    let resolved = transcript_stream.resolve_path(&test_path);
    assert_eq!(resolved, Some(test_path.clone()));

    // Second stream is otel_traces (shared, custom resolver)
    let otel_stream = &streams[1];
    assert_eq!(otel_stream.stream_kind, "otel_traces");
    assert!(otel_stream.shared);
}

#[test]
fn codex_fork_rollout_checkpoint_tracks_one_stream_keyed_by_filename() {
    use crate::repos::test_repo::TestRepo;
    use serde_json::json;

    // A fork/subagent rollout reports the PARENT thread id in hook payloads.
    // The checkpoint path must key the stream by the rollout FILENAME (like
    // sweep discovery); keying by the hook id would track the same file a
    // second time under a different identity and ingest every line twice.
    let repo = TestRepo::new();
    let parent_id = "01a00000-0000-7000-8000-0000000000aa";
    let child_id = "01a00000-0000-7000-8000-0000000000bb";

    let rollout_dir = repo.path().join("rollouts");
    fs::create_dir_all(&rollout_dir).unwrap();
    let rollout = rollout_dir.join(format!("rollout-2026-02-06T20-35-49-{child_id}.jsonl"));
    fs::write(
        &rollout,
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{child_id}\",\"forked_from_id\":\"{parent_id}\",\"thread_source\":\"subagent\",\"model\":\"gpt-5.1-codex\"}}}}\n"
        ),
    )
    .unwrap();

    // Real preset flow: pre/post apply_patch checkpoints carrying the
    // PARENT id, exactly what codex hooks send from a subagent turn.
    let file_path = repo.path().join("edited.txt");
    for hook_event_name in ["PreToolUse", "PostToolUse"] {
        let hook_input = json!({
            "cwd": repo.canonical_path().to_string_lossy(),
            "hook_event_name": hook_event_name,
            "tool_name": "apply_patch",
            "tool_use_id": "tu-fork-1",
            "session_id": parent_id,
            "transcript_path": rollout.to_string_lossy(),
            "tool_input": { "patch": format!("*** Update File: {}\n", file_path.to_string_lossy()) }
        })
        .to_string();
        repo.git_ai(&["checkpoint", "codex", "--hook-input", &hook_input])
            .expect("checkpoint should succeed");
        if hook_event_name == "PreToolUse" {
            fs::write(&file_path, "edited\n").unwrap();
        }
    }
    repo.sync_daemon();
    repo.git_ai(&["await", "--timeout", "60"])
        .expect("await should drain stream registration");

    let db_path = repo
        .daemon_home_path()
        .join(".git-ai")
        .join("internal")
        .join("transcripts-db");
    let db = StreamsDatabase::open(&db_path).unwrap();
    // The daemon canonicalizes stream paths before storing them (macOS tmp
    // dirs live under the /var -> /private/var symlink); comparing against
    // the canonicalized path also pins that checkpoint registration and
    // sweep discovery converge on the same key - the invariant that keeps
    // one row per file.
    let canonical_rollout = rollout.canonicalize().unwrap().display().to_string();
    let rows: Vec<StreamRecord> = db
        .all_streams()
        .unwrap()
        .into_iter()
        .filter(|r| r.stream_path == canonical_rollout)
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "the rollout must be tracked exactly once, not once per identity"
    );
    assert_eq!(rows[0].external_session_id, child_id, "keyed by filename");
    assert_eq!(
        rows[0].external_parent_session_id.as_deref(),
        Some(parent_id),
        "parent resolved from session_meta"
    );
}
