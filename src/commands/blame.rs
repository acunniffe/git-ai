use crate::error::GitAiError;
use crate::git::refs::get_reference_as_authorship_log;
use crate::log_fmt::authorship_log::AuthorshipLog;
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use git2::{BlameOptions, Repository};
use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct BlameHunk {
    /// Line range [start, end] (inclusive)
    pub range: (u32, u32),
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
}

#[derive(Debug, Clone)]
pub struct GitAiBlameOptions {
    // Line range options
    pub line_ranges: Vec<(u32, u32)>,

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
    pub revision: Option<String>,
    pub reverse: Option<String>,
    pub first_parent: bool,

    // Encoding
    pub encoding: Option<String>,
}

impl Default for GitAiBlameOptions {
    fn default() -> Self {
        Self {
            line_ranges: Vec::new(),
            porcelain: false,
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
            color_lines: false,
            color_by_age: false,
            progress: false,
            date_format: None,
            contents_file: None,
            revision: None,
            reverse: None,
            first_parent: false,
            encoding: None,
        }
    }
}

pub fn run(
    repo: &Repository,
    file_path: &str,
    options: &GitAiBlameOptions,
) -> Result<HashMap<u32, String>, GitAiError> {
    // Use repo root for file system operations
    let repo_root = repo
        .workdir()
        .ok_or_else(|| GitAiError::Generic("Repository has no working directory".to_string()))?;
    let abs_file_path = repo_root.join(file_path);

    // Validate that the file exists
    if !abs_file_path.exists() {
        return Err(GitAiError::Generic(format!(
            "File not found: {}",
            abs_file_path.display()
        )));
    }

    // Read the current file content
    let file_content = fs::read_to_string(&abs_file_path)?;
    let lines: Vec<&str> = file_content.lines().collect();
    let total_lines = lines.len() as u32;

    // Determine the line ranges to process
    let line_ranges = if options.line_ranges.is_empty() {
        vec![(1, total_lines)]
    } else {
        options.line_ranges.clone()
    };

    // Validate line ranges
    for (start, end) in &line_ranges {
        if *start == 0 || *end == 0 || start > end || *end > total_lines {
            return Err(GitAiError::Generic(format!(
                "Invalid line range: {}:{}. File has {} lines",
                start, end, total_lines
            )));
        }
    }

    // Step 1: Get Git's native blame for all ranges
    let mut all_blame_hunks = Vec::new();
    for (start_line, end_line) in &line_ranges {
        let hunks = get_git_blame_hunks(repo, file_path, *start_line, *end_line, options)?;
        all_blame_hunks.extend(hunks);
    }

    // Step 2: Overlay AI authorship information
    let line_authors = overlay_ai_authorship(repo, &all_blame_hunks, file_path)?;

    // Output based on format
    if options.porcelain || options.line_porcelain {
        output_porcelain_format(
            repo,
            &line_authors,
            file_path,
            &lines,
            &line_ranges,
            options,
        )?;
    } else if options.incremental {
        output_incremental_format(
            repo,
            &line_authors,
            file_path,
            &lines,
            &line_ranges,
            options,
        )?;
    } else {
        output_default_format(
            repo,
            &line_authors,
            file_path,
            &lines,
            &line_ranges,
            options,
        )?;
    }

    Ok(line_authors)
}

pub fn get_git_blame_hunks(
    repo: &Repository,
    file_path: &str,
    start_line: u32,
    end_line: u32,
    options: &GitAiBlameOptions,
) -> Result<Vec<BlameHunk>, GitAiError> {
    let mut blame_opts = BlameOptions::new();
    blame_opts.min_line(start_line.try_into().unwrap());
    blame_opts.max_line(end_line.try_into().unwrap());

    // Apply boundary options
    if options.blank_boundary {
        // Note: git2 doesn't have a direct equivalent to git blame's -b flag
        // We'll handle boundary detection in the output formatting
    }
    if options.show_root {
        // Note: git2 doesn't have a direct equivalent to git blame's --root flag
        // We'll handle root commit detection in the output formatting
    }

    let blame = repo.blame_file(Path::new(file_path), Some(&mut blame_opts))?;
    let mut hunks = Vec::new();

    let num_hunks = blame.len();
    for i in 0..num_hunks {
        let hunk = blame
            .get_index(i)
            .ok_or_else(|| GitAiError::Generic("Failed to get blame hunk".to_string()))?;

        let start = hunk.final_start_line(); // Already 1-indexed
        let end = start + hunk.lines_in_hunk() - 1;

        let commit_id = hunk.final_commit_id();
        let commit = match repo.find_commit(commit_id) {
            Ok(commit) => commit,
            Err(_) => {
                continue; // Skip this hunk if we can't find the commit
            }
        };

        let author = commit.author();
        let committer = commit.committer();
        let commit_sha = commit_id.to_string();

        // Determine hash length based on options
        let hash_len = if options.long_rev {
            40 // Full hash for long revision
        } else if let Some(abbrev) = options.abbrev {
            abbrev as usize
        } else {
            7 // Default 7 chars
        };

        let abbrev_sha = if hash_len < commit_sha.len() {
            commit_sha[..hash_len].to_string()
        } else {
            commit_sha.clone()
        };

        let original_author = author.name().unwrap_or("unknown").to_string();
        let author_email = author.email().unwrap_or("").to_string();
        let author_time = author.when().seconds();
        let author_tz = format!(
            "{:+03}{:02}",
            author.when().offset_minutes() / 60,
            (author.when().offset_minutes().abs() % 60)
        );
        let committer_name = committer.name().unwrap_or("").to_string();
        let committer_email = committer.email().unwrap_or("").to_string();
        let committer_time = committer.when().seconds();
        let committer_tz = format!(
            "{:+03}{:02}",
            committer.when().offset_minutes() / 60,
            (committer.when().offset_minutes().abs() % 60)
        );

        // Check if this is a boundary commit (has no parent)
        let is_boundary = commit.parent_count() == 0;

        hunks.push(BlameHunk {
            range: (start.try_into().unwrap(), end.try_into().unwrap()),
            commit_sha,
            abbrev_sha,
            original_author,
            author_email,
            author_time,
            author_tz,
            committer: committer_name,
            committer_email,
            committer_time,
            committer_tz,
            is_boundary,
        });
    }

    Ok(hunks)
}

pub fn overlay_ai_authorship(
    repo: &Repository,
    blame_hunks: &[BlameHunk],
    file_path: &str,
) -> Result<HashMap<u32, String>, GitAiError> {
    let mut line_authors: HashMap<u32, String> = HashMap::new();

    // Group hunks by commit SHA to avoid repeated lookups
    let mut commit_authorship_cache: HashMap<String, Option<AuthorshipLog>> = HashMap::new();

    for hunk in blame_hunks {
        // Check if we've already looked up this commit's authorship
        let authorship_log = if let Some(cached) = commit_authorship_cache.get(&hunk.commit_sha) {
            cached.clone()
        } else {
            // Try to get authorship log for this commit
            let ref_name = format!("ai/authorship/{}", hunk.commit_sha);
            let authorship = match get_reference_as_authorship_log(repo, &ref_name) {
                Ok(log) => Some(log),
                Err(_) => None, // No AI authorship data for this commit
            };
            commit_authorship_cache.insert(hunk.commit_sha.clone(), authorship.clone());
            authorship
        };

        // If we have AI authorship data, look up the author for lines in this hunk
        if let Some(authorship_log) = authorship_log {
            if let Some(file_authorship) = authorship_log.files.get(file_path) {
                // Check each line in this hunk for AI authorship
                for line_num in hunk.range.0..=hunk.range.1 {
                    if let Some(author) = file_authorship.get_author(line_num) {
                        line_authors.insert(line_num, author.to_string());
                    } else {
                        // Fall back to original author if no AI authorship
                        line_authors.insert(line_num, hunk.original_author.clone());
                    }
                }
            } else {
                // No file authorship data, use original author for all lines in hunk
                for line_num in hunk.range.0..=hunk.range.1 {
                    line_authors.insert(line_num, hunk.original_author.clone());
                }
            }
        } else {
            // No authorship log, use original author for all lines in hunk
            for line_num in hunk.range.0..=hunk.range.1 {
                line_authors.insert(line_num, hunk.original_author.clone());
            }
        }
    }

    Ok(line_authors)
}

#[allow(unused_variables)]
#[allow(dead_code)]
fn print_blame_summary(line_authors: &HashMap<u32, String>, start_line: u32, end_line: u32) {
    println!("{}", "=".repeat(80));

    let mut author_stats: HashMap<String, u32> = HashMap::new();
    let mut total_lines = 0;

    for line_num in start_line..=end_line {
        let author = line_authors
            .get(&line_num)
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        *author_stats.entry(author.to_string()).or_insert(0) += 1;
        total_lines += 1;
    }

    // Find the longest author name for column width
    let max_author_len = author_stats
        .keys()
        .map(|name| name.len())
        .max()
        .unwrap_or(0);

    // Sort authors by line count (descending)
    let mut sorted_authors: Vec<_> = author_stats.iter().collect();
    sorted_authors.sort_by(|a, b| b.1.cmp(a.1));

    for (author, count) in sorted_authors {
        let percentage = (*count as f64 / total_lines as f64) * 100.0;
        println!(
            "{:>width$} {:>5} {:>8.1}%",
            author,
            count,
            percentage,
            width = max_author_len
        );
    }
}

fn output_porcelain_format(
    repo: &Repository,
    _line_authors: &HashMap<u32, String>,
    file_path: &str,
    lines: &[&str],
    line_ranges: &[(u32, u32)],
    options: &GitAiBlameOptions,
) -> Result<(), GitAiError> {
    // Build a map from line number to BlameHunk for fast lookup
    let mut line_to_hunk: HashMap<u32, BlameHunk> = HashMap::new();
    for (start_line, end_line) in line_ranges {
        let h = get_git_blame_hunks(repo, file_path, *start_line, *end_line, options)?;
        for hunk in h {
            for line_num in hunk.range.0..=hunk.range.1 {
                line_to_hunk.insert(line_num, hunk.clone());
            }
        }
    }

    let mut last_hunk_id = None;
    for (start_line, end_line) in line_ranges {
        for line_num in *start_line..=*end_line {
            let line_index = (line_num - 1) as usize;
            let line_content = if line_index < lines.len() {
                lines[line_index]
            } else {
                ""
            };

            if let Some(hunk) = line_to_hunk.get(&line_num) {
                let author_name = &hunk.original_author;
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

                // Retrieve the commit summary directly from the commit object
                let commit = repo.find_commit(git2::Oid::from_str(commit_sha).unwrap())?;
                let summary = commit.summary().unwrap_or("unknown");

                let hunk_id = (commit_sha.clone(), hunk.range.0);
                if options.line_porcelain {
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
                        // Print full block for first line of hunk
                        println!(
                            "{} {} {} {}",
                            commit_sha,
                            line_num,
                            line_num,
                            hunk.range.1 - hunk.range.0 + 1
                        );
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
                        last_hunk_id = Some(hunk_id);
                    } else {
                        // For subsequent lines, print only the header and content (no metadata block)
                        println!("{} {} {}", commit_sha, line_num, line_num);
                        println!("\t{}", line_content);
                    }
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
) -> Result<(), GitAiError> {
    // Build a map from line number to BlameHunk for fast lookup
    let mut line_to_hunk: HashMap<u32, BlameHunk> = HashMap::new();
    for (start_line, end_line) in line_ranges {
        let h = get_git_blame_hunks(repo, file_path, *start_line, *end_line, options)?;
        for hunk in h {
            for line_num in hunk.range.0..=hunk.range.1 {
                line_to_hunk.insert(line_num, hunk.clone());
            }
        }
    }

    let mut last_hunk_id = None;
    for (start_line, end_line) in line_ranges {
        for line_num in *start_line..=*end_line {
            if let Some(hunk) = line_to_hunk.get(&line_num) {
                // For incremental format, use the original git author, not AI authorship
                let author_name = &hunk.original_author;
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
                    // Print full block - match git's format exactly
                    println!(
                        "{} {} {} {}",
                        commit_sha,
                        line_num,
                        line_num,
                        hunk.range.1 - hunk.range.0 + 1
                    );
                    println!("author {}", author_name);
                    println!("author-mail <{}>", author_email);
                    println!("author-time {}", author_time);
                    println!("author-tz {}", author_tz);
                    println!("committer {}", committer_name);
                    println!("committer-mail <{}>", committer_email);
                    println!("committer-time {}", committer_time);
                    println!("committer-tz {}", committer_tz);
                    println!("summary Initial commit");
                    if hunk.is_boundary {
                        println!("boundary");
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
    }
    Ok(())
}

fn output_default_format(
    repo: &Repository,
    line_authors: &HashMap<u32, String>,
    file_path: &str,
    lines: &[&str],
    line_ranges: &[(u32, u32)],
    options: &GitAiBlameOptions,
) -> Result<(), GitAiError> {
    let mut output = String::new();

    // Build a map from line number to BlameHunk for fast lookup
    let mut line_to_hunk: HashMap<u32, BlameHunk> = HashMap::new();
    for (start_line, end_line) in line_ranges {
        let h = get_git_blame_hunks(repo, file_path, *start_line, *end_line, options)?;
        for hunk in h {
            for line_num in hunk.range.0..=hunk.range.1 {
                line_to_hunk.insert(line_num, hunk.clone());
            }
        }
    }

    for (start_line, end_line) in line_ranges {
        for line_num in *start_line..=*end_line {
            let line_index = (line_num - 1) as usize;
            let line_content = if line_index < lines.len() {
                lines[line_index]
            } else {
                ""
            };

            if let Some(hunk) = line_to_hunk.get(&line_num) {
                // Determine hash length - match git blame default (7 chars)
                let hash_len = if options.long_rev {
                    40 // Full hash for long revision
                } else if let Some(abbrev) = options.abbrev {
                    abbrev as usize
                } else {
                    7 // Default 7 chars
                };
                let sha = if hash_len < hunk.commit_sha.len() {
                    &hunk.commit_sha[..hash_len]
                } else {
                    &hunk.commit_sha
                };

                // Add boundary marker if this is a boundary commit
                let boundary_marker = if hunk.is_boundary && options.blank_boundary {
                    "^"
                } else {
                    ""
                };
                let full_sha = if hunk.is_boundary && options.blank_boundary {
                    format!("{}{}", boundary_marker, "        ") // Empty hash for boundary
                } else {
                    format!("{}{}", boundary_marker, sha)
                };

                // Get the author for this line (AI authorship or original)
                let author = line_authors.get(&line_num).unwrap_or(&hunk.original_author);

                // Format date according to options
                let date_str = format_blame_date(hunk.author_time, &hunk.author_tz, options);

                // Handle different output formats based on flags
                let author_display = if options.suppress_author {
                    "".to_string()
                } else if options.show_email {
                    format!("{} <{}>", author, &hunk.author_email)
                } else {
                    author.to_string()
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
                            "{} {} ({} {} {:>4}) {}\n",
                            full_sha, file_path, author_display, date_str, line_num, line_content
                        ));
                    } else if options.show_number {
                        // Show number format: sha line_number (author date line) code (matches git's -n output)
                        output.push_str(&format!(
                            "{} {} ({} {} {:>4}) {}\n",
                            full_sha, line_num, author_display, date_str, line_num, line_content
                        ));
                    } else {
                        // Normal format: sha (author date line) code
                        output.push_str(&format!(
                            "{} ({} {} {:>4}) {}\n",
                            full_sha, author_display, date_str, line_num, line_content
                        ));
                    }
                }
            } else {
                // Fallback for lines without blame info
                output.push_str(&format!(
                    "{:<8} (unknown        1970-01-01 00:00:00 +0000    {:>4}) {}\n",
                    "????????", line_num, line_content
                ));
            }
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
                options.contents_file = Some(args[i + 1].clone());
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
