use std::collections::HashMap;
use std::fs;
use std::path::Path;

use glob::Pattern;

use crate::feature_flags::FeatureFlags;

use super::author::AuthorConfig;
use super::file::CodexHooksFormat;
use super::file::{
    ConfigPatch, FileConfig, UpdateChannel, build_config, parse_file_config_bytes,
    resolve_telemetry_enabled,
};
use super::notes_backend::{NotesBackendConfig, NotesBackendKind};
use super::patterns::remote_matches_patterns;
use super::prompt_storage::PromptStorageMode;
use super::{
    Config, DEFAULT_API_BASE_URL, DEFAULT_MAX_CHECKPOINT_FILE_SIZE_BYTES,
    DEFAULT_MAX_CHECKPOINT_TOTAL_LINES, DEFAULT_MAX_CHECKPOINT_TOTAL_SIZE_BYTES,
    author_config_file_fingerprint,
};

pub(crate) fn create_test_config(
    allowed_repositories: Vec<String>,
    exclude_repositories: Vec<String>,
) -> Config {
    Config {
        git_path: "/usr/bin/git".to_string(),
        exclude_prompts_in_repositories: vec![],
        include_prompts_in_repositories: vec![],
        allowed_repositories: allowed_repositories
            .into_iter()
            .filter_map(|s| Pattern::new(&s).ok())
            .collect(),
        exclude_repositories: exclude_repositories
            .into_iter()
            .filter_map(|s| Pattern::new(&s).ok())
            .collect(),
        telemetry_enabled: false,
        telemetry_oss_disabled: false,
        telemetry_enterprise_dsn: None,
        disable_version_checks: false,
        disable_auto_updates: false,
        update_channel: UpdateChannel::Latest,
        feature_flags: FeatureFlags::default(),
        api_base_url: DEFAULT_API_BASE_URL.to_string(),
        prompt_storage: "default".to_string(),
        default_prompt_storage: None,
        api_key: None,
        quiet: false,
        allow_superuser: false,
        author: AuthorConfig::default(),
        custom_attributes: HashMap::new(),
        git_ai_hooks: HashMap::new(),
        codex_hooks_format: CodexHooksFormat::ConfigToml,
        notes_backend: NotesBackendConfig::default(),
        transcript_streaming_lookback_days: Some(7),
        max_checkpoint_file_size_bytes: DEFAULT_MAX_CHECKPOINT_FILE_SIZE_BYTES,
        max_checkpoint_total_size_bytes: DEFAULT_MAX_CHECKPOINT_TOTAL_SIZE_BYTES,
        max_checkpoint_total_lines: DEFAULT_MAX_CHECKPOINT_TOTAL_LINES,
        daemon_memory_limit_mb: None,
    }
}

#[test]
fn test_author_config_normalizes_empty_fields() {
    let author = AuthorConfig {
        name: Some("  Alice  ".to_string()),
        email: Some("   ".to_string()),
    }
    .normalized();

    assert_eq!(author.name.as_deref(), Some("Alice"));
    assert!(author.email.is_none());
    assert!(!author.is_empty());
}

#[test]
fn test_author_config_empty_when_all_fields_blank() {
    let author = AuthorConfig {
        name: Some("".to_string()),
        email: Some("   ".to_string()),
    }
    .normalized();

    assert!(author.is_empty());
}

#[test]
fn test_author_config_file_fingerprint_detects_same_length_edits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    fs::write(&path, br#"{"author":{"name":"Alice"}}"#).unwrap();
    let first = author_config_file_fingerprint(&path).unwrap();

    fs::write(&path, br#"{"author":{"name":"Carol"}}"#).unwrap();
    let second = author_config_file_fingerprint(&path).unwrap();

    assert_eq!(first.len, second.len);
    assert_ne!(first, second);
}

#[test]
fn test_exclusion_takes_precedence_over_allow() {
    let config = create_test_config(
        vec!["https://github.com/allowed/repo".to_string()],
        vec!["https://github.com/allowed/repo".to_string()],
    );

    let remotes = vec![(
        "origin".to_string(),
        "https://github.com/allowed/repo".to_string(),
    )];
    assert!(!config.is_allowed_repository_with_context(Some(&remotes), None));
}

#[test]
fn test_empty_allowlist_denies_everything() {
    let config = create_test_config(vec![], vec![]);

    // Collection is opt-in: an empty allowlist denies everything.
    assert!(!config.is_allowed_repository_with_context(None, None));
    let remotes = vec![(
        "origin".to_string(),
        "https://github.com/any/repo".to_string(),
    )];
    assert!(!config.is_allowed_repository_with_context(Some(&remotes), None));
    assert!(!config.is_allowed_repository_with_context(None, Some(Path::new("/work/some-repo"))));
}

#[test]
fn test_exclude_without_allow_still_denies() {
    let config = create_test_config(vec![], vec!["https://github.com/excluded/repo".to_string()]);

    // Exclusions do not turn on collection: the allowlist is still empty.
    assert!(!config.is_allowed_repository_with_context(None, None));
    let remotes = vec![(
        "origin".to_string(),
        "https://github.com/unrelated/repo".to_string(),
    )];
    assert!(!config.is_allowed_repository_with_context(Some(&remotes), None));
}

#[test]
fn test_allow_without_exclude() {
    let config = create_test_config(vec!["https://github.com/allowed/repo".to_string()], vec![]);

    // With an allowlist but no repository context, deny.
    assert!(!config.is_allowed_repository_with_context(None, None));

    let allowed_remotes = vec![(
        "origin".to_string(),
        "https://github.com/allowed/repo".to_string(),
    )];
    assert!(config.is_allowed_repository_with_context(Some(&allowed_remotes), None));

    let other_remotes = vec![(
        "origin".to_string(),
        "https://github.com/other/repo".to_string(),
    )];
    assert!(!config.is_allowed_repository_with_context(Some(&other_remotes), None));
}

#[test]
fn test_allowlist_matches_repo_root_paths() {
    let config = create_test_config(vec!["/work/repos".to_string()], vec![]);

    // Exact directory entry allows the repo itself and everything beneath it.
    assert!(config.is_allowed_repository_with_context(None, Some(Path::new("/work/repos"))));
    assert!(
        config.is_allowed_repository_with_context(None, Some(Path::new("/work/repos/project")))
    );
    assert!(config.is_allowed_repository_with_context(
        None,
        Some(Path::new("/work/repos/nested/deep/project"))
    ));
    assert!(!config.is_allowed_repository_with_context(None, Some(Path::new("/work/other"))));
    assert!(
        !config
            .is_allowed_repository_with_context(None, Some(Path::new("/work/repos-other/project")))
    );
}

#[test]
fn test_allowlist_matches_repo_root_glob() {
    let config = create_test_config(vec!["/home/*/projects".to_string()], vec![]);

    assert!(config.is_allowed_repository_with_context(None, Some(Path::new("/home/dev/projects"))));
    assert!(
        config.is_allowed_repository_with_context(None, Some(Path::new("/home/dev/projects/repo")))
    );
    assert!(!config.is_allowed_repository_with_context(None, Some(Path::new("/home/dev/src"))));
}

#[test]
fn test_allowlist_matches_windows_style_path_entries() {
    // Entries written with backslashes are normalized before matching.
    let config = create_test_config(vec!["C:\\Users\\dev\\work".to_string()], vec![]);

    assert!(
        config.is_allowed_repository_with_context(None, Some(Path::new("C:/Users/dev/work/repo")))
    );
    assert!(
        !config.is_allowed_repository_with_context(None, Some(Path::new("C:/Users/dev/other")))
    );
}

#[test]
fn test_exclusion_matches_repo_root_paths() {
    let config = create_test_config(vec!["/work".to_string()], vec!["/work/secret".to_string()]);

    assert!(config.is_allowed_repository_with_context(None, Some(Path::new("/work/repo"))));
    assert!(!config.is_allowed_repository_with_context(None, Some(Path::new("/work/secret/repo"))));
}

#[test]
fn test_remote_allowlist_ignores_unrelated_repo_root() {
    let config = create_test_config(vec!["https://github.com/org/*".to_string()], vec![]);

    // A repo root that matches nothing does not defeat a remote match.
    let remotes = vec![(
        "origin".to_string(),
        "https://github.com/org/repo".to_string(),
    )];
    assert!(
        config.is_allowed_repository_with_context(Some(&remotes), Some(Path::new("/tmp/clone")))
    );
    // And a path-only repo does not match a URL-only allowlist.
    assert!(!config.is_allowed_repository_with_context(None, Some(Path::new("/tmp/clone"))));
}

#[test]
fn test_glob_pattern_wildcard_in_allow() {
    let config = create_test_config(vec!["https://github.com/myorg/*".to_string()], vec![]);

    // Test that the pattern would match (note: we can't easily test with real Repository objects,
    // but the pattern compilation is tested by the fact that create_test_config succeeds)
    assert!(!config.allowed_repositories.is_empty());
    assert!(config.allowed_repositories[0].matches("https://github.com/myorg/repo1"));
    assert!(config.allowed_repositories[0].matches("https://github.com/myorg/repo2"));
    assert!(!config.allowed_repositories[0].matches("https://github.com/other/repo"));
}

#[test]
fn test_glob_pattern_wildcard_in_exclude() {
    let config = create_test_config(vec![], vec!["https://github.com/private/*".to_string()]);

    // Test pattern matching
    assert!(!config.exclude_repositories.is_empty());
    assert!(config.exclude_repositories[0].matches("https://github.com/private/repo1"));
    assert!(config.exclude_repositories[0].matches("https://github.com/private/secret"));
    assert!(!config.exclude_repositories[0].matches("https://github.com/public/repo"));
}

#[test]
fn test_exact_match_still_works() {
    let config = create_test_config(vec!["https://github.com/exact/match".to_string()], vec![]);

    // Test that exact matches still work (glob treats them as literals)
    assert!(!config.allowed_repositories.is_empty());
    assert!(config.allowed_repositories[0].matches("https://github.com/exact/match"));
    assert!(!config.allowed_repositories[0].matches("https://github.com/exact/other"));
}

#[test]
fn test_complex_glob_patterns() {
    let config = create_test_config(vec!["*@github.com:company/*".to_string()], vec![]);

    // Test more complex patterns with wildcards
    assert!(!config.allowed_repositories.is_empty());
    assert!(config.allowed_repositories[0].matches("git@github.com:company/repo"));
    assert!(config.allowed_repositories[0].matches("user@github.com:company/project"));
    assert!(!config.allowed_repositories[0].matches("git@github.com:other/repo"));
}

#[test]
fn test_remote_pattern_matching_normalizes_common_git_url_forms() {
    let scp_patterns = vec![Pattern::new("git@github.com:company/*").unwrap()];
    assert!(remote_matches_patterns(
        &scp_patterns,
        "ssh://git@github.com/company/repo"
    ));
    assert!(remote_matches_patterns(
        &scp_patterns,
        "ssh://git@github.com:22/company/repo"
    ));
    assert!(!remote_matches_patterns(
        &scp_patterns,
        "ssh://git@github.com/other/repo"
    ));
    assert!(remote_matches_patterns(
        &scp_patterns,
        "https://github.com/company/repo"
    ));
    assert!(remote_matches_patterns(
        &scp_patterns,
        "git://github.com/company/repo.git"
    ));

    let ssh_patterns = vec![Pattern::new("ssh://git@github.com/company/*").unwrap()];
    assert!(remote_matches_patterns(
        &ssh_patterns,
        "git@github.com:company/repo"
    ));
    assert!(remote_matches_patterns(
        &ssh_patterns,
        "https://github.com/company/repo.git"
    ));

    let ssh_port_patterns = vec![Pattern::new("ssh://git@github.com:2222/company/*").unwrap()];
    assert!(remote_matches_patterns(
        &ssh_port_patterns,
        "git@github.com:company/repo"
    ));
    assert!(remote_matches_patterns(
        &ssh_port_patterns,
        "ssh://git@github.com:2022/company/repo"
    ));

    let https_patterns = vec![Pattern::new("https://github.com/company/*").unwrap()];
    assert!(remote_matches_patterns(
        &https_patterns,
        "ssh://git@github.com:2022/company/repo"
    ));
    assert!(remote_matches_patterns(
        &https_patterns,
        "git@github.com:company/repo.git"
    ));
}

#[test]
fn test_remote_pattern_matching_allows_hostless_repository_patterns() {
    let patterns = vec![Pattern::new("company/*").unwrap()];

    assert!(remote_matches_patterns(
        &patterns,
        "https://github.com/company/repo"
    ));
    assert!(remote_matches_patterns(
        &patterns,
        "git@gitlab.com:company/repo.git"
    ));
    assert!(!remote_matches_patterns(
        &patterns,
        "https://github.com/other/repo"
    ));
}

#[test]
fn test_remote_pattern_matching_handles_azure_https_and_ssh_shape_difference() {
    let https_patterns = vec![Pattern::new("https://dev.azure.com/acme/widgets/_git/*").unwrap()];
    assert!(remote_matches_patterns(
        &https_patterns,
        "ssh://git@ssh.dev.azure.com:22/v3/acme/widgets/service"
    ));

    let ssh_patterns = vec![Pattern::new("ssh://git@ssh.dev.azure.com/v3/acme/widgets/*").unwrap()];
    assert!(remote_matches_patterns(
        &ssh_patterns,
        "https://dev.azure.com/acme/widgets/_git/service"
    ));
}

// Tests for exclude_prompts_in_repositories (blacklist)

fn create_test_config_with_exclude_prompts(exclude_prompts_patterns: Vec<String>) -> Config {
    Config {
        exclude_prompts_in_repositories: exclude_prompts_patterns
            .into_iter()
            .filter_map(|s| Pattern::new(&s).ok())
            .collect(),
        ..create_test_config(vec![], vec![])
    }
}

#[test]
fn test_should_exclude_prompts_empty_patterns_returns_false() {
    let config = create_test_config_with_exclude_prompts(vec![]);

    // Empty patterns = share everywhere (blacklist model)
    assert!(!config.should_exclude_prompts(&None));
}

#[test]
fn test_should_exclude_prompts_no_repository_returns_false() {
    let config = create_test_config_with_exclude_prompts(vec!["https://github.com/*".to_string()]);

    // Even with patterns, no repository provided = don't exclude (can't verify)
    assert!(!config.should_exclude_prompts(&None));
}

#[test]
fn test_should_exclude_prompts_pattern_matching() {
    let config =
        create_test_config_with_exclude_prompts(vec!["https://github.com/myorg/*".to_string()]);

    // Test that pattern is compiled correctly
    assert!(!config.exclude_prompts_in_repositories.is_empty());
    assert!(config.exclude_prompts_in_repositories[0].matches("https://github.com/myorg/repo1"));
    assert!(config.exclude_prompts_in_repositories[0].matches("https://github.com/myorg/repo2"));
    assert!(!config.exclude_prompts_in_repositories[0].matches("https://github.com/other/repo"));
}

#[test]
fn test_should_exclude_prompts_wildcard_all() {
    let config = create_test_config_with_exclude_prompts(vec!["*".to_string()]);

    // Wildcard * should match any remote URL pattern (exclude all)
    assert!(!config.exclude_prompts_in_repositories.is_empty());
    assert!(config.exclude_prompts_in_repositories[0].matches("https://github.com/any/repo"));
    assert!(config.exclude_prompts_in_repositories[0].matches("git@gitlab.com:any/project"));

    // Wildcard * should also exclude repos without remotes (None case)
    assert!(config.should_exclude_prompts(&None));
}

#[test]
fn test_debug_self_check_remote_bypasses_prompt_exclusion_wildcard() {
    let config = create_test_config_with_exclude_prompts(vec!["*".to_string()]);
    let remotes = vec![(
        "origin".to_string(),
        crate::diagnostic_sentinels::DEBUG_SELF_CHECK_REMOTE_URL.to_string(),
    )];

    assert!(!config.should_exclude_prompts_with_remotes(Some(&remotes)));
}

#[test]
fn test_should_exclude_prompts_local_repo_not_excluded_without_wildcard() {
    // Test 1: Local repo with no patterns configured - never excluded
    let config_no_patterns = create_test_config_with_exclude_prompts(vec![]);
    assert!(!config_no_patterns.should_exclude_prompts(&None));

    // Test 2: Local repo with non-wildcard patterns - not excluded
    // (patterns only match against remotes, local repos have none)
    let config_with_patterns =
        create_test_config_with_exclude_prompts(vec!["https://github.com/*".to_string()]);
    assert!(
        config_with_patterns.exclude_prompts_in_repositories[0]
            .matches("https://github.com/myorg/repo")
    );
    // Non-wildcard patterns should NOT exclude repos without remotes
    assert!(!config_with_patterns.should_exclude_prompts(&None));
}

#[test]
fn test_should_exclude_prompts_respects_patterns_when_remotes_exist() {
    let config =
        create_test_config_with_exclude_prompts(vec!["https://github.com/private/*".to_string()]);

    // Pattern should match private repos (to exclude)
    assert!(config.exclude_prompts_in_repositories[0].matches("https://github.com/private/repo"));
    // Pattern should not match other repos
    assert!(!config.exclude_prompts_in_repositories[0].matches("https://github.com/public/repo"));
}

#[test]
fn test_exclude_prompt_patterns_match_ssh_equivalent_remotes() {
    let config =
        create_test_config_with_exclude_prompts(vec!["git@github.com:private/*".to_string()]);

    assert!(remote_matches_patterns(
        &config.exclude_prompts_in_repositories,
        "ssh://git@github.com/private/repo"
    ));
}

// Tests for effective_prompt_storage() with include_prompts_in_repositories

fn create_test_config_with_include_prompts(
    include_patterns: Vec<String>,
    exclude_patterns: Vec<String>,
    prompt_storage: &str,
    default_prompt_storage: Option<&str>,
) -> Config {
    Config {
        exclude_prompts_in_repositories: exclude_patterns
            .into_iter()
            .filter_map(|s| Pattern::new(&s).ok())
            .collect(),
        include_prompts_in_repositories: include_patterns
            .into_iter()
            .filter_map(|s| Pattern::new(&s).ok())
            .collect(),
        prompt_storage: prompt_storage.to_string(),
        default_prompt_storage: default_prompt_storage.map(|s| s.to_string()),
        ..create_test_config(vec![], vec![])
    }
}

#[test]
fn test_effective_prompt_storage_no_include_list_uses_global() {
    // No include list = legacy behavior, use global prompt_storage
    let config = create_test_config_with_include_prompts(vec![], vec![], "notes", None);
    assert_eq!(
        config.effective_prompt_storage(&None),
        PromptStorageMode::Notes
    );

    let config = create_test_config_with_include_prompts(vec![], vec![], "local", None);
    assert_eq!(
        config.effective_prompt_storage(&None),
        PromptStorageMode::Local
    );

    let config = create_test_config_with_include_prompts(vec![], vec![], "default", None);
    assert_eq!(
        config.effective_prompt_storage(&None),
        PromptStorageMode::Default
    );
}

#[test]
fn test_effective_prompt_storage_exclude_always_wins() {
    // Exclusion with wildcard should always return Local, regardless of include list
    let config = create_test_config_with_include_prompts(
        vec!["https://github.com/work/*".to_string()],
        vec!["*".to_string()], // Exclude everything
        "default",
        Some("notes"),
    );
    assert_eq!(
        config.effective_prompt_storage(&None),
        PromptStorageMode::Local
    );
}

#[test]
fn test_effective_prompt_storage_wildcard_include_matches_no_repo() {
    // Wildcard include should match repos without remotes (None case)
    let config = create_test_config_with_include_prompts(
        vec!["*".to_string()],
        vec![],
        "default",
        Some("notes"),
    );
    // With wildcard include and None repo, should use prompt_storage (not fallback)
    assert_eq!(
        config.effective_prompt_storage(&None),
        PromptStorageMode::Default
    );
}

#[test]
fn test_effective_prompt_storage_non_wildcard_include_no_match_uses_fallback() {
    // Non-wildcard include with None repo = no match, use fallback
    let config = create_test_config_with_include_prompts(
        vec!["https://github.com/work/*".to_string()],
        vec![],
        "default",
        Some("notes"),
    );
    // None repo can't match non-wildcard pattern, should use default_prompt_storage
    assert_eq!(
        config.effective_prompt_storage(&None),
        PromptStorageMode::Notes
    );
}

#[test]
fn test_effective_prompt_storage_no_fallback_defaults_to_local() {
    // Non-wildcard include with None repo and no fallback = Local
    let config = create_test_config_with_include_prompts(
        vec!["https://github.com/work/*".to_string()],
        vec![],
        "default",
        None, // No fallback configured
    );
    // None repo can't match, and no fallback, should default to Local
    assert_eq!(
        config.effective_prompt_storage(&None),
        PromptStorageMode::Local
    );
}

#[test]
fn test_effective_prompt_storage_include_pattern_matching() {
    let config = create_test_config_with_include_prompts(
        vec!["https://github.com/positron-ai/*".to_string()],
        vec![],
        "default",
        Some("notes"),
    );

    // Test that patterns are compiled correctly
    assert!(!config.include_prompts_in_repositories.is_empty());
    assert!(
        config.include_prompts_in_repositories[0].matches("https://github.com/positron-ai/repo1")
    );
    assert!(
        config.include_prompts_in_repositories[0].matches("https://github.com/positron-ai/project")
    );
    assert!(
        !config.include_prompts_in_repositories[0].matches("https://github.com/other-org/repo")
    );
}

#[test]
fn test_include_prompt_patterns_match_ssh_equivalent_remotes() {
    let config = create_test_config_with_include_prompts(
        vec!["ssh://git@github.com/positron-ai/*".to_string()],
        vec![],
        "default",
        Some("notes"),
    );

    assert!(remote_matches_patterns(
        &config.include_prompts_in_repositories,
        "git@github.com:positron-ai/repo"
    ));
}

#[test]
fn test_prompt_storage_mode_from_str() {
    assert_eq!(
        "default".parse::<PromptStorageMode>().ok(),
        Some(PromptStorageMode::Default)
    );
    assert_eq!(
        "DEFAULT".parse::<PromptStorageMode>().ok(),
        Some(PromptStorageMode::Default)
    );
    assert_eq!(
        "notes".parse::<PromptStorageMode>().ok(),
        Some(PromptStorageMode::Notes)
    );
    assert_eq!(
        "NOTES".parse::<PromptStorageMode>().ok(),
        Some(PromptStorageMode::Notes)
    );
    assert_eq!(
        "local".parse::<PromptStorageMode>().ok(),
        Some(PromptStorageMode::Local)
    );
    assert_eq!(
        "LOCAL".parse::<PromptStorageMode>().ok(),
        Some(PromptStorageMode::Local)
    );
    assert_eq!("invalid".parse::<PromptStorageMode>().ok(), None);
    assert_eq!("".parse::<PromptStorageMode>().ok(), None);
}

#[test]
fn test_prompt_storage_mode_as_str() {
    assert_eq!(PromptStorageMode::Default.as_str(), "default");
    assert_eq!(PromptStorageMode::Notes.as_str(), "notes");
    assert_eq!(PromptStorageMode::Local.as_str(), "local");
}

#[test]
fn test_update_channel_default_is_latest() {
    let channel = UpdateChannel::default();
    assert_eq!(channel, UpdateChannel::Latest);
    assert_eq!(channel.as_str(), "latest");
}

#[test]
fn test_update_channel_enterprise_latest_maps_to_enterprise_latest() {
    let channel = UpdateChannel::from_str("enterprise-latest").unwrap();
    assert_eq!(channel, UpdateChannel::EnterpriseLatest);
    assert_eq!(channel.as_str(), "enterprise-latest");
}

#[test]
fn test_update_channel_enterprise_next_maps_to_enterprise_next() {
    let channel = UpdateChannel::from_str("enterprise-next").unwrap();
    assert_eq!(channel, UpdateChannel::EnterpriseNext);
    assert_eq!(channel.as_str(), "enterprise-next");
}

#[test]
fn test_update_channel_enterprise_latest_parses() {
    let channel = UpdateChannel::from_str("enterprise-latest").unwrap();
    assert_eq!(channel, UpdateChannel::EnterpriseLatest);
    assert_eq!(channel.as_str(), "enterprise-latest");
}

#[test]
fn test_update_channel_enterprise_next_parses() {
    let channel = UpdateChannel::from_str("enterprise-next").unwrap();
    assert_eq!(channel, UpdateChannel::EnterpriseNext);
    assert_eq!(channel.as_str(), "enterprise-next");
}

#[test]
fn test_quiet_default_is_false() {
    let config = create_test_config(vec![], vec![]);
    assert!(!config.is_quiet());
}

#[test]
fn test_quiet_can_be_enabled() {
    let mut config = create_test_config(vec![], vec![]);
    config.quiet = true;
    assert!(config.is_quiet());
}

#[test]
fn test_excluded_repo_with_remotes() {
    let config = create_test_config(vec![], vec!["https://github.com/excluded/*".to_string()]);
    let remotes = vec![(
        "origin".to_string(),
        "https://github.com/excluded/repo".to_string(),
    )];
    assert!(!config.is_allowed_repository_with_context(Some(&remotes), None));
}

#[test]
fn test_allowed_repo_not_excluded_with_remotes() {
    let config = create_test_config(
        vec!["https://github.com/allowed/*".to_string()],
        vec!["https://github.com/excluded/*".to_string()],
    );
    let remotes = vec![(
        "origin".to_string(),
        "https://github.com/allowed/repo".to_string(),
    )];
    assert!(config.is_allowed_repository_with_context(Some(&remotes), None));
}

#[test]
fn test_allowlist_with_remotes() {
    let config = create_test_config(vec!["https://github.com/myorg/*".to_string()], vec![]);
    let remotes = vec![(
        "origin".to_string(),
        "https://github.com/myorg/project".to_string(),
    )];
    assert!(config.is_allowed_repository_with_context(Some(&remotes), None));
}

#[test]
fn test_allowlist_matches_ssh_url_remote_with_scp_pattern() {
    let config = create_test_config(vec!["git@github.com:myorg/*".to_string()], vec![]);
    let remotes = vec![(
        "origin".to_string(),
        "ssh://git@github.com/myorg/project".to_string(),
    )];
    assert!(config.is_allowed_repository_with_context(Some(&remotes), None));
}

#[test]
fn test_allowlist_denies_unmatched_remotes() {
    let config = create_test_config(vec!["https://github.com/myorg/*".to_string()], vec![]);
    let remotes = vec![(
        "origin".to_string(),
        "https://github.com/other/project".to_string(),
    )];
    assert!(!config.is_allowed_repository_with_context(Some(&remotes), None));
}

#[test]
fn test_exclusion_takes_precedence_with_remotes() {
    let config = create_test_config(
        vec!["https://github.com/myorg/*".to_string()],
        vec!["https://github.com/myorg/secret".to_string()],
    );
    let remotes = vec![(
        "origin".to_string(),
        "https://github.com/myorg/secret".to_string(),
    )];
    assert!(!config.is_allowed_repository_with_context(Some(&remotes), None));
}

#[test]
fn test_exclusion_matches_scp_remote_with_ssh_url_pattern() {
    let config = create_test_config(
        vec!["git@github.com:excluded/*".to_string()],
        vec!["ssh://git@github.com/excluded/*".to_string()],
    );
    let remotes = vec![(
        "origin".to_string(),
        "git@github.com:excluded/repo".to_string(),
    )];
    // The remote matches the allowlist, but the normalized exclusion wins.
    assert!(!config.is_allowed_repository_with_context(Some(&remotes), None));
}

#[test]
fn test_no_remotes_denied_with_empty_allowlist() {
    let config = create_test_config(vec![], vec!["https://github.com/excluded/*".to_string()]);
    assert!(!config.is_allowed_repository_with_context(None, None));
}

#[test]
fn test_no_remotes_denied_when_allowlist_active() {
    let config = create_test_config(vec!["https://github.com/myorg/*".to_string()], vec![]);
    assert!(!config.is_allowed_repository_with_context(None, None));
}

#[test]
fn test_empty_remotes_treated_as_no_match_for_exclusion() {
    let config = create_test_config(
        vec!["/work".to_string()],
        vec!["https://github.com/excluded/*".to_string()],
    );
    let remotes: Vec<(String, String)> = vec![];
    // No remotes to exclude; the repo root still satisfies the allowlist.
    assert!(
        config.is_allowed_repository_with_context(Some(&remotes), Some(Path::new("/work/repo")))
    );
}

#[test]
fn test_multiple_remotes_one_excluded() {
    let config = create_test_config(
        vec!["https://github.com/allowed/*".to_string()],
        vec!["https://github.com/excluded/*".to_string()],
    );
    let remotes = vec![
        (
            "origin".to_string(),
            "https://github.com/allowed/repo".to_string(),
        ),
        (
            "upstream".to_string(),
            "https://github.com/excluded/repo".to_string(),
        ),
    ];
    assert!(!config.is_allowed_repository_with_context(Some(&remotes), None));
}

#[test]
fn test_parse_file_config_bytes_accepts_utf8_bom() {
    let mut data = vec![0xEF, 0xBB, 0xBF];
    data.extend_from_slice(br#"{"git_path":"C:\\Program Files\\Git\\cmd\\git.exe"}"#);

    let parsed = parse_file_config_bytes(&data).expect("BOM-prefixed config should parse");
    assert_eq!(
        parsed.git_path.as_deref(),
        Some(r"C:\Program Files\Git\cmd\git.exe")
    );
}

#[test]
fn test_parse_file_config_bytes_without_bom_still_parses() {
    let data = br#"{"git_path":"/usr/bin/git"}"#;

    let parsed = parse_file_config_bytes(data).expect("regular config should parse");
    assert_eq!(parsed.git_path.as_deref(), Some("/usr/bin/git"));
}

#[test]
fn test_telemetry_resolution_defaults_off() {
    assert!(!resolve_telemetry_enabled(None, None));
    assert!(resolve_telemetry_enabled(Some("on"), None));
    assert!(!resolve_telemetry_enabled(Some("off"), None));
    assert!(!resolve_telemetry_enabled(Some("bogus"), None));
    // Legacy key: only an explicit "on" enables; absence or "off" stays off.
    assert!(resolve_telemetry_enabled(None, Some("on")));
    assert!(!resolve_telemetry_enabled(None, Some("off")));
    // The new key wins over the legacy key.
    assert!(!resolve_telemetry_enabled(Some("off"), Some("on")));
    assert!(resolve_telemetry_enabled(Some("on"), Some("off")));
}

#[test]
fn test_file_config_accepts_legacy_allow_repositories_key() {
    let data = br#"{"allow_repositories":["https://github.com/org/*"]}"#;

    let parsed = parse_file_config_bytes(data).expect("legacy key should parse");
    assert_eq!(
        parsed.allowed_repositories,
        Some(vec!["https://github.com/org/*".to_string()])
    );
}

#[test]
fn test_file_config_accepts_allowed_repositories_key() {
    let data = br#"{"allowed_repositories":["/work/repos"]}"#;

    let parsed = parse_file_config_bytes(data).expect("canonical key should parse");
    assert_eq!(
        parsed.allowed_repositories,
        Some(vec!["/work/repos".to_string()])
    );
}

#[test]
fn test_config_patch_accepts_both_allowlist_keys() {
    let patch: ConfigPatch = serde_json::from_str(r#"{"allowed_repositories":["/a"]}"#).unwrap();
    assert_eq!(patch.allowed_repositories, Some(vec!["/a".to_string()]));

    let legacy: ConfigPatch = serde_json::from_str(r#"{"allow_repositories":["/b"]}"#).unwrap();
    assert_eq!(legacy.allowed_repositories, Some(vec!["/b".to_string()]));
}

#[test]
#[cfg(unix)]
fn test_path_is_git_ai_binary_symlink_to_git_ai() {
    // A symlink `git → git-ai` should be detected as git-ai.
    let dir = tempfile::tempdir().unwrap();
    let git_ai = dir.path().join("git-ai");
    fs::write(&git_ai, "fake-binary").unwrap();
    std::os::unix::fs::symlink(&git_ai, dir.path().join("git")).unwrap();
    assert!(super::file::path_is_git_ai_binary(&dir.path().join("git")));
}

#[test]
fn test_path_is_git_ai_binary_real_git_with_sibling_symlink() {
    // A real `git` binary should NOT be flagged just because a `git-ai`
    // symlink exists in the same directory (Docker/server environment).
    let dir = tempfile::tempdir().unwrap();
    let real_git = dir.path().join("git");
    fs::write(&real_git, "real-git-binary").unwrap();
    // git-ai is a different file (or symlink to a different file)
    let git_ai_target = dir.path().join("git-ai-actual");
    fs::write(&git_ai_target, "git-ai-binary").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&git_ai_target, dir.path().join("git-ai")).unwrap();
    #[cfg(unix)]
    assert!(!super::file::path_is_git_ai_binary(&real_git));
}

#[test]
fn test_path_is_git_ai_binary_hardlink() {
    // A hard-linked shim (same inode) should be detected as git-ai.
    let dir = tempfile::tempdir().unwrap();
    let git_ai = dir.path().join("git-ai");
    fs::write(&git_ai, "fake-binary").unwrap();
    #[cfg(unix)]
    {
        let git = dir.path().join("git");
        fs::hard_link(&git_ai, &git).unwrap();
        assert!(super::file::path_is_git_ai_binary(&git));
    }
}

// --- NotesBackendConfig tests ---

#[test]
fn test_notes_backend_config_default_is_sqlite() {
    let cfg = NotesBackendConfig::default();
    assert_eq!(cfg.kind, NotesBackendKind::Sqlite);
    assert!(cfg.backend_url.is_none());
}

#[test]
fn test_notes_backend_kind_roundtrip() {
    // Serialize and deserialize the full notes_backend object
    let json = r#"{"kind": "http", "backend_url": "https://x"}"#;
    let parsed: NotesBackendConfig = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.kind, NotesBackendKind::Http);
    assert_eq!(parsed.backend_url.as_deref(), Some("https://x"));

    let serialized = serde_json::to_string(&parsed).unwrap();
    let reparsed: NotesBackendConfig = serde_json::from_str(&serialized).unwrap();
    assert_eq!(reparsed, parsed);
}

#[test]
fn test_notes_backend_nested_file_config_roundtrip() {
    // Full file config containing notes_backend nested object
    let json = r#"{"notes_backend": {"kind": "http", "backend_url": "https://x"}}"#;
    let parsed: FileConfig = serde_json::from_str(json).unwrap();
    let nb = parsed
        .notes_backend
        .clone()
        .expect("notes_backend should be set");
    assert_eq!(nb.kind, NotesBackendKind::Http);
    assert_eq!(nb.backend_url.as_deref(), Some("https://x"));

    // Round-trip: re-serialize and check key is preserved
    let serialized = serde_json::to_string_pretty(&parsed).unwrap();
    assert!(serialized.contains("notes_backend"));
    assert!(serialized.contains("http"));
}

#[test]
fn test_notes_backend_kind_as_str() {
    assert_eq!(NotesBackendKind::GitNotes.as_str(), "git_notes");
    assert_eq!(NotesBackendKind::Http.as_str(), "http");
}

#[test]
fn test_notes_backend_kind_display() {
    assert_eq!(NotesBackendKind::GitNotes.to_string(), "git_notes");
    assert_eq!(NotesBackendKind::Http.to_string(), "http");
}

#[test]
fn test_notes_backend_url_unset_returns_none() {
    // When backend_url is absent, notes_backend_url() is None. Callers must handle the unconfigured case explicitly.
    let config = create_test_config(vec![], vec![]);
    assert_eq!(config.notes_backend_url(), None);
}

#[test]
fn test_notes_backend_enabled_false_for_git_notes() {
    let config = create_test_config(vec![], vec![]);
    assert!(!config.notes_backend_enabled());
}

#[test]
fn test_notes_backend_kind_env_var_parsing() {
    // Test the parsing logic that build_config() uses for GIT_AI_NOTES_BACKEND_KIND.
    // We mirror the match arm directly rather than calling build_config() to avoid
    // the git-path resolution required by that function.
    let parse_kind = |s: &str| -> Option<NotesBackendKind> {
        match s {
            "http" => Some(NotesBackendKind::Http),
            "git_notes" | "git-notes" => Some(NotesBackendKind::GitNotes),
            "sqlite" => Some(NotesBackendKind::Sqlite),
            _ => None,
        }
    };

    assert_eq!(parse_kind("sqlite"), Some(NotesBackendKind::Sqlite));
    assert_eq!(parse_kind("http"), Some(NotesBackendKind::Http));
    assert_eq!(parse_kind("git_notes"), Some(NotesBackendKind::GitNotes));
    assert_eq!(parse_kind("git-notes"), Some(NotesBackendKind::GitNotes));
    assert_eq!(parse_kind("invalid"), None);
    assert_eq!(parse_kind(""), None);
}

#[test]
fn test_notes_backend_env_var_overrides_file_config_via_fresh() {
    // Verify that GIT_AI_NOTES_BACKEND_KIND=http is correctly resolved in
    // `build_config()`. We call Config::fresh() with the env var set.
    // This test depends on a real git binary being findable (same constraint
    // as all other integration-style config tests).
    let old = std::env::var("GIT_AI_NOTES_BACKEND_KIND").ok();
    unsafe {
        std::env::set_var("GIT_AI_NOTES_BACKEND_KIND", "http");
    }
    let cfg = Config::fresh();
    let result = cfg.notes_backend_kind();
    // Restore the env var before any assertion that might panic
    match old {
        Some(v) => unsafe { std::env::set_var("GIT_AI_NOTES_BACKEND_KIND", v) },
        None => unsafe { std::env::remove_var("GIT_AI_NOTES_BACKEND_KIND") },
    }
    assert_eq!(
        result,
        NotesBackendKind::Http,
        "GIT_AI_NOTES_BACKEND_KIND=http should override the default git_notes"
    );
}

#[test]
fn test_transcript_streaming_lookback_days_default() {
    let config = create_test_config(vec![], vec![]);
    assert_eq!(config.transcript_streaming_lookback_days(), Some(7));
}

#[test]
#[serial_test::serial]
fn test_transcript_streaming_lookback_days_env_override() {
    let previous = std::env::var("GIT_AI_TRANSCRIPT_STREAMING_LOOKBACK_DAYS").ok();
    unsafe { std::env::set_var("GIT_AI_TRANSCRIPT_STREAMING_LOOKBACK_DAYS", "14") };
    let config = build_config();
    let result = config.transcript_streaming_lookback_days;
    match previous {
        Some(v) => unsafe { std::env::set_var("GIT_AI_TRANSCRIPT_STREAMING_LOOKBACK_DAYS", v) },
        None => unsafe { std::env::remove_var("GIT_AI_TRANSCRIPT_STREAMING_LOOKBACK_DAYS") },
    }
    assert_eq!(result, Some(14));
}

#[test]
#[serial_test::serial]
fn test_transcript_streaming_lookback_days_zero_means_unlimited() {
    let previous = std::env::var("GIT_AI_TRANSCRIPT_STREAMING_LOOKBACK_DAYS").ok();
    unsafe { std::env::set_var("GIT_AI_TRANSCRIPT_STREAMING_LOOKBACK_DAYS", "0") };
    let config = build_config();
    let result = config.transcript_streaming_lookback_days;
    match previous {
        Some(v) => unsafe { std::env::set_var("GIT_AI_TRANSCRIPT_STREAMING_LOOKBACK_DAYS", v) },
        None => unsafe { std::env::remove_var("GIT_AI_TRANSCRIPT_STREAMING_LOOKBACK_DAYS") },
    }
    assert_eq!(result, None);
}
