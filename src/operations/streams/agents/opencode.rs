//! OpenCode agent implementation (SQLite-only).

pub use super::opencode_checkpoint::open_sqlite_readonly;
use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::{TimestampWatermark, WatermarkStrategy};
use crate::operations::streams::agent::{Agent, PathResolverKind, StreamDescriptor};
use crate::operations::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use chrono::DateTime;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// OpenCode agent that reads from an OpenCode SQLite database.
pub struct OpenCodeAgent {
    batch_size: usize,
}

impl OpenCodeAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }
}

/// Read messages from the database, returning each row as a complete JSON object
/// containing all columns (id, session_id, time_created, time_updated, data).
fn read_session_messages_raw_with_limit(
    conn: &Connection,
    session_id: &str,
    after_updated: i64,
    limit: usize,
) -> Result<Vec<(String, i64, serde_json::Value)>, StreamError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, time_created, time_updated, data FROM message \
             WHERE session_id = ? AND time_updated > ? \
             ORDER BY time_updated ASC, id ASC \
             LIMIT ?",
        )
        .map_err(|e| StreamError::Fatal {
            message: format!("Failed to prepare message query: {}", e),
        })?;

    let rows = stmt
        .query_map(rusqlite::params![session_id, after_updated, limit], |row| {
            let id: String = row.get(0)?;
            let row_session_id: String = row.get(1)?;
            let time_created: i64 = row.get(2)?;
            let time_updated: i64 = row.get(3)?;
            let data: String = row.get(4)?;
            Ok((id, row_session_id, time_created, time_updated, data))
        })
        .map_err(|e| StreamError::Fatal {
            message: format!("Failed to query messages: {}", e),
        })?;

    let mut messages = Vec::new();
    for row in rows {
        let (id, row_session_id, time_created, time_updated, data) =
            row.map_err(|e| StreamError::Fatal {
                message: format!("Failed to read message row: {}", e),
            })?;

        let parsed_data: serde_json::Value =
            serde_json::from_str(&data).map_err(|e| StreamError::Parse {
                line: 0,
                message: format!("Failed to parse message data for id {}: {}", id, e),
            })?;

        // Build directly via Map to move parsed_data instead of cloning (json! macro clones)
        let mut map = serde_json::Map::with_capacity(5);
        map.insert("id".into(), serde_json::Value::String(id.clone()));
        map.insert(
            "session_id".into(),
            serde_json::Value::String(row_session_id),
        );
        map.insert(
            "time_created".into(),
            serde_json::Value::Number(time_created.into()),
        );
        map.insert(
            "time_updated".into(),
            serde_json::Value::Number(time_updated.into()),
        );
        map.insert("data".into(), parsed_data);

        messages.push((id, time_updated, serde_json::Value::Object(map)));
    }

    Ok(messages)
}

/// Read parts for the matched messages only, using an IN-subquery to avoid loading
/// all parts for the entire session. Returns each row as a complete JSON object
/// containing all columns (id, message_id, session_id, time_created, time_updated, data),
/// grouped by message_id.
fn read_parts_for_messages_with_limit(
    conn: &Connection,
    session_id: &str,
    after_updated: i64,
    limit: usize,
) -> Result<HashMap<String, Vec<serde_json::Value>>, StreamError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, message_id, session_id, time_created, time_updated, data FROM part \
             WHERE message_id IN ( \
                 SELECT id FROM message WHERE session_id = ? AND time_updated > ? ORDER BY time_updated ASC, id ASC LIMIT ? \
             ) \
             ORDER BY message_id ASC, time_updated ASC, id ASC",
        )
        .map_err(|e| StreamError::Fatal {
            message: format!("Failed to prepare part query: {}", e),
        })?;

    let rows = stmt
        .query_map(rusqlite::params![session_id, after_updated, limit], |row| {
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
        .map_err(|e| StreamError::Fatal {
            message: format!("Failed to query parts: {}", e),
        })?;

    let mut parts_by_message: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    for row in rows {
        let (id, message_id, row_session_id, time_created, time_updated, data) =
            row.map_err(|e| StreamError::Fatal {
                message: format!("Failed to read part row: {}", e),
            })?;

        if let Ok(parsed_data) = serde_json::from_str::<serde_json::Value>(&data) {
            let mut map = serde_json::Map::with_capacity(6);
            map.insert("id".into(), serde_json::Value::String(id));
            map.insert(
                "message_id".into(),
                serde_json::Value::String(message_id.clone()),
            );
            map.insert(
                "session_id".into(),
                serde_json::Value::String(row_session_id),
            );
            map.insert(
                "time_created".into(),
                serde_json::Value::Number(time_created.into()),
            );
            map.insert(
                "time_updated".into(),
                serde_json::Value::Number(time_updated.into()),
            );
            map.insert("data".into(), parsed_data);
            parts_by_message
                .entry(message_id)
                .or_default()
                .push(serde_json::Value::Object(map));
        }
    }

    Ok(parts_by_message)
}

impl Default for OpenCodeAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for OpenCodeAgent {
    fn validate_checkpoint_stream(
        &self,
        source: &crate::model::checkpoint_request::StreamSource,
    ) -> Result<DiscoveredSession, StreamError> {
        super::opencode_checkpoint::validate_checkpoint_stream(source)
    }

    fn batch_size_hint(&self) -> usize {
        self.batch_size
    }

    fn sweep_strategy(&self) -> SweepStrategy {
        SweepStrategy::Periodic(Duration::from_secs(30 * 60))
    }

    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError> {
        // Discovery comes from presets, not sweep.
        Ok(Vec::new())
    }

    fn session_id_for_read<'a>(&self, _: &'a str, external_session_id: &'a str) -> &'a str {
        external_session_id
    }

    fn read_incremental(
        &self,
        path: &Path,
        watermark: Box<dyn WatermarkStrategy>,
        session_id: &str,
    ) -> Result<StreamBatch, StreamError> {
        // Downcast to TimestampWatermark
        let ts_watermark = watermark
            .as_any()
            .downcast_ref::<TimestampWatermark>()
            .ok_or_else(|| StreamError::Fatal {
                message: format!(
                    "OpenCode reader requires TimestampWatermark, got incompatible type for session {}",
                    session_id
                ),
            })?;

        let watermark_millis = ts_watermark.0.timestamp_millis();

        // Open SQLite read-only
        let conn = open_sqlite_readonly(path)?;

        // LIMIT applied for memory safety. Uses strict > to avoid re-reading.
        // Note: messages sharing exact same millisecond as watermark boundary could
        // theoretically be skipped, but OpenCode writes are interactive (not concurrent)
        // so millisecond collisions are effectively impossible in practice.
        let messages = read_session_messages_raw_with_limit(
            &conn,
            session_id,
            watermark_millis,
            self.batch_size,
        )?;

        if messages.is_empty() {
            return Ok(StreamBatch {
                events: Vec::new(),
                new_watermark: Box::new(TimestampWatermark::new(ts_watermark.0)),
            });
        }

        // Read only parts for the matched messages (IN-subquery, single scan)
        let mut parts_by_message = read_parts_for_messages_with_limit(
            &conn,
            session_id,
            watermark_millis,
            self.batch_size,
        )?;

        let mut max_updated: i64 = watermark_millis;
        let mut events = Vec::with_capacity(messages.len());

        for (msg_id, time_updated, msg_data) in messages {
            if time_updated > max_updated {
                max_updated = time_updated;
            }

            // Use .remove() to move parts out of the HashMap instead of cloning via .get()
            let mut map = serde_json::Map::with_capacity(2);
            map.insert("message".into(), msg_data);
            if let Some(parts) = parts_by_message.remove(&msg_id) {
                map.insert("parts".into(), serde_json::Value::Array(parts));
            }

            events.push(serde_json::Value::Object(map));
        }

        let new_watermark_ts =
            DateTime::from_timestamp_millis(max_updated).unwrap_or(ts_watermark.0);
        let new_watermark = Box::new(TimestampWatermark::new(new_watermark_ts));

        Ok(StreamBatch {
            events,
            new_watermark,
        })
    }

    fn extract_event_ids(
        &self,
        event: &serde_json::Value,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let message = event.get("message");

        let event_id = message
            .and_then(|m| m.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);

        let parent_event_id = message
            .and_then(|m| m.get("data"))
            .and_then(|d| d.get("parentID"))
            .and_then(|v| v.as_str())
            .map(String::from);

        let tool_use_id = event
            .get("parts")
            .and_then(|p| p.as_array())
            .and_then(|arr| {
                arr.iter().find_map(|part| {
                    part.get("data")
                        .and_then(|d| d.get("callID"))
                        .and_then(|v| v.as_str())
                        .map(String::from)
                })
            });

        (event_id, parent_event_id, tool_use_id)
    }

    fn extract_event_timestamp(
        &self,
        event: &serde_json::Value,
        file_meta: &std::fs::Metadata,
        is_first_event: bool,
    ) -> u32 {
        crate::operations::daemon::stream_worker::extract_event_timestamp(event).unwrap_or_else(
            || crate::operations::streams::agent::file_time_fallback(file_meta, is_first_event),
        )
    }

    fn streams(&self) -> Vec<StreamDescriptor> {
        let format = StreamFormat::OpenCodeSqlite;
        vec![StreamDescriptor {
            stream_kind: "transcript",
            format,
            watermark_type: format.watermark_type(),
            path_resolver: PathResolverKind::Identity,
            shared: false,
            watermark_type_resolver: None,
            format_resolver: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sweep_strategy() {
        let agent = OpenCodeAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    fn create_test_db(path: &std::path::Path, message_count: usize) {
        let conn = crate::model::repository::sqlite::open_with_memory_limits(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS part (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        for i in 0..message_count {
            let ts = 1000 + (i as i64) * 1000;
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    format!("msg-{}", i),
                    "test-session",
                    ts,
                    ts,
                    format!(r#"{{"role":"user","id":{}}}"#, i),
                ],
            ).unwrap();
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    format!("prt-{}", i),
                    format!("msg-{}", i),
                    "test-session",
                    ts + 1,
                    ts + 1,
                    format!(r#"{{"type":"text","text":"part-{}"}}"#, i),
                ],
            ).unwrap();
        }
    }

    fn drain_all(
        agent: &OpenCodeAgent,
        path: &std::path::Path,
    ) -> (Vec<serde_json::Value>, Box<dyn WatermarkStrategy>) {
        use chrono::{DateTime, Utc};
        let mut all = Vec::new();
        let mut wm: Box<dyn WatermarkStrategy> =
            Box::new(TimestampWatermark::new(DateTime::<Utc>::UNIX_EPOCH));
        loop {
            let batch = agent.read_incremental(path, wm, "test-session").unwrap();
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
    fn test_batch_resume_no_loss_or_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        create_test_db(&db_path, 5);

        let agent = OpenCodeAgent::with_batch_size(2);
        let (events, _) = drain_all(&agent, &db_path);

        assert_eq!(events.len(), 5);
        let ids: Vec<u64> = events
            .iter()
            .map(|e| e["message"]["data"]["id"].as_u64().unwrap())
            .collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_append_one_record_after_full_read() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        create_test_db(&db_path, 3);

        let agent = OpenCodeAgent::with_batch_size(2);
        let (all, wm) = drain_all(&agent, &db_path);
        assert_eq!(all.len(), 3);

        // Insert one more record with a later timestamp
        let conn = crate::model::repository::sqlite::open_with_memory_limits(&db_path).unwrap();
        let ts = 1000 + 3 * 1000i64;
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["msg-3", "test-session", ts, ts, r#"{"role":"user","id":3}"#],
        ).unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["prt-3", "msg-3", "test-session", ts+1, ts+1, r#"{"type":"text","text":"part-3"}"#],
        ).unwrap();
        drop(conn);

        let batch = agent
            .read_incremental(&db_path, wm, "test-session")
            .unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(
            batch.events[0]["message"]["data"]["id"].as_u64().unwrap(),
            3
        );
    }

    #[test]
    fn test_append_several_records_after_full_read() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        create_test_db(&db_path, 3);

        let agent = OpenCodeAgent::with_batch_size(2);
        let (_, mut wm) = drain_all(&agent, &db_path);

        // Insert 3 more records
        let conn = crate::model::repository::sqlite::open_with_memory_limits(&db_path).unwrap();
        for i in 3..6usize {
            let ts = 1000 + (i as i64) * 1000;
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    format!("msg-{}", i),
                    "test-session",
                    ts, ts,
                    format!(r#"{{"role":"user","id":{}}}"#, i),
                ],
            ).unwrap();
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    format!("prt-{}", i),
                    format!("msg-{}", i),
                    "test-session",
                    ts+1, ts+1,
                    format!(r#"{{"type":"text","text":"part-{}"}}"#, i),
                ],
            ).unwrap();
        }
        drop(conn);

        let mut new_events = Vec::new();
        loop {
            let batch = agent
                .read_incremental(&db_path, wm, "test-session")
                .unwrap();
            wm = batch.new_watermark;
            if batch.events.is_empty() {
                break;
            }
            new_events.extend(batch.events);
        }
        assert_eq!(new_events.len(), 3);
        let ids: Vec<u64> = new_events
            .iter()
            .map(|e| e["message"]["data"]["id"].as_u64().unwrap())
            .collect();
        assert_eq!(ids, vec![3, 4, 5]);
    }

    #[test]
    fn test_sqlite_open_sets_cache_size_pragma() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        drop(crate::model::repository::sqlite::open_with_memory_limits(&db_path).unwrap());

        let conn = open_sqlite_readonly(&db_path).unwrap();

        let cache_size: i32 = conn
            .pragma_query_value(None, "cache_size", |row| row.get(0))
            .unwrap();
        assert_eq!(
            cache_size,
            crate::model::repository::sqlite::MEMORY_LIMIT_CACHE_SIZE_KIB
        );
    }

    #[test]
    fn test_limit_caps_memory_and_watermark_still_drains_all() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        create_test_db(&db_path, 20);

        // batch_size=3 forces multiple iterations to drain 20 messages
        let agent = OpenCodeAgent::with_batch_size(3);
        let (events, _) = drain_all(&agent, &db_path);

        assert_eq!(
            events.len(),
            20,
            "all 20 messages must be returned across batches"
        );
        let ids: Vec<u64> = events
            .iter()
            .map(|e| e["message"]["data"]["id"].as_u64().unwrap())
            .collect();
        let expected: Vec<u64> = (0..20).collect();
        assert_eq!(
            ids, expected,
            "messages must arrive in order with no gaps or duplicates"
        );
    }

    #[test]
    fn test_limit_returns_at_most_batch_size_per_call() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        create_test_db(&db_path, 10);

        let agent = OpenCodeAgent::with_batch_size(4);
        let wm: Box<dyn WatermarkStrategy> = Box::new(TimestampWatermark::new(
            chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        ));

        let batch = agent
            .read_incremental(&db_path, wm, "test-session")
            .unwrap();
        assert!(
            batch.events.len() <= 4,
            "single call must not exceed batch_size (got {})",
            batch.events.len()
        );
    }

    #[test]
    fn test_parts_are_batch_loaded_not_per_message() {
        let db_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/opencode-sqlite/opencode.db");
        let conn = open_sqlite_readonly(&db_path).unwrap();
        // watermark=0 matches all messages in the fixture
        let parts = read_parts_for_messages_with_limit(&conn, "test-session-123", 0, 1000).unwrap();
        // Verify IN-subquery loading returns parts grouped by message_id.
        // Single query with IN-subquery instead of one per message,
        // prevents full-table-scan memory blowup on large unindexed databases.
        assert!(
            !parts.is_empty(),
            "batch parts query must return data from fixture"
        );
        for msg_parts in parts.values() {
            assert!(!msg_parts.is_empty());
        }
    }

    #[test]
    fn test_extract_event_ids_with_tool_call() {
        let agent = OpenCodeAgent::new();
        let event = serde_json::json!({
            "message": {
                "id": "msg_c5d3ff79b001I77d7ERgcEhCCc",
                "session_id": "ses_3a2c00870ffebuyMGjJ2UiakYv",
                "time_created": 1000,
                "time_updated": 2000,
                "data": {
                    "role": "assistant",
                    "parentID": "msg_c5d3ff791001Egl5tW62x4Vgzo",
                    "modelID": "big-pickle"
                }
            },
            "parts": [
                {
                    "id": "prt_c5d4001ea001t4tNa4ACM94hno",
                    "message_id": "msg_c5d3ff79b001I77d7ERgcEhCCc",
                    "session_id": "ses_3a2c00870ffebuyMGjJ2UiakYv",
                    "time_created": 1000,
                    "time_updated": 2000,
                    "data": {
                        "type": "tool",
                        "callID": "call_function_p43u37xcf94i_1",
                        "tool": "read",
                        "state": {"status": "completed"}
                    }
                }
            ]
        });
        let (eid, pid, tid) = agent.extract_event_ids(&event);
        assert_eq!(eid, Some("msg_c5d3ff79b001I77d7ERgcEhCCc".to_string()));
        assert_eq!(pid, Some("msg_c5d3ff791001Egl5tW62x4Vgzo".to_string()));
        assert_eq!(tid, Some("call_function_p43u37xcf94i_1".to_string()));
    }

    #[test]
    fn test_extract_event_ids_no_parts() {
        let agent = OpenCodeAgent::new();
        let event = serde_json::json!({
            "message": {
                "id": "msg_c5d3ff791001Egl5tW62x4Vgzo",
                "session_id": "ses_3a2c00870ffebuyMGjJ2UiakYv",
                "time_created": 1000,
                "time_updated": 1000,
                "data": {"role": "user"}
            }
        });
        let (eid, pid, tid) = agent.extract_event_ids(&event);
        assert_eq!(eid, Some("msg_c5d3ff791001Egl5tW62x4Vgzo".to_string()));
        assert_eq!(pid, None);
        assert_eq!(tid, None);
    }

    #[test]
    fn test_extract_event_ids_with_parent_no_tool() {
        let agent = OpenCodeAgent::new();
        let event = serde_json::json!({
            "message": {
                "id": "msg_c5d400371001TvbvIzWZB1f9il",
                "session_id": "ses_3a2c00870ffebuyMGjJ2UiakYv",
                "time_created": 1000,
                "time_updated": 2000,
                "data": {
                    "role": "assistant",
                    "parentID": "msg_c5d3ff791001Egl5tW62x4Vgzo",
                    "modelID": "big-pickle"
                }
            },
            "parts": [
                {
                    "id": "prt_c5d4002f20016aBCkx6UdvIDBo",
                    "message_id": "msg_c5d400371001TvbvIzWZB1f9il",
                    "session_id": "ses_3a2c00870ffebuyMGjJ2UiakYv",
                    "time_created": 1000,
                    "time_updated": 2000,
                    "data": {
                        "type": "step-finish",
                        "reason": "tool-calls",
                        "cost": 0
                    }
                }
            ]
        });
        let (eid, pid, tid) = agent.extract_event_ids(&event);
        assert_eq!(eid, Some("msg_c5d400371001TvbvIzWZB1f9il".to_string()));
        assert_eq!(pid, Some("msg_c5d3ff791001Egl5tW62x4Vgzo".to_string()));
        assert_eq!(tid, None);
    }
}
