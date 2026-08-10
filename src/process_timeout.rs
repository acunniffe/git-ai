use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(200);
const OUTPUT_DRAIN_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone)]
pub(crate) struct TimedCommandOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub diagnostics: Vec<String>,
    pub wait_error: Option<String>,
}

enum OutputEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    StdoutDone,
    StderrDone,
    StdoutError(String),
    StderrError(String),
}

#[derive(Default)]
struct OutputState {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_done: bool,
    stderr_done: bool,
    diagnostics: Vec<String>,
}

impl OutputState {
    fn complete(&self) -> bool {
        self.stdout_done && self.stderr_done
    }

    fn finish(
        self,
        status: Option<i32>,
        timed_out: bool,
        wait_error: Option<String>,
    ) -> TimedCommandOutput {
        TimedCommandOutput {
            status,
            stdout: String::from_utf8_lossy(&self.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&self.stderr).trim().to_string(),
            timed_out,
            diagnostics: self.diagnostics,
            wait_error,
        }
    }
}

pub(crate) fn run_command_with_timeout(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
    poll_interval: Duration,
    env_remove: &[&str],
) -> Result<TimedCommandOutput, String> {
    run_command_with_timeout_and_env(program, args, cwd, timeout, poll_interval, env_remove, &[])
}

pub(crate) fn run_command_with_timeout_and_env(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
    poll_interval: Duration,
    env_remove: &[&str],
    env_set: &[(&str, &str)],
) -> Result<TimedCommandOutput, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in env_remove {
        command.env_remove(key);
    }
    for (key, value) in env_set {
        command.env(key, value);
    }
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to execute: {}", e))?;

    let (tx, rx) = mpsc::channel();
    let mut output = OutputState::default();
    match child.stdout.take() {
        Some(stdout) => spawn_output_reader(stdout, tx.clone(), true),
        None => output.stdout_done = true,
    }
    match child.stderr.take() {
        Some(stderr) => spawn_output_reader(stderr, tx.clone(), false),
        None => output.stderr_done = true,
    }
    drop(tx);

    let start = Instant::now();
    loop {
        drain_output_events(&rx, &mut output);
        match child.try_wait() {
            Ok(Some(status)) => {
                collect_output_until(
                    &rx,
                    &mut output,
                    Instant::now() + OUTPUT_DRAIN_GRACE,
                    OUTPUT_DRAIN_POLL,
                );
                if !output.complete() {
                    output.diagnostics.push(
                        "output collection did not finish after the child exited; descendant processes may still be holding stdout/stderr open".to_string(),
                    );
                }
                return Ok(output.finish(status.code(), false, None));
            }
            Ok(None) if start.elapsed() >= timeout => {
                #[cfg(unix)]
                {
                    // Kill the whole process group so an SSH transport cannot keep
                    // inherited stdout/stderr pipes open after Git is reaped.
                    let group = -(child.id() as i32);
                    // SAFETY: `group` names the child-owned process group created above.
                    if unsafe { libc::kill(group, libc::SIGKILL) } == 0 {
                        output
                            .diagnostics
                            .push("sent kill to child process group".to_string());
                    }
                }
                let kill_result = child.kill();
                match &kill_result {
                    Ok(()) => output
                        .diagnostics
                        .push("sent kill to child process".to_string()),
                    Err(e) => output
                        .diagnostics
                        .push(format!("failed to kill child process: {}", e)),
                }

                let wait_result = child.wait();
                let status = match wait_result {
                    Ok(status) => {
                        output.diagnostics.push(format!(
                            "child process exited after timeout with status {}",
                            status
                                .code()
                                .map(|code| code.to_string())
                                .unwrap_or_else(|| "signal".to_string())
                        ));
                        status.code()
                    }
                    Err(e) => {
                        output
                            .diagnostics
                            .push(format!("failed to wait for child after timeout: {}", e));
                        None
                    }
                };

                collect_output_until(
                    &rx,
                    &mut output,
                    Instant::now() + OUTPUT_DRAIN_GRACE,
                    OUTPUT_DRAIN_POLL,
                );
                if !output.complete() {
                    output.diagnostics.push(
                        "output collection incomplete after timeout; descendant processes may still be holding stdout/stderr open".to_string(),
                    );
                }
                return Ok(output.finish(status, true, None));
            }
            Ok(None) => {
                std::thread::sleep(poll_interval);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                collect_output_until(
                    &rx,
                    &mut output,
                    Instant::now() + OUTPUT_DRAIN_GRACE,
                    OUTPUT_DRAIN_POLL,
                );
                return Ok(output.finish(None, false, Some(e.to_string())));
            }
        }
    }
}

fn spawn_output_reader<R>(mut reader: R, tx: Sender<OutputEvent>, stdout: bool)
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let event = if stdout {
                        OutputEvent::Stdout(buf[..n].to_vec())
                    } else {
                        OutputEvent::Stderr(buf[..n].to_vec())
                    };
                    if tx.send(event).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let event = if stdout {
                        OutputEvent::StdoutError(e.to_string())
                    } else {
                        OutputEvent::StderrError(e.to_string())
                    };
                    let _ = tx.send(event);
                    return;
                }
            }
        }

        let event = if stdout {
            OutputEvent::StdoutDone
        } else {
            OutputEvent::StderrDone
        };
        let _ = tx.send(event);
    });
}

fn collect_output_until(
    rx: &Receiver<OutputEvent>,
    output: &mut OutputState,
    deadline: Instant,
    poll_interval: Duration,
) {
    while !output.complete() && Instant::now() < deadline {
        drain_output_events(rx, output);
        if output.complete() {
            break;
        }
        std::thread::sleep(poll_interval);
    }
    drain_output_events(rx, output);
}

fn drain_output_events(rx: &Receiver<OutputEvent>, output: &mut OutputState) {
    while let Ok(event) = rx.try_recv() {
        match event {
            OutputEvent::Stdout(bytes) => output.stdout.extend(bytes),
            OutputEvent::Stderr(bytes) => output.stderr.extend(bytes),
            OutputEvent::StdoutDone => output.stdout_done = true,
            OutputEvent::StderrDone => output.stderr_done = true,
            OutputEvent::StdoutError(err) => {
                output
                    .diagnostics
                    .push(format!("failed to read stdout: {}", err));
                output.stdout_done = true;
            }
            OutputEvent::StderrError(err) => {
                output
                    .diagnostics
                    .push(format!("failed to read stderr: {}", err));
                output.stderr_done = true;
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn timeout_kills_and_reaps_the_child_process_group() {
        let started = Instant::now();
        let output = run_command_with_timeout(
            "sh",
            &["-c", "sleep 30 & wait"],
            None,
            Duration::from_millis(100),
            Duration::from_millis(10),
            &[],
        )
        .expect("timed command should start");

        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            output
                .diagnostics
                .iter()
                .any(|message| message.contains("process group")),
            "timeout diagnostics should confirm process-group termination: {:?}",
            output.diagnostics
        );
    }
}
