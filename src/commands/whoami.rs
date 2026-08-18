use crate::api::{ApiClient, ApiContext, metrics_upload_allowed};
use crate::auth::state::AuthStatus;
use crate::auth::{AuthState, collect_auth_status, format_unix_timestamp};
use crate::config;
use crate::metrics::db::{MetricsDatabase, MetricsStatus};
use std::fmt::Write as _;

pub fn handle_whoami(args: &[String]) {
    if args
        .iter()
        .any(|arg| arg == "--help" || arg == "-h" || arg == "help")
    {
        print_help();
        std::process::exit(0);
    }

    if !args.is_empty() {
        eprintln!("Error: unknown whoami argument(s): {}", args.join(" "));
        print_help();
        std::process::exit(1);
    }

    // Use Config::fresh() to support runtime config updates (daemon mode).
    let config = config::Config::fresh();
    let api_base_url = config.api_base_url().to_string();
    let telemetry_oss_disabled = config.is_telemetry_oss_disabled();
    let auth = collect_auth_status();
    let api_ctx = ApiContext::new(None);
    let api_client = ApiClient::new(api_ctx.clone());
    let metrics_status = collect_metrics_status();

    print!(
        "{}",
        render_whoami(
            &api_base_url,
            &auth,
            &api_ctx,
            &api_client,
            metrics_status.as_ref().map_err(String::as_str),
            telemetry_oss_disabled,
        )
    );

    if should_exit_failure(&auth, &api_ctx) {
        std::process::exit(1);
    }
}

fn collect_metrics_status() -> Result<MetricsStatus, String> {
    let db = MetricsDatabase::global().map_err(|e| e.to_string())?;
    let db = db
        .lock()
        .map_err(|_| "metrics DB lock poisoned".to_string())?;
    db.status().map_err(|e| e.to_string())
}

fn render_whoami(
    api_base_url: &str,
    auth: &AuthStatus,
    api_ctx: &ApiContext,
    api_client: &ApiClient,
    metrics_status: Result<&MetricsStatus, &str>,
    telemetry_oss_disabled: bool,
) -> String {
    let mut out = String::new();

    writeln!(out, "git-ai status").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "API").unwrap();
    writeln!(out, "  Base URL: {}", api_base_url).unwrap();
    writeln!(
        out,
        "  API access: {}",
        api_access_status(auth, api_ctx, api_client)
    )
    .unwrap();
    writeln!(
        out,
        "  Identity: {}",
        author_identity_header_status(api_ctx)
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "Authentication").unwrap();
    writeln!(
        out,
        "  API key: {}",
        api_key_status(api_ctx.api_key.as_deref())
    )
    .unwrap();
    writeln!(out, "  Login: {}", login_status(auth, api_client)).unwrap();
    writeln!(out, "  Credential backend: {}", auth.backend).unwrap();
    if let Some(expires_at) = auth.access_token_expires_at {
        writeln!(
            out,
            "  Access token expires at: {}",
            format_unix_timestamp(expires_at)
        )
        .unwrap();
    }
    if let Some(expires_at) = auth.refresh_token_expires_at {
        writeln!(
            out,
            "  Refresh token expires at: {}",
            format_unix_timestamp(expires_at)
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    if matches!(auth.state, AuthState::LoggedIn) {
        writeln!(out, "Login identity").unwrap();
        writeln!(out, "  User ID: {}", optional_or_unavailable(&auth.user_id)).unwrap();
        writeln!(out, "  Email: {}", optional_or_unavailable(&auth.email)).unwrap();
        writeln!(out, "  Name: {}", optional_or_unavailable(&auth.name)).unwrap();
        writeln!(
            out,
            "  Personal org ID: {}",
            optional_or_unavailable(&auth.personal_org_id)
        )
        .unwrap();
        if auth.orgs.is_empty() {
            writeln!(out, "  Organizations: <none>").unwrap();
        } else {
            writeln!(out, "  Organizations:").unwrap();
            for org in &auth.orgs {
                let org_id = org.org_id.as_deref().unwrap_or("<unknown-id>");
                let org_slug = org.org_slug.as_deref().unwrap_or("<unknown-slug>");
                let org_name = org.org_name.as_deref().unwrap_or("<unknown-name>");
                let role = org.role.as_deref().unwrap_or("<unknown-role>");
                writeln!(
                    out,
                    "    - {} ({}) [{}] role={}",
                    org_slug, org_name, org_id, role
                )
                .unwrap();
            }
        }
        writeln!(out).unwrap();
    }

    writeln!(out, "Telemetry status").unwrap();
    writeln!(
        out,
        "  Metrics delivery: {}",
        metrics_delivery_status(api_base_url, api_client)
    )
    .unwrap();
    writeln!(
        out,
        "  OSS diagnostic telemetry: {}",
        if telemetry_oss_disabled { "off" } else { "on" }
    )
    .unwrap();
    match metrics_status {
        Ok(status) => write_metrics_status(&mut out, status),
        Err(err) => {
            writeln!(out, "  Local metrics database: unavailable ({})", err).unwrap();
        }
    }

    out
}

fn api_access_status(auth: &AuthStatus, api_ctx: &ApiContext, api_client: &ApiClient) -> String {
    match (api_client.has_api_key(), api_client.is_logged_in()) {
        (true, true) => "connected via API key and login".to_string(),
        (true, false) => "connected via API key".to_string(),
        (false, true) => "connected via login".to_string(),
        (false, false) => {
            if matches!(auth.state, AuthState::LoggedIn) {
                return "not connected (login credentials found, but no usable access token)"
                    .to_string();
            }
            if api_ctx.base_url != crate::config::DEFAULT_API_BASE_URL {
                "no auth credential found (custom API URL configured)".to_string()
            } else {
                "not connected".to_string()
            }
        }
    }
}

fn should_exit_failure(auth: &AuthStatus, api_ctx: &ApiContext) -> bool {
    api_ctx.api_key.is_none() && !matches!(auth.state, AuthState::LoggedIn)
}

fn author_identity_header_status(api_ctx: &ApiContext) -> String {
    if api_ctx.api_key.is_none() {
        return "not sent (API key not configured)".to_string();
    }

    api_ctx
        .author_identity
        .clone()
        .unwrap_or_else(|| "unavailable (header will be omitted)".to_string())
}

fn api_key_status(api_key: Option<&str>) -> String {
    api_key
        .map(|key| format!("configured ({})", mask_api_key(key)))
        .unwrap_or_else(|| "unset".to_string())
}

fn login_status(auth: &AuthStatus, api_client: &ApiClient) -> String {
    match &auth.state {
        AuthState::LoggedIn => "logged in".to_string(),
        AuthState::LoggedOut | AuthState::RefreshExpired | AuthState::Error(_) => {
            if api_client.has_api_key() {
                "optional (using api key)".to_string()
            } else {
                "not logged in".to_string()
            }
        }
    }
}

fn metrics_delivery_status(api_base_url: &str, api_client: &ApiClient) -> String {
    if crate::http::is_git_ai_cloud_url(api_base_url) {
        return "off (usegitai.com upload disabled)".to_string();
    }
    if !metrics_upload_allowed(api_base_url, api_client) {
        return "off (requires an API key or login)".to_string();
    }

    match (api_client.has_api_key(), api_client.is_logged_in()) {
        (true, true) => "on (API key and login connected)".to_string(),
        (true, false) => "on (API key configured)".to_string(),
        (false, true) => "on (login connected)".to_string(),
        (false, false) => unreachable!("metrics_upload_allowed requires auth"),
    }
}

fn write_metrics_status(out: &mut String, status: &MetricsStatus) {
    writeln!(out, "  Local metrics database: ok").unwrap();
    writeln!(
        out,
        "  Events: {} total, {} delivered, {} not delivered",
        status.total, status.delivered, status.not_delivered
    )
    .unwrap();
    writeln!(
        out,
        "  Not delivered: {} pending now, {} waiting for retry, {} processing, {} stopped after errors",
        status.pending_retryable,
        status.waiting_retry,
        status.processing,
        status.stopped_after_errors
    )
    .unwrap();
    writeln!(out, "  Rows with sync errors: {}", status.rows_with_errors).unwrap();
    if let Some(latest_error) = &status.latest_error {
        writeln!(out, "  Latest sync error: {}", latest_error).unwrap();
    }
}

fn optional_or_unavailable(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("<unavailable>")
}

fn mask_api_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let prefix: String = chars[..4].iter().collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{}...{}", prefix, suffix)
}

fn print_help() {
    eprintln!("git-ai whoami - Show current auth, identity, and telemetry status");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  git-ai whoami");
    eprintln!("  git-ai whoami --help");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::identity::TokenOrg;

    fn auth_status(state: AuthState) -> AuthStatus {
        AuthStatus {
            backend: "test-backend".to_string(),
            state,
            access_token_expires_at: None,
            refresh_token_expires_at: None,
            user_id: None,
            email: None,
            name: None,
            personal_org_id: None,
            orgs: Vec::new(),
        }
    }

    fn api_context(
        base_url: &str,
        api_key: Option<&str>,
        auth_token: Option<&str>,
        author_identity: Option<&str>,
    ) -> ApiContext {
        ApiContext {
            base_url: base_url.to_string(),
            auth_token: auth_token.map(str::to_string),
            api_key: api_key.map(str::to_string),
            author_identity: author_identity.map(str::to_string),
            timeout_secs: Some(30),
        }
    }

    fn metrics_status() -> MetricsStatus {
        MetricsStatus {
            total: 12,
            delivered: 8,
            not_delivered: 4,
            pending_retryable: 2,
            waiting_retry: 1,
            processing: 1,
            stopped_after_errors: 0,
            rows_with_errors: 1,
            latest_error: Some("temporary outage".to_string()),
        }
    }

    #[test]
    fn render_whoami_treats_api_key_only_as_connected() {
        let auth = auth_status(AuthState::LoggedOut);
        let ctx = api_context(
            crate::config::DEFAULT_API_BASE_URL,
            Some("gitai_test_1234567890"),
            None,
            Some("Alice Example <alice@example.com>"),
        );
        let client = ApiClient::new(ctx.clone());
        let metrics = metrics_status();

        let output = render_whoami(&ctx.base_url, &auth, &ctx, &client, Ok(&metrics), false);

        assert!(output.contains("API access: connected via API key"));
        assert!(output.contains("API key: configured (gita...7890)"));
        assert!(output.contains("Login: optional (using api key)"));
        assert!(output.contains("Identity: Alice Example <alice@example.com>"));
        assert!(!output.contains("Author identity header"));
        assert!(!output.contains("Login identity"));
        assert!(output.contains("Telemetry status"));
        assert!(!output.contains("Telemetry and metrics"));
        assert!(output.contains("Metrics delivery: off (usegitai.com upload disabled)"));
    }

    #[test]
    fn render_whoami_shows_login_identity_and_metrics_summary() {
        let mut auth = auth_status(AuthState::LoggedIn);
        auth.user_id = Some("user-123".to_string());
        auth.email = Some("user@example.com".to_string());
        auth.name = Some("User Example".to_string());
        auth.personal_org_id = Some("org-123".to_string());
        auth.orgs = vec![TokenOrg {
            org_id: Some("org-123".to_string()),
            org_slug: Some("acme".to_string()),
            org_name: Some("Acme".to_string()),
            role: Some("owner".to_string()),
        }];
        let ctx = api_context(
            crate::config::DEFAULT_API_BASE_URL,
            None,
            Some("access-token"),
            None,
        );
        let client = ApiClient::new(ctx.clone());
        let metrics = metrics_status();

        let output = render_whoami(&ctx.base_url, &auth, &ctx, &client, Ok(&metrics), true);

        assert!(output.contains("API access: connected via login"));
        assert!(output.contains("API key: unset"));
        assert!(output.contains("Login: logged in"));
        assert!(output.contains("Identity: not sent"));
        assert!(!output.contains("Author identity header"));
        assert!(output.contains("Login identity"));
        assert!(output.contains("Email: user@example.com"));
        assert!(output.contains("- acme (Acme) [org-123] role=owner"));
        assert!(output.contains("Telemetry status"));
        assert!(!output.contains("Telemetry and metrics"));
        assert!(output.contains("OSS diagnostic telemetry: off"));
        assert!(output.contains("Events: 12 total, 8 delivered, 4 not delivered"));
        assert!(output.contains("Rows with sync errors: 1"));
        assert!(output.contains("Latest sync error: temporary outage"));
    }

    #[test]
    fn render_whoami_shows_unset_api_key_and_not_logged_in() {
        let auth = auth_status(AuthState::LoggedOut);
        let ctx = api_context(crate::config::DEFAULT_API_BASE_URL, None, None, None);
        let client = ApiClient::new(ctx.clone());
        let metrics = metrics_status();

        let output = render_whoami(&ctx.base_url, &auth, &ctx, &client, Ok(&metrics), false);

        assert!(output.contains("API key: unset"));
        assert!(output.contains("Login: not logged in"));
    }

    #[test]
    fn should_exit_failure_preserves_stored_login_success() {
        let logged_out = auth_status(AuthState::LoggedOut);
        let logged_in = auth_status(AuthState::LoggedIn);
        let no_token_no_key = api_context(crate::config::DEFAULT_API_BASE_URL, None, None, None);
        let no_token_with_key = api_context(
            crate::config::DEFAULT_API_BASE_URL,
            Some("gitai_test_1234567890"),
            None,
            Some("Alice Example <alice@example.com>"),
        );

        assert!(should_exit_failure(&logged_out, &no_token_no_key));
        assert!(!should_exit_failure(&logged_out, &no_token_with_key));
        assert!(!should_exit_failure(&logged_in, &no_token_no_key));
    }

    #[test]
    fn render_whoami_distinguishes_login_credentials_from_usable_token() {
        let auth = auth_status(AuthState::LoggedIn);
        let ctx = api_context(crate::config::DEFAULT_API_BASE_URL, None, None, None);
        let client = ApiClient::new(ctx.clone());
        let metrics = metrics_status();

        let output = render_whoami(&ctx.base_url, &auth, &ctx, &client, Ok(&metrics), false);

        assert!(output.contains(
            "API access: not connected (login credentials found, but no usable access token)"
        ));
        assert!(output.contains("Login: logged in"));
        assert!(output.contains("Metrics delivery: off (usegitai.com upload disabled)"));
    }
}
