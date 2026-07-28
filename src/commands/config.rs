use dirs;
use serde_json::Value;
use std::collections::HashMap;

use crate::config::{AuthorConfig, CodexHooksFormat, NotesBackendKind};
use crate::git::repository::find_repository_in_path;

/// Determines the type of pattern value provided
#[derive(Debug, PartialEq)]
enum PatternType {
    /// Global wildcard pattern like "*"
    GlobalWildcard,
    /// URL or git protocol (http://, https://, git@, ssh://, etc.)
    UrlOrGitProtocol,
    /// File path that should be resolved to a repository
    FilePath,
}

/// Detect the type of pattern value
fn detect_pattern_type(value: &str) -> PatternType {
    let trimmed = value.trim();

    // Check for global wildcard
    if trimmed == "*" {
        return PatternType::GlobalWildcard;
    }

    // Check for URL or git protocol patterns
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("git@")
        || trimmed.starts_with("ssh://")
        || trimmed.starts_with("git://")
        || trimmed.contains("://")
        || (trimmed.contains('@') && trimmed.contains(':') && !trimmed.starts_with('/'))
    {
        return PatternType::UrlOrGitProtocol;
    }

    // Check for glob patterns with wildcards (but not just "*")
    // These are patterns like "https://github.com/org/*" or "*@github.com:*"
    if trimmed.contains('*') || trimmed.contains('?') || trimmed.contains('[') {
        return PatternType::UrlOrGitProtocol;
    }

    // Otherwise, treat as file path
    PatternType::FilePath
}

/// Resolve a file path to repository remote URLs
/// Returns the remote URLs for the repository at the given path
fn resolve_path_to_remotes(path: &str) -> Result<Vec<String>, String> {
    // Expand ~ to home directory
    let expanded_path = if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            format!("{}{}", home.to_string_lossy(), &path[1..])
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };

    // Try to find repository at path
    let repo = find_repository_in_path(&expanded_path).map_err(|_| {
        format!(
            "No git repository found at path '{}'. Provide a valid repository path, URL, or glob pattern.",
            path
        )
    })?;

    // Get remotes with URLs
    let remotes = repo
        .remotes_with_urls()
        .map_err(|e| format!("Failed to get remotes for repository at '{}': {}", path, e))?;

    if remotes.is_empty() {
        return Err(format!(
            "Repository at '{}' has no remotes configured. Add a remote first or use a glob pattern.",
            path
        ));
    }

    // Return all remote URLs
    Ok(remotes.into_iter().map(|(_, url)| url).collect())
}

fn print_config_help() {
    println!("git-ai config - View and manage git-ai configuration");
    println!();
    println!("Usage:");
    println!("  git-ai config                Show all config as formatted JSON");
    println!("  git-ai config <key>          Show specific config value");
    println!("  git-ai config set <key> <value>          Set a config value");
    println!("  git-ai config set <key> <value> --add    Add to array (extends existing)");
    println!("  git-ai config --add <key> <value>        Add to array or upsert into object");
    println!("  git-ai config unset <key>    Remove config value (reverts to default)");
    println!();
    println!("Configuration Keys:");
    println!("  git_path                     Path to git binary");
    println!("  exclude_prompts_in_repositories  Repos to exclude prompts from (array)");
    println!("  allow_repositories           Allowed repos (array)");
    println!("  exclude_repositories         Excluded repos (array)");
    println!("  telemetry_oss                OSS telemetry setting (on/off)");
    println!("  telemetry_enterprise_dsn     Enterprise telemetry DSN");
    println!("  disable_version_checks       Disable version checks (bool)");
    println!("  disable_auto_updates         Disable auto updates (bool)");
    println!("  update_channel               Update channel (latest/next)");
    println!("  feature_flags                Feature flags (object)");
    println!("  api_base_url                 API base URL (default: https://usegitai.com)");
    println!("  api_key                      API key for X-API-Key header");
    println!("  author.name                  git-ai author display name override");
    println!("  author.email                 git-ai author email override");
    println!("  prompt_storage               Prompt storage mode (default/notes/local)");
    println!("  include_prompts_in_repositories  Repos to include for prompt storage (array)");
    println!("  default_prompt_storage       Fallback storage mode for non-included repos");
    println!("  quiet                        Suppress chart output after commits (bool)");
    println!("  allow_superuser              Allow running git-ai as root/superuser (bool)");
    println!(
        "  transcript_streaming_lookback_days  Days to look back when sweeping transcripts (0 = unlimited)"
    );
    println!("  max_checkpoint_file_size_bytes      Per-file checkpoint content limit in bytes");
    println!("  max_checkpoint_total_size_bytes     Per-checkpoint content limit in bytes");
    println!("  max_checkpoint_total_lines          Per-checkpoint content limit in lines");
    println!("  custom_attributes            Custom telemetry attributes, string->string (object)");
    println!("  git_ai_hooks                 Hook name -> shell commands map (object)");
    println!("  codex_hooks_format           Codex hook install format (config_toml/hooks_json)");
    println!("  notes_backend.kind           Notes backend kind (git_notes/http)");
    println!("  notes_backend.backend_url    Notes backend base URL. Required when kind=http.");
    println!(
        "                               May include a path prefix; endpoints are appended to it."
    );
    println!(
        "                               e.g. \"https://app.example.com/api/gitai\" -> requests are"
    );
    println!("                               sent to \"<base>/worker/notes/upload\" and");
    println!("                               \"<base>/worker/notes/?commits=...\".");
    println!();
    println!("Repository Patterns:");
    println!("  For exclude/allow/exclude_prompts_in_repositories, you can provide:");
    println!("    - A glob pattern: \"*\", \"https://github.com/org/*\"");
    println!("    - A URL/git protocol: \"git@github.com:org/repo.git\"");
    println!("    - A file path: \".\" or \"/path/to/repo\" (resolves to repo's remotes)");
    println!();
    println!("Examples:");
    println!("  git-ai config exclude_repositories");
    println!("  git-ai config set disable_auto_updates true");
    println!("  git-ai config set author.name \"Alice Example\"");
    println!("  git-ai config set author.email alice@example.com");
    println!("  git-ai config set exclude_repositories \"private/*\"");
    println!("  git-ai config set exclude_repositories .         # Uses current repo's remotes");
    println!("  git-ai config --add exclude_repositories \"temp/*\"");
    println!("  git-ai config --add allow_repositories ~/projects/my-repo");
    println!("  git-ai config --add feature_flags.my_flag true");
    println!("  git-ai config --add git_ai_hooks.post_notes_updated \"./my-hook.sh\"");
    println!("  git-ai config set codex_hooks_format hooks_json");
    println!("  git-ai config set allow_superuser true");
    println!("  git-ai config set transcript_streaming_lookback_days 1");
    println!("  git-ai config set custom_attributes '{{\"team\":\"platform\"}}'");
    println!("  git-ai config --add custom_attributes.team platform");
    println!("  git-ai config unset exclude_repositories");
    println!();
    std::process::exit(0);
}

pub fn handle_config(args: &[String]) {
    if args.is_empty() {
        // Show all config
        if let Err(e) = show_all_config() {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    // Check for help flags
    if args[0] == "--help" || args[0] == "-h" || args[0] == "help" {
        print_config_help();
        return;
    }

    // Check for --add flag anywhere in args
    let is_add_mode = args.iter().any(|a| a == "--add");
    let filtered_args: Vec<&String> = args.iter().filter(|a| *a != "--add").collect();

    if filtered_args.is_empty() {
        // Show all config if only --add was passed (which doesn't make sense)
        eprintln!("Error: --add requires <key> <value>");
        eprintln!("Usage: git-ai config --add <key> <value>");
        eprintln!("   or: git-ai config set <key> <value> --add");
        std::process::exit(1);
    }

    match filtered_args[0].as_str() {
        "set" => {
            if filtered_args.len() < 3 {
                eprintln!("Error: set requires <key> <value>");
                eprintln!("Usage: git-ai config set <key> <value>");
                std::process::exit(1);
            }
            let key = filtered_args[1].as_str();
            let value = filtered_args[2].as_str();
            if let Err(e) = set_config_value(key, value, is_add_mode) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            if key == "feature_flags.transcript_streaming"
                || key == "feature_flags.transcript_sweep"
                || key == "feature_flags.hooks_sync_interval"
                || key == "transcript_streaming_lookback_days"
            {
                println!("Run `git-ai bg restart` for changes to take effect.");
            }
        }
        "unset" => {
            if filtered_args.len() < 2 {
                eprintln!("Error: unset requires <key>");
                eprintln!("Usage: git-ai config unset <key>");
                std::process::exit(1);
            }
            let key = filtered_args[1].as_str();
            if let Err(e) = unset_config_value(key) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        key => {
            if is_add_mode {
                // git-ai config --add <key> <value>
                if filtered_args.len() < 2 {
                    eprintln!("Error: --add requires <key> <value>");
                    eprintln!("Usage: git-ai config --add <key> <value>");
                    std::process::exit(1);
                }
                let value = filtered_args[1].as_str();
                if let Err(e) = set_config_value(key, value, true) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            } else {
                // Get single value
                if let Err(e) = get_config_value(key) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}

fn show_all_config() -> Result<(), String> {
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

    if let Some(ref repos) = file_config.allow_repositories {
        effective_config.insert(
            "allow_repositories".to_string(),
            serde_json::to_value(repos).unwrap(),
        );
    } else {
        effective_config.insert("allow_repositories".to_string(), Value::Array(vec![]));
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

fn get_config_value(key: &str) -> Result<(), String> {
    let file_config = crate::config::load_file_config_public()?;
    let runtime_config = crate::config::Config::get();

    let key_path = parse_key_path(key);

    // Handle top-level keys
    if key_path.len() == 1 {
        let value = match key_path[0].as_str() {
            "git_path" => Value::String(runtime_config.git_cmd().to_string()),
            "exclude_prompts_in_repositories" => {
                if let Some(ref repos) = file_config.exclude_prompts_in_repositories {
                    serde_json::to_value(repos).unwrap()
                } else {
                    Value::Array(vec![])
                }
            }
            "allow_repositories" => {
                if let Some(ref repos) = file_config.allow_repositories {
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
            "telemetry_oss_disabled" => Value::Bool(runtime_config.is_telemetry_oss_disabled()),
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
    if key_path[0] == "feature_flags" || key_path[0] == "git_ai_hooks" {
        let root = if key_path[0] == "feature_flags" {
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

    if key_path[0] == "notes_backend" {
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

    if key_path[0] == "author" {
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

    if key_path[0] == "custom_attributes" {
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

fn set_config_value(key: &str, value: &str, add_mode: bool) -> Result<(), String> {
    let mut file_config = crate::config::load_file_config_public()?;
    let key_path = parse_key_path(key);

    // Handle top-level keys
    if key_path.len() == 1 {
        match key_path[0].as_str() {
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
            "allow_repositories" => {
                let added = set_repository_array_field(
                    &mut file_config.allow_repositories,
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
    if key_path[0] == "feature_flags" {
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

    if key_path[0] == "git_ai_hooks" {
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

    if key_path[0] == "notes_backend" {
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
                eprintln!("[notes_backend.kind]: {}", kind.as_str());
            }
            "backend_url" => {
                backend.backend_url = Some(value.to_string());
                file_config.notes_backend = Some(backend);
                crate::config::save_file_config(&file_config)?;
                eprintln!("[notes_backend.backend_url]: {}", value);
            }
            other => return Err(format!("Unknown notes_backend field: {}", other)),
        }
        return Ok(());
    }

    if key_path[0] == "author" {
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

    if key_path[0] == "custom_attributes" {
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

fn unset_config_value(key: &str) -> Result<(), String> {
    let mut file_config = crate::config::load_file_config_public()?;
    let key_path = parse_key_path(key);

    // Handle top-level keys
    if key_path.len() == 1 {
        match key_path[0].as_str() {
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
            "allow_repositories" => {
                let old_values = file_config.allow_repositories.take();
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
    if key_path[0] == "feature_flags" {
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

    if key_path[0] == "git_ai_hooks" {
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

    if key_path[0] == "notes_backend" {
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
                eprintln!("- [notes_backend.kind]: {}", old.as_str());
            }
            "backend_url" => {
                if let Some(old_url) = backend.backend_url.take() {
                    file_config.notes_backend = if backend.kind == NotesBackendKind::GitNotes {
                        None // whole object is back to defaults, omit from file
                    } else {
                        Some(backend)
                    };
                    crate::config::save_file_config(&file_config)?;
                    eprintln!("- [notes_backend.backend_url]: {}", old_url);
                }
            }
            other => return Err(format!("Unknown notes_backend field: {}", other)),
        }
        return Ok(());
    }

    if key_path[0] == "author" {
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

    if key_path[0] == "custom_attributes" {
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

fn parse_key_path(key: &str) -> Vec<String> {
    key.split('.').map(|s| s.to_string()).collect()
}

/// Set array field for repository patterns (exclude_repositories, allow_repositories, exclude_prompts_in_repositories)
/// This function handles the special logic of detecting if a value is:
///  - A global wildcard pattern like "*"
///  - A URL or git protocol pattern
///  - A file path that should be resolved to repository remotes
///
/// Returns the values that were added/set for logging purposes
fn set_repository_array_field(
    field: &mut Option<Vec<String>>,
    value: &str,
    add_mode: bool,
) -> Result<Vec<String>, String> {
    // Resolve the value(s) to add
    let values_to_add = resolve_repository_value(value)?;

    if add_mode {
        // Add mode: append to existing array
        let mut arr = field.take().unwrap_or_default();
        let added = values_to_add.clone();
        arr.extend(values_to_add);
        *field = Some(arr);
        Ok(added)
    } else {
        // Set mode: try to parse as JSON array, or use resolved values
        if value.starts_with('[') {
            // Parse as JSON array
            let json_value: Value =
                serde_json::from_str(value).map_err(|e| format!("Invalid JSON array: {}", e))?;
            if let Value::Array(arr) = json_value {
                let mut resolved_values = Vec::new();
                for v in arr {
                    if let Value::String(s) = v {
                        let resolved = resolve_repository_value(&s)?;
                        resolved_values.extend(resolved);
                    } else {
                        return Err("Array must contain only strings".to_string());
                    }
                }
                let added = resolved_values.clone();
                *field = Some(resolved_values);
                Ok(added)
            } else {
                Err("Expected a JSON array".to_string())
            }
        } else {
            // Single value - use the resolved values
            let added = values_to_add.clone();
            *field = Some(values_to_add);
            Ok(added)
        }
    }
}

/// Resolve a repository value - returns the actual patterns to store
/// For file paths, resolves to repository remote URLs
/// For URLs/patterns, returns as-is
fn resolve_repository_value(value: &str) -> Result<Vec<String>, String> {
    match detect_pattern_type(value) {
        PatternType::GlobalWildcard | PatternType::UrlOrGitProtocol => {
            // Return as-is
            Ok(vec![value.to_string()])
        }
        PatternType::FilePath => {
            // Resolve to repository remote URLs
            resolve_path_to_remotes(value)
        }
    }
}

/// Log array changes with + prefix for add mode, or just list items for set mode
fn log_array_changes(items: &[String], add_mode: bool) {
    #[allow(clippy::if_same_then_else)]
    if add_mode {
        for item in items {
            println!("+ {}", item);
        }
    } else {
        for item in items {
            println!("+ {}", item);
        }
    }
}

/// Log array removals with - prefix
fn log_array_removals(items: &[String]) {
    for item in items {
        println!("- {}", item);
    }
}

fn parse_git_ai_hooks_object(value: &str) -> Result<HashMap<String, Vec<String>>, String> {
    let parsed: Value =
        serde_json::from_str(value).map_err(|e| format!("Invalid JSON for git_ai_hooks: {}", e))?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| "git_ai_hooks must be a JSON object".to_string())?;

    let mut hooks = HashMap::new();
    for (hook_name, commands_value) in obj {
        let name = hook_name.trim();
        if name.is_empty() {
            return Err("git_ai_hooks contains an empty hook name".to_string());
        }
        let commands = parse_hook_commands_value(commands_value)?;
        hooks.insert(name.to_string(), commands);
    }
    Ok(hooks)
}

/// Parse a JSON object of custom telemetry attributes.
///
/// String/number/bool values are coerced to strings using the same rules as the
/// `GIT_AI_CUSTOM_ATTRIBUTES` env var override (see `build_custom_attributes`).
/// Unlike the env path, which silently drops non-scalar values, the CLI rejects
/// them so a malformed `config set` fails loudly rather than persisting a
/// partially-applied object.
fn parse_custom_attributes_object(value: &str) -> Result<HashMap<String, String>, String> {
    let parsed: Value = serde_json::from_str(value)
        .map_err(|e| format!("Invalid JSON for custom_attributes: {}", e))?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| "custom_attributes must be a JSON object".to_string())?;

    let mut attrs = HashMap::new();
    for (attr_name, attr_value) in obj {
        let name = attr_name.trim();
        if name.is_empty() {
            return Err("custom_attributes contains an empty attribute name".to_string());
        }
        let coerced = match attr_value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => {
                return Err(format!(
                    "custom_attributes value for '{}' must be a string, number, or boolean",
                    name
                ));
            }
        };
        attrs.insert(name.to_string(), coerced);
    }
    Ok(attrs)
}

fn parse_hook_command_values(value: &str) -> Result<Vec<String>, String> {
    if let Ok(parsed) = serde_json::from_str::<Value>(value)
        && (parsed.is_string() || parsed.is_array())
    {
        return parse_hook_commands_value(&parsed);
    }

    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("Hook command cannot be empty".to_string());
    }
    Ok(vec![trimmed.to_string()])
}

fn parse_hook_commands_value(value: &Value) -> Result<Vec<String>, String> {
    match value {
        Value::String(command) => {
            let trimmed = command.trim();
            if trimmed.is_empty() {
                return Err("Hook command cannot be empty".to_string());
            }
            Ok(vec![trimmed.to_string()])
        }
        Value::Array(items) => {
            let mut commands = Vec::new();
            for item in items {
                let command = item.as_str().ok_or_else(|| {
                    "git_ai_hooks hook values must be a string or an array of strings".to_string()
                })?;
                let trimmed = command.trim();
                if trimmed.is_empty() {
                    return Err("Hook command cannot be empty".to_string());
                }
                commands.push(trimmed.to_string());
            }
            if commands.is_empty() {
                return Err("Hook command array cannot be empty".to_string());
            }
            Ok(commands)
        }
        _ => Err("git_ai_hooks hook values must be a string or an array of strings".to_string()),
    }
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value.to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(format!(
            "Invalid boolean value: '{}'. Expected true/false",
            value
        )),
    }
}

fn parse_value(value: &str) -> Result<Value, String> {
    // Try to parse as JSON first
    if let Ok(json_value) = serde_json::from_str::<Value>(value) {
        return Ok(json_value);
    }

    // Otherwise treat as string
    Ok(Value::String(value.to_string()))
}

fn parse_author_config_object(value: &str) -> Result<AuthorConfig, String> {
    let parsed: Value =
        serde_json::from_str(value).map_err(|e| format!("Invalid JSON for author: {}", e))?;
    if !parsed.is_object() {
        return Err("author must be a JSON object".to_string());
    }

    serde_json::from_value::<AuthorConfig>(parsed)
        .map(AuthorConfig::normalized)
        .map_err(|e| format!("Invalid author config: {}", e))
}

/// Mask an API key for display (show first 4 and last 4 chars if long enough)
fn mask_api_key(key: &str) -> String {
    if key.len() > 8 {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    } else {
        "****".to_string()
    }
}

/// Parse notes backend kind from a string value
fn parse_notes_backend_kind(value: &str) -> Result<NotesBackendKind, String> {
    match value.trim().to_lowercase().as_str() {
        "git_notes" | "git-notes" => Ok(NotesBackendKind::GitNotes),
        "http" => Ok(NotesBackendKind::Http),
        _ => Err(format!(
            "Invalid notes_backend.kind '{}'. Expected 'git_notes' or 'http'",
            value
        )),
    }
}

fn parse_codex_hooks_format(value: &str) -> Result<CodexHooksFormat, String> {
    match value.trim().to_lowercase().as_str() {
        "config_toml" | "config-toml" => Ok(CodexHooksFormat::ConfigToml),
        "hooks_json" | "hooks-json" => Ok(CodexHooksFormat::HooksJson),
        _ => Err(format!(
            "Invalid codex_hooks_format '{}'. Expected 'config_toml' or 'hooks_json'",
            value
        )),
    }
}

/// Validate prompt_storage value
fn validate_prompt_storage_value(value: &str) -> Result<(), String> {
    if value != "default" && value != "notes" && value != "local" {
        return Err(format!(
            "Invalid prompt_storage value '{}'. Expected 'default', 'notes', or 'local'",
            value
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prompt_storage_valid_values() {
        for value in ["default", "notes", "local"] {
            let result = validate_prompt_storage_value(value);
            assert!(result.is_ok(), "Expected '{}' to be valid", value);
        }
    }

    #[test]
    fn test_prompt_storage_invalid_value() {
        for value in ["invalid", "defaults", "note", "", "DEFAULT", "NOTES"] {
            let result = validate_prompt_storage_value(value);
            assert!(result.is_err(), "Expected '{}' to be invalid", value);
        }
    }

    #[test]
    fn test_prompt_storage_invalid_value_error_message() {
        let result = validate_prompt_storage_value("invalid");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("invalid"));
        assert!(err.contains("default"));
        assert!(err.contains("notes"));
        assert!(err.contains("local"));
    }

    #[test]
    fn test_codex_hooks_format_valid_values() {
        assert_eq!(
            parse_codex_hooks_format("config_toml").unwrap(),
            CodexHooksFormat::ConfigToml
        );
        assert_eq!(
            parse_codex_hooks_format("hooks_json").unwrap(),
            CodexHooksFormat::HooksJson
        );
    }

    #[test]
    fn test_codex_hooks_format_invalid_value() {
        let result = parse_codex_hooks_format("json");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("config_toml"));
        assert!(err.contains("hooks_json"));
    }

    #[test]
    fn test_parse_bool_valid_true_values() {
        for value in ["true", "1", "yes", "on", "TRUE", "True", "YES", "ON"] {
            let result = parse_bool(value);
            assert!(result.is_ok(), "Expected '{}' to parse as bool", value);
            assert!(result.unwrap(), "Expected '{}' to be true", value);
        }
    }

    #[test]
    fn test_parse_bool_valid_false_values() {
        for value in ["false", "0", "no", "off", "FALSE", "False", "NO", "OFF"] {
            let result = parse_bool(value);
            assert!(result.is_ok(), "Expected '{}' to parse as bool", value);
            assert!(!result.unwrap(), "Expected '{}' to be false", value);
        }
    }

    #[test]
    fn test_parse_bool_invalid_value() {
        let result = parse_bool("invalid");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("Invalid boolean value"));
        assert!(err.contains("invalid"));
    }

    #[test]
    fn test_parse_hook_command_values_supports_plain_string() {
        let commands = parse_hook_command_values("./hooks/post-notes.sh").unwrap();
        assert_eq!(commands, vec!["./hooks/post-notes.sh"]);
    }

    #[test]
    fn test_parse_hook_command_values_supports_json_array() {
        let commands = parse_hook_command_values(r#"["a","b"]"#).unwrap();
        assert_eq!(commands, vec!["a", "b"]);
    }

    #[test]
    fn test_parse_hook_command_values_json_primitive_falls_back_to_string() {
        let commands = parse_hook_command_values("true").unwrap();
        assert_eq!(commands, vec!["true"]);
    }

    #[test]
    fn test_parse_git_ai_hooks_object() {
        let hooks =
            parse_git_ai_hooks_object(r#"{"post_notes_updated":["./hook-a.sh","./hook-b.sh"]}"#)
                .unwrap();
        assert_eq!(
            hooks.get("post_notes_updated"),
            Some(&vec!["./hook-a.sh".to_string(), "./hook-b.sh".to_string()])
        );
    }

    #[test]
    fn test_parse_custom_attributes_object_string_values() {
        let attrs = parse_custom_attributes_object(r#"{"team":"platform","env":"prod"}"#).unwrap();
        assert_eq!(attrs.get("team"), Some(&"platform".to_string()));
        assert_eq!(attrs.get("env"), Some(&"prod".to_string()));
    }

    #[test]
    fn test_parse_custom_attributes_object_coerces_number_and_bool() {
        let attrs = parse_custom_attributes_object(r#"{"count":3,"enabled":true}"#).unwrap();
        assert_eq!(attrs.get("count"), Some(&"3".to_string()));
        assert_eq!(attrs.get("enabled"), Some(&"true".to_string()));
    }

    #[test]
    fn test_parse_custom_attributes_object_rejects_non_object() {
        let err = parse_custom_attributes_object(r#"["a","b"]"#).unwrap_err();
        assert!(err.contains("custom_attributes must be a JSON object"));
    }

    #[test]
    fn test_parse_custom_attributes_object_rejects_nested_value() {
        let err = parse_custom_attributes_object(r#"{"team":{"nested":"x"}}"#).unwrap_err();
        assert!(err.contains("must be a string, number, or boolean"));
    }

    #[test]
    fn test_parse_custom_attributes_object_rejects_empty_name() {
        let err = parse_custom_attributes_object(r#"{"  ":"x"}"#).unwrap_err();
        assert!(err.contains("empty attribute name"));
    }

    // --- Additional comprehensive tests ---

    #[test]
    fn test_parse_value_json_string() {
        let result = parse_value("\"hello\"").unwrap();
        assert_eq!(result, Value::String("hello".to_string()));
    }

    #[test]
    fn test_parse_value_json_number() {
        let result = parse_value("42").unwrap();
        assert_eq!(result, Value::Number(serde_json::Number::from(42)));
    }

    #[test]
    fn test_parse_value_json_boolean() {
        let result = parse_value("true").unwrap();
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn test_parse_value_json_array() {
        let result = parse_value("[1,2,3]").unwrap();
        assert!(result.is_array());
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 3);
    }

    #[test]
    fn test_parse_value_json_object() {
        let result = parse_value(r#"{"key":"value"}"#).unwrap();
        assert!(result.is_object());
    }

    #[test]
    fn test_parse_value_plain_string() {
        let result = parse_value("plain text").unwrap();
        assert_eq!(result, Value::String("plain text".to_string()));
    }

    #[test]
    fn test_parse_author_config_object() {
        let author =
            parse_author_config_object(r#"{"name":"  Alice Example  ","email":"a@example.com"}"#)
                .unwrap();
        assert_eq!(author.name.as_deref(), Some("Alice Example"));
        assert_eq!(author.email.as_deref(), Some("a@example.com"));
    }

    #[test]
    fn test_parse_author_config_object_rejects_non_object() {
        let err = parse_author_config_object(r#""Alice""#).unwrap_err();
        assert!(err.contains("author must be a JSON object"));
    }

    #[test]
    fn test_mask_api_key_long() {
        let key = "abcdefghijklmnop";
        let masked = mask_api_key(key);
        assert_eq!(masked, "abcd...mnop");
    }

    #[test]
    fn test_mask_api_key_short() {
        let key = "short";
        let masked = mask_api_key(key);
        assert_eq!(masked, "****");
    }

    #[test]
    fn test_mask_api_key_exactly_eight() {
        let key = "12345678";
        let masked = mask_api_key(key);
        assert_eq!(masked, "****");
    }

    #[test]
    fn test_mask_api_key_nine_chars() {
        let key = "123456789";
        let masked = mask_api_key(key);
        assert_eq!(masked, "1234...6789");
    }

    #[test]
    fn test_parse_key_path_single() {
        let result = parse_key_path("key");
        assert_eq!(result, vec!["key"]);
    }

    #[test]
    fn test_parse_key_path_nested() {
        let result = parse_key_path("parent.child");
        assert_eq!(result, vec!["parent", "child"]);
    }

    #[test]
    fn test_parse_key_path_deeply_nested() {
        let result = parse_key_path("a.b.c.d");
        assert_eq!(result, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn test_parse_key_path_empty() {
        let result = parse_key_path("");
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn test_detect_pattern_type_global_wildcard() {
        assert_eq!(detect_pattern_type("*"), PatternType::GlobalWildcard);
        assert_eq!(detect_pattern_type(" * "), PatternType::GlobalWildcard);
    }

    #[test]
    fn test_detect_pattern_type_http_url() {
        assert_eq!(
            detect_pattern_type("http://github.com/org/repo"),
            PatternType::UrlOrGitProtocol
        );
        assert_eq!(
            detect_pattern_type("https://github.com/org/repo"),
            PatternType::UrlOrGitProtocol
        );
    }

    #[test]
    fn test_detect_pattern_type_git_ssh() {
        assert_eq!(
            detect_pattern_type("git@github.com:org/repo.git"),
            PatternType::UrlOrGitProtocol
        );
    }

    #[test]
    fn test_detect_pattern_type_ssh_url() {
        assert_eq!(
            detect_pattern_type("ssh://git@github.com/org/repo"),
            PatternType::UrlOrGitProtocol
        );
    }

    #[test]
    fn test_detect_pattern_type_git_protocol() {
        assert_eq!(
            detect_pattern_type("git://github.com/org/repo"),
            PatternType::UrlOrGitProtocol
        );
    }

    #[test]
    fn test_detect_pattern_type_wildcard_in_url() {
        assert_eq!(
            detect_pattern_type("https://github.com/org/*"),
            PatternType::UrlOrGitProtocol
        );
    }

    #[test]
    fn test_detect_pattern_type_question_mark_pattern() {
        assert_eq!(detect_pattern_type("repo-?"), PatternType::UrlOrGitProtocol);
    }

    #[test]
    fn test_detect_pattern_type_bracket_pattern() {
        assert_eq!(
            detect_pattern_type("[abc]def"),
            PatternType::UrlOrGitProtocol
        );
    }

    #[test]
    fn test_detect_pattern_type_file_path_relative() {
        assert_eq!(detect_pattern_type("./path/to/repo"), PatternType::FilePath);
        assert_eq!(detect_pattern_type("path/to/repo"), PatternType::FilePath);
    }

    #[test]
    fn test_detect_pattern_type_file_path_absolute() {
        assert_eq!(detect_pattern_type("/path/to/repo"), PatternType::FilePath);
    }

    #[test]
    fn test_detect_pattern_type_file_path_home() {
        assert_eq!(detect_pattern_type("~/repo"), PatternType::FilePath);
    }

    #[test]
    fn test_detect_pattern_type_single_dot() {
        assert_eq!(detect_pattern_type("."), PatternType::FilePath);
    }

    #[test]
    fn test_detect_pattern_type_double_dot() {
        assert_eq!(detect_pattern_type(".."), PatternType::FilePath);
    }

    #[test]
    fn test_resolve_repository_value_wildcard() {
        let result = resolve_repository_value("*").unwrap();
        assert_eq!(result, vec!["*"]);
    }

    #[test]
    fn test_resolve_repository_value_url() {
        let result = resolve_repository_value("https://github.com/org/repo").unwrap();
        assert_eq!(result, vec!["https://github.com/org/repo"]);
    }

    #[test]
    fn test_resolve_repository_value_git_ssh() {
        let result = resolve_repository_value("git@github.com:org/repo.git").unwrap();
        assert_eq!(result, vec!["git@github.com:org/repo.git"]);
    }

    #[test]
    fn test_log_array_changes_add_mode() {
        let items = vec!["item1".to_string(), "item2".to_string()];
        // Just test that it doesn't panic - output goes to stderr
        log_array_changes(&items, true);
    }

    #[test]
    fn test_log_array_changes_set_mode() {
        let items = vec!["item1".to_string(), "item2".to_string()];
        // Just test that it doesn't panic - output goes to stderr
        log_array_changes(&items, false);
    }

    #[test]
    fn test_log_array_removals() {
        let items = vec!["item1".to_string(), "item2".to_string()];
        // Just test that it doesn't panic - output goes to stderr
        log_array_removals(&items);
    }

    #[test]
    fn test_log_array_changes_empty() {
        let items: Vec<String> = vec![];
        log_array_changes(&items, true);
        log_array_changes(&items, false);
    }

    #[test]
    fn test_log_array_removals_empty() {
        let items: Vec<String> = vec![];
        log_array_removals(&items);
    }

    #[test]
    fn test_parse_bool_case_insensitive() {
        assert!(parse_bool("TRUE").unwrap());
        assert!(parse_bool("True").unwrap());
        assert!(parse_bool("tRuE").unwrap());
        assert!(!parse_bool("FALSE").unwrap());
        assert!(!parse_bool("False").unwrap());
        assert!(!parse_bool("fAlSe").unwrap());
    }

    #[test]
    fn test_parse_bool_numeric() {
        assert!(parse_bool("1").unwrap());
        assert!(!parse_bool("0").unwrap());
    }

    #[test]
    fn test_parse_bool_word_forms() {
        assert!(parse_bool("yes").unwrap());
        assert!(parse_bool("YES").unwrap());
        assert!(parse_bool("on").unwrap());
        assert!(parse_bool("ON").unwrap());
        assert!(!parse_bool("no").unwrap());
        assert!(!parse_bool("NO").unwrap());
        assert!(!parse_bool("off").unwrap());
        assert!(!parse_bool("OFF").unwrap());
    }

    #[test]
    fn test_parse_bool_invalid_number() {
        assert!(parse_bool("2").is_err());
        assert!(parse_bool("-1").is_err());
    }

    #[test]
    fn test_parse_bool_empty_string() {
        assert!(parse_bool("").is_err());
    }

    #[test]
    fn test_parse_bool_whitespace() {
        // Whitespace is not trimmed by parse_bool
        assert!(parse_bool(" true").is_err());
        assert!(parse_bool("true ").is_err());
    }

    #[test]
    fn test_pattern_type_combinations() {
        // Test edge cases with @ and : characters
        assert_eq!(
            detect_pattern_type("user@host:path"),
            PatternType::UrlOrGitProtocol
        );
        assert_eq!(detect_pattern_type("@:"), PatternType::UrlOrGitProtocol);
        // @ but no : means file path
        assert_eq!(detect_pattern_type("file@name"), PatternType::FilePath);
        // : but no @ means file path (unless absolute)
        assert_eq!(detect_pattern_type("file:name"), PatternType::FilePath);
    }

    #[test]
    fn test_pattern_type_custom_protocols() {
        assert_eq!(
            detect_pattern_type("custom://host/path"),
            PatternType::UrlOrGitProtocol
        );
        assert_eq!(
            detect_pattern_type("ftp://host/path"),
            PatternType::UrlOrGitProtocol
        );
    }
}
