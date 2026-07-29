# Graphite Workflow Suite — Failure Taxonomy

First full run: 2026-07-28, branch `graphite-workflow-attribution-suite`,
540 scenarios / 45 buckets. **332 pass, 208 fail** (2,045 itemized
violations). Failures cluster into the mechanisms below; each has been
triaged per PLAN.md Part 3 with representative scenarios re-run in
isolation on a calm machine.

## Validated-healthy areas (all scenarios pass)

- `SYNC_FF` (32/32): forward `reset -q --keep` trunk moves carry the
  working log — upstream #1976 confirmed effective, traced and blind,
  including twice-idempotency.
- `UNDO` (24/24): backward `reset --keep` across the fork point loses
  nothing and fabricates nothing — #1978/#1983 confirmed effective.
- `MODIFY` traced (24/24): amend + descendant cascade preserves
  attribution when observed.
- `CONFLICT_ABORT` blind (8/8): abort restores pre-workflow tips and notes.
- `HOUSEKEEPING`, `FLAVOR_17X` traced: metadata flows and 1.7.x object
  storms are attribution-inert.

## Mechanism 1 — blind gt rewrites are unrecoverable (GENUINE, ~154 scenarios)

**Signature:** observation=blind × attribution=committed →
`human_blame` + `missing_note` on every rewritten commit.
**Families:** SYNC_RESTACK, RESTACK, REFSTDIN, WORKTREE, MODIFY,
MOVE_ONTO, RENAME, LIFECYCLE (blind clusters).
**Triage verdict: GENUINE** (verified on GT-RESTACK-002; traced twin
GT-RESTACK-001 passes in the same run, isolating the loss to the blind
dimension). Evidence: the intact 457-byte note remains attached to the
old pre-restack commit (unreachable except via reflog); the rewritten
commit has none; branch reflog contains only `commit` →
`reset: moving to <sha>` → `commit: poke` — **no rebase spans exist**,
because gt's plumbing path never runs `git rebase`. After restart the
recovery poke cold-seeds a fresh `RefCursor` clamped to its own reflog
entry (`ref_cursor.rs::initialize_from_command_reflog_start_offsets`),
deliberately skipping the blind rows as prior untraced history; note
translation (`handle_non_fast_forward_rewrite`) only runs on
live-observed moves. Main has NO unobserved-rewrite reconciliation, and
pending upstream PR #1961 keys strictly on `rebase (start)/(finish)`
spans — it would not fire on this shape either.
**Upstream direction:** generalize post-restart reconciliation from
rebase spans to arbitrary unobserved non-fast-forward branch-ref moves:
screen recent reflog transitions whose old tip carries an ai note and
whose new tip lacks one, then replay note translation across the
old→new pair. (Server-side backfill from pre-rewrite noted heads remains
the retroactive remedy.)

## Mechanism 2 — working-log loss through traced restacks (GENUINE, ~54 scenarios)

**Signature:** attribution=uncommitted|mixed, even observation=traced →
pending AI lines blamed human; their eventual commit carries an
empty-attestation note (the production "empty note" shape).
**Families:** SYNC_RESTACK, RESTACK, REFSTDIN, WORKTREE, LIFECYCLE,
SUBMIT, PARTIAL_STAGE, CONFLICT_CONTINUE (traced mixed/uncommitted
clusters).
**Triage verdict: GENUINE — three distinct proximate defects, one shared
signature** (verified with minimal repros outside the harness; forward
controls pass, so #1976 itself is confirmed working):

- **(A) Sideways `reset -q --keep` onto a commit-tree-minted sibling**
  (gt's core restack move of the checked-out branch): the daemon's Reset
  handler renames the working log only for ancestor-related moves
  (`src/daemon.rs:5648-5673`; #1976 covers forward only). The minted
  replacement is a sibling, so the flow falls through to
  `handle_non_fast_forward_rewrite`, which shifts committed notes but
  never the working log. Pending checkpoints stay keyed to the
  unreachable old tip → the eventual commit mints an empty-attestation
  note; pending lines blame human. Verified on GT-RESTACK-005; minimal
  4-command repro in the triage log.
- **(C) Cross-worktree `update-ref`**: the update-ref working-log carry
  (`src/daemon.rs:5713-5766`) requires the move to affect the *invoking
  command's* HEAD — a branch checked out in a different worktree records
  no HEAD reflog entry there, so even plain fast-forward moves strand the
  log. Drives the WORKTREE mixed/uncommitted clusters on all shapes.
- **(D) `stash pop -q` restore never runs**: `enrich_stash`
  (`src/daemon/ref_cursor.rs:916-931`) passes `stash_args.get(1)` as the
  stash target without stripping flags, so `-q` fails target resolution
  and the attribution saved at `stash push` (verified present in
  `.git/ai/stashes_v2/<sha>/INITIAL`) is never restored. All upstream
  stash tests are flag-less. Drives CONFLICT_CONTINUE.

**Upstream direction:** carry the working log old→new in the Reset
handler's non-ancestor arm and in the update-ref handler for branches
checked out in *any* worktree (mirroring the unconditional rename that
`apply_checkout_switch_working_log_side_effect` already performs for
checkouts), and skip flags when resolving stash pop targets (default
`stash@{0}`).

## Mechanism 3 — MERGE_IN_STACK blame failure (HARNESS BUG, 4 scenarios)

**Signature:** `git-ai blame <file> failed: ... Failed to canonicalize
file path ... No such file or directory` + missing note.
**Verdict:** harness artifact — NOT a git-ai bug. `git-ai blame` never
crashed; the harness's restack core assumes single-commit branches and,
for the two-commit merge-carrying branch, anchors the synthetic base at
the branch's own AI commit (`old_tip~`) instead of the old stack base.
The merge-tree then resolves to exactly the new trunk tree, deleting the
branch's AI file. **FIXED in the harness** (commit `215634667`): branch
commits are enumerated via `rev-list --reverse --first-parent` and
replayed bottom-up with per-commit old→new mapping; merge commits are
minted as real merges with remapped parents. Post-fix outcomes: both
traced scenarios PASS with zero violations; both blind scenarios fail
purely via Mechanism 1 (content survives, rewritten commits lack notes)
and are reclassified there.

**Side observation from triage** (needs follow-up, possibly genuine): the
daemon attached an empty-attestation note to a *raw, trace2-disabled*
human trunk commit (`base_commit_sha` = itself, one mock_ai session, no
file entries) — a note mis-landed on a commit the daemon nominally never
observed.
