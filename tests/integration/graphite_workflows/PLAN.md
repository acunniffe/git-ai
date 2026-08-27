# Graphite Workflow Attribution Suite — Plan

Goal: an exhaustive, empirically-grounded scenario suite proving that AI
attribution survives (or documenting exactly where it does not survive)
**every** Graphite workflow permutation. 540 scenarios across 19 families,
generated from `generate_matrix.py` (single source of truth; IDs are stable
and append-only) into `scenarios.json`.

Assertion convention: every scenario writes files whose content is **100%
AI-authored** via the `mock_ai` checkpoint harness, so the invariant is
maximally simple — *after the workflow, `git-ai blame` must attribute every
line of every AI file to the AI*, asserted with
`TestFile::assert_committed_lines(lines![...all .ai()])` (plus
`read_authorship_note` presence checks on every rewritten commit). Any
deviation is either an application bug or a harness bug; the triage protocol
below distinguishes them. **We do not expect all scenarios to pass** — at
this point a failure is at least as likely to be a real attribution bug as a
test bug.

## Part 1 — Empirical workflow catalog

Evidence sources: (a) raw Graphite debug logs from this machine (exact
ordered sequences; gt 1.8.6), (b) redacted command-shape summaries from six
more engineers (1,971 invocations; incl. gt 1.7.13), (c) string mining of
the gt 1.8.6 binary, (d) the upstream restack-undo repro
(`scripts/repro/daemon-restack-undo-mapping-bomb`, PRs #1976/#1978/#1983).
Confidence tags: [E] fully empirical ordered sequence; [S] shape-set
reconstruction (commands certain, exact order inferred); [B] binary-derived.

### gt sync (restack variant) — [E], 107-command trace

Anatomy (attribution-relevant core only):

```
branch --show-current ; rev-parse HEAD
stash create                                  # refless snapshot commit
ls-files --others --exclude-standard          # untracked inventory
fetch --no-write-fetch-head --no-tags -f origin <refs>
update-ref refs/heads/<trunk> <sha>           # trunk move (not checked out)
update-index --refresh ; status -z
reset -q --keep <sha> --                      # trunk move (CHECKED OUT)  ⚠
# per stack branch, bottom-up:
cat-file -p <sha>~                            # read old parent
commit-tree <sha>^{tree} -p <sha>~ -m _       # SYNTHETIC base commit
merge-tree --allow-unrelated-histories <synthetic> <branch-tip>
commit-tree <merged-tree> -p <new-parent> -m <original message>
update-ref refs/heads/<branch> <new-sha>      # branch move
# epilogue:
stash create ; ls-files --others              # second snapshot
```

Key facts: **no `git rebase` anywhere**; the checked-out branch moves via
`reset -q --keep` (the exact #1976 forward-reset shape) — in EVERY sync
where trunk advanced, not only undo; rewritten commits are minted with
`commit-tree` (no commit hook, no HEAD movement until update-ref); a
mid-restack failure was captured live (`cleanRebaseMergeTree` exit 1)
after which gt proceeded with remaining branches.

### gt restack — [S] same rewrite core, no fetch/trunk-move prologue.

### gt create — [S] stash create → branch creation → `commit [-m] [-q]`
(fleet shapes: `commit -m <arg>+ -q`, `commit -q`, `commit -p -q`) →
metadata refs. Stash-wrapped commit hazard fires here in fleet data.

### gt modify — [S] amend flow: `commit --amend`-shape on HEAD (fleet:
`commit` bare/`-a`/`--no-interactive` variants) followed by the restack
core for all descendants. Heaviest users in fleet data are the engineers
from the empty-note incident.

### gt submit — [S] restack-if-needed, then per branch:
`push <remote> --force-with-lease[=<ref>]… --progress --no-verify --atomic`.
100% of fleet submits carry `--no-verify` — hooks NEVER run on submit.

### Conflict fallback: gt continue / gt abort — [S]+[B]
When `merge-tree` output is conflicted (`cleanRebaseMergeTree` fails), gt
falls back to a REAL `git rebase` for that branch; binary strings confirm
`rebaseInProgress`, "Hit a conflict during rebase",
`git rebase --abort` (gt abort), and continue flows. Fleet: `gt continue`
used by 3/7 engineers, `gt abort` by 1/7.

### gt undo (restack undo) — [B]+repro
`git reset -q --keep <pre-restack-tip>` moving the branch BACKWARD across
the fork point (the #1978/#1983 mapping-bomb shape; #1976 working-log
orphan shape forward).

### gt move --onto — [S] rewrite core with a non-trunk `onto`; descendants
restacked.

### Housekeeping (checkout/co/track/trunk/info/log) — [E for info] read-only
+ metadata refs; control group.

### gt 1.7.13 flavor — [S] hash-object -w --stdin storms (4,023 in one
engineer's week) + `for-each-ref --sort` + eager metadata `update-ref`
bursts fired even from read commands.

### update-ref --stdin — [B] batch ref moves in one git process
(`updateRefs`): the daemon sees ONE command moving MANY refs.

### Error/undo semantics observed
- merge-tree conflict → per-branch fallback (rebase) or skip; sync
  continues with remaining branches [E: exit-1 capture].
- dirty-worktree refusal: "stash your unstaged changes" guidance;
  `stash --keep-index` suggested [B].
- queue-eviction recovery: gt instructs `gt checkout; git reset; git stash
  pop` [B] — i.e. user-driven reset+stash-pop is a sanctioned flow.
- `gt abort`: `git rebase --abort` + gt snapshot restore [B].

## Part 2 — Harness design

`tests/integration/graphite_workflows/` (module registered in
`tests/integration/main.rs`):

- `gt_sim.rs` — the simulation layer. One function per workflow family
  replaying the EXACT command sequences above via `TestRepo::git` (traced)
  or `git_og_with_env` + TRACE2_DISABLED (blind phases), parameterized by
  `Scenario`. Synthetic-base restack implemented faithfully:
  `cat-file`/`commit-tree -m _`/`merge-tree`/`commit-tree`/`update-ref`
  (individual or `--stdin` per the `refstyle`), `reset -q --keep` for
  checked-out moves, `stash create` snapshots at the boundaries.
- `scenario.rs` — `Scenario` struct + loader for `scenarios.json`
  (serde, `include_str!`).
- `stackbuilder.rs` — constructs the five stack shapes with `mock_ai`
  checkpoints so every content line is AI-attributed; returns a
  `StackState` (branch → (files, expected lines, commit sha)).
- `assertions.rs` — post-workflow invariant checker: for every AI file on
  every surviving branch, `assert_committed_lines(all .ai())`; for every
  rewritten commit, authorship note exists with non-empty attestations;
  for `blind` scenarios, run the daemon-restart + traced-poke recovery
  step first (reconciliation window), then assert.
- Family test files (`sync_ff.rs`, `sync_restack.rs`, …): one `#[test]`
  per (family × observation × repeat) bucket iterating its scenario rows;
  failures collected per scenario ID and reported in one panic message
  per bucket, e.g. `GT-SYNC_RESTACK-042: feature.txt line 3 blamed human,
  expected ai`. This keeps test-symbol count ~45 while preserving
  per-scenario triage.

Runtime budget: fresh `TestRepo` (shared-daemon scope) per scenario,
~5-15 s each → 540 scenarios ≈ 1.5-2.5 h wall with `--test-threads=4`.
Run load-gated; never overlap with other cargo runs (see triage log from
2026-07-26: overloaded-host runs produce mass false failures).

## Part 3 — Triage protocol

For each failing scenario ID:
1. Re-run its bucket alone on a calm machine; discard load-flakes.
2. Reproduce manually at minimal scale; inspect `git-ai blame`, the note
   content, and the daemon log.
3. Classify: **(A) genuine attribution loss** — file issue reference and
   suspected mechanism (empty-note / stranded translation / working-log
   orphan / mapping fabrication / blind-window); mark the scenario in
   `EXPECTED_FAILURES.md` with the classification, keep the test failing
   in a dedicated `#[ignore = "known: <mechanism>"]` bucket variant;
   **(B) harness bug** — fix the harness, re-run.
4. Group A-failures by mechanism for the PR report.

## Part 4 — Scenario matrix

Counts by family (total **540**; full table = `generate_matrix.py
--markdown`, canonical data = `scenarios.json`):

| family | n | core risk it probes |
|---|---|---|
| SYNC_FF | 32 | forward `reset --keep` on checked-out trunk (#1976) |
| SYNC_RESTACK | 160 | merge-tree/commit-tree/update-ref rewrite + note translation |
| RESTACK | 40 | rewrite core without fetch noise |
| CREATE | 16 | stash-wrapped fresh commits |
| MODIFY | 48 | amend + descendant restack (empty-note incident shape) |
| SUBMIT | 12 | --no-verify --atomic pushes; note push/sync behavior |
| CONFLICT_CONTINUE | 24 | rebase fallback mid-gt-flow, resolve, continue |
| CONFLICT_ABORT | 16 | rebase --abort + snapshot restore round-trip |
| UNDO | 24 | backward reset --keep across fork point (#1978/#1983) |
| MOVE_ONTO | 8 | non-trunk onto + descendant moves |
| HOUSEKEEPING | 4 | control: metadata flows must be attribution-inert |
| FLAVOR_17X | 4 | 1.7.x hash-object/metadata storms |
| WORKTREE | 40 | linked-worktree execution (per-worktree logs/reflogs) |
| REFSTDIN | 40 | single update-ref --stdin moving many refs |
| RENAME | 12 | rename-following through merge-tree rewrites |
| MERGE_IN_STACK | 4 | merge-commit mapping in restacks |
| PARTIAL_STAGE | 16 | staged/unstaged/untracked mix at stash boundaries |
| LIFECYCLE | 32 | create→modify→sync→submit chains end-to-end |
| NESTED_CONFLICT | 8 | two conflicts in one sync |
