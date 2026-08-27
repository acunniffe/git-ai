# TokenUsage Events

Token usage and estimated cost per `(session_id, model, speed, bucket_ts)`
in 5-minute UTC buckets, computed from local agent transcripts and emitted as
metric event id 9 (`TokenUsageValues`, `src/metrics/events.rs`). Fast and
standard usage of one model bill at different rates, so speed (0 standard,
1 fast) is part of the bucket identity.

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

The server upserts on `(session_id, model, speed, bucket_ts)` keeping the **highest
`emitted_seq`** — a strictly increasing per-bucket revision, reserved in the
state database *before* events reach the telemetry queue so a crash between
sink and bookkeeping can never produce two payloads with equal revisions —
so re-emissions within the same u32 second cannot tie on `event_ts`,
regardless of upload batching or retry order. Reserved revisions are floored
to wall-clock seconds (`max(prev + 1, now)`), so a rebuilt state database
(the flag toggled off deletes it; local data loss) resumes at revisions
strictly above anything previously uploaded rather than restarting at 1 and
losing every upsert to the server's highest-revision rule. A bucket is emitted iff its aggregate's fingerprint
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
`message_count`, `emitted_seq`, `speed` (0/1, the bucket-key dimension),
`speed_inferred` (any entry's tier was not recorded in the transcript —
resolved from configuration or the standard default), `cache_write_1h_tokens`
(the 1h-TTL portion of `cache_write_tokens`),
`long_context_{input,output,cache_read,cache_write,
cache_write_1h}_tokens` (tokens of requests that selected their model's
long-context tier; base-tier tokens are the totals minus these), and
`transcript_cost_micro_usd` (the portion of `est_cost_micro_usd` that came
from transcript `costUSD` fields). Standard `EventAttributes` carry tool,
model, session ids, and repo_url (no git spawn); the dedicated
`pricing_catalog` attribute carries the id of the catalog that priced the
bucket's catalog-priced entries (`embedded:<version>` or `modelsdev:<hash>`;
a bucket that mixes catalogs — a refresh plus daemon restart mid-bucket —
reports the greatest id, an approximation). `custom_attributes` stays
reserved for the org-configured attribute map every event type carries.

Cost follows ccusage's "auto" mode per entry: the transcript's own `costUSD`
wins (negative/non-finite values are treated as absent, and every per-entry
cost clamps to a $10k sanity ceiling); otherwise cost is computed from the
models.dev pricing catalog (`src/metrics/model_pricing.rs`) with ccusage's
two per-agent formulas (`src/token_usage/cost.rs`): whole-request
long-context tier selection (Claude: uncached input + cache reads + cache
writes vs the model's threshold; Codex: raw per-turn input only), 2x-input
1h-cache-write pricing at the selected tier, per-shape unpublished-cache-rate
defaults, and the model's fast multiplier over the whole request for
fast-speed entries. Codex service tiers come from `thread_settings_applied`
(sticky), falling back to the `service_tier` in the rollout's codex-home
`config.toml`, else standard; unlike ccusage, the fallback is resolved and
stored at extraction time, so a config change never retroactively reprices
history. Fast multipliers and the pre-4.6 Anthropic 200K long-context
premium are hand-tracked override tables in `model_pricing.rs` (no
machine-readable source publishes them). Per Anthropic's pricing page,
Claude 4.6+ models include the full 1M context window at standard pricing,
so the premium override covers only the 1M-beta era (sonnet-4-5).

Costs are event-time: pricing snapshot refreshes do not retroactively
rewrite already-emitted buckets (only changed buckets recompute) - intended.
The emitted splits are the repricing contract: a different pricing sheet can
recompute a bucket as `base tokens x base rates + long-context tokens x
above rates` (whole-request selection already applied client-side), with 1h
cache writes at 2x the tier's input rate, times the fast multiplier when
`speed = 1`, plus `transcript_cost_micro_usd` as a fixed term (its tokens
are included in the totals, so such buckets reprice approximately; the same
holds for buckets containing entries clamped at the $10k per-entry ceiling,
whose token counts are not clamped).
Remaining pricing deviations from ccusage: no `codex-auto-review` model
mapping, and no marginal-at-200K arm for above-rates-without-threshold data
(unreachable with a models.dev-sourced catalog); `git-ai usage` approximates
from the same catalog without tiering/multiplier/1h refinements, so
TokenUsage events are the authoritative dollars.

## Session identity

Usage is leaf-attributed: every agent execution owns its own usage under its
own session — Claude subagent (sidechain) transcripts and Codex fork/subagent
rollouts included — with the parent carried as the
`parent_session_id`/`external_parent_session_id` attributes, exactly like
SessionEvents. The parent-inclusive total is a derived rollup over the parent
relationship, never the stored identity, so per-agent cost stays visible.
(Documented deviation from ccusage, which rolls Claude sidechains into the
parent session: per-session Claude groupings differ, aggregate totals match.)
Sidechain replays of parent messages still deduplicate against the parent's
entries across files because entry dedup is global, not session-scoped.
Forked Codex rollouts skip their replayed prefix by
matching it against the parent rollout's pre-fork usage signatures (ccusage's
parent-prefix matching): the worker resolves the parent — tracked files under
the parent's external session id, else a bounded scan of the codex sessions
tree, always confirmed against the candidate's recorded `session_meta` id —
and the unmatched remainder persists in the extractor state, so the parent is
read once per fork, never per pass. A parent that cannot be resolved falls
back (without retry, like ccusage with an unavailable parent log) to the
rewritten-burst heuristic: leading usage events spaced <= 1s apart are
replayed history, and a fork whose transcript ends with its first usage event
still parked is released by an end-of-file flush once the burst window has
passed in wall-clock time.

## Accepted limitations

Reviewed decisions, accepted rather than accidental:

- **Corrections withheld during exclusion are not replayed.** Events filtered
  by the repo exclude gate are marked delivered like every other metric kind;
  a bucket corrected while its repo was excluded stays diverged on the server
  until the bucket's aggregate changes again after un-exclusion.
- **Uploader abandonment applies.** TokenUsage rides the shared metrics
  uploader, which abandons records after six failed upload attempts
  (~20 hours offline). The state database has already marked those buckets
  emitted, so this loses any emission — first emissions included (e.g. a
  historical backfill that ran entirely during a >20h upload outage), not
  just corrections. A later genuine change to a bucket re-emits it at a
  higher revision and heals that bucket, but a bucket that never changes
  again is lost.
- **Rewritten transcript content stays counted.** Entries have no per-file
  ownership, so history that a rewrite/truncation removed from a transcript
  keeps its rows (bounded by 90-day retention). Transcripts are append-only
  in practice; the shrink path re-reads from byte 0 and dedup absorbs the
  replay.
- **Sweep-discovered work sits outside the `git-ai await` drain barrier**
  (see Pipeline): backfill is eventual, matching stream-worker semantics.
- **A fork burst split across passes can over-count one event — on the
  unresolvable-parent fallback path only.** With parent-prefix matching the
  replayed history is identified by content, so this race is gone for any
  fork whose parent rollout is resolvable. When it is not, the end-of-file
  flush releases a parked first turn after the burst window passes in wall
  clock; if Codex writes the replayed burst lines more than a second apart
  (buffered/laggy serialization carrying sub-second recorded timestamps) and
  a pass lands in the gap, the parked replay is released as own usage. The
  flush leaves the skip machine armed at the released event's timestamp, so
  the late-arriving remainder of the burst is still skipped — the over-count
  is bounded to that single released event per fork.
