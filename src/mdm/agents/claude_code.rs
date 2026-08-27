use crate::config::Config;
use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::profile_roots::{AgentProfile, agent_profile_roots, install_profile_roots};
use crate::mdm::utils::{
    MIN_CLAUDE_VERSION, binary_exists, generate_diff, get_binary_version,
    is_git_ai_checkpoint_command, normalize_windows_path_for_shell, parse_version,
    version_meets_requirement, write_atomic,
};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

// Command patterns for hooks
const CLAUDE_PRE_TOOL_CMD: &str = "checkpoint claude --hook-input stdin";
const CLAUDE_POST_TOOL_CMD: &str = "checkpoint claude --hook-input stdin";
const CLAUDE_CATCH_ALL_MATCHER: &str = "*";

pub struct ClaudeCodeInstaller;

impl ClaudeCodeInstaller {
    /// Returns `(hooks_installed, hooks_up_to_date)` from a parsed settings value.
    /// `hooks_installed` = git-ai checkpoint command exists in ANY matcher block.
    /// `hooks_up_to_date` = git-ai checkpoint command exists in the `"*"` catch-all block.
    fn hook_status(settings: &Value) -> (bool, bool) {
        let pre_tool_blocks = settings
            .get("hooks")
            .and_then(|h| h.get("PreToolUse"))
            .and_then(|v| v.as_array());

        let Some(blocks) = pre_tool_blocks else {
            return (false, false);
        };

        let mut hooks_installed = false;
        let mut hooks_up_to_date = false;

        for block in blocks {
            let is_catch_all = block
                .get("matcher")
                .and_then(|m| m.as_str())
                .map(|m| m == CLAUDE_CATCH_ALL_MATCHER)
                .unwrap_or(false);

            let has_git_ai = block
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
                .unwrap_or(false);

            if has_git_ai {
                hooks_installed = true;
                if is_catch_all {
                    hooks_up_to_date = true;
                }
            }
        }

        (hooks_installed, hooks_up_to_date)
    }

    fn install_hooks_at(
        settings_path: &Path,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        if let Some(dir) = settings_path.parent() {
            fs::create_dir_all(dir)?;
        }

        let existing_content = if settings_path.exists() {
            fs::read_to_string(settings_path)?
        } else {
            String::new()
        };

        let existing: Value = if existing_content.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&existing_content)?
        };

        let binary_path_str = normalize_windows_path_for_shell(&params.binary_path);
        let pre_tool_cmd = format!("{} {}", binary_path_str, CLAUDE_PRE_TOOL_CMD);
        let post_tool_cmd = format!("{} {}", binary_path_str, CLAUDE_POST_TOOL_CMD);

        let mut merged = existing.clone();
        let mut hooks_obj = merged.get("hooks").cloned().unwrap_or_else(|| json!({}));

        for (hook_type, desired_cmd) in &[
            ("PreToolUse", &pre_tool_cmd),
            ("PostToolUse", &post_tool_cmd),
        ] {
            let mut hook_type_array = hooks_obj
                .get(*hook_type)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            // Step 1: Strip git-ai from every non-catch-all matcher block (migration).
            // Track which blocks we emptied so we can remove them below.
            let mut emptied_by_migration = vec![false; hook_type_array.len()];
            for (i, block) in hook_type_array.iter_mut().enumerate() {
                let is_catch_all = block
                    .get("matcher")
                    .and_then(|m| m.as_str())
                    .map(|m| m == CLAUDE_CATCH_ALL_MATCHER)
                    .unwrap_or(false);
                if !is_catch_all
                    && let Some(hooks) = block.get_mut("hooks").and_then(|h| h.as_array_mut())
                {
                    let before = hooks.len();
                    hooks.retain(|hook| {
                        hook.get("command")
                            .and_then(|c| c.as_str())
                            .map(|cmd| !is_git_ai_checkpoint_command(cmd))
                            .unwrap_or(true)
                    });
                    if hooks.is_empty() && before > 0 {
                        emptied_by_migration[i] = true;
                    }
                }
            }
            // Remove blocks that we emptied; leave pre-existing empty blocks alone.
            let mut i = 0;
            hook_type_array.retain(|_| {
                let remove = emptied_by_migration[i];
                i += 1;
                !remove
            });

            // Step 2: Find or create the "*" catch-all matcher block.
            let catch_all_idx = hook_type_array
                .iter()
                .position(|b| {
                    b.get("matcher")
                        .and_then(|m| m.as_str())
                        .map(|m| m == CLAUDE_CATCH_ALL_MATCHER)
                        .unwrap_or(false)
                })
                .unwrap_or_else(|| {
                    hook_type_array.push(json!({
                        "matcher": CLAUDE_CATCH_ALL_MATCHER,
                        "hooks": []
                    }));
                    hook_type_array.len() - 1
                });

            // Step 3: Ensure exactly one git-ai command in the catch-all block.
            let mut hooks_array = hook_type_array[catch_all_idx]
                .get("hooks")
                .and_then(|h| h.as_array())
                .cloned()
                .unwrap_or_default();

            let mut found_idx: Option<usize> = None;
            let mut needs_update = false;

            for (idx, hook) in hooks_array.iter().enumerate() {
                if let Some(cmd) = hook.get("command").and_then(|c| c.as_str())
                    && is_git_ai_checkpoint_command(cmd)
                    && found_idx.is_none()
                {
                    found_idx = Some(idx);
                    if cmd != *desired_cmd {
                        needs_update = true;
                    }
                }
            }

            match found_idx {
                Some(idx) => {
                    if needs_update {
                        hooks_array[idx] = json!({
                            "type": "command",
                            "command": desired_cmd
                        });
                    }
                    // Remove duplicates: keep the first, drop any subsequent git-ai entries.
                    let keep_idx = idx;
                    let mut current_idx = 0;
                    hooks_array.retain(|hook| {
                        if current_idx == keep_idx {
                            current_idx += 1;
                            true
                        } else if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                            let is_dup = is_git_ai_checkpoint_command(cmd);
                            current_idx += 1;
                            !is_dup
                        } else {
                            current_idx += 1;
                            true
                        }
                    });
                }
                None => {
                    hooks_array.push(json!({
                        "type": "command",
                        "command": desired_cmd
                    }));
                }
            }

            if let Some(matcher_block) = hook_type_array[catch_all_idx].as_object_mut() {
                matcher_block.insert("hooks".to_string(), Value::Array(hooks_array));
            }

            if let Some(obj) = hooks_obj.as_object_mut() {
                obj.insert(hook_type.to_string(), Value::Array(hook_type_array));
            }
        }

        if let Some(root) = merged.as_object_mut() {
            root.insert("hooks".to_string(), hooks_obj);
        }

        if existing == merged {
            return Ok(None);
        }

        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(settings_path, &existing_content, &new_content);

        if !dry_run {
            write_atomic(settings_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }

    fn uninstall_hooks_at(
        settings_path: &Path,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        if !settings_path.exists() {
            return Ok(None);
        }

        let existing_content = fs::read_to_string(settings_path)?;
        let existing: Value = serde_json::from_str(&existing_content)?;

        let mut merged = existing.clone();
        let mut hooks_obj = match merged.get("hooks").cloned() {
            Some(h) => h,
            None => return Ok(None),
        };

        let mut changed = false;

        for hook_type in &["PreToolUse", "PostToolUse"] {
            if let Some(hook_type_array) =
                hooks_obj.get_mut(*hook_type).and_then(|v| v.as_array_mut())
            {
                for matcher_block in hook_type_array.iter_mut() {
                    if let Some(hooks_array) = matcher_block
                        .get_mut("hooks")
                        .and_then(|h| h.as_array_mut())
                    {
                        let original_len = hooks_array.len();
                        hooks_array.retain(|hook| {
                            if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                                !is_git_ai_checkpoint_command(cmd)
                            } else {
                                true
                            }
                        });
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

        if let Some(root) = merged.as_object_mut() {
            root.insert("hooks".to_string(), hooks_obj);
        }

        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(settings_path, &existing_content, &new_content);

        if !dry_run {
            write_atomic(settings_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }
}

impl HookInstaller for ClaudeCodeInstaller {
    fn name(&self) -> &str {
        "Claude Code"
    }

    fn id(&self) -> &str {
        "claude-code"
    }

    fn check_hooks(&self, _params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let has_binary = binary_exists("claude");
        let config = Config::fresh();
        let roots: Vec<_> = agent_profile_roots(AgentProfile::Claude, &config)
            .into_iter()
            .filter(|root| root.is_dir())
            .collect();

        if !has_binary && roots.is_empty() {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        if has_binary
            && let Ok(version_str) = get_binary_version("claude")
            && let Some(version) = parse_version(&version_str)
            && !version_meets_requirement(version, MIN_CLAUDE_VERSION)
        {
            return Err(GitAiError::Generic(format!(
                "Claude Code version {}.{} detected, but minimum version {}.{} is required",
                version.0, version.1, MIN_CLAUDE_VERSION.0, MIN_CLAUDE_VERSION.1
            )));
        }

        let mut statuses = Vec::with_capacity(roots.len());
        for root in roots {
            let settings_path = root.join("settings.json");
            let status = if settings_path.exists() {
                let content = fs::read_to_string(settings_path)?;
                let existing: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));
                Self::hook_status(&existing)
            } else {
                (false, false)
            };
            statuses.push(status);
        }

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: !statuses.is_empty() && statuses.iter().all(|status| status.0),
            hooks_up_to_date: !statuses.is_empty() && statuses.iter().all(|status| status.1),
        })
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["claude"]
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let config = Config::fresh();
        let mut diffs = Vec::new();
        for root in install_profile_roots(AgentProfile::Claude, &config) {
            if let Some(diff) =
                Self::install_hooks_at(&root.join("settings.json"), params, dry_run)?
            {
                diffs.push(format!("{}:\n{}", root.display(), diff));
            }
        }
        Ok((!diffs.is_empty()).then(|| diffs.join("\n")))
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let config = Config::fresh();
        let mut diffs = Vec::new();
        for root in agent_profile_roots(AgentProfile::Claude, &config)
            .into_iter()
            .filter(|root| root.is_dir())
        {
            if let Some(diff) = Self::uninstall_hooks_at(&root.join("settings.json"), dry_run)? {
                diffs.push(format!("{}:\n{}", root.display(), diff));
            }
        }
        Ok((!diffs.is_empty()).then(|| diffs.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdm::utils::{clean_path, normalize_windows_path_for_shell};
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_env() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join(".claude").join("settings.json");
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
        format!("{} {}", binary_path().display(), CLAUDE_PRE_TOOL_CMD)
    }

    fn read_settings(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    #[serial]
    fn install_hooks_updates_default_and_configured_profiles() {
        let temp = TempDir::new().unwrap();
        let home = temp.path();
        let default_root = home.join(".claude");
        let extra_root = home.join(".claude-work");
        fs::create_dir_all(&default_root).unwrap();
        fs::create_dir_all(&extra_root).unwrap();
        fs::create_dir_all(home.join(".git-ai")).unwrap();
        fs::write(
            home.join(".git-ai/config.json"),
            serde_json::json!({
                "agent_profile_roots": { "claude": [extra_root] }
            })
            .to_string(),
        )
        .unwrap();
        let previous_home = std::env::var_os("HOME");
        let previous_override = std::env::var_os("CLAUDE_CONFIG_DIR");
        unsafe {
            std::env::set_var("HOME", home);
            std::env::remove_var("CLAUDE_CONFIG_DIR");
        }

        ClaudeCodeInstaller.install_hooks(&params(), false).unwrap();

        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match previous_override {
            Some(value) => unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", value) },
            None => unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") },
        }
        for root in [default_root, extra_root] {
            assert!(
                root.join("settings.json").exists(),
                "hooks missing in {}",
                root.display()
            );
        }
    }

    fn git_ai_blocks_in(hook_type_array: &[Value]) -> Vec<&Value> {
        hook_type_array
            .iter()
            .filter(|block| {
                block
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(is_git_ai_checkpoint_command)
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
            .collect()
    }

    fn catch_all_block(hook_type_array: &[Value]) -> Option<&Value> {
        hook_type_array.iter().find(|b| {
            b.get("matcher")
                .and_then(|m| m.as_str())
                .map(|m| m == CLAUDE_CATCH_ALL_MATCHER)
                .unwrap_or(false)
        })
    }

    fn hooks_in_catch_all<'a>(settings: &'a Value, hook_type: &str) -> Vec<&'a Value> {
        let Some(blocks) = settings
            .get("hooks")
            .and_then(|h| h.get(hook_type))
            .and_then(|v| v.as_array())
        else {
            return Vec::new();
        };
        catch_all_block(blocks)
            .and_then(|b| b.get("hooks").and_then(|h| h.as_array()))
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    // ---- Install scenarios ----

    #[test]
    fn s1_fresh_install_creates_catch_all_block() {
        let (_td, path) = setup_test_env();
        // File does not exist yet
        fs::remove_file(&path).ok();

        let diff = ClaudeCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_some(), "should produce a diff");

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            let hooks = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(hooks.len(), 1, "{hook_type}: expected 1 hook in catch-all");
            assert_eq!(
                hooks[0].get("command").and_then(|c| c.as_str()).unwrap(),
                expected_cmd()
            );
        }
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
                    "PostToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": cmd}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = ClaudeCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_none(), "should return None when already up-to-date");
    }

    #[test]
    fn s3_migration_old_matcher_no_user_hooks() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [{"matcher": "Write|Edit|MultiEdit", "hooks": [{"type":"command","command": cmd}]}],
                    "PostToolUse": [{"matcher": "Write|Edit|MultiEdit", "hooks": [{"type":"command","command": cmd}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ClaudeCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            // git-ai must be in the catch-all block
            let hooks = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(hooks.len(), 1, "{hook_type}: expected git-ai in catch-all");

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
                    "PreToolUse": [{
                        "matcher": "Write|Edit|MultiEdit",
                        "hooks": [
                            {"type":"command","command": "echo before"},
                            {"type":"command","command": cmd}
                        ]
                    }],
                    "PostToolUse": [{
                        "matcher": "Write|Edit|MultiEdit",
                        "hooks": [
                            {"type":"command","command": "prettier --write"},
                            {"type":"command","command": cmd}
                        ]
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ClaudeCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for (hook_type, user_cmd) in &[
            ("PreToolUse", "echo before"),
            ("PostToolUse", "prettier --write"),
        ] {
            // git-ai in catch-all
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 1);

            // user hook still in old matcher block
            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let old_block = blocks
                .iter()
                .find(|b| b.get("matcher").and_then(|m| m.as_str()) == Some("Write|Edit|MultiEdit"))
                .expect("old matcher block should still exist");
            let old_hooks = old_block.get("hooks").and_then(|h| h.as_array()).unwrap();
            assert!(
                old_hooks
                    .iter()
                    .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(*user_cmd)),
                "{hook_type}: user hook '{user_cmd}' should still be in old matcher block"
            );
            // git-ai NOT in old block
            assert!(
                !old_hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(is_git_ai_checkpoint_command)
                        .unwrap_or(false)
                }),
                "{hook_type}: git-ai should not be in old matcher block after migration"
            );
        }
    }

    #[test]
    fn s5_fresh_install_user_has_old_matcher_hook() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [{"matcher": "Write|Edit|MultiEdit", "hooks": [{"type":"command","command": "prettier --write"}]}],
                    "PostToolUse": [{"matcher": "Write|Edit|MultiEdit", "hooks": [{"type":"command","command": "prettier --write"}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ClaudeCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            // git-ai in new catch-all block
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 1);

            // user hook untouched in old block
            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let old_block = blocks
                .iter()
                .find(|b| b.get("matcher").and_then(|m| m.as_str()) == Some("Write|Edit|MultiEdit"))
                .unwrap();
            let old_hooks = old_block.get("hooks").and_then(|h| h.as_array()).unwrap();
            assert_eq!(old_hooks.len(), 1);
            assert_eq!(
                old_hooks[0]
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap(),
                "prettier --write"
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
                    "PreToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": "my-audit-tool"}]}],
                    "PostToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": "my-audit-tool"}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ClaudeCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(
                catch_all.len(),
                2,
                "{hook_type}: should have user hook + git-ai"
            );
            assert_eq!(
                catch_all[0]
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap(),
                "my-audit-tool",
                "user hook should be first"
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
                "PreToolUse": [{"matcher": "*", "hooks": [
                    {"type":"command","command": "my-audit-tool"},
                    {"type":"command","command": cmd}
                ]}],
                "PostToolUse": [{"matcher": "*", "hooks": [
                    {"type":"command","command": "my-audit-tool"},
                    {"type":"command","command": cmd}
                ]}]
            }
        });
        fs::write(&path, serde_json::to_string_pretty(&before).unwrap()).unwrap();

        let diff = ClaudeCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_none(), "should be idempotent");
    }

    #[test]
    fn s8_deduplication_git_ai_in_both_catch_all_and_old_matcher() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        let user_cmd = "echo user-hook";
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [
                        {"matcher": "*", "hooks": [{"type":"command","command": cmd}]},
                        {"matcher": "Write|Edit|MultiEdit", "hooks": [
                            {"type":"command","command": user_cmd},
                            {"type":"command","command": cmd}
                        ]}
                    ],
                    "PostToolUse": [
                        {"matcher": "*", "hooks": [{"type":"command","command": cmd}]},
                        {"matcher": "Write|Edit|MultiEdit", "hooks": [
                            {"type":"command","command": user_cmd},
                            {"type":"command","command": cmd}
                        ]}
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ClaudeCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            // exactly one git-ai in catch-all
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 1);

            // old matcher block has user hook but NOT git-ai
            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let old_block = blocks
                .iter()
                .find(|b| b.get("matcher").and_then(|m| m.as_str()) == Some("Write|Edit|MultiEdit"))
                .unwrap();
            let old_hooks = old_block.get("hooks").and_then(|h| h.as_array()).unwrap();
            assert!(
                old_hooks
                    .iter()
                    .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(user_cmd))
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
    fn s9_deduplication_two_git_ai_in_catch_all_block() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [{"matcher": "*", "hooks": [
                        {"type":"command","command": cmd},
                        {"type":"command","command": cmd}
                    ]}],
                    "PostToolUse": [{"matcher": "*", "hooks": [
                        {"type":"command","command": cmd},
                        {"type":"command","command": cmd}
                    ]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ClaudeCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(
                catch_all.len(),
                1,
                "{hook_type}: should have exactly 1 after dedup"
            );
        }
    }

    #[test]
    fn s10_stale_command_upgraded_in_catch_all() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": "/old/path/git-ai checkpoint claude --hook-input stdin"}]}],
                    "PostToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": "/old/path/git-ai checkpoint claude --hook-input stdin"}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ClaudeCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

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

    #[test]
    fn s11_git_ai_in_arbitrary_old_matcher_migrated() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [
                        {"matcher": "Bash", "hooks": [
                            {"type":"command","command": "user-bash-hook"},
                            {"type":"command","command": cmd}
                        ]}
                    ],
                    "PostToolUse": [
                        {"matcher": "Bash", "hooks": [
                            {"type":"command","command": "user-bash-hook"},
                            {"type":"command","command": cmd}
                        ]}
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ClaudeCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            // git-ai now in catch-all
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert_eq!(catch_all.len(), 1);

            // user-bash-hook preserved in Bash block, git-ai removed
            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let bash_block = blocks
                .iter()
                .find(|b| b.get("matcher").and_then(|m| m.as_str()) == Some("Bash"))
                .unwrap();
            let bash_hooks = bash_block.get("hooks").and_then(|h| h.as_array()).unwrap();
            assert!(
                bash_hooks
                    .iter()
                    .any(|h| h.get("command").and_then(|c| c.as_str()) == Some("user-bash-hook"))
            );
            assert!(!bash_hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(is_git_ai_checkpoint_command)
                    .unwrap_or(false)
            }));
        }
    }

    #[test]
    fn s12_git_ai_spread_across_multiple_old_blocks() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [
                        {"matcher": "Write|Edit|MultiEdit", "hooks": [{"type":"command","command": cmd}]},
                        {"matcher": "Bash", "hooks": [{"type":"command","command": cmd}]}
                    ],
                    "PostToolUse": [
                        {"matcher": "Write|Edit|MultiEdit", "hooks": [{"type":"command","command": cmd}]},
                        {"matcher": "Bash", "hooks": [{"type":"command","command": cmd}]}
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ClaudeCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            // exactly one git-ai total, in catch-all
            let all_blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let git_ai_blocks = git_ai_blocks_in(all_blocks);
            assert_eq!(git_ai_blocks.len(), 1);
            assert_eq!(
                git_ai_blocks[0]
                    .get("matcher")
                    .and_then(|m| m.as_str())
                    .unwrap(),
                CLAUDE_CATCH_ALL_MATCHER
            );
        }
    }

    #[test]
    fn s13_hook_types_handled_independently() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        // PreToolUse: git-ai on old matcher; PostToolUse: git-ai already on catch-all
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [{"matcher": "Write|Edit|MultiEdit", "hooks": [{"type":"command","command": cmd}]}],
                    "PostToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": cmd}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ClaudeCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();

        let settings = read_settings(&path);

        // PreToolUse: migrated to catch-all
        let pre_catch = hooks_in_catch_all(&settings, "PreToolUse");
        assert_eq!(pre_catch.len(), 1);

        // PostToolUse: unchanged, still exactly one in catch-all
        let post_catch = hooks_in_catch_all(&settings, "PostToolUse");
        assert_eq!(post_catch.len(), 1);
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
                    "PostToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": cmd}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = ClaudeCodeInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_some());

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            let catch_all = hooks_in_catch_all(&settings, hook_type);
            assert!(
                !catch_all.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(is_git_ai_checkpoint_command)
                        .unwrap_or(false)
                }),
                "{hook_type}: git-ai should be removed"
            );
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
                    "PreToolUse": [{"matcher": "Write|Edit|MultiEdit", "hooks": [
                        {"type":"command","command": "echo before"},
                        {"type":"command","command": cmd}
                    ]}],
                    "PostToolUse": [{"matcher": "Write|Edit|MultiEdit", "hooks": [
                        {"type":"command","command": "echo before"},
                        {"type":"command","command": cmd}
                    ]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ClaudeCodeInstaller::uninstall_hooks_at(&path, false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            let blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            let old_block = blocks
                .iter()
                .find(|b| b.get("matcher").and_then(|m| m.as_str()) == Some("Write|Edit|MultiEdit"))
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
    fn u3_uninstall_from_multiple_blocks_preserves_user_hooks() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        let user = "echo user";
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [
                        {"matcher": "*", "hooks": [{"type":"command","command": cmd}, {"type":"command","command": user}]},
                        {"matcher": "Write|Edit|MultiEdit", "hooks": [{"type":"command","command": cmd}]}
                    ],
                    "PostToolUse": [
                        {"matcher": "*", "hooks": [{"type":"command","command": cmd}]},
                        {"matcher": "Write|Edit|MultiEdit", "hooks": [{"type":"command","command": cmd}, {"type":"command","command": user}]}
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ClaudeCodeInstaller::uninstall_hooks_at(&path, false).unwrap();

        let settings = read_settings(&path);
        for hook_type in &["PreToolUse", "PostToolUse"] {
            let all_blocks = settings
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            // No git-ai anywhere
            let empty: Vec<Value> = Vec::new();
            for block in all_blocks {
                let hooks = block
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .unwrap_or(&empty);
                assert!(!hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(is_git_ai_checkpoint_command)
                        .unwrap_or(false)
                }));
            }
            // user hook still present somewhere
            let empty: Vec<Value> = Vec::new();
            let all_hooks: Vec<_> = all_blocks
                .iter()
                .flat_map(|b| {
                    b.get("hooks")
                        .and_then(|h| h.as_array())
                        .unwrap_or(&empty)
                        .iter()
                })
                .collect();
            assert!(
                all_hooks
                    .iter()
                    .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(user))
            );
        }
    }

    #[test]
    fn u4_noop_uninstall_when_no_git_ai() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PreToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": "echo hello"}]}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = ClaudeCodeInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(
            diff.is_none(),
            "should return None when nothing to uninstall"
        );
    }

    // ---- check_hooks scenarios ----

    #[test]
    fn c1_no_hooks_returns_not_installed() {
        let settings = json!({});
        let (installed, up_to_date) = ClaudeCodeInstaller::hook_status(&settings);
        assert!(!installed);
        assert!(!up_to_date);
    }

    #[test]
    fn c2_git_ai_in_catch_all_returns_up_to_date() {
        let cmd = expected_cmd();
        let settings = json!({
            "hooks": {
                "PreToolUse": [{"matcher": "*", "hooks": [{"type":"command","command": cmd}]}]
            }
        });
        let (installed, up_to_date) = ClaudeCodeInstaller::hook_status(&settings);
        assert!(installed);
        assert!(up_to_date);
    }

    #[test]
    fn c3_git_ai_only_in_old_matcher_returns_installed_but_not_up_to_date() {
        let cmd = expected_cmd();
        let settings = json!({
            "hooks": {
                "PreToolUse": [{"matcher": "Write|Edit|MultiEdit", "hooks": [{"type":"command","command": cmd}]}]
            }
        });
        let (installed, up_to_date) = ClaudeCodeInstaller::hook_status(&settings);
        assert!(installed, "should be considered installed");
        assert!(!up_to_date, "should not be up-to-date when on old matcher");
    }

    // ---- Path / Windows tests (preserved from original) ----

    #[test]
    fn test_claude_hook_commands_no_windows_extended_path_prefix() {
        let raw_path = PathBuf::from(r"\\?\C:\Users\USERNAME\.git-ai\bin\git-ai.exe");
        let binary_path = clean_path(raw_path);

        let binary_path_str = normalize_windows_path_for_shell(&binary_path);
        let pre_tool_cmd = format!("{} {}", binary_path_str, CLAUDE_PRE_TOOL_CMD);
        let post_tool_cmd = format!("{} {}", binary_path_str, CLAUDE_POST_TOOL_CMD);

        assert!(
            !pre_tool_cmd.contains(r"\\?\"),
            "PreToolUse command should not contain \\\\?\\ prefix, got: {}",
            pre_tool_cmd
        );
        assert!(
            !post_tool_cmd.contains(r"\\?\"),
            "PostToolUse command should not contain \\\\?\\ prefix, got: {}",
            post_tool_cmd
        );
        assert!(
            pre_tool_cmd.contains("checkpoint claude"),
            "command should still contain checkpoint args"
        );
    }

    #[test]
    fn test_claude_hook_commands_use_forward_slash_path_on_windows() {
        let binary_path = PathBuf::from(r"C:\Users\Administrator\.git-ai\bin\git-ai.exe");
        let binary_path_str = normalize_windows_path_for_shell(&binary_path);
        let pre_tool_cmd = format!("{} {}", binary_path_str, CLAUDE_PRE_TOOL_CMD);
        let post_tool_cmd = format!("{} {}", binary_path_str, CLAUDE_POST_TOOL_CMD);

        assert_eq!(
            pre_tool_cmd,
            "C:/Users/Administrator/.git-ai/bin/git-ai.exe checkpoint claude --hook-input stdin",
            "PreToolUse command should use forward-slash path format"
        );
        assert_eq!(
            post_tool_cmd,
            "C:/Users/Administrator/.git-ai/bin/git-ai.exe checkpoint claude --hook-input stdin",
            "PostToolUse command should use forward-slash path format"
        );
    }

    #[test]
    fn test_claude_hook_commands_preserve_unix_path() {
        let binary_path = PathBuf::from("/usr/local/bin/git-ai");
        let binary_path_str = normalize_windows_path_for_shell(&binary_path);
        let pre_tool_cmd = format!("{} {}", binary_path_str, CLAUDE_PRE_TOOL_CMD);

        assert_eq!(
            pre_tool_cmd, "/usr/local/bin/git-ai checkpoint claude --hook-input stdin",
            "Unix paths should be preserved unchanged"
        );
    }

    /// Regression test for #1039: install_hooks_at should succeed even when
    /// the parent directory does not yet exist.
    #[test]
    fn test_install_hooks_creates_missing_parent_dir() {
        let temp_dir = TempDir::new().unwrap();
        // Point to a settings.json inside a directory that does NOT exist yet
        let settings_path = temp_dir.path().join("missing_dir").join("settings.json");
        assert!(!settings_path.parent().unwrap().exists());

        let result =
            ClaudeCodeInstaller::install_hooks_at(&settings_path, &params(), false).unwrap();

        assert!(result.is_some(), "should report changes for fresh install");
        assert!(settings_path.exists(), "settings.json should be created");

        let content: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).expect("valid JSON");
        let hooks = content.get("hooks").expect("hooks key should exist");
        assert!(hooks.get("PreToolUse").is_some());
        assert!(hooks.get("PostToolUse").is_some());
    }
}
