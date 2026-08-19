use super::parse::parse_key_path;
use super::pattern::log_array_removals;
use super::spec::resolve_key;
use crate::config::NotesBackendKind;

pub(super) fn unset_config_value(key: &str) -> Result<(), String> {
    let mut file_config = crate::config::load_file_config_public()?;
    let resolved = resolve_key(key)?;
    let key_path = parse_key_path(key);

    // Handle top-level keys
    if key_path.len() == 1 {
        match resolved.spec.name {
            "git_path" => {
                let old_value = file_config.git_path.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [git_path]: {}", v);
                }
            }
            "exclude_prompts_in_repositories" => {
                let old_values = file_config.exclude_prompts_in_repositories.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(items) = old_values {
                    log_array_removals(&items);
                }
            }
            "allowed_repositories" | "allow_repositories" => {
                let old_values = file_config.allowed_repositories.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(items) = old_values {
                    log_array_removals(&items);
                }
            }
            "exclude_repositories" => {
                let old_values = file_config.exclude_repositories.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(items) = old_values {
                    log_array_removals(&items);
                }
            }
            "telemetry" => {
                let old_value = file_config.telemetry.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [telemetry]: {}", v);
                }
            }
            "telemetry_oss" => {
                let old_value = file_config.telemetry_oss.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [telemetry_oss]: {}", v);
                }
            }
            "telemetry_enterprise_dsn" => {
                let old_value = file_config.telemetry_enterprise_dsn.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [telemetry_enterprise_dsn]: {}", v);
                }
            }
            "disable_version_checks" => {
                let old_value = file_config.disable_version_checks.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [disable_version_checks]: {}", v);
                }
            }
            "disable_auto_updates" => {
                let old_value = file_config.disable_auto_updates.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [disable_auto_updates]: {}", v);
                }
            }
            "update_channel" => {
                let old_value = file_config.update_channel.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [update_channel]: {}", v);
                }
            }
            "feature_flags" => {
                let old_value = file_config.feature_flags.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [feature_flags]: {}", v);
                }
            }
            "api_base_url" => {
                let old_value = file_config.api_base_url.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [api_base_url]: {}", v);
                }
            }
            "api_key" => {
                let old_value = file_config.api_key.take();
                crate::config::save_file_config(&file_config)?;
                if old_value.is_some() {
                    println!("- [api_key]: ****");
                }
            }
            "prompt_storage" => {
                let old_value = file_config.prompt_storage.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [prompt_storage]: {}", v);
                }
            }
            "include_prompts_in_repositories" => {
                let old_value = file_config.include_prompts_in_repositories.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [include_prompts_in_repositories]: {:?}", v);
                }
            }
            "default_prompt_storage" => {
                let old_value = file_config.default_prompt_storage.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [default_prompt_storage]: {}", v);
                }
            }
            "quiet" => {
                let old_value = file_config.quiet.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [quiet]: {}", v);
                }
            }
            "author" => {
                let old_value = file_config.author.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!(
                        "- [author]: {}",
                        serde_json::to_string(&v)
                            .map_err(|e| format!("Failed to serialize author: {}", e))?
                    );
                }
            }
            "git_ai_hooks" => {
                let old_value = file_config.git_ai_hooks.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [git_ai_hooks]: {:?}", v);
                }
            }
            "codex_hooks_format" => {
                let old_value = file_config.codex_hooks_format.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [codex_hooks_format]: {}", v);
                }
            }
            "allow_superuser" => {
                let old_value = file_config.allow_superuser.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [allow_superuser]: {}", v);
                }
            }
            "transcript_streaming_lookback_days" => {
                let old_value = file_config.transcript_streaming_lookback_days.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [transcript_streaming_lookback_days]: {}", v);
                }
            }
            "max_checkpoint_file_size_bytes" => {
                let old_value = file_config.max_checkpoint_file_size_bytes.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [max_checkpoint_file_size_bytes]: {}", v);
                }
            }
            "max_checkpoint_total_size_bytes" => {
                let old_value = file_config.max_checkpoint_total_size_bytes.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [max_checkpoint_total_size_bytes]: {}", v);
                }
            }
            "max_checkpoint_total_lines" => {
                let old_value = file_config.max_checkpoint_total_lines.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [max_checkpoint_total_lines]: {}", v);
                }
            }
            "custom_attributes" => {
                let old_value = file_config.custom_attributes.take();
                crate::config::save_file_config(&file_config)?;
                if let Some(v) = old_value {
                    println!("- [custom_attributes]: {:?}", v);
                }
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

        let mut flags = file_config
            .feature_flags
            .ok_or_else(|| format!("Config key not found: {}", key))?;

        if !flags.is_object() {
            return Err("feature_flags must be a JSON object".to_string());
        }

        // Navigate to the parent of the key to remove
        let flags_obj = flags.as_object_mut().unwrap();
        let nested_key = key_path[1..].join(".");

        if key_path.len() == 2 {
            // Simple nested key: feature_flags.key
            let old_value = flags_obj.remove(&key_path[1]);
            if old_value.is_none() {
                return Err(format!("Config key not found: {}", key));
            }
            file_config.feature_flags = Some(flags);
            crate::config::save_file_config(&file_config)?;
            if let Some(v) = old_value {
                println!("- [{}]: {}", nested_key, v);
            }
        } else {
            // Deep nested key: feature_flags.parent.child...
            let mut current = flags_obj;
            for segment in &key_path[1..key_path.len() - 1] {
                current = current
                    .get_mut(segment)
                    .and_then(|v| v.as_object_mut())
                    .ok_or_else(|| format!("Config key not found: {}", key))?;
            }
            let old_value = current.remove(key_path.last().unwrap());
            if old_value.is_none() {
                return Err(format!("Config key not found: {}", key));
            }
            file_config.feature_flags = Some(flags);
            crate::config::save_file_config(&file_config)?;
            if let Some(v) = old_value {
                println!("- [{}]: {}", nested_key, v);
            }
        }

        return Ok(());
    }

    if resolved.root() == "git_ai_hooks" {
        if key_path.len() != 2 {
            return Err(
                "git_ai_hooks requires a hook name (e.g., git_ai_hooks.post_notes_updated)"
                    .to_string(),
            );
        }

        let hook_name = &key_path[1];
        let mut hooks = file_config
            .git_ai_hooks
            .ok_or_else(|| format!("Config key not found: {}", key))?;
        let old_value = hooks.remove(hook_name);
        if old_value.is_none() {
            return Err(format!("Config key not found: {}", key));
        }

        file_config.git_ai_hooks = if hooks.is_empty() { None } else { Some(hooks) };
        crate::config::save_file_config(&file_config)?;

        if let Some(commands) = old_value {
            for command in commands {
                println!("- [{}]: {}", key, command);
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
                let old = backend.kind;
                backend.kind = NotesBackendKind::GitNotes; // reset to default
                file_config.notes_backend = Some(backend);
                crate::config::save_file_config(&file_config)?;
                println!("- [notes_backend.kind]: {}", old.as_str());
            }
            "backend_url" => {
                if let Some(old_url) = backend.backend_url.take() {
                    file_config.notes_backend = if backend.kind == NotesBackendKind::GitNotes {
                        None // whole object is back to defaults, omit from file
                    } else {
                        Some(backend)
                    };
                    crate::config::save_file_config(&file_config)?;
                    println!("- [notes_backend.backend_url]: {}", old_url);
                }
            }
            other => return Err(format!("Unknown notes_backend field: {}", other)),
        }
        return Ok(());
    }

    if resolved.root() == "author" {
        if key_path.len() != 2 {
            return Err("author requires a field name (author.name or author.email)".to_string());
        }

        let mut author = file_config.author.clone().unwrap_or_default().normalized();
        let old_value = match key_path[1].as_str() {
            "name" => author.name.take(),
            "email" => author.email.take(),
            other => return Err(format!("Unknown author field: {}", other)),
        };

        file_config.author = if author.is_empty() {
            None
        } else {
            Some(author)
        };
        crate::config::save_file_config(&file_config)?;
        if let Some(v) = old_value {
            println!("- [author.{}]: {}", key_path[1], v);
        }
        return Ok(());
    }

    if resolved.root() == "custom_attributes" {
        if key_path.len() != 2 {
            return Err(
                "custom_attributes requires an attribute name (e.g., custom_attributes.team)"
                    .to_string(),
            );
        }
        // Trim to match the nested `set` path, which stores the trimmed name;
        // otherwise an attribute set as `custom_attributes. team` (stored as
        // `team`) could not be removed by the same dotted key.
        let attr_name = key_path[1].trim();
        let mut attrs = file_config
            .custom_attributes
            .ok_or_else(|| format!("Config key not found: {}", key))?;
        let old_value = attrs.remove(attr_name);
        if old_value.is_none() {
            return Err(format!("Config key not found: {}", key));
        }

        file_config.custom_attributes = if attrs.is_empty() { None } else { Some(attrs) };
        crate::config::save_file_config(&file_config)?;
        if let Some(v) = old_value {
            println!("- [custom_attributes.{}]: {}", attr_name, v);
        }
        return Ok(());
    }

    Err(
        "Nested keys are only supported for feature_flags, git_ai_hooks, notes_backend, author, and custom_attributes"
            .to_string(),
    )
}
