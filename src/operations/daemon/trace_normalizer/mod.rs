use crate::error::GitAiError;
use crate::model::domain::{FamilyKey, NormalizedCommand};
use crate::operations::daemon::git_backend::GitBackend;
use crate::operations::daemon::trace_helpers::{trace_argv_primary_command, trace_root_sid};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod event_handlers;
mod frame_helpers;

use frame_helpers::{command_may_mutate_refs, payload_timestamp_ns, select_primary_command};

#[cfg(test)]
mod tests_clone;
#[cfg(test)]
mod tests_frame_primitives;
#[cfg(test)]
pub(super) mod tests_lifecycle;
#[cfg(test)]
mod tests_nested_repo;

#[derive(Debug, Clone)]
pub struct PendingTraceCommand {
    pub root_sid: String,
    pub raw_argv: Vec<String>,
    pub root_cmd_name: Option<String>,
    pub observed_child_commands: Vec<String>,
    pub invocation_worktree: Option<PathBuf>,
    pub worktree: Option<PathBuf>,
    pub family_key: Option<FamilyKey>,
    pub started_at_ns: u128,
    pub exit_code: Option<i32>,
    pub finished_at_ns: Option<u128>,
    pub reflog_start_offsets: HashMap<String, u64>,
    pub saw_def_repo: bool,
}

#[derive(Debug, Clone, Default)]
pub struct TraceNormalizerState {
    pub pending: HashMap<String, PendingTraceCommand>,
    pub deferred_exits: HashMap<String, DeferredRootExit>,
    pub completed_roots: HashSet<String>,
    pub completed_root_order: VecDeque<String>,
    pub sid_to_worktree: HashMap<String, PathBuf>,
    pub sid_to_family: HashMap<String, FamilyKey>,
    pub prestart_root_cmd_names: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct DeferredRootExit {
    pub exit_code: i32,
    pub finished_at_ns: u128,
    pub is_atexit: bool,
}

#[derive(Debug, Clone)]
pub struct OrphanTraceRoot {
    pub root_sid: String,
    pub raw_argv: Vec<String>,
    pub deferred_exit_only: bool,
}

pub struct TraceNormalizer<B: GitBackend> {
    pub(super) backend: Arc<B>,
    pub(super) state: TraceNormalizerState,
}

pub(super) const COMPLETED_ROOT_RETENTION_LIMIT: usize = 16_384;

impl<B: GitBackend> TraceNormalizer<B> {
    pub fn new(backend: Arc<B>) -> Self {
        Self {
            backend,
            state: TraceNormalizerState::default(),
        }
    }

    pub fn state(&self) -> &TraceNormalizerState {
        &self.state
    }

    pub(super) fn is_completed_root(&self, root_sid: &str) -> bool {
        self.state.completed_roots.contains(root_sid)
    }

    pub(super) fn mark_completed_root_with_limit(&mut self, root_sid: &str, limit: usize) {
        if self.state.completed_roots.insert(root_sid.to_string()) {
            self.state
                .completed_root_order
                .push_back(root_sid.to_string());
        }
        while self.state.completed_roots.len() > limit {
            let Some(oldest) = self.state.completed_root_order.pop_front() else {
                break;
            };
            self.state.completed_roots.remove(&oldest);
        }
    }

    pub(super) fn mark_completed_root(&mut self, root_sid: &str) {
        self.mark_completed_root_with_limit(root_sid, COMPLETED_ROOT_RETENTION_LIMIT);
    }

    pub fn remove_pending_root(&mut self, root_sid: &str) -> Option<PendingTraceCommand> {
        let removed = self.state.pending.remove(root_sid);
        if removed.is_some() {
            let _ = self.state.sid_to_worktree.remove(root_sid);
            let _ = self.state.sid_to_family.remove(root_sid);
            let _ = self.state.prestart_root_cmd_names.remove(root_sid);
        }
        removed
    }

    pub fn sweep_orphans(&mut self) -> Vec<OrphanTraceRoot> {
        let mut removed = Vec::new();

        let pending_roots = self.state.pending.keys().cloned().collect::<Vec<_>>();
        for root_sid in pending_roots {
            if let Some(pending) = self.remove_pending_root(&root_sid) {
                removed.push(OrphanTraceRoot {
                    root_sid,
                    raw_argv: pending.raw_argv,
                    deferred_exit_only: false,
                });
            }
        }

        let deferred_roots = self
            .state
            .deferred_exits
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for root_sid in deferred_roots {
            self.state.deferred_exits.remove(&root_sid);
            let _ = self.state.sid_to_worktree.remove(&root_sid);
            let _ = self.state.sid_to_family.remove(&root_sid);
            let _ = self.state.prestart_root_cmd_names.remove(&root_sid);
            removed.push(OrphanTraceRoot {
                root_sid,
                raw_argv: Vec::new(),
                deferred_exit_only: true,
            });
        }

        removed
    }

    pub fn sweep_orphans_for_roots(&mut self, roots: &[String]) -> Vec<OrphanTraceRoot> {
        let mut removed = Vec::new();

        for root_sid in roots {
            if let Some(pending) = self.remove_pending_root(root_sid) {
                removed.push(OrphanTraceRoot {
                    root_sid: root_sid.clone(),
                    raw_argv: pending.raw_argv,
                    deferred_exit_only: false,
                });
                continue;
            }

            if self.state.deferred_exits.remove(root_sid).is_some() {
                let _ = self.state.sid_to_worktree.remove(root_sid);
                let _ = self.state.sid_to_family.remove(root_sid);
                let _ = self.state.prestart_root_cmd_names.remove(root_sid);
                removed.push(OrphanTraceRoot {
                    root_sid: root_sid.clone(),
                    raw_argv: Vec::new(),
                    deferred_exit_only: true,
                });
            }
        }

        removed
    }

    pub(super) fn resolve_primary_hint(
        &self,
        root_cmd_name: Option<&str>,
        observed_child_commands: &[String],
        raw_argv: &[String],
        worktree: Option<&Path>,
        family_key: Option<&FamilyKey>,
    ) -> Result<Option<String>, GitAiError> {
        let argv_primary = trace_argv_primary_command(raw_argv);
        let selected = select_primary_command(root_cmd_name, observed_child_commands, raw_argv)
            .or_else(|| argv_primary.clone());
        let should_resolve_alias = match (&selected, &argv_primary) {
            // Keep child/root-derived command if it differs from the argv command.
            // Alias resolution should only rewrite the invoked command token.
            (Some(selected_cmd), Some(argv_cmd)) => selected_cmd == argv_cmd,
            (None, Some(_)) => true,
            _ => false,
        };
        if should_resolve_alias
            && let (Some(worktree), Some(_family)) = (worktree, family_key)
            && let Some(resolved) = self.backend.resolve_primary_command(worktree, raw_argv)?
        {
            return Ok(Some(resolved));
        }
        Ok(selected)
    }

    pub(super) fn refresh_pending_mutation_capture(
        &mut self,
        root_sid: &str,
    ) -> Result<(), GitAiError> {
        let (primary_hint, raw_argv) = {
            let pending = match self.state.pending.get(root_sid) {
                Some(pending) => pending,
                None => return Ok(()),
            };

            let (Some(worktree), Some(family)) =
                (pending.worktree.as_deref(), pending.family_key.as_ref())
            else {
                return Ok(());
            };

            (
                self.resolve_primary_hint(
                    pending.root_cmd_name.as_deref(),
                    &pending.observed_child_commands,
                    &pending.raw_argv,
                    Some(worktree),
                    Some(family),
                )?,
                pending.raw_argv.clone(),
            )
        };
        if !command_may_mutate_refs(primary_hint.as_deref(), &raw_argv) {
            return Ok(());
        }
        // Ref transitions are resolved by the family cursor after normalization.
        // Avoid any live snapshotting here to keep normalization race-free.
        Ok(())
    }

    pub fn ingest_payload(
        &mut self,
        payload: &serde_json::Value,
    ) -> Result<Option<NormalizedCommand>, GitAiError> {
        let event = payload
            .get("event")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| GitAiError::Generic("trace payload missing event".to_string()))?;
        let sid = payload
            .get("sid")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| GitAiError::Generic("trace payload missing sid".to_string()))?;
        let root_sid = trace_root_sid(sid).to_string();
        if self.is_completed_root(&root_sid) {
            return Ok(None);
        }
        let ts = payload_timestamp_ns(payload)?;

        match event {
            "start" => self.handle_start(payload, sid, &root_sid, ts),
            "def_repo" => self.handle_def_repo(payload, sid, &root_sid),
            "cmd_name" => self.handle_cmd_name(payload, sid, &root_sid),
            "def_param" => self.handle_def_param(payload, &root_sid),
            "exec" => Ok(None),
            "exit" => self.handle_exit(payload, sid, &root_sid, ts, false),
            "atexit" => self.handle_exit(payload, sid, &root_sid, ts, true),
            _ => Ok(None),
        }
    }
}
