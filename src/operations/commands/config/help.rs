use super::spec::config_key_specs;

pub(super) fn print_config_help() {
    println!("git-ai config - View and manage git-ai configuration");
    println!();
    println!("Usage:");
    println!("  git-ai config                Show all config as formatted JSON");
    println!("  git-ai config <key>          Show specific config value");
    println!("  git-ai config set <key> <value>          Set a config value");
    println!("  git-ai config set <key> <value> --add    Add to array (extends existing)");
    println!("  git-ai config --add <key> <value>        Add to array or upsert into object");
    println!("  git-ai config unset <key>    Remove config value (reverts to default)");
    println!();
    println!("Configuration Keys:");
    for spec in config_key_specs().iter().filter(|spec| spec.show_in_help) {
        println!("  {}", spec.help);
    }
    println!(
        "                               May include a path prefix; endpoints are appended to it."
    );
    println!(
        "                               e.g. \"https://app.example.com/api/gitai\" -> requests are"
    );
    println!("                               sent to \"<base>/worker/notes/upload\" and");
    println!("                               \"<base>/worker/notes/?commits=...\".");
    println!();
    println!("Repository Patterns:");
    println!("  For exclude/allow/exclude_prompts_in_repositories, you can provide:");
    println!("    - A glob pattern: \"*\", \"https://github.com/org/*\"");
    println!("    - A URL/git protocol: \"git@github.com:org/repo.git\"");
    println!("    - A file path: \".\" or \"/path/to/repo\" (resolves to repo's remotes)");
    println!();
    println!("Examples:");
    println!("  git-ai config exclude_repositories");
    println!("  git-ai config set disable_auto_updates true");
    println!("  git-ai config set author.name \"Alice Example\"");
    println!("  git-ai config set author.email alice@example.com");
    println!("  git-ai config set exclude_repositories \"private/*\"");
    println!("  git-ai config set exclude_repositories .         # Uses current repo's remotes");
    println!("  git-ai config --add exclude_repositories \"temp/*\"");
    println!("  git-ai config --add allowed_repositories ~/projects/my-repo");
    println!("  git-ai config --add feature_flags.my_flag true");
    println!("  git-ai config --add git_ai_hooks.post_notes_updated \"./my-hook.sh\"");
    println!("  git-ai config set codex_hooks_format hooks_json");
    println!("  git-ai config set allow_superuser true");
    println!("  git-ai config set transcript_streaming_lookback_days 1");
    println!("  git-ai config set daemon_memory_limit_mb 1024");
    println!("  git-ai config set custom_attributes '{{\"team\":\"platform\"}}'");
    println!("  git-ai config --add custom_attributes.team platform");
    println!("  git-ai config unset exclude_repositories");
    println!();
    std::process::exit(0);
}
