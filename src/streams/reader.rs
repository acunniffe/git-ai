//! Shared bounded JSONL reader for line-oriented transcript agents.
//!
//! Every JSONL agent (claude, codex, cursor, gemini, pi, windsurf, droid,
//! copilot, copilot_cli) reads through [`read_jsonl_event_stream`], so the
//! per-line and per-batch byte budgets from config apply to all of them —
//! no reader can opt out of bounded ingestion (#2244).

use std::path::Path;

use crate::streams::types::{JsonlLineState, StreamBatch, StreamError, read_jsonl_line};
use crate::streams::watermark::{ByteOffsetWatermark, HybridWatermark, WatermarkStrategy};

/// Watermark flavor produced by [`read_jsonl_event_stream`].
pub(crate) enum JsonlWatermarkMode {
    /// Plain byte offset.
    ByteOffset,
    /// Byte offset + emitted-record count + max RFC3339 `timestamp` field (droid).
    Hybrid,
}

pub(crate) struct JsonlReadOptions<'a> {
    /// Agent name used in error and log messages.
    pub agent_label: &'a str,
    /// Event-count cap per batch (`Agent::batch_size_hint`).
    pub batch_limit: usize,
    pub mode: JsonlWatermarkMode,
    /// Post-parse filter: events returning `false` consume offset bytes but
    /// are neither emitted nor counted as records (droid keeps only
    /// `type == "message"` entries).
    pub event_filter: Option<&'a dyn Fn(&serde_json::Value) -> bool>,
}

/// Read a JSONL event stream incrementally from the watermark position.
///
/// Lines beyond `max_transcript_line_bytes` are skipped (never buffered) with
/// the watermark advanced past them; the batch stops once it holds
/// `batch_limit` events or `max_transcript_batch_bytes` of raw line bytes,
/// whichever comes first. Remaining events arrive in later batches.
pub(crate) fn read_jsonl_event_stream(
    path: &Path,
    watermark: Box<dyn WatermarkStrategy>,
    session_id: &str,
    opts: &JsonlReadOptions<'_>,
) -> Result<StreamBatch, StreamError> {
    let config = crate::config::Config::get();
    read_jsonl_event_stream_with(
        path,
        watermark,
        session_id,
        opts,
        config.max_transcript_line_bytes(),
        config.max_transcript_batch_bytes(),
    )
}

fn read_jsonl_event_stream_with(
    path: &Path,
    watermark: Box<dyn WatermarkStrategy>,
    session_id: &str,
    opts: &JsonlReadOptions<'_>,
    max_line_bytes: u64,
    max_batch_bytes: usize,
) -> Result<StreamBatch, StreamError> {
    use std::fs::File;
    use std::io::{BufReader, Seek, SeekFrom};

    // Resolve the starting position (and hybrid state) from the watermark.
    let (start_offset, mut record_count, mut latest_timestamp) = match opts.mode {
        JsonlWatermarkMode::ByteOffset => {
            let byte_watermark = watermark
                .as_any()
                .downcast_ref::<ByteOffsetWatermark>()
                .ok_or_else(|| StreamError::Fatal {
                    message: format!(
                        "{} reader requires ByteOffsetWatermark, got incompatible type for session {}",
                        opts.agent_label, session_id
                    ),
                })?;
            (byte_watermark.0, 0, None)
        }
        JsonlWatermarkMode::Hybrid => {
            let hybrid_watermark = watermark
                .as_any()
                .downcast_ref::<HybridWatermark>()
                .ok_or_else(|| StreamError::Fatal {
                    message: format!(
                        "{} reader requires HybridWatermark, got incompatible type for session {}",
                        opts.agent_label, session_id
                    ),
                })?;
            (
                hybrid_watermark.offset,
                hybrid_watermark.record,
                hybrid_watermark.timestamp,
            )
        }
    };

    let file = File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            StreamError::Fatal {
                message: format!("Transcript file not found: {}", path.display()),
            }
        } else if e.kind() == std::io::ErrorKind::PermissionDenied {
            StreamError::Fatal {
                message: format!("Permission denied reading transcript: {}", path.display()),
            }
        } else {
            StreamError::Transient {
                message: format!("Failed to open transcript file: {}", e),
                retry_after: std::time::Duration::from_secs(5),
            }
        }
    })?;

    let mut reader = BufReader::new(file);
    reader
        .seek(SeekFrom::Start(start_offset))
        .map_err(|e| StreamError::Transient {
            message: format!("Failed to seek to offset {}: {}", start_offset, e),
            retry_after: std::time::Duration::from_secs(5),
        })?;

    let mut events = Vec::with_capacity(opts.batch_limit);
    let mut current_offset = start_offset;
    let mut batch_bytes: usize = 0;
    let mut line_number = 0;

    let mut line = String::new();
    loop {
        match read_jsonl_line(&mut reader, &mut line, max_line_bytes).map_err(|e| {
            StreamError::Transient {
                message: format!("I/O error reading line: {}", e),
                retry_after: std::time::Duration::from_secs(5),
            }
        })? {
            JsonlLineState::Eof => break,
            JsonlLineState::Partial => break,
            JsonlLineState::Complete(bytes_read) => {
                line_number += 1;
                current_offset += bytes_read as u64;
            }
            JsonlLineState::Oversized(bytes_read) => {
                // The line was skipped without being buffered; advance the
                // watermark past it so it is never re-read.
                line_number += 1;
                current_offset += bytes_read as u64;
                tracing::warn!(
                    line = line_number,
                    path = %path.display(),
                    agent = opts.agent_label,
                    max_bytes = max_line_bytes,
                    "skipping oversized transcript line"
                );
                continue;
            }
        }

        if line.trim().is_empty() {
            continue;
        }

        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    line = line_number,
                    path = %path.display(),
                    error = %e,
                    "skipping malformed JSON line"
                );
                continue;
            }
        };

        if let Some(filter) = opts.event_filter
            && !filter(&entry)
        {
            continue;
        }

        if matches!(opts.mode, JsonlWatermarkMode::Hybrid) {
            record_count += 1;
            if let Some(ts_str) = entry["timestamp"].as_str()
                && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str)
            {
                let utc_dt = dt.with_timezone(&chrono::Utc);
                if latest_timestamp.is_none() || Some(utc_dt) > latest_timestamp {
                    latest_timestamp = Some(utc_dt);
                }
            }
        }

        batch_bytes += line.len();
        events.push(entry);
        if events.len() >= opts.batch_limit || batch_bytes >= max_batch_bytes {
            break;
        }
    }

    let new_watermark: Box<dyn WatermarkStrategy> = match opts.mode {
        JsonlWatermarkMode::ByteOffset => Box::new(ByteOffsetWatermark::new(current_offset)),
        JsonlWatermarkMode::Hybrid => Box::new(HybridWatermark::new(
            current_offset,
            record_count,
            latest_timestamp,
        )),
    };

    Ok(StreamBatch {
        events,
        new_watermark,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const TEST_LINE_CAP: u64 = 256;
    const TEST_BATCH_CAP: usize = 1024 * 1024;

    fn byte_offset_opts(batch_limit: usize) -> JsonlReadOptions<'static> {
        JsonlReadOptions {
            agent_label: "Test",
            batch_limit,
            mode: JsonlWatermarkMode::ByteOffset,
            event_filter: None,
        }
    }

    fn write_temp(content: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{content}").unwrap();
        file.flush().unwrap();
        file
    }

    fn read(
        file: &tempfile::NamedTempFile,
        watermark: Box<dyn WatermarkStrategy>,
        opts: &JsonlReadOptions<'_>,
        max_line_bytes: u64,
        max_batch_bytes: usize,
    ) -> StreamBatch {
        read_jsonl_event_stream_with(
            file.path(),
            watermark,
            "test-session",
            opts,
            max_line_bytes,
            max_batch_bytes,
        )
        .unwrap()
    }

    #[test]
    fn test_byte_offset_read_and_resume() {
        let file = write_temp("{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n");
        let opts = byte_offset_opts(2);

        let batch = read(
            &file,
            Box::new(ByteOffsetWatermark::new(0)),
            &opts,
            TEST_LINE_CAP,
            TEST_BATCH_CAP,
        );
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events[0]["a"], 1);

        // Resume from the returned watermark: only the third event remains.
        let batch = read(
            &file,
            batch.new_watermark,
            &opts,
            TEST_LINE_CAP,
            TEST_BATCH_CAP,
        );
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["c"], 3);
        assert_eq!(batch.new_watermark.serialize(), "24");
    }

    #[test]
    fn test_oversized_line_skipped_watermark_lands_after_it() {
        let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(TEST_LINE_CAP as usize));
        let content = format!("{{\"a\":1}}\n{big}\n{{\"b\":2}}\n");
        let file = write_temp(&content);
        let opts = byte_offset_opts(10);

        let batch = read(
            &file,
            Box::new(ByteOffsetWatermark::new(0)),
            &opts,
            TEST_LINE_CAP,
            TEST_BATCH_CAP,
        );
        // The giant line is skipped, its neighbors survive, and the watermark
        // covers the entire file so nothing is re-read.
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events[0]["a"], 1);
        assert_eq!(batch.events[1]["b"], 2);
        assert_eq!(batch.new_watermark.serialize(), content.len().to_string());
    }

    #[test]
    fn test_batch_stops_at_byte_budget_with_correct_watermark() {
        // Each line is 12 bytes; a 30-byte batch budget fits 3 lines
        // (the budget check runs after the third push).
        let content = "{\"n\":10000}\n".repeat(5);
        let file = write_temp(&content);
        let opts = byte_offset_opts(100);

        let batch = read(
            &file,
            Box::new(ByteOffsetWatermark::new(0)),
            &opts,
            TEST_LINE_CAP,
            30,
        );
        assert_eq!(batch.events.len(), 3);
        assert_eq!(batch.new_watermark.serialize(), "36");

        // The next poll picks up the remaining two events.
        let batch = read(&file, batch.new_watermark, &opts, TEST_LINE_CAP, 30);
        assert_eq!(batch.events.len(), 2);
    }

    #[test]
    fn test_malformed_and_empty_lines_skipped() {
        let file = write_temp("{\"a\":1}\n\nnot json\n{\"b\":2}\n");
        let opts = byte_offset_opts(10);

        let batch = read(
            &file,
            Box::new(ByteOffsetWatermark::new(0)),
            &opts,
            TEST_LINE_CAP,
            TEST_BATCH_CAP,
        );
        assert_eq!(batch.events.len(), 2);
    }

    #[test]
    fn test_partial_trailing_line_not_consumed() {
        let file = write_temp("{\"a\":1}\n{\"b\":");
        let opts = byte_offset_opts(10);

        let batch = read(
            &file,
            Box::new(ByteOffsetWatermark::new(0)),
            &opts,
            TEST_LINE_CAP,
            TEST_BATCH_CAP,
        );
        assert_eq!(batch.events.len(), 1);
        // Watermark stops before the partial line so it is re-read once complete.
        assert_eq!(batch.new_watermark.serialize(), "8");
    }

    #[test]
    fn test_filter_advances_offset_without_emitting() {
        let content = "{\"type\":\"todo\"}\n{\"type\":\"message\",\"v\":1}\n";
        let file = write_temp(content);
        let filter = |entry: &serde_json::Value| entry["type"].as_str() == Some("message");
        let opts = JsonlReadOptions {
            agent_label: "Test",
            batch_limit: 10,
            mode: JsonlWatermarkMode::Hybrid,
            event_filter: Some(&filter),
        };

        let batch = read(
            &file,
            Box::new(HybridWatermark::new(0, 0, None)),
            &opts,
            TEST_LINE_CAP,
            TEST_BATCH_CAP,
        );
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["v"], 1);

        let hybrid = batch
            .new_watermark
            .as_any()
            .downcast_ref::<HybridWatermark>()
            .unwrap();
        // Filtered lines advance the offset but do not count as records.
        assert_eq!(hybrid.offset, content.len() as u64);
        assert_eq!(hybrid.record, 1);
    }

    #[test]
    fn test_hybrid_tracks_latest_rfc3339_timestamp() {
        let content = "{\"type\":\"message\",\"timestamp\":\"2026-01-02T00:00:00Z\"}\n\
                       {\"type\":\"message\",\"timestamp\":\"2026-01-01T00:00:00Z\"}\n";
        let file = write_temp(content);
        let opts = JsonlReadOptions {
            agent_label: "Test",
            batch_limit: 10,
            mode: JsonlWatermarkMode::Hybrid,
            event_filter: None,
        };

        let batch = read(
            &file,
            Box::new(HybridWatermark::new(0, 0, None)),
            &opts,
            TEST_LINE_CAP,
            TEST_BATCH_CAP,
        );
        let hybrid = batch
            .new_watermark
            .as_any()
            .downcast_ref::<HybridWatermark>()
            .unwrap();
        assert_eq!(hybrid.record, 2);
        // The max timestamp wins even when a later line is older.
        assert_eq!(
            hybrid.timestamp.unwrap().to_rfc3339(),
            "2026-01-02T00:00:00+00:00"
        );
    }

    #[test]
    fn test_wrong_watermark_type_is_fatal() {
        let file = write_temp("{\"a\":1}\n");
        let opts = byte_offset_opts(10);

        let Err(err) = read_jsonl_event_stream_with(
            file.path(),
            Box::new(HybridWatermark::new(0, 0, None)),
            "test-session",
            &opts,
            TEST_LINE_CAP,
            TEST_BATCH_CAP,
        ) else {
            panic!("expected a watermark-type error");
        };
        assert!(matches!(err, StreamError::Fatal { .. }));
    }

    #[test]
    fn test_missing_file_is_fatal() {
        let opts = byte_offset_opts(10);
        let Err(err) = read_jsonl_event_stream_with(
            Path::new("/nonexistent/transcript.jsonl"),
            Box::new(ByteOffsetWatermark::new(0)),
            "test-session",
            &opts,
            TEST_LINE_CAP,
            TEST_BATCH_CAP,
        ) else {
            panic!("expected a missing-file error");
        };
        assert!(matches!(err, StreamError::Fatal { .. }));
    }
}
