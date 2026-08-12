use super::cleanup::is_transient_git_index_lock_error;
use super::*;

enum GitExecutionLocation {
    RepoViaC { process_cwd: Option<PathBuf> },
    WorkingDirectory(PathBuf),
}

impl GitExecutionLocation {
    fn repo_context<'a>(&'a self, repo_path: &'a Path) -> &'a Path {
        match self {
            Self::RepoViaC { .. } => repo_path,
            Self::WorkingDirectory(path) => path,
        }
    }

    fn configure_command(&self, command: &mut Command, repo_path: &Path, args: &mut Vec<String>) {
        match self {
            Self::RepoViaC { process_cwd } => {
                args.push("-C".to_string());
                args.push(repo_path.to_str().unwrap().to_string());
                if let Some(process_cwd) = process_cwd {
                    command.current_dir(process_cwd);
                }
            }
            Self::WorkingDirectory(path) => {
                command.current_dir(path);
            }
        }
    }
}

impl TestRepo {
    pub(super) fn parsed_git_invocation_for_tracking(
        &self,
        args: &[&str],
        repo_context: Option<&Path>,
    ) -> ParsedGitInvocation {
        let argv = args
            .iter()
            .map(|arg| (*arg).to_string())
            .collect::<Vec<_>>();
        let cwd = repo_context.unwrap_or_else(|| self.path().as_path());
        git_ai::operations::daemon::test_sync::tracked_parsed_git_invocation_for_test_sync(
            &argv, cwd,
        )
    }

    pub(crate) fn git_command_affects_daemon_for_tracking(
        &self,
        args: &[&str],
        repo_context: Option<&Path>,
    ) -> bool {
        let parsed = self.parsed_git_invocation_for_tracking(args, repo_context);
        git_ai::operations::daemon::test_sync::tracks_parsed_git_invocation_for_test_sync(&parsed)
    }

    pub fn current_branch(&self) -> String {
        self.git(&["branch", "--show-current"])
            .unwrap()
            .trim()
            .to_string()
    }

    pub fn git(&self, args: &[&str]) -> Result<String, String> {
        self.git_with_env(args, &[], None)
    }

    pub fn git_without_test_sync_for_test(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<String, String> {
        let mut command = Command::new(real_git_executable());
        command.arg("-C").arg(&self.path).args(args);
        self.configure_command_env(&mut command);
        command.envs(envs.iter().copied());

        let output = run_command_output(&mut command, &format!("git-no-test-sync {:?}", args))?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let combined = combine_output(stdout, stderr);
        if output.status.success() {
            Ok(combined)
        } else {
            Err(combined)
        }
    }

    /// Run a git command from a working directory (without using -C flag)
    /// This tests that git-ai correctly finds the repository root when run from a subdirectory
    /// The working_dir will be canonicalized to ensure it's an absolute path
    pub fn git_from_working_dir(
        &self,
        working_dir: &std::path::Path,
        args: &[&str],
    ) -> Result<String, String> {
        self.git_with_env(args, &[], Some(working_dir))
    }

    pub fn git_og(&self, args: &[&str]) -> Result<String, String> {
        self.git_og_with_env(args, &[])
    }

    /// Run a raw git command (bypassing git-ai hooks) with custom environment variables.
    /// Useful for creating commits with specific author/committer identities.
    pub fn git_og_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Result<String, String> {
        #[cfg(windows)]
        let null_hooks = "NUL";
        #[cfg(not(windows))]
        let null_hooks = "/dev/null";

        let retry_limit = 8usize;
        let retry_delay = Duration::from_millis(50);
        let tracked_invocation =
            self.parsed_git_invocation_for_tracking(args, Some(self.path.as_path()));
        let command_affects_daemon = env_explicitly_enables_trace2(envs)
            && git_ai::operations::daemon::test_sync::tracks_parsed_git_invocation_for_test_sync(
                &tracked_invocation,
            );
        for attempt in 0..=retry_limit {
            let daemon_command_pending = command_affects_daemon
                && !git_invocation_routes_to_clone_target(&tracked_invocation);
            let daemon_test_sync_session =
                daemon_command_pending.then(new_daemon_test_sync_session_id);

            let mut command = Command::new(real_git_executable());
            let mut command_args = vec!["-C".to_string(), self.path.to_str().unwrap().to_string()];
            command_args.push("-c".to_string());
            command_args.push(format!("core.hooksPath={}", null_hooks));
            if let Some(session) = daemon_test_sync_session.as_deref() {
                self.append_daemon_test_sync_session_args(&mut command_args, session);
            }
            command_args.extend(args.iter().map(|s| s.to_string()));
            command.args(&command_args);
            configure_test_home_env(&mut command, &self.test_home);
            command.envs(envs.iter().copied());

            let output = run_command_output(&mut command, &format!("git_og {:?}", args))?;

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if output.status.success() {
                let combined = combine_output(stdout, stderr);
                if command_affects_daemon {
                    self.record_completed_git_command(
                        &tracked_invocation,
                        self.path.as_path(),
                        daemon_test_sync_session.as_deref(),
                    );
                }
                return Ok(combined);
            }

            if attempt < retry_limit && is_transient_git_index_lock_error(&stderr) {
                std::thread::sleep(retry_delay);
                continue;
            }

            if daemon_command_pending {
                self.record_daemon_family_expected_completion_session(
                    daemon_test_sync_session
                        .as_deref()
                        .expect("daemon test sync session should exist for tracked command"),
                );
            }
            return Err(format!("{}{}", stdout, stderr));
        }

        Err("git_og_with_env failed after retries".to_string())
    }

    /// Write a file and commit it via `git_og` (bypassing git-ai hooks), with
    /// NO checkpoint fired — the commit lands as a fully untracked change.
    /// A trailing newline is appended if missing for clean 3-way merge
    /// behaviour.
    pub fn commit_untracked_file(&self, filename: &str, content: &str, message: &str) {
        let path = self.path.join(filename);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        let content_with_nl = if content.ends_with('\n') {
            content.to_string()
        } else {
            format!("{}\n", content)
        };
        fs::write(&path, content_with_nl.as_bytes()).expect("write file");
        self.git_og(&["add", filename]).expect("git add");
        self.git_og(&["commit", "-m", message]).expect("git commit");
    }

    pub fn benchmark_git(&self, args: &[&str]) -> Result<BenchmarkResult, String> {
        let output = self.git_with_env(args, &[("GIT_AI_DEBUG_PERFORMANCE", "2")], None)?;

        println!("output: {}", output);
        Self::parse_benchmark_result(&output)
    }

    pub fn benchmark_git_ai(&self, args: &[&str]) -> Result<BenchmarkResult, String> {
        let output = self.git_ai_with_env(args, &[("GIT_AI_DEBUG_PERFORMANCE", "2")])?;

        println!("output: {}", output);
        Self::parse_benchmark_result(&output)
    }

    pub(super) fn parse_benchmark_result(output: &str) -> Result<BenchmarkResult, String> {
        // Find the JSON performance line
        for line in output.lines() {
            if line.contains("[git-ai (perf-json)]") {
                // Extract the JSON part after the colored prefix
                if let Some(json_start) = line.find('{') {
                    let json_str = &line[json_start..];
                    let parsed: serde_json::Value = serde_json::from_str(json_str)
                        .map_err(|e| format!("Failed to parse performance JSON: {}", e))?;

                    return Ok(BenchmarkResult {
                        total_duration: Duration::from_millis(
                            parsed["total_duration_ms"].as_u64().unwrap_or(0),
                        ),
                        git_duration: Duration::from_millis(
                            parsed["git_duration_ms"].as_u64().unwrap_or(0),
                        ),
                        pre_command_duration: Duration::from_millis(
                            parsed["pre_command_duration_ms"].as_u64().unwrap_or(0),
                        ),
                        post_command_duration: Duration::from_millis(
                            parsed["post_command_duration_ms"].as_u64().unwrap_or(0),
                        ),
                    });
                }
            }
        }

        Err("No performance data found in output".to_string())
    }

    pub fn git_with_env(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
        working_dir: Option<&std::path::Path>,
    ) -> Result<String, String> {
        let location = if let Some(working_dir_path) = working_dir {
            GitExecutionLocation::WorkingDirectory(working_dir_path.canonicalize().map_err(
                |e| {
                    format!(
                        "Failed to canonicalize working directory {}: {}",
                        working_dir_path.display(),
                        e
                    )
                },
            )?)
        } else {
            GitExecutionLocation::RepoViaC { process_cwd: None }
        };
        self.run_git_with_env(args, envs, location)
    }

    pub(crate) fn git_with_env_using_c_flag_from(
        &self,
        process_cwd: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<String, String> {
        let process_cwd = process_cwd.canonicalize().map_err(|error| {
            format!(
                "Failed to canonicalize git process working directory {}: {}",
                process_cwd.display(),
                error
            )
        })?;
        self.run_git_with_env(
            args,
            envs,
            GitExecutionLocation::RepoViaC {
                process_cwd: Some(process_cwd),
            },
        )
    }

    fn run_git_with_env(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
        location: GitExecutionLocation,
    ) -> Result<String, String> {
        let command_context = Some(location.repo_context(self.path.as_path()));
        let tracked_invocation = self.parsed_git_invocation_for_tracking(args, command_context);

        if git_invocation_requires_daemon_sync(&tracked_invocation) {
            self.sync_daemon_force();
        }

        let retry_limit = 8usize;
        let retry_delay = Duration::from_millis(50);
        let command_affects_daemon = self.has_active_daemon()
            && git_ai::operations::daemon::test_sync::tracks_parsed_git_invocation_for_test_sync(
                &tracked_invocation,
            );
        for attempt in 0..=retry_limit {
            let daemon_command_pending = command_affects_daemon
                && !git_invocation_routes_to_clone_target(&tracked_invocation);
            let daemon_test_sync_session =
                daemon_command_pending.then(new_daemon_test_sync_session_id);

            let mut command = Command::new(real_git_executable());

            let mut command_args = Vec::<String>::new();
            if let Some(session) = daemon_test_sync_session.as_deref() {
                self.append_daemon_test_sync_session_args(&mut command_args, session);
            }
            location.configure_command(&mut command, self.path.as_path(), &mut command_args);
            command_args.extend(args.iter().map(|arg| (*arg).to_string()));
            command.args(&command_args);

            self.configure_command_env(&mut command);

            command.envs(envs.iter().copied());

            let output = run_command_output(&mut command, &format!("git {:?}", args))?;

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if output.status.success() {
                let combined = combine_output(stdout, stderr);
                if command_affects_daemon {
                    self.record_completed_git_command(
                        &tracked_invocation,
                        location.repo_context(self.path.as_path()),
                        daemon_test_sync_session.as_deref(),
                    );
                }
                return Ok(combined);
            }

            if attempt < retry_limit && is_transient_git_index_lock_error(&stderr) {
                std::thread::sleep(retry_delay);
                continue;
            }

            if daemon_command_pending {
                self.record_daemon_family_expected_completion_session(
                    daemon_test_sync_session
                        .as_deref()
                        .expect("daemon test sync session should exist for tracked command"),
                );
            }
            return Err(stderr);
        }

        Err("git_with_env failed after retries".to_string())
    }

    fn record_completed_git_command(
        &self,
        invocation: &ParsedGitInvocation,
        cwd: &Path,
        session: Option<&str>,
    ) {
        if git_invocation_routes_to_clone_target(invocation) {
            if let Some(target_repo_path) = clone_target_path(invocation, cwd) {
                self.sync_daemon_clone_target(&target_repo_path);
            }
        } else if let Some(session) = session {
            self.record_daemon_family_expected_completion_session(session);
        }
    }
}
