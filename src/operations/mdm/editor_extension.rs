use crate::error::GitAiError;
use crate::operations::mdm::editor_cli::EditorCliCommand;
use std::time::Duration;

const EDITOR_CLI_RETRY_ATTEMPTS: usize = 3;
const EDITOR_CLI_RETRY_DELAY: Duration = Duration::from_millis(300);
pub(crate) const GIT_AI_VSCODE_EXTENSION_ID: &str = "git-ai.git-ai-vscode";

fn run_editor_cli_with_retry<T>(
    mut operation: impl FnMut() -> Result<T, String>,
    fallback_error: impl FnOnce() -> String,
    mut sleep: impl FnMut(Duration),
) -> Result<T, GitAiError> {
    let mut last_error_message = None;

    for attempt in 1..=EDITOR_CLI_RETRY_ATTEMPTS {
        match operation() {
            Ok(value) => return Ok(value),
            Err(message) => last_error_message = Some(message),
        }

        if attempt < EDITOR_CLI_RETRY_ATTEMPTS {
            sleep(EDITOR_CLI_RETRY_DELAY);
        }
    }

    // This text is surfaced verbatim by each editor installer, so retain the
    // legacy Generic wrapper and its existing Display representation.
    Err(GitAiError::Generic(
        last_error_message.unwrap_or_else(fallback_error),
    ))
}

fn is_vsc_editor_extension_installed_with_sleeper(
    cli: &EditorCliCommand,
    id_or_vsix: &str,
    sleep: impl FnMut(Duration),
) -> Result<bool, GitAiError> {
    run_editor_cli_with_retry(
        || {
            let cmd_result = cli.command(&["--list-extensions"]).output();

            match cmd_result {
                Ok(output) => {
                    if !output.status.success() {
                        Err(String::from_utf8_lossy(&output.stderr).to_string())
                    } else {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        Ok(stdout.contains(id_or_vsix))
                    }
                }
                Err(error) => Err(error.to_string()),
            }
        },
        || format!("{} CLI '--list-extensions' failed", cli.program),
        sleep,
    )
}

/// Check if a VS Code extension is installed.
pub fn is_vsc_editor_extension_installed(
    cli: &EditorCliCommand,
    id_or_vsix: &str,
) -> Result<bool, GitAiError> {
    // The editor CLI can be flaky and throw intermittent JavaScript errors.
    is_vsc_editor_extension_installed_with_sleeper(cli, id_or_vsix, std::thread::sleep)
}

fn install_vsc_editor_extension_with_sleeper(
    cli: &EditorCliCommand,
    id_or_vsix: &str,
    sleep: impl FnMut(Duration),
) -> Result<(), GitAiError> {
    run_editor_cli_with_retry(
        || {
            let cmd_status = cli
                .command(&["--install-extension", id_or_vsix, "--force"])
                .status();

            match cmd_status {
                Ok(status) => {
                    if status.success() {
                        Ok(())
                    } else {
                        Err(format!("{} extension install failed", cli.program))
                    }
                }
                Err(error) => Err(error.to_string()),
            }
        },
        || format!("{} extension install failed", cli.program),
        sleep,
    )
}

/// Install a VS Code extension.
pub fn install_vsc_editor_extension(
    cli: &EditorCliCommand,
    id_or_vsix: &str,
) -> Result<(), GitAiError> {
    // The editor CLI can be flaky and throw intermittent JavaScript errors.
    install_vsc_editor_extension_with_sleeper(cli, id_or_vsix, std::thread::sleep)
}

/// Result of ensuring the git-ai extension is present in a VS Code-family editor.
pub(crate) enum ExtensionInstallOutcome {
    CliUnavailable,
    AlreadyInstalled,
    PendingInstall,
    Installed,
    CheckFailed(GitAiError),
    InstallFailed(GitAiError),
}

fn ensure_vsc_editor_extension_with_sleeper(
    cli: Option<&EditorCliCommand>,
    id_or_vsix: &str,
    dry_run: bool,
    before_install: impl FnOnce(),
    mut sleep: impl FnMut(Duration),
) -> ExtensionInstallOutcome {
    let Some(cli) = cli else {
        return ExtensionInstallOutcome::CliUnavailable;
    };

    match is_vsc_editor_extension_installed_with_sleeper(cli, id_or_vsix, &mut sleep) {
        Ok(true) => ExtensionInstallOutcome::AlreadyInstalled,
        Ok(false) if dry_run => ExtensionInstallOutcome::PendingInstall,
        Ok(false) => {
            before_install();
            match install_vsc_editor_extension_with_sleeper(cli, id_or_vsix, &mut sleep) {
                Ok(()) => ExtensionInstallOutcome::Installed,
                Err(error) => ExtensionInstallOutcome::InstallFailed(error),
            }
        }
        Err(error) => ExtensionInstallOutcome::CheckFailed(error),
    }
}

/// Check and, when needed, install an extension through a VS Code-family CLI.
///
/// Product-specific output is supplied by the caller and runs immediately
/// before the install command.
pub(crate) fn ensure_vsc_editor_extension(
    cli: Option<&EditorCliCommand>,
    id_or_vsix: &str,
    dry_run: bool,
    before_install: impl FnOnce(),
) -> ExtensionInstallOutcome {
    ensure_vsc_editor_extension_with_sleeper(
        cli,
        id_or_vsix,
        dry_run,
        before_install,
        std::thread::sleep,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::cell::Cell;
    #[cfg(unix)]
    use std::fs::{self, OpenOptions};
    #[cfg(unix)]
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::path::PathBuf;
    #[cfg(unix)]
    use tempfile::TempDir;

    #[cfg(unix)]
    fn scripted_cli(script: &str) -> (TempDir, EditorCliCommand, PathBuf, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let program = temp_dir.path().join("editor-cli");
        let attempts_path = temp_dir.path().join("attempts");
        let args_path = temp_dir.path().join("args");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();

        let cli = EditorCliCommand {
            program: program.display().to_string(),
            args_prefix: Vec::new(),
            env_vars: vec![
                (
                    "GIT_AI_TEST_EDITOR_CLI_ATTEMPTS".to_string(),
                    attempts_path.display().to_string(),
                ),
                (
                    "GIT_AI_TEST_EDITOR_CLI_ARGS".to_string(),
                    args_path.display().to_string(),
                ),
            ],
        };

        (temp_dir, cli, attempts_path, args_path)
    }

    #[cfg(unix)]
    const RETRY_THEN_SUCCEED_SCRIPT: &str = r#"#!/bin/sh
count=0
if [ -f "$GIT_AI_TEST_EDITOR_CLI_ATTEMPTS" ]; then
    IFS= read -r count < "$GIT_AI_TEST_EDITOR_CLI_ATTEMPTS"
fi
count=$((count + 1))
printf '%s' "$count" > "$GIT_AI_TEST_EDITOR_CLI_ATTEMPTS"
printf '%s' "$*" > "$GIT_AI_TEST_EDITOR_CLI_ARGS"
if [ "$count" -lt 3 ]; then
    printf 'failure-%s\n' "$count" >&2
    exit 1
fi
if [ "$1" = "--list-extensions" ]; then
    printf 'git-ai.git-ai-vscode\n'
fi
"#;

    #[cfg(unix)]
    const ALWAYS_FAIL_SCRIPT: &str = r#"#!/bin/sh
count=0
if [ -f "$GIT_AI_TEST_EDITOR_CLI_ATTEMPTS" ]; then
    IFS= read -r count < "$GIT_AI_TEST_EDITOR_CLI_ATTEMPTS"
fi
count=$((count + 1))
printf '%s' "$count" > "$GIT_AI_TEST_EDITOR_CLI_ATTEMPTS"
printf 'failure-%s\n' "$count" >&2
exit 1
"#;

    #[cfg(unix)]
    const EXTENSION_LIFECYCLE_SCRIPT: &str = r#"#!/bin/sh
count=0
if [ -f "$GIT_AI_TEST_EDITOR_CLI_ATTEMPTS" ]; then
    IFS= read -r count < "$GIT_AI_TEST_EDITOR_CLI_ATTEMPTS"
fi
count=$((count + 1))
printf '%s' "$count" > "$GIT_AI_TEST_EDITOR_CLI_ATTEMPTS"
printf '%s\n' "$*" >> "$GIT_AI_TEST_EDITOR_CLI_ARGS"
if [ "$1" = "--list-extensions" ]; then
    if [ "$GIT_AI_TEST_EXTENSION_INSTALLED" = "true" ]; then
        printf 'git-ai.git-ai-vscode\n'
    fi
    exit 0
fi
if [ "$GIT_AI_TEST_INSTALL_SUCCEEDS" = "true" ]; then
    exit 0
fi
exit 1
"#;

    #[test]
    #[cfg(unix)]
    fn list_extensions_retries_three_times_with_two_300ms_delays() {
        let (_temp_dir, cli, attempts_path, args_path) = scripted_cli(RETRY_THEN_SUCCEED_SCRIPT);
        let mut delays = Vec::new();

        assert!(
            is_vsc_editor_extension_installed_with_sleeper(
                &cli,
                GIT_AI_VSCODE_EXTENSION_ID,
                |delay| delays.push(delay),
            )
            .unwrap(),
            "the third attempt should observe the extension"
        );

        assert_eq!(
            delays,
            [Duration::from_millis(300), Duration::from_millis(300),]
        );
        assert_eq!(fs::read_to_string(attempts_path).unwrap(), "3");
        assert_eq!(fs::read_to_string(args_path).unwrap(), "--list-extensions");
    }

    #[test]
    #[cfg(unix)]
    fn list_extensions_reports_the_last_failed_attempt() {
        let (_temp_dir, cli, attempts_path, _args_path) = scripted_cli(ALWAYS_FAIL_SCRIPT);

        let error = is_vsc_editor_extension_installed_with_sleeper(
            &cli,
            GIT_AI_VSCODE_EXTENSION_ID,
            |_| {},
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "Generic error: failure-3\n");
        assert_eq!(fs::read_to_string(attempts_path).unwrap(), "3");
    }

    #[test]
    #[cfg(unix)]
    fn install_extension_retries_three_times_with_two_300ms_delays() {
        let (_temp_dir, cli, attempts_path, args_path) = scripted_cli(RETRY_THEN_SUCCEED_SCRIPT);
        let mut delays = Vec::new();

        install_vsc_editor_extension_with_sleeper(&cli, GIT_AI_VSCODE_EXTENSION_ID, |delay| {
            delays.push(delay)
        })
        .unwrap();

        assert_eq!(
            delays,
            [Duration::from_millis(300), Duration::from_millis(300),]
        );
        assert_eq!(fs::read_to_string(attempts_path).unwrap(), "3");
        assert_eq!(
            fs::read_to_string(args_path).unwrap(),
            "--install-extension git-ai.git-ai-vscode --force"
        );
    }

    #[test]
    #[cfg(unix)]
    fn install_extension_reports_the_existing_nonzero_status_message() {
        let (_temp_dir, cli, attempts_path, _args_path) = scripted_cli(ALWAYS_FAIL_SCRIPT);
        let expected = format!("Generic error: {} extension install failed", cli.program);

        let error =
            install_vsc_editor_extension_with_sleeper(&cli, GIT_AI_VSCODE_EXTENSION_ID, |_| {})
                .unwrap_err();

        assert_eq!(error.to_string(), expected);
        assert_eq!(fs::read_to_string(attempts_path).unwrap(), "3");
    }

    #[test]
    fn extension_lifecycle_reports_an_unavailable_cli_without_work() {
        assert!(matches!(
            ensure_vsc_editor_extension_with_sleeper(
                None,
                GIT_AI_VSCODE_EXTENSION_ID,
                false,
                || panic!("missing CLI must not announce an install"),
                |_| {},
            ),
            ExtensionInstallOutcome::CliUnavailable
        ));
    }

    #[test]
    #[cfg(unix)]
    fn extension_lifecycle_reports_already_installed_without_announcement() {
        let (_temp_dir, mut cli, attempts_path, _args_path) =
            scripted_cli(EXTENSION_LIFECYCLE_SCRIPT);
        cli.env_vars.push((
            "GIT_AI_TEST_EXTENSION_INSTALLED".to_string(),
            "true".to_string(),
        ));

        let outcome = ensure_vsc_editor_extension_with_sleeper(
            Some(&cli),
            GIT_AI_VSCODE_EXTENSION_ID,
            false,
            || panic!("an installed extension must not be announced"),
            |_| {},
        );

        assert!(matches!(outcome, ExtensionInstallOutcome::AlreadyInstalled));
        assert_eq!(fs::read_to_string(attempts_path).unwrap(), "1");
    }

    #[test]
    #[cfg(unix)]
    fn extension_lifecycle_stops_before_install_during_dry_run() {
        let (_temp_dir, cli, attempts_path, args_path) = scripted_cli(EXTENSION_LIFECYCLE_SCRIPT);

        let outcome = ensure_vsc_editor_extension_with_sleeper(
            Some(&cli),
            GIT_AI_VSCODE_EXTENSION_ID,
            true,
            || panic!("dry-run must not announce an install"),
            |_| {},
        );

        assert!(matches!(outcome, ExtensionInstallOutcome::PendingInstall));
        assert_eq!(fs::read_to_string(attempts_path).unwrap(), "1");
        assert_eq!(
            fs::read_to_string(args_path).unwrap(),
            "--list-extensions\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn extension_lifecycle_announces_immediately_before_install() {
        let (_temp_dir, mut cli, attempts_path, args_path) =
            scripted_cli(EXTENSION_LIFECYCLE_SCRIPT);
        cli.env_vars.push((
            "GIT_AI_TEST_INSTALL_SUCCEEDS".to_string(),
            "true".to_string(),
        ));

        let outcome = ensure_vsc_editor_extension_with_sleeper(
            Some(&cli),
            GIT_AI_VSCODE_EXTENSION_ID,
            false,
            || {
                let mut args = OpenOptions::new().append(true).open(&args_path).unwrap();
                writeln!(args, "announce").unwrap();
            },
            |_| {},
        );

        assert!(matches!(outcome, ExtensionInstallOutcome::Installed));
        assert_eq!(fs::read_to_string(attempts_path).unwrap(), "2");
        assert_eq!(
            fs::read_to_string(args_path).unwrap(),
            "--list-extensions\nannounce\n--install-extension git-ai.git-ai-vscode --force\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn extension_lifecycle_keeps_check_and_install_failures_distinct() {
        let check_announced = Cell::new(false);
        let (_temp_dir, check_cli, _attempts_path, _args_path) = scripted_cli(ALWAYS_FAIL_SCRIPT);
        let check_outcome = ensure_vsc_editor_extension_with_sleeper(
            Some(&check_cli),
            GIT_AI_VSCODE_EXTENSION_ID,
            false,
            || check_announced.set(true),
            |_| {},
        );
        assert!(matches!(
            check_outcome,
            ExtensionInstallOutcome::CheckFailed(_)
        ));
        assert!(!check_announced.get());

        let install_announced = Cell::new(false);
        let (_temp_dir, mut install_cli, _attempts_path, _args_path) =
            scripted_cli(EXTENSION_LIFECYCLE_SCRIPT);
        install_cli.env_vars.extend([
            (
                "GIT_AI_TEST_EXTENSION_INSTALLED".to_string(),
                "false".to_string(),
            ),
            (
                "GIT_AI_TEST_INSTALL_SUCCEEDS".to_string(),
                "false".to_string(),
            ),
        ]);
        let install_outcome = ensure_vsc_editor_extension_with_sleeper(
            Some(&install_cli),
            GIT_AI_VSCODE_EXTENSION_ID,
            false,
            || install_announced.set(true),
            |_| {},
        );
        assert!(matches!(
            install_outcome,
            ExtensionInstallOutcome::InstallFailed(_)
        ));
        assert!(install_announced.get());
    }
}
