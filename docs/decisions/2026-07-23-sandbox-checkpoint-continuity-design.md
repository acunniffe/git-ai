# Sandbox Checkpoint Continuity Design

Status: accepted for Unix-first implementation. Windows startup blocking remains
disabled until durable outbox parity is implemented and tested.

Communication diagram and worked scenarios:
[sandbox checkpoint continuity flows and examples](./2026-07-23-sandbox-checkpoint-continuity-examples.md).

## Decision summary

When a checkpoint hook runs inside an agent sandbox, git-ai should:

1. use an already-running host daemon whenever its control socket is reachable;
2. avoid starting a new long-lived daemon with sandbox restrictions inherited;
3. preserve each allowed repository's fully prepared `CheckpointRequest` in a
   durable, private outbox when delivery is unavailable;
4. have the host daemon validate and replay those requests through the existing
   family sequencer; and
5. keep hook work bounded and non-failing while making any real loss visible in
   diagnostics.

The durable outbox is the source of recovery. Metadata-only inference is not.
The heuristic in upstream PR #1914 should not be part of the initial behavior
because it can misattribute unrelated same-repository activity.

## Why this is one behavior, not three patches

The upstream work is split across:

- [#1912](https://github.com/git-ai-project/git-ai/pull/1912), which refuses
  daemon startup when a sandbox marker is present;
- [#1913](https://github.com/git-ai-project/git-ai/pull/1913), which stores a
  content-free checkpoint summary in `/tmp`; and
- [#1914](https://github.com/git-ai-project/git-ai/pull/1914), which uses that
  summary as a timing hint for attribution recovery.

Landing only the startup guard creates a new loss path in this fork:

- `checkpoint` currently initializes the daemon connection before it parses the
  hook or prepares checkpoint requests;
- a connection/startup failure prints an error and exits zero;
- `bg restart` stops the reachable daemon before it attempts a new start, so a
  sandbox guard added to the shared start path could leave the service down.

The startup policy, fallback delivery, import semantics, and restart lifecycle
therefore need one contract even if they land as several small PRs.

## Current fork constraints

This proposal preserves the architecture already present on `origin/main`:

- `CheckpointRequest` is a complete snapshot. It includes absolute paths,
  captured file contents, repository worktree, and the base commit.
- The CLI checks `allowed_repositories` before sending a checkpoint.
- `ActorDaemonCoordinator::ingest_checkpoint_payload` resolves the repository
  family, fences already-visible trace2 work, and applies the checkpoint through
  the family sequencer.
- Checkpoint application writes the working log and related metrics in the
  daemon.
- Hook commands deliberately exit zero on operational failures so an attribution
  failure does not break the agent or editor operation.
- The checkpoint content budget already caps one file at 3 MiB and one request
  set at 32 MiB / 500,000 lines by default.

The May checkpoint rewrite intentionally removed disk intermediates. This design
adds one narrow exception: a bounded outbox used only after live delivery cannot
be acknowledged. It does not restore a synchronous checkpoint implementation.

## Goals

- Preserve the exact captured checkpoint for an allowed repository.
- Prefer existing host-daemon IPC regardless of sandbox markers.
- Never silently drop a prepared checkpoint solely because daemon startup is
  unsafe in the current environment.
- Reuse the current family sequencer, checkpoint resolver, and working-log path.
- Make replay idempotent at the visible attribution boundary.
- Keep the CLI fallback bounded and free of new Git subprocesses.
- Keep successful live-delivery behavior unchanged; add durable recovery for
  unavailable-daemon and IPC-failure cases whether or not a sandbox caused them.
- Preserve the existing hook exit-zero contract.

## Non-goals

- Reconstruct trace2 events that were never delivered to the host daemon.
- Infer attribution from a nearby timestamp when no exact checkpoint exists.
- Store raw hook input, prompts, or transcript contents in the outbox.
- Introduce a general job queue, event bus, or new synchronous checkpoint path.
- Make telemetry and CAS delivery durable as part of this change.
- Cherry-pick the upstream monolithic modules into the fork's layered layout.

## Required invariants

1. **Allowlist before persistence.** The producer must reject a request before
   writing it when collection is not allowed.
2. **Allowlist again before replay.** The daemon must rediscover the repository
   and apply `Config::fresh()` before enqueueing an imported request.
3. **Snapshot fidelity.** Replay uses the captured contents and base commit. It
   does not reread the current worktree as a substitute.
4. **One ingress path.** Imported requests enter a delivery-aware coordinator
   method; they do not call checkpoint side effects directly.
5. **Acknowledged or durable.** A prepared request is successful when either the
   daemon acknowledges application or a complete outbox record is durably
   published.
6. **Idempotent attribution.** Losing the daemon response or crashing after
   application must eventually produce at most one visible checkpoint for a
   delivery ID.
7. **Private local state.** Outbox records are not readable or replaceable by
   another local user.
8. **Bounded work.** Record size, record count, queue bytes, scan batch, retry
   rate, and retention all have hard limits.
9. **Restart safety.** A caller that is forbidden to start a daemon cannot stop
   a healthy daemon as the first half of `restart`.
10. **No false certainty.** If neither IPC nor durable persistence succeeds, the
    hook still exits zero but records and prints a redacted, actionable failure.
11. **Commit-safe recovery.** A delayed checkpoint must either be incorporated
    before post-commit authorship is written or reconcile one provably affected
    commit. It must not merely append to an old working log after the note was
    produced.

## Proposed flow

### Producer

`checkpoint` must be excluded from the process-wide telemetry pre-connect. Its
handler owns checkpoint delivery:

1. parse the preset and hook input;
2. build `CheckpointRequest`s with the existing orchestrator;
3. validate absolute paths and apply the existing client allowlist;
4. wrap each request in a versioned delivery envelope;
5. try the existing daemon socket first;
6. if no daemon is reachable, start one only when startup policy permits;
7. retry live delivery once; then
8. durably publish the current and remaining unacknowledged envelopes.

Every request gets its delivery ID before the first socket attempt. If the
daemon applied a request but its response was lost, the fallback record carries
the same ID and replay is a no-op.

A decoded control response is an acknowledgement only when `response.ok` is
true. The current handler does not inspect that field; the delivery helper must.

The existing allowlist remains all-or-nothing across a hook invocation: if any
repository is denied, nothing is sent or persisted. After authorization passes,
acknowledged requests are not written again; a transport-failed request and the
remaining unsent requests are written individually.

This deliberately changes the legacy parent-CWD Bash recovery case. If a Bash
hook's CWD cannot be resolved to an allowed repository without running Git, the
entire hook is denied before attempt metadata or raw debug input is persisted.
The producer does not parse shell commands or guess a target repository.
File and human events instead authorize their resolved file repositories; their
process CWD is not an authorization anchor. Empty file events are silent no-ops.
When shorthand input needs a dirty-status scan to discover files, its CWD is
authorized with read-only repository metadata before Git is run.

### Daemon startup policy

Sandbox markers are evidence about spawn safety, not evidence that IPC is
unusable.

The policy order is:

1. ping the configured control and trace sockets;
2. if both are healthy, use that daemon;
3. if the daemon is absent and auto-start is explicitly disabled, use the
   outbox;
4. if a strong sandbox runtime marker is present, do not create a long-lived
   daemon that inherits it; use the outbox;
5. otherwise use the existing detached-start path.

The diagnostic context can recognize `CURSOR_SANDBOX`, `SANDBOX_RUNTIME`,
`CODEX_SANDBOX`, and `CODEX_SANDBOX_NETWORK_DISABLED`. Network-disabled by
itself is supporting context, not sufficient proof that local process creation
or IPC is restricted. The first three are strong runtime markers; the last is
not a standalone spawn ban. No marker ever prevents a connection attempt.

Explicit lifecycle commands use the same preflight without sharing the
checkpoint fallback:

- `bg start` succeeds when the configured daemon is already healthy; otherwise
  it reports that detached startup is blocked.
- `bg restart` performs the preflight before shutdown. If startup is blocked,
  it returns an error and leaves a healthy daemon running.
- `bg run` is an explicit foreground operation and may proceed with a warning;
  it does not silently create a detached sandbox-inherited service.
- daemon self-restart remains host-owned and must not add sandbox markers to the
  child environment.

### Delivery envelope

The persistence contract is separate from `CheckpointRequest`:

```rust
pub struct CheckpointDelivery {
    pub schema_version: u16,
    pub delivery_id: String,
    pub batch_id: String,
    pub batch_ordinal: u32,
    pub captured_at_unix_ms: u64,
    pub producer_version: String,
    pub request: CheckpointRequest,
}
```

The envelope intentionally contains no raw hook input or transcript contents.
Its request does contain file snapshots, transcript paths, external session
identifiers, and arbitrary preset metadata. Agent V1 metadata can include a raw
shell command. These fields are sensitive and are part of the private-state
threat model; silently claiming they are absent would be incorrect.

Existing checkpoint content budgets still apply. Metadata, path count, and
individual path length receive separate caps. Serialize the record to CBOR and
enforce the actual encoded byte length before publication; do not estimate JSON
overhead from raw source bytes.

Use a new control request variant for delivery-aware clients and retain the
current `checkpoint.run` variant during compatibility rollout. The new variant
calls `ingest_checkpoint_delivery(CheckpointDelivery)`. The legacy
`ingest_checkpoint_payload(CheckpointRequest)` constructs an internal delivery
with no durable ID and delegates in the other direction. The delivery context
stays attached through the family sequencer and checkpoint application.

### Outbox location

A global `/tmp/git-ai-sandboxed-checkpoints` directory is not acceptable. Its
first creator effectively owns the path for every user, and it conflates
independent daemon homes.

The producer and daemon derive the same ordered candidate roots from
`DaemonConfig`:

1. an explicit absolute test/managed override;
2. `<internal_dir>/daemon/checkpoint-outbox-v1`; and
3. a per-user, per-daemon fallback below the platform temp directory, named
   from the numeric user identity where available plus a SHA-256 prefix of the
   daemon internal directory.

The producer chooses the first root it can validate and write. The daemon scans
all derived roots, which lets a sandbox fall back to temp even when it can only
read the normal internal directory.

`GIT_AI_CHECKPOINT_OUTBOX_DIR` is a managed/test deployment setting, not a
per-hook discovery mechanism. The producer and an already-running host daemon
must inherit the same absolute value before the daemon starts. Setting or
changing it only in a sandboxed hook process can strand records and is outside
the supported v1 contract.

Recovery is supported only when producer and daemon share all of:

- the same effective user identity;
- the same `DaemonConfig` instance key and allowlist configuration;
- a host-visible outbox root; and
- repository/worktree paths with the same meaning in both namespaces.

The v1 delivery wire format supports UTF-8 repository and file paths. A native
path that cannot be represented as UTF-8 is rejected before live delivery or
outbox publication with a redacted warning; recovery must not claim that such
a checkpoint was preserved.

An explicit root solves only outbox visibility. It does not map a container
worktree path to a host path or reconcile different user identities/config.
Namespace path mapping is out of scope for v1; such records fail closed with an
`unsupported_namespace` diagnostic. A diagnostic command must show every
derived root and whether it is writable, host-visible, invalid, or backlogged,
plus whether the current identity and repository path satisfy this contract.

### Secure publication

On Unix:

- the queue directory must be a real directory, owned by the effective UID,
  and mode `0700`;
- symlink roots and symlink records are rejected;
- a record is created with `create_new` and mode `0600`;
- bytes are written, the file is flushed and `fsync`ed, then atomically renamed
  to a ready suffix;
- the containing directory is `fsync`ed after publication;
- the importer opens without following symlinks and revalidates owner, type,
  mode, and link count.

Windows must provide the equivalent current-user ACL and atomic replace rules.
The startup guard must not be enabled for a platform until its durable outbox
implementation is available and tested.

Ready filenames contain only ordering and identity data, for example:

```text
<captured_at_unix_ms>-<batch_id>-<batch_ordinal>-<delivery_id>.ready
```

Paths, repository names, agent names, and session identifiers stay inside the
private record.

### Capacity and retention

Initial hard defaults:

- maximum encoded record: a hard CBOR byte limit derived from the content,
  metadata, path-count, and path-length caps and checked after serialization;
- maximum ready records: 4,096;
- maximum total ready bytes: 256 MiB;
- import batch: 128 records;
- retry backoff: capped exponential backoff with jitter;
- successful receipt retention: 7 days;
- invalid-record quarantine retention: 24 hours.

Limits should be constants first, with test-only overrides. They should not
become public configuration until operational evidence shows a need.
Ready, claimed, receipt, and quarantine entries all count toward their
respective byte/count caps; moving a record cannot evade capacity accounting.

When full, the queue does not evict an already accepted record to make room.
It rejects the new publication, updates a redacted failure sentinel, and emits
an actionable warning while preserving the hook exit-zero contract.

Directory enumeration filters ready records before selecting the oldest batch.
Because record count is bounded, it may gather the validated candidates and
sort by `(captured_at_unix_ms, batch_id, batch_ordinal, delivery_id)` without the upstream
`read_dir().take(1000)` ordering bug.

### Import and idempotency

The daemon starts an outbox worker after it owns the daemon lock. Trace and
control listeners must become available without waiting for a full backlog
drain. The worker performs a bounded initial scan and then polls for later
records. Each record is:

1. validated and decoded;
2. checked against schema and size limits;
3. checked for an already-completed delivery ID;
4. resolved to exactly one repository family;
5. rechecked against the current host allowlist;
6. submitted to the existing family sequencer with its delivery context;
7. acknowledged only after checkpoint application completes; and
8. moved to a short-lived receipt or removed after durable completion state is
   recorded.

Add an optional `delivery_id` to the persisted checkpoint format. Before
appending a checkpoint, application checks the already-read working log for the
same ID.

That check is not crash-safe with the current truncate/write/flush working-log
rewrite. Before enabling replay, checkpoint-list publication must use a
same-directory temporary file, file `fsync`, atomic rename, and directory
`fsync`. Then the crash cases are deterministic: either the old list remains
and the ready record retries, or the new list durably contains the delivery ID
and replay deduplicates it.

The processing contract is therefore:

- at-least-once importer execution;
- eventually zero checkpoints for a valid no-op or one visible checkpoint for
  a material delivery, never two with the same delivery ID; and
- best-effort metric deduplication keyed by the same delivery ID.

A no-op checkpoint can be recorded in a small completion receipt so it does not
repeat forever. No distributed transaction is introduced between working logs,
metrics SQLite, and filesystem cleanup.

Denied records are deleted and represented only by a redacted status entry.
Malformed or unsupported records move to the private quarantine for at most 24
hours. Neither path retries forever.

### Ordering limits

`captured_at_unix_ms` uses Unix milliseconds because
`ResolvedCheckpointExecution::ts` already uses milliseconds. Sequencer ordering
converts it to nanoseconds with checked multiplication; persisted
`Checkpoint.timestamp` divides it by 1,000 to seconds. Import time is never
substituted as event time.

Batch ordinals preserve one hook invocation's order. Wall clocks do not prove
causal order across concurrent producer processes. Non-overlapping files may be
replayed independently; overlapping records that cannot be ordered by batch,
base commit, phase, and session/tool-use identity become
`needs_reconciliation` rather than receiving an arbitrary delivery-ID order.

This cannot recreate trace2 commands that never reached the daemon. The
implementation and user-facing diagnostics must state that boundary rather than
claim complete historical replay.

A transcript `StreamSource` is also only a path. Exact file attribution can be
replayed from the request, but transcript enrichment remains best-effort when
the sandbox path is not visible to the host.

## Commit race and late reconciliation

Durable delivery is insufficient if a commit is processed before its checkpoint.
Normal replay would append to the captured base's working log after the child
commit's note was already written.

There are two deterministic recovery cases:

1. **Commit not processed yet.** The background worker claims and decodes
   records off the trace-ingestion path and maintains an in-memory pending index
   by family. Once a record is known, it enters the family sequencer with its
   capture ordering before a later commit side effect. Commit handling may
   inspect that in-memory marker, but it does not scan files or wait on an
   unclaimed/corrupt outbox.
2. **One child already processed.** Prove that the child descends directly from
   the captured base and that its relevant blobs match the captured post-edit
   snapshots. Load the existing authorship note and apply a narrow
   reconciliation that changes only lines the existing recovery pipeline
   classifies as unknown. Preserve all known human/AI attribution and existing
   prompt/session metadata.

If there are multiple candidate descendants, rewritten history, a blob mismatch,
no existing note, no exact post-edit snapshot, or an ordering ambiguity, mark
the delivery as `needs_reconciliation` and fail closed. Do not choose a commit
by timestamp or repository proximity.

Do not recreate the deleted parent working log and rerun the full post-commit
conversion: that partial log could overwrite a correct note while omitting
earlier checkpoints. Reconciliation belongs in a narrow authorship operation
that reuses existing unknown-line and note-merge primitives and runs
asynchronously after import.

## Repository placement

Follow the current layered structure:

| Responsibility | Proposed home |
|---|---|
| Pure envelope, IDs, states, validation outcomes | `src/model/checkpoint_delivery.rs` |
| Filesystem outbox and completion receipts | `src/model/repository/checkpoint_outbox/` |
| CLI delivery orchestration | `src/operations/commands/checkpoint_agent/delivery.rs` |
| Spawn policy and sandbox diagnostics | `src/operations/commands/daemon_start_policy.rs` |
| Daemon polling and replay orchestration | `src/operations/daemon/checkpoint_outbox_worker.rs` |
| Exact late-commit reconciliation | `src/operations/authorship/deferred_checkpoint_reconciliation.rs` |
| Control DTO compatibility | `src/model/daemon_control.rs` |

Do not add a generic queue abstraction. The outbox store should expose narrow
operations such as `publish`, `oldest_ready`, `mark_complete`,
`quarantine`, and `prune`.

The importer receives its store explicitly from daemon initialization. It
should not introduce another global singleton.

## Observability

Add redacted counters/status fields for:

- live delivery acknowledged;
- fallback record published;
- fallback publication failed by reason class;
- records pending and bytes pending;
- import success, retry, duplicate, deny, corrupt, expired, unsupported
  namespace, and needs-reconciliation;
- age of the oldest ready record; and
- startup blocked with detected provider/marker.

`git-ai debug` should report roots, permissions, backlog, oldest age, and last
redacted error. It must not print record contents, file paths, repository URLs,
session IDs, or tool input.

## Test plan

### Pure and persistence tests

- marker classification and existing-daemon precedence;
- envelope round trip and forward-version rejection;
- stable delivery IDs, explicit timestamp-unit conversion, and capture ordering;
- batch IDs and ordinals across multi-request hooks;
- concurrent overlapping producers and wall-clock rollback fail closed;
- atomic publication and no partial ready files;
- atomic working-log publication across every simulated crash point;
- collision safety and concurrent publishers;
- owner/mode/symlink/hard-link rejection;
- actual CBOR size plus metadata, path, count, byte, batch, and TTL bounds;
- corrupt records, unsupported versions, and quarantine pruning;
- duplicate delivery IDs and crash-window replay;
- same daemon home maps to the same fallback root; different homes do not;
- user, config, outbox, or repository namespace mismatch fails closed; and
- sensitive command/path/session metadata never appears in diagnostics.

### Integration tests

Use `TestRepo` with isolated daemon homes and config:

- each supported sandbox marker with no daemon publishes an exact record;
- a reachable daemon is used even with every marker present;
- empty `allowed_repositories` produces neither IPC nor a record;
- an allowed worktree publishes and the host daemon imports it;
- changing host config to deny the repo before import rejects and removes it;
- imported pre/post checkpoints preserve line attribution and timestamps;
- a record already indexed by the worker is sequenced before a later commit
  without synchronous outbox I/O on the commit path;
- a directly committed matching snapshot is reconciled if import finishes late;
- late reconciliation preserves the existing note and changes only unknown
  lines;
- ambiguous descendants, ordering, or blob mismatches fail closed;
- a lost response followed by replay produces one visible checkpoint;
- an `ok: false` daemon response is not treated as acknowledgement;
- a corrupt record cannot block later valid records;
- queue capacity failure is visible but the hook exits zero;
- one denied repository keeps the current all-or-nothing multi-repository
  behavior;
- after authorization, a partial transport failure spools only unacknowledged
  requests;
- `bg restart` under a sandbox marker leaves the healthy daemon reachable;
- startup performs a bounded scan without delaying trace socket readiness;
- Unix long socket paths and temp fallback use the same daemon instance key;
- Windows ACL and replay parity before enabling the guard there.

### Regression gates

Every phase runs targeted tests, `task lint`, `task fmt`, and the native
`task test` suite. Ubuntu CI is the first remote signal, followed by macOS and
Windows for phases that touch platform storage or daemon lifecycle.

## Delivery plan

Each phase is independently reviewable and does not enable the guard before the
supported shared-identity/shared-path recovery contract exists.

### P0: contracts and characterization

- Add current failure-path tests.
- Add the delivery envelope and outbox store behind test-only call sites.
- Characterize the existing exit-zero hook contract and restart lifecycle.
- No production behavior change.

### P1: durable outbox, guard still disabled

- Add secure publication, bounds, diagnostics, and client allowlist tests.
- Refactor checkpoint handling into one delivery helper.
- Exclude checkpoint from the top-level telemetry pre-connect.
- On ordinary IPC failure, publish the exact request.

### P2: same-base replay and idempotency

- Add the daemon worker, host-side allowlist validation, completion receipts,
  capture-time replay, and delivery-ID checkpoint deduplication.
- Route imports through the family sequencer.
- Harden working-log publication with atomic rename and durability syncs.
- Add bounded startup scanning and the worker's in-memory pending-family index.

### P3: late-commit reconciliation

- Prove the direct-child matching-snapshot case and merge only exact unknown-line
  attribution into the existing note.
- Make ambiguous descendants, rewrites, and content mismatches visible and
  non-attributing.
- Keep all timing-based recovery out of this path.

### P4: safe startup and restart policy

- Add existing-daemon-first sandbox handling.
- Block sandbox-inherited auto-start only after P1-P3 are proven.
- Preflight restart before shutdown.
- Enable on Unix platforms with a verified outbox.

### P5: platform completion and hardening

- Add Windows ACL/path implementation and enable the same policy there.
- Tune limits only from observed queue and latency data.
- Remove compatibility paths after one release window.

## Alternatives considered

### Merge #1912 alone

Rejected. It prevents one unsafe spawn but turns the current silent checkpoint
drop into the normal sandbox path and can strand `bg restart`.

### Port #1913 and #1914 unchanged

Rejected. The upstream queue is global to `/tmp`, Unix-only, not durably
directory-synced, unbounded in age/bytes, and not idempotent across
DB-commit/unlink crashes. More importantly, its record omits the snapshot and
the follow-up guesses attribution from nearby activity.

### Metadata-only recovery

Rejected as the default. The upstream selector considers same-repository events
within a time window but does not require the candidate file path to match the
committed file. A nearby event can therefore claim unrelated unknown lines.

### Repository-local queue

Rejected. It mutates user repositories, interacts badly with worktrees and
ignore rules, and creates new cleanup and accidental-commit risks.

### Synchronous checkpoint fallback

Rejected. It duplicates daemon behavior and violates the existing single-writer
and family-sequencing architecture.

### Keep silent loss

Rejected. Hook exit zero can remain, but successful capture must mean daemon
acknowledgement or durable publication, and an actual loss must be diagnosable.

## Questions for discussion

1. Is temporarily storing allowed file snapshots and sensitive request metadata
   such as command/path/session identifiers acceptable under the private,
   bounded, short-lived outbox rules? Exact recovery depends on this.
2. Should v1 explicitly support only shared user/config/repository namespaces
   and fail closed for container path mapping? The recommendation is yes.
3. Is Unix-first enablement acceptable, provided the guard remains disabled on
   Windows until parity exists?
4. Should metadata-only heuristic recovery ever return as a separately flagged
   opt-in? The recommendation is no unless it first requires file-path
   correlation, paired bash events, and a confidence threshold that never
   overwrites known attribution.

## Recommendation

Proceed through P0-P4 with the exact outbox design. Do not port upstream #1914
as part of sandbox continuity. Re-evaluate heuristic recovery only after exact
replay has production evidence, and treat it as a separate product decision
rather than a fallback hidden inside daemon startup handling.
