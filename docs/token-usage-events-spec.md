# TokenUsage Events

Token usage and estimated cost per `(session_id, model, bucket_ts)` in
5-minute UTC buckets, computed from local agent transcripts and emitted as
metric event id 9 (`TokenUsageValues`, `src/metrics/events.rs`).

The parsing/deduplication logic is ported from
[ccusage](https://github.com/ccusage/ccusage) (MIT) and adapted to git-ai's
incremental transcript streaming; per-agent deviations are documented in
`src/token_usage/{claude,codex}.rs`. v1 covers Claude Code and Codex.

## Pipeline

```
transcript file --(incremental read, cursor in token-usage DB)-->
  per-agent extractor (src/token_usage/{claude,codex}.rs)
    - prefilter substring check before any JSON parsing
    - per-entry token counts, model, timestamp, transcript costUSD
--> usage_entries (globally deduplicated per-entry rows, token-usage DB)
--> reconcile all session buckets in one pass: aggregate + fingerprint
    compare (bucket_state)
--> TokenUsage MetricEvent --> telemetry queue --> POST /worker/metrics/upload
```

Driven by `TokenUsageWorker` (`src/daemon/token_usage_worker.rs`):

- **Triggers:** a non-blocking ping from the stream worker after it
  successfully processes a transcript (a failed stream pass is picked up by
  the next sweep instead), a startup sweep, and a
  30-minute ticker (missed ticks are skipped, not replayed). Sweeps enumerate
  the streams database's `transcript` rows, so every session git-ai knows
  about is backfilled (a new file starts at byte offset 0); enumeration costs
  one stat per session compared against the token database's size/mtime
  snapshot, and per-file work only happens for files that actually changed.
  Transcripts that no longer exist are skipped. Nothing runs on the trace2
  ingestion path.
- **Queues:** notifications and sweep backfill are separate queues. The
  `git-ai await` drain barrier waits only for notification-driven work, so a
  first-start historical backfill cannot starve `await`; a notification for a
  file already queued for backfill promotes it. Shutdown is observed while a
  pass is in flight (the worker selects on its own shutdown `Notify` around
  the blocking pass).
- **Feature flag:** `token_usage_metrics` (debug: on, release: off) gates the
  worker at daemon startup, like `transcript_streaming`. When the flag is
  off, no worker or ticker runs, the token-usage database is not created, and
  a previously created one is deleted at daemon startup (independent of the
  `transcript_streaming` gate) — no collected data is retained.
- **Quietness:** unchanged files are skipped by size/mtime; unchanged buckets
  are never re-emitted (fingerprints); files whose last pass failed back off
  (5s/30s/5m/30m) instead of retrying on every trigger; the telemetry-buffer
  backpressure mirrors the stream worker.

## State: `~/.git-ai/internal/token-usage-db`

`TokenUsageDatabase` (`src/token_usage/db.rs`):

- `tracked_files` - read cursor (byte offset) + serialized extractor state
  per transcript file, plus the quiet-skip snapshot, error backoff state, and
  a pending-flush marker (a file whose extractor holds buffered entries is
  re-processed even when its bytes have not changed).
- `usage_entries` - deduplicated per-entry usage. Per-entry rows (rather than
  bucket accumulators) make ccusage's replacement policy exact: a later entry
  can *lower* a bucket (streaming partial replaced; sidechain replay replaced
  by the parent's entry), and the bucket then re-aggregates via SQL.
  **Dedup is global across sessions**, matching ccusage's whole-files dedup:
  `claude --resume`/`--continue`/fork copies the parent conversation into a
  new session file with the original message/request ids, and session-scoped
  dedup would re-count that history on every resume. When a replacement moves
  an entry between sessions, the previous owner is durably flagged
  (`needs_reconcile`, in the same transaction) and re-reconciled DB-only —
  inline after the pass and from sweeps for crash recovery — so the
  correction lands even if that session's transcripts were deleted. Entries
  older than the 90-day retention window are dropped at insert (backfill
  never uploads history the prune would delete), and stored entries/bucket
  state are pruned atomically on sweep.
- `bucket_state` - fingerprint, emission revision (`emit_seq`), and timestamp
  of the last emitted aggregate per bucket.

`commit_batch` writes entries, extractor state, and the advanced cursor in a
single transaction. This is why the cursor lives here and not in the streams
database: a crash can never replay transcript lines against post-batch parser
state (which would corrupt Codex's cumulative-delta computation).

## Emission semantics

The server upserts on `(session_id, model, bucket_ts)` keeping the **highest
`emitted_seq`** — a strictly increasing per-bucket revision, reserved in the
state database *before* events reach the telemetry queue so a crash between
sink and bookkeeping can never produce two payloads with equal revisions —
so re-emissions within the same u32 second cannot tie on `event_ts`,
regardless of upload batching or retry order. A bucket is emitted iff its aggregate's fingerprint
differs from the last emitted fingerprint; an emptied bucket therefore emits
an all-zero event exactly once. Changed buckets are found by reconciling
fingerprints across all of the session's buckets in one grouped query on
every pass (no pending-emission state lives in memory), buckets are marked
emitted only after the telemetry queue accepted the events, and the
quiet-skip size/mtime snapshot is written only after a fully successful pass
— so a failed hand-off or a crash at any point is healed by the next pass.
At upload time TokenUsage events get the same repo allow/exclude gate as
SessionEvents (they are transcript-derived, so they keep flowing for sessions
tracked before a repo was excluded); repo_url resolution falls back to the
agent's cwd inference (persisted back) like the SessionEvent path.

Values (`token_usage_pos`): `bucket_ts`, `input_tokens`, `output_tokens`,
`cache_read_tokens`, `cache_write_tokens`, `total_tokens`,
`reasoning_output_tokens` (optional; Codex reports it as a subset of output),
`est_cost_micro_usd` (u64 micro-USD), `credits` (f64, reserved),
`message_count`, and `emitted_seq`. Standard `EventAttributes` carry tool,
model, session ids, and repo_url (no git spawn).

Cost follows ccusage's "auto" mode per entry: the transcript's own `costUSD`
wins; otherwise cost is computed from the models.dev pricing catalog
(`src/metrics/model_pricing.rs`), including the 2x-input rate for 1-hour
ephemeral cache writes. Pricing snapshot refreshes do not retroactively
rewrite already-emitted buckets (only changed buckets recompute) - intended.

## Session identity

Claude subagent transcripts roll up to their parent session (matching
ccusage's session semantics), which also lets sidechain replays of parent
messages deduplicate against the parent's entries across files. Codex
sessions are per rollout file; forked rollouts skip their replayed prefix via
the rewritten-burst heuristic, and a fork whose transcript ends with its
first usage event still parked is released by an end-of-file flush once the
burst window has passed in wall-clock time (see `src/token_usage/codex.rs`
for the deviations from ccusage's parent-prefix matching).
