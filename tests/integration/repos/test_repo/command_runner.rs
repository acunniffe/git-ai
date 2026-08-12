use super::*;

/// A test-only real-Git process with explicit command setup.
///
/// Keep this separate from [`TestRepo::git`] and [`TestRepo::git_og`]: callers
/// use it when they must control low-level Git invocation details without
/// daemon synchronization or git-ai proxy behavior.
pub(crate) struct RawGitCommand<'a> {
    command: Command,
    args: &'a [&'a str],
    stdin_data: Option<&'a [u8]>,
}

impl<'a> RawGitCommand<'a> {
    pub(crate) fn in_working_dir(workdir: &Path, args: &'a [&'a str]) -> Self {
        let mut command = Command::new(real_git_executable());
        command.current_dir(workdir);
        Self {
            command,
            args,
            stdin_data: None,
        }
    }

    pub(crate) fn with_git_c(repo_path: &Path, args: &'a [&'a str]) -> Self {
        let mut command = Command::new(real_git_executable());
        command.arg("-C").arg(repo_path);
        Self {
            command,
            args,
            stdin_data: None,
        }
    }

    pub(crate) fn with_config(mut self, key: &str, value: &str) -> Self {
        self.command.arg("-c").arg(format!("{key}={value}"));
        self
    }

    pub(crate) fn without_hooks(self) -> Self {
        self.with_config("core.hooksPath", "/dev/null")
    }

    pub(crate) fn env(
        mut self,
        key: impl AsRef<std::ffi::OsStr>,
        value: impl AsRef<std::ffi::OsStr>,
    ) -> Self {
        self.command.env(key, value);
        self
    }

    pub(crate) fn configure(mut self, configure: impl FnOnce(&mut Command)) -> Self {
        configure(&mut self.command);
        self
    }

    pub(crate) fn with_stdin(mut self, stdin_data: &'a [u8]) -> Self {
        self.stdin_data = Some(stdin_data);
        self
    }

    pub(crate) fn output(mut self) -> Result<Output, String> {
        self.command.args(self.args);
        let label = format!("raw git {:?}", self.args);
        if let Some(stdin_data) = self.stdin_data {
            run_command_output_with_stdin(&mut self.command, &label, stdin_data)
        } else {
            run_command_output(&mut self.command, &label)
        }
    }
}

/// Run real Git plumbing with a deterministic test identity and hooks disabled.
///
/// This preserves the low-level behavior needed by tests that construct Git
/// objects directly while sharing process setup and captured-output handling.
pub(crate) fn run_raw_git_plumbing(
    repo_path: &Path,
    args: &[&str],
    stdin_data: Option<&[u8]>,
) -> String {
    let command = RawGitCommand::with_git_c(repo_path, args)
        .without_hooks()
        .with_config("user.name", "Test")
        .with_config("user.email", "test@test.com");
    let command = if let Some(stdin_data) = stdin_data {
        command.with_stdin(stdin_data)
    } else {
        command
    };
    let output = command
        .output()
        .expect("failed to run raw git plumbing command");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("non-utf8 git output")
        .trim()
        .to_string()
}

pub(super) fn configure_test_home_env(command: &mut Command, test_home: &Path) {
    command.env("HOME", test_home);
    if !command
        .get_envs()
        .any(|(key, _)| key == std::ffi::OsStr::new("GIT_AI_TEST_NOTES_DB_PATH"))
    {
        command.env(
            "GIT_AI_TEST_NOTES_DB_PATH",
            test_home.join(".git-ai").join("internal").join("notes-db"),
        );
    }
    command.env("GIT_CONFIG_GLOBAL", test_home.join(".gitconfig"));
    // Redirect XDG_CONFIG_HOME so git does not read the real user's
    // $XDG_CONFIG_HOME/git/config (which may contain filter drivers,
    // aliases, or other settings that break test isolation).
    command.env("XDG_CONFIG_HOME", test_home.join(".config"));
    // Suppress system-level git config that could interfere with test isolation.
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    // Sanitize PATH: remove any directories that contain a git-ai wrapper.
    // Without this, git internals (which call `git` sub-processes via PATH) will
    // hit the installed release git-ai binary, which spawns a background daemon
    // for every invocation — causing a process storm.
    #[cfg(not(windows))]
    if let Ok(path) = std::env::var("PATH") {
        let sanitized: Vec<&str> = path
            .split(':')
            .filter(|dir| {
                let git_path = std::path::Path::new(dir).join("git");
                if git_path.is_file() || git_path.is_symlink() {
                    // Shell-script wrapper containing "git-ai"
                    if let Ok(contents) = fs::read_to_string(&git_path)
                        && contents.contains("git-ai")
                    {
                        return false;
                    }
                    // Symlink whose target contains "git-ai"
                    if let Ok(target) = std::fs::read_link(&git_path)
                        && target.to_string_lossy().contains("git-ai")
                    {
                        return false;
                    }
                    // Canonical path contains "git-ai"
                    if let Ok(canonical) = git_path.canonicalize()
                        && canonical.to_string_lossy().contains("git-ai")
                    {
                        return false;
                    }
                }
                true
            })
            .collect();
        command.env("PATH", sanitized.join(":"));
    }
    #[cfg(windows)]
    {
        command.env("USERPROFILE", test_home);
        command.env("APPDATA", test_home.join("AppData").join("Roaming"));
        command.env("LOCALAPPDATA", test_home.join("AppData").join("Local"));
    }
}

pub(super) fn run_command_output(command: &mut Command, label: &str) -> Result<Output, String> {
    run_command_output_with_timeout(command, label, TEST_SUBPROCESS_TIMEOUT)
}

pub(super) fn combine_output(first: String, second: String) -> String {
    if first.is_empty() {
        second
    } else {
        first + &second
    }
}

pub(super) fn run_command_output_with_stdin(
    command: &mut Command,
    label: &str,
    stdin_data: &[u8],
) -> Result<Output, String> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let debug_command = format!("{:?}", command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to spawn {label}: {error}\ncommand: {debug_command}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_data)
            .map_err(|error| format!("failed to write stdin for {label}: {error}"))?;
    }
    collect_child_output_with_timeout(child, label, debug_command, TEST_SUBPROCESS_TIMEOUT)
}

pub(super) fn run_command_output_with_timeout(
    command: &mut Command,
    label: &str,
    timeout: Duration,
) -> Result<Output, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let debug_command = format!("{:?}", command);
    let child = command
        .spawn()
        .map_err(|error| format!("failed to spawn {label}: {error}\ncommand: {debug_command}"))?;
    collect_child_output_with_timeout(child, label, debug_command, timeout)
}

pub(super) fn collect_child_output_with_timeout(
    mut child: Child,
    label: &str,
    debug_command: String,
    timeout: Duration,
) -> Result<Output, String> {
    let pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{label} child stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("{label} child stderr was not piped"))?;

    let stdout_reader = thread::spawn(move || {
        let mut stdout = stdout;
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let stderr_reader = thread::spawn(move || {
        let mut stderr = stderr;
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = stdout_reader.join().unwrap_or_default();
                let stderr = stderr_reader.join().unwrap_or_default();
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = stdout_reader.join().unwrap_or_default();
                let stderr = stderr_reader.join().unwrap_or_default();
                return Err(format!(
                    "failed polling {label} child process {pid}: {error}\ncommand: {debug_command}\nstdout tail:\n{}\nstderr tail:\n{}",
                    output_tail(&stdout),
                    output_tail(&stderr)
                ));
            }
        }

        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let stdout = stdout_reader.join().unwrap_or_default();
            let stderr = stderr_reader.join().unwrap_or_default();
            return Err(format!(
                "{label} timed out after {timeout:?} (pid {pid})\ncommand: {debug_command}\nstdout tail:\n{}\nstderr tail:\n{}",
                output_tail(&stdout),
                output_tail(&stderr)
            ));
        }

        thread::sleep(Duration::from_millis(10));
    }
}

pub(super) fn output_tail(bytes: &[u8]) -> String {
    const MAX_TAIL_BYTES: usize = 4096;
    let start = bytes.len().saturating_sub(MAX_TAIL_BYTES);
    String::from_utf8_lossy(&bytes[start..]).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_git_command_supports_common_test_plumbing() {
        let temp_dir = tempfile::tempdir().expect("temporary repo directory should be created");
        let repo_path = temp_dir.path();

        let init_output = RawGitCommand::in_working_dir(repo_path, &["init"])
            .output()
            .expect("raw git init should run");
        assert!(init_output.status.success());

        let workdir_output =
            RawGitCommand::in_working_dir(repo_path, &["rev-parse", "--show-toplevel"])
                .output()
                .expect("raw git command should run");
        assert!(workdir_output.status.success());

        // Git emits a normal absolute path on Windows, while canonicalize()
        // uses the equivalent verbatim-path form. Compare canonical locations
        // so the assertion checks the repository root rather than its spelling.
        let git_workdir = PathBuf::from(
            String::from_utf8(workdir_output.stdout)
                .expect("git output should be utf-8")
                .trim(),
        )
        .canonicalize()
        .expect("git-reported worktree should canonicalize");
        assert_eq!(
            git_workdir,
            repo_path
                .canonicalize()
                .expect("repo path should canonicalize")
        );

        let stdin_output = RawGitCommand::in_working_dir(repo_path, &["hash-object", "--stdin"])
            .with_stdin(b"raw git test input")
            .output()
            .expect("raw git command with stdin should run");
        assert!(stdin_output.status.success());
        assert!(!stdin_output.stdout.is_empty());

        let failed_output =
            RawGitCommand::in_working_dir(repo_path, &["rev-parse", "--verify", "missing-ref"])
                .output()
                .expect("failed raw git command should still return its output");
        assert!(!failed_output.status.success());
        assert!(!failed_output.stderr.is_empty());

        let plumbing_output = run_raw_git_plumbing(
            repo_path,
            &["hash-object", "--stdin"],
            Some(b"raw git plumbing test input"),
        );
        assert!(!plumbing_output.is_empty());

        let configured_output =
            RawGitCommand::in_working_dir(repo_path, &["config", "--get", "user.name"])
                .with_config("user.name", "Raw Git Test")
                .output()
                .expect("configured raw git command should run");
        assert!(configured_output.status.success());
        assert_eq!(
            String::from_utf8(configured_output.stdout)
                .expect("git output should be utf-8")
                .trim(),
            "Raw Git Test"
        );

        let hooks_output =
            RawGitCommand::in_working_dir(repo_path, &["config", "--get", "core.hooksPath"])
                .without_hooks()
                .output()
                .expect("raw git command with hooks disabled should run");
        assert!(hooks_output.status.success());
        assert_eq!(
            String::from_utf8(hooks_output.stdout)
                .expect("git output should be utf-8")
                .trim(),
            "/dev/null"
        );

        let author_output = RawGitCommand::in_working_dir(repo_path, &["var", "GIT_AUTHOR_IDENT"])
            .env("GIT_AUTHOR_NAME", "Raw Git Author")
            .env("GIT_AUTHOR_EMAIL", "raw-git@example.com")
            .output()
            .expect("raw git command with environment should run");
        assert!(author_output.status.success());
        assert!(
            String::from_utf8(author_output.stdout)
                .expect("git output should be utf-8")
                .starts_with("Raw Git Author <raw-git@example.com>")
        );
    }

    #[test]
    fn test_configure_test_home_env_isolates_notes_database() {
        let test_home = PathBuf::from("isolated-test-home");
        let mut command = Command::new("git");

        configure_test_home_env(&mut command, &test_home);

        let notes_db_path = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("GIT_AI_TEST_NOTES_DB_PATH"))
            .and_then(|(_, value)| value)
            .map(PathBuf::from);
        assert_eq!(
            notes_db_path,
            Some(test_home.join(".git-ai").join("internal").join("notes-db"))
        );
    }

    #[test]
    fn test_configure_test_home_env_preserves_explicit_notes_database() {
        let test_home = PathBuf::from("isolated-test-home");
        let explicit_notes_db = PathBuf::from("explicit-notes-db");
        let mut command = Command::new("git");
        command.env("GIT_AI_TEST_NOTES_DB_PATH", &explicit_notes_db);

        configure_test_home_env(&mut command, &test_home);

        let notes_db_path = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("GIT_AI_TEST_NOTES_DB_PATH"))
            .and_then(|(_, value)| value)
            .map(PathBuf::from);
        assert_eq!(notes_db_path, Some(explicit_notes_db));
    }
}
