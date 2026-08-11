use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

struct DivergedRemote {
    local: TestRepo,
    _upstream: TestRepo,
    local_tip: String,
}

fn diverged_remote() -> DivergedRemote {
    let (local, upstream) = TestRepo::new_with_remote();
    let mut seed = local.filename("seed.txt");
    seed.set_contents(vec!["seed".human()]);
    let mut pending = local.filename("pending.txt");
    pending.set_contents(vec!["pending base AI".ai()]);
    let initial = local.stage_all_and_commit("initial").unwrap().commit_sha;
    local.git(&["push", "-u", "origin", "HEAD"]).unwrap();

    let mut local_file = local.filename("local.txt");
    local_file.set_contents(vec!["local committed AI".ai()]);
    local.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    let local_tip = local.stage_all_and_commit("local AI").unwrap().commit_sha;

    local.git(&["reset", "--hard", &initial]).unwrap();
    let mut remote_file = local.filename("remote.txt");
    remote_file.set_contents(vec!["remote side".human()]);
    local.stage_all_and_commit("remote side").unwrap();
    let branch = local.current_branch();
    local
        .git(&["push", "--force", "origin", &format!("HEAD:{branch}")])
        .unwrap();
    local.git(&["reset", "--hard", &local_tip]).unwrap();

    DivergedRemote {
        local,
        _upstream: upstream,
        local_tip,
    }
}

#[test]
fn pull_no_rebase_merge_carries_committed_and_pending_ai() {
    let setup = diverged_remote();
    let local = setup.local;
    let mut pending = local.filename("pending.txt");
    pending.set_contents_no_stage(vec![
        "pending base AI".ai(),
        "pending across merge pull".ai(),
    ]);

    local
        .git(&["pull", "--no-rebase", "--no-edit", "origin", "main"])
        .unwrap();
    assert_eq!(
        local
            .git(&["rev-list", "--parents", "-n", "1", "HEAD"])
            .unwrap()
            .split_whitespace()
            .count(),
        3
    );
    local
        .stage_all_and_commit("pending after merge pull")
        .unwrap();

    let mut local_file = local.filename("local.txt");
    local_file.assert_lines_and_blame(vec!["local committed AI".ai()]);
    pending.assert_lines_and_blame(vec![
        "pending base AI".ai(),
        "pending across merge pull".ai(),
    ]);
}

#[test]
fn pull_ff_only_rejection_does_not_move_head_or_lose_pending_ai() {
    let setup = diverged_remote();
    let local = setup.local;
    let mut pending = local.filename("pending.txt");
    pending.set_contents_no_stage(vec![
        "pending base AI".ai(),
        "pending after ff-only reject".ai(),
    ]);

    assert!(local.git(&["pull", "--ff-only", "origin", "main"]).is_err());
    assert_eq!(
        local.git(&["rev-parse", "HEAD"]).unwrap().trim(),
        setup.local_tip
    );
    local.stage_all_and_commit("after rejected pull").unwrap();
    pending.assert_lines_and_blame(vec![
        "pending base AI".ai(),
        "pending after ff-only reject".ai(),
    ]);
}

#[test]
fn push_force_with_lease_and_delete_preserve_local_notes_and_publish_new_note() {
    let (local, upstream) = TestRepo::new_with_remote();
    let mut seed = local.filename("seed.txt");
    seed.set_contents(vec!["seed".human()]);
    local.stage_all_and_commit("initial").unwrap();
    let mut file = local.filename("force.txt");
    file.set_contents(vec!["first remote AI".ai()]);
    local.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    let first = local.stage_all_and_commit("first").unwrap().commit_sha;
    local.git(&["push", "-u", "origin", "HEAD"]).unwrap();

    let initial = local
        .git(&["rev-parse", "HEAD^"])
        .unwrap()
        .trim()
        .to_string();
    local.git(&["reset", "--hard", &initial]).unwrap();
    file = local.filename("force.txt");
    file.set_contents(vec!["replacement remote AI".ai()]);
    local.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    let replacement = local
        .stage_all_and_commit("replacement")
        .unwrap()
        .commit_sha;
    local
        .git(&["push", "--force-with-lease", "origin", "HEAD:main"])
        .unwrap();
    assert!(local.read_authorship_note(&first).is_some());
    assert!(
        local
            .read_authorship_note_in_git_dir(upstream.path(), &replacement)
            .is_some()
    );

    local
        .git(&["push", "origin", "HEAD:refs/heads/topic"])
        .unwrap();
    local.git(&["push", "origin", "--delete", "topic"]).unwrap();
    assert!(
        upstream
            .git(&["rev-parse", "--verify", "refs/heads/topic"])
            .is_err()
    );
    file.assert_lines_and_blame(vec!["replacement remote AI".ai()]);
}

#[test]
fn atomic_push_failure_updates_no_refs_and_keeps_pending_ai() {
    let (local, upstream) = TestRepo::new_with_remote();
    let mut file = local.filename("history.txt");
    file.set_contents(vec!["initial".human()]);
    let initial = local.stage_all_and_commit("initial").unwrap().commit_sha;
    local.git(&["push", "-u", "origin", "HEAD"]).unwrap();
    file.set_contents(vec!["initial".human(), "remote current".human()]);
    let remote_main = local
        .stage_all_and_commit("remote current")
        .unwrap()
        .commit_sha;
    local.git(&["push", "origin", "HEAD"]).unwrap();

    let mut pending = local.filename("pending.txt");
    pending.set_contents(vec!["pending after atomic reject".ai()]);
    local.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    assert!(
        local
            .git(&[
                "push",
                "--atomic",
                "origin",
                "HEAD:refs/heads/atomic-good",
                &format!("{initial}:refs/heads/main"),
            ])
            .is_err()
    );
    assert!(
        upstream
            .git(&["rev-parse", "--verify", "refs/heads/atomic-good"])
            .is_err()
    );
    assert_eq!(
        upstream
            .git(&["rev-parse", "refs/heads/main"])
            .unwrap()
            .trim(),
        remote_main
    );
    local.stage_all_and_commit("after atomic reject").unwrap();
    pending.assert_lines_and_blame(vec!["pending after atomic reject".ai()]);
}

#[test]
fn fetch_refspec_force_and_prune_preserve_pending_ai() {
    let (local, upstream) = TestRepo::new_with_remote();
    let mut seed = local.filename("seed.txt");
    seed.set_contents(vec!["seed".human()]);
    let initial = local.stage_all_and_commit("initial").unwrap().commit_sha;
    local.git(&["push", "-u", "origin", "HEAD"]).unwrap();
    let mut second = local.filename("second.txt");
    second.set_contents(vec!["second".human()]);
    let tip = local.stage_all_and_commit("second").unwrap().commit_sha;
    local.git(&["push", "origin", "HEAD"]).unwrap();

    upstream
        .git(&["update-ref", "refs/heads/topic", &tip])
        .unwrap();
    local
        .git(&[
            "fetch",
            "origin",
            "refs/heads/topic:refs/remotes/origin/custom",
        ])
        .unwrap();
    assert_eq!(
        local
            .git(&["rev-parse", "refs/remotes/origin/custom"])
            .unwrap()
            .trim(),
        tip
    );
    upstream
        .git(&["update-ref", "refs/heads/topic", &initial, &tip])
        .unwrap();
    local
        .git(&[
            "fetch",
            "--force",
            "origin",
            "refs/heads/topic:refs/remotes/origin/custom",
        ])
        .unwrap();
    assert_eq!(
        local
            .git(&["rev-parse", "refs/remotes/origin/custom"])
            .unwrap()
            .trim(),
        initial
    );

    upstream
        .git(&["update-ref", "refs/heads/prunable", &tip])
        .unwrap();
    local.git(&["fetch", "origin"]).unwrap();
    upstream
        .git(&["update-ref", "-d", "refs/heads/prunable"])
        .unwrap();
    local.git(&["fetch", "--prune", "origin"]).unwrap();
    assert!(
        local
            .git(&["rev-parse", "--verify", "refs/remotes/origin/prunable"])
            .is_err()
    );

    let mut pending = local.filename("pending.txt");
    pending.set_contents(vec!["pending across fetches".ai()]);
    local.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    local.stage_all_and_commit("after fetch modes").unwrap();
    pending.assert_lines_and_blame(vec!["pending across fetches".ai()]);
}

#[test]
fn normal_and_no_checkout_clone_fetch_source_authorship_notes() {
    let source = TestRepo::new();
    let mut file = source.filename("cloned.txt");
    file.set_contents(vec!["cloned source AI".ai()]);
    source.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    let source_commit = source.stage_all_and_commit("source").unwrap().commit_sha;

    let temp = tempfile::tempdir().unwrap();
    let normal_path = temp.path().join("normal");
    source
        .git(&[
            "clone",
            source.path().to_str().unwrap(),
            normal_path.to_str().unwrap(),
        ])
        .unwrap();
    let normal = TestRepo::new_at_path(&normal_path);
    assert!(normal.read_authorship_note(&source_commit).is_some());
    let mut cloned = normal.filename("cloned.txt");
    cloned.assert_lines_and_blame(vec!["cloned source AI".ai()]);

    let no_checkout_path = temp.path().join("no-checkout");
    source
        .git(&[
            "clone",
            "--no-checkout",
            source.path().to_str().unwrap(),
            no_checkout_path.to_str().unwrap(),
        ])
        .unwrap();
    assert!(!no_checkout_path.join("cloned.txt").exists());
    let no_checkout = TestRepo::new_at_path(&no_checkout_path);
    assert!(no_checkout.read_authorship_note(&source_commit).is_some());
    no_checkout
        .git(&["checkout", "HEAD", "--", "cloned.txt"])
        .unwrap();
    let mut cloned = no_checkout.filename("cloned.txt");
    cloned.assert_lines_and_blame(vec!["cloned source AI".ai()]);
}

#[test]
fn bare_and_shallow_clone_and_init_targets_are_routed_safely() {
    let source = TestRepo::new();
    let mut file = source.filename("history.txt");
    file.set_contents(vec!["one".ai()]);
    source.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    source.stage_all_and_commit("one").unwrap();
    file.set_contents(vec!["one".ai(), "two".ai()]);
    source.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    let tip = source.stage_all_and_commit("two").unwrap().commit_sha;

    let temp = tempfile::tempdir().unwrap();
    let bare_path = temp.path().join("bare.git");
    source
        .git(&[
            "clone",
            "--bare",
            source.path().to_str().unwrap(),
            bare_path.to_str().unwrap(),
        ])
        .unwrap();
    assert_eq!(
        source
            .git_og(&[
                "--git-dir",
                bare_path.to_str().unwrap(),
                "rev-parse",
                "HEAD"
            ])
            .unwrap()
            .trim(),
        tip
    );

    let shallow_path = temp.path().join("shallow");
    let source_url = format!("file://{}", source.path().display());
    source
        .git(&[
            "clone",
            "--depth=1",
            &source_url,
            shallow_path.to_str().unwrap(),
        ])
        .unwrap();
    assert!(shallow_path.join(".git/shallow").is_file());
    let shallow = TestRepo::new_at_path(&shallow_path);
    assert!(shallow.read_authorship_note(&tip).is_some());

    let init_path = temp.path().join("initialized");
    fs::create_dir_all(&init_path).unwrap();
    source
        .git_from_working_dir(&init_path, &["init", "--initial-branch=main"])
        .unwrap();
    assert!(init_path.join(".git/HEAD").is_file());
    let bare_init_path = temp.path().join("initialized-bare.git");
    source
        .git(&["init", "--bare", bare_init_path.to_str().unwrap()])
        .unwrap();
    assert!(bare_init_path.join("HEAD").is_file());
}
