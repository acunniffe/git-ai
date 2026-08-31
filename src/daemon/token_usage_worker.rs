//! Token-usage worker: reads agent transcripts incrementally, aggregates
//! deduplicated token usage into 5-minute UTC buckets, and emits `TokenUsage`
//! metric events for buckets whose aggregate changed.
//!
//! Design notes:
//! - Everything runs off the trace2 ingestion path: work arrives as
//!   non-blocking notifications from the stream worker (after it processed a
//!   transcript), a 30-minute sweep ticker, and a startup sweep.
//! - Sweeps enumerate the streams database's `transcript` rows, so every
//!   session git-ai knows about is backfilled: a newly tracked file starts at
//!   byte offset 0 and its full history is bucketed on first processing.
//!   Sweep enumeration is cheap per session (one stat compared against the
//!   token database's size/mtime snapshot); per-file work — extractor state,
//!   repo discovery, reading — only happens for files that actually changed.
//! - Notifications and sweep backfill run on separate queues: the await
//!   drain barrier waits only for notification-driven work, so a first-start
//!   historical backfill can never starve `git-ai await`.
//! - The read cursor, extractor state, entries, and emission fingerprints all
//!   live in [`TokenUsageDatabase`]; each batch commits atomically, and
//!   emission reconciles fingerprints across all of the session's buckets, so
//!   there is no retry machinery here - a failed pass (or a crash at any
//!   point) is healed by the next notification or sweep, with a per-file
//!   error backoff so a permanently failing transcript is not re-read on
//!   every trigger. The size/mtime quiet-skip snapshot is written only after
//!   a fully successful pass.
//! - Claude subagent transcripts roll up to their parent session (matching
//!   ccusage), which also lets sidechain replays dedup against the parent's
//!   entries. Entry dedup itself is global across sessions (resume/fork
//!   copies); when a replacement moves an entry between sessions, the
//!   previous owner's files are invalidated so its buckets re-reconcile on
//!   the next pass with their own attributes.
//! - The `token_usage_metrics` feature flag gates the worker at daemon
//!   startup (like `transcript_streaming`); when it is off, the token-usage
//!   database is deleted so no collected data is retained.
//! - Sweep/backfill passes are CPU-throttled to a ~30% duty cycle of one
//!   core (each batch's work is followed by a proportional pause), so a
//!   large first-run backfill takes ~3x longer instead of pinning a core.
//!   Notification-driven passes are never throttled: they sit on the
//!   `git-ai await` drain barrier's post-commit critical path, and a task
//!   promoted from the sweep queue by a notification runs unthrottled too.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::{MissedTickBehavior, interval};

use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::daemon::telemetry_worker::DaemonTelemetryWorkerHandle;
use crate::error::GitAiError;
use crate::metrics::{EventAttributes, MetricEvent, PosEncoded, TokenUsageValues};
use crate::streams::db::{StreamRecord, StreamsDatabase};
use crate::token_usage::db::{BatchCommit, TokenUsageDatabase, TrackedFile};
use crate::token_usage::extractor::{ParentPrefixRequest, UsageSignature};
use crate::token_usage::{Speed, extractor_for_tool};

/// One raw JSONL line read as bytes: unlike UTF-8-strict `read_line`, a
/// single invalid byte cannot wedge the cursor forever (the line decodes
/// lossily, fails JSON parsing, is skipped, and the cursor advances —
/// matching upstream ccusage's byte-level reads).
enum LineRead {
    Eof,
    /// No trailing newline: the writer may still be appending.
    Partial(usize),
    Complete(usize),
}

fn read_line_bytes(
    reader: &mut impl std::io::BufRead,
    buf: &mut Vec<u8>,
) -> std::io::Result<LineRead> {
    buf.clear();
    let bytes = reader.read_until(b'\n', buf)?;
    if bytes == 0 {
        return Ok(LineRead::Eof);
    }
    if buf.last() != Some(&b'\n') {
        return Ok(LineRead::Partial(bytes));
    }
    Ok(LineRead::Complete(bytes))
}

/// Entry/byte bounds of one atomic batch commit.
const BATCH_MAX_ENTRIES: usize = 1_000;
const BATCH_MAX_BYTES: usize = 4 * 1024 * 1024;

/// CPU duty cycle for sweep-origin transcript passes: after each batch, the
/// pass sleeps long enough that the work occupies at most this share of one
/// core (work W is followed by a pause of W * (100 - duty) / duty). A multi-
/// gigabyte backfill therefore takes ~3x longer instead of pinning a core.
/// Notification-origin passes are never throttled (see `TaskOrigin`).
const THROTTLE_DUTY_PERCENT: u32 = 30;
/// Batches doing less work than this owe no pause (quiet skips, small
/// appends swept between notifications).
const THROTTLE_MIN_WORK: Duration = Duration::from_millis(5);
/// Upper bound on a single pause (guards pathologically slow batches, and
/// bounds how long a pass can delay a drain or shutdown).
const THROTTLE_MAX_PAUSE: Duration = Duration::from_secs(5);

/// The pause owed after `work` of batch processing under the duty cycle.
fn throttle_pause_for(work: Duration) -> Duration {
    if work < THROTTLE_MIN_WORK {
        return Duration::ZERO;
    }
    (work.saturating_mul(100 - THROTTLE_DUTY_PERCENT) / THROTTLE_DUTY_PERCENT)
        .min(THROTTLE_MAX_PAUSE)
}

/// Sleep out a throttle pause in small chunks so shutdown stays prompt.
fn throttle_sleep(pause: Duration, shutdown_flag: &AtomicBool) {
    let mut remaining = pause;
    while !remaining.is_zero() && !shutdown_flag.load(Ordering::Relaxed) {
        let chunk = remaining.min(Duration::from_millis(50));
        std::thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
}

/// Same telemetry-buffer backpressure as the stream worker.
const BACKPRESSURE_THRESHOLD: usize = 5_000;
const BACKPRESSURE_MAX_WAITS: usize = 40;

/// Entries older than this are neither stored nor emitted, and stored ones
/// are pruned on sweep: backfill must not upload history that the retention
/// prune would immediately delete.
pub(crate) const ENTRY_RETENTION_DAYS: u64 = 90;

/// A transcript file to (re)process, identified by its streams-db row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TokenUsageTask {
    session_id: String,
    tool: String,
    stream_path: String,
}

/// Which queue a task was popped from. Sweep/backfill passes are CPU-
/// throttled; notification-driven passes sit on the `git-ai await` drain
/// path and run unthrottled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskOrigin {
    Notify,
    Sweep,
}

struct DrainRequest {
    completion: tokio::sync::oneshot::Sender<()>,
}

/// Handle for feeding the worker.
#[derive(Clone)]
pub struct TokenUsageWorkerHandle {
    notify_tx: tokio::sync::mpsc::UnboundedSender<TokenUsageTask>,
    drain_tx: tokio::sync::mpsc::UnboundedSender<DrainRequest>,
}

impl TokenUsageWorkerHandle {
    /// Notify the worker that the stream worker finished processing a
    /// transcript (cheap, non-blocking; unsupported tools are dropped here).
    pub fn notify_stream_processed(&self, session_id: &str, tool: &str, stream_path: &Path) {
        if extractor_for_tool(tool).is_none() {
            return;
        }
        let _ = self.notify_tx.send(TokenUsageTask {
            session_id: session_id.to_string(),
            tool: tool.to_string(),
            stream_path: stream_path.display().to_string(),
        });
    }

    /// Wait until all notification-driven work has been processed. Sweep
    /// backfill intentionally continues in the background so a historical
    /// backfill cannot starve the await barrier.
    pub async fn drain(&self) -> Result<(), String> {
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        self.drain_tx
            .send(DrainRequest {
                completion: completion_tx,
            })
            .map_err(|_| "token-usage worker has stopped".to_string())?;
        completion_rx
            .await
            .map_err(|_| "token-usage worker drain was cancelled".to_string())
    }
}

/// Spawn the worker on the current tokio runtime.
pub fn spawn_token_usage_worker(
    streams_db: Arc<StreamsDatabase>,
    token_db: Arc<TokenUsageDatabase>,
    telemetry: DaemonTelemetryWorkerHandle,
    shutdown_notify: Arc<Notify>,
) -> TokenUsageWorkerHandle {
    let (notify_tx, notify_rx) = tokio::sync::mpsc::unbounded_channel();
    let (drain_tx, drain_rx) = tokio::sync::mpsc::unbounded_channel();
    let worker = TokenUsageWorker {
        streams_db,
        token_db,
        telemetry,
        shutdown_notify,
        shutdown_flag: Arc::new(AtomicBool::new(false)),
        notify_rx,
        drain_rx,
        notify_queue: VecDeque::new(),
        sweep_queue: VecDeque::new(),
        queued: HashSet::new(),
        sweep_interval: Duration::from_secs(30 * 60),
        #[cfg(test)]
        test_sink: None,
        #[cfg(test)]
        test_throttle: None,
    };
    tokio::spawn(async move {
        worker.run().await;
    });
    TokenUsageWorkerHandle {
        notify_tx,
        drain_tx,
    }
}

struct TokenUsageWorker {
    streams_db: Arc<StreamsDatabase>,
    token_db: Arc<TokenUsageDatabase>,
    telemetry: DaemonTelemetryWorkerHandle,
    shutdown_notify: Arc<Notify>,
    shutdown_flag: Arc<AtomicBool>,
    notify_rx: tokio::sync::mpsc::UnboundedReceiver<TokenUsageTask>,
    drain_rx: tokio::sync::mpsc::UnboundedReceiver<DrainRequest>,
    /// Notification-driven work: drained by the await barrier.
    notify_queue: VecDeque<TokenUsageTask>,
    /// Sweep/backfill work: processed only in the background loop.
    sweep_queue: VecDeque<TokenUsageTask>,
    queued: HashSet<TokenUsageTask>,
    /// 30 minutes in production; injectable so tests can drive the ticker.
    sweep_interval: Duration,
    /// Test-only event capture: the metrics DB is a process-lifetime
    /// singleton, so run()-level tests inject a sink instead of a real
    /// telemetry handle.
    #[cfg(test)]
    test_sink: Option<TestSink>,
    /// Test-only throttle-pause recorder, replacing the real sleep.
    #[cfg(test)]
    test_throttle: Option<TestThrottle>,
}

#[cfg(test)]
type TestSink = Arc<dyn Fn(&[MetricEvent]) -> Result<(), GitAiError> + Send + Sync>;
#[cfg(test)]
type TestThrottle = Arc<dyn Fn(Duration) + Send + Sync>;

impl TokenUsageWorker {
    /// The sink every task/reconcile pass hands emitted events to.
    fn make_sink(&self) -> impl Fn(&[MetricEvent]) -> Result<(), GitAiError> + Send + use<> {
        let telemetry = self.telemetry.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        #[cfg(test)]
        let test_sink = self.test_sink.clone();
        move |events: &[MetricEvent]| {
            #[cfg(test)]
            if let Some(sink) = &test_sink {
                return sink(events);
            }
            persist_events(&telemetry, &shutdown_flag, events)
        }
    }

    async fn run(mut self) {
        tracing::info!("token-usage worker started");
        let mut sweep_ticker = interval(self.sweep_interval);
        // After suspend/resume, one sweep covers everything; do not replay
        // every missed tick.
        sweep_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        sweep_ticker.tick().await; // skip the immediate tick

        self.prune_old_entries().await;
        self.enqueue_sweep_tasks().await;
        self.reconcile_flagged().await;

        loop {
            if self.shutdown_flag.load(Ordering::Relaxed) {
                break;
            }
            // Ready work is one select arm (the stream worker's pattern), so
            // drain requests, notifications, and shutdown are still serviced
            // between tasks while a long backfill queue is being worked off.
            let has_ready_task = !self.notify_queue.is_empty() || !self.sweep_queue.is_empty();
            tokio::select! {
                _ = self.shutdown_notify.notified() => {
                    self.shutdown_flag.store(true, Ordering::Relaxed);
                    break;
                }
                _ = async {}, if has_ready_task => {
                    // Ingest pending notifications first so fresh work is
                    // picked (and promoted over backfill) before the next
                    // task is chosen.
                    while let Ok(task) = self.notify_rx.try_recv() {
                        self.enqueue_notify(task);
                    }
                    if let Some((task, origin)) = self.pop_next() {
                        self.process_task(task, origin).await;
                    }
                }
                _ = sweep_ticker.tick() => {
                    self.prune_old_entries().await;
                    self.enqueue_sweep_tasks().await;
                    // Crash recovery: reconcile flags left by a pass that
                    // died between its batch commit and its reconcile step,
                    // even when no file traffic ever triggers a task again.
                    self.reconcile_flagged().await;
                }
                Some(task) = self.notify_rx.recv() => {
                    self.enqueue_notify(task);
                }
                Some(request) = self.drain_rx.recv() => {
                    self.handle_drain(request).await;
                }
            }
        }
        tracing::info!("token-usage worker shutdown complete");
    }

    async fn handle_drain(&mut self, request: DrainRequest) {
        // Consume notifications that were queued before the barrier, then
        // process only notification-driven work (not sweep backfill).
        while let Ok(task) = self.notify_rx.try_recv() {
            self.enqueue_notify(task);
        }
        while let Some((task, origin)) = self.pop_queue(true) {
            self.process_task(task, origin).await;
            if self.shutdown_flag.load(Ordering::Relaxed) {
                break;
            }
        }
        let _ = request.completion.send(());
    }

    fn enqueue_notify(&mut self, task: TokenUsageTask) {
        if self.queued.insert(task.clone()) {
            self.notify_queue.push_back(task);
        } else if let Some(pos) = self.sweep_queue.iter().position(|t| *t == task) {
            // Promote: fresh data trumps pending backfill of the same file.
            self.sweep_queue.remove(pos);
            self.notify_queue.push_back(task);
        }
    }

    fn enqueue_sweep(&mut self, task: TokenUsageTask) {
        if self.queued.insert(task.clone()) {
            self.sweep_queue.push_back(task);
        }
    }

    /// Pop the next task with its origin: notification-driven work (the
    /// `git-ai await` drain path — never throttled) is always preferred over
    /// sweep backfill (throttled). A task promoted from the sweep queue by a
    /// notification pops as notify-origin.
    fn pop_queue(&mut self, notify_only: bool) -> Option<(TokenUsageTask, TaskOrigin)> {
        if let Some(task) = self.notify_queue.pop_front() {
            self.queued.remove(&task);
            return Some((task, TaskOrigin::Notify));
        }
        if notify_only {
            return None;
        }
        let task = self.sweep_queue.pop_front()?;
        self.queued.remove(&task);
        Some((task, TaskOrigin::Sweep))
    }

    fn pop_next(&mut self) -> Option<(TokenUsageTask, TaskOrigin)> {
        self.pop_queue(false)
    }

    /// Retention prune, run once per sweep off the async loop.
    async fn prune_old_entries(&self) {
        let token_db = self.token_db.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let cutoff = retention_cutoff_bucket(now_secs());
            if let Err(e) = token_db.prune_buckets_before(cutoff) {
                tracing::warn!(error = %e, "token-usage retention prune failed");
            }
        })
        .await;
    }

    /// Enqueue supported transcripts the streams database knows about whose
    /// files changed since the last completed pass. The DB scans and per-file
    /// stats run in `spawn_blocking`, like all other I/O in this module.
    async fn enqueue_sweep_tasks(&mut self) {
        tracing::info!("token-usage sweep started");
        let streams_db = self.streams_db.clone();
        let token_db = self.token_db.clone();
        let tasks =
            match tokio::task::spawn_blocking(move || sweep_candidates(&streams_db, &token_db))
                .await
            {
                Ok(tasks) => tasks,
                Err(e) => {
                    tracing::error!(error = %e, "token-usage sweep panicked");
                    return;
                }
            };
        tracing::info!(discovered = tasks.len(), "token-usage sweep completed");
        for (index, task) in tasks.iter().enumerate().take(10) {
            tracing::info!(
                index,
                tool = %task.tool,
                session_id = %task.session_id,
                path = %task.stream_path,
                "token-usage sweep item: session"
            );
        }
        if tasks.len() > 10 {
            tracing::info!(
                remaining = tasks.len() - 10,
                "... and more token-usage sweep items"
            );
        }
        for task in tasks {
            self.enqueue_sweep(task);
        }
    }

    /// Reconcile cross-session flags off the async loop (used by sweeps for
    /// crash recovery; the per-task path reconciles inline).
    async fn reconcile_flagged(&self) {
        let token_db = self.token_db.clone();
        let sink = self.make_sink();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = reconcile_flagged_sessions(&token_db, &sink) {
                tracing::warn!(error = %e, "token-usage cross-session reconcile failed");
            }
        })
        .await;
    }

    /// The pause a sweep-origin pass sleeps after each batch. Tests inject a
    /// recorder to pin the per-batch call sites without wall-clock waits.
    fn make_throttle(&self) -> impl Fn(Duration) + Send + Sync + use<> {
        let shutdown_flag = self.shutdown_flag.clone();
        #[cfg(test)]
        let test_throttle = self.test_throttle.clone();
        move |pause: Duration| {
            #[cfg(test)]
            if let Some(throttle) = &test_throttle {
                throttle(pause);
                return;
            }
            throttle_sleep(pause, &shutdown_flag);
        }
    }

    async fn process_task(&mut self, task: TokenUsageTask, origin: TaskOrigin) {
        let streams_db = self.streams_db.clone();
        let token_db = self.token_db.clone();
        let sink = self.make_sink();
        let throttle_fn = self.make_throttle();
        let shutdown_flag = self.shutdown_flag.clone();
        let task_clone = task.clone();
        let mut handle = tokio::task::spawn_blocking(move || {
            // Only sweep/backfill passes are throttled: notification-driven
            // passes sit on the `git-ai await` drain barrier's critical path.
            let throttle: Option<&(dyn Fn(Duration) + Sync)> = match origin {
                TaskOrigin::Sweep => Some(&throttle_fn),
                TaskOrigin::Notify => None,
            };
            process_task_blocking(
                &streams_db,
                &token_db,
                &sink,
                throttle,
                &task_clone,
                &shutdown_flag,
            )
        });
        // Keep watching for shutdown while the blocking pass runs: setting
        // the flag makes the pass stop at its next line/batch boundary.
        let result = tokio::select! {
            result = &mut handle => result,
            _ = self.shutdown_notify.notified() => {
                self.shutdown_flag.store(true, Ordering::Relaxed);
                (&mut handle).await
            }
        };
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, session_id = %task.session_id, "token-usage processing failed");
            }
            Err(e) => {
                tracing::error!(error = %e, session_id = %task.session_id, "token-usage task panicked");
            }
        }
    }
}

/// New files have no snapshot yet and are always swept, which is what
/// backfills history for sessions tracked before this feature ran; settled
/// files (size/mtime match, nothing pending) and missing files are skipped
/// quietly.
fn sweep_candidates(
    streams_db: &StreamsDatabase,
    token_db: &TokenUsageDatabase,
) -> Vec<TokenUsageTask> {
    let streams = match streams_db.all_streams() {
        Ok(streams) => streams,
        Err(e) => {
            tracing::warn!(error = %e, "token-usage sweep: failed to list streams");
            return Vec::new();
        }
    };
    let tracked: HashMap<String, TrackedFile> = match token_db.all_files() {
        Ok(files) => files
            .into_iter()
            .map(|file| (file.stream_path.clone(), file))
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "token-usage sweep: failed to list tracked files");
            return Vec::new();
        }
    };
    let mut tasks = Vec::new();
    for stream in streams {
        if stream.stream_kind != "transcript" || extractor_for_tool(&stream.tool).is_none() {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&stream.stream_path) else {
            continue;
        };
        if let Some(file) = tracked.get(&stream.stream_path)
            && file.last_known_size as u64 == metadata.len()
            && file.last_modified == modified_secs(&metadata)
            && !file.pending_flush
        {
            continue;
        }
        tasks.push(TokenUsageTask {
            session_id: stream.session_id,
            tool: stream.tool,
            stream_path: stream.stream_path,
        });
    }
    tasks
}

/// Rollup identity of the session a transcript belongs to: subagent
/// transcripts attribute to their parent session (matching ccusage).
struct SessionIdentity {
    session_id: String,
    external_session_id: String,
    tool: String,
}

impl SessionIdentity {
    fn from_stream(stream: &StreamRecord) -> Self {
        let (session_id, external_session_id) = match &stream.external_parent_session_id {
            Some(parent_ext) => (
                generate_session_id(parent_ext, &stream.tool),
                parent_ext.clone(),
            ),
            None => (
                stream.session_id.clone(),
                stream.external_session_id.clone(),
            ),
        };
        Self {
            session_id,
            external_session_id,
            tool: stream.tool.clone(),
        }
    }
}

/// Resolve the repo_url for emission attributes: the stream's stored working
/// directory, falling back to the agent's cwd inference (persisted back like
/// the SessionEvent path does) so the repo exclude gate sees a repo_url
/// whenever one is resolvable.
fn resolve_repo_url_for_stream(
    streams_db: &StreamsDatabase,
    stream: &StreamRecord,
) -> Option<String> {
    let work_dir = stream
        .repo_work_dir
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| {
            let inferred = crate::streams::agent::get_agent(&stream.tool)?
                .infer_cwd(Path::new(&stream.stream_path))?;
            let _ = streams_db.update_repo_work_dir(
                &stream.session_id,
                &stream.stream_kind,
                &stream.stream_path,
                &inferred.display().to_string(),
            );
            Some(inferred)
        })?;
    crate::repo_url::resolve_repo_url_from_path(&work_dir)
}

/// Backoff between attempts for a file whose last pass failed, indexed by
/// consecutive error count.
fn error_backoff_secs(errors: i64) -> i64 {
    match errors {
        ..=0 => 0,
        1 => 5,
        2 => 30,
        3 => 300,
        _ => 1800,
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn modified_secs(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// Bucket cutoff below which entries are dropped instead of stored.
fn retention_cutoff_bucket(now: i64) -> u32 {
    now.saturating_sub((ENTRY_RETENTION_DAYS * 24 * 60 * 60) as i64)
        .clamp(0, u32::MAX as i64) as u32
}

fn process_task_blocking(
    streams_db: &StreamsDatabase,
    token_db: &TokenUsageDatabase,
    sink: &impl Fn(&[MetricEvent]) -> Result<(), GitAiError>,
    throttle: Option<&(dyn Fn(Duration) + Sync)>,
    task: &TokenUsageTask,
    shutdown_flag: &AtomicBool,
) -> Result<(), GitAiError> {
    let Some(stream) = streams_db
        .get_stream(&task.session_id, "transcript", &task.stream_path)
        .map_err(|e| GitAiError::Generic(format!("streams db read failed: {e}")))?
    else {
        return Ok(());
    };
    let identity = SessionIdentity::from_stream(&stream);
    let result = process_file(
        token_db,
        &identity,
        &task.stream_path,
        shutdown_flag,
        // Repo discovery is deferred until events are actually emitted; the
        // result is persisted so later DB-only corrections carry the same
        // repo gate attribute.
        || {
            let repo_url = resolve_repo_url_for_stream(streams_db, &stream);
            if let Some(url) = &repo_url {
                let _ = token_db.update_session_repo_url(&identity.session_id, url);
            }
            repo_url
        },
        sink,
        throttle,
    );
    if let Err(e) = &result {
        // Keyed by the rollup session id (subagent transcripts track under
        // their parent), matching the row ensure_file created.
        let _ = token_db.record_error(
            &identity.session_id,
            &task.stream_path,
            &e.to_string(),
            now_secs(),
        );
    }
    // Cross-session replacements flagged other sessions during the batch
    // commits: reconcile them now, DB-only. Failures here belong to the
    // flagged sessions (the durable flag retries them), NOT to this task's
    // transcript — charging them here would put a healthy file into error
    // backoff and point diagnostics at the wrong session.
    if result.is_ok()
        && let Err(e) = reconcile_flagged_sessions(token_db, sink)
    {
        tracing::warn!(error = %e, "token-usage cross-session reconcile failed; flags retained for retry");
    }
    result
}

/// Hand events to the telemetry queue, waiting out buffer backpressure
/// (same thresholds as the stream worker).
fn persist_events(
    telemetry: &DaemonTelemetryWorkerHandle,
    shutdown_flag: &AtomicBool,
    events: &[MetricEvent],
) -> Result<(), GitAiError> {
    for _ in 0..BACKPRESSURE_MAX_WAITS {
        if telemetry.metrics_buffer_len() < BACKPRESSURE_THRESHOLD
            || shutdown_flag.load(Ordering::Relaxed)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    telemetry.persist_metrics_blocking(events).map(|_| ())
}

/// Reconcile sessions flagged by cross-session replacements or the
/// migration purge. Emission is DB-only (aggregate + fingerprint compare),
/// so this works even after the flagged session's transcripts were deleted;
/// corrections carry the stored session identity and the repo_url the
/// session's events were last emitted with, so they face the same repo
/// exclude gate as the originals.
fn reconcile_flagged_sessions(
    token_db: &TokenUsageDatabase,
    sink: &impl Fn(&[MetricEvent]) -> Result<(), GitAiError>,
) -> Result<(), GitAiError> {
    for session in token_db.sessions_needing_reconcile()? {
        let identity = SessionIdentity {
            session_id: session.session_id.clone(),
            external_session_id: session.external_session_id,
            tool: session.tool,
        };
        emit_changed_buckets(token_db, &identity, || session.repo_url, sink)?;
        token_db.clear_needs_reconcile(&session.session_id)?;
    }
    Ok(())
}

/// Speed fallback from the rollout's codex-home `config.toml` (recorded
/// service tiers win; this covers unmarked entries). Cached by config mtime:
/// a pass over many rollouts stats the file once per rollout but reads and
/// parses it only when it changed — never per line.
fn codex_config_fallback_speed(stream_path: &Path) -> Option<Speed> {
    use std::sync::{Mutex, OnceLock};
    use std::time::SystemTime;
    type Cache = HashMap<PathBuf, (Option<SystemTime>, Option<Speed>)>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

    let config_path =
        crate::streams::model_extraction::codex_home_from_transcript_path(stream_path)?
            .join("config.toml");
    let modified = std::fs::metadata(&config_path)
        .ok()
        .and_then(|meta| meta.modified().ok());
    let cache = CACHE.get_or_init(Default::default);
    if let Ok(cache) = cache.lock()
        && let Some((cached_modified, speed)) = cache.get(&config_path)
        && *cached_modified == modified
    {
        return *speed;
    }
    let speed = crate::streams::model_extraction::read_codex_config(&config_path)
        .as_deref()
        .and_then(crate::token_usage::codex::config_fallback_speed);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(config_path, (modified, speed));
    }
    speed
}

/// Cap on directory entries visited when scanning the codex sessions tree
/// for a fork's parent rollout — far above a real `sessions/YYYY/MM/DD`
/// layout, so pathological trees can't wedge a pass.
const PARENT_SCAN_MAX_ENTRIES: usize = 50_000;

/// Locate and read a forked child's parent rollout, returning its pre-fork
/// usage prefix. Candidates come from the token database (files tracked
/// under the parent's external session id — which, for rolled-up children,
/// includes the child itself) and a bounded scan of the codex sessions tree
/// for filenames carrying the parent id; a candidate counts only when its
/// first line is a `session_meta` recording that id (ccusage locates parents
/// by recorded id, never by filename). `None` — the parent is unresolvable —
/// sends the extractor to its rewritten-burst fallback, like ccusage with an
/// unavailable parent log, and is not retried.
fn resolve_parent_prefix(
    token_db: &TokenUsageDatabase,
    tool: &str,
    child_path: &str,
    request: &ParentPrefixRequest,
) -> Option<Vec<UsageSignature>> {
    // The trait hook is provider-neutral, but everything below is codex
    // machinery (rollout layout, session_meta confirmation, codex delta
    // parsing). Another tool's request resolves to "parent unavailable"
    // (its burst fallback) instead of misparsing a foreign transcript as a
    // rollout; extend with per-tool dispatch when a second tool needs it.
    if tool != "codex" {
        return None;
    }
    let tracked = token_db
        .stream_paths_for_external_session(&request.parent_id, tool)
        .unwrap_or_default();
    let parent = tracked
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path != Path::new(child_path))
        .find(|path| records_session_id(path, &request.parent_id))
        .or_else(|| scan_sessions_tree(Path::new(child_path), &request.parent_id))?;
    let file = std::fs::File::open(&parent).ok()?;
    crate::token_usage::codex::collect_parent_prefix(
        BufReader::with_capacity(128 * 1024, file),
        request.fork_ts_ms,
    )
}

/// Whether the rollout's first line records `session_id` as its own id.
fn records_session_id(path: &Path, session_id: &str) -> bool {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // A rollout's session_meta line is small; a candidate whose first line
    // is larger is not one, and must not be buffered wholly into memory
    // (the scan feeds this any file whose name carries the parent id).
    const MAX_FIRST_LINE_BYTES: u64 = 64 * 1024;
    let mut line = Vec::new();
    let mut reader = BufReader::new(file.take(MAX_FIRST_LINE_BYTES));
    if read_line_bytes(&mut reader, &mut line).is_err() {
        return false;
    }
    // A line truncated at the cap is not valid JSON and fails the id parse.
    crate::token_usage::codex::session_meta_id(&String::from_utf8_lossy(&line)).as_deref()
        == Some(session_id)
}

/// Fallback parent discovery for rollouts the token database has not tracked
/// (yet): walk the codex home's `sessions` and `archived_sessions` trees for
/// a filename carrying the parent id (codex rollout filenames embed the
/// session uuid), confirming by the recorded `session_meta` id. The home is
/// resolved like `streams::model_extraction` (configured home, else the
/// nearest `.codex` ancestor) so archived or custom-layout children resolve
/// too; a `sessions`/`archived_sessions` ancestor is the fallback anchor.
fn scan_sessions_tree(child_path: &Path, parent_id: &str) -> Option<PathBuf> {
    let roots: Vec<PathBuf> =
        match crate::streams::model_extraction::codex_home_from_transcript_path(child_path) {
            Some(home) => vec![home.join("sessions"), home.join("archived_sessions")],
            None => {
                let anchor = child_path.ancestors().find(|dir| {
                    matches!(
                        dir.file_name().and_then(|name| name.to_str()),
                        Some("sessions" | "archived_sessions")
                    )
                })?;
                let parent = anchor.parent();
                vec![
                    anchor.to_path_buf(),
                    match parent {
                        Some(dir) if anchor.ends_with("sessions") => dir.join("archived_sessions"),
                        Some(dir) => dir.join("sessions"),
                        None => return None,
                    },
                ]
            }
        };
    let mut pending = roots;
    let mut visited = 0usize;
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > PARENT_SCAN_MAX_ENTRIES {
                return None;
            }
            let path = entry.path();
            // file_type() is free on the readdir result, unlike a stat per
            // entry; symlinked directories are deliberately not followed.
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(path);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(parent_id) && name.ends_with(".jsonl"))
                && path != child_path
                && records_session_id(&path, parent_id)
            {
                return Some(path);
            }
        }
    }
    None
}

/// Incrementally read one transcript file, persist deduplicated entries, and
/// emit changed buckets through `sink`. Split out (with injectable repo
/// resolution and sink) for direct testing without a daemon.
fn process_file(
    token_db: &TokenUsageDatabase,
    identity: &SessionIdentity,
    stream_path: &str,
    shutdown_flag: &AtomicBool,
    resolve_repo_url: impl FnOnce() -> Option<String>,
    sink: &impl Fn(&[MetricEvent]) -> Result<(), GitAiError>,
    throttle: Option<&(dyn Fn(Duration) + Sync)>,
) -> Result<(), GitAiError> {
    let tracked = token_db.ensure_file(
        &identity.session_id,
        stream_path,
        &identity.tool,
        &identity.external_session_id,
    )?;
    let now = now_secs();

    // Error backoff: a file whose last pass failed is not retried on every
    // trigger.
    if tracked.processing_errors > 0
        && let Some(last_error_at) = tracked.last_error_at
        && now < last_error_at.saturating_add(error_backoff_secs(tracked.processing_errors))
    {
        return Ok(());
    }

    let metadata = std::fs::metadata(stream_path)?;
    let size = metadata.len();
    let modified = modified_secs(&metadata);

    let Some(mut extractor) = extractor_for_tool(&identity.tool) else {
        return Ok(());
    };
    // A shrunken file was rewritten, and unreadable persisted state (corrupt
    // or cross-version) means the cursor position is meaningless for the
    // fresh extractor: both restart from scratch. Entry-level dedup keeps
    // re-extraction idempotent, whereas continuing mid-file on default state
    // would book the session's whole cumulative history as one delta.
    let mut offset = tracked.byte_offset;
    if offset > size {
        offset = 0;
    } else if let Some(state) = tracked.state_json.as_deref()
        && !extractor.restore_state(state)
    {
        tracing::warn!(session_id = %identity.session_id, "unreadable extractor state; re-reading from the start");
        offset = 0;
    }

    // Quiet skip: nothing changed since the last completed pass and the
    // extractor holds nothing to flush.
    if size == tracked.last_known_size as u64
        && modified == tracked.last_modified
        && !extractor.has_pending()
    {
        return Ok(());
    }

    // Only lines fed below consume the fallback speed, so quiet-skipped
    // passes never pay the config lookup.
    if identity.tool == "codex" {
        extractor.set_fallback_speed(codex_config_fallback_speed(Path::new(stream_path)));
    }

    // Serve a fork's parent-prefix request as soon as it appears — after
    // restoring persisted state here, and after every consumed line below —
    // so the parent's replayed history is known before the next usage event
    // is judged. A cheap no-op for everything but a freshly detected fork.
    let serve_parent_request = |extractor: &mut Box<dyn crate::token_usage::UsageExtractor>| {
        if let Some(request) = extractor.parent_request() {
            extractor.provide_parent_prefix(resolve_parent_prefix(
                token_db,
                &identity.tool,
                stream_path,
                &request,
            ));
        }
    };
    serve_parent_request(&mut extractor);

    let file = std::fs::File::open(stream_path)?;
    let mut reader = BufReader::with_capacity(128 * 1024, file);
    reader.seek(SeekFrom::Start(offset))?;

    let cutoff = retention_cutoff_bucket(now);
    let mut line: Vec<u8> = Vec::new();
    let mut reached_end = false;
    let mut interrupted = false;
    while !reached_end && !interrupted {
        let batch_started = std::time::Instant::now();
        let mut entries = Vec::new();
        let mut consumed = 0usize;
        loop {
            if shutdown_flag.load(Ordering::Relaxed) {
                interrupted = true;
                break;
            }
            match read_line_bytes(&mut reader, &mut line)? {
                LineRead::Eof => {
                    reached_end = true;
                    break;
                }
                // A trailing line without a newline is usually a write in
                // progress: leave the cursor before it. But if it already
                // parses as complete JSON it will never grow a newline once
                // the writer is gone, and the size/mtime snapshot would
                // suppress every later pass — count it now (upstream counts
                // unterminated final segments too).
                LineRead::Partial(bytes) => {
                    let text = String::from_utf8_lossy(&line);
                    let trimmed = text.trim();
                    if !trimmed.is_empty()
                        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
                    {
                        offset += bytes as u64;
                        if extractor.wants_line(trimmed) {
                            entries.extend(extractor.extract_line(trimmed));
                            serve_parent_request(&mut extractor);
                        }
                    }
                    reached_end = true;
                    break;
                }
                LineRead::Complete(bytes) => {
                    offset += bytes as u64;
                    consumed += bytes;
                    let text = String::from_utf8_lossy(&line);
                    let trimmed = text.trim_end();
                    if extractor.wants_line(trimmed) {
                        entries.extend(extractor.extract_line(trimmed));
                        serve_parent_request(&mut extractor);
                    }
                    if entries.len() >= BATCH_MAX_ENTRIES || consumed >= BATCH_MAX_BYTES {
                        break;
                    }
                }
            }
        }
        if reached_end {
            // End of the file: release buffered entries whose deferral
            // window has passed (e.g. a forked codex session's parked first
            // turn).
            entries.extend(extractor.flush(now.saturating_mul(1000)));
        }
        token_db.commit_batch(&BatchCommit {
            session_id: &identity.session_id,
            stream_path,
            entries: &entries,
            new_offset: offset,
            state_json: extractor.state_json().as_deref(),
            pending_flush: extractor.has_pending(),
            min_bucket_ts: cutoff,
        })?;
        // CPU throttle (sweep-origin passes only): pay for this batch's work
        // with a proportional pause (also after the final batch, so back-to-
        // back file passes during a large backfill hold the duty cycle too).
        if let Some(throttle) = throttle {
            throttle(throttle_pause_for(batch_started.elapsed()));
        }
    }

    if interrupted {
        // Shutdown mid-pass: entries and cursor are committed, and the
        // size/mtime snapshot stays stale, so the next pass resumes and
        // reconciles emission.
        return Ok(());
    }
    emit_changed_buckets(token_db, identity, resolve_repo_url, sink)?;
    // The quiet-skip snapshot is written only after emission succeeded, so a
    // failed hand-off (or a crash anywhere in this pass) leaves the file
    // "changed" and the next pass re-runs reconciliation.
    token_db.update_file_metadata(&identity.session_id, stream_path, size, modified)
}

/// Reconcile the session's buckets in one pass and emit those whose
/// fingerprint differs from the last emitted one, marking them emitted only
/// after the sink accepted the events. Repo discovery runs only when there
/// is something to emit.
fn emit_changed_buckets(
    token_db: &TokenUsageDatabase,
    identity: &SessionIdentity,
    resolve_repo_url: impl FnOnce() -> Option<String>,
    sink: &impl Fn(&[MetricEvent]) -> Result<(), GitAiError>,
) -> Result<(), GitAiError> {
    let changed = token_db.changed_buckets(&identity.session_id)?;
    if changed.is_empty() {
        return Ok(());
    }
    // Reserve the revisions BEFORE the sink: once a payload with revision
    // N+1 may exist in the metrics queue, that revision must never be
    // reused, or a crash before the fingerprint write would re-open the
    // equal-revision tie. A failed sink merely wastes a revision number.
    // The revision is floored to wall-clock seconds so that losing the
    // local revision state (flag off deletes the DB; the DB is rebuilt)
    // cannot restart below revisions the server has already seen — a
    // re-enabled backfill would otherwise re-emit at revision 1 and lose to
    // every previously uploaded value under the highest-revision upsert.
    let seq_floor = now_secs().max(0) as u64;
    let changed: Vec<(crate::token_usage::db::ChangedBucket, u64)> = changed
        .into_iter()
        .map(|bucket| {
            let next_seq = (bucket.emit_seq + 1).max(seq_floor);
            (bucket, next_seq)
        })
        .collect();
    let reservations: Vec<(String, u8, u32, u64)> = changed
        .iter()
        .map(|(bucket, next_seq)| {
            (
                bucket.model.clone(),
                bucket.speed,
                bucket.bucket_ts,
                *next_seq,
            )
        })
        .collect();
    token_db.reserve_emit_seqs(&identity.session_id, &reservations)?;
    let repo_url = resolve_repo_url();
    // The org-configured attribute map every event type carries (the
    // checkpoint and session emitters attach the same one).
    let config = crate::config::Config::fresh();
    let custom_attributes = config.custom_attributes();
    let mut events = Vec::with_capacity(changed.len());
    for (bucket, next_seq) in &changed {
        let aggregate = &bucket.aggregate;
        let values = TokenUsageValues::new()
            .bucket_ts(bucket.bucket_ts as u64)
            .input_tokens(aggregate.input)
            .output_tokens(aggregate.output)
            .cache_read_tokens(aggregate.cache_read)
            .cache_write_tokens(aggregate.cache_write)
            .total_tokens(aggregate.total)
            .reasoning_output_tokens_opt(aggregate.reasoning_output)
            .est_cost_micro_usd(aggregate.cost_micro_usd)
            .message_count(aggregate.message_count)
            // Strictly increasing per bucket: the server keeps the highest
            // revision, so same-second re-emissions cannot tie.
            .emitted_seq(*next_seq)
            .speed(bucket.speed as u32)
            .speed_inferred(aggregate.speed_inferred)
            .cache_write_1h_tokens(aggregate.cache_write_1h)
            .long_context_input_tokens(aggregate.long_context_input)
            .long_context_output_tokens(aggregate.long_context_output)
            .long_context_cache_read_tokens(aggregate.long_context_cache_read)
            .long_context_cache_write_tokens(aggregate.long_context_cache_write)
            .long_context_cache_write_1h_tokens(aggregate.long_context_cache_write_1h)
            .transcript_cost_micro_usd(aggregate.transcript_cost_micro_usd);
        let mut attrs = EventAttributes::with_version(env!("CARGO_PKG_VERSION"))
            .custom_attributes_map(custom_attributes)
            .session_id(identity.session_id.clone())
            .tool(&identity.tool)
            .model(&bucket.model);
        // Rows migrated from schema v1 have no recorded external id ('');
        // omit the attribute rather than emitting an empty string.
        if !identity.external_session_id.is_empty() {
            attrs = attrs.external_session_id(identity.external_session_id.clone());
        }
        if let Some(url) = &repo_url {
            attrs = attrs.repo_url(url.clone());
        }
        if let Some(catalog) = &aggregate.pricing_catalog {
            // A dedicated attribute: custom_attributes stays reserved for
            // the org-configured map every event type carries.
            attrs = attrs.pricing_catalog(catalog.clone());
        }
        events.push(MetricEvent::new(&values, attrs.to_sparse()));
    }
    sink(&events)?;
    let now_ts = now_secs();
    for (bucket, next_seq) in changed {
        token_db.mark_emitted(
            &identity.session_id,
            &bucket.model,
            bucket.speed,
            bucket.bucket_ts,
            &bucket.aggregate.fingerprint(),
            next_seq,
            now_ts,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::events::token_usage_pos;
    use std::fs;
    use std::sync::Mutex;

    fn identity() -> SessionIdentity {
        SessionIdentity {
            session_id: "s_test".to_string(),
            external_session_id: "ext-test".to_string(),
            tool: "claude".to_string(),
        }
    }

    fn setup() -> (tempfile::TempDir, TokenUsageDatabase, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db = TokenUsageDatabase::open(dir.path().join("token-usage-db")).unwrap();
        let transcript = dir.path().join("transcript.jsonl");
        (dir, db, transcript)
    }

    /// Stable anchor for fixture timestamps: yesterday's UTC midnight,
    /// computed once per process. Always in the past (no future-dated
    /// fixtures right after midnight), always inside the retention window,
    /// and never shifting between fixture creation and assertion when a test
    /// straddles a UTC midnight.
    fn fixture_base() -> i64 {
        static BASE: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
        *BASE.get_or_init(|| now_secs() - now_secs() % 86_400 - 86_400)
    }

    /// Recent RFC3339 timestamps so entries fall inside the retention window.
    fn recent_ts(minute: u32, second: u32) -> String {
        chrono::DateTime::from_timestamp(fixture_base() + (minute * 60 + second) as i64, 0)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    fn bucket_of(minute: u32, second: u32) -> u64 {
        let ts = fixture_base() as u64 + (minute * 60 + second) as u64;
        ts - ts % 300
    }

    fn claude_line(msg: &str, req: &str, ts: &str, output: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","sessionId":"ext-test","requestId":"{req}","message":{{"id":"{msg}","model":"claude-sonnet-4-20250514","usage":{{"input_tokens":100,"output_tokens":{output},"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#
        )
    }

    fn run_as(
        db: &TokenUsageDatabase,
        identity: &SessionIdentity,
        transcript: &std::path::Path,
    ) -> Result<Vec<MetricEvent>, GitAiError> {
        let collected = Mutex::new(Vec::new());
        let flag = AtomicBool::new(false);
        process_file(
            db,
            identity,
            &transcript.display().to_string(),
            &flag,
            || Some("https://github.com/acme/repo".to_string()),
            &|events| {
                collected.lock().unwrap().extend(events.to_vec());
                Ok(())
            },
            None,
        )?;
        Ok(collected.into_inner().unwrap())
    }

    fn run(
        db: &TokenUsageDatabase,
        transcript: &std::path::Path,
    ) -> Result<Vec<MetricEvent>, GitAiError> {
        run_as(db, &identity(), transcript)
    }

    fn value_u64(event: &MetricEvent, pos: usize) -> Option<u64> {
        event.values.get(&pos.to_string()).and_then(|v| v.as_u64())
    }

    #[test]
    fn processes_a_transcript_and_emits_bucket_events() {
        let (_dir, db, transcript) = setup();
        fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                claude_line("m1", "r1", &recent_ts(1, 0), 50),
                claude_line("m2", "r2", &recent_ts(6, 0), 70),
            ),
        )
        .unwrap();

        let events = run(&db, &transcript).unwrap();
        assert_eq!(events.len(), 2);
        let mut buckets: Vec<u64> = events
            .iter()
            .map(|e| value_u64(e, token_usage_pos::BUCKET_TS).unwrap())
            .collect();
        buckets.sort_unstable();
        assert_eq!(buckets, vec![bucket_of(1, 0), bucket_of(6, 0)]);
        for event in &events {
            assert_eq!(event.event_id, 9);
            assert_eq!(value_u64(event, token_usage_pos::INPUT_TOKENS), Some(100));
            assert_eq!(value_u64(event, token_usage_pos::MESSAGE_COUNT), Some(1));
            // Revisions are floored to wall-clock seconds so a rebuilt DB
            // can never restart below previously uploaded revisions.
            assert!(
                value_u64(event, token_usage_pos::EMITTED_SEQ).unwrap() >= now_secs() as u64 - 60
            );
            assert!(value_u64(event, token_usage_pos::EST_COST_MICRO_USD).unwrap() > 0);
            let attrs = EventAttributes::from_sparse(&event.attrs);
            assert_eq!(attrs.session_id, Some(Some("s_test".to_string())));
            assert_eq!(attrs.tool, Some(Some("claude".to_string())));
            assert_eq!(
                attrs.model,
                Some(Some("claude-sonnet-4-20250514".to_string()))
            );
            assert_eq!(
                attrs.repo_url,
                Some(Some("https://github.com/acme/repo".to_string()))
            );
        }
    }

    #[test]
    fn unchanged_file_emits_nothing_on_reprocess() {
        let (_dir, db, transcript) = setup();
        fs::write(
            &transcript,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 0), 50)),
        )
        .unwrap();
        assert_eq!(run(&db, &transcript).unwrap().len(), 1);
        // Size/mtime unchanged: skipped entirely.
        assert!(run(&db, &transcript).unwrap().is_empty());
    }

    #[test]
    fn appended_usage_reemits_the_bucket_with_bumped_revision() {
        let (_dir, db, transcript) = setup();
        fs::write(
            &transcript,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 0), 50)),
        )
        .unwrap();
        let first = run(&db, &transcript).unwrap();
        assert_eq!(
            value_u64(&first[0], token_usage_pos::OUTPUT_TOKENS),
            Some(50)
        );
        let first_seq = value_u64(&first[0], token_usage_pos::EMITTED_SEQ).unwrap();

        let mut content = fs::read_to_string(&transcript).unwrap();
        content.push_str(&claude_line("m2", "r2", &recent_ts(2, 0), 30));
        content.push('\n');
        fs::write(&transcript, content).unwrap();

        let second = run(&db, &transcript).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(
            value_u64(&second[0], token_usage_pos::OUTPUT_TOKENS),
            Some(80)
        );
        assert_eq!(
            value_u64(&second[0], token_usage_pos::MESSAGE_COUNT),
            Some(2)
        );
        assert!(value_u64(&second[0], token_usage_pos::EMITTED_SEQ).unwrap() > first_seq);
    }

    #[test]
    fn streaming_replacement_moving_buckets_reemits_zeroed_bucket() {
        let (_dir, db, transcript) = setup();
        fs::write(
            &transcript,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 0), 50)),
        )
        .unwrap();
        assert_eq!(run(&db, &transcript).unwrap().len(), 1);

        // The same message re-emits with larger totals in the next bucket:
        // the old bucket empties and must re-emit as zero exactly once.
        let mut content = fs::read_to_string(&transcript).unwrap();
        content.push_str(&claude_line("m1", "r1", &recent_ts(6, 0), 90));
        content.push('\n');
        fs::write(&transcript, content).unwrap();

        let events = run(&db, &transcript).unwrap();
        assert_eq!(events.len(), 2);
        let mut by_bucket: Vec<(u64, u64)> = events
            .iter()
            .map(|e| {
                (
                    value_u64(e, token_usage_pos::BUCKET_TS).unwrap(),
                    value_u64(e, token_usage_pos::TOTAL_TOKENS).unwrap(),
                )
            })
            .collect();
        by_bucket.sort_unstable();
        assert_eq!(by_bucket[0], (bucket_of(1, 0), 0));
        assert_eq!(by_bucket[1], (bucket_of(6, 0), 190));
        let zero_event = events
            .iter()
            .find(|e| value_u64(e, token_usage_pos::TOTAL_TOKENS) == Some(0))
            .unwrap();
        assert_eq!(
            value_u64(zero_event, token_usage_pos::MESSAGE_COUNT),
            Some(0)
        );
        assert!(
            value_u64(zero_event, token_usage_pos::EMITTED_SEQ).unwrap() >= now_secs() as u64 - 60
        );
    }

    #[test]
    fn partial_trailing_line_is_left_for_the_next_pass() {
        let (_dir, db, transcript) = setup();
        let complete = claude_line("m1", "r1", &recent_ts(1, 0), 50);
        let partial = claude_line("m2", "r2", &recent_ts(2, 0), 70);
        let partial_prefix = &partial[..partial.len() - 10];
        fs::write(&transcript, format!("{complete}\n{partial_prefix}")).unwrap();

        let events = run(&db, &transcript).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::MESSAGE_COUNT),
            Some(1)
        );

        // Writer finishes the line.
        fs::write(&transcript, format!("{complete}\n{partial}\n")).unwrap();
        let events = run(&db, &transcript).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::MESSAGE_COUNT),
            Some(2)
        );
    }

    #[test]
    fn invalid_utf8_lines_are_skipped_and_the_cursor_advances() {
        // A single invalid byte must not wedge the cursor forever (UTF-8
        // strict reads would error at the same offset on every pass and lose
        // all usage after that point).
        let (_dir, db, transcript) = setup();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(claude_line("m1", "r1", &recent_ts(1, 0), 50).as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(b"{\"garbage\": \"\xff\xfe broken\"}\n");
        bytes.extend_from_slice(claude_line("m2", "r2", &recent_ts(2, 0), 30).as_bytes());
        bytes.push(b'\n');
        fs::write(&transcript, bytes).unwrap();

        let events = run(&db, &transcript).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::MESSAGE_COUNT),
            Some(2),
            "usage after the invalid-UTF-8 line must still count"
        );
        // The cursor advanced past everything: the next pass is quiet.
        assert!(run(&db, &transcript).unwrap().is_empty());
    }

    #[test]
    fn complete_final_line_without_newline_is_counted() {
        // Agent killed between writing the JSON object and its newline: the
        // file never grows again, so the entry must be counted now instead
        // of being suppressed forever by the size/mtime snapshot.
        let (_dir, db, transcript) = setup();
        fs::write(
            &transcript,
            claude_line("m1", "r1", &recent_ts(1, 0), 50), // no trailing \n
        )
        .unwrap();
        let events = run(&db, &transcript).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::MESSAGE_COUNT),
            Some(1)
        );
        assert!(run(&db, &transcript).unwrap().is_empty());
    }

    #[test]
    fn truncated_final_json_stays_pending_until_completed() {
        // A half-written JSON object (not yet valid) is a write in progress:
        // the cursor stays before it and the completed line counts later.
        let (_dir, db, transcript) = setup();
        let full = claude_line("m1", "r1", &recent_ts(1, 0), 50);
        fs::write(&transcript, &full[..full.len() - 5]).unwrap();
        assert!(run(&db, &transcript).unwrap().is_empty());
        fs::write(&transcript, format!("{full}\n")).unwrap();
        assert_eq!(run(&db, &transcript).unwrap().len(), 1);
    }

    #[test]
    fn unreadable_state_resets_the_cursor_and_recounts_idempotently() {
        // Continuing mid-file on default codex state would book the whole
        // cumulative history as one fresh delta; a full re-read dedups by
        // entry identity instead.
        let (_dir, db, transcript) = setup();
        let identity = SessionIdentity {
            tool: "codex".to_string(),
            ..identity()
        };
        let token_count = |ts: String, total: u64| {
            format!(
                r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{total},"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":0,"total_tokens":{}}}}}}}}}"#,
                total + 10
            )
        };
        fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                token_count(recent_ts(1, 0), 100),
                token_count(recent_ts(6, 0), 300),
            ),
        )
        .unwrap();
        let events = run_as(&db, &identity, &transcript).unwrap();
        assert_eq!(events.len(), 2);

        // Corrupt the persisted state (e.g. a cross-version enum change) and
        // grow the file so the pass runs.
        let path = transcript.display().to_string();
        db.commit_batch(&BatchCommit {
            session_id: &identity.session_id,
            stream_path: &path,
            entries: &[],
            new_offset: fs::metadata(&transcript).unwrap().len(),
            state_json: Some("{\"kind\": \"from-the-future\""),
            pending_flush: false,
            min_bucket_ts: 0,
        })
        .unwrap();
        let mut content = fs::read_to_string(&transcript).unwrap();
        content.push_str(&token_count(recent_ts(11, 0), 350));
        content.push('\n');
        fs::write(&transcript, content).unwrap();

        // The re-read from offset 0 rebuilds identical entries (deduped) and
        // only the genuinely new turn emits; nothing double counts.
        let events = run_as(&db, &identity, &transcript).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::INPUT_TOKENS),
            Some(50)
        );
        let first_bucket = db
            .aggregate_bucket(&identity.session_id, "gpt-5", 0, bucket_of(1, 0) as u32)
            .unwrap();
        assert_eq!(first_bucket.message_count, 1, "no double count");
    }

    #[test]
    fn shrunken_file_re_reads_idempotently_from_scratch() {
        let (_dir, db, transcript) = setup();
        let l1 = claude_line("m1", "r1", &recent_ts(1, 0), 50);
        let l2 = claude_line("m2", "r2", &recent_ts(2, 0), 30);
        fs::write(&transcript, format!("{l1}\n{l2}\n")).unwrap();
        assert_eq!(run(&db, &transcript).unwrap().len(), 1);

        // The file is truncated back to one line (rewrite/rotation): the
        // cursor resets and the surviving entry re-extracts idempotently
        // (no aggregate change, no re-emission). The truncated-away entry
        // remains counted - a documented residual: entries have no per-file
        // ownership, and rollouts/transcripts are append-only in practice.
        fs::write(&transcript, format!("{l1}\n")).unwrap();
        let events = run(&db, &transcript).unwrap();
        assert!(events.is_empty(), "no aggregate change from the re-read");
        let agg = db
            .aggregate_bucket(
                "s_test",
                "claude-sonnet-4-20250514",
                0,
                bucket_of(1, 0) as u32,
            )
            .unwrap();
        assert_eq!(agg.message_count, 2);
    }

    #[test]
    fn missing_file_is_an_error() {
        let (_dir, db, transcript) = setup();
        assert!(run(&db, &transcript).is_err());
    }

    #[test]
    fn entries_older_than_retention_are_not_emitted() {
        let (_dir, db, transcript) = setup();
        fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                // 2020: far past the retention cutoff.
                claude_line("m1", "r1", "2020-01-01T00:01:00Z", 50),
                claude_line("m2", "r2", &recent_ts(1, 0), 70),
            ),
        )
        .unwrap();
        let events = run(&db, &transcript).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::BUCKET_TS),
            Some(bucket_of(1, 0))
        );
    }

    #[test]
    fn error_backoff_suppresses_immediate_retries() {
        let (_dir, db, transcript) = setup();
        let identity = identity();
        let path = transcript.display().to_string();
        // File is missing: the pass errors and the error is recorded (as the
        // task wrapper does).
        assert!(run_as(&db, &identity, &transcript).is_err());
        db.record_error(&identity.session_id, &path, "boom", now_secs())
            .unwrap();

        // The file now exists with real usage, but the backoff window makes
        // the next pass a quiet no-op instead of a retry.
        fs::write(
            &transcript,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 0), 50)),
        )
        .unwrap();
        assert!(run_as(&db, &identity, &transcript).unwrap().is_empty());

        // Once the window has passed (simulated by backdating the error),
        // processing resumes and a successful pass clears the error state.
        db.record_error(&identity.session_id, &path, "boom", now_secs() - 60)
            .unwrap();
        let events = run_as(&db, &identity, &transcript).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            db.ensure_file(&identity.session_id, &path, "claude", "ext-test")
                .unwrap()
                .processing_errors,
            0
        );
    }

    #[test]
    fn codex_reasoning_tokens_flow_through() {
        let (_dir, db, transcript) = setup();
        let identity = SessionIdentity {
            tool: "codex".to_string(),
            ..identity()
        };
        let token_count_line = format!(
            r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":50,"reasoning_output_tokens":10,"total_tokens":150}}}}}}}}"#,
            recent_ts(1, 0)
        );
        fs::write(
            &transcript,
            format!(
                "{}\n{token_count_line}\n",
                r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.1"}}"#,
            ),
        )
        .unwrap();

        let events = run_as(&db, &identity, &transcript).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::INPUT_TOKENS),
            Some(60)
        );
        assert_eq!(
            value_u64(&events[0], token_usage_pos::CACHE_READ_TOKENS),
            Some(40)
        );
        assert_eq!(
            value_u64(&events[0], token_usage_pos::REASONING_OUTPUT_TOKENS),
            Some(10)
        );
        let attrs = EventAttributes::from_sparse(&events[0].attrs);
        assert_eq!(attrs.model, Some(Some("gpt-5.1".to_string())));
    }

    #[test]
    fn forked_codex_single_turn_is_flushed_at_end_of_file() {
        // The lone parked turn of a forked session is released by the
        // end-of-file flush once the burst window has passed in wall-clock
        // time, instead of being undercounted forever.
        let (_dir, db, transcript) = setup();
        let identity = SessionIdentity {
            tool: "codex".to_string(),
            ..identity()
        };
        let token_count_line = format!(
            r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15}}}}}}}}"#,
            recent_ts(1, 0)
        );
        fs::write(
            &transcript,
            format!(
                "{}\n{token_count_line}\n",
                r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"child","forked_from_id":"parent"}}"#,
            ),
        )
        .unwrap();
        // recent_ts is in the past (>1s), so the flush releases the turn in
        // the same pass.
        let events = run_as(&db, &identity, &transcript).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::INPUT_TOKENS),
            Some(10)
        );
    }

    #[test]
    fn fast_and_standard_usage_emit_separate_bucket_events() {
        let (_dir, db, transcript) = setup();
        let fast_line = format!(
            r#"{{"timestamp":"{}","sessionId":"ext-test","requestId":"r1","costUSD":0.5,"message":{{"id":"m1","model":"claude-sonnet-4-20250514","usage":{{"input_tokens":100,"output_tokens":50,"speed":"fast","cache_creation":{{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20}}}}}}}}"#,
            recent_ts(1, 0)
        );
        fs::write(
            &transcript,
            format!(
                "{fast_line}\n{}\n",
                claude_line("m2", "r2", &recent_ts(1, 30), 50)
            ),
        )
        .unwrap();

        let mut events = run(&db, &transcript).unwrap();
        events.sort_by_key(|e| value_u64(e, token_usage_pos::SPEED));
        assert_eq!(events.len(), 2, "one event per speed, same model+bucket");
        for event in &events {
            assert_eq!(
                value_u64(event, token_usage_pos::BUCKET_TS),
                Some(bucket_of(1, 0))
            );
        }
        let standard = &events[0];
        assert_eq!(value_u64(standard, token_usage_pos::SPEED), Some(0));
        assert_eq!(
            value_u64(standard, token_usage_pos::SPEED_INFERRED),
            Some(1),
            "no usage.speed marker was recorded on the standard entry"
        );
        assert_eq!(
            value_u64(standard, token_usage_pos::TRANSCRIPT_COST_MICRO_USD),
            Some(0)
        );
        // Catalog-priced: the pricing catalog id rides its dedicated
        // attribute (custom_attributes stays free for the org map).
        let attrs = EventAttributes::from_sparse(&standard.attrs);
        assert_eq!(
            attrs.pricing_catalog.flatten().as_deref(),
            Some(crate::metrics::model_pricing::pricing_catalog_id())
        );
        assert_eq!(attrs.custom_attributes, None);

        let fast = &events[1];
        assert_eq!(value_u64(fast, token_usage_pos::SPEED), Some(1));
        assert_eq!(
            value_u64(fast, token_usage_pos::CACHE_WRITE_TOKENS),
            Some(30)
        );
        assert_eq!(
            value_u64(fast, token_usage_pos::CACHE_WRITE_1H_TOKENS),
            Some(20)
        );
        // costUSD 0.5 -> the whole cost is transcript-provided; no catalog.
        assert_eq!(
            value_u64(fast, token_usage_pos::TRANSCRIPT_COST_MICRO_USD),
            Some(500_000)
        );
        assert_eq!(
            value_u64(fast, token_usage_pos::EST_COST_MICRO_USD),
            Some(500_000)
        );
        let attrs = EventAttributes::from_sparse(&fast.attrs);
        assert!(attrs.pricing_catalog.flatten().is_none());
    }

    fn codex_meta_line(id: &str, forked_from: Option<&str>, ts: &str) -> String {
        let fork = forked_from
            .map(|parent| format!(r#","forked_from_id":"{parent}""#))
            .unwrap_or_default();
        format!(r#"{{"timestamp":"{ts}","type":"session_meta","payload":{{"id":"{id}"{fork}}}}}"#)
    }

    fn codex_usage_line(ts: &str, totals: (u64, u64, u64, u64, u64)) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{},"cached_input_tokens":{},"output_tokens":{},"reasoning_output_tokens":{},"total_tokens":{}}}}}}}}}"#,
            totals.0, totals.1, totals.2, totals.3, totals.4
        )
    }

    #[test]
    fn forked_codex_replay_is_skipped_via_the_tracked_parent() {
        // The child's leading event replays the parent's history with a
        // rewritten timestamp and its own turn follows 30s later — far past
        // the burst window, so only parent-prefix matching can tell the
        // replay apart (and the rewritten timestamp gives it a fresh
        // content-derived key, so dedup alone would double count).
        let (dir, db, _) = setup();
        let identity = SessionIdentity {
            tool: "codex".to_string(),
            external_session_id: "parent-ext".to_string(),
            ..identity()
        };
        let parent_path = dir.path().join("parent.jsonl");
        fs::write(
            &parent_path,
            format!(
                "{}\n{}\n",
                codex_meta_line("parent-ext", None, &recent_ts(0, 30)),
                codex_usage_line(&recent_ts(1, 0), (100, 40, 50, 10, 150)),
            ),
        )
        .unwrap();
        assert_eq!(run_as(&db, &identity, &parent_path).unwrap().len(), 1);

        let child_path = dir.path().join("child.jsonl");
        fs::write(
            &child_path,
            format!(
                "{}\n{}\n{}\n",
                codex_meta_line("child-ext", Some("parent-ext"), &recent_ts(2, 0)),
                codex_usage_line(&recent_ts(2, 0), (100, 40, 50, 10, 150)),
                codex_usage_line(&recent_ts(2, 30), (300, 140, 90, 30, 390)),
            ),
        )
        .unwrap();
        let events = run_as(&db, &identity, &child_path).unwrap();
        assert_eq!(events.len(), 1);
        // Parent's 60 + the child's own 100 — without the 60 the replayed
        // copy would have re-added.
        assert_eq!(
            value_u64(&events[0], token_usage_pos::INPUT_TOKENS),
            Some(160)
        );
        assert_eq!(
            value_u64(&events[0], token_usage_pos::MESSAGE_COUNT),
            Some(2)
        );
    }

    #[test]
    fn forked_codex_parent_is_found_by_scanning_the_sessions_tree() {
        // The parent rollout exists on disk but was never tracked (the child
        // is processed first): resolution falls back to scanning the codex
        // sessions tree and confirming the recorded session id.
        let (dir, db, _) = setup();
        let day_dir = dir.path().join("sessions/2026/01/09");
        fs::create_dir_all(&day_dir).unwrap();
        fs::write(
            day_dir.join("rollout-parent-ext.jsonl"),
            format!(
                "{}\n{}\n",
                codex_meta_line("parent-ext", None, &recent_ts(0, 30)),
                codex_usage_line(&recent_ts(1, 0), (100, 40, 50, 10, 150)),
            ),
        )
        .unwrap();
        let child_path = day_dir.join("rollout-child-ext.jsonl");
        fs::write(
            &child_path,
            format!(
                "{}\n{}\n{}\n",
                codex_meta_line("child-ext", Some("parent-ext"), &recent_ts(2, 0)),
                codex_usage_line(&recent_ts(2, 0), (100, 40, 50, 10, 150)),
                codex_usage_line(&recent_ts(2, 30), (300, 140, 90, 30, 390)),
            ),
        )
        .unwrap();

        let identity = SessionIdentity {
            tool: "codex".to_string(),
            external_session_id: "parent-ext".to_string(),
            ..identity()
        };
        let events = run_as(&db, &identity, &child_path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::INPUT_TOKENS),
            Some(100),
            "only the child's own delta counts"
        );
    }

    #[test]
    fn forked_codex_parent_is_found_under_archived_sessions() {
        // Codex moves archived rollouts to `archived_sessions`, a sibling of
        // `sessions`: a fork whose parent was archived must still resolve
        // instead of falling back to the burst heuristic.
        let (dir, db, _) = setup();
        let archived_dir = dir.path().join("archived_sessions/2026/01/08");
        fs::create_dir_all(&archived_dir).unwrap();
        fs::write(
            archived_dir.join("rollout-parent-ext.jsonl"),
            format!(
                "{}\n{}\n",
                codex_meta_line("parent-ext", None, &recent_ts(0, 30)),
                codex_usage_line(&recent_ts(1, 0), (100, 40, 50, 10, 150)),
            ),
        )
        .unwrap();
        let day_dir = dir.path().join("sessions/2026/01/09");
        fs::create_dir_all(&day_dir).unwrap();
        let child_path = day_dir.join("rollout-child-ext.jsonl");
        fs::write(
            &child_path,
            format!(
                "{}\n{}\n{}\n",
                codex_meta_line("child-ext", Some("parent-ext"), &recent_ts(2, 0)),
                codex_usage_line(&recent_ts(2, 0), (100, 40, 50, 10, 150)),
                codex_usage_line(&recent_ts(2, 30), (300, 140, 90, 30, 390)),
            ),
        )
        .unwrap();

        let identity = SessionIdentity {
            tool: "codex".to_string(),
            external_session_id: "parent-ext".to_string(),
            ..identity()
        };
        let events = run_as(&db, &identity, &child_path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::INPUT_TOKENS),
            Some(100),
            "only the child's own delta counts"
        );
    }

    #[test]
    fn codex_config_service_tier_is_the_fallback_for_unmarked_entries() {
        // A rollout under a codex home whose config.toml requests the fast
        // tier: unmarked entries resolve to fast (inferred), while a recorded
        // tier still wins.
        let (dir, db, _) = setup();
        let codex_home = dir.path().join(".codex");
        fs::create_dir_all(codex_home.join("sessions")).unwrap();
        fs::write(
            codex_home.join("config.toml"),
            "model = \"gpt-5.1\"\nservice_tier = \"fast\" # priority lane\n",
        )
        .unwrap();
        let transcript = codex_home.join("sessions").join("rollout-test.jsonl");
        fs::write(
            &transcript,
            format!(
                "{}\n{}\n{}\n",
                codex_usage_line(&recent_ts(1, 0), (100, 40, 50, 10, 150)),
                r#"{"timestamp":"2026-01-01T00:02:00Z","type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{"service_tier":"standard"}}}"#,
                codex_usage_line(&recent_ts(3, 0), (300, 140, 90, 30, 390)),
            ),
        )
        .unwrap();
        let identity = SessionIdentity {
            tool: "codex".to_string(),
            ..identity()
        };
        run_as(&db, &identity, &transcript).unwrap();

        let conn =
            crate::sqlite::open_with_memory_limits(dir.path().join("token-usage-db")).unwrap();
        // Both turns land in the same 5-minute bucket, so order by insertion
        // (file) order rather than the tying bucket_ts.
        let rows: Vec<(u64, i64, bool)> = conn
            .prepare("SELECT input_tokens, speed, speed_inferred FROM usage_entries ORDER BY rowid")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // First turn: unmarked -> config fast, inferred. Second: recorded
        // standard wins, not inferred.
        assert_eq!(rows, vec![(60, 1, true), (100, 0, false)]);
    }

    #[test]
    fn cross_session_replacement_reconciles_the_previous_owner_without_its_file() {
        let (dir, db, transcript_a) = setup();
        let identity_a = identity();
        fs::write(
            &transcript_a,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 0), 50)),
        )
        .unwrap();
        assert_eq!(run_as(&db, &identity_a, &transcript_a).unwrap().len(), 1);
        // Production persists the emission repo_url per session; corrections
        // must carry it so they face the same repo exclude gate.
        db.update_session_repo_url(&identity_a.session_id, "https://github.com/acme/repo")
            .unwrap();
        // Session A's transcript disappears (e.g. Claude Code pruned it):
        // reconciliation of A must not depend on the file.
        fs::remove_file(&transcript_a).unwrap();

        // A resumed session copies the message with larger totals: the entry
        // moves to the new session, and the previous owner is durably
        // flagged for reconciliation.
        let identity_b = SessionIdentity {
            session_id: "s_resumed".to_string(),
            external_session_id: "ext-resumed".to_string(),
            tool: "claude".to_string(),
        };
        let transcript_b = dir.path().join("resumed.jsonl");
        fs::write(
            &transcript_b,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 30), 90)),
        )
        .unwrap();
        let events = run_as(&db, &identity_b, &transcript_b).unwrap();
        assert_eq!(events.len(), 1);
        let attrs = EventAttributes::from_sparse(&events[0].attrs);
        assert_eq!(attrs.session_id, Some(Some("s_resumed".to_string())));
        assert_eq!(db.sessions_needing_reconcile().unwrap().len(), 1);

        // The DB-only reconcile pass emits A's emptied bucket as zero with
        // A's stored identity, no file read required, and clears the flag.
        let collected = Mutex::new(Vec::new());
        reconcile_flagged_sessions(&db, &|events: &[MetricEvent]| {
            collected.lock().unwrap().extend(events.to_vec());
            Ok(())
        })
        .unwrap();
        let events = collected.into_inner().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::TOTAL_TOKENS),
            Some(0)
        );
        let attrs = EventAttributes::from_sparse(&events[0].attrs);
        assert_eq!(attrs.session_id, Some(Some("s_test".to_string())));
        assert_eq!(
            attrs.external_session_id,
            Some(Some("ext-test".to_string()))
        );
        assert_eq!(
            attrs.repo_url,
            Some(Some("https://github.com/acme/repo".to_string())),
            "corrections must carry the stored repo_url for the exclude gate"
        );
        assert!(db.sessions_needing_reconcile().unwrap().is_empty());
    }

    #[test]
    fn failed_reconcile_sink_keeps_the_flag_for_retry() {
        let (dir, db, transcript_a) = setup();
        let identity_a = identity();
        fs::write(
            &transcript_a,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 0), 50)),
        )
        .unwrap();
        assert_eq!(run_as(&db, &identity_a, &transcript_a).unwrap().len(), 1);

        let identity_b = SessionIdentity {
            session_id: "s_resumed".to_string(),
            external_session_id: "ext-resumed".to_string(),
            tool: "claude".to_string(),
        };
        let transcript_b = dir.path().join("resumed.jsonl");
        fs::write(
            &transcript_b,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 30), 90)),
        )
        .unwrap();
        run_as(&db, &identity_b, &transcript_b).unwrap();
        assert_eq!(db.sessions_needing_reconcile().unwrap().len(), 1);

        // The flag survives a failed reconcile sink...
        assert!(
            reconcile_flagged_sessions(&db, &|_: &[MetricEvent]| {
                Err(GitAiError::Generic("sink down".to_string()))
            })
            .is_err()
        );
        assert_eq!(db.sessions_needing_reconcile().unwrap().len(), 1);

        // ...and the retry emits the correction and clears it.
        let collected = Mutex::new(Vec::new());
        reconcile_flagged_sessions(&db, &|events: &[MetricEvent]| {
            collected.lock().unwrap().extend(events.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(collected.into_inner().unwrap().len(), 1);
        assert!(db.sessions_needing_reconcile().unwrap().is_empty());
    }

    #[test]
    fn resumed_session_copy_is_not_double_counted() {
        let (dir, db, transcript_a) = setup();
        let identity_a = identity();
        fs::write(
            &transcript_a,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 0), 50)),
        )
        .unwrap();
        assert_eq!(run_as(&db, &identity_a, &transcript_a).unwrap().len(), 1);

        // The resumed file carries an identical copy: nothing new to emit.
        let identity_b = SessionIdentity {
            session_id: "s_resumed".to_string(),
            external_session_id: "ext-resumed".to_string(),
            tool: "claude".to_string(),
        };
        let transcript_b = dir.path().join("resumed.jsonl");
        fs::write(
            &transcript_b,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 0), 50)),
        )
        .unwrap();
        assert!(run_as(&db, &identity_b, &transcript_b).unwrap().is_empty());
    }

    #[test]
    fn failed_sink_leaves_bucket_unmarked_for_retry() {
        let (_dir, db, transcript) = setup();
        fs::write(
            &transcript,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 0), 50)),
        )
        .unwrap();
        let flag = AtomicBool::new(false);
        let result = process_file(
            &db,
            &identity(),
            &transcript.display().to_string(),
            &flag,
            || None,
            &|_| Err(GitAiError::Generic("sink down".to_string())),
            None,
        );
        assert!(result.is_err());

        // Entries and cursor were committed, but the bucket was not marked
        // emitted and the size/mtime snapshot was not written: the next pass
        // over the *unchanged* file reconciles and emits it.
        let events = run(&db, &transcript).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::MESSAGE_COUNT),
            Some(1)
        );
        // And a further pass stays quiet.
        assert!(run(&db, &transcript).unwrap().is_empty());
    }

    fn stream_record(session_id: &str, tool: &str, path: &str) -> StreamRecord {
        StreamRecord {
            session_id: session_id.to_string(),
            stream_kind: "transcript".to_string(),
            tool: tool.to_string(),
            stream_path: path.to_string(),
            stream_format: "ClaudeJsonl".to_string(),
            watermark_type: "ByteOffset".to_string(),
            watermark_value: "0".to_string(),
            external_session_id: format!("{session_id}-ext"),
            external_parent_session_id: None,
            first_seen_at: 0,
            last_processed_at: 0,
            last_known_size: 0,
            last_modified: None,
            processing_errors: 0,
            last_error: None,
            repo_work_dir: None,
        }
    }

    fn test_worker(
        streams_db: Arc<StreamsDatabase>,
        token_db: Arc<TokenUsageDatabase>,
    ) -> TokenUsageWorker {
        let (_notify_tx, notify_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_drain_tx, drain_rx) = tokio::sync::mpsc::unbounded_channel();
        TokenUsageWorker {
            streams_db,
            token_db,
            telemetry: DaemonTelemetryWorkerHandle::new_noop(),
            shutdown_notify: Arc::new(Notify::new()),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            notify_rx,
            drain_rx,
            notify_queue: VecDeque::new(),
            sweep_queue: VecDeque::new(),
            queued: HashSet::new(),
            sweep_interval: Duration::from_secs(30 * 60),
            test_sink: None,
            test_throttle: None,
        }
    }

    #[tokio::test]
    async fn sweep_enqueues_only_changed_supported_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let streams_db =
            Arc::new(StreamsDatabase::open(dir.path().join("transcripts-db")).unwrap());
        let token_db =
            Arc::new(TokenUsageDatabase::open(dir.path().join("token-usage-db")).unwrap());
        let changed_path = dir.path().join("changed.jsonl");
        fs::write(&changed_path, "{}\n").unwrap();
        let changed_path = changed_path.display().to_string();
        let settled_path = dir.path().join("settled.jsonl");
        fs::write(&settled_path, "{}\n").unwrap();
        let settled_path = settled_path.display().to_string();

        streams_db
            .insert_stream(&stream_record("s_changed", "claude", &changed_path))
            .unwrap();
        streams_db
            .insert_stream(&stream_record("s_settled", "claude", &settled_path))
            .unwrap();
        // Unsupported tool, missing file, and non-transcript kinds skipped.
        streams_db
            .insert_stream(&stream_record("s_gem", "gemini", &changed_path))
            .unwrap();
        streams_db
            .insert_stream(&stream_record("s_gone", "claude", "/definitely/gone.jsonl"))
            .unwrap();
        let mut otel = stream_record("s_otel", "claude", &changed_path);
        otel.stream_kind = "otel_traces".to_string();
        streams_db.insert_stream(&otel).unwrap();

        // The settled file's snapshot matches its current metadata.
        let metadata = fs::metadata(&settled_path).unwrap();
        token_db
            .ensure_file("s_settled", &settled_path, "claude", "s_settled-ext")
            .unwrap();
        token_db
            .update_file_metadata(
                "s_settled",
                &settled_path,
                metadata.len(),
                modified_secs(&metadata),
            )
            .unwrap();

        let candidates = sweep_candidates(&streams_db, &token_db);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "s_changed");

        // Enqueue dedup: repeated sweeps don't re-add queued tasks.
        let mut worker = test_worker(streams_db.clone(), token_db.clone());
        for task in sweep_candidates(&streams_db, &token_db) {
            worker.enqueue_sweep(task);
        }
        for task in sweep_candidates(&streams_db, &token_db) {
            worker.enqueue_sweep(task);
        }
        assert_eq!(worker.sweep_queue.len(), 1);
        assert!(worker.notify_queue.is_empty());
    }

    #[tokio::test]
    async fn notifications_promote_queued_sweep_tasks_and_drain_skips_backfill() {
        let dir = tempfile::tempdir().unwrap();
        let streams_db =
            Arc::new(StreamsDatabase::open(dir.path().join("transcripts-db")).unwrap());
        let token_db =
            Arc::new(TokenUsageDatabase::open(dir.path().join("token-usage-db")).unwrap());
        let mut worker = test_worker(streams_db, token_db);

        let backfill = TokenUsageTask {
            session_id: "s_backfill".to_string(),
            tool: "claude".to_string(),
            stream_path: "/a.jsonl".to_string(),
        };
        let fresh = TokenUsageTask {
            session_id: "s_fresh".to_string(),
            tool: "claude".to_string(),
            stream_path: "/b.jsonl".to_string(),
        };
        worker.enqueue_sweep(backfill.clone());
        worker.enqueue_sweep(fresh.clone());
        // A notification for a file already queued for backfill promotes it.
        worker.enqueue_notify(fresh.clone());
        assert_eq!(worker.sweep_queue.len(), 1);
        assert_eq!(worker.notify_queue.len(), 1);

        // The drain barrier only sees notification-driven work, and a
        // promoted task pops as notify-origin: it runs unthrottled.
        assert_eq!(worker.pop_queue(true), Some((fresh, TaskOrigin::Notify)));
        assert_eq!(worker.pop_queue(true), None);
        // The background loop still gets the backfill, throttled.
        assert_eq!(worker.pop_next(), Some((backfill, TaskOrigin::Sweep)));
        assert_eq!(worker.pop_next(), None);
        assert!(worker.queued.is_empty());
    }

    #[tokio::test]
    async fn processing_errors_are_recorded_under_the_rollup_session() {
        // Subagent transcripts track under their parent session id, so error
        // recording must use the same key or the UPDATE matches no row.
        let dir = tempfile::tempdir().unwrap();
        let streams_db =
            Arc::new(StreamsDatabase::open(dir.path().join("transcripts-db")).unwrap());
        let token_db =
            Arc::new(TokenUsageDatabase::open(dir.path().join("token-usage-db")).unwrap());
        let missing = dir.path().join("gone.jsonl").display().to_string();
        let mut record = stream_record("s_child", "claude", &missing);
        record.external_parent_session_id = Some("parent-ext".to_string());
        streams_db.insert_stream(&record).unwrap();

        let task = TokenUsageTask {
            session_id: "s_child".to_string(),
            tool: "claude".to_string(),
            stream_path: missing.clone(),
        };
        let result = process_task_blocking(
            &streams_db,
            &token_db,
            &|_: &[MetricEvent]| Ok(()),
            None,
            &task,
            &AtomicBool::new(false),
        );
        assert!(result.is_err());

        let parent_session = generate_session_id("parent-ext", "claude");
        let tracked = token_db
            .ensure_file(&parent_session, &missing, "claude", "parent-ext")
            .unwrap();
        assert_eq!(tracked.processing_errors, 1);
        assert!(tracked.last_error_at.is_some());
        assert_eq!(token_db.all_files().unwrap().len(), 1);
    }

    #[test]
    fn subagent_stream_rolls_up_to_parent_session() {
        let mut stream = stream_record("s_child", "claude", "/tmp/child.jsonl");
        stream.external_session_id = "child-ext".to_string();
        stream.external_parent_session_id = Some("parent-ext".to_string());
        let identity = SessionIdentity::from_stream(&stream);
        assert_eq!(
            identity.session_id,
            generate_session_id("parent-ext", "claude")
        );
        assert_eq!(identity.external_session_id, "parent-ext");

        stream.external_parent_session_id = None;
        let identity = SessionIdentity::from_stream(&stream);
        assert_eq!(identity.session_id, "s_child");
        assert_eq!(identity.external_session_id, "child-ext");
    }

    #[test]
    fn throttle_pause_holds_the_duty_cycle() {
        // Below the floor: not throttled (quiet skips, small appends), so
        // notification-driven work and the drain barrier stay fast.
        assert_eq!(throttle_pause_for(Duration::from_millis(4)), Duration::ZERO);
        // 30% duty cycle: work W is followed by a pause of 7W/3.
        assert_eq!(
            throttle_pause_for(Duration::from_millis(300)),
            Duration::from_millis(700)
        );
        assert_eq!(
            throttle_pause_for(Duration::from_millis(900)),
            Duration::from_millis(2100)
        );
        // Capped, so one pathologically slow batch cannot stall a drain or
        // shutdown for minutes.
        assert_eq!(
            throttle_pause_for(Duration::from_secs(60)),
            THROTTLE_MAX_PAUSE
        );
    }

    #[test]
    fn throttle_sleep_exits_promptly_on_shutdown() {
        let flag = AtomicBool::new(true);
        let started = std::time::Instant::now();
        throttle_sleep(Duration::from_secs(5), &flag);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn throttled_passes_pay_a_pause_per_batch_and_quiet_skips_pay_nothing() {
        // Pins the call site (finding: deleting the throttle call failed no
        // test): a throttled multi-batch pass owes exactly one pause per
        // batch commit, recorded via the injectable sleeper — no wall-clock
        // assertions needed.
        let (_dir, db, transcript) = setup();
        let mut content = String::new();
        for i in 0..(BATCH_MAX_ENTRIES + 1) {
            content.push_str(&claude_line(
                &format!("m{i}"),
                &format!("r{i}"),
                &recent_ts(1, 0),
                50,
            ));
            content.push('\n');
        }
        fs::write(&transcript, content).unwrap();

        let pauses = Mutex::new(Vec::new());
        let recorder = |pause: Duration| pauses.lock().unwrap().push(pause);
        let flag = AtomicBool::new(false);
        process_file(
            &db,
            &identity(),
            &transcript.display().to_string(),
            &flag,
            || None,
            &|_| Ok(()),
            Some(&recorder),
        )
        .unwrap();
        assert_eq!(pauses.lock().unwrap().len(), 2, "one pause per batch");

        // Quiet skip: unchanged bytes owe no pause at all.
        process_file(
            &db,
            &identity(),
            &transcript.display().to_string(),
            &flag,
            || None,
            &|_| Ok(()),
            Some(&recorder),
        )
        .unwrap();
        assert_eq!(pauses.lock().unwrap().len(), 2, "quiet skip pays nothing");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_passes_are_throttled_and_notify_passes_are_not() {
        // Origin gating through the real run() loop: the startup sweep pays
        // throttle pauses; a notification-driven pass (the await drain path)
        // never does.
        let dir = tempfile::tempdir().unwrap();
        let streams_db =
            Arc::new(StreamsDatabase::open(dir.path().join("transcripts-db")).unwrap());
        let token_db =
            Arc::new(TokenUsageDatabase::open(dir.path().join("token-usage-db")).unwrap());
        let path = dir.path().join("transcript.jsonl");
        fs::write(
            &path,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 0), 50)),
        )
        .unwrap();
        streams_db
            .insert_stream(&stream_record(
                "s_gate",
                "claude",
                &path.display().to_string(),
            ))
            .unwrap();
        let (collected, sink) = collecting_sink();
        let throttle_calls = Arc::new(Mutex::new(0usize));
        let recorder_calls = throttle_calls.clone();
        let (handle, shutdown) = spawn_run_loop_throttled(
            streams_db,
            token_db,
            sink,
            Duration::from_secs(30 * 60),
            Some(Arc::new(move |_pause: Duration| {
                *recorder_calls.lock().unwrap() += 1;
            })),
        );

        wait_for_bucket(&collected, bucket_of(1, 0)).await;
        let after_sweep = *throttle_calls.lock().unwrap();
        assert!(after_sweep >= 1, "startup sweep pass is throttled");

        // Append and notify: the notification-origin pass emits the new
        // bucket without ever touching the throttle.
        let mut content = fs::read_to_string(&path).unwrap();
        content.push_str(&format!(
            "{}\n",
            claude_line("m2", "r2", &recent_ts(6, 0), 70)
        ));
        fs::write(&path, content).unwrap();
        handle.notify_stream_processed("s_gate", "claude", &path);
        wait_for_bucket(&collected, bucket_of(6, 0)).await;
        assert_eq!(
            *throttle_calls.lock().unwrap(),
            after_sweep,
            "notify-origin pass must not be throttled"
        );
        shutdown.notify_one();
    }

    #[test]
    fn error_backoff_schedule_is_bounded() {
        assert_eq!(error_backoff_secs(0), 0);
        assert_eq!(error_backoff_secs(1), 5);
        assert_eq!(error_backoff_secs(2), 30);
        assert_eq!(error_backoff_secs(3), 300);
        assert_eq!(error_backoff_secs(4), 1800);
        assert_eq!(error_backoff_secs(100), 1800);
    }

    fn collecting_sink() -> (Arc<Mutex<Vec<MetricEvent>>>, TestSink) {
        let collected: Arc<Mutex<Vec<MetricEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_events = collected.clone();
        let sink: TestSink = Arc::new(move |events: &[MetricEvent]| {
            sink_events.lock().unwrap().extend(events.to_vec());
            Ok(())
        });
        (collected, sink)
    }

    #[test]
    fn parked_fork_stays_pending_and_a_later_sweep_pass_releases_it() {
        // A fork whose transcript ends with its first usage event still
        // parked must survive the full worker chain across two passes:
        // pending_flush persists, the unchanged file stays a sweep candidate,
        // and the re-sweep pass releases the turn once the burst window has
        // passed in wall clock.
        let dir = tempfile::tempdir().unwrap();
        let streams_db =
            Arc::new(StreamsDatabase::open(dir.path().join("transcripts-db")).unwrap());
        let token_db =
            Arc::new(TokenUsageDatabase::open(dir.path().join("token-usage-db")).unwrap());
        let path = dir.path().join("rollout.jsonl");
        // Stamped slightly in the future so pass 1 cannot release early on a
        // loaded CI box (release needs >1s of wall clock past this stamp).
        let now = (chrono::Utc::now() + chrono::Duration::seconds(2))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let fork_meta = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"child","forked_from_id":"parent"}}"#;
        let token_count = format!(
            r#"{{"timestamp":"{now}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15}}}}}}}}"#
        );
        fs::write(&path, format!("{fork_meta}\n{token_count}\n")).unwrap();
        let path_str = path.display().to_string();
        streams_db
            .insert_stream(&stream_record("s_fork", "codex", &path_str))
            .unwrap();
        let task = TokenUsageTask {
            session_id: "s_fork".to_string(),
            tool: "codex".to_string(),
            stream_path: path_str.clone(),
        };
        let (collected, sink) = collecting_sink();

        // Pass 1: the usage event is written moments ago, so the EOF flush
        // must not release it (a burst partner may still be coming).
        process_task_blocking(
            &streams_db,
            &token_db,
            &|e| sink(e),
            None,
            &task,
            &flag_off(),
        )
        .unwrap();
        assert!(collected.lock().unwrap().is_empty(), "turn stays parked");
        let tracked = token_db
            .ensure_file("s_fork", &path_str, "codex", "s_fork-ext")
            .unwrap();
        assert!(tracked.pending_flush);
        // The bytes have not changed, but the pending flush alone keeps the
        // file a sweep candidate.
        assert_eq!(sweep_candidates(&streams_db, &token_db).len(), 1);

        // Pass 2, after the burst window: the parked turn is the session's
        // own first turn and is released and emitted. Sleep past the future
        // stamp plus the window plus one second (the flush clock has second
        // granularity).
        std::thread::sleep(Duration::from_millis(4100));
        process_task_blocking(
            &streams_db,
            &token_db,
            &|e| sink(e),
            None,
            &task,
            &flag_off(),
        )
        .unwrap();
        let events = collected.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::TOTAL_TOKENS),
            Some(15)
        );
        let tracked = token_db
            .ensure_file("s_fork", &path_str, "codex", "s_fork-ext")
            .unwrap();
        assert!(!tracked.pending_flush);
        assert!(sweep_candidates(&streams_db, &token_db).is_empty());
    }

    fn flag_off() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn spawn_run_loop(
        streams_db: Arc<StreamsDatabase>,
        token_db: Arc<TokenUsageDatabase>,
        sink: TestSink,
        sweep_interval: Duration,
    ) -> (TokenUsageWorkerHandle, Arc<Notify>) {
        spawn_run_loop_throttled(streams_db, token_db, sink, sweep_interval, None)
    }

    fn spawn_run_loop_throttled(
        streams_db: Arc<StreamsDatabase>,
        token_db: Arc<TokenUsageDatabase>,
        sink: TestSink,
        sweep_interval: Duration,
        test_throttle: Option<TestThrottle>,
    ) -> (TokenUsageWorkerHandle, Arc<Notify>) {
        let (notify_tx, notify_rx) = tokio::sync::mpsc::unbounded_channel();
        let (drain_tx, drain_rx) = tokio::sync::mpsc::unbounded_channel();
        let shutdown_notify = Arc::new(Notify::new());
        let worker = TokenUsageWorker {
            streams_db,
            token_db,
            telemetry: DaemonTelemetryWorkerHandle::new_noop(),
            shutdown_notify: shutdown_notify.clone(),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            notify_rx,
            drain_rx,
            notify_queue: VecDeque::new(),
            sweep_queue: VecDeque::new(),
            queued: HashSet::new(),
            sweep_interval,
            test_sink: Some(sink),
            test_throttle,
        };
        tokio::spawn(worker.run());
        (
            TokenUsageWorkerHandle {
                notify_tx,
                drain_tx,
            },
            shutdown_notify,
        )
    }

    /// Poll until the collected events contain the given bucket, or panic.
    async fn wait_for_bucket(collected: &Arc<Mutex<Vec<MetricEvent>>>, bucket: u64) {
        for _ in 0..600 {
            if collected
                .lock()
                .unwrap()
                .iter()
                .any(|e| value_u64(e, token_usage_pos::BUCKET_TS) == Some(bucket))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("bucket {bucket} was never emitted");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_loop_startup_sweep_backfills_and_ticker_picks_up_appends() {
        // The real run() loop, end to end: the startup sweep backfills a
        // tracked transcript with no notification ever sent, and the sweep
        // ticker later picks up appended lines on its own.
        let dir = tempfile::tempdir().unwrap();
        let streams_db =
            Arc::new(StreamsDatabase::open(dir.path().join("transcripts-db")).unwrap());
        let token_db =
            Arc::new(TokenUsageDatabase::open(dir.path().join("token-usage-db")).unwrap());
        let path = dir.path().join("transcript.jsonl");
        fs::write(
            &path,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 0), 50)),
        )
        .unwrap();
        streams_db
            .insert_stream(&stream_record(
                "s_run",
                "claude",
                &path.display().to_string(),
            ))
            .unwrap();
        let (collected, sink) = collecting_sink();
        let (_handle, shutdown) =
            spawn_run_loop(streams_db, token_db, sink, Duration::from_millis(200));

        wait_for_bucket(&collected, bucket_of(1, 0)).await;

        // Append into a new bucket without notifying: only the ticker sweep
        // can find it.
        let mut content = fs::read_to_string(&path).unwrap();
        content.push_str(&format!(
            "{}\n",
            claude_line("m2", "r2", &recent_ts(6, 0), 70)
        ));
        fs::write(&path, content).unwrap();
        wait_for_bucket(&collected, bucket_of(6, 0)).await;

        shutdown.notify_one();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_loop_restart_backfills_from_the_committed_cursor() {
        // Daemon restart: a fresh worker over the same databases picks up
        // lines appended while no worker ran, via its startup sweep alone,
        // without re-emitting history the previous worker already handled.
        let dir = tempfile::tempdir().unwrap();
        let streams_db =
            Arc::new(StreamsDatabase::open(dir.path().join("transcripts-db")).unwrap());
        let token_db =
            Arc::new(TokenUsageDatabase::open(dir.path().join("token-usage-db")).unwrap());
        let path = dir.path().join("transcript.jsonl");
        fs::write(
            &path,
            format!("{}\n", claude_line("m1", "r1", &recent_ts(1, 0), 50)),
        )
        .unwrap();
        streams_db
            .insert_stream(&stream_record(
                "s_restart",
                "claude",
                &path.display().to_string(),
            ))
            .unwrap();
        let (collected, sink) = collecting_sink();

        // Worker A: 30-minute interval, so only its startup sweep runs.
        let (_handle_a, shutdown_a) = spawn_run_loop(
            streams_db.clone(),
            token_db.clone(),
            sink.clone(),
            Duration::from_secs(30 * 60),
        );
        wait_for_bucket(&collected, bucket_of(1, 0)).await;
        shutdown_a.notify_one();

        // While no worker runs, the agent keeps writing.
        let mut content = fs::read_to_string(&path).unwrap();
        content.push_str(&format!(
            "{}\n",
            claude_line("m2", "r2", &recent_ts(6, 0), 70)
        ));
        fs::write(&path, content).unwrap();

        // Worker B (the restarted daemon) backfills the gap at startup.
        let (_handle_b, shutdown_b) =
            spawn_run_loop(streams_db, token_db, sink, Duration::from_secs(30 * 60));
        wait_for_bucket(&collected, bucket_of(6, 0)).await;
        shutdown_b.notify_one();

        let events = collected.lock().unwrap();
        let first_bucket_emissions = events
            .iter()
            .filter(|e| value_u64(e, token_usage_pos::BUCKET_TS) == Some(bucket_of(1, 0)))
            .count();
        assert_eq!(first_bucket_emissions, 1, "restart must not re-emit");
    }
}
