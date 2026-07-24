use crate::error::GitAiError;
use crate::operations::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::operations::mdm::paths::home_dir;
use crate::operations::mdm::plugin_drop::{self, FileDropSpec};
use std::fs;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

// OpenCode plugin content (TypeScript), embedded from the source file
const OPENCODE_PLUGIN_CONTENT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/agent-support/opencode/git-ai.ts"
));

fn opencode_plugin_path() -> PathBuf {
    home_dir()
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("git-ai.ts")
}

fn opencode_global_config_dir() -> PathBuf {
    home_dir().join(".config").join("opencode")
}

const OPENCODE_SPEC: FileDropSpec = FileDropSpec {
    name: "OpenCode",
    id: "opencode",
    template: OPENCODE_PLUGIN_CONTENT,
    dest_path: opencode_plugin_path,
    global_config_dir: opencode_global_config_dir,
    local_config_dir: ".opencode",
    detect_binary_names: &["opencode", "opencode2"],
    process_names: &["opencode", "opencode2"],
};

/// Path of the plugin file from old installations (`~/.config/opencode/plugin/`,
/// singular). Opportunistically removed on install/uninstall so it can't
/// shadow the current `plugins/` (plural) location. This migration is unique
/// to OpenCode -- Amp and Pi never used a singular directory name.
fn opencode_legacy_plugin_path() -> PathBuf {
    home_dir()
        .join(".config")
        .join("opencode")
        .join("plugin")
        .join("git-ai.ts")
}

pub struct OpenCodeInstaller;

// Test-only convenience wrappers kept so existing content-generation tests
// below don't need to reach into `OPENCODE_SPEC` directly.
#[cfg(test)]
impl OpenCodeInstaller {
    fn plugin_path() -> PathBuf {
        (OPENCODE_SPEC.dest_path)()
    }

    /// Generate plugin content with the absolute binary path substituted in
    fn generate_plugin_content(binary_path: &Path) -> String {
        plugin_drop::generate_content(&OPENCODE_SPEC, binary_path)
    }
}

impl HookInstaller for OpenCodeInstaller {
    fn name(&self) -> &str {
        OPENCODE_SPEC.name
    }

    fn id(&self) -> &str {
        OPENCODE_SPEC.id
    }

    fn process_names(&self) -> Vec<&str> {
        OPENCODE_SPEC.process_names.to_vec()
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        plugin_drop::file_drop_check_hooks(&OPENCODE_SPEC, params)
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        // Remove legacy plugin from old installations (~/.config/opencode/plugin/ singular)
        if !dry_run {
            let _ = fs::remove_file(opencode_legacy_plugin_path());
        }

        plugin_drop::file_drop_install(&OPENCODE_SPEC, params, dry_run)
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        // Remove legacy plugin from old installations (~/.config/opencode/plugin/ singular)
        if !dry_run {
            let _ = fs::remove_file(opencode_legacy_plugin_path());
        }

        plugin_drop::file_drop_uninstall(&OPENCODE_SPEC, dry_run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::mdm::test_env::{
        with_empty_path, with_fake_binary_on_path, with_temp_home,
    };
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_env() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let plugin_path = temp_dir
            .path()
            .join(".config")
            .join("opencode")
            .join("plugins")
            .join("git-ai.ts");
        (temp_dir, plugin_path)
    }

    fn create_test_binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    #[test]
    fn test_opencode_install_plugin_creates_file_from_scratch() {
        let (_temp_dir, plugin_path) = setup_test_env();
        let binary_path = create_test_binary_path();

        if let Some(parent) = plugin_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let generated = OpenCodeInstaller::generate_plugin_content(&binary_path);
        fs::write(&plugin_path, &generated).unwrap();

        assert!(plugin_path.exists());

        let content = fs::read_to_string(&plugin_path).unwrap();
        assert!(content.contains("GitAiPlugin"));
        assert!(content.contains("tool.execute.before"));
        assert!(content.contains("tool.execute.after"));
        // Uses the opencode preset with session_id-based hook input and absolute path
        assert!(content.contains("session_id"));
        // Placeholder should be replaced with actual binary path in the const declaration
        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
        assert!(content.contains(r#"const GIT_AI_BIN = "/usr/local/bin/git-ai""#));
    }

    #[test]
    fn test_opencode_plugin_content_is_valid_typescript() {
        let content = OPENCODE_PLUGIN_CONTENT;

        assert!(content.contains("import type { Plugin }"));
        assert!(content.contains("@opencode-ai/plugin"));
        assert!(content.contains("export const GitAiPlugin: Plugin"));
        assert!(content.contains("export default GitAiPlugin"));
        assert!(content.contains("child_process"));
        assert!(content.contains("\"tool.execute.before\""));
        assert!(content.contains("\"tool.execute.after\""));
        assert!(content.contains("FILE_EDIT_TOOLS"));
        assert!(content.contains("isBashTool"));
        assert!(content.contains("apply_patch"));
        // Template contains placeholder for binary path
        assert!(content.contains("__GIT_AI_BINARY_PATH__"));
        assert!(content.contains("hook_event_name"));
        assert!(content.contains("session_id"));
        assert!(content.contains("PreToolUse"));
        assert!(content.contains("PostToolUse"));
    }

    #[test]
    fn test_opencode_plugin_placeholder_substitution() {
        let binary_path = create_test_binary_path();
        let content = OpenCodeInstaller::generate_plugin_content(&binary_path);

        // Placeholder should be replaced with the actual binary path in the const
        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
        assert!(content.contains(r#"const GIT_AI_BIN = "/usr/local/bin/git-ai""#));
        // Checkpoint execution uses spawn(), which works in OpenCode CLI and Desktop.
        assert!(content.contains("spawn(GIT_AI_BIN"));
        assert!(content.contains(r#""checkpoint", "opencode", "--hook-input", "stdin""#));
        assert!(!content.contains("Bun.$"));
    }

    #[test]
    fn test_opencode_plugin_skips_if_already_exists() {
        let (_temp_dir, plugin_path) = setup_test_env();
        let binary_path = create_test_binary_path();

        if let Some(parent) = plugin_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let generated = OpenCodeInstaller::generate_plugin_content(&binary_path);
        fs::write(&plugin_path, &generated).unwrap();
        let content1 = fs::read_to_string(&plugin_path).unwrap();

        fs::write(&plugin_path, &generated).unwrap();
        let content2 = fs::read_to_string(&plugin_path).unwrap();

        assert_eq!(content1, content2);
    }

    #[test]
    fn test_opencode_plugin_updates_outdated_content() {
        let (_temp_dir, plugin_path) = setup_test_env();
        let binary_path = create_test_binary_path();

        if let Some(parent) = plugin_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let old_content = "// Old plugin version\nexport const OldPlugin = {}";
        fs::write(&plugin_path, old_content).unwrap();

        let content_before = fs::read_to_string(&plugin_path).unwrap();
        assert!(content_before.contains("OldPlugin"));

        let generated = OpenCodeInstaller::generate_plugin_content(&binary_path);
        fs::write(&plugin_path, &generated).unwrap();

        let content_after = fs::read_to_string(&plugin_path).unwrap();
        assert!(content_after.contains("GitAiPlugin"));
        assert!(!content_after.contains("OldPlugin"));
    }

    #[test]
    fn test_opencode_plugin_windows_path_escaping() {
        let binary_path = PathBuf::from(r"C:\Users\foo\.git-ai\bin\git-ai.exe");
        let content = OpenCodeInstaller::generate_plugin_content(&binary_path);

        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
        // Backslashes should be doubled for the TS string literal
        assert!(
            content.contains(r#"const GIT_AI_BIN = "C:\\Users\\foo\\.git-ai\\bin\\git-ai.exe""#)
        );
    }

    #[test]
    fn test_opencode_plugin_handles_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let binary_path = create_test_binary_path();
        let plugin_path = temp_dir
            .path()
            .join(".config")
            .join("opencode")
            .join("plugins")
            .join("git-ai.ts");

        assert!(!plugin_path.parent().unwrap().exists());

        if let Some(parent) = plugin_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let generated = OpenCodeInstaller::generate_plugin_content(&binary_path);
        fs::write(&plugin_path, &generated).unwrap();

        assert!(plugin_path.exists());
        let content = fs::read_to_string(&plugin_path).unwrap();
        assert!(content.contains("GitAiPlugin"));
        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
    }

    // ---- Detection / process_names / check_hooks ----

    #[test]
    fn test_opencode_process_names_includes_opencode2() {
        let installer = OpenCodeInstaller;
        let names = installer.process_names();
        assert!(
            names.contains(&"opencode"),
            "process_names should include 'opencode'"
        );
        assert!(
            names.contains(&"opencode2"),
            "process_names should include 'opencode2' for @next pre-release support"
        );
    }

    #[test]
    #[serial]
    fn test_opencode2_binary_detected_as_tool_installed() {
        with_temp_home(|_home| {
            with_fake_binary_on_path("opencode2", |_| {
                let installer = OpenCodeInstaller;
                let params = HookInstallerParams {
                    binary_path: create_test_binary_path(),
                };
                let result = installer.check_hooks(&params).unwrap();
                assert!(
                    result.tool_installed,
                    "opencode2 binary on PATH should be detected as tool_installed"
                );
                assert!(!result.hooks_installed);
            });
        });
    }

    #[test]
    #[serial]
    fn test_opencode_no_binary_no_config_not_detected() {
        with_temp_home(|_home| {
            with_empty_path(|| {
                let installer = OpenCodeInstaller;
                let params = HookInstallerParams {
                    binary_path: create_test_binary_path(),
                };
                let result = installer.check_hooks(&params).unwrap();
                assert!(
                    !result.tool_installed,
                    "no binary and no config should mean tool_installed=false"
                );
            });
        });
    }

    #[test]
    #[serial]
    fn test_opencode2_binary_install_creates_plugin() {
        with_temp_home(|_home| {
            with_fake_binary_on_path("opencode2", |_| {
                let installer = OpenCodeInstaller;
                let params = HookInstallerParams {
                    binary_path: create_test_binary_path(),
                };
                let result = installer.install_hooks(&params, false).unwrap();
                assert!(result.is_some(), "install_hooks should produce a diff");

                let plugin_path = OpenCodeInstaller::plugin_path();
                assert!(
                    plugin_path.exists(),
                    "install_hooks should create the plugin file"
                );

                let content = fs::read_to_string(&plugin_path).unwrap();
                assert!(
                    content.contains("GitAiPlugin"),
                    "plugin should contain GitAiPlugin"
                );
            });
        });
    }

    #[test]
    #[serial]
    fn test_opencode_install_removes_legacy_singular_plugin() {
        with_temp_home(|home| {
            let legacy = home
                .join(".config")
                .join("opencode")
                .join("plugin")
                .join("git-ai.ts");
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, "// legacy").unwrap();

            let installer = OpenCodeInstaller;
            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };

            // Dry run must leave the legacy file in place.
            installer.install_hooks(&params, true).unwrap();
            assert!(
                legacy.exists(),
                "dry-run install must not delete the legacy plugin"
            );

            // Real install migrates: legacy file removed, new plural path written.
            installer.install_hooks(&params, false).unwrap();
            assert!(
                !legacy.exists(),
                "install must remove the legacy singular-plugin file"
            );
            assert!(
                home.join(".config")
                    .join("opencode")
                    .join("plugins")
                    .join("git-ai.ts")
                    .exists()
            );
        });
    }

    #[test]
    #[serial]
    fn test_opencode_uninstall_removes_legacy_singular_plugin() {
        with_temp_home(|home| {
            let legacy = home
                .join(".config")
                .join("opencode")
                .join("plugin")
                .join("git-ai.ts");
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, "// legacy").unwrap();

            let installer = OpenCodeInstaller;
            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };

            installer.uninstall_hooks(&params, true).unwrap();
            assert!(
                legacy.exists(),
                "dry-run uninstall must not delete the legacy plugin"
            );

            installer.uninstall_hooks(&params, false).unwrap();
            assert!(
                !legacy.exists(),
                "uninstall must remove the legacy singular-plugin file"
            );
        });
    }
}
