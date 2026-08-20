use crate::model::attribution::{Attribution, LineAttribution};
use crate::model::authorship_log::{HumanRecord, PromptRecord, SessionRecord};
use crate::model::authorship_log_serialization::GIT_AI_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};

pub const CHECKPOINT_API_VERSION: &str = "checkpoint/1.0.0";

/// Initial attributions data structure stored in the INITIAL file
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InitialAttributions {
    /// Map of file path to line attributions
    pub files: HashMap<String, Vec<LineAttribution>>,
    /// Map of author_id (hash) to PromptRecord for prompt tracking
    pub prompts: HashMap<String, PromptRecord>,
    /// Optional blob snapshot of the file content represented by INITIAL.
    #[serde(default)]
    pub file_blobs: HashMap<String, String>,
    /// Known human records: `h_<hash>` -> HumanRecord
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub humans: std::collections::BTreeMap<String, HumanRecord>,
    /// Session records: `s_<session_id>` -> SessionRecord
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub sessions: std::collections::BTreeMap<String, SessionRecord>,
}

/// Represents a working log entry for a specific file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkingLogEntry {
    /// The file path relative to the repository root
    pub file: String,
    /// SHA256 hash of the file content at this checkpoint
    #[serde(default)]
    pub blob_sha: String,
    #[serde(default)]
    pub attributions: Vec<Attribution>,
    #[serde(default)]
    pub line_attributions: Vec<LineAttribution>,
}

impl WorkingLogEntry {
    /// Create a new working log entry
    pub fn new(
        file: String,
        blob_sha: String,
        attributions: Vec<Attribution>,
        line_attributions: Vec<LineAttribution>,
    ) -> Self {
        Self {
            file,
            blob_sha,
            attributions,
            line_attributions,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentId {
    pub tool: String, // e.g., "cursor", "windsurf"
    pub id: String,   // id in their domain
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckpointKind {
    Human,
    AiAgent,
    AiTab,
    KnownHuman,
}

impl fmt::Display for CheckpointKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_str())
    }
}

impl CheckpointKind {
    #[allow(dead_code)]
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "human" => CheckpointKind::Human,
            "ai_agent" => CheckpointKind::AiAgent,
            "ai_tab" => CheckpointKind::AiTab,
            "known_human" => CheckpointKind::KnownHuman,
            _ => panic!("Invalid checkpoint kind: {}", s),
        }
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn to_str(&self) -> String {
        match self {
            CheckpointKind::Human => "human".to_string(),
            CheckpointKind::AiAgent => "ai_agent".to_string(),
            CheckpointKind::AiTab => "ai_tab".to_string(),
            CheckpointKind::KnownHuman => "known_human".to_string(),
        }
    }

    /// Returns true if this checkpoint kind represents AI-generated content.
    pub fn is_ai(self) -> bool {
        matches!(self, CheckpointKind::AiAgent | CheckpointKind::AiTab)
    }

    /// Default value to prevent crashes on old versions
    pub fn serde_default() -> Self {
        CheckpointKind::Human
    }
}

/// Metadata stored for KnownHuman checkpoints, identifying the IDE that fired the save event
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnownHumanMetadata {
    pub editor: String,            // e.g. "vscode"
    pub editor_version: String,    // e.g. "1.85.0"
    pub extension_version: String, // e.g. "0.4.1"
}

/// Line-level statistics tracked per checkpoint kind
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CheckpointLineStats {
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
    #[serde(default)]
    pub additions_sloc: u32,
    #[serde(default)]
    pub deletions_sloc: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    #[serde(default = "CheckpointKind::serde_default")]
    pub kind: CheckpointKind,
    pub diff: String,
    pub author: String,
    pub entries: Vec<WorkingLogEntry>,
    pub timestamp: u64,
    pub agent_id: Option<AgentId>,
    #[serde(default)]
    pub agent_metadata: Option<HashMap<String, String>>,
    #[serde(default)]
    pub line_stats: CheckpointLineStats,
    #[serde(default)]
    pub api_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ai_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_human_metadata: Option<KnownHumanMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Delivery-envelope identity for checkpoints applied from a
    /// `CheckpointDeliver` request. At-least-once outbox replay checks this
    /// before appending so a delivery is never applied twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_id: Option<String>,
}

impl Checkpoint {
    pub fn new(
        kind: CheckpointKind,
        diff: String,
        author: String,
        entries: Vec<WorkingLogEntry>,
    ) -> Self {
        let timestamp = crate::model::clock::now_secs();

        Self {
            kind,
            diff,
            author,
            entries,
            timestamp,
            agent_id: None,
            agent_metadata: None,
            line_stats: CheckpointLineStats::default(),
            api_version: CHECKPOINT_API_VERSION.to_string(),
            git_ai_version: Some(GIT_AI_VERSION.to_string()),
            known_human_metadata: None,
            trace_id: None,
            delivery_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkpoint_serialization() {
        let entry = WorkingLogEntry::new(
            "src/xyz.rs".to_string(),
            "abc123def456".to_string(),
            Vec::new(),
            Vec::new(),
        );
        let checkpoint = Checkpoint::new(
            CheckpointKind::AiAgent,
            "".to_string(),
            "claude".to_string(),
            vec![entry],
        );

        // Verify timestamp is set (should be recent)
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        assert!(checkpoint.timestamp > 0);
        assert!(checkpoint.timestamp <= current_time);
        // Transcript field removed from Checkpoint
        assert!(checkpoint.agent_id.is_none());

        let json = serde_json::to_string_pretty(&checkpoint).unwrap();
        let deserialized: Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.diff, "");
        assert_eq!(deserialized.entries.len(), 1);
        assert_eq!(deserialized.entries[0].file, "src/xyz.rs");
        assert_eq!(deserialized.entries[0].blob_sha, "abc123def456");
        assert_eq!(deserialized.timestamp, checkpoint.timestamp);
        // Transcript field removed from Checkpoint
        assert!(deserialized.agent_id.is_none());
    }

    #[test]
    fn test_log_array_serialization() {
        let entry1 = WorkingLogEntry::new(
            "src/xyz.rs".to_string(),
            "sha1".to_string(),
            Vec::new(),
            Vec::new(),
        );
        let checkpoint1 = Checkpoint::new(
            CheckpointKind::AiAgent,
            "".to_string(),
            "claude".to_string(),
            vec![entry1],
        );

        let entry2 = WorkingLogEntry::new(
            "src/xyz.rs".to_string(),
            "sha2".to_string(),
            Vec::new(),
            Vec::new(),
        );
        let checkpoint2 = Checkpoint::new(
            CheckpointKind::AiAgent,
            "/refs/ai/working/xyz.patch".to_string(),
            "user".to_string(),
            vec![entry2],
        );

        // Verify timestamps are set and checkpoint2 is newer than checkpoint1
        assert!(checkpoint1.timestamp > 0);
        assert!(checkpoint2.timestamp > 0);
        assert!(checkpoint2.timestamp >= checkpoint1.timestamp);

        let log = vec![checkpoint1, checkpoint2];
        let json = serde_json::to_string_pretty(&log).unwrap();
        // println!("Working log array JSON:\n{}", json);
        let deserialized: Vec<Checkpoint> = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.len(), 2);
        assert_eq!(deserialized[0].diff, "");
        assert_eq!(deserialized[1].diff, "/refs/ai/working/xyz.patch");
        assert_eq!(deserialized[1].author, "user");
    }

    #[test]
    fn test_checkpoint_kind_known_human_roundtrip() {
        let kind = CheckpointKind::KnownHuman;
        assert_eq!(kind.to_str(), "known_human");
        assert_eq!(
            CheckpointKind::from_str("known_human"),
            CheckpointKind::KnownHuman
        );
        // Serde round-trip
        let json = serde_json::to_string(&kind).unwrap();
        let back: CheckpointKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, CheckpointKind::KnownHuman);
    }

    #[test]
    fn test_is_ai_returns_false_for_human_kinds() {
        assert!(!CheckpointKind::Human.is_ai());
        assert!(!CheckpointKind::KnownHuman.is_ai());
    }

    #[test]
    fn test_is_ai_returns_true_for_ai_kinds() {
        assert!(CheckpointKind::AiAgent.is_ai());
        assert!(CheckpointKind::AiTab.is_ai());
    }

    #[test]
    fn test_checkpoint_with_known_human_metadata_roundtrip() {
        use crate::model::working_log::{Checkpoint, KnownHumanMetadata};
        let mut checkpoint = Checkpoint::new(
            CheckpointKind::KnownHuman,
            "diff".to_string(),
            "Alice <alice@example.com>".to_string(),
            vec![],
        );
        checkpoint.known_human_metadata = Some(KnownHumanMetadata {
            editor: "vscode".to_string(),
            editor_version: "1.85.0".to_string(),
            extension_version: "0.4.1".to_string(),
        });
        // Serde round-trip
        let json = serde_json::to_string(&checkpoint).unwrap();
        let back: Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, CheckpointKind::KnownHuman);
        let meta = back.known_human_metadata.unwrap();
        assert_eq!(meta.editor, "vscode");
        assert_eq!(meta.editor_version, "1.85.0");
        assert_eq!(meta.extension_version, "0.4.1");
    }

    #[test]
    fn test_checkpoint_trace_id_backwards_compat() {
        // Old JSON without trace_id should deserialize with trace_id = None
        let json = r#"{
            "kind": "AiAgent",
            "diff": "",
            "author": "claude",
            "entries": [],
            "timestamp": 1234567890,
            "transcript": null,
            "agent_id": {"tool": "claude", "id": "sess1", "model": "opus"},
            "line_stats": {"additions": 0, "deletions": 0, "additions_sloc": 0, "deletions_sloc": 0},
            "api_version": "checkpoint/1.0.0"
        }"#;
        let checkpoint: Checkpoint = serde_json::from_str(json).unwrap();
        assert_eq!(checkpoint.trace_id, None);

        // New JSON with trace_id should deserialize correctly
        let json_with_trace = r#"{
            "kind": "AiAgent",
            "diff": "",
            "author": "claude",
            "entries": [],
            "timestamp": 1234567890,
            "transcript": null,
            "agent_id": {"tool": "claude", "id": "sess1", "model": "opus"},
            "line_stats": {"additions": 0, "deletions": 0, "additions_sloc": 0, "deletions_sloc": 0},
            "api_version": "checkpoint/1.0.0",
            "trace_id": "t_abcdef01234567"
        }"#;
        let checkpoint: Checkpoint = serde_json::from_str(json_with_trace).unwrap();
        assert_eq!(checkpoint.trace_id, Some("t_abcdef01234567".to_string()));
    }
}
