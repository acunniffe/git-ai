use crate::commands::blame::{GitAiBlameOptions, get_git_blame_hunks, overlay_ai_authorship};
use crate::error::GitAiError;
use git2::{DiffOptions, Repository};
use std::collections::HashMap;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FileStats {
    pub additions: HashMap<String, u32>,
    pub deletions: u32,
    pub total_additions: u32,
}

#[allow(dead_code)]
pub fn run(repo: &Repository, sha: &str) -> Result<(), GitAiError> {
    // Find the commit
    let commit = repo.find_commit(repo.revparse_single(sha)?.id())?;

    // Get the parent commit (for diff)
    let parent = if let Ok(parent) = commit.parent(0) {
        parent
    } else {
        return Err(GitAiError::Generic("Commit has no parent".to_string()));
    };

    // Get the diff between parent and commit
    let tree = commit.tree()?;
    let parent_tree = parent.tree()?;

    let mut diff_opts = DiffOptions::new();
    diff_opts.context_lines(0); // No context lines

    let diff = repo.diff_tree_to_tree(Some(&parent_tree), Some(&tree), Some(&mut diff_opts))?;

    // Process the diff and collect statistics
    let mut file_stats = HashMap::new();
    let mut total_additions_by_author = HashMap::new();
    let mut total_deletions = 0;

    // For each file, collect added and deleted lines
    let mut file_added_lines: HashMap<String, Vec<u32>> = HashMap::new();
    let mut file_deleted_counts: HashMap<String, u32> = HashMap::new();

    diff.foreach(
        &mut |_delta, _| true,
        None,
        None,
        Some(&mut |delta, _hunk, line| {
            let file_path = delta
                .new_file()
                .path()
                .unwrap()
                .to_string_lossy()
                .to_string();
            match line.origin() {
                '+' => {
                    let line_num = line.new_lineno().unwrap_or(0) as u32;
                    file_added_lines
                        .entry(file_path)
                        .or_default()
                        .push(line_num);
                }
                '-' => {
                    *file_deleted_counts.entry(file_path).or_default() += 1;
                    total_deletions += 1;
                }
                _ => {}
            }
            true
        }),
    )?;

    // For each file, use blame overlay logic to attribute added lines
    for (file_path, added_lines) in file_added_lines.iter() {
        // Get blame hunks for the file (for all lines, since we may not know the exact range)
        let total_lines = {
            // Try to get the file from the new tree
            if let Ok(entry) = tree.get_path(std::path::Path::new(file_path)) {
                if let Ok(blob) = repo.find_blob(entry.id()) {
                    let content = std::str::from_utf8(blob.content()).unwrap_or("");
                    content.lines().count() as u32
                } else {
                    0
                }
            } else {
                0
            }
        };
        if total_lines == 0 {
            continue;
        }
        let blame_hunks = match get_git_blame_hunks(
            repo,
            file_path,
            1,
            total_lines,
            &GitAiBlameOptions::default(),
        ) {
            Ok(hunks) => hunks,
            Err(_) => continue,
        };
        let line_authors = match overlay_ai_authorship(repo, &blame_hunks, file_path) {
            Ok(authors) => authors,
            Err(_) => continue,
        };
        let stats = file_stats.entry(file_path.clone()).or_insert(FileStats {
            additions: HashMap::new(),
            deletions: *file_deleted_counts.get(file_path).unwrap_or(&0),
            total_additions: 0,
        });
        for &line_num in added_lines {
            let author = line_authors
                .get(&line_num)
                .cloned()
                .unwrap_or("unknown".to_string());
            *stats.additions.entry(author.clone()).or_insert(0) += 1;
            *total_additions_by_author.entry(author).or_insert(0) += 1;
            stats.total_additions += 1;
        }
    }
    // For files with only deletions
    for (file_path, &del_count) in file_deleted_counts.iter() {
        file_stats.entry(file_path.clone()).or_insert(FileStats {
            additions: HashMap::new(),
            deletions: del_count,
            total_additions: 0,
        });
    }

    // Print the statistics
    print_stats(&file_stats, &total_additions_by_author, total_deletions);

    Ok(())
}

#[allow(dead_code)]
fn print_stats(
    file_stats: &HashMap<String, FileStats>,
    total_additions_by_author: &HashMap<String, u32>,
    total_deletions: u32,
) {
    println!("{}", "=".repeat(50));

    // Print per-file statistics
    for (file_path, stats) in file_stats.iter() {
        print_file_stats(file_path, stats);
    }

    // Print totals
    println!("\nTotal Additions:");
    for (author, count) in total_additions_by_author.iter() {
        println!("    {} +{}", author, count);
    }

    println!("\nTotal Deletions: -{}", total_deletions);
}

#[allow(dead_code)]
fn print_file_stats(file_path: &str, stats: &FileStats) {
    // Calculate total changes for the file
    let total_additions = stats.total_additions;
    let total_deletions = stats.deletions;

    // Print file header with total changes
    println!(
        "\n{} (+{} -{})",
        file_path, total_additions, total_deletions
    );

    // Print additions by author
    for (author, count) in stats.additions.iter() {
        println!("   {} (+{})", author, count);
    }

    // Print deletions (no author attribution)
    if stats.deletions > 0 {
        println!("   -{}", stats.deletions);
    }
}
