use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

fn conflicting_revert_repo() -> (TestRepo, String) {
    let repo = TestRepo::new();
    fs::write(repo.path().join("conflict.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut file = repo.filename("conflict.txt");
    file.set_contents_no_stage(lines!["source AI".ai()]);
    let source_commit = repo
        .stage_all_and_commit("AI source change")
        .unwrap()
        .commit_sha;

    fs::write(repo.path().join("conflict.txt"), "later human\n").unwrap();
    repo.stage_all_and_commit("Later human change").unwrap();
    (repo, source_commit)
}

#[test]
fn test_git_revert_no_commit_then_commit_restores_source_ai_attribution() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("revert.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut file = repo.filename("revert.txt");
    file.set_contents_no_stage(lines!["base".human(), "restored AI".ai()]);
    repo.stage_all_and_commit("Add AI line").unwrap();

    fs::write(repo.path().join("revert.txt"), "base\n").unwrap();
    let delete_commit = repo
        .stage_all_and_commit("Delete AI line")
        .unwrap()
        .commit_sha;

    repo.git(&["revert", "--no-commit", &delete_commit])
        .unwrap();
    repo.git(&["commit", "-m", "Commit deferred revert"])
        .unwrap();

    let mut file = repo.filename("revert.txt");
    file.assert_committed_lines(lines!["base".human(), "restored AI".ai()]);
}

#[test]
fn test_git_revert_no_commit_multiple_sources_merges_restored_provenance() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut first = repo.filename("first.txt");
    first.set_contents_no_stage(lines!["first AI".ai()]);
    repo.stage_all_and_commit("Add first AI file").unwrap();
    let mut second = repo.filename("second.txt");
    second.set_contents_no_stage(lines!["second AI".ai()]);
    repo.stage_all_and_commit("Add second AI file").unwrap();

    repo.git(&["rm", "--", "first.txt"]).unwrap();
    let delete_first = repo
        .stage_all_and_commit("Delete first AI file")
        .unwrap()
        .commit_sha;
    repo.git(&["rm", "--", "second.txt"]).unwrap();
    let delete_second = repo
        .stage_all_and_commit("Delete second AI file")
        .unwrap()
        .commit_sha;

    repo.git(&["revert", "--no-commit", &delete_first, &delete_second])
        .unwrap();
    repo.git(&["commit", "-m", "Commit two deferred reverts"])
        .unwrap();

    let mut first = repo.filename("first.txt");
    first.assert_committed_lines(lines!["first AI".ai()]);
    let mut second = repo.filename("second.txt");
    second.assert_committed_lines(lines!["second AI".ai()]);
}

#[test]
fn test_git_revert_continue_attributes_ai_conflict_resolution() {
    let (repo, source_commit) = conflicting_revert_repo();
    assert!(repo.git(&["revert", &source_commit]).is_err());
    let mut file = repo.filename("conflict.txt");
    file.set_contents_no_stage(lines!["AI revert resolution".ai()]);
    repo.git(&["add", "--", "conflict.txt"]).unwrap();
    repo.git_with_env(&["revert", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let mut file = repo.filename("conflict.txt");
    file.assert_committed_lines(lines!["AI revert resolution".ai()]);
}

#[test]
fn test_git_revert_abort_discards_ai_resolution_checkpoint() {
    let (repo, source_commit) = conflicting_revert_repo();
    assert!(repo.git(&["revert", &source_commit]).is_err());
    let mut file = repo.filename("conflict.txt");
    file.set_contents_no_stage(lines!["discarded AI revert resolution".ai()]);
    repo.git(&["add", "--", "conflict.txt"]).unwrap();
    repo.git(&["revert", "--abort"]).unwrap();

    fs::write(
        repo.path().join("conflict.txt"),
        "discarded AI revert resolution\n",
    )
    .unwrap();
    repo.stage_all_and_commit("Human recreates discarded resolution")
        .unwrap();
    let mut file = repo.filename("conflict.txt");
    file.assert_committed_lines(lines![
        "discarded AI revert resolution".unattributed_human()
    ]);
}

#[test]
fn test_git_revert_quit_keeps_ai_resolution_for_ordinary_commit() {
    let (repo, source_commit) = conflicting_revert_repo();
    assert!(repo.git(&["revert", &source_commit]).is_err());
    let mut file = repo.filename("conflict.txt");
    file.set_contents_no_stage(lines!["AI revert resolution after quit".ai()]);
    repo.git(&["add", "--", "conflict.txt"]).unwrap();
    repo.git(&["revert", "--quit"]).unwrap();
    repo.git(&["commit", "-m", "Commit resolution after revert quit"])
        .unwrap();

    let mut file = repo.filename("conflict.txt");
    file.assert_committed_lines(lines!["AI revert resolution after quit".ai()]);
}

#[test]
fn test_git_revert_skip_preserves_unrelated_ai_checkpoint() {
    let (repo, source_commit) = conflicting_revert_repo();
    let mut unrelated = repo.filename("unrelated.txt");
    unrelated.set_contents_no_stage(lines!["unrelated AI".ai()]);
    assert!(repo.git(&["revert", &source_commit]).is_err());
    let mut conflict = repo.filename("conflict.txt");
    conflict.set_contents_no_stage(lines!["discarded AI skipped resolution".ai()]);
    repo.git(&["add", "--", "conflict.txt"]).unwrap();
    repo.git(&["revert", "--skip"]).unwrap();
    fs::write(
        repo.path().join("conflict.txt"),
        "discarded AI skipped resolution\n",
    )
    .unwrap();
    repo.git(&["add", "--", "unrelated.txt"]).unwrap();
    repo.git(&["add", "--", "conflict.txt"]).unwrap();
    repo.git(&["commit", "-m", "Commit after skipped revert"])
        .unwrap();

    let mut unrelated = repo.filename("unrelated.txt");
    unrelated.assert_committed_lines(lines!["unrelated AI".ai()]);
    let mut conflict = repo.filename("conflict.txt");
    conflict.assert_committed_lines(lines![
        "discarded AI skipped resolution".unattributed_human()
    ]);
}

#[test]
fn test_git_revert_mainline_restores_first_parent_ai_attribution() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut restored = repo.filename("restored.txt");
    restored.set_contents_no_stage(lines!["first-parent AI".ai()]);
    repo.stage_all_and_commit("Add AI on main").unwrap();
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "delete-on-feature"]).unwrap();
    repo.git(&["rm", "--", "restored.txt"]).unwrap();
    repo.git(&["commit", "-m", "Delete AI file on feature"])
        .unwrap();

    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.txt"), "main human\n").unwrap();
    repo.stage_all_and_commit("Diverge main").unwrap();
    repo.git(&[
        "merge",
        "--no-ff",
        "-m",
        "Merge deletion",
        "delete-on-feature",
    ])
    .unwrap();
    let merge_commit = repo.git(&["rev-parse", "HEAD"]).unwrap();

    repo.git(&["revert", "-m", "1", "--no-edit", merge_commit.trim()])
        .unwrap();
    let mut restored = repo.filename("restored.txt");
    restored.assert_committed_lines(lines!["first-parent AI".ai()]);
}
