#!/usr/bin/env bash
#
# Regression-checks the git-ai daemon behavior on a "restack undo" — moving a
# branch ref from a rebased tip back to its pre-rebase tip via
# `git reset --keep <old-tip>` (exactly what Graphite runs when undoing /
# aborting a restack). NOTE: `reset --hard` does NOT reproduce this — the
# daemon's ResetKind::Hard arm only deletes the working log; the non-fast-
# forward rewrite path fires for the non-hard reset kinds (daemon.rs Reset
# handling).
#
# Older builds created one mapping for every unmatched trunk commit. Current
# builds must bound and discard that divergent group before note reads or
# diff-tree work:
#
#   For a restack undo, the old range (merge-base..rebased-tip) contains every
#   trunk commit since the branch forked — thousands on a busy monorepo. If
#   those leading `<` entries are mapped, each noted trunk commit becomes a
#   (trunk_commit, stack_commit) full-root-tree diff pair. Each pair reverses
#   the whole trunk delta:
#
#     daemon RSS  ~=  (trunk commits since fork) x (lines modified on trunk) x ~12B
#
# This script builds a fully synthetic repo, drives a real rebase + reset
# through an ISOLATED per-run daemon (same wiring as the integration-test
# harness; nothing installed system-wide), samples daemon RSS during rewrite
# processing, and verifies the mapping count stays close to STACK_COMMITS
# rather than TRUNK_COMMITS.
#
# Usage:
#   ./repro_1985.sh                 # default regression scale (a few minutes)
#   SCALE=small ./repro_1985.sh     # quick sanity run
#   SCALE=large ./repro_1985.sh     # high-load regression run
#   TRUNK_COMMITS=600 BASE_FILES=80 LINES_PER_FILE=1000 ./repro_1985.sh   # custom
#
# Knobs (env vars):
#   STACK_COMMITS    feature-branch commits with parseable AI authorship notes
#   TRUNK_COMMITS    commits on main after the fork; EVERY one gets an
#                    authorship note (models an org-wide notes fleet) and each
#                    would become one bogus full-root-tree mapping without the
#                    leading-prefix guard
#   BASE_FILES       pre-fork files that the first trunk commit rewrites
#   LINES_PER_FILE   lines per pre-fork file; every line is modified on trunk,
#                    so per-pair '+' lines = BASE_FILES * LINES_PER_FILE
#   PROCESS_TIMEOUT_SECS  how long to wait for the daemon to finish (default 1800)
#   KEEP=1           keep the temp workdir + daemon logs
#   GIT_AI_BIN       path to a git-ai binary (default: <repo>/target/debug/git-ai)
#
# Expected mappings            ~= STACK_COMMITS (trunk prefix discarded)
# Expected per-pair '+' lines  ~= BASE_FILES * LINES_PER_FILE
# Expected daemon RSS delta    ~= mappings * plus_lines * ~12B
set -euo pipefail

# ---------------------------------------------------------------------------
# Knobs
# ---------------------------------------------------------------------------
SCALE="${SCALE:-default}"
case "$SCALE" in
  small)
    STACK_COMMITS="${STACK_COMMITS:-4}"
    TRUNK_COMMITS="${TRUNK_COMMITS:-150}"
    BASE_FILES="${BASE_FILES:-25}"
    LINES_PER_FILE="${LINES_PER_FILE:-400}"
    ;;
  default)
    STACK_COMMITS="${STACK_COMMITS:-4}"
    TRUNK_COMMITS="${TRUNK_COMMITS:-600}"
    BASE_FILES="${BASE_FILES:-80}"
    LINES_PER_FILE="${LINES_PER_FILE:-1000}"
    ;;
  large)
    STACK_COMMITS="${STACK_COMMITS:-4}"
    TRUNK_COMMITS="${TRUNK_COMMITS:-1200}"
    BASE_FILES="${BASE_FILES:-100}"
    LINES_PER_FILE="${LINES_PER_FILE:-1500}"
    ;;
  *) echo "Unknown SCALE=$SCALE (small|default|large)"; exit 1 ;;
esac
PROCESS_TIMEOUT_SECS="${PROCESS_TIMEOUT_SECS:-1800}"
KEEP="${KEEP:-0}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GIT_AI_BIN="${GIT_AI_BIN:-$SCRIPT_DIR/target/debug/git-ai}"

if [[ ! -x "$GIT_AI_BIN" ]]; then
  echo "error: git-ai binary not found at $GIT_AI_BIN — run 'task build' first (or set GIT_AI_BIN)" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Sanitize PATH: drop any dir whose `git` is actually a git-ai wrapper, so the
# repro never talks to a system-installed git-ai (mirrors the test harness).
# ---------------------------------------------------------------------------
sanitize_path() {
  local out="" dir gitp
  local IFS=':'
  for dir in $PATH; do
    gitp="$dir/git"
    if [[ -f "$gitp" || -L "$gitp" ]]; then
      if [[ -L "$gitp" ]] && readlink "$gitp" | grep -q "git-ai"; then continue; fi
      if grep -q "git-ai" "$gitp" 2>/dev/null; then continue; fi
      local canon
      canon="$(cd "$(dirname "$gitp")" 2>/dev/null && pwd -P)/$(basename "$gitp")" || canon="$gitp"
      if [[ "$canon" == *git-ai* ]]; then continue; fi
    fi
    out="${out:+$out:}$dir"
  done
  printf '%s' "$out"
}
SAFE_PATH="$(sanitize_path)"

# ---------------------------------------------------------------------------
# Isolated workspace: repo + fake HOME + per-run daemon sockets
# ---------------------------------------------------------------------------
WORK="$(mktemp -d "${TMPDIR:-/tmp}/git-ai-bomb.XXXXXX")"
HOME_DIR="$WORK/home"
REPO="$WORK/repo"
SOCK_DIR="$WORK/s"
CONTROL_SOCK="$SOCK_DIR/c.sock"
TRACE_SOCK="$SOCK_DIR/t.sock"
TEST_DB="$WORK/git-ai-test-db"
RSS_LOG="$WORK/rss.log"
DAEMON_LOG="$WORK/daemon.stderr.log"
mkdir -p "$HOME_DIR/.git-ai" "$REPO" "$SOCK_DIR"

if (( ${#TRACE_SOCK} >= 100 )); then
  echo "error: socket path too long for AF_UNIX: $TRACE_SOCK" >&2
  exit 1
fi

cat > "$HOME_DIR/.gitconfig" <<'EOF'
[user]
	name = Repro User
	email = repro@example.com
[init]
	defaultBranch = main
[gc]
	auto = 0
[advice]
	detachedHead = false
EOF
# Keep the isolated git-ai fully offline / defaults (local git_notes backend).
cat > "$HOME_DIR/.git-ai/config.json" <<'EOF'
{
  "telemetry_oss": "off",
  "disable_version_checks": true,
  "disable_auto_updates": true
}
EOF

COMMON_ENV=(
  "PATH=$SAFE_PATH"
  "HOME=$HOME_DIR"
  "GIT_CONFIG_GLOBAL=$HOME_DIR/.gitconfig"
  "GIT_CONFIG_NOSYSTEM=1"
  "XDG_CONFIG_HOME=$HOME_DIR/.config"
)
DAEMON_ENV=(
  "GIT_AI_DAEMON_HOME=$HOME_DIR"
  "GIT_AI_DAEMON_CONTROL_SOCKET=$CONTROL_SOCK"
  "GIT_AI_DAEMON_TRACE_SOCKET=$TRACE_SOCK"
  "GIT_AI_TEST_DB_PATH=$TEST_DB"
  "GITAI_TEST_DB_PATH=$TEST_DB"
)

# Real git wired to the per-run daemon via trace2 (exactly how production learns
# about git commands — git-ai does NOT wrap git).
tgit() {
  env "${COMMON_ENV[@]}" \
    "GIT_TRACE2_EVENT=af_unix:stream:$TRACE_SOCK" \
    "GIT_TRACE2_EVENT_NESTING=10" \
    git -C "$REPO" "$@"
}
# git without trace2 (setup plumbing we don't want the daemon to see/process).
qgit() {
  env "${COMMON_ENV[@]}" git -C "$REPO" "$@"
}
gai() {
  (cd "$REPO" && env "${COMMON_ENV[@]}" "${DAEMON_ENV[@]}" "$GIT_AI_BIN" "$@")
}

DAEMON_PID=""
SAMPLER_PID=""
cleanup() {
  [[ -n "$SAMPLER_PID" ]] && kill "$SAMPLER_PID" 2>/dev/null || true
  if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    kill "$DAEMON_PID" 2>/dev/null || true
    for _ in $(seq 1 20); do kill -0 "$DAEMON_PID" 2>/dev/null || break; sleep 0.1; done
    kill -9 "$DAEMON_PID" 2>/dev/null || true
  fi
  if [[ "$KEEP" == "1" ]]; then
    echo "[repro] KEEP=1 — workdir preserved at: $WORK"
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

log() { printf '[repro] %s\n' "$*"; }

PLUS_LINES_PER_PAIR=$(( BASE_FILES * LINES_PER_FILE ))
log "workdir: $WORK"
log "scale: SCALE=$SCALE STACK_COMMITS=$STACK_COMMITS TRUNK_COMMITS=$TRUNK_COMMITS BASE_FILES=$BASE_FILES LINES_PER_FILE=$LINES_PER_FILE"
log "guardrail: discard ~$TRUNK_COMMITS leading commits, avoiding ~$(( TRUNK_COMMITS * PLUS_LINES_PER_PAIR * 12 / 1048576 )) MB of parsed '+'-line structures"

# ---------------------------------------------------------------------------
# Start the isolated daemon (GIT_AI_DEBUG=1 so the daemon log records
# "shift_authorship_notes: N mappings" — direct proof of the mapping bomb).
# ---------------------------------------------------------------------------
env "${COMMON_ENV[@]}" "${DAEMON_ENV[@]}" "GIT_AI_DEBUG=1" "$GIT_AI_BIN" bg run >/dev/null 2>>"$DAEMON_LOG" &
DAEMON_PID=$!
log "daemon pid: $DAEMON_PID"
for i in $(seq 1 100); do
  [[ -S "$TRACE_SOCK" && -S "$CONTROL_SOCK" ]] && break
  kill -0 "$DAEMON_PID" 2>/dev/null || { echo "daemon died at startup; log tail:"; tail -20 "$DAEMON_LOG"; exit 1; }
  sleep 0.1
done
[[ -S "$TRACE_SOCK" ]] || { echo "daemon sockets never appeared"; exit 1; }
log "daemon sockets ready"

# ---------------------------------------------------------------------------
# Build the synthetic repo
# ---------------------------------------------------------------------------
gen_file() { # path lines salt
  awk -v n="$2" -v salt="$3" 'BEGIN {
    for (i = 1; i <= n; i++)
      printf "line %06d of %s :: 0123456789abcdefghijklmnopqrstuvwxyz-payload-%06d\n", i, salt, i * 7
  }' > "$1"
}

qgit init -q -b main .

# Seed commit: the pre-fork content the trunk will later rewrite. The restack
# undo diffs (trunk commit -> original stack commit), which RESTORES this
# content — that reversal is the '+' payload parsed into memory per pair.
mkdir -p "$REPO/src"
for i in $(seq 1 "$BASE_FILES"); do
  gen_file "$REPO/src/module_$(printf '%03d' "$i").txt" "$LINES_PER_FILE" "module_$i"
done
echo "seed" > "$REPO/README.md"
qgit add -A
qgit commit -qm "seed"
FORK_POINT="$(qgit rev-parse HEAD)"
log "seed commit done ($BASE_FILES files x $LINES_PER_FILE lines)"

notes_hit_count() { # file-with-shas
  local list
  list="$(qgit notes --ref=ai list 2>/dev/null | awk '{print $2}')" || true
  [[ -z "$list" ]] && { echo 0; return; }
  grep -cFf <(printf '%s\n' "$list") "$1" || true
}

# Manufacture one real AI authorship note through the daemon. The synthetic
# feature and trunk commits below reuse its blob; their construction is pure
# setup and must stay constant-spawn even when the scale knobs increase.
FEATURE_FILES_DIR="$WORK/feature-files"
TRUNK_FILES_DIR="$WORK/trunk-files"
mkdir -p "$FEATURE_FILES_DIR" "$TRUNK_FILES_DIR"
for i in $(seq 1 "$STACK_COMMITS"); do
  f="feature_$(printf '%03d' "$i").txt"
  gen_file "$FEATURE_FILES_DIR/$f" 40 "feature_$i"
done

qgit checkout -qb note-donor
cp "$FEATURE_FILES_DIR/feature_001.txt" "$REPO/feature_001.txt"
gai checkpoint mock_ai feature_001.txt >/dev/null
tgit add feature_001.txt
tgit commit -qm "AI note donor"
NOTE_DONOR_COMMIT="$(qgit rev-parse HEAD)"
NOTE_DONOR_FILE="$WORK/note_donor.txt"
printf '%s\n' "$NOTE_DONOR_COMMIT" > "$NOTE_DONOR_FILE"

deadline=$(( $(date +%s) + 300 ))
while :; do
  n="$(notes_hit_count "$NOTE_DONOR_FILE")"
  (( n >= 1 )) && break
  if (( $(date +%s) > deadline )); then
    echo "error: note donor did not get an authorship note in 300s" >&2
    echo "daemon log tail:"; tail -30 "$DAEMON_LOG"
    exit 1
  fi
  sleep 1
done

# Prepare the first trunk commit's large tree delta outside the repository.
for i in $(seq 1 "$BASE_FILES"); do
  f="module_$(printf '%03d' "$i").txt"
  awk '{ print $0 " [rewritten-on-trunk]" }' \
    "$REPO/src/$f" > "$TRUNK_FILES_DIR/$f"
done

emit_fast_import_data() {
  local value="$1"
  printf 'data %d\n%s\n' "${#value}" "$value"
}

emit_fast_import_file() {
  local repo_path="$1" source_path="$2" content
  # Command substitution removes the file's one trailing newline; restore it
  # so the inline blob is byte-identical to the generated source file.
  content="$(<"$source_path")"
  content+=$'\n'
  printf 'M 100644 inline %s\n' "$repo_path"
  printf 'data %d\n' "${#content}"
  printf '%s\n' "$content"
}

# Build both user-scaled histories with one Git process. Each feature commit
# adds one file; only the first trunk commit changes the tree, while the
# remaining commits are deliberately empty and differ by message.
IMPORT_TIME="$(date +%s)"
{
  for i in $(seq 1 "$STACK_COMMITS"); do
    mark="$i"
    message="AI feature $i"
    f="feature_$(printf '%03d' "$i").txt"
    printf 'commit refs/heads/feature\n'
    printf 'mark :%d\n' "$mark"
    printf 'committer Repro User <repro@example.com> %d +0000\n' "$(( IMPORT_TIME + i ))"
    emit_fast_import_data "$message"
    if (( i == 1 )); then
      printf 'from %s\n' "$FORK_POINT"
    else
      printf 'from :%d\n' "$(( mark - 1 ))"
    fi
    emit_fast_import_file "$f" "$FEATURE_FILES_DIR/$f"
    printf '\n'
  done

  trunk_mark=$(( STACK_COMMITS + 1 ))
  message="trunk 1: rewrite all base files"
  printf 'commit refs/heads/main\n'
  printf 'mark :%d\n' "$trunk_mark"
  printf 'committer Repro User <repro@example.com> %d +0000\n' \
    "$(( IMPORT_TIME + STACK_COMMITS + 1 ))"
  emit_fast_import_data "$message"
  printf 'from %s\n' "$FORK_POINT"
  for i in $(seq 1 "$BASE_FILES"); do
    f="module_$(printf '%03d' "$i").txt"
    emit_fast_import_file "src/$f" "$TRUNK_FILES_DIR/$f"
  done
  printf '\n'

  for i in $(seq 2 "$TRUNK_COMMITS"); do
    mark=$(( STACK_COMMITS + i ))
    message="trunk $i"
    printf 'commit refs/heads/main\n'
    printf 'mark :%d\n' "$mark"
    printf 'committer Repro User <repro@example.com> %d +0000\n' \
      "$(( IMPORT_TIME + STACK_COMMITS + i ))"
    emit_fast_import_data "$message"
    printf 'from :%d\n\n' "$(( mark - 1 ))"
  done
  printf 'done\n'
} | qgit fast-import --quiet --done

ORIG_TIP="$(qgit rev-parse feature)"
ORIG_COMMITS_FILE="$WORK/orig_commits.txt"
qgit rev-list --reverse "$FORK_POINT..feature" > "$ORIG_COMMITS_FILE"
TRUNK_COMMITS_FILE="$WORK/trunk_commits.txt"
qgit rev-list "$FORK_POINT..main" > "$TRUNK_COMMITS_FILE"
log "created $STACK_COMMITS feature and $TRUNK_COMMITS trunk commits with one fast-import process"

# Attach the donor blob to every synthetic commit with one notes commit and
# one Git process. `N` lets fast-import choose the repository's notes fanout.
NOTE_BLOB="$(qgit notes --ref=ai list "$NOTE_DONOR_COMMIT" 2>/dev/null)"
[[ -n "$NOTE_BLOB" ]] || { echo "error: no authorship note blob found to reuse" >&2; exit 1; }
NOTES_PARENT="$(qgit rev-parse refs/notes/ai)"
{
  printf 'commit refs/notes/ai\n'
  printf 'committer git-ai <git-ai@local> %d +0000\n' \
    "$(( IMPORT_TIME + STACK_COMMITS + TRUNK_COMMITS + 1 ))"
  printf 'data 0\n'
  printf 'from %s\n' "$NOTES_PARENT"
  while read -r sha; do
    printf 'N %s %s\n' "$NOTE_BLOB" "$sha"
  done < "$ORIG_COMMITS_FILE"
  while read -r sha; do
    printf 'N %s %s\n' "$NOTE_BLOB" "$sha"
  done < "$TRUNK_COMMITS_FILE"
  printf '\ndone\n'
} | qgit fast-import --quiet --done

log "attached authorship notes to all $STACK_COMMITS feature and $TRUNK_COMMITS trunk commits"
log "trunk advanced by $TRUNK_COMMITS commits (delta: every line of $BASE_FILES x $LINES_PER_FILE rewritten)"

# The setup restack: rebase feature onto the advanced trunk (traced). This is
# the CHEAP direction — the range-diff old range is just the stack.
qgit checkout -q feature
log "running: git rebase main (traced -> daemon)"
tgit rebase main >/dev/null
REBASED_TIP="$(qgit rev-parse feature)"

REBASED_COMMITS_FILE="$WORK/rebased_commits.txt"
qgit rev-list --reverse "main..feature" > "$REBASED_COMMITS_FILE"

deadline=$(( $(date +%s) + 300 ))
while :; do
  n="$(notes_hit_count "$REBASED_COMMITS_FILE")"
  (( n >= STACK_COMMITS )) && break
  if (( $(date +%s) > deadline )); then
    echo "error: only $n/$STACK_COMMITS rebased commits got migrated notes in 300s" >&2
    echo "daemon log tail:"; tail -30 "$DAEMON_LOG"
    exit 1
  fi
  sleep 1
done
log "rebase note migration complete (notes on all $STACK_COMMITS rebased commits)"
sleep 2  # let the daemon fully quiesce before baselining

NOTES_REF_BEFORE="$(qgit rev-parse refs/notes/ai)"
DAEMON_LOG_LINES_BEFORE="$(wc -l < "$DAEMON_LOG" | tr -d ' ')"

# ---------------------------------------------------------------------------
# RSS sampler for the daemon (+ its direct children, i.e. the spawned
# `git diff-tree` / `git range-diff`). Columns: epoch daemon_rss_kb children_rss_kb
# ---------------------------------------------------------------------------
(
  while kill -0 "$DAEMON_PID" 2>/dev/null; do
    ps -axo pid=,ppid=,rss= | awk -v d="$DAEMON_PID" -v t="$(date +%s)" '
      $1 == d { drss = $3 } $2 == d { crss += $3 }
      END { printf "%s %d %d\n", t, drss, crss }' >> "$RSS_LOG"
    sleep 0.2
  done
) &
SAMPLER_PID=$!

BASELINE_KB="$(ps -o rss= -p "$DAEMON_PID" | tr -d ' ')"
log "daemon baseline RSS: $(( BASELINE_KB / 1024 )) MB"

# ---------------------------------------------------------------------------
# THE TRIGGER: undo the restack — move the branch from the rebased tip back
# to the original tip with `reset --keep`, exactly like gt's restack undo.
# (`reset --hard` would NOT fire the rewrite path — see header comment.)
# ---------------------------------------------------------------------------
log "running: git reset --keep $ORIG_TIP (traced -> daemon)"
RESET_START=$(date +%s)
tgit reset --keep "$ORIG_TIP" >/dev/null
RESET_END=$(date +%s)
log "git reset finished in $(( RESET_END - RESET_START ))s; daemon now processes the rewrite asynchronously"

# Completion: the shift ends with ONE batched notes write, so refs/notes/ai
# moving is the "done" signal.
deadline=$(( $(date +%s) + PROCESS_TIMEOUT_SECS ))
while :; do
  now="$(qgit rev-parse refs/notes/ai)"
  if [[ "$now" != "$NOTES_REF_BEFORE" ]]; then
    DONE_TS=$(date +%s)
    break
  fi
  if (( $(date +%s) > deadline )); then
    echo "error: rewrite processing incomplete after ${PROCESS_TIMEOUT_SECS}s (refs/notes/ai unchanged)" >&2
    echo "daemon log tail:"; tail -40 "$DAEMON_LOG"
    exit 1
  fi
  sleep 0.5
done
PROCESS_SECS=$(( DONE_TS - RESET_END ))
log "rewrite processing complete: refs/notes/ai rewritten ($PROCESS_SECS s after reset exit)"

# Let RSS settle a moment, then stop sampling.
sleep 2
kill "$SAMPLER_PID" 2>/dev/null || true
wait "$SAMPLER_PID" 2>/dev/null || true
SAMPLER_PID=""

# ---------------------------------------------------------------------------
# Ground truth
# ---------------------------------------------------------------------------
# Mapping count as the daemon derived it (debug log), if present.
MAPPINGS_LOGGED="$(tail -n "+$(( DAEMON_LOG_LINES_BEFORE + 1 ))" "$DAEMON_LOG" \
  | grep -o 'shift_authorship_notes: [0-9]* mappings' | grep -o '[0-9]*' | sort -n | tail -1 || true)"
# Unmatched `<` commits in the same range-diff the daemon ran.
RANGE_DIFF_DROPPED="$(qgit range-diff --no-color --no-abbrev -s --creation-factor=100 \
  "$FORK_POINT..$REBASED_TIP" "$FORK_POINT..$ORIG_TIP" | grep -c '<' || true)"
# One pair's diff, same flags as compute_diff_tree_stdin: '+' lines are what
# the parser keeps (one u32 per line), bytes are what gets streamed per pair.
PAIR_STATS="$(qgit diff-tree -p -U0 -M --no-color -r "$(head -1 "$TRUNK_COMMITS_FILE")" "$ORIG_TIP" \
  | awk '/^\+/ && !/^\+\+\+/ { plus++ } { bytes += length($0) + 1 } END { printf "%d %d", plus, bytes }')"
PAIR_PLUS_LINES="$(cut -d' ' -f1 <<<"$PAIR_STATS")"
PAIR_BYTES="$(cut -d' ' -f2 <<<"$PAIR_STATS")"

PEAK_KB="$(awk 'BEGIN{m=0} {if ($2>m) m=$2} END{print m}' "$RSS_LOG")"
PEAK_CHILD_KB="$(awk 'BEGIN{m=0} {if ($3>m) m=$3} END{print m}' "$RSS_LOG")"

if [[ -n "$MAPPINGS_LOGGED" ]] && (( MAPPINGS_LOGGED > STACK_COMMITS )); then
  echo "error: restack undo retained $MAPPINGS_LOGGED mappings; expected at most $STACK_COMMITS stack mappings" >&2
  exit 1
fi

echo
echo "================================ RESULTS ================================"
echo "knobs:                  SCALE=$SCALE STACK_COMMITS=$STACK_COMMITS TRUNK_COMMITS=$TRUNK_COMMITS BASE_FILES=$BASE_FILES LINES_PER_FILE=$LINES_PER_FILE"
echo "mappings (daemon log):  ${MAPPINGS_LOGGED:-n/a} (range-diff '<' commits: $RANGE_DIFF_DROPPED)"
echo "per-pair diff:          $PAIR_PLUS_LINES '+' lines, $(( PAIR_BYTES / 1048576 )) MB raw"
if [[ -n "$MAPPINGS_LOGGED" ]]; then
  MAPPED_BYTES_ESTIMATE=$(( PAIR_BYTES * MAPPINGS_LOGGED / 1048576 ))
  AVOIDED_BYTES_ESTIMATE=$(( PAIR_BYTES * RANGE_DIFF_DROPPED / 1048576 ))
else
  MAPPED_BYTES_ESTIMATE=0
  AVOIDED_BYTES_ESTIMATE=0
fi
echo "mapped diff estimate:   ~$MAPPED_BYTES_ESTIMATE MB at the sampled per-pair size"
echo "avoided bogus estimate: ~$AVOIDED_BYTES_ESTIMATE MB from discarded leading commits"
echo "daemon RSS baseline:    $(( BASELINE_KB / 1024 )) MB"
echo "daemon RSS peak:        $(( PEAK_KB / 1024 )) MB  (delta +$(( (PEAK_KB - BASELINE_KB) / 1024 )) MB)"
echo "peak child git RSS:     $(( PEAK_CHILD_KB / 1024 )) MB  (git diff-tree/range-diff spawned by daemon)"
echo "rewrite processing:     ${PROCESS_SECS}s (reset exit -> batched notes write)"
echo "========================================================================="
