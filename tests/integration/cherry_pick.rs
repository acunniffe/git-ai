use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log::PromptRecord;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::authorship::working_log::AgentId;
use git_ai::git::notes_api::write_note;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Test cherry-picking a single AI-authored commit
#[test]
fn test_single_commit_cherry_pick() {
    let repo = TestRepo::new();

    // Create initial commit on default branch
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["Initial content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Get current branch name
    let main_branch = repo.current_branch();

    // Create feature branch with AI-authored changes
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, crate::lines!["AI feature line".ai()]);
    repo.stage_all_and_commit("Add AI feature").unwrap();
    let feature_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Switch back to main and cherry-pick the feature commit
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["cherry-pick", &feature_commit]).unwrap();

    // Verify final file state - hooks should have preserved AI authorship
    file.assert_lines_and_blame(crate::lines![
        "Initial content".ai(),
        "AI feature line".ai(),
    ]);

    // Verify stats
    let stats = repo.stats().unwrap();
    assert_eq!(
        stats.git_diff_added_lines, 2,
        "Should add 1 AI line (+ newline)"
    );
    assert_eq!(stats.ai_additions, 2, "2 AI lines added");
    assert_eq!(stats.ai_accepted, 2, "2 AI lines accepted");
    assert_eq!(stats.human_additions, 0, "0 human lines added");

    // Verify prompt records have correct stats
    let head_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let log = git_ai::git::notes_api::read_authorship_v3(
        &git_ai::git::find_repository_in_path(repo.path().to_str().unwrap()).unwrap(),
        &head_commit,
    )
    .unwrap();

    assert!(
        log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    assert!(
        !log.metadata.sessions.is_empty(),
        "Should have at least one session record"
    );
    for (session_id, session_record) in &log.metadata.sessions {
        assert!(
            !session_record.agent_id.tool.is_empty(),
            "Session {} should have a non-empty tool",
            session_id
        );
        assert!(
            !session_record.agent_id.model.is_empty(),
            "Session {} should have a non-empty model",
            session_id
        );
    }
}

#[test]
fn test_cherry_pick_preserves_human_only_commit_note_metadata() {
    let repo = TestRepo::new();

    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["human-only change"]);
    let source_commit = repo
        .stage_all_and_commit("human-only commit")
        .expect("create source commit");

    let source_note = repo
        .read_authorship_note(&source_commit.commit_sha)
        .expect("source commit should have a metadata-only note");
    let source_log =
        AuthorshipLog::deserialize_from_string(&source_note).expect("parse source note");
    assert!(source_log.metadata.prompts.is_empty());
    assert!(source_log.metadata.sessions.is_empty());

    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["cherry-pick", &source_commit.commit_sha])
        .unwrap();
    let new_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    let new_note = repo
        .read_authorship_note(&new_commit)
        .expect("cherry-picked commit should preserve metadata-only note");
    let new_log = AuthorshipLog::deserialize_from_string(&new_note).expect("parse new note");
    assert!(new_log.metadata.prompts.is_empty());
    assert!(new_log.metadata.sessions.is_empty());
    assert_eq!(new_log.metadata.base_commit_sha, new_commit);
}

#[test]
fn test_cherry_pick_preserves_prompt_only_commit_note_metadata() {
    let repo = TestRepo::new();

    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["human-only change"]);
    let source_commit = repo
        .stage_all_and_commit("human-only commit")
        .expect("create source commit");

    let source_note = repo
        .read_authorship_note(&source_commit.commit_sha)
        .expect("source commit should have authorship note");
    let mut source_log =
        AuthorshipLog::deserialize_from_string(&source_note).expect("parse source note");
    assert!(
        source_log.metadata.prompts.is_empty(),
        "precondition: source commit should not have AI prompts before test mutation"
    );

    let mut test_attrs = HashMap::new();
    test_attrs.insert("employee_id".to_string(), "E456".to_string());
    test_attrs.insert("team".to_string(), "backend".to_string());
    test_attrs.insert("device_id".to_string(), "MAC-002".to_string());

    source_log.metadata.prompts.insert(
        "prompt-only-session".to_string(),
        PromptRecord {
            agent_id: AgentId {
                tool: "mock_ai".to_string(),
                id: "session-1".to_string(),
                model: "test-model".to_string(),
            },
            human_author: Some("Test User <test@example.com>".to_string()),
            total_additions: 11,
            total_deletions: 2,
            accepted_lines: 0,
            overriden_lines: 0,
            custom_attributes: Some(test_attrs.clone()),
            messages_url: None,
        },
    );

    let mutated_source_note = source_log
        .serialize_to_string()
        .expect("serialize mutated source note");
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(
        &git_ai_repo,
        &source_commit.commit_sha,
        &mutated_source_note,
    )
    .expect("overwrite source note with prompt-only metadata");

    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["cherry-pick", &source_commit.commit_sha])
        .unwrap();
    let new_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    let new_note = repo
        .read_authorship_note(&new_commit)
        .expect("cherry-picked commit should preserve prompt-only note");
    let new_log = AuthorshipLog::deserialize_from_string(&new_note).expect("parse new note");
    assert_eq!(new_log.metadata.prompts.len(), 1);
    assert_eq!(new_log.metadata.base_commit_sha, new_commit);

    let prompt = new_log
        .metadata
        .prompts
        .get("prompt-only-session")
        .expect("prompt metadata should be preserved");
    assert_eq!(prompt.agent_id.tool, "mock_ai");
    assert_eq!(prompt.agent_id.id, "session-1");
    assert_eq!(prompt.agent_id.model, "test-model");
    assert_eq!(prompt.total_additions, 11);
    assert_eq!(prompt.total_deletions, 2);
    assert_eq!(
        prompt.custom_attributes,
        Some(test_attrs),
        "custom_attributes should be preserved through cherry-pick"
    );
}

/// Test cherry-picking multiple commits in sequence
#[test]
fn test_multiple_commits_cherry_pick() {
    let repo = TestRepo::new();

    // Create initial commit on default branch
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["Line 1", ""]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let main_branch = repo.current_branch();

    // Create feature branch with multiple AI-authored commits
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // First AI commit
    file.insert_at(1, crate::lines!["AI line 2".ai()]);
    repo.stage_all_and_commit("AI commit 1").unwrap();
    let commit1 = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Second AI commit
    file.insert_at(2, crate::lines!["AI line 3".ai()]);
    repo.stage_all_and_commit("AI commit 2").unwrap();
    let commit2 = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Third AI commit
    file.insert_at(3, crate::lines!["AI line 4".ai()]);
    repo.stage_all_and_commit("AI commit 3").unwrap();
    let commit3 = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Switch back to main and cherry-pick all three commits
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["cherry-pick", &commit1, &commit2, &commit3])
        .unwrap();

    // Verify final file state - hooks should have preserved AI authorship
    file.assert_lines_and_blame(crate::lines![
        "Line 1".human(),
        "AI line 2".ai(),
        "AI line 3".ai(),
        "AI line 4".ai(),
    ]);

    // Verify stats for the last cherry-picked commit
    let stats = repo.stats().unwrap();
    eprintln!("Stats: {:?}", stats);
    // Last commit inserts "AI line 4" - git_diff_added_lines only counts this commit's changes
    // ai_additions is capped by git_diff_added_lines, so it reflects this commit only
    assert_eq!(stats.git_diff_added_lines, 1, "Should have added 1 lines");
    assert_eq!(stats.ai_additions, 1, "At least 1 AI line in this commit");
    assert_eq!(stats.ai_accepted, 1, "1 AI lines accepted in commit");

    // Verify session records exist
    let head_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let log = git_ai::git::notes_api::read_authorship_v3(
        &git_ai::git::find_repository_in_path(repo.path().to_str().unwrap()).unwrap(),
        &head_commit,
    )
    .unwrap();

    assert!(
        log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    assert!(
        !log.metadata.sessions.is_empty(),
        "Should have session records"
    );
    for (session_id, session_record) in &log.metadata.sessions {
        assert!(
            !session_record.agent_id.tool.is_empty(),
            "Session {} should have a non-empty tool",
            session_id
        );
        assert!(
            !session_record.agent_id.model.is_empty(),
            "Session {} should have a non-empty model",
            session_id
        );
    }
}

/// Test cherry-pick with conflicts and --continue
#[test]
fn test_cherry_pick_with_conflict_and_continue() {
    let repo = TestRepo::new();

    // Create initial commit on default branch
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["Line 1", "Line 2", "Line 3"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let main_branch = repo.current_branch();

    // Create feature branch with AI changes
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.replace_at(1, "AI_FEATURE_VERSION".ai());
    repo.stage_all_and_commit("AI feature").unwrap();
    let feature_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Switch back to main and make conflicting change
    repo.git(&["checkout", &main_branch]).unwrap();
    file.replace_at(1, "MAIN_BRANCH_VERSION".human());
    repo.stage_all_and_commit("Human change").unwrap();

    // Try to cherry-pick (should conflict)
    let cherry_pick_result = repo.git(&["cherry-pick", &feature_commit]);
    assert!(cherry_pick_result.is_err(), "Should have conflict");

    // Resolve conflict by choosing the AI version
    use std::fs;
    fs::write(
        repo.path().join("file.txt"),
        "Line 1\nAI_FEATURE_VERSION\nLine 3",
    )
    .unwrap();
    repo.git(&["add", "file.txt"]).unwrap();

    // Continue cherry-pick
    repo.git(&["cherry-pick", "--continue"]).unwrap();

    // Verify final file state - hooks should have preserved AI authorship
    file.assert_lines_and_blame(crate::lines![
        "Line 1".human(),
        "AI_FEATURE_VERSION".ai(),
        "Line 3".human(),
    ]);
}

/// Test cherry-pick --abort
#[test]
fn test_cherry_pick_abort() {
    let repo = TestRepo::new();

    // Create initial commit on default branch
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["Line 1", "Line 2"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let initial_head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    let main_branch = repo.current_branch();

    // Create feature branch with AI changes (modify line 2)
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.replace_at(1, "AI modification of line 2".ai());
    repo.stage_all_and_commit("AI feature").unwrap();

    // Assert intermediary blame
    file.assert_lines_and_blame(crate::lines![
        "Line 1".human(),
        "AI modification of line 2".ai(),
    ]);

    let feature_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Switch back to main and make conflicting change (also modify line 2)
    repo.git(&["checkout", &main_branch]).unwrap();
    file.replace_at(1, "Human modification of line 2".human());
    repo.stage_all_and_commit("Human change").unwrap();

    // Assert intermediary blame
    file.assert_lines_and_blame(crate::lines![
        "Line 1".human(),
        "Human modification of line 2".human(),
    ]);

    // Try to cherry-pick (should conflict)
    let cherry_pick_result = repo.git(&["cherry-pick", &feature_commit]);
    assert!(cherry_pick_result.is_err(), "Should have conflict");

    // Abort the cherry-pick
    repo.git(&["cherry-pick", "--abort"]).unwrap();

    // Verify HEAD is back to before the cherry-pick
    let current_head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(current_head, initial_head); // Different because we made the "Human change" commit

    // Verify final file state (should have human's version)
    file.assert_lines_and_blame(crate::lines![
        "Line 1".human(),
        "Human modification of line 2".human(),
    ]);
}

/// Test cherry-picking from branch without AI authorship
#[test]
fn test_cherry_pick_no_ai_authorship() {
    let repo = TestRepo::new();

    // Create initial commit on default branch
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["Line 1"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let main_branch = repo.current_branch();
    // Create feature branch with human-only changes (no AI)
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, crate::lines!["Human line 2".human()]);
    repo.stage_all_and_commit("Human feature").unwrap();
    let feature_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Switch back to main and cherry-pick
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["cherry-pick", &feature_commit]).unwrap();

    // Verify final file state - should have no AI authorship
    file.assert_lines_and_blame(crate::lines!["Line 1".human(), "Human line 2".human(),]);
}

/// Test cherry-pick preserving multiple AI sessions from different commits
#[test]
fn test_cherry_pick_multiple_ai_sessions() {
    let repo = TestRepo::new();

    // Create initial commit on default branch
    let mut file = repo.filename("main.rs");
    file.set_contents(crate::lines!["fn main() {}"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let main_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // First AI session adds logging
    file.replace_at(0, "fn main() {".human());
    file.insert_at(1, crate::lines!["    println!(\"Starting\");".ai()]);
    file.insert_at(2, crate::lines!["}".human()]);
    repo.stage_all_and_commit("Add logging").unwrap();
    let commit1 = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Second AI session adds error handling
    file.insert_at(2, crate::lines!["    // TODO: Add error handling".ai()]);
    repo.stage_all_and_commit("Add error handling").unwrap();
    let commit2 = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Cherry-pick both to main
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["cherry-pick", &commit1, &commit2]).unwrap();

    // Verify final file state - hooks should have preserved AI authorship
    file.assert_lines_and_blame(crate::lines![
        "fn main() {".ai(),
        "    println!(\"Starting\");".ai(),
        "    // TODO: Add error handling".ai(),
        "}".human(),
    ]);

    // Verify stats for the last cherry-picked commit
    let stats = repo.stats().unwrap();
    assert_eq!(stats.git_diff_added_lines, 1, "Last commit adds 1 line");
    assert_eq!(stats.ai_additions, 1, "1 AI line in last commit");
    assert_eq!(stats.ai_accepted, 1, "1 AI lines accepted");

    // Verify session records exist
    let head_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let log = git_ai::git::notes_api::read_authorship_v3(
        &git_ai::git::find_repository_in_path(repo.path().to_str().unwrap()).unwrap(),
        &head_commit,
    )
    .unwrap();

    assert!(
        log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    assert!(
        !log.metadata.sessions.is_empty(),
        "Should have at least one session record"
    );
    for (session_id, session_record) in &log.metadata.sessions {
        assert!(
            !session_record.agent_id.tool.is_empty(),
            "Session {} should have a non-empty tool",
            session_id
        );
        assert!(
            !session_record.agent_id.model.is_empty(),
            "Session {} should have a non-empty model",
            session_id
        );
    }
}

/// Test that trees-identical fast path works
#[test]
fn test_cherry_pick_identical_trees() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["Line 1"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let main_branch = repo.current_branch();

    // Create feature branch with AI changes
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, crate::lines!["AI line".ai()]);
    repo.stage_all_and_commit("AI feature").unwrap();
    let feature_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Add another commit on feature (just to have a parent)
    file.insert_at(2, crate::lines!["More AI".ai()]);
    repo.stage_all_and_commit("More AI").unwrap();

    // Cherry-pick the first feature commit to main
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["cherry-pick", &feature_commit]).unwrap();

    // Verify final file state - hooks should have preserved AI authorship
    file.assert_lines_and_blame(crate::lines!["Line 1".ai(), "AI line".ai(),]);
}

/// Test cherry-pick where some commits become empty (already applied)
#[test]
fn test_cherry_pick_empty_commits() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["Line 1"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let main_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, crate::lines!["Feature line".ai()]);
    repo.stage_all_and_commit("Add feature").unwrap();
    let feature_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Manually apply the same change to main
    repo.git(&["checkout", &main_branch]).unwrap();

    // Get a fresh TestFile after branch switch - it will auto-populate from the existing file
    let mut file_on_main = repo.filename("file.txt");
    file_on_main.insert_at(1, crate::lines!["Feature line".human()]);
    repo.stage_all_and_commit("Apply feature manually").unwrap();

    // Try to cherry-pick the feature commit (should become empty or conflict)
    let result = repo.git(&["cherry-pick", &feature_commit]);

    // Git might succeed and skip the empty commit, or it might create a conflict
    // The key is that it shouldn't crash
    match result {
        Ok(_) => {
            // Empty commit was skipped successfully
        }
        Err(_) => {
            // Git reported an error (conflict or empty commit)
            // Abort the cherry-pick to clean up
            let _ = repo.git(&["cherry-pick", "--abort"]);
        }
    }

    // Verify final file state - content should be preserved
    let actual_content = repo.read_file("file.txt").unwrap();
    assert_eq!(
        actual_content.trim(),
        "Line 1\nFeature line",
        "File content should be preserved after cherry-pick/abort"
    );
}

/// Test that custom attributes set via config are preserved through a cherry-pick
/// when the real post-commit pipeline injects them.
#[test]
fn test_cherry_pick_preserves_custom_attributes_from_config() {
    let mut repo =
        TestRepo::new_with_daemon_scope(crate::repos::test_repo::DaemonTestScope::Dedicated);

    // Configure custom attributes via config patch
    let mut attrs = HashMap::new();
    attrs.insert("employee_id".to_string(), "E101".to_string());
    attrs.insert("team".to_string(), "frontend".to_string());
    attrs.insert("device_id".to_string(), "LNX-007".to_string());
    repo.patch_git_ai_config(|patch| {
        patch.custom_attributes = Some(attrs.clone());
    });

    // Create initial commit on default branch
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["Initial content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main_branch = repo.current_branch();

    // Create feature branch with AI-authored changes
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, crate::lines!["AI feature line".ai()]);
    repo.stage_all_and_commit("Add AI feature").unwrap();
    let feature_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Verify custom attributes were set on the original commit
    let original_note = repo
        .read_authorship_note(&feature_commit)
        .expect("original commit should have authorship note");
    let original_log =
        AuthorshipLog::deserialize_from_string(&original_note).expect("parse original note");
    assert!(
        original_log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    assert!(
        !original_log.metadata.sessions.is_empty(),
        "precondition: original commit should have session records"
    );
    for session in original_log.metadata.sessions.values() {
        assert_eq!(
            session.custom_attributes.as_ref(),
            Some(&attrs),
            "precondition: original commit should have custom_attributes from config"
        );
    }

    // Switch back to main and cherry-pick the feature commit
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["cherry-pick", &feature_commit]).unwrap();

    // Verify custom attributes survived the cherry-pick
    let new_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let new_note = repo
        .read_authorship_note(&new_commit)
        .expect("cherry-picked commit should have authorship note");
    let new_log = AuthorshipLog::deserialize_from_string(&new_note).expect("parse new note");
    assert!(
        new_log.metadata.prompts.is_empty(),
        "cherry-picked commit should not have prompts"
    );
    assert!(
        !new_log.metadata.sessions.is_empty(),
        "cherry-picked commit should have session records"
    );
    for session in new_log.metadata.sessions.values() {
        assert_eq!(
            session.custom_attributes.as_ref(),
            Some(&attrs),
            "custom_attributes should be preserved through cherry-pick"
        );
    }

    // Also verify the AI attribution itself survived
    file.assert_lines_and_blame(crate::lines![
        "Initial content".ai(),
        "AI feature line".ai()
    ]);
}

/// Regression test for #952: Failed cherry-pick with bad args should not corrupt state
/// for subsequent valid cherry-picks.
///
/// Bug: git-ai pre-hook writes a CherryPickStart with empty source_commits when given
/// bad revision arguments.  If that stale event is left in the rewrite log, the next
/// valid cherry-pick may process attribution against the wrong (empty) source list,
/// producing zero AI attributions even for lines that came from an AI session.
#[test]
fn test_cherry_pick_bad_args_dont_corrupt_subsequent_attribution() {
    let repo = TestRepo::new();
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["base line"]);
    repo.stage_all_and_commit("initial").unwrap();
    let main_branch = repo.current_branch();

    // Create feature branch with 2 AI commits
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, crate::lines!["AI line 1".ai()]);
    repo.stage_all_and_commit("AI commit 1").unwrap();
    let sha1 = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    file.insert_at(2, crate::lines!["AI line 2".ai()]);
    repo.stage_all_and_commit("AI commit 2").unwrap();
    let sha2 = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    repo.git(&["checkout", &main_branch]).unwrap();

    // Attempt cherry-pick with bad args: two SHAs concatenated into one string, which
    // is not a valid revision.  This must fail and must NOT write a corrupt event to
    // the rewrite log (fixed by skipping CherryPickStart when source_commits is empty).
    let bad_arg = format!("{} {}", sha1, sha2);
    let bad_result = repo.git(&["cherry-pick", &bad_arg]);
    assert!(
        bad_result.is_err(),
        "cherry-pick with invalid revision should fail"
    );
    let _ = repo.git(&["cherry-pick", "--abort"]); // clean up any partial state

    // Cherry-pick sha1 — must produce correct per-line AI attribution despite the
    // prior corrupted attempt.
    repo.git(&["cherry-pick", &sha1]).unwrap();
    // Single-commit cherry-pick: the source commit's note covers all file content,
    // so all lines (including "base line") end up AI-attributed after the copy.
    file.assert_lines_and_blame(crate::lines!["base line".ai(), "AI line 1".ai(),]);

    // Cherry-pick sha2 as well — state must still be clean.
    repo.git(&["cherry-pick", &sha2]).unwrap();
    file.assert_lines_and_blame(crate::lines![
        "base line".ai(),
        "AI line 1".ai(),
        "AI line 2".ai(),
    ]);
}

/// Regression test for #951: cherry-pick --skip should preserve attribution for the
/// remaining commits in the sequence.
///
/// Bug: when a cherry-pick becomes "empty" (its changes are already present) and the
/// user runs `cherry-pick --skip`, git-ai failed to remove the skipped commit from the
/// CherryPickStart source_commits list.  The post-hook then found a mismatch between
/// the number of source commits (3) and the number of new commits actually created (2),
/// and skipped attribution for ALL remaining commits.
#[test]
fn test_cherry_pick_skip_preserves_subsequent_attribution() {
    let repo = TestRepo::new();
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["base line"]);
    repo.stage_all_and_commit("initial").unwrap();
    let main_branch = repo.current_branch();

    // Feature branch: three AI commits that each append one line.
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, crate::lines!["AI line 1".ai()]);
    repo.stage_all_and_commit("AI commit 1").unwrap();
    let sha1 = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    file.insert_at(2, crate::lines!["AI line 2".ai()]);
    repo.stage_all_and_commit("AI commit 2").unwrap();
    let sha2 = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    file.insert_at(3, crate::lines!["AI line 3".ai()]);
    repo.stage_all_and_commit("AI commit 3").unwrap();
    let sha3 = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    repo.git(&["checkout", &main_branch]).unwrap();

    // Pre-apply sha1's change as a plain human commit so that cherry-picking sha1
    // results in an empty diff — forcing git to stop and require --skip.
    let mut main_file = repo.filename("file.txt");
    main_file.insert_at(1, crate::lines!["AI line 1"]); // no .ai() — human commit
    repo.stage_all_and_commit("pre-apply sha1 as human")
        .unwrap();

    // Start cherry-picking all three.  sha1 is now empty → git stops with an error.
    let pick_result = repo.git(&["cherry-pick", &sha1, &sha2, &sha3]);
    assert!(
        pick_result.is_err(),
        "cherry-pick of an already-applied commit should require --skip"
    );

    // Skip the empty sha1 commit; git should then apply sha2 and sha3 automatically.
    repo.git(&["cherry-pick", "--skip"]).unwrap();

    // Final file state after the full series:
    //   "base line"  — initial commit, human
    //   "AI line 1"  — pre-applied as a human commit, but sha2's note carries sha1's
    //                  AI attribution from the feature branch, so it ends up AI after
    //                  the cherry-pick of sha2 overwrites the note.
    //   "AI line 2"  — cherry-picked from sha2, AI session
    //   "AI line 3"  — cherry-picked from sha3, AI session
    file.assert_lines_and_blame(crate::lines![
        "base line".human(),
        "AI line 1".ai(),
        "AI line 2".ai(),
        "AI line 3".ai(),
    ]);
}

/// Regression test for #955: cherry-pick from a remote repo whose notes have not been
/// fetched locally should still produce correct AI attribution.
///
/// Bug: the post-cherry-pick hook looked up notes for the source commit to copy
/// attribution, but found nothing because `refs/notes/ai` hadn't been fetched from the
/// remote.  The fix auto-fetches notes via `fetch_authorship_notes` (the safe,
/// non-destructive pattern) when any source commit is missing local notes.
#[test]
fn test_cherry_pick_from_remote_without_prefetched_notes() {
    // Source repo: one human initial commit, then one AI commit.
    let source_repo = TestRepo::new();
    let mut source_file = source_repo.filename("file.txt");
    source_file.set_contents(crate::lines!["base"]);
    source_repo.stage_all_and_commit("initial").unwrap();
    source_file.insert_at(1, crate::lines!["AI line".ai()]);
    source_repo.stage_all_and_commit("AI commit").unwrap();
    let ai_commit = source_repo
        .git(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Target repo: independent repo with the same base content so the cherry-pick
    // diff ("add AI line after base") applies cleanly.
    let target_repo = TestRepo::new();
    let mut target_file = target_repo.filename("file.txt");
    target_file.set_contents(crate::lines!["base"]);
    target_repo.stage_all_and_commit("initial").unwrap();

    // Register source as a remote and fetch commits — but NOT refs/notes/ai.
    // git fetch only fetches what the refspec asks for; notes are separate.
    target_repo
        .git(&[
            "remote",
            "add",
            "source",
            source_repo.path().to_str().unwrap(),
        ])
        .unwrap();
    // Fetch only the branch objects, explicitly excluding notes.
    target_repo
        .git(&["fetch", "source", "refs/heads/*:refs/remotes/source/*"])
        .unwrap();

    // Confirm notes are absent (the fix relies on detecting this absence).
    let _ = target_repo.git(&["update-ref", "-d", "refs/notes/ai"]);
    let _ = target_repo.git(&["update-ref", "-d", "refs/notes/ai-remote/source"]);

    // Cherry-pick the AI commit.  The fix should auto-fetch notes from "source"
    // and produce correct attribution.
    target_repo.git(&["cherry-pick", &ai_commit]).unwrap();

    // Single-commit cherry-pick: the source note covers all file content, so both lines
    // end up AI-attributed after the note is copied to the cherry-picked commit.
    target_file.assert_lines_and_blame(crate::lines!["base".ai(), "AI line".ai(),]);
}

#[test]
fn test_cherry_pick_local_remote_tracking_ref_missing_from_daemon_snapshot() {
    let repo = TestRepo::new();
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("initial").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, crate::lines!["AI line".ai()]);
    let source_commit = repo.stage_all_and_commit("AI commit").unwrap();

    repo.read_authorship_note(&source_commit.commit_sha)
        .expect("source authorship note should already be local");

    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git_og(&[
        "update-ref",
        "refs/remotes/origin/feature",
        &source_commit.commit_sha,
    ])
    .unwrap();

    repo.git(&["cherry-pick", "origin/feature"]).unwrap();

    file.assert_lines_and_blame(crate::lines!["base".ai(), "AI line".ai(),]);
}

#[test]
fn test_cherry_pick_partial_remote_tracking_ref_resolution_falls_back_to_git() {
    let repo = TestRepo::new();
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("initial").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, crate::lines!["AI line 1".ai()]);
    let first_source_commit = repo.stage_all_and_commit("AI commit 1").unwrap();
    file.insert_at(2, crate::lines!["AI line 2".ai()]);
    let second_source_commit = repo.stage_all_and_commit("AI commit 2").unwrap();

    repo.read_authorship_note(&first_source_commit.commit_sha)
        .expect("first source authorship note should already be local");
    repo.read_authorship_note(&second_source_commit.commit_sha)
        .expect("second source authorship note should already be local");

    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git_og(&[
        "update-ref",
        "refs/remotes/origin/feature",
        &second_source_commit.commit_sha,
    ])
    .unwrap();

    repo.git(&[
        "cherry-pick",
        &first_source_commit.commit_sha,
        "origin/feature",
    ])
    .unwrap();

    file.assert_lines_and_blame(crate::lines![
        "base".ai(),
        "AI line 1".ai(),
        "AI line 2".ai(),
    ]);
}

#[test]
fn test_cherry_pick_failed_continue_keeps_pending_remote_tracking_source() {
    let repo = TestRepo::new();
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["Line 1", "Line 2", "Line 3"]);
    repo.stage_all_and_commit("initial").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.replace_at(1, "AI_REMOTE_VERSION".ai());
    let source_commit = repo.stage_all_and_commit("AI feature").unwrap();
    repo.read_authorship_note(&source_commit.commit_sha)
        .expect("source authorship note should already be local");

    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git_og(&[
        "update-ref",
        "refs/remotes/origin/feature",
        &source_commit.commit_sha,
    ])
    .unwrap();

    file.replace_at(1, "MAIN_BRANCH_VERSION".human());
    repo.stage_all_and_commit("Human change").unwrap();

    let cherry_pick_result = repo.git(&["cherry-pick", "origin/feature"]);
    assert!(cherry_pick_result.is_err(), "cherry-pick should conflict");
    repo.sync_daemon();

    let failed_continue = repo.git(&["cherry-pick", "--continue"]);
    assert!(
        failed_continue.is_err(),
        "unresolved cherry-pick should not continue"
    );
    repo.sync_daemon();

    fs::write(
        repo.path().join("file.txt"),
        "Line 1\nAI_REMOTE_VERSION\nLine 3",
    )
    .unwrap();
    repo.git(&["add", "file.txt"]).unwrap();
    repo.git(&["cherry-pick", "--continue"]).unwrap();

    file.assert_lines_and_blame(crate::lines![
        "Line 1".human(),
        "AI_REMOTE_VERSION".ai(),
        "Line 3".human(),
    ]);
}

#[test]
fn test_cherry_pick_skip_failed_next_conflict_advances_pending_remote_tracking_source() {
    let repo = TestRepo::new();
    let conflict_a_path = repo.path().join("conflict_a.txt");
    let conflict_b_path = repo.path().join("conflict_b.txt");

    fs::write(&conflict_a_path, "base\nshared\n").unwrap();
    fs::write(&conflict_b_path, "base\nshared\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict_a.txt"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict_b.txt"])
        .unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut conflict_a = repo.filename("conflict_a.txt");
    let mut conflict_b = repo.filename("conflict_b.txt");
    conflict_a.assert_committed_lines(crate::lines!["base".human(), "shared".human(),]);
    conflict_b.assert_committed_lines(crate::lines!["base".human(), "shared".human(),]);
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(&conflict_a_path, "base\nFEATURE_A_HUMAN\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict_a.txt"])
        .unwrap();
    let skipped_source_commit = repo.stage_all_and_commit("human conflict A").unwrap();
    conflict_a.assert_committed_lines(crate::lines!["base".human(), "FEATURE_A_HUMAN".human(),]);

    fs::write(&conflict_b_path, "base\nAI_REMOTE_VERSION\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict_b.txt"])
        .unwrap();
    let applied_source_commit = repo.stage_all_and_commit("AI conflict B").unwrap();
    conflict_b.assert_committed_lines(crate::lines!["base".human(), "AI_REMOTE_VERSION".ai(),]);

    repo.read_authorship_note(&skipped_source_commit.commit_sha)
        .expect("skipped source authorship note should already be local");
    repo.read_authorship_note(&applied_source_commit.commit_sha)
        .expect("applied source authorship note should already be local");

    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git_og(&[
        "update-ref",
        "refs/remotes/origin/feature",
        &applied_source_commit.commit_sha,
    ])
    .unwrap();

    fs::write(&conflict_a_path, "base\nMAIN_A_HUMAN\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict_a.txt"])
        .unwrap();
    fs::write(&conflict_b_path, "base\nMAIN_B_HUMAN\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict_b.txt"])
        .unwrap();
    repo.stage_all_and_commit("main conflicts").unwrap();
    conflict_a.assert_committed_lines(crate::lines!["base".human(), "MAIN_A_HUMAN".human(),]);
    conflict_b.assert_committed_lines(crate::lines!["base".human(), "MAIN_B_HUMAN".human(),]);

    let cherry_pick_result = repo.git(&["cherry-pick", "origin/feature~1", "origin/feature"]);
    assert!(
        cherry_pick_result.is_err(),
        "first cherry-pick should conflict"
    );
    repo.sync_daemon();

    let skip_result = repo.git(&["cherry-pick", "--skip"]);
    assert!(
        skip_result.is_err(),
        "skip should advance to the second source and conflict"
    );
    repo.sync_daemon();

    fs::write(&conflict_b_path, "base\nAI_REMOTE_VERSION\n").unwrap();
    repo.git(&["add", "conflict_b.txt"]).unwrap();
    repo.git(&["cherry-pick", "--continue"]).unwrap();

    conflict_a.assert_committed_lines(crate::lines!["base".human(), "MAIN_A_HUMAN".human(),]);
    conflict_b.assert_committed_lines(crate::lines!["base".human(), "AI_REMOTE_VERSION".ai(),]);
}

/// Regression test for #982: a skip in the middle of a cherry-pick sequence must still
/// be remembered when a later `--continue` completes the remaining commits.
#[test]
fn test_cherry_pick_skip_then_continue_applies_remaining_commits() {
    let repo = TestRepo::new();
    let conflict_a_path = repo.path().join("conflict_a.txt");
    let conflict_b_path = repo.path().join("conflict_b.txt");
    let clean_c_path = repo.path().join("clean_c.txt");

    for path in [&conflict_a_path, &conflict_b_path, &clean_c_path] {
        fs::write(path, "base\nshared\n").unwrap();
    }
    for path in ["conflict_a.txt", "conflict_b.txt", "clean_c.txt"] {
        repo.git_ai(&["checkpoint", "mock_known_human", path])
            .unwrap();
    }
    repo.stage_all_and_commit("initial").unwrap();
    let mut conflict_a = repo.filename("conflict_a.txt");
    let mut conflict_b = repo.filename("conflict_b.txt");
    let mut clean_c = repo.filename("clean_c.txt");
    conflict_a.assert_committed_lines(crate::lines!["base".human(), "shared".human(),]);
    conflict_b.assert_committed_lines(crate::lines!["base".human(), "shared".human(),]);
    clean_c.assert_committed_lines(crate::lines!["base".human(), "shared".human(),]);
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(&conflict_a_path, "base\nFEATURE_A_HUMAN\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict_a.txt"])
        .unwrap();
    let skipped_source = repo.stage_all_and_commit("human conflict A").unwrap();
    conflict_a.assert_committed_lines(crate::lines!["base".human(), "FEATURE_A_HUMAN".human(),]);

    fs::write(&conflict_b_path, "base\nAI_B_VERSION\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict_b.txt"])
        .unwrap();
    let continued_source = repo.stage_all_and_commit("AI conflict B").unwrap();
    conflict_b.assert_committed_lines(crate::lines!["base".human(), "AI_B_VERSION".ai(),]);

    fs::write(&clean_c_path, "base\nAI_C_VERSION\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "clean_c.txt"])
        .unwrap();
    let auto_applied_source = repo.stage_all_and_commit("AI clean C").unwrap();
    clean_c.assert_committed_lines(crate::lines!["base".human(), "AI_C_VERSION".ai(),]);

    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(&conflict_a_path, "base\nMAIN_A_HUMAN\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict_a.txt"])
        .unwrap();
    fs::write(&conflict_b_path, "base\nMAIN_B_HUMAN\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict_b.txt"])
        .unwrap();
    repo.stage_all_and_commit("main conflicts").unwrap();
    conflict_a.assert_committed_lines(crate::lines!["base".human(), "MAIN_A_HUMAN".human(),]);
    conflict_b.assert_committed_lines(crate::lines!["base".human(), "MAIN_B_HUMAN".human(),]);
    clean_c.assert_committed_lines(crate::lines!["base".human(), "shared".human(),]);

    let cherry_pick_result = repo.git(&[
        "cherry-pick",
        &skipped_source.commit_sha,
        &continued_source.commit_sha,
        &auto_applied_source.commit_sha,
    ]);
    assert!(
        cherry_pick_result.is_err(),
        "first cherry-pick should conflict"
    );
    repo.sync_daemon();

    let skip_result = repo.git(&["cherry-pick", "--skip"]);
    assert!(
        skip_result.is_err(),
        "skip should advance to the second source and conflict"
    );
    repo.sync_daemon();

    fs::write(&conflict_b_path, "base\nAI_B_VERSION\n").unwrap();
    repo.git(&["add", "conflict_b.txt"]).unwrap();
    repo.git(&["cherry-pick", "--continue"]).unwrap();

    conflict_a.assert_committed_lines(crate::lines!["base".human(), "MAIN_A_HUMAN".human(),]);
    conflict_b.assert_committed_lines(crate::lines!["base".human(), "AI_B_VERSION".ai(),]);
    clean_c.assert_committed_lines(crate::lines!["base".human(), "AI_C_VERSION".ai(),]);
}

#[test]
fn test_cherry_pick_skip_failed_next_conflict_does_not_double_skip_refcursor_sources() {
    let repo = TestRepo::new();
    let conflict_a_path = repo.path().join("conflict_a.txt");
    let clean_b_path = repo.path().join("clean_b.txt");
    let conflict_c_path = repo.path().join("conflict_c.txt");

    fs::write(&conflict_a_path, "base\nshared\n").unwrap();
    fs::write(&clean_b_path, "base\nshared\n").unwrap();
    fs::write(&conflict_c_path, "base\nshared\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict_a.txt"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "clean_b.txt"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict_c.txt"])
        .unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut conflict_a = repo.filename("conflict_a.txt");
    let mut clean_b = repo.filename("clean_b.txt");
    let mut conflict_c = repo.filename("conflict_c.txt");
    conflict_a.assert_committed_lines(crate::lines!["base".human(), "shared".human(),]);
    clean_b.assert_committed_lines(crate::lines!["base".human(), "shared".human(),]);
    conflict_c.assert_committed_lines(crate::lines!["base".human(), "shared".human(),]);
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(&conflict_a_path, "base\nFEATURE_A_HUMAN\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict_a.txt"])
        .unwrap();
    let skipped_source_commit = repo.stage_all_and_commit("human conflict A").unwrap();
    conflict_a.assert_committed_lines(crate::lines!["base".human(), "FEATURE_A_HUMAN".human(),]);

    fs::write(&clean_b_path, "base\nAI_B_VERSION\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "clean_b.txt"])
        .unwrap();
    let applied_during_skip_commit = repo.stage_all_and_commit("AI clean B").unwrap();
    clean_b.assert_committed_lines(crate::lines!["base".human(), "AI_B_VERSION".ai(),]);

    fs::write(&conflict_c_path, "base\nAI_C_VERSION\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict_c.txt"])
        .unwrap();
    let next_conflict_commit = repo.stage_all_and_commit("AI conflict C").unwrap();
    conflict_c.assert_committed_lines(crate::lines!["base".human(), "AI_C_VERSION".ai(),]);

    repo.read_authorship_note(&skipped_source_commit.commit_sha)
        .expect("skipped source authorship note should already be local");
    repo.read_authorship_note(&applied_during_skip_commit.commit_sha)
        .expect("applied source authorship note should already be local");
    repo.read_authorship_note(&next_conflict_commit.commit_sha)
        .expect("next conflict source authorship note should already be local");

    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(&conflict_a_path, "base\nMAIN_A_HUMAN\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict_a.txt"])
        .unwrap();
    fs::write(&conflict_c_path, "base\nMAIN_C_HUMAN\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict_c.txt"])
        .unwrap();
    repo.stage_all_and_commit("main conflicts").unwrap();
    conflict_a.assert_committed_lines(crate::lines!["base".human(), "MAIN_A_HUMAN".human(),]);
    clean_b.assert_committed_lines(crate::lines!["base".human(), "shared".human(),]);
    conflict_c.assert_committed_lines(crate::lines!["base".human(), "MAIN_C_HUMAN".human(),]);

    let cherry_pick_result = repo.git(&[
        "cherry-pick",
        &skipped_source_commit.commit_sha,
        &applied_during_skip_commit.commit_sha,
        &next_conflict_commit.commit_sha,
    ]);
    assert!(
        cherry_pick_result.is_err(),
        "first cherry-pick should conflict"
    );
    repo.sync_daemon();

    let skip_result = repo.git(&["cherry-pick", "--skip"]);
    assert!(
        skip_result.is_err(),
        "skip should apply B and then conflict on C"
    );
    repo.sync_daemon();
    clean_b.assert_committed_lines(crate::lines!["base".human(), "AI_B_VERSION".ai(),]);

    fs::write(&conflict_c_path, "base\nAI_C_VERSION\n").unwrap();
    repo.git(&["add", "conflict_c.txt"]).unwrap();
    repo.git(&["cherry-pick", "--continue"]).unwrap();

    clean_b.assert_committed_lines(crate::lines!["base".human(), "AI_B_VERSION".ai(),]);
    conflict_c.assert_committed_lines(crate::lines!["base".human(), "AI_C_VERSION".ai(),]);
}

fn git_common_dir(repo: &TestRepo) -> PathBuf {
    let raw = repo
        .git_og(&["rev-parse", "--git-common-dir"])
        .expect("rev-parse --git-common-dir should succeed");
    let common_dir = PathBuf::from(raw.trim());
    if common_dir.is_absolute() {
        common_dir
    } else {
        repo.path().join(common_dir)
    }
}

#[test]
fn test_cherry_pick_from_remote_continues_when_notes_import_fails() {
    let source_repo = TestRepo::new();
    let mut source_file = source_repo.filename("file.txt");
    source_file.set_contents(crate::lines!["base"]);
    source_repo.stage_all_and_commit("initial").unwrap();
    source_file.insert_at(1, crate::lines!["AI line".ai()]);
    source_repo.stage_all_and_commit("AI commit").unwrap();
    let ai_commit = source_repo
        .git(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let target_repo = TestRepo::new();
    let mut target_file = target_repo.filename("file.txt");
    target_file.set_contents(crate::lines!["base"]);
    target_repo.stage_all_and_commit("initial").unwrap();

    target_repo
        .git(&[
            "remote",
            "add",
            "source",
            source_repo.path().to_str().unwrap(),
        ])
        .unwrap();
    target_repo
        .git(&["fetch", "source", "refs/heads/*:refs/remotes/source/*"])
        .unwrap();
    let _ = target_repo.git(&["update-ref", "-d", "refs/notes/ai"]);
    let _ = target_repo.git(&["update-ref", "-d", "refs/notes/ai-remote/source"]);

    let notes_dir = git_common_dir(&target_repo).join("refs/notes");
    fs::create_dir_all(&notes_dir).expect("notes dir should be creatable");
    fs::write(notes_dir.join("ai.lock"), "stale lock\n").expect("notes lock should be writable");

    target_repo.git(&["cherry-pick", &ai_commit]).unwrap();
    target_repo.sync_daemon_force();

    target_file.assert_committed_lines(crate::lines![
        "base".human(),
        "AI line".unattributed_human(),
    ]);
}

#[test]
fn test_cherry_pick_no_commit_defers_to_final_commit_tree() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("file.txt");

    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(&file_path, "base\nAI picked line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file.txt"]).unwrap();
    repo.stage_all_and_commit("ai source").unwrap();
    let source_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["cherry-pick", "--no-commit", &source_commit])
        .unwrap();

    fs::write(&file_path, "base\nAI picked line\nlate untracked line\n").unwrap();
    repo.git(&["add", "file.txt"]).unwrap();
    repo.commit("commit no-commit cherry-pick with later edit")
        .unwrap();

    let mut file = repo.filename("file.txt");
    file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "AI picked line".ai(),
        "late untracked line".unattributed_human(),
    ]);
}

crate::reuse_tests_in_worktree!(
    test_single_commit_cherry_pick,
    test_cherry_pick_preserves_human_only_commit_note_metadata,
    test_cherry_pick_preserves_prompt_only_commit_note_metadata,
    test_multiple_commits_cherry_pick,
    test_cherry_pick_with_conflict_and_continue,
    test_cherry_pick_abort,
    test_cherry_pick_no_ai_authorship,
    test_cherry_pick_multiple_ai_sessions,
    test_cherry_pick_identical_trees,
    test_cherry_pick_empty_commits,
    test_cherry_pick_bad_args_dont_corrupt_subsequent_attribution,
    test_cherry_pick_skip_preserves_subsequent_attribution,
    test_cherry_pick_from_remote_without_prefetched_notes,
    test_cherry_pick_from_remote_continues_when_notes_import_fails,
    test_cherry_pick_no_commit_defers_to_final_commit_tree,
    test_cherry_pick_skip_failed_next_conflict_advances_pending_remote_tracking_source,
    test_cherry_pick_skip_then_continue_applies_remaining_commits,
    test_cherry_pick_skip_failed_next_conflict_does_not_double_skip_refcursor_sources,
);
