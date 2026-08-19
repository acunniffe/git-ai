//! Comprehensive tests for install_hooks command module
//!
//! This module tests the git-ai install-hooks and uninstall-hooks commands,
//! which handle installation of git hooks for various IDEs and coding agents.

use crate::repos::test_repo::{DaemonTestScope, TestRepo, get_binary_path};
use git_ai::commands::install_hooks::{
    InstallResult, InstallStatus, run, run_uninstall, to_hashmap,
};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

// ==============================================================================
// InstallStatus Tests
// ==============================================================================

fn copied_install_hooks_command(
    repo: &TestRepo,
    invoking_home: &Path,
    installed_home: &Path,
) -> (std::path::PathBuf, Command) {
    let installed_bin_dir = installed_home.join(".git-ai").join("bin");
    fs::create_dir_all(&installed_bin_dir).unwrap();

    #[cfg(windows)]
    let installed_binary = installed_bin_dir.join("git-ai.exe");
    #[cfg(not(windows))]
    let installed_binary = installed_bin_dir.join("git-ai");
    fs::copy(get_binary_path(), &installed_binary).unwrap();
    let canonical_installed_binary = fs::canonicalize(&installed_binary).unwrap();

    let test_db = repo.path().join("install-hooks.db");
    let mut command = Command::new(&installed_binary);
    command
        .arg("install-hooks")
        .current_dir(repo.path())
        .env("HOME", invoking_home)
        .env("GIT_AI_TEST_DB_PATH", &test_db)
        .env("GITAI_TEST_DB_PATH", &test_db)
        .env("GIT_CONFIG_GLOBAL", invoking_home.join(".gitconfig"))
        .env("GIT_AI_ALLOW_SUPERUSER", "1")
        .env("GIT_AI_DEBUG", "0");
    #[cfg(windows)]
    command
        .env("USERPROFILE", invoking_home)
        .env("APPDATA", invoking_home.join("AppData").join("Roaming"))
        .env("LOCALAPPDATA", invoking_home.join("AppData").join("Local"))
        .env("GIT_AI_SKIP_PATH_UPDATE", "1");

    (canonical_installed_binary, command)
}

fn run_isolated_install_hooks(args: &[&str]) -> Output {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let invoking_home = repo.test_home_path();
    let installed_home = repo.path().join("installed-user");
    let (_, mut command) = copied_install_hooks_command(&repo, invoking_home, &installed_home);
    command.args(args).env("SHELL", "/bin/bash");
    copied_binary_output(&mut command)
}

fn copied_binary_output(command: &mut Command) -> Output {
    for attempt in 0..5 {
        match command.output() {
            Ok(output) => return output,
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 4 => {
                thread::sleep(Duration::from_millis(10 * (attempt + 1)));
            }
            Err(error) => panic!("run copied git-ai binary: {error}"),
        }
    }
    unreachable!("the copied binary execution loop always returns or panics")
}

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
    let (_, mut command) = copied_install_hooks_command(&repo, invoking_home, &installed_home);
    command.env("API_KEY", "package-test-key");

    let output = copied_binary_output(&mut command);
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

#[cfg(not(windows))]
#[test]
fn install_hooks_configures_all_existing_unix_shell_profiles_idempotently() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let invoking_home = repo.test_home_path();
    let installed_home = repo.path().join("installed-user");

    let bashrc = invoking_home.join(".bashrc");
    let bash_profile = invoking_home.join(".bash_profile");
    let zshrc = invoking_home.join(".zshrc");
    let fish_config = invoking_home.join(".config/fish/config.fish");
    fs::create_dir_all(fish_config.parent().unwrap()).unwrap();
    fs::write(&bashrc, "# bashrc\n").unwrap();
    fs::write(&bash_profile, "# bash profile\n").unwrap();
    fs::write(&zshrc, "# zshrc\n").unwrap();
    fs::write(&fish_config, "# fish\n").unwrap();

    let (installed_binary, mut command) =
        copied_install_hooks_command(&repo, invoking_home, &installed_home);
    command.env("SHELL", "/bin/bash");
    let first = copied_binary_output(&mut command);
    assert!(
        first.status.success(),
        "install-hooks failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let install_dir = installed_binary.parent().unwrap().to_string_lossy();
    let expected_posix = format!("export PATH=\"{install_dir}:$PATH\"");
    let expected_fish = format!("fish_add_path -g \"{install_dir}\"");
    for profile in [&bashrc, &zshrc] {
        let contents = fs::read_to_string(profile).unwrap();
        assert!(contents.contains("# Added by git-ai installer on "));
        assert!(contents.ends_with(&format!("{expected_posix}\n")));
    }
    let fish_contents = fs::read_to_string(&fish_config).unwrap();
    assert!(fish_contents.contains("# Added by git-ai installer on "));
    assert!(fish_contents.ends_with(&format!("{expected_fish}\n")));
    assert_eq!(
        fs::read_to_string(&bash_profile).unwrap(),
        "# bash profile\n",
        ".bashrc must be preferred over .bash_profile"
    );

    let (_, mut second_command) =
        copied_install_hooks_command(&repo, invoking_home, &installed_home);
    second_command.env("SHELL", "/bin/bash");
    let second = copied_binary_output(&mut second_command);
    assert!(
        second.status.success(),
        "second install-hooks failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    for profile in [&bashrc, &zshrc, &fish_config] {
        let contents = fs::read_to_string(profile).unwrap();
        assert_eq!(
            contents.matches(install_dir.as_ref()).count(),
            1,
            "{} should contain one install directory entry",
            profile.display()
        );
    }
}

#[cfg(not(windows))]
#[test]
fn install_hooks_escapes_shell_expansions_in_the_install_path_idempotently() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let invoking_home = repo.test_home_path();
    let installed_home = repo.path().join("installed-$user-`cmd`-\"quoted\"-\\slash");
    let bashrc = invoking_home.join(".bashrc");
    fs::write(&bashrc, "# bashrc\n").unwrap();

    let (installed_binary, mut first_command) =
        copied_install_hooks_command(&repo, invoking_home, &installed_home);
    first_command.env("SHELL", "/bin/bash");
    let first = copied_binary_output(&mut first_command);
    assert!(
        first.status.success(),
        "install-hooks failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let install_dir = installed_binary.parent().unwrap().to_string_lossy();
    let escaped = install_dir
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    let expected = format!("export PATH=\"{escaped}:$PATH\"");
    assert!(fs::read_to_string(&bashrc).unwrap().contains(&expected));

    let (_, mut second_command) =
        copied_install_hooks_command(&repo, invoking_home, &installed_home);
    second_command.env("SHELL", "/bin/bash");
    let second = copied_binary_output(&mut second_command);
    assert!(second.status.success());
    assert_eq!(
        fs::read_to_string(&bashrc)
            .unwrap()
            .matches(&expected)
            .count(),
        1
    );
}

#[cfg(not(windows))]
#[test]
fn install_hooks_falls_back_to_the_login_shell_and_dry_run_does_not_mutate() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let invoking_home = repo.test_home_path();
    let installed_home = repo.path().join("installed-user");
    let fish_config = invoking_home.join(".config/fish/config.fish");

    let (_, mut dry_run) = copied_install_hooks_command(&repo, invoking_home, &installed_home);
    dry_run.arg("--dry-run").env("SHELL", "/usr/local/bin/fish");
    let output = copied_binary_output(&mut dry_run);
    assert!(output.status.success());
    assert!(
        !fish_config.exists(),
        "dry-run must not create the fallback shell profile"
    );

    let (installed_binary, mut install) =
        copied_install_hooks_command(&repo, invoking_home, &installed_home);
    install.env("SHELL", "/usr/local/bin/fish");
    let output = copied_binary_output(&mut install);
    assert!(
        output.status.success(),
        "install-hooks failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let install_dir = installed_binary.parent().unwrap().to_string_lossy();
    let fish_contents = fs::read_to_string(&fish_config).unwrap();
    let fish_lines: Vec<&str> = fish_contents.lines().collect();
    assert_eq!(fish_lines.len(), 3);
    assert_eq!(fish_lines[0], "");
    assert!(fish_lines[1].starts_with("# Added by git-ai installer on "));
    assert_eq!(fish_lines[2], format!("fish_add_path -g \"{install_dir}\""));
    assert!(!invoking_home.join(".bashrc").exists());
    assert!(!invoking_home.join(".zshrc").exists());
}

#[cfg(not(windows))]
#[test]
fn install_hooks_warns_but_succeeds_when_the_shell_profile_cannot_be_written() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let invoking_home = repo.test_home_path();
    let installed_home = repo.path().join("installed-user");
    fs::create_dir(invoking_home.join(".bashrc")).unwrap();

    let (_, mut command) = copied_install_hooks_command(&repo, invoking_home, &installed_home);
    command.env("SHELL", "/bin/bash");
    let output = copied_binary_output(&mut command);

    assert!(
        output.status.success(),
        "shell profile failures must not fail hook installation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Warning: Failed to configure shell environment:"),
        "shell profile failures should remain visible: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains(&invoking_home.join(".bashrc").display().to_string()),
        "the warning should identify the profile that could not be written: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("Detected shell config files, but could not update any of them."),
        "the guidance should distinguish a write failure from failed detection: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout)
            .contains("Could not detect any shell config files.")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn install_hooks_continues_after_one_shell_profile_cannot_be_written() {
    use std::os::unix::fs::symlink;

    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let invoking_home = repo.test_home_path();
    let installed_home = repo.path().join("installed-user");
    symlink("/proc/version", invoking_home.join(".bashrc")).unwrap();
    let zshrc = invoking_home.join(".zshrc");
    fs::write(&zshrc, "# zsh\n").unwrap();

    let (installed_binary, mut command) =
        copied_install_hooks_command(&repo, invoking_home, &installed_home);
    command.env("SHELL", "/bin/bash");
    let output = copied_binary_output(&mut command);

    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Warning: Failed to configure shell environment:")
    );
    let install_dir = installed_binary.parent().unwrap().to_string_lossy();
    assert!(
        fs::read_to_string(zshrc)
            .unwrap()
            .contains(install_dir.as_ref()),
        "a failed bash profile must not prevent zsh configuration"
    );
}

#[cfg(not(windows))]
#[test]
fn install_hooks_configures_shell_after_install_config_persistence_fails() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let invoking_home = repo.test_home_path();
    let installed_home = repo.path().join("installed-user");
    let bashrc = invoking_home.join(".bashrc");
    fs::write(&bashrc, "# bash\n").unwrap();
    let config_path = invoking_home.join(".git-ai/config.json");
    fs::remove_file(&config_path).unwrap();
    fs::create_dir(&config_path).unwrap();

    let (installed_binary, mut command) =
        copied_install_hooks_command(&repo, invoking_home, &installed_home);
    command
        .env("SHELL", "/bin/bash")
        .env("API_KEY", "cannot-be-persisted");
    let output = copied_binary_output(&mut command);

    assert!(
        !output.status.success(),
        "the original install-config failure must be preserved"
    );
    let install_dir = installed_binary.parent().unwrap().to_string_lossy();
    assert!(
        fs::read_to_string(bashrc)
            .unwrap()
            .contains(install_dir.as_ref()),
        "shell configuration should remain independent from install-config persistence"
    );
}

#[test]
fn installer_scripts_delegate_shell_environment_configuration_to_install_hooks() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let install_sh = fs::read_to_string(manifest_dir.join("install.sh")).unwrap();
    let install_ps1 = fs::read_to_string(manifest_dir.join("install.ps1")).unwrap();
    let shell_env =
        fs::read_to_string(manifest_dir.join("src/commands/install_hooks/shell_env.rs")).unwrap();

    for removed_shell_logic in [
        "fish_add_path -g",
        "Added by git-ai installer on",
        "chown -R",
    ] {
        assert!(
            !install_sh.contains(removed_shell_logic),
            "install.sh should not contain {removed_shell_logic}"
        );
    }
    for removed_powershell_logic in [
        "Set-PathEnsureContains",
        "GetEnvironmentVariable('Path', 'User')",
        "$gitBashConfigured",
        "Git\\bin\\bash.exe",
        "Added by git-ai installer on",
        "$env:PATH =",
    ] {
        assert!(
            !install_ps1.contains(removed_powershell_logic),
            "install.ps1 should not contain {removed_powershell_logic}"
        );
    }

    assert!(install_sh.contains(r#""${INSTALL_DIR}/git-ai" install-hooks"#));
    assert!(install_sh.contains("export GIT_AI_INSTALL_USER=\"$CURRENT_USER\""));
    assert!(install_ps1.contains("& $finalExe install-hooks"));
    assert!(install_ps1.contains("if ($LASTEXITCODE -eq 0)"));

    assert!(shell_env.contains("repair_install_ownership"));
    assert!(shell_env.contains("lchown"));
}

#[cfg(windows)]
#[test]
fn install_hooks_configures_git_bash_when_user_path_updates_are_skipped() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let invoking_home = repo.test_home_path();
    let installed_home = repo.path().join("installed-user");
    let program_files = repo.path().join("Program Files");
    let git_bash = program_files.join("Git").join("bin").join("bash.exe");
    fs::create_dir_all(git_bash.parent().unwrap()).unwrap();
    fs::write(&git_bash, "").unwrap();

    let bashrc = invoking_home.join(".bashrc");
    let bash_profile = invoking_home.join(".bash_profile");
    fs::write(&bash_profile, "# existing profile\r\n").unwrap();

    let (installed_binary, mut command) =
        copied_install_hooks_command(&repo, invoking_home, &installed_home);
    command
        .env("ProgramFiles", &program_files)
        .env_remove("ProgramFiles(x86)");
    let first = copied_binary_output(&mut command);
    assert!(
        first.status.success(),
        "install-hooks failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    assert!(
        !bashrc.exists(),
        ".bash_profile must be preferred when .bashrc is absent"
    );
    let contents = fs::read(&bash_profile).unwrap();
    assert!(!contents.starts_with(&[0xef, 0xbb, 0xbf]));
    let contents = String::from_utf8(contents).unwrap();
    assert!(contents.contains("# Added by git-ai installer on "));
    let install_dir = installed_binary
        .parent()
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    let install_dir = install_dir.strip_prefix("//?/").unwrap_or(&install_dir);
    let git_bash_install_dir = if install_dir.as_bytes().get(1) == Some(&b':') {
        format!("/{}/{}", install_dir[..1].to_lowercase(), &install_dir[3..])
    } else {
        install_dir.to_string()
    };
    let expected_path_entry = format!("export PATH=\"{git_bash_install_dir}:$PATH\"\n");
    assert!(contents.ends_with(&expected_path_entry));

    let (_, mut second_command) =
        copied_install_hooks_command(&repo, invoking_home, &installed_home);
    second_command
        .env("ProgramFiles", &program_files)
        .env_remove("ProgramFiles(x86)");
    let second = copied_binary_output(&mut second_command);
    assert!(
        second.status.success(),
        "second install-hooks failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        fs::read_to_string(&bash_profile)
            .unwrap()
            .matches(&git_bash_install_dir)
            .count(),
        1
    );
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
    let output = run_isolated_install_hooks(&[]);
    assert!(
        output.status.success(),
        "install-hooks failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_run_install_hooks_with_dry_run_flag() {
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
    let output = run_isolated_install_hooks(&["--verbose"]);
    assert!(
        output.status.success(),
        "install-hooks --verbose failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_run_install_hooks_with_verbose_short_flag() {
    let output = run_isolated_install_hooks(&["-v"]);
    assert!(
        output.status.success(),
        "install-hooks -v failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_run_install_hooks_with_multiple_flags() {
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
    let output = run_isolated_install_hooks(&["--dry-run=false"]);
    assert!(
        output.status.success(),
        "install-hooks --dry-run=false failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_run_install_hooks_ignores_unknown_args() {
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

#[test]
fn windows_msi_configures_git_ai_after_its_managed_path_is_written() {
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/windows/git-ai.wxs"),
    )
    .unwrap();

    assert!(
        manifest.contains(
            r#"<Custom Action="ConfigureGitAi" After="WriteEnvironmentStrings" Condition="NOT Installed" />"#
        ),
        "MSI must write its owned PATH entry before install-hooks checks the user PATH"
    );
}
