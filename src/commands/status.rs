use crate::authorship::ignore::{
    IgnoreMatcher, build_ignore_matcher, effective_ignore_patterns, should_ignore_file_with_matcher,
};
use crate::authorship::stats::{CommitStats, stats_from_authorship_log, write_stats_to_terminal};
use crate::authorship::virtual_attribution::VirtualAttributions;
use crate::authorship::working_log::CheckpointKind;
use crate::error::GitAiError;
use crate::git::find_repository;
use crate::git::repo_storage::InitialAttributions;
use crate::git::repository::{InternalGitProfile, Repository, exec_git_with_profile};
use crate::git::status::MAX_PATHSPEC_ARGS;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize)]
struct CheckpointInfo {
    time_ago: String,
    additions: u32,
    deletions: u32,
    tool_model: String,
    is_human: bool,
}

#[derive(Serialize)]
struct StatusOutput {
    stats: CommitStats,
    /// Per-checkpoint session breakdown. Omitted entirely when `--diff-only`
    /// is requested, so consumers that only care about the current diff scope
    /// get just the diff-scoped `stats`.
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoints: Option<Vec<CheckpointInfo>>,
}

pub fn handle_status(args: &[String]) {
    let mut json_output = false;
    let mut diff_only = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json_output = true,
            "--diff-only" => diff_only = true,
            _ => {}
        }
        i += 1;
    }

    if let Err(e) = run_status(json_output, diff_only) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

fn run_status(json: bool, diff_only: bool) -> Result<(), GitAiError> {
    let repo = find_repository(&[])?;
    let ignore_patterns = effective_ignore_patterns(&repo, &[], &[]);
    let ignore_matcher = build_ignore_matcher(&ignore_patterns);

    let default_user_name = repo.effective_author_identity().formatted_or_unknown();

    let head = repo.head()?;
    let head_sha = head.target()?;

    let working_log = repo.storage.working_log_for_base_commit(&head_sha)?;
    let checkpoints = working_log.read_all_checkpoints()?;
    let initial_attributions = working_log.read_initial_attributions();

    let has_checkpoints = !checkpoints.is_empty();
    let has_initial = !initial_attributions.files.is_empty();

    if !has_checkpoints && !has_initial {
        if json {
            let output = StatusOutput {
                stats: CommitStats::default(),
                checkpoints: if diff_only { None } else { Some(vec![]) },
            };
            let json_str = serde_json::to_string(&output)?;
            println!("{}", json_str);
        } else {
            eprintln!(
                "No checkpoints recorded since last commit ({})",
                &head_sha[..7]
            );
            eprintln!();

            eprintln!(
                "If you've made AI edits recently and don't see them here, you might need to install hooks:"
            );
            eprintln!();
            eprintln!("  git-ai install-hooks");
            eprintln!();
        }
        return Ok(());
    }

    // The per-checkpoint breakdown is only needed for the default (non-diff-only)
    // output, so skip building it entirely when `--diff-only` is requested.
    let mut checkpoint_infos = Vec::new();
    if !diff_only {
        for checkpoint in checkpoints.iter().rev() {
            let (additions, deletions) = (
                checkpoint.line_stats.additions,
                checkpoint.line_stats.deletions,
            );

            let tool_model = checkpoint
                .agent_id
                .as_ref()
                .map(|a| format!("{} {}", &a.tool, &a.model))
                .unwrap_or_else(|| default_user_name.clone());

            let is_human = checkpoint.kind == CheckpointKind::Human;
            checkpoint_infos.push(CheckpointInfo {
                time_ago: format_time_ago(checkpoint.timestamp),
                additions,
                deletions,
                tool_model,
                is_human,
            });
        }
    }

    let working_va = VirtualAttributions::from_just_working_log(
        repo.clone(),
        head_sha.clone(),
        Some(default_user_name.clone()),
    )?;

    let mut pathspecs: HashSet<String> = checkpoints
        .iter()
        .flat_map(|cp| cp.entries.iter().map(|e| e.file.clone()))
        .filter(|file| !should_ignore_file_with_matcher(file, &ignore_matcher))
        .collect();
    for file_path in working_va.files() {
        if !should_ignore_file_with_matcher(&file_path, &ignore_matcher) {
            pathspecs.insert(file_path);
        }
    }

    let (authorship_log, initial, _) = working_va.to_authorship_log_and_initial_working_log(
        &repo,
        &head_sha,
        &head_sha,
        Some(&pathspecs),
        None,
    )?;

    // Get actual git diff stats between HEAD and working directory (like post_commit does)
    let (total_additions, total_deletions) =
        get_working_dir_diff_stats(&repo, Some(&pathspecs), &ignore_matcher)?;

    // For status (uncommitted changes), the AI attributions are in `initial` (uncommitted),
    // not in authorship_log.attestations (which is for committed changes).
    // Count AI lines from the uncommitted attributions.
    let ai_accepted = count_ai_lines_from_initial(&initial, &ignore_matcher);

    let stats = stats_from_authorship_log(
        Some(&authorship_log),
        total_additions,
        total_deletions,
        ai_accepted,
        0,
        &BTreeMap::new(),
    );

    if json {
        let output = StatusOutput {
            stats,
            checkpoints: if diff_only {
                None
            } else {
                Some(checkpoint_infos)
            },
        };
        let json_str = serde_json::to_string(&output)?;
        println!("{}", json_str);
        return Ok(());
    }

    write_stats_to_terminal(&stats, true);

    if diff_only {
        return Ok(());
    }

    println!();
    for cp in &checkpoint_infos {
        let add_str = if cp.additions > 0 {
            format!("+{}", cp.additions)
        } else {
            "0".to_string()
        };
        let del_str = if cp.deletions > 0 {
            format!("-{}", cp.deletions)
        } else {
            "0".to_string()
        };

        let line = format!(
            "{:<14} {:>5}  {:>5}  {}",
            cp.time_ago, add_str, del_str, cp.tool_model
        );

        if cp.is_human {
            println!("\x1b[90m{}\x1b[0m", line);
        } else {
            println!("{}", line);
        }
    }

    Ok(())
}

fn format_time_ago(timestamp: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let diff = now.saturating_sub(timestamp);

    if diff < 60 {
        format!("{} secs ago", diff)
    } else if diff < 3600 {
        format!("{} mins ago", diff / 60)
    } else if diff < 86400 {
        format!("{} hours ago", diff / 3600)
    } else {
        format!("{} days ago", diff / 86400)
    }
}

/// Get git diff statistics between HEAD and the working directory
/// This mirrors the logic in stats.rs get_git_diff_stats but for uncommitted changes
fn get_working_dir_diff_stats(
    repo: &Repository,
    pathspecs: Option<&HashSet<String>>,
    ignore_matcher: &IgnoreMatcher,
) -> Result<(u32, u32), GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.push("diff".to_string());
    args.push("--numstat".to_string());
    args.push("HEAD".to_string());

    // Add pathspecs if provided to scope the diff to specific files
    // Only pass as CLI args when under threshold to avoid E2BIG
    let needs_post_filter = if let Some(paths) = pathspecs {
        if paths.is_empty() {
            return Ok((0, 0));
        }
        if paths.len() > MAX_PATHSPEC_ARGS {
            // Disable rename detection so git reports renames as separate
            // delete + add entries with clean filenames. Without this,
            // numstat outputs "old => new" arrow notation in the filename
            // field, which won't match pathspec entries.
            args.push("--no-renames".to_string());
            true
        } else {
            args.push("--".to_string());
            for path in paths {
                args.push(path.clone());
            }
            false
        }
    } else {
        false
    };

    let output = exec_git_with_profile(&args, InternalGitProfile::NumstatParse)?;
    let stdout = String::from_utf8(output.stdout)?;

    let mut added_lines = 0u32;
    let mut deleted_lines = 0u32;

    // Parse numstat output
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        // Parse numstat format: "added\tdeleted\tfilename"
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            // Post-filter by pathspec when we couldn't pass them as CLI args
            if needs_post_filter
                && let Some(paths) = pathspecs
                && !paths.contains(parts[2])
            {
                continue;
            }

            let file_path = parts[2];
            if should_ignore_file_with_matcher(file_path, ignore_matcher) {
                continue;
            }

            // Parse added lines
            if let Ok(added) = parts[0].parse::<u32>() {
                added_lines += added;
            }

            // Parse deleted lines (handle "-" for binary files)
            if parts[1] != "-"
                && let Ok(deleted) = parts[1].parse::<u32>()
            {
                deleted_lines += deleted;
            }
        }
    }

    Ok((added_lines, deleted_lines))
}

/// Count AI-attributed lines from InitialAttributions (uncommitted changes)
fn count_ai_lines_from_initial(
    initial: &InitialAttributions,
    ignore_matcher: &IgnoreMatcher,
) -> u32 {
    let mut ai_lines = 0u32;

    for (file_path, line_attrs) in &initial.files {
        if should_ignore_file_with_matcher(file_path, ignore_matcher) {
            continue;
        }

        for line_attr in line_attrs {
            let is_ai = if line_attr.author_id.starts_with("s_") {
                let session_key = line_attr
                    .author_id
                    .split("::")
                    .next()
                    .unwrap_or(&line_attr.author_id);
                initial.sessions.contains_key(session_key)
            } else {
                initial.prompts.contains_key(&line_attr.author_id)
            };
            if is_ai {
                let lines_count = line_attr.end_line - line_attr.start_line + 1;
                ai_lines += lines_count;
            }
        }
    }

    ai_lines
}
