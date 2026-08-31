use crate::streams::agents::opencode::open_sqlite_readonly;
use crate::streams::types::{StreamBatch, StreamError};
use crate::streams::watermark::{TimestampCursorWatermark, WatermarkStrategy};
use rusqlite::Connection;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

fn map_sqlite_error(e: rusqlite::Error, context: &str) -> StreamError {
    if let rusqlite::Error::SqliteFailure(ref err, _) = e
        && (err.code == rusqlite::ffi::ErrorCode::DatabaseBusy
            || err.code == rusqlite::ffi::ErrorCode::DatabaseLocked)
    {
        return StreamError::Transient {
            message: format!("{}: {}", context, e),
            retry_after: Duration::from_secs(2),
        };
    }
    StreamError::Fatal {
        message: format!("{}: {}", context, e),
    }
}

/// Read OTEL spans incrementally from a Copilot traces SQLite DB.
///
/// Uses keyset pagination on `(end_time_ms, span_id)` to prevent data loss
/// when multiple spans share the same `end_time_ms` at a batch boundary.
pub fn read_otel_spans_incremental(
    path: &Path,
    watermark: Box<dyn WatermarkStrategy>,
    batch_size: usize,
) -> Result<StreamBatch, StreamError> {
    let config = crate::config::Config::get();
    read_otel_spans_incremental_with(
        path,
        watermark,
        batch_size,
        config.max_transcript_batch_bytes() as u64,
        config.max_transcript_line_bytes(),
    )
}

fn read_otel_spans_incremental_with(
    path: &Path,
    watermark: Box<dyn WatermarkStrategy>,
    batch_size: usize,
    max_batch_bytes: u64,
    max_span_bytes: u64,
) -> Result<StreamBatch, StreamError> {
    let cursor = watermark
        .as_any()
        .downcast_ref::<TimestampCursorWatermark>()
        .ok_or_else(|| StreamError::Fatal {
            message: "OTEL stream requires TimestampCursorWatermark".to_string(),
        })?;

    let conn = open_sqlite_readonly(path)?;

    let mut spans = read_spans_after(&conn, cursor.timestamp_millis, &cursor.last_id, batch_size)?;
    if spans.is_empty() {
        return Ok(StreamBatch {
            events: vec![],
            new_watermark: Box::new(cursor.clone()),
        });
    }

    // Byte-bound the batch: the LIMIT above is count-only, but the attribute
    // and event TEXT payloads are unbounded, so a count-only batch could
    // materialize arbitrarily many MBs at once (#2244). Payload sizes come
    // from SQL LENGTH sums, so nothing is inflated for the estimate.
    let all_ids: Vec<&str> = spans.iter().map(|s| s.span_id.as_str()).collect();
    let payload_bytes = read_payload_bytes_for_spans(&conn, &all_ids)?;

    // A single span whose payload alone exceeds the per-line budget is
    // skipped outright: keyset pagination makes advancing past it exact,
    // and parsing it would defeat the byte bound.
    let first_bytes = payload_bytes.get(&spans[0].span_id).copied().unwrap_or(0);
    if first_bytes > max_span_bytes {
        let first = &spans[0];
        tracing::warn!(
            span_id = %first.span_id,
            payload_bytes = first_bytes,
            max_bytes = max_span_bytes,
            "skipping oversized OTEL span"
        );
        return Ok(StreamBatch {
            events: vec![],
            new_watermark: Box::new(TimestampCursorWatermark::new(
                first.end_time_ms,
                first.span_id.clone(),
            )),
        });
    }

    // Truncate at the batch byte budget (always keeping at least one span);
    // the remainder arrives next poll via the keyset cursor.
    let mut cumulative = 0u64;
    let mut included = 0usize;
    for span in &spans {
        let bytes = payload_bytes.get(&span.span_id).copied().unwrap_or(0);
        if included > 0 && cumulative + bytes > max_batch_bytes {
            break;
        }
        cumulative += bytes;
        included += 1;
    }
    spans.truncate(included);

    let span_ids: Vec<&str> = spans.iter().map(|s| s.span_id.as_str()).collect();
    let attributes = read_attributes_for_spans(&conn, &span_ids)?;
    let events = read_events_for_spans(&conn, &span_ids)?;

    let last_span = spans.last().unwrap();
    let new_watermark =
        TimestampCursorWatermark::new(last_span.end_time_ms, last_span.span_id.clone());

    let json_events: Vec<serde_json::Value> = spans
        .into_iter()
        .map(|span| {
            let span_attrs = attributes.get(&span.span_id).cloned().unwrap_or_default();
            let span_events = events.get(&span.span_id).cloned().unwrap_or_default();
            build_span_event_json(span, span_attrs, span_events)
        })
        .collect();

    Ok(StreamBatch {
        events: json_events,
        new_watermark: Box::new(new_watermark),
    })
}

struct SpanRow {
    span_id: String,
    trace_id: String,
    parent_span_id: Option<String>,
    name: String,
    start_time_ms: f64,
    end_time_ms: f64,
    status_code: i32,
    status_message: Option<String>,
    operation_name: Option<String>,
    provider_name: Option<String>,
    agent_name: Option<String>,
    conversation_id: Option<String>,
    request_model: Option<String>,
    response_model: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cached_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    tool_name: Option<String>,
    tool_call_id: Option<String>,
    tool_type: Option<String>,
    chat_session_id: Option<String>,
    turn_index: Option<i64>,
    ttft_ms: Option<f64>,
}

fn read_spans_after(
    conn: &Connection,
    after_ms: f64,
    after_id: &str,
    limit: usize,
) -> Result<Vec<SpanRow>, StreamError> {
    // Keyset pagination: skip spans at or before the cursor.
    // If after_id is empty (initial state), use simple `>` on timestamp.
    // Otherwise use compound `(ts > ?) OR (ts = ? AND id > ?)` to handle ties.
    // Only read spans that have at least one session identifier (chat_session_id
    // or conversation_id). Spans without either cannot be linked to a session.
    let session_filter = "(chat_session_id IS NOT NULL AND chat_session_id != '') \
                          OR (conversation_id IS NOT NULL AND conversation_id != '')";

    let (sql, params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = if after_id.is_empty() {
        (
            format!(
                "SELECT span_id, trace_id, parent_span_id, name, \
                 start_time_ms, end_time_ms, \
                 status_code, status_message, operation_name, provider_name, agent_name, \
                 conversation_id, request_model, response_model, input_tokens, output_tokens, \
                 cached_tokens, reasoning_tokens, tool_name, tool_call_id, tool_type, \
                 chat_session_id, turn_index, ttft_ms \
                 FROM spans WHERE end_time_ms > ?1 AND ({}) \
                 ORDER BY end_time_ms ASC, span_id ASC LIMIT ?2",
                session_filter
            ),
            vec![
                Box::new(after_ms) as Box<dyn rusqlite::types::ToSql>,
                Box::new(limit as i64),
            ],
        )
    } else {
        (
            format!(
                "SELECT span_id, trace_id, parent_span_id, name, \
                 start_time_ms, end_time_ms, \
                 status_code, status_message, operation_name, provider_name, agent_name, \
                 conversation_id, request_model, response_model, input_tokens, output_tokens, \
                 cached_tokens, reasoning_tokens, tool_name, tool_call_id, tool_type, \
                 chat_session_id, turn_index, ttft_ms \
                 FROM spans WHERE ((end_time_ms > ?1) OR (end_time_ms = ?2 AND span_id > ?3)) \
                 AND ({}) \
                 ORDER BY end_time_ms ASC, span_id ASC LIMIT ?4",
                session_filter
            ),
            vec![
                Box::new(after_ms) as Box<dyn rusqlite::types::ToSql>,
                Box::new(after_ms),
                Box::new(after_id.to_string()),
                Box::new(limit as i64),
            ],
        )
    };

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| map_sqlite_error(e, "Failed to prepare spans query"))?;

    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(SpanRow {
                span_id: row.get(0)?,
                trace_id: row.get(1)?,
                parent_span_id: row.get(2)?,
                name: row.get(3)?,
                start_time_ms: row.get(4)?,
                end_time_ms: row.get(5)?,
                status_code: row.get(6)?,
                status_message: row.get(7)?,
                operation_name: row.get(8)?,
                provider_name: row.get(9)?,
                agent_name: row.get(10)?,
                conversation_id: row.get(11)?,
                request_model: row.get(12)?,
                response_model: row.get(13)?,
                input_tokens: row.get(14)?,
                output_tokens: row.get(15)?,
                cached_tokens: row.get(16)?,
                reasoning_tokens: row.get(17)?,
                tool_name: row.get(18)?,
                tool_call_id: row.get(19)?,
                tool_type: row.get(20)?,
                chat_session_id: row.get(21)?,
                turn_index: row.get(22)?,
                ttft_ms: row.get(23)?,
            })
        })
        .map_err(|e| map_sqlite_error(e, "Failed to query spans"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| map_sqlite_error(e, "Failed to read span row"))
}

/// Approximate payload bytes per span: attribute values plus event attribute
/// blobs (the unbounded TEXT columns; the span row itself is small fixed
/// columns). Computed with SQL LENGTH sums so nothing is materialized.
fn read_payload_bytes_for_spans(
    conn: &Connection,
    span_ids: &[&str],
) -> Result<HashMap<String, u64>, StreamError> {
    let mut result: HashMap<String, u64> = HashMap::new();
    let placeholders: String = span_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    for (table, expr) in [
        ("span_attributes", "LENGTH(COALESCE(value, ''))"),
        ("span_events", "LENGTH(COALESCE(attributes, ''))"),
    ] {
        let sql = format!(
            "SELECT span_id, SUM({expr}) FROM {table} WHERE span_id IN ({placeholders}) GROUP BY span_id"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| map_sqlite_error(e, "Failed to prepare payload size query"))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(span_ids.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| map_sqlite_error(e, "Failed to query payload sizes"))?;
        for row in rows {
            let (span_id, bytes) =
                row.map_err(|e| map_sqlite_error(e, "Failed to read payload size row"))?;
            *result.entry(span_id).or_insert(0) += bytes.max(0) as u64;
        }
    }
    Ok(result)
}

fn read_attributes_for_spans(
    conn: &Connection,
    span_ids: &[&str],
) -> Result<HashMap<String, HashMap<String, String>>, StreamError> {
    if span_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders: String = span_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT span_id, key, value FROM span_attributes WHERE span_id IN ({})",
        placeholders
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| map_sqlite_error(e, "Failed to prepare attributes query"))?;

    let mut result: HashMap<String, HashMap<String, String>> = HashMap::new();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(span_ids.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|e| map_sqlite_error(e, "Failed to query attributes"))?;

    for row in rows {
        let (span_id, key, value) =
            row.map_err(|e| map_sqlite_error(e, "Failed to read attribute row"))?;
        if let Some(v) = value {
            result.entry(span_id).or_default().insert(key, v);
        }
    }
    Ok(result)
}

fn read_events_for_spans(
    conn: &Connection,
    span_ids: &[&str],
) -> Result<HashMap<String, Vec<serde_json::Value>>, StreamError> {
    if span_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders: String = span_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT span_id, name, CAST(timestamp_ms AS INTEGER), attributes FROM span_events \
         WHERE span_id IN ({}) ORDER BY timestamp_ms ASC",
        placeholders
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| map_sqlite_error(e, "Failed to prepare events query"))?;

    let mut result: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(span_ids.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|e| map_sqlite_error(e, "Failed to query events"))?;

    for row in rows {
        let (span_id, name, timestamp_ms, attributes_json) =
            row.map_err(|e| map_sqlite_error(e, "Failed to read event row"))?;
        let attrs: serde_json::Value = attributes_json
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null);
        result.entry(span_id).or_default().push(json!({
            "name": name,
            "timestamp_ms": timestamp_ms,
            "attributes": attrs,
        }));
    }
    Ok(result)
}

fn build_span_event_json(
    span: SpanRow,
    attributes: HashMap<String, String>,
    events: Vec<serde_json::Value>,
) -> serde_json::Value {
    json!({
        "span": {
            "span_id": span.span_id,
            "trace_id": span.trace_id,
            "parent_span_id": span.parent_span_id,
            "name": span.name,
            "start_time_ms": span.start_time_ms as i64,
            "end_time_ms": span.end_time_ms as i64,
            "status_code": span.status_code,
            "status_message": span.status_message,
            "operation_name": span.operation_name,
            "provider_name": span.provider_name,
            "agent_name": span.agent_name,
            "conversation_id": span.conversation_id,
            "request_model": span.request_model,
            "response_model": span.response_model,
            "input_tokens": span.input_tokens,
            "output_tokens": span.output_tokens,
            "cached_tokens": span.cached_tokens,
            "reasoning_tokens": span.reasoning_tokens,
            "tool_name": span.tool_name,
            "tool_call_id": span.tool_call_id,
            "tool_type": span.tool_type,
            "chat_session_id": span.chat_session_id,
            "turn_index": span.turn_index,
            "ttft_ms": span.ttft_ms,
        },
        "attributes": attributes,
        "events": events,
    })
}

/// Extract per-event IDs from an OTEL span event JSON.
/// Returns (event_id=span_id, parent_event_id=parent_span_id, tool_use_id=tool_call_id).
pub fn extract_otel_event_ids(
    event: &serde_json::Value,
) -> (Option<String>, Option<String>, Option<String>) {
    let span = event.get("span");
    let event_id = span
        .and_then(|s| s.get("span_id"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let parent_event_id = span
        .and_then(|s| s.get("parent_span_id"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let tool_use_id = span
        .and_then(|s| s.get("tool_call_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    (event_id, parent_event_id, tool_use_id)
}

/// Extract timestamp (as Unix seconds u32) from an OTEL span event JSON.
pub fn extract_otel_event_timestamp(event: &serde_json::Value) -> Option<u32> {
    event
        .get("span")
        .and_then(|s| s.get("start_time_ms"))
        .and_then(|v| v.as_i64())
        .map(|ms| (ms / 1000) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::watermark::TimestampCursorWatermark;
    use std::str::FromStr;

    fn create_test_otel_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("traces.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE spans (
                span_id TEXT PRIMARY KEY, trace_id TEXT NOT NULL, parent_span_id TEXT,
                name TEXT NOT NULL, start_time_ms INTEGER NOT NULL, end_time_ms INTEGER NOT NULL,
                status_code INTEGER NOT NULL DEFAULT 0, status_message TEXT,
                operation_name TEXT, provider_name TEXT, agent_name TEXT, conversation_id TEXT,
                request_model TEXT, response_model TEXT,
                input_tokens INTEGER, output_tokens INTEGER, cached_tokens INTEGER, reasoning_tokens INTEGER,
                tool_name TEXT, tool_call_id TEXT, tool_type TEXT,
                chat_session_id TEXT, turn_index INTEGER, ttft_ms REAL
            );
            CREATE TABLE span_attributes (
                span_id TEXT NOT NULL REFERENCES spans(span_id) ON DELETE CASCADE,
                key TEXT NOT NULL, value TEXT,
                PRIMARY KEY (span_id, key)
            );
            CREATE TABLE span_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                span_id TEXT NOT NULL REFERENCES spans(span_id) ON DELETE CASCADE,
                name TEXT NOT NULL, timestamp_ms INTEGER NOT NULL, attributes TEXT
            );",
        )
        .unwrap();
        (dir, db_path)
    }

    fn insert_span(
        conn: &rusqlite::Connection,
        span_id: &str,
        end_time_ms: i64,
        input_tokens: i64,
        output_tokens: i64,
    ) {
        conn.execute(
            "INSERT INTO spans (span_id, trace_id, name, start_time_ms, end_time_ms, status_code, \
             operation_name, provider_name, request_model, response_model, input_tokens, output_tokens, chat_session_id) \
             VALUES (?1, 'trace1', 'chat gpt-4.1', ?2, ?3, 0, 'chat', 'github', 'gpt-4.1', 'gpt-4.1-2025-04-14', ?4, ?5, 'session1')",
            rusqlite::params![span_id, end_time_ms - 1000, end_time_ms, input_tokens, output_tokens],
        )
        .unwrap();
    }

    fn insert_span_attr(conn: &rusqlite::Connection, span_id: &str, key: &str, value: &str) {
        conn.execute(
            "INSERT INTO span_attributes (span_id, key, value) VALUES (?1, ?2, ?3)",
            rusqlite::params![span_id, key, value],
        )
        .unwrap();
    }

    #[test]
    fn test_batch_truncates_at_byte_budget_and_resumes() {
        let (_dir, db_path) = create_test_otel_db();
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        for i in 1..=4 {
            let span_id = format!("span{}", i);
            insert_span(&conn, &span_id, i * 1000, 100, 50);
            insert_span_attr(&conn, &span_id, "payload", &"x".repeat(100));
        }
        drop(conn);

        // 250-byte batch budget fits two 100-byte payloads (the check runs
        // before the third would exceed it); per-span budget stays permissive.
        let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
        let batch = read_otel_spans_incremental_with(&db_path, watermark, 100, 250, 1024).unwrap();
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events[1]["span"]["span_id"], "span2");

        // The keyset watermark resumes exactly after the last included span.
        let batch = read_otel_spans_incremental_with(&db_path, batch.new_watermark, 100, 250, 1024)
            .unwrap();
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events[0]["span"]["span_id"], "span3");
        assert_eq!(batch.events[1]["span"]["span_id"], "span4");
    }

    #[test]
    fn test_oversized_span_is_skipped_with_cursor_advanced() {
        let (_dir, db_path) = create_test_otel_db();
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        insert_span(&conn, "giant", 1000, 100, 50);
        insert_span_attr(&conn, "giant", "payload", &"x".repeat(2000));
        insert_span(&conn, "normal", 2000, 100, 50);
        insert_span_attr(&conn, "normal", "payload", &"y".repeat(10));
        drop(conn);

        // The giant span exceeds the 1024-byte per-span budget: skipped, with
        // the cursor advanced past it so it is never re-read.
        let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
        let batch = read_otel_spans_incremental_with(&db_path, watermark, 100, 4096, 1024).unwrap();
        assert!(batch.events.is_empty());
        assert_eq!(batch.new_watermark.serialize(), "1000|giant");

        let batch =
            read_otel_spans_incremental_with(&db_path, batch.new_watermark, 100, 4096, 1024)
                .unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["span"]["span_id"], "normal");
    }

    #[test]
    fn test_empty_db_returns_empty_batch() {
        let (_dir, db_path) = create_test_otel_db();
        let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
        let batch = read_otel_spans_incremental(&db_path, watermark, 100).unwrap();
        assert!(batch.events.is_empty());
    }

    #[test]
    fn test_reads_spans_after_watermark() {
        let (_dir, db_path) = create_test_otel_db();
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        insert_span(&conn, "span1", 1000, 100, 50);
        insert_span(&conn, "span2", 2000, 200, 100);
        insert_span(&conn, "span3", 3000, 300, 150);
        drop(conn);

        let watermark: Box<dyn WatermarkStrategy> =
            Box::new(TimestampCursorWatermark::new(1000.0, "span1".to_string()));
        let batch = read_otel_spans_incremental(&db_path, watermark, 100).unwrap();
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events[0]["span"]["span_id"], "span2");
        assert_eq!(batch.events[1]["span"]["span_id"], "span3");
    }

    #[test]
    fn test_batch_size_limits_results() {
        let (_dir, db_path) = create_test_otel_db();
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        for i in 1..=5 {
            insert_span(&conn, &format!("span{}", i), i * 1000, i * 100, i * 50);
        }
        drop(conn);

        let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
        let batch = read_otel_spans_incremental(&db_path, watermark, 3).unwrap();
        assert_eq!(batch.events.len(), 3);
    }

    #[test]
    fn test_batch_resume_no_loss_no_repeats() {
        let (_dir, db_path) = create_test_otel_db();
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        for i in 1..=5 {
            insert_span(&conn, &format!("span{}", i), i * 1000, i * 100, i * 50);
        }
        drop(conn);

        let mut watermark: Box<dyn WatermarkStrategy> =
            Box::new(TimestampCursorWatermark::initial());
        let mut all_ids = Vec::new();

        loop {
            let batch = read_otel_spans_incremental(&db_path, watermark, 2).unwrap();
            if batch.events.is_empty() {
                break;
            }
            for ev in &batch.events {
                all_ids.push(ev["span"]["span_id"].as_str().unwrap().to_string());
            }
            watermark = batch.new_watermark;
        }

        assert_eq!(all_ids, vec!["span1", "span2", "span3", "span4", "span5"]);
    }

    #[test]
    fn test_no_data_loss_with_duplicate_end_time_ms() {
        let (_dir, db_path) = create_test_otel_db();
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        // 5 spans all sharing the same end_time_ms
        for i in 1..=5 {
            insert_span(&conn, &format!("span{}", i), 3000, i * 100, i * 50);
        }
        drop(conn);

        let mut watermark: Box<dyn WatermarkStrategy> =
            Box::new(TimestampCursorWatermark::initial());
        let mut all_ids = Vec::new();

        loop {
            let batch = read_otel_spans_incremental(&db_path, watermark, 2).unwrap();
            if batch.events.is_empty() {
                break;
            }
            for ev in &batch.events {
                all_ids.push(ev["span"]["span_id"].as_str().unwrap().to_string());
            }
            watermark = batch.new_watermark;
        }

        assert_eq!(all_ids, vec!["span1", "span2", "span3", "span4", "span5"]);
    }

    #[test]
    fn test_attributes_denormalized() {
        let (_dir, db_path) = create_test_otel_db();
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        insert_span(&conn, "span1", 1000, 100, 50);
        conn.execute(
            "INSERT INTO span_attributes (span_id, key, value) VALUES ('span1', 'gen_ai.request.model', 'gpt-4.1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO span_attributes (span_id, key, value) VALUES ('span1', 'gen_ai.agent.name', 'copilot')",
            [],
        )
        .unwrap();
        drop(conn);

        let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
        let batch = read_otel_spans_incremental(&db_path, watermark, 100).unwrap();
        assert_eq!(
            batch.events[0]["attributes"]["gen_ai.request.model"],
            "gpt-4.1"
        );
        assert_eq!(
            batch.events[0]["attributes"]["gen_ai.agent.name"],
            "copilot"
        );
    }

    #[test]
    fn test_span_events_denormalized() {
        let (_dir, db_path) = create_test_otel_db();
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        insert_span(&conn, "span1", 1000, 100, 50);
        conn.execute(
            "INSERT INTO span_events (span_id, name, timestamp_ms, attributes) VALUES ('span1', 'tool_call', 500, '{\"tool\":\"read_file\"}')",
            [],
        )
        .unwrap();
        drop(conn);

        let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
        let batch = read_otel_spans_incremental(&db_path, watermark, 100).unwrap();
        let events = batch.events[0]["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["name"], "tool_call");
        assert_eq!(events[0]["timestamp_ms"], 500);
        assert_eq!(events[0]["attributes"]["tool"], "read_file");
    }

    #[test]
    fn test_extract_event_ids() {
        let event = serde_json::json!({
            "span": {
                "span_id": "abc123",
                "parent_span_id": "parent456",
                "tool_call_id": "call789",
            },
            "attributes": {},
            "events": [],
        });
        let (event_id, parent_id, tool_use_id) = extract_otel_event_ids(&event);
        assert_eq!(event_id, Some("abc123".to_string()));
        assert_eq!(parent_id, Some("parent456".to_string()));
        assert_eq!(tool_use_id, Some("call789".to_string()));
    }

    #[test]
    fn test_extract_event_ids_empty_tool_call_id() {
        let event = serde_json::json!({
            "span": { "span_id": "abc", "parent_span_id": null, "tool_call_id": "" },
            "attributes": {},
            "events": [],
        });
        let (event_id, parent_id, tool_use_id) = extract_otel_event_ids(&event);
        assert_eq!(event_id, Some("abc".to_string()));
        assert_eq!(parent_id, None);
        assert_eq!(tool_use_id, None);
    }

    #[test]
    fn test_extract_event_timestamp() {
        let event = serde_json::json!({
            "span": { "start_time_ms": 1716556800000_i64 },
        });
        let ts = extract_otel_event_timestamp(&event);
        assert_eq!(ts, Some(1716556800));
    }

    #[test]
    fn test_reads_from_real_fixture() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/copilot-otel/traces.db");
        if !fixture_path.exists() {
            return;
        }
        let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
        let batch = read_otel_spans_incremental(&fixture_path, watermark, 100).unwrap();
        assert!(!batch.events.is_empty());
        let first = &batch.events[0];
        assert!(first.get("span").is_some());
        assert!(first.get("attributes").is_some());
        assert!(first.get("events").is_some());
        // Verify token fields are present
        assert!(first["span"].get("input_tokens").is_some());
        assert!(first["span"].get("output_tokens").is_some());
    }

    #[test]
    fn test_extract_event_session_id_chat_session_id() {
        use crate::streams::agent::Agent;
        use crate::streams::agents::CopilotAgent;

        let agent = CopilotAgent::new();
        let event = serde_json::json!({
            "span": {
                "chat_session_id": "chat-sess-123",
                "conversation_id": "conv-456",
            },
            "attributes": {},
            "events": [],
        });
        // Prefers chat_session_id over conversation_id
        assert_eq!(
            agent.extract_event_session_id(&event),
            Some("chat-sess-123".to_string())
        );
    }

    #[test]
    fn test_extract_event_session_id_fallback_to_conversation_id() {
        use crate::streams::agent::Agent;
        use crate::streams::agents::CopilotAgent;

        let agent = CopilotAgent::new();
        let event = serde_json::json!({
            "span": {
                "chat_session_id": null,
                "conversation_id": "conv-789",
            },
            "attributes": {},
            "events": [],
        });
        assert_eq!(
            agent.extract_event_session_id(&event),
            Some("conv-789".to_string())
        );
    }

    #[test]
    fn test_extract_event_session_id_empty_strings_return_none() {
        use crate::streams::agent::Agent;
        use crate::streams::agents::CopilotAgent;

        let agent = CopilotAgent::new();
        let event = serde_json::json!({
            "span": {
                "chat_session_id": "",
                "conversation_id": "",
            },
            "attributes": {},
            "events": [],
        });
        assert_eq!(agent.extract_event_session_id(&event), None);
    }

    #[test]
    fn test_extract_event_session_id_no_span_key() {
        use crate::streams::agent::Agent;
        use crate::streams::agents::CopilotAgent;

        let agent = CopilotAgent::new();
        let event = serde_json::json!({"type": "user", "content": "hello"});
        assert_eq!(agent.extract_event_session_id(&event), None);
    }

    #[test]
    fn test_extract_event_session_id_missing_both_fields() {
        use crate::streams::agent::Agent;
        use crate::streams::agents::CopilotAgent;

        let agent = CopilotAgent::new();
        let event = serde_json::json!({
            "span": {
                "span_id": "abc",
                "trace_id": "t1",
            },
            "attributes": {},
            "events": [],
        });
        assert_eq!(agent.extract_event_session_id(&event), None);
    }

    #[test]
    fn test_extract_event_session_id_empty_chat_falls_to_conversation() {
        use crate::streams::agent::Agent;
        use crate::streams::agents::CopilotAgent;

        let agent = CopilotAgent::new();
        let event = serde_json::json!({
            "span": {
                "chat_session_id": "",
                "conversation_id": "conv-fallback",
            },
            "attributes": {},
            "events": [],
        });
        assert_eq!(
            agent.extract_event_session_id(&event),
            Some("conv-fallback".to_string())
        );
    }

    #[test]
    fn test_spans_without_session_ids_are_filtered() {
        let (_dir, db_path) = create_test_otel_db();
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();

        // Span WITH session ID (should be included)
        conn.execute(
            "INSERT INTO spans (span_id, trace_id, name, start_time_ms, end_time_ms, status_code, \
             chat_session_id, conversation_id) \
             VALUES ('has-session', 'trace1', 'chat', 1000, 2000, 0, 'sess-1', NULL)",
            [],
        )
        .unwrap();

        // Span with only conversation_id (should be included)
        conn.execute(
            "INSERT INTO spans (span_id, trace_id, name, start_time_ms, end_time_ms, status_code, \
             chat_session_id, conversation_id) \
             VALUES ('has-conv-only', 'trace1', 'chat', 2000, 3000, 0, NULL, 'conv-1')",
            [],
        )
        .unwrap();

        // Span WITHOUT any session ID (should be excluded by SQL filter)
        conn.execute(
            "INSERT INTO spans (span_id, trace_id, name, start_time_ms, end_time_ms, status_code, \
             chat_session_id, conversation_id) \
             VALUES ('no-session', 'trace1', 'chat', 3000, 4000, 0, NULL, NULL)",
            [],
        )
        .unwrap();

        // Span with empty strings (should be excluded)
        conn.execute(
            "INSERT INTO spans (span_id, trace_id, name, start_time_ms, end_time_ms, status_code, \
             chat_session_id, conversation_id) \
             VALUES ('empty-session', 'trace1', 'chat', 4000, 5000, 0, '', '')",
            [],
        )
        .unwrap();

        drop(conn);

        let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
        let batch = read_otel_spans_incremental(&db_path, watermark, 100).unwrap();

        assert_eq!(
            batch.events.len(),
            2,
            "only spans with session IDs should be returned"
        );
        let ids: Vec<&str> = batch
            .events
            .iter()
            .map(|e| e["span"]["span_id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"has-session"));
        assert!(ids.contains(&"has-conv-only"));
    }

    #[test]
    fn test_watermark_advances_correctly_after_batch() {
        let (_dir, db_path) = create_test_otel_db();
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        insert_span(&conn, "span-a", 5000, 100, 50);
        insert_span(&conn, "span-b", 7000, 200, 100);
        drop(conn);

        let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
        let batch = read_otel_spans_incremental(&db_path, watermark, 100).unwrap();

        // Watermark should point to last span
        let new_wm = batch
            .new_watermark
            .as_any()
            .downcast_ref::<TimestampCursorWatermark>()
            .unwrap();
        assert_eq!(new_wm.timestamp_millis, 7000.0);
        assert_eq!(new_wm.last_id, "span-b");
    }

    #[test]
    fn test_map_sqlite_error_busy_is_transient() {
        let err = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                extended_code: 5,
            },
            Some("database is locked".to_string()),
        );
        let result = super::map_sqlite_error(err, "test operation");
        match result {
            StreamError::Transient {
                message,
                retry_after,
            } => {
                assert!(message.contains("test operation"));
                assert!(message.contains("database is locked"));
                assert_eq!(retry_after, Duration::from_secs(2));
            }
            other => panic!("Expected Transient, got {:?}", other),
        }
    }

    #[test]
    fn test_map_sqlite_error_other_is_fatal() {
        let err = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseCorrupt,
                extended_code: 11,
            },
            Some("database disk image is malformed".to_string()),
        );
        let result = super::map_sqlite_error(err, "test operation");
        match result {
            StreamError::Fatal { message } => {
                assert!(message.contains("test operation"));
                assert!(message.contains("malformed"));
            }
            other => panic!("Expected Fatal, got {:?}", other),
        }
    }

    #[test]
    fn test_otel_json_structure_has_all_span_fields() {
        let (_dir, db_path) = create_test_otel_db();
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute(
            "INSERT INTO spans (span_id, trace_id, parent_span_id, name, start_time_ms, end_time_ms, \
             status_code, status_message, operation_name, provider_name, agent_name, \
             conversation_id, request_model, response_model, input_tokens, output_tokens, \
             cached_tokens, reasoning_tokens, tool_name, tool_call_id, tool_type, \
             chat_session_id, turn_index, ttft_ms) \
             VALUES ('full-span', 'trace-abc', 'parent-123', 'chat gpt-4.1', 1000, 2000, \
             1, 'OK', 'chat', 'openai', 'copilot-agent', \
             'conv-1', 'gpt-4.1', 'gpt-4.1-2025-04-14', 500, 200, \
             100, 50, 'read_file', 'call-xyz', 'function', \
             'session-abc', 3, 125.5)",
            [],
        ).unwrap();
        drop(conn);

        let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
        let batch = read_otel_spans_incremental(&db_path, watermark, 100).unwrap();
        assert_eq!(batch.events.len(), 1);

        let span = &batch.events[0]["span"];
        assert_eq!(span["span_id"], "full-span");
        assert_eq!(span["trace_id"], "trace-abc");
        assert_eq!(span["parent_span_id"], "parent-123");
        assert_eq!(span["name"], "chat gpt-4.1");
        assert_eq!(span["start_time_ms"], 1000);
        assert_eq!(span["end_time_ms"], 2000);
        assert_eq!(span["status_code"], 1);
        assert_eq!(span["status_message"], "OK");
        assert_eq!(span["operation_name"], "chat");
        assert_eq!(span["provider_name"], "openai");
        assert_eq!(span["agent_name"], "copilot-agent");
        assert_eq!(span["conversation_id"], "conv-1");
        assert_eq!(span["request_model"], "gpt-4.1");
        assert_eq!(span["response_model"], "gpt-4.1-2025-04-14");
        assert_eq!(span["input_tokens"], 500);
        assert_eq!(span["output_tokens"], 200);
        assert_eq!(span["cached_tokens"], 100);
        assert_eq!(span["reasoning_tokens"], 50);
        assert_eq!(span["tool_name"], "read_file");
        assert_eq!(span["tool_call_id"], "call-xyz");
        assert_eq!(span["tool_type"], "function");
        assert_eq!(span["chat_session_id"], "session-abc");
        assert_eq!(span["turn_index"], 3);
        assert_eq!(span["ttft_ms"], 125.5);
    }

    #[test]
    fn test_initial_watermark_uses_simple_greater_than() {
        let (_dir, db_path) = create_test_otel_db();
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        // Span at time 0 should still be found with initial watermark (end_time_ms > 0 fails for this!)
        // Actually initial watermark is timestamp_millis=0, so end_time_ms > 0 catches spans at ms=1+
        // Span at ms=0 would NOT be found since > 0 excludes it. This is OK since ms=0 means epoch.
        insert_span(&conn, "span-early", 1, 10, 5);
        insert_span(&conn, "span-at-zero", 0, 10, 5);
        drop(conn);

        let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
        let batch = read_otel_spans_incremental(&db_path, watermark, 100).unwrap();
        // span-at-zero has end_time_ms=0, initial watermark is > 0, so it's excluded
        // However span-at-zero also needs a session ID to pass the filter
        // Our insert_span helper sets chat_session_id='session1', so it passes the session filter
        // But end_time_ms=0 is NOT > 0, so it's excluded from the initial query
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["span"]["span_id"], "span-early");
    }

    #[test]
    fn test_fractional_real_end_time_ms_no_infinite_loop() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("traces.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE spans (
                span_id TEXT PRIMARY KEY, trace_id TEXT NOT NULL, parent_span_id TEXT,
                name TEXT NOT NULL, start_time_ms REAL NOT NULL, end_time_ms REAL NOT NULL,
                status_code INTEGER NOT NULL DEFAULT 0, status_message TEXT,
                operation_name TEXT, provider_name TEXT, agent_name TEXT, conversation_id TEXT,
                request_model TEXT, response_model TEXT,
                input_tokens INTEGER, output_tokens INTEGER, cached_tokens INTEGER, reasoning_tokens INTEGER,
                tool_name TEXT, tool_call_id TEXT, tool_type TEXT,
                chat_session_id TEXT, turn_index INTEGER, ttft_ms REAL
            );
            CREATE TABLE span_attributes (
                span_id TEXT NOT NULL REFERENCES spans(span_id) ON DELETE CASCADE,
                key TEXT NOT NULL, value TEXT,
                PRIMARY KEY (span_id, key)
            );
            CREATE TABLE span_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                span_id TEXT NOT NULL REFERENCES spans(span_id) ON DELETE CASCADE,
                name TEXT NOT NULL, timestamp_ms INTEGER NOT NULL, attributes TEXT
            );",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO spans (span_id, trace_id, name, start_time_ms, end_time_ms, \
             status_code, chat_session_id) \
             VALUES ('span1', 'trace1', 'chat', 1000.16, 2000.35, 0, 'session1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO spans (span_id, trace_id, name, start_time_ms, end_time_ms, \
             status_code, chat_session_id) \
             VALUES ('span2', 'trace1', 'chat', 2000.50, 3000.94, 0, 'session1')",
            [],
        )
        .unwrap();
        drop(conn);

        // First read: get both spans
        let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
        let batch = read_otel_spans_incremental(&db_path, watermark, 100).unwrap();
        assert_eq!(batch.events.len(), 2);

        // Watermark should store the exact fractional value
        let new_wm = batch
            .new_watermark
            .as_any()
            .downcast_ref::<TimestampCursorWatermark>()
            .unwrap();
        assert_eq!(new_wm.timestamp_millis, 3000.94);
        assert_eq!(new_wm.last_id, "span2");

        // Second read with advanced watermark: should get 0 spans (not loop forever)
        let batch2 = read_otel_spans_incremental(&db_path, batch.new_watermark, 100).unwrap();
        assert_eq!(batch2.events.len(), 0);
    }

    #[test]
    fn test_fractional_real_watermark_roundtrip_prevents_reread() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("traces.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE spans (
                span_id TEXT PRIMARY KEY, trace_id TEXT NOT NULL, parent_span_id TEXT,
                name TEXT NOT NULL, start_time_ms REAL NOT NULL, end_time_ms REAL NOT NULL,
                status_code INTEGER NOT NULL DEFAULT 0, status_message TEXT,
                operation_name TEXT, provider_name TEXT, agent_name TEXT, conversation_id TEXT,
                request_model TEXT, response_model TEXT,
                input_tokens INTEGER, output_tokens INTEGER, cached_tokens INTEGER, reasoning_tokens INTEGER,
                tool_name TEXT, tool_call_id TEXT, tool_type TEXT,
                chat_session_id TEXT, turn_index INTEGER, ttft_ms REAL
            );
            CREATE TABLE span_attributes (
                span_id TEXT NOT NULL REFERENCES spans(span_id) ON DELETE CASCADE,
                key TEXT NOT NULL, value TEXT,
                PRIMARY KEY (span_id, key)
            );
            CREATE TABLE span_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                span_id TEXT NOT NULL REFERENCES spans(span_id) ON DELETE CASCADE,
                name TEXT NOT NULL, timestamp_ms INTEGER NOT NULL, attributes TEXT
            );",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO spans (span_id, trace_id, name, start_time_ms, end_time_ms, \
             status_code, chat_session_id) \
             VALUES ('span-frac', 'trace1', 'chat', 1780519329188.16, 1780519329188.35, 0, 'session1')",
            [],
        )
        .unwrap();
        drop(conn);

        // Read the span
        let watermark: Box<dyn WatermarkStrategy> = Box::new(TimestampCursorWatermark::initial());
        let batch = read_otel_spans_incremental(&db_path, watermark, 100).unwrap();
        assert_eq!(batch.events.len(), 1);

        // Simulate serialization roundtrip (as would happen when persisted to streams DB)
        let wm = batch
            .new_watermark
            .as_any()
            .downcast_ref::<TimestampCursorWatermark>()
            .unwrap();
        let serialized = wm.serialize();
        let deserialized = TimestampCursorWatermark::from_str(&serialized).unwrap();
        assert_eq!(deserialized.timestamp_millis, 1780519329188.35);

        // Re-read with roundtripped watermark: must NOT re-read the same span
        let restored_wm: Box<dyn WatermarkStrategy> = Box::new(deserialized);
        let batch2 = read_otel_spans_incremental(&db_path, restored_wm, 100).unwrap();
        assert_eq!(batch2.events.len(), 0);
    }
}
