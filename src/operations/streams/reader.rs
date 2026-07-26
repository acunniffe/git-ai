//! Shared incremental transcript reader mechanics.

use crate::model::stream_types::{JsonlLineState, StreamBatch, StreamError, read_jsonl_line};
use crate::model::stream_watermark::{
    ByteOffsetWatermark, RecordIndexWatermark, WatermarkStrategy,
};
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::Path;

fn transcript_open_error(path: &Path, error: std::io::Error, open_error_verb: &str) -> StreamError {
    match error.kind() {
        std::io::ErrorKind::NotFound => StreamError::Fatal {
            message: format!("Transcript file not found: {}", path.display()),
        },
        std::io::ErrorKind::PermissionDenied => StreamError::Fatal {
            message: format!("Permission denied reading transcript: {}", path.display()),
        },
        _ => StreamError::Transient {
            message: format!("Failed to {} transcript file: {}", open_error_verb, error),
            retry_after: std::time::Duration::from_secs(5),
        },
    }
}

/// Object-backed JSON array reader policy.
#[derive(Clone, Copy)]
pub(super) enum RecordIndexAdvancePolicy {
    OriginalU64,
    ConvertedUsize,
}

pub(super) struct JsonArrayStreamSpec<'a> {
    reader_name: &'a str,
    array_key: &'a str,
    batch_limit: usize,
    advance_policy: RecordIndexAdvancePolicy,
}

impl<'a> JsonArrayStreamSpec<'a> {
    pub(super) fn new(
        reader_name: &'a str,
        array_key: &'a str,
        batch_limit: usize,
        advance_policy: RecordIndexAdvancePolicy,
    ) -> Self {
        Self {
            reader_name,
            array_key,
            batch_limit,
            advance_policy,
        }
    }
}

fn advance_record_index(
    policy: RecordIndexAdvancePolicy,
    original_index: u64,
    converted_index: usize,
    event_count: usize,
) -> u64 {
    match policy {
        RecordIndexAdvancePolicy::OriginalU64 => original_index + event_count as u64,
        RecordIndexAdvancePolicy::ConvertedUsize => (converted_index + event_count) as u64,
    }
}

/// Read an object-backed JSON array incrementally using a record-index watermark.
///
/// The reader and missing-array diagnostics remain caller-owned because those
/// strings are part of the existing agent-facing error contract.
pub(super) fn read_json_array_stream(
    path: &Path,
    watermark: Box<dyn WatermarkStrategy>,
    session_id: &str,
    spec: JsonArrayStreamSpec<'_>,
    missing_array_error: impl FnOnce(&Path) -> StreamError,
    skip_read: impl FnOnce() -> bool,
) -> Result<StreamBatch, StreamError> {
    let original_index = watermark
        .as_any()
        .downcast_ref::<RecordIndexWatermark>()
        .ok_or_else(|| StreamError::Fatal {
            message: format!(
                "{} reader requires RecordIndexWatermark, got incompatible type for session {}",
                spec.reader_name, session_id
            ),
        })?
        .0;
    let converted_index = original_index as usize;

    if skip_read() {
        return Ok(StreamBatch {
            events: Vec::new(),
            new_watermark: watermark,
        });
    }

    let file = File::open(path).map_err(|error| transcript_open_error(path, error, "read"))?;

    let reader = BufReader::new(file);
    let mut parsed: serde_json::Value =
        serde_json::from_reader(reader).map_err(|error| StreamError::Parse {
            line: 0,
            message: format!("Invalid JSON in {}: {}", path.display(), error),
        })?;

    let records = match parsed
        .as_object_mut()
        .and_then(|object| object.remove(spec.array_key))
    {
        Some(serde_json::Value::Array(records)) => records,
        _ => return Err(missing_array_error(path)),
    };

    let events: Vec<serde_json::Value> = records
        .into_iter()
        .skip(converted_index)
        .take(spec.batch_limit)
        .collect();
    let new_watermark = Box::new(RecordIndexWatermark::new(advance_record_index(
        spec.advance_policy,
        original_index,
        converted_index,
        events.len(),
    )));

    Ok(StreamBatch {
        events,
        new_watermark,
    })
}

/// Read a JSONL transcript incrementally using a byte-offset watermark.
///
/// Agent-specific readers share the same I/O and malformed-line behavior; only
/// the reader name and open-error wording vary for compatibility with existing
/// diagnostics.
pub(super) fn read_jsonl_byte_stream(
    path: &Path,
    watermark: Box<dyn WatermarkStrategy>,
    session_id: &str,
    batch_limit: usize,
    reader_name: &str,
    open_error_verb: &str,
) -> Result<StreamBatch, StreamError> {
    let byte_watermark = watermark
        .as_any()
        .downcast_ref::<ByteOffsetWatermark>()
        .ok_or_else(|| StreamError::Fatal {
            message: format!(
                "{} reader requires ByteOffsetWatermark, got incompatible type for session {}",
                reader_name, session_id
            ),
        })?;

    let start_offset = byte_watermark.0;
    let file =
        File::open(path).map_err(|error| transcript_open_error(path, error, open_error_verb))?;

    let mut reader = BufReader::new(file);
    reader
        .seek(SeekFrom::Start(start_offset))
        .map_err(|error| StreamError::Transient {
            message: format!("Failed to seek to offset {}: {}", start_offset, error),
            retry_after: std::time::Duration::from_secs(5),
        })?;

    let mut events = Vec::with_capacity(batch_limit);
    let mut current_offset = start_offset;
    let mut line_number = 0;
    let mut line = String::new();

    loop {
        match read_jsonl_line(&mut reader, &mut line).map_err(|error| StreamError::Transient {
            message: format!("I/O error reading line: {}", error),
            retry_after: std::time::Duration::from_secs(5),
        })? {
            JsonlLineState::Eof | JsonlLineState::Partial => break,
            JsonlLineState::Complete(bytes_read) => {
                line_number += 1;
                current_offset += bytes_read as u64;
            }
        }

        if line.trim().is_empty() {
            continue;
        }

        let entry = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    line = line_number,
                    path = %path.display(),
                    error = %error,
                    "skipping malformed JSON line"
                );
                continue;
            }
        };

        events.push(entry);
        if events.len() >= batch_limit {
            break;
        }
    }

    Ok(StreamBatch {
        events,
        new_watermark: Box::new(ByteOffsetWatermark::new(current_offset)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_open_errors_keep_existing_variants_and_messages() {
        let path = Path::new("/transcripts/session.json");
        let cases = [
            (
                std::io::ErrorKind::NotFound,
                StreamError::Fatal {
                    message: "Transcript file not found: /transcripts/session.json".to_string(),
                },
            ),
            (
                std::io::ErrorKind::PermissionDenied,
                StreamError::Fatal {
                    message: "Permission denied reading transcript: /transcripts/session.json"
                        .to_string(),
                },
            ),
            (
                std::io::ErrorKind::Other,
                StreamError::Transient {
                    message: "Failed to open transcript file: fixture error".to_string(),
                    retry_after: std::time::Duration::from_secs(5),
                },
            ),
        ];

        for (kind, expected) in cases {
            let actual =
                transcript_open_error(path, std::io::Error::new(kind, "fixture error"), "open");
            match (actual, expected) {
                (
                    StreamError::Fatal { message: actual },
                    StreamError::Fatal { message: expected },
                ) => assert_eq!(actual, expected),
                (
                    StreamError::Transient {
                        message: actual,
                        retry_after: actual_retry,
                    },
                    StreamError::Transient {
                        message: expected,
                        retry_after: expected_retry,
                    },
                ) => {
                    assert_eq!(actual, expected);
                    assert_eq!(actual_retry, expected_retry);
                }
                _ => panic!("open error variant changed"),
            }
        }
    }

    #[test]
    fn json_array_reader_keeps_invalid_json_diagnostic() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "{").unwrap();

        let result = read_json_array_stream(
            file.path(),
            Box::new(RecordIndexWatermark::new(0)),
            "session-1",
            JsonArrayStreamSpec::new(
                "Fixture",
                "items",
                10,
                RecordIndexAdvancePolicy::ConvertedUsize,
            ),
            |_| unreachable!(),
            || false,
        );
        let error = match result {
            Ok(_) => panic!("invalid JSON should fail"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            StreamError::Parse { line: 0, message }
                if message
                    == format!(
                        "Invalid JSON in {}: EOF while parsing an object at line 1 column 1",
                        file.path().display()
                    )
        ));
    }

    #[test]
    fn json_array_reader_batches_from_record_watermark() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            serde_json::json!({"items": [{"id": 1}, {"id": 2}, {"id": 3}]}).to_string(),
        )
        .unwrap();

        let batch = read_json_array_stream(
            file.path(),
            Box::new(RecordIndexWatermark::new(1)),
            "session-1",
            JsonArrayStreamSpec::new(
                "Fixture",
                "items",
                1,
                RecordIndexAdvancePolicy::ConvertedUsize,
            ),
            |_| StreamError::Fatal {
                message: "items missing".to_string(),
            },
            || false,
        )
        .unwrap();

        assert_eq!(batch.events, vec![serde_json::json!({"id": 2})]);
        assert_eq!(
            batch
                .new_watermark
                .as_any()
                .downcast_ref::<RecordIndexWatermark>()
                .unwrap()
                .0,
            2
        );
    }

    #[test]
    fn record_index_advance_policy_preserves_distinct_32_bit_adapter_behavior() {
        let past_usize_max = u64::from(u32::MAX) + 1;

        assert_eq!(
            advance_record_index(RecordIndexAdvancePolicy::OriginalU64, past_usize_max, 0, 2,),
            past_usize_max + 2
        );
        assert_eq!(
            advance_record_index(
                RecordIndexAdvancePolicy::ConvertedUsize,
                past_usize_max,
                0,
                2,
            ),
            2
        );
    }

    #[test]
    fn json_array_reader_keeps_caller_specific_missing_array_error() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "{}").unwrap();

        let result = read_json_array_stream(
            file.path(),
            Box::new(RecordIndexWatermark::new(0)),
            "session-1",
            JsonArrayStreamSpec::new(
                "Fixture",
                "items",
                10,
                RecordIndexAdvancePolicy::ConvertedUsize,
            ),
            |path| StreamError::Fatal {
                message: format!("missing items in {}", path.display()),
            },
            || false,
        );
        let error = match result {
            Ok(_) => panic!("missing array should fail"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            StreamError::Fatal { message }
                if message == format!("missing items in {}", file.path().display())
        ));
    }

    #[test]
    fn skipped_json_array_read_still_validates_the_watermark_first() {
        let result = read_json_array_stream(
            Path::new("/does/not/exist.json"),
            Box::new(ByteOffsetWatermark::new(0)),
            "session-1",
            JsonArrayStreamSpec::new(
                "Fixture",
                "items",
                10,
                RecordIndexAdvancePolicy::ConvertedUsize,
            ),
            |_| unreachable!(),
            || true,
        );
        let error = match result {
            Ok(_) => panic!("incompatible watermark should fail"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            StreamError::Fatal { message }
                if message
                    == "Fixture reader requires RecordIndexWatermark, got incompatible type for session session-1"
        ));

        let batch = read_json_array_stream(
            Path::new("/does/not/exist.json"),
            Box::new(RecordIndexWatermark::new(7)),
            "session-1",
            JsonArrayStreamSpec::new(
                "Fixture",
                "items",
                10,
                RecordIndexAdvancePolicy::ConvertedUsize,
            ),
            |_| unreachable!(),
            || true,
        )
        .unwrap();
        assert!(batch.events.is_empty());
        assert_eq!(
            batch
                .new_watermark
                .as_any()
                .downcast_ref::<RecordIndexWatermark>()
                .unwrap()
                .0,
            7
        );
    }
}
