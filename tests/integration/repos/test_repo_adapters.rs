use std::ops::Deref;
use std::path::Path;
use std::process::Command;

use super::test_repo::{
    TestRepo, git_command_requires_daemon_sync, git_command_routes_to_clone_target,
    new_daemon_test_sync_session_id, real_git_executable,
};

#[allow(dead_code)]
pub(crate) struct TestRepoWithCFlag {
    inner: TestRepo,
}

#[allow(clippy::expect_fun_call, dead_code)]
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
        let arbitrary_dir = std::env::temp_dir();
        let command_affects_daemon = self
            .inner
            .git_command_affects_daemon_for_tracking(args, Some(self.inner.path().as_path()));

        if git_command_requires_daemon_sync(args) {
            self.inner.sync_daemon_force();
        }

        let daemon_command_pending =
            command_affects_daemon && !git_command_routes_to_clone_target(args);
        let daemon_test_sync_session = daemon_command_pending.then(new_daemon_test_sync_session_id);
        let mut full_args = vec![
            "-C".to_string(),
            self.inner.path().to_str().unwrap().to_string(),
        ];
        if let Some(session) = daemon_test_sync_session.as_deref() {
            self.inner
                .append_daemon_test_sync_session_args(&mut full_args, session);
        }
        full_args.extend(args.iter().map(|arg| (*arg).to_string()));

        let mut command = Command::new(real_git_executable());
        command.current_dir(&arbitrary_dir);
        command.args(&full_args);
        command.env("HOME", self.inner.test_home_path());
        command.env(
            "GIT_CONFIG_GLOBAL",
            self.inner.test_home_path().join(".gitconfig"),
        );
        command.env(
            "XDG_CONFIG_HOME",
            self.inner.test_home_path().join(".config"),
        );
        command.env("GIT_CONFIG_NOSYSTEM", "1");
        let trace_socket = self.inner.daemon_trace_socket_path();
        let nesting =
            std::env::var("GIT_AI_TEST_TRACE2_NESTING").unwrap_or_else(|_| "0".to_string());
        command.env(
            "GIT_TRACE2_EVENT",
            git_ai::operations::daemon::DaemonConfig::trace2_event_target_for_path(&trace_socket),
        );
        command.env("GIT_TRACE2_EVENT_NESTING", nesting);

        if let Some(patch) = &self.inner.config_patch
            && let Ok(patch_json) = serde_json::to_string(patch)
        {
            command.env("GIT_AI_TEST_CONFIG_PATCH", patch_json);
        }

        command.env(
            "GIT_AI_TEST_DB_PATH",
            self.inner.test_db_path().to_str().unwrap(),
        );
        command.env(
            "GITAI_TEST_DB_PATH",
            self.inner.test_db_path().to_str().unwrap(),
        );

        let output = command.output().expect(&format!(
            "Failed to execute git command with -C flag: {:?}",
            args
        ));

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if daemon_command_pending {
            self.inner.record_daemon_family_expected_completion_session(
                daemon_test_sync_session
                    .as_deref()
                    .expect("daemon test sync session should exist"),
            );
        }

        if output.status.success() {
            Ok(if stdout.is_empty() { stderr } else { stdout })
        } else {
            Err(stderr)
        }
    }

    pub(crate) fn git_with_env(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
        working_dir: Option<&Path>,
    ) -> Result<String, String> {
        if working_dir.is_none() {
            return self.inner.git_with_env(args, envs, None);
        }

        let arbitrary_dir = std::env::temp_dir();
        let command_affects_daemon = self
            .inner
            .git_command_affects_daemon_for_tracking(args, Some(self.inner.path().as_path()));

        if git_command_requires_daemon_sync(args) {
            self.inner.sync_daemon_force();
        }

        let daemon_command_pending =
            command_affects_daemon && !git_command_routes_to_clone_target(args);
        let daemon_test_sync_session = daemon_command_pending.then(new_daemon_test_sync_session_id);
        let mut full_args = vec![
            "-C".to_string(),
            self.inner.path().to_str().unwrap().to_string(),
        ];
        if let Some(session) = daemon_test_sync_session.as_deref() {
            self.inner
                .append_daemon_test_sync_session_args(&mut full_args, session);
        }
        full_args.extend(args.iter().map(|arg| (*arg).to_string()));

        let mut command = Command::new(real_git_executable());
        command.current_dir(&arbitrary_dir);
        command.args(&full_args);
        command.env("HOME", self.inner.test_home_path());
        command.env(
            "GIT_CONFIG_GLOBAL",
            self.inner.test_home_path().join(".gitconfig"),
        );
        command.env(
            "XDG_CONFIG_HOME",
            self.inner.test_home_path().join(".config"),
        );
        command.env("GIT_CONFIG_NOSYSTEM", "1");
        let trace_socket = self.inner.daemon_trace_socket_path();
        let nesting =
            std::env::var("GIT_AI_TEST_TRACE2_NESTING").unwrap_or_else(|_| "0".to_string());
        command.env(
            "GIT_TRACE2_EVENT",
            git_ai::operations::daemon::DaemonConfig::trace2_event_target_for_path(&trace_socket),
        );
        command.env("GIT_TRACE2_EVENT_NESTING", nesting);

        if let Some(patch) = &self.inner.config_patch
            && let Ok(patch_json) = serde_json::to_string(patch)
        {
            command.env("GIT_AI_TEST_CONFIG_PATCH", patch_json);
        }

        command.env(
            "GIT_AI_TEST_DB_PATH",
            self.inner.test_db_path().to_str().unwrap(),
        );
        command.env(
            "GITAI_TEST_DB_PATH",
            self.inner.test_db_path().to_str().unwrap(),
        );

        for (key, value) in envs {
            command.env(key, value);
        }

        let output = command.output().expect(&format!(
            "Failed to execute git command with -C flag and env: {:?}",
            args
        ));

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if daemon_command_pending {
            self.inner.record_daemon_family_expected_completion_session(
                daemon_test_sync_session
                    .as_deref()
                    .expect("daemon test sync session should exist"),
            );
        }

        if output.status.success() {
            Ok(if stdout.is_empty() { stderr } else { stdout })
        } else {
            Err(stderr)
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
