#[allow(unused_imports)]
use super::*;
use crate::error::GitAiError;
use crate::model::domain::RewriteEvent;
use crate::operations::daemon::cherry_pick_helpers::{
    cherry_pick_command_has_flag, cherry_pick_source_args_for_side_effect,
    resolve_cherry_pick_source_args_with_git_in_head_context,
    resolve_explicit_cherry_pick_sources_for_side_effect,
};
use crate::operations::daemon::revert_rebase_helpers::apply_cherry_pick_complete_rewrite;
use crate::operations::git::find_repository_in_path;
use crate::operations::git::oid::is_non_zero_oid;
use std::path::Path;

impl ActorDaemonCoordinator {
    /// Handle `CherryPickComplete` event: resolve source commits and apply the
    /// cherry-pick note rewrite.
    ///
    /// Returns `Err` when `original_head` is empty — this aborts the whole
    /// `maybe_apply_side_effects_for_applied_command` call via `?` at the call site,
    /// skipping remaining events and post-loop sections.
    pub(crate) fn handle_cherry_pick_complete(
        &self,
        cmd: &crate::model::domain::NormalizedCommand,
        worktree: &str,
        original_head: &str,
        new_head: &str,
        source_commits: &[String],
        new_commits: &[String],
    ) -> Result<(), GitAiError> {
        if new_head.is_empty() {
            return Ok(());
        }
        let wt = Path::new(worktree);
        let repo = find_repository_in_path(worktree)?;
        let mut sources = source_commits.to_vec();
        let is_skip = cherry_pick_command_has_flag(cmd, "--skip");
        let explicit_source_args = cherry_pick_source_args_for_side_effect(cmd);
        if !sources.is_empty() {
            self.clear_pending_cherry_pick_sources_for_worktree(wt)?;
        } else if !explicit_source_args.is_empty() {
            let head_context = (!original_head.is_empty()).then_some(original_head);
            sources = resolve_cherry_pick_source_args_with_git_in_head_context(
                &repo,
                &explicit_source_args,
                head_context,
            )?;
            self.clear_pending_cherry_pick_sources_for_worktree(wt)?;
        } else {
            sources = self.take_pending_cherry_pick_sources_for_worktree(wt)?;
            if is_skip && !sources.is_empty() {
                sources.remove(0);
            }
        }
        let destinations = if new_commits.is_empty() {
            vec![new_head.to_string()]
        } else {
            new_commits.to_vec()
        };
        if original_head != new_head {
            if original_head.is_empty() {
                return Err(GitAiError::Generic(format!(
                    "cherry-pick complete missing original HEAD sid={}",
                    cmd.root_sid
                )));
            }
            apply_cherry_pick_complete_rewrite(&repo, original_head, &sources, &destinations)?;
        }
        Ok(())
    }

    /// Handle `CherryPickNoCommit` event: resolve sources if absent and stash
    /// them as pending no-commit cherry-pick state keyed by head.
    pub(crate) fn handle_cherry_pick_no_commit(
        &self,
        cmd: &crate::model::domain::NormalizedCommand,
        worktree: &str,
        source_commits: &[String],
        head: &str,
    ) -> Result<(), GitAiError> {
        let mut sources = source_commits.to_vec();
        if sources.is_empty() {
            let repo = find_repository_in_path(worktree)?;
            sources = resolve_explicit_cherry_pick_sources_for_side_effect(&repo, cmd)?;
        }
        if !head.is_empty() && !sources.is_empty() {
            self.set_pending_cherry_pick_no_commit_for_worktree(
                Path::new(worktree),
                sources,
                head.to_string(),
            )?;
        }
        Ok(())
    }

    /// Handle `StashOperation` event: dispatch to the stash rewrite handlers.
    pub(crate) fn handle_stash_operation(
        &self,
        cmd: &crate::model::domain::NormalizedCommand,
        worktree: &str,
        kind: &crate::model::domain::StashOpKind,
        head: Option<&str>,
    ) -> Result<(), GitAiError> {
        let repo = find_repository_in_path(worktree)?;
        match kind {
            crate::model::domain::StashOpKind::Push
            | crate::model::domain::StashOpKind::Unknown => {
                let resolved_stash = cmd.stash_target_oid.as_deref().or_else(|| {
                    cmd.ref_changes
                        .iter()
                        .find(|rc| rc.reference == "refs/stash")
                        .map(|rc| rc.new.as_str())
                        .filter(|s| {
                            !s.is_empty() && *s != "0000000000000000000000000000000000000000"
                        })
                });
                if let Some(stash_sha) = resolved_stash {
                    let push_head =
                        stash_base_head(&repo, stash_sha).or_else(|| head.map(ToOwned::to_owned));
                    if let Some(head_sha) = push_head.as_deref() {
                        let pathspecs = Self::stash_pathspecs_from_command(cmd);
                        crate::operations::authorship::rewrite_stash::handle_stash_create(
                            &repo, stash_sha, head_sha, pathspecs,
                        )?;
                    }
                }
            }
            crate::model::domain::StashOpKind::Pop => {
                if let Some(stash_sha) = resolve_stash_sha(cmd) {
                    let base_head = stash_base_head(&repo, stash_sha);
                    let target_head = head.or(base_head.as_deref());
                    crate::operations::authorship::rewrite_stash::handle_stash_pop_or_apply_with_head(
                        &repo, stash_sha, true, target_head,
                    )?;
                }
            }
            crate::model::domain::StashOpKind::Apply
            | crate::model::domain::StashOpKind::Branch => {
                if let Some(stash_sha) = resolve_stash_sha(cmd) {
                    let effective_head =
                        if matches!(kind, crate::model::domain::StashOpKind::Branch) {
                            stash_base_head(&repo, stash_sha)
                        } else {
                            None
                        };
                    let base_head = stash_base_head(&repo, stash_sha);
                    let target_head = effective_head.as_deref().or(head).or(base_head.as_deref());
                    crate::operations::authorship::rewrite_stash::handle_stash_pop_or_apply_with_head(
                        &repo, stash_sha, false, target_head,
                    )?;
                }
            }
            crate::model::domain::StashOpKind::Drop => {
                if let Some(stash_sha) = resolve_stash_sha(cmd) {
                    crate::operations::authorship::rewrite_stash::handle_stash_drop(
                        &repo, stash_sha,
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Handle `Reset` event (guarded to non-trivial transitions by the caller).
    pub(crate) fn handle_reset(
        worktree: &str,
        kind: &crate::model::domain::ResetKind,
        old_head: &str,
        new_head: &str,
    ) -> Result<(), GitAiError> {
        let repo = find_repository_in_path(worktree)?;
        match kind {
            crate::model::domain::ResetKind::Hard => {
                repo.storage.delete_working_log_for_base_commit(old_head)?;
            }
            _ => {
                if is_ancestor_commit(&repo, new_head, old_head) {
                    crate::operations::authorship::rewrite_reset::reconstruct_working_log_after_backward_reset(
                        &repo, old_head, new_head,
                    )?;
                } else if is_ancestor_commit(&repo, old_head, new_head) {
                    repo.storage.rename_working_log(old_head, new_head)?;
                } else {
                    let outcome =
                        crate::operations::authorship::rewrite::handle_rewrite_event_with_metrics(
                            &repo,
                            RewriteEvent::NonFastForward {
                                old_tip: old_head.to_string(),
                                new_tip: new_head.to_string(),
                                onto: None,
                            },
                        )?;
                    crate::operations::daemon::rewrite_metrics::spawn_rewrite_commit_metrics(
                        &repo,
                        outcome.metric_commits,
                    );
                }
            }
        }
        Ok(())
    }

    /// For checkout/switch: record a recent-replay prerequisite and migrate the
    /// working log to the new HEAD.
    pub(crate) fn handle_checkout_switch(
        &self,
        family: Option<&str>,
        cmd: &crate::model::domain::NormalizedCommand,
    ) -> Result<(), GitAiError> {
        if let Some(prerequisite) = recent_checkout_switch_prerequisite_from_command(cmd) {
            let family = family.map(ToOwned::to_owned).or_else(|| {
                cmd.worktree.as_ref().and_then(|worktree| {
                    find_repository_in_path(&worktree.to_string_lossy())
                        .ok()
                        .map(|repo| family_key_for_repository(&repo))
                })
            });
            if let Some(family) = family {
                self.record_recent_replay_prerequisite(&family, prerequisite)?;
            }
        }
        apply_checkout_switch_working_log_side_effect(cmd)?;
        Ok(())
    }

    /// For pull events whose HEAD moved fast-forward, migrate the working log
    /// from old to new head.
    pub(crate) fn handle_pull_fast_forward_working_log(
        cmd: &crate::model::domain::NormalizedCommand,
    ) -> Result<(), GitAiError> {
        let Some(worktree) = cmd.worktree.as_ref() else {
            return Ok(());
        };
        let (old_head, new_head) = Self::resolve_heads_for_command(cmd);
        if old_head.is_empty() || new_head.is_empty() || old_head == new_head {
            return Ok(());
        }
        let repo = find_repository_in_path(&worktree.to_string_lossy())?;
        if repo_is_ancestor(&repo, &old_head, &new_head) {
            apply_pull_fast_forward_working_log_side_effect(
                &worktree.to_string_lossy(),
                &old_head,
                &new_head,
            )?;
        }
        Ok(())
    }

    /// For `update-ref`: per `RefUpdated` event on HEAD/refs/heads/*, migrate the
    /// working log fast-forward or run the NonFastForward note rewrite.
    pub(crate) fn handle_update_ref_migrations(
        cmd: &crate::model::domain::NormalizedCommand,
        events: &[crate::model::domain::SemanticEvent],
    ) -> Result<(), GitAiError> {
        let Some(worktree) = cmd.worktree.as_ref() else {
            return Ok(());
        };
        for event in events {
            let crate::model::domain::SemanticEvent::RefUpdated {
                reference,
                old,
                new,
            } = event
            else {
                continue;
            };
            if reference != "HEAD" && !reference.starts_with("refs/heads/")
                || !is_non_zero_oid(old)
                || !is_non_zero_oid(new)
                || old == new
            {
                continue;
            }
            let repo = find_repository_in_path(&worktree.to_string_lossy())?;
            if repo_is_ancestor(&repo, old, new) {
                let affects_checked_out_branch = reference == "HEAD"
                    || cmd.ref_changes.iter().any(|change| {
                        change.reference == "HEAD" && change.old == *old && change.new == *new
                    });
                if affects_checked_out_branch {
                    if repo.storage.has_working_log(old) {
                        let author = repo.effective_author_identity().formatted_or_unknown();
                        crate::operations::authorship::post_commit::post_commit_from_working_log(
                            &repo,
                            Some(old.to_string()),
                            new.to_string(),
                            author,
                            true,
                        )?;
                    }
                    repo.storage.rename_working_log(old, new)?;
                }
            } else {
                crate::operations::authorship::rewrite::handle_rewrite_event(
                    &repo,
                    RewriteEvent::NonFastForward {
                        old_tip: old.to_string(),
                        new_tip: new.to_string(),
                        onto: None,
                    },
                )?;
            }
        }
        Ok(())
    }

    /// Fire transcript-sweep triggers derived from events, skipping `PostPush`
    /// for dry-run pushes.
    pub(crate) fn trigger_transcript_sweeps_for_command(
        &self,
        cmd: &crate::model::domain::NormalizedCommand,
        events: &[crate::model::domain::SemanticEvent],
    ) {
        let parsed_invocation = parsed_invocation_for_normalized_command(cmd);
        for trigger in transcript_sweep_triggers_for_events(events) {
            if trigger == crate::operations::daemon::stream_worker::SweepTrigger::PostPush
                && crate::operations::git::cli_parser::is_dry_run(&parsed_invocation.command_args)
            {
                tracing::debug!("transcript sweep trigger skipped for dry-run push");
                continue;
            }
            self.trigger_transcript_sweep(trigger);
        }
    }
}
