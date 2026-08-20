mod get;
mod help;
mod parse;
mod pattern;
mod set;
mod spec;
mod unset;

#[cfg(test)]
mod tests;

use get::{get_config_value, show_all_config};
use help::print_config_help;
use set::set_config_value;
use unset::unset_config_value;

pub fn handle_config(args: &[String]) {
    if args.is_empty() {
        // Show all config
        if let Err(e) = show_all_config() {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    // Check for help flags
    if args[0] == "--help" || args[0] == "-h" || args[0] == "help" {
        print_config_help();
        return;
    }

    // Check for --add flag anywhere in args
    let is_add_mode = args.iter().any(|a| a == "--add");
    let filtered_args: Vec<&String> = args.iter().filter(|a| *a != "--add").collect();

    if filtered_args.is_empty() {
        // Show all config if only --add was passed (which doesn't make sense)
        eprintln!("Error: --add requires <key> <value>");
        eprintln!("Usage: git-ai config --add <key> <value>");
        eprintln!("   or: git-ai config set <key> <value> --add");
        std::process::exit(1);
    }

    match filtered_args[0].as_str() {
        "set" => {
            if filtered_args.len() < 3 {
                eprintln!("Error: set requires <key> <value>");
                eprintln!("Usage: git-ai config set <key> <value>");
                std::process::exit(1);
            }
            let key = filtered_args[1].as_str();
            let value = filtered_args[2].as_str();
            if let Err(e) = set_config_value(key, value, is_add_mode) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            if key == "feature_flags.transcript_streaming"
                || key == "feature_flags.transcript_sweep"
                || key == "transcript_streaming_lookback_days"
                || key == "daemon_memory_limit_mb"
            {
                println!("Run `git-ai bg restart` for changes to take effect.");
            }
        }
        "unset" => {
            if filtered_args.len() < 2 {
                eprintln!("Error: unset requires <key>");
                eprintln!("Usage: git-ai config unset <key>");
                std::process::exit(1);
            }
            let key = filtered_args[1].as_str();
            if let Err(e) = unset_config_value(key) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
            if key == "daemon_memory_limit_mb" {
                println!("Run `git-ai bg restart` for changes to take effect.");
            }
        }
        key => {
            if is_add_mode {
                // git-ai config --add <key> <value>
                if filtered_args.len() < 2 {
                    eprintln!("Error: --add requires <key> <value>");
                    eprintln!("Usage: git-ai config --add <key> <value>");
                    std::process::exit(1);
                }
                let value = filtered_args[1].as_str();
                if let Err(e) = set_config_value(key, value, true) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            } else {
                // Get single value
                if let Err(e) = get_config_value(key) {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}
