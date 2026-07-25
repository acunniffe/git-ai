#[allow(unused_imports)]
use super::*;
use crate::error::GitAiError;
#[cfg(not(windows))]
use std::io;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(not(windows))]
use std::os::fd::{AsFd, AsRawFd};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(windows))]
use interprocess::local_socket::ConnectOptions;
#[cfg(not(windows))]
use interprocess::{
    ConnectWaitMode,
    local_socket::{GenericFilePath, Name, prelude::*},
};

#[cfg(windows)]
use named_pipe::{PipeClient as WindowsPipeClient, PipeServer as WindowsPipeServer};

pub(crate) fn checkpoint_control_timeout_uses_ci_or_test_budget() -> bool {
    std::env::var_os("GIT_AI_TEST_DB_PATH").is_some()
        || std::env::var_os("GITAI_TEST_DB_PATH").is_some()
        || std::env::var_os("CI").is_some()
}

pub(crate) fn checkpoint_control_response_timeout(
    request: &ControlRequest,
    use_ci_or_test_budget: bool,
) -> Duration {
    match request {
        // Queued checkpoint requests can block behind trace-ingest ordering. In
        // CI/test we allow the longer budget so replay-heavy daemon tests don't
        // tear down captured state mid-request. Product mode keeps the short
        // control timeout so a wedged prior Git root fails the checkpoint rather
        // than making the caller wait indefinitely.
        ControlRequest::CheckpointRun { .. } | ControlRequest::CheckpointDeliver { .. }
            if use_ci_or_test_budget =>
        {
            DAEMON_CHECKPOINT_RESPONSE_TIMEOUT
        }
        ControlRequest::CheckpointRun { .. } | ControlRequest::CheckpointDeliver { .. } => {
            DAEMON_CONTROL_RESPONSE_TIMEOUT
        }
        ControlRequest::SyncFamily { .. } if use_ci_or_test_budget => {
            DAEMON_CHECKPOINT_RESPONSE_TIMEOUT
        }
        ControlRequest::SyncFamily { .. } => DAEMON_CHECKPOINT_RESPONSE_TIMEOUT,
        ControlRequest::SnapshotWatermarks { .. } => Duration::from_millis(500),
        // Await blocks until the requested timeout is reached; give the daemon
        // a small grace period over the requested limit so the caller sees a
        // response rather than a client-side socket timeout.
        ControlRequest::Await { timeout_secs } => {
            Duration::from_secs(timeout_secs.saturating_add(5))
        }
        _ => DAEMON_CONTROL_RESPONSE_TIMEOUT,
    }
}

fn control_request_response_timeout(request: &ControlRequest) -> Duration {
    checkpoint_control_response_timeout(
        request,
        checkpoint_control_timeout_uses_ci_or_test_budget(),
    )
}

#[cfg(not(windows))]
pub fn local_socket_name<'a>(socket_path: &'a Path) -> Result<Name<'a>, GitAiError> {
    socket_path
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| GitAiError::Generic(format!("invalid daemon socket path: {}", e)))
}

/// Target trace socket receive buffer size in bytes.
///
/// Defaults to `TRACE_SOCKET_RECV_BUFFER_BYTES` and can be overridden via
/// `GIT_AI_TRACE_SOCKET_RECV_BUFFER_BYTES` to ramp toward 1 MiB (or larger)
/// without a code change. A value of `0` disables the buffer bump entirely.
#[cfg(not(windows))]
fn trace_socket_recv_buffer_bytes() -> usize {
    std::env::var("GIT_AI_TRACE_SOCKET_RECV_BUFFER_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(TRACE_SOCKET_RECV_BUFFER_BYTES)
}

#[cfg(not(windows))]
pub fn set_trace_socket_recv_buffer(stream: &LocalSocketStream) -> io::Result<()> {
    match stream {
        LocalSocketStream::UdSocket(stream) => {
            set_socket_recv_buffer(stream, trace_socket_recv_buffer_bytes())
        }
    }
}

/// Raise a socket's kernel receive buffer to `bytes` via `SO_RCVBUF`.
///
/// A `bytes` of `0` is a no-op (buffer bump disabled). The kernel may clamp the
/// request to `net.core.rmem_max` on Linux, so the effective value can be lower
/// than requested; that is fine -- this only ever raises capacity.
#[cfg(not(windows))]
pub(crate) fn set_socket_recv_buffer<S: AsFd>(socket: &S, bytes: usize) -> io::Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    let value = bytes.min(libc::c_int::MAX as usize) as libc::c_int;
    let result = unsafe {
        libc::setsockopt(
            socket.as_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &value as *const libc::c_int as *const libc::c_void,
            std::mem::size_of_val(&value) as libc::socklen_t,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(all(test, not(windows)))]
pub(crate) fn socket_recv_buffer<S: AsFd>(socket: &S) -> io::Result<usize> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of_val(&value) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            socket.as_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &mut value as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value.max(0) as usize)
    }
}

pub fn open_local_socket_stream_with_timeout(
    socket_path: &Path,
    timeout: Duration,
) -> Result<DaemonClientStream, GitAiError> {
    #[cfg(windows)]
    {
        let stream = open_windows_named_pipe_client_with_timeout(socket_path, timeout)?;
        Ok(DaemonClientStream::WindowsPipe(stream))
    }

    #[cfg(not(windows))]
    {
        ConnectOptions::new()
            .name(local_socket_name(socket_path)?)
            .wait_mode(ConnectWaitMode::Timeout(timeout))
            .connect_sync()
            .map_err(|e| {
                GitAiError::Generic(format!(
                    "timed out after {:?} connecting daemon socket {}: {}",
                    timeout,
                    socket_path.display(),
                    e
                ))
            })
    }
}

#[cfg(windows)]
fn open_windows_named_pipe_client_with_timeout(
    socket_path: &Path,
    timeout: Duration,
) -> Result<WindowsPipeClient, GitAiError> {
    let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
    WindowsPipeClient::connect_ms(socket_path.as_os_str(), timeout_ms).map_err(|e| {
        GitAiError::Generic(format!(
            "timed out after {:?} connecting daemon socket {}: {}",
            timeout,
            socket_path.display(),
            e
        ))
    })
}

pub fn set_daemon_client_stream_timeouts(
    stream: &mut DaemonClientStream,
    socket_path: &Path,
    timeout: Duration,
) -> Result<(), GitAiError> {
    #[cfg(windows)]
    {
        let _ = socket_path;
        match stream {
            DaemonClientStream::WindowsPipe(pipe) => {
                pipe.set_read_timeout(Some(timeout));
                pipe.set_write_timeout(Some(timeout));
                Ok(())
            }
        }
    }

    #[cfg(not(windows))]
    {
        stream.set_recv_timeout(Some(timeout)).map_err(|e| {
            GitAiError::Generic(format!(
                "failed to set daemon socket {} recv timeout: {}",
                socket_path.display(),
                e
            ))
        })?;
        stream.set_send_timeout(Some(timeout)).map_err(|e| {
            GitAiError::Generic(format!(
                "failed to set daemon socket {} send timeout: {}",
                socket_path.display(),
                e
            ))
        })
    }
}

/// Text-dedup constructor for the write/flush pair below; Display output is
/// unchanged from the sites it replaces.
fn daemon_request_io_error(action: &str, socket_path: &Path, e: std::io::Error) -> GitAiError {
    GitAiError::Generic(format!(
        "failed {} daemon request to {}: {}",
        action,
        socket_path.display(),
        e
    ))
}

fn write_all_daemon_client_stream(
    stream: &mut DaemonClientStream,
    socket_path: &Path,
    payload: &[u8],
) -> Result<(), GitAiError> {
    stream
        .write_all(payload)
        .map_err(|e| daemon_request_io_error("writing", socket_path, e))?;
    stream
        .flush()
        .map_err(|e| daemon_request_io_error("flushing", socket_path, e))?;
    Ok(())
}

fn read_daemon_client_line(
    reader: &mut BufReader<DaemonClientStream>,
    socket_path: &Path,
    response_timeout: Duration,
) -> Result<String, GitAiError> {
    let mut line = String::new();
    let deadline = std::time::Instant::now() + response_timeout;
    loop {
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err(GitAiError::Generic(format!(
                    "daemon socket {} closed without a response",
                    socket_path.display()
                )));
            }
            Ok(_) => return Ok(line),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if std::time::Instant::now() >= deadline {
                    return Err(GitAiError::Generic(format!(
                        "timed out after {:?} reading daemon response from {}",
                        response_timeout,
                        socket_path.display()
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => {
                return Err(GitAiError::Generic(format!(
                    "failed reading daemon response from {}: {}",
                    socket_path.display(),
                    error
                )));
            }
        }
    }
}

#[cfg(windows)]
fn send_control_request_with_timeouts_windows(
    socket_path: &Path,
    request: &ControlRequest,
    connect_timeout: Duration,
    response_timeout: Duration,
) -> Result<ControlResponse, GitAiError> {
    let mut stream = open_local_socket_stream_with_timeout(socket_path, connect_timeout)?;
    set_daemon_client_stream_timeouts(&mut stream, socket_path, response_timeout)?;
    let mut body = serde_json::to_vec(request)?;
    body.push(b'\n');
    write_all_daemon_client_stream(&mut stream, socket_path, &body)?;

    let mut response_reader = BufReader::new(stream);
    let line = read_daemon_client_line(&mut response_reader, socket_path, response_timeout)?;
    if line.trim().is_empty() {
        return Err(GitAiError::Generic(
            "empty daemon control response".to_string(),
        ));
    }
    serde_json::from_str(line.trim()).map_err(GitAiError::from)
}

#[cfg(not(windows))]
fn send_control_request_with_timeouts_unix(
    socket_path: &Path,
    request: &ControlRequest,
    connect_timeout: Duration,
    response_timeout: Duration,
) -> Result<ControlResponse, GitAiError> {
    let mut stream = open_local_socket_stream_with_timeout(socket_path, connect_timeout)?;
    set_daemon_client_stream_timeouts(&mut stream, socket_path, response_timeout)?;
    let mut body = serde_json::to_vec(request)?;
    body.push(b'\n');
    write_all_daemon_client_stream(&mut stream, socket_path, &body)?;

    let mut response_reader = BufReader::new(stream);
    let line = read_daemon_client_line(&mut response_reader, socket_path, response_timeout)?;
    if line.trim().is_empty() {
        return Err(GitAiError::Generic(
            "empty daemon control response".to_string(),
        ));
    }
    serde_json::from_str(line.trim()).map_err(GitAiError::from)
}

pub fn local_socket_connects_with_timeout(
    socket_path: &Path,
    timeout: Duration,
) -> Result<(), GitAiError> {
    let _stream = open_local_socket_stream_with_timeout(socket_path, timeout)?;
    Ok(())
}

pub fn send_control_request_with_timeout(
    socket_path: &Path,
    request: &ControlRequest,
    timeout: Duration,
) -> Result<ControlResponse, GitAiError> {
    send_control_request_with_timeouts(socket_path, request, timeout, timeout)
}

fn send_control_request_with_timeouts(
    socket_path: &Path,
    request: &ControlRequest,
    connect_timeout: Duration,
    response_timeout: Duration,
) -> Result<ControlResponse, GitAiError> {
    #[cfg(windows)]
    {
        send_control_request_with_timeouts_windows(
            socket_path,
            request,
            connect_timeout,
            response_timeout,
        )
    }

    #[cfg(not(windows))]
    {
        send_control_request_with_timeouts_unix(
            socket_path,
            request,
            connect_timeout,
            response_timeout,
        )
    }
}

pub fn send_control_request(
    socket_path: &Path,
    request: &ControlRequest,
) -> Result<ControlResponse, GitAiError> {
    send_control_request_with_timeouts(
        socket_path,
        request,
        DAEMON_CONTROL_CONNECT_TIMEOUT,
        control_request_response_timeout(request),
    )
}

pub fn send_control_request_fire_and_forget(
    socket_path: &Path,
    request: &ControlRequest,
) -> Result<(), GitAiError> {
    let mut stream =
        open_local_socket_stream_with_timeout(socket_path, DAEMON_CONTROL_CONNECT_TIMEOUT)?;
    let write_timeout = Duration::from_millis(500);
    set_daemon_client_stream_timeouts(&mut stream, socket_path, write_timeout)?;
    let mut body = serde_json::to_vec(request)?;
    body.push(b'\n');
    write_all_daemon_client_stream(&mut stream, socket_path, &body)?;
    Ok(())
}

#[cfg(windows)]
pub fn handle_windows_control_pipe_connection(
    mut server: WindowsPipeServer,
    coordinator: Arc<ActorDaemonCoordinator>,
    runtime_handle: tokio::runtime::Handle,
) {
    let mut reader = BufReader::new(&mut server);
    if let Err(e) = handle_control_connection_actor_reader(&mut reader, coordinator, runtime_handle)
    {
        tracing::debug!(%e, "control connection error");
    }
}

#[cfg(not(windows))]
pub fn handle_control_connection_actor(
    stream: LocalSocketStream,
    coordinator: Arc<ActorDaemonCoordinator>,
    runtime_handle: tokio::runtime::Handle,
) -> Result<(), GitAiError> {
    let mut reader = BufReader::new(stream);
    handle_control_connection_actor_reader(&mut reader, coordinator, runtime_handle)
}

pub fn handle_control_connection_actor_reader<R: Read + Write>(
    reader: &mut BufReader<R>,
    coordinator: Arc<ActorDaemonCoordinator>,
    runtime_handle: tokio::runtime::Handle,
) -> Result<(), GitAiError> {
    while let Some(line) = read_json_line(reader)? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed = serde_json::from_str::<ControlRequest>(trimmed);
        let mut shutdown_after_response = false;
        let response = match parsed {
            Ok(req) => {
                shutdown_after_response = matches!(req, ControlRequest::Shutdown);
                runtime_handle.block_on(async { coordinator.handle_control_request(req).await })
            }
            Err(e) => ControlResponse::err(format!("invalid control request: {}", e)),
        };
        let raw = serde_json::to_string(&response)?;
        reader.get_mut().write_all(raw.as_bytes())?;
        reader.get_mut().write_all(b"\n")?;
        reader.get_mut().flush()?;
        if shutdown_after_response {
            coordinator.request_stop();
        }
    }
    Ok(())
}
