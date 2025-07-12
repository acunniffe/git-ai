use git_ai::tmp_repo::TmpRepo;
use insta::assert_debug_snapshot;
use std::collections::BTreeMap;
use std::process::Command;
use tempfile::tempdir;

// Helper function to run git blame and capture output
fn run_git_blame(repo_path: &std::path::Path, file_path: &str, args: &[&str]) -> String {
    println!("[DEBUG] Running git blame in directory: {:?}", repo_path);
    println!("[DEBUG] File path: {}", file_path);
    println!("[DEBUG] Args: {:?}", args);

    // Process arguments to handle key=value pairs correctly
    let mut processed_args = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--abbrev" => {
                if i + 1 < args.len() {
                    processed_args.push(format!("--abbrev={}", args[i + 1]));
                    i += 2;
                } else {
                    processed_args.push(args[i].to_string());
                    i += 1;
                }
            }
            "--date" => {
                if i + 1 < args.len() {
                    processed_args.push(format!("--date={}", args[i + 1]));
                    i += 2;
                } else {
                    processed_args.push(args[i].to_string());
                    i += 1;
                }
            }
            _ => {
                processed_args.push(args[i].to_string());
                i += 1;
            }
        }
    }

    println!("[DEBUG] Processed args: {:?}", processed_args);

    let output = Command::new("git")
        .current_dir(repo_path)
        .arg("blame")
        .args(&processed_args)
        .arg(file_path)
        .env("GIT_PAGER", "cat") // Force use of cat instead of less
        .env("PAGER", "cat")
        .output()
        .expect("Failed to run git blame");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    println!("[DEBUG] Git blame stdout: {:?}", stdout);
    println!("[DEBUG] Git blame stderr: {:?}", stderr);
    println!("[DEBUG] Git blame status: {:?}", output.status);

    String::from_utf8_lossy(&output.stdout).to_string()
}

// Helper function to run git-ai blame and capture output
fn run_git_ai_blame(repo_path: &std::path::Path, file_path: &str, args: &[&str]) -> String {
    let binary_path = std::env::current_dir().unwrap().join("target/debug/git-ai");
    let output = Command::new(binary_path)
        .current_dir(repo_path)
        .arg("blame")
        .args(args)
        .arg(file_path)
        .env("GIT_PAGER", "cat") // Force use of cat instead of less
        .env("PAGER", "cat")
        .output()
        .expect("Failed to run git-ai blame");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !stderr.is_empty() {
        println!("Git-ai stderr: {}", stderr);
    }

    stdout
}

// Helper function to extract author names from blame output
fn extract_authors(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            // Extract author name from blame line format
            // Format: sha (author date line) code
            if let Some(start) = line.find('(') {
                if let Some(end) = line[start..].find(' ') {
                    Some(line[start + 1..start + end].trim().to_string())
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect()
}
// Helper function to normalize blame output for snapshot comparison
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
            if line.starts_with('^') {
                line[1..].to_string()
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

// Helper function to create a snapshot-comparable structure
fn create_blame_comparison(
    git_output: &str,
    git_ai_output: &str,
    test_name: &str,
) -> BTreeMap<String, String> {
    let mut comparison = BTreeMap::new();

    // Normalize both outputs for comparison
    let git_norm = normalize_for_snapshot(git_output);
    let git_ai_norm = normalize_for_snapshot(git_ai_output);

    comparison.insert(format!("{}_git_normalized", test_name), git_norm);
    comparison.insert(format!("{}_git_ai_normalized", test_name), git_ai_norm);

    // Also store raw outputs for debugging
    comparison.insert(format!("{}_git_raw", test_name), git_output.to_string());
    comparison.insert(
        format!("{}_git_ai_raw", test_name),
        git_ai_output.to_string(),
    );

    comparison
}

#[test]
fn test_blame_basic_format() {
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo
        .write_file("test.txt", "Line 1\nLine 2\n", true)
        .unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 3\nLine 4\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &[]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &[]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "basic_format");

    // Use insta to capture the comparison and catch drift
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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo
        .write_file("test.txt", "Line 1\nLine 2\nLine 3\nLine 4\n", true)
        .unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 5\nLine 6\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    // Test -L flag
    let git_output = run_git_blame(&repo_path, "test.txt", &["-L", "2,4"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["-L", "2,4"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "line_range");
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
fn test_blame_porcelain_format() {
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &["--porcelain"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["--porcelain"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "porcelain_format");
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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &["-e"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["-e"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "show_email");
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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &["-f"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["-f"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "show_name");
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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &["-n"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["-n"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "show_number");
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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &["-s"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["-s"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "suppress_author");
    let git_norm = normalize_for_snapshot(&git_output);
    let git_ai_norm = normalize_for_snapshot(&git_ai_output);
    println!("\n[DEBUG] Normalized git blame output:\n{}", git_norm);
    println!("\n[DEBUG] Normalized git-ai blame output:\n{}", git_ai_norm);
    assert_eq!(
        git_norm, git_ai_norm,
        "Normalized blame outputs should match exactly"
    );

    // Verify both suppress author information
    assert!(
        !git_output.contains("test_user"),
        "Git output should suppress author"
    );
    assert!(
        !git_ai_output.contains("test_user"),
        "Git-ai output should suppress author"
    );
}

#[test]
fn test_blame_long_rev() {
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &["-l"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["-l"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "long_rev");
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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &["-t"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["-t"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "raw_timestamp");
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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &["--abbrev", "4"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["--abbrev", "4"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "abbrev");
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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &["-b"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["-b"]);

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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &["--root"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["--root"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "show_root");
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

//     let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
//     let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

//     tmp_repo
//         .trigger_checkpoint_with_author("test_user")
//         .unwrap();
//     file.append("Line 2\n").unwrap();
//     tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
//     tmp_repo.commit_with_message("Initial commit").unwrap();

//     let git_output = run_git_blame(&repo_path, "test.txt", &["--show-stats"]);
//     let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["--show-stats"]);

//     let comparison = create_blame_comparison(&git_output, &git_ai_output, "show_stats");
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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &["--date", "short"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["--date", "short"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "date_format");
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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo
        .write_file("test.txt", "Line 1\nLine 2\nLine 3\n", true)
        .unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 4\nLine 5\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    // Test multiple flags together
    let git_output = run_git_blame(&repo_path, "test.txt", &["-L", "2,4", "-e", "-n"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["-L", "2,4", "-e", "-n"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "multiple_flags");
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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &["--incremental"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["--incremental"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "incremental_format");
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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("Initial commit").unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &["--line-porcelain"]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &["--line-porcelain"]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "line_porcelain");
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
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    let tmp_repo = TmpRepo::new(repo_path.clone()).unwrap();
    let mut file = tmp_repo.write_file("test.txt", "Line 1\n", true).unwrap();

    // First commit by human
    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 2\n").unwrap();

    // Second commit by AI
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    file.append("Line 3\n").unwrap();

    // Third commit by human
    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    file.append("Line 4\n").unwrap();

    tmp_repo
        .commit_with_message("Mixed authorship commit")
        .unwrap();

    let git_output = run_git_blame(&repo_path, "test.txt", &[]);
    let git_ai_output = run_git_ai_blame(&repo_path, "test.txt", &[]);

    let comparison = create_blame_comparison(&git_output, &git_ai_output, "ai_authorship");
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
        git_ai_authors.iter().any(|a| a.contains("Claude")),
        "Should show Claude as author"
    );
}
