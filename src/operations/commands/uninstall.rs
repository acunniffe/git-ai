//! `git-ai uninstall` — remove git-ai integrations from this machine.
//!
//! Steps (each best-effort; a failure is reported and the remaining steps
//! still run):
//! 1. Remove agent/IDE hooks (reuses the `uninstall-hooks` machinery).
//! 2. Revert the global git trace2 config if it points at the git-ai daemon.
//! 3. Shut the daemon down and remove its sockets/locks.
//! 4. Remove installed binaries and shims (`~/.git-ai/bin`,
//!    `~/.local/bin/git-ai`).
//! 5. Remove installer-added PATH blocks from shell rc files (fence blocks
//!    written by install.sh ≥ 1.7; legacy bare lines are stripped with a
//!    warning when no fence is found).
//! 6. With `--purge`, remove the whole `~/.git-ai` data directory (config,
//!    local databases, logs).
//!
//! When `~/.git-ai/install-manifest.json` exists (written by install.sh /
//! install-hooks ≥ 1.7) the manifest drives steps 4–5; otherwise known
//! locations are used as a fallback for legacy installs.

use crate::config::Config;
use crate::error::GitAiError;
use crate::operations::commands::install_hooks::{
    TRACE2_EVENT_NESTING_KEY, TRACE2_EVENT_TARGET_KEY, remove_global_git_config_section,
};
use crate::operations::commands::install_manifest::{InstallManifest, remove_fence_block};
use crate::operations::daemon::daemon_config::WINDOWS_PIPE_PREFIX;
use crate::operations::mdm::paths::home_dir;
use std::io::IsTerminal;
use std::path::Path;

pub fn run_uninstall_all(args: &[String]) -> Result<(), GitAiError> {
    let mut purge = false;
    let mut assume_yes = false;
    for arg in args {
        match arg.as_str() {
            "--purge" => purge = true,
            "--yes" | "-y" => assume_yes = true,
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => {
                return Err(GitAiError::Generic(format!(
                    "unknown option '{}'; run `git-ai uninstall --help` for usage",
                    other
                )));
            }
        }
    }

    if !assume_yes && !confirm(purge) {
        println!("Aborted.");
        return Ok(());
    }

    let mut report: Vec<String> = Vec::new();

    // 1. Agent/IDE hooks.
    match crate::operations::commands::install_hooks::run_uninstall(&[]) {
        Ok(_) => report.push("agent hooks: removed".to_string()),
        Err(e) => report.push(format!("agent hooks: FAILED ({})", e)),
    }

    // 2. Global git trace2 config — only when it points at our daemon.
    report.push(revert_trace2_config());

    // 3. Daemon shutdown + socket/lock cleanup. Skipped inside test harnesses
    //    (mirrors ensure_daemon), where per-test daemons manage their own state.
    //    Shutdown failure (e.g. daemon not running) is non-fatal: we warn and
    //    continue so the remaining cleanup steps still execute.
    if std::env::var_os("GIT_AI_TEST_DB_PATH").is_none()
        && std::env::var_os("GITAI_TEST_DB_PATH").is_none()
    {
        report.push(shutdown_daemon_best_effort());
    }
    {
        let daemon_dir = home_dir().join(".git-ai").join("internal").join("daemon");
        if daemon_dir.exists() {
            match std::fs::remove_dir_all(&daemon_dir) {
                Ok(()) => report.push("daemon sockets/locks: removed".to_string()),
                Err(e) => report.push(format!("daemon sockets/locks: FAILED ({})", e)),
            }
        }
    }

    // Load manifest (empty when not present — falls back to known locations).
    let manifest = InstallManifest::load();

    // 4. Binaries and shims.
    report.extend(remove_binaries(&manifest));

    // 5. Shell rc PATH blocks.
    report.extend(clean_shell_rc_files(&manifest));

    // 6. Data.
    if purge {
        let data_dir = home_dir().join(".git-ai");
        if data_dir.exists() {
            match std::fs::remove_dir_all(&data_dir) {
                Ok(()) => report.push("~/.git-ai (config + data): removed".to_string()),
                Err(e) => report.push(format!("~/.git-ai: FAILED ({})", e)),
            }
        }
    } else {
        report.push(
            "~/.git-ai (config + local databases): kept — remove with `git-ai uninstall --purge`"
                .to_string(),
        );
    }

    println!("\ngit-ai uninstall summary:");
    for line in &report {
        println!("  - {}", line);
    }
    println!(
        "\nNote: repo-local .git/ai directories are not tracked and were left in place.\n\
         If you installed via Nix or Home Manager, remove git-ai from your Nix configuration as well."
    );
    Ok(())
}

fn confirm(purge: bool) -> bool {
    if !std::io::stdin().is_terminal() {
        eprintln!("Refusing to uninstall without a terminal; pass --yes to proceed.");
        return false;
    }
    if purge {
        println!(
            "This removes all git-ai integrations AND deletes ~/.git-ai (config, local attribution databases)."
        );
    } else {
        println!("This removes all git-ai integrations (hooks, git config, daemon, binaries).");
    }
    print!("Continue? [y/N] ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Remove our trace2 configuration from the global git config, but only when
/// the event target actually points at the git-ai daemon (never clobber a
/// user's own trace2 setup).
///
/// Recognized patterns written by git-ai:
/// - Unix socket:   `af_unix:stream:/…/.git-ai/…`  (contains `.git-ai`)
/// - Windows pipe:  `\\.\pipe\git-ai-<hash16>-trace2`  (starts with [`WINDOWS_PIPE_PREFIX`])
fn revert_trace2_config() -> String {
    let config = Config::fresh();
    let git_cmd = config.git_cmd().to_string();

    let current_target = std::process::Command::new(&git_cmd)
        .args(["config", "--global", "--get", TRACE2_EVENT_TARGET_KEY])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    if current_target.is_empty() {
        return "git trace2 config: not set".to_string();
    }
    if !target_is_git_ai_owned(&current_target) {
        return format!(
            "git trace2 config: left untouched ({} does not point at git-ai)",
            TRACE2_EVENT_TARGET_KEY
        );
    }
    match remove_global_git_config_section(&git_cmd, "trace2") {
        Ok(()) => format!(
            "git trace2 config: removed ({}, {})",
            TRACE2_EVENT_TARGET_KEY, TRACE2_EVENT_NESTING_KEY
        ),
        Err(e) => format!("git trace2 config: FAILED ({})", e),
    }
}

/// Return `true` when `target` was written by git-ai and is safe to remove.
///
/// git-ai writes two distinct formats depending on OS:
/// - Unix: `af_unix:stream:<path>` where `<path>` contains `.git-ai`
/// - Windows: `\\.\pipe\git-ai-<16-hex>-trace2`
///
/// The rare Unix long-path fallback socket (`$TMPDIR/git-ai-d-<hash>/trace.sock`,
/// used when the home path exceeds the unix-socket length limit) is intentionally
/// not matched: it contains neither marker and is left for a future extension.
fn target_is_git_ai_owned(target: &str) -> bool {
    target.contains(".git-ai") || target.starts_with(WINDOWS_PIPE_PREFIX)
}

/// Attempt to shut down the daemon, returning a report string.
///
/// The daemon may not be running at uninstall time, which is normal.  We treat
/// shutdown failure as a non-fatal warning so the rest of the uninstall steps
/// still execute.
fn shutdown_daemon_best_effort() -> String {
    use crate::model::daemon_control::ControlRequest;
    use crate::operations::daemon::{DaemonConfig, send_control_request};

    let config = match DaemonConfig::from_env_or_default_paths() {
        Ok(c) => c,
        Err(e) => return format!("daemon: warning — could not resolve config ({})", e),
    };
    match send_control_request(&config.control_socket_path, &ControlRequest::Shutdown) {
        Ok(_) => "daemon: shutdown requested".to_string(),
        Err(e) => format!("daemon: warning — shutdown skipped ({})", e),
    }
}

fn remove_binaries(manifest: &InstallManifest) -> Vec<String> {
    let mut report = Vec::new();
    let home = home_dir();

    // ~/.local/bin/git-ai symlink (only when it points into ~/.git-ai).
    // Check manifest symlinks first, then fall back to the well-known path.
    let local_bin_link = home.join(".local").join("bin").join("git-ai");
    let check_symlinks: Vec<std::path::PathBuf> = if manifest.symlinks.is_empty() {
        vec![local_bin_link]
    } else {
        manifest
            .symlinks
            .iter()
            .map(std::path::PathBuf::from)
            // Safety: reject any manifest entry that escapes $HOME (hardening
            // against a crafted manifest directing removal at arbitrary paths).
            .filter(|p| {
                p.starts_with(&home)
                    && p.symlink_metadata()
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false)
            })
            .collect()
    };
    for link in &check_symlinks {
        if let Ok(target) = std::fs::read_link(link)
            && target.to_string_lossy().contains(".git-ai")
        {
            match std::fs::remove_file(link) {
                Ok(()) => report.push(format!("{}: symlink removed", link.display())),
                Err(e) => report.push(format!("{}: FAILED ({})", link.display(), e)),
            }
        }
    }

    // ~/.git-ai/bin — the installed binary and git shim. On Unix the currently
    // running binary can be unlinked; on Windows deletion of a running exe
    // fails, so leave instructions instead.
    let bin_dir = home.join(".git-ai").join("bin");
    if bin_dir.exists() {
        #[cfg(windows)]
        {
            report.push(format!(
                "{}: remove manually after this process exits (Windows cannot delete a running executable)",
                bin_dir.display()
            ));
        }
        #[cfg(not(windows))]
        match std::fs::remove_dir_all(&bin_dir) {
            Ok(()) => report.push("~/.git-ai/bin (binary + git shim): removed".to_string()),
            Err(e) => report.push(format!("~/.git-ai/bin: FAILED ({})", e)),
        }
    }
    report
}

/// Strip installer-added PATH blocks from shell rc files.
///
/// For installs from 1.7+, the rc edit is wrapped in a fence block
/// (`# >>> git-ai >>>` / `# <<< git-ai <<<`).  Older installs used bare
/// `# Added by git-ai installer …` comment + PATH line; those are stripped
/// with best-effort and a diff is printed when ambiguous content remains.
fn clean_shell_rc_files(manifest: &InstallManifest) -> Vec<String> {
    let mut report = Vec::new();
    let home = home_dir();

    // Determine which rc files to clean: manifest-recorded (preferred) or
    // well-known fallback list for legacy installs.
    let rc_paths: Vec<std::path::PathBuf> = if !manifest.rc_files.is_empty() {
        manifest
            .rc_files
            .iter()
            .map(|e| std::path::PathBuf::from(&e.path))
            // Safety: reject manifest entries outside $HOME or that are not
            // regular files (hardening against a crafted install-manifest.json).
            .filter(|p| {
                p.starts_with(&home)
                    && p.metadata()
                        .map(|m| m.file_type().is_file())
                        .unwrap_or(false)
            })
            .collect()
    } else {
        vec![
            home.join(".bashrc"),
            home.join(".bash_profile"),
            home.join(".zshrc"),
            home.join(".config").join("fish").join("config.fish"),
        ]
    };

    for rc in rc_paths {
        match clean_rc_file(&rc) {
            Ok(Some(had_legacy_remaining)) => {
                report.push(format!("{}: git-ai PATH block removed", display_home(&rc)));
                if had_legacy_remaining {
                    report.push(format!(
                        "  (note: {} may still contain legacy git-ai lines — please review)",
                        display_home(&rc)
                    ));
                }
            }
            Ok(None) => {}
            Err(e) => report.push(format!("{}: FAILED ({})", display_home(&rc), e)),
        }
    }
    report
}

/// Remove the git-ai PATH block from `path`.
///
/// Returns `Ok(Some(had_legacy))` when something was removed, `Ok(None)` when
/// the file was unchanged, or `Err` on I/O failure.  `had_legacy` is `true`
/// when no fence block was found but bare legacy lines were removed (the caller
/// should warn the user to review the file).
fn clean_rc_file(path: &Path) -> Result<Option<bool>, std::io::Error> {
    if !path.is_file() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)?;
    let result = remove_fence_block(&content);
    if result.text == content {
        return Ok(None);
    }
    std::fs::write(path, &result.text)?;
    // Signal whether the removal was fenced (clean) or legacy (may need review).
    Ok(Some(!result.removed_fence))
}

fn display_home(path: &Path) -> String {
    let home = home_dir();
    match path.strip_prefix(&home) {
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => path.display().to_string(),
    }
}

fn print_help() {
    println!("git-ai uninstall - Remove git-ai integrations from this machine");
    println!();
    println!("Usage:");
    println!("  git-ai uninstall [--yes] [--purge]");
    println!();
    println!("Options:");
    println!("  --yes, -y   Skip the confirmation prompt");
    println!("  --purge     Also delete ~/.git-ai (config and local attribution databases)");
    println!();
    println!("Removes agent hooks, the global git trace2 config, the daemon, installed");
    println!("binaries/shims, and installer-added PATH lines. Without --purge, your");
    println!("configuration and local attribution data are kept.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::commands::install_manifest::{
        FENCE_CLOSE, FENCE_OPEN, make_fence_block,
    };

    #[test]
    fn clean_rc_file_removes_fenced_block() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".zshrc");
        let block = make_fence_block("export PATH=\"$HOME/.git-ai/bin:$PATH\"");
        std::fs::write(&rc, format!("export FOO=1\n{}", block)).unwrap();

        let result = clean_rc_file(&rc).unwrap();
        assert!(
            result.is_some(),
            "expected Some when a fence block was removed"
        );
        assert!(
            !result.unwrap(),
            "removed_fence=true so had_legacy should be false"
        );
        let cleaned = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(cleaned, "export FOO=1\n");
        assert!(!cleaned.contains(FENCE_OPEN));
        assert!(!cleaned.contains(FENCE_CLOSE));
    }

    #[test]
    fn clean_rc_file_strips_legacy_installer_lines() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".zshrc");
        std::fs::write(
            &rc,
            "export FOO=1\n# Added by git-ai installer on Sun Jul 20\nexport PATH=\"/Users/x/.git-ai/bin:$PATH\"\nalias ll='ls -l'\n",
        )
        .unwrap();

        let result = clean_rc_file(&rc).unwrap();
        assert!(
            result.is_some(),
            "expected Some when legacy lines were removed"
        );
        let cleaned = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(cleaned, "export FOO=1\nalias ll='ls -l'\n");

        // Idempotent: a second pass changes nothing.
        assert!(clean_rc_file(&rc).unwrap().is_none());
    }

    #[test]
    fn clean_rc_file_leaves_unrelated_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        let content = "export FOO=1\nexport PATH=\"$HOME/bin:$PATH\"\n";
        std::fs::write(&rc, content).unwrap();

        assert!(clean_rc_file(&rc).unwrap().is_none());
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), content);
    }

    #[test]
    fn clean_rc_file_handles_fish_add_path() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join("config.fish");
        std::fs::write(
            &rc,
            "# Added by git-ai installer on Sun Jul 20\nfish_add_path -g \"/Users/x/.git-ai/bin\"\nset -x EDITOR vim\n",
        )
        .unwrap();

        clean_rc_file(&rc).unwrap();
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), "set -x EDITOR vim\n");
    }

    #[test]
    fn clean_rc_file_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        assert!(clean_rc_file(&dir.path().join("nope")).unwrap().is_none());
    }

    #[test]
    fn clean_rc_file_fence_round_trip_with_content_after() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        let before = "# existing config\n";
        let after = "alias gs='git status'\n";
        let block = make_fence_block("export PATH=\"$HOME/.git-ai/bin:$PATH\"");
        std::fs::write(&rc, format!("{}{}{}", before, block, after)).unwrap();

        clean_rc_file(&rc).unwrap();
        let result = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(result, format!("{}{}", before, after));
    }

    // ── target_is_git_ai_owned ───────────────────────────────────────────────

    #[test]
    fn target_is_git_ai_owned_unix_socket() {
        // af_unix socket path written by install-hooks on Unix.
        assert!(target_is_git_ai_owned(
            "af_unix:stream:/home/user/.git-ai/internal/daemon/trace2.sock"
        ));
    }

    #[test]
    fn target_is_git_ai_owned_windows_pipe() {
        // Named-pipe path written by the daemon on Windows.
        assert!(target_is_git_ai_owned(
            r"\\.\pipe\git-ai-abcdef1234567890-trace2"
        ));
    }

    #[test]
    fn target_is_git_ai_owned_foreign_target_is_rejected() {
        // A user's own tracer must not be removed.
        assert!(!target_is_git_ai_owned("/tmp/my-own-trace.sock"));
        assert!(!target_is_git_ai_owned(r"\\.\pipe\some-other-tool"));
        assert!(!target_is_git_ai_owned("af_unix:stream:/tmp/notus"));
    }

    #[test]
    fn target_is_git_ai_owned_empty_string_is_rejected() {
        assert!(!target_is_git_ai_owned(""));
    }
}
