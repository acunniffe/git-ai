//! The git spawn layer: thin wrappers around `std::process::Command` that
//! execute the real `git` binary for internal git-ai operations. All internal
//! git subprocesses go through these helpers so hook suppression, machine-parse
//! profiles, and trace2 env scrubbing are applied consistently.

use crate::config;
use crate::error::GitAiError;
use std::ffi::OsStr;
use std::process::{Child, Command, Output};

#[cfg(windows)]
use crate::process_spawn::CREATE_NO_WINDOW;
#[cfg(windows)]
use crate::process_spawn::is_interactive_terminal;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

pub mod profile;

pub use profile::{
    InternalGitHooksGuard, InternalGitProfile, args_with_disabled_hooks_if_needed,
    args_with_internal_git_profile, disable_internal_git_hooks,
};

pub fn exec_git(args: &[String]) -> Result<Output, GitAiError> {
    exec_git_with_profile(args, InternalGitProfile::General)
}

/// Helper to execute a git command and return output regardless of exit status.
/// Callers that need success-only behavior should use `exec_git*`.
pub fn exec_git_allow_nonzero(args: &[String]) -> Result<Output, GitAiError> {
    exec_git_allow_nonzero_with_profile(args, InternalGitProfile::General)
}

/// Helper to execute a git command with an explicit internal profile and return output
/// regardless of exit status.
pub fn exec_git_allow_nonzero_with_profile(
    args: &[String],
    profile: InternalGitProfile,
) -> Result<Output, GitAiError> {
    exec_git_allow_nonzero_with_profile_and_env(args, profile, &[])
}

pub fn exec_git_allow_nonzero_with_env(
    args: &[String],
    envs: &[(&str, &OsStr)],
) -> Result<Output, GitAiError> {
    exec_git_allow_nonzero_with_profile_and_env(args, InternalGitProfile::General, envs)
}

#[cfg(feature = "test-support")]
fn spawn_probe_log(effective_args: &[String]) {
    let Ok(path) = std::env::var("GIT_AI_SPAWN_LOG") else {
        return;
    };
    let sub = effective_args
        .iter()
        .find(|a| !a.starts_with('-') && !a.contains('=') && !a.contains('/') && !a.contains('\\'))
        .cloned()
        .unwrap_or_default();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", sub);
    }
}

#[cfg(not(feature = "test-support"))]
#[inline]
fn spawn_probe_log(_effective_args: &[String]) {}

fn exec_git_allow_nonzero_with_profile_and_env(
    args: &[String],
    profile: InternalGitProfile,
    envs: &[(&str, &OsStr)],
) -> Result<Output, GitAiError> {
    let effective_args =
        args_with_internal_git_profile(&args_with_disabled_hooks_if_needed(args), profile);
    spawn_probe_log(&effective_args);
    let mut cmd = Command::new(config::Config::get().git_cmd());
    cmd.args(&effective_args);
    apply_internal_git_env(&mut cmd);
    for (key, value) in envs {
        cmd.env(key, value);
    }

    #[cfg(windows)]
    {
        if !is_interactive_terminal() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }

    cmd.output().map_err(GitAiError::IoError)
}

/// Spawn a git command with stdout piped and stderr inherited.
///
/// This is used by streaming consumers that cannot call `exec_git*` without
/// buffering all stdout in memory.
pub fn spawn_git_stdout(args: &[String]) -> Result<Child, GitAiError> {
    let effective_args = args_with_internal_git_profile(
        &args_with_disabled_hooks_if_needed(args),
        InternalGitProfile::General,
    );
    let mut cmd = Command::new(config::Config::get().git_cmd());
    cmd.args(&effective_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    apply_internal_git_env(&mut cmd);

    #[cfg(windows)]
    {
        if !is_interactive_terminal() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }

    cmd.spawn().map_err(GitAiError::IoError)
}

/// Spawn a git command with stdin/stdout/stderr inherited from git-ai.
///
/// This is used when a command intentionally delegates rendering and paging
/// behavior to git instead of consuming output internally.
pub fn spawn_git_passthrough(args: &[String]) -> Result<Child, GitAiError> {
    let effective_args = args_with_internal_git_profile(
        &args_with_disabled_hooks_if_needed(args),
        InternalGitProfile::General,
    );
    let mut cmd = Command::new(config::Config::get().git_cmd());
    cmd.args(&effective_args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    apply_internal_git_env(&mut cmd);

    #[cfg(windows)]
    {
        if !is_interactive_terminal() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }

    cmd.spawn().map_err(GitAiError::IoError)
}

pub(crate) const INTERNAL_GIT_ENV_REMOVE: &[&str] = &[
    "GIT_EXTERNAL_DIFF",
    "GIT_DIFF_OPTS",
    "GIT_TRACE",
    "GIT_TRACE2_BRIEF",
    "GIT_TRACE2_CONFIG_PARAMS",
    "GIT_TRACE2_ENV_VARS",
    "GIT_TRACE2_EVENT_NESTING",
    "GIT_TRACE2_PARENT_NAME",
    "GIT_TRACE2_PARENT_SID",
];

pub(crate) const INTERNAL_GIT_ENV_SET: &[(&str, &str)] = &[
    ("GIT_TRACE2", "0"),
    ("GIT_TRACE2_EVENT", "0"),
    ("GIT_TRACE2_PERF", "0"),
];

pub(crate) fn apply_internal_git_env(cmd: &mut Command) {
    for key in INTERNAL_GIT_ENV_REMOVE {
        cmd.env_remove(key);
    }
    for (key, value) in INTERNAL_GIT_ENV_SET {
        cmd.env(key, value);
    }
}

/// Build a `GitCliError` from an exit status code, stderr bytes, and the
/// effective argument list.  Consolidates the three call sites that previously
/// duplicated this construction inline.
fn git_cli_error(code: Option<i32>, stderr: &[u8], args: Vec<String>) -> GitAiError {
    GitAiError::GitCliError {
        code,
        stderr: String::from_utf8_lossy(stderr).to_string(),
        args,
    }
}

/// Helper to execute a git command with an explicit internal profile.
pub fn exec_git_with_profile(
    args: &[String],
    profile: InternalGitProfile,
) -> Result<Output, GitAiError> {
    let effective_args =
        args_with_internal_git_profile(&args_with_disabled_hooks_if_needed(args), profile);
    let output = exec_git_allow_nonzero_with_profile(args, profile)?;

    if !output.status.success() {
        return Err(git_cli_error(
            output.status.code(),
            &output.stderr,
            effective_args,
        ));
    }

    Ok(output)
}

/// Helper to execute a git command with data provided on stdin
pub fn exec_git_stdin(args: &[String], stdin_data: &[u8]) -> Result<Output, GitAiError> {
    exec_git_stdin_with_profile(args, stdin_data, InternalGitProfile::General)
}

/// Spawn a fully-piped git child for `effective_args` and start a thread
/// writing `stdin_data` to it. Writing stdin in a separate thread avoids
/// deadlock: if we wrote all stdin before reading stdout, the child's stdout
/// pipe buffer could fill up, causing the child to block on write, which
/// prevents it from consuming more stdin, which would block our write_all.
type StdinWriterHandle = std::thread::JoinHandle<std::io::Result<()>>;

fn spawn_git_piped(effective_args: &[String]) -> Result<Child, GitAiError> {
    spawn_probe_log(effective_args);
    let mut cmd = Command::new(config::Config::get().git_cmd());
    cmd.args(effective_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    apply_internal_git_env(&mut cmd);

    #[cfg(windows)]
    {
        if !is_interactive_terminal() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }

    cmd.spawn().map_err(GitAiError::IoError)
}

fn spawn_git_stdin_piped(
    effective_args: &[String],
    stdin_data: &[u8],
) -> Result<(Child, Option<StdinWriterHandle>), GitAiError> {
    let mut child = spawn_git_piped(effective_args)?;
    let stdin_handle = child.stdin.take().map(|mut stdin| {
        let data = stdin_data.to_vec();
        std::thread::spawn(move || {
            use std::io::Write;
            stdin.write_all(&data)
        })
    });

    Ok((child, stdin_handle))
}

/// Execute a git command while producing its stdin incrementally.
///
/// Unlike [`exec_git_stdin`], the payload is never materialized as one
/// buffer: the producer runs on a scoped thread (so it may borrow
/// caller-owned data) and writes through a bounded `BufWriter` while stdout
/// and stderr are drained concurrently by `wait_with_output`.
pub fn exec_git_with_stdin_writer(
    args: &[String],
    write_stdin: impl FnOnce(&mut dyn std::io::Write) -> std::io::Result<()> + Send,
) -> Result<Output, GitAiError> {
    let effective_args = args_with_internal_git_profile(
        &args_with_disabled_hooks_if_needed(args),
        InternalGitProfile::General,
    );
    let mut child = spawn_git_piped(&effective_args)?;
    let stdin = child.stdin.take().expect("git child stdin is piped");

    let (output, stdin_result) = std::thread::scope(|scope| {
        let stdin_handle = scope.spawn(move || {
            use std::io::Write;

            let mut stdin = std::io::BufWriter::new(stdin);
            write_stdin(&mut stdin)?;
            stdin.flush()
        });
        let output = child.wait_with_output();
        let stdin_result = stdin_handle.join().expect("stdin writer thread panicked");
        (output, stdin_result)
    });
    let output = output.map_err(GitAiError::IoError)?;

    if let Err(e) = stdin_result
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(GitAiError::IoError(e));
    }

    if !output.status.success() {
        return Err(git_cli_error(
            output.status.code(),
            &output.stderr,
            effective_args,
        ));
    }

    Ok(output)
}

/// Like `exec_git_stdin`, but streams the child's stdout to `on_line` one line
/// at a time instead of buffering the entire output in memory. Use this for
/// commands whose output can be arbitrarily large (e.g. batched
/// `diff-tree --stdin -p`), where `wait_with_output()` would hold the full
/// output (plus a lossy-conversion copy) in memory at once.
///
/// Each line is lossily UTF-8 converted individually and passed without its
/// trailing `\n` (and at most one preceding `\r`), matching `str::lines()`.
pub fn exec_git_stdin_streaming(
    args: &[String],
    stdin_data: &[u8],
    mut on_line: impl FnMut(&str),
) -> Result<(), GitAiError> {
    use std::io::BufRead;

    let effective_args = args_with_internal_git_profile(
        &args_with_disabled_hooks_if_needed(args),
        InternalGitProfile::General,
    );
    let (mut child, stdin_handle) = spawn_git_stdin_piped(&effective_args, stdin_data)?;

    // Drain stderr concurrently so the child can never block on a full stderr
    // pipe while we are still reading stdout.
    let stderr_handle = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        })
    });

    let stdout = child.stdout.take().expect("child stdout is piped");
    let mut reader = std::io::BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::new();
    let read_result = loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break Ok(()),
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                    if buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                }
                on_line(&String::from_utf8_lossy(&buf));
            }
            Err(e) => break Err(e),
        }
    };
    if let Err(e) = read_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(GitAiError::IoError(e));
    }

    let status = child.wait().map_err(GitAiError::IoError)?;

    if let Some(handle) = stdin_handle
        && let Err(e) = handle.join().expect("stdin writer thread panicked")
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(GitAiError::IoError(e));
    }

    if !status.success() {
        let stderr_bytes = stderr_handle
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        return Err(git_cli_error(status.code(), &stderr_bytes, effective_args));
    }

    Ok(())
}

/// Helper to execute a git command with data provided on stdin and an explicit profile.
pub fn exec_git_stdin_with_profile(
    args: &[String],
    stdin_data: &[u8],
    profile: InternalGitProfile,
) -> Result<Output, GitAiError> {
    // TODO Make sure to handle process signals, etc.
    let effective_args =
        args_with_internal_git_profile(&args_with_disabled_hooks_if_needed(args), profile);
    let (child, stdin_handle) = spawn_git_stdin_piped(&effective_args, stdin_data)?;

    let output = child.wait_with_output().map_err(GitAiError::IoError)?;

    if let Some(handle) = stdin_handle
        && let Err(e) = handle.join().expect("stdin writer thread panicked")
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(GitAiError::IoError(e));
    }

    if !output.status.success() {
        return Err(git_cli_error(
            output.status.code(),
            &output.stderr,
            effective_args,
        ));
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn explicit_command_env(cmd: &Command, key: &str) -> Option<Option<String>> {
        cmd.get_envs()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.map(|v| v.to_string_lossy().to_string()))
    }

    #[test]
    fn internal_git_env_disables_trace2_targets() {
        let mut cmd = Command::new("git");
        for key in INTERNAL_GIT_ENV_REMOVE {
            cmd.env(key, "inherited");
        }
        for (key, _) in INTERNAL_GIT_ENV_SET {
            cmd.env(key, "inherited");
        }

        apply_internal_git_env(&mut cmd);

        for key in INTERNAL_GIT_ENV_REMOVE {
            assert_eq!(explicit_command_env(&cmd, key), Some(None));
        }
        for (key, value) in INTERNAL_GIT_ENV_SET {
            assert_eq!(
                explicit_command_env(&cmd, key),
                Some(Some((*value).to_string()))
            );
        }
    }
}
