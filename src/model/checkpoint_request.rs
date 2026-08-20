//! Wire-shape DTOs for the CLI→daemon checkpoint chain.
//!
//! These types are serialized over the daemon control socket.  Serde attributes
//! and field order are preserved byte-for-byte from their original locations.

use crate::model::working_log::{AgentId, CheckpointKind};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// The base commit a checkpoint file is diffed against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BaseCommit {
    Sha(String),
    Initial,
}

/// A single file captured in a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointFile {
    pub path: PathBuf,
    pub content: Option<String>,
    pub repo_work_dir: PathBuf,
    pub base_commit: BaseCommit,
}

/// Whether a prepared path was edited or will be edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparedPathRole {
    Edited,
    WillEdit,
}

/// The transcript file format associated with a stream source.
///
/// NOTE: this is the *checkpoint-wire* variant — it is a separate type from
/// `crate::model::stream_types::StreamFormat` which includes additional
/// variants (e.g. `CopilotOtelSqlite`) not present on the checkpoint wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamFormat {
    ClaudeJsonl,
    ContinueJson,
    GeminiJsonl,
    WindsurfJsonl,
    CodexJsonl,
    CursorJsonl,
    DroidJsonl,
    CopilotSessionJson,
    CopilotEventStreamJsonl,
    AmpThreadJson,
    OpenCodeSqlite,
    PiJsonl,
}

impl StreamFormat {
    pub fn watermark_type(self) -> crate::model::stream_watermark::WatermarkType {
        use crate::model::stream_watermark::WatermarkType;
        match self {
            Self::ClaudeJsonl
            | Self::CursorJsonl
            | Self::GeminiJsonl
            | Self::WindsurfJsonl
            | Self::CodexJsonl
            | Self::PiJsonl
            | Self::CopilotEventStreamJsonl => WatermarkType::ByteOffset,
            Self::DroidJsonl => WatermarkType::Hybrid,
            Self::CopilotSessionJson | Self::ContinueJson | Self::AmpThreadJson => {
                WatermarkType::RecordIndex
            }
            Self::OpenCodeSqlite => WatermarkType::Timestamp,
        }
    }
}

/// Identifies the transcript file to be streamed alongside a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamSource {
    pub path: PathBuf,
    pub format: StreamFormat,
    /// Session ID for this transcript (used to query/create session in DB).
    pub session_id: String,
    /// External thread/conversation ID (agent-specific identifier).
    pub external_session_id: String,
    /// Parent session ID for subagent transcripts.
    #[serde(default)]
    pub external_parent_session_id: Option<String>,
}

/// A fully-resolved checkpoint request sent from the CLI to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointRequest {
    pub trace_id: String,
    pub checkpoint_kind: CheckpointKind,
    pub agent_id: Option<AgentId>,
    pub files: Vec<CheckpointFile>,
    pub path_role: PreparedPathRole,
    pub stream_source: Option<StreamSource>,
    pub metadata: HashMap<String, String>,
    /// Identity of the checkpoint-delivery envelope this request arrived in,
    /// stamped daemon-side at ingestion. Persisted into the applied
    /// `Checkpoint` so at-least-once outbox replay can deduplicate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_id: Option<String>,
}
