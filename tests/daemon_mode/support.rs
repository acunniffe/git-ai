use super::repos;

#[path = "completion.rs"]
mod completion;

#[path = "lifecycle.rs"]
mod lifecycle;

#[path = "trace_operations.rs"]
mod trace_operations;

#[path = "reflog_rewrites.rs"]
mod reflog_rewrites;

#[path = "pull_operations.rs"]
mod pull_operations;

#[path = "checkpoint.rs"]
mod checkpoint;

#[path = "trace_listener.rs"]
mod trace_listener;

#[path = "load.rs"]
mod load;

#[path = "memory_watchdog.rs"]
mod memory_watchdog;

#[path = "family_concurrency.rs"]
mod family_concurrency;

#[path = "outbox_replay.rs"]
mod outbox_replay;

use git_ai::config::{NotesBackendConfig, NotesBackendKind};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use git_ai::model::checkpoint_delivery::CHECKPOINT_DELIVERY_SCHEMA_VERSION;
#[cfg(unix)]
use git_ai::model::repository::bash_history_db::BashHistoryDatabase;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use git_ai::model::repository::checkpoint_outbox::{
    candidate_roots, decode_delivery, ready_filename,
};
use git_ai::model::working_log::CheckpointKind;
#[cfg(not(windows))]
use git_ai::operations::commands::checkpoint_agent::orchestrator::{
    BaseCommit, CheckpointFile, CheckpointRequest,
};
#[cfg(not(windows))]
use git_ai::operations::daemon::checkpoint::PreparedPathRole;
#[cfg(not(windows))]
use git_ai::operations::daemon::send_control_request_with_timeout;
use git_ai::operations::daemon::{
    ControlRequest, DaemonConfig, DaemonLock, local_socket_connects_with_timeout,
    open_local_socket_stream_with_timeout, read_daemon_pid, send_control_request,
};
use repos::test_file::ExpectedLineExt;
use repos::test_repo::{
    DAEMON_SPAWN_LOADER_RETRY_ATTEMPTS, DaemonTestCompletionLogEntry, DaemonTestScope,
    RawGitCommand, TestRepo, get_binary_path, is_windows_loader_init_failure, real_git_executable,
};
use serde_json::Value;
use serde_json::json;
use serial_test::serial;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

const DAEMON_TEST_PROBE_TIMEOUT: Duration = Duration::from_millis(100);

/// Outcome of a failed `DaemonGuard` readiness wait: a transient Windows loader
/// hiccup (respawn) versus a genuine failure (fail loudly).
enum DaemonReadyOutcome {
    LoaderInitFailure(String),
    Fatal(String),
}

fn daemon_control_socket_path(repo: &TestRepo) -> PathBuf {
    repo.daemon_control_socket_path()
}

fn daemon_trace_socket_path(repo: &TestRepo) -> PathBuf {
    repo.daemon_trace_socket_path()
}

fn daemon_lock_path(repo: &TestRepo) -> PathBuf {
    DaemonConfig::from_home(&repo.daemon_home_path()).lock_path
}

#[cfg(unix)]
struct ColdDaemonSocketPaths {
    directory: PathBuf,
    control: PathBuf,
    trace: PathBuf,
}

#[cfg(unix)]
impl ColdDaemonSocketPaths {
    fn new(repo: &TestRepo) -> Self {
        let test_key = repo
            .test_home_path()
            .file_name()
            .expect("test home should have a final path component");
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("bs")
            .join(test_key);
        fs::create_dir_all(&directory).expect("cold-daemon socket directory should be creatable");
        let control = directory.join("c");
        let trace = directory.join("t");
        assert!(
            control.as_os_str().as_encoded_bytes().len() < 100
                && trace.as_os_str().as_encoded_bytes().len() < 100,
            "cold-daemon test socket paths must stay below Unix socket limits"
        );
        Self {
            directory,
            control,
            trace,
        }
    }
}

#[cfg(unix)]
impl Drop for ColdDaemonSocketPaths {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ready_checkpoint_outbox_records(repo: &TestRepo) -> Vec<PathBuf> {
    let daemon_config = DaemonConfig::from_home(&repo.daemon_home_path());
    let roots = candidate_roots(
        &daemon_config.internal_dir,
        None,
        &std::env::temp_dir(),
        unsafe { libc::geteuid() },
    )
    .expect("test daemon paths should derive valid checkpoint outbox roots");
    let mut records = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        records.extend(
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "ready")
                }),
        );
    }
    records.sort();
    records
}

#[allow(clippy::zombie_processes)]
fn start_daemon_for_repo(repo: &TestRepo) {
    let daemon_home = repo.daemon_home_path();
    let control_socket_path = daemon_control_socket_path(repo);
    let trace_socket_path = daemon_trace_socket_path(repo);
    let mut command = Command::new(get_binary_path());
    command
        .arg("bg")
        .arg("run")
        .current_dir(repo.path())
        .env("GIT_AI_TEST_DB_PATH", repo.test_db_path())
        .env("GITAI_TEST_DB_PATH", repo.test_db_path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_test_home_env(&mut command, repo.test_home_path());
    configure_test_daemon_env(
        &mut command,
        &daemon_home,
        &control_socket_path,
        &trace_socket_path,
    );
    command.spawn().expect("failed to spawn daemon for repo");

    let repo_workdir = repo_workdir_string(repo);
    for _ in 0..200 {
        if send_control_request(
            &control_socket_path,
            &ControlRequest::StatusFamily {
                repo_working_dir: repo_workdir.clone(),
            },
        )
        .is_ok()
            && local_socket_connects_with_timeout(&trace_socket_path, DAEMON_TEST_PROBE_TIMEOUT)
                .is_ok()
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "daemon did not become ready at {}",
        control_socket_path.display()
    );
}

fn get_rss_kb(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb_str = rest.trim().trim_end_matches(" kB").trim();
            return kb_str.parse().ok();
        }
    }
    None
}

fn send_trace_frames(trace_socket_path: &Path, payloads: &[Value]) {
    let mut stream =
        open_local_socket_stream_with_timeout(trace_socket_path, DAEMON_TEST_PROBE_TIMEOUT)
            .expect("failed to connect to trace socket");
    for payload in payloads {
        let raw = serde_json::to_string(payload).expect("failed to serialize trace payload");
        stream
            .write_all(raw.as_bytes())
            .expect("failed to write trace payload");
        stream
            .write_all(b"\n")
            .expect("failed to write trace newline");
    }
    stream.flush().expect("failed to flush trace payloads");
}

fn trace_atexit_frame(sid: &str, code: i32, time_ns: u64) -> Value {
    json!({
        "event": "atexit",
        "sid": sid,
        "code": code,
        "time_ns": time_ns,
    })
}

struct TraceCommandFrames {
    sid: String,
    argv: Vec<String>,
    worktree: String,
    repo: String,
    start_time_ns: u64,
    exit_code: i32,
}

impl TraceCommandFrames {
    fn new(sid: &str, argv: &[&str], worktree: &str, repo: &str, start_time_ns: u64) -> Self {
        Self {
            sid: sid.to_string(),
            argv: argv.iter().map(|arg| (*arg).to_string()).collect(),
            worktree: worktree.to_string(),
            repo: repo.to_string(),
            start_time_ns,
            exit_code: 0,
        }
    }

    fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.exit_code = exit_code;
        self
    }

    fn into_frames(self) -> Vec<Value> {
        let repository_time_ns = self.start_time_ns + 1;
        let exit_time_ns = self.start_time_ns + 100;
        let atexit_time_ns = self.start_time_ns + 101;

        vec![
            json!({
                "event": "start",
                "sid": &self.sid,
                "argv": &self.argv,
                "time_ns": self.start_time_ns,
            }),
            json!({
                "event": "def_repo",
                "sid": &self.sid,
                "worktree": &self.worktree,
                "repo": &self.repo,
                "time_ns": repository_time_ns,
            }),
            json!({
                "event": "exit",
                "sid": &self.sid,
                "code": self.exit_code,
                "time_ns": exit_time_ns,
            }),
            trace_atexit_frame(&self.sid, self.exit_code, atexit_time_ns),
        ]
    }
}

#[test]
fn trace_command_frames_emit_a_complete_deterministic_lifecycle() {
    let frames = TraceCommandFrames::new(
        "trace-command-frames",
        &["git", "status"],
        "/repo",
        "/repo/.git",
        1_000,
    )
    .with_exit_code(1)
    .into_frames();

    assert_eq!(
        frames,
        vec![
            json!({
                "event": "start",
                "sid": "trace-command-frames",
                "argv": ["git", "status"],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "trace-command-frames",
                "worktree": "/repo",
                "repo": "/repo/.git",
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "trace-command-frames",
                "code": 1,
                "time_ns": 1_100u64,
            }),
            json!({
                "event": "atexit",
                "sid": "trace-command-frames",
                "code": 1,
                "time_ns": 1_101u64,
            }),
        ],
    );
}

#[cfg(not(windows))]
fn write_trace_frames_to_stream(stream: &mut impl Write, payloads: &[Value]) {
    for payload in payloads {
        let raw = serde_json::to_string(payload).expect("failed to serialize trace payload");
        stream
            .write_all(raw.as_bytes())
            .expect("failed to write trace payload");
        stream
            .write_all(b"\n")
            .expect("failed to write trace newline");
    }
    stream.flush().expect("failed to flush trace payloads");
}

fn repo_workdir_string(repo: &TestRepo) -> String {
    repo.path().to_string_lossy().to_string()
}

struct ScopedEnvVar {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl ScopedEnvVar {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        unsafe {
            if let Some(previous) = self.previous.as_ref() {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

struct MockApiServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    rx: mpsc::Receiver<Value>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockApiServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind mock API server");
        listener
            .set_nonblocking(true)
            .expect("failed to set nonblocking listener");
        let addr = listener.local_addr().expect("failed to read listener addr");
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);

        let thread = thread::spawn(move || {
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        handle_http_connection(stream, &tx);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("mock API accept failed: {}", error),
                }
            }
        });

        Self {
            base_url: format!("http://{}", addr),
            stop,
            rx,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Collect all requests captured by the mock so far.
    fn collect_requests(&mut self) -> Vec<Value> {
        let mut requests = Vec::new();
        while let Ok(request) = self.rx.try_recv() {
            requests.push(request);
        }
        requests
    }
}

impl Drop for MockApiServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.trim_start_matches("http://"));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_http_connection(mut stream: TcpStream, tx: &mpsc::Sender<Value>) {
    let Some((path, body)) = read_http_request(&mut stream) else {
        return;
    };

    let request_json: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));

    let response_body = match path.as_str() {
        "/worker/cas/upload" => {
            let _ = tx.send(json!({ "path": path, "body": request_json }));
            let hashes = request_json["objects"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|object| object["hash"].as_str().map(|hash| hash.to_string()))
                .collect::<Vec<_>>();
            json!({
                "results": hashes.iter().map(|hash| {
                    json!({
                        "hash": hash,
                        "status": "ok"
                    })
                }).collect::<Vec<_>>(),
                "success_count": hashes.len(),
                "failure_count": 0
            })
            .to_string()
        }
        "/worker/metrics/upload" => {
            let _ = tx.send(json!({ "path": path, "body": request_json }));
            json!({ "errors": [] }).to_string()
        }
        "/worker/logs/upload" => {
            let accepted = request_json["events"].as_array().map_or(0, Vec::len);
            let _ = tx.send(json!({ "path": path, "body": request_json }));
            json!({
                "accepted": accepted,
                "dropped": 0,
                "enqueued": true,
                "errors": []
            })
            .to_string()
        }
        "/worker/notes/upload" => {
            let _ = tx.send(json!({ "path": path, "body": request_json }));
            let success_count = request_json["entries"]
                .as_array()
                .map(|entries| entries.len())
                .unwrap_or(0);
            json!({
                "success_count": success_count,
                "failure_count": 0
            })
            .to_string()
        }
        _ => "{}".to_string(),
    };

    write_http_response(&mut stream, response_body.as_bytes());
}

fn read_http_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("failed to set mock API read timeout");

    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_header_end(&buffer) {
            break end;
        }
    };

    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let request_line = headers.lines().next()?;
    let path = request_line.split_whitespace().nth(1)?.to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
        })
        .unwrap_or(0);

    while buffer.len() - header_end < content_length {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    Some((
        path,
        buffer[header_end..header_end + content_length].to_vec(),
    ))
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|idx| idx + 4)
}

fn write_http_response(stream: &mut TcpStream, body: &[u8]) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .expect("failed to write mock API response headers");
    stream
        .write_all(body)
        .expect("failed to write mock API response body");
    stream.flush().expect("failed to flush mock API response");
}

fn configure_test_home_env(command: &mut Command, test_home: &Path) {
    command.env("HOME", test_home);
    command.env("GIT_CONFIG_GLOBAL", test_home.join(".gitconfig"));
    // Redirect XDG_CONFIG_HOME so git does not read the real user's
    // $XDG_CONFIG_HOME/git/config (which may contain filter drivers,
    // aliases, or other settings that break test isolation).
    command.env("XDG_CONFIG_HOME", test_home.join(".config"));
    // Suppress system-level git config (e.g., Xcode credential helpers)
    // that could interfere with test isolation.
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    // Sanitize PATH to remove directories containing the Nix git-ai
    // wrapper.  When the wrapper (a release build) runs with HOME
    // pointing to the test home it starts a background daemon at
    // the test socket path, poisoning the test environment.
    if let Ok(path) = std::env::var("PATH") {
        let sanitized: Vec<&str> = path
            .split(':')
            .filter(|dir| {
                // Keep only dirs that do NOT contain a git-ai wrapper
                // (heuristic: skip dirs where the `git` binary is a
                //  shell-script wrapper for git-ai, or a symlink to git-ai).
                let git_path = std::path::Path::new(dir).join("git");
                if git_path.is_file() || git_path.is_symlink() {
                    if let Ok(contents) = std::fs::read_to_string(&git_path)
                        && contents.contains("git-ai")
                    {
                        return false;
                    }
                    if let Ok(target) = std::fs::read_link(&git_path)
                        && target.to_string_lossy().contains("git-ai")
                    {
                        return false;
                    }
                    if let Ok(canonical) = git_path.canonicalize()
                        && canonical.to_string_lossy().contains("git-ai")
                    {
                        return false;
                    }
                }
                true
            })
            .collect();
        command.env("PATH", sanitized.join(":"));
    }
    #[cfg(windows)]
    {
        command.env("USERPROFILE", test_home);
        command.env("APPDATA", test_home.join("AppData").join("Roaming"));
        command.env("LOCALAPPDATA", test_home.join("AppData").join("Local"));
    }
}

fn configure_test_daemon_env(
    command: &mut Command,
    daemon_home: &Path,
    control_socket_path: &Path,
    trace_socket_path: &Path,
) {
    command.env("GIT_AI_DAEMON_HOME", daemon_home);
    command.env("GIT_AI_DAEMON_CONTROL_SOCKET", control_socket_path);
    command.env("GIT_AI_DAEMON_TRACE_SOCKET", trace_socket_path);
}

struct DaemonGuard {
    child: Child,
    control_socket_path: PathBuf,
    trace_socket_path: PathBuf,
    repo_working_dir: String,
    stderr_log_path: PathBuf,
}

impl DaemonGuard {
    fn start(repo: &TestRepo) -> Self {
        Self::start_with_env(repo, &[])
    }

    fn start_with_env(repo: &TestRepo, extra_env: &[(&str, &str)]) -> Self {
        let daemon_home = repo.daemon_home_path();
        let control_socket_path = daemon_control_socket_path(repo);
        let trace_socket_path = daemon_trace_socket_path(repo);
        let stderr_log_path = daemon_home.join("daemon-guard.stderr.log");
        fs::create_dir_all(&daemon_home).expect("failed to create daemon test home");
        let stderr_log = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&stderr_log_path)
            .expect("failed to create daemon stderr log");
        let mut command = Command::new(get_binary_path());
        command
            .arg("bg")
            .arg("run")
            .current_dir(repo.path())
            .env("GIT_AI_TEST_DB_PATH", repo.test_db_path())
            .env("GITAI_TEST_DB_PATH", repo.test_db_path())
            .stdout(Stdio::null())
            .stderr(
                stderr_log
                    .try_clone()
                    .expect("failed to clone daemon stderr log"),
            );
        for (key, value) in extra_env {
            command.env(key, value);
        }
        configure_test_home_env(&mut command, repo.test_home_path());
        configure_test_daemon_env(
            &mut command,
            &daemon_home,
            &control_socket_path,
            &trace_socket_path,
        );

        // Respawn loop: a Windows `STATUS_DLL_INIT_FAILED` exit means the OS
        // loader never started the daemon process (a hosted-Windows-runner
        // hiccup), so retry. Any other early exit / timeout panics immediately.
        let mut attempt = 0;
        loop {
            let child = command.spawn().expect("failed to spawn git-ai subprocess");
            let mut daemon = Self {
                child,
                control_socket_path: control_socket_path.clone(),
                trace_socket_path: trace_socket_path.clone(),
                repo_working_dir: repo_workdir_string(repo),
                stderr_log_path: stderr_log_path.clone(),
            };
            match daemon.wait_until_ready() {
                Ok(()) => return daemon,
                Err(DaemonReadyOutcome::LoaderInitFailure(message)) => {
                    let _ = daemon.child.kill();
                    let _ = daemon.child.wait();
                    attempt += 1;
                    if attempt < DAEMON_SPAWN_LOADER_RETRY_ATTEMPTS {
                        eprintln!(
                            "[test-harness] daemon loader init failed (attempt {}/{}), respawning: {}",
                            attempt, DAEMON_SPAWN_LOADER_RETRY_ATTEMPTS, message
                        );
                        continue;
                    }
                    panic!("{}", message);
                }
                Err(DaemonReadyOutcome::Fatal(message)) => {
                    let _ = daemon.child.kill();
                    let _ = daemon.child.wait();
                    panic!("{}", message);
                }
            }
        }
    }

    fn wait_until_ready(&mut self) -> Result<(), DaemonReadyOutcome> {
        for _ in 0..200 {
            if let Some(status) = self
                .child
                .try_wait()
                .expect("failed to poll daemon process status")
            {
                let message = format!("daemon exited before becoming ready: {}", status);
                if is_windows_loader_init_failure(&status) {
                    return Err(DaemonReadyOutcome::LoaderInitFailure(message));
                }
                return Err(DaemonReadyOutcome::Fatal(message));
            }
            let status = send_control_request(
                &self.control_socket_path,
                &ControlRequest::StatusFamily {
                    repo_working_dir: self.repo_working_dir.clone(),
                },
            );
            if status.is_ok()
                && local_socket_connects_with_timeout(
                    &self.trace_socket_path,
                    DAEMON_TEST_PROBE_TIMEOUT,
                )
                .is_ok()
            {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }
        Err(DaemonReadyOutcome::Fatal(format!(
            "daemon did not become ready at {}",
            self.control_socket_path.display()
        )))
    }

    fn shutdown(&mut self) {
        if self
            .child
            .try_wait()
            .expect("failed polling daemon process")
            .is_some()
        {
            return;
        }

        let _ = send_control_request(&self.control_socket_path, &ControlRequest::Shutdown);

        for _ in 0..200 {
            if self
                .child
                .try_wait()
                .expect("failed polling daemon process")
                .is_some()
            {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn stderr_contents(&self) -> String {
        fs::read_to_string(&self.stderr_log_path).unwrap_or_default()
    }
}

fn git_trace_env(trace_socket_path: &Path) -> [(&'static str, String); 2] {
    [
        (
            "GIT_TRACE2_EVENT",
            DaemonConfig::trace2_event_target_for_path(trace_socket_path),
        ),
        ("GIT_TRACE2_EVENT_NESTING", "0".to_string()),
    ]
}

fn traced_git_with_env(
    repo: &TestRepo,
    args: &[&str],
    envs: &[(&str, &str)],
    expected_top_level_completions: &mut u64,
) -> Result<String, String> {
    *expected_top_level_completions += 1;
    repo.git_og_with_env(args, envs)
}

fn wait_for_expected_top_level_completions(
    repo: &TestRepo,
    baseline: u64,
    expected_top_level_completions: u64,
) {
    repo.wait_for_daemon_total_completion_count(
        baseline,
        baseline.saturating_add(expected_top_level_completions),
    );
}

fn completion_entries_for_command(
    repo: &TestRepo,
    command: &str,
) -> Vec<DaemonTestCompletionLogEntry> {
    repo.daemon_completion_entries()
        .into_iter()
        .filter(|entry| entry.primary_command.as_deref() == Some(command))
        .collect()
}

#[derive(Clone)]
struct WorkdirRaceHarness {
    test_home: PathBuf,
    test_db_path: PathBuf,
    daemon_home: PathBuf,
    control_socket_path: PathBuf,
    trace_socket_path: PathBuf,
}

impl WorkdirRaceHarness {
    fn new(repo: &TestRepo, trace_socket_path: PathBuf) -> Self {
        Self {
            test_home: repo.test_home_path().to_path_buf(),
            test_db_path: repo.test_db_path().to_path_buf(),
            daemon_home: repo.daemon_home_path(),
            control_socket_path: repo.daemon_control_socket_path(),
            trace_socket_path,
        }
    }

    fn run_traced_git(&self, workdir: &Path, args: &[&str]) {
        let output = RawGitCommand::in_working_dir(workdir, args)
            .configure(|command| configure_test_home_env(command, &self.test_home))
            .env("GIT_AI_TEST_DB_PATH", &self.test_db_path)
            .env("GITAI_TEST_DB_PATH", &self.test_db_path)
            .env(
                "GIT_TRACE2_EVENT",
                DaemonConfig::trace2_event_target_for_path(&self.trace_socket_path),
            )
            .env("GIT_TRACE2_EVENT_NESTING", "0")
            .output()
            .expect("failed to execute traced git command");
        assert!(
            output.status.success(),
            "traced git command failed in {}: git {} \nstdout:{}\nstderr:{}",
            workdir.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_delegated_checkpoint(&self, workdir: &Path, file_rel: &str) {
        let mut command = Command::new(get_binary_path());
        command
            .args(["checkpoint", "mock_ai", file_rel])
            .current_dir(workdir);
        configure_test_home_env(&mut command, &self.test_home);
        configure_test_daemon_env(
            &mut command,
            &self.daemon_home,
            &self.control_socket_path,
            &self.trace_socket_path,
        );
        let output = command
            .env("GIT_AI_TEST_DB_PATH", &self.test_db_path)
            .env("GITAI_TEST_DB_PATH", &self.test_db_path)
            .env("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")
            .output()
            .expect("failed to execute delegated checkpoint");
        assert!(
            output.status.success(),
            "delegated checkpoint failed in {} for {} \nstdout:{}\nstderr:{}",
            workdir.display(),
            file_rel,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_ai_line_checkpoint_and_add(&self, workdir: &Path, file_rel: &str, line: &str) {
        fs::write(workdir.join(file_rel), format!("{line}\n"))
            .expect("failed writing ai line test file");
        self.run_delegated_checkpoint(workdir, file_rel);
        self.run_traced_git(workdir, &["add", file_rel]);
    }
}

fn unique_worktree_path(repo: &TestRepo, prefix: &str) -> PathBuf {
    repo.path().parent().unwrap_or(repo.path()).join(format!(
        "{}-{}",
        prefix,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
}

fn parse_blame_line(line: &str) -> (String, String) {
    if let Some(start_paren) = line.find('(')
        && let Some(end_paren) = line.find(')')
    {
        let author_section = &line[start_paren + 1..end_paren];
        let content = line[end_paren + 1..].trim().to_string();

        let parts: Vec<&str> = author_section.split_whitespace().collect();
        let mut author_parts = Vec::new();
        for part in parts {
            if part.chars().next().unwrap_or('a').is_ascii_digit() {
                break;
            }
            author_parts.push(part);
        }
        return (author_parts.join(" "), content);
    }
    ("unknown".to_string(), line.trim().to_string())
}

fn is_ai_author(author: &str) -> bool {
    let author_lower = author.to_lowercase();
    author_lower.contains("mock_ai")
        || author_lower.contains("claude")
        || author_lower.contains("cursor")
        || author_lower.contains("codex")
}

fn assert_blame_lines_for_workdir(
    repo: &TestRepo,
    workdir: &Path,
    file_rel: &str,
    expected: &[(String, bool)],
) {
    let blame_output = repo
        .git_ai_from_working_dir(workdir, &["blame", file_rel])
        .unwrap_or_else(|e| {
            panic!(
                "git-ai blame failed in {} for {}: {}",
                workdir.display(),
                file_rel,
                e
            )
        });
    let actual: Vec<(String, String)> = blame_output
        .lines()
        .filter(|line: &&str| !line.trim().is_empty())
        .map(parse_blame_line)
        .collect();
    assert_eq!(
        actual.len(),
        expected.len(),
        "line count mismatch for {} in {}\nblame:\n{}",
        file_rel,
        workdir.display(),
        blame_output
    );

    for (idx, ((author, content), (expected_content, expected_ai))) in
        actual.iter().zip(expected.iter()).enumerate()
    {
        assert_eq!(
            content,
            expected_content,
            "line {} content mismatch for {} in {}",
            idx + 1,
            file_rel,
            workdir.display()
        );
        let actual_ai = is_ai_author(author);
        assert_eq!(
            actual_ai,
            *expected_ai,
            "line {} attribution mismatch for {} in {} (author='{}', line='{}')",
            idx + 1,
            file_rel,
            workdir.display(),
            author,
            content
        );
    }
}

fn assert_single_ai_line_for_workdir(repo: &TestRepo, workdir: &Path, file_rel: &str, line: &str) {
    assert_blame_lines_for_workdir(repo, workdir, file_rel, &[(line.to_string(), true)]);
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn claude_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("example-claude-code.jsonl")
}

fn assert_post_commit_uploads_prompt_cas() {
    let mock_api = MockApiServer::start();
    let _api_base_url = ScopedEnvVar::set("GIT_AI_API_BASE_URL", mock_api.base_url());
    let _api_key = ScopedEnvVar::set("GIT_AI_API_KEY", "test-api-key");

    // These tests depend on per-test API env vars being visible to the daemon.
    // A shared daemon may already be running from an earlier test with different env.
    let mut repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
        patch.prompt_storage = Some("default".to_string());
        patch.telemetry_oss_disabled = Some(true);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("test.ts");
    fs::write(&file_path, "const x = 1;\n").expect("failed to write initial file");
    repo.stage_all_and_commit("Initial commit")
        .expect("initial commit should succeed");

    let transcript_path = repo_root.join("claude-session.jsonl");
    fs::copy(claude_fixture_path(), &transcript_path).expect("failed to copy transcript fixture");

    let hook_input = json!({
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();

    fs::write(&file_path, "const x = 1;\n// ai line one\n").expect("failed to write AI edit");
    repo.git_ai(&["checkpoint", "claude", "--hook-input", &hook_input])
        .expect("checkpoint should succeed");

    let commit = repo
        .stage_all_and_commit("Add AI line")
        .expect("AI commit should succeed");

    // Sessions no longer upload messages to CAS - only prompts do.
    // Since claude checkpoints create sessions, not prompts, we don't expect a CAS upload.
    // Verify that the authorship note is created with a session record.
    let note = repo
        .read_authorship_note(&commit.commit_sha)
        .expect("commit should have authorship note");
    let log =
        git_ai::model::authorship_log_serialization::AuthorshipLog::deserialize_from_string(&note)
            .expect("authorship note should deserialize");
    // AI checkpoints now produce sessions (not prompts)
    let _session = log
        .metadata
        .sessions
        .values()
        .next()
        .expect("authorship note should contain one session");
    // Sessions no longer have messages or messages_url fields
}
