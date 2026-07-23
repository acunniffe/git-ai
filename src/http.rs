use std::time::Duration;
use ureq::config::Config;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::typestate::{WithBody, WithoutBody};

/// Build a ureq Agent that uses standard proxy environment variables and the
/// platform's native TLS library.
///
/// Uses OpenSSL on Linux, Secure Transport on macOS, and SChannel on
/// Windows — the same TLS implementations that curl uses. This ensures
/// certificates trusted by the OS (including custom CA certs added to
/// the system trust store) are handled identically to curl and browsers.
///
/// Proxy configuration is read from `ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`,
/// and their lowercase variants. `NO_PROXY`/`no_proxy` bypasses matching hosts.
pub fn build_agent(timeout_secs: Option<u64>) -> ureq::Agent {
    let mut builder = Config::builder().http_status_as_error(false).tls_config(
        TlsConfig::builder()
            .provider(TlsProvider::NativeTls)
            .root_certs(RootCerts::PlatformVerifier)
            .build(),
    );

    if let Some(secs) = timeout_secs {
        builder = builder.timeout_global(Some(Duration::from_secs(secs)));
    }

    builder.build().new_agent()
}

/// HTTP response wrapper that normalizes ureq's error handling.
/// ureq treats non-2xx responses as errors; this wrapper treats them as normal
/// responses (matching minreq's previous behavior and what callers expect).
pub struct Response {
    pub status_code: u16,
    body: Vec<u8>,
}

impl Response {
    pub fn as_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.body)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.body
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.body
    }
}

fn read_ureq_response(mut response: ureq::http::Response<ureq::Body>) -> Result<Response, String> {
    let status_code = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .read_to_vec()
        .map_err(|e| format!("Failed to read response body: {}", e))?;
    Ok(Response { status_code, body })
}

/// Execute a ureq request, normalizing errors so that HTTP error status codes
/// are returned as Ok(Response) rather than Err.
pub fn send(request: ureq::RequestBuilder<WithoutBody>) -> Result<Response, String> {
    request
        .call()
        .map_err(|err| err.to_string())
        .and_then(read_ureq_response)
}

/// Execute a ureq request with a string body.
pub fn send_with_body(
    request: ureq::RequestBuilder<WithBody>,
    body: &str,
) -> Result<Response, String> {
    request
        .send(body)
        .map_err(|err| err.to_string())
        .and_then(read_ureq_response)
}
