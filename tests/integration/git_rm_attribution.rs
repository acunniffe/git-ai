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
    target
        .stage_all_and_commit("Human recreates removed path")
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
