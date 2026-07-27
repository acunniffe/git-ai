use crate::config;
use crate::daemon::DaemonConfig;
use crate::error::GitAiError;
use crate::mdm::agents::get_all_installers;
use crate::mdm::hook_installer::HookInstallerParams;
use crate::mdm::skills_installer;
use crate::mdm::spinner::{Spinner, print_diff};
use crate::mdm::utils::get_current_binary_path;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const TRACE2_EVENT_TARGET_KEY: &str = "trace2.eventTarget";
const TRACE2_EVENT_NESTING_KEY: &str = "trace2.eventNesting";
const TRACE2_EVENT_NESTING_VALUE: &str = "0";
const VISUAL_STUDIO_INSTALLER_ID: &str = "visual-studio";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct InstallOptions {
    dry_run: bool,
    verbose: bool,
    install_skills: bool,
    include_visual_studio_extension: bool,
    api_base: Option<String>,
    api_key: Option<String>,
}

/// Installation status for a tool
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStatus {
    /// Tool was not detected on the machine
    NotFound,
    /// Hooks/extensions were successfully installed or updated
    Installed,
    /// Hooks/extensions were already up to date
    AlreadyInstalled,
    /// Installation attempted but failed
    Failed,
}

impl InstallStatus {
    /// Convert status to string representation
    pub fn as_str(&self) -> &'static str {
        match self {
            InstallStatus::NotFound => "not_found",
            InstallStatus::Installed => "installed",
            InstallStatus::AlreadyInstalled => "already_installed",
            InstallStatus::Failed => "failed",
        }
    }
}

/// Detailed install result for metrics tracking
#[derive(Debug, Clone)]
pub struct InstallResult {
    pub status: InstallStatus,
    pub error: Option<String>,
    pub warnings: Vec<String>,
}

impl InstallResult {
    pub fn installed() -> Self {
        Self {
            status: InstallStatus::Installed,
            error: None,
            warnings: Vec::new(),
        }
    }

    pub fn already_installed() -> Self {
        Self {
            status: InstallStatus::AlreadyInstalled,
            error: None,
            warnings: Vec::new(),
        }
    }

    pub fn not_found() -> Self {
        Self {
            status: InstallStatus::NotFound,
            error: None,
            warnings: Vec::new(),
        }
    }

    pub fn failed(msg: impl Into<String>) -> Self {
        Self {
            status: InstallStatus::Failed,
            error: Some(msg.into()),
            warnings: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn with_warning(mut self, warning: impl Into<String>) -> Self {
        self.warnings.push(warning.into());
        self
    }

    /// Get message for ClickHouse (error if failed, else joined warnings)
    pub fn message_for_metrics(&self) -> Option<String> {
        if let Some(err) = &self.error {
            Some(err.clone())
        } else if !self.warnings.is_empty() {
            Some(self.warnings.join("; "))
        } else {
            None
        }
    }
}

/// Convert a HashMap of tool statuses to string keys and values
pub fn to_hashmap(statuses: HashMap<String, InstallStatus>) -> HashMap<String, String> {
    statuses
        .into_iter()
        .map(|(k, v)| (k, v.as_str().to_string()))
        .collect()
}

fn print_amp_plugins_note(installer_id: &str) {
    if installer_id == "amp" {
        println!("  Note: Amp plugins are experimental. Run amp with `PLUGINS=all amp`.");
    }
}

/// Find PIDs of running processes that match any of the given process names.
/// Returns a list of (pid, process_name) tuples for each match found.
fn find_running_pids(process_names: &[&str]) -> Vec<(u32, String)> {
    if process_names.is_empty() {
        return vec![];
    }

    let output = {
        #[cfg(unix)]
        {
            Command::new("ps")
                .args(["axo", "pid,comm"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
        }
        #[cfg(windows)]
        {
            Command::new("tasklist")
                .args(["/FO", "CSV", "/NH"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
        }
    };

    let Ok(output) = output else {
        return vec![];
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results: Vec<(u32, String)> = Vec::new();

    for line in stdout.lines() {
        #[cfg(unix)]
        {
            let trimmed = line.trim();
            // ps output: "  PID COMM" — split on whitespace
            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let pid_str = parts.next().unwrap_or("").trim();
            let comm = parts.next().unwrap_or("").trim();
            // comm may be a full path; extract the basename
            let base = comm.rsplit('/').next().unwrap_or(comm);
            if let Ok(pid) = pid_str.parse::<u32>() {
                for &name in process_names {
                    if base.eq_ignore_ascii_case(name) {
                        results.push((pid, base.to_string()));
                        break;
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            // tasklist CSV: "Image Name","PID",...
            let fields: Vec<&str> = line.split(',').collect();
            if fields.len() >= 2 {
                let image = fields[0].trim_matches('"');
                let pid_str = fields[1].trim_matches('"');
                let base = image.strip_suffix(".exe").unwrap_or(image);
                if let Ok(pid) = pid_str.parse::<u32>() {
                    for &name in process_names {
                        if base.eq_ignore_ascii_case(name) {
                            results.push((pid, base.to_string()));
                            break;
                        }
                    }
                }
            }
        }
    }

    results
}

fn set_global_git_config_value(git_cmd: &str, key: &str, value: &str) -> Result<(), GitAiError> {
    let mut command = Command::new(git_cmd);
    command
        .args(["config", "--global", key, value])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::git::repository::apply_internal_git_env(&mut command);

    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(GitAiError::Generic(format!(
            "failed to set global git config key '{}'",
            key
        )))
    }
}

fn ensure_global_git_config_dirs() -> Result<(), GitAiError> {
    if let Ok(path) = std::env::var("GIT_CONFIG_GLOBAL") {
        let config_path = PathBuf::from(path);
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
    }

    if let Ok(home) = std::env::var("HOME") {
        fs::create_dir_all(home)?;
    }

    Ok(())
}

fn remove_global_git_config_section(git_cmd: &str, section: &str) -> Result<(), GitAiError> {
    let mut command = Command::new(git_cmd);
    command
        .args(["config", "--global", "--remove-section", section])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::git::repository::apply_internal_git_env(&mut command);

    let status = command.status()?;
    // Exit code 128 means the section doesn't exist, which is fine.
    if status.success() || status.code() == Some(128) {
        Ok(())
    } else {
        Err(GitAiError::Generic(format!(
            "failed to remove global git config section '{}'",
            section
        )))
    }
}

fn configure_daemon_trace2(dry_run: bool) -> Result<(), GitAiError> {
    let runtime_config = config::Config::fresh();

    ensure_global_git_config_dirs()?;

    let daemon_config = DaemonConfig::from_env_or_default_paths()?;
    let event_target = daemon_config.trace2_event_target();

    if dry_run {
        return Ok(());
    }

    // Fully reset any existing trace2 config the user may have set
    // (e.g. trace2.normalTarget, trace2.perfTarget, trace2.configParams, etc.)
    // before writing only the keys we need.
    remove_global_git_config_section(runtime_config.git_cmd(), "trace2")?;

    set_global_git_config_value(
        runtime_config.git_cmd(),
        TRACE2_EVENT_TARGET_KEY,
        &event_target,
    )?;
    set_global_git_config_value(
        runtime_config.git_cmd(),
        TRACE2_EVENT_NESTING_KEY,
        TRACE2_EVENT_NESTING_VALUE,
    )?;
    Ok(())
}

fn ensure_daemon(dry_run: bool) {
    if dry_run {
        return;
    }

    // Don't touch daemon inside test harnesses
    if std::env::var_os("GIT_AI_TEST_DB_PATH").is_some()
        || std::env::var_os("GITAI_TEST_DB_PATH").is_some()
    {
        return;
    }

    let Ok(daemon_config) = DaemonConfig::from_env_or_default_paths() else {
        return;
    };

    // Restart daemon so it picks up the freshly-written trace2 config.
    // Uses soft shutdown → hard kill escalation if needed.
    if let Err(e) = crate::commands::daemon::restart_daemon(&daemon_config) {
        eprintln!(
            "[git-ai] warning: failed to restart background service: {}",
            e
        );
    }
}

/// Main entry point for install-hooks command
pub fn run(args: &[String]) -> Result<HashMap<String, String>, GitAiError> {
    let options = parse_install_options(args)?;
    let install_config = InstallConfig {
        api_base: options.api_base.clone().or_else(|| {
            std::env::var("API_BASE")
                .ok()
                .filter(|value| !value.is_empty())
        }),
        api_key: options.api_key.clone().or_else(|| {
            std::env::var("API_KEY")
                .ok()
                .filter(|value| !value.is_empty())
        }),
    };

    // Daemon trace2 config must be in place before any install work starts.
    // Non-fatal: the global git config may be read-only (e.g. Nix store symlink).
    if let Err(e) = configure_daemon_trace2(options.dry_run) {
        eprintln!("Warning: could not configure trace2 (non-fatal): {e}");
    }
    ensure_daemon(options.dry_run);

    // Now that the daemon is (re)started, initialize the telemetry handle so
    // that install-hooks metrics and observability events route through it.
    if !options.dry_run {
        let _ = crate::daemon::telemetry_handle::init_daemon_telemetry_handle();
    }

    // Get absolute path to the current binary
    let binary_path = get_current_binary_path()?;
    persist_install_config_with_values(&binary_path, options.dry_run, &install_config)?;
    let params = HookInstallerParams { binary_path };

    // Run async operations and convert result.
    let statuses = crate::tokio_runtime::block_on(async_run_install(&params, &options))?;

    // Clean up legacy envelope logs directory and related artifacts.
    // These are no longer used — all telemetry now routes through the daemon.
    if !options.dry_run {
        cleanup_legacy_envelope_logs();
    }

    Ok(to_hashmap(statuses))
}

fn parse_install_options(args: &[String]) -> Result<InstallOptions, GitAiError> {
    let mut options = InstallOptions::default();

    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" | "--dry-run=true" => options.dry_run = true,
            "--verbose" | "-v" => options.verbose = true,
            "--skills" => options.install_skills = true,
            "--visual-studio-extension" => options.include_visual_studio_extension = true,
            value if value.starts_with("--api-base=") => {
                options.api_base = non_empty_value(&value[11..]);
            }
            "--api-base" => {
                let value = args.next().ok_or_else(|| {
                    GitAiError::Generic("missing value for --api-base".to_string())
                })?;
                options.api_base = non_empty_value(value);
            }
            value if value.starts_with("--api-key=") => {
                options.api_key = non_empty_value(&value[10..]);
            }
            "--api-key" => {
                let value = args.next().ok_or_else(|| {
                    GitAiError::Generic("missing value for --api-key".to_string())
                })?;
                options.api_key = non_empty_value(value);
            }
            _ => {}
        }
    }

    Ok(options)
}

fn non_empty_value(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn should_include_installer(id: &str, options: &InstallOptions) -> bool {
    options.include_visual_studio_extension || id != VISUAL_STUDIO_INSTALLER_ID
}

#[derive(Default)]
struct InstallConfig {
    api_base: Option<String>,
    api_key: Option<String>,
}

#[cfg(test)]
fn persist_install_config(binary_path: &Path, dry_run: bool) -> Result<bool, GitAiError> {
    persist_install_config_with_values(binary_path, dry_run, &install_config_from_environment())
}

#[cfg(test)]
fn install_config_from_environment() -> InstallConfig {
    InstallConfig {
        api_base: std::env::var("API_BASE").ok().filter(|s| !s.is_empty()),
        api_key: std::env::var("API_KEY").ok().filter(|s| !s.is_empty()),
    }
}

fn persist_install_config_with_values(
    binary_path: &Path,
    dry_run: bool,
    install_config: &InstallConfig,
) -> Result<bool, GitAiError> {
    if dry_run {
        return Ok(false);
    }

    let api_base = &install_config.api_base;
    let api_key = &install_config.api_key;

    if api_base.is_none() && api_key.is_none() {
        return Ok(false);
    }

    let mut file_config = crate::config::load_file_config_public().map_err(GitAiError::Generic)?;
    let mut changed = false;

    if let Some(api_base) = api_base
        && file_config.api_base_url.as_deref() != Some(api_base.as_str())
    {
        file_config.api_base_url = Some(api_base.clone());
        changed = true;
    }

    if let Some(api_key) = api_key
        && file_config.api_key.as_deref() != Some(api_key.as_str())
    {
        file_config.api_key = Some(api_key.clone());
        changed = true;
    }

    if api_base.is_some() {
        let git_path_missing = file_config
            .git_path
            .as_ref()
            .map(|value| value.trim().is_empty())
            .unwrap_or(true);
        if git_path_missing && let Some(git_path) = detect_install_git_path(binary_path) {
            file_config.git_path = Some(git_path);
            changed = true;
        }
    }

    if !changed {
        return Ok(false);
    }

    crate::config::save_file_config(&file_config).map_err(GitAiError::Generic)?;
    Ok(true)
}

fn detect_install_git_path(binary_path: &Path) -> Option<String> {
    let install_dir = binary_path.parent()?;

    #[cfg(windows)]
    {
        parse_git_og_cmd_path(&fs::read_to_string(install_dir.join("git-og.cmd")).ok()?)
    }

    #[cfg(not(windows))]
    {
        let target = fs::read_link(install_dir.join("git-og")).ok()?;
        let resolved = if target.is_absolute() {
            target
        } else {
            install_dir.join(target)
        };
        Some(resolved.to_string_lossy().to_string())
    }
}

#[cfg(windows)]
fn parse_git_og_cmd_path(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let start = line.find('"')?;
        let rest = &line[start + 1..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    })
}

/// Main entry point for uninstall-hooks command
pub fn run_uninstall(args: &[String]) -> Result<HashMap<String, String>, GitAiError> {
    // Parse flags
    let mut dry_run = false;
    let mut verbose = false;
    for arg in args {
        if arg == "--dry-run" || arg == "--dry-run=true" {
            dry_run = true;
        }
        if arg == "--verbose" || arg == "-v" {
            verbose = true;
        }
    }

    // Get absolute path to the current binary
    let binary_path = get_current_binary_path()?;
    let params = HookInstallerParams { binary_path };

    // Run async operations and convert result.
    let statuses = crate::tokio_runtime::block_on(async_run_uninstall(&params, dry_run, verbose))?;
    Ok(to_hashmap(statuses))
}

async fn async_run_install(
    params: &HookInstallerParams,
    options: &InstallOptions,
) -> Result<HashMap<String, InstallStatus>, GitAiError> {
    let mut any_checked = false;
    let mut has_changes = false;
    let mut statuses: HashMap<String, InstallStatus> = HashMap::new();
    // Track detailed results for metrics (tool_id, result)
    let mut detailed_results: Vec<(String, InstallResult)> = Vec::new();

    // === Coding Agents ===
    println!("\n\x1b[1mCoding Agents\x1b[0m");

    let installers = get_all_installers();
    let mut installed_tools: HashSet<String> = HashSet::new();
    // Track agents whose hooks were updated (name, process_names) for restart warnings
    let mut updated_agents: Vec<(String, Vec<String>)> = Vec::new();

    for installer in &installers {
        let name = installer.name();
        let id = installer.id();

        if !should_include_installer(id, options) {
            continue;
        }

        // Check if tool is installed and hooks status
        match installer.check_hooks(params) {
            Ok(check_result) => {
                if !check_result.tool_installed {
                    statuses.insert(id.to_string(), InstallStatus::NotFound);
                    detailed_results.push((id.to_string(), InstallResult::not_found()));
                    continue;
                }

                installed_tools.insert(id.to_string());
                any_checked = true;

                // Install/update hooks (only for tools that use config file hooks)
                if installer.uses_config_hooks() {
                    let spinner = Spinner::new(&format!("{}: checking hooks", name));
                    spinner.start();

                    match installer.install_hooks(params, options.dry_run) {
                        Ok(Some(diff)) => {
                            if options.dry_run {
                                spinner.pending(&format!("{}: Pending updates", name));
                            } else {
                                spinner.success(&format!("{}: Hooks updated", name));
                                print_amp_plugins_note(id);
                            }
                            if options.verbose {
                                println!();
                                print_diff(&diff);
                            }
                            has_changes = true;
                            statuses.insert(id.to_string(), InstallStatus::Installed);
                            detailed_results.push((id.to_string(), InstallResult::installed()));

                            // Track this agent for restart detection (skip in dry-run)
                            if !options.dry_run {
                                let pnames: Vec<String> = installer
                                    .process_names()
                                    .iter()
                                    .map(|s| s.to_string())
                                    .collect();
                                if !pnames.is_empty() {
                                    updated_agents.push((name.to_string(), pnames));
                                }
                            }
                        }
                        Ok(None) => {
                            spinner.success(&format!("{}: Hooks already up to date", name));
                            print_amp_plugins_note(id);
                            statuses.insert(id.to_string(), InstallStatus::AlreadyInstalled);
                            detailed_results
                                .push((id.to_string(), InstallResult::already_installed()));
                        }
                        Err(e) => {
                            let error_msg = e.to_string();
                            spinner.error(&format!("{}: Failed to update hooks", name));
                            eprintln!("  Error: {}", error_msg);
                            statuses.insert(id.to_string(), InstallStatus::Failed);
                            detailed_results
                                .push((id.to_string(), InstallResult::failed(error_msg)));
                        }
                    }
                }

                // Install extras (extensions, git.path, etc.)
                match installer.install_extras(params, options.dry_run) {
                    Ok(results) => {
                        let mut extras_changed = false;
                        for result in results {
                            if result.changed {
                                has_changes = true;
                                extras_changed = true;
                            }
                            if result.changed && !options.dry_run {
                                let extra_spinner = Spinner::new(&result.message);
                                extra_spinner.start();
                                extra_spinner.success(&result.message);
                            } else if result.changed && options.dry_run {
                                let extra_spinner = Spinner::new(&result.message);
                                extra_spinner.start();
                                extra_spinner.pending(&result.message);
                            } else if result.message.contains("already") {
                                let extra_spinner = Spinner::new(&result.message);
                                extra_spinner.start();
                                extra_spinner.success(&result.message);
                            } else if result.message.contains("Unable")
                                || result.message.contains("manually")
                            {
                                let extra_spinner = Spinner::new(&result.message);
                                extra_spinner.start();
                                extra_spinner.pending(&result.message);
                            }
                            if options.verbose
                                && let Some(diff) = result.diff
                            {
                                println!();
                                print_diff(&diff);
                            }

                            // Capture warning-like messages for metrics
                            if (result.message.contains("Unable")
                                || result.message.contains("manually")
                                || result.message.contains("Failed"))
                                && let Some((_, detail)) = detailed_results
                                    .iter_mut()
                                    .find(|(tool_id, _)| tool_id == id)
                            {
                                detail.warnings.push(result.message.clone());
                            }
                        }

                        // Track restart detection for extras-only agents (e.g. JetBrains, VS Code)
                        if extras_changed
                            && !options.dry_run
                            && !updated_agents.iter().any(|(n, _)| n == name)
                        {
                            let pnames: Vec<String> = installer
                                .process_names()
                                .iter()
                                .map(|s| s.to_string())
                                .collect();
                            if !pnames.is_empty() {
                                updated_agents.push((name.to_string(), pnames));
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("  Error installing extras for {}: {}", name, e);
                        // Capture extras error as a warning on the tool's result
                        if let Some((_, detail)) = detailed_results
                            .iter_mut()
                            .find(|(tool_id, _)| tool_id == id)
                        {
                            detail.warnings.push(format!("Extras install error: {}", e));
                        }
                    }
                }
            }
            Err(check_error) => {
                let error_msg = check_error.to_string();
                any_checked = true;
                let spinner = Spinner::new(&format!("{}: checking hooks", name));
                spinner.start();
                spinner.error(&format!("{}: Hook check failed", name));
                eprintln!("  Error: {}", error_msg);
                statuses.insert(id.to_string(), InstallStatus::Failed);
                detailed_results.push((id.to_string(), InstallResult::failed(error_msg)));
            }
        }
    }

    if options.install_skills {
        if let Ok(result) =
            skills_installer::install_skills(options.dry_run, options.verbose, &installed_tools)
            && result.changed
        {
            has_changes = true;
        }
    } else if let Ok(result) = skills_installer::uninstall_skills(options.dry_run, options.verbose)
        && result.changed
    {
        has_changes = true;
    }

    if !any_checked {
        println!("No compatible IDEs or agent configurations detected. Nothing to install.");
    } else if has_changes && options.dry_run {
        println!("\n\x1b[33m⚠ Dry-run mode (default). No changes were made.\x1b[0m");
        println!("To apply these changes, run:");
        println!("\x1b[1m  git-ai install-hooks --dry-run=false\x1b[0m");
    }

    // Check for running agents that had hooks updated and warn about restart
    if !options.dry_run && !updated_agents.is_empty() {
        let mut any_running = false;

        for (agent_name, pnames) in &updated_agents {
            let refs: Vec<&str> = pnames.iter().map(|s| s.as_str()).collect();
            let pids = find_running_pids(&refs);
            if !pids.is_empty() {
                if !any_running {
                    println!(
                        "\n\x1b[33m⚠ The following agents are currently running and must be restarted:\x1b[0m"
                    );
                    any_running = true;
                }
                let pid_list: Vec<String> = pids.iter().map(|(pid, _)| pid.to_string()).collect();
                println!(
                    "  \x1b[1m{}\x1b[0m (PID: {})",
                    agent_name,
                    pid_list.join(", ")
                );
            }
        }

        if any_running {
            println!();
            println!(
                "\x1b[33mRestart the agents listed above for git-ai attribution to take effect.\x1b[0m"
            );
            println!(
                "Any work done before installing git-ai (or before restarting) will be attributed as human."
            );
            println!(
                "This is expected — once you commit and start a fresh session, attribution will work correctly."
            );
            println!(
                "If the issue persists, please open an issue at https://github.com/git-ai-project/git-ai/issues"
            );
        }
    }

    // Emit metrics for each agent/git_client result (only if not dry-run)
    if !options.dry_run {
        emit_install_hooks_metrics(&detailed_results);
    }

    // Warn if git version is below the minimum required for full functionality
    warn_if_git_version_too_old();

    Ok(statuses)
}

/// Minimum git version required for git-ai to function correctly.
/// git 2.22.0 introduced `git worktree list --porcelain` output format improvements
/// and trace2 event logging used by git-ai for attribution.
const MIN_GIT_VERSION: (u32, u32, u32) = (2, 22, 0);

/// Parse a git version string like "git version 2.39.1" into (major, minor, patch).
fn parse_git_version(output: &str) -> Option<(u32, u32, u32)> {
    // Strip the "git version " prefix and any platform suffix (e.g. "(Apple Git-140)")
    let version_str = output.trim().strip_prefix("git version ")?;
    let version_str = version_str.split_whitespace().next()?;
    let mut parts = version_str.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// Print a loud warning if the installed git version is older than MIN_GIT_VERSION.
fn warn_if_git_version_too_old() {
    let mut command = Command::new("git");
    command
        .args(["--version"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::git::repository::apply_internal_git_env(&mut command);

    let output = command.output();

    let version = match output {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout).into_owned();
            parse_git_version(&text)
        }
        Err(_) => None,
    };

    if let Some(v) = version {
        let (maj, min, patch) = MIN_GIT_VERSION;
        if v < (maj, min, patch) {
            let (vmaj, vmin, vpatch) = v;
            eprintln!();
            eprintln!(
                "\x1b[1;31m╔══════════════════════════════════════════════════════════════╗\x1b[0m"
            );
            eprintln!(
                "\x1b[1;31m║  WARNING: git version too old — git-ai will not work         ║\x1b[0m"
            );
            eprintln!(
                "\x1b[1;31m╚══════════════════════════════════════════════════════════════╝\x1b[0m"
            );
            eprintln!(
                "\x1b[1;31mDetected git {}.{}.{} — git-ai requires git >= {}.{}.{}\x1b[0m",
                vmaj, vmin, vpatch, maj, min, patch
            );
            eprintln!("\x1b[33mPlease upgrade git before using git-ai:\x1b[0m");
            eprintln!("  macOS:   brew install git");
            eprintln!(
                "  Ubuntu:  sudo add-apt-repository ppa:git-core/ppa && sudo apt-get update && sudo apt-get install git"
            );
            eprintln!("  Windows: https://git-scm.com/download/win");
            eprintln!();
        }
    }
}

/// Emit metrics events for install-hooks results
fn emit_install_hooks_metrics(results: &[(String, InstallResult)]) {
    use crate::metrics::{EventAttributes, InstallHooksValues};

    let attrs = EventAttributes::with_version(env!("CARGO_PKG_VERSION"));

    for (tool_id, result) in results {
        let mut values = InstallHooksValues::new()
            .tool_id(tool_id.clone())
            .status(result.status.as_str().to_string());

        if let Some(msg) = result.message_for_metrics() {
            values = values.message(msg);
        } else {
            values = values.message_null();
        }

        crate::metrics::record(values, attrs.clone());
    }
}

async fn async_run_uninstall(
    params: &HookInstallerParams,
    dry_run: bool,
    verbose: bool,
) -> Result<HashMap<String, InstallStatus>, GitAiError> {
    let mut any_checked = false;
    let mut has_changes = false;
    let mut statuses: HashMap<String, InstallStatus> = HashMap::new();

    // Uninstall skills first (these are global, not per-agent, silently)
    if let Ok(result) = skills_installer::uninstall_skills(dry_run, verbose) {
        if result.changed {
            has_changes = true;
            statuses.insert("skills".to_string(), InstallStatus::Installed);
        } else {
            statuses.insert("skills".to_string(), InstallStatus::AlreadyInstalled);
        }
    }

    // === Coding Agents ===
    println!("\n\x1b[1mCoding Agents\x1b[0m");

    let installers = get_all_installers();

    for installer in installers {
        let name = installer.name();
        let id = installer.id();

        // Check if tool is installed
        match installer.check_hooks(params) {
            Ok(check_result) => {
                if !check_result.tool_installed {
                    statuses.insert(id.to_string(), InstallStatus::NotFound);
                    continue;
                }

                if !check_result.hooks_installed {
                    statuses.insert(id.to_string(), InstallStatus::NotFound);
                    continue;
                }

                any_checked = true;

                // Uninstall hooks
                let spinner = Spinner::new(&format!("{}: removing hooks", name));
                spinner.start();

                match installer.uninstall_hooks(params, dry_run) {
                    Ok(Some(diff)) => {
                        if dry_run {
                            spinner.pending(&format!("{}: Pending removal", name));
                        } else {
                            spinner.success(&format!("{}: Hooks removed", name));
                        }
                        if verbose {
                            println!();
                            print_diff(&diff);
                        }
                        has_changes = true;
                        statuses.insert(id.to_string(), InstallStatus::Installed);
                    }
                    Ok(None) => {
                        spinner.success(&format!("{}: No hooks to remove", name));
                        statuses.insert(id.to_string(), InstallStatus::AlreadyInstalled);
                    }
                    Err(e) => {
                        spinner.error(&format!("{}: Failed to remove hooks", name));
                        eprintln!("  Error: {}", e);
                        statuses.insert(id.to_string(), InstallStatus::Failed);
                    }
                }

                // Uninstall extras
                match installer.uninstall_extras(params, dry_run) {
                    Ok(results) => {
                        for result in results {
                            if result.changed {
                                has_changes = true;
                            }
                            if !result.message.is_empty() {
                                let extra_spinner = Spinner::new(&result.message);
                                extra_spinner.start();
                                if result.changed {
                                    extra_spinner.success(&result.message);
                                } else {
                                    extra_spinner.pending(&result.message);
                                }
                            }
                            if verbose && let Some(diff) = result.diff {
                                println!();
                                print_diff(&diff);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("  Error uninstalling extras for {}: {}", name, e);
                    }
                }
            }
            Err(e) => {
                eprintln!("  Error checking {}: {}", name, e);
                statuses.insert(id.to_string(), InstallStatus::Failed);
            }
        }
    }

    if !any_checked {
        println!("No git-ai hooks found to uninstall.");
    } else if has_changes && dry_run {
        println!("\n\x1b[33m⚠ Dry-run mode (default). No changes were made.\x1b[0m");
        println!("To apply these changes, run:");
        println!("\x1b[1m  git-ai uninstall-hooks --dry-run=false\x1b[0m");
    } else if !has_changes {
        println!("All git-ai hooks have been removed.");
    }

    Ok(statuses)
}

/// Remove the legacy envelope logs directory and related lock/marker files.
///
/// All telemetry now flows through the daemon control socket, so the per-PID
/// log file system under `~/.git-ai/internal/logs/` is no longer needed.
fn cleanup_legacy_envelope_logs() {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let internal = home.join(".git-ai").join("internal");

    // Remove the entire logs directory
    let logs_dir = internal.join("logs");
    if logs_dir.is_dir() {
        let _ = fs::remove_dir_all(&logs_dir);
    }

    // Remove the flush-logs lock file
    let _ = fs::remove_file(internal.join("flush-logs.lock"));

    // Remove the debounce marker file
    let _ = fs::remove_file(internal.join("last_flush_trigger_ts"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::PathBuf;
    use tempfile::tempdir;

    struct EnvVarGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            // SAFETY: tests marked `serial` avoid concurrent env mutation.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old }
        }

        fn remove(key: &'static str) -> Self {
            let old = std::env::var(key).ok();
            // SAFETY: tests marked `serial` avoid concurrent env mutation.
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, old }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: tests marked `serial` avoid concurrent env mutation.
            unsafe {
                if let Some(old) = &self.old {
                    std::env::set_var(self.key, old);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    fn test_binary_path(install_dir: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            install_dir.join("git-ai.exe")
        }

        #[cfg(not(windows))]
        {
            install_dir.join("git-ai")
        }
    }

    fn write_install_git_marker(install_dir: &Path, git_path: &str) {
        #[cfg(windows)]
        {
            fs::write(
                install_dir.join("git-og.cmd"),
                format!("@echo off\r\n\"{}\" %*\r\n", git_path),
            )
            .unwrap();
        }

        #[cfg(not(windows))]
        {
            std::os::unix::fs::symlink(git_path, install_dir.join("git-og")).unwrap();
        }
    }

    #[test]
    fn parse_install_options_defaults_visual_studio_extension_to_disabled() {
        let options = parse_install_options(&[]).unwrap();

        assert!(!options.include_visual_studio_extension);
        assert!(!should_include_installer(
            VISUAL_STUDIO_INSTALLER_ID,
            &options
        ));
        assert!(should_include_installer("vscode", &options));
    }

    #[test]
    fn parse_install_options_enables_visual_studio_extension_flag() {
        let args = vec![
            "--dry-run".to_string(),
            "--visual-studio-extension".to_string(),
            "--skills".to_string(),
            "-v".to_string(),
        ];
        let options = parse_install_options(&args).unwrap();

        assert!(options.dry_run);
        assert!(options.verbose);
        assert!(options.install_skills);
        assert!(options.include_visual_studio_extension);
        assert!(should_include_installer(
            VISUAL_STUDIO_INSTALLER_ID,
            &options
        ));
    }

    #[test]
    fn parse_install_options_accepts_package_api_configuration() {
        let args = vec![
            "--api-base=https://enterprise.example".to_string(),
            "--api-key".to_string(),
            "sk-enterprise-key".to_string(),
        ];

        let options = parse_install_options(&args).unwrap();

        assert_eq!(
            options.api_base.as_deref(),
            Some("https://enterprise.example")
        );
        assert_eq!(options.api_key.as_deref(), Some("sk-enterprise-key"));
    }

    #[test]
    fn parse_install_options_rejects_missing_package_api_value() {
        let args = vec!["--api-base".to_string()];

        let err = parse_install_options(&args).unwrap_err();

        assert!(err.to_string().contains("missing value for --api-base"));
    }

    #[test]
    #[cfg(not(windows))]
    #[serial]
    fn install_reports_cline_hook_check_errors_as_failed() {
        let temp = tempdir().unwrap();
        let storage = temp.path().join("cline-storage");
        fs::create_dir_all(&storage).unwrap();
        fs::write(temp.path().join("Documents"), "not a directory").unwrap();

        let _home = EnvVarGuard::set("HOME", temp.path().to_str().unwrap());
        let _cline_storage =
            EnvVarGuard::set("GIT_AI_CLINE_STORAGE_PATH", storage.to_str().unwrap());

        let statuses = run(&["--dry-run".to_string()]).unwrap();

        assert_eq!(statuses.get("cline").map(String::as_str), Some("failed"));
    }

    #[test]
    #[cfg(not(windows))]
    #[serial]
    fn uninstall_reports_cline_hook_check_errors_as_failed() {
        let temp = tempdir().unwrap();
        let storage = temp.path().join("cline-storage");
        fs::create_dir_all(&storage).unwrap();
        fs::write(temp.path().join("Documents"), "not a directory").unwrap();

        let _home = EnvVarGuard::set("HOME", temp.path().to_str().unwrap());
        let _cline_storage =
            EnvVarGuard::set("GIT_AI_CLINE_STORAGE_PATH", storage.to_str().unwrap());

        let statuses = run_uninstall(&["--dry-run".to_string()]).unwrap();

        assert_eq!(statuses.get("cline").map(String::as_str), Some("failed"));
    }

    #[test]
    #[serial]
    fn persist_install_config_updates_api_base_and_backfills_git_path() {
        let temp = tempdir().unwrap();
        let install_dir = temp.path().join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(test_binary_path(&install_dir), "").unwrap();

        let expected_git_path = if cfg!(windows) {
            r"C:\Program Files\Git\bin\git.exe"
        } else {
            "/opt/custom/bin/git"
        };
        write_install_git_marker(&install_dir, expected_git_path);

        let _home = EnvVarGuard::set("HOME", temp.path().to_str().unwrap());
        #[cfg(windows)]
        let _userprofile = EnvVarGuard::set("USERPROFILE", temp.path().to_str().unwrap());
        let _api_base = EnvVarGuard::set("API_BASE", "https://enterprise.example");
        let _api_key = EnvVarGuard::remove("API_KEY");

        let changed = persist_install_config(&test_binary_path(&install_dir), false).unwrap();

        assert!(changed);

        let config = crate::config::load_file_config_public().unwrap();
        assert_eq!(
            config.api_base_url.as_deref(),
            Some("https://enterprise.example")
        );
        assert_eq!(config.git_path.as_deref(), Some(expected_git_path));
        assert_eq!(config.api_key, None);
    }

    #[test]
    #[serial]
    fn persist_install_config_preserves_existing_git_path() {
        let temp = tempdir().unwrap();
        let install_dir = temp.path().join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(test_binary_path(&install_dir), "").unwrap();
        write_install_git_marker(
            &install_dir,
            if cfg!(windows) {
                r"C:\Program Files\Git\bin\git.exe"
            } else {
                "/opt/custom/bin/git"
            },
        );

        let _home = EnvVarGuard::set("HOME", temp.path().to_str().unwrap());
        #[cfg(windows)]
        let _userprofile = EnvVarGuard::set("USERPROFILE", temp.path().to_str().unwrap());
        let _api_base = EnvVarGuard::set("API_BASE", "https://enterprise.example");
        let _api_key = EnvVarGuard::remove("API_KEY");

        let existing_git_path = if cfg!(windows) {
            r"D:\PortableGit\bin\git.exe"
        } else {
            "/usr/local/bin/git"
        };
        crate::config::save_file_config(&crate::config::FileConfig {
            git_path: Some(existing_git_path.to_string()),
            ..Default::default()
        })
        .unwrap();

        persist_install_config(&test_binary_path(&install_dir), false).unwrap();

        let config = crate::config::load_file_config_public().unwrap();
        assert_eq!(
            config.api_base_url.as_deref(),
            Some("https://enterprise.example")
        );
        assert_eq!(config.git_path.as_deref(), Some(existing_git_path));
    }

    #[test]
    #[serial]
    fn persist_install_config_skips_without_env_or_in_dry_run() {
        let temp = tempdir().unwrap();
        let install_dir = temp.path().join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(test_binary_path(&install_dir), "").unwrap();

        let _home = EnvVarGuard::set("HOME", temp.path().to_str().unwrap());
        #[cfg(windows)]
        let _userprofile = EnvVarGuard::set("USERPROFILE", temp.path().to_str().unwrap());
        let _api_base = EnvVarGuard::remove("API_BASE");
        let _api_key = EnvVarGuard::remove("API_KEY");

        let changed = persist_install_config(&test_binary_path(&install_dir), false).unwrap();
        assert!(!changed);
        assert!(!temp.path().join(".git-ai").join("config.json").exists());

        let _api_base = EnvVarGuard::set("API_BASE", "https://enterprise.example");
        let changed = persist_install_config(&test_binary_path(&install_dir), true).unwrap();
        assert!(!changed);
        assert!(!temp.path().join(".git-ai").join("config.json").exists());
    }

    #[test]
    #[serial]
    fn persist_install_config_persists_api_key() {
        let temp = tempdir().unwrap();
        let install_dir = temp.path().join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(test_binary_path(&install_dir), "").unwrap();

        let _home = EnvVarGuard::set("HOME", temp.path().to_str().unwrap());
        #[cfg(windows)]
        let _userprofile = EnvVarGuard::set("USERPROFILE", temp.path().to_str().unwrap());
        let _api_base = EnvVarGuard::remove("API_BASE");
        let _api_key = EnvVarGuard::set("API_KEY", "sk-enterprise-key-12345");

        let changed = persist_install_config(&test_binary_path(&install_dir), false).unwrap();

        assert!(changed);

        let config = crate::config::load_file_config_public().unwrap();
        assert_eq!(config.api_key.as_deref(), Some("sk-enterprise-key-12345"));
        assert_eq!(config.api_base_url, None);
    }

    #[test]
    #[serial]
    fn persist_install_config_persists_both_api_base_and_api_key() {
        let temp = tempdir().unwrap();
        let install_dir = temp.path().join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(test_binary_path(&install_dir), "").unwrap();

        let _home = EnvVarGuard::set("HOME", temp.path().to_str().unwrap());
        #[cfg(windows)]
        let _userprofile = EnvVarGuard::set("USERPROFILE", temp.path().to_str().unwrap());
        let _api_base = EnvVarGuard::set("API_BASE", "https://enterprise.example");
        let _api_key = EnvVarGuard::set("API_KEY", "sk-enterprise-key-12345");

        let changed = persist_install_config(&test_binary_path(&install_dir), false).unwrap();

        assert!(changed);

        let config = crate::config::load_file_config_public().unwrap();
        assert_eq!(
            config.api_base_url.as_deref(),
            Some("https://enterprise.example")
        );
        assert_eq!(config.api_key.as_deref(), Some("sk-enterprise-key-12345"));
    }

    #[cfg(windows)]
    #[test]
    fn parse_git_og_cmd_path_extracts_wrapped_git_path() {
        assert_eq!(
            parse_git_og_cmd_path("@echo off\r\n\"C:\\Program Files\\Git\\bin\\git.exe\" %*\r\n"),
            Some("C:\\Program Files\\Git\\bin\\git.exe".to_string())
        );
    }

    #[test]
    fn parse_git_version_standard() {
        assert_eq!(parse_git_version("git version 2.39.1"), Some((2, 39, 1)));
    }

    #[test]
    fn parse_git_version_apple_suffix() {
        assert_eq!(
            parse_git_version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39, 3))
        );
    }

    #[test]
    fn parse_git_version_no_patch() {
        assert_eq!(parse_git_version("git version 2.22"), Some((2, 22, 0)));
    }

    #[test]
    fn parse_git_version_old() {
        assert_eq!(parse_git_version("git version 2.17.1"), Some((2, 17, 1)));
        assert!(parse_git_version("git version 2.17.1").unwrap() < MIN_GIT_VERSION);
    }

    #[test]
    fn parse_git_version_at_minimum() {
        assert_eq!(parse_git_version("git version 2.22.0"), Some((2, 22, 0)));
        assert!(parse_git_version("git version 2.22.0").unwrap() >= MIN_GIT_VERSION);
    }

    #[test]
    fn parse_git_version_invalid() {
        assert_eq!(parse_git_version("not a git version"), None);
        assert_eq!(parse_git_version(""), None);
    }
}
