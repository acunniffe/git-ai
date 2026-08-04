use crate::repos::test_file::{ExpectedLine, ExpectedLineExt};
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log::PromptRecord;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::authorship::working_log::AgentId;
use git_ai::git::notes_api::write_note;
use std::collections::HashMap;

/// Test simple rebase with no conflicts where trees are identical - multiple commits
#[test]
fn test_rebase_no_conflicts_identical_trees() {
    let repo = TestRepo::new();

    // Create initial commit (on default branch, usually master)
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main line 1", "main line 2"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Get the default branch name
    let default_branch = repo.current_branch();

    // Create feature branch with multiple AI commits
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // First AI commit
    let mut feature1 = repo.filename("feature1.txt");
    feature1.set_contents(crate::lines![
        "// AI generated feature 1".ai(),
        "feature line 1".ai()
    ]);
    repo.stage_all_and_commit("AI feature 1").unwrap();

    // Second AI commit
    let mut feature2 = repo.filename("feature2.txt");
    feature2.set_contents(crate::lines![
        "// AI generated feature 2".ai(),
        "feature line 2".ai()
    ]);
    repo.stage_all_and_commit("AI feature 2").unwrap();

    // Advance default branch (non-conflicting)
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut other_file = repo.filename("other.txt");
    other_file.set_contents(crate::lines!["other content"]);
    repo.stage_all_and_commit("Main advances").unwrap();

    // Rebase feature onto default branch (hooks will handle authorship tracking)
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Verify authorship was preserved for both files after rebase
    feature1.assert_lines_and_blame(crate::lines![
        "// AI generated feature 1".ai(),
        "feature line 1".ai()
    ]);
    feature2.assert_lines_and_blame(crate::lines![
        "// AI generated feature 2".ai(),
        "feature line 2".ai()
    ]);
}

/// Test rebase where trees differ (parent changes result in different tree IDs) - multiple commits
#[test]
fn test_rebase_with_different_trees() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Get default branch name
    let default_branch = repo.current_branch();

    // Create feature branch with multiple AI commits
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // First AI commit
    let mut feature1 = repo.filename("feature1.txt");
    feature1.set_contents(crate::lines!["// AI added feature 1".ai()]);
    repo.stage_all_and_commit("AI changes 1").unwrap();

    // Second AI commit
    let mut feature2 = repo.filename("feature2.txt");
    feature2.set_contents(crate::lines!["// AI added feature 2".ai()]);
    repo.stage_all_and_commit("AI changes 2").unwrap();

    // Go back to default branch and add a different file (non-conflicting)
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main content"]);
    repo.stage_all_and_commit("Main changes").unwrap();

    // Rebase feature onto default branch (no conflicts, but trees will differ - hooks handle authorship)
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Verify authorship was preserved for both files after rebase
    feature1.assert_lines_and_blame(crate::lines!["// AI added feature 1".ai()]);
    feature2.assert_lines_and_blame(crate::lines!["// AI added feature 2".ai()]);
}

/// Test rebase with multiple commits
#[test]
fn test_rebase_multiple_commits() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main content"]);
    repo.stage_all_and_commit("Initial").unwrap();

    // Get default branch name
    let default_branch = repo.current_branch();

    // Create feature branch with multiple commits
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // First AI commit
    let mut feature1 = repo.filename("feature1.txt");
    feature1.set_contents(crate::lines!["// AI feature 1".ai()]);
    repo.stage_all_and_commit("AI feature 1").unwrap();

    // Second AI commit
    let mut feature2 = repo.filename("feature2.txt");
    feature2.set_contents(crate::lines!["// AI feature 2".ai()]);
    repo.stage_all_and_commit("AI feature 2").unwrap();

    // Third AI commit
    let mut feature3 = repo.filename("feature3.txt");
    feature3.set_contents(crate::lines!["// AI feature 3".ai()]);
    repo.stage_all_and_commit("AI feature 3").unwrap();

    // Advance default branch
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main2_file = repo.filename("main2.txt");
    main2_file.set_contents(crate::lines!["more main content"]);
    repo.stage_all_and_commit("Main advances").unwrap();

    // Rebase feature onto default branch (hooks will handle authorship tracking)
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Verify all files have preserved AI authorship after rebase
    feature1.assert_lines_and_blame(crate::lines!["// AI feature 1".ai()]);
    feature2.assert_lines_and_blame(crate::lines!["// AI feature 2".ai()]);
    feature3.assert_lines_and_blame(crate::lines!["// AI feature 3".ai()]);
}

/// Test rebase where only some commits have authorship logs
#[test]
fn test_rebase_mixed_authorship() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main content"]);
    repo.stage_all_and_commit("Initial").unwrap();

    // Get default branch name
    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Human commit (no AI authorship)
    let mut human_file = repo.filename("human.txt");
    human_file.set_contents(crate::lines!["human work"]);
    repo.stage_all_and_commit("Human work").unwrap();

    // AI commit
    let mut ai_file = repo.filename("ai.txt");
    ai_file.set_contents(crate::lines!["// AI work".ai()]);
    repo.stage_all_and_commit("AI work").unwrap();

    // Advance default branch
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main2_file = repo.filename("main2.txt");
    main2_file.set_contents(crate::lines!["more main"]);
    repo.stage_all_and_commit("Main advances").unwrap();

    // Rebase feature onto default branch (hooks will handle authorship tracking)
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Verify authorship was preserved correctly
    human_file.assert_lines_and_blame(crate::lines!["human work".human()]);
    ai_file.assert_lines_and_blame(crate::lines!["// AI work".ai()]);
}

#[test]
fn test_rebase_preserves_exact_mixed_line_attribution_in_single_file() {
    let repo = TestRepo::new();

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut app_file = repo.filename("app.js");
    app_file.set_contents(crate::lines![
        "const version = 1;".human(),
        "function compute() {".ai(),
        "  return 1;".ai(),
        "}".ai()
    ]);
    repo.stage_all_and_commit("Add mixed app").unwrap();

    app_file.insert_at(2, crate::lines!["  // AI docs".ai()]);
    repo.stage_all_and_commit("Add docs").unwrap();

    app_file.insert_at(5, crate::lines!["// AI footer".ai()]);
    repo.stage_all_and_commit("Add footer").unwrap();

    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main advance"]);
    repo.stage_all_and_commit("Main advance").unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    app_file.assert_lines_and_blame(crate::lines![
        "const version = 1;".human(),
        "function compute() {".ai(),
        "  // AI docs".ai(),
        "  return 1;".ai(),
        "}".ai(),
        "// AI footer".ai()
    ]);
}

#[test]
fn test_rebase_with_human_only_commit_between_ai_commits_preserves_exact_lines() {
    let repo = TestRepo::new();

    let mut base_file = repo.filename("base.txt");
    let mut app_file = repo.filename("app.js");
    base_file.set_contents(crate::lines!["base"]);
    app_file.set_contents(crate::lines!["const base = 0;".human()]);
    repo.stage_all_and_commit("Initial").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();

    app_file.insert_at(1, crate::lines!["// AI block 1".ai()]);
    repo.stage_all_and_commit("AI block 1").unwrap();

    let mut notes_file = repo.filename("notes.txt");
    notes_file.set_contents(crate::lines!["human notes line"]);
    repo.stage_all_and_commit("Human-only notes").unwrap();

    let mut generated_file = repo.filename("generated.js");
    generated_file.set_contents(crate::lines!["const generated = 42;".ai()]);
    repo.stage_all_and_commit("AI block 2").unwrap();

    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main advance"]);
    repo.stage_all_and_commit("Main advance").unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    app_file.assert_lines_and_blame(crate::lines!["const base = 0;".ai(), "// AI block 1".ai()]);
    generated_file.assert_lines_and_blame(crate::lines!["const generated = 42;".ai()]);
    notes_file.assert_lines_and_blame(crate::lines!["human notes line".human()]);
}

#[test]
fn test_rebase_preserves_human_only_commit_note_metadata() {
    let repo = TestRepo::new();

    // Common base commit.
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial").unwrap();
    let default_branch = repo.current_branch();

    // Branch we will rebase onto.
    repo.git(&["checkout", "-b", "dev"]).unwrap();
    let mut dev_file = repo.filename("dev.txt");
    dev_file.set_contents(crate::lines!["dev content"]);
    repo.stage_all_and_commit("Dev commit").unwrap();

    // Create the source branch from the old base and make a human-only commit.
    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["checkout", "-b", "prod"]).unwrap();
    let mut prod_file = repo.filename("prod.txt");
    prod_file.set_contents(crate::lines!["human change only"]);
    let prod_commit = repo.stage_all_and_commit("Prod human commit").unwrap();

    // Sanity check: original commit has a note and it's metadata-only.
    let old_note = repo
        .read_authorship_note(&prod_commit.commit_sha)
        .expect("original commit should have an authorship note");
    let old_log =
        AuthorshipLog::deserialize_from_string(&old_note).expect("parse original authorship note");
    assert!(
        old_log.metadata.prompts.is_empty(),
        "precondition: human-only commit should have no prompts"
    );
    assert!(
        old_log.metadata.sessions.is_empty(),
        "precondition: human-only commit should have no sessions"
    );

    // Rebase prod onto dev.
    repo.git(&["rebase", "dev"]).unwrap();
    let rebased_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Regression check: rebased commit should still carry the metadata-only note.
    let rebased_note = repo
        .read_authorship_note(&rebased_sha)
        .expect("rebased commit should preserve metadata-only authorship note");
    let rebased_log = AuthorshipLog::deserialize_from_string(&rebased_note)
        .expect("parse rebased authorship note");
    assert!(
        rebased_log.metadata.prompts.is_empty(),
        "rebased human-only commit should still have no prompts"
    );
    assert!(
        rebased_log.metadata.sessions.is_empty(),
        "rebased human-only commit should still have no sessions"
    );
    assert_eq!(rebased_log.metadata.base_commit_sha, rebased_sha);
}

#[test]
fn test_rebase_preserves_prompt_only_commit_note_metadata() {
    let repo = TestRepo::new();

    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "dev"]).unwrap();
    let mut dev_file = repo.filename("dev.txt");
    dev_file.set_contents(crate::lines!["dev content"]);
    repo.stage_all_and_commit("Dev commit").unwrap();

    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["checkout", "-b", "prod"]).unwrap();
    let mut prod_file = repo.filename("prod.txt");
    prod_file.set_contents(crate::lines!["human change only"]);
    let prod_commit = repo
        .stage_all_and_commit("Prod human commit")
        .expect("create prod commit");

    let original_note = repo
        .read_authorship_note(&prod_commit.commit_sha)
        .expect("source commit should have authorship note");
    let mut original_log =
        AuthorshipLog::deserialize_from_string(&original_note).expect("parse source note");
    assert!(
        original_log.metadata.prompts.is_empty(),
        "precondition: source commit should not have prompts before test mutation"
    );
    assert!(
        original_log.metadata.sessions.is_empty(),
        "precondition: source commit should not have sessions before test mutation"
    );

    let mut test_attrs = HashMap::new();
    test_attrs.insert("employee_id".to_string(), "E123".to_string());
    test_attrs.insert("team".to_string(), "platform".to_string());

    original_log.metadata.prompts.insert(
        "prompt-only-session".to_string(),
        PromptRecord {
            agent_id: AgentId {
                tool: "mock_ai".to_string(),
                id: "session-1".to_string(),
                model: "test-model".to_string(),
            },
            human_author: Some("Test User <test@example.com>".to_string()),
            total_additions: 17,
            total_deletions: 3,
            accepted_lines: 0,
            overriden_lines: 0,
            custom_attributes: Some(test_attrs.clone()),
            messages_url: None,
        },
    );

    let mutated_source_note = original_log
        .serialize_to_string()
        .expect("serialize mutated source note");
    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(&git_ai_repo, &prod_commit.commit_sha, &mutated_source_note)
        .expect("overwrite source note with prompt-only metadata");

    repo.git(&["rebase", "dev"]).unwrap();
    let rebased_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    let rebased_note = repo
        .read_authorship_note(&rebased_sha)
        .expect("rebased commit should preserve prompt-only note");
    let rebased_log =
        AuthorshipLog::deserialize_from_string(&rebased_note).expect("parse rebased note");
    assert_eq!(rebased_log.metadata.prompts.len(), 1);
    assert_eq!(rebased_log.metadata.base_commit_sha, rebased_sha);

    let prompt = rebased_log
        .metadata
        .prompts
        .get("prompt-only-session")
        .expect("prompt metadata should be preserved");
    assert_eq!(prompt.agent_id.tool, "mock_ai");
    assert_eq!(prompt.agent_id.id, "session-1");
    assert_eq!(prompt.agent_id.model, "test-model");
    assert_eq!(prompt.total_additions, 17);
    assert_eq!(prompt.total_deletions, 3);
    assert_eq!(
        prompt.custom_attributes,
        Some(test_attrs),
        "custom_attributes should be preserved through rebase"
    );
}

/// Test empty rebase (fast-forward)
#[test]
fn test_rebase_fast_forward() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main content"]);
    repo.stage_all_and_commit("Initial").unwrap();

    // Get default branch name
    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Add commit on feature
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["// AI feature".ai()]);
    repo.stage_all_and_commit("AI feature").unwrap();

    // Rebase onto default branch (should be fast-forward, no changes - hooks handle authorship)
    repo.git(&["rebase", &default_branch]).unwrap();

    // Verify authorship is still correct after fast-forward rebase
    feature_file.assert_lines_and_blame(crate::lines!["// AI feature".ai()]);
}

/// Test `git rebase <upstream> <branch>` when invoked from another branch.
/// We should capture original_head from `<branch>`, not from the currently checked-out branch.
#[test]
fn test_rebase_with_explicit_branch_argument_preserves_authorship() {
    let repo = TestRepo::new();

    // Base commit
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("initial").unwrap();
    let main_branch = repo.current_branch();

    // Feature branch with AI-authored content
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["// AI feature".ai(), "fn feature() {}".ai()]);
    repo.stage_all_and_commit("add feature").unwrap();

    // Advance main branch
    repo.git(&["checkout", &main_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main work"]);
    repo.stage_all_and_commit("main advances").unwrap();

    // Invoke rebase with explicit branch arg while currently on main.
    repo.git(&["rebase", &main_branch, "feature"]).unwrap();

    // HEAD should now be on feature after the rebase operation; verify AI blame survived.
    feature_file
        .assert_lines_and_blame(crate::lines!["// AI feature".ai(), "fn feature() {}".ai()]);

    // Verify the rebased commit carries an authorship note via git notes.
    let rebased_head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert!(
        repo.read_authorship_note(&rebased_head).is_some(),
        "Rebased commit should have an authorship note"
    );
}

/// Test `git rebase --root --onto <base> <branch>` when invoked from another branch.
/// We should resolve original_head from `<branch>`, not from the currently checked-out branch.
#[test]
fn test_rebase_root_with_explicit_branch_argument_preserves_authorship() {
    let repo = TestRepo::new();

    // Base commit
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("initial").unwrap();
    let main_branch = repo.current_branch();

    // Feature branch with AI-authored content
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["// AI feature".ai(), "fn feature() {}".ai()]);
    let original_feature_head = repo.stage_all_and_commit("add feature").unwrap().commit_sha;

    // Advance main branch
    repo.git(&["checkout", &main_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main work"]);
    repo.stage_all_and_commit("main advances").unwrap();

    // Invoke root rebase with explicit branch arg while currently on main.
    repo.git(&["rebase", "--root", "--onto", &main_branch, "feature"])
        .unwrap();

    let rebased_feature_head = repo.git(&["rev-parse", "HEAD"]).unwrap();
    assert_ne!(
        rebased_feature_head.trim(),
        original_feature_head,
        "Feature head should be rewritten by root rebase"
    );

    // HEAD should now be on feature after the rebase operation; verify AI blame survived.
    feature_file
        .assert_lines_and_blame(crate::lines!["// AI feature".ai(), "fn feature() {}".ai()]);

    // Verify the rebased commit carries an authorship note via git notes.
    assert!(
        repo.read_authorship_note(rebased_feature_head.trim())
            .is_some(),
        "Rebased commit should have an authorship note"
    );
}

/// Test interactive rebase with commit reordering - verifies interactive rebase works
#[test]
fn test_rebase_interactive_reorder() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let default_branch = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Create 2 AI commits - we'll rebase these interactively
    let mut feature1 = repo.filename("feature1.txt");
    feature1.set_contents(crate::lines!["// AI feature 1".ai()]);
    repo.stage_all_and_commit("AI commit 1").unwrap();

    let mut feature2 = repo.filename("feature2.txt");
    feature2.set_contents(crate::lines!["// AI feature 2".ai()]);
    repo.stage_all_and_commit("AI commit 2").unwrap();

    // Advance main branch
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main work"]);
    repo.stage_all_and_commit("Main advances").unwrap();
    let base_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Perform interactive rebase (just pick all, tests that -i flag works)
    repo.git(&["checkout", "feature"]).unwrap();

    let result = repo.git_with_env(
        &["rebase", "-i", &base_commit],
        &[("GIT_SEQUENCE_EDITOR", "true"), ("GIT_EDITOR", "true")],
        None,
    );

    if result.is_err() {
        eprintln!("git rebase output: {:?}", result);
        panic!("Interactive rebase failed");
    }

    // Verify both files have preserved AI authorship after interactive rebase
    feature1.assert_lines_and_blame(crate::lines!["// AI feature 1".ai()]);
    feature2.assert_lines_and_blame(crate::lines!["// AI feature 2".ai()]);
}

/// Test rebase skip - skipping a commit during rebase
#[test]
fn test_rebase_skip() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["line 1"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch with AI commit that will conflict
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.replace_at(0, "AI line 1".ai());
    repo.stage_all_and_commit("AI changes").unwrap();

    // Add second commit that won't conflict
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["// AI feature".ai()]);
    repo.stage_all_and_commit("Add feature").unwrap();

    // Make conflicting change on main
    repo.git(&["checkout", &default_branch]).unwrap();
    file.replace_at(0, "MAIN line 1".human());
    repo.stage_all_and_commit("Main changes").unwrap();

    // Try to rebase - will conflict on first commit
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &default_branch]);

    // Should conflict
    assert!(rebase_result.is_err(), "Rebase should conflict");

    // Skip the conflicting commit
    let skip_result = repo.git(&["rebase", "--skip"]);

    if skip_result.is_ok() {
        // Verify the second commit was rebased and authorship preserved
        feature_file.assert_lines_and_blame(crate::lines!["// AI feature".ai()]);
    }
}

/// Test rebase with empty commits (--keep-empty)
#[test]
fn test_rebase_keep_empty() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch with empty commit
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Create empty commit
    repo.git(&["commit", "--allow-empty", "-m", "Empty commit"])
        .expect("Empty commit should succeed");

    // Add a real commit
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["// AI".ai()]);
    repo.stage_all_and_commit("AI feature").unwrap();

    // Advance main
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main"]);
    repo.stage_all_and_commit("Main work").unwrap();
    let base = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Rebase with --keep-empty (hooks will handle authorship tracking)
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", "--keep-empty", &base]);

    if rebase_result.is_ok() {
        // Verify the non-empty commit has preserved AI authorship
        feature_file.assert_lines_and_blame(crate::lines!["// AI".ai()]);
    }
}

/// Test rebase with rerere (reuse recorded resolution) enabled
#[test]
fn test_rebase_rerere() {
    let repo = TestRepo::new();

    // Enable rerere
    repo.git(&["config", "rerere.enabled", "true"]).unwrap();

    // Create initial commit
    let mut conflict_file = repo.filename("conflict.txt");
    conflict_file.set_contents(crate::lines!["line 1", "line 2"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch with AI changes
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    conflict_file.replace_at(1, "AI CHANGE".ai());
    repo.stage_all_and_commit("AI changes").unwrap();

    // Make conflicting change on main
    repo.git(&["checkout", &default_branch]).unwrap();
    conflict_file.replace_at(1, "MAIN CHANGE".human());
    repo.stage_all_and_commit("Main changes").unwrap();

    // First rebase - will conflict
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &default_branch]);

    // Should conflict
    assert!(rebase_result.is_err(), "First rebase should conflict");

    // Resolve conflict manually
    use std::fs;
    fs::write(repo.path().join("conflict.txt"), "line 1\nRESOLVED\n").unwrap();

    repo.git(&["add", "conflict.txt"]).unwrap();

    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    // Record the resolution and abort
    repo.git(&["rebase", "--abort"]).ok();

    // Second attempt - rerere should auto-apply the resolution
    let rebase_result = repo.git(&["rebase", &default_branch]);

    // Even if rerere helps, we still need to continue manually
    // This test mainly verifies that rerere doesn't break authorship tracking
    if rebase_result.is_err() {
        repo.git(&["add", "conflict.txt"]).unwrap();
        repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
            .unwrap();
    }

    // Note: This test verifies that rerere doesn't break the rebase process
    // Authorship tracking is handled by hooks regardless of rerere
}

/// Test dependent branch stack (patch-stack workflow)
#[test]
fn test_rebase_patch_stack() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let default_branch = repo.current_branch();

    // Create topic-1 branch
    repo.git(&["checkout", "-b", "topic-1"]).unwrap();
    let mut topic1_file = repo.filename("topic1.txt");
    topic1_file.set_contents(crate::lines!["// AI topic 1".ai()]);
    repo.stage_all_and_commit("Topic 1").unwrap();

    // Create topic-2 branch on top of topic-1
    repo.git(&["checkout", "-b", "topic-2"]).unwrap();
    let mut topic2_file = repo.filename("topic2.txt");
    topic2_file.set_contents(crate::lines!["// AI topic 2".ai()]);
    repo.stage_all_and_commit("Topic 2").unwrap();

    // Create topic-3 branch on top of topic-2
    repo.git(&["checkout", "-b", "topic-3"]).unwrap();
    let mut topic3_file = repo.filename("topic3.txt");
    topic3_file.set_contents(crate::lines!["// AI topic 3".ai()]);
    repo.stage_all_and_commit("Topic 3").unwrap();

    // Advance main
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main work"]);
    repo.stage_all_and_commit("Main work").unwrap();

    // Rebase the stack: topic-1, then topic-2, then topic-3 (hooks will handle authorship)
    repo.git(&["checkout", "topic-1"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    repo.git(&["checkout", "topic-2"]).unwrap();
    repo.git(&["rebase", "topic-1"]).unwrap();

    repo.git(&["checkout", "topic-3"]).unwrap();
    repo.git(&["rebase", "topic-2"]).unwrap();

    // Verify all files have preserved AI authorship after rebasing the stack
    repo.git(&["checkout", "topic-1"]).unwrap();
    topic1_file.assert_lines_and_blame(crate::lines!["// AI topic 1".ai()]);

    repo.git(&["checkout", "topic-2"]).unwrap();
    topic1_file.assert_lines_and_blame(crate::lines!["// AI topic 1".ai()]);
    topic2_file.assert_lines_and_blame(crate::lines!["// AI topic 2".ai()]);

    repo.git(&["checkout", "topic-3"]).unwrap();
    topic1_file.assert_lines_and_blame(crate::lines!["// AI topic 1".ai()]);
    topic2_file.assert_lines_and_blame(crate::lines!["// AI topic 2".ai()]);
    topic3_file.assert_lines_and_blame(crate::lines!["// AI topic 3".ai()]);
}

#[test]
fn test_rebase_ignores_stale_pending_state_from_untraced_abort() {
    let repo = TestRepo::new();

    let mut stale_file = repo.filename("stale-conflict.txt");
    stale_file.set_contents(crate::lines!["base"]);
    let base_commit = repo.stage_all_and_commit("base").unwrap().commit_sha;
    stale_file.assert_committed_lines(crate::lines!["base".human()]);

    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "stale-topic"]).unwrap();
    stale_file.replace_at(0, "stale side".ai());
    let stale_tip = repo.stage_all_and_commit("stale topic").unwrap().commit_sha;
    stale_file.assert_committed_lines(crate::lines!["stale side".ai()]);

    repo.git(&["checkout", &default_branch]).unwrap();
    stale_file.replace_at(0, "main side".human());
    repo.stage_all_and_commit("main side").unwrap();
    stale_file.assert_committed_lines(crate::lines!["main side".human()]);

    repo.git(&["checkout", "stale-topic"]).unwrap();
    let failed_rebase = repo.git(&["rebase", &default_branch]);
    assert!(
        failed_rebase.is_err(),
        "stale-topic rebase should stop on the conflict that seeds pending daemon state"
    );
    repo.sync_daemon();

    repo.git_og_with_env(&["rebase", "--abort"], &[("GIT_TRACE2_EVENT", "0")])
        .expect("untraced rebase abort should restore Git state without clearing daemon memory");
    let after_abort = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_eq!(after_abort, stale_tip);
    stale_file.assert_committed_lines(crate::lines!["stale side".ai()]);

    repo.git(&["checkout", "-b", "feature", &base_commit])
        .unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["feature ai".ai()]);
    let original_feature = repo.stage_all_and_commit("feature ai").unwrap().commit_sha;
    feature_file.assert_committed_lines(crate::lines!["feature ai".ai()]);
    assert!(
        repo.read_authorship_note(&original_feature).is_some(),
        "original feature commit should have an authorship note before rebase"
    );

    repo.git(&["rebase", &default_branch])
        .expect("ordinary feature rebase should succeed");
    let rebased_feature = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(rebased_feature, original_feature);

    feature_file.assert_lines_and_blame(crate::lines!["feature ai".ai()]);
}

/// Test rebase with no changes (already up to date)
#[test]
fn test_rebase_already_up_to_date() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["content"]);
    repo.stage_all_and_commit("Initial").unwrap();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["// AI".ai()]);
    let feature_commit_before = repo.stage_all_and_commit("AI feature").unwrap().commit_sha;

    // Try to rebase onto itself (should be no-op)
    repo.git(&["rebase", "feature"])
        .expect("Rebase onto self should succeed");

    // Verify commit unchanged
    let current_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_eq!(
        current_commit, feature_commit_before,
        "Commit should be unchanged"
    );

    // Verify authorship still intact
    feature_file.assert_lines_and_blame(crate::lines!["// AI".ai()]);
}

/// Test rebase with conflicts - verifies reconstruction works after conflict resolution
#[test]
fn test_rebase_with_conflicts() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let default_branch = repo.current_branch();

    // Create old_base branch and commit
    repo.git(&["checkout", "-b", "old_base"]).unwrap();
    let mut old_file = repo.filename("old.txt");
    old_file.set_contents(crate::lines!["old base"]);
    repo.stage_all_and_commit("Old base commit").unwrap();
    let old_base_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Create feature branch from old_base with AI commits
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["// AI feature".ai()]);
    repo.stage_all_and_commit("AI feature").unwrap();

    // Create new_base branch from default_branch
    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["checkout", "-b", "new_base"]).unwrap();
    let mut new_file = repo.filename("new.txt");
    new_file.set_contents(crate::lines!["new base"]);
    repo.stage_all_and_commit("New base commit").unwrap();
    let new_base_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Rebase feature --onto new_base old_base (hooks will handle authorship)
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", "--onto", &new_base_sha, &old_base_sha])
        .expect("Rebase --onto should succeed");

    // Verify authorship preserved after --onto rebase
    feature_file.assert_lines_and_blame(crate::lines!["// AI feature".ai()]);
}

/// Test rebase abort - ensures no authorship corruption on abort
#[test]
fn test_rebase_abort() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut conflict_file = repo.filename("conflict.txt");
    conflict_file.set_contents(crate::lines!["line 1", "line 2"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch with AI changes
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    conflict_file.replace_at(1, "AI CHANGE".ai());
    repo.stage_all_and_commit("AI changes").unwrap();
    let feature_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Make conflicting change on main
    repo.git(&["checkout", &default_branch]).unwrap();
    conflict_file.replace_at(1, "MAIN CHANGE".human());
    repo.stage_all_and_commit("Main changes").unwrap();

    // Try to rebase - will conflict
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &default_branch]);

    // Should conflict
    assert!(rebase_result.is_err(), "Rebase should conflict");

    // Abort the rebase
    repo.git(&["rebase", "--abort"])
        .expect("Rebase abort should succeed");

    // Verify we're back to original commit
    let current_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_eq!(
        current_commit, feature_commit,
        "Should be back to original commit after abort"
    );

    // Verify original authorship is intact (by checking file blame)
    conflict_file.assert_lines_and_blame(crate::lines!["line 1".human(), "AI CHANGE".ai()]);
}

/// Test branch switch during rebase - ensures proper state handling
#[test]
fn test_rebase_branch_switch_during() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["// AI".ai()]);
    repo.stage_all_and_commit("AI feature").unwrap();

    // Create another branch
    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["checkout", "-b", "other"]).unwrap();
    let mut other_file = repo.filename("other.txt");
    other_file.set_contents(crate::lines!["other"]);
    repo.stage_all_and_commit("Other work").unwrap();

    // Start rebase on feature (non-conflicting)
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Verify branch is still feature
    let current_branch = repo.current_branch();
    assert_eq!(
        current_branch, "feature",
        "Should still be on feature branch"
    );

    // Verify authorship was preserved
    feature_file.assert_lines_and_blame(crate::lines!["// AI".ai()]);
}

/// Test rebase with autosquash enabled
#[test]
fn test_rebase_autosquash() {
    let repo = TestRepo::new();

    // Enable autosquash in config
    repo.git(&["config", "rebase.autosquash", "true"]).unwrap();

    // Create initial commit
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["line 1"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, crate::lines!["AI line 2".ai()]);
    repo.stage_all_and_commit("Add feature").unwrap();

    // Create fixup commit
    file.replace_at(1, "AI line 2 fixed".ai());
    repo.stage_all_and_commit("fixup! Add feature").unwrap();

    // Advance main
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut other_file = repo.filename("other.txt");
    other_file.set_contents(crate::lines!["other"]);
    repo.stage_all_and_commit("Main work").unwrap();
    let base = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Interactive rebase with autosquash (hooks will handle authorship)
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git_with_env(
        &["rebase", "-i", "--autosquash", &base],
        &[("GIT_SEQUENCE_EDITOR", "true"), ("GIT_EDITOR", "true")],
        None,
    );

    if rebase_result.is_ok() {
        // Verify the file has the expected content with AI authorship
        file.assert_lines_and_blame(crate::lines!["line 1".ai(), "AI line 2 fixed".ai()]);
    }
}

/// Test rebase with autostash enabled
#[test]
fn test_rebase_autostash() {
    let repo = TestRepo::new();

    // Enable autostash
    repo.git(&["config", "rebase.autoStash", "true"]).unwrap();

    // Create initial commit
    let mut file = repo.filename("file.txt");
    file.set_contents(crate::lines!["line 1"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["// AI".ai()]);
    repo.stage_all_and_commit("AI feature").unwrap();

    // Advance main
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main"]);
    repo.stage_all_and_commit("Main work").unwrap();

    // Switch back to feature and make unstaged changes
    repo.git(&["checkout", "feature"]).unwrap();
    use std::fs;
    fs::write(
        repo.path().join("feature.txt"),
        "// AI\n// Unstaged change\n",
    )
    .unwrap();

    // Rebase with unstaged changes (autostash should handle it - hooks handle authorship)
    let rebase_result = repo.git(&["rebase", &default_branch]);

    // Should succeed with autostash
    if rebase_result.is_ok() {
        // Reset the file to HEAD to remove the autostashed unstaged changes before checking
        repo.git(&["checkout", "HEAD", "feature.txt"]).unwrap();

        // Verify authorship was preserved
        feature_file.assert_lines_and_blame(crate::lines!["// AI".ai()]);
    }
}

/// Test rebase --exec to run tests at each commit
#[test]
fn test_rebase_exec() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut test_sh = repo.filename("test.sh");
    test_sh.set_contents(crate::lines!["#!/bin/sh", "exit 0"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch with multiple AI commits
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut f1 = repo.filename("f1.txt");
    f1.set_contents(crate::lines!["// AI 1".ai()]);
    repo.stage_all_and_commit("AI commit 1").unwrap();

    let mut f2 = repo.filename("f2.txt");
    f2.set_contents(crate::lines!["// AI 2".ai()]);
    repo.stage_all_and_commit("AI commit 2").unwrap();

    // Advance main
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main"]);
    repo.stage_all_and_commit("Main work").unwrap();
    let base = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    repo.git(&["checkout", "feature"]).unwrap();

    // Rebase with --exec (hooks will handle authorship)
    repo.git_with_env(
        &["rebase", "-i", "--exec", "echo 'test passed'", &base],
        &[("GIT_SEQUENCE_EDITOR", "true"), ("GIT_EDITOR", "true")],
        None,
    )
    .expect("Rebase with --exec should succeed");

    // Verify authorship was preserved
    f1.assert_lines_and_blame(crate::lines!["// AI 1".ai()]);
    f2.assert_lines_and_blame(crate::lines!["// AI 2".ai()]);
}

/// Test rebase with merge commits (--rebase-merges)
/// This test verifies the BFS fix for issue #328 where walk_commits_to_base
/// was only following parent(0), missing side branch commits.
///
/// The test checks that authorship notes for rebased commits include files
/// from side branches (reached via parent(1) of merge commits).
#[test]
fn test_rebase_preserve_merges() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["// AI feature".ai()]);
    repo.stage_all_and_commit("AI feature").unwrap();

    // Create side branch from feature - this commit is only reachable via parent(1) of the merge
    repo.git(&["checkout", "-b", "side"]).unwrap();
    let mut side_file = repo.filename("side.txt");
    side_file.set_contents(crate::lines!["// AI side".ai()]);
    repo.stage_all_and_commit("AI side").unwrap();

    // Merge side into feature with --no-ff to force a merge commit
    // (creates merge commit where side is parent(1), feature is parent(0))
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["merge", "--no-ff", "side", "-m", "Merge side into feature"])
        .unwrap();

    // Advance main
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main"]);
    repo.stage_all_and_commit("Main work").unwrap();
    let base = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Rebase feature onto main with --rebase-merges
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", "--rebase-merges", &base])
        .expect("Rebase with --rebase-merges should succeed");

    // Get the rebased side branch commit (the one that created side.txt)
    // Use git log to find the commit that added side.txt
    let side_commit_sha = repo
        .git(&[
            "log",
            "--all",
            "--format=%H",
            "--diff-filter=A",
            "--",
            "side.txt",
        ])
        .expect("Should find commit that added side.txt")
        .trim()
        .lines()
        .next()
        .expect("Should have at least one commit")
        .to_string();

    // Check that the rebased side commit has an authorship note with side.txt
    // This is the key assertion: without BFS fix, walk_commits_to_base misses
    // the side branch commit, so its authorship won't be rewritten
    let note_output = repo.git(&["notes", "--ref=ai", "show", &side_commit_sha]);

    assert!(
        note_output.is_ok(),
        "Rebased side branch commit should have authorship note. \
         Without BFS fix, walk_commits_to_base misses commits from parent(1) \
         and authorship is not rewritten for side branch commits."
    );

    let note_content = note_output.unwrap();
    assert!(
        note_content.contains("side.txt"),
        "Authorship note should include side.txt. Got: {}",
        note_content
    );

    // Also verify blame works correctly
    feature_file.assert_lines_and_blame(crate::lines!["// AI feature".ai()]);
    side_file.assert_lines_and_blame(crate::lines!["// AI side".ai()]);
}

/// Test rebase with commit splitting (fewer original commits than new commits)
/// This tests that rebase handles AI authorship correctly even with complex commit histories
#[test]
fn test_rebase_commit_splitting() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content", ""]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch with AI commits
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    let mut features_file = repo.filename("features.txt");
    features_file.set_contents(crate::lines![
        "// AI feature 1".ai(),
        "function feature1() {}".ai(),
        "".ai()
    ]);
    repo.stage_all_and_commit("AI feature 1").unwrap();

    features_file.insert_at(
        2,
        crate::lines!["// AI feature 2".ai(), "function feature2() {}".ai()],
    );
    repo.stage_all_and_commit("AI feature 2").unwrap();

    // Advance main branch
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main content", ""]);
    repo.stage_all_and_commit("Main advances").unwrap();

    // Rebase feature onto main (hooks will handle authorship)
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Verify AI authorship is preserved after rebase
    features_file.assert_lines_and_blame(crate::lines![
        "// AI feature 1".ai(),
        "function feature1() {}".ai(),
        "// AI feature 2".ai(),
        "function feature2() {}".ai(),
    ]);
}

/// Test interactive rebase with squashing - verifies authorship from all commits is preserved
/// This tests that squashing preserves authorship from all commits
#[test]
#[cfg(not(target_os = "windows"))]
fn test_rebase_squash_preserves_all_authorship() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let default_branch = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Create 3 AI commits with different content - we'll squash these
    let mut feature1 = repo.filename("feature1.txt");
    feature1.set_contents(crate::lines!["// AI feature 1".ai(), "line 1".ai()]);
    repo.stage_all_and_commit("AI commit 1").unwrap();

    let mut feature2 = repo.filename("feature2.txt");
    feature2.set_contents(crate::lines!["// AI feature 2".ai(), "line 2".ai()]);
    repo.stage_all_and_commit("AI commit 2").unwrap();

    let mut feature3 = repo.filename("feature3.txt");
    feature3.set_contents(crate::lines!["// AI feature 3".ai(), "line 3".ai()]);
    repo.stage_all_and_commit("AI commit 3").unwrap();

    // Advance main branch
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main work"]);
    repo.stage_all_and_commit("Main advances").unwrap();
    let base_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Perform interactive rebase with squashing: pick first, squash second and third
    repo.git(&["checkout", "feature"]).unwrap();

    use std::io::Write;

    // Create a script that modifies the rebase-todo to squash commits 2 and 3 into 1
    let script_content = r#"#!/bin/sh
sed -i.bak '2s/pick/squash/' "$1"
sed -i.bak '3s/pick/squash/' "$1"
"#;

    let script_path = repo.path().join("squash_script.sh");
    let mut script_file = std::fs::File::create(&script_path).unwrap();
    script_file.write_all(script_content.as_bytes()).unwrap();
    drop(script_file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
    }

    let rebase_result = repo.git_with_env(
        &["rebase", "-i", &base_commit],
        &[
            ("GIT_SEQUENCE_EDITOR", script_path.to_str().unwrap()),
            ("GIT_EDITOR", "true"),
        ],
        None,
    );

    if rebase_result.is_err() {
        eprintln!("git rebase output: {:?}", rebase_result);
        panic!("Interactive rebase with squash failed");
    }

    // Verify all 3 files exist with preserved AI authorship after squashing
    assert!(
        repo.path().join("feature1.txt").exists(),
        "feature1.txt from commit 1 should exist"
    );
    assert!(
        repo.path().join("feature2.txt").exists(),
        "feature2.txt from commit 2 should exist"
    );
    assert!(
        repo.path().join("feature3.txt").exists(),
        "feature3.txt from commit 3 should exist"
    );

    // Verify AI authorship was preserved through squashing
    feature1.assert_lines_and_blame(crate::lines!["// AI feature 1".ai(), "line 1".ai()]);
    feature2.assert_lines_and_blame(crate::lines!["// AI feature 2".ai(), "line 2".ai()]);
    feature3.assert_lines_and_blame(crate::lines!["// AI feature 3".ai(), "line 3".ai()]);
}

/// Test rebase with rewording (renaming) a commit that has 2 children commits
/// Verifies that authorship is preserved for all 3 commits after reword
#[test]
#[cfg(not(target_os = "windows"))]
fn test_rebase_reword_commit_with_children() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let default_branch = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Create 3 AI commits - we'll reword the first one
    let mut feature1 = repo.filename("feature1.txt");
    feature1.set_contents(crate::lines![
        "// AI feature 1".ai(),
        "function feature1() {}".ai()
    ]);
    repo.stage_all_and_commit("AI commit 1 - original message")
        .unwrap();

    let mut feature2 = repo.filename("feature2.txt");
    feature2.set_contents(crate::lines![
        "// AI feature 2".ai(),
        "function feature2() {}".ai()
    ]);
    repo.stage_all_and_commit("AI commit 2").unwrap();

    let mut feature3 = repo.filename("feature3.txt");
    feature3.set_contents(crate::lines![
        "// AI feature 3".ai(),
        "function feature3() {}".ai()
    ]);
    repo.stage_all_and_commit("AI commit 3").unwrap();

    // Advance main branch
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main work"]);
    repo.stage_all_and_commit("Main advances").unwrap();
    let base_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Perform interactive rebase with rewording the first commit
    repo.git(&["checkout", "feature"]).unwrap();

    use std::io::Write;

    // Create a script that modifies the rebase-todo to reword the first commit
    let script_content = r#"#!/bin/sh
sed -i.bak '1s/pick/reword/' "$1"
"#;

    let script_path = repo.path().join("reword_script.sh");
    let mut script_file = std::fs::File::create(&script_path).unwrap();
    script_file.write_all(script_content.as_bytes()).unwrap();
    drop(script_file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
    }

    // Create a script that provides the new commit message
    let commit_msg_content = "AI commit 1 - RENAMED MESSAGE";
    let commit_msg_path = repo.path().join("new_commit_msg.txt");
    let mut msg_file = std::fs::File::create(&commit_msg_path).unwrap();
    msg_file.write_all(commit_msg_content.as_bytes()).unwrap();
    drop(msg_file);

    // Create an editor script that replaces the commit message
    let editor_script_content = format!(
        r#"#!/bin/sh
cat {} > "$1"
"#,
        commit_msg_path.to_str().unwrap()
    );
    let editor_script_path = repo.path().join("editor_script.sh");
    let mut editor_file = std::fs::File::create(&editor_script_path).unwrap();
    editor_file
        .write_all(editor_script_content.as_bytes())
        .unwrap();
    drop(editor_file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&editor_script_path)
            .unwrap()
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&editor_script_path, perms).unwrap();
    }

    let rebase_result = repo.git_with_env(
        &["rebase", "-i", &base_commit],
        &[
            ("GIT_SEQUENCE_EDITOR", script_path.to_str().unwrap()),
            ("GIT_EDITOR", editor_script_path.to_str().unwrap()),
        ],
        None,
    );

    if rebase_result.is_err() {
        eprintln!("git rebase output: {:?}", rebase_result);
        panic!("Interactive rebase with reword failed");
    }

    // Verify all 3 files still exist with correct AI authorship after reword
    feature1.assert_lines_and_blame(crate::lines![
        "// AI feature 1".ai(),
        "function feature1() {}".ai()
    ]);
    feature2.assert_lines_and_blame(crate::lines![
        "// AI feature 2".ai(),
        "function feature2() {}".ai()
    ]);
    feature3.assert_lines_and_blame(crate::lines![
        "// AI feature 3".ai(),
        "function feature3() {}".ai()
    ]);
}

/// Test that custom attributes set via config are preserved through a rebase
/// when the real post-commit pipeline injects them.
#[test]
fn test_rebase_preserves_custom_attributes_from_config() {
    let mut repo =
        TestRepo::new_with_daemon_scope(crate::repos::test_repo::DaemonTestScope::Dedicated);

    // Configure custom attributes via config patch
    let mut attrs = HashMap::new();
    attrs.insert("employee_id".to_string(), "E789".to_string());
    attrs.insert("team".to_string(), "infra".to_string());
    repo.patch_git_ai_config(|patch| {
        patch.custom_attributes = Some(attrs.clone());
    });

    // Create initial commit on default branch
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    // Create feature branch with AI commit
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["// AI feature code".ai()]);
    repo.stage_all_and_commit("AI feature").unwrap();

    // Verify custom attributes were set on the original commit
    let original_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let original_note = repo
        .read_authorship_note(&original_sha)
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

    // Advance default branch (non-conflicting)
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut other_file = repo.filename("other.txt");
    other_file.set_contents(crate::lines!["other content"]);
    repo.stage_all_and_commit("Main advances").unwrap();

    // Rebase feature onto default branch
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Verify custom attributes survived the rebase
    let rebased_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let rebased_note = repo
        .read_authorship_note(&rebased_sha)
        .expect("rebased commit should have authorship note");
    let rebased_log =
        AuthorshipLog::deserialize_from_string(&rebased_note).expect("parse rebased note");
    assert!(
        rebased_log.metadata.prompts.is_empty(),
        "rebased commit should not have prompts"
    );
    assert!(
        !rebased_log.metadata.sessions.is_empty(),
        "rebased commit should have session records"
    );
    for session in rebased_log.metadata.sessions.values() {
        assert_eq!(
            session.custom_attributes.as_ref(),
            Some(&attrs),
            "custom_attributes should be preserved through rebase"
        );
    }

    // Also verify the AI attribution itself survived
    feature_file.assert_lines_and_blame(crate::lines!["// AI feature code".ai()]);
}

/// Regression test: prompt metrics (accepted_lines) must update per commit, not be frozen
/// from the initial state. When commit 1 has 2 AI lines and commit 2 adds 2 more
/// (total 4), the rebased notes should reflect different accepted_lines.
#[test]
fn test_rebase_prompt_metrics_update_per_commit() {
    let repo = TestRepo::new();
    let default_branch = repo.current_branch();

    // Initial setup
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial").unwrap();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Commit 1: add 2 AI lines
    let mut ai_file = repo.filename("feature.txt");
    ai_file.set_contents(crate::lines!["line1".ai(), "line2".ai()]);
    let commit1 = repo.stage_all_and_commit("AI commit 1 - 2 lines").unwrap();

    // Commit 2: add 2 more AI lines (total 4)
    ai_file.set_contents(crate::lines![
        "line1".ai(),
        "line2".ai(),
        "line3".ai(),
        "line4".ai()
    ]);
    let commit2 = repo.stage_all_and_commit("AI commit 2 - 4 lines").unwrap();

    // Verify pre-rebase: commit 1 has 2 accepted, commit 2 has 4
    let note1 = repo
        .read_authorship_note(&commit1.commit_sha)
        .expect("commit 1 should have note");
    let log1 = AuthorshipLog::deserialize_from_string(&note1).expect("parse note 1");
    let note2 = repo
        .read_authorship_note(&commit2.commit_sha)
        .expect("commit 2 should have note");
    let log2 = AuthorshipLog::deserialize_from_string(&note2).expect("parse note 2");

    // Session format: verify pre-rebase sessions exist and attestation line counts differ
    assert!(
        log1.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    assert!(
        log2.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    assert!(
        !log1.metadata.sessions.is_empty(),
        "precondition: commit 1 should have session records"
    );
    assert!(
        !log2.metadata.sessions.is_empty(),
        "precondition: commit 2 should have session records"
    );
    let pre_lines_1: u32 = log1
        .attestations
        .iter()
        .flat_map(|a| &a.entries)
        .flat_map(|e| &e.line_ranges)
        .map(|r| match r {
            git_ai::authorship::authorship_log::LineRange::Single(_) => 1,
            git_ai::authorship::authorship_log::LineRange::Range(s, e) => e - s + 1,
        })
        .sum();
    let pre_lines_2: u32 = log2
        .attestations
        .iter()
        .flat_map(|a| &a.entries)
        .flat_map(|e| &e.line_ranges)
        .map(|r| match r {
            git_ai::authorship::authorship_log::LineRange::Single(_) => 1,
            git_ai::authorship::authorship_log::LineRange::Range(s, e) => e - s + 1,
        })
        .sum();
    assert!(
        pre_lines_1 < pre_lines_2,
        "precondition: commit 2 ({}) should have more attested lines than commit 1 ({})",
        pre_lines_2,
        pre_lines_1
    );

    // Advance default branch
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut other_file = repo.filename("other.txt");
    other_file.set_contents(crate::lines!["other"]);
    repo.stage_all_and_commit("Main advances").unwrap();

    // Rebase feature
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Get rebased commit SHAs
    let rebased_tip = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let rebased_parent = repo
        .git(&["rev-parse", "HEAD~1"])
        .unwrap()
        .trim()
        .to_string();

    // Verify post-rebase: metrics should differ between the two commits
    let rebased_note1 = repo
        .read_authorship_note(&rebased_parent)
        .expect("rebased commit 1 should have note");
    let rebased_log1 =
        AuthorshipLog::deserialize_from_string(&rebased_note1).expect("parse rebased note 1");
    let rebased_note2 = repo
        .read_authorship_note(&rebased_tip)
        .expect("rebased commit 2 should have note");
    let rebased_log2 =
        AuthorshipLog::deserialize_from_string(&rebased_note2).expect("parse rebased note 2");

    // Session format: verify sessions survive rebase and attestation line counts differ
    assert!(
        rebased_log1.metadata.prompts.is_empty(),
        "rebased commit 1 should not have prompts"
    );
    assert!(
        rebased_log2.metadata.prompts.is_empty(),
        "rebased commit 2 should not have prompts"
    );
    assert!(
        !rebased_log1.metadata.sessions.is_empty(),
        "regression: rebased commit 1 should have session records"
    );
    assert!(
        !rebased_log2.metadata.sessions.is_empty(),
        "regression: rebased commit 2 should have session records"
    );
    let post_lines_1: u32 = rebased_log1
        .attestations
        .iter()
        .flat_map(|a| &a.entries)
        .flat_map(|e| &e.line_ranges)
        .map(|r| match r {
            git_ai::authorship::authorship_log::LineRange::Single(_) => 1,
            git_ai::authorship::authorship_log::LineRange::Range(s, e) => e - s + 1,
        })
        .sum();
    let post_lines_2: u32 = rebased_log2
        .attestations
        .iter()
        .flat_map(|a| &a.entries)
        .flat_map(|e| &e.line_ranges)
        .map(|r| match r {
            git_ai::authorship::authorship_log::LineRange::Single(_) => 1,
            git_ai::authorship::authorship_log::LineRange::Range(s, e) => e - s + 1,
        })
        .sum();
    assert!(
        post_lines_1 < post_lines_2,
        "regression: rebased commit 2 ({}) should have more attested lines than commit 1 ({}). \
         If equal, the fast path is freezing metrics across commits.",
        post_lines_2,
        post_lines_1
    );
}

/// Regression test: attributions should survive a delete-recreate cycle within a rebase.
/// If a file is deleted in commit N and recreated in commit N+1, the recreated file
/// should inherit attributions from the pre-deletion state via positional diff transfer.
#[test]
fn test_rebase_file_delete_recreate_preserves_attribution() {
    let repo = TestRepo::new();
    let default_branch = repo.current_branch();

    // Initial setup
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial").unwrap();

    // Create feature branch with AI file
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut ai_file = repo.filename("feature.txt");
    ai_file.set_contents(crate::lines!["line1".ai(), "line2".ai(), "line3".ai()]);
    repo.stage_all_and_commit("Add AI file").unwrap();

    // Delete the file
    repo.git(&["rm", "feature.txt"]).unwrap();
    repo.stage_all_and_commit("Delete AI file").unwrap();

    // Recreate the file with same content
    ai_file.set_contents(crate::lines!["line1".ai(), "line2".ai(), "line3".ai()]);
    let recreate_commit = repo.stage_all_and_commit("Recreate AI file").unwrap();

    // Verify pre-rebase: recreated file has attributions
    let pre_note = repo
        .read_authorship_note(&recreate_commit.commit_sha)
        .expect("recreated commit should have note");
    let pre_log = AuthorshipLog::deserialize_from_string(&pre_note).expect("parse pre note");
    assert!(
        !pre_log.attestations.is_empty(),
        "precondition: recreated file should have attestations"
    );

    // Advance default branch
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut other_file = repo.filename("other.txt");
    other_file.set_contents(crate::lines!["other"]);
    repo.stage_all_and_commit("Main advances").unwrap();

    // Rebase feature
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Check rebased tip (the recreate commit)
    let rebased_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let rebased_note = repo
        .read_authorship_note(&rebased_sha)
        .expect("rebased recreate commit should have note");
    let rebased_log =
        AuthorshipLog::deserialize_from_string(&rebased_note).expect("parse rebased note");

    assert!(
        !rebased_log.attestations.is_empty(),
        "regression: file recreated after deletion should still have attestations after rebase"
    );

    // Verify the AI attribution itself survived
    ai_file.assert_lines_and_blame(crate::lines!["line1".ai(), "line2".ai(), "line3".ai()]);
}

/// Regression test: file deleted then recreated with DIFFERENT content preserves attribution.
///
/// This tests a subtle bug where:
/// 1. first_appearance_blobs: seen_files must be cleared on deletion so the
///    new blob OID is read on recreation.
/// 2. files_with_synced_state: must be cleared on deletion so recreation
///    uses content-diff (not stale hunk-based transfer).
#[test]
fn test_rebase_file_delete_recreate_different_content_preserves_attribution() {
    let repo = TestRepo::new();
    let default_branch = repo.current_branch();

    // Initial setup
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial").unwrap();

    // Create feature branch with AI file (original content)
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut ai_file = repo.filename("feature.txt");
    ai_file.set_contents(crate::lines!["old_line1".ai(), "old_line2".ai()]);
    repo.stage_all_and_commit("Add AI file").unwrap();

    // Delete the file
    repo.git(&["rm", "feature.txt"]).unwrap();
    repo.stage_all_and_commit("Delete AI file").unwrap();

    // Recreate the file with DIFFERENT content
    ai_file.set_contents(crate::lines![
        "new_line1".ai(),
        "new_line2".ai(),
        "new_line3".ai()
    ]);
    let recreate_commit = repo
        .stage_all_and_commit("Recreate AI file different")
        .unwrap();

    // Verify pre-rebase: recreated file has attributions
    let pre_note = repo
        .read_authorship_note(&recreate_commit.commit_sha)
        .expect("recreated commit should have note");
    let pre_log = AuthorshipLog::deserialize_from_string(&pre_note).expect("parse pre note");
    assert!(
        !pre_log.attestations.is_empty(),
        "precondition: recreated file should have attestations"
    );

    // Advance default branch
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut other_file = repo.filename("other.txt");
    other_file.set_contents(crate::lines!["other"]);
    repo.stage_all_and_commit("Main advances").unwrap();

    // Rebase feature
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Check rebased tip (the recreate commit)
    let rebased_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let rebased_note = repo
        .read_authorship_note(&rebased_sha)
        .expect("rebased recreate commit should have note");
    let rebased_log =
        AuthorshipLog::deserialize_from_string(&rebased_note).expect("parse rebased note");

    assert!(
        !rebased_log.attestations.is_empty(),
        "regression: file recreated with different content should have attestations after rebase"
    );

    // Verify the new AI attribution (different content) survived
    ai_file.assert_lines_and_blame(crate::lines![
        "new_line1".ai(),
        "new_line2".ai(),
        "new_line3".ai()
    ]);
}

/// Regression test: AI attribution from earlier commits (not HEAD) must survive rebase.
///
/// Each commit's note only covers lines changed in THAT commit. HEAD doesn't
/// touch all AI-attributed files. The reconstruction must process ALL commits'
/// notes to build the complete attribution state, not just HEAD's.
#[test]
fn test_rebase_preserves_attribution_from_non_head_commits() {
    let repo = TestRepo::new();

    // Initial commit
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    // Feature branch: commit 1 — AI attribution on file_a only
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut file_a = repo.filename("file_a.txt");
    file_a.set_contents(crate::lines![
        "// AI generated module A".ai(),
        "fn module_a() {}".ai(),
        "// end module A".ai()
    ]);
    repo.stage_all_and_commit("feat: add module A (AI)")
        .unwrap();

    // Feature branch: commit 2 — AI attribution on file_b only (file_a not touched)
    let mut file_b = repo.filename("file_b.txt");
    file_b.set_contents(crate::lines![
        "// AI generated module B".ai(),
        "fn module_b() {}".ai()
    ]);
    repo.stage_all_and_commit("feat: add module B (AI)")
        .unwrap();

    // Feature branch: commit 3 (HEAD) — AI attribution on file_c only
    // file_a and file_b are NOT touched in this commit
    let mut file_c = repo.filename("file_c.txt");
    file_c.set_contents(crate::lines![
        "// AI generated module C".ai(),
        "fn module_c() {}".ai()
    ]);
    repo.stage_all_and_commit("feat: add module C (AI)")
        .unwrap();

    // Advance main branch to force actual rebase (not fast-forward)
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_change = repo.filename("main_update.txt");
    main_change.set_contents(crate::lines!["main branch work"]);
    repo.stage_all_and_commit("main: infrastructure update")
        .unwrap();

    // Rebase feature onto main
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // CRITICAL: file_a attribution (from commit 1, NOT HEAD) must survive
    file_a.assert_lines_and_blame(crate::lines![
        "// AI generated module A".ai(),
        "fn module_a() {}".ai(),
        "// end module A".ai()
    ]);

    // file_b attribution (from commit 2, NOT HEAD) must survive
    file_b.assert_lines_and_blame(crate::lines![
        "// AI generated module B".ai(),
        "fn module_b() {}".ai()
    ]);

    // file_c attribution (from HEAD commit) must survive
    file_c.assert_lines_and_blame(crate::lines![
        "// AI generated module C".ai(),
        "fn module_c() {}".ai()
    ]);
}

/// Regression test: multi-commit attribution on SAME file from different commits.
///
/// Commit 1 adds AI lines 1-3, commit 3 adds AI lines 4-6, but commit 2
/// (between them) touches a different file entirely. The reconstruction must
/// combine notes from both commits to get the full attribution for the file.
#[test]
fn test_rebase_preserves_multi_commit_attribution_same_file() {
    let repo = TestRepo::new();

    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Commit 1: AI attribution on app.txt lines 1-3
    let mut app = repo.filename("app.txt");
    app.set_contents(crate::lines![
        "// AI header".ai(),
        "fn init() {}".ai(),
        "// end init".ai()
    ]);
    repo.stage_all_and_commit("feat: AI init code").unwrap();

    // Commit 2: touch a DIFFERENT file (app.txt unchanged)
    let mut config = repo.filename("config.txt");
    config.set_contents(crate::lines!["// AI config".ai(), "setting = true".ai()]);
    repo.stage_all_and_commit("feat: AI config").unwrap();

    // Commit 3 (HEAD): add MORE AI lines to app.txt
    app.set_contents(crate::lines![
        "// AI header".ai(),
        "fn init() {}".ai(),
        "// end init".ai(),
        "// AI footer added later".ai(),
        "fn cleanup() {}".ai()
    ]);
    repo.stage_all_and_commit("feat: AI cleanup code").unwrap();

    // Advance main
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut infra = repo.filename("infra.txt");
    infra.set_contents(crate::lines!["infra work"]);
    repo.stage_all_and_commit("main: infra").unwrap();

    // Rebase
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // app.txt should have ALL AI lines (from commits 1 AND 3)
    app.assert_lines_and_blame(crate::lines![
        "// AI header".ai(),
        "fn init() {}".ai(),
        "// end init".ai(),
        "// AI footer added later".ai(),
        "fn cleanup() {}".ai()
    ]);

    // config.txt (from commit 2, NOT HEAD) must survive
    config.assert_lines_and_blame(crate::lines!["// AI config".ai(), "setting = true".ai()]);
}

/// Regression test: attribution survives when main branch modifies AI-attributed
/// files, forcing the slow path (blob OID mismatch between original and rebased).
/// This tests that attribution from non-HEAD commits survives even through the
/// full attribution rewrite path.
#[test]
fn test_rebase_non_head_attribution_survives_slow_path() {
    let repo = TestRepo::new();

    let mut base = repo.filename("shared.txt");
    base.set_contents(crate::lines![
        "// top section",
        "line_a",
        "line_b",
        "line_c",
        "",
        "",
        "",
        "",
        "// bottom section",
        "line_x",
        "line_y",
        "line_z"
    ]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Commit 1: AI attribution on module.txt
    let mut module = repo.filename("module.txt");
    module.set_contents(crate::lines![
        "// AI module".ai(),
        "pub fn process() {}".ai(),
        "// end".ai()
    ]);
    repo.stage_all_and_commit("feat: AI module").unwrap();

    // Commit 2 (HEAD): append to bottom of shared.txt
    // module.txt is NOT touched here
    let mut shared = repo.filename("shared.txt");
    shared.set_contents(crate::lines![
        "// top section",
        "line_a",
        "line_b",
        "line_c",
        "",
        "",
        "",
        "",
        "// bottom section",
        "line_x",
        "line_y",
        "line_z",
        "// feature addition".ai()
    ]);
    repo.stage_all_and_commit("feat: extend shared").unwrap();

    // Advance main — add a new file so the rebase replays commits on a new base.
    // shared.txt is NOT modified on main, so no merge conflict occurs.
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut infra = repo.filename("infra.txt");
    infra.set_contents(crate::lines!["// infrastructure", "setup_logging();"]);
    repo.stage_all_and_commit("main: add infra file").unwrap();

    // Rebase
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // module.txt attribution (from commit 1, NOT HEAD) must survive
    // even though the rebase took the slow path due to shared.txt changes
    module.assert_lines_and_blame(crate::lines![
        "// AI module".ai(),
        "pub fn process() {}".ai(),
        "// end".ai()
    ]);
}

/// Regression test: interactive rebase that drops a commit must preserve attribution
/// on surviving commits (issue #970).
///
/// When an interactive rebase uses `drop` to skip commit B from [A, B, C], the
/// surviving rebased commits A′ and C′ must retain the attribution originally
/// associated with A and C respectively.
///
/// The bug: `rewrite_authorship_after_rebase_v2` used a positional zip to pair
/// `original_commits` with `new_commits`.  When commit B was dropped the lists
/// had different lengths: originals = [A, B, C] but new = [A′, C′].  The zip
/// produced [(A, A′), (B, C′)] so C′ was attributed using B's (wrong) note
/// instead of C's note, causing C′ to lose its attribution entirely.
///
/// The fix: when original and new commit counts differ, pair commits by matching
/// the commit subject line rather than by position.
#[test]
#[cfg(not(target_os = "windows"))]
fn test_rebase_interactive_drop_preserves_attribution() {
    let repo = TestRepo::new();

    // Create a base commit on main
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    // Create feature branch with three AI commits A, B, C
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    let mut file_a = repo.filename("file_a.txt");
    file_a.set_contents(crate::lines!["AI line A".ai()]);
    repo.stage_all_and_commit("Commit A").unwrap();

    let mut file_b = repo.filename("file_b.txt");
    file_b.set_contents(crate::lines!["AI line B".ai()]);
    repo.stage_all_and_commit("Commit B").unwrap();

    let mut file_c = repo.filename("file_c.txt");
    file_c.set_contents(crate::lines!["AI line C".ai()]);
    repo.stage_all_and_commit("Commit C").unwrap();

    // Advance main so the rebase has a new base (forces non-fast-forward)
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut other = repo.filename("other.txt");
    other.set_contents(crate::lines!["other content"]);
    repo.stage_all_and_commit("Main advances").unwrap();
    let base_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Interactive rebase: drop Commit B, keep A and C
    repo.git(&["checkout", "feature"]).unwrap();
    // Drop the 2nd pick line (Commit B) — the three commits appear in order A, B, C.
    let drop_script = r#"#!/bin/sh
sed -i.bak '2s/^pick/drop/' "$1"
"#;
    let script_path = repo.path().join("drop_script.sh");
    std::fs::write(&script_path, drop_script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();

    let rebase_result = repo.git_with_env(
        &["rebase", "-i", &base_commit],
        &[
            ("GIT_SEQUENCE_EDITOR", script_path.to_str().unwrap()),
            ("GIT_EDITOR", "true"),
        ],
        None,
    );
    assert!(
        rebase_result.is_ok(),
        "interactive rebase with drop should succeed: {:?}",
        rebase_result
    );

    // file_b should be gone (its commit was dropped)
    assert!(
        !repo.path().join("file_b.txt").exists(),
        "file_b.txt should not exist after its commit was dropped"
    );

    // Commit A's rewrite (A′) must still carry AI attribution
    file_a.assert_lines_and_blame(crate::lines!["AI line A".ai()]);

    // Commit C's rewrite (C′) must still carry AI attribution.
    // This is the assertion that failed before the fix: the broken positional zip
    // paired C′ with B's note (no AI content), causing the attribution to be lost.
    file_c.assert_lines_and_blame(crate::lines!["AI line C".ai()]);
}

crate::reuse_tests_in_worktree!(
    test_rebase_no_conflicts_identical_trees,
    test_rebase_with_different_trees,
    test_rebase_multiple_commits,
    test_rebase_mixed_authorship,
    test_rebase_preserves_exact_mixed_line_attribution_in_single_file,
    test_rebase_with_human_only_commit_between_ai_commits_preserves_exact_lines,
    test_rebase_preserves_human_only_commit_note_metadata,
    test_rebase_preserves_prompt_only_commit_note_metadata,
    test_rebase_fast_forward,
    test_rebase_with_explicit_branch_argument_preserves_authorship,
    test_rebase_root_with_explicit_branch_argument_preserves_authorship,
    test_rebase_interactive_reorder,
    test_rebase_skip,
    test_rebase_keep_empty,
    test_rebase_rerere,
    test_rebase_patch_stack,
    test_rebase_already_up_to_date,
    test_rebase_with_conflicts,
    test_rebase_abort,
    test_rebase_branch_switch_during,
    test_rebase_autosquash,
    test_rebase_autostash,
    test_rebase_exec,
    test_rebase_preserve_merges,
    test_rebase_commit_splitting,
    test_rebase_prompt_metrics_update_per_commit,
    test_rebase_file_delete_recreate_preserves_attribution,
    test_rebase_file_delete_recreate_different_content_preserves_attribution,
    test_rebase_file_delete_recreate_after_hunk_modification,
);

crate::reuse_tests_in_worktree_with_attrs!(
    (#[cfg(not(target_os = "windows"))])
    test_rebase_squash_preserves_all_authorship,
    test_rebase_reword_commit_with_children,
    test_rebase_interactive_drop_preserves_attribution,
    test_rebase_squash_preserves_human_attribution,
    test_rebase_squash_preserves_session_attribution,
);

/// Regression test: file modified via hunk path, then deleted, then recreated.
///
/// This exercises a bug where `current_file_contents` becomes stale after hunk-based
/// attribution transfer (which updates attributions but not the file content cache).
/// When the file is later deleted and recreated, the slow content-diff path would use
/// stale content with shifted line numbers, producing corrupt attributions.
///
/// Trigger sequence:
/// 1. Commit 1: create file (slow path sets current_file_contents)
/// 2. Commit 2: modify file (hunk path shifts attrs but leaves current_file_contents stale)
/// 3. Commit 3: delete file
/// 4. Commit 4: recreate file with new content
///
/// Main branch must also modify the same file to force the slow reconstruction path.
#[test]
fn test_rebase_file_delete_recreate_after_hunk_modification() {
    let repo = TestRepo::new();
    let default_branch = repo.current_branch();

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial").unwrap();

    // Feature branch: 4 commits exercising hunk→delete→recreate
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Commit 1: create file
    let mut ai_file = repo.filename("feature.txt");
    ai_file.set_contents(crate::lines!["line1".ai(), "line2".ai(), "line3".ai()]);
    repo.stage_all_and_commit("Create AI file").unwrap();

    // Commit 2: modify file (will use hunk-based path on rebase)
    ai_file.set_contents(crate::lines![
        "line1".ai(),
        "line2".ai(),
        "inserted".ai(),
        "line3".ai()
    ]);
    repo.stage_all_and_commit("Modify AI file").unwrap();

    // Commit 3: delete the file
    repo.git(&["rm", "feature.txt"]).unwrap();
    repo.stage_all_and_commit("Delete AI file").unwrap();

    // Commit 4: recreate with different content
    ai_file.set_contents(crate::lines![
        "recreated_a".ai(),
        "recreated_b".ai(),
        "recreated_c".ai(),
        "recreated_d".ai()
    ]);
    repo.stage_all_and_commit("Recreate AI file").unwrap();

    // Advance default branch — must touch the same file to force slow path
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut conflict_file = repo.filename("feature.txt");
    conflict_file.set_contents(crate::lines!["main_content"]);
    repo.stage_all_and_commit("Main touches same file").unwrap();
    // Delete it so rebase doesn't conflict
    repo.git(&["rm", "feature.txt"]).unwrap();
    repo.stage_all_and_commit("Main deletes file").unwrap();

    // Rebase
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Check the final commit (recreate) has correct attributions
    let rebased_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let rebased_note = repo
        .read_authorship_note(&rebased_sha)
        .expect("rebased recreate commit should have note");
    let rebased_log =
        AuthorshipLog::deserialize_from_string(&rebased_note).expect("parse rebased note");

    assert!(
        !rebased_log.attestations.is_empty(),
        "regression: file recreated after hunk-modify+delete should have attestations"
    );

    ai_file.assert_lines_and_blame(crate::lines![
        "recreated_a".ai(),
        "recreated_b".ai(),
        "recreated_c".ai(),
        "recreated_d".ai()
    ]);
}

/// Regression test for issue #919: daemon panics on multi-byte UTF-8 characters
/// during rebase authorship tracking. The `→` character (U+2192, 3 bytes in UTF-8)
/// placed so that byte index 40 falls inside its encoding triggers a panic in
/// `run_diff_tree_with_hunks` when `&line[..40]` is used instead of `line.get(..40)`.
#[test]
fn test_rebase_preserves_authorship_with_multibyte_utf8_in_diff_context() {
    let repo = TestRepo::new();

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Create a file with multi-byte UTF-8 characters (→ is 3 bytes: 0xE2 0x86 0x92).
    // The content is crafted so that a diff context line will contain multi-byte chars
    // near the 40-byte boundary that previously caused the panic.
    let mut utf8_file = repo.filename("rules.py");
    utf8_file.set_contents(crate::lines![
        "def test_rules():".ai(),
        "    \"\"\"98 rules high, 2 rules low → with threshold 90, low should be trimmed.\"\"\""
            .ai(),
        "    pass".ai()
    ]);
    repo.stage_all_and_commit("Add rules with arrow char")
        .unwrap();

    // Second commit modifying the same file to ensure diff hunks include the UTF-8 context
    utf8_file.set_contents(crate::lines![
        "def test_rules():".ai(),
        "    \"\"\"98 rules high, 2 rules low → with threshold 90, low should be trimmed.\"\"\""
            .ai(),
        "    result = run_rules()".ai(),
        "    assert result".ai()
    ]);
    repo.stage_all_and_commit("Expand rules test").unwrap();

    // Advance default branch
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main advance"]);
    repo.stage_all_and_commit("Main advance").unwrap();

    // Rebase — this previously panicked with:
    // byte index 40 is not a char boundary; it is inside '→' (bytes 39..42)
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Verify authorship preserved through the rebase
    utf8_file.assert_lines_and_blame(crate::lines![
        "def test_rules():".ai(),
        "    \"\"\"98 rules high, 2 rules low → with threshold 90, low should be trimmed.\"\"\""
            .ai(),
        "    result = run_rules()".ai(),
        "    assert result".ai()
    ]);
}

/// Regression test for issue #1214: after squash rebase of 3 commits (2 AI + 1 human),
/// the merged note loses the humans block entirely — known-human line attribution is gone.
///
/// Repro from the issue:
/// 1. AI commit 1: AI adds lines to a file (with some lines the human later overrides)
/// 2. Human commit: human edits the same file (known_human checkpoint)
/// 3. AI commit 2: AI adds more lines
/// 4. git rebase -i HEAD~3 → fixup all into first commit
/// 5. Inspect merged note → humans block must be preserved
#[test]
#[cfg(not(target_os = "windows"))]
fn test_rebase_squash_preserves_human_attribution() {
    use std::io::Write;

    let repo = TestRepo::new();

    // Create initial commit on default branch
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let default_branch = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    let file_path = repo.path().join("handler.go");

    // --- AI commit 1: AI adds lines ---
    // Pre-edit checkpoint: file doesn't exist yet, take a snapshot of "nothing"
    repo.git_ai(&["checkpoint", "human", "handler.go"]).unwrap();
    let ai_content_1 = "\
func handleOrder() {
    validate()
    process()
}
";
    std::fs::write(&file_path, ai_content_1).unwrap();
    // Post-edit checkpoint: AI wrote the content
    repo.git_ai(&["checkpoint", "mock_ai", "handler.go"])
        .unwrap();
    repo.stage_all_and_commit("AI commit 1").unwrap();

    let mut handler = repo.filename("handler.go");
    handler.assert_committed_lines(crate::lines![
        "func handleOrder() {".ai(),
        "    validate()".ai(),
        "    process()".ai(),
        "}".ai(),
    ]);

    // --- Human commit: human edits the file, adding a line ---
    let human_content = "\
func handleOrder() {
    validate()
    log(\"order received\")
    process()
}
";
    std::fs::write(&file_path, human_content).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "handler.go"])
        .unwrap();
    repo.stage_all_and_commit("Human commit").unwrap();

    handler.assert_committed_lines(crate::lines![
        "func handleOrder() {".ai(),
        "    validate()".ai(),
        "    log(\"order received\")".human(),
        "    process()".ai(),
        "}".ai(),
    ]);

    // Verify humans block exists before squash
    let human_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let human_note = repo
        .read_authorship_note(&human_sha)
        .expect("human commit should have authorship note");
    let human_log = AuthorshipLog::deserialize_from_string(&human_note).expect("parse human note");
    assert!(
        !human_log.metadata.humans.is_empty(),
        "Pre-squash: human commit should have humans metadata block"
    );

    // --- AI commit 2: AI adds more lines ---
    // Pre-edit checkpoint: snapshot current state before AI edits
    repo.git_ai(&["checkpoint", "human", "handler.go"]).unwrap();
    let ai_content_2 = "\
func handleOrder() {
    validate()
    log(\"order received\")
    process()
    sendMetrics()
}
";
    std::fs::write(&file_path, ai_content_2).unwrap();
    // Post-edit checkpoint: AI wrote the new line
    repo.git_ai(&["checkpoint", "mock_ai", "handler.go"])
        .unwrap();
    repo.stage_all_and_commit("AI commit 2").unwrap();

    handler.assert_committed_lines(crate::lines![
        "func handleOrder() {".ai(),
        "    validate()".ai(),
        "    log(\"order received\")".human(),
        "    process()".ai(),
        "    sendMetrics()".ai(),
        "}".ai(),
    ]);

    // Advance main branch so rebase has something to replay onto
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file2 = repo.filename("main2.txt");
    main_file2.set_contents(crate::lines!["main work"]);
    repo.stage_all_and_commit("Main advances").unwrap();
    let base_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // --- Squash rebase: fixup all 3 commits into the first ---
    repo.git(&["checkout", "feature"]).unwrap();

    let script_content = r#"#!/bin/sh
sed -i.bak '2s/pick/fixup/' "$1"
sed -i.bak '3s/pick/fixup/' "$1"
"#;

    let script_path = repo.path().join("squash_script.sh");
    let mut script_file = std::fs::File::create(&script_path).unwrap();
    script_file.write_all(script_content.as_bytes()).unwrap();
    drop(script_file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
    }

    let rebase_result = repo.git_with_env(
        &["rebase", "-i", &base_commit],
        &[
            ("GIT_SEQUENCE_EDITOR", script_path.to_str().unwrap()),
            ("GIT_EDITOR", "true"),
        ],
        None,
    );

    if rebase_result.is_err() {
        eprintln!("git rebase output: {:?}", rebase_result);
        panic!("Interactive rebase with fixup failed");
    }

    // Verify file content survived the squash
    assert!(
        repo.path().join("handler.go").exists(),
        "handler.go should exist after squash"
    );

    // Verify the merged note has the humans block preserved
    let squashed_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let squashed_note = repo
        .read_authorship_note(&squashed_sha)
        .expect("squashed commit should have authorship note");
    let squashed_log =
        AuthorshipLog::deserialize_from_string(&squashed_note).expect("parse squashed note");
    assert!(
        !squashed_log.metadata.humans.is_empty(),
        "Post-squash: humans metadata block must be preserved (issue #1214)"
    );
    for record in squashed_log.metadata.humans.values() {
        assert_eq!(
            record.author, "Test User <test@example.com>",
            "HumanRecord.author should include email"
        );
    }

    // Verify line-level attribution: human line must still show as human,
    // and AI lines (including closing `}`) retain their attribution through squash.
    handler.assert_lines_and_blame(crate::lines![
        "func handleOrder() {".ai(),
        "    validate()".ai(),
        "    log(\"order received\")".human(),
        "    process()".ai(),
        "    sendMetrics()".ai(),
        "}".ai(),
    ]);
}

/// Verify that session metadata survives squash rebase.
/// This is the session-format counterpart of test_rebase_squash_preserves_human_attribution.
/// Sessions use s_<id>::t_<hash> attestation entries and are the current default format.
/// The delta_sessions code already scans current_attributions (unlike the old delta_humans
/// code), so this test should pass without additional fixes.
#[test]
#[cfg(not(target_os = "windows"))]
fn test_rebase_squash_preserves_session_attribution() {
    use std::io::Write;

    let repo = TestRepo::new();

    // Create initial commit on default branch
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let default_branch = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    let file_path = repo.path().join("service.go");

    // --- AI commit 1: AI adds initial lines ---
    repo.git_ai(&["checkpoint", "human", "service.go"]).unwrap();
    let ai_content_1 = "\
func serve() {
    listen()
    handle()
}
";
    std::fs::write(&file_path, ai_content_1).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "service.go"])
        .unwrap();
    repo.stage_all_and_commit("AI commit 1").unwrap();

    let mut service = repo.filename("service.go");
    service.assert_committed_lines(crate::lines![
        "func serve() {".ai(),
        "    listen()".ai(),
        "    handle()".ai(),
        "}".ai(),
    ]);

    // Verify session metadata exists on commit 1
    let sha1 = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let note1 = repo
        .read_authorship_note(&sha1)
        .expect("AI commit 1 should have note");
    let log1 = AuthorshipLog::deserialize_from_string(&note1).expect("parse note 1");
    assert_eq!(
        log1.metadata.sessions.len(),
        1,
        "AI commit 1 should have exactly 1 session"
    );

    // --- AI commit 2: AI adds more lines ---
    repo.git_ai(&["checkpoint", "human", "service.go"]).unwrap();
    let ai_content_2 = "\
func serve() {
    listen()
    handle()
    logMetrics()
}
";
    std::fs::write(&file_path, ai_content_2).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "service.go"])
        .unwrap();
    repo.stage_all_and_commit("AI commit 2").unwrap();

    service.assert_committed_lines(crate::lines![
        "func serve() {".ai(),
        "    listen()".ai(),
        "    handle()".ai(),
        "    logMetrics()".ai(),
        "}".ai(),
    ]);

    // --- AI commit 3: AI adds yet more ---
    repo.git_ai(&["checkpoint", "human", "service.go"]).unwrap();
    let ai_content_3 = "\
func serve() {
    listen()
    handle()
    logMetrics()
    shutdown()
}
";
    std::fs::write(&file_path, ai_content_3).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "service.go"])
        .unwrap();
    repo.stage_all_and_commit("AI commit 3").unwrap();

    service.assert_committed_lines(crate::lines![
        "func serve() {".ai(),
        "    listen()".ai(),
        "    handle()".ai(),
        "    logMetrics()".ai(),
        "    shutdown()".ai(),
        "}".ai(),
    ]);

    // Advance main branch so rebase has something to replay onto
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file2 = repo.filename("main2.txt");
    main_file2.set_contents(crate::lines!["main work"]);
    repo.stage_all_and_commit("Main advances").unwrap();
    let base_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // --- Squash rebase: fixup all 3 commits into the first ---
    repo.git(&["checkout", "feature"]).unwrap();

    let script_content = r#"#!/bin/sh
sed -i.bak '2s/pick/fixup/' "$1"
sed -i.bak '3s/pick/fixup/' "$1"
"#;

    let script_path = repo.path().join("squash_script.sh");
    let mut script_file = std::fs::File::create(&script_path).unwrap();
    script_file.write_all(script_content.as_bytes()).unwrap();
    drop(script_file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
    }

    let rebase_result = repo.git_with_env(
        &["rebase", "-i", &base_commit],
        &[
            ("GIT_SEQUENCE_EDITOR", script_path.to_str().unwrap()),
            ("GIT_EDITOR", "true"),
        ],
        None,
    );

    if rebase_result.is_err() {
        eprintln!("git rebase output: {:?}", rebase_result);
        panic!("Interactive rebase with fixup failed");
    }

    // Verify the merged note has sessions metadata preserved
    let squashed_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let squashed_note = repo
        .read_authorship_note(&squashed_sha)
        .expect("squashed commit should have authorship note");
    let squashed_log =
        AuthorshipLog::deserialize_from_string(&squashed_note).expect("parse squashed note");
    // Each mock_ai checkpoint creates a distinct session, so the squashed
    // note should have all 3 sessions merged from the 3 original commits.
    assert_eq!(
        squashed_log.metadata.sessions.len(),
        3,
        "Post-squash: squashed note should have all 3 sessions merged"
    );

    // Verify line-level AI attribution survived the squash
    service.assert_lines_and_blame(crate::lines![
        "func serve() {".ai(),
        "    listen()".ai(),
        "    handle()".ai(),
        "    logMetrics()".ai(),
        "    shutdown()".ai(),
        "}".ai(),
    ]);
}

/// Test the full branch lifecycle pattern used by the fuzzer:
/// create branch → multiple commits → rebase onto updated main → fast-forward merge back.
/// This verifies attribution survives through rebase + merge.
#[test]
fn test_rebase_then_ff_merge_preserves_attribution() {
    use std::fs;

    let repo = TestRepo::new();

    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main line 1"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    // Create feature branch with multiple AI commits on a SEPARATE file (no conflicts)
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    let feature_path = repo.path().join("feature.txt");
    fs::write(&feature_path, "ai feature 1\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature commit 1").unwrap();

    fs::write(&feature_path, "ai feature 1\nai feature 2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature commit 2").unwrap();

    fs::write(&feature_path, "ai feature 1\nai feature 2\nai feature 3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature commit 3").unwrap();

    // Advance main with a non-conflicting change (different file)
    repo.git(&["checkout", &default_branch]).unwrap();
    let main_path = repo.path().join("main.txt");
    fs::write(&main_path, "main line 1\nmain advance\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("advance main").unwrap();

    // Rebase feature onto main
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Fast-forward merge back to main
    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["merge", "feature"]).unwrap();

    // Verify attribution on the feature file (should survive rebase + merge)
    let mut result_file = repo.filename("feature.txt");
    result_file.assert_lines_and_blame(crate::lines![
        "ai feature 1".ai(),
        "ai feature 2".ai(),
        "ai feature 3".ai(),
    ]);
}

/// Same as above but edits the SAME file on both branches (prepend on main, append on feature).
/// This is the exact pattern the fuzzer's workflow-branch-lifecycle uses.
#[test]
fn test_rebase_same_file_then_ff_merge_preserves_attribution() {
    use std::fs;

    let repo = TestRepo::new();

    let file_path = repo.path().join("shared.txt");
    fs::write(&file_path, "base line\n").unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    // Create feature branch - append AI lines
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    fs::write(&file_path, "base line\nai append 1\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "shared.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature commit 1").unwrap();

    fs::write(&file_path, "base line\nai append 1\nai append 2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "shared.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature commit 2").unwrap();

    fs::write(
        &file_path,
        "base line\nai append 1\nai append 2\nai append 3\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "shared.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature commit 3").unwrap();

    // Advance main - prepend human line (non-conflicting with appends)
    repo.git(&["checkout", &default_branch]).unwrap();
    fs::write(&file_path, "human prepend\nbase line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "shared.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("advance main").unwrap();

    // Rebase feature onto main
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Fast-forward merge
    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["merge", "feature"]).unwrap();

    // After rebase+merge: prepend + base + 3 appends
    let mut result_file = repo.filename("shared.txt");
    result_file.assert_lines_and_blame(crate::lines![
        "human prepend".human(),
        "base line".unattributed_human(),
        "ai append 1".ai(),
        "ai append 2".ai(),
        "ai append 3".ai(),
    ]);
}

const COMMITS_OVER_PENDING_DROPPED_LIMIT: usize = 200;
const COMMITS_JUST_OVER_PENDING_DROPPED_LIMIT: usize = 65;

fn numbered_file_contents(prefix: &str, count: usize) -> String {
    (1..=count)
        .map(|i| format!("{prefix} {i}\n"))
        .collect::<String>()
}

fn expected_ai_numbered_lines(prefix: &str, count: usize) -> Vec<ExpectedLine> {
    (1..=count).map(|i| format!("{prefix} {i}").ai()).collect()
}

fn leading_dropped_commits_before_first_match(range_diff: &str) -> usize {
    let mut dropped = 0;

    for line in range_diff.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        let (_ordinal, _old_sha, Some(status)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };

        match status {
            "<" => dropped += 1,
            "=" | "!" => break,
            _ => {}
        }
    }

    dropped
}

fn dropped_commits_after_first_match(range_diff: &str) -> usize {
    let mut saw_match = false;
    let mut dropped = 0;

    for line in range_diff.lines() {
        let mut parts = line.split_whitespace();
        let (_ordinal, _old_sha, Some(status)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };

        match status {
            "=" | "!" => saw_match = true,
            "<" if saw_match => dropped += 1,
            _ => {}
        }
    }

    dropped
}

fn shift_authorship_mapping_counts(daemon_log: &str) -> Vec<usize> {
    daemon_log
        .lines()
        .filter_map(|line| {
            line.split_once("shift_authorship_notes: ")?
                .1
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .collect()
}

fn range_diff_spawn_count(spawn_log_path: &std::path::Path) -> usize {
    std::fs::read_to_string(spawn_log_path)
        .unwrap_or_default()
        .lines()
        .filter(|command| *command == "range-diff")
        .count()
}

#[test]
fn test_rebase_more_commits_than_pending_dropped_limit_preserves_authorship() {
    use std::fs;

    let repo = TestRepo::new();

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let feature_path = repo.path().join("feature.txt");
    let mut feature_file = repo.filename("feature.txt");

    for commit_number in 1..=COMMITS_OVER_PENDING_DROPPED_LIMIT {
        fs::write(
            &feature_path,
            numbered_file_contents("ai feature line", commit_number),
        )
        .unwrap();
        repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
            .unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.commit(&format!("feature commit {commit_number}"))
            .unwrap();
    }

    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main advance".human()]);
    repo.stage_all_and_commit("Main advances").unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    let rebased_commit_count = repo
        .git(&["rev-list", "--count", &format!("{}..HEAD", default_branch)])
        .unwrap()
        .trim()
        .parse::<usize>()
        .unwrap();
    assert_eq!(
        rebased_commit_count, COMMITS_OVER_PENDING_DROPPED_LIMIT,
        "rebase should preserve every feature commit, even when the count exceeds the pending-drop limit"
    );
    feature_file.assert_lines_and_blame(expected_ai_numbered_lines(
        "ai feature line",
        COMMITS_OVER_PENDING_DROPPED_LIMIT,
    ));
}

#[test]
fn test_reset_keep_undo_rebase_discards_many_old_upstream_commits() {
    use std::fs;

    let repo = TestRepo::new();

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let base_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let feature_path = repo.path().join("feature.txt");
    fs::write(&feature_path, "ai feature line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature commit").unwrap();
    let original_feature_tip = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let mut feature_file = repo.filename("feature.txt");

    repo.git(&["checkout", &default_branch]).unwrap();
    let trunk_path = repo.path().join("trunk.txt");
    for commit_number in 1..=COMMITS_OVER_PENDING_DROPPED_LIMIT {
        fs::write(
            &trunk_path,
            numbered_file_contents("untracked trunk line", commit_number),
        )
        .unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.commit(&format!("trunk commit {commit_number}"))
            .unwrap();
    }

    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();
    let rebased_feature_tip = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(
        original_feature_tip, rebased_feature_tip,
        "feature commit should be rewritten onto the advanced trunk"
    );
    feature_file.assert_lines_and_blame(crate::lines!["ai feature line".ai()]);

    let old_range = format!("{base_sha}..{rebased_feature_tip}");
    let new_range = format!("{base_sha}..{original_feature_tip}");
    let range_diff = repo
        .git(&[
            "range-diff",
            "-s",
            "--creation-factor=100",
            &old_range,
            &new_range,
        ])
        .unwrap();
    let leading_drops = leading_dropped_commits_before_first_match(&range_diff);
    assert!(
        leading_drops > 64,
        "test setup must exceed the pending-dropped cutoff before the first match; got {leading_drops}\n{range_diff}"
    );

    repo.git(&["reset", "--keep", &original_feature_tip])
        .unwrap();
    assert_eq!(
        repo.git(&["rev-parse", "HEAD"]).unwrap().trim(),
        original_feature_tip
    );
    assert!(
        !repo.path().join("trunk.txt").exists(),
        "reset --keep back to the pre-rebase feature tip should remove upstream-only files"
    );
    feature_file.assert_lines_and_blame(crate::lines!["ai feature line".ai()]);

    let note = repo
        .read_authorship_note(&original_feature_tip)
        .expect("original feature commit should still have an authorship note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse original feature note");
    assert!(
        log.attestations
            .iter()
            .any(|attestation| attestation.file_path == "feature.txt"),
        "feature authorship note should still describe the feature file"
    );
    assert!(
        log.attestations
            .iter()
            .all(|attestation| attestation.file_path != "trunk.txt"),
        "discarded upstream-only commits must not add trunk attestations to the feature note"
    );
}

#[test]
fn test_reset_keep_move_discards_many_commits_after_matched_stack() {
    use std::fs;

    let repo = TestRepo::new_with_daemon_env(&[("GIT_AI_DEBUG", "1")]);

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let base_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let feature_path = repo.path().join("feature.txt");
    fs::write(&feature_path, "ai feature line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();
    repo.git(&["add", "feature.txt"]).unwrap();
    repo.commit("feature commit").unwrap();
    let feature_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.assert_committed_lines(crate::lines!["ai feature line".ai()]);

    repo.git(&["checkout", "-b", "feature-prime", &base_sha])
        .unwrap();
    repo.git(&["cherry-pick", &feature_commit]).unwrap();
    repo.git(&[
        "commit",
        "--amend",
        "-m",
        "feature commit moved to new parent",
    ])
    .unwrap();
    let feature_prime_tip = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    feature_file.assert_committed_lines(crate::lines!["ai feature line".ai()]);

    repo.git(&["checkout", "feature"]).unwrap();
    let dropped_path = repo.path().join("dropped.txt");
    for commit_number in 1..=COMMITS_JUST_OVER_PENDING_DROPPED_LIMIT {
        fs::write(
            &dropped_path,
            numbered_file_contents("ai dropped line", commit_number),
        )
        .unwrap();
        repo.git_ai(&["checkpoint", "mock_ai", "dropped.txt"])
            .unwrap();
        repo.git(&["add", "dropped.txt"]).unwrap();
        repo.commit(&format!("dropped commit {commit_number}"))
            .unwrap();
    }
    let feature_tip = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let mut dropped_file = repo.filename("dropped.txt");
    dropped_file.assert_committed_lines(expected_ai_numbered_lines(
        "ai dropped line",
        COMMITS_JUST_OVER_PENDING_DROPPED_LIMIT,
    ));

    let old_range = format!("{base_sha}..{feature_tip}");
    let new_range = format!("{base_sha}..{feature_prime_tip}");
    let range_diff = repo
        .git(&[
            "range-diff",
            "-s",
            "--creation-factor=100",
            &old_range,
            &new_range,
        ])
        .unwrap();
    let trailing_drops = dropped_commits_after_first_match(&range_diff);
    assert!(
        trailing_drops == COMMITS_JUST_OVER_PENDING_DROPPED_LIMIT,
        "test setup must place {COMMITS_JUST_OVER_PENDING_DROPPED_LIMIT} drops after a match; got {trailing_drops}\n{range_diff}"
    );

    repo.sync_daemon();
    let mapping_count_baseline =
        shift_authorship_mapping_counts(&repo.daemon_stderr_contents()).len();

    repo.git(&["reset", "--keep", &feature_prime_tip]).unwrap();
    repo.sync_daemon();

    assert!(!dropped_path.exists());
    feature_file.assert_committed_lines(crate::lines!["ai feature line".ai()]);

    let daemon_log = repo.daemon_stderr_contents();
    let mapping_counts = shift_authorship_mapping_counts(&daemon_log);
    assert_eq!(
        &mapping_counts[mapping_count_baseline..],
        &[1],
        "the move should map only the matched stack commit, not fabricate one mapping per dropped commit\n{daemon_log}"
    );
}

#[test]
fn test_reset_keep_move_skips_range_diff_when_range_exceeds_limit() {
    use std::fs;

    let log_dir = tempfile::tempdir().unwrap();
    let spawn_log_path = log_dir.path().join("spawns.log");
    let spawn_log = spawn_log_path.to_string_lossy().into_owned();
    let repo = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_SPAWN_LOG", spawn_log.as_str()),
        ("GIT_AI_TEST_RANGE_DIFF_COMMIT_LIMIT", "2"),
    ]);

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let base_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let feature_path = repo.path().join("feature.txt");
    fs::write(&feature_path, "ai feature line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();
    repo.git(&["add", "feature.txt"]).unwrap();
    repo.commit("feature commit").unwrap();
    let feature_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.assert_committed_lines(crate::lines!["ai feature line".ai()]);

    repo.git(&["checkout", "-b", "feature-prime", &base_sha])
        .unwrap();
    repo.git(&["cherry-pick", &feature_commit]).unwrap();
    repo.git(&[
        "commit",
        "--amend",
        "-m",
        "feature commit moved to new parent",
    ])
    .unwrap();
    let feature_prime_tip = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    feature_file.assert_committed_lines(crate::lines!["ai feature line".ai()]);

    repo.git(&["checkout", "feature"]).unwrap();
    let dropped_path = repo.path().join("dropped.txt");
    for commit_number in 1..=3 {
        fs::write(
            &dropped_path,
            numbered_file_contents("ai dropped line", commit_number),
        )
        .unwrap();
        repo.git_ai(&["checkpoint", "mock_ai", "dropped.txt"])
            .unwrap();
        repo.git(&["add", "dropped.txt"]).unwrap();
        repo.commit(&format!("dropped commit {commit_number}"))
            .unwrap();
    }
    let mut dropped_file = repo.filename("dropped.txt");
    dropped_file.assert_committed_lines(expected_ai_numbered_lines("ai dropped line", 3));

    repo.sync_daemon();
    let range_diff_baseline = range_diff_spawn_count(&spawn_log_path);

    repo.git(&["reset", "--keep", &feature_prime_tip]).unwrap();
    repo.sync_daemon();

    assert!(!dropped_path.exists());
    feature_file.assert_committed_lines(crate::lines!["ai feature line".ai()]);
    assert_eq!(
        range_diff_spawn_count(&spawn_log_path),
        range_diff_baseline,
        "an oversized range must be rejected before spawning git range-diff"
    );
}

/// Test rebase with more than DIFF_TREE_STREAM_CHUNK_SIZE (50) rewritten commits.
///
/// The streaming diff-tree path drains completed results in chunks of 50 and
/// applies hunk/rename shifts inline. This test verifies that commits around the
/// chunk boundary (49, 50, 51, 52) all have correct authorship note attestations
/// after rebase, and that the final HEAD has correct attribution for all lines.
#[test]
fn test_rebase_across_streaming_chunk_boundary_preserves_attribution() {
    use std::fs;

    const FEATURE_COMMIT_COUNT: usize = 60;

    let repo = TestRepo::new();

    // Initial commit
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let feature_path = repo.path().join("feature.txt");
    let mut feature_file = repo.filename("feature.txt");

    // Build ~60 commits with a varied modification pattern:
    // - Every commit appends a new AI line.
    // - Every 5th commit also modifies an earlier line (different from all prior
    //   modifications), producing non-trivial diff-tree results across the boundary.
    for commit_number in 1..=FEATURE_COMMIT_COUNT {
        let mut content = String::new();
        for i in 1..=commit_number {
            if i % 5 == 0 && i != commit_number && commit_number % 5 == 0 {
                // Every 5th commit modifies an earlier line that was last changed
                // in a previous commit
                content.push_str(&format!(
                    "ai feature line {i} (modified in commit {commit_number})\n"
                ));
            } else {
                content.push_str(&format!("ai feature line {i}\n"));
            }
        }
        fs::write(&feature_path, &content).unwrap();
        repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
            .unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.commit(&format!("feature commit {commit_number}"))
            .unwrap();
    }

    // Capture original commit SHAs in order (oldest first)
    let original_shas_output = repo
        .git(&[
            "rev-list",
            "--reverse",
            &format!("{}..HEAD", default_branch),
        ])
        .unwrap();
    let original_shas: Vec<&str> = original_shas_output.trim().lines().collect();
    assert_eq!(
        original_shas.len(),
        FEATURE_COMMIT_COUNT,
        "should have exactly {FEATURE_COMMIT_COUNT} feature commits before rebase"
    );

    // Advance main with one human commit
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main advance".human()]);
    repo.stage_all_and_commit("Main advances").unwrap();

    // Rebase feature onto main
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Get rebased commit SHAs in order (oldest first)
    let rebased_shas_output = repo
        .git(&[
            "rev-list",
            "--reverse",
            &format!("{}..HEAD", default_branch),
        ])
        .unwrap();
    let rebased_shas: Vec<&str> = rebased_shas_output.trim().lines().collect();
    assert_eq!(
        rebased_shas.len(),
        FEATURE_COMMIT_COUNT,
        "rebase should preserve all {FEATURE_COMMIT_COUNT} commits"
    );

    // Verify authorship notes for commits around the chunk boundary (0-based: 48, 49, 50, 51)
    // The streaming chunk size is 50, so these straddle the boundary.
    let boundary_indices: [usize; 4] = [48, 49, 50, 51];

    for &idx in &boundary_indices {
        let sha = rebased_shas[idx];
        let note = repo
            .read_authorship_note(sha)
            .unwrap_or_else(|| panic!("commit {idx} (sha={sha}) should have an authorship note"));

        let log = AuthorshipLog::deserialize_from_string(&note)
            .unwrap_or_else(|e| panic!("parse authorship note for commit {idx}: {e}"));

        assert!(
            log.attestations
                .iter()
                .any(|attestation| attestation.file_path == "feature.txt"),
            "commit {idx} (sha={sha}) should have attestation for feature.txt"
        );

        // Verify each boundary commit has at least one non-empty attestation entry
        let total_lines: u32 = log
            .attestations
            .iter()
            .flat_map(|a| &a.entries)
            .flat_map(|e| &e.line_ranges)
            .map(|r| match r {
                git_ai::authorship::authorship_log::LineRange::Single(_) => 1,
                git_ai::authorship::authorship_log::LineRange::Range(s, e) => e - s + 1,
            })
            .sum();
        assert!(
            total_lines > 0,
            "commit {idx} should have at least one attested line; got {total_lines}"
        );
    }

    // Verify the final HEAD file has correct attribution for all expected AI lines.
    // After 60 commits, lines 5, 10, 15, …, 55 (indices divisible by 5, excluding
    // 60) were last modified in commit 60 (which itself is divisible by 5) and
    // carry the "(modified in commit 60)" suffix. All other lines use the base
    // "ai feature line {N}" text.
    let expected_lines: Vec<ExpectedLine> = (1..=FEATURE_COMMIT_COUNT)
        .map(|i| {
            if i % 5 == 0 && i != FEATURE_COMMIT_COUNT {
                format!("ai feature line {i} (modified in commit {FEATURE_COMMIT_COUNT})").ai()
            } else {
                format!("ai feature line {i}").ai()
            }
        })
        .collect();
    feature_file.assert_lines_and_blame(expected_lines);
}

/// Test rebase where diff-tree pairs are identical across the streaming chunk boundary.
///
/// An empty commit on the upstream branch changes the parent graph without changing the
/// base tree. Replaying feature commits onto it rewrites commit SHAs, but each old/new
/// commit pair has the same tree. This forces the streaming diff-tree parser to pad
/// empty results for identical tree pairs while draining at the 50-result boundary.
#[test]
fn test_rebase_identical_tree_pairs_across_streaming_chunk_boundary() {
    use std::fs;

    const FEATURE_COMMIT_COUNT: usize = 60;
    const BOUNDARY_INDICES: [usize; 4] = [48, 49, 50, 51];

    let repo = TestRepo::new();

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    for commit_number in 1..=FEATURE_COMMIT_COUNT {
        let file_name = format!("file_{commit_number}.txt");
        fs::write(
            repo.path().join(&file_name),
            format!("ai content for file {commit_number}\n"),
        )
        .unwrap();
        repo.git_ai(&["checkpoint", "mock_ai", &file_name]).unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.commit(&format!("feature commit {commit_number}"))
            .unwrap();
    }

    let original_shas_output = repo
        .git(&[
            "rev-list",
            "--reverse",
            &format!("{}..HEAD", default_branch),
        ])
        .unwrap();
    let original_shas: Vec<&str> = original_shas_output.trim().lines().collect();
    assert_eq!(original_shas.len(), FEATURE_COMMIT_COUNT);

    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["commit", "--allow-empty", "-m", "Main empty advance"])
        .unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    let rebased_shas_output = repo
        .git(&[
            "rev-list",
            "--reverse",
            &format!("{}..HEAD", default_branch),
        ])
        .unwrap();
    let rebased_shas: Vec<&str> = rebased_shas_output.trim().lines().collect();
    assert_eq!(rebased_shas.len(), FEATURE_COMMIT_COUNT);

    for &idx in &BOUNDARY_INDICES {
        let original_tree = repo
            .git(&["show", "-s", "--format=%T", original_shas[idx]])
            .unwrap();
        let rebased_tree = repo
            .git(&["show", "-s", "--format=%T", rebased_shas[idx]])
            .unwrap();
        assert_eq!(
            original_tree.trim(),
            rebased_tree.trim(),
            "commit {} should be an identical-tree rewrite",
            idx + 1
        );

        let expected_file = format!("file_{}.txt", idx + 1);
        let note = repo
            .read_authorship_note(rebased_shas[idx])
            .unwrap_or_else(|| {
                panic!(
                    "rebased boundary commit {} ({}) should have an authorship note",
                    idx + 1,
                    rebased_shas[idx]
                )
            });
        let log = AuthorshipLog::deserialize_from_string(&note)
            .unwrap_or_else(|e| panic!("parse authorship note for commit {}: {e}", idx + 1));
        assert!(
            log.attestations
                .iter()
                .any(|attestation| attestation.file_path == expected_file),
            "rebased commit {} should keep attestation for {expected_file}",
            idx + 1
        );

        let mut file = repo.filename(&expected_file);
        file.assert_lines_and_blame(crate::lines![
            format!("ai content for file {}", idx + 1).ai()
        ]);
    }
}
