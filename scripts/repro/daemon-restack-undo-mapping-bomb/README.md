# Repro: daemon memory blowup on restack undo (range-diff mapping bomb)

Minimal, fully synthetic reproduction of a production incident where the git-ai
daemon reached ~30 GB RSS (and made every git command crawl via machine-wide
memory pressure) while processing the rewrite-attribution side effect of a
**restack undo** — moving a branch from its rebased tip back to the pre-rebase
tip, which is exactly what Graphite runs (`git reset --keep <old-tip>`) when a
restack is undone or aborted.

This fires **despite** the streaming diff-tree fix ("Fix high memory usage on
large rebases", `b0efde3fb`, shipped in v1.6.15). That fix stopped buffering
the raw patch text; this bug is the *parsed* structures, multiplied by an
unbounded mapping count.

Everything here is isolated: the script creates a brand-new temp repo, a fake
`$HOME`, and a **per-run daemon** wired via trace2 sockets (the same wiring the
integration-test harness uses). Nothing is installed system-wide and nothing
touches your real `~/.git-ai`.

## Root cause

A non-hard `git reset` to a non-ancestor (the daemon's Reset handling in
`src/daemon.rs`, non-fast-forward arm) raises
`RewriteEvent::NonFastForward { old_tip: rebased_tip, new_tip: orig_tip }`,
which flows into `handle_non_fast_forward_rewrite_with_operation`
(`src/authorship/rewrite.rs:445`):

1. `derive_mappings_from_range_diff` (`rewrite.rs:901`) runs
   `git range-diff <merge-base>..<rebased-tip> <merge-base>..<orig-tip>`
   (`run_range_diff`, `rewrite.rs:987`). For a restack undo the merge base is
   the branch's **fork point**, so the left range contains **every trunk
   commit landed since the branch forked** — thousands on a busy monorepo.
   None of them match the right range, so each is emitted as an unmatched `<`
   line, and `parse_range_diff_output` (`rewrite.rs:1008`) maps **every one**
   of them to a commit of the new range (`previous_new_sha` / the
   `pending_dropped` drain, `rewrite.rs:1031-1055`). Mapping count is
   unbounded: ~= trunk commits since fork.
2. `shift_authorship_notes_with_existing_mode` (`rewrite.rs:720`) batch-reads
   the notes for all mapping endpoints (`read_notes_batch`, `rewrite.rs:738`)
   and queues a **full-root-tree diff pair per mapping whose source commit has
   a note** (`rewrite.rs:752-787`). On the Robinhood fleet effectively every
   trunk commit has an authorship note, so ~every bogus mapping becomes a diff
   pair — plus its parsed `AuthorshipLog` held in `pending`.
3. `compute_diff_trees_batch` (`rewrite.rs:1231`) streams one
   `git diff-tree --stdin -p -U0 -M -r` over all pairs. The raw text is no
   longer buffered, **but** each pair still materializes a `DiffTreeResult`
   (`rewrite.rs:34`): `added_lines_by_file` holds **one `u32` per `+` line**
   and `hunks_by_file` one 16-byte `DiffHunk` per hunk
   (`DiffTreeChunkParser`, `rewrite.rs:1321`), all collected into
   `Vec<DiffTreeResult>` — alive until every pair is parsed and shifted.

The blowup: each pair diffs a **trunk commit's tree against the original
branch tip's tree**, i.e. it *reverses the entire trunk delta*. Every line the
trunk modified since the fork point comes back as a `+` line (its pre-fork
content), so:

```
daemon RSS  ≈  (trunk commits since fork)  ×  (lines modified on trunk)  ×  ~12 B
```

Both factors are unbounded and multiplicative. A monorepo branch forked a few
weeks back easily has thousands of trunk commits × millions of modified lines
— tens of GB. On top of the memory, `-U0 -M` diff generation across thousands
of full-tree pairs is what burns the CPU/wall-clock (the raw patch text
streamed through the parser is `pairs × trunk-delta bytes`, even though it is
no longer held).

Note `git reset --hard` does **not** reproduce this: the daemon's
`ResetKind::Hard` arm only deletes the working log. The rewrite path fires for
the non-hard kinds — and `--keep` is what Graphite uses.

## Running it

```bash
task build   # produces target/debug/git-ai (or pass GIT_AI_BIN=...)
./scripts/repro/daemon-restack-undo-mapping-bomb/repro.sh              # default scale
SCALE=small ./scripts/repro/daemon-restack-undo-mapping-bomb/repro.sh  # quick sanity
SCALE=large ./scripts/repro/daemon-restack-undo-mapping-bomb/repro.sh  # multi-GB peak
```

What the script does:

1. `git init` a temp repo; seed commit with `BASE_FILES × LINES_PER_FILE`
   lines of pre-fork content.
2. Create a `feature` branch with `STACK_COMMITS` commits, each preceded by
   `git-ai checkpoint mock_ai <file>` so the daemon writes real authorship
   notes. Waits until all notes exist.
3. Advance `main` by `TRUNK_COMMITS` commits; the first one rewrites **every
   line** of the pre-fork files (the trunk delta), the rest are cheap. Attach
   an authorship note to **every** trunk commit (reusing a real note blob) —
   modeling the org-wide notes fleet, where virtually every commit has one.
4. `git rebase main` on the feature branch (traced → daemon) — the *cheap*
   direction; waits for note migration to finish.
5. **The trigger:** `git reset --keep <orig-tip>` (traced → daemon), i.e. the
   restack undo. Samples daemon RSS (and child git processes) every 200 ms.
6. Waits for the batched notes write that ends the shift, then prints peak
   RSS, the mapping count straight from the daemon's debug log
   (`shift_authorship_notes: N mappings`), the unmatched-commit count from the
   same range-diff the daemon ran, and one pair's `+`-line/byte counts as
   ground truth.

### Knobs

| Env var | Meaning |
|---|---|
| `STACK_COMMITS` | feature-branch commits with real AI notes (default 4) |
| `TRUNK_COMMITS` | trunk commits after the fork; each gets a note and becomes one bogus mapping = one full-root-tree diff pair |
| `BASE_FILES` | pre-fork files the first trunk commit rewrites |
| `LINES_PER_FILE` | lines per pre-fork file; per-pair `+` lines = `BASE_FILES × LINES_PER_FILE` |
| `PROCESS_TIMEOUT_SECS` | max wait for daemon processing (default 1800) |
| `KEEP=1` | keep the temp workdir (repo, daemon stderr log, raw RSS samples) |
| `GIT_AI_BIN` | git-ai binary to use (default `target/debug/git-ai`) |

Expected: mappings ≈ `TRUNK_COMMITS`; parsed-structure memory ≈
`TRUNK_COMMITS × BASE_FILES × LINES_PER_FILE × ~12 B`; bytes streamed through
the parser ≈ `TRUNK_COMMITS ×` (one pair's raw diff bytes).

## Measured results

Measured on an M-series MacBook Pro (48 GB), macOS, debug build of git-ai
(post-streaming-fix, branch `andrew/best-effort-source-note-fetch`). "Peak" is
daemon RSS sampled at 200 ms during rewrite processing; baseline is daemon RSS
immediately before the reset. Mapping counts are read from the daemon's own
debug log (`shift_authorship_notes: N mappings`).

| | `SCALE=small` | `SCALE=default` |
|---|---|---|
| trunk commits since fork | 150 | 600 |
| trunk delta (`+` lines per pair) | 10,160 | 80,160 |
| **mappings derived by the daemon** | **154** | **604** |
| raw diff streamed through parser | 263 MB | 8,385 MB |
| daemon RSS baseline | 26 MB | 28 MB |
| daemon RSS peak | 36 MB | 234 MB |
| daemon RSS delta | **+9 MB** | **+206 MB** |
| rewrite processing wall time | 7 s | 33 s |
| RSS delta per `+` line per pair | ~6 B | ~4.3 B |

Two things worth noting:

- The stack is 4 commits in both runs. The 150/600 extra mappings are pure
  fabrication by `parse_range_diff_output` — the daemon literally logs
  `shift_authorship_notes: 604 mappings` for a 4-commit branch move.
- The RSS delta tracks `mappings × per-pair '+' lines × ~4–6 B` (the
  `Vec<u32>` in `added_lines_by_file`, plus map/allocator overhead), while the
  *streamed* raw text (8.4 GB at default scale) no longer shows up in RSS —
  i.e. the streaming fix works, and the residual growth is exactly the parsed
  accumulation.

Extrapolation to the production incident (v1.6.15, rh monorepo): a stack
forked weeks earlier easily faces ~3,000 trunk commits × ~2 M lines modified
on trunk; at ~4–6 B per line per pair that is **24–36 GB of daemon RSS** —
matching the observed ~30 GB — with the `-U0 -M` diff generation over
thousands of full-tree pairs supplying the multi-minute-to-hours runtime and
the machine-wide memory pressure that makes ordinary git commands take 30+
seconds.

## Fix directions

The two factors need independent bounds:

- **Clamp mapping derivation**: unmatched `<` commits mapped to
  `previous_new_sha` should be bounded (or dropped entirely when the
  unmatched count dwarfs the new range — a restack undo is not a squash of
  3000 trunk commits into a 4-commit stack).
- **Bound the per-batch parsed accumulation**: process diff pairs in bounded
  chunks and drop each `DiffTreeResult` after its shift is applied, instead
  of materializing all pairs before applying any.
