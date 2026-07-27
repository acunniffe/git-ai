use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log::{LineRange, PromptRecord};
use git_ai::authorship::authorship_log_serialization::{
    AttestationEntry, AuthorshipLog, FileAttestation,
};
use git_ai::authorship::working_log::AgentId;
use git_ai::commands::blame::GitAiBlameOptions;
use git_ai::git::notes_api::write_note;
use git_ai::git::repository as GitAiRepository;

// Helper function to extract author names from blame output
fn extract_authors(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            // Extract author name from blame line format
            // Format: sha (author date line) code
            if let Some(start) = line.find('(') {
                line[start..]
                    .find(' ')
                    .map(|end| line[start + 1..start + end].trim().to_string())
            } else {
                None
            }
        })
        .collect()
}

// Helper function to normalize blame output for comparison
// This replaces author names with consistent placeholders to avoid drift from author names
fn normalize_for_snapshot(output: &str) -> String {
    output
        .lines()
        .map(|line| {
            // Handle porcelain format lines
            if line.starts_with("author-mail") || line.starts_with("committer-mail") {
                // Keep these lines as-is for porcelain format
                line.to_string()
            } else if line.starts_with("author ") || line.starts_with("committer ") {
                // Keep author/committer lines as-is for porcelain format
                line.to_string()
            } else if line.starts_with("author-time")
                || line.starts_with("author-tz")
                || line.starts_with("committer-time")
                || line.starts_with("committer-tz")
            {
                // Keep time/tz lines as-is for porcelain format
                line.to_string()
            } else if line.starts_with("summary")
                || line.starts_with("boundary")
                || line.starts_with("filename")
            {
                // Keep metadata lines as-is for porcelain format
                line.to_string()
            } else if line.starts_with('\t') {
                // Keep content lines (starting with tab) as-is for porcelain format
                line.to_string()
            } else if let Some(start) = line.find('(') {
                if let Some(end) = line[start..].find(')') {
                    // Replace the entire author/date/line section with a consistent placeholder
                    let before = &line[..start + 1];
                    let after = &line[start + end..];
                    format!("{}<AUTHOR_INFO>{}", before, after)
                } else {
                    line.to_string()
                }
            } else {
                line.to_string()
            }
        })
        .map(|line| {
            // Remove the ^ prefix that git adds for boundary commits
            if let Some(stripped) = line.strip_prefix('^') {
                stripped.to_string()
            } else {
                line
            }
        })
        .map(|line| {
            // Only normalize hash length for lines that look like blame output (start with hash)
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let first_part = parts[0];
                // Only apply hash normalization if the first part looks like a hash (hex chars)
                if first_part.chars().all(|c| c.is_ascii_hexdigit()) && first_part.len() >= 7 {
                    let rest = &parts[1..];
                    // Truncate hash to 7 characters for consistent comparison (git blame default)
                    let normalized_hash = if first_part.len() > 7 {
                        &first_part[..7]
                    } else {
                        first_part
                    };
                    format!("{} {}", normalized_hash, rest.join(" "))
                } else {
                    line
                }
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn test_blame_basic_format() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1",
        "Line 2",
        "Line 3".ai(),
        "Line 4".ai()
    ]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    // Run git blame and git-ai blame
    let git_output = repo.git(&["blame", "test.txt"]).unwrap();
    let git_ai_output = repo.git_ai(&["blame", "test.txt"]).unwrap();

    // Compare normalized outputs
    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );
}

#[test]
fn test_blame_line_range() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1",
        "Line 2",
        "Line 3",
        "Line 4",
        "Line 5".ai(),
        "Line 6".ai()
    ]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    // Test -L flag
    let git_output = repo.git(&["blame", "-L", "2,4", "test.txt"]).unwrap();
    let git_ai_output = repo.git_ai(&["blame", "-L", "2,4", "test.txt"]).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );
}

#[test]
fn test_blame_multiple_line_ranges_default() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1",
        "Line 2",
        "Line 3",
        "Line 4",
        "Line 5".ai(),
        "Line 6".ai(),
        "Line 7",
        "Line 8"
    ]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let args = ["blame", "-L", "2,3", "-L", "6,8", "test.txt"];
    let git_output = repo.git(&args).unwrap();
    let git_ai_output = repo.git_ai(&args).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly for multiple -L ranges"
    );
}

#[test]
fn test_blame_multiple_line_ranges_default_reversed_order() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1",
        "Line 2",
        "Line 3",
        "Line 4",
        "Line 5".ai(),
        "Line 6".ai(),
        "Line 7",
        "Line 8"
    ]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let args = ["blame", "-L", "6,8", "-L", "2,3", "test.txt"];
    let git_output = repo.git(&args).unwrap();
    let git_ai_output = repo.git_ai(&args).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly when -L ranges are specified out of order"
    );
}

#[test]
fn test_blame_multiple_line_ranges_overlap_default() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1",
        "Line 2",
        "Line 3",
        "Line 4",
        "Line 5".ai(),
        "Line 6".ai(),
        "Line 7",
        "Line 8"
    ]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let args = ["blame", "-L", "2,5", "-L", "4,7", "test.txt"];
    let git_output = repo.git(&args).unwrap();
    let git_ai_output = repo.git_ai(&args).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly for overlapping -L ranges"
    );
}

#[test]
fn test_blame_multiple_line_ranges_porcelain() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1",
        "Line 2",
        "Line 3",
        "Line 4",
        "Line 5".ai(),
        "Line 6".ai(),
        "Line 7",
        "Line 8"
    ]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let args = ["blame", "--porcelain", "-L", "2,3", "-L", "6,8", "test.txt"];
    let git_output = repo.git(&args).unwrap();
    let git_ai_output = repo.git_ai(&args).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    assert_eq!(
        git_norm, git_ai_norm,
        "Porcelain output should match exactly for multiple -L ranges"
    );
}

#[test]
fn test_blame_multiple_line_ranges_line_porcelain() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1",
        "Line 2",
        "Line 3",
        "Line 4",
        "Line 5".ai(),
        "Line 6".ai(),
        "Line 7",
        "Line 8"
    ]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let args = [
        "blame",
        "--line-porcelain",
        "-L",
        "2,3",
        "-L",
        "6,8",
        "test.txt",
    ];
    let git_output = repo.git(&args).unwrap();
    let git_ai_output = repo.git_ai(&args).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    assert_eq!(
        git_norm, git_ai_norm,
        "Line porcelain output should match exactly for multiple -L ranges"
    );
}

#[test]
fn test_blame_multiple_line_ranges_incremental() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1",
        "Line 2",
        "Line 3",
        "Line 4",
        "Line 5".ai(),
        "Line 6".ai(),
        "Line 7",
        "Line 8"
    ]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let args = [
        "blame",
        "--incremental",
        "-L",
        "2,3",
        "-L",
        "6,8",
        "test.txt",
    ];
    let git_output = repo.git(&args).unwrap();
    let git_ai_output = repo.git_ai(&args).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    assert_eq!(
        git_norm, git_ai_norm,
        "Incremental output should match exactly for multiple -L ranges"
    );
}

#[test]
fn test_blame_porcelain_multiple_hunks_same_commit_matches_git_filename_behavior() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1", "Line 2", "Line 3", "Line 4", "Line 5"
    ]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    file.set_contents(crate::lines![
        "Line 1",
        "Line 2",
        "Changed line 3",
        "Line 4",
        "Line 5"
    ]);
    repo.stage_all_and_commit("Change middle line").unwrap();

    let args = ["blame", "--porcelain", "-L", "1,2", "-L", "4,5", "test.txt"];
    let git_output = repo.git(&args).unwrap();
    let git_ai_output = repo.git_ai(&args).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    assert_eq!(
        git_norm, git_ai_norm,
        "Porcelain output should match exactly when the same commit appears in multiple hunks"
    );

    let git_filename_count = git_output
        .lines()
        .filter(|line| line.starts_with("filename "))
        .count();
    let git_ai_filename_count = git_ai_output
        .lines()
        .filter(|line| line.starts_with("filename "))
        .count();
    assert_eq!(
        git_filename_count, git_ai_filename_count,
        "git-ai should emit filename lines in the same places as git"
    );
    assert_eq!(
        1, git_ai_filename_count,
        "When git has already emitted commit metadata, subsequent hunks from the same commit do not repeat filename"
    );
}

#[test]
fn test_blame_incremental_uses_real_commit_summaries() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    file.set_contents(crate::lines!["Line 1", "Updated line 2"]);
    repo.stage_all_and_commit("Update second line").unwrap();

    let args = ["blame", "--incremental", "-L", "1,2", "test.txt"];
    let git_output = repo.git(&args).unwrap();
    let git_ai_output = repo.git_ai(&args).unwrap();

    let mut git_summaries: Vec<&str> = git_output
        .lines()
        .filter(|line| line.starts_with("summary "))
        .collect();
    let mut git_ai_summaries: Vec<&str> = git_ai_output
        .lines()
        .filter(|line| line.starts_with("summary "))
        .collect();
    git_summaries.sort_unstable();
    git_ai_summaries.sort_unstable();
    assert_eq!(
        git_summaries, git_ai_summaries,
        "git-ai incremental output should report the same commit summaries as git"
    );

    assert!(
        git_output.contains("summary Update second line"),
        "Sanity check: git incremental output should contain the newer commit summary"
    );
    assert!(
        git_ai_output.contains("summary Update second line"),
        "git-ai incremental output should include real commit summaries"
    );
}

#[test]
fn test_blame_porcelain_format() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let git_output = repo.git(&["blame", "--porcelain", "test.txt"]).unwrap();
    let git_ai_output = repo.git_ai(&["blame", "--porcelain", "test.txt"]).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );
}

#[test]
fn test_blame_show_email() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let git_output = repo.git(&["blame", "-e", "test.txt"]).unwrap();
    let git_ai_output = repo.git_ai(&["blame", "-e", "test.txt"]).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );

    // Verify both contain email addresses
    assert!(git_output.contains("@"), "Git output should contain email");
    assert!(
        git_ai_output.contains("@"),
        "Git-ai output should contain email"
    );
}

#[test]
fn test_blame_show_name() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let git_output = repo.git(&["blame", "-f", "test.txt"]).unwrap();
    let git_ai_output = repo.git_ai(&["blame", "-f", "test.txt"]).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );

    // Verify both contain filename information
    assert!(
        git_output.contains("test.txt"),
        "Git output should contain filename"
    );
    assert!(
        git_ai_output.contains("test.txt"),
        "Git-ai output should contain filename"
    );
}

#[test]
fn test_blame_show_number() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let git_output = repo.git(&["blame", "-n", "test.txt"]).unwrap();
    let git_ai_output = repo.git_ai(&["blame", "-n", "test.txt"]).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );
}

#[test]
fn test_blame_suppress_author() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let git_output = repo.git(&["blame", "-s", "test.txt"]).unwrap();
    let git_ai_output = repo.git_ai(&["blame", "-s", "test.txt"]).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );

    // Verify both suppress author information (should not contain "Test User")
    assert!(
        !git_output.contains("Test User"),
        "Git output should suppress author"
    );
    assert!(
        !git_ai_output.contains("Test User"),
        "Git-ai output should suppress author"
    );
}

#[test]
fn test_blame_long_rev() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let git_output = repo.git(&["blame", "-l", "test.txt"]).unwrap();
    let git_ai_output = repo.git_ai(&["blame", "-l", "test.txt"]).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );

    // Verify both show long revision hashes
    let git_sha_len = git_output
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .len();
    let git_ai_sha_len = git_ai_output
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .len();

    assert!(git_sha_len > 8, "Git should show long revision");
    assert!(git_ai_sha_len > 8, "Git-ai should show long revision");
}

#[test]
fn test_blame_raw_timestamp() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let git_output = repo.git(&["blame", "-t", "test.txt"]).unwrap();
    let git_ai_output = repo.git_ai(&["blame", "-t", "test.txt"]).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );

    // Verify both contain raw timestamps (Unix timestamps)
    assert!(
        git_output.chars().any(|c| c.is_numeric()),
        "Git output should contain timestamps"
    );
    assert!(
        git_ai_output.chars().any(|c| c.is_numeric()),
        "Git-ai output should contain timestamps"
    );
}

#[test]
fn test_blame_abbrev() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    // Note: git requires --abbrev=4 format, git-ai accepts --abbrev 4
    let git_output = repo.git(&["blame", "--abbrev=4", "test.txt"]).unwrap();
    let git_ai_output = repo
        .git_ai(&["blame", "--abbrev", "4", "test.txt"])
        .unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );
}

#[test]
fn test_blame_blank_boundary() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let git_output = repo.git(&["blame", "-b", "test.txt"]).unwrap();
    let git_ai_output = repo.git_ai(&["blame", "-b", "test.txt"]).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );
}

#[test]
fn test_blame_show_root() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let git_output = repo.git(&["blame", "--root", "test.txt"]).unwrap();
    let git_ai_output = repo.git_ai(&["blame", "--root", "test.txt"]).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );

    // Verify both handle root commits
    assert!(
        git_output.lines().count() > 0,
        "Git should handle root commits"
    );
    assert!(
        git_ai_output.lines().count() > 0,
        "Git-ai should handle root commits"
    );
}

// #[test]
// fn test_blame_show_stats() {
//     let tmp_dir = tempdir().unwrap();
//     let repo_path = tmp_dir.path().to_path_buf();

//     let tmp_repo = TmpRepo::new().unwrap();
//     let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

//     tmp_repo
//         .trigger_checkpoint_with_author("test_user")
//         .unwrap();
//     file.append("Line 2\n").unwrap();
//     tmp_repo.trigger_checkpoint_with_ai("Claude", Some("claude-3-sonnet"), Some("cursor")).unwrap();
//     tmp_repo.commit_with_message("Initial commit").unwrap();

//     let git_output = run_git_blame(tmp_repo.path(), "test.txt", &["--show-stats"]);
//     let git_ai_output = run_git_ai_blame(tmp_repo.path(), "test.txt", &["--show-stats"]);

//     let _comparison = create_blame_comparison(&git_output, &git_ai_output, "show_stats");
//     let git_norm = normalize_for_snapshot(&git_output);
//     let git_ai_norm = normalize_for_snapshot(&git_ai_output);
//     println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
//     println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
//     assert_eq!(
//         git_norm, git_ai_norm,
//         "Normalized blame outputs should match exactly"
//     );

//     // Verify both show statistics
//     assert!(
//         git_output.contains("%"),
//         "Git output should contain statistics"
//     );
//     assert!(
//         git_ai_output.contains("%"),
//         "Git-ai output should contain statistics"
//     );
// }
#[test]
fn test_blame_date_format() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    // Note: git requires --date=short format, git-ai accepts --date short
    let git_output = repo.git(&["blame", "--date=short", "test.txt"]).unwrap();
    let git_ai_output = repo
        .git_ai(&["blame", "--date", "short", "test.txt"])
        .unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );

    // Verify both use short date format
    assert!(git_output.contains("-"), "Git output should contain date");
    assert!(
        git_ai_output.contains("-"),
        "Git-ai output should contain date"
    );
}

#[test]
fn test_blame_multiple_flags() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1",
        "Line 2",
        "Line 3",
        "Line 4".ai(),
        "Line 5".ai()
    ]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    // Test multiple flags together
    let git_output = repo
        .git(&["blame", "-L", "2,4", "-e", "-n", "test.txt"])
        .unwrap();
    let git_ai_output = repo
        .git_ai(&["blame", "-L", "2,4", "-e", "-n", "test.txt"])
        .unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );

    // Verify both handle multiple flags
    assert!(
        git_output.lines().count() > 0,
        "Git should handle multiple flags"
    );
    assert!(
        git_ai_output.lines().count() > 0,
        "Git-ai should handle multiple flags"
    );

    // Verify both contain email and line numbers
    assert!(git_output.contains("@"), "Git output should contain email");
    assert!(
        git_ai_output.contains("@"),
        "Git-ai output should contain email"
    );
}

#[test]
fn test_blame_incremental_format() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let git_output = repo.git(&["blame", "--incremental", "test.txt"]).unwrap();
    let git_ai_output = repo
        .git_ai(&["blame", "--incremental", "test.txt"])
        .unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );
}

#[test]
fn test_blame_line_porcelain() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let git_output = repo
        .git(&["blame", "--line-porcelain", "test.txt"])
        .unwrap();
    let git_ai_output = repo
        .git_ai(&["blame", "--line-porcelain", "test.txt"])
        .unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );
}

#[test]
fn test_blame_with_ai_authorship() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2", "Line 3".ai(), "Line 4"]);

    repo.stage_all_and_commit("Mixed authorship commit")
        .unwrap();

    let git_output = repo.git(&["blame", "test.txt"]).unwrap();
    let git_ai_output = repo.git_ai(&["blame", "test.txt"]).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );

    // Extract authors from both outputs
    let git_authors = extract_authors(&git_output);
    let git_ai_authors = extract_authors(&git_ai_output);

    // Git should show the same author for all lines (the committer)
    // Git-ai should show different authors based on AI authorship
    assert_ne!(
        git_authors, git_ai_authors,
        "AI authorship should change the output"
    );

    // Verify git-ai shows AI authors where appropriate
    assert!(
        git_ai_authors
            .iter()
            .any(|a| a.contains("mock_ai") || a.contains("mock_ai")),
        "Should show AI as author. Got: {:?}",
        git_ai_authors
    );
}

#[test]
fn test_blame_contents_from_stdin() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create initial file and commit
    file.set_contents(crate::lines!["Line 1", "Line 2".ai(), "Line 3", " "]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Now simulate uncommitted changes that would be passed via stdin
    // This is what an IDE would do - pass the buffer contents that haven't been saved yet
    let modified_content = "Changed\nLine 2\nLine 3\nLine 4 NEW\n";

    // Run git-ai blame with --contents - (read from stdin)
    let git_ai_output = repo
        .git_ai_with_stdin(
            &["blame", "--contents", "-", "test.txt"],
            modified_content.as_bytes(),
        )
        .unwrap();

    println!("\n[DEBUG] git-ai blame output:\n{}", git_ai_output);
    let lines = git_ai_output.lines().collect::<Vec<&str>>();

    assert!(
        lines[0].starts_with("00000000 (External file (--contents)"),
        "First line should be the  --contents"
    );

    assert!(
        lines[3].starts_with("00000000 (External file (--contents)"),
        "Last line should be the --contents"
    );
}

#[test]
fn test_blame_mark_unknown_without_authorship_log() {
    // Test that --mark-unknown shows "Unknown" for commits without authorship logs
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create a commit WITHOUT using git-ai (bypassing hooks to avoid authorship log)
    file.set_contents(crate::lines!["Line from untracked commit"]);

    // Use git_og to bypass git-ai hooks
    repo.git_og(&["add", "test.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "Commit without authorship log"])
        .unwrap();

    // Without --mark-unknown: should show author name
    let output_without_flag = repo.git_ai(&["blame", "test.txt"]).unwrap();
    println!("\n[DEBUG] Without --mark-unknown:\n{}", output_without_flag);
    assert!(
        output_without_flag.contains("Test User"),
        "Without flag, lines from untracked commits should show author name"
    );

    // With --mark-unknown: should show "Unknown"
    let output_with_flag = repo
        .git_ai(&["blame", "--mark-unknown", "test.txt"])
        .unwrap();
    println!("\n[DEBUG] With --mark-unknown:\n{}", output_with_flag);
    assert!(
        output_with_flag.contains("Unknown"),
        "With flag, lines from untracked commits should show 'Unknown'"
    );
    assert!(
        !output_with_flag.contains("Test User"),
        "With flag, should not show author name for untracked commits"
    );
}

#[test]
fn test_blame_mark_unknown_mixed_commits() {
    // Test a file with lines from both tracked and untracked commits
    // We'll create two separate files - one from untracked commit, one from tracked
    let repo = TestRepo::new();

    // Create file1 WITHOUT authorship log (using git_og)
    let file1_path = repo.path().join("untracked.txt");
    std::fs::write(&file1_path, "Untracked line\n").unwrap();
    repo.git_og(&["add", "untracked.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "Untracked commit"]).unwrap();

    // Create file2 WITH authorship log (through git-ai)
    let mut file2 = repo.filename("tracked.txt");
    file2.set_contents(crate::lines!["Tracked human line", "Tracked AI line".ai()]);
    repo.stage_all_and_commit("Tracked commit").unwrap();

    // Test untracked file with --mark-unknown
    let output1 = repo
        .git_ai(&["blame", "--mark-unknown", "untracked.txt"])
        .unwrap();
    println!("\n[DEBUG] Untracked file with --mark-unknown:\n{}", output1);
    assert!(
        output1.contains("Unknown"),
        "Untracked file should show Unknown: {}",
        output1
    );

    // Test tracked file with --mark-unknown
    let output2 = repo
        .git_ai(&["blame", "--mark-unknown", "tracked.txt"])
        .unwrap();
    println!("\n[DEBUG] Tracked file with --mark-unknown:\n{}", output2);

    let lines: Vec<&str> = output2.lines().collect();

    // Line 1 should show "Test User" (human line from tracked commit)
    assert!(
        lines[0].contains("Test User"),
        "Line 1 should show Test User: {}",
        lines[0]
    );

    // Line 2 should show AI tool name (AI line from tracked commit)
    assert!(
        lines[1].contains("mock_ai"),
        "Line 2 should show mock_ai: {}",
        lines[1]
    );
}

#[test]
fn test_blame_mark_unknown_backward_compatible() {
    // Ensure that without --mark-unknown, behavior matches git blame exactly
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create commit without authorship log (using git_og)
    file.set_contents(crate::lines!["Line 1", "Line 2"]);
    repo.git_og(&["add", "test.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "Untracked commit"]).unwrap();

    let git_output = repo.git(&["blame", "test.txt"]).unwrap();
    let git_ai_output = repo.git_ai(&["blame", "test.txt"]).unwrap();

    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);

    println!("\n[DEBUG] git blame:\n{}", git_norm);
    println!("\n[DEBUG] git-ai blame:\n{}", git_ai_norm);

    assert_eq!(
        git_norm, git_ai_norm,
        "Without --mark-unknown, git-ai blame should match git blame exactly"
    );
}

// =============================================================================
// Tests for .git-blame-ignore-revs auto-detection (Issue #363)
// =============================================================================

#[test]
fn test_blame_auto_detects_git_blame_ignore_revs_file() {
    // Test that git-ai blame automatically uses .git-blame-ignore-revs when present
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create initial commit with some content
    file.set_contents(crate::lines!["Line 1", "Line 2", "Line 3"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Get the initial commit SHA
    let initial_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Create a "formatting" commit that we want to ignore
    file.set_contents(crate::lines!["  Line 1", "  Line 2", "  Line 3"]); // Add indentation
    repo.stage_all_and_commit("Format: add indentation")
        .unwrap();

    // Get the formatting commit SHA
    let format_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Create .git-blame-ignore-revs file with the formatting commit
    let ignore_revs_path = repo.path().join(".git-blame-ignore-revs");
    std::fs::write(
        &ignore_revs_path,
        format!("# Formatting commit\n{}\n", format_sha),
    )
    .unwrap();

    // Run git-ai blame - it should auto-detect .git-blame-ignore-revs
    let git_ai_output = repo.git_ai(&["blame", "test.txt"]).unwrap();
    println!(
        "\n[DEBUG] git-ai blame with auto-detected ignore-revs:\n{}",
        git_ai_output
    );

    // The blame should show the initial commit, not the formatting commit
    // (because the formatting commit is in .git-blame-ignore-revs)
    assert!(
        git_ai_output.contains(&initial_sha[..7]),
        "Blame should show initial commit SHA when formatting commit is ignored. Output: {}",
        git_ai_output
    );
}

#[test]
fn test_blame_no_ignore_revs_file_flag_disables_auto_detection() {
    // Test that --no-ignore-revs-file disables auto-detection
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create initial commit
    file.set_contents(crate::lines!["Line 1", "Line 2"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Create a second commit
    file.set_contents(crate::lines!["Modified Line 1", "Line 2"]);
    repo.stage_all_and_commit("Modify line 1").unwrap();

    // Get the second commit SHA
    let second_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Create .git-blame-ignore-revs to ignore the second commit
    let ignore_revs_path = repo.path().join(".git-blame-ignore-revs");
    std::fs::write(&ignore_revs_path, format!("{}\n", second_sha)).unwrap();

    // Run with --no-ignore-revs-file - should NOT use .git-blame-ignore-revs
    let output_without_ignore = repo
        .git_ai(&["blame", "--no-ignore-revs-file", "test.txt"])
        .unwrap();
    println!(
        "\n[DEBUG] git-ai blame with --no-ignore-revs-file:\n{}",
        output_without_ignore
    );

    // The second commit should appear in the output (not ignored)
    assert!(
        output_without_ignore.contains(&second_sha[..7]),
        "With --no-ignore-revs-file, the second commit should appear in blame. Output: {}",
        output_without_ignore
    );
}

#[test]
fn test_blame_explicit_ignore_revs_file_takes_precedence() {
    // Test that explicit --ignore-revs-file takes precedence over auto-detection
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create initial commit
    file.set_contents(crate::lines!["Line 1"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let _initial_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Create second commit
    file.set_contents(crate::lines!["Line 1 modified"]);
    repo.stage_all_and_commit("Second commit").unwrap();

    let second_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Create third commit
    file.set_contents(crate::lines!["Line 1 modified again"]);
    repo.stage_all_and_commit("Third commit").unwrap();

    let third_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Create .git-blame-ignore-revs that ignores the SECOND commit
    let default_ignore_path = repo.path().join(".git-blame-ignore-revs");
    std::fs::write(&default_ignore_path, format!("{}\n", second_sha)).unwrap();

    // Create a custom ignore file that ignores the THIRD commit
    let custom_ignore_path = repo.path().join("custom-ignore-revs");
    std::fs::write(&custom_ignore_path, format!("{}\n", third_sha)).unwrap();

    // Run with explicit --ignore-revs-file pointing to custom file
    let output = repo
        .git_ai(&[
            "blame",
            "--ignore-revs-file",
            "custom-ignore-revs",
            "test.txt",
        ])
        .unwrap();
    println!(
        "\n[DEBUG] git-ai blame with explicit --ignore-revs-file:\n{}",
        output
    );

    // The second commit (ignored by default file) should appear
    // because we're using the custom file which ignores the third commit
    assert!(
        output.contains(&second_sha[..7]),
        "Explicit --ignore-revs-file should take precedence. Second commit should appear. Output: {}",
        output
    );
}

#[test]
fn test_blame_respects_git_config_blame_ignore_revs_file() {
    // Test that git-ai respects the blame.ignoreRevsFile git config
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create initial commit
    file.set_contents(crate::lines!["Line 1", "Line 2"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let initial_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Create a commit we want to ignore
    file.set_contents(crate::lines!["Line 1 reformatted", "Line 2 reformatted"]);
    repo.stage_all_and_commit("Reformat code").unwrap();

    let reformat_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Create ignore file with a custom name (not .git-blame-ignore-revs)
    let custom_ignore_path = repo.path().join("my-ignore-revs");
    std::fs::write(&custom_ignore_path, format!("{}\n", reformat_sha)).unwrap();

    // Set git config to point to the custom file
    repo.git_og(&["config", "blame.ignoreRevsFile", "my-ignore-revs"])
        .unwrap();

    // Run git-ai blame - should auto-detect from git config
    let output = repo.git_ai(&["blame", "test.txt"]).unwrap();
    println!(
        "\n[DEBUG] git-ai blame with blame.ignoreRevsFile config:\n{}",
        output
    );

    // The initial commit should appear (reformat commit should be ignored via config)
    assert!(
        output.contains(&initial_sha[..7]),
        "Should respect blame.ignoreRevsFile config. Initial commit should appear. Output: {}",
        output
    );
}

#[test]
fn test_blame_without_ignore_revs_file_works_normally() {
    // Test that blame works normally when no .git-blame-ignore-revs exists
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create initial commit
    file.set_contents(crate::lines!["Line 1"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Create second commit
    file.set_contents(crate::lines!["Line 1 modified"]);
    repo.stage_all_and_commit("Second commit").unwrap();

    let second_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // No .git-blame-ignore-revs file exists

    // Run git-ai blame - should work normally
    let git_ai_output = repo.git_ai(&["blame", "test.txt"]).unwrap();
    let git_output = repo.git(&["blame", "test.txt"]).unwrap();

    println!("\n[DEBUG] git blame:\n{}", git_output);
    println!("\n[DEBUG] git-ai blame:\n{}", git_ai_output);

    // Both should show the second commit
    assert!(
        git_ai_output.contains(&second_sha[..7]),
        "git-ai blame should show second commit when no ignore file exists. Output: {}",
        git_ai_output
    );

    // Outputs should match (normalized)
    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    assert_eq!(
        git_norm, git_ai_norm,
        "Blame outputs should match when no ignore file exists"
    );
}

#[test]
fn test_blame_ignore_revs_with_multiple_commits() {
    // Test ignoring multiple commits in .git-blame-ignore-revs
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create initial commit
    file.set_contents(crate::lines!["original"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let initial_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Create first formatting commit
    file.set_contents(crate::lines!["  original"]);
    repo.stage_all_and_commit("Format 1: add spaces").unwrap();

    let format1_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Create second formatting commit
    file.set_contents(crate::lines!["    original"]);
    repo.stage_all_and_commit("Format 2: add more spaces")
        .unwrap();

    let format2_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Create .git-blame-ignore-revs with both formatting commits
    let ignore_revs_path = repo.path().join(".git-blame-ignore-revs");
    std::fs::write(
        &ignore_revs_path,
        format!(
            "# Formatting commits to ignore\n{}\n{}\n",
            format1_sha, format2_sha
        ),
    )
    .unwrap();

    // Run git-ai blame
    let output = repo.git_ai(&["blame", "test.txt"]).unwrap();
    println!(
        "\n[DEBUG] git-ai blame with multiple ignored commits:\n{}",
        output
    );

    // Should show the initial commit (both formatting commits are ignored)
    assert!(
        output.contains(&initial_sha[..7]),
        "Both formatting commits should be ignored. Initial commit should appear. Output: {}",
        output
    );
}

#[test]
fn test_blame_ai_human_author() {
    let repo = TestRepo::new();

    let mut file = repo.filename("test.txt");

    // Create initial commit
    file.set_contents(crate::lines!["first line", "second line", "third line"]);

    let initial_sha = repo
        .stage_all_and_commit("Initial commit")
        .unwrap()
        .commit_sha;

    // Create authorship log with two prompts - one for line 1, one for line 2
    let mut authorship_log = AuthorshipLog::new();
    authorship_log.metadata.base_commit_sha = initial_sha.clone();

    // First prompt for line 1
    let prompt_hash_1 = "abc12345".to_string();
    let agent_id_1 = AgentId {
        tool: "cursor".to_string(),
        id: "session_line1".to_string(),
        model: "claude-3-sonnet".to_string(),
    };
    authorship_log.metadata.prompts.insert(
        prompt_hash_1.clone(),
        PromptRecord {
            agent_id: agent_id_1,
            human_author: Some("First <first@example.com>".to_string()),
            total_additions: 1,
            total_deletions: 0,
            accepted_lines: 1,
            overriden_lines: 0,
            custom_attributes: None,
            messages_url: None,
        },
    );

    // Second prompt for line 2
    let prompt_hash_2 = "xyz67890".to_string();
    let agent_id_2 = AgentId {
        tool: "cursor".to_string(),
        id: "session_line2".to_string(),
        model: "claude-3-sonnet".to_string(),
    };
    authorship_log.metadata.prompts.insert(
        prompt_hash_2.clone(),
        PromptRecord {
            agent_id: agent_id_2,
            human_author: Some("Second <second@example.com>".to_string()),
            total_additions: 1,
            total_deletions: 0,
            accepted_lines: 1,
            overriden_lines: 0,
            custom_attributes: None,
            messages_url: None,
        },
    );

    // Add attestations - line 1 attributed to first prompt, line 2 to second
    let mut file_attestation = FileAttestation::new("test.txt".to_string());
    file_attestation.add_entry(AttestationEntry::new(
        prompt_hash_1,
        vec![LineRange::Single(1)],
    ));
    file_attestation.add_entry(AttestationEntry::new(
        prompt_hash_2,
        vec![LineRange::Single(2)],
    ));
    authorship_log.attestations.push(file_attestation);

    // Serialize and add the note
    let note_content = authorship_log.serialize_to_string().unwrap();
    let gitai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("Failed to find repository");
    write_note(&gitai_repo, &initial_sha, &note_content).unwrap();

    // Call blame_hunks on the file
    let options = GitAiBlameOptions::default();
    let hunks = gitai_repo
        .blame_hunks("test.txt", 1, 2, &options)
        .expect("Failed to get blame hunks");

    let ai_human_authors = hunks
        .iter()
        .map(|hunk| hunk.ai_human_author.clone())
        .collect::<Vec<_>>();

    assert_eq!(
        ai_human_authors,
        vec![
            Some("First <first@example.com>".to_string()),
            Some("Second <second@example.com>".to_string())
        ]
    );
}

crate::reuse_tests_in_worktree!(
    test_blame_basic_format,
    test_blame_line_range,
    test_blame_porcelain_format,
    test_blame_show_email,
    test_blame_show_name,
    test_blame_show_number,
    test_blame_suppress_author,
    test_blame_long_rev,
    test_blame_raw_timestamp,
    test_blame_abbrev,
    test_blame_blank_boundary,
    test_blame_show_root,
    test_blame_date_format,
    test_blame_multiple_flags,
    test_blame_incremental_format,
    test_blame_line_porcelain,
    test_blame_with_ai_authorship,
    test_blame_contents_from_stdin,
    test_blame_mark_unknown_without_authorship_log,
    test_blame_mark_unknown_mixed_commits,
    test_blame_mark_unknown_backward_compatible,
    test_blame_auto_detects_git_blame_ignore_revs_file,
    test_blame_no_ignore_revs_file_flag_disables_auto_detection,
    test_blame_explicit_ignore_revs_file_takes_precedence,
    test_blame_respects_git_config_blame_ignore_revs_file,
    test_blame_without_ignore_revs_file_works_normally,
    test_blame_ignore_revs_with_multiple_commits,
    test_blame_ai_human_author,
);
