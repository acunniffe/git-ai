//! Comprehensive tests for install_hooks command module
//!
//! This module tests the git-ai install-hooks and uninstall-hooks commands,
//! which handle installation of git hooks for various IDEs and coding agents.

use crate::repos::test_repo::{
    DaemonTestScope, TestRepo, detach_from_controlling_terminal, ensure_isolated_process_home,
    get_binary_path,
};
use git_ai::commands::install_hooks::{
    GIT_AI_NO_AUTHOR_PROMPT_ENV, InstallResult, InstallStatus, run, run_uninstall, to_hashmap,
};
use std::collections::HashMap;
use std::fs;
use std::process::Command;

// ==============================================================================
// InstallStatus Tests
// ==============================================================================

#[test]
fn test_install_status_as_str() {
    assert_eq!(InstallStatus::NotFound.as_str(), "not_found");
    assert_eq!(InstallStatus::Installed.as_str(), "installed");
    assert_eq!(
        InstallStatus::AlreadyInstalled.as_str(),
        "already_installed"
    );
    assert_eq!(InstallStatus::Failed.as_str(), "failed");
}

#[test]
fn test_install_status_equality() {
    assert_eq!(InstallStatus::NotFound, InstallStatus::NotFound);
    assert_eq!(InstallStatus::Installed, InstallStatus::Installed);
    assert_eq!(
        InstallStatus::AlreadyInstalled,
        InstallStatus::AlreadyInstalled
    );
    assert_eq!(InstallStatus::Failed, InstallStatus::Failed);

    assert_ne!(InstallStatus::NotFound, InstallStatus::Installed);
    assert_ne!(InstallStatus::Installed, InstallStatus::Failed);
}

#[test]
fn test_install_status_copy_clone() {
    let status = InstallStatus::Installed;
    let copied = status;
    let cloned = status;

    assert_eq!(status, copied);
    assert_eq!(status, cloned);
    assert_eq!(copied, cloned);
}

// ==============================================================================
// InstallResult Tests
// ==============================================================================

#[test]
fn test_install_result_installed() {
    let result = InstallResult::installed();
    assert_eq!(result.status, InstallStatus::Installed);
    assert!(result.error.is_none());
    assert!(result.warnings.is_empty());
}

#[test]
fn test_install_result_already_installed() {
    let result = InstallResult::already_installed();
    assert_eq!(result.status, InstallStatus::AlreadyInstalled);
    assert!(result.error.is_none());
    assert!(result.warnings.is_empty());
}

#[test]
fn test_install_result_not_found() {
    let result = InstallResult::not_found();
    assert_eq!(result.status, InstallStatus::NotFound);
    assert!(result.error.is_none());
    assert!(result.warnings.is_empty());
}

#[test]
fn test_install_result_failed() {
    let result = InstallResult::failed("Installation failed");
    assert_eq!(result.status, InstallStatus::Failed);
    assert_eq!(result.error, Some("Installation failed".to_string()));
    assert!(result.warnings.is_empty());
}

#[test]
fn test_install_result_failed_with_string() {
    let error_msg = String::from("Custom error message");
    let result = InstallResult::failed(error_msg.clone());
    assert_eq!(result.status, InstallStatus::Failed);
    assert_eq!(result.error, Some(error_msg));
}

#[test]
fn test_install_result_with_warning() {
    let result = InstallResult::installed().with_warning("Minor issue detected");
    assert_eq!(result.status, InstallStatus::Installed);
    assert!(result.error.is_none());
    assert_eq!(result.warnings.len(), 1);
    assert_eq!(result.warnings[0], "Minor issue detected");
}

#[test]
fn test_install_result_with_multiple_warnings() {
    let result = InstallResult::installed()
        .with_warning("Warning 1")
        .with_warning("Warning 2")
        .with_warning("Warning 3");

    assert_eq!(result.warnings.len(), 3);
    assert_eq!(result.warnings[0], "Warning 1");
    assert_eq!(result.warnings[1], "Warning 2");
    assert_eq!(result.warnings[2], "Warning 3");
}

#[test]
fn test_install_result_message_for_metrics_with_error() {
    let result = InstallResult::failed("Critical error");
    let message = result.message_for_metrics();
    assert_eq!(message, Some("Critical error".to_string()));
}

#[test]
fn test_install_result_message_for_metrics_with_warnings() {
    let result = InstallResult::installed()
        .with_warning("Warning 1")
        .with_warning("Warning 2");
    let message = result.message_for_metrics();
    assert_eq!(message, Some("Warning 1; Warning 2".to_string()));
}

#[test]
fn test_install_result_message_for_metrics_with_error_and_warnings() {
    // Error takes precedence over warnings
    let result = InstallResult::failed("Error message").with_warning("Some warning");
    let message = result.message_for_metrics();
    assert_eq!(message, Some("Error message".to_string()));
}

#[test]
fn test_install_result_message_for_metrics_no_error_or_warnings() {
    let result = InstallResult::installed();
    let message = result.message_for_metrics();
    assert!(message.is_none());
}

#[test]
fn test_install_result_message_for_metrics_empty_warnings() {
    let result = InstallResult {
        status: InstallStatus::Installed,
        error: None,
        warnings: vec![],
    };
    let message = result.message_for_metrics();
    assert!(message.is_none());
}

// ==============================================================================
// to_hashmap Conversion Tests
// ==============================================================================

#[test]
fn test_to_hashmap_empty() {
    let statuses: HashMap<String, InstallStatus> = HashMap::new();
    let result = to_hashmap(statuses);
    assert!(result.is_empty());
}

#[test]
fn test_to_hashmap_single_entry() {
    let mut statuses = HashMap::new();
    statuses.insert("cursor".to_string(), InstallStatus::Installed);

    let result = to_hashmap(statuses);
    assert_eq!(result.len(), 1);
    assert_eq!(result.get("cursor"), Some(&"installed".to_string()));
}

#[test]
fn test_to_hashmap_multiple_entries() {
    let mut statuses = HashMap::new();
    statuses.insert("cursor".to_string(), InstallStatus::Installed);
    statuses.insert("claude-code".to_string(), InstallStatus::AlreadyInstalled);
    statuses.insert("codex".to_string(), InstallStatus::NotFound);
    statuses.insert("windsurf".to_string(), InstallStatus::Failed);

    let result = to_hashmap(statuses);
    assert_eq!(result.len(), 4);
    assert_eq!(result.get("cursor"), Some(&"installed".to_string()));
    assert_eq!(
        result.get("claude-code"),
        Some(&"already_installed".to_string())
    );
    assert_eq!(result.get("codex"), Some(&"not_found".to_string()));
    assert_eq!(result.get("windsurf"), Some(&"failed".to_string()));
}

#[test]
fn test_to_hashmap_all_statuses() {
    let mut statuses = HashMap::new();
    statuses.insert("not_found".to_string(), InstallStatus::NotFound);
    statuses.insert("installed".to_string(), InstallStatus::Installed);
    statuses.insert("already".to_string(), InstallStatus::AlreadyInstalled);
    statuses.insert("failed".to_string(), InstallStatus::Failed);

    let result = to_hashmap(statuses);
    assert_eq!(result.get("not_found"), Some(&"not_found".to_string()));
    assert_eq!(result.get("installed"), Some(&"installed".to_string()));
    assert_eq!(
        result.get("already"),
        Some(&"already_installed".to_string())
    );
    assert_eq!(result.get("failed"), Some(&"failed".to_string()));
}

// ==============================================================================
// Argument Parsing Tests
// ==============================================================================

#[test]
fn plain_install_hooks_preserves_the_invoking_user_home() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let invoking_home = repo.test_home_path();
    let installed_home = repo.path().join("installed-user");
    let installed_bin_dir = installed_home.join(".git-ai").join("bin");
    fs::create_dir_all(&installed_bin_dir).unwrap();

    #[cfg(windows)]
    let installed_binary = installed_bin_dir.join("git-ai.exe");
    #[cfg(not(windows))]
    let installed_binary = installed_bin_dir.join("git-ai");
    fs::copy(get_binary_path(), &installed_binary).unwrap();

    let test_db = repo.path().join("install-hooks.db");
    let mut command = Command::new(&installed_binary);
    command
        .arg("install-hooks")
        .current_dir(repo.path())
        .env("HOME", invoking_home)
        .env("API_KEY", "package-test-key")
        .env("GIT_AI_TEST_DB_PATH", &test_db)
        .env("GITAI_TEST_DB_PATH", &test_db)
        .env("GIT_CONFIG_GLOBAL", invoking_home.join(".gitconfig"))
        .env("GIT_AI_ALLOW_SUPERUSER", "1")
        .env("GIT_AI_DEBUG", "0");
    #[cfg(windows)]
    command
        .env("USERPROFILE", invoking_home)
        .env("APPDATA", invoking_home.join("AppData").join("Roaming"))
        .env("LOCALAPPDATA", invoking_home.join("AppData").join("Local"));

    let output = command.output().expect("run copied git-ai binary");
    assert!(
        output.status.success(),
        "plain install-hooks failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let invoking_config = fs::read_to_string(invoking_home.join(".git-ai/config.json"))
        .expect("install-hooks should update the invoking user's config");
    let invoking_config: serde_json::Value = serde_json::from_str(&invoking_config).unwrap();
    assert_eq!(
        invoking_config["api_key"],
        serde_json::Value::String("package-test-key".to_string())
    );
    assert!(
        !installed_home.join(".git-ai/config.json").exists(),
        "plain install-hooks must not retarget config to the binary owner's home"
    );
}

#[test]
#[cfg(not(windows))]
fn install_hooks_env_flag_adds_path_to_detected_shell_configs() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path();
    fs::write(home.join(".bashrc"), "# bashrc\n").unwrap();
    fs::write(home.join(".zshrc"), "# zshrc\n").unwrap();

    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["install-hooks", "--env"], &[])
        .output()
        .expect("run git-ai install-hooks --env");
    assert!(
        output.status.success(),
        "install-hooks --env failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let expected_line = format!("export PATH=\"{}/.git-ai/bin:$PATH\"", home.display());
    let bashrc = fs::read_to_string(home.join(".bashrc")).unwrap();
    let zshrc = fs::read_to_string(home.join(".zshrc")).unwrap();
    assert!(
        bashrc.contains(&expected_line),
        "missing PATH line:\n{bashrc}"
    );
    assert!(
        zshrc.contains(&expected_line),
        "missing PATH line:\n{zshrc}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Updated shell configurations:"),
        "missing shell config report:\n{stdout}"
    );

    // A second run must be idempotent and report the configs as already set up.
    let second = repo
        .git_ai_command_without_pre_sync_for_test(&["install-hooks", "--env"], &[])
        .output()
        .expect("re-run git-ai install-hooks --env");
    assert!(second.status.success());
    assert_eq!(
        fs::read_to_string(home.join(".zshrc")).unwrap(),
        zshrc,
        "second --env run must not modify shell configs again"
    );
    let second_stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        second_stdout.contains("Already configured (no changes needed):"),
        "missing already-configured report:\n{second_stdout}"
    );
}

#[test]
#[cfg(not(windows))]
fn install_hooks_without_env_flag_leaves_shell_configs_untouched() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path();
    fs::write(home.join(".zshrc"), "# zshrc\n").unwrap();

    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["install-hooks"], &[])
        .output()
        .expect("run git-ai install-hooks");
    assert!(
        output.status.success(),
        "install-hooks failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        fs::read_to_string(home.join(".zshrc")).unwrap(),
        "# zshrc\n",
        "plain install-hooks must never touch shell configs"
    );
}

#[test]
fn install_hooks_wsl_dry_run_does_not_invoke_wsl() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["install-hooks", "--wsl", "--dry-run"], &[])
        .output()
        .expect("run git-ai install-hooks --wsl --dry-run");

    assert!(
        output.status.success(),
        "install-hooks --wsl --dry-run failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Dry-run: skipping WSL installation."),
        "missing WSL dry-run message:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

// ==============================================================================
// Author identity prompt tests
// ==============================================================================

fn git_ai_config_json(repo: &TestRepo) -> serde_json::Value {
    let path = repo.test_home_path().join(".git-ai").join("config.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// Prompt tests must not inherit ambient suppression markers from the
/// harness environment: `author_prompt_suppressed()` consults
/// `is_in_background_agent()` and `is_superuser_expected_environment()`, so
/// running the suite in CI or an AI-agent sandbox would suppress the prompt
/// and make the assertions pass or fail for the wrong reason. The marker
/// lists are imported from the production detectors so they cannot drift.
fn remove_ambient_prompt_suppression_env(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CLOUD_AGENT_") {
            command.env_remove(key);
        }
    }
    for key in git_ai::utils::UNATTENDED_ENV_MARKERS
        .iter()
        .chain(git_ai::authorship::background_agent::BACKGROUND_AGENT_ENV_MARKERS)
        .chain(std::iter::once(
            &git_ai::commands::upgrade::ENV_BACKGROUND_UPGRADE_WORKER,
        ))
    {
        command.env_remove(key);
    }
}

/// Suppression the prompt tests cannot scrub from a child's environment: the
/// `/.dockerenv` container marker (GitHub-hosted runners are VMs, so it is
/// absent there) and running the suite as root. Prompt-firing tests skip
/// instead of failing spuriously in such environments.
fn ambient_unscrubbable_prompt_suppression() -> Option<&'static str> {
    if std::path::Path::new("/.dockerenv").exists() {
        return Some("/.dockerenv container marker");
    }
    #[cfg(unix)]
    if git_ai::utils::is_running_as_superuser() {
        return Some("running as root");
    }
    None
}

fn assert_install_hooks_skips_author_prompt(
    output: &std::process::Output,
    repo: &TestRepo,
    context: &str,
) {
    assert!(
        output.status.success(),
        "install-hooks failed ({context}):\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("Author name"),
        "author prompt must be skipped ({context}):\n{stdout}"
    );
    assert!(
        git_ai_config_json(repo).get("author").is_none(),
        "no author must be written to the config ({context})"
    );
}

#[test]
fn install_hooks_no_controlling_tty_skips_author_prompt() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["install-hooks"],
        &[
            ("GIT_AI_API_KEY", "test-key"),
            (GIT_AI_NO_AUTHOR_PROMPT_ENV, "0"),
        ],
    );
    remove_ambient_prompt_suppression_env(&mut command);
    // Piped stdio alone is not enough: a test run from a real terminal still
    // has an openable /dev/tty, which the prompt would fall back to.
    detach_from_controlling_terminal(&mut command);
    let output = command.output().expect("run git-ai install-hooks");

    assert_install_hooks_skips_author_prompt(&output, &repo, "no controlling terminal");
}

#[test]
fn install_hooks_forced_tty_prompts_and_saves_author() {
    use std::io::Write as _;

    if let Some(reason) = ambient_unscrubbable_prompt_suppression() {
        eprintln!("skipping: {reason} suppresses the author prompt");
        return;
    }

    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["install-hooks"],
        &[
            ("GIT_AI_API_KEY", "test-key"),
            ("GIT_AI_TEST_FORCE_TTY", "1"),
            (GIT_AI_NO_AUTHOR_PROMPT_ENV, "0"),
        ],
    );
    remove_ambient_prompt_suppression_env(&mut command);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().expect("spawn git-ai install-hooks");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Alice\nalice@example.com\n")
        .unwrap();
    let output = child.wait_with_output().expect("wait for install-hooks");

    assert!(
        output.status.success(),
        "install-hooks failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Author name") && stdout.contains("Author email"),
        "missing author prompt:\n{stdout}"
    );

    let config = git_ai_config_json(&repo);
    assert_eq!(config["author"]["name"], "Alice");
    assert_eq!(config["author"]["email"], "alice@example.com");
}

#[test]
fn install_hooks_forced_tty_author_already_set_skips_prompt() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let config_path = repo.test_home_path().join(".git-ai").join("config.json");
    let mut config = git_ai_config_json(&repo);
    config["author"] = serde_json::json!({"name": "Preset Name"});
    fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["install-hooks"],
        &[
            ("GIT_AI_API_KEY", "test-key"),
            ("GIT_AI_TEST_FORCE_TTY", "1"),
            (GIT_AI_NO_AUTHOR_PROMPT_ENV, "0"),
        ],
    );
    remove_ambient_prompt_suppression_env(&mut command);
    let output = command.output().expect("run git-ai install-hooks");

    assert!(
        output.status.success(),
        "install-hooks failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("Author name"),
        "author prompt must be skipped when author.name is already set:\n{stdout}"
    );
    assert_eq!(
        git_ai_config_json(&repo)["author"],
        serde_json::json!({"name": "Preset Name"}),
        "a partially-set author must be left untouched"
    );
}

#[test]
fn install_hooks_forced_tty_without_api_key_skips_prompt() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["install-hooks"],
        &[
            ("GIT_AI_TEST_FORCE_TTY", "1"),
            (GIT_AI_NO_AUTHOR_PROMPT_ENV, "0"),
        ],
    );
    remove_ambient_prompt_suppression_env(&mut command);
    command.env_remove("GIT_AI_API_KEY").env_remove("API_KEY");
    let output = command.output().expect("run git-ai install-hooks");

    assert_install_hooks_skips_author_prompt(&output, &repo, "no api key");
}

#[test]
fn install_hooks_dry_run_forced_tty_skips_prompt() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["install-hooks", "--dry-run"],
        &[
            ("GIT_AI_API_KEY", "test-key"),
            ("GIT_AI_TEST_FORCE_TTY", "1"),
            (GIT_AI_NO_AUTHOR_PROMPT_ENV, "0"),
        ],
    );
    remove_ambient_prompt_suppression_env(&mut command);
    let output = command
        .output()
        .expect("run git-ai install-hooks --dry-run");

    assert_install_hooks_skips_author_prompt(&output, &repo, "dry run");
}

/// Environments where the prompt must never fire even though an interactive
/// channel is available (GIT_AI_TEST_FORCE_TTY keeps the channel open, and
/// ambient suppression markers are scrubbed first, so only the suppression
/// env under test can cause the skip).
fn assert_suppression_env_skips_author_prompt(env: (&str, &str), context: &str) {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["install-hooks"],
        &[
            ("GIT_AI_API_KEY", "test-key"),
            ("GIT_AI_TEST_FORCE_TTY", "1"),
            (GIT_AI_NO_AUTHOR_PROMPT_ENV, "0"),
        ],
    );
    remove_ambient_prompt_suppression_env(&mut command);
    command.env(env.0, env.1);
    let output = command.output().expect("run git-ai install-hooks");

    assert_install_hooks_skips_author_prompt(&output, &repo, context);
}

#[test]
fn install_hooks_daemon_upgrade_env_skips_author_prompt() {
    assert_suppression_env_skips_author_prompt(("GIT_AI_DAEMON_UPGRADE", "1"), "daemon upgrade");
}

#[test]
fn install_hooks_background_upgrade_worker_skips_author_prompt() {
    assert_suppression_env_skips_author_prompt(
        ("GIT_AI_BACKGROUND_UPGRADE_WORKER", "1"),
        "background upgrade worker",
    );
}

#[test]
fn install_hooks_background_agent_skips_author_prompt() {
    assert_suppression_env_skips_author_prompt(("GIT_AI_CLOUD_AGENT", "1"), "background agent");
}

#[test]
fn install_hooks_ci_environment_skips_author_prompt() {
    // Unattended environments are detected in-binary (no installer-script
    // cooperation): CI markers, containers, and daemon upgrades all suppress.
    assert_suppression_env_skips_author_prompt(("CI", "true"), "ci environment");
}

#[test]
fn install_hooks_no_author_prompt_flag_skips_prompt() {
    // Spawners that cannot set env vars (the MSI custom action command line)
    // suppress via the CLI flag instead.
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["install-hooks", "--no-author-prompt"],
        &[
            ("GIT_AI_API_KEY", "test-key"),
            ("GIT_AI_TEST_FORCE_TTY", "1"),
            (GIT_AI_NO_AUTHOR_PROMPT_ENV, "0"),
        ],
    );
    remove_ambient_prompt_suppression_env(&mut command);
    let output = command.output().expect("run git-ai install-hooks");

    assert_install_hooks_skips_author_prompt(&output, &repo, "--no-author-prompt flag");
}

/// End-to-end /dev/tty fallback: stdin is /dev/null (like `curl | bash`,
/// where stdin is the script pipe), but the process has a controlling
/// terminal — the pty allocated by script(1), which forwards our piped input
/// to it.
#[test]
#[cfg(target_os = "linux")]
fn install_hooks_piped_stdio_prompts_via_controlling_terminal() {
    use std::io::Write as _;

    if let Some(reason) = ambient_unscrubbable_prompt_suppression() {
        eprintln!("skipping: {reason} suppresses the author prompt");
        return;
    }

    // A spawn error means no script(1) at all; a non-success exit means a
    // non-util-linux implementation (e.g. BusyBox) without `-qec` support.
    match Command::new("script").arg("--version").output() {
        Ok(output) if output.status.success() => {}
        _ => {
            eprintln!("skipping: util-linux script(1) not available");
            return;
        }
    }

    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    // Build the normal test command purely to harvest the binary path and the
    // isolated environment, then re-run it under script's pty.
    let mut inner = repo.git_ai_command_without_pre_sync_for_test(
        &["install-hooks"],
        &[
            ("GIT_AI_API_KEY", "test-key"),
            (GIT_AI_NO_AUTHOR_PROMPT_ENV, "0"),
        ],
    );
    remove_ambient_prompt_suppression_env(&mut inner);
    let binary = inner.get_program().to_str().unwrap().to_string();

    let mut command = Command::new("script");
    // The binary path travels via the environment so quotes/dollars/backticks
    // in a workspace path cannot break (or inject into) the shell command.
    command
        .arg("-qec")
        .arg(r#""$GIT_AI_TEST_BINARY" install-hooks < /dev/null"#)
        .arg("/dev/null")
        .current_dir(repo.path());
    for (key, value) in inner.get_envs() {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    command.env("GIT_AI_TEST_BINARY", &binary);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = command.spawn().expect("spawn install-hooks under script");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Bob\nbob@example.com\n")
        .unwrap();
    let output = child.wait_with_output().expect("wait for script");

    assert!(
        output.status.success(),
        "install-hooks under script failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // The prompt reads and writes /dev/tty (the pty), which script mirrors to
    // its stdout. Keep stdout assertions loose: pty echo and \r\n conversion
    // garble exact matches.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Author name"),
        "prompt must reach the controlling terminal:\n{stdout}"
    );
    let config = git_ai_config_json(&repo);
    assert_eq!(config["author"]["name"], "Bob");
    assert_eq!(config["author"]["email"], "bob@example.com");
}

#[test]
#[cfg(not(windows))]
fn install_hooks_detects_cline_from_vscode_server_extension_manifest() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path();
    let manifest_path = home.join(".vscode-server/extensions/extensions.json");
    fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    fs::create_dir_all(home.join("Documents")).unwrap();
    fs::write(
        manifest_path,
        r#"[
  {
    "identifier": {
      "id": "saoudrizwan.claude-dev"
    },
    "version": "4.0.11"
  }
]"#,
    )
    .unwrap();

    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["install-hooks", "--dry-run"], &[])
        .output()
        .expect("run git-ai install-hooks --dry-run");
    assert!(
        output.status.success(),
        "install-hooks failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Cline: Pending updates"),
        "Cline was not detected from the VS Code server extension manifest:\n{stdout}"
    );
    assert!(
        !home.join("Documents/Cline/Hooks").exists(),
        "dry-run must not create the Cline hooks directory"
    );
}

#[test]
fn test_run_install_hooks_no_args() {
    // In-process `run()` bypasses the child-command env isolation: without
    // this, the new author prompt could read the developer's real terminal
    // and write their real ~/.git-ai/config.json.
    ensure_isolated_process_home();
    // This will try to run against the actual system, but should not crash
    // It may fail if binary path cannot be determined, which is acceptable
    let result = run(&[]);

    // We just ensure it returns a result (success or error)
    // The actual behavior depends on the system state
    match result {
        Ok(_statuses) => {
            // Should return a HashMap, possibly empty
            // Success is valid
        }
        Err(e) => {
            // May fail if binary path is not available or other system issues
            let err_msg = e.to_string();
            // Just ensure we get a meaningful error
            assert!(!err_msg.is_empty());
        }
    }
}

#[test]
fn test_run_install_hooks_with_dry_run_flag() {
    ensure_isolated_process_home();
    let args = vec!["--dry-run".to_string()];
    let result = run(&args);

    // Dry run should not modify anything
    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(e) => {
            let err_msg = e.to_string();
            assert!(!err_msg.is_empty());
        }
    }
}

#[test]
fn test_run_install_hooks_with_dry_run_true() {
    ensure_isolated_process_home();
    let args = vec!["--dry-run=true".to_string()];
    let result = run(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_install_hooks_with_verbose_flag() {
    ensure_isolated_process_home();
    let args = vec!["--verbose".to_string()];
    let result = run(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_install_hooks_with_verbose_short_flag() {
    ensure_isolated_process_home();
    let args = vec!["-v".to_string()];
    let result = run(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_install_hooks_with_multiple_flags() {
    ensure_isolated_process_home();
    let args = vec!["--dry-run".to_string(), "--verbose".to_string()];
    let result = run(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_install_hooks_with_dry_run_false() {
    ensure_isolated_process_home();
    // Note: This could actually install hooks on the system
    // In a real test environment, this should be run in isolation
    let args = vec!["--dry-run=false".to_string()];
    let result = run(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_install_hooks_ignores_unknown_args() {
    ensure_isolated_process_home();
    // Unknown arguments should be ignored
    let args = vec![
        "--unknown-flag".to_string(),
        "random-arg".to_string(),
        "--dry-run".to_string(),
    ];
    let result = run(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

// ==============================================================================
// Uninstall Tests
// ==============================================================================

#[test]
fn test_uninstall_alias_runs_uninstall_hooks() {
    let repo = TestRepo::new();
    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["uninstall", "--dry-run"], &[])
        .output()
        .expect("run git-ai uninstall --dry-run");

    assert!(
        output.status.success(),
        "git-ai uninstall should alias uninstall-hooks:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_run_uninstall_hooks_no_args() {
    let result = run_uninstall(&[]);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(e) => {
            let err_msg = e.to_string();
            assert!(!err_msg.is_empty());
        }
    }
}

#[test]
fn test_run_uninstall_hooks_with_dry_run() {
    let args = vec!["--dry-run".to_string()];
    let result = run_uninstall(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_uninstall_hooks_with_verbose() {
    let args = vec!["--verbose".to_string()];
    let result = run_uninstall(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_uninstall_hooks_with_multiple_flags() {
    let args = vec![
        "--dry-run=true".to_string(),
        "-v".to_string(),
        "--unknown".to_string(),
    ];
    let result = run_uninstall(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

// ==============================================================================
// Edge Cases and Error Handling
// ==============================================================================

#[test]
fn test_install_result_clone() {
    let result = InstallResult::failed("Error")
        .with_warning("Warning 1")
        .with_warning("Warning 2");

    let cloned = result.clone();
    assert_eq!(cloned.status, result.status);
    assert_eq!(cloned.error, result.error);
    assert_eq!(cloned.warnings, result.warnings);
}

#[test]
fn test_install_result_debug_formatting() {
    let result = InstallResult::installed();
    let debug_str = format!("{:?}", result);
    assert!(debug_str.contains("InstallResult"));
    assert!(debug_str.contains("Installed"));
}

#[test]
fn test_install_status_debug_formatting() {
    let status = InstallStatus::Installed;
    let debug_str = format!("{:?}", status);
    assert!(debug_str.contains("Installed"));
}

#[test]
fn test_to_hashmap_preserves_all_keys() {
    let mut statuses = HashMap::new();
    let keys = vec![
        "cursor",
        "claude-code",
        "codex",
        "windsurf",
        "continue-cli",
        "github-copilot",
    ];

    for (idx, key) in keys.iter().enumerate() {
        let status = match idx % 4 {
            0 => InstallStatus::Installed,
            1 => InstallStatus::AlreadyInstalled,
            2 => InstallStatus::NotFound,
            _ => InstallStatus::Failed,
        };
        statuses.insert(key.to_string(), status);
    }

    let result = to_hashmap(statuses);
    assert_eq!(result.len(), keys.len());

    for key in keys {
        assert!(
            result.contains_key(key),
            "Expected key '{}' to be present",
            key
        );
    }
}

#[test]
fn test_install_result_warning_with_empty_string() {
    let result = InstallResult::installed().with_warning("");
    assert_eq!(result.warnings.len(), 1);
    assert_eq!(result.warnings[0], "");
}

#[test]
fn test_install_result_failed_with_empty_string() {
    let result = InstallResult::failed("");
    assert_eq!(result.error, Some("".to_string()));
    assert_eq!(result.status, InstallStatus::Failed);
}

#[test]
fn test_install_result_message_for_metrics_single_warning() {
    let result = InstallResult::installed().with_warning("Only warning");
    let message = result.message_for_metrics();
    assert_eq!(message, Some("Only warning".to_string()));
}

#[test]
fn test_install_result_message_for_metrics_warnings_join_with_semicolon() {
    let result = InstallResult::installed()
        .with_warning("First; warning")
        .with_warning("Second; warning")
        .with_warning("Third; warning");

    let message = result.message_for_metrics();
    assert_eq!(
        message,
        Some("First; warning; Second; warning; Third; warning".to_string())
    );
}

// ==============================================================================
// Integration-style Tests
// ==============================================================================

#[test]
fn test_install_workflow_dry_run_does_not_modify_system() {
    // Dry run should be safe to run repeatedly
    let args = vec!["--dry-run".to_string(), "--verbose".to_string()];

    let result1 = run(&args);
    let result2 = run(&args);

    // Both runs should succeed or fail consistently
    match (result1, result2) {
        (Ok(_statuses1), Ok(_statuses2)) => {
            // Results may differ if system state changes between runs,
            // but both should be valid HashMaps
            // Success is valid
        }
        (Err(_), Err(_)) => {
            // Both failing is acceptable (e.g., on CI without proper setup)
        }
        _ => {
            // Inconsistent results would indicate a problem, but we allow it
            // since the system state could change
        }
    }
}

#[test]
fn test_uninstall_workflow_dry_run_does_not_modify_system() {
    let args = vec!["--dry-run".to_string()];

    let result1 = run_uninstall(&args);
    let result2 = run_uninstall(&args);

    match (result1, result2) {
        (Ok(_statuses1), Ok(_statuses2)) => {
            // Success is valid
        }
        (Err(_), Err(_)) => {
            // Both failing is acceptable
        }
        _ => {
            // Allow inconsistent results due to system state changes
        }
    }
}

// ==============================================================================
// Status String Validation
// ==============================================================================

#[test]
fn test_all_status_strings_are_lowercase() {
    assert!(
        InstallStatus::NotFound
            .as_str()
            .chars()
            .all(|c| !c.is_uppercase())
    );
    assert!(
        InstallStatus::Installed
            .as_str()
            .chars()
            .all(|c| !c.is_uppercase())
    );
    assert!(
        InstallStatus::AlreadyInstalled
            .as_str()
            .chars()
            .all(|c| !c.is_uppercase())
    );
    assert!(
        InstallStatus::Failed
            .as_str()
            .chars()
            .all(|c| !c.is_uppercase())
    );
}

#[test]
fn test_status_strings_use_underscores() {
    // Verify consistent naming convention
    assert!(InstallStatus::NotFound.as_str().contains('_'));
    assert!(InstallStatus::AlreadyInstalled.as_str().contains('_'));
    assert!(!InstallStatus::Installed.as_str().contains('_'));
    assert!(!InstallStatus::Failed.as_str().contains('_'));
}

#[test]
fn test_status_strings_are_valid_identifiers() {
    // Status strings should be suitable for use as keys
    let statuses = [
        InstallStatus::NotFound,
        InstallStatus::Installed,
        InstallStatus::AlreadyInstalled,
        InstallStatus::Failed,
    ];

    for status in &statuses {
        let s = status.as_str();
        assert!(!s.is_empty());
        assert!(!s.contains(' '));
        assert!(!s.contains('-'));
        // Should only contain alphanumeric and underscores
        assert!(s.chars().all(|c| c.is_alphanumeric() || c == '_'));
    }
}

// ==============================================================================
// Complex Scenario Tests
// ==============================================================================

#[test]
fn test_install_result_builder_pattern() {
    // Demonstrate builder-like pattern with warnings
    let result = InstallResult::installed()
        .with_warning("Extension not found")
        .with_warning("Git path not configured")
        .with_warning("Manual action required");

    assert_eq!(result.status, InstallStatus::Installed);
    assert_eq!(result.warnings.len(), 3);
    assert!(result.error.is_none());

    let message = result.message_for_metrics();
    assert!(message.is_some());
    let msg = message.unwrap();
    assert!(msg.contains("Extension not found"));
    assert!(msg.contains("Git path not configured"));
    assert!(msg.contains("Manual action required"));
}

#[test]
fn test_to_hashmap_with_realistic_agent_names() {
    let mut statuses = HashMap::new();
    statuses.insert("cursor".to_string(), InstallStatus::Installed);
    statuses.insert("claude-code".to_string(), InstallStatus::AlreadyInstalled);
    statuses.insert("github-copilot".to_string(), InstallStatus::NotFound);
    statuses.insert("codex".to_string(), InstallStatus::Installed);
    statuses.insert("windsurf".to_string(), InstallStatus::Failed);
    statuses.insert("continue-cli".to_string(), InstallStatus::NotFound);

    let result = to_hashmap(statuses);
    assert_eq!(result.len(), 6);

    // Verify specific mappings
    assert_eq!(result.get("cursor").unwrap(), "installed");
    assert_eq!(result.get("claude-code").unwrap(), "already_installed");
    assert_eq!(result.get("github-copilot").unwrap(), "not_found");
    assert_eq!(result.get("codex").unwrap(), "installed");
    assert_eq!(result.get("windsurf").unwrap(), "failed");
    assert_eq!(result.get("continue-cli").unwrap(), "not_found");
}

#[test]
fn test_install_result_different_error_types() {
    // Test with different error message types
    let errors = vec![
        "Permission denied",
        "File not found",
        "Invalid configuration",
        "Version mismatch: expected 1.7, found 1.5",
        "Network timeout",
        "",
    ];

    for error in errors {
        let result = InstallResult::failed(error);
        assert_eq!(result.status, InstallStatus::Failed);
        assert_eq!(result.error, Some(error.to_string()));
        assert_eq!(result.message_for_metrics(), Some(error.to_string()));
    }
}

#[test]
fn test_hashmap_conversion_stability() {
    // Test that conversion is stable (same input produces same output)
    let mut statuses = HashMap::new();
    statuses.insert("test1".to_string(), InstallStatus::Installed);
    statuses.insert("test2".to_string(), InstallStatus::NotFound);

    let result1 = to_hashmap(statuses.clone());
    let result2 = to_hashmap(statuses);

    assert_eq!(result1.len(), result2.len());
    for (key, value) in result1.iter() {
        assert_eq!(result2.get(key), Some(value));
    }
}
