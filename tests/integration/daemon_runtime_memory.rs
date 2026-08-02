use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;
use std::time::{Duration, Instant};

fn runtime_build_count(path: &std::path::Path) -> usize {
    fs::read_to_string(path).unwrap_or_default().lines().count()
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
fn daemon_limits_glibc_allocator_arenas() {
    let temp = tempfile::tempdir().unwrap();
    let allocator_policy_log = temp.path().join("allocator-policy.log");
    let _repo = TestRepo::new_with_daemon_env(&[
        (
            "GIT_AI_TEST_ALLOCATOR_POLICY_LOG",
            allocator_policy_log.to_str().unwrap(),
        ),
        ("MALLOC_ARENA_MAX", "2"),
    ]);

    assert_eq!(
        fs::read_to_string(&allocator_policy_log).unwrap_or_default(),
        "arena_max=2\n",
        "the long-lived daemon must constrain glibc arena retention before starting worker threads"
    );
}

#[test]
fn bounded_helper_pool_sustains_multifile_checkpoint_throughput() {
    const FILE_COUNT: usize = 12;
    const FILE_WORK_MILLIS: u64 = 200;
    const MIN_FILES_PER_SECOND: f64 = 10.0;

    let file_work_millis = FILE_WORK_MILLIS.to_string();
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_CHECKPOINT_FILE_DELAY_MS",
        file_work_millis.as_str(),
    )]);
    let filenames: Vec<_> = (0..FILE_COUNT)
        .map(|index| format!("throughput-{index}.txt"))
        .collect();

    for filename in &filenames {
        fs::write(repo.path().join(filename), "base\n").unwrap();
    }
    repo.stage_all_and_commit("throughput base").unwrap();
    for filename in &filenames {
        repo.filename(filename)
            .assert_committed_lines(lines!["base".unattributed_human()]);
    }

    for (index, filename) in filenames.iter().enumerate() {
        fs::write(
            repo.path().join(filename),
            format!("base\nAI throughput line {index}\n"),
        )
        .unwrap();
    }
    let mut checkpoint_args = vec!["checkpoint", "mock_ai"];
    checkpoint_args.extend(filenames.iter().map(String::as_str));

    let started = Instant::now();
    repo.git_ai(&checkpoint_args).unwrap();
    repo.sync_daemon();
    let elapsed = started.elapsed();
    let files_per_second = FILE_COUNT as f64 / elapsed.as_secs_f64();
    eprintln!(
        "bounded helper checkpoint throughput: {files_per_second:.1} files/s \
         ({FILE_COUNT} files in {elapsed:?})"
    );

    assert!(
        elapsed >= Duration::from_millis(FILE_WORK_MILLIS),
        "the injected per-file work was not exercised; elapsed={elapsed:?}"
    );
    assert!(
        files_per_second >= MIN_FILES_PER_SECOND,
        "bounded helper pool checkpoint throughput regressed: {files_per_second:.1} files/s \
         ({FILE_COUNT} files in {elapsed:?}, minimum {MIN_FILES_PER_SECOND:.1} files/s)"
    );

    repo.stage_all_and_commit("throughput checkpoint").unwrap();
    for (index, filename) in filenames.iter().enumerate() {
        repo.filename(filename).assert_committed_lines(lines![
            "base".unattributed_human(),
            format!("AI throughput line {index}").ai(),
        ]);
    }
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
