use std::collections::HashSet;
use std::path::Path;

use crate::clients::git_cli::exec_git;
use crate::error::GitAiError;
use crate::operations::git::oid::is_non_zero_oid;
use crate::operations::git::repository::Repository;
use crate::operations::git::sequencer_args::{
    cherry_pick_source_args_from_command_args, revert_source_args_from_command_args,
};

use super::{
    is_valid_oid, parsed_invocation_for_normalized_command, rebase_is_control_mode,
    valid_non_zero_ref_change,
};
use crate::clients::git_cli::exec_git_stdin;
use crate::operations::git::repo_state::git_dir_for_worktree;

pub(crate) fn rebase_new_tip_from_command(
    cmd: &crate::model::domain::NormalizedCommand,
    original_head: &str,
) -> Option<String> {
    if let Some(new_tip) = cmd
        .ref_changes
        .iter()
        .rev()
        .find(|change| {
            change.reference.starts_with("refs/heads/")
                && valid_non_zero_ref_change(change)
                && change.old == original_head
        })
        .map(|change| change.new.clone())
    {
        return Some(new_tip);
    }

    if !rebase_is_control_mode(cmd) {
        return None;
    }

    let branch_ref_names = cmd
        .ref_changes
        .iter()
        .filter(|change| {
            change.reference.starts_with("refs/heads/") && valid_non_zero_ref_change(change)
        })
        .map(|change| change.reference.as_str())
        .collect::<std::collections::HashSet<_>>();
    if branch_ref_names.len() == 1
        && let Some(new_tip) = cmd
            .ref_changes
            .iter()
            .rev()
            .find(|change| {
                change.reference.starts_with("refs/heads/") && valid_non_zero_ref_change(change)
            })
            .map(|change| change.new.clone())
    {
        return Some(new_tip);
    }

    cmd.ref_changes
        .iter()
        .rev()
        .find(|change| change.reference == "HEAD" && valid_non_zero_ref_change(change))
        .map(|change| change.new.clone())
}

pub(crate) fn cherry_pick_destination_commits(
    cmd: &crate::model::domain::NormalizedCommand,
) -> Vec<String> {
    cmd.ref_changes
        .iter()
        .filter(|change| change.reference == "HEAD")
        .filter(|change| {
            is_non_zero_oid(&change.old) && is_non_zero_oid(&change.new) && change.old != change.new
        })
        .map(|change| change.new.clone())
        .collect()
}

fn first_head_transition_old(cmd: &crate::model::domain::NormalizedCommand) -> Option<String> {
    cmd.ref_changes
        .iter()
        .find(|change| {
            change.reference == "HEAD"
                && is_non_zero_oid(&change.old)
                && is_non_zero_oid(&change.new)
                && change.old != change.new
        })
        .map(|change| change.old.clone())
}

pub(crate) fn cherry_pick_original_head(
    cmd: &crate::model::domain::NormalizedCommand,
) -> Option<String> {
    first_head_transition_old(cmd)
}

pub(crate) fn revert_original_head(
    cmd: &crate::model::domain::NormalizedCommand,
) -> Option<String> {
    first_head_transition_old(cmd)
}

pub(crate) fn cherry_pick_source_args_for_side_effect(
    cmd: &crate::model::domain::NormalizedCommand,
) -> Vec<String> {
    let parsed = parsed_invocation_for_normalized_command(cmd);
    if parsed.command.as_deref() != Some("cherry-pick")
        && cmd.primary_command.as_deref() != Some("cherry-pick")
    {
        return Vec::new();
    }

    cherry_pick_source_args_from_command_args(&parsed.command_args)
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()
}

pub(crate) fn cherry_pick_command_has_flag(
    cmd: &crate::model::domain::NormalizedCommand,
    flag: &str,
) -> bool {
    let parsed = parsed_invocation_for_normalized_command(cmd);
    if parsed.command.as_deref() != Some("cherry-pick")
        && cmd.primary_command.as_deref() != Some("cherry-pick")
    {
        return false;
    }

    parsed.command_args.iter().any(|arg| arg == flag)
}

fn cherry_pick_source_is_range(source: &str) -> bool {
    source.contains("..")
}

fn cherry_pick_range_has_omitted_side(source: &str) -> bool {
    if let Some((left, right)) = source.split_once("...") {
        left.is_empty() || right.is_empty()
    } else if let Some((left, right)) = source.split_once("..") {
        left.is_empty() || right.is_empty()
    } else {
        false
    }
}

pub(crate) fn resolve_cherry_pick_source_args_with_git_in_head_context(
    repo: &Repository,
    source_args: &[String],
    head_context: Option<&str>,
) -> Result<Vec<String>, GitAiError> {
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();

    for source in source_args {
        let source = head_context
            .map(|head| rewrite_head_source_arg_for_side_effect(source, head))
            .unwrap_or_else(|| source.clone());
        let oids = if cherry_pick_source_is_range(&source) {
            if cherry_pick_range_has_omitted_side(&source) {
                Vec::new()
            } else {
                resolve_cherry_pick_range_source_with_git(repo, &source)?
            }
        } else {
            resolve_cherry_pick_single_source_with_git(repo, &source)?
        };

        for oid in oids {
            if seen.insert(oid.clone()) {
                resolved.push(oid);
            }
        }
    }

    Ok(resolved)
}

fn rewrite_head_source_arg_for_side_effect(source: &str, head_context: &str) -> String {
    if head_context.is_empty() || !is_valid_oid(head_context) {
        return source.to_string();
    }
    if let Some((left, right)) = source.split_once("...") {
        return format!(
            "{}...{}",
            rewrite_head_source_term_for_side_effect(left, head_context),
            rewrite_head_source_term_for_side_effect(right, head_context)
        );
    }
    if let Some((left, right)) = source.split_once("..") {
        return format!(
            "{}..{}",
            rewrite_head_source_term_for_side_effect(left, head_context),
            rewrite_head_source_term_for_side_effect(right, head_context)
        );
    }
    rewrite_head_source_term_for_side_effect(source, head_context)
}

fn rewrite_head_source_term_for_side_effect(term: &str, head_context: &str) -> String {
    if term == "HEAD" || term == "@" {
        return head_context.to_string();
    }
    if let Some(suffix) = term.strip_prefix("HEAD")
        && (suffix.starts_with('~') || suffix.starts_with('^'))
    {
        return format!("{head_context}{suffix}");
    }
    if let Some(suffix) = term.strip_prefix('@')
        && (suffix.starts_with('~') || suffix.starts_with('^'))
    {
        return format!("{head_context}{suffix}");
    }
    term.to_string()
}

fn resolve_cherry_pick_single_source_with_git(
    repo: &Repository,
    source: &str,
) -> Result<Vec<String>, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "cat-file".to_string(),
        "--batch-check=%(objectname) %(objecttype)".to_string(),
    ]);
    let stdin_data = format!("{source}^{{commit}}\n");
    let output = exec_git_stdin(&args, stdin_data.as_bytes())?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let oid = parts.next()?;
            (parts.next() == Some("commit") && is_valid_oid(oid)).then(|| oid.to_string())
        })
        .collect())
}

fn resolve_cherry_pick_range_source_with_git(
    repo: &Repository,
    source: &str,
) -> Result<Vec<String>, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "rev-list".to_string(),
        "--reverse".to_string(),
        source.to_string(),
    ]);
    let output = exec_git(&args)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| is_valid_oid(line))
        .map(ToOwned::to_owned)
        .collect())
}

pub(crate) fn resolve_explicit_cherry_pick_sources_for_side_effect(
    repo: &Repository,
    cmd: &crate::model::domain::NormalizedCommand,
) -> Result<Vec<String>, GitAiError> {
    let source_args = cherry_pick_source_args_for_side_effect(cmd);
    if source_args.is_empty() {
        return Ok(Vec::new());
    }
    let original_head = cherry_pick_original_head(cmd);
    resolve_cherry_pick_source_args_with_git_in_head_context(
        repo,
        &source_args,
        original_head.as_deref(),
    )
}

pub(crate) fn revert_source_args_for_side_effect(
    cmd: &crate::model::domain::NormalizedCommand,
) -> Vec<String> {
    let parsed = parsed_invocation_for_normalized_command(cmd);
    if parsed.command.as_deref() != Some("revert")
        && cmd.primary_command.as_deref() != Some("revert")
    {
        return Vec::new();
    }

    revert_source_args_from_command_args(&parsed.command_args)
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()
}

pub(crate) fn resolve_explicit_revert_sources_for_side_effect(
    repo: &Repository,
    cmd: &crate::model::domain::NormalizedCommand,
) -> Result<Vec<String>, GitAiError> {
    let source_args = revert_source_args_for_side_effect(cmd);
    if source_args.is_empty() {
        return Ok(Vec::new());
    }
    let original_head = revert_original_head(cmd);
    resolve_cherry_pick_source_args_with_git_in_head_context(
        repo,
        &source_args,
        original_head.as_deref(),
    )
}

pub(crate) fn cherry_pick_state_exists_for_worktree(worktree: &Path) -> bool {
    git_dir_for_worktree(worktree).is_some_and(|git_dir| {
        git_dir.join("CHERRY_PICK_HEAD").exists() || git_dir.join("sequencer").join("todo").exists()
    })
}
