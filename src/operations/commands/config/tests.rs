use super::parse::*;
use super::pattern::*;
use super::spec::{ConfigMutation, ConfigNesting, ConfigValueKind, config_key_specs, resolve_key};
use serde_json::Value;

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
    use crate::config::CodexHooksFormat;
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

#[test]
fn test_config_key_registry_resolves_canonical_names_and_aliases() {
    let canonical = resolve_key("allowed_repositories").unwrap();
    assert_eq!(canonical.spec.name, "allowed_repositories");
    assert_eq!(canonical.spec.nesting, ConfigNesting::None);

    let alias = resolve_key("allow_repositories").unwrap();
    assert_eq!(alias.spec.name, "allowed_repositories");

    // `telemetry_oss` is the persisted legacy key. The effective read name is
    // retained as an alias so the registry documents the compatibility bridge.
    let legacy = resolve_key("telemetry_oss_disabled").unwrap();
    assert_eq!(legacy.spec.name, "telemetry_oss");
}

#[test]
fn test_config_key_registry_records_sensitivity_and_mutation_metadata() {
    let api_key = resolve_key("api_key").unwrap();
    assert!(api_key.spec.sensitive);
    assert_eq!(api_key.spec.value_kind, ConfigValueKind::Secret);
    assert_eq!(api_key.spec.mutation, ConfigMutation::Replace);

    let feature_flag = resolve_key("feature_flags.transcript_sweep").unwrap();
    assert_eq!(feature_flag.spec.name, "feature_flags");
    assert_eq!(feature_flag.spec.nesting, ConfigNesting::Dynamic);
    assert_eq!(feature_flag.spec.mutation, ConfigMutation::ReplaceOrAdd);
}

#[test]
fn test_config_key_registry_rejects_unknown_roots() {
    let error = resolve_key("not_a_config_key").unwrap_err();
    assert!(error.contains("Unknown config key"));
    assert!(resolve_key("not_a_config_key.value").is_err());
}

#[test]
fn test_config_key_registry_help_entries_include_legacy_and_nested_keys() {
    let names = config_key_specs()
        .iter()
        .filter(|spec| spec.show_in_help)
        .map(|spec| spec.name)
        .collect::<Vec<_>>();

    assert!(names.contains(&"telemetry_oss"));
    assert!(names.contains(&"author.name"));
    assert!(names.contains(&"notes_backend.kind"));
    assert!(names.iter().all(|name| !name.is_empty()));
}
