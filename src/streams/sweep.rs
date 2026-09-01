// src/streams/sweep.rs

use std::path::PathBuf;
use std::time::Duration;

/// Strategy for discovering new/updated sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepStrategy {
    /// Periodic polling at the given interval
    Periodic(Duration),
    /// File system watcher (not implemented yet)
    FsWatcher,
    /// HTTP API polling (not implemented yet)
    HttpApi,
    /// No sweep support for this agent
    None,
}

/// A session discovered during a sweep.
#[derive(Debug, Clone)]
pub struct DiscoveredSession {
    pub session_id: String,
    pub tool: String,
    pub stream_path: PathBuf,
    pub external_session_id: String,
    pub external_parent_session_id: Option<String>,
}

/// Transcript file format enum
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    ClaudeJsonl,
    CursorJsonl,
    DroidJsonl,
    CopilotSessionJson,
    CopilotEventStreamJsonl,
    GeminiJsonl,
    ContinueJson,
    WindsurfJsonl,
    CodexJsonl,
    AmpThreadJson,
    OpenCodeSqlite,
    PiJsonl,
    GrokJsonl,
    CopilotOtelSqlite,
}

impl std::fmt::Display for StreamFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClaudeJsonl => write!(f, "ClaudeJsonl"),
            Self::CursorJsonl => write!(f, "CursorJsonl"),
            Self::DroidJsonl => write!(f, "DroidJsonl"),
            Self::CopilotSessionJson => write!(f, "CopilotSessionJson"),
            Self::CopilotEventStreamJsonl => write!(f, "CopilotEventStreamJsonl"),
            Self::GeminiJsonl => write!(f, "GeminiJsonl"),
            Self::ContinueJson => write!(f, "ContinueJson"),
            Self::WindsurfJsonl => write!(f, "WindsurfJsonl"),
            Self::CodexJsonl => write!(f, "CodexJsonl"),
            Self::AmpThreadJson => write!(f, "AmpThreadJson"),
            Self::OpenCodeSqlite => write!(f, "OpenCodeSqlite"),
            Self::PiJsonl => write!(f, "PiJsonl"),
            Self::GrokJsonl => write!(f, "GrokJsonl"),
            Self::CopilotOtelSqlite => write!(f, "CopilotOtelSqlite"),
        }
    }
}

impl StreamFormat {
    pub fn watermark_type(self) -> super::watermark::WatermarkType {
        use super::watermark::WatermarkType;
        match self {
            Self::ClaudeJsonl
            | Self::CursorJsonl
            | Self::GeminiJsonl
            | Self::WindsurfJsonl
            | Self::CodexJsonl
            | Self::PiJsonl
            | Self::GrokJsonl
            | Self::CopilotEventStreamJsonl => WatermarkType::ByteOffset,
            Self::DroidJsonl => WatermarkType::Hybrid,
            Self::CopilotSessionJson | Self::ContinueJson | Self::AmpThreadJson => {
                WatermarkType::RecordIndex
            }
            Self::OpenCodeSqlite => WatermarkType::Timestamp,
            Self::CopilotOtelSqlite => WatermarkType::TimestampCursor,
        }
    }
}
