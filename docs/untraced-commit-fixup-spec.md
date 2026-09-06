# Untraced Commit Fixup Spec

Status: authoritative spec for how the git-ai daemon attributes commits it never
saw through trace2. Companion docs:

- `docs/daemon-trace2-ingestion-spec.md` — the traced path this is a fallback to.
- `docs/rewrite-ops-spec.md` — why rewrites are out of scope here.

## The problem

Attribution runs in the daemon, driven by trace2. Three situations produce
commits that never reach it:

- the daemon was off (upgrade, crash, reboot);
- the git client emits no trace2 (JGit / IntelliJ, libgit2-based tools);
- trace2 is emitted but cannot reach the daemon socket (sandboxes; the daemon
  also refuses to start inside Cursor/Claude/Codex sandboxes).

Those commits got no authorship note and no `Committed` telemetry, even when
working logs existed for them. Since the checkpoint CLI exits quietly when the
daemon is unreachable, daemon-off periods usually have no working logs either;
attribution recovery (`src/authorship/attribution_recovery.rs`) is then the
main evidence.

## Ownership model

The per-worktree `HEAD` reflog is written only for operations performed in
that worktree, with a tool-generated message:

| reflog message prefix | meaning | fixup |
|---|---|---|
| `commit:`, `commit (initial):`, `commit (merge):` | a new commit created here | candidate |
| `commit (amend):` | rewrite | skip |
| `rebase*`, `pull*`, `cherry-pick:`, `revert:`, `merge *:`, `reset:`, `checkout:`, `am:`, anything else | rewrite, replay, or remote content | skip |

A candidate must be **corroborated by the branch reflog**: the identical
`old new … message` record in `<common_dir>/logs/refs/heads/<branch>`. Git writes
both records for a commit on a checked-out branch and only `HEAD`'s for a
detached commit, including a manual `git commit` inside a stopped rebase. This
excludes rebase replays structurally; pulled commits never produce `commit:`
records at all. Detached-HEAD commits are skipped (fail closed).

**Exclusive ownership.** A reflog record has exactly one owner. Records a
traced command consumed are skipped by the fixup; records the fixup claims are
consumed the same way, so a late trace2 command cannot claim them again. All
decisions happen inside the family actor, in sequence
(`RefCursor::claim_untraced_commits`).

**One cursor owner.** The actor keeps a per-worktree fixup cursor (byte offset
plus the anchoring record), separate from the traced in-order cursor so that
settling past records nobody owns (a rebase still open in an editor) never
hides them from the command that eventually completes. The repo-family store
persists a snapshot of it and seeds a cold actor.

## No backfill

The cursor starts at the actor's own fixup position, else at the persisted
seed from an earlier daemon lifetime, else at the traced cursor's observation
point (its in-order offset, or the newest record it consumed while cold), else
at the reflog end. A cursor that no longer matches the reflog (offset beyond
EOF, anchor mismatch after `reflog expire`) is re-seeded at the end. Only
records appended after a valid cursor are ever considered. "Known repo" means:
a cursor from before the missed commit exists.

Ownership is decided from what traced commands actually consumed: records
still in the consumed set, and byte ranges the traced cursor compacted or
jumped over while consuming. The traced in-order offset alone is not
ownership — a cold seed from an ingress capture deliberately jumps over prior
untraced history, which is exactly what the fixup exists to claim.

## Non-interference invariants

1. Nothing is added to trace ingestion, checkpoint admission, or the
   normalizer. The worker reads two counters and skips its tick while trace
   frames are still queued.
2. A fixup pass is a family sequencer entry appended at the current time: the
   causal fence holds it behind older open roots, and the per-family exec lock
   serializes it with commands and checkpoints.
3. Records younger than `GIT_AI_DAEMON_UNTRACED_FIXUP_MIN_AGE_MS` (5 s) are left
   for a later pass.
4. The fixup never writes the `HEAD` reflog; post-commit writes notes and
   working logs exactly as the traced path does.
5. An unchanged reflog costs one `stat` per worktree per tick. No git spawns
   without a candidate. A pass attributes at most 10 commits and checks their
   notes in one batch; a tick schedules at most 32 worktrees.
6. Fail closed: invalid cursor, missing reflog, bare repository, detached HEAD,
   unknown message shape — skip and settle, never guess.

## What runs

- **Claim** (`src/daemon/ref_cursor.rs`, `src/daemon/family_actor.rs`): claimed
  records become synthetic `git commit` commands with exact `ref_changes`
  (HEAD and branch), reduced through the normal reducer so the analyzers emit
  `CommitCreated` and `FamilyState` stays current.
- **Pass** (`src/daemon.rs`, `FamilySequencerEntry::UntracedCommitScan`): the
  normal side-effect dispatch runs for each claimed commit — working logs are
  consumed and carried forward, recovery fills holes, the note is written, and
  the `Committed` metric carries `commit_source = untraced_fixup` (position 18;
  null for traced commits). Commits that already have a note are skipped.
  Synthetic commits are never `sync_tracked` in the test completion log.
- **Store** (`src/daemon/repo_family_store.rs`): families and per-worktree
  cursors, written only by the fixup. Families whose common dir is missing for
  7 days, unseen for 90 days, or beyond the 1000 most recent are pruned.
- **Worker** (`src/daemon/untraced_commit_fixup.rs`): every
  `GIT_AI_DAEMON_UNTRACED_FIXUP_INTERVAL_MS` (10 s; first tick at startup),
  gated by the `untraced_commit_fixup` feature flag. `fixup.scan` on the control
  socket runs a round on demand, regardless of the flag.
- **Health**: `untraced_commits_fixed`, `untraced_commits_skipped`,
  `untraced_cursor_reseeds`, `untraced_scan_errors`, `known_repo_families` in
  `status.daemon`, `git-ai bg status`, and the heartbeat.

## Known limitations

- Detached-HEAD commits are never fixed up.
- libgit2 callers may write arbitrary or empty reflog messages; those are skipped.
- Rewrites made untraced (amend, rebase, cherry-pick, revert) are never
  migrated; the replacement commits get no notes.
- A pass runs later than the commit, so time-window recovery solvers (bash
  mtime, session events) only apply while the committed files are unchanged
  in the worktree; commit-metadata and working-log evidence always apply.
- Checkpoints that reach the daemon between an untraced commit and its fixup
  pass are keyed by the new HEAD without the `INITIAL` attribution the pass
  writes later; for a file already checkpointed in that window, leftover
  uncommitted AI lines are not carried into the next commit. The traced path
  is not exposed to this because its post-commit runs before later
  checkpoints; the untraced window is `interval + min age` (about 15 s).
- Corroboration reads each branch reflog's tail (1 MiB) and falls back to the
  whole file once per pass; a pass walks at most 4 MiB of `HEAD` reflog before
  handing the rest to a follow-up pass.

## Test obligations

`tests/integration/untraced_commit_fixup.rs` covers: attribution from a
delivered working log and from recovery alone; daemon-off commit after
restart; untraced amend, rebase (daemon on and off), pull, and detached commit
skipped; no backfill on first sighting; exactly-once (one note, one metric,
traced commits never reclaimed); linked worktrees; the periodic and startup
scans; the feature flag. Unit tests live in `src/daemon/ref_cursor.rs`
(claim), `src/daemon/family_actor.rs`, `src/daemon/repo_family_store.rs`, and
`src/git/repo_state.rs` (worktree enumeration).
