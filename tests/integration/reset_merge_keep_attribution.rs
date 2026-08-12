use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

#[test]
fn test_reset_merge_discards_unwound_attribution_with_worktree_content() {
    let target = TestRepo::new();
    fs::write(target.path().join("merge-reset.txt"), "base\n").unwrap();
    let base = target.stage_all_and_commit("Base").unwrap();
    target.assert_file_committed_lines("merge-reset.txt", lines!["base".human()]);
    let mut file = target.filename("merge-reset.txt");
    file.set_contents(lines!["base", "discarded AI line".ai()]);
    target.stage_all_and_commit("AI commit").unwrap();
    target.assert_file_committed_lines(
        "merge-reset.txt",
        lines!["base".human(), "discarded AI line".ai()],
    );

    target
        .git(&["reset", "--merge", &base.commit_sha])
        .expect("actual reset --merge should succeed");
    assert_eq!(
        target.read_file("merge-reset.txt").as_deref(),
        Some("base\n")
    );

    fs::write(
        target.path().join("merge-reset.txt"),
        "base\ndiscarded AI line\n",
    )
    .unwrap();
    target
        .stage_all_and_commit("Human recreates discarded line")
        .unwrap();
    let mut file = target.filename("merge-reset.txt");
    file.assert_committed_lines(lines![
        "base".unattributed_human(),
        "discarded AI line".unattributed_human(),
    ]);
}

#[test]
fn test_reset_keep_preserves_local_ai_but_discards_unwound_commit_attribution() {
    let target = TestRepo::new();
    fs::write(target.path().join("base.txt"), "base\n").unwrap();
    let base = target.stage_all_and_commit("Base").unwrap();
    target.assert_file_committed_lines("base.txt", lines!["base".human()]);
    let mut discarded = target.filename("discarded.txt");
    discarded.set_contents(lines!["discarded committed AI".ai()]);
    target.stage_all_and_commit("AI commit to unwind").unwrap();
    target.assert_file_committed_lines("discarded.txt", lines!["discarded committed AI".ai()]);

    let mut kept = target.filename("kept-local.txt");
    kept.set_contents_no_stage(lines!["kept local AI".ai()]);
    target
        .git(&["reset", "--keep", &base.commit_sha])
        .expect("reset --keep should retain non-overlapping local work");
    assert!(target.read_file("discarded.txt").is_none());
    assert_eq!(
        target.read_file("kept-local.txt").as_deref(),
        Some("kept local AI")
    );

    target
        .stage_all_and_commit("Commit retained local work")
        .unwrap();
    let mut kept = target.filename("kept-local.txt");
    kept.assert_committed_lines(lines!["kept local AI".ai()]);

    fs::write(
        target.path().join("discarded.txt"),
        "discarded committed AI\n",
    )
    .unwrap();
    target
        .stage_all_and_commit("Human recreates discarded file")
        .unwrap();
    let mut discarded = target.filename("discarded.txt");
    discarded.assert_committed_lines(lines!["discarded committed AI".unattributed_human()]);
}
