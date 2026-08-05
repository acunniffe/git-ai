use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{TestRepo, real_git_executable};

fn lite_repo() -> TestRepo {
    TestRepo::new_with_daemon_env(&[("GIT_AI_LITE_MODE", "true")])
}

fn working_log_dir(repo: &TestRepo, commit: &str) -> std::path::PathBuf {
    repo.path()
        .join(".git")
        .join("ai")
        .join("working_logs")
        .join(commit)
}

fn traced_git_with_stdin(repo: &TestRepo, args: &[&str], stdin: &str) {
    repo.sync_daemon();

    let mut command = Command::new(real_git_executable());
    command.arg("-C").arg(repo.path()).args(args);
    command.env("HOME", repo.test_home_path());
    command.env(
        "GIT_CONFIG_GLOBAL",
        repo.test_home_path().join(".gitconfig"),
    );
    command.env("XDG_CONFIG_HOME", repo.test_home_path().join(".config"));
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env(
        "GIT_TRACE2_EVENT",
        git_ai::daemon::DaemonConfig::trace2_event_target_for_path(
            &repo.daemon_trace_socket_path(),
        ),
    );
    command.env(
        "GIT_TRACE2_EVENT_NESTING",
        std::env::var("GIT_AI_TEST_TRACE2_NESTING").unwrap_or_else(|_| "0".to_string()),
    );
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to run traced git {args:?}: {error}"));
    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(stdin.as_bytes())
        .expect("write traced git stdin");
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("failed to wait for traced git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "traced git {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    repo.sync_daemon();
}

#[test]
fn test_lite_mode_skips_rebase_notes_but_tracks_the_next_commit() {
    let repo = lite_repo();
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    base.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature = repo.filename("feature.txt");
    feature.set_contents(crate::lines!["feature AI".ai()]);
    let original_feature = repo.stage_all_and_commit("feature").unwrap().commit_sha;
    feature.assert_committed_lines(crate::lines!["feature AI".ai()]);
    assert!(repo.read_authorship_note(&original_feature).is_some());

    repo.git(&["checkout", &main]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main"]);
    repo.stage_all_and_commit("advance main").unwrap();
    main_file.assert_committed_lines(crate::lines!["main".human()]);

    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main]).unwrap();
    let rebased_feature = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(rebased_feature, original_feature);
    assert!(
        repo.read_authorship_note(&rebased_feature).is_none(),
        "lite mode must not rewrite the source note onto the rebased commit"
    );
    assert!(
        repo.read_authorship_note(&original_feature).is_some(),
        "lite mode must leave the original source note intact"
    );
    feature.assert_committed_lines(crate::lines!["feature AI".human()]);

    feature.insert_at(1, crate::lines!["new AI after rebase".ai()]);
    let post_rebase_commit = repo.stage_all_and_commit("new work").unwrap().commit_sha;
    feature.assert_committed_lines(crate::lines!["feature AI".ai(), "new AI after rebase".ai(),]);
    assert!(repo.read_authorship_note(&post_rebase_commit).is_some());
}

#[test]
fn test_lite_mode_skips_amend_notes() {
    let repo = lite_repo();
    let mut file = repo.filename("amend.txt");
    file.set_contents(crate::lines!["original AI".ai()]);
    let original = repo.stage_all_and_commit("original").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["original AI".ai()]);

    file.insert_at(1, crate::lines!["amended AI".ai()]);
    repo.git(&["add", "amend.txt"]).unwrap();
    repo.git(&["commit", "--amend", "--no-edit"]).unwrap();
    let amended = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(amended, original);
    assert!(repo.read_authorship_note(&amended).is_none());
    assert!(repo.read_authorship_note(&original).is_some());
    file.assert_committed_lines(crate::lines!["original AI".human(), "amended AI".human(),]);
}

#[test]
fn test_lite_mode_preserves_uncommitted_ai_attribution_through_amend() {
    let repo = lite_repo();
    let mut committed = repo.filename("committed.txt");
    committed.set_contents(crate::lines!["committed"]);
    repo.stage_all_and_commit("base").unwrap();
    committed.assert_committed_lines(crate::lines!["committed".human()]);

    let mut pending = repo.filename("pending.txt");
    pending.set_contents_no_stage(crate::lines!["pending AI".ai()]);
    committed.insert_at(1, crate::lines!["amended"]);
    repo.git(&["add", "committed.txt"]).unwrap();
    repo.git(&["commit", "--amend", "--no-edit"]).unwrap();

    let amended = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert!(repo.read_authorship_note(&amended).is_none());
    committed.assert_committed_lines(crate::lines!["committed".human(), "amended".human(),]);

    repo.stage_all_and_commit("commit pending work").unwrap();
    committed.assert_committed_lines(crate::lines!["committed".human(), "amended".human(),]);
    pending.assert_committed_lines(crate::lines!["pending AI".ai()]);
}

#[test]
fn test_lite_mode_does_not_reapply_amended_attribution_on_the_next_commit() {
    let repo = lite_repo();
    let path = repo.path().join("amend-same-file.txt");

    fs::write(&path, "base\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let mut file = repo.filename("amend-same-file.txt");
    file.assert_committed_lines(crate::lines!["base".unattributed_human()]);

    fs::write(&path, "base\namended AI\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "amend-same-file.txt"])
        .unwrap();
    repo.git(&["add", "amend-same-file.txt"]).unwrap();
    repo.git(&["commit", "--amend", "--no-edit"]).unwrap();
    let amended = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert!(repo.read_authorship_note(&amended).is_none());
    file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "amended AI".unattributed_human(),
    ]);

    fs::write(&path, "base\namended AI\nlater AI\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "amend-same-file.txt"])
        .unwrap();
    repo.stage_all_and_commit("later work").unwrap();
    file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "amended AI".unattributed_human(),
        "later AI".ai(),
    ]);
}

#[test]
fn test_ci_rewrites_rebase_notes_with_a_lite_mode_daemon() {
    let repo = lite_repo();
    let mut base = repo.filename("ci-base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    base.assert_committed_lines(crate::lines!["base".human()]);
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature = repo.filename("ci-feature.txt");
    feature.set_contents(crate::lines!["feature AI".ai()]);
    let original_feature = repo.stage_all_and_commit("feature").unwrap().commit_sha;
    feature.assert_committed_lines(crate::lines!["feature AI".ai()]);

    repo.git(&["checkout", "main"]).unwrap();
    let mut main_file = repo.filename("ci-main.txt");
    main_file.set_contents(crate::lines!["main"]);
    let base_sha = repo
        .stage_all_and_commit("advance main")
        .unwrap()
        .commit_sha;
    main_file.assert_committed_lines(crate::lines!["main".human()]);

    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", "main"]).unwrap();
    let rebased_feature = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(rebased_feature, original_feature);
    assert!(
        repo.read_authorship_note(&rebased_feature).is_none(),
        "the lite daemon must skip automatic note rewriting"
    );

    let output = repo
        .git_ai(&[
            "ci",
            "local",
            "sync",
            "--previous-head-sha",
            &original_feature,
            "--base-ref",
            "main",
            "--base-sha",
            &base_sha,
            "--head-sha",
            &rebased_feature,
            "--skip-fetch-notes",
            "--skip-push",
        ])
        .expect("ci local sync should succeed in lite mode");
    assert!(
        output.contains("Local CI (sync): authorship rewritten successfully"),
        "expected explicit CI rewrite, got: {output}"
    );
    feature.assert_committed_lines(crate::lines!["feature AI".ai()]);
}

#[test]
fn test_lite_mode_skips_cherry_pick_notes() {
    let repo = lite_repo();
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    base.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "source"]).unwrap();
    let mut picked = repo.filename("picked.txt");
    picked.set_contents(crate::lines!["picked AI".ai()]);
    let source = repo.stage_all_and_commit("source").unwrap().commit_sha;
    picked.assert_committed_lines(crate::lines!["picked AI".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main"]);
    repo.stage_all_and_commit("advance main").unwrap();
    main_file.assert_committed_lines(crate::lines!["main".human()]);

    repo.git(&["cherry-pick", &source]).unwrap();
    let destination = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(destination, source);
    assert!(repo.read_authorship_note(&destination).is_none());
    assert!(repo.read_authorship_note(&source).is_some());
    picked.assert_committed_lines(crate::lines!["picked AI".human()]);
}

#[test]
fn test_lite_mode_preserves_uncommitted_ai_attribution_through_cherry_pick() {
    let repo = lite_repo();
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    base.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "source"]).unwrap();
    let mut picked = repo.filename("picked.txt");
    picked.set_contents(crate::lines!["picked"]);
    let source = repo.stage_all_and_commit("source").unwrap().commit_sha;
    picked.assert_committed_lines(crate::lines!["picked".human()]);

    repo.git(&["checkout", &main]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main"]);
    repo.stage_all_and_commit("advance main").unwrap();
    main_file.assert_committed_lines(crate::lines!["main".human()]);

    let mut pending = repo.filename("pending.txt");
    pending.set_contents_no_stage(crate::lines!["pending AI".ai()]);
    repo.git(&["cherry-pick", &source]).unwrap();
    let destination = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert!(repo.read_authorship_note(&destination).is_none());
    picked.assert_committed_lines(crate::lines!["picked".human()]);

    repo.stage_all_and_commit("commit pending work").unwrap();
    picked.assert_committed_lines(crate::lines!["picked".human()]);
    pending.assert_committed_lines(crate::lines!["pending AI".ai()]);
}

#[test]
fn test_lite_mode_skips_revert_notes() {
    let repo = lite_repo();
    let path = repo.path().join("revert.txt");

    fs::write(&path, "keep\nrestored AI\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "revert.txt"])
        .unwrap();
    repo.stage_all_and_commit("source AI").unwrap();
    let mut file = repo.filename("revert.txt");
    file.assert_committed_lines(crate::lines!["keep".ai(), "restored AI".ai()]);

    fs::write(&path, "keep\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "revert.txt"])
        .unwrap();
    let deletion = repo.stage_all_and_commit("delete AI").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["keep".ai()]);

    repo.git(&["revert", "--no-edit", &deletion]).unwrap();
    let reverted = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert!(repo.read_authorship_note(&reverted).is_none());
    file.assert_committed_lines(crate::lines!["keep".ai(), "restored AI".human(),]);
}

#[test]
fn test_lite_mode_preserves_uncommitted_ai_attribution_through_revert() {
    let repo = lite_repo();
    let mut reverted = repo.filename("reverted.txt");
    reverted.set_contents(crate::lines!["restore me"]);
    repo.stage_all_and_commit("base").unwrap();
    reverted.assert_committed_lines(crate::lines!["restore me".human()]);

    fs::remove_file(repo.path().join("reverted.txt")).unwrap();
    let deletion = repo.stage_all_and_commit("delete file").unwrap().commit_sha;
    assert!(!repo.path().join("reverted.txt").exists());

    let mut pending = repo.filename("pending.txt");
    pending.set_contents_no_stage(crate::lines!["pending AI".ai()]);
    repo.git(&["revert", "--no-edit", &deletion]).unwrap();
    let destination = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert!(repo.read_authorship_note(&destination).is_none());
    reverted.assert_committed_lines(crate::lines!["restore me".human()]);

    repo.stage_all_and_commit("commit pending work").unwrap();
    reverted.assert_committed_lines(crate::lines!["restore me".human()]);
    pending.assert_committed_lines(crate::lines!["pending AI".ai()]);
}

#[test]
fn test_lite_mode_skips_update_ref_restack_notes() {
    let repo = lite_repo();
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    base.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature = repo.filename("restack.txt");
    feature.set_contents(crate::lines!["restacked AI".ai()]);
    let original = repo.stage_all_and_commit("feature").unwrap().commit_sha;
    feature.assert_committed_lines(crate::lines!["restacked AI".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main"]);
    let new_parent = repo
        .stage_all_and_commit("advance main")
        .unwrap()
        .commit_sha;
    main_file.assert_committed_lines(crate::lines!["main".human()]);

    let tree = repo
        .git(&["rev-parse", &format!("{original}^{{tree}}")])
        .unwrap()
        .trim()
        .to_string();
    let restacked = repo
        .git(&["commit-tree", &tree, "-p", &new_parent, "-m", "restacked"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["update-ref", "refs/heads/feature", &restacked, &original])
        .unwrap();
    assert!(repo.read_authorship_note(&restacked).is_none());
    assert!(repo.read_authorship_note(&original).is_some());

    repo.git(&["checkout", "feature"]).unwrap();
    feature.assert_committed_lines(crate::lines!["restacked AI".human()]);
}

#[test]
fn test_lite_mode_does_not_move_working_log_for_unrelated_branch_update() {
    let repo = lite_repo();
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    let shared_tip = repo.stage_all_and_commit("base").unwrap().commit_sha;
    base.assert_committed_lines(crate::lines!["base".human()]);

    repo.git(&["checkout", "-b", "work"]).unwrap();
    let mut pending = repo.filename("pending.txt");
    pending.set_contents_no_stage(crate::lines!["pending AI".ai()]);

    let tree = repo
        .git(&["rev-parse", &format!("{shared_tip}^{{tree}}")])
        .unwrap()
        .trim()
        .to_string();
    let advanced_main = repo
        .git(&[
            "commit-tree",
            &tree,
            "-p",
            &shared_tip,
            "-m",
            "advance main",
        ])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["update-ref", "refs/heads/main", &advanced_main, &shared_tip])
        .unwrap();
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap().trim(), shared_tip);

    repo.stage_all_and_commit("commit pending work").unwrap();
    pending.assert_committed_lines(crate::lines!["pending AI".ai()]);
}

#[test]
fn test_lite_mode_moves_working_log_for_checked_out_fast_forward_update_ref() {
    let repo = lite_repo();
    let base_path = repo.path().join("base.txt");
    fs::write(&base_path, "base\n").unwrap();
    let old_tip = repo.stage_all_and_commit("base").unwrap().commit_sha;
    let mut base = repo.filename("base.txt");
    base.assert_committed_lines(crate::lines!["base".human()]);

    let pending_path = repo.path().join("pending.txt");
    fs::write(&pending_path, "pending AI\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "pending.txt"])
        .unwrap();

    let tree = repo
        .git(&["rev-parse", &format!("{old_tip}^{{tree}}")])
        .unwrap()
        .trim()
        .to_string();
    let new_tip = repo
        .git(&["commit-tree", &tree, "-p", &old_tip, "-m", "fast-forward"])
        .unwrap()
        .trim()
        .to_string();
    let branch = repo.current_branch();
    repo.git(&[
        "update-ref",
        &format!("refs/heads/{branch}"),
        &new_tip,
        &old_tip,
    ])
    .unwrap();

    assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap().trim(), new_tip);
    assert!(repo.read_authorship_note(&new_tip).is_none());
    base.assert_committed_lines(crate::lines!["base".human()]);

    repo.stage_all_and_commit("commit pending work").unwrap();
    base.assert_committed_lines(crate::lines!["base".human()]);
    let mut pending = repo.filename("pending.txt");
    pending.assert_committed_lines(crate::lines!["pending AI".ai()]);
}

#[test]
fn test_lite_mode_moves_working_log_for_multi_step_checked_out_update_ref() {
    let repo = lite_repo();
    let base_path = repo.path().join("multi-step-base.txt");
    fs::write(&base_path, "base\n").unwrap();
    let old_tip = repo.stage_all_and_commit("base").unwrap().commit_sha;
    let mut base = repo.filename("multi-step-base.txt");
    base.assert_committed_lines(crate::lines!["base".human()]);

    let pending_path = repo.path().join("multi-step-pending.txt");
    fs::write(&pending_path, "pending AI\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "multi-step-pending.txt"])
        .unwrap();
    repo.sync_daemon();
    assert!(working_log_dir(&repo, &old_tip).exists());

    let tree = repo
        .git(&["rev-parse", &format!("{old_tip}^{{tree}}")])
        .unwrap()
        .trim()
        .to_string();
    let middle_tip = repo
        .git(&["commit-tree", &tree, "-p", &old_tip, "-m", "middle"])
        .unwrap()
        .trim()
        .to_string();
    let new_tip = repo
        .git(&["commit-tree", &tree, "-p", &middle_tip, "-m", "new tip"])
        .unwrap()
        .trim()
        .to_string();
    traced_git_with_stdin(
        &repo,
        &["update-ref", "--stdin"],
        &format!(
            "start\nupdate HEAD {middle_tip} {old_tip}\nprepare\ncommit\n\
             start\nupdate HEAD {new_tip} {middle_tip}\nprepare\ncommit\n"
        ),
    );

    assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap().trim(), new_tip);
    assert!(!working_log_dir(&repo, &old_tip).exists());
    assert!(working_log_dir(&repo, &new_tip).exists());
    assert!(repo.read_authorship_note(&new_tip).is_none());
    base.assert_committed_lines(crate::lines!["base".human()]);

    repo.stage_all_and_commit("commit pending work").unwrap();
    base.assert_committed_lines(crate::lines!["base".human()]);
    let mut pending = repo.filename("multi-step-pending.txt");
    pending.assert_committed_lines(crate::lines!["pending AI".ai()]);
}

#[test]
fn test_lite_mode_does_not_move_working_log_for_fast_forward_merge() {
    let repo = lite_repo();
    let base_path = repo.path().join("base.txt");
    fs::write(&base_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    let base_tip = repo.stage_all_and_commit("base").unwrap().commit_sha;
    let mut base = repo.filename("base.txt");
    base.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let merged_path = repo.path().join("merged.txt");
    fs::write(&merged_path, "merged\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "merged.txt"])
        .unwrap();
    let feature_tip = repo.stage_all_and_commit("feature").unwrap().commit_sha;
    base.assert_committed_lines(crate::lines!["base".human()]);
    let mut merged = repo.filename("merged.txt");
    merged.assert_committed_lines(crate::lines!["merged".human()]);

    repo.git(&["checkout", &main]).unwrap();
    let pending_path = repo.path().join("pending.txt");
    fs::write(&pending_path, "pending AI\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "pending.txt"])
        .unwrap();
    repo.sync_daemon();
    assert!(working_log_dir(&repo, &base_tip).exists());

    repo.git(&["merge", "--ff-only", "feature"]).unwrap();
    assert_eq!(
        repo.git(&["rev-parse", "HEAD"]).unwrap().trim(),
        feature_tip
    );
    assert!(working_log_dir(&repo, &base_tip).exists());
    base.assert_committed_lines(crate::lines!["base".human()]);
    merged.assert_committed_lines(crate::lines!["merged".human()]);
}

#[test]
fn test_lite_mode_does_not_move_working_log_for_merge_commit() {
    let repo = lite_repo();
    let base_path = repo.path().join("base.txt");
    fs::write(&base_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let mut base = repo.filename("base.txt");
    base.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let merged_path = repo.path().join("merged.txt");
    fs::write(&merged_path, "merged\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "merged.txt"])
        .unwrap();
    repo.stage_all_and_commit("feature").unwrap();
    base.assert_committed_lines(crate::lines!["base".human()]);
    let mut merged = repo.filename("merged.txt");
    merged.assert_committed_lines(crate::lines!["merged".human()]);

    repo.git(&["checkout", &main]).unwrap();
    let main_path = repo.path().join("main.txt");
    fs::write(&main_path, "main\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    let pre_merge_tip = repo.stage_all_and_commit("main").unwrap().commit_sha;
    base.assert_committed_lines(crate::lines!["base".human()]);
    let mut main_file = repo.filename("main.txt");
    main_file.assert_committed_lines(crate::lines!["main".human()]);

    let pending_path = repo.path().join("pending.txt");
    fs::write(&pending_path, "pending AI\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "pending.txt"])
        .unwrap();
    repo.sync_daemon();
    assert!(working_log_dir(&repo, &pre_merge_tip).exists());

    repo.git(&["merge", "--no-ff", "feature", "-m", "merge feature"])
        .unwrap();
    let merge_tip = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(merge_tip, pre_merge_tip);
    assert!(working_log_dir(&repo, &pre_merge_tip).exists());
    assert!(!working_log_dir(&repo, &merge_tip).exists());
    base.assert_committed_lines(crate::lines!["base".human()]);
    main_file.assert_committed_lines(crate::lines!["main".human()]);
    merged.assert_committed_lines(crate::lines!["merged".human()]);
}

#[test]
fn test_lite_mode_leaves_conflict_stop_working_log_at_its_rebase_base() {
    let repo = lite_repo();
    let conflict_path = repo.path().join("conflict.txt");
    fs::write(&conflict_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict.txt"])
        .unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let mut conflict = repo.filename("conflict.txt");
    conflict.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(&conflict_path, "feature\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict.txt"])
        .unwrap();
    let original_feature = repo.stage_all_and_commit("feature").unwrap().commit_sha;
    conflict.assert_committed_lines(crate::lines!["feature".human()]);

    repo.git(&["checkout", &main]).unwrap();
    fs::write(&conflict_path, "main\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict.txt"])
        .unwrap();
    let onto = repo.stage_all_and_commit("main").unwrap().commit_sha;
    conflict.assert_committed_lines(crate::lines!["main".human()]);

    repo.git(&["checkout", "feature"]).unwrap();
    assert!(repo.git(&["rebase", &main]).is_err());
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap().trim(), onto);

    fs::write(&conflict_path, "resolved AI\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.txt"])
        .unwrap();
    let pending_path = repo.path().join("pending-during-rebase.txt");
    fs::write(&pending_path, "pending AI\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "pending-during-rebase.txt"])
        .unwrap();
    repo.sync_daemon();
    let resolution_log = repo.working_logs_for_base_commit(&onto);
    assert!(!resolution_log.read_all_checkpoints().unwrap().is_empty());

    repo.git(&["add", "conflict.txt"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();
    let rebased = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(rebased, original_feature);
    assert!(repo.read_authorship_note(&rebased).is_none());
    assert!(
        working_log_dir(&repo, &onto).exists(),
        "lite mode should leave the conflict-stop log scoped to its rebase base"
    );
    assert!(!resolution_log.read_all_checkpoints().unwrap().is_empty());
    conflict.assert_committed_lines(crate::lines!["resolved AI".human()]);
    assert_eq!(fs::read_to_string(pending_path).unwrap(), "pending AI\n");
}

#[test]
fn test_lite_mode_preserves_regular_squash_notes() {
    let repo = lite_repo();
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    base.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature = repo.filename("squash.txt");
    feature.set_contents(crate::lines!["squashed AI".ai()]);
    repo.stage_all_and_commit("feature").unwrap();
    feature.assert_committed_lines(crate::lines!["squashed AI".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    let squash = repo.stage_all_and_commit("squash").unwrap().commit_sha;
    feature.assert_committed_lines(crate::lines!["squashed AI".ai()]);
    assert!(repo.read_authorship_note(&squash).is_some());
}

#[test]
#[cfg(unix)]
fn test_lite_mode_skips_interactive_rebase_squash_notes() {
    use std::os::unix::fs::PermissionsExt;

    let repo = lite_repo();
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    base.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut first = repo.filename("first.txt");
    first.set_contents(crate::lines!["first AI".ai()]);
    repo.stage_all_and_commit("first").unwrap();
    first.assert_committed_lines(crate::lines!["first AI".ai()]);
    let mut second = repo.filename("second.txt");
    second.set_contents(crate::lines!["second AI".ai()]);
    repo.stage_all_and_commit("second").unwrap();
    second.assert_committed_lines(crate::lines!["second AI".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(crate::lines!["main"]);
    repo.stage_all_and_commit("advance main").unwrap();
    main_file.assert_committed_lines(crate::lines!["main".human()]);

    repo.git(&["checkout", "feature"]).unwrap();
    let editor = repo.path().join("squash-editor.sh");
    fs::write(&editor, "#!/bin/sh\nsed -i.bak '2s/^pick/squash/' \"$1\"\n").unwrap();
    let mut permissions = fs::metadata(&editor).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&editor, permissions).unwrap();
    repo.git_with_env(
        &["rebase", "-i", &main],
        &[
            ("GIT_SEQUENCE_EDITOR", editor.to_str().unwrap()),
            ("GIT_EDITOR", "true"),
        ],
        None,
    )
    .unwrap();

    let squashed = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert!(repo.read_authorship_note(&squashed).is_none());
    first.assert_committed_lines(crate::lines!["first AI".human()]);
    second.assert_committed_lines(crate::lines!["second AI".human()]);
}
