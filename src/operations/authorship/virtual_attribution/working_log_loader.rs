use super::types::VirtualAttributions;
use super::working_log_state::{WorkingLogContentSource, load_working_log_state};
use crate::error::GitAiError;
use crate::operations::git::repository::Repository;
use std::collections::HashMap;

impl VirtualAttributions {
    /// Create VirtualAttributions from just the working log (no blame)
    ///
    /// This is a fast path that skips the expensive blame operation.
    /// Use this when you only care about working log data and don't need historical blame.
    ///
    /// This function:
    /// 1. Loads INITIAL attributions (unstaged AI code from previous working state)
    /// 2. Applies working log checkpoints on top
    /// 3. Returns VirtualAttributions with just the working log data
    pub fn from_just_working_log(
        repo: Repository,
        base_commit: String,
        human_author: Option<String>,
    ) -> Result<Self, GitAiError> {
        load_working_log_state(
            repo,
            base_commit,
            human_author,
            WorkingLogContentSource::LiveWorktree,
        )
    }

    /// Create VirtualAttributions from working-log state using an exact captured snapshot
    /// instead of the live worktree.
    pub fn from_working_log_snapshot(
        repo: Repository,
        base_commit: String,
        human_author: Option<String>,
        final_state_snapshot: &HashMap<String, String>,
    ) -> Result<Self, GitAiError> {
        load_working_log_state(
            repo,
            base_commit,
            human_author,
            WorkingLogContentSource::CapturedSnapshot(final_state_snapshot),
        )
    }
}
