//! Watermarking strategies for tracking transcript processing progress.

use super::types::StreamError;
use chrono::{DateTime, Utc};
use std::fmt;
use std::str::FromStr;

/// Strategy for tracking progress through a transcript.
pub trait WatermarkStrategy: Send + Sync {
    /// Serialize the watermark to a string for database storage.
    fn serialize(&self) -> String;

    /// Advance the watermark based on bytes and records read.
    fn advance(&mut self, bytes_read: usize, records_read: usize);

    /// Downcast support for concrete watermark types.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Type of watermark strategy (used for deserialization).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatermarkType {
    ByteOffset,
    RecordIndex,
    Timestamp,
    Hybrid,
    TimestampCursor,
}

impl fmt::Display for WatermarkType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WatermarkType::ByteOffset => write!(f, "ByteOffset"),
            WatermarkType::RecordIndex => write!(f, "RecordIndex"),
            WatermarkType::Timestamp => write!(f, "Timestamp"),
            WatermarkType::Hybrid => write!(f, "Hybrid"),
            WatermarkType::TimestampCursor => write!(f, "TimestampCursor"),
        }
    }
}

impl FromStr for WatermarkType {
    type Err = StreamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ByteOffset" => Ok(WatermarkType::ByteOffset),
            "RecordIndex" => Ok(WatermarkType::RecordIndex),
            "Timestamp" => Ok(WatermarkType::Timestamp),
            "Hybrid" => Ok(WatermarkType::Hybrid),
            "TimestampCursor" => Ok(WatermarkType::TimestampCursor),
            _ => Err(StreamError::Parse {
                line: 0,
                message: format!("Invalid watermark type: {}", s),
            }),
        }
    }
}

impl WatermarkType {
    /// Deserialize a watermark value based on the strategy type.
    pub fn deserialize(&self, s: &str) -> Result<Box<dyn WatermarkStrategy>, StreamError> {
        match self {
            WatermarkType::ByteOffset => Ok(Box::new(ByteOffsetWatermark::from_str(s)?)),
            WatermarkType::RecordIndex => Ok(Box::new(RecordIndexWatermark::from_str(s)?)),
            WatermarkType::Timestamp => Ok(Box::new(TimestampWatermark::from_str(s)?)),
            WatermarkType::Hybrid => Ok(Box::new(HybridWatermark::from_str(s)?)),
            WatermarkType::TimestampCursor => Ok(Box::new(TimestampCursorWatermark::from_str(s)?)),
        }
    }

    pub fn create_initial_watermark(&self) -> Box<dyn WatermarkStrategy> {
        match self {
            WatermarkType::ByteOffset => Box::new(ByteOffsetWatermark::new(0)),
            WatermarkType::RecordIndex => Box::new(RecordIndexWatermark::new(0)),
            WatermarkType::Timestamp => Box::new(TimestampWatermark::new(
                chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
            )),
            WatermarkType::Hybrid => Box::new(HybridWatermark::new(0, 0, None)),
            WatermarkType::TimestampCursor => Box::new(TimestampCursorWatermark::initial()),
        }
    }

    /// Initial watermark for a newly-registered stream backed by `path`.
    ///
    /// Byte-seeking streams (byte-offset and hybrid) clamp the initial
    /// backfill: a first-seen transcript is otherwise ingested from byte 0,
    /// and a multi-hundred-MB historical file (long agent sessions embed file
    /// contents in every event) forces a full re-ingest whose memory cost
    /// tripped the memory watchdog in production (#2244). Only the newest
    /// `Config::max_transcript_backfill_bytes()` are ingested; older history
    /// is skipped — these streams feed best-effort diagnostics, not authorship
    /// data. The capped offset is aligned to the next line boundary so the
    /// first read never starts mid-line (a cut inside a multi-byte UTF-8
    /// character would otherwise fail UTF-8 validation on every retry and
    /// wedge the stream).
    pub fn create_initial_watermark_for_file(
        &self,
        path: &std::path::Path,
    ) -> Box<dyn WatermarkStrategy> {
        let max_backfill_bytes = crate::config::Config::get().max_transcript_backfill_bytes();
        self.create_initial_watermark_for_file_with(path, max_backfill_bytes)
    }

    fn create_initial_watermark_for_file_with(
        &self,
        path: &std::path::Path,
        max_backfill_bytes: u64,
    ) -> Box<dyn WatermarkStrategy> {
        if let Some(start) = self.capped_backfill_start(path, max_backfill_bytes) {
            return match self {
                WatermarkType::Hybrid => Box::new(HybridWatermark::new(start, 0, None)),
                _ => Box::new(ByteOffsetWatermark::new(start)),
            };
        }
        self.create_initial_watermark()
    }

    /// For byte-seeking watermark types over a file larger than
    /// `max_backfill_bytes`, the line-aligned offset where the capped
    /// backfill starts. `None` means backfill from the type's zero watermark.
    fn capped_backfill_start(
        &self,
        path: &std::path::Path,
        max_backfill_bytes: u64,
    ) -> Option<u64> {
        if !matches!(self, WatermarkType::ByteOffset | WatermarkType::Hybrid) {
            return None;
        }
        let meta = std::fs::metadata(path).ok()?;
        if meta.len() <= max_backfill_bytes {
            return None;
        }
        let capped = meta.len() - max_backfill_bytes;
        let start = backfill_start_from_alignment(
            align_offset_to_next_line(path, capped),
            meta.len(),
            path,
        );
        tracing::warn!(
            path = %path.display(),
            file_bytes = meta.len(),
            start_offset = start,
            "large first-seen stream: skipping historical backfill beyond cap"
        );
        Some(start)
    }
}

/// Fallback when line alignment of the capped backfill offset fails: never
/// persist the raw capped offset — it can split a multi-byte UTF-8 character
/// or begin mid-record, leaving a permanently misaligned cursor. Skipping the
/// existing content entirely (EOF) is the safe degradation for a transient
/// I/O failure: these streams feed best-effort diagnostics, and new appends
/// still flow from a line boundary once the current line completes.
fn backfill_start_from_alignment(
    aligned: std::io::Result<u64>,
    file_len: u64,
    path: &std::path::Path,
) -> u64 {
    match aligned {
        Ok(start) => start,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "backfill offset alignment failed; skipping existing content"
            );
            file_len
        }
    }
}

/// Smallest line-start offset at or after `offset`: `offset` itself when it
/// already begins a line, otherwise the byte after the enclosing line's
/// newline (the file length when the final line is unterminated).
fn align_offset_to_next_line(path: &std::path::Path, offset: u64) -> std::io::Result<u64> {
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

    if offset == 0 {
        return Ok(0);
    }
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset - 1))?;
    let mut prev = [0u8; 1];
    file.read_exact(&mut prev)?;
    if prev == [b'\n'] {
        return Ok(offset);
    }
    // skip_until discards the enclosing line's remainder without buffering it
    // and returns the bytes consumed, including the newline when found.
    let skipped = BufReader::new(file).skip_until(b'\n')?;
    Ok(offset + skipped as u64)
}

/// Byte-offset based watermark for append-only files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteOffsetWatermark(pub u64);

impl ByteOffsetWatermark {
    pub fn new(offset: u64) -> Self {
        Self(offset)
    }
}

impl WatermarkStrategy for ByteOffsetWatermark {
    fn serialize(&self) -> String {
        self.0.to_string()
    }

    fn advance(&mut self, bytes_read: usize, _records_read: usize) {
        self.0 += bytes_read as u64;
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl FromStr for ByteOffsetWatermark {
    type Err = StreamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<u64>()
            .map(ByteOffsetWatermark)
            .map_err(|e| StreamError::Parse {
                line: 0,
                message: format!("Invalid byte offset watermark: {}", e),
            })
    }
}

/// Record-index based watermark for sequential formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordIndexWatermark(pub u64);

impl RecordIndexWatermark {
    pub fn new(index: u64) -> Self {
        Self(index)
    }
}

impl WatermarkStrategy for RecordIndexWatermark {
    fn serialize(&self) -> String {
        self.0.to_string()
    }

    fn advance(&mut self, _bytes_read: usize, records_read: usize) {
        self.0 += records_read as u64;
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl FromStr for RecordIndexWatermark {
    type Err = StreamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<u64>()
            .map(RecordIndexWatermark)
            .map_err(|e| StreamError::Parse {
                line: 0,
                message: format!("Invalid record index watermark: {}", e),
            })
    }
}

/// Timestamp-based watermark for time-ordered streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimestampWatermark(pub DateTime<Utc>);

impl TimestampWatermark {
    pub fn new(timestamp: DateTime<Utc>) -> Self {
        Self(timestamp)
    }
}

impl WatermarkStrategy for TimestampWatermark {
    fn serialize(&self) -> String {
        self.0.to_rfc3339()
    }

    fn advance(&mut self, _bytes_read: usize, _records_read: usize) {
        // Timestamp watermarks don't auto-advance
        // They must be explicitly updated based on record timestamps
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl FromStr for TimestampWatermark {
    type Err = StreamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        DateTime::parse_from_rfc3339(s)
            .map(|dt| TimestampWatermark(dt.with_timezone(&Utc)))
            .map_err(|e| StreamError::Parse {
                line: 0,
                message: format!("Invalid timestamp watermark: {}", e),
            })
    }
}

/// Timestamp + cursor watermark for keyset pagination over time-ordered data.
/// Stores (timestamp_millis, last_cursor_id) to handle ties at batch boundaries.
/// The cursor is the last-seen ID at the watermark timestamp, enabling
/// `WHERE (ts > ?1 OR (ts = ?1 AND id > ?2))` style queries.
#[derive(Debug, Clone, PartialEq)]
pub struct TimestampCursorWatermark {
    pub timestamp_millis: f64,
    pub last_id: String,
}

impl TimestampCursorWatermark {
    pub fn new(timestamp_millis: f64, last_id: String) -> Self {
        Self {
            timestamp_millis,
            last_id,
        }
    }

    pub fn initial() -> Self {
        Self {
            timestamp_millis: 0.0,
            last_id: String::new(),
        }
    }
}

impl WatermarkStrategy for TimestampCursorWatermark {
    fn serialize(&self) -> String {
        format!("{}|{}", self.timestamp_millis, self.last_id)
    }

    fn advance(&mut self, _bytes_read: usize, _records_read: usize) {
        // Must be explicitly updated with new timestamp + cursor
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl FromStr for TimestampCursorWatermark {
    type Err = StreamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (ts_str, id) = s.split_once('|').ok_or_else(|| StreamError::Parse {
            line: 0,
            message: format!(
                "Invalid TimestampCursor watermark format: expected 'millis|id', got '{}'",
                s
            ),
        })?;
        let timestamp_millis = ts_str.parse::<f64>().map_err(|e| StreamError::Parse {
            line: 0,
            message: format!("Invalid timestamp in TimestampCursor watermark: {}", e),
        })?;
        Ok(Self {
            timestamp_millis,
            last_id: id.to_string(),
        })
    }
}

/// Hybrid watermark combining multiple strategies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HybridWatermark {
    pub offset: u64,
    pub record: u64,
    pub timestamp: Option<DateTime<Utc>>,
}

impl HybridWatermark {
    pub fn new(offset: u64, record: u64, timestamp: Option<DateTime<Utc>>) -> Self {
        Self {
            offset,
            record,
            timestamp,
        }
    }
}

impl WatermarkStrategy for HybridWatermark {
    fn serialize(&self) -> String {
        match &self.timestamp {
            Some(ts) => format!("{}|{}|{}", self.offset, self.record, ts.to_rfc3339()),
            None => format!("{}|{}|", self.offset, self.record),
        }
    }

    fn advance(&mut self, bytes_read: usize, records_read: usize) {
        self.offset += bytes_read as u64;
        self.record += records_read as u64;
        // Timestamp must be explicitly updated based on record data
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl FromStr for HybridWatermark {
    type Err = StreamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('|').collect();
        if parts.len() != 3 {
            return Err(StreamError::Parse {
                line: 0,
                message: format!(
                    "Invalid hybrid watermark format: expected 3 parts, got {}",
                    parts.len()
                ),
            });
        }

        let offset = parts[0].parse::<u64>().map_err(|e| StreamError::Parse {
            line: 0,
            message: format!("Invalid offset in hybrid watermark: {}", e),
        })?;

        let record = parts[1].parse::<u64>().map_err(|e| StreamError::Parse {
            line: 0,
            message: format!("Invalid record in hybrid watermark: {}", e),
        })?;

        let timestamp = if parts[2].is_empty() {
            None
        } else {
            Some(
                DateTime::parse_from_rfc3339(parts[2])
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| StreamError::Parse {
                        line: 0,
                        message: format!("Invalid timestamp in hybrid watermark: {}", e),
                    })?,
            )
        };

        Ok(HybridWatermark {
            offset,
            record,
            timestamp,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_byte_offset_watermark_serialize() {
        let wm = ByteOffsetWatermark::new(1234);
        assert_eq!(wm.serialize(), "1234");
    }

    #[test]
    fn test_byte_offset_watermark_deserialize() {
        let wm = ByteOffsetWatermark::from_str("5678").unwrap();
        assert_eq!(wm.0, 5678);
    }

    #[test]
    fn test_byte_offset_watermark_advance() {
        let mut wm = ByteOffsetWatermark::new(100);
        wm.advance(50, 10);
        assert_eq!(wm.0, 150);
    }

    #[test]
    fn test_byte_offset_watermark_roundtrip() {
        let original = ByteOffsetWatermark::new(9999);
        let serialized = original.serialize();
        let deserialized = ByteOffsetWatermark::from_str(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_byte_offset_watermark_invalid() {
        let result = ByteOffsetWatermark::from_str("not_a_number");
        assert!(result.is_err());
    }

    #[test]
    fn test_record_index_watermark_serialize() {
        let wm = RecordIndexWatermark::new(42);
        assert_eq!(wm.serialize(), "42");
    }

    #[test]
    fn test_record_index_watermark_deserialize() {
        let wm = RecordIndexWatermark::from_str("123").unwrap();
        assert_eq!(wm.0, 123);
    }

    #[test]
    fn test_record_index_watermark_advance() {
        let mut wm = RecordIndexWatermark::new(10);
        wm.advance(1000, 5);
        assert_eq!(wm.0, 15);
    }

    #[test]
    fn test_record_index_watermark_roundtrip() {
        let original = RecordIndexWatermark::new(7777);
        let serialized = original.serialize();
        let deserialized = RecordIndexWatermark::from_str(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_timestamp_watermark_serialize() {
        let ts = DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let wm = TimestampWatermark::new(ts);
        assert_eq!(wm.serialize(), "2024-01-01T12:00:00+00:00");
    }

    #[test]
    fn test_timestamp_watermark_deserialize() {
        let wm = TimestampWatermark::from_str("2024-01-01T12:00:00Z").unwrap();
        let expected = DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(wm.0, expected);
    }

    #[test]
    fn test_timestamp_watermark_advance_noop() {
        let ts = DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut wm = TimestampWatermark::new(ts);
        let original_ts = wm.0;
        wm.advance(100, 10);
        assert_eq!(wm.0, original_ts); // Should not change
    }

    #[test]
    fn test_timestamp_watermark_roundtrip() {
        let ts = DateTime::parse_from_rfc3339("2024-06-15T08:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let original = TimestampWatermark::new(ts);
        let serialized = original.serialize();
        let deserialized = TimestampWatermark::from_str(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_hybrid_watermark_serialize_with_timestamp() {
        let ts = DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let wm = HybridWatermark::new(1000, 50, Some(ts));
        assert_eq!(wm.serialize(), "1000|50|2024-01-01T12:00:00+00:00");
    }

    #[test]
    fn test_hybrid_watermark_serialize_without_timestamp() {
        let wm = HybridWatermark::new(2000, 100, None);
        assert_eq!(wm.serialize(), "2000|100|");
    }

    #[test]
    fn test_hybrid_watermark_deserialize_with_timestamp() {
        let wm = HybridWatermark::from_str("1500|75|2024-01-01T12:00:00Z").unwrap();
        assert_eq!(wm.offset, 1500);
        assert_eq!(wm.record, 75);
        assert!(wm.timestamp.is_some());
    }

    #[test]
    fn test_hybrid_watermark_deserialize_without_timestamp() {
        let wm = HybridWatermark::from_str("3000|150|").unwrap();
        assert_eq!(wm.offset, 3000);
        assert_eq!(wm.record, 150);
        assert!(wm.timestamp.is_none());
    }

    #[test]
    fn test_hybrid_watermark_advance() {
        let mut wm = HybridWatermark::new(100, 10, None);
        wm.advance(50, 5);
        assert_eq!(wm.offset, 150);
        assert_eq!(wm.record, 15);
    }

    #[test]
    fn test_hybrid_watermark_roundtrip_with_timestamp() {
        let ts = DateTime::parse_from_rfc3339("2024-03-15T10:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let original = HybridWatermark::new(5000, 250, Some(ts));
        let serialized = original.serialize();
        let deserialized = HybridWatermark::from_str(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_hybrid_watermark_roundtrip_without_timestamp() {
        let original = HybridWatermark::new(6000, 300, None);
        let serialized = original.serialize();
        let deserialized = HybridWatermark::from_str(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_hybrid_watermark_invalid_format() {
        let result = HybridWatermark::from_str("1000|50");
        assert!(result.is_err());
    }

    #[test]
    fn test_hybrid_watermark_invalid_offset() {
        let result = HybridWatermark::from_str("abc|50|");
        assert!(result.is_err());
    }

    #[test]
    fn test_hybrid_watermark_invalid_record() {
        let result = HybridWatermark::from_str("1000|xyz|");
        assert!(result.is_err());
    }

    #[test]
    fn test_watermark_type_deserialize_byte_offset() {
        let wm = WatermarkType::ByteOffset.deserialize("1234").unwrap();
        assert_eq!(wm.serialize(), "1234");
    }

    #[test]
    fn test_watermark_type_deserialize_record_index() {
        let wm = WatermarkType::RecordIndex.deserialize("42").unwrap();
        assert_eq!(wm.serialize(), "42");
    }

    #[test]
    fn test_watermark_type_deserialize_timestamp() {
        let wm = WatermarkType::Timestamp
            .deserialize("2024-01-01T12:00:00Z")
            .unwrap();
        assert_eq!(wm.serialize(), "2024-01-01T12:00:00+00:00");
    }

    #[test]
    fn test_watermark_type_deserialize_hybrid() {
        let wm = WatermarkType::Hybrid.deserialize("1000|50|").unwrap();
        assert_eq!(wm.serialize(), "1000|50|");
    }

    #[test]
    fn test_watermark_type_deserialize_invalid() {
        let result = WatermarkType::ByteOffset.deserialize("invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_watermark_type_display() {
        assert_eq!(WatermarkType::ByteOffset.to_string(), "ByteOffset");
        assert_eq!(WatermarkType::RecordIndex.to_string(), "RecordIndex");
        assert_eq!(WatermarkType::Timestamp.to_string(), "Timestamp");
        assert_eq!(WatermarkType::Hybrid.to_string(), "Hybrid");
        assert_eq!(
            WatermarkType::TimestampCursor.to_string(),
            "TimestampCursor"
        );
    }

    #[test]
    fn test_watermark_type_from_str() {
        assert_eq!(
            WatermarkType::from_str("ByteOffset").unwrap(),
            WatermarkType::ByteOffset
        );
        assert_eq!(
            WatermarkType::from_str("RecordIndex").unwrap(),
            WatermarkType::RecordIndex
        );
        assert_eq!(
            WatermarkType::from_str("Timestamp").unwrap(),
            WatermarkType::Timestamp
        );
        assert_eq!(
            WatermarkType::from_str("Hybrid").unwrap(),
            WatermarkType::Hybrid
        );
        assert_eq!(
            WatermarkType::from_str("TimestampCursor").unwrap(),
            WatermarkType::TimestampCursor
        );
    }

    #[test]
    fn test_watermark_type_from_str_invalid() {
        let result = WatermarkType::from_str("Invalid");
        assert!(result.is_err());
        match result {
            Err(StreamError::Parse { message, .. }) => {
                assert!(message.contains("Invalid watermark type"));
            }
            _ => panic!("Expected Parse error"),
        }
    }

    #[test]
    fn test_watermark_type_roundtrip() {
        let types = [
            WatermarkType::ByteOffset,
            WatermarkType::RecordIndex,
            WatermarkType::Timestamp,
            WatermarkType::Hybrid,
            WatermarkType::TimestampCursor,
        ];

        for wm_type in &types {
            let serialized = wm_type.to_string();
            let deserialized = WatermarkType::from_str(&serialized).unwrap();
            assert_eq!(*wm_type, deserialized);
        }
    }

    #[test]
    fn test_timestamp_cursor_watermark_serialize() {
        let wm = TimestampCursorWatermark::new(12345.0, "span_abc".to_string());
        assert_eq!(wm.serialize(), "12345|span_abc");
    }

    #[test]
    fn test_timestamp_cursor_watermark_serialize_fractional() {
        let wm = TimestampCursorWatermark::new(12345.67, "span_abc".to_string());
        assert_eq!(wm.serialize(), "12345.67|span_abc");
    }

    #[test]
    fn test_timestamp_cursor_watermark_deserialize() {
        let wm = TimestampCursorWatermark::from_str("67890|span_xyz").unwrap();
        assert_eq!(wm.timestamp_millis, 67890.0);
        assert_eq!(wm.last_id, "span_xyz");
    }

    #[test]
    fn test_timestamp_cursor_watermark_deserialize_fractional() {
        let wm = TimestampCursorWatermark::from_str("67890.35|span_xyz").unwrap();
        assert_eq!(wm.timestamp_millis, 67890.35);
        assert_eq!(wm.last_id, "span_xyz");
    }

    #[test]
    fn test_timestamp_cursor_watermark_initial() {
        let wm = TimestampCursorWatermark::initial();
        assert_eq!(wm.timestamp_millis, 0.0);
        assert_eq!(wm.last_id, "");
        assert_eq!(wm.serialize(), "0|");
    }

    #[test]
    fn test_timestamp_cursor_watermark_roundtrip() {
        let original = TimestampCursorWatermark::new(999999.0, "my-span-id".to_string());
        let serialized = original.serialize();
        let deserialized = TimestampCursorWatermark::from_str(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_timestamp_cursor_watermark_roundtrip_fractional() {
        let original = TimestampCursorWatermark::new(1780519329188.35, "span_id".to_string());
        let serialized = original.serialize();
        let deserialized = TimestampCursorWatermark::from_str(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_timestamp_cursor_watermark_invalid_format() {
        let result = TimestampCursorWatermark::from_str("no_pipe_separator");
        assert!(result.is_err());
    }

    #[test]
    fn test_timestamp_cursor_watermark_invalid_millis() {
        let result = TimestampCursorWatermark::from_str("not_a_number|span1");
        assert!(result.is_err());
    }

    #[test]
    fn test_watermark_type_deserialize_timestamp_cursor() {
        let wm = WatermarkType::TimestampCursor
            .deserialize("5000|span_42")
            .unwrap();
        assert_eq!(wm.serialize(), "5000|span_42");
    }

    /// Backfill cap for fixtures; small so tests stay fast.
    const TEST_BACKFILL_CAP: u64 = 8 * 1024;

    /// Write a JSONL file of identical lines whose total size exceeds
    /// [`TEST_BACKFILL_CAP`], returning the file and the expected
    /// line-aligned capped start offset.
    fn oversized_jsonl_fixture(line_body: &str) -> (tempfile::NamedTempFile, u64) {
        use std::io::Write;

        let line = format!("{line_body}\n");
        let line_len = line.len() as u64;
        let line_count = TEST_BACKFILL_CAP / line_len + 2;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for _ in 0..line_count {
            file.write_all(line.as_bytes()).unwrap();
        }
        file.flush().unwrap();

        let total = line_count * line_len;
        let capped = total - TEST_BACKFILL_CAP;
        let expected = capped.div_ceil(line_len) * line_len;
        (file, expected)
    }

    #[test]
    fn test_capped_initial_watermark_aligns_to_next_line_boundary() {
        // 1003-byte lines guarantee the raw cap offset lands mid-line.
        let (file, expected) = oversized_jsonl_fixture(&"x".repeat(1002));
        assert_ne!(
            expected,
            std::fs::metadata(file.path()).unwrap().len() - TEST_BACKFILL_CAP,
            "fixture must exercise the mid-line case"
        );

        let wm = WatermarkType::ByteOffset
            .create_initial_watermark_for_file_with(file.path(), TEST_BACKFILL_CAP);
        assert_eq!(wm.serialize(), expected.to_string());
    }

    #[test]
    fn test_capped_initial_watermark_never_splits_multibyte_characters() {
        // 3-byte characters ensure a naive len-cap offset would slice
        // mid-character (the 8 KiB cap is not divisible by the 1000-byte line
        // stride below) and wedge the reader on UTF-8 validation.
        let (file, expected) = oversized_jsonl_fixture(&"€".repeat(333));

        let wm = WatermarkType::ByteOffset
            .create_initial_watermark_for_file_with(file.path(), TEST_BACKFILL_CAP);
        let start: u64 = wm.serialize().parse().unwrap();
        assert_eq!(start, expected);

        // The first read from the aligned offset must be a complete valid line.
        use std::io::{Seek, SeekFrom};
        let mut reader = std::io::BufReader::new(std::fs::File::open(file.path()).unwrap());
        reader.seek(SeekFrom::Start(start)).unwrap();
        let mut line = String::new();
        match crate::streams::types::read_jsonl_line(&mut reader, &mut line, TEST_BACKFILL_CAP)
            .unwrap()
        {
            crate::streams::types::JsonlLineState::Complete(_) => {
                assert_eq!(line.trim_end(), "€".repeat(333));
            }
            other => panic!("expected a complete line at the aligned offset, got {other:?}"),
        }
    }

    #[test]
    fn test_capped_initial_watermark_applies_to_hybrid_streams() {
        // Droid transcripts seek by byte offset through a Hybrid watermark;
        // they need the same backfill cap as plain byte-offset streams.
        let (file, expected) = oversized_jsonl_fixture(&"x".repeat(1002));

        let wm = WatermarkType::Hybrid
            .create_initial_watermark_for_file_with(file.path(), TEST_BACKFILL_CAP);
        assert_eq!(wm.serialize(), format!("{expected}|0|"));
    }

    #[test]
    fn test_capped_initial_watermark_aligns_across_crlf_line_endings() {
        // Windows-written transcripts end lines with \r\n; alignment searches
        // for \n, so the aligned offset must begin the next line (after the
        // full \r\n pair), never between \r and \n.
        let (file, expected) = oversized_jsonl_fixture(&format!("{}\r", "x".repeat(1001)));

        let wm = WatermarkType::ByteOffset
            .create_initial_watermark_for_file_with(file.path(), TEST_BACKFILL_CAP);
        let start: u64 = wm.serialize().parse().unwrap();
        assert_eq!(start, expected);
        assert_eq!(start % 1003, 0, "aligned offset must begin a line");
    }

    #[test]
    fn test_alignment_failure_falls_back_to_eof_not_raw_offset() {
        // A failed alignment must never persist the raw capped offset (it can
        // split a UTF-8 character or begin mid-record); the safe degradation
        // is skipping the existing content entirely.
        let err = std::io::Error::other("injected alignment failure");
        let start = backfill_start_from_alignment(Err(err), 12345, std::path::Path::new("t"));
        assert_eq!(start, 12345);

        let start = backfill_start_from_alignment(Ok(777), 12345, std::path::Path::new("t"));
        assert_eq!(start, 777);
    }

    #[test]
    fn test_small_file_keeps_zero_initial_watermark() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"{\"ok\":1}\n").unwrap();
        file.flush().unwrap();

        let wm = WatermarkType::ByteOffset
            .create_initial_watermark_for_file_with(file.path(), TEST_BACKFILL_CAP);
        assert_eq!(wm.serialize(), "0");
        let wm = WatermarkType::Hybrid
            .create_initial_watermark_for_file_with(file.path(), TEST_BACKFILL_CAP);
        assert_eq!(wm.serialize(), "0|0|");
    }
}
