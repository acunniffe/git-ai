use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

fn divergent_conflict_repo() -> (TestRepo, String) {
    let repo = TestRepo::new();
    fs::write(repo.path().join("conflict.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut conflict = repo.filename("conflict.txt");
    conflict.set_contents_no_stage(lines!["feature AI".ai()]);
    let mut feature_only = repo.filename("feature-only.txt");
    feature_only.set_contents_no_stage(lines!["feature-only AI".ai()]);
    repo.stage_all_and_commit("Feature changes").unwrap();

    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("conflict.txt"), "main human\n").unwrap();
    repo.stage_all_and_commit("Main changes").unwrap();
    (repo, main)
}

#[test]
fn test_git_merge_no_commit_then_commit_preserves_source_attribution() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature = repo.filename("feature.txt");
    feature.set_contents_no_stage(lines!["feature AI".ai()]);
    repo.stage_all_and_commit("Feature AI").unwrap();
    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.txt"), "main human\n").unwrap();
    repo.stage_all_and_commit("Main human").unwrap();

    repo.git(&["merge", "--no-commit", "--no-ff", "feature"])
        .unwrap();
    repo.git(&["commit", "-m", "Explicit merge commit"])
        .unwrap();

    let mut feature = repo.filename("feature.txt");
    feature.assert_committed_lines(lines!["feature AI".ai()]);
    let mut main_file = repo.filename("main.txt");
    main_file.assert_committed_lines(lines!["main human".human()]);
}

#[test]
fn test_git_merge_continue_attributes_ai_conflict_resolution() {
    let (repo, _main) = divergent_conflict_repo();
    assert!(repo.git(&["merge", "feature"]).is_err());
    let mut conflict = repo.filename("conflict.txt");
    conflict.set_contents_no_stage(lines!["resolved by AI".ai()]);
    repo.git(&["add", "--", "conflict.txt"]).unwrap();

    repo.git_with_env(&["merge", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let mut conflict = repo.filename("conflict.txt");
    conflict.assert_committed_lines(lines!["resolved by AI".ai()]);
    let mut feature_only = repo.filename("feature-only.txt");
    feature_only.assert_committed_lines(lines!["feature-only AI".ai()]);
}

#[test]
fn test_git_merge_abort_discards_ai_resolution_checkpoint() {
    let (repo, _main) = divergent_conflict_repo();
    assert!(repo.git(&["merge", "feature"]).is_err());
    let mut conflict = repo.filename("conflict.txt");
    conflict.set_contents_no_stage(lines!["discarded AI resolution".ai()]);
    repo.git(&["add", "--", "conflict.txt"]).unwrap();
    repo.git(&["merge", "--abort"]).unwrap();

    fs::write(
        repo.path().join("conflict.txt"),
        "discarded AI resolution\n",
    )
    .unwrap();
    repo.stage_all_and_commit("Human recreates discarded resolution")
        .unwrap();
    let mut conflict = repo.filename("conflict.txt");
    conflict.assert_committed_lines(lines!["discarded AI resolution".unattributed_human()]);
}

#[test]
fn test_git_merge_quit_keeps_ai_resolution_for_ordinary_commit() {
    let (repo, _main) = divergent_conflict_repo();
    assert!(repo.git(&["merge", "feature"]).is_err());
    let mut conflict = repo.filename("conflict.txt");
    conflict.set_contents_no_stage(lines!["AI resolution after quit".ai()]);
    repo.git(&["add", "--", "conflict.txt"]).unwrap();
    repo.git(&["merge", "--quit"]).unwrap();
    repo.git(&["commit", "-m", "Commit resolution without merge metadata"])
        .unwrap();

    let mut conflict = repo.filename("conflict.txt");
    conflict.assert_committed_lines(lines!["AI resolution after quit".ai()]);
}

#[test]
fn test_git_merge_ff_only_rejection_preserves_unrelated_ai_checkpoint() {
    let (repo, _main) = divergent_conflict_repo();
    let mut unrelated = repo.filename("unrelated.txt");
    unrelated.set_contents_no_stage(lines!["unrelated AI".ai()]);
    assert!(repo.git(&["merge", "--ff-only", "feature"]).is_err());

    repo.git(&["add", "--", "unrelated.txt"]).unwrap();
    repo.git(&["commit", "-m", "Commit after rejected ff-only"])
        .unwrap();
    let mut unrelated = repo.filename("unrelated.txt");
    unrelated.assert_committed_lines(lines!["unrelated AI".ai()]);
}

#[test]
fn test_git_merge_fast_forward_carries_dirty_ai_checkpoint_to_new_head() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(repo.path().join("feature.txt"), "feature human\n").unwrap();
    repo.stage_all_and_commit("Feature commit").unwrap();
    repo.git(&["checkout", &main]).unwrap();

    let mut dirty = repo.filename("dirty.txt");
    dirty.set_contents_no_stage(lines!["dirty AI before fast-forward".ai()]);
    repo.git(&["merge", "--ff-only", "feature"]).unwrap();
    repo.git(&["add", "--", "dirty.txt"]).unwrap();
    repo.git(&["commit", "-m", "Commit carried dirty file"])
        .unwrap();

    let mut dirty = repo.filename("dirty.txt");
    dirty.assert_committed_lines(lines!["dirty AI before fast-forward".ai()]);
}

#[test]
fn test_git_merge_commit_carries_unrelated_dirty_ai_checkpoint() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature = repo.filename("feature.txt");
    feature.set_contents_no_stage(lines!["feature AI".ai()]);
    repo.stage_all_and_commit("Feature AI").unwrap();
    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.txt"), "main human\n").unwrap();
    repo.stage_all_and_commit("Main human").unwrap();

    let mut dirty = repo.filename("dirty.txt");
    dirty.set_contents_no_stage(lines!["dirty AI before merge commit".ai()]);
    repo.git(&["merge", "--no-ff", "-m", "Merge feature", "feature"])
        .unwrap();

    let mut feature = repo.filename("feature.txt");
    feature.assert_committed_lines(lines!["feature AI".ai()]);
    repo.git(&["add", "--", "dirty.txt"]).unwrap();
    repo.git(&["commit", "-m", "Commit carried dirty file"])
        .unwrap();
    let mut dirty = repo.filename("dirty.txt");
    dirty.assert_committed_lines(lines!["dirty AI before merge commit".ai()]);
}
