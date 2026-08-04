use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;

#[test]
fn reftable_commit_preserves_mixed_attribution_across_commits() {
    let repo = TestRepo::new_reftable();
    let mut file = repo.filename("mixed.txt");

    file.set_contents(lines!["Human base", "AI first".ai()]);
    repo.stage_all_and_commit("mixed root").unwrap();
    file.assert_committed_lines(lines!["Human base".human(), "AI first".ai()]);

    file.set_contents(lines![
        "Human base".human(),
        "AI first".ai(),
        "AI second".ai(),
    ]);
    repo.stage_all_and_commit("mixed follow-up").unwrap();
    file.assert_committed_lines(lines![
        "Human base".human(),
        "AI first".ai(),
        "AI second".ai(),
    ]);
}

#[test]
fn reftable_sha256_commit_preserves_mixed_attribution() {
    let repo = TestRepo::new_reftable_sha256();
    let mut file = repo.filename("sha256.txt");
    file.set_contents(lines!["Human SHA-256", "AI SHA-256".ai()]);
    repo.stage_all_and_commit("sha256 root").unwrap();
    file.assert_committed_lines(lines!["Human SHA-256".human(), "AI SHA-256".ai()]);

    file.insert_at(2, lines!["AI SHA-256 amend".ai()]);
    repo.git(&["add", "sha256.txt"]).unwrap();
    repo.git(&["commit", "--amend", "--no-edit"]).unwrap();
    file.assert_committed_lines(lines![
        "Human SHA-256".human(),
        "AI SHA-256".ai(),
        "AI SHA-256 amend".ai(),
    ]);
}

#[test]
fn reftable_amend_and_soft_reset_preserve_attribution() {
    let repo = TestRepo::new_reftable();
    let mut file = repo.filename("rewrite.txt");
    file.set_contents(lines!["Human root", "AI one".ai()]);
    let root = repo.stage_all_and_commit("root").unwrap();
    file.assert_committed_lines(lines!["Human root".human(), "AI one".ai()]);

    file.insert_at(2, lines!["AI two".ai()]);
    repo.stage_all_and_commit("second").unwrap();
    file.assert_committed_lines(lines!["Human root".human(), "AI one".ai(), "AI two".ai(),]);

    repo.git(&["reset", "--soft", &root.commit_sha]).unwrap();
    repo.commit("squashed").unwrap();
    file.assert_committed_lines(lines!["Human root".human(), "AI one".ai(), "AI two".ai(),]);

    file.insert_at(3, lines!["AI amended".ai()]);
    repo.git(&["add", "rewrite.txt"]).unwrap();
    repo.git(&["commit", "--amend", "--no-edit"]).unwrap();
    file.assert_committed_lines(lines![
        "Human root".human(),
        "AI one".ai(),
        "AI two".ai(),
        "AI amended".ai(),
    ]);
}

#[test]
fn reftable_checkout_switch_and_branch_history_preserve_attribution() {
    let repo = TestRepo::new_reftable();
    let mut file = repo.filename("branches.txt");
    file.set_contents(lines!["Human root", ""]);
    repo.stage_all_and_commit("root").unwrap();
    file.assert_committed_lines(lines!["Human root".human()]);

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, lines!["AI feature".ai()]);
    repo.stage_all_and_commit("feature").unwrap();
    file.assert_committed_lines(lines!["Human root".human(), "AI feature".ai()]);

    repo.git(&["switch", "main"]).unwrap();
    file = repo.filename("branches.txt");
    file.assert_committed_lines(lines!["Human root".human()]);
    repo.git(&["switch", "feature"]).unwrap();
    file = repo.filename("branches.txt");
    file.assert_committed_lines(lines!["Human root".human(), "AI feature".ai()]);
}

#[test]
fn reftable_stash_push_apply_pop_and_drop_preserve_attribution() {
    let repo = TestRepo::new_reftable();
    let mut file = repo.filename("stash.txt");
    file.set_contents(lines!["Human root", ""]);
    repo.stage_all_and_commit("root").unwrap();
    file.assert_committed_lines(lines!["Human root".human()]);

    file.insert_at(1, lines!["AI popped".ai()]);
    repo.git(&["stash", "push", "-m", "pop me"]).unwrap();
    repo.git(&["stash", "pop"]).unwrap();
    repo.stage_all_and_commit("popped").unwrap();
    file.assert_committed_lines(lines!["Human root".human(), "AI popped".ai()]);

    file.insert_at(2, lines!["AI applied".ai()]);
    repo.git(&["stash", "push", "-m", "apply me"]).unwrap();
    repo.git(&["stash", "apply", "stash@{0}"]).unwrap();
    repo.git(&["stash", "drop", "stash@{0}"]).unwrap();
    repo.stage_all_and_commit("applied").unwrap();
    file.assert_committed_lines(lines![
        "Human root".human(),
        "AI popped".ai(),
        "AI applied".ai(),
    ]);
}

#[test]
fn reftable_revert_and_multi_cherry_pick_preserve_attribution() {
    let repo = TestRepo::new_reftable();
    let mut file = repo.filename("history.txt");
    file.set_contents(lines!["Human root", ""]);
    repo.stage_all_and_commit("root").unwrap();
    file.assert_committed_lines(lines!["Human root".human()]);

    repo.git(&["checkout", "-b", "source"]).unwrap();
    file.insert_at(1, lines!["AI source one".ai()]);
    let source_one = repo.stage_all_and_commit("source one").unwrap();
    file.assert_committed_lines(lines!["Human root".human(), "AI source one".ai()]);
    file.insert_at(2, lines!["AI source two".ai()]);
    let source_two = repo.stage_all_and_commit("source two").unwrap();
    file.assert_committed_lines(lines![
        "Human root".human(),
        "AI source one".ai(),
        "AI source two".ai(),
    ]);

    repo.git(&["switch", "main"]).unwrap();
    repo.git(&[
        "cherry-pick",
        &source_one.commit_sha,
        &source_two.commit_sha,
    ])
    .unwrap();
    file = repo.filename("history.txt");
    repo.git(&["checkout", "--detach", "HEAD~1"]).unwrap();
    file.assert_committed_lines(lines!["Human root".human(), "AI source one".ai()]);
    repo.git(&["switch", "main"]).unwrap();
    file.assert_committed_lines(lines![
        "Human root".human(),
        "AI source one".ai(),
        "AI source two".ai(),
    ]);

    repo.git(&["revert", "--no-edit", "HEAD"]).unwrap();
    file = repo.filename("history.txt");
    file.assert_committed_lines(lines!["Human root".human(), "AI source one".ai()]);
}

#[test]
fn reftable_rebase_and_update_ref_stdin_preserve_attribution() {
    let repo = TestRepo::new_reftable();
    let mut file = repo.filename("rebase.txt");
    file.set_contents(lines!["Human root", ""]);
    let root = repo.stage_all_and_commit("root").unwrap();
    file.assert_committed_lines(lines!["Human root".human()]);

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, lines!["AI feature".ai()]);
    repo.stage_all_and_commit("feature").unwrap();
    file.assert_committed_lines(lines!["Human root".human(), "AI feature".ai()]);

    repo.git(&["switch", "main"]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(lines!["Human main"]);
    repo.stage_all_and_commit("main advance").unwrap();
    main_file.assert_committed_lines(lines!["Human main".human()]);

    repo.git(&["switch", "feature"]).unwrap();
    repo.git(&["rebase", "main"]).unwrap();
    file = repo.filename("rebase.txt");
    file.assert_committed_lines(lines!["Human root".human(), "AI feature".ai()]);
    main_file = repo.filename("main.txt");
    main_file.assert_committed_lines(lines!["Human main".human()]);

    let rebased = repo.git(&["rev-parse", "HEAD"]).unwrap();
    repo.git_with_stdin(
        &["update-ref", "--stdin"],
        format!("update refs/heads/saved {}\n", rebased.trim()).as_bytes(),
    )
    .unwrap();
    repo.git(&["update-ref", "refs/heads/saved", &root.commit_sha])
        .unwrap();
    repo.git(&["switch", "saved"]).unwrap();
    file = repo.filename("rebase.txt");
    file.assert_committed_lines(lines!["Human root".human()]);
}

#[test]
fn reftable_linked_worktree_uses_its_own_head_log() {
    let repo = TestRepo::new_reftable_worktree();
    let mut file = repo.filename("linked.txt");
    file.set_contents(lines!["Human linked", "AI linked".ai()]);
    repo.stage_all_and_commit("linked root").unwrap();
    file.assert_committed_lines(lines!["Human linked".human(), "AI linked".ai()]);

    file.insert_at(2, lines!["AI linked follow-up".ai()]);
    repo.stage_all_and_commit("linked follow-up").unwrap();
    file.assert_committed_lines(lines![
        "Human linked".human(),
        "AI linked".ai(),
        "AI linked follow-up".ai(),
    ]);
}

#[test]
fn migrating_between_files_and_reftable_keeps_cursor_and_checkpoint_state() {
    let repo = TestRepo::new();
    let mut file = repo.filename("migration.txt");
    file.set_contents(lines!["Human files", "AI files".ai()]);
    repo.stage_all_and_commit("files root").unwrap();
    file.assert_committed_lines(lines!["Human files".human(), "AI files".ai()]);

    repo.git(&["refs", "migrate", "--ref-format=reftable"])
        .unwrap();
    file.insert_at(2, lines!["AI reftable".ai()]);
    repo.stage_all_and_commit("after reftable migration")
        .unwrap();
    file.assert_committed_lines(lines![
        "Human files".human(),
        "AI files".ai(),
        "AI reftable".ai(),
    ]);

    repo.git(&["refs", "migrate", "--ref-format=files"])
        .unwrap();
    file.insert_at(3, lines!["AI files again".ai()]);
    repo.stage_all_and_commit("after files migration").unwrap();
    file.assert_committed_lines(lines![
        "Human files".human(),
        "AI files".ai(),
        "AI reftable".ai(),
        "AI files again".ai(),
    ]);
}

#[test]
fn reftable_compaction_and_log_expiry_do_not_stale_cached_cursors() {
    let repo = TestRepo::new_reftable();
    let mut file = repo.filename("compact.txt");
    file.set_contents(lines!["Human compact", ""]);
    repo.stage_all_and_commit_with_env(
        "compact root",
        &[("GIT_TEST_REFTABLE_AUTOCOMPACTION", "0")],
    )
    .unwrap();
    file.assert_committed_lines(lines!["Human compact".human()]);

    file.insert_at(1, lines!["AI before compaction".ai()]);
    repo.stage_all_and_commit_with_env(
        "before compaction",
        &[("GIT_TEST_REFTABLE_AUTOCOMPACTION", "0")],
    )
    .unwrap();
    file.assert_committed_lines(lines!["Human compact".human(), "AI before compaction".ai(),]);

    repo.git(&["refs", "optimize"]).unwrap();
    repo.git(&["reflog", "expire", "--expire=all", "--all"])
        .unwrap();
    file.insert_at(2, lines!["AI after expiry".ai()]);
    repo.stage_all_and_commit("after expiry")
        .unwrap_or_else(|error| panic!("{error}\n{}", repo.daemon_stderr_contents()));
    file.assert_committed_lines(lines![
        "Human compact".human(),
        "AI before compaction".ai(),
        "AI after expiry".ai(),
    ]);
}

#[test]
fn reftable_pull_rebase_preserves_local_ai_commit() {
    let (repo, upstream) = TestRepo::new_with_remote();
    let mut file = repo.filename("pull.txt");
    file.set_contents(lines!["Human root", ""]);
    let root = repo.stage_all_and_commit("root").unwrap();
    file.assert_committed_lines(lines!["Human root".human()]);
    repo.git(&["push", "-u", "origin", "main"]).unwrap();
    repo.git(&["refs", "migrate", "--ref-format=reftable"])
        .unwrap();

    file.insert_at(1, lines!["AI local".ai()]);
    repo.stage_all_and_commit("local AI").unwrap();
    file.assert_committed_lines(lines!["Human root".human(), "AI local".ai()]);

    let tree = upstream
        .git(&["rev-parse", "refs/heads/main^{tree}"])
        .unwrap();
    let remote_commit = upstream
        .git(&[
            "-c",
            "user.name=Remote User",
            "-c",
            "user.email=remote@example.com",
            "commit-tree",
            tree.trim(),
            "-p",
            &root.commit_sha,
            "-m",
            "remote advance",
        ])
        .unwrap();
    upstream
        .git(&[
            "update-ref",
            "refs/heads/main",
            remote_commit.trim(),
            &root.commit_sha,
        ])
        .unwrap();

    repo.git(&["pull", "--rebase", "origin", "main"]).unwrap();
    file = repo.filename("pull.txt");
    file.assert_committed_lines(lines!["Human root".human(), "AI local".ai()]);
}
