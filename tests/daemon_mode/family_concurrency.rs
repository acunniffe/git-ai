//! Cross-family drain concurrency: a slow side effect in one repository
//! family must not stall attribution for other families on the same daemon.

use super::*;

fn family_b_git_ai(repo: &TestRepo, workdir: &Path, args: &[&str]) {
    let mut command = Command::new(get_binary_path());
    command.args(args).current_dir(workdir);
    configure_test_home_env(&mut command, repo.test_home_path());
    command.env("GIT_AI_TEST_DB_PATH", repo.test_db_path());
    command.env("GITAI_TEST_DB_PATH", repo.test_db_path());
    if let Some(patch) = repo.config_patch_json() {
        command.env("GIT_AI_TEST_CONFIG_PATCH", patch);
    }
    command.env("GIT_AI_DAEMON_HOME", repo.daemon_home_path());
    command.env(
        "GIT_AI_DAEMON_CONTROL_SOCKET",
        repo.daemon_control_socket_path(),
    );
    command.env(
        "GIT_AI_DAEMON_TRACE_SOCKET",
        repo.daemon_trace_socket_path(),
    );
    command.env("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true");
    let output = command.output().expect("failed to run git-ai for family B");
    assert!(
        output.status.success(),
        "family B git-ai {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn family_b_note_exists(repo: &TestRepo, workdir: &Path) -> bool {
    RawGitCommand::in_working_dir(workdir, &["notes", "--ref=ai", "show", "HEAD"])
        .configure(|command| configure_test_home_env(command, repo.test_home_path()))
        .output()
        .expect("failed to probe family B note")
        .status
        .success()
}

/// Family A runs a rebase whose side effect is delayed by 6s (test hook).
/// Family B then checkpoints and commits on the same daemon. B's authorship
/// note must land well before A's rebase side effect finishes — with
/// serialized drains (the old behavior) the single ingest worker sleeps
/// inside A's drain, so B's checkpoint fence and post-commit authorship
/// cannot complete before the 6s delay elapses.
#[test]
fn slow_family_side_effect_does_not_stall_other_families() {
    const REBASE_DELAY_MS: u64 = 6_000;
    let delay = format!("rebase={REBASE_DELAY_MS}");
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_DELAY_SIDE_EFFECT_MS_FOR_COMMAND",
        delay.as_str(),
    )]);

    // Family A: two commits, then rewrite the last one so the daemon sees a
    // top-level rebase whose side effect sleeps for REBASE_DELAY_MS.
    fs::write(repo.path().join("a-base.txt"), "base\n").expect("failed to write base");
    repo.git(&["add", "a-base.txt"]).expect("stage base");
    repo.git(&["commit", "-m", "base"]).expect("commit base");
    fs::write(repo.path().join("a-second.txt"), "second\n").expect("failed to write second");
    repo.git(&["add", "a-second.txt"]).expect("stage second");
    repo.git(&["commit", "-m", "second"])
        .expect("commit second");

    repo.git(&["rebase", "--force-rebase", "HEAD~1"])
        .expect("rebase should succeed");

    // Family B: a fresh repository under the allowed temp root, driven
    // through the same daemon's sockets.
    let other = tempfile::tempdir().expect("failed to create family B dir");
    let family_b = other.path().join("family-b");
    fs::create_dir_all(&family_b).expect("failed to create family B repo dir");
    let harness = WorkdirRaceHarness::new(&repo, repo.daemon_trace_socket_path());
    RawGitCommand::in_working_dir(&family_b, &["init"])
        .configure(|command| configure_test_home_env(command, repo.test_home_path()))
        .output()
        .expect("git init should run");
    for (key, value) in [("user.email", "b@example.com"), ("user.name", "Family B")] {
        RawGitCommand::in_working_dir(&family_b, &["config", key, value])
            .configure(|command| configure_test_home_env(command, repo.test_home_path()))
            .output()
            .expect("git config should run");
    }
    fs::write(family_b.join("b.txt"), "ai line for family b\n")
        .expect("failed to write family B file");
    family_b_git_ai(&repo, &family_b, &["checkpoint", "mock_ai", "b.txt"]);
    harness.run_traced_git(&family_b, &["add", "b.txt"]);
    let committed_at = std::time::Instant::now();
    harness.run_traced_git(&family_b, &["commit", "-m", "family b commit"]);

    // B's post-commit authorship must land while A's rebase side effect is
    // still sleeping.
    let deadline = committed_at + Duration::from_millis(4_000);
    while !family_b_note_exists(&repo, &family_b) {
        assert!(
            std::time::Instant::now() < deadline,
            "family B's authorship note did not land while family A's delayed \
             rebase side effect was still running"
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        completion_entries_for_command(&repo, "rebase").len(),
        0,
        "family A's delayed rebase side effect should still be in flight when \
         family B's note lands"
    );

    // Let family A finish so the daemon shuts down with no in-flight work.
    let rebase_deadline = std::time::Instant::now() + Duration::from_secs(20);
    while completion_entries_for_command(&repo, "rebase").is_empty() {
        assert!(
            std::time::Instant::now() < rebase_deadline,
            "family A's rebase side effect never completed"
        );
        thread::sleep(Duration::from_millis(100));
    }
}
