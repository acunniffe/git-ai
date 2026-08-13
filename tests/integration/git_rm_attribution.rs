use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

/// Removing a tracked path discards both its bytes and any uncommitted AI
/// provenance. Identical bytes recreated later by a human must not inherit the
/// abandoned checkpoint.
#[test]
fn test_git_rm_discards_deleted_path_ai_attribution() {
    let target = TestRepo::new();
    fs::write(target.path().join("removed.txt"), "base\n").unwrap();
    target.stage_all_and_commit("Initial commit").unwrap();
    target
        .filename("removed.txt")
        .assert_committed_lines(lines!["base".unattributed_human()]);
    let mut removed = target.filename("removed.txt");
    removed.set_contents_no_stage(lines!["discarded AI bytes".ai()]);

    target.git(&["rm", "-f", "--", "removed.txt"]).unwrap();
    assert!(target.read_file("removed.txt").is_none());
    target.sync_daemon();
    fs::write(target.path().join("removed.txt"), "discarded AI bytes\n").unwrap();
    // Stage before the daemon processes `rm`; the post-command index must not
    // make the removed operand look protected.
    target.git_og(&["add", "removed.txt"]).unwrap();
    target
        .git(&["commit", "-m", "Human recreates removed path"])
        .unwrap();

    let mut removed = target.filename("removed.txt");
    removed.assert_committed_lines(lines!["discarded AI bytes".unattributed_human()]);
}

/// `rm --cached` changes only the index. The AI-authored worktree file remains
/// present, so a later re-add must retain the original AI checkpoint.
#[test]
fn test_git_rm_cached_preserves_worktree_ai_attribution() {
    let target = TestRepo::new();
    fs::write(target.path().join("cached.txt"), "base\n").unwrap();
    target.stage_all_and_commit("Initial commit").unwrap();
    target
        .filename("cached.txt")
        .assert_committed_lines(lines!["base".unattributed_human()]);
    let mut cached = target.filename("cached.txt");
    cached.set_contents_no_stage(lines!["AI worktree replacement".ai()]);
    target.git_og(&["add", "cached.txt"]).unwrap();

    target.git(&["rm", "--cached", "--", "cached.txt"]).unwrap();
    assert_eq!(
        target.read_file("cached.txt").as_deref(),
        Some("AI worktree replacement")
    );
    target.stage_all_and_commit("Re-add cached path").unwrap();

    let mut cached = target.filename("cached.txt");
    cached.assert_committed_lines(lines!["AI worktree replacement".ai()]);
}

/// Recursive removal must prune only paths that disappeared. Checkpoints for
/// unrelated tracked edits in the same working log must survive.
#[test]
fn test_git_rm_recursive_prunes_only_removed_paths() {
    let target = TestRepo::new();
    fs::create_dir_all(target.path().join("pkg")).unwrap();
    fs::write(target.path().join("pkg/deleted.txt"), "base deleted\n").unwrap();
    fs::write(target.path().join("kept.txt"), "base kept\n").unwrap();
    target.stage_all_and_commit("Initial commit").unwrap();
    target
        .filename("pkg/deleted.txt")
        .assert_committed_lines(lines!["base deleted".unattributed_human()]);
    target
        .filename("kept.txt")
        .assert_committed_lines(lines!["base kept".unattributed_human()]);
    let mut deleted = target.filename("pkg/deleted.txt");
    deleted.set_contents_no_stage(lines!["discarded directory AI".ai()]);
    let mut kept = target.filename("kept.txt");
    kept.set_contents_no_stage(lines!["kept AI edit".ai()]);

    target.git(&["rm", "-rf", "--", "pkg"]).unwrap();
    target.sync_daemon();
    fs::create_dir_all(target.path().join("pkg")).unwrap();
    fs::write(
        target.path().join("pkg/deleted.txt"),
        "discarded directory AI\n",
    )
    .unwrap();
    target
        .stage_all_and_commit("Recreate removed directory path")
        .unwrap();

    let mut deleted = target.filename("pkg/deleted.txt");
    deleted.assert_committed_lines(lines!["discarded directory AI".unattributed_human()]);
    let mut kept = target.filename("kept.txt");
    kept.assert_committed_lines(lines!["kept AI edit".ai()]);
}

/// Path lists select explicit operands just like command-line pathspecs. The
/// daemon must not interpret the option-only argv as "remove everything".
#[test]
fn test_git_rm_pathspec_file_prunes_only_listed_paths() {
    let target = TestRepo::new();
    fs::write(target.path().join("removed.txt"), "base removed\n").unwrap();
    fs::write(target.path().join("kept.txt"), "base kept\n").unwrap();
    target.stage_all_and_commit("Initial commit").unwrap();
    target
        .filename("removed.txt")
        .assert_committed_lines(lines!["base removed".unattributed_human()]);
    target
        .filename("kept.txt")
        .assert_committed_lines(lines!["base kept".unattributed_human()]);

    target
        .filename("removed.txt")
        .set_contents_no_stage(lines!["discarded list AI".ai()]);
    target
        .filename("kept.txt")
        .set_contents_no_stage(lines!["kept list AI".ai()]);
    fs::write(target.path().join("remove-paths.txt"), "removed.txt\n").unwrap();

    target
        .git(&["rm", "-f", "--pathspec-from-file=remove-paths.txt"])
        .unwrap();
    fs::write(target.path().join("removed.txt"), "discarded list AI\n").unwrap();
    target.git_og(&["add", "removed.txt", "kept.txt"]).unwrap();
    fs::remove_file(target.path().join("remove-paths.txt")).unwrap();
    target
        .git(&["commit", "-m", "Commit after pathspec-file removal"])
        .unwrap();

    target
        .filename("removed.txt")
        .assert_committed_lines(lines!["discarded list AI".unattributed_human()]);
    target
        .filename("kept.txt")
        .assert_committed_lines(lines!["kept list AI".ai()]);
}

/// Dry-run is observational and must not disturb the worktree or provenance.
#[test]
fn test_git_rm_dry_run_preserves_ai_attribution() {
    let target = TestRepo::new();
    fs::write(target.path().join("dry.txt"), "base\n").unwrap();
    target.stage_all_and_commit("Initial commit").unwrap();
    target
        .filename("dry.txt")
        .assert_committed_lines(lines!["base".unattributed_human()]);
    let mut dry = target.filename("dry.txt");
    dry.set_contents_no_stage(lines!["AI edit kept by dry run".ai()]);

    target.git(&["rm", "-n", "-f", "--", "dry.txt"]).unwrap();
    assert_eq!(
        target.read_file("dry.txt").as_deref(),
        Some("AI edit kept by dry run")
    );
    target
        .stage_all_and_commit("Commit after rm dry run")
        .unwrap();

    let mut dry = target.filename("dry.txt");
    dry.assert_committed_lines(lines!["AI edit kept by dry run".ai()]);
}

#[test]
fn test_git_rm_from_subdirectory_only_prunes_the_selected_same_named_path() {
    let repo = TestRepo::new();
    let subdir = repo.path().join("sub");
    fs::create_dir_all(&subdir).unwrap();
    fs::write(repo.path().join("foo.txt"), "root base\n").unwrap();
    fs::write(subdir.join("foo.txt"), "sub base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "foo.txt"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "sub/foo.txt"])
        .unwrap();
    repo.stage_all_and_commit("Initial same-named files")
        .unwrap();
    repo.assert_file_committed_lines("foo.txt", lines!["root base".human()]);
    repo.assert_file_committed_lines("sub/foo.txt", lines!["sub base".human()]);

    repo.write_ai_edit("foo.txt", "root AI kept\n");
    repo.write_ai_edit("sub/foo.txt", "sub AI removed\n");
    repo.git_from_working_dir(&subdir, &["rm", "-f", "foo.txt"])
        .unwrap();
    repo.stage_all_and_commit("Remove only subdirectory file")
        .unwrap();

    repo.assert_file_committed_lines("foo.txt", lines!["root AI kept".ai()]);
    assert!(!subdir.join("foo.txt").exists());
}
