use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use glob::Pattern;
use serde::{Deserialize, Serialize};

use crate::feature_flags::FeatureFlags;
use crate::operations::mdm::utils::home_dir;

use super::author::AuthorConfig;
use super::notes_backend::{NotesBackendConfig, NotesBackendKind};
use super::{
    Config, DEFAULT_API_BASE_URL, DEFAULT_MAX_CHECKPOINT_FILE_SIZE_BYTES,
    DEFAULT_MAX_CHECKPOINT_TOTAL_LINES, DEFAULT_MAX_CHECKPOINT_TOTAL_SIZE_BYTES,
};

#[cfg(any(test, feature = "test-support"))]
use std::sync::RwLock;

#[cfg(any(test, feature = "test-support"))]
pub(crate) static TEST_FEATURE_FLAGS_OVERRIDE: RwLock<Option<FeatureFlags>> = RwLock::new(None);

/// Which Codex hook file git-ai should use when installing Codex hooks.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodexHooksFormat {
    /// Default: install git-ai Codex hooks inline in ~/.codex/config.toml.
    #[default]
    ConfigToml,
    /// Install git-ai Codex hooks in ~/.codex/hooks.json.
    HooksJson,
}

impl CodexHooksFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            CodexHooksFormat::ConfigToml => "config_toml",
            CodexHooksFormat::HooksJson => "hooks_json",
        }
    }

    pub(crate) fn from_str(input: &str) -> Option<Self> {
        match input.trim().to_lowercase().as_str() {
            "config_toml" | "config-toml" => Some(CodexHooksFormat::ConfigToml),
            "hooks_json" | "hooks-json" => Some(CodexHooksFormat::HooksJson),
            _ => None,
        }
    }
}

impl std::fmt::Display for CodexHooksFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateChannel {
    #[default]
    Latest,
    Next,
    EnterpriseLatest,
    EnterpriseNext,
}

impl UpdateChannel {
    pub fn as_str(&self) -> &'static str {
        match self {
            UpdateChannel::Latest => "latest",
            UpdateChannel::Next => "next",
            UpdateChannel::EnterpriseLatest => "enterprise-latest",
            UpdateChannel::EnterpriseNext => "enterprise-next",
        }
    }

    pub(crate) fn from_str(input: &str) -> Option<Self> {
        match input.trim().to_lowercase().as_str() {
            "latest" => Some(UpdateChannel::Latest),
            "next" => Some(UpdateChannel::Next),
            "enterprise-latest" => Some(UpdateChannel::EnterpriseLatest),
            "enterprise-next" => Some(UpdateChannel::EnterpriseNext),
            _ => None,
        }
    }
}

#[derive(Deserialize, Serialize, Default)]
pub struct FileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_prompts_in_repositories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_prompts_in_repositories: Option<Vec<String>>,
    #[serde(
        default,
        alias = "allow_repositories",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_repositories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_repositories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_oss: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_enterprise_dsn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_version_checks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_auto_updates: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_flags: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_storage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_prompt_storage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_superuser: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<AuthorConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_attributes: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ai_hooks: Option<HashMap<String, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_hooks_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_backend: Option<NotesBackendConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_streaming_lookback_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_checkpoint_file_size_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_checkpoint_total_size_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_checkpoint_total_lines: Option<usize>,
}

/// Serializable config patch for test overrides
/// All fields are optional to allow patching only specific properties
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_prompts_in_repositories: Option<Vec<String>>,
    #[serde(
        default,
        alias = "allow_repositories",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_repositories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_oss_disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_version_checks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_auto_updates: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_storage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<AuthorConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_attributes: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_flags: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_hooks_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_backend: Option<NotesBackendConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_streaming_lookback_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_checkpoint_file_size_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_checkpoint_total_size_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_checkpoint_total_lines: Option<usize>,
}

pub(crate) fn load_file_config() -> Option<FileConfig> {
    let path = config_file_path()?;
    let data = fs::read(&path).ok()?;
    parse_file_config_bytes(&data).ok()
}

/// Master telemetry switch resolution: off unless explicitly enabled via the
/// `telemetry` key ("on"/"off") or the legacy `telemetry_oss` key ("on").
pub(crate) fn resolve_telemetry_enabled(telemetry: Option<&str>, legacy_oss: Option<&str>) -> bool {
    match telemetry.map(str::trim) {
        Some("on") => true,
        Some("off") => false,
        Some(other) => {
            eprintln!("Warning: Invalid telemetry value '{}', using 'off'", other);
            false
        }
        None => legacy_oss.map(str::trim) == Some("on"),
    }
}

/// Strip a leading UTF-8 byte-order mark (`EF BB BF`, the encoding of
/// `'\u{feff}'`) from `data`, if present. Shared by config-file parsing
/// (Windows PowerShell 5.1 writes UTF-8 with BOM by default for
/// `Out-File -Encoding UTF8`) and checkpoint hook-input decoding, both of
/// which need to tolerate BOM-prefixed input.
pub(crate) fn strip_utf8_bom(data: &[u8]) -> &[u8] {
    data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data)
}

pub(crate) fn parse_file_config_bytes(data: &[u8]) -> Result<FileConfig, serde_json::Error> {
    let data = strip_utf8_bom(data);
    serde_json::from_slice::<FileConfig>(data)
}

pub(crate) fn config_file_path() -> Option<PathBuf> {
    Some(home_dir().join(".git-ai").join("config.json"))
}

/// Public accessor for config file path
#[allow(dead_code)]
pub fn config_file_path_public() -> Option<PathBuf> {
    config_file_path()
}

/// Load the raw file config
pub fn load_file_config_public() -> Result<FileConfig, String> {
    let path =
        config_file_path().ok_or_else(|| "Could not determine config file path".to_string())?;

    if !path.exists() {
        // Return empty config if file doesn't exist
        return Ok(FileConfig::default());
    }

    let data = fs::read(&path).map_err(|e| format!("Failed to read config file: {}", e))?;

    parse_file_config_bytes(&data).map_err(|e| format!("Failed to parse config file: {}", e))
}

/// Save the file config
pub fn save_file_config(config: &FileConfig) -> Result<(), String> {
    let path =
        config_file_path().ok_or_else(|| "Could not determine config file path".to_string())?;

    // Ensure the directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create config directory: {}", e))?;
    }

    let json = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;

    fs::write(&path, json).map_err(|e| format!("Failed to write config file: {}", e))
}

pub(crate) fn build_config() -> Config {
    let file_cfg = load_file_config();
    let exclude_prompts_in_repositories = compile_glob_field(
        file_cfg
            .as_ref()
            .and_then(|c| c.exclude_prompts_in_repositories.clone()),
        "exclude_prompts_in_repositories",
    );
    let include_prompts_in_repositories = compile_glob_field(
        file_cfg
            .as_ref()
            .and_then(|c| c.include_prompts_in_repositories.clone()),
        "include_prompts_in_repositories",
    );
    let allowed_repositories = compile_glob_field(
        file_cfg
            .as_ref()
            .and_then(|c| c.allowed_repositories.clone()),
        "allowed_repositories",
    );
    let exclude_repositories = compile_glob_field(
        file_cfg
            .as_ref()
            .and_then(|c| c.exclude_repositories.clone()),
        "exclude_repositories",
    );
    let telemetry_oss_disabled = file_cfg
        .as_ref()
        .and_then(|c| c.telemetry_oss.clone())
        .filter(|s| s == "off")
        .is_some();
    let telemetry_enabled = resolve_telemetry_enabled(
        file_cfg.as_ref().and_then(|c| c.telemetry.as_deref()),
        file_cfg.as_ref().and_then(|c| c.telemetry_oss.as_deref()),
    );
    let telemetry_enterprise_dsn = file_cfg
        .as_ref()
        .and_then(|c| c.telemetry_enterprise_dsn.clone())
        .filter(|s| !s.is_empty());

    // Default to disabled (true) unless this is an OSS build
    // OSS builds set OSS_BUILD env var at compile time to "1", which enables auto-updates by default
    let auto_update_flags_default_disabled = option_env!("OSS_BUILD") != Some("1");

    let disable_version_checks = file_cfg
        .as_ref()
        .and_then(|c| c.disable_version_checks)
        .unwrap_or(auto_update_flags_default_disabled);
    let disable_auto_updates = file_cfg
        .as_ref()
        .and_then(|c| c.disable_auto_updates)
        .unwrap_or(auto_update_flags_default_disabled);
    let update_channel = file_cfg
        .as_ref()
        .and_then(|c| c.update_channel.as_deref())
        .and_then(UpdateChannel::from_str)
        .unwrap_or_default();

    let git_path = resolve_git_path(&file_cfg);

    // Build feature flags from file config
    let feature_flags = build_feature_flags(&file_cfg);

    // Get API base URL from config, env var, or default
    let api_base_url = file_cfg
        .as_ref()
        .and_then(|c| c.api_base_url.clone())
        .or_else(|| env::var("GIT_AI_API_BASE_URL").ok())
        .unwrap_or_else(|| DEFAULT_API_BASE_URL.to_string());

    // Get prompt_storage setting (defaults to "local": prompts stay on this
    // machine unless the user explicitly opts into "notes" or "default"/CAS)
    // Valid values: "default", "notes", "local"
    let prompt_storage = file_cfg
        .as_ref()
        .and_then(|c| c.prompt_storage.clone())
        .unwrap_or_else(|| "local".to_string());
    let prompt_storage = match prompt_storage.as_str() {
        "default" | "notes" | "local" => prompt_storage,
        other => {
            eprintln!(
                "Warning: Invalid prompt_storage value '{}', using 'local'",
                other
            );
            "local".to_string()
        }
    };

    // Get default_prompt_storage setting (fallback for repos not in include list)
    // Valid values: "default", "notes", "local", or None (defaults to "local")
    let default_prompt_storage = file_cfg
        .as_ref()
        .and_then(|c| c.default_prompt_storage.clone())
        .and_then(|s| {
            if matches!(s.as_str(), "default" | "notes" | "local") {
                Some(s)
            } else {
                eprintln!(
                    "Warning: Invalid default_prompt_storage value '{}', ignoring",
                    s
                );
                None
            }
        });

    // Get API key from env var or config file (env var takes precedence)
    let api_key = env::var("GIT_AI_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            file_cfg
                .as_ref()
                .and_then(|c| c.api_key.clone())
                .filter(|s| !s.is_empty())
        });

    // Get quiet setting (defaults to false)
    let quiet = file_cfg.as_ref().and_then(|c| c.quiet).unwrap_or(false);

    let allow_superuser = file_cfg
        .as_ref()
        .and_then(|c| c.allow_superuser)
        .unwrap_or(false);

    let author = file_cfg
        .as_ref()
        .and_then(|c| c.author.clone())
        .unwrap_or_default()
        .normalized();

    // Build custom attributes: file config as base, env var overrides
    let custom_attributes = build_custom_attributes(&file_cfg);

    let git_ai_hooks = file_cfg
        .as_ref()
        .and_then(|c| c.git_ai_hooks.clone())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(hook_name, commands)| {
            let hook_name = hook_name.trim().to_string();
            if hook_name.is_empty() {
                return None;
            }

            let commands: Vec<String> = commands
                .into_iter()
                .map(|command| command.trim().to_string())
                .filter(|command| !command.is_empty())
                .collect();
            if commands.is_empty() {
                return None;
            }

            Some((hook_name, commands))
        })
        .collect::<HashMap<String, Vec<String>>>();

    let codex_hooks_format = file_cfg
        .as_ref()
        .and_then(|c| c.codex_hooks_format.as_deref())
        .and_then(|value| {
            let parsed = CodexHooksFormat::from_str(value);
            if parsed.is_none() {
                eprintln!(
                    "Warning: Invalid codex_hooks_format value '{}', using 'config_toml'",
                    value
                );
            }
            parsed
        })
        .unwrap_or_default();

    // Resolve notes_backend config: env vars override file config, which overrides defaults.
    let file_backend = file_cfg.as_ref().and_then(|c| c.notes_backend.clone());
    let kind_from_env = env::var("GIT_AI_NOTES_BACKEND_KIND")
        .ok()
        .and_then(|s| match s.as_str() {
            "http" => Some(NotesBackendKind::Http),
            "git_notes" | "git-notes" => Some(NotesBackendKind::GitNotes),
            "sqlite" => Some(NotesBackendKind::Sqlite),
            _ => None,
        });
    let url_from_env = env::var("GIT_AI_NOTES_BACKEND_URL").ok();

    // Unconfigured default: sqlite in production. Test builds default to
    // git_notes because in-process test code (which cannot use per-test config
    // patches without racing on process env) predates the sqlite backend and
    // asserts against refs/notes/ai; sqlite-backend behavior is covered by
    // tests that pin the kind explicitly.
    #[cfg(any(test, feature = "test-support"))]
    let unconfigured_kind = NotesBackendKind::GitNotes;
    #[cfg(not(any(test, feature = "test-support")))]
    let unconfigured_kind = NotesBackendKind::default();

    let notes_backend = NotesBackendConfig {
        kind: kind_from_env
            .or_else(|| file_backend.as_ref().map(|b| b.kind))
            .unwrap_or(unconfigured_kind),
        backend_url: url_from_env
            .or_else(|| file_backend.as_ref().and_then(|b| b.backend_url.clone())),
    };

    // Transcript streaming lookback: env > file > default (7 days). 0 means unlimited (None).
    let transcript_streaming_lookback_days = Some(env_or_file(
        "GIT_AI_TRANSCRIPT_STREAMING_LOOKBACK_DAYS",
        file_cfg
            .as_ref()
            .and_then(|c| c.transcript_streaming_lookback_days),
        7u32,
    ))
    .filter(|&v| v != 0);

    // Checkpoint content limits: env > file > defaults.
    let max_checkpoint_file_size_bytes = env_or_file(
        "GIT_AI_MAX_CHECKPOINT_FILE_SIZE_BYTES",
        file_cfg
            .as_ref()
            .and_then(|c| c.max_checkpoint_file_size_bytes),
        DEFAULT_MAX_CHECKPOINT_FILE_SIZE_BYTES,
    );

    let max_checkpoint_total_size_bytes = env_or_file(
        "GIT_AI_MAX_CHECKPOINT_TOTAL_SIZE_BYTES",
        file_cfg
            .as_ref()
            .and_then(|c| c.max_checkpoint_total_size_bytes),
        DEFAULT_MAX_CHECKPOINT_TOTAL_SIZE_BYTES,
    );

    let max_checkpoint_total_lines = env_or_file(
        "GIT_AI_MAX_CHECKPOINT_TOTAL_LINES",
        file_cfg.as_ref().and_then(|c| c.max_checkpoint_total_lines),
        DEFAULT_MAX_CHECKPOINT_TOTAL_LINES,
    );

    let config = Config {
        git_path,
        exclude_prompts_in_repositories,
        include_prompts_in_repositories,
        allowed_repositories,
        exclude_repositories,
        telemetry_enabled,
        telemetry_oss_disabled,
        telemetry_enterprise_dsn,
        disable_version_checks,
        disable_auto_updates,
        update_channel,
        feature_flags,
        api_base_url,
        prompt_storage,
        default_prompt_storage,
        api_key,
        quiet,
        allow_superuser,
        author,
        custom_attributes,
        git_ai_hooks,
        codex_hooks_format,
        notes_backend,
        transcript_streaming_lookback_days,
        max_checkpoint_file_size_bytes,
        max_checkpoint_total_size_bytes,
        max_checkpoint_total_lines,
    };

    #[cfg(any(test, feature = "test-support"))]
    let config = {
        let mut config = config;
        apply_test_config_patch(&mut config);
        config
    };

    config
}

/// Compile a `Vec<String>` glob-pattern config field into `Vec<Pattern>`,
/// dropping (and warning about) any pattern that fails to compile.
fn compile_glob_field(patterns_opt: Option<Vec<String>>, field_name: &str) -> Vec<Pattern> {
    patterns_opt
        .unwrap_or_default()
        .into_iter()
        .filter_map(|pattern_str| {
            Pattern::new(&pattern_str)
                .map_err(|e| {
                    eprintln!(
                        "Warning: Invalid glob pattern in {} '{}': {}",
                        field_name, pattern_str, e
                    );
                })
                .ok()
        })
        .collect()
}

/// Resolve a numeric config value: env var > file config > default.
fn env_or_file<T: FromStr>(env_key: &str, file_val: Option<T>, default: T) -> T {
    env::var(env_key)
        .ok()
        .and_then(|v| v.parse::<T>().ok())
        .or(file_val)
        .unwrap_or(default)
}

/// Build custom attributes from file config and `GIT_AI_CUSTOM_ATTRIBUTES` env var.
/// Env var keys override file config keys on conflict.
fn build_custom_attributes(file_cfg: &Option<FileConfig>) -> HashMap<String, String> {
    let mut attrs = file_cfg
        .as_ref()
        .and_then(|c| c.custom_attributes.clone())
        .unwrap_or_default();

    if let Ok(env_val) = env::var("GIT_AI_CUSTOM_ATTRIBUTES") {
        if let Ok(env_attrs) = serde_json::from_str::<HashMap<String, serde_json::Value>>(&env_val)
        {
            for (k, v) in env_attrs {
                match v {
                    serde_json::Value::String(s) => {
                        attrs.insert(k, s);
                    }
                    serde_json::Value::Number(n) => {
                        attrs.insert(k, n.to_string());
                    }
                    serde_json::Value::Bool(b) => {
                        attrs.insert(k, b.to_string());
                    }
                    _ => {} // silently drop arrays, objects, null
                }
            }
        } else {
            tracing::debug!("GIT_AI_CUSTOM_ATTRIBUTES is not valid JSON, ignoring");
        }
    }

    attrs
}

fn build_feature_flags(file_cfg: &Option<FileConfig>) -> FeatureFlags {
    let mut file_flags_value = file_cfg
        .as_ref()
        .and_then(|c| c.feature_flags.as_ref())
        .cloned();

    // Backward-compatible alias: accept `feature_flags.globalGitHooks` from config files.
    if let Some(serde_json::Value::Object(ref mut flags)) = file_flags_value
        && let Some(value) = flags.get("globalGitHooks").cloned()
        && !flags.contains_key("global_git_hooks")
    {
        flags.insert("global_git_hooks".to_string(), value);
    }

    // Try to deserialize the feature flags from the JSON value
    let file_flags = file_flags_value.and_then(|value| {
        // Use from_value to deserialize, but ignore any errors and fall back to defaults
        serde_json::from_value(value).ok()
    });

    FeatureFlags::from_env_and_file(file_flags)
}

fn resolve_git_path(file_cfg: &Option<FileConfig>) -> String {
    // 1) From config file
    if let Some(cfg) = file_cfg
        && let Some(path) = cfg.git_path.as_ref()
    {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            let p = Path::new(trimmed);
            if is_executable(p) && !path_is_git_ai_binary(p) {
                return trimmed.to_string();
            }
        }
    }

    // 2) Probe common locations across platforms.
    // All candidates are guarded by path_is_git_ai_binary so that a git-ai shim at any
    // of these locations can never be returned as the "real git" (fork bomb prevention).
    #[cfg(not(windows))]
    let local_bin_git = format!("{}/.local/bin/git", home_dir().display());

    #[cfg(windows)]
    let local_app_data_candidates: Vec<String> = std::env::var("LOCALAPPDATA")
        .ok()
        .map(|lad| {
            vec![
                format!(r"{}\Programs\Git\cmd\git.exe", lad),
                format!(r"{}\Programs\Git\bin\git.exe", lad),
            ]
        })
        .unwrap_or_default();

    let static_candidates: &[&str] = &[
        #[cfg(not(windows))]
        local_bin_git.as_str(),
        #[cfg(not(windows))]
        "/opt/homebrew/bin/git",
        #[cfg(not(windows))]
        "/usr/local/bin/git",
        #[cfg(not(windows))]
        "/usr/bin/git",
        #[cfg(not(windows))]
        "/bin/git",
        #[cfg(not(windows))]
        "/usr/local/sbin/git",
        #[cfg(not(windows))]
        "/usr/sbin/git",
        #[cfg(windows)]
        r"C:\Program Files\Git\cmd\git.exe",
        #[cfg(windows)]
        r"C:\Program Files\Git\bin\git.exe",
        #[cfg(windows)]
        r"C:\Program Files (x86)\Git\cmd\git.exe",
        #[cfg(windows)]
        r"C:\Program Files (x86)\Git\bin\git.exe",
    ];

    #[cfg(windows)]
    let all_candidates: Vec<&str> = {
        let mut v: Vec<&str> = static_candidates.to_vec();
        for c in &local_app_data_candidates {
            v.push(c.as_str());
        }
        v
    };

    #[cfg(windows)]
    let candidates: &[&str] = &all_candidates;
    #[cfg(not(windows))]
    let candidates: &[&str] = static_candidates;

    if let Some(found) = candidates
        .iter()
        .map(Path::new)
        .find(|p| is_executable(p) && !path_is_git_ai_binary(p))
    {
        return found.to_string_lossy().to_string();
    }

    // 3) Windows-only: try `where.exe git.exe` as a PATH-based fallback
    #[cfg(windows)]
    {
        if let Ok(output) = std::process::Command::new("where.exe")
            .arg("git.exe")
            .output()
            && output.status.success()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let trimmed = line.trim();
                let p = Path::new(trimmed);
                if is_executable(p) && !path_is_git_ai_binary(p) {
                    return trimmed.to_string();
                }
            }
        }
    }

    eprintln!(
        "Fatal: Could not locate a real 'git' binary.\n\
         Expected a valid 'git_path' in {cfg_path} or in standard locations.\n\
         Please install Git or update your config JSON.",
        cfg_path = config_file_path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "~/.git-ai/config.json".to_string()),
    );
    std::process::exit(1);
}

fn is_executable(path: &Path) -> bool {
    if !path.exists() || !path.is_file() {
        return false;
    }
    // Basic check: existence is sufficient for our purposes; OS will enforce exec perms.
    // On Unix we could check permissions, but many filesystems differ. Keep it simple.
    true
}

/// Check whether two paths refer to the same underlying file.
/// On Unix this compares (dev, ino); on other platforms it falls back to
/// comparing canonicalized paths.
#[cfg(not(windows))]
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) {
            return ma.dev() == mb.dev() && ma.ino() == mb.ino();
        }
    }
    #[cfg(not(unix))]
    {
        if let (Ok(ca), Ok(cb)) = (a.canonicalize(), b.canonicalize()) {
            return ca == cb;
        }
    }
    false
}

/// Detect if a path is actually the git-ai binary (or a symlink to it).
/// This prevents `git_cmd()` from returning the git-ai shim, which would
/// cause infinite recursion: handle_git() → proxy_to_git() → shim → handle_git() → ...
pub(crate) fn path_is_git_ai_binary(path: &Path) -> bool {
    // Check canonical path — if the path resolves to a binary whose name
    // is git-ai (or a variant), it is the git-ai binary regardless of what
    // the original path looks like (catches symlinks like `git → git-ai`).
    if let Ok(canonical) = path.canonicalize()
        && let Some(name) = canonical.file_name().and_then(|n| n.to_str())
    {
        let stem = name.strip_suffix(".exe").unwrap_or(name);
        if stem == "git-ai" || stem.starts_with("git-ai-") || stem.starts_with("git_ai") {
            return true;
        }
    }

    // Check if a sibling "git-ai" exists in the same directory.
    // On Windows the installer copies git-ai.exe to git.exe (not a symlink or
    // hard-link), so same_file() would return false. A sibling git-ai.exe
    // existing is sufficient to identify this as the git-ai install directory.
    // On Unix, additionally verify both refer to the same underlying file
    // (hard-link / bind-mount) to avoid false-positives in environments where
    // a real git binary legitimately coexists with a git-ai symlink (e.g.
    // Docker images that compile git from source into /usr/local/bin).
    if let Some(parent) = path.parent() {
        #[cfg(windows)]
        let sibling = parent.join("git-ai.exe");
        #[cfg(not(windows))]
        let sibling = parent.join("git-ai");

        #[cfg(windows)]
        if sibling.exists() {
            return true;
        }
        #[cfg(not(windows))]
        if sibling.exists() && same_file(path, &sibling) {
            return true;
        }
    }

    false
}

/// Returns true if `p` is an executable git binary that is NOT git-ai.
/// Used by test infrastructure to probe for the real git binary independently
/// of `Config::get()` (which reads HOME and must not be called before HOME is isolated).
pub fn is_real_git_candidate(p: &Path) -> bool {
    is_executable(p) && !path_is_git_ai_binary(p)
}

/// Apply test config patch from environment variable (test-only)
/// Reads GIT_AI_TEST_CONFIG_PATCH env var containing JSON and applies patches to config
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn apply_test_config_patch(config: &mut Config) {
    if let Ok(patch_json) = env::var("GIT_AI_TEST_CONFIG_PATCH")
        && let Ok(patch) = serde_json::from_str::<ConfigPatch>(&patch_json)
    {
        if let Some(patterns) = patch.allowed_repositories {
            config.allowed_repositories = patterns
                .into_iter()
                .filter_map(|pattern_str| {
                    Pattern::new(&pattern_str)
                        .map_err(|e| {
                            eprintln!(
                                "Warning: Invalid test pattern in allowed_repositories '{}': {}",
                                pattern_str, e
                            );
                        })
                        .ok()
                })
                .collect();
        }
        if let Some(patterns) = patch.exclude_prompts_in_repositories {
            config.exclude_prompts_in_repositories = patterns
                    .into_iter()
                    .filter_map(|pattern_str| {
                        Pattern::new(&pattern_str)
                            .map_err(|e| {
                                eprintln!(
                                    "Warning: Invalid test pattern in exclude_prompts_in_repositories '{}': {}",
                                    pattern_str, e
                                );
                            })
                            .ok()
                    })
                    .collect();
        }
        if let Some(telemetry_oss_disabled) = patch.telemetry_oss_disabled {
            config.telemetry_oss_disabled = telemetry_oss_disabled;
        }
        if let Some(telemetry) = patch.telemetry {
            match telemetry.trim() {
                "on" => config.telemetry_enabled = true,
                "off" => config.telemetry_enabled = false,
                other => {
                    eprintln!(
                        "Warning: Invalid test telemetry value '{}', ignoring",
                        other
                    );
                }
            }
        }
        if let Some(disable_version_checks) = patch.disable_version_checks {
            config.disable_version_checks = disable_version_checks;
        }
        if let Some(disable_auto_updates) = patch.disable_auto_updates {
            config.disable_auto_updates = disable_auto_updates;
        }
        if let Some(prompt_storage) = patch.prompt_storage {
            // Validate the value
            if matches!(prompt_storage.as_str(), "default" | "notes" | "local") {
                config.prompt_storage = prompt_storage;
            } else {
                eprintln!(
                    "Warning: Invalid test prompt_storage value '{}', ignoring",
                    prompt_storage
                );
            }
        }
        if let Some(custom_attributes) = patch.custom_attributes {
            config.custom_attributes = custom_attributes;
        }
        if let Some(author) = patch.author {
            config.author = author.normalized();
        }
        if let Some(feature_flags_value) = patch.feature_flags
            && let Ok(deserialized) = serde_json::from_value::<
                crate::feature_flags::DeserializableFeatureFlags,
            >(feature_flags_value)
        {
            config.feature_flags = crate::feature_flags::FeatureFlags::merge_with(
                config.feature_flags.clone(),
                deserialized,
            );
        }
        if let Some(codex_hooks_format) = patch.codex_hooks_format {
            if let Some(format) = CodexHooksFormat::from_str(&codex_hooks_format) {
                config.codex_hooks_format = format;
            } else {
                eprintln!(
                    "Warning: Invalid test codex_hooks_format value '{}', ignoring",
                    codex_hooks_format
                );
            }
        }
        if let Some(nb) = patch.notes_backend {
            config.notes_backend.kind = nb.kind;
            if let Some(url) = nb.backend_url {
                config.notes_backend.backend_url = Some(url);
            }
        }
        if let Some(days) = patch.transcript_streaming_lookback_days {
            config.transcript_streaming_lookback_days = if days == 0 { None } else { Some(days) };
        }
        if let Some(max_bytes) = patch.max_checkpoint_file_size_bytes {
            config.max_checkpoint_file_size_bytes = max_bytes;
        }
        if let Some(max_bytes) = patch.max_checkpoint_total_size_bytes {
            config.max_checkpoint_total_size_bytes = max_bytes;
        }
        if let Some(max_lines) = patch.max_checkpoint_total_lines {
            config.max_checkpoint_total_lines = max_lines;
        }
    }
}
