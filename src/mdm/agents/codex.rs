use crate::config::{CodexHooksFormat, Config};
#[cfg(unix)]
use crate::daemon::DaemonConfig;
use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{
    binary_exists, codex_home_dir, generate_diff, is_git_ai_checkpoint_command, write_atomic,
};
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use toml::Value as TomlValue;
use toml::map::Map;

const CODEX_CHECKPOINT_CMD: &str = "checkpoint codex --hook-input stdin";
const CODEX_HOOK_EVENTS: [&str; 3] = ["PreToolUse", "PostToolUse", "Stop"];

pub struct CodexInstaller;

impl CodexInstaller {
    fn config_path() -> PathBuf {
        codex_home_dir().join("config.toml")
    }

    fn hooks_json_path() -> PathBuf {
        codex_home_dir().join("hooks.json")
    }

    fn desired_command(binary_path: &Path) -> String {
        format!("{} {}", binary_path.display(), CODEX_CHECKPOINT_CMD)
    }

    fn parse_config_toml(content: &str) -> Result<TomlValue, GitAiError> {
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

    fn parse_hooks_json(content: &str) -> Result<JsonValue, GitAiError> {
        if content.trim().is_empty() {
            return Ok(json!({}));
        }

        let parsed: JsonValue = serde_json::from_str(content)?;
        if !parsed.is_object() {
            return Err(GitAiError::Generic(
                "Codex hooks.json root must be a JSON object".to_string(),
            ));
        }
        Ok(parsed)
    }

    fn notify_args_from_config(config: &TomlValue) -> Option<Vec<String>> {
        let arr = config.get("notify")?.as_array()?;
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            out.push(item.as_str()?.to_string());
        }
        Some(out)
    }

    fn is_git_ai_codex_command(cmd: &str) -> bool {
        is_git_ai_checkpoint_command(cmd) && cmd.contains("checkpoint codex")
    }

    fn is_git_ai_codex_notify_args(args: &[String]) -> bool {
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

    fn event_name_to_snake_case(event: &str) -> &'static str {
        match event {
            "PreToolUse" => "pre_tool_use",
            "PostToolUse" => "post_tool_use",
            "Stop" => "stop",
            _ => unreachable!("unknown Codex hook event: {event}"),
        }
    }

    fn canonical_json(value: &JsonValue) -> JsonValue {
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

    fn compute_trust_hash(event_name_snake: &str, command: &str) -> Result<String, GitAiError> {
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

    fn config_hooks_feature_enabled(config: &TomlValue) -> bool {
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

    fn config_has_inline_hooks(config: &TomlValue) -> bool {
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

    fn config_with_hooks_feature_enabled(config: &TomlValue) -> Result<TomlValue, GitAiError> {
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

    fn config_with_trace_socket_allowed(config: &TomlValue) -> Result<TomlValue, GitAiError> {
        #[cfg(not(unix))]
        return Ok(config.clone());

        #[cfg(unix)]
        {
            let trace_socket_path = DaemonConfig::from_env_or_default_paths()?.trace_socket_path;
            let mut merged = config.clone();
            let root = merged.as_table_mut().ok_or_else(|| {
                GitAiError::Generic("Codex config root must be a table".to_string())
            })?;
            let features = root
                .entry("features")
                .or_insert_with(|| TomlValue::Table(Map::new()));
            if !features.is_table() {
                *features = TomlValue::Table(Map::new());
            }
            let features = features.as_table_mut().ok_or_else(|| {
                GitAiError::Generic("Codex config features field must be a table".to_string())
            })?;

            let network_proxy = features
                .entry("network_proxy")
                .or_insert_with(|| TomlValue::Table(Map::new()));
            if let Some(enabled) = network_proxy.as_bool() {
                *network_proxy = TomlValue::Table(Map::from_iter([(
                    "enabled".to_string(),
                    TomlValue::Boolean(enabled),
                )]));
            } else if !network_proxy.is_table() {
                *network_proxy = TomlValue::Table(Map::new());
            }
            let network_proxy = network_proxy.as_table_mut().ok_or_else(|| {
                GitAiError::Generic(
                    "Codex config features.network_proxy field must be a table".to_string(),
                )
            })?;
            network_proxy
                .entry("enabled")
                .or_insert(TomlValue::Boolean(true));

            let unix_sockets = network_proxy
                .entry("unix_sockets")
                .or_insert_with(|| TomlValue::Table(Map::new()));
            if !unix_sockets.is_table() {
                *unix_sockets = TomlValue::Table(Map::new());
            }
            unix_sockets
                .as_table_mut()
                .ok_or_else(|| {
                    GitAiError::Generic(
                        "Codex config features.network_proxy.unix_sockets field must be a table"
                            .to_string(),
                    )
                })?
                .insert(
                    trace_socket_path.to_string_lossy().into_owned(),
                    TomlValue::String("allow".to_string()),
                );

            Ok(merged)
        }
    }

    fn config_without_trace_socket_allowance(config: &TomlValue) -> Result<TomlValue, GitAiError> {
        #[cfg(not(unix))]
        return Ok(config.clone());

        #[cfg(unix)]
        {
            let trace_socket_path = DaemonConfig::from_env_or_default_paths()?.trace_socket_path;
            let trace_socket = trace_socket_path.to_string_lossy();
            let mut merged = config.clone();
            let root = merged.as_table_mut().ok_or_else(|| {
                GitAiError::Generic("Codex config root must be a table".to_string())
            })?;
            let Some(features) = root.get_mut("features").and_then(TomlValue::as_table_mut) else {
                return Ok(merged);
            };
            let Some(network_proxy) = features
                .get_mut("network_proxy")
                .and_then(TomlValue::as_table_mut)
            else {
                return Ok(merged);
            };

            let remove_unix_sockets = network_proxy
                .get_mut("unix_sockets")
                .and_then(TomlValue::as_table_mut)
                .is_some_and(|unix_sockets| {
                    unix_sockets.remove(trace_socket.as_ref());
                    unix_sockets.is_empty()
                });
            if remove_unix_sockets {
                network_proxy.remove("unix_sockets");
            }
            if network_proxy.len() == 1
                && network_proxy.get("enabled").and_then(TomlValue::as_bool) == Some(true)
            {
                network_proxy.remove("enabled");
            }
            if network_proxy.is_empty() {
                features.remove("network_proxy");
            }
            if features.is_empty() {
                root.remove("features");
            }

            Ok(merged)
        }
    }

    fn config_with_installed_hooks(
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

    fn remove_notify_if_git_ai(config: &TomlValue) -> Result<Option<TomlValue>, GitAiError> {
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

    fn remove_inline_hooks_from_config(
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

    fn remove_feature_flags(config: &TomlValue) -> Result<TomlValue, GitAiError> {
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

    fn remove_codex_hooks_from_json(
        hooks_json: &JsonValue,
    ) -> Result<(JsonValue, bool), GitAiError> {
        let mut merged = hooks_json.clone();
        let root = merged.as_object_mut().ok_or_else(|| {
            GitAiError::Generic("Codex hooks.json root must be a JSON object".to_string())
        })?;
        let Some(hooks_entry) = root.get_mut("hooks") else {
            return Ok((merged, false));
        };
        if !hooks_entry.is_object() {
            return Ok((merged, false));
        }
        let hooks_obj = hooks_entry.as_object_mut().ok_or_else(|| {
            GitAiError::Generic("Codex hooks field must be a JSON object".to_string())
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

                if let Some(block_obj) = cleaned_block.as_object_mut() {
                    block_obj.insert("hooks".to_string(), JsonValue::Array(cleaned_hooks));
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
                hooks_obj.insert(event_name.to_string(), JsonValue::Array(cleaned_blocks));
            }
        }

        Ok((merged, changed))
    }

    fn hooks_json_with_installed_hooks(
        hooks_json: &JsonValue,
        binary_path: &Path,
    ) -> Result<JsonValue, GitAiError> {
        let (mut merged, _) = Self::remove_codex_hooks_from_json(hooks_json)?;
        let root = merged.as_object_mut().ok_or_else(|| {
            GitAiError::Generic("Codex hooks.json root must be a JSON object".to_string())
        })?;
        let hooks_entry = root.entry("hooks").or_insert_with(|| json!({}));
        if !hooks_entry.is_object() {
            *hooks_entry = json!({});
        }
        let hooks_obj = hooks_entry.as_object_mut().ok_or_else(|| {
            GitAiError::Generic("Codex hooks field must be a JSON object".to_string())
        })?;

        let desired_command = Self::desired_command(binary_path);
        for event_name in CODEX_HOOK_EVENTS {
            let mut blocks = hooks_obj
                .get(event_name)
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();

            let target_idx = blocks
                .iter()
                .position(|block| block.get("matcher").is_none())
                .unwrap_or_else(|| {
                    blocks.push(json!({ "hooks": [] }));
                    blocks.len() - 1
                });

            if !blocks[target_idx].is_object() {
                blocks[target_idx] = json!({ "hooks": [] });
            }
            let block_obj = blocks[target_idx].as_object_mut().ok_or_else(|| {
                GitAiError::Generic("Codex hooks.json hook block must be an object".to_string())
            })?;
            let hooks = block_obj.entry("hooks").or_insert_with(|| json!([]));
            if !hooks.is_array() {
                *hooks = json!([]);
            }
            let hooks_array = hooks.as_array_mut().ok_or_else(|| {
                GitAiError::Generic("Codex hooks.json hooks entry must be an array".to_string())
            })?;
            hooks_array.push(json!({
                "type": "command",
                "command": desired_command.clone(),
            }));

            hooks_obj.insert(event_name.to_string(), JsonValue::Array(blocks));
        }

        Ok(merged)
    }

    fn hooks_json_has_any_entries(hooks_json: &JsonValue) -> bool {
        hooks_json
            .get("hooks")
            .and_then(|value| value.as_object())
            .map(|hooks| {
                hooks.values().any(|value| {
                    value
                        .as_array()
                        .map(|blocks| !blocks.is_empty())
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }

    fn hooks_json_has_git_ai_entries(hooks_json: &JsonValue) -> bool {
        CODEX_HOOK_EVENTS.iter().any(|event_name| {
            hooks_json
                .get("hooks")
                .and_then(|hooks| hooks.get(*event_name))
                .and_then(|value| value.as_array())
                .map(|blocks| {
                    blocks.iter().any(|block| {
                        block
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

impl HookInstaller for CodexInstaller {
    fn name(&self) -> &str {
        "Codex"
    }

    fn id(&self) -> &str {
        "codex"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["codex"]
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let has_binary = binary_exists("codex");
        let has_dotfiles = codex_home_dir().exists();

        if !has_binary && !has_dotfiles {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let config_path = Self::config_path();
        let config = if config_path.exists() {
            Self::parse_config_toml(&fs::read_to_string(&config_path)?)?
        } else {
            TomlValue::Table(Map::new())
        };
        let hooks_json_path = Self::hooks_json_path();
        let hooks_json = if hooks_json_path.exists() {
            Self::parse_hooks_json(&fs::read_to_string(&hooks_json_path)?)?
        } else {
            json!({})
        };

        if Config::fresh().codex_hooks_format() == CodexHooksFormat::HooksJson {
            let config_without_notify =
                Self::remove_notify_if_git_ai(&config)?.unwrap_or(config.clone());
            let (config_without_inline_hooks, _) =
                Self::remove_inline_hooks_from_config(&config_without_notify)?;
            let desired_config = Self::config_with_trace_socket_allowed(
                &Self::config_with_hooks_feature_enabled(&config_without_inline_hooks)?,
            )?;
            let desired_hooks_json =
                Self::hooks_json_with_installed_hooks(&hooks_json, &params.binary_path)?;
            let has_json_hooks = Self::hooks_json_has_git_ai_entries(&hooks_json);
            let hooks_installed = Self::config_hooks_feature_enabled(&config) && has_json_hooks;
            let hooks_up_to_date = config == desired_config && hooks_json == desired_hooks_json;

            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed,
                hooks_up_to_date,
            });
        }

        let desired_config = Self::config_with_trace_socket_allowed(
            &Self::config_with_installed_hooks(&config, &params.binary_path)?,
        )?;
        let has_inline_hooks = Self::config_has_inline_hooks(&config);
        let has_legacy_hooks_json = Self::hooks_json_has_git_ai_entries(&hooks_json);
        let hooks_installed = Self::config_hooks_feature_enabled(&config)
            && (has_inline_hooks || has_legacy_hooks_json);
        let hooks_up_to_date = config == desired_config && !has_legacy_hooks_json;

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
        let config_path = Self::config_path();
        let hooks_json_path = Self::hooks_json_path();

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let existing_config_content = if config_path.exists() {
            fs::read_to_string(&config_path)?
        } else {
            String::new()
        };

        let existing_config = Self::parse_config_toml(&existing_config_content)?;

        let existing_hooks_content = if hooks_json_path.exists() {
            fs::read_to_string(&hooks_json_path)?
        } else {
            String::new()
        };
        let existing_hooks = Self::parse_hooks_json(&existing_hooks_content)?;

        if Config::fresh().codex_hooks_format() == CodexHooksFormat::HooksJson {
            let config_without_notify =
                Self::remove_notify_if_git_ai(&existing_config)?.unwrap_or(existing_config.clone());
            let (config_without_inline_hooks, _) =
                Self::remove_inline_hooks_from_config(&config_without_notify)?;
            let merged_config = Self::config_with_trace_socket_allowed(
                &Self::config_with_hooks_feature_enabled(&config_without_inline_hooks)?,
            )?;
            let merged_hooks =
                Self::hooks_json_with_installed_hooks(&existing_hooks, &params.binary_path)?;

            let config_changed = existing_config != merged_config;
            let hooks_json_changed = existing_hooks != merged_hooks;
            if !config_changed && !hooks_json_changed {
                return Ok(None);
            }

            let mut diff_output = Vec::new();

            if config_changed {
                let new_config_content = toml::to_string_pretty(&merged_config).map_err(|e| {
                    GitAiError::Generic(format!("Failed to serialize Codex config.toml: {e}"))
                })?;
                diff_output.push(generate_diff(
                    &config_path,
                    &existing_config_content,
                    &new_config_content,
                ));
                if !dry_run {
                    write_atomic(&config_path, new_config_content.as_bytes())?;
                }
            }

            if hooks_json_changed {
                let new_hooks_content = serde_json::to_string_pretty(&merged_hooks)?;
                diff_output.push(generate_diff(
                    &hooks_json_path,
                    &existing_hooks_content,
                    &new_hooks_content,
                ));
                if !dry_run {
                    write_atomic(&hooks_json_path, new_hooks_content.as_bytes())?;
                }
            }

            return Ok(Some(diff_output.join("\n")));
        }

        let merged_config = Self::config_with_trace_socket_allowed(
            &Self::config_with_installed_hooks(&existing_config, &params.binary_path)?,
        )?;

        // Check if legacy hooks.json needs migration
        let (hooks_json_changed, existing_hooks_content) = if hooks_json_path.exists() {
            let (_cleaned_hooks, changed) = Self::remove_codex_hooks_from_json(&existing_hooks)?;
            (changed, existing_hooks_content)
        } else {
            (false, String::new())
        };

        let config_changed = existing_config != merged_config;
        if !config_changed && !hooks_json_changed {
            return Ok(None);
        }

        let mut diff_output = Vec::new();

        // Write config.toml FIRST (contains the replacement inline hooks)
        if config_changed {
            let new_config_content = toml::to_string_pretty(&merged_config).map_err(|e| {
                GitAiError::Generic(format!("Failed to serialize Codex config.toml: {e}"))
            })?;
            diff_output.push(generate_diff(
                &config_path,
                &existing_config_content,
                &new_config_content,
            ));
            if !dry_run {
                write_atomic(&config_path, new_config_content.as_bytes())?;
            }
        }

        // THEN clean up legacy hooks.json (safe: config.toml already has the hooks)
        if hooks_json_changed {
            let existing_hooks = Self::parse_hooks_json(&existing_hooks_content)?;
            let (cleaned_hooks, _) = Self::remove_codex_hooks_from_json(&existing_hooks)?;
            if Self::hooks_json_has_any_entries(&cleaned_hooks) {
                let new_hooks_content = serde_json::to_string_pretty(&cleaned_hooks)?;
                diff_output.push(generate_diff(
                    &hooks_json_path,
                    &existing_hooks_content,
                    &new_hooks_content,
                ));
                if !dry_run {
                    write_atomic(&hooks_json_path, new_hooks_content.as_bytes())?;
                }
            } else {
                diff_output.push(generate_diff(&hooks_json_path, &existing_hooks_content, ""));
                if !dry_run {
                    fs::remove_file(&hooks_json_path)?;
                }
            }
        }

        Ok(Some(diff_output.join("\n")))
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let config_path = Self::config_path();
        let hooks_json_path = Self::hooks_json_path();
        if !config_path.exists() && !hooks_json_path.exists() {
            return Ok(None);
        }

        let existing_config_content = if config_path.exists() {
            fs::read_to_string(&config_path)?
        } else {
            String::new()
        };
        let existing_config = Self::parse_config_toml(&existing_config_content)?;

        // Remove inline hooks from config.toml
        let config_without_notify =
            Self::remove_notify_if_git_ai(&existing_config)?.unwrap_or(existing_config.clone());
        let (config_without_hooks, inline_hooks_changed) =
            Self::remove_inline_hooks_from_config(&config_without_notify)?;
        let config_without_trace_socket =
            Self::config_without_trace_socket_allowance(&config_without_hooks)?;
        let merged_config = Self::remove_feature_flags(&config_without_trace_socket)?;

        // Check if legacy hooks.json needs cleanup
        let (hooks_json_changed, existing_hooks_content) = if hooks_json_path.exists() {
            let content = fs::read_to_string(&hooks_json_path)?;
            let existing_hooks = Self::parse_hooks_json(&content)?;
            let (_cleaned_hooks, changed) = Self::remove_codex_hooks_from_json(&existing_hooks)?;
            (changed, content)
        } else {
            (false, String::new())
        };

        let config_changed = merged_config != existing_config;
        if !config_changed && !inline_hooks_changed && !hooks_json_changed {
            return Ok(None);
        }

        let mut diff_output = Vec::new();

        // Write config.toml changes first
        if config_changed || inline_hooks_changed {
            let new_config_content = toml::to_string_pretty(&merged_config).map_err(|e| {
                GitAiError::Generic(format!("Failed to serialize Codex config.toml: {e}"))
            })?;
            diff_output.push(generate_diff(
                &config_path,
                &existing_config_content,
                &new_config_content,
            ));
            if !dry_run {
                write_atomic(&config_path, new_config_content.as_bytes())?;
            }
        }

        // Then clean up legacy hooks.json
        if hooks_json_changed {
            let existing_hooks = Self::parse_hooks_json(&existing_hooks_content)?;
            let (cleaned_hooks, _) = Self::remove_codex_hooks_from_json(&existing_hooks)?;
            if Self::hooks_json_has_any_entries(&cleaned_hooks) {
                let new_hooks_content = serde_json::to_string_pretty(&cleaned_hooks)?;
                diff_output.push(generate_diff(
                    &hooks_json_path,
                    &existing_hooks_content,
                    &new_hooks_content,
                ));
                if !dry_run {
                    write_atomic(&hooks_json_path, new_hooks_content.as_bytes())?;
                }
            } else {
                diff_output.push(generate_diff(&hooks_json_path, &existing_hooks_content, ""));
                if !dry_run {
                    fs::remove_file(&hooks_json_path)?;
                }
            }
        }

        Ok(Some(diff_output.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdm::hook_installer::{HookInstaller, HookInstallerParams};
    use serial_test::serial;
    use std::path::Path;
    use tempfile::tempdir;

    fn test_binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    fn with_temp_home<F: FnOnce(&Path)>(f: F) {
        let temp = tempdir().unwrap();
        let home = temp.path().to_path_buf();

        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        let prev_codex_home = std::env::var_os("CODEX_HOME");

        // SAFETY: tests are serialized via #[serial], so mutating process env is safe.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("USERPROFILE", &home);
            std::env::remove_var("CODEX_HOME");
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
            match prev_codex_home {
                Some(v) => std::env::set_var("CODEX_HOME", v),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }
    }

    fn write_git_ai_config(home: &Path, codex_hooks_format: &str) {
        let git_ai_dir = home.join(".git-ai");
        fs::create_dir_all(&git_ai_dir).unwrap();
        fs::write(
            git_ai_dir.join("config.json"),
            serde_json::to_string_pretty(&json!({
                "codex_hooks_format": codex_hooks_format
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn test_is_git_ai_codex_notify_args_true_for_absolute_binary() {
        let args = vec![
            "/usr/local/bin/git-ai".to_string(),
            "checkpoint".to_string(),
            "codex".to_string(),
            "--hook-input".to_string(),
        ];

        assert!(CodexInstaller::is_git_ai_codex_notify_args(&args));
    }

    #[test]
    fn test_is_git_ai_codex_notify_args_true_for_legacy_via_codex_notify_args() {
        let args = vec![
            "/Users/svarlamov/.git-ai/bin/git-ai".to_string(),
            "checkpoint".to_string(),
            "codex".to_string(),
            "--via-codex-notify".to_string(),
            "--hook-input".to_string(),
            "stdin".to_string(),
        ];

        assert!(CodexInstaller::is_git_ai_codex_notify_args(&args));
    }

    #[test]
    fn test_is_git_ai_codex_notify_args_false_for_non_git_ai_command() {
        let args = vec![
            "notify-send".to_string(),
            "Codex".to_string(),
            "done".to_string(),
        ];

        assert!(!CodexInstaller::is_git_ai_codex_notify_args(&args));
    }

    #[test]
    fn test_remove_notify_if_git_ai_removes_only_git_ai_notify() {
        let config = CodexInstaller::parse_config_toml(
            r#"
model = "gpt-5"
notify = ["/usr/local/bin/git-ai", "checkpoint", "codex", "--hook-input"]
"#,
        )
        .unwrap();

        let merged = CodexInstaller::remove_notify_if_git_ai(&config)
            .unwrap()
            .expect("notify should be removed");
        assert!(merged.get("notify").is_none());
        assert_eq!(merged.get("model").and_then(|v| v.as_str()), Some("gpt-5"));
    }

    #[test]
    fn test_remove_notify_if_git_ai_removes_legacy_via_codex_notify_args() {
        let config = CodexInstaller::parse_config_toml(
            r#"
model = "gpt-5"
notify = ["/Users/svarlamov/.git-ai/bin/git-ai", "checkpoint", "codex", "--via-codex-notify", "--hook-input", "stdin"]
"#,
        )
        .unwrap();

        let merged = CodexInstaller::remove_notify_if_git_ai(&config)
            .unwrap()
            .expect("legacy git-ai notify should be removed");
        assert!(merged.get("notify").is_none());
        assert_eq!(merged.get("model").and_then(|v| v.as_str()), Some("gpt-5"));
    }

    #[test]
    fn test_remove_notify_if_git_ai_preserves_custom_notify() {
        let config = CodexInstaller::parse_config_toml(
            r#"
model = "gpt-5"
notify = ["notify-send", "Codex"]
"#,
        )
        .unwrap();

        let merged = CodexInstaller::remove_notify_if_git_ai(&config).unwrap();
        assert!(
            merged.is_none(),
            "Custom notify config should remain untouched"
        );
    }

    #[test]
    fn test_config_with_installed_hooks_sets_new_feature_flag_and_inline_hooks() {
        let existing = CodexInstaller::parse_config_toml(
            r#"
model = "gpt-5"
notify = ["/usr/local/bin/git-ai", "checkpoint", "codex", "--hook-input"]
"#,
        )
        .unwrap();

        let merged =
            CodexInstaller::config_with_installed_hooks(&existing, &test_binary_path()).unwrap();
        assert!(CodexInstaller::notify_args_from_config(&merged).is_none());
        assert_eq!(
            merged
                .get("features")
                .and_then(|value| value.get("hooks"))
                .and_then(|value| value.as_bool()),
            Some(true),
            "should use new 'hooks' feature flag"
        );
        assert!(
            merged
                .get("features")
                .and_then(|value| value.get("codex_hooks"))
                .is_none(),
            "legacy codex_hooks flag should be removed"
        );
        assert!(
            CodexInstaller::config_has_inline_hooks(&merged),
            "inline hooks should be present in config"
        );
        assert_eq!(
            merged.get("model").and_then(|value| value.as_str()),
            Some("gpt-5"),
            "other config should be preserved"
        );
    }

    #[test]
    fn test_config_with_installed_hooks_migrates_legacy_codex_hooks_flag() {
        let existing = CodexInstaller::parse_config_toml(
            r#"
model = "gpt-5"

[features]
codex_hooks = true
"#,
        )
        .unwrap();

        let merged =
            CodexInstaller::config_with_installed_hooks(&existing, &test_binary_path()).unwrap();
        assert_eq!(
            merged
                .get("features")
                .and_then(|value| value.get("hooks"))
                .and_then(|value| value.as_bool()),
            Some(true),
            "should use new 'hooks' feature flag"
        );
        assert!(
            merged
                .get("features")
                .and_then(|value| value.get("codex_hooks"))
                .is_none(),
            "legacy codex_hooks flag should be removed"
        );
    }

    #[test]
    fn test_config_with_installed_hooks_adds_inline_hooks_for_all_events() {
        let existing = CodexInstaller::parse_config_toml("model = \"gpt-5\"\n").unwrap();

        let merged =
            CodexInstaller::config_with_installed_hooks(&existing, &test_binary_path()).unwrap();

        let desired_cmd = CodexInstaller::desired_command(&test_binary_path());
        for event_name in CODEX_HOOK_EVENTS {
            let blocks = merged
                .get("hooks")
                .and_then(|h| h.get(event_name))
                .and_then(|v| v.as_array())
                .unwrap_or_else(|| panic!("missing hooks.{event_name}"));
            assert!(
                blocks.iter().any(|block| {
                    block.get("matcher").is_none()
                        && block
                            .get("hooks")
                            .and_then(|v| v.as_array())
                            .map(|hooks| {
                                hooks.iter().any(|hook| {
                                    hook.get("command").and_then(|v| v.as_str())
                                        == Some(desired_cmd.as_str())
                                })
                            })
                            .unwrap_or(false)
                }),
                "expected unscoped git-ai block for {event_name}"
            );
        }
    }

    #[test]
    fn test_config_with_installed_hooks_preserves_existing_matched_hooks() {
        let existing = CodexInstaller::parse_config_toml(
            r#"
model = "gpt-5"

[[hooks.PreToolUse]]
matcher = "Bash"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "echo keep-me"
"#,
        )
        .unwrap();

        let merged =
            CodexInstaller::config_with_installed_hooks(&existing, &test_binary_path()).unwrap();

        let pre_blocks = merged
            .get("hooks")
            .and_then(|h| h.get("PreToolUse"))
            .and_then(|v| v.as_array())
            .expect("PreToolUse blocks should exist");
        assert!(
            pre_blocks.iter().any(|block| {
                block.get("matcher").and_then(|v| v.as_str()) == Some("Bash")
                    && block
                        .get("hooks")
                        .and_then(|v| v.as_array())
                        .map(|hooks| {
                            hooks.iter().any(|hook| {
                                hook.get("command").and_then(|v| v.as_str()) == Some("echo keep-me")
                            })
                        })
                        .unwrap_or(false)
            }),
            "existing matched hooks should be preserved"
        );
    }

    #[test]
    fn test_config_hooks_feature_enabled_detects_new_flag() {
        let config = CodexInstaller::parse_config_toml(
            r#"
[features]
hooks = true
"#,
        )
        .unwrap();
        assert!(CodexInstaller::config_hooks_feature_enabled(&config));
    }

    #[test]
    fn test_config_hooks_feature_enabled_detects_legacy_flag() {
        let config = CodexInstaller::parse_config_toml(
            r#"
[features]
codex_hooks = true
"#,
        )
        .unwrap();
        assert!(CodexInstaller::config_hooks_feature_enabled(&config));
    }

    #[test]
    fn test_remove_inline_hooks_from_config() {
        let config = CodexInstaller::parse_config_toml(
            r#"
model = "gpt-5"

[features]
hooks = true

[[hooks.PreToolUse]]

[[hooks.PreToolUse.hooks]]
type = "command"
command = "/usr/local/bin/git-ai checkpoint codex --hook-input stdin"

[[hooks.PostToolUse]]

[[hooks.PostToolUse.hooks]]
type = "command"
command = "/usr/local/bin/git-ai checkpoint codex --hook-input stdin"

[[hooks.Stop]]

[[hooks.Stop.hooks]]
type = "command"
command = "/usr/local/bin/git-ai checkpoint codex --hook-input stdin"
"#,
        )
        .unwrap();

        let (merged, changed) = CodexInstaller::remove_inline_hooks_from_config(&config).unwrap();
        assert!(changed);
        assert!(
            merged.get("hooks").is_none(),
            "[hooks] table should be removed when empty"
        );
    }

    #[test]
    fn test_remove_inline_hooks_preserves_non_git_ai_hooks() {
        let config = CodexInstaller::parse_config_toml(
            r#"
model = "gpt-5"

[[hooks.PreToolUse]]

[[hooks.PreToolUse.hooks]]
type = "command"
command = "/usr/local/bin/git-ai checkpoint codex --hook-input stdin"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "echo keep-me"
"#,
        )
        .unwrap();

        let (merged, changed) = CodexInstaller::remove_inline_hooks_from_config(&config).unwrap();
        assert!(changed);
        let pre_blocks = merged
            .get("hooks")
            .and_then(|h| h.get("PreToolUse"))
            .and_then(|v| v.as_array())
            .expect("PreToolUse should still exist");
        assert_eq!(pre_blocks.len(), 1);
        let hooks_arr = pre_blocks[0]
            .get("hooks")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(hooks_arr.len(), 1);
        assert_eq!(
            hooks_arr[0].get("command").and_then(|v| v.as_str()),
            Some("echo keep-me")
        );
    }

    #[test]
    fn test_remove_feature_flags_removes_both_old_and_new() {
        let config = CodexInstaller::parse_config_toml(
            r#"
model = "gpt-5"

[features]
hooks = true
codex_hooks = true
"#,
        )
        .unwrap();

        let merged = CodexInstaller::remove_feature_flags(&config).unwrap();
        assert!(
            merged.get("features").is_none(),
            "features section should be removed when empty"
        );
    }

    #[test]
    fn test_remove_codex_hooks_from_json_removes_only_git_ai_entries() {
        let existing = json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "hooks": [
                            { "type": "command", "command": "/usr/local/bin/git-ai checkpoint codex --hook-input stdin" },
                            { "type": "command", "command": "echo keep" }
                        ]
                    }
                ],
                "Stop": [
                    {
                        "hooks": [
                            { "type": "command", "command": "/usr/local/bin/git-ai checkpoint codex --hook-input stdin" }
                        ]
                    }
                ]
            }
        });

        let (merged, changed) = CodexInstaller::remove_codex_hooks_from_json(&existing).unwrap();
        assert!(changed);
        assert_eq!(
            merged["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str(),
            Some("echo keep")
        );
        assert!(
            merged["hooks"].get("Stop").is_none(),
            "empty event arrays should be removed"
        );
    }

    #[test]
    #[serial]
    fn test_install_hooks_writes_inline_toml_and_check_reports_up_to_date() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            fs::write(&config_path, "model = \"gpt-5\"\n").unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let diff = installer
                .install_hooks(&params, false)
                .expect("install should succeed");
            assert!(diff.is_some(), "install should report a config diff");

            let content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&content).unwrap();
            assert_eq!(
                parsed
                    .get("features")
                    .and_then(|value| value.get("hooks"))
                    .and_then(|value| value.as_bool()),
                Some(true),
                "should set [features].hooks = true"
            );
            assert!(
                CodexInstaller::config_has_inline_hooks(&parsed),
                "inline hooks should be in config.toml"
            );

            let check = installer
                .check_hooks(&params)
                .expect("check should succeed");
            assert!(check.tool_installed);
            assert!(check.hooks_installed);
            assert!(check.hooks_up_to_date);
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_respects_codex_home() {
        with_temp_home(|home| {
            let default_codex_dir = home.join(".codex");
            let custom_codex_home = home.join("custom-codex-home");
            fs::create_dir_all(&custom_codex_home).unwrap();

            // SAFETY: tests are serialized via #[serial], so mutating process env is safe.
            unsafe {
                std::env::set_var("CODEX_HOME", &custom_codex_home);
            }

            let config_path = custom_codex_home.join("config.toml");
            fs::write(&config_path, "model = \"gpt-5\"\n").unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let diff = installer
                .install_hooks(&params, false)
                .expect("install should succeed");
            assert!(diff.is_some(), "install should report a config diff");
            assert!(
                !default_codex_dir.exists(),
                "default ~/.codex should not be touched when CODEX_HOME is set"
            );

            let content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&content).unwrap();
            assert!(
                CodexInstaller::config_has_inline_hooks(&parsed),
                "inline hooks should be in CODEX_HOME/config.toml"
            );

            let state = parsed
                .get("hooks")
                .and_then(|hooks| hooks.get("state"))
                .and_then(|state| state.as_table())
                .expect("hook trust state should be written");
            let config_path_str = config_path.to_string_lossy();
            assert!(
                state
                    .keys()
                    .all(|key| key.starts_with(config_path_str.as_ref())),
                "trust state keys should reference CODEX_HOME/config.toml"
            );

            let check = installer
                .check_hooks(&params)
                .expect("check should succeed");
            assert!(check.tool_installed);
            assert!(check.hooks_installed);
            assert!(check.hooks_up_to_date);
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_migrates_notify_and_sets_new_feature_flag() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            fs::write(
                &config_path,
                r#"
model = "gpt-5"
notify = ["/usr/local/bin/git-ai", "checkpoint", "codex", "--hook-input"]
"#,
            )
            .unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            installer
                .install_hooks(&params, false)
                .expect("install should succeed");

            let config_content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&config_content).unwrap();
            assert!(
                CodexInstaller::notify_args_from_config(&parsed).is_none(),
                "git-ai notify should be removed during migration"
            );
            assert_eq!(
                parsed
                    .get("features")
                    .and_then(|v| v.get("hooks"))
                    .and_then(|v| v.as_bool()),
                Some(true),
                "install should use new hooks feature flag"
            );
            assert!(
                CodexInstaller::config_has_inline_hooks(&parsed),
                "hooks should be inline in config.toml"
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_migrates_legacy_via_codex_notify() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            fs::write(
                &config_path,
                r#"
model = "gpt-5"
notify = ["/Users/svarlamov/.git-ai/bin/git-ai", "checkpoint", "codex", "--via-codex-notify", "--hook-input", "stdin"]
"#,
            )
            .unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            installer
                .install_hooks(&params, false)
                .expect("install should succeed");

            let config_content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&config_content).unwrap();
            assert!(
                CodexInstaller::notify_args_from_config(&parsed).is_none(),
                "legacy git-ai notify should be removed during migration"
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_preserves_custom_notify() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            fs::write(
                &config_path,
                r#"
model = "gpt-5"
notify = ["notify-send", "Codex finished"]
"#,
            )
            .unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            installer
                .install_hooks(&params, false)
                .expect("install should succeed");

            let config_content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&config_content).unwrap();
            assert_eq!(
                CodexInstaller::notify_args_from_config(&parsed),
                Some(vec![
                    "notify-send".to_string(),
                    "Codex finished".to_string(),
                ]),
                "non-git-ai notify must be preserved"
            );
            assert_eq!(
                parsed
                    .get("features")
                    .and_then(|v| v.get("hooks"))
                    .and_then(|v| v.as_bool()),
                Some(true),
                "install should still enable hooks feature flag"
            );
            assert!(
                CodexInstaller::config_has_inline_hooks(&parsed),
                "hooks should be inline in config.toml"
            );
        });
    }

    #[test]
    fn test_parse_config_toml_malformed() {
        let result = CodexInstaller::parse_config_toml("invalid [[ toml");
        assert!(result.is_err(), "Malformed TOML should return Err");
    }

    #[test]
    fn test_parse_config_toml_non_table_root() {
        let result = CodexInstaller::parse_config_toml("42");
        assert!(result.is_err(), "Non-table root value should return Err");
    }

    #[test]
    #[serial]
    fn test_install_hooks_dry_run() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            let original_content = "model = \"gpt-5\"\n";
            fs::write(&config_path, original_content).unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let diff = installer
                .install_hooks(&params, true)
                .expect("dry-run install should succeed");
            assert!(diff.is_some(), "dry-run should still produce a diff");

            let after = fs::read_to_string(&config_path).unwrap();
            assert_eq!(
                after, original_content,
                "File should remain unchanged after dry-run install"
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_idempotent() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            fs::write(&config_path, "model = \"gpt-5\"\n").unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let first = installer
                .install_hooks(&params, false)
                .expect("first install should succeed");
            assert!(first.is_some(), "first install should report changes");

            let second = installer
                .install_hooks(&params, false)
                .expect("second install should succeed");
            assert!(
                second.is_none(),
                "second install should return None (no changes needed)"
            );

            let content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&content).unwrap();
            assert!(CodexInstaller::config_has_inline_hooks(&parsed));
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_migrates_hooks_json_to_inline_toml() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            let hooks_json_path = codex_dir.join("hooks.json");
            fs::write(
                &config_path,
                r#"
model = "gpt-5"

[features]
codex_hooks = true
"#,
            )
            .unwrap();
            fs::write(
                &hooks_json_path,
                serde_json::to_string_pretty(&json!({
                    "hooks": {
                        "PreToolUse": [
                            {
                                "hooks": [
                                    { "type": "command", "command": "/old/git-ai checkpoint codex --hook-input stdin" },
                                ]
                            }
                        ],
                        "PostToolUse": [
                            {
                                "hooks": [
                                    { "type": "command", "command": "/old/git-ai checkpoint codex --hook-input stdin" }
                                ]
                            }
                        ],
                        "Stop": [
                            {
                                "hooks": [
                                    { "type": "command", "command": "/old/git-ai checkpoint codex --hook-input stdin" }
                                ]
                            }
                        ]
                    }
                }))
                .unwrap(),
            )
            .unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            installer
                .install_hooks(&params, false)
                .expect("install should succeed");

            let config_content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&config_content).unwrap();
            assert_eq!(
                parsed
                    .get("features")
                    .and_then(|v| v.get("hooks"))
                    .and_then(|v| v.as_bool()),
                Some(true),
                "should migrate to new hooks feature flag"
            );
            assert!(
                parsed
                    .get("features")
                    .and_then(|v| v.get("codex_hooks"))
                    .is_none(),
                "legacy codex_hooks flag should be removed"
            );
            assert!(
                CodexInstaller::config_has_inline_hooks(&parsed),
                "hooks should now be inline in config.toml"
            );
            assert!(
                !hooks_json_path.exists(),
                "hooks.json should be removed after migration (no other entries)"
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_migrates_hooks_json_preserves_non_git_ai_entries() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            let hooks_json_path = codex_dir.join("hooks.json");
            fs::write(&config_path, "model = \"gpt-5\"\n").unwrap();
            fs::write(
                &hooks_json_path,
                serde_json::to_string_pretty(&json!({
                    "hooks": {
                        "PreToolUse": [
                            {
                                "matcher": "Bash",
                                "hooks": [
                                    { "type": "command", "command": "/old/git-ai checkpoint codex --hook-input stdin" },
                                    { "type": "command", "command": "echo keep" }
                                ]
                            }
                        ]
                    }
                }))
                .unwrap(),
            )
            .unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            installer
                .install_hooks(&params, false)
                .expect("install should succeed");

            assert!(
                hooks_json_path.exists(),
                "hooks.json should be preserved when non-git-ai entries remain"
            );
            let hooks_content = fs::read_to_string(&hooks_json_path).unwrap();
            let hooks_json: serde_json::Value = serde_json::from_str(&hooks_content).unwrap();
            let pre_blocks = hooks_json["hooks"]["PreToolUse"].as_array().unwrap();
            assert!(
                pre_blocks.iter().any(|block| {
                    block["hooks"].as_array().is_some_and(|hooks| {
                        hooks
                            .iter()
                            .any(|hook| hook["command"].as_str() == Some("echo keep"))
                    })
                }),
                "non-git-ai hooks should be preserved in hooks.json"
            );
            assert!(
                !pre_blocks.iter().any(|block| {
                    block["hooks"].as_array().is_some_and(|hooks| {
                        hooks.iter().any(|hook| {
                            hook["command"]
                                .as_str()
                                .map(CodexInstaller::is_git_ai_codex_command)
                                .unwrap_or(false)
                        })
                    })
                }),
                "git-ai hooks should be removed from hooks.json"
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_prefers_hooks_json_when_configured() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            write_git_ai_config(home, "hooks_json");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            let hooks_json_path = codex_dir.join("hooks.json");
            fs::write(&config_path, "model = \"gpt-5\"\n").unwrap();
            fs::write(
                &hooks_json_path,
                serde_json::to_string_pretty(&json!({
                    "hooks": {
                        "PreToolUse": [
                            {
                                "matcher": "Bash",
                                "hooks": [
                                    { "type": "command", "command": "echo keep" }
                                ]
                            }
                        ]
                    }
                }))
                .unwrap(),
            )
            .unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            installer
                .install_hooks(&params, false)
                .expect("install should succeed");

            let config_content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&config_content).unwrap();
            assert_eq!(
                parsed
                    .get("features")
                    .and_then(|v| v.get("hooks"))
                    .and_then(|v| v.as_bool()),
                Some(true),
                "install should still enable Codex hooks"
            );
            assert!(
                !CodexInstaller::config_has_inline_hooks(&parsed),
                "configured hooks_json format should not install git-ai inline hooks"
            );

            let hooks_content = fs::read_to_string(&hooks_json_path).unwrap();
            let hooks_json: serde_json::Value = serde_json::from_str(&hooks_content).unwrap();
            assert!(
                CodexInstaller::hooks_json_has_git_ai_entries(&hooks_json),
                "git-ai hooks should be installed into hooks.json"
            );
            let pre_blocks = hooks_json["hooks"]["PreToolUse"].as_array().unwrap();
            assert!(
                pre_blocks.iter().any(|block| {
                    block["hooks"].as_array().is_some_and(|hooks| {
                        hooks
                            .iter()
                            .any(|hook| hook["command"].as_str() == Some("echo keep"))
                    })
                }),
                "existing hooks.json entries should be preserved"
            );

            let check = installer
                .check_hooks(&params)
                .expect("check should succeed");
            assert!(check.tool_installed);
            assert!(check.hooks_installed);
            assert!(check.hooks_up_to_date);
        });
    }

    #[test]
    #[serial]
    #[cfg(unix)]
    fn test_hooks_json_install_allows_trace_socket_and_is_idempotent() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            write_git_ai_config(home, "hooks_json");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            fs::write(&config_path, "model = \"gpt-5\"\n").unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };
            installer
                .install_hooks(&params, false)
                .expect("install should succeed");

            let installed_content = fs::read_to_string(&config_path).unwrap();
            let installed = CodexInstaller::parse_config_toml(&installed_content).unwrap();
            let network_proxy = installed
                .get("features")
                .and_then(|features| features.get("network_proxy"))
                .expect("network proxy config");
            let trace_socket = DaemonConfig::from_home(home)
                .trace_socket_path
                .to_string_lossy()
                .into_owned();
            assert_eq!(
                network_proxy.get("enabled").and_then(TomlValue::as_bool),
                Some(true)
            );
            assert_eq!(
                network_proxy
                    .get("unix_sockets")
                    .and_then(|sockets| sockets.get(&trace_socket))
                    .and_then(TomlValue::as_str),
                Some("allow")
            );

            let second_install = installer
                .install_hooks(&params, false)
                .expect("second install should succeed");
            assert!(second_install.is_none(), "second install should be a no-op");
            assert_eq!(fs::read_to_string(config_path).unwrap(), installed_content);

            installer
                .uninstall_hooks(&params, false)
                .expect("uninstall should succeed");
            let uninstalled = CodexInstaller::parse_config_toml(
                &fs::read_to_string(codex_dir.join("config.toml")).unwrap(),
            )
            .unwrap();
            assert!(
                uninstalled.get("features").is_none(),
                "uninstall should remove proxy config created solely for git-ai"
            );
        });
    }

    #[test]
    #[serial]
    #[cfg(unix)]
    fn test_uninstall_removes_trace_socket_and_preserves_network_proxy_settings() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            write_git_ai_config(home, "hooks_json");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            fs::write(
                &config_path,
                r#"[features.network_proxy]
enabled = true

[features.network_proxy.unix_sockets]
"/tmp/existing-agent.sock" = "allow"
"#,
            )
            .unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };
            installer
                .install_hooks(&params, false)
                .expect("install should succeed");
            installer
                .uninstall_hooks(&params, false)
                .expect("uninstall should succeed");

            let uninstalled_content = fs::read_to_string(&config_path).unwrap();
            let uninstalled = CodexInstaller::parse_config_toml(&uninstalled_content).unwrap();
            let network_proxy = uninstalled
                .get("features")
                .and_then(|features| features.get("network_proxy"))
                .expect("existing network proxy config should remain");
            let trace_socket = DaemonConfig::from_home(home)
                .trace_socket_path
                .to_string_lossy()
                .into_owned();

            assert_eq!(
                network_proxy.get("enabled").and_then(TomlValue::as_bool),
                Some(true),
                "uninstall must preserve the user's network proxy setting"
            );
            assert!(
                network_proxy
                    .get("unix_sockets")
                    .and_then(|sockets| sockets.get(&trace_socket))
                    .is_none(),
                "uninstall must remove the git-ai trace socket rule"
            );
            assert_eq!(
                network_proxy
                    .get("unix_sockets")
                    .and_then(|sockets| sockets.get("/tmp/existing-agent.sock"))
                    .and_then(TomlValue::as_str),
                Some("allow"),
                "uninstall must preserve unrelated socket rules"
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_prefers_hooks_json_removes_existing_inline_git_ai_hooks() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            write_git_ai_config(home, "hooks_json");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            let hooks_json_path = codex_dir.join("hooks.json");
            fs::write(
                &config_path,
                r#"
model = "gpt-5"

[features]
hooks = true

[[hooks.PreToolUse]]

[[hooks.PreToolUse.hooks]]
type = "command"
command = "/usr/local/bin/git-ai checkpoint codex --hook-input stdin"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "echo keep-inline"

[[hooks.PostToolUse]]

[[hooks.PostToolUse.hooks]]
type = "command"
command = "/usr/local/bin/git-ai checkpoint codex --hook-input stdin"

[[hooks.Stop]]

[[hooks.Stop.hooks]]
type = "command"
command = "/usr/local/bin/git-ai checkpoint codex --hook-input stdin"
"#,
            )
            .unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            installer
                .install_hooks(&params, false)
                .expect("install should succeed");

            let config_content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&config_content).unwrap();
            assert!(
                !CodexInstaller::config_has_inline_hooks(&parsed),
                "old git-ai inline hooks should be removed in hooks_json mode"
            );
            let pre_blocks = parsed
                .get("hooks")
                .and_then(|h| h.get("PreToolUse"))
                .and_then(|v| v.as_array())
                .expect("custom PreToolUse block should remain");
            assert!(
                pre_blocks.iter().any(|block| {
                    block
                        .get("hooks")
                        .and_then(|v| v.as_array())
                        .is_some_and(|hooks| {
                            hooks.iter().any(|hook| {
                                hook.get("command").and_then(|v| v.as_str())
                                    == Some("echo keep-inline")
                            })
                        })
                }),
                "non-git-ai inline hooks should be preserved"
            );

            let hooks_content = fs::read_to_string(&hooks_json_path).unwrap();
            let hooks_json: serde_json::Value = serde_json::from_str(&hooks_content).unwrap();
            assert!(
                CodexInstaller::hooks_json_has_git_ai_entries(&hooks_json),
                "git-ai hooks should be installed into hooks.json"
            );

            let check = installer
                .check_hooks(&params)
                .expect("check should succeed");
            assert!(check.hooks_installed);
            assert!(check.hooks_up_to_date);
        });
    }

    #[test]
    #[serial]
    fn test_uninstall_hooks_removes_inline_hooks_and_feature_flags() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            fs::write(
                &config_path,
                r#"
model = "gpt-5"

[features]
hooks = true

[[hooks.PreToolUse]]

[[hooks.PreToolUse.hooks]]
type = "command"
command = "/usr/local/bin/git-ai checkpoint codex --hook-input stdin"

[[hooks.PostToolUse]]

[[hooks.PostToolUse.hooks]]
type = "command"
command = "/usr/local/bin/git-ai checkpoint codex --hook-input stdin"

[[hooks.Stop]]

[[hooks.Stop.hooks]]
type = "command"
command = "/usr/local/bin/git-ai checkpoint codex --hook-input stdin"
"#,
            )
            .unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let diff = installer
                .uninstall_hooks(&params, false)
                .expect("uninstall should succeed");
            assert!(diff.is_some(), "uninstall should report a diff");

            let parsed =
                CodexInstaller::parse_config_toml(&fs::read_to_string(&config_path).unwrap())
                    .unwrap();
            assert!(
                parsed.get("features").is_none(),
                "features section should be removed"
            );
            assert!(
                parsed.get("hooks").is_none(),
                "hooks section should be removed"
            );
            assert_eq!(
                parsed.get("model").and_then(|v| v.as_str()),
                Some("gpt-5"),
                "other config should be preserved"
            );
        });
    }

    #[test]
    #[serial]
    fn test_uninstall_hooks_removes_legacy_hooks_json() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            let hooks_json_path = codex_dir.join("hooks.json");
            fs::write(
                &config_path,
                r#"
model = "gpt-5"
[features]
codex_hooks = true
"#,
            )
            .unwrap();
            fs::write(
                &hooks_json_path,
                serde_json::to_string_pretty(&json!({
                    "hooks": {
                        "PreToolUse": [{ "hooks": [{ "type": "command", "command": "/usr/local/bin/git-ai checkpoint codex --hook-input stdin" }] }],
                        "PostToolUse": [{ "hooks": [{ "type": "command", "command": "/usr/local/bin/git-ai checkpoint codex --hook-input stdin" }] }],
                        "Stop": [{ "hooks": [{ "type": "command", "command": "/usr/local/bin/git-ai checkpoint codex --hook-input stdin" }] }],
                    }
                }))
                .unwrap(),
            )
            .unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let diff = installer
                .uninstall_hooks(&params, false)
                .expect("uninstall should succeed");
            assert!(diff.is_some(), "uninstall should report a diff");

            let parsed =
                CodexInstaller::parse_config_toml(&fs::read_to_string(&config_path).unwrap())
                    .unwrap();
            assert!(
                parsed.get("features").is_none(),
                "feature flags should be removed"
            );
            assert!(
                !hooks_json_path.exists(),
                "hooks.json should be removed when only git-ai entries existed"
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_creates_missing_codex_dir() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            assert!(!codex_dir.exists());

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let result = installer.install_hooks(&params, false).unwrap();
            assert!(result.is_some(), "should report changes for fresh install");

            let config_path = codex_dir.join("config.toml");
            assert!(config_path.exists(), "config.toml should be created");

            let content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&content).unwrap();
            assert!(
                CodexInstaller::config_has_inline_hooks(&parsed),
                "config.toml should contain inline hooks"
            );
            assert_eq!(
                parsed
                    .get("features")
                    .and_then(|v| v.get("hooks"))
                    .and_then(|v| v.as_bool()),
                Some(true),
                "should set hooks feature flag"
            );
        });
    }

    #[test]
    #[serial]
    fn test_check_hooks_detects_legacy_hooks_json_installation() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            let hooks_json_path = codex_dir.join("hooks.json");
            fs::write(
                &config_path,
                r#"
model = "gpt-5"
[features]
codex_hooks = true
"#,
            )
            .unwrap();
            fs::write(
                &hooks_json_path,
                serde_json::to_string_pretty(&json!({
                    "hooks": {
                        "PreToolUse": [{ "hooks": [{ "type": "command", "command": "/usr/local/bin/git-ai checkpoint codex --hook-input stdin" }] }],
                        "PostToolUse": [{ "hooks": [{ "type": "command", "command": "/usr/local/bin/git-ai checkpoint codex --hook-input stdin" }] }],
                        "Stop": [{ "hooks": [{ "type": "command", "command": "/usr/local/bin/git-ai checkpoint codex --hook-input stdin" }] }],
                    }
                }))
                .unwrap(),
            )
            .unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            let check = installer
                .check_hooks(&params)
                .expect("check should succeed");
            assert!(check.tool_installed);
            assert!(
                check.hooks_installed,
                "should detect legacy hooks.json installation"
            );
            assert!(
                !check.hooks_up_to_date,
                "legacy format should not be considered up-to-date"
            );
        });
    }

    #[test]
    fn test_compute_trust_hash_deterministic() {
        let hash1 = CodexInstaller::compute_trust_hash(
            "pre_tool_use",
            "/usr/local/bin/git-ai checkpoint codex --hook-input stdin",
        )
        .unwrap();
        let hash2 = CodexInstaller::compute_trust_hash(
            "pre_tool_use",
            "/usr/local/bin/git-ai checkpoint codex --hook-input stdin",
        )
        .unwrap();
        assert_eq!(hash1, hash2);
        assert!(hash1.starts_with("sha256:"));
        assert_eq!(hash1.len(), 7 + 64); // "sha256:" + 64 hex chars
    }

    #[test]
    fn test_compute_trust_hash_differs_by_event() {
        let hash_pre = CodexInstaller::compute_trust_hash(
            "pre_tool_use",
            "/usr/local/bin/git-ai checkpoint codex --hook-input stdin",
        )
        .unwrap();
        let hash_post = CodexInstaller::compute_trust_hash(
            "post_tool_use",
            "/usr/local/bin/git-ai checkpoint codex --hook-input stdin",
        )
        .unwrap();
        let hash_stop = CodexInstaller::compute_trust_hash(
            "stop",
            "/usr/local/bin/git-ai checkpoint codex --hook-input stdin",
        )
        .unwrap();
        assert_ne!(hash_pre, hash_post);
        assert_ne!(hash_pre, hash_stop);
        assert_ne!(hash_post, hash_stop);
    }

    #[test]
    fn test_compute_trust_hash_differs_by_command() {
        let hash1 = CodexInstaller::compute_trust_hash(
            "pre_tool_use",
            "/usr/local/bin/git-ai checkpoint codex --hook-input stdin",
        )
        .unwrap();
        let hash2 = CodexInstaller::compute_trust_hash(
            "pre_tool_use",
            "/opt/bin/git-ai checkpoint codex --hook-input stdin",
        )
        .unwrap();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_canonical_json_sorts_keys() {
        let input: JsonValue = serde_json::json!({
            "z_key": 1,
            "a_key": 2,
            "m_key": {"b": 1, "a": 2}
        });
        let result = CodexInstaller::canonical_json(&input);
        let serialized = serde_json::to_string(&result).unwrap();
        assert_eq!(serialized, r#"{"a_key":2,"m_key":{"a":2,"b":1},"z_key":1}"#);
    }

    #[test]
    #[serial]
    fn test_install_hooks_writes_trust_state() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            fs::write(&config_path, "model = \"o3\"\n").unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            installer
                .install_hooks(&params, false)
                .expect("install should succeed");

            let content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&content).unwrap();

            let state = parsed
                .get("hooks")
                .and_then(|v| v.get("state"))
                .and_then(|v| v.as_table())
                .expect("hooks.state should exist");

            let config_path_str = config_path.to_string_lossy().to_string();
            for event in ["pre_tool_use", "post_tool_use", "stop"] {
                let key = format!("{config_path_str}:{event}:0:0");
                let entry = state
                    .get(&key)
                    .and_then(|v| v.as_table())
                    .unwrap_or_else(|| panic!("state entry for {event} should exist"));
                assert_eq!(entry.get("enabled").and_then(|v| v.as_bool()), Some(true));
                let hash = entry
                    .get("trusted_hash")
                    .and_then(|v| v.as_str())
                    .expect("trusted_hash should exist");
                assert!(
                    hash.starts_with("sha256:"),
                    "hash should have sha256 prefix"
                );
                assert_eq!(hash.len(), 71, "hash should be sha256: + 64 hex chars");
            }
        });
    }

    #[test]
    #[serial]
    fn test_uninstall_hooks_removes_trust_state() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            fs::write(&config_path, "model = \"o3\"\n").unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            installer
                .install_hooks(&params, false)
                .expect("install should succeed");

            // Verify state exists before uninstall
            let content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&content).unwrap();
            assert!(
                parsed
                    .get("hooks")
                    .and_then(|v| v.get("state"))
                    .and_then(|v| v.as_table())
                    .is_some(),
                "state should exist after install"
            );

            installer
                .uninstall_hooks(&params, false)
                .expect("uninstall should succeed");

            let content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&content).unwrap();
            assert!(
                parsed.get("hooks").is_none(),
                "hooks table should be removed after uninstall"
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_preserves_existing_state_entries() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            fs::write(
                &config_path,
                r#"
model = "o3"

[hooks.state."/some/other/hooks.json:pre_tool_use:0:0"]
enabled = true
trusted_hash = "sha256:abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
"#,
            )
            .unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            installer
                .install_hooks(&params, false)
                .expect("install should succeed");

            let content = fs::read_to_string(&config_path).unwrap();
            let parsed = CodexInstaller::parse_config_toml(&content).unwrap();
            let state = parsed
                .get("hooks")
                .and_then(|v| v.get("state"))
                .and_then(|v| v.as_table())
                .expect("hooks.state should exist");

            // Existing state entry should be preserved
            assert!(
                state.contains_key("/some/other/hooks.json:pre_tool_use:0:0"),
                "non-git-ai state entry should be preserved"
            );

            // Our state entries should also be present
            let config_path_str = config_path.to_string_lossy().to_string();
            assert!(
                state.contains_key(&format!("{config_path_str}:pre_tool_use:0:0")),
                "git-ai state entry should be added"
            );
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_trust_state_idempotent() {
        with_temp_home(|home| {
            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            let config_path = codex_dir.join("config.toml");
            fs::write(&config_path, "model = \"o3\"\n").unwrap();

            let installer = CodexInstaller;
            let params = HookInstallerParams {
                binary_path: test_binary_path(),
            };

            installer
                .install_hooks(&params, false)
                .expect("first install should succeed");
            let content_after_first = fs::read_to_string(&config_path).unwrap();

            // Second install should be a no-op (returns None)
            let diff = installer
                .install_hooks(&params, false)
                .expect("second install should succeed");
            assert!(diff.is_none(), "second install should be a no-op");

            let content_after_second = fs::read_to_string(&config_path).unwrap();
            assert_eq!(content_after_first, content_after_second);
        });
    }
}
