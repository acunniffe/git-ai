use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{TestRepo, default_branchname};

fn repo_with_pending_ai() -> TestRepo {
    let repo = TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(vec!["seed".human()]);
    repo.stage_all_and_commit("initial").unwrap();
    seed.assert_lines_and_blame(vec!["seed".human()]);
    let mut pending = repo.filename("pending.txt");
    pending.set_contents(vec!["branch-safe ai".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    repo
}

fn commit_and_assert_pending(repo: &TestRepo, message: &str) {
    repo.stage_all_and_commit(message).unwrap();
    let mut pending = repo.filename("pending.txt");
    pending.assert_lines_and_blame(vec!["branch-safe ai".ai()]);
}

#[test]
fn branch_create_force_reset_delete_and_recreate_preserve_pending_ai() {
    let repo = repo_with_pending_ai();
    repo.git(&["branch", "created"]).unwrap();
    repo.git(&["branch", "-f", "created", "HEAD"]).unwrap();
    repo.git(&["branch", "-d", "created"]).unwrap();
    repo.git(&["branch", "created", "HEAD"]).unwrap();
    repo.git(&["branch", "-D", "created"]).unwrap();
    commit_and_assert_pending(&repo, "after branch lifecycle");
}

#[test]
fn branch_multi_delete_preserves_pending_ai() {
    let repo = repo_with_pending_ai();
    repo.git(&["branch", "merged-one"]).unwrap();
    repo.git(&["branch", "merged-two"]).unwrap();
    repo.git(&["branch", "-d", "merged-one", "merged-two"])
        .unwrap();
    repo.git(&["branch", "forced-one"]).unwrap();
    repo.git(&["branch", "forced-two"]).unwrap();
    repo.git(&["branch", "-D", "forced-one", "forced-two"])
        .unwrap();
    commit_and_assert_pending(&repo, "after multi delete");
}

#[test]
fn branch_rename_current_and_explicit_other_preserve_pending_ai() {
    let repo = repo_with_pending_ai();
    repo.git(&["branch", "other-old"]).unwrap();
    repo.git(&["branch", "-m", "other-old", "other-new"])
        .unwrap();
    repo.git(&["branch", "-m", "current-renamed"]).unwrap();
    assert_eq!(repo.current_branch(), "current-renamed");
    commit_and_assert_pending(&repo, "after branch rename");
}

#[test]
fn branch_copy_current_explicit_and_force_preserve_pending_ai() {
    let repo = repo_with_pending_ai();
    let current = default_branchname();
    repo.git(&["branch", "-c", "current-copy"]).unwrap();
    assert_eq!(repo.current_branch(), current);
    repo.git(&["branch", "source-copy"]).unwrap();
    repo.git(&["branch", "-c", "source-copy", "explicit-copy"])
        .unwrap();
    repo.git(&["branch", "-C", "source-copy", "explicit-copy"])
        .unwrap();
    commit_and_assert_pending(&repo, "after branch copy");
}

#[test]
fn branch_tracking_and_upstream_only_mutations_preserve_pending_ai() {
    let repo = TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(vec!["seed".human()]);
    repo.stage_all_and_commit("initial").unwrap();
    seed.assert_lines_and_blame(vec!["seed".human()]);

    let main = default_branchname();
    let remote_ref = format!("refs/remotes/origin/{main}");
    let repo_path = repo.path().to_string_lossy().to_string();
    repo.git_og(&["remote", "add", "origin", &repo_path])
        .unwrap();
    repo.git_og(&["update-ref", &remote_ref, "HEAD"]).unwrap();

    let mut pending = repo.filename("pending.txt");
    pending.set_contents(vec!["branch-safe ai".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();

    let upstream = format!("origin/{main}");
    let attached = format!("--set-upstream-to={upstream}");
    repo.git(&["branch", &attached]).unwrap();
    repo.git(&["branch", "--unset-upstream"]).unwrap();
    repo.git(&["branch", "--set-upstream-to", &upstream])
        .unwrap();
    repo.git(&["branch", "--unset-upstream"]).unwrap();
    repo.git(&["branch", "--track", "tracked", &upstream])
        .unwrap();
    repo.git(&["branch", "--no-track", "untracked", &upstream])
        .unwrap();
    commit_and_assert_pending(&repo, "after upstream configuration");
}

#[test]
#[cfg(unix)]
fn shell_wrapper_waits_for_read_only_and_each_mutating_command() {
    let repo = TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(vec!["seed".human()]);
    repo.stage_all_and_commit("initial").unwrap();
    seed.assert_lines_and_blame(vec!["seed".human()]);

    repo.shell_git("false && {git} status --short || true")
        .unwrap();
    repo.shell_git("{git} status --short && {git} branch first && {git} branch second")
        .unwrap();

    assert!(
        repo.git_og(&["show-ref", "--verify", "refs/heads/first"])
            .is_ok()
    );
    assert!(
        repo.git_og(&["show-ref", "--verify", "refs/heads/second"])
            .is_ok()
    );
}
