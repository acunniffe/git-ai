use super::parse::{mask_api_key, parse_key_path};
use super::spec::resolve_key;
use serde_json::Value;

pub(super) fn show_all_config() -> Result<(), String> {
    let file_config = crate::config::load_file_config_public()?;

    // Build a complete effective config representation
    let mut effective_config = serde_json::Map::new();

    // Get the actual runtime config
    let runtime_config = crate::config::Config::get();

    // Add fields with their effective values
    effective_config.insert(
        "git_path".to_string(),
        Value::String(runtime_config.git_cmd().to_string()),
    );

    // Arrays
    if let Some(ref repos) = file_config.exclude_prompts_in_repositories {
        effective_config.insert(
            "exclude_prompts_in_repositories".to_string(),
            serde_json::to_value(repos).unwrap(),
        );
    } else {
        effective_config.insert(
            "exclude_prompts_in_repositories".to_string(),
            Value::Array(vec![]),
        );
    }

    if let Some(ref repos) = file_config.allowed_repositories {
        effective_config.insert(
            "allowed_repositories".to_string(),
            serde_json::to_value(repos).unwrap(),
        );
    } else {
        effective_config.insert("allowed_repositories".to_string(), Value::Array(vec![]));
    }

    if let Some(ref repos) = file_config.exclude_repositories {
        effective_config.insert(
            "exclude_repositories".to_string(),
            serde_json::to_value(repos).unwrap(),
        );
    } else {
        effective_config.insert("exclude_repositories".to_string(), Value::Array(vec![]));
    }

    // Booleans with runtime values
    effective_config.insert(
        "telemetry".to_string(),
        Value::String(if runtime_config.telemetry_enabled() {
            "on".to_string()
        } else {
            "off".to_string()
        }),
    );
    effective_config.insert(
        "telemetry_oss_disabled".to_string(),
        Value::Bool(runtime_config.is_telemetry_oss_disabled()),
    );
    effective_config.insert(
        "disable_version_checks".to_string(),
        Value::Bool(runtime_config.version_checks_disabled()),
    );
    effective_config.insert(
        "disable_auto_updates".to_string(),
        Value::Bool(runtime_config.auto_updates_disabled()),
    );

    // Optional strings
    if let Some(ref dsn) = file_config.telemetry_enterprise_dsn {
        effective_config.insert(
            "telemetry_enterprise_dsn".to_string(),
            Value::String(dsn.clone()),
        );
    }

    effective_config.insert(
        "update_channel".to_string(),
        Value::String(runtime_config.update_channel().as_str().to_string()),
    );

    effective_config.insert(
        "prompt_storage".to_string(),
        Value::String(runtime_config.prompt_storage().to_string()),
    );

    // include_prompts_in_repositories
    if let Some(ref repos) = file_config.include_prompts_in_repositories {
        effective_config.insert(
            "include_prompts_in_repositories".to_string(),
            serde_json::to_value(repos).unwrap_or(Value::Array(vec![])),
        );
    }

    // default_prompt_storage
    if let Some(ref storage) = file_config.default_prompt_storage {
        effective_config.insert(
            "default_prompt_storage".to_string(),
            Value::String(storage.clone()),
        );
    }

    effective_config.insert("quiet".to_string(), Value::Bool(runtime_config.is_quiet()));

    effective_config.insert(
        "author".to_string(),
        serde_json::to_value(runtime_config.author())
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
    );

    effective_config.insert(
        "git_ai_hooks".to_string(),
        serde_json::to_value(runtime_config.git_ai_hooks())
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
    );

    effective_config.insert(
        "codex_hooks_format".to_string(),
        Value::String(runtime_config.codex_hooks_format().as_str().to_string()),
    );

    effective_config.insert(
        "allow_superuser".to_string(),
        Value::Bool(runtime_config.allow_superuser()),
    );

    // transcript_streaming_lookback_days: runtime normalizes 0 -> None (unlimited).
    // Surface unlimited as 0 so it round-trips through `config set`.
    effective_config.insert(
        "transcript_streaming_lookback_days".to_string(),
        Value::Number(
            runtime_config
                .transcript_streaming_lookback_days()
                .unwrap_or(0)
                .into(),
        ),
    );

    effective_config.insert(
        "max_checkpoint_file_size_bytes".to_string(),
        Value::Number(runtime_config.max_checkpoint_file_size_bytes().into()),
    );
    effective_config.insert(
        "max_checkpoint_total_size_bytes".to_string(),
        Value::Number(runtime_config.max_checkpoint_total_size_bytes().into()),
    );
    effective_config.insert(
        "max_checkpoint_total_lines".to_string(),
        Value::Number(runtime_config.max_checkpoint_total_lines().into()),
    );

    effective_config.insert(
        "custom_attributes".to_string(),
        serde_json::to_value(runtime_config.custom_attributes())
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
    );

    // Feature flags - show effective flags with defaults applied
    let flags_value = serde_json::to_value(runtime_config.get_feature_flags())
        .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
    effective_config.insert("feature_flags".to_string(), flags_value);

    // API base URL
    effective_config.insert(
        "api_base_url".to_string(),
        Value::String(runtime_config.api_base_url().to_string()),
    );

    // API key - show masked value if set
    if let Some(ref key) = file_config.api_key {
        let masked = mask_api_key(key);
        effective_config.insert("api_key".to_string(), Value::String(masked));
    }

    // notes_backend
    {
        let nb = runtime_config.notes_backend();
        let mut nb_map = serde_json::Map::new();
        nb_map.insert(
            "kind".to_string(),
            Value::String(nb.kind.as_str().to_string()),
        );
        if let Some(ref url) = nb.backend_url {
            nb_map.insert("backend_url".to_string(), Value::String(url.clone()));
        }
        effective_config.insert("notes_backend".to_string(), Value::Object(nb_map));
    }

    let json = serde_json::to_string_pretty(&effective_config)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;

    println!("{}", json);
    Ok(())
}

pub(super) fn get_config_value(key: &str) -> Result<(), String> {
    let file_config = crate::config::load_file_config_public()?;
    let runtime_config = crate::config::Config::get();
    let resolved = resolve_key(key)?;

    let key_path = parse_key_path(key);

    // Handle top-level keys
    if key_path.len() == 1 {
        let value = match resolved.spec.name {
            "git_path" => Value::String(runtime_config.git_cmd().to_string()),
            "exclude_prompts_in_repositories" => {
                if let Some(ref repos) = file_config.exclude_prompts_in_repositories {
                    serde_json::to_value(repos).unwrap()
                } else {
                    Value::Array(vec![])
                }
            }
            "allowed_repositories" | "allow_repositories" => {
                if let Some(ref repos) = file_config.allowed_repositories {
                    serde_json::to_value(repos).unwrap()
                } else {
                    Value::Array(vec![])
                }
            }
            "exclude_repositories" => {
                if let Some(ref repos) = file_config.exclude_repositories {
                    serde_json::to_value(repos).unwrap()
                } else {
                    Value::Array(vec![])
                }
            }
            "telemetry" => Value::String(if runtime_config.telemetry_enabled() {
                "on".to_string()
            } else {
                "off".to_string()
            }),
            "telemetry_oss" => Value::Bool(runtime_config.is_telemetry_oss_disabled()),
            "telemetry_enterprise_dsn" => {
                if let Some(ref dsn) = file_config.telemetry_enterprise_dsn {
                    Value::String(dsn.clone())
                } else {
                    Value::Null
                }
            }
            "disable_version_checks" => Value::Bool(runtime_config.version_checks_disabled()),
            "disable_auto_updates" => Value::Bool(runtime_config.auto_updates_disabled()),
            "update_channel" => Value::String(runtime_config.update_channel().as_str().to_string()),
            "feature_flags" => {
                // Show effective flags with defaults applied
                serde_json::to_value(runtime_config.get_feature_flags())
                    .unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
            }
            "api_base_url" => Value::String(runtime_config.api_base_url().to_string()),
            "api_key" => {
                if let Some(ref key) = file_config.api_key {
                    Value::String(mask_api_key(key))
                } else {
                    Value::Null
                }
            }
            "prompt_storage" => Value::String(runtime_config.prompt_storage().to_string()),
            "include_prompts_in_repositories" => {
                if let Some(ref repos) = file_config.include_prompts_in_repositories {
                    serde_json::to_value(repos).unwrap()
                } else {
                    Value::Array(vec![])
                }
            }
            "default_prompt_storage" => {
                if let Some(ref storage) = file_config.default_prompt_storage {
                    Value::String(storage.clone())
                } else {
                    Value::Null
                }
            }
            "quiet" => Value::Bool(runtime_config.is_quiet()),
            "author" => serde_json::to_value(runtime_config.author())
                .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
            "git_ai_hooks" => serde_json::to_value(runtime_config.git_ai_hooks())
                .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
            "codex_hooks_format" => {
                Value::String(runtime_config.codex_hooks_format().as_str().to_string())
            }
            "allow_superuser" => Value::Bool(runtime_config.allow_superuser()),
            "transcript_streaming_lookback_days" => Value::Number(
                runtime_config
                    .transcript_streaming_lookback_days()
                    .unwrap_or(0)
                    .into(),
            ),
            "max_checkpoint_file_size_bytes" => {
                Value::Number(runtime_config.max_checkpoint_file_size_bytes().into())
            }
            "max_checkpoint_total_size_bytes" => {
                Value::Number(runtime_config.max_checkpoint_total_size_bytes().into())
            }
            "max_checkpoint_total_lines" => {
                Value::Number(runtime_config.max_checkpoint_total_lines().into())
            }
            "custom_attributes" => serde_json::to_value(runtime_config.custom_attributes())
                .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
            "notes_backend" => {
                let nb = runtime_config.notes_backend();
                let mut map = serde_json::Map::new();
                map.insert(
                    "kind".to_string(),
                    Value::String(nb.kind.as_str().to_string()),
                );
                if let Some(ref url) = nb.backend_url {
                    map.insert("backend_url".to_string(), Value::String(url.clone()));
                }
                Value::Object(map)
            }
            _ => return Err(format!("Unknown config key: {}", key)),
        };

        let json = serde_json::to_string_pretty(&value)
            .map_err(|e| format!("Failed to serialize value: {}", e))?;
        println!("{}", json);
        return Ok(());
    }

    // Handle nested keys (dot notation)
    if resolved.root() == "feature_flags" || resolved.root() == "git_ai_hooks" {
        let root = if resolved.root() == "feature_flags" {
            serde_json::to_value(runtime_config.get_feature_flags())
                .unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
        } else {
            serde_json::to_value(runtime_config.git_ai_hooks())
                .unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
        };

        let mut current = &root;
        for segment in &key_path[1..] {
            current = current
                .get(segment)
                .ok_or_else(|| format!("Config key not found: {}", key))?;
        }

        let json = serde_json::to_string_pretty(current)
            .map_err(|e| format!("Failed to serialize value: {}", e))?;
        println!("{}", json);
        return Ok(());
    }

    if resolved.root() == "notes_backend" {
        if key_path.len() != 2 {
            return Err(
                "notes_backend requires a field name (notes_backend.kind or notes_backend.backend_url)"
                    .to_string(),
            );
        }
        let nb = runtime_config.notes_backend();
        let value = match key_path[1].as_str() {
            "kind" => Value::String(nb.kind.as_str().to_string()),
            "backend_url" => nb
                .backend_url
                .as_ref()
                .map(|u| Value::String(u.clone()))
                .unwrap_or(Value::Null),
            other => return Err(format!("Unknown notes_backend field: {}", other)),
        };
        let json = serde_json::to_string_pretty(&value)
            .map_err(|e| format!("Failed to serialize value: {}", e))?;
        println!("{}", json);
        return Ok(());
    }

    if resolved.root() == "author" {
        if key_path.len() != 2 {
            return Err("author requires a field name (author.name or author.email)".to_string());
        }
        let author = runtime_config.author();
        let value = match key_path[1].as_str() {
            "name" => author
                .name
                .as_ref()
                .map(|name| Value::String(name.clone()))
                .unwrap_or(Value::Null),
            "email" => author
                .email
                .as_ref()
                .map(|email| Value::String(email.clone()))
                .unwrap_or(Value::Null),
            other => return Err(format!("Unknown author field: {}", other)),
        };
        let json = serde_json::to_string_pretty(&value)
            .map_err(|e| format!("Failed to serialize value: {}", e))?;
        println!("{}", json);
        return Ok(());
    }

    if resolved.root() == "custom_attributes" {
        if key_path.len() != 2 {
            return Err(
                "custom_attributes requires an attribute name (e.g., custom_attributes.team)"
                    .to_string(),
            );
        }
        let attr_key = key_path[1].trim();
        let value = runtime_config
            .custom_attributes()
            .get(attr_key)
            .map(|v| Value::String(v.clone()))
            .unwrap_or(Value::Null);
        let json = serde_json::to_string_pretty(&value)
            .map_err(|e| format!("Failed to serialize value: {}", e))?;
        println!("{}", json);
        return Ok(());
    }

    Err(
        "Nested keys are only supported for feature_flags, git_ai_hooks, notes_backend, author, and custom_attributes"
            .to_string(),
    )
}
