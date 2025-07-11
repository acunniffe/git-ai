mod commands;
mod error;
mod git;
mod log_fmt;

use clap::{Parser, Subcommand};
use git::find_repository;

use std::env;
use std::process::Command;

/// Print debug messages with yellow [git-ai] prefix when in development mode
fn eprint_debug(msg: &str) {
    // Check if we're in development mode (cargo run) or production
    let is_dev = env::var("CARGO_RUNNING").is_ok()
        || env::var("RUST_BACKTRACE").is_ok()
        || cfg!(debug_assertions);

    if is_dev {
        // ANSI escape codes for yellow text
        let yellow = "\x1b[33m";
        let reset = "\x1b[0m";
        eprintln!("{}{}[git-ai]{} {}", yellow, yellow, reset, msg);
    }
}

#[derive(Parser)]
#[command(name = "git-ai")]
#[command(about = "track AI authorship and prompts in git", long_about = None)]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Pass through to git command (e.g., git-ai pull -> git pull)
    #[arg(trailing_var_arg = true)]
    git_args: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// [tool use] create a checkpoint with the current working directory state
    Checkpoint {
        /// author of the checkpoint
        #[arg(long)]
        author: Option<String>,
        /// show log of working copy changes
        #[arg(long)]
        show_working_log: bool,
        /// rest working copy changes
        #[arg(long)]
        reset: bool,
        /// AI model + version
        #[arg(long)]
        model: Option<String>,
    },
    /// line-by-line ownership for a file
    Blame {
        /// file to blame (can include line range like "file.rs:10-20")
        file: String,
    },
    /// show authorship statistics for a commit
    Stats {
        /// commit SHA to analyze (defaults to HEAD)
        sha: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();

    // If we have a git-ai specific command, handle it
    if let Some(command) = cli.command {
        handle_git_ai_command(command);
        return;
    }

    // Otherwise, proxy to git
    if cli.git_args.is_empty() {
        eprintln!("Usage: git-ai <git-command> or git-ai <git-ai-command>");
        eprintln!("Examples:");
        eprintln!("  git-ai pull                    # git pull");
        eprintln!("  git-ai commit -m 'message'     # git commit with pre/post hooks");
        eprintln!("  git-ai checkpoint              # create checkpoint");
        std::process::exit(1);
    }

    let git_command = &cli.git_args[0];
    let git_args = &cli.git_args[1..];

    // Handle special cases that need wrapping
    match git_command.as_str() {
        "commit" => handle_git_commit(git_args),
        "blame" => handle_git_blame(git_args),
        _ => proxy_to_git(git_command, git_args),
    }
}

fn handle_git_ai_command(command: Commands) {
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

    // Execute the command
    if let Err(e) = match command {
        Commands::Checkpoint {
            author,
            show_working_log,
            reset,
            model,
        } => {
            let final_author = author.as_ref().unwrap_or(&default_user_name);
            let result = commands::checkpoint(
                &repo,
                final_author,
                show_working_log,
                reset,
                false,
                model.as_deref(),
                Some(&default_user_name),
            );
            // Convert the tuple result to unit result to match other commands
            result.map(|_| ())
        }
        Commands::Blame { file } => {
            // Parse file argument for line range (e.g., "file.rs:10-20" or "file.rs:10")
            let (file_path, line_range) = parse_file_with_line_range(&file);
            // Convert the blame result to unit result to match other commands
            commands::blame(&repo, &file_path, line_range).map(|_| ())
        }
        Commands::Stats { sha } => {
            let sha = sha.as_deref().unwrap_or("HEAD");
            commands::stats(&repo, sha)
        }
    } {
        eprintln!("Command failed: {}", e);
        std::process::exit(1);
    }
}

fn handle_git_commit(args: &[String]) {
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
            Err(_) => "unknown".to_string(),
        },
        Err(_) => "unknown".to_string(),
    };

    // Run pre-commit hook
    if let Err(e) = commands::pre_commit(&repo, default_user_name.clone()) {
        eprintln!("Pre-commit hook failed: {}", e);
        std::process::exit(1);
    }
    eprint_debug("ran pre-commit hook");

    // Build git commit command
    let mut git_cmd = Command::new("git");
    git_cmd.arg("commit");
    git_cmd.args(args);

    // Run git commit
    let status = match git_cmd.status() {
        Ok(status) => status,
        Err(e) => {
            eprint_debug(&format!("Failed to execute git commit: {}", e));
            std::process::exit(1);
        }
    };

    // If git commit failed, exit with the same code
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    // Run post-commit hook
    if let Err(e) = commands::post_commit(&repo, false) {
        eprintln!("Post-commit hook failed: {}", e);
        // Don't exit here as the commit was successful
    }

    eprint_debug("ran post-commit hook");
}

fn proxy_to_git(command: &str, args: &[String]) {
    let mut git_cmd = Command::new("git");
    git_cmd.arg(command);

    match command {
        "fetch" => {
            // For simple fetch commands, append AI authorship refspecs
            if args.is_empty() || (args.len() == 1 && !args[0].starts_with('-')) {
                // git fetch [remote] - if no remote, defaults to origin
                let mut new_args = Vec::new();
                if let Ok(repo) = find_repository() {
                    let remote = args.first().map(|s| s.as_str()).unwrap_or("origin");
                    new_args.push(remote.to_string());
                    let fetch_refspecs = get_fetch_refspecs(&repo, remote);
                    new_args.extend(fetch_refspecs);
                    // Add AI authorship refspec
                    new_args.push(format!(
                        "+refs/ai/authorship/*:refs/remotes/{}/ai/authorship/*",
                        remote
                    ));
                } else {
                    // Fallback to original args if no repo found
                    new_args = args.to_vec();
                }
                git_cmd.args(&new_args);
            } else {
                // Complex fetch command, pass through as-is
                git_cmd.args(args);
            }
        }
        "push" => {
            // For simple push commands, append AI authorship refspecs
            if args.is_empty() || (args.len() == 1 && !args[0].starts_with('-')) {
                // git push [remote] - if no remote, defaults to origin
                let mut new_args = Vec::new();
                if let Ok(repo) = find_repository() {
                    let remote = args.first().map(|s| s.as_str()).unwrap_or("origin");
                    new_args.push(remote.to_string());
                    let push_refspecs = get_push_refspecs(&repo, remote);
                    new_args.extend(push_refspecs);
                    // Add AI authorship refspec
                    new_args.push("refs/ai/authorship/*:refs/ai/authorship/*".to_string());
                } else {
                    // Fallback to original args if no repo found
                    new_args = args.to_vec();
                }
                git_cmd.args(&new_args);
            } else {
                // Complex push command, pass through as-is
                git_cmd.args(args);
            }
        }
        _ => {
            git_cmd.args(args);
        }
    }

    for (key, value) in env::vars() {
        git_cmd.env(key, value);
    }

    let status = match git_cmd.status() {
        Ok(status) => status,
        Err(e) => {
            eprintln!("Failed to execute git {}: {}", command, e);
            std::process::exit(1);
        }
    };

    std::process::exit(status.code().unwrap_or(1));
}

fn handle_git_blame(args: &[String]) {
    // Find the git repository
    let repo = match find_repository() {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!("Failed to find repository: {}", e);
            std::process::exit(1);
        }
    };

    // Parse the file argument from git blame args
    if args.is_empty() {
        eprintln!("Usage: git-ai blame <file>");
        std::process::exit(1);
    }

    let file_arg = &args[0];
    let (file_path, line_range) = parse_file_with_line_range(file_arg);

    // Run our custom blame command
    if let Err(e) = commands::blame(&repo, &file_path, line_range) {
        eprintln!("Blame failed: {}", e);
        std::process::exit(1);
    }
}

fn get_fetch_refspecs(_repo: &git2::Repository, remote: &str) -> Vec<String> {
    let output = Command::new("git")
        .args(["config", "--get-all", &format!("remote.{}.fetch", remote)])
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => vec![],
    }
}

fn get_push_refspecs(_repo: &git2::Repository, remote: &str) -> Vec<String> {
    let output = Command::new("git")
        .args(["config", "--get-all", &format!("remote.{}.push", remote)])
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => vec![],
    }
}

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
