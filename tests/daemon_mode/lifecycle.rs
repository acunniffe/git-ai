use super::*;

fn bg_command(repo: &TestRepo, subcommand: &str, extra_args: &[&str]) -> Output {
    let daemon_home = repo.daemon_home_path();
    let control_socket_path = daemon_control_socket_path(repo);
    let trace_socket_path = daemon_trace_socket_path(repo);
    let mut command = Command::new(get_binary_path());
    command.arg("bg").arg(subcommand);
    for arg in extra_args {
        command.arg(arg);
    }
    command
        .current_dir(repo.path())
        .env("GIT_AI_TEST_DB_PATH", repo.test_db_path())
        .env("GITAI_TEST_DB_PATH", repo.test_db_path());
    configure_test_home_env(&mut command, repo.test_home_path());
    configure_test_daemon_env(
        &mut command,
        &daemon_home,
        &control_socket_path,
        &trace_socket_path,
    );
    command.output().expect("failed to invoke bg command")
}

use std::process::Output;

#[test]
#[serial]
fn daemon_shutdown_hard_kills_process() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut guard = DaemonGuard::start(&repo);

    let config = DaemonConfig::from_home(&repo.daemon_home_path());
    let pid = read_daemon_pid(&config).expect("should read daemon pid");

    // Verify daemon process is alive.
    assert!(
        process_exists(pid),
        "daemon process {} should be alive before hard shutdown",
        pid
    );

    let output = bg_command(&repo, "shutdown", &["--hard"]);
    assert!(
        output.status.success(),
        "shutdown --hard should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Reap the child so the zombie doesn't linger (our test process is the parent).
    let _ = guard.child.wait();

    // Process should be dead.
    for _ in 0..40 {
        if !process_exists(pid) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !process_exists(pid),
        "daemon process {} should be dead after hard shutdown",
        pid
    );
}

#[test]
#[serial]
fn daemon_restart_brings_up_new_process() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut guard = DaemonGuard::start(&repo);

    let config = DaemonConfig::from_home(&repo.daemon_home_path());
    let old_pid = read_daemon_pid(&config).expect("should read daemon pid");

    // Reap the child first — on Linux the killed process is a zombie until we wait.
    let _ = guard.child.kill();
    let _ = guard.child.wait();

    let output = bg_command(&repo, "restart", &[]);
    assert!(
        output.status.success(),
        "restart should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // New daemon should be up with a different PID.
    let new_pid = read_daemon_pid(&config).expect("should read new daemon pid");
    assert_ne!(old_pid, new_pid, "restart should produce a new daemon PID");

    // New daemon should be responsive.
    let status = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::StatusFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
    );
    assert!(
        status.is_ok(),
        "new daemon should respond to status request"
    );

    // Clean up the new detached daemon.
    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
}

#[test]
#[serial]
fn daemon_restart_hard_kills_and_restarts() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut guard = DaemonGuard::start(&repo);

    let config = DaemonConfig::from_home(&repo.daemon_home_path());
    let old_pid = read_daemon_pid(&config).expect("should read daemon pid");

    // Reap the child first — on Linux the killed process is a zombie until we wait.
    let _ = guard.child.kill();
    let _ = guard.child.wait();

    let output = bg_command(&repo, "restart", &["--hard"]);
    assert!(
        output.status.success(),
        "restart --hard should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // New daemon should be up.
    let new_pid = read_daemon_pid(&config).expect("should read new daemon pid");
    assert_ne!(
        old_pid, new_pid,
        "hard restart should produce a new daemon PID"
    );

    // Clean up.
    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
}

#[test]
#[serial]
fn daemon_shutdown_hard_when_not_running_fails_gracefully() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    // Don't start any daemon — just run shutdown --hard on a cold config.
    // It should not panic / crash.
    let output = bg_command(&repo, "shutdown", &["--hard"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Should fail with a readable error about the service not running.
    assert!(
        !output.status.success(),
        "shutdown --hard on cold config should fail"
    );
    assert!(
        stderr.contains("not running")
            || stderr.contains("pid")
            || stderr.contains("not found")
            || stderr.contains("No such file"),
        "shutdown --hard on cold config should fail gracefully: {}",
        stderr
    );
}

#[test]
#[serial]
fn daemon_restart_when_not_running_starts_fresh() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    // No daemon running — restart should just start a new one.
    let output = bg_command(&repo, "restart", &[]);
    assert!(
        output.status.success(),
        "restart with no running daemon should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Daemon should be up.
    let status = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::StatusFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
    );
    assert!(
        status.is_ok(),
        "daemon should be reachable after restart from cold state"
    );

    // Clean up.
    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
}

fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

/// Regression test for issue #919: daemon must recover from panics in the
/// side-effect pipeline and continue processing subsequent commands.
///
/// This test:
/// 1. Starts a dedicated daemon with a file-based panic flag.
/// 2. Sends a git commit that triggers side-effect processing → panic.
/// 3. Verifies the daemon process is still alive (not a zombie).
/// 4. Removes the panic flag file.
/// 5. Sends another git commit and verifies the daemon processes it normally.
/// 6. Cleanly shuts down the daemon.
#[test]
#[serial]
fn daemon_recovers_from_panic_in_side_effect_pipeline() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    // Create a flag file that will trigger a panic in the side-effect pipeline.
    let panic_flag_path = repo.path().join(".panic_flag");
    fs::write(&panic_flag_path, "1").expect("failed to write panic flag");

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[(
            "GIT_AI_TEST_PANIC_IN_SIDE_EFFECT_FLAG",
            panic_flag_path
                .to_str()
                .expect("panic flag path should be utf-8"),
        )],
    );
    let daemon_pid = daemon.child.id();

    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];

    // Phase 1 — Send a commit while the panic flag is active.
    // The daemon will panic inside the side-effect pipeline, but catch_unwind
    // should keep it alive.  Because panicked commands do NOT emit completion
    // log entries, we cannot use wait_for_expected_top_level_completions here.
    // Instead we track these commands in a throwaway counter and poll the
    // daemon's control socket to confirm it is still responsive.
    let mut _throwaway = 0u64;

    fs::write(repo.path().join("file.txt"), "initial\n").expect("failed to write initial file");
    traced_git_with_env(&repo, &["add", "file.txt"], &env_refs, &mut _throwaway)
        .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "initial"],
        &env_refs,
        &mut _throwaway,
    )
    .expect("initial commit should succeed");

    // Give the daemon enough time to ingest the trace events and attempt
    // (and panic in) side-effect processing.  Poll the control socket to
    // confirm the daemon is still responsive.
    let mut daemon_responded = false;
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if send_control_request(
            &daemon.control_socket_path,
            &ControlRequest::StatusFamily {
                repo_working_dir: daemon.repo_working_dir.clone(),
            },
        )
        .is_ok()
        {
            daemon_responded = true;
            break;
        }
    }
    assert!(
        daemon_responded,
        "daemon control socket should respond after panic in side-effect pipeline"
    );

    // Verify the daemon process is still alive after the panic.
    assert!(
        process_exists(daemon_pid),
        "daemon process should still be alive after a panic in side-effect pipeline"
    );
    assert!(
        daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_none(),
        "daemon should not have exited after panic"
    );

    // Phase 2 — Remove the panic flag and verify the daemon processes a new
    // commit end-to-end (completion log entry recorded).
    fs::remove_file(&panic_flag_path).expect("failed to remove panic flag");

    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("file.txt"), "updated\n").expect("failed to write updated file");
    traced_git_with_env(
        &repo,
        &["add", "file.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("second add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "second commit"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("second commit should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    // Verify the daemon is still alive after recovering and processing normal commands.
    assert!(
        process_exists(daemon_pid),
        "daemon should still be alive after recovering and processing normal commands"
    );

    // Clean shutdown.
    daemon.shutdown();
}

/// When the daemon's socket files are deleted from the filesystem while the
/// daemon process is still running, the daemon becomes a zombie: alive but
/// unreachable. New clients cannot connect because the filesystem entries are
/// gone, even though the kernel-level socket fds are still open.
///
/// The daemon should detect that its socket files have been unlinked and
/// initiate a graceful shutdown so that the next wrapper invocation can
/// spawn a fresh daemon via ensure_daemon_running.
#[test]
#[serial]
#[cfg(unix)]
fn daemon_shuts_down_when_socket_files_are_deleted() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket_path = daemon_control_socket_path(&repo);
    let trace_socket_path = daemon_trace_socket_path(&repo);

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS", "1"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );

    // Verify the daemon is alive and both sockets exist on disk.
    assert!(
        control_socket_path.exists(),
        "control socket should exist after daemon start"
    );
    assert!(
        trace_socket_path.exists(),
        "trace socket should exist after daemon start"
    );
    assert!(
        send_control_request(
            &control_socket_path,
            &ControlRequest::StatusFamily {
                repo_working_dir: repo_workdir_string(&repo),
            },
        )
        .is_ok(),
        "daemon should respond to status requests"
    );

    // Verify daemon is actually still running before we delete sockets.
    assert!(
        daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_none(),
        "daemon process should still be running before socket deletion"
    );

    // Delete the socket files out from under the running daemon.
    fs::remove_file(&control_socket_path).expect("failed to delete control socket");
    fs::remove_file(&trace_socket_path).expect("failed to delete trace socket");
    assert!(
        !control_socket_path.exists(),
        "control socket should be deleted"
    );
    assert!(
        !trace_socket_path.exists(),
        "trace socket should be deleted"
    );

    // Wait for the daemon to notice and shut down. With a 1-second check
    // interval, it should detect the missing sockets within a few seconds.
    let mut daemon_exited = false;
    for _ in 0..100 {
        if daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_some()
        {
            daemon_exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    assert!(
        daemon_exited,
        "daemon should shut down after its socket files are deleted, \
         but the process is still running after 10 seconds"
    );

    // DaemonGuard::drop calls shutdown(), which is a no-op if already exited.
    daemon.shutdown();
}

/// After detecting that its sockets have been deleted, the daemon should
/// spawn a detached `git-ai bg restart --hard` process that reaps the
/// zombie and starts a fresh daemon. Verify that a new, reachable daemon
/// is running after the original one dies.
#[test]
#[serial]
#[cfg(unix)]
fn daemon_self_heals_after_socket_deletion() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let control_socket_path = daemon_control_socket_path(&repo);
    let trace_socket_path = daemon_trace_socket_path(&repo);

    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS", "1"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
            ("GIT_AI_DAEMON_MIN_UPTIME_FOR_RESTART_SECS", "0"),
        ],
    );

    // Verify the daemon is alive and responsive.
    assert!(
        send_control_request(
            &control_socket_path,
            &ControlRequest::StatusFamily {
                repo_working_dir: repo_workdir_string(&repo),
            },
        )
        .is_ok(),
        "original daemon should respond to status requests"
    );

    // Delete both socket files.
    fs::remove_file(&control_socket_path).expect("failed to delete control socket");
    fs::remove_file(&trace_socket_path).expect("failed to delete trace socket");

    // Wait for the original daemon to exit.
    let mut original_exited = false;
    for _ in 0..100 {
        if daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_some()
        {
            original_exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        original_exited,
        "original daemon should shut down after socket deletion"
    );

    // Wait for a new daemon to come up with fresh sockets.
    let mut new_daemon_reachable = false;
    for _ in 0..200 {
        if control_socket_path.exists()
            && send_control_request(
                &control_socket_path,
                &ControlRequest::StatusFamily {
                    repo_working_dir: repo_workdir_string(&repo),
                },
            )
            .is_ok()
        {
            new_daemon_reachable = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    assert!(
        new_daemon_reachable,
        "a new daemon should be reachable after the original self-healed"
    );

    // Clean up the new daemon.
    let _ = send_control_request(&control_socket_path, &ControlRequest::Shutdown);
    for _ in 0..100 {
        if !control_socket_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
}
