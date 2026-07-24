use crate::error::GitAiError;
use crate::operations::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::operations::mdm::hooks_merge::{MissingBehavior, edit_settings_json};
use crate::operations::mdm::hooks_merge_flat::{
    remove_command_hooks_flat, upsert_command_hooks_flat,
};
use crate::operations::mdm::paths::home_dir;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

const FIREBENDER_CHECKPOINT_CMD: &str = "checkpoint firebender --hook-input stdin";
const FIREBENDER_HOOK_NAMES: &[&str] = &["preToolUse", "postToolUse"];

pub struct FirebenderInstaller;

impl FirebenderInstaller {
    fn hooks_path() -> PathBuf {
        home_dir().join(".firebender").join("hooks.json")
    }

    fn is_firebender_checkpoint_command(cmd: &str) -> bool {
        cmd.contains("checkpoint firebender")
            && (cmd.contains("git-ai") || cmd.ends_with(FIREBENDER_CHECKPOINT_CMD))
    }

    /// Firebender's legacy hooks carried a `"matcher"` field that's no longer
    /// used; its presence alone forces an update so it gets stripped.
    fn needs_matcher_strip(existing_hook: &Value) -> bool {
        existing_hook.get("matcher").is_some()
    }
}

impl HookInstaller for FirebenderInstaller {
    fn name(&self) -> &str {
        "Firebender"
    }

    fn id(&self) -> &str {
        "firebender"
    }

    fn check_hooks(&self, _params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let has_dotfiles = home_dir().join(".firebender").exists();
        if !has_dotfiles {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

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

        let has_pre = existing
            .get("hooks")
            .and_then(|h| h.get("preToolUse"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|item| {
                    item.get("command")
                        .and_then(|c| c.as_str())
                        .map(Self::is_firebender_checkpoint_command)
                        .unwrap_or(false)
                        && item.get("matcher").is_none()
                })
            })
            .unwrap_or(false);

        let has_post = existing
            .get("hooks")
            .and_then(|h| h.get("postToolUse"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|item| {
                    item.get("command")
                        .and_then(|c| c.as_str())
                        .map(Self::is_firebender_checkpoint_command)
                        .unwrap_or(false)
                        && item.get("matcher").is_none()
                })
            })
            .unwrap_or(false);

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: has_pre && has_post,
            hooks_up_to_date: has_pre && has_post,
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
            FIREBENDER_CHECKPOINT_CMD
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
                    FIREBENDER_HOOK_NAMES,
                    Self::is_firebender_checkpoint_command,
                    Self::needs_matcher_strip,
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
                    FIREBENDER_HOOK_NAMES,
                    Self::is_firebender_checkpoint_command,
                );
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::mdm::test_env::with_temp_home;
    use serial_test::serial;

    fn create_test_binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    #[test]
    fn test_firebender_command_detection() {
        assert!(FirebenderInstaller::is_firebender_checkpoint_command(
            "/usr/local/bin/git-ai checkpoint firebender --hook-input stdin"
        ));
        assert!(FirebenderInstaller::is_firebender_checkpoint_command(
            "git-ai something checkpoint firebender"
        ));
        assert!(!FirebenderInstaller::is_firebender_checkpoint_command(
            "git-ai checkpoint cursor"
        ));
    }

    #[test]
    #[serial]
    fn test_install_hooks_creates_expected_entries() {
        with_temp_home(|home| {
            let hooks_path = home.join(".firebender").join("hooks.json");
            let installer = FirebenderInstaller;
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
                    .contains("checkpoint firebender")
            );
            assert!(
                content["hooks"]["postToolUse"][0]["command"]
                    .as_str()
                    .unwrap()
                    .contains("checkpoint firebender")
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_is_idempotent() {
        with_temp_home(|_home| {
            let installer = FirebenderInstaller;
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
            let hooks_path = home.join(".firebender").join("hooks.json");
            if let Some(parent) = hooks_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }

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

            let installer = FirebenderInstaller;
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
                    .contains("checkpoint firebender")
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_updates_existing_firebender_command() {
        with_temp_home(|home| {
            let hooks_path = home.join(".firebender").join("hooks.json");
            if let Some(parent) = hooks_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }

            let existing = json!({
                "version": 1,
                "hooks": {
                    "preToolUse": [{ "command": "/old/path/git-ai checkpoint firebender --hook-input stdin" }],
                    "postToolUse": [{ "command": "/old/path/git-ai checkpoint firebender --hook-input stdin" }]
                }
            });
            fs::write(
                &hooks_path,
                serde_json::to_string_pretty(&existing).unwrap(),
            )
            .unwrap();

            let installer = FirebenderInstaller;
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
                FIREBENDER_CHECKPOINT_CMD
            );
            assert_eq!(
                updated["hooks"]["preToolUse"][0]["command"]
                    .as_str()
                    .unwrap(),
                expected
            );
            assert_eq!(
                updated["hooks"]["postToolUse"][0]["command"]
                    .as_str()
                    .unwrap(),
                expected
            );
        });
    }

    #[test]
    #[serial]
    fn test_uninstall_hooks_removes_only_firebender_entries() {
        with_temp_home(|home| {
            let hooks_path = home.join(".firebender").join("hooks.json");
            if let Some(parent) = hooks_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }

            let existing = json!({
                "version": 1,
                "hooks": {
                    "preToolUse": [
                        { "command": "/usr/local/bin/git-ai checkpoint firebender --hook-input stdin" },
                        { "command": "echo keep-before" }
                    ],
                    "postToolUse": [
                        { "command": "/usr/local/bin/git-ai checkpoint firebender --hook-input stdin" },
                        { "command": "echo keep-after" }
                    ]
                }
            });
            fs::write(
                &hooks_path,
                serde_json::to_string_pretty(&existing).unwrap(),
            )
            .unwrap();

            let installer = FirebenderInstaller;
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
                updated["hooks"]["preToolUse"][0]["command"]
                    .as_str()
                    .unwrap(),
                "echo keep-before"
            );
            assert_eq!(
                updated["hooks"]["postToolUse"][0]["command"]
                    .as_str()
                    .unwrap(),
                "echo keep-after"
            );
        });
    }

    #[test]
    #[serial]
    fn test_check_hooks_not_up_to_date_when_matcher_present() {
        with_temp_home(|home| {
            let hooks_path = home.join(".firebender").join("hooks.json");
            if let Some(parent) = hooks_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }

            let existing = json!({
                "version": 1,
                "hooks": {
                    "preToolUse": [{ "matcher": "Write|Edit|Delete", "command": "/usr/local/bin/git-ai checkpoint firebender --hook-input stdin" }],
                    "postToolUse": [{ "matcher": "Write|Edit|Delete", "command": "/usr/local/bin/git-ai checkpoint firebender --hook-input stdin" }]
                }
            });
            fs::write(
                &hooks_path,
                serde_json::to_string_pretty(&existing).unwrap(),
            )
            .unwrap();

            let installer = FirebenderInstaller;
            let result = installer
                .check_hooks(&HookInstallerParams {
                    binary_path: create_test_binary_path(),
                })
                .unwrap();

            assert!(result.tool_installed);
            assert!(
                !result.hooks_installed,
                "hooks with matcher should not be considered installed"
            );
            assert!(
                !result.hooks_up_to_date,
                "hooks with matcher should not be up to date"
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_removes_matcher_from_existing_entry() {
        with_temp_home(|home| {
            let hooks_path = home.join(".firebender").join("hooks.json");
            if let Some(parent) = hooks_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }

            let existing = json!({
                "version": 1,
                "hooks": {
                    "preToolUse": [{ "matcher": "Write|Edit|Delete", "command": "/old/path/git-ai checkpoint firebender --hook-input stdin" }],
                    "postToolUse": [{ "matcher": "Write|Edit|Delete", "command": "/old/path/git-ai checkpoint firebender --hook-input stdin" }]
                }
            });
            fs::write(
                &hooks_path,
                serde_json::to_string_pretty(&existing).unwrap(),
            )
            .unwrap();

            let installer = FirebenderInstaller;
            let diff = installer
                .install_hooks(
                    &HookInstallerParams {
                        binary_path: create_test_binary_path(),
                    },
                    false,
                )
                .unwrap();

            assert!(
                diff.is_some(),
                "should produce a diff when removing matcher"
            );

            let updated: Value =
                serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
            assert!(
                updated["hooks"]["preToolUse"][0].get("matcher").is_none(),
                "matcher should be removed from preToolUse"
            );
            assert!(
                updated["hooks"]["postToolUse"][0].get("matcher").is_none(),
                "matcher should be removed from postToolUse"
            );
            let expected_cmd = format!(
                "{} {}",
                create_test_binary_path().display(),
                FIREBENDER_CHECKPOINT_CMD
            );
            assert_eq!(
                updated["hooks"]["preToolUse"][0]["command"]
                    .as_str()
                    .unwrap(),
                expected_cmd
            );
        });
    }
}
