use super::*;

#[test]
#[cfg(unix)]
#[serial]
fn daemon_symlink_repo_path_trace_and_status_use_same_family() {
    let unique = format!(
        "git-ai-symlink-family-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let real_path = std::env::temp_dir().join(format!("{unique}-real"));
    let alias_path = std::env::temp_dir().join(format!("{unique}-alias"));
    fs::create_dir_all(&real_path).expect("failed to create real test repo path");
    std::os::unix::fs::symlink(&real_path, &alias_path).expect("failed to create repo symlink");

    let repo = TestRepo::new_at_path_with_daemon_scope(&alias_path, DaemonTestScope::Dedicated);
    assert_ne!(
        repo.path(),
        &repo.canonical_path(),
        "test must exercise an alias path distinct from its canonical path"
    );

    let completion_baseline = repo.daemon_total_completion_count();
    fs::write(repo.path().join("alias.txt"), "alias\n").expect("failed writing aliased file");
    repo.git(&["add", "alias.txt"])
        .expect("aliased path git add should succeed");
    repo.wait_for_daemon_total_completion_count(
        completion_baseline,
        completion_baseline.saturating_add(1),
    );

    let status = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::StatusFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
    )
    .expect("daemon status request should succeed for aliased path");
    assert!(status.ok, "aliased path daemon status should be ok");

    let checkpoint_baseline = repo.daemon_total_completion_count();
    fs::write(repo.path().join("alias.txt"), "alias\nhuman\n")
        .expect("failed writing human aliased file");
    repo.git_ai(&["checkpoint", "human"])
        .expect("aliased path human checkpoint should succeed");
    repo.wait_for_next_daemon_checkpoint_completion(checkpoint_baseline);

    let watermark_for = |path: &Path| {
        let response = send_control_request(
            &daemon_control_socket_path(&repo),
            &ControlRequest::SnapshotWatermarks {
                repo_working_dir: path.to_string_lossy().to_string(),
            },
        )
        .expect("daemon watermark request should succeed");
        assert!(
            response.ok,
            "daemon watermark response should be ok for {}: {:?}",
            path.display(),
            response.error
        );
        response
            .data
            .as_ref()
            .and_then(|data| data.get("worktree_watermark"))
            .and_then(serde_json::Value::as_u64)
    };

    assert!(
        watermark_for(repo.path()).is_some(),
        "aliased worktree path should see full-checkpoint watermark"
    );
    assert!(
        watermark_for(&repo.canonical_path()).is_some(),
        "canonical worktree path should see same full-checkpoint watermark"
    );

    let _ = fs::remove_file(&alias_path);
}

#[test]
#[serial]
fn daemon_pure_trace_socket_commit_after_ai_checkpoint_preserves_ai_replacement_attribution() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let file_path = repo.path().join("daemon-ai-replace.txt");
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(&file_path, "old line\n").expect("failed to write base contents");
    traced_git_with_env(
        &repo,
        &["add", "daemon-ai-replace.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    fs::write(&file_path, "new line from ai\n").expect("failed to write ai contents");
    expected_top_level_completions += 1;
    repo.git_ai_with_env(
        &["checkpoint", "mock_ai", "daemon-ai-replace.txt"],
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("ai checkpoint should succeed");
    traced_git_with_env(
        &repo,
        &["add", "daemon-ai-replace.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "commit ai replacement"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("commit should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    let mut file = repo.filename("daemon-ai-replace.txt");
    file.assert_lines_and_blame(lines!["new line from ai".ai()]);
}

#[test]
fn daemon_trace_current_dir_commands_reserve_order_from_def_repo() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    fs::write(repo.path().join("base.txt"), "base\n").expect("failed to write base");
    repo.git_og(&["add", "base.txt"])
        .expect("base add should succeed");
    repo.git_og(&["commit", "-m", "base"])
        .expect("base commit should succeed");

    fs::write(repo.path().join("a.txt"), "a ai\n").expect("failed to write a.txt");
    repo.git_ai(&["checkpoint", "mock_ai", "a.txt"])
        .expect("a checkpoint should succeed");
    repo.git_og(&["add", "a.txt"])
        .expect("a add should succeed");
    repo.git_og(&["commit", "-m", "commit A"])
        .expect("commit A should succeed");
    let commit_a = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse A should succeed")
        .trim()
        .to_string();

    fs::write(repo.path().join("b.txt"), "b ai\n").expect("failed to write b.txt");
    repo.git_ai(&["checkpoint", "mock_ai", "b.txt"])
        .expect("b checkpoint should succeed");
    repo.git_og(&["add", "b.txt"])
        .expect("b add should succeed");
    repo.git_og(&["commit", "-m", "commit B"])
        .expect("commit B should succeed");
    let commit_b = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse B should succeed")
        .trim()
        .to_string();

    let session_a = repos::test_repo::new_daemon_test_sync_session_id();
    let session_b = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg_a = format!("git-ai.testSyncSession={session_a}");
    let session_arg_b = format!("git-ai.testSyncSession={session_b}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "current-dir-a",
                "argv": ["git", "-c", session_arg_a, "commit", "-m", "commit A"],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "current-dir-a",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "start",
                "sid": "current-dir-b",
                "argv": ["git", "-c", session_arg_b, "commit", "-m", "commit B"],
                "time_ns": 2_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "current-dir-b",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 2_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "current-dir-b",
                "code": 0,
                "time_ns": 2_100u64,
            }),
            trace_atexit_frame("current-dir-b", 0, 2_101u64),
            json!({
                "event": "exit",
                "sid": "current-dir-a",
                "code": 0,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("current-dir-a", 0, 1_101u64),
        ],
    );
    repo.sync_daemon_external_completion_sessions(&[session_a, session_b]);

    assert!(
        repo.read_authorship_note(&commit_a).is_some(),
        "commit A should retain a note even when its trace exit is delivered after commit B"
    );
    assert!(
        repo.read_authorship_note(&commit_b).is_some(),
        "commit B should have a note"
    );
    let mut file_a = repo.filename("a.txt");
    file_a.assert_committed_lines(lines!["a ai".ai()]);
    let mut file_b = repo.filename("b.txt");
    file_b.assert_committed_lines(lines!["b ai".ai()]);
}

#[test]
#[cfg(not(windows))]
fn daemon_trace_listener_stalled_connection_does_not_block_later_trace_connections() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let _stalled_stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to open stalled trace socket");

    let session = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "stalled-listener-followup",
                "argv": [
                    "git",
                    "-c",
                    session_arg,
                    "commit",
                    "--dry-run",
                    "-m",
                    "synthetic",
                ],
                "time_ns": 10_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "stalled-listener-followup",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 10_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "stalled-listener-followup",
                "code": 0,
                "time_ns": 10_100u64,
            }),
            trace_atexit_frame("stalled-listener-followup", 0, 10_101u64),
        ],
    );

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if repo
            .daemon_completion_entries()
            .iter()
            .any(|entry| entry.test_sync_session.as_deref() == Some(session.as_str()))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }

    panic!(
        "daemon did not process a later trace connection while an earlier trace socket was stalled"
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_stalled_unidentified_trace_connection_does_not_block_checkpoint_control_request() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let control_socket = daemon_control_socket_path(&repo);

    let _stalled_stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to open stalled trace socket");
    thread::sleep(Duration::from_millis(150));

    let file_path = repo.path().join("checkpoint-after-stalled-trace.txt");
    fs::write(&file_path, "checkpoint content\n").unwrap();

    let request = CheckpointRequest {
        trace_id: "checkpoint-after-stalled-trace".to_string(),
        checkpoint_kind: CheckpointKind::Human,
        agent_id: None,
        files: vec![CheckpointFile {
            path: PathBuf::from("checkpoint-after-stalled-trace.txt"),
            content: Some("checkpoint content\n".to_string()),
            repo_work_dir: repo.path().to_path_buf(),
            base_commit: BaseCommit::Initial,
        }],
        path_role: PreparedPathRole::Edited,
        stream_source: None,
        metadata: Default::default(),
    };

    let response = send_control_request_with_timeout(
        &control_socket,
        &ControlRequest::CheckpointRun {
            request: Box::new(request),
        },
        Duration::from_millis(500),
    )
    .expect("checkpoint control request should not block on unidentified trace sockets");

    assert!(
        response.ok,
        "checkpoint control request should succeed: {:?}",
        response
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_checkpoint_resolution_applies_total_content_budget() {
    let mut repo = TestRepo::new_dedicated_daemon();
    repo.patch_git_ai_config(|p| {
        p.max_checkpoint_file_size_bytes = Some(1024);
        p.max_checkpoint_total_size_bytes = Some(96);
        p.max_checkpoint_total_lines = Some(1000);
    });

    let control_socket = daemon_control_socket_path(&repo);
    fs::write(repo.path().join("a_kept.txt"), "a".repeat(48)).unwrap();
    fs::write(repo.path().join("z_skipped.txt"), "z".repeat(64)).unwrap();

    let request = CheckpointRequest {
        trace_id: "daemon-checkpoint-budget".to_string(),
        checkpoint_kind: CheckpointKind::Human,
        agent_id: None,
        files: vec![
            CheckpointFile {
                path: PathBuf::from("a_kept.txt"),
                content: Some("a".repeat(48)),
                repo_work_dir: repo.path().to_path_buf(),
                base_commit: BaseCommit::Initial,
            },
            CheckpointFile {
                path: PathBuf::from("z_skipped.txt"),
                content: Some("z".repeat(64)),
                repo_work_dir: repo.path().to_path_buf(),
                base_commit: BaseCommit::Initial,
            },
        ],
        path_role: PreparedPathRole::Edited,
        stream_source: None,
        metadata: Default::default(),
    };

    let response = send_control_request_with_timeout(
        &control_socket,
        &ControlRequest::CheckpointRun {
            request: Box::new(request),
        },
        Duration::from_secs(5),
    )
    .expect("checkpoint control request should succeed");

    assert!(
        response.ok,
        "checkpoint control request should succeed: {:?}",
        response
    );

    let checkpoints = repo
        .current_working_logs()
        .read_all_checkpoints()
        .expect("checkpoints should be readable");
    assert_eq!(checkpoints.len(), 1, "expected exactly one checkpoint");
    let checkpoint = checkpoints.last().unwrap();
    assert_eq!(
        checkpoint.entries.len(),
        1,
        "expected daemon resolver to apply aggregate content budget"
    );
    assert_eq!(checkpoint.entries[0].file, "a_kept.txt");
}

#[test]
#[cfg(not(windows))]
fn daemon_stalled_unidentified_trace_connection_does_not_block_sync_control_request() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let control_socket = daemon_control_socket_path(&repo);

    let _stalled_stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to open stalled trace socket");
    thread::sleep(Duration::from_millis(150));

    let response = send_control_request_with_timeout(
        &control_socket,
        &ControlRequest::SyncFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
        Duration::from_millis(500),
    )
    .expect("sync control request should not block on unidentified trace sockets");

    assert!(
        response.ok,
        "sync control request should succeed: {:?}",
        response
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_partial_trace_line_does_not_block_checkpoint_control_request() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let control_socket = daemon_control_socket_path(&repo);

    let mut stalled_stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to open stalled trace socket");
    stalled_stream
        .write_all(br#"{"event":"start""#)
        .expect("failed to write partial trace frame");
    stalled_stream
        .flush()
        .expect("failed to flush partial trace frame");
    thread::sleep(Duration::from_millis(150));

    let file_path = repo.path().join("checkpoint-after-partial-trace.txt");
    fs::write(&file_path, "checkpoint content\n").unwrap();

    let request = CheckpointRequest {
        trace_id: "checkpoint-after-partial-trace".to_string(),
        checkpoint_kind: CheckpointKind::Human,
        agent_id: None,
        files: vec![CheckpointFile {
            path: PathBuf::from("checkpoint-after-partial-trace.txt"),
            content: Some("checkpoint content\n".to_string()),
            repo_work_dir: repo.path().to_path_buf(),
            base_commit: BaseCommit::Initial,
        }],
        path_role: PreparedPathRole::Edited,
        stream_source: None,
        metadata: Default::default(),
    };

    let response = send_control_request_with_timeout(
        &control_socket,
        &ControlRequest::CheckpointRun {
            request: Box::new(request),
        },
        Duration::from_millis(500),
    )
    .expect("checkpoint control request should not block on incomplete trace frames");

    assert!(
        response.ok,
        "checkpoint control request should succeed: {:?}",
        response
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_trace_listener_partial_line_does_not_block_later_trace_connections() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut stalled_stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to open stalled trace socket");
    stalled_stream
        .write_all(br#"{"event":"start""#)
        .expect("failed to write partial trace frame");
    stalled_stream
        .flush()
        .expect("failed to flush partial trace frame");
    thread::sleep(Duration::from_millis(200));

    let session = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "partial-listener-followup",
                "argv": [
                    "git",
                    "-c",
                    session_arg,
                    "commit",
                    "--dry-run",
                    "-m",
                    "synthetic",
                ],
                "time_ns": 10_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "partial-listener-followup",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 10_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "partial-listener-followup",
                "code": 0,
                "time_ns": 10_100u64,
            }),
            trace_atexit_frame("partial-listener-followup", 0, 10_101u64),
        ],
    );

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if repo
            .daemon_completion_entries()
            .iter()
            .any(|entry| entry.test_sync_session.as_deref() == Some(session.as_str()))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }

    panic!(
        "daemon did not process a later trace connection while an earlier trace socket held a partial line"
    );
}

#[test]
#[cfg(not(windows))]
fn daemon_trace_connection_close_without_atexit_does_not_block_later_trace() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "closed-before-atexit",
                "argv": ["git", "commit", "--dry-run", "-m", "incomplete"],
                "time_ns": 9_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "closed-before-atexit",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 9_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "closed-before-atexit",
                "code": 0,
                "time_ns": 9_100u64,
            }),
        ],
    );

    let session = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "complete-after-closed-root",
                "argv": [
                    "git",
                    "-c",
                    session_arg,
                    "commit",
                    "--dry-run",
                    "-m",
                    "synthetic",
                ],
                "time_ns": 10_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "complete-after-closed-root",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 10_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "complete-after-closed-root",
                "code": 0,
                "time_ns": 10_100u64,
            }),
            trace_atexit_frame("complete-after-closed-root", 0, 10_101u64),
        ],
    );

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if repo
            .daemon_completion_entries()
            .iter()
            .any(|entry| entry.test_sync_session.as_deref() == Some(session.as_str()))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }

    panic!("daemon did not process a later trace after a mutating root closed before atexit");
}

#[test]
#[cfg(not(windows))]
fn daemon_control_listener_stalled_connection_does_not_block_later_control_requests() {
    let repo = TestRepo::new_dedicated_daemon();
    let control_socket = daemon_control_socket_path(&repo);
    let _stalled_stream =
        open_local_socket_stream_with_timeout(&control_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to open stalled control socket");
    thread::sleep(Duration::from_millis(50));

    let response = send_control_request(
        &control_socket,
        &ControlRequest::StatusFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
    )
    .expect("later control request should complete while an earlier control socket is stalled");

    assert!(
        response.ok,
        "later control request should return an ok response: {:?}",
        response
    );
}

#[test]
#[cfg(windows)]
fn daemon_windows_control_pipe_worker_exhaustion_does_not_block_later_control_requests() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_WINDOWS_CONTROL_PIPE_WORKERS", "2"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );
    let control_socket = daemon_control_socket_path(&repo);

    let _stalled_streams = (0..2)
        .map(|_| {
            open_local_socket_stream_with_timeout(&control_socket, DAEMON_TEST_PROBE_TIMEOUT)
                .expect("failed to open stalled control pipe")
        })
        .collect::<Vec<_>>();
    thread::sleep(Duration::from_millis(100));

    let response = send_control_request(
        &control_socket,
        &ControlRequest::StatusFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
    )
    .expect("control request should complete after every original pipe worker is stalled");

    assert!(
        response.ok,
        "later control request should return an ok response: {:?}",
        response
    );
    daemon.shutdown();
}

#[test]
#[cfg(windows)]
fn daemon_windows_trace_pipe_worker_exhaustion_does_not_block_later_trace_connections() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_WINDOWS_TRACE_PIPE_WORKERS", "2"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let _stalled_streams = (0..2)
        .map(|_| {
            open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
                .expect("failed to open stalled trace pipe")
        })
        .collect::<Vec<_>>();
    thread::sleep(Duration::from_millis(100));

    let session = repos::test_repo::new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");
    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "windows-exhaustion-followup",
                "argv": ["git", "-c", session_arg, "commit", "-m", "synthetic"],
                "time_ns": 15_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "windows-exhaustion-followup",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 15_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "windows-exhaustion-followup",
                "code": 0,
                "time_ns": 15_100u64,
            }),
            trace_atexit_frame("windows-exhaustion-followup", 0, 15_101u64),
        ],
    );

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if repo
            .daemon_completion_entries()
            .iter()
            .any(|entry| entry.test_sync_session.as_deref() == Some(session.as_str()))
        {
            daemon.shutdown();
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }

    daemon.shutdown();
    panic!(
        "daemon did not process a later trace connection after every original pipe worker was stalled"
    );
}

#[test]
#[serial]
#[cfg(not(windows))]
fn daemon_trace_ingest_backpressure_shuts_down_without_blocking_listener() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut daemon = DaemonGuard::start_with_env(
        &repo,
        &[
            ("GIT_AI_TEST_TRACE_INGEST_QUEUE_CAPACITY", "1"),
            ("GIT_AI_TEST_TRACE_INGEST_WORKER_START_DELAY_MS", "5000"),
            ("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL", "86400"),
            ("GIT_AI_DAEMON_MAX_UPTIME_SECS", "86400"),
        ],
    );
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to connect trace socket");
    write_trace_frames_to_stream(
        &mut stream,
        &[
            json!({
                "event": "start",
                "sid": "backpressure-root",
                "argv": ["git", "commit", "-m", "synthetic"],
                "time_ns": 20_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "backpressure-root",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 20_001u64,
            }),
        ],
    );

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if daemon
            .child
            .try_wait()
            .expect("failed to poll daemon")
            .is_some()
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }

    panic!("daemon did not fail closed within 2s when trace ingest queue capacity was exhausted");
}

#[test]
#[cfg(not(windows))]
fn daemon_sync_for_unrelated_family_ignores_open_mutating_root_of_other_family() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let control_socket = daemon_control_socket_path(&repo);
    let repo_working_dir = repo_workdir_string(&repo);

    // A second repository family: the daemon resolves families from the
    // filesystem alone, so a minimal worktree with a .git dir suffices.
    let other = tempfile::tempdir().expect("failed to create other family dir");
    let other_worktree = other.path().join("other-repo");
    let other_git_dir = other_worktree.join(".git");
    fs::create_dir_all(&other_git_dir).expect("failed to create other .git dir");
    fs::write(other_git_dir.join("HEAD"), "ref: refs/heads/main\n")
        .expect("failed to write other HEAD");
    let other_working_dir = other_worktree
        .canonicalize()
        .expect("failed to canonicalize other worktree")
        .to_string_lossy()
        .to_string();

    // Hold open a mutating trace root attributed to the other family: start +
    // def_repo frames, no exit/atexit, connection kept open.
    let mut open_root_stream =
        open_local_socket_stream_with_timeout(&trace_socket, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to open trace socket for the long-running root");
    write_trace_frames_to_stream(
        &mut open_root_stream,
        &[
            json!({
                "event": "start",
                "sid": "other-family-open-root",
                "argv": ["git", "commit", "-m", "long-running commit"],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "other-family-open-root",
                "worktree": other_worktree.to_string_lossy(),
                "repo": other_git_dir.to_string_lossy(),
                "time_ns": 1_001u64,
            }),
        ],
    );

    // Wait until the daemon has registered the open root: syncs of the
    // originating family start hitting the drain fence (client timeout).
    let registered_deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let probe = send_control_request_with_timeout(
            &control_socket,
            &ControlRequest::SyncFamily {
                repo_working_dir: other_working_dir.clone(),
            },
            Duration::from_millis(250),
        );
        if probe.is_err() {
            break;
        }
        assert!(
            std::time::Instant::now() < registered_deadline,
            "the open mutating root never started fencing syncs of its own family"
        );
        thread::sleep(Duration::from_millis(25));
    }

    // An unrelated family must sync promptly while the other family's
    // mutating root stays open.
    let response = send_control_request_with_timeout(
        &control_socket,
        &ControlRequest::SyncFamily {
            repo_working_dir: repo_working_dir.clone(),
        },
        Duration::from_secs(10),
    )
    .expect("sync of an unrelated family must not wait on another family's open mutating root");
    assert!(
        response.ok,
        "unrelated family sync should succeed: {:?}",
        response.error
    );

    // Guard: the originating family stays fenced until its root closes.
    let (done_tx, done_rx) = mpsc::channel();
    let waiter_control_socket = control_socket.clone();
    let waiter_working_dir = other_working_dir.clone();
    let waiter = thread::spawn(move || {
        let result = send_control_request_with_timeout(
            &waiter_control_socket,
            &ControlRequest::SyncFamily {
                repo_working_dir: waiter_working_dir,
            },
            Duration::from_secs(30),
        );
        let _ = done_tx.send(result.is_ok());
    });
    assert!(
        done_rx.recv_timeout(Duration::from_secs(1)).is_err(),
        "the originating family must stay fenced while its mutating root is open"
    );

    drop(open_root_stream);
    let completed = done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the originating family sync should complete once the root closes");
    assert!(
        completed,
        "the originating family sync should get a response after the root closes"
    );
    waiter.join().expect("waiter thread should not panic");
}
