//! Bounded JSONL scanning shared by model extraction.
//!
//! Transcripts grow without bound, so every scan here is byte-capped: the
//! tail scan reads at most the trailing window, and the head scan streams
//! line-by-line with per-line and total caps instead of loading whole lines
//! of arbitrary size.

use crate::model::stream_types::StreamError;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

pub(crate) const MAX_JSONL_SCAN_BYTES: u64 = 50 * 1024;
const MAX_JSONL_HEAD_SCAN_BYTES: usize = 1024 * 1024;
const MAX_JSONL_HEAD_LINES: usize = 20;

/// Scans the trailing `MAX_JSONL_SCAN_BYTES` window, newest line first,
/// returning the first extracted value plus whether the file extended beyond
/// the window (callers use the flag to decide whether a head scan is worth
/// running).
pub(crate) fn scan_jsonl_tail(
    path: &Path,
    extract_from_line: fn(&str) -> Option<String>,
) -> Result<(Option<String>, bool), StreamError> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok((None, false)),
    };

    let file_size = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Ok((None, false)),
    };

    if file_size == 0 {
        return Ok((None, false));
    }

    let read_size = std::cmp::min(MAX_JSONL_SCAN_BYTES, file_size);
    let seek_pos = file_size - read_size;

    if file.seek(SeekFrom::Start(seek_pos)).is_err() {
        return Ok((None, false));
    }

    let mut window = Vec::with_capacity(read_size as usize);
    if file.take(read_size).read_to_end(&mut window).is_err() {
        return Ok((None, false));
    }

    // A truncated window usually starts mid-line — possibly mid-way through a
    // multi-byte character — so drop everything up to the first newline
    // instead of letting an invalid first line abort the whole scan.
    let window = if seek_pos > 0 {
        match window.iter().position(|byte| *byte == b'\n') {
            Some(first_newline) => &window[first_newline + 1..],
            None => return Ok((None, true)),
        }
    } else {
        &window[..]
    };

    for line in window.rsplit(|byte| *byte == b'\n') {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        if let Some(value) = extract_from_line(line) {
            return Ok((Some(value), seek_pos > 0));
        }
    }

    Ok((None, seek_pos > 0))
}

/// Scans up to `MAX_JSONL_HEAD_LINES` leading lines within a
/// `MAX_JSONL_HEAD_SCAN_BYTES` budget. Lines longer than the tail window are
/// skipped rather than buffered, so a single oversized record cannot inflate
/// memory or hide later lines.
pub(crate) fn scan_jsonl_head(
    path: &Path,
    extract_from_line: fn(&str) -> Option<String>,
) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut line_is_oversized = false;
    let mut lines_scanned = 0;
    let mut bytes_scanned = 0;

    while lines_scanned < MAX_JSONL_HEAD_LINES && bytes_scanned < MAX_JSONL_HEAD_SCAN_BYTES {
        let bytes_remaining = MAX_JSONL_HEAD_SCAN_BYTES - bytes_scanned;
        let (consumed, reached_newline) = {
            let buffer = reader.fill_buf().ok()?;
            if buffer.is_empty() {
                if !line_is_oversized
                    && let Ok(line) = std::str::from_utf8(&line)
                    && let Some(value) = extract_from_line(line)
                {
                    return Some(value);
                }
                break;
            }

            let searchable = &buffer[..buffer.len().min(bytes_remaining)];
            let newline = searchable.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(searchable.len(), |index| index + 1);

            if !line_is_oversized {
                if line.len() + consumed <= MAX_JSONL_SCAN_BYTES as usize {
                    line.extend_from_slice(&searchable[..consumed]);
                } else {
                    line.clear();
                    line_is_oversized = true;
                }
            }

            (consumed, newline.is_some())
        };

        reader.consume(consumed);
        bytes_scanned += consumed;

        if reached_newline {
            lines_scanned += 1;
            if !line_is_oversized
                && let Ok(line) = std::str::from_utf8(&line)
                && let Some(value) = extract_from_line(line)
            {
                return Some(value);
            }
            line.clear();
            line_is_oversized = false;
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn take_model(line: &str) -> Option<String> {
        let json: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        json.get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    #[test]
    fn test_scan_jsonl_head_skips_oversized_record() {
        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        let oversized_record = serde_json::json!({ "padding": "x".repeat(51_200) });
        writeln!(file, "{oversized_record}").unwrap();
        writeln!(file, r#"{{"model":"model-after-limit"}}"#).unwrap();
        writeln!(file, "{oversized_record}").unwrap();
        file.flush().unwrap();

        let result = scan_jsonl_head(file.path(), take_model);
        assert_eq!(result, Some("model-after-limit".to_string()));
    }

    #[test]
    fn test_scan_jsonl_head_bounds_total_scan() {
        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        let oversized_record =
            serde_json::json!({ "padding": "x".repeat(MAX_JSONL_HEAD_SCAN_BYTES) });
        writeln!(file, "{oversized_record}").unwrap();
        writeln!(file, r#"{{"model":"model-after-total-limit"}}"#).unwrap();
        file.flush().unwrap();

        let result = scan_jsonl_head(file.path(), take_model);
        assert_eq!(result, None);
    }

    #[test]
    fn test_scan_jsonl_head_reads_final_unterminated_line() {
        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        write!(file, r#"{{"model":"no-trailing-newline"}}"#).unwrap();
        file.flush().unwrap();

        let result = scan_jsonl_head(file.path(), take_model);
        assert_eq!(result, Some("no-trailing-newline".to_string()));
    }

    #[test]
    fn test_scan_jsonl_tail_reports_truncation() {
        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        for i in 0..2000 {
            writeln!(file, r#"{{"padding":"line {i} of enough filler to exceed the tail window when repeated"}}"#).unwrap();
        }
        file.flush().unwrap();

        let (model, truncated) = scan_jsonl_tail(file.path(), take_model).unwrap();
        assert_eq!(model, None);
        assert!(
            truncated,
            "a file larger than the window must report truncation"
        );
    }

    #[test]
    fn test_scan_jsonl_tail_handles_utf8_at_window_boundary() {
        // The window start lands mid-way through a multi-byte character: the
        // partial first line must be skipped without aborting the scan, so
        // the model on the last line is still found.
        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        writeln!(file, "{}", "€".repeat(20_000)).unwrap();
        writeln!(file, r#"{{"model":"tail-model"}}"#).unwrap();
        file.flush().unwrap();

        let file_size = std::fs::metadata(file.path()).unwrap().len();
        let window_start = file_size - MAX_JSONL_SCAN_BYTES;
        assert_ne!(
            window_start % 3,
            0,
            "window must start mid-character for this fixture"
        );

        let (model, truncated) = scan_jsonl_tail(file.path(), take_model).unwrap();
        assert_eq!(model, Some("tail-model".to_string()));
        assert!(truncated);
    }

    #[test]
    fn test_scan_jsonl_tail_small_file_not_truncated() {
        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        writeln!(file, r#"{{"model":"small-file-model"}}"#).unwrap();
        file.flush().unwrap();

        let (model, truncated) = scan_jsonl_tail(file.path(), take_model).unwrap();
        assert_eq!(model, Some("small-file-model".to_string()));
        assert!(!truncated);
    }
}
