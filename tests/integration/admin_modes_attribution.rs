use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;

fn repo_with_source_and_pending() -> (TestRepo, String) {
    let repo = TestRepo::new();
    let mut source = repo.filename("source.txt");
    source.set_contents(vec!["durable source AI".ai()]);
    let source_commit = repo.stage_all_and_commit("source").unwrap().commit_sha;
    source.assert_committed_lines(lines!["durable source AI".ai()]);
    let mut pending = repo.filename("pending.txt");
    pending.set_contents(vec!["pending through admin operation".ai()]);
    (repo, source_commit)
}

fn commit_and_assert_durability(repo: &TestRepo, source_commit: &str, message: &str) {
    repo.stage_all_and_commit(message).unwrap();
    let mut source = repo.filename("source.txt");
    source.assert_lines_and_blame(vec!["durable source AI".ai()]);
    let mut pending = repo.filename("pending.txt");
    pending.assert_lines_and_blame(vec!["pending through admin operation".ai()]);
    assert!(repo.read_authorship_note(source_commit).is_some());
}

#[test]
fn remote_and_local_config_lifecycle_preserve_authorship_state() {
    let (repo, source_commit) = repo_with_source_and_pending();
    let upstream = TestRepo::new_bare();
    let url = upstream.path().to_str().unwrap();

    repo.git(&["remote", "add", "backup", url]).unwrap();
    repo.git(&["remote", "rename", "backup", "archive"])
        .unwrap();
    repo.git(&["remote", "set-url", "archive", url]).unwrap();
    repo.git(&["remote", "remove", "archive"]).unwrap();

    repo.git(&["config", "attribution-test.mode", "first"])
        .unwrap();
    repo.git(&["config", "--add", "attribution-test.mode", "second"])
        .unwrap();
    assert!(
        repo.git(&["config", "--get-all", "attribution-test.mode"])
            .unwrap()
            .contains("second")
    );
    repo.git(&["config", "--unset-all", "attribution-test.mode"])
        .unwrap();

    commit_and_assert_durability(&repo, &source_commit, "after remote config");
}

#[test]
fn gc_repack_pack_refs_and_reflog_expire_preserve_notes_and_live_working_log() {
    let (repo, source_commit) = repo_with_source_and_pending();
    repo.git(&["tag", "durability-tag", &source_commit])
        .unwrap();
    repo.git(&["branch", "durability-branch", &source_commit])
        .unwrap();

    repo.git(&["gc", "--prune=now"]).unwrap();
    repo.git(&["repack", "-Ad"]).unwrap();
    repo.git(&["pack-refs", "--all", "--prune"]).unwrap();
    repo.git(&[
        "reflog",
        "expire",
        "--expire=now",
        "--expire-unreachable=now",
        "--all",
    ])
    .unwrap();

    assert_eq!(
        repo.git(&["rev-parse", "durability-tag"]).unwrap().trim(),
        source_commit
    );
    commit_and_assert_durability(&repo, &source_commit, "after object maintenance");
}

#[test]
fn maintenance_register_run_and_unregister_preserve_pending_ai() {
    let (repo, source_commit) = repo_with_source_and_pending();

    repo.git(&["maintenance", "register"]).unwrap();
    repo.git(&["maintenance", "run", "--task=commit-graph", "--task=gc"])
        .unwrap();
    repo.git(&["maintenance", "unregister"]).unwrap();

    commit_and_assert_durability(&repo, &source_commit, "after maintenance");
}
