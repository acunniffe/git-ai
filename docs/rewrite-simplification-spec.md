# Authorship Rewrite Simplification Spec

## Overview

Replace the entire rewrite_log / per-operation-type / mid-operation-interception system with a single unified algorithm: when authorship notes need to follow code through history rewrites, use `diff-tree` to shift line-level attributions between old and new commit trees. The git wrapper/proxy is fully removed — only the daemon flow matters.

## Goals

- Delete ~16,200 lines of rewrite machinery + dead code (~800-1,000 lines of new code written, net reduction ~15,200-15,400)
- One core function (`shift_authorship_notes`) for all commit-rewriting operations (rebase, amend, cherry-pick, squash, reset)
- A single entrypoint (`handle_rewrite_event`) that normalizes all commit-rewrite types into a common `Vec<(source, new)>` mapping before calling the core function
- Stash handling is a separate lightweight path (it migrates working logs, not authorship notes on commits)
- No persistent rewrite_log, no mid-operation interception
- Minimal in-memory state (only for cherry-pick conflict flow)
- Best-effort attribution through rewrites: hunks that changed get invalidated, everything else shifts

---

## Architecture: Single Entrypoint

All rewrite handling flows through one function:

```rust
/// Single entrypoint for ALL authorship rewriting.
/// Normalizes any rewrite event into commit mappings, then shifts notes.
pub fn handle_rewrite_event(repo: &Repository, event: RewriteEvent) -> Result<(), GitAiError> {
    let mappings: Vec<(String, String)> = match event {
        RewriteEvent::NonFastForward { old_tip, new_tip } => {
            derive_mappings_from_range_diff(repo, &old_tip, &new_tip)?
        }
        RewriteEvent::CherryPickComplete { sources, new_commits } => {
            sources.into_iter().zip(new_commits).collect()
        }
    };

    if mappings.is_empty() {
        return Ok(());
    }

    shift_authorship_notes(repo, &mappings)?;
    migrate_working_log_if_needed(repo, &mappings)?;
    Ok(())
}
```

The daemon's job is reduced to: detect which `RewriteEvent` occurred, then call `handle_rewrite_event`. The entrypoint handles:
- Mapping derivation (range-diff for non-FF, positional for cherry-pick)
- Squash detection (internal to `derive_mappings_from_range_diff`)
- The core diff-tree + shift algorithm
- Working log migration

No operation-specific logic leaks beyond the `RewriteEvent` enum.

```
┌─────────────────────────────────────────────────────────────┐
│ Daemon (trace2 listener)                                    │
│                                                             │
│  Detects:                                                   │
│    • Non-FF ref move on refs/heads/* ──┐                    │
│    • Cherry-pick completion ───────────┘                    │
│                                         ▼                   │
│                              ┌─────────────────────┐        │
│                              │ handle_rewrite_event │        │
│                              │  (single entrypoint) │        │
│                              └─────────┬───────────┘        │
│                                        │                    │
│                    ┌───────────────────┼───────────────┐    │
│                    ▼                                   ▼    │
│        derive_mappings_from              (cherry-pick:      │
│          _range_diff()                    already have      │
│          (incl. squash detection)         mappings)         │
│                    │                          │             │
│                    └───────────┬──────────────┘             │
│                                ▼                            │
│                   ┌────────────────────────┐                │
│                   │ shift_authorship_notes  │                │
│                   │  (core: diff-tree +    │                │
│                   │   shift per pair)      │                │
│                   └────────────────────────┘                │
│                                │                            │
│                                ▼                            │
│                   ┌────────────────────────┐                │
│                   │ migrate_working_log_if │                │
│                   │   _needed              │                │
│                   └────────────────────────┘                │
└─────────────────────────────────────────────────────────────┘
```

---

## What Gets Deleted

### Entire files

| File | Lines | Reason |
|---|---|---|
| `src/commands/git_handlers.rs` | 585 | Git wrapper proxy |
| `src/commands/git_ai_handlers.rs` | 1,189 | Wrapper dispatch |
| `src/commands/git_hook_handlers.rs` | 615 | Core hooks feature |
| `src/commands/hooks/mod.rs` | 3 | Hooks module |
| `src/commands/hooks/rebase_hooks.rs` | 166 | Wrapper rebase hooks |
| `src/commands/hooks/stash_hooks.rs` | 353 | Wrapper stash hooks |
| `src/commands/hooks/push_hooks.rs` | 203 | Wrapper push hooks (useful functions moved to sync_authorship.rs) |
| `src/commands/install_hooks.rs` | 1,330 | Hook installation (extract `configure_daemon_trace2` + `ensure_daemon` ~55 lines to new `src/commands/install.rs` before deletion) |
| `src/commands/ci_handlers.rs` | 346 | CI wrapper dispatch (core CI logic migrated) |
| `src/git/rewrite_log.rs` | 710 | Rewrite log |
| `src/authorship/rebase_authorship.rs` | 4,786 | All per-operation rewrite logic |
| `src/commands/squash_authorship.rs` | 361 | Dead — only caller is deleted git_ai_handlers |
| `src/authorship/range_authorship.rs` | 478 | Dead — only caller is deleted git_ai_handlers |

### Gutted from daemon.rs (~3,850 lines)

**Key insight**: `maybe_apply_side_effects_for_applied_command` currently calls `rewrite_events_from_semantic_events` → `apply_rewrite_side_effect`. After this rewrite, that call chain is REPLACED with the new non-FF detection + `handle_rewrite_event`. The caller survives but its rewrite-related logic is rewritten.

**Utility functions that MUST survive** (used by non-rewrite daemon logic):
- `is_valid_oid` — used throughout side-effect processing
- `is_zero_oid` — used throughout side-effect processing
- `is_non_auxiliary_ref` — used by `resolve_heads_for_command` (surviving)
- `is_ancestor_commit` — used by surviving code paths

**Note**: `pending_ai_edits_by_family` (line 3859) is NOT deleted — still used in async checkpoint processing flow.

| Section | ~Lines | Reason |
|---|---|---|
| `apply_rewrite_side_effect` + helpers | 283 | Per-event dispatch → replaced by `handle_rewrite_event` call |
| `rewrite_events_from_semantic_events` | 1,192 | Per-operation mapping synthesis → replaced by non-FF detection |
| Pending state fields + accessors (rebase + cherry-pick old-style) | ~70 | Mid-operation tracking (new cherry-pick uses separate HashMap) |
| Pending state accessor methods (5 functions) | ~70 | set/clear/take for dead pending state |
| `strict_cherry_pick_mappings_from_command` + related | ~200 | Replaced by two-pass matching |
| `resolve_linear_head_commit_chain_for_worktree` | ~150 | Old cherry-pick chain resolution |
| All `RewriteLogEvent` construction/handling | scattered ~300 | Gone |
| `deferred_commit_carryover_context` + carryover logic | ~250 | Gone |
| `apply_stash_rewrite_side_effect` | ~100 | Replaced by new stash handler |
| `match_source_to_new_commits_by_message` | ~50 | Replaced by two-pass matching |
| Wrapper state infrastructure (WrapperStateEntry, store/apply/timeout) | ~100 | Wrapper-era: no more wrapper invocations |
| Wrapper telemetry (send_wrapper_pre/post_state) + ControlRequest handlers | ~40 | Wrapper-era |
| Rewrite-log-dependent helpers (8 functions) | ~204 | preceding_merge_squash, latest_reset, commit_has_authorship_log, rewrite_log_mentions_commit, filter_commit_replay_files, build_human_replay_checkpoint_request, inferred_top_stash_sha_from_rewrite_history, exact_final_state_for_commit_replay |
| Replay/recovery helpers (3 functions) | ~228 | recover_reset_working_log, seed_merge_squash_working_log, recover_recent_replay_prerequisites |
| Other helpers only called from deleted paths | ~600 | Dead code |

### Other dead code (outside daemon.rs)

| Location | Lines | Reason |
|---|---|---|
| `src/git/repository.rs:handle_rewrite_log_event` | 37 | Zero callers (dead NOW) |
| `src/daemon/domain.rs:wrapper_invocation_id` field | 2 | Wrapper-era (field becomes universally None) |
| `src/daemon/control_api.rs` wrapper variants | 12 | Wrapper-era (WrapperPreState, WrapperPostState) |
| `src/daemon/telemetry_handle.rs` wrapper methods | 30 | Wrapper-era (send_wrapper_pre/post_state + related) |
| `src/feature_flags.rs:rewrite_stash` | ~10 | Vestigial, never checked at runtime |
| `src/utils.rs:resolve_git_ai_exe_from_invocation_path` | ~55 | Calls deleted `is_git_hook_binary_name` |
| `src/authorship/mod.rs:range_authorship` declaration | 1 | Module deleted |
| `src/authorship/authorship_log_serialization.rs:convert_to_checkpoints_for_squash` | ~150 | `#[allow(dead_code)]`, zero external callers |
| `src/authorship/authorship_log_serialization.rs:_serialize_to_writer/_deserialize_from_reader` | ~14 | Dead, underscore prefix convention |
| `src/authorship/prompt_utils.rs:find_prompt_in_commit` | ~38 | Zero external callers |
| `src/authorship/ignore.rs:load_linguist_generated_patterns_from_root_gitattributes` | ~6 | Zero external callers (path-based version used instead) |

Note: telemetry_handle.rs entries are the function definitions; the call sites and ControlRequest handlers are in the daemon.rs table above.

### Deletion math summary

| Category | Lines |
|---|---|
| Entire files (13 files) | ~11,125 |
| Gutted from daemon.rs | ~3,850 |
| Other dead code (across 12 locations) | ~355 |
| virtual_attribution.rs function removals (10 functions) | ~721 |
| post_commit.rs function removals | ~35 |
| ci_context.rs pre-computation removal | ~70 |
| Test file deletions (install_hooks_comprehensive, rebase_authorship_unit, git_alias_resolution, wrapper mode tests) | ~1,050 |
| **Total** | **~17,200** |

The ~16,200 target in Goals accounts for the fact that ~1,000 lines of test deletions are "free" (not reflected in production code complexity).

### Simplified (NOT deleted)

| File | Change |
|---|---|
| `src/authorship/virtual_attribution.rs` | Remove: `restore_stashed_va`, `filter_to_commits`, `from_working_log_for_commit`, `from_working_log_for_commit_snapshot`, `new_with_prompts`, `to_authorship_log`, `calculate_and_update_prompt_metrics`, `to_authorship_log_index_only`, `get_char_attributions`, `get_line_attributions` (all only called from rebase_authorship.rs). KEEP: `merge_attributions_favoring_first` (called internally within VA itself), `content_has_conflict_markers`, `strip_conflict_markers_keep_ours`, `from_just_working_log`, `from_working_log_snapshot`, `from_persisted_working_log`, `to_authorship_log_and_initial_working_log`, `snapshot_contents_for_files`, `to_initial_working_log_only`. Absorb `restore_working_log_carryover` + `restore_virtual_attribution_carryover` (~70 lines) from deleted `rebase_authorship.rs`. |
| `src/authorship/post_commit.rs` | Remove: `post_commit` wrapper (only caller: rebase_authorship.rs), `estimate_stats_cost_for_head` (only caller: git_handlers.rs). Keep daemon-called `post_commit_with_final_state`. |
| `src/ci/ci_context.rs` | Migrate to call `handle_rewrite_event`. Delete ~70 lines of pre-computation (commit list building, rebase-vs-squash detection) that becomes redundant. |
| `src/daemon/trace_normalizer.rs` | Remove wrapper-state correlation (~12 lines). |
| `src/git/repo_storage.rs` | Remove rewrite_log persistence (~24 lines). Add stash metadata persistence. |
| `main.rs` | Remove argv[0] git-proxy dispatch + `GIT_AI=git` debug mode. Binary is always `git-ai`. Replace ~1,800 lines of handler dispatch with ~50-line subcommand match. See "main.rs routing" section. |
| `src/utils.rs` | Remove `resolve_git_ai_exe_from_invocation_path` (~55 lines) and `is_git_hook_binary_name` references. Simplify `current_git_ai_exe` to not handle hook binary names. |

---

## The RewriteEvent Enum

```rust
pub enum RewriteEvent {
    /// A branch ref moved non-fast-forward (rebase, amend, reset-forward, update-ref).
    /// The entrypoint derives commit mappings via range-diff internally.
    /// Squash (N→1) is detected and handled as a special case within this path.
    NonFastForward { old_tip: String, new_tip: String },

    /// A cherry-pick completed. Sources and new commits already paired by the caller.
    CherryPickComplete { sources: Vec<String>, new_commits: Vec<String> },
}
```

Note: there is no `Squash` variant. Squash is detected inside `derive_mappings_from_range_diff` and handled by returning `vec![(old_tip, new_commit)]`. See "Squash Detection" section below.

---

## Mapping Derivation: `derive_mappings_from_range_diff`

Called only for `NonFastForward` events. This is where range-diff runs.

### Pre-checks

```
1. base = git merge-base <old_tip> <new_tip>
2. If merge-base fails (no common ancestor) → skip gracefully. Return empty.
3. If base == new_tip → rewind (branch moved backward).
   → Delegate to reconstruct_working_log_after_backward_reset() if applicable.
   → Return empty mappings (old commits' notes are already correct).
4. If base == old_tip → fast-forward. Should never reach here (filtered by caller).
```

### Squash detection (internal)

Before running range-diff, check for full squash:

```rust
fn is_full_squash(repo: &Repository, base: &str, old_tip: &str, new_tip: &str) -> bool {
    git_rev_parse(new_tip^) == base        // exactly one commit between base and new_tip
    && git_rev_parse(new_tip^2).is_err()   // not a merge commit
    && git_rev_list_count(base..old_tip) > 1  // multiple old commits existed
}
```

If squash detected: return `vec![(old_tip, new_tip)]` immediately (skip range-diff). The old_tip's note represents cumulative authorship of the branch, and diff-tree will show how the final tree changed.

### range-diff invocation

```bash
git -c core.abbrev=no range-diff --no-color --no-abbrev -s --creation-factor=100 <base>..<old_tip> <base>..<new_tip>
```

- Two-range form is mandatory (three-dot syntax includes upstream noise)
- `--no-abbrev`: full 40-char SHAs for reliable parsing
- `-s`: suppress inner diffs (we only need the summary lines)
- `--creation-factor=100`: required for matching amends and conflict-resolved rebases. Default (60) fails to match commits whose context lines changed.

**Output stability caveat**: Git's docs note that range-diff output is "porcelain" and subject to change across versions. The format has been stable in practice for many years, but future git versions could theoretically change it.

### Parsing

Each output line follows:
```
<padded_pos>:  <40-char-sha> <status> <padded_pos>:  <40-char-sha> <commit_subject>
```

Regex: `^\s*(\d+|-):  ([0-9a-f]{40}|-{40}) ([=!<>]) \s*(\d+|-):  ([0-9a-f]{40}|-{40}) (.+)$`

Parsing rules:
- `=` or `!` → matched pair: extract `(old_sha, new_sha)` into mappings
- `<` → old commit dropped (no new equivalent) → skip
- `>` → new commit with no old equivalent (e.g., upstream commits) → skip

### Merge commit handling

range-diff silently excludes merge commits from its output. Additional steps:

```
1. git rev-list --merges --topo-order --reverse <base>..<old_tip> → old_merges
2. git rev-list --merges --topo-order --reverse <base>..<new_tip> → new_merges
3. For each old_merge (processed in leaves-first order via --reverse):
   a. Look up old_merge's parents in the non-merge mapping
   b. Find the new_merge whose parents match the mapped equivalents
   c. Add (old_merge, new_merge) to mappings
   d. If any parent has no mapping → skip this merge (parent was dropped)
4. Skip merges without authorship notes (common case — no AI editing during merge)
```

Note: `--reverse` is essential because git's default topo-order outputs tips first. We need leaves first so that dependent merges (whose parent IS another merge) find their parents already in the mapping. Works correctly for nested merges and octopus merges (3+ parents).

---

## Core Function: `shift_authorship_notes`

```rust
fn shift_authorship_notes(repo: &Repository, mappings: &[(String, String)]) -> Result<()>
```

For each `(source_sha, new_sha)` in mappings:

```
1. Read authorship note:
   note_content = git notes --ref=ai show <source_sha>
   If no note exists → skip this pair

2. Deserialize into AuthorshipLog (attestation entries + metadata)

3. Compute tree diff (with rename detection):
   hunks = git diff-tree -p -U0 -M <source_sha> <new_sha>
   Parse into per-file hunk list + rename mappings

4. Apply file renames:
   For each rename (old_path → new_path) in the diff output:
     Update FileAttestation.file_path from old_path to new_path

5. Shift attestation entries:
   For each file in the note:
     adjusted_entries = apply_hunk_shifts(entries_for_file, hunks_for_file)
     Remove entries with empty line_ranges after shifting
   Remove files with no remaining entries

6. Update metadata:
   Set base_commit_sha = new_sha

7. Serialize adjusted AuthorshipLog

8. Collect for batch write: (new_sha, serialized_note)
```

After processing all pairs, write all notes in a single batch. Failure handling:
- diff-tree failure for one pair: copy note verbatim to new commit (stale line numbers > lost note)
- range-diff failure: skip remapping entirely, log warning, notes orphaned
- Note write failure: log error (partial writes are acceptable — idempotent on retry since `git notes add -f` overwrites)

Old notes are NOT removed. Let `git gc` handle orphaned notes naturally.

### Authorship note data model

The actual serialized format:

```
src/file.rs
  abc123 1,2,19-222
  h_def456 400-405
src/file2.rs
  s_session1 1-111,245,260
---
{ "schema_version": "authorship/3.0.0", "base_commit_sha": "...", ... }
```

In-memory representation:

```rust
struct AttestationEntry {
    hash: String,                  // maps to metadata (prompt/human/session)
    line_ranges: Vec<LineRange>,   // Single(u32) or Range(u32, u32), inclusive
}
```

- Plain hex hash = AI prompt attribution
- `h_` prefix = known human
- `s_` prefix = session record
- Lines with no attestation entry = implicitly untracked

The shift algorithm operates on each entry's `line_ranges`, adjusting line numbers. The `hash` and metadata section (prompts, humans, sessions) are copied unchanged. Stale metadata entries (hashes with no remaining line ranges) are harmless and not pruned.

### Hunk shift algorithm

Operates per-file on a list of `AttestationEntry` items.

```
Input:
  - entries: Vec<AttestationEntry>  (each has hash + line_ranges)
  - hunks: Vec<DiffHunk>  where DiffHunk = { old_start, old_count, new_start, new_count }
    (sorted by old_start)

For each file:
  If file has no hunks → copy entries unchanged
  Else:
    For each entry, walk its line_ranges against the hunks:
      - Lines BEFORE next hunk: shift by accumulated offset
      - Lines INSIDE a hunk's old range: drop (remove from line_ranges)
      - Lines AFTER all hunks: shift by final accumulated offset

    Offset accumulator:
      For each hunk: offset += (hunk.new_count - hunk.old_count)
```

Lines that fall inside diff hunks are removed from their attestation entry (they become implicitly untracked). This correctly handles conflict resolution, evil merges, and any edits made during the rewrite — we don't try to attribute them.

### File rename handling

The diff-tree invocation uses `-M` for rename detection. When a rename is detected in the output:

```
diff --git a/old_name.rs b/new_name.rs
rename from old_name.rs
rename to new_name.rs
```

The algorithm:
1. Parse rename pairs from diff output
2. Before shifting, update the `file_path` in the corresponding `FileAttestation` from old name to new name
3. Apply hunk shifts normally (renames can include content changes too)

Without `-M`, renames would appear as delete + add, causing all attributions for renamed files to be lost.

### Reusable existing code

The following can be extracted from `rebase_authorship.rs` before deletion:
- `DiffHunk` struct (simplified, without `added_lines`)
- `parse_hunk_header` / `parse_range_spec` functions (make `pub(crate)`)
- The segment-building logic from `apply_hunks_to_line_attributions` (adapted for `AttestationEntry`)
- `AuthorshipLog::serialize_to_string()` / `deserialize_from_string()` (already in authorship_log_serialization.rs)
- `remap_note_content_for_target_commit` (for `base_commit_sha` update)

---

## Working Log Migration

```rust
fn migrate_working_log_if_needed(repo: &Repository, mappings: &[(String, String)]) -> Result<()>
```

Check if `.git/ai/working_logs/<any source_sha>/` exists for any source in mappings. If so, and if that source maps to the current HEAD (i.e., it's the branch tip), migrate it.

### Migration logic

```
For the (source, new) pair where new == current HEAD:
  old_dir = .git/ai/working_logs/<source>/
  new_dir = .git/ai/working_logs/<new>/

  If diff-tree -M source new shows no changes to files in the working log:
    → Just rename old_dir to new_dir (common case: simple amend, stash pop)

  Else:
    → Shift INITIAL file's LineAttribution entries using same hunk-shift algorithm
    → Clear `file_blobs` entries for shifted files (blob SHAs reference old content;
      graceful fallback already exists in read path for missing blobs)
    → Apply file renames to INITIAL keys if applicable
    → Character-level Attribution data in checkpoints.jsonl: leave as-is
      (next checkpoint will re-diff against current file state anyway)
    → Write adjusted INITIAL to new_dir
    → Copy checkpoints.jsonl and blobs/ as-is to new_dir

  Delete old_dir only AFTER new_dir is fully written
```

For non-tip mappings (intermediate commits during a rebase): delete their working log directories if they exist.

### Reset --soft backward (quarantined handler)

When the pre-check detects a rewind (`merge-base(old, new) == new`) AND a working log exists at `.git/ai/working_logs/<old_tip>/`, the simple hunk-shift algorithm is insufficient. A backward reset moves HEAD to an earlier commit while preserving the working tree — the working log needs to be reconstructed with correct coordinate-space attribution.

This is handled by a separate quarantined function:

```rust
fn reconstruct_working_log_after_backward_reset(
    repo: &Repository,
    old_tip: &str,
    new_tip: &str,
    final_state: Option<HashMap<String, Vec<u8>>>,  // working dir snapshot at exit time
) -> Result<()>
```

This function:
1. Reads the existing working log for `old_tip` (which already contains cumulative attribution data from the entire branch — NOT individual commit notes)
2. Filters to files that changed between `new_tip` and `old_tip` AND have AI attribution
3. Reads current working directory state (from `final_state` snapshot or live filesystem)
4. **Transforms attributions from old_head coordinate space to current file state** using diff-based line tracking (the working log's line numbers correspond to old_head's file content, but HEAD is now at new_tip — content may differ)
5. Writes the reconstructed working log keyed to `new_tip`
6. Deletes the old working log

**Critical detail**: Step 4 is necessary because after `git reset --soft HEAD~3`, the working directory is unchanged but the working log was keyed to old_head's content state. If any of the undone commits changed file content (which they almost certainly did), the attribution line numbers in the old working log don't correspond to the file as it exists in the working tree. The transformation uses the same `update_attributions` diff-tracking mechanism already used elsewhere in the codebase.

**What this is NOT**: The function does not "read authorship notes from undone commits and merge them." The working log at old_head already contains all attribution data accumulated during the session. It just needs coordinate-space transformation and re-keying.

This is the ONE case that requires more than hunk-shifting. It is deliberately kept separate from the main shift path to avoid polluting the unified algorithm.

---

## Daemon Detection Layer

The daemon monitors trace2 events and fires `handle_rewrite_event` in two cases:

### 1. Non-fast-forward ref move

```
On command completion:
  For each ref change in NormalizedCommand.ref_changes:
    1. Filter: ref must match refs/heads/*
       Exclude: HEAD, ORIG_HEAD, FETCH_HEAD, refs/remotes/*, refs/tags/*, refs/stash
    2. Collapse: if multiple changes for same branch, use (first_old, last_new)
    3. Check: is collapsed change non-fast-forward?
       (git merge-base --is-ancestor <old> <new> → exit 1 means non-FF)
    4. Fire: handle_rewrite_event(repo, NonFastForward { old_tip, new_tip })
```

This single rule catches: rebase, amend, reset --hard forward, interactive rebase, `update-ref` (including from tools like Graphite), squash merges, force-push receives.

**Guards against false positives:**
- Skip if `old_oid` is null (`0000...`) — branch was just created, not rewritten
- Skip if reflog reason contains "fetch" — force-fetch into local refs/heads/* is not a local rewrite (the branch mirrors a remote, not local work)

**Backward moves** (rewind/abort): handled by `derive_mappings_from_range_diff`'s pre-check which detects `merge-base == new_tip` and delegates to the reset reconstruction handler if needed.

### 2. Cherry-pick completion

```
On command where cmd_name == "cherry-pick":
  If ref_changes shows HEAD/branch moved forward (from reflog delta):
    Derive (source, new) pairs (see Cherry-Pick section)
    Fire: handle_rewrite_event(repo, CherryPickComplete { sources, new_commits })
```

Note: The signal is ref movement via reflog delta, not exit code. A cherry-pick that exits 0 but makes no ref changes (e.g., `--no-commit`) correctly produces no event.

### `update-ref` support

**Single update-ref** (e.g., `git update-ref refs/heads/main <sha> <old>`): Already supported — the daemon's HistoryAnalyzer parses the command args and emits a RefUpdated event, which triggers non-FF detection via the standard reflog delta path.

**Batch `update-ref --stdin`** (used by Graphite, git-town, git-stack): Currently NOT supported — the daemon's parser returns `None` for `--stdin`/`--batch-updates` and falls back to reflog delta, but only tracks the current branch's reflog. Other branches moved in the batch are missed.

**Deferred improvement**: Full batch support requires expanding `tracked_reflog_refs_for_command()` to monitor all `refs/heads/*` reflogs when the command is `update-ref`. This is an enhancement — single-ref support is sufficient for MVP. A single `update-ref --stdin` command would then produce N independent `NonFastForward` firings — one per `refs/heads/*` ref that moved non-FF.

---

## Cherry-Pick Handling

Cherry-pick is a fast-forward on the target branch, so non-FF detection won't fire. It gets its own detection path but calls the same `handle_rewrite_event` entrypoint.

### Why it uses the unified function

Cherry-picks can conflict. After conflict resolution, `diff-tree source_sha cherry_picked_sha` shows both base-difference line shifts AND conflict resolution edits in one set of hunks. Lines that were conflict-resolved land inside a hunk → correctly marked unattributed. This is identical to how the core shift function handles any other rewrite.

### Clean cherry-pick (exit 0, no prior failure)

```
Sources: parse from argv (expand ranges via git rev-list if argv contains "..")
New commits: reflog entries since pre-command HEAD (labeled "cherry-pick:" or "commit (cherry-pick):")
Pairing: two-pass matching algorithm (see below)
```

No state needed.

### Cherry-pick with conflicts (in-memory state)

```rust
struct PendingCherryPick {
    all_sources: Vec<String>,      // full source list (expanded from argv/ranges)
    pre_command_head: String,      // HEAD before the sequence started
}

// Single HashMap, ephemeral process memory
pending_cherry_picks: HashMap<PathBuf, PendingCherryPick>
```

**State machine:**

```
On cherry-pick [shas...] (initial invocation, NOT --continue/--skip/--abort/--quit):
  → Expand sources from argv (resolve ranges via git rev-list)
  → Store PendingCherryPick { all_sources, pre_command_head }
  (Regardless of exit code — stores on success too, consumed below)

On cherry-pick with ref_changes showing HEAD moved (sequence complete):
  → Retrieve stored entry
  → Get all new commits from reflog since pre_command_head
  → Run two-pass matching to pair sources with new commits
  → Fire handle_rewrite_event(CherryPickComplete { sources, new_commits })
  → Clear map entry

On cherry-pick --continue/--skip exit != 0:
  → Still conflicting. NO ACTION on pending state.
  (Critical: do NOT clear pending sources on failure — this is a bug in the current impl)

On cherry-pick --skip with ref_changes showing HEAD moved:
  → Sequence complete, same as above (fire event, clear entry)

On cherry-pick --abort:
  → Clear map entry. Abort undoes everything including previously-applied commits.

On cherry-pick --quit:
  → Clear map entry. Partial commits survive but their notes come from
    normal commit flow (post-commit hook wrote them when they were created).
```

**Single-commit `git commit` bypass**: If a user resolves a single-commit cherry-pick conflict with `git commit` instead of `--continue`, the normal commit flow writes the authorship note correctly. The `PendingCherryPick` entry leaks in memory until daemon restart — acceptable since it's a small HashMap entry and the daemon cleans up on restart.

**Lost on daemon restart:** Acceptable. A cherry-pick that conflicted before restart and completes after won't have its notes copied.

### Two-pass matching algorithm

Pairs source commits with new cherry-picked commits after a sequence completes. Handles: clean picks, conflict-resolved picks, `--skip`, edited subjects, duplicate subjects.

```
Input:
  - sources: Vec<String>      // all original source SHAs, in order
  - new_commits: Vec<String>  // all new commits since pre_command_head, in order

Pass 1 (patch-id anchoring):
  For each new commit, compute patch-id (git show <sha> | git patch-id --stable)
  For each source, compute patch-id
  Match pairs where patch-ids are identical (definitive for clean picks)
  Mark both sides as matched

Pass 2 (positional gap-fill):
  Walk remaining unmatched sources and unmatched new commits in their original order
  Since cherry-pick preserves sequence, pair positionally:
    i-th unmatched new commit corresponds to i-th unmatched source
  Sources left over after all new commits are paired = skipped (no new commit for them)

Output:
  Vec<(source_sha, new_sha)> for all successfully paired commits
  (skipped sources are simply absent from the output)
```

This is O(n) after patch-id computation and handles all cases reliably. The order-preservation invariant of cherry-pick guarantees the positional fallback is correct.

### `--no-commit` cherry-picks

Ignored. No commit created, no reflog entry. The changes are staged — attribution is handled by the normal checkpoint → commit flow.

---

## Stash Handling

### On stash create

Daemon sees stash SHA being created. Two actions:

1. Write metadata file:

```
.git/ai/stashes/<stash_sha>.json
```

```json
{
  "base_commit": "<HEAD at stash-create time>",
  "timestamp": 1715000000,
  "pathspecs": ["src/", "lib/"]  // empty array = all files
}
```

The `pathspecs` field records which files were stashed (from argv parsing of `git stash push -- <paths>`). Needed for partial stash restoration — without it, popping a partial stash would incorrectly restore attributions for all files.

2. **Clean up working log entries** for stashed files:
   - Read the current working log at `base_commit`
   - Remove INITIAL attribution entries for files matching pathspecs (or all files if no pathspecs)
   - This prevents subsequent commits from using stale attributions for files that are no longer in the working tree

### On stash pop/apply

1. Read `.git/ai/stashes/<stash_sha>.json` → get `base_commit` and `pathspecs`
2. If `base_commit != current HEAD`: call `migrate_working_log_if_needed(repo, &[(base_commit, current_HEAD)])` to shift attributions to current HEAD's coordinate space
3. Restore INITIAL attribution entries for stashed files (filtered by `pathspecs`) into the working log at current HEAD
4. On pop (not apply): delete the stash file

**Conflict handling**: Even if `git stash pop` exits with non-zero code (merge conflict), attribution restoration still proceeds. The stashed files are in the working tree regardless of conflict state.

### On stash drop

Delete `.git/ai/stashes/<stash_sha>.json` (no migration needed).

### Stash SHA resolution

The stash SHA is NOT in trace2 events directly. The daemon resolves it at **exit time** from the reflog delta for `refs/stash` — the delta captures what was at `refs/stash` before the command removed it (for pop/drop) or what was created (for push). This is the existing pattern: the daemon tracks reflog byte offsets at command start, reads the delta at exit.

### Garbage collection

On daemon startup, scan `.git/ai/stashes/` and remove entries whose SHA no longer exists in the stash reflog.

---

## CI Module Migration

The CI module (`src/ci/ci_context.rs`) is a **user-facing feature** that runs on GitHub/GitLab CI runners to handle merge operations (where no daemon is running). It currently calls `rewrite_authorship_after_rebase_v2` and `rewrite_authorship_after_squash_or_rebase` from the deleted `rebase_authorship.rs`.

### Semantic note

CI's use case is cross-branch: it maps `head_sha` (PR branch tip) → `merge_commit_sha` (post-merge on main). This is technically a different topology than same-branch non-FF moves that the daemon detects. However, `NonFastForward` still works correctly because `derive_mappings_from_range_diff` only cares about merge-base and tree diffs — not whether the commits are on the same branch.

### Migration

```rust
// Before (deleted):
rewrite_authorship_after_rebase_v2(repo, original_commits, new_commits, ...)?;

// After:
handle_rewrite_event(repo, RewriteEvent::NonFastForward {
    old_tip: original_tip.clone(),
    new_tip: new_tip.clone(),
})?;
```

The CI module already has access to old_tip and new_tip. The new function derives mappings internally via range-diff, which is more robust than the CI module's current positional matching.

**Additional deletion in ci_context.rs (~70 lines)**: The CI module currently pre-computes commit lists, detects rebase-vs-squash by comparing counts, and routes to different functions. All of this becomes redundant — `derive_mappings_from_range_diff` handles squash detection and commit mapping internally. The CI module shrinks from ~250 lines of rewrite logic to ~3 lines (a single `handle_rewrite_event` call).

### CLI Dispatch

Since `git_ai_handlers.rs` is deleted, `git-ai ci` commands need a new entry point. The minimal `main.rs` dispatch (see "main.rs routing" below) routes `git-ai ci *` directly to the CI module. The CI module itself (`src/ci/`) is unchanged except for swapping the rewrite function call.

---

## Notes Push Syncing

`push_hooks.rs` is deleted entirely. The relevant functions are redistributed:

- **`resolve_push_remote` + `resolve_push_remote_url` (~93 lines)** → moved into `src/git/sync_authorship.rs` (already handles notes sync logic)
- **Skip-check logic + orchestration (~15 lines)** → inlined directly into the daemon's `apply_push_side_effect`

The daemon continues to trigger notes sync on push events. The flow is:
1. Daemon detects push via trace2
2. `apply_push_side_effect` runs skip checks inline (dry-run, delete, mirror)
3. Calls `sync_authorship::push_notes_to_remote(repo, remote)` which uses the moved `resolve_push_remote` internally

No new files created. Net result: one file deleted, two existing files absorb ~108 lines total.

---

## Data Formats

### Authorship note format (unchanged)

```
src/file.rs
  abc123 1,2,19-222
  h_def456 400-405
src/file2.rs
  s_session1 1-111,245,260
---
{
  "schema_version": "authorship/3.0.0",
  "base_commit_sha": "...",
  "prompts": { ... },
  "humans": { ... },
  "sessions": { ... }
}
```

Read via `git notes --ref=ai show <sha>`. Write via `git notes --ref=ai add -f`. Deserialized/serialized using existing `AuthorshipLog` serialization code.

### Working log format (unchanged)

`.git/ai/working_logs/<base_commit_sha>/` contains:
- `INITIAL` — JSON with `LineAttribution` entries per file (line-level, shiftable)
- `checkpoints.jsonl` — JSONL of `Checkpoint` records (character-level, not shifted)
- `blobs/` — file content snapshots

### Stash metadata (new)

`.git/ai/stashes/<stash_sha>.json` — simple JSON with `base_commit` and `timestamp`.

---

## Edge Cases

### Rebase with conflicts resolved manually

diff-tree between old and new commit shows hunks covering the conflict region. Those attribution ranges get dropped (marked unattributed). Correct — we don't know who resolved the conflict.

### Interactive rebase with reordering

range-diff matches by patch content regardless of order. Reordered commits get matched correctly.

### Interactive rebase with squash/fixup (partial)

range-diff reports the surviving commits as matched (`=` or `!`) and squashed-away commits as dropped (`<`). Attributions from squashed-away commits that had notes: those notes are orphaned (acceptable loss).

### Full squash (N → 1)

Detected by `is_full_squash` pre-check. Uses old_tip's note as the cumulative source, diff-tree `old_tip new_commit` for shift. Works because old_tip's tree represents the final state of the old branch.

### Amend

Merge-base = parent commit. range-diff matches the single `(old, new)` pair. diff-tree shows exactly what changed in the amend. Standard flow. Message-only amends produce empty diff-tree output → note copied verbatim with updated `base_commit_sha`.

### Reset --hard backward

`merge-base(old, new) == new` → pre-check detects rewind. Delegates to `reconstruct_working_log_after_backward_reset` if working logs exist, otherwise no-op. Old commits' notes remain correct.

### Rebase --abort

Restores branch to pre-rebase state. The ref move (back to original) triggers non-FF detection. Pre-check sees `merge-base == new_tip` (the original tip is ancestor of the partial-replay tip, or they diverge). In all cases, the original notes are already correct so no remapping is needed.

### Multiple refs changing in one command

Processed per the detection rules: filter `refs/heads/*` → collapse same-branch → non-FF check → fire independently.

### Fast-forward (no-op)

Detected by `merge-base --is-ancestor` check. Not a rewrite. Skip entirely.

### Binary files in diff-tree

Produce no hunk headers (`Binary files differ`). Since "no hunks for this file" means attributions are copied unchanged, binary file attributions (if any exist) are preserved.

### File renames

Handled by `-M` flag on diff-tree. Old filename updated to new filename in attestation entries before hunk shifting. See "File rename handling" section.

---

## Explicitly Out of Scope

- **Startup reconciliation**: rewrites that complete during daemon downtime are not retroactively processed. Acceptable loss.
- **`git filter-branch` / `git filter-repo`**: complete history rewrites with no common ancestor. Skip gracefully when merge-base fails.
- **`cherry-pick --no-commit`**: handled by normal checkpoint/commit flow.
- **Partial squash attribution recovery** (N→M where M>1): unmatched old commits' notes orphaned.
- **AI attribution during conflict resolution**: those lines land in diff hunks → unattributed. By design.
- **Detached HEAD moves**: not monitored (only `refs/heads/*`).
- **Cherry-pick note migration after daemon restart**: lost if daemon was down during conflicted cherry-pick.
- **Batch `update-ref --stdin`**: Multiple branches moved in a single batch command only tracks current branch's reflog. Full batch support deferred as enhancement.
- **`cherry-pick --quit` note recovery**: Partial commits get notes via normal commit flow; no retroactive rewrite mapping attempted.

---

## Extract Before Delete

### From `git_ai_handlers.rs` → per-command modules

Most subcommands already have their own module file. These do NOT and their logic (~560 lines) must be moved into proper modules before `git_ai_handlers.rs` is deleted:

| Subcommand | Lines | Destination | Effort |
|---|---|---|---|
| `checkpoint` | 190 | `src/commands/checkpoint_agent/` (subdir exists, add entry point) | HIGH |
| `stats` | 126 | New `src/commands/stats.rs` | HIGH |
| `notes` | 88 | New `src/commands/notes.rs` (notes_migrate.rs already exists for sub-subcommand) | MODERATE |
| `blame` (handler) | 86 | Existing `src/commands/blame.rs` (merge with existing module) | HIGH |
| `effective-ignore-patterns` | 18 | New `src/commands/effective_ignore.rs` | MINIMAL |
| `blame-analysis` | 22 | New `src/commands/blame_analysis.rs` | MINIMAL |
| `fetch-authorship-notes` | 15 | New `src/commands/fetch_authorship_notes.rs` | MINIMAL |
| `push-authorship-notes` | 13 | New `src/commands/push_authorship_notes.rs` | MINIMAL |
| `git-path` | 3 | Inline in main.rs match arm | TRIVIAL |

This is NOT new code — it's relocation of existing logic. The ~560 lines move from the monolithic handler into dedicated modules with minimal changes (add argument parsing that was previously handled by the dispatcher).

### From `install_hooks.rs` → new `src/commands/install.rs`

Extract before deleting `install_hooks.rs`:
- `configure_daemon_trace2()` (~30 lines) — configures `trace2.eventTarget` pointing to daemon socket
- `ensure_daemon()` (~25 lines) — restarts daemon after config changes

These are critical for `git-ai install` to function. The remaining ~1,275 lines of hook symlink management are deleted.

### From `rebase_authorship.rs` → surviving modules

Functions that must be moved OUT of `rebase_authorship.rs` before it is deleted, because they are still called by the daemon's checkout/switch handler and have zero dependency on deleted code:

| Function | ~Lines | Move to | Reason |
|---|---|---|---|
| `restore_working_log_carryover` | ~18 | `src/authorship/virtual_attribution.rs` | Copies working log dir from old HEAD to new HEAD on branch switch |
| `restore_virtual_attribution_carryover` | ~50 | `src/authorship/virtual_attribution.rs` | Merges virtual attribution entries from old HEAD into new HEAD context |

These functions are pure filesystem/data-structure operations (copy directory, merge HashMap). They do NOT use `rewrite_log`, `rebase_authorship` internals, or any other deleted code.

Additionally, extract from `rebase_authorship.rs` into `src/authorship/hunk_shift.rs` (new file, ~80 lines):
- `DiffHunk` struct (simplified — drop `added_lines` field)
- `parse_hunk_header` function
- `parse_range_spec` function
- Core segment-building logic from `apply_hunks_to_line_attributions` (adapted for `AttestationEntry`)

---

## main.rs Routing

After removing the `argv[0] == "git"` proxy dispatch, `main.rs` becomes a simple subcommand router:

```rust
fn main() {
    // No more argv[0] sniffing. Binary is always invoked as `git-ai`.
    let args: Vec<String> = std::env::args().collect();
    let subcommand = args.get(1).map(|s| s.as_str());

    match subcommand {
        // Core daemon & checkpoint
        Some("daemon") | Some("bg") | Some("d") => commands::daemon::run(&args[2..]),
        Some("checkpoint") => commands::checkpoint::run(&args[2..]),

        // User-facing display/query
        Some("blame") => commands::blame::run(&args[2..]),
        Some("diff") => commands::diff::run(&args[2..]),
        Some("status") => commands::status::run(&args[2..]),
        Some("log") => commands::log::run(&args[2..]),
        Some("show") => commands::show::run(&args[2..]),
        Some("stats") => commands::stats::run(&args[2..]),
        Some("show-prompt") => commands::show_prompt::run(&args[2..]),

        // Auth & config
        Some("login") => commands::login::run(&args[2..]),
        Some("logout") => commands::logout::run(&args[2..]),
        Some("whoami") => commands::whoami::run(&args[2..]),
        Some("config") => commands::config::run(&args[2..]),

        // Setup & maintenance
        Some("install-hooks") | Some("install") => commands::install::run(&args[2..]),
        Some("uninstall-hooks") => commands::uninstall::run(&args[2..]),
        Some("upgrade") => commands::upgrade::run(&args[2..]),
        Some("fetch-notes") => commands::fetch_notes::run(&args[2..]),
        Some("notes") => commands::notes::run(&args[2..]),

        // CI (runs on CI runners without daemon)
        Some("ci") => ci::run(&args[2..]),

        // Internal machine commands (JSON protocol, not user-facing)
        Some("effective-ignore-patterns") => commands::effective_ignore::run(&args[2..]),
        Some("blame-analysis") => commands::blame_analysis::run(&args[2..]),
        Some("fetch-authorship-notes") | Some("fetch_authorship_notes") => commands::fetch_authorship_notes::run(&args[2..]),
        Some("push-authorship-notes") | Some("push_authorship_notes") => commands::push_authorship_notes::run(&args[2..]),
        Some("exchange-nonce") => commands::exchange_nonce::run(&args[2..]),

        // Dashboard & utility
        Some("dash") | Some("dashboard") => commands::dashboard::run(&args[2..]),
        Some("debug") => commands::debug::run(&args[2..]),
        Some("git-path") => commands::git_path::run(&args[2..]),
        Some("flush-metrics-db") => commands::flush_metrics::run(&args[2..]),

        // Meta
        Some("version") | Some("--version") | Some("-v") => print_version(),
        Some("help") | Some("--help") | Some("-h") => print_help(),
        _ => print_usage_and_exit(),
    }
}
```

This replaces the current ~1,800 lines across `git_handlers.rs` + `git_ai_handlers.rs` + `git_hook_handlers.rs` with ~50 lines of direct dispatch. Each subcommand module owns its own argument parsing. The logic that currently lives in `git_ai_handlers.rs` (arg parsing, help text, pre-flight checks per command) is pushed down into each subcommand's own module.

**Dead commands removed entirely:**
- `git-hooks` (sunset/deprecated — removal codepath can be inlined into `uninstall-hooks` if needed)
- `squash-authorship` (only caller was git_ai_handlers dispatch)

The `GIT_AI=git` debug-only proxy mode and `argv[0]` sniffing are removed entirely (tests that relied on this must be updated — see Cascading Cleanup).

---

## Cascading Cleanup

Deleting the files in the deletion table causes compile errors in dependent code. These must be addressed as part of the deletion pass:

### Module declarations (`src/commands/mod.rs`)

Remove these `pub mod` declarations:
- `git_handlers`
- `git_ai_handlers`
- `git_hook_handlers`
- `hooks` (entire submodule)
- `install_hooks`
- `squash_authorship`
- `ci_handlers`

### Module declarations (`src/authorship/mod.rs`)

Remove:
- `range_authorship` (entire module dead — only caller was git_ai_handlers)

### Broken imports

Files that import from deleted modules and need updating:
- `src/utils.rs` — remove `resolve_git_ai_exe_from_invocation_path` (~55 lines), remove `is_git_hook_binary_name` references
- `src/main.rs` — remove `is_git_hook_binary_name` check, `GIT_AI=git` routing, argv[0] sniffing
- `src/git/repository.rs` — remove `handle_rewrite_log_event` (already dead code)
- `src/git/repo_storage.rs` — remove rewrite_log persistence, add stash metadata persistence

### Test files (~113 tests affected, ~5.4% of suite)

The majority of the test suite (~1,990 of 2,103 tests) uses `TestRepo.git_ai()` which invokes the binary directly — these are **unaffected**.

**Tests to DELETE** (testing deleted features):
- `tests/integration/install_hooks_comprehensive.rs` — 48 tests, tests deleted `install_hooks` module internals
- `tests/integration/rebase_authorship_unit.rs` — 27 tests, tests deleted `rebase_authorship` functions directly
- `tests/integration/git_alias_resolution.rs` — 14 tests, tests deleted `git_handlers::resolve_alias_invocation`
- Wrapper-daemon test variants across `async_mode.rs`, `daemon_mode.rs`, `notes_sync_regression.rs`

**Tests to REWRITE** (test surviving features via deleted paths):
- `tests/integration/ci_squash_rebase.rs` — 14 tests, calls `rewrite_authorship_after_squash_or_rebase()` directly. Must be updated to call `handle_rewrite_event` or test via the CI module's public API.

**Snapshot orphans**: Delete snapshots in `tests/snapshots/` that correspond to deleted test paths.

### Constants and fields

- Move `ENV_SKIP_ALL_HOOKS` constant from `git_hook_handlers.rs` → `utils.rs` (still referenced by utils.rs for env filtering)
- Delete `ENV_SKIP_MANAGED_HOOKS` — only used within `git_handlers.rs` (being deleted)
- Delete `is_git_hook_binary_name()` — callers in main.rs and utils.rs are being rewritten/removed
- Delete `wrapper_invocation_id: Option<String>` field from `NormalizedCommand` in `domain.rs` — universally None after wrapper removal. Remove all `wrapper_invocation_id: None` assignments across daemon code.

### Feature flags

Remove `rewrite_stash` from `src/feature_flags.rs` — the feature flag is vestigial and never checked at runtime.

---

## Implementation Order

1. **Extract reusable code** — Before deleting anything:
   - Extract `DiffHunk`, `parse_hunk_header`, `parse_range_spec` from `rebase_authorship.rs` into `src/authorship/hunk_shift.rs`
   - Move `restore_working_log_carryover` + `restore_virtual_attribution_carryover` into `virtual_attribution.rs`
   - Move `resolve_push_remote` + `resolve_push_remote_url` from `push_hooks.rs` into `sync_authorship.rs`
   - Extract `configure_daemon_trace2` + `ensure_daemon` from `install_hooks.rs` into new `src/commands/install.rs`
   - Move ~560 lines of subcommand logic from `git_ai_handlers.rs` into per-command modules (checkpoint, stats, notes, blame handler, internal machine commands)

2. **Write `shift_authorship_notes`** — the core per-pair algorithm (diff-tree parse, rename handling, hunk shift, note read/write). Unit-testable with TestRepo.

3. **Write `derive_mappings_from_range_diff`** — range-diff invocation, parsing, squash detection, merge-commit mapping. Testable in isolation.

4. **Write `handle_rewrite_event`** — the single entrypoint. Thin function that dispatches.

5. **Write cherry-pick two-pass matching** — patch-id + positional algorithm.

6. **Write `reconstruct_working_log_after_backward_reset`** — quarantined reset handler (~100-120 lines, simplified from current 206-line version by dropping rewrite_log integration and legacy fallbacks).

7. **Wire non-FF detection into daemon** — detect ref changes from reflog delta, collapse, ancestor check, fire `handle_rewrite_event(NonFastForward {...})`.

8. **Wire cherry-pick detection** — cmd_name check, argv parsing, in-memory state for conflicts, fire `handle_rewrite_event(CherryPickComplete {...})`.

9. **Wire stash record/migrate** — small, self-contained.

10. **Migrate CI module** — update `ci_context.rs` to call `handle_rewrite_event`.

11. **Rewrite main.rs routing** — replace argv[0] dispatch + git_ai_handlers with minimal subcommand router (~20 lines).

12. **Delete dead code** — remove all files/functions listed in the deletion table. Address cascading cleanup (module declarations, broken imports, feature flags). Inline push skip-checks into daemon.

13. **Update tests** — delete/convert wrapper-proxy tests, delete orphaned snapshots, update remaining integration tests to exercise the new unified function. Existing rebase/cherry-pick/amend tests should pass with minimal changes (they use `TestRepo` which invokes `git-ai` directly, not the proxy).

---

## Success Criteria

- `git rebase`, `git commit --amend`, `git cherry-pick`, `git merge --squash` all result in authorship notes on the new commits with correctly shifted line attributions
- Cherry-picks (clean and conflicted) copy and shift source attribution to new commits
- File renames during rewrites preserve attribution under new filename
- Stash pop onto a different HEAD preserves working log data
- Reset --soft backward reconstructs working log correctly
- Lines modified during any rewrite are marked unattributed (not incorrectly attributed)
- Merge commits in `--rebase-merges` are mapped via parent matching and shifted
- The codebase is ~15,200-15,400 lines smaller net (~16,200 deleted, ~800-1,000 new)
- No persistent rewrite_log file
- No per-operation dispatch logic outside the `RewriteEvent` enum (stash is a separate lightweight handler, not a commit-rewrite)
- One core shift function used by all commit-rewrite paths
- All plumbing commands (update-ref, including --stdin) flow through the same detection
- CI module continues to work via the new entrypoint
- Authorship notes continue to be pushed to remotes on `git push`
