#[macro_use]
#[path = "integration/repos/mod.rs"]
mod repos;

use git_ai::operations::daemon::control_api::CasSyncPayload;
#[cfg(unix)]
use git_ai::operations::daemon::send_control_request_with_timeout;
use git_ai::operations::daemon::{
    ControlRequest, ControlResponse, DaemonConfig, TelemetryEnvelope,
    local_socket_connects_with_timeout, open_local_socket_stream_with_timeout,
    send_control_request,
};
use repos::test_repo::{DaemonTestScope, TestRepo, get_binary_path, real_git_executable};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::Duration;

const DAEMON_TEST_PROBE_TIMEOUT: Duration = Duration::from_millis(100);

fn read_global_git_config(repo: &TestRepo, key: &str) -> Option<String> {
    let mut command = Command::new(real_git_executable());
    command.args(["config", "--global", "--get", key]);
    command.current_dir(repo.path());
    configure_test_home_env(&mut command, repo);
    let output = command.output().expect("failed to read global git config");

    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if value.is_empty() { None } else { Some(value) }
    } else {
        None
    }
}

fn daemon_trace_socket_path(repo: &TestRepo) -> PathBuf {
    repo.daemon_trace_socket_path()
}

fn daemon_control_socket_path(repo: &TestRepo) -> PathBuf {
    repo.daemon_control_socket_path()
}

fn configure_test_home_env(command: &mut Command, repo: &TestRepo) {
    command.env("HOME", repo.test_home_path());
    command.env(
        "GIT_CONFIG_GLOBAL",
        repo.test_home_path().join(".gitconfig"),
    );
    // Redirect XDG_CONFIG_HOME so git does not read the real user's
    // $XDG_CONFIG_HOME/git/config (which may contain filter drivers,
    // aliases, or other settings that break test isolation).
    command.env("XDG_CONFIG_HOME", repo.test_home_path().join(".config"));
    #[cfg(windows)]
    {
        command.env("USERPROFILE", repo.test_home_path());
        command.env(
            "APPDATA",
            repo.test_home_path().join("AppData").join("Roaming"),
        );
        command.env(
            "LOCALAPPDATA",
            repo.test_home_path().join("AppData").join("Local"),
        );
    }
}

fn configure_test_daemon_env(command: &mut Command, repo: &TestRepo) {
    command.env("GIT_AI_DAEMON_HOME", repo.daemon_home_path());
    command.env(
        "GIT_AI_DAEMON_CONTROL_SOCKET",
        daemon_control_socket_path(repo),
    );
    command.env("GIT_AI_DAEMON_TRACE_SOCKET", daemon_trace_socket_path(repo));
}

fn write_daemon_config(repo: &TestRepo) {
    let config_dir = repo.test_home_path().join(".git-ai");
    fs::create_dir_all(&config_dir).expect("failed to create daemon config dir");
    let config_path = config_dir.join("config.json");
    let config = serde_json::json!({
        "git_path": real_git_executable(),
        "disable_auto_updates": true,
        "feature_flags": {},
        "quiet": false
    });
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).expect("failed to serialize daemon config"),
    )
    .expect("failed to write daemon config");
}

fn git_ai_with_daemon_env(repo: &TestRepo, args: &[&str]) -> Result<String, String> {
    let daemon_home = repo.daemon_home_path().to_string_lossy().to_string();
    let control_socket = daemon_control_socket_path(repo)
        .to_string_lossy()
        .to_string();
    let trace_socket = daemon_trace_socket_path(repo).to_string_lossy().to_string();
    let envs = [
        ("GIT_AI_DAEMON_HOME", daemon_home.as_str()),
        ("GIT_AI_DAEMON_CONTROL_SOCKET", control_socket.as_str()),
        ("GIT_AI_DAEMON_TRACE_SOCKET", trace_socket.as_str()),
    ];
    repo.git_ai_with_env(args, &envs)
}

fn wait_for_daemon_sockets(repo: &TestRepo) {
    let control = daemon_control_socket_path(repo);
    let trace = daemon_trace_socket_path(repo);
    for _ in 0..200 {
        let status = send_control_request(
            &control,
            &ControlRequest::StatusFamily {
                repo_working_dir: repo.canonical_path().to_string_lossy().to_string(),
            },
        );
        if status.is_ok()
            && local_socket_connects_with_timeout(&trace, DAEMON_TEST_PROBE_TIMEOUT).is_ok()
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "daemon sockets did not become ready: control={}, trace={}",
        control.display(),
        trace.display()
    );
}

fn wait_for_daemon_latest_seq(repo: &TestRepo, min_seq: u64) {
    let control = daemon_control_socket_path(repo);
    let repo_working_dir = repo.canonical_path().to_string_lossy().to_string();
    for _ in 0..200 {
        let response = send_control_request(
            &control,
            &ControlRequest::StatusFamily {
                repo_working_dir: repo_working_dir.clone(),
            },
        )
        .expect("status request should succeed while waiting for traced command");
        let latest_seq = response
            .data
            .as_ref()
            .and_then(|data| data.get("latest_seq"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if latest_seq >= min_seq {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "daemon did not observe traced command for {}",
        repo.canonical_path().display()
    );
}

fn daemon_command_output(repo: &TestRepo, args: &[&str], cwd: &Path) -> Output {
    let mut command = Command::new(get_binary_path());
    command.args(args).current_dir(cwd);
    configure_test_home_env(&mut command, repo);
    configure_test_daemon_env(&mut command, repo);
    command
        .output()
        .expect("failed to invoke git-ai daemon command")
}

fn daemon_status_response(home_repo: &TestRepo, target_repo: &TestRepo) -> Value {
    let output = daemon_command_output(
        home_repo,
        &[
            "bg",
            "status",
            "--repo",
            target_repo.canonical_path().to_string_lossy().as_ref(),
        ],
        target_repo.path(),
    );
    assert!(
        output.status.success(),
        "daemon status command should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("daemon status output should be valid JSON")
}

fn assert_daemon_status_ok_after_launch_repo_removed(home_repo: &TestRepo, target_repo: &TestRepo) {
    let response = daemon_status_response(home_repo, target_repo);
    assert!(
        response.get("ok").and_then(Value::as_bool) == Some(true),
        "daemon status should remain ok after deleting launch repo cwd: {}",
        response
    );
}

#[allow(clippy::zombie_processes)]
fn start_daemon(repo: &TestRepo) {
    let mut daemon_cmd = Command::new(repos::test_repo::get_binary_path());
    daemon_cmd
        .arg("bg")
        .arg("run")
        .current_dir(repo.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_test_home_env(&mut daemon_cmd, repo);
    configure_test_daemon_env(&mut daemon_cmd, repo);
    daemon_cmd.spawn().expect("failed to start daemon");
}

fn shutdown_daemon(home_repo: &TestRepo) {
    let output = daemon_command_output(home_repo, &["bg", "shutdown"], home_repo.test_home_path());
    assert!(
        output.status.success(),
        "daemon shutdown command should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_for_child_exit(child: &mut Child) {
    for _ in 0..100 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
#[test]
fn control_request_protocol_preserves_framing_and_errors() {
    use git_ai::error::GitAiError;
    const TIMEOUT: Duration = Duration::from_millis(30);

    fn exchange(response: Option<(&'static str, u64)>) -> Result<ControlResponse, GitAiError> {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("control.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; b"{\"method\":\"ping\"}\n".len()];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"{\"method\":\"ping\"}\n");
            if let Some((body, delay)) = response {
                thread::sleep(Duration::from_millis(delay));
                let _ = stream.write_all(body.as_bytes());
            }
        });
        let result = send_control_request_with_timeout(&socket, &ControlRequest::Ping, TIMEOUT);
        server.join().unwrap();
        result
    }

    let missing = tempfile::tempdir().unwrap().path().join("missing");
    let connect_error = send_control_request_with_timeout(&missing, &ControlRequest::Ping, TIMEOUT)
        .unwrap_err()
        .to_string();
    assert!(connect_error.contains("timed out after 30ms connecting daemon socket"));

    assert!(exchange(Some(("{\"ok\":true}\n", 0))).unwrap().ok);
    let malformed = exchange(Some(("{\n", 0)));
    assert!(matches!(malformed, Err(GitAiError::JsonError(_))));
    let error = |response| exchange(response).unwrap_err().to_string();
    assert_eq!(
        error(Some(("\n", 0))),
        "Generic error: empty daemon control response"
    );
    assert!(error(None).contains("closed without a response"));
    assert!(
        error(Some(("{\"ok\":true}\n", 80)))
            .contains("timed out after 30ms reading daemon response")
    );
}

#[test]
fn install_hooks_async_mode_sets_daemon_trace2_global_config() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let output = git_ai_with_daemon_env(&repo, &["install-hooks", "--dry-run=false"])
        .expect("install-hooks should succeed");

    assert!(
        !output.contains("trace2.eventTarget") && !output.contains("trace2.eventNesting"),
        "install preflight should run silently without trace2 config output"
    );

    let expected_trace_socket = daemon_trace_socket_path(&repo);
    let expected_target = DaemonConfig::trace2_event_target_for_path(&expected_trace_socket);

    let target = read_global_git_config(&repo, "trace2.eventTarget");
    let nesting = read_global_git_config(&repo, "trace2.eventNesting");

    assert_eq!(target.as_deref(), Some(expected_target.as_str()));
    assert_eq!(nesting.as_deref(), Some("0"));
}

#[test]
fn install_hooks_async_mode_dry_run_does_not_write_trace2_global_config() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    git_ai_with_daemon_env(&repo, &["install-hooks", "--dry-run=true"])
        .expect("install-hooks dry-run should succeed");

    let target = read_global_git_config(&repo, "trace2.eventTarget");
    let nesting = read_global_git_config(&repo, "trace2.eventNesting");

    assert!(
        target.is_none(),
        "install-hooks dry-run should not set trace2.eventTarget"
    );
    assert!(
        nesting.is_none(),
        "install-hooks dry-run should not set trace2.eventNesting"
    );
}

#[test]
fn install_hooks_async_mode_trace2_target_routes_real_git_trace_to_daemon() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    git_ai_with_daemon_env(&repo, &["install-hooks", "--dry-run=false"])
        .expect("install-hooks should succeed");

    let start_output = daemon_command_output(&repo, &["bg", "start"], repo.path());
    assert!(
        start_output.status.success(),
        "daemon start should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&start_output.stdout),
        String::from_utf8_lossy(&start_output.stderr)
    );
    wait_for_daemon_sockets(&repo);

    // Use a mutating git command so the trace2 events flow through the full
    // ingest pipeline and increment applied_seq. Readonly commands (like
    // `git status`) are discarded by the daemon's readonly fast-path before
    // reaching the ingest queue, so they never advance latest_seq.
    let mut git_command = Command::new(real_git_executable());
    git_command.args(["config", "--local", "git-ai.tracing-test", "1"]);
    git_command.current_dir(repo.path());
    configure_test_home_env(&mut git_command, &repo);
    let git_output = git_command
        .output()
        .expect("failed to run traced git config");
    assert!(
        git_output.status.success(),
        "traced git config should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&git_output.stdout),
        String::from_utf8_lossy(&git_output.stderr)
    );

    wait_for_daemon_latest_seq(&repo, 1);
    shutdown_daemon(&repo);
}

#[test]
fn async_mode_checkpoint_starts_daemon_when_down() {
    // Test builds disable auto-spawning daemons from ensure_daemon_running
    // to prevent process storms under parallel test load. This test verifies
    // production-only auto-start behavior, so we manually start the daemon
    // and then verify the checkpoint delegates to it.
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    write_daemon_config(&repo);

    let control = daemon_control_socket_path(&repo);
    let trace = daemon_trace_socket_path(&repo);
    let _ = fs::remove_file(&control);
    let _ = fs::remove_file(&trace);

    // Manually start the daemon (production auto-start is disabled in test builds)
    start_daemon(&repo);
    wait_for_daemon_sockets(&repo);

    fs::write(
        repo.path().join("async-checkpoint.txt"),
        "async checkpoint\n",
    )
    .expect("failed to write async checkpoint file");

    let output = repo
        .git_ai(&["checkpoint", "mock_ai", "async-checkpoint.txt"])
        .expect("async mode checkpoint should succeed");

    assert!(
        !output.contains("[BENCHMARK] Starting checkpoint run"),
        "async mode checkpoint should delegate to daemon instead of running synchronously: {}",
        output
    );

    assert_daemon_status_ok_after_launch_repo_removed(&repo, &repo);
    shutdown_daemon(&repo);
}

#[test]
fn daemon_status_does_not_self_emit_trace2_events() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    fs::create_dir_all(repo.test_home_path()).expect("failed to create test HOME directory");
    let trace_target = DaemonConfig::trace2_event_target_for_path(&daemon_trace_socket_path(&repo));

    let mut set_target_command = Command::new(real_git_executable());
    set_target_command.args(["config", "--global", "trace2.eventTarget", &trace_target]);
    set_target_command.current_dir(repo.path());
    configure_test_home_env(&mut set_target_command, &repo);
    let set_target = set_target_command
        .output()
        .expect("failed to set global trace2.eventTarget");
    assert!(
        set_target.status.success(),
        "setting trace2.eventTarget failed: stdout={} stderr={}",
        String::from_utf8_lossy(&set_target.stdout),
        String::from_utf8_lossy(&set_target.stderr)
    );

    let mut set_nesting_command = Command::new(real_git_executable());
    set_nesting_command.args(["config", "--global", "trace2.eventNesting", "0"]);
    set_nesting_command.current_dir(repo.path());
    configure_test_home_env(&mut set_nesting_command, &repo);
    let set_nesting = set_nesting_command
        .output()
        .expect("failed to set global trace2.eventNesting");
    assert!(
        set_nesting.status.success(),
        "setting trace2.eventNesting failed: stdout={} stderr={}",
        String::from_utf8_lossy(&set_nesting.stdout),
        String::from_utf8_lossy(&set_nesting.stderr)
    );

    let mut daemon_cmd = Command::new(repos::test_repo::get_binary_path());
    daemon_cmd
        .arg("bg")
        .arg("run")
        .current_dir(repo.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_test_home_env(&mut daemon_cmd, &repo);
    configure_test_daemon_env(&mut daemon_cmd, &repo);
    let mut daemon = daemon_cmd.spawn().expect("failed to start daemon");

    wait_for_daemon_sockets(&repo);

    let repo_working_dir = repo.canonical_path().to_string_lossy().to_string();
    let status_response = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::StatusFamily { repo_working_dir },
    )
    .expect("status request failed");
    assert!(status_response.ok, "daemon status should succeed");
    let status_data = status_response.data.expect("status response missing data");
    let latest_seq = status_data
        .get("latest_seq")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(u64::MAX);

    assert_eq!(
        latest_seq, 0,
        "daemon status should not create self-trace events when global trace2 target points to daemon"
    );

    let _ = send_control_request(
        &daemon_control_socket_path(&repo),
        &ControlRequest::Shutdown,
    );
    for _ in 0..100 {
        match daemon.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn daemon_run_survives_deleted_launch_repo_cwd() {
    let launch_repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let target_repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let mut daemon_cmd = Command::new(get_binary_path());
    daemon_cmd
        .arg("bg")
        .arg("run")
        .current_dir(launch_repo.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_test_home_env(&mut daemon_cmd, &launch_repo);
    configure_test_daemon_env(&mut daemon_cmd, &launch_repo);
    let mut daemon = daemon_cmd.spawn().expect("failed to start daemon");

    wait_for_daemon_sockets(&launch_repo);
    fs::remove_dir_all(launch_repo.path()).expect("failed to remove launch repo");

    assert_daemon_status_ok_after_launch_repo_removed(&launch_repo, &target_repo);

    shutdown_daemon(&launch_repo);
    wait_for_child_exit(&mut daemon);
}

#[test]
fn daemon_start_survives_deleted_launch_repo_cwd() {
    let launch_repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let target_repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let output = daemon_command_output(&launch_repo, &["bg", "start"], launch_repo.path());
    assert!(
        output.status.success(),
        "daemon start should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    wait_for_daemon_sockets(&launch_repo);
    fs::remove_dir_all(launch_repo.path()).expect("failed to remove launch repo");

    assert_daemon_status_ok_after_launch_repo_removed(&launch_repo, &target_repo);

    shutdown_daemon(&launch_repo);
}

/// Helper: send a ControlRequest over an existing buffered stream and read one response line.
fn send_on_persistent_conn<R: Read + Write>(
    reader: &mut BufReader<R>,
    request: &ControlRequest,
) -> ControlResponse {
    let mut body = serde_json::to_vec(request).expect("serialize request");
    body.push(b'\n');
    reader
        .get_mut()
        .write_all(&body)
        .expect("write request to daemon");
    reader.get_mut().flush().expect("flush request");
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("read response from daemon");
    assert!(
        !line.trim().is_empty(),
        "daemon response should not be empty"
    );
    serde_json::from_str::<ControlResponse>(line.trim()).expect("parse daemon response")
}

/// Integration test: verifies that a persistent control socket connection can
/// deliver telemetry envelopes and CAS payloads to the daemon, and that the
/// daemon acknowledges each request with `ok: true` without closing the
/// connection between requests.
#[test]
fn daemon_telemetry_and_cas_over_persistent_connection() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    // Start the daemon
    let start_output = daemon_command_output(&repo, &["bg", "start"], repo.path());
    assert!(
        start_output.status.success(),
        "daemon start should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&start_output.stdout),
        String::from_utf8_lossy(&start_output.stderr)
    );
    wait_for_daemon_sockets(&repo);

    // Open a single persistent connection (mirrors the shared handle in telemetry_handle.rs)
    let control_path = daemon_control_socket_path(&repo);
    let stream = open_local_socket_stream_with_timeout(&control_path, Duration::from_secs(2))
        .expect("should connect to daemon control socket");
    let mut reader = BufReader::new(stream);

    // 1. Send telemetry envelopes (Message + Error variants)
    let telemetry_req = ControlRequest::SubmitTelemetry {
        envelopes: vec![
            TelemetryEnvelope::Message {
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                message: "integration test message".to_string(),
                level: "info".to_string(),
                context: None,
            },
            TelemetryEnvelope::Error {
                timestamp: "2026-01-01T00:00:01Z".to_string(),
                message: "integration test error event".to_string(),
                context: None,
            },
        ],
    };
    let resp = send_on_persistent_conn(&mut reader, &telemetry_req);
    assert!(resp.ok, "telemetry submit should succeed: {:?}", resp.error);

    // 2. Send CAS payloads over the *same* connection
    let cas_req = ControlRequest::SubmitCas {
        records: vec![
            CasSyncPayload {
                hash: "abc123".to_string(),
                data: "test cas data".to_string(),
                metadata: None,
            },
            CasSyncPayload {
                hash: "def456".to_string(),
                data: "more cas data".to_string(),
                metadata: Some("test-meta".to_string()),
            },
        ],
    };
    let resp = send_on_persistent_conn(&mut reader, &cas_req);
    assert!(resp.ok, "CAS submit should succeed: {:?}", resp.error);

    // 3. Send another batch of telemetry to confirm the connection stays alive
    let telemetry_req2 = ControlRequest::SubmitTelemetry {
        envelopes: vec![TelemetryEnvelope::Error {
            timestamp: "2026-01-01T00:00:02Z".to_string(),
            message: "integration test error".to_string(),
            context: None,
        }],
    };
    let resp = send_on_persistent_conn(&mut reader, &telemetry_req2);
    assert!(
        resp.ok,
        "second telemetry submit should succeed on persistent connection: {:?}",
        resp.error
    );

    // 4. Verify the daemon is still healthy via a status request on the same conn
    let status_req = ControlRequest::StatusFamily {
        repo_working_dir: repo.canonical_path().to_string_lossy().to_string(),
    };
    let resp = send_on_persistent_conn(&mut reader, &status_req);
    assert!(
        resp.ok,
        "status request should succeed on persistent connection: {:?}",
        resp.error
    );

    // Clean up
    drop(reader);
    shutdown_daemon(&repo);
}
