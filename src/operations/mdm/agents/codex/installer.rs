use crate::config::{CodexHooksFormat, Config};
use crate::error::GitAiError;
use crate::operations::mdm::editor_cli::binary_exists;
use crate::operations::mdm::file_ops::{generate_diff, write_atomic};
use crate::operations::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::operations::mdm::paths::codex_home_dir;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use toml::Value as TomlValue;
use toml::map::Map;

use super::CodexInstaller;

// --- Path helpers and command construction ---

impl CodexInstaller {
    pub(super) fn config_path() -> PathBuf {
        codex_home_dir().join("config.toml")
    }

    pub(super) fn hooks_json_path() -> PathBuf {
        codex_home_dir().join("hooks.json")
    }

    pub(super) fn desired_command(binary_path: &Path) -> String {
        format!("{} {}", binary_path.display(), super::CODEX_CHECKPOINT_CMD)
    }
}

// --- HookInstaller implementation ---

impl HookInstaller for CodexInstaller {
    fn name(&self) -> &str {
        "Codex"
    }

    fn id(&self) -> &str {
        "codex"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["codex"]
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let has_binary = binary_exists("codex");
        let has_dotfiles = codex_home_dir().exists();

        if !has_binary && !has_dotfiles {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let config_path = Self::config_path();
        let config = if config_path.exists() {
            Self::parse_config_toml(&fs::read_to_string(&config_path)?)?
        } else {
            TomlValue::Table(Map::new())
        };
        let hooks_json_path = Self::hooks_json_path();
        let hooks_json = if hooks_json_path.exists() {
            Self::parse_hooks_json(&fs::read_to_string(&hooks_json_path)?)?
        } else {
            json!({})
        };

        if Config::fresh().codex_hooks_format() == CodexHooksFormat::HooksJson {
            let config_without_notify =
                Self::remove_notify_if_git_ai(&config)?.unwrap_or(config.clone());
            let (config_without_inline_hooks, _) =
                Self::remove_inline_hooks_from_config(&config_without_notify)?;
            let desired_config =
                Self::config_with_hooks_feature_enabled(&config_without_inline_hooks)?;
            let desired_hooks_json =
                Self::hooks_json_with_installed_hooks(&hooks_json, &params.binary_path)?;
            let has_json_hooks = Self::hooks_json_has_git_ai_entries(&hooks_json);
            let hooks_installed = Self::config_hooks_feature_enabled(&config) && has_json_hooks;
            let hooks_up_to_date = config == desired_config && hooks_json == desired_hooks_json;

            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed,
                hooks_up_to_date,
            });
        }

        let desired_config = Self::config_with_installed_hooks(&config, &params.binary_path)?;
        let has_inline_hooks = Self::config_has_inline_hooks(&config);
        let has_legacy_hooks_json = Self::hooks_json_has_git_ai_entries(&hooks_json);
        let hooks_installed = Self::config_hooks_feature_enabled(&config)
            && (has_inline_hooks || has_legacy_hooks_json);
        let hooks_up_to_date = config == desired_config && !has_legacy_hooks_json;

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed,
            hooks_up_to_date,
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let config_path = Self::config_path();
        let hooks_json_path = Self::hooks_json_path();

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let existing_config_content = if config_path.exists() {
            fs::read_to_string(&config_path)?
        } else {
            String::new()
        };

        let existing_config = Self::parse_config_toml(&existing_config_content)?;

        let existing_hooks_content = if hooks_json_path.exists() {
            fs::read_to_string(&hooks_json_path)?
        } else {
            String::new()
        };
        let existing_hooks = Self::parse_hooks_json(&existing_hooks_content)?;

        if Config::fresh().codex_hooks_format() == CodexHooksFormat::HooksJson {
            let config_without_notify =
                Self::remove_notify_if_git_ai(&existing_config)?.unwrap_or(existing_config.clone());
            let (config_without_inline_hooks, _) =
                Self::remove_inline_hooks_from_config(&config_without_notify)?;
            let merged_config =
                Self::config_with_hooks_feature_enabled(&config_without_inline_hooks)?;
            let merged_hooks =
                Self::hooks_json_with_installed_hooks(&existing_hooks, &params.binary_path)?;

            let config_changed = existing_config != merged_config;
            let hooks_json_changed = existing_hooks != merged_hooks;
            if !config_changed && !hooks_json_changed {
                return Ok(None);
            }

            let mut diff_output = Vec::new();

            if config_changed {
                let new_config_content = toml::to_string_pretty(&merged_config).map_err(|e| {
                    GitAiError::Generic(format!("Failed to serialize Codex config.toml: {e}"))
                })?;
                diff_output.push(generate_diff(
                    &config_path,
                    &existing_config_content,
                    &new_config_content,
                ));
                if !dry_run {
                    write_atomic(&config_path, new_config_content.as_bytes())?;
                }
            }

            if hooks_json_changed {
                let new_hooks_content = serde_json::to_string_pretty(&merged_hooks)?;
                diff_output.push(generate_diff(
                    &hooks_json_path,
                    &existing_hooks_content,
                    &new_hooks_content,
                ));
                if !dry_run {
                    write_atomic(&hooks_json_path, new_hooks_content.as_bytes())?;
                }
            }

            return Ok(Some(diff_output.join("\n")));
        }

        let merged_config =
            Self::config_with_installed_hooks(&existing_config, &params.binary_path)?;

        // Check if legacy hooks.json needs migration
        let (hooks_json_changed, existing_hooks_content) = if hooks_json_path.exists() {
            let (_cleaned_hooks, changed) = Self::remove_codex_hooks_from_json(&existing_hooks)?;
            (changed, existing_hooks_content)
        } else {
            (false, String::new())
        };

        let config_changed = existing_config != merged_config;
        if !config_changed && !hooks_json_changed {
            return Ok(None);
        }

        let mut diff_output = Vec::new();

        // Write config.toml FIRST (contains the replacement inline hooks)
        if config_changed {
            let new_config_content = toml::to_string_pretty(&merged_config).map_err(|e| {
                GitAiError::Generic(format!("Failed to serialize Codex config.toml: {e}"))
            })?;
            diff_output.push(generate_diff(
                &config_path,
                &existing_config_content,
                &new_config_content,
            ));
            if !dry_run {
                write_atomic(&config_path, new_config_content.as_bytes())?;
            }
        }

        // THEN clean up legacy hooks.json (safe: config.toml already has the hooks)
        if hooks_json_changed {
            let existing_hooks = Self::parse_hooks_json(&existing_hooks_content)?;
            let (cleaned_hooks, _) = Self::remove_codex_hooks_from_json(&existing_hooks)?;
            if Self::hooks_json_has_any_entries(&cleaned_hooks) {
                let new_hooks_content = serde_json::to_string_pretty(&cleaned_hooks)?;
                diff_output.push(generate_diff(
                    &hooks_json_path,
                    &existing_hooks_content,
                    &new_hooks_content,
                ));
                if !dry_run {
                    write_atomic(&hooks_json_path, new_hooks_content.as_bytes())?;
                }
            } else {
                diff_output.push(generate_diff(&hooks_json_path, &existing_hooks_content, ""));
                if !dry_run {
                    fs::remove_file(&hooks_json_path)?;
                }
            }
        }

        Ok(Some(diff_output.join("\n")))
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let config_path = Self::config_path();
        let hooks_json_path = Self::hooks_json_path();
        if !config_path.exists() && !hooks_json_path.exists() {
            return Ok(None);
        }

        let existing_config_content = if config_path.exists() {
            fs::read_to_string(&config_path)?
        } else {
            String::new()
        };
        let existing_config = Self::parse_config_toml(&existing_config_content)?;

        // Remove inline hooks from config.toml
        let config_without_notify =
            Self::remove_notify_if_git_ai(&existing_config)?.unwrap_or(existing_config.clone());
        let (config_without_hooks, inline_hooks_changed) =
            Self::remove_inline_hooks_from_config(&config_without_notify)?;
        let merged_config = Self::remove_feature_flags(&config_without_hooks)?;

        // Check if legacy hooks.json needs cleanup
        let (hooks_json_changed, existing_hooks_content) = if hooks_json_path.exists() {
            let content = fs::read_to_string(&hooks_json_path)?;
            let existing_hooks = Self::parse_hooks_json(&content)?;
            let (_cleaned_hooks, changed) = Self::remove_codex_hooks_from_json(&existing_hooks)?;
            (changed, content)
        } else {
            (false, String::new())
        };

        let config_changed = merged_config != existing_config;
        if !config_changed && !inline_hooks_changed && !hooks_json_changed {
            return Ok(None);
        }

        let mut diff_output = Vec::new();

        // Write config.toml changes first
        if config_changed || inline_hooks_changed {
            let new_config_content = toml::to_string_pretty(&merged_config).map_err(|e| {
                GitAiError::Generic(format!("Failed to serialize Codex config.toml: {e}"))
            })?;
            diff_output.push(generate_diff(
                &config_path,
                &existing_config_content,
                &new_config_content,
            ));
            if !dry_run {
                write_atomic(&config_path, new_config_content.as_bytes())?;
            }
        }

        // Then clean up legacy hooks.json
        if hooks_json_changed {
            let existing_hooks = Self::parse_hooks_json(&existing_hooks_content)?;
            let (cleaned_hooks, _) = Self::remove_codex_hooks_from_json(&existing_hooks)?;
            if Self::hooks_json_has_any_entries(&cleaned_hooks) {
                let new_hooks_content = serde_json::to_string_pretty(&cleaned_hooks)?;
                diff_output.push(generate_diff(
                    &hooks_json_path,
                    &existing_hooks_content,
                    &new_hooks_content,
                ));
                if !dry_run {
                    write_atomic(&hooks_json_path, new_hooks_content.as_bytes())?;
                }
            } else {
                diff_output.push(generate_diff(&hooks_json_path, &existing_hooks_content, ""));
                if !dry_run {
                    fs::remove_file(&hooks_json_path)?;
                }
            }
        }

        Ok(Some(diff_output.join("\n")))
    }
}
