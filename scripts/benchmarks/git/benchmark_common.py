"""Shared support code for the Git performance benchmark harnesses."""

from __future__ import annotations

import os
import shutil
import subprocess
import time
from pathlib import Path

DEFAULT_COMMAND_TIMEOUT_S = 900
LONG_COMMAND_TIMEOUT_S = 5400


class BenchmarkError(RuntimeError):
    """Raised when benchmark setup, execution, or validation cannot continue."""


def now_iso_utc() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


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


def run_cmd(
    cmd: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    timeout_s: int = DEFAULT_COMMAND_TIMEOUT_S,
    check: bool = True,
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
    if check and proc.returncode != 0:
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
    binary_name = "git-ai.exe" if os.name == "nt" else "git-ai"
    binary = target_dir / "release" / binary_name
    if not binary.exists():
        raise BenchmarkError(f"Expected binary not found: {binary}")
    return binary


def git_output(repo_dir: Path, args: list[str]) -> str:
    proc = run_cmd(["git", *args], cwd=repo_dir, env=dict(os.environ), timeout_s=120)
    return (proc.stdout or "").strip()


def prepare_main_worktree(
    repo_root: Path,
    main_ref: str,
    worktree_dir: Path,
    *,
    timeout_s: int = 900,
) -> None:
    worktree_dir.parent.mkdir(parents=True, exist_ok=True)
    if worktree_dir.exists():
        shutil.rmtree(worktree_dir)
    run_cmd(
        ["git", "fetch", "--quiet", "origin", "main"],
        cwd=repo_root,
        env=dict(os.environ),
        timeout_s=timeout_s,
    )
    run_cmd(
        ["git", "worktree", "add", "--detach", str(worktree_dir), main_ref],
        cwd=repo_root,
        env=dict(os.environ),
        timeout_s=timeout_s,
    )


def remove_main_worktree(
    repo_root: Path,
    worktree_dir: Path,
    *,
    timeout_s: int = 900,
) -> None:
    run_cmd(
        ["git", "worktree", "remove", "--force", str(worktree_dir)],
        cwd=repo_root,
        env=dict(os.environ),
        timeout_s=timeout_s,
    )


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
