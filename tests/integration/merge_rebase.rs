use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;

#[test]
fn test_blame_after_merge_with_ai_contributions() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create base file and initial commit
    file.set_contents(crate::lines!["Base line 1", "Base line 2", "Base line 3"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Save the default branch name before creating feature branch
    let default_branch = repo.current_branch();

    // Create a feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Make AI changes on feature branch (insert after line 3)
    file.insert_at(
        3,
        crate::lines!["FEATURE LINE 1".ai(), "FEATURE LINE 2".ai()],
    );
    repo.stage_all_and_commit("feature branch changes").unwrap();

    // Switch back to default branch and make human changes
    repo.git(&["checkout", &default_branch]).unwrap();
    file = repo.filename("test.txt"); // Reload file from default branch
    // Insert at beginning to avoid conflict with feature branch
    file.insert_at(0, crate::lines!["MAIN LINE 1", "MAIN LINE 2"]);
    repo.stage_all_and_commit("main branch changes").unwrap();

    // Merge feature branch into default branch (should not conflict)
    repo.git(&["merge", "feature", "-m", "merge feature into main"])
        .unwrap();

    // Test blame after merge - should have both AI and human contributions
    file = repo.filename("test.txt");
    file.assert_lines_and_blame(crate::lines![
        "MAIN LINE 1".human(),
        "MAIN LINE 2".human(),
        "Base line 1".human(),
        "Base line 2".human(),
        "Base line 3".ai(),
        "FEATURE LINE 1".ai(),
        "FEATURE LINE 2".ai(),
    ]);
}

// #[test]
// fn test_blame_after_rebase_with_ai_contributions() {
//     let tmp_dir = tempdir().unwrap();
//     let repo_path = tmp_dir.path().to_path_buf();

//     // Create initial repository with base commit
//     let (mut tmp_repo, mut lines, mut alphabet) =
//         TmpRepo::new_with_base_commit(repo_path.clone()).unwrap();

//     // Create a feature branch
//     tmp_repo.create_branch("feature").unwrap();

//     // Make changes on feature branch (add lines at the end)
//     lines
//         .append("REBASE FEATURE LINE 1\nREBASE FEATURE LINE 2\n")
//         .unwrap();
//     tmp_repo.trigger_checkpoint_with_ai("Claude", Some("claude-3-sonnet"), Some("cursor")).unwrap();
//     tmp_repo
//         .commit_with_message("feature branch changes")
//         .unwrap();

//     // Switch back to the default branch and make different changes (insert in middle)
//     let default_branch = tmp_repo.get_default_branch().unwrap();
//     tmp_repo.switch_branch(&default_branch).unwrap();
//     lines
//         .insert_at(15 * 2, "REBASE MAIN LINE 1\nREBASE MAIN LINE 2\n")
//         .unwrap();
//     tmp_repo
//         .trigger_checkpoint_with_author("test_user")
//         .unwrap();
//     tmp_repo.commit_with_message("main branch changes").unwrap();

//     // Switch back to feature and rebase onto the default branch
//     tmp_repo.switch_branch("feature").unwrap();
//     let default_branch = tmp_repo.get_default_branch().unwrap();
//     tmp_repo.rebase_onto("feature", &default_branch).unwrap();

//     // Test blame after rebase
//     let blame = tmp_repo.blame_for_file(&lines, Some((30, 36))).unwrap();
//     assert_debug_snapshot!(blame);
// }

#[test]
fn test_blame_after_complex_merge_scenario() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create base file and initial commit
    file.set_contents(crate::lines!["Base line 1", "Base line 2", "Base line 3"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Save the default branch name
    let default_branch = repo.current_branch();

    // Create feature-a branch
    repo.git(&["checkout", "-b", "feature-a"]).unwrap();
    file.insert_at(
        2,
        crate::lines!["FEATURE A LINE 1".ai(), "FEATURE A LINE 2".ai()],
    );
    repo.stage_all_and_commit("feature a changes").unwrap();

    // Create feature-b branch (from feature-a)
    repo.git(&["checkout", "-b", "feature-b"]).unwrap();
    file.insert_at(
        4,
        crate::lines!["FEATURE B LINE 1".ai(), "FEATURE B LINE 2".ai()],
    );
    repo.stage_all_and_commit("feature b changes").unwrap();

    // Switch back to default branch and make human changes
    repo.git(&["checkout", &default_branch]).unwrap();
    file = repo.filename("test.txt"); // Reload file from default branch
    // Insert at beginning to avoid conflicts
    file.insert_at(
        0,
        crate::lines!["MAIN COMPLEX LINE 1", "MAIN COMPLEX LINE 2"],
    );
    repo.stage_all_and_commit("main complex changes").unwrap();

    // Merge feature-a into default branch
    repo.git(&["merge", "feature-a", "-m", "merge feature-a into main"])
        .unwrap();

    // Merge feature-b into default branch
    repo.git(&["merge", "feature-b", "-m", "merge feature-b into main"])
        .unwrap();

    // Test blame after complex merge - should have all contributions
    file = repo.filename("test.txt");
    file.assert_lines_and_blame(crate::lines![
        "MAIN COMPLEX LINE 1".human(),
        "MAIN COMPLEX LINE 2".human(),
        "Base line 1".human(),
        "Base line 2".human(),
        "FEATURE A LINE 1".ai(),
        "FEATURE A LINE 2".ai(),
        "FEATURE B LINE 1".ai(),
        "FEATURE B LINE 2".ai(),
        "Base line 3".human(),
    ]);
}

// #[test]
// fn test_blame_after_rebase_chain() {
//     let tmp_dir = tempdir().unwrap();
//     let repo_path = tmp_dir.path().to_path_buf();

//     // Create initial repository with base commit
//     let (mut tmp_repo, mut lines, mut alphabet) =
//         TmpRepo::new_with_base_commit(repo_path.clone()).unwrap();

//     // Create a feature branch
//     tmp_repo.create_branch("feature").unwrap();

//     // Make multiple commits on feature branch
//     lines.append("REBASE CHAIN 1\n").unwrap();
//     tmp_repo.trigger_checkpoint_with_ai("Claude", Some("claude-3-sonnet"), Some("cursor")).unwrap();
//     tmp_repo.commit_with_message("feature commit 1").unwrap();

//     lines.append("REBASE CHAIN 2\n").unwrap();
//     tmp_repo.trigger_checkpoint_with_author("GPT-4").unwrap();
//     tmp_repo.commit_with_message("feature commit 2").unwrap();

//     // Switch back to the default branch and make changes
//     let default_branch = tmp_repo.get_default_branch().unwrap();
//     tmp_repo.switch_branch(&default_branch).unwrap();
//     lines.append("MAIN CHAIN 1\n").unwrap();
//     tmp_repo
//         .trigger_checkpoint_with_author("test_user")
//         .unwrap();
//     tmp_repo.commit_with_message("main commit 1").unwrap();

//     lines.append("MAIN CHAIN 2\n").unwrap();
//     tmp_repo
//         .trigger_checkpoint_with_author("test_user")
//         .unwrap();
//     tmp_repo.commit_with_message("main commit 2").unwrap();

//     // Switch back to feature and rebase onto the default branch
//     tmp_repo.switch_branch("feature").unwrap();
//     let default_branch = tmp_repo.get_default_branch().unwrap();
//     tmp_repo.rebase_onto("feature", &default_branch).unwrap();

//     // Test blame after rebase chain
//     let blame = tmp_repo.blame_for_file(&lines, None).unwrap();
//     println!("blame: {:?}", blame);
//     assert_debug_snapshot!(blame);
// }

#[test]
fn test_blame_after_merge_conflict_resolution() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create base file with multiple lines
    file.set_contents(crate::lines![
        "Line 1", "Line 2", "Line 3", "Line 4", "Line 5", "Line 6", "Line 7", "Line 8", "Line 9",
        "Line 10",
    ]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Save the default branch name
    let default_branch = repo.current_branch();

    // Create a feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Make AI changes on feature branch (replace line 5)
    file.replace_at(4, "CONFLICT FEATURE VERSION".ai());
    repo.stage_all_and_commit("feature conflict changes")
        .unwrap();

    // Switch back to default branch and make conflicting human changes
    repo.git(&["checkout", &default_branch]).unwrap();
    file = repo.filename("test.txt"); // Reload file from main branch
    file.replace_at(4, "CONFLICT MAIN VERSION");
    repo.stage_all_and_commit("main conflict changes").unwrap();

    // Merge feature branch into main (conflicts will occur)
    // Git will exit with error on conflict, so we handle it
    let merge_result = repo.git(&[
        "merge",
        "feature",
        "-m",
        "merge feature with conflict resolution",
    ]);

    if merge_result.is_err() {
        // Resolve conflict by accepting main's version
        file = repo.filename("test.txt");
        file.set_contents(crate::lines![
            "Line 1",
            "Line 2",
            "Line 3",
            "Line 4",
            "CONFLICT MAIN VERSION",
            "Line 6",
            "Line 7",
            "Line 8",
            "Line 9",
            "Line 10",
        ]);
        repo.stage_all_and_commit("merge feature with conflict resolution")
            .unwrap();
    }

    // Test blame after conflict resolution
    file = repo.filename("test.txt");
    file.assert_lines_and_blame(crate::lines![
        "Line 1".human(),
        "Line 2".human(),
        "Line 3".human(),
        "Line 4".human(),
        "CONFLICT MAIN VERSION".human(),
        "Line 6".human(),
        "Line 7".human(),
        "Line 8".human(),
        "Line 9".human(),
        "Line 10".human(),
    ]);
}

/// Regression test for #953: a merge conflict resolved by AI (mock_ai) via checkpoint,
/// committed without an active AI coding session, must produce correct attribution.
///
/// Scenario: Two branches diverge on the same line.  The merge conflicts.  An AI tool
/// (mock_ai) resolves the conflict by rewriting the file and calling `git-ai checkpoint`.
/// The human then stages and commits.  The commit should attribute the AI-written line
/// to mock_ai and the unchanged human line to the human author.
#[test]
fn test_merge_conflict_ai_resolution_outside_session() {
    let repo = TestRepo::new();

    // Base: two-line Python class.
    let mut file = repo.filename("app.py");
    file.set_contents(crate::lines!["class App:", "    pass"]);
    repo.stage_all_and_commit("initial").unwrap();
    let main_branch = repo.current_branch();

    // Feature branch: AI rewrites line 2.
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("app.py");
    feature_file.replace_at(1, "    def feature(): pass".ai());
    repo.stage_all_and_commit("feature AI change").unwrap();

    // Main: human rewrites the same line differently → guaranteed conflict on merge.
    repo.git(&["checkout", &main_branch]).unwrap();
    let mut main_file = repo.filename("app.py");
    main_file.replace_at(1, "    def main(): pass");
    repo.stage_all_and_commit("main human change").unwrap();

    // Merge feature into main — conflicts on line 2.
    let merge_result = repo.git(&["merge", "feature"]);
    assert!(
        merge_result.is_err(),
        "merge should conflict because both branches modified the same line"
    );

    // AI (mock_ai) resolves the conflict: keeps both methods.
    // "class App:" is unchanged from HEAD (human attribution).
    // "    def feature(): pass" is the new AI-introduced line.
    // "    def main(): pass" was already in HEAD (human attribution).
    use std::fs;
    let resolved_content = "class App:\n    def feature(): pass\n    def main(): pass\n";
    fs::write(repo.path().join("app.py"), resolved_content).unwrap();

    // Stage the resolved file first.  The checkpoint skips files that git still considers
    // "unmerged" (i.e. not yet staged after conflict resolution), so we must `git add`
    // before calling checkpoint to ensure the file is processed and attributed.
    repo.git(&["add", "app.py"]).unwrap();

    // Checkpoint attributes the staged content to mock_ai.
    repo.git_ai(&["checkpoint", "mock_ai", "app.py"]).unwrap();

    // Human commits the merge resolution.
    let _merge_commit = repo.stage_all_and_commit("merge resolved by AI").unwrap();

    // "class App:" was never in the conflict — it was identical on both branches → human.
    // "    def feature(): pass" and "    def main(): pass" — the mock_ai checkpoint
    // attributed the entire resolution to AI. The post-commit hook's working-log-based
    // note (ground truth from checkpoint) takes precedence over any rewrite handler.
    file.assert_lines_and_blame(crate::lines![
        "class App:".human(),
        "    def feature(): pass".ai(),
        "    def main(): pass".ai(),
    ]);
}

crate::reuse_tests_in_worktree!(
    test_blame_after_merge_conflict_resolution,
    test_merge_conflict_ai_resolution_outside_session,
);
