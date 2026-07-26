// src/streams/agent.rs

use super::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::model::authorship_log_serialization::generate_session_id;
use crate::model::checkpoint_request::{
    StreamFormat as CheckpointStreamFormat, StreamSource as CheckpointStreamSource,
};
use crate::model::stream_types::{StreamBatch, StreamError};
use crate::model::stream_watermark::WatermarkStrategy;
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
    Box<dyn Fn(&Path) -> crate::model::stream_watermark::WatermarkType + Send + Sync>;
pub type FormatResolverFn = Box<dyn Fn(&Path) -> StreamFormat + Send + Sync>;

pub struct StreamDescriptor {
    pub stream_kind: &'static str,
    pub format: StreamFormat,
    pub watermark_type: crate::model::stream_watermark::WatermarkType,
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
    pub(crate) fn identity_transcript(format: StreamFormat) -> Self {
        Self {
            stream_kind: "transcript",
            format,
            watermark_type: format.watermark_type(),
            path_resolver: PathResolverKind::Identity,
            shared: false,
            watermark_type_resolver: None,
            format_resolver: None,
        }
    }

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
    ) -> crate::model::stream_watermark::WatermarkType {
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
    /// Daemon-owned roots used for sweep discovery and bounded checkpoint validation.
    ///
    /// Checkpoint IPC must never turn an arbitrary caller-supplied path into a
    /// host read. Checkpoint validation resolves one claimed candidate beneath
    /// these roots; it must not require a full `discover_sessions` sweep.
    fn trusted_stream_roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Validate one checkpoint-provided stream candidate without sweeping agent storage.
    ///
    /// Implementations must bind the external session ID to a host-visible candidate
    /// beneath an agent-owned root. The default is intentionally fail-closed.
    fn validate_checkpoint_stream(
        &self,
        _source: &CheckpointStreamSource,
    ) -> Result<DiscoveredSession, StreamError> {
        Err(checkpoint_stream_denied())
    }

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

    /// Select the session identifier passed to `read_incremental`.
    ///
    /// Stream bookkeeping uses the internal ID. Agents whose native storage is
    /// keyed by an external ID can override this reader-boundary mapping.
    fn session_id_for_read<'a>(
        &self,
        session_id: &'a str,
        _external_session_id: &'a str,
    ) -> &'a str {
        session_id
    }

    /// Read transcript incrementally from the given watermark.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the transcript file
    /// * `watermark` - Current watermark position to resume from
    /// * `session_id` - Session ID for context (used in error messages)
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

pub(crate) fn validate_checkpoint_stream_file(
    source: &CheckpointStreamSource,
    expected_tool: &str,
    expected_format: CheckpointStreamFormat,
    trusted_roots: Vec<PathBuf>,
    bind_session: impl FnOnce(&Path) -> Option<(String, Option<String>)>,
) -> Result<DiscoveredSession, StreamError> {
    validate_checkpoint_stream_claim(source, expected_tool, expected_format)?;
    if !source.path.is_absolute() {
        return Err(checkpoint_stream_denied());
    }

    let canonical_path = source
        .path
        .canonicalize()
        .map_err(|_| checkpoint_stream_denied())?;
    if !canonical_path.is_file() {
        return Err(checkpoint_stream_denied());
    }
    let beneath_trusted_root = trusted_roots
        .into_iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| canonical_path.starts_with(root));
    if !beneath_trusted_root {
        return Err(checkpoint_stream_denied());
    }

    let (external_session_id, external_parent_session_id) =
        bind_session(&canonical_path).ok_or_else(checkpoint_stream_denied)?;
    if external_session_id != source.external_session_id {
        return Err(checkpoint_stream_denied());
    }

    Ok(DiscoveredSession {
        session_id: source.session_id.clone(),
        tool: expected_tool.to_string(),
        stream_path: canonical_path,
        external_session_id,
        external_parent_session_id,
    })
}

pub(crate) fn validate_checkpoint_stream_claim(
    source: &CheckpointStreamSource,
    expected_tool: &str,
    expected_format: CheckpointStreamFormat,
) -> Result<(), StreamError> {
    if source.format != expected_format
        || source.external_session_id.trim().is_empty()
        || source.session_id != generate_session_id(&source.external_session_id, expected_tool)
    {
        return Err(checkpoint_stream_denied());
    }
    Ok(())
}

pub(crate) fn checkpoint_stream_denied() -> StreamError {
    StreamError::Fatal {
        message: "checkpoint stream source authority could not be verified".to_string(),
    }
}

pub(crate) fn checkpoint_stream_has_extension(path: &Path, expected: &str) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some(expected)
}

pub(super) fn discover_path_sessions(
    tool: &str,
    paths: impl IntoIterator<Item = PathBuf>,
    mut bind_session: impl FnMut(&Path) -> Option<(String, Option<String>)>,
) -> Vec<DiscoveredSession> {
    paths
        .into_iter()
        .filter_map(|stream_path| {
            let (external_session_id, external_parent_session_id) = bind_session(&stream_path)?;
            Some(DiscoveredSession {
                session_id: generate_session_id(&external_session_id, tool),
                tool: tool.to_string(),
                stream_path,
                external_session_id,
                external_parent_session_id,
            })
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_transcript_descriptor_has_the_standard_policy() {
        let descriptor = StreamDescriptor::identity_transcript(StreamFormat::AmpThreadJson);

        assert_eq!(descriptor.stream_kind, "transcript");
        assert_eq!(descriptor.format, StreamFormat::AmpThreadJson);
        assert_eq!(
            descriptor.watermark_type,
            StreamFormat::AmpThreadJson.watermark_type()
        );
        assert!(matches!(
            descriptor.path_resolver,
            PathResolverKind::Identity
        ));
        assert!(!descriptor.shared);
        assert!(descriptor.watermark_type_resolver.is_none());
        assert!(descriptor.format_resolver.is_none());
    }

    #[test]
    fn path_discovery_preserves_input_order_and_parent_binding() {
        let paths = vec![
            PathBuf::from("/sessions/beta.json"),
            PathBuf::from("/sessions/skip.json"),
            PathBuf::from("/sessions/alpha.json"),
        ];

        let sessions = discover_path_sessions("test-tool", paths, |path| {
            let external_id = path.file_stem()?.to_str()?.to_string();
            if external_id == "skip" {
                return None;
            }
            let parent = (external_id == "alpha").then(|| "parent-session".to_string());
            Some((external_id, parent))
        });

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].external_session_id, "beta");
        assert_eq!(
            sessions[0].stream_path,
            PathBuf::from("/sessions/beta.json")
        );
        assert_eq!(sessions[0].external_parent_session_id, None);
        assert_eq!(sessions[1].external_session_id, "alpha");
        assert_eq!(
            sessions[1].external_parent_session_id.as_deref(),
            Some("parent-session")
        );
        assert_eq!(sessions[0].tool, "test-tool");
        assert_eq!(
            sessions[0].session_id,
            generate_session_id("beta", "test-tool")
        );
    }
}
