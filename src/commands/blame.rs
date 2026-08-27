use crate::auth::CredentialStore;
use crate::authorship::authorship_log::{HumanRecord, PromptRecord, SessionRecord};
use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::authorship::working_log::CheckpointKind;
use crate::error::GitAiError;
use crate::git::notes_api::read_authorship_v3;
use crate::git::repository::Repository;
use crate::git::repository::{exec_git, exec_git_stdin};
#[cfg(windows)]
use crate::utils::normalize_to_posix;
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::sync::LazyLock;

//🐰🥚 @todo use actual date Git AI was installed in each repo
pub static OLDEST_AI_BLAME_DATE: LazyLock<DateTime<FixedOffset>> = LazyLock::new(|| {
    FixedOffset::east_opt(0)
        .unwrap()
        .with_ymd_and_hms(2025, 7, 4, 0, 0, 0)
        .unwrap()
});

#[derive(Debug, Clone, Serialize)]
pub struct BlameHunk {
    /// Line range [start, end] (inclusive) - current line numbers in the file
    pub range: (u32, u32),
    /// Original line range [start, end] (inclusive) - line numbers in the commit that introduced them
    pub orig_range: (u32, u32),
    /// Commit SHA that introduced this hunk
    pub commit_sha: String,
    /// Abbreviated commit SHA
    #[allow(dead_code)]
    pub abbrev_sha: String,
    /// Original author from Git blame
    pub original_author: String,
    /// Author email
    pub author_email: String,
    /// Author time (unix timestamp)
    pub author_time: i64,
    /// Author timezone (e.g. "+0000")
    pub author_tz: String,
    /// AI human author name
    pub ai_human_author: Option<String>,
    /// Committer name
    pub committer: String,
    /// Committer email
    pub committer_email: String,
    /// Committer time (unix timestamp)
    pub committer_time: i64,
    /// Committer timezone
    pub committer_tz: String,
    /// Whether this is a boundary commit
    pub is_boundary: bool,
    /// The filename at the blamed commit (may differ from current if file was renamed)
    #[serde(default)]
    pub orig_filename: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlameAnalysisResult {
    pub line_authors: HashMap<u32, String>,
    pub prompt_records: HashMap<String, PromptRecord>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub session_records: HashMap<String, SessionRecord>,
    pub blame_hunks: Vec<BlameHunk>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub humans: BTreeMap<String, HumanRecord>,
}

struct PreparedBlameRequest {
    relative_file_path: String,
    file_content: String,
    line_ranges: Vec<(u32, u32)>,
    options: GitAiBlameOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitAiBlameOptions {
    // Line range options
    pub line_ranges: Vec<(u32, u32)>,

    pub newest_commit: Option<String>,
    pub oldest_commit: Option<String>,
    pub oldest_date: Option<DateTime<FixedOffset>>,
    /// Raw git --since value (e.g. "2 weeks ago", "2026-01-01"), used when callers
    /// need idiomatic git date parsing without pre-parsing to RFC3339.
    pub oldest_date_spec: Option<String>,

    // Output format options
    pub porcelain: bool,
    pub line_porcelain: bool,
    pub incremental: bool,
    pub show_name: bool,
    pub show_number: bool,
    pub show_email: bool,
    pub suppress_author: bool,
    pub show_stats: bool,

    // Commit display options
    pub long_rev: bool,
    pub raw_timestamp: bool,
    pub abbrev: Option<u32>,

    // Boundary options
    pub blank_boundary: bool,
    pub show_root: bool,

    // Movement detection options
    pub detect_moves: bool,
    pub detect_copies: u32, // Number of -C flags (0-3)
    pub move_threshold: Option<u32>,

    // Ignore options
    pub ignore_revs: Vec<String>,
    pub ignore_revs_file: Option<String>,
    /// Disable auto-detection of .git-blame-ignore-revs file
    pub no_ignore_revs_file: bool,

    // Color options
    pub color_lines: bool,
    pub color_by_age: bool,

    // Progress options
    pub progress: bool,

    // Date format
    pub date_format: Option<String>,

    // Content options
    pub contents_file: Option<String>,

    // Revision options
    #[allow(dead_code)]
    pub reverse: Option<String>,
    pub first_parent: bool,

    // Encoding
    pub encoding: Option<String>,

    // Pre-read contents data (from --contents flag, either from stdin or file)
    // This is populated during argument parsing and used by blame
    pub contents_data: Option<Vec<u8>>,

    // Use prompt hashes as name instead of author names
    pub use_prompt_hashes_as_names: bool,

    // Return all human authors as CheckpointKind::Human
    pub return_human_authors_as_human: bool,

    // No output
    pub no_output: bool,

    // Ignore whitespace
    pub ignore_whitespace: bool,

    // JSON output format
    pub json: bool,

    // Mark lines from commits without authorship logs as "Unknown"
    pub mark_unknown: bool,

    // Show prompt hashes inline and dump prompts when piped
    pub show_prompt: bool,

    // Split hunks when lines have different AI human authors
    // When true, a single git blame hunk may be split into multiple hunks
    // if different lines were authored by different humans working with AI
    pub split_hunks_by_ai_author: bool,

    // Skip populating `ai_human_author` on blame hunks (the `populate_ai_human_authors`
    // pass). That pass reads each blamed commit's authorship note and runs per-line
    // attribution purely to annotate hunks. Callers that consume only `line_authors` /
    // `prompt_records` from `blame()` (e.g. range stats via `diff_ai_accepted_stats`
    // and `VirtualAttributions`) discard the hunks, so for them the pass is dead work.
    // It is output-invariant to skip because `overlay_ai_authorship` derives
    // `line_authors` per line independently of hunk grouping/`ai_human_author`.
    pub skip_human_author_population: bool,
}

impl Default for GitAiBlameOptions {
    fn default() -> Self {
        Self {
            line_ranges: Vec::new(),
            porcelain: false,
            newest_commit: None,
            oldest_commit: None,
            oldest_date: None,
            oldest_date_spec: None,
            line_porcelain: false,
            incremental: false,
            show_name: false,
            show_number: false,
            show_email: false,
            suppress_author: false,
            show_stats: false,
            long_rev: false,
            raw_timestamp: false,
            abbrev: None,
            blank_boundary: false,
            show_root: false,
            detect_moves: false,
            detect_copies: 0,
            move_threshold: None,
            ignore_revs: Vec::new(),
            ignore_revs_file: None,
            no_ignore_revs_file: false,
            color_lines: false,
            color_by_age: false,
            progress: false,
            date_format: None,
            contents_file: None,
            reverse: None,
            first_parent: false,
            encoding: None,
            contents_data: None,
            use_prompt_hashes_as_names: false,
            return_human_authors_as_human: false,
            no_output: false,
            ignore_whitespace: false,
            json: false,
            mark_unknown: false,
            show_prompt: false,
            split_hunks_by_ai_author: true,
            skip_human_author_population: false,
        }
    }
}

impl Repository {
    const BLAME_ABBREV_BATCH_SIZE: usize = 256;

    fn normalize_blame_file_path(&self, file_path: &str) -> Result<String, GitAiError> {
        let repo_root = self.workdir().map_err(|e| {
            GitAiError::Generic(format!("Repository has no working directory: {}", e))
        })?;

        // Normalize the file path to be relative to repo root.
        // This is important for AI authorship lookup which stores paths relative to repo root.
        let file_path_buf = std::path::Path::new(file_path);
        let relative_file_path = if file_path_buf.is_absolute() {
            // Convert absolute path to relative path.
            // Canonicalize both paths to handle symlinks (e.g., /var -> /private/var on macOS).
            let canonical_file_path = file_path_buf.canonicalize().map_err(|e| {
                GitAiError::Generic(format!(
                    "Failed to canonicalize file path '{}': {}",
                    file_path, e
                ))
            })?;
            let canonical_repo_root = repo_root.canonicalize().map_err(|e| {
                GitAiError::Generic(format!(
                    "Failed to canonicalize repository root '{}': {}",
                    repo_root.display(),
                    e
                ))
            })?;

            canonical_file_path
                .strip_prefix(&canonical_repo_root)
                .map_err(|_| {
                    GitAiError::Generic(format!(
                        "File path '{}' is not within repository root '{}'",
                        file_path,
                        repo_root.display()
                    ))
                })?
                .to_string_lossy()
                .to_string()
        } else {
            file_path.to_string()
        };

        // Normalize path separators and leading ./.
        #[cfg(windows)]
        let relative_file_path = {
            let normalized = normalize_to_posix(&relative_file_path);
            normalized
                .strip_prefix("./")
                .unwrap_or(&normalized)
                .to_string()
        };

        #[cfg(not(windows))]
        let relative_file_path = {
            relative_file_path
                .strip_prefix("./")
                .unwrap_or(&relative_file_path)
                .to_string()
        };

        Ok(relative_file_path)
    }

    fn effective_blame_options(options: &GitAiBlameOptions) -> GitAiBlameOptions {
        // For JSON output, default to HEAD to exclude uncommitted changes
        // and use prompt hashes as names so we can correlate with prompt_records.
        if options.json {
            let mut opts = options.clone();
            if opts.newest_commit.is_none() {
                opts.newest_commit = Some("HEAD".to_string());
            }
            opts.use_prompt_hashes_as_names = true;
            opts
        } else if options.show_prompt {
            let mut opts = options.clone();
            opts.use_prompt_hashes_as_names = true;
            opts
        } else {
            options.clone()
        }
    }

    fn read_blame_file_content(
        &self,
        relative_file_path: &str,
        options: &GitAiBlameOptions,
    ) -> Result<String, GitAiError> {
        // Read file content from one of:
        // 1. Provided contents_data (from --contents flag)
        // 2. A specific commit
        // 3. The working directory
        if let Some(ref data) = options.contents_data {
            // Use pre-read contents data (from --contents stdin or file)
            Ok(String::from_utf8_lossy(data).to_string())
        } else if let Some(ref commit) = options.newest_commit {
            // Read file content from the specified commit.
            // This ensures blame is independent of which branch is checked out.
            let commit_obj = self.find_commit(commit.clone())?;
            let tree = commit_obj.tree()?;

            match tree.get_path(std::path::Path::new(relative_file_path)) {
                Ok(entry) => {
                    if let Ok(blob) = self.find_blob(entry.id()) {
                        let blob_content = blob.content().unwrap_or_default();
                        Ok(String::from_utf8_lossy(&blob_content).to_string())
                    } else {
                        Err(GitAiError::Generic(format!(
                            "File '{}' is not a blob in commit {}",
                            relative_file_path, commit
                        )))
                    }
                }
                Err(_) => Err(GitAiError::Generic(format!(
                    "File '{}' not found in commit {}",
                    relative_file_path, commit
                ))),
            }
        } else {
            // Read from working directory.
            let repo_root = self.workdir().map_err(|e| {
                GitAiError::Generic(format!("Repository has no working directory: {}", e))
            })?;
            let abs_file_path = repo_root.join(relative_file_path);

            if !abs_file_path.exists() {
                return Err(GitAiError::Generic(format!(
                    "File not found: {}",
                    abs_file_path.display()
                )));
            }

            let raw_bytes = fs::read(&abs_file_path)?;
            Ok(String::from_utf8_lossy(&raw_bytes).into_owned())
        }
    }

    fn prepare_blame_request(
        &self,
        file_path: &str,
        options: &GitAiBlameOptions,
    ) -> Result<PreparedBlameRequest, GitAiError> {
        let relative_file_path = self.normalize_blame_file_path(file_path)?;
        let options = Self::effective_blame_options(options);
        let file_content = self.read_blame_file_content(&relative_file_path, &options)?;
        let total_lines = file_content.lines().count() as u32;

        // Determine the line ranges to process.
        let line_ranges = if options.line_ranges.is_empty() {
            vec![(1, total_lines)]
        } else {
            options.line_ranges.clone()
        };

        // Validate line ranges.
        for (start, end) in &line_ranges {
            if *start == 0 || *end == 0 || start > end || *end > total_lines {
                return Err(GitAiError::Generic(format!(
                    "Invalid line range: {}:{}. File has {} lines",
                    start, end, total_lines
                )));
            }
        }

        Ok(PreparedBlameRequest {
            relative_file_path,
            file_content,
            line_ranges,
            options,
        })
    }

    #[allow(clippy::type_complexity)]
    fn run_blame_analysis_pipeline(
        &self,
        relative_file_path: &str,
        line_ranges: &[(u32, u32)],
        options: &GitAiBlameOptions,
    ) -> Result<
        (
            BlameAnalysisResult,
            Vec<AuthorshipLog>,
            HashMap<String, Vec<String>>,
            std::collections::HashSet<String>, // commits with real authorship notes
        ),
        GitAiError,
    > {
        // Step 1: Get Git's native blame for all ranges in one invocation.
        let blame_hunks = self.blame_hunks_for_ranges(relative_file_path, line_ranges, options)?;

        // Step 2: Overlay AI authorship information.
        let (
            line_authors,
            prompt_records,
            session_records,
            humans,
            authorship_logs,
            prompt_commits,
            commits_with_notes,
        ) = overlay_ai_authorship(self, &blame_hunks, relative_file_path, options)?;

        Ok((
            BlameAnalysisResult {
                line_authors,
                prompt_records,
                session_records,
                blame_hunks,
                humans,
            },
            authorship_logs,
            prompt_commits,
            commits_with_notes,
        ))
    }

    fn blame_requested_abbrev_len(options: &GitAiBlameOptions, is_boundary: bool) -> usize {
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

    fn resolve_blame_abbrev_shas_batched(
        &self,
        requests_by_len: &HashMap<usize, Vec<String>>,
    ) -> HashMap<(String, usize), String> {
        let mut resolved: HashMap<(String, usize), String> = HashMap::new();

        for (&requested_len, commit_shas) in requests_by_len {
            if commit_shas.is_empty() {
                continue;
            }

            for commit_sha_batch in commit_shas.chunks(Self::BLAME_ABBREV_BATCH_SIZE) {
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

    fn populate_hunk_abbrev_shas(&self, hunks: &mut [BlameHunk], options: &GitAiBlameOptions) {
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

    #[allow(clippy::type_complexity)]
    pub fn blame(
        &self,
        file_path: &str,
        options: &GitAiBlameOptions,
    ) -> Result<(HashMap<u32, String>, HashMap<String, PromptRecord>), GitAiError> {
        let request = self.prepare_blame_request(file_path, options)?;
        let lines: Vec<&str> = request.file_content.lines().collect();
        let (analysis, authorship_logs, prompt_commits, commits_with_notes) = self
            .run_blame_analysis_pipeline(
                &request.relative_file_path,
                &request.line_ranges,
                &request.options,
            )?;
        let BlameAnalysisResult {
            line_authors,
            prompt_records,
            session_records: _,
            blame_hunks: _,
            humans: _,
        } = analysis;

        if request.options.no_output {
            return Ok((line_authors, prompt_records));
        }

        // Output based on format
        if options.json {
            output_json_format(
                self,
                &line_authors,
                &prompt_records,
                &authorship_logs,
                &prompt_commits,
                &request.relative_file_path,
            )?;
        } else if request.options.porcelain || request.options.line_porcelain {
            output_porcelain_format(
                self,
                &line_authors,
                &request.relative_file_path,
                &lines,
                &request.line_ranges,
                &request.options,
                &commits_with_notes,
            )?;
        } else if request.options.incremental {
            output_incremental_format(
                self,
                &line_authors,
                &request.relative_file_path,
                &lines,
                &request.line_ranges,
                &request.options,
                &commits_with_notes,
            )?;
        } else {
            output_default_format(
                self,
                &line_authors,
                &prompt_records,
                &request.relative_file_path,
                &lines,
                &request.line_ranges,
                &request.options,
            )?;
        }

        Ok((line_authors, prompt_records))
    }

    pub fn blame_analysis(
        &self,
        file_path: &str,
        options: &GitAiBlameOptions,
    ) -> Result<BlameAnalysisResult, GitAiError> {
        let request = self.prepare_blame_request(file_path, options)?;
        let (analysis, _authorship_logs, _prompt_commits, _commits_with_notes) = self
            .run_blame_analysis_pipeline(
                &request.relative_file_path,
                &request.line_ranges,
                &request.options,
            )?;
        Ok(analysis)
    }

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
                cur_meta.filename = Some(crate::utils::unescape_git_path(rest));
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

        self.populate_hunk_abbrev_shas(&mut hunks, options);

        // Post-process hunks to populate ai_human_author from authorship logs.
        // Callers that consume only line_authors/prompt_records and discard the hunks
        // (range stats) opt out of this pass — it is dead work for them and skipping it
        // avoids a second per-commit authorship-note read over every blamed commit.
        //
        // Guard the safe-use contract: skipping drops `ai_human_author` and hunk
        // splitting, which only stays output-invariant for callers that discard the
        // hunks. Every current opt-in (diff_ai_accepted_stats, VirtualAttributions) is a
        // `no_output` path, so assert that here to catch future misuse in debug builds.
        debug_assert!(
            !options.skip_human_author_population || options.no_output,
            "skip_human_author_population is only safe on hunk-discarding (no_output) blame paths"
        );
        let hunks = if options.skip_human_author_population {
            hunks
        } else {
            self.populate_ai_human_authors(hunks, file_path, options)?
        };

        Ok(hunks)
    }

    /// Post-process blame hunks to populate ai_human_author from authorship logs.
    /// For each hunk, looks up the authorship log for its commit and finds the human_author
    /// from the prompt record that covers lines in the hunk.
    /// If `split_hunks_by_ai_author` is true and different lines in a hunk have different
    /// human_authors, the hunk is split into multiple hunks.
    fn populate_ai_human_authors(
        &self,
        hunks: Vec<BlameHunk>,
        file_path: &str,
        options: &GitAiBlameOptions,
    ) -> Result<Vec<BlameHunk>, GitAiError> {
        // Cache authorship logs by commit SHA to avoid repeated lookups
        let mut commit_authorship_cache: HashMap<String, Option<AuthorshipLog>> = HashMap::new();
        // Cache for foreign prompts to avoid repeated grepping
        let mut foreign_prompts_cache: HashMap<String, Option<PromptRecord>> = HashMap::new();

        let mut result_hunks: Vec<BlameHunk> = Vec::new();

        // Batch-resolve all blamed commits' authorship notes up front (one `cat-file
        // --batch` instead of a `git notes show` per unique commit), then read from the
        // cache in the loop below. Identical results to the per-commit lazy path.
        let unique_commit_shas: Vec<String> = hunks
            .iter()
            .map(|hunk| hunk.commit_sha.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        for (sha, authorship) in
            crate::git::notes_api::read_authorship_v3_batch(self, &unique_commit_shas)
        {
            commit_authorship_cache.entry(sha).or_insert(authorship);
        }

        for hunk in hunks {
            // Get or fetch the authorship log for this commit
            let authorship_log = if let Some(cached) = commit_authorship_cache.get(&hunk.commit_sha)
            {
                cached.clone()
            } else {
                let authorship = read_authorship_v3(self, &hunk.commit_sha).ok();
                commit_authorship_cache.insert(hunk.commit_sha.clone(), authorship.clone());
                authorship
            };

            // If we have an authorship log, look up human_author for each line
            if let Some(ref authorship_log) = authorship_log {
                // Collect human_author for each line in this hunk
                let num_lines = hunk.range.1 - hunk.range.0 + 1;
                let mut line_authors: Vec<Option<String>> = Vec::with_capacity(num_lines as usize);

                for i in 0..num_lines {
                    let orig_line_num = hunk.orig_range.0 + i;

                    let human_author = if let Some((_author, _prompt_hash, Some(prompt_record))) =
                        authorship_log.get_line_attribution(
                            self,
                            file_path,
                            orig_line_num,
                            &mut foreign_prompts_cache,
                        ) {
                        prompt_record.human_author.clone()
                    } else {
                        None
                    };
                    line_authors.push(human_author);
                }

                if options.split_hunks_by_ai_author {
                    // Split hunk by consecutive lines with the same human_author
                    let mut current_start_idx: u32 = 0;
                    let mut current_author = line_authors.first().cloned().flatten();

                    for (i, author) in line_authors.iter().enumerate() {
                        let author_flat = author.clone();
                        if author_flat != current_author {
                            // Create a hunk for the previous group
                            let group_start = hunk.range.0 + current_start_idx;
                            let group_end = hunk.range.0 + (i as u32) - 1;
                            let orig_group_start = hunk.orig_range.0 + current_start_idx;
                            let orig_group_end = hunk.orig_range.0 + (i as u32) - 1;

                            let mut new_hunk = hunk.clone();
                            new_hunk.range = (group_start, group_end);
                            new_hunk.orig_range = (orig_group_start, orig_group_end);
                            new_hunk.ai_human_author = current_author.clone();
                            result_hunks.push(new_hunk);

                            // Start a new group
                            current_start_idx = i as u32;
                            current_author = author_flat;
                        }
                    }

                    // Don't forget the last group
                    let group_start = hunk.range.0 + current_start_idx;
                    let group_end = hunk.range.1;
                    let orig_group_start = hunk.orig_range.0 + current_start_idx;
                    let orig_group_end = hunk.orig_range.1;

                    let mut new_hunk = hunk.clone();
                    new_hunk.range = (group_start, group_end);
                    new_hunk.orig_range = (orig_group_start, orig_group_end);
                    new_hunk.ai_human_author = current_author;
                    result_hunks.push(new_hunk);
                } else {
                    // Don't split - just use the first human_author found
                    let mut new_hunk = hunk;
                    new_hunk.ai_human_author = line_authors.into_iter().flatten().next();
                    result_hunks.push(new_hunk);
                }
            } else {
                // No authorship log, keep hunk as-is
                result_hunks.push(hunk);
            }
        }

        Ok(result_hunks)
    }
}

#[allow(clippy::type_complexity)]
fn overlay_ai_authorship(
    repo: &Repository,
    blame_hunks: &[BlameHunk],
    file_path: &str,
    options: &GitAiBlameOptions,
) -> Result<
    (
        HashMap<u32, String>,
        HashMap<String, PromptRecord>,
        HashMap<String, SessionRecord>,
        BTreeMap<String, HumanRecord>, // humans map
        Vec<AuthorshipLog>,
        HashMap<String, Vec<String>>,      // prompt_hash -> commit_shas
        std::collections::HashSet<String>, // commit SHAs with real authorship notes
    ),
    GitAiError,
> {
    let mut line_authors: HashMap<u32, String> = HashMap::new();
    let mut prompt_records: HashMap<String, PromptRecord> = HashMap::new();
    let mut session_records: HashMap<String, SessionRecord> = HashMap::new();
    let mut humans: BTreeMap<String, HumanRecord> = BTreeMap::new();
    // Track which commits contain each prompt hash
    let mut prompt_commits: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    // Track commit SHAs that have real (non-simulated) authorship notes
    let mut commits_with_notes: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    // Group hunks by commit SHA to avoid repeated lookups
    let mut commit_authorship_cache: HashMap<String, Option<AuthorshipLog>> = HashMap::new();
    // Simulated authorship logs for agent commits without notes. We keep these separate
    // from commit_authorship_cache so a single agent commit can be handled across multiple
    // blame hunks without being limited to the first hunk's line range.
    let mut simulated_authorship_logs: HashMap<String, AuthorshipLog> = HashMap::new();
    // Cache for foreign prompts to avoid repeated grepping
    let mut foreign_prompts_cache: HashMap<String, Option<PromptRecord>> = HashMap::new();

    // Resolve every blamed commit's authorship note in a single batched read instead of
    // one `git notes show` subprocess per unique commit. This is the dominant cost of
    // `git-ai stats` over a range; the per-hunk loop below then only reads from the cache.
    // The batch produces values identical to the per-commit get_reference_as_authorship_log_v3.
    let unique_commit_shas: Vec<String> = blame_hunks
        .iter()
        .map(|hunk| hunk.commit_sha.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    for (sha, authorship) in
        crate::git::notes_api::read_authorship_v3_batch(repo, &unique_commit_shas)
    {
        commit_authorship_cache.entry(sha).or_insert(authorship);
    }

    for hunk in blame_hunks {
        // Check if we've already looked up this commit's authorship
        let authorship_log = if let Some(cached) = commit_authorship_cache.get(&hunk.commit_sha) {
            cached.clone()
        } else {
            // Try to get authorship log for this commit
            let authorship = read_authorship_v3(repo, &hunk.commit_sha).ok();
            commit_authorship_cache.insert(hunk.commit_sha.clone(), authorship.clone());
            authorship
        };

        // If we have AI authorship data, look up the author for lines in this hunk
        if let Some(ref authorship_log) = authorship_log {
            commits_with_notes.insert(hunk.commit_sha.clone());

            // Collect humans from this authorship log
            for (human_id, human_record) in &authorship_log.metadata.humans {
                humans
                    .entry(human_id.clone())
                    .or_insert_with(|| human_record.clone());
            }

            // Collect session records from this authorship log
            for (session_id, session_record) in &authorship_log.metadata.sessions {
                session_records
                    .entry(session_id.clone())
                    .or_insert_with(|| session_record.clone());
            }

            // Check each line in this hunk for AI authorship using compact schema
            // IMPORTANT: Use the original line numbers from the commit, not the current line numbers
            // Use the original filename from git blame (handles renames)
            let lookup_path = hunk.orig_filename.as_deref().unwrap_or(file_path);
            let num_lines = hunk.range.1 - hunk.range.0 + 1;
            for i in 0..num_lines {
                let current_line_num = hunk.range.0 + i;
                let orig_line_num = hunk.orig_range.0 + i;

                if let Some((author, prompt_hash, prompt)) = authorship_log.get_line_attribution(
                    repo,
                    lookup_path,
                    orig_line_num,
                    &mut foreign_prompts_cache,
                ) {
                    // If this line is AI-assisted, display the tool name; otherwise the human username
                    if let Some(prompt_record) = prompt {
                        let prompt_hash = prompt_hash.unwrap();
                        // Track that this prompt hash appears in this commit
                        prompt_commits
                            .entry(prompt_hash.clone())
                            .or_default()
                            .insert(hunk.commit_sha.clone());
                        if options.use_prompt_hashes_as_names {
                            line_authors.insert(current_line_num, prompt_hash.clone());
                        } else {
                            line_authors
                                .insert(current_line_num, prompt_record.agent_id.tool.clone());
                        }

                        prompt_records.insert(prompt_hash, prompt_record.clone());
                    } else if let Some(ref hash) = prompt_hash
                        && hash.starts_with("h_")
                    {
                        // Known human attestation (h_-prefixed hash from KnownHuman checkpoint)
                        if options.use_prompt_hashes_as_names {
                            line_authors.insert(current_line_num, hash.clone());
                        } else if options.return_human_authors_as_human {
                            line_authors.insert(
                                current_line_num,
                                CheckpointKind::Human.to_str().to_string(),
                            );
                        } else {
                            line_authors.insert(current_line_num, author.username.clone());
                        }
                    } else {
                        // Has authorship log but line not AI and not KnownHuman = unattested
                        if options.return_human_authors_as_human {
                            line_authors.insert(
                                current_line_num,
                                CheckpointKind::Human.to_str().to_string(),
                            );
                        } else {
                            line_authors.insert(current_line_num, author.username.clone());
                        }
                    }
                } else {
                    // Has authorship log but no attribution found = unattested (unknown)
                    if options.return_human_authors_as_human {
                        line_authors
                            .insert(current_line_num, CheckpointKind::Human.to_str().to_string());
                    } else {
                        line_authors.insert(current_line_num, hunk.original_author.clone());
                    }
                }
            }
        } else if let Some(tool) =
            crate::authorship::agent_detection::match_email_to_agent(&hunk.author_email)
        {
            // No authorship log, but commit author email matches a known AI agent.
            // Simulate authorship data so this commit is attributed to the agent.
            let (simulated_log, prompt_hash) =
                crate::authorship::agent_detection::simulate_agent_authorship(
                    &hunk.commit_sha,
                    tool,
                    file_path,
                    hunk.range.0,
                    hunk.range.1,
                );

            // Merge this hunk's simulated data into a per-commit simulated log.
            // (A single agent commit can produce multiple non-contiguous blame hunks.)
            simulated_authorship_logs
                .entry(hunk.commit_sha.clone())
                .and_modify(|existing| {
                    // Merge attestation entries for this file
                    if let Some(file_attestation) = simulated_log.attestations.first() {
                        for entry in &file_attestation.entries {
                            existing
                                .get_or_create_file(file_path)
                                .add_entry(entry.clone());
                        }
                    }

                    // Merge prompt stats (sum line counts across hunks)
                    if let Some(pr) = simulated_log.metadata.prompts.get(&prompt_hash) {
                        if let Some(existing_pr) = existing.metadata.prompts.get_mut(&prompt_hash) {
                            existing_pr.total_additions += pr.total_additions;
                            existing_pr.accepted_lines += pr.accepted_lines;
                        } else {
                            existing
                                .metadata
                                .prompts
                                .insert(prompt_hash.clone(), pr.clone());
                        }
                    }
                })
                .or_insert_with(|| simulated_log.clone());

            // Insert (merged) prompt record and track commits
            if let Some(pr) = simulated_authorship_logs
                .get(&hunk.commit_sha)
                .and_then(|log| log.metadata.prompts.get(&prompt_hash))
            {
                prompt_records.insert(prompt_hash.clone(), pr.clone());
                prompt_commits
                    .entry(prompt_hash.clone())
                    .or_default()
                    .insert(hunk.commit_sha.clone());
            }

            // Mark all lines in this hunk as AI-authored by the detected tool
            for line_num in hunk.range.0..=hunk.range.1 {
                if options.use_prompt_hashes_as_names {
                    line_authors.insert(line_num, prompt_hash.clone());
                } else {
                    line_authors.insert(line_num, tool.to_string());
                }
            }
        } else {
            // No authorship log for this commit and not a known agent
            for line_num in hunk.range.0..=hunk.range.1 {
                if options.mark_unknown {
                    // User wants explicit distinction - mark as Unknown
                    line_authors.insert(line_num, "Unknown".to_string());
                } else if options.return_human_authors_as_human {
                    line_authors.insert(line_num, CheckpointKind::Human.to_str().to_string());
                } else {
                    line_authors.insert(line_num, hunk.original_author.clone());
                }
            }
        }
    }

    // Collect all authorship logs we've seen (for JSON output to find other files)
    let mut authorship_logs: Vec<AuthorshipLog> =
        commit_authorship_cache.into_values().flatten().collect();
    authorship_logs.extend(simulated_authorship_logs.into_values());

    // Convert HashSet to Vec and sort for deterministic output
    let prompt_commits_vec: HashMap<String, Vec<String>> = prompt_commits
        .into_iter()
        .map(|(hash, commits)| {
            let mut commits_vec: Vec<String> = commits.into_iter().collect();
            commits_vec.sort();
            (hash, commits_vec)
        })
        .collect();

    Ok((
        line_authors,
        prompt_records,
        session_records,
        humans,
        authorship_logs,
        prompt_commits_vec,
        commits_with_notes,
    ))
}

/// Metadata about user's auth state and git identity
#[derive(Debug, Serialize)]
struct BlameMetadata {
    is_logged_in: bool,
    current_user: Option<String>,
}

/// JSON output structure for blame
#[derive(Debug, Serialize)]
struct JsonBlameOutput {
    lines: std::collections::BTreeMap<String, String>,
    prompts: HashMap<String, PromptRecordWithOtherFiles>,
    metadata: BlameMetadata,
}

/// Read model that patches PromptRecord with other_files and commits fields
#[derive(Debug, Serialize)]
struct PromptRecordWithOtherFiles {
    #[serde(flatten)]
    prompt_record: PromptRecord,
    other_files: Vec<String>,
    commits: Vec<String>,
}

/// Helper function to get all files touched by a prompt hash across authorship logs
fn get_files_for_prompt_hash(
    prompt_hash: &str,
    authorship_logs: &[AuthorshipLog],
    exclude_file: &str,
) -> Vec<String> {
    let mut files = std::collections::HashSet::new();

    for log in authorship_logs {
        for file_attestation in &log.attestations {
            // Skip the file we're currently blaming
            if file_attestation.file_path == exclude_file {
                continue;
            }

            // Check if any entry in this file has the prompt hash
            let has_hash = file_attestation
                .entries
                .iter()
                .any(|entry| entry.hash == prompt_hash);

            if has_hash {
                files.insert(file_attestation.file_path.clone());
            }
        }
    }

    let mut file_vec: Vec<String> = files.into_iter().collect();
    file_vec.sort();
    file_vec
}

fn output_json_format(
    repo: &Repository,
    line_authors: &HashMap<u32, String>,
    prompt_records: &HashMap<String, PromptRecord>,
    authorship_logs: &[AuthorshipLog],
    prompt_commits: &HashMap<String, Vec<String>>,
    current_file: &str,
) -> Result<(), GitAiError> {
    // Filter to only AI lines (where author is a prompt_id in prompt_records)
    let mut ai_lines: Vec<(u32, String)> = line_authors
        .iter()
        .filter(|(_, author)| prompt_records.contains_key(*author))
        .map(|(line, author)| (*line, author.clone()))
        .collect();

    // Sort by line number
    ai_lines.sort_by_key(|(line, _)| *line);

    // Group consecutive lines with the same prompt_id into ranges
    let mut lines_map: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();

    if !ai_lines.is_empty() {
        let mut range_start = ai_lines[0].0;
        let mut range_end = ai_lines[0].0;
        let mut current_prompt_id = ai_lines[0].1.clone();

        for (line, prompt_id) in ai_lines.iter().skip(1) {
            if *prompt_id == current_prompt_id && *line == range_end + 1 {
                // Extend current range
                range_end = *line;
            } else {
                // Save current range and start new one
                let range_key = if range_start == range_end {
                    range_start.to_string()
                } else {
                    format!("{}-{}", range_start, range_end)
                };
                lines_map.insert(range_key, current_prompt_id.clone());

                range_start = *line;
                range_end = *line;
                current_prompt_id = prompt_id.clone();
            }
        }

        // Don't forget the last range
        let range_key = if range_start == range_end {
            range_start.to_string()
        } else {
            format!("{}-{}", range_start, range_end)
        };
        lines_map.insert(range_key, current_prompt_id);
    }

    // Only include prompts that are actually referenced in lines
    let referenced_prompt_ids: std::collections::HashSet<&String> = lines_map.values().collect();

    // Create read models with other_files and commits populated
    let filtered_prompts: HashMap<String, PromptRecordWithOtherFiles> = prompt_records
        .iter()
        .filter(|(k, _)| referenced_prompt_ids.contains(k))
        .map(|(k, v)| {
            let other_files = get_files_for_prompt_hash(k, authorship_logs, current_file);
            let commits = prompt_commits.get(k).cloned().unwrap_or_default();
            (
                k.clone(),
                PromptRecordWithOtherFiles {
                    prompt_record: v.clone(),
                    other_files,
                    commits,
                },
            )
        })
        .collect();

    // Compute metadata
    let is_logged_in = CredentialStore::new()
        .load()
        .ok()
        .flatten()
        .map(|creds| !creds.is_refresh_token_expired())
        .unwrap_or(false);

    let current_user = repo.effective_author_identity().formatted();

    let output = JsonBlameOutput {
        lines: lines_map,
        prompts: filtered_prompts,
        metadata: BlameMetadata {
            is_logged_in,
            current_user,
        },
    };

    let json_str = serde_json::to_string_pretty(&output)
        .map_err(|e| GitAiError::Generic(format!("Failed to serialize JSON output: {}", e)))?;

    println!("{}", json_str);
    Ok(())
}

fn output_porcelain_format(
    repo: &Repository,
    _line_authors: &HashMap<u32, String>,
    file_path: &str,
    lines: &[&str],
    line_ranges: &[(u32, u32)],
    options: &GitAiBlameOptions,
    commits_with_notes: &std::collections::HashSet<String>,
) -> Result<(), GitAiError> {
    // Use options that don't split hunks to match git's native porcelain output
    let mut no_split_options = options.clone();
    no_split_options.split_hunks_by_ai_author = false;

    // Build a map from line number to BlameHunk for fast lookup
    let mut line_to_hunk: HashMap<u32, BlameHunk> = HashMap::new();
    let hunks = repo.blame_hunks_for_ranges(file_path, line_ranges, &no_split_options)?;
    for hunk in hunks {
        for line_num in hunk.range.0..=hunk.range.1 {
            line_to_hunk.insert(line_num, hunk.clone());
        }
    }
    let mut requested_lines: Vec<u32> = line_to_hunk.keys().copied().collect();
    requested_lines.sort_unstable();

    let mut last_hunk_id = None;
    let mut commit_summaries: HashMap<String, String> = HashMap::new();
    let mut seen_commits: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line_num in requested_lines {
        let line_index = (line_num - 1) as usize;
        let line_content = if line_index < lines.len() {
            lines[line_index]
        } else {
            ""
        };

        if let Some(hunk) = line_to_hunk.get(&line_num) {
            // For agent-detected commits (email matches known agent, no authorship note),
            // override the author name with the tool name. Otherwise use git's original author.
            // Only apply agent detection when no real authorship note exists for this commit.
            let author_name = if !commits_with_notes.contains(&hunk.commit_sha) {
                crate::authorship::agent_detection::match_email_to_agent(&hunk.author_email)
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

fn output_incremental_format(
    repo: &Repository,
    _line_authors: &HashMap<u32, String>,
    file_path: &str,
    _lines: &[&str],
    line_ranges: &[(u32, u32)],
    options: &GitAiBlameOptions,
    commits_with_notes: &std::collections::HashSet<String>,
) -> Result<(), GitAiError> {
    // Use options that don't split hunks to match git's native incremental output
    let mut no_split_options = options.clone();
    no_split_options.split_hunks_by_ai_author = false;

    // Build a map from line number to BlameHunk for fast lookup
    let mut line_to_hunk: HashMap<u32, BlameHunk> = HashMap::new();
    let hunks = repo.blame_hunks_for_ranges(file_path, line_ranges, &no_split_options)?;
    for hunk in hunks {
        for line_num in hunk.range.0..=hunk.range.1 {
            line_to_hunk.insert(line_num, hunk.clone());
        }
    }
    let mut requested_lines: Vec<u32> = line_to_hunk.keys().copied().collect();
    requested_lines.sort_unstable();

    let mut last_hunk_id = None;
    let mut commit_summaries: HashMap<String, String> = HashMap::new();
    let mut seen_commits: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line_num in requested_lines {
        if let Some(hunk) = line_to_hunk.get(&line_num) {
            // For agent-detected commits (email matches known agent, no authorship note),
            // override the author name with the tool name. Otherwise use git's original author.
            // Only apply agent detection when no real authorship note exists for this commit.
            let author_name = if !commits_with_notes.contains(&hunk.commit_sha) {
                crate::authorship::agent_detection::match_email_to_agent(&hunk.author_email)
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

fn output_default_format(
    repo: &Repository,
    line_authors: &HashMap<u32, String>,
    prompt_records: &HashMap<String, PromptRecord>,
    file_path: &str,
    lines: &[&str],
    line_ranges: &[(u32, u32)],
    options: &GitAiBlameOptions,
) -> Result<(), GitAiError> {
    let mut output = String::new();

    // Use options that don't split hunks for formatting purposes
    let mut no_split_options = options.clone();
    no_split_options.split_hunks_by_ai_author = false;

    let hunks = repo.blame_hunks_for_ranges(file_path, line_ranges, &no_split_options)?;

    // Build a map from line number to BlameHunk for fast lookup
    let mut line_to_hunk: HashMap<u32, BlameHunk> = HashMap::new();
    for hunk in &hunks {
        for line_num in hunk.range.0..=hunk.range.1 {
            line_to_hunk.insert(line_num, hunk.clone());
        }
    }
    let mut requested_lines: Vec<u32> = line_to_hunk.keys().copied().collect();
    requested_lines.sort_unstable();

    // Calculate the maximum line number width for proper padding
    let max_line_num = lines.len() as u32;
    let line_num_width = max_line_num.to_string().len();

    // Calculate the maximum author name width for proper padding
    let mut max_author_width = 0;
    for hunk in &hunks {
        let author = line_authors
            .get(&hunk.range.0)
            .unwrap_or(&hunk.original_author);
        let author_display = if options.suppress_author {
            "".to_string()
        } else if options.show_prompt && prompt_records.contains_key(author) {
            let prompt = &prompt_records[author];
            let short_hash = &author[..7.min(author.len())];
            format!("{} [{}]", prompt.agent_id.tool, short_hash)
        } else if options.show_email {
            format!("{} <{}>", author, &hunk.author_email)
        } else {
            author.to_string()
        };
        max_author_width = max_author_width.max(author_display.len());
    }

    let blank_boundary_hash_width = if options.long_rev {
        40
    } else {
        ((options.abbrev.unwrap_or(7).max(1) as usize) + 1).min(40)
    };

    for line_num in requested_lines {
        let line_index = (line_num - 1) as usize;
        let line_content = if line_index < lines.len() {
            lines[line_index]
        } else {
            ""
        };

        if let Some(hunk) = line_to_hunk.get(&line_num) {
            let sha = &hunk.abbrev_sha;

            // Match git blame boundary formatting:
            // - default boundary: prefix abbreviated hash with '^'
            // - -b/--blank-boundary: print a blank hash column
            let full_sha = if hunk.is_boundary && options.blank_boundary && !options.show_root {
                " ".repeat(blank_boundary_hash_width)
            } else {
                let boundary_marker = if hunk.is_boundary && !options.show_root {
                    "^"
                } else {
                    ""
                };
                format!("{}{}", boundary_marker, sha)
            };

            // Get the author for this line (AI authorship or original)
            let author = line_authors.get(&line_num).unwrap_or(&hunk.original_author);

            // Format date according to options
            let date_str = format_blame_date(hunk.author_time, &hunk.author_tz, options);

            // Handle different output formats based on flags
            let author_display = if options.suppress_author {
                "".to_string()
            } else if options.show_prompt && prompt_records.contains_key(author) {
                let prompt = &prompt_records[author];
                let short_hash = &author[..7.min(author.len())];
                format!("{} [{}]", prompt.agent_id.tool, short_hash)
            } else if options.show_email {
                format!("{} <{}>", author, &hunk.author_email)
            } else {
                author.to_string()
            };

            // Pad author name to consistent width
            let padded_author = if max_author_width > 0 {
                format!("{:<width$}", author_display, width = max_author_width)
            } else {
                author_display
            };

            let _filename_display = if options.show_name {
                format!("{} ", file_path)
            } else {
                "".to_string()
            };

            let _number_display = if options.show_number {
                format!("{} ", line_num)
            } else {
                "".to_string()
            };

            // Format exactly like git blame: sha (author date line) code
            if options.suppress_author {
                // Suppress author format: sha line_number) code
                output.push_str(&format!("{} {}) {}\n", full_sha, line_num, line_content));
            } else {
                // Normal format: sha (author date line) code
                if options.show_name {
                    // Show filename format: sha filename (author date line) code
                    output.push_str(&format!(
                        "{} {} ({} {} {:>width$}) {}\n",
                        full_sha,
                        file_path,
                        padded_author,
                        date_str,
                        line_num,
                        line_content,
                        width = line_num_width
                    ));
                } else if options.show_number {
                    // Show number format: sha line_number (author date line) code (matches git's -n output)
                    output.push_str(&format!(
                        "{} {} ({} {} {:>width$}) {}\n",
                        full_sha,
                        line_num,
                        padded_author,
                        date_str,
                        line_num,
                        line_content,
                        width = line_num_width
                    ));
                } else {
                    // Normal format: sha (author date line) code
                    output.push_str(&format!(
                        "{} ({} {} {:>width$}) {}\n",
                        full_sha,
                        padded_author,
                        date_str,
                        line_num,
                        line_content,
                        width = line_num_width
                    ));
                }
            }
        } else {
            // Fallback for lines without blame info
            output.push_str(&format!(
                "{:<8} (unknown        1970-01-01 00:00:00 +0000    {:>width$}) {}\n",
                "????????",
                line_num,
                line_content,
                width = line_num_width
            ));
        }
    }

    // Print stats if requested (at the end, like git blame)
    if options.show_stats {
        // Append git-like stats lines to output string
        let stats = "num read blob: 1\nnum get patch: 0\nnum commits: 0\n";
        output.push_str(stats);
    }

    // Output handling - respect pager environment variables
    let pager = std::env::var("GIT_PAGER")
        .or_else(|_| std::env::var("PAGER"))
        .unwrap_or_else(|_| "less".to_string());

    // If pager is set to "cat" or empty, output directly
    if pager == "cat" || pager.is_empty() {
        print!("{}", output);
    } else if io::stdout().is_terminal() {
        // Try to use the specified pager
        match std::process::Command::new(&pager)
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(stdin) = child.stdin.as_mut() {
                    if stdin.write_all(output.as_bytes()).is_ok() {
                        let _ = child.wait();
                    } else {
                        // Fall back to direct output if pager fails
                        print!("{}", output);
                    }
                } else {
                    // Fall back to direct output if pager fails
                    print!("{}", output);
                }
            }
            Err(_) => {
                // Fall back to direct output if pager fails
                print!("{}", output);
            }
        }
    } else {
        // Not a terminal, output directly
        print!("{}", output);
    }
    Ok(())
}

fn format_blame_date(author_time: i64, author_tz: &str, options: &GitAiBlameOptions) -> String {
    let dt = DateTime::from_timestamp(author_time, 0)
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap());

    // Parse timezone string like +0200 or -0500
    let offset = if author_tz.len() == 5 {
        let sign = if &author_tz[0..1] == "+" { 1 } else { -1 };
        let hours: i32 = author_tz[1..3].parse().unwrap_or(0);
        let mins: i32 = author_tz[3..5].parse().unwrap_or(0);
        FixedOffset::east_opt(sign * (hours * 3600 + mins * 60))
            .unwrap_or(FixedOffset::east_opt(0).unwrap())
    } else {
        FixedOffset::east_opt(0).unwrap()
    };

    let dt = offset.from_utc_datetime(&dt.naive_utc());

    // Format date according to options (default: iso)
    if let Some(fmt) = &options.date_format {
        // TODO: support all git date formats
        match fmt.as_str() {
            "iso" | "iso8601" => dt.format("%Y-%m-%d %H:%M:%S %z").to_string(),
            "short" => dt.format("%Y-%m-%d").to_string(),
            "relative" => format!("{} seconds ago", (Utc::now().timestamp() - author_time)),
            _ => dt.format("%Y-%m-%d %H:%M:%S %z").to_string(),
        }
    } else {
        dt.format("%Y-%m-%d %H:%M:%S %z").to_string()
    }
}

pub fn parse_blame_args(args: &[String]) -> Result<(String, GitAiBlameOptions), GitAiError> {
    let mut options = GitAiBlameOptions::default();
    let mut file_path = None;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            // Line range options
            "-L" => {
                if i + 1 >= args.len() {
                    return Err(GitAiError::Generic("Missing argument for -L".to_string()));
                }
                let range_str = &args[i + 1];
                if let Some((start, end)) = parse_line_range(range_str) {
                    options.line_ranges.push((start, end));
                } else {
                    return Err(GitAiError::Generic(format!(
                        "Invalid line range: {}",
                        range_str
                    )));
                }
                i += 2;
            }

            // Output format options
            "--porcelain" => {
                options.porcelain = true;
                i += 1;
            }
            "--line-porcelain" => {
                options.line_porcelain = true;
                options.porcelain = true; // Implies --porcelain
                i += 1;
            }
            "--incremental" => {
                options.incremental = true;
                i += 1;
            }
            "-f" | "--show-name" => {
                options.show_name = true;
                i += 1;
            }
            "-n" | "--show-number" => {
                options.show_number = true;
                i += 1;
            }
            "-e" | "--show-email" => {
                options.show_email = true;
                i += 1;
            }
            "-s" => {
                options.suppress_author = true;
                i += 1;
            }
            "--show-stats" => {
                options.show_stats = true;
                i += 1;
            }

            // Commit display options
            "-l" => {
                options.long_rev = true;
                i += 1;
            }
            "-t" => {
                options.raw_timestamp = true;
                i += 1;
            }
            "--abbrev" => {
                if i + 1 >= args.len() {
                    return Err(GitAiError::Generic(
                        "Missing argument for --abbrev".to_string(),
                    ));
                }
                if let Ok(n) = args[i + 1].parse::<u32>() {
                    options.abbrev = Some(n);
                } else {
                    return Err(GitAiError::Generic(
                        "Invalid number for --abbrev".to_string(),
                    ));
                }
                i += 2;
            }

            // Boundary options
            "-b" => {
                options.blank_boundary = true;
                i += 1;
            }
            "--root" => {
                options.show_root = true;
                i += 1;
            }

            // Movement detection options
            "-M" => {
                options.detect_moves = true;
                if i + 1 < args.len() {
                    if let Ok(threshold) = args[i + 1].parse::<u32>() {
                        options.move_threshold = Some(threshold);
                        i += 2;
                    } else {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            "-C" => {
                options.detect_copies = (options.detect_copies + 1).min(3);
                if i + 1 < args.len() {
                    if let Ok(threshold) = args[i + 1].parse::<u32>() {
                        options.move_threshold = Some(threshold);
                        i += 2;
                    } else {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }

            // Ignore options
            "--ignore-rev" => {
                if i + 1 >= args.len() {
                    return Err(GitAiError::Generic(
                        "Missing argument for --ignore-rev".to_string(),
                    ));
                }
                options.ignore_revs.push(args[i + 1].clone());
                i += 2;
            }
            "--ignore-revs-file" => {
                if i + 1 >= args.len() {
                    return Err(GitAiError::Generic(
                        "Missing argument for --ignore-revs-file".to_string(),
                    ));
                }
                options.ignore_revs_file = Some(args[i + 1].clone());
                i += 2;
            }
            "--no-ignore-revs-file" => {
                // Disable auto-detection of .git-blame-ignore-revs file
                options.no_ignore_revs_file = true;
                i += 1;
            }

            // Color options
            "--color-lines" => {
                options.color_lines = true;
                i += 1;
            }
            "--color-by-age" => {
                options.color_by_age = true;
                i += 1;
            }

            // Progress options
            "--progress" => {
                options.progress = true;
                i += 1;
            }

            // Date format
            "--date" => {
                if i + 1 >= args.len() {
                    return Err(GitAiError::Generic(
                        "Missing argument for --date".to_string(),
                    ));
                }
                options.date_format = Some(args[i + 1].clone());
                i += 2;
            }

            // Content options
            "--contents" => {
                if i + 1 >= args.len() {
                    return Err(GitAiError::Generic(
                        "Missing argument for --contents".to_string(),
                    ));
                }
                let contents_arg = &args[i + 1];
                options.contents_file = Some(contents_arg.clone());

                // Read the contents now - either from stdin or from a file
                let data = if contents_arg == "-" {
                    // Read from stdin
                    use std::io::Read;
                    let mut buffer = Vec::new();
                    io::stdin().read_to_end(&mut buffer).map_err(|e| {
                        GitAiError::Generic(format!("Failed to read from stdin: {}", e))
                    })?;
                    buffer
                } else {
                    // Read from file
                    fs::read(contents_arg).map_err(|e| {
                        GitAiError::Generic(format!(
                            "Failed to read contents file '{}': {}",
                            contents_arg, e
                        ))
                    })?
                };
                options.contents_data = Some(data);
                i += 2;
            }

            // Revision options
            "--reverse" => {
                if i + 1 >= args.len() {
                    return Err(GitAiError::Generic(
                        "Missing argument for --reverse".to_string(),
                    ));
                }
                options.reverse = Some(args[i + 1].clone());
                i += 2;
            }
            "--first-parent" => {
                options.first_parent = true;
                i += 1;
            }

            // Encoding
            "--encoding" => {
                if i + 1 >= args.len() {
                    return Err(GitAiError::Generic(
                        "Missing argument for --encoding".to_string(),
                    ));
                }
                options.encoding = Some(args[i + 1].clone());
                i += 2;
            }

            // Date filtering
            "--since" => {
                if i + 1 >= args.len() {
                    return Err(GitAiError::Generic(
                        "Missing argument for --since".to_string(),
                    ));
                }
                options.oldest_date =
                    Some(DateTime::parse_from_rfc3339(&args[i + 1]).map_err(|e| {
                        GitAiError::Generic(format!("Invalid date format for --since: {}", e))
                    })?);
                i += 2;
            }
            // JSON output format
            "--json" => {
                options.json = true;
                i += 1;
            }

            // Mark unknown authorship
            "--mark-unknown" => {
                options.mark_unknown = true;
                i += 1;
            }

            // Show prompt hashes inline
            "--show-prompt" => {
                options.show_prompt = true;
                i += 1;
            }

            // File path (non-option argument)
            arg if !arg.starts_with('-') => {
                if file_path.is_none() {
                    file_path = Some(arg.to_string());
                } else {
                    return Err(GitAiError::Generic(
                        "Multiple file paths specified".to_string(),
                    ));
                }
                i += 1;
            }

            // Unknown option
            _ => {
                return Err(GitAiError::Generic(format!("Unknown option: {}", args[i])));
            }
        }
    }

    let file_path =
        file_path.ok_or_else(|| GitAiError::Generic("No file path specified".to_string()))?;

    Ok((file_path, options))
}

fn parse_line_range(range_str: &str) -> Option<(u32, u32)> {
    if let Some(dash_pos) = range_str.find(',') {
        let start_str = &range_str[..dash_pos];
        let end_str = &range_str[dash_pos + 1..];

        if let (Ok(start), Ok(end)) = (start_str.parse::<u32>(), end_str.parse::<u32>()) {
            return Some((start, end));
        }
    } else if let Ok(line) = range_str.parse::<u32>() {
        return Some((line, line));
    }

    None
}
