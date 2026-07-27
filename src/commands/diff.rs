use crate::authorship::authorship_log::{HumanRecord, LineRange, PromptRecord, SessionRecord};
use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::authorship::ignore::{
    build_ignore_matcher, effective_ignore_patterns, should_ignore_file_with_matcher,
};
use crate::commands::blame::GitAiBlameOptions;
use crate::error::GitAiError;
use crate::git::notes_api::{read_authorship, read_note};
use crate::git::repository::{InternalGitProfile, Repository, exec_git_with_profile};
use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::IsTerminal;
use unicode_normalization::UnicodeNormalization;

// ============================================================================
// Data Structures
// ============================================================================

#[derive(Debug, Clone)]
pub enum DiffSpec {
    SingleCommit(String),      // SHA
    TwoCommit(String, String), // start..end
}

#[derive(Debug, Clone)]
pub enum DiffFormat {
    Json,
    GitCompatibleTerminal,
}

#[derive(Debug)]
pub struct DiffHunk {
    pub file_path: String,
    pub old_file_path: Option<String>,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub deleted_lines: Vec<u32>, // Absolute line numbers in OLD file
    pub added_lines: Vec<u32>,   // Absolute line numbers in NEW file
    pub deleted_contents: Vec<String>,
    pub added_contents: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DiffCommandOptions {
    pub format: DiffFormat,
    pub blame_deletions: bool,
    pub blame_deletions_since: Option<String>,
    pub include_stats: bool,
    pub all_prompts: bool,
}

impl Default for DiffCommandOptions {
    fn default() -> Self {
        Self {
            format: DiffFormat::GitCompatibleTerminal,
            blame_deletions: false,
            blame_deletions_since: None,
            include_stats: false,
            all_prompts: false,
        }
    }
}

#[derive(Debug)]
pub struct ParsedDiffArgs {
    pub spec: DiffSpec,
    pub options: DiffCommandOptions,
}

#[derive(Debug, Hash, Eq, PartialEq, Clone)]
pub struct DiffLineKey {
    pub file: String,
    pub line: u32,
    pub side: LineSide,
}

/// JSON output format for git-ai diff --json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffJson {
    /// Per-file diff information with annotations
    pub files: BTreeMap<String, FileDiffJson>,
    /// Prompt records keyed by prompt hash (old-format, bare 16-char hex)
    pub prompts: BTreeMap<String, PromptRecord>,
    /// Session records keyed by full attestation hash (s_xxx::t_yyy)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sessions: BTreeMap<String, SessionRecord>,
    /// Human records keyed by human hash (h_-prefixed)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub humans: BTreeMap<String, HumanRecord>,
    /// Per-hunk records for machine consumption
    #[serde(default)]
    pub hunks: Vec<DiffJsonHunk>,
    /// Commit metadata for all commits referenced by hunks
    #[serde(default)]
    pub commits: BTreeMap<String, DiffCommitMetadata>,
    /// Optional commit stats for single-commit diffs (`--json --include-stats`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_stats: Option<DiffCommitStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct DiffToolModelStats {
    #[serde(default)]
    pub ai_lines_added: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct DiffCommitStats {
    #[serde(default)]
    pub ai_lines_added: u32,
    #[serde(default)]
    pub human_lines_added: u32,
    #[serde(default)]
    pub unknown_lines_added: u32,
    #[serde(default)]
    pub git_lines_added: u32,
    #[serde(default)]
    pub git_lines_deleted: u32,
    #[serde(default)]
    pub tool_model_breakdown: BTreeMap<String, DiffToolModelStats>,
}

/// Per-file diff information in JSON output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDiffJson {
    /// Annotations mapping prompt hash to line ranges
    /// Line ranges are serialized as JSON tuples: [start, end] or single number
    #[serde(serialize_with = "serialize_annotations")]
    pub annotations: BTreeMap<String, Vec<LineRange>>,
    /// The unified diff for this file
    pub diff: String,
    /// The base content of the file (before changes)
    pub base_content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffJsonHunk {
    pub commit_sha: String,
    pub content_hash: String,
    pub hunk_kind: String, // "addition" | "deletion"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_commit_sha: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub human_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffCommitMetadata {
    pub authored_time: String,
    pub msg: String,
    pub full_msg: String,
    pub author: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorship_note: Option<String>,
}

#[derive(Debug, Hash, Eq, PartialEq, Clone)]
pub enum LineSide {
    Old, // For deleted lines
    New, // For added lines
}

#[derive(Debug, Clone)]
pub enum Attribution {
    Ai(String),    // Tool name: "cursor", "claude", etc.
    Human(String), // Username
    NoData,        // No authorship data available
}

#[derive(Debug, Clone)]
struct LineAttributionDetail {
    commit_sha: Option<String>,
    prompt_id: Option<String>,
    human_id: Option<String>,
}

#[derive(Debug)]
pub struct DiffBuildArtifacts {
    pub attributions: HashMap<DiffLineKey, Attribution>,
    pub annotations_by_file: BTreeMap<String, BTreeMap<String, Vec<LineRange>>>,
    pub prompts: BTreeMap<String, PromptRecord>,
    pub sessions: BTreeMap<String, SessionRecord>,
    pub humans: BTreeMap<String, HumanRecord>,
    pub json_hunks: Vec<DiffJsonHunk>,
    pub commits: BTreeMap<String, DiffCommitMetadata>,
    pub included_files: HashSet<String>,
}

// ============================================================================
// Main Entry Point
// ============================================================================

pub fn handle_diff(repo: &Repository, args: &[String]) -> Result<(), GitAiError> {
    if args.is_empty() {
        eprintln!("Error: diff requires a commit or commit range argument");
        eprintln!("Usage: git-ai diff <commit>");
        eprintln!("       git-ai diff <commit1>..<commit2>");
        std::process::exit(1);
    }

    let parsed = parse_diff_args(args)?;
    let output = execute_diff(repo, parsed)?;
    print!("{}", output);

    Ok(())
}

// ============================================================================
// Argument Parsing
// ============================================================================

pub fn parse_diff_args(args: &[String]) -> Result<ParsedDiffArgs, GitAiError> {
    let mut options = DiffCommandOptions::default();
    let mut positional_args: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => {
                options.format = DiffFormat::Json;
                i += 1;
            }
            "--blame-deletions" => {
                options.blame_deletions = true;
                i += 1;
            }
            "--blame-deletions-since" => {
                if i + 1 >= args.len() {
                    return Err(GitAiError::Generic(
                        "--blame-deletions-since requires a value".to_string(),
                    ));
                }
                options.blame_deletions_since = Some(args[i + 1].clone());
                i += 2;
            }
            "--include-stats" => {
                options.include_stats = true;
                i += 1;
            }
            "--all-prompts" => {
                options.all_prompts = true;
                i += 1;
            }
            arg if arg.starts_with("--") => {
                return Err(GitAiError::Generic(format!("Unknown option: {}", arg)));
            }
            _ => {
                positional_args.push(args[i].as_str());
                i += 1;
            }
        }
    }

    if options.blame_deletions_since.is_some() && !options.blame_deletions {
        return Err(GitAiError::Generic(
            "--blame-deletions-since requires --blame-deletions".to_string(),
        ));
    }
    if options.include_stats && !matches!(options.format, DiffFormat::Json) {
        return Err(GitAiError::Generic(
            "--include-stats requires --json".to_string(),
        ));
    }
    if options.all_prompts && !matches!(options.format, DiffFormat::Json) {
        return Err(GitAiError::Generic(
            "--all-prompts requires --json".to_string(),
        ));
    }

    let spec = match positional_args.as_slice() {
        [] => {
            return Err(GitAiError::Generic(
                "diff requires a commit or commit range argument".to_string(),
            ));
        }
        [start, end] => {
            if start.contains("..") || end.contains("..") {
                return Err(GitAiError::Generic(
                    "Invalid diff arguments. Expected: <commit>, <commit1>..<commit2>, or <commit1> <commit2>".to_string(),
                ));
            }
            DiffSpec::TwoCommit((*start).to_string(), (*end).to_string())
        }
        [arg] => {
            // Check for commit range (start..end)
            if arg.contains("..") {
                let parts: Vec<&str> = arg.split("..").collect();
                if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                    DiffSpec::TwoCommit(parts[0].to_string(), parts[1].to_string())
                } else {
                    return Err(GitAiError::Generic(
                        "Invalid commit range format. Expected: <commit>..<commit>".to_string(),
                    ));
                }
            } else {
                DiffSpec::SingleCommit(positional_args[0].to_string())
            }
        }
        _ => {
            return Err(GitAiError::Generic(
                "Invalid diff arguments. Expected: <commit>, <commit1>..<commit2>, or <commit1> <commit2>".to_string(),
            ));
        }
    };

    if options.include_stats && matches!(spec, DiffSpec::TwoCommit(_, _)) {
        return Err(GitAiError::Generic(
            "--include-stats is only supported for single-commit diffs".to_string(),
        ));
    }
    if options.all_prompts && matches!(spec, DiffSpec::TwoCommit(_, _)) {
        return Err(GitAiError::Generic(
            "--all-prompts is only supported for single-commit diffs".to_string(),
        ));
    }

    Ok(ParsedDiffArgs { spec, options })
}

// ============================================================================
// Core Execution Logic
// ============================================================================

pub fn execute_diff(repo: &Repository, parsed: ParsedDiffArgs) -> Result<String, GitAiError> {
    let is_single_commit = matches!(&parsed.spec, DiffSpec::SingleCommit(_));

    // Resolve commits to get from/to SHAs
    let (from_commit, to_commit) = match parsed.spec {
        DiffSpec::TwoCommit(start, end) => {
            // Resolve both commits
            let from = resolve_commit(repo, &start)?;
            let to = resolve_commit(repo, &end)?;
            (from, to)
        }
        DiffSpec::SingleCommit(commit) => {
            // Resolve the commit and its parent
            let to = resolve_commit(repo, &commit)?;
            let from = resolve_parent(repo, &to)?;
            (from, to)
        }
    };

    // Build a single set of artifacts used by both terminal and JSON outputs.
    let artifacts = build_diff_artifacts(repo, &from_commit, &to_commit, &parsed.options)?;

    // Format and output annotated diff
    let output = match parsed.options.format {
        DiffFormat::Json => {
            let mut output_prompts = artifacts.prompts.clone();
            let mut output_sessions = artifacts.sessions.clone();
            if is_single_commit && parsed.options.all_prompts {
                merge_missing_prompts_and_sessions_from_authorship_note(
                    repo,
                    &to_commit,
                    &mut output_prompts,
                    &mut output_sessions,
                );
            }

            let commit_stats = if parsed.options.include_stats {
                let mut stats_prompts = output_prompts.clone();
                let mut stats_sessions = output_sessions.clone();
                if is_single_commit && !parsed.options.all_prompts {
                    merge_missing_prompts_and_sessions_from_authorship_note(
                        repo,
                        &to_commit,
                        &mut stats_prompts,
                        &mut stats_sessions,
                    );
                }
                Some(calculate_diff_commit_stats(
                    &artifacts,
                    &stats_prompts,
                    &stats_sessions,
                ))
            } else {
                None
            };

            let diff_json = build_diff_json(
                repo,
                &from_commit,
                &to_commit,
                &artifacts,
                &output_prompts,
                &output_sessions,
                commit_stats,
            )?;
            serde_json::to_string(&diff_json)
                .map_err(|e| GitAiError::Generic(format!("Failed to serialize JSON: {}", e)))?
        }
        DiffFormat::GitCompatibleTerminal => format_annotated_diff(
            repo,
            &from_commit,
            &to_commit,
            &artifacts.attributions,
            &artifacts.humans,
            &artifacts.included_files,
        )?,
    };

    Ok(output)
}

// ============================================================================
// Commit Resolution
// ============================================================================

fn resolve_commit(repo: &Repository, rev: &str) -> Result<String, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.push("rev-parse".to_string());
    args.push(rev.to_string());

    let output = exec_git_with_profile(&args, InternalGitProfile::General)?;
    let sha = String::from_utf8(output.stdout)
        .map_err(|e| GitAiError::Generic(format!("Failed to parse rev-parse output: {}", e)))?
        .trim()
        .to_string();

    if sha.is_empty() {
        return Err(GitAiError::Generic(format!(
            "Could not resolve commit: {}",
            rev
        )));
    }

    Ok(sha)
}

fn resolve_parent(repo: &Repository, commit: &str) -> Result<String, GitAiError> {
    let parent_rev = format!("{}^", commit);

    // Try to resolve parent
    let mut args = repo.global_args_for_exec();
    args.push("rev-parse".to_string());
    args.push(parent_rev);

    let output = exec_git_with_profile(&args, InternalGitProfile::General);

    match output {
        Ok(out) => {
            let sha = String::from_utf8(out.stdout)
                .map_err(|e| GitAiError::Generic(format!("Failed to parse parent SHA: {}", e)))?
                .trim()
                .to_string();

            if sha.is_empty() {
                // No parent, this is initial commit - use empty tree
                Ok("4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string())
            } else {
                Ok(sha)
            }
        }
        Err(_) => {
            // No parent, this is initial commit - use empty tree hash
            Ok("4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string())
        }
    }
}

// ============================================================================
// Diff Retrieval with Line Numbers
// ============================================================================

pub fn get_diff_with_line_numbers(
    repo: &Repository,
    from: &str,
    to: &str,
) -> Result<Vec<DiffHunk>, GitAiError> {
    let diff_text = get_diff_text(repo, from, to, true)?;
    parse_diff_hunks(&diff_text)
}

fn get_diff_text(
    repo: &Repository,
    from: &str,
    to: &str,
    zero_context: bool,
) -> Result<String, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.push("diff".to_string());
    if zero_context {
        args.push("-U0".to_string()); // No context lines, just changes
    }
    // Use permissive rename detection so rename+edit commits are represented
    // as renames with edit hunks instead of delete/add file pairs.
    args.push("--find-renames=1%".to_string());
    args.push("--no-color".to_string());
    args.push(from.to_string());
    args.push(to.to_string());

    let output = exec_git_with_profile(&args, InternalGitProfile::PatchParse)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_diff_hunks(diff_text: &str) -> Result<Vec<DiffHunk>, GitAiError> {
    let mut hunks = Vec::new();
    let mut current_old_file: Option<String> = None;
    let mut current_file = String::new();
    let mut current_hunk: Option<DiffHunk> = None;
    let mut old_line_cursor = 0u32;
    let mut new_line_cursor = 0u32;

    let flush_current_hunk = |hunks: &mut Vec<DiffHunk>, current_hunk: &mut Option<DiffHunk>| {
        if let Some(hunk) = current_hunk.take() {
            hunks.push(hunk);
        }
    };

    for line in diff_text.lines() {
        if line.starts_with("diff --git ") {
            flush_current_hunk(&mut hunks, &mut current_hunk);
            if let Some((old_file, new_file)) = parse_diff_git_header_paths(line) {
                current_old_file = Some(old_file);
                current_file = new_file;
            } else {
                current_old_file = None;
                current_file.clear();
            }
            continue;
        }

        if current_hunk.is_none() {
            if let Some(path_opt) = parse_old_file_path_from_minus_header_line(line) {
                current_old_file = path_opt;
                if current_file.is_empty() {
                    current_file = current_old_file.clone().unwrap_or_default();
                }
                continue;
            }

            if let Some(path_opt) = parse_new_file_path_from_plus_header_line(line) {
                current_file = path_opt
                    .or_else(|| current_old_file.clone())
                    .unwrap_or_default();
                continue;
            }
        }

        if line.starts_with("@@ ") {
            flush_current_hunk(&mut hunks, &mut current_hunk);
            let old_file_path = current_old_file
                .as_deref()
                .filter(|old_path| *old_path != current_file.as_str());
            if let Some(mut hunk) = parse_hunk_line(line, &current_file, old_file_path)? {
                old_line_cursor = hunk.old_start;
                new_line_cursor = hunk.new_start;
                hunk.deleted_lines.clear();
                hunk.added_lines.clear();
                current_hunk = Some(hunk);
            }
            continue;
        }

        if let Some(hunk) = current_hunk.as_mut() {
            if let Some(stripped) = line.strip_prefix('-') {
                hunk.deleted_lines.push(old_line_cursor);
                hunk.deleted_contents.push(stripped.to_string());
                old_line_cursor += 1;
            } else if let Some(stripped) = line.strip_prefix('+') {
                hunk.added_lines.push(new_line_cursor);
                hunk.added_contents.push(stripped.to_string());
                new_line_cursor += 1;
            } else if line.starts_with(' ') {
                old_line_cursor += 1;
                new_line_cursor += 1;
            }
        }
    }

    flush_current_hunk(&mut hunks, &mut current_hunk);
    Ok(hunks)
}

fn normalize_diff_path_token(path: &str) -> String {
    let unescaped = crate::utils::unescape_git_path(path.trim_end());
    let prefixes = ["a/", "b/", "c/", "w/", "i/", "o/"];
    let stripped = prefixes
        .iter()
        .find_map(|prefix| unescaped.strip_prefix(prefix))
        .unwrap_or(&unescaped);
    stripped.nfc().collect()
}

fn parse_new_file_path_from_plus_header_line(line: &str) -> Option<Option<String>> {
    parse_file_path_from_header_line(line, "+++ ")
}

fn parse_old_file_path_from_minus_header_line(line: &str) -> Option<Option<String>> {
    parse_file_path_from_header_line(line, "--- ")
}

fn parse_file_path_from_header_line(line: &str, prefix: &str) -> Option<Option<String>> {
    let raw = line.strip_prefix(prefix)?;
    if raw.trim_end() == "/dev/null" {
        return Some(None);
    }
    Some(Some(normalize_diff_path_token(raw)))
}

fn parse_diff_git_header_paths(line: &str) -> Option<(String, String)> {
    let raw = line.strip_prefix("diff --git ")?;
    let (old_raw, new_raw) = parse_two_git_path_tokens(raw)?;
    Some((
        normalize_diff_path_token(&old_raw),
        normalize_diff_path_token(&new_raw),
    ))
}

fn parse_two_git_path_tokens(raw: &str) -> Option<(String, String)> {
    let mut chars = raw.chars().peekable();
    let mut tokens: Vec<String> = Vec::new();

    while tokens.len() < 2 {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }

        let mut token = String::new();
        if chars.peek() == Some(&'"') {
            token.push(chars.next().unwrap_or('"'));
            let mut escaped = false;
            for ch in chars.by_ref() {
                token.push(ch);
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                if ch == '"' {
                    break;
                }
            }
        } else {
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() {
                    break;
                }
                token.push(ch);
                chars.next();
            }
        }

        if token.is_empty() {
            return None;
        }
        tokens.push(token);
    }

    if tokens.len() == 2 {
        Some((tokens[0].clone(), tokens[1].clone()))
    } else {
        None
    }
}

fn parse_hunk_line(
    line: &str,
    file_path: &str,
    old_file_path: Option<&str>,
) -> Result<Option<DiffHunk>, GitAiError> {
    // Parse hunk header format: @@ -old_start,old_count +new_start,new_count @@
    // Also handles: @@ -old_start +new_start,new_count @@ (single line deletion)
    // Also handles: @@ -old_start,old_count +new_start @@ (single line addition)

    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 3 {
        return Ok(None);
    }

    let old_part = parts[1]; // e.g., "-10,3" or "-10"
    let new_part = parts[2]; // e.g., "+15,5" or "+15"

    // Parse old part
    let (old_start, old_count) = if let Some(old_str) = old_part.strip_prefix('-') {
        if let Some((start_str, count_str)) = old_str.split_once(',') {
            let start: u32 = start_str.parse().unwrap_or(0);
            let count: u32 = count_str.parse().unwrap_or(0);
            (start, count)
        } else {
            let start: u32 = old_str.parse().unwrap_or(0);
            (start, 1)
        }
    } else {
        (0, 0)
    };

    // Parse new part
    let (new_start, new_count) = if let Some(new_str) = new_part.strip_prefix('+') {
        if let Some((start_str, count_str)) = new_str.split_once(',') {
            let start: u32 = start_str.parse().unwrap_or(0);
            let count: u32 = count_str.parse().unwrap_or(0);
            (start, count)
        } else {
            let start: u32 = new_str.parse().unwrap_or(0);
            (start, 1)
        }
    } else {
        (0, 0)
    };

    // Build line number lists
    let deleted_lines: Vec<u32> = if old_count > 0 {
        (old_start..old_start + old_count).collect()
    } else {
        Vec::new()
    };

    let added_lines: Vec<u32> = if new_count > 0 {
        (new_start..new_start + new_count).collect()
    } else {
        Vec::new()
    };

    Ok(Some(DiffHunk {
        file_path: file_path.to_string(),
        old_file_path: old_file_path.map(ToString::to_string),
        old_start,
        old_count,
        new_start,
        new_count,
        deleted_lines,
        added_lines,
        deleted_contents: Vec::new(),
        added_contents: Vec::new(),
    }))
}

// ============================================================================
// Attribution Overlay
// ============================================================================

pub fn overlay_diff_attributions(
    repo: &Repository,
    from_commit: &str,
    to_commit: &str,
    hunks: &[DiffHunk],
) -> Result<HashMap<DiffLineKey, Attribution>, GitAiError> {
    let (_, attributions, _, _, _, _, _) = build_line_attribution_data(
        repo,
        from_commit,
        to_commit,
        hunks,
        &DiffCommandOptions::default(),
    )?;
    Ok(attributions)
}

pub fn build_diff_artifacts(
    repo: &Repository,
    from_commit: &str,
    to_commit: &str,
    options: &DiffCommandOptions,
) -> Result<DiffBuildArtifacts, GitAiError> {
    build_diff_artifacts_with_note(repo, from_commit, to_commit, options, None)
}

pub fn build_diff_artifacts_with_note(
    repo: &Repository,
    from_commit: &str,
    to_commit: &str,
    options: &DiffCommandOptions,
    authorship_log: Option<&AuthorshipLog>,
) -> Result<DiffBuildArtifacts, GitAiError> {
    let hunks = get_diff_with_line_numbers(repo, from_commit, to_commit)?;

    if let Some(note) = authorship_log {
        return build_diff_artifacts_from_hunks(repo, hunks, to_commit, Some(note));
    }

    // Slow path: no authorship log, needs blame
    let effective_patterns = effective_ignore_patterns(repo, &[], &[]);
    let ignore_matcher = build_ignore_matcher(&effective_patterns);
    let diff_sections = get_diff_sections_by_file(repo, from_commit, to_commit)?;
    let mut included_files: HashSet<String> = diff_sections
        .into_iter()
        .map(|(file_path, _)| file_path)
        .filter(|file_path| {
            !file_path.is_empty() && !should_ignore_file_with_matcher(file_path, &ignore_matcher)
        })
        .collect();

    let mut hunks = hunks;
    hunks.retain(|hunk| {
        !hunk.file_path.is_empty()
            && !should_ignore_file_with_matcher(&hunk.file_path, &ignore_matcher)
    });
    included_files.extend(hunks.iter().map(|h| h.file_path.clone()));
    let line_contents = build_line_content_map(&hunks);

    let (annotations_by_file, attributions, line_details, prompts, sessions, humans, mut commits) =
        build_line_attribution_data(repo, from_commit, to_commit, &hunks, options)?;

    let json_hunks = build_json_hunks(
        repo,
        &hunks,
        &line_details,
        &line_contents,
        to_commit,
        &mut commits,
    )?;

    Ok(DiffBuildArtifacts {
        attributions,
        annotations_by_file,
        prompts,
        sessions,
        humans,
        json_hunks,
        commits,
        included_files,
    })
}

/// Build diff artifacts from pre-computed hunks, avoiding redundant git subprocess calls.
/// Used by the post-commit hook path where the caller already has the hunks from a single
/// `get_diff_with_line_numbers` call.
pub fn build_diff_artifacts_from_hunks(
    repo: &Repository,
    hunks: Vec<DiffHunk>,
    to_commit: &str,
    authorship_log: Option<&AuthorshipLog>,
) -> Result<DiffBuildArtifacts, GitAiError> {
    let effective_patterns = effective_ignore_patterns(repo, &[], &[]);
    let ignore_matcher = build_ignore_matcher(&effective_patterns);

    let mut hunks = hunks;
    hunks.retain(|hunk| {
        !hunk.file_path.is_empty()
            && !should_ignore_file_with_matcher(&hunk.file_path, &ignore_matcher)
    });

    let included_files: HashSet<String> = hunks.iter().map(|h| h.file_path.clone()).collect();
    let line_contents = build_line_content_map(&hunks);

    let (annotations_by_file, attributions, line_details, prompts, sessions, humans, mut commits) =
        if let Some(note) = authorship_log {
            build_line_attribution_from_note(to_commit, &hunks, note)
        } else {
            (
                BTreeMap::new(),
                HashMap::new(),
                HashMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
            )
        };

    let json_hunks = build_json_hunks(
        repo,
        &hunks,
        &line_details,
        &line_contents,
        to_commit,
        &mut commits,
    )?;

    Ok(DiffBuildArtifacts {
        attributions,
        annotations_by_file,
        prompts,
        sessions,
        humans,
        json_hunks,
        commits,
        included_files,
    })
}

#[allow(clippy::type_complexity)]
fn build_line_attribution_data(
    repo: &Repository,
    from_commit: &str,
    to_commit: &str,
    hunks: &[DiffHunk],
    options: &DiffCommandOptions,
) -> Result<
    (
        BTreeMap<String, BTreeMap<String, Vec<LineRange>>>,
        HashMap<DiffLineKey, Attribution>,
        HashMap<DiffLineKey, LineAttributionDetail>,
        BTreeMap<String, PromptRecord>,
        BTreeMap<String, SessionRecord>,
        BTreeMap<String, HumanRecord>,
        BTreeMap<String, DiffCommitMetadata>,
    ),
    GitAiError,
> {
    let mut annotations_by_file: BTreeMap<String, BTreeMap<String, Vec<LineRange>>> =
        BTreeMap::new();
    let mut attributions: HashMap<DiffLineKey, Attribution> = HashMap::new();
    let mut line_details: HashMap<DiffLineKey, LineAttributionDetail> = HashMap::new();
    let mut prompts: BTreeMap<String, PromptRecord> = BTreeMap::new();
    let mut sessions: BTreeMap<String, SessionRecord> = BTreeMap::new();
    let mut humans: BTreeMap<String, HumanRecord> = BTreeMap::new();
    let mut commits: BTreeMap<String, DiffCommitMetadata> = BTreeMap::new();

    let added_lines_by_file = collect_lines_by_file(hunks, LineSide::New);
    for (file_path, lines) in &added_lines_by_file {
        apply_blame_for_side(
            repo,
            file_path,
            file_path,
            lines,
            LineSide::New,
            from_commit,
            Some(to_commit),
            None,
            &mut annotations_by_file,
            &mut attributions,
            &mut line_details,
            &mut prompts,
            &mut sessions,
            &mut humans,
            &mut commits,
        );
    }

    if options.blame_deletions {
        let deleted_lines_by_blame_and_result = collect_old_lines_by_blame_and_result(hunks);
        for ((blame_file_path, result_file_path), lines) in deleted_lines_by_blame_and_result {
            apply_blame_for_side(
                repo,
                &blame_file_path,
                &result_file_path,
                &lines,
                LineSide::Old,
                from_commit,
                None,
                options.blame_deletions_since.clone(),
                &mut annotations_by_file,
                &mut attributions,
                &mut line_details,
                &mut prompts,
                &mut sessions,
                &mut humans,
                &mut commits,
            );
        }
    }

    Ok((
        annotations_by_file,
        attributions,
        line_details,
        prompts,
        sessions,
        humans,
        commits,
    ))
}

#[allow(clippy::type_complexity)]
fn build_line_attribution_from_note(
    to_commit: &str,
    hunks: &[DiffHunk],
    note: &AuthorshipLog,
) -> (
    BTreeMap<String, BTreeMap<String, Vec<LineRange>>>,
    HashMap<DiffLineKey, Attribution>,
    HashMap<DiffLineKey, LineAttributionDetail>,
    BTreeMap<String, PromptRecord>,
    BTreeMap<String, SessionRecord>,
    BTreeMap<String, HumanRecord>,
    BTreeMap<String, DiffCommitMetadata>,
) {
    let mut annotations_by_file: BTreeMap<String, BTreeMap<String, Vec<LineRange>>> =
        BTreeMap::new();
    let mut attributions: HashMap<DiffLineKey, Attribution> = HashMap::new();
    let mut line_details: HashMap<DiffLineKey, LineAttributionDetail> = HashMap::new();
    let prompts: BTreeMap<String, PromptRecord> = note.metadata.prompts.clone();
    let sessions: BTreeMap<String, SessionRecord> = note.metadata.sessions.clone();
    let humans: BTreeMap<String, HumanRecord> = note.metadata.humans.clone();
    let commits: BTreeMap<String, DiffCommitMetadata> = BTreeMap::new();

    let added_lines_by_file = collect_lines_by_file(hunks, LineSide::New);
    for (file_path, lines) in &added_lines_by_file {
        let file_attestation = note
            .attestations
            .iter()
            .find(|fa| &fa.file_path == file_path);

        let mut file_annotations: BTreeMap<String, Vec<LineRange>> = BTreeMap::new();

        for line in lines {
            let key = DiffLineKey {
                file: file_path.clone(),
                line: *line,
                side: LineSide::New,
            };

            let mut found_hash: Option<&str> = None;
            if let Some(fa) = file_attestation {
                for entry in &fa.entries {
                    if entry.line_ranges.iter().any(|r| r.contains(*line)) {
                        found_hash = Some(&entry.hash);
                        break;
                    }
                }
            }

            if let Some(hash) = found_hash {
                let is_prompt = prompts.contains_key(hash) || hash.starts_with("s_");
                let is_human = hash.starts_with("h_");

                let (prompt_id, human_id, attribution) = if is_prompt {
                    let tool = prompts
                        .get(hash)
                        .map(|p| p.agent_id.tool.clone())
                        .unwrap_or_else(|| {
                            let session_key = extract_session_id(hash);
                            sessions
                                .get(session_key)
                                .map(|s| s.agent_id.tool.clone())
                                .unwrap_or_else(|| "unknown".to_string())
                        });
                    (Some(hash.to_string()), None, Attribution::Ai(tool))
                } else if is_human {
                    (
                        None,
                        Some(hash.to_string()),
                        Attribution::Human(hash.to_string()),
                    )
                } else {
                    (None, None, Attribution::NoData)
                };

                if let Some(ref pid) = prompt_id {
                    file_annotations
                        .entry(pid.clone())
                        .or_default()
                        .push(LineRange::Single(*line));
                }

                attributions.insert(key.clone(), attribution);
                line_details.insert(
                    key,
                    LineAttributionDetail {
                        commit_sha: Some(to_commit.to_string()),
                        prompt_id,
                        human_id,
                    },
                );
            } else {
                attributions.insert(key.clone(), Attribution::NoData);
                line_details.insert(
                    key,
                    LineAttributionDetail {
                        commit_sha: Some(to_commit.to_string()),
                        prompt_id: None,
                        human_id: None,
                    },
                );
            }
        }

        if !file_annotations.is_empty() {
            annotations_by_file.insert(file_path.clone(), file_annotations);
        }
    }

    // Deleted lines: include with NoData attribution (no blame)
    // Use file_path (new name) for DiffLineKey to match build_json_hunk_segments lookup
    for hunk in hunks {
        for line in &hunk.deleted_lines {
            let key = DiffLineKey {
                file: hunk.file_path.clone(),
                line: *line,
                side: LineSide::Old,
            };
            attributions.insert(key.clone(), Attribution::NoData);
            line_details.insert(
                key,
                LineAttributionDetail {
                    commit_sha: None,
                    prompt_id: None,
                    human_id: None,
                },
            );
        }
    }

    (
        annotations_by_file,
        attributions,
        line_details,
        prompts,
        sessions,
        humans,
        commits,
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_blame_for_side(
    repo: &Repository,
    blame_file_path: &str,
    result_file_path: &str,
    lines: &[u32],
    side: LineSide,
    from_commit: &str,
    newest_commit: Option<&str>,
    oldest_date_spec: Option<String>,
    annotations_by_file: &mut BTreeMap<String, BTreeMap<String, Vec<LineRange>>>,
    attributions: &mut HashMap<DiffLineKey, Attribution>,
    line_details: &mut HashMap<DiffLineKey, LineAttributionDetail>,
    prompts: &mut BTreeMap<String, PromptRecord>,
    sessions: &mut BTreeMap<String, SessionRecord>,
    humans: &mut BTreeMap<String, HumanRecord>,
    commits: &mut BTreeMap<String, DiffCommitMetadata>,
) {
    if lines.is_empty() {
        return;
    }

    let line_ranges = lines_to_ranges(lines);
    if line_ranges.is_empty() {
        return;
    }

    let mut blame_options = GitAiBlameOptions {
        line_ranges,
        no_output: true,
        use_prompt_hashes_as_names: true,
        newest_commit: Some(newest_commit.unwrap_or(from_commit).to_string()),
        ..GitAiBlameOptions::default()
    };
    if matches!(side, LineSide::New) {
        blame_options.oldest_commit = Some(from_commit.to_string());
    } else {
        blame_options.oldest_date_spec = oldest_date_spec;
    }

    let analysis = match repo.blame_analysis(blame_file_path, &blame_options) {
        Ok(analysis) => analysis,
        Err(_) => {
            for line in lines {
                attributions.insert(
                    DiffLineKey {
                        file: result_file_path.to_string(),
                        line: *line,
                        side: side.clone(),
                    },
                    Attribution::NoData,
                );
            }
            return;
        }
    };

    for (prompt_id, prompt_record) in &analysis.prompt_records {
        if prompt_id.starts_with("s_") {
            // Session-format attestation: look up the SessionRecord from blame analysis
            let session_key = extract_session_id(prompt_id);
            if let Some(session_record) = analysis.session_records.get(session_key) {
                sessions
                    .entry(session_key.to_string())
                    .or_insert_with(|| session_record.clone());
            } else {
                // Fallback: convert PromptRecord back to SessionRecord
                sessions
                    .entry(session_key.to_string())
                    .or_insert_with(|| SessionRecord {
                        agent_id: prompt_record.agent_id.clone(),
                        human_author: prompt_record.human_author.clone(),
                        custom_attributes: prompt_record.custom_attributes.clone(),
                    });
            }
        } else {
            prompts
                .entry(prompt_id.clone())
                .or_insert_with(|| prompt_record.clone());
        }
    }

    for (human_id, human_record) in &analysis.humans {
        humans
            .entry(human_id.clone())
            .or_insert_with(|| human_record.clone());
    }

    let mut line_to_commit: HashMap<u32, String> = HashMap::new();
    for blame_hunk in &analysis.blame_hunks {
        ensure_commit_metadata(repo, &blame_hunk.commit_sha, commits);
        for line in blame_hunk.range.0..=blame_hunk.range.1 {
            line_to_commit.insert(line, blame_hunk.commit_sha.clone());
        }
    }

    let mut lines_by_prompt_id: HashMap<String, Vec<u32>> = HashMap::new();

    for line in lines {
        let key = DiffLineKey {
            file: result_file_path.to_string(),
            line: *line,
            side: side.clone(),
        };

        if let Some(author_marker) = analysis.line_authors.get(line) {
            let prompt_id = if analysis.prompt_records.contains_key(author_marker) {
                Some(author_marker.clone())
            } else {
                None
            };

            let human_id = if author_marker.starts_with("h_") {
                Some(author_marker.clone())
            } else {
                None
            };

            let attribution = if let Some(ref id) = prompt_id {
                let tool = analysis
                    .prompt_records
                    .get(id)
                    .map(|prompt| prompt.agent_id.tool.clone())
                    .unwrap_or_else(|| "unknown".to_string());
                lines_by_prompt_id
                    .entry(id.clone())
                    .or_default()
                    .push(*line);
                Attribution::Ai(tool)
            } else if author_marker.starts_with("h_") {
                // Known human attestation (h_-prefixed hash from KnownHuman checkpoint)
                Attribution::Human(author_marker.clone())
            } else {
                // Legacy or unrecognized marker (e.g. "human") — treat as unattested
                Attribution::NoData
            };
            attributions.insert(key.clone(), attribution);
            line_details.insert(
                key,
                LineAttributionDetail {
                    commit_sha: line_to_commit.get(line).cloned(),
                    prompt_id,
                    human_id,
                },
            );
        } else {
            attributions.insert(key.clone(), Attribution::NoData);
            line_details.insert(
                key,
                LineAttributionDetail {
                    commit_sha: None,
                    prompt_id: None,
                    human_id: None,
                },
            );
        }
    }

    if matches!(side, LineSide::New) {
        let file_annotations = annotations_by_file
            .entry(result_file_path.to_string())
            .or_default();
        for (prompt_id, mut prompt_lines) in lines_by_prompt_id {
            prompt_lines.sort_unstable();
            prompt_lines.dedup();
            file_annotations.insert(prompt_id, LineRange::compress_lines(&prompt_lines));
        }
    }
}

/// Convert a sorted list of line numbers to contiguous ranges
/// e.g., [1, 2, 3, 5, 6, 10] -> [(1, 3), (5, 6), (10, 10)]
fn lines_to_ranges(lines: &[u32]) -> Vec<(u32, u32)> {
    if lines.is_empty() {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut start = lines[0];
    let mut end = lines[0];

    for &line in &lines[1..] {
        if line == end + 1 {
            // Contiguous, extend the range
            end = line;
        } else {
            // Gap found, save current range and start new one
            ranges.push((start, end));
            start = line;
            end = line;
        }
    }

    // Don't forget the last range
    ranges.push((start, end));

    ranges
}

fn collect_lines_by_file(hunks: &[DiffHunk], side: LineSide) -> HashMap<String, Vec<u32>> {
    let mut lines_by_file: HashMap<String, Vec<u32>> = HashMap::new();
    for hunk in hunks {
        let lines = match side {
            LineSide::Old => &hunk.deleted_lines,
            LineSide::New => &hunk.added_lines,
        };
        if lines.is_empty() {
            continue;
        }
        let key = match side {
            LineSide::Old => hunk.old_file_path.as_deref().unwrap_or(&hunk.file_path),
            LineSide::New => &hunk.file_path,
        };
        lines_by_file
            .entry(key.to_string())
            .or_default()
            .extend(lines.iter().copied());
    }

    for lines in lines_by_file.values_mut() {
        lines.sort_unstable();
        lines.dedup();
    }

    lines_by_file
}

fn collect_old_lines_by_blame_and_result(
    hunks: &[DiffHunk],
) -> HashMap<(String, String), Vec<u32>> {
    let mut lines_by_file_pair: HashMap<(String, String), Vec<u32>> = HashMap::new();

    for hunk in hunks {
        if hunk.deleted_lines.is_empty() {
            continue;
        }

        let blame_file = hunk
            .old_file_path
            .clone()
            .unwrap_or_else(|| hunk.file_path.clone());
        let result_file = hunk.file_path.clone();
        lines_by_file_pair
            .entry((blame_file, result_file))
            .or_default()
            .extend(hunk.deleted_lines.iter().copied());
    }

    for lines in lines_by_file_pair.values_mut() {
        lines.sort_unstable();
        lines.dedup();
    }

    lines_by_file_pair
}

fn build_line_content_map(hunks: &[DiffHunk]) -> HashMap<DiffLineKey, String> {
    let mut content_map = HashMap::new();

    for hunk in hunks {
        for (line, content) in hunk.deleted_lines.iter().zip(hunk.deleted_contents.iter()) {
            content_map.insert(
                DiffLineKey {
                    file: hunk.file_path.clone(),
                    line: *line,
                    side: LineSide::Old,
                },
                content.clone(),
            );
        }
        for (line, content) in hunk.added_lines.iter().zip(hunk.added_contents.iter()) {
            content_map.insert(
                DiffLineKey {
                    file: hunk.file_path.clone(),
                    line: *line,
                    side: LineSide::New,
                },
                content.clone(),
            );
        }
    }

    content_map
}

fn build_json_hunks(
    repo: &Repository,
    diff_hunks: &[DiffHunk],
    line_details: &HashMap<DiffLineKey, LineAttributionDetail>,
    line_contents: &HashMap<DiffLineKey, String>,
    diff_to_commit: &str,
    commits: &mut BTreeMap<String, DiffCommitMetadata>,
) -> Result<Vec<DiffJsonHunk>, GitAiError> {
    let mut hunks: Vec<DiffJsonHunk> = Vec::new();

    for diff_hunk in diff_hunks {
        hunks.extend(build_json_hunk_segments(
            repo,
            diff_hunk,
            LineSide::New,
            "addition",
            diff_to_commit,
            line_details,
            line_contents,
            commits,
        )?);
        hunks.extend(build_json_hunk_segments(
            repo,
            diff_hunk,
            LineSide::Old,
            "deletion",
            diff_to_commit,
            line_details,
            line_contents,
            commits,
        )?);
    }

    Ok(hunks)
}

#[allow(clippy::too_many_arguments)]
fn build_json_hunk_segments(
    repo: &Repository,
    diff_hunk: &DiffHunk,
    side: LineSide,
    kind: &str,
    diff_to_commit: &str,
    line_details: &HashMap<DiffLineKey, LineAttributionDetail>,
    line_contents: &HashMap<DiffLineKey, String>,
    commits: &mut BTreeMap<String, DiffCommitMetadata>,
) -> Result<Vec<DiffJsonHunk>, GitAiError> {
    let lines = match side {
        LineSide::Old => &diff_hunk.deleted_lines,
        LineSide::New => &diff_hunk.added_lines,
    };
    if lines.is_empty() {
        return Ok(Vec::new());
    }

    let mut segments: Vec<DiffJsonHunk> = Vec::new();
    let mut current_start = 0u32;
    let mut current_end = 0u32;
    let mut current_prompt_id: Option<String> = None;
    let mut current_human_id: Option<String> = None;
    let mut current_original_commit_sha: Option<String> = None;
    let mut current_commit_sha = String::new();
    let mut current_contents: Vec<String> = Vec::new();

    let flush = |segments: &mut Vec<DiffJsonHunk>,
                 current_start: &mut u32,
                 current_end: &mut u32,
                 current_prompt_id: &mut Option<String>,
                 current_human_id: &mut Option<String>,
                 current_original_commit_sha: &mut Option<String>,
                 current_commit_sha: &mut String,
                 current_contents: &mut Vec<String>| {
        if *current_start == 0 {
            return;
        }
        let content_hash = hash_hunk_content(current_contents);
        let session_id = current_prompt_id.as_ref().and_then(|id| {
            if id.starts_with("s_") {
                Some(extract_session_id(id).to_string())
            } else {
                None
            }
        });
        segments.push(DiffJsonHunk {
            commit_sha: current_commit_sha.clone(),
            content_hash,
            hunk_kind: kind.to_string(),
            original_commit_sha: current_original_commit_sha.clone(),
            start_line: *current_start,
            end_line: *current_end,
            file_path: diff_hunk.file_path.clone(),
            prompt_id: current_prompt_id.clone(),
            session_id,
            human_id: current_human_id.clone(),
        });
        *current_start = 0;
        *current_end = 0;
        *current_prompt_id = None;
        *current_human_id = None;
        *current_original_commit_sha = None;
        current_commit_sha.clear();
        current_contents.clear();
    };

    for line in lines {
        let key = DiffLineKey {
            file: diff_hunk.file_path.clone(),
            line: *line,
            side: side.clone(),
        };
        let detail = line_details.get(&key);
        let prompt_id = detail.and_then(|d| d.prompt_id.clone());
        let human_id = detail.and_then(|d| d.human_id.clone());
        let original_commit_sha = if matches!(side, LineSide::Old) {
            detail.and_then(|d| d.commit_sha.clone())
        } else {
            None
        };
        let commit_sha = if matches!(side, LineSide::Old) {
            diff_to_commit.to_string()
        } else {
            detail
                .and_then(|d| d.commit_sha.clone())
                .unwrap_or_else(|| diff_to_commit.to_string())
        };

        if let Some(ref original_sha) = original_commit_sha {
            ensure_commit_metadata(repo, original_sha, commits);
        }
        ensure_commit_metadata(repo, &commit_sha, commits);

        let can_extend = current_start != 0
            && *line == current_end + 1
            && prompt_id == current_prompt_id
            && human_id == current_human_id
            && original_commit_sha == current_original_commit_sha
            && commit_sha == current_commit_sha;

        if !can_extend {
            flush(
                &mut segments,
                &mut current_start,
                &mut current_end,
                &mut current_prompt_id,
                &mut current_human_id,
                &mut current_original_commit_sha,
                &mut current_commit_sha,
                &mut current_contents,
            );
            current_start = *line;
            current_end = *line;
            current_prompt_id = prompt_id.clone();
            current_human_id = human_id.clone();
            current_original_commit_sha = original_commit_sha.clone();
            current_commit_sha = commit_sha;
        } else {
            current_end = *line;
        }

        current_contents.push(line_contents.get(&key).cloned().unwrap_or_default());
    }

    flush(
        &mut segments,
        &mut current_start,
        &mut current_end,
        &mut current_prompt_id,
        &mut current_human_id,
        &mut current_original_commit_sha,
        &mut current_commit_sha,
        &mut current_contents,
    );

    Ok(segments)
}

fn hash_hunk_content(lines: &[String]) -> String {
    let joined = lines.join("\n");
    let mut hasher = Sha256::new();
    hasher.update(joined.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn ensure_commit_metadata(
    repo: &Repository,
    commit_sha: &str,
    commits: &mut BTreeMap<String, DiffCommitMetadata>,
) {
    if commits.contains_key(commit_sha) {
        return;
    }
    if let Ok(metadata) = load_commit_metadata(repo, commit_sha) {
        commits.insert(commit_sha.to_string(), metadata);
    }
}

fn load_commit_metadata(
    repo: &Repository,
    commit_sha: &str,
) -> Result<DiffCommitMetadata, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.push("show".to_string());
    args.push("-s".to_string());
    args.push("--no-notes".to_string());
    args.push("--encoding=UTF-8".to_string());
    args.push("--format=%an%x00%ae%x00%aI%x00%s%x00%B".to_string());
    args.push(commit_sha.to_string());

    let output = exec_git_with_profile(&args, InternalGitProfile::General)?;
    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| GitAiError::Generic(format!("Failed to parse commit metadata: {}", e)))?;
    let mut parts = stdout.splitn(5, '\0');
    let author_name = parts.next().unwrap_or("").trim();
    let author_email = parts.next().unwrap_or("").trim();
    let authored_time = parts.next().unwrap_or("").trim().to_string();
    let msg = parts.next().unwrap_or("").trim().to_string();
    let full_msg = parts.next().unwrap_or("").trim_end().to_string();
    let author = format_git_ident(author_name, author_email);
    let authorship_note = read_note(repo, commit_sha);

    Ok(DiffCommitMetadata {
        authored_time,
        msg,
        full_msg,
        author,
        authorship_note,
    })
}

fn format_git_ident(name: &str, email: &str) -> String {
    if !name.is_empty() && !email.is_empty() {
        format!("{} <{}>", name, email)
    } else if !name.is_empty() {
        name.to_string()
    } else if !email.is_empty() {
        format!("<{}>", email)
    } else {
        String::new()
    }
}

fn merge_missing_prompts_and_sessions_from_authorship_note(
    repo: &Repository,
    commit_sha: &str,
    prompts: &mut BTreeMap<String, PromptRecord>,
    sessions: &mut BTreeMap<String, SessionRecord>,
) {
    if let Some(authorship_log) = read_authorship(repo, commit_sha) {
        for (prompt_id, prompt_record) in &authorship_log.metadata.prompts {
            prompts
                .entry(prompt_id.clone())
                .or_insert_with(|| prompt_record.clone());
        }
        // Insert session records keyed by session ID only (s_xxx)
        for file_attestation in &authorship_log.attestations {
            for entry in &file_attestation.entries {
                if entry.hash.starts_with("s_") {
                    let session_key = extract_session_id(&entry.hash);
                    if let Some(session_record) = authorship_log.metadata.sessions.get(session_key)
                    {
                        sessions
                            .entry(session_key.to_string())
                            .or_insert_with(|| session_record.clone());
                    }
                }
            }
        }
    }
}

/// Extract session ID from a combined session::trace ID
/// For session-format IDs like "s_xxx::t_yyy", returns "s_xxx"
/// For other IDs, returns the original ID unchanged
fn extract_session_id(id: &str) -> &str {
    if id.starts_with("s_") {
        id.split("::").next().unwrap_or(id)
    } else {
        id
    }
}

fn line_range_len(range: &LineRange) -> u32 {
    match range {
        LineRange::Single(_) => 1,
        LineRange::Range(start, end) => end.saturating_sub(*start) + 1,
    }
}

fn calculate_diff_commit_stats(
    artifacts: &DiffBuildArtifacts,
    prompts: &BTreeMap<String, PromptRecord>,
    sessions: &BTreeMap<String, SessionRecord>,
) -> DiffCommitStats {
    let mut stats = DiffCommitStats::default();

    for annotations in artifacts.annotations_by_file.values() {
        for (prompt_id, ranges) in annotations {
            let landed_lines = ranges.iter().map(line_range_len).sum::<u32>();
            stats.ai_lines_added += landed_lines;
            let session_key = extract_session_id(prompt_id);
            let key = prompts
                .get(prompt_id)
                .map(|r| &r.agent_id)
                .or_else(|| sessions.get(session_key).map(|r| &r.agent_id))
                .map(|agent_id| format!("{}::{}", agent_id.tool, agent_id.model));
            if let Some(key) = key {
                let tool_stats = stats.tool_model_breakdown.entry(key).or_default();
                tool_stats.ai_lines_added += landed_lines;
            }
        }
    }

    for (line_key, attribution) in &artifacts.attributions {
        if !matches!(line_key.side, LineSide::New) {
            continue;
        }
        match attribution {
            Attribution::Human(_) => stats.human_lines_added += 1,
            Attribution::NoData => stats.unknown_lines_added += 1,
            Attribution::Ai(_) => {}
        }
    }
    stats.git_lines_added =
        stats.ai_lines_added + stats.human_lines_added + stats.unknown_lines_added;

    for hunk in &artifacts.json_hunks {
        if hunk.hunk_kind == "deletion" {
            stats.git_lines_deleted += hunk.end_line.saturating_sub(hunk.start_line) + 1;
        }
    }

    stats
}

// ============================================================================
// JSON Output Building
// ============================================================================

/// Build the DiffJson structure for --json output
fn build_diff_json(
    repo: &Repository,
    from_commit: &str,
    to_commit: &str,
    artifacts: &DiffBuildArtifacts,
    prompts: &BTreeMap<String, PromptRecord>,
    sessions: &BTreeMap<String, SessionRecord>,
    commit_stats: Option<DiffCommitStats>,
) -> Result<DiffJson, GitAiError> {
    let mut files: BTreeMap<String, FileDiffJson> = BTreeMap::new();
    let file_diffs = get_diff_split_by_file(repo, from_commit, to_commit)?;
    let mut files_sorted: Vec<&String> = artifacts.included_files.iter().collect();
    files_sorted.sort();

    for file_path in files_sorted {
        let diff = file_diffs.get(file_path).cloned().unwrap_or_default();

        let base_content = match repo.get_file_content(file_path, from_commit) {
            Ok(bytes) => String::from_utf8(bytes).unwrap_or_default(),
            Err(_) => String::new(),
        };
        let annotations = artifacts
            .annotations_by_file
            .get(file_path)
            .cloned()
            .unwrap_or_default();

        files.insert(
            file_path.clone(),
            FileDiffJson {
                annotations,
                diff,
                base_content,
            },
        );
    }

    Ok(DiffJson {
        files,
        prompts: prompts.clone(),
        sessions: sessions.clone(),
        humans: artifacts.humans.clone(),
        hunks: artifacts.json_hunks.clone(),
        commits: artifacts.commits.clone(),
        commit_stats,
    })
}

/// Get the unified diff split by file path
fn get_diff_split_by_file(
    repo: &Repository,
    from_commit: &str,
    to_commit: &str,
) -> Result<HashMap<String, String>, GitAiError> {
    let sections = get_diff_sections_by_file(repo, from_commit, to_commit)?;
    let mut file_diffs: HashMap<String, String> = HashMap::new();
    for (file_path, diff_text) in sections {
        file_diffs.insert(file_path, diff_text);
    }
    Ok(file_diffs)
}

fn get_diff_sections_by_file(
    repo: &Repository,
    from_commit: &str,
    to_commit: &str,
) -> Result<Vec<(String, String)>, GitAiError> {
    let diff_text = get_diff_text(repo, from_commit, to_commit, false)?;
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current_file = String::new();
    let mut current_diff = String::new();
    let mut current_old_file: Option<String> = None;
    let mut in_hunk = false;

    let flush_section = |sections: &mut Vec<(String, String)>,
                         current_file: &mut String,
                         current_diff: &mut String| {
        if !current_file.is_empty() && !current_diff.is_empty() {
            sections.push((current_file.clone(), current_diff.clone()));
        }
        current_file.clear();
        current_diff.clear();
    };

    for line in diff_text.lines() {
        if line.starts_with("diff --git ") {
            flush_section(&mut sections, &mut current_file, &mut current_diff);
            if let Some((old_file, new_file)) = parse_diff_git_header_paths(line) {
                current_old_file = Some(old_file);
                current_file = new_file;
            } else {
                current_old_file = None;
                current_file.clear();
            }
            in_hunk = false;
            current_diff.push_str(line);
            current_diff.push('\n');
            continue;
        }

        if current_diff.is_empty() {
            continue;
        }

        current_diff.push_str(line);
        current_diff.push('\n');

        if line.starts_with("@@ ") {
            in_hunk = true;
            continue;
        }

        if !in_hunk {
            if let Some(path_opt) = parse_old_file_path_from_minus_header_line(line) {
                current_old_file = path_opt.clone();
                if current_file.is_empty() {
                    current_file = path_opt.unwrap_or_default();
                }
                continue;
            }

            if let Some(path_opt) = parse_new_file_path_from_plus_header_line(line) {
                current_file = path_opt
                    .or_else(|| current_old_file.clone())
                    .unwrap_or_default();
                continue;
            }
        }
    }

    flush_section(&mut sections, &mut current_file, &mut current_diff);

    // Exclude binary files from diff output — git emits "Binary files ... differ"
    // lines for these, and they carry no useful text hunks.
    sections.retain(|(_, section_text)| !is_binary_diff_section(section_text));

    Ok(sections)
}

/// Returns `true` when a diff section produced by git describes a binary file.
/// Git emits a line starting with "Binary files" instead of unified-diff hunks
/// for files it considers binary.
fn is_binary_diff_section(section_text: &str) -> bool {
    section_text
        .lines()
        .any(|line| line.starts_with("Binary files"))
}

// ============================================================================
// Output Formatting
// ============================================================================

#[allow(clippy::if_same_then_else)]
pub fn format_annotated_diff(
    repo: &Repository,
    from_commit: &str,
    to_commit: &str,
    attributions: &HashMap<DiffLineKey, Attribution>,
    humans: &BTreeMap<String, HumanRecord>,
    included_files: &HashSet<String>,
) -> Result<String, GitAiError> {
    let sections = get_diff_sections_by_file(repo, from_commit, to_commit)?;
    let use_color = std::io::stdout().is_terminal();
    let mut result = String::new();

    for (file_path, section_text) in sections {
        if !included_files.contains(&file_path) {
            continue;
        }

        let mut old_line_num = 0u32;
        let mut new_line_num = 0u32;
        let mut in_hunk = false;

        for line in section_text.lines() {
            if is_diff_header_line(line, in_hunk) {
                if line.starts_with("diff --git") {
                    in_hunk = false;
                }
                result.push_str(&format_line(
                    line,
                    LineType::DiffHeader,
                    use_color,
                    None,
                    humans,
                ));
            } else if line.starts_with("@@ ") {
                in_hunk = true;
                if let Some((old_start, new_start)) = parse_hunk_header_for_line_nums(line) {
                    old_line_num = old_start;
                    new_line_num = new_start;
                }
                result.push_str(&format_line(
                    line,
                    LineType::HunkHeader,
                    use_color,
                    None,
                    humans,
                ));
            } else if in_hunk && line.starts_with('-') {
                let key = DiffLineKey {
                    file: file_path.clone(),
                    line: old_line_num,
                    side: LineSide::Old,
                };
                let attribution = attributions.get(&key);
                result.push_str(&format_line(
                    line,
                    LineType::Deletion,
                    use_color,
                    attribution,
                    humans,
                ));
                old_line_num += 1;
            } else if in_hunk && line.starts_with('+') {
                let key = DiffLineKey {
                    file: file_path.clone(),
                    line: new_line_num,
                    side: LineSide::New,
                };
                let attribution = attributions.get(&key);
                result.push_str(&format_line(
                    line,
                    LineType::Addition,
                    use_color,
                    attribution,
                    humans,
                ));
                new_line_num += 1;
            } else if in_hunk && line.starts_with(' ') {
                result.push_str(&format_line(
                    line,
                    LineType::Context,
                    use_color,
                    None,
                    humans,
                ));
                old_line_num += 1;
                new_line_num += 1;
            } else if line.starts_with("Binary files") {
                result.push_str(&format_line(
                    line,
                    LineType::Binary,
                    use_color,
                    None,
                    humans,
                ));
            } else {
                result.push_str(&format_line(
                    line,
                    LineType::Context,
                    use_color,
                    None,
                    humans,
                ));
            }
        }
    }

    Ok(result)
}

fn is_diff_header_line(line: &str, in_hunk: bool) -> bool {
    line.starts_with("diff --git")
        || line.starts_with("index ")
        || (!in_hunk && (line.starts_with("--- ") || line.starts_with("+++ ")))
}

fn parse_hunk_header_for_line_nums(line: &str) -> Option<(u32, u32)> {
    // Parse @@ -old_start,old_count +new_start,new_count @@
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 3 {
        return None;
    }

    let old_part = parts[1];
    let new_part = parts[2];

    // Extract old_start
    let old_start = if let Some(old_str) = old_part.strip_prefix('-') {
        if let Some((start_str, _)) = old_str.split_once(',') {
            start_str.parse::<u32>().ok()?
        } else {
            old_str.parse::<u32>().ok()?
        }
    } else {
        return None;
    };

    // Extract new_start
    let new_start = if let Some(new_str) = new_part.strip_prefix('+') {
        if let Some((start_str, _)) = new_str.split_once(',') {
            start_str.parse::<u32>().ok()?
        } else {
            new_str.parse::<u32>().ok()?
        }
    } else {
        return None;
    };

    Some((old_start, new_start))
}

#[derive(Debug)]
enum LineType {
    DiffHeader,
    HunkHeader,
    Addition,
    Deletion,
    Context,
    Binary,
}

fn format_line(
    line: &str,
    line_type: LineType,
    use_color: bool,
    attribution: Option<&Attribution>,
    humans: &BTreeMap<String, HumanRecord>,
) -> String {
    let annotation = if let Some(attr) = attribution {
        format_attribution(attr, humans)
    } else {
        String::new()
    };

    if use_color {
        match line_type {
            LineType::DiffHeader => {
                format!("\x1b[1m{}\x1b[0m\n", line) // Bold
            }
            LineType::HunkHeader => {
                format!("\x1b[36m{}\x1b[0m\n", line) // Cyan
            }
            LineType::Addition => {
                if annotation.is_empty() {
                    format!("\x1b[32m{}\x1b[0m\n", line) // Green
                } else {
                    format!("\x1b[32m{}\x1b[0m  \x1b[2m{}\x1b[0m\n", line, annotation) // Green + dim annotation
                }
            }
            LineType::Deletion => {
                if annotation.is_empty() {
                    format!("\x1b[31m{}\x1b[0m\n", line) // Red
                } else {
                    format!("\x1b[31m{}\x1b[0m  \x1b[2m{}\x1b[0m\n", line, annotation) // Red + dim annotation
                }
            }
            LineType::Context | LineType::Binary => {
                format!("{}\n", line)
            }
        }
    } else {
        // No color
        if annotation.is_empty() {
            format!("{}\n", line)
        } else {
            format!("{}  {}\n", line, annotation)
        }
    }
}

fn format_attribution(attribution: &Attribution, humans: &BTreeMap<String, HumanRecord>) -> String {
    match attribution {
        Attribution::Ai(tool) => format!("🤖{}", tool),
        Attribution::Human(human_id) => {
            // Resolve human_id (h_-prefixed hash) to actual author name
            if let Some(human_record) = humans.get(human_id) {
                format!("👤{}", human_record.author)
            } else {
                // Fallback to showing the ID if not found in humans map
                format!("👤{}", human_id)
            }
        }
        Attribution::NoData => "[no-data]".to_string(),
    }
}

/// Custom serializer for annotations that converts LineRange to JSON tuples
fn serialize_annotations<S>(
    annotations: &BTreeMap<String, Vec<LineRange>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use serde::ser::SerializeMap;
    let mut map = serializer.serialize_map(Some(annotations.len()))?;
    for (key, ranges) in annotations {
        let json_ranges: Vec<serde_json::Value> = ranges
            .iter()
            .map(|range| match range {
                LineRange::Single(line) => serde_json::Value::Number((*line).into()),
                LineRange::Range(start, end) => serde_json::Value::Array(vec![
                    serde_json::Value::Number((*start).into()),
                    serde_json::Value::Number((*end).into()),
                ]),
            })
            .collect();
        map.serialize_entry(key, &json_ranges)?;
    }
    map.end()
}

// ============================================================================
// Filtered Diff for Bundle Sharing
// ============================================================================

/// Options for getting a diff with optional filtering
#[derive(Default)]
pub struct DiffOptions {
    /// If provided, only include files with attributions from these prompts
    pub prompt_ids: Option<Vec<String>>,
    /// Whether to filter files to only those with attributions from prompt_ids
    pub filter_to_attributed_files: bool,
}

/// Get diff JSON for a single commit with optional filtering by prompt attributions
///
/// This function is designed for bundle sharing:
/// - If `options.filter_to_attributed_files` is true, only includes files that have
///   attributions from the specified `prompt_ids`
/// - If `options.prompt_ids` is Some, filters the returned prompts to only those IDs
pub fn get_diff_json_filtered(
    repo: &Repository,
    commit_sha: &str,
    options: DiffOptions,
) -> Result<DiffJson, GitAiError> {
    // Resolve the commit to get from/to SHAs (parent -> commit)
    let to_commit = resolve_commit(repo, commit_sha)?;
    let from_commit = resolve_parent(repo, &to_commit)?;

    let artifacts = build_diff_artifacts(
        repo,
        &from_commit,
        &to_commit,
        &DiffCommandOptions {
            format: DiffFormat::Json,
            ..DiffCommandOptions::default()
        },
    )?;

    let mut diff_json = build_diff_json(
        repo,
        &from_commit,
        &to_commit,
        &artifacts,
        &artifacts.prompts,
        &artifacts.sessions,
        None,
    )?;

    // Apply filtering if requested
    if options.filter_to_attributed_files
        && let Some(ref prompt_ids) = options.prompt_ids
    {
        let prompt_id_set: HashSet<&String> = prompt_ids.iter().collect();

        // Filter files to only those with attributions from the specified prompts
        diff_json.files.retain(|_file_path, file_diff| {
            // Check if any annotation key matches a prompt_id
            file_diff
                .annotations
                .keys()
                .any(|key| prompt_id_set.contains(key))
        });

        let kept_files: HashSet<String> = diff_json.files.keys().cloned().collect();
        diff_json
            .hunks
            .retain(|hunk| kept_files.contains(&hunk.file_path));
    }

    // Filter prompts/sessions to only those specified (if any)
    if let Some(ref prompt_ids) = options.prompt_ids {
        let prompt_id_set: HashSet<&String> = prompt_ids.iter().collect();
        diff_json
            .prompts
            .retain(|key, _| prompt_id_set.contains(key));
        // Session keys are session IDs only, but prompt_ids may contain combined IDs
        // Extract session IDs from prompt_ids for session filtering
        let session_id_set: HashSet<&str> =
            prompt_ids.iter().map(|id| extract_session_id(id)).collect();
        diff_json
            .sessions
            .retain(|key, _| session_id_set.contains(key.as_str()));
    }

    let mut referenced_commit_shas: HashSet<String> = HashSet::new();
    for hunk in &diff_json.hunks {
        referenced_commit_shas.insert(hunk.commit_sha.clone());
        if let Some(original) = &hunk.original_commit_sha {
            referenced_commit_shas.insert(original.clone());
        }
    }
    diff_json
        .commits
        .retain(|sha, _| referenced_commit_shas.contains(sha));

    Ok(diff_json)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::working_log::AgentId;
    use std::collections::{BTreeMap, HashMap, HashSet};

    #[test]
    fn test_parse_diff_args_single_commit() {
        let args = vec!["abc123".to_string()];
        let parsed = parse_diff_args(&args).unwrap();

        match parsed.spec {
            DiffSpec::SingleCommit(sha) => {
                assert_eq!(sha, "abc123");
            }
            _ => panic!("Expected SingleCommit"),
        }

        assert!(matches!(
            parsed.options.format,
            DiffFormat::GitCompatibleTerminal
        ));
        assert!(!parsed.options.blame_deletions);
        assert!(parsed.options.blame_deletions_since.is_none());
        assert!(!parsed.options.include_stats);
        assert!(!parsed.options.all_prompts);
    }

    #[test]
    fn test_parse_diff_args_commit_range() {
        let args = vec!["abc123..def456".to_string()];
        let parsed = parse_diff_args(&args).unwrap();

        match parsed.spec {
            DiffSpec::TwoCommit(start, end) => {
                assert_eq!(start, "abc123");
                assert_eq!(end, "def456");
            }
            _ => panic!("Expected TwoCommit"),
        }
    }

    #[test]
    fn test_parse_diff_args_two_positional_commits() {
        let args = vec!["abc123".to_string(), "def456".to_string()];
        let parsed = parse_diff_args(&args).unwrap();

        match parsed.spec {
            DiffSpec::TwoCommit(start, end) => {
                assert_eq!(start, "abc123");
                assert_eq!(end, "def456");
            }
            _ => panic!("Expected TwoCommit"),
        }
    }

    #[test]
    fn test_parse_diff_args_two_positional_commits_with_json() {
        let args = vec![
            "abc123".to_string(),
            "def456".to_string(),
            "--json".to_string(),
        ];
        let parsed = parse_diff_args(&args).unwrap();

        match parsed.spec {
            DiffSpec::TwoCommit(start, end) => {
                assert_eq!(start, "abc123");
                assert_eq!(end, "def456");
            }
            _ => panic!("Expected TwoCommit"),
        }

        assert!(matches!(parsed.options.format, DiffFormat::Json));
    }

    #[test]
    fn test_parse_diff_args_include_stats_requires_json() {
        let args = vec!["abc123".to_string(), "--include-stats".to_string()];
        let result = parse_diff_args(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_diff_args_include_stats_single_commit_json() {
        let args = vec![
            "abc123".to_string(),
            "--json".to_string(),
            "--include-stats".to_string(),
        ];
        let parsed = parse_diff_args(&args).unwrap();
        assert!(matches!(parsed.spec, DiffSpec::SingleCommit(_)));
        assert!(matches!(parsed.options.format, DiffFormat::Json));
        assert!(parsed.options.include_stats);
    }

    #[test]
    fn test_parse_diff_args_include_stats_rejects_ranges() {
        let args = vec![
            "abc123..def456".to_string(),
            "--json".to_string(),
            "--include-stats".to_string(),
        ];
        let result = parse_diff_args(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_diff_args_all_prompts_requires_json() {
        let args = vec!["abc123".to_string(), "--all-prompts".to_string()];
        let result = parse_diff_args(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_diff_args_all_prompts_single_commit_json() {
        let args = vec![
            "abc123".to_string(),
            "--json".to_string(),
            "--all-prompts".to_string(),
        ];
        let parsed = parse_diff_args(&args).unwrap();
        assert!(matches!(parsed.spec, DiffSpec::SingleCommit(_)));
        assert!(matches!(parsed.options.format, DiffFormat::Json));
        assert!(parsed.options.all_prompts);
    }

    #[test]
    fn test_parse_diff_args_all_prompts_rejects_ranges() {
        let args = vec![
            "abc123..def456".to_string(),
            "--json".to_string(),
            "--all-prompts".to_string(),
        ];
        let result = parse_diff_args(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_diff_args_blame_deletions_flags() {
        let args = vec![
            "abc123".to_string(),
            "--blame-deletions".to_string(),
            "--blame-deletions-since".to_string(),
            "2 weeks ago".to_string(),
        ];
        let parsed = parse_diff_args(&args).unwrap();
        assert!(parsed.options.blame_deletions);
        assert_eq!(
            parsed.options.blame_deletions_since,
            Some("2 weeks ago".to_string())
        );
    }

    #[test]
    fn test_parse_diff_args_blame_deletions_since_requires_blame_deletions() {
        let args = vec![
            "abc123".to_string(),
            "--blame-deletions-since".to_string(),
            "2026-01-01".to_string(),
        ];
        let result = parse_diff_args(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_diff_args_too_many_positional_args() {
        let args = vec![
            "abc123".to_string(),
            "def456".to_string(),
            "ghi789".to_string(),
        ];
        let result = parse_diff_args(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_diff_args_only_json_flag() {
        let args = vec!["--json".to_string()];
        let result = parse_diff_args(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_diff_args_invalid_range() {
        let args = vec!["..".to_string()];
        let result = parse_diff_args(&args);
        assert!(result.is_err());

        let args = vec!["abc..".to_string()];
        let result = parse_diff_args(&args);
        assert!(result.is_err());

        let args = vec!["..def".to_string()];
        let result = parse_diff_args(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_hunk_line_basic() {
        let line = "@@ -10,3 +15,5 @@ fn main() {";
        let result = parse_hunk_line(line, "test.rs", None).unwrap().unwrap();

        assert_eq!(result.file_path, "test.rs");
        assert_eq!(result.old_file_path, None);
        assert_eq!(result.old_start, 10);
        assert_eq!(result.old_count, 3);
        assert_eq!(result.new_start, 15);
        assert_eq!(result.new_count, 5);
        assert_eq!(result.deleted_lines, vec![10, 11, 12]);
        assert_eq!(result.added_lines, vec![15, 16, 17, 18, 19]);
    }

    #[test]
    fn test_parse_hunk_line_single_line_deletion() {
        let line = "@@ -10 +10,2 @@ fn main() {";
        let result = parse_hunk_line(line, "test.rs", None).unwrap().unwrap();

        assert_eq!(result.old_start, 10);
        assert_eq!(result.old_count, 1);
        assert_eq!(result.new_start, 10);
        assert_eq!(result.new_count, 2);
        assert_eq!(result.deleted_lines, vec![10]);
        assert_eq!(result.added_lines, vec![10, 11]);
    }

    #[test]
    fn test_parse_hunk_line_single_line_addition() {
        let line = "@@ -10,2 +10 @@ fn main() {";
        let result = parse_hunk_line(line, "test.rs", None).unwrap().unwrap();

        assert_eq!(result.old_start, 10);
        assert_eq!(result.old_count, 2);
        assert_eq!(result.new_start, 10);
        assert_eq!(result.new_count, 1);
        assert_eq!(result.deleted_lines, vec![10, 11]);
        assert_eq!(result.added_lines, vec![10]);
    }

    #[test]
    fn test_parse_hunk_line_pure_addition() {
        let line = "@@ -0,0 +1,3 @@ fn main() {";
        let result = parse_hunk_line(line, "test.rs", None).unwrap().unwrap();

        assert_eq!(result.old_start, 0);
        assert_eq!(result.old_count, 0);
        assert_eq!(result.new_start, 1);
        assert_eq!(result.new_count, 3);
        assert_eq!(result.deleted_lines.len(), 0);
        assert_eq!(result.added_lines, vec![1, 2, 3]);
    }

    #[test]
    fn test_parse_hunk_line_pure_deletion() {
        let line = "@@ -5,3 +0,0 @@ fn main() {";
        let result = parse_hunk_line(line, "test.rs", None).unwrap().unwrap();

        assert_eq!(result.old_start, 5);
        assert_eq!(result.old_count, 3);
        assert_eq!(result.new_start, 0);
        assert_eq!(result.new_count, 0);
        assert_eq!(result.deleted_lines, vec![5, 6, 7]);
        assert_eq!(result.added_lines.len(), 0);
    }

    #[test]
    fn test_parse_hunk_header_for_line_nums() {
        let line = "@@ -10,5 +20,3 @@ context";
        let result = parse_hunk_header_for_line_nums(line).unwrap();
        assert_eq!(result, (10, 20));
    }

    #[test]
    fn test_parse_hunk_header_for_line_nums_single_line() {
        let line = "@@ -10 +20,3 @@ context";
        let result = parse_hunk_header_for_line_nums(line).unwrap();
        assert_eq!(result, (10, 20));

        let line = "@@ -10,5 +20 @@ context";
        let result = parse_hunk_header_for_line_nums(line).unwrap();
        assert_eq!(result, (10, 20));
    }

    #[test]
    fn test_parse_hunk_header_for_line_nums_invalid() {
        let line = "not a hunk header";
        let result = parse_hunk_header_for_line_nums(line);
        assert!(result.is_none());

        let line = "@@ invalid @@";
        let result = parse_hunk_header_for_line_nums(line);
        assert!(result.is_none());
    }

    #[test]
    fn test_format_attribution_ai() {
        let humans = BTreeMap::new();
        let attr = Attribution::Ai("cursor".to_string());
        assert_eq!(format_attribution(&attr, &humans), "🤖cursor");

        let attr = Attribution::Ai("claude".to_string());
        assert_eq!(format_attribution(&attr, &humans), "🤖claude");
    }

    #[test]
    fn test_format_attribution_human() {
        let mut humans = BTreeMap::new();
        humans.insert(
            "h_alice123".to_string(),
            HumanRecord {
                author: "alice".to_string(),
            },
        );
        humans.insert(
            "h_bob456".to_string(),
            HumanRecord {
                author: "bob@example.com".to_string(),
            },
        );

        let attr = Attribution::Human("h_alice123".to_string());
        assert_eq!(format_attribution(&attr, &humans), "👤alice");

        let attr = Attribution::Human("h_bob456".to_string());
        assert_eq!(format_attribution(&attr, &humans), "👤bob@example.com");

        // Test fallback when human_id not in map
        let attr = Attribution::Human("h_unknown".to_string());
        assert_eq!(format_attribution(&attr, &humans), "👤h_unknown");
    }

    #[test]
    fn test_format_attribution_no_data() {
        let humans = BTreeMap::new();
        let attr = Attribution::NoData;
        assert_eq!(format_attribution(&attr, &humans), "[no-data]");
    }

    #[test]
    fn test_format_git_ident_prefers_full_ident() {
        assert_eq!(
            format_git_ident("Test User", "test@example.com"),
            "Test User <test@example.com>"
        );
    }

    #[test]
    fn test_format_git_ident_handles_missing_parts() {
        assert_eq!(format_git_ident("Test User", ""), "Test User");
        assert_eq!(
            format_git_ident("", "test@example.com"),
            "<test@example.com>"
        );
        assert_eq!(format_git_ident("", ""), "");
    }

    #[test]
    fn test_diff_line_key_equality() {
        let key1 = DiffLineKey {
            file: "test.rs".to_string(),
            line: 10,
            side: LineSide::Old,
        };

        let key2 = DiffLineKey {
            file: "test.rs".to_string(),
            line: 10,
            side: LineSide::Old,
        };

        let key3 = DiffLineKey {
            file: "test.rs".to_string(),
            line: 10,
            side: LineSide::New,
        };

        assert_eq!(key1, key2);
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_parse_diff_hunks_multiple_files() {
        let diff_text = r#"diff --git a/file1.rs b/file1.rs
index abc123..def456 100644
--- a/file1.rs
+++ b/file1.rs
@@ -10,2 +10,3 @@ fn main() {
diff --git a/file2.rs b/file2.rs
index 111222..333444 100644
--- a/file2.rs
+++ b/file2.rs
@@ -5,1 +5,2 @@ fn test() {
"#;

        let result = parse_diff_hunks(diff_text).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].file_path, "file1.rs");
        assert_eq!(result[1].file_path, "file2.rs");
    }

    #[test]
    fn test_parse_diff_hunks_empty() {
        let diff_text = "";
        let result = parse_diff_hunks(diff_text).unwrap();
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_parse_diff_hunks_no_prefix_paths() {
        let diff_text = r#"diff --git file1.rs file1.rs
index abc123..def456 100644
--- file1.rs
+++ file1.rs
@@ -1,0 +1,1 @@
+fn added() {}
"#;

        let result = parse_diff_hunks(diff_text).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].file_path, "file1.rs");
    }

    #[test]
    fn test_parse_diff_hunks_custom_prefix_paths() {
        let diff_text = r#"diff --git SRC/file1.rs DST/file1.rs
index abc123..def456 100644
--- SRC/file1.rs
+++ DST/file1.rs
@@ -1,0 +1,1 @@
+fn added() {}
"#;

        let result = parse_diff_hunks(diff_text).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].file_path, "DST/file1.rs");
        assert_eq!(result[0].old_file_path, Some("SRC/file1.rs".to_string()));
    }

    #[test]
    fn test_parse_diff_hunks_rename_tracks_old_file_path() {
        let diff_text = r#"diff --git a/old_name.txt b/new_name.txt
similarity index 62%
rename from old_name.txt
rename to new_name.txt
index 7f4f5e8..1c84817 100644
--- a/old_name.txt
+++ b/new_name.txt
@@ -1,3 +1,2 @@
 keep
-drop-me
 tail
"#;

        let result = parse_diff_hunks(diff_text).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].file_path, "new_name.txt");
        assert_eq!(result[0].old_file_path, Some("old_name.txt".to_string()));
    }

    #[test]
    fn test_parse_diff_hunks_preserves_header_like_content_inside_hunk() {
        let diff_text = r#"diff --git a/query.sql b/query.sql
index abc123..def456 100644
--- a/query.sql
+++ b/query.sql
@@ -10,3 +10,3 @@
--- old sql comment
-WHERE id = 1;
+++ new marker
+WHERE id = 2;
 SELECT * FROM users;
@@ -30,1 +30,1 @@
-regular old
+regular new
"#;

        let result = parse_diff_hunks(diff_text).unwrap();
        assert_eq!(result.len(), 2);

        assert_eq!(result[0].file_path, "query.sql");
        assert_eq!(result[0].deleted_lines, vec![10, 11]);
        assert_eq!(
            result[0].deleted_contents,
            vec!["-- old sql comment", "WHERE id = 1;"]
        );
        assert_eq!(result[0].added_lines, vec![10, 11]);
        assert_eq!(
            result[0].added_contents,
            vec!["++ new marker", "WHERE id = 2;"]
        );

        assert_eq!(result[1].file_path, "query.sql");
        assert_eq!(result[1].deleted_lines, vec![30]);
        assert_eq!(result[1].added_lines, vec![30]);
    }

    #[test]
    fn test_parse_diff_hunks_preserves_plus_plus_plus_content_inside_hunk() {
        let diff_text = r#"diff --git a/script.lua b/script.lua
index abc123..def456 100644
--- a/script.lua
+++ b/script.lua
@@ -41,0 +42,2 @@
+++ section marker
+print("hello")
"#;

        let result = parse_diff_hunks(diff_text).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].file_path, "script.lua");
        assert_eq!(result[0].added_lines, vec![42, 43]);
        assert_eq!(
            result[0].added_contents,
            vec!["++ section marker", "print(\"hello\")"]
        );
    }

    #[test]
    fn test_is_diff_header_line_respects_hunk_state() {
        assert!(is_diff_header_line("diff --git a/f b/f", false));
        assert!(is_diff_header_line("index abc..def 100644", false));
        assert!(is_diff_header_line("--- a/file.txt", false));
        assert!(is_diff_header_line("+++ b/file.txt", false));
        assert!(!is_diff_header_line("--- content line", true));
        assert!(!is_diff_header_line("+++ content line", true));
    }

    #[test]
    fn test_parse_diff_git_header_paths_standard_and_quoted() {
        let parsed = parse_diff_git_header_paths("diff --git a/src/lib.rs b/src/lib.rs")
            .expect("standard diff header should parse");
        assert_eq!(parsed, ("src/lib.rs".to_string(), "src/lib.rs".to_string()));

        let parsed = parse_diff_git_header_paths(r#"diff --git "a/my file.rs" "b/my file.rs""#)
            .expect("quoted diff header should parse");
        assert_eq!(parsed, ("my file.rs".to_string(), "my file.rs".to_string()));
    }

    #[test]
    fn test_calculate_diff_commit_stats_tracks_unknown_added_lines() {
        fn prompt_record(tool: &str, model: &str, additions: u32, deletions: u32) -> PromptRecord {
            PromptRecord {
                agent_id: AgentId {
                    tool: tool.to_string(),
                    id: format!("{}-id", tool),
                    model: model.to_string(),
                },
                human_author: None,
                total_additions: additions,
                total_deletions: deletions,
                accepted_lines: 0,
                overriden_lines: 0,
                custom_attributes: None,
                messages_url: None,
            }
        }

        let mut attributions = HashMap::new();
        attributions.insert(
            DiffLineKey {
                file: "f.rs".to_string(),
                line: 1,
                side: LineSide::New,
            },
            Attribution::Ai("cursor".to_string()),
        );
        attributions.insert(
            DiffLineKey {
                file: "f.rs".to_string(),
                line: 2,
                side: LineSide::New,
            },
            Attribution::Human("alice".to_string()),
        );
        attributions.insert(
            DiffLineKey {
                file: "f.rs".to_string(),
                line: 3,
                side: LineSide::New,
            },
            Attribution::NoData,
        );
        // Old-side no-data should not affect unknown_lines_added.
        attributions.insert(
            DiffLineKey {
                file: "f.rs".to_string(),
                line: 10,
                side: LineSide::Old,
            },
            Attribution::NoData,
        );

        let mut annotations = BTreeMap::new();
        annotations.insert("p1".to_string(), vec![LineRange::Single(1)]);
        let mut annotations_by_file = BTreeMap::new();
        annotations_by_file.insert("f.rs".to_string(), annotations);

        let mut prompts = BTreeMap::new();
        prompts.insert("p1".to_string(), prompt_record("cursor", "gpt-4o", 5, 2));

        let artifacts = DiffBuildArtifacts {
            attributions,
            annotations_by_file,
            prompts: prompts.clone(),
            humans: BTreeMap::new(),
            sessions: BTreeMap::new(),
            json_hunks: vec![DiffJsonHunk {
                commit_sha: "abc".to_string(),
                content_hash: "hash".to_string(),
                hunk_kind: "deletion".to_string(),
                original_commit_sha: None,
                start_line: 5,
                end_line: 6,
                file_path: "f.rs".to_string(),
                prompt_id: None,
                session_id: None,
                human_id: None,
            }],
            commits: BTreeMap::new(),
            included_files: HashSet::new(),
        };

        let sessions: BTreeMap<String, SessionRecord> = BTreeMap::new();
        let stats = calculate_diff_commit_stats(&artifacts, &prompts, &sessions);
        assert_eq!(stats.ai_lines_added, 1);
        assert_eq!(stats.human_lines_added, 1);
        assert_eq!(stats.unknown_lines_added, 1);
        assert_eq!(stats.git_lines_added, 3);
        assert_eq!(stats.git_lines_deleted, 2);

        let breakdown = stats
            .tool_model_breakdown
            .get("cursor::gpt-4o")
            .expect("expected cursor::gpt-4o breakdown entry");
        assert_eq!(breakdown.ai_lines_added, 1);
    }

    #[test]
    fn test_is_binary_diff_section_detects_binary() {
        let section = "diff --git a/image.png b/image.png\nnew file mode 100644\nindex 0000000..abc1234\nBinary files /dev/null and b/image.png differ\n";
        assert!(is_binary_diff_section(section));
    }

    #[test]
    fn test_is_binary_diff_section_allows_text() {
        let section = "diff --git a/src/main.rs b/src/main.rs\nindex abc1234..def5678 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,1 +1,2 @@\n fn main() {}\n+fn added() {}\n";
        assert!(!is_binary_diff_section(section));
    }
}
