use crate::error::GitAiError;
use crate::operations::mdm::hooks_merge::is_git_ai_checkpoint_command;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use std::path::Path;
use toml::Value as TomlValue;
use toml::map::Map;

use super::{CODEX_HOOK_EVENTS, CodexInstaller};

// --- Parsing ---

impl CodexInstaller {
    pub(super) fn parse_config_toml(content: &str) -> Result<TomlValue, GitAiError> {
        if content.trim().is_empty() {
            return Ok(TomlValue::Table(Map::new()));
        }

        let parsed: TomlValue = toml::from_str(content)
            .map_err(|e| GitAiError::Generic(format!("Failed to parse Codex config.toml: {e}")))?;

        if !parsed.is_table() {
            return Err(GitAiError::Generic(
                "Codex config.toml root must be a TOML table".to_string(),
            ));
        }

        Ok(parsed)
    }

    pub(super) fn notify_args_from_config(config: &TomlValue) -> Option<Vec<String>> {
        let arr = config.get("notify")?.as_array()?;
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            out.push(item.as_str()?.to_string());
        }
        Some(out)
    }

    pub(super) fn is_git_ai_codex_command(cmd: &str) -> bool {
        is_git_ai_checkpoint_command(cmd) && cmd.contains("checkpoint codex")
    }

    pub(super) fn is_git_ai_codex_notify_args(args: &[String]) -> bool {
        if args.len() < 4 {
            return false;
        }

        let has_git_ai_bin = args
            .first()
            .map(|bin| {
                bin == "git-ai"
                    || bin.ends_with("/git-ai")
                    || bin.ends_with("\\git-ai")
                    || bin.ends_with("/git-ai.exe")
                    || bin.ends_with("\\git-ai.exe")
            })
            .unwrap_or(false);

        let has_checkpoint_codex = args
            .windows(2)
            .any(|window| window[0] == "checkpoint" && window[1] == "codex");
        let has_hook_input = args.iter().any(|arg| arg == "--hook-input");

        has_git_ai_bin && has_checkpoint_codex && has_hook_input
    }

    pub(super) fn event_name_to_snake_case(event: &str) -> &'static str {
        match event {
            "PreToolUse" => "pre_tool_use",
            "PostToolUse" => "post_tool_use",
            "Stop" => "stop",
            _ => unreachable!("unknown Codex hook event: {event}"),
        }
    }
}

// --- Trust hash computation ---

impl CodexInstaller {
    pub(super) fn canonical_json(value: &JsonValue) -> JsonValue {
        match value {
            JsonValue::Object(map) => {
                let mut sorted = serde_json::Map::new();
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for key in keys {
                    sorted.insert(key.clone(), Self::canonical_json(&map[key]));
                }
                JsonValue::Object(sorted)
            }
            JsonValue::Array(items) => {
                JsonValue::Array(items.iter().map(Self::canonical_json).collect())
            }
            other => other.clone(),
        }
    }

    pub(super) fn compute_trust_hash(
        event_name_snake: &str,
        command: &str,
    ) -> Result<String, GitAiError> {
        let mut handler = Map::new();
        handler.insert("type".to_string(), TomlValue::String("command".to_string()));
        handler.insert("async".to_string(), TomlValue::Boolean(false));
        handler.insert(
            "command".to_string(),
            TomlValue::String(command.to_string()),
        );
        handler.insert("timeout".to_string(), TomlValue::Integer(600));

        let mut identity = Map::new();
        identity.insert(
            "event_name".to_string(),
            TomlValue::String(event_name_snake.to_string()),
        );
        identity.insert(
            "hooks".to_string(),
            TomlValue::Array(vec![TomlValue::Table(handler)]),
        );

        let toml_value = TomlValue::Table(identity);
        let json_value = serde_json::to_value(&toml_value).map_err(|e| {
            GitAiError::Generic(format!(
                "Failed to convert TOML to JSON for trust hash: {e}"
            ))
        })?;
        let canonical = Self::canonical_json(&json_value);
        let bytes = serde_json::to_vec(&canonical).map_err(|e| {
            GitAiError::Generic(format!("Failed to serialize JSON for trust hash: {e}"))
        })?;

        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let hash = hasher.finalize();
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        Ok(format!("sha256:{hex}"))
    }
}

// --- Feature flag and inline hook detection ---

impl CodexInstaller {
    pub(super) fn config_hooks_feature_enabled(config: &TomlValue) -> bool {
        let features = config.get("features");
        let new_flag = features
            .and_then(|v| v.get("hooks"))
            .and_then(|v| v.as_bool())
            == Some(true);
        let legacy_flag = features
            .and_then(|v| v.get("codex_hooks"))
            .and_then(|v| v.as_bool())
            == Some(true);
        new_flag || legacy_flag
    }

    pub(super) fn config_has_inline_hooks(config: &TomlValue) -> bool {
        CODEX_HOOK_EVENTS.iter().all(|event_name| {
            config
                .get("hooks")
                .and_then(|hooks| hooks.get(*event_name))
                .and_then(|value| value.as_array())
                .map(|blocks| {
                    blocks.iter().any(|block| {
                        let is_catch_all = block.get("matcher").is_none()
                            || block
                                .get("matcher")
                                .and_then(|v| v.as_str())
                                .map(|s| s == "*")
                                .unwrap_or(false);
                        is_catch_all
                            && block
                                .get("hooks")
                                .and_then(|value| value.as_array())
                                .map(|hooks| {
                                    hooks.iter().any(|hook| {
                                        hook.get("command")
                                            .and_then(|value| value.as_str())
                                            .map(Self::is_git_ai_codex_command)
                                            .unwrap_or(false)
                                    })
                                })
                                .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        })
    }
}

// --- Config mutation ---

impl CodexInstaller {
    pub(super) fn config_with_hooks_feature_enabled(
        config: &TomlValue,
    ) -> Result<TomlValue, GitAiError> {
        let mut merged = Self::remove_notify_if_git_ai(config)?.unwrap_or(config.clone());
        let root = merged
            .as_table_mut()
            .ok_or_else(|| GitAiError::Generic("Codex config root must be a table".to_string()))?;

        // Set [features].hooks = true (replacing legacy codex_hooks if present)
        if let Some(features) = root.get_mut("features").and_then(|v| v.as_table_mut()) {
            features.remove("codex_hooks");
            features.insert("hooks".to_string(), TomlValue::Boolean(true));
        } else {
            root.insert(
                "features".to_string(),
                TomlValue::Table(Map::from_iter([(
                    "hooks".to_string(),
                    TomlValue::Boolean(true),
                )])),
            );
        }

        Ok(merged)
    }

    pub(super) fn config_with_installed_hooks(
        config: &TomlValue,
        binary_path: &Path,
    ) -> Result<TomlValue, GitAiError> {
        let mut merged = Self::config_with_hooks_feature_enabled(config)?;
        let root = merged
            .as_table_mut()
            .ok_or_else(|| GitAiError::Generic("Codex config root must be a table".to_string()))?;

        // Add inline hooks to config.toml under [hooks] table
        let desired_command = Self::desired_command(binary_path);
        let hooks_table = root
            .entry("hooks")
            .or_insert_with(|| TomlValue::Table(Map::new()));
        if !hooks_table.is_table() {
            *hooks_table = TomlValue::Table(Map::new());
        }
        let hooks_obj = hooks_table.as_table_mut().ok_or_else(|| {
            GitAiError::Generic("Codex config hooks field must be a table".to_string())
        })?;

        let mut installed_positions: Vec<(&str, usize, usize)> = Vec::new();

        for event_name in CODEX_HOOK_EVENTS {
            let blocks = hooks_obj
                .get(event_name)
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            let mut cleaned_blocks = Vec::new();

            for block in blocks {
                let mut cleaned_block = block;
                let original_hook_count = cleaned_block
                    .get("hooks")
                    .and_then(|value| value.as_array())
                    .map(|hooks| hooks.len())
                    .unwrap_or(0);

                let cleaned_hooks = cleaned_block
                    .get("hooks")
                    .and_then(|value| value.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|hook| {
                        hook.get("command")
                            .and_then(|value| value.as_str())
                            .map(|cmd| !Self::is_git_ai_codex_command(cmd))
                            .unwrap_or(true)
                    })
                    .collect::<Vec<_>>();

                if let Some(block_tbl) = cleaned_block.as_table_mut() {
                    block_tbl.insert("hooks".to_string(), TomlValue::Array(cleaned_hooks));
                }

                let remaining_hook_count = cleaned_block
                    .get("hooks")
                    .and_then(|value| value.as_array())
                    .map(|hooks| hooks.len())
                    .unwrap_or(0);
                if remaining_hook_count > 0 || original_hook_count == 0 {
                    cleaned_blocks.push(cleaned_block);
                }
            }

            let target_idx = cleaned_blocks
                .iter()
                .position(|block| block.get("matcher").is_none())
                .unwrap_or_else(|| {
                    cleaned_blocks.push(TomlValue::Table(Map::from_iter([(
                        "hooks".to_string(),
                        TomlValue::Array(Vec::new()),
                    )])));
                    cleaned_blocks.len() - 1
                });

            if let Some(hooks_array) = cleaned_blocks[target_idx]
                .get_mut("hooks")
                .and_then(|value| value.as_array_mut())
            {
                let handler_idx = hooks_array.len();
                let mut hook_entry = Map::new();
                hook_entry.insert("type".to_string(), TomlValue::String("command".to_string()));
                hook_entry.insert(
                    "command".to_string(),
                    TomlValue::String(desired_command.clone()),
                );
                hooks_array.push(TomlValue::Table(hook_entry));
                installed_positions.push((event_name, target_idx, handler_idx));
            }

            hooks_obj.insert(event_name.to_string(), TomlValue::Array(cleaned_blocks));
        }

        // Write trust state so Codex auto-trusts our hooks without TUI approval
        let config_path_str = Self::config_path().to_string_lossy().to_string();
        let state_table = hooks_obj
            .entry("state")
            .or_insert_with(|| TomlValue::Table(Map::new()));
        if !state_table.is_table() {
            *state_table = TomlValue::Table(Map::new());
        }
        let state_obj = state_table.as_table_mut().ok_or_else(|| {
            GitAiError::Generic("Codex config hooks.state must be a table".to_string())
        })?;

        for (event_name, group_idx, handler_idx) in &installed_positions {
            let snake_name = Self::event_name_to_snake_case(event_name);
            let state_key = format!(
                "{}:{}:{}:{}",
                config_path_str, snake_name, group_idx, handler_idx
            );
            let trust_hash = Self::compute_trust_hash(snake_name, &desired_command)?;

            let mut entry = Map::new();
            entry.insert("enabled".to_string(), TomlValue::Boolean(true));
            entry.insert("trusted_hash".to_string(), TomlValue::String(trust_hash));
            state_obj.insert(state_key, TomlValue::Table(entry));
        }

        Ok(merged)
    }

    pub(super) fn remove_notify_if_git_ai(
        config: &TomlValue,
    ) -> Result<Option<TomlValue>, GitAiError> {
        let Some(notify_args) = Self::notify_args_from_config(config) else {
            return Ok(None);
        };

        if !Self::is_git_ai_codex_notify_args(&notify_args) {
            return Ok(None);
        }

        let mut merged = config.clone();
        let root = merged
            .as_table_mut()
            .ok_or_else(|| GitAiError::Generic("Codex config root must be a table".to_string()))?;
        root.remove("notify");
        Ok(Some(merged))
    }

    pub(super) fn remove_inline_hooks_from_config(
        config: &TomlValue,
    ) -> Result<(TomlValue, bool), GitAiError> {
        let mut merged = config.clone();
        let root = merged
            .as_table_mut()
            .ok_or_else(|| GitAiError::Generic("Codex config root must be a table".to_string()))?;

        let Some(hooks_table) = root.get_mut("hooks") else {
            return Ok((merged, false));
        };
        if !hooks_table.is_table() {
            return Ok((merged, false));
        }
        let hooks_obj = hooks_table.as_table_mut().ok_or_else(|| {
            GitAiError::Generic("Codex config hooks field must be a table".to_string())
        })?;

        let mut changed = false;
        for event_name in CODEX_HOOK_EVENTS {
            let Some(blocks) = hooks_obj.get(event_name).and_then(|value| value.as_array()) else {
                continue;
            };

            let mut cleaned_blocks = Vec::new();
            for block in blocks.clone() {
                let mut cleaned_block = block;
                let original_hook_count = cleaned_block
                    .get("hooks")
                    .and_then(|value| value.as_array())
                    .map(|hooks| hooks.len())
                    .unwrap_or(0);
                let cleaned_hooks = cleaned_block
                    .get("hooks")
                    .and_then(|value| value.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|hook| {
                        hook.get("command")
                            .and_then(|value| value.as_str())
                            .map(|cmd| !Self::is_git_ai_codex_command(cmd))
                            .unwrap_or(true)
                    })
                    .collect::<Vec<_>>();
                if cleaned_hooks.len() != original_hook_count {
                    changed = true;
                }

                if let Some(block_tbl) = cleaned_block.as_table_mut() {
                    block_tbl.insert("hooks".to_string(), TomlValue::Array(cleaned_hooks));
                }

                let remaining_hook_count = cleaned_block
                    .get("hooks")
                    .and_then(|value| value.as_array())
                    .map(|hooks| hooks.len())
                    .unwrap_or(0);
                if remaining_hook_count > 0 {
                    cleaned_blocks.push(cleaned_block);
                }
            }

            if cleaned_blocks.is_empty() {
                hooks_obj.remove(event_name);
            } else {
                hooks_obj.insert(event_name.to_string(), TomlValue::Array(cleaned_blocks));
            }
        }

        // Remove trust state entries for git-ai hooks
        let config_path_str = Self::config_path().to_string_lossy().to_string();
        if let Some(state_table) = hooks_obj.get_mut("state").and_then(|v| v.as_table_mut()) {
            let keys_to_remove: Vec<String> = state_table
                .keys()
                .filter(|key| {
                    key.starts_with(&config_path_str)
                        && CODEX_HOOK_EVENTS.iter().any(|event| {
                            let snake = Self::event_name_to_snake_case(event);
                            key.contains(&format!(":{snake}:"))
                        })
                })
                .cloned()
                .collect();
            for key in &keys_to_remove {
                state_table.remove(key);
                changed = true;
            }
            if state_table.is_empty() {
                hooks_obj.remove("state");
            }
        }

        // Remove [hooks] table entirely if empty
        if hooks_obj.is_empty() {
            root.remove("hooks");
        }

        Ok((merged, changed))
    }

    pub(super) fn remove_feature_flags(config: &TomlValue) -> Result<TomlValue, GitAiError> {
        let mut merged = config.clone();
        let root = merged
            .as_table_mut()
            .ok_or_else(|| GitAiError::Generic("Codex config root must be a table".to_string()))?;

        if let Some(features) = root
            .get_mut("features")
            .and_then(|value| value.as_table_mut())
        {
            features.remove("hooks");
            features.remove("codex_hooks");
            if features.is_empty() {
                root.remove("features");
            }
        }

        Ok(merged)
    }
}
