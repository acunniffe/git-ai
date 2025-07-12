use crate::error::GitAiError;
use crate::git::refs::{get_reference, put_reference};
use crate::log_fmt::working_log::{AgentMetadata, Checkpoint, Line, WorkingLogEntry};
use crate::utils::debug_log;
use git2::{Repository, StatusOptions};
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use std::collections::HashMap;
use std::path::Path;

pub fn run(
    repo: &Repository,
    author: &str,
    show_working_log: bool,
    reset: bool,
    quiet: bool,
    model: Option<&str>,
    human_author: Option<&str>,
) -> Result<(usize, usize, usize), GitAiError> {
    // Robustly handle zero-commit repos
    let base_commit = match repo.head() {
        Ok(head) => {
            if let Some(oid) = head.target() {
                oid.to_string()
            } else {
                "initial".to_string()
            }
        }
        Err(_) => "initial".to_string(),
    };

    // aidan
    let files = get_all_files(repo)?;
    let mut working_log = if reset {
        // If reset flag is set, start with an empty working log
        Vec::new()
    } else {
        get_or_create_working_log(repo, &base_commit)?
    };

    // Clear ai-working-log/diffs references when reset is true
    if reset {
        clear_working_log_diffs(repo, &base_commit)?;
    }

    if show_working_log {
        if working_log.is_empty() {
            debug_log("No working log entries found.");
        } else {
            debug_log("Working Log Entries:");
            debug_log(&format!("{}", "=".repeat(80)));
            for (i, checkpoint) in working_log.iter().enumerate() {
                debug_log(&format!("Checkpoint {}: {}", i + 1, checkpoint.snapshot));
                debug_log(&format!("  Diff: {}", checkpoint.diff));
                debug_log(&format!("  Author: {}", checkpoint.author));
                if let Some(metadata) = &checkpoint.agent_metadata {
                    debug_log("  Agent Metadata:");
                    debug_log(&format!("    Model: {}", metadata.model));
                    if let Some(human_author) = &metadata.human_author {
                        debug_log(&format!("    Human Author: {}", human_author));
                    }
                }
                debug_log("  Entries:");
                for entry in &checkpoint.entries {
                    debug_log(&format!("    File: {}", entry.file));
                    debug_log(&format!("    Added lines: {:?}", entry.added_lines));
                    debug_log(&format!("    Deleted lines: {:?}", entry.deleted_lines));
                }
                debug_log("");
            }
        }
        return Ok((0, files.len(), working_log.len()));
    }

    let previous_commit = if reset {
        None
    } else {
        working_log.last().map(|c| c.snapshot.clone())
    };

    let file_hashes: std::collections::HashMap<String, String> = files
        .iter()
        .map(|file_path| {
            let mut hasher = sha2::Sha256::new();
            hasher.update(file_path.as_bytes());
            let file_hash = format!("{:x}", hasher.finalize());
            (file_path.clone(), file_hash)
        })
        .collect();

    // Order file hashes by key and create a hash of the ordered hashes
    let mut ordered_hashes: Vec<_> = file_hashes.iter().collect();
    ordered_hashes.sort_by_key(|(file_path, _)| *file_path);

    let mut combined_hasher = Sha256::new();
    for (file_path, hash) in ordered_hashes {
        combined_hasher.update(file_path.as_bytes());
        combined_hasher.update(hash.as_bytes());
    }
    let combined_hash = format!("{:x}", combined_hasher.finalize());

    // If this is not the first checkpoint, diff against the last saved state
    let entries = if working_log.is_empty() || reset {
        // First checkpoint or reset - diff against base commit
        get_initial_checkpoint_entries(repo, &files, &base_commit)?
    } else {
        // Subsequent checkpoint - diff against last saved state
        get_subsequent_checkpoint_entries(
            repo,
            &files,
            &file_hashes,
            previous_commit.as_deref(),
            &base_commit,
        )?
    };

    let checkpoint = if let Some(model) = model {
        let agent_metadata = AgentMetadata {
            model: model.to_string(),
            human_author: human_author.map(|s| s.to_string()),
        };
        Checkpoint::new_with_metadata(
            base_commit.clone(),
            combined_hash.clone(),
            author.to_string(),
            entries.clone(),
            agent_metadata,
        )
    } else {
        Checkpoint::new(
            base_commit.clone(),
            combined_hash.clone(),
            author.to_string(),
            entries.clone(),
        )
    };

    working_log.push(checkpoint);

    // Use pretty formatting in debug builds, single-line in release builds
    let working_log_json = if cfg!(debug_assertions) {
        serde_json::to_string_pretty(&working_log)?
    } else {
        serde_json::to_string(&working_log)?
    };

    put_reference(
        repo,
        &format!("ai-working-log/{}", base_commit),
        &working_log_json,
        &format!("Checkpoint by {}", author),
    )?;

    save_current_file_states(repo, &base_commit, &files)?;

    // Print summary with new format
    if reset {
        debug_log("Working log reset. Starting fresh checkpoint.");
    }

    let label = if entries.len() > 1 {
        "checkpoint"
    } else {
        "commit"
    };

    if !quiet {
        eprintln!(
            "{} changed {} of the {} file(s) that have changed since the last {}",
            author,
            entries.len(),
            files.len(),
            label
        );
    }

    // Return the requested values: (entries_len, files_len, working_log_len)
    Ok((entries.len(), files.len(), working_log.len()))
}

fn get_all_files(repo: &Repository) -> Result<Vec<String>, GitAiError> {
    let mut files = Vec::new();

    let mut status_opts = StatusOptions::new();
    status_opts.include_untracked(true);
    status_opts.include_ignored(false);
    status_opts.include_unmodified(false);

    let statuses = repo.statuses(Some(&mut status_opts))?;
    for entry in statuses.iter() {
        if let Some(path) = entry.path() {
            // Only include text files
            if is_text_file(repo, path) {
                files.push(path.to_string());
            }
        }
    }

    // Also check for deleted files by looking at the working directory vs HEAD
    if let Ok(head) = repo.head() {
        if let Some(target) = head.target() {
            if let Ok(commit) = repo.find_commit(target) {
                if let Ok(tree) = commit.tree() {
                    // Recursively traverse the tree to find files that exist in HEAD but not in working directory
                    fn walk_tree(
                        tree: &git2::Tree,
                        repo: &Repository,
                        files: &mut Vec<String>,
                        prefix: &str,
                    ) -> Result<(), GitAiError> {
                        for entry in tree.iter() {
                            let name = entry.name().unwrap_or("");
                            let path = if prefix.is_empty() {
                                name.to_string()
                            } else {
                                format!("{}/{}", prefix, name)
                            };

                            match entry.kind() {
                                Some(git2::ObjectType::Blob) => {
                                    // Check if file exists in working directory and is a text file
                                    if !Path::new(&path).exists()
                                        && !files.contains(&path)
                                        && is_text_file(repo, &path)
                                    {
                                        files.push(path);
                                    }
                                }
                                Some(git2::ObjectType::Tree) => {
                                    if let Ok(subtree) = repo.find_tree(entry.id()) {
                                        walk_tree(&subtree, repo, files, &path)?;
                                    }
                                }
                                _ => {}
                            }
                        }
                        Ok(())
                    }

                    walk_tree(&tree, repo, &mut files, "")?;
                }
            }
        }
    }

    Ok(files)
}

fn save_current_file_states(
    repo: &Repository,
    base_commit: &str,
    files: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    let mut file_hashes = HashMap::new();

    for file_path in files {
        let repo_workdir = repo.workdir().unwrap_or_else(|| Path::new("."));
        let abs_path = repo_workdir.join(file_path);
        let content = if abs_path.exists() {
            // Read file as bytes first, then convert to string with UTF-8 lossy conversion
            match std::fs::read(&abs_path) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(_) => String::new(), // If we can't read the file, treat as empty
            }
        } else {
            String::new()
        };

        // Create a hash for the file name to use as ref name
        let mut hasher = Sha256::new();
        hasher.update(file_path.as_bytes());
        let file_hash = format!("{:x}", hasher.finalize());

        let ref_name = format!("ai-working-log/diffs/{}-{}", base_commit, file_hash);
        put_reference(
            repo,
            &ref_name,
            &content,
            &format!("File state for {}", file_path),
        )?;

        file_hashes.insert(file_path.clone(), file_hash);
    }

    Ok(file_hashes)
}

fn get_or_create_working_log(
    repo: &Repository,
    base_commit: &str,
) -> Result<Vec<Checkpoint>, GitAiError> {
    match get_reference(repo, &format!("ai-working-log/{}", base_commit)) {
        Ok(content) => {
            let working_log: Vec<Checkpoint> = serde_json::from_str(&content)?;
            Ok(working_log)
        }
        Err(_) => Ok(Vec::new()), // No working log exists yet
    }
}

fn get_initial_checkpoint_entries(
    repo: &Repository,
    files: &[String],
    base_commit: &str,
) -> Result<Vec<WorkingLogEntry>, GitAiError> {
    let mut entries = Vec::new();

    for file_path in files {
        let repo_workdir = repo.workdir().unwrap_or_else(|| Path::new("."));
        let abs_path = repo_workdir.join(file_path);

        let current_content = if abs_path.exists() {
            match std::fs::read(&abs_path) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(_) => String::new(),
            }
        } else {
            String::new()
        };

        let base_content = if base_commit == "initial" {
            String::new()
        } else {
            match git2::Oid::from_str(base_commit) {
                Ok(oid) => {
                    let commit = repo.find_commit(oid)?;
                    let tree = commit.tree()?;
                    match tree.get_path(Path::new(file_path)) {
                        Ok(entry) => {
                            let blob = repo.find_blob(entry.id())?;
                            String::from_utf8_lossy(blob.content()).to_string()
                        }
                        Err(_) => String::new(),
                    }
                }
                Err(_) => String::new(),
            }
        };

        if current_content != base_content {
            let (added_lines, deleted_lines) = get_changed_lines(&base_content, &current_content)?;
            if !added_lines.is_empty() || !deleted_lines.is_empty() {
                entries.push(WorkingLogEntry::new(
                    file_path.clone(),
                    added_lines,
                    deleted_lines,
                ));
            }
        }
    }

    Ok(entries)
}

fn get_subsequent_checkpoint_entries(
    repo: &Repository,
    files: &[String],
    file_hashes: &HashMap<String, String>,
    previous_commit: Option<&str>,
    base_commit: &str,
) -> Result<Vec<WorkingLogEntry>, GitAiError> {
    let mut entries = Vec::new();

    for file_path in files {
        let repo_workdir = repo.workdir().unwrap_or_else(|| Path::new("."));
        let abs_path = repo_workdir.join(file_path);
        let current_content = if abs_path.exists() {
            // Read file as bytes first, then convert to string with UTF-8 lossy conversion
            match std::fs::read(&abs_path) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(_) => String::new(), // If we can't read the file, treat as empty
            }
        } else {
            String::new()
        };

        // Get the previous state from refs using previous_commit, or fall back to base commit
        let previous_content = if let Some(file_hash) = file_hashes.get(file_path) {
            if let Some(prev_commit) = previous_commit {
                let ref_name = format!("ai-working-log/diffs/{}-{}", prev_commit, file_hash);
                match get_reference(repo, &ref_name) {
                    Ok(content) => content,
                    Err(_) => {
                        // Fall back to base commit tree
                        if base_commit == "initial" {
                            String::new() // No base commit (initial commit)
                        } else {
                            match git2::Oid::from_str(base_commit) {
                                Ok(oid) => {
                                    let commit = repo.find_commit(oid)?;
                                    let tree = commit.tree()?;
                                    match tree.get_path(Path::new(file_path)) {
                                        Ok(entry) => {
                                            let blob = repo.find_blob(entry.id())?;
                                            String::from_utf8_lossy(blob.content()).to_string()
                                        }
                                        Err(_) => String::new(), // File doesn't exist in base commit
                                    }
                                }
                                Err(_) => String::new(), // Invalid commit hash
                            }
                        }
                    }
                }
            } else {
                // No previous commit, fall back to base commit tree
                if base_commit == "initial" {
                    String::new() // No base commit (initial commit)
                } else {
                    match git2::Oid::from_str(base_commit) {
                        Ok(oid) => {
                            let commit = repo.find_commit(oid)?;
                            let tree = commit.tree()?;
                            match tree.get_path(Path::new(file_path)) {
                                Ok(entry) => {
                                    let blob = repo.find_blob(entry.id())?;
                                    String::from_utf8_lossy(blob.content()).to_string()
                                }
                                Err(_) => String::new(), // File doesn't exist in base commit
                            }
                        }
                        Err(_) => String::new(), // Invalid commit hash
                    }
                }
            }
        } else {
            String::new() // No file hash found
        };

        // If content changed, create diff and track changed lines
        if current_content != previous_content {
            let (added_lines, deleted_lines) =
                get_changed_lines(&previous_content, &current_content)?;
            if !added_lines.is_empty() || !deleted_lines.is_empty() {
                entries.push(WorkingLogEntry::new(
                    file_path.clone(),
                    added_lines,
                    deleted_lines,
                ));
            }
        }
    }

    Ok(entries)
}

fn get_changed_lines(
    previous_content: &str,
    current_content: &str,
) -> Result<(Vec<Line>, Vec<Line>), GitAiError> {
    let mut added_lines = Vec::new();
    let mut deleted_lines = Vec::new();

    let prev_lines: Vec<&str> = previous_content.lines().collect();
    let curr_lines: Vec<&str> = current_content.lines().collect();

    let diff = TextDiff::from_slices(&prev_lines, &curr_lines);

    let mut prev_line_num = 1;
    let mut curr_line_num = 1;

    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            match change.tag() {
                ChangeTag::Delete => {
                    deleted_lines.push(Line::Single(prev_line_num));
                    prev_line_num += 1;
                }
                ChangeTag::Insert => {
                    added_lines.push(Line::Single(curr_line_num));
                    curr_line_num += 1;
                }
                ChangeTag::Equal => {
                    prev_line_num += 1;
                    curr_line_num += 1;
                }
            }
        }
    }

    // Optimize consecutive lines into ranges
    let optimized_added_lines = optimize_line_ranges(added_lines);
    let optimized_deleted_lines = optimize_line_ranges(deleted_lines);

    Ok((optimized_added_lines, optimized_deleted_lines))
}

fn optimize_line_ranges(lines: Vec<Line>) -> Vec<Line> {
    if lines.is_empty() {
        return lines;
    }

    let mut optimized = Vec::new();
    let mut start = lines[0].start();
    let mut end = lines[0].end();

    for line in lines.iter().skip(1) {
        if line.start() == end + 1 {
            // Consecutive, extend range
            end = line.end();
        } else {
            // Not consecutive, save current range and start new one
            if start == end {
                optimized.push(Line::Single(start));
            } else {
                optimized.push(Line::Range(start, end));
            }
            start = line.start();
            end = line.end();
        }
    }

    // Add the last range
    if start == end {
        optimized.push(Line::Single(start));
    } else {
        optimized.push(Line::Range(start, end));
    }

    optimized
}

/// Check if a file is text-based using git's native approach
fn is_text_file(repo: &Repository, file_path: &str) -> bool {
    // Check for common binary file extensions first
    let path = Path::new(file_path);
    if let Some(extension) = path.extension() {
        let ext = extension.to_string_lossy().to_lowercase();
        let binary_extensions = [
            "jpg",
            "jpeg",
            "png",
            "gif",
            "bmp",
            "tiff",
            "ico",
            "svg",
            "pdf",
            "doc",
            "docx",
            "xls",
            "xlsx",
            "ppt",
            "pptx",
            "zip",
            "tar",
            "gz",
            "bz2",
            "xz",
            "rar",
            "7z",
            "exe",
            "dll",
            "so",
            "dylib",
            "bin",
            "obj",
            "mp3",
            "mp4",
            "avi",
            "mov",
            "wmv",
            "flv",
            "mkv",
            "db",
            "sqlite",
            "sqlite3",
            "class",
            "jar",
            "war",
            "ear",
            "pyc",
            "pyo",
            "__pycache__",
            "o",
            "a",
            "lib",
            "dylib",
            "so",
            "dll",
            "woff",
            "woff2",
            "ttf",
            "otf",
            "eot",
            "ico",
            "cur",
            "ani",
            "psd",
            "ai",
            "eps",
            "indd",
            "mpg",
            "mpeg",
            "wav",
            "flac",
            "aac",
            "iso",
            "img",
            "vmdk",
            "vdi",
            "vhd",
            "bak",
            "tmp",
            "temp",
            "cache",
            "log",
        ];

        if binary_extensions.contains(&ext.as_str()) {
            return false;
        }
    }

    // First check if the file exists in the working directory
    if Path::new(file_path).exists() {
        // Read a sample of the file to check for null bytes and other binary indicators
        if let Ok(bytes) = std::fs::read(file_path) {
            // Check for null bytes in the first 8KB (git's default sample size)
            let sample_size = std::cmp::min(bytes.len(), 8192);
            if sample_size > 0 {
                let sample = &bytes[..sample_size];

                // Check for null bytes
                if sample.contains(&0) {
                    return false; // Contains null bytes, likely binary
                }

                // Check for high percentage of control characters (excluding common ones like \n, \r, \t)
                let control_chars = sample
                    .iter()
                    .filter(|&&b| {
                        b < 32 && b != 9 && b != 10 && b != 13 // Not tab, newline, or carriage return
                    })
                    .count();

                if control_chars > sample_size / 4 {
                    return false; // Too many control characters, likely binary
                }
            }
        }
    } else {
        // File doesn't exist in working directory, check if it exists in HEAD
        if let Ok(head) = repo.head() {
            if let Some(target) = head.target() {
                if let Ok(commit) = repo.find_commit(target) {
                    if let Ok(tree) = commit.tree() {
                        if let Ok(entry) = tree.get_path(Path::new(file_path)) {
                            // Check if the blob in git is binary
                            if let Ok(blob) = repo.find_blob(entry.id()) {
                                let content = blob.content();
                                let sample_size = std::cmp::min(content.len(), 8192);
                                if sample_size > 0 {
                                    let sample = &content[..sample_size];

                                    // Check for null bytes
                                    if sample.contains(&0) {
                                        return false; // Contains null bytes, likely binary
                                    }

                                    // Check for high percentage of control characters
                                    let control_chars = sample
                                        .iter()
                                        .filter(|&&b| b < 32 && b != 9 && b != 10 && b != 13)
                                        .count();

                                    if control_chars > sample_size / 4 {
                                        return false; // Too many control characters, likely binary
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    true
}

/// Clear all ai-working-log/diffs references for a specific base commit
/// This is called when the --reset flag is used to clean up old diff references
fn clear_working_log_diffs(repo: &Repository, base_commit: &str) -> Result<(), GitAiError> {
    // Use git CLI to list and remove references that match the pattern
    let output = std::process::Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/ai-working-log/diffs/",
        ])
        .current_dir(repo.workdir().unwrap_or_else(|| Path::new(".")))
        .output()?;

    if output.status.success() {
        let refs_output = String::from_utf8_lossy(&output.stdout);
        let prefix = format!("refs/ai-working-log/diffs/{}-", base_commit);

        for line in refs_output.lines() {
            let ref_name = line.trim();
            if ref_name.starts_with(&prefix) {
                // Remove the reference using git CLI
                let _ = std::process::Command::new("git")
                    .args(["update-ref", "-d", ref_name])
                    .current_dir(repo.workdir().unwrap_or_else(|| Path::new(".")))
                    .output();
            }
        }
    }

    Ok(())
}
