use crate::error::GitAiError;
use crate::operations::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::operations::mdm::paths::home_dir;
use crate::operations::mdm::plugin_drop::{self, FileDropSpec};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

const PI_EXTENSION_CONTENT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/agent-support/pi/git-ai.ts"
));

fn pi_extension_path() -> PathBuf {
    home_dir()
        .join(".pi")
        .join("agent")
        .join("extensions")
        .join("git-ai.ts")
}

fn pi_global_config_dir() -> PathBuf {
    home_dir().join(".pi")
}

const PI_SPEC: FileDropSpec = FileDropSpec {
    name: "Pi",
    id: "pi",
    template: PI_EXTENSION_CONTENT,
    dest_path: pi_extension_path,
    global_config_dir: pi_global_config_dir,
    local_config_dir: ".pi",
    detect_binary_names: &["pi"],
    // Pi does not participate in post-update process-restart detection (no
    // override of HookInstaller::process_names below, matching pre-refactor
    // behavior of relying on the trait's default empty list).
    process_names: &[],
};

pub struct PiInstaller;

// Test-only convenience wrappers kept so existing content-generation tests
// below don't need to reach into `PI_SPEC` directly.
#[cfg(test)]
impl PiInstaller {
    fn extension_path() -> PathBuf {
        (PI_SPEC.dest_path)()
    }

    fn generate_extension_content(binary_path: &Path) -> String {
        plugin_drop::generate_content(&PI_SPEC, binary_path)
    }
}

impl HookInstaller for PiInstaller {
    fn name(&self) -> &str {
        PI_SPEC.name
    }

    fn id(&self) -> &str {
        PI_SPEC.id
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        plugin_drop::file_drop_check_hooks(&PI_SPEC, params)
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        plugin_drop::file_drop_install(&PI_SPEC, params, dry_run)
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        plugin_drop::file_drop_uninstall(&PI_SPEC, dry_run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::mdm::test_env::{with_empty_path, with_temp_home};
    use serial_test::serial;
    use std::fs;

    fn create_test_binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    #[test]
    fn test_pi_extension_output_path() {
        assert_eq!(
            PiInstaller::extension_path(),
            home_dir()
                .join(".pi")
                .join("agent")
                .join("extensions")
                .join("git-ai.ts")
        );
    }

    #[test]
    fn test_pi_extension_content_contains_checkpoint_command() {
        let content = PiInstaller::generate_extension_content(&create_test_binary_path());

        assert!(content.contains("'checkpoint', 'pi', '--hook-input', 'stdin'"));
        assert!(content.contains("--hook-input', 'stdin'"));
        assert!(content.contains("hook_event_name"));
        assert!(content.contains("session_path"));
        assert!(content.contains("tool_name_raw"));
        assert!(content.contains("tool_name"));
        assert!(content.contains("will_edit_filepaths"));
        assert!(content.contains("edited_filepaths"));
        assert!(content.contains(r#"const GIT_AI_BIN = '/usr/local/bin/git-ai'"#));
    }

    #[test]
    fn test_pi_extension_content_contains_tool_normalization_table() {
        let content = PiInstaller::generate_extension_content(&create_test_binary_path());

        assert!(content.contains("type ToolOverridePolicy = {"));
        assert!(content.contains("type OverrideConfig = {"));
        assert!(content.contains("version: 1;"));
        assert!(content.contains("tools: Record<string, ToolOverridePolicy>;"));
        assert!(
            content.contains("const DEFAULT_TOOL_POLICIES: Record<string, ToolOverridePolicy> = {")
        );
        assert!(content.contains("edit: {"));
        assert!(content.contains("canonical: 'edit'"));
        assert!(content.contains("write: {"));
        assert!(content.contains("canonical: 'write'"));
        assert!(content.contains("filepath_fields: ['path']"));
        assert!(content.contains("git-ai.override.json"));
        assert!(content.contains("normalizeToolPolicy"));
        assert!(content.contains("override?.tools ?? {}"));
    }

    // ---- Detection / check_hooks / install_hooks / uninstall_hooks (trait-level) ----

    #[test]
    #[serial]
    fn test_pi_no_binary_no_config_not_detected() {
        with_temp_home(|_home| {
            with_empty_path(|| {
                let installer = PiInstaller;
                let params = HookInstallerParams {
                    binary_path: create_test_binary_path(),
                };
                let result = installer.check_hooks(&params).unwrap();
                assert!(!result.tool_installed);
                assert!(!result.hooks_installed);
            });
        });
    }

    #[test]
    #[serial]
    fn test_pi_install_hooks_creates_extension_via_trait() {
        with_temp_home(|home| {
            let installer = PiInstaller;
            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };

            let result = installer.install_hooks(&params, false).unwrap();
            assert!(result.is_some(), "install_hooks should produce a diff");

            let extension_path = home
                .join(".pi")
                .join("agent")
                .join("extensions")
                .join("git-ai.ts");
            assert!(extension_path.exists());

            let content = fs::read_to_string(&extension_path).unwrap();
            assert!(content.contains("'checkpoint', 'pi', '--hook-input', 'stdin'"));
            assert!(!content.contains("__GIT_AI_BINARY_PATH__"));

            // check_hooks should now report installed + up to date
            let check = installer.check_hooks(&params).unwrap();
            assert!(check.tool_installed);
            assert!(check.hooks_installed);
            assert!(check.hooks_up_to_date);

            // A second install_hooks call is a no-op (already up to date)
            let second = installer.install_hooks(&params, false).unwrap();
            assert!(second.is_none());
        });
    }

    #[test]
    #[serial]
    fn test_pi_install_hooks_dry_run_does_not_write() {
        with_temp_home(|home| {
            let installer = PiInstaller;
            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };

            let result = installer.install_hooks(&params, true).unwrap();
            assert!(result.is_some(), "dry_run should still report a diff");

            let extension_path = home
                .join(".pi")
                .join("agent")
                .join("extensions")
                .join("git-ai.ts");
            assert!(!extension_path.exists(), "dry_run must not write the file");
        });
    }

    #[test]
    #[serial]
    fn test_pi_uninstall_hooks_removes_extension() {
        with_temp_home(|home| {
            let installer = PiInstaller;
            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };
            installer.install_hooks(&params, false).unwrap();

            let extension_path = home
                .join(".pi")
                .join("agent")
                .join("extensions")
                .join("git-ai.ts");
            assert!(extension_path.exists());

            let result = installer.uninstall_hooks(&params, false).unwrap();
            assert!(result.is_some());
            assert!(!extension_path.exists());

            // Uninstalling again is a no-op
            let second = installer.uninstall_hooks(&params, false).unwrap();
            assert!(second.is_none());
        });
    }

    #[test]
    fn test_pi_process_names_is_empty_by_default() {
        let installer = PiInstaller;
        assert!(installer.process_names().is_empty());
    }
}
