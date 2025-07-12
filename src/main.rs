mod commands;
mod error;
mod git;
mod log_fmt;
mod utils;

use clap::Parser;
use git::find_repository;
use git::refs::AI_AUTHORSHIP_REFSPEC;
use std::io::{IsTerminal, Write};
use std::process::Command;
use utils::debug_log;

#[derive(Parser)]
#[command(name = "git-ai")]
#[command(about = "git proxy with AI authorship tracking", long_about = None)]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(disable_help_flag = true)]
#[command(disable_version_flag = true)]
struct Cli {
    /// Git command and arguments
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
}

fn main() {
    let cli = Cli::parse();

    if cli.args.is_empty() {
        // No arguments provided, proxy to git help
        proxy_to_git(&["help".to_string()]);
        return;
    }

    let command = &cli.args[0];
    let args = &cli.args[1..];

    match command.as_str() {
        "checkpoint" => {
            handle_checkpoint(args);
        }
        "blame" => {
            debug_log(&format!("overriding: git blame"));
            handle_blame(args);
        }
        "commit" => {
            // debug_log(&format!("wrapping: git commit"));
            handle_commit(args);
        }
        "pre-commit" => {
            // Backwards compatibility: do nothing and exit 0
            std::process::exit(0);
        }
        "post-commit" => {
            // Backwards compatibility: do nothing and exit 0
            std::process::exit(0);
        }
        "fetch" => {
            handle_fetch_or_pull("fetch", args);
        }
        "push" => {
            handle_fetch_or_pull("push", args);
        }
        _ => {
            debug_log(&format!("proxying: git {}", command));
            // Proxy all other commands to git
            proxy_to_git(&cli.args);
        }
    }
}

fn handle_checkpoint(args: &[String]) {
    // Parse checkpoint-specific arguments
    let mut author = None;
    let mut show_working_log = false;
    let mut reset = false;
    let mut model = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--author" => {
                if i + 1 < args.len() {
                    author = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("Error: --author requires a value");
                    std::process::exit(1);
                }
            }
            "--show-working-log" => {
                show_working_log = true;
                i += 1;
            }
            "--reset" => {
                reset = true;
                i += 1;
            }
            "--model" => {
                if i + 1 < args.len() {
                    model = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("Error: --model requires a value");
                    std::process::exit(1);
                }
            }

            _ => {
                eprintln!("Unknown checkpoint argument: {}", args[i]);
                std::process::exit(1);
            }
        }
    }

    // Find the git repository
    let repo = match find_repository() {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!("Failed to find repository: {}", e);
            std::process::exit(1);
        }
    };

    // Get the current user name from git config
    let default_user_name = match repo.config() {
        Ok(config) => match config.get_string("user.name") {
            Ok(name) => name,
            Err(_) => {
                eprintln!("Warning: git user.name not configured. Using 'unknown' as author.");
                "unknown".to_string()
            }
        },
        Err(_) => {
            eprintln!("Warning: Failed to get git config. Using 'unknown' as author.");
            "unknown".to_string()
        }
    };

    let final_author = author.as_ref().unwrap_or(&default_user_name);

    if let Err(e) = commands::checkpoint(
        &repo,
        final_author,
        show_working_log,
        reset,
        false,
        model.as_deref(),
        Some(&default_user_name),
    ) {
        eprintln!("Checkpoint failed: {}", e);
        std::process::exit(1);
    }
}

fn handle_blame(args: &[String]) {
    if args.is_empty() {
        eprintln!("Error: blame requires a file argument");
        std::process::exit(1);
    }

    // Find the git repository
    let repo = match find_repository() {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!("Failed to find repository: {}", e);
            std::process::exit(1);
        }
    };

    // Parse blame arguments
    let (file_path, options) = match commands::blame::parse_blame_args(args) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("Failed to parse blame arguments: {}", e);
            std::process::exit(1);
        }
    };

    // Check if this is an interactive terminal
    let is_interactive = std::io::stdout().is_terminal();

    if is_interactive && options.incremental {
        // For incremental mode in interactive terminal, we need special handling
        // This would typically involve a pager like less
        let mut full_args = vec!["blame".to_string()];
        full_args.extend_from_slice(args);
        proxy_to_git(&full_args);
        return;
    }

    if let Err(e) = commands::blame::run(&repo, &file_path, &options) {
        eprintln!("Blame failed: {}", e);
        std::process::exit(1);
    }
}

fn handle_commit(args: &[String]) {
    // Find the git repository
    let repo = match find_repository() {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!("Failed to find repository: {}", e);
            std::process::exit(1);
        }
    };

    // Get the current user name from git config
    let default_user_name = match repo.config() {
        Ok(config) => match config.get_string("user.name") {
            Ok(name) => name,
            Err(_) => {
                eprintln!("Warning: git user.name not configured. Using 'unknown' as author.");
                "unknown".to_string()
            }
        },
        Err(_) => {
            eprintln!("Warning: Failed to get git config. Using 'unknown' as author.");
            "unknown".to_string()
        }
    };

    // Run pre-commit logic
    if let Err(e) = git::pre_commit::pre_commit(&repo, default_user_name.clone()) {
        eprintln!("Pre-commit failed: {}", e);
        std::process::exit(1);
    }

    // Proxy to git commit with interactive support
    let mut full_args = vec!["commit".to_string()];
    full_args.extend_from_slice(args);

    let child = std::process::Command::new("git").args(&full_args).spawn();

    match child {
        Ok(mut child) => {
            // Wait for the process to complete
            let status = child.wait();
            match status {
                Ok(status) => {
                    let code = status.code().unwrap_or(1);
                    // If commit succeeded, run post-commit
                    if code == 0 {
                        if let Err(e) = git::post_commit::post_commit(&repo, false) {
                            eprintln!("Post-commit failed: {}", e);
                        }
                    }
                    std::process::exit(code);
                }
                Err(e) => {
                    eprintln!("Failed to wait for git commit process: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to execute git commit: {}", e);
            std::process::exit(1);
        }
    }
}

fn handle_fetch_or_pull(cmd: &str, args: &[String]) {
    // Find the git repository
    let repo = match find_repository() {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!("Failed to find repository: {}", e);
            std::process::exit(1);
        }
    };
    let remotes = repo.remotes().ok();
    let remote_names: Vec<String> = remotes
        .as_ref()
        .map(|r| {
            (0..r.len())
                .filter_map(|i| r.get(i).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if args.is_empty() {
        // git fetch or git pull (no remote): inject default remote and refspec
        if let Some(default_remote) = get_default_remote(&repo) {
            proxy_to_git(&[
                cmd.to_string(),
                default_remote,
                AI_AUTHORSHIP_REFSPEC.to_string(),
            ]);
        } else {
            eprintln!("No git remotes found.");
            std::process::exit(1);
        }
        return;
    }
    if args.len() == 1 && remote_names.contains(&args[0]) {
        // git fetch <remote> or git pull <remote>: inject refspec after remote
        proxy_to_git(&[
            cmd.to_string(),
            args[0].clone(),
            AI_AUTHORSHIP_REFSPEC.to_string(),
        ]);
        return;
    }
    // More complex: just proxy as-is
    let mut full_args = vec![cmd.to_string()];
    full_args.extend_from_slice(args);
    proxy_to_git(&full_args);
}

fn get_default_remote(repo: &git2::Repository) -> Option<String> {
    if let Ok(remotes) = repo.remotes() {
        if remotes.len() == 0 {
            return None;
        }
        // Prefer 'origin' if it exists
        for i in 0..remotes.len() {
            if let Some(name) = remotes.get(i) {
                if name == "origin" {
                    return Some("origin".to_string());
                }
            }
        }
        // Otherwise, just use the first remote
        remotes.get(0).map(|s| s.to_string())
    } else {
        None
    }
}

fn proxy_to_git(args: &[String]) {
    // Check if this is an interactive command that needs special handling
    let interactive_commands = [
        "add",
        "am",
        "apply",
        "bisect",
        "branch",
        "checkout",
        "cherry-pick",
        "clean",
        "clone",
        "commit",
        "config",
        "fetch",
        "help",
        "init",
        "interactive",
        "merge",
        "mv",
        "notes",
        "pull",
        "push",
        "rebase",
        "remote",
        "reflog",
        "reset",
        "restore",
        "revert",
        "rm",
        "stash",
        "submodule",
        "switch",
        "tag",
        "worktree",
    ];
    let is_interactive = args
        .first()
        .map(|cmd| interactive_commands.contains(&cmd.as_str()))
        .unwrap_or(false);

    if is_interactive {
        // Use spawn for interactive commands
        let child = Command::new("git").args(args).spawn();

        match child {
            Ok(mut child) => {
                let status = child.wait();
                match status {
                    Ok(status) => {
                        std::process::exit(status.code().unwrap_or(1));
                    }
                    Err(e) => {
                        eprintln!("Failed to wait for git process: {}", e);
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                eprintln!("Failed to execute git command: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        // Use output for non-interactive commands
        let output = Command::new("git").args(args).output();

        match output {
            Ok(output) => {
                // Forward stdout and stderr
                if !output.stdout.is_empty() {
                    std::io::stdout().write_all(&output.stdout).unwrap();
                }
                if !output.stderr.is_empty() {
                    std::io::stderr().write_all(&output.stderr).unwrap();
                }

                // Forward the exit code
                std::process::exit(output.status.code().unwrap_or(1));
            }
            Err(e) => {
                eprintln!("Failed to execute git command: {}", e);
                std::process::exit(1);
            }
        }
    }
}

#[allow(dead_code)]
fn parse_file_with_line_range(file_arg: &str) -> (String, Option<(u32, u32)>) {
    if let Some(colon_pos) = file_arg.rfind(':') {
        let file_path = file_arg[..colon_pos].to_string();
        let range_part = &file_arg[colon_pos + 1..];

        if let Some(dash_pos) = range_part.find('-') {
            // Range format: start-end
            let start_str = &range_part[..dash_pos];
            let end_str = &range_part[dash_pos + 1..];

            if let (Ok(start), Ok(end)) = (start_str.parse::<u32>(), end_str.parse::<u32>()) {
                return (file_path, Some((start, end)));
            }
        } else {
            // Single line format: line
            if let Ok(line) = range_part.parse::<u32>() {
                return (file_path, Some((line, line)));
            }
        }
    }
    (file_arg.to_string(), None)
}
