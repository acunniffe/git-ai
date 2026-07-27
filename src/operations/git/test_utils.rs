//! Test-only helpers for in-tree unit tests, integration tests, and benchmarks.
//!
//! Gated on `#[cfg(any(test, feature = "test-support"))]` so this module is
//! available to:
//!   - in-tree `#[cfg(test)]` modules in the library
//!   - integration tests that depend on `git-ai` with `features = ["test-support"]`
//!   - benchmarks (same — `[dev-dependencies] git-ai = { features = ["test-support"] }`)
//!
//! All helpers shell out to the real `git` binary; the historical `git2`-based
//! variants were retired alongside the libgit2 dependency removal.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use tempfile::TempDir;

use crate::error::GitAiError;
use crate::operations::git::repository::{Repository, find_repository_in_path};

/// Process-wide one-shot init for the dummy git author/committer identity.
///
/// Sets `GIT_AUTHOR_*` and `GIT_COMMITTER_*` env vars so that `git commit`
/// invocations made by tests and benches succeed without depending on the
/// developer's `~/.gitconfig`.
pub fn init_test_git_config() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // SAFETY: test-only env-var init at startup, before any test threads
        // observe these variables.
        unsafe {
            std::env::set_var("GIT_AUTHOR_NAME", "git-ai test");
            std::env::set_var("GIT_AUTHOR_EMAIL", "test@git-ai.local");
            std::env::set_var("GIT_COMMITTER_NAME", "git-ai test");
            std::env::set_var("GIT_COMMITTER_EMAIL", "test@git-ai.local");
        }
    });
}

/// A temporary git repository for tests/benches.
///
/// The on-disk repo is deleted when the `TmpRepo` is dropped.
pub struct TmpRepo {
    _tmp: TempDir,
    path: PathBuf,
    repo: Repository,
}

impl TmpRepo {
    /// Initialise a new temporary git repo with a default `main` branch and a
    /// deterministic committer identity.
    pub fn new() -> Result<Self, GitAiError> {
        init_test_git_config();

        let tmp = tempfile::tempdir()?;
        let path = tmp.path().to_path_buf();

        run_git_in(&path, &["init", "-b", "main", "."])?;

        // Per-repo config so that even if the env vars are scrubbed by a child
        // process, `git commit` still succeeds.
        for (k, v) in [
            ("user.email", "test@git-ai.local"),
            ("user.name", "git-ai test"),
            ("commit.gpgsign", "false"),
        ] {
            run_git_in(&path, &["config", k, v])?;
        }

        let path_str = path
            .to_str()
            .ok_or_else(|| GitAiError::Generic("TmpRepo path is not utf-8".to_string()))?;
        let repo = find_repository_in_path(path_str)?;

        Ok(Self {
            _tmp: tmp,
            path,
            repo,
        })
    }

    /// Working-tree path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow the wrapped `git_ai` `Repository`.
    pub fn gitai_repo(&self) -> &Repository {
        &self.repo
    }

    /// Write a file relative to the repo working directory and return its path.
    ///
    /// The third parameter is retained for backward compatibility with the prior
    /// `git2`-based helper signature; staging is now the caller's responsibility
    /// (use `git_command(&["add", "-A"])` or `commit_all`).
    pub fn write_file(
        &self,
        name: &str,
        contents: &str,
        _legacy_stage_flag: bool,
    ) -> Result<PathBuf, GitAiError> {
        let p = self.path.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, contents)?;
        Ok(p)
    }

    /// Run an arbitrary git subcommand against this repo and return stdout.
    pub fn git_command(&self, args: &[&str]) -> Result<String, GitAiError> {
        run_git_in(&self.path, args)
    }

    /// Stage all changes and create a commit. Returns the resulting commit SHA.
    pub fn commit_all(&self, message: &str) -> Result<String, GitAiError> {
        self.git_command(&["add", "-A"])?;
        self.git_command(&["commit", "--allow-empty", "-m", message])?;
        Ok(self.git_command(&["rev-parse", "HEAD"])?.trim().to_string())
    }

    /// Bench-API alias for `commit_all`.
    pub fn commit_with_message(&self, message: &str) -> Result<String, GitAiError> {
        self.commit_all(message)
    }

    /// Create a new branch (does not switch to it).
    pub fn create_branch(&self, name: &str) -> Result<(), GitAiError> {
        self.git_command(&["branch", name])?;
        Ok(())
    }

    /// Switch to an existing branch.
    pub fn switch_branch(&self, name: &str) -> Result<(), GitAiError> {
        self.git_command(&["switch", name])?;
        Ok(())
    }

    /// Rebase the current branch onto `onto`. The `feature_branch` argument is
    /// kept in the signature for backward-compatibility with the historical
    /// helper but is unused — the rebase always operates on the current branch.
    pub fn rebase_onto(&self, _feature_branch: &str, onto: &str) -> Result<(), GitAiError> {
        self.git_command(&["rebase", onto])?;
        Ok(())
    }

    /// Bench-only no-op stub; preserved for API compatibility with callers that
    /// previously triggered a `git-ai checkpoint` here. Benchmarks that need
    /// real attribution data should pre-write notes via `notes_add`.
    pub fn trigger_checkpoint_with_author(&self, _author: &str) -> Result<(), GitAiError> {
        Ok(())
    }
}

fn run_git_in(cwd: &Path, args: &[&str]) -> Result<String, GitAiError> {
    let output = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
    if !output.status.success() {
        return Err(GitAiError::GitCliError {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            args: args.iter().map(|s| s.to_string()).collect(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Create a minimal *valid* directory-form git dir fixture: the directory
/// itself plus a `HEAD` file, which repository discovery requires before
/// treating a `.git` directory as a repository boundary. For synthetic
/// fixtures that don't run `git init`.
pub fn seed_valid_git_dir(git_dir: &Path) {
    std::fs::create_dir_all(git_dir).unwrap();
    let head = git_dir.join("HEAD");
    if !head.exists() {
        std::fs::write(head, "ref: refs/heads/main\n").unwrap();
    }
}
