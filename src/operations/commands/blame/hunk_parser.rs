use std::collections::{HashMap, HashSet};

use crate::clients::git_cli::{exec_git, exec_git_stdin};
use crate::error::GitAiError;
use crate::operations::git::repository::Repository;

use super::{BlameHunk, GitAiBlameOptions};

/// Batch size for git rev-parse --short calls used to resolve abbreviated SHAs.
pub const BLAME_ABBREV_BATCH_SIZE: usize = 256;

pub(super) struct BlameRenderPreparation {
    pub(super) hunks: Vec<BlameHunk>,
    pub(super) line_to_hunk: HashMap<u32, BlameHunk>,
    pub(super) requested_lines: Vec<u32>,
}

fn prepare_blame_hunks(hunks: Vec<BlameHunk>) -> BlameRenderPreparation {
    let mut line_to_hunk = HashMap::new();
    for hunk in &hunks {
        for line_num in hunk.range.0..=hunk.range.1 {
            line_to_hunk.insert(line_num, hunk.clone());
        }
    }
    let mut requested_lines = line_to_hunk.keys().copied().collect::<Vec<_>>();
    requested_lines.sort_unstable();
    BlameRenderPreparation {
        hunks,
        line_to_hunk,
        requested_lines,
    }
}

impl Repository {
    pub fn blame_hunks(
        &self,
        file_path: &str,
        start_line: u32,
        end_line: u32,
        options: &GitAiBlameOptions,
    ) -> Result<Vec<BlameHunk>, GitAiError> {
        self.blame_hunks_for_ranges(file_path, &[(start_line, end_line)], options)
    }

    pub fn blame_hunks_for_ranges(
        &self,
        file_path: &str,
        line_ranges: &[(u32, u32)],
        options: &GitAiBlameOptions,
    ) -> Result<Vec<BlameHunk>, GitAiError> {
        if line_ranges.is_empty() {
            return Ok(Vec::new());
        }

        // Build git blame --line-porcelain command
        let mut args = self.global_args_for_exec();
        args.push("blame".to_string());
        args.push("--line-porcelain".to_string());

        // Ignore whitespace option
        if options.ignore_whitespace {
            args.push("-w".to_string());
        }

        // Detect lines moved within a file (-M) and copied from other files (-C, implies -M).
        // Needed so that lines shifted by an adjacent insertion/deletion are traced back to the
        // commit that originally wrote them rather than the commit that moved them.
        if options.detect_moves {
            args.push("-M".to_string());
        }
        for _ in 0..options.detect_copies {
            args.push("-C".to_string());
        }

        // Respect ignore options in use
        for rev in &options.ignore_revs {
            args.push("--ignore-rev".to_string());
            args.push(rev.clone());
        }
        if let Some(file) = &options.ignore_revs_file {
            args.push("--ignore-revs-file".to_string());
            args.push(file.clone());
        }

        // Limit to the specified ranges (git blame supports multiple -L flags).
        for (start_line, end_line) in line_ranges {
            args.push("-L".to_string());
            args.push(format!("{},{}", start_line, end_line));
        }

        // Add --since flag if oldest_date is specified
        // This controls the absolute lower bound of how far back to look
        if let Some(ref date_spec) = options.oldest_date_spec {
            args.push("--since".to_string());
            args.push(date_spec.clone());
        } else if let Some(ref date) = options.oldest_date {
            args.push("--since".to_string());
            args.push(date.to_rfc3339());
        }

        // Support newest_commit option (equivalent to libgit2's newest_commit)
        // This limits blame to only consider commits up to and including the specified commit
        // When oldest_commit is also set, we use a range: oldest_commit..newest_commit
        match (&options.oldest_commit, &options.newest_commit) {
            (Some(oldest), Some(newest)) => {
                // Use range format: git blame START_COMMIT..END_COMMIT -- file.txt
                args.push(format!("{}..{}", oldest, newest));
            }
            (None, Some(newest)) => {
                // Only newest_commit set, use it as the commit to blame at
                args.push(newest.clone());
            }
            (Some(_oldest), None) => {
                // oldest_commit without newest_commit doesn't make sense for blame
                // Just ignore oldest_commit in this case
            }
            (None, None) => {
                // No commit specified, blame at HEAD (default)
            }
        }

        // Add --contents flag if we have content data to pass via stdin
        if options.contents_data.is_some() {
            args.push("--contents".to_string());
            args.push("-".to_string());
        }

        args.push("--".to_string());
        args.push(file_path.to_string());

        // Execute git blame, using stdin if we have contents data
        let output = if let Some(ref data) = options.contents_data {
            exec_git_stdin(&args, data)?
        } else {
            exec_git(&args)?
        };
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

        let mut hunks = parse_porcelain_blame_output(&stdout, file_path);

        self.populate_hunk_abbrev_shas(&mut hunks, options);

        // Post-process hunks to populate ai_human_author from authorship logs
        let hunks = self.populate_ai_human_authors(hunks, file_path, options)?;

        Ok(hunks)
    }

    pub(super) fn prepare_blame_render(
        &self,
        file_path: &str,
        line_ranges: &[(u32, u32)],
        options: &GitAiBlameOptions,
    ) -> Result<BlameRenderPreparation, GitAiError> {
        let mut no_split_options = options.clone();
        no_split_options.split_hunks_by_ai_author = false;
        self.blame_hunks_for_ranges(file_path, line_ranges, &no_split_options)
            .map(prepare_blame_hunks)
    }

    pub(super) fn blame_requested_abbrev_len(
        options: &GitAiBlameOptions,
        is_boundary: bool,
    ) -> usize {
        let base_len = options.abbrev.unwrap_or(7).max(1) as usize;
        if is_boundary && !options.show_root {
            base_len
        } else {
            (base_len + 1).min(40)
        }
    }

    fn fallback_blame_abbrev_sha(commit_sha: &str, requested_len: usize) -> String {
        if requested_len < commit_sha.len() {
            commit_sha[..requested_len].to_string()
        } else {
            commit_sha.to_string()
        }
    }

    pub(super) fn resolve_blame_abbrev_shas_batched(
        &self,
        requests_by_len: &HashMap<usize, Vec<String>>,
    ) -> HashMap<(String, usize), String> {
        let mut resolved: HashMap<(String, usize), String> = HashMap::new();

        for (&requested_len, commit_shas) in requests_by_len {
            if commit_shas.is_empty() {
                continue;
            }

            for commit_sha_batch in commit_shas.chunks(BLAME_ABBREV_BATCH_SIZE) {
                let mut args = self.global_args_for_exec();
                args.push("rev-parse".to_string());
                args.push(format!("--short={requested_len}"));
                args.extend(commit_sha_batch.iter().cloned());

                let batched_result = exec_git(&args)
                    .ok()
                    .and_then(|output| String::from_utf8(output.stdout).ok())
                    .map(|stdout| {
                        stdout
                            .lines()
                            .map(str::trim)
                            .filter(|line| !line.is_empty())
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    });

                if let Some(short_shas) = batched_result
                    && short_shas.len() == commit_sha_batch.len()
                {
                    for (commit_sha, short_sha) in commit_sha_batch.iter().zip(short_shas) {
                        resolved.insert((commit_sha.clone(), requested_len), short_sha);
                    }
                    continue;
                }

                for commit_sha in commit_sha_batch {
                    resolved
                        .entry((commit_sha.clone(), requested_len))
                        .or_insert_with(|| {
                            Self::fallback_blame_abbrev_sha(commit_sha, requested_len)
                        });
                }
            }
        }

        resolved
    }

    pub(super) fn populate_hunk_abbrev_shas(
        &self,
        hunks: &mut [BlameHunk],
        options: &GitAiBlameOptions,
    ) {
        if options.long_rev {
            for hunk in hunks {
                hunk.abbrev_sha = hunk.commit_sha.clone();
            }
            return;
        }

        let mut requests_by_len: HashMap<usize, Vec<String>> = HashMap::new();
        let mut seen_by_len: HashMap<usize, HashSet<String>> = HashMap::new();

        for hunk in hunks.iter() {
            let requested_len = Self::blame_requested_abbrev_len(options, hunk.is_boundary);
            let seen = seen_by_len.entry(requested_len).or_default();
            if seen.insert(hunk.commit_sha.clone()) {
                requests_by_len
                    .entry(requested_len)
                    .or_default()
                    .push(hunk.commit_sha.clone());
            }
        }

        let resolved = self.resolve_blame_abbrev_shas_batched(&requests_by_len);

        for hunk in hunks.iter_mut() {
            let requested_len = Self::blame_requested_abbrev_len(options, hunk.is_boundary);
            hunk.abbrev_sha = resolved
                .get(&(hunk.commit_sha.clone(), requested_len))
                .cloned()
                .unwrap_or_else(|| {
                    Self::fallback_blame_abbrev_sha(&hunk.commit_sha, requested_len)
                });
        }
    }
}

/// Parse git blame --line-porcelain output into a list of hunks.
fn parse_porcelain_blame_output(stdout: &str, file_path: &str) -> Vec<BlameHunk> {
    // Parser state for current hunk
    #[derive(Default)]
    struct CurMeta {
        author: String,
        author_mail: String,
        author_time: i64,
        author_tz: String,
        committer: String,
        committer_mail: String,
        committer_time: i64,
        committer_tz: String,
        boundary: bool,
        filename: Option<String>,
    }

    let mut hunks: Vec<BlameHunk> = Vec::new();
    let mut cur_commit: Option<String> = None;
    let mut cur_final_start: u32 = 0;
    let mut cur_orig_start: u32 = 0;
    let mut cur_group_size: u32 = 0;
    let mut cur_meta = CurMeta::default();

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }

        if line.starts_with('\t') {
            // Content line; nothing to do, boundaries are driven by headers
            continue;
        }

        // Metadata lines
        if let Some(rest) = line.strip_prefix("author ") {
            cur_meta.author = rest.to_string();
            continue;
        }
        if let Some(rest) = line.strip_prefix("author-mail ") {
            // Usually in form: <mail>
            cur_meta.author_mail = rest
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string();
            continue;
        }
        if let Some(rest) = line.strip_prefix("author-time ") {
            if let Ok(t) = rest.trim().parse::<i64>() {
                cur_meta.author_time = t;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("author-tz ") {
            cur_meta.author_tz = rest.trim().to_string();
            continue;
        }
        if let Some(rest) = line.strip_prefix("committer ") {
            cur_meta.committer = rest.to_string();
            continue;
        }
        if let Some(rest) = line.strip_prefix("committer-mail ") {
            cur_meta.committer_mail = rest
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string();
            continue;
        }
        if let Some(rest) = line.strip_prefix("committer-time ") {
            if let Ok(t) = rest.trim().parse::<i64>() {
                cur_meta.committer_time = t;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("committer-tz ") {
            cur_meta.committer_tz = rest.trim().to_string();
            continue;
        }
        if line == "boundary" {
            cur_meta.boundary = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("filename ") {
            cur_meta.filename = Some(crate::operations::git::path_format::unescape_git_path(rest));
            continue;
        }

        // Header line: either 4 fields (new hunk) or 3 fields (continuation)
        let mut parts = line.split_whitespace();
        let sha = parts.next().unwrap_or("");
        let p2 = parts.next().unwrap_or("");
        let p3 = parts.next().unwrap_or("");
        let p4 = parts.next();

        let is_header = !sha.is_empty()
            && sha.chars().all(|c| c.is_ascii_hexdigit())
            && !p2.is_empty()
            && !p3.is_empty();
        if !is_header {
            continue;
        }

        // If we encounter a new hunk header (4 fields), flush previous hunk first
        if p4.is_some() {
            if let Some(prev_sha) = cur_commit.take() {
                // Push the previous hunk
                let start = cur_final_start;
                let end = if cur_group_size > 0 {
                    start + cur_group_size - 1
                } else {
                    start
                };
                let orig_start = cur_orig_start;
                let orig_end = if cur_group_size > 0 {
                    orig_start + cur_group_size - 1
                } else {
                    orig_start
                };

                let orig_filename = cur_meta.filename.take().filter(|f| f != file_path);
                hunks.push(BlameHunk {
                    range: (start, end),
                    orig_range: (orig_start, orig_end),
                    commit_sha: prev_sha,
                    abbrev_sha: String::new(),
                    original_author: cur_meta.author.clone(),
                    author_email: cur_meta.author_mail.clone(),
                    author_time: cur_meta.author_time,
                    author_tz: cur_meta.author_tz.clone(),
                    ai_human_author: None,
                    committer: cur_meta.committer.clone(),
                    committer_email: cur_meta.committer_mail.clone(),
                    committer_time: cur_meta.committer_time,
                    committer_tz: cur_meta.committer_tz.clone(),
                    is_boundary: cur_meta.boundary,
                    orig_filename,
                });
            }

            // Start new hunk
            cur_commit = Some(sha.to_string());
            // According to docs: fields are orig_lineno, final_lineno, group_size
            let orig_start = p2.parse::<u32>().unwrap_or(0);
            let final_start = p3.parse::<u32>().unwrap_or(0);
            let group = p4.unwrap_or("1").parse::<u32>().unwrap_or(1);
            cur_orig_start = orig_start;
            cur_final_start = final_start;
            cur_group_size = group;
            // Reset metadata for the new hunk
            cur_meta = CurMeta::default();
        } else {
            // 3-field header: continuation line within current hunk
            // Nothing to do for grouping since we use recorded group_size
            // Metadata remains from the first line of the hunk
            if cur_commit.is_none() {
                // Defensive: if no current hunk, start one with size 1
                cur_commit = Some(sha.to_string());
                cur_orig_start = p2.parse::<u32>().unwrap_or(0);
                cur_final_start = p3.parse::<u32>().unwrap_or(0);
                cur_group_size = 1;
            }
        }
    }

    // Flush the final hunk if present
    if let Some(prev_sha) = cur_commit.take() {
        let start = cur_final_start;
        let end = if cur_group_size > 0 {
            start + cur_group_size - 1
        } else {
            start
        };
        let orig_start = cur_orig_start;
        let orig_end = if cur_group_size > 0 {
            orig_start + cur_group_size - 1
        } else {
            orig_start
        };

        let orig_filename = cur_meta.filename.take().filter(|f| f != file_path);
        hunks.push(BlameHunk {
            range: (start, end),
            orig_range: (orig_start, orig_end),
            commit_sha: prev_sha,
            abbrev_sha: String::new(),
            original_author: cur_meta.author.clone(),
            author_email: cur_meta.author_mail.clone(),
            author_time: cur_meta.author_time,
            author_tz: cur_meta.author_tz.clone(),
            ai_human_author: None,
            committer: cur_meta.committer.clone(),
            committer_email: cur_meta.committer_mail.clone(),
            committer_time: cur_meta.committer_time,
            committer_tz: cur_meta.committer_tz.clone(),
            is_boundary: cur_meta.boundary,
            orig_filename,
        });
    }

    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_preparation_handles_empty_overlap_and_owned_sorting() {
        let empty = prepare_blame_hunks(vec![]);
        assert!(empty.hunks.is_empty() && empty.line_to_hunk.is_empty());
        assert!(empty.requested_lines.is_empty());
        let hunks = parse_porcelain_blame_output("a 2 2 2\nb 1 1 2", "f");
        let mut prepared = prepare_blame_hunks(hunks);
        assert_eq!(prepared.requested_lines, [1, 2, 3]);
        assert_eq!(prepared.hunks[1].commit_sha, "b");
        assert_eq!(prepared.line_to_hunk[&2].commit_sha, "b");
        prepared.hunks[1].commit_sha = "changed".to_string();
        assert_eq!(prepared.line_to_hunk[&2].commit_sha, "b");
    }
}
