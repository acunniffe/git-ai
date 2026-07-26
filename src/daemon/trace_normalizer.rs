use crate::daemon::domain::{CommandScope, Confidence, FamilyKey, NormalizedCommand};
use crate::daemon::git_backend::GitBackend;
use crate::error::GitAiError;
use crate::git::cli_parser::parse_git_cli_args;
use crate::git::repo_state::{
    common_dir_for_repo_path, common_dir_for_worktree, worktree_root_for_path,
};
use crate::observability;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    backend: Arc<B>,
    state: TraceNormalizerState,
}

const COMPLETED_ROOT_RETENTION_LIMIT: usize = 16_384;

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

    fn is_completed_root(&self, root_sid: &str) -> bool {
        self.state.completed_roots.contains(root_sid)
    }

    fn mark_completed_root_with_limit(&mut self, root_sid: &str, limit: usize) {
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

    fn mark_completed_root(&mut self, root_sid: &str) {
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

    fn resolve_primary_hint(
        &self,
        root_cmd_name: Option<&str>,
        observed_child_commands: &[String],
        raw_argv: &[String],
        worktree: Option<&Path>,
        family_key: Option<&FamilyKey>,
    ) -> Result<Option<String>, GitAiError> {
        let argv_primary = argv_primary_command(raw_argv);
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

    fn refresh_pending_mutation_capture(&mut self, root_sid: &str) -> Result<(), GitAiError> {
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
        payload: &Value,
    ) -> Result<Option<NormalizedCommand>, GitAiError> {
        let event = payload
            .get("event")
            .and_then(Value::as_str)
            .ok_or_else(|| GitAiError::Generic("trace payload missing event".to_string()))?;
        let sid = payload
            .get("sid")
            .and_then(Value::as_str)
            .ok_or_else(|| GitAiError::Generic("trace payload missing sid".to_string()))?;
        let root_sid = root_sid(sid).to_string();
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

    fn handle_start(
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

        let raw_argv = payload_argv(payload);
        let worktree = payload_worktree(payload)
            .or_else(|| worktree_from_argv(&raw_argv))
            .or_else(|| payload_cwd(payload))
            .or_else(|| self.state.sid_to_worktree.get(root_sid).cloned());

        let family_key = if let Some(worktree) = worktree.as_deref() {
            if let Some(common_dir) = common_dir_for_worktree(worktree) {
                let family = FamilyKey::new(
                    common_dir
                        .canonicalize()
                        .unwrap_or(common_dir)
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

    fn handle_def_param(
        &mut self,
        _payload: &Value,
        _root_sid: &str,
    ) -> Result<Option<NormalizedCommand>, GitAiError> {
        Ok(None)
    }

    fn handle_def_repo(
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
            .map(|repo| worktree_from_def_repo_repo(&repo).unwrap_or(repo))
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
            .and_then(|pending| argv_primary_command(&pending.raw_argv))
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
                common_dir
                    .canonicalize()
                    .unwrap_or(common_dir)
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

    fn handle_cmd_name(
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

    fn handle_exit(
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

    fn finalize_root_exit(
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
                    common_dir
                        .canonicalize()
                        .unwrap_or(common_dir)
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
                        common_dir
                            .canonicalize()
                            .unwrap_or(common_dir)
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

fn trace_debug_lifecycle(message: &str) {
    if std::env::var("GIT_AI_DEBUG_DAEMON_TRACE").is_ok() {
        eprintln!("\u{1b}[1;33m[git-ai]\u{1b}[0m {}", message);
    }
}

fn payload_timestamp_ns(payload: &Value) -> Result<u128, GitAiError> {
    for key in ["ts", "time_ns", "time"] {
        if let Some(time) = payload.get(key).and_then(Value::as_u64) {
            return Ok(time as u128);
        }
    }
    if let Some(time) = payload
        .get("time")
        .and_then(Value::as_str)
        .and_then(rfc3339_to_unix_nanos)
    {
        return Ok(time);
    }
    if let Some(seconds) = payload.get("t_abs").and_then(Value::as_f64) {
        return Ok((seconds * 1_000_000_000_f64) as u128);
    }
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos())
}

fn rfc3339_to_unix_nanos(value: &str) -> Option<u128> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .and_then(|timestamp| u128::try_from(timestamp.timestamp_nanos_opt()?).ok())
}

fn payload_argv(payload: &Value) -> Vec<String> {
    payload
        .get("argv")
        .and_then(Value::as_array)
        .map(|argv| {
            argv.iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn payload_worktree(payload: &Value) -> Option<PathBuf> {
    payload
        .get("worktree")
        .or_else(|| payload.get("repo_working_dir"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .map(|path| worktree_root_for_path(&path).unwrap_or(path))
}

fn payload_cwd(payload: &Value) -> Option<PathBuf> {
    payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .map(|path| worktree_root_for_path(&path).unwrap_or(path))
}

fn payload_reflog_start_offsets(payload: &Value) -> HashMap<String, u64> {
    payload
        .get(crate::daemon::TRACE_ROOT_REFLOG_START_OFFSETS_FIELD)
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| value.as_u64().map(|offset| (key.clone(), offset)))
                .collect()
        })
        .unwrap_or_default()
}

fn merge_reflog_start_offsets_from_payload(pending: &mut PendingTraceCommand, payload: &Value) {
    for (key, offset) in payload_reflog_start_offsets(payload) {
        pending.reflog_start_offsets.entry(key).or_insert(offset);
    }
}

fn worktree_from_def_repo_repo(repo: &Path) -> Option<PathBuf> {
    if repo.file_name().and_then(|name| name.to_str()) == Some(".git") {
        return repo.parent().map(PathBuf::from);
    }

    let linked_gitdir = repo.join("gitdir");
    if linked_gitdir.is_file() {
        let content = fs::read_to_string(&linked_gitdir).ok()?;
        let path = PathBuf::from(content.trim());
        if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            return path.parent().map(PathBuf::from);
        }
    }

    None
}

fn trace_argv_has_executable_prefix(argv: &[String]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    let file_name = std::path::Path::new(first)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(first);
    file_name.eq_ignore_ascii_case("git") || file_name.eq_ignore_ascii_case("git.exe")
}

fn trace_argv_invocation_tokens(argv: &[String]) -> &[String] {
    if trace_argv_has_executable_prefix(argv) {
        &argv[1..]
    } else {
        argv
    }
}

fn canonical_invocation(
    raw_argv: &[String],
    primary_command: Option<&str>,
) -> (Option<String>, Vec<String>) {
    let tokens = trace_argv_invocation_tokens(raw_argv);
    let parsed = parse_git_cli_args(tokens);
    if let Some(command) = parsed.command {
        return (Some(command), parsed.command_args);
    }
    if let Some(command) = primary_command.filter(|value| !value.trim().is_empty()) {
        return (
            Some(command.to_string()),
            args_after_command(tokens, command),
        );
    }
    (None, Vec::new())
}

fn args_after_command(argv: &[String], command: &str) -> Vec<String> {
    argv.iter()
        .position(|arg| arg == command)
        .and_then(|idx| argv.get(idx + 1..))
        .map(|args| args.to_vec())
        .unwrap_or_default()
}

fn root_sid(sid: &str) -> &str {
    sid.split('/').next().unwrap_or(sid)
}

fn is_internal_cmd_name(name: &str) -> bool {
    name.starts_with("_run_")
}

fn worktree_from_argv(argv: &[String]) -> Option<PathBuf> {
    let mut idx = 0;
    while idx < argv.len() {
        if argv[idx] == "-C" && idx + 1 < argv.len() {
            let path = PathBuf::from(argv[idx + 1].clone());
            return Some(worktree_root_for_path(&path).unwrap_or(path));
        }
        idx += 1;
    }
    None
}

fn argv_primary_command(argv: &[String]) -> Option<String> {
    let mut idx = 0;
    if argv.first().map(|v| is_git_binary(v)).unwrap_or(false) {
        idx = 1;
    }
    while idx < argv.len() {
        let token = argv[idx].as_str();
        if token == "-C" {
            idx += 2;
            continue;
        }
        if takes_value_option(token) {
            idx += 2;
            continue;
        }
        if token.starts_with("--") && token.contains('=') {
            idx += 1;
            continue;
        }
        if token.starts_with('-') {
            idx += 1;
            continue;
        }
        return Some(token.to_string());
    }
    None
}

fn is_git_binary(token: &str) -> bool {
    if token == "git" || token == "git.exe" {
        return true;
    }
    std::path::Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name == "git" || name == "git.exe")
        .unwrap_or(false)
}

fn takes_value_option(token: &str) -> bool {
    matches!(
        token,
        "-c" | "--config-env"
            | "--git-dir"
            | "--work-tree"
            | "--namespace"
            | "--super-prefix"
            | "--exec-path"
            | "--worktree-attributes"
            | "--attr-source"
    )
}

fn command_may_mutate_refs(primary_command: Option<&str>, raw_argv: &[String]) -> bool {
    primary_command.is_some_and(|command| {
        let (_invoked_command, invoked_args) = canonical_invocation(raw_argv, Some(command));
        crate::git::command_classification::git_invocation_may_mutate_repo_state(
            command,
            &invoked_args,
        )
    })
}

fn select_primary_command(
    root_cmd_name: Option<&str>,
    observed_child_commands: &[String],
    argv: &[String],
) -> Option<String> {
    if let Some(name) = root_cmd_name
        && !is_internal_cmd_name(name)
        && !is_git_binary(name)
    {
        return Some(name.to_string());
    }

    for child in observed_child_commands {
        if !is_internal_cmd_name(child) && !is_git_binary(child) {
            return Some(child.clone());
        }
    }

    argv_primary_command(argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    fn normalize_path_key_from_str(path: &str) -> String {
        PathBuf::from(path).to_string_lossy().replace('\\', "/")
    }

    fn normalize_path_key(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    #[derive(Default)]
    struct MockBackend {
        family_by_worktree: Mutex<HashMap<String, FamilyKey>>,
        alias_by_worktree_command: Mutex<HashMap<String, HashMap<String, String>>>,
    }

    impl MockBackend {
        fn set_family(&self, worktree: &str, family: &str) {
            self.family_by_worktree.lock().unwrap().insert(
                normalize_path_key_from_str(worktree),
                FamilyKey::new(family.to_string()),
            );
        }

        fn set_alias(&self, worktree: &str, alias: &str, target_command: &str) {
            self.alias_by_worktree_command
                .lock()
                .unwrap()
                .entry(normalize_path_key_from_str(worktree))
                .or_default()
                .insert(alias.to_string(), target_command.to_string());
        }
    }

    impl GitBackend for MockBackend {
        fn resolve_family(&self, worktree: &Path) -> Result<FamilyKey, GitAiError> {
            self.family_by_worktree
                .lock()
                .unwrap()
                .get(&normalize_path_key(worktree))
                .cloned()
                .ok_or_else(|| GitAiError::Generic("family not found".to_string()))
        }

        fn resolve_primary_command(
            &self,
            worktree: &Path,
            argv: &[String],
        ) -> Result<Option<String>, GitAiError> {
            Ok(self
                .resolve_invocation(worktree, argv)?
                .map(|(command, _args)| command))
        }

        fn resolve_invocation(
            &self,
            worktree: &Path,
            argv: &[String],
        ) -> Result<Option<(String, Vec<String>)>, GitAiError> {
            let raw = argv_primary_command(argv);
            let Some(command) = raw else {
                return Ok(None);
            };
            let worktree_key = normalize_path_key(worktree);
            // Stored alias targets may carry flags (e.g. "pull --rebase"); split
            // them into a command token plus leading args, then append the
            // invocation's own trailing args, mirroring real alias expansion.
            let expansion = self
                .alias_by_worktree_command
                .lock()
                .unwrap()
                .get(&worktree_key)
                .and_then(|commands| commands.get(&command))
                .cloned();
            let trailing = args_after_command(argv, &command);
            match expansion {
                Some(target) => {
                    let mut tokens = target.split_whitespace().map(str::to_string);
                    let Some(resolved_command) = tokens.next() else {
                        return Ok(Some((command, trailing)));
                    };
                    let mut args = tokens.collect::<Vec<_>>();
                    args.extend(trailing);
                    Ok(Some((resolved_command, args)))
                }
                None => Ok(Some((command, trailing))),
            }
        }

        fn clone_target(&self, _argv: &[String], _cwd_hint: Option<&Path>) -> Option<PathBuf> {
            let tokens: &[String] = if _argv
                .first()
                .is_some_and(|value| value == "git" || value == "git.exe")
            {
                &_argv[1..]
            } else {
                _argv
            };
            let parsed = parse_git_cli_args(tokens);
            if parsed.command.as_deref() != Some("clone") {
                return None;
            }

            let args = parsed.command_args;
            let mut positional = Vec::new();
            let mut idx = 0;
            while idx < args.len() {
                let arg = &args[idx];
                if arg == "--" {
                    positional.extend(args[idx + 1..].iter().cloned());
                    break;
                }
                if arg.starts_with('-') {
                    let takes_value = matches!(
                        arg.as_str(),
                        "-b" | "--branch"
                            | "--origin"
                            | "--upload-pack"
                            | "--template"
                            | "--separate-git-dir"
                            | "--reference"
                            | "--dissociate"
                            | "--config"
                            | "--object-format"
                    );
                    if takes_value && idx + 1 < args.len() {
                        idx += 2;
                        continue;
                    }
                    idx += 1;
                    continue;
                }
                positional.push(arg.clone());
                idx += 1;
            }
            if positional.is_empty() {
                return None;
            }
            let target = if positional.len() >= 2 {
                PathBuf::from(&positional[1])
            } else {
                let source = positional[0].trim_end_matches('/');
                let source = source.strip_suffix(".git").unwrap_or(source);
                let name = source.rsplit('/').next()?.rsplit(':').next()?.to_string();
                if name.is_empty() {
                    return None;
                }
                PathBuf::from(name)
            };
            Some(if target.is_absolute() {
                target
            } else if let Some(cwd) = _cwd_hint {
                cwd.join(target)
            } else {
                target
            })
        }

        fn init_target(&self, _argv: &[String], _cwd_hint: Option<&Path>) -> Option<PathBuf> {
            let tokens: &[String] = if _argv
                .first()
                .is_some_and(|value| value == "git" || value == "git.exe")
            {
                &_argv[1..]
            } else {
                _argv
            };
            let parsed = parse_git_cli_args(tokens);
            if parsed.command.as_deref() != Some("init") {
                return None;
            }

            let args = parsed.command_args;
            let mut positional = Vec::new();
            let mut idx = 0;
            while idx < args.len() {
                let arg = &args[idx];
                if arg == "--" {
                    positional.extend(args[idx + 1..].iter().cloned());
                    break;
                }
                if arg.starts_with('-') {
                    idx += 1;
                    continue;
                }
                positional.push(arg.clone());
                idx += 1;
            }
            let target = positional
                .first()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            Some(if target.is_absolute() {
                target
            } else if let Some(cwd) = _cwd_hint {
                cwd.join(target)
            } else {
                target
            })
        }
    }

    fn payload(event: &str, sid: &str, ts: u64) -> Value {
        serde_json::json!({
            "event": event,
            "sid": sid,
            "ts": ts,
        })
    }

    fn atexit_payload(sid: &str, ts: u64) -> Value {
        serde_json::json!({
            "event": "atexit",
            "sid": sid,
            "ts": ts,
            "code": 0,
        })
    }

    fn create_git_dir(worktree: &Path) {
        let git_dir = worktree.join(".git");
        fs::create_dir_all(&git_dir).expect("create git dir");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
    }

    #[test]
    fn payload_timestamp_prefers_stock_trace2_rfc3339_time_over_relative_t_abs() {
        let payload = serde_json::json!({
            "event": "start",
            "sid": "s-time",
            "time": "2026-06-09T22:47:40.822668Z",
            "t_abs": 0.000226,
        });

        assert_eq!(
            payload_timestamp_ns(&payload).unwrap(),
            1_781_045_260_822_668_000
        );
    }

    #[test]
    fn normalizer_emits_one_command_for_start_exit_atexit() {
        let backend = Arc::new(MockBackend::default());
        backend.set_family("/repo", "/repo/.git");
        let mut normalizer = TraceNormalizer::new(backend);

        let start = serde_json::json!({
            "event":"start",
            "sid":"s1",
            "ts":1,
            "argv":["git","status"],
            "worktree":"/repo"
        });
        let exit = serde_json::json!({
            "event":"exit",
            "sid":"s1",
            "ts":2,
            "code":0
        });
        let atexit = serde_json::json!({
            "event":"atexit",
            "sid":"s1",
            "ts":3,
            "code":0
        });

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
        assert_eq!(cmd.root_sid, "s1");
        assert_eq!(cmd.primary_command.as_deref(), Some("status"));
        assert_eq!(cmd.exit_code, 0);
    }

    #[test]
    fn normalizer_preserves_reflog_start_offsets_from_def_repo() {
        let backend = Arc::new(MockBackend::default());
        backend.set_family("/repo", "/repo/.git");
        let mut normalizer = TraceNormalizer::new(backend);

        let start = serde_json::json!({
            "event":"start",
            "sid":"s-reflog-offsets",
            "ts":1,
            "argv":["git","stash","push"],
            "worktree":"/repo"
        });
        let mut def_repo = serde_json::json!({
            "event":"def_repo",
            "sid":"s-reflog-offsets",
            "ts":2,
            "worktree":"/repo"
        });
        def_repo.as_object_mut().unwrap().insert(
            crate::daemon::TRACE_ROOT_REFLOG_START_OFFSETS_FIELD.to_string(),
            serde_json::json!({"common:refs/stash": 123_u64}),
        );
        let atexit = atexit_payload("s-reflog-offsets", 3);

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&def_repo).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();

        assert_eq!(
            cmd.reflog_start_offsets.get("common:refs/stash"),
            Some(&123)
        );
    }

    #[test]
    fn normalizer_uses_atexit_when_exit_is_missing() {
        let backend = Arc::new(MockBackend::default());
        backend.set_family("/repo", "/repo/.git");
        let mut normalizer = TraceNormalizer::new(backend);

        let start = serde_json::json!({
            "event":"start",
            "sid":"s1-atexit",
            "ts":1,
            "argv":["git","status"],
            "worktree":"/repo"
        });
        let atexit = serde_json::json!({
            "event":"atexit",
            "sid":"s1-atexit",
            "ts":2,
            "code":0
        });

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
        assert_eq!(cmd.root_sid, "s1-atexit");
        assert_eq!(cmd.primary_command.as_deref(), Some("status"));
        assert_eq!(cmd.exit_code, 0);
    }

    #[test]
    fn normalizer_defers_root_completion_until_atexit() {
        let backend = Arc::new(MockBackend::default());
        backend.set_family("/repo", "/repo/.git");
        let mut normalizer = TraceNormalizer::new(backend);

        let start = serde_json::json!({
            "event":"start",
            "sid":"s1-defer",
            "ts":1,
            "argv":["git","rebase","main","feature"],
            "worktree":"/repo"
        });
        let exit = serde_json::json!({
            "event":"exit",
            "sid":"s1-defer",
            "ts":2,
            "code":0
        });
        let atexit = serde_json::json!({
            "event":"atexit",
            "sid":"s1-defer",
            "ts":3,
            "code":0
        });

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(
            normalizer.ingest_payload(&exit).unwrap().is_none(),
            "trace2 exit fires before Git atexit cleanup, so it must not finalize reflog-driven side effects"
        );
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
        assert_eq!(cmd.root_sid, "s1-defer");
        assert_eq!(cmd.primary_command.as_deref(), Some("rebase"));
        assert_eq!(cmd.exit_code, 0);
    }

    #[test]
    fn completed_root_retention_does_not_clear_all_recent_roots() {
        let backend = Arc::new(MockBackend::default());
        let mut normalizer = TraceNormalizer::new(backend);

        normalizer.mark_completed_root_with_limit("root-a", 3);
        normalizer.mark_completed_root_with_limit("root-b", 3);
        normalizer.mark_completed_root_with_limit("root-c", 3);

        let late_payload = serde_json::json!({
            "event":"atexit",
            "sid":"root-a",
            "ts":10,
            "code":0
        });
        assert!(
            normalizer.ingest_payload(&late_payload).unwrap().is_none(),
            "late payloads for recently completed roots should stay ignored"
        );

        normalizer.mark_completed_root_with_limit("root-d", 3);
        assert_eq!(normalizer.state.completed_roots.len(), 3);
        assert!(normalizer.state.completed_roots.contains("root-b"));
        assert!(normalizer.state.completed_roots.contains("root-c"));
        assert!(normalizer.state.completed_roots.contains("root-d"));
        assert!(!normalizer.state.completed_roots.contains("root-a"));
        assert_eq!(normalizer.state.completed_root_order.len(), 3);
    }

    #[test]
    fn alias_commit_resolves_primary_command() {
        let backend = Arc::new(MockBackend::default());
        let temp = tempfile::tempdir().expect("create tempdir");
        let worktree = temp.path().join("repo");
        create_git_dir(&worktree);
        backend.set_alias(worktree.to_str().expect("utf8 worktree"), "ci", "commit");
        let mut normalizer = TraceNormalizer::new(backend);

        let start = serde_json::json!({
            "event":"start",
            "sid":"alias-commit",
            "ts":1,
            "argv":["git","ci","-m","msg"],
            "worktree":worktree
        });
        let exit = serde_json::json!({
            "event":"exit",
            "sid":"alias-commit",
            "ts":2,
            "code":0
        });
        let atexit = atexit_payload("alias-commit", 3);

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
        assert_eq!(cmd.primary_command.as_deref(), Some("commit"));
    }

    #[test]
    fn alias_pull_rebase_expands_invoked_args_with_flags() {
        // `git up origin main` where `up = pull --rebase` must surface the
        // alias-expanded flags so downstream reflog-action matching sees
        // `pull --rebase ...` (matching git's reflog label) rather than the
        // literal alias token `up`.
        let backend = Arc::new(MockBackend::default());
        let temp = tempfile::tempdir().expect("create tempdir");
        let worktree = temp.path().join("repo");
        create_git_dir(&worktree);
        backend.set_alias(
            worktree.to_str().expect("utf8 worktree"),
            "up",
            "pull --rebase",
        );
        let mut normalizer = TraceNormalizer::new(backend);

        let start = serde_json::json!({
            "event":"start",
            "sid":"alias-pull",
            "ts":1,
            "argv":["git","up","origin","main"],
            "worktree":worktree
        });
        // git emits a child cmd_name of `pull` for the expanded alias, exactly
        // as observed in real trace2 output, so the primary command is already
        // resolved without consulting the backend.
        let child = serde_json::json!({
            "event":"cmd_name",
            "sid":"alias-pull/child",
            "ts":1,
            "name":"pull",
            "hierarchy":"_run_git_alias_/pull"
        });
        let exit = serde_json::json!({
            "event":"exit",
            "sid":"alias-pull",
            "ts":2,
            "code":0
        });
        let atexit = atexit_payload("alias-pull", 3);

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&child).unwrap().is_none());
        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();

        assert_eq!(cmd.primary_command.as_deref(), Some("pull"));
        assert_eq!(cmd.invoked_command.as_deref(), Some("pull"));
        assert_eq!(
            cmd.invoked_args,
            vec![
                "--rebase".to_string(),
                "origin".to_string(),
                "main".to_string()
            ],
            "alias-expanded invocation must carry the --rebase flag and trailing args"
        );
    }

    #[test]
    fn alias_pull_rebase_expands_invoked_args_without_child_cmd_name() {
        // If a trace stream omits child cmd_name events, alias resolution must
        // still use the backend fallback instead of leaving the literal alias
        // token as the normalized command.
        let backend = Arc::new(MockBackend::default());
        let temp = tempfile::tempdir().expect("create tempdir");
        let worktree = temp.path().join("repo");
        create_git_dir(&worktree);
        backend.set_alias(
            worktree.to_str().expect("utf8 worktree"),
            "up",
            "pull --rebase",
        );
        let mut normalizer = TraceNormalizer::new(backend);

        let start = serde_json::json!({
            "event":"start",
            "sid":"alias-pull-no-child",
            "ts":1,
            "argv":["git","up","origin","main"],
            "worktree":worktree
        });
        let exit = serde_json::json!({
            "event":"exit",
            "sid":"alias-pull-no-child",
            "ts":2,
            "code":0
        });
        let atexit = atexit_payload("alias-pull-no-child", 3);

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();

        assert_eq!(cmd.primary_command.as_deref(), Some("pull"));
        assert_eq!(cmd.invoked_command.as_deref(), Some("pull"));
        assert_eq!(
            cmd.invoked_args,
            vec![
                "--rebase".to_string(),
                "origin".to_string(),
                "main".to_string()
            ],
            "backend fallback must preserve alias-expanded flags without child events"
        );
    }

    #[test]
    fn non_alias_pull_invocation_is_unchanged() {
        // A plain (non-alias) invocation must expand to the identical command
        // token, leaving invoked_args byte-identical to the pre-alias behavior.
        let backend = Arc::new(MockBackend::default());
        let temp = tempfile::tempdir().expect("create tempdir");
        let worktree = temp.path().join("repo");
        create_git_dir(&worktree);
        let mut normalizer = TraceNormalizer::new(backend);

        let start = serde_json::json!({
            "event":"start",
            "sid":"plain-pull",
            "ts":1,
            "argv":["git","pull","--rebase","origin","main"],
            "worktree":worktree
        });
        let exit = serde_json::json!({
            "event":"exit",
            "sid":"plain-pull",
            "ts":2,
            "code":0
        });
        let atexit = atexit_payload("plain-pull", 3);

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();

        assert_eq!(cmd.primary_command.as_deref(), Some("pull"));
        assert_eq!(cmd.invoked_command.as_deref(), Some("pull"));
        assert_eq!(
            cmd.invoked_args,
            vec![
                "--rebase".to_string(),
                "origin".to_string(),
                "main".to_string()
            ]
        );
    }

    #[test]
    fn normalizer_defers_exit_seen_before_start_until_atexit() {
        let backend = Arc::new(MockBackend::default());
        backend.set_family("/repo", "/repo/.git");
        let mut normalizer = TraceNormalizer::new(backend);

        let exit = serde_json::json!({
            "event":"exit",
            "sid":"s2",
            "ts":10,
            "code":0
        });
        let start = serde_json::json!({
            "event":"start",
            "sid":"s2",
            "ts":1,
            "argv":["git","status"],
            "worktree":"/repo"
        });
        let atexit = serde_json::json!({
            "event":"atexit",
            "sid":"s2",
            "ts":11,
            "code":0
        });

        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
        assert_eq!(cmd.root_sid, "s2");
        assert_eq!(cmd.primary_command.as_deref(), Some("status"));
        assert_eq!(cmd.exit_code, 0);
    }

    #[test]
    fn child_cmd_name_enriches_root() {
        let backend = Arc::new(MockBackend::default());
        backend.set_family("/repo", "/repo/.git");
        let mut normalizer = TraceNormalizer::new(backend);

        let start = serde_json::json!({
            "event":"start",
            "sid":"s3",
            "ts":1,
            "argv":["git","foo"],
            "worktree":"/repo"
        });
        let child = serde_json::json!({
            "event":"cmd_name",
            "sid":"s3/child1",
            "ts":2,
            "name":"status"
        });
        let exit = serde_json::json!({
            "event":"exit",
            "sid":"s3",
            "ts":3,
            "code":0
        });
        let atexit = atexit_payload("s3", 4);

        normalizer.ingest_payload(&start).unwrap();
        normalizer.ingest_payload(&child).unwrap();
        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
        assert_eq!(cmd.observed_child_commands, vec!["status".to_string()]);
        assert_eq!(cmd.primary_command.as_deref(), Some("status"));
    }

    #[test]
    fn child_exit_does_not_finalize_without_root_exit() {
        let backend = Arc::new(MockBackend::default());
        backend.set_family("/repo", "/repo/.git");
        let mut normalizer = TraceNormalizer::new(backend);

        let start = serde_json::json!({
            "event":"start",
            "sid":"s-exec",
            "ts":1,
            "argv":["git","notes","show","abc123"],
            "worktree":"/repo"
        });
        let cmd_name = serde_json::json!({
            "event":"cmd_name",
            "sid":"s-exec",
            "ts":2,
            "name":"notes"
        });
        let exec = serde_json::json!({
            "event":"exec",
            "sid":"s-exec",
            "ts":3,
            "argv":["git","show","def456"]
        });
        let child_exit = serde_json::json!({
            "event":"exit",
            "sid":"s-exec/child",
            "ts":4,
            "code":0
        });
        let root_exit = serde_json::json!({
            "event":"exit",
            "sid":"s-exec",
            "ts":5,
            "code":0
        });
        let root_atexit = atexit_payload("s-exec", 6);

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&cmd_name).unwrap().is_none());
        assert!(normalizer.ingest_payload(&exec).unwrap().is_none());
        assert!(normalizer.ingest_payload(&child_exit).unwrap().is_none());
        assert_eq!(normalizer.state().pending.len(), 1);

        assert!(normalizer.ingest_payload(&root_exit).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&root_atexit).unwrap().unwrap();
        assert_eq!(cmd.root_sid, "s-exec");
        assert_eq!(cmd.primary_command.as_deref(), Some("notes"));
        assert_eq!(cmd.exit_code, 0);
        assert!(normalizer.state().pending.is_empty());
    }

    #[test]
    fn child_exit_before_root_exec_is_ignored_until_root_exit() {
        let backend = Arc::new(MockBackend::default());
        backend.set_family("/repo", "/repo/.git");
        let mut normalizer = TraceNormalizer::new(backend);

        let start = serde_json::json!({
            "event":"start",
            "sid":"s-exec-oop",
            "ts":1,
            "argv":["git","notes","show","abc123"],
            "worktree":"/repo"
        });
        let cmd_name = serde_json::json!({
            "event":"cmd_name",
            "sid":"s-exec-oop",
            "ts":2,
            "name":"notes"
        });
        let child_exit = serde_json::json!({
            "event":"exit",
            "sid":"s-exec-oop/child",
            "ts":3,
            "code":0
        });
        let exec = serde_json::json!({
            "event":"exec",
            "sid":"s-exec-oop",
            "ts":4,
            "argv":["git","show","def456"]
        });
        let root_exit = serde_json::json!({
            "event":"exit",
            "sid":"s-exec-oop",
            "ts":5,
            "code":0
        });
        let root_atexit = atexit_payload("s-exec-oop", 6);

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&cmd_name).unwrap().is_none());
        assert!(normalizer.ingest_payload(&child_exit).unwrap().is_none());
        assert_eq!(normalizer.state().pending.len(), 1);

        assert!(normalizer.ingest_payload(&exec).unwrap().is_none());
        assert!(normalizer.ingest_payload(&root_exit).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&root_atexit).unwrap().unwrap();
        assert_eq!(cmd.root_sid, "s-exec-oop");
        assert_eq!(cmd.primary_command.as_deref(), Some("notes"));
        assert_eq!(cmd.exit_code, 0);
        assert!(normalizer.state().pending.is_empty());
    }

    #[test]
    fn clone_relative_target_falls_back_to_argv_target_when_def_repo_candidate_fails() {
        let backend = Arc::new(MockBackend::default());
        let mut normalizer = TraceNormalizer::new(backend);
        let temp = tempfile::tempdir().expect("create tempdir");
        let outer = temp.path().join("outer");
        let clone_dir = outer.join("nested").join("relative-clone");
        create_git_dir(&clone_dir);

        let start = serde_json::json!({
            "event":"start",
            "sid":"clone-rel",
            "ts":1,
            "argv":["git","clone","ssh://example/repo.git","nested/relative-clone"],
            "worktree":outer
        });
        let def_repo = serde_json::json!({
            "event":"def_repo",
            "sid":"clone-rel",
            "ts":2,
            "repo":clone_dir.join(".git")
        });
        let exit = serde_json::json!({
            "event":"exit",
            "sid":"clone-rel",
            "ts":3,
            "code":0
        });
        let atexit = atexit_payload("clone-rel", 4);

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&def_repo).unwrap().is_none());
        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();

        assert_eq!(cmd.primary_command.as_deref(), Some("clone"));
        assert_eq!(cmd.worktree.as_ref(), Some(&clone_dir));
        assert!(matches!(cmd.scope, CommandScope::Family(_)));
    }

    #[test]
    fn clone_with_late_family_resolution_does_not_need_ref_metadata() {
        let backend = Arc::new(MockBackend::default());
        let mut normalizer = TraceNormalizer::new(backend);
        let temp = tempfile::tempdir().expect("create tempdir");
        let outer = temp.path().join("outer");
        let clone_dir = outer.join("nested").join("relative-clone");
        fs::create_dir_all(clone_dir.parent().expect("clone parent")).expect("create clone parent");

        let start = serde_json::json!({
            "event":"start",
            "sid":"clone-late-family",
            "ts":1,
            "argv":["git","clone","ssh://example/repo.git","nested/relative-clone"],
            "worktree":outer
        });
        let def_repo = serde_json::json!({
            "event":"def_repo",
            "sid":"clone-late-family",
            "ts":2,
            "worktree":clone_dir
        });
        let cmd_name = serde_json::json!({
            "event":"cmd_name",
            "sid":"clone-late-family",
            "ts":3,
            "name":"clone"
        });
        let exit = serde_json::json!({
            "event":"exit",
            "sid":"clone-late-family",
            "ts":4,
            "code":0
        });
        let atexit = atexit_payload("clone-late-family", 5);

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&def_repo).unwrap().is_none());
        assert!(normalizer.ingest_payload(&cmd_name).unwrap().is_none());

        // Simulate repo discoverability only once clone is about to exit.
        create_git_dir(&clone_dir);

        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        let cmd = normalizer
            .ingest_payload(&atexit)
            .expect("clone finalize should not error")
            .expect("clone should emit a normalized command");

        assert_eq!(cmd.primary_command.as_deref(), Some("clone"));
        assert!(matches!(cmd.scope, CommandScope::Family(_)));
    }

    #[test]
    fn clone_prefers_target_family_over_source_cwd_family() {
        let backend = Arc::new(MockBackend::default());
        let mut normalizer = TraceNormalizer::new(backend);
        let temp = tempfile::tempdir().expect("create tempdir");
        let source_repo = temp.path().join("source-repo");
        let cloned_repo = temp.path().join("cloned-repo");
        create_git_dir(&source_repo);
        create_git_dir(&cloned_repo);

        let start = serde_json::json!({
            "event":"start",
            "sid":"clone-source-cwd",
            "ts":1,
            "argv":["git","clone","ssh://example/repo.git",cloned_repo],
            "worktree":source_repo
        });
        let def_repo = serde_json::json!({
            "event":"def_repo",
            "sid":"clone-source-cwd",
            "ts":2,
            "repo":cloned_repo.join(".git")
        });
        let exit = serde_json::json!({
            "event":"exit",
            "sid":"clone-source-cwd",
            "ts":3,
            "code":0
        });
        let atexit = atexit_payload("clone-source-cwd", 4);

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&def_repo).unwrap().is_none());
        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();

        assert_eq!(cmd.primary_command.as_deref(), Some("clone"));
        assert_eq!(cmd.worktree.as_ref(), Some(&cloned_repo));
        let expected_family = cloned_repo
            .join(".git")
            .canonicalize()
            .unwrap_or_else(|_| cloned_repo.join(".git"));
        assert_eq!(
            cmd.family_key.as_ref().map(|family| family.0.as_str()),
            Some(expected_family.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn clone_child_def_repo_does_not_overwrite_root_worktree() {
        // Real git trace2 output shows child processes (remote-https, index-pack)
        // emit def_repo with the CWD as worktree, not the clone destination.
        // The root process's def_repo has the correct newly-created repo path.
        // Verify that child def_repo events don't clobber the root's worktree.
        let backend = Arc::new(MockBackend::default());
        let mut normalizer = TraceNormalizer::new(backend);
        let temp = tempfile::tempdir().expect("create tempdir");
        let cwd = temp.path().join("projects"); // non-repo CWD
        let clone_dest = cwd.join("testing-git"); // the clone destination
        create_git_dir(&clone_dest);

        let root_sid = "20260327T000000.000000Z-Hdeadbeef-P00010000";
        let child_sid = format!("{}/20260327T000000.000001Z-Hdeadbeef-P00010001", root_sid);

        let start = serde_json::json!({
            "event": "start",
            "sid": root_sid,
            "ts": 1,
            "argv": ["git", "clone", "https://github.com/svarlamov/testing-git"]
            // No worktree or cwd — matches real trace2 start from non-repo dir
        });
        // Root def_repo: correct clone destination
        let root_def_repo = serde_json::json!({
            "event": "def_repo",
            "sid": root_sid,
            "ts": 2,
            "worktree": clone_dest
        });
        // Child def_repo from remote-https: reports CWD (parent), not destination
        let child_def_repo = serde_json::json!({
            "event": "def_repo",
            "sid": child_sid,
            "ts": 3,
            "worktree": cwd
        });
        let exit = serde_json::json!({
            "event": "exit",
            "sid": root_sid,
            "ts": 4,
            "code": 0
        });
        let atexit = atexit_payload(root_sid, 5);

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&root_def_repo).unwrap().is_none());
        // Child def_repo must NOT overwrite the root worktree
        assert!(
            normalizer
                .ingest_payload(&child_def_repo)
                .unwrap()
                .is_none()
        );

        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
        assert_eq!(cmd.primary_command.as_deref(), Some("clone"));
        assert_eq!(
            cmd.worktree.as_ref(),
            Some(&clone_dest),
            "clone worktree should be the destination, not the parent CWD"
        );
    }

    #[test]
    fn no_repo_routes_to_global_scope() {
        let backend = Arc::new(MockBackend::default());
        let mut normalizer = TraceNormalizer::new(backend);

        let start = serde_json::json!({
            "event":"start",
            "sid":"s4",
            "ts":1,
            "argv":["git","version"]
        });
        let exit = serde_json::json!({
            "event":"exit",
            "sid":"s4",
            "ts":2,
            "code":0
        });
        let atexit = atexit_payload("s4", 3);

        normalizer.ingest_payload(&start).unwrap();
        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
        assert!(matches!(cmd.scope, CommandScope::Global));
    }

    #[test]
    fn ignores_non_supported_trace_events() {
        let backend = Arc::new(MockBackend::default());
        let mut normalizer = TraceNormalizer::new(backend);
        let p = payload("region_enter", "s5", 1);
        assert!(normalizer.ingest_payload(&p).unwrap().is_none());
    }

    #[test]
    fn interleaved_roots_with_out_of_order_exits_finalize_independently() {
        let backend = Arc::new(MockBackend::default());
        let mut normalizer = TraceNormalizer::new(backend);
        let temp = tempfile::tempdir().expect("create tempdir");
        let repo_a = temp.path().join("repo-a");
        let repo_b = temp.path().join("repo-b");
        create_git_dir(&repo_a);
        create_git_dir(&repo_b);

        let start_a = serde_json::json!({
            "event":"start",
            "sid":"s-a",
            "ts":1,
            "argv":["git","commit","-m","a"],
            "worktree":repo_a
        });
        let start_b = serde_json::json!({
            "event":"start",
            "sid":"s-b",
            "ts":2,
            "argv":["git","push","origin","main"],
            "worktree":repo_b
        });
        let exit_b = serde_json::json!({
            "event":"exit",
            "sid":"s-b",
            "ts":3,
            "code":0
        });
        let exit_a = serde_json::json!({
            "event":"exit",
            "sid":"s-a",
            "ts":4,
            "code":0
        });
        let atexit_b = atexit_payload("s-b", 5);
        let atexit_a = atexit_payload("s-a", 6);

        assert!(normalizer.ingest_payload(&start_a).unwrap().is_none());
        assert!(normalizer.ingest_payload(&start_b).unwrap().is_none());

        assert!(normalizer.ingest_payload(&exit_b).unwrap().is_none());
        let cmd_b = normalizer.ingest_payload(&atexit_b).unwrap().unwrap();
        assert_eq!(cmd_b.root_sid, "s-b");
        assert_eq!(cmd_b.primary_command.as_deref(), Some("push"));
        assert_eq!(cmd_b.worktree.as_deref(), Some(repo_b.as_path()));
        assert!(matches!(cmd_b.scope, CommandScope::Family(_)));

        assert!(normalizer.ingest_payload(&exit_a).unwrap().is_none());
        let cmd_a = normalizer.ingest_payload(&atexit_a).unwrap().unwrap();
        assert_eq!(cmd_a.root_sid, "s-a");
        assert_eq!(cmd_a.primary_command.as_deref(), Some("commit"));
        assert_eq!(cmd_a.worktree.as_deref(), Some(repo_a.as_path()));
        assert!(matches!(cmd_a.scope, CommandScope::Family(_)));

        assert!(normalizer.state().pending.is_empty());
    }

    #[test]
    fn start_ignores_repo_gitdir_hint_and_uses_cwd_for_worktree_resolution() {
        let backend = Arc::new(MockBackend::default());
        let mut normalizer = TraceNormalizer::new(backend.clone());
        let temp = tempfile::tempdir().expect("create tempdir");
        let repo_base = temp.path().join("repo-base");
        let common_git_dir = repo_base.join(".git");
        let worker_git_dir = common_git_dir.join("worktrees").join("worker-b");
        let worker_worktree = temp.path().join("repo-worker-b");
        let worker_head = "1111111111111111111111111111111111111111";
        fs::create_dir_all(common_git_dir.join("refs").join("heads"))
            .expect("create common refs/heads");
        fs::create_dir_all(&worker_git_dir).expect("create linked worktree git dir");
        fs::create_dir_all(&worker_worktree).expect("create worker worktree");
        fs::write(
            worker_worktree.join(".git"),
            format!("gitdir: {}\n", worker_git_dir.display()),
        )
        .expect("write linked worktree .git pointer");
        fs::write(worker_git_dir.join("HEAD"), "ref: refs/heads/worker-b\n")
            .expect("write worker HEAD");
        fs::write(
            common_git_dir.join("refs").join("heads").join("worker-b"),
            format!("{worker_head}\n"),
        )
        .expect("write worker branch ref");

        let start = serde_json::json!({
            "event":"start",
            "sid":"s-repo-field",
            "ts":1,
            "argv":["git","commit","-m","msg"],
            "repo":common_git_dir,
            "cwd":worker_worktree
        });
        let def_repo = serde_json::json!({
            "event":"def_repo",
            "sid":"s-repo-field",
            "ts":2,
            "repo":worker_git_dir
        });
        let cmd_name = serde_json::json!({
            "event":"cmd_name",
            "sid":"s-repo-field",
            "ts":3,
            "name":"commit"
        });
        let exit = serde_json::json!({
            "event":"exit",
            "sid":"s-repo-field",
            "ts":4,
            "code":0
        });
        let atexit = atexit_payload("s-repo-field", 5);

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&def_repo).unwrap().is_none());
        assert!(normalizer.ingest_payload(&cmd_name).unwrap().is_none());

        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
        assert_eq!(cmd.worktree.as_deref(), Some(worker_worktree.as_path()));
    }

    #[test]
    fn destructive_stash_can_normalize_without_pre_command_target_oid() {
        let backend = Arc::new(MockBackend::default());
        let mut normalizer = TraceNormalizer::new(backend);
        let temp = tempfile::tempdir().expect("create tempdir");
        let repo = temp.path().join("repo");
        create_git_dir(&repo);

        let start = serde_json::json!({
            "event":"start",
            "sid":"stash-missing-meta",
            "ts":1,
            "argv":["git","stash","pop"],
            "worktree":repo
        });
        let exit = serde_json::json!({
            "event":"exit",
            "sid":"stash-missing-meta",
            "ts":2,
            "code":0
        });
        let atexit = atexit_payload("stash-missing-meta", 3);

        assert!(normalizer.ingest_payload(&start).unwrap().is_none());
        assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
        let cmd = normalizer
            .ingest_payload(&atexit)
            .expect("missing stash metadata should not block normalization")
            .expect("atexit payload should emit a normalized command");
        assert!(cmd.stash_target_oid.is_none());
    }
}
