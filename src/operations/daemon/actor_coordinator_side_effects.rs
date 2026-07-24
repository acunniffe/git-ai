#[allow(unused_imports)]
use super::*;
use crate::error::GitAiError;
use crate::operations::daemon::cherry_pick_helpers::{
    cherry_pick_command_has_flag, cherry_pick_destination_commits, cherry_pick_original_head,
    cherry_pick_state_exists_for_worktree, resolve_explicit_cherry_pick_sources_for_side_effect,
};
use crate::operations::daemon::revert_rebase_helpers::strict_rebase_original_head_from_command;
use crate::operations::git::find_repository_in_path;
use crate::operations::git::oid::is_non_zero_oid;
use std::ops::ControlFlow;
#[cfg(feature = "test-support")]
use std::time::Duration;

/// Bundled rebase-mode flags threaded from `compute_rebase_mode` into
/// the CommitCreated arm and `apply_event_side_effects`.
pub(crate) struct RebaseMode {
    pub(crate) is_completing_rebase: bool,
    pub(crate) is_pull_rebase: bool,
}

/// Bundled pull-event presence flags computed once per command.
struct PullFlags {
    saw_pull_event: bool,
    pull_uses_rebase: bool,
}

impl ActorDaemonCoordinator {
    pub(crate) async fn maybe_apply_side_effects_for_applied_command(
        &self,
        family: Option<&str>,
        applied: &crate::model::domain::AppliedCommand,
        commit_file_timestamp_snapshots: &mut CommitFileTimestampSnapshotHandles,
    ) -> Result<(), GitAiError> {
        // Test-only: allow inducing a panic in the side-effect pipeline to verify
        // that the daemon's catch_unwind recovery keeps the process alive.
        // Uses a file-based flag so the test can remove the file between commands.
        #[cfg(feature = "test-support")]
        if let Ok(path) = std::env::var("GIT_AI_TEST_PANIC_IN_SIDE_EFFECT_FLAG")
            && std::path::Path::new(&path).exists()
        {
            panic!("test-induced panic in side-effect pipeline");
        }

        let cmd = &applied.command;
        let events = &applied.analysis.events;

        if let Some(error) = commit_enrichment_unrecoverable_error(cmd, events) {
            return Err(error);
        }

        let primary = cmd.primary_command.as_deref().unwrap_or("unknown");

        #[cfg(feature = "test-support")]
        if let Ok(spec) = std::env::var("GIT_AI_TEST_DELAY_SIDE_EFFECT_MS_FOR_COMMAND") {
            for entry in spec.split(',') {
                let Some((command, delay_ms)) = entry.split_once('=') else {
                    continue;
                };
                if command == primary
                    && let Ok(delay_ms) = delay_ms.parse::<u64>()
                    && delay_ms > 0
                {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    break;
                }
            }
        }

        log_write_op_completion(primary, cmd);

        let PullFlags {
            saw_pull_event,
            pull_uses_rebase,
        } = pull_event_flags(events);

        trace_side_effect_debug(cmd, applied.seq, events);

        let rebase_mode =
            self.compute_rebase_mode_and_detect_non_ff(cmd, events, pull_uses_rebase)?;

        if cmd.exit_code != 0 {
            match self.handle_failed_command(family, cmd, events, pull_uses_rebase)? {
                ControlFlow::Break(()) => return Ok(()),
                ControlFlow::Continue(()) => {}
            }
        }

        self.apply_event_side_effects(cmd, events, &rebase_mode, commit_file_timestamp_snapshots)
            .await?;

        if matches!(cmd.primary_command.as_deref(), Some("checkout" | "switch")) {
            self.handle_checkout_switch(family, cmd)?;
        }

        if saw_pull_event {
            Self::handle_pull_fast_forward_working_log(cmd)?;
        }

        if primary == "update-ref" {
            Self::handle_update_ref_migrations(cmd, events)?;
        }

        self.trigger_transcript_sweeps_for_command(cmd, events);

        Ok(())
    }

    /// Classify the command's rebase mode, clear pending rebase state on --abort,
    /// and invoke non-FF rewrite detection for eligible commands.
    fn compute_rebase_mode_and_detect_non_ff(
        &self,
        cmd: &crate::model::domain::NormalizedCommand,
        events: &[crate::model::domain::SemanticEvent],
        pull_uses_rebase: bool,
    ) -> Result<RebaseMode, GitAiError> {
        // Non-FF rewrite detection: fires for commands that rewrite history via ref moves.
        // Skip for: checkout/switch/branch (no rewriting), cherry-pick (handled separately),
        // and plain commit/amend (CommitCreated/CommitAmended events handle those).
        // Do NOT skip for rebase — the CommitCreated events during rebase are intermediate
        // replayed commits; note transfer happens via non-FF detection on the final ref move.
        // But DO skip for rebase --abort, which restores state instead of finishing a rewrite.
        let is_rebase = cmd.primary_command.as_deref() == Some("rebase");
        let is_rebase_abort = is_rebase && cmd.invoked_args.iter().any(|a| a == "--abort");
        let is_completing_rebase = is_rebase && !is_rebase_abort;
        let is_pull_rebase = pull_uses_rebase && cmd.primary_command.as_deref() == Some("pull");
        let skip_non_ff = if is_completing_rebase || is_pull_rebase {
            false
        } else if is_rebase_abort {
            if let Some(worktree) = cmd.worktree.as_ref() {
                self.clear_pending_rebase_original_head_for_worktree(worktree)?;
            }
            true
        } else {
            events.iter().any(|event| {
                matches!(
                    event,
                    crate::model::domain::SemanticEvent::CommitAmended { .. }
                        | crate::model::domain::SemanticEvent::CommitCreated { .. }
                        | crate::model::domain::SemanticEvent::CherryPickComplete { .. }
                        | crate::model::domain::SemanticEvent::Reset { .. }
                )
            }) || matches!(
                cmd.primary_command.as_deref(),
                Some("checkout" | "switch" | "branch" | "stash")
            )
        };
        if !skip_non_ff && cmd.exit_code == 0 {
            self.detect_and_handle_non_ff_rewrites(cmd)?;
        }
        Ok(RebaseMode {
            is_completing_rebase,
            is_pull_rebase,
        })
    }

    /// Handle a failed command (exit_code != 0): persist pending rebase/cherry-pick
    /// state and decide whether to early-return or fall through for conflict cases.
    ///
    /// Returns `ControlFlow::Break(())` to signal that the caller should return
    /// `Ok(())` (equivalent to the original `return Ok(())` at line 312).
    fn handle_failed_command(
        &self,
        family: Option<&str>,
        cmd: &crate::model::domain::NormalizedCommand,
        events: &[crate::model::domain::SemanticEvent],
        pull_uses_rebase: bool,
    ) -> Result<ControlFlow<()>, GitAiError> {
        self.persist_pending_rebase_state(family, cmd, pull_uses_rebase)?;
        self.persist_failed_cherry_pick_state(cmd)?;

        // Fix #957: `checkout/switch --merge` exits with code 1 when it produces
        // conflict markers but HEAD still moves to the target branch.  We must not
        // return early here — fall through so apply_checkout_switch_working_log_side_effect
        // and recent_checkout_switch_prerequisite_from_command can migrate the working log.
        let is_merge_checkout =
            matches!(cmd.primary_command.as_deref(), Some("checkout" | "switch")) && {
                let p = parsed_invocation_for_normalized_command(cmd);
                p.has_command_flag("--merge") || p.has_command_flag("-m")
            };
        // For stash pop/apply/branch with non-zero exit (typically conflict), don't
        // skip processing. The stash may have been partially applied and attribution
        // should still be restored. We cannot rely on `has_stash_conflict_for_repo`
        // because in daemon mode the conflict check runs lazily at sync time -- by
        // which point the user may already have resolved the conflict with `git add`.
        // Instead, always attempt restoration for stash restore operations; if the
        // stash was never applied the restore is a harmless no-op.
        let is_stash_restore = cmd.primary_command.as_deref() == Some("stash")
            && events.iter().any(|event| {
                matches!(
                    event,
                    crate::model::domain::SemanticEvent::StashOperation {
                        kind: crate::model::domain::StashOpKind::Pop
                            | crate::model::domain::StashOpKind::Apply
                            | crate::model::domain::StashOpKind::Branch,
                        ..
                    }
                )
            });
        let is_merge_squash = cmd.primary_command.as_deref() == Some("merge")
            && events.iter().any(|event| {
                matches!(
                    event,
                    crate::model::domain::SemanticEvent::MergeSquash { .. }
                )
            });
        if !is_merge_checkout && !is_stash_restore && !is_merge_squash {
            return Ok(ControlFlow::Break(()));
        }
        if is_stash_restore {
            tracing::debug!(
                sid = %cmd.root_sid,
                "stash restore with non-zero exit, continuing to restore attribution"
            );
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Persist pending rebase original-head for interrupted rebases.
    fn persist_pending_rebase_state(
        &self,
        family: Option<&str>,
        cmd: &crate::model::domain::NormalizedCommand,
        pull_uses_rebase: bool,
    ) -> Result<(), GitAiError> {
        let rebase_start = cmd
            .ref_changes
            .iter()
            .find(|change| {
                change.reference == "HEAD"
                    && is_non_zero_oid(&change.old)
                    && is_non_zero_oid(&change.new)
            })
            .map(|change| (change.old.clone(), change.new.clone()));
        let pull_has_rebase_start =
            cmd.primary_command.as_deref() == Some("pull") && rebase_start.is_some();
        let is_rebase_like = cmd.primary_command.as_deref() == Some("rebase")
            || (cmd.primary_command.as_deref() == Some("pull")
                && (pull_uses_rebase || pull_has_rebase_start));
        if !is_rebase_like {
            return Ok(());
        }
        let worktree = cmd.worktree.as_ref().ok_or_else(|| {
            GitAiError::Generic(format!(
                "rebase side-effect state requires worktree sid={}",
                cmd.root_sid
            ))
        })?;
        if cmd.invoked_args.iter().any(|arg| arg == "--abort") {
            self.clear_pending_rebase_original_head_for_worktree(worktree)?;
        } else if !rebase_is_control_mode(cmd) {
            let semantic_old_head = rebase_start
                .as_ref()
                .map(|(old, _)| old.as_str())
                .unwrap_or("");
            let pending_old_head = strict_rebase_original_head_from_command(cmd, semantic_old_head);
            if let Some(old_head) = pending_old_head {
                let rebase_onto = rebase_start.as_ref().map(|(_, new)| new.clone());
                if std::env::var("GIT_AI_DEBUG_DAEMON_TRACE")
                    .ok()
                    .as_deref()
                    .is_some_and(|v| v == "1")
                {
                    tracing::debug!(
                        ?family,
                        %old_head,
                        ?rebase_onto,
                        "pending rebase original head set"
                    );
                }
                self.set_pending_rebase_original_head_for_worktree(
                    worktree,
                    old_head,
                    rebase_onto,
                )?;
            }
        }
        Ok(())
    }

    /// Persist/advance pending cherry-pick sources for conflicted cherry-picks.
    fn persist_failed_cherry_pick_state(
        &self,
        cmd: &crate::model::domain::NormalizedCommand,
    ) -> Result<(), GitAiError> {
        if cmd.primary_command.as_deref() != Some("cherry-pick") {
            return Ok(());
        }
        let worktree = cmd.worktree.as_ref().ok_or_else(|| {
            GitAiError::Generic(format!(
                "cherry-pick side-effect state requires worktree sid={}",
                cmd.root_sid
            ))
        })?;
        if cmd.invoked_args.iter().any(|arg| arg == "--abort") {
            self.clear_pending_cherry_pick_sources_for_worktree(worktree)?;
            self.clear_pending_cherry_pick_no_commit_for_worktree(worktree)?;
            return Ok(());
        }
        let new_commits = cherry_pick_destination_commits(cmd);
        let is_continue = cherry_pick_command_has_flag(cmd, "--continue");
        let is_skip = cherry_pick_command_has_flag(cmd, "--skip");
        let mut source_oids = cmd.cherry_pick_source_oids.clone();
        let mut source_oids_from_daemon_pending = false;
        if source_oids.is_empty()
            && (!new_commits.is_empty() || cherry_pick_state_exists_for_worktree(worktree))
        {
            let repo = find_repository_in_path(&worktree.to_string_lossy())?;
            source_oids = resolve_explicit_cherry_pick_sources_for_side_effect(&repo, cmd)?;
        }
        if source_oids.is_empty() && (is_continue || is_skip) {
            source_oids = self.pending_cherry_pick_sources_for_worktree(worktree)?;
            source_oids_from_daemon_pending = !source_oids.is_empty();
        }
        let skipped_sources = usize::from(is_skip && source_oids_from_daemon_pending);
        let applied_source_oids = source_oids
            .iter()
            .skip(skipped_sources)
            .cloned()
            .collect::<Vec<_>>();
        if !new_commits.is_empty() && !applied_source_oids.is_empty() {
            let repo = find_repository_in_path(&worktree.to_string_lossy())?;
            let original_head = cherry_pick_original_head(cmd).ok_or_else(|| {
                GitAiError::Generic(format!(
                    "cherry-pick completed commits without original HEAD sid={}",
                    cmd.root_sid
                ))
            })?;
            crate::operations::daemon::revert_rebase_helpers::apply_cherry_pick_complete_rewrite(
                &repo,
                &original_head,
                &applied_source_oids,
                &new_commits,
            )?;
        }
        if !source_oids.is_empty() || is_continue || is_skip {
            let applied_sources = new_commits
                .len()
                .min(source_oids.len().saturating_sub(skipped_sources));
            let consumed_sources = skipped_sources + applied_sources;
            let remaining = source_oids
                .iter()
                .skip(consumed_sources.min(source_oids.len()))
                .cloned()
                .collect();
            self.set_pending_cherry_pick_sources_for_worktree(worktree, remaining)?;
        }
        Ok(())
    }

    /// Dispatch each semantic event to its handler, carrying `handled_revert_commits`
    /// across multiple `CommitCreated` events from a single `git revert A B`.
    async fn apply_event_side_effects(
        &self,
        cmd: &crate::model::domain::NormalizedCommand,
        events: &[crate::model::domain::SemanticEvent],
        rebase_mode: &RebaseMode,
        commit_file_timestamp_snapshots: &mut CommitFileTimestampSnapshotHandles,
    ) -> Result<(), GitAiError> {
        let Some(worktree) = cmd.worktree.as_ref() else {
            return Ok(());
        };
        let worktree = worktree.to_string_lossy().to_string();
        let mut handled_revert_commits = false;
        for event in events {
            match event {
                crate::model::domain::SemanticEvent::CloneCompleted { .. } => {
                    apply_clone_notes_sync_side_effect(&worktree)?;
                }
                crate::model::domain::SemanticEvent::PullCompleted { .. } => {
                    apply_pull_notes_sync_side_effect(
                        &worktree,
                        cmd.invoked_command.as_deref(),
                        &cmd.invoked_args,
                    )?;
                }
                crate::model::domain::SemanticEvent::PushCompleted { .. } => {
                    apply_push_side_effect(
                        &worktree,
                        cmd.invoked_command.as_deref(),
                        &cmd.invoked_args,
                    )?;
                }
                crate::model::domain::SemanticEvent::CherryPickComplete {
                    original_head,
                    new_head,
                    source_commits,
                    new_commits,
                } => {
                    self.handle_cherry_pick_complete(
                        cmd,
                        &worktree,
                        original_head,
                        new_head,
                        source_commits,
                        new_commits,
                    )?;
                }
                crate::model::domain::SemanticEvent::CherryPickNoCommit {
                    source_commits,
                    head,
                } => {
                    self.handle_cherry_pick_no_commit(cmd, &worktree, source_commits, head)?;
                }
                crate::model::domain::SemanticEvent::MergeSquash { source_head, onto } => {
                    self.set_pending_squash_merge_for_worktree(
                        worktree.as_ref(),
                        source_head.clone(),
                        onto.clone(),
                    )?;
                }
                crate::model::domain::SemanticEvent::StashOperation { kind, head } => {
                    self.handle_stash_operation(cmd, &worktree, kind, head.as_deref())?;
                }
                crate::model::domain::SemanticEvent::CommitCreated { base, new_head } => {
                    self.handle_commit_created(
                        cmd,
                        &worktree,
                        base.as_deref(),
                        new_head,
                        rebase_mode,
                        &mut handled_revert_commits,
                        commit_file_timestamp_snapshots,
                    )
                    .await?;
                }
                crate::model::domain::SemanticEvent::CommitAmended { old_head, new_head } => {
                    self.handle_commit_amended(
                        &worktree,
                        old_head,
                        new_head,
                        commit_file_timestamp_snapshots,
                    )
                    .await?;
                }
                crate::model::domain::SemanticEvent::Reset {
                    kind,
                    old_head,
                    new_head,
                } if !old_head.is_empty() && !new_head.is_empty() && old_head != new_head => {
                    Self::handle_reset(&worktree, kind, old_head, new_head)?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// `Some(error)` when a `commit` invocation exited 0 but ref-cursor
/// enrichment (`RefCursor::enrich_commit`, including its existing
/// cold-seed-clamp and ingress-hint mitigations) still found no HEAD
/// transition, and this isn't one of the known legitimate no-op shapes.
/// Reuses `commit_skip_reason`'s exact classification of "opaque outcome for
/// a commit" so the diagnostic-only skip reason recorded in the completion
/// log and this fail-loud gate can never drift apart.
///
/// It deliberately does not rescan the reflog: message/timestamp matching is
/// explicitly rejected by the ingestion spec as unsound. This only makes the
/// existing fail-closed outcome observable; it does not change enrichment.
///
/// Known legitimate exceptions (must NOT error):
/// - `--dry-run`: never touches the reflog, by design.
/// - non-zero exit: excluded by the explicit `exit_code == 0` check (a
///   rejected/empty/conflicted commit attempt routinely produces no HEAD
///   move and is not a bug).
///
/// Not handled here (documented limitations, not attempted): a repo with
/// `core.logAllRefUpdates=false` has no reflog for ref-cursor to read at all,
/// so every commit there would fail loud; and a `commit --amend` that
/// reproduces a byte-identical commit object (requires a fixed
/// `GIT_COMMITTER_DATE`) moves no ref and writes no reflog entry. Both are
/// pre-existing constraints of the reflog-cursor attribution model itself,
/// not introduced by this check.
pub(crate) fn commit_enrichment_unrecoverable_error(
    cmd: &crate::model::domain::NormalizedCommand,
    events: &[crate::model::domain::SemanticEvent],
) -> Option<GitAiError> {
    if cmd.exit_code != 0 {
        return None;
    }
    crate::operations::daemon::actor_coordinator_seq::commit_skip_reason(
        cmd.primary_command.as_deref(),
        events,
    )?;
    let command_args = crate::operations::daemon::analyzers::command_args(cmd);
    if crate::operations::git::cli_parser::is_dry_run(&command_args)
        || command_args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--short" | "--porcelain" | "--long"))
    {
        return None;
    }
    // Collection is opt-in per repository: a note was never going to be
    // written for a non-allowed repo, so a lost enrichment there is not an
    // attribution loss and must not raise error telemetry. This resolve+check
    // runs only on the already-rare failure path, never per healthy commit.
    if let Some(worktree) = cmd.worktree.as_deref()
        && let Ok(repo) =
            crate::operations::git::find_repository_in_path(&worktree.to_string_lossy())
        && !repo.is_collection_allowed(&crate::config::Config::fresh())
    {
        return None;
    }
    Some(GitAiError::Generic(format!(
        "commit sid={} exited 0 but ref-cursor enrichment found no HEAD transition; no AI \
         attribution note was written for this commit (see \
         docs/architecture/daemon-trace2-ingestion-spec.md)",
        cmd.root_sid
    )))
}

/// Emit a tracing telemetry record when a successful write-class git op completed.
fn log_write_op_completion(primary: &str, cmd: &crate::model::domain::NormalizedCommand) {
    let is_write_op = matches!(
        primary,
        "commit"
            | "rebase"
            | "merge"
            | "cherry-pick"
            | "am"
            | "stash"
            | "reset"
            | "push"
            | "update-ref"
    );
    if is_write_op && cmd.exit_code == 0 {
        let repo_path = cmd
            .worktree
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let post_head = cmd
            .ref_changes
            .iter()
            .rev()
            .find(|change| change.reference == "HEAD")
            .map(|change| change.new.clone())
            .unwrap_or_default();
        tracing::info!(
            op = primary,
            repo = %repo_path,
            new_head = %post_head,
            "git write op completed"
        );
    }
}

/// Compute `saw_pull_event` and `pull_uses_rebase` by scanning the event list.
fn pull_event_flags(events: &[crate::model::domain::SemanticEvent]) -> PullFlags {
    let saw_pull_event = events.iter().any(|event| {
        matches!(
            event,
            crate::model::domain::SemanticEvent::PullCompleted { .. }
        )
    });
    let pull_uses_rebase = events.iter().any(|event| {
        matches!(
            event,
            crate::model::domain::SemanticEvent::PullCompleted {
                strategy: crate::model::domain::PullStrategy::Rebase
                    | crate::model::domain::PullStrategy::RebaseMerges,
                ..
            }
        )
    });
    PullFlags {
        saw_pull_event,
        pull_uses_rebase,
    }
}

/// Emit a full side-effect debug trace when `GIT_AI_DEBUG_DAEMON_TRACE=1`.
fn trace_side_effect_debug(
    cmd: &crate::model::domain::NormalizedCommand,
    seq: u64,
    events: &[crate::model::domain::SemanticEvent],
) {
    if std::env::var("GIT_AI_DEBUG_DAEMON_TRACE")
        .ok()
        .as_deref()
        .is_some_and(|v| v == "1")
    {
        tracing::debug!(
            command = cmd.invoked_command.clone().unwrap_or_default(),
            primary = cmd.primary_command.clone().unwrap_or_default(),
            seq,
            argv = ?cmd.raw_argv,
            invoked_args = ?cmd.invoked_args,
            ref_changes_len = cmd.ref_changes.len(),
            ref_changes = ?cmd.ref_changes,
            events = ?events,
            exit_code = cmd.exit_code,
            "side-effect trace"
        );
    }
}
