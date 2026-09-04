use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{binary_exists, generate_diff, home_dir, write_atomic};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

/// ZCode hook config lives at ~/.zcode/cli/config.json and uses a "process"
/// hook schema (command = binary path, args = argument array), unlike the
/// Claude Code "command" schema. Keep the matcher in sync with the tool names
/// ZCode actually fires (capitalized Write/Edit/Bash).
const ZCODE_MATCHER: &str = "Edit|Write|Bash";
const ZCODE_HOOK_ARGS: [&str; 4] = ["checkpoint", "zcode", "--hook-input", "stdin"];

pub struct ZcodeInstaller;

impl ZcodeInstaller {
    fn config_path() -> PathBuf {
        home_dir().join(".zcode").join("cli").join("config.json")
    }

    /// True if a hook entry is one of our git-ai checkpoint process hooks.
    /// Unlike the shell-command installers, ZCode stores checkpoint args in
    /// the `args` array, so a plain command-string match is not sufficient.
    fn is_git_ai_process_hook(hook: &Value) -> bool {
        let is_git_ai = hook
            .get("command")
            .and_then(|c| c.as_str())
            .map(|c| c.contains("git-ai"))
            .unwrap_or(false);
        let has_checkpoint = hook
            .get("args")
            .and_then(|a| a.as_array())
            .map(|args| args.iter().any(|a| a.as_str() == Some("checkpoint")))
            .unwrap_or(false);
        is_git_ai && has_checkpoint
    }

    /// Whether a git-ai process hook already targets the zcode preset.
    fn is_zcode_hook(hook: &Value) -> bool {
        hook.get("args")
            .and_then(|a| a.as_array())
            .map(|args| args.iter().any(|a| a.as_str() == Some("zcode")))
            .unwrap_or(false)
    }

    /// Returns `(hooks_installed, hooks_up_to_date)` from a parsed config value.
    /// `hooks_installed` = any git-ai checkpoint process hook exists in either
    /// hook event; `hooks_up_to_date` = such a hook already uses the zcode
    /// preset (a claude-preset hook is installed but needs an update).
    fn hook_status(config: &Value) -> (bool, bool) {
        let Some(events) = config.get("hooks").and_then(|h| h.get("events")) else {
            return (false, false);
        };

        let mut hooks_installed = false;
        let mut hooks_up_to_date = false;
        for hook_type in ["PreToolUse", "PostToolUse"] {
            let Some(blocks) = events.get(hook_type).and_then(|v| v.as_array()) else {
                continue;
            };
            for block in blocks {
                let Some(hooks) = block.get("hooks").and_then(|h| h.as_array()) else {
                    continue;
                };
                for hook in hooks {
                    if Self::is_git_ai_process_hook(hook) {
                        hooks_installed = true;
                        if Self::is_zcode_hook(hook) {
                            hooks_up_to_date = true;
                        }
                    }
                }
            }
        }
        (hooks_installed, hooks_up_to_date)
    }

    /// The git-ai hook entry we want in the config.
    fn desired_hook(params: &HookInstallerParams) -> Value {
        json!({
            "type": "process",
            "command": params.binary_path.to_string_lossy().to_string(),
            "enabled": true,
            "args": ZCODE_HOOK_ARGS
        })
    }

    fn install_hooks_at(
        config_path: &Path,
        params: &HookInstallerParams,
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

        // If the existing config has an unexpected top-level shape (e.g. a
        // bare array/string), start from an empty object so hooks still get
        // installed instead of silently reporting "already up to date".
        let mut merged = if existing.is_object() {
            existing.clone()
        } else {
            json!({})
        };
        let mut hooks_obj = merged.get("hooks").cloned().unwrap_or_else(|| json!({}));
        if !hooks_obj.is_object() {
            hooks_obj = json!({});
        }

        if let Some(obj) = hooks_obj.as_object_mut() {
            obj.insert("enabled".to_string(), Value::Bool(true));
        }
        let mut events_obj = hooks_obj
            .get("events")
            .cloned()
            .unwrap_or_else(|| json!({}));

        for hook_type in ["PreToolUse", "PostToolUse"] {
            let mut blocks = events_obj
                .get(hook_type)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            // Find or create the matcher block that carries our hook.
            let block_idx = blocks
                .iter()
                .position(|b| {
                    b.get("matcher")
                        .and_then(|m| m.as_str())
                        .map(|m| m == ZCODE_MATCHER)
                        .unwrap_or(false)
                })
                .unwrap_or_else(|| {
                    blocks.push(json!({
                        "matcher": ZCODE_MATCHER,
                        "hooks": []
                    }));
                    blocks.len() - 1
                });

            // Keep user hooks, drop any duplicate git-ai entries, then ensure
            // exactly one git-ai zcode hook remains (migrates an existing
            // claude-preset hook to zcode).
            let mut hooks_array = blocks[block_idx]
                .get("hooks")
                .and_then(|h| h.as_array())
                .cloned()
                .unwrap_or_default();

            hooks_array.retain(|hook| !Self::is_git_ai_process_hook(hook));
            hooks_array.push(Self::desired_hook(params));

            if let Some(block) = blocks[block_idx].as_object_mut() {
                block.insert("hooks".to_string(), Value::Array(hooks_array));
            }

            if let Some(obj) = events_obj.as_object_mut() {
                obj.insert(hook_type.to_string(), Value::Array(blocks));
            }
        }

        if let Some(obj) = hooks_obj.as_object_mut() {
            obj.insert("events".to_string(), events_obj);
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
        let mut hooks_obj = match merged.get("hooks").cloned() {
            Some(h) => h,
            None => return Ok(None),
        };
        let mut events_obj = match hooks_obj.get("events").cloned() {
            Some(e) => e,
            None => return Ok(None),
        };

        let mut changed = false;
        for hook_type in &["PreToolUse", "PostToolUse"] {
            if let Some(blocks) = events_obj
                .get_mut(*hook_type)
                .and_then(|v| v.as_array_mut())
            {
                for block in blocks.iter_mut() {
                    if let Some(hooks_array) = block.get_mut("hooks").and_then(|h| h.as_array_mut())
                    {
                        let original_len = hooks_array.len();
                        hooks_array.retain(|hook| !Self::is_git_ai_process_hook(hook));
                        if hooks_array.len() != original_len {
                            changed = true;
                        }
                    }
                }
            }
        }

        if !changed {
            return Ok(None);
        }

        if let Some(obj) = hooks_obj.as_object_mut() {
            obj.insert("events".to_string(), events_obj);
        }
        if let Some(root) = merged.as_object_mut() {
            root.insert("hooks".to_string(), hooks_obj);
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

    fn check_hooks(&self, _params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        // Detect the tool via its config directory or binary on PATH (mirroring
        // the other installers) so a fresh ZCode install that has not produced
        // a config file yet is still reported as installed and gets hooks set up.
        let tool_installed = home_dir().join(".zcode").exists() || binary_exists("zcode");
        if !tool_installed {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let config_path = Self::config_path();
        if !config_path.exists() {
            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let content = fs::read_to_string(&config_path)?;
        let existing: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));
        let (hooks_installed, hooks_up_to_date) = Self::hook_status(&existing);

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed,
            hooks_up_to_date,
        })
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["zcode"]
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        Self::install_hooks_at(&Self::config_path(), params, dry_run)
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
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_env() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir
            .path()
            .join(".zcode")
            .join("cli")
            .join("config.json");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        (temp_dir, config_path)
    }

    fn binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    fn params() -> HookInstallerParams {
        HookInstallerParams {
            binary_path: binary_path(),
        }
    }

    fn read_config(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn hooks_in_matcher<'a>(config: &'a Value, hook_type: &str) -> Vec<&'a Value> {
        config
            .get("hooks")
            .and_then(|h| h.get("events"))
            .and_then(|e| e.get(hook_type))
            .and_then(|v| v.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .find(|b| {
                        b.get("matcher")
                            .and_then(|m| m.as_str())
                            .map(|m| m == ZCODE_MATCHER)
                            .unwrap_or(false)
                    })
                    .and_then(|b| b.get("hooks").and_then(|h| h.as_array()))
                    .map(|v| v.iter().collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    fn args_of(hook: &Value) -> Vec<String> {
        hook.get("args")
            .and_then(|a| a.as_array())
            .map(|args| {
                args.iter()
                    .filter_map(|a| a.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    // ---- Install scenarios ----

    #[test]
    fn s1_fresh_install_creates_process_hook() {
        let (_td, path) = setup_test_env();
        // File does not exist yet
        fs::remove_file(&path).ok();

        let diff = ZcodeInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_some(), "should produce a diff");

        let config = read_config(&path);
        assert_eq!(
            config.get("hooks").and_then(|h| h.get("enabled")),
            Some(&Value::Bool(true)),
            "hooks.enabled should be true"
        );
        for hook_type in ["PreToolUse", "PostToolUse"] {
            let hooks = hooks_in_matcher(&config, hook_type);
            assert_eq!(
                hooks.len(),
                1,
                "{hook_type}: expected 1 hook in matcher block"
            );
            let hook = hooks[0];
            assert_eq!(hook.get("type").and_then(|t| t.as_str()), Some("process"));
            assert_eq!(
                hook.get("command").and_then(|c| c.as_str()),
                Some("/usr/local/bin/git-ai")
            );
            assert_eq!(hook.get("enabled"), Some(&Value::Bool(true)));
            assert_eq!(
                args_of(hook),
                vec!["checkpoint", "zcode", "--hook-input", "stdin"]
            );
        }
    }

    #[test]
    fn s2_idempotent_already_on_zcode() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "PreToolUse": [{"matcher": ZCODE_MATCHER, "hooks": [
                            {"type":"process","command":"/usr/local/bin/git-ai","enabled":true,"args":ZCODE_HOOK_ARGS}
                        ]}],
                        "PostToolUse": [{"matcher": ZCODE_MATCHER, "hooks": [
                            {"type":"process","command":"/usr/local/bin/git-ai","enabled":true,"args":ZCODE_HOOK_ARGS}
                        ]}]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = ZcodeInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_none(), "should return None when already up-to-date");
    }

    #[test]
    fn s3_migrates_claude_preset_hook_to_zcode() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "PreToolUse": [{"matcher": ZCODE_MATCHER, "hooks": [
                            {"type":"process","command":"/usr/local/bin/git-ai","enabled":true,"args":["checkpoint","claude","--hook-input","stdin"]}
                        ]}],
                        "PostToolUse": [{"matcher": ZCODE_MATCHER, "hooks": [
                            {"type":"process","command":"/usr/local/bin/git-ai","enabled":true,"args":["checkpoint","claude","--hook-input","stdin"]}
                        ]}]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ZcodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let config = read_config(&path);
        for hook_type in ["PreToolUse", "PostToolUse"] {
            let hooks = hooks_in_matcher(&config, hook_type);
            assert_eq!(
                hooks.len(),
                1,
                "{hook_type}: expected exactly 1 git-ai hook"
            );
            assert_eq!(
                args_of(hooks[0]),
                vec!["checkpoint", "zcode", "--hook-input", "stdin"]
            );
        }
    }

    #[test]
    fn s4_preserves_other_top_level_keys() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "mcp": {"servers": {"my-server": {"type": "stdio"}}},
                "plugins": {"enabledPlugins": {"my-plugin": true}}
            }))
            .unwrap(),
        )
        .unwrap();

        ZcodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let config = read_config(&path);
        assert!(
            config.get("mcp").and_then(|m| m.get("servers")).is_some(),
            "mcp block should be preserved"
        );
        assert!(
            config.get("plugins").is_some(),
            "plugins block should be preserved"
        );
        for hook_type in ["PreToolUse", "PostToolUse"] {
            assert_eq!(hooks_in_matcher(&config, hook_type).len(), 1);
        }
    }

    #[test]
    fn s5_preserves_user_hooks_in_matcher_block() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "PreToolUse": [{"matcher": ZCODE_MATCHER, "hooks": [
                            {"type":"process","command":"/opt/my-audit-tool","enabled":true,"args":["--check"]}
                        ]}],
                        "PostToolUse": [{"matcher": ZCODE_MATCHER, "hooks": [
                            {"type":"process","command":"/opt/my-audit-tool","enabled":true,"args":["--check"]}
                        ]}]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ZcodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let config = read_config(&path);
        for hook_type in ["PreToolUse", "PostToolUse"] {
            let hooks = hooks_in_matcher(&config, hook_type);
            assert_eq!(hooks.len(), 2, "{hook_type}: user hook + git-ai");
            assert!(
                hooks.iter().any(|h| {
                    h.get("command").and_then(|c| c.as_str()) == Some("/opt/my-audit-tool")
                }),
                "{hook_type}: user hook should be preserved"
            );
            assert!(
                hooks
                    .iter()
                    .any(|h| args_of(h).first().map(|a| a.as_str()) == Some("checkpoint")),
                "{hook_type}: git-ai hook should be present"
            );
        }
    }

    #[test]
    fn s6_deduplicates_multiple_git_ai_hooks() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "PreToolUse": [{"matcher": ZCODE_MATCHER, "hooks": [
                            {"type":"process","command":"/usr/local/bin/git-ai","enabled":true,"args":["checkpoint","zcode","--hook-input","stdin"]},
                            {"type":"process","command":"/usr/local/bin/git-ai","enabled":true,"args":["checkpoint","zcode","--hook-input","stdin"]}
                        ]}],
                        "PostToolUse": [{"matcher": ZCODE_MATCHER, "hooks": [
                            {"type":"process","command":"/usr/local/bin/git-ai","enabled":true,"args":["checkpoint","zcode","--hook-input","stdin"]}
                        ]}]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ZcodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let config = read_config(&path);
        for hook_type in ["PreToolUse", "PostToolUse"] {
            assert_eq!(
                hooks_in_matcher(&config, hook_type).len(),
                1,
                "{hook_type}: should have exactly 1 after dedup"
            );
        }
    }

    #[test]
    fn s7_install_overwrites_unexpected_config_shape() {
        let (_td, path) = setup_test_env();
        // Top-level value is an array, not an object — install must still
        // produce a valid hooks block instead of silently doing nothing.
        fs::write(&path, "[]").unwrap();

        let diff = ZcodeInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_some(), "should produce a diff for malformed config");

        let config = read_config(&path);
        for hook_type in ["PreToolUse", "PostToolUse"] {
            assert_eq!(hooks_in_matcher(&config, hook_type).len(), 1);
        }
    }

    // ---- Uninstall scenarios ----

    #[test]
    fn u1_uninstall_removes_git_ai_preserves_user_hook() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "PreToolUse": [{"matcher": ZCODE_MATCHER, "hooks": [
                            {"type":"process","command":"/opt/my-audit-tool","enabled":true,"args":["--check"]},
                            {"type":"process","command":"/usr/local/bin/git-ai","enabled":true,"args":["checkpoint","zcode","--hook-input","stdin"]}
                        ]}],
                        "PostToolUse": [{"matcher": ZCODE_MATCHER, "hooks": [
                            {"type":"process","command":"/usr/local/bin/git-ai","enabled":true,"args":["checkpoint","zcode","--hook-input","stdin"]}
                        ]}]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = ZcodeInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_some());

        let config = read_config(&path);
        for hook_type in ["PreToolUse", "PostToolUse"] {
            let hooks = hooks_in_matcher(&config, hook_type);
            assert!(
                !hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("git-ai"))
                        .unwrap_or(false)
                }),
                "{hook_type}: git-ai should be removed"
            );
        }
        let pre_hooks = hooks_in_matcher(&config, "PreToolUse");
        assert!(
            pre_hooks
                .iter()
                .any(|h| h.get("command").and_then(|c| c.as_str()) == Some("/opt/my-audit-tool")),
            "user hook should be preserved"
        );
    }

    #[test]
    fn u2_noop_uninstall_when_no_git_ai() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "PreToolUse": [{"matcher": ZCODE_MATCHER, "hooks": [
                            {"type":"process","command":"/opt/my-audit-tool","enabled":true,"args":["--check"]}
                        ]}]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = ZcodeInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(
            diff.is_none(),
            "should return None when nothing to uninstall"
        );
    }

    // ---- check_hooks scenarios ----

    #[test]
    fn c1_no_hooks_returns_not_installed() {
        let config = json!({});
        let (installed, up_to_date) = ZcodeInstaller::hook_status(&config);
        assert!(!installed);
        assert!(!up_to_date);
    }

    #[test]
    fn c2_zcode_hook_returns_up_to_date() {
        let config = json!({
            "hooks": {
                "enabled": true,
                "events": {
                    "PreToolUse": [{"matcher": ZCODE_MATCHER, "hooks": [
                        {"type":"process","command":"/usr/local/bin/git-ai","enabled":true,"args":["checkpoint","zcode","--hook-input","stdin"]}
                    ]}]
                }
            }
        });
        let (installed, up_to_date) = ZcodeInstaller::hook_status(&config);
        assert!(installed);
        assert!(up_to_date);
    }

    #[test]
    fn c3_claude_preset_hook_returns_installed_but_not_up_to_date() {
        let config = json!({
            "hooks": {
                "enabled": true,
                "events": {
                    "PreToolUse": [{"matcher": ZCODE_MATCHER, "hooks": [
                        {"type":"process","command":"/usr/local/bin/git-ai","enabled":true,"args":["checkpoint","claude","--hook-input","stdin"]}
                    ]}]
                }
            }
        });
        let (installed, up_to_date) = ZcodeInstaller::hook_status(&config);
        assert!(installed, "should be considered installed");
        assert!(
            !up_to_date,
            "should not be up-to-date when on claude preset"
        );
    }

    #[test]
    fn c4_install_creates_missing_parent_dir() {
        let temp_dir = TempDir::new().unwrap();
        // Point to a config.json inside a directory that does NOT exist yet
        let config_path = temp_dir.path().join("missing_dir").join("config.json");
        assert!(!config_path.parent().unwrap().exists());

        let result = ZcodeInstaller::install_hooks_at(&config_path, &params(), false).unwrap();

        assert!(result.is_some(), "should report changes for fresh install");
        assert!(config_path.exists(), "config.json should be created");

        let content: Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).expect("valid JSON");
        assert!(content.get("hooks").and_then(|h| h.get("events")).is_some());
    }
}
