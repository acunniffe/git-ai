use crate::daemon::daemon_log_file_path;
use crate::daemon::{
    ControlRequest, DaemonConfig, local_socket_connects_with_timeout, read_daemon_pid,
    remove_stale_daemon_files, send_control_request, send_control_request_with_timeout,
};
use crate::sandbox::ensure_daemon_start_allowed;
use crate::utils::LockFile;
#[cfg(windows)]
use crate::utils::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(windows)]
use std::{ffi::OsStr, path::Path};

pub fn handle_daemon(args: &[String]) {
    if args.is_empty() || is_help(args[0].as_str()) {
        print_help();
        std::process::exit(0);
    }

    match args[0].as_str() {
        "start" => {
            if let Err(e) = handle_start(&args[1..]) {
                eprintln!("Failed to start: {}", e);
                std::process::exit(1);
            }
        }
        "run" => {
            if let Err(e) = handle_run(&args[1..]) {
                eprintln!("Failed to run: {}", e);
                std::process::exit(1);
            }
        }
        "status" => {
            let repo = parse_repo_arg(&args[1..]).unwrap_or_else(default_repo_path);
            if let Err(e) = handle_status(repo) {
                eprintln!("Failed to get status: {}", e);
                std::process::exit(1);
            }
        }
        "shutdown" => {
            if let Err(e) = handle_shutdown(&args[1..]) {
                eprintln!("Failed to shut down: {}", e);
                std::process::exit(1);
            }
        }
        "restart" => {
            if let Err(e) = handle_restart(&args[1..]) {
                eprintln!("Failed to restart: {}", e);
                std::process::exit(1);
            }
        }
        "tail" => {
            if let Err(e) = handle_tail(&args[1..]) {
                eprintln!("Failed to tail log: {}", e);
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("Unknown subcommand: {}", args[0]);
            print_help();
            std::process::exit(1);
        }
    }
}

fn handle_start(args: &[String]) -> Result<(), String> {
    if has_flag(args, "--mode") {
        return Err("--mode is no longer supported; daemon always runs in write mode".to_string());
    }
    ensure_daemon_running_attached(daemon_startup_timeout()).map(|_| ())
}

fn daemon_startup_timeout() -> Duration {
    #[cfg(windows)]
    {
        if std::env::var_os("GIT_AI_TEST_DB_PATH").is_some()
            || std::env::var_os("GITAI_TEST_DB_PATH").is_some()
            || std::env::var_os("CI").is_some()
        {
            return Duration::from_secs(12);
        }

        Duration::from_secs(5)
    }

    #[cfg(not(windows))]
    {
        Duration::from_secs(2)
    }
}

/// Spawn a daemon and wait for it to become healthy. Used by explicit CLI
/// commands (`bg start`, `bg restart`) — NOT guarded for test builds.
///
/// On Unix, spawns with piped stderr so startup failures are surfaced to the
/// user. On Windows, spawns fully detached (null stdio) because piped handles
/// cause the parent to hang when the daemon outlives it.
fn ensure_daemon_running_attached(timeout: Duration) -> Result<DaemonConfig, String> {
    let config = daemon_config_from_env_or_default_paths()?;
    if daemon_is_up(&config) {
        return Ok(config);
    }

    ensure_daemon_start_allowed()?;

    remove_stale_daemon_files(&config);

    if daemon_startup_is_blocked(&config) {
        return Err(format!(
            "daemon startup blocked: lock held at {}",
            config.lock_path.display()
        ));
    }

    #[cfg(not(windows))]
    {
        let mut child = spawn_daemon_run_with_piped_stderr(&config)?;
        let deadline = Instant::now() + timeout;
        loop {
            if daemon_is_up(&config) {
                return Ok(config);
            }
            match child.try_wait() {
                Ok(Some(status)) if !status.success() => {
                    let mut stderr_buf = String::new();
                    if let Some(mut stderr) = child.stderr.take() {
                        use std::io::Read;
                        let _ = stderr.read_to_string(&mut stderr_buf);
                    }
                    let detail = if stderr_buf.trim().is_empty() {
                        format!("daemon process exited with {}", status)
                    } else {
                        stderr_buf.trim().to_string()
                    };
                    return Err(format!("daemon failed to start: {}", detail));
                }
                Ok(Some(_)) => {
                    return Err("daemon process exited before sockets were ready".to_string());
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out after {:?} waiting for daemon sockets {} and {}",
                    timeout,
                    config.control_socket_path.display(),
                    config.trace_socket_path.display()
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(windows)]
    {
        spawn_daemon_run_detached(&config)?;
        if wait_for_daemon_up(&config, timeout) {
            return Ok(config);
        }
        Err(format!(
            "timed out after {:?} waiting for daemon sockets {} and {}",
            timeout,
            config.control_socket_path.display(),
            config.trace_socket_path.display()
        ))
    }
}

fn daemon_config_from_env_or_default_paths() -> Result<DaemonConfig, String> {
    DaemonConfig::from_env_or_default_paths().map_err(|e| e.to_string())
}

fn handle_run(args: &[String]) -> Result<(), String> {
    if has_flag(args, "--mode") {
        return Err("--mode is no longer supported; daemon always runs in write mode".to_string());
    }
    ensure_daemon_start_allowed()?;
    let config = daemon_config_from_env_or_default_paths()?;
    let runtime_dir = daemon_runtime_dir(&config)?;
    std::env::set_current_dir(&runtime_dir).map_err(|e| {
        format!(
            "failed to set daemon runtime cwd to {}: {}",
            runtime_dir.display(),
            e
        )
    })?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let exit_action = runtime
        .block_on(async move { crate::daemon::run_daemon(config).await })
        .map_err(|e| e.to_string())?;

    match exit_action {
        crate::daemon::DaemonExitAction::Stop => {}
        crate::daemon::DaemonExitAction::Restart => {
            ensure_daemon_running(Duration::from_secs(5)).map(|_| ())?;
        }
        crate::daemon::DaemonExitAction::RestartAfterUpdate => {
            // Daemon is fully dead (lock released, sockets removed, threads joined).
            // Now safe to self-update — if the install cannot proceed, bring the
            // daemon back so a failed update does not leave the service down.
            match crate::daemon::daemon_run_pending_self_update() {
                crate::daemon::DaemonSelfUpdateOutcome::Installed => {
                    #[cfg(not(windows))]
                    {
                        ensure_daemon_running(Duration::from_secs(5)).map(|_| ())?;
                    }
                }
                crate::daemon::DaemonSelfUpdateOutcome::NoUpdate
                | crate::daemon::DaemonSelfUpdateOutcome::Failed => {
                    ensure_daemon_running(Duration::from_secs(5)).map(|_| ())?;
                }
            }
        }
    }

    Ok(())
}

pub(crate) fn ensure_daemon_running(
    #[cfg_attr(any(test, feature = "test-support"), allow(unused))] timeout: Duration,
) -> Result<DaemonConfig, String> {
    let config = daemon_config_from_env_or_default_paths()?;
    if daemon_is_up(&config) {
        return Ok(config);
    }

    // In test builds, never auto-spawn a daemon. The test harness manages
    // daemon lifecycle via DaemonProcess::start / shared daemon pool.
    // Without this guard, parallel test threads that see a briefly-unavailable
    // daemon each call spawn_daemon_run_detached, creating a process storm.
    #[cfg(any(test, feature = "test-support"))]
    {
        Err("daemon not running (test build: auto-spawn disabled)".to_string())
    }

    #[cfg(not(any(test, feature = "test-support")))]
    {
        if std::env::var("_GITAI_INTERNAL_DISABLE_WRAPPER_DAEMON_AUTOSPAWN")
            .is_ok_and(|v| v == "1" || v == "true")
        {
            return Err(
                "daemon auto-spawn disabled (_GITAI_INTERNAL_DISABLE_WRAPPER_DAEMON_AUTOSPAWN)"
                    .to_string(),
            );
        }

        start_daemon_detached_with_config(config, timeout)
    }
}

fn daemon_startup_is_blocked(config: &DaemonConfig) -> bool {
    if let Some(parent) = config.lock_path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return false;
    }

    match LockFile::try_acquire(&config.lock_path) {
        Some(lock) => {
            drop(lock);
            false
        }
        None => true,
    }
}

pub(crate) fn daemon_is_up(config: &DaemonConfig) -> bool {
    #[cfg(not(windows))]
    {
        if !config.control_socket_path.exists() || !config.trace_socket_path.exists() {
            return false;
        }
    }
    let probe_timeout = Duration::from_millis(100);
    let control_ok = send_control_request_with_timeout(
        &config.control_socket_path,
        &ControlRequest::Ping,
        probe_timeout,
    )
    .is_ok();
    control_ok
        && local_socket_connects_with_timeout(&config.trace_socket_path, probe_timeout).is_ok()
}

#[cfg(any(windows, not(any(test, feature = "test-support"))))]
fn wait_for_daemon_up(config: &DaemonConfig, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if daemon_is_up(config) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(not(any(test, feature = "test-support")))]
fn start_daemon_detached_with_config(
    config: DaemonConfig,
    timeout: Duration,
) -> Result<DaemonConfig, String> {
    if daemon_is_up(&config) {
        return Ok(config);
    }

    ensure_daemon_start_allowed()?;

    remove_stale_daemon_files(&config);

    if daemon_startup_is_blocked(&config) {
        return Err(format!(
            "daemon startup blocked: lock held at {}",
            config.lock_path.display()
        ));
    }

    spawn_daemon_run_detached(&config)?;
    if wait_for_daemon_up(&config, timeout) {
        return Ok(config);
    }

    Err(format!(
        "timed out after {:?} waiting for daemon sockets {} and {}",
        timeout,
        config.control_socket_path.display(),
        config.trace_socket_path.display()
    ))
}

fn daemon_runtime_dir(config: &DaemonConfig) -> Result<PathBuf, String> {
    config.ensure_parent_dirs().map_err(|e| e.to_string())?;
    config
        .lock_path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| "daemon lock path has no parent".to_string())
}

#[cfg(windows)]
fn powershell_single_quote_literal(value: &OsStr) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "''"))
}

#[cfg(any(windows, not(any(test, feature = "test-support"))))]
fn spawn_daemon_run_detached(config: &DaemonConfig) -> Result<(), String> {
    // Use current_git_ai_exe() instead of current_exe() to resolve through
    // symlinks. When the current exe is the git shim (e.g. ~/.local/bin/git),
    // current_exe() would spawn `git daemon run` which re-enters handle_git()
    // instead of handle_git_ai(), causing a fork bomb in async mode.
    let exe = crate::utils::current_git_ai_exe().map_err(|e| e.to_string())?;
    let runtime_dir = daemon_runtime_dir(config)?;

    #[cfg(windows)]
    {
        let script = format!(
            "Start-Process -FilePath {} -ArgumentList @('bg','run') -WorkingDirectory {} -WindowStyle Hidden",
            powershell_single_quote_literal(exe.as_os_str()),
            powershell_single_quote_literal(Path::new(&runtime_dir).as_os_str())
        );
        let mut child = Command::new("powershell.exe");
        child
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-WindowStyle")
            .arg("Hidden")
            .arg("-Command")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Remove git environment variables that must not leak into the daemon.
        for var in crate::daemon::GIT_ENV_VARS_TO_SANITIZE {
            child.env_remove(var);
        }
        child.env_remove("GIT_AI");

        let preferred_flags =
            CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB;
        child.creation_flags(preferred_flags);
        match child.spawn() {
            Ok(_) => Ok(()),
            Err(preferred_err) => {
                tracing::debug!(
                    "detached daemon spawn with CREATE_BREAKAWAY_FROM_JOB failed, retrying without it: {}",
                    preferred_err
                );
                child.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
                child.spawn().map(|_| ()).map_err(|fallback_err| {
                    format!(
                        "failed to spawn detached daemon with flags {:#x}: {}; retry without CREATE_BREAKAWAY_FROM_JOB also failed: {}",
                        preferred_flags, preferred_err, fallback_err
                    )
                })
            }
        }
    }

    #[cfg(not(windows))]
    {
        let mut child = Command::new(exe);
        child
            .arg("bg")
            .arg("run")
            .current_dir(&runtime_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Remove git environment variables that must not leak into the daemon.
        // The daemon is repository-agnostic; variables like GIT_DIR override
        // the -C flag and cause repository resolution failures.
        for var in crate::daemon::GIT_ENV_VARS_TO_SANITIZE {
            child.env_remove(var);
        }
        // GIT_AI controls debug routing in the binary (GIT_AI=git → handle_git).
        // A daemon that inherits this would route "bg run" to the git proxy instead
        // of starting as a daemon.
        child.env_remove("GIT_AI");
        child.spawn().map(|_| ()).map_err(|e| e.to_string())
    }
}

#[cfg(not(windows))]
fn spawn_daemon_run_with_piped_stderr(
    config: &DaemonConfig,
) -> Result<std::process::Child, String> {
    let exe = crate::utils::current_git_ai_exe().map_err(|e| e.to_string())?;
    let runtime_dir = daemon_runtime_dir(config)?;
    let mut child = Command::new(exe);
    child
        .arg("bg")
        .arg("run")
        .current_dir(&runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for var in crate::daemon::GIT_ENV_VARS_TO_SANITIZE {
        child.env_remove(var);
    }
    child.env_remove("GIT_AI");
    child.spawn().map_err(|e| e.to_string())
}

fn handle_status(repo_working_dir: String) -> Result<(), String> {
    let config = daemon_config_from_env_or_default_paths()?;

    // Check if the path is inside a git repository before contacting the daemon.
    // When run outside a git repo, still check daemon health but skip the
    // family-level status query which requires a valid repo.
    if crate::git::find_repository_in_path(&repo_working_dir).is_err() {
        let daemon_running = daemon_is_up(&config);
        let response = serde_json::json!({
            "ok": true,
            "git_repo": false,
            "daemon_running": daemon_running,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&response).map_err(|e| e.to_string())?
        );
        return Ok(());
    }

    let request = ControlRequest::StatusFamily { repo_working_dir };
    let response =
        send_control_request(&config.control_socket_path, &request).map_err(|e| e.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&response).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn handle_tail(args: &[String]) -> Result<(), String> {
    let config = daemon_config_from_env_or_default_paths()?;
    if !daemon_is_up(&config) {
        return Err("background service is not running".to_string());
    }

    let log_path =
        daemon_log_file_path(&config).map_err(|e| format!("cannot locate log: {}", e))?;
    if !log_path.exists() {
        return Err(format!("log file not found: {}", log_path.display()));
    }

    let full = has_flag(args, "--full");
    let follow = has_flag(args, "--follow") || has_flag(args, "-f");
    let lines: usize = parse_number_arg(args, "-n")
        .or_else(|| parse_number_arg(args, "--lines"))
        .unwrap_or(20);

    let file = std::fs::File::open(&log_path)
        .map_err(|e| format!("cannot open {}: {}", log_path.display(), e))?;

    if full {
        // Print entire file then continue tailing.
        let reader = BufReader::new(&file);
        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;
            println!("{}", line);
        }
    } else {
        // Print last N lines.
        print_last_n_lines(&file, lines).map_err(|e| e.to_string())?;
    }

    if follow {
        tail_file(file).map_err(|e| e.to_string())
    } else {
        Ok(())
    }
}

fn parse_number_arg(args: &[String], flag: &str) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag && i + 1 < args.len() {
            return args[i + 1].parse().ok();
        }
        i += 1;
    }
    None
}

fn print_last_n_lines(file: &std::fs::File, n: usize) -> Result<(), std::io::Error> {
    use std::io::Read;
    let metadata = file.metadata()?;
    let file_size = metadata.len();
    if file_size == 0 {
        return Ok(());
    }

    // Read up to 64KB from the end to find the last N lines.
    let read_size = file_size.min(64 * 1024) as usize;
    let mut buf = vec![0u8; read_size];
    let mut f = file;
    f.seek(SeekFrom::End(-(read_size as i64)))?;
    f.read_exact(&mut buf)?;

    let text = String::from_utf8_lossy(&buf);
    let all_lines: Vec<&str> = text.lines().collect();
    let start = all_lines.len().saturating_sub(n);
    for line in &all_lines[start..] {
        println!("{}", line);
    }

    // Seek to end so tail_file can pick up from here.
    f.seek(SeekFrom::End(0))?;
    Ok(())
}

fn tail_file(file: std::fs::File) -> Result<(), std::io::Error> {
    let mut reader = BufReader::new(file);
    // Seek to end in case print_last_n_lines didn't (full mode).
    reader.seek(SeekFrom::End(0))?;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n > 0 {
            print!("{}", line);
        } else {
            thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Timeout for graceful shutdown before a hard kill during restart.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn handle_shutdown(args: &[String]) -> Result<(), String> {
    let config = daemon_config_from_env_or_default_paths()?;
    if has_flag(args, "--hard") {
        if !daemon_is_up(&config) && !daemon_startup_is_blocked(&config) {
            return Err("background service is not running".to_string());
        }
        hard_kill_daemon(&config)
    } else {
        soft_shutdown_daemon(&config)
    }
}

fn handle_restart(args: &[String]) -> Result<(), String> {
    let config = daemon_config_from_env_or_default_paths()?;
    let hard = has_flag(args, "--hard");

    // Only attempt shutdown if daemon appears to be running.
    let was_running = daemon_is_up(&config) || daemon_startup_is_blocked(&config);
    if was_running {
        // Read the PID before shutdown so we can verify the process actually dies.
        let old_pid = read_daemon_pid(&config).ok();

        if hard {
            hard_kill_daemon(&config)?;
        } else {
            // Attempt soft shutdown; escalate to hard kill on timeout.
            let _ = send_control_request(&config.control_socket_path, &ControlRequest::Shutdown);
            if !wait_for_daemon_dead(&config, GRACEFUL_SHUTDOWN_TIMEOUT) {
                eprintln!("graceful shutdown timed out, force-killing daemon");
                hard_kill_daemon(&config)?;
            }
        }

        // Even after lock+sockets are gone, the process may still be alive
        // (e.g. tokio runtime draining blocking tasks). Verify and force-kill.
        if let Some(pid) = old_pid {
            wait_for_process_exit(pid, Duration::from_secs(2));
        }
    }

    // Start a fresh daemon.
    ensure_daemon_running_attached(daemon_startup_timeout()).map(|_| ())
}

fn soft_shutdown_daemon(config: &DaemonConfig) -> Result<(), String> {
    let response = send_control_request(&config.control_socket_path, &ControlRequest::Shutdown)
        .map_err(|e| e.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&response).map_err(|e| e.to_string())?
    );
    Ok(())
}

#[cfg(unix)]
fn hard_kill_daemon(config: &DaemonConfig) -> Result<(), String> {
    let pid = read_daemon_pid(config).map_err(|e| format!("cannot read daemon pid: {}", e))?;
    let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            // Process already dead — not an error.
            return Ok(());
        }
        return Err(format!("kill -9 {} failed: {}", pid, err));
    }
    // Wait briefly for the OS to reap the process and release the lock.
    let _ = wait_for_daemon_dead(config, Duration::from_secs(2));
    Ok(())
}

#[cfg(windows)]
fn hard_kill_daemon(config: &DaemonConfig) -> Result<(), String> {
    let pid = read_daemon_pid(config).map_err(|e| format!("cannot read daemon pid: {}", e))?;
    let output = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .output()
        .map_err(|e| format!("failed to run taskkill: {}", e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Process already dead is not an error.
        if !stderr.contains("not found") {
            return Err(format!(
                "taskkill /F /T /PID {} failed: {}",
                pid,
                stderr.trim()
            ));
        }
    }
    let _ = wait_for_daemon_dead(config, Duration::from_secs(2));
    Ok(())
}

fn wait_for_daemon_dead(config: &DaemonConfig, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let sockets_down = !daemon_is_up(config);
        let lock_free = LockFile::try_acquire(&config.lock_path)
            .map(|l| {
                drop(l);
                true
            })
            .unwrap_or(false);
        if sockets_down && lock_free {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Wait for a process to exit, force-killing it if it doesn't die within timeout.
/// This handles the case where the daemon lock/sockets are gone but the process
/// is still alive (e.g. tokio runtime draining blocking tasks).
///
/// Note: relies on PID liveness only. Theoretically susceptible to PID reuse if
/// the process is reaped and the PID recycled within the timeout window, but on
/// macOS/Linux with ~100k PID space this is not a realistic concern.
#[cfg(unix)]
fn wait_for_process_exit(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if ret != 0 {
            return; // Process is dead
        }
        if Instant::now() >= deadline {
            // Process still alive after timeout — force kill
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn wait_for_process_exit(_pid: u32, _timeout: Duration) {
    // On Windows, hard_kill_daemon uses taskkill /F which is synchronous.
}

/// Shut down the running daemon (soft then hard) and wait for it to fully exit.
/// Used by internal callers (install-hooks, upgrade) that need the daemon stopped
/// before proceeding.
pub(crate) fn stop_daemon(config: &DaemonConfig, timeout: Duration) -> Result<(), String> {
    // Nothing to do if daemon isn't running.
    if !daemon_is_up(config) && !daemon_startup_is_blocked(config) {
        return Ok(());
    }

    // Attempt soft shutdown via control socket if reachable.
    if local_socket_connects_with_timeout(&config.control_socket_path, Duration::from_millis(100))
        .is_ok()
    {
        let _ = send_control_request(&config.control_socket_path, &ControlRequest::Shutdown);
    }

    if wait_for_daemon_dead(config, timeout) {
        return Ok(());
    }

    // Soft shutdown didn't work — escalate.
    hard_kill_daemon(config)
}

/// Shut down the running daemon and start a fresh one. Escalates to hard kill
/// if the soft shutdown doesn't complete within GRACEFUL_SHUTDOWN_TIMEOUT.
pub(crate) fn restart_daemon(config: &DaemonConfig) -> Result<(), String> {
    let was_running = daemon_is_up(config) || daemon_startup_is_blocked(config);
    if was_running {
        stop_daemon(config, GRACEFUL_SHUTDOWN_TIMEOUT)?;
    }
    ensure_daemon_running(Duration::from_secs(5)).map(|_| ())
}

fn parse_repo_arg(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--repo" && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

fn default_repo_path() -> String {
    PathBuf::from(".")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."))
        .to_string_lossy()
        .to_string()
}

fn is_help(value: &str) -> bool {
    value == "help" || value == "--help" || value == "-h"
}

fn print_help() {
    eprintln!("git-ai bg - run and control git-ai background service");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  git-ai bg start");
    eprintln!("  git-ai bg run");
    eprintln!("  git-ai bg status [--repo <path>]");
    eprintln!("  git-ai bg shutdown [--hard]");
    eprintln!("  git-ai bg restart [--hard]");
    eprintln!("  git-ai bg tail [-n <lines>] [--full] [-f | --follow]");
}
