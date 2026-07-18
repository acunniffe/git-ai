use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log::LineRange;
use git_ai::authorship::authorship_log::PromptRecord;
use git_ai::authorship::authorship_log_serialization::AttestationEntry;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::authorship::authorship_log_serialization::FileAttestation;
use git_ai::authorship::authorship_log_serialization::generate_short_hash;
use git_ai::authorship::stats::*;
use git_ai::authorship::working_log::AgentId;
use git_ai::git::repository::find_repository_in_path;
use std::collections::BTreeMap;
use std::collections::HashMap;

#[test]
fn test_stats_for_simple_ai_commit() {
    let repo = TestRepo::new();

    std::fs::write(repo.path().join("test.txt"), "Line1\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();

    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    repo.stage_all_and_commit("Initial commit").unwrap();

    // AI adds 2 lines
    std::fs::write(repo.path().join("test.txt"), "Line1\nLine 2\nLine 3\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();

    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();

    repo.stage_all_and_commit("AI adds lines").unwrap();

    // Get the commit SHA for the AI commit
    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Test our stats function
    let stats = stats_for_commit_stats(&gitai_repo, &head_sha, &[]).unwrap();

    // Verify the stats
    assert_eq!(
        stats.human_additions, 0,
        "No human additions in AI-only commit"
    );
    assert_eq!(stats.ai_additions, 2, "AI added 2 lines");
    assert_eq!(stats.ai_accepted, 2, "AI lines were accepted");
    assert_eq!(
        stats.git_diff_added_lines, 2,
        "Git diff shows 2 added lines"
    );
    assert_eq!(
        stats.git_diff_deleted_lines, 0,
        "Git diff shows 0 deleted lines"
    );
}

#[test]
fn test_stats_for_mixed_commit() {
    let repo = TestRepo::new();

    std::fs::write(repo.path().join("test.txt"), "Base line\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();

    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    repo.stage_all_and_commit("Initial commit").unwrap();

    // AI adds lines
    std::fs::write(
        repo.path().join("test.txt"),
        "Base line\nAI line 1\nAI line 2\n",
    )
    .unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();

    // Human adds lines
    std::fs::write(
        repo.path().join("test.txt"),
        "Base line\nAI line 1\nAI line 2\nHuman line 1\nHuman line 2\n",
    )
    .unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    repo.stage_all_and_commit("Mixed commit").unwrap();

    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let stats = stats_for_commit_stats(&gitai_repo, &head_sha, &[]).unwrap();

    // Verify the stats
    // trigger_checkpoint_with_author produces KnownHuman checkpoints (post Task 9),
    // so human-written lines have h_-prefixed attestation entries → human_additions.
    assert_eq!(stats.human_additions, 2, "Human added 2 lines");
    assert_eq!(stats.ai_additions, 2, "AI added 2 lines");
    assert_eq!(stats.ai_accepted, 2, "AI lines were accepted");
    assert_eq!(
        stats.git_diff_added_lines, 4,
        "Git diff shows 4 added lines total"
    );
    assert_eq!(
        stats.git_diff_deleted_lines, 0,
        "Git diff shows 0 deleted lines"
    );
}

#[test]
fn test_stats_for_initial_commit() {
    let repo = TestRepo::new();

    std::fs::write(repo.path().join("test.txt"), "Line1\nLine2\nLine3\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();

    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    repo.stage_all_and_commit("Initial commit").unwrap();

    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let stats = stats_for_commit_stats(&gitai_repo, &head_sha, &[]).unwrap();

    // KnownHuman checkpoints record h_<hash> attributions for all human-edited lines,
    // so they appear as human_additions (not unknown) even on pure-human commits.
    assert_eq!(
        stats.human_additions, 3,
        "All 3 lines should be KnownHuman-attested human_additions"
    );
    assert_eq!(
        stats.unknown_additions, 0,
        "No unattested lines in a KnownHuman-checkpointed commit"
    );
    assert_eq!(stats.ai_additions, 0, "No AI additions in initial commit");
    assert_eq!(stats.ai_accepted, 0, "No AI lines to accept");
    assert_eq!(
        stats.git_diff_added_lines, 3,
        "Git diff shows 3 added lines (initial commit)"
    );
    assert_eq!(
        stats.git_diff_deleted_lines, 0,
        "Git diff shows 0 deleted lines"
    );
}

#[test]
fn test_stats_ignores_single_lockfile() {
    let repo = TestRepo::new();

    // Initial commit
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    repo.git(&["add", "src/main.rs"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "src/main.rs"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Commit that adds source code and a large lockfile
    std::fs::write(
        repo.path().join("src/main.rs"),
        "fn main() {}\nfn helper() {}\n",
    )
    .unwrap();
    repo.git(&["add", "src/main.rs"]).unwrap();
    std::fs::write(repo.path().join("Cargo.lock"), "# lockfile\n".repeat(1000)).unwrap();
    repo.git(&["add", "Cargo.lock"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/main.rs"])
        .unwrap();
    repo.stage_all_and_commit("Add helper and deps").unwrap();

    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Test WITHOUT ignore - should count lockfile
    let stats_with_lockfile = stats_for_commit_stats(&gitai_repo, &head_sha, &[]).unwrap();
    assert_eq!(stats_with_lockfile.git_diff_added_lines, 1001); // 1 source + 1000 lockfile

    // Test WITH ignore - should exclude lockfile
    let ignore_patterns = vec!["Cargo.lock".to_string()];
    let stats_without_lockfile =
        stats_for_commit_stats(&gitai_repo, &head_sha, &ignore_patterns).unwrap();
    assert_eq!(stats_without_lockfile.git_diff_added_lines, 1); // Only 1 source line
    assert_eq!(stats_without_lockfile.ai_additions, 1);
}

#[test]
fn test_stats_ignores_multiple_lockfiles() {
    let repo = TestRepo::new();

    // Initial commit
    std::fs::write(repo.path().join("README.md"), "# Project\n").unwrap();
    repo.git(&["add", "README.md"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "README.md"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Commit that updates multiple lockfiles and one source file
    std::fs::write(repo.path().join("README.md"), "# Project\n## New\n").unwrap();
    repo.git(&["add", "README.md"]).unwrap();
    std::fs::write(repo.path().join("Cargo.lock"), "# cargo\n".repeat(500)).unwrap();
    repo.git(&["add", "Cargo.lock"]).unwrap();
    std::fs::write(repo.path().join("package-lock.json"), "{}\n".repeat(500)).unwrap();
    repo.git(&["add", "package-lock.json"]).unwrap();
    std::fs::write(repo.path().join("yarn.lock"), "# yarn\n".repeat(500)).unwrap();
    repo.git(&["add", "yarn.lock"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "README.md"])
        .unwrap();
    repo.stage_all_and_commit("Update deps").unwrap();

    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Test WITHOUT ignore - counts all files (1501 lines)
    let stats_all = stats_for_commit_stats(&gitai_repo, &head_sha, &[]).unwrap();
    assert_eq!(stats_all.git_diff_added_lines, 1501);

    // Test WITH ignore - only counts README (1 line)
    let ignore_patterns = vec![
        "Cargo.lock".to_string(),
        "package-lock.json".to_string(),
        "yarn.lock".to_string(),
    ];
    let stats_filtered = stats_for_commit_stats(&gitai_repo, &head_sha, &ignore_patterns).unwrap();
    assert_eq!(stats_filtered.git_diff_added_lines, 1);
    // KnownHuman checkpoints record h_<hash> attributions, so the README line is human_additions.
    assert_eq!(stats_filtered.human_additions, 1);
    assert_eq!(stats_filtered.unknown_additions, 0);
}

#[test]
fn test_stats_with_lockfile_only_commit() {
    let repo = TestRepo::new();

    // Initial commit
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/lib.rs"), "pub fn foo() {}\n").unwrap();
    repo.git(&["add", "src/lib.rs"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "src/lib.rs"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Commit that ONLY updates lockfiles (common during dependency updates)
    std::fs::write(repo.path().join("Cargo.lock"), "# updated\n".repeat(2000)).unwrap();
    repo.git(&["add", "Cargo.lock"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "Cargo.lock"])
        .unwrap();
    repo.stage_all_and_commit("Update dependencies").unwrap();

    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Test WITHOUT ignore - shows 2000 lines
    let stats_with = stats_for_commit_stats(&gitai_repo, &head_sha, &[]).unwrap();
    assert_eq!(stats_with.git_diff_added_lines, 2000);

    // Test WITH ignore - shows 0 lines (lockfile-only commit)
    let ignore_patterns = vec!["Cargo.lock".to_string()];
    let stats_without = stats_for_commit_stats(&gitai_repo, &head_sha, &ignore_patterns).unwrap();
    assert_eq!(stats_without.git_diff_added_lines, 0);
    assert_eq!(stats_without.ai_additions, 0);
    assert_eq!(stats_without.human_additions, 0);
}

#[test]
fn test_stats_empty_ignore_patterns() {
    let repo = TestRepo::new();

    // Initial commit
    std::fs::write(repo.path().join("test.txt"), "Line1\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Add lines
    std::fs::write(repo.path().join("test.txt"), "Line1\nLine2\nLine3\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();
    repo.stage_all_and_commit("Add lines").unwrap();

    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Test with empty patterns - should behave same as no filtering
    let stats = stats_for_commit_stats(&gitai_repo, &head_sha, &[]).unwrap();
    assert_eq!(stats.git_diff_added_lines, 2);
    assert_eq!(stats.ai_additions, 2);
}

#[test]
fn test_stats_with_glob_patterns() {
    let repo = TestRepo::new();

    // Initial commit
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/lib.rs"), "pub fn foo() {}\n").unwrap();
    repo.git(&["add", "src/lib.rs"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "src/lib.rs"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Commit with source code + lockfiles + generated files
    std::fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn foo() {}\npub fn bar() {}\n",
    )
    .unwrap();
    repo.git(&["add", "src/lib.rs"]).unwrap();
    std::fs::write(repo.path().join("Cargo.lock"), "# lock\n".repeat(1000)).unwrap();
    repo.git(&["add", "Cargo.lock"]).unwrap();
    std::fs::write(repo.path().join("package-lock.json"), "{}\n".repeat(500)).unwrap();
    repo.git(&["add", "package-lock.json"]).unwrap();
    std::fs::write(
        repo.path().join("api.generated.ts"),
        "// generated\n".repeat(300),
    )
    .unwrap();
    repo.git(&["add", "api.generated.ts"]).unwrap();
    std::fs::write(
        repo.path().join("schema.generated.js"),
        "// schema\n".repeat(200),
    )
    .unwrap();
    repo.git(&["add", "schema.generated.js"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/lib.rs"])
        .unwrap();
    repo.stage_all_and_commit("Add code").unwrap();

    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Test WITHOUT ignore - all files included (2001 lines)
    let stats_all = stats_for_commit_stats(&gitai_repo, &head_sha, &[]).unwrap();
    assert_eq!(stats_all.git_diff_added_lines, 2001);

    // Test WITH glob patterns - only source code (1 line)
    let glob_patterns = vec![
        "*.lock".to_string(),        // Matches Cargo.lock
        "*lock.json".to_string(),    // Matches package-lock.json
        "*.generated.*".to_string(), // Matches *.generated.ts, *.generated.js
    ];
    let stats_filtered = stats_for_commit_stats(&gitai_repo, &head_sha, &glob_patterns).unwrap();
    assert_eq!(stats_filtered.git_diff_added_lines, 1);
    assert_eq!(stats_filtered.ai_additions, 1);
}

#[test]
fn test_accepted_lines_no_authorship_log() {
    let added_lines: HashMap<String, Vec<u32>> = HashMap::new();
    let (accepted, known_human, per_tool) =
        accepted_lines_from_attestations(None, &added_lines, false);
    assert_eq!(accepted, 0);
    assert_eq!(known_human, 0);
    assert!(per_tool.is_empty());
}

#[test]
fn test_accepted_lines_merge_commit() {
    // Even with a real authorship log, merge commits should short-circuit to (0, empty)
    let mut log = AuthorshipLog::new();
    let agent_id = AgentId {
        tool: "cursor".to_string(),
        id: "session_1".to_string(),
        model: "claude-3-sonnet".to_string(),
    };
    let hash = generate_short_hash(&agent_id.id, &agent_id.tool);
    log.metadata.prompts.insert(
        hash.clone(),
        PromptRecord {
            agent_id,
            human_author: None,
            total_additions: 5,
            total_deletions: 0,
            accepted_lines: 5,
            overriden_lines: 0,
            custom_attributes: None,
            messages_url: None,
        },
    );

    let mut file_att = FileAttestation::new("foo.rs".to_string());
    file_att.add_entry(AttestationEntry::new(hash, vec![LineRange::Range(1, 3)]));
    log.attestations.push(file_att);

    let mut added_lines: HashMap<String, Vec<u32>> = HashMap::new();
    added_lines.insert("foo.rs".to_string(), vec![1, 2, 3]);

    let (accepted, known_human, per_tool) =
        accepted_lines_from_attestations(Some(&log), &added_lines, true);
    assert_eq!(accepted, 0);
    assert_eq!(known_human, 0);
    assert!(per_tool.is_empty());
}

#[test]
fn test_accepted_lines_no_matching_files() {
    let mut log = AuthorshipLog::new();
    let agent_id = AgentId {
        tool: "cursor".to_string(),
        id: "session_2".to_string(),
        model: "claude-3-sonnet".to_string(),
    };
    let hash = generate_short_hash(&agent_id.id, &agent_id.tool);
    log.metadata.prompts.insert(
        hash.clone(),
        PromptRecord {
            agent_id,
            human_author: None,
            total_additions: 3,
            total_deletions: 0,
            accepted_lines: 3,
            overriden_lines: 0,
            custom_attributes: None,
            messages_url: None,
        },
    );

    let mut file_att = FileAttestation::new("foo.rs".to_string());
    file_att.add_entry(AttestationEntry::new(hash, vec![LineRange::Range(1, 3)]));
    log.attestations.push(file_att);

    // added_lines has "bar.rs" but NOT "foo.rs"
    let mut added_lines: HashMap<String, Vec<u32>> = HashMap::new();
    added_lines.insert("bar.rs".to_string(), vec![1, 2, 3]);

    let (accepted, known_human, per_tool) =
        accepted_lines_from_attestations(Some(&log), &added_lines, false);
    assert_eq!(accepted, 0);
    assert_eq!(known_human, 0);
    assert!(per_tool.is_empty());
}

#[test]
fn test_accepted_lines_basic_match() {
    let mut log = AuthorshipLog::new();
    let agent_id = AgentId {
        tool: "cursor".to_string(),
        id: "session_3".to_string(),
        model: "claude-3-sonnet".to_string(),
    };
    let hash = generate_short_hash(&agent_id.id, &agent_id.tool);
    log.metadata.prompts.insert(
        hash.clone(),
        PromptRecord {
            agent_id,
            human_author: None,
            total_additions: 3,
            total_deletions: 0,
            accepted_lines: 3,
            overriden_lines: 0,
            custom_attributes: None,
            messages_url: None,
        },
    );

    let mut file_att = FileAttestation::new("foo.rs".to_string());
    file_att.add_entry(AttestationEntry::new(
        hash.clone(),
        vec![LineRange::Range(1, 3)],
    ));
    log.attestations.push(file_att);

    let mut added_lines: HashMap<String, Vec<u32>> = HashMap::new();
    added_lines.insert("foo.rs".to_string(), vec![1, 2, 3]);

    let (accepted, known_human, per_tool) =
        accepted_lines_from_attestations(Some(&log), &added_lines, false);
    assert_eq!(accepted, 3);
    assert_eq!(known_human, 0);

    // Verify per-tool breakdown contains the right key
    let expected_key = "cursor::claude-3-sonnet".to_string();
    assert_eq!(per_tool.get(&expected_key), Some(&3));
}

// --- line_range_overlap_len tests ---

#[test]
fn test_overlap_single_hit() {
    let count = line_range_overlap_len(&LineRange::Single(5), &[3, 5, 7]);
    assert_eq!(count, 1);
}

#[test]
fn test_overlap_single_miss() {
    let count = line_range_overlap_len(&LineRange::Single(4), &[3, 5, 7]);
    assert_eq!(count, 0);
}

#[test]
fn test_overlap_range_full() {
    let count = line_range_overlap_len(&LineRange::Range(3, 7), &[3, 4, 5, 6, 7]);
    assert_eq!(count, 5);
}

#[test]
fn test_overlap_range_partial() {
    // Range [4, 8] intersected with [3, 5, 7, 9]: only 5 and 7 are in range
    let count = line_range_overlap_len(&LineRange::Range(4, 8), &[3, 5, 7, 9]);
    assert_eq!(count, 2);
}

#[test]
fn test_overlap_range_miss() {
    let count = line_range_overlap_len(&LineRange::Range(10, 20), &[1, 2, 3]);
    assert_eq!(count, 0);
}

#[test]
fn test_overlap_range_empty_added() {
    let count = line_range_overlap_len(&LineRange::Range(1, 10), &[]);
    assert_eq!(count, 0);
}

#[test]
fn test_stats_for_merge_commit_skips_ai_acceptance() {
    let repo = TestRepo::new();

    std::fs::write(repo.path().join("test.txt"), "base\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let default_branch = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    std::fs::write(repo.path().join("test.txt"), "base\nfeature line\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();
    repo.stage_all_and_commit("Feature change").unwrap();

    repo.git(&["checkout", &default_branch]).unwrap();
    std::fs::write(repo.path().join("main.txt"), "main line\n").unwrap();
    repo.git(&["add", "main.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.stage_all_and_commit("Main change").unwrap();

    repo.git(&["merge", "feature", "-m", "Merge feature"])
        .unwrap();

    let merge_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let stats = stats_for_commit_stats(&gitai_repo, &merge_sha, &[]).unwrap();

    assert_eq!(stats.git_diff_added_lines, 0);
    assert_eq!(stats.git_diff_deleted_lines, 0);
    assert_eq!(stats.ai_accepted, 0);
    assert_eq!(stats.ai_additions, 0);
    assert_eq!(stats.human_additions, 0);
    assert_eq!(stats.unknown_additions, 0);
}

#[test]
fn test_stats_command_nonexistent_commit() {
    let repo = TestRepo::new();

    std::fs::write(repo.path().join("test.txt"), "content\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.stage_all_and_commit("Commit").unwrap();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Non-existent SHA should error
    let result = stats_command(
        &gitai_repo,
        Some("0000000000000000000000000000000000000000"),
        false,
        &[],
    );
    assert!(result.is_err());
}

#[test]
fn test_stats_command_with_json_output() {
    let repo = TestRepo::new();

    std::fs::write(repo.path().join("test.txt"), "content\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Commit").unwrap();

    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Should succeed with json output
    let result = stats_command(&gitai_repo, Some(&head_sha), true, &[]);
    assert!(result.is_ok());
}

#[test]
fn test_stats_command_default_to_head() {
    let repo = TestRepo::new();

    std::fs::write(repo.path().join("test.txt"), "content\n").unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Commit").unwrap();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // No SHA provided should default to HEAD
    let result = stats_command(&gitai_repo, None, false, &[]);
    assert!(result.is_ok());
}

#[test]
fn test_get_git_diff_stats_binary_files() {
    let repo = TestRepo::new();

    // Create initial commit
    std::fs::write(repo.path().join("text.txt"), "text\n").unwrap();
    repo.git(&["add", "text.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "text.txt"])
        .unwrap();
    repo.stage_all_and_commit("Initial").unwrap();

    // Add binary file (git will detect it as binary if it contains null bytes)
    let binary_content = vec![0u8, 1u8, 2u8, 3u8, 255u8];
    std::fs::write(repo.path().join("binary.bin"), &binary_content).unwrap();
    repo.git(&["add", "binary.bin"]).unwrap();

    repo.stage_all_and_commit("Add binary").unwrap();

    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Binary files should be handled (shown as "-" in numstat)
    let result = get_git_diff_stats(&gitai_repo, &head_sha, &[]);
    assert!(result.is_ok());
}

#[test]
fn test_stats_from_authorship_log_no_log() {
    let stats = stats_from_authorship_log(None, 10, 5, 3, 0, &BTreeMap::new());

    assert_eq!(stats.git_diff_added_lines, 10);
    assert_eq!(stats.git_diff_deleted_lines, 5);
    assert_eq!(stats.ai_accepted, 3);
    assert_eq!(stats.ai_additions, 3); // ai_accepted when no mixed
    assert_eq!(stats.human_additions, 0); // no known-human attestations passed
    assert_eq!(stats.unknown_additions, 7); // 10 - 3 (unattested lines)
}

#[test]
fn test_stats_from_authorship_log_mixed_cap() {
    // Test that mixed_additions is capped to remaining added lines
    let mut log = AuthorshipLog::new();
    let agent_id = AgentId {
        tool: "cursor".to_string(),
        id: "session".to_string(),
        model: "claude-3-sonnet".to_string(),
    };
    let hash = generate_short_hash(&agent_id.id, &agent_id.tool);

    // Prompt with 100 overridden lines (way more than the diff)
    log.metadata.prompts.insert(
        hash,
        PromptRecord {
            agent_id,
            human_author: None,
            total_additions: 50,
            total_deletions: 0,
            accepted_lines: 0,
            overriden_lines: 100, // Unrealistically high
            custom_attributes: None,
            messages_url: None,
        },
    );

    // Only 10 lines added, 5 accepted by AI
    let stats = stats_from_authorship_log(Some(&log), 10, 0, 5, 0, &BTreeMap::new());

    assert_eq!(stats.ai_additions, 5); // ai_accepted
    assert_eq!(stats.human_additions, 0); // no known-human attestations passed
}

#[test]
fn test_line_range_overlap_edge_cases() {
    // Empty added_lines
    assert_eq!(line_range_overlap_len(&LineRange::Single(5), &[]), 0);
    assert_eq!(line_range_overlap_len(&LineRange::Range(1, 10), &[]), 0);

    // Range with start == end
    assert_eq!(line_range_overlap_len(&LineRange::Range(5, 5), &[5]), 1);
    assert_eq!(line_range_overlap_len(&LineRange::Range(5, 5), &[4, 6]), 0);

    // Range before all lines
    assert_eq!(
        line_range_overlap_len(&LineRange::Range(1, 2), &[10, 20, 30]),
        0
    );

    // Range after all lines
    assert_eq!(
        line_range_overlap_len(&LineRange::Range(50, 60), &[10, 20, 30]),
        0
    );

    // Range partially overlapping
    assert_eq!(
        line_range_overlap_len(&LineRange::Range(5, 15), &[1, 3, 10, 12, 20]),
        2
    );
}
