use crate::clients::auth::{CredentialStore, OAuthClient};

/// Handle the `git-ai login` command
pub fn handle_login(_args: &[String]) {
    let store = CredentialStore::new();

    // Check if already logged in
    if let Ok(Some(creds)) = store.load()
        && !creds.is_refresh_token_expired()
    {
        eprintln!("Already logged in. Use 'git-ai logout' to log out first.");
        std::process::exit(0);
    }

    let client = OAuthClient::new();

    // Start device flow
    eprintln!("Starting device authorization...\n");

    let auth_response = match client.start_device_flow() {
        Ok(response) => response,
        Err(e) => {
            eprintln!("Failed to start authorization: {}", e);
            std::process::exit(1);
        }
    };

    // Build the display URL
    let display_url = auth_response
        .verification_uri_complete
        .as_ref()
        .unwrap_or(&auth_response.verification_uri);

    // Display instructions
    eprintln!("To authorize this device:");
    eprintln!("  1. Open this URL in your browser:");
    eprintln!("     {}", display_url);
    eprintln!();
    eprintln!("  2. Enter this code when prompted:");
    eprintln!("     {}", auth_response.user_code);
    eprintln!();

    // Try to open browser automatically
    if super::browser::browser_command(display_url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
        .is_err()
    {
        eprintln!("  (Could not open browser automatically)");
        eprintln!();
    }

    eprintln!("Waiting for authorization...");

    // Poll for token
    match client.poll_for_token(
        &auth_response.device_code,
        auth_response.interval,
        auth_response.expires_in,
    ) {
        Ok(creds) => {
            // Store credentials
            if let Err(e) = store.store(&creds) {
                eprintln!("\nWarning: Failed to store credentials: {}", e);
                eprintln!("You may need to log in again next time.");
            }

            eprintln!("\nSuccessfully logged in!");
        }
        Err(e) => {
            eprintln!("\nAuthorization failed: {}", e);
            std::process::exit(1);
        }
    }
}
