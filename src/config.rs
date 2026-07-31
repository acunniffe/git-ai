use std::collections::{HashMap, hash_map::DefaultHasher};
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use glob::Pattern;
use serde::{Deserialize, Serialize, Serializer};

use crate::feature_flags::FeatureFlags;
use crate::git::repository::Repository;
use crate::mdm::utils::home_dir;

#[cfg(any(test, feature = "test-support"))]
use std::sync::RwLock;

/// Default API base URL for comparison
pub const DEFAULT_API_BASE_URL: &str = "https://usegitai.com";
pub const DEFAULT_MAX_CHECKPOINT_FILE_SIZE_BYTES: usize = 3 * 1024 * 1024;
pub const DEFAULT_MAX_CHECKPOINT_TOTAL_SIZE_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_MAX_CHECKPOINT_TOTAL_LINES: usize = 500_000;
/// Jitter is added on top of the 24h update interval. Four days of jitter
/// keeps checks within the requested 24h..5d window.
pub const MAX_UPGRADE_JITTER_SECONDS: u64 = 4 * 24 * 60 * 60;

/// Which backend to use for storing authorship notes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NotesBackendKind {
    /// Default: store notes in git refs/notes/ai (existing behavior)
    #[default]
    GitNotes,
    /// HTTP backend: queue writes to notes-db, flush via daemon, reads from cache
    Http,
}

impl NotesBackendKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            NotesBackendKind::GitNotes => "git_notes",
            NotesBackendKind::Http => "http",
        }
    }
}

impl std::fmt::Display for NotesBackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Configuration for the notes backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct NotesBackendConfig {
    #[serde(default)]
    pub kind: NotesBackendKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_url: Option<String>,
}

/// Optional git-ai author override for authorship metadata.
///
/// Any unset field falls back to the effective Git committer identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AuthorConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl AuthorConfig {
    pub fn normalized(mut self) -> Self {
        self.name = normalize_optional_string(self.name);
        self.email = normalize_optional_string(self.email);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.email.is_none()
    }
}

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

    fn from_str(input: &str) -> Option<Self> {
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

/// Prompt storage mode enum for type-safe handling
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptStorageMode {
    /// Default mode: prompts uploaded via CAS API, stripped from git notes
    Default,
    /// Notes mode: prompts stored in git notes (after secret redaction)
    Notes,
    /// Local mode: prompts only stored in local SQLite, never shared
    Local,
}

impl PromptStorageMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PromptStorageMode::Default => "default",
            PromptStorageMode::Notes => "notes",
            PromptStorageMode::Local => "local",
        }
    }
}

impl std::str::FromStr for PromptStorageMode {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input.trim().to_lowercase().as_str() {
            "default" => Ok(PromptStorageMode::Default),
            "notes" => Ok(PromptStorageMode::Notes),
            "local" => Ok(PromptStorageMode::Local),
            other => Err(format!("invalid prompt storage mode: '{}'", other)),
        }
    }
}

impl std::fmt::Display for PromptStorageMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Serialize)]
pub struct Config {
    git_path: String,
    #[serde(serialize_with = "serialize_patterns")]
    exclude_prompts_in_repositories: Vec<Pattern>,
    #[serde(serialize_with = "serialize_patterns")]
    include_prompts_in_repositories: Vec<Pattern>,
    #[serde(serialize_with = "serialize_patterns")]
    allow_repositories: Vec<Pattern>,
    #[serde(serialize_with = "serialize_patterns")]
    exclude_repositories: Vec<Pattern>,
    telemetry_oss_disabled: bool,
    telemetry_enterprise_dsn: Option<String>,
    disable_version_checks: bool,
    disable_auto_updates: bool,
    update_channel: UpdateChannel,
    upgrade_jitter_seconds: u64,
    minimum_package_upgrade_age_seconds: u64,
    feature_flags: FeatureFlags,
    api_base_url: String,
    prompt_storage: String,
    default_prompt_storage: Option<String>,
    #[serde(serialize_with = "serialize_masked_api_key")]
    api_key: Option<String>,
    quiet: bool,
    allow_superuser: bool,
    author: AuthorConfig,
    custom_attributes: HashMap<String, String>,
    git_ai_hooks: HashMap<String, Vec<String>>,
    codex_hooks_format: CodexHooksFormat,
    notes_backend: NotesBackendConfig,
    transcript_streaming_lookback_days: Option<u32>,
    max_checkpoint_file_size_bytes: usize,
    max_checkpoint_total_size_bytes: usize,
    max_checkpoint_total_lines: usize,
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

    fn from_str(input: &str) -> Option<Self> {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_repositories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_repositories: Option<Vec<String>>,
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
    pub upgrade_jitter_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_package_upgrade_age_seconds: Option<u64>,
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

static CONFIG: OnceLock<Config> = OnceLock::new();

const AUTHOR_CONFIG_CACHE_TTL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthorConfigCacheKey {
    config_path: Option<PathBuf>,
    config_fingerprint: Option<AuthorConfigFileFingerprint>,
    #[cfg(any(test, feature = "test-support"))]
    test_patch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthorConfigFileFingerprint {
    len: u64,
    hash: u64,
}

#[derive(Debug, Clone)]
struct CachedAuthorConfig {
    key: AuthorConfigCacheKey,
    loaded_at: Instant,
    author: AuthorConfig,
}

static AUTHOR_CONFIG_CACHE: OnceLock<Mutex<Option<CachedAuthorConfig>>> = OnceLock::new();

#[cfg(any(test, feature = "test-support"))]
static TEST_FEATURE_FLAGS_OVERRIDE: RwLock<Option<FeatureFlags>> = RwLock::new(None);

/// Serializable config patch for test overrides
/// All fields are optional to allow patching only specific properties
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_prompts_in_repositories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_oss_disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_version_checks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_auto_updates: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_jitter_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_package_upgrade_age_seconds: Option<u64>,
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

impl Config {
    /// Initialize the global configuration exactly once.
    /// Safe to call multiple times; subsequent calls are no-ops.
    #[allow(dead_code)]
    pub fn init() {
        let _ = CONFIG.get_or_init(build_config);
    }

    /// Access the global configuration. Lazily initializes if not already initialized.
    pub fn get() -> &'static Config {
        CONFIG.get_or_init(build_config)
    }

    /// Build a fresh config snapshot from disk/env without using the global cache.
    ///
    /// This is useful for long-lived daemon processes that must observe runtime
    /// config updates (for example, prompt sharing/privacy toggles).
    pub fn fresh() -> Self {
        build_config()
    }

    /// Return the fresh author override with a short process-local TTL.
    ///
    /// Author identity is consulted in hot paths such as checkpoint bursts and
    /// daemon replay. This avoids the global `Config::get()` singleton while
    /// still bounding repeated config file reads during a burst of operations.
    pub fn fresh_author_cached() -> AuthorConfig {
        let key = author_config_cache_key();
        let now = Instant::now();
        let cache = AUTHOR_CONFIG_CACHE.get_or_init(|| Mutex::new(None));
        if let Ok(mut guard) = cache.lock() {
            if let Some(cached) = guard.as_ref()
                && cached.key == key
                && now.duration_since(cached.loaded_at) < AUTHOR_CONFIG_CACHE_TTL
            {
                return cached.author.clone();
            }

            let author = build_config().author;
            *guard = Some(CachedAuthorConfig {
                key,
                loaded_at: now,
                author: author.clone(),
            });
            return author;
        }

        build_config().author
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn clear_author_config_cache_for_tests() {
        if let Some(cache) = AUTHOR_CONFIG_CACHE.get()
            && let Ok(mut guard) = cache.lock()
        {
            *guard = None;
        }
    }

    /// Returns the command to invoke git.
    pub fn git_cmd(&self) -> &str {
        &self.git_path
    }

    pub fn has_repository_filters(&self) -> bool {
        !self.allow_repositories.is_empty() || !self.exclude_repositories.is_empty()
    }

    pub fn is_allowed_repository(&self, repository: &Option<Repository>) -> bool {
        // Fetch remotes once and reuse for both exclude and allow checks
        let remotes = repository
            .as_ref()
            .and_then(|repo| repo.remotes_with_urls().ok());

        self.is_allowed_repository_with_remotes(remotes.as_ref())
    }

    /// Helper that accepts pre-fetched remotes to avoid multiple git operations
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn is_allowed_repository_with_remotes(
        &self,
        remotes: Option<&Vec<(String, String)>>,
    ) -> bool {
        if remotes.is_some_and(|remotes| {
            remotes.iter().any(|(_, remote_url)| {
                crate::diagnostic_sentinels::is_debug_self_check_remote_url(remote_url)
            })
        }) {
            return true;
        }

        // First check if repository is in exclusion list - exclusions take precedence
        if !self.exclude_repositories.is_empty()
            && let Some(remotes) = remotes
        {
            // If any remote matches the exclusion patterns, deny access
            if remotes
                .iter()
                .any(|remote| remote_matches_patterns(&self.exclude_repositories, &remote.1))
            {
                return false;
            }
        }

        // If allowlist is empty, allow everything (unless excluded above)
        if self.allow_repositories.is_empty() {
            return true;
        }

        // If allowlist is defined, only allow repos whose remotes match the patterns
        match remotes {
            Some(remotes) => remotes
                .iter()
                .any(|remote| remote_matches_patterns(&self.allow_repositories, &remote.1)),
            None => false, // Can't verify, deny by default when allowlist is active
        }
    }

    /// Returns true if prompts should be excluded (not shared) for the given repository.
    /// This uses a blacklist model: empty list = share everywhere, patterns = repos to exclude.
    /// Local repositories (no remotes) are only excluded if wildcard "*" pattern is present.
    pub fn should_exclude_prompts(&self, repository: &Option<Repository>) -> bool {
        let remotes = repository
            .as_ref()
            .and_then(|repo| repo.remotes_with_urls().ok());

        self.should_exclude_prompts_with_remotes(remotes.as_ref())
    }

    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn should_exclude_prompts_with_remotes(
        &self,
        remotes: Option<&Vec<(String, String)>>,
    ) -> bool {
        // Empty exclusion list = never exclude
        if self.exclude_prompts_in_repositories.is_empty() {
            return false;
        }

        if remotes.is_some_and(|remotes| {
            remotes.iter().any(|(_, remote_url)| {
                crate::diagnostic_sentinels::is_debug_self_check_remote_url(remote_url)
            })
        }) {
            return false;
        }

        // Check for wildcard "*" pattern - excludes ALL repos including local
        let has_wildcard = self
            .exclude_prompts_in_repositories
            .iter()
            .any(|pattern| pattern.as_str() == "*");
        if has_wildcard {
            return true;
        }

        match remotes {
            Some(remotes) => {
                if remotes.is_empty() {
                    // No remotes = local-only repo, not excluded (unless wildcard, handled above)
                    false
                } else {
                    // Has remotes - check if any match exclusion patterns
                    remotes.iter().any(|remote| {
                        remote_matches_patterns(&self.exclude_prompts_in_repositories, &remote.1)
                    })
                }
            }
            None => false, // Can't get remotes = don't exclude
        }
    }

    /// Returns true if OSS telemetry is disabled.
    pub fn is_telemetry_oss_disabled(&self) -> bool {
        self.telemetry_oss_disabled
    }

    /// Returns the telemetry_enterprise_dsn if set.
    pub fn telemetry_enterprise_dsn(&self) -> Option<&str> {
        self.telemetry_enterprise_dsn.as_deref()
    }

    pub fn version_checks_disabled(&self) -> bool {
        self.disable_version_checks
    }

    pub fn auto_updates_disabled(&self) -> bool {
        self.disable_auto_updates
    }

    pub fn update_channel(&self) -> UpdateChannel {
        self.update_channel
    }

    pub fn upgrade_jitter_seconds(&self) -> u64 {
        self.upgrade_jitter_seconds
    }

    pub fn minimum_package_upgrade_age_seconds(&self) -> u64 {
        self.minimum_package_upgrade_age_seconds
    }

    pub fn feature_flags(&self) -> &FeatureFlags {
        &self.feature_flags
    }

    /// Returns the API base URL
    pub fn api_base_url(&self) -> &str {
        &self.api_base_url
    }

    /// Returns the prompt storage mode: "default", "notes", or "local"
    /// - "default": Messages uploaded via CAS API
    /// - "notes": Messages stored in git notes
    /// - "local": Messages only stored in sqlite (not in notes, not uploaded)
    pub fn prompt_storage(&self) -> &str {
        &self.prompt_storage
    }

    /// Returns the effective prompt storage mode for a given repository.
    ///
    /// The resolution order is:
    /// 1. If repo matches exclude_prompts_in_repositories → always "local" (exclusion wins)
    /// 2. If include_prompts_in_repositories is empty → use prompt_storage (legacy behavior)
    /// 3. If repo matches include_prompts_in_repositories → use prompt_storage
    /// 4. If repo doesn't match include list → use default_prompt_storage, or "local" if not set
    ///
    /// This enables two use cases:
    /// - User A: git-ai everywhere, CAS for work repos, notes for others
    ///   (prompt_storage="default", include_prompts=["positron-ai/*"], default_prompt_storage="notes")
    /// - User B: git-ai only in work repos (via allow_repositories), CAS there
    ///   (prompt_storage="default", no include list needed)
    pub fn effective_prompt_storage(&self, repository: &Option<Repository>) -> PromptStorageMode {
        // Step 1: Check exclusion list first (deny always wins)
        if self.should_exclude_prompts(repository) {
            return PromptStorageMode::Local;
        }

        // Step 2: If no include list, use the global prompt_storage (legacy behavior)
        if self.include_prompts_in_repositories.is_empty() {
            return self
                .prompt_storage
                .parse::<PromptStorageMode>()
                .unwrap_or(PromptStorageMode::Default);
        }

        // Step 3: Check if repo matches include list
        let remotes = repository
            .as_ref()
            .and_then(|repo| repo.remotes_with_urls().ok());

        let matches_include = match &remotes {
            Some(remotes) if !remotes.is_empty() => {
                // Has remotes - check if any match inclusion patterns
                remotes.iter().any(|remote| {
                    remote_matches_patterns(&self.include_prompts_in_repositories, &remote.1)
                })
            }
            _ => {
                // No remotes or no repository - check for wildcard "*" in include patterns
                self.include_prompts_in_repositories
                    .iter()
                    .any(|pattern| pattern.as_str() == "*")
            }
        };

        if matches_include {
            // Step 3a: Repo is in include list → use primary prompt_storage
            self.prompt_storage
                .parse::<PromptStorageMode>()
                .unwrap_or(PromptStorageMode::Default)
        } else {
            // Step 4: Repo not in include list → use fallback
            self.default_prompt_storage
                .as_ref()
                .and_then(|s| s.parse::<PromptStorageMode>().ok())
                .unwrap_or(PromptStorageMode::Local) // Safe default
        }
    }

    /// Returns the API key if configured
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// Returns the notes backend config.
    pub fn notes_backend(&self) -> &NotesBackendConfig {
        &self.notes_backend
    }

    /// Returns the notes backend kind.
    pub fn notes_backend_kind(&self) -> NotesBackendKind {
        self.notes_backend.kind
    }

    /// Returns the configured notes backend URL, or `None` if unset.
    ///
    /// Callers must handle `None` explicitly — typically by skipping the operation when the HTTP backend
    /// is enabled but no URL has been configured.
    pub fn notes_backend_url(&self) -> Option<&str> {
        self.notes_backend.backend_url.as_deref()
    }

    /// Returns true when the HTTP notes backend is active.
    pub fn notes_backend_enabled(&self) -> bool {
        matches!(self.notes_backend.kind, NotesBackendKind::Http)
    }

    pub fn transcript_streaming_lookback_days(&self) -> Option<u32> {
        self.transcript_streaming_lookback_days
    }

    /// Returns the per-file size limit for checkpoint content reads.
    pub fn max_checkpoint_file_size_bytes(&self) -> usize {
        self.max_checkpoint_file_size_bytes
    }

    /// Returns the total byte budget for content in one checkpoint request.
    pub fn max_checkpoint_total_size_bytes(&self) -> usize {
        self.max_checkpoint_total_size_bytes
    }

    /// Returns the total line budget for content in one checkpoint request.
    pub fn max_checkpoint_total_lines(&self) -> usize {
        self.max_checkpoint_total_lines
    }

    /// Returns true if quiet mode is enabled (suppresses chart output after commits)
    pub fn is_quiet(&self) -> bool {
        self.quiet
    }

    pub fn allow_superuser(&self) -> bool {
        self.allow_superuser
    }

    /// Returns the configured git-ai author override.
    pub fn author(&self) -> &AuthorConfig {
        &self.author
    }

    /// Returns the custom attributes map (from config file + env var override).
    pub fn custom_attributes(&self) -> &HashMap<String, String> {
        &self.custom_attributes
    }

    /// Returns all configured git-ai hook commands.
    pub fn git_ai_hooks(&self) -> &HashMap<String, Vec<String>> {
        &self.git_ai_hooks
    }

    /// Returns configured shell commands for a specific hook.
    pub fn git_ai_hook_commands(&self, hook_name: &str) -> Option<&Vec<String>> {
        self.git_ai_hooks.get(hook_name)
    }

    pub fn codex_hooks_format(&self) -> CodexHooksFormat {
        self.codex_hooks_format
    }

    /// Serialize the effective runtime config into pretty JSON.
    /// Sensitive values are redacted via field serializers.
    pub fn to_printable_json_pretty(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize runtime config: {}", e))
    }

    /// Override feature flags for testing purposes.
    /// Only available when the `test-support` feature is enabled or in test mode.
    /// Must be `pub` to work with integration tests in the `tests/` directory.
    #[cfg(any(test, feature = "test-support"))]
    #[allow(dead_code)]
    pub fn set_test_feature_flags(flags: FeatureFlags) {
        let mut override_flags = TEST_FEATURE_FLAGS_OVERRIDE
            .write()
            .expect("Failed to acquire write lock on test feature flags");
        *override_flags = Some(flags);
    }

    /// Clear any feature flag overrides.
    /// Only available when the `test-support` feature is enabled or in test mode.
    /// This should be called in test cleanup to reset to default behavior.
    #[cfg(any(test, feature = "test-support"))]
    #[allow(dead_code)]
    pub fn clear_test_feature_flags() {
        let mut override_flags = TEST_FEATURE_FLAGS_OVERRIDE
            .write()
            .expect("Failed to acquire write lock on test feature flags");
        *override_flags = None;
    }

    /// Get feature flags, checking for test overrides first.
    /// In test mode, this will return overridden flags if set, otherwise the normal flags.
    #[cfg(any(test, feature = "test-support"))]
    pub fn get_feature_flags(&self) -> FeatureFlags {
        let override_flags = TEST_FEATURE_FLAGS_OVERRIDE
            .read()
            .expect("Failed to acquire read lock on test feature flags");
        override_flags
            .clone()
            .unwrap_or_else(|| self.feature_flags.clone())
    }

    /// Get feature flags (non-test version, just returns a reference).
    #[cfg(not(any(test, feature = "test-support")))]
    pub fn get_feature_flags(&self) -> &FeatureFlags {
        &self.feature_flags
    }
}

fn serialize_patterns<S>(patterns: &[Pattern], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let as_strings: Vec<&str> = patterns.iter().map(Pattern::as_str).collect();
    as_strings.serialize(serializer)
}

fn remote_matches_patterns(patterns: &[Pattern], remote_url: &str) -> bool {
    let remote_candidates = repo_remote_match_candidates(remote_url);
    patterns.iter().any(|pattern| {
        repo_pattern_match_candidates(pattern.as_str())
            .iter()
            .filter_map(|candidate| Pattern::new(candidate).ok())
            .any(|candidate_pattern| {
                remote_candidates
                    .iter()
                    .any(|candidate| candidate_pattern.matches(candidate))
            })
    })
}

fn repo_pattern_match_candidates(value: &str) -> Vec<String> {
    let mut candidates = vec![value.trim().to_string()];

    if let Some((host, path_variants)) = repo_match_parts(value) {
        for path in path_variants {
            candidates.push(format!("{}/{}", host, path));
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

fn repo_remote_match_candidates(value: &str) -> Vec<String> {
    let mut candidates = vec![value.trim().to_string()];

    if let Some((host, path_variants)) = repo_match_parts(value) {
        for path in path_variants {
            candidates.push(format!("{}/{}", host, path));
            candidates.push(path);
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

fn repo_match_parts(value: &str) -> Option<(String, Vec<String>)> {
    let value = value.trim();

    if let Some((_, rest)) = value.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        return Some((
            normalize_repo_authority(authority)?,
            normalize_repo_path_variants(path)?,
        ));
    }

    let (user_host, path) = value.split_once(':')?;
    if value.starts_with('/') || !user_host.contains('@') || path.is_empty() {
        return None;
    }

    let (_, host) = user_host.rsplit_once('@')?;
    Some((
        normalize_repo_host(host)?,
        normalize_repo_path_variants(path)?,
    ))
}

fn normalize_repo_authority(authority: &str) -> Option<String> {
    let host = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    normalize_repo_host(host)
}

fn normalize_repo_host(host: &str) -> Option<String> {
    let host = strip_repo_host_port(host.trim());
    if host.is_empty() {
        return None;
    }

    let host = host.to_ascii_lowercase();
    if matches!(host.as_str(), "dev.azure.com" | "ssh.dev.azure.com") {
        Some("azure".to_string())
    } else {
        Some(host)
    }
}

fn strip_repo_host_port(host: &str) -> &str {
    if let Some(stripped) = strip_bracketed_host_port(host) {
        return stripped;
    }

    let Some((host_without_port, port)) = host.rsplit_once(':') else {
        return host;
    };
    if host_without_port.contains(':') || port.is_empty() {
        host
    } else {
        host_without_port
    }
}

fn strip_bracketed_host_port(host: &str) -> Option<&str> {
    let rest = host.strip_prefix('[')?;
    let bracket_index = rest.find(']')?;
    let bracket_end = bracket_index + 2;
    let after_bracket = host.get(bracket_end..)?;

    if after_bracket.is_empty() || after_bracket.starts_with(':') {
        Some(&host[..bracket_end])
    } else {
        None
    }
}

fn normalize_repo_path_variants(path: &str) -> Option<Vec<String>> {
    let path = path
        .split(['?', '#'])
        .next()
        .unwrap_or(path)
        .trim_start_matches('/')
        .trim_end_matches('/')
        .trim_end_matches(".git");

    if path.is_empty() {
        return None;
    }

    let mut variants = vec![path.to_string()];
    let segments: Vec<&str> = path.split('/').collect();
    if segments.first() == Some(&"v3") && segments.len() > 1 {
        variants.push(segments[1..].join("/"));
    }
    if let Some(git_segment_index) = segments.iter().position(|segment| *segment == "_git")
        && git_segment_index > 0
        && git_segment_index + 1 < segments.len()
    {
        let mut without_git_segment = segments.clone();
        without_git_segment.remove(git_segment_index);
        variants.push(without_git_segment.join("/"));
    }

    variants.sort();
    variants.dedup();
    Some(variants)
}

fn serialize_masked_api_key<S>(api_key: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let masked = api_key.as_ref().map(|key| {
        let chars: Vec<char> = key.chars().collect();
        if chars.len() > 8 {
            let prefix: String = chars[..4].iter().collect();
            let suffix: String = chars[chars.len() - 4..].iter().collect();
            format!("{}...{}", prefix, suffix)
        } else {
            "****".to_string()
        }
    });
    masked.serialize(serializer)
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn author_config_cache_key() -> AuthorConfigCacheKey {
    let config_path = config_file_path();
    let config_fingerprint = config_path
        .as_ref()
        .and_then(|path| author_config_file_fingerprint(path));

    AuthorConfigCacheKey {
        config_path,
        config_fingerprint,
        #[cfg(any(test, feature = "test-support"))]
        test_patch: env::var("GIT_AI_TEST_CONFIG_PATCH").ok(),
    }
}

fn author_config_file_fingerprint(path: &Path) -> Option<AuthorConfigFileFingerprint> {
    let data = fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    Some(AuthorConfigFileFingerprint {
        len: data.len() as u64,
        hash: hasher.finish(),
    })
}

fn normalize_upgrade_jitter_seconds(value: u64) -> u64 {
    if value > MAX_UPGRADE_JITTER_SECONDS {
        eprintln!(
            "Warning: upgrade_jitter_seconds cannot exceed {} seconds; using {}",
            MAX_UPGRADE_JITTER_SECONDS, MAX_UPGRADE_JITTER_SECONDS
        );
        MAX_UPGRADE_JITTER_SECONDS
    } else {
        value
    }
}

fn build_config() -> Config {
    let file_cfg = load_file_config();
    let exclude_prompts_in_repositories = file_cfg
        .as_ref()
        .and_then(|c| c.exclude_prompts_in_repositories.clone())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|pattern_str| {
            Pattern::new(&pattern_str)
                .map_err(|e| {
                    eprintln!(
                        "Warning: Invalid glob pattern in exclude_prompts_in_repositories '{}': {}",
                        pattern_str, e
                    );
                })
                .ok()
        })
        .collect();
    let include_prompts_in_repositories = file_cfg
        .as_ref()
        .and_then(|c| c.include_prompts_in_repositories.clone())
        .unwrap_or(vec![])
        .into_iter()
        .filter_map(|pattern_str| {
            Pattern::new(&pattern_str)
                .map_err(|e| {
                    eprintln!(
                        "Warning: Invalid glob pattern in include_prompts_in_repositories '{}': {}",
                        pattern_str, e
                    );
                })
                .ok()
        })
        .collect();
    let allow_repositories = file_cfg
        .as_ref()
        .and_then(|c| c.allow_repositories.clone())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|pattern_str| {
            Pattern::new(&pattern_str)
                .map_err(|e| {
                    eprintln!(
                        "Warning: Invalid glob pattern in allow_repositories '{}': {}",
                        pattern_str, e
                    );
                })
                .ok()
        })
        .collect();
    let exclude_repositories = file_cfg
        .as_ref()
        .and_then(|c| c.exclude_repositories.clone())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|pattern_str| {
            Pattern::new(&pattern_str)
                .map_err(|e| {
                    eprintln!(
                        "Warning: Invalid glob pattern in exclude_repositories '{}': {}",
                        pattern_str, e
                    );
                })
                .ok()
        })
        .collect();
    let telemetry_oss_disabled = file_cfg
        .as_ref()
        .and_then(|c| c.telemetry_oss.clone())
        .filter(|s| s == "off")
        .is_some();
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
    let upgrade_jitter_seconds = normalize_upgrade_jitter_seconds(
        file_cfg
            .as_ref()
            .and_then(|c| c.upgrade_jitter_seconds)
            .unwrap_or(0),
    );
    let minimum_package_upgrade_age_seconds = file_cfg
        .as_ref()
        .and_then(|c| c.minimum_package_upgrade_age_seconds)
        .unwrap_or(0);

    let git_path = resolve_git_path(&file_cfg);

    // Build feature flags from file config
    let feature_flags = build_feature_flags(&file_cfg);

    // Get API base URL from config, env var, or default
    let api_base_url = file_cfg
        .as_ref()
        .and_then(|c| c.api_base_url.clone())
        .or_else(|| env::var("GIT_AI_API_BASE_URL").ok())
        .unwrap_or_else(|| DEFAULT_API_BASE_URL.to_string());

    // Get prompt_storage setting (defaults to "default")
    // Valid values: "default", "notes", "local"
    let prompt_storage = file_cfg
        .as_ref()
        .and_then(|c| c.prompt_storage.clone())
        .unwrap_or_else(|| "default".to_string());
    let prompt_storage = match prompt_storage.as_str() {
        "default" | "notes" | "local" => prompt_storage,
        other => {
            eprintln!(
                "Warning: Invalid prompt_storage value '{}', using 'default'",
                other
            );
            "default".to_string()
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
            _ => None,
        });
    let url_from_env = env::var("GIT_AI_NOTES_BACKEND_URL").ok();

    let notes_backend = NotesBackendConfig {
        kind: kind_from_env
            .or_else(|| file_backend.as_ref().map(|b| b.kind))
            .unwrap_or(NotesBackendKind::GitNotes),
        backend_url: url_from_env
            .or_else(|| file_backend.as_ref().and_then(|b| b.backend_url.clone())),
    };

    // Transcript streaming lookback: env > file > default (7 days). 0 means unlimited (None).
    let transcript_streaming_lookback_days = env::var("GIT_AI_TRANSCRIPT_STREAMING_LOOKBACK_DAYS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .or_else(|| {
            file_cfg
                .as_ref()
                .and_then(|c| c.transcript_streaming_lookback_days)
        })
        .or(Some(7))
        .and_then(|v| if v == 0 { None } else { Some(v) });

    // Checkpoint content limits: env > file > defaults.
    let max_checkpoint_file_size_bytes = env::var("GIT_AI_MAX_CHECKPOINT_FILE_SIZE_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .or_else(|| {
            file_cfg
                .as_ref()
                .and_then(|c| c.max_checkpoint_file_size_bytes)
        })
        .unwrap_or(DEFAULT_MAX_CHECKPOINT_FILE_SIZE_BYTES);

    let max_checkpoint_total_size_bytes = env::var("GIT_AI_MAX_CHECKPOINT_TOTAL_SIZE_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .or_else(|| {
            file_cfg
                .as_ref()
                .and_then(|c| c.max_checkpoint_total_size_bytes)
        })
        .unwrap_or(DEFAULT_MAX_CHECKPOINT_TOTAL_SIZE_BYTES);

    let max_checkpoint_total_lines = env::var("GIT_AI_MAX_CHECKPOINT_TOTAL_LINES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .or_else(|| file_cfg.as_ref().and_then(|c| c.max_checkpoint_total_lines))
        .unwrap_or(DEFAULT_MAX_CHECKPOINT_TOTAL_LINES);

    #[cfg(any(test, feature = "test-support"))]
    {
        let mut config = Config {
            git_path,
            exclude_prompts_in_repositories,
            include_prompts_in_repositories,
            allow_repositories,
            exclude_repositories,
            telemetry_oss_disabled,
            telemetry_enterprise_dsn,
            disable_version_checks,
            disable_auto_updates,
            update_channel,
            upgrade_jitter_seconds,
            minimum_package_upgrade_age_seconds,
            feature_flags,
            api_base_url,
            prompt_storage,
            default_prompt_storage,
            api_key,
            quiet,
            allow_superuser,
            author,
            custom_attributes: custom_attributes.clone(),
            git_ai_hooks: git_ai_hooks.clone(),
            codex_hooks_format,
            notes_backend,
            transcript_streaming_lookback_days,
            max_checkpoint_file_size_bytes,
            max_checkpoint_total_size_bytes,
            max_checkpoint_total_lines,
        };
        apply_test_config_patch(&mut config);
        config
    }

    #[cfg(not(any(test, feature = "test-support")))]
    Config {
        git_path,
        exclude_prompts_in_repositories,
        include_prompts_in_repositories,
        allow_repositories,
        exclude_repositories,
        telemetry_oss_disabled,
        telemetry_enterprise_dsn,
        disable_version_checks,
        disable_auto_updates,
        update_channel,
        upgrade_jitter_seconds,
        minimum_package_upgrade_age_seconds,
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
    }
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

fn load_file_config() -> Option<FileConfig> {
    let path = config_file_path()?;
    let data = fs::read(&path).ok()?;
    parse_file_config_bytes(&data).ok()
}

fn parse_file_config_bytes(data: &[u8]) -> Result<FileConfig, serde_json::Error> {
    // Windows PowerShell 5.1 writes UTF-8 with BOM by default for `Out-File -Encoding UTF8`.
    // Tolerate BOM-prefixed config files so upgrades/installers don't brick config parsing.
    let data = data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data);
    serde_json::from_slice::<FileConfig>(data)
}

fn config_file_path() -> Option<PathBuf> {
    Some(home_dir().join(".git-ai").join("config.json"))
}

/// Public accessor for config file path
#[allow(dead_code)]
pub fn config_file_path_public() -> Option<PathBuf> {
    config_file_path()
}

/// Returns the path to the git-ai base directory (~/.git-ai)
pub fn git_ai_dir_path() -> Option<PathBuf> {
    Some(home_dir().join(".git-ai"))
}

/// Returns the path to the internal state directory (~/.git-ai/internal)
/// This is where git-ai stores internal files like distinct_id, update_check, etc.
pub fn internal_dir_path() -> Option<PathBuf> {
    git_ai_dir_path().map(|dir| dir.join("internal"))
}

/// Returns the path to the skills directory (~/.git-ai/skills)
/// This is where git-ai installs skills for Claude Code and other agents
pub fn skills_dir_path() -> Option<PathBuf> {
    git_ai_dir_path().map(|dir| dir.join("skills"))
}

/// Public accessor for ID file path (~/.git-ai/internal/distinct_id)
pub fn id_file_path() -> Option<PathBuf> {
    internal_dir_path().map(|dir| dir.join("distinct_id"))
}

/// Cache for the distinct_id to avoid repeated file reads
static DISTINCT_ID: OnceLock<String> = OnceLock::new();

/// Get or create the distinct_id (UUID) from ~/.git-ai/internal/distinct_id
/// If the file doesn't exist, generates a new UUID and writes it to the file.
/// The result is cached for the lifetime of the process.
pub fn get_or_create_distinct_id() -> String {
    DISTINCT_ID
        .get_or_init(|| {
            let id_path = match id_file_path() {
                Some(path) => path,
                None => return "unknown".to_string(),
            };

            // Try to read existing ID
            if let Ok(existing_id) = fs::read_to_string(&id_path) {
                let trimmed = existing_id.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }

            // Generate new UUID
            let new_id = crate::uuid::generate_v4();

            // Ensure directory exists
            if let Some(parent) = id_path.parent() {
                let _ = fs::create_dir_all(parent);
            }

            // Write the new ID to file
            if let Err(e) = fs::write(&id_path, &new_id) {
                eprintln!("Warning: Failed to write distinct_id file: {}", e);
            }

            new_id
        })
        .clone()
}

/// Returns the path to the update check cache file (~/.git-ai/internal/update_check)
pub fn update_check_path() -> Option<PathBuf> {
    internal_dir_path().map(|dir| dir.join("update_check"))
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
fn path_is_git_ai_binary(path: &Path) -> bool {
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
fn apply_test_config_patch(config: &mut Config) {
    if let Ok(patch_json) = env::var("GIT_AI_TEST_CONFIG_PATCH")
        && let Ok(patch) = serde_json::from_str::<ConfigPatch>(&patch_json)
    {
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
        if let Some(disable_version_checks) = patch.disable_version_checks {
            config.disable_version_checks = disable_version_checks;
        }
        if let Some(disable_auto_updates) = patch.disable_auto_updates {
            config.disable_auto_updates = disable_auto_updates;
        }
        if let Some(upgrade_jitter_seconds) = patch.upgrade_jitter_seconds {
            config.upgrade_jitter_seconds =
                normalize_upgrade_jitter_seconds(upgrade_jitter_seconds);
        }
        if let Some(minimum_package_upgrade_age_seconds) = patch.minimum_package_upgrade_age_seconds
        {
            config.minimum_package_upgrade_age_seconds = minimum_package_upgrade_age_seconds;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_config(
        allow_repositories: Vec<String>,
        exclude_repositories: Vec<String>,
    ) -> Config {
        Config {
            git_path: "/usr/bin/git".to_string(),
            exclude_prompts_in_repositories: vec![],
            include_prompts_in_repositories: vec![],
            allow_repositories: allow_repositories
                .into_iter()
                .filter_map(|s| Pattern::new(&s).ok())
                .collect(),
            exclude_repositories: exclude_repositories
                .into_iter()
                .filter_map(|s| Pattern::new(&s).ok())
                .collect(),
            telemetry_oss_disabled: false,
            telemetry_enterprise_dsn: None,
            disable_version_checks: false,
            disable_auto_updates: false,
            update_channel: UpdateChannel::Latest,
            upgrade_jitter_seconds: 0,
            minimum_package_upgrade_age_seconds: 0,
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

        // Test with None repository - should return false when allowlist is active
        assert!(!config.is_allowed_repository(&None));
    }

    #[test]
    fn test_empty_allowlist_allows_everything() {
        let config = create_test_config(vec![], vec![]);

        // With empty allowlist, should allow everything
        assert!(config.is_allowed_repository(&None));
    }

    #[test]
    fn test_exclude_without_allow() {
        let config =
            create_test_config(vec![], vec!["https://github.com/excluded/repo".to_string()]);

        // With empty allowlist but exclusions, should allow everything (exclusions only matter when checking remotes)
        assert!(config.is_allowed_repository(&None));
    }

    #[test]
    fn test_allow_without_exclude() {
        let config =
            create_test_config(vec!["https://github.com/allowed/repo".to_string()], vec![]);

        // With allowlist but no exclusions, should deny when no repository provided
        assert!(!config.is_allowed_repository(&None));
    }

    #[test]
    fn test_glob_pattern_wildcard_in_allow() {
        let config = create_test_config(vec!["https://github.com/myorg/*".to_string()], vec![]);

        // Test that the pattern would match (note: we can't easily test with real Repository objects,
        // but the pattern compilation is tested by the fact that create_test_config succeeds)
        assert!(!config.allow_repositories.is_empty());
        assert!(config.allow_repositories[0].matches("https://github.com/myorg/repo1"));
        assert!(config.allow_repositories[0].matches("https://github.com/myorg/repo2"));
        assert!(!config.allow_repositories[0].matches("https://github.com/other/repo"));
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
        assert!(!config.allow_repositories.is_empty());
        assert!(config.allow_repositories[0].matches("https://github.com/exact/match"));
        assert!(!config.allow_repositories[0].matches("https://github.com/exact/other"));
    }

    #[test]
    fn test_complex_glob_patterns() {
        let config = create_test_config(vec!["*@github.com:company/*".to_string()], vec![]);

        // Test more complex patterns with wildcards
        assert!(!config.allow_repositories.is_empty());
        assert!(config.allow_repositories[0].matches("git@github.com:company/repo"));
        assert!(config.allow_repositories[0].matches("user@github.com:company/project"));
        assert!(!config.allow_repositories[0].matches("git@github.com:other/repo"));
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
        let https_patterns =
            vec![Pattern::new("https://dev.azure.com/acme/widgets/_git/*").unwrap()];
        assert!(remote_matches_patterns(
            &https_patterns,
            "ssh://git@ssh.dev.azure.com:22/v3/acme/widgets/service"
        ));

        let ssh_patterns =
            vec![Pattern::new("ssh://git@ssh.dev.azure.com/v3/acme/widgets/*").unwrap()];
        assert!(remote_matches_patterns(
            &ssh_patterns,
            "https://dev.azure.com/acme/widgets/_git/service"
        ));
    }

    // Tests for exclude_prompts_in_repositories (blacklist)

    fn create_test_config_with_exclude_prompts(exclude_prompts_patterns: Vec<String>) -> Config {
        Config {
            git_path: "/usr/bin/git".to_string(),
            exclude_prompts_in_repositories: exclude_prompts_patterns
                .into_iter()
                .filter_map(|s| Pattern::new(&s).ok())
                .collect(),
            include_prompts_in_repositories: vec![],
            allow_repositories: vec![],
            exclude_repositories: vec![],
            telemetry_oss_disabled: false,
            telemetry_enterprise_dsn: None,
            disable_version_checks: false,
            disable_auto_updates: false,
            update_channel: UpdateChannel::Latest,
            upgrade_jitter_seconds: 0,
            minimum_package_upgrade_age_seconds: 0,
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
        let config =
            create_test_config_with_exclude_prompts(vec!["https://github.com/*".to_string()]);

        // Even with patterns, no repository provided = don't exclude (can't verify)
        assert!(!config.should_exclude_prompts(&None));
    }

    #[test]
    fn test_should_exclude_prompts_pattern_matching() {
        let config =
            create_test_config_with_exclude_prompts(vec!["https://github.com/myorg/*".to_string()]);

        // Test that pattern is compiled correctly
        assert!(!config.exclude_prompts_in_repositories.is_empty());
        assert!(
            config.exclude_prompts_in_repositories[0].matches("https://github.com/myorg/repo1")
        );
        assert!(
            config.exclude_prompts_in_repositories[0].matches("https://github.com/myorg/repo2")
        );
        assert!(
            !config.exclude_prompts_in_repositories[0].matches("https://github.com/other/repo")
        );
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
        let config = create_test_config_with_exclude_prompts(vec![
            "https://github.com/private/*".to_string(),
        ]);

        // Pattern should match private repos (to exclude)
        assert!(
            config.exclude_prompts_in_repositories[0].matches("https://github.com/private/repo")
        );
        // Pattern should not match other repos
        assert!(
            !config.exclude_prompts_in_repositories[0].matches("https://github.com/public/repo")
        );
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
            git_path: "/usr/bin/git".to_string(),
            exclude_prompts_in_repositories: exclude_patterns
                .into_iter()
                .filter_map(|s| Pattern::new(&s).ok())
                .collect(),
            include_prompts_in_repositories: include_patterns
                .into_iter()
                .filter_map(|s| Pattern::new(&s).ok())
                .collect(),
            allow_repositories: vec![],
            exclude_repositories: vec![],
            telemetry_oss_disabled: false,
            telemetry_enterprise_dsn: None,
            disable_version_checks: false,
            disable_auto_updates: false,
            update_channel: UpdateChannel::Latest,
            upgrade_jitter_seconds: 0,
            minimum_package_upgrade_age_seconds: 0,
            feature_flags: FeatureFlags::default(),
            api_base_url: DEFAULT_API_BASE_URL.to_string(),
            prompt_storage: prompt_storage.to_string(),
            default_prompt_storage: default_prompt_storage.map(|s| s.to_string()),
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
            config.include_prompts_in_repositories[0]
                .matches("https://github.com/positron-ai/repo1")
        );
        assert!(
            config.include_prompts_in_repositories[0]
                .matches("https://github.com/positron-ai/project")
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
    fn test_upgrade_timing_defaults_are_disabled() {
        let config = create_test_config(vec![], vec![]);
        assert_eq!(config.upgrade_jitter_seconds(), 0);
        assert_eq!(config.minimum_package_upgrade_age_seconds(), 0);
    }

    #[test]
    fn test_excluded_repo_with_remotes() {
        let config = create_test_config(vec![], vec!["https://github.com/excluded/*".to_string()]);
        let remotes = vec![(
            "origin".to_string(),
            "https://github.com/excluded/repo".to_string(),
        )];
        assert!(!config.is_allowed_repository_with_remotes(Some(&remotes)));
    }

    #[test]
    fn test_allowed_repo_not_excluded_with_remotes() {
        let config = create_test_config(vec![], vec!["https://github.com/excluded/*".to_string()]);
        let remotes = vec![(
            "origin".to_string(),
            "https://github.com/allowed/repo".to_string(),
        )];
        assert!(config.is_allowed_repository_with_remotes(Some(&remotes)));
    }

    #[test]
    fn test_allowlist_with_remotes() {
        let config = create_test_config(vec!["https://github.com/myorg/*".to_string()], vec![]);
        let remotes = vec![(
            "origin".to_string(),
            "https://github.com/myorg/project".to_string(),
        )];
        assert!(config.is_allowed_repository_with_remotes(Some(&remotes)));
    }

    #[test]
    fn test_allowlist_matches_ssh_url_remote_with_scp_pattern() {
        let config = create_test_config(vec!["git@github.com:myorg/*".to_string()], vec![]);
        let remotes = vec![(
            "origin".to_string(),
            "ssh://git@github.com/myorg/project".to_string(),
        )];
        assert!(config.is_allowed_repository_with_remotes(Some(&remotes)));
    }

    #[test]
    fn test_allowlist_denies_unmatched_remotes() {
        let config = create_test_config(vec!["https://github.com/myorg/*".to_string()], vec![]);
        let remotes = vec![(
            "origin".to_string(),
            "https://github.com/other/project".to_string(),
        )];
        assert!(!config.is_allowed_repository_with_remotes(Some(&remotes)));
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
        assert!(!config.is_allowed_repository_with_remotes(Some(&remotes)));
    }

    #[test]
    fn test_exclusion_matches_scp_remote_with_ssh_url_pattern() {
        let config =
            create_test_config(vec![], vec!["ssh://git@github.com/excluded/*".to_string()]);
        let remotes = vec![(
            "origin".to_string(),
            "git@github.com:excluded/repo".to_string(),
        )];
        assert!(!config.is_allowed_repository_with_remotes(Some(&remotes)));
    }

    #[test]
    fn test_no_remotes_allowed_when_only_excludes() {
        let config = create_test_config(vec![], vec!["https://github.com/excluded/*".to_string()]);
        assert!(config.is_allowed_repository_with_remotes(None));
    }

    #[test]
    fn test_no_remotes_denied_when_allowlist_active() {
        let config = create_test_config(vec!["https://github.com/myorg/*".to_string()], vec![]);
        assert!(!config.is_allowed_repository_with_remotes(None));
    }

    #[test]
    fn test_empty_remotes_treated_as_no_match_for_exclusion() {
        let config = create_test_config(vec![], vec!["https://github.com/excluded/*".to_string()]);
        let remotes: Vec<(String, String)> = vec![];
        assert!(config.is_allowed_repository_with_remotes(Some(&remotes)));
    }

    #[test]
    fn test_multiple_remotes_one_excluded() {
        let config = create_test_config(vec![], vec!["https://github.com/excluded/*".to_string()]);
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
        assert!(!config.is_allowed_repository_with_remotes(Some(&remotes)));
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
    fn test_path_is_git_ai_binary_symlink_to_git_ai() {
        // A symlink `git → git-ai` should be detected as git-ai.
        let dir = tempfile::tempdir().unwrap();
        let git_ai = dir.path().join("git-ai");
        fs::write(&git_ai, "fake-binary").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&git_ai, dir.path().join("git")).unwrap();
        #[cfg(unix)]
        assert!(path_is_git_ai_binary(&dir.path().join("git")));
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
        assert!(!path_is_git_ai_binary(&real_git));
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
            assert!(path_is_git_ai_binary(&git));
        }
    }

    // --- NotesBackendConfig tests ---

    #[test]
    fn test_notes_backend_config_default_is_git_notes() {
        let cfg = NotesBackendConfig::default();
        assert_eq!(cfg.kind, NotesBackendKind::GitNotes);
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
                _ => None,
            }
        };

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
}
