use crate::error::GitAiError;
use crate::operations::mdm::editor_cli::resolve_editor_cli;
use crate::operations::mdm::hook_installer::{
    HookCheckResult, HookInstaller, HookInstallerParams, InstallResult,
};
use crate::operations::mdm::hooks_merge::{MissingBehavior, edit_settings_json};
use crate::operations::mdm::hooks_merge_flat::{
    remove_command_hooks_flat, upsert_command_hooks_flat,
};
use crate::operations::mdm::paths::home_dir;
use crate::operations::mdm::version::{
    MIN_CURSOR_VERSION, get_editor_version, parse_version, version_meets_requirement,
};
use crate::operations::mdm::vscode_settings::{
    install_vsc_editor_extension, is_vsc_editor_extension_installed, settings_paths_for_products,
    should_process_settings_target,
};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

const CURSOR_PRE_TOOL_USE_CMD: &str = "checkpoint cursor --hook-input stdin";
const CURSOR_HOOK_NAMES: &[&str] = &["preToolUse", "postToolUse"];

pub struct CursorInstaller;

impl CursorInstaller {
    fn hooks_path() -> PathBuf {
        home_dir().join(".cursor").join("hooks.json")
    }

    fn settings_targets() -> Vec<PathBuf> {
        settings_paths_for_products(&["Cursor"])
    }

    fn is_cursor_checkpoint_command(cmd: &str) -> bool {
        cmd.contains("git-ai checkpoint cursor")
            || (cmd.contains("git-ai") && cmd.contains("checkpoint") && cmd.contains("cursor"))
    }
}

impl HookInstaller for CursorInstaller {
    fn name(&self) -> &str {
        "Cursor"
    }

    fn id(&self) -> &str {
        "cursor"
    }

    fn check_hooks(&self, _params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let resolved_cli = resolve_editor_cli("cursor");
        let has_cli = resolved_cli.is_some();
        let has_dotfiles = home_dir().join(".cursor").exists();
        let has_settings_targets = Self::settings_targets()
            .iter()
            .any(|path| should_process_settings_target(path));

        if !has_cli && !has_dotfiles && !has_settings_targets {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        // If we have a CLI, check version
        if let Some(cli) = &resolved_cli
            && let Ok(version_str) = get_editor_version(cli)
            && let Some(version) = parse_version(&version_str)
            && !version_meets_requirement(version, MIN_CURSOR_VERSION)
        {
            return Err(GitAiError::Generic(format!(
                "Cursor version {}.{} detected, but minimum version {}.{} is required",
                version.0, version.1, MIN_CURSOR_VERSION.0, MIN_CURSOR_VERSION.1
            )));
        }

        // Check if hooks are installed
        let hooks_path = Self::hooks_path();
        if !hooks_path.exists() {
            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let content = fs::read_to_string(&hooks_path)?;
        let existing: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));

        let has_hooks = existing
            .get("hooks")
            .and_then(|h| h.get("preToolUse"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(Self::is_cursor_checkpoint_command)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: has_hooks,
            hooks_up_to_date: has_hooks,
        })
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["Cursor", "cursor"]
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let desired_cmd = format!(
            "{} {}",
            params.binary_path.display(),
            CURSOR_PRE_TOOL_USE_CMD
        );

        edit_settings_json(
            &Self::hooks_path(),
            dry_run,
            MissingBehavior::TreatAsEmpty,
            |content| Ok(serde_json::from_str(content)?),
            |merged| {
                upsert_command_hooks_flat(
                    merged,
                    &desired_cmd,
                    CURSOR_HOOK_NAMES,
                    Self::is_cursor_checkpoint_command,
                    |_hook| false,
                );
            },
        )
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        edit_settings_json(
            &Self::hooks_path(),
            dry_run,
            MissingBehavior::NoOp,
            |content| Ok(serde_json::from_str(content)?),
            |merged| {
                remove_command_hooks_flat(
                    merged,
                    CURSOR_HOOK_NAMES,
                    Self::is_cursor_checkpoint_command,
                );
            },
        )
    }

    fn install_extras(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Vec<InstallResult>, GitAiError> {
        let mut results = Vec::new();

        // Install VS Code extension
        if let Some(cli) = resolve_editor_cli("cursor") {
            match is_vsc_editor_extension_installed(&cli, "git-ai.git-ai-vscode") {
                Ok(true) => {
                    results.push(InstallResult {
                        changed: false,
                        diff: None,
                        message: "Cursor: Extension already installed".to_string(),
                    });
                }
                Ok(false) => {
                    if dry_run {
                        results.push(InstallResult {
                            changed: true,
                            diff: None,
                            message: "Cursor: Pending extension install".to_string(),
                        });
                    } else {
                        println!("Installing extensions...");
                        println!("\tInstalling extension 'git-ai.git-ai-vscode'...");
                        match install_vsc_editor_extension(&cli, "git-ai.git-ai-vscode") {
                            Ok(()) => {
                                results.push(InstallResult {
                                    changed: true,
                                    diff: None,
                                    message: "\tExtension 'git-ai.git-ai-vscode' was successfully installed.".to_string(),
                                });
                            }
                            Err(e) => {
                                tracing::debug!(
                                    "Cursor: Error automatically installing extension: {}",
                                    e
                                );
                                results.push(InstallResult {
                                    changed: false,
                                    diff: None,
                                    message: "Cursor: Unable to automatically install extension. Please cmd+click on the following link to install: cursor:extension/git-ai.git-ai-vscode (or search for 'git-ai-vscode' in the Cursor extensions tab)".to_string(),
                                });
                            }
                        }
                    }
                }
                Err(e) => {
                    results.push(InstallResult {
                        changed: false,
                        diff: None,
                        message: format!("Cursor: Failed to check extension: {}", e),
                    });
                }
            }
        } else {
            // resolve_editor_cli returned None -- the only way to reach this
            // branch. Cursor was detected only from its config dotfiles
            // (~/.cursor) and isn't actually installed, so there's nothing to
            // install the extension into. Don't emit a misleading "unable to
            // install" nag here; genuine install/check failures are already
            // reported by the match arms above.
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::mdm::paths::clean_path;
    use crate::operations::mdm::test_env::with_temp_home;
    use serial_test::serial;
    use std::fs;

    fn create_test_binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    // ---- characterization: real install_hooks/uninstall_hooks invocations ----
    // (the tests below this point call the trait methods directly, unlike the
    // pre-existing tests above which hand-construct expected JSON)

    #[test]
    #[serial]
    fn test_install_hooks_creates_expected_entries() {
        with_temp_home(|home| {
            let hooks_path = home.join(".cursor").join("hooks.json");
            let installer = CursorInstaller;
            let diff = installer
                .install_hooks(
                    &HookInstallerParams {
                        binary_path: create_test_binary_path(),
                    },
                    false,
                )
                .unwrap();
            assert!(diff.is_some());

            let content: Value =
                serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
            assert_eq!(content.get("version").unwrap(), &json!(1));
            assert!(
                content["hooks"]["preToolUse"][0]["command"]
                    .as_str()
                    .unwrap()
                    .contains("git-ai checkpoint cursor")
            );
            assert!(
                content["hooks"]["postToolUse"][0]["command"]
                    .as_str()
                    .unwrap()
                    .contains("git-ai checkpoint cursor")
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_is_idempotent() {
        with_temp_home(|_home| {
            let installer = CursorInstaller;
            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };
            installer.install_hooks(&params, false).unwrap();
            let second = installer.install_hooks(&params, false).unwrap();
            assert!(second.is_none(), "reinstall should be a no-op");
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_preserves_existing_foreign_hooks() {
        with_temp_home(|home| {
            let hooks_path = home.join(".cursor").join("hooks.json");
            fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
            let existing = json!({
                "version": 1,
                "hooks": {
                    "preToolUse": [{ "command": "echo 'before'" }],
                    "postToolUse": [{ "command": "echo 'after'" }]
                }
            });
            fs::write(
                &hooks_path,
                serde_json::to_string_pretty(&existing).unwrap(),
            )
            .unwrap();

            let installer = CursorInstaller;
            let diff = installer
                .install_hooks(
                    &HookInstallerParams {
                        binary_path: create_test_binary_path(),
                    },
                    false,
                )
                .unwrap();
            assert!(diff.is_some());

            let updated: Value =
                serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
            let pre = updated["hooks"]["preToolUse"].as_array().unwrap();
            let post = updated["hooks"]["postToolUse"].as_array().unwrap();
            assert_eq!(pre.len(), 2);
            assert_eq!(post.len(), 2);
            assert_eq!(pre[0]["command"], "echo 'before'");
            assert_eq!(post[0]["command"], "echo 'after'");
            assert!(
                pre[1]["command"]
                    .as_str()
                    .unwrap()
                    .contains("git-ai checkpoint cursor")
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_updates_existing_cursor_command() {
        with_temp_home(|home| {
            let hooks_path = home.join(".cursor").join("hooks.json");
            fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
            let existing = json!({
                "version": 1,
                "hooks": {
                    "preToolUse": [{ "command": "/old/path/git-ai checkpoint cursor --hook-input stdin" }],
                    "postToolUse": [{ "command": "/old/path/git-ai checkpoint cursor --hook-input stdin" }]
                }
            });
            fs::write(
                &hooks_path,
                serde_json::to_string_pretty(&existing).unwrap(),
            )
            .unwrap();

            let installer = CursorInstaller;
            let diff = installer
                .install_hooks(
                    &HookInstallerParams {
                        binary_path: create_test_binary_path(),
                    },
                    false,
                )
                .unwrap();
            assert!(diff.is_some());

            let updated: Value =
                serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
            let expected = format!(
                "{} {}",
                create_test_binary_path().display(),
                CURSOR_PRE_TOOL_USE_CMD
            );
            assert_eq!(updated["hooks"]["preToolUse"][0]["command"], expected);
            assert_eq!(updated["hooks"]["postToolUse"][0]["command"], expected);
        });
    }

    #[test]
    #[serial]
    fn test_uninstall_hooks_removes_only_cursor_entries() {
        with_temp_home(|home| {
            let hooks_path = home.join(".cursor").join("hooks.json");
            fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
            let existing = json!({
                "version": 1,
                "hooks": {
                    "preToolUse": [
                        { "command": "/usr/local/bin/git-ai checkpoint cursor --hook-input stdin" },
                        { "command": "echo keep-before" }
                    ],
                    "postToolUse": [
                        { "command": "/usr/local/bin/git-ai checkpoint cursor --hook-input stdin" },
                        { "command": "echo keep-after" }
                    ]
                }
            });
            fs::write(
                &hooks_path,
                serde_json::to_string_pretty(&existing).unwrap(),
            )
            .unwrap();

            let installer = CursorInstaller;
            let diff = installer
                .uninstall_hooks(
                    &HookInstallerParams {
                        binary_path: create_test_binary_path(),
                    },
                    false,
                )
                .unwrap();
            assert!(diff.is_some());

            let updated: Value =
                serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
            assert_eq!(updated["hooks"]["preToolUse"].as_array().unwrap().len(), 1);
            assert_eq!(updated["hooks"]["postToolUse"].as_array().unwrap().len(), 1);
            assert_eq!(
                updated["hooks"]["preToolUse"][0]["command"],
                "echo keep-before"
            );
            assert_eq!(
                updated["hooks"]["postToolUse"][0]["command"],
                "echo keep-after"
            );
        });
    }

    #[test]
    fn test_install_extras_does_not_nag_when_cli_absent() {
        // Regression: when the Cursor app/CLI isn't resolvable (e.g. only the
        // ~/.cursor dotfiles exist), install_extras must not emit the misleading
        // "Unable to automatically install extension" message. dry_run=true means
        // a real install is never attempted, so this never spawns an editor.
        let params = HookInstallerParams {
            binary_path: create_test_binary_path(),
        };
        let results = CursorInstaller.install_extras(&params, true).unwrap();
        assert!(
            results
                .iter()
                .all(|r| !r.message.contains("Unable to automatically install")),
            "unexpected extension nag: {:?}",
            results
                .iter()
                .map(|r| r.message.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_cursor_hook_commands_no_windows_extended_path_prefix() {
        let raw_path = PathBuf::from(r"\\?\C:\Users\USERNAME\.git-ai\bin\git-ai.exe");
        let binary_path = clean_path(raw_path);

        let cmd = format!("{} {}", binary_path.display(), CURSOR_PRE_TOOL_USE_CMD);

        assert!(
            !cmd.contains(r"\\?\"),
            "hook command should not contain \\\\?\\ prefix, got: {}",
            cmd
        );
        assert!(
            cmd.contains("checkpoint cursor"),
            "command should still contain checkpoint args"
        );
    }

    #[test]
    fn test_cursor_settings_targets_returns_candidates() {
        let targets = CursorInstaller::settings_targets();
        assert!(!targets.is_empty());
    }
}
