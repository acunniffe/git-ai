use crate::error::GitAiError;
use crate::operations::mdm::editor_cli::resolve_editor_cli;
use crate::operations::mdm::hook_installer::{
    HookCheckResult, HookInstaller, HookInstallerParams, InstallResult, UninstallResult,
};
use crate::operations::mdm::hooks_merge::{
    MissingBehavior, edit_settings_json, is_git_ai_checkpoint_command,
    upsert_singleton_command_hook,
};
use crate::operations::mdm::paths::home_dir;
use crate::operations::mdm::vscode_settings::{
    install_vsc_editor_extension, is_github_codespaces, is_vsc_editor_extension_installed,
};

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const WINDSURF_CHECKPOINT_CMD: &str = "checkpoint windsurf --hook-input stdin";

/// The Windsurf Cascade hook events we install into.
const HOOK_EVENTS: &[&str] = &[
    "pre_write_code",
    "post_write_code",
    "pre_run_command",
    "post_run_command",
    "post_cascade_response_with_transcript",
];

pub struct WindsurfInstaller;

impl WindsurfInstaller {
    /// Both locations where Windsurf looks for hooks.
    fn hooks_paths() -> [PathBuf; 2] {
        // https://docs.windsurf.com/windsurf/cascade/hooks#user-level
        let codeium = home_dir().join(".codeium");
        [
            // for intellej
            codeium.join("hooks.json"),
            // for windsurf
            codeium.join("windsurf").join("hooks.json"),
        ]
    }

    /// Install hooks into a single hooks.json file, returning a diff if changes were made.
    fn install_hooks_at(
        hooks_path: &Path,
        desired_cmd: &str,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        edit_settings_json(
            hooks_path,
            dry_run,
            MissingBehavior::TreatAsEmpty,
            |content| Ok(serde_json::from_str(content)?),
            |merged| {
                let mut hooks_obj = merged.get("hooks").cloned().unwrap_or_else(|| json!({}));

                for event in HOOK_EVENTS {
                    let mut event_array = hooks_obj
                        .get(*event)
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();

                    upsert_singleton_command_hook(
                        &mut event_array,
                        desired_cmd,
                        json!({
                            "command": desired_cmd,
                            "show_output": false
                        }),
                    );

                    if let Some(obj) = hooks_obj.as_object_mut() {
                        obj.insert(event.to_string(), Value::Array(event_array));
                    }
                }

                if let Some(root) = merged.as_object_mut() {
                    root.insert("hooks".to_string(), hooks_obj);
                }
            },
        )
    }

    /// Remove hooks from a single hooks.json file, returning a diff if changes were made.
    fn uninstall_hooks_at(hooks_path: &Path, dry_run: bool) -> Result<Option<String>, GitAiError> {
        edit_settings_json(
            hooks_path,
            dry_run,
            MissingBehavior::NoOp,
            |content| Ok(serde_json::from_str(content)?),
            |merged| {
                let Some(mut hooks_obj) = merged.get("hooks").cloned() else {
                    return;
                };

                let mut changed = false;
                for event in HOOK_EVENTS {
                    if let Some(event_array) =
                        hooks_obj.get_mut(*event).and_then(|v| v.as_array_mut())
                    {
                        let original_len = event_array.len();
                        event_array.retain(|item| {
                            if let Some(cmd) = item.get("command").and_then(|c| c.as_str()) {
                                !is_git_ai_checkpoint_command(cmd)
                            } else {
                                true
                            }
                        });
                        if event_array.len() != original_len {
                            changed = true;
                        }
                    }
                }

                if changed && let Some(root) = merged.as_object_mut() {
                    root.insert("hooks".to_string(), hooks_obj);
                }
            },
        )
    }
}

impl HookInstaller for WindsurfInstaller {
    fn name(&self) -> &str {
        "Windsurf"
    }

    fn id(&self) -> &str {
        "windsurf"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["Windsurf", "windsurf"]
    }

    fn check_hooks(&self, _params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let has_cli = resolve_editor_cli("windsurf").is_some();
        let has_dotfiles =
            home_dir().join(".codeium").exists() || home_dir().join(".windsurf").exists();

        if !has_cli && !has_dotfiles {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        // Check all hook locations
        let mut any_installed = false;
        let mut all_installed = true;
        for hooks_path in Self::hooks_paths() {
            if !hooks_path.exists() {
                all_installed = false;
                continue;
            }

            let content = fs::read_to_string(&hooks_path)?;
            let existing: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));

            let has_hooks = HOOK_EVENTS.iter().all(|event| {
                existing
                    .get("hooks")
                    .and_then(|h| h.get(*event))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter().any(|item| {
                            item.get("command")
                                .and_then(|c| c.as_str())
                                .map(is_git_ai_checkpoint_command)
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            });

            if has_hooks {
                any_installed = true;
            } else {
                all_installed = false;
            }
        }

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: any_installed,
            hooks_up_to_date: all_installed,
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let desired_cmd = format!(
            "{} {}",
            params.binary_path.display(),
            WINDSURF_CHECKPOINT_CMD
        );

        let mut all_diffs = Vec::new();

        for hooks_path in Self::hooks_paths() {
            if let Some(diff) = Self::install_hooks_at(&hooks_path, &desired_cmd, dry_run)? {
                all_diffs.push(diff);
            }
        }

        if all_diffs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(all_diffs.join("\n")))
        }
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let mut all_diffs = Vec::new();

        for hooks_path in Self::hooks_paths() {
            if let Some(diff) = Self::uninstall_hooks_at(&hooks_path, dry_run)? {
                all_diffs.push(diff);
            }
        }

        if all_diffs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(all_diffs.join("\n")))
        }
    }

    fn install_extras(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Vec<InstallResult>, GitAiError> {
        let mut results = Vec::new();

        // Skip extension installation in GitHub Codespaces
        // Extensions must be configured via devcontainer.json in Codespaces
        if is_github_codespaces() {
            results.push(InstallResult {
                changed: false,
                diff: None,
                message: "Windsurf: Unable to install extension in GitHub Codespaces. Add to your devcontainer.json: \"customizations\": { \"vscode\": { \"extensions\": [\"git-ai.git-ai-vscode\"] } }".to_string(),
            });
            return Ok(results);
        }

        // Install VS Code extension
        if let Some(cli) = resolve_editor_cli("windsurf") {
            match is_vsc_editor_extension_installed(&cli, "git-ai.git-ai-vscode") {
                Ok(true) => {
                    results.push(InstallResult {
                        changed: false,
                        diff: None,
                        message: "Windsurf: Extension already installed".to_string(),
                    });
                }
                Ok(false) => {
                    if dry_run {
                        results.push(InstallResult {
                            changed: true,
                            diff: None,
                            message: "Windsurf: Pending extension install".to_string(),
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
                                    "Windsurf: Error automatically installing extension: {}",
                                    e
                                );
                                results.push(InstallResult {
                                    changed: false,
                                    diff: None,
                                    message: "Windsurf: Unable to automatically install extension. Please cmd+click on the following link to install: windsurf:extension/git-ai.git-ai-vscode (or search for 'git-ai-vscode' in the Windsurf extensions tab)".to_string(),
                                });
                            }
                        }
                    }
                }
                Err(e) => {
                    results.push(InstallResult {
                        changed: false,
                        diff: None,
                        message: format!("Windsurf: Failed to check extension: {}", e),
                    });
                }
            }
        } else {
            // resolve_editor_cli returned None -- the only way to reach this
            // branch. Windsurf was detected only from its config dotfiles
            // (~/.codeium) and isn't actually installed, so there's nothing to
            // install the extension into. Don't emit a misleading "unable to
            // install" nag here; genuine install/check failures are already
            // reported by the match arms above.
        }

        Ok(results)
    }

    fn uninstall_extras(
        &self,
        _params: &HookInstallerParams,
        _dry_run: bool,
    ) -> Result<Vec<UninstallResult>, GitAiError> {
        Ok(vec![UninstallResult {
            changed: false,
            diff: None,
            message: "Windsurf: Extension must be uninstalled manually through the editor"
                .to_string(),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_install_extras_does_not_nag_when_cli_absent() {
        // Regression: when the Windsurf app/CLI isn't resolvable (e.g. only the
        // ~/.codeium dotfiles exist), install_extras must not emit the misleading
        // "Unable to automatically install extension" message. dry_run=true means
        // a real install is never attempted, so this never spawns an editor.
        let params = HookInstallerParams {
            binary_path: std::path::PathBuf::from("/usr/local/bin/git-ai"),
        };
        let results = WindsurfInstaller.install_extras(&params, true).unwrap();
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
}
