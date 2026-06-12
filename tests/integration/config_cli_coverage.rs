//! Integration coverage for `git-ai config` keys that previously had no CLI
//! handling: `allow_superuser`, `transcript_streaming_lookback_days`, and
//! `custom_attributes` (including its nested `custom_attributes.<key>` form).
//!
//! These run the real binary against an isolated test HOME, so `config set`
//! writes land in the sandboxed `~/.git-ai/config.json` rather than the user's.

use crate::repos::test_repo::TestRepo;
use git_ai::config::{AuthorConfig, FileConfig, NotesBackendConfig};
use serde_json::Value;
use std::collections::HashMap;

/// Parse the JSON emitted by `git-ai config <key>` into a serde value.
fn get_json(repo: &TestRepo, key: &str) -> Value {
    let out = repo
        .git_ai(&["config", key])
        .unwrap_or_else(|e| panic!("config get {key} failed: {e}"));
    serde_json::from_str(out.trim())
        .unwrap_or_else(|e| panic!("config get {key} returned non-JSON {out:?}: {e}"))
}

#[test]
fn test_config_allow_superuser_set_get_unset() {
    let repo = TestRepo::new();

    // Default is false.
    assert_eq!(get_json(&repo, "allow_superuser"), Value::Bool(false));

    repo.git_ai(&["config", "set", "allow_superuser", "true"])
        .expect("set allow_superuser");
    assert_eq!(get_json(&repo, "allow_superuser"), Value::Bool(true));

    repo.git_ai(&["config", "unset", "allow_superuser"])
        .expect("unset allow_superuser");
    assert_eq!(get_json(&repo, "allow_superuser"), Value::Bool(false));
}

#[test]
fn test_config_transcript_streaming_lookback_days_set_get_unset() {
    let repo = TestRepo::new();

    // Default is 7 days.
    assert_eq!(
        get_json(&repo, "transcript_streaming_lookback_days"),
        Value::Number(7.into())
    );

    repo.git_ai(&["config", "set", "transcript_streaming_lookback_days", "1"])
        .expect("set lookback to 1");
    assert_eq!(
        get_json(&repo, "transcript_streaming_lookback_days"),
        Value::Number(1.into())
    );

    // 0 means unlimited; runtime normalizes to None and we surface it as 0.
    repo.git_ai(&["config", "set", "transcript_streaming_lookback_days", "0"])
        .expect("set lookback to 0");
    assert_eq!(
        get_json(&repo, "transcript_streaming_lookback_days"),
        Value::Number(0.into())
    );

    // Non-numeric input is rejected.
    assert!(
        repo.git_ai(&["config", "set", "transcript_streaming_lookback_days", "abc"])
            .is_err()
    );

    repo.git_ai(&["config", "unset", "transcript_streaming_lookback_days"])
        .expect("unset lookback");
    assert_eq!(
        get_json(&repo, "transcript_streaming_lookback_days"),
        Value::Number(7.into())
    );
}

#[test]
fn test_config_checkpoint_budget_set_get_unset() {
    let repo = TestRepo::new();

    repo.git_ai(&[
        "config",
        "set",
        "max_checkpoint_total_size_bytes",
        "1048576",
    ])
    .expect("set max_checkpoint_total_size_bytes");
    assert_eq!(
        get_json(&repo, "max_checkpoint_total_size_bytes"),
        Value::Number(1_048_576.into())
    );

    repo.git_ai(&["config", "set", "max_checkpoint_total_lines", "4096"])
        .expect("set max_checkpoint_total_lines");
    assert_eq!(
        get_json(&repo, "max_checkpoint_total_lines"),
        Value::Number(4096.into())
    );

    repo.git_ai(&["config", "unset", "max_checkpoint_total_size_bytes"])
        .expect("unset max_checkpoint_total_size_bytes");
    assert_eq!(
        get_json(&repo, "max_checkpoint_total_size_bytes"),
        Value::Number((32 * 1024 * 1024).into())
    );

    repo.git_ai(&["config", "unset", "max_checkpoint_total_lines"])
        .expect("unset max_checkpoint_total_lines");
    assert_eq!(
        get_json(&repo, "max_checkpoint_total_lines"),
        Value::Number(500_000.into())
    );
}

#[test]
fn test_config_custom_attributes_object_set_get_unset() {
    let repo = TestRepo::new();

    // Default is an empty object.
    assert_eq!(
        get_json(&repo, "custom_attributes"),
        Value::Object(serde_json::Map::new())
    );

    repo.git_ai(&[
        "config",
        "set",
        "custom_attributes",
        r#"{"team":"platform","env":"prod"}"#,
    ])
    .expect("set custom_attributes object");

    let value = get_json(&repo, "custom_attributes");
    assert_eq!(value["team"], Value::String("platform".to_string()));
    assert_eq!(value["env"], Value::String("prod".to_string()));

    repo.git_ai(&["config", "unset", "custom_attributes"])
        .expect("unset custom_attributes");
    assert_eq!(
        get_json(&repo, "custom_attributes"),
        Value::Object(serde_json::Map::new())
    );
}

#[test]
fn test_config_custom_attributes_nested_set_get_unset() {
    let repo = TestRepo::new();

    // Set a single attribute via dot notation.
    repo.git_ai(&["config", "set", "custom_attributes.team", "platform"])
        .expect("set custom_attributes.team");
    assert_eq!(
        get_json(&repo, "custom_attributes.team"),
        Value::String("platform".to_string())
    );

    // --add upserts another attribute without clobbering the first.
    repo.git_ai(&["config", "--add", "custom_attributes.env", "prod"])
        .expect("add custom_attributes.env");
    let value = get_json(&repo, "custom_attributes");
    assert_eq!(value["team"], Value::String("platform".to_string()));
    assert_eq!(value["env"], Value::String("prod".to_string()));

    // Unknown nested attribute reads back as null.
    assert_eq!(get_json(&repo, "custom_attributes.missing"), Value::Null);

    // Unset one attribute leaves the other intact.
    repo.git_ai(&["config", "unset", "custom_attributes.team"])
        .expect("unset custom_attributes.team");
    let value = get_json(&repo, "custom_attributes");
    assert!(value.get("team").is_none());
    assert_eq!(value["env"], Value::String("prod".to_string()));

    // Unsetting a missing attribute is an error.
    assert!(
        repo.git_ai(&["config", "unset", "custom_attributes.team"])
            .is_err()
    );
}

#[test]
fn test_config_custom_attributes_set_empty_object_is_omitted() {
    let repo = TestRepo::new();

    // Setting an empty object should normalize to "unset" (mirrors `author`),
    // not persist a redundant `{}`.
    repo.git_ai(&["config", "set", "custom_attributes", "{}"])
        .expect("set empty custom_attributes");
    assert_eq!(
        get_json(&repo, "custom_attributes"),
        Value::Object(serde_json::Map::new())
    );

    // The config file should not carry a `custom_attributes` key at all.
    let config_path = repo.test_home_path().join(".git-ai").join("config.json");
    if let Ok(contents) = std::fs::read_to_string(&config_path) {
        let parsed: Value = serde_json::from_str(&contents).unwrap_or(Value::Null);
        assert!(
            parsed.get("custom_attributes").is_none(),
            "empty custom_attributes should be omitted from config file, got: {contents}"
        );
    }
}

#[test]
fn test_config_custom_attributes_nested_unset_trims_name() {
    let repo = TestRepo::new();

    // Set with a leading space in the attribute name; the set path trims it.
    repo.git_ai(&["config", "set", "custom_attributes. team", "platform"])
        .expect("set custom_attributes. team");
    assert_eq!(
        get_json(&repo, "custom_attributes.team"),
        Value::String("platform".to_string())
    );

    // Get with the same (untrimmed) dotted key must return the stored value.
    assert_eq!(
        get_json(&repo, "custom_attributes. team"),
        Value::String("platform".to_string())
    );

    // Unset with the same (untrimmed) dotted key must succeed symmetrically.
    repo.git_ai(&["config", "unset", "custom_attributes. team"])
        .expect("unset custom_attributes. team should match trimmed name");
    assert_eq!(get_json(&repo, "custom_attributes.team"), Value::Null);
}

#[test]
fn test_config_show_all_includes_new_keys() {
    let repo = TestRepo::new();
    let out = repo.git_ai(&["config"]).expect("show all config");
    let value: Value =
        serde_json::from_str(out.trim()).expect("config show-all should emit valid JSON");

    assert!(value.get("allow_superuser").is_some());
    assert!(value.get("transcript_streaming_lookback_days").is_some());
    assert!(value.get("max_checkpoint_total_size_bytes").is_some());
    assert!(value.get("max_checkpoint_total_lines").is_some());
    assert!(value.get("custom_attributes").is_some());
}

/// Map a `FileConfig` field name to the CLI key used to read it back, when the
/// two differ. Most fields share a name with their CLI key; the exceptions are
/// enumerated here so the divergence stays explicit and reviewed.
fn cli_key_for_field(field: &str) -> &str {
    match field {
        // `telemetry_oss` is written by the file/CLI but read back as the
        // effective `telemetry_oss_disabled` boolean.
        "telemetry_oss" => "telemetry_oss_disabled",
        other => other,
    }
}

/// Fields that are intentionally mutated only via dot-notation subkeys
/// (e.g. `notes_backend.kind`) and have no bare top-level `set`/`unset` arm.
/// They remain fully readable via `config <field>` and show-all; only the
/// mutation handlers are nested-only. Listed explicitly so the exception is
/// reviewed rather than silently assumed.
fn is_nested_only_for_mutation(field: &str) -> bool {
    matches!(field, "notes_backend")
}

/// Build a `FileConfig` with every field populated so serde emits all of them
/// (Optional fields skip when `None`). This is the source-of-truth field list
/// for the coverage guard below.
fn fully_populated_file_config() -> FileConfig {
    let mut custom_attributes = HashMap::new();
    custom_attributes.insert("k".to_string(), "v".to_string());
    let mut git_ai_hooks = HashMap::new();
    git_ai_hooks.insert(
        "post_notes_updated".to_string(),
        vec!["./hook.sh".to_string()],
    );

    FileConfig {
        git_path: Some("git".to_string()),
        exclude_prompts_in_repositories: Some(vec!["*".to_string()]),
        include_prompts_in_repositories: Some(vec!["*".to_string()]),
        allow_repositories: Some(vec!["*".to_string()]),
        exclude_repositories: Some(vec!["*".to_string()]),
        telemetry_oss: Some("off".to_string()),
        telemetry_enterprise_dsn: Some("https://example.com".to_string()),
        disable_version_checks: Some(true),
        disable_auto_updates: Some(true),
        update_channel: Some("latest".to_string()),
        upgrade_jitter_seconds: Some(345_600),
        minimum_package_upgrade_age_seconds: Some(86_400),
        feature_flags: Some(serde_json::json!({"transcript_sweep": true})),
        api_base_url: Some("https://usegitai.com".to_string()),
        prompt_storage: Some("default".to_string()),
        default_prompt_storage: Some("local".to_string()),
        api_key: Some("secret-key".to_string()),
        quiet: Some(true),
        allow_superuser: Some(true),
        author: Some(AuthorConfig {
            name: Some("Alice".to_string()),
            email: Some("alice@example.com".to_string()),
        }),
        custom_attributes: Some(custom_attributes),
        git_ai_hooks: Some(git_ai_hooks),
        codex_hooks_format: Some("config_toml".to_string()),
        notes_backend: Some(NotesBackendConfig::default()),
        transcript_streaming_lookback_days: Some(7),
        max_checkpoint_file_size_bytes: Some(3 * 1024 * 1024),
        max_checkpoint_total_size_bytes: Some(32 * 1024 * 1024),
        max_checkpoint_total_lines: Some(500_000),
    }
}

fn file_config_field_names() -> Vec<String> {
    let serialized =
        serde_json::to_value(fully_populated_file_config()).expect("serialize FileConfig");
    serialized
        .as_object()
        .expect("FileConfig serializes to an object")
        .keys()
        .cloned()
        .collect()
}

/// Regression guard: every persisted `FileConfig` field must be reachable through
/// the `git-ai config` CLI read path (`config <key>`).
///
/// The field list is derived from a fully-populated `FileConfig` via serde rather
/// than hardcoded, so adding a new persisted field WILL break this test until the
/// CLI handlers (and this guard's expectations) are updated. That is the point:
/// CLI read coverage cannot silently regress.
///
/// `get_config_value` has a top-level match arm for every field plus a catch-all
/// that returns "Unknown config key", making it the canonical completeness check:
/// it is the one read path that must handle every field as a bare key (show-all
/// hides unset optionals; mutation handlers are nested-only for some fields).
#[test]
fn test_every_file_config_field_has_cli_get_coverage() {
    let fields = file_config_field_names();

    // Sanity: serde actually emitted every field (none silently skipped).
    assert!(
        fields.len() >= 23,
        "expected all FileConfig fields to serialize, got {}: {:?}",
        fields.len(),
        fields
    );

    let repo = TestRepo::new();
    let mut unknown_to_get = Vec::new();

    for field in &fields {
        let get_key = cli_key_for_field(field);
        // We tolerate any success output; we only fail on the explicit
        // "Unknown config key" rejection produced by the get catch-all.
        match repo.git_ai(&["config", get_key]) {
            Ok(_) => {}
            Err(e) if e.contains("Unknown config key") => {
                unknown_to_get.push(format!("{field} (cli key: {get_key}): {e}"));
            }
            // Other errors (e.g. environment-specific) are not coverage gaps.
            Err(_) => {}
        }
    }

    assert!(
        unknown_to_get.is_empty(),
        "FileConfig fields rejected by `git-ai config <key>` as unknown \
         (add them to get_config_value): {unknown_to_get:?}"
    );
}

/// Regression guard: every persisted `FileConfig` field must be mutable through
/// the `git-ai config unset <key>` write path, except fields documented as
/// nested-only (see `is_nested_only_for_mutation`).
///
/// `unset` needs no value and is non-destructive against the isolated test
/// config, so it cleanly exercises the write handler's top-level coverage. A new
/// field added without a `set`/`unset` arm trips the catch-all here.
#[test]
fn test_every_file_config_field_has_cli_unset_coverage() {
    let repo = TestRepo::new();
    let mut unknown_to_unset = Vec::new();

    for field in &file_config_field_names() {
        if is_nested_only_for_mutation(field) {
            continue;
        }
        match repo.git_ai(&["config", "unset", field]) {
            Ok(_) => {}
            Err(e) if e.contains("Unknown config key") => {
                unknown_to_unset.push(format!("{field}: {e}"));
            }
            Err(_) => {}
        }
    }

    assert!(
        unknown_to_unset.is_empty(),
        "FileConfig fields rejected by `git-ai config unset <key>` as unknown \
         (add them to set_config_value and unset_config_value, or document them \
         in is_nested_only_for_mutation): {unknown_to_unset:?}"
    );
}
