// src/streams/agent.rs

use super::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use super::types::{StreamBatch, StreamError};
use super::watermark::WatermarkStrategy;
use std::path::{Path, PathBuf};

/// Sentinel session_id for shared stream watermark rows.
/// Shared streams (e.g., a global OTEL SQLite DB) don't belong to any session —
/// they use this constant as their DB key. The `stream_path` column
/// disambiguates when multiple shared streams exist.
pub const SHARED_STREAM_SESSION_ID: &str = "__shared__";

/// Type alias for the custom path resolver function used in `PathResolverKind::Custom`.
pub type PathResolverFn = Box<dyn Fn(&Path) -> Option<PathBuf> + Send + Sync>;

pub enum PathResolverKind {
    /// Same path as the session's stream_path
    Identity,
    /// Same directory, different filename
    Sibling { filename: &'static str },
    /// Custom resolution function
    Custom(PathResolverFn),
}

/// Type alias for resolver functions that derive values from the resolved path.
pub type WatermarkTypeResolverFn =
    Box<dyn Fn(&Path) -> super::watermark::WatermarkType + Send + Sync>;
pub type FormatResolverFn = Box<dyn Fn(&Path) -> StreamFormat + Send + Sync>;

pub struct StreamDescriptor {
    pub stream_kind: &'static str,
    pub format: StreamFormat,
    pub watermark_type: super::watermark::WatermarkType,
    pub path_resolver: PathResolverKind,
    /// When true, this stream's data source is shared across multiple sessions
    /// (e.g., a global OTEL SQLite DB). The session_id for the DB record is derived
    /// from the canonical path rather than the triggering session, so all sessions
    /// share a single watermark.
    pub shared: bool,
    /// Optional function to determine watermark type from the resolved path.
    /// Used when a single stream descriptor covers files with different watermark
    /// strategies (e.g., Copilot .json vs .jsonl files).
    pub watermark_type_resolver: Option<WatermarkTypeResolverFn>,
    /// Optional function to determine transcript format from the resolved path.
    pub format_resolver: Option<FormatResolverFn>,
}

impl StreamDescriptor {
    pub fn resolve_path(&self, stream_path: &Path) -> Option<PathBuf> {
        match &self.path_resolver {
            PathResolverKind::Identity => Some(stream_path.to_path_buf()),
            PathResolverKind::Sibling { filename } => {
                stream_path.parent().map(|p| p.join(filename))
            }
            PathResolverKind::Custom(f) => f(stream_path),
        }
    }

    pub fn effective_watermark_type(
        &self,
        resolved_path: &Path,
    ) -> super::watermark::WatermarkType {
        if let Some(resolver) = &self.watermark_type_resolver {
            resolver(resolved_path)
        } else {
            self.watermark_type
        }
    }

    pub fn effective_format(&self, resolved_path: &Path) -> StreamFormat {
        if let Some(resolver) = &self.format_resolver {
            resolver(resolved_path)
        } else {
            self.format
        }
    }
}

/// Unified trait for transcript agents.
///
/// Combines sweep discovery and incremental reading in one interface.
/// Agents that don't support sweeping return `SweepStrategy::None`.
pub trait Agent: Send + Sync {
    /// Returns the sweep strategy for this agent.
    fn sweep_strategy(&self) -> SweepStrategy;

    /// Discover all sessions in the agent's storage.
    ///
    /// Returns ALL sessions found, regardless of whether they're in transcripts-db.
    /// The coordinator will compare against the DB to decide what to process.
    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError>;

    /// Maximum number of events to return per `read_incremental` call.
    /// Bounds peak memory to batch_size × avg_event_size instead of file_size.
    /// The caller loops until an empty batch is returned.
    fn batch_size_hint(&self) -> usize {
        1000
    }

    /// Read transcript incrementally from the given watermark.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the transcript file
    /// * `watermark` - Current watermark position to resume from
    /// * `session_id` - Source session ID. Readers use this to filter multiplexed sources.
    fn read_incremental(
        &self,
        path: &Path,
        watermark: Box<dyn WatermarkStrategy>,
        session_id: &str,
    ) -> Result<StreamBatch, StreamError>;

    /// Extract per-event external IDs from a raw transcript event.
    ///
    /// Returns (external_event_id, external_parent_event_id, external_tool_use_id).
    /// Agents that don't have event-level identifiers return (None, None, None).
    fn extract_event_ids(
        &self,
        _event: &serde_json::Value,
    ) -> (Option<String>, Option<String>, Option<String>) {
        (None, None, None)
    }

    /// Extract the event timestamp as seconds since Unix epoch.
    ///
    /// Every agent MUST provide a concrete timestamp for each event. Agents with
    /// per-event timestamps in JSON should parse them; agents without should fall
    /// back to file metadata (birthtime for first event, mtime for others).
    fn extract_event_timestamp(
        &self,
        event: &serde_json::Value,
        file_meta: &std::fs::Metadata,
        is_first_event: bool,
    ) -> u32;

    /// Extract the per-event session identifier from a raw event.
    ///
    /// For shared data sources (e.g., a global OTEL DB covering multiple sessions),
    /// returns the session identifier embedded in the event itself. The worker uses
    /// this to derive the correct session_id per emitted MetricEvent.
    ///
    /// Returns None to use the session record's session_id as-is.
    fn extract_event_session_id(&self, _event: &serde_json::Value) -> Option<String> {
        None
    }

    /// Infer the working directory from the transcript file content.
    ///
    /// Reads the first few lines of the transcript looking for a `cwd` field.
    /// Returns None if the agent format doesn't include cwd or it can't be found.
    fn infer_cwd(&self, _stream_path: &Path) -> Option<std::path::PathBuf> {
        None
    }

    /// Returns the stream descriptors for this agent.
    fn streams(&self) -> Vec<StreamDescriptor>;
}

/// Fallback timestamp from file metadata when an event lacks a per-event timestamp.
/// Uses birthtime (creation time) for the first event, mtime for all others.
pub fn file_time_fallback(meta: &std::fs::Metadata, is_first_event: bool) -> u32 {
    let time = if is_first_event {
        meta.created().or_else(|_| meta.modified()).ok()
    } else {
        meta.modified().ok()
    };
    time.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as u32)
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32
        })
}

const ALL_AGENT_TYPES: &[&str] = &[
    "claude",
    "cursor",
    "droid",
    "copilot",
    "copilot-cli",
    "gemini",
    "continue-cli",
    "windsurf",
    "codex",
    "amp",
    "opencode",
    "pi",
];

/// Get an agent implementation by type name.
///
/// Returns None for agents without sweep/read support (e.g., "human", "mock_ai").
pub fn get_agent(agent_type: &str) -> Option<Box<dyn Agent>> {
    match agent_type {
        "claude" => Some(Box::new(super::agents::ClaudeAgent::new())),
        "cursor" => Some(Box::new(super::agents::CursorAgent::new())),
        "droid" => Some(Box::new(super::agents::DroidAgent::new())),
        "copilot" | "github-copilot" => Some(Box::new(super::agents::CopilotAgent::new())),
        "copilot-cli" | "github-copilot-cli" => {
            Some(Box::new(super::agents::CopilotCliAgent::new()))
        }
        "gemini" => Some(Box::new(super::agents::GeminiAgent::new())),
        "continue-cli" => Some(Box::new(super::agents::ContinueAgent::new())),
        "windsurf" => Some(Box::new(super::agents::WindsurfAgent::new())),
        "codex" => Some(Box::new(super::agents::CodexAgent::new())),
        "amp" => Some(Box::new(super::agents::AmpAgent::new())),
        "opencode" => Some(Box::new(super::agents::OpenCodeAgent::new())),
        "pi" => Some(Box::new(super::agents::PiAgent::new())),
        _ => None,
    }
}

/// Get all registered agents as (type_name, agent) pairs.
pub fn get_all_agents() -> Vec<(String, Box<dyn Agent>)> {
    ALL_AGENT_TYPES
        .iter()
        .filter_map(|&name| get_agent(name).map(|agent| (name.to_string(), agent)))
        .collect()
}
