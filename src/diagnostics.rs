use crate::authorship::working_log::CheckpointKind;
use crate::commands::blame::{BlameAnalysisResult, GitAiBlameOptions};
use crate::config::Config;
use crate::daemon::control_api::{ControlRequest, FamilyStatus};
use crate::diagnostic_sentinels::{
    DEBUG_SELF_CHECK_REMOTE_URL, debug_self_check_root, path_is_in_debug_self_check_root,
};
use crate::git::repository::discover_repository_in_path_no_git_exec;
use crate::process_timeout::run_command_with_timeout;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SELF_CHECK_FILE: &str = "git-ai-debug-self-check.txt";
const SELF_CHECK_CONTENT_UNTRACKED: &str = "Untracked line\n";
const SELF_CHECK_CONTENT_KNOWN_HUMAN: &str = "Untracked line\nKnown human line\n";
const SELF_CHECK_CONTENT_AI: &str = "Untracked line\nKnown human line\nAI line\n";
const TRACE2_EVENT_TARGET_KEY: &str = "trace2.eventTarget";
const TRACE2_EVENT_NESTING_KEY: &str = "trace2.eventNesting";
const TRACE2_EVENT_NESTING_VALUE: &str = "0";
const SELF_CHECK_TRACE_ENV_REMOVE: &[&str] = &[
    "GIT_TRACE2_PARENT_SID",
    "GIT_TRACE2_PARENT_NAME",
    "GIT_AI_WRAPPER_INVOCATION_ID",
    "GIT_TRACE2_ENV_VARS",
];
const DEBUG_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const DAEMON_CONTROL_TIMEOUT: Duration = Duration::from_millis(500);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

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
    fn success(&self) -> bool {
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
    fn passed(
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

    fn with_trace2_json(mut self, trace2_json: Option<String>) -> Self {
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

    let config = match crate::daemon::DaemonConfig::from_env_or_default_paths() {
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

    let initially_up = crate::commands::daemon::daemon_is_up(&config);
    details.push(format!("initial daemon running: {}", initially_up));

    let mut restarted = false;
    if initially_up && daemon_binary_is_stale(&config).unwrap_or(false) {
        details.push("running daemon was started before the current git-ai binary was written; restarting daemon".to_string());
        if let Err(err) = crate::commands::daemon::restart_daemon(&config) {
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
        if let Err(err) = crate::commands::daemon::ensure_daemon_running(DEBUG_CHECK_TIMEOUT) {
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
            if let Err(restart_err) = crate::commands::daemon::restart_daemon(&config) {
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

pub fn check_trace2_global_config(target: &GitDiagnosticTarget) -> DiagnosticCheckResult {
    let mut commands = Vec::new();
    let expected_target = match crate::daemon::DaemonConfig::from_env_or_default_paths() {
        Ok(config) => config.trace2_event_target(),
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "trace2 global config could not be inspected",
                trace2_config_failure_details(
                    &format!("failed to determine expected trace2 target: {}", err),
                    None,
                    None,
                    None,
                ),
                commands,
            );
        }
    };

    let event_targets =
        read_global_git_config_values(&mut commands, &target.program, TRACE2_EVENT_TARGET_KEY);
    let event_nesting =
        read_global_git_config_values(&mut commands, &target.program, TRACE2_EVENT_NESTING_KEY);

    let event_targets = match event_targets {
        Ok(values) => values,
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "trace2 global config could not be inspected",
                trace2_config_failure_details(&err, Some(&expected_target), None, None),
                commands,
            );
        }
    };
    let event_nesting = match event_nesting {
        Ok(values) => values,
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "trace2 global config could not be inspected",
                trace2_config_failure_details(
                    &err,
                    Some(&expected_target),
                    Some(&event_targets),
                    None,
                ),
                commands,
            );
        }
    };

    let target_matches = event_targets.iter().any(|value| value == &expected_target);
    let nesting_matches = event_nesting
        .iter()
        .any(|value| value == TRACE2_EVENT_NESTING_VALUE);

    if target_matches && nesting_matches {
        return DiagnosticCheckResult::passed(
            "trace2 global config is configured",
            vec![
                format!("{}: {}", TRACE2_EVENT_TARGET_KEY, expected_target),
                format!(
                    "{}: {}",
                    TRACE2_EVENT_NESTING_KEY, TRACE2_EVENT_NESTING_VALUE
                ),
            ],
            commands,
        );
    }

    DiagnosticCheckResult::failed(
        "trace2 global config is not configured",
        trace2_config_failure_details(
            "trace2 is not configured for git-ai daemon mode",
            Some(&expected_target),
            Some(&event_targets),
            Some(&event_nesting),
        ),
        commands,
    )
}

pub fn run_attribution_self_check(target: &GitDiagnosticTarget) -> DiagnosticCheckResult {
    let mut commands = Vec::new();
    let deadline = Instant::now() + DEBUG_CHECK_TIMEOUT;
    let repo_path = debug_self_check_root().join(format!(
        "{}-{}",
        sanitize_label(&target.label),
        crate::uuid::generate_v4()
    ));
    let file_path = repo_path.join(SELF_CHECK_FILE);

    let result = (|| -> Result<Vec<String>, String> {
        fs::create_dir_all(&repo_path)
            .map_err(|e| format!("failed to create {}: {}", repo_path.display(), e))?;

        run_required_until(
            &mut commands,
            &target.program,
            &["init", "."],
            Some(&repo_path),
            deadline,
        )?;
        run_required_until(
            &mut commands,
            &target.program,
            &["config", "user.name", "Git AI Debug"],
            Some(&repo_path),
            deadline,
        )?;
        run_required_until(
            &mut commands,
            &target.program,
            &["config", "user.email", "debug-self-check@git-ai.invalid"],
            Some(&repo_path),
            deadline,
        )?;
        run_required_until(
            &mut commands,
            &target.program,
            &["remote", "add", "origin", DEBUG_SELF_CHECK_REMOTE_URL],
            Some(&repo_path),
            deadline,
        )?;

        fs::write(&file_path, SELF_CHECK_CONTENT_UNTRACKED)
            .map_err(|e| format!("failed to write {}: {}", file_path.display(), e))?;
        run_git_ai_checkpoint(&mut commands, &repo_path, "human", deadline)?;
        wait_for_checkpoint_count(&repo_path, 1, deadline)?;

        fs::write(&file_path, SELF_CHECK_CONTENT_KNOWN_HUMAN)
            .map_err(|e| format!("failed to write {}: {}", file_path.display(), e))?;
        run_git_ai_checkpoint(&mut commands, &repo_path, "mock_known_human", deadline)?;
        wait_for_checkpoint_count(&repo_path, 2, deadline)?;

        fs::write(&file_path, SELF_CHECK_CONTENT_AI)
            .map_err(|e| format!("failed to write {}: {}", file_path.display(), e))?;
        run_git_ai_checkpoint(&mut commands, &repo_path, "mock_ai", deadline)?;
        wait_for_checkpoint_count(&repo_path, 3, deadline)?;

        run_required_until(
            &mut commands,
            &target.program,
            &["add", SELF_CHECK_FILE],
            Some(&repo_path),
            deadline,
        )?;
        run_required_until(
            &mut commands,
            &target.program,
            &["commit", "-m", "git-ai debug self check"],
            Some(&repo_path),
            deadline,
        )?;

        let commit_sha = run_required_until(
            &mut commands,
            &target.program,
            &["rev-parse", "HEAD"],
            Some(&repo_path),
            deadline,
        )?
        .stdout
        .trim()
        .to_string();

        // Attribution is produced asynchronously after the commit. Give that
        // phase its own bounded budget instead of sharing the setup deadline.
        let attribution_deadline = Instant::now() + DEBUG_CHECK_TIMEOUT;
        let mut details =
            poll_self_check_attribution(&repo_path, &commit_sha, attribution_deadline)?;
        details.insert(0, format!("repo: {}", repo_path.display()));
        details.insert(1, format!("commit: {}", commit_sha));
        details.insert(
            2,
            format!("notes backend: {}", Config::get().notes_backend_kind()),
        );
        Ok(details)
    })();

    match result {
        Ok(details) => {
            let _ = fs::remove_dir_all(&repo_path);
            DiagnosticCheckResult::passed("attribution self-check completed", details, commands)
        }
        Err(err) => {
            let mut details = vec![format!("repo: {}", repo_path.display()), err];
            details.push(daemon_family_status_detail(&repo_path));
            if path_is_in_debug_self_check_root(&repo_path) {
                details.push(
                    "failed self-check repository was left in place for inspection".to_string(),
                );
            }
            DiagnosticCheckResult::failed("attribution self-check failed", details, commands)
        }
    }
}

pub fn run_trace2_file_self_check(target: &GitDiagnosticTarget) -> DiagnosticCheckResult {
    let mut commands = Vec::new();
    let deadline = Instant::now() + DEBUG_CHECK_TIMEOUT;
    let trace_dir = crate::mdm::utils::home_dir()
        .join(".git-ai")
        .join("internal")
        .join("daemon");
    let trace_path = trace_dir.join(format!(
        "trace2-debug-check-{}-{}.json",
        sanitize_label(&target.label),
        crate::uuid::generate_v4()
    ));
    let trace_command_dir = debug_self_check_root().join(format!(
        "trace2-{}-{}",
        sanitize_label(&target.label),
        crate::uuid::generate_v4()
    ));

    let snapshot = match snapshot_global_trace2_event_target(&mut commands, &target.program) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "trace2 file self-check failed",
                vec![err],
                commands,
            );
        }
    };

    let mut changed_global_event_target = false;
    let result = (|| -> Result<(Vec<String>, String), String> {
        fs::create_dir_all(&trace_dir)
            .map_err(|e| format!("failed to create {}: {}", trace_dir.display(), e))?;
        fs::create_dir_all(&trace_command_dir)
            .map_err(|e| format!("failed to create {}: {}", trace_command_dir.display(), e))?;
        let _ = fs::remove_file(&trace_path);
        let trace_path_string = trace_path.to_string_lossy().to_string();

        // This intentionally uses global git config rather than a process-local
        // GIT_TRACE2_EVENT override so the diagnostic exercises the install path.
        run_required_until(
            &mut commands,
            &target.program,
            &[
                "config",
                "--global",
                "--replace-all",
                TRACE2_EVENT_TARGET_KEY,
                trace_path_string.as_str(),
            ],
            None,
            deadline,
        )?;
        changed_global_event_target = true;

        // Use init rather than version: when terminal git is the git-ai proxy,
        // read-only commands intentionally suppress trace2 before invoking real git.
        run_required_until(
            &mut commands,
            &target.program,
            &["init", "."],
            Some(&trace_command_dir),
            deadline,
        )?;

        let trace2_json = fs::read_to_string(&trace_path)
            .map_err(|e| format!("failed to read {}: {}", trace_path.display(), e))?;
        let details = validate_trace2_command_events(&trace2_json, "init")?;
        Ok((details, trace2_json))
    })();

    let restore_result = if changed_global_event_target {
        restore_global_trace2_event_target(&mut commands, &target.program, &snapshot)
    } else {
        Ok(())
    };
    let _ = fs::remove_file(&trace_path);
    let _ = fs::remove_dir_all(&trace_command_dir);

    match (result, restore_result) {
        (Ok((mut details, trace2_json)), Ok(())) => {
            details.insert(0, format!("trace2 file: {}", trace_path.display()));
            details.insert(1, format!("command dir: {}", trace_command_dir.display()));
            DiagnosticCheckResult::passed("trace2 file self-check completed", details, commands)
                .with_trace2_json(Some(trace2_json))
        }
        (Ok((mut details, trace2_json)), Err(restore_err)) => {
            details.insert(0, format!("trace2 file: {}", trace_path.display()));
            details.insert(1, format!("command dir: {}", trace_command_dir.display()));
            details.push(format!("restore failed: {}", restore_err));
            DiagnosticCheckResult::failed("trace2 file self-check failed", details, commands)
                .with_trace2_json(Some(trace2_json))
        }
        (Err(err), Ok(())) => DiagnosticCheckResult::failed(
            "trace2 file self-check failed",
            vec![
                format!("trace2 file: {}", trace_path.display()),
                format!("command dir: {}", trace_command_dir.display()),
                err,
            ],
            commands,
        ),
        (Err(err), Err(restore_err)) => DiagnosticCheckResult::failed(
            "trace2 file self-check failed",
            vec![
                format!("trace2 file: {}", trace_path.display()),
                format!("command dir: {}", trace_command_dir.display()),
                err,
                format!("restore failed: {}", restore_err),
            ],
            commands,
        ),
    }
}

fn run_daemon_trace2_ingestion_probe(
    commands: &mut Vec<CommandRecord>,
    git_program: &str,
    config: &crate::daemon::DaemonConfig,
    deadline: Instant,
) -> Result<Vec<String>, String> {
    let probe_path =
        debug_self_check_root().join(format!("daemon-probe-{}", crate::uuid::generate_v4()));

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

        let status = wait_for_daemon_family_status(config, &probe_path, 1, deadline)?;
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

fn run_git_ai_checkpoint(
    commands: &mut Vec<CommandRecord>,
    repo_path: &Path,
    preset: &str,
    deadline: Instant,
) -> Result<CommandRecord, String> {
    let git_ai = std::env::current_exe()
        .map_err(|e| format!("failed to resolve git-ai binary path: {}", e))?;
    let git_ai = git_ai.to_string_lossy().to_string();
    run_required_until(
        commands,
        &git_ai,
        &["checkpoint", preset, SELF_CHECK_FILE],
        Some(repo_path),
        deadline,
    )
}

fn run_required_until(
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

fn run_logged_command(program: &str, args: &[&str], cwd: Option<&Path>) -> CommandRecord {
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

fn remaining_timeout(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn wait_for_checkpoint_count(
    repo_path: &Path,
    expected_min_count: usize,
    deadline: Instant,
) -> Result<(), String> {
    let start = Instant::now();
    let mut last_error = None;

    while Instant::now() < deadline {
        match read_checkpoint_count(repo_path) {
            Ok(count) if count >= expected_min_count => return Ok(()),
            Ok(count) => {
                last_error = Some(format!(
                    "only {} checkpoint(s) visible, expected at least {}",
                    count, expected_min_count
                ));
            }
            Err(e) => last_error = Some(e),
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    Err(format!(
        "timed out after {:.1}s waiting for checkpoint persistence: {}",
        start.elapsed().as_secs_f64(),
        last_error.unwrap_or_else(|| {
            format!(
                "no checkpoint status available for repo {}",
                repo_path.display()
            )
        })
    ))
}

fn read_checkpoint_count(repo_path: &Path) -> Result<usize, String> {
    let repo = discover_repository_in_path_no_git_exec(repo_path).map_err(|e| e.to_string())?;
    let working_log = repo
        .storage
        .working_log_for_base_commit("initial")
        .map_err(|e| e.to_string())?;
    working_log
        .read_all_checkpoints()
        .map(|checkpoints| checkpoints.len())
        .map_err(|e| e.to_string())
}

fn poll_self_check_attribution(
    repo_path: &Path,
    commit_sha: &str,
    deadline: Instant,
) -> Result<Vec<String>, String> {
    let start = Instant::now();
    let repo = discover_repository_in_path_no_git_exec(repo_path).map_err(|e| e.to_string())?;
    let notes_backend = Config::get().notes_backend_kind();
    let mut last_error = None;

    while Instant::now() < deadline {
        match validate_self_check_blame_analysis(
            repo.blame_analysis(SELF_CHECK_FILE, &self_check_blame_options(commit_sha))
                .map_err(|e| e.to_string()),
        ) {
            Ok(details) => return Ok(details),
            Err(err) => last_error = Some(err),
        }

        if remaining_timeout(deadline).is_zero() {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    Err(format!(
        "timed out after {:.1}s waiting for expected attribution via {} backend for {} in {}: {}",
        start.elapsed().as_secs_f64(),
        notes_backend,
        commit_sha,
        repo_path.display(),
        last_error.unwrap_or_else(|| "no blame analysis result available".to_string())
    ))
}

fn self_check_blame_options(commit_sha: &str) -> GitAiBlameOptions {
    GitAiBlameOptions {
        line_ranges: vec![(1, 3)],
        newest_commit: Some(commit_sha.to_string()),
        use_prompt_hashes_as_names: true,
        return_human_authors_as_human: true,
        ..GitAiBlameOptions::default()
    }
}

fn wait_for_daemon_family_status(
    config: &crate::daemon::DaemonConfig,
    repo_path: &Path,
    expected_min_seq: u64,
    deadline: Instant,
) -> Result<FamilyStatus, String> {
    let mut last_error = None;

    while Instant::now() < deadline {
        match read_daemon_family_status(config, repo_path) {
            Ok(status) if status.latest_seq >= expected_min_seq => return Ok(status),
            Ok(status) => {
                last_error = Some(format!(
                    "latest_seq={}, expected at least {}, last_error={}",
                    status.latest_seq,
                    expected_min_seq,
                    status.last_error.as_deref().unwrap_or("<none>")
                ));
            }
            Err(err) => last_error = Some(err),
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    Err(format!(
        "timed out waiting for daemon family status: {}",
        last_error.unwrap_or_else(|| format!("no status for {}", repo_path.display()))
    ))
}

fn read_daemon_family_status(
    config: &crate::daemon::DaemonConfig,
    repo_path: &Path,
) -> Result<FamilyStatus, String> {
    let request = ControlRequest::StatusFamily {
        repo_working_dir: repo_path.display().to_string(),
    };
    let response = crate::daemon::send_control_request_with_timeout(
        &config.control_socket_path,
        &request,
        DAEMON_CONTROL_TIMEOUT,
    )
    .map_err(|e| e.to_string())?;

    if !response.ok {
        return Err(response
            .error
            .unwrap_or_else(|| "daemon status request failed".to_string()));
    }

    let data = response
        .data
        .ok_or_else(|| "daemon status response had no data".to_string())?;
    serde_json::from_value::<FamilyStatus>(data).map_err(|e| e.to_string())
}

fn daemon_family_status_detail(repo_path: &Path) -> String {
    let config = match crate::daemon::DaemonConfig::from_env_or_default_paths() {
        Ok(config) => config,
        Err(err) => {
            return format!("daemon status for repo: <error: {}>", err);
        }
    };

    match read_daemon_family_status(&config, repo_path) {
        Ok(status) => format!(
            "daemon status for repo: latest_seq={}, last_error={}",
            status.latest_seq,
            status.last_error.as_deref().unwrap_or("<none>")
        ),
        Err(err) => format!("daemon status for repo: <error: {}>", err),
    }
}

fn daemon_binary_is_stale(config: &crate::daemon::DaemonConfig) -> Result<bool, String> {
    let Some(started_at_ns) = read_daemon_started_at_ns(config)? else {
        return Ok(false);
    };
    let binary_modified_ns = current_binary_modified_ns()?;
    Ok(binary_modified_ns > started_at_ns)
}

fn read_daemon_started_at_ns(config: &crate::daemon::DaemonConfig) -> Result<Option<u128>, String> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineClassification {
    Untracked,
    KnownHuman,
    Ai,
    Unknown,
}

impl LineClassification {
    fn as_str(self) -> &'static str {
        match self {
            LineClassification::Untracked => "untracked",
            LineClassification::KnownHuman => "known_human",
            LineClassification::Ai => "ai",
            LineClassification::Unknown => "unknown",
        }
    }
}

fn validate_self_check_blame_analysis(
    analysis: Result<BlameAnalysisResult, String>,
) -> Result<Vec<String>, String> {
    let analysis = analysis.map_err(|err| format!("blame analysis failed: {}", err))?;
    let expected = [
        (1, LineClassification::Untracked),
        (2, LineClassification::KnownHuman),
        (3, LineClassification::Ai),
    ];
    let mut details = Vec::new();

    for (line, expected_class) in expected {
        let actual = classify_line(&analysis, line);
        let raw_author = analysis
            .line_authors
            .get(&line)
            .cloned()
            .unwrap_or_else(|| "<missing>".to_string());
        details.push(format!(
            "line {}: {} (expected {}, raw={})",
            line,
            actual.as_str(),
            expected_class.as_str(),
            raw_author
        ));
        if actual != expected_class {
            return Err(format!(
                "unexpected attribution for line {}: got {}, expected {}\n{}",
                line,
                actual.as_str(),
                expected_class.as_str(),
                format_blame_analysis_debug(&analysis)
            ));
        }
    }

    details.push(format_blame_analysis_debug(&analysis));
    Ok(details)
}

fn classify_line(analysis: &BlameAnalysisResult, line: u32) -> LineClassification {
    let Some(author) = analysis.line_authors.get(&line) else {
        return LineClassification::Unknown;
    };

    if author == &CheckpointKind::Human.to_str() {
        return LineClassification::Untracked;
    }

    if author.starts_with("h_") && analysis.humans.contains_key(author) {
        return LineClassification::KnownHuman;
    }

    if analysis
        .prompt_records
        .get(author)
        .is_some_and(|prompt| prompt.agent_id.tool == "mock_ai")
    {
        return LineClassification::Ai;
    }

    LineClassification::Unknown
}

fn format_blame_analysis_debug(analysis: &BlameAnalysisResult) -> String {
    let mut prompt_keys = analysis.prompt_records.keys().cloned().collect::<Vec<_>>();
    prompt_keys.sort();
    let mut session_keys = analysis.session_records.keys().cloned().collect::<Vec<_>>();
    session_keys.sort();
    let mut human_keys = analysis.humans.keys().cloned().collect::<Vec<_>>();
    human_keys.sort();

    format!(
        "blame analysis: line_authors={:?}, prompt_keys={:?}, session_keys={:?}, human_keys={:?}",
        analysis.line_authors, prompt_keys, session_keys, human_keys
    )
}

#[derive(Debug, Clone, Default)]
struct Trace2EventTargetSnapshot {
    values: Vec<String>,
}

fn snapshot_global_trace2_event_target(
    commands: &mut Vec<CommandRecord>,
    git_program: &str,
) -> Result<Trace2EventTargetSnapshot, String> {
    let record = run_logged_command(
        git_program,
        &[
            "config",
            "--global",
            "--no-includes",
            "--get-all",
            TRACE2_EVENT_TARGET_KEY,
        ],
        None,
    );
    let snapshot = if record.success() {
        record
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    } else if record.status == Some(1) {
        Vec::new()
    } else {
        let err = format!(
            "failed to snapshot global {}: status={}, stderr={}",
            TRACE2_EVENT_TARGET_KEY,
            format_status(record.status),
            record.stderr
        );
        commands.push(record);
        return Err(err);
    };
    commands.push(record);
    Ok(Trace2EventTargetSnapshot { values: snapshot })
}

fn restore_global_trace2_event_target(
    commands: &mut Vec<CommandRecord>,
    git_program: &str,
    snapshot: &Trace2EventTargetSnapshot,
) -> Result<(), String> {
    let remove = run_logged_command(
        git_program,
        &["config", "--global", "--unset-all", TRACE2_EVENT_TARGET_KEY],
        None,
    );
    let remove_ok = remove.success() || remove.status == Some(5);
    let remove_error = if remove_ok {
        None
    } else {
        Some(format!(
            "failed to remove temporary {}: status={}, stderr={}",
            TRACE2_EVENT_TARGET_KEY,
            format_status(remove.status),
            remove.stderr
        ))
    };
    commands.push(remove);
    if let Some(error) = remove_error {
        return Err(error);
    }

    for value in &snapshot.values {
        let record = run_logged_command(
            git_program,
            &[
                "config",
                "--global",
                "--add",
                TRACE2_EVENT_TARGET_KEY,
                value,
            ],
            None,
        );
        let error = if record.success() {
            None
        } else {
            Some(format!(
                "failed to restore {}: status={}, stderr={}",
                TRACE2_EVENT_TARGET_KEY,
                format_status(record.status),
                record.stderr
            ))
        };
        commands.push(record);
        if let Some(error) = error {
            return Err(error);
        }
    }

    Ok(())
}

fn validate_trace2_command_events(
    trace2_json: &str,
    expected_command: &str,
) -> Result<Vec<String>, String> {
    let mut events = Vec::new();
    for (idx, line) in trace2_json.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("invalid trace2 JSON on line {}: {}", idx + 1, e))?;
        events.push(value);
    }

    if events.is_empty() {
        return Err("trace2 file was empty".to_string());
    }

    let has_version = events
        .iter()
        .any(|event| event.get("event").and_then(Value::as_str) == Some("version"));
    let has_start = events
        .iter()
        .any(|event| event.get("event").and_then(Value::as_str) == Some("start"));
    let has_cmd_name_expected = events.iter().any(|event| {
        event.get("event").and_then(Value::as_str) == Some("cmd_name")
            && event.get("name").and_then(Value::as_str) == Some(expected_command)
    });
    let has_exit_zero = events.iter().any(|event| {
        event.get("event").and_then(Value::as_str) == Some("exit")
            && event.get("code").and_then(Value::as_i64) == Some(0)
    });
    let has_atexit_zero = events.iter().any(|event| {
        event.get("event").and_then(Value::as_str) == Some("atexit")
            && event.get("code").and_then(Value::as_i64) == Some(0)
    });

    let failures = [
        (has_version, "missing version event"),
        (has_start, "missing start event"),
        (
            has_cmd_name_expected,
            "missing cmd_name event for expected command",
        ),
        (has_exit_zero, "missing exit event with code 0"),
        (has_atexit_zero, "missing atexit event with code 0"),
    ]
    .into_iter()
    .filter_map(|(ok, msg)| (!ok).then_some(msg))
    .collect::<Vec<_>>();

    if !failures.is_empty() {
        return Err(format!("unexpected trace2 events: {}", failures.join(", ")));
    }

    Ok(vec![
        format!("events: {}", events.len()),
        format!(
            "validated: version/start/cmd_name({})/exit(0)/atexit(0)",
            expected_command
        ),
    ])
}

fn read_global_git_config_values(
    commands: &mut Vec<CommandRecord>,
    git_program: &str,
    key: &str,
) -> Result<Vec<String>, String> {
    let record = run_logged_command(git_program, &["config", "--global", "--get-all", key], None);
    let values = if record.success() {
        record
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    } else if record.status == Some(1) {
        Vec::new()
    } else {
        let err = format!(
            "failed to read global {}: status={}, stderr={}",
            key,
            format_status(record.status),
            record.stderr
        );
        commands.push(record);
        return Err(err);
    };
    commands.push(record);
    Ok(values)
}

fn trace2_config_failure_details(
    reason: &str,
    expected_target: Option<&str>,
    actual_targets: Option<&[String]>,
    actual_nesting: Option<&[String]>,
) -> Vec<String> {
    let mut details = vec![
        format!("ERROR: {}", reason),
        "Why this matters: git-ai daemon mode relies on Git trace2 events to match real Git commands to checkpoint and authorship state; without this config, commit/rebase/merge attribution can be missed or delayed.".to_string(),
    ];

    if let Some(expected_target) = expected_target {
        details.push(format!(
            "Expected {}: {}",
            TRACE2_EVENT_TARGET_KEY, expected_target
        ));
    }
    if let Some(actual_targets) = actual_targets {
        details.push(format!(
            "Actual {}: {}",
            TRACE2_EVENT_TARGET_KEY,
            format_config_values(actual_targets)
        ));
    }
    details.push(format!(
        "Expected {}: {}",
        TRACE2_EVENT_NESTING_KEY, TRACE2_EVENT_NESTING_VALUE
    ));
    if let Some(actual_nesting) = actual_nesting {
        details.push(format!(
            "Actual {}: {}",
            TRACE2_EVENT_NESTING_KEY,
            format_config_values(actual_nesting)
        ));
    }

    details.push("Common causes: `git-ai install-hooks` has not run, was run with `--dry-run`, or failed while writing global Git config.".to_string());
    details.push("Common causes: git-ai cannot edit the same global Git config Git reads because HOME/USERPROFILE/XDG_CONFIG_HOME/GIT_CONFIG_GLOBAL points somewhere different, the global config file or parent directory is read-only or locked, permissions or ownership are wrong, or the configured git and terminal git use different config locations.".to_string());
    details
}

fn format_config_values(values: &[String]) -> String {
    if values.is_empty() {
        "<missing>".to_string()
    } else {
        values.join(", ")
    }
}

fn sanitize_label(label: &str) -> String {
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

fn format_command(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .map(shell_quote_for_display)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote_for_display(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-_./:=@".contains(ch))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn format_status(status: Option<i32>) -> String {
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
    fn test_validate_trace2_command_events_accepts_expected_events() {
        let trace = r#"{"event":"version"}
{"event":"start","argv":["git","init","."]}
{"event":"cmd_name","name":"init"}
{"event":"exit","code":0}
{"event":"atexit","code":0}
"#;

        let details = validate_trace2_command_events(trace, "init").unwrap();
        assert!(details.iter().any(|detail| detail == "events: 5"));
    }

    #[test]
    fn test_validate_trace2_command_events_rejects_missing_cmd_name() {
        let trace = r#"{"event":"version"}
{"event":"start","argv":["git","init","."]}
{"event":"exit","code":0}
{"event":"atexit","code":0}
"#;

        let err = validate_trace2_command_events(trace, "init").unwrap_err();
        assert!(err.contains("missing cmd_name event for expected command"));
    }

    #[test]
    fn test_trace2_config_failure_details_explains_missing_config() {
        let empty = Vec::new();
        let details = trace2_config_failure_details(
            "trace2 is not configured for git-ai daemon mode",
            Some("af_unix:stream:/tmp/git-ai-trace2.sock"),
            Some(&empty),
            Some(&empty),
        );

        assert!(details[0].contains("ERROR: trace2 is not configured"));
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("Why this matters"))
        );
        assert!(
            details
                .iter()
                .any(|detail| detail == "Actual trace2.eventTarget: <missing>")
        );
        assert!(
            details
                .iter()
                .any(|detail| detail == "Actual trace2.eventNesting: <missing>")
        );
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("Common causes"))
        );
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
