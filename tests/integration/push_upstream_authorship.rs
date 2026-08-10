use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;

#[test]
fn push_with_set_upstream_flag_pushes_authorship_notes() {
    let (local, upstream) = TestRepo::new_with_remote();

    let mut file = local.filename("upstream_feature.rs");
    file.set_contents(vec!["fn upstream_feature() {}".ai()]);
    let commit = local
        .stage_all_and_commit("add upstream feature")
        .expect("commit should succeed");
    file.assert_committed_lines(vec!["fn upstream_feature() {}".ai()]);

    local
        .git(&["push", "-u", "origin", "HEAD"])
        .expect("push with -u should succeed");
    let note = local.wait_for_authorship_note_in_git_dir(upstream.path(), &commit.commit_sha);
    assert!(
        note.is_some(),
        "expected authorship notes to be pushed to the remote when using -u"
    );
}

#[test]
fn push_after_branch_set_upstream_pushes_authorship_notes() {
    let (local, upstream) = TestRepo::new_with_remote();

    let mut file = local.filename("upstream_branch.rs");
    file.set_contents(vec!["fn initial() {}".ai()]);
    local
        .stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");
    file.assert_committed_lines(vec!["fn initial() {}".ai()]);

    local
        .git(&["push", "origin", "HEAD"])
        .expect("initial push should succeed");

    let branch = local.current_branch();
    local
        .git(&["branch", "-u", &format!("origin/{}", branch)])
        .expect("branch -u should succeed");

    file.set_contents(vec!["fn initial() {}".ai(), "fn follow_up() {}".ai()]);
    let follow_up = local
        .stage_all_and_commit("follow-up commit")
        .expect("follow-up commit should succeed");
    file.assert_committed_lines(vec!["fn initial() {}".ai(), "fn follow_up() {}".ai()]);

    local
        .git(&["push"])
        .expect("push with configured upstream should succeed");
    let note = local.wait_for_authorship_note_in_git_dir(upstream.path(), &follow_up.commit_sha);
    assert!(
        note.is_some(),
        "expected authorship notes to be pushed after setting upstream with git branch -u"
    );
}

crate::reuse_tests_in_worktree!(
    push_with_set_upstream_flag_pushes_authorship_notes,
    push_after_branch_set_upstream_pushes_authorship_notes,
);
