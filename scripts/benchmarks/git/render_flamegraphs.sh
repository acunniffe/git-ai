#!/usr/bin/env bash
set -euo pipefail

PROFILE_ROOT="${1:?usage: render_flamegraphs.sh <profile-root> <output-root>}"
OUTPUT_ROOT="${2:?usage: render_flamegraphs.sh <profile-root> <output-root>}"

if ! command -v perf >/dev/null 2>&1; then
  echo "error: perf is required to render flamegraphs" >&2
  exit 1
fi
if ! command -v inferno-collapse-perf >/dev/null 2>&1; then
  echo "error: inferno-collapse-perf is required to render flamegraphs" >&2
  exit 1
fi
if ! command -v inferno-flamegraph >/dev/null 2>&1; then
  echo "error: inferno-flamegraph is required to render flamegraphs" >&2
  exit 1
fi

mkdir -p "$OUTPUT_ROOT"

for variant in main_daemon current_daemon; do
  variant_root="$PROFILE_ROOT/$variant"
  if [[ ! -d "$variant_root" ]]; then
    echo "warning: no profiles found for $variant" >&2
    continue
  fi

  perf_script="$OUTPUT_ROOT/${variant}.perf"
  folded="$OUTPUT_ROOT/${variant}.folded"
  flamegraph="$OUTPUT_ROOT/${variant}.svg"
  : > "$perf_script"

  while IFS= read -r -d '' data_file; do
    perf script -i "$data_file" >> "$perf_script"
  done < <(find "$variant_root" -type f -name '*.data' -print0 | sort -z)

  if [[ ! -s "$perf_script" ]]; then
    echo "warning: no perf samples found for $variant" >&2
    continue
  fi

  inferno-collapse-perf < "$perf_script" > "$folded"
  inferno-flamegraph --title "git-ai $variant nightly daemon profile" < "$folded" > "$flamegraph"
  echo "wrote $flamegraph"
done
