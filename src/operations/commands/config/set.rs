use super::parse::{
    mask_api_key, parse_author_config_object, parse_bool, parse_codex_hooks_format,
    parse_custom_attributes_object, parse_git_ai_hooks_object, parse_hook_command_values,
    parse_key_path, parse_notes_backend_kind, parse_value, validate_prompt_storage_value,
};
use super::pattern::{log_array_changes, resolve_repository_value, set_repository_array_field};
use super::spec::resolve_key;
use serde_json::Value;

pub(super) fn set_config_value(key: &str, value: &str, add_mode: bool) -> Result<(), String> {
    let mut file_config = crate::config::load_file_config_public()?;
    let resolved = resolve_key(key)?;
    let key_path = parse_key_path(key);

    // Handle top-level keys
    if key_path.len() == 1 {
        match resolved.spec.name {
            "git_path" => {
                file_config.git_path = Some(value.to_string());
                crate::config::save_file_config(&file_config)?;
                println!("[git_path]: {}", value);
            }
            "exclude_prompts_in_repositories" => {
                let added = set_repository_array_field(
                    &mut file_config.exclude_prompts_in_repositories,
                    value,
                    add_mode,
                )?;
                crate::config::save_file_config(&file_config)?;
                log_array_changes(&added, add_mode);
            }
            "allowed_repositories" | "allow_repositories" => {
                let added = set_repository_array_field(
                    &mut file_config.allowed_repositories,
                    value,
                    add_mode,
                )?;
                crate::config::save_file_config(&file_config)?;
                log_array_changes(&added, add_mode);
            }
            "exclude_repositories" => {
                let added = set_repository_array_field(
                    &mut file_config.exclude_repositories,
                    value,
                    add_mode,
                )?;
                crate::config::save_file_config(&file_config)?;
                log_array_changes(&added, add_mode);
            }
            "telemetry" => {
                if !matches!(value.trim(), "on" | "off") {
                    return Err(format!(
                        "Invalid telemetry value '{}': expected 'on' or 'off'",
                        value
                    ));
                }
                file_config.telemetry = Some(value.trim().to_string());
                crate::config::save_file_config(&file_config)?;
                println!("[telemetry]: {}", value.trim());
            }
            "telemetry_oss" => {
                file_config.telemetry_oss = Some(value.to_string());
                crate::config::save_file_config(&file_config)?;
                println!("[telemetry_oss]: {}", value);
            }
            "telemetry_enterprise_dsn" => {
                file_config.telemetry_enterprise_dsn = Some(value.to_string());
                crate::config::save_file_config(&file_config)?;
                println!("[telemetry_enterprise_dsn]: {}", value);
            }
            "disable_version_checks" => {
                let bool_value = parse_bool(value)?;
                file_config.disable_version_checks = Some(bool_value);
                crate::config::save_file_config(&file_config)?;
                println!("[disable_version_checks]: {}", bool_value);
            }
            "disable_auto_updates" => {
                let bool_value = parse_bool(value)?;
                file_config.disable_auto_updates = Some(bool_value);
                crate::config::save_file_config(&file_config)?;
                println!("[disable_auto_updates]: {}", bool_value);
            }
            "update_channel" => {
                // Validate update channel
                if value != "latest" && value != "next" {
                    return Err(
                        "Invalid update_channel value. Expected 'latest' or 'next'".to_string()
                    );
                }
                file_config.update_channel = Some(value.to_string());
                crate::config::save_file_config(&file_config)?;
                println!("[update_channel]: {}", value);
            }
            "feature_flags" => {
                if add_mode {
                    return Err("Cannot use --add with feature_flags at top level. Use dot notation: feature_flags.key".to_string());
                }
                // Parse as JSON object
                let json_value: Value = serde_json::from_str(value)
                    .map_err(|e| format!("Invalid JSON for feature_flags: {}", e))?;
                if !json_value.is_object() {
                    return Err("feature_flags must be a JSON object".to_string());
                }
                file_config.feature_flags = Some(json_value);
                crate::config::save_file_config(&file_config)?;
                println!("[feature_flags]: {}", value);
            }
            "api_base_url" => {
                file_config.api_base_url = Some(value.to_string());
                crate::config::save_file_config(&file_config)?;
                println!("[api_base_url]: {}", value);
            }
            "api_key" => {
                file_config.api_key = Some(value.to_string());
                crate::config::save_file_config(&file_config)?;
                let masked = mask_api_key(value);
                println!("[api_key]: {}", masked);
            }
            "prompt_storage" => {
                validate_prompt_storage_value(value)?;
                file_config.prompt_storage = Some(value.to_string());
                crate::config::save_file_config(&file_config)?;
                println!("[prompt_storage]: {}", value);
            }
            "include_prompts_in_repositories" => {
                let resolved = resolve_repository_value(value)?;
                if add_mode {
                    let mut list = file_config
                        .include_prompts_in_repositories
                        .unwrap_or_default();
                    for pattern in &resolved {
                        if !list.contains(pattern) {
                            list.push(pattern.clone());
                        }
                    }
                    file_config.include_prompts_in_repositories = Some(list);
                } else {
                    file_config.include_prompts_in_repositories = Some(resolved.clone());
                }
                crate::config::save_file_config(&file_config)?;
                for pattern in resolved {
                    println!("[include_prompts_in_repositories]: {}", pattern);
                }
            }
            "default_prompt_storage" => {
                validate_prompt_storage_value(value)?;
                file_config.default_prompt_storage = Some(value.to_string());
                crate::config::save_file_config(&file_config)?;
                println!("[default_prompt_storage]: {}", value);
            }
            "quiet" => {
                let bool_value = parse_bool(value)?;
                file_config.quiet = Some(bool_value);
                crate::config::save_file_config(&file_config)?;
                println!("[quiet]: {}", bool_value);
            }
            "author" => {
                if add_mode {
                    return Err(
                        "Cannot use --add with author. Use author.name or author.email."
                            .to_string(),
                    );
                }
                let author = parse_author_config_object(value)?;
                file_config.author = if author.is_empty() {
                    None
                } else {
                    Some(author.clone())
                };
                crate::config::save_file_config(&file_config)?;
                println!(
                    "[author]: {}",
                    serde_json::to_string(&author)
                        .map_err(|e| format!("Failed to serialize author: {}", e))?
                );
            }
            "git_ai_hooks" => {
                if add_mode {
                    return Err("Cannot use --add with git_ai_hooks at top level. Use dot notation: git_ai_hooks.post_notes_updated".to_string());
                }
                file_config.git_ai_hooks = Some(parse_git_ai_hooks_object(value)?);
                crate::config::save_file_config(&file_config)?;
                println!("[git_ai_hooks]: {}", value);
            }
            "codex_hooks_format" => {
                let format = parse_codex_hooks_format(value)?;
                file_config.codex_hooks_format = Some(format.as_str().to_string());
                crate::config::save_file_config(&file_config)?;
                println!("[codex_hooks_format]: {}", format.as_str());
            }
            "allow_superuser" => {
                let bool_value = parse_bool(value)?;
                file_config.allow_superuser = Some(bool_value);
                crate::config::save_file_config(&file_config)?;
                println!("[allow_superuser]: {}", bool_value);
            }
            "transcript_streaming_lookback_days" => {
                let days = value.trim().parse::<u32>().map_err(|_| {
                    format!(
                        "Invalid transcript_streaming_lookback_days value '{}'. Expected a non-negative integer (0 = unlimited)",
                        value
                    )
                })?;
                file_config.transcript_streaming_lookback_days = Some(days);
                crate::config::save_file_config(&file_config)?;
                println!("[transcript_streaming_lookback_days]: {}", days);
            }
            "max_checkpoint_file_size_bytes" => {
                let bytes = value.trim().parse::<usize>().map_err(|_| {
                    format!(
                        "Invalid max_checkpoint_file_size_bytes value '{}'. Expected a non-negative integer in bytes",
                        value
                    )
                })?;
                file_config.max_checkpoint_file_size_bytes = Some(bytes);
                crate::config::save_file_config(&file_config)?;
                println!("[max_checkpoint_file_size_bytes]: {}", bytes);
            }
            "max_checkpoint_total_size_bytes" => {
                let bytes = value.trim().parse::<usize>().map_err(|_| {
                    format!(
                        "Invalid max_checkpoint_total_size_bytes value '{}'. Expected a non-negative integer in bytes",
                        value
                    )
                })?;
                file_config.max_checkpoint_total_size_bytes = Some(bytes);
                crate::config::save_file_config(&file_config)?;
                println!("[max_checkpoint_total_size_bytes]: {}", bytes);
            }
            "max_checkpoint_total_lines" => {
                let lines = value.trim().parse::<usize>().map_err(|_| {
                    format!(
                        "Invalid max_checkpoint_total_lines value '{}'. Expected a non-negative integer in lines",
                        value
                    )
                })?;
                file_config.max_checkpoint_total_lines = Some(lines);
                crate::config::save_file_config(&file_config)?;
                println!("[max_checkpoint_total_lines]: {}", lines);
            }
            "daemon_memory_limit_mb" => {
                let limit_mb = value.trim().parse::<u64>().map_err(|_| {
                    format!(
                        "Invalid daemon_memory_limit_mb value '{}'. Expected a positive integer in MiB",
                        value
                    )
                })?;
                if limit_mb == 0 || limit_mb > crate::config::MAX_DAEMON_MEMORY_LIMIT_MB {
                    return Err(format!(
                        "Invalid daemon_memory_limit_mb value '{}'. Expected an integer between 1 and {} MiB",
                        value,
                        crate::config::MAX_DAEMON_MEMORY_LIMIT_MB
                    ));
                }
                file_config.daemon_memory_limit_mb = Some(limit_mb);
                crate::config::save_file_config(&file_config)?;
                println!("[daemon_memory_limit_mb]: {}", limit_mb);
            }
            "custom_attributes" => {
                if add_mode {
                    return Err("Cannot use --add with custom_attributes at top level. Use dot notation: custom_attributes.key".to_string());
                }
                let attrs = parse_custom_attributes_object(value)?;
                // Mirror the `author`/`git_ai_hooks` convention: an empty object
                // is stored as None so the key is omitted from the config file
                // rather than persisted as a redundant `{}`.
                file_config.custom_attributes = if attrs.is_empty() { None } else { Some(attrs) };
                crate::config::save_file_config(&file_config)?;
                println!("[custom_attributes]: {}", value);
            }
            _ => return Err(format!("Unknown config key: {}", key)),
        }

        return Ok(());
    }

    // Handle nested keys (dot notation) - only for feature_flags
    if resolved.root() == "feature_flags" {
        if key_path.len() < 2 {
            return Err(
                "feature_flags requires a nested key (e.g., feature_flags.some_flag)".to_string(),
            );
        }

        // Get or create feature_flags object
        let mut flags = file_config
            .feature_flags
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        if !flags.is_object() {
            return Err("feature_flags must be a JSON object".to_string());
        }

        // Navigate to the nested location
        let flags_obj = flags.as_object_mut().unwrap();

        let nested_key = key_path[1..].join(".");
        if key_path.len() == 2 {
            // Simple nested key: feature_flags.key
            let parsed_value = parse_value(value)?;
            if add_mode {
                // For add mode on objects, this is an upsert
                flags_obj.insert(key_path[1].clone(), parsed_value);
            } else {
                flags_obj.insert(key_path[1].clone(), parsed_value);
            }
        } else {
            // Deep nested key: feature_flags.parent.child...
            let mut current = flags_obj;
            for segment in &key_path[1..key_path.len() - 1] {
                current = current
                    .entry(segment.clone())
                    .or_insert_with(|| Value::Object(serde_json::Map::new()))
                    .as_object_mut()
                    .ok_or_else(|| format!("Cannot navigate through non-object at {}", segment))?;
            }
            let parsed_value = parse_value(value)?;
            current.insert(key_path.last().unwrap().clone(), parsed_value);
        }

        file_config.feature_flags = Some(flags);
        crate::config::save_file_config(&file_config)?;
        println!("+ [{}]: {}", nested_key, value);
        return Ok(());
    }

    if resolved.root() == "git_ai_hooks" {
        if key_path.len() != 2 {
            return Err(
                "git_ai_hooks requires a hook name (e.g., git_ai_hooks.post_notes_updated)"
                    .to_string(),
            );
        }

        let hook_name = key_path[1].clone();
        let mut hooks = file_config.git_ai_hooks.unwrap_or_default();

        if add_mode {
            let mut existing_commands = hooks.get(&hook_name).cloned().unwrap_or_default();
            let commands_to_add = parse_hook_command_values(value)?;
            existing_commands.extend(commands_to_add.clone());
            hooks.insert(hook_name.clone(), existing_commands);
            file_config.git_ai_hooks = Some(hooks);
            crate::config::save_file_config(&file_config)?;
            for command in commands_to_add {
                println!("+ [{}.{}]: {}", key_path[0], hook_name, command);
            }
        } else {
            let commands = parse_hook_command_values(value)?;
            hooks.insert(hook_name.clone(), commands.clone());
            file_config.git_ai_hooks = Some(hooks);
            crate::config::save_file_config(&file_config)?;
            for command in commands {
                println!("[{}.{}]: {}", key_path[0], hook_name, command);
            }
        }

        return Ok(());
    }

    if resolved.root() == "notes_backend" {
        if key_path.len() != 2 {
            return Err(
                "notes_backend requires a field name (notes_backend.kind or notes_backend.backend_url)"
                    .to_string(),
            );
        }
        let field = key_path[1].as_str();
        let mut backend = file_config.notes_backend.clone().unwrap_or_default();
        match field {
            "kind" => {
                let kind = parse_notes_backend_kind(value)?;
                backend.kind = kind;
                file_config.notes_backend = Some(backend);
                crate::config::save_file_config(&file_config)?;
                println!("[notes_backend.kind]: {}", kind.as_str());
            }
            "backend_url" => {
                backend.backend_url = Some(value.to_string());
                file_config.notes_backend = Some(backend);
                crate::config::save_file_config(&file_config)?;
                println!("[notes_backend.backend_url]: {}", value);
            }
            other => return Err(format!("Unknown notes_backend field: {}", other)),
        }
        return Ok(());
    }

    if resolved.root() == "author" {
        if add_mode {
            return Err("Cannot use --add with author fields".to_string());
        }
        if key_path.len() != 2 {
            return Err("author requires a field name (author.name or author.email)".to_string());
        }

        let mut author = file_config.author.clone().unwrap_or_default().normalized();
        let normalized_value = value.trim().to_string();
        if normalized_value.is_empty() {
            return Err(format!("author.{} cannot be empty", key_path[1]));
        }
        match key_path[1].as_str() {
            "name" => author.name = Some(normalized_value.clone()),
            "email" => author.email = Some(normalized_value.clone()),
            other => return Err(format!("Unknown author field: {}", other)),
        }

        file_config.author = Some(author);
        crate::config::save_file_config(&file_config)?;
        println!("[author.{}]: {}", key_path[1], normalized_value);
        return Ok(());
    }

    if resolved.root() == "custom_attributes" {
        if key_path.len() != 2 {
            return Err(
                "custom_attributes requires an attribute name (e.g., custom_attributes.team)"
                    .to_string(),
            );
        }
        let attr_name = key_path[1].trim();
        if attr_name.is_empty() {
            return Err("custom_attributes attribute name cannot be empty".to_string());
        }
        let mut attrs = file_config.custom_attributes.unwrap_or_default();
        attrs.insert(attr_name.to_string(), value.to_string());
        file_config.custom_attributes = Some(attrs);
        crate::config::save_file_config(&file_config)?;
        let prefix = if add_mode { "+ " } else { "" };
        println!("{}[custom_attributes.{}]: {}", prefix, attr_name, value);
        return Ok(());
    }

    Err(
        "Nested keys are only supported for feature_flags, git_ai_hooks, notes_backend, author, and custom_attributes"
            .to_string(),
    )
}
