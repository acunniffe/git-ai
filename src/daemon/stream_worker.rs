//! Daemon-side transcript worker for sweep-based transcript discovery.
//!
//! Runs inside the daemon process with two event sources:
//! 1. **Checkpoint notifications** (Immediate priority, <100ms) - fired when `git-ai checkpoint` is called
//! 2. **Periodic sweeps** (Low priority, every 30min) - agent-specific discovery of all sessions

use crate::authorship::authorship_log_serialization::{generate_session_id, generate_trace_id};
use crate::config;
use crate::daemon::telemetry_worker::DaemonTelemetryWorkerHandle;
use crate::daemon::transcript_redaction::redact_json_secrets;
use crate::metrics::{
    EventAttributes, MetricEvent, OtelTraceValues, PosEncoded, SessionEventValues,
};
use crate::streams::agent::{SHARED_STREAM_SESSION_ID, StreamDescriptor};
use crate::streams::db::{StreamRecord, StreamsDatabase};
use crate::streams::types::StreamError;
use crate::streams::watermark::{WatermarkStrategy, WatermarkType};
use chrono::{TimeZone, Utc};
use std::collections::{BinaryHeap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Notify;
use tokio::time::{Duration, interval};

const TRIGGERED_SWEEP_COOLDOWN: Duration = Duration::from_secs(30);

/// Extract a Unix-epoch u32 timestamp from a raw JSON event's "timestamp" field.
/// Handles both ISO 8601 strings (e.g. "2026-05-11T23:13:12.819Z") and numeric
/// milliseconds (e.g. 1759845073835). Returns None if the field is missing or unparseable.
pub fn extract_event_timestamp(event: &serde_json::Value) -> Option<u32> {
    let ts_val = event.get("timestamp")?;
    if let Some(s) = ts_val.as_str() {
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.timestamp() as u32)
    } else {
        ts_val.as_u64().map(|ms| (ms / 1000) as u32)
    }
}

/// Priority levels for processing tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub(super) enum Priority {
    Low = 2, // Sweep-discovered sessions
    Immediate = 0, // Checkpoint-triggered, process first
             // REMOVED: High = 1 (was polling)
}

/// Task to process a session's transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub(super) struct ProcessingTask {
    pub(super) priority: Priority,
    pub(super) session_id: String,
    pub(super) stream_kind: String,
    pub(super) tool: String,
    pub(super) trace_id: Option<String>,
    pub(super) tool_use_id: Option<String>,
    pub(super) canonical_path: PathBuf,
    pub(super) repo_work_dir: Option<PathBuf>,
    pub(super) retry_count: u32,
    #[cfg_attr(test, serde(skip))]
    pub(super) next_retry_at: Option<std::time::Instant>,
}

impl PartialOrd for ProcessingTask {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ProcessingTask {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Higher priority first (Immediate=0 < High=1 < Low=2)
        // Reverse comparison so smaller numeric value = higher priority = popped first from max-heap
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| self.session_id.cmp(&other.session_id))
    }
}

/// Source of an explicit transcript sweep request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepTrigger {
    PostCommit,
    PostPush,
}

impl std::fmt::Display for SweepTrigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PostCommit => f.write_str("post_commit"),
            Self::PostPush => f.write_str("post_push"),
        }
    }
}

struct SweepRequest {
    trigger: SweepTrigger,
    priority: Priority,
    completion: Option<std::sync::mpsc::Sender<Result<(), String>>>,
}

struct DrainRequest {
    completion: tokio::sync::oneshot::Sender<()>,
}

impl SweepRequest {
    fn normal(trigger: SweepTrigger) -> Self {
        Self {
            trigger,
            priority: Priority::Low,
            completion: None,
        }
    }
}

#[derive(Clone)]
struct SweepTriggerGate {
    last_triggered_at: Arc<Mutex<Option<Instant>>>,
}

impl SweepTriggerGate {
    fn new() -> Self {
        Self {
            last_triggered_at: Arc::new(Mutex::new(None)),
        }
    }

    fn try_trigger_at(
        &self,
        now: Instant,
        source: &str,
        trigger_action: impl FnOnce() -> bool,
    ) -> bool {
        let Ok(mut last_triggered_at) = self.last_triggered_at.lock() else {
            tracing::warn!("failed to lock transcript sweep trigger cooldown");
            return false;
        };

        if let Some(last) = *last_triggered_at {
            let elapsed = now.checked_duration_since(last).unwrap_or_default();
            if elapsed < TRIGGERED_SWEEP_COOLDOWN {
                tracing::debug!(
                    source,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "transcript sweep trigger suppressed by cooldown"
                );
                return false;
            }
        }

        if !trigger_action() {
            return false;
        }

        *last_triggered_at = Some(now);
        true
    }

    fn try_mark_sweep_at(&self, now: Instant, source: &str) -> bool {
        self.try_trigger_at(now, source, || true)
    }

    fn force_trigger_at(&self, now: Instant, trigger_action: impl FnOnce() -> bool) -> bool {
        let Ok(mut last_triggered_at) = self.last_triggered_at.lock() else {
            tracing::warn!("failed to lock transcript sweep trigger cooldown");
            return false;
        };

        if !trigger_action() {
            return false;
        }

        *last_triggered_at = Some(now);
        true
    }
}

/// Handle for sending checkpoint notifications and sweep requests to the worker.
#[derive(Clone)]
pub struct StreamWorkerHandle {
    checkpoint_tx: tokio::sync::mpsc::UnboundedSender<CheckpointNotification>,
    sweep_tx: tokio::sync::mpsc::UnboundedSender<SweepRequest>,
    drain_tx: tokio::sync::mpsc::UnboundedSender<DrainRequest>,
    sweep_trigger_gate: SweepTriggerGate,
}

impl StreamWorkerHandle {
    /// Notify the worker that a checkpoint was recorded.
    #[allow(clippy::too_many_arguments)]
    pub fn notify_checkpoint(
        &self,
        session_id: String,
        tool: String,
        trace_id: String,
        tool_use_id: Option<String>,
        stream_path: PathBuf,
        repo_work_dir: Option<PathBuf>,
        external_session_id: String,
        external_parent_session_id: Option<String>,
    ) {
        let notification = CheckpointNotification {
            session_id,
            tool,
            trace_id,
            tool_use_id,
            stream_path,
            repo_work_dir,
            external_session_id,
            external_parent_session_id,
        };
        let _ = self.checkpoint_tx.send(notification);
    }

    /// Request a full sweep unless another sweep was triggered recently.
    ///
    /// Returns true when a request was sent to the worker, false when it was
    /// suppressed by the cooldown or the worker has already stopped.
    pub fn trigger_sweep(&self, trigger: SweepTrigger) -> bool {
        self.trigger_sweep_at(trigger, Instant::now())
    }

    /// Request a sweep for commit-time attribution recovery.
    ///
    /// This bypasses the user-facing cooldown because the caller performs its own
    /// short bounded wait for a repo-linked metric row, but still marks the gate
    /// so the regular post-commit sweep for the same command is suppressed.
    pub fn trigger_sweep_for_recovery(
        &self,
        trigger: SweepTrigger,
    ) -> Option<std::sync::mpsc::Receiver<Result<(), String>>> {
        let (completion_tx, completion_rx) = std::sync::mpsc::channel();
        let mut completion_tx = Some(completion_tx);
        let sent = self
            .sweep_trigger_gate
            .force_trigger_at(Instant::now(), || {
                self.sweep_tx
                    .send(SweepRequest {
                        trigger,
                        priority: Priority::Immediate,
                        completion: completion_tx.take(),
                    })
                    .is_ok()
            });
        sent.then_some(completion_rx)
    }

    fn trigger_sweep_at(&self, trigger: SweepTrigger, now: Instant) -> bool {
        let source = trigger.to_string();
        self.sweep_trigger_gate.try_trigger_at(now, &source, || {
            self.sweep_tx.send(SweepRequest::normal(trigger)).is_ok()
        })
    }

    /// Request that the worker drain all immediate processing tasks.
    ///
    /// Returns after the worker has finished processing all currently-enqueued
    /// immediate-priority tasks and any that are already in flight.
    pub async fn drain(&self) -> Result<(), String> {
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        self.drain_tx
            .send(DrainRequest {
                completion: completion_tx,
            })
            .map_err(|_| "stream worker has stopped".to_string())?;
        completion_rx
            .await
            .map_err(|_| "stream worker drain was cancelled".to_string())
    }

    #[cfg(test)]
    fn for_test_sweep_triggers(sweep_tx: tokio::sync::mpsc::UnboundedSender<SweepRequest>) -> Self {
        let (checkpoint_tx, _checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
        let (drain_tx, _drain_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            checkpoint_tx,
            sweep_tx,
            drain_tx,
            sweep_trigger_gate: SweepTriggerGate::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct CheckpointNotification {
    session_id: String,
    tool: String,
    trace_id: String,
    tool_use_id: Option<String>,
    stream_path: PathBuf,
    repo_work_dir: Option<PathBuf>,
    external_session_id: String,
    external_parent_session_id: Option<String>,
}

/// Worker that processes transcript changes.
struct StreamWorker {
    streams_db: Arc<StreamsDatabase>,
    sweep_coordinator: crate::daemon::sweep_coordinator::SweepCoordinator, // NEW
    priority_queue: BinaryHeap<ProcessingTask>,
    delayed_tasks: Vec<ProcessingTask>,
    in_flight: HashSet<(PathBuf, String)>,
    telemetry_handle: DaemonTelemetryWorkerHandle,
    shutdown_notify: Arc<Notify>,
    shutdown_flag: Arc<AtomicBool>,
    checkpoint_rx: tokio::sync::mpsc::UnboundedReceiver<CheckpointNotification>,
    sweep_rx: tokio::sync::mpsc::UnboundedReceiver<SweepRequest>,
    drain_rx: tokio::sync::mpsc::UnboundedReceiver<DrainRequest>,
    sweep_trigger_gate: SweepTriggerGate,
}

impl StreamWorker {
    /// Create a new transcript worker.
    #[allow(clippy::too_many_arguments)]
    fn new(
        streams_db: Arc<StreamsDatabase>,
        telemetry_handle: DaemonTelemetryWorkerHandle,
        shutdown_notify: Arc<Notify>,
        shutdown_flag: Arc<AtomicBool>,
        checkpoint_rx: tokio::sync::mpsc::UnboundedReceiver<CheckpointNotification>,
        sweep_rx: tokio::sync::mpsc::UnboundedReceiver<SweepRequest>,
        drain_rx: tokio::sync::mpsc::UnboundedReceiver<DrainRequest>,
        sweep_trigger_gate: SweepTriggerGate,
    ) -> Self {
        let sweep_coordinator =
            crate::daemon::sweep_coordinator::SweepCoordinator::new(streams_db.clone());

        Self {
            streams_db,
            sweep_coordinator, // NEW
            priority_queue: BinaryHeap::new(),
            delayed_tasks: Vec::new(),
            in_flight: HashSet::new(),
            telemetry_handle,
            shutdown_notify,
            shutdown_flag,
            checkpoint_rx,
            sweep_rx,
            drain_rx,
            sweep_trigger_gate,
        }
    }

    /// Main processing loop.
    async fn run(mut self) {
        tracing::info!("transcript worker started");

        let sweep_enabled = config::Config::get().get_feature_flags().transcript_sweep;

        let mut sweep_ticker = interval(Duration::from_secs(30 * 60)); // NEW: 30 minutes

        // Skip the first immediate tick
        sweep_ticker.tick().await;

        // Run initial sweep on startup
        if sweep_enabled
            && self
                .sweep_trigger_gate
                .try_mark_sweep_at(Instant::now(), "initial")
            && let Err(e) = self.run_sweep(Priority::Low).await
        {
            tracing::error!(error = %e, "initial sweep failed");
        }

        loop {
            self.promote_ready_delayed_tasks(Instant::now());
            let has_ready_task = !self.priority_queue.is_empty();
            let next_retry_at = if has_ready_task {
                None
            } else {
                self.next_delayed_task_at()
            };
            let retry_sleep = async {
                if let Some(at) = next_retry_at {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };

            tokio::select! {
                _ = self.shutdown_notify.notified() => {
                    tracing::info!("transcript worker received shutdown signal");
                    self.drain_immediate_tasks().await;
                    self.shutdown_flag.store(true, Ordering::Relaxed);
                    break;
                }
                _ = async {}, if has_ready_task => {
                    self.process_next_task().await;
                }
                _ = retry_sleep => {}
                _ = sweep_ticker.tick() => {  // NEW: sweep ticker
                    if sweep_enabled
                        && self
                            .sweep_trigger_gate
                            .try_mark_sweep_at(Instant::now(), "periodic")
                        && let Err(e) = self.run_sweep(Priority::Low).await
                    {
                        tracing::error!(error = %e, "sweep failed");
                    }
                }
                Some(notification) = self.checkpoint_rx.recv() => {
                    self.handle_checkpoint_notification(notification).await;
                }
                Some(request) = self.sweep_rx.recv() => {
                    self.handle_sweep_request(request, sweep_enabled).await;
                }
                Some(request) = self.drain_rx.recv() => {
                    self.handle_drain_request(request, sweep_enabled).await;
                }
            }
        }

        tracing::info!("transcript worker shutdown complete");
    }

    fn promote_ready_delayed_tasks(&mut self, now: std::time::Instant) {
        let mut i = 0;
        while i < self.delayed_tasks.len() {
            if self.delayed_tasks[i].next_retry_at.is_none_or(|t| now >= t) {
                let task = self.delayed_tasks.swap_remove(i);
                self.priority_queue.push(task);
            } else {
                i += 1;
            }
        }
    }

    async fn handle_sweep_request(&mut self, request: SweepRequest, sweep_enabled: bool) {
        if sweep_enabled {
            tracing::info!(trigger = %request.trigger, "triggered transcript sweep requested");
            let result = self.run_sweep(request.priority).await;
            if let Err(e) = &result {
                tracing::error!(trigger = %request.trigger, error = %e, "triggered sweep failed");
            }
            if let Some(completion) = request.completion {
                let _ = completion.send(result.map(|_| ()));
            }
        } else {
            tracing::debug!(
                trigger = %request.trigger,
                "triggered transcript sweep skipped because transcript_sweep is disabled"
            );
            if let Some(completion) = request.completion {
                let _ = completion.send(Err("transcript_sweep feature is disabled".to_string()));
            }
        }
    }

    async fn handle_drain_request(&mut self, request: DrainRequest, sweep_enabled: bool) {
        // Checkpoint and sweep requests use separate channels from the drain
        // barrier, so consume ingress that was already queued before the barrier.
        while let Ok(notification) = self.checkpoint_rx.try_recv() {
            self.handle_checkpoint_notification(notification).await;
        }
        while let Ok(sweep_request) = self.sweep_rx.try_recv() {
            self.handle_sweep_request(sweep_request, sweep_enabled)
                .await;
        }

        // Process immediate priority tasks that are already queued.
        self.drain_immediate_tasks().await;

        // Continue processing newly promoted tasks until there is no ready
        // work and no in-flight processing.
        while !self.priority_queue.is_empty() || !self.in_flight.is_empty() {
            self.process_next_task().await;
        }

        let _ = request.completion.send(());
    }

    fn next_delayed_task_at(&self) -> Option<std::time::Instant> {
        self.delayed_tasks
            .iter()
            .filter_map(|task| task.next_retry_at)
            .min()
    }

    /// Run a sweep across all agents to discover new/behind sessions.
    async fn run_sweep(&mut self, priority: Priority) -> Result<(), String> {
        use crate::daemon::sweep_coordinator::SweepItem;

        let items = self
            .sweep_coordinator
            .run_sweep()
            .map_err(|e| e.to_string())?;

        tracing::info!(discovered = items.len(), "sweep completed");

        for (i, item) in items.iter().enumerate().take(10) {
            match item {
                SweepItem::Session {
                    session_id,
                    tool,
                    canonical_path,
                    ..
                } => {
                    tracing::info!(
                        index = i,
                        tool = %tool,
                        session_id = %session_id,
                        path = %canonical_path.display(),
                        "sweep item: session"
                    );
                }
                SweepItem::SharedStream {
                    tool,
                    stream_kind,
                    canonical_path,
                } => {
                    tracing::info!(
                        index = i,
                        tool = %tool,
                        stream_kind = %stream_kind,
                        path = %canonical_path.display(),
                        "sweep item: shared stream"
                    );
                }
            }
        }
        if items.len() > 10 {
            tracing::info!(remaining = items.len() - 10, "... and more sweep items");
        }

        let mut enqueued_this_sweep: HashSet<(PathBuf, String)> = HashSet::new();

        for item in items {
            match item {
                SweepItem::Session {
                    session_id,
                    tool,
                    canonical_path,
                    external_session_id,
                    external_parent_session_id,
                } => {
                    let inferred_cwd = crate::streams::agent::get_agent(&tool)
                        .as_ref()
                        .and_then(|a| a.infer_cwd(&canonical_path));

                    let tasks = self.enqueue_streams_for_session(
                        &tool,
                        &canonical_path,
                        priority,
                        None,
                        None,
                        Some(external_session_id.as_str()),
                        external_parent_session_id.as_deref(),
                        inferred_cwd.as_deref(),
                        &session_id,
                        &mut enqueued_this_sweep,
                    );

                    for task in tasks {
                        self.priority_queue.push(task);
                    }
                }
                SweepItem::SharedStream {
                    tool,
                    stream_kind,
                    canonical_path,
                } => {
                    let dedup_key = (canonical_path.clone(), stream_kind.clone());
                    if self.in_flight.contains(&dedup_key)
                        || enqueued_this_sweep.contains(&dedup_key)
                    {
                        continue;
                    }

                    let Some(agent) = crate::streams::agent::get_agent(&tool) else {
                        continue;
                    };
                    let Some(stream) = agent
                        .streams()
                        .into_iter()
                        .find(|s| s.stream_kind == stream_kind)
                    else {
                        continue;
                    };

                    if let Err(e) = self.ensure_stream_session(
                        SHARED_STREAM_SESSION_ID,
                        &tool,
                        &stream,
                        &canonical_path,
                        None,
                        None,
                        None,
                    ) {
                        tracing::warn!(
                            tool = %tool,
                            stream_kind = %stream_kind,
                            error = %e,
                            "failed to ensure shared stream session, skipping"
                        );
                        continue;
                    }

                    enqueued_this_sweep.insert(dedup_key);
                    self.priority_queue.push(ProcessingTask {
                        priority,
                        session_id: SHARED_STREAM_SESSION_ID.to_string(),
                        stream_kind,
                        tool,
                        trace_id: None,
                        tool_use_id: None,
                        canonical_path,
                        repo_work_dir: None,
                        retry_count: 0,
                        next_retry_at: None,
                    });
                }
            }
        }

        Ok(())
    }

    /// Handle a checkpoint notification.
    async fn handle_checkpoint_notification(&mut self, notification: CheckpointNotification) {
        let canonical_path = std::fs::canonicalize(&notification.stream_path)
            .unwrap_or_else(|_| notification.stream_path.clone());

        let mut enqueued: HashSet<(PathBuf, String)> = HashSet::new();
        let tasks = self.enqueue_streams_for_session(
            &notification.tool,
            &canonical_path,
            Priority::Immediate,
            Some(notification.trace_id.clone()),
            notification.tool_use_id.clone(),
            Some(notification.external_session_id.as_str()),
            notification.external_parent_session_id.as_deref(),
            notification.repo_work_dir.as_deref(),
            &notification.session_id,
            &mut enqueued,
        );

        for task in tasks {
            self.priority_queue.push(task);
        }

        // Sweep subagent transcripts for this main session (Claude only for now)
        if notification.tool == "claude" {
            self.sweep_subagents_for_session(&notification);
        }
    }

    /// Discover and enqueue subagent transcripts belonging to a main Claude session.
    ///
    /// Given a main session at `<project>/<uuid>.jsonl`, subagents live at
    /// `<project>/<uuid>/subagents/agent-*.jsonl`.
    fn sweep_subagents_for_session(&mut self, notification: &CheckpointNotification) {
        let stream_path = &notification.stream_path;

        let subagents_dir = match stream_path.file_stem() {
            Some(stem) => stream_path.with_file_name(stem).join("subagents"),
            None => return,
        };

        if !subagents_dir.is_dir() {
            return;
        }

        let Ok(entries) = std::fs::read_dir(&subagents_dir) else {
            return;
        };

        let external_parent_session_id = stream_path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());

        let lookback_cutoff = config::Config::get()
            .transcript_streaming_lookback_days()
            .map(|days| {
                std::time::SystemTime::now()
                    - std::time::Duration::from_secs(u64::from(days) * 24 * 60 * 60)
            });

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().map(|ext| ext == "jsonl") != Some(true) {
                continue;
            }

            let Some(external_session_id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
            else {
                continue;
            };

            let session_id = generate_session_id(&external_session_id, "claude");

            let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let dedup_key = (canonical.clone(), "transcript".to_string());
            if self.in_flight.contains(&dedup_key) {
                continue;
            }

            // Only apply lookback to NEW (untracked) subagent files — already-tracked
            // files are always processed so partial watermarks aren't abandoned.
            let path_str = canonical.display().to_string();
            let already_tracked = self
                .streams_db
                .get_stream(&session_id, "transcript", &path_str)
                .ok()
                .flatten()
                .is_some();

            if !already_tracked && let Some(cutoff) = lookback_cutoff {
                let too_old = path
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .is_some_and(|mtime| mtime < cutoff);
                if too_old {
                    continue;
                }
            }

            // Ensure the subagent session exists in the DB
            if let Err(e) = self.ensure_subagent_session(
                &session_id,
                &canonical,
                &external_session_id,
                external_parent_session_id.as_deref(),
                notification.repo_work_dir.as_deref(),
            ) {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "failed to ensure subagent session exists"
                );
                continue;
            }

            self.priority_queue.push(ProcessingTask {
                priority: Priority::Low,
                session_id,
                stream_kind: "transcript".to_string(),
                tool: "claude".to_string(),
                trace_id: Some(notification.trace_id.clone()),
                tool_use_id: None,
                canonical_path: canonical,
                repo_work_dir: notification.repo_work_dir.clone(),
                retry_count: 0,
                next_retry_at: None,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn enqueue_streams_for_session(
        &self,
        tool: &str,
        canonical_path: &Path,
        priority: Priority,
        trace_id: Option<String>,
        tool_use_id: Option<String>,
        external_session_id: Option<&str>,
        external_parent_session_id: Option<&str>,
        repo_work_dir: Option<&Path>,
        non_shared_session_id: &str,
        enqueued: &mut HashSet<(PathBuf, String)>,
    ) -> Vec<ProcessingTask> {
        let agent = crate::streams::agent::get_agent(tool);
        let streams = agent.as_ref().map(|a| a.streams()).unwrap_or_default();
        let mut tasks = Vec::new();

        for stream in streams {
            // Shared streams are global singletons that serve all sessions. By
            // default they are processed during sweeps; Copilot OTEL is the
            // exception because a Copilot transcript checkpoint implies the
            // same underlying agent likely wrote fresh trace rows.
            if stream.shared
                && priority == Priority::Immediate
                && !Self::should_enqueue_shared_stream_immediately(tool, &stream)
            {
                continue;
            }

            let stream_path = match stream.resolve_path(canonical_path) {
                Some(p) if p.exists() => p,
                _ => continue,
            };

            let effective_session_id = if stream.shared {
                SHARED_STREAM_SESSION_ID.to_string()
            } else {
                non_shared_session_id.to_string()
            };

            if let Err(e) = self.ensure_stream_session(
                &effective_session_id,
                tool,
                &stream,
                &stream_path,
                external_session_id,
                external_parent_session_id,
                repo_work_dir,
            ) {
                tracing::warn!(
                    session_id = %effective_session_id,
                    stream_kind = %stream.stream_kind,
                    error = %e,
                    "failed to ensure stream session exists"
                );
                continue;
            }

            let dedup_key = (stream_path.clone(), stream.stream_kind.to_string());
            if self.in_flight.contains(&dedup_key) || enqueued.contains(&dedup_key) {
                continue;
            }

            enqueued.insert(dedup_key);
            tasks.push(ProcessingTask {
                priority,
                session_id: effective_session_id,
                stream_kind: stream.stream_kind.to_string(),
                tool: tool.to_string(),
                trace_id: trace_id.clone(),
                tool_use_id: tool_use_id.clone(),
                canonical_path: stream_path,
                repo_work_dir: repo_work_dir.map(|p| p.to_path_buf()),
                retry_count: 0,
                next_retry_at: None,
            });
        }

        tasks
    }

    fn should_enqueue_shared_stream_immediately(tool: &str, stream: &StreamDescriptor) -> bool {
        matches!(tool, "copilot" | "github-copilot") && stream.stream_kind == "otel_traces"
    }

    fn ensure_subagent_session(
        &self,
        session_id: &str,
        path: &Path,
        external_session_id: &str,
        external_parent_session_id: Option<&str>,
        repo_work_dir: Option<&Path>,
    ) -> Result<(), StreamError> {
        let path_str = path.display().to_string();
        if self
            .streams_db
            .get_stream(session_id, "transcript", &path_str)?
            .is_some()
        {
            return Ok(());
        }

        use crate::streams::watermark::ByteOffsetWatermark;

        let initial_watermark = ByteOffsetWatermark::new(0);
        let record = StreamRecord {
            session_id: session_id.to_string(),
            stream_kind: "transcript".to_string(),
            tool: "claude".to_string(),
            stream_path: path_str,
            stream_format: "ClaudeJsonl".to_string(),
            watermark_type: "ByteOffset".to_string(),
            watermark_value: initial_watermark.serialize(),
            external_session_id: external_session_id.to_string(),
            external_parent_session_id: external_parent_session_id.map(|s| s.to_string()),
            first_seen_at: chrono::Utc::now().timestamp(),
            last_processed_at: 0,
            last_known_size: 0,
            last_modified: None,
            processing_errors: 0,
            last_error: None,
            repo_work_dir: repo_work_dir.map(|p| p.display().to_string()),
        };

        self.streams_db.insert_stream(&record)
    }

    #[allow(clippy::too_many_arguments)]
    fn ensure_stream_session(
        &self,
        session_id: &str,
        tool: &str,
        stream: &StreamDescriptor,
        stream_path: &Path,
        external_session_id: Option<&str>,
        external_parent_session_id: Option<&str>,
        repo_work_dir: Option<&Path>,
    ) -> Result<(), StreamError> {
        let path_str = stream_path.display().to_string();
        if self
            .streams_db
            .get_stream(session_id, stream.stream_kind, &path_str)?
            .is_some()
        {
            return Ok(());
        }

        let effective_wm_type = stream.effective_watermark_type(stream_path);
        let initial_watermark = effective_wm_type.create_initial_watermark();

        // For shared streams, external_session_id/parent/repo_work_dir are meaningless
        // since the resource serves all sessions — use empty/None to avoid stale first-caller data
        let is_shared = session_id == SHARED_STREAM_SESSION_ID;
        let record = StreamRecord {
            session_id: session_id.to_string(),
            stream_kind: stream.stream_kind.to_string(),
            tool: tool.to_string(),
            stream_path: path_str,
            stream_format: format!("{:?}", stream.effective_format(stream_path)),
            watermark_type: format!("{:?}", effective_wm_type),
            watermark_value: initial_watermark.serialize(),
            external_session_id: if is_shared {
                String::new()
            } else {
                external_session_id.unwrap_or("").to_string()
            },
            external_parent_session_id: if is_shared {
                None
            } else {
                external_parent_session_id.map(|s| s.to_string())
            },
            first_seen_at: chrono::Utc::now().timestamp(),
            last_processed_at: 0,
            last_known_size: 0,
            last_modified: None,
            processing_errors: 0,
            last_error: None,
            repo_work_dir: if is_shared {
                None
            } else {
                repo_work_dir.map(|p| p.display().to_string())
            },
        };

        self.streams_db.insert_stream(&record)
    }

    /// Process the next task from the queue.
    async fn process_next_task(&mut self) {
        // Move any now-ready delayed tasks back to the priority queue
        let now = std::time::Instant::now();
        self.promote_ready_delayed_tasks(now);

        let Some(task) = self.priority_queue.pop() else {
            return;
        };

        // Check if task is ready to be processed (retry delay)
        if let Some(next_retry_at) = task.next_retry_at
            && now < next_retry_at
        {
            self.delayed_tasks.push(task);
            return;
        }

        // Mark as in-flight
        self.in_flight
            .insert((task.canonical_path.clone(), task.stream_kind.clone()));

        // Process the session (spawn blocking to avoid blocking the worker loop)
        let db = self.streams_db.clone();
        let telemetry = self.telemetry_handle.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let task_clone = task.clone();

        let result = tokio::task::spawn_blocking(move || {
            Self::process_session_blocking(&db, &telemetry, &task_clone, &shutdown_flag)
        })
        .await;

        // Remove from in-flight
        self.in_flight
            .remove(&(task.canonical_path.clone(), task.stream_kind.clone()));

        // Handle result
        match result {
            Ok(Ok(())) => {
                // Success - task is done
            }
            Ok(Err(e)) => {
                // Error - handle retry logic
                self.handle_processing_error(task, e).await;
            }
            Err(e) => {
                // Panic in spawn_blocking
                tracing::error!(error = %e, session_id = %task.session_id, "task panicked");
                self.handle_processing_error(
                    task,
                    StreamError::Fatal {
                        message: format!("task panicked: {}", e),
                    },
                )
                .await;
            }
        }
    }

    /// Process a session (blocking I/O).
    ///
    /// Loops over bounded batches from `read_incremental`, saving the watermark
    /// after each batch for crash resilience. Applies backpressure between
    /// batches when the telemetry buffer is above a threshold, sleeping to let
    /// the 3-second flush cycle drain it.
    fn process_session_blocking(
        db: &StreamsDatabase,
        telemetry: &DaemonTelemetryWorkerHandle,
        task: &ProcessingTask,
        shutdown_flag: &AtomicBool,
    ) -> Result<(), StreamError> {
        let task_path_str = task.canonical_path.display().to_string();
        let stream = db
            .get_stream(&task.session_id, &task.stream_kind, &task_path_str)?
            .ok_or_else(|| StreamError::Fatal {
                message: format!("stream not found: {}", task.session_id),
            })?;

        let agent =
            crate::streams::agent::get_agent(&task.tool).ok_or_else(|| StreamError::Fatal {
                message: format!("unknown agent type: {}", task.tool),
            })?;

        let watermark_type: WatermarkType = stream.watermark_type.parse()?;

        let mut current_watermark = watermark_type.deserialize(&stream.watermark_value)?;
        let path = PathBuf::from(&stream.stream_path);
        let mut total_events = 0usize;
        let is_shared_stream = stream.session_id == SHARED_STREAM_SESSION_ID;

        // For shared streams, parent/repo/external attrs are meaningless since they'd
        // reflect whichever session first created the record. Per-event overrides handle
        // session_id and external_session_id; parent_session_id and repo_url are omitted.
        let parent_session_id = if is_shared_stream {
            None
        } else {
            stream
                .external_parent_session_id
                .as_ref()
                .map(|ext_pid| generate_session_id(ext_pid, &stream.tool))
        };

        // Resolve repo_work_dir with priority: task (hook) > DB > infer from transcript.
        // Shared streams serve multiple repos so repo_url must not be set at the batch level.
        let resolved_work_dir = if is_shared_stream {
            None
        } else {
            task.repo_work_dir
                .clone()
                .or_else(|| stream.repo_work_dir.as_ref().map(PathBuf::from))
                .or_else(|| agent.infer_cwd(&path))
        };

        // Persist inferred cwd to DB if stream didn't already have one
        if !is_shared_stream
            && stream.repo_work_dir.is_none()
            && let Some(ref work_dir) = resolved_work_dir
        {
            let _ = db.update_repo_work_dir(
                &stream.session_id,
                &task.stream_kind,
                &stream.stream_path,
                &work_dir.display().to_string(),
            );
        }

        let mut base_attrs = EventAttributes::with_version(env!("CARGO_PKG_VERSION"))
            .session_id(stream.session_id.clone())
            .tool(&stream.tool);

        if !is_shared_stream {
            base_attrs = base_attrs
                .external_session_id(stream.external_session_id.clone())
                .external_parent_session_id_opt(stream.external_parent_session_id.clone())
                .parent_session_id_opt(parent_session_id);
        }

        if let Some(ref work_dir) = resolved_work_dir
            && let Some(url) = crate::repo_url::resolve_repo_url_from_path(work_dir)
        {
            base_attrs = base_attrs.repo_url(url);
        }

        let file_meta = std::fs::metadata(&path).ok();
        let watermark_type_str = &stream.watermark_type;
        let is_initial_watermark = stream.watermark_value.is_empty()
            || watermark_type_str
                .parse::<crate::streams::watermark::WatermarkType>()
                .ok()
                .map(|wt| wt.create_initial_watermark().serialize() == stream.watermark_value)
                .unwrap_or(false);

        loop {
            if shutdown_flag.load(Ordering::Relaxed) {
                break;
            }

            let source_session_id = if stream.external_session_id.is_empty() {
                stream.session_id.as_str()
            } else {
                stream.external_session_id.as_str()
            };
            let batch = agent.read_incremental(&path, current_watermark, source_session_id)?;

            if batch.events.is_empty() {
                db.update_watermark(
                    &stream.session_id,
                    &task.stream_kind,
                    &stream.stream_path,
                    batch.new_watermark.as_ref(),
                )?;
                break;
            }

            let batch_count = batch.events.len();

            let is_otel_stream = task.stream_kind == "otel_traces";
            let metric_events: Vec<MetricEvent> = batch
                .events
                .into_iter()
                .enumerate()
                .filter_map(|(idx, raw_event)| {
                    let (eid, pid, tid) = agent.extract_event_ids(&raw_event);
                    let is_first_event = is_initial_watermark && total_events == 0 && idx == 0;
                    let event_ts = match &file_meta {
                        Some(meta) => {
                            agent.extract_event_timestamp(&raw_event, meta, is_first_event)
                        }
                        None => extract_event_timestamp(&raw_event).unwrap_or_else(|| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as u32
                        }),
                    };
                    let trace_id = generate_trace_id();
                    let mut event_attrs = base_attrs.clone().trace_id(trace_id);

                    if let Some(event_sid) = agent.extract_event_session_id(&raw_event) {
                        let derived_session_id = generate_session_id(&event_sid, &stream.tool);
                        event_attrs = event_attrs
                            .session_id(derived_session_id)
                            .external_session_id(event_sid);
                    } else if is_otel_stream {
                        tracing::debug!(
                            session_id = %task.session_id,
                            "dropping OTEL span without extractable session identifier"
                        );
                        return None;
                    }

                    let attrs_sparse = event_attrs.to_sparse();
                    let raw_event = redact_json_secrets(raw_event);
                    Some(if is_otel_stream {
                        MetricEvent::from_values_with_timestamp(
                            OtelTraceValues::with_ids(raw_event, eid, pid, tid),
                            attrs_sparse,
                            Some(event_ts),
                        )
                    } else {
                        MetricEvent::from_values_with_timestamp(
                            SessionEventValues::with_ids(raw_event, eid, pid, tid),
                            attrs_sparse,
                            Some(event_ts),
                        )
                    })
                })
                .collect();

            if let Err(e) = telemetry.persist_metrics_blocking(&metric_events) {
                tracing::warn!(%e, "telemetry: failed to persist transcript metrics locally");
            }

            // Backpressure: after synchronous local persistence, this mainly
            // throttles when metrics upload is available and pending DB rows
            // are accumulating faster than the flush loop can deliver them.
            // Short sleeps (~100ms) keep shutdown latency low since this runs
            // inside spawn_blocking. Capped at ~4s to avoid blocking forever.
            const BACKPRESSURE_THRESHOLD: usize = 5_000;
            const BACKPRESSURE_MAX_WAITS: usize = 40;
            for _ in 0..BACKPRESSURE_MAX_WAITS {
                if telemetry.metrics_buffer_len() < BACKPRESSURE_THRESHOLD {
                    break;
                }
                if shutdown_flag.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            total_events += batch_count;
            db.update_watermark(
                &stream.session_id,
                &task.stream_kind,
                &stream.stream_path,
                batch.new_watermark.as_ref(),
            )?;
            current_watermark = batch.new_watermark;
        }

        if let Ok(metadata) = std::fs::metadata(&stream.stream_path) {
            let file_size = metadata.len();
            let modified = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| Utc.timestamp_opt(d.as_secs() as i64, 0).unwrap());
            db.update_file_metadata(
                &stream.session_id,
                &task.stream_kind,
                &stream.stream_path,
                file_size,
                modified,
            )?;
        }

        tracing::debug!(
            session_id = %task.session_id,
            events = total_events,
            "processed session"
        );

        Ok(())
    }

    /// Handle a processing error with exponential backoff.
    async fn handle_processing_error(&mut self, task: ProcessingTask, error: StreamError) {
        match error {
            StreamError::Transient { message, .. } => {
                // Retry with exponential backoff: 5s, 30s, 5m, 30m
                let retry_count = task.retry_count + 1;
                let max_retries = 4;

                if retry_count >= max_retries {
                    tracing::error!(
                        session_id = %task.session_id,
                        error = %message,
                        "max retries exceeded, dropping task"
                    );
                    if let Err(e) = self.streams_db.record_error(
                        &task.session_id,
                        &task.stream_kind,
                        &task.canonical_path.display().to_string(),
                        &format!("max retries: {}", message),
                    ) {
                        tracing::warn!(session_id = %task.session_id, error = %e, "failed to record error in database");
                    }
                    return;
                }

                let delay = match retry_count {
                    1 => Duration::from_secs(5),
                    2 => Duration::from_secs(30),
                    3 => Duration::from_secs(5 * 60),
                    _ => Duration::from_secs(30 * 60),
                };

                tracing::warn!(
                    session_id = %task.session_id,
                    error = %message,
                    retry = retry_count,
                    delay_secs = delay.as_secs(),
                    "transient error, will retry"
                );

                // Re-queue with updated retry count and next_retry_at
                let mut retried_task = task.clone();
                retried_task.retry_count = retry_count;
                retried_task.next_retry_at = Some(std::time::Instant::now() + delay);
                self.delayed_tasks.push(retried_task);
            }
            StreamError::Parse { line, message } => {
                // Parse errors are not retried
                tracing::error!(
                    session_id = %task.session_id,
                    line = line,
                    error = %message,
                    "parse error, skipping session"
                );
                if let Err(e) = self.streams_db.record_error(
                    &task.session_id,
                    &task.stream_kind,
                    &task.canonical_path.display().to_string(),
                    &format!("parse line {}: {}", line, message),
                ) {
                    tracing::warn!(session_id = %task.session_id, error = %e, "failed to record error in database");
                }
            }
            StreamError::Fatal { message } => {
                // Fatal errors are not retried
                tracing::error!(
                    session_id = %task.session_id,
                    error = %message,
                    "fatal error, skipping session"
                );
                if let Err(e) = self.streams_db.record_error(
                    &task.session_id,
                    &task.stream_kind,
                    &task.canonical_path.display().to_string(),
                    &format!("fatal: {}", message),
                ) {
                    tracing::warn!(session_id = %task.session_id, error = %e, "failed to record error in database");
                }
            }
        }
    }

    /// Drain immediate priority tasks before shutdown.
    async fn drain_immediate_tasks(&mut self) {
        let immediate_tasks = self.take_immediate_tasks();

        tracing::info!(tasks = immediate_tasks.len(), "draining immediate tasks");

        // Process immediate tasks
        for task in immediate_tasks {
            self.in_flight
                .insert((task.canonical_path.clone(), task.stream_kind.clone()));
            let db = self.streams_db.clone();
            let telemetry = self.telemetry_handle.clone();
            let shutdown_flag = self.shutdown_flag.clone();
            let task_clone = task.clone();

            let result = tokio::task::spawn_blocking(move || {
                Self::process_session_blocking(&db, &telemetry, &task_clone, &shutdown_flag)
            })
            .await;

            self.in_flight
                .remove(&(task.canonical_path.clone(), task.stream_kind.clone()));

            match result {
                Err(e) => {
                    tracing::error!(error = %e, session_id = %task.session_id, "drain task panicked");
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, session_id = %task.session_id, "drain task processing error");
                }
                Ok(Ok(())) => {}
            }
        }
    }

    /// Removes immediate tasks while preserving lower-priority work.
    fn take_immediate_tasks(&mut self) -> Vec<ProcessingTask> {
        let mut immediate_tasks = Vec::new();
        let mut retained_tasks = Vec::new();

        while let Some(task) = self.priority_queue.pop() {
            if task.priority == Priority::Immediate {
                immediate_tasks.push(task);
            } else {
                retained_tasks.push(task);
            }
        }
        self.priority_queue.extend(retained_tasks);

        let mut i = 0;
        while i < self.delayed_tasks.len() {
            if self.delayed_tasks[i].priority == Priority::Immediate {
                immediate_tasks.push(self.delayed_tasks.swap_remove(i));
            } else {
                i += 1;
            }
        }

        immediate_tasks
    }
}

/// Spawn the transcript worker.
pub fn spawn_stream_worker(
    streams_db: Arc<StreamsDatabase>,
    telemetry_handle: DaemonTelemetryWorkerHandle,
    shutdown_notify: Arc<Notify>,
) -> StreamWorkerHandle {
    let (checkpoint_tx, checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
    let (sweep_tx, sweep_rx) = tokio::sync::mpsc::unbounded_channel();
    let (drain_tx, drain_rx) = tokio::sync::mpsc::unbounded_channel();
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let sweep_trigger_gate = SweepTriggerGate::new();

    let worker = StreamWorker::new(
        streams_db,
        telemetry_handle,
        shutdown_notify,
        shutdown_flag,
        checkpoint_rx,
        sweep_rx,
        drain_rx,
        sweep_trigger_gate.clone(),
    );

    tokio::spawn(async move {
        worker.run().await;
    });

    StreamWorkerHandle {
        checkpoint_tx,
        sweep_tx,
        drain_tx,
        sweep_trigger_gate,
    }
}

#[cfg(test)]
mod extract_event_timestamp_tests {
    use super::*;

    #[test]
    fn test_rfc3339() {
        let event = serde_json::json!({"timestamp": "2026-05-11T23:13:12.819Z"});
        assert_eq!(extract_event_timestamp(&event), Some(1778541192));
    }

    #[test]
    fn test_rfc3339_without_millis() {
        let event = serde_json::json!({"timestamp": "2026-05-12T00:21:05Z"});
        assert_eq!(extract_event_timestamp(&event), Some(1778545265));
    }

    #[test]
    fn test_rfc3339_subsecond_discarded() {
        let event = serde_json::json!({"timestamp": "2026-05-11T23:13:12.999Z"});
        assert_eq!(extract_event_timestamp(&event), Some(1778541192));
    }

    #[test]
    fn test_numeric_millis() {
        let event = serde_json::json!({"timestamp": 1759845073835u64});
        assert_eq!(extract_event_timestamp(&event), Some(1759845073));
    }

    #[test]
    fn test_missing_field() {
        let event = serde_json::json!({"type": "user.message"});
        assert_eq!(extract_event_timestamp(&event), None);
    }

    #[test]
    fn test_null_value() {
        let event = serde_json::json!({"timestamp": null});
        assert_eq!(extract_event_timestamp(&event), None);
    }

    #[test]
    fn test_invalid_string() {
        let event = serde_json::json!({"timestamp": "not-a-date"});
        assert_eq!(extract_event_timestamp(&event), None);
    }

    #[test]
    fn test_empty_string() {
        let event = serde_json::json!({"timestamp": ""});
        assert_eq!(extract_event_timestamp(&event), None);
    }
}

#[cfg(test)]
mod scheduling_tests {
    use super::*;
    use tempfile::TempDir;

    fn make_worker() -> (
        TempDir,
        StreamWorker,
        Arc<Notify>,
        tokio::sync::mpsc::UnboundedSender<CheckpointNotification>,
    ) {
        let temp = TempDir::new().unwrap();
        let db = Arc::new(StreamsDatabase::open(temp.path().join("streams.db")).unwrap());
        let (checkpoint_tx, checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_sweep_tx, sweep_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_drain_tx, drain_rx) = tokio::sync::mpsc::unbounded_channel();
        let shutdown = Arc::new(Notify::new());
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let telemetry = DaemonTelemetryWorkerHandle::new_noop();
        let sweep_trigger_gate = SweepTriggerGate::new();
        assert!(sweep_trigger_gate.try_mark_sweep_at(Instant::now(), "test"));
        let worker = StreamWorker::new(
            db,
            telemetry,
            shutdown.clone(),
            shutdown_flag,
            checkpoint_rx,
            sweep_rx,
            drain_rx,
            sweep_trigger_gate,
        );
        (temp, worker, shutdown, checkpoint_tx)
    }

    fn task(session_id: &str, next_retry_at: Option<std::time::Instant>) -> ProcessingTask {
        ProcessingTask {
            priority: Priority::Immediate,
            session_id: session_id.to_string(),
            stream_kind: "transcript".to_string(),
            tool: "test".to_string(),
            trace_id: None,
            tool_use_id: None,
            canonical_path: PathBuf::from(format!("/test/{session_id}")),
            repo_work_dir: None,
            retry_count: 0,
            next_retry_at,
        }
    }

    #[test]
    fn promotes_only_ready_delayed_tasks() {
        let (_temp, mut worker, _shutdown, _checkpoint_tx) = make_worker();
        let now = std::time::Instant::now();
        worker.delayed_tasks.push(task("ready", Some(now)));
        worker
            .delayed_tasks
            .push(task("future", Some(now + Duration::from_secs(5))));

        worker.promote_ready_delayed_tasks(now);

        assert_eq!(worker.priority_queue.len(), 1);
        assert_eq!(worker.priority_queue.peek().unwrap().session_id, "ready");
        assert_eq!(worker.delayed_tasks.len(), 1);
        assert_eq!(worker.delayed_tasks[0].session_id, "future");
    }

    #[test]
    fn next_delayed_task_at_returns_earliest_retry_deadline() {
        let (_temp, mut worker, _shutdown, _checkpoint_tx) = make_worker();
        let now = std::time::Instant::now();
        let later = now + Duration::from_secs(30);
        let earlier = now + Duration::from_secs(5);
        worker.delayed_tasks.push(task("later", Some(later)));
        worker.delayed_tasks.push(task("earlier", Some(earlier)));

        assert_eq!(worker.next_delayed_task_at(), Some(earlier));
    }

    #[test]
    fn take_immediate_tasks_preserves_low_priority_tasks() {
        let (_temp, mut worker, _shutdown, _checkpoint_tx) = make_worker();
        let immediate = task("immediate", None);
        let mut queued_low = task("queued-low", None);
        queued_low.priority = Priority::Low;
        let mut delayed_low = task("delayed-low", None);
        delayed_low.priority = Priority::Low;

        worker.priority_queue.push(immediate.clone());
        worker.priority_queue.push(queued_low.clone());
        worker.delayed_tasks.push(delayed_low.clone());

        assert_eq!(worker.take_immediate_tasks(), vec![immediate]);
        assert_eq!(worker.priority_queue.into_vec(), vec![queued_low]);
        assert_eq!(worker.delayed_tasks, vec![delayed_low]);
    }

    #[tokio::test]
    async fn transient_errors_are_stored_as_delayed_retries_not_ready_work() {
        let (_temp, mut worker, _shutdown, _checkpoint_tx) = make_worker();

        worker
            .handle_processing_error(
                task("retry", None),
                StreamError::Transient {
                    message: "temporary".to_string(),
                    retry_after: Duration::from_secs(1),
                },
            )
            .await;

        assert!(worker.priority_queue.is_empty());
        assert_eq!(worker.delayed_tasks.len(), 1);
        assert_eq!(worker.delayed_tasks[0].session_id, "retry");
        assert_eq!(worker.delayed_tasks[0].retry_count, 1);
        assert!(worker.delayed_tasks[0].next_retry_at.is_some());
    }

    #[tokio::test]
    async fn shutdown_wakes_idle_worker() {
        let (_temp, worker, shutdown, _checkpoint_tx) = make_worker();
        let handle = tokio::spawn(worker.run());
        tokio::task::yield_now().await;

        shutdown.notify_one();

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("idle worker should exit promptly after shutdown notification")
            .expect("worker task should not panic");
    }

    #[tokio::test]
    async fn shutdown_wakes_worker_sleeping_until_future_retry() {
        let (_temp, mut worker, shutdown, _checkpoint_tx) = make_worker();
        worker.delayed_tasks.push(task(
            "future-retry",
            Some(std::time::Instant::now() + Duration::from_secs(60 * 60)),
        ));
        let handle = tokio::spawn(worker.run());
        tokio::task::yield_now().await;

        shutdown.notify_one();

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("retry-sleeping worker should exit promptly after shutdown notification")
            .expect("worker task should not panic");
    }

    #[tokio::test]
    async fn drain_processes_queued_checkpoint_notifications_before_completing() {
        let (temp, mut worker, _shutdown, checkpoint_tx) = make_worker();
        checkpoint_tx
            .send(CheckpointNotification {
                session_id: "session".to_string(),
                tool: "unknown".to_string(),
                trace_id: "trace".to_string(),
                tool_use_id: None,
                stream_path: temp.path().join("transcript.jsonl"),
                repo_work_dir: Some(temp.path().to_path_buf()),
                external_session_id: "external".to_string(),
                external_parent_session_id: None,
            })
            .unwrap();
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();

        worker
            .handle_drain_request(
                DrainRequest {
                    completion: completion_tx,
                },
                false,
            )
            .await;

        completion_rx.await.unwrap();
        assert!(
            worker.checkpoint_rx.is_empty(),
            "drain must process checkpoint notifications queued before the barrier"
        );
    }
}

#[cfg(test)]
mod subagent_sweep_tests {
    use super::*;
    use std::io::Write;
    use std::time::Instant;
    use tempfile::TempDir;
    use tokio::sync::mpsc::error::TryRecvError;

    fn make_worker(db: Arc<StreamsDatabase>) -> StreamWorker {
        let (_checkpoint_tx, checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_sweep_tx, sweep_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_drain_tx, drain_rx) = tokio::sync::mpsc::unbounded_channel();
        let shutdown = Arc::new(Notify::new());
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let telemetry = DaemonTelemetryWorkerHandle::new_noop();
        StreamWorker::new(
            db,
            telemetry,
            shutdown,
            shutdown_flag,
            checkpoint_rx,
            sweep_rx,
            drain_rx,
            SweepTriggerGate::new(),
        )
    }

    #[test]
    fn triggered_sweeps_share_thirty_second_cooldown() {
        let (sweep_tx, mut sweep_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = StreamWorkerHandle::for_test_sweep_triggers(sweep_tx);
        let started_at = Instant::now();

        assert!(
            handle
                .sweep_trigger_gate
                .try_mark_sweep_at(started_at, "periodic")
        );

        assert!(!handle.trigger_sweep_at(
            SweepTrigger::PostCommit,
            started_at + Duration::from_secs(29)
        ));
        assert!(matches!(sweep_rx.try_recv(), Err(TryRecvError::Empty)));

        assert!(handle.trigger_sweep_at(
            SweepTrigger::PostCommit,
            started_at + Duration::from_secs(30)
        ));
        let request = sweep_rx.try_recv().unwrap();
        assert_eq!(request.trigger, SweepTrigger::PostCommit);
        assert_eq!(request.priority, Priority::Low);
        assert!(request.completion.is_none());

        assert!(
            !handle.trigger_sweep_at(SweepTrigger::PostPush, started_at + Duration::from_secs(59))
        );
        assert!(matches!(sweep_rx.try_recv(), Err(TryRecvError::Empty)));

        assert!(
            handle.trigger_sweep_at(SweepTrigger::PostPush, started_at + Duration::from_secs(60))
        );
        let request = sweep_rx.try_recv().unwrap();
        assert_eq!(request.trigger, SweepTrigger::PostPush);
        assert_eq!(request.priority, Priority::Low);
        assert!(request.completion.is_none());
    }

    #[test]
    fn recovery_sweep_bypasses_cooldown_and_suppresses_followup_trigger() {
        let (sweep_tx, mut sweep_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = StreamWorkerHandle::for_test_sweep_triggers(sweep_tx);
        let started_at = Instant::now();

        assert!(
            handle
                .sweep_trigger_gate
                .try_mark_sweep_at(started_at, "periodic")
        );

        let completion = handle
            .trigger_sweep_for_recovery(SweepTrigger::PostCommit)
            .expect("recovery sweep should enqueue");
        let request = sweep_rx.try_recv().unwrap();
        assert_eq!(request.trigger, SweepTrigger::PostCommit);
        assert_eq!(request.priority, Priority::Immediate);
        assert!(request.completion.is_some());
        drop(completion);

        assert!(!handle.trigger_sweep_at(
            SweepTrigger::PostCommit,
            started_at + Duration::from_secs(29)
        ));
        assert!(matches!(sweep_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn test_sweep_subagents_discovers_subagent_files() {
        let tmp = TempDir::new().unwrap();

        // Create a main session transcript: <project>/sess-abc.jsonl
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let main_transcript = project_dir.join("sess-abc.jsonl");
        let mut f = std::fs::File::create(&main_transcript).unwrap();
        writeln!(f, r#"{{"type":"session","id":"sess-abc"}}"#).unwrap();

        // Create subagents directory: <project>/sess-abc/subagents/
        let subagents_dir = project_dir.join("sess-abc").join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();

        // Create two subagent transcripts
        let sub1 = subagents_dir.join("agent-sub1.jsonl");
        let mut f = std::fs::File::create(&sub1).unwrap();
        writeln!(
            f,
            r#"{{"type":"message","message":{{"role":"user","content":"hi"}}}}"#
        )
        .unwrap();

        let sub2 = subagents_dir.join("agent-sub2.jsonl");
        let mut f = std::fs::File::create(&sub2).unwrap();
        writeln!(
            f,
            r#"{{"type":"message","message":{{"role":"assistant","content":"hello"}}}}"#
        )
        .unwrap();

        // Also create a .meta.json file that should be ignored
        let meta = subagents_dir.join("agent-sub1.meta.json");
        std::fs::File::create(&meta).unwrap();

        // Set up worker with DB
        let db_path = tmp.path().join("test.db");
        let db = Arc::new(StreamsDatabase::open(&db_path).unwrap());
        let mut worker = make_worker(db.clone());

        let notification = CheckpointNotification {
            session_id: "internal-sess-abc".to_string(),
            tool: "claude".to_string(),
            trace_id: "trace-1".to_string(),
            tool_use_id: None,
            stream_path: main_transcript.clone(),
            repo_work_dir: Some(tmp.path().to_path_buf()),
            external_session_id: "sess-abc".to_string(),
            external_parent_session_id: None,
        };

        worker.sweep_subagents_for_session(&notification);

        // Should have enqueued 2 subagent tasks
        assert_eq!(worker.priority_queue.len(), 2);

        // Both should be in the DB (paths are canonicalized before storage)
        let sub1_sid = generate_session_id("agent-sub1", "claude");
        let sub2_sid = generate_session_id("agent-sub2", "claude");
        let sub1_canonical = std::fs::canonicalize(&sub1).unwrap();
        let sub2_canonical = std::fs::canonicalize(&sub2).unwrap();

        let rec1 = db
            .get_stream(
                &sub1_sid,
                "transcript",
                &sub1_canonical.display().to_string(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(rec1.external_session_id, "agent-sub1");
        assert_eq!(rec1.external_parent_session_id.as_deref(), Some("sess-abc"));
        assert_eq!(rec1.tool, "claude");

        let rec2 = db
            .get_stream(
                &sub2_sid,
                "transcript",
                &sub2_canonical.display().to_string(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(rec2.external_session_id, "agent-sub2");
        assert_eq!(rec2.external_parent_session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn test_sweep_subagents_no_dir_is_noop() {
        let tmp = TempDir::new().unwrap();
        let main_transcript = tmp.path().join("sess-xyz.jsonl");
        let mut f = std::fs::File::create(&main_transcript).unwrap();
        writeln!(f, r#"{{"type":"session"}}"#).unwrap();

        let db_path = tmp.path().join("test.db");
        let db = Arc::new(StreamsDatabase::open(&db_path).unwrap());
        let mut worker = make_worker(db.clone());

        let notification = CheckpointNotification {
            session_id: "internal-sess-xyz".to_string(),
            tool: "claude".to_string(),
            trace_id: "trace-2".to_string(),
            tool_use_id: None,
            stream_path: main_transcript,
            repo_work_dir: None,
            external_session_id: "sess-xyz".to_string(),
            external_parent_session_id: None,
        };

        worker.sweep_subagents_for_session(&notification);
        assert_eq!(worker.priority_queue.len(), 0);
    }

    #[tokio::test]
    async fn test_handle_checkpoint_skips_subagent_sweep_for_non_claude() {
        let tmp = TempDir::new().unwrap();
        let main_transcript = tmp.path().join("sess-abc.jsonl");
        std::fs::File::create(&main_transcript).unwrap();

        let subagents_dir = tmp.path().join("sess-abc").join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();
        let sub = subagents_dir.join("agent-sub1.jsonl");
        let mut f = std::fs::File::create(&sub).unwrap();
        writeln!(f, r#"{{"type":"message"}}"#).unwrap();

        let db_path = tmp.path().join("test.db");
        let db = Arc::new(StreamsDatabase::open(&db_path).unwrap());
        let mut worker = make_worker(db.clone());

        let notification = CheckpointNotification {
            session_id: "internal-sess-abc".to_string(),
            tool: "copilot".to_string(),
            trace_id: "trace-3".to_string(),
            tool_use_id: None,
            stream_path: main_transcript,
            repo_work_dir: None,
            external_session_id: "sess-abc".to_string(),
            external_parent_session_id: None,
        };

        worker.handle_checkpoint_notification(notification).await;

        // Only the main session should be enqueued — no subagent sweep for copilot
        assert_eq!(worker.priority_queue.len(), 1);
        let task = worker.priority_queue.pop().unwrap();
        assert_eq!(task.session_id, "internal-sess-abc");
        assert_eq!(task.tool, "copilot");
    }

    #[tokio::test]
    async fn test_copilot_checkpoint_enqueues_shared_otel_stream_immediately() {
        let tmp = TempDir::new().unwrap();
        let user_dir = tmp.path().join("User");
        let transcript_dir = user_dir
            .join("workspaceStorage")
            .join("workspace-hash")
            .join("GitHub.copilot-chat")
            .join("transcripts");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let transcript = transcript_dir.join("sess-otel.jsonl");
        let mut f = std::fs::File::create(&transcript).unwrap();
        writeln!(f, r#"{{"type":"session.start"}}"#).unwrap();

        let otel_dir = user_dir.join("globalStorage").join("github.copilot-chat");
        std::fs::create_dir_all(&otel_dir).unwrap();
        let otel_db = otel_dir.join("agent-traces.db");
        std::fs::File::create(&otel_db).unwrap();
        let canonical_otel_db = std::fs::canonicalize(&otel_db).unwrap();

        let repo_work_dir = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_work_dir).unwrap();

        let db_path = tmp.path().join("test.db");
        let db = Arc::new(StreamsDatabase::open(&db_path).unwrap());
        let mut worker = make_worker(db.clone());

        let notification = CheckpointNotification {
            session_id: "internal-sess-otel".to_string(),
            tool: "github-copilot".to_string(),
            trace_id: "trace-otel".to_string(),
            tool_use_id: Some("tool-1".to_string()),
            stream_path: transcript,
            repo_work_dir: Some(repo_work_dir),
            external_session_id: "sess-otel".to_string(),
            external_parent_session_id: None,
        };

        worker.handle_checkpoint_notification(notification).await;

        let tasks: Vec<_> = worker.priority_queue.iter().collect();
        assert_eq!(tasks.len(), 2);
        let transcript_task = tasks
            .iter()
            .find(|task| task.stream_kind == "transcript")
            .unwrap();
        let otel_task = tasks
            .iter()
            .find(|task| task.stream_kind == "otel_traces")
            .unwrap();

        assert_eq!(transcript_task.priority, Priority::Immediate);
        assert_eq!(transcript_task.session_id, "internal-sess-otel");
        assert_eq!(otel_task.priority, Priority::Immediate);
        assert_eq!(
            otel_task.session_id,
            crate::streams::agent::SHARED_STREAM_SESSION_ID
        );
        assert_eq!(otel_task.canonical_path, canonical_otel_db);

        let otel_record = db
            .get_stream(
                crate::streams::agent::SHARED_STREAM_SESSION_ID,
                "otel_traces",
                &canonical_otel_db.display().to_string(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(otel_record.tool, "github-copilot");
        assert_eq!(otel_record.stream_format, "CopilotOtelSqlite");
        assert_eq!(otel_record.watermark_type, "TimestampCursor");
        assert_eq!(otel_record.external_session_id, "");
        assert_eq!(otel_record.external_parent_session_id, None);
        assert_eq!(otel_record.repo_work_dir, None);
    }

    #[test]
    fn test_sweep_subagents_deduplicates_in_flight() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let main_transcript = project_dir.join("sess-dup.jsonl");
        std::fs::File::create(&main_transcript).unwrap();

        let subagents_dir = project_dir.join("sess-dup").join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();
        let sub = subagents_dir.join("agent-inflight.jsonl");
        let mut f = std::fs::File::create(&sub).unwrap();
        writeln!(f, r#"{{"type":"message"}}"#).unwrap();

        let db_path = tmp.path().join("test.db");
        let db = Arc::new(StreamsDatabase::open(&db_path).unwrap());
        let mut worker = make_worker(db.clone());

        // Mark the subagent's canonical path as in-flight
        let canonical = std::fs::canonicalize(&sub).unwrap();
        worker
            .in_flight
            .insert((canonical, "transcript".to_string()));

        let notification = CheckpointNotification {
            session_id: "internal-sess-dup".to_string(),
            tool: "claude".to_string(),
            trace_id: "trace-4".to_string(),
            tool_use_id: None,
            stream_path: main_transcript,
            repo_work_dir: None,
            external_session_id: "sess-dup".to_string(),
            external_parent_session_id: None,
        };

        worker.sweep_subagents_for_session(&notification);

        // Should not enqueue the in-flight subagent
        assert_eq!(worker.priority_queue.len(), 0);
    }
}
