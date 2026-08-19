//! Shell/environment configuration for `install-hooks --env`.
//!
//! Ports the shell-profile PATH setup that previously lived in `install.sh`
//! and `install.ps1`. Everything here is best-effort: failures are printed as
//! warnings and never fail the install command, matching the scripts'
//! warn-and-continue semantics.

/// Apply the shell/environment configuration for the current platform.
pub fn configure_shell_env() {
    #[cfg(unix)]
    unix::configure_shell_env();
}

#[cfg(unix)]
mod unix {
    use crate::mdm::utils::home_dir;
    use std::fs;
    use std::io::Write;
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
        let login_shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty());
        let timestamp = chrono::Local::now()
            .format("%a %b %e %H:%M:%S %Y")
            .to_string();
        let report = apply_env_config(&home, login_shell.as_deref(), &timestamp);
        print!("{}", render_report(&report, &install_dir_string(&home)));
        chown_created_paths(&report.created_paths);
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

        if !config_file.is_file() {
            report.created_paths.push(config_file.clone());
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(config_file)?;

        if file_contains(config_file, install_dir.as_bytes()) {
            report.already_configured.push(shell_config.clone());
        } else {
            file.write_all(
                format!("\n# Added by git-ai installer on {timestamp}\n{path_cmd}\n").as_bytes(),
            )?;
            report.configured.push(shell_config.clone());
        }

        Ok(())
    }

    /// Byte-level fixed-string search, matching `grep -qsF` (no UTF-8
    /// requirement; missing/unreadable file counts as "not present").
    fn file_contains(path: &Path, needle: &[u8]) -> bool {
        match fs::read(path) {
            Ok(bytes) => bytes.windows(needle.len()).any(|window| window == needle),
            Err(_) => false,
        }
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
            out.push_str("\nCould not detect any shell config files.\n");
            out.push_str("Please add the following line to your shell config and restart:\n");
            out.push_str(&format!("  export PATH=\"{install_dir}:$PATH\"\n"));
        }

        out
    }

    /// In root/MDM installs (e.g. JAMF), hand ownership of any files this
    /// process created back to the target user, mirroring the
    /// `chown "$INSTALL_USER" "$created_path"` loop from install.sh. The
    /// installer script passes the user via GIT_AI_INSTALL_USER.
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
                    "\nCould not detect any shell config files.\n\
                     Please add the following line to your shell config and restart:\n\
                     \x20 export PATH=\"{install_dir}:$PATH\"\n"
                )
            );
        }
    }
}
