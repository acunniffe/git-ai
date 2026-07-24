use crate::error::GitAiError;
use crate::operations::mdm::editor_cli::binary_exists;
use crate::operations::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
#[cfg(test)]
use crate::operations::mdm::hooks_merge::is_git_ai_checkpoint_command;
use crate::operations::mdm::hooks_merge::{
    MissingBehavior, catch_all_hook_status, edit_settings_json, install_catch_all_hooks,
    uninstall_catch_all_hooks,
};
use crate::operations::mdm::paths::gemini_config_dir;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

// Command patterns for hooks
const GEMINI_BEFORE_TOOL_CMD: &str = "checkpoint gemini --hook-input stdin";
const GEMINI_AFTER_TOOL_CMD: &str = "checkpoint gemini --hook-input stdin";
#[cfg(test)]
const GEMINI_CATCH_ALL_MATCHER: &str = "*";

pub struct GeminiInstaller;

impl GeminiInstaller {
    fn settings_path() -> PathBuf {
        gemini_config_dir().join("settings.json")
    }

    /// Returns `(hooks_installed, hooks_up_to_date)` from a parsed settings value.
    /// `hooks_installed` = git-ai checkpoint command exists in ANY matcher block.
    /// `hooks_up_to_date` = git-ai checkpoint command exists in the `"*"` catch-all block.
    fn hook_status(settings: &Value) -> (bool, bool) {
        catch_all_hook_status(settings, "BeforeTool")
    }

    fn install_hooks_at(
        settings_path: &Path,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let before_tool_cmd = format!(
            "{} {}",
            params.binary_path.display(),
            GEMINI_BEFORE_TOOL_CMD
        );
        let after_tool_cmd = format!("{} {}", params.binary_path.display(), GEMINI_AFTER_TOOL_CMD);

        edit_settings_json(
            settings_path,
            dry_run,
            MissingBehavior::TreatAsEmpty,
            |content| Ok(serde_json::from_str(content)?),
            |merged| {
                // Ensure tools.enableHooks is set to true.
                if let Some(tools_obj) = merged.get_mut("tools").and_then(|t| t.as_object_mut()) {
                    if tools_obj.get("enableHooks") != Some(&json!(true)) {
                        tools_obj.insert("enableHooks".to_string(), json!(true));
                    }
                } else if let Some(root) = merged.as_object_mut() {
                    root.insert("tools".to_string(), json!({ "enableHooks": true }));
                }

                let mut hooks_obj = merged.get("hooks").cloned().unwrap_or_else(|| json!({}));
                install_catch_all_hooks(
                    &mut hooks_obj,
                    &[
                        ("BeforeTool", before_tool_cmd.as_str()),
                        ("AfterTool", after_tool_cmd.as_str()),
                    ],
                );
                if let Some(root) = merged.as_object_mut() {
                    root.insert("hooks".to_string(), hooks_obj);
                }
            },
        )
    }

    fn uninstall_hooks_at(
        settings_path: &Path,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        edit_settings_json(
            settings_path,
            dry_run,
            MissingBehavior::NoOp,
            |content| Ok(serde_json::from_str(content)?),
            |merged| {
                let Some(mut hooks_obj) = merged.get("hooks").cloned() else {
                    return;
                };
                if uninstall_catch_all_hooks(&mut hooks_obj, &["BeforeTool", "AfterTool"])
                    && let Some(root) = merged.as_object_mut()
                {
                    root.insert("hooks".to_string(), hooks_obj);
                }
            },
        )
    }
}

impl HookInstaller for GeminiInstaller {
    fn name(&self) -> &str {
        "Gemini"
    }

    fn id(&self) -> &str {
        "gemini"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["gemini"]
    }

    fn check_hooks(&self, _params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let has_binary = binary_exists("gemini");
        let has_dotfiles = gemini_config_dir().exists();

        if !has_binary && !has_dotfiles {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let settings_path = Self::settings_path();
        if !settings_path.exists() {
            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let content = fs::read_to_string(&settings_path)?;
        let existing: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));
        let (hooks_installed, hooks_up_to_date) = Self::hook_status(&existing);

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed,
            hooks_up_to_date,
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        Self::install_hooks_at(&Self::settings_path(), params, dry_run)
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        Self::uninstall_hooks_at(&Self::settings_path(), dry_run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_env() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join(".gemini").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        (temp_dir, settings_path)
    }

    fn with_temp_home<F: FnOnce(&Path)>(f: F) {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path().to_path_buf();

        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        let prev_gemini_cli_home = std::env::var_os("GEMINI_CLI_HOME");

        // SAFETY: tests are serialized via #[serial], so mutating process env is safe.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("USERPROFILE", &home);
            std::env::remove_var("GEMINI_CLI_HOME");
        }

        f(&home);

        // SAFETY: tests are serialized via #[serial], so restoring process env is safe.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
            match prev_gemini_cli_home {
                Some(v) => std::env::set_var("GEMINI_CLI_HOME", v),
                None => std::env::remove_var("GEMINI_CLI_HOME"),
            }
        }
    }

    fn binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    fn params() -> HookInstallerParams {
        HookInstallerParams {
            binary_path: binary_path(),
        }
    }

    fn expected_before_cmd() -> String {
        format!("{} {}", binary_path().display(), GEMINI_BEFORE_TOOL_CMD)
    }

    fn expected_after_cmd() -> String {
        format!("{} {}", binary_path().display(), GEMINI_AFTER_TOOL_CMD)
    }

    fn read_settings(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn hooks_in_catch_all<'a>(settings: &'a Value, hook_type: &str) -> Vec<&'a Value> {
        let Some(blocks) = settings
            .get("hooks")
            .and_then(|h| h.get(hook_type))
            .and_then(|v| v.as_array())
        else {
            return Vec::new();
        };
        blocks
            .iter()
            .find(|b| {
                b.get("matcher")
                    .and_then(|m| m.as_str())
                    .map(|m| m == GEMINI_CATCH_ALL_MATCHER)
                    .unwrap_or(false)
            })
            .and_then(|b| b.get("hooks").and_then(|h| h.as_array()))
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    // ---- Install scenarios ----

    #[test]
    fn s1_fresh_install_creates_catch_all_block() {
        let (_td, path) = setup_test_env();
        fs::remove_file(&path).ok();

        let diff = GeminiInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_some());

        let settings = read_settings(&path);

        // tools.enableHooks must be set
        assert_eq!(
            settings.get("tools").and_then(|t| t.get("enableHooks")),
            Some(&json!(true))
        );

        for (hook_type, expected) in &[
            ("BeforeTool", expected_before_cmd()),
            ("AfterTool", expected_after_cmd()),
        ] {
            let hooks = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(hooks.len(), 1, "{hook_type}: expected 1 hook in catch-all");
            assert_eq!(
                hooks[0].get("command").and_then(|c| c.as_str()).unwrap(),
                expected
            );
        }
    }

    #[test]
    #[serial]
    fn test_install_hooks_respects_gemini_cli_home() {
        with_temp_home(|home| {
            let default_gemini_dir = home.join(".gemini");
            let custom_gemini_home = home.join("custom-gemini-home");
            fs::create_dir_all(&custom_gemini_home).unwrap();

            // SAFETY: tests are serialized via #[serial], so mutating process env is safe.
            unsafe {
                std::env::set_var("GEMINI_CLI_HOME", &custom_gemini_home);
            }

            let settings_path = custom_gemini_home.join(".gemini").join("settings.json");
            fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
            fs::write(&settings_path, "{}").unwrap();

            let installer = GeminiInstaller;
            let result = installer.install_hooks(&params(), false).unwrap();
            assert!(result.is_some(), "install should report a settings diff");
            assert!(
                !default_gemini_dir.exists(),
                "default ~/.gemini should not be touched when GEMINI_CLI_HOME is set"
            );

            let settings = read_settings(&settings_path);
            assert_eq!(settings["tools"]["enableHooks"], true);
            assert_eq!(
                hooks_in_catch_all(&settings, "BeforeTool")[0]["command"],
                expected_before_cmd()
            );
            assert_eq!(
                hooks_in_catch_all(&settings, "AfterTool")[0]["command"],
                expected_after_cmd()
            );

            let check = installer.check_hooks(&params()).unwrap();
            assert!(check.tool_installed);
            assert!(check.hooks_installed);
            assert!(check.hooks_up_to_date);
        });
    }

    #[test]
    fn s2_idempotent_already_on_catch_all() {
        let (_td, path) = setup_test_env();
        let bc = expected_before_cmd();
        let ac = expected_after_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "tools": {"enableHooks": true},
                "hooks": {
                    "BeforeTool": [{"matcher": "*", "hooks": [{"type":"command","command": bc}]}],
                    "AfterTool": [{"matcher": "*", "hooks": [{"type":"command","command": ac}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = GeminiInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_none(), "should be idempotent");
    }

    #[test]
    fn s3_migration_old_matcher_no_user_hooks() {
        let (_td, path) = setup_test_env();
        let bc = expected_before_cmd();
        let ac = expected_after_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "tools": {"enableHooks": true},
                "hooks": {
                    "BeforeTool": [{"matcher": "write_file|replace", "hooks": [{"type":"command","command": bc}]}],
                    "AfterTool": [{"matcher": "write_file|replace", "hooks": [{"type":"command","command": ac}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        GeminiInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["BeforeTool", "AfterTool"] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(
                catch_all.len(),
                1,
                "{hook_type}: git-ai should be in catch-all"
            );

            // The old matcher block had only our hook, so it must be removed entirely.
            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            assert_eq!(
                blocks.len(),
                1,
                "{hook_type}: old matcher block should be removed, only catch-all should remain"
            );
        }
    }

    #[test]
    fn s4_migration_old_matcher_user_hook_preserved() {
        let (_td, path) = setup_test_env();
        let bc = expected_before_cmd();
        let ac = expected_after_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "tools": {"enableHooks": true},
                "hooks": {
                    "BeforeTool": [{"matcher": "write_file|replace", "hooks": [{"type":"command","command": "echo before"}, {"type":"command","command": bc}]}],
                    "AfterTool": [{"matcher": "write_file|replace", "hooks": [{"type":"command","command": "echo after"}, {"type":"command","command": ac}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        GeminiInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for (hook_type, user_cmd) in &[("BeforeTool", "echo before"), ("AfterTool", "echo after")] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 1);

            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let old_block = blocks
                .iter()
                .find(|b| b.get("matcher").and_then(|m| m.as_str()) == Some("write_file|replace"))
                .unwrap();
            let old_hooks = old_block.get("hooks").and_then(|h| h.as_array()).unwrap();
            assert!(
                old_hooks
                    .iter()
                    .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(*user_cmd))
            );
            assert!(!old_hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(is_git_ai_checkpoint_command)
                    .unwrap_or(false)
            }));
        }
    }

    #[test]
    fn s5_fresh_install_user_has_old_matcher_hook() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "tools": {"enableHooks": true},
                "hooks": {
                    "BeforeTool": [{"matcher": "write_file|replace", "hooks": [{"type":"command","command": "echo user"}]}],
                    "AfterTool": [{"matcher": "write_file|replace", "hooks": [{"type":"command","command": "echo user"}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        GeminiInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["BeforeTool", "AfterTool"] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 1);

            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let old_block = blocks
                .iter()
                .find(|b| b.get("matcher").and_then(|m| m.as_str()) == Some("write_file|replace"))
                .unwrap();
            let old_hooks = old_block.get("hooks").and_then(|h| h.as_array()).unwrap();
            assert_eq!(old_hooks.len(), 1);
            assert_eq!(
                old_hooks[0]
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap(),
                "echo user"
            );
        }
    }

    #[test]
    fn s6_fresh_install_user_has_catch_all_hook() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "tools": {"enableHooks": true},
                "hooks": {
                    "BeforeTool": [{"matcher": "*", "hooks": [{"type":"command","command": "audit-tool"}]}],
                    "AfterTool": [{"matcher": "*", "hooks": [{"type":"command","command": "audit-tool"}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        GeminiInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for (hook_type, expected) in &[
            ("BeforeTool", expected_before_cmd()),
            ("AfterTool", expected_after_cmd()),
        ] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 2);
            assert_eq!(
                catch_all[0]
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap(),
                "audit-tool"
            );
            assert_eq!(
                catch_all[1]
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn s7_idempotent_user_catch_all_plus_git_ai() {
        let (_td, path) = setup_test_env();
        let bc = expected_before_cmd();
        let ac = expected_after_cmd();
        let before = json!({
            "tools": {"enableHooks": true},
            "hooks": {
                "BeforeTool": [{"matcher": "*", "hooks": [{"type":"command","command": "audit-tool"}, {"type":"command","command": bc}]}],
                "AfterTool": [{"matcher": "*", "hooks": [{"type":"command","command": "audit-tool"}, {"type":"command","command": ac}]}]
            }
        });
        fs::write(&path, serde_json::to_string_pretty(&before).unwrap()).unwrap();
        let diff = GeminiInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_none());
    }

    #[test]
    fn s8_deduplication_git_ai_in_both_blocks() {
        let (_td, path) = setup_test_env();
        let bc = expected_before_cmd();
        let ac = expected_after_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "tools": {"enableHooks": true},
                "hooks": {
                    "BeforeTool": [
                        {"matcher": "*", "hooks": [{"type":"command","command": bc}]},
                        {"matcher": "write_file|replace", "hooks": [{"type":"command","command": "user"}, {"type":"command","command": bc}]}
                    ],
                    "AfterTool": [
                        {"matcher": "*", "hooks": [{"type":"command","command": ac}]},
                        {"matcher": "write_file|replace", "hooks": [{"type":"command","command": "user"}, {"type":"command","command": ac}]}
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        GeminiInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["BeforeTool", "AfterTool"] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 1);

            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let old_block = blocks
                .iter()
                .find(|b| b.get("matcher").and_then(|m| m.as_str()) == Some("write_file|replace"))
                .unwrap();
            let old_hooks = old_block.get("hooks").and_then(|h| h.as_array()).unwrap();
            assert!(
                old_hooks
                    .iter()
                    .any(|h| h.get("command").and_then(|c| c.as_str()) == Some("user"))
            );
            assert!(!old_hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(is_git_ai_checkpoint_command)
                    .unwrap_or(false)
            }));
        }
    }

    #[test]
    fn s9_deduplication_two_git_ai_in_catch_all() {
        let (_td, path) = setup_test_env();
        let bc = expected_before_cmd();
        let ac = expected_after_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "tools": {"enableHooks": true},
                "hooks": {
                    "BeforeTool": [{"matcher": "*", "hooks": [{"type":"command","command": bc}, {"type":"command","command": bc}]}],
                    "AfterTool": [{"matcher": "*", "hooks": [{"type":"command","command": ac}, {"type":"command","command": ac}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        GeminiInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["BeforeTool", "AfterTool"] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 1);
        }
    }

    #[test]
    fn s10_stale_command_upgraded() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "tools": {"enableHooks": true},
                "hooks": {
                    "BeforeTool": [{"matcher": "*", "hooks": [{"type":"command","command": "/old/git-ai checkpoint gemini"}]}],
                    "AfterTool": [{"matcher": "*", "hooks": [{"type":"command","command": "/old/git-ai checkpoint gemini"}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        GeminiInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        let before_hooks = hooks_in_catch_all(&settings, "BeforeTool");
        let after_hooks = hooks_in_catch_all(&settings, "AfterTool");
        assert_eq!(before_hooks.len(), 1);
        assert_eq!(
            before_hooks[0]
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap(),
            expected_before_cmd()
        );
        assert_eq!(
            after_hooks[0]
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap(),
            expected_after_cmd()
        );
    }

    #[test]
    fn s11_enables_hooks_when_missing() {
        let (_td, path) = setup_test_env();
        // No tools.enableHooks set
        fs::write(&path, "{}").unwrap();

        GeminiInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        assert_eq!(
            settings.get("tools").and_then(|t| t.get("enableHooks")),
            Some(&json!(true))
        );
    }

    // ---- Uninstall scenarios ----

    #[test]
    fn u1_uninstall_from_catch_all() {
        let (_td, path) = setup_test_env();
        let bc = expected_before_cmd();
        let ac = expected_after_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "tools": {"enableHooks": true},
                "hooks": {
                    "BeforeTool": [{"matcher": "*", "hooks": [{"type":"command","command": bc}]}],
                    "AfterTool": [{"matcher": "*", "hooks": [{"type":"command","command": ac}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = GeminiInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_some());

        let settings = read_settings(&path);
        for hook_type in &["BeforeTool", "AfterTool"] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert!(!catch_all.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(is_git_ai_checkpoint_command)
                    .unwrap_or(false)
            }));
        }
    }

    #[test]
    fn u2_uninstall_from_old_matcher_preserves_user_hook() {
        let (_td, path) = setup_test_env();
        let bc = expected_before_cmd();
        let ac = expected_after_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "tools": {"enableHooks": true},
                "hooks": {
                    "BeforeTool": [{"matcher": "write_file|replace", "hooks": [{"type":"command","command": "echo before"}, {"type":"command","command": bc}]}],
                    "AfterTool": [{"matcher": "write_file|replace", "hooks": [{"type":"command","command": "echo after"}, {"type":"command","command": ac}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        GeminiInstaller::uninstall_hooks_at(&path, false).unwrap();

        let settings = read_settings(&path);
        for (hook_type, user_cmd) in &[("BeforeTool", "echo before"), ("AfterTool", "echo after")] {
            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let old_block = blocks
                .iter()
                .find(|b| b.get("matcher").and_then(|m| m.as_str()) == Some("write_file|replace"))
                .unwrap();
            let hooks = old_block.get("hooks").and_then(|h| h.as_array()).unwrap();
            assert!(
                hooks
                    .iter()
                    .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(*user_cmd))
            );
            assert!(!hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(is_git_ai_checkpoint_command)
                    .unwrap_or(false)
            }));
        }
    }

    #[test]
    fn u3_uninstall_from_multiple_blocks() {
        let (_td, path) = setup_test_env();
        let bc = expected_before_cmd();
        let ac = expected_after_cmd();
        let user = "echo user";
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "tools": {"enableHooks": true},
                "hooks": {
                    "BeforeTool": [
                        {"matcher": "*", "hooks": [{"type":"command","command": bc}, {"type":"command","command": user}]},
                        {"matcher": "write_file|replace", "hooks": [{"type":"command","command": bc}]}
                    ],
                    "AfterTool": [
                        {"matcher": "*", "hooks": [{"type":"command","command": ac}]},
                        {"matcher": "write_file|replace", "hooks": [{"type":"command","command": ac}, {"type":"command","command": user}]}
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        GeminiInstaller::uninstall_hooks_at(&path, false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["BeforeTool", "AfterTool"] {
            let all_blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            for block in all_blocks {
                let empty_hooks: Vec<Value> = Vec::new();
                let hooks = block
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .unwrap_or(&empty_hooks);
                assert!(!hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(is_git_ai_checkpoint_command)
                        .unwrap_or(false)
                }));
            }
        }
    }

    #[test]
    fn u4_noop_uninstall_when_no_git_ai() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({"hooks": {"BeforeTool": [{"matcher": "*", "hooks": [{"type":"command","command": "echo hello"}]}]}}))
                .unwrap(),
        )
        .unwrap();

        let diff = GeminiInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_none());
    }

    // ---- check_hooks scenarios ----

    #[test]
    fn c1_no_hooks_returns_not_installed() {
        let (installed, up_to_date) = GeminiInstaller::hook_status(&json!({}));
        assert!(!installed);
        assert!(!up_to_date);
    }

    #[test]
    fn c2_git_ai_in_catch_all_returns_up_to_date() {
        let cmd = expected_before_cmd();
        let settings = json!({"hooks": {"BeforeTool": [{"matcher": "*", "hooks": [{"type":"command","command": cmd}]}]}});
        let (installed, up_to_date) = GeminiInstaller::hook_status(&settings);
        assert!(installed);
        assert!(up_to_date);
    }

    #[test]
    fn c3_git_ai_only_in_old_matcher_not_up_to_date() {
        let cmd = expected_before_cmd();
        let settings = json!({"hooks": {"BeforeTool": [{"matcher": "write_file|replace", "hooks": [{"type":"command","command": cmd}]}]}});
        let (installed, up_to_date) = GeminiInstaller::hook_status(&settings);
        assert!(installed);
        assert!(!up_to_date);
    }
}
