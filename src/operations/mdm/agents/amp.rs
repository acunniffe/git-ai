use crate::error::GitAiError;
use crate::operations::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::operations::mdm::paths::home_dir;
use crate::operations::mdm::plugin_drop::{self, FileDropSpec};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

// Amp plugin content (TypeScript), embedded from the source file
const AMP_PLUGIN_CONTENT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/agent-support/amp/git-ai.ts"
));

fn amp_plugin_path() -> PathBuf {
    home_dir()
        .join(".config")
        .join("amp")
        .join("plugins")
        .join("git-ai.ts")
}

fn amp_global_config_dir() -> PathBuf {
    home_dir().join(".config").join("amp")
}

const AMP_SPEC: FileDropSpec = FileDropSpec {
    name: "Amp",
    id: "amp",
    template: AMP_PLUGIN_CONTENT,
    dest_path: amp_plugin_path,
    global_config_dir: amp_global_config_dir,
    local_config_dir: ".amp",
    detect_binary_names: &["amp"],
    process_names: &["amp"],
};

pub struct AmpInstaller;

// Test-only convenience wrappers kept so existing content-generation tests
// below don't need to reach into `AMP_SPEC` directly.
#[cfg(test)]
impl AmpInstaller {
    fn plugin_path() -> PathBuf {
        (AMP_SPEC.dest_path)()
    }

    /// Generate plugin content with the absolute binary path substituted in.
    fn generate_plugin_content(binary_path: &Path) -> String {
        plugin_drop::generate_content(&AMP_SPEC, binary_path)
    }
}

impl HookInstaller for AmpInstaller {
    fn name(&self) -> &str {
        AMP_SPEC.name
    }

    fn id(&self) -> &str {
        AMP_SPEC.id
    }

    fn process_names(&self) -> Vec<&str> {
        AMP_SPEC.process_names.to_vec()
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        plugin_drop::file_drop_check_hooks(&AMP_SPEC, params)
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        plugin_drop::file_drop_install(&AMP_SPEC, params, dry_run)
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        plugin_drop::file_drop_uninstall(&AMP_SPEC, dry_run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::mdm::test_env::{with_empty_path, with_temp_home};
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_env() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let plugin_path = temp_dir
            .path()
            .join(".config")
            .join("amp")
            .join("plugins")
            .join("git-ai.ts");
        (temp_dir, plugin_path)
    }

    fn create_test_binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    #[test]
    fn test_amp_install_plugin_creates_file_from_scratch() {
        let (_temp_dir, plugin_path) = setup_test_env();
        let binary_path = create_test_binary_path();

        if let Some(parent) = plugin_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let generated = AmpInstaller::generate_plugin_content(&binary_path);
        fs::write(&plugin_path, &generated).unwrap();

        assert!(plugin_path.exists());

        let content = fs::read_to_string(&plugin_path).unwrap();
        assert!(content.contains("hook_event_name"));
        assert!(content.contains("PreToolUse"));
        assert!(content.contains("PostToolUse"));
        assert!(content.contains("checkpoint amp"));
        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
        assert!(content.contains(r#"const GIT_AI_BIN = '/usr/local/bin/git-ai'"#));
    }

    #[test]
    fn test_amp_plugin_output_path() {
        assert_eq!(
            AmpInstaller::plugin_path(),
            home_dir()
                .join(".config")
                .join("amp")
                .join("plugins")
                .join("git-ai.ts")
        );
    }

    #[test]
    fn test_amp_plugin_content_is_valid_typescript() {
        let content = AMP_PLUGIN_CONTENT;

        assert!(
            content
                .lines()
                .next()
                .unwrap_or_default()
                .contains("@i-know-the-amp-plugin-api-is-wip-and-very-experimental-right-now")
        );
        assert!(
            content.contains("import type { PluginAPI, ToolCallEvent } from '@ampcode/plugin'")
        );
        assert!(content.contains("amp.on('tool.call'"));
        assert!(content.contains("amp.on('tool.result'"));
        assert!(content.contains("filesModifiedByToolCall"));
        assert!(content.contains("spawn("));
        assert!(content.contains("'--hook-input', 'stdin'"));
        assert!(content.contains("rev-parse"));
        assert!(content.contains("__GIT_AI_BINARY_PATH__"));
    }

    #[test]
    fn test_amp_plugin_placeholder_substitution() {
        let binary_path = create_test_binary_path();
        let content = AmpInstaller::generate_plugin_content(&binary_path);

        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
        assert!(content.contains(r#"const GIT_AI_BIN = '/usr/local/bin/git-ai'"#));
        assert!(content.contains("'checkpoint'"));
        assert!(content.contains("'amp'"));
        assert!(content.contains("'--hook-input'"));
        assert!(content.contains("'stdin'"));
    }

    #[test]
    fn test_amp_plugin_windows_path_escaping() {
        let binary_path = PathBuf::from(r"C:\Users\foo\.git-ai\bin\git-ai.exe");
        let content = AmpInstaller::generate_plugin_content(&binary_path);

        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
        assert!(
            content.contains(r#"const GIT_AI_BIN = 'C:\\Users\\foo\\.git-ai\\bin\\git-ai.exe'"#)
        );
    }

    #[test]
    fn test_amp_plugin_handles_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let binary_path = create_test_binary_path();
        let plugin_path = temp_dir
            .path()
            .join(".config")
            .join("amp")
            .join("plugins")
            .join("git-ai.ts");

        assert!(!plugin_path.parent().unwrap().exists());

        if let Some(parent) = plugin_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let generated = AmpInstaller::generate_plugin_content(&binary_path);
        fs::write(&plugin_path, &generated).unwrap();

        assert!(plugin_path.exists());
        let content = fs::read_to_string(&plugin_path).unwrap();
        assert!(content.contains("tool.call"));
        assert!(content.contains("tool.result"));
        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
    }

    // ---- Detection / check_hooks / install_hooks / uninstall_hooks (trait-level) ----

    #[test]
    #[serial]
    fn test_amp_no_binary_no_config_not_detected() {
        with_temp_home(|_home| {
            with_empty_path(|| {
                let installer = AmpInstaller;
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
    fn test_amp_install_hooks_creates_plugin_via_trait() {
        with_temp_home(|home| {
            let installer = AmpInstaller;
            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };

            let result = installer.install_hooks(&params, false).unwrap();
            assert!(result.is_some(), "install_hooks should produce a diff");

            let plugin_path = home
                .join(".config")
                .join("amp")
                .join("plugins")
                .join("git-ai.ts");
            assert!(plugin_path.exists());

            let content = fs::read_to_string(&plugin_path).unwrap();
            assert!(content.contains("checkpoint amp"));
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
    fn test_amp_install_hooks_dry_run_does_not_write() {
        with_temp_home(|home| {
            let installer = AmpInstaller;
            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };

            let result = installer.install_hooks(&params, true).unwrap();
            assert!(result.is_some(), "dry_run should still report a diff");

            let plugin_path = home
                .join(".config")
                .join("amp")
                .join("plugins")
                .join("git-ai.ts");
            assert!(!plugin_path.exists(), "dry_run must not write the file");
        });
    }

    #[test]
    #[serial]
    fn test_amp_uninstall_hooks_removes_plugin() {
        with_temp_home(|home| {
            let installer = AmpInstaller;
            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };
            installer.install_hooks(&params, false).unwrap();

            let plugin_path = home
                .join(".config")
                .join("amp")
                .join("plugins")
                .join("git-ai.ts");
            assert!(plugin_path.exists());

            let result = installer.uninstall_hooks(&params, false).unwrap();
            assert!(result.is_some());
            assert!(!plugin_path.exists());

            // Uninstalling again is a no-op
            let second = installer.uninstall_hooks(&params, false).unwrap();
            assert!(second.is_none());
        });
    }

    #[test]
    fn test_amp_process_names() {
        let installer = AmpInstaller;
        assert_eq!(installer.process_names(), vec!["amp"]);
    }
}
