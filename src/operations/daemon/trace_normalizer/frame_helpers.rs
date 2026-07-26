use crate::error::GitAiError;
use crate::operations::daemon::trace_helpers::{
    rfc3339_to_unix_nanos, trace_argv_primary_command, trace_token_is_git_executable,
};
use crate::operations::git::cli_parser::parse_git_cli_args;
use crate::operations::git::repo_state::worktree_root_for_path;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

use super::PendingTraceCommand;

pub(super) fn trace_debug_lifecycle(message: &str) {
    if std::env::var("GIT_AI_DEBUG_DAEMON_TRACE").is_ok() {
        eprintln!("\u{1b}[1;33m[git-ai]\u{1b}[0m {}", message);
    }
}

pub(super) fn payload_timestamp_ns(payload: &Value) -> Result<u128, GitAiError> {
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
    Ok(crate::model::clock::now_nanos())
}

pub(super) fn payload_worktree(payload: &Value) -> Option<PathBuf> {
    payload
        .get("worktree")
        .or_else(|| payload.get("repo_working_dir"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .map(|path| worktree_root_for_path(&path).unwrap_or(path))
}

pub(super) fn payload_cwd(payload: &Value) -> Option<PathBuf> {
    payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .map(|path| worktree_root_for_path(&path).unwrap_or(path))
}

pub(super) fn payload_reflog_start_offsets(payload: &Value) -> HashMap<String, u64> {
    payload
        .get(crate::operations::daemon::TRACE_ROOT_REFLOG_START_OFFSETS_FIELD)
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| value.as_u64().map(|offset| (key.clone(), offset)))
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn merge_reflog_start_offsets_from_payload(
    pending: &mut PendingTraceCommand,
    payload: &Value,
) {
    for (key, offset) in payload_reflog_start_offsets(payload) {
        pending.reflog_start_offsets.entry(key).or_insert(offset);
    }
}

pub(super) fn trace_argv_has_executable_prefix(argv: &[String]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    let file_name = std::path::Path::new(first)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(first);
    file_name.eq_ignore_ascii_case("git") || file_name.eq_ignore_ascii_case("git.exe")
}

pub(super) fn trace_argv_invocation_tokens(argv: &[String]) -> &[String] {
    if trace_argv_has_executable_prefix(argv) {
        &argv[1..]
    } else {
        argv
    }
}

pub(super) fn canonical_invocation(
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

pub(super) fn args_after_command(argv: &[String], command: &str) -> Vec<String> {
    argv.iter()
        .position(|arg| arg == command)
        .and_then(|idx| argv.get(idx + 1..))
        .map(|args| args.to_vec())
        .unwrap_or_default()
}

pub(super) fn is_internal_cmd_name(name: &str) -> bool {
    name.starts_with("_run_")
}

pub(super) fn worktree_from_argv(argv: &[String]) -> Option<PathBuf> {
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

pub(super) fn command_may_mutate_refs(primary_command: Option<&str>, raw_argv: &[String]) -> bool {
    primary_command.is_some_and(|command| {
        let (_invoked_command, invoked_args) = canonical_invocation(raw_argv, Some(command));
        crate::operations::git::command_classification::git_invocation_may_mutate_repo_state(
            command,
            &invoked_args,
        )
    })
}

pub(super) fn select_primary_command(
    root_cmd_name: Option<&str>,
    observed_child_commands: &[String],
    argv: &[String],
) -> Option<String> {
    if let Some(name) = root_cmd_name
        && !is_internal_cmd_name(name)
        && !trace_token_is_git_executable(name)
    {
        return Some(name.to_string());
    }

    for child in observed_child_commands {
        if !is_internal_cmd_name(child) && !trace_token_is_git_executable(child) {
            return Some(child.clone());
        }
    }

    trace_argv_primary_command(argv)
}
