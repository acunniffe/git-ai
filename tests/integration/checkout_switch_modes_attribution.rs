use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{TestRepo, default_branchname};
use git_ai::git::repository::find_repository_in_path;

fn repo_with_pending_ai() -> TestRepo {
    let repo = TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(vec!["seed".human()]);
    repo.stage_all_and_commit("initial").unwrap();
    seed.assert_lines_and_blame(vec!["seed".human()]);

    let mut pending = repo.filename("pending.txt");
    pending.set_contents(vec!["generated one".ai(), "generated two".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    repo
}

fn commit_and_assert_pending(repo: &TestRepo, message: &str) {
    repo.stage_all_and_commit(message).unwrap();
    let mut pending = repo.filename("pending.txt");
    pending.assert_lines_and_blame(vec!["generated one".ai(), "generated two".ai()]);
}

#[test]
fn checkout_force_create_reset_carries_pending_ai() {
    let repo = repo_with_pending_ai();
    repo.git(&["branch", "target"]).unwrap();
    repo.git(&["checkout", "-B", "target"]).unwrap();
    commit_and_assert_pending(&repo, "checkout -B");
}

#[test]
fn checkout_detach_carries_pending_ai() {
    let repo = repo_with_pending_ai();
    repo.git(&["checkout", "--detach", "HEAD"]).unwrap();
    commit_and_assert_pending(&repo, "detached commit");
}

#[test]
fn checkout_orphan_carries_pending_ai_to_root_commit() {
    let repo = repo_with_pending_ai();
    repo.git(&["checkout", "--orphan", "orphan-checkout"])
        .unwrap();
    commit_and_assert_pending(&repo, "orphan checkout root");
}

#[test]
fn failed_checkout_track_does_not_corrupt_pending_ai() {
    let repo = repo_with_pending_ai();
    assert!(
        repo.git(&["checkout", "--track", "missing/branch"])
            .is_err()
    );
    commit_and_assert_pending(&repo, "after failed checkout track");
}

#[test]
fn switch_create_carries_pending_ai() {
    let repo = repo_with_pending_ai();
    repo.git(&["switch", "-c", "switch-created"]).unwrap();
    commit_and_assert_pending(&repo, "switch -c");
}

#[test]
fn switch_force_create_reset_carries_pending_ai() {
    let repo = repo_with_pending_ai();
    repo.git(&["branch", "switch-target"]).unwrap();
    repo.git(&["switch", "-C", "switch-target"]).unwrap();
    commit_and_assert_pending(&repo, "switch -C");
}

#[test]
fn switch_detach_carries_pending_ai() {
    let repo = repo_with_pending_ai();
    repo.git(&["switch", "--detach", "HEAD"]).unwrap();
    commit_and_assert_pending(&repo, "switch detached commit");
}

#[test]
fn switch_without_pending_state_does_not_materialize_a_recovery_boundary() {
    let repo = TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(vec!["seed".human()]);
    repo.stage_all_and_commit("initial").unwrap();
    seed.assert_lines_and_blame(vec!["seed".human()]);

    repo.git(&["switch", "-c", "clean-switch"]).unwrap();
    repo.sync_daemon();
    let head = repo.git_og(&["rev-parse", "HEAD"]).unwrap();
    let repository = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    assert!(!repository.storage.has_working_log(head.trim()));
}

#[test]
fn switch_orphan_carries_pending_ai_to_root_commit() {
    let repo = repo_with_pending_ai();
    repo.git(&["switch", "--orphan", "orphan-switch"]).unwrap();
    commit_and_assert_pending(&repo, "orphan switch root");
}

#[test]
fn failed_switch_track_does_not_corrupt_pending_ai() {
    let repo = repo_with_pending_ai();
    assert!(repo.git(&["switch", "--track", "missing/branch"]).is_err());
    commit_and_assert_pending(&repo, "after failed switch track");
}

fn repo_with_remote_tracking_branch_and_pending_ai() -> (TestRepo, String) {
    let repo = repo_with_pending_ai();
    let remote_ref = "refs/remotes/origin/remote-feature";
    let repo_path = repo.path().to_string_lossy().to_string();
    repo.git_og(&["remote", "add", "origin", &repo_path])
        .unwrap();
    repo.git_og(&["update-ref", remote_ref, "HEAD"]).unwrap();
    (repo, "origin/remote-feature".to_string())
}

#[test]
fn checkout_track_success_carries_pending_ai() {
    let (repo, remote) = repo_with_remote_tracking_branch_and_pending_ai();
    repo.git(&["checkout", "--track", &remote]).unwrap();
    assert_eq!(repo.current_branch(), "remote-feature");
    commit_and_assert_pending(&repo, "checkout tracked branch");
}

#[test]
fn switch_track_success_carries_pending_ai() {
    let (repo, remote) = repo_with_remote_tracking_branch_and_pending_ai();
    repo.git(&["switch", "--track", &remote]).unwrap();
    assert_eq!(repo.current_branch(), "remote-feature");
    commit_and_assert_pending(&repo, "switch tracked branch");
}

fn repo_with_ai_feature_conflict() -> TestRepo {
    let repo = TestRepo::new();
    let mut conflict = repo.filename("conflict.txt");
    conflict.set_contents(vec!["base".human()]);
    repo.stage_all_and_commit("base").unwrap();
    conflict.assert_lines_and_blame(vec!["base".human()]);

    repo.git(&["checkout", "-b", "ai-feature"]).unwrap();
    conflict.set_contents(vec!["feature ai".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    repo.stage_all_and_commit("AI feature").unwrap();
    conflict.assert_lines_and_blame(vec!["feature ai".ai()]);

    repo.git(&["checkout", default_branchname()]).unwrap();
    conflict.set_contents(vec!["main human".human()]);
    repo.stage_all_and_commit("human main").unwrap();
    conflict.assert_lines_and_blame(vec!["main human".human()]);

    let mut carry = repo.filename("carry.txt");
    carry.set_contents(vec!["unrelated pending ai".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    repo.git_og(&["reset", "HEAD", "carry.txt"]).unwrap();
    repo.git(&["merge", "ai-feature"]).unwrap_err();
    assert_eq!(
        repo.git_og(&["show", ":2:conflict.txt"]).unwrap(),
        "main human"
    );
    assert_eq!(
        repo.git_og(&["show", ":3:conflict.txt"]).unwrap(),
        "feature ai"
    );
    repo
}

#[test]
fn checkout_ours_resolves_conflict_without_losing_unrelated_pending_ai() {
    let repo = repo_with_ai_feature_conflict();
    repo.git(&["checkout", "--ours", "--", "conflict.txt"])
        .unwrap();
    repo.stage_all_and_commit("resolve with ours").unwrap();

    let mut conflict = repo.filename("conflict.txt");
    conflict.assert_lines_and_blame(vec!["main human".human()]);
    let mut carry = repo.filename("carry.txt");
    carry.assert_lines_and_blame(vec!["unrelated pending ai".ai()]);
}

#[test]
fn checkout_theirs_restores_source_ai_and_preserves_unrelated_pending_ai() {
    let repo = repo_with_ai_feature_conflict();
    repo.git(&["checkout", "--theirs", "--", "conflict.txt"])
        .unwrap();
    assert_eq!(
        repo.read_file("conflict.txt").as_deref(),
        Some("feature ai")
    );
    repo.stage_all_and_commit("resolve with theirs").unwrap();

    let mut conflict = repo.filename("conflict.txt");
    conflict.assert_lines_and_blame(vec!["feature ai".ai()]);
    let mut carry = repo.filename("carry.txt");
    carry.assert_lines_and_blame(vec!["unrelated pending ai".ai()]);
}
