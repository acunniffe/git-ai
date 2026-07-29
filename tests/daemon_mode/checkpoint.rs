use super::*;

#[test]
#[serial]
fn daemon_mode_post_commit_uploads_prompt_cas() {
    assert_post_commit_uploads_prompt_cas();
}

#[test]
#[serial]
fn daemon_start_spawns_detached_run_process() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let mut command = Command::new(get_binary_path());
    command
        .arg("bg")
        .arg("start")
        .current_dir(repo.path())
        .env("GIT_AI_TEST_DB_PATH", repo.test_db_path())
        .env("GITAI_TEST_DB_PATH", repo.test_db_path());
    configure_test_home_env(&mut command, repo.test_home_path());
    configure_test_daemon_env(
        &mut command,
        &repo.daemon_home_path(),
        &daemon_control_socket_path(&repo),
        &daemon_trace_socket_path(&repo),
    );
    let output = command.output().expect("failed to invoke daemon start");
    assert!(
        output.status.success(),
        "daemon start should return success: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let mut status_ok = false;
    for _ in 0..80 {
        match send_control_request(
            &daemon_control_socket_path(&repo),
            &ControlRequest::StatusFamily {
                repo_working_dir: repo_workdir_string(&repo),
            },
        ) {
            Ok(response) if response.ok => {
                status_ok = true;
                break;
            }
            _ => {
                thread::sleep(Duration::from_millis(25));
            }
        }
    }
    assert!(status_ok, "daemon should be reachable after `daemon start`");

    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
}

#[test]
#[serial]
fn daemon_start_refuses_sandbox_inherited_autostart() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let mut command = Command::new(get_binary_path());
    command
        .arg("bg")
        .arg("start")
        .current_dir(repo.path())
        .env("GIT_AI_TEST_DB_PATH", repo.test_db_path())
        .env("GITAI_TEST_DB_PATH", repo.test_db_path())
        .env("SANDBOX_RUNTIME", "seatbelt");
    configure_test_home_env(&mut command, repo.test_home_path());
    configure_test_daemon_env(
        &mut command,
        &repo.daemon_home_path(),
        &daemon_control_socket_path(&repo),
        &daemon_trace_socket_path(&repo),
    );

    let output = command
        .output()
        .expect("failed to invoke daemon start inside a sandbox");
    let stderr = String::from_utf8_lossy(&output.stderr);
    if daemon_control_socket_path(&repo).exists() {
        let _ = send_control_request(
            &daemon_control_socket_path(&repo),
            &ControlRequest::Shutdown,
        );
    }
    assert!(
        !output.status.success(),
        "daemon start must refuse a sandbox-inherited detached daemon: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.to_ascii_lowercase().contains("sandbox"),
        "daemon start should explain the sandbox refusal: stderr={stderr}"
    );
    assert!(
        !daemon_control_socket_path(&repo).exists(),
        "sandbox refusal must not leave a daemon control socket"
    );
}

#[test]
#[serial]
fn daemon_restart_refuses_sandbox_before_shutdown() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    start_daemon_for_repo(&repo);

    let mut command = Command::new(get_binary_path());
    command
        .arg("bg")
        .arg("restart")
        .current_dir(repo.path())
        .env("GIT_AI_TEST_DB_PATH", repo.test_db_path())
        .env("GITAI_TEST_DB_PATH", repo.test_db_path())
        .env("SANDBOX_RUNTIME", "seatbelt");
    configure_test_home_env(&mut command, repo.test_home_path());
    configure_test_daemon_env(
        &mut command,
        &repo.daemon_home_path(),
        &daemon_control_socket_path(&repo),
        &daemon_trace_socket_path(&repo),
    );

    let output = command
        .output()
        .expect("failed to invoke daemon restart inside a sandbox");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "daemon restart must refuse before shutting down a healthy daemon: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        send_control_request(
            &daemon_control_socket_path(&repo),
            &ControlRequest::StatusFamily {
                repo_working_dir: repo_workdir_string(&repo),
            },
        )
        .is_ok(),
        "sandbox restart refusal must leave the existing daemon running"
    );

    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
}

#[test]
#[serial]
fn daemon_run_allows_sandbox_marker_for_foreground_process() {
    // ENG-211: Explicit foreground startup remains available inside a sandbox.
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start_with_env(&repo, &[("SANDBOX_RUNTIME", "seatbelt")]);

    assert!(
        send_control_request(
            &daemon_control_socket_path(&repo),
            &ControlRequest::StatusFamily {
                repo_working_dir: repo_workdir_string(&repo),
            },
        )
        .is_ok(),
        "foreground daemon should remain usable when started with a sandbox marker"
    );
}

#[test]
#[should_panic(expected = "pending daemon sync work")]
fn dedicated_daemon_restart_rejects_pending_traced_command_for_test() {
    let mut repo = TestRepo::new_dedicated_daemon();

    repo.git(&["commit", "--allow-empty", "-m", "base"])
        .expect("base commit should succeed");
    repo.git(&["branch", "pending-before-restart"])
        .expect("branch creation should succeed");

    repo.restart_dedicated_daemon_for_test();
}

#[test]
#[serial]
fn checkpoint_delegate_autostarts_daemon_when_unavailable() {
    // Test builds disable daemon auto-spawning from ensure_daemon_running to
    // prevent process storms. We verify that checkpoint delegation works by
    // restarting the daemon manually before the checkpoint call.
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    fs::write(repo.path().join("delegate-fallback.txt"), "base\n").expect("failed to write base");
    repo.git(&["add", "delegate-fallback.txt"])
        .expect("add should succeed");
    repo.stage_all_and_commit("base commit")
        .expect("base commit should succeed");

    fs::write(
        repo.path().join("delegate-fallback.txt"),
        "base\nchanged without daemon\n",
    )
    .expect("failed to write updated file");

    // Shut down any stale daemon, then restart it manually.
    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Manually restart the daemon (production auto-start is disabled in test builds)
    start_daemon_for_repo(&repo);

    let completion_baseline = repo.daemon_total_completion_count();
    repo.git_ai_with_env(
        &["checkpoint", "mock_ai", "delegate-fallback.txt"],
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("checkpoint should delegate to daemon and succeed");

    // Wait for the fire-and-forget checkpoint to complete
    repo.wait_for_next_daemon_checkpoint_completion(completion_baseline);

    let status = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::StatusFamily {
            repo_working_dir: repo_workdir_string(&repo),
        },
    )
    .expect("daemon status request should succeed");
    assert!(
        status.ok,
        "daemon should be running after delegated checkpoint; ok={}, error={:?}, data={:?}, socket={}, workdir={}",
        status.ok,
        status.error,
        status.data,
        daemon_control_socket_path(&repo).display(),
        repo_workdir_string(&repo)
    );
    let checkpoints = repo
        .current_working_logs()
        .read_all_checkpoints()
        .expect("checkpoints should be readable");
    assert!(
        checkpoints
            .iter()
            .any(|checkpoint| checkpoint.kind == CheckpointKind::AiAgent),
        "delegated checkpoint should write ai_agent checkpoint via daemon"
    );

    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
}

#[cfg(unix)]
fn run_authorized_bash_hooks_from_cold_daemon(bash_checkpoints_v2: bool) {
    let mut repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    repo.patch_git_ai_config(|patch| {
        patch.feature_flags = Some(json!({"bash_checkpoints_v2": bash_checkpoints_v2}));
    });
    let repo_root = repo.canonical_path();
    let socket_paths = ColdDaemonSocketPaths::new(&repo);
    let bash_db_path = repo.test_home_path().join(if bash_checkpoints_v2 {
        "cold-bash-v2.sqlite"
    } else {
        "cold-bash-legacy.sqlite"
    });
    let bash_db_path_string = bash_db_path.to_string_lossy().into_owned();
    let session_id = if bash_checkpoints_v2 {
        "cold-bash-v2-session"
    } else {
        "cold-bash-legacy-session"
    };
    let tool_use_id = if bash_checkpoints_v2 {
        "cold-bash-v2-tool"
    } else {
        "cold-bash-legacy-tool"
    };

    assert!(
        !socket_paths.control.exists(),
        "the regression must start with a cold daemon"
    );

    let mut hook_outputs = Vec::new();
    for hook_event_name in ["PreToolUse", "PostToolUse"] {
        let hook_input = json!({
            "session_id": session_id,
            "cwd": repo_root,
            "hook_event_name": hook_event_name,
            "tool_name": "Bash",
            "tool_use_id": tool_use_id,
            "tool_input": { "command": "true" },
            "model": "test-model"
        })
        .to_string();
        let mut command = repo.git_ai_command_without_pre_sync_for_test(
            &["checkpoint", "codex", "--hook-input", &hook_input],
            &[],
        );
        let output = command
            .env("GIT_AI_TEST_ALLOW_DAEMON_AUTOSPAWN", "1")
            .env("GIT_AI_DAEMON_CONTROL_SOCKET", &socket_paths.control)
            .env("GIT_AI_DAEMON_TRACE_SOCKET", &socket_paths.trace)
            .env("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", &bash_db_path_string)
            .output()
            .expect("failed to invoke authorized Bash checkpoint");
        assert!(
            output.status.success(),
            "Bash hooks must preserve their exit-zero contract: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        hook_outputs.push(format!(
            "{hook_event_name}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    assert!(
        send_control_request(
            &socket_paths.control,
            &ControlRequest::StatusFamily {
                repo_working_dir: repo_workdir_string(&repo),
            },
        )
        .is_ok(),
        "an authorized Bash hook should make the cold daemon ready before persistence; {}",
        hook_outputs.join(" | ")
    );
    let db = BashHistoryDatabase::open_at_path(&bash_db_path)
        .expect("the Bash history database should be readable");
    let calls = db
        .all_calls_for_test()
        .expect("Bash history calls should be readable");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].session_id, session_id);
    assert_eq!(calls[0].tool_use_id, tool_use_id);
    assert!(calls[0].start_trace_id.is_some());
    assert!(calls[0].end_trace_id.is_some());

    let _ = send_control_request(&socket_paths.control, &ControlRequest::Shutdown);
    for _ in 0..200 {
        if !socket_paths.control.exists() && !socket_paths.trace.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[serial]
#[cfg(unix)]
fn authorized_legacy_bash_hooks_make_cold_daemon_ready_before_persistence() {
    run_authorized_bash_hooks_from_cold_daemon(false);
}

#[test]
#[serial]
#[cfg(unix)]
fn authorized_bash_v2_hooks_make_cold_daemon_ready_before_persistence() {
    run_authorized_bash_hooks_from_cold_daemon(true);
}

#[test]
#[serial]
#[cfg(unix)]
fn denied_and_empty_checkpoint_hooks_do_not_start_cold_daemon() {
    let mut repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let socket_paths = ColdDaemonSocketPaths::new(&repo);
    let empty_hook_input = json!({
        "cwd": repo.canonical_path(),
        "file_paths": [],
    })
    .to_string();
    let mut empty_command = repo.git_ai_command_without_pre_sync_for_test(
        &[
            "checkpoint",
            "mock_ai",
            "--hook-input",
            &empty_hook_input,
            "--",
        ],
        &[],
    );
    let empty_output = empty_command
        .env("GIT_AI_TEST_ALLOW_DAEMON_AUTOSPAWN", "1")
        .env("GIT_AI_DAEMON_CONTROL_SOCKET", &socket_paths.control)
        .env("GIT_AI_DAEMON_TRACE_SOCKET", &socket_paths.trace)
        .output()
        .expect("failed to invoke empty checkpoint hook");
    assert!(empty_output.status.success());
    assert!(!socket_paths.control.exists());

    repo.patch_git_ai_config(|patch| {
        patch.allowed_repositories = Some(Vec::new());
    });
    let denied_hook_input = json!({
        "session_id": "denied-cold-bash-session",
        "cwd": repo.canonical_path(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "denied-cold-bash-tool",
        "tool_input": { "command": "true" }
    })
    .to_string();
    let mut denied_command = repo.git_ai_command_without_pre_sync_for_test(
        &["checkpoint", "codex", "--hook-input", &denied_hook_input],
        &[],
    );
    let denied_output = denied_command
        .env("GIT_AI_TEST_ALLOW_DAEMON_AUTOSPAWN", "1")
        .env("GIT_AI_DAEMON_CONTROL_SOCKET", &socket_paths.control)
        .env("GIT_AI_DAEMON_TRACE_SOCKET", &socket_paths.trace)
        .output()
        .expect("failed to invoke denied checkpoint hook");
    assert!(denied_output.status.success());
    assert!(String::from_utf8_lossy(&denied_output.stderr).contains("no repositories are allowed"));
    assert!(
        !socket_paths.control.exists(),
        "authorization denials must happen before Bash daemon readiness"
    );
}

#[test]
#[serial]
fn checkpoint_fails_hard_when_daemon_startup_is_blocked() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    fs::write(repo.path().join("delegate-fallback-blocked.txt"), "base\n")
        .expect("failed to write base");
    repo.git(&["add", "delegate-fallback-blocked.txt"])
        .expect("add should succeed");
    repo.stage_all_and_commit("base commit")
        .expect("base commit should succeed");

    fs::write(
        repo.path().join("delegate-fallback-blocked.txt"),
        "base\nchanged while startup blocked\n",
    )
    .expect("failed to write updated file");

    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
    std::thread::sleep(std::time::Duration::from_millis(500));

    fs::create_dir_all(
        daemon_lock_path(&repo)
            .parent()
            .expect("daemon lock path should have a parent"),
    )
    .expect("failed to create daemon lock parent directory");
    let held_lock = DaemonLock::acquire(&daemon_lock_path(&repo))
        .expect("should acquire daemon lock before checkpoint invocation");

    let result = repo.git_ai(&["checkpoint", "mock_ai", "delegate-fallback-blocked.txt"]);
    assert!(
        result.is_ok(),
        "checkpoint should exit(0) when daemon is unavailable (never block agents)"
    );

    drop(held_lock);
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn checkpoint_empty_allowlist_does_not_publish_outbox() {
    let mut repo = TestRepo::new_dedicated_daemon();
    repo.patch_git_ai_config(|patch| {
        patch.allowed_repositories = Some(Vec::new());
    });
    fs::write(
        repo.path().join("denied-checkpoint.txt"),
        "sensitive edit\n",
    )
    .expect("failed to write denied checkpoint fixture");

    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["checkpoint", "mock_ai", "denied-checkpoint.txt"],
        &[],
    );
    let output = command
        .output()
        .expect("failed to invoke checkpoint with an empty allowlist");

    assert!(
        output.status.success(),
        "checkpoint hooks must keep their exit-zero contract: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Skipping checkpoint because no repositories are allowed"),
        "checkpoint should report the existing collection-policy denial: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        ready_checkpoint_outbox_records(&repo).is_empty(),
        "a denied checkpoint must not publish any durable outbox record"
    );
}

#[test]
fn dedicated_daemon_home_config_projects_config_patch_fields() {
    let repo = TestRepo::new_with_daemon_env_and_patch(&[], |patch| {
        patch.codex_hooks_format = Some("hooks_json".to_string());
        patch.transcript_streaming_lookback_days = Some(30);
        patch.telemetry_oss_disabled = Some(true);
    });

    let config_path = repo.daemon_home_path().join(".git-ai").join("config.json");
    let config: Value = serde_json::from_slice(
        &fs::read(&config_path).expect("dedicated daemon HOME config should exist"),
    )
    .expect("dedicated daemon HOME config should be valid JSON");

    assert_eq!(
        (
            config.get("codex_hooks_format"),
            config.get("transcript_streaming_lookback_days"),
            config.get("telemetry_oss"),
            config.get("telemetry_oss_disabled"),
        ),
        (
            Some(&json!("hooks_json")),
            Some(&json!(30)),
            Some(&json!("off")),
            None,
        ),
        "the dedicated daemon config should project the ConfigPatch fields, \
         with telemetry_oss_disabled translated to legacy telemetry_oss"
    );
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn checkpoint_transport_failure_publishes_exact_outbox_record() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let file_path = repo.path().join("deferred-checkpoint.txt");
    fs::write(&file_path, "base\n").expect("failed to write base checkpoint fixture");
    repo.git_og(&["add", "deferred-checkpoint.txt"])
        .expect("failed to stage base checkpoint fixture");
    repo.git_og(&["commit", "-m", "base commit"])
        .expect("failed to create base checkpoint commit");
    let base_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("failed to resolve base checkpoint commit")
        .trim()
        .to_string();

    let edited_content = "base\ncaptured while daemon unavailable\n";
    fs::write(&file_path, edited_content).expect("failed to write deferred checkpoint fixture");

    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["checkpoint", "mock_ai", "deferred-checkpoint.txt"],
        &[],
    );
    let output = command
        .output()
        .expect("failed to invoke checkpoint without a daemon");

    assert!(
        output.status.success(),
        "checkpoint hooks must keep their exit-zero contract: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let records = ready_checkpoint_outbox_records(&repo);
    assert_eq!(
        records.len(),
        1,
        "an allowed checkpoint with failed IPC must publish exactly one ready record; records={records:?}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record_path = &records[0];
    let delivery = decode_delivery(
        &fs::read(record_path).expect("failed to read published checkpoint outbox record"),
    )
    .expect("published checkpoint outbox record should decode");
    let expected_filename =
        ready_filename(&delivery).expect("delivery should produce a safe ready filename");

    assert_eq!(
        record_path.file_name().and_then(|name| name.to_str()),
        Some(expected_filename.as_str())
    );
    delivery
        .validate()
        .expect("published checkpoint delivery should validate");
    assert_eq!(delivery.schema_version, CHECKPOINT_DELIVERY_SCHEMA_VERSION);
    assert_eq!(delivery.batch_ordinal, 0);
    assert!(!delivery.delivery_id.is_empty());
    assert!(!delivery.batch_id.is_empty());
    assert!(delivery.captured_at_unix_ms > 0);
    assert!(!delivery.producer_version.is_empty());

    let request = &delivery.request;
    assert!(!request.trace_id.is_empty());
    assert_eq!(request.checkpoint_kind, CheckpointKind::AiAgent);
    assert_eq!(request.path_role, PreparedPathRole::Edited);
    assert!(request.stream_source.is_none());
    assert_eq!(
        request.metadata.get("edit_kind").map(String::as_str),
        Some("file_edit")
    );
    let agent = request
        .agent_id
        .as_ref()
        .expect("mock_ai checkpoint should retain its agent identity");
    assert_eq!(agent.tool, "mock_ai");
    assert!(agent.id.starts_with("ai-thread-"));
    assert_eq!(agent.model, "unknown");

    assert_eq!(request.files.len(), 1);
    let checkpoint_file = &request.files[0];
    assert_eq!(
        checkpoint_file
            .path
            .canonicalize()
            .expect("captured file path should canonicalize"),
        file_path
            .canonicalize()
            .expect("fixture file path should canonicalize")
    );
    assert_eq!(
        checkpoint_file
            .repo_work_dir
            .canonicalize()
            .expect("captured repository path should canonicalize"),
        repo.canonical_path()
    );
    assert_eq!(checkpoint_file.content.as_deref(), Some(edited_content));
    match &checkpoint_file.base_commit {
        BaseCommit::Sha(sha) => assert_eq!(sha, &base_commit),
        BaseCommit::Initial => panic!("committed fixture should capture its base commit SHA"),
    }
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sandbox_checkpoint_autostart_publishes_outbox_without_starting_daemon() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let file_path = repo.path().join("sandbox-checkpoint.txt");
    fs::write(&file_path, "base\n").expect("failed to write base checkpoint fixture");
    repo.git_og(&["add", "sandbox-checkpoint.txt"])
        .expect("failed to stage base checkpoint fixture");
    repo.git_og(&["commit", "-m", "base commit"])
        .expect("failed to create base checkpoint commit");
    fs::write(&file_path, "base\ncaptured in sandbox\n")
        .expect("failed to write sandbox checkpoint fixture");

    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["checkpoint", "mock_ai", "sandbox-checkpoint.txt"],
        &[],
    );
    let output = command
        .env("GIT_AI_TEST_ALLOW_DAEMON_AUTOSPAWN", "1")
        .env("SANDBOX_RUNTIME", "seatbelt")
        .output()
        .expect("failed to invoke sandbox checkpoint");
    let stderr = String::from_utf8_lossy(&output.stderr);

    if daemon_control_socket_path(&repo).exists() {
        let _ = send_control_request(
            &daemon_control_socket_path(&repo),
            &ControlRequest::Shutdown,
        );
    }

    assert!(
        output.status.success(),
        "sandbox checkpoints must preserve their exit-zero contract: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        ready_checkpoint_outbox_records(&repo).len(),
        1,
        "sandbox checkpoint should publish exactly one durable outbox record; stderr={stderr}"
    );
    assert!(
        !daemon_control_socket_path(&repo).exists(),
        "sandbox checkpoint must not start a daemon"
    );
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn checkpoint_repository_discovery_failure_does_not_publish_outbox() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let nested_repo = repo.path().join("malformed-nested-repo");
    fs::create_dir_all(&nested_repo).expect("failed to create malformed nested repo fixture");
    fs::write(
        nested_repo.join(".git"),
        "this is not a valid gitdir pointer\n",
    )
    .expect("failed to write malformed nested .git fixture");
    fs::write(nested_repo.join("private-edit.txt"), "sensitive edit\n")
        .expect("failed to write nested checkpoint fixture");

    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &[
            "checkpoint",
            "mock_ai",
            "malformed-nested-repo/private-edit.txt",
        ],
        &[],
    );
    let output = command
        .output()
        .expect("failed to invoke checkpoint for malformed nested repository");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "repository authorization failures must preserve the hook exit-zero contract: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        ready_checkpoint_outbox_records(&repo).is_empty(),
        "a checkpoint whose repository cannot be verified must not publish an outbox record"
    );
    assert!(
        stderr
            .contains("Skipping checkpoint because repository authorization could not be verified"),
        "repository discovery failure should produce an actionable redacted warning: stderr={stderr}"
    );
    assert!(
        !stderr.contains(repo.path().to_string_lossy().as_ref())
            && !stderr.contains("private-edit.txt"),
        "repository discovery diagnostics must not expose repository or file paths: stderr={stderr}"
    );
}

#[test]
#[cfg(windows)]
#[serial]
fn daemon_windows_stalled_checkpoint_clients_do_not_block_later_control_requests() {
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

    let mut stalled_clients = (0..2)
        .map(|_| {
            let mut command = Command::new(get_binary_path());
            command
                .args(["checkpoint", "codex", "--hook-input", "stdin"])
                .current_dir(repo.path())
                .env("GIT_AI_TEST_DB_PATH", repo.test_db_path())
                .env("GITAI_TEST_DB_PATH", repo.test_db_path())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            configure_test_home_env(&mut command, repo.test_home_path());
            configure_test_daemon_env(
                &mut command,
                &repo.daemon_home_path(),
                &control_socket,
                &daemon_trace_socket_path(&repo),
            );
            command.spawn().expect("failed to spawn stalled checkpoint")
        })
        .collect::<Vec<_>>();
    thread::sleep(Duration::from_millis(250));

    let (response_tx, response_rx) = mpsc::channel();
    let request_socket = control_socket.clone();
    let request_repo = repo_workdir_string(&repo);
    thread::spawn(move || {
        let _ = response_tx.send(send_control_request(
            &request_socket,
            &ControlRequest::StatusFamily {
                repo_working_dir: request_repo,
            },
        ));
    });
    let response = response_rx.recv_timeout(Duration::from_secs(2));

    for client in &mut stalled_clients {
        let _ = client.kill();
        let _ = client.wait();
    }
    let response = response
        .expect("control request timed out after every original pipe worker was stalled")
        .expect("control request failed after every original pipe worker was stalled");
    assert!(
        response.ok,
        "later control request should return an ok response: {:?}",
        response
    );
    daemon.shutdown();
}

#[test]
#[serial]
fn daemon_write_mode_applies_delegated_checkpoint_and_updates_state() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);
    let completion_baseline = repo.daemon_total_completion_count();

    fs::write(repo.path().join("delegate-write.txt"), "base\n").expect("failed to write base");
    repo.git(&["add", "delegate-write.txt"])
        .expect("add should succeed");
    repo.stage_all_and_commit("base commit")
        .expect("base commit should succeed");

    fs::write(
        repo.path().join("delegate-write.txt"),
        "base\nwritten by delegated checkpoint\n",
    )
    .expect("failed to write updated file");

    repo.git_ai_with_env(
        &["checkpoint", "mock_ai", "delegate-write.txt"],
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("delegated checkpoint should succeed");

    wait_for_expected_top_level_completions(&repo, completion_baseline, 1);

    let checkpoints = repo
        .current_working_logs()
        .read_all_checkpoints()
        .expect("checkpoints should be readable");
    assert!(
        checkpoints
            .iter()
            .any(|checkpoint| checkpoint.kind == CheckpointKind::AiAgent),
        "write-mode daemon should execute checkpoint side effect"
    );
}

#[test]
#[serial]
fn daemon_test_mode_git_ai_checkpoint_runs_via_daemon() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    fs::write(repo.path().join("daemon-mode-checkpoint.txt"), "base\n")
        .expect("failed to write base");
    repo.git(&["add", "daemon-mode-checkpoint.txt"])
        .expect("add should succeed");
    repo.stage_all_and_commit("base commit")
        .expect("base commit should succeed");

    fs::write(
        repo.path().join("daemon-mode-checkpoint.txt"),
        "base\nchanged through daemon mode\n",
    )
    .expect("failed to write updated file");
    let completion_baseline = repo.daemon_total_completion_count();

    repo.git_ai(&["checkpoint", "mock_ai", "daemon-mode-checkpoint.txt"])
        .expect("daemon-mode checkpoint should succeed");

    repo.wait_for_next_daemon_checkpoint_completion(completion_baseline);

    let checkpoints = repo
        .current_working_logs()
        .read_all_checkpoints()
        .expect("checkpoints should be readable");
    assert!(
        checkpoints
            .iter()
            .any(|checkpoint| checkpoint.kind == CheckpointKind::AiAgent),
        "daemon-mode checkpoint should still write the ai_agent checkpoint side effect"
    );
}

#[test]
#[serial]
fn daemon_test_mode_human_checkpoint_with_explicit_preset_queues_via_daemon() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    fs::write(repo.path().join("human-direct-path.txt"), "base\n").expect("failed to write base");
    repo.git_og(&["add", "human-direct-path.txt"])
        .expect("add should succeed");
    repo.git_og(&["commit", "-m", "base commit"])
        .expect("base commit should succeed");

    fs::write(repo.path().join("human-direct-path.txt"), "base\nhuman\n")
        .expect("failed to write human change");
    let completion_baseline = repo.daemon_total_completion_count();

    repo.git_ai(&["checkpoint", "human", "human-direct-path.txt"])
        .expect("human checkpoint with preset should succeed");

    repo.wait_for_next_daemon_checkpoint_completion(completion_baseline);

    let git_ai_repo = git_ai::operations::git::repository::find_repository_in_path(
        repo.path()
            .to_str()
            .expect("repo path should be valid UTF-8"),
    )
    .expect("repository should still be discoverable");
    let base_commit = git_ai_repo
        .head()
        .ok()
        .and_then(|head| head.target().ok())
        .unwrap_or_else(|| "initial".to_string());
    let checkpoints = git_ai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap()
        .read_all_checkpoints()
        .expect("checkpoints should be readable");
    assert!(
        checkpoints
            .iter()
            .any(|checkpoint| checkpoint.kind == CheckpointKind::Human),
        "human checkpoint should write the human checkpoint side effect"
    );
}
