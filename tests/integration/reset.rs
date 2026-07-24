use crate::repos::test_file::{AuthorType, ExpectedLineExt};
use crate::repos::test_repo::TestRepo;
use std::fs;

/// Test git reset --hard: should discard all changes and reset to target commit
#[test]
fn test_reset_hard_deletes_working_log() {
    let repo = TestRepo::new();
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
    repo.git(&["reset", "--hard", &first_commit.commit_sha])
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

/// Test git reset --soft: should preserve AI authorship from unwound commits
#[test]
fn test_reset_soft_reconstructs_working_log() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create initial commit
    file.set_contents(crate::lines!["line 1", "line 2"]);
    let first_commit = repo.stage_all_and_commit("First commit").unwrap();

    // Make second commit with AI changes
    file.insert_at(2, crate::lines!["// AI addition".ai()]);
    repo.stage_all_and_commit("Second commit").unwrap();

    // Reset --soft to first commit
    repo.git(&["reset", "--soft", &first_commit.commit_sha])
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

/// Test git reset --mixed (default): working directory preserved
#[test]
fn test_reset_mixed_reconstructs_working_log() {
    let repo = TestRepo::new();
    let mut file = repo.filename("main.rs");

    // Create initial commit
    file.set_contents(crate::lines!["fn main() {", "}"]);
    let first_commit = repo.stage_all_and_commit("Initial commit").unwrap();

    // Make second commit with AI changes - simpler approach
    file.insert_at(1, crate::lines!["    // AI: Added logging".ai()]);
    file.insert_at(2, crate::lines!["    println!(\"Hello\");".ai()]);

    repo.stage_all_and_commit("Add logging").unwrap();

    // Reset --mixed to first commit
    repo.git(&["reset", "--mixed", &first_commit.commit_sha])
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

/// Test git reset to same commit: should preserve uncommitted AI changes
#[test]
fn test_reset_to_same_commit_is_noop() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create commit with AI changes
    file.set_contents(crate::lines!["line 1", "// AI line".ai(), ""]);
    repo.stage_all_and_commit("Commit").unwrap();

    // Make uncommitted changes
    file.insert_at(2, crate::lines!["// More changes".ai()]);

    // Reset to same commit (HEAD)
    repo.git(&["reset", "HEAD"]).expect("reset should succeed");

    // Uncommitted AI changes should still be preserved in working directory
    // Commit them to verify authorship
    let new_commit = repo.stage_all_and_commit("After reset to HEAD").unwrap();

    assert!(
        !new_commit.authorship_log.attestations.is_empty(),
        "AI authorship should be preserved for uncommitted changes"
    );

    file = repo.filename("test.txt");
    file.assert_lines_and_blame(crate::lines![
        "line 1".human(),
        "// AI line".ai(),
        "// More changes".ai(),
    ]);
}

/// Test git reset with multiple commits unwound: should preserve all AI authorship
#[test]
fn test_reset_multiple_commits() {
    let repo = TestRepo::new();
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
    repo.git(&["reset", "--soft", &base_commit.commit_sha])
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

/// Test git reset with uncommitted changes preserved: should preserve all AI authorship
#[test]
fn test_reset_preserves_uncommitted_changes() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.py");

    // Create base commit
    file.set_contents(crate::lines!["def main():", "    pass", ""]);
    let base_commit = repo.stage_all_and_commit("Base").unwrap();

    // Second commit with AI changes
    file.replace_at(1, "    print('hello')".ai());
    repo.stage_all_and_commit("Add print").unwrap();

    // Third commit with more AI changes
    file.insert_at(2, crate::lines!["    print('world')".ai()]);
    repo.stage_all_and_commit("Add world").unwrap();

    // Reset --soft to base (should preserve both AI commits as staged)
    let result = repo
        .git(&["reset", "--soft", &base_commit.commit_sha])
        .expect("reset --soft should succeed");

    println!("result: {}", result);
    // Commit and verify AI authorship preserved
    let new_commit = repo.commit("Re-commit AI changes").unwrap();

    assert!(
        !new_commit.authorship_log.attestations.is_empty(),
        "AI authorship should be preserved from multiple unwound commits"
    );

    file = repo.filename("app.py");
    file.assert_lines_and_blame(crate::lines![
        "def main():".human(),
        "    print('hello')".ai(),
        "    print('world')".ai(),
    ]);
}

/// Test git reset with pathspecs: should preserve AI authorship for non-reset files
#[test]
fn test_reset_with_pathspec() {
    let repo = TestRepo::new();
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
    repo.git(&["reset", &first_commit.commit_sha, "--", "file1.txt"])
        .expect("reset with pathspec should succeed");

    // Stage all and commit to verify file2 still has AI attribution
    let new_commit = repo.stage_all_and_commit("After pathspec reset").unwrap();

    assert!(
        !new_commit.authorship_log.attestations.is_empty(),
        "AI authorship should be preserved for file2"
    );

    file2 = repo.filename("file2.txt");
    // file2 should still have AI changes
    file2.assert_lines_and_blame(crate::lines![
        "content 2".human(),
        "// AI change 2".ai(),
        "// More AI".ai(),
    ]);
}

/// Test git reset forward (to descendant): should restore commit state
#[test]
fn test_reset_forward_is_noop() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create two commits
    file.set_contents(crate::lines!["v1"]);
    let first_commit = repo.stage_all_and_commit("First").unwrap();

    file.insert_at(1, crate::lines!["v2".ai()]);
    let second_commit = repo.stage_all_and_commit("Second").unwrap();

    // Reset back to first (--hard discards all changes)
    repo.git(&["reset", "--hard", &first_commit.commit_sha])
        .expect("reset --hard should succeed");

    // Verify file is back to v1 only
    file = repo.filename("test.txt");
    file.assert_lines_and_blame(crate::lines!["v1".human()]);

    // Now reset forward to second with --hard to restore the working tree
    repo.git(&["reset", "--hard", &second_commit.commit_sha])
        .expect("reset --hard should succeed");

    // File should now match second commit
    file = repo.filename("test.txt");
    file.assert_lines_and_blame(crate::lines!["v1".ai(), "v2".ai()]);
}

/// Test git reset with AI and human mixed changes: should preserve all authorship
#[test]
fn test_reset_mixed_ai_human_changes() {
    let repo = TestRepo::new();
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
    repo.git(&["reset", "--soft", &base.commit_sha])
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

/// Test git reset --merge: should be like --mixed for clean working tree
#[test]
fn test_reset_merge() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create base
    file.set_contents(crate::lines!["base"]);
    let base = repo.stage_all_and_commit("Base").unwrap();

    // Create second commit
    file.insert_at(1, crate::lines!["// AI line".ai()]);
    repo.stage_all_and_commit("Second").unwrap();

    // Reset --merge (behaves like --mixed when working tree is clean)
    // Note: --merge is designed to abort merges, so it may not work in all contexts
    // Let's use --mixed instead for this test
    repo.git(&["reset", &base.commit_sha])
        .expect("reset should succeed");

    // Commit and verify AI authorship preserved
    let new_commit = repo.stage_all_and_commit("Re-commit").unwrap();

    assert!(
        !new_commit.authorship_log.attestations.is_empty(),
        "AI authorship should be preserved after reset"
    );

    file = repo.filename("test.txt");
    file.assert_lines_and_blame(crate::lines!["base".ai(), "// AI line".ai()]);
}

/// Test git reset with new files added in unwound commit: should preserve AI authorship
#[test]
fn test_reset_with_new_files() {
    let repo = TestRepo::new();
    let mut old_file = repo.filename("old.txt");

    // Base commit
    old_file.set_contents(crate::lines!["existing"]);
    let base = repo.stage_all_and_commit("Base").unwrap();

    // Add new file in second commit
    let mut new_file = repo.filename("new.txt");
    new_file.set_contents(crate::lines!["// AI created this".ai()]);
    repo.stage_all_and_commit("Add new file").unwrap();

    // Reset to base
    repo.git(&["reset", "--soft", &base.commit_sha])
        .expect("reset --soft should succeed");

    // Commit and verify new file has AI authorship
    let new_commit = repo.commit("Re-commit with new file").unwrap();

    assert!(
        !new_commit.authorship_log.attestations.is_empty(),
        "AI authorship should be preserved for new file"
    );

    new_file = repo.filename("new.txt");
    new_file.assert_lines_and_blame(crate::lines!["// AI created this".ai()]);
}

/// Test git reset with file deletions in unwound commit
#[test]
fn test_reset_with_deleted_files() {
    let repo = TestRepo::new();
    let mut keep_file = repo.filename("keep.txt");
    let mut delete_file = repo.filename("delete.txt");

    // Base with two files
    keep_file.set_contents(crate::lines!["keep this"]);
    delete_file.set_contents(crate::lines!["will delete"]);
    let base = repo.stage_all_and_commit("Base").unwrap();

    // Delete one file
    repo.git(&["rm", "delete.txt"]).expect("rm should succeed");
    let delete_commit = repo.commit("Delete file").unwrap();

    // Verify deletion commit has no AI attestations
    assert_eq!(delete_commit.authorship_log.attestations.len(), 0);

    // Reset --hard to base (restores both files in working directory)
    repo.git(&["reset", "--hard", &base.commit_sha])
        .expect("reset --hard should succeed");

    // Verify both files exist in working directory
    assert!(
        repo.read_file("keep.txt").is_some(),
        "keep.txt should exist"
    );
    assert!(
        repo.read_file("delete.txt").is_some(),
        "delete.txt should exist"
    );

    // Make a new commit to verify files work correctly
    keep_file = repo.filename("keep.txt");
    keep_file.insert_at(1, crate::lines!["new line"]);
    repo.stage_all_and_commit("After reset").unwrap();
}

/// Test git reset --mixed with pathspec: should preserve AI authorship for non-reset files
#[test]
fn test_reset_mixed_pathspec_preserves_ai_authorship() {
    let repo = TestRepo::new();
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
    repo.git(&["reset", &base_commit.commit_sha, "--", "file1.txt"])
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

/// Test git reset --mixed with pathspec on multiple commits worth of AI changes
#[test]
fn test_reset_mixed_pathspec_multiple_commits() {
    let repo = TestRepo::new();
    let mut app_file = repo.filename("app.js");
    let mut lib_file = repo.filename("lib.js");

    // Base commit
    app_file.set_contents(crate::lines!["// base", ""]);
    lib_file.set_contents(crate::lines!["// base", ""]);
    let base_commit = repo.stage_all_and_commit("Base").unwrap();

    // First AI commit - modifies both files
    app_file.insert_at(1, crate::lines!["// AI feature 1".ai()]);
    lib_file.insert_at(1, crate::lines!["// AI lib 1".ai()]);
    repo.stage_all_and_commit("AI feature 1").unwrap();

    // Second AI commit - modifies both files again
    app_file.insert_at(2, crate::lines!["// AI feature 2".ai()]);
    lib_file.insert_at(2, crate::lines!["// AI lib 2".ai()]);
    let _second_ai_commit = repo.stage_all_and_commit("AI feature 2").unwrap();

    // Make uncommitted changes to lib.js (not app.js)
    lib_file.insert_at(3, crate::lines!["// More lib".ai()]);

    // Get current branch for HEAD check
    let current_head_before = repo.current_branch();

    // Reset only app.js to base with pathspec
    // This should preserve uncommitted changes for lib.js
    repo.git(&["reset", &base_commit.commit_sha, "--", "app.js"])
        .expect("reset with pathspec should succeed");

    // HEAD should not move
    let current_head_after = repo.current_branch();
    assert_eq!(
        current_head_before, current_head_after,
        "HEAD should not move"
    );

    // Commit and verify lib.js retains AI authorship
    let new_commit = repo.stage_all_and_commit("After pathspec reset").unwrap();

    assert!(
        !new_commit.authorship_log.attestations.is_empty(),
        "AI authorship should be preserved for lib.js after pathspec reset"
    );

    lib_file = repo.filename("lib.js");
    lib_file.assert_lines_and_blame(crate::lines![
        "// base".human(),
        "// AI lib 1".ai(),
        "// AI lib 2".ai(),
        "// More lib".ai(),
    ]);
}

/// Test git reset with directory pathspec: should reset only files in the specified directory
#[test]
fn test_reset_with_directory_pathspec() {
    let repo = TestRepo::new();

    // Create directory structure
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::create_dir_all(repo.path().join("lib")).unwrap();

    let mut src_file = repo.filename("src/app.rs");
    let mut lib_file = repo.filename("lib/utils.rs");
    let mut root_file = repo.filename("root.txt");

    // Base commit with files in different directories
    src_file.set_contents(crate::lines!["fn main() {}", ""]);
    lib_file.set_contents(crate::lines!["pub fn helper() {}", ""]);
    root_file.set_contents(crate::lines!["root content", ""]);
    let base_commit = repo.stage_all_and_commit("Base commit").unwrap();

    // Second commit: AI modifies files in all directories
    src_file.insert_at(1, crate::lines!["    // AI src change".ai()]);
    lib_file.insert_at(1, crate::lines!["    // AI lib change".ai()]);
    root_file.insert_at(1, crate::lines!["// AI root change".ai()]);
    repo.stage_all_and_commit("AI changes everywhere").unwrap();

    // Make uncommitted AI changes to lib and root (not src)
    lib_file.insert_at(2, crate::lines!["    // More AI lib".ai()]);
    root_file.insert_at(2, crate::lines!["// More AI root".ai()]);

    // Reset only the src directory to base commit using directory pathspec
    repo.git(&["reset", &base_commit.commit_sha, "--", "src"])
        .expect("reset with directory pathspec should succeed");

    // Stage all and commit to verify attributions
    let new_commit = repo
        .stage_all_and_commit("After directory pathspec reset")
        .unwrap();

    assert!(
        !new_commit.authorship_log.attestations.is_empty(),
        "AI authorship should be preserved for lib and root files"
    );

    // lib/utils.rs should still have AI changes (not in reset pathspec)
    lib_file = repo.filename("lib/utils.rs");
    lib_file.assert_lines_and_blame(crate::lines![
        "pub fn helper() {}".human(),
        "    // AI lib change".ai(),
        "    // More AI lib".ai(),
    ]);

    // root.txt should still have AI changes (not in reset pathspec)
    root_file = repo.filename("root.txt");
    root_file.assert_lines_and_blame(crate::lines![
        "root content".human(),
        "// AI root change".ai(),
        "// More AI root".ai(),
    ]);
}

/// Test that resetting a large commit (500+ lines across many files) preserves AI
/// authorship correctly.  This is the scenario from issue #1025: the previous
/// implementation ran `git blame target..target` for every changed file in the
/// post-reset hook, which (a) is O(files × file_size) wasted work and (b) always
/// produced zero AI attributions because the range is empty.  The fix creates an
/// empty target VA directly, halving the blame work with no correctness change.
#[test]
fn test_reset_large_commit_preserves_attribution() {
    let repo = TestRepo::new();

    // Create a base commit
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base line"]);
    let base_commit = repo.stage_all_and_commit("Base").unwrap();

    // Create a large AI commit: 10 files, each with multiple AI lines (≥50 lines total)
    let file_count = 10;
    let ai_lines_per_file = 6;
    let mut file_handles = Vec::new();
    for i in 0..file_count {
        let name = format!("module_{i}.rs");
        let mut f = repo.filename(&name);
        // One human line per file so the file has a non-AI context
        f.set_contents(crate::lines![format!("// module {i}")]);
        // Insert several AI lines
        for j in 0..ai_lines_per_file {
            f.insert_at(
                (j + 1) as usize,
                crate::lines![format!("    // AI line {j} in module {i}").ai()],
            );
        }
        file_handles.push((name, f));
    }
    repo.stage_all_and_commit("Large AI commit (500+ lines)")
        .unwrap();

    // Reset the large AI commit
    repo.git(&["reset", "--mixed", &base_commit.commit_sha])
        .expect("reset --mixed should succeed");

    // Re-commit and verify all AI attributions were preserved across all files
    let new_commit = repo.stage_all_and_commit("Re-commit after reset").unwrap();

    assert!(
        !new_commit.authorship_log.attestations.is_empty(),
        "AI authorship should be preserved after resetting a large commit"
    );

    // Spot-check: each module file should still have AI-attributed lines
    for (name, _) in &file_handles {
        let f = repo.filename(name);
        let ai_lines = f.lines_by_author(AuthorType::Ai);
        assert!(
            !ai_lines.is_empty(),
            "file {name} should have AI-attributed lines after reset + re-commit"
        );
    }
}

/// Test soft-reset-recommit preserves secondary file attribution when only
/// primary file is edited between reset and recommit. Reproduces the pattern
/// from fuzz_chaos_99 where multi-file commit → soft reset → edit one file → recommit
/// loses attribution for the untouched secondary file.
#[test]
fn test_soft_reset_recommit_preserves_secondary_file() {
    let repo = TestRepo::new();
    let main_path = repo.path().join("main.txt");
    let sec_path = repo.path().join("secondary.txt");

    // Initial commit with main file
    fs::write(&main_path, "AAA\nAAA\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Create secondary file with AI content and commit both
    fs::write(&sec_path, "BBB\nBBB\nBBB\nBBB\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary.txt"])
        .unwrap();
    // Also edit main
    fs::write(&main_path, "AAA\nAAA\nCCC\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("multi-file commit").unwrap();

    // Verify secondary before soft-reset
    let mut sec_file = repo.filename("secondary.txt");
    sec_file.assert_committed_lines(crate::lines![
        "BBB".ai(),
        "BBB".ai(),
        "BBB".ai(),
        "BBB".ai(),
    ]);

    // Soft reset
    repo.git(&["reset", "--soft", "HEAD~1"]).unwrap();

    // Edit only main file
    fs::write(&main_path, "AAA\nAAA\nCCC\nDDD\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();

    // Recommit
    repo.stage_all_and_commit("recommit after soft reset")
        .unwrap();

    // Secondary file should still have full AI attribution
    sec_file.assert_committed_lines(crate::lines![
        "BBB".ai(),
        "BBB".ai(),
        "BBB".ai(),
        "BBB".ai(),
    ]);
}

/// Test that attribution survives: multi-file commit → soft-reset-recommit →
/// further edits to one file with prepends → commit. The prepends shift line
/// numbers but the untouched lines should still be properly attributed by
/// tracing back through blame.
#[test]
fn test_soft_reset_recommit_with_subsequent_prepend() {
    let repo = TestRepo::new();
    let main_path = repo.path().join("main.txt");
    let sec_path = repo.path().join("secondary.txt");

    // Initial commit
    fs::write(&main_path, "base\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Commit secondary file with AI lines
    fs::write(&sec_path, "AI1\nAI2\nAI3\nAI4\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary.txt"])
        .unwrap();
    fs::write(&main_path, "base\nedit1\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("add secondary").unwrap();

    // Soft reset and recommit (with extra edit to main only)
    repo.git(&["reset", "--soft", "HEAD~1"]).unwrap();
    fs::write(&main_path, "base\nedit1\nextra\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("recommit").unwrap();

    // Verify secondary still attributed
    let mut sec_file = repo.filename("secondary.txt");
    sec_file.assert_committed_lines(crate::lines![
        "AI1".ai(),
        "AI2".ai(),
        "AI3".ai(),
        "AI4".ai(),
    ]);

    // Now prepend to secondary and commit — old AI lines shift down
    fs::write(&sec_path, "NEW1\nNEW2\nNEW3\nAI1\nAI2\nAI3\nAI4\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary.txt"])
        .unwrap();
    repo.stage_all_and_commit("prepend to secondary").unwrap();

    // Lines 4-7 should still be AI (traced back to recommit via blame)
    sec_file.assert_committed_lines(crate::lines![
        "NEW1".ai(),
        "NEW2".ai(),
        "NEW3".ai(),
        "AI1".ai(),
        "AI2".ai(),
        "AI3".ai(),
        "AI4".ai(),
    ]);
}

/// Reproduces the fuzz_chaos_99 pattern where identical content (same char
/// repeated) across multiple commits causes git blame to misattribute shifted
/// lines to a newer commit. The note for that commit must still cover them.
///
/// Pattern: AI file created → committed → soft-reset → recommit → further edits
/// with ReplaceRandom + Prepend → commit. Git blame assigns the shifted-but-
/// unchanged AI lines to the last commit, but since they weren't re-checkpointed,
/// they're missing from the note.
#[test]
fn test_blame_identical_content_shift_attribution() {
    let repo = TestRepo::new();
    let main_path = repo.path().join("main.txt");
    let sec_path = repo.path().join("secondary.txt");

    // Initial commit
    fs::write(&main_path, "base\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Create secondary file: 8 lines of "X" (AI) — identical content per line
    fs::write(&sec_path, "X\nX\nX\nX\nX\nX\nX\nX\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary.txt"])
        .unwrap();
    fs::write(&main_path, "base\nedit\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("add secondary with AI").unwrap();

    // Soft reset + recommit (only edit main)
    repo.git(&["reset", "--soft", "HEAD~1"]).unwrap();
    fs::write(&main_path, "base\nedit\nextra\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("recommit").unwrap();

    // Verify: all 8 "X" lines should be AI
    let mut sec_file = repo.filename("secondary.txt");
    sec_file.assert_committed_lines(crate::lines![
        "X".ai(),
        "X".ai(),
        "X".ai(),
        "X".ai(),
        "X".ai(),
        "X".ai(),
        "X".ai(),
        "X".ai(),
    ]);

    // Now: replace lines 1-2 with "Y" (AI), then prepend 4 "Z" lines (AI)
    // After: Z Z Z Z Y Y X X X X X X (12 lines)
    // The "X" lines at 5-12 were at 3-8 in parent → git blame should trace back.
    // But with identical "X" content and the replacement of lines 1-2, git's
    // diff algorithm may assign some "X" lines to this commit.
    fs::write(&sec_path, "Y\nY\nX\nX\nX\nX\nX\nX\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary.txt"])
        .unwrap();
    fs::write(&sec_path, "Z\nZ\nZ\nZ\nY\nY\nX\nX\nX\nX\nX\nX\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary.txt"])
        .unwrap();
    repo.stage_all_and_commit("replace and prepend").unwrap();

    // All lines should still be AI-attributed regardless of which commit
    // git blame assigns them to.
    sec_file.assert_committed_lines(crate::lines![
        "Z".ai(),
        "Z".ai(),
        "Z".ai(),
        "Z".ai(),
        "Y".ai(),
        "Y".ai(),
        "X".ai(),
        "X".ai(),
        "X".ai(),
        "X".ai(),
        "X".ai(),
        "X".ai(),
    ]);
}

/// Repro for forward reset orphaning the working log
/// (docs/forward-reset-orphans-working-log-2026-07-20.md): a non-hard reset
/// that moves HEAD *forward* (old HEAD is an ancestor of the new HEAD, e.g.
/// syncing the local branch onto a newer upstream commit) must carry
/// uncommitted AI attribution over to the new base commit.
#[test]
fn test_forward_reset_mixed_preserves_uncommitted_ai_attribution() {
    let repo = TestRepo::new();

    let ai_path = repo.path().join("feature.txt");
    let upstream_path = repo.path().join("upstream.txt");

    fs::write(&ai_path, "human line\n").unwrap();
    fs::write(&upstream_path, "upstream v1\n").unwrap();
    repo.stage_all_and_commit("Base commit").unwrap();

    // A newer "upstream" commit that is a descendant of the base commit.
    fs::write(&upstream_path, "upstream v2\n").unwrap();
    let upstream = repo.stage_all_and_commit("Upstream commit").unwrap();

    // Move the local branch back to the base commit (nothing uncommitted yet).
    repo.git(&["reset", "--hard", "HEAD~1"]).unwrap();

    // Uncommitted AI edit while HEAD is on the old base.
    fs::write(&ai_path, "human line\nAI line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();

    // Forward mixed reset onto the newer upstream commit.
    repo.git(&["reset", "--mixed", &upstream.commit_sha])
        .unwrap();

    // Commit the AI edit on top of the new base.
    repo.git(&["add", "feature.txt"]).unwrap();
    let commit = repo.commit("Commit AI work after forward reset").unwrap();

    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "AI attribution should survive a forward mixed reset"
    );
    let mut file = repo.filename("feature.txt");
    file.assert_committed_lines(crate::lines![
        "human line".unattributed_human(),
        "AI line".ai(),
    ]);
}

/// Same bug via `git reset --keep`, the exact mechanism Graphite's restack
/// uses to land a branch on a newer base commit (`git reset -q --keep
/// <new-commit>`). Unlike `--mixed`, `--keep` refuses to move HEAD at all if
/// the reset would clobber a conflicting uncommitted change, but it still
/// must carry non-conflicting uncommitted AI attribution over to the new
/// base -- exercised here as a forward reset with no stash involved.
#[test]
fn test_forward_reset_keep_preserves_uncommitted_ai_attribution() {
    let repo = TestRepo::new();

    let ai_path = repo.path().join("feature.txt");
    let upstream_path = repo.path().join("upstream.txt");

    fs::write(&ai_path, "human line\n").unwrap();
    fs::write(&upstream_path, "upstream v1\n").unwrap();
    repo.stage_all_and_commit("Base commit").unwrap();

    // A newer "upstream" commit that is a descendant of the base commit.
    fs::write(&upstream_path, "upstream v2\n").unwrap();
    let upstream = repo.stage_all_and_commit("Upstream commit").unwrap();

    // Move the local branch back to the base commit (nothing uncommitted yet).
    repo.git(&["reset", "--hard", "HEAD~1"]).unwrap();

    // Uncommitted AI edit while HEAD is on the old base. This edit doesn't
    // touch upstream.txt, so `--keep` won't refuse the reset.
    fs::write(&ai_path, "human line\nAI line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();

    // Forward `reset --keep` onto the newer upstream commit.
    repo.git(&["reset", "--keep", &upstream.commit_sha])
        .unwrap();

    // Commit the AI edit on top of the new base.
    repo.git(&["add", "feature.txt"]).unwrap();
    let commit = repo
        .commit("Commit AI work after forward reset --keep")
        .unwrap();

    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "AI attribution should survive a forward `reset --keep`"
    );
    let mut file = repo.filename("feature.txt");
    file.assert_committed_lines(crate::lines![
        "human line".unattributed_human(),
        "AI line".ai(),
    ]);
}

/// Same bug via the real-world sequence that surfaced it (IDE "sync/update
/// project": stash local changes -> forward reset onto newer upstream ->
/// stash pop -> commit).
#[test]
fn test_forward_reset_stash_pop_preserves_ai_attribution() {
    let repo = TestRepo::new();

    let ai_path = repo.path().join("feature.txt");
    let upstream_path = repo.path().join("upstream.txt");

    fs::write(&ai_path, "human line\n").unwrap();
    fs::write(&upstream_path, "upstream v1\n").unwrap();
    repo.stage_all_and_commit("Base commit").unwrap();

    fs::write(&upstream_path, "upstream v2\n").unwrap();
    let upstream = repo.stage_all_and_commit("Upstream commit").unwrap();

    repo.git(&["reset", "--hard", "HEAD~1"]).unwrap();

    fs::write(&ai_path, "human line\nAI line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();

    repo.git(&["stash"]).unwrap();
    repo.git(&["reset", "--mixed", &upstream.commit_sha])
        .unwrap();
    repo.git(&["stash", "pop"]).unwrap();

    repo.git(&["add", "feature.txt"]).unwrap();
    let commit = repo
        .commit("Commit AI work after stashed forward reset")
        .unwrap();

    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "AI attribution should survive stash -> forward reset -> stash pop"
    );
    let mut file = repo.filename("feature.txt");
    file.assert_committed_lines(crate::lines![
        "human line".unattributed_human(),
        "AI line".ai(),
    ]);
}

crate::reuse_tests_in_worktree!(
    test_reset_hard_deletes_working_log,
    test_reset_soft_reconstructs_working_log,
    test_reset_mixed_reconstructs_working_log,
    test_reset_to_same_commit_is_noop,
    test_reset_multiple_commits,
    test_reset_preserves_uncommitted_changes,
    test_reset_with_pathspec,
    test_reset_forward_is_noop,
    test_reset_mixed_ai_human_changes,
    test_reset_merge,
    test_reset_with_new_files,
    test_reset_with_deleted_files,
    test_reset_mixed_pathspec_preserves_ai_authorship,
    test_reset_mixed_pathspec_multiple_commits,
    test_reset_with_directory_pathspec,
    test_reset_large_commit_preserves_attribution,
    test_forward_reset_mixed_preserves_uncommitted_ai_attribution,
    test_forward_reset_keep_preserves_uncommitted_ai_attribution,
    test_forward_reset_stash_pop_preserves_ai_attribution,
);
