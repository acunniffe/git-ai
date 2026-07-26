use git_ai::api::client::ApiContext;
use git_ai::auth::client::OAuthClient;
/// Tests for config refresh behavior (Config::fresh() vs Config::get())
/// These tests verify that Config::fresh() reads from disk while Config::get() uses cached values.
use git_ai::config::{Config, load_file_config_public, save_file_config};
use serial_test::serial;
use std::env;
use tempfile::TempDir;

/// RAII guard that redirects home-directory env vars to a temp path for the duration of a test,
/// then restores them on drop.  Handles both Unix (`HOME`) and Windows (`USERPROFILE`,
/// `HOMEDRIVE`, `HOMEPATH`) so that `home_dir()` in src/mdm/utils.rs resolves to the temp dir
/// on all platforms.
struct HomeEnvGuard {
    original_home: Option<String>,
    #[cfg(windows)]
    original_userprofile: Option<String>,
    #[cfg(windows)]
    original_homedrive: Option<String>,
    #[cfg(windows)]
    original_homepath: Option<String>,
}

impl HomeEnvGuard {
    fn set(path: &std::path::Path) -> Self {
        let original_home = env::var("HOME").ok();
        #[cfg(windows)]
        let original_userprofile = env::var("USERPROFILE").ok();
        #[cfg(windows)]
        let original_homedrive = env::var("HOMEDRIVE").ok();
        #[cfg(windows)]
        let original_homepath = env::var("HOMEPATH").ok();

        unsafe {
            env::set_var("HOME", path);
            #[cfg(windows)]
            {
                env::set_var("USERPROFILE", path);
                env::remove_var("HOMEDRIVE");
                env::remove_var("HOMEPATH");
            }
        }

        HomeEnvGuard {
            original_home,
            #[cfg(windows)]
            original_userprofile,
            #[cfg(windows)]
            original_homedrive,
            #[cfg(windows)]
            original_homepath,
        }
    }
}

impl Drop for HomeEnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.original_home {
                Some(v) => env::set_var("HOME", v),
                None => env::remove_var("HOME"),
            }
            #[cfg(windows)]
            {
                match &self.original_userprofile {
                    Some(v) => env::set_var("USERPROFILE", v),
                    None => env::remove_var("USERPROFILE"),
                }
                match &self.original_homedrive {
                    Some(v) => env::set_var("HOMEDRIVE", v),
                    None => env::remove_var("HOMEDRIVE"),
                }
                match &self.original_homepath {
                    Some(v) => env::set_var("HOMEPATH", v),
                    None => env::remove_var("HOMEPATH"),
                }
            }
        }
    }
}

/// RAII guard that clears the `GIT_AI_API_KEY` env var for the duration of a
/// test, restoring the prior value on drop. `Config` resolves the API key from
/// `GIT_AI_API_KEY` ahead of the on-disk config, so tests that
/// assert on the file-config api_key must clear any ambient value to remain
/// hermetic (otherwise they fail on a developer machine that has the var set,
/// while passing in CI's clean environment).
struct ApiKeyEnvGuard {
    original: Option<String>,
}

impl ApiKeyEnvGuard {
    fn clear() -> Self {
        let original = env::var("GIT_AI_API_KEY").ok();
        unsafe { env::remove_var("GIT_AI_API_KEY") };
        ApiKeyEnvGuard { original }
    }
}

impl Drop for ApiKeyEnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.original {
                Some(v) => env::set_var("GIT_AI_API_KEY", v),
                None => env::remove_var("GIT_AI_API_KEY"),
            }
        }
    }
}

/// Test that Config::fresh() picks up changes to config file
#[test]
#[serial]
fn test_config_fresh_picks_up_file_changes() {
    // Create a temporary config directory
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let _home_guard = HomeEnvGuard::set(temp_dir.path());

    let config_dir = temp_dir.path().join(".git-ai");
    std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

    // Write initial config with api_base_url = "https://old.example.com"
    let mut file_config = git_ai::config::FileConfig {
        api_base_url: Some("https://old.example.com".to_string()),
        ..Default::default()
    };
    save_file_config(&file_config).expect("Failed to save config");

    // Verify file was written
    let config_file = config_dir.join("config.json");
    assert!(config_file.exists(), "Config file should exist");

    // Read with Config::fresh() - should see old URL
    let config1 = Config::fresh();
    assert_eq!(config1.api_base_url(), "https://old.example.com");

    // Change the config file to new URL
    file_config.api_base_url = Some("https://new.example.com".to_string());
    save_file_config(&file_config).expect("Failed to save updated config");

    // Read with Config::fresh() again - should see new URL
    let config2 = Config::fresh();
    assert_eq!(config2.api_base_url(), "https://new.example.com");
}

/// Test that Config::get() returns cached config and doesn't pick up changes
#[test]
#[serial]
fn test_config_get_uses_cache() {
    // This test demonstrates the problem: Config::get() uses OnceLock
    // which means it's initialized once and never refreshed

    // Create a temporary config directory
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let config_dir = temp_dir.path().join(".git-ai");
    std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

    let _home_guard = HomeEnvGuard::set(temp_dir.path());

    // Write initial config
    let file_config = git_ai::config::FileConfig {
        api_base_url: Some("https://initial.example.com".to_string()),
        ..Default::default()
    };
    save_file_config(&file_config).expect("Failed to save config");

    // First call to Config::get() initializes the cache
    // Note: We can't actually test this directly because Config::get()
    // uses a global OnceLock that persists across tests.
    // This test documents the expected behavior.

    // The issue is that in daemon mode, if we call Config::get() once,
    // then change the config file, subsequent calls to Config::get()
    // will still return the cached version.
}

/// Test that api_key changes are picked up by Config::fresh()
#[test]
#[serial]
fn test_config_fresh_picks_up_api_key_changes() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let config_dir = temp_dir.path().join(".git-ai");
    std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

    let _home_guard = HomeEnvGuard::set(temp_dir.path());
    // Clear any ambient GIT_AI_API_KEY so this test exercises file config only.
    let _api_key_guard = ApiKeyEnvGuard::clear();

    // Initially no API key
    let file_config = git_ai::config::FileConfig::default();
    save_file_config(&file_config).expect("Failed to save config");

    let config1 = Config::fresh();
    assert!(config1.api_key().is_none());

    // Add API key
    let mut file_config = load_file_config_public().expect("Failed to load config");
    file_config.api_key = Some("test_api_key_12345".to_string());
    save_file_config(&file_config).expect("Failed to save updated config");

    // Config::fresh() should see the new API key
    let config2 = Config::fresh();
    assert_eq!(config2.api_key(), Some("test_api_key_12345"));

    // Remove API key
    let mut file_config = load_file_config_public().expect("Failed to load config");
    file_config.api_key = None;
    save_file_config(&file_config).expect("Failed to save updated config");

    // Config::fresh() should see it's gone
    let config3 = Config::fresh();
    assert!(config3.api_key().is_none());
}

/// Test that environment variable is read when config file doesn't specify value
#[test]
#[serial]
fn test_config_fresh_respects_env_vars() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");

    let original_api_url = env::var("GIT_AI_API_BASE_URL").ok();

    let _home_guard = HomeEnvGuard::set(temp_dir.path());

    unsafe {
        // Remove the env var initially
        env::remove_var("GIT_AI_API_BASE_URL");
    }

    let config_dir = temp_dir.path().join(".git-ai");
    std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

    // Create config file WITHOUT api_base_url, so env var should be used
    let file_config = git_ai::config::FileConfig::default();
    save_file_config(&file_config).expect("Failed to save config");

    // Without env var, should use default
    let config1 = Config::fresh();
    assert_eq!(config1.api_base_url(), "https://usegitai.com");

    // With env var set, should use env var
    unsafe {
        env::set_var("GIT_AI_API_BASE_URL", "https://env-var.example.com");
    }
    let config2 = Config::fresh();
    assert_eq!(config2.api_base_url(), "https://env-var.example.com");

    // Remove env var, should go back to default
    unsafe {
        env::remove_var("GIT_AI_API_BASE_URL");
    }
    let config3 = Config::fresh();
    assert_eq!(config3.api_base_url(), "https://usegitai.com");

    // Restore original GIT_AI_API_BASE_URL (home guard restores home vars via Drop)
    unsafe {
        if let Some(api_url) = original_api_url {
            env::set_var("GIT_AI_API_BASE_URL", api_url);
        } else {
            env::remove_var("GIT_AI_API_BASE_URL");
        }
    }
}
/// Test that ApiContext picks up config changes via Config::fresh()
#[test]
#[serial]
fn test_api_context_uses_fresh_config() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let _home_guard = HomeEnvGuard::set(temp_dir.path());

    let config_dir = temp_dir.path().join(".git-ai");
    std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

    // Set initial API URL
    let mut file_config = git_ai::config::FileConfig {
        api_base_url: Some("https://api1.example.com".to_string()),
        ..Default::default()
    };
    save_file_config(&file_config).expect("Failed to save config");

    // Create ApiContext - should use the first URL
    let ctx1 = ApiContext::new(None);
    assert_eq!(ctx1.base_url, "https://api1.example.com");

    // Change the config file
    file_config.api_base_url = Some("https://api2.example.com".to_string());
    save_file_config(&file_config).expect("Failed to save updated config");

    // Create new ApiContext - should pick up the new URL
    let ctx2 = ApiContext::new(None);
    assert_eq!(ctx2.base_url, "https://api2.example.com");
}

/// Test that OAuthClient picks up config changes via Config::fresh()
#[test]
#[serial]
fn test_oauth_client_uses_fresh_config() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let _home_guard = HomeEnvGuard::set(temp_dir.path());

    let config_dir = temp_dir.path().join(".git-ai");
    std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

    // Set initial API URL
    let mut file_config = git_ai::config::FileConfig {
        api_base_url: Some("https://auth1.example.com".to_string()),
        ..Default::default()
    };
    save_file_config(&file_config).expect("Failed to save config");

    // Create OAuthClient - should use the first URL
    let _client1 = OAuthClient::new();
    // We can't directly access base_url, but we can verify it doesn't panic

    // Change the config file
    file_config.api_base_url = Some("https://auth2.example.com".to_string());
    save_file_config(&file_config).expect("Failed to save updated config");

    // Create new OAuthClient - should pick up the new URL
    let _client2 = OAuthClient::new();
    // Again, just verify it doesn't panic
}

/// Test that api_key changes are picked up by ApiContext
#[test]
#[serial]
fn test_api_context_picks_up_api_key_changes() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let _home_guard = HomeEnvGuard::set(temp_dir.path());
    // Clear any ambient GIT_AI_API_KEY so this test exercises file config only.
    let _api_key_guard = ApiKeyEnvGuard::clear();

    let config_dir = temp_dir.path().join(".git-ai");
    std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

    // Initially no API key
    let file_config = git_ai::config::FileConfig::default();
    save_file_config(&file_config).expect("Failed to save config");

    let ctx1 = ApiContext::new(None);
    assert!(ctx1.api_key.is_none());

    // Add API key
    let mut file_config = load_file_config_public().expect("Failed to load config");
    file_config.api_key = Some("test_key_123".to_string());
    save_file_config(&file_config).expect("Failed to save updated config");

    // Create new ApiContext - should pick up the API key
    let ctx2 = ApiContext::new(None);
    assert_eq!(ctx2.api_key, Some("test_key_123".to_string()));

    // Remove API key
    let mut file_config = load_file_config_public().expect("Failed to load config");
    file_config.api_key = None;
    save_file_config(&file_config).expect("Failed to save updated config");

    // Create new ApiContext - should see no API key
    let ctx3 = ApiContext::new(None);
    assert!(ctx3.api_key.is_none());
}
