use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{
    binary_exists, generate_diff, grok_home_dir, is_git_ai_checkpoint_command,
    normalize_windows_path_for_shell, write_atomic,
};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const GROK_CHECKPOINT_CMD: &str = "checkpoint grok --hook-input stdin";

pub struct GrokInstaller;

impl GrokInstaller {
    fn hooks_path() -> PathBuf {
        grok_home_dir().join("hooks").join("git-ai.json")
    }

    fn desired_hooks(binary_path: &Path) -> Value {
        let command = format!(
            "{} {}",
            normalize_windows_path_for_shell(binary_path),
            GROK_CHECKPOINT_CMD
        );
        let hook = json!({
            "type": "command",
            "command": command,
            "timeout": 10
        });
        json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "*",
                    "hooks": [hook.clone()]
                }],
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [hook]
                }]
            }
        })
    }

    fn has_grok_checkpoint(content: &str) -> bool {
        content.contains("checkpoint grok") && is_git_ai_checkpoint_command(content)
    }
}

impl HookInstaller for GrokInstaller {
    fn name(&self) -> &str {
        "Grok"
    }

    fn id(&self) -> &str {
        "grok"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["grok"]
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let tool_installed = binary_exists("grok") || grok_home_dir().exists();
        if !tool_installed {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let hooks_path = Self::hooks_path();
        if !hooks_path.exists() {
            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let current = fs::read_to_string(&hooks_path).unwrap_or_default();
        let desired = serde_json::to_string_pretty(&Self::desired_hooks(&params.binary_path))?;
        let installed = Self::has_grok_checkpoint(&current);

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: installed,
            hooks_up_to_date: installed && current.trim() == desired.trim(),
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let hooks_path = Self::hooks_path();
        if let Some(dir) = hooks_path.parent()
            && !dry_run
        {
            fs::create_dir_all(dir)?;
        }

        let existing = if hooks_path.exists() {
            fs::read_to_string(&hooks_path)?
        } else {
            String::new()
        };
        let new_content = serde_json::to_string_pretty(&Self::desired_hooks(&params.binary_path))?;

        if existing.trim() == new_content.trim() {
            return Ok(None);
        }

        let diff_output = generate_diff(&hooks_path, &existing, &new_content);
        if !dry_run {
            write_atomic(&hooks_path, new_content.as_bytes())?;
        }
        Ok(Some(diff_output))
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let hooks_path = Self::hooks_path();
        if !hooks_path.exists() {
            return Ok(None);
        }

        let existing = fs::read_to_string(&hooks_path)?;
        if !Self::has_grok_checkpoint(&existing) {
            return Ok(None);
        }

        let diff_output = generate_diff(&hooks_path, &existing, "");
        if !dry_run {
            fs::remove_file(&hooks_path)?;
        }
        Ok(Some(diff_output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    fn with_temp_grok_home<F: FnOnce(&Path)>(f: F) {
        let temp_dir = TempDir::new().unwrap();
        let prev_grok_home = std::env::var_os("GROK_HOME");
        unsafe {
            std::env::set_var("GROK_HOME", temp_dir.path());
        }
        f(temp_dir.path());
        unsafe {
            match prev_grok_home {
                Some(v) => std::env::set_var("GROK_HOME", v),
                None => std::env::remove_var("GROK_HOME"),
            }
        }
    }

    fn params() -> HookInstallerParams {
        HookInstallerParams {
            binary_path: PathBuf::from("/usr/local/bin/git-ai"),
        }
    }

    #[test]
    #[serial]
    fn test_install_hooks_writes_git_ai_file() {
        with_temp_grok_home(|home| {
            let installer = GrokInstaller;
            let diff = installer.install_hooks(&params(), false).unwrap();
            assert!(diff.is_some());

            let content = fs::read_to_string(home.join("hooks/git-ai.json")).unwrap();
            assert!(content.contains("checkpoint grok --hook-input stdin"));
            assert!(content.contains("PreToolUse"));
            assert!(content.contains("PostToolUse"));

            let status = installer.check_hooks(&params()).unwrap();
            assert!(status.tool_installed);
            assert!(status.hooks_installed);
            assert!(status.hooks_up_to_date);
        });
    }

    #[test]
    #[serial]
    fn test_install_hooks_is_idempotent() {
        with_temp_grok_home(|_home| {
            let installer = GrokInstaller;
            installer.install_hooks(&params(), false).unwrap();
            let second = installer.install_hooks(&params(), false).unwrap();
            assert!(second.is_none());
        });
    }

    #[test]
    #[serial]
    fn test_uninstall_removes_git_ai_hooks() {
        with_temp_grok_home(|home| {
            let installer = GrokInstaller;
            installer.install_hooks(&params(), false).unwrap();
            let diff = installer.uninstall_hooks(&params(), false).unwrap();
            assert!(diff.is_some());
            assert!(!home.join("hooks/git-ai.json").exists());
        });
    }

    #[test]
    #[serial]
    fn test_home_without_hooks_is_not_installed() {
        with_temp_grok_home(|_home| {
            let status = GrokInstaller.check_hooks(&params()).unwrap();
            assert!(status.tool_installed);
            assert!(!status.hooks_installed);
        });
    }
}
