use crate::authorship::ignore::effective_ignore_patterns;
use crate::authorship::internal_db::InternalDatabase;
use crate::authorship::range_authorship;
use crate::authorship::stats::stats_command;
use crate::commands;
use crate::config;
use crate::daemon::ControlRequest;
use crate::git::find_repository;
use crate::git::find_repository_in_path;
use crate::git::repository::{CommitRange, Repository};
use crate::git::sync_authorship::{NotesExistence, fetch_authorship_notes, push_authorship_notes};
use crate::observability::log_message;
use crate::utils::is_interactive_terminal;
use serde::{Deserialize, Serialize};
use std::env;
use std::io::IsTerminal;
use std::io::Read;

pub fn handle_git_ai(args: &[String]) {
    let perf_entry =
        if std::env::var("GIT_AI_DEBUG_PERFORMANCE").is_ok_and(|v| !v.is_empty() && v != "0") {
            Some(std::time::Instant::now())
        } else {
            None
        };

    if args.is_empty() {
        print_help();
        return;
    }

    // Initialize the global telemetry handle so that observability and CAS
    // events are routed over the control socket instead of being written to
    // per-PID log files.
    //
    // Skip for commands that must work without a running background service
    // (help, version, config, d management, debug, upgrade) so users can
    // always diagnose and recover from a broken state.
    let needs_daemon = !matches!(
        args[0].as_str(),
        "help"
            | "--help"
            | "-h"
            | "version"
            | "--version"
            | "-v"
            | "config"
            | "bg"
            | "d"
            | "daemon"
            | "debug"
            | "upgrade"
            | "install-hooks"
            | "install"
            | "uninstall-hooks"
            | "usage"
    );
    if needs_daemon {
        use crate::daemon::telemetry_handle::{
            DaemonTelemetryInitResult, init_daemon_telemetry_handle,
        };
        match init_daemon_telemetry_handle() {
            DaemonTelemetryInitResult::Connected | DaemonTelemetryInitResult::Skipped => {}
            DaemonTelemetryInitResult::Failed(err) => {
                eprintln!(
                    "error: failed to connect to git-ai background service: {}",
                    err
                );
                if args[0].as_str() == "checkpoint" {
                    std::process::exit(0);
                }
                std::process::exit(1);
            }
        }
    }

    // Start DB warmup early for commands that need database access
    if args[0].as_str() == "show-prompt" {
        InternalDatabase::warmup();
    }

    match args[0].as_str() {
        "help" | "--help" | "-h" => {
            print_help();
        }
        "version" | "--version" | "-v" => {
            if cfg!(debug_assertions) {
                println!("{} (debug)", env!("CARGO_PKG_VERSION"));
            } else {
                println!(env!("CARGO_PKG_VERSION"));
            }
            std::process::exit(0);
        }
        "config" => {
            commands::config::handle_config(&args[1..]);
            if is_interactive_terminal() {
                log_message("config", "info", None)
            }
        }
        "debug" => {
            commands::debug::handle_debug(&args[1..]);
        }
        "bg" | "d" | "daemon" => {
            commands::daemon::handle_daemon(&args[1..]);
        }
        "stats" => {
            if is_interactive_terminal() {
                log_message("stats", "info", None)
            }
            handle_stats(&args[1..]);
        }
        "usage" => {
            commands::usage::handle_usage(&args[1..]);
        }
        "analyze" => {
            commands::analyze::handle_analyze(&args[1..]);
            if is_interactive_terminal() {
                log_message("analyze", "info", None)
            }
        }
        "status" => {
            commands::status::handle_status(&args[1..]);
        }
        "show" => {
            commands::show::handle_show(&args[1..]);
        }
        "checkpoint" => {
            if let Some(t) = perf_entry {
                eprintln!(
                    "[perf] checkpoint: entry_overhead={:.1}ms (binary startup + clap + dispatch)",
                    t.elapsed().as_secs_f64() * 1000.0
                );
            }
            handle_checkpoint(&args[1..]);
        }
        "log" => {
            let status = commands::log::handle_log(&args[1..]);
            if is_interactive_terminal() {
                log_message("log", "info", None)
            }
            exit_with_log_status(status);
        }
        "blame" => {
            handle_ai_blame(&args[1..]);
            if is_interactive_terminal() {
                log_message("blame", "info", None)
            }
        }
        "diff" => {
            handle_ai_diff(&args[1..]);
            if is_interactive_terminal() {
                log_message("diff", "info", None)
            }
        }
        "git-path" => {
            let config = config::Config::get();
            println!("{}", config.git_cmd());
            std::process::exit(0);
        }
        "install-hooks" | "install" => match commands::install_hooks::run(&args[1..]) {
            Ok(statuses) => {
                if let Ok(statuses_value) = serde_json::to_value(&statuses) {
                    log_message("install-hooks", "info", Some(statuses_value));
                }
            }
            Err(e) => {
                eprintln!("Install hooks failed: {}", e);
                std::process::exit(1);
            }
        },
        "uninstall-hooks" => match commands::install_hooks::run_uninstall(&args[1..]) {
            Ok(statuses) => {
                if let Ok(statuses_value) = serde_json::to_value(&statuses) {
                    log_message("uninstall-hooks", "info", Some(statuses_value));
                }
            }
            Err(e) => {
                eprintln!("Uninstall hooks failed: {}", e);
                std::process::exit(1);
            }
        },
        "git-hooks" => {
            handle_git_hooks(&args[1..]);
        }
        "ci" => {
            commands::ci_handlers::handle_ci(&args[1..]);
        }
        "upgrade" => {
            commands::upgrade::run_with_args(&args[1..]);
        }
        "flush-metrics-db" => {
            commands::flush_metrics_db::handle_flush_metrics_db(&args[1..]);
        }
        "await" => {
            commands::r#await::handle_await(&args[1..]);
        }
        "login" => {
            commands::login::handle_login(&args[1..]);
        }
        "logout" => {
            commands::logout::handle_logout(&args[1..]);
        }
        "whoami" => {
            commands::whoami::handle_whoami(&args[1..]);
        }
        "exchange-nonce" => {
            commands::exchange_nonce::handle_exchange_nonce(&args[1..]);
        }
        "dash" | "dashboard" => {
            commands::personal_dashboard::handle_personal_dashboard(&args[1..]);
        }
        "show-prompt" => {
            commands::show_prompt::handle_show_prompt(&args[1..]);
        }
        "fetch-notes" => {
            commands::fetch_notes::handle_fetch_notes(&args[1..]);
        }
        "effective-ignore-patterns" => {
            handle_effective_ignore_patterns_internal(&args[1..]);
        }
        "blame-analysis" => {
            handle_blame_analysis_internal(&args[1..]);
        }
        "fetch-authorship-notes" | "fetch_authorship_notes" => {
            handle_fetch_authorship_notes_internal(&args[1..]);
        }
        "push-authorship-notes" | "push_authorship_notes" => {
            handle_push_authorship_notes_internal(&args[1..]);
        }
        "notes" => {
            handle_notes_subcommand(&args[1..]);
        }
        _ => {
            println!("Unknown git-ai command: {}", args[0]);
            std::process::exit(1);
        }
    }
}

/// Dispatch `git-ai notes <subcommand>` commands.
pub(crate) fn handle_notes_subcommand(args: &[String]) {
    let subcommand = args.first().map(|s| s.as_str()).unwrap_or("--help");
    match subcommand {
        "migrate" => {
            commands::notes_migrate::handle_notes_migrate(&args[1..]);
        }
        // Hidden: in-memory reference implementation of the notes backend HTTP
        // contract. Intentionally not advertised in `--help`; it is for
        // developers, tests, and benchmarks, not end users.
        "serve" => {
            handle_notes_serve(&args[1..]);
        }
        "--help" | "-h" | "help" => {
            eprintln!("git ai notes - Notes backend management commands");
            eprintln!();
            eprintln!("Usage: git ai notes <subcommand> [options]");
            eprintln!();
            eprintln!("Subcommands:");
            eprintln!("  migrate    Bulk-upload existing git notes to the HTTP backend");
            eprintln!();
            eprintln!("Run 'git ai notes <subcommand> --help' for details.");
        }
        other => {
            eprintln!("Unknown git-ai notes subcommand: {}", other);
            eprintln!("Run 'git ai notes --help' for usage.");
            std::process::exit(1);
        }
    }
}

/// `git-ai notes serve` — run the in-memory reference notes backend.
///
/// This is a developer/test tool. The server stores everything in process
/// memory and accepts any auth header. See
/// `crate::notes::reference_server` for the wire contract.
fn handle_notes_serve(args: &[String]) {
    let mut bind: String = "127.0.0.1:0".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" if i + 1 < args.len() => {
                bind = args[i + 1].clone();
                i += 2;
            }
            "--port" if i + 1 < args.len() => {
                bind = format!("127.0.0.1:{}", args[i + 1]);
                i += 2;
            }
            "--help" | "-h" => {
                eprintln!(
                    "git ai notes serve - Run the in-memory notes backend reference server\n\
                     \n\
                     Usage: git ai notes serve [--bind <addr:port>] [--port <port>]\n\
                     \n\
                     This is a reference implementation. All notes are stored in process\n\
                     memory; auth headers are accepted but not validated. It exists to\n\
                     document the HTTP wire contract and to enable local testing of the\n\
                     `notes_backend.kind = http` code path without a real backend."
                );
                return;
            }
            other => {
                eprintln!("Unknown argument to `git ai notes serve`: {}", other);
                std::process::exit(1);
            }
        }
    }

    if let Err(e) = crate::notes::reference_server::run_blocking(&bind) {
        eprintln!("notes reference server failed: {}", e);
        std::process::exit(1);
    }
}

fn print_help() {
    eprintln!("git-ai - git proxy with AI authorship tracking");
    eprintln!();
    eprintln!("Usage: git-ai <command> [args...]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  checkpoint         Checkpoint working changes and attribute author");
    eprintln!(
        "    Presets: claude, cline, codex, continue-cli, cursor, gemini, github-copilot, amp, windsurf, opencode, pi, ai_tab, firebender, human, mock_ai, mock_known_human, known_human"
    );
    eprintln!(
        "    --hook-input <json|stdin>   JSON payload required by presets, or 'stdin' to read from stdin"
    );
    eprintln!("    human [pathspecs...]             Untracked/legacy human checkpoint");
    eprintln!("    mock_ai [pathspecs...]           Test preset accepting optional file pathspecs");
    eprintln!("    mock_known_human [pathspecs...]  Test preset for KnownHuman checkpoints");
    eprintln!("  log [args...]      Show commit log with AI authorship stats");
    eprintln!("                        Use --raw or --notes to include raw authorship note data");
    eprintln!("  blame <file>       Git blame with AI authorship overlay");
    eprintln!("  diff <commit|range>  Show diff with AI authorship annotations");
    eprintln!("    <commit>              Diff from commit's parent to commit");
    eprintln!("    <commit1>..<commit2>  Diff between two commits");
    eprintln!("    --json                 Output in JSON format");
    eprintln!(
        "    --include-stats        Include commit_stats in JSON output (single commit only)"
    );
    eprintln!(
        "    --all-prompts          Include all prompts from commit note in JSON output (single commit only)"
    );
    eprintln!("  stats [commit]     Show AI authorship statistics for a commit");
    eprintln!("    --json                 Output in JSON format");
    eprintln!("  usage              Show local AI usage statistics");
    eprintln!("    --period <1d|3d|7d|30d>  Time window (default: 30d)");
    eprintln!("    --json                 Output in JSON format");
    eprintln!("  analyze [beta]      Analyze agent sessions and effectiveness");
    eprintln!("  status             Show uncommitted AI authorship status (debug)");
    eprintln!("    --json                 Output in JSON format");
    eprintln!(
        "    --diff-only            Report only current-diff stats, omitting the per-checkpoint breakdown"
    );
    eprintln!("  show <rev|range>   Display authorship logs for a revision or range");
    eprintln!("  show-prompt <id>   Display a prompt record by its ID");
    eprintln!("    --commit <rev>        Look in a specific commit only");
    eprintln!(
        "    --offset <n>          Skip n occurrences (0 = most recent, mutually exclusive with --commit)"
    );
    eprintln!("  config             View and manage git-ai configuration");
    eprintln!("                        Show all config as formatted JSON");
    eprintln!("    <key>                 Show specific config value (supports dot notation)");
    eprintln!("    set <key> <value>     Set a config value (arrays: single value = [value])");
    eprintln!("    --add <key> <value>   Add to array or upsert into object");
    eprintln!("    unset <key>           Remove config value (reverts to default)");
    eprintln!("  debug              Print support/debug diagnostics");
    eprintln!("  bg                 Run and control git-ai background service");
    eprintln!("  install-hooks      Install git hooks for AI authorship tracking");
    eprintln!("    --skills               Also install agent skill files");
    eprintln!("    --visual-studio-extension");
    eprintln!("                           Also install the Visual Studio extension on Windows");
    eprintln!("  uninstall-hooks    Remove git-ai hooks from all detected tools");
    eprintln!("  ci                 Continuous integration utilities");
    eprintln!("    github                 GitHub CI helpers");
    eprintln!("  git-path           Print the path to the underlying git executable");
    eprintln!("  await [beta]       Wait for the background service to finish all work");
    eprintln!("    --timeout <seconds>    Maximum time to wait (default: 30)");
    eprintln!("  upgrade            Check for updates and install if available");
    eprintln!("    --force               Reinstall latest version even if already up to date");
    eprintln!("  fetch-notes [remote] Synchronously fetch AI authorship notes");
    eprintln!("    --remote <name>       Explicit remote name (default: upstream or origin)");
    eprintln!("    --json                Output result as JSON");
    eprintln!("  login              Authenticate with Git AI");
    eprintln!("  logout             Clear stored credentials");
    eprintln!("  whoami             Show auth state and login identity");
    eprintln!("  version, -v, --version     Print the git-ai version");
    eprintln!("  help, -h, --help           Show this help message");
    eprintln!();
    std::process::exit(0);
}

fn handle_checkpoint(args: &[String]) {
    let perf = std::env::var("GIT_AI_DEBUG_PERFORMANCE").is_ok_and(|v| !v.is_empty() && v != "0");
    let t0 = std::time::Instant::now();

    let mut hook_input = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--hook-input" => {
                if i + 1 < args.len() {
                    hook_input = Some(strip_utf8_bom(args[i + 1].clone()));
                    if hook_input.as_ref().unwrap() == "stdin" {
                        let mut stdin = std::io::stdin();
                        let mut buffer = Vec::new();
                        if let Err(e) = stdin.read_to_end(&mut buffer) {
                            eprintln!("Failed to read stdin for hook input: {}", e);
                            std::process::exit(0);
                        }
                        let buffer = match decode_hook_input_bytes(buffer) {
                            Ok(buffer) => buffer,
                            Err(e) => {
                                eprintln!("Failed to decode stdin for hook input: {}", e);
                                std::process::exit(0);
                            }
                        };
                        if buffer.trim().is_empty() {
                            eprintln!("No hook input provided (via --hook-input or stdin).");
                            std::process::exit(0);
                        }
                        hook_input = Some(strip_utf8_bom(buffer));
                    } else if hook_input.as_ref().unwrap().trim().is_empty() {
                        eprintln!("Error: --hook-input requires a value");
                        std::process::exit(0);
                    }
                    i += 2;
                } else {
                    eprintln!("Error: --hook-input requires a value or 'stdin' to read from stdin");
                    std::process::exit(0);
                }
            }
            _ => {
                i += 1;
            }
        }
    }

    if perf {
        eprintln!(
            "[perf] checkpoint: arg_parse={:.1}ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }

    let (preset_name, file_args): (&str, &[String]) = if args.is_empty() {
        ("human", &[])
    } else if args[0] == "--" {
        ("human", &args[1..])
    } else if crate::commands::checkpoint_agent::presets::resolve_preset(args[0].as_str()).is_err()
    {
        eprintln!("Usage: git-ai checkpoint <preset> [--hook-input <json|stdin>] [files...]");
        std::process::exit(0);
    } else {
        (args[0].as_str(), &args[1..])
    };

    let effective_hook_input =
        hook_input.unwrap_or_else(|| synthesize_hook_input_from_cli_args(preset_name, file_args));

    if perf {
        eprintln!(
            "[perf] checkpoint: synth_hook_input={:.1}ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }

    let t_orchestrator = std::time::Instant::now();
    let requests = match crate::commands::checkpoint_agent::orchestrator::execute_preset_checkpoint(
        preset_name,
        &effective_hook_input,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{} preset error: {}", preset_name, e);
            std::process::exit(0);
        }
    };

    if perf {
        eprintln!(
            "[perf] checkpoint: orchestrator={:.1}ms (requests={}, files={})",
            t_orchestrator.elapsed().as_secs_f64() * 1000.0,
            requests.len(),
            requests.iter().map(|r| r.files.len()).sum::<usize>(),
        );
    }

    if requests.is_empty() {
        std::process::exit(0);
    }

    for request in &requests {
        for file in &request.files {
            if !file.path.is_absolute() {
                eprintln!("Error: file path must be absolute: {}", file.path.display());
                std::process::exit(0);
            }
        }
    }

    // Check repository allowlist before sending to daemon.
    // Skip entirely when no allow/exclude filters are configured (common case)
    // to avoid spawning a `git remote -v` subprocess.
    let t_allowlist = std::time::Instant::now();
    {
        let config = config::Config::get();
        if config.has_repository_filters() {
            let mut checked_repos = std::collections::HashSet::new();
            for request in &requests {
                for file in &request.files {
                    if checked_repos.insert(file.repo_work_dir.clone())
                        && let Ok(repo) =
                            crate::git::repository::discover_repository_in_path_no_git_exec(
                                &file.repo_work_dir,
                            )
                        && !config.is_allowed_repository(&Some(repo))
                    {
                        eprintln!(
                            "Skipping checkpoint because repository is excluded or not in allow_repositories list"
                        );
                        std::process::exit(0);
                    }
                }
            }
        }
    }

    if perf {
        eprintln!(
            "[perf] checkpoint: allowlist={:.1}ms",
            t_allowlist.elapsed().as_secs_f64() * 1000.0
        );
    }

    let t_daemon_config = std::time::Instant::now();
    let daemon_config =
        crate::daemon::DaemonConfig::from_env_or_default_paths().map_err(|e| e.to_string());

    let config = match daemon_config {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Background worker unavailable: {}", e);
            std::process::exit(0);
        }
    };

    if perf {
        eprintln!(
            "[perf] checkpoint: daemon_config={:.1}ms",
            t_daemon_config.elapsed().as_secs_f64() * 1000.0
        );
    }

    let mut sent_count = 0u64;
    for request in requests {
        let t_send = std::time::Instant::now();
        let control_request = ControlRequest::CheckpointRun {
            request: Box::new(request),
        };
        let send_result =
            crate::daemon::send_control_request(&config.control_socket_path, &control_request);
        if perf {
            eprintln!(
                "[perf] checkpoint: ipc_send={:.1}ms",
                t_send.elapsed().as_secs_f64() * 1000.0,
            );
        }
        if let Err(e) = send_result {
            eprintln!("Failed to send checkpoint to background worker: {}", e);
            std::process::exit(0);
        }
        sent_count += 1;
    }

    if std::env::var_os("GIT_AI_TEST_DB_PATH").is_some() {
        println!("checkpoint_requests={}", sent_count);
    }

    if perf {
        eprintln!(
            "[perf] checkpoint: total={:.1}ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }
}

fn strip_utf8_bom(input: String) -> String {
    if let Some(stripped) = input.strip_prefix('\u{feff}') {
        stripped.to_string()
    } else {
        input
    }
}

fn decode_hook_input_bytes(bytes: Vec<u8>) -> Result<String, String> {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return decode_utf16_hook_input(&bytes[2..], Utf16Endian::Little);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return decode_utf16_hook_input(&bytes[2..], Utf16Endian::Big);
    }

    match likely_utf16_endian(&bytes) {
        Some(endian) => decode_utf16_hook_input(&bytes, endian),
        None => String::from_utf8(bytes).map_err(|e| e.to_string()),
    }
}

#[derive(Clone, Copy)]
enum Utf16Endian {
    Little,
    Big,
}

fn likely_utf16_endian(bytes: &[u8]) -> Option<Utf16Endian> {
    let sample_len = bytes.len().min(512);
    if sample_len < 8 {
        return None;
    }

    let sample = &bytes[..sample_len];
    let even_nuls = sample.iter().step_by(2).filter(|&&b| b == 0).count();
    let odd_nuls = sample
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|&&b| b == 0)
        .count();
    let min_nuls = sample_len / 8;

    if odd_nuls > min_nuls && odd_nuls > even_nuls.saturating_mul(4) {
        Some(Utf16Endian::Little)
    } else if even_nuls > min_nuls && even_nuls > odd_nuls.saturating_mul(4) {
        Some(Utf16Endian::Big)
    } else {
        None
    }
}

fn decode_utf16_hook_input(bytes: &[u8], endian: Utf16Endian) -> Result<String, String> {
    let chunks = bytes.chunks_exact(2);
    if !chunks.remainder().is_empty() {
        return Err("UTF-16 hook input has an odd byte length".to_string());
    }

    let code_units = chunks.map(|chunk| match endian {
        Utf16Endian::Little => u16::from_le_bytes([chunk[0], chunk[1]]),
        Utf16Endian::Big => u16::from_be_bytes([chunk[0], chunk[1]]),
    });

    String::from_utf16(&code_units.collect::<Vec<u16>>()).map_err(|e| e.to_string())
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EffectiveIgnorePatternsRequest {
    user_patterns: Vec<String>,
    extra_patterns: Vec<String>,
}

#[derive(Debug, Serialize)]
struct EffectiveIgnorePatternsResponse {
    patterns: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlameAnalysisRequest {
    file_path: String,
    #[serde(default)]
    options: commands::blame::GitAiBlameOptions,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorshipRemoteRequest {
    remote_name: String,
}

#[derive(Debug, Serialize)]
struct FetchAuthorshipNotesResponse {
    notes_existence: String,
}

#[derive(Debug, Serialize)]
struct PushAuthorshipNotesResponse {
    ok: bool,
}

fn parse_machine_json_arg(args: &[String], command: &str) -> Result<String, String> {
    if args.len() != 2 || args[0] != "--json" {
        return Err(format!("Usage: git-ai {} --json '<json-payload>'", command));
    }

    let payload = strip_utf8_bom(args[1].clone());
    if payload.trim().is_empty() {
        return Err("JSON payload cannot be empty".to_string());
    }

    Ok(payload)
}

fn emit_machine_json_error(message: impl AsRef<str>) -> ! {
    let payload = serde_json::json!({ "error": message.as_ref() });
    if let Ok(json) = serde_json::to_string(&payload) {
        eprintln!("{}", json);
    } else {
        eprintln!(r#"{{"error":"failed to serialize error payload"}}"#);
    }
    std::process::exit(1);
}

fn print_machine_json(value: &serde_json::Value) {
    match serde_json::to_string(value) {
        Ok(json) => println!("{}", json),
        Err(e) => emit_machine_json_error(format!("Failed to serialize JSON output: {}", e)),
    }
}

fn disable_debug_logs_for_machine_command() {
    // SAFETY: git-ai command handlers run on the main thread and mutate process env
    // before spawning any worker threads for these internal machine commands.
    unsafe {
        std::env::set_var("GIT_AI_DEBUG", "0");
        std::env::remove_var("GIT_AI_DEBUG_PERFORMANCE");
    }
}

fn parse_authorship_remote_request(
    args: &[String],
    command: &str,
) -> (Repository, AuthorshipRemoteRequest) {
    let payload =
        parse_machine_json_arg(args, command).unwrap_or_else(|msg| emit_machine_json_error(msg));

    let request: AuthorshipRemoteRequest = serde_json::from_str(&payload)
        .unwrap_or_else(|e| emit_machine_json_error(format!("Invalid JSON payload: {}", e)));

    if request.remote_name.trim().is_empty() {
        emit_machine_json_error("remote_name cannot be empty");
    }

    let repo = find_repository(&Vec::<String>::new())
        .unwrap_or_else(|e| emit_machine_json_error(format!("Failed to find repository: {}", e)));

    (repo, request)
}

fn notes_existence_label(existence: NotesExistence) -> &'static str {
    match existence {
        NotesExistence::Found => "found",
        NotesExistence::NotFound => "not_found",
    }
}

pub(crate) fn handle_effective_ignore_patterns_internal(args: &[String]) {
    let payload = parse_machine_json_arg(args, "effective-ignore-patterns")
        .unwrap_or_else(|msg| emit_machine_json_error(msg));

    let request: EffectiveIgnorePatternsRequest = serde_json::from_str(&payload)
        .unwrap_or_else(|e| emit_machine_json_error(format!("Invalid JSON payload: {}", e)));

    let repo = find_repository(&Vec::<String>::new())
        .unwrap_or_else(|e| emit_machine_json_error(format!("Failed to find repository: {}", e)));

    let response = EffectiveIgnorePatternsResponse {
        patterns: effective_ignore_patterns(&repo, &request.user_patterns, &request.extra_patterns),
    };

    let response_value = serde_json::to_value(response).unwrap_or_else(|e| {
        emit_machine_json_error(format!("Failed to serialize command response: {}", e))
    });
    print_machine_json(&response_value);
}

pub(crate) fn handle_blame_analysis_internal(args: &[String]) {
    let payload = parse_machine_json_arg(args, "blame-analysis")
        .unwrap_or_else(|msg| emit_machine_json_error(msg));

    let request: BlameAnalysisRequest = serde_json::from_str(&payload)
        .unwrap_or_else(|e| emit_machine_json_error(format!("Invalid JSON payload: {}", e)));

    if request.file_path.trim().is_empty() {
        emit_machine_json_error("file_path cannot be empty");
    }

    let repo = find_repository(&Vec::<String>::new())
        .unwrap_or_else(|e| emit_machine_json_error(format!("Failed to find repository: {}", e)));

    let analysis = repo
        .blame_analysis(&request.file_path, &request.options)
        .unwrap_or_else(|e| emit_machine_json_error(format!("blame_analysis failed: {}", e)));

    let response_value = serde_json::to_value(analysis).unwrap_or_else(|e| {
        emit_machine_json_error(format!("Failed to serialize command response: {}", e))
    });
    print_machine_json(&response_value);
}

pub(crate) fn handle_fetch_authorship_notes_internal(args: &[String]) {
    disable_debug_logs_for_machine_command();
    let (repo, request) = parse_authorship_remote_request(args, "fetch-authorship-notes");

    let notes_existence = fetch_authorship_notes(&repo, &request.remote_name).unwrap_or_else(|e| {
        emit_machine_json_error(format!("fetch_authorship_notes failed: {}", e))
    });

    let response = FetchAuthorshipNotesResponse {
        notes_existence: notes_existence_label(notes_existence).to_string(),
    };
    let response_value = serde_json::to_value(response).unwrap_or_else(|e| {
        emit_machine_json_error(format!("Failed to serialize command response: {}", e))
    });
    print_machine_json(&response_value);
}

pub(crate) fn handle_push_authorship_notes_internal(args: &[String]) {
    disable_debug_logs_for_machine_command();
    let (repo, request) = parse_authorship_remote_request(args, "push-authorship-notes");

    push_authorship_notes(&repo, &request.remote_name).unwrap_or_else(|e| {
        emit_machine_json_error(format!("push_authorship_notes failed: {}", e))
    });

    let response = PushAuthorshipNotesResponse { ok: true };
    let response_value = serde_json::to_value(response).unwrap_or_else(|e| {
        emit_machine_json_error(format!("Failed to serialize command response: {}", e))
    });
    print_machine_json(&response_value);
}

fn handle_ai_blame(args: &[String]) {
    if args.is_empty() {
        eprintln!("Error: blame requires a file argument");
        std::process::exit(1);
    }

    // Find the git repository from current directory
    let current_dir = env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .to_string_lossy()
        .to_string();
    let repo = match find_repository_in_path(&current_dir) {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!("Failed to find repository: {}", e);
            std::process::exit(1);
        }
    };

    // Parse blame arguments
    let (file_path, mut options) = match commands::blame::parse_blame_args(args) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("Failed to parse blame arguments: {}", e);
            std::process::exit(1);
        }
    };

    // Auto-detect ignore-revs-file if not explicitly provided, not disabled via --no-ignore-revs-file,
    // and git version supports --ignore-revs-file (git >= 2.23)
    if options.ignore_revs_file.is_none()
        && !options.no_ignore_revs_file
        && repo.git_supports_ignore_revs_file()
    {
        // First, check git config for blame.ignoreRevsFile
        if let Ok(Some(config_path)) = repo.config_get_str("blame.ignoreRevsFile")
            && !config_path.is_empty()
        {
            // Config path could be relative to repo root or absolute
            if let Ok(workdir) = repo.workdir() {
                let full_path = if std::path::Path::new(&config_path).is_absolute() {
                    std::path::PathBuf::from(&config_path)
                } else {
                    workdir.join(&config_path)
                };
                if full_path.exists() {
                    options.ignore_revs_file = Some(full_path.to_string_lossy().to_string());
                }
            }
        }

        // If still not set, check for .git-blame-ignore-revs in the repository root
        if options.ignore_revs_file.is_none()
            && let Ok(workdir) = repo.workdir()
        {
            let ignore_revs_path = workdir.join(".git-blame-ignore-revs");
            if ignore_revs_path.exists() {
                options.ignore_revs_file = Some(ignore_revs_path.to_string_lossy().to_string());
            }
        }
    }

    // Check if this is an interactive terminal
    let is_interactive = std::io::stdout().is_terminal();

    if is_interactive && options.incremental {
        // For incremental mode in interactive terminal, we need special handling
        // This would typically involve a pager like less
        eprintln!("Error: incremental mode is not supported in interactive terminal");
        std::process::exit(1);
    }

    let file_path = if !std::path::Path::new(&file_path).is_absolute() {
        let current_dir_path = std::path::PathBuf::from(&current_dir);
        current_dir_path
            .join(&file_path)
            .to_string_lossy()
            .to_string()
    } else {
        file_path
    };

    if let Err(e) = repo.blame(&file_path, &options) {
        eprintln!("Blame failed: {}", e);
        std::process::exit(1);
    }
}

fn handle_ai_diff(args: &[String]) {
    let current_dir = env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .to_string_lossy()
        .to_string();
    let repo = match find_repository_in_path(&current_dir) {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!("Failed to find repository: {}", e);
            std::process::exit(1);
        }
    };
    if let Err(e) = commands::diff::handle_diff(&repo, args) {
        eprintln!("Diff failed: {}", e);
        std::process::exit(1);
    }
}

fn handle_stats(args: &[String]) {
    // Find the git repository
    let repo = match find_repository(&Vec::<String>::new()) {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!("Failed to find repository: {}", e);
            std::process::exit(1);
        }
    };
    // Parse stats-specific arguments
    let mut json_output = false;
    let mut commit_sha = None;
    let mut commit_range: Option<CommitRange> = None;
    let mut ignore_patterns: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => {
                json_output = true;
                i += 1;
            }
            "--ignore" => {
                // Collect all arguments after --ignore until we hit another flag or commit SHA
                // This supports shell glob expansion: `--ignore *.lock` expands to `--ignore Cargo.lock package.lock`
                i += 1;
                let mut found_pattern = false;
                while i < args.len() {
                    let arg = &args[i];
                    // Stop if we hit another flag
                    if arg.starts_with("--") {
                        break;
                    }
                    // Stop if this looks like a commit SHA or range (contains ..)
                    if arg.contains("..")
                        || (commit_sha.is_none() && !found_pattern && arg.len() >= 7)
                    {
                        // Could be a commit SHA, stop collecting patterns
                        break;
                    }
                    ignore_patterns.push(arg.clone());
                    found_pattern = true;
                    i += 1;
                }
                if !found_pattern {
                    eprintln!("--ignore requires at least one pattern argument");
                    std::process::exit(1);
                }
            }
            _ => {
                // First non-flag argument is treated as commit SHA or range
                if commit_sha.is_none() {
                    let arg = &args[i];
                    // Check if this is a commit range (contains "..")
                    if arg.contains("..") {
                        let parts: Vec<&str> = arg.split("..").collect();
                        if parts.len() == 2 {
                            match CommitRange::new_infer_refname(
                                &repo,
                                normalize_head_rev(parts[0]),
                                normalize_head_rev(parts[1]),
                                // @todo this is probably fine, but we might want to give users an option to override from this command.
                                None,
                            ) {
                                Ok(range) => {
                                    commit_range = Some(range);
                                }
                                Err(e) => {
                                    eprintln!("Failed to create commit range: {}", e);
                                    std::process::exit(1);
                                }
                            }
                        } else {
                            eprintln!("Invalid commit range format. Expected: <commit>..<commit>");
                            std::process::exit(1);
                        }
                    } else {
                        commit_sha = Some(normalize_head_rev(arg));
                    }
                    i += 1;
                } else {
                    eprintln!("Unknown stats argument: {}", args[i]);
                    std::process::exit(1);
                }
            }
        }
    }

    let effective_patterns = effective_ignore_patterns(&repo, &ignore_patterns, &[]);

    // Handle commit range if detected
    if let Some(range) = commit_range {
        match range_authorship::range_authorship(range, false, &effective_patterns, None) {
            Ok(stats) => {
                if json_output {
                    let json_str = serde_json::to_string(&stats).unwrap();
                    println!("{}", json_str);
                } else {
                    range_authorship::print_range_authorship_stats(&stats);
                }
            }
            Err(e) => {
                eprintln!("Range authorship failed: {}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    if let Err(e) = stats_command(
        &repo,
        commit_sha.as_deref(),
        json_output,
        &effective_patterns,
    ) {
        match e {
            crate::error::GitAiError::Generic(msg) if msg.starts_with("No commit found:") => {
                eprintln!("{}", msg);
            }
            _ => {
                eprintln!("Stats failed: {}", e);
            }
        }
        std::process::exit(1);
    }
}

/// Normalise a revision token that the user may have typed with a lowercase
/// "head" prefix.  On case-insensitive file systems (macOS) git accepts both
/// "head" and "HEAD", but in a linked worktree "head" can resolve to the
/// *main* repository's HEAD file rather than the worktree's own HEAD, so the
/// wrong commit is used.  On case-sensitive file systems (Linux) "head"
/// simply fails with "Not a valid revision".  Normalising to uppercase "HEAD"
/// before passing to git fixes both issues.
///
/// Only the four-character prefix is replaced; suffixes like `~2`, `^1` or
/// `@{0}` are preserved verbatim.
fn normalize_head_rev(rev: &str) -> String {
    if rev.len() >= 4 && rev[..4].eq_ignore_ascii_case("head") {
        let suffix = &rev[4..];
        if suffix.is_empty()
            || suffix.starts_with('~')
            || suffix.starts_with('^')
            || suffix.starts_with('@')
        {
            return format!("HEAD{}", suffix);
        }
    }
    rev.to_string()
}

fn handle_git_hooks(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("remove") | Some("uninstall") => {
            let repo = match find_repository(&Vec::<String>::new()) {
                Ok(repo) => repo,
                Err(e) => {
                    eprintln!("Failed to find repository: {}", e);
                    std::process::exit(1);
                }
            };

            match commands::git_hook_handlers::remove_repo_hooks(&repo, false) {
                Ok(report) => {
                    let status = if report.changed { "removed" } else { "ok" };
                    println!(
                        "repo hooks {}: {}",
                        status,
                        report.managed_hooks_path.to_string_lossy()
                    );
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Failed to remove repo hooks: {}", e);
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("The git core hooks feature has been sunset.");
            eprintln!("Usage: git-ai git-hooks remove");
            std::process::exit(1);
        }
    }
}

/// Synthesize JSON hook_input from CLI args for mock/test presets that can be
/// invoked without --hook-input.
fn synthesize_hook_input_from_cli_args(preset_name: &str, remaining_args: &[String]) -> String {
    match preset_name {
        "human" | "mock_ai" | "mock_known_human" => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let mut paths: Vec<String> = remaining_args
                .iter()
                .filter(|a| !a.starts_with("--"))
                .map(|s| {
                    let p = std::path::Path::new(s.as_str());
                    if p.is_absolute() {
                        s.clone()
                    } else {
                        cwd.join(p).to_string_lossy().to_string()
                    }
                })
                .collect();
            if paths.is_empty() {
                paths = discover_dirty_files_from_status(&cwd);
            }
            serde_json::json!({
                "file_paths": paths,
                "cwd": cwd.to_string_lossy(),
            })
            .to_string()
        }
        "known_human" => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let mut editor = "unknown".to_string();
            let mut editor_version = "unknown".to_string();
            let mut extension_version = "unknown".to_string();
            let mut files: Vec<String> = Vec::new();
            let mut i = 0usize;
            while i < remaining_args.len() {
                match remaining_args[i].as_str() {
                    "--editor" if i + 1 < remaining_args.len() => {
                        editor = remaining_args[i + 1].clone();
                        i += 2;
                    }
                    "--editor-version" if i + 1 < remaining_args.len() => {
                        editor_version = remaining_args[i + 1].clone();
                        i += 2;
                    }
                    "--extension-version" if i + 1 < remaining_args.len() => {
                        extension_version = remaining_args[i + 1].clone();
                        i += 2;
                    }
                    "--" => {
                        files.extend(remaining_args[i + 1..].iter().map(|s| {
                            let p = std::path::Path::new(s.as_str());
                            if p.is_absolute() {
                                s.clone()
                            } else {
                                cwd.join(p).to_string_lossy().to_string()
                            }
                        }));
                        break;
                    }
                    arg if !arg.starts_with("--") => {
                        let p = std::path::Path::new(arg);
                        if p.is_absolute() {
                            files.push(arg.to_string());
                        } else {
                            files.push(cwd.join(p).to_string_lossy().to_string());
                        }
                        i += 1;
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            serde_json::json!({
                "editor": editor,
                "editor_version": editor_version,
                "extension_version": extension_version,
                "cwd": cwd.to_string_lossy(),
                "edited_filepaths": files,
            })
            .to_string()
        }
        _ => String::new(),
    }
}

fn discover_dirty_files_from_status(cwd: &std::path::Path) -> Vec<String> {
    let repo_root = crate::git::repository::discover_repository_in_path_no_git_exec(cwd)
        .ok()
        .and_then(|r| r.workdir().ok())
        .unwrap_or_else(|| cwd.to_path_buf());

    let args = vec![
        "-C".to_string(),
        cwd.to_string_lossy().to_string(),
        "status".to_string(),
        "--porcelain".to_string(),
        "-uall".to_string(),
    ];
    let output = crate::git::repository::exec_git(&args).ok();
    let Some(output) = output else {
        return vec![];
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            let raw_file = line[3..].trim();
            if raw_file.is_empty() {
                return None;
            }
            let unescaped = crate::utils::unescape_git_path(raw_file);
            let mut file = unescaped.as_str();
            // Renames show as "old_name -> new_name"; take only the new name
            if let Some(arrow_pos) = file.find(" -> ") {
                file = &file[arrow_pos + 4..];
            }
            let p = std::path::Path::new(file);
            if p.is_absolute() {
                Some(file.to_string())
            } else {
                Some(repo_root.join(p).to_string_lossy().to_string())
            }
        })
        .collect()
}

/// Exit mirroring the child's termination status, re-raising the original
/// signal on Unix so the calling shell sees the correct termination reason
/// (e.g. SIGPIPE from `git ai log | head`).
fn exit_with_log_status(status: std::process::ExitStatus) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
                libc::raise(sig);
            }
            unreachable!();
        }
    }
    std::process::exit(status.code().unwrap_or(1));
}
