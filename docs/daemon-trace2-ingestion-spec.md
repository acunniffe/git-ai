# Daemon Trace2 Ingestion Spec

Status: authoritative spec for how the git-ai daemon establishes exact
ownership of git ref transitions from trace2 event streams. Companion docs:

- `docs/rewrite-ops-spec.md` — what happens once exact transitions are known.
- `docs/attribution-fuzzer-spec.md` — correctness pressure.

## The problem: exact command ownership

The daemon learns about git commands asynchronously: git writes trace2 events
to a socket, the daemon reads them later. Git mutates refs synchronously
inside the git process. By the time the daemon processes a command, the repo
may have moved on — more commands, user edits, other worktrees.

Stock trace2 does not include created commit SHAs or complete ref-update OIDs
for most commands. A delayed trace2 root saying "git commit ran in repo X
with argv Y, exit 0" identifies *that a command ran*, not *which reflog
entries it produced*. Attribution requires the latter.

**Ownership rule.** A ref-moving command's transitions are exact if and only
if at least one of:

1. **Pre-command cursor** — the daemon held a reflog cursor (byte offset +
   anchor) for the relevant ref from *before* the command; entries appended
   after the cursor, matching the command's expected transition shape, belong
   to it.
2. **Immutable argv OIDs** — the command line itself contains full OIDs
   sufficient to identify the operation (e.g. `merge --squash <sha>`,
   `update-ref ref <new> <old>`, `cherry-pick <sha1> <sha2>`).

Otherwise the command is **not exact**: the daemon must fail closed for
attribution (no guessed authorship, no note migration) and may only use the
command as a *future baseline* — observe the current reflog ends so the *next*
command is exact.

Banned as ownership proof (each was tried and failed; see Postmortem):
reflog timestamps (seconds-resolution, not causally tied to a trace2 root),
commit/reflog message matching (messages collide), latest-HEAD guessing, and
daemon-ingress "start" offsets captured after the fact.

## Data path

```
git (trace2 socket target)
  → socket listener (src/daemon.rs)
      prepare_trace_payload_for_ingest: filters definitely-read-only roots,
      enqueues mutating roots with sequence numbers
  → TraceNormalizer (src/daemon/trace_normalizer.rs)
      groups frames by root sid; terminal event → NormalizedCommand
  → coordinator → family actor (one actor per repo family = common git dir)
      owns ordered state for the family, including the RefCursor
  → RefCursor::enrich_command (src/daemon/ref_cursor.rs)
      consumes cursor-bounded reflog entries → cmd.ref_changes
  → analyzers (src/daemon/analyzers/history.rs)
      classified semantic events → rewrite/post-commit side effects
```

Separation of concerns:

- The **normalizer** parses trace2/argv facts only. It never reads mutable
  repo state to synthesize missing command facts.
- The **family actor** owns all ordered, stateful reasoning: the ref cursor,
  the stash stack, pending operation state. Commands and checkpoints for one
  repo family are processed in arrival order, sequenced by the originating
  command's start time (a checkpoint's receipt time). A still-running
  mutating command is not a sequencer entry: it is an open trace root that
  fences later entries of its family until it finishes, and a root the reader
  has already seen finishing keeps that fence until the ingest worker
  processes its final frames. See "Causal fence" for how long a root can hold.
- **Side effects** run only after enrichment, on exact data.

### Causal fence

Trace2 frames of one root arrive in order (one connection, sequential writes),
but across roots there is no order: independent reader threads feed a serial
ingest worker. The hazard this creates is a git process that has changed refs
while its final frame has not reached the sequencer. An open, mutating (or not
yet classified) root that started at or before a completed entry holds that
entry — and `sync.family`, the shutdown drain, and `await` — as follows,
decided from data first and heuristics last:

1. **Finishing** (`atexit` already read by the reader, or its socket closed):
   holds until the worker processes its queued frame. The worker clears the
   root even when that frame fails to normalize or ingest; as a safety net,
   the hard cap releases a finishing root too (`reason=finishing_root_cap`).
2. **Written**: the reader captured the root's worktree `HEAD` reflog length
   when it started; if that reflog has grown, or was modified after the root's
   start on git's clock, the root has changed refs and holds until it
   finishes (a post-write hook, say), bounded by 600 × grace, or by the hard
   cap once its process is gone (a sibling connection may keep a dead root
   registered). The modification time covers a length the reader recorded
   late, after the root had already written, and cannot be set by a committer
   date.
3. **Unwritten**: neither signal fired, so the root has changed nothing and
   fences nothing. Nobody waits for an editor or a pre-commit hook, not even a
   grace.
4. **No reflog to consult** (the root's worktree is unknown, or the repository
   has no `HEAD` reflog): time and liveness decide. The root holds for the
   causal grace (`FAMILY_CAUSAL_GRACE`, 1 s; `GIT_AI_DAEMON_CAUSAL_GRACE_MS`)
   measured from when the waiting work became ready; past it, a root whose pid
   (encoded in its sid, `-P<hex>`) is alive is released
   (`reason=causal_grace_expired`) and one whose process is gone or unknown
   holds until the hard cap (`reason=causal_fence_hard_cap`).

A heuristic release (`causal_grace_expired`, `causal_fence_hard_cap`,
`written_root_cap`, `finishing_root_cap`) is logged and counted once per
root. When the root's process was gone at the time, the release is also
recorded against the root: a dead process cannot write again, so later work,
`sync.family` and `await` do not wait out the bound behind it while it stays
registered. A live root's release is per waiter, since it may still write
and the next waiter's grace covers that write's frames. The hard cap is
30 × grace.

Unattributed roots (no `def_repo` yet) fail closed and fence every family.
Roots that started after the waiting work cannot precede it and never fence
it. Heuristic releases are `WARN`-logged once per root with the root, its
command, family, age and wait, and counted for the health snapshot.

### NormalizedCommand

Facts: family/scope, worktree, root sid, raw argv, primary command, observed
child commands, exit code, trace start/finish timestamps, optional
`reflog_start_offsets`, operation-specific immutable OIDs from argv
(stash target, cherry-pick sources, revert sources), `ref_changes` (output of
enrichment), confidence.

`reflog_start_offsets` carries *claimed* command-start reflog positions.
Trust is decided at the cursor, not at ingress (see below).

## Ref cursor model

Per family, the cursor stores per-ref-key:

- byte offset into the reflog file (always at a line boundary)
- anchor: the full reflog record ending at that offset (old/new OIDs,
  message) proving the offset still belongs to the same reflog generation
- consumed offsets/anchors (entries already owned by earlier commands)
- in-memory stash stack and pending cherry-pick source OIDs

Ref keys distinguish `worktree:<git_dir>:HEAD` (per-worktree HEAD reflogs)
from `common:<ref>` (shared refs like `refs/heads/main`, `refs/stash`).

Robustness requirements (all implemented and tested):

- incomplete trailing reflog lines are ignored (a writer may be mid-append)
- a saved offset is honored only if it lands on a newline and its anchor
  matches the record ending there; otherwise the cursor is cleared
- offset beyond file length (pruned/truncated reflog) clears the cursor
- branch delete/recreate clears the stale cursor
- expiry/`reflog expire` invalidates via anchor mismatch

### Consumption

`enrich_command` consumes reflog entries appended after the cursor that match
the command's *expected transition shape* — per-command message-prefix
families (`commit`, `commit (amend):`, `rebase`, `reset:`, `checkout:`, ...)
and expected old/new OID constraints derived from family state and argv.
Matched entries move into the consumed set so the next command cannot claim
them. Multi-entry operations (rebase, multi-pick) consume contiguous spans
where each entry's `old` equals the previous entry's `new`. Message prefixes
and timestamps may *narrow* an already-exact candidate set; they are never
the proof of ownership by themselves.

If nothing matches: `ref_changes` stays empty, confidence stays low, side
effects skip attribution (fail closed), and the cursor advances to the
current reflog end as a baseline for future commands only.

### Seeding

Cursors come into existence at trusted observation points:

1. **Trace ingress capture** (best-effort): when the daemon sees a *live*,
   non-terminal trace2 frame for a mutating command, it captures current
   reflog ends and attaches them to the root as claimed start offsets. Because
   delivery is asynchronous, these claims may already be post-append;
   `command_start_offset_is_authoritative` only accepts a claimed offset if
   records exist after it and it does not move an existing cursor backward.
   An accepted-but-late offset can only shrink the window a command may claim
   (it can lose attribution, never steal another command's entries) — late
   capture is *conservative*, satisfying fail-closed.
2. **Checkpoints**: a checkpoint arriving at the family actor is a real,
   ordered causal observation; processing it establishes family state (and
   hence expected-transition inputs) for subsequent commands.
3. **Command completion**: after any command is processed — resolved or not —
   the cursor observes the current reflog ends as the baseline for the next
   command.

### Cold start

"Cold" = the repo was set up without trace2 (or before the daemon existed),
so no cursor predates the first traced command.

- The first traced command must process without crash, deadlock, or state
  poisoning.
- If it lacks immutable argv OIDs and no cursor existed, it fails closed for
  attribution and seeds the baseline.
- Subsequent commands are exact.
- Special case: first traced command whose argv contains sufficient immutable
  OIDs (e.g. `merge --squash <sha>`) is exact even cold.

## Operation-specific ownership notes

- **commit / amend**: expected HEAD transition with `commit`/`commit (amend):`
  message family; branch ref entry consumed alongside HEAD when they describe
  the same transition.
- **rebase**: consumes the contiguous HEAD span from original tip through
  final tip; `rebase --continue` of an in-progress rebase relies on
  family-actor pending state from the failed command's consumed prefix, never
  on reading `.git/rebase-merge` after the fact.
- **cherry-pick / revert**: source OIDs from argv when immutable; symbolic
  sources resolved only at a cursor-bounded boundary; pending source state
  carries across conflicted stop/continue.
- **stash**: the in-memory stash stack mirrors `refs/stash` mutations observed
  through consumed entries; `stash@{N}` resolves against that stack, not
  against the live ref at processing time.
- **reset**: `reset:` message family, old-OID constraint relaxed (reset can
  move from any state); backward-reset detection happens downstream from the
  exact transition.
- **update-ref**: argv carries ref name and usually both OIDs (immutable);
  used by graphite-style restacks (`commit-tree` + `update-ref`). Multiple
  same-command ref updates are correlated by OID first, with the command's
  time window only narrowing candidates.
- **pull**: decomposes into fetch + merge/rebase; ownership follows the
  underlying HEAD/branch transitions.
- **merge-tree / commit-tree alone**: create objects, move no refs — no
  transition to own; nothing happens until a ref moves.
- **push / fetch / clone**: notes-sync side effects keyed off argv remotes;
  missing `refs/notes/ai` is a no-op, not an error.

## Reads must not sync

Production read commands (`show`, `blame`, `status`, ...) must not trigger a
hidden daemon sync or barrier. Tests use explicit sync immediately before
assertions; a barrier in production would hide races instead of fixing them
and cannot create missing command-start data anyway.

## Postmortem: rejected approaches

These were implemented, found unsound, and removed. Do not reintroduce.

1. **mtime-guarded worktree snapshots** (post-commit carryover): read the live
   worktree after git exited, guarded by `mtime <= git_finish_time`.
   Filesystem clocks are coarse; later writes land in the same quantum; the
   snapshot can capture the *next* operation's content. Replaced by persisted
   working logs + committed tree data.
2. **Live-worktree stash restore**: same race, same fix — reconstruct from
   stash objects + target head in isolation.
3. **Daemon-ingress offsets as proof**: a captured "start" offset may actually
   be post-command. Demoted from proof to conservative, validated hint (see
   Seeding); never primary evidence.
4. **Trace2 barrier / hidden read sync**: hides races; doesn't create data.
5. **Reflog timestamp matching as proof**: seconds-resolution, collides;
   allowed only to narrow already-exact candidate sets.
6. **Message matching without a cursor**: duplicate commit messages are
   ubiquitous; cold duplicate-message commands fail closed instead.

## Test obligations

Deterministic tests must cover, at minimum:

1. delayed duplicate-message commits without a cursor fail closed
2. checkpoint-then-commit attributes exactly
3. cold first traced commit does not guess; seeds baseline only
4. partial trailing reflog line ignored; partial and full prune clear cursor
5. branch delete/recreate clears cursor state
6. symbolic ref movement after a delayed command does not corrupt attribution
7. immutable argv OIDs work cold (squash, cherry-pick, update-ref)
8. live worktree edits after commit/stash-pop do not leak into attribution
9. symlink/canonical path variants map to one repo family
10. no hidden sync before `show`/`blame`
11. daemon survives partial trace roots, socket close ordering, child trace
    traffic, and never deadlocks checkpoints behind unidentified sockets

Primary suites: `tests/daemon_mode.rs`, `tests/commit_tree_update_ref.rs`,
`tests/integration/rewrite_ops_attribution.rs`, ref-cursor unit tests in
`src/daemon/ref_cursor.rs`.

## Bottom line

Exactness is structural, not heuristic: cursor, or immutable argv OIDs, or
fail closed. The unavoidable consequence — git-ai cannot attribute the very
first delayed write command in a cold repo from stock trace2 alone — is
missing information, not a bug. Every mechanism that tried to paper over that
gap (timestamps, messages, latest-state guesses, post-hoc offsets) produced
misattribution and was removed.
