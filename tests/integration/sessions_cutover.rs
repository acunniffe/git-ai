// Critical regression tests for old-format/new-format coexistence during cutover scenarios
//
// These tests verify that git-ai correctly handles:
// 1. Old-format authorship notes (bare 16-char hex hashes, prompts-only metadata)
// 2. New-format authorship notes (s_::t_ hashes, sessions metadata)
// 3. Mixed scenarios where both formats coexist in the same note or across operations
//
// Format detection: checkpoint.trace_id.is_some() determines which format is used.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::git::notes_api::write_note;
use serde_json::Value;
use std::fs;

// Test 1: Old format note can be read and deserializes correctly
#[test]
fn test_old_format_note_can_be_attached_and_read() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut file = repo.filename("test.txt");
    file.set_contents(crate::lines!["Human line", "AI line".ai()]);
    let commit = repo.stage_all_and_commit("Initial commit").unwrap();

    // Replace with an old-format note (using "cursor" as tool name)
    let old_hash = "5a1b2c3d4e5f6789"; // 16-char bare hex
    let base_sha = &commit.commit_sha;
    let old_note = format!(
        r#"test.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.3.3",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "cursor", "id": "old_session", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }}
}}"#,
        old_hash, base_sha, old_hash
    );

    // Attach old-format note
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, base_sha, &old_note).expect("add old-format note");

    // Verify old format note is present and reads correctly
    let read_note = repo
        .read_authorship_note(base_sha)
        .expect("should have note");
    let log =
        AuthorshipLog::deserialize_from_string(&read_note).expect("should deserialize old note");

    // Verify structure
    assert_eq!(log.metadata.prompts.len(), 1, "should have 1 prompt");
    assert_eq!(
        log.metadata.sessions.len(),
        0,
        "should have no sessions (old format)"
    );

    // Verify old prompt metadata
    let prompt = log
        .metadata
        .prompts
        .get(old_hash)
        .expect("old hash should be in prompts");
    assert_eq!(prompt.agent_id.tool, "cursor");
    assert_eq!(prompt.total_additions, 1);
    assert_eq!(prompt.accepted_lines, 1);

    // Verify attestation uses old format
    assert_eq!(log.attestations.len(), 1);
    assert_eq!(log.attestations[0].entries.len(), 1);
    assert_eq!(log.attestations[0].entries[0].hash, old_hash);

    // Verify blame works with old format note
    file.assert_committed_lines(crate::lines!["Human line".human(), "AI line".ai(),]);
}

// Test 2: Note with both old and new format attestations deserializes and blame works
#[test]
fn test_mixed_format_note_with_both_prompts_and_sessions() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut file = repo.filename("test.txt");
    file.set_contents(crate::lines!["Line 1", "Line 2".ai(), "Line 3".ai()]);
    let commit = repo.stage_all_and_commit("Initial commit").unwrap();

    // Replace with a mixed-format note that has BOTH prompts and sessions
    let old_hash = "abcd1234ef567890"; // 16-char hex for old format
    // The new hash will be extracted from the original note
    let original_note = repo
        .read_authorship_note(&commit.commit_sha)
        .expect("should have original note");
    let original_log =
        AuthorshipLog::deserialize_from_string(&original_note).expect("parse original note");

    // Get the new-format session ID from the original note
    let new_hash = if !original_log.metadata.sessions.is_empty() {
        original_log
            .metadata
            .sessions
            .keys()
            .next()
            .unwrap()
            .clone()
    } else {
        "s_1234567890abcd".to_string() // fallback
    };

    let mixed_note = format!(
        r#"test.txt
  {} 2-2
  {}::t_fedcba0987654321 3-3
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.3.3",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "cursor", "id": "old_session", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }},
  "sessions": {{
    "{}": {{
      "agent_id": {{"tool": "mock_ai", "id": "new_session", "model": "gpt-4"}},
      "human_author": null,
      "messages": []
    }}
  }}
}}"#,
        old_hash, new_hash, commit.commit_sha, old_hash, new_hash
    );

    // Attach mixed-format note
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &mixed_note).expect("add mixed-format note");

    // Read and verify the note
    let read_note = repo
        .read_authorship_note(&commit.commit_sha)
        .expect("should have note");
    let log = AuthorshipLog::deserialize_from_string(&read_note).expect("should parse note");

    // Verify both prompts and sessions are present
    assert_eq!(log.metadata.prompts.len(), 1, "should have 1 prompt");
    assert_eq!(log.metadata.sessions.len(), 1, "should have 1 session");

    // Verify attestations have both formats
    assert_eq!(log.attestations.len(), 1);
    assert_eq!(
        log.attestations[0].entries.len(),
        2,
        "should have 2 attestation entries"
    );

    let mut has_old_format = false;
    let mut has_new_format = false;
    for entry in &log.attestations[0].entries {
        if entry.hash.len() == 16 && !entry.hash.contains("::") {
            has_old_format = true;
        }
        if entry.hash.contains("::t_") {
            has_new_format = true;
        }
    }
    assert!(has_old_format, "should have old-format attestation");
    assert!(has_new_format, "should have new-format attestation");

    // Verify blame works for both formats
    file.assert_committed_lines(crate::lines![
        "Line 1".human(),
        "Line 2".ai(),
        "Line 3".ai(),
    ]);
}

// Test 3: Rebase chain with old and new format notes
#[test]
fn test_rebase_chain_with_old_and_new_format_notes() {
    let repo = TestRepo::new();

    // Create base commit on main
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["Base line"]);
    repo.stage_all_and_commit("Base commit").unwrap();
    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Commit A with AI content on feature
    let mut file_a = repo.filename("file_a.txt");
    file_a.set_contents(crate::lines!["Human line A", "AI line A".ai()]);
    let commit_a = repo.stage_all_and_commit("Commit A").unwrap();

    // Replace commit A's note with old-format note (using "claude" as tool name)
    let old_hash_a = "1111222233334444";
    let old_note_a = format!(
        r#"file_a.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.3.3",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "claude", "id": "old_agent", "model": "claude-3.5"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }}
}}"#,
        old_hash_a, commit_a.commit_sha, old_hash_a
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit_a.commit_sha, &old_note_a).expect("add old-format note A");

    // Commit B with AI content (will use new format naturally)
    let mut file_b = repo.filename("file_b.txt");
    file_b.set_contents(crate::lines!["Human line B", "AI line B".ai()]);
    repo.stage_all_and_commit("Commit B").unwrap();

    // Go back to main, add unrelated commit
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut other = repo.filename("other.txt");
    other.set_contents(crate::lines!["Other line"]);
    repo.stage_all_and_commit("Other commit").unwrap();

    // Rebase feature onto main
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Find the two rebased commits (A' and B')
    let log_output = repo
        .git(&["log", "--oneline", "--no-decorate", "-2"])
        .unwrap();
    let lines: Vec<&str> = log_output.trim().lines().collect();
    assert_eq!(lines.len(), 2, "should have 2 commits");

    // Get commit SHAs (most recent first)
    let commit_b_prime_sha = lines[0].split_whitespace().next().unwrap();
    let commit_a_prime_sha = lines[1].split_whitespace().next().unwrap();

    // Verify commit A' still has prompts (old format preserved)
    let note_a_prime = repo
        .read_authorship_note(commit_a_prime_sha)
        .expect("commit A' should have note");
    let log_a_prime = AuthorshipLog::deserialize_from_string(&note_a_prime).expect("parse note A'");
    assert!(
        !log_a_prime.metadata.prompts.is_empty(),
        "commit A' should have prompts"
    );
    assert_eq!(
        log_a_prime.metadata.sessions.len(),
        0,
        "commit A' should not have sessions (old format)"
    );

    // Verify old prompt data preserved
    assert!(
        log_a_prime.metadata.prompts.contains_key(old_hash_a),
        "old hash should be preserved"
    );

    // Verify commit B' still has sessions (new format preserved)
    let note_b_prime = repo
        .read_authorship_note(commit_b_prime_sha)
        .expect("commit B' should have note");
    let log_b_prime = AuthorshipLog::deserialize_from_string(&note_b_prime).expect("parse note B'");
    assert!(
        !log_b_prime.metadata.sessions.is_empty(),
        "commit B' should have sessions (new format)"
    );

    // Verify blame works correctly on both commits
    repo.git(&["checkout", commit_a_prime_sha]).unwrap();
    file_a.assert_committed_lines(crate::lines!["Human line A".human(), "AI line A".ai(),]);

    repo.git(&["checkout", commit_b_prime_sha]).unwrap();
    file_b.assert_committed_lines(crate::lines!["Human line B".human(), "AI line B".ai(),]);
}

// Test 4: Cherry-pick old format note with AI lines preserved
#[test]
fn test_cherry_pick_old_format_note_with_ai_lines_preserved() {
    let repo = TestRepo::new();

    // Create initial commit on main
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["Base line"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    // Create source branch
    repo.git(&["checkout", "-b", "source"]).unwrap();

    // Add AI content and commit
    let mut file = repo.filename("source.txt");
    file.set_contents(crate::lines!["Human line", "AI line".ai()]);
    let source_commit = repo.stage_all_and_commit("Source commit").unwrap();

    // Replace with old-format note INCLUDING attestation (using "copilot" as tool name)
    let old_hash = "9876543210fedcba";
    let old_note = format!(
        r#"source.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.3.3",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "copilot", "id": "cherry_agent", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }}
}}"#,
        old_hash, source_commit.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &source_commit.commit_sha, &old_note).expect("add old-format note");

    // Go back to main and cherry-pick
    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["cherry-pick", &source_commit.commit_sha])
        .unwrap();

    // Get cherry-picked commit SHA
    let picked_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Verify cherry-picked commit has prompts (not sessions)
    let picked_note = repo
        .read_authorship_note(&picked_sha)
        .expect("cherry-picked commit should have note");
    let picked_log =
        AuthorshipLog::deserialize_from_string(&picked_note).expect("parse cherry-picked note");

    assert!(
        !picked_log.metadata.prompts.is_empty(),
        "cherry-picked commit should have prompts"
    );
    // Note: cherry-pick may add sessions if there are new changes; we primarily care that prompts are preserved
    assert!(
        picked_log.metadata.prompts.contains_key(old_hash),
        "old hash should be preserved in cherry-pick"
    );

    // Verify AI lines correctly attributed
    file.assert_committed_lines(crate::lines!["Human line".human(), "AI line".ai(),]);
}

// Test 5: Verify that sessions-format is the default for all new operations
// This test documents that the current system produces sessions, not prompts
#[test]
fn test_current_system_produces_sessions_not_prompts() {
    let repo = TestRepo::new();

    // Create commit with AI content using standard helpers
    let mut file = repo.filename("test.txt");
    file.set_contents(crate::lines!["Line 1", "AI line".ai()]);
    repo.stage_all_and_commit("AI commit").unwrap();

    // Read note
    let sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note = repo.read_authorship_note(&sha).expect("should have note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse note");

    // Should have sessions, NOT prompts (this is the new default)
    assert!(
        log.metadata.prompts.is_empty(),
        "new system should not produce prompts"
    );
    assert!(
        !log.metadata.sessions.is_empty(),
        "new system should produce sessions"
    );

    // Verify attestations use session format (s_::t_)
    let mut has_session_format = false;
    for file_att in &log.attestations {
        for entry in &file_att.entries {
            if entry.hash.starts_with("s_") && entry.hash.contains("::t_") {
                has_session_format = true;
                break;
            }
        }
    }
    assert!(
        has_session_format,
        "attestations should use session format (s_::t_)"
    );

    // Verify blame works
    file.assert_committed_lines(crate::lines!["Line 1".human(), "AI line".ai(),]);
}

// Test 6: Old format note roundtrips through operations without corruption
#[test]
fn test_old_format_note_roundtrips_without_corruption() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut file = repo.filename("test.txt");
    file.set_contents(crate::lines!["Line 1"]);
    let _initial_commit = repo.stage_all_and_commit("Initial").unwrap();

    // Create commit with AI content
    file.set_contents(crate::lines!["Line 1", "AI line".ai()]);
    let ai_commit = repo.stage_all_and_commit("AI commit").unwrap();

    // Replace with genuine old-format note with stats
    let old_hash = "0123456789abcdef";
    let old_note = format!(
        r#"test.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.3.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "roundtrip_tool", "id": "roundtrip_agent", "model": "roundtrip_model"}},
      "human_author": null,
      "messages": [],
      "total_additions": 42,
      "total_deletions": 7,
      "accepted_lines": 35,
      "overriden_lines": 3
    }}
  }},
  "humans": {{
    "h_fedcba9876543210": {{
      "author": "Test User <test@example.com>"
    }}
  }}
}}"#,
        old_hash, ai_commit.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &ai_commit.commit_sha, &old_note).expect("add old-format note");

    // Read it back
    let note_v1 = repo
        .read_authorship_note(&ai_commit.commit_sha)
        .expect("should have note");
    let log_v1 = AuthorshipLog::deserialize_from_string(&note_v1).expect("parse note v1");

    // Verify structure
    assert_eq!(log_v1.metadata.prompts.len(), 1);
    assert_eq!(log_v1.metadata.sessions.len(), 0);
    assert_eq!(log_v1.metadata.humans.len(), 1);

    // Verify stats preserved
    let prompt_v1 = log_v1
        .metadata
        .prompts
        .get(old_hash)
        .expect("should have old hash");
    assert_eq!(prompt_v1.total_additions, 42);
    assert_eq!(prompt_v1.total_deletions, 7);
    assert_eq!(prompt_v1.accepted_lines, 35);
    assert_eq!(prompt_v1.overriden_lines, 3);

    // Serialize and deserialize again (roundtrip)
    let serialized = log_v1.serialize_to_string().expect("serialize");
    let log_v2 = AuthorshipLog::deserialize_from_string(&serialized).expect("parse note v2");

    // Verify structure unchanged
    assert_eq!(log_v2.metadata.prompts.len(), 1);
    assert_eq!(log_v2.metadata.sessions.len(), 0);
    assert_eq!(log_v2.metadata.humans.len(), 1);

    // Verify stats still preserved
    let prompt_v2 = log_v2
        .metadata
        .prompts
        .get(old_hash)
        .expect("should still have old hash");
    assert_eq!(prompt_v2.total_additions, 42);
    assert_eq!(prompt_v2.total_deletions, 7);
    assert_eq!(prompt_v2.accepted_lines, 35);
    assert_eq!(prompt_v2.overriden_lines, 3);

    // Verify serialized output doesn't contain "sessions" key
    assert!(
        !serialized.contains("\"sessions\""),
        "should not add sessions key to old-format note"
    );
}

// Test 7: Reset with old format notes
#[test]
fn test_reset_preserves_old_format_notes_in_working_log() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("test.txt");

    // Create initial commit
    fs::write(&file_path, "Line 1\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Initial").unwrap();

    // Create commit with AI content
    fs::write(&file_path, "Line 1\nAI line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();
    let commit = repo.stage_all_and_commit("AI commit").unwrap();

    // Replace with old-format note (using "windsurf" as tool name)
    let old_hash = "aabbccddeeff1122";
    let human_hash = "h_resetoldhuman";
    let old_note = format!(
        r#"test.txt
  {} 1-1
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.3.3",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "windsurf", "id": "reset_agent", "model": "claude-3.5"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }},
  "humans": {{
    "{}": {{
      "author": "Test User <test@example.com>"
    }}
  }}
}}"#,
        human_hash, old_hash, commit.commit_sha, old_hash, human_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &old_note).expect("add old-format note");

    // Reset --soft to un-commit but keep changes staged
    repo.git(&["reset", "--soft", "HEAD~1"]).unwrap();

    // Re-commit
    repo.commit("Recommit").unwrap();

    // Verify note is preserved with prompts
    let new_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let new_note = repo
        .read_authorship_note(&new_sha)
        .expect("should have note after reset");
    let new_log = AuthorshipLog::deserialize_from_string(&new_note).expect("parse note");

    // Should have prompts from the old format note
    assert!(
        !new_log.metadata.prompts.is_empty(),
        "should preserve prompts after reset"
    );

    // Verify AI attribution still works
    let mut file = repo.filename("test.txt");
    file.assert_committed_lines(crate::lines!["Line 1".human(), "AI line".ai(),]);
}

// Test 8: Verify that new checkpoints always produce sessions, never prompts
#[test]
fn test_new_checkpoints_always_produce_sessions() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut file = repo.filename("test.txt");
    file.set_contents(crate::lines!["Line 1"]);
    repo.stage_all_and_commit("Initial").unwrap();

    // Use the standard helper which calls mock_ai checkpoint
    file.set_contents(crate::lines!["Line 1", "AI line".ai()]);
    repo.stage_all_and_commit("AI commit").unwrap();

    // Read note
    let sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note = repo.read_authorship_note(&sha).expect("should have note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse note");

    // Should have sessions, NOT prompts
    assert!(
        log.metadata.prompts.is_empty(),
        "new checkpoints should not produce prompts"
    );
    assert!(
        !log.metadata.sessions.is_empty(),
        "new checkpoints should produce sessions"
    );

    // Verify session format in attestations (s_::t_)
    let mut has_session_format = false;
    for file_att in &log.attestations {
        for entry in &file_att.entries {
            if entry.hash.starts_with("s_") && entry.hash.contains("::t_") {
                has_session_format = true;
                break;
            }
        }
    }
    assert!(
        has_session_format,
        "attestations should use session format (s_::t_)"
    );
}

// Test 9: Amend a commit that has an old-format note, with new-format checkpoints in the working log.
// This simulates: user had git-ai old version, made a commit (old prompts note), then upgraded git-ai,
// makes new edits (which produce session-format checkpoints), and amends the commit.
// The post-amend note must have BOTH old prompts AND new sessions.
#[test]
fn test_amend_old_prompts_commit_with_new_session_checkpoints() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("example.txt");

    // Step 1: Create initial commit with known-human context and AI content
    fs::write(&file_path, "Human line 1\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "example.txt"])
        .unwrap();
    let initial = "Human line 1\nAI old line\n";
    fs::write(&file_path, initial).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.txt"])
        .unwrap();
    let commit = repo.stage_all_and_commit("Initial commit").unwrap();

    // Step 2: Replace the note with an old-format note (simulating pre-upgrade git-ai)
    let old_hash = "deadbeef12345678"; // 16-char bare hex (old format)
    let human_hash = "h_amendoldhuman";
    let old_note = format!(
        r#"example.txt
  {} 1-1
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "cursor", "id": "old_session_abc", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }},
  "humans": {{
    "{}": {{
      "author": "Test User <test@example.com>"
    }}
  }}
}}"#,
        human_hash, old_hash, commit.commit_sha, old_hash, human_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &old_note).expect("attach old-format note");

    // Step 3: Make new edits and checkpoint with new-format (mock_ai produces trace_id)
    let edited = "Human line 1\nAI old line\nAI new line\n";
    fs::write(&file_path, edited).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.txt"])
        .unwrap();

    // Step 4: Amend the commit (this triggers the amend rewrite pipeline)
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "--amend", "-m", "Amended commit"])
        .unwrap();

    // Step 5: Read the post-amend note and verify BOTH formats are present
    let amended_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note = repo
        .read_authorship_note(&amended_sha)
        .expect("amended commit should have note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse amended note");

    // The old-format prompt from the original note should still be there
    // (it was referenced by an attestation for line 2 which still exists)
    assert!(
        !log.metadata.prompts.is_empty(),
        "amended note should preserve old prompts from original note"
    );
    assert!(
        log.metadata.prompts.contains_key(old_hash),
        "old prompt hash should be preserved in amended note"
    );

    // The new checkpoint (with trace_id) should have produced a session
    assert!(
        !log.metadata.sessions.is_empty(),
        "amended note should have sessions from new checkpoint"
    );

    // Verify attestations include both formats
    let mut has_old_format_att = false;
    let mut has_new_format_att = false;
    for file_att in &log.attestations {
        for entry in &file_att.entries {
            if entry.hash == old_hash {
                has_old_format_att = true;
            }
            if entry.hash.starts_with("s_") && entry.hash.contains("::t_") {
                has_new_format_att = true;
            }
        }
    }
    assert!(
        has_old_format_att,
        "amended note should have old-format attestation hash"
    );
    assert!(
        has_new_format_att,
        "amended note should have new-format (s_::t_) attestation hash"
    );

    // Verify blame works correctly
    let mut file = repo.filename("example.txt");
    file.assert_committed_lines(crate::lines![
        "Human line 1".human(),
        "AI old line".ai(),
        "AI new line".ai(),
    ]);
}

// Test 10: Mixed working log where old-format checkpoints (trace_id: null, bare hex author_ids)
// coexist with new-format checkpoints (trace_id: Some, s_::t_ author_ids) in the SAME commit.
// This simulates: user upgrades git-ai mid-session. The working log has checkpoints from before
// the upgrade (no trace_id) and after the upgrade (with trace_id). On commit, old entries should
// go to prompts and new entries should go to sessions.
#[test]
fn test_mixed_working_log_old_and_new_checkpoints_produce_both_prompts_and_sessions() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("mixed.txt");

    // Step 1: Create a base commit (human only)
    let base = "Base line\n";
    fs::write(&file_path, base).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "mixed.txt"])
        .unwrap();
    let base_commit = repo.stage_all_and_commit("Base commit").unwrap();

    // Step 2: Make an AI edit using current (new-format) checkpoint
    let edit1 = "Base line\nAI line from old version\n";
    fs::write(&file_path, edit1).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "mixed.txt"])
        .unwrap();

    // Step 3: Manipulate the checkpoints.jsonl to downgrade the FIRST AI checkpoint
    // to old format (remove trace_id, replace s_::t_ author_ids with bare hex)
    let working_log = repo.current_working_logs();
    let checkpoints_file = working_log.dir.join("checkpoints.jsonl");
    assert!(
        checkpoints_file.exists(),
        "checkpoints.jsonl should exist after checkpoint"
    );

    let content = fs::read_to_string(&checkpoints_file).expect("read checkpoints.jsonl");
    let mut modified_lines = Vec::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut checkpoint: Value = serde_json::from_str(line).expect("parse checkpoint JSON");

        // Find AI checkpoints and downgrade the first one we find
        let kind = checkpoint
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("");

        if kind == "AiAgent"
            && checkpoint
                .get("trace_id")
                .and_then(|t| t.as_str())
                .is_some()
        {
            // Compute the correct old-format author_id from agent_id fields
            // (this is what the old system would have stored)
            let agent_tool = checkpoint
                .get("agent_id")
                .and_then(|a| a.get("tool"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let agent_id_str = checkpoint
                .get("agent_id")
                .and_then(|a| a.get("id"))
                .and_then(|i| i.as_str())
                .unwrap_or("");
            let old_author_id =
                git_ai::authorship::authorship_log_serialization::generate_short_hash(
                    agent_id_str,
                    agent_tool,
                );

            // Downgrade: remove trace_id, replace s_::t_ author_ids with old-format hash
            checkpoint["trace_id"] = Value::Null;

            if let Some(entries) = checkpoint.get_mut("entries").and_then(|e| e.as_array_mut()) {
                for entry in entries {
                    if let Some(attributions) =
                        entry.get_mut("attributions").and_then(|a| a.as_array_mut())
                    {
                        for attr in attributions {
                            if let Some(author_id) =
                                attr.get("author_id").and_then(|id| id.as_str())
                                && author_id.starts_with("s_")
                            {
                                attr["author_id"] = Value::String(old_author_id.clone());
                            }
                        }
                    }
                    if let Some(line_attrs) = entry
                        .get_mut("line_attributions")
                        .and_then(|a| a.as_array_mut())
                    {
                        for line_attr in line_attrs {
                            if let Some(author_id) =
                                line_attr.get("author_id").and_then(|id| id.as_str())
                                && author_id.starts_with("s_")
                            {
                                line_attr["author_id"] = Value::String(old_author_id.clone());
                            }
                        }
                    }
                }
            }
        }

        modified_lines
            .push(serde_json::to_string(&checkpoint).expect("serialize modified checkpoint"));
    }
    let new_content = modified_lines.join("\n") + "\n";
    fs::write(&checkpoints_file, new_content).expect("write modified checkpoints.jsonl");

    // Step 4: Make ANOTHER edit with new-format checkpoint (upgrade happened mid-session)
    let edit2 = "Base line\nAI line from old version\nAI line from new version\n";
    fs::write(&file_path, edit2).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "mixed.txt"])
        .unwrap();

    // Step 5: Commit - this should produce a note with BOTH prompts and sessions
    repo.git(&["add", "."]).unwrap();
    repo.commit("Mixed format commit").unwrap();

    // Step 6: Verify the resulting note
    let commit_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(commit_sha, base_commit.commit_sha, "should be a new commit");

    let note = repo
        .read_authorship_note(&commit_sha)
        .expect("mixed commit should have note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse mixed note");

    // Old-format checkpoint (trace_id: null, bare hex) should produce a prompt
    assert!(
        !log.metadata.prompts.is_empty(),
        "old-format checkpoint (no trace_id) should produce a prompt entry, got: prompts={:?}",
        log.metadata.prompts
    );

    // New-format checkpoint (trace_id: Some, s_::t_) should produce a session
    assert!(
        !log.metadata.sessions.is_empty(),
        "new-format checkpoint (with trace_id) should produce a session entry, got: sessions={:?}",
        log.metadata.sessions
    );

    // Verify attestations have both formats
    let mut has_old_att = false;
    let mut has_new_att = false;
    for file_att in &log.attestations {
        for entry in &file_att.entries {
            if !entry.hash.starts_with("s_")
                && !entry.hash.starts_with("h_")
                && entry.hash.len() == 16
            {
                has_old_att = true;
            }
            if entry.hash.starts_with("s_") && entry.hash.contains("::t_") {
                has_new_att = true;
            }
        }
    }
    assert!(
        has_old_att,
        "attestations should include old-format (bare hex) hash, got: {:?}",
        log.attestations
    );
    assert!(
        has_new_att,
        "attestations should include new-format (s_::t_) hash, got: {:?}",
        log.attestations
    );

    // The old-format prompt key should match an attestation hash (both are generate_short_hash output)
    let prompt_key = log.metadata.prompts.keys().next().unwrap();
    assert_eq!(
        prompt_key.len(),
        16,
        "prompt key should be 16 chars (old format)"
    );
    assert!(
        !prompt_key.starts_with("s_"),
        "prompt key should not have session prefix"
    );

    // Verify blame works correctly for all lines
    let mut file = repo.filename("mixed.txt");
    file.assert_committed_lines(crate::lines![
        "Base line".human(),
        "AI line from old version".ai(),
        "AI line from new version".ai(),
    ]);
}

// Test 11: Reset --soft of a commit with old-format note, then make new AI edits (sessions),
// then re-commit. The working log after reset has INITIAL from old note (bare hex prompts).
// New checkpoints produce sessions. Re-commit must have BOTH prompts and sessions.
#[test]
fn test_reset_soft_old_note_then_new_session_checkpoints() {
    let repo = TestRepo::new_with_daemon_scope(crate::repos::test_repo::DaemonTestScope::Dedicated);
    let file_path = repo.path().join("reset_test.txt");

    // Step 1: Create initial commit (needed as parent)
    let base = "Base line\n";
    fs::write(&file_path, base).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "reset_test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Base commit").unwrap();

    // Step 2: Create second commit with AI content
    let second = "Base line\nOld AI line\n";
    fs::write(&file_path, second).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "reset_test.txt"])
        .unwrap();
    let commit = repo.stage_all_and_commit("AI commit").unwrap();

    // Step 3: Replace the note with old-format (simulating pre-upgrade git-ai)
    let old_hash = "f1e2d3c4b5a69788";
    let old_note = format!(
        r#"reset_test.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "cursor", "id": "old_reset_session", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }}
}}"#,
        old_hash, commit.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &old_note).expect("attach old-format note");

    // Step 4: Reset --soft HEAD~1 (uncommit, triggers working log reconstruction with old prompts)
    repo.git(&["reset", "--soft", "HEAD~1"]).unwrap();

    // Step 5: Make new AI edits (produces session-format checkpoints)
    let third = "Base line\nOld AI line\nNew session AI line\n";
    fs::write(&file_path, third).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "reset_test.txt"])
        .unwrap();

    // Step 6: Re-commit
    repo.git(&["add", "."]).unwrap();
    repo.commit("Re-committed with new edits").unwrap();

    // Step 7: Verify the resulting note has BOTH formats
    let new_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note = repo
        .read_authorship_note(&new_sha)
        .expect("re-committed commit should have note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse note");

    // Old-format prompt from the reset commit's note should be preserved
    assert!(
        !log.metadata.prompts.is_empty(),
        "re-committed note should have prompts from old-format note (via reset reconstruction)"
    );

    // New-format session from the fresh checkpoint should be present
    assert!(
        !log.metadata.sessions.is_empty(),
        "re-committed note should have sessions from new checkpoint"
    );

    // Verify attestations have both formats
    let mut has_old_att = false;
    let mut has_new_att = false;
    for file_att in &log.attestations {
        for entry in &file_att.entries {
            if !entry.hash.starts_with("s_")
                && !entry.hash.starts_with("h_")
                && entry.hash.len() == 16
            {
                has_old_att = true;
            }
            if entry.hash.starts_with("s_") && entry.hash.contains("::t_") {
                has_new_att = true;
            }
        }
    }
    assert!(has_old_att, "should have old-format attestation hash");
    assert!(
        has_new_att,
        "should have new-format (s_::t_) attestation hash"
    );

    // Verify blame
    let mut file = repo.filename("reset_test.txt");
    file.assert_committed_lines(crate::lines![
        "Base line".human(),
        "Old AI line".ai(),
        "New session AI line".ai(),
    ]);
}

// Test 12: Squash merge a feature branch where some commits have old-format notes
// and others have new-format notes. The squashed commit must contain BOTH prompts and sessions.
#[test]
fn test_squash_merge_mixed_format_commits() {
    let repo = TestRepo::new();

    // Step 1: Create base commit on main
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["Base line"]);
    repo.stage_all_and_commit("Base commit").unwrap();
    let default_branch = repo.current_branch();

    // Step 2: Create feature branch
    repo.git(&["checkout", "-b", "feature-mixed"]).unwrap();

    // Step 3: Commit C1 with AI content, then replace with old-format note
    let mut file_a = repo.filename("feature_a.txt");
    file_a.set_contents(crate::lines!["Human A", "AI A".ai()]);
    let commit_a = repo.stage_all_and_commit("Feature commit A").unwrap();

    let old_hash = "aaaa1111bbbb2222";
    let old_note = format!(
        r#"feature_a.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "windsurf", "id": "old_squash_session", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }}
}}"#,
        old_hash, commit_a.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit_a.commit_sha, &old_note).expect("attach old-format note");

    // Step 4: Commit C2 with AI content using standard helpers (produces new-format/sessions)
    let mut file_b = repo.filename("feature_b.txt");
    file_b.set_contents(crate::lines!["Human B", "AI B".ai()]);
    repo.stage_all_and_commit("Feature commit B").unwrap();

    // Step 5: Switch to main, squash merge
    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["merge", "--squash", "feature-mixed"]).unwrap();
    repo.commit("Squash merge mixed formats").unwrap();

    // Step 6: Verify squashed commit note has BOTH prompts and sessions
    let squash_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note = repo
        .read_authorship_note(&squash_sha)
        .expect("squash commit should have note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse squash note");

    // C1's old-format prompts should be preserved
    assert!(
        !log.metadata.prompts.is_empty(),
        "squash note should have prompts from old-format commit C1"
    );

    // C2's new-format sessions should be present
    assert!(
        !log.metadata.sessions.is_empty(),
        "squash note should have sessions from new-format commit C2"
    );

    // Verify both file attestations work for blame
    file_a.assert_committed_lines(crate::lines!["Human A".human(), "AI A".ai(),]);
    file_b.assert_committed_lines(crate::lines!["Human B".human(), "AI B".ai(),]);
}

// Test 13: Stash and pop a mixed-format working log.
// The working log has old-format checkpoints (downgraded, no trace_id) + new-format checkpoints.
// After stash push + pop + commit, the note should have BOTH prompts and sessions.
#[test]
fn test_stash_pop_mixed_format_working_log() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("stash_test.txt");

    // Step 1: Create base commit
    let base = "Base line\n";
    fs::write(&file_path, base).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "stash_test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Base commit").unwrap();

    // Step 2: Make an AI edit (new format checkpoint)
    let edit1 = "Base line\nAI old line\n";
    fs::write(&file_path, edit1).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "stash_test.txt"])
        .unwrap();

    // Step 3: Downgrade that checkpoint to old format
    let working_log = repo.current_working_logs();
    let checkpoints_file = working_log.dir.join("checkpoints.jsonl");
    assert!(checkpoints_file.exists(), "checkpoints.jsonl should exist");

    let content = fs::read_to_string(&checkpoints_file).expect("read checkpoints");
    let mut modified_lines = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut checkpoint: Value = serde_json::from_str(line).expect("parse checkpoint");
        let kind = checkpoint
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("");
        if kind == "AiAgent"
            && checkpoint
                .get("trace_id")
                .and_then(|t| t.as_str())
                .is_some()
        {
            let agent_tool = checkpoint
                .get("agent_id")
                .and_then(|a| a.get("tool"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let agent_id_str = checkpoint
                .get("agent_id")
                .and_then(|a| a.get("id"))
                .and_then(|i| i.as_str())
                .unwrap_or("");
            let old_author_id =
                git_ai::authorship::authorship_log_serialization::generate_short_hash(
                    agent_id_str,
                    agent_tool,
                );
            checkpoint["trace_id"] = Value::Null;
            if let Some(entries) = checkpoint.get_mut("entries").and_then(|e| e.as_array_mut()) {
                for entry in entries {
                    if let Some(attributions) =
                        entry.get_mut("attributions").and_then(|a| a.as_array_mut())
                    {
                        for attr in attributions {
                            if let Some(author_id) =
                                attr.get("author_id").and_then(|id| id.as_str())
                                && author_id.starts_with("s_")
                            {
                                attr["author_id"] = Value::String(old_author_id.clone());
                            }
                        }
                    }
                    if let Some(line_attrs) = entry
                        .get_mut("line_attributions")
                        .and_then(|a| a.as_array_mut())
                    {
                        for line_attr in line_attrs {
                            if let Some(author_id) =
                                line_attr.get("author_id").and_then(|id| id.as_str())
                                && author_id.starts_with("s_")
                            {
                                line_attr["author_id"] = Value::String(old_author_id.clone());
                            }
                        }
                    }
                }
            }
        }
        modified_lines.push(serde_json::to_string(&checkpoint).expect("serialize"));
    }
    fs::write(&checkpoints_file, modified_lines.join("\n") + "\n").expect("write");

    // Step 4: Make another AI edit (new format)
    let edit2 = "Base line\nAI old line\nAI new line\n";
    fs::write(&file_path, edit2).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "stash_test.txt"])
        .unwrap();

    // Step 5: Stash
    repo.git(&["stash", "push", "-u"]).unwrap();

    // Step 6: Pop
    repo.git(&["stash", "pop"]).unwrap();

    // Step 7: Commit
    repo.git(&["add", "."]).unwrap();
    repo.commit("After stash pop").unwrap();

    // Step 8: Verify the note
    let sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note = repo
        .read_authorship_note(&sha)
        .expect("post-stash commit should have note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse note");

    // Old-format checkpoint should produce prompt
    assert!(
        !log.metadata.prompts.is_empty(),
        "post-stash note should have prompts from old-format checkpoint"
    );

    // New-format checkpoint should produce session
    assert!(
        !log.metadata.sessions.is_empty(),
        "post-stash note should have sessions from new-format checkpoint"
    );

    // Verify blame
    let mut file = repo.filename("stash_test.txt");
    file.assert_committed_lines(crate::lines![
        "Base line".human(),
        "AI old line".ai(),
        "AI new line".ai(),
    ]);
}

// Test 14: Rebase a commit with old-format note that conflicts, AI resolves the conflict
// (producing new session-format checkpoints). The resulting note should have sessions from
// the conflict resolution. This documents that build_note_from_conflict_wl uses the
// working log's format, not the original commit's note format.
#[test]
fn test_rebase_conflict_old_note_ai_resolves_with_sessions() {
    let repo = TestRepo::new_with_daemon_scope(crate::repos::test_repo::DaemonTestScope::Dedicated);

    // Step 1: Create base commit
    let mut file = repo.filename("conflict.txt");
    file.set_contents(crate::lines!["Original line"]);
    repo.stage_all_and_commit("Base commit").unwrap();
    let default_branch = repo.current_branch();

    // Step 2: Create feature branch with AI content
    repo.git(&["checkout", "-b", "feature-conflict"]).unwrap();
    file.set_contents(crate::lines!["Original line", "AI feature line".ai()]);
    let feature_commit = repo.stage_all_and_commit("Feature commit").unwrap();

    // Replace with old-format note
    let old_hash = "cccc3333dddd4444";
    let old_note = format!(
        r#"conflict.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "claude", "id": "old_rebase_session", "model": "claude-3.5"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }}
}}"#,
        old_hash, feature_commit.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &feature_commit.commit_sha, &old_note)
        .expect("attach old-format note");

    // Step 3: Go back to main, make conflicting change
    repo.git(&["checkout", &default_branch]).unwrap();
    file.set_contents(crate::lines!["Modified base line"]);
    repo.stage_all_and_commit("Conflicting commit on main")
        .unwrap();

    // Step 4: Rebase feature onto main (will conflict)
    repo.git(&["checkout", "feature-conflict"]).unwrap();
    let rebase_result = repo.git(&["rebase", &default_branch]);
    assert!(rebase_result.is_err(), "rebase should conflict");

    // Step 5: AI resolves the conflict by writing the merged file and checkpointing
    // (using set_contents which calls checkpoint mock_ai + stages the file)
    file.set_contents(crate::lines![
        "Modified base line".human(),
        "AI resolved line".ai()
    ]);
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    // Step 6: Verify the rebased commit's note
    repo.sync_daemon_force();
    let rebased_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note = repo
        .read_authorship_note(&rebased_sha)
        .expect("rebased commit should have note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse rebased note");

    // The conflict was re-resolved from scratch, so build_note_from_conflict_wl
    // creates the note from the conflict working log. The AI checkpoint used new-format
    // (with trace_id), so the result should have sessions.
    assert!(
        !log.metadata.sessions.is_empty(),
        "rebased note should have sessions from AI conflict resolution checkpoint"
    );

    // Verify AI attribution on the resolved line
    file.assert_committed_lines(crate::lines![
        "Modified base line".human(),
        "AI resolved line".ai(),
    ]);
}

// Test 15: show-prompt with an old-format prompt ID finds it in metadata.prompts
#[test]
fn test_show_prompt_finds_old_format_prompt_by_id() {
    let repo = TestRepo::new();

    // Create commit with AI content
    let mut file = repo.filename("test.txt");
    file.set_contents(crate::lines!["Human line", "AI line".ai()]);
    let commit = repo.stage_all_and_commit("AI commit").unwrap();

    // Replace with old-format note
    let old_hash = "abcd1234efgh5678";
    let old_note = format!(
        r#"test.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "cursor", "id": "show_prompt_session", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 5,
      "total_deletions": 2,
      "accepted_lines": 3,
      "overriden_lines": 1
    }}
  }}
}}"#,
        old_hash, commit.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &old_note).expect("attach old-format note");

    // show-prompt with --commit should find the old-format prompt
    let output = repo
        .git_ai(&["show-prompt", old_hash, "--commit", "HEAD"])
        .expect("show-prompt should find old-format prompt");

    let json: Value = serde_json::from_str(output.trim()).unwrap();
    assert_eq!(json["prompt_id"].as_str(), Some(old_hash));
    assert_eq!(json["prompt"]["agent_id"]["tool"].as_str(), Some("cursor"));
    assert_eq!(json["prompt"]["agent_id"]["model"].as_str(), Some("gpt-4"));
}

// Test 16: show-prompt searches history for old-format prompt
#[test]
fn test_show_prompt_finds_old_format_prompt_in_history() {
    let repo = TestRepo::new();

    // Create commit with AI content
    let mut file = repo.filename("test.txt");
    file.set_contents(crate::lines!["Human line", "AI line".ai()]);
    let commit = repo.stage_all_and_commit("AI commit").unwrap();

    // Replace with old-format note
    let old_hash = "1122334455667788";
    let old_note = format!(
        r#"test.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "windsurf", "id": "history_session", "model": "claude-3.5"}},
      "human_author": null,
      "messages": [],
      "total_additions": 10,
      "total_deletions": 3,
      "accepted_lines": 7,
      "overriden_lines": 2
    }}
  }}
}}"#,
        old_hash, commit.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &old_note).expect("attach old-format note");

    // show-prompt without --commit should search history and find it
    let output = repo
        .git_ai(&["show-prompt", old_hash])
        .expect("show-prompt should find old-format prompt in history");

    let json: Value = serde_json::from_str(output.trim()).unwrap();
    assert_eq!(json["prompt_id"].as_str(), Some(old_hash));
    assert_eq!(
        json["prompt"]["agent_id"]["tool"].as_str(),
        Some("windsurf")
    );
}

// Test 17: git-ai stats --json works correctly with old-format notes.
// After the stats simplification (PR #1154), prompt-era fields like total_additions,
// total_deletions, and overriden_lines are no longer surfaced. Stats are now purely
// diff-based. This test verifies that old-format notes don't break stats and that
// diff-based ai_accepted still works correctly.
#[test]
fn test_stats_json_works_with_old_format_notes() {
    let repo = TestRepo::new();

    // Create commit with AI content
    let mut file = repo.filename("test.txt");
    file.set_contents(crate::lines!["Human line", "AI line".ai()]);
    let commit = repo.stage_all_and_commit("AI commit").unwrap();

    // Replace with old-format note that has specific stats
    let old_hash = "aabb11223344ccdd";
    let old_note = format!(
        r#"test.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "cursor", "id": "stats_session", "model": "gpt-4o"}},
      "human_author": null,
      "messages": [],
      "total_additions": 15,
      "total_deletions": 5,
      "accepted_lines": 8,
      "overriden_lines": 3
    }}
  }}
}}"#,
        old_hash, commit.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &old_note).expect("attach old-format note");

    // Run git-ai stats --json — should not crash on old-format notes
    let output = repo
        .git_ai(&["stats", "--json"])
        .expect("stats should work with old-format notes");
    let json: Value = serde_json::from_str(output.trim()).unwrap();

    // Diff-based ai_accepted should still correctly count AI lines from the attestation
    let ai_accepted = json["ai_accepted"].as_u64().unwrap_or(0);
    assert_eq!(
        ai_accepted, 1,
        "ai_accepted should count AI lines from old-format attestation (1 line at line 2)"
    );

    // ai_additions should equal ai_accepted (post-PR-1154: no mixed component)
    let ai_additions = json["ai_additions"].as_u64().unwrap_or(0);
    assert_eq!(
        ai_additions, ai_accepted,
        "ai_additions should equal ai_accepted"
    );

    // tool_model_breakdown should still include the old-format prompt's tool::model
    let breakdown = &json["tool_model_breakdown"];
    assert!(
        breakdown.get("cursor::gpt-4o").is_some(),
        "tool_model_breakdown should include old-format prompt's tool::model"
    );
}

// Test 18: git-ai diff --json --all-prompts with new-format (sessions-only) commit
// includes sessions in the dedicated "sessions" output key
#[test]
fn test_diff_json_all_prompts_includes_sessions() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut file = repo.filename("test.txt");
    file.set_contents(crate::lines!["Line 1"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Create second commit with AI content (produces sessions, not prompts)
    file.set_contents(crate::lines!["Line 1", "AI line".ai()]);
    let commit = repo.stage_all_and_commit("AI commit").unwrap();

    // Run git-ai diff --json --all-prompts
    let output = repo
        .git_ai(&["diff", "--json", "--all-prompts", &commit.commit_sha])
        .expect("diff --json --all-prompts should work");
    let json: Value = serde_json::from_str(output.trim()).unwrap();

    // Sessions appear in the dedicated "sessions" key, not in "prompts"
    let sessions = json["sessions"].as_object();
    assert!(
        sessions.is_some() && !sessions.unwrap().is_empty(),
        "diff --json --all-prompts should include sessions in 'sessions' key, got: {:?}",
        json["sessions"]
    );

    // prompts should be empty for new-format-only commits
    let prompts = json["prompts"].as_object();
    assert!(
        prompts.is_none() || prompts.unwrap().is_empty(),
        "new-format commit should not have entries in 'prompts'"
    );

    // Session keys should use s_xxx format (session ID only, not combined with trace ID)
    let first_key = sessions.unwrap().keys().next().unwrap();
    assert!(
        first_key.starts_with("s_") && !first_key.contains("::"),
        "session key should be session ID only (s_xxx), not combined ID (s_xxx::t_yyy), got: {}",
        first_key
    );

    // The session should have mock_ai as the tool
    let first_session = sessions.unwrap().values().next().unwrap();
    assert_eq!(
        first_session["agent_id"]["tool"].as_str(),
        Some("mock_ai"),
        "session should have correct agent_id"
    );
}

// Test 19: git-ai diff --json --all-prompts with old-format (prompts-only) commit
// includes old prompts in the output
#[test]
fn test_diff_json_all_prompts_includes_old_format_prompts() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut file = repo.filename("test.txt");
    file.set_contents(crate::lines!["Line 1"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Create second commit with AI content
    file.set_contents(crate::lines!["Line 1", "AI line".ai()]);
    let commit = repo.stage_all_and_commit("AI commit").unwrap();

    // Replace with old-format note
    let old_hash = "5566778899aabbcc";
    let old_note = format!(
        r#"test.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "copilot", "id": "diff_session", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 5,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }}
}}"#,
        old_hash, commit.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &old_note).expect("attach old-format note");

    // Run git-ai diff --json --all-prompts
    let output = repo
        .git_ai(&["diff", "--json", "--all-prompts", &commit.commit_sha])
        .expect("diff --json --all-prompts should work");
    let json: Value = serde_json::from_str(output.trim()).unwrap();

    // The prompts should include the old-format prompt
    let prompts = json["prompts"].as_object();
    assert!(
        prompts.is_some() && !prompts.unwrap().is_empty(),
        "diff --json --all-prompts should include old-format prompts"
    );
    assert!(
        prompts.unwrap().contains_key(old_hash),
        "old prompt hash should be present in diff --all-prompts output"
    );
}

// Test 20: Amend a commit with old-format prompts where the user DELETES the AI line.
// The pruning logic should remove the now-unreferenced prompt from metadata.
// Then the user adds a NEW AI line (session-format). The result should have only sessions.
#[test]
fn test_amend_old_prompts_delete_ai_line_then_add_new_session_line() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("prune.txt");

    // Step 1: Create initial commit with known-human context and AI content
    fs::write(&file_path, "Human line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "prune.txt"])
        .unwrap();
    let initial = "Human line\nOld AI line\n";
    fs::write(&file_path, initial).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "prune.txt"])
        .unwrap();
    let commit = repo.stage_all_and_commit("Initial commit").unwrap();

    // Step 2: Replace note with old-format
    let old_hash = "prunetest1234567";
    let human_hash = "h_pruneoldhuman";
    let old_note = format!(
        r#"prune.txt
  {} 1-1
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "cursor", "id": "prune_agent", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }},
  "humans": {{
    "{}": {{
      "author": "Test User <test@example.com>"
    }}
  }}
}}"#,
        human_hash, old_hash, commit.commit_sha, old_hash, human_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &old_note).expect("attach old-format note");

    // Step 3: Delete the old AI line and add a new one with new-format checkpoint
    let edited = "Human line\nNew session AI line\n";
    fs::write(&file_path, edited).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "prune.txt"])
        .unwrap();

    // Step 4: Amend
    repo.git(&["add", "."]).unwrap();
    repo.git(&[
        "commit",
        "--amend",
        "-m",
        "Amended: deleted old AI, added new",
    ])
    .unwrap();

    // Step 5: Verify
    let amended_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note = repo
        .read_authorship_note(&amended_sha)
        .expect("amended commit should have note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse amended note");

    // The old prompt should be PRUNED because its referenced line (line 2) was deleted
    assert!(
        !log.metadata.prompts.contains_key(old_hash),
        "old prompt should be pruned when its AI line is deleted during amend"
    );

    // The new checkpoint should produce a session
    assert!(
        !log.metadata.sessions.is_empty(),
        "new AI line should produce a session entry"
    );

    // Verify attestations only have new-format
    let mut has_old_att = false;
    let mut has_new_att = false;
    for file_att in &log.attestations {
        for entry in &file_att.entries {
            if entry.hash == old_hash {
                has_old_att = true;
            }
            if entry.hash.starts_with("s_") && entry.hash.contains("::t_") {
                has_new_att = true;
            }
        }
    }
    assert!(
        !has_old_att,
        "old attestation hash should be removed when line is deleted"
    );
    assert!(has_new_att, "new session attestation should be present");

    // Verify blame
    let mut file = repo.filename("prune.txt");
    file.assert_committed_lines(crate::lines![
        "Human line".human(),
        "New session AI line".ai(),
    ]);
}

// Test 21: Amend a commit with old-format prompts, KEEPING the old AI line
// and adding a new AI line in the SAME file. Both old prompt AND new session
// must be present in the final note, with correct per-line attribution.
#[test]
fn test_amend_old_prompts_keep_old_line_add_new_session_same_file() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("keepold.txt");

    // Step 1: Create initial commit with known-human context and AI content
    fs::write(&file_path, "Human line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "keepold.txt"])
        .unwrap();
    let initial = "Human line\nOld AI line\n";
    fs::write(&file_path, initial).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "keepold.txt"])
        .unwrap();
    let commit = repo.stage_all_and_commit("Initial commit").unwrap();

    // Step 2: Replace note with old-format
    let old_hash = "keepoldtest12345";
    let human_hash = "h_keepoldhuman";
    let old_note = format!(
        r#"keepold.txt
  {} 1-1
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "windsurf", "id": "keep_agent", "model": "claude-3.5"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }},
  "humans": {{
    "{}": {{
      "author": "Test User <test@example.com>"
    }}
  }}
}}"#,
        human_hash, old_hash, commit.commit_sha, old_hash, human_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &old_note).expect("attach old-format note");

    // Step 3: Add a new line at the end (keep existing content) with new-format checkpoint
    let edited = "Human line\nOld AI line\nNew session AI line\n";
    fs::write(&file_path, edited).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "keepold.txt"])
        .unwrap();

    // Step 4: Amend
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "--amend", "-m", "Amended: kept old, added new"])
        .unwrap();

    // Step 5: Verify
    let amended_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note = repo
        .read_authorship_note(&amended_sha)
        .expect("amended commit should have note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse amended note");

    // Old prompt should be preserved (its line is still there)
    assert!(
        log.metadata.prompts.contains_key(old_hash),
        "old prompt should be preserved when its AI line still exists"
    );

    // New session should also be present
    assert!(
        !log.metadata.sessions.is_empty(),
        "new AI line should produce a session entry"
    );

    // Verify attestations have BOTH formats for different lines
    let mut old_att_lines: Vec<String> = Vec::new();
    let mut new_att_lines: Vec<String> = Vec::new();
    for file_att in &log.attestations {
        for entry in &file_att.entries {
            if entry.hash == old_hash {
                old_att_lines.push(format!("{:?}", entry.line_ranges));
            }
            if entry.hash.starts_with("s_") && entry.hash.contains("::t_") {
                new_att_lines.push(format!("{:?}", entry.line_ranges));
            }
        }
    }
    assert!(
        !old_att_lines.is_empty(),
        "old-format attestation should still reference old AI line"
    );
    assert!(
        !new_att_lines.is_empty(),
        "new-format attestation should reference new AI line"
    );

    // Verify blame shows both lines as AI
    let mut file = repo.filename("keepold.txt");
    file.assert_committed_lines(crate::lines![
        "Human line".human(),
        "Old AI line".ai(),
        "New session AI line".ai(),
    ]);
}

// Test 22: Multiple sequential amends on the same commit, mixing formats.
// Commit starts with old prompts → first amend adds session lines → second amend adds more.
// All attributions must survive through multiple amends.
#[test]
fn test_multiple_amends_mixed_format_accumulation() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("multi.txt");

    // Step 1: Initial commit with known-human context and AI content
    fs::write(&file_path, "Line 1\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "multi.txt"])
        .unwrap();
    let initial = "Line 1\nOld AI line\n";
    fs::write(&file_path, initial).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "multi.txt"])
        .unwrap();
    let commit = repo.stage_all_and_commit("Initial commit").unwrap();

    // Step 2: Replace note with old-format
    let old_hash = "multiamend123456";
    let human_hash = "h_multiamendhuman";
    let old_note = format!(
        r#"multi.txt
  {} 1-1
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "copilot", "id": "multi_agent", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }},
  "humans": {{
    "{}": {{
      "author": "Test User <test@example.com>"
    }}
  }}
}}"#,
        human_hash, old_hash, commit.commit_sha, old_hash, human_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &old_note).expect("attach old-format note");

    // Step 3: First amend - add new AI line
    let edit1 = "Line 1\nOld AI line\nFirst session line\n";
    fs::write(&file_path, edit1).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "multi.txt"])
        .unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "--amend", "-m", "First amend"])
        .unwrap();

    // Verify after first amend
    let sha1 = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note1 = repo
        .read_authorship_note(&sha1)
        .expect("first amend should have note");
    let log1 = AuthorshipLog::deserialize_from_string(&note1).expect("parse first amend note");
    assert!(
        log1.metadata.prompts.contains_key(old_hash),
        "first amend should preserve old prompt"
    );
    assert!(
        !log1.metadata.sessions.is_empty(),
        "first amend should have sessions"
    );

    // Step 4: Second amend - add another AI line
    let edit2 = "Line 1\nOld AI line\nFirst session line\nSecond session line\n";
    fs::write(&file_path, edit2).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "multi.txt"])
        .unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "--amend", "-m", "Second amend"])
        .unwrap();

    // Verify after second amend
    let sha2 = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note2 = repo
        .read_authorship_note(&sha2)
        .expect("second amend should have note");
    let log2 = AuthorshipLog::deserialize_from_string(&note2).expect("parse second amend note");

    // Old prompt should STILL be preserved (its original AI line hasn't been deleted)
    assert!(
        log2.metadata.prompts.contains_key(old_hash),
        "second amend should still preserve old prompt (line is still there)"
    );

    // Sessions should be present (both amend additions)
    assert!(
        !log2.metadata.sessions.is_empty(),
        "second amend should have sessions"
    );

    // Verify blame: all AI lines are correctly attributed
    let mut file = repo.filename("multi.txt");
    file.assert_committed_lines(crate::lines![
        "Line 1".human(),
        "Old AI line".ai(),
        "First session line".ai(),
        "Second session line".ai(),
    ]);
}

// Test 23: Working log INITIAL from old-format note (via reset) contains old-format
// author_ids in the file entries. When the user adds both a known_human edit and a
// session-format AI edit, the resulting commit should properly route:
// - INITIAL's old-format author_ids → prompts
// - New known_human edits → humans
// - New AI edits → sessions
#[test]
fn test_initial_from_old_note_plus_human_and_session_edits() {
    let repo = TestRepo::new_with_daemon_scope(crate::repos::test_repo::DaemonTestScope::Dedicated);
    let file_path = repo.path().join("triple.txt");

    // Step 1: Base commit
    let base = "Line 1\n";
    fs::write(&file_path, base).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "triple.txt"])
        .unwrap();
    repo.stage_all_and_commit("Base").unwrap();

    // Step 2: Commit with AI content
    let ai_edit = "Line 1\nOld AI line\n";
    fs::write(&file_path, ai_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "triple.txt"])
        .unwrap();
    let ai_commit = repo.stage_all_and_commit("AI commit").unwrap();

    // Step 3: Replace with old-format note
    let old_hash = "tripletest567890";
    let old_note = format!(
        r#"triple.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "cursor", "id": "triple_agent", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }}
}}"#,
        old_hash, ai_commit.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &ai_commit.commit_sha, &old_note).expect("attach old-format note");

    // Step 4: Reset --soft to bring content back to working tree
    repo.git(&["reset", "--soft", "HEAD~1"]).unwrap();

    // Step 5: Add a known-human line
    let human_edit = "Line 1\nOld AI line\nHuman typed this\n";
    fs::write(&file_path, human_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "triple.txt"])
        .unwrap();

    // Step 6: Add a new AI line (session-format)
    let session_edit = "Line 1\nOld AI line\nHuman typed this\nNew AI session line\n";
    fs::write(&file_path, session_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "triple.txt"])
        .unwrap();

    // Step 7: Commit
    repo.git(&["add", "."]).unwrap();
    repo.commit("Mixed triple commit").unwrap();

    // Step 8: Verify
    let sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note = repo
        .read_authorship_note(&sha)
        .expect("triple commit should have note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse triple note");

    // Old-format INITIAL attributions should produce prompts
    assert!(
        !log.metadata.prompts.is_empty(),
        "old-format INITIAL author_ids should route to prompts"
    );

    // Known-human edit should produce humans
    assert!(
        !log.metadata.humans.is_empty(),
        "known_human checkpoint should produce humans entry"
    );

    // New AI edit should produce sessions
    assert!(
        !log.metadata.sessions.is_empty(),
        "new AI checkpoint should produce sessions entry"
    );

    // Verify blame: all three types of attribution should work correctly
    let mut file = repo.filename("triple.txt");
    file.assert_committed_lines(crate::lines![
        "Line 1".human(),
        "Old AI line".ai(),
        "Human typed this".human(),
        "New AI session line".ai(),
    ]);
}

// Test 24: Amend with old-format prompts where a DIFFERENT file gets new session edits.
// Tests cross-file mixed format: file A has old prompt attestation, file B has new session attestation.
#[test]
fn test_amend_old_prompts_different_file_gets_session_edits() {
    let repo = TestRepo::new();
    let file_a = repo.path().join("file_a.txt");
    let file_b = repo.path().join("file_b.txt");

    // Step 1: Initial commit with known-human context and AI content in file_a
    fs::write(&file_a, "Human line A\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "file_a.txt"])
        .unwrap();
    let initial_a = "Human line A\nOld AI line A\n";
    fs::write(&file_a, initial_a).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file_a.txt"])
        .unwrap();
    let commit = repo.stage_all_and_commit("Initial commit").unwrap();

    // Step 2: Replace with old-format note for file_a
    let old_hash = "crossfile1234567";
    let human_hash = "h_crossfilehuman";
    let old_note = format!(
        r#"file_a.txt
  {} 1-1
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "copilot", "id": "cross_agent", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }},
  "humans": {{
    "{}": {{
      "author": "Test User <test@example.com>"
    }}
  }}
}}"#,
        human_hash, old_hash, commit.commit_sha, old_hash, human_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &old_note).expect("attach old-format note");

    // Step 3: Create file_b with new session AI content (different file, not in original commit)
    let content_b = "New session AI line B\n";
    fs::write(&file_b, content_b).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file_b.txt"])
        .unwrap();

    // Step 4: Amend to include file_b
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "--amend", "-m", "Amended: added file_b"])
        .unwrap();

    // Step 5: Verify
    let amended_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note = repo
        .read_authorship_note(&amended_sha)
        .expect("amended commit should have note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse amended note");

    // Old prompt from file_a should be preserved
    assert!(
        log.metadata.prompts.contains_key(old_hash),
        "old prompt for file_a should be preserved in cross-file amend"
    );

    // New session from file_b should be present
    assert!(
        !log.metadata.sessions.is_empty(),
        "new session for file_b should be present"
    );

    // Verify attestations reference both files correctly
    let mut file_a_has_old = false;
    let mut file_b_has_new = false;
    for file_att in &log.attestations {
        let path = &file_att.file_path;
        for entry in &file_att.entries {
            if path == "file_a.txt" && entry.hash == old_hash {
                file_a_has_old = true;
            }
            if path == "file_b.txt" && entry.hash.starts_with("s_") && entry.hash.contains("::t_") {
                file_b_has_new = true;
            }
        }
    }
    assert!(file_a_has_old, "file_a should have old-format attestation");
    assert!(file_b_has_new, "file_b should have new-format attestation");

    // Verify blame on both files
    let mut fa = repo.filename("file_a.txt");
    fa.assert_committed_lines(crate::lines!["Human line A".human(), "Old AI line A".ai(),]);
    let mut fb = repo.filename("file_b.txt");
    fb.assert_committed_lines(crate::lines!["New session AI line B".ai(),]);
}

// Test 25: git ai status correctly counts AI lines from old-format INITIAL entries.
// When a reset brings old-format prompts into the INITIAL working log, `git ai status`
// should recognize those author_ids as AI (via prompts map lookup) and report them.
#[test]
fn test_status_counts_ai_lines_from_old_format_initial() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("status_test.txt");

    // Step 1: Base commit
    let base = "Base line\n";
    fs::write(&file_path, base).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "status_test.txt"])
        .unwrap();
    repo.stage_all_and_commit("Base").unwrap();

    // Step 2: Commit with AI content
    let ai_edit = "Base line\nAI status line\nAnother AI line\n";
    fs::write(&file_path, ai_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "status_test.txt"])
        .unwrap();
    let ai_commit = repo.stage_all_and_commit("AI commit").unwrap();

    // Step 3: Replace with old-format note
    let old_hash = "statustest123456";
    let old_note = format!(
        r#"status_test.txt
  {} 2-3
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "cursor", "id": "status_agent", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 2,
      "total_deletions": 0,
      "accepted_lines": 2,
      "overriden_lines": 0
    }}
  }}
}}"#,
        old_hash, ai_commit.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &ai_commit.commit_sha, &old_note).expect("attach old-format note");

    // Step 4: Reset --soft to bring content into working log with old-format INITIAL
    repo.git(&["reset", "--soft", "HEAD~1"]).unwrap();
    repo.sync_daemon_force();

    // Step 5: Run git ai status --json and check it counts AI lines from old-format INITIAL
    let status_output = repo.git_ai(&["status", "--json"]);
    assert!(
        status_output.is_ok(),
        "git ai status should work with old-format INITIAL"
    );
    let output = status_output.unwrap();
    let json: Value = serde_json::from_str(output.trim()).unwrap();

    // The status should report AI lines from the old-format INITIAL
    let ai_accepted = json["stats"]["ai_accepted"].as_u64().unwrap_or(0);
    assert!(
        ai_accepted >= 2,
        "status should count AI lines from old-format INITIAL (got {})",
        ai_accepted
    );
}

// Test 26: git-ai diff --json on a commit that has BOTH old prompts and new sessions
// (e.g. after amending an old-format commit with new-format checkpoints).
// Verifies that prompts go to "prompts", sessions go to "sessions", and stats
// correctly aggregate both.
#[test]
fn test_diff_json_mixed_format_commit_separates_prompts_and_sessions() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("mixed_diff.txt");

    // Step 1: Create base commit
    fs::write(&file_path, "base line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "mixed_diff.txt"])
        .unwrap();
    repo.stage_all_and_commit("Base commit").unwrap();

    // Step 2: Add AI content and commit
    fs::write(&file_path, "base line\nold ai line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "mixed_diff.txt"])
        .unwrap();
    let commit = repo.stage_all_and_commit("AI commit").unwrap();

    // Step 3: Replace note with old-format (simulating pre-upgrade git-ai)
    let old_hash = "oldfmt1234567890";
    let old_note = format!(
        r#"mixed_diff.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "cursor", "id": "old_session", "model": "gpt-4"}},
      "human_author": null,
      "messages": [],
      "total_additions": 5,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }}
}}"#,
        old_hash, commit.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &old_note).expect("attach old-format note");

    // Step 4: Amend with new-format AI content
    fs::write(&file_path, "base line\nold ai line\nnew ai line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "mixed_diff.txt"])
        .unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "--amend", "--no-edit"]).unwrap();

    // Step 5: Run diff --json --include-stats on the amended commit
    let sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let output = repo
        .git_ai(&["diff", &sha, "--json", "--include-stats"])
        .expect("diff --json --include-stats should succeed");
    let json: Value = serde_json::from_str(output.trim()).expect("diff JSON should parse");

    // Verify prompts contains old-format entry
    let prompts = json["prompts"]
        .as_object()
        .expect("prompts should be object");
    assert!(
        prompts.contains_key(old_hash),
        "prompts should contain old-format hash '{}', got keys: {:?}",
        old_hash,
        prompts.keys().collect::<Vec<_>>()
    );
    let old_prompt = &prompts[old_hash];
    assert_eq!(old_prompt["agent_id"]["tool"].as_str(), Some("cursor"));
    assert_eq!(old_prompt["agent_id"]["model"].as_str(), Some("gpt-4"));

    // Verify sessions contains new-format entry with s_::t_ key
    let sessions = json["sessions"]
        .as_object()
        .expect("sessions should be object");
    assert!(
        !sessions.is_empty(),
        "sessions should contain new-format entries"
    );
    let session_key = sessions.keys().next().unwrap();
    assert!(
        session_key.starts_with("s_") && !session_key.contains("::"),
        "session key should be session ID only (s_xxx), not combined ID (s_xxx::t_yyy), got: {}",
        session_key
    );
    let session = &sessions[session_key];
    assert_eq!(session["agent_id"]["tool"].as_str(), Some("mock_ai"));

    // Verify stats aggregate both formats
    let stats = json["commit_stats"]
        .as_object()
        .expect("commit_stats should exist");
    let ai_lines_added = stats["ai_lines_added"].as_u64().unwrap();
    assert_eq!(
        ai_lines_added, 2,
        "should count AI lines from both old prompt (1) and new session (1)"
    );

    // Verify tool_model_breakdown has entries from both formats
    let breakdown = stats["tool_model_breakdown"].as_object().unwrap();
    assert!(
        breakdown.contains_key("cursor::gpt-4"),
        "breakdown should have old-format tool::model, got: {:?}",
        breakdown.keys().collect::<Vec<_>>()
    );
    assert!(
        breakdown.contains_key("mock_ai::unknown"),
        "breakdown should have new-format tool::model, got: {:?}",
        breakdown.keys().collect::<Vec<_>>()
    );
}

// Test 27: git-ai diff --json across a history that has both old-format and new-format commits.
// Uses --all-prompts on each individual commit to verify correct routing, then also checks
// a plain range diff includes hunks from both.
#[test]
fn test_diff_json_history_with_mixed_old_and_new_format_commits() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("range_mixed.txt");

    // Step 1: Base commit
    fs::write(&file_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "range_mixed.txt"])
        .unwrap();
    let base = repo.stage_all_and_commit("Base").unwrap();

    // Step 2: Old-format AI commit
    fs::write(&file_path, "base\nold ai line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "range_mixed.txt"])
        .unwrap();
    let old_commit = repo.stage_all_and_commit("Old AI commit").unwrap();

    // Replace note with old-format
    let old_hash = "rangemix123456ab";
    let old_note = format!(
        r#"range_mixed.txt
  {} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "windsurf", "id": "old_range", "model": "claude-3.5"}},
      "human_author": null,
      "messages": [],
      "total_additions": 1,
      "total_deletions": 0,
      "accepted_lines": 1,
      "overriden_lines": 0
    }}
  }}
}}"#,
        old_hash, base.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &old_commit.commit_sha, &old_note).expect("attach old-format note");

    // Step 3: New-format AI commit
    fs::write(&file_path, "base\nold ai line\nnew session line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "range_mixed.txt"])
        .unwrap();
    let new_commit = repo.stage_all_and_commit("New AI commit").unwrap();

    // Verify old-format commit individually: prompts populated, sessions empty
    let old_json: Value = {
        let output = repo
            .git_ai(&["diff", &old_commit.commit_sha, "--json", "--all-prompts"])
            .expect("diff old commit should succeed");
        serde_json::from_str(output.trim()).unwrap()
    };
    let old_prompts = old_json["prompts"].as_object().expect("prompts obj");
    assert!(
        old_prompts.contains_key(old_hash),
        "old commit should have prompt hash '{}', got: {:?}",
        old_hash,
        old_prompts.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        old_prompts[old_hash]["agent_id"]["tool"].as_str(),
        Some("windsurf")
    );
    assert!(
        old_json["sessions"]
            .as_object()
            .is_none_or(|s| s.is_empty()),
        "old commit should have no sessions"
    );

    // Verify new-format commit individually: sessions populated, prompts empty
    let new_json: Value = {
        let output = repo
            .git_ai(&["diff", &new_commit.commit_sha, "--json", "--all-prompts"])
            .expect("diff new commit should succeed");
        serde_json::from_str(output.trim()).unwrap()
    };
    let new_sessions = new_json["sessions"].as_object().expect("sessions obj");
    assert!(
        !new_sessions.is_empty(),
        "new commit should have session entries"
    );
    let session_key = new_sessions.keys().next().unwrap();
    assert!(
        session_key.starts_with("s_") && !session_key.contains("::"),
        "session key should be session ID only (s_xxx), not combined ID (s_xxx::t_yyy), got: {}",
        session_key
    );
    assert!(
        new_json["prompts"].as_object().is_none_or(|p| p.is_empty()),
        "new commit should have no prompts"
    );

    // Verify plain range diff (no --all-prompts, no --include-stats) includes hunks from both
    let range = format!("{}..{}", base.commit_sha, new_commit.commit_sha);
    let range_json: Value = {
        let output = repo
            .git_ai(&["diff", &range, "--json"])
            .expect("range diff should succeed");
        serde_json::from_str(output.trim()).unwrap()
    };
    let commits = range_json["commits"].as_object().expect("commits obj");
    assert!(
        commits.len() >= 2,
        "range should span at least 2 commits, got {}",
        commits.len()
    );
    let hunks = range_json["hunks"].as_array().expect("hunks array");
    assert!(!hunks.is_empty(), "range should have hunks");
}

// Test 28: git-ai diff --json --include-stats on single commit with old-format note
// verifies stats still work correctly (tool_model_breakdown uses old prompt's tool::model)
#[test]
fn test_diff_json_stats_with_old_format_note_only() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("old_stats.txt");

    // Base commit
    fs::write(&file_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "old_stats.txt"])
        .unwrap();
    let base = repo.stage_all_and_commit("Base").unwrap();

    // AI commit
    fs::write(&file_path, "base\nai one\nai two\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "old_stats.txt"])
        .unwrap();
    let commit = repo.stage_all_and_commit("AI commit").unwrap();

    // Replace with old-format note (2 AI lines)
    let old_hash = "oldstats12345678";
    let old_note = format!(
        r#"old_stats.txt
  {} 2-3
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{}",
  "prompts": {{
    "{}": {{
      "agent_id": {{"tool": "copilot", "id": "stats_test", "model": "gpt-4o"}},
      "human_author": null,
      "messages": [],
      "total_additions": 10,
      "total_deletions": 3,
      "accepted_lines": 2,
      "overriden_lines": 0
    }}
  }}
}}"#,
        old_hash, base.commit_sha, old_hash
    );
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &commit.commit_sha, &old_note).expect("attach old-format note");

    // Run diff --json --include-stats
    let output = repo
        .git_ai(&["diff", &commit.commit_sha, "--json", "--include-stats"])
        .expect("diff --json --include-stats should succeed");
    let json: Value = serde_json::from_str(output.trim()).expect("diff JSON should parse");

    // Stats should reflect attestation-based data (2 AI lines), not checkpoint stats
    let stats = json["commit_stats"]
        .as_object()
        .expect("commit_stats should exist");
    assert_eq!(
        stats["ai_lines_added"].as_u64().unwrap(),
        2,
        "ai_lines_added should be 2 (from attestation line ranges, not total_additions=10)"
    );
    assert_eq!(
        stats["human_lines_added"].as_u64().unwrap(),
        0,
        "no human lines"
    );

    // tool_model_breakdown should use old-format prompt's tool::model
    let breakdown = stats["tool_model_breakdown"].as_object().unwrap();
    assert!(
        breakdown.contains_key("copilot::gpt-4o"),
        "breakdown should have copilot::gpt-4o from old-format prompt, got: {:?}",
        breakdown.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        breakdown["copilot::gpt-4o"]["ai_lines_added"]
            .as_u64()
            .unwrap(),
        2,
        "copilot::gpt-4o should have 2 AI lines"
    );

    // Prompts should be in prompts key, sessions should be empty
    let prompts = json["prompts"]
        .as_object()
        .expect("prompts should be object");
    assert!(
        prompts.contains_key(old_hash),
        "prompts should have old-format hash"
    );
    assert!(
        json["sessions"].as_object().is_none_or(|s| s.is_empty()),
        "sessions should be empty for old-format-only commit"
    );
}

// Regression: under the HTTP notes backend (notes live in the local notes-db,
// NOT in refs/notes/ai), amending a commit must preserve the sessions
// metadata on the rebuilt note. The amend pipeline re-reads the original note
// and session history through refs/notes/ai-only helpers
// (`refs::get_reference_as_authorship_log_v3`, `refs::grep_ai_notes`), which
// find nothing under the HTTP backend — so the amended note keeps its s_::t_
// attestation hashes but silently loses `metadata.sessions`, and downstream
// consumers bucket every AI line as tool=unknown.
#[test]
fn test_amend_preserves_sessions_under_http_notes_backend() {
    use git_ai::config::{ConfigPatch, NotesBackendConfig, NotesBackendKind};
    use git_ai::notes::db::NotesDatabase;

    // The daemon owns note writes and the amend rebuild, so the DAEMON must run
    // with the HTTP backend. The test-home config.json writer does not cover
    // notes_backend and the daemon caches config at startup, so pass the patch
    // via env at daemon spawn.
    let daemon_patch = ConfigPatch {
        exclude_prompts_in_repositories: Some(vec![]),
        prompt_storage: Some("notes".to_string()),
        notes_backend: Some(NotesBackendConfig {
            kind: NotesBackendKind::Http,
            backend_url: None,
        }),
        ..Default::default()
    };
    let daemon_patch_json =
        serde_json::to_string(&daemon_patch).expect("serialize daemon config patch");
    // `dirs::home_dir()` does not honor HOME/USERPROFILE overrides on Windows,
    // so explicitly isolate the daemon's HTTP notes cache at a path this test
    // can read on every platform.
    let notes_db_dir = tempfile::tempdir().expect("create isolated notes-db directory");
    let notes_db_path = notes_db_dir.path().join("notes-db");
    let notes_db_path_string = notes_db_path.to_string_lossy().to_string();
    let mut repo = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_TEST_CONFIG_PATCH", daemon_patch_json.as_str()),
        ("GIT_AI_TEST_NOTES_DB_PATH", notes_db_path_string.as_str()),
    ]);
    // CLI invocations (checkpoint, blame) should use the HTTP backend too.
    repo.patch_git_ai_config(|patch| {
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::Http,
            backend_url: None,
        });
    });

    // Poll the notes-db for a commit's note: post-commit note writes land in the
    // daemon's notes-db queue (never refs/notes/ai), so the harness's usual
    // "note visible in refs/notes/ai" commit assertion cannot be used here.
    let read_note_from_db = |sha: &str| -> Option<String> {
        for _ in 0..100 {
            if let Ok(db) = NotesDatabase::open_at_path(&notes_db_path)
                && let Ok(Some(content)) = db.get_note(sha)
            {
                return Some(content);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        None
    };

    let file_path = repo.path().join("http_amend.txt");
    fs::write(&file_path, "Human line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "http_amend.txt"])
        .unwrap();
    fs::write(&file_path, "Human line\nAI line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "http_amend.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-m", "AI commit"]).unwrap();
    repo.sync_daemon();
    let original_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    let original_note =
        read_note_from_db(&original_sha).expect("original commit should have a note in notes-db");
    // Under the HTTP backend the note must be in notes-db, not refs/notes/ai.
    assert!(
        repo.read_authorship_note(&original_sha).is_none(),
        "HTTP backend should not write to refs/notes/ai"
    );
    let original_log =
        AuthorshipLog::deserialize_from_string(&original_note).expect("parse original note");
    assert!(
        !original_log.metadata.sessions.is_empty(),
        "original note should carry sessions metadata"
    );

    // Amend the commit message only — the attributed content is unchanged, so
    // the rebuilt note must still attest the AI line to the same session.
    repo.git(&["commit", "--amend", "-m", "Amended commit"])
        .unwrap();
    repo.sync_daemon();
    let amended_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(amended_sha, original_sha, "amend should rewrite HEAD");

    let amended_note =
        read_note_from_db(&amended_sha).expect("amended commit should have a note in notes-db");
    let amended_log =
        AuthorshipLog::deserialize_from_string(&amended_note).expect("parse amended note");

    // The AI line's attestation must still use the session format...
    let has_session_attestation = amended_log
        .attestations
        .iter()
        .flat_map(|fa| fa.entries.iter())
        .any(|entry| entry.hash.starts_with("s_"));
    assert!(
        has_session_attestation,
        "amended note should still attest AI lines to a session hash:\n{}",
        amended_note
    );

    // ...and the sessions map those hashes resolve through must survive the amend.
    assert!(
        !amended_log.metadata.sessions.is_empty(),
        "amended note lost metadata.sessions — session attestations no longer resolve to a tool:\n{}",
        amended_note
    );

    // The surviving record must be the same session as the original note.
    for session_id in original_log.metadata.sessions.keys() {
        assert!(
            amended_log.metadata.sessions.contains_key(session_id),
            "session {} from the original note is missing after amend",
            session_id
        );
    }
}
