//! Stream processing module for tracking and reading AI agent streams.
//!
//! This module provides:
//! - Watermarking strategies for incremental stream processing
//! - SQLite database for stream cursor tracking and state persistence
//! - Error types for stream processing failures
//!
//! # Architecture
//!
//! The streams module is designed to work with the daemon worker to:
//! 1. Track stream files for multiple AI agents (Claude Code, Cursor, etc.)
//! 2. Maintain processing state via watermarks (byte offsets, record indices, timestamps)
//! 3. Emit telemetry events from stream data
//!
//! # Example
//!
//! ```ignore
//! use crate::streams::{StreamsDatabase, StreamRecord};
//! use crate::streams::watermark::{ByteOffsetWatermark, WatermarkStrategy};
//!
//! // Open database
//! // Note: the file is still named "transcripts-db" for backwards compatibility.
//! let db = StreamsDatabase::open("~/.git-ai/transcripts-db")?;
//!
//! // Create stream record with watermark
//! let stream = StreamRecord {
//!     session_id: "session-123".to_string(),
//!     tool: "claude-code".to_string(),
//!     stream_path: "/path/to/transcript.jsonl".to_string(),
//!     watermark_type: "ByteOffset".to_string(),
//!     watermark_value: "0".to_string(),
//!     // ... other fields
//! };
//! db.insert_stream(&stream)?;
//!
//! // Process transcript and update watermark
//! let mut watermark = ByteOffsetWatermark::new(0);
//! // ... read and process transcript ...
//! watermark.advance(1024, 10);
//! db.update_watermark("session-123", "transcript", "/path/to/file", &watermark)?;
//! ```

pub mod agent;
pub mod agents;
pub mod db;
pub mod model_extraction;
pub(crate) mod reader;
pub mod sweep;
pub mod types;
pub mod watermark;

// Re-export main types for convenient access
pub use db::{StreamRecord, StreamsDatabase};
pub use types::{StreamBatch, StreamError};
pub use watermark::{
    ByteOffsetWatermark, HybridWatermark, RecordIndexWatermark, TimestampCursorWatermark,
    TimestampWatermark, WatermarkStrategy, WatermarkType,
};
