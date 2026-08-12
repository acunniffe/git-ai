use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

/// Restoring a path in the worktree discards its uncommitted provenance. If a
/// human later recreates identical bytes, the abandoned AI checkpoint must not
/// be resurrected into the commit.
#[test]
fn test_git_restore_worktree_discards_stale_ai_attribution() {
    let target = TestRepo::new();
    fs::write(target.path().join("restored.txt"), "base\n").expect("base should be writable");
    target
        .stage_all_and_commit("Initial commit")
        .expect("initial commit should succeed");
    target
        .filename("restored.txt")
        .assert_committed_lines(lines!["base".unattributed_human()]);
    let mut restored = target.filename("restored.txt");
    restored.set_contents_no_stage(lines!["discarded AI bytes".ai()]);
    target
        .git(&["restore", "--worktree", "--", "restored.txt"])
        .expect("worktree restore should succeed");
    assert_eq!(target.read_file("restored.txt").as_deref(), Some("base\n"));

    fs::write(target.path().join("restored.txt"), "discarded AI bytes\n")
        .expect("later human recreation should be writable");
    target
        .stage_all_and_commit("Human recreates text")
        .expect("later commit should succeed");
    let mut restored = target.filename("restored.txt");
    restored.assert_committed_lines(lines!["discarded AI bytes".unattributed_human()]);
}

/// `--staged` changes only the index. The AI worktree edit still exists and
/// must retain its checkpoint when it is re-added and committed later.
#[test]
fn test_git_restore_staged_preserves_uncommitted_ai_attribution() {
    let target = TestRepo::new();
    fs::write(target.path().join("unstaged.txt"), "base\n").expect("base should be writable");
    target
        .stage_all_and_commit("Initial commit")
        .expect("initial commit should succeed");
    target
        .filename("unstaged.txt")
        .assert_committed_lines(lines!["base".unattributed_human()]);

    let mut unstaged = target.filename("unstaged.txt");
    unstaged.set_contents_no_stage(lines!["AI worktree edit".ai()]);
    target
        .git_og(&["add", "unstaged.txt"])
        .expect("AI edit should be staged for the fixture");
    target
        .git(&["restore", "--staged", "--", "unstaged.txt"])
        .expect("staged restore should succeed");
    assert_eq!(
        target.read_file("unstaged.txt").as_deref(),
        Some("AI worktree edit")
    );

    target
        .stage_all_and_commit("Re-add AI work")
        .expect("later commit should succeed");
    let mut unstaged = target.filename("unstaged.txt");
    unstaged.assert_committed_lines(lines!["AI worktree edit".ai()]);
}

/// Restoring from another tree copies existing authored content. A subsequent
/// commit should preserve the source commit's line provenance rather than
/// treating the restored bytes as newly authored by the committer.
#[test]
fn test_git_restore_source_staged_worktree_preserves_source_attribution() {
    let target = TestRepo::new();
    fs::write(target.path().join("sourced.txt"), "base\n").expect("base should be writable");
    target
        .stage_all_and_commit("Initial commit")
        .expect("initial commit should succeed");
    target
        .filename("sourced.txt")
        .assert_committed_lines(lines!["base".unattributed_human()]);
    let main_branch = target.current_branch();
    target
        .git(&["branch", "source"])
        .expect("source branch should be created");
    target
        .git(&["checkout", "source"])
        .expect("source branch checkout should succeed");
    let mut sourced = target.filename("sourced.txt");
    sourced.set_contents(lines!["authored on source".ai()]);
    target
        .stage_all_and_commit("AI source commit")
        .expect("source commit should succeed");
    sourced.assert_committed_lines(lines!["authored on source".ai()]);
    target
        .git(&["checkout", &main_branch])
        .expect("main checkout should succeed");

    target
        .git(&[
            "restore",
            "--source=source",
            "--staged",
            "--worktree",
            "--",
            "sourced.txt",
        ])
        .expect("source restore should succeed");
    target
        .git(&["commit", "-m", "Copy restored source"])
        .expect("restored source commit should succeed");

    let mut sourced = target.filename("sourced.txt");
    sourced.assert_committed_lines(lines!["authored on source".ai()]);
}

fn conflicted_restore_repo() -> (TestRepo, String) {
    let target = TestRepo::new();
    fs::write(target.path().join("conflict.txt"), "base\n").unwrap();
    target.stage_all_and_commit("base").unwrap();
    target
        .filename("conflict.txt")
        .assert_committed_lines(lines!["base".unattributed_human()]);
    let main = target.current_branch();
    target.git(&["checkout", "-b", "side"]).unwrap();
    let mut conflict = target.filename("conflict.txt");
    conflict.set_contents(lines!["side AI".ai()]);
    target.stage_all_and_commit("side AI").unwrap();
    conflict.assert_committed_lines(lines!["side AI".ai()]);
    target.git(&["checkout", &main]).unwrap();
    fs::write(target.path().join("conflict.txt"), "main human\n").unwrap();
    target.stage_all_and_commit("main human").unwrap();
    conflict = target.filename("conflict.txt");
    conflict.assert_committed_lines(lines!["main human".unattributed_human()]);
    assert!(target.git(&["merge", "side"]).is_err());
    (target, main)
}

#[test]
fn test_git_restore_ours_conflict_stage_selects_current_human_side() {
    let (target, _main) = conflicted_restore_repo();
    let mut pending = target.filename("pending.txt");
    pending.set_contents_no_stage(lines!["pending conflict AI".ai()]);
    target
        .git(&["restore", "--ours", "--worktree", "--", "conflict.txt"])
        .unwrap();
    target.stage_all_and_commit("resolve with ours").unwrap();

    let mut conflict = target.filename("conflict.txt");
    conflict.assert_committed_lines(lines!["main human".unattributed_human()]);
    pending.assert_committed_lines(lines!["pending conflict AI".ai()]);
}

#[test]
fn test_git_restore_theirs_conflict_stage_selects_ai_source_side() {
    let (target, _main) = conflicted_restore_repo();
    let mut pending = target.filename("pending.txt");
    pending.set_contents_no_stage(lines!["pending conflict AI".ai()]);
    target
        .git(&["restore", "--theirs", "--worktree", "--", "conflict.txt"])
        .unwrap();
    target.stage_all_and_commit("resolve with theirs").unwrap();

    let mut conflict = target.filename("conflict.txt");
    conflict.assert_committed_lines(lines!["side AI".ai()]);
    pending.assert_committed_lines(lines!["pending conflict AI".ai()]);
}

#[test]
fn test_git_restore_source_with_nul_pathspec_file_preserves_each_source_note() {
    let target = TestRepo::new();
    fs::write(target.path().join("one.txt"), "one base\n").unwrap();
    fs::write(target.path().join("two.txt"), "two base\n").unwrap();
    target.stage_all_and_commit("base").unwrap();
    target
        .filename("one.txt")
        .assert_committed_lines(lines!["one base".unattributed_human()]);
    target
        .filename("two.txt")
        .assert_committed_lines(lines!["two base".unattributed_human()]);
    let main = target.current_branch();
    target.git(&["checkout", "-b", "source-pathspec"]).unwrap();
    let mut one = target.filename("one.txt");
    let mut two = target.filename("two.txt");
    one.set_contents(lines!["one source AI".ai()]);
    two.set_contents(lines!["two source AI".ai()]);
    target.stage_all_and_commit("source paths").unwrap();
    one.assert_committed_lines(lines!["one source AI".ai()]);
    two.assert_committed_lines(lines!["two source AI".ai()]);
    target.git(&["checkout", &main]).unwrap();

    let pathspec_dir = tempfile::tempdir().unwrap();
    let pathspec = pathspec_dir.path().join("paths.nul");
    fs::write(&pathspec, b"one.txt\0two.txt\0").unwrap();
    target
        .git(&[
            "restore",
            "--source=source-pathspec",
            "--staged",
            "--worktree",
            &format!("--pathspec-from-file={}", pathspec.display()),
            "--pathspec-file-nul",
        ])
        .unwrap();
    target
        .git(&["commit", "-m", "restore pathspec sources"])
        .unwrap();

    one = target.filename("one.txt");
    two = target.filename("two.txt");
    one.assert_committed_lines(lines!["one source AI".ai()]);
    two.assert_committed_lines(lines!["two source AI".ai()]);
}
