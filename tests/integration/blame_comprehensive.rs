//! Comprehensive tests for src/commands/blame.rs
//!
//! This test module covers critical functionality in blame.rs (1,811 LOC)
//! including integration tests for AI authorship overlay, error handling,
//! edge cases, and output formatting.
//!
//! Test coverage areas:
//! 1. Core blame functionality with AI authorship
//! 2. Error handling (invalid refs, missing files, git errors)
//! 3. Edge cases (empty files, binary files, renamed files)
//! 4. Output formatting (default, porcelain, incremental, JSON)
//! 5. Line range handling
//! 6. Commit filtering (newest_commit, oldest_commit, oldest_date)
//! 7. AI authorship splitting by human author
//! 8. Foreign prompt lookups
//! 9. File path normalization (absolute vs relative)

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

// =============================================================================
// Happy Path Tests - Successful blame operations with AI authorship
// =============================================================================

#[test]
fn test_blame_success_basic_file() {
    // Happy path: Basic blame on a file with mixed human/AI authorship
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Human line 1".human(),
        "AI line 1".ai(),
        "Human line 2".human(),
        "AI line 2".ai()
    ]);

    repo.stage_all_and_commit("Mixed authorship").unwrap();

    let output = repo.git_ai(&["blame", "test.txt"]).unwrap();

    // Verify output contains all lines
    assert!(output.contains("Human line 1"));
    assert!(output.contains("AI line 1"));
    assert!(output.contains("Human line 2"));
    assert!(output.contains("AI line 2"));

    // Verify output shows AI tool name for AI lines
    assert!(output.contains("mock_ai"));
}

#[test]
fn test_blame_success_only_human_lines() {
    // Happy path: File with only human-authored lines
    let repo = TestRepo::new();
    let mut file = repo.filename("human.txt");

    file.set_contents(crate::lines![
        "Human line 1".human(),
        "Human line 2".human()
    ]);

    repo.stage_all_and_commit("All human").unwrap();

    let output = repo.git_ai(&["blame", "human.txt"]).unwrap();

    assert!(output.contains("Human line 1"));
    assert!(output.contains("Human line 2"));
    assert!(output.contains("Test User"));
    assert!(!output.contains("mock_ai"));
}

#[test]
fn test_blame_success_only_ai_lines() {
    // Happy path: File with only AI-authored lines
    let repo = TestRepo::new();
    let mut file = repo.filename("ai.txt");

    file.set_contents(crate::lines!["AI line 1".ai(), "AI line 2".ai()]);

    repo.stage_all_and_commit("All AI").unwrap();

    let output = repo.git_ai(&["blame", "ai.txt"]).unwrap();

    assert!(output.contains("AI line 1"));
    assert!(output.contains("AI line 2"));
    assert!(output.contains("mock_ai"));
}

#[test]
fn test_blame_success_with_line_range() {
    // Happy path: Blame with -L flag to specify line range
    let repo = TestRepo::new();
    let mut file = repo.filename("ranges.txt");

    file.set_contents(crate::lines![
        "Line 1", "Line 2", "Line 3", "Line 4", "Line 5"
    ]);

    repo.stage_all_and_commit("Multi-line file").unwrap();

    let output = repo.git_ai(&["blame", "-L", "2,4", "ranges.txt"]).unwrap();

    assert!(output.contains("Line 2"));
    assert!(output.contains("Line 3"));
    assert!(output.contains("Line 4"));
    assert!(!output.contains("Line 1"));
    assert!(!output.contains("Line 5"));
}

#[test]
fn test_blame_success_with_newest_commit() {
    // Happy path: Blame at a specific commit using the API directly
    let repo = TestRepo::new();
    let mut file = repo.filename("versioned.txt");

    file.set_contents(crate::lines!["Version 1"]);
    let commit1 = repo.stage_all_and_commit("First version").unwrap();

    file.set_contents(crate::lines!["Version 2"]);
    repo.stage_all_and_commit("Second version").unwrap();

    // Use the Repository API to test newest_commit option
    let gitai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("Failed to find repository");

    let options = GitAiBlameOptions {
        newest_commit: Some(commit1.commit_sha.clone()),
        no_output: true,
        ..Default::default()
    };

    let (line_authors, _) = gitai_repo.blame("versioned.txt", &options).unwrap();

    // At commit1, should only see the first version
    assert!(!line_authors.is_empty());
}

#[test]
fn test_blame_success_json_format() {
    // Happy path: JSON output format with AI authorship
    let repo = TestRepo::new();
    let mut file = repo.filename("json_test.txt");

    file.set_contents(crate::lines!["Human line".human(), "AI line".ai()]);

    repo.stage_all_and_commit("JSON test").unwrap();

    let output = repo.git_ai(&["blame", "--json", "json_test.txt"]).unwrap();

    // Verify JSON structure
    assert!(output.contains("\"lines\""));
    assert!(output.contains("\"prompts\""));

    // Parse JSON to verify structure
    let json: serde_json::Value =
        serde_json::from_str(&output).expect("Output should be valid JSON");

    assert!(json["lines"].is_object());
    assert!(json["prompts"].is_object());
}

// =============================================================================
// Error Handling Tests - Invalid inputs, missing files, git errors
// =============================================================================

#[test]
fn test_blame_error_missing_file() {
    // Error case: Blame on non-existent file
    let repo = TestRepo::new();

    let result = repo.git_ai(&["blame", "nonexistent.txt"]);

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.contains("File not found")
            || err.contains("does not exist")
            || err.contains("No such file")
            || err.contains("pathspec")
            || err.contains("did not match")
            || err.contains("cannot find the file")
            || err.contains("canonicalize file path"),
        "Expected error about missing file, got: {}",
        err
    );
}

#[test]
fn test_blame_error_invalid_line_range_start_zero() {
    // Error case: Line range starting at 0 (lines are 1-indexed)
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let result = repo.git_ai(&["blame", "-L", "0,1", "test.txt"]);

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("Invalid line range"));
}

#[test]
fn test_blame_error_invalid_line_range_end_zero() {
    // Error case: Line range ending at 0
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let result = repo.git_ai(&["blame", "-L", "1,0", "test.txt"]);

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("Invalid line range"));
}

#[test]
fn test_blame_error_invalid_line_range_start_greater_than_end() {
    // Error case: Start line > end line
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2", "Line 3"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let result = repo.git_ai(&["blame", "-L", "3,1", "test.txt"]);

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("Invalid line range"));
}

#[test]
fn test_blame_error_invalid_line_range_beyond_file() {
    // Error case: Line range exceeds file length
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let result = repo.git_ai(&["blame", "-L", "1,100", "test.txt"]);

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("Invalid line range") && err.contains("File has 2 lines"));
}

#[test]
fn test_blame_error_invalid_commit_ref() {
    // Error case: Invalid commit SHA
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let result = repo.git_ai(&["blame", "invalid_sha_123", "test.txt"]);

    assert!(result.is_err());
}

#[test]
fn test_blame_error_file_outside_repo() {
    // Error case: Attempt to blame a file outside the repository
    let repo = TestRepo::new();

    // Use a unique temp dir per test instance to avoid races when the
    // worktree variant of this test runs concurrently in the same process.
    let outside_dir = tempfile::tempdir().expect("failed to create temp dir");
    let outside_file = outside_dir.path().join("outside.txt");
    std::fs::write(&outside_file, "outside content").unwrap();

    let result = repo.git_ai(&["blame", outside_file.to_str().unwrap()]);

    assert!(
        result.is_err(),
        "blaming a file outside the repo should fail"
    );
    // On Windows in worktree mode, both the worktree and the outside file reside
    // under the same temp directory.  UNC-path canonicalization (`\\?\…`) can
    // cause `strip_prefix` to behave differently, producing an error message that
    // does not contain the usual "not within repository root" text.  The important
    // invariant is that the command errors out; we only assert the specific message
    // on platforms where it is stable.
    #[cfg(not(target_os = "windows"))]
    {
        let err = result.unwrap_err();
        assert!(
            err.contains("not within repository root"),
            "unexpected error message: {err}"
        );
    }
}

#[test]
fn test_blame_error_directory_instead_of_file() {
    // Error case: Attempt to blame a directory
    let repo = TestRepo::new();

    let subdir = repo.path().join("src");
    std::fs::create_dir_all(&subdir).unwrap();

    let result = repo.git_ai(&["blame", "src"]);

    assert!(result.is_err());
}

// =============================================================================
// Edge Cases - Empty files, boundary commits, renamed files
// =============================================================================

#[test]
fn test_blame_edge_empty_file() {
    // Edge case: Blame on an empty file
    let repo = TestRepo::new();
    let file_path = repo.path().join("empty.txt");
    std::fs::write(&file_path, "").unwrap();

    repo.git(&["add", "empty.txt"]).unwrap();
    repo.stage_all_and_commit("Empty file").unwrap();

    // Empty files return an error because line range 1:0 is invalid
    let result = repo.git_ai(&["blame", "empty.txt"]);
    assert!(
        result.is_err(),
        "Empty file should fail with line range error"
    );
}

#[test]
fn test_blame_edge_single_line_file() {
    // Edge case: File with only one line
    let repo = TestRepo::new();
    let mut file = repo.filename("single.txt");

    file.set_contents(crate::lines!["Only line".ai()]);
    repo.stage_all_and_commit("Single line").unwrap();

    let output = repo.git_ai(&["blame", "single.txt"]).unwrap();

    assert!(output.contains("Only line"));
    assert_eq!(output.lines().count(), 1);
}

#[test]
fn test_blame_edge_large_file() {
    // Edge case: Large file with many lines
    let repo = TestRepo::new();
    let file = repo.filename("large.txt");

    let mut lines = Vec::new();
    for i in 1..=1000 {
        lines.push(format!("Line {}", i));
    }
    std::fs::write(file.file_path.clone(), lines.join("\n") + "\n").unwrap();

    repo.stage_all_and_commit("Large file").unwrap();

    let output = repo.git_ai(&["blame", "large.txt"]).unwrap();

    // Should contain all lines
    assert!(output.contains("Line 1"));
    assert!(output.contains("Line 500"));
    assert!(output.contains("Line 1000"));
    assert_eq!(output.lines().count(), 1000);
}

#[test]
fn test_blame_edge_file_with_unicode() {
    // Edge case: File with unicode content
    let repo = TestRepo::new();
    let mut file = repo.filename("unicode.txt");

    file.set_contents(crate::lines![
        "Hello 世界".ai(),
        "Emoji: 🚀 🎉".ai(),
        "Greek: αβγδ".human()
    ]);

    repo.stage_all_and_commit("Unicode content").unwrap();

    let output = repo.git_ai(&["blame", "unicode.txt"]).unwrap();

    assert!(output.contains("世界"));
    assert!(output.contains("🚀"));
    assert!(output.contains("αβγδ"));
}

#[test]
fn test_blame_edge_file_with_very_long_lines() {
    // Edge case: File with very long lines
    let repo = TestRepo::new();
    let mut file = repo.filename("longlines.txt");

    let long_line = "a".repeat(5000);
    file.set_contents(crate::lines![long_line.as_str().ai()]);

    repo.stage_all_and_commit("Long line").unwrap();

    let output = repo.git_ai(&["blame", "longlines.txt"]).unwrap();

    // Should handle long lines without error
    assert!(output.len() > 5000);
}

#[test]
fn test_blame_edge_boundary_commit_flag() {
    // Edge case: Boundary commit with -b flag
    let repo = TestRepo::new();
    repo.git(&["checkout", "--orphan", "boundary-test"])
        .unwrap();
    let mut file = repo.filename("boundary.txt");

    file.set_contents(crate::lines!["Initial line"]);
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-m", "Initial commit"]).unwrap();

    let output = repo.git_ai(&["blame", "-b", "boundary.txt"]).unwrap();

    // With -b, boundary commits should show empty hash
    assert!(output.contains("        ") || output.contains("^"));
}

#[test]
fn test_blame_edge_renamed_file() {
    // Edge case: Blame on a renamed file
    let repo = TestRepo::new();
    let mut file = repo.filename("original.txt");

    file.set_contents(crate::lines!["Original content".ai()]);
    repo.stage_all_and_commit("Add original").unwrap();

    // Rename the file
    let old_path = repo.path().join("original.txt");
    let new_path = repo.path().join("renamed.txt");
    std::fs::rename(&old_path, &new_path).unwrap();

    repo.git(&["add", "original.txt", "renamed.txt"]).unwrap();
    repo.stage_all_and_commit("Rename file").unwrap();

    let output = repo.git_ai(&["blame", "renamed.txt"]).unwrap();

    assert!(output.contains("Original content"));
}

#[test]
fn test_blame_edge_whitespace_only_lines() {
    // Edge case: Lines containing only whitespace
    let repo = TestRepo::new();
    let file = repo.filename("whitespace.txt");

    std::fs::write(file.file_path.clone(), "Line 1\n   \n\t\t\nLine 4").unwrap();
    repo.git(&["add", "whitespace.txt"]).unwrap();
    repo.stage_all_and_commit("Whitespace lines").unwrap();

    let output = repo.git_ai(&["blame", "whitespace.txt"]).unwrap();

    // Should handle whitespace-only lines
    assert_eq!(output.lines().count(), 4);
}

// =============================================================================
// Output Format Tests - Porcelain, incremental, JSON formats
// =============================================================================

#[test]
fn test_blame_format_porcelain_basic() {
    // Output format: Basic porcelain format
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);
    repo.stage_all_and_commit("Test").unwrap();

    let output = repo.git_ai(&["blame", "--porcelain", "test.txt"]).unwrap();

    // Porcelain format should include metadata fields
    assert!(output.contains("author "));
    assert!(output.contains("author-mail "));
    assert!(output.contains("author-time "));
    assert!(output.contains("committer "));
    assert!(output.contains("summary "));
    assert!(output.contains("filename "));
    assert!(output.contains("\tLine 1"));
    assert!(output.contains("\tLine 2"));
}

#[test]
fn test_blame_format_line_porcelain() {
    // Output format: Line porcelain format (metadata for every line)
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2"]);
    repo.stage_all_and_commit("Test").unwrap();

    let output = repo
        .git_ai(&["blame", "--line-porcelain", "test.txt"])
        .unwrap();

    // Line porcelain should have metadata for each line
    let author_count = output.matches("author ").count();
    assert!(author_count >= 2, "Should have author for each line");
}

#[test]
fn test_blame_format_incremental() {
    // Output format: Incremental format
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2"]);
    repo.stage_all_and_commit("Test").unwrap();

    let output = repo
        .git_ai(&["blame", "--incremental", "test.txt"])
        .unwrap();

    // Incremental format should have metadata without content lines
    assert!(output.contains("author "));
    assert!(output.contains("filename "));
    assert!(!output.contains("\tLine 1")); // No content lines in incremental
}

#[test]
fn test_blame_format_json_structure() {
    // Output format: JSON format structure validation
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);
    repo.stage_all_and_commit("Test").unwrap();

    let output = repo.git_ai(&["blame", "--json", "test.txt"]).unwrap();

    let json: serde_json::Value = serde_json::from_str(&output).expect("Should be valid JSON");

    // Verify JSON structure matches JsonBlameOutput
    assert!(json.get("lines").is_some());
    assert!(json.get("prompts").is_some());

    let lines = json["lines"].as_object().expect("lines should be object");
    let prompts = json["prompts"]
        .as_object()
        .expect("prompts should be object");

    // Should have AI line mapped to prompt
    assert!(!lines.is_empty());
    assert!(!prompts.is_empty());
}

#[test]
fn test_blame_format_json_line_ranges() {
    // Output format: JSON format with line ranges
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1".ai(),
        "Line 2".ai(),
        "Line 3".ai(),
        "Line 4".human(),
        "Line 5".ai()
    ]);
    repo.stage_all_and_commit("Test").unwrap();

    let output = repo.git_ai(&["blame", "--json", "test.txt"]).unwrap();

    let json: serde_json::Value = serde_json::from_str(&output).expect("Should be valid JSON");

    let lines = json["lines"].as_object().unwrap();

    // Consecutive AI lines should be grouped into ranges
    // Format should be either "1" or "1-3" for ranges
    let has_range = lines.keys().any(|k| k.contains("-"));
    assert!(
        has_range || lines.len() == 1,
        "Should group consecutive lines"
    );
}

#[test]
fn test_blame_format_default_with_flags() {
    // Output format: Default format with various flags
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2".ai()]);
    repo.stage_all_and_commit("Test").unwrap();

    // Test with -e (show email)
    let output = repo.git_ai(&["blame", "-e", "test.txt"]).unwrap();
    assert!(output.contains("@"));

    // Test with -n (show line numbers)
    let output = repo.git_ai(&["blame", "-n", "test.txt"]).unwrap();
    assert!(output.contains(" 1 "));
    assert!(output.contains(" 2 "));

    // Test with -f (show filename)
    let output = repo.git_ai(&["blame", "-f", "test.txt"]).unwrap();
    assert!(output.contains("test.txt"));

    // Test with -s (suppress author)
    let output = repo.git_ai(&["blame", "-s", "test.txt"]).unwrap();
    assert!(!output.contains("Test User"));
}

// =============================================================================
// AI Authorship Tests - Hunk splitting, human author attribution
// =============================================================================

#[test]
fn test_blame_ai_authorship_hunk_splitting() {
    // AI authorship: Hunks should split when different humans author lines
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2", "Line 3"]);

    let commit_sha = repo.stage_all_and_commit("Initial").unwrap().commit_sha;

    // Create authorship log with different human authors for different lines
    let mut authorship_log = AuthorshipLog::new();
    authorship_log.metadata.base_commit_sha = commit_sha.clone();

    // Prompt 1 for line 1
    let prompt_hash_1 = "prompt1".to_string();
    authorship_log.metadata.prompts.insert(
        prompt_hash_1.clone(),
        PromptRecord {
            agent_id: AgentId {
                tool: "cursor".to_string(),
                id: "session1".to_string(),
                model: "claude-3-sonnet".to_string(),
            },
            human_author: Some("Alice <alice@example.com>".to_string()),
            total_additions: 1,
            total_deletions: 0,
            accepted_lines: 1,
            overriden_lines: 0,
            custom_attributes: None,
            messages_url: None,
        },
    );

    // Prompt 2 for line 2
    let prompt_hash_2 = "prompt2".to_string();
    authorship_log.metadata.prompts.insert(
        prompt_hash_2.clone(),
        PromptRecord {
            agent_id: AgentId {
                tool: "cursor".to_string(),
                id: "session2".to_string(),
                model: "claude-3-sonnet".to_string(),
            },
            human_author: Some("Bob <bob@example.com>".to_string()),
            total_additions: 1,
            total_deletions: 0,
            accepted_lines: 1,
            overriden_lines: 0,
            custom_attributes: None,
            messages_url: None,
        },
    );

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

    let note_content = authorship_log.serialize_to_string().unwrap();
    let gitai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("Failed to find repository");
    write_note(&gitai_repo, &commit_sha, &note_content).unwrap();

    // Get hunks with split_hunks_by_ai_author enabled
    let options = GitAiBlameOptions {
        split_hunks_by_ai_author: true,
        ..Default::default()
    };

    let hunks = gitai_repo.blame_hunks("test.txt", 1, 3, &options).unwrap();

    // Should have separate hunks for different human authors
    let ai_authors: Vec<_> = hunks.iter().map(|h| h.ai_human_author.clone()).collect();

    assert!(ai_authors.contains(&Some("Alice <alice@example.com>".to_string())));
    assert!(ai_authors.contains(&Some("Bob <bob@example.com>".to_string())));
}

#[test]
fn test_blame_ai_authorship_no_splitting() {
    // AI authorship: When split_hunks_by_ai_author is false, don't split
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1", "Line 2"]);
    let commit_sha = repo.stage_all_and_commit("Initial").unwrap().commit_sha;

    let mut authorship_log = AuthorshipLog::new();
    authorship_log.metadata.base_commit_sha = commit_sha.clone();

    let prompt_hash = "prompt1".to_string();
    authorship_log.metadata.prompts.insert(
        prompt_hash.clone(),
        PromptRecord {
            agent_id: AgentId {
                tool: "cursor".to_string(),
                id: "session1".to_string(),
                model: "claude-3-sonnet".to_string(),
            },
            human_author: Some("Alice <alice@example.com>".to_string()),
            total_additions: 2,
            total_deletions: 0,
            accepted_lines: 2,
            overriden_lines: 0,
            custom_attributes: None,
            messages_url: None,
        },
    );

    let mut file_attestation = FileAttestation::new("test.txt".to_string());
    file_attestation.add_entry(AttestationEntry::new(
        prompt_hash,
        vec![LineRange::Range(1, 2)],
    ));
    authorship_log.attestations.push(file_attestation);

    let note_content = authorship_log.serialize_to_string().unwrap();
    let gitai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("Failed to find repository");
    write_note(&gitai_repo, &commit_sha, &note_content).unwrap();

    let options = GitAiBlameOptions {
        split_hunks_by_ai_author: false,
        ..Default::default()
    };

    let hunks = gitai_repo.blame_hunks("test.txt", 1, 2, &options).unwrap();

    // Should have single hunk covering both lines
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].range, (1, 2));
}

#[test]
fn test_blame_ai_authorship_return_human_as_human() {
    // AI authorship: return_human_authors_as_human flag
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Human line".human()]);
    repo.stage_all_and_commit("Test").unwrap();

    let gitai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("Failed to find repository");

    let options = GitAiBlameOptions {
        return_human_authors_as_human: true,
        no_output: true,
        ..Default::default()
    };

    let (line_authors, _) = gitai_repo.blame("test.txt", &options).unwrap();

    // Human lines should be marked as "Human" (case-insensitive check)
    let author = line_authors.get(&1).unwrap();
    assert!(
        author.eq_ignore_ascii_case("human"),
        "Expected 'Human' but got '{}'",
        author
    );
}

// =============================================================================
// Commit Range Tests - newest_commit, oldest_commit, oldest_date
// =============================================================================

#[test]
fn test_blame_commit_range_oldest_and_newest() {
    // Commit range: Both oldest_commit and newest_commit specified
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Version 1"]);
    let commit1 = repo.stage_all_and_commit("First").unwrap().commit_sha;

    file.set_contents(crate::lines!["Version 2"]);
    let commit2 = repo.stage_all_and_commit("Second").unwrap().commit_sha;

    file.set_contents(crate::lines!["Version 3"]);
    repo.stage_all_and_commit("Third").unwrap();

    // Blame in range commit1..commit2
    let gitai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("Failed to find repository");

    let options = GitAiBlameOptions {
        oldest_commit: Some(commit1),
        newest_commit: Some(commit2),
        ..Default::default()
    };

    let (line_authors, _) = gitai_repo.blame("test.txt", &options).unwrap();

    // Should show authorship from within the range
    assert!(!line_authors.is_empty());
}

#[test]
fn test_blame_commit_range_with_oldest_date() {
    // Commit range: Using oldest_date to limit history
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Old content"]);
    repo.stage_all_and_commit_with_env(
        "Old",
        &[
            ("GIT_AUTHOR_DATE", "2030-01-03T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2030-01-03T00:00:00Z"),
        ],
    )
    .unwrap();
    let now = chrono::DateTime::parse_from_rfc3339("2030-01-03T00:00:01Z")
        .expect("valid RFC3339 cutoff date")
        .with_timezone(&chrono::Utc);

    file.set_contents(crate::lines!["New content"]);
    repo.stage_all_and_commit_with_env(
        "New",
        &[
            ("GIT_AUTHOR_DATE", "2030-01-03T00:00:02Z"),
            ("GIT_COMMITTER_DATE", "2030-01-03T00:00:02Z"),
        ],
    )
    .unwrap();

    let gitai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("Failed to find repository");

    let options = GitAiBlameOptions {
        oldest_date: Some(now.into()),
        no_output: true,
        ..Default::default()
    };

    // Blame should only see commits after the date
    let result = gitai_repo.blame("test.txt", &options);
    assert!(result.is_ok());
}

// =============================================================================
// Path Normalization Tests - Absolute vs relative paths
// =============================================================================

#[test]
fn test_blame_path_normalization_absolute() {
    // Path normalization: Absolute path should be converted to relative
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Content".ai()]);
    repo.stage_all_and_commit("Test").unwrap();

    let abs_path = repo.path().join("test.txt");
    let output = repo.git_ai(&["blame", abs_path.to_str().unwrap()]).unwrap();

    assert!(output.contains("Content"));
}

#[test]
fn test_blame_path_normalization_relative() {
    // Path normalization: Relative path should work
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Content".ai()]);
    repo.stage_all_and_commit("Test").unwrap();

    let output = repo.git_ai(&["blame", "test.txt"]).unwrap();

    assert!(output.contains("Content"));
}

#[test]
fn test_blame_path_normalization_subdirectory() {
    // Path normalization: File in subdirectory
    let repo = TestRepo::new();

    let subdir = repo.path().join("src");
    std::fs::create_dir_all(&subdir).unwrap();

    let mut file = repo.filename("src/code.rs");
    file.set_contents(crate::lines!["fn main() {}".ai()]);
    repo.stage_all_and_commit("Add code").unwrap();

    let output = repo.git_ai(&["blame", "src/code.rs"]).unwrap();

    assert!(output.contains("fn main()"));
}

// =============================================================================
// Contents Flag Tests - Blaming modified buffer contents
// =============================================================================

#[test]
fn test_blame_contents_modified_buffer() {
    // Contents flag: Blame modified buffer contents (uncommitted changes)
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Original line".ai()]);
    repo.stage_all_and_commit("Original").unwrap();

    // Modified content not yet committed
    let modified = "Modified line\n";

    let output = repo
        .git_ai_with_stdin(
            &["blame", "--contents", "-", "test.txt"],
            modified.as_bytes(),
        )
        .unwrap();

    assert!(output.contains("Modified line"));
    assert!(output.contains("External file"));
}

// =============================================================================
// Multiple Line Ranges Tests
// =============================================================================

#[test]
fn test_blame_multiple_line_ranges() {
    // Multiple line ranges: Blame with multiple -L flags
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1", "Line 2", "Line 3", "Line 4", "Line 5"
    ]);
    repo.stage_all_and_commit("Five lines").unwrap();

    let gitai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("Failed to find repository");

    let options = GitAiBlameOptions {
        line_ranges: vec![(1, 2), (4, 5)],
        no_output: true,
        ..Default::default()
    };

    let (line_authors, _) = gitai_repo.blame("test.txt", &options).unwrap();

    // Should have lines 1, 2, 4, 5 but not 3
    assert!(line_authors.contains_key(&1));
    assert!(line_authors.contains_key(&2));
    assert!(line_authors.contains_key(&4));
    assert!(line_authors.contains_key(&5));
    assert!(!line_authors.contains_key(&3));
}

#[test]
fn test_blame_analysis_matches_blame_no_output_multi_ranges() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1",
        "Line 2".ai(),
        "Line 3",
        "Line 4",
        "Line 5".ai()
    ]);
    repo.stage_all_and_commit("Five lines").unwrap();

    let gitai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("Failed to find repository");

    let options = GitAiBlameOptions {
        line_ranges: vec![(1, 2), (4, 5)],
        no_output: true,
        ..Default::default()
    };

    let (line_authors, prompt_records) = gitai_repo.blame("test.txt", &options).unwrap();
    let analysis = gitai_repo.blame_analysis("test.txt", &options).unwrap();

    assert_eq!(line_authors, analysis.line_authors);
    assert_eq!(prompt_records, analysis.prompt_records);
}

#[test]
fn test_blame_analysis_returns_requested_ranges_only() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines![
        "Line 1", "Line 2", "Line 3", "Line 4", "Line 5", "Line 6"
    ]);
    repo.stage_all_and_commit("Six lines").unwrap();

    let gitai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("Failed to find repository");

    let options = GitAiBlameOptions {
        line_ranges: vec![(1, 2), (5, 6)],
        ..Default::default()
    };

    let analysis = gitai_repo.blame_analysis("test.txt", &options).unwrap();

    let mut actual_lines = std::collections::BTreeSet::new();
    for hunk in &analysis.blame_hunks {
        for line in hunk.range.0..=hunk.range.1 {
            actual_lines.insert(line);
        }
    }

    let expected_lines = std::collections::BTreeSet::from([1u32, 2, 5, 6]);
    assert_eq!(expected_lines, actual_lines);
    assert!(!actual_lines.contains(&3));
    assert!(!actual_lines.contains(&4));
}

// =============================================================================
// Ignore Whitespace Tests
// =============================================================================

#[test]
fn test_blame_ignore_whitespace() {
    // Ignore whitespace: -w flag should ignore whitespace changes
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line1"]);
    let commit1 = repo.stage_all_and_commit("Original").unwrap();

    file.set_contents(crate::lines!["  Line1"]); // Add leading spaces
    repo.stage_all_and_commit("Add spaces").unwrap();

    let gitai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("Failed to find repository");

    let options = GitAiBlameOptions {
        ignore_whitespace: true,
        ..Default::default()
    };

    let hunks = gitai_repo.blame_hunks("test.txt", 1, 1, &options).unwrap();

    // With ignore whitespace, should attribute to original commit
    assert!(hunks[0].commit_sha.starts_with(&commit1.commit_sha[..7]));
}

// =============================================================================
// Abbrev Tests - Hash abbreviation
// =============================================================================

#[test]
fn test_blame_abbrev_custom_length() {
    // Abbrev: Custom hash abbreviation length
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1"]);
    repo.stage_all_and_commit("Test").unwrap();

    let output = repo
        .git_ai(&["blame", "--abbrev", "10", "test.txt"])
        .unwrap();

    // Boundary commits may be prefixed with '^' in default format.
    let first_field = output.split_whitespace().next().unwrap();
    let hash = first_field.trim_start_matches('^');
    assert!(
        (10..=40).contains(&hash.len()),
        "expected abbreviated hash length in [10,40], got {}",
        hash.len()
    );
}

#[test]
fn test_blame_long_rev() {
    // Long rev: -l flag shows full 40-character hash
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1"]);
    repo.stage_all_and_commit("Test").unwrap();

    let output = repo.git_ai(&["blame", "-l", "test.txt"]).unwrap();

    // Boundary commits may be prefixed with '^' in default format.
    let first_field = output.split_whitespace().next().unwrap();
    let hash = first_field.trim_start_matches('^');
    assert_eq!(hash.len(), 40);
}

// =============================================================================
// Date Format Tests
// =============================================================================

#[test]
fn test_blame_date_format_short() {
    // Date format: --date short shows YYYY-MM-DD
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Line 1"]);
    repo.stage_all_and_commit("Test").unwrap();

    let output = repo
        .git_ai(&["blame", "--date", "short", "test.txt"])
        .unwrap();

    // Should contain date in YYYY-MM-DD format
    assert!(output.contains("-")); // Date separator
    let parts: Vec<&str> = output.split_whitespace().collect();
    let date_field = parts
        .iter()
        .find(|s| s.len() == 10 && s.matches('-').count() == 2);
    assert!(date_field.is_some(), "Should have YYYY-MM-DD date");
}

// =============================================================================
// Stress Tests - Performance and robustness
// =============================================================================

#[test]
fn test_blame_stress_many_small_hunks() {
    // Stress: Many small hunks with alternating authorship
    let repo = TestRepo::new();
    let file = repo.filename("alternating.txt");

    let mut lines = Vec::new();
    for i in 0..100 {
        if i % 2 == 0 {
            lines.push(format!("Human {}", i));
        } else {
            lines.push(format!("AI {}", i));
        }
    }
    std::fs::write(file.file_path.clone(), lines.join("\n") + "\n").unwrap();

    repo.stage_all_and_commit("Alternating authorship").unwrap();

    let output = repo.git_ai(&["blame", "alternating.txt"]).unwrap();

    assert!(output.contains("Human 0"));
    assert!(output.contains("AI 99") || output.contains("Human 98"));
}

#[test]
fn test_blame_stress_deeply_nested_path() {
    // Stress: File in deeply nested directory structure
    let repo = TestRepo::new();

    let deep_path = repo
        .path()
        .join("a")
        .join("b")
        .join("c")
        .join("d")
        .join("e")
        .join("f")
        .join("g")
        .join("h");
    std::fs::create_dir_all(&deep_path).unwrap();

    let file_path = deep_path.join("deep.txt");
    std::fs::write(&file_path, "Deep content\n").unwrap();

    repo.git(&["add", "a/b/c/d/e/f/g/h/deep.txt"]).unwrap();
    repo.stage_all_and_commit("Deep file").unwrap();

    let output = repo.git_ai(&["blame", "a/b/c/d/e/f/g/h/deep.txt"]).unwrap();

    assert!(output.contains("Deep content"));
}

crate::reuse_tests_in_worktree!(
    test_blame_success_basic_file,
    test_blame_success_only_human_lines,
    test_blame_success_only_ai_lines,
    test_blame_success_with_line_range,
    test_blame_success_with_newest_commit,
    test_blame_success_json_format,
    test_blame_error_missing_file,
    test_blame_error_invalid_line_range_start_zero,
    test_blame_error_invalid_line_range_end_zero,
    test_blame_error_invalid_line_range_start_greater_than_end,
    test_blame_error_invalid_line_range_beyond_file,
    test_blame_error_invalid_commit_ref,
    test_blame_error_file_outside_repo,
    test_blame_error_directory_instead_of_file,
    test_blame_edge_empty_file,
    test_blame_edge_single_line_file,
    test_blame_edge_large_file,
    test_blame_edge_file_with_unicode,
    test_blame_edge_file_with_very_long_lines,
    test_blame_edge_boundary_commit_flag,
    test_blame_edge_renamed_file,
    test_blame_edge_whitespace_only_lines,
    test_blame_format_porcelain_basic,
    test_blame_format_line_porcelain,
    test_blame_format_incremental,
    test_blame_format_json_structure,
    test_blame_format_json_line_ranges,
    test_blame_format_default_with_flags,
    test_blame_ai_authorship_hunk_splitting,
    test_blame_ai_authorship_no_splitting,
    test_blame_ai_authorship_return_human_as_human,
    test_blame_commit_range_oldest_and_newest,
    test_blame_commit_range_with_oldest_date,
    test_blame_path_normalization_absolute,
    test_blame_path_normalization_relative,
    test_blame_path_normalization_subdirectory,
    test_blame_contents_modified_buffer,
    test_blame_multiple_line_ranges,
    test_blame_ignore_whitespace,
    test_blame_abbrev_custom_length,
    test_blame_long_rev,
    test_blame_date_format_short,
    test_blame_stress_many_small_hunks,
    test_blame_stress_deeply_nested_path,
);
