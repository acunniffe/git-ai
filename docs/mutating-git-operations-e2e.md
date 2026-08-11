# Mutating Git operations: attribution E2E campaign

This document and the adjacent CSV are the living acceptance ledger for the
TestRepo campaign derived from the production shell-command census at
`reports/recent-session-git-command-shapes-2026-08-10.csv` in the
`attribution-bug-squasher` project. The fixed census window is 2026-07-20
20:21:28.394 UTC through 2026-08-10 20:21:28.394 UTC: 32,967 normalized Git
shapes and 2,382,802 literal invocations.

## Coverage standard

“Comprehensive” means every semantic partition in the CSV has a deterministic
TestRepo oracle. It does not mean copying all 32,967 value-level CLI rows into
tests. Equivalent paths are grouped only when they produce the same ref,
index, worktree, and commit-graph behavior. Behavior-changing options remain
separate rows.

Every completed row must assert the relevant subset of:

- exact line attribution after every created or rewritten commit;
- a valid authorship note on every destination commit, not only final `HEAD`;
- preservation of prior AI, known-human, and unknown lines outside the target;
- exact ref graph and per-worktree working-log base;
- success, no-op, pre-mutation failure, and partial-mutation nonzero outcomes;
- continuation state (`--continue`, `--skip`, `--abort`, `--quit`) where Git
  exposes it;
- direct, `git -C`, nested-CWD, linked-worktree, and representative Bash wrapper
  contexts without multiplying every semantic test by every syntactic spelling.

Tests must run Git through `TestRepo::git`, `git_from_working_dir`, or
`git_with_env` when trace ingestion is part of the contract. `git_og` and raw
`Command::new("git")` are fixture-only and do not count as daemon E2E coverage.

## Attribution policy

Operations that rewrite or copy existing commits (`rebase`, `cherry-pick`,
`merge`, `revert`, Graphite/plumbing rewrites) preserve source provenance first.
An enclosing AI Bash call may fill only genuinely unknown lines introduced by
that call; it must not blanket-paint copied history as AI.

`git am` is special: it imports patch content and commits it in the same Git
root. Format-patch mail carries no git-ai note. When the root overlaps an agent
Bash call in the same repository, every unknown line introduced in every
destination commit belongs to that session. Outside such a call, `am` alone is
not evidence of AI authorship.

## First confirmed defect: `git am`

At campaign start, `am` appeared in daemon informational write-op logging but
was absent from ordered mutator classification, the analyzer registry, the ref
cursor, and TestRepo completion tracking. A real Codex PreToolUse → `git am` →
PostToolUse test produced a clean applied commit with no authorship note.

The implementation under test now:

1. classifies application/control modes as ordered mutators while dropping
   `--help` and `--show-current-patch*` as read-only;
2. captures the complete `am:` reflog span, including a successful prefix from
   a command that later exits nonzero;
3. emits one batch semantic event containing all created commits;
4. batches parent→commit diffs and note writes so process count does not grow
   with mailbox size;
5. correlates recovery to the exact overlapping Bash-command window and repo,
   avoiding the three-second open-call heuristic and supporting files deleted
   by a later patch in the same mailbox.

## Second confirmed defect: index-only Bash mutations

`git apply --cached` followed by `git commit` initially produced a human-only
note because the patch never touched the worktree and therefore had no mtime
checkpoint. A tempting command-window fallback would incorrectly claim work
that was staged before the agent call.

The implementation now snapshots the index as a tree at Bash `PreToolUse` and
persists that boundary with the Bash-call record. Commit recovery intersects
the pre-index→commit added lines with the commit's unknown lines. It also
validates already-present attestations against that boundary, closing the race
where `PostToolUse` checkpoints a worktree file before asynchronous commit
finalization. Tests cover `--cached`, line-granular `--index` with human and AI
changes in the same file, ordinary worktree apply followed by a later commit,
successful read-only `--check`, and failed-check provenance isolation.
The remaining patch mechanics run under real Codex Bash windows: three-way
index application, reverse application, a context-rich `--reject` command that
applies one hunk before exiting nonzero, and patch content streamed on stdin.
Each resulting commit has an exact line oracle, including the untouched side of
the partial failure.

## Restore, clean, and abandoned operation state

Path-level `git restore` and `git clean` were previously opaque roots, so
discarded uncommitted checkpoints could survive and falsely reappear if the
same bytes were created later. They are now ordered workspace operations.
Worktree restore removes only affected path provenance; staged-only restore
preserves it. Clean prunes checkpoint paths that actually disappeared while
leaving tracked modifications and dry runs untouched.

`restore --source` needs copy semantics rather than deletion semantics. The
daemon records the source tree and path set until commit, reconstructs the
source paths' full history-aware virtual attribution, shifts it to the
destination, and merges it into the new note. Reset and checkout also clear
pending restore and squash-copy state, fixing the unrelated-commit session leak
after an abandoned `merge --squash`.

Conflict-stage `restore --ours/--theirs` now has the same exact human/current
and AI/source-side oracles as checkout, with unrelated pending AI carried
through the merge resolution. Source restoration also reads LF- or
NUL-delimited `--pathspec-from-file` inputs relative to the command worktree;
without this, Git copied the right blobs but the deferred source-note mapping
had an empty path set.

Tracked removal has the same stale-provenance hazard as restore and clean, but
`git rm --cached` is an important inverse case: it removes only the index entry
and leaves the attributed worktree bytes available for re-add. Mutating `rm`
commands are now ordered workspace roots. Successful worktree removals prune
only checkpoint paths that are actually absent; recursive removal does not
disturb an unrelated AI edit. Cached removal and dry-run modes leave working
provenance unchanged.

The ignored `clean -fdx` case exposed an asynchronous-state race. By the time a
side effect ran, the removed path could already be recreated, staged, and
committed, making both live filesystem existence and the live index misleading.
Destructive workspace roots now carry their command-time `HEAD` plus a compact
index-tree OID captured at trace start. Clean selection is evaluated against
that immutable index: a file staged before clean is preserved, while a path
recreated after clean has its stale checkpoint removed even if daemon handling
lags behind the later commit.

Rename detection alone is insufficient for `git mv --force` over an existing
tracked destination: Git can encode that commit as source deletion plus
destination modification, with no rename record. The daemon now derives
file-level mappings from the explicit `mv` operands and the post-command index,
using a constant number of Git processes for directory moves. Pending mappings
compose across chained moves before a commit. Full history-aware source
provenance is shifted against the destination content, so carried lines survive
while edits introduced during the move remain available for normal AI recovery.

`git add` is now an ordered index mutator (except `-n`/`--dry-run`, including
short-option clusters). TestRepo coverage verifies that `-u` excludes an
untracked AI file without losing its checkpoint, and that a NUL-delimited
`--pathspec-from-file` commit carries the unselected AI path into a later
commit. This supplements the existing line-partial staging suite.

## Merge lifecycle and asynchronous state

A conflicted merge is a multi-command transaction: the initial nonzero
`git merge` establishes `MERGE_HEAD`, file edits and `git add` record the
resolution, and `merge --continue`, `--abort`, or `--quit` decides which state
survives. The daemon now snapshots the pre-attempt working log only when
`MERGE_HEAD` actually exists, so a rejected `--ff-only` does not create false
pending state. Abort restores that snapshot and removes discarded resolution
provenance; quit drops only the snapshot and leaves resolution checkpoints for
an ordinary later commit.

`merge --continue` required a separate reflog shape: Git records its commit as
`commit (merge): ...`, not `merge ...`. The ref cursor recognizes that exact
form and the merge-complete side effect finalizes the commit note through the
normal recovery pipeline. Fast-forward merges migrate the old-tip working log
to the new tip, while real merge commits finalize their diff and carry any
unrelated dirty checkpoint forward. TestRepo coverage includes all controls,
pre-mutation rejection, explicit `--no-commit`, fast-forward and two-parent
dirty-state carry, cold daemon starts, conflict resolution, and a two-generation
merge scenario that was previously ignored because of a stale trailing-line
fixture.

## Revert lifecycle and historical restoration

`revert --no-commit` previously lost the source commit identity before the
eventual ordinary `git commit`, turning restored AI lines human. Deferred revert
state now retains the original head and every resolved source OID. The same
state model snapshots a conflicted attempt: abort and skip restore pre-attempt
provenance, quit keeps the live resolution, and continue consumes the pending
sources while preserving AI-authored conflict resolution. Git records the
continue commit as a plain `commit: Revert ...` reflog entry on the tested Git
version, which is now an explicit cursor shape.

Mainline and chained reverts exposed a second issue: the relevant attribution
often lives in an ancestor note rather than directly on the reverted commit's
selected parent. The direct-note path remains the fast path. If it leaves
restored lines uncovered, the implementation walks the pre-revert history once,
batch-reads reachable notes, batch-computes every note-to-destination shift,
and fills ranges newest-first. This recovers every destination in a multi-revert
without a Git subprocess per commit or file; the dedicated scaling test measured
46 daemon Git spawns for both two and eight reverted commits.

## Cherry-pick controls, stdin, and mainline merges

Cherry-pick abort and single-source skip had the same stale-resolution hazard as
merge and revert. A command-time working-log snapshot now follows a conflicted
attempt. Abort and a terminal skip restore it, while quit drops pending source
state without deleting the staged AI resolution. Partial multi-commit commands
can emit both a completed-prefix event and a fresh prepared-attempt event, so the
snapshot is rebased onto the actual surviving tip.

`cherry-pick --stdin` does not leave its source OIDs in trace2. The cursor now
consumes the full `cherry-pick:` reflog span. When sources remain absent, one
bounded fallback scans commits outside the destination ancestry, computes stable
patch IDs in batch, and prefers matching candidates with authorship notes. The
two-source TestRepo case is run repeatedly to exercise the former race.

Mainline picks cannot rely on the merge commit's exact note because side-parent
lines may be inherited from older notes. The mainline path resolves every
selected parent in one command, reads the source commit graph once, determines
each `mainline_parent..source_merge` range in memory, batch-shifts all relevant
notes, clips them to destination-added lines, and merges conflict resolution.
Both one- and two-merge commands are covered. The scaling guard measured 34
daemon Git spawns for both two and six mainline merge commits. `-x`, original
empty commits, and `--empty=drop|keep` also have dirty-checkpoint or provenance
oracles.

Commit-mode coverage now includes NUL `--pathspec-from-file` subset commits,
`-a`, fixup/squash subjects, `-C`/`-c`/`--reuse-message`, and an empty commit
between checkpoint and content commit. A ref-cursor defect surfaced here: for
`--squash=<commit> -m body`, Git records `squash! <target subject>` rather than
the literal body as the reflog subject. The cursor no longer applies an
impossible message constraint for fixup/squash; it uses the ordered transition
and reflog offset instead.

## Rebase control state and updated refs

A failed rebase attempt now snapshots its command-time working attribution.
`--abort` and `--skip` restore that snapshot so AI conflict-resolution bytes
discarded by Git cannot reappear as AI in a later ordinary commit. `--quit`
deliberately keeps the live resolution checkpoint while dropping the pending
transaction. These controls are explicit semantic events rather than being
inferred only from a later non-fast-forward transition.

The ref graph oracles also cover `--update-refs` and
`--rebase-merges --update-refs`: intermediate branches must move to their
rewritten commits and every rewritten destination must retain its source line
provenance. During this validation, an unrelated regression exposed that the
new Bash command-window recovery had conflated every traced Git command with an
actual enclosing Bash tool call. Pre-index scoping and suppression of adjacent
edge recovery now activate only when an overlapping Bash record with a captured
index baseline exists.

## Stash partitions and stack lifecycle

Stash provenance was previously split by pathspec alone and then removed
wholesale from the live working log. That is incorrect for `--keep-index`,
`--staged`, and `--patch`, where Git can divide index and worktree state or even
two hunks in one file. Creation now reconstructs the pre-stash virtual
attribution once, reads the main stash tree plus its untracked/ignored third
parent in batch, and shifts provenance onto both the actual stash snapshot and
the post-command worktree. Explicit pathspecs still bound which paths belong to
the stash because Git's internal WIP tree can contain more paths than it later
applies.

TestRepo coverage verifies staged and unstaged partitions in both directions,
interactive two-hunk selection, ignored files under `--all`, and index-state
restoration with `apply --index`, including linked worktrees. It also found an
option parser bug where `--index` was treated as the stash reference; restore
targets now skip flags before resolving the named stack entry.

`create`, `store`, and `clear` are explicit stash operations rather than the old
unknown/push fallback. A detached `create` remains non-consuming; `store`
copies its provenance into compact stash state while preserving the live copy;
targeted `drop` removes only its selected OID; and `clear` removes every compact
entry. Legacy `stash save` still round-trips AI attribution. The original 48
stash attribution tests and four large-note/round-trip tests remain green.

## Sparse checkout visibility

Sparse checkout changes index visibility, not repository content, so a missing
worktree path must never be treated as deleted attribution. Traced TestRepo
coverage exercises cone and non-cone initialization, sparse-index mode,
argument and stdin `set`, additive patterns, reapply, and disable. A committed
AI path can be hidden and revealed repeatedly with its note unchanged, including
inside a linked worktree.

Git deliberately retains a dirty tracked path even after new sparse rules
exclude its directory. The checkpoint remains live through `set` and `reapply`,
`git add --sparse` commits the pending AI line exactly, and a later reapply can
hide the now-clean file without losing blame. The observed but invalid
`sparse-checkout add --no-cone` shape is also a negative control: mode selection
belongs to init/set, Git rejects the add, and the pattern set remains unchanged.

## Submodule lifecycle and nested repository isolation

`git submodule` was missing from the built-in command resolver, mutation
classifier, family sequencer, and deterministic TestRepo completion tracking.
Lifecycle commands are now ordered while no-argument/status/summary queries
remain read-only. Tests cover local-path add, remote update, forced deinit and
init/update restoration, and `absorbgitdirs`, with exact pending-AI blame in the
superproject and unchanged source attribution.

The nested checkout is a separate repository family even though commands are
launched through the superproject TestRepo. Working-directory helpers now
resolve the actual repository for completion sessions, checkpoint waits, and
read synchronization. An AI checkpoint and commit inside the submodule gets a
nested authorship note; the later superproject gitlink commit independently
retains its own pending AI edit.

## Transport modes and target repositories

The existing suite already covered ordinary/upstream pushes, fast-forward
pulls, rebases, conflicts, and autostash. Gap tests add a real divergent
`pull --no-rebase` merge while unrelated tracked AI work remains dirty, and a
divergent `--ff-only` rejection that leaves HEAD and pending attribution
untouched. Push coverage now includes force-with-lease note publication, remote
branch deletion, and an atomic multi-ref rejection proving that neither remote
ref moves while local pending AI survives.

Fetch tests exercise explicit destination refspecs, a forced remote-tracking
rewind, normal discovery, and prune after remote deletion. Clone target routing
is tested for normal, no-checkout, bare, and file-URL shallow clones; source
authorship notes are fetched into usable clones and the shallow boundary is
real. Normal and bare init targets are also launched through traced TestRepo
commands without affecting the launching repository.

## Repository configuration and maintenance durability

Mixed config and reflog commands now participate in ordering only when they
mutate: config list/get and reflog show/exists remain read-only, while config
writes and reflog expiration are sequenced. The same support was added for gc,
repack, maintenance, and pack-refs, including built-in command resolution and
deterministic TestRepo waits.

Remote add/rename/set-url/remove and local config add/get/unset leave source
notes and pending AI exact. A durability sequence runs immediate-prune gc,
full repack, packed-ref pruning, and expiration of every reflog before the next
commit; both the packed branch/tag and attribution state survive. Maintenance
register, commit-graph/gc run, and unregister provide the equivalent managed
workflow oracle.

## Invocation routing and shell wrappers

Representative commits now run through global `-c`, chained `-C`, and explicit
`--git-dir`/`--work-tree` forms. Nested directories, a parent directory above
the repository, and an unrelated sibling repository all route completion and
attribution to the parsed target. Parent/sibling `-C` initially registered the
session against the launching CWD; TestRepo now reuses the no-exec invocation
resolver before synchronizing or recording a family.

A bounded shell helper replaces a literal Git token with the deterministic
test-sync config while retaining the real Trace2 process tree. It covers `env`,
`timeout`, the shell `command` builtin, `nohup`, a nested shell, a true
conditional, an `update-ref --stdin` pipeline, and a detached background
commit. The final case synchronizes on the Git marker after the launching shell
returns, so success proves completion timing rather than a lucky sleep.

## Fast import and whole-history filters

`fast-import`, `filter-branch`, and `filter-repo` are unstructured ref mutators:
their root command can move several refs without using the porcelain-specific
reflog shapes. The ref cursor now scans only reflog records after the captured
command-start offsets, across the worktree HEAD and common refs, so the work is
bounded to that root instead of rescanning repository history.

An AI Bash `fast-import` fast-forward uses the same command-window/index
recovery as direct `commit-tree` plus `update-ref`. Multi-commit streams are
expanded with one `rev-list --parents` call and every imported commit is
finalized against its real first parent; an outside-agent import remains
unattributed. This prevents an apparently correct final-tip note from leaving
earlier imported files unknown in blame.

Whole-history filters can rewrite the root commit, leaving old and new graphs
with no merge base and making ordinary range-diff return no mappings. Filter
roots now pair both reachable histories in batch using stable patch IDs and an
ordered fallback, then shift all source notes to their rewritten destinations.
Single- and multi-commit environment rewrites plus an index-filter
`--prune-empty` case retain exact surviving AI attribution.

## Tags, notes, symbolic refs, and replacement refs

Ref-only porcelain is covered with exact pending-working-log and source-note
oracles. Lightweight, annotated, forced, and deleted tags; the auxiliary
default notes namespace; custom symbolic refs; and create/delete/graft replace
refs all leave authorship state intact. In particular, exercising ordinary
`git notes` commands verifies that their default namespace cannot corrupt the
reserved `refs/notes/ai` attribution note.

Changing symbolic `HEAD` directly to a branch at an older tip exposed a shared
backward-move bug. Git emits a blank-message HEAD reflog transition, but reset
reconstruction archived the old-base working log before its pending checkpoints
were moved. Backward reconstruction now first merges that live log into the new
base and then overlays attribution recovered from unwound commit notes. The
focused symbolic-HEAD case and existing soft/mixed reset tests retain exact AI
blame.

## Command-window recovery and empty merge-note hygiene

Every traced commit has a Git command start/finish interval, but that does not
mean the edit-producing Bash tool call overlaps the commit itself. The first
command-window implementation replaced the established mtime recovery path and
lost attribution when an agent edited a file in Bash, returned from the tool,
and committed later. Recovery now uses the exact window when a Bash call
overlaps it and falls back to mtime only when no call overlaps. An overlapping
call authoritatively discovered in another repository blocks that fallback, so
an imported patch cannot steal a concurrent agent session. The full 17-test
Bash recovery module and the opposing cross-repository `git am` controls cover
both sides of that boundary.

Real merge commits are finalized by the daemon because they do not always pass
through the ordinary commit-hook shape. Clean human merges initially acquired
a schema-only `refs/notes/ai` note as a side effect. The merge-specific
finalizer now suppresses a note only when the computed result has no
attestations; attributed source lines, pending dirty AI work, and AI conflict
resolution still produce notes. Four direct/pull-rebase leak regressions plus
the merge-control, merge/rebase, and squash-merge suites verify the policy.

## Checkout, branch, and linked-worktree ref topology

Checkout and switch now have exact TestRepo oracles for create/reset (`-B`,
`-c`, and `-C`), detach, successful and refused remote tracking, and orphan
branches. Orphan transitions exposed a real loss: Git makes HEAD unborn without
the ordinary old-to-new reflog row, so the pending working log stayed under the
old commit while the first root commit consumed the special `initial` slot.
The ref cursor now emits an explicit old-HEAD-to-unborn transition and the side
effect moves the log into `initial`; failed orphan commands emit no transition.

Conflict-stage checkout is covered separately. `--ours` selects the current
human side, `--theirs` restores the AI-attributed source side, and an unrelated
pending AI file survives either resolution. Existing force-discard, merge,
pathspec, same-branch, mixed-attribution, and linked-worktree variants remain
green.

Branch-only ref mutations must never relocate a working log because the
worktree's base commit has not changed. Five tests cover create/force-reset,
safe and forced single/multi-delete, delete/recreate cursor reuse, current and
explicit rename, current/explicit/forced copy, both upstream option spellings,
unset-upstream, track, and no-track. Every sequence is followed by a real
commit with exact pending-AI blame.

Linked worktrees are now exercised through traced `TestRepo::git` roots rather
than raw setup helpers. Normal inferred branches, explicit `-b` and `-B`,
detached, orphan, and no-checkout adds all checkpoint and commit from the linked
CWD, proving isolated working logs and shared authorship notes. Lifecycle tests
cover clean and forced removal, move with an outstanding checkpoint,
lock/reason/unlock, repair, prune of a missing sibling, and same-path recreation
without stale AI leakage. Clean removal found a daemon race: generic
non-fast-forward detection eagerly reopened the command worktree even when the
command had no history transition, after Git had already deleted that path.
Repository discovery is now delayed until a qualifying ref transition exists.

## Status meanings

- `covered-existing`: audited line-level TestRepo coverage already meets the row.
- `covered-new`: added in this campaign and passing.
- `partial`: useful tests exist but one or more semantic/outcome partitions are missing.
- `red`: a deterministic failing regression exists and awaits a fix.
- `planned`: no sufficient line-level TestRepo oracle yet.
- `negative-only`: the supported contract is artifact survival/no attribution change.

Update the CSV in the same change as each test/fix. A row is not complete based
only on note existence, filesystem-change detection, raw-Git fixtures, ignored
tests, or external Graphite tests.

## Final validation

The completed ledger contains 77 semantic partitions: 63 `covered-new` and 14
`covered-existing`, with no `partial`, `red`, or `planned` rows. Final validation
on 2026-08-11 used serialized integration execution to avoid known parallel
daemon/test-environment interference:

- `cargo check --all-targets`: passed;
- `cargo test --lib --quiet`: 2,117 passed, 0 failed;
- `cargo test --test integration -- --test-threads=1`: 3,384 passed, 0 failed,
  85 intentionally ignored.
