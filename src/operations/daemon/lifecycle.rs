#[allow(unused_imports)]
use super::*;
use crate::config;
use crate::error::GitAiError;
use std::path::PathBuf;
use std::sync::Arc;

pub fn finalize_trace_connection_roots(
    coordinator: Arc<ActorDaemonCoordinator>,
    observed_roots: std::collections::BTreeSet<String>,
) -> Result<(), GitAiError> {
    if observed_roots.is_empty() {
        coordinator.trace_unidentified_connection_identified_or_closed()?;
        return Ok(());
    }

    let roots = observed_roots.into_iter().collect::<Vec<_>>();
    let close_marker_roots = coordinator.record_trace_connection_close(&roots)?;
    coordinator.enqueue_trace_connection_close_markers(close_marker_roots)
}

/// Git environment variables that must not leak into the daemon process.
///
/// The daemon is a long-lived, repository-agnostic process that serves requests
/// for many different repositories. Environment variables like `GIT_DIR` and
/// `GIT_WORK_TREE` pin git operations to a single repository and override the
/// `-C <path>` flag that the daemon uses to target each repository individually.
///
/// When a daemon is spawned by a git wrapper invocation (e.g. `git add`), the
/// parent process may have these variables set by git itself (hook context) or
/// by test harnesses. Clearing them at daemon startup prevents incorrect
/// repository resolution that manifests as `fatal: not a git repository: '/dev/null'`.
///
/// This list is used in two places:
/// - `spawn_daemon_run_detached` strips them from the child process via `env_remove`.
/// - `sanitize_git_env_for_daemon` clears them from the current process at daemon startup
///   as a belt-and-suspenders defence (the daemon may be launched by another mechanism).
pub const GIT_ENV_VARS_TO_SANITIZE: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_CEILING_DIRECTORIES",
    "GIT_QUARANTINE_PATH",
    "GIT_NAMESPACE",
];

pub fn sanitize_git_env_for_daemon() {
    for var in GIT_ENV_VARS_TO_SANITIZE {
        // SAFETY: daemon startup is single-threaded at this point -- the tokio
        // runtime is not yet running and no other threads exist.
        unsafe {
            std::env::remove_var(var);
        }
    }
}

pub fn disable_trace2_for_daemon_process() {
    // The daemon executes internal git commands while processing events and control requests.
    // If trace2.eventTarget points at this daemon socket globally, those internal git
    // commands can recursively feed trace2 events back into the daemon and starve progress.
    // Force-disable trace2 emission for the daemon process and all of its child git commands.
    unsafe {
        std::env::set_var("GIT_TRACE2_EVENT", "0");
    }
}

/// How often the daemon wakes up to evaluate whether an update check is due.
pub const DAEMON_UPDATE_CHECK_INTERVAL_SECS: u64 = 3600;

/// Maximum daemon uptime before a proactive restart (24.5 hours).
/// Deliberately offset from the 24h update-check cadence so the uptime restart
/// never races with an update-triggered shutdown.
pub const DAEMON_MAX_UPTIME_SECS: u64 = 24 * 3600 + 30 * 60;

/// Returns the update check interval, respecting an env var override for testing.
pub fn daemon_update_check_interval() -> u64 {
    std::env::var("GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DAEMON_UPDATE_CHECK_INTERVAL_SECS)
}

/// Returns the maximum uptime in nanoseconds, respecting an env var override for testing.
pub fn daemon_max_uptime_ns() -> u128 {
    let secs = std::env::var("GIT_AI_DAEMON_MAX_UPTIME_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DAEMON_MAX_UPTIME_SECS);
    secs as u128 * 1_000_000_000
}

pub const DAEMON_SOCKET_HEALTH_CHECK_SECS: u64 = 30;

pub fn daemon_socket_health_check_interval() -> u64 {
    std::env::var("GIT_AI_DAEMON_SOCKET_HEALTH_CHECK_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DAEMON_SOCKET_HEALTH_CHECK_SECS)
}

/// Spawn a detached `git-ai bg restart --hard` process that will reap the
/// current (zombie) daemon and start a fresh one.  The child inherits the
/// daemon env vars (GIT_AI_DAEMON_HOME, etc.) so it targets the same
/// instance.  Returns Ok if the process was spawned; the caller should
/// still request_shutdown so the current daemon exits promptly.
pub fn spawn_self_restart() -> Result<(), String> {
    let exe = crate::cli::git_ai_exe::current_git_ai_exe().map_err(|e| e.to_string())?;
    tracing::info!(?exe, "spawning detached restart process");

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["bg", "restart", "--hard"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    for var in GIT_ENV_VARS_TO_SANITIZE {
        cmd.env_remove(var);
    }
    cmd.env_remove("GIT_AI");

    #[cfg(windows)]
    {
        use crate::process_spawn::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }

    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to spawn restart process: {}", e))
}

pub const DAEMON_MIN_UPTIME_FOR_SELF_RESTART_SECS: u64 = 60;

pub fn daemon_min_uptime_for_self_restart() -> u64 {
    std::env::var("GIT_AI_DAEMON_MIN_UPTIME_FOR_RESTART_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DAEMON_MIN_UPTIME_FOR_SELF_RESTART_SECS)
}

/// Background loop that verifies the daemon's sockets are reachable by
/// actually connecting to them.  A successful connect proves the socket file
/// exists, points to this daemon's listener, and that the listener thread is
/// alive and calling accept().  If either probe fails (deleted file, stale
/// socket, hung listener), the daemon spawns a detached restart process and
/// shuts down.
///
/// To prevent restart loops when the underlying issue is systemic (e.g.
/// filesystem permissions, broken paths), the daemon only self-restarts if
/// it has been up for at least 60 seconds.  If sockets fail before that,
/// it shuts down without restart — the next wrapper invocation will attempt
/// to start a fresh daemon.
pub fn daemon_socket_health_check_loop(
    coordinator: Arc<ActorDaemonCoordinator>,
    control_socket_path: PathBuf,
    trace_socket_path: PathBuf,
) {
    let started = std::time::Instant::now();
    let interval = daemon_socket_health_check_interval().max(1);
    tracing::info!(
        interval,
        control = %control_socket_path.display(),
        trace = %trace_socket_path.display(),
        "socket health check started"
    );

    loop {
        {
            let guard = crate::model::repository::sqlite::poisoned_lock(
                &coordinator.shutdown_condvar_mutex,
            );
            if coordinator.is_shutting_down() {
                return;
            }
            let _ = coordinator
                .shutdown_condvar
                .wait_timeout(guard, std::time::Duration::from_secs(interval));
        }

        if coordinator.is_shutting_down() {
            return;
        }

        let control_ok =
            local_socket_connects_with_timeout(&control_socket_path, DAEMON_SOCKET_PROBE_TIMEOUT);
        let trace_ok =
            local_socket_connects_with_timeout(&trace_socket_path, DAEMON_SOCKET_PROBE_TIMEOUT);

        if control_ok.is_err() || trace_ok.is_err() {
            let uptime = started.elapsed();
            let min_uptime = std::time::Duration::from_secs(daemon_min_uptime_for_self_restart());

            if uptime >= min_uptime {
                tracing::warn!(
                    control = %control_ok.err().map(|e| e.to_string()).unwrap_or_else(|| "ok".into()),
                    trace = %trace_ok.err().map(|e| e.to_string()).unwrap_or_else(|| "ok".into()),
                    "socket health check failed, spawning restart and shutting down"
                );
                if let Err(e) = spawn_self_restart() {
                    tracing::error!("failed to spawn self-restart: {}", e);
                }
            } else {
                tracing::warn!(
                    control = %control_ok.err().map(|e| e.to_string()).unwrap_or_else(|| "ok".into()),
                    trace = %trace_ok.err().map(|e| e.to_string()).unwrap_or_else(|| "ok".into()),
                    uptime_secs = uptime.as_secs(),
                    "socket health check failed within minimum uptime, shutting down without restart"
                );
            }
            coordinator.request_shutdown();
            return;
        }
    }
}

/// Background loop that periodically checks for available updates.
///
/// Sleeps in short increments so it can exit promptly when the coordinator
/// signals shutdown.  When an update is detected, it requests a graceful
/// shutdown so the daemon can self-update after draining in-flight work.
pub fn daemon_update_check_loop(coordinator: Arc<ActorDaemonCoordinator>, started_at_ns: u128) {
    use crate::operations::commands::upgrade::{
        DaemonUpdateCheckResult, check_for_update_available,
    };

    let interval = daemon_update_check_interval().max(1);

    loop {
        {
            let guard = crate::model::repository::sqlite::poisoned_lock(
                &coordinator.shutdown_condvar_mutex,
            );
            if coordinator.is_shutting_down() {
                return;
            }
            let _ = coordinator
                .shutdown_condvar
                .wait_timeout(guard, std::time::Duration::from_secs(interval));
        }

        if coordinator.is_shutting_down() {
            return;
        }

        coordinator.gc_stale_family_state();

        match check_for_update_available() {
            Ok(DaemonUpdateCheckResult::UpdateReady) => {
                tracing::info!("update check: newer version available, requesting shutdown");
                coordinator.request_restart_after_update();
                return;
            }
            Ok(DaemonUpdateCheckResult::NoUpdate) => {
                tracing::info!("update check: no update needed");
            }
            Err(err) => {
                tracing::warn!(%err, "update check failed");
            }
        }

        let uptime_ns = now_unix_nanos().saturating_sub(started_at_ns);
        if uptime_ns >= daemon_max_uptime_ns() {
            tracing::info!("uptime exceeded max, requesting restart");
            coordinator.request_restart();
            return;
        }
    }
}

/// After the daemon has fully shut down, attempt to install any pending update.
///
/// On Unix the installer atomically replaces the binary via `mv`; on Windows
/// the installer is spawned as a detached process that polls until the exe is
/// unlocked.
pub fn daemon_run_pending_self_update() -> DaemonSelfUpdateOutcome {
    use crate::operations::commands::upgrade::{
        DaemonUpdateCheckResult, check_and_install_update_if_available,
    };

    match check_and_install_update_if_available() {
        Ok(DaemonUpdateCheckResult::UpdateReady) => {
            tracing::info!("self-update: installation completed successfully");
            DaemonSelfUpdateOutcome::Installed
        }
        Ok(DaemonUpdateCheckResult::NoUpdate) => {
            tracing::info!("self-update: no update to install");
            DaemonSelfUpdateOutcome::NoUpdate
        }
        Err(err) => {
            tracing::warn!(%err, "self-update: installation failed");
            crate::operations::commands::upgrade::clear_cached_update_state();
            DaemonSelfUpdateOutcome::Failed
        }
    }
}

pub(crate) async fn run_daemon(config: DaemonConfig) -> Result<DaemonExitAction, GitAiError> {
    sanitize_git_env_for_daemon();
    disable_trace2_for_daemon_process();
    config.ensure_parent_dirs()?;
    remove_stale_daemon_files(&config);
    let _lock = DaemonLock::acquire(&config.lock_path)?;
    let _active_guard = DaemonProcessActiveGuard::enter();
    write_pid_metadata(&config)?;

    // Initialize tracing subscriber before log file redirect so the fmt layer
    // captures stderr (fd 2). After dup2, writes go to the daemon log file.
    {
        use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

        let env_filter = if std::env::var("GIT_AI_DEBUG").as_deref() == Ok("1") {
            EnvFilter::new("debug")
        } else {
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
        };

        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(false)
                    .with_thread_ids(false)
                    .with_ansi(false)
                    .with_writer(std::io::stderr),
            )
            .with(crate::operations::daemon::sentry_layer::SentryLayer)
            .with(crate::operations::daemon::daemon_log_layer::DaemonLogUploadLayer)
            .init();
    }

    let _log_guard = maybe_setup_daemon_log_file(&config);

    tracing::info!(
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "daemon started"
    );

    remove_socket_if_exists(&config.trace_socket_path)?;
    remove_socket_if_exists(&config.control_socket_path)?;

    let mut coordinator_inner = ActorDaemonCoordinator::new();

    // Resolve store handles once here (pre-Arc window) so neither the telemetry
    // flush loop nor the coordinator control handlers call ::global() at runtime.
    // Each ::global() call triggers OnceLock::get_or_init with a degrade-don't-crash
    // fallback; calling them here means the fallback runs at most once.
    // The expects below are unreachable today — all three ::global() impls are
    // infallible (they open an in-process temp SQLite on any real error).  If that
    // ever becomes fallible, prefer logging + skipping rather than aborting.
    let metrics_db = crate::model::repository::metrics_db::MetricsDatabase::global()
        .expect("metrics DB fallback cannot fail");
    let notes_db = crate::model::repository::notes_db::NotesDatabase::global()
        .expect("notes DB fallback cannot fail");
    let internal_db = crate::model::repository::internal_db::InternalDatabase::global()
        .expect("internal DB fallback cannot fail");
    let bash_history_db =
        crate::model::repository::bash_history_db::BashHistoryDatabase::global().ok();

    let telemetry_stores = crate::operations::daemon::telemetry_worker::TelemetryStores {
        metrics: metrics_db,
        notes: notes_db,
        internal: internal_db,
    };

    // Spawn the telemetry worker inside the daemon's tokio runtime.
    let telemetry_handle =
        crate::operations::daemon::telemetry_worker::spawn_telemetry_worker(telemetry_stores);
    crate::operations::daemon::telemetry_worker::set_daemon_internal_telemetry(
        telemetry_handle.clone(),
    );
    coordinator_inner.telemetry_worker = Some(telemetry_handle.clone());
    coordinator_inner.bash_history_db = bash_history_db;
    coordinator_inner.metrics_db = Some(metrics_db);

    // Spawn the transcript worker BEFORE wrapping coordinator in Arc
    if config::Config::get()
        .get_feature_flags()
        .transcript_streaming
    {
        // Named "transcripts-db" for backwards compatibility with existing installations.
        // TODO: rename to "streams-db" with a migration that moves the file.
        let streams_db_path = config.internal_dir.join("transcripts-db");
        match crate::model::repository::streams_db::StreamsDatabase::open(&streams_db_path) {
            Ok(streams_db) => {
                let streams_db = std::sync::Arc::new(streams_db);
                let shutdown_notify = Arc::new(tokio::sync::Notify::new());
                let transcript_handle =
                    crate::operations::daemon::stream_worker::spawn_stream_worker(
                        streams_db.clone(),
                        telemetry_handle.clone(),
                        shutdown_notify.clone(),
                    );
                coordinator_inner.streams_db = Some(streams_db);
                coordinator_inner.stream_worker = Some(transcript_handle);
                let _ = coordinator_inner
                    .transcript_shutdown_notify
                    .set(shutdown_notify);
                tracing::info!("transcript worker spawned");
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to open transcripts database, transcript worker not started");
            }
        }
    }

    let coordinator = Arc::new(coordinator_inner);
    coordinator.start_trace_ingest_worker()?;
    let rt_handle = tokio::runtime::Handle::current();
    let control_socket_path = config.control_socket_path.clone();
    let trace_socket_path = config.trace_socket_path.clone();

    let control_coord = coordinator.clone();
    let control_shutdown_coord = coordinator.clone();
    let control_handle = rt_handle.clone();
    let control_thread = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            control_listener_loop_actor(control_socket_path, control_coord, control_handle)
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(%e, "control listener exited with error");
            }
            Err(_) => {
                tracing::error!("control listener panicked");
            }
        }
        // Always request shutdown so the daemon doesn't stay half-alive.
        control_shutdown_coord.request_shutdown();
    });

    let trace_coord = coordinator.clone();
    let trace_shutdown_coord = coordinator.clone();
    let trace_thread = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            trace_listener_loop_actor(trace_socket_path, trace_coord)
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(%e, "trace listener exited with error");
            }
            Err(_) => {
                tracing::error!("trace listener panicked");
            }
        }
        // Always request shutdown so the daemon doesn't stay half-alive.
        trace_shutdown_coord.request_shutdown();
    });

    let started_at_ns = now_unix_nanos();
    let update_coord = coordinator.clone();
    let update_thread = std::thread::spawn(move || {
        daemon_update_check_loop(update_coord, started_at_ns);
    });

    let health_coord = coordinator.clone();
    let health_control = config.control_socket_path.clone();
    let health_trace = config.trace_socket_path.clone();
    let health_thread = std::thread::spawn(move || {
        daemon_socket_health_check_loop(health_coord, health_control, health_trace);
    });

    coordinator.wait_for_shutdown().await;

    // Best-effort wake listeners to allow clean process exit.
    // Connect to each socket to unblock `accept()`.  If the socket files
    // were deleted (which is exactly what the health-check detects), the
    // connection will fail — fall back to a timed join so the process still
    // exits instead of hanging forever.
    let _ = local_socket_connects_with_timeout(
        &config.control_socket_path,
        DAEMON_SOCKET_PROBE_TIMEOUT,
    );
    let _ =
        local_socket_connects_with_timeout(&config.trace_socket_path, DAEMON_SOCKET_PROBE_TIMEOUT);

    let join_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    for (name, thread) in [
        ("control", control_thread),
        ("trace", trace_thread),
        ("update", update_thread),
        ("health", health_thread),
    ] {
        let remaining = join_deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            tracing::debug!("skipping join for {} thread (deadline exceeded)", name);
            continue;
        }
        let handle = std::thread::spawn(move || {
            let _ = thread.join();
        });
        let poll_until =
            std::time::Instant::now() + remaining.min(std::time::Duration::from_millis(500));
        loop {
            if handle.is_finished() {
                let _ = handle.join();
                break;
            }
            if std::time::Instant::now() >= poll_until {
                tracing::debug!("{} thread did not join in time, proceeding", name);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    remove_socket_if_exists(&config.trace_socket_path)?;
    remove_socket_if_exists(&config.control_socket_path)?;
    remove_pid_metadata(&config)?;

    let action = coordinator.shutdown_action();
    tracing::info!(?action, "daemon shutdown complete");

    Ok(action)
}
