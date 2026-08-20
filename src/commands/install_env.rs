//! Shell/environment configuration for `install-hooks --env`.
//!
//! Ports the shell-profile PATH setup that previously lived in `install.sh`
//! and `install.ps1`. Everything here is best-effort: failures are printed as
//! warnings and never fail the install command, matching the scripts'
//! warn-and-continue semantics.

use std::io::Write;
use std::path::Path;

/// Apply the shell/environment configuration for the current platform.
pub fn configure_shell_env() {
    #[cfg(unix)]
    unix::configure_shell_env();

    #[cfg(windows)]
    windows::configure_shell_env();
}

/// Byte-level fixed-string search, matching `grep -qsF` (no UTF-8
/// requirement; missing/unreadable file counts as "not present").
fn file_contains(path: &Path, needle: &[u8]) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => bytes.windows(needle.len()).any(|window| window == needle),
        Err(_) => false,
    }
}

/// Append a `# Added by git-ai installer` comment plus the given config line,
/// creating the file if needed (UTF-8 without BOM, LF line endings).
fn append_installer_line(path: &Path, timestamp: &str, line: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(format!("\n# Added by git-ai installer on {timestamp}\n{line}\n").as_bytes())
}

#[cfg(unix)]
mod unix {
    use super::{append_installer_line, file_contains};
    use crate::mdm::utils::{binary_exists, home_dir};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    const GREEN: &str = "\x1b[0;32m";
    const YELLOW: &str = "\x1b[0;33m";
    const NC: &str = "\x1b[0m";

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct ShellConfig {
        shell: &'static str,
        config_file: PathBuf,
    }

    #[derive(Debug, Default)]
    pub(super) struct EnvSetupReport {
        configured: Vec<ShellConfig>,
        already_configured: Vec<ShellConfig>,
        created_paths: Vec<PathBuf>,
    }

    pub(super) fn configure_shell_env() {
        let home = home_dir();
        let login_shell = resolve_login_shell(std::env::var("SHELL").ok(), binary_exists("zsh"));
        let timestamp = chrono::Local::now()
            .format("%a %b %e %H:%M:%S %Y")
            .to_string();
        let report = apply_env_config(&home, login_shell.as_deref(), &timestamp);
        print!("{}", render_report(&report, &install_dir_string(&home)));
        chown_created_paths(&report.created_paths);
    }

    /// Resolve the login shell like install.sh's preamble: use `$SHELL` when
    /// set, otherwise prefer zsh when it is installed (SHELL is often unbound
    /// under MDM runs, and zsh is the macOS default). `None` makes
    /// `detect_all_shells` fall back to bash, matching the script.
    pub(super) fn resolve_login_shell(
        env_shell: Option<String>,
        zsh_available: bool,
    ) -> Option<String> {
        env_shell
            .filter(|s| !s.is_empty())
            .or_else(|| zsh_available.then(|| "zsh".to_string()))
    }

    fn install_dir_string(home: &Path) -> String {
        format!("{}/.git-ai/bin", home.display())
    }

    /// Detect all shells with existing config files, mirroring
    /// `detect_all_shells` from install.sh: bash (~/.bashrc preferred over
    /// ~/.bash_profile), zsh, fish; if none exist, fall back to the login
    /// shell's config (defaulting to bash) so it gets created.
    pub(super) fn detect_all_shells(home: &Path, login_shell: Option<&str>) -> Vec<ShellConfig> {
        let mut shells = Vec::new();

        let bashrc = home.join(".bashrc");
        let bash_profile = home.join(".bash_profile");
        if bashrc.is_file() {
            shells.push(ShellConfig {
                shell: "bash",
                config_file: bashrc.clone(),
            });
        } else if bash_profile.is_file() {
            shells.push(ShellConfig {
                shell: "bash",
                config_file: bash_profile,
            });
        }

        let zshrc = home.join(".zshrc");
        if zshrc.is_file() {
            shells.push(ShellConfig {
                shell: "zsh",
                config_file: zshrc.clone(),
            });
        }

        let fish_config = home.join(".config/fish/config.fish");
        if fish_config.is_file() {
            shells.push(ShellConfig {
                shell: "fish",
                config_file: fish_config.clone(),
            });
        }

        if shells.is_empty() {
            let basename = login_shell
                .map(Path::new)
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let fallback = match basename {
                "fish" => ShellConfig {
                    shell: "fish",
                    config_file: fish_config,
                },
                "zsh" => ShellConfig {
                    shell: "zsh",
                    config_file: zshrc,
                },
                _ => ShellConfig {
                    shell: "bash",
                    config_file: bashrc,
                },
            };
            shells.push(fallback);
        }

        shells
    }

    /// Add the install dir to PATH in every detected shell config, mirroring
    /// the PATH-append loop from install.sh (idempotent via a literal
    /// substring check, like `grep -qsF "$INSTALL_DIR"`).
    pub(super) fn apply_env_config(
        home: &Path,
        login_shell: Option<&str>,
        timestamp: &str,
    ) -> EnvSetupReport {
        let install_dir = install_dir_string(home);
        let mut report = EnvSetupReport::default();

        for shell_config in detect_all_shells(home, login_shell) {
            if let Err(e) = configure_shell(&shell_config, &install_dir, timestamp, &mut report) {
                eprintln!(
                    "{YELLOW}Warning: failed to update {}: {e}{NC}",
                    shell_config.config_file.display()
                );
            }
        }

        report
    }

    fn configure_shell(
        shell_config: &ShellConfig,
        install_dir: &str,
        timestamp: &str,
        report: &mut EnvSetupReport,
    ) -> std::io::Result<()> {
        let config_file = &shell_config.config_file;

        // Check for the install dir before touching the file so an
        // already-configured (possibly read-only) config never needs write
        // access, like install.sh's `grep -qsF` check.
        if file_contains(config_file, install_dir.as_bytes()) {
            report.already_configured.push(shell_config.clone());
            return Ok(());
        }

        let path_cmd = if shell_config.shell == "fish" {
            // Create the fish config directory if it doesn't exist (fallback case).
            if let Some(config_dir) = config_file.parent()
                && !config_dir.is_dir()
            {
                fs::create_dir_all(config_dir)?;
                report.created_paths.push(config_dir.to_path_buf());
            }
            format!("fish_add_path -g \"{install_dir}\"")
        } else {
            format!("export PATH=\"{install_dir}:$PATH\"")
        };

        let created = !config_file.is_file();
        append_installer_line(config_file, timestamp, &path_cmd)?;
        if created {
            report.created_paths.push(config_file.clone());
        }
        report.configured.push(shell_config.clone());

        Ok(())
    }

    pub(super) fn render_report(report: &EnvSetupReport, install_dir: &str) -> String {
        let mut out = String::new();

        if !report.configured.is_empty() {
            out.push_str("\nUpdated shell configurations:\n");
            for entry in &report.configured {
                out.push_str(&format!("{GREEN}  ✓ {}{NC}\n", entry.config_file.display()));
            }
            out.push_str("\nTo apply changes immediately:\n");
            for entry in &report.configured {
                out.push_str(&format!(
                    "  - For {}: source {}\n",
                    entry.shell,
                    entry.config_file.display()
                ));
            }
        }

        if !report.already_configured.is_empty() {
            out.push_str("\nAlready configured (no changes needed):\n");
            for entry in &report.already_configured {
                out.push_str(&format!("  ✓ {}\n", entry.config_file.display()));
            }
        }

        if report.configured.is_empty() && report.already_configured.is_empty() {
            // detect_all_shells always yields at least one config, so an empty
            // report means every update failed (warnings already on stderr).
            out.push_str("\nNo shell config files could be updated.\n");
            out.push_str("Please add the following line to your shell config and restart:\n");
            out.push_str(&format!("  export PATH=\"{install_dir}:$PATH\"\n"));
        }

        out
    }

    /// In root/MDM installs (e.g. JAMF), hand ownership of any files this
    /// process created back to the target user, mirroring the
    /// `chown "$INSTALL_USER" "$created_path"` loop from install.sh. The
    /// handoff only runs when the caller (e.g. the install script, once it
    /// delegates to `--env`) passes the target user via GIT_AI_INSTALL_USER.
    fn chown_created_paths(created_paths: &[PathBuf]) {
        if created_paths.is_empty() || !crate::utils::is_running_as_superuser() {
            return;
        }
        let Some(install_user) = std::env::var("GIT_AI_INSTALL_USER")
            .ok()
            .filter(|u| !u.is_empty())
        else {
            return;
        };
        for path in created_paths {
            let _ = Command::new("chown")
                .arg(&install_user)
                .arg(path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tempfile::tempdir;

        const TS: &str = "Tue Aug 19 10:00:00 2025";

        fn shell(report_entry: &ShellConfig) -> (&'static str, &Path) {
            (report_entry.shell, &report_entry.config_file)
        }

        #[test]
        fn detects_only_bashrc_when_only_bash_config_exists() {
            let home = tempdir().unwrap();
            fs::write(home.path().join(".bashrc"), "# bashrc\n").unwrap();

            let shells = detect_all_shells(home.path(), Some("/bin/zsh"));

            assert_eq!(shells.len(), 1);
            assert_eq!(
                shell(&shells[0]),
                ("bash", home.path().join(".bashrc").as_path())
            );
        }

        #[test]
        fn detects_only_zshrc_when_only_zsh_config_exists() {
            let home = tempdir().unwrap();
            fs::write(home.path().join(".zshrc"), "# zshrc\n").unwrap();

            let shells = detect_all_shells(home.path(), Some("/bin/bash"));

            assert_eq!(shells.len(), 1);
            assert_eq!(
                shell(&shells[0]),
                ("zsh", home.path().join(".zshrc").as_path())
            );
        }

        #[test]
        fn detects_only_fish_config_when_only_fish_config_exists() {
            let home = tempdir().unwrap();
            let fish_dir = home.path().join(".config/fish");
            fs::create_dir_all(&fish_dir).unwrap();
            fs::write(fish_dir.join("config.fish"), "# fish\n").unwrap();

            let shells = detect_all_shells(home.path(), Some("/bin/bash"));

            assert_eq!(shells.len(), 1);
            assert_eq!(
                shell(&shells[0]),
                ("fish", fish_dir.join("config.fish").as_path())
            );
        }

        #[test]
        fn detects_all_three_shells_in_order_when_all_configs_exist() {
            let home = tempdir().unwrap();
            fs::write(home.path().join(".bashrc"), "# bashrc\n").unwrap();
            fs::write(home.path().join(".zshrc"), "# zshrc\n").unwrap();
            let fish_dir = home.path().join(".config/fish");
            fs::create_dir_all(&fish_dir).unwrap();
            fs::write(fish_dir.join("config.fish"), "# fish\n").unwrap();

            let shells = detect_all_shells(home.path(), Some("/bin/bash"));

            assert_eq!(
                shells.iter().map(|s| s.shell).collect::<Vec<_>>(),
                vec!["bash", "zsh", "fish"]
            );
        }

        #[test]
        fn detects_bash_and_zsh_when_both_exist_without_fish() {
            let home = tempdir().unwrap();
            fs::write(home.path().join(".bashrc"), "# bashrc\n").unwrap();
            fs::write(home.path().join(".zshrc"), "# zshrc\n").unwrap();

            let shells = detect_all_shells(home.path(), Some("/bin/bash"));

            assert_eq!(
                shells.iter().map(|s| s.shell).collect::<Vec<_>>(),
                vec!["bash", "zsh"]
            );
        }

        #[test]
        fn prefers_bashrc_over_bash_profile() {
            let home = tempdir().unwrap();
            fs::write(home.path().join(".bashrc"), "# bashrc\n").unwrap();
            fs::write(home.path().join(".bash_profile"), "# bash_profile\n").unwrap();

            let shells = detect_all_shells(home.path(), Some("/bin/bash"));

            assert_eq!(shells.len(), 1);
            assert_eq!(
                shell(&shells[0]),
                ("bash", home.path().join(".bashrc").as_path())
            );
        }

        #[test]
        fn uses_bash_profile_when_bashrc_does_not_exist() {
            let home = tempdir().unwrap();
            fs::write(home.path().join(".bash_profile"), "# bash_profile\n").unwrap();

            let shells = detect_all_shells(home.path(), Some("/bin/bash"));

            assert_eq!(shells.len(), 1);
            assert_eq!(
                shell(&shells[0]),
                ("bash", home.path().join(".bash_profile").as_path())
            );
        }

        #[test]
        fn falls_back_to_login_shell_zsh_when_no_configs_exist() {
            let home = tempdir().unwrap();

            let shells = detect_all_shells(home.path(), Some("/usr/bin/zsh"));

            assert_eq!(shells.len(), 1);
            assert_eq!(
                shell(&shells[0]),
                ("zsh", home.path().join(".zshrc").as_path())
            );
        }

        #[test]
        fn falls_back_to_login_shell_bash_when_no_configs_exist() {
            let home = tempdir().unwrap();

            let shells = detect_all_shells(home.path(), Some("/bin/bash"));

            assert_eq!(shells.len(), 1);
            assert_eq!(
                shell(&shells[0]),
                ("bash", home.path().join(".bashrc").as_path())
            );
        }

        #[test]
        fn falls_back_to_login_shell_fish_when_no_configs_exist() {
            let home = tempdir().unwrap();

            let shells = detect_all_shells(home.path(), Some("/usr/bin/fish"));

            assert_eq!(shells.len(), 1);
            assert_eq!(
                shell(&shells[0]),
                (
                    "fish",
                    home.path().join(".config/fish/config.fish").as_path()
                )
            );
        }

        #[test]
        fn falls_back_to_bash_for_unknown_login_shell() {
            let home = tempdir().unwrap();

            let shells = detect_all_shells(home.path(), Some("/bin/tcsh"));

            assert_eq!(shells.len(), 1);
            assert_eq!(
                shell(&shells[0]),
                ("bash", home.path().join(".bashrc").as_path())
            );
        }

        #[test]
        fn falls_back_to_bash_when_login_shell_is_unset() {
            let home = tempdir().unwrap();

            let shells = detect_all_shells(home.path(), None);

            assert_eq!(shells.len(), 1);
            assert_eq!(
                shell(&shells[0]),
                ("bash", home.path().join(".bashrc").as_path())
            );
        }

        #[test]
        fn appends_bash_and_zsh_export_lines() {
            let home = tempdir().unwrap();
            fs::write(home.path().join(".bashrc"), "# bashrc\n").unwrap();
            fs::write(home.path().join(".zshrc"), "# zshrc\n").unwrap();

            let report = apply_env_config(home.path(), None, TS);

            let install_dir = install_dir_string(home.path());
            let expected_suffix = format!(
                "\n# Added by git-ai installer on {TS}\nexport PATH=\"{install_dir}:$PATH\"\n"
            );
            let bashrc = fs::read_to_string(home.path().join(".bashrc")).unwrap();
            let zshrc = fs::read_to_string(home.path().join(".zshrc")).unwrap();
            assert_eq!(bashrc, format!("# bashrc\n{expected_suffix}"));
            assert_eq!(zshrc, format!("# zshrc\n{expected_suffix}"));
            assert_eq!(report.configured.len(), 2);
            assert!(report.already_configured.is_empty());
            assert!(report.created_paths.is_empty());
        }

        #[test]
        fn appends_fish_add_path_line_for_fish() {
            let home = tempdir().unwrap();
            let fish_dir = home.path().join(".config/fish");
            fs::create_dir_all(&fish_dir).unwrap();
            fs::write(fish_dir.join("config.fish"), "# fish\n").unwrap();

            let report = apply_env_config(home.path(), None, TS);

            let install_dir = install_dir_string(home.path());
            let contents = fs::read_to_string(fish_dir.join("config.fish")).unwrap();
            assert_eq!(
                contents,
                format!(
                    "# fish\n\n# Added by git-ai installer on {TS}\nfish_add_path -g \"{install_dir}\"\n"
                )
            );
            assert_eq!(report.configured.len(), 1);
        }

        #[test]
        fn second_run_is_idempotent() {
            let home = tempdir().unwrap();
            fs::write(home.path().join(".zshrc"), "# zshrc\n").unwrap();

            let first = apply_env_config(home.path(), None, TS);
            let after_first = fs::read(home.path().join(".zshrc")).unwrap();
            let second = apply_env_config(home.path(), None, TS);
            let after_second = fs::read(home.path().join(".zshrc")).unwrap();

            assert_eq!(first.configured.len(), 1);
            assert!(first.already_configured.is_empty());
            assert!(second.configured.is_empty());
            assert_eq!(second.already_configured.len(), 1);
            assert_eq!(after_first, after_second);
        }

        #[test]
        fn skips_file_already_containing_install_dir_anywhere() {
            let home = tempdir().unwrap();
            let install_dir = install_dir_string(home.path());
            fs::write(
                home.path().join(".bashrc"),
                format!("PATH={install_dir}:$PATH # custom setup\n"),
            )
            .unwrap();

            let report = apply_env_config(home.path(), None, TS);

            assert!(report.configured.is_empty());
            assert_eq!(report.already_configured.len(), 1);
        }

        #[test]
        fn fallback_creates_config_file_and_records_created_paths() {
            let home = tempdir().unwrap();

            let report = apply_env_config(home.path(), Some("/usr/bin/zsh"), TS);

            let zshrc = home.path().join(".zshrc");
            assert!(zshrc.is_file());
            let install_dir = install_dir_string(home.path());
            assert_eq!(
                fs::read_to_string(&zshrc).unwrap(),
                format!(
                    "\n# Added by git-ai installer on {TS}\nexport PATH=\"{install_dir}:$PATH\"\n"
                )
            );
            assert_eq!(report.created_paths, vec![zshrc]);
        }

        #[test]
        fn fallback_creates_fish_config_dir_and_file() {
            let home = tempdir().unwrap();

            let report = apply_env_config(home.path(), Some("/usr/bin/fish"), TS);

            let fish_dir = home.path().join(".config/fish");
            let fish_config = fish_dir.join("config.fish");
            assert!(fish_config.is_file());
            assert_eq!(report.created_paths, vec![fish_dir, fish_config]);
            assert_eq!(report.configured.len(), 1);
        }

        #[test]
        fn non_utf8_config_contents_do_not_break_the_presence_check() {
            let home = tempdir().unwrap();
            fs::write(home.path().join(".bashrc"), [0xff, 0xfe, b'\n']).unwrap();

            let report = apply_env_config(home.path(), None, TS);

            assert_eq!(report.configured.len(), 1);
            let bytes = fs::read(home.path().join(".bashrc")).unwrap();
            assert!(bytes.starts_with(&[0xff, 0xfe, b'\n']));
        }

        #[test]
        fn render_report_for_configured_shells_matches_installer_output() {
            let home = tempdir().unwrap();
            fs::write(home.path().join(".bashrc"), "").unwrap();
            fs::write(home.path().join(".zshrc"), "").unwrap();

            let report = apply_env_config(home.path(), None, TS);
            let install_dir = install_dir_string(home.path());
            let rendered = render_report(&report, &install_dir);

            let bashrc = home.path().join(".bashrc").display().to_string();
            let zshrc = home.path().join(".zshrc").display().to_string();
            assert_eq!(
                rendered,
                format!(
                    "\nUpdated shell configurations:\n\
                     {GREEN}  ✓ {bashrc}{NC}\n\
                     {GREEN}  ✓ {zshrc}{NC}\n\
                     \nTo apply changes immediately:\n\
                     \x20 - For bash: source {bashrc}\n\
                     \x20 - For zsh: source {zshrc}\n"
                )
            );
        }

        #[test]
        fn render_report_for_already_configured_shells_matches_installer_output() {
            let home = tempdir().unwrap();
            fs::write(home.path().join(".zshrc"), "").unwrap();
            apply_env_config(home.path(), None, TS);

            let report = apply_env_config(home.path(), None, TS);
            let install_dir = install_dir_string(home.path());
            let rendered = render_report(&report, &install_dir);

            let zshrc = home.path().join(".zshrc").display().to_string();
            assert_eq!(
                rendered,
                format!("\nAlready configured (no changes needed):\n  ✓ {zshrc}\n")
            );
        }

        #[test]
        fn render_report_for_empty_report_prints_manual_instructions() {
            let install_dir = "/home/user/.git-ai/bin";
            let rendered = render_report(&EnvSetupReport::default(), install_dir);

            assert_eq!(
                rendered,
                format!(
                    "\nNo shell config files could be updated.\n\
                     Please add the following line to your shell config and restart:\n\
                     \x20 export PATH=\"{install_dir}:$PATH\"\n"
                )
            );
        }

        #[test]
        fn resolve_login_shell_prefers_the_shell_env_var() {
            assert_eq!(
                resolve_login_shell(Some("/bin/bash".to_string()), true),
                Some("/bin/bash".to_string())
            );
        }

        #[test]
        fn resolve_login_shell_probes_zsh_when_shell_is_unset_or_empty() {
            // Mirrors install.sh: an unbound SHELL (e.g. MDM runs) prefers an
            // installed zsh over the bash fallback.
            assert_eq!(resolve_login_shell(None, true), Some("zsh".to_string()));
            assert_eq!(
                resolve_login_shell(Some(String::new()), true),
                Some("zsh".to_string())
            );
        }

        #[test]
        fn resolve_login_shell_falls_back_to_bash_when_zsh_is_missing() {
            // None makes detect_all_shells take the bash fallback arm.
            assert_eq!(resolve_login_shell(None, false), None);
        }

        #[test]
        fn already_configured_read_only_file_is_reported_without_write_access() {
            let home = tempdir().unwrap();
            let install_dir = install_dir_string(home.path());
            let bashrc = home.path().join(".bashrc");
            let contents = format!("PATH={install_dir}:$PATH # managed dotfile\n");
            fs::write(&bashrc, &contents).unwrap();
            let mut perms = fs::metadata(&bashrc).unwrap().permissions();
            perms.set_readonly(true);
            fs::set_permissions(&bashrc, perms).unwrap();

            let report = apply_env_config(home.path(), None, TS);

            assert_eq!(report.already_configured.len(), 1);
            assert!(report.configured.is_empty());
            assert!(report.created_paths.is_empty());
            assert_eq!(fs::read_to_string(&bashrc).unwrap(), contents);
        }

        #[test]
        fn failed_write_records_nothing_in_the_report() {
            if crate::utils::is_running_as_superuser() {
                return; // root bypasses file permissions
            }
            let home = tempdir().unwrap();
            let bashrc = home.path().join(".bashrc");
            fs::write(&bashrc, "# bashrc\n").unwrap();
            let mut perms = fs::metadata(&bashrc).unwrap().permissions();
            perms.set_readonly(true);
            fs::set_permissions(&bashrc, perms).unwrap();

            let report = apply_env_config(home.path(), None, TS);

            assert!(report.configured.is_empty());
            assert!(report.already_configured.is_empty());
            assert!(report.created_paths.is_empty());
            assert_eq!(fs::read_to_string(&bashrc).unwrap(), "# bashrc\n");
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::{append_installer_line, file_contains};
    use crate::mdm::utils::home_dir;
    use std::path::{Path, PathBuf};

    const GREEN: &str = "\x1b[0;32m";
    const YELLOW: &str = "\x1b[0;33m";
    const RED: &str = "\x1b[0;31m";
    const NC: &str = "\x1b[0m";

    /// Marker used by install.ps1 to detect an existing Git Bash PATH entry.
    const GIT_BASH_MARKER: &str = ".git-ai/bin";

    pub(super) fn configure_shell_env() {
        let home = home_dir();

        // Persistent user PATH (skippable, like install.ps1's skip gate; the
        // Git Bash config below runs regardless).
        if std::env::var("GIT_AI_SKIP_PATH_UPDATE").as_deref() == Ok("1") {
            eprintln!("{YELLOW}Skipping PATH updates because GIT_AI_SKIP_PATH_UPDATE=1{NC}");
        } else {
            let install_dir = home.join(".git-ai").join("bin");
            match ensure_user_path_contains(&install_dir) {
                UserPathStatus::Updated => {
                    println!("{GREEN}Successfully added git-ai to the user PATH.{NC}")
                }
                UserPathStatus::AlreadyPresent => {
                    println!("{GREEN}git-ai already present in the user PATH.{NC}")
                }
                UserPathStatus::Error => eprintln!("{RED}Failed to update the user PATH.{NC}"),
            }
        }

        // Configure Git Bash shell profiles so git-ai takes precedence over
        // /mingw64/bin/git: Git Bash prepends its own directories to PATH,
        // which shadows the Windows user PATH entry set above.
        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        match configure_git_bash(&home, &timestamp) {
            GitBashStatus::NotInstalled => {}
            GitBashStatus::Configured(file) => {
                println!(
                    "{GREEN}Successfully configured Git Bash ({}){NC}",
                    file.display()
                )
            }
            GitBashStatus::AlreadyConfigured(file) => {
                println!(
                    "{GREEN}Git Bash already configured ({}){NC}",
                    file.display()
                )
            }
            GitBashStatus::Failed(message) => {
                eprintln!("{YELLOW}Warning: Failed to configure Git Bash: {message}{NC}")
            }
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum UserPathStatus {
        Updated,
        AlreadyPresent,
        Error,
    }

    /// Ensure the install dir is on the User PATH (appended if absent),
    /// mirroring install.ps1's Set-PathEnsureContains: user scope only, no
    /// admin required, no positioning logic. All errors are non-fatal.
    fn ensure_user_path_contains(install_dir: &Path) -> UserPathStatus {
        match try_update_user_path(&install_dir.to_string_lossy()) {
            Ok(true) => UserPathStatus::Updated,
            Ok(false) => UserPathStatus::AlreadyPresent,
            Err(_) => UserPathStatus::Error,
        }
    }

    fn try_update_user_path(install_dir: &str) -> std::io::Result<bool> {
        use winreg::RegKey;
        use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ};
        use winreg::types::FromRegValue;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let env_key = hkcu.open_subkey_with_flags("Environment", KEY_READ | KEY_SET_VALUE)?;
        // .NET's GetEnvironmentVariable('Path', 'User') — which install.ps1
        // used — expands %VAR% references on read only for REG_EXPAND_SZ
        // values (REG_SZ is returned verbatim) and writes the merged result
        // back as REG_SZ. Replicate that exactly.
        // Only a missing value counts as an empty PATH; any other read
        // failure must fail closed rather than overwrite an existing PATH
        // we could not read.
        let expanded: Option<String> = match env_key.get_raw_value("Path") {
            Ok(value) => {
                let raw = String::from_reg_value(&value)?;
                Some(if value.vtype == REG_EXPAND_SZ {
                    expand_env_strings(&raw)
                } else {
                    raw
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        match user_path_merge(expanded.as_deref(), install_dir) {
            Some(new_path) => {
                env_key.set_value("Path", &new_path)?;
                broadcast_environment_change();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Merge the install dir into a `;`-separated PATH value. Returns the new
    /// value to write, or None if the dir is already present.
    fn user_path_merge(current: Option<&str>, to_add: &str) -> Option<String> {
        let normalized_add = normalize_path_for_compare(to_add);
        let current = current.unwrap_or("");
        let already_present = current
            .split(';')
            .filter(|entry| !entry.trim().is_empty())
            .any(|entry| normalize_path_for_compare(entry) == normalized_add);
        if already_present {
            None
        } else if current.is_empty() {
            Some(to_add.to_string())
        } else {
            Some(format!("{current};{to_add}"))
        }
    }

    /// Full-path-normalized, case-insensitive comparison key, mirroring
    /// install.ps1's `[IO.Path]::GetFullPath($p.Trim()).TrimEnd('\')
    /// .ToLowerInvariant()` (falling back to the trimmed input on error).
    fn normalize_path_for_compare(path: &str) -> String {
        let trimmed = path.trim();
        let full = std::path::absolute(trimmed)
            .map(|abs| abs.to_string_lossy().into_owned())
            .unwrap_or_else(|_| trimmed.to_string());
        full.trim_end_matches('\\').to_lowercase()
    }

    /// Expand `%VAR%` references like `ExpandEnvironmentStringsW` (which is
    /// what .NET registry reads do for REG_EXPAND_SZ values).
    fn expand_env_strings(value: &str) -> String {
        use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;

        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let needed = ExpandEnvironmentStringsW(wide.as_ptr(), std::ptr::null_mut(), 0);
            if needed == 0 {
                return value.to_string();
            }
            let mut buffer = vec![0u16; needed as usize];
            let written = ExpandEnvironmentStringsW(wide.as_ptr(), buffer.as_mut_ptr(), needed);
            if written == 0 || written > needed {
                return value.to_string();
            }
            String::from_utf16_lossy(&buffer[..written as usize - 1])
        }
    }

    /// Notify running applications that the environment changed, matching
    /// .NET's SetEnvironmentVariable behavior (a 1s-timeout broadcast) so new
    /// shells pick up the PATH.
    fn broadcast_environment_change() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
        };

        let param: Vec<u16> = "Environment"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                param.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                1000,
                std::ptr::null_mut(),
            );
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum GitBashStatus {
        NotInstalled,
        Configured(PathBuf),
        AlreadyConfigured(PathBuf),
        Failed(String),
    }

    fn git_bash_installed() -> bool {
        let candidates = [
            ("ProgramFiles", r"Git\bin\bash.exe"),
            ("ProgramFiles(x86)", r"Git\bin\bash.exe"),
            ("LOCALAPPDATA", r"Programs\Git\bin\bash.exe"),
        ];
        candidates.iter().any(|(var, relative)| {
            std::env::var(var)
                .ok()
                .filter(|value| !value.is_empty())
                .is_some_and(|base| Path::new(&base).join(relative).exists())
        })
    }

    /// Prefer ~/.bashrc, fall back to an existing ~/.bash_profile, otherwise
    /// create ~/.bashrc.
    fn select_bash_config(home: &Path) -> PathBuf {
        let bashrc = home.join(".bashrc");
        if bashrc.exists() {
            return bashrc;
        }
        let bash_profile = home.join(".bash_profile");
        if bash_profile.exists() {
            return bash_profile;
        }
        bashrc
    }

    fn configure_git_bash(home: &Path, timestamp: &str) -> GitBashStatus {
        if !git_bash_installed() {
            return GitBashStatus::NotInstalled;
        }
        let target = select_bash_config(home);
        match append_git_bash_path(&target, timestamp) {
            Ok(true) => GitBashStatus::Configured(target),
            Ok(false) => GitBashStatus::AlreadyConfigured(target),
            Err(e) => GitBashStatus::Failed(e.to_string()),
        }
    }

    fn append_git_bash_path(target: &Path, timestamp: &str) -> std::io::Result<bool> {
        if file_contains(target, GIT_BASH_MARKER.as_bytes()) {
            return Ok(false);
        }

        // UTF-8 without BOM and LF line endings, matching install.ps1's
        // AppendAllText with UTF8Encoding($false) and `n.
        append_installer_line(target, timestamp, "export PATH=\"$HOME/.git-ai/bin:$PATH\"")?;
        Ok(true)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;
        use tempfile::tempdir;

        const TS: &str = "2025-08-19 10:00:00";

        #[test]
        fn normalize_path_for_compare_is_case_and_trailing_slash_insensitive() {
            assert_eq!(
                normalize_path_for_compare(r"C:\Users\Test\.git-ai\bin\"),
                normalize_path_for_compare(r"c:\users\test\.git-ai\bin")
            );
            assert_eq!(
                normalize_path_for_compare(r"  C:\Users\Test\.git-ai\bin  "),
                normalize_path_for_compare(r"C:\Users\Test\.git-ai\bin")
            );
        }

        #[test]
        fn user_path_merge_appends_when_missing() {
            assert_eq!(
                user_path_merge(Some(r"C:\Windows;C:\Tools"), r"C:\Users\t\.git-ai\bin"),
                Some(r"C:\Windows;C:\Tools;C:\Users\t\.git-ai\bin".to_string())
            );
        }

        #[test]
        fn user_path_merge_uses_dir_alone_when_path_is_empty_or_missing() {
            assert_eq!(
                user_path_merge(None, r"C:\Users\t\.git-ai\bin"),
                Some(r"C:\Users\t\.git-ai\bin".to_string())
            );
            assert_eq!(
                user_path_merge(Some(""), r"C:\Users\t\.git-ai\bin"),
                Some(r"C:\Users\t\.git-ai\bin".to_string())
            );
        }

        #[test]
        fn user_path_merge_detects_existing_entry_with_different_casing() {
            assert_eq!(
                user_path_merge(
                    Some(r"C:\Windows;c:\users\T\.GIT-AI\BIN\"),
                    r"C:\Users\t\.git-ai\bin"
                ),
                None
            );
        }

        #[test]
        fn expand_env_strings_resolves_userprofile_references() {
            let userprofile = std::env::var("USERPROFILE").unwrap();
            assert_eq!(
                expand_env_strings(r"%USERPROFILE%\.git-ai\bin"),
                format!(r"{userprofile}\.git-ai\bin")
            );
        }

        #[test]
        fn select_bash_config_prefers_bashrc_then_bash_profile_then_creates_bashrc() {
            let home = tempdir().unwrap();
            assert_eq!(select_bash_config(home.path()), home.path().join(".bashrc"));

            fs::write(home.path().join(".bash_profile"), "# profile\n").unwrap();
            assert_eq!(
                select_bash_config(home.path()),
                home.path().join(".bash_profile")
            );

            fs::write(home.path().join(".bashrc"), "# bashrc\n").unwrap();
            assert_eq!(select_bash_config(home.path()), home.path().join(".bashrc"));
        }

        #[test]
        fn append_git_bash_path_writes_utf8_lf_without_bom() {
            let home = tempdir().unwrap();
            let target = home.path().join(".bashrc");

            assert!(append_git_bash_path(&target, TS).unwrap());

            let bytes = fs::read(&target).unwrap();
            assert!(
                !bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
                "must not write a BOM"
            );
            assert!(!bytes.contains(&b'\r'), "must use LF line endings");
            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                format!(
                    "\n# Added by git-ai installer on {TS}\nexport PATH=\"$HOME/.git-ai/bin:$PATH\"\n"
                )
            );
        }

        #[test]
        fn append_git_bash_path_is_idempotent_via_marker() {
            let home = tempdir().unwrap();
            let target = home.path().join(".bashrc");

            assert!(append_git_bash_path(&target, TS).unwrap());
            let after_first = fs::read(&target).unwrap();
            assert!(!append_git_bash_path(&target, TS).unwrap());
            assert_eq!(fs::read(&target).unwrap(), after_first);
        }

        #[test]
        fn append_git_bash_path_skips_files_with_preexisting_marker() {
            let home = tempdir().unwrap();
            let target = home.path().join(".bashrc");
            fs::write(
                &target,
                "export PATH=\"$HOME/.git-ai/bin:$PATH\" # custom\n",
            )
            .unwrap();

            assert!(!append_git_bash_path(&target, TS).unwrap());
        }
    }
}
