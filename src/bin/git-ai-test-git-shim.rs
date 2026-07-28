use git_ai::daemon::test_sync::{
    TEST_SYNC_SESSION_CONFIG_KEY, tracked_parsed_git_invocation_for_test_sync,
    tracks_parsed_git_invocation_for_test_sync,
};
use serde::Serialize;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
#[cfg(not(unix))]
use std::process::Stdio;
use std::time::Duration;

#[derive(Serialize)]
struct StartedGitInvocationLogEntry {
    command: Option<String>,
    command_args: Vec<String>,
    cwd: Option<String>,
    test_sync_session: Option<String>,
}

fn select_target(argv: &[String]) -> Result<String, String> {
    let tracked_target = env::var("GIT_AI_TEST_GIT_SHIM_TARGET")
        .map_err(|_| "GIT_AI_TEST_GIT_SHIM_TARGET is required".to_string())?;
    let fallback_target =
        env::var("GIT_AI_TEST_GIT_SHIM_FALLBACK_TARGET").unwrap_or_else(|_| tracked_target.clone());
    let cwd = env::current_dir().map_err(|e| format!("read shim cwd failed: {e}"))?;
    let parsed = tracked_parsed_git_invocation_for_test_sync(argv, &cwd);
    if tracks_parsed_git_invocation_for_test_sync(&parsed) {
        Ok(tracked_target)
    } else {
        Ok(fallback_target)
    }
}

fn append_started_log(
    log_path: &PathBuf,
    argv: &[String],
    test_sync_session: Option<&str>,
) -> Result<(), String> {
    let cwd = env::current_dir().map_err(|e| format!("read shim cwd failed: {e}"))?;
    let parsed = tracked_parsed_git_invocation_for_test_sync(argv, &cwd);
    if !tracks_parsed_git_invocation_for_test_sync(&parsed) {
        return Ok(());
    }

    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create shim log dir failed: {e}"))?;
    }

    let entry = StartedGitInvocationLogEntry {
        command: parsed.command.clone(),
        command_args: parsed.command_args.clone(),
        cwd: Some(cwd.to_string_lossy().to_string()),
        test_sync_session: test_sync_session.map(str::to_string),
    };
    let mut line = serde_json::to_vec(&entry).map_err(|e| format!("serialize shim log: {e}"))?;
    line.push(b'\n');

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| format!("open shim log failed: {e}"))?;
    file.write_all(&line)
        .map_err(|e| format!("write shim log failed: {e}"))?;
    file.flush()
        .map_err(|e| format!("flush shim log failed: {e}"))?;
    Ok(())
}

fn new_test_sync_session() -> String {
    format!("gt-shim-{}", git_ai::uuid::generate_v4())
}

fn delay_for_test() {
    let Ok(delay_ms) = env::var("GIT_AI_TEST_GIT_SHIM_DELAY_MS") else {
        return;
    };
    let Ok(delay_ms) = delay_ms.parse::<u64>() else {
        return;
    };
    std::thread::sleep(Duration::from_millis(delay_ms));
}

fn argv_with_test_sync_session(argv: &[String], test_sync_session: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len() + 2);
    out.push("-c".to_string());
    out.push(format!(
        "{}={}",
        TEST_SYNC_SESSION_CONFIG_KEY, test_sync_session
    ));
    out.extend(argv.iter().cloned());
    out
}

#[cfg(unix)]
fn exec_target(target: &str, argv: &[String]) -> ! {
    let mut command = Command::new(target);
    command.args(argv);
    let error = command.exec();
    eprintln!("git-ai-test-git-shim failed to exec {target}: {error}");
    std::process::exit(127);
}

#[cfg(not(unix))]
fn exec_target(target: &str, argv: &[String]) -> ! {
    let mut command = Command::new(target);
    command
        .args(argv)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    match command.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("git-ai-test-git-shim failed to spawn {target}: {error}");
            std::process::exit(127);
        }
    }
}

#[cfg(unix)]
fn main() {
    let argv = env::args().skip(1).collect::<Vec<_>>();
    delay_for_test();
    let target = select_target(&argv).unwrap_or_else(|error| panic!("{error}"));
    let mut effective_argv = argv.clone();
    let mut test_sync_session = None;
    if let Ok(log_path) = env::var("GIT_AI_TEST_SYNC_START_LOG") {
        let log_path = PathBuf::from(log_path);
        let cwd =
            env::current_dir().unwrap_or_else(|error| panic!("read shim cwd failed: {error}"));
        let parsed = tracked_parsed_git_invocation_for_test_sync(&argv, &cwd);
        if tracks_parsed_git_invocation_for_test_sync(&parsed) {
            test_sync_session = Some(new_test_sync_session());
            if let Some(session) = test_sync_session.as_deref() {
                effective_argv = argv_with_test_sync_session(&argv, session);
            }
        }
        if let Err(error) = append_started_log(&log_path, &argv, test_sync_session.as_deref()) {
            panic!("git-ai-test-git-shim failed: {error}");
        }
    }
    exec_target(&target, &effective_argv);
}

#[cfg(not(unix))]
fn main() {
    let argv = env::args().skip(1).collect::<Vec<_>>();
    delay_for_test();
    let target = select_target(&argv).unwrap_or_else(|error| panic!("{error}"));
    let mut effective_argv = argv.clone();
    let mut test_sync_session = None;
    if let Ok(log_path) = env::var("GIT_AI_TEST_SYNC_START_LOG") {
        let log_path = PathBuf::from(log_path);
        let cwd =
            env::current_dir().unwrap_or_else(|error| panic!("read shim cwd failed: {error}"));
        let parsed = tracked_parsed_git_invocation_for_test_sync(&argv, &cwd);
        if tracks_parsed_git_invocation_for_test_sync(&parsed) {
            test_sync_session = Some(new_test_sync_session());
            if let Some(session) = test_sync_session.as_deref() {
                effective_argv = argv_with_test_sync_session(&argv, session);
            }
        }
        if let Err(error) = append_started_log(&log_path, &argv, test_sync_session.as_deref()) {
            panic!("git-ai-test-git-shim failed: {error}");
        }
    }
    exec_target(&target, &effective_argv)
}
