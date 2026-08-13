use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

fn seeded_repo() -> TestRepo {
    let repo = TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(vec!["seed".human()]);
    repo.stage_all_and_commit("initial").unwrap();
    repo
}

fn stage_ai(repo: &TestRepo, path: &str, contents: &str) {
    let mut file = repo.filename(path);
    file.set_contents(vec![contents.ai()]);
}

fn assert_ai(repo: &TestRepo, path: &str, contents: &str) {
    let mut file = repo.filename(path);
    file.assert_lines_and_blame(vec![contents.ai()]);
}

#[test]
fn global_c_config_and_explicit_git_dir_work_tree_commit_to_exact_repo() {
    let repo = seeded_repo();

    stage_ai(&repo, "config-axis.txt", "global config axis AI");
    repo.git(&[
        "-c",
        "commit.cleanup=strip",
        "commit",
        "-m",
        "global config axis",
    ])
    .unwrap();
    assert_ai(&repo, "config-axis.txt", "global config axis AI");

    stage_ai(&repo, "dash-c-axis.txt", "global dash C axis AI");
    repo.git(&[
        "-C",
        repo.path().to_str().unwrap(),
        "commit",
        "-m",
        "global dash C axis",
    ])
    .unwrap();
    assert_ai(&repo, "dash-c-axis.txt", "global dash C axis AI");

    stage_ai(&repo, "git-dir-axis.txt", "git dir work tree axis AI");
    let git_dir = repo.path().join(".git");
    repo.git(&[
        &format!("--git-dir={}", git_dir.display()),
        &format!("--work-tree={}", repo.path().display()),
        "commit",
        "-m",
        "git dir work tree axis",
    ])
    .unwrap();
    assert_ai(&repo, "git-dir-axis.txt", "git dir work tree axis AI");
}

#[test]
fn nested_parent_and_sibling_cwds_route_completion_and_attribution_to_target() {
    let target = seeded_repo();
    let sibling = seeded_repo();
    let nested = target.path().join("nested/dir");
    fs::create_dir_all(&nested).unwrap();

    stage_ai(&target, "nested-axis.txt", "nested cwd axis AI");
    target
        .git_from_working_dir(&nested, &["commit", "-m", "nested cwd axis"])
        .unwrap();
    assert_ai(&target, "nested-axis.txt", "nested cwd axis AI");

    stage_ai(&target, "parent-axis.txt", "parent cwd axis AI");
    target
        .git_from_working_dir(
            target.path().parent().unwrap(),
            &[
                "-C",
                target.path().to_str().unwrap(),
                "commit",
                "-m",
                "parent cwd axis",
            ],
        )
        .unwrap();
    assert_ai(&target, "parent-axis.txt", "parent cwd axis AI");

    stage_ai(&target, "sibling-axis.txt", "sibling cwd axis AI");
    target
        .git_from_working_dir(
            sibling.path(),
            &[
                "-C",
                target.path().to_str().unwrap(),
                "commit",
                "-m",
                "sibling cwd axis",
            ],
        )
        .unwrap();
    assert_ai(&target, "sibling-axis.txt", "sibling cwd axis AI");
    assert!(
        sibling
            .git(&["log", "-1", "--format=%s"])
            .unwrap()
            .contains("initial")
    );
}

#[test]
#[cfg(unix)]
fn env_timeout_command_nohup_and_nested_shell_wrappers_are_traced() {
    let repo = seeded_repo();
    for (path, contents, script) in [
        (
            "env-axis.txt",
            "env wrapper AI",
            "env {git} commit -m env-wrapper",
        ),
        (
            "timeout-axis.txt",
            "timeout wrapper AI",
            "timeout 20 {git} commit -m timeout-wrapper",
        ),
        (
            "command-axis.txt",
            "command wrapper AI",
            "command {git} commit -m command-wrapper",
        ),
        (
            "nohup-axis.txt",
            "nohup wrapper AI",
            "nohup {git} commit -m nohup-wrapper >/dev/null 2>&1",
        ),
        (
            "nested-shell-axis.txt",
            "nested shell wrapper AI",
            "sh -c '{git} commit -m nested-shell-wrapper'",
        ),
    ] {
        stage_ai(&repo, path, contents);
        repo.shell_git(script).unwrap();
        assert_ai(&repo, path, contents);
    }
}

#[test]
#[cfg(unix)]
fn conditional_pipeline_stdin_and_background_completion_are_traced() {
    let repo = seeded_repo();

    stage_ai(&repo, "conditional-axis.txt", "conditional wrapper AI");
    repo.shell_git("test -f conditional-axis.txt && {git} commit -m conditional-wrapper")
        .unwrap();
    assert_ai(&repo, "conditional-axis.txt", "conditional wrapper AI");

    stage_ai(&repo, "pipeline-pending.txt", "pipeline pending AI");
    repo.shell_git(
        "printf 'create refs/heads/pipeline-axis %s\\n' \"$({git} rev-parse HEAD)\" | {git} update-ref --stdin",
    )
    .unwrap();
    repo.stage_all_and_commit("after update-ref stdin pipeline")
        .unwrap();
    assert_ai(&repo, "pipeline-pending.txt", "pipeline pending AI");

    stage_ai(&repo, "background-axis.txt", "background wrapper AI");
    repo.shell_git("nohup {git} commit -m background-wrapper >/dev/null 2>&1 &")
        .unwrap();
    assert_ai(&repo, "background-axis.txt", "background wrapper AI");
}
