use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

fn source_repo() -> TestRepo {
    let repo = TestRepo::new();
    let mut file = repo.filename("library.txt");
    file.set_contents(vec!["source library AI".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    repo.stage_all_and_commit("source initial").unwrap();
    repo
}

fn superproject_with_pending_ai() -> TestRepo {
    let repo = TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(vec!["super seed".human()]);
    repo.stage_all_and_commit("super initial").unwrap();
    let mut pending = repo.filename("pending.txt");
    pending.set_contents(vec!["pending superproject AI".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    repo
}

fn add_submodule(superproject: &TestRepo, source: &TestRepo) {
    superproject
        .git(&[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            source.path().to_str().unwrap(),
            "deps/library",
        ])
        .unwrap();
}

fn commit_and_assert_parent_pending(superproject: &TestRepo, message: &str, expected: &[&str]) {
    superproject.stage_all_and_commit(message).unwrap();
    let mut pending = superproject.filename("pending.txt");
    pending.assert_lines_and_blame(expected.iter().map(|line| (*line).ai()).collect());
}

#[test]
fn submodule_add_and_absorbgitdirs_preserve_superproject_pending_ai() {
    let source = source_repo();
    let superproject = superproject_with_pending_ai();

    add_submodule(&superproject, &source);
    superproject
        .git(&["submodule", "absorbgitdirs", "deps/library"])
        .unwrap();

    assert!(superproject.path().join("deps/library/.git").is_file());
    commit_and_assert_parent_pending(&superproject, "add submodule", &["pending superproject AI"]);
    let mut source_file = source.filename("library.txt");
    source_file.assert_lines_and_blame(vec!["source library AI".ai()]);
}

#[test]
fn submodule_deinit_update_init_round_trip_preserves_parent_and_source() {
    let source = source_repo();
    let superproject = superproject_with_pending_ai();
    add_submodule(&superproject, &source);
    superproject
        .stage_all_and_commit("record submodule")
        .unwrap();

    let mut pending = superproject.filename("pending.txt");
    pending.set_contents(vec![
        "pending superproject AI".ai(),
        "pending reinit AI".ai(),
    ]);
    superproject.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    superproject
        .git(&["submodule", "deinit", "-f", "--", "deps/library"])
        .unwrap();
    assert!(
        !superproject
            .path()
            .join("deps/library/library.txt")
            .exists()
    );
    superproject
        .git(&[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--",
            "deps/library",
        ])
        .unwrap();
    assert!(
        superproject
            .path()
            .join("deps/library/library.txt")
            .is_file()
    );

    commit_and_assert_parent_pending(
        &superproject,
        "after submodule reinit",
        &["pending superproject AI", "pending reinit AI"],
    );
    let mut source_file = source.filename("library.txt");
    source_file.assert_lines_and_blame(vec!["source library AI".ai()]);
}

#[test]
fn submodule_update_remote_moves_gitlink_without_losing_parent_pending_ai() {
    let source = source_repo();
    let superproject = superproject_with_pending_ai();
    add_submodule(&superproject, &source);
    superproject
        .stage_all_and_commit("record submodule")
        .unwrap();

    let mut source_file = source.filename("library.txt");
    source_file.set_contents(vec!["source library AI".ai(), "source update AI".ai()]);
    source.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    let source_tip = source
        .stage_all_and_commit("source update")
        .unwrap()
        .commit_sha;

    let mut pending = superproject.filename("pending.txt");
    pending.set_contents(vec![
        "pending superproject AI".ai(),
        "pending update AI".ai(),
    ]);
    superproject.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    superproject
        .git(&[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--remote",
            "--",
            "deps/library",
        ])
        .unwrap();
    let nested_tip = superproject
        .git_from_working_dir(
            &superproject.path().join("deps/library"),
            &["rev-parse", "HEAD"],
        )
        .unwrap();
    assert_eq!(nested_tip.trim(), source_tip);

    commit_and_assert_parent_pending(
        &superproject,
        "update submodule gitlink",
        &["pending superproject AI", "pending update AI"],
    );
    source_file.assert_lines_and_blame(vec!["source library AI".ai(), "source update AI".ai()]);
}

#[test]
fn nested_submodule_commit_is_attributed_in_nested_family_not_superproject() {
    let source = source_repo();
    let superproject = superproject_with_pending_ai();
    add_submodule(&superproject, &source);
    superproject
        .stage_all_and_commit("record submodule")
        .unwrap();

    let nested = superproject.path().join("deps/library");
    let mut pending = superproject.filename("pending.txt");
    pending.set_contents(vec![
        "pending superproject AI".ai(),
        "pending nested-isolation AI".ai(),
    ]);
    superproject.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    let nested_file = nested.join("nested.txt");
    fs::write(&nested_file, "nested AI\n").unwrap();
    superproject
        .git_ai_from_working_dir(
            &nested,
            &["checkpoint", "mock_ai", nested_file.to_str().unwrap()],
        )
        .unwrap();
    superproject
        .git_from_working_dir(&nested, &["add", "nested.txt"])
        .unwrap();
    superproject
        .git_from_working_dir(&nested, &["commit", "-m", "nested AI commit"])
        .unwrap();
    let nested_note = superproject
        .git_from_working_dir(&nested, &["notes", "--ref=ai", "show", "HEAD"])
        .unwrap();
    assert!(nested_note.contains("nested.txt"), "note: {nested_note}");

    commit_and_assert_parent_pending(
        &superproject,
        "record nested gitlink",
        &["pending superproject AI", "pending nested-isolation AI"],
    );
}
