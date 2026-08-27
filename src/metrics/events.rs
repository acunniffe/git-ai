//! Event-specific value structs for metrics.

use super::pos_encoded::{
    PosEncoded, PosField, f64_to_json, sparse_get_f64, sparse_get_string, sparse_get_u32,
    sparse_get_u64, sparse_get_vec_string, sparse_get_vec_u32, sparse_set, string_to_json,
    u32_to_json, u64_to_json, vec_string_to_json, vec_u32_to_json,
};
use super::types::{EventValues, MetricEventId, SparseArray};

/// Value positions for "committed" event.
pub mod committed_pos {
    // Scalar fields
    pub const HUMAN_ADDITIONS: usize = 0;
    pub const GIT_DIFF_DELETED_LINES: usize = 1;
    pub const GIT_DIFF_ADDED_LINES: usize = 2;

    // Array fields (parallel arrays, index 0 = "all" aggregate, index 1+ = per tool/model)
    pub const TOOL_MODEL_PAIRS: usize = 3;
    pub const MIXED_ADDITIONS: usize = 4;
    pub const AI_ADDITIONS: usize = 5;
    pub const AI_ACCEPTED: usize = 6;
    pub const TOTAL_AI_ADDITIONS: usize = 7;
    pub const TOTAL_AI_DELETIONS: usize = 8;
    // Position 9 was time_waiting_for_ai (removed)

    // New scalar fields
    pub const FIRST_CHECKPOINT_TS: usize = 10; // u64 (null if no checkpoints)
    pub const COMMIT_SUBJECT: usize = 11; // String
    pub const COMMIT_BODY: usize = 12; // String (null if empty)
    pub const AUTHORSHIP_NOTE: usize = 13; // String (full serialized authorship note)
    pub const HUNKS: usize = 14; // String (JSON array of DiffJsonHunk)
    pub const AUTHOR_TS: usize = 15; // u64 (git author timestamp, %at)
    pub const COMMIT_TS: usize = 16; // u64 (git committer timestamp, %ct)
    pub const PATCH_ID: usize = 17; // String (git patch-id --stable)
}

/// Values for Event ID 1: committed
///
/// Recorded when AI-assisted code is committed.
///
/// **Scalar fields:**
/// | Position | Name | Type |
/// |----------|------|------|
/// | 0 | human_additions | u32 |
/// | 1 | git_diff_deleted_lines | u32 |
/// | 2 | git_diff_added_lines | u32 |
///
/// **Array fields (parallel arrays, index 0 = "all" for aggregate, index 1+ = per tool/model):**
/// | Position | Name | Type |
/// |----------|------|------|
/// | 3 | tool_model_pairs | `Vec<String>` |
/// | 4 | (removed) | - |
/// | 5 | ai_additions | `Vec<u32>` |
/// | 6 | ai_accepted | `Vec<u32>` |
/// | 7 | (removed) | - |
/// | 8 | (removed) | - |
/// | 9 | (removed) | - |
/// | 10 | first_checkpoint_ts | u64 |
/// | 11 | commit_subject | String |
/// | 12 | commit_body | String |
/// | 13 | authorship_note | String |
/// | 14 | hunks | String |
/// | 15 | author_ts | u64 |
/// | 16 | commit_ts | u64 |
/// | 17 | patch_id | String |
#[derive(Debug, Clone, Default)]
pub struct CommittedValues {
    // Scalar fields
    pub human_additions: PosField<u32>,
    pub git_diff_deleted_lines: PosField<u32>,
    pub git_diff_added_lines: PosField<u32>,

    // Array fields (parallel arrays)
    pub tool_model_pairs: PosField<Vec<String>>,
    pub ai_additions: PosField<Vec<u32>>,
    pub ai_accepted: PosField<Vec<u32>>,

    // New scalar fields
    pub first_checkpoint_ts: PosField<u64>,
    pub commit_subject: PosField<String>,
    pub commit_body: PosField<String>,
    pub authorship_note: PosField<String>,
    pub hunks: PosField<String>,
    pub author_ts: PosField<u64>,
    pub commit_ts: PosField<u64>,
    pub patch_id: PosField<String>,
}

impl CommittedValues {
    pub fn new() -> Self {
        Self::default()
    }

    // Builder methods for scalar fields

    pub fn human_additions(mut self, value: u32) -> Self {
        self.human_additions = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn human_additions_null(mut self) -> Self {
        self.human_additions = Some(None);
        self
    }

    pub fn git_diff_deleted_lines(mut self, value: u32) -> Self {
        self.git_diff_deleted_lines = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn git_diff_deleted_lines_null(mut self) -> Self {
        self.git_diff_deleted_lines = Some(None);
        self
    }

    pub fn git_diff_added_lines(mut self, value: u32) -> Self {
        self.git_diff_added_lines = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn git_diff_added_lines_null(mut self) -> Self {
        self.git_diff_added_lines = Some(None);
        self
    }

    // Builder methods for array fields

    pub fn tool_model_pairs(mut self, value: Vec<String>) -> Self {
        self.tool_model_pairs = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn tool_model_pairs_null(mut self) -> Self {
        self.tool_model_pairs = Some(None);
        self
    }

    pub fn ai_additions(mut self, value: Vec<u32>) -> Self {
        self.ai_additions = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn ai_additions_null(mut self) -> Self {
        self.ai_additions = Some(None);
        self
    }

    pub fn ai_accepted(mut self, value: Vec<u32>) -> Self {
        self.ai_accepted = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn ai_accepted_null(mut self) -> Self {
        self.ai_accepted = Some(None);
        self
    }

    // Builder methods for new scalar fields

    pub fn first_checkpoint_ts(mut self, value: u64) -> Self {
        self.first_checkpoint_ts = Some(Some(value));
        self
    }

    pub fn first_checkpoint_ts_null(mut self) -> Self {
        self.first_checkpoint_ts = Some(None);
        self
    }

    pub fn commit_subject(mut self, value: impl Into<String>) -> Self {
        self.commit_subject = Some(Some(value.into()));
        self
    }

    pub fn commit_subject_null(mut self) -> Self {
        self.commit_subject = Some(None);
        self
    }

    pub fn commit_body(mut self, value: impl Into<String>) -> Self {
        self.commit_body = Some(Some(value.into()));
        self
    }

    pub fn commit_body_null(mut self) -> Self {
        self.commit_body = Some(None);
        self
    }

    pub fn authorship_note(mut self, value: impl Into<String>) -> Self {
        self.authorship_note = Some(Some(value.into()));
        self
    }

    pub fn authorship_note_null(mut self) -> Self {
        self.authorship_note = Some(None);
        self
    }

    pub fn hunks(mut self, value: impl Into<String>) -> Self {
        self.hunks = Some(Some(value.into()));
        self
    }

    pub fn hunks_null(mut self) -> Self {
        self.hunks = Some(None);
        self
    }

    pub fn author_ts(mut self, value: u64) -> Self {
        self.author_ts = Some(Some(value));
        self
    }

    pub fn author_ts_null(mut self) -> Self {
        self.author_ts = Some(None);
        self
    }

    pub fn commit_ts(mut self, value: u64) -> Self {
        self.commit_ts = Some(Some(value));
        self
    }

    pub fn commit_ts_null(mut self) -> Self {
        self.commit_ts = Some(None);
        self
    }

    pub fn patch_id(mut self, value: impl Into<String>) -> Self {
        self.patch_id = Some(Some(value.into()));
        self
    }

    pub fn patch_id_null(mut self) -> Self {
        self.patch_id = Some(None);
        self
    }
}

impl PosEncoded for CommittedValues {
    fn to_sparse(&self) -> SparseArray {
        let mut map = SparseArray::new();

        // Scalar fields
        sparse_set(
            &mut map,
            committed_pos::HUMAN_ADDITIONS,
            u32_to_json(&self.human_additions),
        );
        sparse_set(
            &mut map,
            committed_pos::GIT_DIFF_DELETED_LINES,
            u32_to_json(&self.git_diff_deleted_lines),
        );
        sparse_set(
            &mut map,
            committed_pos::GIT_DIFF_ADDED_LINES,
            u32_to_json(&self.git_diff_added_lines),
        );

        // Array fields
        sparse_set(
            &mut map,
            committed_pos::TOOL_MODEL_PAIRS,
            vec_string_to_json(&self.tool_model_pairs),
        );
        sparse_set(
            &mut map,
            committed_pos::AI_ADDITIONS,
            vec_u32_to_json(&self.ai_additions),
        );
        sparse_set(
            &mut map,
            committed_pos::AI_ACCEPTED,
            vec_u32_to_json(&self.ai_accepted),
        );

        // New scalar fields
        sparse_set(
            &mut map,
            committed_pos::FIRST_CHECKPOINT_TS,
            u64_to_json(&self.first_checkpoint_ts),
        );
        sparse_set(
            &mut map,
            committed_pos::COMMIT_SUBJECT,
            string_to_json(&self.commit_subject),
        );
        sparse_set(
            &mut map,
            committed_pos::COMMIT_BODY,
            string_to_json(&self.commit_body),
        );
        sparse_set(
            &mut map,
            committed_pos::AUTHORSHIP_NOTE,
            string_to_json(&self.authorship_note),
        );
        sparse_set(&mut map, committed_pos::HUNKS, string_to_json(&self.hunks));
        sparse_set(
            &mut map,
            committed_pos::AUTHOR_TS,
            u64_to_json(&self.author_ts),
        );
        sparse_set(
            &mut map,
            committed_pos::COMMIT_TS,
            u64_to_json(&self.commit_ts),
        );
        sparse_set(
            &mut map,
            committed_pos::PATCH_ID,
            string_to_json(&self.patch_id),
        );

        map
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        Self {
            // Scalar fields
            human_additions: sparse_get_u32(arr, committed_pos::HUMAN_ADDITIONS),
            git_diff_deleted_lines: sparse_get_u32(arr, committed_pos::GIT_DIFF_DELETED_LINES),
            git_diff_added_lines: sparse_get_u32(arr, committed_pos::GIT_DIFF_ADDED_LINES),

            // Array fields
            tool_model_pairs: sparse_get_vec_string(arr, committed_pos::TOOL_MODEL_PAIRS),
            ai_additions: sparse_get_vec_u32(arr, committed_pos::AI_ADDITIONS),
            ai_accepted: sparse_get_vec_u32(arr, committed_pos::AI_ACCEPTED),

            // New scalar fields
            first_checkpoint_ts: sparse_get_u64(arr, committed_pos::FIRST_CHECKPOINT_TS),
            commit_subject: sparse_get_string(arr, committed_pos::COMMIT_SUBJECT),
            commit_body: sparse_get_string(arr, committed_pos::COMMIT_BODY),
            authorship_note: sparse_get_string(arr, committed_pos::AUTHORSHIP_NOTE),
            hunks: sparse_get_string(arr, committed_pos::HUNKS),
            author_ts: sparse_get_u64(arr, committed_pos::AUTHOR_TS),
            commit_ts: sparse_get_u64(arr, committed_pos::COMMIT_TS),
            patch_id: sparse_get_string(arr, committed_pos::PATCH_ID),
        }
    }
}

impl EventValues for CommittedValues {
    fn event_id() -> MetricEventId {
        MetricEventId::Committed
    }

    fn to_sparse(&self) -> SparseArray {
        PosEncoded::to_sparse(self)
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        PosEncoded::from_sparse(arr)
    }
}

/// Value positions for "rewrite_committed" event.
pub mod rewrite_committed_pos {
    pub const HUMAN_ADDITIONS: usize = 0;
    pub const GIT_DIFF_DELETED_LINES: usize = 1;
    pub const GIT_DIFF_ADDED_LINES: usize = 2;
    pub const TOOL_MODEL_PAIRS: usize = 3;
    // Keep positions 0-14 aligned with committed_pos for ingestion consistency.
    // Position 4 mirrors committed_pos::MIXED_ADDITIONS, which is no longer emitted.
    pub const AI_ADDITIONS: usize = 5;
    pub const AI_ACCEPTED: usize = 6;
    // Positions 7-9 mirror removed committed event fields.
    // Position 10 is intentionally omitted: rewrite events have no first checkpoint timestamp.
    pub const COMMIT_SUBJECT: usize = 11;
    pub const COMMIT_BODY: usize = 12;
    pub const AUTHORSHIP_NOTE: usize = 13;
    pub const HUNKS: usize = 14;
    pub const OPERATION_KIND: usize = 15;
    pub const ORIGINAL_COMMIT_SHAS: usize = 16;
}

/// Values for Event ID 7: rewrite_committed.
///
/// Recorded after rewrite operations create new commit SHAs and authorship
/// notes have been migrated to those post-rewrite commits.
#[derive(Debug, Clone, Default)]
pub struct RewriteCommittedValues {
    pub human_additions: PosField<u32>,
    pub git_diff_deleted_lines: PosField<u32>,
    pub git_diff_added_lines: PosField<u32>,
    pub tool_model_pairs: PosField<Vec<String>>,
    pub ai_additions: PosField<Vec<u32>>,
    pub ai_accepted: PosField<Vec<u32>>,
    pub commit_subject: PosField<String>,
    pub commit_body: PosField<String>,
    pub authorship_note: PosField<String>,
    pub hunks: PosField<String>,
    pub operation_kind: PosField<String>,
    pub original_commit_shas: PosField<Vec<String>>,
}

impl RewriteCommittedValues {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn human_additions(mut self, value: u32) -> Self {
        self.human_additions = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn human_additions_null(mut self) -> Self {
        self.human_additions = Some(None);
        self
    }

    pub fn git_diff_deleted_lines(mut self, value: u32) -> Self {
        self.git_diff_deleted_lines = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn git_diff_deleted_lines_null(mut self) -> Self {
        self.git_diff_deleted_lines = Some(None);
        self
    }

    pub fn git_diff_added_lines(mut self, value: u32) -> Self {
        self.git_diff_added_lines = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn git_diff_added_lines_null(mut self) -> Self {
        self.git_diff_added_lines = Some(None);
        self
    }

    pub fn tool_model_pairs(mut self, value: Vec<String>) -> Self {
        self.tool_model_pairs = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn tool_model_pairs_null(mut self) -> Self {
        self.tool_model_pairs = Some(None);
        self
    }

    pub fn ai_additions(mut self, value: Vec<u32>) -> Self {
        self.ai_additions = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn ai_additions_null(mut self) -> Self {
        self.ai_additions = Some(None);
        self
    }

    pub fn ai_accepted(mut self, value: Vec<u32>) -> Self {
        self.ai_accepted = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn ai_accepted_null(mut self) -> Self {
        self.ai_accepted = Some(None);
        self
    }

    pub fn commit_subject(mut self, value: impl Into<String>) -> Self {
        self.commit_subject = Some(Some(value.into()));
        self
    }

    pub fn commit_subject_null(mut self) -> Self {
        self.commit_subject = Some(None);
        self
    }

    pub fn commit_body(mut self, value: impl Into<String>) -> Self {
        self.commit_body = Some(Some(value.into()));
        self
    }

    pub fn commit_body_null(mut self) -> Self {
        self.commit_body = Some(None);
        self
    }

    pub fn authorship_note(mut self, value: impl Into<String>) -> Self {
        self.authorship_note = Some(Some(value.into()));
        self
    }

    pub fn authorship_note_null(mut self) -> Self {
        self.authorship_note = Some(None);
        self
    }

    pub fn hunks(mut self, value: impl Into<String>) -> Self {
        self.hunks = Some(Some(value.into()));
        self
    }

    pub fn hunks_null(mut self) -> Self {
        self.hunks = Some(None);
        self
    }

    pub fn operation_kind(mut self, value: impl Into<String>) -> Self {
        self.operation_kind = Some(Some(value.into()));
        self
    }

    #[allow(dead_code)]
    pub fn operation_kind_null(mut self) -> Self {
        self.operation_kind = Some(None);
        self
    }

    pub fn original_commit_shas(mut self, value: Vec<String>) -> Self {
        self.original_commit_shas = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn original_commit_shas_null(mut self) -> Self {
        self.original_commit_shas = Some(None);
        self
    }
}

impl PosEncoded for RewriteCommittedValues {
    fn to_sparse(&self) -> SparseArray {
        let mut map = SparseArray::new();

        sparse_set(
            &mut map,
            rewrite_committed_pos::HUMAN_ADDITIONS,
            u32_to_json(&self.human_additions),
        );
        sparse_set(
            &mut map,
            rewrite_committed_pos::GIT_DIFF_DELETED_LINES,
            u32_to_json(&self.git_diff_deleted_lines),
        );
        sparse_set(
            &mut map,
            rewrite_committed_pos::GIT_DIFF_ADDED_LINES,
            u32_to_json(&self.git_diff_added_lines),
        );
        sparse_set(
            &mut map,
            rewrite_committed_pos::TOOL_MODEL_PAIRS,
            vec_string_to_json(&self.tool_model_pairs),
        );
        sparse_set(
            &mut map,
            rewrite_committed_pos::AI_ADDITIONS,
            vec_u32_to_json(&self.ai_additions),
        );
        sparse_set(
            &mut map,
            rewrite_committed_pos::AI_ACCEPTED,
            vec_u32_to_json(&self.ai_accepted),
        );
        sparse_set(
            &mut map,
            rewrite_committed_pos::COMMIT_SUBJECT,
            string_to_json(&self.commit_subject),
        );
        sparse_set(
            &mut map,
            rewrite_committed_pos::COMMIT_BODY,
            string_to_json(&self.commit_body),
        );
        sparse_set(
            &mut map,
            rewrite_committed_pos::AUTHORSHIP_NOTE,
            string_to_json(&self.authorship_note),
        );
        sparse_set(
            &mut map,
            rewrite_committed_pos::HUNKS,
            string_to_json(&self.hunks),
        );
        sparse_set(
            &mut map,
            rewrite_committed_pos::OPERATION_KIND,
            string_to_json(&self.operation_kind),
        );
        sparse_set(
            &mut map,
            rewrite_committed_pos::ORIGINAL_COMMIT_SHAS,
            vec_string_to_json(&self.original_commit_shas),
        );

        map
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        Self {
            human_additions: sparse_get_u32(arr, rewrite_committed_pos::HUMAN_ADDITIONS),
            git_diff_deleted_lines: sparse_get_u32(
                arr,
                rewrite_committed_pos::GIT_DIFF_DELETED_LINES,
            ),
            git_diff_added_lines: sparse_get_u32(arr, rewrite_committed_pos::GIT_DIFF_ADDED_LINES),
            tool_model_pairs: sparse_get_vec_string(arr, rewrite_committed_pos::TOOL_MODEL_PAIRS),
            ai_additions: sparse_get_vec_u32(arr, rewrite_committed_pos::AI_ADDITIONS),
            ai_accepted: sparse_get_vec_u32(arr, rewrite_committed_pos::AI_ACCEPTED),
            commit_subject: sparse_get_string(arr, rewrite_committed_pos::COMMIT_SUBJECT),
            commit_body: sparse_get_string(arr, rewrite_committed_pos::COMMIT_BODY),
            authorship_note: sparse_get_string(arr, rewrite_committed_pos::AUTHORSHIP_NOTE),
            hunks: sparse_get_string(arr, rewrite_committed_pos::HUNKS),
            operation_kind: sparse_get_string(arr, rewrite_committed_pos::OPERATION_KIND),
            original_commit_shas: sparse_get_vec_string(
                arr,
                rewrite_committed_pos::ORIGINAL_COMMIT_SHAS,
            ),
        }
    }
}

impl EventValues for RewriteCommittedValues {
    fn event_id() -> MetricEventId {
        MetricEventId::RewriteCommitted
    }

    fn to_sparse(&self) -> SparseArray {
        PosEncoded::to_sparse(self)
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        PosEncoded::from_sparse(arr)
    }
}

/// Values for Event ID 2: agent_usage
///
/// Recorded on every AI checkpoint to track agent usage.
/// Uses attributes (prompt_id, tool, model) rather than event-specific values.
#[derive(Debug, Clone, Default)]
pub struct AgentUsageValues {}

impl AgentUsageValues {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PosEncoded for AgentUsageValues {
    fn to_sparse(&self) -> SparseArray {
        SparseArray::new()
    }

    fn from_sparse(_arr: &SparseArray) -> Self {
        Self::default()
    }
}

impl EventValues for AgentUsageValues {
    fn event_id() -> MetricEventId {
        MetricEventId::AgentUsage
    }

    fn to_sparse(&self) -> SparseArray {
        PosEncoded::to_sparse(self)
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        PosEncoded::from_sparse(arr)
    }
}

/// Value positions for "install_hooks" event.
/// One event per tool attempted during install-hooks.
pub mod install_hooks_pos {
    pub const TOOL_ID: usize = 0; // String - tool id (e.g., "cursor", "vscode")
    pub const STATUS: usize = 1; // String - "not_found", "installed", "already_installed", "failed"
    pub const MESSAGE: usize = 2; // Option<String> - error message or warnings
}

/// Values for Event ID 3: install_hooks
///
/// Recorded for each tool during git-ai install-hooks command.
/// One event per tool attempted.
///
/// **Fields:**
/// | Position | Name | Type |
/// |----------|------|------|
/// | 0 | tool_id | String |
/// | 1 | status | String |
/// | 2 | message | `Option<String>` |
#[derive(Debug, Clone, Default)]
pub struct InstallHooksValues {
    pub tool_id: PosField<String>,
    pub status: PosField<String>,
    pub message: PosField<String>,
}

impl InstallHooksValues {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tool_id(mut self, value: String) -> Self {
        self.tool_id = Some(Some(value));
        self
    }

    pub fn status(mut self, value: String) -> Self {
        self.status = Some(Some(value));
        self
    }

    pub fn message(mut self, value: String) -> Self {
        self.message = Some(Some(value));
        self
    }

    pub fn message_null(mut self) -> Self {
        self.message = Some(None);
        self
    }
}

impl PosEncoded for InstallHooksValues {
    fn to_sparse(&self) -> SparseArray {
        let mut map = SparseArray::new();

        sparse_set(
            &mut map,
            install_hooks_pos::TOOL_ID,
            string_to_json(&self.tool_id),
        );
        sparse_set(
            &mut map,
            install_hooks_pos::STATUS,
            string_to_json(&self.status),
        );
        sparse_set(
            &mut map,
            install_hooks_pos::MESSAGE,
            string_to_json(&self.message),
        );

        map
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        Self {
            tool_id: sparse_get_string(arr, install_hooks_pos::TOOL_ID),
            status: sparse_get_string(arr, install_hooks_pos::STATUS),
            message: sparse_get_string(arr, install_hooks_pos::MESSAGE),
        }
    }
}

impl EventValues for InstallHooksValues {
    fn event_id() -> MetricEventId {
        MetricEventId::InstallHooks
    }

    fn to_sparse(&self) -> SparseArray {
        PosEncoded::to_sparse(self)
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        PosEncoded::from_sparse(arr)
    }
}

/// Value positions for "checkpoint" event.
/// One event per file in the checkpoint.
pub mod checkpoint_pos {
    pub const CHECKPOINT_TS: usize = 0; // u64 - checkpoint timestamp
    pub const KIND: usize = 1; // String ("human", "ai_agent", "ai_tab")
    pub const FILE_PATH: usize = 2; // String - full relative file path
    pub const LINES_ADDED: usize = 3; // u32 - for this file
    pub const LINES_DELETED: usize = 4; // u32 - for this file
    pub const LINES_ADDED_SLOC: usize = 5; // u32 - for this file
    pub const LINES_DELETED_SLOC: usize = 6; // u32 - for this file
    pub const TOOL_USE_ID: usize = 7; // String - nullable
    pub const EDIT_KIND: usize = 8; // String - nullable ("file_edit" | "bash")
    pub const CHECKPOINT_TYPE: usize = 9; // String - nullable ("recovered_bash", etc.)
    pub const ATTRIBUTION_RECOVERY_METADATA: usize = 10; // String - nullable JSON
}

/// Values for Event ID 4: checkpoint
///
/// Recorded for each file in a checkpoint.
/// Uses EventAttributes for standard metadata (repo_url, author, tool, model, etc.)
///
/// **Fields:**
/// | Position | Name | Type |
/// |----------|------|------|
/// | 0 | checkpoint_ts | u64 |
/// | 1 | kind | String |
/// | 2 | file_path | String |
/// | 3 | lines_added | u32 |
/// | 4 | lines_deleted | u32 |
/// | 5 | lines_added_sloc | u32 |
/// | 6 | lines_deleted_sloc | u32 |
/// | 7 | external_tool_use_id | String (nullable) |
/// | 8 | edit_kind | String (nullable) |
/// | 9 | checkpoint_type | String (nullable) |
/// | 10 | attribution_recovery_metadata | String (nullable JSON) |
#[derive(Debug, Clone, Default)]
pub struct CheckpointValues {
    pub checkpoint_ts: PosField<u64>,
    pub kind: PosField<String>,
    pub file_path: PosField<String>,
    pub lines_added: PosField<u32>,
    pub lines_deleted: PosField<u32>,
    pub lines_added_sloc: PosField<u32>,
    pub lines_deleted_sloc: PosField<u32>,
    pub external_tool_use_id: PosField<String>,
    pub edit_kind: PosField<String>,
    pub checkpoint_type: PosField<String>,
    pub attribution_recovery_metadata: PosField<String>,
}

impl CheckpointValues {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn checkpoint_ts(mut self, value: u64) -> Self {
        self.checkpoint_ts = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn checkpoint_ts_null(mut self) -> Self {
        self.checkpoint_ts = Some(None);
        self
    }

    pub fn kind(mut self, value: impl Into<String>) -> Self {
        self.kind = Some(Some(value.into()));
        self
    }

    #[allow(dead_code)]
    pub fn kind_null(mut self) -> Self {
        self.kind = Some(None);
        self
    }

    pub fn file_path(mut self, value: impl Into<String>) -> Self {
        self.file_path = Some(Some(value.into()));
        self
    }

    #[allow(dead_code)]
    pub fn file_path_null(mut self) -> Self {
        self.file_path = Some(None);
        self
    }

    pub fn lines_added(mut self, value: u32) -> Self {
        self.lines_added = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn lines_added_null(mut self) -> Self {
        self.lines_added = Some(None);
        self
    }

    pub fn lines_deleted(mut self, value: u32) -> Self {
        self.lines_deleted = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn lines_deleted_null(mut self) -> Self {
        self.lines_deleted = Some(None);
        self
    }

    pub fn lines_added_sloc(mut self, value: u32) -> Self {
        self.lines_added_sloc = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn lines_added_sloc_null(mut self) -> Self {
        self.lines_added_sloc = Some(None);
        self
    }

    pub fn lines_deleted_sloc(mut self, value: u32) -> Self {
        self.lines_deleted_sloc = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn lines_deleted_sloc_null(mut self) -> Self {
        self.lines_deleted_sloc = Some(None);
        self
    }

    pub fn external_tool_use_id(mut self, value: impl Into<String>) -> Self {
        self.external_tool_use_id = Some(Some(value.into()));
        self
    }

    #[allow(dead_code)]
    pub fn external_tool_use_id_null(mut self) -> Self {
        self.external_tool_use_id = Some(None);
        self
    }

    pub fn edit_kind(mut self, value: impl Into<String>) -> Self {
        self.edit_kind = Some(Some(value.into()));
        self
    }

    #[allow(dead_code)]
    pub fn edit_kind_null(mut self) -> Self {
        self.edit_kind = Some(None);
        self
    }

    pub fn checkpoint_type(mut self, value: impl Into<String>) -> Self {
        self.checkpoint_type = Some(Some(value.into()));
        self
    }

    #[allow(dead_code)]
    pub fn checkpoint_type_null(mut self) -> Self {
        self.checkpoint_type = Some(None);
        self
    }

    pub fn attribution_recovery_metadata(mut self, value: impl Into<String>) -> Self {
        self.attribution_recovery_metadata = Some(Some(value.into()));
        self
    }

    #[allow(dead_code)]
    pub fn attribution_recovery_metadata_null(mut self) -> Self {
        self.attribution_recovery_metadata = Some(None);
        self
    }
}

impl PosEncoded for CheckpointValues {
    fn to_sparse(&self) -> SparseArray {
        let mut map = SparseArray::new();

        sparse_set(
            &mut map,
            checkpoint_pos::CHECKPOINT_TS,
            u64_to_json(&self.checkpoint_ts),
        );
        sparse_set(&mut map, checkpoint_pos::KIND, string_to_json(&self.kind));
        sparse_set(
            &mut map,
            checkpoint_pos::FILE_PATH,
            string_to_json(&self.file_path),
        );
        sparse_set(
            &mut map,
            checkpoint_pos::LINES_ADDED,
            u32_to_json(&self.lines_added),
        );
        sparse_set(
            &mut map,
            checkpoint_pos::LINES_DELETED,
            u32_to_json(&self.lines_deleted),
        );
        sparse_set(
            &mut map,
            checkpoint_pos::LINES_ADDED_SLOC,
            u32_to_json(&self.lines_added_sloc),
        );
        sparse_set(
            &mut map,
            checkpoint_pos::LINES_DELETED_SLOC,
            u32_to_json(&self.lines_deleted_sloc),
        );
        sparse_set(
            &mut map,
            checkpoint_pos::TOOL_USE_ID,
            string_to_json(&self.external_tool_use_id),
        );
        sparse_set(
            &mut map,
            checkpoint_pos::EDIT_KIND,
            string_to_json(&self.edit_kind),
        );
        sparse_set(
            &mut map,
            checkpoint_pos::CHECKPOINT_TYPE,
            string_to_json(&self.checkpoint_type),
        );
        sparse_set(
            &mut map,
            checkpoint_pos::ATTRIBUTION_RECOVERY_METADATA,
            string_to_json(&self.attribution_recovery_metadata),
        );

        map
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        Self {
            checkpoint_ts: sparse_get_u64(arr, checkpoint_pos::CHECKPOINT_TS),
            kind: sparse_get_string(arr, checkpoint_pos::KIND),
            file_path: sparse_get_string(arr, checkpoint_pos::FILE_PATH),
            lines_added: sparse_get_u32(arr, checkpoint_pos::LINES_ADDED),
            lines_deleted: sparse_get_u32(arr, checkpoint_pos::LINES_DELETED),
            lines_added_sloc: sparse_get_u32(arr, checkpoint_pos::LINES_ADDED_SLOC),
            lines_deleted_sloc: sparse_get_u32(arr, checkpoint_pos::LINES_DELETED_SLOC),
            external_tool_use_id: sparse_get_string(arr, checkpoint_pos::TOOL_USE_ID),
            edit_kind: sparse_get_string(arr, checkpoint_pos::EDIT_KIND),
            checkpoint_type: sparse_get_string(arr, checkpoint_pos::CHECKPOINT_TYPE),
            attribution_recovery_metadata: sparse_get_string(
                arr,
                checkpoint_pos::ATTRIBUTION_RECOVERY_METADATA,
            ),
        }
    }
}

impl EventValues for CheckpointValues {
    fn event_id() -> MetricEventId {
        MetricEventId::Checkpoint
    }

    fn to_sparse(&self) -> SparseArray {
        PosEncoded::to_sparse(self)
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        PosEncoded::from_sparse(arr)
    }
}

/// Value positions for "daemon_ingest_anomaly" event.
pub mod daemon_ingest_anomaly_pos {
    pub const TRACE_PAYLOADS_DROPPED_QUEUE_FULL: usize = 0; // u64 - delta since last report
    pub const TRACE_CONNECTIONS_DROPPED: usize = 1; // u64 - delta since last report
    pub const TELEMETRY_METRIC_BATCHES_DROPPED: usize = 2; // u64 - delta since last report
    pub const CHECKPOINTS_DROPPED: usize = 3; // u64 - delta since last report
}

/// Values for Event ID 8: daemon_ingest_anomaly
///
/// Emitted by the daemon's socket-health loop whenever trace payloads,
/// trace connections, or telemetry metric batches were dropped since the
/// previous report. Attribution loss must be loud, never silent.
///
/// **Fields:**
/// | Position | Name | Type |
/// |----------|------|------|
/// | 0 | trace_payloads_dropped_queue_full | u64 |
/// | 1 | trace_connections_dropped | u64 |
/// | 2 | telemetry_metric_batches_dropped | u64 |
/// | 3 | checkpoints_dropped | u64 |
#[derive(Debug, Clone, Default)]
pub struct DaemonIngestAnomalyValues {
    pub trace_payloads_dropped_queue_full: PosField<u64>,
    pub trace_connections_dropped: PosField<u64>,
    pub telemetry_metric_batches_dropped: PosField<u64>,
    pub checkpoints_dropped: PosField<u64>,
}

impl DaemonIngestAnomalyValues {
    pub fn new(
        trace_payloads_dropped_queue_full: u64,
        trace_connections_dropped: u64,
        telemetry_metric_batches_dropped: u64,
        checkpoints_dropped: u64,
    ) -> Self {
        Self {
            trace_payloads_dropped_queue_full: Some(Some(trace_payloads_dropped_queue_full)),
            trace_connections_dropped: Some(Some(trace_connections_dropped)),
            telemetry_metric_batches_dropped: Some(Some(telemetry_metric_batches_dropped)),
            checkpoints_dropped: Some(Some(checkpoints_dropped)),
        }
    }
}

impl PosEncoded for DaemonIngestAnomalyValues {
    fn to_sparse(&self) -> SparseArray {
        let mut map = SparseArray::new();

        sparse_set(
            &mut map,
            daemon_ingest_anomaly_pos::TRACE_PAYLOADS_DROPPED_QUEUE_FULL,
            u64_to_json(&self.trace_payloads_dropped_queue_full),
        );
        sparse_set(
            &mut map,
            daemon_ingest_anomaly_pos::TRACE_CONNECTIONS_DROPPED,
            u64_to_json(&self.trace_connections_dropped),
        );
        sparse_set(
            &mut map,
            daemon_ingest_anomaly_pos::TELEMETRY_METRIC_BATCHES_DROPPED,
            u64_to_json(&self.telemetry_metric_batches_dropped),
        );
        sparse_set(
            &mut map,
            daemon_ingest_anomaly_pos::CHECKPOINTS_DROPPED,
            u64_to_json(&self.checkpoints_dropped),
        );

        map
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        Self {
            trace_payloads_dropped_queue_full: sparse_get_u64(
                arr,
                daemon_ingest_anomaly_pos::TRACE_PAYLOADS_DROPPED_QUEUE_FULL,
            ),
            trace_connections_dropped: sparse_get_u64(
                arr,
                daemon_ingest_anomaly_pos::TRACE_CONNECTIONS_DROPPED,
            ),
            telemetry_metric_batches_dropped: sparse_get_u64(
                arr,
                daemon_ingest_anomaly_pos::TELEMETRY_METRIC_BATCHES_DROPPED,
            ),
            checkpoints_dropped: sparse_get_u64(
                arr,
                daemon_ingest_anomaly_pos::CHECKPOINTS_DROPPED,
            ),
        }
    }
}

impl EventValues for DaemonIngestAnomalyValues {
    fn event_id() -> MetricEventId {
        MetricEventId::DaemonIngestAnomaly
    }

    fn to_sparse(&self) -> SparseArray {
        PosEncoded::to_sparse(self)
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        PosEncoded::from_sparse(arr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn daemon_ingest_anomaly_values_round_trip() {
        use super::PosEncoded;

        let values = DaemonIngestAnomalyValues::new(3, 1, 7, 2);
        let sparse = PosEncoded::to_sparse(&values);
        let decoded = <DaemonIngestAnomalyValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(decoded.trace_payloads_dropped_queue_full, Some(Some(3)));
        assert_eq!(decoded.trace_connections_dropped, Some(Some(1)));
        assert_eq!(decoded.telemetry_metric_batches_dropped, Some(Some(7)));
        assert_eq!(decoded.checkpoints_dropped, Some(Some(2)));
        assert_eq!(DaemonIngestAnomalyValues::event_id() as u16, 8);
    }

    #[test]
    fn test_committed_values_builder() {
        let values = CommittedValues::new()
            .human_additions(50)
            .git_diff_deleted_lines(20)
            .git_diff_added_lines(150)
            .tool_model_pairs(vec!["all".to_string(), "claude-code:claude-3".to_string()])
            .ai_additions(vec![100, 70])
            .ai_accepted(vec![80, 55]);

        assert_eq!(values.human_additions, Some(Some(50)));
        assert_eq!(
            values.tool_model_pairs,
            Some(Some(vec![
                "all".to_string(),
                "claude-code:claude-3".to_string()
            ]))
        );
        assert_eq!(values.ai_additions, Some(Some(vec![100, 70])));
    }

    #[test]
    fn test_committed_values_to_sparse() {
        use super::PosEncoded;

        let values = CommittedValues::new()
            .human_additions(50)
            .git_diff_deleted_lines(20)
            .git_diff_added_lines(150)
            .tool_model_pairs(vec!["all".to_string(), "cursor:gpt-4".to_string()])
            .ai_additions(vec![100, 30]);

        let sparse = PosEncoded::to_sparse(&values);

        assert_eq!(sparse.get("0"), Some(&Value::Number(50.into())));
        assert_eq!(sparse.get("1"), Some(&Value::Number(20.into())));
        assert_eq!(sparse.get("2"), Some(&Value::Number(150.into())));
        assert_eq!(
            sparse.get("3"),
            Some(&Value::Array(vec![
                Value::String("all".to_string()),
                Value::String("cursor:gpt-4".to_string())
            ]))
        );
        assert_eq!(
            sparse.get("5"),
            Some(&Value::Array(vec![
                Value::Number(100.into()),
                Value::Number(30.into())
            ]))
        );
    }

    #[test]
    fn test_committed_values_with_commit_timestamps_and_patch_id() {
        use super::PosEncoded;

        let values = CommittedValues::new()
            .author_ts(1_704_067_200)
            .commit_ts(1_704_067_260)
            .patch_id("abc123");

        let sparse = PosEncoded::to_sparse(&values);

        assert_eq!(
            sparse.get("15"),
            Some(&Value::Number(1_704_067_200u64.into()))
        );
        assert_eq!(
            sparse.get("16"),
            Some(&Value::Number(1_704_067_260u64.into()))
        );
        assert_eq!(sparse.get("17"), Some(&Value::String("abc123".to_string())));
    }

    #[test]
    fn test_committed_values_from_sparse() {
        use super::PosEncoded;

        let mut sparse = SparseArray::new();
        sparse.insert("0".to_string(), Value::Number(75.into()));
        sparse.insert(
            "3".to_string(),
            Value::Array(vec![
                Value::String("all".to_string()),
                Value::String("copilot:gpt-4".to_string()),
            ]),
        );
        sparse.insert(
            "5".to_string(),
            Value::Array(vec![Value::Number(200.into()), Value::Number(100.into())]),
        );

        let values = <CommittedValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(values.human_additions, Some(Some(75)));
        assert_eq!(
            values.tool_model_pairs,
            Some(Some(vec!["all".to_string(), "copilot:gpt-4".to_string()]))
        );
        assert_eq!(values.ai_additions, Some(Some(vec![200, 100])));
        assert_eq!(values.git_diff_deleted_lines, None); // not set
    }

    #[test]
    fn test_committed_values_event_id() {
        assert_eq!(CommittedValues::event_id(), MetricEventId::Committed);
        assert_eq!(CommittedValues::event_id() as u16, 1);
    }

    #[test]
    fn test_rewrite_committed_values_event_id() {
        assert_eq!(
            RewriteCommittedValues::event_id(),
            MetricEventId::RewriteCommitted
        );
        assert_eq!(RewriteCommittedValues::event_id() as u16, 7);
    }

    #[test]
    fn test_rewrite_committed_values_sparse_roundtrip() {
        let original = RewriteCommittedValues::new()
            .human_additions(5)
            .git_diff_deleted_lines(2)
            .git_diff_added_lines(7)
            .tool_model_pairs(vec!["all".to_string(), "codex:gpt-5".to_string()])
            .ai_additions(vec![3, 3])
            .ai_accepted(vec![3, 3])
            .commit_subject("rebased commit")
            .commit_body_null()
            .authorship_note("note")
            .hunks("[]")
            .operation_kind("rebase")
            .original_commit_shas(vec!["old1".to_string()]);

        let sparse = PosEncoded::to_sparse(&original);

        assert!(!sparse.contains_key("10"));
        assert_eq!(sparse.get("15"), Some(&Value::String("rebase".to_string())));
        assert_eq!(
            sparse.get("16"),
            Some(&Value::Array(vec![Value::String("old1".to_string())]))
        );

        let restored = <RewriteCommittedValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(restored.human_additions, Some(Some(5)));
        assert_eq!(
            restored.tool_model_pairs,
            Some(Some(vec!["all".to_string(), "codex:gpt-5".to_string()]))
        );
        assert_eq!(restored.operation_kind, Some(Some("rebase".to_string())));
        assert_eq!(
            restored.original_commit_shas,
            Some(Some(vec!["old1".to_string()]))
        );
    }

    #[test]
    fn test_committed_values_null_fields() {
        let values = CommittedValues::new()
            .human_additions_null()
            .git_diff_deleted_lines_null()
            .tool_model_pairs_null();

        assert_eq!(values.human_additions, Some(None));
        assert_eq!(values.git_diff_deleted_lines, Some(None));
        assert_eq!(values.tool_model_pairs, Some(None));
    }

    #[test]
    fn test_committed_values_with_commit_info() {
        let values = CommittedValues::new()
            .human_additions(10)
            .first_checkpoint_ts(1704067200)
            .commit_subject("Initial commit")
            .commit_body("This is the commit body\n\nWith multiple lines");

        assert_eq!(values.first_checkpoint_ts, Some(Some(1704067200)));
        assert_eq!(
            values.commit_subject,
            Some(Some("Initial commit".to_string()))
        );
        assert_eq!(
            values.commit_body,
            Some(Some(
                "This is the commit body\n\nWith multiple lines".to_string()
            ))
        );
    }

    #[test]
    fn test_committed_values_roundtrip_with_new_fields() {
        use super::PosEncoded;

        let original = CommittedValues::new()
            .human_additions(25)
            .first_checkpoint_ts(1700000000)
            .commit_subject("Test commit")
            .commit_body_null()
            .author_ts(1700000100)
            .commit_ts(1700000200)
            .patch_id("stable-patch-id");

        let sparse = PosEncoded::to_sparse(&original);
        let restored = <CommittedValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(restored.human_additions, Some(Some(25)));
        assert_eq!(restored.first_checkpoint_ts, Some(Some(1700000000)));
        assert_eq!(
            restored.commit_subject,
            Some(Some("Test commit".to_string()))
        );
        assert_eq!(restored.commit_body, Some(None));
        assert_eq!(restored.author_ts, Some(Some(1700000100)));
        assert_eq!(restored.commit_ts, Some(Some(1700000200)));
        assert_eq!(restored.patch_id, Some(Some("stable-patch-id".to_string())));
    }

    #[test]
    fn test_committed_values_with_hunks() {
        let hunks_json = r#"[{"commit_sha":"abc123","content_hash":"def456","hunk_kind":"addition","start_line":1,"end_line":5,"file_path":"src/main.rs"}]"#;
        let values = CommittedValues::new().human_additions(10).hunks(hunks_json);

        assert_eq!(values.hunks, Some(Some(hunks_json.to_string())));
    }

    #[test]
    fn test_committed_values_hunks_null() {
        let values = CommittedValues::new().hunks_null();
        assert_eq!(values.hunks, Some(None));
    }

    #[test]
    fn test_committed_values_hunks_roundtrip() {
        use super::PosEncoded;

        let hunks_json = r#"[{"commit_sha":"abc","content_hash":"def","hunk_kind":"addition","start_line":1,"end_line":3,"file_path":"test.rs"}]"#;
        let original = CommittedValues::new().human_additions(5).hunks(hunks_json);

        let sparse = PosEncoded::to_sparse(&original);
        assert_eq!(
            sparse.get("14"),
            Some(&Value::String(hunks_json.to_string()))
        );

        let restored = <CommittedValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(restored.hunks, Some(Some(hunks_json.to_string())));
    }

    #[test]
    fn test_agent_usage_values() {
        let values = AgentUsageValues::new();
        assert_eq!(AgentUsageValues::event_id(), MetricEventId::AgentUsage);
        assert_eq!(AgentUsageValues::event_id() as u16, 2);

        // Should produce empty sparse array
        let sparse = PosEncoded::to_sparse(&values);
        assert!(sparse.is_empty());
    }

    #[test]
    fn test_agent_usage_values_roundtrip() {
        use super::PosEncoded;

        let original = AgentUsageValues::new();
        let sparse = PosEncoded::to_sparse(&original);
        let restored = <AgentUsageValues as PosEncoded>::from_sparse(&sparse);

        // Both should be empty
        assert!(PosEncoded::to_sparse(&restored).is_empty());
    }

    #[test]
    fn test_install_hooks_values_builder() {
        let values = InstallHooksValues::new()
            .tool_id("cursor".to_string())
            .status("installed".to_string())
            .message("Successfully installed".to_string());

        assert_eq!(values.tool_id, Some(Some("cursor".to_string())));
        assert_eq!(values.status, Some(Some("installed".to_string())));
        assert_eq!(
            values.message,
            Some(Some("Successfully installed".to_string()))
        );
    }

    #[test]
    fn test_install_hooks_values_with_null_message() {
        let values = InstallHooksValues::new()
            .tool_id("vscode".to_string())
            .status("not_found".to_string())
            .message_null();

        assert_eq!(values.message, Some(None));
    }

    #[test]
    fn test_install_hooks_values_to_sparse() {
        use super::PosEncoded;

        let values = InstallHooksValues::new()
            .tool_id("copilot".to_string())
            .status("failed".to_string())
            .message("Error: permission denied".to_string());

        let sparse = PosEncoded::to_sparse(&values);

        assert_eq!(sparse.get("0"), Some(&Value::String("copilot".to_string())));
        assert_eq!(sparse.get("1"), Some(&Value::String("failed".to_string())));
        assert_eq!(
            sparse.get("2"),
            Some(&Value::String("Error: permission denied".to_string()))
        );
    }

    #[test]
    fn test_install_hooks_values_from_sparse() {
        use super::PosEncoded;

        let mut sparse = SparseArray::new();
        sparse.insert("0".to_string(), Value::String("windsurf".to_string()));
        sparse.insert(
            "1".to_string(),
            Value::String("already_installed".to_string()),
        );
        sparse.insert("2".to_string(), Value::Null);

        let values = <InstallHooksValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(values.tool_id, Some(Some("windsurf".to_string())));
        assert_eq!(values.status, Some(Some("already_installed".to_string())));
        assert_eq!(values.message, Some(None));
    }

    #[test]
    fn test_install_hooks_event_id() {
        assert_eq!(InstallHooksValues::event_id(), MetricEventId::InstallHooks);
        assert_eq!(InstallHooksValues::event_id() as u16, 3);
    }

    #[test]
    fn test_checkpoint_values_builder() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1704067200)
            .kind("ai_agent")
            .file_path("src/main.rs")
            .lines_added(50)
            .lines_deleted(10)
            .lines_added_sloc(45)
            .lines_deleted_sloc(8);

        assert_eq!(values.checkpoint_ts, Some(Some(1704067200)));
        assert_eq!(values.kind, Some(Some("ai_agent".to_string())));
        assert_eq!(values.file_path, Some(Some("src/main.rs".to_string())));
        assert_eq!(values.lines_added, Some(Some(50)));
        assert_eq!(values.lines_deleted, Some(Some(10)));
        assert_eq!(values.lines_added_sloc, Some(Some(45)));
        assert_eq!(values.lines_deleted_sloc, Some(Some(8)));
    }

    #[test]
    fn test_checkpoint_values_with_nulls() {
        let values = CheckpointValues::new()
            .checkpoint_ts_null()
            .kind_null()
            .file_path_null()
            .lines_added_null();

        assert_eq!(values.checkpoint_ts, Some(None));
        assert_eq!(values.kind, Some(None));
        assert_eq!(values.file_path, Some(None));
        assert_eq!(values.lines_added, Some(None));
    }

    #[test]
    fn test_checkpoint_values_to_sparse() {
        use super::PosEncoded;

        let values = CheckpointValues::new()
            .checkpoint_ts(1700000000)
            .kind("human")
            .file_path("tests/test.rs")
            .lines_added(100)
            .lines_deleted(20);

        let sparse = PosEncoded::to_sparse(&values);

        assert_eq!(sparse.get("0"), Some(&Value::Number(1700000000.into())));
        assert_eq!(sparse.get("1"), Some(&Value::String("human".to_string())));
        assert_eq!(
            sparse.get("2"),
            Some(&Value::String("tests/test.rs".to_string()))
        );
        assert_eq!(sparse.get("3"), Some(&Value::Number(100.into())));
        assert_eq!(sparse.get("4"), Some(&Value::Number(20.into())));
    }

    #[test]
    fn test_checkpoint_values_from_sparse() {
        use super::PosEncoded;

        let mut sparse = SparseArray::new();
        sparse.insert("0".to_string(), Value::Number(1704067200.into()));
        sparse.insert("1".to_string(), Value::String("ai_tab".to_string()));
        sparse.insert("2".to_string(), Value::String("lib.rs".to_string()));
        sparse.insert("3".to_string(), Value::Number(75.into()));
        sparse.insert("4".to_string(), Value::Number(15.into()));
        sparse.insert("5".to_string(), Value::Number(70.into()));
        sparse.insert("6".to_string(), Value::Number(12.into()));

        let values = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(values.checkpoint_ts, Some(Some(1704067200)));
        assert_eq!(values.kind, Some(Some("ai_tab".to_string())));
        assert_eq!(values.file_path, Some(Some("lib.rs".to_string())));
        assert_eq!(values.lines_added, Some(Some(75)));
        assert_eq!(values.lines_deleted, Some(Some(15)));
        assert_eq!(values.lines_added_sloc, Some(Some(70)));
        assert_eq!(values.lines_deleted_sloc, Some(Some(12)));
    }

    #[test]
    fn test_checkpoint_event_id() {
        assert_eq!(CheckpointValues::event_id(), MetricEventId::Checkpoint);
        assert_eq!(CheckpointValues::event_id() as u16, 4);
    }

    #[test]
    fn test_committed_values_with_all_arrays() {
        let values = CommittedValues::new()
            .tool_model_pairs(vec!["all".to_string(), "cursor:gpt-4".to_string()])
            .ai_additions(vec![100, 50])
            .ai_accepted(vec![80, 40]);

        assert_eq!(
            values.tool_model_pairs,
            Some(Some(vec!["all".to_string(), "cursor:gpt-4".to_string()]))
        );
        assert_eq!(values.ai_additions, Some(Some(vec![100, 50])));
        assert_eq!(values.ai_accepted, Some(Some(vec![80, 40])));
    }

    #[test]
    fn test_committed_values_array_nulls() {
        let values = CommittedValues::new().ai_accepted_null();

        assert_eq!(values.ai_accepted, Some(None));
    }

    #[test]
    fn test_checkpoint_values_with_external_tool_use_id() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1704067200)
            .kind("ai_agent")
            .file_path("src/main.rs")
            .lines_added(50)
            .external_tool_use_id("tool-use-123");

        assert_eq!(
            values.external_tool_use_id,
            Some(Some("tool-use-123".to_string()))
        );
    }

    #[test]
    fn test_checkpoint_values_external_tool_use_id_null() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1704067200)
            .kind("human")
            .external_tool_use_id_null();

        assert_eq!(values.external_tool_use_id, Some(None));
    }

    #[test]
    fn test_checkpoint_values_to_sparse_with_external_tool_use_id() {
        use super::PosEncoded;

        let values = CheckpointValues::new()
            .checkpoint_ts(1700000000)
            .kind("ai_agent")
            .file_path("tests/test.rs")
            .lines_added(100)
            .external_tool_use_id("tool-xyz");

        let sparse = PosEncoded::to_sparse(&values);

        assert_eq!(sparse.get("0"), Some(&Value::Number(1700000000.into())));
        assert_eq!(
            sparse.get("1"),
            Some(&Value::String("ai_agent".to_string()))
        );
        assert_eq!(
            sparse.get("2"),
            Some(&Value::String("tests/test.rs".to_string()))
        );
        assert_eq!(sparse.get("3"), Some(&Value::Number(100.into())));
        assert_eq!(
            sparse.get("7"),
            Some(&Value::String("tool-xyz".to_string()))
        );
    }

    #[test]
    fn test_checkpoint_values_from_sparse_with_external_tool_use_id() {
        use super::PosEncoded;

        let mut sparse = SparseArray::new();
        sparse.insert("0".to_string(), Value::Number(1704067200.into()));
        sparse.insert("1".to_string(), Value::String("ai_tab".to_string()));
        sparse.insert("2".to_string(), Value::String("lib.rs".to_string()));
        sparse.insert("3".to_string(), Value::Number(75.into()));
        sparse.insert("7".to_string(), Value::String("tool-abc".to_string()));

        let values = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(values.checkpoint_ts, Some(Some(1704067200)));
        assert_eq!(values.kind, Some(Some("ai_tab".to_string())));
        assert_eq!(values.file_path, Some(Some("lib.rs".to_string())));
        assert_eq!(values.lines_added, Some(Some(75)));
        assert_eq!(
            values.external_tool_use_id,
            Some(Some("tool-abc".to_string()))
        );
    }

    #[test]
    fn test_checkpoint_values_roundtrip_with_external_tool_use_id() {
        use super::PosEncoded;

        let original = CheckpointValues::new()
            .checkpoint_ts(1700000000)
            .kind("ai_agent")
            .file_path("src/lib.rs")
            .lines_added(50)
            .external_tool_use_id_null();

        let sparse = PosEncoded::to_sparse(&original);
        let restored = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(restored.checkpoint_ts, Some(Some(1700000000)));
        assert_eq!(restored.kind, Some(Some("ai_agent".to_string())));
        assert_eq!(restored.file_path, Some(Some("src/lib.rs".to_string())));
        assert_eq!(restored.lines_added, Some(Some(50)));
        assert_eq!(restored.external_tool_use_id, Some(None)); // explicitly null
    }

    #[test]
    fn test_checkpoint_values_external_tool_use_id_not_set() {
        use super::PosEncoded;

        let mut sparse = SparseArray::new();
        sparse.insert("0".to_string(), Value::Number(1700000000.into()));
        sparse.insert("1".to_string(), Value::String("human".to_string()));
        // external_tool_use_id not included

        let values = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(values.external_tool_use_id, None); // not set
    }

    #[test]
    fn test_checkpoint_values_with_edit_kind() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1704067200)
            .kind("ai_agent")
            .file_path("src/main.rs")
            .edit_kind("file_edit");

        assert_eq!(values.edit_kind, Some(Some("file_edit".to_string())));
    }

    #[test]
    fn test_checkpoint_values_edit_kind_null() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1704067200)
            .kind("ai_agent")
            .edit_kind_null();

        assert_eq!(values.edit_kind, Some(None));
    }

    #[test]
    fn test_checkpoint_values_with_recovery_metadata() {
        use super::PosEncoded;

        let values = CheckpointValues::new()
            .checkpoint_type("recovered_bash")
            .attribution_recovery_metadata(r#"{"solver":"bash_mtime"}"#);

        let sparse = PosEncoded::to_sparse(&values);
        assert_eq!(
            sparse.get("9"),
            Some(&Value::String("recovered_bash".to_string()))
        );
        assert_eq!(
            sparse.get("10"),
            Some(&Value::String(r#"{"solver":"bash_mtime"}"#.to_string()))
        );

        let restored = <CheckpointValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(
            restored.checkpoint_type,
            Some(Some("recovered_bash".to_string()))
        );
        assert_eq!(
            restored.attribution_recovery_metadata,
            Some(Some(r#"{"solver":"bash_mtime"}"#.to_string()))
        );
    }

    #[test]
    fn test_checkpoint_values_to_sparse_with_edit_kind() {
        use super::PosEncoded;

        let values = CheckpointValues::new()
            .checkpoint_ts(1700000000)
            .kind("ai_agent")
            .file_path("tests/test.rs")
            .edit_kind("bash");

        let sparse = PosEncoded::to_sparse(&values);

        assert_eq!(sparse.get("0"), Some(&Value::Number(1700000000.into())));
        assert_eq!(
            sparse.get("1"),
            Some(&Value::String("ai_agent".to_string()))
        );
        assert_eq!(sparse.get("8"), Some(&Value::String("bash".to_string())));
    }

    #[test]
    fn test_checkpoint_values_from_sparse_with_edit_kind() {
        use super::PosEncoded;

        let mut sparse = SparseArray::new();
        sparse.insert("0".to_string(), Value::Number(1704067200.into()));
        sparse.insert("1".to_string(), Value::String("ai_agent".to_string()));
        sparse.insert("2".to_string(), Value::String("lib.rs".to_string()));
        sparse.insert("8".to_string(), Value::String("file_edit".to_string()));

        let values = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(values.checkpoint_ts, Some(Some(1704067200)));
        assert_eq!(values.kind, Some(Some("ai_agent".to_string())));
        assert_eq!(values.edit_kind, Some(Some("file_edit".to_string())));
    }

    #[test]
    fn test_checkpoint_values_roundtrip_with_edit_kind() {
        use super::PosEncoded;

        let original = CheckpointValues::new()
            .checkpoint_ts(1700000000)
            .kind("ai_agent")
            .file_path("src/lib.rs")
            .lines_added(50)
            .edit_kind("bash");

        let sparse = PosEncoded::to_sparse(&original);
        let restored = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(restored.checkpoint_ts, Some(Some(1700000000)));
        assert_eq!(restored.kind, Some(Some("ai_agent".to_string())));
        assert_eq!(restored.file_path, Some(Some("src/lib.rs".to_string())));
        assert_eq!(restored.lines_added, Some(Some(50)));
        assert_eq!(restored.edit_kind, Some(Some("bash".to_string())));
    }

    #[test]
    fn test_checkpoint_values_edit_kind_not_set() {
        use super::PosEncoded;

        let mut sparse = SparseArray::new();
        sparse.insert("0".to_string(), Value::Number(1700000000.into()));
        sparse.insert("1".to_string(), Value::String("human".to_string()));

        let values = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(values.edit_kind, None);
    }
}

/// Value positions for "session_event" event.
pub mod session_event_pos {
    pub const RAW_JSON: usize = 0;
    pub const EXTERNAL_EVENT_ID: usize = 1;
    pub const EXTERNAL_PARENT_EVENT_ID: usize = 2;
    pub const EXTERNAL_TOOL_USE_ID: usize = 3;
}

/// Values for Event ID 5: session_event
///
/// Each event is the raw JSON from the agent's transcript file, stored at position 0.
/// Uses EventAttributes for session_id, trace_id, tool metadata.
#[derive(Debug, Clone, Default)]
pub struct SessionEventValues {
    pub raw_json: serde_json::Value,
    pub external_event_id: Option<String>,
    pub external_parent_event_id: Option<String>,
    pub external_tool_use_id: Option<String>,
}

impl SessionEventValues {
    pub fn new(raw_json: serde_json::Value) -> Self {
        Self {
            raw_json,
            external_event_id: None,
            external_parent_event_id: None,
            external_tool_use_id: None,
        }
    }

    pub fn with_ids(
        raw_json: serde_json::Value,
        external_event_id: Option<String>,
        external_parent_event_id: Option<String>,
        external_tool_use_id: Option<String>,
    ) -> Self {
        Self {
            raw_json,
            external_event_id,
            external_parent_event_id,
            external_tool_use_id,
        }
    }
}

impl PosEncoded for SessionEventValues {
    fn to_sparse(&self) -> SparseArray {
        let mut map = SparseArray::new();
        map.insert(
            session_event_pos::RAW_JSON.to_string(),
            self.raw_json.clone(),
        );
        if let Some(ref id) = self.external_event_id {
            map.insert(
                session_event_pos::EXTERNAL_EVENT_ID.to_string(),
                serde_json::Value::String(id.clone()),
            );
        }
        if let Some(ref id) = self.external_parent_event_id {
            map.insert(
                session_event_pos::EXTERNAL_PARENT_EVENT_ID.to_string(),
                serde_json::Value::String(id.clone()),
            );
        }
        if let Some(ref id) = self.external_tool_use_id {
            map.insert(
                session_event_pos::EXTERNAL_TOOL_USE_ID.to_string(),
                serde_json::Value::String(id.clone()),
            );
        }
        map
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        let raw_json = arr
            .get(&session_event_pos::RAW_JSON.to_string())
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let external_event_id = arr
            .get(&session_event_pos::EXTERNAL_EVENT_ID.to_string())
            .and_then(|v| v.as_str())
            .map(String::from);
        let external_parent_event_id = arr
            .get(&session_event_pos::EXTERNAL_PARENT_EVENT_ID.to_string())
            .and_then(|v| v.as_str())
            .map(String::from);
        let external_tool_use_id = arr
            .get(&session_event_pos::EXTERNAL_TOOL_USE_ID.to_string())
            .and_then(|v| v.as_str())
            .map(String::from);
        Self {
            raw_json,
            external_event_id,
            external_parent_event_id,
            external_tool_use_id,
        }
    }
}

impl EventValues for SessionEventValues {
    fn event_id() -> MetricEventId {
        MetricEventId::SessionEvent
    }

    fn to_sparse(&self) -> SparseArray {
        PosEncoded::to_sparse(self)
    }

    fn into_sparse(self) -> SparseArray {
        let mut map = SparseArray::new();
        map.insert(session_event_pos::RAW_JSON.to_string(), self.raw_json);
        if let Some(id) = self.external_event_id {
            map.insert(
                session_event_pos::EXTERNAL_EVENT_ID.to_string(),
                serde_json::Value::String(id),
            );
        }
        if let Some(id) = self.external_parent_event_id {
            map.insert(
                session_event_pos::EXTERNAL_PARENT_EVENT_ID.to_string(),
                serde_json::Value::String(id),
            );
        }
        if let Some(id) = self.external_tool_use_id {
            map.insert(
                session_event_pos::EXTERNAL_TOOL_USE_ID.to_string(),
                serde_json::Value::String(id),
            );
        }
        map
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        PosEncoded::from_sparse(arr)
    }
}

pub mod otel_trace_pos {
    pub const RAW_JSON: usize = 0;
    pub const EXTERNAL_EVENT_ID: usize = 1;
    pub const EXTERNAL_PARENT_EVENT_ID: usize = 2;
    pub const EXTERNAL_TOOL_USE_ID: usize = 3;
}

/// Values for Event ID 6: otel_trace
///
/// Each event is an OTEL span from a Copilot traces SQLite DB, stored as JSON at position 0.
/// Uses EventAttributes for session_id, trace_id, tool metadata.
#[derive(Debug, Clone, Default)]
pub struct OtelTraceValues {
    pub raw_json: serde_json::Value,
    pub external_event_id: Option<String>,
    pub external_parent_event_id: Option<String>,
    pub external_tool_use_id: Option<String>,
}

impl OtelTraceValues {
    pub fn new(raw_json: serde_json::Value) -> Self {
        Self {
            raw_json,
            external_event_id: None,
            external_parent_event_id: None,
            external_tool_use_id: None,
        }
    }

    pub fn with_ids(
        raw_json: serde_json::Value,
        external_event_id: Option<String>,
        external_parent_event_id: Option<String>,
        external_tool_use_id: Option<String>,
    ) -> Self {
        Self {
            raw_json,
            external_event_id,
            external_parent_event_id,
            external_tool_use_id,
        }
    }
}

impl PosEncoded for OtelTraceValues {
    fn to_sparse(&self) -> SparseArray {
        let mut map = SparseArray::new();
        map.insert(otel_trace_pos::RAW_JSON.to_string(), self.raw_json.clone());
        if let Some(ref id) = self.external_event_id {
            map.insert(
                otel_trace_pos::EXTERNAL_EVENT_ID.to_string(),
                serde_json::Value::String(id.clone()),
            );
        }
        if let Some(ref id) = self.external_parent_event_id {
            map.insert(
                otel_trace_pos::EXTERNAL_PARENT_EVENT_ID.to_string(),
                serde_json::Value::String(id.clone()),
            );
        }
        if let Some(ref id) = self.external_tool_use_id {
            map.insert(
                otel_trace_pos::EXTERNAL_TOOL_USE_ID.to_string(),
                serde_json::Value::String(id.clone()),
            );
        }
        map
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        let raw_json = arr
            .get(&otel_trace_pos::RAW_JSON.to_string())
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let external_event_id = arr
            .get(&otel_trace_pos::EXTERNAL_EVENT_ID.to_string())
            .and_then(|v| v.as_str())
            .map(String::from);
        let external_parent_event_id = arr
            .get(&otel_trace_pos::EXTERNAL_PARENT_EVENT_ID.to_string())
            .and_then(|v| v.as_str())
            .map(String::from);
        let external_tool_use_id = arr
            .get(&otel_trace_pos::EXTERNAL_TOOL_USE_ID.to_string())
            .and_then(|v| v.as_str())
            .map(String::from);
        Self {
            raw_json,
            external_event_id,
            external_parent_event_id,
            external_tool_use_id,
        }
    }
}

impl EventValues for OtelTraceValues {
    fn event_id() -> MetricEventId {
        MetricEventId::OtelTrace
    }

    fn to_sparse(&self) -> SparseArray {
        PosEncoded::to_sparse(self)
    }

    fn into_sparse(self) -> SparseArray {
        let mut map = SparseArray::new();
        map.insert(otel_trace_pos::RAW_JSON.to_string(), self.raw_json);
        if let Some(id) = self.external_event_id {
            map.insert(
                otel_trace_pos::EXTERNAL_EVENT_ID.to_string(),
                serde_json::Value::String(id),
            );
        }
        if let Some(id) = self.external_parent_event_id {
            map.insert(
                otel_trace_pos::EXTERNAL_PARENT_EVENT_ID.to_string(),
                serde_json::Value::String(id),
            );
        }
        if let Some(id) = self.external_tool_use_id {
            map.insert(
                otel_trace_pos::EXTERNAL_TOOL_USE_ID.to_string(),
                serde_json::Value::String(id),
            );
        }
        map
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        PosEncoded::from_sparse(arr)
    }
}

#[cfg(test)]
mod session_event_tests {
    use super::*;

    #[test]
    fn test_session_event_values_new() {
        let raw = serde_json::json!({"type": "user", "uuid": "abc"});
        let values = SessionEventValues::new(raw.clone());
        assert_eq!(values.raw_json, raw);
        assert_eq!(values.external_event_id, None);
        assert_eq!(values.external_parent_event_id, None);
        assert_eq!(values.external_tool_use_id, None);
    }

    #[test]
    fn test_session_event_values_with_ids() {
        let raw = serde_json::json!({"type": "assistant"});
        let values = SessionEventValues::with_ids(
            raw.clone(),
            Some("evt-123".to_string()),
            Some("parent-456".to_string()),
            Some("toolu_789".to_string()),
        );

        assert_eq!(values.raw_json, raw);
        assert_eq!(values.external_event_id, Some("evt-123".to_string()));
        assert_eq!(
            values.external_parent_event_id,
            Some("parent-456".to_string())
        );
        assert_eq!(values.external_tool_use_id, Some("toolu_789".to_string()));
    }

    #[test]
    fn test_session_event_values_sparse_roundtrip_with_ids() {
        let raw = serde_json::json!({"type": "assistant", "data": 42});
        let values = SessionEventValues::with_ids(
            raw.clone(),
            Some("event-id".to_string()),
            Some("parent-id".to_string()),
            Some("tool-use-id".to_string()),
        );

        let sparse = PosEncoded::to_sparse(&values);
        assert_eq!(sparse.get("0"), Some(&raw));
        assert_eq!(
            sparse.get("1"),
            Some(&serde_json::Value::String("event-id".to_string()))
        );
        assert_eq!(
            sparse.get("2"),
            Some(&serde_json::Value::String("parent-id".to_string()))
        );
        assert_eq!(
            sparse.get("3"),
            Some(&serde_json::Value::String("tool-use-id".to_string()))
        );

        let restored = <SessionEventValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(restored.raw_json, raw);
        assert_eq!(restored.external_event_id, Some("event-id".to_string()));
        assert_eq!(
            restored.external_parent_event_id,
            Some("parent-id".to_string())
        );
        assert_eq!(
            restored.external_tool_use_id,
            Some("tool-use-id".to_string())
        );
    }

    #[test]
    fn test_session_event_values_sparse_none_ids_omitted() {
        let raw = serde_json::json!({"type": "user"});
        let values = SessionEventValues::new(raw.clone());

        let sparse = PosEncoded::to_sparse(&values);
        assert_eq!(sparse.get("0"), Some(&raw));
        assert_eq!(sparse.get("1"), None);
        assert_eq!(sparse.get("2"), None);
        assert_eq!(sparse.get("3"), None);
    }

    #[test]
    fn test_session_event_values_into_sparse_with_ids() {
        let raw = serde_json::json!({"msg": "hello"});
        let values = SessionEventValues::with_ids(
            raw.clone(),
            Some("eid".to_string()),
            None,
            Some("tid".to_string()),
        );

        let sparse = EventValues::into_sparse(values);
        assert_eq!(sparse.get("0"), Some(&raw));
        assert_eq!(
            sparse.get("1"),
            Some(&serde_json::Value::String("eid".to_string()))
        );
        assert_eq!(sparse.get("2"), None);
        assert_eq!(
            sparse.get("3"),
            Some(&serde_json::Value::String("tid".to_string()))
        );
    }

    #[test]
    fn test_otel_trace_values_new() {
        let raw = serde_json::json!({"span": {"span_id": "abc", "trace_id": "t1"}});
        let values = OtelTraceValues::new(raw.clone());
        assert_eq!(values.raw_json, raw);
        assert_eq!(values.external_event_id, None);
        assert_eq!(values.external_parent_event_id, None);
        assert_eq!(values.external_tool_use_id, None);
    }

    #[test]
    fn test_otel_trace_values_with_ids() {
        let raw = serde_json::json!({"span": {"span_id": "s1", "trace_id": "t1"}});
        let values = OtelTraceValues::with_ids(
            raw.clone(),
            Some("span-123".to_string()),
            Some("parent-456".to_string()),
            Some("call-789".to_string()),
        );

        assert_eq!(values.raw_json, raw);
        assert_eq!(values.external_event_id, Some("span-123".to_string()));
        assert_eq!(
            values.external_parent_event_id,
            Some("parent-456".to_string())
        );
        assert_eq!(values.external_tool_use_id, Some("call-789".to_string()));
    }

    #[test]
    fn test_otel_trace_values_sparse_roundtrip_with_ids() {
        let raw = serde_json::json!({"span": {"span_id": "s1", "trace_id": "t1"}, "attributes": {"key": "val"}});
        let values = OtelTraceValues::with_ids(
            raw.clone(),
            Some("span-id".to_string()),
            Some("parent-id".to_string()),
            Some("tool-call-id".to_string()),
        );

        let sparse = PosEncoded::to_sparse(&values);
        assert_eq!(sparse.get("0"), Some(&raw));
        assert_eq!(
            sparse.get("1"),
            Some(&serde_json::Value::String("span-id".to_string()))
        );
        assert_eq!(
            sparse.get("2"),
            Some(&serde_json::Value::String("parent-id".to_string()))
        );
        assert_eq!(
            sparse.get("3"),
            Some(&serde_json::Value::String("tool-call-id".to_string()))
        );

        let restored = <OtelTraceValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(restored.raw_json, raw);
        assert_eq!(restored.external_event_id, Some("span-id".to_string()));
        assert_eq!(
            restored.external_parent_event_id,
            Some("parent-id".to_string())
        );
        assert_eq!(
            restored.external_tool_use_id,
            Some("tool-call-id".to_string())
        );
    }

    #[test]
    fn test_otel_trace_values_sparse_none_ids_omitted() {
        let raw = serde_json::json!({"span": {"span_id": "s1"}});
        let values = OtelTraceValues::new(raw.clone());

        let sparse = PosEncoded::to_sparse(&values);
        assert_eq!(sparse.get("0"), Some(&raw));
        assert_eq!(sparse.get("1"), None);
        assert_eq!(sparse.get("2"), None);
        assert_eq!(sparse.get("3"), None);
    }

    #[test]
    fn test_otel_trace_values_into_sparse_with_ids() {
        let raw = serde_json::json!({"data": "test"});
        let values = OtelTraceValues::with_ids(
            raw.clone(),
            Some("eid".to_string()),
            None,
            Some("tid".to_string()),
        );

        let sparse = EventValues::into_sparse(values);
        assert_eq!(sparse.get("0"), Some(&raw));
        assert_eq!(
            sparse.get("1"),
            Some(&serde_json::Value::String("eid".to_string()))
        );
        assert_eq!(sparse.get("2"), None);
        assert_eq!(
            sparse.get("3"),
            Some(&serde_json::Value::String("tid".to_string()))
        );
    }

    #[test]
    fn test_otel_trace_values_event_id() {
        assert_eq!(OtelTraceValues::event_id(), MetricEventId::OtelTrace);
        assert_eq!(OtelTraceValues::event_id() as u16, 6);
    }
}

/// Value positions for "token_usage" event.
pub mod token_usage_pos {
    pub const BUCKET_TS: usize = 0; // u64 - 5-minute UTC bucket start (unix seconds)
    pub const INPUT_TOKENS: usize = 1; // u64
    pub const OUTPUT_TOKENS: usize = 2; // u64
    pub const CACHE_READ_TOKENS: usize = 3; // u64
    pub const CACHE_WRITE_TOKENS: usize = 4; // u64
    pub const TOTAL_TOKENS: usize = 5; // u64
    pub const REASONING_OUTPUT_TOKENS: usize = 6; // u64 - subset of output, agents that report it
    pub const EST_COST_MICRO_USD: usize = 7; // u64 - estimated cost in 1e-6 USD
    pub const CREDITS: usize = 8; // f64 - reserved for credit-based agents
    pub const MESSAGE_COUNT: usize = 9; // u32 - deduplicated entries in the bucket
    pub const EMITTED_SEQ: usize = 10; // u64 - per-bucket emission revision
    pub const SPEED: usize = 11; // u32 - bucket key dimension: 0 standard, 1 fast
    pub const SPEED_INFERRED: usize = 12; // u32 0/1 - any entry's tier unrecorded (config/default)
    pub const CACHE_WRITE_1H_TOKENS: usize = 13; // u64 - 1h-TTL portion of cache_write_tokens
    pub const LONG_CONTEXT_INPUT_TOKENS: usize = 14; // u64 - portion billed at long-context rates
    pub const LONG_CONTEXT_OUTPUT_TOKENS: usize = 15; // u64
    pub const LONG_CONTEXT_CACHE_READ_TOKENS: usize = 16; // u64
    pub const LONG_CONTEXT_CACHE_WRITE_TOKENS: usize = 17; // u64
    pub const LONG_CONTEXT_CACHE_WRITE_1H_TOKENS: usize = 18; // u64
    pub const TRANSCRIPT_COST_MICRO_USD: usize = 19; // u64 - portion of cost from transcript costUSD
}

/// Values for Event ID 9: token_usage
///
/// One event per (session_id, model, speed, bucket_ts): the deduplicated
/// token usage and estimated cost of a 5-minute UTC bucket. The server
/// upserts on that key keeping the highest emitted_seq (a strictly
/// increasing per-bucket revision), so a bucket is re-emitted whenever its
/// aggregate changes (including corrections downward and drops to zero) and
/// same-second re-emissions cannot tie on the u32-second event_ts. Uses
/// EventAttributes for standard metadata (repo_url, tool, model, session
/// ids, etc.); the model attribute is the bucket's model, and the pricing
/// catalog id rides custom_attributes.
///
/// Positions 11-19 carry what a different pricing sheet needs to recompute
/// the cost: the speed dimension and inference flag, the 1h cache-write
/// split, the long-context token splits (tokens of requests that selected
/// their model's long-context tier — whole-request selection happens client
/// side; base-tier tokens are the totals minus these), and the
/// non-recomputable transcript-priced portion of the cost.
///
/// **Fields:**
/// | Position | Name | Type |
/// |----------|------|------|
/// | 0 | bucket_ts | u64 |
/// | 1 | input_tokens | u64 |
/// | 2 | output_tokens | u64 |
/// | 3 | cache_read_tokens | u64 |
/// | 4 | cache_write_tokens | u64 |
/// | 5 | total_tokens | u64 |
/// | 6 | reasoning_output_tokens | u64 (unset when the agent reports none) |
/// | 7 | est_cost_micro_usd | u64 |
/// | 8 | credits | f64 (reserved, unset) |
/// | 9 | message_count | u32 |
/// | 10 | emitted_seq | u64 |
/// | 11 | speed | u32 (0 standard, 1 fast; part of the bucket key) |
/// | 12 | speed_inferred | u32 (0/1) |
/// | 13 | cache_write_1h_tokens | u64 |
/// | 14 | long_context_input_tokens | u64 |
/// | 15 | long_context_output_tokens | u64 |
/// | 16 | long_context_cache_read_tokens | u64 |
/// | 17 | long_context_cache_write_tokens | u64 |
/// | 18 | long_context_cache_write_1h_tokens | u64 |
/// | 19 | transcript_cost_micro_usd | u64 |
#[derive(Debug, Clone, Default)]
pub struct TokenUsageValues {
    pub bucket_ts: PosField<u64>,
    pub input_tokens: PosField<u64>,
    pub output_tokens: PosField<u64>,
    pub cache_read_tokens: PosField<u64>,
    pub cache_write_tokens: PosField<u64>,
    pub total_tokens: PosField<u64>,
    pub reasoning_output_tokens: PosField<u64>,
    pub est_cost_micro_usd: PosField<u64>,
    pub credits: PosField<f64>,
    pub message_count: PosField<u32>,
    pub emitted_seq: PosField<u64>,
    pub speed: PosField<u32>,
    pub speed_inferred: PosField<u32>,
    pub cache_write_1h_tokens: PosField<u64>,
    pub long_context_input_tokens: PosField<u64>,
    pub long_context_output_tokens: PosField<u64>,
    pub long_context_cache_read_tokens: PosField<u64>,
    pub long_context_cache_write_tokens: PosField<u64>,
    pub long_context_cache_write_1h_tokens: PosField<u64>,
    pub transcript_cost_micro_usd: PosField<u64>,
}

impl TokenUsageValues {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bucket_ts(mut self, value: u64) -> Self {
        self.bucket_ts = Some(Some(value));
        self
    }

    pub fn input_tokens(mut self, value: u64) -> Self {
        self.input_tokens = Some(Some(value));
        self
    }

    pub fn output_tokens(mut self, value: u64) -> Self {
        self.output_tokens = Some(Some(value));
        self
    }

    pub fn cache_read_tokens(mut self, value: u64) -> Self {
        self.cache_read_tokens = Some(Some(value));
        self
    }

    pub fn cache_write_tokens(mut self, value: u64) -> Self {
        self.cache_write_tokens = Some(Some(value));
        self
    }

    pub fn total_tokens(mut self, value: u64) -> Self {
        self.total_tokens = Some(Some(value));
        self
    }

    /// Left unset (not zero) when the agent reports no reasoning tokens.
    pub fn reasoning_output_tokens_opt(mut self, value: Option<u64>) -> Self {
        if let Some(value) = value {
            self.reasoning_output_tokens = Some(Some(value));
        }
        self
    }

    pub fn est_cost_micro_usd(mut self, value: u64) -> Self {
        self.est_cost_micro_usd = Some(Some(value));
        self
    }

    #[allow(dead_code)]
    pub fn credits(mut self, value: f64) -> Self {
        self.credits = Some(Some(value));
        self
    }

    pub fn message_count(mut self, value: u32) -> Self {
        self.message_count = Some(Some(value));
        self
    }

    pub fn emitted_seq(mut self, value: u64) -> Self {
        self.emitted_seq = Some(Some(value));
        self
    }

    pub fn speed(mut self, value: u32) -> Self {
        self.speed = Some(Some(value));
        self
    }

    pub fn speed_inferred(mut self, value: bool) -> Self {
        self.speed_inferred = Some(Some(value as u32));
        self
    }

    pub fn cache_write_1h_tokens(mut self, value: u64) -> Self {
        self.cache_write_1h_tokens = Some(Some(value));
        self
    }

    pub fn long_context_input_tokens(mut self, value: u64) -> Self {
        self.long_context_input_tokens = Some(Some(value));
        self
    }

    pub fn long_context_output_tokens(mut self, value: u64) -> Self {
        self.long_context_output_tokens = Some(Some(value));
        self
    }

    pub fn long_context_cache_read_tokens(mut self, value: u64) -> Self {
        self.long_context_cache_read_tokens = Some(Some(value));
        self
    }

    pub fn long_context_cache_write_tokens(mut self, value: u64) -> Self {
        self.long_context_cache_write_tokens = Some(Some(value));
        self
    }

    pub fn long_context_cache_write_1h_tokens(mut self, value: u64) -> Self {
        self.long_context_cache_write_1h_tokens = Some(Some(value));
        self
    }

    pub fn transcript_cost_micro_usd(mut self, value: u64) -> Self {
        self.transcript_cost_micro_usd = Some(Some(value));
        self
    }
}

impl PosEncoded for TokenUsageValues {
    fn to_sparse(&self) -> SparseArray {
        let mut map = SparseArray::new();
        sparse_set(
            &mut map,
            token_usage_pos::BUCKET_TS,
            u64_to_json(&self.bucket_ts),
        );
        sparse_set(
            &mut map,
            token_usage_pos::INPUT_TOKENS,
            u64_to_json(&self.input_tokens),
        );
        sparse_set(
            &mut map,
            token_usage_pos::OUTPUT_TOKENS,
            u64_to_json(&self.output_tokens),
        );
        sparse_set(
            &mut map,
            token_usage_pos::CACHE_READ_TOKENS,
            u64_to_json(&self.cache_read_tokens),
        );
        sparse_set(
            &mut map,
            token_usage_pos::CACHE_WRITE_TOKENS,
            u64_to_json(&self.cache_write_tokens),
        );
        sparse_set(
            &mut map,
            token_usage_pos::TOTAL_TOKENS,
            u64_to_json(&self.total_tokens),
        );
        sparse_set(
            &mut map,
            token_usage_pos::REASONING_OUTPUT_TOKENS,
            u64_to_json(&self.reasoning_output_tokens),
        );
        sparse_set(
            &mut map,
            token_usage_pos::EST_COST_MICRO_USD,
            u64_to_json(&self.est_cost_micro_usd),
        );
        sparse_set(
            &mut map,
            token_usage_pos::CREDITS,
            f64_to_json(&self.credits),
        );
        sparse_set(
            &mut map,
            token_usage_pos::MESSAGE_COUNT,
            u32_to_json(&self.message_count),
        );
        sparse_set(
            &mut map,
            token_usage_pos::EMITTED_SEQ,
            u64_to_json(&self.emitted_seq),
        );
        sparse_set(&mut map, token_usage_pos::SPEED, u32_to_json(&self.speed));
        sparse_set(
            &mut map,
            token_usage_pos::SPEED_INFERRED,
            u32_to_json(&self.speed_inferred),
        );
        sparse_set(
            &mut map,
            token_usage_pos::CACHE_WRITE_1H_TOKENS,
            u64_to_json(&self.cache_write_1h_tokens),
        );
        sparse_set(
            &mut map,
            token_usage_pos::LONG_CONTEXT_INPUT_TOKENS,
            u64_to_json(&self.long_context_input_tokens),
        );
        sparse_set(
            &mut map,
            token_usage_pos::LONG_CONTEXT_OUTPUT_TOKENS,
            u64_to_json(&self.long_context_output_tokens),
        );
        sparse_set(
            &mut map,
            token_usage_pos::LONG_CONTEXT_CACHE_READ_TOKENS,
            u64_to_json(&self.long_context_cache_read_tokens),
        );
        sparse_set(
            &mut map,
            token_usage_pos::LONG_CONTEXT_CACHE_WRITE_TOKENS,
            u64_to_json(&self.long_context_cache_write_tokens),
        );
        sparse_set(
            &mut map,
            token_usage_pos::LONG_CONTEXT_CACHE_WRITE_1H_TOKENS,
            u64_to_json(&self.long_context_cache_write_1h_tokens),
        );
        sparse_set(
            &mut map,
            token_usage_pos::TRANSCRIPT_COST_MICRO_USD,
            u64_to_json(&self.transcript_cost_micro_usd),
        );
        map
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        Self {
            bucket_ts: sparse_get_u64(arr, token_usage_pos::BUCKET_TS),
            input_tokens: sparse_get_u64(arr, token_usage_pos::INPUT_TOKENS),
            output_tokens: sparse_get_u64(arr, token_usage_pos::OUTPUT_TOKENS),
            cache_read_tokens: sparse_get_u64(arr, token_usage_pos::CACHE_READ_TOKENS),
            cache_write_tokens: sparse_get_u64(arr, token_usage_pos::CACHE_WRITE_TOKENS),
            total_tokens: sparse_get_u64(arr, token_usage_pos::TOTAL_TOKENS),
            reasoning_output_tokens: sparse_get_u64(arr, token_usage_pos::REASONING_OUTPUT_TOKENS),
            est_cost_micro_usd: sparse_get_u64(arr, token_usage_pos::EST_COST_MICRO_USD),
            credits: sparse_get_f64(arr, token_usage_pos::CREDITS),
            message_count: sparse_get_u32(arr, token_usage_pos::MESSAGE_COUNT),
            emitted_seq: sparse_get_u64(arr, token_usage_pos::EMITTED_SEQ),
            speed: sparse_get_u32(arr, token_usage_pos::SPEED),
            speed_inferred: sparse_get_u32(arr, token_usage_pos::SPEED_INFERRED),
            cache_write_1h_tokens: sparse_get_u64(arr, token_usage_pos::CACHE_WRITE_1H_TOKENS),
            long_context_input_tokens: sparse_get_u64(
                arr,
                token_usage_pos::LONG_CONTEXT_INPUT_TOKENS,
            ),
            long_context_output_tokens: sparse_get_u64(
                arr,
                token_usage_pos::LONG_CONTEXT_OUTPUT_TOKENS,
            ),
            long_context_cache_read_tokens: sparse_get_u64(
                arr,
                token_usage_pos::LONG_CONTEXT_CACHE_READ_TOKENS,
            ),
            long_context_cache_write_tokens: sparse_get_u64(
                arr,
                token_usage_pos::LONG_CONTEXT_CACHE_WRITE_TOKENS,
            ),
            long_context_cache_write_1h_tokens: sparse_get_u64(
                arr,
                token_usage_pos::LONG_CONTEXT_CACHE_WRITE_1H_TOKENS,
            ),
            transcript_cost_micro_usd: sparse_get_u64(
                arr,
                token_usage_pos::TRANSCRIPT_COST_MICRO_USD,
            ),
        }
    }
}

impl EventValues for TokenUsageValues {
    fn event_id() -> MetricEventId {
        MetricEventId::TokenUsage
    }

    fn to_sparse(&self) -> SparseArray {
        PosEncoded::to_sparse(self)
    }

    fn from_sparse(arr: &SparseArray) -> Self {
        PosEncoded::from_sparse(arr)
    }
}

#[cfg(test)]
mod token_usage_tests {
    use super::*;

    #[test]
    fn test_token_usage_values_event_id() {
        assert_eq!(TokenUsageValues::event_id(), MetricEventId::TokenUsage);
        assert_eq!(TokenUsageValues::event_id() as u16, 9);
    }

    #[test]
    fn test_token_usage_values_sparse_roundtrip() {
        let values = TokenUsageValues::new()
            .bucket_ts(1_700_000_100)
            .input_tokens(100)
            .output_tokens(50)
            .cache_read_tokens(200)
            .cache_write_tokens(30)
            .total_tokens(380)
            .reasoning_output_tokens_opt(Some(12))
            .est_cost_micro_usd(4_567)
            .message_count(3)
            .emitted_seq(7);

        let sparse = PosEncoded::to_sparse(&values);
        let restored = <TokenUsageValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(restored.bucket_ts, Some(Some(1_700_000_100)));
        assert_eq!(restored.input_tokens, Some(Some(100)));
        assert_eq!(restored.output_tokens, Some(Some(50)));
        assert_eq!(restored.cache_read_tokens, Some(Some(200)));
        assert_eq!(restored.cache_write_tokens, Some(Some(30)));
        assert_eq!(restored.total_tokens, Some(Some(380)));
        assert_eq!(restored.reasoning_output_tokens, Some(Some(12)));
        assert_eq!(restored.est_cost_micro_usd, Some(Some(4_567)));
        assert_eq!(restored.credits, None);
        assert_eq!(restored.message_count, Some(Some(3)));
        assert_eq!(restored.emitted_seq, Some(Some(7)));
    }

    #[test]
    fn test_token_usage_wire_encoding_is_pinned() {
        // The sparse positions are the server's decoding contract: a silent
        // renumbering would corrupt field decoding for every uploaded event.
        let values = TokenUsageValues::new()
            .bucket_ts(1_767_225_600)
            .input_tokens(1)
            .output_tokens(2)
            .cache_read_tokens(3)
            .cache_write_tokens(4)
            .total_tokens(10)
            .reasoning_output_tokens_opt(Some(5))
            .est_cost_micro_usd(6)
            .credits(7.5)
            .message_count(8)
            .emitted_seq(9)
            .speed(1)
            .speed_inferred(true)
            .cache_write_1h_tokens(11)
            .long_context_input_tokens(12)
            .long_context_output_tokens(13)
            .long_context_cache_read_tokens(14)
            .long_context_cache_write_tokens(15)
            .long_context_cache_write_1h_tokens(16)
            .transcript_cost_micro_usd(17);
        let sparse = PosEncoded::to_sparse(&values);
        let ordered: std::collections::BTreeMap<usize, &serde_json::Value> = sparse
            .iter()
            .map(|(k, v)| (k.parse::<usize>().unwrap(), v))
            .collect();
        insta::assert_snapshot!(
            serde_json::to_string(&ordered).unwrap(),
            @r#"{"0":1767225600,"1":1,"2":2,"3":3,"4":4,"5":10,"6":5,"7":6,"8":7.5,"9":8,"10":9,"11":1,"12":1,"13":11,"14":12,"15":13,"16":14,"17":15,"18":16,"19":17}"#
        );
    }

    #[test]
    fn test_reasoning_tokens_omitted_when_absent() {
        let values = TokenUsageValues::new()
            .bucket_ts(300)
            .reasoning_output_tokens_opt(None);
        let sparse = PosEncoded::to_sparse(&values);
        assert!(!sparse.contains_key(&token_usage_pos::REASONING_OUTPUT_TOKENS.to_string()));
        assert!(!sparse.contains_key(&token_usage_pos::CREDITS.to_string()));
    }
}
