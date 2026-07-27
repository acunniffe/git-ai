use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::attribution_tracker::LineAttribution;
use git_ai::authorship::authorship_log::{HumanRecord, PromptRecord, SessionRecord};
use git_ai::authorship::working_log::AgentId;
use git_ai::git::repo_storage::InitialAttributions;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;

fn stash_v2_dir(repo: &TestRepo) -> PathBuf {
    repo.path().join(".git").join("ai").join("stashes_v2")
}

fn single_stash_v2_initial(repo: &TestRepo) -> InitialAttributions {
    let stashes = stash_v2_dir(repo);
    let stash_dir = fs::read_dir(&stashes)
        .expect("stashes_v2 dir exists")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .expect("a compact stash dir should exist");
    let initial = fs::read_to_string(stash_dir.join("INITIAL")).expect("stash INITIAL exists");
    serde_json::from_str(&initial).expect("stash INITIAL is valid")
}

fn current_checkpoint_files(repo: &TestRepo) -> BTreeSet<String> {
    repo.current_working_logs()
        .read_all_checkpoints()
        .expect("read current checkpoints")
        .into_iter()
        .flat_map(|checkpoint| checkpoint.entries.into_iter().map(|entry| entry.file))
        .collect()
}

fn joke_lines(file_idx: usize, count: usize) -> Vec<String> {
    (0..count)
        .map(|line_idx| format!("joke file {file_idx} line {line_idx}: boilerplate punchline"))
        .collect()
}

fn lines_to_content(lines: &[String]) -> String {
    let mut content = lines.join("\n");
    content.push('\n');
    content
}

fn test_agent(id: &str) -> AgentId {
    AgentId {
        tool: "test".to_string(),
        id: id.to_string(),
        model: "test-model".to_string(),
    }
}

fn test_prompt(id: &str) -> PromptRecord {
    PromptRecord {
        agent_id: test_agent(id),
        human_author: None,
        messages_url: None,
        total_additions: 0,
        total_deletions: 0,
        accepted_lines: 0,
        overriden_lines: 0,
        custom_attributes: None,
    }
}

#[test]
fn test_stash_pop_with_ai_attribution() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create a file with AI attribution
    let mut example = repo.filename("example.txt");
    example.set_contents(vec!["line 1".ai(), "line 2".ai(), "line 3".ai()]);

    // Run checkpoint to track AI attribution
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash the changes
    repo.git(&["stash", "push", "-m", "test stash"])
        .expect("stash should succeed");

    // Verify file is gone
    assert!(repo.read_file("example.txt").is_none());

    // Pop the stash
    repo.git(&["stash", "pop"])
        .expect("stash pop should succeed");

    // Verify file is back
    assert!(repo.read_file("example.txt").is_some());

    // Commit the changes
    let commit = repo
        .stage_all_and_commit("apply stashed changes")
        .expect("commit should succeed");

    // Verify AI attribution is preserved
    example.assert_lines_and_blame(vec!["line 1".ai(), "line 2".ai(), "line 3".ai()]);

    // Check authorship log has AI prompts
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log"
    );
}

#[test]
fn test_stash_apply_with_ai_attribution() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create a file with AI attribution
    let mut example = repo.filename("example.txt");
    example.set_contents(vec!["line 1".ai(), "line 2".ai()]);

    // Run checkpoint to track AI attribution
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash the changes
    repo.git(&["stash"]).expect("stash should succeed");

    // Apply (not pop) the stash
    repo.git(&["stash", "apply"])
        .expect("stash apply should succeed");

    // Commit the changes
    let commit = repo
        .stage_all_and_commit("apply stashed changes")
        .expect("commit should succeed");

    // Verify AI attribution is preserved
    example.assert_lines_and_blame(vec!["line 1".ai(), "line 2".ai()]);

    // Check authorship log has AI prompts
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log"
    );
}

#[test]
fn test_stash_apply_named_reference() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create first stash
    let mut file1 = repo.filename("file1.txt");
    file1.set_contents(vec!["first stash".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");
    repo.git(&["stash"]).expect("first stash should succeed");

    // Create second stash
    let mut file2 = repo.filename("file2.txt");
    file2.set_contents(vec!["second stash".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");
    repo.git(&["stash"]).expect("second stash should succeed");

    // Apply the first stash (stash@{1})
    repo.git(&["stash", "apply", "stash@{1}"])
        .expect("stash apply stash@{1} should succeed");

    // Verify file1 is back
    assert!(repo.read_file("file1.txt").is_some());
    assert!(repo.read_file("file2.txt").is_none());

    // Commit and verify attribution
    let commit = repo
        .stage_all_and_commit("apply first stash")
        .expect("commit should succeed");

    file1.assert_lines_and_blame(vec!["first stash".ai()]);

    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log"
    );
}

#[test]
fn test_stash_pop_with_existing_stack_entries() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    let mut first = repo.filename("first.txt");
    first.set_contents(vec!["first stash line".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");
    repo.git(&["stash", "push", "-m", "first"])
        .expect("first stash should succeed");

    let mut second = repo.filename("second.txt");
    second.set_contents(vec!["second stash line".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");
    repo.git(&["stash", "push", "-m", "second"])
        .expect("second stash should succeed");

    // Pop when stash stack still has another entry (non-empty -> non-empty on some Git versions).
    repo.git(&["stash", "pop"])
        .expect("first pop should succeed");
    let first_pop_commit = repo
        .stage_all_and_commit("apply top stash entry")
        .expect("commit after first pop should succeed");

    second.assert_lines_and_blame(vec!["second stash line".ai()]);
    assert!(
        !first_pop_commit.authorship_log.metadata.sessions.is_empty(),
        "expected sessions for first pop commit"
    );

    // Pop remaining stash entry and verify attribution still restores correctly.
    repo.git(&["stash", "pop"])
        .expect("second pop should succeed");
    let second_pop_commit = repo
        .stage_all_and_commit("apply remaining stash entry")
        .expect("commit after second pop should succeed");

    first.assert_lines_and_blame(vec!["first stash line".ai()]);
    assert!(
        !second_pop_commit
            .authorship_log
            .metadata
            .sessions
            .is_empty(),
        "expected sessions for second pop commit"
    );
}

#[test]
fn test_stash_multiple_files() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create multiple files with AI attribution
    let mut file1 = repo.filename("file1.txt");
    file1.set_contents(vec!["file 1 line 1".ai(), "file 1 line 2".ai()]);

    let mut file2 = repo.filename("file2.txt");
    file2.set_contents(vec!["file 2 line 1".ai(), "file 2 line 2".ai()]);

    let mut file3 = repo.filename("file3.txt");
    file3.set_contents(vec!["file 3 line 1".ai()]);

    // Run checkpoint to track AI attribution
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash all changes
    repo.git(&["stash"]).expect("stash should succeed");

    // Verify files are gone
    assert!(repo.read_file("file1.txt").is_none());
    assert!(repo.read_file("file2.txt").is_none());
    assert!(repo.read_file("file3.txt").is_none());

    // Pop the stash
    repo.git(&["stash", "pop"])
        .expect("stash pop should succeed");

    // Commit all files
    let commit = repo
        .stage_all_and_commit("apply multi-file stash")
        .expect("commit should succeed");

    // Verify all files have AI attribution
    file1.assert_lines_and_blame(vec!["file 1 line 1".ai(), "file 1 line 2".ai()]);
    file2.assert_lines_and_blame(vec!["file 2 line 1".ai(), "file 2 line 2".ai()]);
    file3.assert_lines_and_blame(vec!["file 3 line 1".ai()]);

    // Check authorship log has the files
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log"
    );
    assert_eq!(
        commit.authorship_log.attestations.len(),
        3,
        "Expected 3 files in authorship log"
    );
}

#[test]
fn test_stash_with_existing_initial_attributions() {
    // Test that stash attributions merge correctly with existing INITIAL attributions
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create a file and commit it (this will have some attribution)
    let example_path = repo.path().join("example.txt");
    fs::write(&example_path, "existing line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "example.txt"])
        .unwrap();
    let mut example = repo.filename("example.txt");
    let _first_commit = repo
        .stage_all_and_commit("add example")
        .expect("commit should succeed");

    // Modify the file with AI
    example.set_contents(vec!["existing line".human(), "new AI line".ai()]);

    // Run checkpoint
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash the changes
    repo.git(&["stash"]).expect("stash should succeed");

    // Verify file reverted to original
    let content = repo.read_file("example.txt").expect("file should exist");
    assert_eq!(content.lines().count(), 1, "Should have reverted to 1 line");

    // Pop the stash
    repo.git(&["stash", "pop"])
        .expect("stash pop should succeed");

    // Commit
    let commit = repo
        .stage_all_and_commit("apply stash")
        .expect("commit should succeed");

    // Verify mixed attribution
    example.assert_lines_and_blame(vec!["existing line".human(), "new AI line".ai()]);

    // Should have both human and AI in authorship
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log"
    );
}

#[test]
fn test_stash_pop_default_reference() {
    // Test that stash pop defaults to stash@{0}
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create AI content
    let mut example = repo.filename("example.txt");
    example.set_contents(vec!["AI content".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash without explicit reference
    repo.git(&["stash"]).expect("stash should succeed");

    // Pop without explicit reference (should use stash@{0})
    repo.git(&["stash", "pop"])
        .expect("stash pop should succeed");

    // Commit and verify
    let commit = repo
        .stage_all_and_commit("apply default stash")
        .expect("commit should succeed");

    example.assert_lines_and_blame(vec!["AI content".ai()]);

    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log"
    );
}

#[test]
fn test_stash_pop_empty_repo() {
    // Test that stash operations don't crash on edge cases
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Try to pop when there's no stash - should fail gracefully
    let result = repo.git(&["stash", "pop"]);
    assert!(result.is_err(), "Should fail when no stash exists");
}

#[test]
fn test_stash_mixed_human_and_ai() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create file with mixed attribution
    let mut example = repo.filename("example.txt");
    example.set_contents(vec![
        "line 1".human(),
        "line 2".ai(),
        "line 3".human(),
        "line 4".ai(),
    ]);

    // Run checkpoint
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash and pop
    repo.git(&["stash"]).expect("stash should succeed");
    repo.git(&["stash", "pop"])
        .expect("stash pop should succeed");

    // Commit
    let commit = repo
        .stage_all_and_commit("mixed content")
        .expect("commit should succeed");

    // Verify blame shows mixed attribution
    example.assert_lines_and_blame(vec![
        "line 1".human(),
        "line 2".ai(),
        "line 3".human(),
        "line 4".ai(),
    ]);

    // Authorship log should have AI prompts
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log"
    );
}

#[test]
fn test_stash_push_with_pathspec_single_file() {
    // Test git stash push -- file.txt only stashes that file
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create two files with AI content
    let mut file1 = repo.filename("file1.txt");
    file1.set_contents(vec!["file1 line 1".ai(), "file1 line 2".ai()]);

    let mut file2 = repo.filename("file2.txt");
    file2.set_contents(vec!["file2 line 1".ai(), "file2 line 2".ai()]);

    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash only file1.txt
    repo.git(&["stash", "push", "--", "file1.txt"])
        .expect("stash push should succeed");

    // file1 should be gone, file2 should still exist
    assert!(repo.read_file("file1.txt").is_none());
    assert!(repo.read_file("file2.txt").is_some());

    // Pop the stash
    repo.git(&["stash", "pop"])
        .expect("stash pop should succeed");

    // Now file1 is back
    assert!(repo.read_file("file1.txt").is_some());

    // Commit everything
    let commit = repo
        .stage_all_and_commit("apply partial stash")
        .expect("commit should succeed");

    // Both files should have AI attribution
    file1.assert_lines_and_blame(vec!["file1 line 1".ai(), "file1 line 2".ai()]);
    file2.assert_lines_and_blame(vec!["file2 line 1".ai(), "file2 line 2".ai()]);

    // Should have AI prompts
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log"
    );
}

#[test]
fn test_stash_push_with_pathspec_directory() {
    // Test git stash push -- dir/ only stashes that directory
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create files in a directory and root
    let mut root_file = repo.filename("root.txt");
    root_file.set_contents(vec!["root line 1".ai()]);

    // Create src directory
    std::fs::create_dir_all(repo.path().join("src")).expect("Failed to create src dir");

    let mut dir_file1 = repo.filename("src/file1.txt");
    dir_file1.set_contents(vec!["src file1 line 1".ai()]);

    let mut dir_file2 = repo.filename("src/file2.txt");
    dir_file2.set_contents(vec!["src file2 line 1".ai()]);

    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash only src/ directory
    repo.git(&["stash", "push", "--", "src/"])
        .expect("stash push should succeed");

    // src files should be gone, root file should remain
    assert!(repo.read_file("src/file1.txt").is_none());
    assert!(repo.read_file("src/file2.txt").is_none());
    assert!(repo.read_file("root.txt").is_some());

    // Pop the stash
    repo.git(&["stash", "pop"])
        .expect("stash pop should succeed");

    // Commit everything
    let commit = repo
        .stage_all_and_commit("apply directory stash")
        .expect("commit should succeed");

    // All files should have AI attribution
    root_file.assert_lines_and_blame(vec!["root line 1".ai()]);
    dir_file1.assert_lines_and_blame(vec!["src file1 line 1".ai()]);
    dir_file2.assert_lines_and_blame(vec!["src file2 line 1".ai()]);

    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log"
    );
}

#[test]
fn test_stash_push_multiple_pathspecs() {
    // Test git stash push -- file1.txt file2.txt
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create three files with AI content
    let mut file1 = repo.filename("file1.txt");
    file1.set_contents(vec!["file1".ai()]);

    let mut file2 = repo.filename("file2.txt");
    file2.set_contents(vec!["file2".ai()]);

    let mut file3 = repo.filename("file3.txt");
    file3.set_contents(vec!["file3".ai()]);

    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash only file1 and file2
    repo.git(&["stash", "push", "--", "file1.txt", "file2.txt"])
        .expect("stash push should succeed");

    // file1 and file2 should be gone, file3 remains
    assert!(repo.read_file("file1.txt").is_none());
    assert!(repo.read_file("file2.txt").is_none());
    assert!(repo.read_file("file3.txt").is_some());

    // Pop the stash
    repo.git(&["stash", "pop"])
        .expect("stash pop should succeed");

    // Commit everything
    let commit = repo
        .stage_all_and_commit("apply multi-pathspec stash")
        .expect("commit should succeed");

    // All files should have AI attribution
    file1.assert_lines_and_blame(vec!["file1".ai()]);
    file2.assert_lines_and_blame(vec!["file2".ai()]);
    file3.assert_lines_and_blame(vec!["file3".ai()]);

    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log"
    );
}

#[test]
fn test_stash_pop_with_conflict() {
    // Test that attribution is preserved when there's a conflict during stash pop
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create a file with mixed human and AI content
    let mut example = repo.filename("example.txt");
    example.set_contents(vec![
        "header".human(),
        "line 1 AI".ai(),
        "line 2 AI".ai(),
        "footer".human(),
    ]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash the changes
    repo.git(&["stash"]).expect("stash should succeed");

    // Now create a conflicting version with different mixed content
    example.set_contents(vec![
        "header".human(),
        "line 1 different".ai(),
        "line 2 different".ai(),
        "footer".human(),
    ]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");
    repo.stage_all_and_commit("conflicting changes")
        .expect("commit should succeed");

    // Try to pop - this WILL create a conflict
    let _result = repo.git(&["stash", "pop"]);

    // Verify there's a conflict
    let content = repo.read_file("example.txt").expect("file should exist");
    assert!(
        content.contains("<<<<<<<"),
        "Expected conflict markers in file, got: {}",
        content
    );

    // Manually resolve the conflict by taking parts from both versions
    example.set_contents(vec![
        "header".human(),        // from both (same)
        "line 1 AI".ai(),        // from stash
        "line 2 different".ai(), // from HEAD
        "footer".human(),        // from both (same)
    ]);

    // Mark as resolved and commit
    repo.git(&["add", "example.txt"])
        .expect("should be able to add resolved file");

    let _commit = repo
        .stage_all_and_commit("resolved conflict")
        .expect("commit should succeed");

    // Verify mixed human and AI attributions are preserved
    example.assert_lines_and_blame(vec![
        "header".human(),
        "line 1 AI".ai(),
        "line 2 different".ai(),
        "footer".human(),
    ]);
}

#[test]
fn test_stash_mixed_staged_and_unstaged() {
    // Test stashing with a mix of staged and unstaged changes
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create a file with AI content
    let mut example = repo.filename("example.txt");
    example.set_contents(vec!["staged line 1".ai(), "staged line 2".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stage these changes
    repo.git(&["add", "example.txt"])
        .expect("should stage example.txt");

    // Now add more unstaged changes
    example.set_contents(vec![
        "staged line 1".ai(),
        "staged line 2".ai(),
        "unstaged line 3".ai(),
        "unstaged line 4".ai(),
    ]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash both staged and unstaged (git stash by default stashes both)
    repo.git(&["stash", "--include-untracked"])
        .expect("stash should succeed");

    // Verify file is back to original state (doesn't exist)
    assert!(repo.read_file("example.txt").is_none());

    // Pop the stash
    repo.git(&["stash", "pop"])
        .expect("stash pop should succeed");

    // Commit all changes
    let commit = repo
        .stage_all_and_commit("apply mixed stash")
        .expect("commit should succeed");

    // All lines should have AI attribution preserved (both staged and unstaged)
    example.assert_lines_and_blame(vec![
        "staged line 1".ai(),
        "staged line 2".ai(),
        "unstaged line 3".ai(),
        "unstaged line 4".ai(),
    ]);

    // Should have AI prompts
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log"
    );
}

#[test]
fn test_stash_pop_onto_head_with_ai_changes() {
    // Test that popping stash onto a HEAD with AI changes preserves both attributions
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create file1 with AI content from first session
    let mut file1 = repo.filename("file1.txt");
    file1.set_contents(vec!["file1 line 1".ai(), "file1 line 2".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash file1
    repo.git(&["stash"]).expect("stash should succeed");
    assert!(repo.read_file("file1.txt").is_none());

    // Now create file2 with AI content and commit it to HEAD
    let mut file2 = repo.filename("file2.txt");
    file2.set_contents(vec![
        "file2 line 1".ai(),
        "file2 line 2".ai(),
        "file2 line 3".ai(),
    ]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    let head_commit = repo
        .stage_all_and_commit("add file2 with AI")
        .expect("commit should succeed");

    // Verify HEAD has AI attribution
    file2.assert_lines_and_blame(vec![
        "file2 line 1".ai(),
        "file2 line 2".ai(),
        "file2 line 3".ai(),
    ]);
    assert!(
        !head_commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in HEAD commit"
    );

    // Pop the stash (file1 with AI attribution from stash)
    repo.git(&["stash", "pop"])
        .expect("stash pop should succeed");

    // Commit the popped changes
    let final_commit = repo
        .stage_all_and_commit("apply stash onto HEAD with AI")
        .expect("commit should succeed");

    // Verify BOTH files maintain their AI attributions:
    // file1 should have AI attribution from the stash
    file1.assert_lines_and_blame(vec!["file1 line 1".ai(), "file1 line 2".ai()]);

    // file2 should STILL have AI attribution (unchanged from HEAD)
    file2.assert_lines_and_blame(vec![
        "file2 line 1".ai(),
        "file2 line 2".ai(),
        "file2 line 3".ai(),
    ]);

    // The authorship log should track file1 (the new changes from stash)
    // file2 should already be in the repo from the previous commit
    assert!(
        final_commit
            .authorship_log
            .attestations
            .iter()
            .any(|a| a.file_path.ends_with("file1.txt")),
        "Expected file1.txt in authorship log"
    );
}

#[test]
fn test_stash_pop_across_branches() {
    // Test that AI attributions are preserved when stashing, switching branches, and popping
    let repo = TestRepo::new();

    // Create initial commit on main branch
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create a file with existing human content
    let example_path = repo.path().join("example.txt");
    fs::write(&example_path, "line 1\nline 2\nline 3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "example.txt"])
        .unwrap();
    let mut example = repo.filename("example.txt");
    repo.stage_all_and_commit("add example file")
        .expect("commit should succeed");

    // Add 5 AI-generated lines at the bottom
    example.set_contents(vec![
        "line 1".human(),
        "line 2".human(),
        "line 3".human(),
        "AI line 1".ai(),
        "AI line 2".ai(),
        "AI line 3".ai(),
        "AI line 4".ai(),
        "AI line 5".ai(),
    ]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash the AI changes
    repo.git(&["stash"]).expect("stash should succeed");

    // Verify file reverted to 3 lines
    let content = repo.read_file("example.txt").expect("file should exist");
    assert_eq!(
        content.lines().count(),
        3,
        "Should have reverted to 3 lines"
    );

    // Create and checkout a new branch
    repo.git(&["checkout", "-b", "feature-branch"])
        .expect("should create and checkout new branch");

    // Pop the stash on the new branch
    repo.git(&["stash", "pop"])
        .expect("stash pop should succeed");

    // Commit the changes on the new branch
    let commit = repo
        .stage_all_and_commit("apply AI changes on feature branch")
        .expect("commit should succeed");

    // Verify all AI attributions are preserved
    example.assert_lines_and_blame(vec![
        "line 1".human(),
        "line 2".human(),
        "line 3".human(),
        "AI line 1".ai(),
        "AI line 2".ai(),
        "AI line 3".ai(),
        "AI line 4".ai(),
        "AI line 5".ai(),
    ]);

    // Should have AI prompts in authorship log
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log"
    );
}

#[test]
fn test_stash_pop_across_branches_with_conflict() {
    // Test that AI attributions are preserved when resolving conflicts after stash pop across branches
    let repo = TestRepo::new();

    // Create initial commit on main branch
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create a file with existing content
    let example_path = repo.path().join("example.txt");
    fs::write(&example_path, "line 1\nline 2\nline 3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "example.txt"])
        .unwrap();
    let mut example = repo.filename("example.txt");
    repo.stage_all_and_commit("add example file")
        .expect("commit should succeed");

    // Add 5 AI-generated lines at the bottom
    example.set_contents(vec![
        "line 1".human(),
        "line 2".human(),
        "line 3".human(),
        "AI line 1".ai(),
        "AI line 2".ai(),
        "AI line 3".ai(),
        "AI line 4".ai(),
        "AI line 5".ai(),
    ]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash the AI changes
    repo.git(&["stash"]).expect("stash should succeed");

    // Create and checkout a new branch
    repo.git(&["checkout", "-b", "feature-branch"])
        .expect("should create and checkout new branch");

    // Make conflicting changes on the new branch (add different content at the bottom)
    example.set_contents(vec![
        "line 1".human(),
        "line 2".human(),
        "line 3".human(),
        "feature line 1".ai(),
        "feature line 2".ai(),
    ]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");
    repo.stage_all_and_commit("add feature content")
        .expect("commit should succeed");

    // Try to pop the stash - this will create a conflict
    let _result = repo.git(&["stash", "pop"]);

    // Verify there's a conflict
    let content = repo.read_file("example.txt").expect("file should exist");
    assert!(
        content.contains("<<<<<<<") || content.contains(">>>>>>>"),
        "Expected conflict markers in file"
    );

    // Resolve the conflict by keeping both (feature branch lines + stashed AI lines)
    example.set_contents(vec![
        "line 1".human(),
        "line 2".human(),
        "line 3".human(),
        "feature line 1".ai(),
        "feature line 2".ai(),
        "AI line 1".ai(),
        "AI line 2".ai(),
        "AI line 3".ai(),
        "AI line 4".ai(),
        "AI line 5".ai(),
    ]);

    // Mark as resolved and commit
    repo.git(&["add", "example.txt"])
        .expect("should be able to add resolved file");

    let commit = repo
        .stage_all_and_commit("resolved conflict keeping both changes")
        .expect("commit should succeed");

    // Verify all AI attributions are preserved for both sets of changes
    example.assert_lines_and_blame(vec![
        "line 1".human(),
        "line 2".human(),
        "line 3".human(),
        "feature line 1".ai(),
        "feature line 2".ai(),
        "AI line 1".ai(),
        "AI line 2".ai(),
        "AI line 3".ai(),
        "AI line 4".ai(),
        "AI line 5".ai(),
    ]);

    // Should have AI prompts in authorship log
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log"
    );
}

#[test]
fn test_stash_apply_reset_apply_again() {
    // Test that AI attributions survive multiple apply/reset cycles
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create a file with AI content
    let mut example = repo.filename("example.txt");
    example.set_contents(vec!["AI line 1".ai(), "AI line 2".ai(), "AI line 3".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash the changes (using regular stash, not apply, so we can test the workflow)
    repo.git(&["stash"]).expect("stash should succeed");
    assert!(repo.read_file("example.txt").is_none());

    // Apply the stash (NOT pop, so it stays in the stash list)
    repo.git(&["stash", "apply", "stash@{0}"])
        .expect("stash apply should succeed");
    assert!(repo.read_file("example.txt").is_some());

    // Reset to undo the apply
    repo.git(&["reset", "--hard"])
        .expect("reset should succeed");
    assert!(repo.read_file("example.txt").is_none());

    // Apply the same stash again
    repo.git(&["stash", "apply", "stash@{0}"])
        .expect("second stash apply should succeed");
    assert!(repo.read_file("example.txt").is_some());

    // Commit the changes
    let commit = repo
        .stage_all_and_commit("apply stash after reset")
        .expect("commit should succeed");

    // Verify AI attribution is preserved after multiple apply/reset cycles
    example.assert_lines_and_blame(vec!["AI line 1".ai(), "AI line 2".ai(), "AI line 3".ai()]);

    // Check authorship log has AI prompts
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log after multiple apply/reset cycles"
    );
}

#[test]
fn test_stash_branch_preserves_ai_attribution() {
    // ISSUE-009: git stash branch loses AI attribution
    // git stash branch creates a new branch at the stash parent, applies the stash, and drops it.
    // The post_stash_hook must handle the "branch" subcommand to restore attribution.
    //
    // Key: we make a commit AFTER stashing so HEAD advances. git stash branch then
    // resets HEAD to the stash parent, so the working log keyed to the advanced HEAD
    // is irrelevant. Only the stash note (refs/notes/ai-stash) can provide attribution.
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create a file with AI attribution
    let mut example = repo.filename("example.txt");
    example.set_contents(vec!["ai line 1".ai(), "ai line 2".ai(), "ai line 3".ai()]);

    // Run checkpoint to track AI attribution
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash the changes
    repo.git(&["stash", "push", "-m", "ai-work"])
        .expect("stash should succeed");

    // Verify file is gone
    assert!(repo.read_file("example.txt").is_none());

    // Make a commit to advance HEAD past the stash parent.
    // This ensures that git stash branch will reset HEAD to the stash parent,
    // invalidating any working log entries keyed to the current HEAD.
    let mut other = repo.filename("other.txt");
    other.set_contents(vec!["some other work".human()]);
    repo.stage_all_and_commit("advance HEAD past stash parent")
        .expect("commit should succeed");

    // Use git stash branch to create a new branch from the stash.
    // This resets HEAD to the stash parent commit and applies the stash.
    repo.git(&["stash", "branch", "new-feature", "stash@{0}"])
        .expect("stash branch should succeed");

    // Verify file is back on the new branch
    assert!(
        repo.read_file("example.txt").is_some(),
        "example.txt should exist after stash branch"
    );

    // Commit the changes on the new branch
    let commit = repo
        .stage_all_and_commit("apply stash via branch")
        .expect("commit should succeed");

    // Verify AI attribution is preserved
    example.assert_lines_and_blame(vec!["ai line 1".ai(), "ai line 2".ai(), "ai line 3".ai()]);

    // Check authorship log has AI prompts
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log after stash branch"
    );
}

#[test]
fn test_stash_pop_conflict_preserves_ai_attribution_without_new_checkpoint() {
    // ISSUE-010: git stash pop with conflict loses all AI attribution
    // When git stash pop encounters a conflict, git exits with code 1.
    // The post_stash_hook bails on !exit_status.success(), never restoring attribution.
    // This test resolves conflicts by writing files directly (no checkpoint),
    // so it verifies the stash attribution was properly restored by the hook.
    let repo = TestRepo::new();

    // Create initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit")
        .expect("commit should succeed");

    // Create a file with human content and commit it
    let mut conflict_file = repo.filename("conflict.txt");
    conflict_file.set_contents(vec!["original line".human()]);
    repo.stage_all_and_commit("add conflict file")
        .expect("commit should succeed");

    // AI edits the file (adds lines)
    conflict_file.set_contents(vec![
        "original line".human(),
        "ai addition 1".ai(),
        "ai addition 2".ai(),
        "ai addition 3".ai(),
    ]);
    repo.git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Stash the AI changes
    repo.git(&["stash", "push", "-m", "ai-changes"])
        .expect("stash should succeed");

    // Make a conflicting human commit on the same file
    // Write the file directly to avoid creating AI checkpoints
    std::fs::write(
        repo.path().join("conflict.txt"),
        "original line\nhuman edit on same file\n",
    )
    .expect("write should succeed");
    repo.git(&["add", "-A"]).expect("add should succeed");
    repo.git_ai(&["checkpoint", "--"])
        .expect("human checkpoint should succeed");
    repo.stage_all_and_commit("human conflicting edit")
        .expect("commit should succeed");

    // Try to pop the stash - this will create a conflict (exit code 1)
    let result = repo.git(&["stash", "pop"]);
    // stash pop with conflict returns error
    assert!(result.is_err(), "stash pop should fail due to conflict");

    // Verify there are conflict markers
    let content = repo.read_file("conflict.txt").expect("file should exist");
    assert!(
        content.contains("<<<<<<<") || content.contains(">>>>>>>"),
        "Expected conflict markers in file, got: {}",
        content
    );

    // Resolve the conflict manually by writing the file directly (NO checkpoint, NO set_contents)
    // This simulates a user resolving conflict in their editor without AI assistance
    std::fs::write(
        repo.path().join("conflict.txt"),
        "original line\nhuman edit on same file\nai addition 1\nai addition 2\nai addition 3\n",
    )
    .expect("write should succeed");

    // Mark as resolved and commit
    repo.git(&["add", "conflict.txt"])
        .expect("should be able to add resolved file");

    let commit = repo
        .stage_all_and_commit("resolved conflict")
        .expect("commit should succeed");

    // The AI lines from the stash should still be attributed to AI
    // This will fail if the post_stash_hook bailed on exit code 1
    // and never restored attribution from refs/notes/ai-stash
    conflict_file.assert_lines_and_blame(vec![
        "original line".human(),
        "human edit on same file".human(),
        "ai addition 1".ai(),
        "ai addition 2".ai(),
        "ai addition 3".ai(),
    ]);

    assert!(
        commit
            .authorship_log
            .attestations
            .iter()
            .any(|a| a.file_path.ends_with("conflict.txt")),
        "Expected conflict.txt in authorship log attestations - stash attribution was not restored"
    );

    // Check that AI prompts are present (from the stash attribution)
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log - stash attribution was lost due to conflict exit code"
    );
}

#[test]
fn test_stash_apply_shift_uses_final_commit_tree_after_later_edit() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("example.txt");

    fs::write(&file_path, "root\nanchor\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    fs::write(&file_path, "root\nAI stashed\nanchor\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.txt"])
        .unwrap();
    repo.git(&["stash", "push", "-m", "ai stash"])
        .expect("stash should succeed");

    fs::write(&file_path, "root\nanchor\ntarget human\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "example.txt"])
        .unwrap();
    repo.stage_all_and_commit("target head change").unwrap();

    repo.git(&["stash", "apply"])
        .expect("stash apply should succeed");
    fs::write(
        &file_path,
        "root\nAI stashed\nanchor\ntarget human\nlate untracked\n",
    )
    .unwrap();
    repo.git(&["add", "example.txt"]).unwrap();
    repo.commit("commit applied stash with later edit").unwrap();

    let mut file = repo.filename("example.txt");
    file.assert_committed_lines(crate::lines![
        "root".unattributed_human(),
        "AI stashed".ai(),
        "anchor".unattributed_human(),
        "target human".human(),
        "late untracked".unattributed_human(),
    ]);
}

/// Regression: on case-insensitive filesystems (macOS/Windows), the shift-path
/// reconstruction (`reconstruct_stash_applied_contents`) checked out the target
/// tree with `checkout-index -a` (no `-f`). If that tree contained a case-colliding
/// pair (e.g. `README.md` and `readme.md`), git aborted the second checkout with
/// "already exists, no checkout" (exit 1), and `require_success` zeroed out the
/// entire stash attribution restore -- silently dropping the AI note.
#[test]
fn test_stash_apply_shift_survives_case_colliding_target_tree() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("example.txt");

    fs::write(&file_path, "root\nanchor\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Stash an AI change against the current base.
    fs::write(&file_path, "root\nAI stashed\nanchor\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.txt"])
        .unwrap();
    repo.git(&["stash", "push", "-m", "ai stash"])
        .expect("stash should succeed");

    // Advance HEAD (so base_commit != current_head => shift path) and give the
    // target tree a case-colliding pair via plumbing. The working tree can't hold
    // both casings on a case-insensitive FS, so build the extra index entry from
    // the existing README blob and commit it.
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("add README").unwrap();

    let readme_blob = repo
        .git_og(&["rev-parse", "HEAD:README.md"])
        .unwrap()
        .trim()
        .to_string();
    repo.git_og(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{readme_blob},readme.md"),
    ])
    .unwrap();
    repo.git_og(&["commit", "-m", "add case-colliding readme.md"])
        .unwrap();

    // Apply the stash onto the new HEAD and commit.
    repo.git(&["stash", "apply"])
        .expect("stash apply should succeed");
    repo.git(&["add", "example.txt"]).unwrap();
    let commit = repo.commit("apply stash onto case-colliding tree").unwrap();

    // The AI attribution must survive despite the case-colliding target tree.
    let mut file = repo.filename("example.txt");
    file.assert_committed_lines(crate::lines![
        "root".unattributed_human(),
        "AI stashed".ai(),
        "anchor".unattributed_human(),
    ]);
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected sessions in authorship log - stash attribution lost on case-colliding target tree"
    );
}

#[test]
fn test_repeated_stash_pop_does_not_duplicate_checkpoints() {
    let repo = TestRepo::new();
    for file_idx in 0..10 {
        fs::write(repo.path().join(format!("jokes_{file_idx}.txt")), "base\n").unwrap();
    }
    repo.stage_all_and_commit("initial jokes").unwrap();

    let expected_first_file = joke_lines(0, 300);
    for file_idx in 0..10 {
        let lines = joke_lines(file_idx, 300);
        fs::write(
            repo.path().join(format!("jokes_{file_idx}.txt")),
            lines_to_content(&lines),
        )
        .unwrap();
    }
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();

    let initial_working_log = repo.current_working_logs();
    let initial_checkpoint_count = initial_working_log
        .read_all_checkpoints()
        .expect("read checkpoints before stash")
        .len();
    let initial_size = fs::metadata(initial_working_log.dir.join("checkpoints.jsonl"))
        .expect("checkpoints file exists before stash")
        .len();
    assert!(
        initial_checkpoint_count > 0,
        "test setup should create at least one checkpoint"
    );

    for round in 0..5 {
        repo.git(&["stash", "push", "-m", &format!("round {round}")])
            .expect("stash push should succeed");
        repo.git(&["stash", "pop"])
            .expect("stash pop should succeed");
    }

    let final_working_log = repo.current_working_logs();
    let final_checkpoints = final_working_log
        .read_all_checkpoints()
        .expect("read checkpoints after repeated stash");
    let final_size = fs::metadata(final_working_log.dir.join("checkpoints.jsonl"))
        .map(|metadata| metadata.len())
        .unwrap_or(0);

    assert!(
        final_checkpoints.len() <= initial_checkpoint_count,
        "stash/pop should not duplicate checkpoint history: initial={}, final={}",
        initial_checkpoint_count,
        final_checkpoints.len()
    );
    assert!(
        final_size <= initial_size.max(1),
        "checkpoints.jsonl should not grow across stash/pop cycles: initial={} final={}",
        initial_size,
        final_size
    );

    repo.stage_all_and_commit("commit repeated stash result")
        .expect("commit should succeed");
    let mut file = repo.filename("jokes_0.txt");
    file.assert_committed_lines(
        expected_first_file
            .into_iter()
            .map(|line| line.ai())
            .collect::<Vec<_>>(),
    );
}

#[test]
fn test_partial_stash_truncates_oversized_live_checkpoints_before_filtering() {
    let repo = TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_CHECKPOINTS_JSONL_MAX_BYTES", "64")]);
    fs::write(repo.path().join("a.txt"), "base a\n").unwrap();
    fs::write(repo.path().join("b.txt"), "base b\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    fs::write(repo.path().join("a.txt"), "base a\nai a\n").unwrap();
    fs::write(repo.path().join("b.txt"), "base b\nai b\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();

    let working_log = repo.current_working_logs();
    let checkpoints_file = working_log.dir.join("checkpoints.jsonl");
    let checkpoint_line = fs::read_to_string(&checkpoints_file)
        .expect("checkpoint file exists")
        .lines()
        .find(|line| !line.trim().is_empty())
        .expect("checkpoint fixture should contain one line")
        .to_string();
    fs::write(
        &checkpoints_file,
        (0..8)
            .map(|_| checkpoint_line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .expect("inflate checkpoint file above test limit");
    assert!(
        fs::metadata(&checkpoints_file).unwrap().len() > 64,
        "test setup should exceed the daemon's test checkpoint size limit"
    );

    repo.git(&["stash", "push", "--", "a.txt"])
        .expect("partial stash should survive oversized checkpoints.jsonl");
    repo.sync_daemon_force();

    let reset_size = fs::metadata(&checkpoints_file)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    assert_eq!(
        reset_size, 0,
        "oversized live checkpoints file should be reset before path filtering"
    );
    assert!(
        repo.current_working_logs()
            .read_all_checkpoints()
            .expect("read reset checkpoints")
            .is_empty(),
        "oversized checkpoint history should be discarded"
    );

    repo.git(&["stash", "pop"])
        .expect("stash pop after oversized checkpoint recovery should succeed");
    repo.stage_all_and_commit("commit recovered stash").unwrap();

    let mut a = repo.filename("a.txt");
    a.assert_committed_lines(crate::lines![
        "base a".unattributed_human(),
        "ai a".unattributed_human(),
    ]);
    let mut b = repo.filename("b.txt");
    b.assert_committed_lines(crate::lines![
        "base b".unattributed_human(),
        "ai b".unattributed_human(),
    ]);
}

#[test]
fn test_stash_operation_deletes_legacy_stashes_dir() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("example.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    fs::write(repo.path().join("example.txt"), "base\nai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.txt"])
        .unwrap();

    let legacy_dir = repo.path().join(".git").join("ai").join("stashes");
    fs::create_dir_all(legacy_dir.join("old_stash_worklog")).unwrap();
    fs::write(
        legacy_dir
            .join("old_stash_worklog")
            .join("checkpoints.jsonl"),
        "legacy checkpoint data\n".repeat(1024),
    )
    .unwrap();

    repo.git(&["stash", "push", "-m", "legacy cleanup"])
        .expect("stash push should succeed");
    repo.sync_daemon_force();

    assert!(
        !legacy_dir.exists(),
        "legacy .git/ai/stashes must be deleted instead of read or appended"
    );
    assert!(
        stash_v2_dir(&repo).exists(),
        "new stash data should be stored under stashes_v2"
    );
}

#[test]
fn test_partial_stash_trims_unstashed_initial_metadata() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("a.txt"), "base a\n").unwrap();
    fs::write(repo.path().join("b.txt"), "base b\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    let a_content = "a ai\n";
    let b_content = "b prompt\nb human\nb session\n";
    fs::write(repo.path().join("a.txt"), a_content).unwrap();
    fs::write(repo.path().join("b.txt"), b_content).unwrap();

    let mut files = HashMap::new();
    files.insert(
        "a.txt".to_string(),
        vec![LineAttribution::new(1, 1, "prompt_a".to_string(), None)],
    );
    files.insert(
        "b.txt".to_string(),
        vec![
            LineAttribution::new(1, 1, "prompt_b".to_string(), None),
            LineAttribution::new(2, 2, "h_b".to_string(), None),
            LineAttribution::new(3, 3, "s_b::t_1".to_string(), None),
        ],
    );

    let mut prompts = HashMap::new();
    prompts.insert("prompt_a".to_string(), test_prompt("prompt-a"));
    prompts.insert("prompt_b".to_string(), test_prompt("prompt-b"));

    let mut humans = BTreeMap::new();
    humans.insert(
        "h_b".to_string(),
        HumanRecord {
            author: "B Human <b@example.com>".to_string(),
        },
    );

    let mut file_contents = HashMap::new();
    file_contents.insert("a.txt".to_string(), a_content.to_string());
    file_contents.insert("b.txt".to_string(), b_content.to_string());

    let mut sessions = BTreeMap::new();
    sessions.insert(
        "s_b".to_string(),
        SessionRecord {
            agent_id: test_agent("session-b"),
            human_author: None,
            custom_attributes: None,
        },
    );

    repo.current_working_logs()
        .write_initial_attributions_with_contents(files, prompts, humans, file_contents, sessions)
        .unwrap();

    repo.git(&["stash", "push", "--", "a.txt"]).unwrap();
    repo.sync_daemon_force();

    let stash_initial = single_stash_v2_initial(&repo);
    assert_eq!(
        stash_initial.files.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from(["a.txt".to_string()])
    );
    assert!(
        stash_initial.prompts.contains_key("prompt_a"),
        "stashed file prompt metadata should be retained"
    );
    assert!(
        !stash_initial.prompts.contains_key("prompt_b"),
        "unstashed file prompt metadata should be dropped"
    );
    assert!(
        stash_initial.humans.is_empty(),
        "unstashed known-human metadata should be dropped"
    );
    assert!(
        stash_initial.sessions.is_empty(),
        "unstashed session metadata should be dropped"
    );

    let live_initial = repo.current_working_logs().read_initial_attributions();
    assert!(
        !live_initial.prompts.contains_key("prompt_a"),
        "live INITIAL should not retain metadata for the stashed file"
    );
    assert!(
        live_initial.prompts.contains_key("prompt_b"),
        "live INITIAL should retain metadata for the unstashed file"
    );
    assert!(
        live_initial.humans.contains_key("h_b"),
        "live INITIAL should retain unstashed known-human metadata"
    );
    assert!(
        live_initial.sessions.contains_key("s_b"),
        "live INITIAL should retain unstashed session metadata"
    );
}

/// Regression (#5): `git stash push -- <pathspec>` must only save attribution
/// for the stashed paths and leave unstashed attribution live.
#[test]
fn test_stash_push_pathspec_excludes_unstashed_file_from_stash_log() {
    let repo = TestRepo::new();
    let mut readme = repo.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    repo.stage_all_and_commit("initial commit").unwrap();

    let mut a = repo.filename("a.txt");
    a.set_contents(vec!["a line 1".ai(), "a line 2".ai()]);
    let mut b = repo.filename("b.txt");
    b.set_contents(vec!["b line 1".ai(), "b line 2".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();

    repo.git(&["stash", "push", "--", "a.txt"]).unwrap();
    repo.sync_daemon_force();

    let stashed_files: BTreeSet<_> = single_stash_v2_initial(&repo).files.into_keys().collect();
    let live_checkpoint_files = current_checkpoint_files(&repo);

    assert!(
        stashed_files.contains("a.txt"),
        "stash should carry the stashed file a.txt, got {:?}",
        stashed_files
    );
    assert!(
        !stashed_files.contains("b.txt"),
        "stash must NOT carry the unstashed file b.txt, got {:?}",
        stashed_files
    );
    assert!(
        !live_checkpoint_files.contains("a.txt"),
        "live checkpoints must not retain stashed file a.txt, got {:?}",
        live_checkpoint_files
    );
    assert!(
        live_checkpoint_files.contains("b.txt"),
        "live checkpoints must retain unstashed file b.txt, got {:?}",
        live_checkpoint_files
    );

    repo.git(&["stash", "pop"]).unwrap();
    repo.stage_all_and_commit("apply partial stash").unwrap();
    a.assert_committed_lines(vec!["a line 1".ai(), "a line 2".ai()]);
    b.assert_committed_lines(vec!["b line 1".ai(), "b line 2".ai()]);
}

crate::reuse_tests_in_worktree!(
    test_stash_pop_with_ai_attribution,
    test_stash_apply_with_ai_attribution,
    test_stash_apply_named_reference,
    test_stash_pop_with_existing_stack_entries,
    test_stash_multiple_files,
    test_stash_with_existing_initial_attributions,
    test_stash_pop_default_reference,
    test_stash_pop_empty_repo,
    test_stash_mixed_human_and_ai,
    test_stash_push_with_pathspec_single_file,
    test_stash_push_with_pathspec_directory,
    test_stash_push_multiple_pathspecs,
    test_stash_pop_with_conflict,
    test_stash_mixed_staged_and_unstaged,
    test_stash_pop_onto_head_with_ai_changes,
    test_stash_pop_across_branches,
    test_stash_pop_across_branches_with_conflict,
    test_stash_apply_reset_apply_again,
    test_stash_branch_preserves_ai_attribution,
    test_stash_pop_conflict_preserves_ai_attribution_without_new_checkpoint,
    test_stash_apply_shift_uses_final_commit_tree_after_later_edit,
);
