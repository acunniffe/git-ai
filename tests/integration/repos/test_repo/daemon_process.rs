#[cfg(unix)]
use super::cleanup::{is_process_alive, reap_child_if_exited};
use super::*;

#[derive(Debug, Clone)]
pub(super) struct DaemonProcess {
    pub(super) pid: u32,
    pub(super) daemon_home: PathBuf,
    pub(super) test_db_path: PathBuf,
    pub(super) control_socket_path: PathBuf,
    pub(super) trace_socket_path: PathBuf,
    pub(super) stderr_log_path: PathBuf,
}

#[cfg(windows)]
struct TestDaemonJob {
    handle: HANDLE,
}

#[cfg(windows)]
impl TestDaemonJob {
    pub(super) fn new() -> Self {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        assert!(
            !handle.is_null(),
            "failed to create Windows test daemon job object"
        );

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &mut limits as *mut _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            unsafe {
                CloseHandle(handle);
            }
            panic!("failed to configure Windows test daemon job object");
        }

        Self { handle }
    }

    pub(super) fn assign_pid(&self, pid: u32) {
        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        assert!(
            !process.is_null(),
            "failed to open daemon process {} for job assignment",
            pid
        );

        let ok = unsafe { AssignProcessToJobObject(self.handle, process) };
        unsafe {
            CloseHandle(process);
        }
        assert_ne!(
            ok, 0,
            "failed to assign daemon process {} to Windows test daemon job",
            pid
        );
    }
}

// Windows job handles are kernel object handles. We only share the stable handle
// value and close it once from the OnceLock-owned wrapper at process teardown.
#[cfg(windows)]
unsafe impl Send for TestDaemonJob {}
#[cfg(windows)]
unsafe impl Sync for TestDaemonJob {}

#[cfg(windows)]
impl Drop for TestDaemonJob {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
static TEST_DAEMON_JOB: OnceLock<TestDaemonJob> = OnceLock::new();

#[cfg(windows)]
fn assign_daemon_to_test_job(pid: u32) {
    TEST_DAEMON_JOB
        .get_or_init(TestDaemonJob::new)
        .assign_pid(pid);
}

#[cfg(not(windows))]
fn assign_daemon_to_test_job(_pid: u32) {}

impl DaemonProcess {
    pub(super) fn control_socket_path_for_home(test_home: &Path) -> PathBuf {
        DaemonConfig::from_home(test_home).control_socket_path
    }

    pub(super) fn trace_socket_path_for_home(test_home: &Path) -> PathBuf {
        DaemonConfig::from_home(test_home).trace_socket_path
    }

    pub(super) fn start(repo_path: &Path, test_home: &Path, test_db_path: &Path) -> Self {
        Self::start_with_env(repo_path, test_home, test_db_path, &[])
    }

    pub(super) fn start_with_env(
        repo_path: &Path,
        test_home: &Path,
        test_db_path: &Path,
        extra_env: &[(&str, &str)],
    ) -> Self {
        let control_socket_path = Self::control_socket_path_for_home(test_home);
        let trace_socket_path = Self::trace_socket_path_for_home(test_home);
        let stderr_log_path = test_home
            .join(".git-ai")
            .join("internal")
            .join("daemon")
            .join("daemon.test.stderr.log");
        fs::create_dir_all(
            stderr_log_path
                .parent()
                .expect("daemon stderr path should have parent"),
        )
        .expect("failed to create daemon log dir");
        let stderr_log = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&stderr_log_path)
            .expect("failed to create daemon stderr log");

        // Build the daemon spawn command once; we may run it more than once if
        // the Windows loader fails to start the process image (see below).
        let spawn_daemon = || {
            let mut command = Command::new(get_binary_path());
            command
                .arg("bg")
                .arg("run")
                .current_dir(test_home)
                .env("GIT_AI_TEST_DB_PATH", test_db_path)
                .env("GITAI_TEST_DB_PATH", test_db_path)
                .env("GIT_AI_DAEMON_HOME", test_home)
                .env("GIT_AI_DAEMON_CONTROL_SOCKET", &control_socket_path)
                .env("GIT_AI_DAEMON_TRACE_SOCKET", &trace_socket_path)
                .stdout(Stdio::null())
                .stderr(
                    stderr_log
                        .try_clone()
                        .expect("failed to clone daemon stderr log file"),
                );
            for (key, value) in extra_env {
                command.env(key, value);
            }
            configure_test_home_env(&mut command, test_home);
            command
                .spawn()
                .expect("failed to spawn git-ai subprocess for test mode")
        };

        // Respawn loop: a `STATUS_DLL_INIT_FAILED` exit means the OS loader
        // never started the daemon (a hosted-Windows-runner hiccup), so retry.
        // Any other failure panics immediately.
        let mut attempt = 0;
        loop {
            let mut child = spawn_daemon();
            let pid = child.id();
            assign_daemon_to_test_job(pid);

            let daemon = Self {
                pid,
                daemon_home: test_home.to_path_buf(),
                test_db_path: test_db_path.to_path_buf(),
                control_socket_path: control_socket_path.clone(),
                trace_socket_path: trace_socket_path.clone(),
                stderr_log_path: stderr_log_path.clone(),
            };
            match daemon.wait_until_ready(repo_path, &mut child) {
                Ok(()) => {
                    drop(child);
                    return daemon;
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    attempt += 1;
                    if matches!(error, DaemonReadyError::LoaderInitFailure(_))
                        && attempt < DAEMON_SPAWN_LOADER_RETRY_ATTEMPTS
                    {
                        eprintln!(
                            "[test-harness] daemon loader init failed (attempt {}/{}), respawning: {}",
                            attempt,
                            DAEMON_SPAWN_LOADER_RETRY_ATTEMPTS,
                            error.message()
                        );
                        continue;
                    }
                    panic!("{}", error.message());
                }
            }
        }
    }

    fn wait_until_ready(
        &self,
        repo_path: &Path,
        child: &mut Child,
    ) -> Result<(), DaemonReadyError> {
        let repo_working_dir = repo_path.to_string_lossy().to_string();
        let mut last_status_error: Option<String> = None;
        let start = Instant::now();
        while start.elapsed() < DAEMON_TEST_READY_TOTAL_TIMEOUT {
            if let Some(status) = child.try_wait().map_err(|e| {
                DaemonReadyError::Fatal(format!("failed polling daemon child status: {}", e))
            })? {
                let stderr_tail = self.read_stderr_tail();
                let message = format!(
                    "daemon exited before becoming ready (pid {}, status {}): sockets {} {}{}",
                    self.pid,
                    status,
                    self.control_socket_path.display(),
                    self.trace_socket_path.display(),
                    stderr_tail
                );
                if is_windows_loader_init_failure(&status) {
                    return Err(DaemonReadyError::LoaderInitFailure(message));
                }
                return Err(DaemonReadyError::Fatal(message));
            }

            #[cfg(unix)]
            {
                if !is_process_alive(self.pid) {
                    let stderr_tail = self.read_stderr_tail();
                    return Err(DaemonReadyError::Fatal(
                        format!(
                            "daemon exited before becoming ready (pid {}): sockets {} {}",
                            self.pid,
                            self.control_socket_path.display(),
                            self.trace_socket_path.display()
                        ) + &stderr_tail,
                    ));
                }
            }

            let status = send_control_request_with_timeout(
                &self.control_socket_path,
                &ControlRequest::StatusFamily {
                    repo_working_dir: repo_working_dir.clone(),
                },
                DAEMON_TEST_READY_CONTROL_TIMEOUT,
            );
            match status {
                Ok(response) => {
                    if local_socket_connects_with_timeout(
                        &self.trace_socket_path,
                        DAEMON_TEST_PROBE_TIMEOUT,
                    )
                    .is_ok()
                    {
                        let baseline_seq = response
                            .data
                            .as_ref()
                            .and_then(|data| data.get("latest_seq"))
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0);
                        self.wait_until_trace_pipeline_ready(
                            repo_path,
                            &repo_working_dir,
                            baseline_seq,
                        )
                        .map_err(DaemonReadyError::Fatal)?;
                        return Ok(());
                    }
                }
                Err(error) => {
                    last_status_error = Some(error.to_string());
                }
            }
            thread::sleep(Duration::from_millis(25));
        }

        let stderr_tail = self.read_stderr_tail();
        Err(DaemonReadyError::Fatal(
            format!(
                "daemon did not become ready within {:?} at {} (trace socket: {}, last_status_error={})",
                DAEMON_TEST_READY_TOTAL_TIMEOUT,
                self.control_socket_path.display(),
                self.trace_socket_path.display(),
                last_status_error.as_deref().unwrap_or("none")
            ) + &stderr_tail,
        ))
    }

    pub(super) fn wait_until_trace_pipeline_ready(
        &self,
        repo_path: &Path,
        repo_working_dir: &str,
        baseline_seq: u64,
    ) -> Result<(), String> {
        #[cfg(windows)]
        let null_hooks = "NUL";
        #[cfg(not(windows))]
        let null_hooks = "/dev/null";

        let mut command = Command::new(real_git_executable());
        command
            .arg("-C")
            .arg(repo_path)
            .arg("-c")
            .arg(format!("core.hooksPath={}", null_hooks))
            .args(["config", "--local", "git-ai.test-readiness-probe", "1"])
            .env(
                "GIT_TRACE2_EVENT",
                DaemonConfig::trace2_event_target_for_path(&self.trace_socket_path),
            )
            .env("GIT_TRACE2_EVENT_NESTING", "0");
        configure_test_home_env(&mut command, &self.daemon_home);

        let output = run_command_output(&mut command, "daemon readiness probe git config")
            .map_err(|error| {
                format!("failed to run daemon readiness probe git config: {}", error)
            })?;
        if !output.status.success() {
            return Err(format!(
                "daemon readiness probe git config failed:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let start = Instant::now();
        while start.elapsed() < DAEMON_TEST_TRACE_READY_TIMEOUT {
            let response = send_control_request_with_timeout(
                &self.control_socket_path,
                &ControlRequest::StatusFamily {
                    repo_working_dir: repo_working_dir.to_string(),
                },
                DAEMON_TEST_CONTROL_TIMEOUT,
            )
            .map_err(|error| format!("failed polling daemon readiness seq: {}", error))?;
            let latest_seq = response
                .data
                .as_ref()
                .and_then(|data| data.get("latest_seq"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            if latest_seq > baseline_seq {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }

        Err(format!(
            "daemon trace pipeline did not advance latest_seq beyond {} for {}",
            baseline_seq, repo_working_dir
        ))
    }

    pub(super) fn read_stderr_tail(&self) -> String {
        let mut file = match fs::File::open(&self.stderr_log_path) {
            Ok(file) => file,
            Err(_) => return String::new(),
        };
        let mut content = String::new();
        if file.read_to_string(&mut content).is_err() {
            return String::new();
        }
        if content.trim().is_empty() {
            return String::new();
        }
        let mut lines: Vec<&str> = content.lines().collect();
        if lines.len() > 20 {
            lines = lines.split_off(lines.len() - 20);
        }
        format!(
            "\nDaemon stderr tail ({})\n{}",
            self.stderr_log_path.display(),
            lines.join("\n")
        )
    }

    pub(super) fn shutdown(&self) {
        let _ = send_control_request(&self.control_socket_path, &ControlRequest::Shutdown);

        #[cfg(unix)]
        {
            for _ in 0..200 {
                if reap_child_if_exited(self.pid) {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }

            let _ = unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) };
            for _ in 0..100 {
                if reap_child_if_exited(self.pid) {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        #[cfg(not(unix))]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output();
        }
    }

    /// Windows process termination is synchronous, but the daemon's lock
    /// handle can remain unavailable briefly while the process teardown is
    /// finalized. Wait for the lock itself before restarting this daemon's
    /// test home, otherwise the replacement can fail with "lock held".
    #[cfg(windows)]
    pub(super) fn wait_until_stopped(&self) {
        let lock_path = DaemonConfig::from_home(&self.daemon_home).lock_path;
        let started = Instant::now();
        while started.elapsed() < DAEMON_TEST_SHUTDOWN_TIMEOUT {
            if let Some(lock) = LockFile::try_acquire(&lock_path) {
                drop(lock);
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }

        panic!(
            "daemon process {} did not release test lock within {:?}: {}",
            self.pid,
            DAEMON_TEST_SHUTDOWN_TIMEOUT,
            lock_path.display()
        );
    }
}

/// Number of times a daemon spawn is retried when the Windows OS loader fails
/// to even start the process image (see [`is_windows_loader_init_failure`]).
pub(crate) const DAEMON_SPAWN_LOADER_RETRY_ATTEMPTS: usize = 5;

/// Outcome of a failed daemon-readiness wait, distinguishing a transient
/// Windows loader hiccup (respawn) from a genuine failure (fail loudly).
enum DaemonReadyError {
    /// The Windows loader aborted process startup; safe to respawn.
    LoaderInitFailure(String),
    /// Any other failure — the daemon started and misbehaved, or timed out.
    Fatal(String),
}

impl DaemonReadyError {
    fn message(&self) -> &str {
        match self {
            DaemonReadyError::LoaderInitFailure(m) | DaemonReadyError::Fatal(m) => m,
        }
    }
}

/// Returns `true` when `status` indicates the Windows process loader failed to
/// initialize the process image *before any of our code ran* — i.e. the daemon
/// never had a chance to start, as opposed to starting and then failing.
///
/// On the GitHub-hosted Windows runners, spawning many short-lived processes
/// concurrently occasionally trips `STATUS_DLL_INIT_FAILED` (0xC0000142) or
/// `STATUS_DLL_NOT_FOUND` (0xC0000135): the loader aborts during DLL
/// initialization and the process exits before `main`. This is an environment
/// hiccup, not a daemon defect, so the test harness respawns rather than
/// failing. The match is intentionally narrow — any *other* nonzero exit
/// (including a daemon that starts and then crashes) is still a hard failure.
pub(crate) fn is_windows_loader_init_failure(status: &std::process::ExitStatus) -> bool {
    if !cfg!(windows) {
        return false;
    }
    // ExitStatus::code() returns the raw NTSTATUS as i32 on Windows.
    matches!(
        status.code(),
        Some(code) if (code as u32) == 0xC000_0142 || (code as u32) == 0xC000_0135
    )
}
