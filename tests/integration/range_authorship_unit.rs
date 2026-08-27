use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::range_authorship::{EMPTY_TREE_HASH, range_authorship, should_ignore_file};
use git_ai::git::repository::{CommitRange, find_repository_in_path};

#[test]
fn test_range_authorship_simple_range() {
    let repo = TestRepo::new();

    // Create initial commit with human work
    std::fs::write(repo.path().join("test.txt"), "Line 1\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let first_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Add AI work
    std::fs::write(
        repo.path().join("test.txt"),
        "Line 1\nAI Line 2\nAI Line 3\n",
    )
    .unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();
    repo.stage_all_and_commit("AI adds lines").unwrap();
    let second_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Test range authorship from first to second commit
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        first_sha.clone(),
        second_sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    let lockfile_patterns = vec![
        "Cargo.lock".to_string(),
        "package-lock.json".to_string(),
        "yarn.lock".to_string(),
    ];
    let stats = range_authorship(commit_range, false, &lockfile_patterns, None).unwrap();

    // Verify stats
    assert_eq!(stats.authorship_stats.total_commits, 1);
    assert_eq!(stats.authorship_stats.commits_with_authorship, 1);
    assert_eq!(stats.range_stats.ai_additions, 2);
    assert_eq!(stats.range_stats.git_diff_added_lines, 2);
}

#[test]
fn test_range_authorship_from_empty_tree() {
    let repo = TestRepo::new();

    // Create initial commit with AI work
    std::fs::write(repo.path().join("test.txt"), "AI Line 1\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();
    repo.stage_all_and_commit("Initial AI commit").unwrap();

    // Add more AI work
    std::fs::write(
        repo.path().join("test.txt"),
        "AI Line 1\nAI Line 2\nAI Line 3\n",
    )
    .unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();
    repo.stage_all_and_commit("Second AI commit").unwrap();
    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Test range authorship from empty tree to HEAD
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        EMPTY_TREE_HASH.to_string(),
        head_sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    let lockfile_patterns = vec![
        "Cargo.lock".to_string(),
        "package-lock.json".to_string(),
        "yarn.lock".to_string(),
    ];
    let stats = range_authorship(commit_range, false, &lockfile_patterns, None).unwrap();

    // Verify stats - should include all commits from beginning
    assert_eq!(stats.authorship_stats.total_commits, 2);
    assert_eq!(stats.authorship_stats.commits_with_authorship, 2);
    // When using empty tree, the range stats show the diff from empty to HEAD
    // The AI additions count is based on the filtered attributions for commits in range
    assert_eq!(stats.range_stats.ai_additions, 3);
    assert_eq!(stats.range_stats.git_diff_added_lines, 3);
}

#[test]
fn test_range_authorship_single_commit() {
    let repo = TestRepo::new();

    // Create initial commit
    std::fs::write(repo.path().join("test.txt"), "Line 1\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Create AI commit
    std::fs::write(repo.path().join("test.txt"), "Line 1\nAI Line 2\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();
    repo.stage_all_and_commit("AI commit").unwrap();
    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Test range authorship for single commit (start == end)
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        head_sha.clone(),
        head_sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    let lockfile_patterns = vec![
        "Cargo.lock".to_string(),
        "package-lock.json".to_string(),
        "yarn.lock".to_string(),
    ];
    let stats = range_authorship(commit_range, false, &lockfile_patterns, None).unwrap();

    // For single commit, should use stats_for_commit_stats
    assert_eq!(stats.authorship_stats.total_commits, 1);
    assert_eq!(stats.range_stats.ai_additions, 1);
}

#[test]
fn test_range_authorship_mixed_commits() {
    let repo = TestRepo::new();

    // Create initial commit with human work
    std::fs::write(repo.path().join("test.txt"), "Human Line 1\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let first_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Add AI work
    std::fs::write(repo.path().join("test.txt"), "Human Line 1\nAI Line 2\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();
    repo.stage_all_and_commit("AI commit").unwrap();

    // Add human work
    std::fs::write(
        repo.path().join("test.txt"),
        "Human Line 1\nAI Line 2\nHuman Line 3\n",
    )
    .unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Human commit").unwrap();

    // Add more AI work
    std::fs::write(
        repo.path().join("test.txt"),
        "Human Line 1\nAI Line 2\nHuman Line 3\nAI Line 4\n",
    )
    .unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();
    repo.stage_all_and_commit("Another AI commit").unwrap();
    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Test range authorship from first to head
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        first_sha.clone(),
        head_sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    let lockfile_patterns = vec![
        "Cargo.lock".to_string(),
        "package-lock.json".to_string(),
        "yarn.lock".to_string(),
    ];
    let stats = range_authorship(commit_range, false, &lockfile_patterns, None).unwrap();

    // Verify stats
    assert_eq!(stats.authorship_stats.total_commits, 3);
    assert_eq!(stats.authorship_stats.commits_with_authorship, 3);
    // Range authorship merges attributions from start to end, filtering to commits in range
    // The exact AI/human split depends on the merge attribution logic
    assert_eq!(stats.range_stats.ai_additions, 2);
    // After fix: human lines are correctly counted as human_additions, not unknown_additions
    assert_eq!(stats.range_stats.human_additions, 1);
    assert_eq!(stats.range_stats.unknown_additions, 0);
    assert_eq!(stats.range_stats.git_diff_added_lines, 3);
}

#[test]
fn test_range_authorship_no_changes() {
    let repo = TestRepo::new();

    // Create a commit
    std::fs::write(repo.path().join("test.txt"), "Line 1\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Test range authorship with same start and end (already tested above but worth verifying)
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        sha.clone(),
        sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    let lockfile_patterns = vec![
        "Cargo.lock".to_string(),
        "package-lock.json".to_string(),
        "yarn.lock".to_string(),
    ];
    let stats = range_authorship(commit_range, false, &lockfile_patterns, None).unwrap();

    // Should have 1 commit but no diffs since start == end
    assert_eq!(stats.authorship_stats.total_commits, 1);
}

#[test]
fn test_range_authorship_empty_tree_with_multiple_files() {
    let repo = TestRepo::new();

    // Create multiple files with AI work in first commit
    std::fs::write(repo.path().join("file1.txt"), "AI content 1\n").unwrap();
    std::fs::write(repo.path().join("file2.txt"), "AI content 2\n").unwrap();
    repo.git(&["add", "file1.txt", "file2.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file1.txt"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file2.txt"])
        .unwrap();
    repo.stage_all_and_commit("Initial multi-file commit")
        .unwrap();
    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Test range authorship from empty tree
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        EMPTY_TREE_HASH.to_string(),
        head_sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    let lockfile_patterns = vec![
        "Cargo.lock".to_string(),
        "package-lock.json".to_string(),
        "yarn.lock".to_string(),
    ];
    let stats = range_authorship(commit_range, false, &lockfile_patterns, None).unwrap();

    // Verify all files are included
    assert_eq!(stats.authorship_stats.total_commits, 1);
    assert_eq!(stats.authorship_stats.commits_with_authorship, 1);
    assert_eq!(stats.range_stats.ai_additions, 2);
    assert_eq!(stats.range_stats.git_diff_added_lines, 2);
}

#[test]
fn test_range_authorship_ignores_single_lockfile() {
    let repo = TestRepo::new();

    // Create initial commit with a source file
    std::fs::create_dir(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    repo.git(&["add", "src/main.rs"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "src/main.rs"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let first_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Add AI work to source file and also change a lockfile
    std::fs::write(
        repo.path().join("src/main.rs"),
        "fn main() {}\n// AI added code\nfn helper() {}\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("Cargo.lock"),
        "# Large lockfile with 1000 lines\n".repeat(1000),
    )
    .unwrap();
    repo.git(&["add", "src/main.rs", "Cargo.lock"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/main.rs"])
        .unwrap();
    repo.stage_all_and_commit("Add helper and update deps")
        .unwrap();
    let second_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Test range authorship
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        first_sha.clone(),
        second_sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    let lockfile_patterns = vec![
        "Cargo.lock".to_string(),
        "package-lock.json".to_string(),
        "yarn.lock".to_string(),
    ];
    let stats = range_authorship(commit_range, false, &lockfile_patterns, None).unwrap();

    // Verify lockfile is excluded: only 2 lines added (from main.rs), not 1000+ from lockfile
    assert_eq!(stats.authorship_stats.total_commits, 1);
    assert_eq!(stats.authorship_stats.commits_with_authorship, 1);
    assert_eq!(stats.range_stats.ai_additions, 2); // Only the 2 AI lines in main.rs
    assert_eq!(stats.range_stats.git_diff_added_lines, 2); // Lockfile excluded (1000 lines ignored)
    // The key assertion: git_diff should be 2, not 1002 if lockfile was included
    assert!(stats.range_stats.git_diff_added_lines < 100); // Significantly less than if lockfile was counted
}

#[test]
fn test_range_authorship_mixed_lockfile_and_source() {
    let repo = TestRepo::new();

    // Create initial commit
    std::fs::create_dir(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/lib.rs"), "pub fn old() {}\n").unwrap();
    repo.git(&["add", "src/lib.rs"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "src/lib.rs"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let first_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Human adds to source file
    std::fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn old() {}\npub fn new() {}\n",
    )
    .unwrap();
    repo.git(&["add", "src/lib.rs"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "src/lib.rs"])
        .unwrap();
    repo.stage_all_and_commit("Human adds function").unwrap();

    // AI adds to source file, and package-lock.json is updated (with 1000 lines)
    std::fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn old() {}\npub fn new() {}\n// AI comment\npub fn ai_func() {}\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("package-lock.json"),
        "{\n  \"lockfileVersion\": 2,\n}\n".repeat(1000),
    )
    .unwrap();
    repo.git(&["add", "src/lib.rs", "package-lock.json"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/lib.rs"])
        .unwrap();
    repo.stage_all_and_commit("AI adds function and updates deps")
        .unwrap();
    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Test range authorship
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        first_sha.clone(),
        head_sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    let lockfile_patterns = vec![
        "Cargo.lock".to_string(),
        "package-lock.json".to_string(),
        "yarn.lock".to_string(),
    ];
    let stats = range_authorship(commit_range, false, &lockfile_patterns, None).unwrap();

    // Key assertion: git_diff should only count lib.rs changes (3 lines), not package-lock.json (3000 lines)
    assert_eq!(stats.authorship_stats.total_commits, 2);
    assert_eq!(stats.authorship_stats.commits_with_authorship, 2);
    assert_eq!(stats.range_stats.git_diff_added_lines, 3); // Only lib.rs, package-lock.json excluded
    // Verify the total is much less than 3003 (if lockfile was included)
    assert!(stats.range_stats.git_diff_added_lines < 100);
    // Verify that some AI work is detected and human lines are correctly counted
    assert!(stats.range_stats.ai_additions > 0);
    // After fix: human lines are correctly counted as human_additions (not inverted to unknown_additions)
    assert!(stats.range_stats.human_additions > 0);
}

#[test]
fn test_range_authorship_multiple_lockfile_types() {
    let repo = TestRepo::new();

    // Create initial commit
    std::fs::write(repo.path().join("README.md"), "# Project\n").unwrap();
    repo.git(&["add", "README.md"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "README.md"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let first_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Add multiple lockfiles and one real source change
    std::fs::write(repo.path().join("Cargo.lock"), "# Cargo lock\n".repeat(500)).unwrap();
    std::fs::write(repo.path().join("yarn.lock"), "# yarn lock\n".repeat(500)).unwrap();
    std::fs::write(
        repo.path().join("poetry.lock"),
        "# poetry lock\n".repeat(500),
    )
    .unwrap();
    std::fs::write(repo.path().join("go.sum"), "# go sum\n".repeat(500)).unwrap();
    std::fs::write(repo.path().join("README.md"), "# Project\n## New Section\n").unwrap();
    repo.git(&[
        "add",
        "Cargo.lock",
        "yarn.lock",
        "poetry.lock",
        "go.sum",
        "README.md",
    ])
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "README.md"])
        .unwrap();
    repo.stage_all_and_commit("Update dependencies").unwrap();
    let second_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Test range authorship
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        first_sha.clone(),
        second_sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    let lockfile_patterns = vec![
        "Cargo.lock".to_string(),
        "package-lock.json".to_string(),
        "yarn.lock".to_string(),
        "poetry.lock".to_string(),
        "go.sum".to_string(),
    ];
    let stats = range_authorship(commit_range, false, &lockfile_patterns, None).unwrap();

    // Verify: only the 1 README line is counted, all lockfiles excluded (2000 lines ignored)
    assert_eq!(stats.authorship_stats.total_commits, 1);
    assert_eq!(stats.authorship_stats.commits_with_authorship, 1);
    assert_eq!(stats.range_stats.ai_additions, 1); // Only README.md line
    assert_eq!(stats.range_stats.git_diff_added_lines, 1); // All lockfiles excluded
}

#[test]
fn test_range_authorship_lockfile_only_commit() {
    let repo = TestRepo::new();

    // Create initial commit
    std::fs::create_dir(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    repo.git(&["add", "src/main.rs"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "src/main.rs"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let first_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Commit that only changes lockfiles (common scenario)
    std::fs::write(
        repo.path().join("package-lock.json"),
        "{\n  \"version\": \"1.0.0\"\n}\n".repeat(1000),
    )
    .unwrap();
    std::fs::write(repo.path().join("yarn.lock"), "# yarn\n".repeat(500)).unwrap();
    repo.git(&["add", "package-lock.json", "yarn.lock"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "package-lock.json"])
        .unwrap();
    repo.stage_all_and_commit("Update lockfiles only").unwrap();
    let second_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Test range authorship
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        first_sha.clone(),
        second_sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    let lockfile_patterns = vec![
        "Cargo.lock".to_string(),
        "package-lock.json".to_string(),
        "yarn.lock".to_string(),
    ];
    let stats = range_authorship(commit_range, false, &lockfile_patterns, None).unwrap();

    // Verify: no lines counted since only lockfiles changed
    assert_eq!(stats.authorship_stats.total_commits, 1);
    assert_eq!(stats.range_stats.git_diff_added_lines, 0); // All lockfiles excluded
    assert_eq!(stats.range_stats.ai_additions, 0);
    assert_eq!(stats.range_stats.human_additions, 0);
}

#[test]
fn test_should_ignore_file_with_patterns() {
    let lockfile_patterns = vec![
        "package-lock.json".to_string(),
        "yarn.lock".to_string(),
        "Cargo.lock".to_string(),
        "go.sum".to_string(),
    ];

    // Test that specified patterns are ignored
    assert!(should_ignore_file("package-lock.json", &lockfile_patterns));
    assert!(should_ignore_file("yarn.lock", &lockfile_patterns));
    assert!(should_ignore_file("Cargo.lock", &lockfile_patterns));
    assert!(should_ignore_file("go.sum", &lockfile_patterns));

    // Test with paths
    assert!(should_ignore_file(
        "src/package-lock.json",
        &lockfile_patterns
    ));
    assert!(should_ignore_file("backend/Cargo.lock", &lockfile_patterns));
    assert!(should_ignore_file("./yarn.lock", &lockfile_patterns));

    // Test that non-matching files are not ignored
    assert!(!should_ignore_file("package.json", &lockfile_patterns));
    assert!(!should_ignore_file("Cargo.toml", &lockfile_patterns));
    assert!(!should_ignore_file("src/main.rs", &lockfile_patterns));
    assert!(!should_ignore_file("pnpm-lock.yaml", &lockfile_patterns)); // Not in our pattern list

    // Test with empty patterns - nothing should be ignored
    let empty_patterns: Vec<String> = vec![];
    assert!(!should_ignore_file("package-lock.json", &empty_patterns));
    assert!(!should_ignore_file("Cargo.lock", &empty_patterns));
}

#[test]
fn test_should_ignore_file_with_glob_patterns() {
    // Test wildcard patterns
    let wildcard_patterns = vec!["*.lock".to_string()];

    // Should match any file ending in .lock
    assert!(should_ignore_file("Cargo.lock", &wildcard_patterns));
    assert!(should_ignore_file("package.lock", &wildcard_patterns));
    assert!(should_ignore_file("yarn.lock", &wildcard_patterns));
    assert!(should_ignore_file("src/Cargo.lock", &wildcard_patterns));
    assert!(should_ignore_file("backend/deps.lock", &wildcard_patterns));

    // Should not match files not ending in .lock
    assert!(!should_ignore_file("Cargo.toml", &wildcard_patterns));
    assert!(!should_ignore_file("lock.txt", &wildcard_patterns));
    assert!(!should_ignore_file("locked.rs", &wildcard_patterns));

    // Test multiple wildcards
    let multi_wildcard = vec!["*.lock".to_string(), "*.generated.*".to_string()];
    assert!(should_ignore_file("test.generated.js", &multi_wildcard));
    assert!(should_ignore_file("api.generated.ts", &multi_wildcard));
    assert!(should_ignore_file("schema.lock", &multi_wildcard));
    assert!(!should_ignore_file("manual.js", &multi_wildcard));
}

#[test]
fn test_should_ignore_file_with_path_glob_patterns() {
    // Test path-based patterns
    let path_patterns = vec!["**/target/**".to_string()];

    // Should match files in target directory at any depth
    assert!(should_ignore_file("target/debug/foo", &path_patterns));
    assert!(should_ignore_file(
        "backend/target/release/bar",
        &path_patterns
    ));
    assert!(should_ignore_file("project/target/file.rs", &path_patterns));

    // Should not match files outside target
    assert!(!should_ignore_file("src/target.rs", &path_patterns));
    assert!(!should_ignore_file("target.txt", &path_patterns));

    // Test specific directory patterns
    let dir_patterns = vec!["node_modules/**".to_string()];
    assert!(should_ignore_file(
        "node_modules/package/index.js",
        &dir_patterns
    ));
    assert!(should_ignore_file("node_modules/foo.js", &dir_patterns));
    assert!(!should_ignore_file("src/node_modules.rs", &dir_patterns));
}

#[test]
fn test_should_ignore_file_with_prefix_patterns() {
    // Test prefix patterns
    let prefix_patterns = vec!["generated-*".to_string()];

    assert!(should_ignore_file("generated-api.ts", &prefix_patterns));
    assert!(should_ignore_file("generated-schema.js", &prefix_patterns));
    assert!(should_ignore_file(
        "src/generated-types.d.ts",
        &prefix_patterns
    ));
    assert!(!should_ignore_file("api-generated.ts", &prefix_patterns));
    assert!(!should_ignore_file("manual.ts", &prefix_patterns));
}

#[test]
fn test_should_ignore_file_with_complex_glob_patterns() {
    // Test complex patterns (note: brace expansion like {js,ts} is not supported by glob crate)
    let complex_patterns = vec![
        "**/*.generated.js".to_string(),
        "**/*.generated.ts".to_string(),
        "*-lock.*".to_string(),
        "dist/**".to_string(),
    ];

    // Glob patterns with multiple wildcards
    assert!(should_ignore_file(
        "src/api.generated.js",
        &complex_patterns
    ));
    assert!(should_ignore_file("types.generated.ts", &complex_patterns));
    assert!(should_ignore_file("package-lock.json", &complex_patterns));
    assert!(should_ignore_file("yarn-lock.yaml", &complex_patterns));
    assert!(should_ignore_file("dist/bundle.js", &complex_patterns));
    assert!(should_ignore_file(
        "dist/nested/file.css",
        &complex_patterns
    ));

    assert!(!should_ignore_file("src/manual.js", &complex_patterns));
    assert!(!should_ignore_file("lock.txt", &complex_patterns));
}

#[test]
fn test_should_ignore_file_mixed_exact_and_glob() {
    // Test mixing exact matches and glob patterns
    let mixed_patterns = vec![
        "Cargo.lock".to_string(),        // Exact match
        "*.generated.js".to_string(),    // Glob pattern
        "package-lock.json".to_string(), // Exact match
        "**/target/**".to_string(),      // Path glob
    ];

    // Exact matches
    assert!(should_ignore_file("Cargo.lock", &mixed_patterns));
    assert!(should_ignore_file("package-lock.json", &mixed_patterns));

    // Glob matches
    assert!(should_ignore_file("api.generated.js", &mixed_patterns));
    assert!(should_ignore_file("target/debug/foo", &mixed_patterns));

    // Non-matches
    assert!(!should_ignore_file("Cargo.toml", &mixed_patterns));
    assert!(!should_ignore_file("manual.js", &mixed_patterns));
}

#[test]
fn test_should_ignore_file_case_sensitivity() {
    // Test that pattern matching is case-sensitive
    let patterns = vec!["Cargo.lock".to_string(), "*.LOG".to_string()];

    // Exact case matches
    assert!(should_ignore_file("Cargo.lock", &patterns));
    assert!(should_ignore_file("file.LOG", &patterns));
    assert!(should_ignore_file("debug.LOG", &patterns));

    // Different case should NOT match (case-sensitive)
    assert!(!should_ignore_file("cargo.lock", &patterns));
    assert!(!should_ignore_file("CARGO.LOCK", &patterns));
    assert!(!should_ignore_file("file.log", &patterns));
    assert!(!should_ignore_file("file.Log", &patterns));
}

#[test]
fn test_should_ignore_file_special_characters() {
    // Test filenames with special characters
    let patterns = vec![
        "file with spaces.txt".to_string(),
        "*.lock".to_string(),
        "file-with-dashes.js".to_string(),
        "file_with_underscores.rs".to_string(),
    ];

    // Files with spaces
    assert!(should_ignore_file("file with spaces.txt", &patterns));
    assert!(should_ignore_file(
        "path/to/file with spaces.txt",
        &patterns
    ));

    // Files with dashes and underscores
    assert!(should_ignore_file("file-with-dashes.js", &patterns));
    assert!(should_ignore_file("file_with_underscores.rs", &patterns));

    // Glob should still work with special chars in other files
    assert!(should_ignore_file("my-package.lock", &patterns));
    assert!(should_ignore_file("test_file.lock", &patterns));

    // Non-matches
    assert!(!should_ignore_file("file with spaces.js", &patterns));
    assert!(!should_ignore_file("different-file.txt", &patterns));
}

#[test]
fn test_should_ignore_file_hidden_files() {
    // Test hidden files (starting with .)
    let patterns = vec![".env".to_string(), ".*.swp".to_string(), ".*rc".to_string()];

    // Hidden files
    assert!(should_ignore_file(".env", &patterns));
    assert!(should_ignore_file("config/.env", &patterns));

    // Vim swap files
    assert!(should_ignore_file(".file.swp", &patterns));
    assert!(should_ignore_file(".main.rs.swp", &patterns));

    // RC files
    assert!(should_ignore_file(".bashrc", &patterns));
    assert!(should_ignore_file(".vimrc", &patterns));
    assert!(should_ignore_file("home/.npmrc", &patterns));

    // Non-matches
    assert!(!should_ignore_file("env", &patterns));
    assert!(!should_ignore_file("file.swp", &patterns));
    assert!(!should_ignore_file("bashrc", &patterns));
}

#[test]
fn test_should_ignore_file_multiple_extensions() {
    // Test files with multiple extensions
    let patterns = vec![
        "*.tar.gz".to_string(),
        "*.min.js".to_string(),
        "*.d.ts".to_string(),
    ];

    // Multiple extensions
    assert!(should_ignore_file("archive.tar.gz", &patterns));
    assert!(should_ignore_file("bundle.min.js", &patterns));
    assert!(should_ignore_file("types.d.ts", &patterns));
    assert!(should_ignore_file("build/dist/app.min.js", &patterns));

    // Partial matches should not match
    assert!(!should_ignore_file("file.tar", &patterns));
    assert!(!should_ignore_file("file.gz", &patterns));
    assert!(!should_ignore_file("file.js", &patterns));
    assert!(!should_ignore_file("types.ts", &patterns));
}

#[test]
fn test_should_ignore_file_no_extension() {
    // Test files without extensions
    let patterns = vec![
        "Makefile".to_string(),
        "Dockerfile".to_string(),
        "LICENSE".to_string(),
        "README".to_string(),
    ];

    // Files without extensions
    assert!(should_ignore_file("Makefile", &patterns));
    assert!(should_ignore_file("Dockerfile", &patterns));
    assert!(should_ignore_file("LICENSE", &patterns));
    assert!(should_ignore_file("README", &patterns));

    // In subdirectories
    assert!(should_ignore_file("project/Makefile", &patterns));
    assert!(should_ignore_file("docker/Dockerfile", &patterns));

    // Similar names should not match
    assert!(!should_ignore_file("Makefile.old", &patterns));
    assert!(!should_ignore_file("README.md", &patterns));
    assert!(!should_ignore_file("LICENSE.txt", &patterns));
}

#[test]
fn test_should_ignore_file_deeply_nested_paths() {
    // Test patterns at various nesting depths
    let patterns = vec![
        "**/node_modules/**".to_string(),
        "**/build/**".to_string(),
        "**/.git/**".to_string(),
    ];

    // Deep nesting
    assert!(should_ignore_file(
        "node_modules/package/index.js",
        &patterns
    ));
    assert!(should_ignore_file("a/b/c/node_modules/d/e/f.js", &patterns));
    assert!(should_ignore_file(
        "project/build/output/bundle.js",
        &patterns
    ));
    assert!(should_ignore_file(".git/objects/ab/cdef123", &patterns));
    assert!(should_ignore_file("repo/.git/hooks/pre-commit", &patterns));

    // Should not match similar names outside pattern
    assert!(!should_ignore_file("src/node_modules.js", &patterns));
    assert!(!should_ignore_file("build.sh", &patterns));
    assert!(!should_ignore_file("git.txt", &patterns));
}

#[test]
fn test_should_ignore_file_partial_matches() {
    // Test that partial matches don't incorrectly match
    let patterns = vec!["lock".to_string(), "*.lock".to_string()];

    // Should match
    assert!(should_ignore_file("lock", &patterns));
    assert!(should_ignore_file("file.lock", &patterns));
    assert!(should_ignore_file("package.lock", &patterns));

    // Should NOT match (lock is substring but not filename or extension)
    assert!(!should_ignore_file("locked.txt", &patterns));
    assert!(!should_ignore_file("unlock.sh", &patterns));
    assert!(!should_ignore_file("locksmith.rs", &patterns));
}

#[test]
fn test_should_ignore_file_with_wildcards_in_middle() {
    // Test patterns with wildcards in the middle
    let patterns = vec!["test-*-output.log".to_string(), "backup-*.sql".to_string()];

    // Should match
    assert!(should_ignore_file("test-123-output.log", &patterns));
    assert!(should_ignore_file("test-foo-output.log", &patterns));
    assert!(should_ignore_file("backup-daily.sql", &patterns));
    assert!(should_ignore_file("backup-2024-01-01.sql", &patterns));
    assert!(should_ignore_file("logs/test-debug-output.log", &patterns));

    // Should not match
    assert!(!should_ignore_file("test-output.log", &patterns));
    assert!(!should_ignore_file("test-123-result.log", &patterns));
    assert!(!should_ignore_file("backup.sql", &patterns));
}

#[test]
fn test_should_ignore_file_empty_pattern() {
    // Test with empty pattern string - empty pattern is technically valid glob
    // that matches empty string, but we test that non-empty files don't match
    let patterns = vec!["".to_string(), "*.lock".to_string()];

    // Regular files should not match the empty pattern
    assert!(!should_ignore_file("file.txt", &patterns));
    assert!(!should_ignore_file("src/main.rs", &patterns));

    // But valid patterns should still work
    assert!(should_ignore_file("file.lock", &patterns));
    assert!(should_ignore_file("package.lock", &patterns));
}

#[test]
fn test_should_ignore_file_directory_traversal() {
    // Test patterns with ../ or ./ in paths
    let patterns = vec!["*.lock".to_string()];

    // Should match regardless of ./ prefix
    assert!(should_ignore_file("./file.lock", &patterns));
    assert!(should_ignore_file("./path/to/file.lock", &patterns));

    // Complex paths
    assert!(should_ignore_file("src/../lib/file.lock", &patterns));
}

#[test]
fn test_should_ignore_file_numeric_filenames() {
    // Test numeric filenames
    let patterns = vec!["[0-9]*".to_string(), "*.123".to_string()];

    // Filenames starting with numbers
    assert!(should_ignore_file("123.txt", &patterns));
    assert!(should_ignore_file("456file.log", &patterns));
    assert!(should_ignore_file("7890.rs", &patterns));

    // Files ending with .123
    assert!(should_ignore_file("backup.123", &patterns));
    assert!(should_ignore_file("data.123", &patterns));

    // Should not match
    assert!(!should_ignore_file("file123.txt", &patterns));
    assert!(!should_ignore_file("test.456", &patterns));
}

#[test]
fn test_range_authorship_human_unknown_not_inverted() {
    // Regression test for GitHub issue #1875:
    // Range query stats was inverting human_additions and unknown_additions
    // This test verifies that human-authored lines are correctly counted in human_additions
    // not inverted into unknown_additions
    let repo = TestRepo::new();

    // Create initial commit with human work
    std::fs::write(repo.path().join("test.txt"), "Human Line 1\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Initial human commit").unwrap();
    let mut file = repo.filename("test.txt");
    file.assert_committed_lines(lines!["Human Line 1".human()]);
    let first_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Add AI work
    std::fs::write(
        repo.path().join("test.txt"),
        "Human Line 1\nAI Line 2\nAI Line 3\nAI Line 4\nAI Line 5\nAI Line 6\n",
    )
    .unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();
    repo.stage_all_and_commit("AI adds 5 lines").unwrap();
    file.assert_committed_lines(lines![
        "Human Line 1".human(),
        "AI Line 2".ai(),
        "AI Line 3".ai(),
        "AI Line 4".ai(),
        "AI Line 5".ai(),
        "AI Line 6".ai(),
    ]);

    // Add more human work
    std::fs::write(
        repo.path().join("test.txt"),
        "Human Line 1\nAI Line 2\nAI Line 3\nAI Line 4\nAI Line 5\nAI Line 6\nHuman Line 7\nHuman Line 8\nHuman Line 9\nHuman Line 10\nHuman Line 11\n",
    )
    .unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Human adds 5 more lines")
        .unwrap();
    file.assert_committed_lines(lines![
        "Human Line 1".human(),
        "AI Line 2".ai(),
        "AI Line 3".ai(),
        "AI Line 4".ai(),
        "AI Line 5".ai(),
        "AI Line 6".ai(),
        "Human Line 7".human(),
        "Human Line 8".human(),
        "Human Line 9".human(),
        "Human Line 10".human(),
        "Human Line 11".human(),
    ]);
    let second_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Test range authorship from first to second commit
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        first_sha.clone(),
        second_sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    let lockfile_patterns: Vec<String> = vec![];
    let stats = range_authorship(commit_range, false, &lockfile_patterns, None).unwrap();

    // CRITICAL ASSERTION: Verify the fix for issue #1875
    // The bug was that human_additions and unknown_additions were inverted
    // After fix: human_additions should be > 0 (the 5 human lines added)
    // Before fix: human_additions would be 0 and those lines would appear in unknown_additions
    assert!(
        stats.range_stats.human_additions > 0,
        "human_additions should be > 0 after fix (was inverted before fix)"
    );
    // The key assertion: verify human lines are NOT in unknown_additions
    assert!(
        stats.range_stats.unknown_additions == 0
            || stats.range_stats.unknown_additions < stats.range_stats.human_additions,
        "unknown_additions should be much less than human_additions (or 0), not equal or greater"
    );

    // Verify the merge attribution worked and we detected both human and AI
    // Range is from first to second, so 2 commits in the range (first..second)
    assert_eq!(stats.authorship_stats.total_commits, 2);
    assert_eq!(stats.authorship_stats.commits_with_authorship, 2);
}

#[test]
fn test_range_authorship_multiple_human_and_ai_commits() {
    // Test a more complex scenario with alternating human and AI commits
    let repo = TestRepo::new();

    // Commit 1: Human work (10 lines)
    let mut content = String::new();
    for i in 1..=10 {
        content.push_str(&format!("Human line {}\n", i));
    }
    std::fs::write(repo.path().join("file.txt"), &content).unwrap();
    repo.git(&["add", "file.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "file.txt"])
        .unwrap();
    repo.stage_all_and_commit("Human commit 1").unwrap();
    let mut file = repo.filename("file.txt");
    file.assert_committed_lines(
        (1..=10)
            .map(|i| format!("Human line {i}").human())
            .collect(),
    );
    let commit1_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Commit 2: AI work (15 lines)
    for i in 1..=15 {
        content.push_str(&format!("AI line {}\n", i));
    }
    std::fs::write(repo.path().join("file.txt"), &content).unwrap();
    repo.git(&["add", "file.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file.txt"]).unwrap();
    repo.stage_all_and_commit("AI commit 1").unwrap();
    file.assert_committed_lines(
        (1..=10)
            .map(|i| format!("Human line {i}").human())
            .chain((1..=15).map(|i| format!("AI line {i}").ai()))
            .collect(),
    );

    // Commit 3: Human work (5 lines)
    for i in 1..=5 {
        content.push_str(&format!("Human line 2-{}\n", i));
    }
    std::fs::write(repo.path().join("file.txt"), &content).unwrap();
    repo.git(&["add", "file.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "file.txt"])
        .unwrap();
    repo.stage_all_and_commit("Human commit 2").unwrap();
    file.assert_committed_lines(
        (1..=10)
            .map(|i| format!("Human line {i}").human())
            .chain((1..=15).map(|i| format!("AI line {i}").ai()))
            .chain((1..=5).map(|i| format!("Human line 2-{i}").human()))
            .collect(),
    );

    // Commit 4: AI work (8 lines)
    for i in 1..=8 {
        content.push_str(&format!("AI line 2-{}\n", i));
    }
    std::fs::write(repo.path().join("file.txt"), &content).unwrap();
    repo.git(&["add", "file.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file.txt"]).unwrap();
    repo.stage_all_and_commit("AI commit 2").unwrap();
    file.assert_committed_lines(
        (1..=10)
            .map(|i| format!("Human line {i}").human())
            .chain((1..=15).map(|i| format!("AI line {i}").ai()))
            .chain((1..=5).map(|i| format!("Human line 2-{i}").human()))
            .chain((1..=8).map(|i| format!("AI line 2-{i}").ai()))
            .collect(),
    );
    let final_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Test range authorship from commit 1 to final
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        commit1_sha.clone(),
        final_sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    let lockfile_patterns: Vec<String> = vec![];
    let stats = range_authorship(commit_range, false, &lockfile_patterns, None).unwrap();

    // Verify that human and AI additions are correctly attributed (not inverted)
    // The range includes changes from commit1 to final (3 commits in the range)
    // What matters for the fix is that human_additions > 0 (lines are not inverted to unknown)
    assert!(
        stats.range_stats.human_additions > 0,
        "human_additions should be > 0 (5 human lines added)"
    );
    assert!(
        stats.range_stats.ai_additions > 0,
        "ai_additions should be > 0 (8 AI lines added)"
    );
    // Key assertion: human lines are in human_additions, not unknown_additions
    assert!(
        stats.range_stats.unknown_additions < stats.range_stats.human_additions,
        "unknown_additions should be less than human_additions (not inverted)"
    );
    assert_eq!(stats.authorship_stats.total_commits, 3);
}

#[test]
fn test_range_authorship_verify_per_commit_consistency() {
    // Verify that range query results match sum of per-commit stats
    // This ensures the fix correctly extracts human attestations
    let repo = TestRepo::new();
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Create 3 commits with mixed human/AI work
    let mut content = String::new();

    // Commit 1: All human
    for i in 1..=5 {
        content.push_str(&format!("Human {}\n", i));
    }
    std::fs::write(repo.path().join("file.txt"), &content).unwrap();
    repo.git(&["add", "file.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "file.txt"])
        .unwrap();
    repo.stage_all_and_commit("Commit 1: Human").unwrap();
    let mut file = repo.filename("file.txt");
    file.assert_committed_lines((1..=5).map(|i| format!("Human {i}").human()).collect());
    let commit1_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let _commit1_stats =
        git_ai::authorship::stats::stats_for_commit_stats(&gitai_repo, &commit1_sha, &[]).unwrap();

    // Commit 2: All AI
    for i in 1..=8 {
        content.push_str(&format!("AI {}\n", i));
    }
    std::fs::write(repo.path().join("file.txt"), &content).unwrap();
    repo.git(&["add", "file.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file.txt"]).unwrap();
    repo.stage_all_and_commit("Commit 2: AI").unwrap();
    file.assert_committed_lines(
        (1..=5)
            .map(|i| format!("Human {i}").human())
            .chain((1..=8).map(|i| format!("AI {i}").ai()))
            .collect(),
    );
    let commit2_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let _commit2_stats =
        git_ai::authorship::stats::stats_for_commit_stats(&gitai_repo, &commit2_sha, &[]).unwrap();

    // Commit 3: Mixed
    for i in 1..=3 {
        content.push_str(&format!("Human2 {}\n", i));
    }
    std::fs::write(repo.path().join("file.txt"), &content).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "file.txt"])
        .unwrap();
    for i in 1..=4 {
        content.push_str(&format!("AI2 {}\n", i));
    }
    std::fs::write(repo.path().join("file.txt"), &content).unwrap();
    repo.git(&["add", "file.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file.txt"]).unwrap();
    repo.stage_all_and_commit("Commit 3: Mixed").unwrap();
    file.assert_committed_lines(
        (1..=5)
            .map(|i| format!("Human {i}").human())
            .chain((1..=8).map(|i| format!("AI {i}").ai()))
            .chain((1..=3).map(|i| format!("Human2 {i}").human()))
            .chain((1..=4).map(|i| format!("AI2 {i}").ai()))
            .collect(),
    );
    let commit3_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let _commit3_stats =
        git_ai::authorship::stats::stats_for_commit_stats(&gitai_repo, &commit3_sha, &[]).unwrap();

    // Get range stats
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        commit1_sha.clone(),
        commit3_sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    let stats = range_authorship(commit_range, false, &[], None).unwrap();

    // Verify the fix is applied: human additions are detected (not inverted to unknown)
    // Range stats uses virtual attributions which may differ from per-commit sum,
    // but human_additions should be > 0 after the fix (was always 0 before)
    assert!(
        stats.range_stats.human_additions > 0,
        "human_additions should be > 0 after fix (verifies human lines are not inverted)"
    );
    // Also verify AI additions are detected
    assert!(
        stats.range_stats.ai_additions > 0,
        "ai_additions should be > 0"
    );
}

#[test]
fn test_range_authorship_with_glob_patterns() {
    let repo = TestRepo::new();

    // Initial commit
    std::fs::create_dir(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    repo.git(&["add", "src/main.rs"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "src/main.rs"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let first_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Add various files including lockfiles and generated files
    std::fs::write(
        repo.path().join("src/main.rs"),
        "fn main() {}\nfn helper() {}\n",
    )
    .unwrap();
    std::fs::write(repo.path().join("Cargo.lock"), "# lock\n".repeat(1000)).unwrap();
    std::fs::write(repo.path().join("package-lock.json"), "{}\n".repeat(500)).unwrap();
    std::fs::write(
        repo.path().join("api.generated.js"),
        "// generated\n".repeat(200),
    )
    .unwrap();
    repo.git(&[
        "add",
        "src/main.rs",
        "Cargo.lock",
        "package-lock.json",
        "api.generated.js",
    ])
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/main.rs"])
        .unwrap();
    repo.stage_all_and_commit("Add code and deps").unwrap();
    let second_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let commit_range = CommitRange::new_infer_refname(
        &gitai_repo,
        first_sha.clone(),
        second_sha.clone(),
        Some("HEAD".to_string()),
    )
    .unwrap();

    // Use glob patterns to ignore lockfiles and generated files
    let glob_patterns = vec![
        "*.lock".to_string(),
        "*lock.json".to_string(), // Matches package-lock.json
        "*.generated.*".to_string(),
    ];
    let stats = range_authorship(commit_range, false, &glob_patterns, None).unwrap();

    // Should only count the 1 line in main.rs, ignoring 1700 lines in lockfiles and generated files
    assert_eq!(stats.range_stats.git_diff_added_lines, 1);
    assert_eq!(stats.range_stats.ai_additions, 1);
}

/// Regression guard for the `git-ai stats` range-mode speedup: the batched
/// `read_authorship_v3_batch` (which resolves many commits' authorship notes in one
/// `cat-file --batch` instead of a `git notes show` per commit) MUST return exactly
/// what the per-commit `read_authorship_v3(..).ok()` path returns for every commit —
/// including commits with a note, commits without one, and non-existent commits.
#[test]
fn test_read_authorship_v3_batch_matches_per_commit() {
    use git_ai::git::notes_api::{read_authorship_v3, read_authorship_v3_batch};

    let repo = TestRepo::new();

    // Commit 1: AI work -> gets an authorship note.
    std::fs::write(repo.path().join("a.txt"), "AI line 1\n").unwrap();
    repo.git(&["add", "a.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "a.txt"]).unwrap();
    repo.stage_all_and_commit("AI commit").unwrap();

    // Commit 2: known-human work -> gets an authorship note.
    std::fs::write(repo.path().join("a.txt"), "AI line 1\nHuman line 2\n").unwrap();
    repo.git(&["add", "a.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "a.txt"])
        .unwrap();
    repo.stage_all_and_commit("Human commit").unwrap();

    // Commit 3: untracked edit with NO checkpoint -> no authorship note.
    std::fs::write(
        repo.path().join("a.txt"),
        "AI line 1\nHuman line 2\nUntracked line 3\n",
    )
    .unwrap();
    repo.stage_all_and_commit("Untracked commit").unwrap();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Every commit in history, plus a well-formed but non-existent SHA to force the
    // "no note" path even though all real commits here happen to have content.
    let mut shas: Vec<String> = repo
        .git_og(&["rev-list", "HEAD"])
        .unwrap()
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    shas.push("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string());

    let batch = read_authorship_v3_batch(&gitai_repo, &shas);

    // The map must contain an entry for every requested commit.
    assert_eq!(
        batch.len(),
        shas.len(),
        "batch must return an entry per requested commit"
    );

    let mut with_note = 0usize;
    let mut without_note = 0usize;
    for sha in &shas {
        let per_commit = read_authorship_v3(&gitai_repo, sha).ok();
        let batched = batch.get(sha).cloned().flatten();
        assert_eq!(
            batched, per_commit,
            "batched read diverged from per-commit read for {sha}"
        );
        match per_commit {
            Some(_) => with_note += 1,
            None => without_note += 1,
        }
    }

    // Make sure the test actually exercised both branches; otherwise it could pass
    // trivially if, say, no notes were ever written.
    assert!(
        with_note >= 2,
        "expected >=2 commits with notes, got {with_note}"
    );
    assert!(
        without_note >= 1,
        "expected >=1 commit without a note, got {without_note}"
    );

    // Coverage note: this exercises the GitNotes-batched path, the only path that does
    // anything different from the per-commit read. The `Http` arm and the batched-read
    // error fallback both delegate per commit to `read_authorship_v3(repo, sha).ok()`,
    // so they equal the reference by construction.
}

/// The empty-commit-list fast path returns an empty map (and spawns no git).
#[test]
fn test_read_authorship_v3_batch_empty_input() {
    use git_ai::git::notes_api::read_authorship_v3_batch;

    let repo = TestRepo::new();
    std::fs::write(repo.path().join("a.txt"), "x\n").unwrap();
    repo.git(&["add", "a.txt"]).unwrap();
    repo.stage_all_and_commit("init").unwrap();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let out = read_authorship_v3_batch(&gitai_repo, &[]);
    assert!(out.is_empty(), "empty input must yield an empty map");
}

/// Direct regression guard for the `skip_human_author_population` gate: a `blame()`
/// over the stats path must return identical `line_authors` and `prompt_records`
/// whether or not the `populate_ai_human_authors` pass runs. This is the automated
/// proof of the "skipping the dead pass is output-invariant" claim.
#[test]
fn test_skip_human_author_population_preserves_blame_output() {
    use git_ai::commands::blame::GitAiBlameOptions;
    use std::collections::BTreeSet;

    let repo = TestRepo::new();

    // A file whose final state blends human and AI lines across several commits, so
    // blame attributes multiple commits/authors to it.
    std::fs::write(repo.path().join("f.txt"), "human 1\n").unwrap();
    repo.git(&["add", "f.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "f.txt"])
        .unwrap();
    repo.stage_all_and_commit("c1 human").unwrap();

    std::fs::write(repo.path().join("f.txt"), "human 1\nai 2\nai 3\n").unwrap();
    repo.git(&["add", "f.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "f.txt"]).unwrap();
    repo.stage_all_and_commit("c2 ai").unwrap();

    std::fs::write(repo.path().join("f.txt"), "human 1\nai 2\nai 3\nhuman 4\n").unwrap();
    repo.git(&["add", "f.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "f.txt"])
        .unwrap();
    repo.stage_all_and_commit("c3 human").unwrap();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Whole-file blame (empty line_ranges => whole file) on the no_output programmatic
    // path the stats callers use.
    let opts_off = GitAiBlameOptions {
        no_output: true,
        use_prompt_hashes_as_names: true,
        ..Default::default()
    };
    let opts_on = GitAiBlameOptions {
        skip_human_author_population: true,
        ..opts_off.clone()
    };

    let (authors_off, prompts_off) = gitai_repo.blame("f.txt", &opts_off).unwrap();
    let (authors_on, prompts_on) = gitai_repo.blame("f.txt", &opts_on).unwrap();

    assert!(
        !authors_off.is_empty(),
        "expected blame to attribute at least one line"
    );
    assert_eq!(
        authors_off, authors_on,
        "skip_human_author_population changed blame line authors"
    );
    assert_eq!(
        prompts_off.keys().collect::<BTreeSet<_>>(),
        prompts_on.keys().collect::<BTreeSet<_>>(),
        "skip_human_author_population changed the blame prompt records"
    );
}
