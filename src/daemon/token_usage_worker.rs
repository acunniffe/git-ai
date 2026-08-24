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
//! - The read cursor, extractor state, entries, and emission fingerprints all
//!   live in [`TokenUsageDatabase`]; each batch commits atomically, so there
//!   is no retry machinery here - a failed file is retried on the next
//!   notification or sweep, and unchanged files are skipped by size/mtime.
//! - Claude subagent transcripts roll up to their parent session (matching
//!   ccusage), which also lets sidechain replays dedup against the parent's
//!   entries.
//! - The `token_usage_metrics` feature flag is read fresh on every trigger,
//!   so the worker can be disabled without a daemon restart (enabling it
//!   only requires the daemon to be running with the worker spawned).

use std::collections::HashSet;
use std::collections::VecDeque;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::interval;

use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::daemon::telemetry_worker::DaemonTelemetryWorkerHandle;
use crate::error::GitAiError;
use crate::metrics::{EventAttributes, MetricEvent, PosEncoded, TokenUsageValues};
use crate::streams::db::{StreamRecord, StreamsDatabase};
use crate::streams::types::{JsonlLineState, read_jsonl_line};
use crate::token_usage::db::{DirtyBuckets, TokenUsageDatabase};
use crate::token_usage::extractor_for_tool;

/// Entry/byte bounds of one atomic batch commit.
const BATCH_MAX_ENTRIES: usize = 1_000;
const BATCH_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Same telemetry-buffer backpressure as the stream worker.
const BACKPRESSURE_THRESHOLD: usize = 5_000;
const BACKPRESSURE_MAX_WAITS: usize = 40;

/// A transcript file to (re)process, identified by its streams-db row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TokenUsageTask {
    session_id: String,
    tool: String,
    stream_path: String,
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

    /// Wait until all currently queued work has been processed.
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
        queue: VecDeque::new(),
        queued: HashSet::new(),
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
    queue: VecDeque<TokenUsageTask>,
    queued: HashSet<TokenUsageTask>,
}

/// Read the flag fresh so config changes apply without a daemon restart.
fn token_usage_enabled() -> bool {
    crate::config::Config::fresh()
        .get_feature_flags()
        .token_usage_metrics
}

impl TokenUsageWorker {
    async fn run(mut self) {
        tracing::info!("token-usage worker started");
        let mut sweep_ticker = interval(Duration::from_secs(30 * 60));
        sweep_ticker.tick().await; // skip the immediate tick

        if token_usage_enabled() {
            self.enqueue_sweep_tasks();
        }

        loop {
            self.process_queued().await;
            if self.shutdown_flag.load(Ordering::Relaxed) {
                break;
            }
            tokio::select! {
                _ = self.shutdown_notify.notified() => {
                    self.shutdown_flag.store(true, Ordering::Relaxed);
                    break;
                }
                _ = sweep_ticker.tick() => {
                    if token_usage_enabled() {
                        self.enqueue_sweep_tasks();
                    }
                }
                Some(task) = self.notify_rx.recv() => {
                    if token_usage_enabled() {
                        self.enqueue(task);
                    }
                }
                Some(request) = self.drain_rx.recv() => {
                    self.handle_drain(request).await;
                }
            }
        }
        tracing::info!("token-usage worker shutdown complete");
    }

    async fn handle_drain(&mut self, request: DrainRequest) {
        // Consume notifications that were queued before the barrier.
        while let Ok(task) = self.notify_rx.try_recv() {
            if token_usage_enabled() {
                self.enqueue(task);
            }
        }
        self.process_queued().await;
        let _ = request.completion.send(());
    }

    fn enqueue(&mut self, task: TokenUsageTask) {
        if self.queued.insert(task.clone()) {
            self.queue.push_back(task);
        }
    }

    /// Enqueue every supported transcript the streams database knows about.
    /// New files get a zero cursor in the token-usage database, which is what
    /// backfills history for sessions tracked before this feature ran.
    fn enqueue_sweep_tasks(&mut self) {
        let streams = match self.streams_db.all_streams() {
            Ok(streams) => streams,
            Err(e) => {
                tracing::warn!(error = %e, "token-usage sweep: failed to list streams");
                return;
            }
        };
        for stream in streams {
            if stream.stream_kind != "transcript" || extractor_for_tool(&stream.tool).is_none() {
                continue;
            }
            // Sessions whose transcript is gone can't be backfilled; skip
            // them without recording errors so sweeps stay quiet.
            if !std::path::Path::new(&stream.stream_path).exists() {
                continue;
            }
            self.enqueue(TokenUsageTask {
                session_id: stream.session_id,
                tool: stream.tool,
                stream_path: stream.stream_path,
            });
        }
    }

    async fn process_queued(&mut self) {
        while let Some(task) = self.queue.pop_front() {
            self.queued.remove(&task);
            if self.shutdown_flag.load(Ordering::Relaxed) {
                return;
            }
            let streams_db = self.streams_db.clone();
            let token_db = self.token_db.clone();
            let telemetry = self.telemetry.clone();
            let shutdown_flag = self.shutdown_flag.clone();
            let task_clone = task.clone();
            let result = tokio::task::spawn_blocking(move || {
                process_task_blocking(
                    &streams_db,
                    &token_db,
                    &telemetry,
                    &task_clone,
                    &shutdown_flag,
                )
            })
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, session_id = %task.session_id, "token-usage processing failed");
                    let _ = self.token_db.record_error(
                        &task.session_id,
                        &task.stream_path,
                        &e.to_string(),
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, session_id = %task.session_id, "token-usage task panicked");
                }
            }
        }
    }
}

/// Per-file emission context derived from the streams-db row.
struct EmissionContext {
    /// Rollup session: subagent transcripts attribute to their parent session
    /// (matching ccusage), which also dedups sidechain replays.
    session_id: String,
    external_session_id: String,
    tool: String,
    repo_url: Option<String>,
}

impl EmissionContext {
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
        let repo_url = stream
            .repo_work_dir
            .as_ref()
            .and_then(|dir| crate::repo_url::resolve_repo_url_from_path(&PathBuf::from(dir)));
        Self {
            session_id,
            external_session_id,
            tool: stream.tool.clone(),
            repo_url,
        }
    }
}

fn process_task_blocking(
    streams_db: &StreamsDatabase,
    token_db: &TokenUsageDatabase,
    telemetry: &DaemonTelemetryWorkerHandle,
    task: &TokenUsageTask,
    shutdown_flag: &AtomicBool,
) -> Result<(), GitAiError> {
    let Some(stream) = streams_db
        .get_stream(&task.session_id, "transcript", &task.stream_path)
        .map_err(|e| GitAiError::Generic(format!("streams db read failed: {e}")))?
    else {
        return Ok(());
    };
    let ctx = EmissionContext::from_stream(&stream);
    process_file(token_db, &ctx, &task.stream_path, shutdown_flag, |events| {
        // Backpressure before handing a batch to the telemetry queue.
        for _ in 0..BACKPRESSURE_MAX_WAITS {
            if telemetry.metrics_buffer_len() < BACKPRESSURE_THRESHOLD
                || shutdown_flag.load(Ordering::Relaxed)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        telemetry.persist_metrics_blocking(events).map(|_| ())
    })
}

/// Incrementally read one transcript file, persist deduplicated entries, and
/// emit changed buckets through `sink`. Split out (with an injectable sink)
/// for direct testing without a daemon.
fn process_file(
    token_db: &TokenUsageDatabase,
    ctx: &EmissionContext,
    stream_path: &str,
    shutdown_flag: &AtomicBool,
    sink: impl Fn(&[MetricEvent]) -> Result<(), GitAiError>,
) -> Result<(), GitAiError> {
    let tracked = token_db.ensure_file(&ctx.session_id, stream_path, &ctx.tool)?;
    let metadata = std::fs::metadata(stream_path)?;
    let size = metadata.len();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);

    // Quiet skip: nothing changed since the last completed pass.
    if size == tracked.last_known_size as u64
        && modified == tracked.last_modified
        && tracked.byte_offset <= size
    {
        return Ok(());
    }

    let Some(mut extractor) = extractor_for_tool(&ctx.tool) else {
        return Ok(());
    };
    // A shrunken file was rewritten: restart from scratch. Entry-level dedup
    // keeps re-extraction idempotent.
    let mut offset = tracked.byte_offset;
    if offset > size {
        offset = 0;
    } else if let Some(state) = tracked.state_json.as_deref() {
        extractor.restore_state(state);
    }

    let file = std::fs::File::open(stream_path)?;
    let mut reader = BufReader::with_capacity(128 * 1024, file);
    reader.seek(SeekFrom::Start(offset))?;

    let mut dirty = DirtyBuckets::new();
    let mut line = String::new();
    let mut reached_end = false;
    let mut interrupted = false;
    while !reached_end && !interrupted {
        let mut entries = Vec::new();
        let mut consumed = 0usize;
        loop {
            if shutdown_flag.load(Ordering::Relaxed) {
                interrupted = true;
                break;
            }
            match read_jsonl_line(&mut reader, &mut line)? {
                JsonlLineState::Eof => {
                    reached_end = true;
                    break;
                }
                // A partial trailing line is still being appended; re-read it
                // next pass (the cursor stays before it).
                JsonlLineState::Partial => {
                    reached_end = true;
                    break;
                }
                JsonlLineState::Complete(bytes) => {
                    offset += bytes as u64;
                    consumed += bytes;
                    let trimmed = line.trim_end();
                    if extractor.wants_line(trimmed) {
                        entries.extend(extractor.extract_line(trimmed));
                    }
                    if entries.len() >= BATCH_MAX_ENTRIES || consumed >= BATCH_MAX_BYTES {
                        break;
                    }
                }
            }
        }
        dirty.extend(token_db.commit_batch(
            &ctx.session_id,
            stream_path,
            &entries,
            offset,
            extractor.state_json().as_deref(),
        )?);
    }

    if reached_end && !interrupted {
        token_db.update_file_metadata(&ctx.session_id, stream_path, size, modified)?;
    }

    emit_changed_buckets(token_db, ctx, &dirty, sink)
}

/// Re-aggregate each dirty bucket and emit those whose fingerprint differs
/// from the last emitted one, marking them emitted only after the sink
/// accepted the events.
fn emit_changed_buckets(
    token_db: &TokenUsageDatabase,
    ctx: &EmissionContext,
    dirty: &DirtyBuckets,
    sink: impl Fn(&[MetricEvent]) -> Result<(), GitAiError>,
) -> Result<(), GitAiError> {
    let mut events = Vec::new();
    let mut emitted = Vec::new();
    for (model, bucket) in dirty {
        let aggregate = token_db.aggregate_bucket(&ctx.session_id, model, *bucket)?;
        let fingerprint = aggregate.fingerprint();
        if token_db
            .emitted_fingerprint(&ctx.session_id, model, *bucket)?
            .as_deref()
            == Some(fingerprint.as_str())
        {
            continue;
        }
        let values = TokenUsageValues::new()
            .bucket_ts(*bucket as u64)
            .input_tokens(aggregate.input)
            .output_tokens(aggregate.output)
            .cache_read_tokens(aggregate.cache_read)
            .cache_write_tokens(aggregate.cache_write)
            .total_tokens(aggregate.total)
            .reasoning_output_tokens_opt(aggregate.reasoning_output)
            .est_cost_micro_usd(aggregate.cost_micro_usd)
            .message_count(aggregate.message_count);
        let mut attrs = EventAttributes::with_version(env!("CARGO_PKG_VERSION"))
            .session_id(ctx.session_id.clone())
            .external_session_id(ctx.external_session_id.clone())
            .tool(&ctx.tool)
            .model(model);
        if let Some(url) = &ctx.repo_url {
            attrs = attrs.repo_url(url.clone());
        }
        events.push(MetricEvent::new(&values, attrs.to_sparse()));
        emitted.push((model.clone(), *bucket, fingerprint));
    }
    if events.is_empty() {
        return Ok(());
    }
    sink(&events)?;
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    for (model, bucket, fingerprint) in emitted {
        token_db.mark_emitted(&ctx.session_id, &model, bucket, &fingerprint, now_ts)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::PosEncoded;
    use crate::metrics::events::token_usage_pos;
    use std::fs;
    use std::sync::Mutex;

    fn ctx() -> EmissionContext {
        EmissionContext {
            session_id: "s_test".to_string(),
            external_session_id: "ext-test".to_string(),
            tool: "claude".to_string(),
            repo_url: Some("https://github.com/acme/repo".to_string()),
        }
    }

    fn setup() -> (tempfile::TempDir, TokenUsageDatabase, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db = TokenUsageDatabase::open(dir.path().join("token-usage-db")).unwrap();
        let transcript = dir.path().join("transcript.jsonl");
        (dir, db, transcript)
    }

    fn claude_line(msg: &str, req: &str, ts: &str, output: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","sessionId":"ext-test","requestId":"{req}","message":{{"id":"{msg}","model":"claude-sonnet-4-20250514","usage":{{"input_tokens":100,"output_tokens":{output},"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#
        )
    }

    fn run(
        db: &TokenUsageDatabase,
        transcript: &std::path::Path,
    ) -> Result<Vec<MetricEvent>, GitAiError> {
        let collected = Mutex::new(Vec::new());
        let flag = AtomicBool::new(false);
        process_file(
            db,
            &ctx(),
            &transcript.display().to_string(),
            &flag,
            |events| {
                collected.lock().unwrap().extend(events.to_vec());
                Ok(())
            },
        )?;
        Ok(collected.into_inner().unwrap())
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
                claude_line("m1", "r1", "2026-01-01T00:01:00Z", 50),
                claude_line("m2", "r2", "2026-01-01T00:06:00Z", 70),
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
        // 2026-01-01T00:00:00Z = 1767225600.
        assert_eq!(buckets, vec![1_767_225_600, 1_767_225_900]);
        for event in &events {
            assert_eq!(event.event_id, 9);
            assert_eq!(value_u64(event, token_usage_pos::INPUT_TOKENS), Some(100));
            assert_eq!(value_u64(event, token_usage_pos::MESSAGE_COUNT), Some(1));
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
            format!("{}\n", claude_line("m1", "r1", "2026-01-01T00:01:00Z", 50)),
        )
        .unwrap();
        assert_eq!(run(&db, &transcript).unwrap().len(), 1);
        // Size/mtime unchanged: skipped entirely.
        assert!(run(&db, &transcript).unwrap().is_empty());
    }

    #[test]
    fn appended_usage_reemits_the_bucket_with_higher_totals() {
        let (_dir, db, transcript) = setup();
        fs::write(
            &transcript,
            format!("{}\n", claude_line("m1", "r1", "2026-01-01T00:01:00Z", 50)),
        )
        .unwrap();
        let first = run(&db, &transcript).unwrap();
        assert_eq!(
            value_u64(&first[0], token_usage_pos::OUTPUT_TOKENS),
            Some(50)
        );

        let mut content = fs::read_to_string(&transcript).unwrap();
        content.push_str(&claude_line("m2", "r2", "2026-01-01T00:02:00Z", 30));
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
    }

    #[test]
    fn streaming_replacement_moving_buckets_reemits_zeroed_bucket() {
        let (_dir, db, transcript) = setup();
        fs::write(
            &transcript,
            format!("{}\n", claude_line("m1", "r1", "2026-01-01T00:01:00Z", 50)),
        )
        .unwrap();
        assert_eq!(run(&db, &transcript).unwrap().len(), 1);

        // The same message re-emits with larger totals in the next bucket:
        // the old bucket empties and must re-emit as zero exactly once.
        let mut content = fs::read_to_string(&transcript).unwrap();
        content.push_str(&claude_line("m1", "r1", "2026-01-01T00:06:00Z", 90));
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
        assert_eq!(by_bucket[0], (1_767_225_600, 0));
        assert_eq!(by_bucket[1], (1_767_225_900, 190));
        let zero_event = events
            .iter()
            .find(|e| value_u64(e, token_usage_pos::TOTAL_TOKENS) == Some(0))
            .unwrap();
        assert_eq!(
            value_u64(zero_event, token_usage_pos::MESSAGE_COUNT),
            Some(0)
        );
    }

    #[test]
    fn partial_trailing_line_is_left_for_the_next_pass() {
        let (_dir, db, transcript) = setup();
        let complete = claude_line("m1", "r1", "2026-01-01T00:01:00Z", 50);
        let partial = claude_line("m2", "r2", "2026-01-01T00:02:00Z", 70);
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
    fn missing_file_is_an_error() {
        let (_dir, db, transcript) = setup();
        assert!(run(&db, &transcript).is_err());
    }

    #[test]
    fn codex_reasoning_tokens_flow_through() {
        let (_dir, db, transcript) = setup();
        let ctx = EmissionContext {
            tool: "codex".to_string(),
            ..ctx()
        };
        fs::write(
            &transcript,
            concat!(
                r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.1"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-01T00:01:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":50,"reasoning_output_tokens":10,"total_tokens":150}}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let collected = Mutex::new(Vec::new());
        let flag = AtomicBool::new(false);
        process_file(
            &db,
            &ctx,
            &transcript.display().to_string(),
            &flag,
            |events| {
                collected.lock().unwrap().extend(events.to_vec());
                Ok(())
            },
        )
        .unwrap();
        let events = collected.into_inner().unwrap();
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
    fn failed_sink_leaves_bucket_unmarked_for_retry() {
        let (_dir, db, transcript) = setup();
        fs::write(
            &transcript,
            format!("{}\n", claude_line("m1", "r1", "2026-01-01T00:01:00Z", 50)),
        )
        .unwrap();
        let flag = AtomicBool::new(false);
        let result = process_file(
            &db,
            &ctx(),
            &transcript.display().to_string(),
            &flag,
            |_| Err(GitAiError::Generic("sink down".to_string())),
        );
        assert!(result.is_err());

        // Entries and cursor were committed, but the bucket was not marked
        // emitted: the next pass over a changed file re-emits it. Touch the
        // file so the size/mtime skip doesn't apply.
        let mut content = fs::read_to_string(&transcript).unwrap();
        content.push_str(&claude_line("m2", "r2", "2026-01-01T00:02:00Z", 5));
        content.push('\n');
        fs::write(&transcript, content).unwrap();
        let events = run(&db, &transcript).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            value_u64(&events[0], token_usage_pos::MESSAGE_COUNT),
            Some(2)
        );
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

    #[tokio::test]
    async fn sweep_enqueues_supported_existing_transcripts_once() {
        let dir = tempfile::tempdir().unwrap();
        let streams_db =
            Arc::new(StreamsDatabase::open(dir.path().join("transcripts-db")).unwrap());
        let token_db =
            Arc::new(TokenUsageDatabase::open(dir.path().join("token-usage-db")).unwrap());
        let claude_path = dir.path().join("claude.jsonl");
        fs::write(&claude_path, "{}\n").unwrap();
        let claude_path = claude_path.display().to_string();

        streams_db
            .insert_stream(&stream_record("s_claude", "claude", &claude_path))
            .unwrap();
        // Unsupported tool and missing file are both skipped.
        streams_db
            .insert_stream(&stream_record("s_gem", "gemini", &claude_path))
            .unwrap();
        streams_db
            .insert_stream(&stream_record("s_gone", "claude", "/definitely/gone.jsonl"))
            .unwrap();
        // Non-transcript stream kinds are skipped.
        let mut otel = stream_record("s_otel", "claude", &claude_path);
        otel.stream_kind = "otel_traces".to_string();
        streams_db.insert_stream(&otel).unwrap();

        let (_notify_tx, notify_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_drain_tx, drain_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut worker = TokenUsageWorker {
            streams_db,
            token_db,
            telemetry: DaemonTelemetryWorkerHandle::new_noop(),
            shutdown_notify: Arc::new(Notify::new()),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            notify_rx,
            drain_rx,
            queue: VecDeque::new(),
            queued: HashSet::new(),
        };

        worker.enqueue_sweep_tasks();
        worker.enqueue_sweep_tasks(); // dedup: repeated sweeps don't re-add
        assert_eq!(worker.queue.len(), 1);
        assert_eq!(worker.queue[0].session_id, "s_claude");
        assert_eq!(worker.queue[0].tool, "claude");
    }

    #[test]
    fn subagent_stream_rolls_up_to_parent_session() {
        let stream = StreamRecord {
            session_id: "s_child".to_string(),
            stream_kind: "transcript".to_string(),
            tool: "claude".to_string(),
            stream_path: "/tmp/child.jsonl".to_string(),
            stream_format: "ClaudeJsonl".to_string(),
            watermark_type: "ByteOffset".to_string(),
            watermark_value: "0".to_string(),
            external_session_id: "child-ext".to_string(),
            external_parent_session_id: Some("parent-ext".to_string()),
            first_seen_at: 0,
            last_processed_at: 0,
            last_known_size: 0,
            last_modified: None,
            processing_errors: 0,
            last_error: None,
            repo_work_dir: None,
        };
        let ctx = EmissionContext::from_stream(&stream);
        assert_eq!(ctx.session_id, generate_session_id("parent-ext", "claude"));
        assert_eq!(ctx.external_session_id, "parent-ext");

        let mut top_level = stream;
        top_level.external_parent_session_id = None;
        let ctx = EmissionContext::from_stream(&top_level);
        assert_eq!(ctx.session_id, "s_child");
        assert_eq!(ctx.external_session_id, "child-ext");
    }
}
