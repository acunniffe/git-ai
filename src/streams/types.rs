//! Core types for transcript processing.

use std::io::BufRead;
use std::time::Duration;

/// Result of reading a single line from a JSONL reader.
pub enum JsonlLineState {
    /// End of file reached.
    Eof,
    /// Incomplete line (no trailing newline) — writer still appending.
    Partial,
    /// Complete line ready for processing. Contains bytes read.
    Complete(usize),
}

/// Read a line from a BufReader, detecting partial writes from concurrent writers.
///
/// Returns `Eof` if no more data, `Partial` if the line lacks a trailing newline,
/// or `Complete(bytes)` on success, where `bytes` is the raw byte count read
/// (including the newline). Invalid UTF-8 never fails the read: invalid bytes
/// are replaced with U+FFFD, so a corrupt line either fails JSON parsing (and
/// is skipped by callers) or parses with replacement characters.
pub fn read_jsonl_line(
    reader: &mut impl BufRead,
    line: &mut String,
) -> std::io::Result<JsonlLineState> {
    // Reuse the caller's String allocation as the byte buffer so the per-line
    // read loop stays allocation-free for valid UTF-8 (the common case).
    let mut buf = std::mem::take(line).into_bytes();
    buf.clear();
    let bytes_read = reader.read_until(b'\n', &mut buf)?;
    let complete = buf.ends_with(b"\n");
    *line = String::from_utf8(buf)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    if bytes_read == 0 {
        return Ok(JsonlLineState::Eof);
    }
    if !complete {
        return Ok(JsonlLineState::Partial);
    }
    Ok(JsonlLineState::Complete(bytes_read))
}

/// Errors that can occur during transcript processing.
#[derive(Debug, Clone)]
pub enum StreamError {
    /// Transient errors that should be retried (file locked, network timeout).
    Transient {
        message: String,
        retry_after: Duration,
    },
    /// Parse errors from malformed data (bad JSON, unexpected format).
    Parse { line: usize, message: String },
    /// Fatal errors that cannot be recovered (file deleted, permissions denied).
    Fatal { message: String },
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::Transient {
                message,
                retry_after,
            } => write!(
                f,
                "Transient error (retry after {:?}): {}",
                retry_after, message
            ),
            StreamError::Parse { line, message } => {
                write!(f, "Parse error at line {}: {}", line, message)
            }
            StreamError::Fatal { message } => write!(f, "Fatal error: {}", message),
        }
    }
}

impl std::error::Error for StreamError {}

/// Batch of transcript events returned by transcript readers after processing.
pub struct StreamBatch {
    /// Raw JSON events from the transcript.
    pub events: Vec<serde_json::Value>,
    /// Updated watermark position after processing this batch.
    pub new_watermark: Box<dyn crate::streams::WatermarkStrategy>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transient_error_display() {
        let err = StreamError::Transient {
            message: "file locked".to_string(),
            retry_after: Duration::from_secs(5),
        };
        let display = format!("{}", err);
        assert!(display.contains("Transient error"));
        assert!(display.contains("5s"));
        assert!(display.contains("file locked"));
    }

    #[test]
    fn test_parse_error_display() {
        let err = StreamError::Parse {
            line: 42,
            message: "invalid JSON".to_string(),
        };
        let display = format!("{}", err);
        assert!(display.contains("Parse error at line 42"));
        assert!(display.contains("invalid JSON"));
    }

    #[test]
    fn test_fatal_error_display() {
        let err = StreamError::Fatal {
            message: "file deleted".to_string(),
        };
        let display = format!("{}", err);
        assert!(display.contains("Fatal error"));
        assert!(display.contains("file deleted"));
    }

    #[test]
    fn test_error_is_std_error() {
        let err = StreamError::Fatal {
            message: "test".to_string(),
        };
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn test_error_clone() {
        let err = StreamError::Transient {
            message: "test".to_string(),
            retry_after: Duration::from_secs(10),
        };
        let cloned = err.clone();
        match cloned {
            StreamError::Transient {
                message,
                retry_after,
            } => {
                assert_eq!(message, "test");
                assert_eq!(retry_after, Duration::from_secs(10));
            }
            _ => panic!("Expected Transient variant"),
        }
    }

    #[test]
    fn test_read_jsonl_line_eof() {
        let data = b"";
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut line = String::new();
        let result = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(result, JsonlLineState::Eof));
    }

    #[test]
    fn test_read_jsonl_line_complete() {
        let data = b"{\"id\":1}\n";
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut line = String::new();
        let result = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(result, JsonlLineState::Complete(9)));
        assert_eq!(line, "{\"id\":1}\n");
    }

    #[test]
    fn test_read_jsonl_line_partial() {
        let data = b"{\"id\":1}";
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut line = String::new();
        let result = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(result, JsonlLineState::Partial));
    }

    #[test]
    fn test_read_jsonl_line_multiple_lines() {
        let data = b"{\"a\":1}\n{\"b\":2}\n";
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut line = String::new();

        let r1 = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(r1, JsonlLineState::Complete(8)));

        let r2 = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(r2, JsonlLineState::Complete(8)));

        let r3 = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(r3, JsonlLineState::Eof));
    }

    #[test]
    fn test_read_jsonl_line_complete_then_partial() {
        let data = b"{\"a\":1}\n{\"b\":2}";
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut line = String::new();

        let r1 = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(r1, JsonlLineState::Complete(8)));

        let r2 = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(r2, JsonlLineState::Partial));
    }

    #[test]
    fn test_read_jsonl_line_invalid_utf8_line_advances() {
        // Middle line contains invalid UTF-8 bytes; reading must not error,
        // must advance past it, and the following valid line must still parse.
        let mut data = Vec::new();
        data.extend_from_slice(b"{\"a\":1}\n");
        data.extend_from_slice(b"{\"bad\":\"\xff\xfe\"}\n");
        data.extend_from_slice(b"{\"c\":3}\n");
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut line = String::new();

        let r1 = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(r1, JsonlLineState::Complete(8)));
        assert_eq!(line, "{\"a\":1}\n");

        // Byte count must be the raw byte count (13), not the length of the
        // lossily-decoded string (replacement chars are 3 bytes each).
        let r2 = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(r2, JsonlLineState::Complete(13)));
        // Invalid bytes decode to U+FFFD. Note the result here is still valid
        // JSON, so callers ingest such a line rather than skip it.
        assert_eq!(line, "{\"bad\":\"\u{FFFD}\u{FFFD}\"}\n");

        let r3 = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(r3, JsonlLineState::Complete(8)));
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["c"].as_i64(), Some(3));

        let r4 = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(r4, JsonlLineState::Eof));
    }

    #[test]
    fn test_read_jsonl_line_partial_mid_multibyte() {
        // File ends mid-write, in the middle of a multi-byte UTF-8 sequence
        // ("€" is \xe2\x82\xac; only the first two bytes are present). This
        // must be reported as Partial, not an error.
        let data = b"{\"a\":1}\n{\"x\":\"\xe2\x82";
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut line = String::new();

        let r1 = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(r1, JsonlLineState::Complete(8)));

        let r2 = read_jsonl_line(&mut reader, &mut line).unwrap();
        assert!(matches!(r2, JsonlLineState::Partial));
    }
}
