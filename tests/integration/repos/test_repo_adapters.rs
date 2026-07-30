use std::cell::Cell;
use std::ops::Deref;
use std::path::Path;

use super::test_repo::TestRepo;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepoInvocation {
    Root,
    Subdirectory,
    CFlag,
}

thread_local! {
    static REPO_INVOCATION: Cell<RepoInvocation> = const { Cell::new(RepoInvocation::Subdirectory) };
}

#[allow(dead_code)]
pub(crate) fn with_repo_invocation<F, R>(invocation: RepoInvocation, f: F) -> R
where
    F: FnOnce() -> R,
{
    REPO_INVOCATION.with(|current| {
        let previous = current.replace(invocation);

        struct Reset<'a> {
            current: &'a Cell<RepoInvocation>,
            previous: RepoInvocation,
        }

        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.current.set(self.previous);
            }
        }

        let _reset = Reset { current, previous };
        f()
    })
}

/// TestRepo facade that routes repository-sensitive commands through one of the
/// invocation modes exercised by the rewrite integration scenarios.
#[allow(dead_code)]
pub(crate) struct InvocationTestRepo {
    inner: TestRepo,
    invocation: RepoInvocation,
}

#[allow(dead_code)]
impl InvocationTestRepo {
    pub(crate) fn new() -> Self {
        let invocation = REPO_INVOCATION.with(Cell::get);
        Self {
            inner: TestRepo::new(),
            invocation,
        }
    }

    pub(crate) fn git_from_working_dir(
        &self,
        working_dir: &Path,
        args: &[&str],
    ) -> Result<String, String> {
        match self.invocation {
            RepoInvocation::Root => self.inner.git_from_working_dir(self.inner.path(), args),
            RepoInvocation::Subdirectory => self.inner.git_from_working_dir(working_dir, args),
            RepoInvocation::CFlag => {
                self.inner
                    .git_with_env_using_c_flag_from(&std::env::temp_dir(), args, &[])
            }
        }
    }

    pub(crate) fn git_with_env(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
        working_dir: Option<&Path>,
    ) -> Result<String, String> {
        match (self.invocation, working_dir) {
            (RepoInvocation::Root, Some(_)) => {
                self.inner.git_with_env(args, envs, Some(self.inner.path()))
            }
            (RepoInvocation::Subdirectory, Some(working_dir)) => {
                self.inner.git_with_env(args, envs, Some(working_dir))
            }
            (RepoInvocation::CFlag, Some(_)) => {
                self.inner
                    .git_with_env_using_c_flag_from(&std::env::temp_dir(), args, envs)
            }
            (_, None) => self.inner.git_with_env(args, envs, None),
        }
    }
}

impl Deref for InvocationTestRepo {
    type Target = TestRepo;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[allow(dead_code)]
pub(crate) struct WorktreeTestRepo {
    inner: TestRepo,
}

#[allow(dead_code)]
impl WorktreeTestRepo {
    pub(crate) fn new() -> Self {
        Self {
            inner: TestRepo::new_worktree(),
        }
    }

    pub(crate) fn new_with_remote() -> (Self, Self) {
        let (local, upstream) = TestRepo::new_with_remote();
        (Self { inner: local }, Self { inner: upstream })
    }
}

impl Deref for WorktreeTestRepo {
    type Target = TestRepo;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_flag_adapter_preserves_success_stdout_and_stderr() {
        with_repo_invocation(RepoInvocation::CFlag, || {
            let repo = InvocationTestRepo::new();
            repo.git(&[
                "config",
                "alias.mixed-output",
                "!echo stdout-token && echo stderr-token >&2",
            ])
            .unwrap();

            let output = repo
                .git_from_working_dir(repo.path().as_path(), &["mixed-output"])
                .unwrap();

            assert!(output.contains("stdout-token"), "{output:?}");
            assert!(output.contains("stderr-token"), "{output:?}");
        });
    }

    #[test]
    fn c_flag_adapter_uses_repo_root_from_unrelated_process_cwd() {
        with_repo_invocation(RepoInvocation::CFlag, || {
            let repo = InvocationTestRepo::new();
            let unrelated = tempfile::tempdir().unwrap();

            let top_level = repo
                .git_from_working_dir(unrelated.path(), &["rev-parse", "--show-toplevel"])
                .unwrap();

            assert_eq!(
                Path::new(top_level.trim()).canonicalize().unwrap(),
                repo.path().canonicalize().unwrap()
            );
        });
    }
}
