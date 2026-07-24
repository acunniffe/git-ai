use crate::error::GitAiError;
use crate::operations::mdm::editor_cli::binary_exists;
use crate::operations::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
#[cfg(test)]
use crate::operations::mdm::hooks_merge::is_git_ai_checkpoint_command;
use crate::operations::mdm::hooks_merge::{
    MissingBehavior, catch_all_hook_status, edit_settings_json, install_catch_all_hooks,
    uninstall_catch_all_hooks,
};
use crate::operations::mdm::paths::home_dir;
use jsonc_parser::ParseOptions;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

/// Droid's settings.json uses JSONC (JSON with `//` line comments, `/* */` block
/// comments, and trailing commas). Standard `serde_json` rejects these, so we
/// parse through `jsonc_parser` first and convert to `serde_json::Value`.
/// NOTE: This parse-to-serde-Value approach discards JSONC comments and trailing
/// commas. If comment preservation becomes important, migrate to CstRootNode
/// (as used in utils.rs::update_vscode_chat_hook_settings).
fn parse_jsonc_settings(content: &str) -> Result<Value, GitAiError> {
    let parsed = jsonc_parser::parse_to_value(content, &ParseOptions::default())
        .map_err(|e| GitAiError::Generic(format!("Failed to parse Droid settings: {e}")))?;
    Ok(match parsed {
        Some(val) => jsonc_to_serde(val),
        None => json!({}),
    })
}

fn jsonc_to_serde(val: jsonc_parser::JsonValue<'_>) -> Value {
    match val {
        jsonc_parser::JsonValue::Null => Value::Null,
        jsonc_parser::JsonValue::Boolean(b) => Value::Bool(b),
        jsonc_parser::JsonValue::Number(n) => serde_json::from_str(n).unwrap_or(Value::Null),
        jsonc_parser::JsonValue::String(s) => Value::String(s.into_owned()),
        jsonc_parser::JsonValue::Array(arr) => {
            Value::Array(arr.into_iter().map(jsonc_to_serde).collect())
        }
        jsonc_parser::JsonValue::Object(obj) => {
            let map: serde_json::Map<String, Value> = obj
                .into_iter()
                .map(|(k, v)| (k, jsonc_to_serde(v)))
                .collect();
            Value::Object(map)
        }
    }
}

const DROID_PRE_TOOL_CMD: &str = "checkpoint droid --hook-input stdin";
const DROID_POST_TOOL_CMD: &str = "checkpoint droid --hook-input stdin";
#[cfg(test)]
const DROID_CATCH_ALL_MATCHER: &str = "*";

pub struct DroidInstaller;

impl DroidInstaller {
    fn settings_path() -> PathBuf {
        home_dir().join(".factory").join("settings.json")
    }

    /// Returns `(hooks_installed, hooks_up_to_date)` from a parsed settings value.
    /// `hooks_installed` = git-ai checkpoint command exists in ANY matcher block.
    /// `hooks_up_to_date` = git-ai checkpoint command exists in the `"*"` catch-all block.
    fn hook_status(settings: &Value) -> (bool, bool) {
        catch_all_hook_status(settings, "PreToolUse")
    }

    fn install_hooks_at(
        settings_path: &Path,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let binary_path = params.binary_path.to_string_lossy().to_string();
        let pre_tool_cmd = format!("{} {}", binary_path, DROID_PRE_TOOL_CMD);
        let post_tool_cmd = format!("{} {}", binary_path, DROID_POST_TOOL_CMD);

        edit_settings_json(
            settings_path,
            dry_run,
            MissingBehavior::TreatAsEmpty,
            parse_jsonc_settings,
            |merged| {
                let mut hooks_obj = merged.get("hooks").cloned().unwrap_or_else(|| json!({}));
                install_catch_all_hooks(
                    &mut hooks_obj,
                    &[
                        ("PreToolUse", pre_tool_cmd.as_str()),
                        ("PostToolUse", post_tool_cmd.as_str()),
                    ],
                );
                if let Some(root) = merged.as_object_mut() {
                    root.insert("hooks".to_string(), hooks_obj);
                }

                // Add claudeHooksImported flag if it doesn't exist.
                if let Some(hooks) = merged.get_mut("hooks").and_then(|h| h.as_object_mut())
                    && !hooks.contains_key("claudeHooksImported")
                {
                    hooks.insert("claudeHooksImported".to_string(), json!(true));
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
            parse_jsonc_settings,
            |merged| {
                let Some(mut hooks_obj) = merged.get("hooks").cloned() else {
                    return;
                };
                if uninstall_catch_all_hooks(&mut hooks_obj, &["PreToolUse", "PostToolUse"])
                    && let Some(root) = merged.as_object_mut()
                {
                    root.insert("hooks".to_string(), hooks_obj);
                }
            },
        )
    }
}

impl HookInstaller for DroidInstaller {
    fn name(&self) -> &str {
        "Droid"
    }

    fn id(&self) -> &str {
        "droid"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["droid"]
    }

    fn check_hooks(&self, _params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let has_binary = binary_exists("droid");
        let has_dotfiles = home_dir().join(".factory").exists();

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
        let existing: Value = parse_jsonc_settings(&content).unwrap_or_else(|_| json!({}));
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
    use crate::operations::mdm::test_env::{with_fake_binary_on_path, with_temp_home};
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_env() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join(".factory").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        (temp_dir, settings_path)
    }

    fn binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    fn params() -> HookInstallerParams {
        HookInstallerParams {
            binary_path: binary_path(),
        }
    }

    fn expected_cmd() -> String {
        format!("{} {}", binary_path().display(), DROID_PRE_TOOL_CMD)
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
                    .map(|m| m == DROID_CATCH_ALL_MATCHER)
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

        let diff = DroidInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_some());

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            let hooks = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(hooks.len(), 1, "{hook_type}: expected 1 hook in catch-all");
            assert_eq!(
                hooks[0].get("command").and_then(|c| c.as_str()).unwrap(),
                expected_cmd()
            );
        }
        // claudeHooksImported flag should be set
        assert_eq!(
            settings
                .get("hooks")
                .and_then(|h| h.get("claudeHooksImported")),
            Some(&json!(true))
        );
    }

    #[test]
    fn s2_idempotent_already_on_catch_all() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": cmd}]}],
                    "PostToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": cmd}]}],
                    "claudeHooksImported": true
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = DroidInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_none(), "should be idempotent");
    }

    #[test]
    fn s3_migration_old_matcher_no_user_hooks() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [{"matcher": "^(Edit|Write|Create|ApplyPatch)$", "hooks": [{"type":"command","command": cmd}]}],
                    "PostToolUse": [{"matcher": "^(Edit|Write|Create|ApplyPatch)$", "hooks": [{"type":"command","command": cmd}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        DroidInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            let hooks = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(hooks.len(), 1, "{hook_type}: git-ai should be in catch-all");

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
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [{"matcher": "^(Edit|Write|Create|ApplyPatch)$", "hooks": [
                        {"type":"command","command": "echo before"},
                        {"type":"command","command": cmd}
                    ]}],
                    "PostToolUse": [{"matcher": "^(Edit|Write|Create|ApplyPatch)$", "hooks": [
                        {"type":"command","command": "prettier --write"},
                        {"type":"command","command": cmd}
                    ]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        DroidInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for (hook_type, user_cmd) in &[
            ("PreToolUse", "echo before"),
            ("PostToolUse", "prettier --write"),
        ] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 1);

            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let old_block = blocks
                .iter()
                .find(|b| {
                    b.get("matcher").and_then(|m| m.as_str())
                        == Some("^(Edit|Write|Create|ApplyPatch)$")
                })
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
                "hooks": {
                    "PreToolUse": [{"matcher": "^(Edit|Write|Create|ApplyPatch)$", "hooks": [{"type":"command","command": "echo user"}]}],
                    "PostToolUse": [{"matcher": "^(Edit|Write|Create|ApplyPatch)$", "hooks": [{"type":"command","command": "echo user"}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        DroidInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 1);

            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let old_block = blocks
                .iter()
                .find(|b| {
                    b.get("matcher").and_then(|m| m.as_str())
                        == Some("^(Edit|Write|Create|ApplyPatch)$")
                })
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
                "hooks": {
                    "PreToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": "audit-tool"}]}],
                    "PostToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": "audit-tool"}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        DroidInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 2);
            assert_eq!(
                catch_all[0]
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap(),
                "audit-tool"
            );
            assert!(is_git_ai_checkpoint_command(
                catch_all[1]
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap()
            ));
        }
    }

    #[test]
    fn s7_idempotent_user_catch_all_plus_git_ai() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        let before = json!({
            "hooks": {
                "PreToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": "audit-tool"}, {"type":"command","command": cmd}]}],
                "PostToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": "audit-tool"}, {"type":"command","command": cmd}]}],
                "claudeHooksImported": true
            }
        });
        fs::write(&path, serde_json::to_string_pretty(&before).unwrap()).unwrap();
        let diff = DroidInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_none());
    }

    #[test]
    fn s8_deduplication_git_ai_in_both_blocks() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [
                        {"matcher": "*", "hooks": [{"type":"command","command": cmd}]},
                        {"matcher": "^(Edit|Write|Create|ApplyPatch)$", "hooks": [{"type":"command","command": "user"}, {"type":"command","command": cmd}]}
                    ],
                    "PostToolUse": [
                        {"matcher": "*", "hooks": [{"type":"command","command": cmd}]},
                        {"matcher": "^(Edit|Write|Create|ApplyPatch)$", "hooks": [{"type":"command","command": "user"}, {"type":"command","command": cmd}]}
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        DroidInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 1);

            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let old_block = blocks
                .iter()
                .find(|b| {
                    b.get("matcher").and_then(|m| m.as_str())
                        == Some("^(Edit|Write|Create|ApplyPatch)$")
                })
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
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": cmd}, {"type":"command","command": cmd}]}],
                    "PostToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": cmd}, {"type":"command","command": cmd}]}],
                    "claudeHooksImported": true
                }
            }))
            .unwrap(),
        )
        .unwrap();

        DroidInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
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
                "hooks": {
                    "PreToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": "/old/git-ai checkpoint droid"}]}],
                    "PostToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": "/old/git-ai checkpoint droid"}]}],
                    "claudeHooksImported": true
                }
            }))
            .unwrap(),
        )
        .unwrap();

        DroidInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 1);
            assert_eq!(
                catch_all[0]
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap(),
                expected_cmd()
            );
        }
    }

    // ---- Uninstall scenarios ----

    #[test]
    fn u1_uninstall_from_catch_all() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": cmd}]}],
                    "PostToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": cmd}]}],
                    "claudeHooksImported": true
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = DroidInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_some());

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
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
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [{"matcher": "^(Edit|Write|Create|ApplyPatch)$", "hooks": [
                        {"type":"command","command": "echo before"},
                        {"type":"command","command": cmd}
                    ]}],
                    "PostToolUse": [{"matcher": "^(Edit|Write|Create|ApplyPatch)$", "hooks": [
                        {"type":"command","command": "echo before"},
                        {"type":"command","command": cmd}
                    ]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        DroidInstaller::uninstall_hooks_at(&path, false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let old_block = blocks
                .iter()
                .find(|b| {
                    b.get("matcher").and_then(|m| m.as_str())
                        == Some("^(Edit|Write|Create|ApplyPatch)$")
                })
                .unwrap();
            let hooks = old_block.get("hooks").and_then(|h| h.as_array()).unwrap();
            assert!(
                hooks
                    .iter()
                    .any(|h| h.get("command").and_then(|c| c.as_str()) == Some("echo before"))
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
        let cmd = expected_cmd();
        let user = "echo user";
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [
                        {"matcher": "*", "hooks": [{"type":"command","command": cmd}, {"type":"command","command": user}]},
                        {"matcher": "^(Edit|Write|Create|ApplyPatch)$", "hooks": [{"type":"command","command": cmd}]}
                    ],
                    "PostToolUse": [
                        {"matcher": "*", "hooks": [{"type":"command","command": cmd}]},
                        {"matcher": "^(Edit|Write|Create|ApplyPatch)$", "hooks": [{"type":"command","command": cmd}, {"type":"command","command": user}]}
                    ],
                    "claudeHooksImported": true
                }
            }))
            .unwrap(),
        )
        .unwrap();

        DroidInstaller::uninstall_hooks_at(&path, false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
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
            serde_json::to_string_pretty(&json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": "echo hello"}]}]}}))
                .unwrap(),
        )
        .unwrap();

        let diff = DroidInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_none());
    }

    // ---- check_hooks scenarios ----

    #[test]
    fn s11_install_into_jsonc_settings_with_comments() {
        let (_td, path) = setup_test_env();
        let jsonc_content = r#"// Factory CLI Settings
// This file contains your Factory CLI configuration.
{
  "model": "claude-opus-4-5-20251101",
  // Some inline comment
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {"type": "command", "command": "echo existing"}
        ]
      }
    ]
  }
}"#;
        fs::write(&path, jsonc_content).unwrap();

        let diff = DroidInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_some());

        let settings = read_settings(&path);
        let catch_all = hooks_in_catch_all(&settings, "PreToolUse");
        assert_eq!(catch_all.len(), 2);
        assert_eq!(
            catch_all[0]
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap(),
            "echo existing"
        );
        assert!(is_git_ai_checkpoint_command(
            catch_all[1]
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap()
        ));
    }

    #[test]
    fn s12_install_into_jsonc_settings_with_trailing_commas() {
        let (_td, path) = setup_test_env();
        let jsonc_content = r#"{
  "allowlist": ["a", "b",],
  "hooks": {},
}"#;
        fs::write(&path, jsonc_content).unwrap();

        let diff = DroidInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_some());

        let settings = read_settings(&path);
        let catch_all = hooks_in_catch_all(&settings, "PreToolUse");
        assert_eq!(catch_all.len(), 1);
    }

    #[test]
    fn u5_uninstall_from_jsonc_settings_with_comments() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        let jsonc_content = format!(
            r#"// Factory CLI Settings
{{
  "hooks": {{
    "PreToolUse": [{{"matcher": "*", "hooks": [{{"type":"command","command": "{cmd}"}}]}}],
    "PostToolUse": [{{"matcher": "*", "hooks": [{{"type":"command","command": "{cmd}"}}]}}],
    "claudeHooksImported": true
  }}
}}"#
        );
        fs::write(&path, jsonc_content).unwrap();

        let diff = DroidInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_some());
    }

    // ---- check_hooks scenarios ----

    #[test]
    fn c1_no_hooks_returns_not_installed() {
        let (installed, up_to_date) = DroidInstaller::hook_status(&json!({}));
        assert!(!installed);
        assert!(!up_to_date);
    }

    #[test]
    fn c2_git_ai_in_catch_all_returns_up_to_date() {
        let cmd = expected_cmd();
        let settings = json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": cmd}]}]}});
        let (installed, up_to_date) = DroidInstaller::hook_status(&settings);
        assert!(installed);
        assert!(up_to_date);
    }

    #[test]
    fn c3_git_ai_only_in_old_matcher_not_up_to_date() {
        let cmd = expected_cmd();
        let settings = json!({"hooks": {"PreToolUse": [{"matcher": "^(Edit|Write|Create|ApplyPatch)$", "hooks": [{"type":"command","command": cmd}]}]}});
        let (installed, up_to_date) = DroidInstaller::hook_status(&settings);
        assert!(installed);
        assert!(!up_to_date);
    }

    // ---- JSONC parsing ----

    #[test]
    fn jsonc_parse_with_line_comments() {
        let input = r#"// header comment
{
  "key": "value", // inline comment
  "num": 42
}"#;
        let val = parse_jsonc_settings(input).unwrap();
        assert_eq!(val.get("key").and_then(|v| v.as_str()), Some("value"));
        assert_eq!(val.get("num").and_then(|v| v.as_i64()), Some(42));
    }

    #[test]
    fn jsonc_parse_with_block_comments() {
        let input = r#"{ /* block */ "key": "value" }"#;
        let val = parse_jsonc_settings(input).unwrap();
        assert_eq!(val.get("key").and_then(|v| v.as_str()), Some("value"));
    }

    #[test]
    fn jsonc_parse_with_trailing_commas() {
        let input = r#"{ "a": [1, 2,], "b": 3, }"#;
        let val = parse_jsonc_settings(input).unwrap();
        assert_eq!(val.get("b").and_then(|v| v.as_i64()), Some(3));
    }

    #[test]
    fn jsonc_parse_empty_returns_empty_object() {
        let val = parse_jsonc_settings("").unwrap();
        assert_eq!(val, json!({}));
    }

    // ---- Detection / check_hooks ----

    #[test]
    #[serial]
    fn c4_binary_on_path_without_dotfiles_detects_tool() {
        with_temp_home(|_home| {
            with_fake_binary_on_path("droid", |_| {
                let installer = DroidInstaller;
                let result = installer.check_hooks(&params()).unwrap();
                assert!(
                    result.tool_installed,
                    "droid binary on PATH should be detected even without ~/.factory"
                );
                assert!(!result.hooks_installed);
                assert!(!result.hooks_up_to_date);
            });
        });
    }

    #[test]
    #[serial]
    fn c5_no_binary_no_dotfiles_not_detected() {
        with_temp_home(|_home| {
            let installer = DroidInstaller;
            let result = installer.check_hooks(&params()).unwrap();
            assert!(
                !result.tool_installed,
                "no binary and no ~/.factory should mean tool_installed=false"
            );
        });
    }

    #[test]
    #[serial]
    fn c6_dotfiles_without_binary_detects_tool() {
        with_temp_home(|home| {
            fs::create_dir_all(home.join(".factory")).unwrap();
            let installer = DroidInstaller;
            let result = installer.check_hooks(&params()).unwrap();
            assert!(
                result.tool_installed,
                "~/.factory dir should be enough to detect tool even without binary"
            );
        });
    }

    #[test]
    #[serial]
    fn c7_binary_on_path_install_creates_settings() {
        with_temp_home(|_home| {
            with_fake_binary_on_path("droid", |_| {
                let installer = DroidInstaller;
                let result = installer.install_hooks(&params(), false).unwrap();
                assert!(result.is_some(), "install_hooks should produce a diff");

                let settings_path = DroidInstaller::settings_path();
                assert!(
                    settings_path.exists(),
                    "install_hooks should create ~/.factory/settings.json"
                );

                let content = fs::read_to_string(&settings_path).unwrap();
                assert!(
                    content.contains("checkpoint droid"),
                    "settings.json should contain the checkpoint hook command"
                );
            });
        });
    }
}
