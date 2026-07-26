use crate::error::GitAiError;
use crate::model::domain::{CommandScope, Confidence, FamilyKey, NormalizedCommand};
use crate::observability;
use crate::operations::daemon::git_backend::GitBackend;
use crate::operations::daemon::trace_helpers::{
    daemon_worktree_from_repo_path, trace_argv_primary_command, trace_payload_argv,
};
use crate::operations::git::repo_state::{
    common_dir_for_repo_path, common_dir_for_worktree, worktree_root_for_path,
};
use serde_json::Value;
use std::path::PathBuf;

use super::frame_helpers::{
    canonical_invocation, is_internal_cmd_name, merge_reflog_start_offsets_from_payload,
    payload_cwd, payload_reflog_start_offsets, payload_worktree, trace_debug_lifecycle,
    worktree_from_argv,
};
use super::{DeferredRootExit, PendingTraceCommand, TraceNormalizer};

impl<B: GitBackend> TraceNormalizer<B> {
    pub(super) fn handle_start(
        &mut self,
        payload: &Value,
        sid: &str,
        root_sid: &str,
        started_at_ns: u128,
    ) -> Result<Option<NormalizedCommand>, GitAiError> {
        if sid != root_sid {
            return Ok(None);
        }
        if self.is_completed_root(root_sid) {
            return Ok(None);
        }

        let raw_argv = trace_payload_argv(payload);
        let worktree = payload_worktree(payload)
            .or_else(|| worktree_from_argv(&raw_argv))
            .or_else(|| payload_cwd(payload))
            .or_else(|| self.state.sid_to_worktree.get(root_sid).cloned());

        let family_key = if let Some(worktree) = worktree.as_deref() {
            if let Some(common_dir) = common_dir_for_worktree(worktree) {
                let family = FamilyKey::new(
                    crate::operations::git::canonicalize::canonicalize_or_self(&common_dir)
                        .to_string_lossy()
                        .to_string(),
                );
                self.state
                    .sid_to_family
                    .insert(root_sid.to_string(), family.clone());
                Some(family)
            } else {
                self.state.sid_to_family.get(root_sid).cloned()
            }
        } else {
            self.state.sid_to_family.get(root_sid).cloned()
        };

        let pending = PendingTraceCommand {
            root_sid: root_sid.to_string(),
            raw_argv,
            root_cmd_name: None,
            observed_child_commands: Vec::new(),
            invocation_worktree: worktree.clone(),
            worktree,
            family_key,
            started_at_ns,
            exit_code: None,
            finished_at_ns: None,
            reflog_start_offsets: payload_reflog_start_offsets(payload),
            saw_def_repo: false,
        };
        trace_debug_lifecycle(&format!(
            "trace normalizer start sid={} argv={:?} worktree={:?}",
            root_sid, pending.raw_argv, pending.worktree
        ));
        self.state.pending.insert(root_sid.to_string(), pending);
        if let Some(prestart_cmd_name) = self.state.prestart_root_cmd_names.remove(root_sid)
            && let Some(pending) = self.state.pending.get_mut(root_sid)
            && pending.root_cmd_name.is_none()
        {
            pending.root_cmd_name = Some(prestart_cmd_name);
        }
        if let Some(deferred) = self.state.deferred_exits.remove(root_sid) {
            if deferred.is_atexit {
                return self.finalize_root_exit(
                    root_sid,
                    deferred.exit_code,
                    deferred.finished_at_ns,
                );
            }
            self.state
                .deferred_exits
                .insert(root_sid.to_string(), deferred);
        }

        Ok(None)
    }

    pub(super) fn handle_def_param(
        &mut self,
        _payload: &Value,
        _root_sid: &str,
    ) -> Result<Option<NormalizedCommand>, GitAiError> {
        Ok(None)
    }

    pub(super) fn handle_def_repo(
        &mut self,
        payload: &Value,
        _sid: &str,
        root_sid: &str,
    ) -> Result<Option<NormalizedCommand>, GitAiError> {
        let payload_worktree = payload_worktree(payload);
        let payload_repo = payload
            .get("repo")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .map(|repo| daemon_worktree_from_repo_path(&repo).unwrap_or(repo))
            .map(|repo| worktree_root_for_path(&repo).unwrap_or(repo));

        let pending_worktree = self
            .state
            .pending
            .get(root_sid)
            .and_then(|pending| pending.worktree.clone());
        let prefer_def_repo_target = self
            .state
            .pending
            .get(root_sid)
            .and_then(|pending| trace_argv_primary_command(&pending.raw_argv))
            .is_some_and(|command| matches!(command.as_str(), "clone" | "init"));

        // For clone/init the root process's def_repo carries the newly created
        // repo path.  Child processes (remote-https, index-pack, rev-list, …)
        // inherit the parent CWD and their def_repo reports that CWD — not the
        // clone destination.  Once we've captured the root def_repo (first
        // arrival), skip subsequent child def_repo events entirely to prevent
        // overwriting the correct worktree, family, and sid lookup maps.
        if prefer_def_repo_target {
            let already_saw_def_repo = self
                .state
                .pending
                .get(root_sid)
                .is_some_and(|pending| pending.saw_def_repo);
            if already_saw_def_repo {
                return Ok(None);
            }
        }

        // Trace2 `def_repo.repo` may point at a common-dir `.git` path for worktrees.
        // For normal in-repo commands we keep the start/cwd-derived worktree when available.
        // For clone/init the `def_repo` target is the repo we actually created and must win.
        let repo = if prefer_def_repo_target {
            payload_worktree
                .or(payload_repo)
                .or(pending_worktree)
                .ok_or_else(|| GitAiError::Generic("def_repo missing repo path".to_string()))?
        } else {
            payload_worktree
                .or(pending_worktree)
                .or(payload_repo)
                .ok_or_else(|| GitAiError::Generic("def_repo missing repo path".to_string()))?
        };
        let repo = worktree_root_for_path(&repo).unwrap_or(repo);

        self.state
            .sid_to_worktree
            .insert(root_sid.to_string(), repo.clone());

        let family = common_dir_for_repo_path(&repo).map(|common_dir| {
            FamilyKey::new(
                crate::operations::git::canonicalize::canonicalize_or_self(&common_dir)
                    .to_string_lossy()
                    .to_string(),
            )
        });
        if let Some(family) = family.as_ref() {
            self.state
                .sid_to_family
                .insert(root_sid.to_string(), family.clone());
        }
        if let Some(pending) = self.state.pending.get_mut(root_sid) {
            merge_reflog_start_offsets_from_payload(pending, payload);
            pending.saw_def_repo = true;
            pending.worktree = Some(repo);
            if let Some(family) = family.as_ref() {
                pending.family_key = Some(family.clone());
            }
        }
        self.refresh_pending_mutation_capture(root_sid)?;
        Ok(None)
    }

    pub(super) fn handle_cmd_name(
        &mut self,
        payload: &Value,
        sid: &str,
        root_sid: &str,
    ) -> Result<Option<NormalizedCommand>, GitAiError> {
        let cmd = payload
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| GitAiError::Generic("cmd_name missing name".to_string()))?
            .to_string();

        if is_internal_cmd_name(&cmd) {
            return Ok(None);
        }

        if sid == root_sid {
            if let Some(pending) = self.state.pending.get_mut(root_sid) {
                merge_reflog_start_offsets_from_payload(pending, payload);
                pending.root_cmd_name = Some(cmd);
            } else {
                self.state
                    .prestart_root_cmd_names
                    .insert(root_sid.to_string(), cmd);
                return Ok(None);
            }
            self.refresh_pending_mutation_capture(root_sid)?;
            return Ok(None);
        }

        if let Some(pending) = self.state.pending.get_mut(root_sid) {
            pending.observed_child_commands.push(cmd);
        }
        self.refresh_pending_mutation_capture(root_sid)?;
        Ok(None)
    }

    pub(super) fn handle_exit(
        &mut self,
        payload: &Value,
        sid: &str,
        root_sid: &str,
        finished_at_ns: u128,
        is_atexit: bool,
    ) -> Result<Option<NormalizedCommand>, GitAiError> {
        if sid != root_sid {
            let _ = payload;
            let _ = finished_at_ns;
            return Ok(None);
        }
        if self.is_completed_root(root_sid) {
            return Ok(None);
        }

        if let Some(pending) = self.state.pending.get_mut(root_sid) {
            merge_reflog_start_offsets_from_payload(pending, payload);
        }

        let exit_code = payload
            .get("code")
            .or_else(|| payload.get("exit_code"))
            .and_then(Value::as_i64)
            .unwrap_or(0) as i32;

        if !self.state.pending.contains_key(root_sid) {
            let deferred = self
                .state
                .deferred_exits
                .entry(root_sid.to_string())
                .or_insert(DeferredRootExit {
                    exit_code,
                    finished_at_ns,
                    is_atexit,
                });
            deferred.exit_code = exit_code;
            deferred.is_atexit |= is_atexit;
            if finished_at_ns > deferred.finished_at_ns {
                deferred.finished_at_ns = finished_at_ns;
            }
            trace_debug_lifecycle(&format!(
                "trace normalizer deferred terminal event sid={} code={} is_atexit={} (start not seen yet)",
                root_sid, exit_code, is_atexit
            ));
            return Ok(None);
        }

        if !is_atexit {
            let deferred = self
                .state
                .deferred_exits
                .entry(root_sid.to_string())
                .or_insert(DeferredRootExit {
                    exit_code,
                    finished_at_ns,
                    is_atexit: false,
                });
            deferred.exit_code = exit_code;
            if finished_at_ns > deferred.finished_at_ns {
                deferred.finished_at_ns = finished_at_ns;
            }
            trace_debug_lifecycle(&format!(
                "trace normalizer observed exit sid={} code={} waiting for atexit",
                root_sid, exit_code
            ));
            return Ok(None);
        }

        self.state.deferred_exits.remove(root_sid);
        trace_debug_lifecycle(&format!(
            "trace normalizer atexit sid={} code={} pending_before_finalize={}",
            root_sid,
            exit_code,
            self.state.pending.len()
        ));

        self.finalize_root_exit(root_sid, exit_code, finished_at_ns)
    }

    pub(super) fn finalize_root_exit(
        &mut self,
        root_sid: &str,
        exit_code: i32,
        finished_at_ns: u128,
    ) -> Result<Option<NormalizedCommand>, GitAiError> {
        let mut pending = self.state.pending.remove(root_sid).ok_or_else(|| {
            GitAiError::Generic("missing pending command at finalize".to_string())
        })?;

        pending.exit_code = Some(exit_code);
        pending.finished_at_ns = Some(finished_at_ns);

        if pending.worktree.is_none()
            && let Some(worktree) = self.state.sid_to_worktree.get(root_sid)
        {
            pending.worktree = Some(worktree.clone());
        }
        if pending.family_key.is_none()
            && let Some(family) = self.state.sid_to_family.get(root_sid)
        {
            pending.family_key = Some(family.clone());
        }
        if pending.family_key.is_none()
            && let Some(worktree) = pending.worktree.as_deref()
        {
            pending.family_key = common_dir_for_worktree(worktree).map(|common_dir| {
                FamilyKey::new(
                    crate::operations::git::canonicalize::canonicalize_or_self(&common_dir)
                        .to_string_lossy()
                        .to_string(),
                )
            });
        }

        let mut primary_command = self.resolve_primary_hint(
            pending.root_cmd_name.as_deref(),
            &pending.observed_child_commands,
            &pending.raw_argv,
            pending.worktree.as_deref(),
            pending.family_key.as_ref(),
        )?;
        let (mut invoked_command, mut invoked_args) =
            canonical_invocation(&pending.raw_argv, primary_command.as_deref());
        // Git expands user aliases (e.g. `up` → `pull --rebase`) before it runs
        // the command and before it writes reflog messages, so the literal argv
        // token and its trailing args do not reflect the flags that shaped the
        // reflog. Surface the alias-expanded command+args when the invoked token
        // was itself an alias, so downstream analyzers (notably the pull span
        // matcher, which reconstructs a command's reflog action from its args)
        // see the same flags git did.
        //
        // Fast path: only consult the backend when the resolved command differs
        // from the literal invoked token — that mismatch is exactly the alias
        // signature (git ignores an alias that shadows a builtin, so a plain
        // `pull`/`status`/etc. always has primary == invoked and is skipped
        // here). This keeps every common, non-aliased command off the
        // alias-cache path entirely, so trace-ingestion finalize pays nothing
        // extra for it.
        if primary_command.as_deref() != invoked_command.as_deref()
            && let Some(worktree) = pending.worktree.as_deref()
            && let Some((expanded_command, expanded_args)) = self
                .backend
                .resolve_invocation(worktree, &pending.raw_argv)?
            && Some(expanded_command.as_str()) != invoked_command.as_deref()
        {
            invoked_command = Some(expanded_command);
            invoked_args = expanded_args;
        }
        if primary_command.is_none() {
            primary_command = invoked_command.clone();
        }
        let confidence = Confidence::Low;
        let ref_changes = Vec::new();

        let mut family_key = pending.family_key.clone();
        let mut scope = if let Some(key) = family_key.clone() {
            CommandScope::Family(key)
        } else {
            CommandScope::Global
        };

        if exit_code == 0 && matches!(primary_command.as_deref(), Some("clone" | "init")) {
            let cwd_hint = pending.invocation_worktree.as_deref();
            let target_from_def_repo = pending
                .saw_def_repo
                .then(|| pending.worktree.clone())
                .flatten();
            let target_from_argv = if primary_command.as_deref() == Some("clone") {
                self.backend.clone_target(&pending.raw_argv, cwd_hint)
            } else {
                self.backend.init_target(&pending.raw_argv, cwd_hint)
            };

            let mut candidates = Vec::new();
            // Prefer the def_repo target — it comes from git's own trace2
            // event and is always an absolute path.  The argv-derived target
            // may be relative and resolve against an unrelated ancestor repo.
            if let Some(target) = target_from_def_repo.as_ref() {
                candidates.push(target.clone());
            }
            if let Some(target) = target_from_argv.as_ref() {
                let duplicate = candidates.iter().any(|existing| existing == target);
                if !duplicate {
                    candidates.push(target.clone());
                }
            }

            let mut resolved = false;
            let mut last_error: Option<(PathBuf, GitAiError)> = None;
            for candidate in candidates {
                if let Some(common_dir) = common_dir_for_repo_path(&candidate) {
                    let resolved_family = FamilyKey::new(
                        crate::operations::git::canonicalize::canonicalize_or_self(&common_dir)
                            .to_string_lossy()
                            .to_string(),
                    );
                    pending.worktree = Some(candidate);
                    family_key = Some(resolved_family.clone());
                    scope = CommandScope::Family(resolved_family);
                    resolved = true;
                    break;
                } else {
                    last_error = Some((
                        candidate.clone(),
                        GitAiError::Generic(format!(
                            "failed to resolve clone/init target family from filesystem: {}",
                            candidate.display()
                        )),
                    ));
                }
            }

            if !resolved {
                // Keep the best available worktree hint even when family resolution fails.
                if let Some(target) = target_from_def_repo.or(target_from_argv) {
                    pending.worktree = Some(target);
                }
                if let Some((target, error)) = last_error {
                    observability::log_error(
                        &error,
                        Some(serde_json::json!({
                            "component": "trace_normalizer",
                            "phase": "resolve_clone_or_init_target_family",
                            "root_sid": pending.root_sid,
                            "target": target,
                        })),
                    );
                }
            }
        }

        let normalized = NormalizedCommand {
            scope,
            family_key,
            worktree: pending.worktree,
            root_sid: pending.root_sid,
            raw_argv: pending.raw_argv,
            primary_command,
            invoked_command,
            invoked_args,
            observed_child_commands: pending.observed_child_commands,
            exit_code,
            started_at_ns: pending.started_at_ns,
            finished_at_ns,
            reflog_start_offsets: pending.reflog_start_offsets,
            stash_target_oid: None,
            cherry_pick_source_oids: Vec::new(),
            revert_source_oids: Vec::new(),
            ref_changes,
            confidence,
        };

        trace_debug_lifecycle(&format!(
            "trace normalizer finalized sid={} primary={:?} pending_after_finalize={}",
            root_sid,
            normalized.primary_command,
            self.state.pending.len()
        ));
        self.mark_completed_root(root_sid);
        let _ = self.state.sid_to_worktree.remove(root_sid);
        let _ = self.state.sid_to_family.remove(root_sid);
        let _ = self.state.prestart_root_cmd_names.remove(root_sid);

        Ok(Some(normalized))
    }
}
