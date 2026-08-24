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
--> usage_entries (deduplicated per-entry rows, token-usage DB)
--> reconcile all session buckets: aggregate + fingerprint compare (bucket_state)
--> TokenUsage MetricEvent --> telemetry queue --> POST /worker/metrics/upload
```

Driven by `TokenUsageWorker` (`src/daemon/token_usage_worker.rs`):

- **Triggers:** a non-blocking ping from the stream worker after it processes
  a transcript, a startup sweep, and a 30-minute ticker. Sweeps enumerate the
  streams database's `transcript` rows, so every session git-ai knows about is
  backfilled (a new file starts at byte offset 0); transcripts that no longer
  exist are skipped. Nothing runs on the trace2 ingestion path.
- **Feature flag:** `token_usage_metrics` (debug: on, release: off). Read via
  `Config::fresh()` on every trigger, so it can be disabled without a daemon
  restart. The worker is spawned inside the `transcript_streaming` gate, since
  its notifications come from the stream worker.
- **Quietness:** unchanged files are skipped by size/mtime; unchanged buckets
  are never re-emitted (fingerprints); missing files record one error and are
  skipped by later sweeps; the telemetry-buffer backpressure mirrors the
  stream worker.

## State: `~/.git-ai/internal/token-usage-db`

`TokenUsageDatabase` (`src/token_usage/db.rs`):

- `tracked_files` - read cursor (byte offset) + serialized extractor state
  per transcript file.
- `usage_entries` - deduplicated per-entry usage. Per-entry rows (rather than
  bucket accumulators) make ccusage's replacement policy exact: a later entry
  can *lower* a bucket (streaming partial replaced; sidechain replay replaced
  by the parent's entry), and the bucket then re-aggregates via SQL. Retention
  is 90 days (pruned on sweep).
- `bucket_state` - fingerprint of the last emitted aggregate per bucket.

`commit_batch` writes entries, extractor state, and the advanced cursor in a
single transaction. This is why the cursor lives here and not in the streams
database: a crash can never replay transcript lines against post-batch parser
state (which would corrupt Codex's cumulative-delta computation).

## Emission semantics

The server upserts on `(session_id, model, bucket_ts)` with the newest
`event_ts` winning. Events are stamped with emission time (not bucket time),
so a re-emitted correction - including one that lowers a bucket or zeroes it
out entirely - always supersedes the previous value. A bucket is emitted iff
its aggregate's fingerprint differs from the last emitted fingerprint; an
emptied bucket therefore emits an all-zero event exactly once. Changed
buckets are found by reconciling fingerprints across all of the session's
buckets on every pass (no pending-emission state lives in memory), buckets
are marked emitted only after the telemetry queue accepted the events, and
the quiet-skip size/mtime snapshot is written only after a fully successful
pass - so a failed hand-off or a crash at any point is healed by the next
pass. At upload time TokenUsage events get the same repo allow/exclude gate
as SessionEvents (they are transcript-derived, so they keep flowing for
sessions tracked before a repo was excluded).

Values (`token_usage_pos`): `bucket_ts`, `input_tokens`, `output_tokens`,
`cache_read_tokens`, `cache_write_tokens`, `total_tokens`,
`reasoning_output_tokens` (optional; Codex reports it as a subset of output),
`est_cost_micro_usd` (u64 micro-USD), `credits` (f64, reserved), and
`message_count`. Standard `EventAttributes` carry tool, model, session ids,
and repo_url (resolved from the session's working directory, no git spawn).

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
the rewritten-burst heuristic (see `src/token_usage/codex.rs` for the
deviation from ccusage's parent-prefix matching).
