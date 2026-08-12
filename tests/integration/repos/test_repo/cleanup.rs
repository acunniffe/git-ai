use super::*;

impl Drop for TestRepo {
    fn drop(&mut self) {
        if std::env::var("GIT_AI_TEST_KEEP_REPOS")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            return;
        }

        if self.daemon_scope == DaemonTestScope::Dedicated
            && let Some(daemon) = self.daemon_process.take()
        {
            daemon.shutdown();
        }

        let remove_test_db = self.daemon_scope != DaemonTestScope::Shared;

        if let Some(base_path) = &self._base_repo_path {
            let mut command = Command::new(real_git_executable());
            command.args([
                "-C",
                base_path.to_str().unwrap(),
                "worktree",
                "remove",
                "--force",
                self.path.to_str().unwrap(),
            ]);
            let _ = run_command_output(&mut command, "remove linked test worktree");

            let _ = remove_dir_all_with_retry(&self.path, 80, Duration::from_millis(50));
            let _ = remove_dir_all_with_retry(base_path, 80, Duration::from_millis(50));

            if let Some(base_db_path) = &self._base_test_db_path
                && remove_test_db
            {
                let _ = remove_dir_all_with_retry(base_db_path, 40, Duration::from_millis(25));
            }

            if remove_test_db {
                let _ =
                    remove_dir_all_with_retry(&self.test_db_path, 40, Duration::from_millis(25));
            }
            let _ = remove_dir_all_with_retry(&self.test_home, 40, Duration::from_millis(25));
            return;
        }

        remove_dir_all_with_retry(&self.path, 80, Duration::from_millis(50))
            .expect("failed to remove test repo");
        // Also clean up the test database directory (may not exist if no DB operations were done)
        if remove_test_db {
            let _ = remove_dir_all_with_retry(&self.test_db_path, 40, Duration::from_millis(25));
        }
        let _ = remove_dir_all_with_retry(&self.test_home, 40, Duration::from_millis(25));
    }
}

pub(super) fn remove_dir_all_with_retry(
    path: &std::path::Path,
    attempts: usize,
    delay: Duration,
) -> std::io::Result<()> {
    for attempt in 0..attempts {
        match fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) if should_retry_remove_dir_error(&err) => {
                if attempt + 1 == attempts {
                    return Err(err);
                }
                std::thread::sleep(delay);
            }
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

#[cfg(unix)]
pub(super) fn is_process_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error()
        .raw_os_error()
        .is_some_and(|code| code == libc::EPERM)
}

#[cfg(unix)]
pub(super) fn reap_child_if_exited(pid: u32) -> bool {
    let mut status: libc::c_int = 0;
    let rc = unsafe {
        libc::waitpid(
            pid as libc::pid_t,
            &mut status as *mut libc::c_int,
            libc::WNOHANG,
        )
    };
    rc == pid as libc::pid_t || rc == -1
}

pub(super) fn should_retry_remove_dir_error(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::DirectoryNotEmpty
        || err.kind() == std::io::ErrorKind::PermissionDenied
    {
        return true;
    }

    #[cfg(windows)]
    {
        // Windows can report transient file locks as `Uncategorized` with raw code 32.
        // Retry these so process teardown races don't fail otherwise-successful tests.
        if let Some(code) = err.raw_os_error() {
            return matches!(code, 5 | 32 | 145);
        }
    }

    false
}

pub(super) fn is_transient_git_index_lock_error(stderr: &str) -> bool {
    stderr.contains(".git/index.lock")
        && (stderr.contains("File exists")
            || stderr.contains("Another git process seems to be running"))
}
