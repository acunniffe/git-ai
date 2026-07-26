use super::daemon_config::DaemonConfig;
use crate::error::GitAiError;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle};

pub const PID_META_FILE: &str = "daemon.pid.json";

#[cfg(windows)]
pub const WINDOWS_STDOUT_HANDLE: u32 = (-11i32) as u32;
#[cfg(windows)]
pub const WINDOWS_STDERR_HANDLE: u32 = (-12i32) as u32;

#[cfg(windows)]
unsafe extern "system" {
    fn SetStdHandle(nstdhandle: u32, hhandle: *mut std::ffi::c_void) -> i32;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonPidMeta {
    pid: u32,
    started_at_ns: u128,
}

pub fn now_unix_nanos() -> u128 {
    crate::model::clock::now_nanos()
}

pub fn remove_socket_if_exists(path: &Path) -> Result<(), GitAiError> {
    #[cfg(unix)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(not(windows))]
pub fn set_socket_owner_only(path: &Path) -> Result<(), GitAiError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

pub fn pid_metadata_path(config: &DaemonConfig) -> PathBuf {
    config
        .lock_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(PID_META_FILE)
}

/// Returns the log file path for the currently running daemon, if any.
/// Reads the PID from daemon.pid.json and constructs the log path.
pub fn daemon_log_file_path(config: &DaemonConfig) -> Result<PathBuf, GitAiError> {
    let meta_path = pid_metadata_path(config);
    let contents = fs::read_to_string(&meta_path).map_err(|e| {
        GitAiError::Generic(format!(
            "failed to read daemon pid metadata at {}: {}",
            meta_path.display(),
            e
        ))
    })?;
    let meta: DaemonPidMeta = serde_json::from_str(&contents)?;
    let log_dir = config.internal_dir.join("daemon").join("logs");
    Ok(log_dir.join(format!("{}.log", meta.pid)))
}

pub fn write_pid_metadata(config: &DaemonConfig) -> Result<(), GitAiError> {
    let meta = DaemonPidMeta {
        pid: std::process::id(),
        started_at_ns: now_unix_nanos(),
    };
    let path = pid_metadata_path(config);
    fs::write(path, serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}

/// Read the PID of the currently running daemon from the pid metadata file.
pub fn read_daemon_pid(config: &DaemonConfig) -> Result<u32, GitAiError> {
    let meta_path = pid_metadata_path(config);
    let contents = fs::read_to_string(&meta_path).map_err(|e| {
        GitAiError::Generic(format!(
            "failed to read daemon pid metadata at {}: {}",
            meta_path.display(),
            e
        ))
    })?;
    let meta: DaemonPidMeta = serde_json::from_str(&contents)?;
    Ok(meta.pid)
}

pub fn remove_pid_metadata(config: &DaemonConfig) -> Result<(), GitAiError> {
    let path = pid_metadata_path(config);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Remove daemon artifacts that may be inaccessible due to ownership mismatch
/// (e.g. left by a prior root invocation). Called only from `run_daemon()` at
/// startup — never from probe functions — so it cannot break flock visibility
/// for read-only lock checks.
#[cfg(unix)]
pub fn remove_stale_daemon_files(config: &DaemonConfig) {
    let pid_path = pid_metadata_path(config);
    for path in [
        config.lock_path.as_path(),
        config.control_socket_path.as_path(),
        config.trace_socket_path.as_path(),
        pid_path.as_path(),
    ] {
        let dominated_by_wrong_owner = match std::fs::metadata(path) {
            Ok(meta) => {
                use std::os::unix::fs::MetadataExt;
                meta.uid() != unsafe { libc::getuid() }
            }
            Err(_) => false,
        };
        if dominated_by_wrong_owner {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(not(unix))]
pub fn remove_stale_daemon_files(_config: &DaemonConfig) {}

#[cfg(not(windows))]
pub fn daemon_is_test_mode() -> bool {
    std::env::var_os("GIT_AI_TEST_DB_PATH").is_some()
        || std::env::var_os("GITAI_TEST_DB_PATH").is_some()
}

pub fn daemon_log_dir(config: &DaemonConfig) -> PathBuf {
    config.internal_dir.join("daemon").join("logs")
}

/// Redirect stdout and stderr to a per-PID log file inside the daemon logs
/// directory. Skipped in test mode to keep test output on the console.
/// Returns a guard that keeps the log file open for the lifetime of the daemon.
#[cfg(unix)]
pub fn maybe_setup_daemon_log_file(config: &DaemonConfig) -> Option<DaemonLogGuard> {
    if daemon_is_test_mode() {
        return None;
    }
    match setup_daemon_log_file(config) {
        Ok(guard) => Some(guard),
        Err(e) => {
            tracing::error!(%e, "log file setup failed");
            None
        }
    }
}

#[cfg(windows)]
pub fn maybe_setup_daemon_log_file(config: &DaemonConfig) -> Option<DaemonLogGuard> {
    match setup_daemon_log_file(config) {
        Ok(guard) => Some(guard),
        Err(e) => {
            tracing::error!(%e, "log file setup failed");
            None
        }
    }
}

pub struct DaemonLogGuard {
    _file: File,
}

#[cfg(unix)]
pub fn setup_daemon_log_file(config: &DaemonConfig) -> Result<DaemonLogGuard, GitAiError> {
    use std::os::unix::io::AsRawFd;

    let log_dir = daemon_log_dir(config);
    fs::create_dir_all(&log_dir)?;

    let prune_dir = log_dir.clone();
    std::thread::spawn(move || prune_stale_daemon_logs(&prune_dir));

    let log_path = log_dir.join(format!("{}.log", std::process::id()));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let fd = file.as_raw_fd();
    // SAFETY: dup2 is a standard POSIX call; we redirect stdout/stderr to our
    // open log file descriptor. The file is kept alive by the returned guard.
    unsafe {
        if libc::dup2(fd, libc::STDOUT_FILENO) == -1 {
            return Err(GitAiError::Generic("dup2 stdout failed".to_string()));
        }
        if libc::dup2(fd, libc::STDERR_FILENO) == -1 {
            return Err(GitAiError::Generic("dup2 stderr failed".to_string()));
        }
    }

    Ok(DaemonLogGuard { _file: file })
}

#[cfg(windows)]
pub fn setup_daemon_log_file(config: &DaemonConfig) -> Result<DaemonLogGuard, GitAiError> {
    let log_dir = daemon_log_dir(config);
    fs::create_dir_all(&log_dir)?;

    let prune_dir = log_dir.clone();
    std::thread::spawn(move || prune_stale_daemon_logs(&prune_dir));

    let log_path = log_dir.join(format!("{}.log", std::process::id()));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    redirect_windows_stdio_to_log_file(&file)?;
    eprintln!("[git-ai] daemon log initialized at {}", log_path.display());

    Ok(DaemonLogGuard { _file: file })
}

#[cfg(windows)]
pub fn redirect_windows_stdio_to_log_file(file: &File) -> Result<(), GitAiError> {
    redirect_windows_stdio_stream(file, 1, WINDOWS_STDOUT_HANDLE)?;
    redirect_windows_stdio_stream(file, 2, WINDOWS_STDERR_HANDLE)?;
    Ok(())
}

#[cfg(windows)]
pub fn redirect_windows_stdio_stream(
    file: &File,
    std_fd: libc::c_int,
    std_handle: u32,
) -> Result<(), GitAiError> {
    let clone = file.try_clone()?;
    let raw_handle = clone.into_raw_handle();
    let fd = unsafe {
        libc::open_osfhandle(
            raw_handle as libc::intptr_t,
            libc::O_APPEND | libc::O_BINARY,
        )
    };
    if fd == -1 {
        unsafe {
            drop(File::from_raw_handle(raw_handle));
        }
        return Err(GitAiError::Generic(format!(
            "open_osfhandle failed for daemon log stream {}: {}",
            std_fd,
            std::io::Error::last_os_error()
        )));
    }

    let dup_result = unsafe { libc::dup2(fd, std_fd) };
    if dup_result == -1 {
        let err = std::io::Error::last_os_error();
        let _ = unsafe { libc::close(fd) };
        return Err(GitAiError::Generic(format!(
            "dup2 failed for daemon log stream {}: {}",
            std_fd, err
        )));
    }
    if unsafe { libc::close(fd) } == -1 {
        tracing::debug!(
            std_fd,
            error = %std::io::Error::last_os_error(),
            "close failed for log stream after successful redirect"
        );
    }

    let set_handle_result = unsafe { SetStdHandle(std_handle, file.as_raw_handle()) };
    if set_handle_result == 0 {
        return Err(GitAiError::Generic(format!(
            "SetStdHandle failed for daemon log stream {}: {}",
            std_fd,
            std::io::Error::last_os_error()
        )));
    }

    Ok(())
}

/// Remove log files from previous daemon runs that are older than one week and
/// whose PID is no longer alive, to avoid unbounded growth while keeping recent
/// logs available for debugging.
pub fn prune_stale_daemon_logs(log_dir: &Path) {
    let one_week = std::time::Duration::from_secs(7 * 24 * 60 * 60);
    let entries = match fs::read_dir(log_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        let _pid: u32 = match stem.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let dominated = path
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > one_week);
        if !dominated {
            continue;
        }
        #[cfg(unix)]
        {
            if process_alive(_pid) {
                continue;
            }
        }
        let _ = fs::remove_file(&path);
    }
}

#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    // kill(pid, 0) checks existence without sending a signal.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

pub fn read_json_line<R: BufRead>(reader: &mut R) -> Result<Option<String>, GitAiError> {
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

pub fn read_json_line_bounded<R: BufRead>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<String>, GitAiError> {
    let read_limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut limited = std::io::Read::take(&mut *reader, read_limit);
    let mut line = String::new();
    let read = limited.read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if read > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("daemon control frame exceeds {max_bytes} bytes"),
        )
        .into());
    }
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bounded_json_line_rejects_oversized_input_without_consuming_the_tail() {
        let mut reader = Cursor::new(b"1234567890\nnext\n".to_vec());

        let error = read_json_line_bounded(&mut reader, 8).unwrap_err();

        assert_eq!(
            error.to_string(),
            "IO error: daemon control frame exceeds 8 bytes"
        );
        assert_eq!(reader.position(), 9);
    }

    #[test]
    fn bounded_json_line_accepts_a_frame_at_the_limit() {
        let mut reader = Cursor::new(b"1234567\nnext\n".to_vec());

        assert_eq!(
            read_json_line_bounded(&mut reader, 8).unwrap(),
            Some("1234567\n".to_string())
        );
        assert_eq!(
            read_json_line_bounded(&mut reader, 8).unwrap(),
            Some("next\n".to_string())
        );
    }
}
