use std::fs;

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;

fn lite_repo() -> TestRepo {
    TestRepo::new_with_daemon_env(&[("GIT_AI_LITE_MODE", "true")])
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

    repo.stage_all_and_commit("commit pending work").unwrap();
    pending.assert_committed_lines(crate::lines!["pending AI".ai()]);
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

    repo.stage_all_and_commit("commit pending work").unwrap();
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

    let mut pending = repo.filename("pending.txt");
    pending.set_contents_no_stage(crate::lines!["pending AI".ai()]);
    repo.git(&["revert", "--no-edit", &deletion]).unwrap();
    let destination = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert!(repo.read_authorship_note(&destination).is_none());

    repo.stage_all_and_commit("commit pending work").unwrap();
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
