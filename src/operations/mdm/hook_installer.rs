use crate::error::GitAiError;
use std::path::PathBuf;

/// Parameters passed to hook installers
#[derive(Clone)]
pub struct HookInstallerParams {
    /// Path to the git-ai binary
    pub binary_path: PathBuf,
}

/// Result of checking hook status
pub struct HookCheckResult {
    /// Whether the tool (IDE/agent) is installed
    pub tool_installed: bool,
    /// Whether hooks are installed
    pub hooks_installed: bool,
    /// Whether hooks are up to date
    #[allow(dead_code)]
    pub hooks_up_to_date: bool,
}

impl HookCheckResult {
    pub(crate) fn tool_not_installed() -> Self {
        Self {
            tool_installed: false,
            hooks_installed: false,
            hooks_up_to_date: false,
        }
    }

    pub(crate) fn installed_without_hooks() -> Self {
        Self::installed(false, false)
    }

    pub(crate) fn installed(hooks_installed: bool, hooks_up_to_date: bool) -> Self {
        Self {
            tool_installed: true,
            hooks_installed,
            hooks_up_to_date,
        }
    }
}

/// Result of an install operation
pub struct InstallResult {
    /// Whether changes were made
    pub changed: bool,
    /// Diff output if changes were made
    pub diff: Option<String>,
    /// Human-readable message
    pub message: String,
}

/// Result of an uninstall operation
pub struct UninstallResult {
    /// Whether changes were made
    pub changed: bool,
    /// Diff output if changes were made
    pub diff: Option<String>,
    /// Human-readable message
    pub message: String,
}

/// Trait for installing hooks into various IDEs and agent configurations
pub trait HookInstaller: Send + Sync {
    /// Human-readable name of the tool (e.g., "Claude Code", "Cursor")
    fn name(&self) -> &str;

    /// Short identifier for status maps (e.g., "claude-code", "cursor")
    fn id(&self) -> &str;

    /// Whether this tool uses config file hooks (vs only extras like plugins)
    /// Default is true. Tools that only use install_extras should return false.
    fn uses_config_hooks(&self) -> bool {
        true
    }

    /// Check if the tool is installed and hook status
    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError>;

    /// Install or update hooks
    /// Returns Ok(Some(diff)) if changes were made, Ok(None) if already up to date
    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError>;

    /// Uninstall hooks
    /// Returns Ok(Some(diff)) if changes were made, Ok(None) if nothing to uninstall
    fn uninstall_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError>;

    /// Install extras (e.g., VS Code extensions, git.path configuration)
    /// Default implementation does nothing
    fn install_extras(
        &self,
        _params: &HookInstallerParams,
        _dry_run: bool,
    ) -> Result<Vec<InstallResult>, GitAiError> {
        Ok(vec![])
    }

    /// Process names to search for in the system process list.
    /// Used after hook updates to detect running instances that need restarting.
    /// Default implementation returns an empty list (no process detection).
    fn process_names(&self) -> Vec<&str> {
        vec![]
    }

    /// Uninstall extras (e.g., VS Code extensions, git.path configuration)
    /// Default implementation does nothing
    fn uninstall_extras(
        &self,
        _params: &HookInstallerParams,
        _dry_run: bool,
    ) -> Result<Vec<UninstallResult>, GitAiError> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::HookCheckResult;

    #[test]
    fn hook_check_result_constructors_encode_status_flags() {
        let cases = [
            (HookCheckResult::tool_not_installed(), false, false, false),
            (
                HookCheckResult::installed_without_hooks(),
                true,
                false,
                false,
            ),
            (HookCheckResult::installed(false, false), true, false, false),
            (HookCheckResult::installed(true, false), true, true, false),
            (HookCheckResult::installed(true, true), true, true, true),
        ];

        for (result, tool_installed, hooks_installed, hooks_up_to_date) in cases {
            assert_eq!(result.tool_installed, tool_installed);
            assert_eq!(result.hooks_installed, hooks_installed);
            assert_eq!(result.hooks_up_to_date, hooks_up_to_date);
        }
    }
}
