use super::*;

static DEFAULT_BRANCH_NAME: OnceLock<String> = OnceLock::new();
static TEMPLATE_REPO: OnceLock<PathBuf> = OnceLock::new();
static TEMPLATE_BARE_REPO: OnceLock<PathBuf> = OnceLock::new();
static COMPILED_BINARY: OnceLock<PathBuf> = OnceLock::new();

/// Find the real git binary by directly probing candidate paths — without reading
/// any HOME-derived config. Called once during process HOME isolation setup.
pub(super) fn find_real_git_by_probe() -> String {
    // Read HOME *before* we replace it with the isolated dir.
    let local_git = std::env::var("HOME")
        .map(|h| format!("{h}/.local/bin/git"))
        .unwrap_or_default();

    // Check ~/.local/bin/git first (Linux XDG user binary dir)
    if !local_git.is_empty() {
        let p = Path::new(&local_git);
        if git_ai::config::is_real_git_candidate(p) {
            return local_git;
        }
    }

    let candidates: &[&str] = &[
        "/opt/homebrew/bin/git", // macOS Homebrew ARM
        "/usr/local/bin/git",    // macOS Homebrew Intel / manual
        "/usr/bin/git",
        "/bin/git",
    ];
    for c in candidates {
        let p = Path::new(c);
        if git_ai::config::is_real_git_candidate(p) {
            return c.to_string();
        }
    }

    // Last resort: rely on PATH (will fail if only git-ai is on PATH, but
    // that scenario is caught by other guards).
    "git".to_string()
}

/// Redirect this test binary's own HOME to an isolated temp directory.
///
/// This must run before any code reads HOME, which is why it is called at the
/// top of both `real_git_executable()` and `new_with_daemon_scope()`.
/// The `OnceLock` guarantees the init runs exactly once even under parallel tests.
///
/// After this call:
/// - `~/.git-ai/config.json` in the isolated HOME has `git_path` → real git,
///   so no daemon auto-spawn from in-process Config::get() calls.
/// - `~/.gitconfig` is a minimal stub so plain git subprocesses don't fail.
/// - Developer's real `~/.git-ai/`, `~/.claude/`, `~/.gitconfig` are unreachable.
pub(super) fn ensure_isolated_process_home() {
    static PROCESS_HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    PROCESS_HOME.get_or_init(|| {
        let home = std::env::temp_dir().join(format!("git-ai-test-home-{}", std::process::id()));

        fs::create_dir_all(&home).expect("create isolated process HOME");

        // Minimal ~/.gitconfig so plain git subprocesses work
        fs::write(
            home.join(".gitconfig"),
            "[user]\n\tname = Test User\n\temail = test@example.com\n",
        )
        .expect("write test .gitconfig");

        // Probe for real git before we overwrite HOME
        let real_git = find_real_git_by_probe();

        // Minimal ~/.git-ai/config.json: real git_path
        let git_ai_dir = home.join(".git-ai");
        fs::create_dir_all(&git_ai_dir).expect("create .git-ai dir");
        // Escape backslashes for JSON (relevant on Windows)
        let real_git_json = real_git.replace('\\', "\\\\");
        fs::write(
            git_ai_dir.join("config.json"),
            format!(r#"{{"git_path":"{real_git_json}"}}"#),
        )
        .expect("write test git-ai config");

        // Set a process-level test DB marker so that any in-process git-ai code
        // (e.g., `CiContext` in the `ci_fork_notes` tests) treats the test
        // harness as a test environment and does not run background-agent
        // detection like `/opt/.devin`.
        let process_test_db = home.join("git-ai-test-db");
        fs::create_dir_all(&process_test_db).expect("create process test DB dir");

        // SAFETY: called once via OnceLock before any parallel test thread reads
        // HOME or PATH. The OnceLock ensures no concurrent env var writes.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("GIT_AI_TEST_DB_PATH", &process_test_db);
            #[cfg(windows)]
            {
                std::env::set_var("USERPROFILE", &home);
                std::env::set_var("HOMEDRIVE", "");
                std::env::set_var("HOMEPATH", "");
            }

            // Sanitize the process-level PATH to remove git-ai wrapper directories.
            // This covers subprocess calls that don't go through configure_test_home_env
            // (e.g., template repo init, bare repo init, worktree setup), preventing
            // git internals from resolving `git` via PATH to the installed git-ai
            // release binary (which would spawn daemons).
            #[cfg(not(windows))]
            if let Ok(path) = std::env::var("PATH") {
                let sanitized = path
                    .split(':')
                    .filter(|dir| {
                        let git_path = std::path::Path::new(dir).join("git");
                        if git_path.is_file() || git_path.is_symlink() {
                            if let Ok(contents) = fs::read_to_string(&git_path)
                                && contents.contains("git-ai")
                            {
                                return false;
                            }
                            if let Ok(target) = std::fs::read_link(&git_path)
                                && target.to_string_lossy().contains("git-ai")
                            {
                                return false;
                            }
                            if let Ok(canonical) = git_path.canonicalize()
                                && canonical.to_string_lossy().contains("git-ai")
                            {
                                return false;
                            }
                        }
                        true
                    })
                    .collect::<Vec<_>>()
                    .join(":");
                std::env::set_var("PATH", sanitized);
            }
        }
        home
    });
}

pub(crate) fn real_git_executable() -> &'static str {
    // Ensure HOME is isolated before Config::get() caches HOME-derived paths.
    ensure_isolated_process_home();
    git_ai::config::Config::get().git_cmd()
}

/// Create a pre-initialized template repo (cached across all tests in the process).
/// Subsequent calls to `clone_template_to()` copy this instead of running git init.
pub(super) fn init_template_repo() -> PathBuf {
    let path = std::env::temp_dir().join(format!("git-ai-test-template-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);

    let p = path.to_str().unwrap();
    let git = real_git_executable();

    let mut command = Command::new(git);
    command.args(["init", p]);
    let output = run_command_output(&mut command, "init template repo")
        .expect("failed to init template repo");
    assert!(output.status.success(), "template git init failed");

    for args in [
        vec!["-C", p, "config", "user.name", "Test User"],
        vec!["-C", p, "config", "user.email", "test@example.com"],
        vec!["-C", p, "symbolic-ref", "HEAD", "refs/heads/main"],
    ] {
        let mut command = Command::new(git);
        command.args(&args);
        let output = run_command_output(&mut command, "configure template repo")
            .expect("failed to configure template repo");
        assert!(
            output.status.success(),
            "template config failed: {:?}",
            args
        );
    }

    path
}

pub(super) fn init_bare_template_repo() -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("git-ai-test-template-bare-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);

    let p = path.to_str().unwrap();
    let git = real_git_executable();

    let mut command = Command::new(git);
    command.args(["init", "--bare", p]);
    let output = run_command_output(&mut command, "init bare template repo")
        .expect("failed to init bare template repo");
    assert!(output.status.success(), "bare template git init failed");

    let mut command = Command::new(git);
    command.args(["-C", p, "symbolic-ref", "HEAD", "refs/heads/main"]);
    let output = run_command_output(&mut command, "set HEAD in bare template")
        .expect("failed to set HEAD in bare template");
    assert!(output.status.success());

    path
}

pub(super) fn copy_dir_recursive(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Clone the cached template repo to a new destination path.
pub(super) fn clone_template_to(dest: &std::path::Path) {
    let template = TEMPLATE_REPO.get_or_init(init_template_repo);
    copy_dir_recursive(template, dest).expect("failed to copy template repo");
}

/// Clone the cached bare template repo to a new destination path.
pub(super) fn clone_bare_template_to(dest: &std::path::Path) {
    let template = TEMPLATE_BARE_REPO.get_or_init(init_bare_template_repo);
    copy_dir_recursive(template, dest).expect("failed to copy bare template repo");
}

/// Set user.name and user.email on a repo using git CLI (no git2 needed).
pub(super) fn set_repo_user_config(repo_path: &std::path::Path) {
    let p = repo_path.to_str().unwrap();
    let git = real_git_executable();
    for args in [
        vec!["-C", p, "config", "user.name", "Test User"],
        vec!["-C", p, "config", "user.email", "test@example.com"],
    ] {
        let mut command = Command::new(git);
        command.args(&args);
        let output = run_command_output(&mut command, "set repo user config")
            .expect("failed to set user config");
        assert!(output.status.success());
    }
}

pub(super) fn get_default_branch_name() -> String {
    // Since TestRepo::new() explicitly sets the default branch to "main" via symbolic-ref,
    // we always return "main" to match that behavior and ensure test consistency across
    // different Git versions and configurations.
    "main".to_string()
}

pub fn default_branchname() -> &'static str {
    DEFAULT_BRANCH_NAME.get_or_init(get_default_branch_name)
}

pub(super) fn compile_binary() -> PathBuf {
    if let Ok(override_path) = std::env::var("GIT_AI_TEST_BINARY_PATH") {
        let path = PathBuf::from(override_path);
        if path.is_file() {
            println!(
                "Using prebuilt git-ai test binary from GIT_AI_TEST_BINARY_PATH: {}",
                path.display()
            );
            return path;
        }
        panic!(
            "GIT_AI_TEST_BINARY_PATH does not point to a file: {}",
            path.display()
        );
    }

    println!("Compiling git-ai binary for tests...");

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let output = Command::new("cargo")
        .args(["build", "--bin", "git-ai", "--features", "test-support"])
        .current_dir(manifest_dir)
        .output()
        .expect("Failed to compile git-ai binary");

    if !output.status.success() {
        panic!(
            "Failed to compile git-ai:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Respect CARGO_TARGET_DIR if set, otherwise fall back to manifest-relative target/
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| {
        PathBuf::from(manifest_dir)
            .join("target")
            .to_string_lossy()
            .into_owned()
    });
    #[cfg(windows)]
    let binary_path = PathBuf::from(&target_dir).join("debug/git-ai.exe");
    #[cfg(not(windows))]
    let binary_path = PathBuf::from(&target_dir).join("debug/git-ai");

    // Warm the freshly built binary once so the first daemon startups in highly parallel
    // suites don't all pay cold process initialization overhead at the same time.
    let _ = Command::new(&binary_path).arg("--version").output();

    binary_path
}

pub fn get_binary_path() -> &'static PathBuf {
    COMPILED_BINARY.get_or_init(compile_binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_isolated_process_home_controls_git_ai_internal_dir() {
        ensure_isolated_process_home();

        let home = PathBuf::from(std::env::var("HOME").expect("HOME should be isolated"));

        #[cfg(windows)]
        {
            assert_eq!(
                std::env::var_os("USERPROFILE").map(PathBuf::from),
                Some(home.clone()),
                "Windows home lookup prefers USERPROFILE, so the test harness must isolate it"
            );
            assert_eq!(
                std::env::var("HOMEDRIVE").unwrap_or_default(),
                "",
                "HOMEDRIVE should not point git-ai back at the real user profile"
            );
            assert_eq!(
                std::env::var("HOMEPATH").unwrap_or_default(),
                "",
                "HOMEPATH should not point git-ai back at the real user profile"
            );
        }

        assert_eq!(
            git_ai::config::internal_dir_path().expect("internal dir should resolve"),
            home.join(".git-ai").join("internal"),
            "in-process git-ai config lookup must use the isolated test home"
        );
    }
}
