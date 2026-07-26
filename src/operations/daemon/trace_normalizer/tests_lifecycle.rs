use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::GitAiError;
use crate::model::domain::FamilyKey;
use crate::operations::daemon::git_backend::GitBackend;
use crate::operations::daemon::trace_helpers::trace_argv_primary_command;
use crate::operations::git::cli_parser::parse_git_cli_args;

use super::TraceNormalizer;
use super::frame_helpers::{args_after_command, payload_timestamp_ns};

pub(super) fn normalize_path_key_from_str(path: &str) -> String {
    PathBuf::from(path).to_string_lossy().replace('\\', "/")
}

pub(super) fn normalize_path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[derive(Default)]
pub(super) struct MockBackend {
    pub(super) family_by_worktree: Mutex<HashMap<String, FamilyKey>>,
    pub(super) alias_by_worktree_command: Mutex<HashMap<String, HashMap<String, String>>>,
}

impl MockBackend {
    pub(super) fn set_family(&self, worktree: &str, family: &str) {
        self.family_by_worktree.lock().unwrap().insert(
            normalize_path_key_from_str(worktree),
            FamilyKey::new(family.to_string()),
        );
    }

    pub(super) fn set_alias(&self, worktree: &str, alias: &str, target_command: &str) {
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
        let raw = trace_argv_primary_command(argv);
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

pub(super) fn payload(event: &str, sid: &str, ts: u64) -> serde_json::Value {
    serde_json::json!({
        "event": event,
        "sid": sid,
        "ts": ts,
    })
}

pub(super) fn atexit_payload(sid: &str, ts: u64) -> serde_json::Value {
    serde_json::json!({
        "event": "atexit",
        "sid": sid,
        "ts": ts,
        "code": 0,
    })
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
        crate::operations::daemon::TRACE_ROOT_REFLOG_START_OFFSETS_FIELD.to_string(),
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
    fs::create_dir_all(worktree.join(".git")).expect("create git dir");
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
    fs::create_dir_all(worktree.join(".git")).expect("create git dir");
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
    fs::create_dir_all(worktree.join(".git")).expect("create git dir");
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
    fs::create_dir_all(worktree.join(".git")).expect("create git dir");
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
