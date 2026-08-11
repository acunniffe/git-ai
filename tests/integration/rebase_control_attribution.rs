use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

fn conflicting_rebase_repo() -> TestRepo {
    let repo = TestRepo::new();
    fs::write(repo.path().join("conflict.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut file = repo.filename("conflict.txt");
    file.set_contents_no_stage(lines!["feature AI".ai()]);
    repo.stage_all_and_commit("Feature AI").unwrap();
    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("conflict.txt"), "main human\n").unwrap();
    repo.stage_all_and_commit("Main human").unwrap();
    repo.git(&["checkout", "feature"]).unwrap();
    assert!(repo.git(&["rebase", &main]).is_err());
    repo
}

#[test]
fn test_git_rebase_quit_keeps_ai_resolution_for_ordinary_commit() {
    let repo = conflicting_rebase_repo();
    let mut file = repo.filename("conflict.txt");
    file.set_contents_no_stage(lines!["AI rebase resolution after quit".ai()]);
    repo.git(&["add", "--", "conflict.txt"]).unwrap();
    repo.git(&["rebase", "--quit"]).unwrap();
    repo.git(&["commit", "-m", "Commit resolution after rebase quit"])
        .unwrap();

    let mut file = repo.filename("conflict.txt");
    file.assert_committed_lines(lines!["AI rebase resolution after quit".ai()]);
}

#[test]
fn test_git_rebase_abort_discards_ai_resolution_checkpoint() {
    let repo = conflicting_rebase_repo();
    let mut file = repo.filename("conflict.txt");
    file.set_contents_no_stage(lines!["discarded AI rebase resolution".ai()]);
    repo.git(&["add", "--", "conflict.txt"]).unwrap();
    repo.git(&["rebase", "--abort"]).unwrap();

    fs::write(
        repo.path().join("conflict.txt"),
        "discarded AI rebase resolution\n",
    )
    .unwrap();
    repo.stage_all_and_commit("Human recreates discarded rebase resolution")
        .unwrap();
    let mut file = repo.filename("conflict.txt");
    file.assert_committed_lines(lines![
        "discarded AI rebase resolution".unattributed_human()
    ]);
}

#[test]
fn test_git_rebase_skip_discards_ai_resolution_checkpoint() {
    let repo = conflicting_rebase_repo();
    let mut file = repo.filename("conflict.txt");
    file.set_contents_no_stage(lines!["discarded AI rebase skip".ai()]);
    repo.git(&["add", "--", "conflict.txt"]).unwrap();
    repo.git(&["rebase", "--skip"]).unwrap();

    fs::write(
        repo.path().join("conflict.txt"),
        "discarded AI rebase skip\n",
    )
    .unwrap();
    repo.stage_all_and_commit("Human recreates skipped rebase resolution")
        .unwrap();
    let mut file = repo.filename("conflict.txt");
    file.assert_committed_lines(lines!["discarded AI rebase skip".unattributed_human()]);
}

#[test]
fn test_git_rebase_update_refs_moves_intermediate_branch_and_notes() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut first = repo.filename("first.txt");
    first.set_contents_no_stage(lines!["first AI".ai()]);
    repo.stage_all_and_commit("First feature AI").unwrap();
    repo.git(&["branch", "intermediate"]).unwrap();
    let old_intermediate = repo.git(&["rev-parse", "intermediate"]).unwrap();
    let mut second = repo.filename("second.txt");
    second.set_contents_no_stage(lines!["second AI".ai()]);
    repo.stage_all_and_commit("Second feature AI").unwrap();

    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.txt"), "main human\n").unwrap();
    repo.stage_all_and_commit("Advance main").unwrap();
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", "--update-refs", &main]).unwrap();

    let new_intermediate = repo.git(&["rev-parse", "intermediate"]).unwrap();
    assert_ne!(old_intermediate.trim(), new_intermediate.trim());
    repo.git(&["checkout", "intermediate"]).unwrap();
    let mut first = repo.filename("first.txt");
    first.assert_committed_lines(lines!["first AI".ai()]);
    repo.git(&["checkout", "feature"]).unwrap();
    let mut first = repo.filename("first.txt");
    first.assert_committed_lines(lines!["first AI".ai()]);
    let mut second = repo.filename("second.txt");
    second.assert_committed_lines(lines!["second AI".ai()]);
}

#[test]
fn test_git_rebase_merges_update_refs_moves_side_branch_with_attribution() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature = repo.filename("feature.txt");
    feature.set_contents_no_stage(lines!["feature AI".ai()]);
    repo.stage_all_and_commit("Feature AI").unwrap();
    repo.git(&["checkout", "-b", "side"]).unwrap();
    let mut side = repo.filename("side.txt");
    side.set_contents_no_stage(lines!["side AI".ai()]);
    repo.stage_all_and_commit("Side AI").unwrap();
    let old_side = repo.git(&["rev-parse", "side"]).unwrap();
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["merge", "--no-ff", "-m", "Merge side", "side"])
        .unwrap();

    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.txt"), "main human\n").unwrap();
    repo.stage_all_and_commit("Advance main").unwrap();
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", "--rebase-merges", "--update-refs", &main])
        .unwrap();

    let new_side = repo.git(&["rev-parse", "side"]).unwrap();
    assert_ne!(old_side.trim(), new_side.trim());
    repo.git(&["checkout", "side"]).unwrap();
    let mut side = repo.filename("side.txt");
    side.assert_committed_lines(lines!["side AI".ai()]);
    repo.git(&["checkout", "feature"]).unwrap();
    let mut feature = repo.filename("feature.txt");
    feature.assert_committed_lines(lines!["feature AI".ai()]);
    let mut side = repo.filename("side.txt");
    side.assert_committed_lines(lines!["side AI".ai()]);
}
