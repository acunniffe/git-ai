use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{generate_diff, is_git_ai_checkpoint_command, write_atomic, zcode_cli_dir};

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const ZCODE_CHECKPOINT_CMD: &str = "checkpoint zcode --hook-input stdin";

/// The zcode hook events git-ai installs into. zcode matches tool events with a
/// regex `matcher` and treats an omitted matcher as "match everything", so the
/// installer omits it entirely (`"*"` would be an invalid regex that silently
/// never matches).
const HOOK_EVENTS: &[&str] = &["PreToolUse", "PostToolUse"];

pub struct ZcodeInstaller;

impl ZcodeInstaller {
    fn config_path() -> PathBuf {
        zcode_cli_dir().join("config.json")
    }

    /// Returns `(hooks_installed, hooks_up_to_date)` from a parsed config.
    /// `hooks_installed` = a git-ai checkpoint command exists in every event we
    /// install; `hooks_up_to_date` is the same contract here.
    fn hook_status(config: &Value) -> (bool, bool) {
        let events = config
            .get("hooks")
            .and_then(|h| h.get("events"))
            .and_then(|e| e.as_object());

        let Some(events) = events else {
            return (false, false);
        };

        let all_installed = HOOK_EVENTS.iter().all(|event| {
            events
                .get(*event)
                .and_then(|v| v.as_array())
                .map(|blocks| {
                    blocks.iter().any(|block| {
                        block
                            .get("hooks")
                            .and_then(|h| h.as_array())
                            .map(|hooks| {
                                hooks.iter().any(|hook| {
                                    hook.get("command")
                                        .and_then(|c| c.as_str())
                                        .map(is_git_ai_checkpoint_command)
                                        .unwrap_or(false)
                                })
                            })
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        });

        (all_installed, all_installed)
    }

    /// Is this hook entry one of ours?
    fn is_git_ai_hook(hook: &Value) -> bool {
        hook.get("command")
            .and_then(|c| c.as_str())
            .map(is_git_ai_checkpoint_command)
            .unwrap_or(false)
    }

    fn install_hooks_at(
        config_path: &Path,
        desired_cmd: &str,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        if let Some(dir) = config_path.parent() {
            fs::create_dir_all(dir)?;
        }

        let existing_content = if config_path.exists() {
            fs::read_to_string(config_path)?
        } else {
            String::new()
        };

        let existing: Value = if existing_content.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&existing_content)?
        };

        let mut merged = existing.clone();
        if !merged.is_object() {
            return Err(GitAiError::Generic(format!(
                "{} is not a JSON object",
                config_path.display()
            )));
        }
        let mut hooks_obj = merged.get("hooks").cloned().unwrap_or_else(|| json!({}));

        // zcode disables config-file hooks unless explicitly enabled.
        if let Some(hooks_map) = hooks_obj.as_object_mut() {
            hooks_map.insert("enabled".to_string(), Value::Bool(true));
        }

        let mut events_obj = hooks_obj
            .get("events")
            .cloned()
            .unwrap_or_else(|| json!({}));

        for event in HOOK_EVENTS {
            let mut blocks = events_obj
                .get(*event)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            // Refresh any existing git-ai entry in place (keeping its position)
            // and drop duplicate git-ai hooks.
            let mut found_git_ai = false;
            for block in blocks.iter_mut() {
                let Some(hooks_arr) = block.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                    continue;
                };
                let mut seen_git_ai = false;
                let mut updated: Vec<Value> = Vec::with_capacity(hooks_arr.len());
                for hook in hooks_arr.iter() {
                    if Self::is_git_ai_hook(hook) {
                        if seen_git_ai {
                            continue;
                        }
                        updated.push(json!({
                            "type": "command",
                            "command": desired_cmd
                        }));
                        seen_git_ai = true;
                        found_git_ai = true;
                    } else {
                        updated.push(hook.clone());
                    }
                }
                *hooks_arr = updated;
            }

            if !found_git_ai {
                blocks.push(json!({
                    "hooks": [
                        {
                            "type": "command",
                            "command": desired_cmd
                        }
                    ]
                }));
            }

            if let Some(obj) = events_obj.as_object_mut() {
                obj.insert(event.to_string(), Value::Array(blocks));
            }
        }

        if let Some(hooks_map) = hooks_obj.as_object_mut() {
            hooks_map.insert("events".to_string(), events_obj);
        }
        if let Some(root) = merged.as_object_mut() {
            root.insert("hooks".to_string(), hooks_obj);
        }

        if existing == merged {
            return Ok(None);
        }

        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(config_path, &existing_content, &new_content);

        if !dry_run {
            write_atomic(config_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }

    fn uninstall_hooks_at(config_path: &Path, dry_run: bool) -> Result<Option<String>, GitAiError> {
        if !config_path.exists() {
            return Ok(None);
        }

        let existing_content = fs::read_to_string(config_path)?;
        let existing: Value = serde_json::from_str(&existing_content)?;

        let mut merged = existing.clone();
        let hooks_obj = match merged.get_mut("hooks").and_then(|h| h.as_object_mut()) {
            Some(h) => h,
            None => return Ok(None),
        };

        let mut changed = false;
        for event in HOOK_EVENTS {
            let Some(events_obj) = hooks_obj.get_mut("events").and_then(|e| e.as_object_mut())
            else {
                continue;
            };
            let Some(blocks) = events_obj.get_mut(*event).and_then(|v| v.as_array_mut()) else {
                continue;
            };

            for block in blocks.iter_mut() {
                if let Some(hooks_arr) = block.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                    let original_len = hooks_arr.len();
                    hooks_arr.retain(|h| !Self::is_git_ai_hook(h));
                    if hooks_arr.len() != original_len {
                        changed = true;
                    }
                }
            }

            // Drop blocks left empty by the removal.
            let original_len = blocks.len();
            blocks.retain(|block| {
                block
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| !hooks.is_empty())
                    .unwrap_or(true)
            });
            if blocks.len() != original_len {
                changed = true;
            }

            // Drop the event key entirely when nothing remains.
            if blocks.is_empty() && events_obj.remove(*event).is_some() {
                changed = true;
            }
        }

        if !changed {
            return Ok(None);
        }

        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(config_path, &existing_content, &new_content);

        if !dry_run {
            write_atomic(config_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }
}

impl HookInstaller for ZcodeInstaller {
    fn name(&self) -> &str {
        "ZCode"
    }

    fn id(&self) -> &str {
        "zcode"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["ZCode"]
    }

    fn check_hooks(&self, _params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let tool_installed = zcode_cli_dir().exists();

        if !tool_installed {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let config_path = Self::config_path();
        let (hooks_installed, hooks_up_to_date) = if config_path.exists() {
            let content = fs::read_to_string(&config_path)?;
            let config: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));
            Self::hook_status(&config)
        } else {
            (false, false)
        };

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
        let desired_cmd = format!("{} {}", params.binary_path.display(), ZCODE_CHECKPOINT_CMD);
        Self::install_hooks_at(&Self::config_path(), &desired_cmd, dry_run)
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        Self::uninstall_hooks_at(&Self::config_path(), dry_run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_config() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".zcode").join("cli").join("config.json");
        (dir, path)
    }

    fn desired_cmd() -> String {
        "/usr/local/bin/git-ai checkpoint zcode --hook-input stdin".to_string()
    }

    fn read_config(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn s1_fresh_install_writes_hooks_and_enables() {
        let (_dir, path) = temp_config();

        let diff = ZcodeInstaller::install_hooks_at(&path, &desired_cmd(), false)
            .unwrap()
            .expect("fresh install should produce a diff");

        assert!(diff.contains("hooks"), "diff should mention hooks");

        let config = read_config(&path);
        assert_eq!(
            config["hooks"]["enabled"],
            json!(true),
            "config-file hooks are disabled by default in zcode and must be enabled"
        );

        for event in HOOK_EVENTS {
            let blocks = config["hooks"]["events"][event].as_array().unwrap();
            assert_eq!(blocks.len(), 1, "one block for {event}");
            assert!(
                blocks[0].get("matcher").is_none(),
                "matcher must be omitted (omission matches all tools; \"*\" is an invalid regex)"
            );
            let hooks = blocks[0]["hooks"].as_array().unwrap();
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0]["type"], "command");
            assert_eq!(hooks[0]["command"], json!(desired_cmd()));
        }
    }

    #[test]
    fn s2_fresh_install_is_idempotent() {
        let (_dir, path) = temp_config();

        ZcodeInstaller::install_hooks_at(&path, &desired_cmd(), false).unwrap();
        let second = ZcodeInstaller::install_hooks_at(&path, &desired_cmd(), false).unwrap();

        assert!(
            second.is_none(),
            "reinstall with the same command should be a no-op"
        );
    }

    #[test]
    fn s3_install_preserves_foreign_config_and_hooks() {
        let (_dir, path) = temp_config();
        let foreign_cmd = "node /somewhere/auto-recall.mjs".to_string();

        let existing = json!({
            "version": 3,
            "telemetry": {"enabled": false},
            "hooks": {
                "events": {
                    "UserPromptSubmit": [
                        {"matcher": "", "hooks": [{"type": "process", "command": foreign_cmd}]}
                    ],
                    "PreToolUse": [
                        {"hooks": [{"type": "process", "command": foreign_cmd}]}
                    ]
                }
            }
        });
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_string_pretty(&existing).unwrap()).unwrap();

        ZcodeInstaller::install_hooks_at(&path, &desired_cmd(), false)
            .unwrap()
            .expect("install over existing config should produce a diff");

        let config = read_config(&path);

        // Unrelated top-level keys are preserved untouched.
        assert_eq!(config["version"], json!(3));
        assert_eq!(config["telemetry"], json!({"enabled": false}));

        // Foreign events are preserved untouched (including invalid matchers,
        // which belong to the user, not us).
        assert_eq!(
            config["hooks"]["events"]["UserPromptSubmit"],
            existing["hooks"]["events"]["UserPromptSubmit"]
        );

        // Foreign PreToolUse hook is preserved alongside the new git-ai block.
        let pre_blocks = config["hooks"]["events"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_blocks.len(), 2, "foreign block + new git-ai block");
        assert_eq!(pre_blocks[0]["hooks"][0]["command"], json!(foreign_cmd));

        let git_ai_block = pre_blocks
            .iter()
            .find(|b| {
                b["hooks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(ZcodeInstaller::is_git_ai_hook)
            })
            .expect("git-ai block should exist");
        assert_eq!(git_ai_block["hooks"][0]["command"], json!(desired_cmd()));

        assert_eq!(config["hooks"]["enabled"], json!(true));
    }

    #[test]
    fn s4_install_updates_stale_git_ai_command() {
        let (_dir, path) = temp_config();

        let stale = json!({
            "hooks": {
                "events": {
                    "PreToolUse": [
                        {"hooks": [{"type": "command", "command": "/old/path/git-ai checkpoint zcode --hook-input stdin"}]}
                    ],
                    "PostToolUse": [
                        {"hooks": [{"type": "command", "command": "/old/path/git-ai checkpoint zcode --hook-input stdin"}]}
                    ]
                }
            }
        });
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_string_pretty(&stale).unwrap()).unwrap();

        ZcodeInstaller::install_hooks_at(&path, &desired_cmd(), false)
            .unwrap()
            .expect("updating a stale command should produce a diff");

        let config = read_config(&path);
        for event in HOOK_EVENTS {
            let blocks = config["hooks"]["events"][event].as_array().unwrap();
            assert_eq!(
                blocks.len(),
                1,
                "existing git-ai block is refreshed in place for {event}"
            );
            let hooks = blocks[0]["hooks"].as_array().unwrap();
            assert_eq!(hooks.len(), 1, "no duplicate git-ai hooks");
            assert_eq!(hooks[0]["command"], json!(desired_cmd()));
        }
    }

    #[test]
    fn s5_uninstall_removes_only_git_ai_hooks() {
        let (_dir, path) = temp_config();
        let foreign_cmd = "node /somewhere/session-start.mjs".to_string();

        ZcodeInstaller::install_hooks_at(&path, &desired_cmd(), false).unwrap();

        // Add a foreign hook alongside ours in PostToolUse.
        let config = read_config(&path);
        let mut with_foreign = config.clone();
        with_foreign["hooks"]["events"]["PostToolUse"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "process", "command": foreign_cmd}));
        fs::write(&path, serde_json::to_string_pretty(&with_foreign).unwrap()).unwrap();

        let diff = ZcodeInstaller::uninstall_hooks_at(&path, false)
            .unwrap()
            .expect("uninstall should produce a diff");

        assert!(diff.contains("PostToolUse"));

        let config = read_config(&path);
        // git-ai blocks are gone entirely (they were left empty).
        assert!(config["hooks"]["events"]["PreToolUse"].as_array().is_none());
        // The foreign hook survives and the block stays.
        let post_blocks = config["hooks"]["events"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post_blocks.len(), 1);
        let hooks = post_blocks[0]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0]["command"], json!(foreign_cmd));

        // enabled is left as-is on uninstall.
        assert_eq!(config["hooks"]["enabled"], json!(true));
    }

    #[test]
    fn s6_hook_status_reflects_config() {
        // Empty config: nothing installed.
        assert_eq!(ZcodeInstaller::hook_status(&json!({})), (false, false));

        // Only one event installed: partial.
        let partial = json!({
            "hooks": {"events": {"PreToolUse": [{"hooks": [{"command": "git-ai checkpoint zcode --hook-input stdin"}]}]}}
        });
        assert_eq!(ZcodeInstaller::hook_status(&partial), (false, false));

        // Both events: installed.
        let full = json!({
            "hooks": {"events": {
                "PreToolUse": [{"hooks": [{"command": "git-ai checkpoint zcode --hook-input stdin"}]}],
                "PostToolUse": [{"hooks": [{"command": "git-ai checkpoint zcode --hook-input stdin"}]}]
            }}
        });
        assert_eq!(ZcodeInstaller::hook_status(&full), (true, true));

        // Foreign hooks only: not installed.
        let foreign = json!({
            "hooks": {"events": {
                "PreToolUse": [{"hooks": [{"command": "node other.mjs"}]}],
                "PostToolUse": [{"hooks": [{"command": "node other.mjs"}]}]
            }}
        });
        assert_eq!(ZcodeInstaller::hook_status(&foreign), (false, false));
    }
}
