/// Test that build_agent creates a working agent using the platform's
/// native TLS library (OpenSSL on Linux, Secure Transport on macOS,
/// SChannel on Windows).
#[test]
fn test_build_agent_default_config() {
    let agent = git_ai::http::build_agent(Some(5));
    // Agent should be created successfully - just verify it doesn't panic
    drop(agent);
}

/// Test that the agent can make a real HTTPS request, proving that the
/// native TLS stack and system certificate store are working correctly.
#[test]
fn test_https_request_uses_system_certs() {
    const URLS: &[&str] = &[
        "https://example.com",
        "https://www.rust-lang.org",
        "https://github.com",
    ];
    const ATTEMPTS_PER_URL: usize = 3;

    let mut failures = Vec::new();
    for url in URLS {
        for attempt in 1..=ATTEMPTS_PER_URL {
            let agent = git_ai::http::build_agent(Some(10));
            match git_ai::http::send(agent.get(*url)) {
                Ok(response) if (200..400).contains(&response.status_code) => return,
                Ok(response) => failures.push(format!(
                    "{} attempt {} returned status {}",
                    url, attempt, response.status_code
                )),
                Err(error) => {
                    failures.push(format!("{} attempt {} failed: {}", url, attempt, error))
                }
            }
        }
    }

    panic!(
        "HTTPS requests to trusted public endpoints failed; native TLS certs may be broken or the network is unavailable:\n{}",
        failures.join("\n")
    );
}
