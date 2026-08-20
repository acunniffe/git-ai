//! Best-effort installation of Git AI into Windows Subsystem for Linux distros.

#[cfg(windows)]
use crate::authorship::authorship_log_serialization::GIT_AI_VERSION;

#[cfg(any(windows, test))]
const INSTALL_SCRIPT: &str = "curl -fsSL https://usegitai.com/install.sh | bash";

pub struct InstallConfig<'a> {
    pub api_base: Option<&'a str>,
    pub api_key: Option<&'a str>,
}

pub fn install(config: InstallConfig<'_>, dry_run: bool) {
    if dry_run {
        println!("Dry-run: skipping WSL installation.");
        return;
    }

    #[cfg(windows)]
    install_on_windows(config);

    #[cfg(not(windows))]
    {
        let _ = config;
        eprintln!(
            "[git-ai] warning: --wsl is only supported on Windows; skipping WSL installation."
        );
    }
}

#[cfg(windows)]
fn install_on_windows(config: InstallConfig<'_>) {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    use crate::utils::CREATE_NO_WINDOW;

    let output = match Command::new("wsl.exe")
        .args(["--list", "--quiet"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            eprintln!(
                "[git-ai] warning: could not list WSL distributions (exit code {}); skipping WSL installation.",
                output.status
            );
            return;
        }
        Err(error) => {
            eprintln!(
                "[git-ai] warning: WSL is not available ({error}); skipping WSL installation."
            );
            return;
        }
    };

    let distros = eligible_distros(&decode_wsl_output(&output.stdout));
    if distros.is_empty() {
        println!("No eligible WSL distributions detected; skipping Linux installation.");
        return;
    }

    let release_tag = release_tag(
        std::env::var("GIT_AI_RELEASE_TAG").ok().as_deref(),
        GIT_AI_VERSION,
        cfg!(debug_assertions),
    );
    let failures = install_in_distros(&distros, |distro| {
        println!("Installing Git AI in WSL distribution: {distro}");
        let args = install_args(distro);
        let env = forwarded_env(
            config.api_base,
            config.api_key,
            release_tag.as_deref(),
            std::env::var("WSLENV").ok().as_deref(),
        );
        let mut command = Command::new("wsl.exe");
        command
            .args(&args)
            .envs(env)
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        let status = command.status();
        match status {
            Ok(status) if status.success() => true,
            Ok(status) => {
                eprintln!(
                    "[git-ai] warning: Git AI installation failed in WSL distribution '{distro}' ({status})."
                );
                false
            }
            Err(error) => {
                eprintln!(
                    "[git-ai] warning: could not start Git AI installation in WSL distribution '{distro}': {error}"
                );
                false
            }
        }
    });

    if failures.is_empty() {
        println!(
            "Successfully installed Git AI in {} WSL distribution(s).",
            distros.len()
        );
    } else {
        eprintln!(
            "[git-ai] warning: Git AI could not be installed in {} of {} WSL distribution(s): {}",
            failures.len(),
            distros.len(),
            failures.join(", ")
        );
    }
}

#[cfg(any(windows, test))]
fn decode_wsl_output(bytes: &[u8]) -> String {
    if bytes.len() >= 2
        && bytes.len().is_multiple_of(2)
        && bytes.chunks_exact(2).any(|pair| pair[1] == 0)
    {
        let words = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        String::from_utf16_lossy(&words)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

#[cfg(any(windows, test))]
fn eligible_distros(output: &str) -> Vec<String> {
    output
        .lines()
        .map(|line| line.trim().trim_start_matches('\u{feff}'))
        .filter(|line| !line.is_empty())
        .filter(|line| !line.to_ascii_lowercase().starts_with("docker-desktop"))
        .map(str::to_owned)
        .collect()
}

#[cfg(any(windows, test))]
fn release_tag(env_release_tag: Option<&str>, binary_version: &str, debug: bool) -> Option<String> {
    if let Some(tag) = env_release_tag
        .map(str::trim)
        .filter(|tag| !tag.is_empty() && *tag != "latest")
    {
        return Some(tag.to_owned());
    }
    if debug || binary_version.starts_with("development") {
        None
    } else {
        Some(format!("v{}", binary_version.trim_start_matches('v')))
    }
}

#[cfg(any(windows, test))]
fn install_args(distro: &str) -> Vec<String> {
    vec![
        "--distribution".to_owned(),
        distro.to_owned(),
        "--exec".to_owned(),
        "bash".to_owned(),
        "-c".to_owned(),
        INSTALL_SCRIPT.to_owned(),
    ]
}

#[cfg(any(windows, test))]
fn forwarded_env(
    api_base: Option<&str>,
    api_key: Option<&str>,
    release_tag: Option<&str>,
    inherited_wslenv: Option<&str>,
) -> Vec<(String, String)> {
    let values = [
        ("API_BASE", api_base),
        ("API_KEY", api_key),
        ("GIT_AI_RELEASE_TAG", release_tag),
    ];
    let mut env = values
        .into_iter()
        .filter_map(|(name, value)| value.map(|value| (name.to_owned(), value.to_owned())))
        .collect::<Vec<_>>();
    if env.is_empty() {
        return env;
    }

    let mut wslenv = inherited_wslenv
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_default();
    for (name, _) in &env {
        let already_forwarded = wslenv.split(':').any(|entry| {
            entry
                .split_once('/')
                .map_or(entry, |(entry_name, _)| entry_name)
                .eq_ignore_ascii_case(name)
        });
        if !already_forwarded {
            if !wslenv.is_empty() {
                wslenv.push(':');
            }
            wslenv.push_str(name);
        }
    }
    env.push(("WSLENV".to_owned(), wslenv));
    env
}

#[cfg(any(windows, test))]
fn install_in_distros<F>(distros: &[String], mut install_one: F) -> Vec<&str>
where
    F: FnMut(&str) -> bool,
{
    let mut failures = Vec::new();
    for distro in distros {
        if !install_one(distro) {
            failures.push(distro.as_str());
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf16_wsl_output_and_filters_docker_distros() {
        let text = "\u{feff}Ubuntu\r\ndocker-desktop\r\nDocker-Desktop-Data\r\nDebian\r\n";
        let bytes = text
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();

        assert_eq!(
            eligible_distros(&decode_wsl_output(&bytes)),
            ["Ubuntu", "Debian"]
        );
    }

    #[test]
    fn decodes_utf8_wsl_output_and_ignores_blank_lines() {
        assert_eq!(
            eligible_distros(&decode_wsl_output(b"Ubuntu\n\n Debian \n")),
            ["Ubuntu", "Debian"]
        );
    }

    #[test]
    fn install_arguments_do_not_expose_forwarded_configuration() {
        assert_eq!(
            install_args("Ubuntu Dev"),
            [
                "--distribution",
                "Ubuntu Dev",
                "--exec",
                "bash",
                "-c",
                INSTALL_SCRIPT,
            ]
        );
    }

    #[test]
    fn forwarded_configuration_uses_wslenv_without_overwriting_existing_entries() {
        assert_eq!(
            forwarded_env(
                Some("https://enterprise.example/path?a=b"),
                Some("key with spaces; untouched"),
                Some("v1.2.3"),
                Some("EXISTING/p:API_KEY/u")
            ),
            [
                (
                    "API_BASE".to_owned(),
                    "https://enterprise.example/path?a=b".to_owned()
                ),
                (
                    "API_KEY".to_owned(),
                    "key with spaces; untouched".to_owned()
                ),
                ("GIT_AI_RELEASE_TAG".to_owned(), "v1.2.3".to_owned()),
                (
                    "WSLENV".to_owned(),
                    "EXISTING/p:API_KEY/u:API_BASE:GIT_AI_RELEASE_TAG".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn explicit_release_tag_wins_and_latest_falls_back() {
        assert_eq!(
            release_tag(Some("v9.8.7"), "1.2.3", false).as_deref(),
            Some("v9.8.7")
        );
        assert_eq!(
            release_tag(Some("latest"), "1.2.3", false).as_deref(),
            Some("v1.2.3")
        );
        assert_eq!(release_tag(None, "development:1.2.3", true), None);
    }

    #[test]
    fn attempts_every_distro_after_failures() {
        let distros = vec![
            "Ubuntu".to_owned(),
            "Debian".to_owned(),
            "Fedora".to_owned(),
        ];
        let mut attempted = Vec::new();

        let failures = install_in_distros(&distros, |distro| {
            attempted.push(distro.to_owned());
            distro != "Debian"
        });

        assert_eq!(attempted, distros);
        assert_eq!(failures, ["Debian"]);
    }
}
