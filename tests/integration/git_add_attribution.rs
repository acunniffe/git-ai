use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

#[test]
fn test_git_add_update_stages_tracked_only_and_preserves_untracked_ai_for_later() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("tracked-a.txt"), "base a\n").unwrap();
    fs::write(repo.path().join("tracked-b.txt"), "base b\n").unwrap();
    repo.stage_all_and_commit("Initial files").unwrap();

    let mut tracked_a = repo.filename("tracked-a.txt");
    tracked_a.set_contents_no_stage(lines!["tracked A AI".ai()]);
    let mut tracked_b = repo.filename("tracked-b.txt");
    tracked_b.set_contents_no_stage(lines!["tracked B AI".ai()]);
    let mut untracked = repo.filename("untracked.txt");
    untracked.set_contents_no_stage(lines!["untracked AI".ai()]);

    repo.git(&["add", "-u"]).unwrap();
    repo.git(&["commit", "-m", "Commit tracked updates"])
        .unwrap();
    let mut tracked_a = repo.filename("tracked-a.txt");
    tracked_a.assert_committed_lines(lines!["tracked A AI".ai()]);
    let mut tracked_b = repo.filename("tracked-b.txt");
    tracked_b.assert_committed_lines(lines!["tracked B AI".ai()]);
    assert!(repo.read_file("untracked.txt").is_some());

    repo.git(&["add", "--", "untracked.txt"]).unwrap();
    repo.git(&["commit", "-m", "Commit residual untracked file"])
        .unwrap();
    let mut untracked = repo.filename("untracked.txt");
    untracked.assert_committed_lines(lines!["untracked AI".ai()]);
}

#[test]
fn test_git_add_pathspec_from_file_nul_commits_selected_file_and_carries_residual() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut selected = repo.filename("selected name.txt");
    selected.set_contents_no_stage(lines!["selected AI".ai()]);
    let mut residual = repo.filename("residual.txt");
    residual.set_contents_no_stage(lines!["residual AI".ai()]);

    let pathspec_dir = tempfile::tempdir().unwrap();
    let pathspec_file = pathspec_dir.path().join("paths.nul");
    fs::write(&pathspec_file, b"selected name.txt\0").unwrap();
    repo.git(&[
        "add",
        &format!("--pathspec-from-file={}", pathspec_file.display()),
        "--pathspec-file-nul",
    ])
    .unwrap();
    repo.git(&["commit", "-m", "Commit selected pathspec"])
        .unwrap();
    let mut selected = repo.filename("selected name.txt");
    selected.assert_committed_lines(lines!["selected AI".ai()]);

    repo.git(&["add", "--", "residual.txt"]).unwrap();
    repo.git(&["commit", "-m", "Commit residual path"]).unwrap();
    let mut residual = repo.filename("residual.txt");
    residual.assert_committed_lines(lines!["residual AI".ai()]);
}

#[test]
fn test_git_add_dry_run_does_not_stage_or_discard_ai_attribution() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("dry.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut dry = repo.filename("dry.txt");
    dry.set_contents_no_stage(lines!["AI edit".ai()]);

    repo.git(&["add", "--dry-run", "--", "dry.txt"]).unwrap();
    assert!(repo.git_og(&["diff", "--cached", "--quiet"]).is_ok());
    repo.git(&["add", "--", "dry.txt"]).unwrap();
    repo.git(&["commit", "-m", "Commit after dry run"]).unwrap();

    let mut dry = repo.filename("dry.txt");
    dry.assert_committed_lines(lines!["AI edit".ai()]);
}
