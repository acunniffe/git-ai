//! Core types for transcript processing.

use std::io::{BufRead, Read};
use std::time::Duration;

/// Upper bound on a single JSONL line. Lines beyond this are skipped rather
/// than buffered: `read_line` is otherwise unbounded, and a single multi-hundred-MB
/// transcript line can balloon daemon RSS past the memory watchdog's hard limit
/// (git-ai#2244). 8 MiB comfortably fits any legitimate transcript event.
pub const MAX_JSONL_LINE_BYTES: u64 = 8 * 1024 * 1024;

/// Upper bound on the total raw bytes carried by one transcript batch.
/// Batches were previously capped by event count only (1000), so a transcript
/// whose events embed file contents could put hundreds of MB into a single
/// batch — which downstream redaction + metrics conversion amplifies several
/// times over (git-ai#2244). The batch loop stops early once this budget is
/// spent; remaining events arrive in later batches.
pub const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;

/// Result of reading a single line from a JSONL reader.
#[derive(Debug)]
pub enum JsonlLineState {
    /// End of file reached.
    Eof,
    /// Incomplete line (no trailing newline) — writer still appending.
    Partial,
    /// Complete line ready for processing. Contains bytes read.
    Complete(usize),
    /// Line exceeded [`MAX_JSONL_LINE_BYTES`] and was skipped without being
    /// buffered. Contains total bytes consumed (cap + remainder up to and
    /// including the newline) so callers can advance their watermark past it.
    Oversized(usize),
}

/// Read a line from a BufReader, detecting partial writes from concurrent writers.
///
/// Returns `Eof` if no more data, `Partial` if the line lacks a trailing newline,
/// `Complete(bytes)` on success, or `Oversized(bytes)` when the line exceeded
/// [`MAX_JSONL_LINE_BYTES`] (content is discarded, reader advanced past the newline).
pub fn read_jsonl_line(
    reader: &mut impl BufRead,
    line: &mut String,
) -> std::io::Result<JsonlLineState> {
    line.clear();
    // Read raw bytes, not via read_line: the byte cap can slice a multi-byte
    // character, and read_line's UTF-8 validation would then return
    // InvalidData instead of letting us classify the line as Oversized —
    // wedging the stream at a fixed watermark.
    let mut buf = Vec::new();
    let bytes_read = reader
        .by_ref()
        .take(MAX_JSONL_LINE_BYTES)
        .read_until(b'\n', &mut buf)?;
    if bytes_read == 0 {
        return Ok(JsonlLineState::Eof);
    }
    if buf.last() != Some(&b'\n') && bytes_read as u64 == MAX_JSONL_LINE_BYTES {
        // The cap was hit mid-line: discard what we buffered (no UTF-8
        // conversion is attempted on discarded content) and skip the rest of
        // the physical line without storing it. If EOF arrives before the
        // newline (giant line still being written), we still report
        // Oversized — re-reading it later would OOM anyway.
        drop(buf);
        let skipped = reader.skip_until(b'\n')?;
        return Ok(JsonlLineState::Oversized(bytes_read + skipped));
    }
    // Same contract as read_line: non-UTF-8 content is an InvalidData error.
    *line = String::from_utf8(buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if !line.ends_with('\n') {
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
    fn test_read_jsonl_line_oversized_is_skipped_and_reader_recovers() {
        let big = "x".repeat(MAX_JSONL_LINE_BYTES as usize + 100);
        let data = format!("{big}\n{{\"ok\":1}}\n");
        let mut reader = std::io::BufReader::new(data.as_bytes());
        let mut line = String::new();

        match read_jsonl_line(&mut reader, &mut line).unwrap() {
            JsonlLineState::Oversized(consumed) => {
                assert_eq!(consumed, big.len() + 1);
                assert!(line.is_empty(), "oversized content must not be retained");
            }
            _ => panic!("expected Oversized for a line beyond MAX_JSONL_LINE_BYTES"),
        }

        match read_jsonl_line(&mut reader, &mut line).unwrap() {
            JsonlLineState::Complete(_) => assert_eq!(line.trim_end(), "{\"ok\":1}"),
            _ => panic!("expected the next line to parse normally"),
        }
    }

    #[test]
    fn test_read_jsonl_line_oversized_without_newline_at_eof() {
        let big = "x".repeat(MAX_JSONL_LINE_BYTES as usize + 50);
        let mut reader = std::io::BufReader::new(big.as_bytes());
        let mut line = String::new();

        match read_jsonl_line(&mut reader, &mut line).unwrap() {
            JsonlLineState::Oversized(consumed) => assert_eq!(consumed, big.len()),
            _ => panic!("expected Oversized even when the giant line lacks a newline"),
        }
    }

    #[test]
    fn test_read_jsonl_line_oversized_multibyte_at_cap_boundary() {
        // 8 MiB is not divisible by 3, so a line of 3-byte characters
        // guarantees the byte cap slices mid-character. The reader must
        // classify Oversized — a read_line-based implementation returns
        // InvalidData here and wedges the stream at a fixed watermark.
        let euro = "€"; // 3 bytes in UTF-8
        let big = euro.repeat((MAX_JSONL_LINE_BYTES as usize / 3) + 50);
        let data = format!("{big}\n{{\"ok\":1}}\n");
        let mut reader = std::io::BufReader::new(data.as_bytes());
        let mut line = String::new();

        match read_jsonl_line(&mut reader, &mut line).unwrap() {
            JsonlLineState::Oversized(consumed) => assert_eq!(consumed, big.len() + 1),
            _ => panic!("expected Oversized for a multi-byte giant line"),
        }

        match read_jsonl_line(&mut reader, &mut line).unwrap() {
            JsonlLineState::Complete(_) => assert_eq!(line.trim_end(), "{\"ok\":1}"),
            _ => panic!("expected the next line to parse normally"),
        }
    }

    #[test]
    fn test_read_jsonl_line_invalid_utf8_within_cap_still_errors() {
        // Non-UTF-8 content on a normal-sized line keeps read_line's
        // InvalidData contract.
        let data: &[u8] = b"\xff\xfe bad bytes\n";
        let mut reader = std::io::BufReader::new(data);
        let mut line = String::new();
        let err = read_jsonl_line(&mut reader, &mut line).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
