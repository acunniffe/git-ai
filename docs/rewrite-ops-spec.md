# Rewrite Operations Attribution Spec

Status: authoritative spec for how git-ai migrates authorship attribution
across history-rewriting git operations. Companion docs:

- `docs/daemon-trace2-ingestion-spec.md` — how the daemon learns *which*
  operation ran and its exact ref transitions.
- `docs/attribution-fuzzer-spec.md` — how correctness is pressured beyond the
  deterministic suite.

## The problem

Authorship attribution lives in two places:

1. **Authorship notes** (`refs/notes/ai`, one note per commit): per-file
   attestation ranges proven by checkpoints, plus prompt metadata. A note is
   addressed by the commit it describes, so any operation that *replaces* a
   commit (rebase, amend, cherry-pick, squash, restack) strands the note on the
   dead commit.
2. **Working logs** (`.git/ai/working_logs/<base_commit>/`): uncommitted
   attribution keyed by the HEAD commit at checkpoint time. Any operation that
   *moves HEAD without committing the work* (reset, stash, checkout/switch with
   dirty tree) strands the working log under the wrong base.

Rewrite handling is the machinery that moves this data so attribution follows
the code. It must never *invent* attribution and must never *lose* attribution
for content that verifiably survived.

## Invariants (first principles)

**I1 — Evidence rule.** A line is attributed to an actor only on checkpoint
evidence (a working-log checkpoint) or on prior committed evidence (an
authorship note) connected to the new location by immutable git object data.
No inference from similarity, timestamps, or "probably the same line".

**I2 — Conservation rule.** If a line's content survives a rewrite unchanged
*as established by git's own tree diff*, its attribution survives with it
(including across file renames). If git's diff says a region changed, the old
attribution there is dropped — new content needs new evidence (I1).

**I3 — Immutability rule.** Every input to a rewrite decision must be
immutable at decision time: commit SHAs, tree SHAs, blob contents, notes,
persisted working-log snapshots, and exact command-owned ref transitions. The
live worktree is a valid input only inside checkpoint processing, at
checkpoint time. A daemon side effect that runs after git exits must never
read `workdir/path` and treat it as the state from the earlier command — the
user may already have changed it. (This rule killed the historical
mtime-guarded snapshot and live-worktree stash-restore races.)

**I4 — Fail-closed rule.** When the facts required by I1–I3 are not available
(e.g. the exact old/new tips of a delayed rewrite cannot be established), the
correct output is *no migration* — attribution gaps are acceptable;
misattribution is not.

## Architecture

```
daemon (trace2 ingestion, see companion doc)
  └─ exact ref transitions + operation classification
       └─ RewriteEvent  ──►  handle_rewrite_event()      src/authorship/rewrite.rs
             ├─ NonFastForward { old_tip, new_tip, onto }   rebase, amend, restack, branch -f
             ├─ CherryPickComplete { sources, new_commits }
             └─ SquashMerge { source_head, squash_commit, onto }
       └─ non-event paths (same module family):
             ├─ backward reset  → rewrite_reset.rs   (working-log reconstruction)
             ├─ stash save/apply/pop → rewrite_stash.rs
             └─ revert → rewrite_revert.rs
```

One event type, one entry point. The daemon normalizes every flavor of
history rewrite (rebase, `pull --rebase`, amend, `commit-tree`+`update-ref`
restacks, squashes) into these events with *exact* commit SHAs. The rewrite
core never guesses what operation happened; that is the ingestion layer's job.

### Lite mode

The `lite_mode` feature flag disables authorship-note migration for rewrites
handled asynchronously by the daemon. In lite mode, ordinary new commits and
regular `merge --squash` commits still write authorship notes, while rebase,
amend, cherry-pick, revert, divergent reset/restack, and direct `update-ref`
rewrites do not create notes for their replacement commits. Working-log moves
that do not rewrite notes are preserved.

This gate belongs only to daemon side-effect dispatch. Explicit `git-ai ci`
commands continue to use the rewrite core regardless of lite mode.

## Core note-shift algorithm

Given a set of `(source_commit, destination_commit)` mappings:

1. Batch-read all source and destination notes (`notes_api::read_notes_batch`).
2. Resolve every unique commit SHA to its tree SHA in one `git rev-parse`.
3. Run one `git diff-tree --stdin -p -U0 -M -r` over all tree pairs.
4. Parse hunks and renames per pair.
5. Compute preserved segments (regions outside all hunks) with cumulative
   line-number offsets (`hunk_shift.rs::build_preserved_segments`).
6. Shift attestation ranges that fall in preserved segments; renames carry
   attribution to the new path; ranges overlapping any hunk are dropped (I2).
7. Merge with any existing destination note (conflict-resolution checkpoints
   may have already written attribution there); attestations dedupe by
   `(file_path, hash)` with range union; metadata merges first-wins.
8. Update `metadata.base_commit_sha` to the destination commit.
9. Batch-write destination notes (`notes_api::write_notes_batch`).

Performance contract: the number of spawned git processes per rewrite batch is
O(1), not O(commits) or O(files). Work proportional to history size happens
*inside* single git invocations (`diff-tree --stdin`, `range-diff`,
`rev-list`, `log --stdin` + `patch-id --stable`), which is the floor git
itself imposes.

Known, intentional limitation: the algorithm is line-based. Content rewritten
inside a hunk loses old attribution even if a human would call it "the same
line, reworded". Fuzzy matching is a heuristic layer banned by I1.

## Per-operation semantics

### Non-fast-forward (rebase, amend, interactive restack, branch -f)

Input: exact `old_tip`, `new_tip`, optional `onto`.

1. `merge_base(old_tip, new_tip)`:
   - `base == old_tip` → fast-forward; nothing to migrate.
   - `base == new_tip` → backward reset; go to the reset path below.
   - otherwise → genuine rewrite.
2. `git range-diff` over `base..old_tip` vs `base..new_tip` yields
   old→new commit mappings, representing reorders, edits, drops, splits, and
   squashes (multiple old → one new).
3. Merge commits are mapped by parent-list correspondence.
4. Run the core note-shift over the mappings.

Conflict resolution during rebase:

- Lines preserved from either side keep that side's attribution (I2 over the
  appropriate source).
- Rewritten conflict-region lines are attributed only via resolution
  checkpoints: AI checkpoint → AI; known-human checkpoint → known human; no
  checkpoint → unattributed (I1). The destination-note merge in step 7 above
  is where checkpoint-derived resolution attribution and migrated source
  attribution combine.
- `rebase --continue` resolution is handled by the same path: the daemon
  reports the final exact transition; resolution checkpoints recorded during
  the stopped rebase supply the new-content evidence.

> **Note (intentional, not a regression).** When a conflict region's *content
> changes* during resolution and no resolution checkpoint covers it, those
> lines are left **unattributed** — even if a pre-conflict source note had AI
> attestation for the old content. This is the fail-closed reading of I1/I4:
> changed content needs fresh evidence. It is a deliberate tightening over the
> legacy `#1079` behavior, which remapped the old source attestation onto the
> rewritten lines by position; that remap violated I1 (similarity/position
> inference) and I2 (git's diff says the region changed), so it was removed.
> Surfacing an attribution gap here is correct; resurrecting stale attribution
> would be misattribution.

### Cherry-pick

Input: exact source commit list, exact created commit list.

Pairing: compute `git patch-id --stable` for both sides in batch; pair equal
patch-ids first; pair the remainder positionally in order. Positional
gap-fill is sound only because both sequences are exact and ordered
(ownership was established upstream); it must never compensate for unknown
ownership. Skipped sources pair with nothing.

`--no-commit` creates no commits; there is nothing to migrate at the commit
layer, and the index/worktree changes only become attributable via
checkpoints or the eventual commit.

### Reset (soft / mixed, backward)

A backward reset un-commits work. The relevant uncommitted content after
`reset --soft|--mixed` is the *old tip's tree*, not the live worktree at
daemon processing time (I3).

1. List undone commits `new_tip..old_tip`; batch-read their notes.
2. Shift each note's attributions into old-tip coordinate space (core
   algorithm), merging chronologically.
3. Batch-read file contents at `old_tip` and `new_tip` trees; keep files whose
   content differs.
4. Write the result as INITIAL working-log data under `new_tip`.
5. Never clear `checkpoints.jsonl` — checkpoints appended between the reset
   and daemon processing are real evidence and must survive.

`reset --hard` discards the work; discarded content gets no reconstruction.
Pathspec reset (`git reset -- path`) only unstages; it does not move HEAD and
needs no note migration.

### Stash

Stash is a working-log migration, not a note rewrite.

- **Save**: persist stash metadata keyed by the stash commit SHA (base commit,
  pathspecs); copy the relevant working-log data into `.git/ai/stashes`;
  remove stashed paths from the live working log.
- **Apply/pop onto the same base**: restore the saved working-log data under
  the target head.
- **Apply/pop onto a different base**: reconstruct the applied content from
  the stash commit's trees plus the target head using
  `VirtualAttributions` and content mapping over immutable blobs
  (`batch_read_paths_at_treeishes`) — never the user's live worktree (I3).
- Stash identity is the stash *commit SHA*. `stash@{N}` is mutable and may
  only be resolved inside a cursor-bounded command boundary (ingestion doc).

### Squash merge

Input: exact `source_head`, created `squash_commit`, exact `onto`.

1. `merge_base(source_head, onto)` → list source commits.
2. Batch-read all source notes; shift intermediate notes into source-head
   coordinates; merge into one log (many-to-one).
3. Shift the merged log from `source_head` to `squash_commit`.
4. Merge with any existing squash-commit note (conflict-resolution
   checkpoints), and with the working log on `onto` if one exists (the squash
   commit also commits any locally checkpointed resolution work).

`merge --squash <immutable-oid>` is exactly recoverable even cold. `merge
--squash <branch>` delayed-and-cold is recoverable only with a cursor (I4
otherwise).

### Revert

A revert can resurrect previously deleted lines. Restored lines recover the
attribution they had when they last existed: shift the reverted commit's
parent-side attribution through the diff onto the revert commit, clipped to
lines the revert actually re-introduced. Uncheckpointed *novel* lines in a
conflicted revert remain unattributed (I1).

### Amend

Amend is a 1→1 non-fast-forward: `old_tip = HEAD@{1}`, `new_tip = HEAD`,
mapped directly (range-diff degenerates to one pair). The amended commit's
note merges migrated attribution with the working log's new checkpoint
evidence via the normal post-commit path.

### commit-tree / update-ref restacks (graphite-style)

Tools like Graphite rewrite stacks with plumbing: `git commit-tree` +
`git update-ref refs/heads/X <new>`. There are no porcelain hooks to observe;
the ingestion layer recognizes the update-ref transition (old tip → new tip on
a branch ref) and emits `NonFastForward`. `git merge-tree` is read-only tree
arithmetic and never triggers migration by itself; only the subsequent
`update-ref`/`commit` does.

## What was removed (and must stay removed)

The legacy machinery is deleted, not deprecated:

- `src/authorship/rebase_authorship.rs` (monolithic per-op rewriter)
- `src/git/rewrite_log.rs` and `.git/ai/rewrite_log` (pre/post-hook event journal)
- `src/commands/hooks/{rebase,stash,push}_hooks.rs` (wrapper pre/post hooks for
  rewrite ops — superseded by daemon trace2 ownership)
- `src/commands/squash_authorship.rs`
- `src/git/diff_tree_to_tree.rs` (per-pair diff spawning)
- mtime-guarded worktree snapshots and any live-worktree read in rewrite or
  post-commit side effects

Any reappearance of these patterns — per-commit git spawns in a rewrite loop,
live-worktree reads in delayed side effects, mutable `.git/rebase-*` reads for
completed commands — is a regression against this spec.

## Test obligations

- Deterministic coverage per operation family lives in
  `tests/integration/rewrite_ops_attribution.rs`, `tests/integration/reset.rs`,
  `tests/integration/stash_attribution.rs`, `tests/integration/squash_merge.rs`,
  `tests/integration/pull_rebase_ff.rs`, `tests/integration/rebase*.rs`,
  `tests/commit_tree_update_ref.rs`, plus unit tests in `rewrite.rs` and
  `hunk_shift.rs`.
- Every conflict-resolution mode (keep-ours, keep-theirs, keep-both in both
  orders, AI rewrite, known-human rewrite, uncheckpointed rewrite, delete-both)
  must have a deterministic test asserting all three attribution classes.
- Line-level attribution is asserted after every commit in every test
  (`assert_committed_lines` / `assert_lines_and_blame`).
- The attribution fuzzer (companion doc) pressures the composition of these
  operations; every fuzzer find becomes a minimized deterministic regression.
