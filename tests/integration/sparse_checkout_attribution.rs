use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;

fn repo_with_committed_ai_directories(worktree: bool) -> TestRepo {
    let repo = if worktree {
        TestRepo::new_worktree()
    } else {
        TestRepo::new()
    };
    let mut keep = repo.filename("keep/keep.txt");
    keep.set_contents(vec!["kept AI".ai()]);
    let mut hidden = repo.filename("hidden/hidden.txt");
    hidden.set_contents(vec!["hidden AI".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    repo.stage_all_and_commit("AI directories").unwrap();
    repo
}

#[test]
fn sparse_checkout_cone_set_add_reapply_disable_preserves_hidden_ai_note() {
    let repo = repo_with_committed_ai_directories(false);
    repo.git(&["sparse-checkout", "init", "--cone", "--sparse-index"])
        .unwrap();
    repo.git(&["sparse-checkout", "set", "keep"]).unwrap();
    assert!(!repo.path().join("hidden/hidden.txt").exists());

    repo.git(&["sparse-checkout", "add", "--skip-checks", "hidden"])
        .unwrap();
    let mut hidden = repo.filename("hidden/hidden.txt");
    hidden.assert_lines_and_blame(vec!["hidden AI".ai()]);

    repo.git(&["sparse-checkout", "set", "keep"]).unwrap();
    repo.git(&["sparse-checkout", "reapply"]).unwrap();
    assert!(!repo.path().join("hidden/hidden.txt").exists());
    repo.git(&["sparse-checkout", "disable"]).unwrap();
    hidden.assert_lines_and_blame(vec!["hidden AI".ai()]);
}

#[test]
fn sparse_checkout_no_cone_set_and_add_preserve_hidden_ai_note() {
    let repo = repo_with_committed_ai_directories(false);
    repo.git(&["sparse-checkout", "init", "--no-cone"]).unwrap();
    repo.git(&["sparse-checkout", "set", "--no-cone", "keep/"])
        .unwrap();
    assert!(!repo.path().join("hidden/hidden.txt").exists());
    assert!(
        repo.git(&["sparse-checkout", "add", "--no-cone", "hidden/hidden.txt"])
            .is_err(),
        "add does not accept the set/init mode flag and must not mutate patterns"
    );
    assert!(!repo.path().join("hidden/hidden.txt").exists());
    repo.git(&["sparse-checkout", "add", "hidden/hidden.txt"])
        .unwrap();
    let mut hidden = repo.filename("hidden/hidden.txt");
    hidden.assert_lines_and_blame(vec!["hidden AI".ai()]);
    repo.git(&["sparse-checkout", "disable"]).unwrap();
}

#[test]
fn sparse_checkout_stdin_and_sparse_index_preserve_notes_in_linked_worktree() {
    let repo = repo_with_committed_ai_directories(true);
    repo.git(&["sparse-checkout", "init", "--cone", "--sparse-index"])
        .unwrap();
    repo.git_with_stdin(
        &["sparse-checkout", "set", "--sparse-index", "--stdin"],
        b"keep\n",
    )
    .unwrap();
    assert!(!repo.path().join("hidden/hidden.txt").exists());
    repo.git(&["sparse-checkout", "reapply"]).unwrap();
    repo.git(&["sparse-checkout", "disable"]).unwrap();
    let mut hidden = repo.filename("hidden/hidden.txt");
    hidden.assert_lines_and_blame(vec!["hidden AI".ai()]);
}

#[test]
fn sparse_checkout_does_not_discard_dirty_checkpoint_in_excluded_directory() {
    let repo = repo_with_committed_ai_directories(false);
    repo.git(&["sparse-checkout", "init", "--cone"]).unwrap();
    repo.git(&["sparse-checkout", "set", "keep", "hidden"])
        .unwrap();
    let mut hidden = repo.filename("hidden/hidden.txt");
    hidden.set_contents(vec!["hidden AI".ai(), "pending AI".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    repo.git_og(&["reset", "HEAD", "hidden/hidden.txt"])
        .unwrap();

    repo.git(&["sparse-checkout", "set", "--cone", "keep"])
        .unwrap();
    assert!(
        repo.path().join("hidden/hidden.txt").exists(),
        "Git should retain a dirty path even when sparse rules exclude it"
    );
    repo.git(&["sparse-checkout", "reapply"]).unwrap();
    assert!(repo.path().join("hidden/hidden.txt").exists());

    repo.git(&["add", "--sparse", "hidden/hidden.txt"]).unwrap();
    repo.commit("commit retained dirty path").unwrap();
    hidden.assert_lines_and_blame(vec!["hidden AI".ai(), "pending AI".ai()]);
    repo.git(&["sparse-checkout", "reapply"]).unwrap();
    assert!(!repo.path().join("hidden/hidden.txt").exists());
    repo.git(&["sparse-checkout", "disable"]).unwrap();
    hidden.assert_lines_and_blame(vec!["hidden AI".ai(), "pending AI".ai()]);
}
