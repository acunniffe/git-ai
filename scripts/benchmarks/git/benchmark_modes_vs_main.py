#!/usr/bin/env python3
"""
Benchmark git-ai's daemon against the main-branch daemon.

Compares:
1) main(daemon)
2) current(daemon)

git-ai is daemon-architecture: all attribution side effects run in the daemon
(the git proxy is a thin trace2-emitting passthrough), so the only meaningful
comparison is daemon-vs-daemon -- this branch's daemon vs main's daemon.

The harness builds binaries for both branches, runs a scenario matrix that
covers basic and complex Git operations, and emits:
- raw CSV timings
- machine-readable JSON summary
- Markdown report
"""

from __future__ import annotations

import argparse
import csv
import dataclasses
import json
import math
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Callable

from benchmark_common import (
    BenchmarkError,
    build_release_binary,
    create_link_or_copy,
    git_output,
    now_iso_utc,
    prepare_main_worktree,
    remove_main_worktree,
    resolve_real_git_binary,
    run_cmd,
)


# Daemon quiescence polling: the daemon applies attribution asynchronously, so
# we poll `daemon status` until the family's latest_seq holds steady. These bound
# how long we wait and how often we poll.
DAEMON_IDLE_POLL_S = 0.02
DAEMON_IDLE_TIMEOUT_S = 120.0


@dataclasses.dataclass(frozen=True)
class Variant:
    key: str
    label: str
    binary: Path
    mode: str  # wrapper | daemon


@dataclasses.dataclass(frozen=True)
class Scenario:
    key: str
    description: str
    complexity: str  # basic | complex
    setup: Callable[["VariantRunner", Path], None]
    measure: Callable[["VariantRunner", Path, int], None]
    # Post-sync correctness check (runs OUTSIDE the timed window). Asserts the
    # daemon actually produced the expected authorship notes for this scenario.
    # None for scenarios whose measure step creates no attributable commit
    # (e.g. stash round-trip, mixed reset that only moves HEAD backwards).
    validate: Callable[["VariantRunner", Path], None] | None = None


@dataclasses.dataclass
class RunResult:
    scenario: str
    complexity: str
    variant: str
    run_index: int
    duration_ms: float


@dataclasses.dataclass(frozen=True)
class MarginCheckResult:
    scenario: str
    variant: str
    baseline_ms: float
    median_ms: float
    allowed_ms: float
    slowdown_pct: float
    passed: bool


def write_seed_file(path: Path, seed: int, lines: int) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as fh:
        for i in range(1, lines + 1):
            payload = (seed * 1315423911 + i * 2654435761) & 0xFFFFFFFF
            fh.write(f"seed={seed:08d} line={i:04d} payload={payload:08x}\n")


def append_line(path: Path, line: str) -> None:
    with path.open("a", encoding="utf-8") as fh:
        fh.write(f"{line}\n")


def ignore_transient_git_lockfiles(_src: str, names: list[str]) -> set[str]:
    return {name for name in names if name.endswith(".lock")}


class VariantRunner:
    def __init__(
        self,
        variant: Variant,
        run_root: Path,
        real_git: Path,
        timeout_s: int = 900,
    ) -> None:
        self.variant = variant
        self.real_git = real_git
        self.run_root = run_root
        self.timeout_s = timeout_s

        tmp_root = Path("/tmp") if os.name != "nt" else Path(tempfile.gettempdir())
        self.home_dir = Path(
            tempfile.mkdtemp(prefix=f"gai-modes-{self.variant.key}-", dir=str(tmp_root))
        )
        self.bin_dir = run_root / "bin"
        self.git_wrapper = self.bin_dir / ("git.exe" if os.name == "nt" else "git")

        self.home_dir.mkdir(parents=True, exist_ok=True)
        self.bin_dir.mkdir(parents=True, exist_ok=True)

        if self.variant.mode == "wrapper":
            create_link_or_copy(self.variant.binary, self.git_wrapper)

        self.base_env = dict(os.environ)
        self.base_env["HOME"] = str(self.home_dir)
        self.base_env["GIT_CONFIG_GLOBAL"] = str(self.home_dir / ".gitconfig")
        self.base_env["GIT_TERMINAL_PROMPT"] = "0"
        self.base_env["GIT_AI_DEBUG"] = "0"
        self.base_env["GIT_AI_DEBUG_PERFORMANCE"] = "0"
        self.base_env["PATH"] = f"{self.bin_dir}{os.pathsep}{self.base_env.get('PATH', '')}"

        self.daemon_process: subprocess.Popen[str] | None = None
        self.daemon_started = False
        self.daemon_socket_dir = self.home_dir / ".git-ai" / "internal" / "daemon"
        self.daemon_trace_socket = self.daemon_socket_dir / "trace2.sock"
        self.daemon_control_socket = self.daemon_socket_dir / "control.sock"

        if self.variant.mode == "daemon":
            self.start_daemon()
            self.base_env["GIT_TRACE2_EVENT"] = (
                f"af_unix:stream:{self.daemon_trace_socket}"
            )
            self.base_env["GIT_TRACE2_EVENT_NESTING"] = os.environ.get(
                "GIT_AI_TEST_TRACE2_NESTING",
                "0",
            )
            self.base_env["GIT_AI_DAEMON_CHECKPOINT_DELEGATE"] = "true"
            self.base_env["GIT_AI_DAEMON_CONTROL_SOCKET"] = str(
                self.daemon_control_socket
            )

    def start_daemon(self) -> None:
        self.daemon_socket_dir.mkdir(parents=True, exist_ok=True)
        proc = subprocess.Popen(
            [
                str(self.variant.binary),
                "daemon",
                "run",
            ],
            cwd=str(self.run_root),
            env=self.base_env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        self.daemon_process = proc

        exit_code: int | None = None
        for _ in range(300):
            if self.daemon_control_socket.exists() and self.daemon_trace_socket.exists():
                self.daemon_started = True
                if proc.poll() is not None:
                    self.daemon_process = None
                return
            if exit_code is None:
                exit_code = proc.poll()
            time.sleep(0.01)

        raise BenchmarkError(
            "Timed out waiting for daemon sockets "
            f"(control={self.daemon_control_socket}, trace={self.daemon_trace_socket})"
        )

    def close(self) -> None:
        if self.variant.mode == "daemon" and self.daemon_started:
            try:
                run_cmd(
                    [str(self.variant.binary), "daemon", "shutdown"],
                    cwd=self.run_root,
                    env=self.base_env,
                    timeout_s=20,
                )
            except Exception:
                pass

        if self.daemon_process is not None:
            deadline = time.time() + 5.0
            while time.time() < deadline:
                if self.daemon_process.poll() is not None:
                    break
                time.sleep(0.05)

            if self.daemon_process.poll() is None:
                self.daemon_process.kill()
                self.daemon_process.wait(timeout=5)
            self.daemon_process = None

        shutil.rmtree(self.home_dir, ignore_errors=True)

    def daemon_request(self, args: list[str]) -> dict[str, Any]:
        if self.variant.mode != "daemon":
            raise BenchmarkError(
                f"daemon_request called for non-daemon variant: {self.variant.key}"
            )
        proc = run_cmd(
            [str(self.variant.binary), "daemon", *args],
            cwd=self.run_root,
            env=self.base_env,
            timeout_s=30,
        )
        payload = (proc.stdout or "").strip()
        if not payload:
            raise BenchmarkError(
                f"Daemon command returned empty payload: {' '.join(args)}"
            )
        try:
            decoded = json.loads(payload)
        except json.JSONDecodeError as err:
            raise BenchmarkError(
                f"Failed to decode daemon response for {' '.join(args)}: {err}\n{payload}"
            ) from err
        if not isinstance(decoded, dict):
            raise BenchmarkError(
                f"Unexpected daemon response shape for {' '.join(args)}: {decoded!r}"
            )
        return decoded

    def wait_for_daemon_idle(self, repo_dir: Path) -> None:
        """Block until the daemon has finished applying side effects for this repo.

        The daemon processes attribution asynchronously after the foreground git
        command returns, so a real timing/validation pass MUST wait for it to go
        quiescent before reading notes. We poll ``daemon status`` and watch the
        family's ``latest_seq`` (the actor's applied-command counter): once it has
        advanced past zero and then held steady across several polls, every queued
        command -- and its synchronous ``refs/notes/ai`` write -- has been applied.

        This is the only sync primitive available on BOTH binaries under test:
        the removed ``daemon barrier`` subcommand and the ``sync.family`` control
        method do not exist on the main-branch daemon, but ``daemon status`` does.
        Each scenario runs in a freshly copied repo (a fresh daemon family that
        starts at seq 0), so ``latest_seq > 0`` reliably means "work was seen".
        """
        if self.variant.mode != "daemon":
            return

        repo = str(repo_dir)
        last_seq = -1
        stable_polls = 0
        required_stable_polls = 5
        deadline = time.monotonic() + DAEMON_IDLE_TIMEOUT_S
        while time.monotonic() < deadline:
            status = self.daemon_request(["status", "--repo", repo])
            if not bool(status.get("ok")):
                raise BenchmarkError(
                    f"Daemon status failed for {repo}: {status.get('error')}"
                )
            data = status.get("data")
            if not isinstance(data, dict):
                raise BenchmarkError(
                    f"Daemon status response missing data for {repo}: {status!r}"
                )
            last_error = data.get("last_error")
            if last_error:
                raise BenchmarkError(
                    f"Daemon reported a side-effect error for {repo}: {last_error}"
                )
            latest_seq = int(data.get("latest_seq") or 0)
            if latest_seq == last_seq:
                stable_polls += 1
                if latest_seq > 0 and stable_polls >= required_stable_polls:
                    return
            else:
                stable_polls = 0
                last_seq = latest_seq
            time.sleep(DAEMON_IDLE_POLL_S)

        raise BenchmarkError(
            f"Daemon did not go quiescent for repo {repo} "
            f"(latest_seq={last_seq}) within {DAEMON_IDLE_TIMEOUT_S}s"
        )

    def run_git(self, args: list[str], cwd: Path) -> subprocess.CompletedProcess[str]:
        if self.variant.mode == "wrapper":
            cmd = [str(self.git_wrapper), *args]
        else:
            cmd = [str(self.real_git), *args]
        return run_cmd(cmd, cwd=cwd, env=self.base_env, timeout_s=self.timeout_s)

    def run_git_ai(self, args: list[str], cwd: Path) -> subprocess.CompletedProcess[str]:
        return run_cmd(
            [str(self.variant.binary), *args],
            cwd=cwd,
            env=self.base_env,
            timeout_s=self.timeout_s,
        )

    def init_repo(self, repo_dir: Path) -> None:
        repo_dir.mkdir(parents=True, exist_ok=True)
        init = subprocess.run(
            [str(self.real_git), "init", "-q", "-b", "main"],
            cwd=str(repo_dir),
            env=self.base_env,
            text=True,
            capture_output=True,
            check=False,
        )
        if init.returncode != 0:
            self.run_git(["init", "-q"], cwd=repo_dir)
            self.run_git(["checkout", "-q", "-b", "main"], cwd=repo_dir)

        self.run_git(["config", "user.name", "Benchmark Bot"], cwd=repo_dir)
        self.run_git(["config", "user.email", "benchmark@git-ai.local"], cwd=repo_dir)


    def checkpoint_mock_ai(self, repo_dir: Path, files: list[str]) -> None:
        if not files:
            return
        self.run_git_ai(["checkpoint", "mock_ai", *files], cwd=repo_dir)

    def commit_has_ai_note(self, repo_dir: Path, rev: str) -> bool:
        """True if `rev` carries an authorship note under refs/notes/ai."""
        sha = self.run_git(["rev-parse", rev], cwd=repo_dir).stdout.strip()
        probe = run_cmd(
            [str(self.real_git), "notes", "--ref=ai", "show", sha],
            cwd=repo_dir,
            env=self.base_env,
            timeout_s=self.timeout_s,
            check=False,
        )
        return probe.returncode == 0

    def commit_stats(self, repo_dir: Path, rev: str) -> dict[str, Any]:
        """Parsed `git-ai stats <rev> --json` for `rev`."""
        proc = self.run_git_ai(["stats", rev, "--json"], cwd=repo_dir)
        payload = (proc.stdout or "").strip()
        try:
            decoded = json.loads(payload)
        except json.JSONDecodeError as err:
            raise BenchmarkError(
                f"Failed to decode `git-ai stats {rev} --json`: {err}\n{payload}"
            ) from err
        if not isinstance(decoded, dict):
            raise BenchmarkError(
                f"Unexpected `git-ai stats` shape for {rev}: {decoded!r}"
            )
        return decoded

    def assert_ai_note(self, repo_dir: Path, rev: str, *, context: str) -> None:
        """Assert `rev` has an ai note AND reports AI-attributed additions.

        Validates both that the daemon ran at all (note present) and that it
        attributed the AI-authored lines rather than silently dropping them
        (ai_additions > 0). Catches "daemon did nothing" and "daemon
        mis-attributed" without re-implementing the integration suite.
        """
        sha = self.run_git(["rev-parse", rev], cwd=repo_dir).stdout.strip()
        if not self.commit_has_ai_note(repo_dir, sha):
            raise BenchmarkError(
                f"[{self.variant.key}] {context}: commit {sha[:12]} has no "
                f"refs/notes/ai note after daemon sync"
            )
        stats = self.commit_stats(repo_dir, sha)
        ai_additions = int(stats.get("ai_additions") or 0)
        if ai_additions <= 0:
            raise BenchmarkError(
                f"[{self.variant.key}] {context}: commit {sha[:12]} reports "
                f"ai_additions={ai_additions} (expected > 0); stats={stats!r}"
            )


def seed_basic_repo(runner: VariantRunner, repo_dir: Path, file_count: int = 24) -> list[str]:
    runner.init_repo(repo_dir)
    files: list[str] = []
    for i in range(file_count):
        rel = f"bench/basic/file_{i:03d}.txt"
        write_seed_file(repo_dir / rel, 1000 + i, 70)
        files.append(rel)
    runner.run_git(["add", "-A"], cwd=repo_dir)
    runner.run_git(["commit", "-q", "-m", "seed basic"], cwd=repo_dir)
    return files


def seed_structured_repo(runner: VariantRunner, repo_dir: Path) -> dict[str, list[str]]:
    runner.init_repo(repo_dir)
    groups = {
        "main": [f"bench/main/main_{i:02d}.txt" for i in range(8)],
        "feature": [f"bench/feature/feature_{i:02d}.txt" for i in range(10)],
        "side": [f"bench/side/side_{i:02d}.txt" for i in range(6)],
    }
    seed = 2000
    for files in groups.values():
        for rel in files:
            write_seed_file(repo_dir / rel, seed, 80)
            seed += 1
    runner.run_git(["add", "-A"], cwd=repo_dir)
    runner.run_git(["commit", "-q", "-m", "seed structured"], cwd=repo_dir)
    return groups


def create_ai_commit(
    runner: VariantRunner,
    repo_dir: Path,
    rel_files: list[str],
    marker: str,
    message: str,
) -> None:
    for rel in rel_files:
        append_line(repo_dir / rel, marker)
    runner.checkpoint_mock_ai(repo_dir, rel_files)
    runner.run_git(["add", "-A"], cwd=repo_dir)
    runner.run_git(["commit", "-q", "-m", message], cwd=repo_dir)


def create_plain_commit(
    runner: VariantRunner,
    repo_dir: Path,
    rel_files: list[str],
    marker: str,
    message: str,
) -> None:
    for rel in rel_files:
        append_line(repo_dir / rel, marker)
    runner.run_git(["add", "-A"], cwd=repo_dir)
    runner.run_git(["commit", "-q", "-m", message], cwd=repo_dir)


def setup_human_commit(runner: VariantRunner, repo_dir: Path) -> None:
    seed_basic_repo(runner, repo_dir)


def measure_human_commit(runner: VariantRunner, repo_dir: Path, run_idx: int) -> None:
    files = [f"bench/basic/file_{i:03d}.txt" for i in range(6)]
    for idx, rel in enumerate(files):
        append_line(repo_dir / rel, f"human-change run={run_idx} idx={idx}")
    runner.run_git(["add", "-A"], cwd=repo_dir)
    runner.run_git(["commit", "-q", "-m", f"bench human run {run_idx}"], cwd=repo_dir)


def setup_ai_checkpoint_commit(runner: VariantRunner, repo_dir: Path) -> None:
    seed_basic_repo(runner, repo_dir)


def measure_ai_checkpoint_commit(runner: VariantRunner, repo_dir: Path, run_idx: int) -> None:
    files = [f"bench/basic/file_{i:03d}.txt" for i in range(5)]
    for idx, rel in enumerate(files):
        append_line(repo_dir / rel, f"ai-change run={run_idx} idx={idx}")
    runner.checkpoint_mock_ai(repo_dir, files)
    runner.run_git(["add", "-A"], cwd=repo_dir)
    runner.run_git(["commit", "-q", "-m", f"bench ai commit run {run_idx}"], cwd=repo_dir)


def setup_reset_mixed(runner: VariantRunner, repo_dir: Path) -> None:
    files = seed_basic_repo(runner, repo_dir)
    for i in range(12):
        target = files[i % len(files)]
        create_ai_commit(
            runner,
            repo_dir,
            [target],
            marker=f"history-ai-{i}",
            message=f"history ai commit {i}",
        )


def measure_reset_mixed(runner: VariantRunner, repo_dir: Path, run_idx: int) -> None:
    for i in range(5):
        append_line(repo_dir / f"bench/basic/file_{i:03d}.txt", f"pending-reset-{run_idx}-{i}")
    runner.run_git(["reset", "--mixed", "HEAD~6"], cwd=repo_dir)


def setup_stash_roundtrip(runner: VariantRunner, repo_dir: Path) -> None:
    files = seed_basic_repo(runner, repo_dir)
    create_ai_commit(
        runner,
        repo_dir,
        files[:3],
        marker="seed-ai-stash",
        message="seed ai for stash",
    )


def measure_stash_roundtrip(runner: VariantRunner, repo_dir: Path, run_idx: int) -> None:
    tracked = [f"bench/basic/file_{i:03d}.txt" for i in range(4, 9)]
    for idx, rel in enumerate(tracked):
        append_line(repo_dir / rel, f"stash-tracked-{run_idx}-{idx}")
    runner.checkpoint_mock_ai(repo_dir, tracked[:3])

    untracked = repo_dir / "bench" / f"untracked_{run_idx}.txt"
    write_seed_file(untracked, 7000 + run_idx, 20)

    runner.run_git(["stash", "push", "-u", "-m", f"bench stash {run_idx}"], cwd=repo_dir)
    runner.run_git(["stash", "pop"], cwd=repo_dir)


def setup_cherry_pick_three(runner: VariantRunner, repo_dir: Path) -> None:
    files = seed_basic_repo(runner, repo_dir)
    runner.run_git(["checkout", "-q", "-b", "feature"], cwd=repo_dir)
    for i in range(3):
        create_ai_commit(
            runner,
            repo_dir,
            [files[i]],
            marker=f"feature-cherry-{i}",
            message=f"feature cherry commit {i}",
        )
        runner.run_git(["tag", f"bench-cherry-{i}", "HEAD"], cwd=repo_dir)
    create_plain_commit(
        runner,
        repo_dir,
        [files[10]],
        marker="feature-extra",
        message="feature extra commit",
    )
    runner.run_git(["checkout", "-q", "main"], cwd=repo_dir)
    create_plain_commit(
        runner,
        repo_dir,
        [files[20]],
        marker="main-diverge",
        message="main diverge commit",
    )


def measure_cherry_pick_three(runner: VariantRunner, repo_dir: Path, run_idx: int) -> None:
    commit_ids = [
        runner.run_git(["rev-parse", f"bench-cherry-{i}"], cwd=repo_dir).stdout.strip()
        for i in range(3)
    ]
    if len(commit_ids) != 3:
        raise BenchmarkError("Expected exactly 3 feature commits for cherry-pick scenario")
    runner.run_git(["cherry-pick", *commit_ids], cwd=repo_dir)


def setup_rebase_linear(runner: VariantRunner, repo_dir: Path) -> None:
    groups = seed_structured_repo(runner, repo_dir)
    for i in range(4):
        create_plain_commit(
            runner,
            repo_dir,
            [groups["main"][i % len(groups["main"])]],
            marker=f"main-pre-feature-{i}",
            message=f"main pre feature {i}",
        )

    runner.run_git(["checkout", "-q", "-b", "feature", "main~3"], cwd=repo_dir)
    for i in range(8):
        create_ai_commit(
            runner,
            repo_dir,
            [groups["feature"][i % len(groups["feature"])]],
            marker=f"feature-linear-{i}",
            message=f"feature linear {i}",
        )

    runner.run_git(["checkout", "-q", "main"], cwd=repo_dir)
    for i in range(6):
        create_plain_commit(
            runner,
            repo_dir,
            [groups["main"][(i + 4) % len(groups["main"])]],
            marker=f"main-after-feature-{i}",
            message=f"main after feature {i}",
        )
    runner.run_git(["checkout", "-q", "feature"], cwd=repo_dir)


def measure_rebase_linear(runner: VariantRunner, repo_dir: Path, run_idx: int) -> None:
    runner.run_git(["rebase", "main"], cwd=repo_dir)


def setup_rebase_merges(runner: VariantRunner, repo_dir: Path) -> None:
    groups = seed_structured_repo(runner, repo_dir)
    for i in range(5):
        create_plain_commit(
            runner,
            repo_dir,
            [groups["main"][i % len(groups["main"])]],
            marker=f"main-start-{i}",
            message=f"main start {i}",
        )

    runner.run_git(["checkout", "-q", "-b", "feature", "main~2"], cwd=repo_dir)
    for i in range(6):
        create_ai_commit(
            runner,
            repo_dir,
            [groups["feature"][i % len(groups["feature"])]],
            marker=f"feature-rm-{i}",
            message=f"feature rm {i}",
        )

    runner.run_git(["checkout", "-q", "-b", "side", "feature~3"], cwd=repo_dir)
    for i in range(4):
        create_ai_commit(
            runner,
            repo_dir,
            [groups["side"][i % len(groups["side"])]],
            marker=f"side-rm-{i}",
            message=f"side rm {i}",
        )

    runner.run_git(["checkout", "-q", "feature"], cwd=repo_dir)
    runner.run_git(["merge", "--no-ff", "-q", "-m", "merge side", "side"], cwd=repo_dir)
    for i in range(2):
        create_ai_commit(
            runner,
            repo_dir,
            [groups["feature"][(i + 6) % len(groups["feature"])]],
            marker=f"feature-post-merge-{i}",
            message=f"feature post merge {i}",
        )

    runner.run_git(["checkout", "-q", "main"], cwd=repo_dir)
    for i in range(4):
        create_plain_commit(
            runner,
            repo_dir,
            [groups["main"][(i + 5) % len(groups["main"])]],
            marker=f"main-upstream-{i}",
            message=f"main upstream {i}",
        )
    runner.run_git(["checkout", "-q", "feature"], cwd=repo_dir)


def measure_rebase_merges(runner: VariantRunner, repo_dir: Path, run_idx: int) -> None:
    runner.run_git(["rebase", "--rebase-merges", "main"], cwd=repo_dir)


def setup_squash_merge(runner: VariantRunner, repo_dir: Path) -> None:
    groups = seed_structured_repo(runner, repo_dir)
    runner.run_git(["checkout", "-q", "-b", "feature"], cwd=repo_dir)
    for i in range(10):
        create_ai_commit(
            runner,
            repo_dir,
            [groups["feature"][i % len(groups["feature"])]],
            marker=f"squash-feature-{i}",
            message=f"squash feature {i}",
        )

    runner.run_git(["checkout", "-q", "main"], cwd=repo_dir)
    for i in range(4):
        create_plain_commit(
            runner,
            repo_dir,
            [groups["main"][i % len(groups["main"])]],
            marker=f"squash-main-{i}",
            message=f"squash main {i}",
        )


def measure_squash_merge(runner: VariantRunner, repo_dir: Path, run_idx: int) -> None:
    runner.run_git(["merge", "--squash", "feature"], cwd=repo_dir)
    runner.run_git(["commit", "-q", "-m", f"squash merge run {run_idx}"], cwd=repo_dir)


def validate_head_is_ai(runner: VariantRunner, repo_dir: Path) -> None:
    """Assert HEAD is an AI-attributed commit after the daemon has synced.

    Used by scenarios whose measure step leaves an AI-authored commit at HEAD
    (fresh AI commit, cherry-pick, rebase, squash-merge). Confirms the daemon
    both ran (note present) and attributed the AI lines (ai_additions > 0).
    """
    runner.assert_ai_note(repo_dir, "HEAD", context="HEAD after measure")


SCENARIOS = [
    Scenario(
        key="commit_human",
        description="Human-only add/commit on modified tracked files",
        complexity="basic",
        setup=setup_human_commit,
        measure=measure_human_commit,
        # Human-only: no AI attribution to validate.
        validate=None,
    ),
    Scenario(
        key="checkpoint_commit_ai",
        description="AI checkpoint + commit flow",
        complexity="basic",
        setup=setup_ai_checkpoint_commit,
        measure=measure_ai_checkpoint_commit,
        validate=validate_head_is_ai,
    ),
    Scenario(
        key="reset_mixed_head6",
        description="Reset mixed with pending worktree edits",
        complexity="basic",
        setup=setup_reset_mixed,
        measure=measure_reset_mixed,
        # `reset --mixed HEAD~6` lands HEAD on an AI commit created in setup;
        # validating it confirms the note survived the ref move.
        validate=validate_head_is_ai,
    ),
    Scenario(
        key="stash_roundtrip",
        description="stash push -u + pop on AI-touched and untracked files",
        complexity="basic",
        setup=setup_stash_roundtrip,
        measure=measure_stash_roundtrip,
        # stash push/pop creates no commit; HEAD stays the setup AI commit.
        validate=validate_head_is_ai,
    ),
    Scenario(
        key="cherry_pick_three",
        description="Cherry-pick three AI commits onto diverged main",
        complexity="basic",
        setup=setup_cherry_pick_three,
        measure=measure_cherry_pick_three,
        validate=validate_head_is_ai,
    ),
    Scenario(
        key="rebase_linear",
        description="Linear feature branch rebase onto updated main",
        complexity="complex",
        setup=setup_rebase_linear,
        measure=measure_rebase_linear,
        validate=validate_head_is_ai,
    ),
    Scenario(
        key="rebase_rebase_merges",
        description="Rebase-merges on branch with merge commit",
        complexity="complex",
        setup=setup_rebase_merges,
        measure=measure_rebase_merges,
        validate=validate_head_is_ai,
    ),
    Scenario(
        key="squash_merge_commit",
        description="merge --squash + commit from feature branch",
        complexity="complex",
        setup=setup_squash_merge,
        measure=measure_squash_merge,
        validate=validate_head_is_ai,
    ),
]


def summarize_runs(results: list[RunResult]) -> dict[str, dict[str, dict[str, float | list[float]]]]:
    grouped: dict[str, dict[str, list[float]]] = {}
    for item in results:
        grouped.setdefault(item.scenario, {}).setdefault(item.variant, []).append(item.duration_ms)

    summary: dict[str, dict[str, dict[str, float | list[float]]]] = {}
    for scenario, by_variant in grouped.items():
        scenario_summary: dict[str, dict[str, float | list[float]]] = {}
        for variant, samples in by_variant.items():
            ordered = sorted(samples)
            median = statistics.median(ordered)
            mean = statistics.mean(ordered)
            stdev = statistics.pstdev(ordered) if len(ordered) > 1 else 0.0
            scenario_summary[variant] = {
                "runs_ms": [round(x, 3) for x in samples],
                "median_ms": round(median, 3),
                "mean_ms": round(mean, 3),
                "min_ms": round(min(ordered), 3),
                "max_ms": round(max(ordered), 3),
                "stdev_ms": round(stdev, 3),
            }
        summary[scenario] = scenario_summary
    return summary


def compute_slowdowns(
    summary: dict[str, dict[str, dict[str, float | list[float]]]],
    baseline_key: str,
) -> dict[str, dict[str, float]]:
    slowdowns: dict[str, dict[str, float]] = {}
    for scenario, by_variant in summary.items():
        if baseline_key not in by_variant:
            continue
        baseline = float(by_variant[baseline_key]["median_ms"])  # type: ignore[index]
        if baseline <= 0:
            continue
        scenario_slowdown: dict[str, float] = {}
        for variant, stats in by_variant.items():
            if variant == baseline_key:
                continue
            med = float(stats["median_ms"])  # type: ignore[index]
            scenario_slowdown[variant] = round(((med - baseline) / baseline) * 100.0, 3)
        slowdowns[scenario] = scenario_slowdown
    return slowdowns


def compute_margin_checks(
    summary: dict[str, dict[str, dict[str, float | list[float]]]],
    *,
    baseline_key: str,
    margin_pct: float,
    variants: list[str],
) -> list[MarginCheckResult]:
    checks: list[MarginCheckResult] = []
    multiplier = 1.0 + (margin_pct / 100.0)
    for scenario, by_variant in summary.items():
        if baseline_key not in by_variant:
            continue
        baseline = float(by_variant[baseline_key]["median_ms"])  # type: ignore[index]
        if baseline <= 0.0:
            continue
        allowed = baseline * multiplier
        for variant in variants:
            if variant not in by_variant:
                continue
            median = float(by_variant[variant]["median_ms"])  # type: ignore[index]
            slowdown = ((median - baseline) / baseline) * 100.0
            checks.append(
                MarginCheckResult(
                    scenario=scenario,
                    variant=variant,
                    baseline_ms=round(baseline, 3),
                    median_ms=round(median, 3),
                    allowed_ms=round(allowed, 3),
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
    metadata: dict[str, str | int | dict[str, str]],
    scenarios: list[Scenario],
    variants: list[Variant],
    summary: dict[str, dict[str, dict[str, float | list[float]]]],
    slowdowns: dict[str, dict[str, float]],
    margin_checks: list[MarginCheckResult],
) -> None:
    baseline_key = "main_daemon"
    margin_baseline_key = str(metadata["margin_baseline"])
    margin_baseline_label = margin_baseline_key.replace("_", " ")

    lines: list[str] = []
    lines.append("# git-ai Mode Benchmark Report")
    lines.append("")
    lines.append("## Run Metadata")
    lines.append("")
    lines.append(f"- Timestamp (UTC): `{metadata['timestamp_utc']}`")
    lines.append(f"- Repo: `{metadata['repo_root']}`")
    lines.append(f"- Branch: `{metadata['branch']}`")
    lines.append(f"- Branch SHA: `{metadata['branch_sha']}`")
    lines.append(f"- Main Ref: `{metadata['main_ref']}`")
    lines.append(f"- Main SHA: `{metadata['main_sha']}`")
    lines.append(f"- Real git: `{metadata['real_git']}`")
    lines.append(f"- Iterations (basic): `{metadata['iterations_basic']}`")
    lines.append(f"- Iterations (complex): `{metadata['iterations_complex']}`")
    lines.append("")
    lines.append("## Variants")
    lines.append("")
    for variant in variants:
        lines.append(f"- `{variant.key}`: {variant.label} (`{variant.binary}`)")
    lines.append("")
    lines.append("## Scenario Matrix")
    lines.append("")
    for scenario in scenarios:
        lines.append(f"- `{scenario.key}` ({scenario.complexity}): {scenario.description}")
    lines.append("")
    lines.append("## Exact Timings (ms)")
    lines.append("")
    lines.append(
        "| Scenario | main(daemon) runs | current(daemon) runs |"
    )
    lines.append("|---|---:|---:|")
    for scenario in scenarios:
        row = [scenario.key]
        for key in [
            "main_daemon",
            "current_daemon",
        ]:
            runs = summary[scenario.key][key]["runs_ms"]  # type: ignore[index]
            row.append(", ".join(f"{float(v):.3f}" for v in runs))  # type: ignore[arg-type]
        lines.append(
            f"| {row[0]} | {row[1]} | {row[2]} |"
        )
    lines.append("")
    lines.append("## Median Summary (ms) and Slowdown vs main(daemon)")
    lines.append("")
    lines.append(
        "| Scenario | main(daemon) | current(daemon) | daemon Δ% |"
    )
    lines.append("|---|---:|---:|---:|")
    for scenario in scenarios:
        data = summary[scenario.key]
        base = float(data["main_daemon"]["median_ms"])  # type: ignore[index]
        cd = float(data["current_daemon"]["median_ms"])  # type: ignore[index]
        s = slowdowns.get(scenario.key, {})
        lines.append(
            f"| {scenario.key} | {base:.3f} | {cd:.3f} | "
            f"{s.get('current_daemon', 0.0):.3f}% |"
        )

    ratios: dict[str, list[float]] = {
        "current_daemon": [],
    }
    for scenario in scenarios:
        data = summary[scenario.key]
        base = float(data[baseline_key]["median_ms"])  # type: ignore[index]
        for key in ratios:
            med = float(data[key]["median_ms"])  # type: ignore[index]
            ratios[key].append(med / base if base > 0 else 1.0)

    lines.append("")
    lines.append("## Aggregate Comparison")
    lines.append("")
    lines.append("| Variant | Geometric Mean Ratio vs main(daemon) | Geometric Mean Slowdown |")
    lines.append("|---|---:|---:|")
    for key, ratio_values in ratios.items():
        gm = geometric_mean(ratio_values)
        lines.append(f"| {key} | {gm:.4f}x | {(gm - 1.0) * 100.0:.3f}% |")

    lines.append("")
    lines.append("## Margin Check")
    lines.append("")
    lines.append(
        f"- Required margin: current modes must be <= `{float(metadata['margin_pct']):.1f}%` slower than `{margin_baseline_label}`"
    )
    lines.append(
        "| Scenario | Variant | Baseline (ms) | Variant Median (ms) | Allowed Max (ms) | Slowdown | Status |"
    )
    lines.append("|---|---|---:|---:|---:|---:|---|")
    for check in sorted(margin_checks, key=lambda c: (c.scenario, c.variant)):
        status = "PASS" if check.passed else "FAIL"
        lines.append(
            f"| {check.scenario} | {check.variant} | {check.baseline_ms:.3f} | "
            f"{check.median_ms:.3f} | {check.allowed_ms:.3f} | {check.slowdown_pct:.3f}% | {status} |"
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
        "python3 scripts/benchmarks/git/benchmark_modes_vs_main.py --iterations-basic "
        f"{metadata['iterations_basic']} --iterations-complex {metadata['iterations_complex']} "
        f"--margin-pct {float(metadata['margin_pct']):.1f} "
        f"--margin-baseline {metadata['margin_baseline']}"
    )
    lines.append("```")

    report_path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def write_raw_csv(path: Path, results: list[RunResult]) -> None:
    with path.open("w", encoding="utf-8", newline="") as fh:
        writer = csv.writer(fh)
        writer.writerow(["scenario", "complexity", "variant", "run_index", "duration_ms"])
        for item in results:
            writer.writerow(
                [
                    item.scenario,
                    item.complexity,
                    item.variant,
                    item.run_index,
                    f"{item.duration_ms:.3f}",
                ]
            )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Benchmark main(daemon) against current(daemon) "
            "across basic and complex git workflows."
        )
    )
    parser.add_argument(
        "--work-root",
        type=Path,
        default=None,
        help="Artifact working directory (defaults to a temp dir).",
    )
    parser.add_argument(
        "--main-ref",
        default="origin/main",
        help="Git ref used for baseline main build (default: origin/main).",
    )
    parser.add_argument(
        "--iterations-basic",
        type=int,
        default=3,
        help="Iterations per basic scenario per variant (default: 3).",
    )
    parser.add_argument(
        "--iterations-complex",
        type=int,
        default=3,
        help="Iterations per complex scenario per variant (default: 3).",
    )
    parser.add_argument(
        "--keep-artifacts",
        action="store_true",
        help="Keep template and run repositories under work-root.",
    )
    parser.add_argument(
        "--current-bin",
        type=Path,
        default=None,
        help="Use an existing current-branch git-ai binary (skip current build).",
    )
    parser.add_argument(
        "--main-bin",
        type=Path,
        default=None,
        help="Use an existing main-branch git-ai binary (skip main build/worktree).",
    )
    parser.add_argument(
        "--margin-pct",
        type=float,
        default=25.0,
        help="Maximum allowed slowdown percentage relative to --margin-baseline.",
    )
    parser.add_argument(
        "--enforce-margin",
        action="store_true",
        help="Exit non-zero when any current_daemon margin check fails.",
    )
    parser.add_argument(
        "--margin-baseline",
        type=str,
        choices=["main_daemon"],
        default="main_daemon",
        help="Baseline variant for margin checks.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    repo_root = Path(__file__).resolve().parents[3]

    if args.iterations_basic <= 0 or args.iterations_complex <= 0:
        raise BenchmarkError("Iterations must be positive integers.")
    if args.margin_pct < 0:
        raise BenchmarkError("--margin-pct must be non-negative")

    if args.work_root is None:
        work_root = Path(tempfile.mkdtemp(prefix="git-ai-modes-bench-"))
    else:
        work_root = args.work_root.resolve()
        work_root.mkdir(parents=True, exist_ok=True)

    real_git = resolve_real_git_binary(repo_root)

    build_dir = work_root / "build"
    build_dir.mkdir(parents=True, exist_ok=True)
    targets_dir = build_dir / "targets"
    targets_dir.mkdir(parents=True, exist_ok=True)

    main_worktree = build_dir / "main-worktree"
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
            main_bin = build_release_binary(main_worktree, targets_dir / "main")
            main_sha = git_output(main_worktree, ["rev-parse", "HEAD"])

        # git-ai is daemon-architecture: all attribution side effects run in the
        # daemon (the git proxy is a thin trace2-emitting passthrough). Wrapper
        # mode with no daemon captures nothing, so the only meaningful comparison
        # is daemon-vs-daemon: this branch's daemon vs main's daemon.
        variants = [
            Variant("main_daemon", "main(daemon)", main_bin, "daemon"),
            Variant("current_daemon", "current(daemon)", current_bin, "daemon"),
        ]

        timestamp = time.strftime("%Y%m%d-%H%M%S", time.localtime())
        artifacts_dir = work_root / "artifacts" / timestamp
        artifacts_dir.mkdir(parents=True, exist_ok=True)

        raw_results: list[RunResult] = []
        templates_root = work_root / "templates"
        runs_root = work_root / "runs"
        templates_root.mkdir(parents=True, exist_ok=True)
        runs_root.mkdir(parents=True, exist_ok=True)

        for scenario in SCENARIOS:
            iterations = (
                args.iterations_basic
                if scenario.complexity == "basic"
                else args.iterations_complex
            )

            for variant in variants:
                scenario_variant_root = templates_root / scenario.key / variant.key
                if scenario_variant_root.exists():
                    shutil.rmtree(scenario_variant_root)
                scenario_variant_root.mkdir(parents=True, exist_ok=True)

                runner = VariantRunner(variant, scenario_variant_root, real_git)
                try:
                    template_repo = scenario_variant_root / "repo-template"
                    print(f"[setup] scenario={scenario.key} variant={variant.key}")
                    scenario.setup(runner, template_repo)
                    runner.wait_for_daemon_idle(template_repo)

                    for run_index in range(1, iterations + 1):
                        run_dir = runs_root / scenario.key / variant.key / f"run_{run_index:02d}"
                        if run_dir.exists():
                            shutil.rmtree(run_dir)
                        run_repo = run_dir / "repo"
                        run_repo.parent.mkdir(parents=True, exist_ok=True)
                        shutil.copytree(
                            template_repo,
                            run_repo,
                            ignore=ignore_transient_git_lockfiles,
                        )
                        # The daemon does attribution asynchronously, AFTER the
                        # foreground git command returns. To measure the real
                        # end-to-end cost (foreground passthrough + daemon
                        # attribution) the quiescence wait MUST be inside the
                        # timed window -- otherwise we only time the passthrough,
                        # which is near-identical between two daemon builds.
                        t0 = time.perf_counter()
                        scenario.measure(runner, run_repo, run_index)
                        runner.wait_for_daemon_idle(run_repo)
                        duration_ms = (time.perf_counter() - t0) * 1000.0

                        # Correctness gate (outside the timed window): confirm the
                        # daemon actually produced the expected authorship notes,
                        # so a build that is "fast" because it silently dropped
                        # attribution cannot pass.
                        if scenario.validate is not None:
                            scenario.validate(runner, run_repo)
                        raw_results.append(
                            RunResult(
                                scenario=scenario.key,
                                complexity=scenario.complexity,
                                variant=variant.key,
                                run_index=run_index,
                                duration_ms=duration_ms,
                            )
                        )
                        print(
                            f"[run] scenario={scenario.key} variant={variant.key} "
                            f"run={run_index}/{iterations} duration_ms={duration_ms:.3f}"
                        )

                        if not args.keep_artifacts and run_dir.exists():
                            shutil.rmtree(run_dir)
                finally:
                    runner.close()

        summary = summarize_runs(raw_results)
        slowdowns = compute_slowdowns(summary, baseline_key="main_daemon")
        margin_checks = compute_margin_checks(
            summary,
            baseline_key=args.margin_baseline,
            margin_pct=args.margin_pct,
            variants=["current_daemon"],
        )

        metadata: dict[str, str | int | dict[str, str]] = {
            "timestamp_utc": now_iso_utc(),
            "repo_root": str(repo_root),
            "branch": git_output(repo_root, ["rev-parse", "--abbrev-ref", "HEAD"]),
            "branch_sha": git_output(repo_root, ["rev-parse", "HEAD"]),
            "main_ref": args.main_ref,
            "main_sha": main_sha,
            "real_git": str(real_git),
            "iterations_basic": args.iterations_basic,
            "iterations_complex": args.iterations_complex,
            "margin_pct": args.margin_pct,
            "margin_baseline": args.margin_baseline,
            "variants": {v.key: str(v.binary) for v in variants},
        }

        csv_path = artifacts_dir / "raw_results.csv"
        json_path = artifacts_dir / "summary.json"
        md_path = artifacts_dir / "report.md"
        write_raw_csv(csv_path, raw_results)
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
        render_report(md_path, metadata, SCENARIOS, variants, summary, slowdowns, margin_checks)

        print("")
        print("Benchmark complete")
        print(f"- Report: {md_path}")
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
