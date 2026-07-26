use crate::test_utils::{fixture_path, load_fixture};
use git_ai::model::stream_watermark::RecordIndexWatermark;
use git_ai::operations::daemon::stream_worker::extract_event_timestamp;
use git_ai::operations::streams::agent::Agent;
use git_ai::operations::streams::agents::CopilotAgent;

#[test]
fn legacy_file_time_fallback_path_remains_public() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let metadata = file.as_file().metadata().unwrap();

    let _ = git_ai::operations::streams::agent::file_time_fallback(&metadata, false);
}

#[test]
fn test_copilot_vscode_event_stream_timestamps() {
    let content = load_fixture("copilot_vscode_event_stream.jsonl");
    let events: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(events.len(), 13);

    // Every event must have a timestamp
    for (i, event) in events.iter().enumerate() {
        assert!(
            extract_event_timestamp(event).is_some(),
            "Event {} should have a timestamp",
            i
        );
    }

    // Verify exact values for first and last
    assert_eq!(extract_event_timestamp(&events[0]), Some(1778541192)); // 2026-05-11T23:13:12.819Z
    assert_eq!(extract_event_timestamp(&events[12]), Some(1778542087)); // 2026-05-11T23:28:07.005Z (last)
}

#[test]
fn test_copilot_cli_event_stream_timestamps() {
    let content = load_fixture("copilot_cli_session_events.jsonl");
    let events: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(events.len(), 6);

    for (i, event) in events.iter().enumerate() {
        assert!(
            extract_event_timestamp(event).is_some(),
            "Event {} should have a timestamp",
            i
        );
    }

    // First event: "2026-05-12T00:21:05.254Z"
    assert_eq!(extract_event_timestamp(&events[0]), Some(1778545265));
}

#[test]
fn test_copilot_session_event_stream_timestamps() {
    let content = load_fixture("copilot_session_event_stream.jsonl");
    let events: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(events.len(), 7);

    for (i, event) in events.iter().enumerate() {
        assert!(
            extract_event_timestamp(event).is_some(),
            "Event {} should have a timestamp",
            i
        );
    }

    // First event: "2026-02-14T03:02:25.825Z"
    assert_eq!(extract_event_timestamp(&events[0]), Some(1771038145));
    // Last event: "2026-02-14T03:02:41.547Z"
    assert_eq!(extract_event_timestamp(&events[6]), Some(1771038161));
}

#[test]
fn test_copilot_session_json_numeric_timestamps() {
    let agent = CopilotAgent::new();
    let fixture = fixture_path("copilot_session_simple.json");
    let watermark = Box::new(RecordIndexWatermark::new(0));
    let batch = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Should parse copilot session JSON");

    assert_eq!(batch.events.len(), 3);

    // These are numeric millisecond timestamps
    assert_eq!(extract_event_timestamp(&batch.events[0]), Some(1759845073)); // 1759845073835 ms
    assert_eq!(extract_event_timestamp(&batch.events[1]), Some(1759845101)); // 1759845101282 ms
    assert_eq!(extract_event_timestamp(&batch.events[2]), Some(1759850150)); // 1759850150757 ms
}

#[test]
fn test_claude_code_timestamps() {
    let content = load_fixture("claude-model-not-last.jsonl");
    let events: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // First event: "2026-04-19T18:05:00.000Z"
    assert_eq!(extract_event_timestamp(&events[0]), Some(1776621900));
    // Second event: "2026-04-19T18:05:01.000Z"
    assert_eq!(extract_event_timestamp(&events[1]), Some(1776621901));
}

#[test]
fn test_codex_timestamps() {
    let content = load_fixture("codex-session-simple.jsonl");
    let events: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // First event: "2026-02-11T05:53:33.335Z"
    assert_eq!(extract_event_timestamp(&events[0]), Some(1770789213));
    // Second event: "2026-02-11T05:53:33.340Z"
    assert_eq!(extract_event_timestamp(&events[1]), Some(1770789213)); // same second
}

#[test]
fn test_droid_timestamps() {
    let content = load_fixture("droid-session.jsonl");
    let events: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // First event (session_start) has NO timestamp field
    assert_eq!(extract_event_timestamp(&events[0]), None);

    // Second event has timestamp: "2026-01-28T16:57:01.391Z"
    assert_eq!(extract_event_timestamp(&events[1]), Some(1769619421));

    // All subsequent events should have timestamps
    for (i, event) in events.iter().enumerate().skip(1) {
        assert!(
            extract_event_timestamp(event).is_some(),
            "Droid event {} should have a timestamp",
            i
        );
    }
}

#[test]
fn test_gemini_timestamps() {
    let content = load_fixture("gemini-session-simple.jsonl");
    let events: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // First event: "2025-12-06T18:25:18.042Z"
    assert_eq!(extract_event_timestamp(&events[0]), Some(1765045518));

    // All regular events (first 6) should have timestamps
    for (i, event) in events.iter().take(6).enumerate() {
        assert!(
            extract_event_timestamp(event).is_some(),
            "Gemini event {} should have a timestamp",
            i
        );
    }

    // The last event is a $set metadata record with no timestamp field
    assert_eq!(extract_event_timestamp(&events[6]), None);
}

#[test]
fn test_pi_timestamps() {
    let content = load_fixture("pi-session-simple.jsonl");
    let events: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // First event: "2026-03-31T10:00:00.000Z"
    assert_eq!(extract_event_timestamp(&events[0]), Some(1774951200));
    // Second event: "2026-03-31T10:00:01.000Z"
    assert_eq!(extract_event_timestamp(&events[1]), Some(1774951201));
}

#[test]
fn test_cursor_has_no_timestamps() {
    let content = load_fixture("cursor-session-simple.jsonl");
    let events: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert!(!events.is_empty());

    for (i, event) in events.iter().enumerate() {
        assert_eq!(
            extract_event_timestamp(event),
            None,
            "Cursor event {} should NOT have a timestamp",
            i
        );
    }
}

#[test]
fn test_windsurf_has_no_timestamps() {
    let content = load_fixture("windsurf-session-simple.jsonl");
    let events: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert!(!events.is_empty());

    for (i, event) in events.iter().enumerate() {
        assert_eq!(
            extract_event_timestamp(event),
            None,
            "Windsurf event {} should NOT have a timestamp",
            i
        );
    }
}
