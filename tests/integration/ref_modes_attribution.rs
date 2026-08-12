use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;

fn repo_with_pending_ai() -> TestRepo {
    let repo = TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(vec!["seed".human()]);
    repo.stage_all_and_commit("initial").unwrap();
    let mut pending = repo.filename("pending.txt");
    pending.set_contents(vec!["pending ref-safe AI".ai()]);
    repo
}

fn commit_and_assert_pending(repo: &TestRepo, message: &str) {
    repo.stage_all_and_commit(message).unwrap();
    let mut pending = repo.filename("pending.txt");
    pending.assert_lines_and_blame(vec!["pending ref-safe AI".ai()]);
}

#[test]
fn lightweight_annotated_force_and_delete_tags_preserve_pending_ai() {
    let repo = repo_with_pending_ai();
    repo.git(&["tag", "lightweight"]).unwrap();
    repo.git(&["tag", "-a", "annotated", "-m", "annotation"])
        .unwrap();
    repo.git(&["tag", "-f", "lightweight", "HEAD"]).unwrap();
    repo.git(&["tag", "-d", "lightweight", "annotated"])
        .unwrap();
    commit_and_assert_pending(&repo, "after tags");
}

#[test]
fn auxiliary_notes_add_append_copy_remove_and_prune_preserve_authorship() {
    let repo = TestRepo::new();
    let mut source = repo.filename("source.txt");
    source.set_contents(vec!["source AI".ai()]);
    let source_commit = repo.stage_all_and_commit("source").unwrap().commit_sha;
    repo.git(&["commit", "--allow-empty", "-m", "target"])
        .unwrap();
    let target = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    repo.git(&["notes", "add", "-m", "auxiliary", &source_commit])
        .unwrap();
    repo.git(&["notes", "append", "-m", "more", &source_commit])
        .unwrap();
    repo.git(&["notes", "copy", "-f", &source_commit, &target])
        .unwrap();
    repo.git(&["notes", "remove", &target]).unwrap();
    repo.git(&["notes", "prune"]).unwrap();
    assert!(
        repo.git(&["notes", "--ref=ai", "show", &source_commit])
            .is_ok()
    );
    source.assert_lines_and_blame(vec!["source AI".ai()]);
}

#[test]
fn custom_symbolic_ref_set_and_delete_preserve_pending_ai() {
    let repo = repo_with_pending_ai();
    let target = format!("refs/heads/{}", repo.current_branch());
    repo.git(&["symbolic-ref", "refs/git-ai-test/alias", &target])
        .unwrap();
    repo.git(&["symbolic-ref", "--delete", "refs/git-ai-test/alias"])
        .unwrap();
    commit_and_assert_pending(&repo, "after auxiliary symbolic ref");
}

#[test]
fn symbolic_ref_head_to_different_tip_carries_pending_ai_to_new_base() {
    let repo = TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(vec!["seed".human()]);
    repo.stage_all_and_commit("initial").unwrap();
    repo.git(&["branch", "other"]).unwrap();
    let mut main_only = repo.filename("main-only.txt");
    main_only.set_contents(vec!["main human".human()]);
    repo.stage_all_and_commit("advance main").unwrap();

    let mut pending = repo.filename("pending.txt");
    pending.set_contents(vec!["pending ref-safe AI".ai()]);
    repo.git(&["symbolic-ref", "HEAD", "refs/heads/other"])
        .unwrap();
    assert_eq!(repo.current_branch(), "other");
    commit_and_assert_pending(&repo, "commit after symbolic HEAD move");
}

#[test]
fn replace_create_graft_and_delete_preserve_pending_ai_and_source_note() {
    let repo = TestRepo::new();
    let mut source = repo.filename("source.txt");
    source.set_contents(vec!["source AI".ai()]);
    let first = repo.stage_all_and_commit("source").unwrap().commit_sha;
    repo.git(&["commit", "--allow-empty", "-m", "replacement"])
        .unwrap();
    let second = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    repo.git(&["replace", &first, &second]).unwrap();
    repo.git(&["replace", "-d", &first]).unwrap();
    repo.git(&["replace", "--graft", &second]).unwrap();
    repo.git(&["replace", "-d", &second]).unwrap();

    let mut pending = repo.filename("pending.txt");
    pending.set_contents(vec!["pending ref-safe AI".ai()]);
    commit_and_assert_pending(&repo, "after replace lifecycle");
    source.assert_lines_and_blame(vec!["source AI".ai()]);
}
