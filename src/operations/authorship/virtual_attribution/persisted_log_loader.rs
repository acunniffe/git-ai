use super::types::VirtualAttributions;
use super::working_log_state::{WorkingLogContentSource, load_working_log_state};
use crate::error::GitAiError;
use crate::operations::git::repository::Repository;
use std::collections::HashMap;

impl VirtualAttributions {
    /// Create VirtualAttributions from only the persisted working-log state.
    ///
    /// Unlike `from_just_working_log`, this never reads the live worktree. It is intended for
    /// daemon-side async reconstruction where the command's final state has already been captured.
    pub fn from_persisted_working_log(
        repo: Repository,
        base_commit: String,
        human_author: Option<String>,
    ) -> Result<Self, GitAiError> {
        load_working_log_state(
            repo,
            base_commit,
            human_author,
            WorkingLogContentSource::Persisted,
        )
    }

    /// Build amend attributions from the original commit's blame data, persisted
    /// working-log checkpoints, and an explicit final-state snapshot.
    pub async fn from_working_log_for_commit_snapshot(
        repo: Repository,
        base_commit: String,
        pathspecs: &[String],
        human_author: Option<String>,
        blame_start_commit: Option<String>,
        final_state_snapshot: &HashMap<String, String>,
    ) -> Result<Self, GitAiError> {
        let blame_va = Self::new_for_base_commit(
            repo.clone(),
            base_commit.clone(),
            pathspecs,
            blame_start_commit,
        )
        .await?;

        let checkpoint_va =
            Self::from_persisted_working_log(repo.clone(), base_commit.clone(), human_author)?;

        // Save session prompt IDs before the merge consumes checkpoint_va.
        // Exclude INITIAL-only prompts from prior commits.
        let checkpoint_prompt_ids: std::collections::HashSet<String> = checkpoint_va
            .prompts
            .keys()
            .filter(|id| !checkpoint_va.initial_only_prompt_ids.contains(*id))
            .cloned()
            .collect();

        let final_state = final_state_snapshot.clone();
        let mut merged_va =
            crate::operations::authorship::virtual_attribution::merge_attributions_favoring_first(
                checkpoint_va,
                blame_va,
                final_state,
            )?;

        // Mark all non-session prompts (same logic as `from_working_log_for_commit`).
        merged_va.initial_only_prompt_ids = merged_va
            .prompts
            .keys()
            .filter(|id| !checkpoint_prompt_ids.contains(*id))
            .cloned()
            .collect();

        // Prune blame-history prompts whose lines were deleted.  Same logic as
        // `from_working_log_for_commit`.
        let referenced_in_merged: std::collections::HashSet<String> = merged_va
            .attributions
            .values()
            .flat_map(|(_, line_attrs)| line_attrs.iter())
            .map(|la| la.author_id.clone())
            .collect();
        merged_va.prompts.retain(|id, _| {
            checkpoint_prompt_ids.contains(id) || referenced_in_merged.contains(id)
        });
        merged_va
            .humans
            .retain(|id, _| referenced_in_merged.contains(id));
        let referenced_session_ids: std::collections::HashSet<String> = referenced_in_merged
            .iter()
            .filter(|id| id.starts_with("s_"))
            .map(|id| id.split("::").next().unwrap_or(id).to_string())
            .collect();
        merged_va
            .sessions
            .retain(|id, _| referenced_session_ids.contains(id));

        Ok(merged_va)
    }
}
