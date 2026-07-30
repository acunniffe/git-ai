use crate::repos::test_file::ExpectedLineExt;
use std::fs;

crate::subdir_test_variants! {
    fn commit() {
        // Test that git commit works correctly when run from within a subdirectory
        let repo = TestRepo::new();

        // Create a subdirectory structure
        let working_dir = repo.path().join("src").join("lib");
        fs::create_dir_all(&working_dir).unwrap();

        // Create initial file in root
        let mut root_file = repo.filename("README.md");
        root_file.set_contents(crate::lines!["# Project".human()]);
        repo.stage_all_and_commit("Initial commit").unwrap();

        // Create a file in the subdirectory
        let subdir_file_path = working_dir.join("utils.rs");
        fs::write(&subdir_file_path, "pub fn helper() {\n    println!(\"hello\");\n}\n").unwrap();

        // Stage the file
        repo.git(&["add", "src/lib/utils.rs"]).unwrap();

        // Create AI checkpoint for the file in subdirectory
        repo.git_ai(&["checkpoint", "mock_ai", "src/lib/utils.rs"]).unwrap();

        // Now commit from within the subdirectory (not using -C flag)
        // This simulates running "git commit" from within the subdirectory
        // git-ai should automatically find the repository root
        repo.git_from_working_dir(&working_dir, &["commit", "-m", "Add utils from subdirectory"])
            .expect("Failed to commit from subdirectory");

        // Verify that the file was committed and has AI attribution
        let mut file = repo.filename("src/lib/utils.rs");
        file.assert_lines_and_blame(crate::lines![
            "pub fn helper() {".ai(),
            "    println!(\"hello\");".ai(),
            "}".ai(),
        ]);
    }
}

crate::subdir_test_variants! {
    fn commit_with_mixed_files() {
        // Test committing files from both root and subdirectory
    let repo = TestRepo::new();

    // Create subdirectory structure
    let working_dir = repo.path().join("src");
    fs::create_dir_all(&working_dir).unwrap();

    // Create initial commit
    let mut root_file = repo.filename("README.md");
    root_file.set_contents(crate::lines!["# Project".human()]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Create file in subdirectory (AI-authored)
    let subdir_file_path = working_dir.join("main.rs");
    fs::write(&subdir_file_path, "fn main() {\n    println!(\"Hello, world!\");\n}\n").unwrap();

    // Create AI checkpoint for the file in subdirectory
    repo.git_ai(&["checkpoint", "mock_ai", "src/main.rs"]).unwrap();

    // Create file in root (human-authored)
    let root_file_path = repo.path().join("LICENSE");
    fs::write(&root_file_path, "MIT License\n").unwrap();

    // Stage both files
    repo.git(&["add", "src/main.rs", "LICENSE"]).unwrap();

    // Create human checkpoint
    repo.git_ai(&["checkpoint"]).unwrap(); // Human checkpoint for LICENSE

    // Commit (not using -C flag)
    // git-ai should automatically find the repository root
    repo.git_from_working_dir(&working_dir, &["commit", "-m", "Add files"])
        .expect("Failed to commit");

    // Verify AI attribution for subdirectory file
    let mut subdir_file = repo.filename("src/main.rs");
    subdir_file.assert_lines_and_blame(crate::lines![
        "fn main() {".ai(),
        "    println!(\"Hello, world!\");".ai(),
        "}".ai(),
    ]);

    // Verify human attribution for root file
    let mut license_file = repo.filename("LICENSE");
    license_file.assert_lines_and_blame(crate::lines![
        "MIT License".human(),
    ]);
    }
}

crate::subdir_test_variants! {
    fn commit_nested() {
        // Test committing from a deeply nested subdirectory
    let repo = TestRepo::new();

    // Create deeply nested subdirectory structure
    let working_dir = repo.path().join("a").join("b").join("c");
    fs::create_dir_all(&working_dir).unwrap();

    // Create initial commit
    let mut root_file = repo.filename("README.md");
    root_file.set_contents(crate::lines!["# Project".human()]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Create file in nested subdirectory
    let nested_file_path = working_dir.join("deep.rs");
    fs::write(&nested_file_path, "pub mod deep {\n    pub fn func() {}\n}\n").unwrap();

    // Stage the file
    repo.git(&["add", "a/b/c/deep.rs"]).unwrap();

    // Create AI checkpoint
    repo.git_ai(&["checkpoint", "mock_ai", "a/b/c/deep.rs"]).unwrap();

    // Commit (not using -C flag)
    // git-ai should automatically find the repository root
    repo.git_from_working_dir(&working_dir, &["commit", "-m", "Add deep file"])
        .expect("Failed to commit");

    // Verify attribution
    let mut file = repo.filename("a/b/c/deep.rs");
    file.assert_lines_and_blame(crate::lines![
        "pub mod deep {".ai(),
        "    pub fn func() {}".ai(),
        "}".ai(),
    ]);
    }
}

crate::subdir_test_variants! {
    fn rebase_no_conflicts() {
        // Test that rebase works correctly
    let repo = TestRepo::new();

    // Create subdirectory structure
    let working_dir = repo.path().join("src").join("lib");
    fs::create_dir_all(&working_dir).unwrap();

    // Create initial commit
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main line 1", "main line 2"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Get the default branch name
    let default_branch = repo.current_branch();

    // Create feature branch with AI commits
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
    repo.git_from_working_dir(&working_dir, &["rebase", &default_branch])
        .expect("Rebase should succeed");

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
}

crate::subdir_test_variants! {
    fn rebase_multiple_commits() {
        // Test rebase with multiple commits
    let repo = TestRepo::new();

    // Create subdirectory structure
    let working_dir = repo.path().join("src");
    fs::create_dir_all(&working_dir).unwrap();

    // Create initial commit
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main content"]);
    repo.stage_all_and_commit("Initial").unwrap();

    // Get default branch name
    let default_branch = repo.current_branch();

    // Create feature branch with multiple AI commits
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
    repo.git_from_working_dir(&working_dir, &["rebase", &default_branch])
        .expect("Rebase should succeed");

    // Verify all files have preserved AI authorship after rebase
    feature1.assert_lines_and_blame(crate::lines!["// AI feature 1".ai()]);
    feature2.assert_lines_and_blame(crate::lines!["// AI feature 2".ai()]);
    feature3.assert_lines_and_blame(crate::lines!["// AI feature 3".ai()]);
    }
}

crate::subdir_test_variants! {
    fn rebase_mixed_authorship() {
        // Test rebase where only some commits have authorship logs
    let repo = TestRepo::new();

    // Create subdirectory structure
    let working_dir = repo.path().join("src").join("components");
    fs::create_dir_all(&working_dir).unwrap();

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
    repo.git_from_working_dir(&working_dir, &["rebase", &default_branch])
        .expect("Rebase should succeed");

    // Verify authorship was preserved correctly
    human_file.assert_lines_and_blame(crate::lines!["human work".human()]);
    ai_file.assert_lines_and_blame(crate::lines!["// AI work".ai()]);
    }
}

crate::subdir_test_variants! {
    fn rebase_with_different_trees() {
        // Test rebase where trees differ (parent changes result in different tree IDs)
    let repo = TestRepo::new();

    // Create subdirectory structure
    let working_dir = repo.path().join("lib");
    fs::create_dir_all(&working_dir).unwrap();

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

    // Rebase feature onto default branch (no conflicts, but trees will differ)
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git_from_working_dir(&working_dir, &["rebase", &default_branch])
        .expect("Rebase should succeed");

    // Verify authorship was preserved for both files after rebase
    feature1.assert_lines_and_blame(crate::lines!["// AI added feature 1".ai()]);
    feature2.assert_lines_and_blame(crate::lines!["// AI added feature 2".ai()]);
    }
}

crate::subdir_test_variants! {
    fn rebase_with_files_in_subdirs() {
        // Test rebase where feature branch has files in subdirectories
    let repo = TestRepo::new();

    // Create subdirectory structure
    let working_dir = repo.path().join("src").join("lib");
    fs::create_dir_all(&working_dir).unwrap();

    // Create initial commit
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Get default branch name
    let default_branch = repo.current_branch();

    // Create feature branch with AI commits in subdirectories
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Create subdirectory file with AI content
    let subdir_path = repo.path().join("src").join("lib");
    fs::create_dir_all(&subdir_path).unwrap();
    let mut feature_file = repo.filename("src/lib/utils.rs");
    feature_file.set_contents(crate::lines![
        "// AI generated utils".ai(),
        "pub fn helper() {}".ai()
    ]);
    repo.stage_all_and_commit("AI feature in subdir").unwrap();

    // Advance default branch
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut other_file = repo.filename("other.txt");
    other_file.set_contents(crate::lines!["other content"]);
    repo.stage_all_and_commit("Main advances").unwrap();

    // Rebase feature onto default branch
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git_from_working_dir(&working_dir, &["rebase", &default_branch])
        .expect("Rebase should succeed");

    // Verify authorship was preserved for file in subdirectory after rebase
    feature_file.assert_lines_and_blame(crate::lines![
        "// AI generated utils".ai(),
        "pub fn helper() {}".ai()
    ]);
    }
}

crate::subdir_test_variants! {
    fn rebase_nested() {
        // Test rebase when run from a deeply nested subdirectory
    let repo = TestRepo::new();

    // Create deeply nested subdirectory structure
    let working_dir = repo.path().join("a").join("b").join("c");
    fs::create_dir_all(&working_dir).unwrap();

    // Create initial commit
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Get default branch name
    let default_branch = repo.current_branch();

    // Create feature branch with AI commits
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // First AI commit
    let mut feature1 = repo.filename("feature1.txt");
    feature1.set_contents(crate::lines![
        "// AI feature 1".ai(),
        "function feature1() {}".ai()
    ]);
    repo.stage_all_and_commit("AI feature 1").unwrap();

    // Second AI commit
    let mut feature2 = repo.filename("feature2.txt");
    feature2.set_contents(crate::lines![
        "// AI feature 2".ai(),
        "function feature2() {}".ai()
    ]);
    repo.stage_all_and_commit("AI feature 2").unwrap();

    // Advance default branch
    repo.git(&["checkout", &default_branch]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main work"]);
    repo.stage_all_and_commit("Main advances").unwrap();

    // Rebase feature onto default branch
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git_from_working_dir(&working_dir, &["rebase", &default_branch])
        .expect("Rebase should succeed");

    // Verify AI authorship is preserved after rebase
    feature1.assert_lines_and_blame(crate::lines![
        "// AI feature 1".ai(),
        "function feature1() {}".ai()
    ]);
    feature2.assert_lines_and_blame(crate::lines![
        "// AI feature 2".ai(),
        "function feature2() {}".ai()
    ]);
    }
}

crate::subdir_test_variants! {
    fn rebase_fast_forward() {
        // Test empty rebase (fast-forward)
    let repo = TestRepo::new();

    // Create subdirectory structure
    let working_dir = repo.path().join("src");
    fs::create_dir_all(&working_dir).unwrap();

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

    // Rebase onto default branch (should be fast-forward, no changes)
    repo.git_from_working_dir(&working_dir, &["rebase", &default_branch])
        .expect("Fast-forward rebase should succeed");

    // Verify authorship is still correct after fast-forward rebase
    feature_file.assert_lines_and_blame(crate::lines!["// AI feature".ai()]);
    }
}

crate::subdir_test_variants! {
    fn rebase_with_conflicts() {
        // Test rebase --onto from a subdirectory; ensure authorship preserved
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src").join("lib");
        fs::create_dir_all(&working_dir).unwrap();

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

        // Create feature branch from old_base with AI commit
        repo.git(&["checkout", "-b", "feature"]).unwrap();
        let mut feature_file = repo.filename("feature.txt");
        feature_file.set_contents(crate::lines!["// AI feature".ai()]);
        repo.stage_all_and_commit("AI feature").unwrap();

        // Create new_base branch from default branch
        repo.git(&["checkout", &default_branch]).unwrap();
        repo.git(&["checkout", "-b", "new_base"]).unwrap();
        let mut new_file = repo.filename("new.txt");
        new_file.set_contents(crate::lines!["new base"]);
        repo.stage_all_and_commit("New base commit").unwrap();
        let new_base_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

        // Rebase feature --onto new_base old_base from the subdirectory
        repo.git(&["checkout", "feature"]).unwrap();
        repo.git_from_working_dir(
            &working_dir,
            &["rebase", "--onto", &new_base_sha, &old_base_sha]
        )
        .expect("Rebase --onto should succeed");

        // Verify authorship preserved after rebase
        feature_file.assert_lines_and_blame(crate::lines!["// AI feature".ai()]);
    }
}

crate::subdir_test_variants! {
    fn rebase_abort() {
        // Test rebase abort - ensures no authorship corruption
    let repo = TestRepo::new();

    // Create subdirectory structure
    let working_dir = repo.path().join("src");
    fs::create_dir_all(&working_dir).unwrap();

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
    let rebase_result = repo.git_from_working_dir(&working_dir, &["rebase", &default_branch]);

    // Should conflict
    assert!(rebase_result.is_err(), "Rebase should conflict");

    // Abort the rebase
    repo.git_from_working_dir(&working_dir, &["rebase", "--abort"])
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
}

crate::subdir_test_variants! {
    fn reset_hard() {
        // Test git reset --hard: should discard all changes and reset to target commit
    let repo = TestRepo::new();

    // Create subdirectory structure
    let working_dir = repo.path().join("src").join("lib");
    fs::create_dir_all(&working_dir).unwrap();

    let mut file = repo.filename("test.txt");

    // Create initial commit
    file.set_contents(crate::lines!["line 1", "line 2", "line 3"]);
    let first_commit = repo.stage_all_and_commit("First commit").unwrap();

    // Make second commit with AI changes
    file.insert_at(3, crate::lines!["// AI line".ai()]);
    repo.stage_all_and_commit("Second commit").unwrap();

    // Make some uncommitted AI changes
    file.insert_at(4, crate::lines!["// Uncommitted".ai()]);

    // Reset --hard to first commit
    repo.git_from_working_dir(&working_dir, &["reset", "--hard", &first_commit.commit_sha])
        .expect("reset --hard should succeed");

    // After hard reset, file should match first commit (no AI lines, no uncommitted changes)
    file = repo.filename("test.txt");
    file.assert_lines_and_blame(crate::lines!["line 1", "line 2", "line 3"]);

    // Make a new commit to verify working directory is clean
    file.insert_at(3, crate::lines!["new line"]);
    repo.stage_all_and_commit("After reset").unwrap();
    file = repo.filename("test.txt");
    file.assert_lines_and_blame(crate::lines!["line 1", "line 2", "line 3", "new line",]);
    }
}

crate::subdir_test_variants! {
    fn reset_soft() {
        // Test git reset --soft: should preserve AI authorship from unwound commits
    let repo = TestRepo::new();

    // Create subdirectory structure
    let working_dir = repo.path().join("src");
    fs::create_dir_all(&working_dir).unwrap();

    let mut file = repo.filename("test.txt");

    // Create initial commit
    file.set_contents(crate::lines!["line 1", "line 2"]);
    let first_commit = repo.stage_all_and_commit("First commit").unwrap();

    // Make second commit with AI changes
    file.insert_at(2, crate::lines!["// AI addition".ai()]);
    repo.stage_all_and_commit("Second commit").unwrap();

    // Reset --soft to first commit
    repo.git_from_working_dir(&working_dir, &["reset", "--soft", &first_commit.commit_sha])
        .expect("reset --soft should succeed");

    // After soft reset, changes should be staged, and when we commit them
    // they should retain AI authorship
    let new_commit = repo.commit("Re-commit AI changes").unwrap();

    // Verify AI authorship was preserved in the commit
    assert!(
        !new_commit.authorship_log.attestations.is_empty(),
        "AI authorship should be preserved after reset --soft"
    );

    // Verify blame shows AI authorship
    file = repo.filename("test.txt");
    file.assert_lines_and_blame(crate::lines![
        "line 1".human(),
        "line 2".ai(),
        "// AI addition".ai(),
    ]);
    }
}

crate::subdir_test_variants! {
    fn reset_mixed() {
        // Test git reset --mixed (default): working directory preserved
    let repo = TestRepo::new();

    // Create subdirectory structure
    let working_dir = repo.path().join("src").join("lib");
    fs::create_dir_all(&working_dir).unwrap();

    let mut file = repo.filename("main.rs");

    // Create initial commit
    file.set_contents(crate::lines!["fn main() {", "}"]);
    let first_commit = repo.stage_all_and_commit("Initial commit").unwrap();

    // Make second commit with AI changes
    file.insert_at(1, crate::lines!["    // AI: Added logging".ai()]);
    file.insert_at(2, crate::lines!["    println!(\"Hello\");".ai()]);

    repo.stage_all_and_commit("Add logging").unwrap();

    // Reset --mixed to first commit
    repo.git_from_working_dir(&working_dir, &["reset", "--mixed", &first_commit.commit_sha])
        .expect("reset --mixed should succeed");

    // After mixed reset, changes should be unstaged but in working directory
    // Stage and commit them to verify AI authorship was preserved
    let new_commit = repo.stage_all_and_commit("Re-commit after reset").unwrap();

    // Verify AI authorship was preserved
    assert!(
        !new_commit.authorship_log.attestations.is_empty(),
        "AI authorship should be preserved after reset --mixed"
    );

    file = repo.filename("main.rs");
    file.assert_lines_and_blame(crate::lines![
        "fn main() {".human(),
        "    // AI: Added logging".ai(),
        "    println!(\"Hello\");".ai(),
        "}".human(),
    ]);
    }
}

crate::subdir_test_variants! {
    fn reset_multiple_commits() {
        // Test git reset with multiple commits unwound: should preserve all AI authorship
    let repo = TestRepo::new();

    // Create subdirectory structure
    let working_dir = repo.path().join("lib");
    fs::create_dir_all(&working_dir).unwrap();

    let mut file = repo.filename("code.js");

    // Create base commit
    file.set_contents(crate::lines!["// Base", ""]);
    let base_commit = repo.stage_all_and_commit("Base").unwrap();

    // Second commit - AI adds feature
    file.insert_at(1, crate::lines!["// AI feature 1".ai()]);
    repo.stage_all_and_commit("Feature 1").unwrap();

    // Third commit - AI adds another feature
    file.insert_at(2, crate::lines!["// AI feature 2".ai()]);
    repo.stage_all_and_commit("Feature 2").unwrap();

    // Reset --soft to base
    repo.git_from_working_dir(&working_dir, &["reset", "--soft", &base_commit.commit_sha])
        .expect("reset --soft should succeed");

    // Commit and verify both AI features are attributed correctly
    let new_commit = repo.commit("Re-commit features").unwrap();

    assert!(
        !new_commit.authorship_log.attestations.is_empty(),
        "AI authorship should be preserved for all unwound commits"
    );

    file = repo.filename("code.js");
    file.assert_lines_and_blame(crate::lines![
        "// Base".human(),
        "// AI feature 1".ai(),
        "// AI feature 2".ai(),
    ]);
    }
}

crate::subdir_test_variants! {
    fn reset_with_pathspec() {
        // Test git reset with pathspecs: should preserve AI authorship for non-reset files
    let repo = TestRepo::new();

    // Create subdirectory structure
    let working_dir = repo.path().join("src").join("components");
    fs::create_dir_all(&working_dir).unwrap();

    let mut file1 = repo.filename("file1.txt");
    let mut file2 = repo.filename("file2.txt");

    // Create initial commit with multiple files
    file1.set_contents(crate::lines!["content 1", ""]);
    file2.set_contents(crate::lines!["content 2", ""]);
    let first_commit = repo.stage_all_and_commit("Initial").unwrap();

    // Commit AI changes to both files
    file1.insert_at(1, crate::lines!["// AI change 1".ai()]);
    file2.insert_at(1, crate::lines!["// AI change 2".ai()]);
    repo.stage_all_and_commit("AI changes both files").unwrap();

    // Make uncommitted changes to both files
    file1.insert_at(2, crate::lines!["// More AI".ai()]);
    file2.insert_at(2, crate::lines!["// More AI".ai()]);

    // Now reset only file1.txt to first commit with pathspec
    repo.git_from_working_dir(&working_dir, &["reset", &first_commit.commit_sha, "--", "file1.txt"])
        .expect("reset with pathspec should succeed");

    // Stage all and commit to verify file2 still has AI attribution
    let new_commit = repo.stage_all_and_commit("After pathspec reset").unwrap();

    assert!(
        !new_commit.authorship_log.attestations.is_empty(),
        "AI authorship should be preserved for file2 after pathspec reset"
    );

    file2 = repo.filename("file2.txt");
    // file2 should still have AI changes
    file2.assert_lines_and_blame(crate::lines![
        "content 2".human(),
        "// AI change 2".ai(),
        "// More AI".ai(),
    ]);
    }
}

crate::subdir_test_variants! {
    fn reset_mixed_ai_human_changes() {
        // Test git reset with AI and human mixed changes: should preserve all authorship
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src");
        fs::create_dir_all(&working_dir).unwrap();

        let mut file = repo.filename("main.rs");

        // Base commit has known-human wrapper context.
        file.set_contents(crate::lines!["fn main() {", "}"]);
        let base = repo.stage_all_and_commit("Base").unwrap();

        // AI commit
        file.insert_at(1, crate::lines!["    // AI".ai()]);
        repo.stage_all_and_commit("AI changes").unwrap();

        // Human commit
        file.insert_at(2, crate::lines!["    // Human"]);
        repo.stage_all_and_commit("Human changes").unwrap();

        // Reset to base
        repo.git_from_working_dir(&working_dir, &["reset", "--soft", &base.commit_sha])
            .expect("reset --soft should succeed");

        // Commit and verify authorship
        let new_commit = repo.commit("Re-commit mixed changes").unwrap();

        assert!(
            !new_commit.authorship_log.attestations.is_empty(),
            "AI authorship should be preserved in mixed AI/human changes"
        );

        file = repo.filename("main.rs");
        file.assert_lines_and_blame(crate::lines![
            "fn main() {".human(),
            "    // AI".ai(),
            "    // Human".human(),
            "}".human(),
        ]);
    }
}

crate::subdir_test_variants! {
    fn reset_with_new_files() {
        // Test git reset with new files added in unwound commit: should preserve AI authorship
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src").join("lib");
        fs::create_dir_all(&working_dir).unwrap();

        let mut old_file = repo.filename("old.txt");

        // Base commit
        old_file.set_contents(crate::lines!["existing"]);
        let base = repo.stage_all_and_commit("Base").unwrap();

        // Add new file in second commit
        let mut new_file = repo.filename("new.txt");
        new_file.set_contents(crate::lines!["// AI created this".ai()]);
        repo.stage_all_and_commit("Add new file").unwrap();

        // Reset to base
        repo.git_from_working_dir(&working_dir, &["reset", "--soft", &base.commit_sha])
            .expect("reset --soft should succeed");

        // Commit and verify new file has AI authorship
        let new_commit = repo.commit("Re-commit with new file").unwrap();

        assert!(
            !new_commit.authorship_log.attestations.is_empty(),
            "AI authorship should be preserved for new file after reset"
        );

        new_file = repo.filename("new.txt");
        new_file.assert_lines_and_blame(crate::lines!["// AI created this".ai()]);
    }
}

crate::subdir_test_variants! {
    fn reset_nested() {
        // Test git reset when run from a deeply nested subdirectory
        let repo = TestRepo::new();

        // Create deeply nested subdirectory structure
        let working_dir = repo.path().join("a").join("b").join("c");
        fs::create_dir_all(&working_dir).unwrap();

        let mut file = repo.filename("test.txt");

        // Create base commit
        file.set_contents(crate::lines!["base content"]);
        let base_commit = repo.stage_all_and_commit("Base").unwrap();

        // Second commit with AI changes
        file.insert_at(1, crate::lines!["// AI feature".ai()]);
        repo.stage_all_and_commit("AI feature").unwrap();

        // Reset --soft to base
        repo.git_from_working_dir(&working_dir, &["reset", "--soft", &base_commit.commit_sha])
            .expect("reset --soft should succeed");

        // Commit and verify AI authorship preserved
        let new_commit = repo.commit("Re-commit after reset").unwrap();

        assert!(
            !new_commit.authorship_log.attestations.is_empty(),
            "AI authorship should be preserved after reset"
        );

        file = repo.filename("test.txt");
        file.assert_lines_and_blame(crate::lines![
            "base content".ai(),
            "// AI feature".ai(),
        ]);
    }
}

crate::subdir_test_variants! {
    fn reset_to_same_commit() {
        // Test git reset to same commit: should preserve uncommitted AI changes
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src");
        fs::create_dir_all(&working_dir).unwrap();

        let mut file = repo.filename("test.txt");

        // Create commit with AI changes
        file.set_contents(crate::lines!["line 1", "// AI line".ai(), ""]);
        repo.stage_all_and_commit("Commit").unwrap();

        // Make uncommitted changes
        file.insert_at(2, crate::lines!["// More changes".ai()]);

        // Reset to same commit (HEAD)
        repo.git_from_working_dir(&working_dir, &["reset", "HEAD"])
            .expect("reset should succeed");

        // Uncommitted AI changes should still be preserved in working directory
        // Commit them to verify authorship
        let new_commit = repo.stage_all_and_commit("After reset to HEAD").unwrap();

        assert!(
            !new_commit.authorship_log.attestations.is_empty(),
            "AI authorship should be preserved for uncommitted changes after reset"
        );

        file = repo.filename("test.txt");
        file.assert_lines_and_blame(crate::lines![
            "line 1".human(),
            "// AI line".ai(),
            "// More changes".ai(),
        ]);
    }
}

crate::subdir_test_variants! {
    fn reset_forward() {
        // Test git reset forward (to descendant): should restore commit state
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src").join("lib");
        fs::create_dir_all(&working_dir).unwrap();

        let mut file = repo.filename("test.txt");

        // Create two commits
        file.set_contents(crate::lines!["v1"]);
        let first_commit = repo.stage_all_and_commit("First").unwrap();

        file.insert_at(1, crate::lines!["v2".ai()]);
        let second_commit = repo.stage_all_and_commit("Second").unwrap();

        // Reset back to first (--hard discards all changes)
        repo.git_from_working_dir(&working_dir, &["reset", "--hard", &first_commit.commit_sha])
            .expect("reset --hard should succeed");

        // Verify file is back to v1 only
        file = repo.filename("test.txt");
        file.assert_lines_and_blame(crate::lines!["v1".human()]);

        // Now reset forward to second with --hard to restore the working tree
        repo.git_from_working_dir(&working_dir, &["reset", "--hard", &second_commit.commit_sha])
            .expect("reset --hard forward should succeed");

        // File should now match second commit
        file = repo.filename("test.txt");
        file.assert_lines_and_blame(crate::lines!["v1".ai(), "v2".ai()]);
    }
}

crate::subdir_test_variants! {
    fn reset_mixed_pathspec() {
        // Test git reset --mixed with pathspec: should preserve AI authorship for non-reset files
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src").join("components");
        fs::create_dir_all(&working_dir).unwrap();

        let mut file1 = repo.filename("file1.txt");
        let mut file2 = repo.filename("file2.txt");

        // Base commit with two files
        file1.set_contents(crate::lines!["base content 1", ""]);
        file2.set_contents(crate::lines!["base content 2", ""]);
        let base_commit = repo.stage_all_and_commit("Base commit").unwrap();

        // Second commit: AI modifies both files
        file1.insert_at(1, crate::lines!["// AI change to file1".ai()]);
        file2.insert_at(1, crate::lines!["// AI change to file2".ai()]);
        let _second_commit = repo.stage_all_and_commit("AI modifies both files").unwrap();

        // Make uncommitted changes to file2 (not file1)
        file2.insert_at(2, crate::lines!["// More AI changes".ai()]);

        // Get current branch for HEAD check
        let current_head_before = repo.current_branch();

        // Reset only file1.txt to base commit with pathspec
        // This should preserve uncommitted changes for file2.txt
        repo.git_from_working_dir(&working_dir, &["reset", &base_commit.commit_sha, "--", "file1.txt"])
            .expect("reset with pathspec should succeed");

        // HEAD should not move with pathspec reset
        let current_head_after = repo.current_branch();
        assert_eq!(
            current_head_before, current_head_after,
            "HEAD should not move with pathspec reset"
        );

        // Commit and verify file2 still has AI authorship
        let new_commit = repo.stage_all_and_commit("After pathspec reset").unwrap();

        assert!(
            !new_commit.authorship_log.attestations.is_empty(),
            "AI authorship should be preserved for file2 after pathspec reset"
        );

        file2 = repo.filename("file2.txt");
        file2.assert_lines_and_blame(crate::lines![
            "base content 2".human(),
            "// AI change to file2".ai(),
            "// More AI changes".ai(),
        ]);
    }
}

crate::subdir_test_variants! {
    fn cherry_pick_single_commit() {
        // Test cherry-picking a single AI-authored commit
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src").join("lib");
        fs::create_dir_all(&working_dir).unwrap();

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
        repo.git_from_working_dir(&working_dir, &["cherry-pick", &feature_commit])
            .expect("Cherry-pick should succeed");

        // Verify final file state - hooks should have preserved AI authorship
        file.assert_lines_and_blame(crate::lines!["Initial content".ai(), "AI feature line".ai(),]);
    }
}

crate::subdir_test_variants! {
    fn cherry_pick_multiple_commits() {
        // Test cherry-picking multiple commits in sequence
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src");
        fs::create_dir_all(&working_dir).unwrap();

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
        repo.git_from_working_dir(&working_dir, &["cherry-pick", &commit1, &commit2, &commit3])
            .expect("Cherry-pick multiple commits should succeed");

        // Verify final file state - hooks should have preserved AI authorship
        file.assert_lines_and_blame(crate::lines![
            "Line 1".human(),
            "AI line 2".ai(),
            "AI line 3".ai(),
            "AI line 4".ai(),
        ]);
    }
}

crate::subdir_test_variants! {
    fn cherry_pick_with_conflict() {
        // Test cherry-pick with conflicts and --continue
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src").join("lib");
        fs::create_dir_all(&working_dir).unwrap();

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
        let cherry_pick_result = repo.git_from_working_dir(&working_dir, &["cherry-pick", &feature_commit]);
        assert!(cherry_pick_result.is_err(), "Should have conflict");

        // Resolve conflict by choosing the AI version
        fs::write(
            repo.path().join("file.txt"),
            "Line 1\nAI_FEATURE_VERSION\nLine 3",
        )
        .unwrap();
        repo.git(&["add", "file.txt"]).unwrap();

        // Continue cherry-pick (need GIT_EDITOR for commit message)
        repo.git_with_env(
            &["cherry-pick", "--continue"],
            &[("GIT_EDITOR", "true")],
            Some(&working_dir)
        )
        .expect("Cherry-pick continue should succeed");

        // Verify final file state - hooks should have preserved AI authorship
        file.assert_lines_and_blame(crate::lines![
            "Line 1".human(),
            "AI_FEATURE_VERSION".ai(),
            "Line 3".human(),
        ]);
    }
}

crate::subdir_test_variants! {
    fn cherry_pick_abort() {
        // Test cherry-pick --abort
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src");
        fs::create_dir_all(&working_dir).unwrap();

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
        file.assert_lines_and_blame(crate::lines!["Line 1".human(), "AI modification of line 2".ai(),]);

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
        let cherry_pick_result = repo.git_from_working_dir(&working_dir, &["cherry-pick", &feature_commit]);
        assert!(cherry_pick_result.is_err(), "Should have conflict");

        // Abort the cherry-pick
        repo.git_from_working_dir(&working_dir, &["cherry-pick", "--abort"])
            .expect("Cherry-pick abort should succeed");

        // Verify HEAD is back to before the cherry-pick
        let current_head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        assert_ne!(current_head, initial_head); // Different because we made the "Human change" commit

        // Verify final file state (should have human's version)
        file.assert_lines_and_blame(crate::lines![
            "Line 1".human(),
            "Human modification of line 2".human(),
        ]);
    }
}

crate::subdir_test_variants! {
    fn cherry_pick_no_ai_authorship() {
        // Test cherry-picking from branch without AI authorship
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src").join("lib");
        fs::create_dir_all(&working_dir).unwrap();

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
        repo.git_from_working_dir(&working_dir, &["cherry-pick", &feature_commit])
            .expect("Cherry-pick should succeed");

        // Verify final file state - should have no AI authorship
        file.assert_lines_and_blame(crate::lines!["Line 1".human(), "Human line 2".human(),]);
    }
}

crate::subdir_test_variants! {
    fn cherry_pick_multiple_ai_sessions() {
        // Test cherry-pick preserving multiple AI sessions from different commits
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src");
        fs::create_dir_all(&working_dir).unwrap();

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
        repo.git_from_working_dir(&working_dir, &["cherry-pick", &commit1, &commit2])
            .expect("Cherry-pick multiple AI sessions should succeed");

        // Verify final file state - hooks should have preserved AI authorship
        file.assert_lines_and_blame(crate::lines![
            "fn main() {".ai(),
            "    println!(\"Starting\");".ai(),
            "    // TODO: Add error handling".ai(),
            "}".human(),
        ]);
    }
}

crate::subdir_test_variants! {
    fn amend_add_lines() {
        // Test amending a commit by adding AI-authored lines
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src").join("lib");
        fs::create_dir_all(&working_dir).unwrap();

        let mut file = repo.filename("test.txt");

        // Initial file with human content
        file.set_contents(crate::lines!["line 1", "line 2", "line 3", "line 4", "line 5"]);

        repo.git(&["add", "-A"]).unwrap();

        repo.commit("Initial commit").unwrap();

        // AI adds lines at the top
        file.insert_at(
            0,
            crate::lines!["// AI added line 1".ai(), "// AI added line 2".ai()],
        );

        // Amend the commit (WITHOUT staging the AI lines)
        repo.git_from_working_dir(&working_dir, &["commit", "--amend", "-m", "Initial commit (amended)"])
            .expect("Amend should succeed");

        // Now stage and commit the AI lines
        repo.stage_all_and_commit("Add AI lines").unwrap();

        // Verify AI authorship is preserved after the second commit
        file.assert_lines_and_blame(crate::lines![
            "// AI added line 1".ai(),
            "// AI added line 2".ai(),
            "line 1".human(),
            "line 2".human(),
            "line 3".human(),
            "line 4".human(),
            "line 5".human()
        ]);
    }
}

crate::subdir_test_variants! {
    fn amend_add_lines_in_middle() {
        // Test amending a commit by adding AI-authored lines in the middle
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src");
        fs::create_dir_all(&working_dir).unwrap();

        let mut file = repo.filename("test.txt");

        // Initial file with human content
        file.set_contents(crate::lines!["line 1", "line 2", "line 3", "line 4", "line 5"]);
        repo.stage_all_and_commit("Initial commit").unwrap();

        // AI adds lines in the middle
        file.insert_at(
            2,
            crate::lines!["// AI inserted line 1".ai(), "// AI inserted line 2".ai()],
        );

        // Amend the commit
        repo.git(&["add", "-A"]).unwrap();
        repo.git_from_working_dir(&working_dir, &["commit", "--amend", "-m", "Initial commit (amended)"])
            .expect("Amend should succeed");

        // Verify AI authorship is preserved
        file.assert_lines_and_blame(crate::lines![
            "line 1".human(),
            "line 2".human(),
            "// AI inserted line 1".ai(),
            "// AI inserted line 2".ai(),
            "line 3".human(),
            "line 4".human(),
            "line 5".human()
        ]);
    }
}

crate::subdir_test_variants! {
    fn amend_multiple_changes() {
        // Test amending with multiple AI changes
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src").join("lib");
        fs::create_dir_all(&working_dir).unwrap();

        let mut file = repo.filename("code.js");

        // Initial file with AI content
        file.set_contents(crate::lines![
            "function example() {".ai(),
            "  return 42;".ai(),
            "}".ai()
        ]);
        repo.stage_all_and_commit("Add example function").unwrap();

        // AI adds header comment
        file.insert_at(0, crate::lines!["// Header comment".ai()]);
        // After inserting at 0, the file now has 4 lines

        // AI adds documentation in middle (after line 2: "function example() {")
        file.insert_at(2, crate::lines!["  // Added documentation".ai()]);
        // After inserting at 2, the file now has 5 lines

        // AI adds footer at bottom (at the end after "}")
        file.insert_at(5, crate::lines!["// Footer".ai()]);

        // Amend the commit
        repo.git(&["add", "-A"]).unwrap();
        repo.git_from_working_dir(&working_dir, &["commit", "--amend", "-m", "Add example function (amended)"])
            .expect("Amend should succeed");

        // Verify all AI authorship is preserved
        file.assert_lines_and_blame(crate::lines![
            "// Header comment".ai(),
            "function example() {".ai(),
            "  // Added documentation".ai(),
            "  return 42;".ai(),
            "}".ai(),
            "// Footer".ai()
        ]);
    }
}

crate::subdir_test_variants! {
    fn amend_with_unstaged_ai_code() {
        // Test amending with unstaged AI code in other file
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src");
        fs::create_dir_all(&working_dir).unwrap();

        // Create initial commit with fileA
        let mut file_a = repo.filename("fileA.txt");
        file_a.set_contents(crate::lines!["fileA line 1", "fileA line 2"]);
        repo.stage_all_and_commit("Initial commit").unwrap();

        // Create fileB with AI code but DON'T stage it yet
        let mut file_b = repo.filename("fileB.txt");
        file_b.set_contents_no_stage(crate::lines![
            "// AI code in fileB".ai(),
            "function foo() {".ai(),
            "  return 'bar';".ai(),
            "}".ai()
        ]);

        // Modify fileA and amend the previous commit (fileB stays unstaged in working tree)
        file_a.insert_at(2, crate::lines!["fileA line 3"]);
        repo.git(&["add", "fileA.txt"]).unwrap();
        repo.git_from_working_dir(&working_dir, &["commit", "--amend", "-m", "Initial commit (amended)"])
            .expect("Amend should succeed");

        // Now stage and commit fileB in a new commit
        repo.stage_all_and_commit("Add fileB").unwrap();

        // Verify fileB has AI authorship
        file_b.assert_lines_and_blame(crate::lines![
            "// AI code in fileB".ai(),
            "function foo() {".ai(),
            "  return 'bar';".ai(),
            "}".ai()
        ]);
    }
}

crate::subdir_test_variants! {
    fn amend_nested() {
        // Test amending a commit when run from a deeply nested subdirectory
        let repo = TestRepo::new();

        // Create deeply nested subdirectory structure
        let working_dir = repo.path().join("a").join("b").join("c");
        fs::create_dir_all(&working_dir).unwrap();

        let mut file = repo.filename("test.txt");

        // Initial file with human content
        file.set_contents(crate::lines!["line 1", "line 2", "line 3"]);
        repo.stage_all_and_commit("Initial commit").unwrap();

        // AI adds lines at the bottom
        file.insert_at(
            3,
            crate::lines!["// AI appended line 1".ai(), "// AI appended line 2".ai()],
        );

        // Amend the commit
        repo.git(&["add", "-A"]).unwrap();
        repo.git_from_working_dir(&working_dir, &["commit", "--amend", "-m", "Initial commit (amended)"])
            .expect("Amend should succeed");

        // Verify AI authorship is preserved
        file.assert_lines_and_blame(crate::lines![
            "line 1".human(),
            "line 2".human(),
            "line 3".ai(),
            "// AI appended line 1".ai(),
            "// AI appended line 2".ai()
        ]);
    }
}

crate::subdir_test_variants! {
    fn amend_preserves_unstaged_ai_attribution() {
        // Test that unstaged AI code in the tree is attributed after amending HEAD
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src").join("components");
        fs::create_dir_all(&working_dir).unwrap();

        // Create initial commit with fileA
        let mut file_a = repo.filename("fileA.txt");
        file_a.set_contents(crate::lines!["original content"]);
        repo.stage_all_and_commit("Initial commit").unwrap();

        // Stage changes to fileA
        file_a.insert_at(1, crate::lines!["staged addition"]);
        repo.git(&["add", "fileA.txt"]).unwrap();

        // Create fileB with unstaged AI code
        let mut file_b = repo.filename("fileB.txt");
        file_b.set_contents_no_stage(crate::lines![
            "// Unstaged AI line 1".ai(),
            "// Unstaged AI line 2".ai(),
            "// Unstaged AI line 3".ai()
        ]);

        // Amend HEAD with fileA (fileB remains unstaged)
        repo.git_from_working_dir(&working_dir, &["commit", "--amend", "-m", "Amended commit"])
            .expect("Amend should succeed");

        // Verify that fileB's AI attribution was saved in INITIAL attributions
        let initial = repo.current_working_logs().read_initial_attributions();
        assert!(
            initial.files.contains_key("fileB.txt"),
            "fileB.txt should be in initial attributions"
        );
        let file_b_attrs = &initial.files["fileB.txt"];
        assert_eq!(
            file_b_attrs.len(),
            1,
            "fileB should have 1 attribution range"
        );
        assert_eq!(file_b_attrs[0].start_line, 1);
        assert_eq!(file_b_attrs[0].end_line, 3);

        // Now stage and commit fileB
        repo.stage_all_and_commit("Add fileB").unwrap();

        // Verify fileB retains AI authorship
        file_b.assert_lines_and_blame(crate::lines![
            "// Unstaged AI line 1".ai(),
            "// Unstaged AI line 2".ai(),
            "// Unstaged AI line 3".ai()
        ]);
    }
}

crate::subdir_test_variants! {
    fn merge_with_ai_contributions() {
        // Test merge with AI contributions
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src").join("lib");
        fs::create_dir_all(&working_dir).unwrap();

        let mut file = repo.filename("test.txt");

        // Create base file and initial commit
        file.set_contents(crate::lines!["Base line 1", "Base line 2", "Base line 3"]);
        repo.stage_all_and_commit("Initial commit").unwrap();

        // Save the default branch name before creating feature branch
        let default_branch = repo.current_branch();

        // Create a feature branch
        repo.git(&["checkout", "-b", "feature"]).unwrap();

        // Make AI changes on feature branch (insert after line 3)
        file.insert_at(3, crate::lines!["FEATURE LINE 1".ai(), "FEATURE LINE 2".ai()]);
        repo.stage_all_and_commit("feature branch changes").unwrap();

        // Switch back to default branch and make human changes
        repo.git(&["checkout", &default_branch]).unwrap();
        file = repo.filename("test.txt"); // Reload file from default branch
        // Insert at beginning to avoid conflict with feature branch
        file.insert_at(0, crate::lines!["MAIN LINE 1", "MAIN LINE 2"]);
        repo.stage_all_and_commit("main branch changes").unwrap();

        // Merge feature branch into default branch (should not conflict)
        repo.git_from_working_dir(&working_dir, &["merge", "feature", "-m", "merge feature into main"])
            .expect("Merge should succeed");

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
}

crate::subdir_test_variants! {
    fn merge_with_conflicts() {
        // Test merge with conflicts
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src");
        fs::create_dir_all(&working_dir).unwrap();

        let mut file = repo.filename("conflict.txt");

        // Create base file and initial commit
        file.set_contents(crate::lines!["line 1", "line 2", "line 3"]);
        repo.stage_all_and_commit("Initial commit").unwrap();

        let default_branch = repo.current_branch();

        // Create feature branch with AI changes
        repo.git(&["checkout", "-b", "feature"]).unwrap();
        file.replace_at(1, "AI_FEATURE_VERSION".ai());
        repo.stage_all_and_commit("AI feature").unwrap();

        // Switch back to default branch and make conflicting change
        repo.git(&["checkout", &default_branch]).unwrap();
        file.replace_at(1, "MAIN_BRANCH_VERSION".human());
        repo.stage_all_and_commit("Human change").unwrap();

        // Try to merge (should conflict)
        let merge_result = repo.git_from_working_dir(&working_dir, &["merge", "feature", "-m", "Merge feature"]);
        assert!(merge_result.is_err(), "Should have conflict");

        // Resolve conflict by choosing the AI version
        fs::write(
            repo.path().join("conflict.txt"),
            "line 1\nAI_FEATURE_VERSION\nline 3",
        )
        .unwrap();
        repo.git(&["add", "conflict.txt"]).unwrap();

        // Continue merge (need GIT_EDITOR for commit message)
        repo.git_with_env(
            &["commit", "--no-edit"],
            &[("GIT_EDITOR", "true")],
            Some(&working_dir)
        )
        .expect("Merge continue should succeed");

        // Verify final file state - hooks should have preserved AI authorship
        file.assert_lines_and_blame(crate::lines![
            "line 1".human(),
            "AI_FEATURE_VERSION".ai(),
            "line 3".human(),
        ]);
    }
}

crate::subdir_test_variants! {
    fn squash_merge() {
        // Test merge --squash with AI contributions
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src").join("lib");
        fs::create_dir_all(&working_dir).unwrap();

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
        repo.stage_all_and_commit("Add AI feature").unwrap();

        // Add human changes on feature branch
        file.insert_at(4, crate::lines!["// Human refinement"]);
        repo.stage_all_and_commit("Human refinement").unwrap();

        // Go back to master and squash merge
        repo.git(&["checkout", &default_branch]).unwrap();
        repo.git_from_working_dir(&working_dir, &["merge", "--squash", "feature"])
            .expect("Squash merge should succeed");
        repo.commit("Squashed feature").unwrap();

        // Verify AI attribution is preserved
        file.assert_lines_and_blame(crate::lines![
            "line 1".human(),
            "line 2".human(),
            "line 3".human(),
            "// AI added feature".ai(),
            "// Human refinement".human()
        ]);
    }
}

crate::subdir_test_variants! {
    fn stash_pop_with_ai_attribution() {
        // Test stash pop with AI attribution
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src");
        fs::create_dir_all(&working_dir).unwrap();

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
        repo.git_from_working_dir(&working_dir, &["stash", "push", "-m", "test stash"])
            .expect("stash should succeed");

        // Verify file is gone
        assert!(repo.read_file("example.txt").is_none());

        // Pop the stash
        repo.git_from_working_dir(&working_dir, &["stash", "pop"])
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
}

crate::subdir_test_variants! {
    fn stash_apply_with_ai_attribution() {
        // Test stash apply with AI attribution
        let repo = TestRepo::new();

        // Create subdirectory structure
        let working_dir = repo.path().join("src").join("lib");
        fs::create_dir_all(&working_dir).unwrap();

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
        repo.git_from_working_dir(&working_dir, &["stash"])
            .expect("stash should succeed");

        // Apply (not pop) the stash
        repo.git_from_working_dir(&working_dir, &["stash", "apply"])
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
}

crate::subdir_test_variants! {
    fn stash_nested() {
        // Test stash operations when run from a deeply nested subdirectory
        let repo = TestRepo::new();

        // Create deeply nested subdirectory structure
        let working_dir = repo.path().join("a").join("b").join("c");
        fs::create_dir_all(&working_dir).unwrap();

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
        repo.git_from_working_dir(&working_dir, &["stash", "push", "-m", "test stash"])
            .expect("stash should succeed");

        // Verify file is gone
        assert!(repo.read_file("example.txt").is_none());

        // Pop the stash
        repo.git_from_working_dir(&working_dir, &["stash", "pop"])
            .expect("stash pop should succeed");

        // Verify file is back
        assert!(repo.read_file("example.txt").is_some());

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
}
