use clap::Parser;
use git_ai::commands;
use git_ai::utils::{SuperuserCheckResult, check_superuser_guard, print_superuser_warning};

#[derive(Parser)]
#[command(name = "git-ai")]
#[command(about = "git proxy with AI authorship tracking", long_about = None)]
#[command(disable_help_flag = true, disable_version_flag = true)]
struct Cli {
    /// Git command and arguments
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

fn is_superuser_exempt_command(args: &[String]) -> bool {
    let first = match args.first() {
        Some(a) => a.as_str(),
        None => return true,
    };
    matches!(
        first,
        "help"
            | "--help"
            | "-h"
            | "version"
            | "--version"
            | "-v"
            | "upgrade"
            | "debug"
            | "uninstall"
            | "uninstall-hooks"
    ) || (first == "bg" || first == "d" || first == "daemon")
        && args
            .get(1)
            .is_some_and(|s| s == "run" || s == "status" || s == "shutdown")
}

fn main() {
    // Get the binary name that was called
    let binary_name = std::env::args_os()
        .next()
        .and_then(|arg| arg.into_string().ok())
        .and_then(|path| {
            std::path::Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or("git-ai".to_string());

    if commands::git_hook_handlers::is_git_hook_binary_name(&binary_name) {
        eprintln!(
            "git-ai: the git core hooks feature has been sunset.\n\
             To remove the deprecated git-ai hook symlinks from this repository, run:\n\
             \n\
             \x20 git-ai git-hooks remove\n"
        );
        std::process::exit(0);
    }

    let cli = Cli::parse();

    #[cfg(debug_assertions)]
    {
        if std::env::var("GIT_AI").as_deref() == Ok("git") {
            commands::git_handlers::handle_git(&cli.args);
            return;
        }
    }

    if binary_name == "git-ai" || binary_name == "git-ai.exe" {
        // Block elevated privileges to prevent creating root-owned files
        // that break normal-user daemon startup. Only applies to direct
        // `git-ai` commands (not the git proxy, which must stay transparent).
        // Exempt commands that must work regardless (upgrade, daemon run, help, etc.).
        if !is_superuser_exempt_command(&cli.args) {
            match check_superuser_guard() {
                SuperuserCheckResult::WarnFutureBlock => print_superuser_warning(),
                SuperuserCheckResult::AllowedWithWarning => {
                    eprintln!(
                        "[git-ai] warning: running as superuser (GIT_AI_ALLOW_SUPERUSER is set)"
                    );
                }
                SuperuserCheckResult::Allowed => {}
            }
        }
        commands::git_ai_handlers::handle_git_ai(&cli.args);
        std::process::exit(0);
    }

    commands::git_handlers::handle_git(&cli.args);
}
