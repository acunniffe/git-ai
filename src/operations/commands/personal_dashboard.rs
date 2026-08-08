use crate::config;

/// Handle the `git-ai personal-dashboard` command
pub fn handle_personal_dashboard(_args: &[String]) {
    // Use Config::fresh() to support runtime config updates (daemon mode)
    let config = config::Config::fresh();
    let api_base_url = config.api_base_url();

    let dashboard_url = format!("{}/me", api_base_url);

    eprintln!("Opening dashboard: {}", dashboard_url);

    if super::browser::browser_command(&dashboard_url)
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
        .is_err()
    {
        eprintln!("Could not open browser automatically.");
        eprintln!("Visit this URL in your browser:");
        eprintln!("  {}", dashboard_url);
    }
}
