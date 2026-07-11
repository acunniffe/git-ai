use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use std::collections::HashMap;

fn deterministic_commit_env(timestamp: &'static str) -> [(&'static str, &'static str); 2] {
    [
        ("GIT_AUTHOR_DATE", timestamp),
        ("GIT_COMMITTER_DATE", timestamp),
    ]
}

/// Test merge --squash with a simple feature branch containing AI and human edits
#[test]
fn test_prepare_working_log_simple_squash() {
    let repo = TestRepo::new();
    let mut file = repo.filename("main.txt");

    // Create master branch with initial content
    file.set_contents(crate::lines!["line 1", "line 2", "line 3", ""]);
    repo.stage_all_and_commit("Initial commit on master")
        .unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Add AI changes on feature branch
    file.insert_at(3, crate::lines!["// AI added feature".ai()]);
    repo.stage_all_and_commit_with_env(
        "Add AI feature",
        &deterministic_commit_env("2030-01-01T00:00:00Z"),
    )
    .unwrap();

    // Add human changes on feature branch
    file.insert_at(4, crate::lines!["// Human refinement"]);
    repo.stage_all_and_commit_with_env(
        "Human refinement",
        &deterministic_commit_env("2030-01-01T00:00:01Z"),
    )
    .unwrap();

    // Go back to master and squash merge
    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    repo.commit("Squashed feature").unwrap();

    // Verify AI attribution is preserved
    file.assert_lines_and_blame(crate::lines![
        "line 1".human(),
        "line 2".human(),
        "line 3".human(),
        "// AI added feature".ai(),
        "// Human refinement".human()
    ]);

    // Verify stats for squashed commit
    let stats = repo.stats().unwrap();
    assert_eq!(stats.git_diff_added_lines, 2, "Squash commit adds 2 lines");
    assert_eq!(stats.ai_additions, 1, "1 AI line from feature branch");
    assert_eq!(stats.ai_accepted, 1, "1 AI line accepted without edits");
    assert_eq!(
        stats.human_additions, 1,
        "1 human lines from feature branch"
    );
}

/// Test merge --squash with out-of-band changes on master (handles 3-way merge)
#[test]
fn test_prepare_working_log_squash_with_main_changes() {
    let repo = TestRepo::new();
    let mut file = repo.filename("document.txt");

    // Create master branch with initial content
    file.set_contents(crate::lines!["section 1", "section 2", "section 3"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch and add AI changes
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(3, crate::lines!["// AI feature addition at end".ai()]);
    repo.stage_all_and_commit("AI adds feature").unwrap();

    // Switch back to master and make out-of-band changes
    repo.git(&["checkout", &default_branch]).unwrap();

    // Re-initialize file after checkout to get current master state
    let mut file = repo.filename("document.txt");
    file.insert_at(0, crate::lines!["// Master update at top"]);
    repo.stage_all_and_commit("Out-of-band update on master")
        .unwrap();

    // Squash merge feature into master
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    repo.stage_all_and_commit("Squashed feature with out-of-band")
        .unwrap();

    // Verify both changes are present with correct attribution.
    // "section 3" gets AI attribution because git's diff includes it in the hunk
    // (its trailing newline changed when the AI line was appended after it).
    file.assert_lines_and_blame(crate::lines![
        "// Master update at top".human(),
        "section 1".human(),
        "section 2".human(),
        "section 3".ai(),
        "// AI feature addition at end".ai()
    ]);

    // Verify stats for squashed commit
    let stats = repo.stats().unwrap();
    assert_eq!(
        stats.git_diff_added_lines, 2,
        "Squash commit adds 2 lines from feature (includes newline)"
    );
    assert_eq!(stats.ai_additions, 2, "2 AI lines from feature branch");
    assert_eq!(stats.ai_accepted, 2, "2 AI lines accepted without edits");
    assert_eq!(
        stats.human_additions, 0,
        "0 human lines from feature branch"
    );
}

/// Test merge --squash with multiple AI sessions and human edits
#[test]
fn test_prepare_working_log_squash_multiple_sessions() {
    let repo = TestRepo::new();
    let mut file = repo.filename("file.txt");

    // Create master branch
    file.set_contents(crate::lines!["header", "body", "footer"]);
    repo.stage_all_and_commit("Initial").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // First AI session
    file.insert_at(1, crate::lines!["// AI session 1".ai()]);
    repo.stage_all_and_commit("AI session 1").unwrap();

    // Human edit
    file.insert_at(3, crate::lines!["// Human addition"]);
    repo.stage_all_and_commit("Human edit").unwrap();

    // Second AI session (different agent - simulated by new checkpoint)
    file.insert_at(5, crate::lines!["// AI session 2".ai()]);
    repo.stage_all_and_commit("AI session 2").unwrap();

    // Squash merge into master
    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    repo.commit("Squashed multiple sessions").unwrap();

    // Verify all authorship is preserved.
    // "footer" gets AI attribution because git's diff includes it in the hunk
    // (trailing newline changed when AI session 2 appended after it).
    file.assert_lines_and_blame(crate::lines![
        "header".human(),
        "// AI session 1".ai(),
        "body".human(),
        "// Human addition".human(),
        "footer".ai(),
        "// AI session 2".ai()
    ]);

    // Verify stats for squashed commit with multiple sessions
    let stats = repo.stats().unwrap();
    assert_eq!(
        stats.git_diff_added_lines, 4,
        "Squash commit adds 4 lines total (includes newline)"
    );
    assert_eq!(
        stats.ai_additions, 3,
        "3 AI lines from feature branch (both sessions + trailing-newline on footer)"
    );
    assert_eq!(stats.ai_accepted, 3, "3 AI lines accepted without edits");
    assert_eq!(
        stats.human_additions, 1,
        "1 human line from feature branch (Human addition)"
    );
}

/// Test merge --squash with mixed additions (AI code edited by human before commit)
#[test]
fn test_prepare_working_log_squash_with_mixed_additions() {
    let repo = TestRepo::new();
    let mut file = repo.filename("code.txt");

    // Create master branch with initial content
    file.set_contents(crate::lines![
        "function start() {",
        "  // initial code",
        "}"
    ]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // AI adds 3 lines (without committing)
    file.insert_at(
        2,
        crate::lines![
            "  const x = 1;".ai(),
            "  const y = 2;".ai(),
            "  const z = 3;".ai()
        ],
    );

    // Human immediately edits the middle AI line (before committing)
    // This creates a "mixed addition" - AI generated, human edited
    file.replace_at(3, "  const y = 20; // human modified");

    // Now commit with both AI and human changes together
    repo.stage_all_and_commit("AI adds variables, human refines")
        .unwrap();

    file.insert_at(
        0,
        crate::lines![
            "// AI comment".ai(),
            "// Describing the code".ai(),
            "// And how it works".ai(),
        ],
    );

    repo.stage_all_and_commit("AI adds comment").unwrap();

    // Squash merge back to master
    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    let squash_commit = repo.commit("Squashed feature with mixed edits").unwrap();
    squash_commit.print_authorship();

    // Verify attribution - edited line should be human
    file.assert_lines_and_blame(crate::lines![
        "// AI comment".ai(),
        "// Describing the code".ai(),
        "// And how it works".ai(),
        "function start() {".human(),
        "  // initial code".human(),
        "  const x = 1;".ai(),
        "  const y = 20; // human modified".human(), // Human edited AI line
        "  const z = 3;".ai(),
        "}".human()
    ]);

    // Verify stats
    let stats = repo.stats().unwrap();
    println!("stats: {:?}", stats);
    assert_eq!(
        stats.git_diff_added_lines, 6,
        "Squash commit adds 6 lines total"
    );
    assert_eq!(stats.ai_additions, 5, "5 AI lines total");
    assert_eq!(stats.ai_accepted, 5, "5 AI lines accepted");
    assert_eq!(stats.human_additions, 1, "1 human addition");

    // Verify session records exist (sessions don't have stats fields)
    let sessions = &squash_commit.authorship_log.metadata.sessions;
    assert!(
        !sessions.is_empty(),
        "Should have at least one session record"
    );

    // Sessions don't track stats like prompts did - they only have agent_id, human_author, messages, etc.
    for (session_id, session_record) in sessions {
        println!(
            "Session {}: agent_id={:?}, human_author={:?}",
            session_id, session_record.agent_id, session_record.human_author
        );
    }
}

/// Test that custom attributes set via config are preserved through a squash merge
/// when the real post-commit pipeline injects them.
#[test]
fn test_squash_merge_preserves_custom_attributes_from_config() {
    let mut repo = TestRepo::new_dedicated_daemon();

    // Configure custom attributes via config patch
    let mut attrs = HashMap::new();
    attrs.insert("employee_id".to_string(), "E303".to_string());
    attrs.insert("team".to_string(), "data".to_string());
    repo.patch_git_ai_config(|patch| {
        patch.custom_attributes = Some(attrs.clone());
    });

    // Create initial commit on default branch
    let mut file = repo.filename("main.txt");
    file.set_contents(crate::lines!["line 1", "line 2", "line 3", ""]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    // Create feature branch with AI commit
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(3, crate::lines!["// AI feature line".ai()]);
    repo.stage_all_and_commit_with_env(
        "Add AI feature",
        &deterministic_commit_env("2030-01-02T00:00:00Z"),
    )
    .unwrap();

    // Add another AI commit on the feature branch
    file.insert_at(4, crate::lines!["// AI feature line 2".ai()]);
    repo.stage_all_and_commit_with_env(
        "Add AI feature 2",
        &deterministic_commit_env("2030-01-02T00:00:01Z"),
    )
    .unwrap();

    // Verify custom attributes were set on the feature commits
    let feature_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let feature_note = repo
        .read_authorship_note(&feature_sha)
        .expect("feature commit should have authorship note");
    let feature_log =
        AuthorshipLog::deserialize_from_string(&feature_note).expect("parse feature note");
    for prompt in feature_log.metadata.sessions.values() {
        assert_eq!(
            prompt.custom_attributes.as_ref(),
            Some(&attrs),
            "precondition: feature commit should have custom_attributes from config"
        );
    }

    // Go back to default branch and squash merge
    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    repo.commit("Squashed feature").unwrap();

    // Verify custom attributes survived the squash merge
    let squash_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let squash_note = repo
        .read_authorship_note(&squash_sha)
        .expect("squash commit should have authorship note");
    let squash_log =
        AuthorshipLog::deserialize_from_string(&squash_note).expect("parse squash note");
    assert!(
        !squash_log.metadata.sessions.is_empty(),
        "squash commit should have session records"
    );
    for prompt in squash_log.metadata.sessions.values() {
        assert_eq!(
            prompt.custom_attributes.as_ref(),
            Some(&attrs),
            "custom_attributes should be preserved through squash merge"
        );
    }

    // Also verify the AI attribution itself survived
    file.assert_lines_and_blame(crate::lines![
        "line 1".human(),
        "line 2".human(),
        "line 3".human(),
        "// AI feature line".ai(),
        "// AI feature line 2".ai()
    ]);
}

/// Regression test for #950: squash rebase should preserve all AI attribution
/// even when two sessions have interleaved lines
#[test]
fn test_squash_rebase_preserves_interleaved_attribution() {
    let repo = TestRepo::new();

    // Create initial commit
    let mut file = repo.filename("module.py");
    file.set_contents(crate::lines!["# module"]);
    repo.stage_all_and_commit("initial").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Session A creates a 5-line class
    file.set_contents(crate::lines![
        "class Store:".ai(),
        "    def __init__(self):".ai(),
        "        self.data = {}".ai(),
        "    def get(self, k):".ai(),
        "        return self.data.get(k)".ai(),
    ]);
    repo.stage_all_and_commit("Session A: create Store class")
        .unwrap();

    // Session B adds interleaved lines (some between A's lines, some after)
    file.set_contents(crate::lines![
        "class Store:".ai(),
        "    \"\"\"A data store.\"\"\"".ai(),
        "    def __init__(self):".ai(),
        "        self.data = {}".ai(),
        "        self.cache = {}".ai(),
        "    def get(self, k):".ai(),
        "        \"\"\"Get value.\"\"\"".ai(),
        "        return self.data.get(k)".ai(),
        "    def set(self, k, v):".ai(),
        "        self.data[k] = v".ai(),
    ]);
    repo.stage_all_and_commit("Session B: add docstrings and set method")
        .unwrap();

    // Squash the two commits using merge --squash
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    repo.stage_all_and_commit("squash merge").unwrap();

    let stats = repo.stats().unwrap();

    assert_eq!(
        stats.ai_additions, 10,
        "All 10 lines should be AI-attributed after squash, got ai={} human={}",
        stats.ai_additions, stats.human_additions
    );
    assert_eq!(stats.human_additions, 0, "No human lines expected");

    // Verify each line individually via blame — the stats check above could in theory
    // pass if the numbers happen to match but the wrong lines are attributed.
    file.assert_lines_and_blame(crate::lines![
        "class Store:".ai(),
        "    \"\"\"A data store.\"\"\"".ai(),
        "    def __init__(self):".ai(),
        "        self.data = {}".ai(),
        "        self.cache = {}".ai(),
        "    def get(self, k):".ai(),
        "        \"\"\"Get value.\"\"\"".ai(),
        "        return self.data.get(k)".ai(),
        "    def set(self, k, v):".ai(),
        "        self.data[k] = v".ai(),
    ]);
}

/// Variant of test_prepare_working_log_squash_with_main_changes using unattributed (legacy)
/// human checkpoints. With git diff-tree hunk-shift, "section 3" gains AI attribution
/// because git's diff includes it in the hunk (trailing newline changed when AI appended
/// a line after it). This matches git's own attribution semantics.
#[test]
fn test_prepare_working_log_squash_with_main_changes_standard_human() {
    let repo = TestRepo::new_with_daemon_scope(crate::repos::test_repo::DaemonTestScope::Dedicated);
    let mut file = repo.filename("document.txt");

    // Create master branch with initial content
    file.set_contents(crate::lines![
        "section 1".unattributed_human(),
        "section 2".unattributed_human(),
        "section 3".unattributed_human()
    ]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch and add AI changes
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(3, crate::lines!["// AI feature addition at end".ai()]);
    repo.stage_all_and_commit("AI adds feature").unwrap();

    // Switch back to master and make out-of-band changes
    repo.git(&["checkout", &default_branch]).unwrap();

    // Re-initialize file after checkout to get current master state
    let mut file = repo.filename("document.txt");
    file.insert_at(
        0,
        crate::lines!["// Master update at top".unattributed_human()],
    );
    repo.stage_all_and_commit("Out-of-band update on master")
        .unwrap();

    // Squash merge feature into master
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    repo.stage_all_and_commit("Squashed feature with out-of-band")
        .unwrap();

    // "section 3" gets AI attribution because git's diff includes it in the hunk
    // (trailing newline changed when AI appended a line after it).
    file.assert_lines_and_blame(crate::lines![
        "// Master update at top".human(),
        "section 1".human(),
        "section 2".human(),
        "section 3".ai(),
        "// AI feature addition at end".ai()
    ]);

    let stats = repo.stats().unwrap();
    assert_eq!(
        stats.git_diff_added_lines, 2,
        "Squash commit adds 2 lines from feature (includes newline)"
    );
    assert_eq!(stats.ai_additions, 2, "2 AI lines from feature branch");
    assert_eq!(stats.ai_accepted, 2, "2 AI lines accepted without edits");
    assert_eq!(
        stats.human_additions, 0,
        "0 human lines from feature branch"
    );
}

/// Variant of test_prepare_working_log_squash_multiple_sessions using unattributed (legacy)
/// human checkpoints. With git diff-tree hunk-shift, "footer" gains AI attribution because
/// git's diff includes it in the hunk (trailing newline changed when AI session 2 appended).
#[test]
fn test_prepare_working_log_squash_multiple_sessions_standard_human() {
    let repo = TestRepo::new();
    let mut file = repo.filename("file.txt");

    // Create master branch
    file.set_contents(crate::lines![
        "header".unattributed_human(),
        "body".unattributed_human(),
        "footer".unattributed_human()
    ]);
    repo.stage_all_and_commit("Initial").unwrap();

    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // First AI session
    file.insert_at(1, crate::lines!["// AI session 1".ai()]);
    repo.stage_all_and_commit("AI session 1").unwrap();

    // Human edit
    file.insert_at(3, crate::lines!["// Human addition".unattributed_human()]);
    repo.stage_all_and_commit("Human edit").unwrap();

    // Second AI session (different agent - simulated by new checkpoint)
    file.insert_at(5, crate::lines!["// AI session 2".ai()]);
    repo.stage_all_and_commit("AI session 2").unwrap();

    // Squash merge into master
    repo.git(&["checkout", &default_branch]).unwrap();
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    repo.commit("Squashed multiple sessions").unwrap();

    // "footer" gets AI attribution — git's diff includes it in the hunk
    // (trailing newline changed when AI session 2 appended after it).
    file.assert_lines_and_blame(crate::lines![
        "header".human(),
        "// AI session 1".ai(),
        "body".human(),
        "// Human addition".human(),
        "footer".ai(),
        "// AI session 2".ai()
    ]);

    let stats = repo.stats().unwrap();
    assert_eq!(
        stats.git_diff_added_lines, 4,
        "Squash commit adds 4 lines total (includes newline)"
    );
    assert_eq!(
        stats.ai_additions, 3,
        "3 AI lines from feature branch (both sessions + trailing-newline on footer)"
    );
    assert_eq!(stats.ai_accepted, 3, "3 AI lines accepted without edits");
    assert_eq!(
        stats.human_additions, 1,
        "1 KnownHuman-attested line recovered from the no-AI human addition"
    );
    assert_eq!(stats.unknown_additions, 0, "0 unattested lines");
}

crate::reuse_tests_in_worktree!(
    test_prepare_working_log_simple_squash,
    test_prepare_working_log_squash_with_main_changes,
    test_prepare_working_log_squash_multiple_sessions,
    test_prepare_working_log_squash_with_mixed_additions,
    test_prepare_working_log_squash_with_main_changes_standard_human,
    test_prepare_working_log_squash_multiple_sessions_standard_human,
);
