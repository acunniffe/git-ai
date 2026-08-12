use super::*;

impl TestRepo {
    pub fn git_ai(&self, args: &[&str]) -> Result<String, String> {
        self.git_ai_with_env(args, &[])
    }

    /// Run the standard preset checkpoint form with inline hook input.
    ///
    /// Tests that need a custom CWD, environment, stdin, or trailing checkpoint
    /// arguments should keep using the lower-level command helpers.
    pub fn checkpoint_with_hook_input(
        &self,
        preset: &str,
        hook_input: &str,
    ) -> Result<String, String> {
        self.git_ai(&["checkpoint", preset, "--hook-input", hook_input])
    }

    pub fn git_ai_without_pre_sync_for_test(&self, args: &[&str]) -> Result<String, String> {
        self.run_git_ai(args, &[], &self.path, None, false, false)
    }

    pub fn git_ai_with_env_without_pre_sync_for_test(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<String, String> {
        self.run_git_ai(args, envs, &self.path, None, false, false)
    }

    pub fn git_ai_command_without_pre_sync_for_test(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Command {
        self.git_ai_command(args, envs, &self.path)
    }

    fn git_ai_command(&self, args: &[&str], envs: &[(&str, &str)], cwd: &Path) -> Command {
        let normalized_args = normalize_test_git_ai_checkpoint_args(args);

        let mut command = Command::new(get_binary_path());
        command.args(&normalized_args).current_dir(cwd);
        self.configure_git_ai_env(&mut command);
        command.envs(envs.iter().copied());

        command
    }

    pub fn git_ai_from_working_dir(
        &self,
        working_dir: &std::path::Path,
        args: &[&str],
    ) -> Result<String, String> {
        let absolute_working_dir = working_dir.canonicalize().map_err(|e| {
            format!(
                "Failed to canonicalize working directory {}: {}",
                working_dir.display(),
                e
            )
        })?;
        self.run_git_ai(args, &[], &absolute_working_dir, None, true, true)
    }

    pub fn git_ai_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Result<String, String> {
        self.run_git_ai(args, envs, &self.path, None, true, false)
    }

    fn run_git_ai(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
        cwd: &Path,
        stdin: Option<&[u8]>,
        sync_before_read: bool,
        resolve_checkpoint_families: bool,
    ) -> Result<String, String> {
        if sync_before_read && git_ai_command_requires_daemon_sync(args) {
            self.sync_daemon_force();
        }

        let is_checkpoint = git_ai_primary_command(args) == Some("checkpoint");

        let mut command = self.git_ai_command(args, envs, cwd);
        let output = if let Some(stdin) = stdin {
            run_command_output_with_stdin(&mut command, &format!("git-ai stdin {:?}", args), stdin)?
        } else {
            run_command_output(&mut command, &format!("git-ai {:?}", args))?
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if output.status.success() {
            if is_checkpoint && self.has_active_daemon() {
                let count = parse_checkpoint_request_count(&stdout);
                if count > 0 {
                    if resolve_checkpoint_families {
                        let mut registry = daemon_sync_registry()
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        for (family_key, count) in
                            self.resolve_checkpoint_family_keys_from_args(args)
                        {
                            registry.raise_expected_checkpoint_count(&family_key, count);
                        }
                    } else {
                        self.record_pending_checkpoint_completions(count);
                    }
                }
            }
            Ok(combine_output(stdout, stderr))
        } else {
            // Combine stdout and stderr so callers can find structured
            // output (e.g. JSON errors) that the command wrote to stdout
            // before exiting with a non-zero status.
            Err(combine_output(stderr, stdout))
        }
    }

    /// Run a git-ai command with data provided on stdin
    pub fn git_ai_with_stdin(&self, args: &[&str], stdin_data: &[u8]) -> Result<String, String> {
        self.run_git_ai(args, &[], &self.path, Some(stdin_data), true, false)
    }
}
