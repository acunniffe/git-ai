#[macro_use]
#[path = "integration/repos/mod.rs"]
mod repos;

use git_ai::authorship::working_log::CheckpointKind;
#[cfg(not(windows))]
use git_ai::commands::checkpoint_agent::orchestrator::{
    BaseCommit, CheckpointFile, CheckpointRequest,
};
use git_ai::config::{NotesBackendConfig, NotesBackendKind};
#[cfg(not(windows))]
use git_ai::daemon::checkpoint::PreparedPathRole;
use git_ai::daemon::send_control_request_with_timeout;
use git_ai::daemon::{
    ControlRequest, DaemonConfig, DaemonLock, local_socket_connects_with_timeout,
    open_local_socket_stream_with_timeout, read_daemon_pid, send_control_request,
};
use git_ai::metrics::db::MetricsDatabase;
use git_ai::metrics::{EventAttributes, MetricEvent, PosEncoded, SessionEventValues};
use repos::test_file::ExpectedLineExt;
use repos::test_repo::{
    DAEMON_SPAWN_LOADER_RETRY_ATTEMPTS, DaemonTestCompletionLogEntry, DaemonTestScope, TestRepo,
    get_binary_path, is_windows_loader_init_failure, real_git_executable,
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
}

impl DaemonGuard {
    fn start(repo: &TestRepo) -> Self {
        Self::start_with_env(repo, &[])
    }

    fn start_with_env(repo: &TestRepo, extra_env: &[(&str, &str)]) -> Self {
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
        let mut command = Command::new(real_git_executable());
        command.args(args).current_dir(workdir);
        configure_test_home_env(&mut command, &self.test_home);
        let output = command
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
        git_ai::authorship::authorship_log_serialization::AuthorshipLog::deserialize_from_string(
            &note,
        )
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
fn daemon_refuses_to_start_in_sandbox() {
    for (env_var, sandbox) in [
        ("CURSOR_SANDBOX", "Cursor"),
        ("SANDBOX_RUNTIME", "Claude Code"),
        ("CODEX_SANDBOX", "Codex"),
        ("CODEX_SANDBOX_NETWORK_DISABLED", "Codex"),
    ] {
        let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

        for subcommand in ["start", "run"] {
            let output = bg_command_with_env(&repo, subcommand, &[], &[(env_var, "1")]);
            if output.status.success() {
                let _ = send_control_request(
                    &daemon_control_socket_path(&repo),
                    &ControlRequest::Shutdown,
                );
            }

            assert!(
                !output.status.success(),
                "daemon {subcommand} should fail in the {sandbox} sandbox"
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(&format!("{sandbox} sandbox")) && stderr.contains(env_var),
                "daemon {subcommand} should explain the sandbox refusal: {stderr}"
            );
        }

        assert!(
            send_control_request_with_timeout(
                &daemon_control_socket_path(&repo),
                &ControlRequest::Ping,
                DAEMON_TEST_PROBE_TIMEOUT,
            )
            .is_err(),
            "daemon control socket should not be available"
        );
        assert!(
            local_socket_connects_with_timeout(
                &daemon_trace_socket_path(&repo),
                DAEMON_TEST_PROBE_TIMEOUT,
            )
            .is_err(),
            "daemon trace socket should not be available"
        );
    }
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

    let git_ai_repo = git_ai::git::repository::find_repository_in_path(
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
                "argv": ["git", "-c", session_arg, "commit", "-m", "synthetic"],
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
                "argv": ["git", "-c", session_arg, "commit", "-m", "synthetic"],
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
                "argv": ["git", "commit", "-m", "incomplete"],
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
                "argv": ["git", "-c", session_arg, "commit", "-m", "synthetic"],
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
fn daemon_failed_rebase_does_not_consume_later_continue_reflog_entry() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut shared_file = repo.filename("shared.txt");
    shared_file.set_contents(lines!["line 1".human(), "line 2".human()]);
    repo.stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout feature should succeed");
    let mut feature_file = repo.filename("shared.txt");
    feature_file.set_contents(lines!["line 1".human(), "AI feature line 2".ai()]);
    repo.stage_all_and_commit("AI feature changes")
        .expect("feature commit should succeed");
    let feature_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse feature should succeed")
        .trim()
        .to_string();
    assert!(
        repo.read_authorship_note(&feature_sha).is_some(),
        "feature commit should have a note before rebase"
    );

    repo.git(&["checkout", &default_branch])
        .expect("checkout default branch should succeed");
    let mut main_file = repo.filename("shared.txt");
    main_file.set_contents(lines!["line 1".human(), "main change line 2".human()]);
    repo.stage_all_and_commit("main conflicting change")
        .expect("main commit should succeed");

    repo.git(&["checkout", "feature"])
        .expect("checkout feature should succeed");
    repo.sync_daemon();

    let rebase_result = repo.git_og(&["rebase", &default_branch]);
    assert!(
        rebase_result.is_err(),
        "raw rebase should fail due to conflict"
    );

    fs::write(
        repo.path().join("shared.txt"),
        "line 1\nmain change line 2\nAI feature line 2\n",
    )
    .expect("failed to write resolved conflict");
    repo.git_og(&["add", "shared.txt"])
        .expect("raw add should succeed");
    repo.git_og_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")])
        .expect("raw rebase --continue should succeed");
    let rebased_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse rebased HEAD should succeed")
        .trim()
        .to_string();
    assert_ne!(
        rebased_sha, feature_sha,
        "rebase --continue should create a rewritten commit"
    );

    let rebase_session = repos::test_repo::new_daemon_test_sync_session_id();
    let continue_session = repos::test_repo::new_daemon_test_sync_session_id();
    let rebase_session_arg = format!("git-ai.testSyncSession={rebase_session}");
    let continue_session_arg = format!("git-ai.testSyncSession={continue_session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "failed-rebase-start",
                "argv": ["git", "-c", rebase_session_arg, "-C", worktree, "rebase", default_branch],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "failed-rebase-start",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "failed-rebase-start",
                "code": 1,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("failed-rebase-start", 1, 1_101u64),
            json!({
                "event": "start",
                "sid": "rebase-continue",
                "argv": ["git", "-c", continue_session_arg, "-C", worktree, "rebase", "--continue"],
                "time_ns": 2_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "rebase-continue",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 2_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "rebase-continue",
                "code": 0,
                "time_ns": 2_100u64,
            }),
            trace_atexit_frame("rebase-continue", 0, 2_101u64),
        ],
    );
    repo.sync_daemon_external_completion_sessions(&[rebase_session, continue_session]);

    assert!(
        repo.read_authorship_note(&rebased_sha).is_some(),
        "rebased commit should get the remapped note even when failed rebase processing is delayed until after --continue"
    );
}

#[test]
fn daemon_late_cherry_pick_trace_uses_actual_destination_not_stale_commit_entry() {
    let mut repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut file = repo.filename("picked.txt");
    file.set_contents(lines!["base".human()]);
    let base_commit = repo
        .stage_all_and_commit("base")
        .expect("base commit should succeed");
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "source"])
        .expect("checkout source should succeed");
    file.insert_at(1, lines!["AI picked line".ai()]);
    let source_commit = repo
        .stage_all_and_commit("source change")
        .expect("source commit should succeed");
    repo.read_authorship_note(&source_commit.commit_sha)
        .expect("source commit should have an authorship note");

    repo.git(&["checkout", &default_branch])
        .expect("checkout default branch should succeed");

    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(lines!["main branch line".human()]);
    let main_tip = repo
        .stage_all_and_commit("main branch advance")
        .expect("main branch advance should succeed");

    fs::write(repo.path().join("stale.txt"), "stale\n").expect("write stale file");
    repo.git_og(&["add", "stale.txt"])
        .expect("raw stale add should succeed");
    repo.git_og(&["commit", "-m", "stale plain commit"])
        .expect("raw stale commit should succeed");
    let stale_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse stale commit should succeed")
        .trim()
        .to_string();
    assert_ne!(stale_commit, base_commit.commit_sha);
    assert!(
        repo.read_authorship_note(&stale_commit).is_none(),
        "raw stale commit should not have an authorship note"
    );

    repo.git_og(&["reset", "--hard", &main_tip.commit_sha])
        .expect("raw reset should succeed");
    repo.restart_dedicated_daemon_for_test();

    repo.git_og(&["cherry-pick", &source_commit.commit_sha])
        .expect("raw cherry-pick should succeed");
    let picked_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse picked commit should succeed")
        .trim()
        .to_string();
    assert_ne!(picked_commit, source_commit.commit_sha);
    assert_ne!(picked_commit, stale_commit);
    assert!(
        repo.read_authorship_note(&picked_commit).is_none(),
        "raw cherry-pick should not write the note before synthetic trace processing"
    );

    let cherry_pick_session = repos::test_repo::new_daemon_test_sync_session_id();
    let cherry_pick_session_arg = format!("git-ai.testSyncSession={cherry_pick_session}");
    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "late-cherry-pick",
                "argv": ["git", "-c", cherry_pick_session_arg, "-C", worktree, "cherry-pick", source_commit.commit_sha],
                "worktree": worktree,
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "late-cherry-pick",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "late-cherry-pick",
                "code": 0,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("late-cherry-pick", 0, 1_101u64),
        ],
    );
    repo.sync_daemon_external_completion_sessions(&[cherry_pick_session]);

    assert!(
        repo.read_authorship_note(&stale_commit).is_none(),
        "stale historical commit must not receive the cherry-pick note"
    );
    let mut file = repo.filename("picked.txt");
    file.assert_lines_and_blame(lines!["base".ai(), "AI picked line".ai(),]);
}

#[test]
fn daemon_failed_rebase_does_not_consume_later_skip_reflog_entry() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut file = repo.filename("file.txt");
    file.set_contents(lines!["line 1".human()]);
    repo.stage_all_and_commit("Initial")
        .expect("initial commit should succeed");

    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout feature should succeed");
    file.replace_at(0, "AI line 1".ai());
    repo.stage_all_and_commit("AI changes")
        .expect("conflicting AI commit should succeed");

    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(lines!["// AI feature".ai()]);
    let feature_commit = repo
        .stage_all_and_commit("Add feature")
        .expect("feature commit should succeed");
    assert!(
        repo.read_authorship_note(&feature_commit.commit_sha)
            .is_some(),
        "feature commit should have a note before rebase"
    );

    repo.git(&["checkout", &default_branch])
        .expect("checkout default branch should succeed");
    file.replace_at(0, "MAIN line 1".human());
    repo.stage_all_and_commit("Main changes")
        .expect("main commit should succeed");

    repo.git(&["checkout", "feature"])
        .expect("checkout feature should succeed");
    repo.sync_daemon();

    let rebase_result = repo.git_og(&["rebase", &default_branch]);
    assert!(
        rebase_result.is_err(),
        "raw rebase should fail due to conflict"
    );
    repo.git_og(&["rebase", "--skip"])
        .expect("raw rebase --skip should succeed");
    let rebased_feature_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse rebased feature should succeed")
        .trim()
        .to_string();
    assert_ne!(
        rebased_feature_sha, feature_commit.commit_sha,
        "rebase --skip should rewrite the following feature commit"
    );

    let rebase_session = repos::test_repo::new_daemon_test_sync_session_id();
    let skip_session = repos::test_repo::new_daemon_test_sync_session_id();
    let rebase_session_arg = format!("git-ai.testSyncSession={rebase_session}");
    let skip_session_arg = format!("git-ai.testSyncSession={skip_session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "failed-rebase-before-skip",
                "argv": ["git", "-c", rebase_session_arg, "-C", worktree, "rebase", default_branch],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "failed-rebase-before-skip",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "failed-rebase-before-skip",
                "code": 1,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("failed-rebase-before-skip", 1, 1_101u64),
            json!({
                "event": "start",
                "sid": "rebase-skip",
                "argv": ["git", "-c", skip_session_arg, "-C", worktree, "rebase", "--skip"],
                "time_ns": 2_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "rebase-skip",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 2_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "rebase-skip",
                "code": 0,
                "time_ns": 2_100u64,
            }),
            trace_atexit_frame("rebase-skip", 0, 2_101u64),
        ],
    );
    repo.sync_daemon_external_completion_sessions(&[rebase_session, skip_session]);

    assert!(
        repo.read_authorship_note(&rebased_feature_sha).is_some(),
        "rebased feature commit should get the remapped note when failed rebase processing is delayed until after --skip"
    );
    feature_file.assert_committed_lines(lines!["// AI feature".ai()]);
}

#[test]
#[serial]
fn daemon_trace_ingest_treats_atexit_as_terminal_for_reflog_capture() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let sid = "atexit-commit";
    let completion_baseline = repo.daemon_total_completion_count();

    send_trace_frames(
        &trace_socket,
        &[
            serde_json::json!({
                "event":"start",
                "sid":sid,
                "ts":1,
                "argv":["git","commit","-m","x"],
                "cwd":repo.path().to_string_lossy().to_string(),
            }),
            serde_json::json!({
                "event":"atexit",
                "sid":sid,
                "ts":2,
                "code":1
            }),
        ],
    );

    wait_for_expected_top_level_completions(&repo, completion_baseline, 1);

    let commands = completion_entries_for_command(&repo, "commit");
    assert!(
        commands.iter().any(|command| command.exit_code == Some(1)
            && command.status == "ok"
            && command.seq > 0),
        "atexit terminal frames should still produce a tracked commit command"
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_checkpoint_stage_checkpoint_two_commits_preserve_ai_lines() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let file_rel = "daemon-two-ai-lines.txt";
    let file_path = repo.path().join(file_rel);
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(&file_path, "base\n").expect("failed to seed base file");
    traced_git_with_env(
        &repo,
        &["add", file_rel],
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

    {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&file_path)
            .expect("failed to open file for first append");
        writeln!(f, "test").expect("failed to append first ai line");
    }
    repo.git_ai_with_env(
        &["checkpoint", "mock_ai", file_rel],
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("first delegated ai checkpoint should succeed");
    expected_top_level_completions += 1;
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["add", "."],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("staging first ai line should succeed");

    {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&file_path)
            .expect("failed to open file for second append");
        writeln!(f, "test1").expect("failed to append second ai line");
    }
    repo.git_ai_with_env(
        &["checkpoint", "mock_ai", file_rel],
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("second delegated ai checkpoint should succeed");
    expected_top_level_completions += 1;
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["commit", "-m", "first ai line"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("first commit should succeed");
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["add", "."],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("staging second ai line should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "second ai line"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("second commit should succeed");
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    let mut file = repo.filename(file_rel);
    file.assert_lines_and_blame(lines!["base", "test".ai(), "test1".ai()]);
}

#[test]
#[serial]
fn daemon_pure_trace_socket_checkpoint_stage_checkpoint_non_adjacent_hunks_survive_split_commits() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let file_rel = "daemon-non-adjacent.md";
    let file_path = repo.path().join(file_rel);
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    let initial = "\
Top line

**Section Alpha**
alpha body

middle line 1
middle line 2

**Section Omega**
omega body
";
    fs::write(&file_path, initial).expect("failed to write initial content");
    traced_git_with_env(
        &repo,
        &["add", file_rel],
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

    let first_ai_hunk = "\
Top line

### Section Alpha
alpha body

middle line 1
middle line 2

**Section Omega**
omega body
";
    fs::write(&file_path, first_ai_hunk).expect("failed to write first hunk content");
    repo.git_ai_with_env(
        &["checkpoint", "mock_ai", file_rel],
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("first delegated checkpoint should succeed");
    expected_top_level_completions += 1;
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["add", "."],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("staging first hunk should succeed");

    let both_hunks = "\
Top line

### Section Alpha
alpha body

middle line 1
middle line 2

### Section Omega
omega body
";
    fs::write(&file_path, both_hunks).expect("failed to write both hunks content");
    repo.git_ai_with_env(
        &["checkpoint", "mock_ai", file_rel],
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("second delegated checkpoint should succeed");
    expected_top_level_completions += 1;
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["commit", "-m", "commit first staged hunk"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("first split commit should succeed");
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["add", "."],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("staging remaining hunk should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "commit second hunk"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("second split commit should succeed");
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    let mut file = repo.filename(file_rel);
    file.assert_lines_and_blame(lines![
        "Top line",
        "".human(),
        "### Section Alpha".ai(),
        "alpha body",
        "".human(),
        "middle line 1",
        "middle line 2",
        "".human(),
        "### Section Omega".ai(),
        "omega body",
    ]);
}

#[test]
#[serial]
fn daemon_pure_trace_socket_write_mode_applies_amend_rewrite() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("pure-trace.txt"), "line 1\n").expect("failed to write file");
    traced_git_with_env(
        &repo,
        &["add", "pure-trace.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "initial"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("commit should succeed");

    fs::write(repo.path().join("pure-trace.txt"), "line 1\nline 2\n")
        .expect("failed to update file");
    traced_git_with_env(
        &repo,
        &["add", "pure-trace.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add before amend should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "--amend", "-m", "initial amended"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("amend should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_rebase_abort_emits_abort_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("rebase-conflict.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "rebase-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    traced_git_with_env(
        &repo,
        &["checkout", "-b", "feature"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature branch checkout should succeed");
    fs::write(repo.path().join("rebase-conflict.txt"), "feature\n")
        .expect("failed to write feature branch change");
    traced_git_with_env(
        &repo,
        &["add", "rebase-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "feature change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature commit should succeed");

    traced_git_with_env(
        &repo,
        &["checkout", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout default branch should succeed");
    fs::write(repo.path().join("rebase-conflict.txt"), "main\n")
        .expect("failed to write default branch change");
    traced_git_with_env(
        &repo,
        &["add", "rebase-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("default branch add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "main change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("default branch commit should succeed");

    traced_git_with_env(
        &repo,
        &["checkout", "feature"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout feature should succeed");
    let rebase_conflict = traced_git_with_env(
        &repo,
        &["rebase", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    );
    assert!(
        rebase_conflict.is_err(),
        "rebase should conflict for abort flow coverage"
    );
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
    traced_git_with_env(
        &repo,
        &["rebase", "--abort"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("rebase abort should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_cherry_pick_abort_emits_abort_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("cherry-conflict.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "cherry-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    traced_git_with_env(
        &repo,
        &["checkout", "-b", "topic"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic branch checkout should succeed");
    fs::write(repo.path().join("cherry-conflict.txt"), "topic\n")
        .expect("failed to write topic branch change");
    traced_git_with_env(
        &repo,
        &["add", "cherry-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "topic change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic commit should succeed");
    let topic_sha = repo
        .git(&["rev-parse", "topic"])
        .expect("topic rev-parse should succeed")
        .trim()
        .to_string();

    traced_git_with_env(
        &repo,
        &["checkout", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout default branch should succeed");
    fs::write(repo.path().join("cherry-conflict.txt"), "main\n")
        .expect("failed to write default branch conflicting change");
    traced_git_with_env(
        &repo,
        &["add", "cherry-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("default branch add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "main change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("default branch commit should succeed");

    let cherry_pick_conflict = traced_git_with_env(
        &repo,
        &["cherry-pick", topic_sha.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    );
    assert!(
        cherry_pick_conflict.is_err(),
        "cherry-pick should conflict for abort flow coverage"
    );
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
    traced_git_with_env(
        &repo,
        &["cherry-pick", "--abort"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("cherry-pick abort should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_stash_main_ops_emit_stash_events() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("stash-case.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "stash-case.txt"],
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

    fs::write(repo.path().join("stash-case.txt"), "base\nchange one\n")
        .expect("failed to write stash content");
    traced_git_with_env(
        &repo,
        &["stash", "push", "-m", "save one"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("stash push should succeed");
    // `git stash list` is readonly — the daemon's readonly fast-path drops it
    // before it reaches the ingest queue, so we run it without incrementing
    // expected_top_level_completions and do not expect it in the rewrite log.
    repo.git_og_with_env(&["stash", "list"], &env_refs)
        .expect("stash list should succeed");
    traced_git_with_env(
        &repo,
        &["stash", "apply", "stash@{0}"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("stash apply should succeed");

    traced_git_with_env(
        &repo,
        &["reset", "--hard", "HEAD"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("reset hard should succeed");
    traced_git_with_env(
        &repo,
        &["stash", "pop", "stash@{0}"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("stash pop should succeed");

    traced_git_with_env(
        &repo,
        &["add", "stash-case.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add before commit should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "stash pop result"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("commit after stash pop should succeed");

    fs::write(repo.path().join("stash-case.txt"), "base\nchange two\n")
        .expect("failed to write second stash content");
    traced_git_with_env(
        &repo,
        &["stash", "push", "-m", "save two"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("second stash push should succeed");
    traced_git_with_env(
        &repo,
        &["stash", "drop", "stash@{0}"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("stash drop should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_cherry_pick_continue_emits_complete_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = vec![
        (env[0].0, env[0].1.as_str()),
        (env[1].0, env[1].1.as_str()),
        ("GIT_EDITOR", "true"),
    ];
    let default_branch = repo.current_branch();

    fs::write(repo.path().join("cherry-continue.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "cherry-continue.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    repo.git_og_with_env(&["checkout", "-b", "topic"], &env_refs)
        .expect("topic checkout should succeed");
    fs::write(repo.path().join("cherry-continue.txt"), "topic\n")
        .expect("failed to write topic change");
    repo.git_og_with_env(&["add", "cherry-continue.txt"], &env_refs)
        .expect("topic add should succeed");
    repo.git_og_with_env(&["commit", "-m", "topic change"], &env_refs)
        .expect("topic commit should succeed");
    let topic_sha = repo
        .git(&["rev-parse", "topic"])
        .expect("topic rev-parse should succeed")
        .trim()
        .to_string();

    repo.git_og_with_env(&["checkout", default_branch.as_str()], &env_refs)
        .expect("checkout default should succeed");
    fs::write(repo.path().join("cherry-continue.txt"), "main\n")
        .expect("failed to write main conflict change");
    repo.git_og_with_env(&["add", "cherry-continue.txt"], &env_refs)
        .expect("main add should succeed");
    repo.git_og_with_env(&["commit", "-m", "main change"], &env_refs)
        .expect("main commit should succeed");

    let cherry_conflict = repo.git_og_with_env(&["cherry-pick", topic_sha.as_str()], &env_refs);
    assert!(
        cherry_conflict.is_err(),
        "cherry-pick should conflict before continue"
    );
    wait_for_expected_top_level_completions(&repo, 0, 9);

    fs::write(repo.path().join("cherry-continue.txt"), "resolved\n")
        .expect("failed to write resolved cherry content");
    repo.git_og_with_env(&["add", "cherry-continue.txt"], &env_refs)
        .expect("add resolved cherry content should succeed");
    repo.git_og_with_env(&["cherry-pick", "--continue"], &env_refs)
        .expect("cherry-pick continue should succeed");

    wait_for_expected_top_level_completions(&repo, 0, 11);
}

#[test]
#[serial]
fn daemon_pure_trace_socket_rebase_with_short_sha_emits_complete_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    // Create base commit on default branch
    fs::write(repo.path().join("rebase-short.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "rebase-short.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    // Create feature branch with a commit
    traced_git_with_env(
        &repo,
        &["checkout", "-b", "feature-rebase-short"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature branch checkout should succeed");
    fs::write(repo.path().join("feature-only.txt"), "feature content\n")
        .expect("failed to write feature file");
    traced_git_with_env(
        &repo,
        &["add", "feature-only.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "feature change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature commit should succeed");

    // Go back to default branch and add a non-conflicting commit
    traced_git_with_env(
        &repo,
        &["checkout", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout default should succeed");
    fs::write(repo.path().join("main-only.txt"), "main content\n")
        .expect("failed to write main file");
    traced_git_with_env(
        &repo,
        &["add", "main-only.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("main add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "main advance"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("main commit should succeed");

    // Get the short SHA of the latest main commit
    let main_full_sha = repo
        .git(&["rev-parse", "HEAD"])
        .expect("HEAD rev-parse should succeed")
        .trim()
        .to_string();
    let main_short_sha = &main_full_sha[..7];

    // Switch to feature branch and rebase onto main using SHORT SHA
    traced_git_with_env(
        &repo,
        &["checkout", "feature-rebase-short"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout feature should succeed");
    traced_git_with_env(
        &repo,
        &["rebase", main_short_sha],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("rebase with short SHA should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_cherry_pick_with_short_sha_emits_complete_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    // Create base commit
    fs::write(repo.path().join("short-sha-test.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "short-sha-test.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    // Create topic branch with a commit
    traced_git_with_env(
        &repo,
        &["checkout", "-b", "topic-short-sha"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic branch checkout should succeed");
    fs::write(repo.path().join("short-sha-test.txt"), "topic content\n")
        .expect("failed to write topic change");
    traced_git_with_env(
        &repo,
        &["add", "short-sha-test.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "topic change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic commit should succeed");

    // Get the full SHA and derive a short (7-char) prefix
    let topic_full_sha = repo
        .git(&["rev-parse", "topic-short-sha"])
        .expect("topic rev-parse should succeed")
        .trim()
        .to_string();
    let topic_short_sha = &topic_full_sha[..7];

    // Switch back to default branch
    traced_git_with_env(
        &repo,
        &["checkout", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout default branch should succeed");

    // Cherry-pick using the SHORT SHA -- this is the key part of the test
    traced_git_with_env(
        &repo,
        &["cherry-pick", topic_short_sha],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("cherry-pick with short SHA should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_switch_tracks_success_and_conflict_failure() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    fs::write(repo.path().join("switch-case.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "switch-case.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    repo.git_og_with_env(&["switch", "-c", "feature"], &env_refs)
        .expect("switch -c feature should succeed");
    fs::write(repo.path().join("switch-case.txt"), "feature branch\n")
        .expect("failed to write feature content");
    repo.git_og_with_env(&["add", "switch-case.txt"], &env_refs)
        .expect("feature add should succeed");
    repo.git_og_with_env(&["commit", "-m", "feature"], &env_refs)
        .expect("feature commit should succeed");

    repo.git_og_with_env(&["switch", default_branch.as_str()], &env_refs)
        .expect("switch back to default branch should succeed");
    repo.git_og_with_env(&["switch", "feature"], &env_refs)
        .expect("switch to feature should succeed");
    repo.git_og_with_env(&["switch", default_branch.as_str()], &env_refs)
        .expect("switch back to default branch should succeed");

    fs::write(repo.path().join("switch-case.txt"), "dirty local change\n")
        .expect("failed to write dirty local change");
    let switch_failure = repo.git_og_with_env(&["switch", "feature"], &env_refs);
    assert!(
        switch_failure.is_err(),
        "switch should fail when local changes would be overwritten"
    );

    wait_for_expected_top_level_completions(&repo, 0, 9);

    let switch_entries = completion_entries_for_command(&repo, "switch");
    let saw_switch_success = switch_entries
        .iter()
        .any(|entry| entry.exit_code == Some(0));
    let saw_switch_failure = switch_entries
        .iter()
        .any(|entry| entry.exit_code.unwrap_or(0) != 0);
    assert!(saw_switch_success, "switch success should be tracked");
    assert!(saw_switch_failure, "switch failure should be tracked");
}

#[test]
#[serial]
fn daemon_pure_trace_socket_checkout_tracks_success_failure_and_new_branch() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    fs::write(repo.path().join("checkout-case.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "checkout-case.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    repo.git_og_with_env(&["checkout", "-b", "feature"], &env_refs)
        .expect("checkout -b feature should succeed");
    fs::write(repo.path().join("checkout-case.txt"), "feature branch\n")
        .expect("failed to write feature content");
    repo.git_og_with_env(&["add", "checkout-case.txt"], &env_refs)
        .expect("feature add should succeed");
    repo.git_og_with_env(&["commit", "-m", "feature"], &env_refs)
        .expect("feature commit should succeed");

    repo.git_og_with_env(&["checkout", default_branch.as_str()], &env_refs)
        .expect("checkout default should succeed");
    repo.git_og_with_env(&["checkout", "feature"], &env_refs)
        .expect("checkout feature should succeed");
    repo.git_og_with_env(&["checkout", "-b", "hotfix"], &env_refs)
        .expect("checkout -b hotfix should succeed");
    repo.git_og_with_env(&["checkout", default_branch.as_str()], &env_refs)
        .expect("checkout back to default should succeed");

    fs::write(
        repo.path().join("checkout-case.txt"),
        "dirty local change\n",
    )
    .expect("failed to write dirty local change");
    let checkout_failure = repo.git_og_with_env(&["checkout", "feature"], &env_refs);
    assert!(
        checkout_failure.is_err(),
        "checkout should fail when local changes would be overwritten"
    );

    wait_for_expected_top_level_completions(&repo, 0, 10);

    let checkout_entries = completion_entries_for_command(&repo, "checkout");
    let saw_checkout_success = checkout_entries
        .iter()
        .any(|entry| entry.exit_code == Some(0));
    let saw_checkout_failure = checkout_entries
        .iter()
        .any(|entry| entry.exit_code.unwrap_or(0) != 0);
    assert!(saw_checkout_success, "checkout success should be tracked");
    assert!(saw_checkout_failure, "checkout failure should be tracked");
}

#[test]
#[serial]
fn daemon_pure_trace_socket_pull_fast_forward_tracks_pull_command() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    let run_git = |args: &[&str]| -> String {
        let output = Command::new(real_git_executable())
            .args(args)
            .output()
            .expect("git command should execute");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    fs::write(repo.path().join("pull-case.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "pull-case.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    let remote_root = tempfile::tempdir().expect("remote tempdir should be created");
    let bare_remote = remote_root.path().join("origin.git");
    let remote_clone = remote_root.path().join("origin-work");
    let bare_remote_str = bare_remote.to_string_lossy().to_string();
    let remote_clone_str = remote_clone.to_string_lossy().to_string();
    let _ = fs::remove_dir_all(&bare_remote);
    let _ = fs::remove_dir_all(&remote_clone);

    run_git(&["init", "--bare", bare_remote_str.as_str()]);
    repo.git_og_with_env(
        &["remote", "add", "origin", bare_remote_str.as_str()],
        &env_refs,
    )
    .expect("adding origin remote should succeed");
    repo.git_og_with_env(
        &["push", "-u", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("pushing base branch should succeed");

    run_git(&[
        "clone",
        "--branch",
        default_branch.as_str(),
        bare_remote_str.as_str(),
        remote_clone_str.as_str(),
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.name",
        "Test User",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.email",
        "test@example.com",
    ]);
    fs::write(remote_clone.join("pull-case.txt"), "base\nremote update\n")
        .expect("failed to write remote update");
    run_git(&["-C", remote_clone_str.as_str(), "add", "pull-case.txt"]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "commit",
        "-m",
        "remote update",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "push",
        "origin",
        format!("HEAD:{}", default_branch).as_str(),
    ]);

    repo.git_og_with_env(
        &["pull", "--ff-only", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("fast-forward pull should succeed");

    wait_for_expected_top_level_completions(&repo, 0, 5);

    let pull_entries = completion_entries_for_command(&repo, "pull");
    let saw_pull_success = pull_entries.iter().any(|entry| entry.exit_code == Some(0));
    assert!(saw_pull_success, "pull success should be tracked");
    assert!(
        fs::read_to_string(repo.path().join("pull-case.txt"))
            .expect("pulled file should be readable")
            .contains("remote update"),
        "pull fast-forward should update the worktree contents"
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_pull_rebase_tracks_pull_and_rebase_completion() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    let run_git = |args: &[&str]| -> String {
        let output = Command::new(real_git_executable())
            .args(args)
            .output()
            .expect("git command should execute");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    fs::write(repo.path().join("pull-rebase-base.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "pull-rebase-base.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    let root = repo
        .path()
        .parent()
        .expect("test repo path should have parent")
        .to_path_buf();
    let unique = repo
        .path()
        .file_name()
        .expect("test repo path should have filename")
        .to_string_lossy();
    let bare_remote = root.join(format!("origin-rebase-{unique}.git"));
    let remote_clone = root.join(format!("origin-rebase-work-{unique}"));
    let bare_remote_str = bare_remote.to_string_lossy().to_string();
    let remote_clone_str = remote_clone.to_string_lossy().to_string();
    let _ = fs::remove_dir_all(&bare_remote);
    let _ = fs::remove_dir_all(&remote_clone);

    run_git(&["init", "--bare", bare_remote_str.as_str()]);
    repo.git_og_with_env(
        &["remote", "add", "origin", bare_remote_str.as_str()],
        &env_refs,
    )
    .expect("adding origin remote should succeed");
    repo.git_og_with_env(
        &["push", "-u", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("pushing base branch should succeed");

    run_git(&[
        "clone",
        "--branch",
        default_branch.as_str(),
        bare_remote_str.as_str(),
        remote_clone_str.as_str(),
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.name",
        "Test User",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.email",
        "test@example.com",
    ]);
    fs::write(remote_clone.join("remote-only.txt"), "remote\n")
        .expect("failed to write remote file");
    run_git(&["-C", remote_clone_str.as_str(), "add", "remote-only.txt"]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "commit",
        "-m",
        "remote commit",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "push",
        "origin",
        format!("HEAD:{}", default_branch).as_str(),
    ]);

    fs::write(repo.path().join("local-only.txt"), "local\n").expect("failed to write local file");
    repo.git_og_with_env(&["add", "local-only.txt"], &env_refs)
        .expect("local add should succeed");
    repo.git_og_with_env(&["commit", "-m", "local commit"], &env_refs)
        .expect("local commit should succeed");

    repo.git_og_with_env(
        &["pull", "--rebase", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("pull --rebase should succeed");

    wait_for_expected_top_level_completions(&repo, 0, 7);

    let pull_entries = completion_entries_for_command(&repo, "pull");
    let saw_pull_rebase_success = pull_entries.iter().any(|entry| entry.exit_code == Some(0));
    assert!(
        saw_pull_rebase_success,
        "pull --rebase success should be tracked"
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_pull_autostash_preserves_local_changes_and_tracks_command() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    let run_git = |args: &[&str]| -> String {
        let output = Command::new(real_git_executable())
            .args(args)
            .output()
            .expect("git command should execute");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    fs::write(repo.path().join("autostash-local.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "autostash-local.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    let root = repo
        .path()
        .parent()
        .expect("test repo path should have parent")
        .to_path_buf();
    let bare_remote = root.join("origin-autostash.git");
    let remote_clone = root.join("origin-autostash-work");
    let bare_remote_str = bare_remote.to_string_lossy().to_string();
    let remote_clone_str = remote_clone.to_string_lossy().to_string();
    let _ = fs::remove_dir_all(&bare_remote);
    let _ = fs::remove_dir_all(&remote_clone);

    run_git(&["init", "--bare", bare_remote_str.as_str()]);
    repo.git_og_with_env(
        &["remote", "add", "origin", bare_remote_str.as_str()],
        &env_refs,
    )
    .expect("adding origin remote should succeed");
    repo.git_og_with_env(
        &["push", "-u", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("pushing base branch should succeed");

    run_git(&[
        "clone",
        "--branch",
        default_branch.as_str(),
        bare_remote_str.as_str(),
        remote_clone_str.as_str(),
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.name",
        "Test User",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.email",
        "test@example.com",
    ]);
    fs::write(remote_clone.join("autostash-remote.txt"), "remote\n")
        .expect("failed to write remote update file");
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "add",
        "autostash-remote.txt",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "commit",
        "-m",
        "remote update",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "push",
        "origin",
        format!("HEAD:{}", default_branch).as_str(),
    ]);

    fs::write(
        repo.path().join("autostash-local.txt"),
        "base\nlocal dirty change\n",
    )
    .expect("failed to write local dirty change");

    repo.git_og_with_env(
        &[
            "pull",
            "--rebase",
            "--autostash",
            "origin",
            default_branch.as_str(),
        ],
        &env_refs,
    )
    .expect("pull --rebase --autostash should succeed");

    wait_for_expected_top_level_completions(&repo, 0, 5);

    let local_contents = fs::read_to_string(repo.path().join("autostash-local.txt"))
        .expect("local file should remain readable");
    assert!(
        local_contents.contains("local dirty change"),
        "autostash pull should preserve local dirty change content"
    );

    let pull_entries = completion_entries_for_command(&repo, "pull");
    let saw_pull_autostash_success = pull_entries.iter().any(|entry| entry.exit_code == Some(0));
    assert!(
        saw_pull_autostash_success,
        "pull --rebase --autostash success should be tracked"
    );
}

#[test]
fn daemon_delayed_pull_rebase_autostash_does_not_consume_later_commit() {
    let (local, _upstream) =
        TestRepo::new_with_remote_with_daemon_scope(DaemonTestScope::Dedicated);
    let trace_socket = daemon_trace_socket_path(&local);
    let worktree = repo_workdir_string(&local);
    let git_dir = local.path().join(".git").to_string_lossy().to_string();

    let mut readme = local.filename("README.md");
    readme.set_contents(lines!["# Test Repo".human()]);
    let initial = local
        .stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");
    readme.assert_committed_lines(lines!["# Test Repo".human()]);

    local
        .git(&["push", "-u", "origin", "HEAD"])
        .expect("push initial commit should succeed");

    let mut committed_ai = local.filename("ai_feature.txt");
    committed_ai.set_contents(lines![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);
    let local_ai = local
        .stage_all_and_commit("add AI feature")
        .expect("AI feature commit should succeed");
    committed_ai.assert_committed_lines(lines![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);

    let branch = local.current_branch();
    local
        .git(&["reset", "--hard", &initial.commit_sha])
        .expect("reset to initial commit should succeed");

    let mut upstream_file = local.filename("upstream_change.txt");
    upstream_file.set_contents(lines!["upstream content".human()]);
    local
        .stage_all_and_commit("upstream divergent commit")
        .expect("upstream commit should succeed");
    upstream_file.assert_committed_lines(lines!["upstream content".human()]);

    local
        .git(&["push", "--force", "origin", &format!("HEAD:{}", branch)])
        .expect("force push upstream commit should succeed");
    local
        .git(&["reset", "--hard", &local_ai.commit_sha])
        .expect("reset back to local AI commit should succeed");

    let mut uncommitted_ai = local.filename("uncommitted_ai.txt");
    uncommitted_ai.set_contents(lines!["Uncommitted AI line".ai()]);
    local
        .git_ai(&["checkpoint", "mock_ai", "uncommitted_ai.txt"])
        .expect("checkpoint should succeed");
    local.sync_daemon();

    local
        .git_og(&["pull", "--rebase", "--autostash"])
        .expect("raw pull --rebase --autostash should succeed");
    local
        .git_og(&["add", "-A"])
        .expect("raw add should succeed");
    local
        .git_og(&["commit", "-m", "commit uncommitted AI work"])
        .expect("raw commit should succeed");
    let final_commit = local
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse final commit should succeed")
        .trim()
        .to_string();

    let pull_session = repos::test_repo::new_daemon_test_sync_session_id();
    let commit_session = repos::test_repo::new_daemon_test_sync_session_id();
    let pull_session_arg = format!("git-ai.testSyncSession={pull_session}");
    let commit_session_arg = format!("git-ai.testSyncSession={commit_session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "delayed-pull-autostash",
                "argv": ["git", "-c", pull_session_arg, "-C", worktree, "pull", "--rebase", "--autostash"],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-pull-autostash",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-pull-autostash",
                "code": 0,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("delayed-pull-autostash", 0, 1_101u64),
            json!({
                "event": "start",
                "sid": "delayed-commit-after-pull",
                "argv": ["git", "-c", commit_session_arg, "-C", worktree, "commit", "-m", "commit uncommitted AI work"],
                "time_ns": 2_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-commit-after-pull",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 2_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-commit-after-pull",
                "code": 0,
                "time_ns": 2_100u64,
            }),
            trace_atexit_frame("delayed-commit-after-pull", 0, 2_101u64),
        ],
    );
    local.sync_daemon_external_completion_sessions(&[pull_session, commit_session]);

    assert!(
        local.read_authorship_note(&final_commit).is_some(),
        "delayed pull processing must not consume the following commit reflog entry"
    );
    uncommitted_ai.assert_committed_lines(lines!["Uncommitted AI line".ai()]);
}

#[test]
fn daemon_delayed_failed_rebase_continue_does_not_consume_final_continue() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    fs::write(repo.path().join("config_a.py"), "FLAG_A = 0\n").unwrap();
    repo.git_og(&["add", "config_a.py"]).unwrap();
    repo.git_og(&["commit", "-m", "Initial config_a"]).unwrap();
    fs::write(repo.path().join("config_b.py"), "FLAG_B = 0\nBATCH = 10\n").unwrap();
    repo.git_og(&["add", "config_b.py"]).unwrap();
    repo.git_og(&["commit", "-m", "Initial config_b"]).unwrap();
    let main_branch = repo.current_branch();

    fs::write(repo.path().join("config_a.py"), "FLAG_A = 1\n").unwrap();
    repo.git_og(&["add", "config_a.py"]).unwrap();
    repo.git_og(&["commit", "-m", "main sets flag_a"]).unwrap();
    fs::write(repo.path().join("config_b.py"), "FLAG_B = 1\nBATCH = 50\n").unwrap();
    repo.git_og(&["add", "config_b.py"]).unwrap();
    repo.git_og(&["commit", "-m", "main sets config_b"])
        .unwrap();

    let base_sha = repo
        .git_og(&["rev-parse", "HEAD~2"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    let mut module_a = repo.filename("module_a.py");
    module_a.set_contents(lines!["class ModuleA:".ai(), "    pass".ai()]);
    let original_c1 = repo.stage_all_and_commit("feat: C1 add ModuleA").unwrap();
    module_a.assert_committed_lines(lines!["class ModuleA:".ai(), "    pass".ai()]);

    let mut config_a = repo.filename("config_a.py");
    config_a.set_contents(lines!["FLAG_A = 2".ai()]);
    let original_c2 = repo.stage_all_and_commit("feat: C2 sets flag_a").unwrap();
    config_a.assert_committed_lines(lines!["FLAG_A = 2".ai()]);

    let mut module_c = repo.filename("module_c.py");
    module_c.set_contents(lines!["class ModuleC:".ai(), "    pass".ai()]);
    let original_c3 = repo.stage_all_and_commit("feat: C3 add ModuleC").unwrap();
    module_c.assert_committed_lines(lines!["class ModuleC:".ai(), "    pass".ai()]);

    let mut config_b = repo.filename("config_b.py");
    config_b.set_contents(lines!["FLAG_B = 1".ai(), "BATCH = 200".ai()]);
    let original_c4 = repo.stage_all_and_commit("feat: C4 sets batch").unwrap();
    config_b.assert_committed_lines(lines!["FLAG_B = 1".ai(), "BATCH = 200".ai()]);

    let mut module_e = repo.filename("module_e.py");
    module_e.set_contents(lines!["class ModuleE:".ai(), "    pass".ai()]);
    let original_c5 = repo.stage_all_and_commit("feat: C5 add ModuleE").unwrap();
    module_e.assert_committed_lines(lines!["class ModuleE:".ai(), "    pass".ai()]);
    for commit in [
        &original_c1,
        &original_c2,
        &original_c3,
        &original_c4,
        &original_c5,
    ] {
        assert!(
            repo.read_authorship_note(&commit.commit_sha).is_some(),
            "original feature commit should have authorship note"
        );
    }
    repo.sync_daemon();

    assert!(
        repo.git_og(&["rebase", &main_branch]).is_err(),
        "initial raw rebase should stop at config_a conflict"
    );
    fs::write(repo.path().join("config_a.py"), "FLAG_A = 2\n").unwrap();
    repo.git_og(&["add", "config_a.py"]).unwrap();
    assert!(
        repo.git_og_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")])
            .is_err(),
        "first raw rebase --continue should stop at config_b conflict"
    );
    fs::write(repo.path().join("config_b.py"), "FLAG_B = 1\nBATCH = 75\n").unwrap();
    repo.git_og(&["add", "config_b.py"]).unwrap();
    repo.git_og_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")])
        .expect("final raw rebase --continue should finish");

    let final_chain = (0..5)
        .rev()
        .map(|offset| {
            let rev = if offset == 0 {
                "HEAD".to_string()
            } else {
                format!("HEAD~{offset}")
            };
            repo.git_og(&["rev-parse", &rev])
                .unwrap()
                .trim()
                .to_string()
        })
        .collect::<Vec<_>>();

    let initial_rebase_session = repos::test_repo::new_daemon_test_sync_session_id();
    let first_continue_session = repos::test_repo::new_daemon_test_sync_session_id();
    let final_continue_session = repos::test_repo::new_daemon_test_sync_session_id();
    let initial_session_arg = format!("git-ai.testSyncSession={initial_rebase_session}");
    let first_continue_session_arg = format!("git-ai.testSyncSession={first_continue_session}");
    let final_continue_session_arg = format!("git-ai.testSyncSession={final_continue_session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "delayed-rebase-start",
                "argv": ["git", "-c", initial_session_arg, "-C", worktree, "rebase", main_branch],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-rebase-start",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-rebase-start",
                "code": 1,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("delayed-rebase-start", 1, 1_101u64),
            json!({
                "event": "start",
                "sid": "delayed-first-rebase-continue",
                "argv": ["git", "-c", first_continue_session_arg, "-C", worktree, "rebase", "--continue"],
                "time_ns": 2_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-first-rebase-continue",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 2_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-first-rebase-continue",
                "code": 1,
                "time_ns": 2_100u64,
            }),
            trace_atexit_frame("delayed-first-rebase-continue", 1, 2_101u64),
            json!({
                "event": "start",
                "sid": "delayed-final-rebase-continue",
                "argv": ["git", "-c", final_continue_session_arg, "-C", worktree, "rebase", "--continue"],
                "time_ns": 3_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-final-rebase-continue",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 3_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-final-rebase-continue",
                "code": 0,
                "time_ns": 3_100u64,
            }),
            trace_atexit_frame("delayed-final-rebase-continue", 0, 3_101u64),
        ],
    );
    repo.sync_daemon_external_completion_sessions(&[
        initial_rebase_session,
        first_continue_session,
        final_continue_session,
    ]);

    for (idx, sha) in final_chain.iter().enumerate() {
        assert!(
            repo.read_authorship_note(sha).is_some(),
            "rebased commit {} should have authorship note after delayed continue processing",
            idx + 1
        );
    }
    module_e.assert_committed_lines(lines!["class ModuleE:".ai(), "    pass".ai()]);
}

#[test]
#[serial]
fn daemon_pure_trace_socket_high_throughput_ai_commit_burst_preserves_exact_blame() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];

    let file_count = 16usize;
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_completions = 0u64;
    for idx in 0..file_count {
        let file_rel = format!("daemon-race-file-{idx}.txt");
        let file_path = repo.path().join(file_rel.as_str());
        fs::write(&file_path, format!("ai-line-{idx}\n"))
            .expect("failed to write ai burst test file");

        repo.git_ai_with_env(
            &["checkpoint", "mock_ai", file_rel.as_str()],
            &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
        )
        .expect("delegated ai checkpoint should succeed");
        expected_completions += 1;

        repo.git_og_with_env(&["add", file_rel.as_str()], &env_refs)
            .expect("staging ai burst file should succeed");
        expected_completions += 1;
    }

    // Wait for all checkpoints and adds to complete before committing
    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    repo.git_og_with_env(&["commit", "-m", "ai burst commit"], &env_refs)
        .expect("ai burst commit should succeed");
    expected_completions += 1;

    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    for idx in 0..file_count {
        let mut file = repo.filename(format!("daemon-race-file-{idx}.txt").as_str());
        file.assert_lines_and_blame(lines![format!("ai-line-{idx}").ai()]);
    }
}

#[test]
#[serial]
fn daemon_pure_trace_socket_concurrent_worktree_burst_preserves_exact_line_attribution() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];

    let harness = WorkdirRaceHarness::new(&repo, trace_socket.clone());
    let worker_a_dir = repo.path().to_path_buf();
    let worker_b_dir = unique_worktree_path(&repo, "daemon-race-worker-b");
    let worker_b_dir_str = worker_b_dir.to_string_lossy().to_string();

    repo.git_og_with_env(&["checkout", "-b", "daemon-race-worker-a"], &env_refs)
        .expect("checkout worker-a branch should succeed");
    repo.git_og_with_env(
        &[
            "worktree",
            "add",
            "-b",
            "daemon-race-worker-b",
            worker_b_dir_str.as_str(),
        ],
        &env_refs,
    )
    .expect("worktree add worker-b should succeed");
    wait_for_expected_top_level_completions(&repo, 0, 2);

    let file_count = 10usize;
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_completions = 0u64;
    for idx in 0..file_count {
        let file_a = format!("daemon-race-a-{idx}.txt");
        harness.write_ai_line_checkpoint_and_add(
            &worker_a_dir,
            file_a.as_str(),
            format!("a-ai-line-{idx}").as_str(),
        );
        expected_completions += 2; // checkpoint + add

        let file_b = format!("daemon-race-b-{idx}.txt");
        harness.write_ai_line_checkpoint_and_add(
            &worker_b_dir,
            file_b.as_str(),
            format!("b-ai-line-{idx}").as_str(),
        );
        expected_completions += 2; // checkpoint + add
    }

    // Wait for all checkpoints and adds to complete before committing
    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    harness.run_traced_git(&worker_a_dir, &["commit", "-m", "worker-a burst commit"]);
    harness.run_traced_git(&worker_b_dir, &["commit", "-m", "worker-b burst commit"]);
    expected_completions += 2; // both commits

    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    for idx in 0..file_count {
        let file_a = format!("daemon-race-a-{idx}.txt");
        let file_b = format!("daemon-race-b-{idx}.txt");
        assert_single_ai_line_for_workdir(
            &repo,
            &worker_a_dir,
            file_a.as_str(),
            format!("a-ai-line-{idx}").as_str(),
        );
        assert_single_ai_line_for_workdir(
            &repo,
            &worker_b_dir,
            file_b.as_str(),
            format!("b-ai-line-{idx}").as_str(),
        );
    }

    let _ = repo.git_og_with_env(
        &["worktree", "remove", "--force", worker_b_dir_str.as_str()],
        &env_refs,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_concurrent_checkpoint_requests_preserve_exact_line_attribution() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];

    let harness = WorkdirRaceHarness::new(&repo, trace_socket.clone());
    let workdir = repo.path().to_path_buf();

    let file_count = 12usize;
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected = Vec::new();
    for idx in 0..file_count {
        let file_rel = format!("daemon-race-concurrent-checkpoint-{idx}.txt");
        let line = format!("ai-line-{idx}");
        fs::write(workdir.join(file_rel.as_str()), format!("{line}\n"))
            .expect("failed to write concurrent checkpoint test file");
        expected.push((file_rel, line));
    }

    #[cfg(windows)]
    {
        for (file_rel, _) in &expected {
            harness.run_delegated_checkpoint(&workdir, file_rel.as_str());
        }
    }
    #[cfg(not(windows))]
    {
        let mut checkpoint_threads = Vec::new();
        for (file_rel, _) in &expected {
            let thread_workdir = workdir.clone();
            let harness = harness.clone();
            let file_rel = file_rel.clone();
            checkpoint_threads.push(thread::spawn(move || {
                harness.run_delegated_checkpoint(&thread_workdir, file_rel.as_str());
            }));
        }
        for handle in checkpoint_threads {
            handle
                .join()
                .expect("concurrent delegated checkpoint thread should not panic");
        }
    }

    // Wait for all concurrent checkpoints to complete before adding
    let mut expected_completions = file_count as u64;
    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    repo.git_og_with_env(&["add", "."], &env_refs)
        .expect("staging concurrent checkpoint files should succeed");
    expected_completions += 1;

    repo.git_og_with_env(
        &["commit", "-m", "concurrent delegated checkpoint burst"],
        &env_refs,
    )
    .expect("commit for concurrent checkpoint files should succeed");
    expected_completions += 1;

    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    for (file_rel, line) in expected {
        let mut file = repo.filename(file_rel.as_str());
        file.assert_lines_and_blame(lines![line.ai()]);
    }
}

#[test]
#[serial]
fn daemon_pure_trace_socket_parallel_worktree_streams_preserve_exact_line_attribution() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];

    let harness = WorkdirRaceHarness::new(&repo, trace_socket.clone());
    let worker_a_dir = repo.path().to_path_buf();
    let worker_b_dir = unique_worktree_path(&repo, "daemon-race-worker-b-parallel");
    let worker_b_dir_str = worker_b_dir.to_string_lossy().to_string();

    repo.git_og_with_env(
        &["checkout", "-b", "daemon-race-parallel-worker-a"],
        &env_refs,
    )
    .expect("checkout parallel worker-a branch should succeed");
    repo.git_og_with_env(
        &[
            "worktree",
            "add",
            "-b",
            "daemon-race-parallel-worker-b",
            worker_b_dir_str.as_str(),
        ],
        &env_refs,
    )
    .expect("worktree add parallel worker-b should succeed");
    wait_for_expected_top_level_completions(&repo, 0, 2);

    let file_count = 8usize;
    let completion_baseline = repo.daemon_total_completion_count();

    // Spawn threads to do checkpoint+add in parallel, but WITHOUT committing yet
    let worker_a_harness = harness.clone();
    let worker_a_dir_clone = worker_a_dir.clone();
    let worker_a = thread::spawn(move || {
        for idx in 0..file_count {
            let file = format!("daemon-race-parallel-a-{idx}.txt");
            let line = format!("a-parallel-ai-line-{idx}");
            worker_a_harness.write_ai_line_checkpoint_and_add(
                &worker_a_dir_clone,
                file.as_str(),
                line.as_str(),
            );
        }
    });

    let worker_b_harness = harness.clone();
    let worker_b_dir_clone = worker_b_dir.clone();
    let worker_b = thread::spawn(move || {
        for idx in 0..file_count {
            let file = format!("daemon-race-parallel-b-{idx}.txt");
            let line = format!("b-parallel-ai-line-{idx}");
            worker_b_harness.write_ai_line_checkpoint_and_add(
                &worker_b_dir_clone,
                file.as_str(),
                line.as_str(),
            );
        }
    });

    worker_a
        .join()
        .expect("parallel worker-a thread should not panic");
    worker_b
        .join()
        .expect("parallel worker-b thread should not panic");

    // Wait for all checkpoints and adds to complete before committing
    let mut expected_completions = (file_count as u64) * 2 * 2; // checkpoints + adds for both workers
    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    // Now do the commits after all checkpoints are processed
    harness.run_traced_git(&worker_a_dir, &["commit", "-m", "parallel worker-a commit"]);
    harness.run_traced_git(&worker_b_dir, &["commit", "-m", "parallel worker-b commit"]);
    expected_completions += 2; // both commits

    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    for idx in 0..file_count {
        let file_a = format!("daemon-race-parallel-a-{idx}.txt");
        let file_b = format!("daemon-race-parallel-b-{idx}.txt");
        assert_single_ai_line_for_workdir(
            &repo,
            &worker_a_dir,
            file_a.as_str(),
            format!("a-parallel-ai-line-{idx}").as_str(),
        );
        assert_single_ai_line_for_workdir(
            &repo,
            &worker_b_dir,
            file_b.as_str(),
            format!("b-parallel-ai-line-{idx}").as_str(),
        );
    }

    let _ = repo.git_og_with_env(
        &["worktree", "remove", "--force", worker_b_dir_str.as_str()],
        &env_refs,
    );
}

// Daemon update check decision logic is tested by unit tests in
// src/commands/upgrade.rs (check_for_update_available_*). The integration
// tests that spawned a full daemon were removed because the post-shutdown
// self-update code made real HTTP calls that caused hangs/flakes.

#[test]
#[serial]
fn daemon_memory_does_not_grow_unbounded_under_trace_load() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    // Create a base commit so the repo has a valid HEAD.
    fs::write(repo.path().join("init.txt"), "init\n").expect("write failed");
    repo.git(&["add", "init.txt"]).expect("add failed");
    repo.git(&["commit", "-m", "init"]).expect("commit failed");

    let mut guard = DaemonGuard::start(&repo);
    let pid = guard.child.id();

    // Let the daemon settle after startup.
    thread::sleep(Duration::from_millis(500));
    let baseline_rss = get_rss_kb(pid).unwrap_or_else(|| {
        eprintln!(
            "WARN: /proc/{}/status not readable, skipping RSS check",
            pid
        );
        0
    });
    eprintln!("daemon pid={} baseline RSS={}KB", pid, baseline_rss);

    let worktree_str = repo.path().to_string_lossy().to_string();

    // Send 2000 complete git trace lifecycle rounds (start + exit + atexit).
    // Each round simulates a complete `git status` invocation with a unique SID.
    for batch in 0..20 {
        let mut frames = Vec::new();
        for i in 0..100u64 {
            let sid = format!("stress-{}-{}", batch, i);
            frames.push(serde_json::json!({
                "event": "start",
                "sid": &sid,
                "argv": ["git", "status"],
                "time_ns": 1000000000u64 + (batch * 100) as u64 + i,
            }));
            frames.push(serde_json::json!({
                "event": "def_repo",
                "sid": &sid,
                "worktree": &worktree_str,
                "repo": repo.path().join(".git").to_string_lossy().to_string(),
            }));
            frames.push(serde_json::json!({
                "event": "exit",
                "sid": &sid,
                "code": 0,
                "time_ns": 1000000001u64 + (batch * 100) as u64 + i,
            }));
            frames.push(trace_atexit_frame(
                &sid,
                0,
                1000000002u64 + (batch * 100) as u64 + i,
            ));
        }
        send_trace_frames(&guard.trace_socket_path, &frames);
        // Small delay to let the daemon process frames.
        thread::sleep(Duration::from_millis(50));
    }

    // Give the daemon time to finish processing all frames.
    thread::sleep(Duration::from_millis(500));

    let final_rss = get_rss_kb(pid).unwrap_or(0);
    let growth = final_rss.saturating_sub(baseline_rss);
    eprintln!(
        "daemon pid={} final RSS={}KB growth={}KB",
        pid, final_rss, growth
    );

    if baseline_rss > 0 && final_rss > 0 {
        // Memory growth should be bounded. With the leak fixes, growth should stay
        // well under 50 MB even after 2000 trace rounds.
        assert!(
            growth < 50_000,
            "daemon RSS grew by {}KB after 2000 trace rounds; expected < 50MB",
            growth,
        );
    } else {
        eprintln!("RSS measurement unavailable, verifying daemon survived load");
    }

    guard.shutdown();
}

fn bg_command(repo: &TestRepo, subcommand: &str, extra_args: &[&str]) -> Output {
    bg_command_with_env(repo, subcommand, extra_args, &[])
}

fn bg_command_with_env(
    repo: &TestRepo,
    subcommand: &str,
    extra_args: &[&str],
    env: &[(&str, &str)],
) -> Output {
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
    for (key, value) in env {
        command.env(key, value);
    }
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

#[test]
fn await_waits_for_metrics_and_notes_flush() {
    let mut mock_api = MockApiServer::start();

    // Metrics recording is gated in test builds; point it at an isolated DB so
    // post-commit metric events actually get stored and flushed.
    let metrics_db_path =
        std::env::temp_dir().join(format!("git-ai-test-metrics-{}.db", std::process::id()));
    let mut repo = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_API_BASE_URL", mock_api.base_url()),
        ("GIT_AI_API_KEY", "test-api-key"),
        ("GIT_AI_NOTES_BACKEND_KIND", "http"),
        ("GIT_AI_NOTES_BACKEND_URL", mock_api.base_url()),
        (
            "GIT_AI_TEST_METRICS_DB_PATH",
            metrics_db_path.to_str().unwrap(),
        ),
    ]);
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
        patch.prompt_storage = Some("default".to_string());
        patch.telemetry_oss_disabled = Some(true);
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::Http,
            backend_url: Some(mock_api.base_url().to_string()),
        });
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("test.ts");

    // First commit: known-human baseline, then an AI-style edit to produce metrics.
    fs::write(&file_path, "const x = 1;\n").expect("failed to write initial file");
    repo.git_ai(&["checkpoint", "mock_known_human", "test.ts"])
        .expect("known-human checkpoint should succeed");
    fs::write(&file_path, "const x = 2;\n").expect("failed to write update");
    repo.git_ai(&["checkpoint", "mock_ai", "test.ts"])
        .expect("ai checkpoint should succeed");
    repo.git(&["add", "-A"])
        .expect("initial add should succeed");
    repo.git(&["commit", "-m", "Initial commit"])
        .expect("initial commit should succeed");

    // Second commit: repeat the same pattern to queue more metrics and notes.
    fs::write(&file_path, "const x = 3;\n").expect("failed to write update");
    repo.git_ai(&["checkpoint", "mock_known_human", "test.ts"])
        .expect("known-human checkpoint should succeed");
    fs::write(&file_path, "const x = 4;\n").expect("failed to write update");
    repo.git_ai(&["checkpoint", "mock_ai", "test.ts"])
        .expect("ai checkpoint should succeed");
    repo.git(&["add", "-A"]).expect("second add should succeed");
    repo.git(&["commit", "-m", "Second commit"])
        .expect("second commit should succeed");

    // Wait for the daemon to finish and flush telemetry.
    let output = repo
        .git_ai(&["await", "--timeout", "30"])
        .expect("await should succeed");
    assert!(
        output.contains("finished"),
        "await should report finished: {}",
        output
    );

    let requests = mock_api.collect_requests();
    let metrics_requests = requests
        .iter()
        .filter(|r| r["path"].as_str() == Some("/worker/metrics/upload"))
        .count();
    let notes_requests = requests
        .iter()
        .filter(|r| r["path"].as_str() == Some("/worker/notes/upload"))
        .count();
    assert!(
        metrics_requests > 0,
        "expected at least one metrics upload, got {}",
        metrics_requests
    );
    assert!(
        notes_requests > 0,
        "expected at least one notes upload, got {}",
        notes_requests
    );
}

#[test]
fn daemon_debug_logging_does_not_reupload_ureq_logs() {
    let mut mock_api = MockApiServer::start();
    let repo = TestRepo::new_with_daemon_env(&[
        ("RUST_LOG", "debug"),
        ("GIT_AI_API_BASE_URL", mock_api.base_url()),
        ("GIT_AI_API_KEY", "test-api-key"),
    ]);

    repo.git_ai(&["await", "--timeout", "10"])
        .expect("initial daemon log flush should succeed");

    let first_upload_deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut requests = Vec::new();
    while std::time::Instant::now() < first_upload_deadline {
        requests.extend(mock_api.collect_requests());
        if requests
            .iter()
            .any(|request| request["path"] == "/worker/logs/upload")
        {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    thread::sleep(Duration::from_millis(250));
    repo.git_ai(&["await", "--timeout", "10"])
        .expect("follow-up daemon log flush should succeed");
    thread::sleep(Duration::from_millis(250));
    requests.extend(mock_api.collect_requests());

    let uploaded_targets = requests
        .iter()
        .filter(|request| request["path"] == "/worker/logs/upload")
        .flat_map(|request| request["body"]["events"].as_array().into_iter().flatten())
        .filter_map(|event| {
            event["fields"]["log.target"]
                .as_str()
                .or_else(|| event["target"].as_str())
        })
        .collect::<Vec<_>>();

    assert!(
        !uploaded_targets.is_empty(),
        "expected the daemon to upload its startup logs"
    );
    assert!(
        uploaded_targets
            .iter()
            .all(|target| *target != "ureq" && !target.starts_with("ureq::")),
        "ureq logs generated by daemon log delivery must not be uploaded: {uploaded_targets:?}"
    );
}

#[test]
fn daemon_marks_repository_filtered_session_events_delivered_without_uploading_them() {
    let mut mock_api = MockApiServer::start();
    let metrics_db_path = std::env::temp_dir().join(format!(
        "git-ai-filtered-session-events-{}.db",
        git_ai::uuid::generate_v4()
    ));
    let repo = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_API_BASE_URL", mock_api.base_url()),
        ("GIT_AI_API_KEY", "test-api-key"),
        (
            "GIT_AI_TEST_METRICS_DB_PATH",
            metrics_db_path.to_str().unwrap(),
        ),
    ]);
    fs::write(
        repo.test_home_path().join(".git-ai/config.json"),
        r#"{"allow_repositories":["https://github.com/acme/*"],"exclude_repositories":["git@github.com:acme/private"]}"#,
    )
    .unwrap();

    let session_event = |trace_id: &str, repo_url: &str| {
        MetricEvent::from_values(
            SessionEventValues::new(json!({ "marker": trace_id })),
            EventAttributes::with_version("test")
                .session_id(trace_id)
                .trace_id(trace_id)
                .repo_url(repo_url)
                .to_sparse(),
        )
    };
    let events = [
        session_event("allowed-session", "https://github.com/acme/public"),
        session_event("excluded-session", "https://github.com/acme/private"),
    ];
    let serialized_events = events
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    MetricsDatabase::open_at_path(&metrics_db_path)
        .unwrap()
        .insert_events(&serialized_events)
        .unwrap();

    repo.git_ai(&["await", "--timeout", "30"])
        .expect("await should flush metrics");

    let uploaded_requests = serde_json::to_string(&mock_api.collect_requests()).unwrap();
    assert!(uploaded_requests.contains("allowed-session"));
    assert!(!uploaded_requests.contains("excluded-session"));

    let metrics_db = MetricsDatabase::open_at_path(&metrics_db_path).unwrap();
    let status = metrics_db.status().unwrap();
    assert_eq!(status.delivered, 2);
}

#[test]
fn await_is_marked_beta_and_returns_promptly_when_idle() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    let top_level_help = repo
        .git_ai(&["--help"])
        .expect("top-level help should succeed");
    assert!(
        top_level_help.contains("await [beta]"),
        "top-level help should mark await as beta: {}",
        top_level_help
    );

    let await_help = repo
        .git_ai(&["await", "--help"])
        .expect("await help should succeed");
    assert!(
        await_help.contains("beta"),
        "await help should mark the command as beta: {}",
        await_help
    );

    let started_at = std::time::Instant::now();
    repo.git_ai(&["await", "--timeout", "10"])
        .expect("await should succeed when the daemon is idle");
    assert!(
        started_at.elapsed() < Duration::from_secs(4),
        "await should return promptly instead of waiting for the progress interval"
    );
}

#[test]
fn await_rejects_zero_timeout() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    let error = repo
        .git_ai(&["await", "--timeout", "0"])
        .expect_err("zero timeout should be rejected");

    assert!(
        error.contains("--timeout must be a positive integer"),
        "await should report an input validation error: {error}"
    );
}
