use crate::config::Config;
use crate::mdm::utils::home_dir;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentProfile {
    Claude,
    Codex,
    Cursor,
    Droid,
    Gemini,
    ContinueCli,
    CopilotCli,
    Amp,
}

impl AgentProfile {
    pub fn config_key(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Droid => "droid",
            Self::Gemini => "gemini",
            Self::ContinueCli => "continue_cli",
            Self::CopilotCli => "copilot_cli",
            Self::Amp => "amp",
        }
    }
}

pub fn agent_profile_roots(agent: AgentProfile, config: &Config) -> Vec<PathBuf> {
    let home = home_dir();
    let environment = official_environment_override_root(agent);
    let default = official_default_root_for_home(agent, &home);
    let configured = config
        .agent_profile_roots()
        .get(agent.config_key())
        .map(Vec::as_slice)
        .unwrap_or_default();

    merge_profile_roots(configured, environment.as_deref(), &default, &home)
}

/// 返回该 agent 的官方环境变量覆盖根目录（如 `CODEX_HOME`、`CLAUDE_CONFIG_DIR`、
/// `CURSOR_CONFIG_DIR`、`GEMINI_CLI_HOME`），仅当对应环境变量已设置且非空时返回 `Some`。
///
/// 与 [`official_default_root`] 对应：两者都属于“官方”根，即使目录尚不存在，install 路径
/// 也应主动创建并安装 hooks。这与用户显式配置的额外根（`agent_profile_roots` 配置项）不同——
/// 后者必须已存在才会被安装，避免为任意配置路径凭空创建目录。
pub fn official_environment_override_root(agent: AgentProfile) -> Option<PathBuf> {
    let environment_value = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
    };
    match agent {
        AgentProfile::Claude => environment_value("CLAUDE_CONFIG_DIR"),
        AgentProfile::Codex => environment_value("CODEX_HOME"),
        AgentProfile::Cursor => environment_value("CURSOR_CONFIG_DIR"),
        AgentProfile::Droid | AgentProfile::ContinueCli | AgentProfile::CopilotCli => None,
        AgentProfile::Gemini => {
            environment_value("GEMINI_CLI_HOME").map(|path| path.join(".gemini"))
        }
        AgentProfile::Amp => None,
    }
}

/// 返回 install 路径应当处理的 profile roots：已存在的目录，加上两个“官方”根
/// （官方默认根 + 官方环境变量覆盖根）——即使这两个官方根对应的目录尚不存在也包含在内，
/// 因为 install 会负责创建它们。
///
/// check/uninstall 路径不应使用本函数：它们应继续基于 [`agent_profile_roots`] + `is_dir()`，
/// 因为只有在目录已存在时才需要检查或卸载其中的 hooks。
pub fn install_profile_roots(agent: AgentProfile, config: &Config) -> Vec<PathBuf> {
    let default = official_default_root(agent);
    let environment_override = official_environment_override_root(agent);
    agent_profile_roots(agent, config)
        .into_iter()
        .filter(|root| {
            root.is_dir() || root == &default || environment_override.as_ref() == Some(root)
        })
        .collect()
}

pub fn official_default_root(agent: AgentProfile) -> PathBuf {
    official_default_root_for_home(agent, &home_dir())
}

fn official_default_root_for_home(agent: AgentProfile, home: &Path) -> PathBuf {
    match agent {
        AgentProfile::Claude => home.join(".claude"),
        AgentProfile::Codex => home.join(".codex"),
        AgentProfile::Cursor => home.join(".cursor"),
        AgentProfile::Droid => home.join(".factory"),
        AgentProfile::Gemini => home.join(".gemini"),
        AgentProfile::ContinueCli => home.join(".continue"),
        AgentProfile::CopilotCli => home.join(".copilot"),
        AgentProfile::Amp => amp_data_root(home),
    }
}

#[cfg(not(target_os = "windows"))]
fn amp_data_root(home: &Path) -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("amp")
}

#[cfg(target_os = "windows")]
fn amp_data_root(home: &Path) -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .or_else(|_| std::env::var("APPDATA"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData/Local"))
        .join("amp")
}

fn merge_profile_roots(
    configured: &[String],
    environment: Option<&Path>,
    default: &Path,
    home: &Path,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();

    let configured = configured.iter().filter_map(|value| {
        let value = value.trim();
        if value == "~" {
            Some(home.to_path_buf())
        } else if let Some(relative) = value.strip_prefix("~/") {
            Some(home.join(relative))
        } else {
            let path = PathBuf::from(value);
            path.is_absolute().then_some(path)
        }
    });

    for root in configured
        .chain(
            environment
                .filter(|path| path.is_absolute())
                .map(Path::to_path_buf),
        )
        .chain(std::iter::once(default.to_path_buf()))
    {
        let identity = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        if seen.insert(identity) {
            roots.push(root);
        }
    }

    roots
}

#[cfg(test)]
mod tests {
    use super::{
        AgentProfile, agent_profile_roots, install_profile_roots, merge_profile_roots,
        official_default_root_for_home, official_environment_override_root,
    };
    use crate::config::Config;
    use std::fs;

    #[test]
    fn configured_environment_and_default_roots_are_merged_without_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let default = home.join(".codex");
        let configured = home.join(".codex-personal2");
        let environment = home.join(".codex-work");
        let unconfigured_sibling = home.join(".codex-personal");
        for path in [&default, &configured, &environment, &unconfigured_sibling] {
            fs::create_dir_all(path).unwrap();
        }

        let configured_values = vec![configured.to_string_lossy().into_owned()];
        let roots = merge_profile_roots(
            &configured_values,
            Some(environment.as_path()),
            &default,
            home,
        );

        assert_eq!(roots, vec![configured, environment, default]);
        assert!(!roots.contains(&unconfigured_sibling));
    }

    #[test]
    fn roots_expand_home_ignore_relative_paths_and_deduplicate() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let default = home.join(".codex");
        let configured = vec![
            "~/.codex-personal2".to_string(),
            "relative/profile".to_string(),
            "~/.codex-personal2".to_string(),
        ];

        let roots = merge_profile_roots(&configured, Some(&default), &default, home);

        assert_eq!(roots, vec![home.join(".codex-personal2"), default]);
    }

    #[test]
    #[serial_test::serial]
    fn codex_roots_include_config_environment_and_official_default() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let config_dir = home.join(".git-ai");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.json"),
            r#"{
                "agent_profile_roots": {
                    "codex": ["~/.codex-personal2"]
                }
            }"#,
        )
        .unwrap();
        let environment_root = home.join(".codex-work");

        let previous_home = std::env::var_os("HOME");
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("CODEX_HOME", &environment_root);
        }
        let config = Config::fresh();
        let roots = agent_profile_roots(AgentProfile::Codex, &config);
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match previous_codex_home {
            Some(value) => unsafe { std::env::set_var("CODEX_HOME", value) },
            None => unsafe { std::env::remove_var("CODEX_HOME") },
        }

        assert_eq!(
            roots,
            vec![
                home.join(".codex-personal2"),
                environment_root,
                home.join(".codex")
            ]
        );
    }

    #[test]
    fn every_sweep_agent_has_a_stable_config_key() {
        assert_eq!(AgentProfile::Claude.config_key(), "claude");
        assert_eq!(AgentProfile::Codex.config_key(), "codex");
        assert_eq!(AgentProfile::Cursor.config_key(), "cursor");
        assert_eq!(AgentProfile::Droid.config_key(), "droid");
        assert_eq!(AgentProfile::Gemini.config_key(), "gemini");
        assert_eq!(AgentProfile::ContinueCli.config_key(), "continue_cli");
        assert_eq!(AgentProfile::CopilotCli.config_key(), "copilot_cli");
        assert_eq!(AgentProfile::Amp.config_key(), "amp");
    }

    #[test]
    fn every_sweep_agent_keeps_its_official_default() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        assert_eq!(
            official_default_root_for_home(AgentProfile::Claude, home),
            home.join(".claude")
        );
        assert_eq!(
            official_default_root_for_home(AgentProfile::Codex, home),
            home.join(".codex")
        );
        assert_eq!(
            official_default_root_for_home(AgentProfile::Cursor, home),
            home.join(".cursor")
        );
        assert_eq!(
            official_default_root_for_home(AgentProfile::Droid, home),
            home.join(".factory")
        );
        assert_eq!(
            official_default_root_for_home(AgentProfile::Gemini, home),
            home.join(".gemini")
        );
        assert_eq!(
            official_default_root_for_home(AgentProfile::ContinueCli, home),
            home.join(".continue")
        );
        assert_eq!(
            official_default_root_for_home(AgentProfile::CopilotCli, home),
            home.join(".copilot")
        );
        assert!(official_default_root_for_home(AgentProfile::Amp, home).ends_with("amp"));
    }

    #[test]
    #[serial_test::serial]
    fn environment_override_root_resolves_official_env_vars_per_agent() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();

        let previous_home = std::env::var_os("HOME");
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let previous_claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR");
        let previous_cursor_config_dir = std::env::var_os("CURSOR_CONFIG_DIR");
        let previous_gemini_cli_home = std::env::var_os("GEMINI_CLI_HOME");
        unsafe {
            std::env::set_var("HOME", home);
            std::env::remove_var("CODEX_HOME");
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::remove_var("CURSOR_CONFIG_DIR");
            std::env::remove_var("GEMINI_CLI_HOME");
        }

        // Droid 等没有官方环境变量覆盖的 agent 恒为 None
        assert_eq!(
            official_environment_override_root(AgentProfile::Droid),
            None
        );

        let codex_override = home.join("codex-override");
        let claude_override = home.join("claude-override");
        let cursor_override = home.join("cursor-override");
        let gemini_cli_home = home.join("gemini-cli-home");
        unsafe {
            std::env::set_var("CODEX_HOME", &codex_override);
            std::env::set_var("CLAUDE_CONFIG_DIR", &claude_override);
            std::env::set_var("CURSOR_CONFIG_DIR", &cursor_override);
            std::env::set_var("GEMINI_CLI_HOME", &gemini_cli_home);
        }

        assert_eq!(
            official_environment_override_root(AgentProfile::Codex),
            Some(codex_override)
        );
        assert_eq!(
            official_environment_override_root(AgentProfile::Claude),
            Some(claude_override)
        );
        assert_eq!(
            official_environment_override_root(AgentProfile::Cursor),
            Some(cursor_override)
        );
        // Gemini 官方根固定落在 GEMINI_CLI_HOME 下的 .gemini 子目录
        assert_eq!(
            official_environment_override_root(AgentProfile::Gemini),
            Some(gemini_cli_home.join(".gemini"))
        );

        unsafe {
            match previous_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match previous_codex_home {
                Some(v) => std::env::set_var("CODEX_HOME", v),
                None => std::env::remove_var("CODEX_HOME"),
            }
            match previous_claude_config_dir {
                Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
                None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
            }
            match previous_cursor_config_dir {
                Some(v) => std::env::set_var("CURSOR_CONFIG_DIR", v),
                None => std::env::remove_var("CURSOR_CONFIG_DIR"),
            }
            match previous_gemini_cli_home {
                Some(v) => std::env::set_var("GEMINI_CLI_HOME", v),
                None => std::env::remove_var("GEMINI_CLI_HOME"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn install_profile_roots_includes_missing_environment_override_but_filters_missing_configured()
    {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let config_dir = home.join(".git-ai");
        fs::create_dir_all(&config_dir).unwrap();
        // 用户显式配置的额外根：已存在的参与 install；尚不存在的必须被过滤，install 不凭空创建
        let existing_configured = home.join(".codex-configured-existing");
        let missing_configured = home.join(".codex-configured-missing");
        fs::create_dir_all(&existing_configured).unwrap();
        fs::write(
            config_dir.join("config.json"),
            r#"{
                "agent_profile_roots": {
                    "codex": ["~/.codex-configured-missing", "~/.codex-configured-existing"]
                }
            }"#,
        )
        .unwrap();

        // 官方环境变量覆盖根尚不存在：作为官方根，即使缺失也必须参与 install
        let missing_env_override = home.join("codex-env-override-missing");

        let previous_home = std::env::var_os("HOME");
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("CODEX_HOME", &missing_env_override);
        }
        let config = Config::fresh();
        let roots = install_profile_roots(AgentProfile::Codex, &config);
        unsafe {
            match previous_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match previous_codex_home {
                Some(v) => std::env::set_var("CODEX_HOME", v),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }

        let default_root = home.join(".codex");
        assert!(
            roots.contains(&default_root),
            "official default root must always be install-eligible: {roots:?}"
        );
        assert!(
            roots.contains(&missing_env_override),
            "official env-override root must remain install-eligible even when missing: {roots:?}"
        );
        assert!(
            roots.contains(&existing_configured),
            "existing configured root must be install-eligible: {roots:?}"
        );
        assert!(
            !roots.contains(&missing_configured),
            "missing non-official configured root must not be install-eligible: {roots:?}"
        );
    }
}
