use crate::auth::{CredentialStore, OAuthClient};
use crate::config;
use crate::error::GitAiError;
use crate::git::repository::{current_git_committer_identity_resolution, parse_git_var_identity};
use crate::http;
use once_cell::sync::Lazy;
use std::sync::Mutex;
use url::Url;

/// Global mutex to prevent multiple threads from refreshing simultaneously.
/// This provides in-process synchronization to avoid thundering herd issues.
/// Note: Cross-process races are acceptable - both processes get valid tokens.
static REFRESH_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// Attempt to load stored credentials and refresh if needed.
/// Returns None on any failure (not logged in, expired, refresh failed).
/// Uses in-process Mutex for thread safety during token refresh.
fn try_load_auth_token() -> Option<String> {
    let store = CredentialStore::new();

    let creds = match store.load() {
        Ok(Some(c)) => c,
        _ => return None,
    };

    // If refresh token expired, can't authenticate
    if creds.is_refresh_token_expired() {
        return None;
    }

    // Fast path: if access token is valid (with 5 min buffer), use it directly
    if !creds.is_access_token_expired(300) {
        return Some(creds.access_token);
    }

    // Need to refresh - acquire mutex to prevent thundering herd within this process
    // If mutex is poisoned (previous panic), we return None gracefully
    let _guard = REFRESH_LOCK.lock().ok()?;

    // Re-check credentials after acquiring lock - another thread may have refreshed
    let creds = match store.load() {
        Ok(Some(c)) => c,
        _ => return None,
    };

    // Check again if access token is now valid (another thread may have refreshed)
    if !creds.is_access_token_expired(300) {
        return Some(creds.access_token);
    }

    // Still expired - we need to refresh
    let client = OAuthClient::new();
    match client.refresh_access_token(&creds.refresh_token) {
        Ok(new_creds) => {
            // Store refreshed credentials (ignore errors - we still have the token)
            let _ = store.store(&new_creds);
            Some(new_creds.access_token)
        }
        Err(_) => None,
    }
    // Mutex guard is automatically released when _guard is dropped
}

/// Resolve the git-ai effective author identity without requiring a Repository instance.
///
/// Uses the shared git identity helper to get the current user's identity,
/// respecting the full git precedence chain (env vars > config > system defaults),
/// then overlays any configured git-ai author fields.
/// Falls back to the system hostname if git identity is unavailable.
fn resolve_git_identity() -> Option<String> {
    let author_config = config::Config::fresh_author_cached();
    let identity = current_git_committer_identity_resolution()
        .identity
        .with_author_config(&author_config);
    if let Some(formatted) = identity.formatted() {
        return Some(encode_for_header(&formatted));
    }

    resolve_fallback_identity()
        .map(|id| parse_git_var_identity(&id).with_author_config(&author_config))
        .and_then(|identity| identity.formatted())
        .map(|id| encode_for_header(&id))
}

/// Build a fallback identity matching git's format: `"Username <username@hostname>"`.
fn resolve_fallback_identity() -> Option<String> {
    let username = resolve_username()?;
    let hostname = resolve_hostname().unwrap_or_else(|| "localhost".to_string());
    Some(format!("{} <{}@{}>", username, username, hostname))
}

fn resolve_username() -> Option<String> {
    #[cfg(windows)]
    if let Ok(u) = std::env::var("USERNAME")
        && !u.trim().is_empty()
    {
        return Some(u.trim().to_string());
    }
    #[cfg(not(windows))]
    if let Ok(u) = std::env::var("USER")
        && !u.trim().is_empty()
    {
        return Some(u.trim().to_string());
    }
    None
}

fn resolve_hostname() -> Option<String> {
    #[cfg(windows)]
    if let Ok(h) = std::env::var("COMPUTERNAME")
        && !h.trim().is_empty()
    {
        return Some(h.trim().to_string());
    }
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.trim().is_empty()
    {
        return Some(h.trim().to_string());
    }
    let mut cmd = std::process::Command::new("hostname");
    #[cfg(windows)]
    {
        use crate::utils::CREATE_NO_WINDOW;
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, CREATE_NO_WINDOW);
    }
    let output = cmd.output().ok()?;
    let h = String::from_utf8(output.stdout).ok()?;
    let h = h.trim();
    if h.is_empty() {
        None
    } else {
        Some(h.to_string())
    }
}

/// Percent-encode non-ASCII and control bytes so the value is safe for HTTP headers.
/// ureq 2.x accepts only visible ASCII (0x21..=0x7E) and space/tab in header values.
fn encode_for_header(value: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'%' => encoded.push_str("%25"),
            0x20..=0x7E => encoded.push(byte as char),
            _ => write!(encoded, "%{:02X}", byte).unwrap(),
        }
    }
    encoded
}

/// API client context with optional authentication
#[derive(Clone)]
pub struct ApiContext {
    /// Base URL for the API (e.g., `https://app.com`)
    pub base_url: String,
    /// Optional authentication token
    pub auth_token: Option<String>,
    /// Optional API key for X-API-Key header
    pub api_key: Option<String>,
    /// Optional git author identity for X-Author-Identity header (only sent when API key is set)
    pub author_identity: Option<String>,
    /// Request timeout in seconds
    pub timeout_secs: Option<u64>,
}

impl std::fmt::Debug for ApiContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiContext")
            .field("base_url", &self.base_url)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("author_identity", &self.author_identity)
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

impl ApiContext {
    /// Get the default API base URL from config
    /// Uses Config::fresh() to support runtime config updates (daemon mode)
    fn default_base_url() -> String {
        config::Config::fresh().api_base_url().to_string()
    }

    /// Create a GET request with common headers (User-Agent, X-Distinct-ID)
    /// Use this for all HTTP GET requests to ensure consistent headers.
    /// The returned (Agent, Request) pair uses the system's native certificate store.
    pub fn http_get(
        url: &str,
        timeout_secs: Option<u64>,
    ) -> (
        ureq::Agent,
        ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    ) {
        let agent = http::build_agent(timeout_secs);
        let request = agent
            .get(url)
            .header(
                "User-Agent",
                &format!("git-ai/{}", env!("CARGO_PKG_VERSION")),
            )
            .header("X-Distinct-ID", &config::get_or_create_distinct_id());
        (agent, request)
    }

    /// Create a POST request with common headers (User-Agent, X-Distinct-ID)
    /// Use this for all HTTP POST requests to ensure consistent headers.
    /// The returned (Agent, Request) pair uses the system's native certificate store.
    pub fn http_post(
        url: &str,
        timeout_secs: Option<u64>,
    ) -> (ureq::Agent, ureq::RequestBuilder<ureq::typestate::WithBody>) {
        let agent = http::build_agent(timeout_secs);
        let request = agent
            .post(url)
            .header(
                "User-Agent",
                &format!("git-ai/{}", env!("CARGO_PKG_VERSION")),
            )
            .header("X-Distinct-ID", &config::get_or_create_distinct_id());
        (agent, request)
    }

    /// Create a new API context, automatically using stored credentials if available
    /// If base_url is None, uses api_base_url from config (which can be set via config file, env var, or defaults)
    /// Uses Config::fresh() to support runtime config updates (daemon mode)
    pub fn new(base_url: Option<String>) -> Self {
        let cfg = config::Config::fresh();
        let api_key = cfg.api_key().map(|s| s.to_string());
        let author_identity = if api_key.is_some() {
            resolve_git_identity()
        } else {
            None
        };
        Self {
            base_url: base_url.unwrap_or_else(Self::default_base_url),
            auth_token: try_load_auth_token(),
            api_key,
            author_identity,
            timeout_secs: Some(30),
        }
    }

    /// Create a new API context explicitly without authentication
    /// Use this when you need to ensure no auth token is sent
    /// Uses Config::fresh() to support runtime config updates (daemon mode)
    #[allow(dead_code)]
    pub fn without_auth(base_url: Option<String>) -> Self {
        let cfg = config::Config::fresh();
        let api_key = cfg.api_key().map(|s| s.to_string());
        let author_identity = if api_key.is_some() {
            resolve_git_identity()
        } else {
            None
        };
        Self {
            base_url: base_url.unwrap_or_else(Self::default_base_url),
            auth_token: None,
            api_key,
            author_identity,
            timeout_secs: Some(30),
        }
    }

    /// Create a new API context with authentication
    /// If base_url is None, uses api_base_url from config (which can be set via config file, env var, or defaults)
    /// Uses Config::fresh() to support runtime config updates (daemon mode)
    #[allow(dead_code)]
    pub fn with_auth(base_url: Option<String>, auth_token: String) -> Self {
        let cfg = config::Config::fresh();
        let api_key = cfg.api_key().map(|s| s.to_string());
        let author_identity = if api_key.is_some() {
            resolve_git_identity()
        } else {
            None
        };
        Self {
            base_url: base_url.unwrap_or_else(Self::default_base_url),
            auth_token: Some(auth_token),
            api_key,
            author_identity,
            timeout_secs: Some(30),
        }
    }

    /// Set a custom timeout
    pub fn with_timeout(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = Some(timeout_secs);
        self
    }

    /// Build the full URL for an endpoint.
    ///
    /// The endpoint is appended to the base URL preserving any path prefix on
    /// the base — i.e. `https://host/api/gitai` + `/worker/notes/upload`
    /// yields `https://host/api/gitai/worker/notes/upload`. Leading/trailing
    /// slashes are normalized so the join works regardless of which side
    /// carries the separator.
    fn build_url(&self, endpoint: &str) -> Result<String, GitAiError> {
        Url::parse(&self.base_url)
            .map_err(|e| GitAiError::Generic(format!("Invalid base URL: {}", e)))?;
        let joined = format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            endpoint.trim_start_matches('/')
        );
        Url::parse(&joined)
            .map_err(|e| GitAiError::Generic(format!("Invalid endpoint URL: {}", e)))?;
        Ok(joined)
    }

    /// Make a POST request with JSON body
    pub fn post_json<T: serde::Serialize>(
        &self,
        endpoint: &str,
        body: &T,
    ) -> Result<http::Response, GitAiError> {
        let url = self.build_url(endpoint)?;
        let body_json = serde_json::to_string(body).map_err(GitAiError::JsonError)?;

        let (_agent, mut request) = Self::http_post(&url, self.timeout_secs);
        request = request.header("Content-Type", "application/json");

        if let Some(api_key) = &self.api_key {
            request = request.header("X-API-Key", api_key);
            if let Some(identity) = &self.author_identity {
                request = request.header("X-Author-Identity", identity);
            }
        }
        if let Some(token) = &self.auth_token {
            request = request.header("Authorization", &format!("Bearer {}", token));
        }

        http::send_with_body(request, &body_json)
            .map_err(|e| GitAiError::Generic(format!("HTTP request failed: {}", e)))
    }

    /// Make a GET request
    pub fn get(&self, endpoint: &str) -> Result<http::Response, GitAiError> {
        let url = self.build_url(endpoint)?;

        let (_agent, mut request) = Self::http_get(&url, self.timeout_secs);

        if let Some(api_key) = &self.api_key {
            request = request.header("X-API-Key", api_key);
            if let Some(identity) = &self.author_identity {
                request = request.header("X-Author-Identity", identity);
            }
        }
        if let Some(token) = &self.auth_token {
            request = request.header("Authorization", &format!("Bearer {}", token));
        }

        http::send(request).map_err(|e| GitAiError::Generic(format!("HTTP request failed: {}", e)))
    }
}

/// API client wrapper
#[derive(Debug, Clone)]
pub struct ApiClient {
    context: ApiContext,
}

impl ApiClient {
    /// Create a new API client with the given context
    pub fn new(context: ApiContext) -> Self {
        Self { context }
    }

    /// Get a reference to the API context
    pub fn context(&self) -> &ApiContext {
        &self.context
    }

    /// Get a mutable reference to the API context
    #[allow(dead_code)]
    pub fn context_mut(&mut self) -> &mut ApiContext {
        &mut self.context
    }

    /// Check if user is logged in (has an auth token)
    pub fn is_logged_in(&self) -> bool {
        self.context.auth_token.is_some()
    }

    /// Check if an API key is configured
    pub fn has_api_key(&self) -> bool {
        self.context.api_key.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ============= ApiContext Tests =============

    #[test]
    fn test_api_context_without_auth() {
        let ctx = ApiContext::without_auth(Some("https://example.com".to_string()));
        assert!(ctx.auth_token.is_none());
        assert_eq!(ctx.base_url, "https://example.com");
    }

    #[test]
    fn test_api_context_with_auth() {
        let ctx = ApiContext::with_auth(
            Some("https://example.com".to_string()),
            "test_token".to_string(),
        );
        assert_eq!(ctx.auth_token, Some("test_token".to_string()));
        assert_eq!(ctx.base_url, "https://example.com");
    }

    #[test]
    fn test_api_context_with_timeout() {
        let ctx =
            ApiContext::without_auth(Some("https://example.com".to_string())).with_timeout(60);
        assert_eq!(ctx.timeout_secs, Some(60));
    }

    #[test]
    fn test_api_context_default_timeout() {
        let ctx = ApiContext::without_auth(Some("https://example.com".to_string()));
        assert_eq!(ctx.timeout_secs, Some(30));
    }

    // ============= ApiClient Tests =============

    #[test]
    fn test_api_client_is_logged_in_true() {
        let ctx =
            ApiContext::with_auth(Some("https://example.com".to_string()), "token".to_string());
        let client = ApiClient::new(ctx);
        assert!(client.is_logged_in());
    }

    #[test]
    fn test_api_client_is_logged_in_false() {
        let ctx = ApiContext::without_auth(Some("https://example.com".to_string()));
        let client = ApiClient::new(ctx);
        assert!(!client.is_logged_in());
    }

    #[test]
    fn test_api_client_context_access() {
        let ctx =
            ApiContext::with_auth(Some("https://example.com".to_string()), "token".to_string());
        let client = ApiClient::new(ctx);
        assert_eq!(client.context().base_url, "https://example.com");
    }

    // ============= URL Building Tests =============

    #[test]
    fn test_build_url_simple() {
        let ctx = ApiContext::without_auth(Some("https://example.com".to_string()));
        let url = ctx.build_url("/api/test").unwrap();
        assert_eq!(url, "https://example.com/api/test");
    }

    #[test]
    fn test_build_url_with_trailing_slash() {
        let ctx = ApiContext::without_auth(Some("https://example.com/".to_string()));
        let url = ctx.build_url("api/test").unwrap();
        assert_eq!(url, "https://example.com/api/test");
    }

    #[test]
    fn test_build_url_invalid_base() {
        let ctx = ApiContext::without_auth(Some("not-a-url".to_string()));
        let result = ctx.build_url("/api/test");
        assert!(result.is_err());
    }

    #[test]
    fn test_build_url_preserves_path_prefix() {
        let ctx = ApiContext::without_auth(Some("https://example.com/api/gitai".to_string()));
        let url = ctx.build_url("/worker/notes/upload").unwrap();
        assert_eq!(url, "https://example.com/api/gitai/worker/notes/upload");
    }

    #[test]
    fn test_build_url_preserves_path_prefix_with_trailing_slash() {
        let ctx = ApiContext::without_auth(Some("https://example.com/api/gitai/".to_string()));
        let url = ctx.build_url("/worker/notes/upload").unwrap();
        assert_eq!(url, "https://example.com/api/gitai/worker/notes/upload");
    }

    #[test]
    fn test_build_url_preserves_query_string() {
        let ctx = ApiContext::without_auth(Some("https://example.com/api/gitai".to_string()));
        let url = ctx.build_url("/worker/notes/?commits=abc,def").unwrap();
        assert_eq!(
            url,
            "https://example.com/api/gitai/worker/notes/?commits=abc,def"
        );
    }

    // ============= Mutex Thread Safety Tests =============

    #[test]
    fn test_mutex_is_accessible() {
        // Simple test to verify the mutex can be locked
        let guard = REFRESH_LOCK.lock();
        assert!(guard.is_ok());
        // Guard drops here, releasing the lock
    }

    #[test]
    fn test_concurrent_access_to_mutex() {
        // Test that multiple threads can safely contend for the mutex
        let counter = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];

        for _ in 0..5 {
            let counter_clone = counter.clone();
            let handle = std::thread::spawn(move || {
                if let Ok(_guard) = REFRESH_LOCK.lock() {
                    counter_clone.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // All threads should have acquired the lock sequentially
        let final_count = counter.load(Ordering::SeqCst);
        assert_eq!(final_count, 5);
    }

    // ============= encode_for_header Tests =============

    #[test]
    fn test_encode_for_header_ascii_passthrough() {
        let value = "John Doe <john@example.com>";
        assert_eq!(encode_for_header(value), value);
    }

    #[test]
    fn test_encode_for_header_non_ascii() {
        assert_eq!(
            encode_for_header("Ex\u{00f6}utf8lastname <user@example.com>"),
            "Ex%C3%B6utf8lastname <user@example.com>"
        );
    }

    #[test]
    fn test_encode_for_header_percent_encoded_for_reversibility() {
        assert_eq!(encode_for_header("100% done"), "100%25 done");
    }

    #[test]
    fn test_encode_for_header_special_ascii_chars_passthrough() {
        let value = "Name+Tag <user+tag@sub.example.com>";
        assert_eq!(encode_for_header(value), value);
    }

    #[test]
    fn test_encode_for_header_all_bytes_valid_for_ureq() {
        let input = "Ñoño García <nono@example.com>";
        let encoded = encode_for_header(input);
        assert!(
            encoded
                .bytes()
                .all(|b| b == b' ' || b == b'\t' || (0x21..=0x7E).contains(&b)),
            "encoded value contains invalid header bytes: {:?}",
            encoded
        );
    }
}
