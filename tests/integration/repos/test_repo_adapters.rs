use std::ops::Deref;
use std::path::Path;

use super::test_repo::TestRepo;

#[allow(dead_code)]
pub(crate) struct TestRepoWithCFlag {
    inner: TestRepo,
}

#[allow(dead_code)]
impl TestRepoWithCFlag {
    pub(crate) fn new() -> Self {
        Self {
            inner: TestRepo::new(),
        }
    }

    pub(crate) fn git_from_working_dir(
        &self,
        _working_dir: &Path,
        args: &[&str],
    ) -> Result<String, String> {
        self.inner
            .git_with_env_using_c_flag_from(&std::env::temp_dir(), args, &[])
    }

    pub(crate) fn git_with_env(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
        working_dir: Option<&Path>,
    ) -> Result<String, String> {
        if working_dir.is_some() {
            self.inner
                .git_with_env_using_c_flag_from(&std::env::temp_dir(), args, envs)
        } else {
            self.inner.git_with_env(args, envs, None)
        }
    }
}

impl Deref for TestRepoWithCFlag {
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
        let repo = TestRepoWithCFlag::new();
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
    }

    #[test]
    fn c_flag_adapter_uses_repo_root_from_unrelated_process_cwd() {
        let repo = TestRepoWithCFlag::new();
        let unrelated = tempfile::tempdir().unwrap();

        let top_level = repo
            .git_from_working_dir(unrelated.path(), &["rev-parse", "--show-toplevel"])
            .unwrap();

        assert_eq!(
            Path::new(top_level.trim()).canonicalize().unwrap(),
            repo.path().canonicalize().unwrap()
        );
    }
}
