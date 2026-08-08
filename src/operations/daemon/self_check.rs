//! Core types and generic command-execution infrastructure for `git-ai debug`
//! self-checks, plus the daemon-readiness self-check itself.
//!
//! The attribution self-check lives in `attribution_self_check`, trace2
//! validation lives in `crate::operations::git::trace2_validation`, and blame
//! result classification lives in `crate::operations::commands::blame`. All of
//! them build on the [`DiagnosticCheckResult`] / [`CommandRecord`] /
//! [`GitDiagnosticTarget`] types and command-running helpers defined here.

use crate::process_spawn::format_posix_shell_command as format_command;
use crate::process_timeout::run_command_with_timeout;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SELF_CHECK_TRACE_ENV_REMOVE: &[&str] = &[
    "GIT_TRACE2_PARENT_SID",
    "GIT_TRACE2_PARENT_NAME",
    "GIT_AI_WRAPPER_INVOCATION_ID",
    "GIT_TRACE2_ENV_VARS",
];
pub(crate) const DEBUG_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticStatus {
    Passed,
    Failed,
    Skipped,
}

impl DiagnosticStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DiagnosticStatus::Passed => "passed",
            DiagnosticStatus::Failed => "failed",
            DiagnosticStatus::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandRecord {
    pub command: String,
    pub cwd: Option<String>,
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl CommandRecord {
    pub(crate) fn success(&self) -> bool {
        !self.timed_out && self.status == Some(0)
    }
}

#[derive(Debug, Clone)]
pub struct DiagnosticCheckResult {
    pub status: DiagnosticStatus,
    pub summary: String,
    pub details: Vec<String>,
    pub commands: Vec<CommandRecord>,
    pub trace2_json: Option<String>,
}

impl DiagnosticCheckResult {
    pub(crate) fn passed(
        summary: impl Into<String>,
        details: Vec<String>,
        commands: Vec<CommandRecord>,
    ) -> Self {
        Self {
            status: DiagnosticStatus::Passed,
            summary: summary.into(),
            details,
            commands,
            trace2_json: None,
        }
    }

    pub(crate) fn failed(
        summary: impl Into<String>,
        details: Vec<String>,
        commands: Vec<CommandRecord>,
    ) -> Self {
        Self {
            status: DiagnosticStatus::Failed,
            summary: summary.into(),
            details,
            commands,
            trace2_json: None,
        }
    }

    pub(crate) fn skipped(summary: impl Into<String>, details: Vec<String>) -> Self {
        Self {
            status: DiagnosticStatus::Skipped,
            summary: summary.into(),
            details,
            commands: Vec::new(),
            trace2_json: None,
        }
    }

    pub(crate) fn with_trace2_json(mut self, trace2_json: Option<String>) -> Self {
        self.trace2_json = trace2_json;
        self
    }
}

#[derive(Debug, Clone)]
pub struct GitDiagnosticTarget {
    pub label: String,
    pub program: String,
}

impl GitDiagnosticTarget {
    pub fn new(label: impl Into<String>, program: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            program: program.into(),
        }
    }
}

pub fn prepare_daemon_for_debug_self_checks(git_program: &str) -> DiagnosticCheckResult {
    let mut commands = Vec::new();
    let mut details = Vec::new();
    let mut probe_deadline = Instant::now() + DEBUG_CHECK_TIMEOUT;

    let config = match crate::operations::daemon::DaemonConfig::from_env_or_default_paths() {
        Ok(config) => config,
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "daemon readiness could not be inspected",
                vec![format!("failed to determine daemon paths: {}", err)],
                commands,
            );
        }
    };

    details.push(format!(
        "control socket: {}",
        config.control_socket_path.display()
    ));
    details.push(format!(
        "trace2 socket: {}",
        config.trace_socket_path.display()
    ));
    details.push(format!("lock: {}", config.lock_path.display()));

    let initially_up = crate::operations::commands::daemon::daemon_is_up(&config);
    details.push(format!("initial daemon running: {}", initially_up));

    let mut restarted = false;
    if initially_up && daemon_binary_is_stale(&config).unwrap_or(false) {
        details.push("running daemon was started before the current git-ai binary was written; restarting daemon".to_string());
        if let Err(err) = crate::operations::commands::daemon::restart_daemon(&config) {
            details.push(format!("restart failed: {}", err));
            return DiagnosticCheckResult::failed(
                "daemon readiness check failed",
                details,
                commands,
            );
        }
        restarted = true;
        probe_deadline = Instant::now() + DEBUG_CHECK_TIMEOUT;
    } else if !initially_up {
        details.push("daemon was not running; starting daemon".to_string());
        if let Err(err) =
            crate::operations::commands::daemon::ensure_daemon_running(DEBUG_CHECK_TIMEOUT)
        {
            details.push(format!("start failed: {}", err));
            return DiagnosticCheckResult::failed(
                "daemon readiness check failed",
                details,
                commands,
            );
        }
        probe_deadline = Instant::now() + DEBUG_CHECK_TIMEOUT;
    }

    match run_daemon_trace2_ingestion_probe(&mut commands, git_program, &config, probe_deadline) {
        Ok(mut probe_details) => {
            details.append(&mut probe_details);
            details.push(format!("daemon restarted: {}", restarted));
            DiagnosticCheckResult::passed(
                "daemon is ready for debug self-checks",
                details,
                commands,
            )
        }
        Err(first_err) if !restarted => {
            details.push(format!(
                "initial trace2 daemon ingestion probe failed: {}",
                first_err
            ));
            details
                .push("restarting daemon and retrying trace2 daemon ingestion probe".to_string());
            if let Err(restart_err) = crate::operations::commands::daemon::restart_daemon(&config) {
                details.push(format!("restart failed: {}", restart_err));
                return DiagnosticCheckResult::failed(
                    "daemon readiness check failed",
                    details,
                    commands,
                );
            }
            restarted = true;

            match run_daemon_trace2_ingestion_probe(
                &mut commands,
                git_program,
                &config,
                Instant::now() + DEBUG_CHECK_TIMEOUT,
            ) {
                Ok(mut probe_details) => {
                    details.append(&mut probe_details);
                    details.push(format!("daemon restarted: {}", restarted));
                    DiagnosticCheckResult::passed(
                        "daemon is ready for debug self-checks",
                        details,
                        commands,
                    )
                }
                Err(retry_err) => {
                    details.push(format!(
                        "trace2 daemon ingestion probe failed after restart: {}",
                        retry_err
                    ));
                    details.push(format!("daemon restarted: {}", restarted));
                    DiagnosticCheckResult::failed(
                        "daemon readiness check failed",
                        details,
                        commands,
                    )
                }
            }
        }
        Err(err) => {
            details.push(format!("trace2 daemon ingestion probe failed: {}", err));
            details.push(format!("daemon restarted: {}", restarted));
            DiagnosticCheckResult::failed("daemon readiness check failed", details, commands)
        }
    }
}

fn run_daemon_trace2_ingestion_probe(
    commands: &mut Vec<CommandRecord>,
    git_program: &str,
    config: &crate::operations::daemon::DaemonConfig,
    deadline: Instant,
) -> Result<Vec<String>, String> {
    let probe_path = crate::diagnostic_sentinels::debug_self_check_root()
        .join(format!("daemon-probe-{}", crate::uuid::generate_v4()));

    let result = (|| -> Result<Vec<String>, String> {
        fs::create_dir_all(&probe_path)
            .map_err(|e| format!("failed to create {}: {}", probe_path.display(), e))?;
        run_required_until(
            commands,
            git_program,
            &["init", "."],
            Some(&probe_path),
            deadline,
        )?;

        let status = crate::operations::daemon::control_api::wait_for_daemon_family_status(
            config,
            &probe_path,
            1,
            deadline,
        )?;
        Ok(vec![
            format!("daemon trace2 probe repo: {}", probe_path.display()),
            format!("daemon trace2 probe latest_seq: {}", status.latest_seq),
            format!(
                "daemon trace2 probe last_error: {}",
                status.last_error.unwrap_or_else(|| "<none>".to_string())
            ),
        ])
    })();

    if result.is_ok() {
        let _ = fs::remove_dir_all(&probe_path);
    }

    result
}

pub(crate) fn run_required_until(
    commands: &mut Vec<CommandRecord>,
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    deadline: Instant,
) -> Result<CommandRecord, String> {
    let timeout = remaining_timeout(deadline);
    if timeout.is_zero() {
        let record = CommandRecord {
            command: format_command(program, args),
            cwd: cwd.map(|p| p.display().to_string()),
            status: None,
            stdout: String::new(),
            stderr: format!(
                "self-check timed out after {:.1}s before this command could start",
                DEBUG_CHECK_TIMEOUT.as_secs_f64()
            ),
            timed_out: true,
        };
        let error = format!("command timed out before start: {}", record.command);
        commands.push(record);
        return Err(error);
    }

    run_required_with_timeout(commands, program, args, cwd, timeout)
}

fn run_required_with_timeout(
    commands: &mut Vec<CommandRecord>,
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<CommandRecord, String> {
    let record = run_logged_command_with_timeout(program, args, cwd, timeout);
    let success = record.success();
    let error = if success {
        None
    } else if record.timed_out {
        let mut error = format!(
            "command timed out: {} (timeout={:.1}s, status={})",
            record.command,
            timeout.as_secs_f64(),
            format_status(record.status)
        );
        if let Some(cwd) = &record.cwd {
            error.push_str(&format!(", cwd={}", cwd));
        }
        if !record.stdout.trim().is_empty() {
            error.push_str(&format!(", stdout={}", record.stdout.trim()));
        }
        if !record.stderr.trim().is_empty() {
            error.push_str(&format!(", stderr={}", record.stderr.trim()));
        }
        Some(error)
    } else {
        Some(format!(
            "command failed: {} (status={})",
            record.command,
            format_status(record.status)
        ))
    };
    commands.push(record.clone());
    match error {
        Some(error) => Err(error),
        None => Ok(record),
    }
}

pub(crate) fn run_logged_command(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
) -> CommandRecord {
    run_logged_command_with_timeout(program, args, cwd, DEBUG_CHECK_TIMEOUT)
}

fn run_logged_command_with_timeout(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> CommandRecord {
    let command = format_command(program, args);
    let cwd_display = cwd.map(|p| p.display().to_string());
    match run_command_with_timeout(
        program,
        args,
        cwd,
        timeout,
        POLL_INTERVAL,
        SELF_CHECK_TRACE_ENV_REMOVE,
    ) {
        Ok(output) => {
            let stderr = format_logged_stderr(
                output.timed_out,
                timeout,
                output.stderr,
                output.diagnostics,
                output.wait_error,
            );
            CommandRecord {
                command,
                cwd: cwd_display,
                status: output.status,
                stdout: output.stdout,
                stderr,
                timed_out: output.timed_out,
            }
        }
        Err(e) => CommandRecord {
            command,
            cwd: cwd_display,
            status: None,
            stdout: String::new(),
            stderr: e,
            timed_out: false,
        },
    }
}

fn format_logged_stderr(
    timed_out: bool,
    timeout: Duration,
    process_stderr: String,
    diagnostics: Vec<String>,
    wait_error: Option<String>,
) -> String {
    let mut stderr = String::new();
    if timed_out {
        stderr.push_str(&format!("timed out after {:.1}s", timeout.as_secs_f64()));
        if !process_stderr.trim().is_empty() {
            stderr.push_str("\nstderr before timeout:\n");
            stderr.push_str(process_stderr.trim());
        }
    } else {
        stderr.push_str(process_stderr.trim());
    }

    if let Some(wait_error) = wait_error {
        append_stderr_line(
            &mut stderr,
            &format!("failed while waiting for command: {}", wait_error),
        );
    }
    for diagnostic in diagnostics {
        append_stderr_line(&mut stderr, &diagnostic);
    }
    stderr
}

fn append_stderr_line(stderr: &mut String, line: &str) {
    if !stderr.is_empty() {
        stderr.push('\n');
    }
    stderr.push_str(line);
}

pub(crate) fn remaining_timeout(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn daemon_binary_is_stale(
    config: &crate::operations::daemon::DaemonConfig,
) -> Result<bool, String> {
    let Some(started_at_ns) = read_daemon_started_at_ns(config)? else {
        return Ok(false);
    };
    let binary_modified_ns = current_binary_modified_ns()?;
    Ok(binary_modified_ns > started_at_ns)
}

fn read_daemon_started_at_ns(
    config: &crate::operations::daemon::DaemonConfig,
) -> Result<Option<u128>, String> {
    let pid_path = config.internal_dir.join("daemon").join("daemon.pid.json");
    let contents = match fs::read_to_string(&pid_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "failed to read daemon pid metadata at {}: {}",
                pid_path.display(),
                err
            ));
        }
    };
    let value: Value = serde_json::from_str(&contents).map_err(|e| e.to_string())?;
    Ok(value
        .get("started_at_ns")
        .and_then(Value::as_u64)
        .map(u128::from))
}

fn current_binary_modified_ns() -> Result<u128, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let modified = fs::metadata(&exe)
        .and_then(|metadata| metadata.modified())
        .map_err(|e| format!("failed to read mtime for {}: {}", exe.display(), e))?;
    system_time_to_unix_nanos(modified).ok_or_else(|| {
        format!(
            "failed to convert mtime for {} to unix timestamp",
            exe.display()
        )
    })
}

fn system_time_to_unix_nanos(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

pub(crate) fn sanitize_label(label: &str) -> String {
    let sanitized = label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "git".to_string()
    } else {
        trimmed.to_lowercase()
    }
}

pub(crate) fn format_status(status: Option<i32>) -> String {
    status
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unavailable".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    fn stdout_stderr_sleep_command() -> (&'static str, Vec<&'static str>) {
        (
            "sh",
            vec!["-c", "printf out; printf err >&2; exec sleep 60"],
        )
    }

    #[cfg(windows)]
    fn stdout_stderr_sleep_command() -> (&'static str, Vec<&'static str>) {
        (
            "powershell.exe",
            vec![
                "-NoProfile",
                "-Command",
                "[Console]::Out.Write('out'); [Console]::Error.Write('err'); Start-Sleep -Seconds 60",
            ],
        )
    }

    #[test]
    fn format_command_preserves_posix_shell_words() {
        assert_eq!(format_command("g", &["a b", "x'y"]), "g 'a b' 'x'\\''y'");
    }

    #[test]
    fn test_run_logged_command_with_timeout_reports_partial_output() {
        let (program, args) = stdout_stderr_sleep_command();
        let record =
            run_logged_command_with_timeout(program, &args, None, Duration::from_millis(300));

        assert!(record.timed_out, "{record:?}");
        assert_eq!(record.stdout, "out");
        assert!(record.stderr.contains("timed out after"), "{record:?}");
        assert!(
            record.stderr.contains("sent kill to child process")
                || record.stderr.contains("failed to kill child process"),
            "{record:?}"
        );
        assert!(
            record.stderr.contains("stderr before timeout"),
            "{record:?}"
        );
        assert!(record.stderr.contains("err"), "{record:?}");
    }
}
