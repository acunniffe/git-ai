#!/usr/bin/env python3
"""Run the nasty rebase benchmark across mode variants."""

from __future__ import annotations

import argparse
import csv
import dataclasses
import json
import math
import os
import signal
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


class BenchmarkError(RuntimeError):
    pass


@dataclasses.dataclass(frozen=True)
class Variant:
    key: str
    label: str
    binary: Path
    mode: str  # wrapper | daemon


@dataclasses.dataclass
class VariantRunResult:
    variant: str
    repetition: int
    durations_s: dict[str, float]
    statuses: dict[str, str]
    saved_logs: dict[str, int]
    head_has_note: dict[str, str]


@dataclasses.dataclass(frozen=True)
class MarginCheckResult:
    scenario: str
    variant: str
    baseline_s: float
    median_s: float
    allowed_s: float
    slowdown_pct: float
    passed: bool


def now_iso_utc() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def run_cmd(
    cmd: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    timeout_s: int = 5400,
) -> subprocess.CompletedProcess[str]:
    proc = subprocess.run(
        cmd,
        cwd=str(cwd),
        env=env,
        text=True,
        capture_output=True,
        check=False,
        timeout=timeout_s,
    )
    if proc.returncode != 0:
        raise BenchmarkError(
            "Command failed\n"
            f"cmd: {' '.join(cmd)}\n"
            f"cwd: {cwd}\n"
            f"exit: {proc.returncode}\n"
            f"stdout:\n{proc.stdout}\n"
            f"stderr:\n{proc.stderr}\n"
        )
    return proc


def build_release_binary(repo_dir: Path, target_dir: Path) -> Path:
    env = dict(os.environ)
    env["CARGO_TARGET_DIR"] = str(target_dir)
    run_cmd(
        ["cargo", "build", "--release", "--bin", "git-ai"],
        cwd=repo_dir,
        env=env,
        timeout_s=3600,
    )
    if os.name == "nt":
        binary = target_dir / "release" / "git-ai.exe"
    else:
        binary = target_dir / "release" / "git-ai"
    if not binary.exists():
        raise BenchmarkError(f"Expected binary not found: {binary}")
    return binary


def prepare_main_worktree(repo_root: Path, main_ref: str, worktree_dir: Path) -> None:
    if worktree_dir.exists():
        shutil.rmtree(worktree_dir)
    run_cmd(["git", "fetch", "--quiet", "origin", "main"], cwd=repo_root, env=dict(os.environ))
    run_cmd(
        ["git", "worktree", "add", "--detach", str(worktree_dir), main_ref],
        cwd=repo_root,
        env=dict(os.environ),
    )


def remove_main_worktree(repo_root: Path, worktree_dir: Path) -> None:
    run_cmd(
        ["git", "worktree", "remove", "--force", str(worktree_dir)],
        cwd=repo_root,
        env=dict(os.environ),
    )


def create_link_or_copy(target: Path, link_path: Path) -> None:
    if link_path.exists() or link_path.is_symlink():
        if link_path.is_dir() and not link_path.is_symlink():
            shutil.rmtree(link_path)
        else:
            link_path.unlink()
    link_path.parent.mkdir(parents=True, exist_ok=True)
    try:
        link_path.symlink_to(target)
    except OSError:
        shutil.copy2(target, link_path)


def resolve_real_git_binary(repo_root: Path) -> Path:
    preferred = [
        Path("/usr/bin/git"),
        Path("/opt/homebrew/bin/git"),
        Path("/usr/local/bin/git"),
        Path("/bin/git"),
    ]
    for candidate in preferred:
        if candidate.exists() and os.access(candidate, os.X_OK):
            return candidate.resolve()

    fallback = shutil.which("git")
    if not fallback:
        raise BenchmarkError("Unable to resolve system git from PATH.")

    fallback_path = Path(fallback).resolve()
    if (
        "git-ai" in fallback_path.name.lower()
        or str(repo_root / "target") in str(fallback_path)
    ):
        raise BenchmarkError(
            "Resolved `git` points to a git-ai wrapper, not the real git binary. "
            "Install git or pass a clean PATH."
        )
    return fallback_path


def git_output(repo_dir: Path, args: list[str]) -> str:
    proc = run_cmd(["git", *args], cwd=repo_dir, env=dict(os.environ), timeout_s=120)
    return (proc.stdout or "").strip()


def clone_seed_repo(repo_url: str, seed_repo_dir: Path, real_git: Path) -> tuple[Path, str]:
    if seed_repo_dir.exists():
        shutil.rmtree(seed_repo_dir)
    run_cmd(
        [str(real_git), "clone", "--depth", "1", repo_url, str(seed_repo_dir)],
        cwd=seed_repo_dir.parent,
        env=dict(os.environ),
        timeout_s=3600,
    )
    seed_head = run_cmd(
        [str(real_git), "rev-parse", "HEAD"],
        cwd=seed_repo_dir,
        env=dict(os.environ),
    ).stdout.strip()
    return seed_repo_dir, seed_head


def start_perf_profiler(
    daemon_proc: subprocess.Popen[str] | None,
    output_path: Path,
) -> tuple[subprocess.Popen[bytes], Any]:
    if daemon_proc is None:
        raise BenchmarkError("Cannot profile a daemon that is not running")
    if os.name != "posix":
        raise BenchmarkError("Flamegraph profiling requires a POSIX perf environment")
    if shutil.which("perf") is None:
        raise BenchmarkError("Flamegraph profiling requires `perf` on PATH")

    output_path.parent.mkdir(parents=True, exist_ok=True)
    log_path = output_path.with_suffix(".log")
    log_file = log_path.open("w", encoding="utf-8")
    profiler = subprocess.Popen(
        [
            "perf",
            "record",
            "-F",
            "99",
            "--call-graph",
            "dwarf,4096",
            "--output",
            str(output_path),
            "--pid",
            str(daemon_proc.pid),
        ],
        stdout=log_file,
        stderr=subprocess.STDOUT,
    )
    time.sleep(0.5)
    if profiler.poll() is not None:
        log_file.close()
        detail = log_path.read_text(encoding="utf-8", errors="replace")
        raise BenchmarkError(
            f"perf profiler exited before collection started for {output_path}:\n{detail}"
        )
    return profiler, log_file


def stop_perf_profiler(
    profiler: tuple[subprocess.Popen[bytes], Any] | None,
    output_path: Path | None,
) -> None:
    if profiler is None:
        return

    process, log_file = profiler
    try:
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
    finally:
        log_file.close()

    if process.returncode not in (0, -signal.SIGINT, 130):
        log_path = output_path.with_suffix(".log") if output_path else None
        detail = (
            log_path.read_text(encoding="utf-8", errors="replace")
            if log_path and log_path.exists()
            else ""
        )
        raise BenchmarkError(
            f"perf profiler failed for {output_path}: exit={process.returncode}\n{detail}"
        )
    if output_path is not None and not output_path.exists():
        raise BenchmarkError(f"perf profiler did not create {output_path}")


def setup_variant_runtime(
    variant: Variant,
    runtime_root: Path,
    real_git: Path,
    profile_output: Path | None = None,
) -> tuple[
    dict[str, str],
    Path,
    subprocess.Popen[str] | None,
    Path,
    tuple[subprocess.Popen[bytes], Any] | None,
]:
    tmp_root = Path("/tmp") if os.name != "nt" else Path(tempfile.gettempdir())
    home_dir = Path(
        tempfile.mkdtemp(prefix=f"gai-nasty-{variant.key}-", dir=str(tmp_root))
    )
    bin_dir = runtime_root / "bin"
    wrapper_git = bin_dir / ("git.exe" if os.name == "nt" else "git")

    home_dir.mkdir(parents=True, exist_ok=True)
    bin_dir.mkdir(parents=True, exist_ok=True)

    if variant.mode == "wrapper":
        create_link_or_copy(variant.binary, wrapper_git)

    env = dict(os.environ)
    env["HOME"] = str(home_dir)
    env["GIT_CONFIG_GLOBAL"] = str(home_dir / ".gitconfig")
    env["GIT_TERMINAL_PROMPT"] = "0"
    env["GIT_AI_DEBUG"] = "0"
    env["GIT_AI_DEBUG_PERFORMANCE"] = "0"
    env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"

    daemon_proc: subprocess.Popen[str] | None = None
    profiler: tuple[subprocess.Popen[bytes], Any] | None = None
    if variant.mode == "daemon":
        daemon_dir = home_dir / ".git-ai" / "internal" / "daemon"
        control_socket = daemon_dir / "control.sock"
        trace_socket = daemon_dir / "trace2.sock"
        daemon_dir.mkdir(parents=True, exist_ok=True)
        daemon_proc = subprocess.Popen(
            [str(variant.binary), "daemon", "run"],
            cwd=str(runtime_root),
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        exit_code: int | None = None
        for _ in range(300):
            if control_socket.exists() and trace_socket.exists():
                break
            if exit_code is None:
                exit_code = daemon_proc.poll()
            time.sleep(0.01)
        else:
            raise BenchmarkError(
                "timed out waiting for daemon sockets "
                f"(control={control_socket}, trace={trace_socket})"
            )
        if daemon_proc.poll() is not None:
            daemon_proc = None

        env["GIT_TRACE2_EVENT"] = f"af_unix:stream:{trace_socket}"
        env["GIT_TRACE2_EVENT_NESTING"] = os.environ.get(
            "GIT_AI_TEST_TRACE2_NESTING",
            "0",
        )
        env["GIT_AI_DAEMON_CHECKPOINT_DELEGATE"] = "true"
        env["GIT_AI_DAEMON_CONTROL_SOCKET"] = str(control_socket)
        if profile_output is not None:
            profiler = start_perf_profiler(daemon_proc, profile_output)

    git_bin = wrapper_git if variant.mode == "wrapper" else real_git
    return env, git_bin, daemon_proc, home_dir, profiler


def shutdown_daemon(
    variant: Variant,
    runtime_root: Path,
    env: dict[str, str],
    daemon_proc: subprocess.Popen[str] | None,
    profiler: tuple[subprocess.Popen[bytes], Any] | None = None,
    profile_output: Path | None = None,
) -> None:
    if variant.mode != "daemon":
        return
    try:
        stop_perf_profiler(profiler, profile_output)
    except BenchmarkError as err:
        print(f"::warning::{err}")
    try:
        run_cmd(
            [str(variant.binary), "daemon", "shutdown"],
            cwd=runtime_root,
            env=env,
            timeout_s=30,
        )
    except Exception:
        pass

    if daemon_proc is not None:
        deadline = time.time() + 5.0
        while time.time() < deadline:
            if daemon_proc.poll() is not None:
                return
            time.sleep(0.05)

        if daemon_proc.poll() is None:
            daemon_proc.kill()
            daemon_proc.wait(timeout=5)


def parse_results_tsv(path: Path) -> tuple[dict[str, float], dict[str, str], dict[str, int], dict[str, str]]:
    if not path.exists():
        raise BenchmarkError(f"Missing results TSV: {path}")

    durations: dict[str, float] = {}
    statuses: dict[str, str] = {}
    saved_logs: dict[str, int] = {}
    head_has_note: dict[str, str] = {}

    with path.open("r", encoding="utf-8") as fh:
        reader = csv.DictReader(fh, delimiter="\t")
        for row in reader:
            scenario = (row.get("scenario") or "").strip()
            if not scenario:
                continue
            durations[scenario] = float(row.get("duration_s") or 0.0)
            statuses[scenario] = (row.get("status") or "").strip()
            saved_logs[scenario] = int(float(row.get("saved_logs") or 0))
            head_has_note[scenario] = (row.get("head_note") or "").strip()

    if not durations:
        raise BenchmarkError(f"No scenario rows parsed from {path}")
    return durations, statuses, saved_logs, head_has_note


def summarize_variant_runs(
    all_runs: list[VariantRunResult],
) -> dict[str, dict[str, dict[str, Any]]]:
    grouped: dict[str, dict[str, list[float]]] = {}
    for run in all_runs:
        for scenario, value in run.durations_s.items():
            grouped.setdefault(scenario, {}).setdefault(run.variant, []).append(value)

    summary: dict[str, dict[str, dict[str, Any]]] = {}
    for scenario, by_variant in grouped.items():
        scenario_summary: dict[str, dict[str, Any]] = {}
        for variant, samples in by_variant.items():
            ordered = sorted(samples)
            scenario_summary[variant] = {
                "runs_s": [round(v, 3) for v in samples],
                "median_s": round(statistics.median(ordered), 3),
                "mean_s": round(statistics.mean(ordered), 3),
                "min_s": round(min(ordered), 3),
                "max_s": round(max(ordered), 3),
                "stdev_s": round(statistics.pstdev(ordered) if len(ordered) > 1 else 0.0, 3),
            }
        summary[scenario] = scenario_summary
    return summary


def compute_slowdowns(
    summary: dict[str, dict[str, dict[str, Any]]],
    baseline_key: str,
) -> dict[str, dict[str, float]]:
    out: dict[str, dict[str, float]] = {}
    for scenario, by_variant in summary.items():
        if baseline_key not in by_variant:
            continue
        baseline = float(by_variant[baseline_key]["median_s"])
        if baseline <= 0:
            continue
        row: dict[str, float] = {}
        for variant, stats in by_variant.items():
            if variant == baseline_key:
                continue
            median = float(stats["median_s"])
            row[variant] = round(((median - baseline) / baseline) * 100.0, 3)
        out[scenario] = row
    return out


def compute_margin_checks(
    summary: dict[str, dict[str, dict[str, Any]]],
    *,
    baseline_key: str,
    margin_pct: float,
    variants: list[str],
) -> list[MarginCheckResult]:
    checks: list[MarginCheckResult] = []
    margin_multiplier = 1.0 + (margin_pct / 100.0)
    for scenario, by_variant in summary.items():
        baseline = by_variant.get(baseline_key, {}).get("median_s")
        if baseline is None:
            continue
        baseline_f = float(baseline)
        if baseline_f <= 0.0:
            continue
        allowed = baseline_f * margin_multiplier
        for variant in variants:
            stats = by_variant.get(variant)
            if stats is None:
                continue
            median = float(stats["median_s"])
            slowdown = ((median - baseline_f) / baseline_f) * 100.0
            checks.append(
                MarginCheckResult(
                    scenario=scenario,
                    variant=variant,
                    baseline_s=round(baseline_f, 3),
                    median_s=round(median, 3),
                    allowed_s=round(allowed, 3),
                    slowdown_pct=round(slowdown, 3),
                    passed=median <= allowed,
                )
            )
    return checks


def geometric_mean(values: list[float]) -> float:
    if not values:
        return 1.0
    return math.exp(sum(math.log(v) for v in values) / len(values))


def render_report(
    report_path: Path,
    metadata: dict[str, Any],
    summary: dict[str, dict[str, dict[str, Any]]],
    slowdowns: dict[str, dict[str, float]],
    margin_checks: list[MarginCheckResult],
) -> None:
    scenarios = ["linear", "onto", "rebase_merges"]
    variants = [
        "main_daemon",
        "current_daemon",
    ]
    margin_baseline_key = str(metadata["margin_baseline"])
    margin_baseline_label = margin_baseline_key.replace("_", " ")

    lines: list[str] = []
    lines.append("# git-ai Nasty Rebase Benchmark (Modes vs main)")
    lines.append("")
    lines.append("## Run Metadata")
    lines.append("")
    lines.append(f"- Timestamp (UTC): `{metadata['timestamp_utc']}`")
    lines.append(f"- Repo root: `{metadata['repo_root']}`")
    lines.append(f"- Branch: `{metadata['branch']}`")
    lines.append(f"- Branch SHA: `{metadata['branch_sha']}`")
    lines.append(f"- Main ref: `{metadata['main_ref']}`")
    lines.append(f"- Main SHA: `{metadata['main_sha']}`")
    lines.append(f"- Seed repo source URL: `{metadata['repo_url']}`")
    lines.append(f"- Seed repo head SHA: `{metadata['seed_repo_head']}`")
    lines.append(f"- Repetitions: `{metadata['repetitions']}`")
    lines.append(
        "- Workload: "
        f"feature={metadata['feature_commits']}, main={metadata['main_commits']}, "
        f"side={metadata['side_commits']}, files={metadata['files']}, "
        f"lines/file={metadata['lines_per_file']}, burst_every={metadata['burst_every']}"
    )
    lines.append("")

    lines.append("## Median Duration (s) and Slowdown vs main(daemon)")
    lines.append("")
    lines.append("| Scenario | main(daemon) | current(daemon) | daemon Δ% |")
    lines.append("|---|---:|---:|---:|")

    for scenario in scenarios:
        row = summary.get(scenario, {})
        base = float(row.get("main_daemon", {}).get("median_s", 0.0))
        cd = float(row.get("current_daemon", {}).get("median_s", 0.0))
        s = slowdowns.get(scenario, {})
        lines.append(
            f"| {scenario} | {base:.3f} | {cd:.3f} | "
            f"{s.get('current_daemon', 0.0):.3f}% |"
        )

    lines.append("")
    lines.append("## Aggregate Comparison")
    lines.append("")
    lines.append("| Variant | Geometric Mean Ratio vs main(daemon) | Geometric Mean Slowdown |")
    lines.append("|---|---:|---:|")

    for key in ["current_daemon"]:
        ratios: list[float] = []
        for scenario in scenarios:
            row = summary.get(scenario, {})
            base = float(row.get("main_daemon", {}).get("median_s", 0.0))
            med = float(row.get(key, {}).get("median_s", 0.0))
            if base > 0 and med > 0:
                ratios.append(med / base)
        gm = geometric_mean(ratios)
        lines.append(f"| {key} | {gm:.4f}x | {(gm - 1.0) * 100.0:.3f}% |")

    lines.append("")
    lines.append("## Margin Check")
    lines.append("")
    lines.append(
        f"- Required margin: current modes must be <= `{metadata['margin_pct']:.1f}%` slower than `{margin_baseline_label}`"
    )
    lines.append(
        "| Scenario | Variant | Baseline (s) | Variant Median (s) | Allowed Max (s) | Slowdown | Status |"
    )
    lines.append("|---|---|---:|---:|---:|---:|---|")
    for check in sorted(margin_checks, key=lambda c: (c.scenario, c.variant)):
        status = "PASS" if check.passed else "FAIL"
        lines.append(
            f"| {check.scenario} | {check.variant} | {check.baseline_s:.3f} | "
            f"{check.median_s:.3f} | {check.allowed_s:.3f} | {check.slowdown_pct:.3f}% | {status} |"
        )
    failed = [check for check in margin_checks if not check.passed]
    lines.append("")
    lines.append(
        f"- Overall: `{len(margin_checks) - len(failed)}/{len(margin_checks)}` checks passing"
    )

    lines.append("")
    lines.append("## Re-run")
    lines.append("")
    lines.append("```bash")
    lines.append(
        "python3 scripts/benchmarks/git/benchmark_nasty_modes_vs_main.py "
        f"--repetitions {metadata['repetitions']} "
        f"--feature-commits {metadata['feature_commits']} "
        f"--main-commits {metadata['main_commits']} "
        f"--side-commits {metadata['side_commits']} "
        f"--files {metadata['files']} "
        f"--lines-per-file {metadata['lines_per_file']} "
        f"--burst-every {metadata['burst_every']} "
        f"--margin-pct {metadata['margin_pct']:.1f} "
        f"--margin-baseline {metadata['margin_baseline']}"
    )
    lines.append("```")

    report_path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run heavy nasty rebase benchmark across mode variants."
    )
    parser.add_argument("--work-root", type=Path, default=None)
    parser.add_argument("--main-ref", default="origin/main")
    parser.add_argument("--repo-url", default="https://github.com/python/cpython.git")
    parser.add_argument("--feature-commits", type=int, default=90)
    parser.add_argument("--main-commits", type=int, default=35)
    parser.add_argument("--side-commits", type=int, default=25)
    parser.add_argument("--files", type=int, default=6)
    parser.add_argument("--lines-per-file", type=int, default=1500)
    parser.add_argument("--burst-every", type=int, default=15)
    parser.add_argument("--repetitions", type=int, default=1)
    parser.add_argument(
        "--margin-pct",
        type=float,
        default=25.0,
        help="Maximum allowed slowdown percentage relative to --margin-baseline.",
    )
    parser.add_argument(
        "--enforce-margin",
        action="store_true",
        help="Exit non-zero when the current_daemon margin check fails.",
    )
    parser.add_argument(
        "--margin-baseline",
        type=str,
        choices=["main_daemon"],
        default="main_daemon",
        help="Baseline variant for margin checks.",
    )
    parser.add_argument("--current-bin", type=Path, default=None)
    parser.add_argument("--main-bin", type=Path, default=None)
    parser.add_argument(
        "--flamegraph-dir",
        type=Path,
        default=None,
        help="Collect one perf.data profile per daemon variant and repetition.",
    )
    parser.add_argument("--keep-artifacts", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    repo_root = Path(__file__).resolve().parents[3]
    nasty_script = repo_root / "scripts" / "benchmarks" / "git" / "benchmark_nasty_rebases.sh"

    if not nasty_script.exists():
        raise BenchmarkError(f"Missing benchmark script: {nasty_script}")

    if args.repetitions <= 0:
        raise BenchmarkError("--repetitions must be positive")
    if args.margin_pct < 0:
        raise BenchmarkError("--margin-pct must be non-negative")

    if args.work_root is None:
        work_root = Path(tempfile.mkdtemp(prefix="git-ai-nasty-modes-"))
    else:
        work_root = args.work_root.resolve()
        work_root.mkdir(parents=True, exist_ok=True)

    real_git = resolve_real_git_binary(repo_root)
    build_dir = work_root / "build"
    targets_dir = build_dir / "targets"
    main_worktree = build_dir / "main-worktree"
    seed_repo_dir = work_root / "seed-repo"
    build_dir.mkdir(parents=True, exist_ok=True)
    targets_dir.mkdir(parents=True, exist_ok=True)

    created_main_worktree = False

    try:
        if args.current_bin is not None:
            current_bin = args.current_bin.resolve()
            if not current_bin.exists():
                raise BenchmarkError(f"Current binary not found: {current_bin}")
        else:
            print("Building current branch binary...")
            current_bin = build_release_binary(repo_root, targets_dir / "current")

        if args.main_bin is not None:
            main_bin = args.main_bin.resolve()
            if not main_bin.exists():
                raise BenchmarkError(f"Main binary not found: {main_bin}")
            main_sha = "unknown (external binary)"
        else:
            print(f"Preparing main worktree at {args.main_ref}...")
            prepare_main_worktree(repo_root, args.main_ref, main_worktree)
            created_main_worktree = True
            print("Building main branch binary...")
            try:
                main_bin = build_release_binary(main_worktree, targets_dir / "main")
            except BenchmarkError as err:
                print(
                    "::warning::Skipping nasty benchmark because the main baseline failed to build."
                )
                print(f"Baseline build error: {err}")
                return 0
            main_sha = git_output(main_worktree, ["rev-parse", "HEAD"])

        print("Cloning seed repo snapshot...")
        seed_repo_path, seed_repo_head = clone_seed_repo(args.repo_url, seed_repo_dir, real_git)

        # git-ai is daemon-architecture: all attribution side effects run in the
        # daemon (the git proxy is a thin trace2-emitting passthrough). Wrapper
        # mode with no daemon captures nothing, so the only meaningful comparison
        # is daemon-vs-daemon: this branch's daemon vs main's daemon.
        variants = [
            Variant("main_daemon", "main(daemon)", main_bin, "daemon"),
            Variant("current_daemon", "current(daemon)", current_bin, "daemon"),
        ]

        all_results: list[VariantRunResult] = []

        if args.flamegraph_dir is not None:
            args.flamegraph_dir.mkdir(parents=True, exist_ok=True)

        for variant in variants:
            for repetition in range(1, args.repetitions + 1):
                rep_root = work_root / "runs" / variant.key / f"rep_{repetition:02d}"
                if rep_root.exists():
                    shutil.rmtree(rep_root)
                rep_root.mkdir(parents=True, exist_ok=True)

                runtime_root = rep_root / "runtime"
                profile_output = (
                    args.flamegraph_dir
                    / variant.key
                    / f"rep_{repetition:02d}.data"
                    if args.flamegraph_dir is not None
                    else None
                )
                env, git_bin, daemon_proc, home_dir, profiler = setup_variant_runtime(
                    variant, runtime_root, real_git, profile_output
                )
                try:
                    cmd = [
                        "bash",
                        str(nasty_script),
                        "--repo-url",
                        str(seed_repo_path),
                        "--work-root",
                        str(rep_root / "benchmark"),
                        "--feature-commits",
                        str(args.feature_commits),
                        "--main-commits",
                        str(args.main_commits),
                        "--side-commits",
                        str(args.side_commits),
                        "--files",
                        str(args.files),
                        "--lines-per-file",
                        str(args.lines_per_file),
                        "--burst-every",
                        str(args.burst_every),
                        "--git-bin",
                        str(git_bin),
                        "--git-ai-bin",
                        str(variant.binary),
                        "--hook-mode",
                        variant.mode,
                    ]

                    print(
                        f"[variant-run] variant={variant.key} repetition={repetition}/{args.repetitions}"
                    )
                    run_cmd(cmd, cwd=repo_root, env=env, timeout_s=14400)

                    results_tsv = rep_root / "benchmark" / "results.tsv"
                    durations, statuses, saved_logs, head_note = parse_results_tsv(results_tsv)
                    all_results.append(
                        VariantRunResult(
                            variant=variant.key,
                            repetition=repetition,
                            durations_s=durations,
                            statuses=statuses,
                            saved_logs=saved_logs,
                            head_has_note=head_note,
                        )
                    )

                    for scenario in sorted(durations.keys()):
                        print(
                            f"[variant-result] variant={variant.key} rep={repetition} "
                            f"scenario={scenario} status={statuses.get(scenario)} duration_s={durations[scenario]:.3f}"
                        )

                    if not args.keep_artifacts:
                        bench_repo = rep_root / "benchmark" / "repo"
                        if bench_repo.exists():
                            shutil.rmtree(bench_repo, ignore_errors=True)
                finally:
                    shutdown_daemon(
                        variant,
                        runtime_root,
                        env,
                        daemon_proc,
                        profiler,
                        profile_output,
                    )
                    shutil.rmtree(home_dir, ignore_errors=True)

        summary = summarize_variant_runs(all_results)
        slowdowns = compute_slowdowns(summary, baseline_key="main_daemon")
        margin_checks = compute_margin_checks(
            summary,
            baseline_key=args.margin_baseline,
            margin_pct=args.margin_pct,
            variants=["current_daemon"],
        )

        timestamp = time.strftime("%Y%m%d-%H%M%S", time.localtime())
        artifacts = work_root / "artifacts" / timestamp
        artifacts.mkdir(parents=True, exist_ok=True)

        metadata: dict[str, Any] = {
            "timestamp_utc": now_iso_utc(),
            "repo_root": str(repo_root),
            "branch": git_output(repo_root, ["rev-parse", "--abbrev-ref", "HEAD"]),
            "branch_sha": git_output(repo_root, ["rev-parse", "HEAD"]),
            "main_ref": args.main_ref,
            "main_sha": main_sha,
            "repo_url": args.repo_url,
            "seed_repo_head": seed_repo_head,
            "repetitions": args.repetitions,
            "feature_commits": args.feature_commits,
            "main_commits": args.main_commits,
            "side_commits": args.side_commits,
            "files": args.files,
            "lines_per_file": args.lines_per_file,
            "burst_every": args.burst_every,
            "real_git": str(real_git),
            "margin_pct": args.margin_pct,
            "margin_baseline": args.margin_baseline,
            "flamegraph_dir": str(args.flamegraph_dir) if args.flamegraph_dir else None,
            "variants": {v.key: str(v.binary) for v in variants},
        }

        raw_rows: list[dict[str, Any]] = []
        for result in all_results:
            for scenario, duration_s in result.durations_s.items():
                raw_rows.append(
                    {
                        "scenario": scenario,
                        "variant": result.variant,
                        "repetition": result.repetition,
                        "duration_s": round(duration_s, 3),
                        "status": result.statuses.get(scenario, "unknown"),
                        "saved_logs": result.saved_logs.get(scenario, 0),
                        "head_note": result.head_has_note.get(scenario, ""),
                    }
                )

        csv_path = artifacts / "raw_results.csv"
        with csv_path.open("w", encoding="utf-8", newline="") as fh:
            writer = csv.DictWriter(
                fh,
                fieldnames=[
                    "scenario",
                    "variant",
                    "repetition",
                    "duration_s",
                    "status",
                    "saved_logs",
                    "head_note",
                ],
            )
            writer.writeheader()
            writer.writerows(raw_rows)

        json_path = artifacts / "summary.json"
        json_path.write_text(
            json.dumps(
                {
                    "metadata": metadata,
                    "summary": summary,
                    "slowdowns_pct_vs_main_daemon": slowdowns,
                    "margin_checks": [dataclasses.asdict(check) for check in margin_checks],
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )

        report_path = artifacts / "report.md"
        render_report(report_path, metadata, summary, slowdowns, margin_checks)

        print("")
        print("Nasty mode benchmark complete")
        print(f"- Report: {report_path}")
        print(f"- JSON:   {json_path}")
        print(f"- CSV:    {csv_path}")
        failed_checks = [check for check in margin_checks if not check.passed]
        print(
            f"- Margin checks: {len(margin_checks) - len(failed_checks)}/{len(margin_checks)} passing"
        )
        if args.enforce_margin and failed_checks:
            print("")
            print("Margin enforcement failed:")
            for check in failed_checks:
                print(
                    f"  - {check.scenario} / {check.variant}: "
                    f"{check.slowdown_pct:.3f}% > {args.margin_pct:.1f}%"
                )
            return 2
        return 0

    finally:
        if created_main_worktree:
            try:
                remove_main_worktree(repo_root, main_worktree)
            except Exception as err:  # noqa: BLE001
                print(f"warning: failed to remove main worktree: {err}", file=sys.stderr)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except BenchmarkError as err:
        print(f"error: {err}", file=sys.stderr)
        raise SystemExit(1)
