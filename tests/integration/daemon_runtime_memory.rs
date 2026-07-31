use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

fn runtime_build_count(path: &std::path::Path) -> usize {
    fs::read_to_string(path).unwrap_or_default().lines().count()
}

#[test]
fn repeated_agent_commits_reuse_the_daemon_helper_runtime() {
    let temp = tempfile::tempdir().unwrap();
    let runtime_build_log = temp.path().join("runtime-builds.log");
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_TOKIO_RUNTIME_BUILD_LOG",
        runtime_build_log.to_str().unwrap(),
    )]);
    let path = repo.path().join("crew-state.txt");

    fs::write(&path, "base\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let mut file = repo.filename("crew-state.txt");
    file.assert_committed_lines(lines!["base".unattributed_human()]);

    let runtime_builds_before_agent_commits = runtime_build_count(&runtime_build_log);
    let mut expected = vec!["base".unattributed_human()];
    let mut contents = String::from("base\n");

    for index in 0..3 {
        repo.git_ai(&["checkpoint", "human", "crew-state.txt"])
            .unwrap();
        let line = format!("agent state {index}");
        contents.push_str(&line);
        contents.push('\n');
        fs::write(&path, &contents).unwrap();
        repo.git_ai(&["checkpoint", "mock_ai", "crew-state.txt"])
            .unwrap();
        repo.stage_all_and_commit(&format!("agent state {index}"))
            .unwrap();

        expected.push(line.ai());
        file.assert_committed_lines(expected.clone());
    }

    let helper_runtimes_built =
        runtime_build_count(&runtime_build_log) - runtime_builds_before_agent_commits;
    assert!(
        helper_runtimes_built <= 1,
        "the daemon must reuse one bounded helper runtime across commits; built {helper_runtimes_built}"
    );
}
