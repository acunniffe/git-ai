use crate::authorship::authorship_log::LineRange;
use crate::authorship::ignore::{build_ignore_matcher, should_ignore_file_with_matcher};
use crate::error::GitAiError;
use crate::git::notes_api::read_authorship;
use crate::git::repository::{Repository, exec_git};
use crate::mdm::spinner::Spinner;
use crate::utils::is_interactive_terminal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RECENT_COMMIT_WINDOW: Duration = Duration::from_secs(60);
const AUTHORSHIP_NOTE_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const AUTHORSHIP_NOTE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const WAIT_MESSAGE: &str = "Waiting for git-ai to process this commit";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolModelHeadlineStats {
    #[serde(default)]
    pub ai_additions: u32, // Number of lines committed with AI attribution
    #[serde(default)]
    pub ai_accepted: u32, // Number of AI-generated lines that were accepted by the user without any human edits
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommitStats {
    #[serde(default)]
    pub human_additions: u32, // Number of lines committed with human attribution
    #[serde(default)]
    pub unknown_additions: u32, // Number of lines with no attestation at all
    #[serde(default)]
    pub ai_additions: u32, // Number of lines committed with AI attribution
    #[serde(default)]
    pub ai_accepted: u32, // Number of AI-generated lines that were accepted by the user without any human edits
    #[serde(default)]
    pub git_diff_deleted_lines: u32,
    #[serde(default)]
    pub git_diff_added_lines: u32,
    #[serde(default)]
    pub tool_model_breakdown: BTreeMap<String, ToolModelHeadlineStats>,
}

pub fn stats_command(
    repo: &Repository,
    commit_sha: Option<&str>,
    json: bool,
    ignore_patterns: &[String],
) -> Result<(), GitAiError> {
    let (target, refname) = if let Some(sha) = commit_sha {
        // Validate that the commit exists using revparse_single
        match repo.revparse_single(sha) {
            Ok(commit_obj) => {
                // For a specific commit, we don't have a refname, so use the commit SHA
                let full_sha = commit_obj.id();
                (full_sha, sha.to_string())
            }
            Err(GitAiError::GitCliError { .. }) => {
                return Err(GitAiError::Generic(format!("No commit found: {}", sha)));
            }
            Err(e) => return Err(e),
        }
    } else {
        // Default behavior: use current HEAD
        let head = repo.head()?;
        let target = head.target()?;
        let name = head.name().unwrap_or("HEAD").to_string();
        (target, name)
    };

    tracing::debug!(
        "Stats command found commit: {} refname: {}",
        target,
        refname
    );

    let authorship_log = wait_for_recent_authorship(repo, &target)?;
    let stats = stats_for_commit_stats_with_authorship(
        repo,
        &target,
        ignore_patterns,
        authorship_log.as_ref(),
    )?;

    if json {
        let json_str = serde_json::to_string(&stats)?;
        println!("{}", json_str);
    } else {
        write_stats_to_terminal(&stats, true);
    }

    Ok(())
}

fn wait_for_recent_authorship(
    repo: &Repository,
    commit_sha: &str,
) -> Result<Option<crate::authorship::authorship_log_serialization::AuthorshipLog>, GitAiError> {
    if let Some(authorship_log) = read_authorship(repo, commit_sha) {
        return Ok(Some(authorship_log));
    }

    let commit_timestamp = commit_timestamp(repo, commit_sha)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            GitAiError::Generic(format!("System clock is before Unix epoch: {error}"))
        })?
        .as_secs();
    if now.abs_diff(commit_timestamp) > RECENT_COMMIT_WINDOW.as_secs() {
        return Ok(None);
    }

    let force_tty = std::env::var_os("GIT_AI_TEST_FORCE_TTY").is_some();
    let spinner = if force_tty {
        // Indicatif intentionally hides itself when stderr is captured. Keep the
        // existing test-only TTY override useful for subprocess assertions.
        eprintln!("{WAIT_MESSAGE}");
        None
    } else {
        is_interactive_terminal().then(|| Spinner::new(WAIT_MESSAGE))
    };
    let started = Instant::now();
    let authorship_log = loop {
        let remaining = AUTHORSHIP_NOTE_WAIT_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break None;
        }
        std::thread::sleep(AUTHORSHIP_NOTE_POLL_INTERVAL.min(remaining));
        if let Some(authorship_log) = read_authorship(repo, commit_sha) {
            break Some(authorship_log);
        }
    };
    if let Some(spinner) = spinner {
        spinner.finish_and_clear();
    }

    Ok(authorship_log)
}

fn commit_timestamp(repo: &Repository, commit_sha: &str) -> Result<u64, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "show".to_string(),
        "-s".to_string(),
        "--no-notes".to_string(),
        "--format=%ct".to_string(),
        commit_sha.to_string(),
    ]);
    let output = exec_git(&args)?;
    String::from_utf8(output.stdout)?
        .trim()
        .parse()
        .map_err(|error| {
            GitAiError::Generic(format!(
                "Invalid commit timestamp for {commit_sha}: {error}"
            ))
        })
}

pub fn write_stats_to_terminal(stats: &CommitStats, is_interactive: bool) -> String {
    let mut output = String::new();

    // Set maximum bar width to 40 characters
    let bar_width: usize = 40;

    // Handle deletion-only commits (no additions)
    if stats.git_diff_added_lines == 0 && stats.git_diff_deleted_lines > 0 {
        // Show gray bar for deletion-only commit
        let mut progress_bar = String::new();
        progress_bar.push_str("you  ");
        progress_bar.push_str("\x1b[90m"); // Gray color
        progress_bar.push_str(&" ".repeat(bar_width)); // Gray bar
        progress_bar.push_str("\x1b[0m"); // Reset color
        progress_bar.push_str(" ai");

        output.push_str(&progress_bar);
        output.push('\n');
        if is_interactive {
            println!("{}", progress_bar);
        }

        // Show "(no additions)" message below the bar
        let no_additions_msg = format!("     \x1b[90m{:^40}\x1b[0m", "(no additions)");
        output.push_str(&no_additions_msg);
        output.push('\n');
        if is_interactive {
            println!("{}", no_additions_msg);
        }
        // No percentage line or AI stats for deletion-only commits
        return output;
    }

    // Calculate total additions: known human + unknown (untracked) + AI
    let total_additions = stats.human_additions + stats.unknown_additions + stats.ai_additions;

    // (ai_additions == ai_accepted after mixed removal, so acceptance is always 100%)

    // Determine whether to show the untracked segment (raw float check, before rounding)
    let untracked_pct_raw = if total_additions > 0 {
        stats.unknown_additions as f64 / total_additions as f64 * 100.0
    } else {
        0.0
    };
    let show_untracked = untracked_pct_raw > 1.0;

    // Calculate human bar segment
    let human_bars = if total_additions > 0 {
        ((stats.human_additions as f64 / total_additions as f64) * bar_width as f64) as usize
    } else {
        0
    };

    // Ensure human contributions get at least 2 visible blocks if they have more than 1 line
    let min_human_bars = if stats.human_additions > 1 { 2 } else { 0 };
    let final_human_bars = human_bars.max(min_human_bars);

    // Distribute remaining width between untracked and AI proportionally.
    // When untracked is below the 1% threshold, all remaining width goes to AI.
    let remaining_width = bar_width.saturating_sub(final_human_bars);
    let (final_untracked_bars, final_ai_bars) = if show_untracked {
        let total_other = stats.unknown_additions + stats.ai_additions;
        let untracked_bars = if total_other > 0 {
            ((stats.unknown_additions as f64 / total_other as f64) * remaining_width as f64)
                as usize
        } else {
            0
        };
        (
            untracked_bars,
            remaining_width.saturating_sub(untracked_bars),
        )
    } else {
        (0, remaining_width)
    };

    // Build the progress bar
    let mut progress_bar = String::new();
    progress_bar.push_str("you  ");
    progress_bar.push_str(&"█".repeat(final_human_bars)); // known human (attested)
    progress_bar.push_str(&"·".repeat(final_untracked_bars)); // untracked (no attestation)
    progress_bar.push_str(&"░".repeat(final_ai_bars)); // AI
    progress_bar.push_str(" ai");

    // Calculate percentages for display
    let human_percentage = if total_additions > 0 {
        ((stats.human_additions as f64 / total_additions as f64) * 100.0).round() as u32
    } else {
        0
    };
    let ai_percentage = if total_additions > 0 {
        ((stats.ai_additions as f64 / total_additions as f64) * 100.0).round() as u32
    } else {
        0
    };

    // Print the stats
    output.push_str(&progress_bar);
    output.push('\n');
    if is_interactive {
        println!("{}", progress_bar);
    }

    // Percentage line: three anchors (human / untracked / AI) when untracked is visible,
    // two anchors (human / AI) otherwise.
    if show_untracked {
        let untracked_percentage = untracked_pct_raw.round() as u32;
        // When interactive, wrap "untracked" in an OSC 8 hyperlink so it is clickable in
        // supporting terminals (iTerm2, Warp, etc.). Spaces are constructed manually —
        // not via format-width padding on the label — so that invisible escape bytes do
        // not misalign the output.
        let untracked_label = if is_interactive {
            "\x1b]8;;https://usegitai.com/docs/cli/untracked\x1b\\\x1b[4muntracked\x1b[24m\x1b]8;;\x1b\\"
                .to_string()
        } else {
            "untracked".to_string()
        };
        let percentage_line = format!(
            "     {:<3}{:>10}{} {:>3}%{:>10}{:>3}%",
            format!("{}%", human_percentage),
            "",
            untracked_label,
            untracked_percentage,
            "",
            ai_percentage
        );
        output.push_str(&percentage_line);
        output.push('\n');
        if is_interactive {
            println!("{}", percentage_line);
        }
    } else {
        let percentage_line = format!(
            "     {:<3}{:>33}{:>3}%",
            format!("{}%", human_percentage),
            "",
            ai_percentage
        );
        output.push_str(&percentage_line);
        output.push('\n');
        if is_interactive {
            println!("{}", percentage_line);
        }
    }

    output
}

#[allow(dead_code)]
pub fn write_stats_to_markdown(stats: &CommitStats) -> String {
    let mut output = String::new();

    // Set maximum bar width to 20 characters
    let bar_width: usize = 20;

    // Handle deletion-only commits (no additions)
    if stats.git_diff_added_lines == 0 && stats.git_diff_deleted_lines > 0 {
        output.push_str("(no additions)");
        output.push('\n');
        return output;
    }

    // Calculate total additions for the progress bar
    let total_additions = stats.git_diff_added_lines;

    // Human additions: known-human attested + unattested
    let pure_human = stats.human_additions + stats.unknown_additions;
    // AI = AI lines accepted
    let pure_ai = stats.ai_accepted;

    // Calculate percentages for display
    let pure_human_percentage = if total_additions > 0 {
        ((pure_human as f64 / total_additions as f64) * 100.0).round() as u32
    } else {
        0
    };
    let ai_percentage = if total_additions > 0 {
        ((pure_ai as f64 / total_additions as f64) * 100.0).round() as u32
    } else {
        0
    };

    // Calculate bar sizes
    let pure_human_bars = if total_additions > 0 {
        let calculated =
            ((pure_human as f64 / total_additions as f64) * bar_width as f64).round() as usize;
        // Ensure at least 1 block if value > 0
        if pure_human > 0 && calculated == 0 {
            1
        } else {
            calculated
        }
    } else {
        0
    };

    let ai_bars = if total_additions > 0 {
        let calculated =
            ((pure_ai as f64 / total_additions as f64) * bar_width as f64).round() as usize;
        // Ensure at least 1 block if value > 0
        if pure_ai > 0 && calculated == 0 {
            1
        } else {
            calculated
        }
    } else {
        0
    };

    output.push_str("Stats powered by [Git AI](https://github.com/git-ai-project/git-ai)\n\n");
    // Build the fenced code block
    output.push_str("```text\n");

    // Human line: dark blocks for human, light blocks for rest
    output.push_str("🧠 you    ");
    output.push_str(&"█".repeat(pure_human_bars));
    output.push_str(&"░".repeat(bar_width.saturating_sub(pure_human_bars)));
    output.push_str(&format!("  {}%\n", pure_human_percentage));

    // AI line: light blocks for non-ai, dark blocks for ai
    output.push_str("🤖 ai     ");
    output.push_str(&"░".repeat(bar_width.saturating_sub(ai_bars)));
    output.push_str(&"█".repeat(ai_bars));
    output.push_str(&format!("  {}%\n", ai_percentage));

    output.push_str("```");

    // Add details section
    output.push_str("\n\n<details>\n");
    output.push_str("<summary>More stats</summary>\n\n");

    // Find top model by accepted lines
    if !stats.tool_model_breakdown.is_empty()
        && let Some((model_name, model_stats)) = stats
            .tool_model_breakdown
            .iter()
            .max_by_key(|(_, stats)| stats.ai_accepted)
    {
        output.push_str(&format!(
            "- Top model: {} ({} accepted lines)\n",
            model_name, model_stats.ai_accepted
        ));
    }

    output.push_str("\n</details>");

    output
}

/// Calculate commit stats from an authorship log
/// This helper can work with both fetched and in-memory authorship logs
pub fn stats_from_authorship_log(
    _authorship_log: Option<&crate::authorship::authorship_log_serialization::AuthorshipLog>,
    git_diff_added_lines: u32,
    git_diff_deleted_lines: u32,
    ai_accepted: u32,
    known_human_accepted: u32,
    ai_accepted_by_tool: &BTreeMap<String, u32>,
) -> CommitStats {
    let mut commit_stats = CommitStats {
        human_additions: 0,
        unknown_additions: 0,
        ai_additions: 0,
        ai_accepted,
        tool_model_breakdown: BTreeMap::new(),
        git_diff_deleted_lines,
        git_diff_added_lines,
    };

    // Update tool-level accepted counts using diff-based attribution.
    for (tool_model, accepted) in ai_accepted_by_tool {
        let tool_stats = commit_stats
            .tool_model_breakdown
            .entry(tool_model.clone())
            .or_default();
        tool_stats.ai_accepted = *accepted;
    }

    // AI additions = ai_accepted (no mixed component)
    commit_stats.ai_additions = commit_stats.ai_accepted;

    // Set ai_additions for each tool: ai_additions = ai_accepted
    for tool_stats in commit_stats.tool_model_breakdown.values_mut() {
        tool_stats.ai_additions = tool_stats.ai_accepted;
    }

    // KnownHuman-attested additions (positively identified as human-authored)
    commit_stats.human_additions = known_human_accepted;

    // Unknown additions: lines with no attestation at all (not AI-accepted, not KnownHuman)
    commit_stats.unknown_additions = git_diff_added_lines
        .saturating_sub(commit_stats.ai_accepted)
        .saturating_sub(known_human_accepted);

    commit_stats
}

pub fn stats_for_commit_stats(
    repo: &Repository,
    commit_sha: &str,
    ignore_patterns: &[String],
) -> Result<CommitStats, GitAiError> {
    let authorship_log = read_authorship(repo, commit_sha);
    stats_for_commit_stats_with_authorship(
        repo,
        commit_sha,
        ignore_patterns,
        authorship_log.as_ref(),
    )
}

pub fn stats_for_commit_stats_with_authorship(
    repo: &Repository,
    commit_sha: &str,
    ignore_patterns: &[String],
    authorship_log: Option<&crate::authorship::authorship_log_serialization::AuthorshipLog>,
) -> Result<CommitStats, GitAiError> {
    let commit_obj = repo.revparse_single(commit_sha)?.peel_to_commit()?;
    let parent_count = commit_obj.parent_count()?;

    if parent_count > 1 {
        return stats_for_commit_stats_from_hunks(
            repo,
            commit_sha,
            ignore_patterns,
            &[],
            authorship_log,
        );
    }

    let parent_sha = if parent_count == 0 {
        None
    } else {
        Some(commit_obj.parent(0)?.id())
    };

    stats_for_commit_stats_with_parent_and_authorship(
        repo,
        commit_sha,
        parent_sha.as_deref(),
        ignore_patterns,
        authorship_log,
    )
}

pub fn stats_for_commit_stats_with_parent_and_authorship(
    repo: &Repository,
    commit_sha: &str,
    parent_sha: Option<&str>,
    ignore_patterns: &[String],
    authorship_log: Option<&crate::authorship::authorship_log_serialization::AuthorshipLog>,
) -> Result<CommitStats, GitAiError> {
    use crate::commands::diff::get_diff_with_line_numbers;

    let from_ref = parent_sha.unwrap_or("4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    let hunks = get_diff_with_line_numbers(repo, from_ref, commit_sha)?;
    stats_for_commit_stats_from_hunks(repo, commit_sha, ignore_patterns, &hunks, authorship_log)
}

#[doc(hidden)]
pub fn accepted_lines_from_attestations(
    authorship_log: Option<&crate::authorship::authorship_log_serialization::AuthorshipLog>,
    added_lines_by_file: &HashMap<String, Vec<u32>>,
    is_merge_commit: bool,
) -> (u32, u32, BTreeMap<String, u32>) {
    // returns (ai_accepted, known_human_accepted, per_tool_model)
    if is_merge_commit {
        return (0, 0, BTreeMap::new());
    }

    let mut total_ai_accepted = 0u32;
    let mut known_human_accepted = 0u32;
    let mut per_tool_model = BTreeMap::new();

    let Some(log) = authorship_log else {
        return (0, 0, per_tool_model);
    };

    for file_attestation in &log.attestations {
        let Some(added_lines) = added_lines_by_file.get(&file_attestation.file_path) else {
            continue;
        };

        for entry in &file_attestation.entries {
            // KnownHuman entries (h_ prefix): count as known-human-attested lines.
            if entry.hash.starts_with("h_") {
                let accepted = entry
                    .line_ranges
                    .iter()
                    .map(|line_range| line_range_overlap_len(line_range, added_lines))
                    .sum::<u32>();
                if accepted > 0 {
                    known_human_accepted += accepted;
                }
                continue;
            }

            let accepted = entry
                .line_ranges
                .iter()
                .map(|line_range| line_range_overlap_len(line_range, added_lines))
                .sum::<u32>();

            if accepted == 0 {
                continue;
            }

            total_ai_accepted += accepted;

            // Session entries (s_ prefix): look up in sessions map
            if entry.hash.starts_with("s_") {
                let session_key = entry.hash.split("::").next().unwrap_or(&entry.hash);
                if let Some(session_record) = log.metadata.sessions.get(session_key) {
                    let tool_model = format!(
                        "{}::{}",
                        session_record.agent_id.tool, session_record.agent_id.model
                    );
                    *per_tool_model.entry(tool_model).or_insert(0) += accepted;
                }
            } else if let Some(prompt_record) = log.metadata.prompts.get(&entry.hash) {
                let tool_model = format!(
                    "{}::{}",
                    prompt_record.agent_id.tool, prompt_record.agent_id.model
                );
                *per_tool_model.entry(tool_model).or_insert(0) += accepted;
            }
        }
    }

    (total_ai_accepted, known_human_accepted, per_tool_model)
}

#[doc(hidden)]
pub fn line_range_overlap_len(range: &LineRange, added_lines: &[u32]) -> u32 {
    match range {
        LineRange::Single(line) => u32::from(added_lines.binary_search(line).is_ok()),
        LineRange::Range(start, end) => {
            let start_idx = added_lines.partition_point(|line| *line < *start);
            let end_idx = added_lines.partition_point(|line| *line <= *end);
            end_idx.saturating_sub(start_idx) as u32
        }
    }
}

/// Like `stats_for_commit_stats` but accepts pre-computed diff hunks and authorship log,
/// avoiding redundant git subprocess calls in the post-commit hook path.
pub fn stats_for_commit_stats_from_hunks(
    repo: &Repository,
    commit_sha: &str,
    ignore_patterns: &[String],
    hunks: &[crate::commands::diff::DiffHunk],
    authorship_log: Option<&crate::authorship::authorship_log_serialization::AuthorshipLog>,
) -> Result<CommitStats, GitAiError> {
    let commit_obj = repo.revparse_single(commit_sha)?.peel_to_commit()?;
    let parent_count = commit_obj.parent_count()?;
    let is_merge_commit = parent_count > 1;

    Ok(stats_for_commit_stats_from_hunks_with_merge_flag(
        ignore_patterns,
        hunks,
        authorship_log,
        is_merge_commit,
    ))
}

pub(crate) fn stats_for_commit_stats_from_hunks_with_merge_flag(
    ignore_patterns: &[String],
    hunks: &[crate::commands::diff::DiffHunk],
    authorship_log: Option<&crate::authorship::authorship_log_serialization::AuthorshipLog>,
    is_merge_commit: bool,
) -> CommitStats {
    let ignore_matcher = build_ignore_matcher(ignore_patterns);

    let mut git_diff_added_lines = 0u32;
    let mut git_diff_deleted_lines = 0u32;
    let mut added_lines_by_file: HashMap<String, Vec<u32>> = HashMap::new();

    for hunk in hunks {
        if should_ignore_file_with_matcher(&hunk.file_path, &ignore_matcher) {
            continue;
        }
        git_diff_added_lines += hunk.added_lines.len() as u32;
        git_diff_deleted_lines += hunk.deleted_lines.len() as u32;

        if !is_merge_commit && !hunk.added_lines.is_empty() {
            added_lines_by_file
                .entry(hunk.file_path.clone())
                .or_default()
                .extend(hunk.added_lines.iter().copied());
        }
    }

    for lines in added_lines_by_file.values_mut() {
        lines.sort_unstable();
        lines.dedup();
    }

    let (ai_accepted, known_human_accepted, ai_accepted_by_tool) =
        accepted_lines_from_attestations(authorship_log, &added_lines_by_file, is_merge_commit);

    stats_from_authorship_log(
        authorship_log,
        git_diff_added_lines,
        git_diff_deleted_lines,
        ai_accepted,
        known_human_accepted,
        &ai_accepted_by_tool,
    )
}

/// Get git diff statistics between commit and its parent
/// Uses the same diff engine as git ai diff to properly handle renames
pub fn get_git_diff_stats(
    repo: &Repository,
    commit_sha: &str,
    ignore_patterns: &[String],
) -> Result<(u32, u32), GitAiError> {
    use crate::commands::diff::get_diff_with_line_numbers;

    let commit_obj = repo.revparse_single(commit_sha)?.peel_to_commit()?;
    let parent_count = commit_obj.parent_count()?;

    // For merge commits, return (0, 0) to match the behavior of `git show --numstat`
    // which shows a combined diff (typically 0 lines for clean merges)
    if parent_count > 1 {
        return Ok((0, 0));
    }

    let from_ref = if parent_count == 0 {
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()
    } else {
        commit_obj.parent(0)?.id()
    };

    // Use the diff engine which properly handles renames with --find-renames=1%
    let hunks = get_diff_with_line_numbers(repo, &from_ref, commit_sha)?;

    let ignore_matcher = build_ignore_matcher(ignore_patterns);
    let mut added_lines = 0u32;
    let mut deleted_lines = 0u32;

    for hunk in hunks {
        if should_ignore_file_with_matcher(&hunk.file_path, &ignore_matcher) {
            continue;
        }
        added_lines += hunk.added_lines.len() as u32;
        deleted_lines += hunk.deleted_lines.len() as u32;
    }

    Ok((added_lines, deleted_lines))
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_debug_snapshot;

    #[test]
    fn test_terminal_stats_display() {
        // Test with mixed human/AI stats
        let stats = CommitStats {
            human_additions: 50,
            unknown_additions: 0,
            ai_additions: 100,
            ai_accepted: 25,
            git_diff_deleted_lines: 15,
            git_diff_added_lines: 80,
            tool_model_breakdown: BTreeMap::new(),
        };

        let mixed_output = write_stats_to_terminal(&stats, false);
        assert_debug_snapshot!(mixed_output);

        // Test with AI-only stats
        let ai_stats = CommitStats {
            human_additions: 0,
            unknown_additions: 0,
            ai_additions: 100,
            ai_accepted: 95,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 100,
            tool_model_breakdown: BTreeMap::new(),
        };

        let ai_only_output = write_stats_to_terminal(&ai_stats, false);
        assert_debug_snapshot!(ai_only_output);

        // Test with human-only stats
        let human_stats = CommitStats {
            human_additions: 75,
            unknown_additions: 0,
            ai_additions: 0,
            ai_accepted: 0,
            git_diff_deleted_lines: 10,
            git_diff_added_lines: 75,
            tool_model_breakdown: BTreeMap::new(),
        };

        let human_only_output = write_stats_to_terminal(&human_stats, false);
        assert_debug_snapshot!(human_only_output);

        // Test with minimal human contribution (should get at least 2 blocks)
        let minimal_human_stats = CommitStats {
            human_additions: 2,
            unknown_additions: 0,
            ai_additions: 100,
            ai_accepted: 95,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 102,
            tool_model_breakdown: BTreeMap::new(),
        };

        let minimal_human_output = write_stats_to_terminal(&minimal_human_stats, false);
        assert_debug_snapshot!(minimal_human_output);

        // Test with deletion-only commit (no additions)
        let deletion_only_stats = CommitStats {
            human_additions: 0,
            unknown_additions: 0,
            ai_additions: 0,
            ai_accepted: 0,
            git_diff_deleted_lines: 25,
            git_diff_added_lines: 0,
            tool_model_breakdown: BTreeMap::new(),
        };

        let deletion_only_output = write_stats_to_terminal(&deletion_only_stats, false);
        assert_debug_snapshot!(deletion_only_output);

        // --- New test cases for untracked segment ---

        // 18% human / 22% untracked / 60% AI — matches the design example
        let untracked_stats = CommitStats {
            human_additions: 180,
            unknown_additions: 220,
            ai_additions: 600,
            ai_accepted: 462,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 1000,
            tool_model_breakdown: BTreeMap::new(),
        };
        let with_untracked_output = write_stats_to_terminal(&untracked_stats, false);
        assert_debug_snapshot!(with_untracked_output);

        // untracked exactly at the 1% threshold — should NOT show untracked segment
        let threshold_stats = CommitStats {
            human_additions: 49,
            unknown_additions: 1,
            ai_additions: 50,
            ai_accepted: 50,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 100,
            tool_model_breakdown: BTreeMap::new(),
        };
        let untracked_at_threshold_output = write_stats_to_terminal(&threshold_stats, false);
        assert_debug_snapshot!(untracked_at_threshold_output);

        // untracked just above 1% threshold (~2%) — should show untracked segment
        let above_threshold_stats = CommitStats {
            human_additions: 97,
            unknown_additions: 2,
            ai_additions: 0,
            ai_accepted: 0,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 99,
            tool_model_breakdown: BTreeMap::new(),
        };
        let untracked_just_above_output = write_stats_to_terminal(&above_threshold_stats, false);
        assert_debug_snapshot!(untracked_just_above_output);

        // 100% untracked — entire bar is · chars
        let all_untracked_stats = CommitStats {
            human_additions: 0,
            unknown_additions: 100,
            ai_additions: 0,
            ai_accepted: 0,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 100,
            tool_model_breakdown: BTreeMap::new(),
        };
        let all_untracked_output = write_stats_to_terminal(&all_untracked_stats, false);
        assert_debug_snapshot!(all_untracked_output);

        // OSC 8 hyperlink emitted when is_interactive = true
        // Not a snapshot test — asserts presence of the escape sequence directly.
        let hyperlink_output = write_stats_to_terminal(&untracked_stats, true);
        assert!(
            hyperlink_output.contains("\x1b]8;;https://usegitai.com/docs/cli/untracked\x1b\\"),
            "Expected OSC 8 hyperlink in interactive output, got: {:?}",
            hyperlink_output
        );
        assert!(
            hyperlink_output.contains("untracked"),
            "Expected 'untracked' label in interactive output"
        );
    }

    #[test]
    fn test_markdown_stats_display() {
        // Test with mixed human/AI stats
        let stats = CommitStats {
            human_additions: 50,
            unknown_additions: 0,
            ai_additions: 100,
            ai_accepted: 25,
            git_diff_deleted_lines: 15,
            git_diff_added_lines: 80,
            tool_model_breakdown: BTreeMap::new(),
        };

        let mixed_output = write_stats_to_markdown(&stats);
        assert_debug_snapshot!(mixed_output);

        // Test with AI-only stats
        let ai_stats = CommitStats {
            human_additions: 0,
            unknown_additions: 0,
            ai_additions: 100,
            ai_accepted: 95,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 100,
            tool_model_breakdown: BTreeMap::new(),
        };

        let ai_only_output = write_stats_to_markdown(&ai_stats);
        assert_debug_snapshot!(ai_only_output);

        // Test with human-only stats
        let human_stats = CommitStats {
            human_additions: 75,
            unknown_additions: 0,
            ai_additions: 0,
            ai_accepted: 0,
            git_diff_deleted_lines: 10,
            git_diff_added_lines: 75,
            tool_model_breakdown: BTreeMap::new(),
        };

        let human_only_output = write_stats_to_markdown(&human_stats);
        assert_debug_snapshot!(human_only_output);

        // Test with minimal human contribution (should get at least 2 blocks)
        let minimal_human_stats = CommitStats {
            human_additions: 2,
            unknown_additions: 0,
            ai_additions: 100,
            ai_accepted: 95,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 102,
            tool_model_breakdown: BTreeMap::new(),
        };

        let minimal_human_output = write_stats_to_markdown(&minimal_human_stats);
        assert_debug_snapshot!(minimal_human_output);

        // Test with deletion-only commit (no additions)
        let deletion_only_stats = CommitStats {
            human_additions: 0,
            unknown_additions: 0,
            ai_additions: 0,
            ai_accepted: 0,
            git_diff_deleted_lines: 25,
            git_diff_added_lines: 0,
            tool_model_breakdown: BTreeMap::new(),
        };

        let deletion_only_output = write_stats_to_markdown(&deletion_only_stats);
        assert_debug_snapshot!(deletion_only_output);
    }
}
