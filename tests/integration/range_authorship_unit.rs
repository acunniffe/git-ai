use crate::repos::test_repo::TestRepo;
use git_ai::operations::authorship::range_authorship::{
    EMPTY_TREE_HASH, range_authorship, should_ignore_file,
};
use git_ai::operations::git::repository::{CommitRange, find_repository_in_path};

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
    // Known-human lines in the range count as human additions, not unknown.
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
    // Verify that some AI work is detected and human lines are correctly
    // counted as human additions rather than unknown.
    assert!(stats.range_stats.ai_additions > 0);
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

#[test]
fn test_range_authorship_counts_known_human_additions() {
    // Range stats hardcoded human additions to zero, so known-human lines in
    // a range surfaced as unknown additions instead of human additions.
    use crate::repos::test_file::ExpectedLineExt;

    let repo = TestRepo::new();

    std::fs::write(repo.path().join("test.txt"), "Human Line 1\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Initial human commit").unwrap();
    let mut file = repo.filename("test.txt");
    file.assert_committed_lines(crate::lines!["Human Line 1".human()]);
    let first_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    std::fs::write(
        repo.path().join("test.txt"),
        "Human Line 1\nAI Line 2\nAI Line 3\n",
    )
    .unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();
    repo.stage_all_and_commit("AI adds 2 lines").unwrap();
    file.assert_committed_lines(crate::lines![
        "Human Line 1".human(),
        "AI Line 2".ai(),
        "AI Line 3".ai(),
    ]);

    std::fs::write(
        repo.path().join("test.txt"),
        "Human Line 1\nAI Line 2\nAI Line 3\nHuman Line 4\nHuman Line 5\n",
    )
    .unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Human adds 2 more lines")
        .unwrap();
    file.assert_committed_lines(crate::lines![
        "Human Line 1".human(),
        "AI Line 2".ai(),
        "AI Line 3".ai(),
        "Human Line 4".human(),
        "Human Line 5".human(),
    ]);
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

    let stats = range_authorship(commit_range, false, &[], None).unwrap();

    // The two known-human lines added inside the range count as human
    // additions, not unknown additions.
    assert!(
        stats.range_stats.human_additions > 0,
        "known-human lines in the range should be counted as human additions, got {:?}",
        stats.range_stats
    );
    assert!(
        stats.range_stats.unknown_additions < stats.range_stats.human_additions,
        "unknown_additions should not absorb the known-human lines, got {:?}",
        stats.range_stats
    );
    assert_eq!(stats.authorship_stats.total_commits, 2);
    assert_eq!(stats.authorship_stats.commits_with_authorship, 2);
}
