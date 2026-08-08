use std::collections::{HashMap, HashSet};

use crate::error::GitAiError;
use crate::operations::git::repository::Repository;

use super::GitAiBlameOptions;

pub(super) fn output_porcelain_format(
    repo: &Repository,
    _line_authors: &HashMap<u32, String>,
    file_path: &str,
    lines: &[&str],
    line_ranges: &[(u32, u32)],
    options: &GitAiBlameOptions,
    commits_with_notes: &HashSet<String>,
) -> Result<(), GitAiError> {
    let prepared = repo.prepare_blame_render(file_path, line_ranges, options)?;

    let mut last_hunk_id = None;
    let mut commit_summaries: HashMap<String, String> = HashMap::new();
    let mut seen_commits: HashSet<String> = HashSet::new();
    for line_num in prepared.requested_lines {
        let line_index = (line_num - 1) as usize;
        let line_content = if line_index < lines.len() {
            lines[line_index]
        } else {
            ""
        };

        if let Some(hunk) = prepared.line_to_hunk.get(&line_num) {
            // For agent-detected commits (email matches known agent, no authorship note),
            // override the author name with the tool name. Otherwise use git's original author.
            // Only apply agent detection when no real authorship note exists for this commit.
            let author_name = if !commits_with_notes.contains(&hunk.commit_sha) {
                crate::operations::authorship::agent_detection::match_email_to_agent(
                    &hunk.author_email,
                )
                .map(|t| t.to_string())
            } else {
                None
            };
            let author_name = author_name.as_deref().unwrap_or(&hunk.original_author);
            let commit_sha = &hunk.commit_sha;
            let author_email = &hunk.author_email;
            let author_time = hunk.author_time;
            let author_tz = &hunk.author_tz;
            let committer_name = &hunk.committer;
            let committer_email = &hunk.committer_email;
            let committer_time = hunk.committer_time;
            let committer_tz = &hunk.committer_tz;
            let boundary = hunk.is_boundary;
            let filename = file_path;

            let hunk_id = (commit_sha.clone(), hunk.range.0);
            if options.line_porcelain {
                let summary = if let Some(summary) = commit_summaries.get(commit_sha) {
                    summary.clone()
                } else {
                    let commit = repo.find_commit(commit_sha.clone())?;
                    let summary = commit.summary()?;
                    commit_summaries.insert(commit_sha.clone(), summary.clone());
                    summary
                };
                if last_hunk_id.as_ref() != Some(&hunk_id) {
                    // First line of hunk: 4-field header
                    println!(
                        "{} {} {} {}",
                        commit_sha,
                        line_num,
                        line_num,
                        hunk.range.1 - hunk.range.0 + 1
                    );
                    last_hunk_id = Some(hunk_id);
                } else {
                    // Subsequent lines: 3-field header
                    println!("{} {} {}", commit_sha, line_num, line_num);
                }
                println!("author {}", author_name);
                println!("author-mail <{}>", author_email);
                println!("author-time {}", author_time);
                println!("author-tz {}", author_tz);
                println!("committer {}", committer_name);
                println!("committer-mail <{}>", committer_email);
                println!("committer-time {}", committer_time);
                println!("committer-tz {}", committer_tz);
                println!("summary {}", summary);
                if boundary {
                    println!("boundary");
                }
                println!("filename {}", filename);
                println!("\t{}", line_content);
            } else if options.porcelain {
                if last_hunk_id.as_ref() != Some(&hunk_id) {
                    // First line of hunk.
                    println!(
                        "{} {} {} {}",
                        commit_sha,
                        line_num,
                        line_num,
                        hunk.range.1 - hunk.range.0 + 1
                    );
                    if !seen_commits.contains(commit_sha) {
                        let summary = if let Some(summary) = commit_summaries.get(commit_sha) {
                            summary.clone()
                        } else {
                            let commit = repo.find_commit(commit_sha.clone())?;
                            let summary = commit.summary()?;
                            commit_summaries.insert(commit_sha.clone(), summary.clone());
                            summary
                        };
                        println!("author {}", author_name);
                        println!("author-mail <{}>", author_email);
                        println!("author-time {}", author_time);
                        println!("author-tz {}", author_tz);
                        println!("committer {}", committer_name);
                        println!("committer-mail <{}>", committer_email);
                        println!("committer-time {}", committer_time);
                        println!("committer-tz {}", committer_tz);
                        println!("summary {}", summary);
                        if boundary {
                            println!("boundary");
                        }
                        println!("filename {}", filename);
                        seen_commits.insert(commit_sha.clone());
                    }
                    println!("\t{}", line_content);
                    last_hunk_id = Some(hunk_id);
                } else {
                    // For subsequent lines, print only the header and content (no metadata block)
                    println!("{} {} {}", commit_sha, line_num, line_num);
                    println!("\t{}", line_content);
                }
            }
        }
    }
    Ok(())
}

pub(super) fn output_incremental_format(
    repo: &Repository,
    _line_authors: &HashMap<u32, String>,
    file_path: &str,
    _lines: &[&str],
    line_ranges: &[(u32, u32)],
    options: &GitAiBlameOptions,
    commits_with_notes: &HashSet<String>,
) -> Result<(), GitAiError> {
    let prepared = repo.prepare_blame_render(file_path, line_ranges, options)?;

    let mut last_hunk_id = None;
    let mut commit_summaries: HashMap<String, String> = HashMap::new();
    let mut seen_commits: HashSet<String> = HashSet::new();
    for line_num in prepared.requested_lines {
        if let Some(hunk) = prepared.line_to_hunk.get(&line_num) {
            // For agent-detected commits (email matches known agent, no authorship note),
            // override the author name with the tool name. Otherwise use git's original author.
            // Only apply agent detection when no real authorship note exists for this commit.
            let author_name = if !commits_with_notes.contains(&hunk.commit_sha) {
                crate::operations::authorship::agent_detection::match_email_to_agent(
                    &hunk.author_email,
                )
                .map(|t| t.to_string())
            } else {
                None
            };
            let author_name = author_name.as_deref().unwrap_or(&hunk.original_author);
            let commit_sha = &hunk.commit_sha;
            let author_email = &hunk.author_email;
            let author_time = hunk.author_time;
            let author_tz = &hunk.author_tz;
            let committer_name = &hunk.committer;
            let committer_email = &hunk.committer_email;
            let committer_time = hunk.committer_time;
            let committer_tz = &hunk.committer_tz;

            // Only print the full block for the first line of a hunk
            let hunk_id = (hunk.commit_sha.clone(), hunk.range.0);
            if last_hunk_id.as_ref() != Some(&hunk_id) {
                // Print first line for this hunk.
                println!(
                    "{} {} {} {}",
                    commit_sha,
                    line_num,
                    line_num,
                    hunk.range.1 - hunk.range.0 + 1
                );
                if !seen_commits.contains(commit_sha) {
                    let summary = if let Some(summary) = commit_summaries.get(commit_sha) {
                        summary.clone()
                    } else {
                        let commit = repo.find_commit(commit_sha.clone())?;
                        let summary = commit.summary()?;
                        commit_summaries.insert(commit_sha.clone(), summary.clone());
                        summary
                    };
                    println!("author {}", author_name);
                    println!("author-mail <{}>", author_email);
                    println!("author-time {}", author_time);
                    println!("author-tz {}", author_tz);
                    println!("committer {}", committer_name);
                    println!("committer-mail <{}>", committer_email);
                    println!("committer-time {}", committer_time);
                    println!("committer-tz {}", committer_tz);
                    println!("summary {}", summary);
                    if hunk.is_boundary {
                        println!("boundary");
                    }
                    seen_commits.insert(commit_sha.clone());
                }
                println!("filename {}", file_path);
                last_hunk_id = Some(hunk_id);
            }
            // For incremental, no content lines (no \tLine)
        } else {
            // Fallback for lines without blame info
            println!(
                "0000000000000000000000000000000000000000 {} {} 1",
                line_num, line_num
            );
            println!("author unknown");
            println!("author-mail <unknown@example.com>");
            println!("author-time 0");
            println!("author-tz +0000");
            println!("committer unknown");
            println!("committer-mail <unknown@example.com>");
            println!("committer-time 0");
            println!("committer-tz +0000");
            println!("summary unknown");
            println!("filename {}", file_path);
        }
    }
    Ok(())
}
