//! Core types for transcript processing.

use std::io::{BufRead, Read};
use std::time::Duration;

/// Result of reading a single line from a JSONL reader.
#[derive(Debug)]
pub enum JsonlLineState {
    /// End of file reached.
    Eof,
    /// Incomplete line (no trailing newline) — writer still appending.
    Partial,
    /// Complete line ready for processing. Contains bytes read.
    Complete(usize),
    /// Line exceeded the byte cap and was skipped without being buffered.
    /// Contains total bytes consumed (cap + remainder up to and including the
    /// newline) so callers can advance their watermark past it.
    Oversized(usize),
}

/// Read a line from a BufReader, detecting partial writes from concurrent writers.
///
/// `max_line_bytes` bounds how much of a line is ever buffered
/// (`Config::max_transcript_line_bytes()` in production): `read_line` is
/// otherwise unbounded, and a single multi-hundred-MB transcript line can
/// balloon daemon RSS past the memory watchdog's hard limit (#2244).
///
/// Returns `Eof` if no more data, `Partial` if the line lacks a trailing newline,
/// `Complete(bytes)` on success, or `Oversized(bytes)` when the line exceeded
/// `max_line_bytes` (content is discarded, reader advanced past the newline).
pub fn read_jsonl_line(
    reader: &mut impl BufRead,
    line: &mut String,
    max_line_bytes: u64,
) -> std::io::Result<JsonlLineState> {
    // Read raw bytes, not via read_line: the byte cap can slice a multi-byte
    // character, and read_line's UTF-8 validation would then return
    // InvalidData instead of letting us classify the line as Oversized —
    // wedging the stream at a fixed watermark. The caller's buffer is reused
    // across calls (agents hold one String for a whole batch scan), so
    // steady-state reads stay allocation-free.
    line.clear();
    let mut buf = std::mem::take(line).into_bytes();
    let bytes_read = reader
        .by_ref()
        .take(max_line_bytes)
        .read_until(b'\n', &mut buf)?;
    if buf.last() != Some(&b'\n') && bytes_read as u64 == max_line_bytes {
        // The cap was hit mid-line: discard what we buffered (no UTF-8
        // conversion is attempted on discarded content, and dropping the
        // buffer releases the cap-sized allocation) and skip the rest of the
        // physical line without storing it. If EOF arrives before the
        // newline (giant line still being written), we still report
        // Oversized — re-reading it later would OOM anyway.
        drop(buf);
        let skipped = reader.skip_until(b'\n')?;
        return Ok(JsonlLineState::Oversized(bytes_read + skipped));
    }
    // Same contract as read_line: non-UTF-8 content is an InvalidData error.
    *line = String::from_utf8(buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if bytes_read == 0 {
        return Ok(JsonlLineState::Eof);
    }
    if !line.ends_with('\n') {
        return Ok(JsonlLineState::Partial);
    }
    Ok(JsonlLineState::Complete(bytes_read))
}

/// Read up to `max_lines` leading lines of a JSONL file, each bounded by
/// `Config::max_transcript_line_bytes()`, with line endings stripped.
///
/// This is the bounded replacement for `BufRead::lines().take(n)` in
/// metadata sniffers (`infer_cwd` and friends): those run before the capped
/// batch loop and before any watermark advance, so an unbounded read of one
/// giant line could balloon daemon RSS with no persisted progress (#2244).
/// Oversized and invalid-UTF-8 lines are skipped but count toward the scan
/// budget; a final unterminated line is returned as-is.
pub fn read_leading_jsonl_lines(path: &std::path::Path, max_lines: usize) -> Vec<String> {
    let max_line_bytes = crate::config::Config::get().max_transcript_line_bytes();
    read_leading_jsonl_lines_with(path, max_lines, max_line_bytes)
}

fn read_leading_jsonl_lines_with(
    path: &std::path::Path,
    max_lines: usize,
    max_line_bytes: u64,
) -> Vec<String> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut reader = std::io::BufReader::new(file);
    let mut lines = Vec::with_capacity(max_lines.min(64));
    let mut line = String::new();
    for _ in 0..max_lines {
        match read_jsonl_line(&mut reader, &mut line, max_line_bytes) {
            Ok(JsonlLineState::Eof) => break,
            Ok(JsonlLineState::Partial) => {
                lines.push(std::mem::take(&mut line));
                break;
            }
            Ok(JsonlLineState::Complete(_)) => {
                lines.push(line.trim_end_matches(['\n', '\r']).to_string());
            }
            Ok(JsonlLineState::Oversized(_)) => {}
            // The reader consumed the offending line through its newline, so
            // the scan can continue on the next one.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {}
            Err(_) => break,
        }
    }
    lines
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

    /// Cap large enough that ordinary test lines never hit it.
    const TEST_CAP: u64 = 1024;

    #[test]
    fn test_read_jsonl_line_eof() {
        let data = b"";
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut line = String::new();
        let result = read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap();
        assert!(matches!(result, JsonlLineState::Eof));
    }

    #[test]
    fn test_read_jsonl_line_complete() {
        let data = b"{\"id\":1}\n";
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut line = String::new();
        let result = read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap();
        assert!(matches!(result, JsonlLineState::Complete(9)));
        assert_eq!(line, "{\"id\":1}\n");
    }

    #[test]
    fn test_read_jsonl_line_partial() {
        let data = b"{\"id\":1}";
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut line = String::new();
        let result = read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap();
        assert!(matches!(result, JsonlLineState::Partial));
    }

    #[test]
    fn test_read_jsonl_line_multiple_lines() {
        let data = b"{\"a\":1}\n{\"b\":2}\n";
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut line = String::new();

        let r1 = read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap();
        assert!(matches!(r1, JsonlLineState::Complete(8)));

        let r2 = read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap();
        assert!(matches!(r2, JsonlLineState::Complete(8)));

        let r3 = read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap();
        assert!(matches!(r3, JsonlLineState::Eof));
    }

    #[test]
    fn test_read_jsonl_line_complete_then_partial() {
        let data = b"{\"a\":1}\n{\"b\":2}";
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut line = String::new();

        let r1 = read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap();
        assert!(matches!(r1, JsonlLineState::Complete(8)));

        let r2 = read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap();
        assert!(matches!(r2, JsonlLineState::Partial));
    }

    #[test]
    fn test_read_jsonl_line_oversized_is_skipped_and_reader_recovers() {
        let big = "x".repeat(TEST_CAP as usize + 100);
        let data = format!("{big}\n{{\"ok\":1}}\n");
        let mut reader = std::io::BufReader::new(data.as_bytes());
        let mut line = String::new();

        match read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap() {
            JsonlLineState::Oversized(consumed) => {
                assert_eq!(consumed, big.len() + 1);
                assert!(line.is_empty(), "oversized content must not be retained");
            }
            other => panic!("expected Oversized for a line beyond the cap, got {other:?}"),
        }

        match read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap() {
            JsonlLineState::Complete(_) => assert_eq!(line.trim_end(), "{\"ok\":1}"),
            other => panic!("expected the next line to parse normally, got {other:?}"),
        }
    }

    #[test]
    fn test_read_jsonl_line_oversized_without_newline_at_eof() {
        let big = "x".repeat(TEST_CAP as usize + 50);
        let mut reader = std::io::BufReader::new(big.as_bytes());
        let mut line = String::new();

        match read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap() {
            JsonlLineState::Oversized(consumed) => assert_eq!(consumed, big.len()),
            other => panic!("expected Oversized even without a trailing newline, got {other:?}"),
        }
    }

    #[test]
    fn test_read_jsonl_line_oversized_multibyte_at_cap_boundary() {
        // 1024 is not divisible by 3, so a line of 3-byte characters
        // guarantees the byte cap slices mid-character. The reader must
        // classify Oversized — a read_line-based implementation returns
        // InvalidData here and wedges the stream at a fixed watermark.
        let euro = "€"; // 3 bytes in UTF-8
        let big = euro.repeat((TEST_CAP as usize / 3) + 50);
        let data = format!("{big}\n{{\"ok\":1}}\n");
        let mut reader = std::io::BufReader::new(data.as_bytes());
        let mut line = String::new();

        match read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap() {
            JsonlLineState::Oversized(consumed) => assert_eq!(consumed, big.len() + 1),
            other => panic!("expected Oversized for a multi-byte giant line, got {other:?}"),
        }

        match read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap() {
            JsonlLineState::Complete(_) => assert_eq!(line.trim_end(), "{\"ok\":1}"),
            other => panic!("expected the next line to parse normally, got {other:?}"),
        }
    }

    #[test]
    fn test_read_jsonl_line_invalid_utf8_within_cap_still_errors() {
        // Non-UTF-8 content on a normal-sized line keeps read_line's
        // InvalidData contract.
        let data: &[u8] = b"\xff\xfe bad bytes\n";
        let mut reader = std::io::BufReader::new(data);
        let mut line = String::new();
        let err = read_jsonl_line(&mut reader, &mut line, TEST_CAP).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_read_leading_jsonl_lines_is_bounded_and_skips_oversized() {
        use std::io::Write;

        // First line oversized, then a complete line, then an unterminated
        // final line: the sniff scan must survive the giant line without
        // buffering it and still return the readable lines.
        let big = "x".repeat(TEST_CAP as usize + 10);
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{big}\n{{\"cwd\":\"/repo\"}}\n{{\"n\":2}}").unwrap();
        file.flush().unwrap();

        let lines = read_leading_jsonl_lines_with(file.path(), 3, TEST_CAP);
        assert_eq!(
            lines,
            vec!["{\"cwd\":\"/repo\"}".to_string(), "{\"n\":2}".to_string()]
        );

        // The oversized first line counts toward the scan budget.
        assert!(read_leading_jsonl_lines_with(file.path(), 1, TEST_CAP).is_empty());
    }
}
