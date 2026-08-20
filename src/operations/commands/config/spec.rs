//! Canonical metadata for the `git-ai config` command.
//!
//! The command keeps typed mutation code in the get/set/unset modules, but all
//! public key names, aliases, nesting rules, and help text live here.  This
//! makes adding a persisted setting an explicit, reviewable change instead of
//! another independent string match in each command.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConfigValueKind {
    String,
    Boolean,
    Integer,
    Array,
    Object,
    Secret,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConfigNesting {
    None,
    /// The parent has a fixed set of known child paths, but the command owns
    /// the child-specific error text and validation.
    Fixed,
    /// The parent accepts user-defined child names (for example feature flags
    /// and custom attributes).
    Dynamic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConfigMutation {
    Replace,
    ReplaceOrAdd,
    NestedOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ConfigKeySpec {
    pub(super) name: &'static str,
    pub(super) aliases: &'static [&'static str],
    pub(super) value_kind: ConfigValueKind,
    pub(super) sensitive: bool,
    pub(super) nesting: ConfigNesting,
    pub(super) mutation: ConfigMutation,
    /// The complete line printed by `git-ai config --help`, without the
    /// command's two-space indentation prefix.
    pub(super) help: &'static str,
    pub(super) show_in_help: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ResolvedConfigKey {
    pub(super) spec: &'static ConfigKeySpec,
}

impl ResolvedConfigKey {
    /// Return the top-level path component used by the typed handlers.
    pub(super) fn root(self) -> &'static str {
        self.spec.name.split('.').next().unwrap_or(self.spec.name)
    }
}

const NO_ALIASES: &[&str] = &[];

// Keep this order aligned with the existing help output.  Hidden parent
// entries are retained in the registry so nested paths resolve through the
// same metadata as top-level paths without changing that output.
const CONFIG_KEY_SPECS: &[ConfigKeySpec] = &[
    ConfigKeySpec {
        name: "git_path",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::String,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "git_path                     Path to git binary",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "exclude_prompts_in_repositories",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Array,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::ReplaceOrAdd,
        help: "exclude_prompts_in_repositories  Repos to exclude prompts from (array)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "allowed_repositories",
        aliases: &["allow_repositories"],
        value_kind: ConfigValueKind::Array,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::ReplaceOrAdd,
        help: "allowed_repositories         Repositories where collection is enabled (array; empty = collect nothing)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "exclude_repositories",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Array,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::ReplaceOrAdd,
        help: "exclude_repositories         Excluded repos (array)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "telemetry",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::String,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "telemetry                    Master telemetry switch (on/off; default off)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "telemetry_oss",
        aliases: &["telemetry_oss_disabled"],
        value_kind: ConfigValueKind::String,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "telemetry_oss                Legacy OSS telemetry setting (on/off)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "telemetry_enterprise_dsn",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::String,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "telemetry_enterprise_dsn     Enterprise telemetry DSN",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "disable_version_checks",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Boolean,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "disable_version_checks       Disable version checks (bool)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "disable_auto_updates",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Boolean,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "disable_auto_updates         Disable auto updates (bool)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "update_channel",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::String,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "update_channel               Update channel (latest/next)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "feature_flags",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Object,
        sensitive: false,
        nesting: ConfigNesting::Dynamic,
        mutation: ConfigMutation::ReplaceOrAdd,
        help: "feature_flags                Feature flags (object)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "api_base_url",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::String,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "api_base_url                 API base URL (default: https://usegitai.com)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "api_key",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Secret,
        sensitive: true,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "api_key                      API key for X-API-Key header",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "author",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Object,
        sensitive: false,
        nesting: ConfigNesting::Fixed,
        mutation: ConfigMutation::ReplaceOrAdd,
        help: "",
        show_in_help: false,
    },
    ConfigKeySpec {
        name: "author.name",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::String,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "author.name                  git-ai author display name override",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "author.email",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::String,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "author.email                 git-ai author email override",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "prompt_storage",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::String,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "prompt_storage               Prompt storage mode (default/notes/local)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "include_prompts_in_repositories",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Array,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::ReplaceOrAdd,
        help: "include_prompts_in_repositories  Repos to include for prompt storage (array)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "default_prompt_storage",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::String,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "default_prompt_storage       Fallback storage mode for non-included repos",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "quiet",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Boolean,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "quiet                        Suppress chart output after commits (bool)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "allow_superuser",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Boolean,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "allow_superuser              Allow running git-ai as root/superuser (bool)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "transcript_streaming_lookback_days",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Integer,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "transcript_streaming_lookback_days  Days to look back when sweeping transcripts (0 = unlimited)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "max_checkpoint_file_size_bytes",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Integer,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "max_checkpoint_file_size_bytes      Per-file checkpoint content limit in bytes",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "max_checkpoint_total_size_bytes",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Integer,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "max_checkpoint_total_size_bytes     Per-checkpoint content limit in bytes",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "max_checkpoint_total_lines",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Integer,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "max_checkpoint_total_lines          Per-checkpoint content limit in lines",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "daemon_memory_limit_mb",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Integer,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "daemon_memory_limit_mb              Daemon peak-RSS limit in MiB",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "custom_attributes",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Object,
        sensitive: false,
        nesting: ConfigNesting::Dynamic,
        mutation: ConfigMutation::ReplaceOrAdd,
        help: "custom_attributes            Custom telemetry attributes, string->string (object)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "git_ai_hooks",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Object,
        sensitive: false,
        nesting: ConfigNesting::Dynamic,
        mutation: ConfigMutation::ReplaceOrAdd,
        help: "git_ai_hooks                 Hook name -> shell commands map (object)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "codex_hooks_format",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::String,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "codex_hooks_format           Codex hook install format (config_toml/hooks_json)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "notes_backend",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::Object,
        sensitive: false,
        nesting: ConfigNesting::Fixed,
        mutation: ConfigMutation::NestedOnly,
        help: "",
        show_in_help: false,
    },
    ConfigKeySpec {
        name: "notes_backend.kind",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::String,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "notes_backend.kind           Notes backend kind (git_notes/http)",
        show_in_help: true,
    },
    ConfigKeySpec {
        name: "notes_backend.backend_url",
        aliases: NO_ALIASES,
        value_kind: ConfigValueKind::String,
        sensitive: false,
        nesting: ConfigNesting::None,
        mutation: ConfigMutation::Replace,
        help: "notes_backend.backend_url    Notes backend base URL. Required when kind=http.",
        show_in_help: true,
    },
];

pub(super) fn config_key_specs() -> &'static [ConfigKeySpec] {
    CONFIG_KEY_SPECS
}

pub(super) fn resolve_key(key: &str) -> Result<ResolvedConfigKey, String> {
    let key = key.trim();

    if let Some(spec) = CONFIG_KEY_SPECS
        .iter()
        .find(|spec| spec.name == key || spec.aliases.contains(&key))
    {
        return Ok(ResolvedConfigKey { spec });
    }

    let root = key.split('.').next().unwrap_or_default();
    if let Some(spec) = CONFIG_KEY_SPECS
        .iter()
        .find(|spec| spec.name == root && spec.nesting != ConfigNesting::None)
    {
        return Ok(ResolvedConfigKey { spec });
    }

    Err(format!("Unknown config key: {}", key))
}
