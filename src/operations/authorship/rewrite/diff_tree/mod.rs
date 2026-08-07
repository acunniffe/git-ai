use std::collections::HashMap;

use crate::clients::git_cli::{exec_git, exec_git_stdin_streaming};
use crate::error::GitAiError;
use crate::model::hunk_shift::{DiffHunk, parse_hunk_header};
use crate::operations::git::oid::is_full_oid;
use crate::operations::git::repository::Repository;

use super::DiffTreeResult;

#[cfg(test)]
mod tests;

const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const DIFF_TREE_STREAM_CHUNK_SIZE: usize = 50;

fn empty_tree_sha() -> &'static str {
    EMPTY_TREE_SHA
}

fn tree_revision_arg(sha: &str) -> Option<String> {
    if sha == "initial" {
        None
    } else {
        Some(format!("{}^{{tree}}", sha))
    }
}

fn insert_known_tree(sha_to_tree: &mut HashMap<String, String>, sha: &str) -> bool {
    if sha == "initial" {
        sha_to_tree.insert(sha.to_string(), empty_tree_sha().to_string());
        true
    } else {
        false
    }
}

fn unique_pair_shas(pairs: &[(String, String)]) -> Vec<String> {
    let mut unique_shas = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (src, dst) in pairs {
        if seen.insert(src.clone()) {
            unique_shas.push(src.clone());
        }
        if seen.insert(dst.clone()) {
            unique_shas.push(dst.clone());
        }
    }
    unique_shas
}

fn resolve_tree_shas(
    repo: &Repository,
    unique_shas: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    let mut sha_to_tree = HashMap::new();
    let mut shas_to_resolve = Vec::new();

    for sha in unique_shas {
        if !insert_known_tree(&mut sha_to_tree, sha) {
            shas_to_resolve.push(sha.clone());
        }
    }

    if shas_to_resolve.is_empty() {
        return Ok(sha_to_tree);
    }

    let mut rev_parse_args = repo.global_args_for_exec();
    rev_parse_args.push("rev-parse".to_string());
    for sha in &shas_to_resolve {
        if let Some(arg) = tree_revision_arg(sha) {
            rev_parse_args.push(arg);
        }
    }
    let rev_output = exec_git(&rev_parse_args)?;
    let rev_stdout = String::from_utf8_lossy(&rev_output.stdout);
    let tree_shas: Vec<&str> = rev_stdout.lines().collect();

    if tree_shas.len() != shas_to_resolve.len() {
        return Err(GitAiError::Generic(format!(
            "rev-parse returned {} trees for {} commits",
            tree_shas.len(),
            shas_to_resolve.len()
        )));
    }

    for (commit, tree) in shas_to_resolve.into_iter().zip(tree_shas) {
        sha_to_tree.insert(commit, tree.to_string());
    }

    Ok(sha_to_tree)
}

fn tree_for_commit<'a>(
    sha_to_tree: &'a HashMap<String, String>,
    sha: &str,
) -> Result<&'a str, GitAiError> {
    sha_to_tree
        .get(sha)
        .map(String::as_str)
        .ok_or_else(|| GitAiError::Generic(format!("missing tree for commit {}", sha)))
}

fn build_diff_tree_stdin(
    pairs: &[(String, String)],
    sha_to_tree: &HashMap<String, String>,
) -> Result<String, GitAiError> {
    let mut stdin_data = String::new();
    for (src, dst) in pairs {
        let src_tree = tree_for_commit(sha_to_tree, src)?;
        let dst_tree = tree_for_commit(sha_to_tree, dst)?;
        stdin_data.push_str(src_tree);
        stdin_data.push(' ');
        stdin_data.push_str(dst_tree);
        stdin_data.push('\n');
    }
    Ok(stdin_data)
}

fn build_diff_tree_stdin_args(repo: &Repository) -> Vec<String> {
    // Single git diff-tree --stdin call.
    //
    // We intentionally use the General profile (no PatchParse prefix forcing)
    // here: `diff-tree` is plumbing and -- unlike the `git diff` porcelain --
    // ignores the user's diff.{noprefix,mnemonicPrefix,srcPrefix,dstPrefix},
    // diff.external, and per-path textconv attributes. It always emits raw
    // content with default `a/`..`b/` prefixes, which is exactly what
    // extract_b_path / parse_diff_tree_output expect. (Contrast diff_added_lines
    // in repository.rs, which DOES run `git diff` and therefore must force
    // InternalGitProfile::PatchParse.)
    let mut args = repo.global_args_for_exec();
    args.extend([
        "diff-tree".to_string(),
        "--stdin".to_string(),
        "-p".to_string(),
        "-U0".to_string(),
        "-M".to_string(),
        "--no-color".to_string(),
        "-r".to_string(),
    ]);
    args
}

fn compute_diff_tree_stdin(
    repo: &Repository,
    stdin_data: String,
    pair_count: usize,
) -> Result<Vec<DiffTreeResult>, GitAiError> {
    // Stream the output line-by-line into the parser instead of buffering it:
    // after a rebase across a large trunk delta, every pair's root-tree diff
    // contains that whole delta, so the batched output is
    // (trunk delta bytes) x (pair count) and buffering it has driven the
    // daemon to multi-GB RSS. The parsed hunk/line structures are a small
    // fraction of the raw patch text.
    let args = build_diff_tree_stdin_args(repo);
    let mut parser = BatchedDiffTreeParser::new(pair_count);
    exec_git_stdin_streaming(&args, stdin_data.as_bytes(), |line| parser.feed_line(line))?;
    Ok(parser.finish())
}

/// Batch-compute diff-trees for multiple commit pairs in a single git process.
/// Resolves commits to tree SHAs, then pipes all pairs into `git diff-tree --stdin`.
pub(crate) fn compute_diff_trees_batch(
    repo: &Repository,
    pairs: &[(String, String)],
) -> Result<Vec<DiffTreeResult>, GitAiError> {
    if pairs.is_empty() {
        return Ok(Vec::new());
    }

    let unique_shas = unique_pair_shas(pairs);
    let sha_to_tree = resolve_tree_shas(repo, &unique_shas)?;
    let stdin_data = build_diff_tree_stdin(pairs, &sha_to_tree)?;
    compute_diff_tree_stdin(repo, stdin_data, pairs.len())
}

/// Compute all diff-tree pairs in one Git process while releasing parsed
/// results in bounded chunks. The callback runs inline with stdout reads, so
/// pausing to apply a chunk naturally back-pressures the child process.
pub(super) fn compute_diff_trees_streaming(
    repo: &Repository,
    pairs: &[(String, String)],
    mut apply_chunk: impl FnMut(Vec<DiffTreeResult>),
) -> Result<(), GitAiError> {
    if pairs.is_empty() {
        return Ok(());
    }

    let unique_shas = unique_pair_shas(pairs);
    let sha_to_tree = resolve_tree_shas(repo, &unique_shas)?;
    let stdin_data = build_diff_tree_stdin(pairs, &sha_to_tree)?;
    let args = build_diff_tree_stdin_args(repo);
    let mut parser = BatchedDiffTreeParser::new(pairs.len());

    exec_git_stdin_streaming(&args, stdin_data.as_bytes(), |line| {
        parser.feed_line(line);
        if parser.completed_count() >= DIFF_TREE_STREAM_CHUNK_SIZE {
            apply_chunk(parser.take_completed());
        }
    })?;

    let tail = parser.take_all();
    if !tail.is_empty() {
        apply_chunk(tail);
    }
    Ok(())
}

/// Incremental parser for the output of `git diff-tree --stdin`, which
/// produces one patch per tree pair, each preceded by a "tree1 tree2"
/// separator line. Results are positional: the Nth separator starts the Nth
/// pair's patch. Fed one line at a time so callers can stream arbitrarily
/// large diff output without holding the raw patch text in memory.
struct BatchedDiffTreeParser {
    expected_pairs: usize,
    results: Vec<DiffTreeResult>,
    current: DiffTreeChunkParser,
    seen_first_header: bool,
}

impl BatchedDiffTreeParser {
    fn new(expected_pairs: usize) -> Self {
        Self {
            expected_pairs,
            results: Vec::with_capacity(expected_pairs.min(DIFF_TREE_STREAM_CHUNK_SIZE)),
            current: DiffTreeChunkParser::default(),
            seen_first_header: false,
        }
    }

    fn feed_line(&mut self, line: &str) {
        // Separator lines are exactly "tree_sha1 tree_sha2" (two OIDs separated by a space)
        if is_tree_pair_separator(line) {
            if self.seen_first_header {
                let chunk = std::mem::take(&mut self.current);
                self.results.push(chunk.finish());
            }
            self.seen_first_header = true;
        } else if self.seen_first_header {
            self.current.feed_line(line);
        }
    }

    fn completed_count(&self) -> usize {
        self.results.len()
    }

    fn take_completed(&mut self) -> Vec<DiffTreeResult> {
        let completed = std::mem::take(&mut self.results);
        self.expected_pairs = self.expected_pairs.saturating_sub(completed.len());
        self.results = Vec::with_capacity(self.expected_pairs.min(DIFF_TREE_STREAM_CHUNK_SIZE));
        completed
    }

    fn take_all(&mut self) -> Vec<DiffTreeResult> {
        if self.seen_first_header {
            let current = std::mem::take(&mut self.current);
            self.results.push(current.finish());
            self.seen_first_header = false;
        }

        let mut remaining = std::mem::take(&mut self.results);
        while remaining.len() < self.expected_pairs {
            remaining.push(DiffTreeResult::default());
        }
        self.expected_pairs = 0;
        remaining
    }

    fn finish(mut self) -> Vec<DiffTreeResult> {
        self.take_all()
    }
}

/// Parse the output of `git diff-tree --stdin` provided as a single string.
/// Thin wrapper over `BatchedDiffTreeParser` (which the streaming path feeds
/// directly).
#[cfg(test)]
fn parse_batched_diff_tree_output(output: &str, expected_pairs: usize) -> Vec<DiffTreeResult> {
    let mut parser = BatchedDiffTreeParser::new(expected_pairs);
    for line in output.lines() {
        parser.feed_line(line);
    }
    parser.finish()
}

fn is_tree_pair_separator(line: &str) -> bool {
    // "tree1 tree2" — two git OIDs separated by a single space. Validate both
    // halves structurally via is_full_oid so this accepts both the 81-byte
    // SHA-1 separator and the 129-byte SHA-256 separator (rather than a
    // hard-coded length).
    let Some((old, new)) = line.split_once(' ') else {
        return false;
    };
    is_full_oid(old) && is_full_oid(new)
}

/// Incremental parser for a single tree pair's diff-tree patch.
#[derive(Default)]
struct DiffTreeChunkParser {
    hunks_by_file: HashMap<String, Vec<DiffHunk>>,
    added_lines_by_file: HashMap<String, Vec<u32>>,
    renames: Vec<(String, String)>,
    current_file: Option<String>,
    current_rename_from: Option<String>,
    active_hunk_new_line: Option<u32>,
}

impl DiffTreeChunkParser {
    fn feed_line(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // Extract the b/ path from "a/old b/new"
            self.current_file = extract_b_path(rest);
            self.current_rename_from = None;
            self.active_hunk_new_line = None;
        } else if let Some(from_path) = line.strip_prefix("rename from ") {
            self.current_rename_from = Some(from_path.to_string());
            self.active_hunk_new_line = None;
        } else if let Some(to_path) = line.strip_prefix("rename to ") {
            if let Some(from_path) = self.current_rename_from.take() {
                self.renames.push((from_path, to_path.to_string()));
            }
        } else if line.starts_with("@@")
            && let Some(ref file) = self.current_file
            && let Some(hunk) = parse_hunk_header(line)
        {
            self.active_hunk_new_line = Some(hunk.new_start);
            self.hunks_by_file
                .entry(file.clone())
                .or_default()
                .push(hunk);
        } else if let Some(new_line) = self.active_hunk_new_line.as_mut() {
            if line.starts_with('+') {
                if let Some(ref file) = self.current_file {
                    self.added_lines_by_file
                        .entry(file.clone())
                        .or_default()
                        .push(*new_line);
                }
                *new_line += 1;
            } else if line.starts_with('-') || line.starts_with('\\') {
                // Removed lines and "\ No newline at end of file" markers do
                // not advance the new-file line cursor.
            } else {
                *new_line += 1;
            }
        }
    }

    fn finish(mut self) -> DiffTreeResult {
        for lines in self.added_lines_by_file.values_mut() {
            lines.sort_unstable();
            lines.dedup();
        }

        DiffTreeResult {
            hunks_by_file: self.hunks_by_file,
            added_lines_by_file: self.added_lines_by_file,
            renames: self.renames,
        }
    }
}

#[cfg(test)]
fn parse_diff_tree_output(output: &str) -> DiffTreeResult {
    let mut parser = DiffTreeChunkParser::default();
    for line in output.lines() {
        parser.feed_line(line);
    }
    parser.finish()
}

fn extract_b_path(diff_header: &str) -> Option<String> {
    // Format: "a/path b/path" or "a/path with spaces b/path with spaces"
    // The b/ path starts after the last occurrence of " b/"
    let marker = " b/";
    let pos = diff_header.rfind(marker)?;
    Some(diff_header[pos + marker.len()..].to_string())
}
