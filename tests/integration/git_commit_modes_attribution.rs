use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

#[test]
fn test_git_commit_pathspec_from_file_nul_commits_subset_and_carries_residual() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("selected.txt"), "base selected\n").unwrap();
    fs::write(repo.path().join("residual.txt"), "base residual\n").unwrap();
    repo.stage_all_and_commit("Initial files").unwrap();
    let mut selected = repo.filename("selected.txt");
    selected.set_contents_no_stage(lines!["selected AI".ai()]);
    let mut residual = repo.filename("residual.txt");
    residual.set_contents_no_stage(lines!["residual AI".ai()]);
    repo.git(&["add", "-A"]).unwrap();

    let pathspec_dir = tempfile::tempdir().unwrap();
    let pathspec_file = pathspec_dir.path().join("commit-paths.nul");
    fs::write(&pathspec_file, b"selected.txt\0").unwrap();
    repo.git(&[
        "commit",
        &format!("--pathspec-from-file={}", pathspec_file.display()),
        "--pathspec-file-nul",
        "-m",
        "Commit selected path",
    ])
    .unwrap();
    let mut selected = repo.filename("selected.txt");
    selected.assert_committed_lines(lines!["selected AI".ai()]);
    assert_eq!(
        repo.git_og(&["show", "HEAD:residual.txt"]).unwrap(),
        "base residual\n"
    );

    repo.git(&["commit", "-m", "Commit residual path"]).unwrap();
    let mut residual = repo.filename("residual.txt");
    residual.assert_committed_lines(lines!["residual AI".ai()]);
}

#[test]
fn test_git_commit_all_commits_tracked_ai_and_preserves_untracked_ai() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("tracked.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut tracked = repo.filename("tracked.txt");
    tracked.set_contents_no_stage(lines!["tracked AI".ai()]);
    let mut untracked = repo.filename("untracked.txt");
    untracked.set_contents_no_stage(lines!["untracked AI".ai()]);

    repo.git(&["commit", "-a", "-m", "Commit tracked only"])
        .unwrap();
    let mut tracked = repo.filename("tracked.txt");
    tracked.assert_committed_lines(lines!["tracked AI".ai()]);
    assert!(repo.read_file("untracked.txt").is_some());

    repo.git(&["add", "--", "untracked.txt"]).unwrap();
    repo.git(&["commit", "-m", "Commit residual untracked"])
        .unwrap();
    let mut untracked = repo.filename("untracked.txt");
    untracked.assert_committed_lines(lines!["untracked AI".ai()]);
}

#[test]
fn test_git_commit_fixup_and_squash_preserve_new_ai_lines() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("fixup.txt"), "base\n").unwrap();
    let base = repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("fixup.txt");
    file.set_contents_no_stage(lines!["first AI".ai()]);
    repo.git(&["add", "--", "fixup.txt"]).unwrap();
    repo.git(&["commit", &format!("--fixup={}", base.commit_sha)])
        .unwrap();
    let mut file = repo.filename("fixup.txt");
    file.assert_committed_lines(lines!["first AI".ai()]);

    file.set_contents_no_stage(lines!["first AI".ai(), "second AI".ai()]);
    repo.git(&["add", "--", "fixup.txt"]).unwrap();
    repo.git(&[
        "commit",
        &format!("--squash={}", base.commit_sha),
        "-m",
        "squash body",
    ])
    .unwrap();
    let mut file = repo.filename("fixup.txt");
    file.assert_committed_lines(lines!["first AI".ai(), "second AI".ai()]);
}

#[test]
fn test_git_commit_allow_empty_carries_unstaged_ai_to_next_commit() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut later = repo.filename("later.txt");
    later.set_contents_no_stage(lines!["AI after empty boundary".ai()]);

    repo.git(&["commit", "--allow-empty", "-m", "Empty boundary"])
        .unwrap();
    assert!(repo.read_file("later.txt").is_some());
    repo.git(&["add", "--", "later.txt"]).unwrap();
    repo.git(&["commit", "-m", "Commit carried AI"]).unwrap();

    let mut later = repo.filename("later.txt");
    later.assert_committed_lines(lines!["AI after empty boundary".ai()]);
}

#[test]
fn test_git_commit_reuse_and_reedit_message_modes_preserve_ai_content() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("reuse.txt"), "base\n").unwrap();
    let base = repo.stage_all_and_commit("Reusable message").unwrap();

    let mut file = repo.filename("reuse.txt");
    file.set_contents_no_stage(lines!["AI via -C".ai()]);
    repo.git(&["add", "--", "reuse.txt"]).unwrap();
    repo.git(&["commit", "-C", &base.commit_sha]).unwrap();

    let mut file = repo.filename("reuse.txt");
    file.set_contents_no_stage(lines!["AI via -C".ai(), "AI via reuse".ai()]);
    repo.git(&["add", "--", "reuse.txt"]).unwrap();
    repo.git(&["commit", &format!("--reuse-message={}", base.commit_sha)])
        .unwrap();

    let mut file = repo.filename("reuse.txt");
    file.set_contents_no_stage(lines![
        "AI via -C".ai(),
        "AI via reuse".ai(),
        "AI via -c".ai(),
    ]);
    repo.git(&["add", "--", "reuse.txt"]).unwrap();
    repo.git_with_env(
        &["commit", "-c", &base.commit_sha],
        &[("GIT_EDITOR", "true")],
        None,
    )
    .unwrap();

    let mut file = repo.filename("reuse.txt");
    file.assert_committed_lines(lines![
        "AI via -C".ai(),
        "AI via reuse".ai(),
        "AI via -c".ai(),
    ]);
}
